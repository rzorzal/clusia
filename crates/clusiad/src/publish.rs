//! Publishing a draft as one GitHub review (spec §6.4), through GraphQL:
//! a pending review with the line comments, the replies inside it, the submit, then the resolves.

use std::path::PathBuf;

use clusia_core::time::parse_rfc3339;
use clusia_core::{
    ActivityKind, ItemStatus, PrRef, ReplyPayload, Review, ReviewEvent, ReviewInfo, ReviewPayload,
    ReviewState, Role, Verdict, plan_publish,
};
use clusia_git::{pin_commit, reviewed_ref};
use clusia_protocol::{ErrorCode, Outcome, ProtocolError, PublishResult, Reply};
use clusia_provider::{GitHub, ProviderError, PublishedReview};

use crate::activity::read_off_thread;
use crate::handlers::{no_token, provider_error};
use crate::reviews::{
    announce, cleanup_checkout, drop_cache, files_for, invalid_state, load_existing, lock, record,
    save,
};
use crate::state::Shared;
use crate::sync::{github_client, now_unix};
use crate::{relocate, worktrees};

fn conflict(message: &str) -> Outcome {
    Outcome::Err(ProtocolError::new(ErrorCode::Conflict, message))
}

/// Returns the review to `saved` after a failed publish; the draft is intact.
fn fail_publish(shared: &Shared, review: &mut Review) {
    if let Err(e) = review.apply(ReviewEvent::PublishFailed, now_unix()) {
        tracing::warn!(error = %e, "unexpected state after a failed publish");
    }
    if let Err(Outcome::Err(e)) = save(shared, review) {
        tracing::warn!(error = %e.message, "cannot save the review after a failed publish");
    }
}

/// Records a completed publish and removes the review file. The review is already on
/// GitHub, so failures here are logged, never returned.
pub(crate) fn finish_published(
    shared: &Shared,
    review: &mut Review,
    pr: &PrRef,
    client: &str,
    url: Option<String>,
) {
    settle_published(shared, review, pr);
    record(shared, ActivityKind::ReviewPublished, pr, client, url, None);
}

/// Marks the review published and removes its file. The terminal state is saved first: a file
/// left behind by a failed delete then opens as a fresh review instead of a draft to post again.
fn settle_published(shared: &Shared, review: &mut Review, pr: &PrRef) {
    if let Err(e) = review.apply(ReviewEvent::PublishOk, now_unix()) {
        tracing::warn!(error = %e, pr = %pr, "unexpected state after publishing");
    }
    if let Err(Outcome::Err(e)) = save(shared, review) {
        tracing::warn!(error = %e.message, pr = %pr, "published, but cannot save the review as published");
    }
    if let Err(e) = clusia_store::delete_review(&shared.paths, pr) {
        tracing::warn!(error = %e, pr = %pr, "published, but cannot delete the review file");
    }
    drop_cache(shared, pr);
}

/// How far GitHub's clock may trail ours when telling whether a review was submitted after the
/// Finalize moment.
const CLOCK_SKEW_SECS: i64 = 30;

/// The review `viewer` submitted on `head_sha` at or after `finalized_at`, if GitHub has one:
/// proof that an earlier publish got through although its answer was lost. `recorded` holds
/// the URLs of reviews already recorded as published: an earlier publish on the same head,
/// within the clock-skew margin, is not this one.
pub(crate) fn find_posted<'a>(
    reviews: &'a [ReviewInfo],
    viewer: &str,
    head_sha: &str,
    finalized_at: i64,
    recorded: &[String],
) -> Option<&'a ReviewInfo> {
    reviews
        .iter()
        .filter(|r| r.author.eq_ignore_ascii_case(viewer) && r.state != "PENDING")
        .filter(|r| !recorded.contains(&r.url))
        .filter(|r| r.commit_id.as_deref() == Some(head_sha))
        .filter(|r| {
            r.submitted_at
                .as_deref()
                .and_then(parse_rfc3339)
                .is_some_and(|at| at >= finalized_at - CLOCK_SKEW_SECS)
        })
        .max_by_key(|r| r.id)
}

