//! The window's review tabs (`tabs.json`), so they come back when it opens again. The window
//! decides the set; the daemon writes the file and keeps when each tab was last shown.

use std::collections::HashSet;

use clusia_core::{ActivityKind, OpenTabs, PrRef, ReviewState, TAB_IDLE_DAYS};
use clusia_protocol::{ErrorCode, Outcome, ProtocolError, Reply};
use clusia_store::{
    ReviewLoad, list_reviews, load_open_tabs, load_review, read_activity, save_open_tabs,
};

use crate::reviews;
use crate::state::Shared;
use crate::sync::now_unix;

#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
fn load(shared: &Shared) -> Result<OpenTabs, Outcome> {
    load_open_tabs(&shared.paths).map_err(|e| {
        Outcome::Err(ProtocolError::new(
            ErrorCode::Internal,
            format!("cannot read the open tabs: {e}"),
        ))
    })
}

#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
fn save(shared: &Shared, tabs: &OpenTabs) -> Result<(), Outcome> {
    save_open_tabs(&shared.paths, tabs).map_err(|e| {
        Outcome::Err(ProtocolError::new(
            ErrorCode::Internal,
            format!("cannot save the open tabs: {e}"),
        ))
    })
}

pub(crate) async fn set(shared: &Shared, tabs: OpenTabs) -> Outcome {
    let _serialized = shared.tabs_lock.lock().await;
    let stored = match load(shared) {
        Ok(stored) => stored,
        Err(out) => return out,
    };
    match save(shared, &stored.replaced_by(tabs, now_unix())) {
        Ok(()) => Outcome::Ok(Reply::Ack),
        Err(out) => out,
    }
}

/// The saved tabs, once the window's start has tidied up:
/// - an idle tab (not shown for `TAB_IDLE_DAYS`, nothing in its draft, no client holding the
///   review) closes as `CloseReview` would;
/// - a tab whose review was discarded or published after it was last seen is dropped;
/// - an Active review with an empty draft that no tab shows, no client holds and nobody
///   touched for `TAB_IDLE_DAYS` closes too, so a window that crashed or never saved its tabs
///   leaves nothing open for good.
pub(crate) async fn restore(shared: &Shared, client: &str) -> Outcome {
    let now = now_unix();
    let saved = {
        let _serialized = shared.tabs_lock.lock().await;
        match load(shared) {
            Ok(tabs) => tabs,
            Err(out) => return out,
        }
    };
    // Out of the tabs lock: closing takes the review's lock and may wait for a running turn.
    let mut dropped = Vec::new();
    for pr in saved.idle(now) {
        if close_idle(shared, client, &pr).await {
            dropped.push(pr);
        }
    }
    dropped.extend(ended_since_seen(shared, &saved, &dropped));
    let tabbed: HashSet<&PrRef> = saved.tabs.iter().map(|t| &t.pr).collect();
    for pr in untabbed_idle(shared, &tabbed, now) {
        close_idle(shared, client, &pr).await;
    }
    let _serialized = shared.tabs_lock.lock().await;
    // Read again: the window may have sent its tabs while the idle ones closed.
    let tabs = match load(shared) {
        Ok(tabs) => tabs.without(&dropped),
        Err(out) => return out,
    };
    // The tabs were read fine: hand them back even if the tidied set could not be saved. An
    // error here would make the window start from no tabs, and its first save would replace
    // the saved set with that.
    if !dropped.is_empty()
        && let Err(Outcome::Err(e)) = save(shared, &tabs)
    {
        tracing::warn!(error = %e.message, "the file keeps the old set of open tabs");
    }
    Outcome::Ok(Reply::OpenTabs(tabs))
}

/// Closes `pr` like `CloseReview` unless a client holds it or its draft has items; `true` when
/// it is closed.
async fn close_idle(shared: &Shared, client: &str, pr: &PrRef) -> bool {
    if shared.holds.is_held(pr) {
        return false;
    }
    match reviews::close_if_empty(shared, client, pr).await {
        Ok(closed) => closed,
        Err(_) => {
            tracing::warn!(pr = %pr, "cannot close an idle review; it stays");
            false
        }
    }
}

/// The saved tabs (not already `dropped`) whose review file is gone because it was discarded
/// or published after the tab was last seen. A tab whose open never finished has no file
/// either, and stays.
fn ended_since_seen(shared: &Shared, saved: &OpenTabs, dropped: &[PrRef]) -> Vec<PrRef> {
    let gone: Vec<_> = saved
        .tabs
        .iter()
        .filter(|t| !dropped.contains(&t.pr))
        .filter(|t| matches!(load_review(&shared.paths, &t.pr), Ok(ReviewLoad::Missing)))
        .collect();
    if gone.is_empty() {
        return Vec::new();
    }
    let activity = match read_activity(&shared.paths) {
        Ok((activity, _)) => activity,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the activity log; every tab stays");
            return Vec::new();
        }
    };
    gone.into_iter()
        .filter(|tab| {
            activity.iter().any(|a| {
                a.pr == tab.pr
                    && a.ts >= tab.seen_at
                    && matches!(
                        a.kind,
                        ActivityKind::ReviewDiscarded | ActivityKind::ReviewPublished
                    )
            })
        })
        .map(|tab| tab.pr.clone())
        .collect()
}

/// Active reviews with an empty draft that no tab shows and nobody touched for
/// `TAB_IDLE_DAYS`. The draft is checked again under the review lock when each one closes.
fn untabbed_idle(shared: &Shared, tabbed: &HashSet<&PrRef>, now: i64) -> Vec<PrRef> {
    let cutoff = now - TAB_IDLE_DAYS * 86_400;
    match list_reviews(&shared.paths) {
        Ok(reviews) => reviews
            .into_iter()
            .filter(|r| {
                r.state == ReviewState::Active
                    && r.draft.is_empty()
                    && r.updated_at < cutoff
                    && !tabbed.contains(&r.pr)
            })
            .map(|r| r.pr)
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "cannot list the reviews; none is tidied");
            Vec::new()
        }
    }
}
