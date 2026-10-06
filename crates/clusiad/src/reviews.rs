//! The review lifecycle as daemon commands (spec §6).

use std::path::PathBuf;
use std::sync::Arc;

use clusia_core::{
    Activity, ActivityKind, Anchor, DraftKind, FileDiff, PrRef, Review, ReviewCache, ReviewEvent,
    ReviewState, Role, Side, ThreadRef, can_comment,
};
use clusia_git::{
    base_pin_ref, pin_commit, remove_worktree, repo_of_worktree, reviewed_ref, unpin,
};
use clusia_protocol::{
    AnchorInput, CachedReview, ErrorCode, Event, FileSummary, LoadStep, LoadStepKind, Outcome,
    ProtocolError, Reply, ReviewSummary, ReviewView, StepStatus, topics,
};
use clusia_store::{
    ReviewLoad, append_activity, delete_review, delete_review_cache, list_reviews, load_review,
    load_review_cache, save_review, save_review_cache,
};

use crate::handlers::{no_token, provider_error};
use crate::state::Shared;
use crate::sync::{github_client, now_unix};
use crate::{relocate, worktrees};

pub(crate) async fn lock(shared: &Shared, pr: &PrRef) -> tokio::sync::OwnedMutexGuard<()> {
    let mutex = {
        let mut locks = shared
            .review_locks
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        locks
            .entry(pr.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    mutex.lock_owned().await
}

#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
pub(crate) fn save(shared: &Shared, review: &Review) -> Result<(), Outcome> {
    save_review(&shared.paths, review).map_err(|e| {
        Outcome::Err(ProtocolError::new(
            ErrorCode::Internal,
            format!("could not save the review: {e}"),
        ))
    })
}

pub(crate) fn record(
    shared: &Shared,
    kind: ActivityKind,
    pr: &PrRef,
    client: &str,
    url: Option<String>,
    note: Option<String>,
) {
    let activity = Activity {
        ts: now_unix(),
        kind,
        pr: pr.clone(),
        client: client.to_string(),
        url,
        note,
    };
    if let Err(e) = append_activity(&shared.paths, &activity) {
        tracing::warn!(error = %e, "cannot append to the activity log");
    }
}

pub(crate) fn announce(shared: &Shared, review: &Review) {
    shared.publish(
        topics::REVIEWS,
        Event::ReviewChanged {
            pr: review.pr.clone(),
            state: review.state,
            items: review.draft.items.len(),
        },
    );
}

fn step(
    shared: &Shared,
    pr: &PrRef,
    step: LoadStepKind,
    status: StepStatus,
    message: Option<String>,
) {
    shared.publish(
        topics::REVIEWS,
        Event::LoadStep(LoadStep {
            pr: pr.clone(),
            step,
            status,
            message,
        }),
    );
}

pub(crate) fn invalid_state(message: impl Into<String>) -> Outcome {
    Outcome::Err(ProtocolError::new(ErrorCode::InvalidState, message))
}

pub(crate) async fn open(shared: &Shared, client: &str, pr: &PrRef) -> Outcome {
    let gh = match github_client(shared).await {
        Ok(Some(gh)) => gh,
        Ok(None) => return no_token(),
        Err(e) => return provider_error(e),
    };
    step(shared, pr, LoadStepKind::Repo, StepStatus::Running, None);
    let mut detail = match gh.get_pr(pr).await {
        Ok(d) => d,
        Err(e) => {
            step(
                shared,
                pr,
                LoadStepKind::Repo,
                StepStatus::Failed,
                Some(e.to_string()),
            );
            return provider_error(e);
        }
    };
    shared.touch(pr);
    let checkout = {
        let _serialized = shared.worktree_lock.lock().await;
        worktrees::checkout(shared, pr, &detail).await
    };
    let (info, remote) = match checkout {
        Ok(x) => x,
        Err(e) => {
            step(
                shared,
                pr,
                LoadStepKind::Repo,
                StepStatus::Failed,
                Some(e.to_string()),
            );
            return Outcome::Err(ProtocolError::new(ErrorCode::Git, e.to_string()));
        }
    };
    if info.head_sha != detail.head_sha {
        // A push landed between get_pr and the fetch; accept it only if GitHub now agrees.
        match gh.get_pr(pr).await {
            Ok(d) if d.head_sha == info.head_sha => detail = d,
            _ => {
                let msg = "the pull request changed while opening; try again";
                step(
                    shared,
                    pr,
                    LoadStepKind::Repo,
                    StepStatus::Failed,
                    Some(msg.into()),
                );
                return Outcome::Err(ProtocolError::new(ErrorCode::Conflict, msg));
            }
        }
    }
    step(
        shared,
        pr,
        LoadStepKind::Repo,
        StepStatus::Done,
        Some(info.clone.clone()),
    );
    step(
        shared,
        pr,
        LoadStepKind::Branch,
        StepStatus::Done,
        Some(info.path.clone()),
    );
    let repo = PathBuf::from(&info.clone);
    relocate::refresh_base(&repo, &remote, pr, &detail.base_ref).await;

    step(shared, pr, LoadStepKind::Pr, StepStatus::Running, None);
    let (files, conversation, checks) = tokio::join!(
        gh.get_files(pr),
        gh.get_conversation(pr),
        gh.get_checks(pr, &detail.head_sha),
    );
    let checks = checks
        .inspect_err(|e| tracing::warn!(error = %e, pr = %pr, "cannot read the checks"))
        .ok();
    let (files, conversation) = match files.and_then(|f| Ok((f, conversation?))) {
        Ok((f, c)) => (Arc::new(f), c),
        Err(e) => {
            step(
                shared,
                pr,
                LoadStepKind::Pr,
                StepStatus::Failed,
                Some(e.to_string()),
            );
            return provider_error(e);
        }
    };
    shared
        .files_cache
        .lock()
        .await
        .insert(pr.clone(), (detail.head_sha.clone(), files.clone()));
    step(
        shared,
        pr,
        LoadStepKind::Pr,
        StepStatus::Done,
        Some(format!("{} files", files.len())),
    );
    step(
        shared,
        pr,
        LoadStepKind::Agent,
        StepStatus::Skipped,
        Some("arrives with the harness (SP2)".into()),
    );

    let viewer = gh.viewer().await.ok().map(|v| v.login);

    let _guard = lock(shared, pr).await;
    let now = now_unix();
    let stored = match load_review(&shared.paths, pr) {
        Ok(ReviewLoad::Found(r)) => Some(r),
        Ok(ReviewLoad::Missing) => None,
        Ok(ReviewLoad::Quarantined { path, .. }) => {
            tracing::warn!(file = %path.display(), "review file was corrupt and has been set aside");
            None
        }
        Err(e) => {
            return Outcome::Err(ProtocolError::new(
                ErrorCode::Internal,
                format!("cannot read the stored review for {pr}: {e}; it was left untouched"),
            ));
        }
    };
    let mut review = match stored {
        Some(r) if !r.state.is_terminal() => r,
        _ => {
            record(shared, ActivityKind::ReviewOpened, pr, client, None, None);
            Review::new(
                pr.clone(),
                detail.summary.title.clone(),
                detail.base_sha.clone(),
                detail.head_sha.clone(),
                now,
            )
        }
    };
    if review.state == clusia_core::ReviewState::Publishing {
        let _ = review.apply(ReviewEvent::PublishFailed, now);
    }
    if review.head_sha != detail.head_sha || review.base_sha != detail.base_sha {
        let head_changed = review.head_sha != detail.head_sha;
        let report =
            relocate::relocate_review(&mut review, &detail, &repo, Some(files.as_slice())).await;
        // A base that only moved ahead changes nothing for the draft: take it silently.
        if head_changed || report.moved + report.obsolete > 0 {
            let _ = review.apply(ReviewEvent::NewHead, now);
        }
        if report.moved + report.obsolete > 0 {
            let note = format!("{} moved, {} obsolete", report.moved, report.obsolete);
            record(
                shared,
                ActivityKind::ReviewOutdated,
                pr,
                "",
                None,
                Some(note),
            );
        }
    }
    review.title = detail.summary.title.clone();
    if let Err(e) = review.apply(ReviewEvent::Reopen, now) {
        return invalid_state(e.to_string());
    }
    if let Err(out) = save(shared, &review) {
        return out;
    }
    if let Err(e) = pin_commit(&repo, &reviewed_ref(pr.number), &review.head_sha).await {
        tracing::warn!(error = %e, pr = %pr, "cannot pin the reviewed commit");
    }
    announce(shared, &review);

    let role = match &viewer {
        Some(login) if login.eq_ignore_ascii_case(&detail.summary.author) => Role::Author,
        _ => Role::Reviewer,
    };
    let cache = ReviewCache {
        pr: detail,
        files: files.to_vec(),
        conversation,
        checks,
        role,
        viewer,
        worktree: Some(info.path),
        fetched_at: now,
    };
    if let Err(e) = save_review_cache(&shared.paths, pr, &cache) {
        tracing::warn!(error = %e, pr = %pr, "cannot write the review cache");
    }
    Outcome::Ok(Reply::Review(Box::new(view_of(review, cache))))
}

/// The view the window shows, from a review and what GitHub said about its pull request.
fn view_of(review: Review, cache: ReviewCache) -> ReviewView {
    ReviewView {
        review,
        pr: cache.pr,
        files: cache.files.iter().map(FileSummary::from).collect(),
        role: cache.role,
        worktree: cache.worktree,
        viewer: cache.viewer,
        checks: cache.checks,
        conversation: Some(cache.conversation),
        diff: cache.files,
    }
}

/// The review as the last successful open saw it; never touches the network.
pub(crate) async fn cached(shared: &Shared, pr: &PrRef) -> Outcome {
    let not_found = || {
        Outcome::Err(ProtocolError::new(
            ErrorCode::NotFound,
            format!("no cached copy of {pr}"),
        ))
    };
    let cache = match load_review_cache(&shared.paths, pr) {
        Ok(Some(c)) => c,
        Ok(None) => return not_found(),
        Err(e) => {
            tracing::warn!(error = %e, pr = %pr, "cannot read the review cache");
            return not_found();
        }
    };
    let review = match load_stored(shared, pr) {
        Ok(Some(r)) => r,
        Ok(None) => return not_found(),
        Err(out) => return out,
    };
    let fetched_at = cache.fetched_at;
    Outcome::Ok(Reply::Cached(Box::new(CachedReview {
        view: view_of(review, cache),
        fetched_at,
    })))
}

/// Forgets the cached copy of `pr` (best effort): its review is gone.
pub(crate) fn drop_cache(shared: &Shared, pr: &PrRef) {
    if let Err(e) = delete_review_cache(&shared.paths, pr) {
        tracing::warn!(error = %e, pr = %pr, "cannot delete the review cache");
    }
}

/// Loads the stored review; `Ok(None)` when there is none (or it was quarantined).
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
pub(crate) fn load_stored(shared: &Shared, pr: &PrRef) -> Result<Option<Review>, Outcome> {
    match load_review(&shared.paths, pr) {
        Ok(ReviewLoad::Found(r)) => Ok(Some(r)),
        Ok(ReviewLoad::Missing) => Ok(None),
        Ok(ReviewLoad::Quarantined { path, .. }) => {
            tracing::warn!(file = %path.display(), "review file was corrupt and has been set aside");
            Ok(None)
        }
        Err(e) => {
            tracing::warn!(error = %e, pr = %pr, "cannot read the review file");
            Err(Outcome::Err(ProtocolError::new(
                ErrorCode::Internal,
                format!("cannot read the stored review for {pr}"),
            )))
        }
    }
}

/// Loads the review a mutation command acts on; it must already exist.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
pub(crate) fn load_existing(shared: &Shared, pr: &PrRef) -> Result<Review, Outcome> {
    load_stored(shared, pr)?.ok_or_else(|| no_review(pr))
}

fn bad_request(message: impl Into<String>) -> Outcome {
    Outcome::Err(ProtocolError::new(ErrorCode::BadRequest, message))
}

fn no_review(pr: &PrRef) -> Outcome {
    invalid_state(format!("no review for {pr}; open it first"))
}

fn editable(review: &Review) -> bool {
    matches!(
        review.state,
        ReviewState::Active | ReviewState::Saved | ReviewState::Outdated | ReviewState::Revalidated
    )
}

/// PR files for `head`, from the cache or GitHub.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
pub(crate) async fn files_for(
    shared: &Shared,
    pr: &PrRef,
    head: &str,
) -> Result<Arc<Vec<FileDiff>>, Outcome> {
    if let Some((cached_head, files)) = shared.files_cache.lock().await.get(pr)
        && cached_head == head
    {
        return Ok(files.clone());
    }
    let gh = match github_client(shared).await {
        Ok(Some(gh)) => gh,
        Ok(None) => return Err(no_token()),
        Err(e) => return Err(provider_error(e)),
    };
    let files = Arc::new(gh.get_files(pr).await.map_err(provider_error)?);
    shared
        .files_cache
        .lock()
        .await
        .insert(pr.clone(), (head.to_string(), files.clone()));
    Ok(files)
}

#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
async fn resolve_anchor(
    shared: &Shared,
    review: &Review,
    input: AnchorInput,
) -> Result<Anchor, Outcome> {
    let files = files_for(shared, &review.pr, &review.head_sha).await?;
    let file = files.iter().find(|f| f.path == input.path).ok_or_else(|| {
        bad_request(format!(
            "{} is not changed in this pull request",
            input.path
        ))
    })?;
    let patch = file.patch.as_deref().ok_or_else(|| {
        bad_request(format!(
            "{} has no diff to comment on (binary or too large)",
            input.path
        ))
    })?;
    for line in [input.start_line, Some(input.line)].into_iter().flatten() {
        if !can_comment(patch, input.side, line) {
            return Err(bad_request(format!(
                "line {line} of {} is not part of this pull request's diff",
                input.path
            )));
        }
    }
    let commit = match input.side {
        Side::Right => review.head_sha.clone(),
        Side::Left => review.base_sha.clone(),
    };
    let anchor = Anchor {
        path: input.path,
        line: input.line,
        start_line: input.start_line,
        side: input.side,
        commit,
    };
    // Every line of a range must be commentable: GitHub rejects ranges that span two hunks.
    if let Some(start) = anchor.start_line
        && start <= anchor.line
        && !relocate::fits_diff(&files, &anchor)
    {
        return Err(bad_request(format!(
            "lines {start}-{} of {} are not one continuous part of this pull request's diff",
            anchor.line, anchor.path
        )));
    }
    Ok(anchor)
}

pub(crate) async fn get(shared: &Shared, pr: &PrRef) -> Outcome {
    match load_existing(shared, pr) {
        Ok(r) => Outcome::Ok(Reply::ReviewFile(Box::new(r))),
        Err(out) => out,
    }
}

pub(crate) async fn add_item(
    shared: &Shared,
    client: &str,
    pr: &PrRef,
    kind: DraftKind,
    anchor: Option<AnchorInput>,
    thread: Option<ThreadRef>,
    body: &str,
) -> Outcome {
    let _guard = lock(shared, pr).await;
    let mut review = match load_existing(shared, pr) {
        Ok(r) => r,
        Err(out) => return out,
    };
    if !editable(&review) {
        return invalid_state(format!(
            "the review of {pr} is {:?}; open it again",
            review.state
        ));
    }
    let anchor = match anchor {
        Some(input) => match resolve_anchor(shared, &review, input).await {
            Ok(a) => Some(a),
            Err(out) => return out,
        },
        None => None,
    };
    let now = now_unix();
    let item = match review.draft.add(kind, anchor, thread, body, now) {
        Ok(item) => item.clone(),
        Err(e) => return bad_request(e.to_string()),
    };
    review.updated_at = now;
    if let Err(out) = save(shared, &review) {
        return out;
    }
    record(shared, ActivityKind::ItemAdded, pr, client, None, None);
    announce(shared, &review);
    Outcome::Ok(Reply::DraftItem(item))
}

pub(crate) async fn update_item(shared: &Shared, pr: &PrRef, id: &str, body: &str) -> Outcome {
    let _guard = lock(shared, pr).await;
    let mut review = match load_existing(shared, pr) {
        Ok(r) => r,
        Err(out) => return out,
    };
    if !editable(&review) {
        return invalid_state(format!(
            "the review of {pr} is {:?}; open it again",
            review.state
        ));
    }
    if let Err(e) = review.draft.update_body(id, body) {
        return bad_request(e.to_string());
    }
    review.updated_at = now_unix();
    if let Err(out) = save(shared, &review) {
        return out;
    }
    announce(shared, &review);
    match review.draft.get(id) {
        Some(item) => Outcome::Ok(Reply::DraftItem(item.clone())),
        None => bad_request(format!("there is no draft item {id}")),
    }
}

pub(crate) async fn remove_item(shared: &Shared, pr: &PrRef, id: &str) -> Outcome {
    let _guard = lock(shared, pr).await;
    let mut review = match load_existing(shared, pr) {
        Ok(r) => r,
        Err(out) => return out,
    };
    if !editable(&review) {
        return invalid_state(format!(
            "the review of {pr} is {:?}; open it again",
            review.state
        ));
    }
    if let Err(e) = review.draft.remove(id) {
        return bad_request(e.to_string());
    }
    review.updated_at = now_unix();
    if let Err(out) = save(shared, &review) {
        return out;
    }
    announce(shared, &review);
    Outcome::Ok(Reply::Ack)
}

pub(crate) async fn close(shared: &Shared, client: &str, pr: &PrRef) -> Outcome {
    let _guard = lock(shared, pr).await;
    let mut review = match load_stored(shared, pr) {
        Ok(Some(r)) => r,
        Ok(None) => return Outcome::Ok(Reply::Ack),
        Err(out) => return out,
    };
    if review.state != ReviewState::Active {
        return Outcome::Ok(Reply::Ack);
    }
    if review.draft.is_empty() {
        if let Err(e) = delete_review(&shared.paths, pr) {
            return Outcome::Err(ProtocolError::new(
                ErrorCode::Internal,
                format!("could not delete the review file: {e}"),
            ));
        }
        drop_cache(shared, pr);
        return Outcome::Ok(Reply::Ack);
    }
    if let Err(e) = review.apply(ReviewEvent::Leave, now_unix()) {
        return invalid_state(e.to_string());
    }
    if let Err(out) = save(shared, &review) {
        return out;
    }
    record(shared, ActivityKind::ReviewSaved, pr, client, None, None);
    announce(shared, &review);
    Outcome::Ok(Reply::Ack)
}

/// Removes Clúsia's worktree and pins for `pr` (best effort).
pub(crate) async fn cleanup_checkout(shared: &Shared, pr: &PrRef) {
    let path = shared.paths.worktree_for(pr);
    let _serialized = shared.worktree_lock.lock().await;
    if let Ok(repo) = repo_of_worktree(&path).await {
        let _ = unpin(&repo, &reviewed_ref(pr.number)).await;
        let _ = unpin(&repo, &base_pin_ref(pr.number)).await;
        if let Err(e) = remove_worktree(&repo, &path).await {
            tracing::warn!(error = %e, "cannot remove the worktree");
        }
    } else if path.exists() {
        let _ = tokio::fs::remove_dir_all(&path).await;
    }
}

pub(crate) async fn discard(shared: &Shared, client: &str, pr: &PrRef) -> Outcome {
    let _guard = lock(shared, pr).await;
    let mut review = match load_existing(shared, pr) {
        Ok(r) => r,
        Err(out) => return out,
    };
    if let Err(e) = review.apply(ReviewEvent::Discard, now_unix()) {
        return invalid_state(e.to_string());
    }
    if let Err(e) = delete_review(&shared.paths, pr) {
        return Outcome::Err(ProtocolError::new(
            ErrorCode::Internal,
            format!("could not delete the review file: {e}"),
        ));
    }
    drop_cache(shared, pr);
    cleanup_checkout(shared, pr).await;
    record(
        shared,
        ActivityKind::ReviewDiscarded,
        pr,
        client,
        None,
        None,
    );
    review.draft.items.clear();
    announce(shared, &review);
    Outcome::Ok(Reply::Ack)
}

pub(crate) async fn list(shared: &Shared) -> Outcome {
    match list_reviews(&shared.paths) {
        Ok(reviews) => Outcome::Ok(Reply::Reviews(
            reviews
                .into_iter()
                .filter(|r| !r.state.is_terminal())
                .map(|r| ReviewSummary {
                    pr: r.pr,
                    title: r.title,
                    state: r.state,
                    items: r.draft.items.len(),
                    updated_at: r.updated_at,
                })
                .collect(),
        )),
        Err(e) => Outcome::Err(ProtocolError::new(
            ErrorCode::Internal,
            format!("cannot list reviews: {e}"),
        )),
    }
}

pub(crate) async fn diff(shared: &Shared, pr: &PrRef) -> Outcome {
    let gh = match github_client(shared).await {
        Ok(Some(gh)) => gh,
        Ok(None) => return no_token(),
        Err(e) => return provider_error(e),
    };
    let head = match gh.get_pr(pr).await {
        Ok(d) => d.head_sha,
        Err(e) => return provider_error(e),
    };
    match gh.get_files(pr).await {
        Ok(files) => {
            shared
                .files_cache
                .lock()
                .await
                .insert(pr.clone(), (head, Arc::new(files.clone())));
            Outcome::Ok(Reply::Diff(files))
        }
        Err(e) => provider_error(e),
    }
}

pub(crate) async fn conversation(shared: &Shared, pr: &PrRef) -> Outcome {
    let gh = match github_client(shared).await {
        Ok(Some(gh)) => gh,
        Ok(None) => return no_token(),
        Err(e) => return provider_error(e),
    };
    match gh.get_conversation(pr).await {
        Ok(c) => Outcome::Ok(Reply::Conversation(c)),
        Err(e) => provider_error(e),
    }
}
