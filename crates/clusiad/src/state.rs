//! State shared by every connection and the sync loop.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::{Duration, Instant, SystemTime};

use clusia_core::config::AuthSource;
use clusia_core::{Config, FileDiff, Paths, PrRef, PrSummary};
use clusia_git::{LocalClone, RepoId};
use clusia_platform::SecretStore;
use clusia_protocol::{Event, PermissionStatus, SyncStatus};
use clusia_provider::{GitHub, Token, TokenOrigin};
use tokio::sync::{Mutex, Notify, RwLock, broadcast, watch};

use crate::calls::Indexes;
use crate::checks::Checks;
use crate::diagrams::Diagrams;
use crate::holds::Holds;
use crate::inbox::InboxData;
use crate::news::Backoff;
use crate::notifications::Engine;
use crate::options::DaemonOptions;
use crate::permissions::Permissions;
use crate::revalidate::Revalidations;
use crate::review_cache::ReviewCaches;
use crate::sessions::Sessions;
use crate::spawner::Spawner;
use crate::story::Stories;

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct PrLists {
    pub assigned: Vec<PrSummary>,
    pub mine: Vec<PrSummary>,
}

/// Head SHA and the parsed files fetched for it.
pub(crate) type CachedFiles = (String, Arc<Vec<FileDiff>>);

/// How long a token from `gh auth token` is used before the CLI is asked again.
const TOKEN_TTL: Duration = Duration::from_secs(300);

/// What `gh` keeps its accounts in, as `stat` sees it: the modification time and size of
/// `hosts.yml`, or `None` when there is no such file. `gh auth login`, `logout` and `switch`
/// all rewrite it.
pub(crate) type GhStamp = Option<(Option<SystemTime>, u64)>;

/// The last token `gh auth token` gave for a sign-in method and host, as long as `hosts.yml`
/// is as it was. Tokens from the environment and the Keychain are not kept: they are cheap to
/// read, and a stored one changes through the daemon, which forgets this entry then.
#[derive(Default)]
pub(crate) struct TokenCache(Option<(AuthSource, String, GhStamp, Token, Instant)>);

impl TokenCache {
    /// The kept token when it is for `auth` and `host`, `hosts.yml` still reads `stamp`, and it
    /// is less than `TOKEN_TTL` old at `now`.
    pub fn get(&self, auth: AuthSource, host: &str, stamp: GhStamp, now: Instant) -> Option<Token> {
        let (kept_auth, kept_host, kept_stamp, token, at) = self.0.as_ref()?;
        (*kept_auth == auth
            && kept_host == host
            && *kept_stamp == stamp
            && now.saturating_duration_since(*at) < TOKEN_TTL)
            .then(|| token.clone())
    }

    /// Keeps `token`, taken at `now` while `hosts.yml` read `stamp`, when it came from the CLI.
    pub fn put(
        &mut self,
        auth: AuthSource,
        host: &str,
        stamp: GhStamp,
        token: &Token,
        now: Instant,
    ) {
        if token.origin == TokenOrigin::GhCli {
            self.0 = Some((auth, host.to_string(), stamp, token.clone(), now));
        }
    }

    pub fn forget(&mut self) {
        self.0 = None;
    }
}

/// How long "no clone in the folders" is believed before the folders are walked again.
const NO_CLONE_TTL: Duration = Duration::from_secs(300);

/// Which clone in the user's folders holds each repository, so an open does not walk the
/// folders to find it again. A found clone is kept until its folder is gone or the folders to
/// look in change; a repository with no clone there is kept for `NO_CLONE_TTL`.
#[derive(Default)]
pub(crate) struct CloneMap {
    roots: Vec<PathBuf>,
    found: HashMap<RepoId, (Option<LocalClone>, Instant)>,
}

impl CloneMap {
    /// What an earlier look found for `target` in `roots`; `None` when there is nothing to trust.
    pub fn get(
        &mut self,
        roots: &[PathBuf],
        target: &RepoId,
        now: Instant,
    ) -> Option<Option<LocalClone>> {
        if self.roots != roots {
            self.roots = roots.to_vec();
            self.found.clear();
            return None;
        }
        let (clone, at) = self.found.get(target)?;
        let current = match clone {
            Some(local) => local.path.join(".git").exists(),
            None => now.saturating_duration_since(*at) < NO_CLONE_TTL,
        };
        if !current {
            self.found.remove(target);
            return None;
        }
        Some(clone.clone())
    }

    /// Remembers what a look at `roots` found for `target`.
    pub fn put(
        &mut self,
        roots: &[PathBuf],
        target: &RepoId,
        clone: Option<LocalClone>,
        now: Instant,
    ) {
        if self.roots != roots {
            self.roots = roots.to_vec();
            self.found.clear();
        }
        self.found.insert(target.clone(), (clone, now));
    }