/// What GitHub says about a publish whose outcome is unknown.
pub(crate) enum Posted {
    /// The review is there, at this URL.
    Yes(String),
    /// The review is there and was already recorded as published: only its file was left.
    Recorded(String),
    No,
    /// GitHub could not be asked.
    Unknown(ProviderError),
}

pub(crate) async fn posted_review(
    shared: &Shared,
    gh: &GitHub,
    pr: &PrRef,
    head_sha: &str,
    finalized_at: i64,
) -> Posted {
    let viewer = match gh.viewer().await {
        Ok(v) => v.login,
        Err(e) => return Posted::Unknown(e),
    };
    let reviews = match gh.list_reviews(pr).await {
        Ok(reviews) => reviews,
        Err(e) => return Posted::Unknown(e),
    };
    let recorded = recorded_reviews(shared, pr).await;
    match find_posted(&reviews, &viewer, head_sha, finalized_at, &recorded.earlier) {
        Some(r) if recorded.current.contains(&r.url) => Posted::Recorded(r.url.clone()),
        Some(r) => Posted::Yes(r.url.clone()),
        None => Posted::No,
    }
}

/// The URLs of the reviews on `pr` this daemon recorded as published, split at the last time a
/// fresh review was opened there.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RecordedReviews {
    /// Published by earlier reviews: never this one.
    pub earlier: Vec<String>,
    /// Published since the current review was opened: this one, already recorded.
    pub current: Vec<String>,
}

pub(crate) fn split_recorded(activity: &[clusia_core::Activity], pr: &PrRef) -> RecordedReviews {
    let mut out = RecordedReviews::default();
    for a in activity.iter().filter(|a| &a.pr == pr) {
        match a.kind {
            ActivityKind::ReviewOpened => {
                let current = std::mem::take(&mut out.current);
                out.earlier.extend(current);
            }
            ActivityKind::ReviewPublished => out.current.extend(a.url.clone()),
            _ => {}
        }
    }
    out
}

async fn recorded_reviews(shared: &Shared, pr: &PrRef) -> RecordedReviews {
    match read_off_thread(&shared.paths).await {
        Ok((activity, _)) => split_recorded(&activity, pr),
        Err(e) => {
            tracing::warn!(error = %e, pr = %pr, "cannot read which reviews were already published");
            RecordedReviews::default()
        }
    }
}

/// A review found `publishing` (the daemon died or the answer was lost): when GitHub already
/// has it, records it as published and returns `true`; when it does not, `false`, and the
/// caller returns the review to `saved`. When GitHub cannot be asked, an error outcome: the
/// review is left as it is, so nothing is posted twice.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
pub(crate) async fn settle_interrupted(
    shared: &Shared,
    gh: &GitHub,
    client: &str,
    review: &mut Review,
) -> Result<bool, Outcome> {
    let finalized_at = review.updated_at;
    match posted_review(shared, gh, &review.pr, &review.head_sha, finalized_at).await {
        Posted::Yes(url) => {
            let pr = review.pr.clone();
            finish_published(shared, review, &pr, client, Some(url));
            Ok(true)
        }
        Posted::Recorded(_) => {
            let pr = review.pr.clone();
            settle_published(shared, review, &pr);
            Ok(true)
        }
        Posted::No => Ok(false),
        Posted::Unknown(e) => Err(unknown_outcome(&e)),
    }
}

/// The answer for a review left `publishing` because GitHub could not say whether it has it.
fn unknown_outcome(e: &ProviderError) -> Outcome {
    Outcome::Err(ProtocolError::new(
        ErrorCode::Upstream,
        format!(
            "cannot tell whether your last publish reached GitHub ({e}); try again when you are online"
        ),
    ))
}

/// Appended when a cleanup failed and GitHub may still hold our pending review.
const LEFTOVER: &str =
    "a pending review may remain on GitHub; discard it there before publishing again";

