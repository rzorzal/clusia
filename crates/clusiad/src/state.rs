//! State shared by every connection and the sync loop.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::time::Instant;

use clusia_core::{Config, Paths, PrSummary};
use clusia_platform::SecretStore;
use clusia_protocol::{Event, SyncStatus};
use clusia_provider::GitHub;
use tokio::sync::{Mutex, Notify, RwLock, broadcast, watch};

use crate::options::DaemonOptions;

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct PrLists {
    pub assigned: Vec<PrSummary>,
    pub mine: Vec<PrSummary>,
}

pub(crate) struct Shared {
    pub paths: Paths,
    pub config: RwLock<Config>,
    pub started: Instant,
    pub clients: AtomicUsize,
    pub events: broadcast::Sender<(String, Event)>,
    pub shutdown: watch::Sender<bool>,
    pub github_api: Option<String>,
    pub github_token: Option<String>,
    pub gh_program: PathBuf,
    pub secrets: Arc<dyn SecretStore>,
    pub background_sync: bool,
    pub prs: RwLock<PrLists>,
    pub sync: RwLock<SyncStatus>,
    /// Wakes the sync loop early (e.g. after a new token is stored).
    pub sync_now: Notify,
    /// Reused while host and token are unchanged, so ETag caching survives between syncs.
    pub client: Mutex<Option<Arc<GitHub>>>,
    pub worktree_lock: Mutex<()>,
    /// Held for the whole of a sync, so the loop, `SyncNow` and `ListPrs` never overlap.
    pub sync_lock: Mutex<()>,
    /// Becomes `true` once the first sync has finished (whatever its outcome).
    pub first_sync_done: watch::Sender<bool>,
}

impl Shared {
    pub fn new(paths: Paths, config: Config, options: DaemonOptions) -> Self {
        let (events, _) = broadcast::channel(256);
        let (shutdown, _) = watch::channel(false);
        let (first_sync_done, _) = watch::channel(false);
        Self {
            paths,
            config: RwLock::new(config),
            started: Instant::now(),
            clients: AtomicUsize::new(0),
            events,
            shutdown,
            github_api: options.github_api,
            github_token: options.github_token,
            gh_program: options.gh_program,
            secrets: options.secrets,
            background_sync: options.background_sync,
            prs: RwLock::new(PrLists::default()),
            sync: RwLock::new(SyncStatus::default()),
            sync_now: Notify::new(),
            client: Mutex::new(None),
            worktree_lock: Mutex::new(()),
            sync_lock: Mutex::new(()),
            first_sync_done,
        }
    }

    pub fn trigger_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    /// Sends `event` to every connection subscribed to `topic`. No listeners is fine.
    pub fn publish(&self, topic: &str, event: Event) {
        let _ = self.events.send((topic.to_string(), event));
    }
}
