//! Publishing a draft as one GitHub review (spec §6.4), through GraphQL:
//! a pending review with the line comments, the replies inside it, the submit, then the resolves.

use std::path::PathBuf;

use clusia_core::{
    ActivityKind, ItemStatus, PrRef, ReplyPayload, Review, ReviewEvent, ReviewPayload, ReviewState,
    Role, Verdict, plan_publish,
};
use clusia_git::{pin_commit, reviewed_ref};
use clusia_protocol::{ErrorCode, Outcome, ProtocolError, PublishResult, Reply};
use clusia_provider::{GitHub, ProviderError, PublishedReview};

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
fn finish_published(
    shared: &Shared,
    review: &mut Review,
    pr: &PrRef,
    client: &str,
    url: Option<String>,
) {
    if let Err(e) = review.apply(ReviewEvent::PublishOk, now_unix()) {
        tracing::warn!(error = %e, pr = %pr, "unexpected state after publishing");
    }
    if let Err(e) = clusia_store::delete_review(&shared.paths, pr) {
        tracing::warn!(error = %e, pr = %pr, "published, but cannot delete the review file");
    }
    drop_cache(shared, pr);
    record(shared, ActivityKind::ReviewPublished, pr, client, url, None);
}

/// Appended when a cleanup failed and GitHub may still hold our pending review.
const LEFTOVER: &str =
    "a pending review may remain on GitHub; discard it there before publishing again";

/// Why the review did not reach GitHub.
struct SubmitFailure {
    error: ProviderError,
    /// The submit was sent, its answer was lost and the pending review could not be
    /// deleted: GitHub may have the review anyway.
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
    let (error, unsure) = match reply_all(gh, &pending.id, replies).await {
        Err(error) => (error, false),
        Ok(()) => match gh
            .submit_review(&pending.id, &payload.event, &payload.body)
            .await
        {
            Ok(published) => return Ok(published),
            Err(error) => {
                let unsure = error.is_ambiguous();
                (error, unsure)
            }
        },
    };
    // A submitted review is no longer pending: when the delete works, nothing was posted.
    let leftover = discard_pending(gh, pr, &pending.id).await;
    Err(SubmitFailure {
        error,
        maybe_posted: unsure && leftover,
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
                fail_publish(shared, &mut review);
                announce(shared, &review);
                return failure.into_outcome();
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
    cleanup_checkout(shared, pr).await;
    shared.files_cache.lock().await.remove(pr);
    announce(shared, &review);
    if let Some(e) = close_error {
        let message = format!(
            "review published ({}), but closing the pull request failed: {e}",
            url.unwrap_or_default()
        );
        return Outcome::Err(ProtocolError::new(ErrorCode::Upstream, message));
    }
    Outcome::Ok(Reply::Published(PublishResult {
        url,
        closed: plan.close,
        unresolved,
    }))
}