/// Why the review did not reach GitHub.
struct SubmitFailure {
    error: ProviderError,
    /// The submit was sent and the pending review could not be deleted afterwards: GitHub
    /// may have the review anyway, whatever the submit answered.
    maybe_posted: bool,
    /// A pending review we started may still be on GitHub (its cleanup failed).
    leftover: bool,
}

impl SubmitFailure {
    fn into_outcome(self) -> Outcome {
        let Self {
            error,
            maybe_posted,
            leftover,
        } = self;
        let message = match (maybe_posted, leftover) {
            (true, true) => format!(
                "the review may have been posted; check the pull request on GitHub before publishing again ({error}); if it was not, {LEFTOVER}"
            ),
            (true, false) => format!(
                "the review may have been posted; check the pull request on GitHub before publishing again ({error})"
            ),
            (false, true) => format!("{error}; {LEFTOVER}"),
            (false, false) => return provider_error(error),
        };
        Outcome::Err(ProtocolError::new(ErrorCode::Upstream, message))
    }
}

/// Deletes a pending review we started; `true` when it may still be on GitHub.
async fn discard_pending(gh: &GitHub, pr: &PrRef, review_id: &str) -> bool {
    match gh.delete_pending_review(review_id).await {
        Ok(()) => false,
        Err(e) => {
            tracing::warn!(error = %e, pr = %pr, "cannot delete the pending review");
            true
        }
    }
}

/// After an ambiguous start, GitHub may hold a pending review we never heard of. None existed
/// before (publishing checks first), so one there now is ours: delete it. Returns `true`
/// when a pending review may remain.
async fn discard_orphan(gh: &GitHub, pr: &PrRef) -> bool {
    match gh.pr_node(pr).await {
        Ok(node) => match node.pending {
            Some(pending) => discard_pending(gh, pr, &pending.id).await,
            None => false,
        },
        Err(e) => {
            tracing::warn!(error = %e, pr = %pr, "cannot check for a pending review after a failed start");
            true
        }
    }
}

/// Adds every reply to the pending review, stopping at the first refusal.
async fn reply_all(
    gh: &GitHub,
    review_id: &str,
    replies: &[ReplyPayload],
) -> Result<(), ProviderError> {
    for reply in replies {
        gh.reply_in_review(review_id, &reply.thread, &reply.body)
            .await?;
    }
    Ok(())
}

/// Pending review with the line comments → each reply → submit. Any failure once the
/// pending review exists deletes it (best effort), so GitHub keeps nothing half-done.
async fn submit_review(
    gh: &GitHub,
    pr: &PrRef,
    pr_node: &str,
    payload: &ReviewPayload,
    replies: &[ReplyPayload],
) -> Result<PublishedReview, SubmitFailure> {
    let pending = match gh
        .start_review(pr_node, &payload.commit_id, &payload.comments)
        .await
    {
        Ok(pending) => pending,
        Err(error) => {
            let leftover = error.is_ambiguous() && discard_orphan(gh, pr).await;
            return Err(SubmitFailure {
                error,
                maybe_posted: false,
                leftover,
            });
        }
    };
    let (error, submitted) = match reply_all(gh, &pending.id, replies).await {
        Err(error) => (error, false),
        Ok(()) => match gh
            .submit_review(&pending.id, &payload.event, &payload.body)
            .await
        {
            Ok(published) => return Ok(published),
            Err(error) => (error, true),
        },
    };
    // A submitted review is no longer pending: when the delete works, nothing was posted. A 5xx
    // is no proof either way (a gateway may time out on a submit GitHub did take), so any failed
    // delete after a submit means it may be posted.
    let leftover = discard_pending(gh, pr, &pending.id).await;
    Err(SubmitFailure {
        error,
        maybe_posted: submitted && leftover,
        leftover,
    })
}

