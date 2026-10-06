//! State shared by every connection and the sync loop.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Instant;

use clusia_core::{Config, FileDiff, Paths, PrRef, PrSummary};
use clusia_platform::SecretStore;
use clusia_protocol::{Event, SyncStatus};
use clusia_provider::GitHub;
use tokio::sync::{Mutex, Notify, RwLock, broadcast, watch};

use crate::options::DaemonOptions;
use crate::spawner::Spawner;

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct PrLists {
    pub assigned: Vec<PrSummary>,
    pub mine: Vec<PrSummary>,
}

/// Head SHA and the parsed files fetched for it.
pub(crate) type CachedFiles = (String, Arc<Vec<FileDiff>>);

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
    pub tray_program: Option<PathBuf>,
    pub spawner: Arc<dyn Spawner>,
    pub media_extra_hosts: Vec<String>,
    pub media_allow_local: bool,
    pub giphy_api: String,
    pub harness_search_paths: Vec<PathBuf>,
    /// Connections subscribed to the `window` topic (open windows).
    pub window_listeners: AtomicUsize,
    pub prs: RwLock<PrLists>,
    pub sync: RwLock<SyncStatus>,
    /// Wakes the sync loop early (e.g. after a new token is stored).
    pub sync_now: Notify,
    /// Background syncing is paused (`PauseSync`). In memory only.
    pub paused: AtomicBool,
    /// Reused while host and token are unchanged, so ETag caching survives between syncs.
    pub client: Mutex<Option<Arc<GitHub>>>,
    pub worktree_lock: Mutex<()>,
    /// File keys of pull requests checked out since the daemon started; retention leaves them alone.
    pub touched: std::sync::Mutex<HashSet<String>>,
    /// One mutex per PR serialises review mutations.
    pub review_locks: std::sync::Mutex<HashMap<PrRef, Arc<tokio::sync::Mutex<()>>>>,
    /// Parsed files per PR, keyed by the head SHA they were fetched for.
    pub files_cache: Mutex<HashMap<PrRef, CachedFiles>>,
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
            tray_program: options.tray_program,
            spawner: options.spawner,
            media_extra_hosts: options.media_extra_hosts,
            media_allow_local: options.media_allow_local,
            giphy_api: options
                .giphy_api
                .unwrap_or_else(|| "https://api.giphy.com".to_string()),
            harness_search_paths: options.harness_search_paths,
            window_listeners: AtomicUsize::new(0),
            prs: RwLock::new(PrLists::default()),
            sync: RwLock::new(SyncStatus::default()),
            sync_now: Notify::new(),
            paused: AtomicBool::new(false),
            client: Mutex::new(None),
            worktree_lock: Mutex::new(()),
            touched: std::sync::Mutex::new(HashSet::new()),
            review_locks: std::sync::Mutex::new(HashMap::new()),
            files_cache: Mutex::new(HashMap::new()),
            sync_lock: Mutex::new(()),
            first_sync_done,
        }
    }

    /// Marks `pr`'s worktree as in use this session. Call before checking it out.
    pub fn touch(&self, pr: &PrRef) {
        self.touched
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(pr.file_key());
    }

    pub fn is_touched(&self, key: &str) -> bool {
        self.touched
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(key)
    }

    pub fn trigger_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    /// Sends `event` to every connection subscribed to `topic`. No listeners is fine.
    pub fn publish(&self, topic: &str, event: Event) {
        let _ = self.events.send((topic.to_string(), event));
    }
}
