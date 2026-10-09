//! State shared by every connection and the sync loop.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::{Duration, Instant};

use clusia_core::{Config, FileDiff, Paths, PrRef, PrSummary};
use clusia_platform::SecretStore;
use clusia_protocol::{Event, PermissionStatus, SyncStatus};
use clusia_provider::GitHub;
use tokio::sync::{Mutex, Notify, RwLock, broadcast, watch};

use crate::holds::Holds;
use crate::inbox::InboxData;
use crate::news::Backoff;
use crate::notifications::Engine;
use crate::options::DaemonOptions;
use crate::permissions::Permissions;
use crate::sessions::Sessions;
use crate::spawner::Spawner;

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct PrLists {
    pub assigned: Vec<PrSummary>,
    pub mine: Vec<PrSummary>,
}

/// Head SHA and the parsed files fetched for it.
pub(crate) type CachedFiles = (String, Arc<Vec<FileDiff>>);

/// Counts one piece of work in `Shared::busy` while it lives, so a shutdown can wait for it.
pub(crate) struct Busy(Arc<Shared>);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.busy.send_modify(|n| *n -= 1);
    }
}

pub(crate) struct Shared {
    pub paths: Paths,
    pub config: RwLock<Config>,
    pub started: Instant,
    pub clients: AtomicUsize,
    pub events: broadcast::Sender<(String, Event)>,
    pub shutdown: watch::Sender<bool>,
    /// How many requests are being handled (a handler plus the write of its response).
    pub busy: watch::Sender<usize>,
    pub github_api: Option<String>,
    pub github_token: Option<String>,
    pub gh_program: PathBuf,
    pub secrets: Arc<dyn SecretStore>,
    pub background_sync: bool,
    pub tray_program: Option<PathBuf>,
    pub spawner: Arc<dyn Spawner>,
    pub media_extra_hosts: Vec<String>,
    pub media_allow_local: bool,
    /// The client media fetches use (see `DaemonOptions::media_resolve`).
    pub media_http: reqwest::Client,
    pub giphy_api: String,
    pub harness_search_paths: Vec<PathBuf>,
    /// The `claude` program of `DaemonOptions` (see `sessions::program_path`).
    pub claude_program: Option<PathBuf>,
    /// The `clusiad` that runs as the permission bridge; this executable when `None`.
    pub bridge_program: Option<PathBuf>,
    /// The permission requests waiting for the reviewer.
    pub permissions: Permissions,
    /// The running agent turns of every review.
    pub sessions: Sessions,
    /// Which reviews a connected client has open, so the daemon knows when no window shows one.
    pub holds: Holds,
    /// Connections subscribed to the `window` topic (open windows).
    pub window_listeners: AtomicUsize,
    /// Connections subscribed to the `tray` topic (running trays).
    pub tray_listeners: AtomicUsize,
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
    /// When each saved review whose worktree would not update may be tried again.
    pub checkout_backoff: std::sync::Mutex<HashMap<PrRef, Backoff>>,
    /// Held for the whole of a sync, so the loop, `SyncNow` and `ListPrs` never overlap.
    pub sync_lock: Mutex<()>,
    /// Becomes `true` once the first sync has finished (whatever its outcome).
    pub first_sync_done: watch::Sender<bool>,
    /// The persisted inbox and the routing memory of the notification rules.
    pub engine: Mutex<Engine>,
    /// State files set aside since the last sync, as (the file, the name it was moved to).
    pub recovered: std::sync::Mutex<Vec<(String, String)>>,
    /// What the tray last reported about macOS notification permission. In memory only.
    pub permission: std::sync::Mutex<PermissionStatus>,
    /// Wakes the task that looks at the checks of your pull requests.
    pub checks_wake: Notify,
}

impl Shared {
    pub fn new(paths: Paths, config: Config, options: DaemonOptions) -> Self {
        let (events, _) = broadcast::channel(256);
        let (shutdown, _) = watch::channel(false);
        let (busy, _) = watch::channel(0);
        let (first_sync_done, _) = watch::channel(false);
        let inbox = InboxData::load(&paths);
        let recovered = inbox
            .quarantined
            .iter()
            .filter_map(|file| file.file_name())
            .map(|name| {
                (
                    "inbox.json".to_string(),
                    name.to_string_lossy().into_owned(),
                )
            })
            .collect();
        Self {
            paths,
            config: RwLock::new(config),
            started: Instant::now(),
            clients: AtomicUsize::new(0),
            events,
            shutdown,
            busy,
            github_api: options.github_api,
            github_token: options.github_token,
            gh_program: options.gh_program,
            secrets: options.secrets,
            background_sync: options.background_sync,
            tray_program: options.tray_program,
            spawner: options.spawner,
            media_extra_hosts: options.media_extra_hosts,
            media_allow_local: options.media_allow_local,
            media_http: crate::media::client_resolving(&options.media_resolve),
            giphy_api: options
                .giphy_api
                .unwrap_or_else(|| "https://api.giphy.com".to_string()),
            harness_search_paths: options.harness_search_paths,
            claude_program: options.claude_program,
            bridge_program: options.bridge_program,
            permissions: Permissions::default(),
            sessions: Sessions::default(),
            holds: Holds::default(),
            window_listeners: AtomicUsize::new(0),
            tray_listeners: AtomicUsize::new(0),
            prs: RwLock::new(PrLists::default()),
            sync: RwLock::new(SyncStatus::default()),
            sync_now: Notify::new(),
            paused: AtomicBool::new(false),
            client: Mutex::new(None),
            worktree_lock: Mutex::new(()),
            touched: std::sync::Mutex::new(HashSet::new()),
            review_locks: std::sync::Mutex::new(HashMap::new()),
            files_cache: Mutex::new(HashMap::new()),
            checkout_backoff: std::sync::Mutex::new(HashMap::new()),
            sync_lock: Mutex::new(()),
            first_sync_done,
            engine: Mutex::new(Engine::new(inbox.data)),
            recovered: std::sync::Mutex::new(recovered),
            permission: std::sync::Mutex::new(PermissionStatus::default()),
            checks_wake: Notify::new(),
        }
    }

    /// Seconds since the daemon started; the clock `checkout_backoff` runs on.
    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// Whether the saved review of `pr` may try to update its worktree now.
    pub fn checkout_due(&self, pr: &PrRef) -> bool {
        let now = self.uptime_secs();
        self.checkout_backoff
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(pr)
            .is_none_or(|b| b.due(now))
    }

    pub fn checkout_failed(&self, pr: &PrRef) {
        let now = self.uptime_secs();
        self.checkout_backoff
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(pr.clone())
            .or_default()
            .failed(now);
    }

    pub fn checkout_worked(&self, pr: &PrRef) {
        self.checkout_backoff
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(pr);
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

    /// Marks a request as being handled until the returned guard is dropped.
    pub fn begin_work(self: &Arc<Self>) -> Busy {
        self.busy.send_modify(|n| *n += 1);
        Busy(self.clone())
    }

    /// Waits until no request is being handled; `false` when `limit` ran out first.
    pub async fn wait_idle(&self, limit: Duration) -> bool {
        let mut busy = self.busy.subscribe();
        tokio::time::timeout(limit, busy.wait_for(|n| *n == 0))
            .await
            .is_ok()
    }

    pub fn trigger_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    /// Sends `event` to every connection subscribed to `topic`. No listeners is fine.
    pub fn publish(&self, topic: &str, event: Event) {
        let _ = self.events.send((topic.to_string(), event));
    }
}
