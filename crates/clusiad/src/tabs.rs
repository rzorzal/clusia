//! The window's review tabs (`tabs.json`), so they come back when it opens again. Only the
//! window writes them; the daemon keeps when each tab was last shown.

use clusia_core::OpenTabs;
use clusia_protocol::{ErrorCode, Outcome, ProtocolError, Reply};
use clusia_store::{load_open_tabs, save_open_tabs};

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

/// The saved tabs, once the idle ones are closed: not shown for `TAB_IDLE_DAYS`, nothing in
/// the draft, and no client holding the review. They close as `CloseReview` would.
pub(crate) async fn restore(shared: &Shared, client: &str) -> Outcome {
    let idle = {
        let _serialized = shared.tabs_lock.lock().await;
        match load(shared) {
            Ok(tabs) => tabs.idle(now_unix()),
            Err(out) => return out,
        }
    };
    // Out of the tabs lock: closing takes the review's lock and may wait for a running turn.
    let mut closed = Vec::new();
    for pr in idle {
        if shared.holds.is_held(&pr) {
            continue;
        }
        match reviews::close_if_empty(shared, client, &pr).await {
            Ok(true) => closed.push(pr),
            Ok(false) => {}
            Err(_) => tracing::warn!(pr = %pr, "cannot close an idle tab's review; it stays"),
        }
    }
    let _serialized = shared.tabs_lock.lock().await;
    // Read again: the window may have sent its tabs while the idle ones closed.
    let tabs = match load(shared) {
        Ok(tabs) => tabs.without(&closed),
        Err(out) => return out,
    };
    if !closed.is_empty()
        && let Err(out) = save(shared, &tabs)
    {
        return out;
    }
    Outcome::Ok(Reply::OpenTabs(tabs))
}
