//! What the user does with the proposals of a revalidation: read them, decide one or all of them,
//! and point a comment at another line. The draft changes only here, on a click.

use clusia_core::revalidation::{DecideError, accept_all, decide};
use clusia_core::{FileDiff, PrRef, Review};
use clusia_protocol::{AnchorInput, ErrorCode, Outcome, ProtocolError, Reply};

use crate::revalidate::{current, files_at, tell};
use crate::reviews::{self, announce, bad_request, editable, load_existing, lock, save};
use crate::state::Shared;
use crate::sync::now_unix;

/// `GetRevalidation`: the round of the review, with what the agent is doing now.
pub(crate) fn get(shared: &Shared, pr: &PrRef) -> Outcome {
    match load_existing(shared, pr) {
        Ok(review) => Outcome::Ok(Reply::Revalidation(current(shared, &review))),
        Err(out) => out,
    }
}

fn refused(error: DecideError) -> Outcome {
    let code = match error {
        DecideError::Changed => ErrorCode::Conflict,
        DecideError::UnknownItem | DecideError::NoRound => ErrorCode::NotFound,
        DecideError::AlreadyDecided
        | DecideError::CannotPutBack(_)
        | DecideError::NothingToDecide => ErrorCode::InvalidState,
    };
    Outcome::Err(ProtocolError::new(code, error.to_string()))
}

/// The pull request's files at the head of the review; none when they cannot be read, which
/// makes every move back refused rather than guessed.
async fn files_of(shared: &Shared, review: &Review) -> Vec<FileDiff> {
    files_at(shared, &review.pr, &review.head_sha)
        .await
        .map(|files| files.to_vec())
        .unwrap_or_default()
}

/// Saves what a decision changed and tells every window.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
fn store(shared: &Shared, review: &mut Review) -> Result<(), Outcome> {
    review.updated_at = now_unix();
    save(shared, review)?;
    announce(shared, review);
    tell(shared, review);
    Ok(())
}

fn stored(shared: &Shared, mut review: Review) -> Outcome {
    match store(shared, &mut review) {
        Ok(()) => Outcome::Ok(Reply::ReviewFile(Box::new(review))),
        Err(out) => out,
    }
}

/// `DecideProposal`: accepts or rejects the proposal for `item`. A comment that changed since the
/// suggestion is refused with `Conflict`.
pub(crate) async fn decide_proposal(
    shared: &Shared,
    pr: &PrRef,
    item: &str,
    accept: bool,
) -> Outcome {
    let _guard = lock(shared, pr).await;
    let mut review = match load_existing(shared, pr) {
        Ok(review) => review,
        Err(out) => return out,
    };
    if !editable(&review) {
        return reviews::invalid_state(format!(
            "the review of {pr} is {:?}; open it again",
            review.state
        ));
    }
    let files = files_of(shared, &review).await;
    if let Err(error) = decide(&mut review, item, accept, &files) {
        return refused(error);
    }
    stored(shared, review)
}

/// `AcceptAllProposals`: accepts every undecided proposal that can be accepted and still fits
/// its comment; the others stay for the user.
pub(crate) async fn accept_all_proposals(shared: &Shared, pr: &PrRef) -> Outcome {
    let _guard = lock(shared, pr).await;
    let mut review = match load_existing(shared, pr) {
        Ok(review) => review,
        Err(out) => return out,
    };
    if !editable(&review) {
        return reviews::invalid_state(format!(
            "the review of {pr} is {:?}; open it again",
            review.state
        ));
    }
    if review.revalidation.is_none() {
        return refused(DecideError::NoRound);
    }
    let files = files_of(shared, &review).await;
    if accept_all(&mut review, &files) == 0 {
        return Outcome::Ok(Reply::ReviewFile(Box::new(review)));
    }
    stored(shared, review)
}

/// `ReanchorDraftItem`: points a line comment at `anchor`, or makes it a general comment with
/// `None`. The anchor must be a commentable line of the pull request, as for a new comment.
pub(crate) async fn reanchor(
    shared: &Shared,
    pr: &PrRef,
    id: &str,
    anchor: Option<AnchorInput>,
) -> Outcome {
    let _guard = lock(shared, pr).await;
    let mut review = match load_existing(shared, pr) {
        Ok(review) => review,
        Err(out) => return out,
    };
    if !editable(&review) {
        return reviews::invalid_state(format!(
            "the review of {pr} is {:?}; open it again",
            review.state
        ));
    }
    let anchor = match anchor {
        Some(input) => match reviews::resolve_anchor(shared, &review, input).await {
            Ok(anchor) => Some(anchor),
            Err(out) => return out,
        },
        None => None,
    };
    if let Err(e) = review.draft.reanchor(id, anchor) {
        return bad_request(e.to_string());
    }
    // An anchored comment is back in the draft: it is no longer a removal.
    review.removed.remove(id);
    if let Err(out) = store(shared, &mut review) {
        return out;
    }
    match review.draft.get(id) {
        Some(item) => Outcome::Ok(Reply::DraftItem(item.clone())),
        None => bad_request(format!("there is no draft item {id}")),
    }
}
