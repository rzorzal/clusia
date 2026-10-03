//! The review lifecycle as daemon commands (spec §6).

use std::path::PathBuf;
use std::sync::Arc;

use clusia_core::{Activity, ActivityKind, PrRef, Review, ReviewEvent, Role};
use clusia_git::{base_pin_ref, fetch_branch, pin_commit, reviewed_ref};
use clusia_protocol::{
    ErrorCode, Event, FileSummary, LoadStep, LoadStepKind, Outcome, ProtocolError, Reply,
    ReviewView, StepStatus, topics,
};
use clusia_store::{ReviewLoad, append_activity, load_review, save_review};

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

#[allow(dead_code)] // used by the later review commands
pub(crate) fn load(shared: &Shared, pr: &PrRef) -> Option<Review> {
    match load_review(&shared.paths, pr) {
        Ok(ReviewLoad::Found(r)) => Some(r),
        Ok(ReviewLoad::Missing) => None,
        Ok(ReviewLoad::Quarantined { path, .. }) => {
            tracing::warn!(file = %path.display(), "review file was corrupt and has been set aside");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, pr = %pr, "cannot read the review file");
            None
        }
    }
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
    if let Ok(sha) = fetch_branch(&repo, &remote, &detail.base_ref).await
        && let Err(e) = pin_commit(&repo, &base_pin_ref(pr.number), &sha).await
    {
        tracing::warn!(error = %e, pr = %pr, "cannot pin the base commit");
    }

    step(shared, pr, LoadStepKind::Pr, StepStatus::Running, None);
    let files = match gh.get_files(pr).await {
        Ok(f) => Arc::new(f),
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
        let report = relocate::relocate_review(&mut review, &detail, &repo).await;
        let _ = review.apply(ReviewEvent::NewHead, now);
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
    let files = files.iter().map(FileSummary::from).collect();
    Outcome::Ok(Reply::Review(Box::new(ReviewView {
        review,
        pr: detail,
        files,
        role,
        worktree: Some(info.path),
        viewer,
    })))
}