    /// Drops what is known about `target`, for a clone that turned out not to work.
    pub fn forget(&mut self, target: &RepoId) {
        self.found.remove(target);
    }
}

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
    /// The `codex` program of `DaemonOptions` (see `turns::program_path`).
    pub codex_program: Option<PathBuf>,
    /// The folder of `gh`'s `hosts.yml` (see `DaemonOptions::gh_config_dir`).
    pub gh_config_dir: Option<PathBuf>,
    /// The `clusiad` that runs as the permission bridge; this executable when `None`.
    pub bridge_program: Option<PathBuf>,
    /// Added to the daemon's environment for a custom command's process; empty in production.
    pub harness_env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    /// Starts docker, sandbox-exec and the recipe's shell for the tests (see
    /// `DaemonOptions::test_exec`).
    pub test_exec: Arc<dyn clusia_testrun::exec::Exec>,
    /// The `docker` program of `DaemonOptions`; `None` finds it as `engine::detect` does.
    pub docker_program: Option<PathBuf>,
    /// The `sandbox-exec` of the local runner (see `DaemonOptions::sandbox_exec_program`).
    pub sandbox_exec_program: PathBuf,
    /// The test environments, setups and runs of every review.
    pub tests: crate::tests::Bench,
    /// The permission requests waiting for the reviewer.
    pub permissions: Permissions,
    /// The running agent turns of every review.
    pub sessions: Sessions,
    /// The running security and audit checks of every review. Not `checks_wake`, which wakes
    /// the task that looks at the continuous-integration checks of your pull requests.
    pub checks: Checks,
    /// The running diagram drawings of every review.
    pub diagrams: Diagrams,
    /// The call graph indexes built for the reviews in use, and their builds.
    pub indexes: Indexes,
    /// The running story turns of every review.
    pub stories: Stories,
    /// The `codex app-server` processes, one per open review.
    pub codex: crate::codex::Servers,
    /// The timers and runs that revalidate the drafts of saved reviews.
    pub revalidations: Revalidations,
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
    /// The token `gh auth token` gave, so a request does not start a process to learn it again.
    pub token_cache: std::sync::Mutex<TokenCache>,
    /// Where the user's clones of repositories were found.
    pub clone_map: std::sync::Mutex<CloneMap>,
    pub worktree_lock: Mutex<()>,
    /// Serialises the read-modify-write of the window's open tabs.
    pub tabs_lock: Mutex<()>,
    /// Serialises the read-modify-write of the hidden comments.
    pub hidden_lock: Mutex<()>,
    /// File keys of pull requests checked out since the daemon started; retention leaves them alone.
    pub touched: std::sync::Mutex<HashSet<String>>,
    /// One mutex per PR serialises review mutations.
    pub review_locks: std::sync::Mutex<HashMap<PrRef, Arc<tokio::sync::Mutex<()>>>>,
    /// The parsed review caches and the writes behind them.
    pub review_caches: Arc<ReviewCaches>,
    /// Parsed files per PR, keyed by the head SHA they were fetched for.
    pub files_cache: Mutex<HashMap<PrRef, CachedFiles>>,
    /// What the last open of each PR read from GitHub, so the news right after it asks for less.
    pub fresh_reads: std::sync::Mutex<HashMap<PrRef, crate::news::FreshRead>>,
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
    /// Makes the next request ask for the token again.
    pub fn forget_token(&self) {
        self.token_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .forget();
    }

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
            review_caches: Arc::new(ReviewCaches::new(paths.clone())),
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
            codex_program: options.codex_program,
            gh_config_dir: options.gh_config_dir,
            bridge_program: options.bridge_program,
            harness_env: options.harness_env,
            test_exec: options.test_exec,
            docker_program: options.docker_program,
            sandbox_exec_program: options.sandbox_exec_program,
            tests: crate::tests::Bench::default(),
            permissions: Permissions::default(),
            sessions: Sessions::default(),
            checks: Checks::default(),
            diagrams: Diagrams::default(),
            indexes: Indexes::default(),
            stories: Stories::default(),
            codex: crate::codex::Servers::default(),
            revalidations: Revalidations::default(),
            holds: Holds::default(),
            window_listeners: AtomicUsize::new(0),
            tray_listeners: AtomicUsize::new(0),
            prs: RwLock::new(PrLists::default()),
            sync: RwLock::new(SyncStatus::default()),
            sync_now: Notify::new(),
            paused: AtomicBool::new(false),
            client: Mutex::new(None),
            token_cache: std::sync::Mutex::new(TokenCache::default()),
            clone_map: std::sync::Mutex::new(CloneMap::default()),
            worktree_lock: Mutex::new(()),
            tabs_lock: Mutex::new(()),
            hidden_lock: Mutex::new(()),
            touched: std::sync::Mutex::new(HashSet::new()),
            review_locks: std::sync::Mutex::new(HashMap::new()),
            files_cache: Mutex::new(HashMap::new()),
            fresh_reads: std::sync::Mutex::new(HashMap::new()),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn kept(origin: TokenOrigin, now: Instant) -> TokenCache {
        let mut cache = TokenCache::default();
        cache.put(
            AuthSource::GhCli,
            "github.com",
            None,
            &Token::new("secret", origin),
            now,
        );
        cache
    }

    #[test]
    fn a_cli_token_is_kept_until_it_is_five_minutes_old() {
        let t0 = Instant::now();
        let cache = kept(TokenOrigin::GhCli, t0);
        let at = |secs| t0 + Duration::from_secs(secs);
        assert!(
            cache
                .get(AuthSource::GhCli, "github.com", None, at(299))
                .is_some()
        );
        assert!(
            cache
                .get(AuthSource::GhCli, "github.com", None, at(300))
                .is_none()
        );
    }

    #[test]
    fn a_kept_token_is_for_its_sign_in_method_and_host_only() {
        let t0 = Instant::now();
        let cache = kept(TokenOrigin::GhCli, t0);
        assert!(cache.get(AuthSource::Pat, "github.com", None, t0).is_none());
        assert!(
            cache
                .get(AuthSource::GhCli, "ghe.example.com", None, t0)
                .is_none()
        );
    }

    #[test]
    fn tokens_that_are_cheap_to_read_are_not_kept() {
        let t0 = Instant::now();
        for origin in [TokenOrigin::Env, TokenOrigin::Pat] {
            assert!(
                kept(origin, t0)
                    .get(AuthSource::GhCli, "github.com", None, t0)
                    .is_none()
            );
        }
    }

    #[test]
    fn a_kept_token_is_only_good_while_hosts_yml_is_as_it_was() {
        let t0 = Instant::now();
        let cache = kept(TokenOrigin::GhCli, t0);
        let rewritten = Some((Some(SystemTime::UNIX_EPOCH), 40));
        assert!(
            cache
                .get(AuthSource::GhCli, "github.com", rewritten, t0)
                .is_none()
        );
    }

    #[test]
    fn forgetting_empties_it() {
        let t0 = Instant::now();
        let mut cache = kept(TokenOrigin::GhCli, t0);
        cache.forget();
        assert!(
            cache
                .get(AuthSource::GhCli, "github.com", None, t0)
                .is_none()
        );
    }

    fn widgets() -> RepoId {
        RepoId::new("github.com", "acme", "widgets")
    }

    fn roots() -> Vec<PathBuf> {
        vec![PathBuf::from("/repos")]
    }

    #[test]
    fn a_clone_is_kept_while_its_folder_is_there() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let local = LocalClone {
            path: dir.path().to_path_buf(),
            remote: "origin".into(),
        };
        let t0 = Instant::now();
        let mut map = CloneMap::default();
        map.put(&roots(), &widgets(), Some(local.clone()), t0);
        let much_later = t0 + Duration::from_secs(86_400);
        assert_eq!(map.get(&roots(), &widgets(), much_later), Some(Some(local)));
        std::fs::remove_dir(dir.path().join(".git")).unwrap();
        assert_eq!(map.get(&roots(), &widgets(), much_later), None);
    }

    #[test]
    fn no_clone_is_believed_for_five_minutes() {
        let t0 = Instant::now();
        let mut map = CloneMap::default();
        map.put(&roots(), &widgets(), None, t0);
        let at = |secs| t0 + Duration::from_secs(secs);
        assert_eq!(map.get(&roots(), &widgets(), at(299)), Some(None));
        assert_eq!(map.get(&roots(), &widgets(), at(300)), None);
    }

    #[test]
    fn other_folders_to_look_in_start_over() {
        let t0 = Instant::now();
        let mut map = CloneMap::default();
        map.put(&roots(), &widgets(), None, t0);
        assert_eq!(
            map.get(&[PathBuf::from("/elsewhere")], &widgets(), t0),
            None
        );
        assert_eq!(
            map.get(&roots(), &widgets(), t0),
            None,
            "the old answer is gone"
        );
    }

    #[test]
    fn forgetting_a_repository_leaves_the_others() {
        let t0 = Instant::now();
        let other = RepoId::new("github.com", "acme", "gadgets");
        let mut map = CloneMap::default();
        map.put(&roots(), &widgets(), None, t0);
        map.put(&roots(), &other, None, t0);
        map.forget(&widgets());
        assert_eq!(map.get(&roots(), &widgets(), t0), None);
        assert_eq!(map.get(&roots(), &other, t0), Some(None));
    }
}
