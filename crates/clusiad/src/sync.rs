//! Keeping the PR lists in sync with GitHub (spec §5.1).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clusia_core::PrFilter;
use clusia_protocol::{Event, SyncState, SyncStatus, topics};
use clusia_provider::{GitHub, ProviderError, TokenSources, api_base_for_host, resolve_token};

use crate::state::{PrLists, Shared};

pub(crate) const NO_TOKEN: &str =
    "no GitHub token: run `gh auth login`, or pipe a token into `clusia auth login`";
const OFFLINE_RETRY_SECS: u64 = 30;
const IDLE_RETRY_SECS: u64 = 300;
/// How long `ListPrs` waits for the first sync before answering from the (empty) cache.
const FIRST_SYNC_WAIT: Duration = Duration::from_secs(10);

pub(crate) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The client for the configured host and current token. `None` means there is no token.
pub(crate) async fn github_client(shared: &Shared) -> Result<Option<Arc<GitHub>>, ProviderError> {
    let github = shared.config.read().await.github.clone();
    let sources = TokenSources {
        env: shared.github_token.as_deref(),
        gh_program: &shared.gh_program,
        secrets: shared.secrets.as_ref(),
    };
    let token = match resolve_token(github.auth, &github.host, &sources).await {
        Ok(Some(token)) => token,
        Ok(None) => return Ok(None),
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the stored token");
            return Ok(None);
        }
    };
    let api = shared
        .github_api
        .clone()
        .unwrap_or_else(|| api_base_for_host(&github.host));
    let mut cached = shared.client.lock().await;
    if let Some(client) = cached.as_ref()
        && client.api() == api
        && client.token() == &token
    {
        return Ok(Some(client.clone()));
    }
    let client = Arc::new(GitHub::new(&api, token)?);
    *cached = Some(client.clone());
    Ok(Some(client))
}

pub(crate) fn status_for_error(
    e: &ProviderError,
    last: Option<i64>,
    now: i64,
    poll: u64,
) -> SyncStatus {
    let (state, next) = match e {
        ProviderError::Unauthorized => (SyncState::Unauthorized, None),
        ProviderError::RateLimited { retry_after_secs } => {
            (SyncState::RateLimited, Some(now + *retry_after_secs as i64))
        }
        _ => (
            SyncState::Offline,
            Some(now + poll.min(OFFLINE_RETRY_SECS) as i64),
        ),
    };
    SyncStatus {
        state,
        last_sync_unix: last,
        next_sync_unix: next,
        message: Some(e.to_string()),
    }
}

async fn fetch_lists(gh: &GitHub) -> Result<PrLists, ProviderError> {
    Ok(PrLists {
        assigned: gh.list_prs(PrFilter::Assigned).await?,
        mine: gh.list_prs(PrFilter::Mine).await?,
    })
}

async fn store_lists(shared: &Shared, lists: PrLists) {
    let mut current = shared.prs.write().await;
    if *current == lists {
        return;
    }
    let event = Event::PrsUpdated {
        assigned: lists.assigned.len(),
        mine: lists.mine.len(),
    };
    *current = lists;
    shared.publish(topics::PRS, event);
}

async fn set_status(shared: &Shared, status: SyncStatus) {
    let mut current = shared.sync.write().await;
    let changed = current.state != status.state || current.message != status.message;
    *current = status.clone();
    if changed {
        shared.publish(topics::SYNC, Event::SyncChanged(status));
    }
}

/// One sync with GitHub. The cached lists are only replaced on success.
/// Syncs never run concurrently.
pub(crate) async fn sync_once(shared: &Shared) -> SyncStatus {
    let _guard = shared.sync_lock.lock().await;
    let status = sync_locked(shared).await;
    shared.first_sync_done.send_replace(true);
    status
}

/// Returns once the first sync has finished, or after `FIRST_SYNC_WAIT`. Without the
/// background loop, the first caller runs that sync itself.
pub(crate) async fn wait_for_first_sync(shared: &Shared) {
    if *shared.first_sync_done.borrow() {
        return;
    }
    let wait = async {
        if shared.background_sync {
            let mut done = shared.first_sync_done.subscribe();
            let _ = done.wait_for(|done| *done).await;
        } else {
            let _guard = shared.sync_lock.lock().await;
            if !*shared.first_sync_done.borrow() {
                sync_locked(shared).await;
                shared.first_sync_done.send_replace(true);
            }
        }
    };
    if tokio::time::timeout(FIRST_SYNC_WAIT, wait).await.is_err() {
        tracing::warn!("first sync still running; answering from the empty cache");
    }
}

async fn sync_locked(shared: &Shared) -> SyncStatus {
    let poll = shared.config.read().await.github.poll_interval_secs;
    let now = now_unix();
    let last = shared.sync.read().await.last_sync_unix;
    let result = match github_client(shared).await {
        Ok(Some(gh)) => fetch_lists(&gh).await.map(Some),
        Ok(None) => Ok(None),
        Err(e) => Err(e),
    };
    let status = match result {
        Ok(Some(lists)) => {
            store_lists(shared, lists).await;
            SyncStatus {
                state: SyncState::Online,
                last_sync_unix: Some(now),
                next_sync_unix: Some(now + poll as i64),
                message: None,
            }
        }
        Ok(None) => SyncStatus {
            state: SyncState::Unauthorized,
            last_sync_unix: last,
            next_sync_unix: None,
            message: Some(NO_TOKEN.to_string()),
        },
        Err(e) => status_for_error(&e, last, now, poll),
    };
    set_status(shared, status.clone()).await;
    status
}

pub(crate) async fn run_loop(shared: Arc<Shared>) {
    let mut shutdown = shared.shutdown.subscribe();
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        let status = sync_once(&shared).await;
        crate::news::check_saved_reviews(&shared).await;
        let wait = status
            .next_sync_unix
            .map_or(IDLE_RETRY_SECS, |at| (at - now_unix()).max(1) as u64);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
            _ = shared.sync_now.notified() => {}
            changed = shutdown.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_for_error_maps_states_and_next_sync() {
        let s = status_for_error(&ProviderError::Unauthorized, Some(5), 100, 60);
        assert_eq!(
            (s.state, s.next_sync_unix, s.last_sync_unix),
            (SyncState::Unauthorized, None, Some(5))
        );
        let s = status_for_error(
            &ProviderError::RateLimited {
                retry_after_secs: 90,
            },
            None,
            100,
            60,
        );
        assert_eq!(
            (s.state, s.next_sync_unix),
            (SyncState::RateLimited, Some(190))
        );
        let s = status_for_error(&ProviderError::Offline("down".into()), None, 100, 600);
        assert_eq!((s.state, s.next_sync_unix), (SyncState::Offline, Some(130)));
        let s = status_for_error(
            &ProviderError::Http {
                status: 502,
                message: "bad gateway".into(),
            },
            None,
            100,
            20,
        );
        assert_eq!((s.state, s.next_sync_unix), (SyncState::Offline, Some(120)));
        assert!(s.message.unwrap().contains("502"));
    }
}
