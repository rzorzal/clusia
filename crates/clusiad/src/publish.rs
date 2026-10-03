//! Publishing a draft as one GitHub review (spec §6.4).

use std::path::PathBuf;

use clusia_core::{
    ActivityKind, PrRef, Review, ReviewEvent, ReviewState, Role, Verdict, plan_publish,
};
use clusia_git::{pin_commit, reviewed_ref};
use clusia_protocol::{ErrorCode, Outcome, ProtocolError, PublishResult, Reply};

use crate::handlers::{no_token, provider_error};
use crate::reviews::{
    announce, cleanup_checkout, invalid_state, load_existing, lock, record, save,
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
    if review.state == ReviewState::Outdated {
        return conflict(
            "the pull request changed since you saved this review; open it again to check the moved comments",
        );
    }
    let detail = match gh.get_pr(pr).await {
        Ok(d) => d,
        Err(e) => return provider_error(e),
    };
    if detail.head_sha != review.head_sha {
        let checkout = {
            let _serialized = shared.worktree_lock.lock().await;
            worktrees::checkout(shared, pr, &detail).await
        };
        if let Err(e) = &checkout {
            tracing::warn!(error = %e, pr = %pr, "cannot check out the new head to re-anchor the review");
        }
        if let Ok((info, _remote)) = checkout {
            let repo = PathBuf::from(&info.clone);
            relocate::relocate_review(&mut review, &detail, &repo).await;
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
    if let Err(e) = review.apply(ReviewEvent::Finalize, now_unix()) {
        return invalid_state(e.to_string());
    }
    if let Err(out) = save(shared, &review) {
        return out;
    }

    let mut url = None;
    if let Some(payload) = &plan.review {
        match gh.create_review(pr, payload).await {
            Ok(published) => url = Some(published.url),
            Err(e) => {
                fail_publish(shared, &mut review);
                announce(shared, &review);
                return provider_error(e);
            }
        }
    }
    let mut close_error = None;
    if plan.close
        && let Err(e) = gh.close_pr(pr).await
    {
        if url.is_none() {
            fail_publish(shared, &mut review);
            announce(shared, &review);
            return provider_error(e);
        }
        close_error = Some(e);
    }

    if let Err(e) = review.apply(ReviewEvent::PublishOk, now_unix()) {
        tracing::warn!(error = %e, pr = %pr, "unexpected state after publishing");
    }
    // The review is already on GitHub: a failure here must not turn into an error reply.
    if let Err(e) = clusia_store::delete_review(&shared.paths, pr) {
        tracing::warn!(error = %e, pr = %pr, "published, but cannot delete the review file");
    }
    cleanup_checkout(shared, pr).await;
    shared.files_cache.lock().await.remove(pr);
    record(
        shared,
        ActivityKind::ReviewPublished,
        pr,
        client,
        url.clone(),
        None,
    );
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
    }))
}