/// Resolves every marked thread; returns the ids GitHub left open and the last error.
async fn resolve_threads(
    gh: &GitHub,
    pr: &PrRef,
    threads: &[String],
) -> (Vec<String>, Option<ProviderError>) {
    let mut unresolved = Vec::new();
    let mut last_error = None;
    for thread in threads {
        if let Err(e) = gh.resolve_thread(thread).await {
            tracing::warn!(error = %e, pr = %pr, thread = %thread, "cannot resolve a review thread");
            unresolved.push(thread.clone());
            last_error = Some(e);
        }
    }
    (unresolved, last_error)
}

pub(crate) async fn publish(
    shared: &Shared,
    client: &str,
    pr: &PrRef,
    verdict: Verdict,
    summary: &str,
) -> Outcome {
    let gh = match github_client(shared).await {
        Ok(Some(gh)) => gh,
        Ok(None) => return no_token(),
        Err(e) => return provider_error(e),
    };
    // Before the lock: a turn may be waiting for it, and could not see the stop under it. A
    // publish that then fails leaves the session stored, so the next question resumes it.
    crate::agent::stop(shared, pr).await;
    let _guard = lock(shared, pr).await;
    let mut review = match load_existing(shared, pr) {
        Ok(r) => r,
        Err(out) => return out,
    };
    if review.state == ReviewState::Publishing {
        return invalid_state("a publish is in progress or was interrupted; open the review again");
    }
    if review.state == ReviewState::Outdated {
        return conflict(
            "the pull request changed since you saved this review; open it again to check the moved comments",
        );
    }
    let obsolete = review
        .draft
        .items
        .iter()
        .filter(|i| matches!(i.status, ItemStatus::Obsolete { .. }))
        .count();
    if obsolete > 0 {
        return Outcome::Err(ProtocolError::new(
            ErrorCode::BadRequest,
            format!("{obsolete} comment(s) are obsolete; remove or re-add them before publishing"),
        ));
    }
    let detail = match gh.get_pr(pr).await {
        Ok(d) => d,
        Err(e) => return provider_error(e),
    };
    if detail.head_sha != review.head_sha {
        shared.touch(pr);
        let checkout = {
            let _serialized = shared.worktree_lock.lock().await;
            worktrees::checkout(shared, pr, &detail).await
        };
        if let Err(e) = &checkout {
            tracing::warn!(error = %e, pr = %pr, "cannot check out the new head to re-anchor the review");
        }
        let Ok((info, remote)) = checkout else {
            return conflict(
                "the pull request changed since you started this review; open it again to re-anchor your comments",
            );
        };
        {
            let repo = PathBuf::from(&info.clone);
            relocate::refresh_base(&repo, &remote, pr, &detail.base_ref).await;
            let files = match files_for(shared, pr, &detail.head_sha).await {
                Ok(f) => Some(f),
                Err(_) => {
                    tracing::warn!(pr = %pr, "cannot load the files to check the re-anchored comments");
                    None
                }
            };
            relocate::relocate_review(
                &mut review,
                &detail,
                &repo,
                files.as_deref().map(Vec::as_slice),
            )
            .await;
            let _ = review.apply(ReviewEvent::NewHead, now_unix());
            if let Err(out) = save(shared, &review) {
                return out;
            }
            if let Err(e) = pin_commit(&repo, &reviewed_ref(pr.number), &review.head_sha).await {
                tracing::warn!(error = %e, pr = %pr, "cannot pin the reviewed commit");
            }
            announce(shared, &review);
        }
        return conflict(
            "the pull request has new commits; your comments were re-anchored — check them and publish again",
        );
    }
    let viewer = match gh.viewer().await {
        Ok(v) => v.login,
        Err(e) => return provider_error(e),
    };
    let role = if viewer.eq_ignore_ascii_case(&detail.summary.author) {
        Role::Author
    } else {
        Role::Reviewer
    };
    let plan = match plan_publish(&review, verdict, summary, role) {
        Ok(p) => p,
        Err(e) => return Outcome::Err(ProtocolError::new(ErrorCode::BadRequest, e.to_string())),
    };
    // GitHub allows one pending review per user and pull request. One started on github.com
    // may hold comments written there: never reuse or delete it; ask the user to finish it.
    // The pull request's node is bound to the review it carries: no review, no node.
    let posting = match &plan.review {
        Some(payload) => match gh.pr_node(pr).await {
            Ok(node) => {
                if let Some(pending) = node.pending {
                    return invalid_state(format!(
                        "You already have a pending review on GitHub for this pull request. Submit or discard it there, then publish again: {}",
                        pending.url
                    ));
                }
                Some((payload, node.id))
            }
            Err(e) => return provider_error(e),
        },
        None => None,
    };
    if let Err(e) = review.apply(ReviewEvent::Finalize, now_unix()) {
        return invalid_state(e.to_string());
    }
    if let Err(out) = save(shared, &review) {
        return out;
    }
    let finalized_at = review.updated_at;

    let mut url = None;
    let mut posted = false;
    if let Some((payload, pr_node)) = posting {
        match submit_review(&gh, pr, &pr_node, payload, &plan.replies).await {
            Ok(published) => {
                // Persist the terminal state before any further network call, so a crash while
                // resolving or closing cannot leave a `publishing` file that invites a re-post.
                finish_published(shared, &mut review, pr, client, Some(published.url.clone()));
                url = Some(published.url);
                posted = true;
            }
            Err(failure) => {
                // An answer that was lost or unreadable may hide a review GitHub did take.
                let found = if failure.error.is_ambiguous() || failure.maybe_posted {
                    match posted_review(shared, &gh, pr, &review.head_sha, finalized_at).await {
                        Posted::Yes(found) | Posted::Recorded(found) => Some(found),
                        Posted::No => None,
                        // Back in `saved`, one click would post it a second time: the review
                        // stays `publishing` until opening it again can ask GitHub.
                        Posted::Unknown(e) if failure.maybe_posted => {
                            announce(shared, &review);
                            return unknown_outcome(&e);
                        }
                        Posted::Unknown(_) => None,
                    }
                } else {
                    None
                };
                let Some(found) = found else {
                    fail_publish(shared, &mut review);
                    announce(shared, &review);
                    return failure.into_outcome();
                };
                finish_published(shared, &mut review, pr, client, Some(found.clone()));
                url = Some(found);
                posted = true;
            }
        }
    }
    let (unresolved, resolve_error) = resolve_threads(&gh, pr, &plan.resolves).await;
    if !posted && !plan.resolves.is_empty() {
        // Resolve-only: it counts as published once GitHub took at least one resolve.
        if unresolved.len() == plan.resolves.len() {
            fail_publish(shared, &mut review);
            announce(shared, &review);
            let reason = resolve_error.map(|e| e.to_string()).unwrap_or_default();
            return Outcome::Err(ProtocolError::new(
                ErrorCode::Upstream,
                format!("no thread could be resolved on GitHub: {reason}"),
            ));
        }
        finish_published(shared, &mut review, pr, client, None);
        posted = true;
    }
    let mut close_error = None;
    if plan.close
        && let Err(e) = gh.close_pr(pr).await
    {
        if !posted {
            fail_publish(shared, &mut review);
            announce(shared, &review);
            return provider_error(e);
        }
        close_error = Some(e);
    }
    if !posted {
        finish_published(shared, &mut review, pr, client, None);
    }
    crate::agent::forget(shared, pr);
    cleanup_checkout(shared, pr).await;
    shared.files_cache.lock().await.remove(pr);
    announce(shared, &review);
    // The review is published either way; a failed close is reported, not raised.
    Outcome::Ok(Reply::Published(PublishResult {
        url,
        closed: plan.close && close_error.is_none(),
        unresolved,
        close_error: close_error.map(|e| e.to_string()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(id: u64, who: &str, state: &str, commit: Option<&str>, at: &str) -> ReviewInfo {
        ReviewInfo {
            id,
            author: who.into(),
            state: state.into(),
            body: String::new(),
            submitted_at: Some(at.into()),
            url: format!("https://github.com/acme/widgets/pull/7#pullrequestreview-{id}"),
            commit_id: commit.map(String::from),
        }
    }

    // 2026-10-07T12:00:00Z
    const FINALIZED: i64 = 1_791_374_400;

    #[test]
    fn finds_your_review_on_the_head_after_finalize() {
        let reviews = [
            review(1, "mona", "APPROVED", Some("abc"), "2026-10-07T12:00:05Z"),
            review(2, "me", "COMMENTED", Some("old"), "2026-10-07T12:00:05Z"),
            review(3, "ME", "COMMENTED", Some("abc"), "2026-10-07T12:00:05Z"),
        ];
        assert_eq!(
            find_posted(&reviews, "me", "abc", FINALIZED, &[]).map(|r| r.id),
            Some(3),
            "the login is compared without case"
        );
    }

    #[test]
    fn ignores_reviews_from_before_finalize_and_pending_ones() {
        let reviews = [
            review(1, "me", "COMMENTED", Some("abc"), "2026-10-07T11:00:00Z"),
            review(2, "me", "PENDING", Some("abc"), "2026-10-07T12:00:05Z"),
            review(3, "me", "COMMENTED", None, "2026-10-07T12:00:05Z"),
            review(4, "me", "COMMENTED", Some("abc"), "not a date"),
        ];
        assert!(find_posted(&reviews, "me", "abc", FINALIZED, &[]).is_none());
    }

    #[test]
    fn a_clock_a_little_behind_still_matches() {
        let at = "2026-10-07T11:59:45Z"; // 15 s before the Finalize moment we recorded
        let reviews = [review(1, "me", "COMMENTED", Some("abc"), at)];
        assert!(find_posted(&reviews, "me", "abc", FINALIZED, &[]).is_some());
        let too_old = [review(
            1,
            "me",
            "COMMENTED",
            Some("abc"),
            "2026-10-07T11:59:00Z",
        )];
        assert!(find_posted(&too_old, "me", "abc", FINALIZED, &[]).is_none());
    }

    #[test]
    fn the_latest_matching_review_wins() {
        let reviews = [
            review(5, "me", "COMMENTED", Some("abc"), "2026-10-07T12:00:05Z"),
            review(9, "me", "COMMENTED", Some("abc"), "2026-10-07T12:00:09Z"),
        ];
        assert_eq!(
            find_posted(&reviews, "me", "abc", FINALIZED, &[]).map(|r| r.id),
            Some(9)
        );
    }

    fn activity(kind: ActivityKind, pr: &str, url: Option<&str>) -> clusia_core::Activity {
        clusia_core::Activity {
            ts: FINALIZED,
            kind,
            pr: pr.parse().unwrap(),
            client: "clusia".into(),
            url: url.map(String::from),
            note: None,
        }
    }

    #[test]
    fn reviews_published_before_the_last_open_are_earlier_ones() {
        let pr: PrRef = "acme/widgets#7".parse().unwrap();
        let log = [
            activity(ActivityKind::ReviewOpened, "acme/widgets#7", None),
            activity(ActivityKind::ReviewPublished, "acme/widgets#7", Some("u1")),
            activity(ActivityKind::ReviewOpened, "acme/widgets#7", None),
            activity(
                ActivityKind::ReviewPublished,
                "acme/widgets#8",
                Some("other"),
            ),
            activity(ActivityKind::ReviewPublished, "acme/widgets#7", Some("u2")),
        ];
        assert_eq!(
            split_recorded(&log, &pr),
            RecordedReviews {
                earlier: vec!["u1".into()],
                current: vec!["u2".into()],
            }
        );
        assert_eq!(
            split_recorded(&log[..3], &pr).current,
            Vec::<String>::new(),
            "a fresh review has published nothing yet"
        );
    }

    #[test]
    fn a_review_already_recorded_is_never_adopted_again() {
        let first = review(5, "me", "COMMENTED", Some("abc"), "2026-10-07T12:00:05Z");
        let recorded = [first.url.clone()];
        assert!(find_posted(&[first], "me", "abc", FINALIZED, &recorded).is_none());
    }
}
