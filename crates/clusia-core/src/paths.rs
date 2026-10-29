//! Where Clúsia keeps its files. Every binary resolves paths through here.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// macOS limits `sun_path` to 104 bytes including the trailing NUL.
pub const MAX_SOCKET_PATH: usize = 103;

/// The login item's file name, which is also its launchd label plus `.plist`.
pub const LAUNCH_AGENT_FILE: &str = "io.github.rzorzal.clusia.daemon.plist";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    root: PathBuf,
    logs: PathBuf,
    launch_agents: PathBuf,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PathsError {
    #[error("neither CLUSIA_HOME nor HOME is set")]
    NoHome,
}

impl Paths {
    /// Everything under `root`, logs in `root/logs`. Used by tests and `--home`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let logs = root.join("logs");
        let launch_agents = root.join("LaunchAgents");
        Self {
            root,
            logs,
            launch_agents,
        }
    }

    /// `CLUSIA_HOME` when set and non-empty, otherwise the macOS defaults.
    pub fn from_env() -> Result<Self, PathsError> {
        Self::resolve(std::env::var_os("CLUSIA_HOME"), std::env::var_os("HOME"))
    }

    /// The macOS defaults for this user, whatever `CLUSIA_HOME` says: where an install keeps
    /// its data.
    pub fn user_default() -> Result<Self, PathsError> {
        Self::resolve(None, std::env::var_os("HOME"))
    }

    fn resolve(clusia_home: Option<OsString>, home: Option<OsString>) -> Result<Self, PathsError> {
        if let Some(dir) = clusia_home.filter(|d| !d.is_empty()) {
            let dir = PathBuf::from(dir);
            return Ok(Self::new(std::path::absolute(&dir).unwrap_or(dir)));
        }
        let home = PathBuf::from(home.filter(|h| !h.is_empty()).ok_or(PathsError::NoHome)?);
        Ok(Self {
            root: home.join("Library/Application Support/Clusia"),
            logs: home.join("Library/Logs/Clusia"),
            launch_agents: home.join("Library/LaunchAgents"),
        })
    }

    pub fn repos_dir(&self) -> PathBuf {
        self.root.join("repos")
    }

    pub fn worktrees_dir(&self) -> PathBuf {
        self.root.join("worktrees")
    }

    /// Where the worktree for `pr` lives.
    pub fn worktree_for(&self, pr: &crate::PrRef) -> PathBuf {
        self.worktrees_dir().join(pr.file_key())
    }

    pub fn reviews_dir(&self) -> PathBuf {
        self.root.join("reviews")
    }

    pub fn review_file(&self, pr: &crate::PrRef) -> PathBuf {
        self.reviews_dir().join(format!("{}.json", pr.file_key()))
    }

    pub fn review_cache_dir(&self) -> PathBuf {
        self.root.join("cache/reviews")
    }

    /// What the last successful open fetched for `pr`, for "Open from cache".
    pub fn review_cache_file(&self, pr: &crate::PrRef) -> PathBuf {
        self.review_cache_dir()
            .join(format!("{}.json", pr.file_key()))
    }

    /// Images and GIFs downloaded for comments, named by `media::cache_key`. Only the daemon
    /// writes here.
    pub fn media_dir(&self) -> PathBuf {
        self.root.join("cache/media")
    }

    /// The review agent's logs and state, one pair of files per pull request.
    pub fn agent_dir(&self) -> PathBuf {
        self.root.join("agent")
    }

    /// Every event of the agent chat for `pr`, one JSON object per line.
    pub fn agent_log(&self, pr: &crate::PrRef) -> PathBuf {
        self.agent_dir().join(format!("{}.jsonl", pr.file_key()))
    }

    /// What the agent session remembers about `pr` between turns.
    pub fn agent_state(&self, pr: &crate::PrRef) -> PathBuf {
        self.agent_dir()
            .join(format!("{}.state.json", pr.file_key()))
    }

    /// The results of the security checks and audits, one file per pull request.
    pub fn checks_dir(&self) -> PathBuf {
        self.root.join("checks")
    }

    /// What the checks of `pr` found, for as long as the review lasts.
    pub fn checks_file(&self, pr: &crate::PrRef) -> PathBuf {
        self.checks_dir().join(format!("{}.json", pr.file_key()))
    }

    /// What the test setup and the test runs keep: records, logs, temp folders and caches.
    pub fn tests_dir(&self) -> PathBuf {
        self.root.join("tests")
    }

    /// What the tests of `pr` know, for as long as the review lasts.
    pub fn tests_file(&self, pr: &crate::PrRef) -> PathBuf {
        self.tests_dir().join(format!("{}.json", pr.file_key()))
    }

    /// The logs of the test runs of `pr`.
    pub fn test_logs_for(&self, pr: &crate::PrRef) -> PathBuf {
        self.tests_dir().join("logs").join(pr.file_key())
    }

    pub fn test_log(&self, pr: &crate::PrRef, run: &str) -> PathBuf {
        self.test_logs_for(pr).join(format!("{run}.log"))
    }

    /// The temp folder of the runs of `pr`. Named by a short hash, because a Unix-socket path
    /// has a length limit.
    pub fn test_tmp_for(&self, pr: &crate::PrRef) -> PathBuf {
        let digest = Sha256::digest(pr.file_key().as_bytes());
        let short = digest[..6]
            .iter()
            .fold(String::new(), |acc, b| acc + &format!("{b:02x}"));
        self.tests_dir().join("tmp").join(short)
    }

    /// The package caches that the runs of one repository share.
    pub fn test_cache_for(&self, owner: &str, repo: &str) -> PathBuf {
        self.tests_dir()
            .join("cache")
            .join(format!("{owner}~{repo}"))
    }

    /// The drawn diagrams of every review, one folder per pull request.
    pub fn diagrams_dir(&self) -> PathBuf {
        self.root.join("diagrams")
    }

    /// The versions of the diagrams of `pr`, as `v<n>.json` files, for as long as the review lasts.
    pub fn diagrams_for(&self, pr: &crate::PrRef) -> PathBuf {
        self.diagrams_dir().join(pr.file_key())
    }

    /// The Stories of every review, one folder per pull request.
    pub fn stories_dir(&self) -> PathBuf {
        self.root.join("stories")
    }

    /// The versions of the Story of `pr` as `v<n>.json` files, and its `read.json`, for as long as
    /// the review lasts.
    pub fn stories_for(&self, pr: &crate::PrRef) -> PathBuf {
        self.stories_dir().join(pr.file_key())
    }

    /// The call indexes of every review, one folder per pull request.
    pub fn calls_dir(&self) -> PathBuf {
        self.root.join("calls")
    }

    /// The call index of the head commit of `pr`, as one `<sha>.json` file.
    pub fn calls_for(&self, pr: &crate::PrRef) -> PathBuf {
        self.calls_dir().join(pr.file_key())
    }

    /// The comments hidden in each pull request, one file per pull request. They outlive the
    /// review: closing it, even with an empty draft, leaves them.
    pub fn hidden_dir(&self) -> PathBuf {
        self.root.join("hidden")
    }

    pub fn hidden_file(&self, pr: &crate::PrRef) -> PathBuf {
        self.hidden_dir().join(format!("{}.json", pr.file_key()))
    }

    /// The review tabs the window had open (`OpenTabs`). The window decides the set; the daemon
    /// writes the file.
    pub fn open_tabs_file(&self) -> PathBuf {
        self.root.join("tabs.json")
    }

    /// Where the window last sat on each display (the window's own file; the daemon never reads it).
    pub fn window_frames_file(&self) -> PathBuf {
        self.root.join("window.json")
    }

    pub fn activity_file(&self) -> PathBuf {
        self.root.join("activity.jsonl")
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn logs_dir(&self) -> &Path {
        &self.logs
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// Held by the open window (`clusia-app`), so a second launch hands over instead.
    pub fn app_lock(&self) -> PathBuf {
        self.root.join("app.lock")
    }

    /// Held by the running daemon (`flock`), so a second one exits instead of competing.
    pub fn daemon_lock(&self) -> PathBuf {
        self.root.join("clusiad.lock")
    }

    /// The notification inbox: what the tray list shows and which events were already seen.
    pub fn inbox(&self) -> PathBuf {
        self.root.join("inbox.json")
    }

    /// The user's LaunchAgents folder.
    pub fn launch_agents_dir(&self) -> &Path {
        &self.launch_agents
    }

    /// The login item that starts the daemon.
    pub fn launch_agent(&self) -> PathBuf {
        self.launch_agents.join(LAUNCH_AGENT_FILE)
    }

    pub fn socket(&self) -> PathBuf {
        self.root.join("clusiad.sock")
    }

    /// Whether `socket()` is short enough to bind on macOS.
    pub fn socket_path_fits(&self) -> bool {
        self.socket().as_os_str().len() <= MAX_SOCKET_PATH
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_lock_lives_in_the_root() {
        let p = Paths::new("/tmp/clusia-home");
        assert_eq!(p.app_lock(), PathBuf::from("/tmp/clusia-home/app.lock"));
    }

    #[test]
    fn agent_files_are_keyed_by_pull_request() {
        let p = Paths::new("/tmp/c");
        let pr = crate::PrRef::new("acme", "widgets", 7).unwrap();
        assert_eq!(p.agent_dir(), PathBuf::from("/tmp/c/agent"));
        assert_eq!(
            p.agent_log(&pr),
            PathBuf::from("/tmp/c/agent/acme~widgets~7.jsonl")
        );
        assert_eq!(
            p.agent_state(&pr),
            PathBuf::from("/tmp/c/agent/acme~widgets~7.state.json")
        );
    }

    #[test]
    fn check_results_are_keyed_by_pull_request() {
        let p = Paths::new("/tmp/c");
        let pr = crate::PrRef::new("acme", "widgets", 7).unwrap();
        assert_eq!(p.checks_dir(), PathBuf::from("/tmp/c/checks"));
        assert_eq!(
            p.checks_file(&pr),
            PathBuf::from("/tmp/c/checks/acme~widgets~7.json")
        );
    }

    #[test]
    fn diagrams_are_keyed_by_pull_request() {
        let p = Paths::new("/tmp/c");
        let pr = crate::PrRef::new("acme", "widgets", 7).unwrap();
        assert_eq!(p.diagrams_dir(), PathBuf::from("/tmp/c/diagrams"));
        assert_eq!(
            p.diagrams_for(&pr),
            PathBuf::from("/tmp/c/diagrams/acme~widgets~7")
        );
    }

    #[test]
    fn stories_and_calls_are_keyed_by_pull_request() {
        let p = Paths::new("/tmp/c");
        let pr: crate::PrRef = "acme/widgets#7".parse().unwrap();
        assert_eq!(p.stories_dir(), PathBuf::from("/tmp/c/stories"));
        assert_eq!(
            p.stories_for(&pr),
            PathBuf::from("/tmp/c/stories/acme~widgets~7")
        );
        assert_eq!(p.calls_dir(), PathBuf::from("/tmp/c/calls"));
        assert_eq!(
            p.calls_for(&pr),
            PathBuf::from("/tmp/c/calls/acme~widgets~7")
        );
    }

    #[test]
    fn media_lives_under_cache() {
        let p = Paths::new("/tmp/c");
        assert_eq!(p.media_dir(), PathBuf::from("/tmp/c/cache/media"));
    }

    #[test]
    fn new_puts_everything_under_root() {
        let p = Paths::new("/tmp/c");
        assert_eq!(p.root(), Path::new("/tmp/c"));
        assert_eq!(p.logs_dir(), Path::new("/tmp/c/logs"));
        assert_eq!(p.config_file(), PathBuf::from("/tmp/c/config.toml"));
        assert_eq!(p.socket(), PathBuf::from("/tmp/c/clusiad.sock"));
    }

    #[test]
    fn resolve_prefers_clusia_home() {
        let p = Paths::resolve(Some("/x".into()), Some("/Users/me".into())).unwrap();
        assert_eq!(p, Paths::new("/x"));
    }

    #[test]
    fn resolve_uses_macos_defaults() {
        let p = Paths::resolve(None, Some("/Users/me".into())).unwrap();
        assert_eq!(
            p.root(),
            Path::new("/Users/me/Library/Application Support/Clusia")
        );
        assert_eq!(p.logs_dir(), Path::new("/Users/me/Library/Logs/Clusia"));
    }

    #[test]
    fn resolve_ignores_empty_clusia_home() {
        let p = Paths::resolve(Some("".into()), Some("/Users/me".into())).unwrap();
        assert_eq!(
            p.root(),
            Path::new("/Users/me/Library/Application Support/Clusia")
        );
    }

    #[test]
    fn resolve_without_home_errors() {
        assert_eq!(Paths::resolve(None, None), Err(PathsError::NoHome));
    }

    #[test]
    fn socket_path_fits_detects_long_roots() {
        assert!(Paths::new("/tmp/c").socket_path_fits());
        assert!(!Paths::new(format!("/tmp/{}", "a".repeat(120))).socket_path_fits());
    }

    #[test]
    fn repo_and_worktree_dirs() {
        let p = Paths::new("/tmp/c");
        assert_eq!(p.repos_dir(), PathBuf::from("/tmp/c/repos"));
        assert_eq!(p.worktrees_dir(), PathBuf::from("/tmp/c/worktrees"));
        let pr: crate::PrRef = "acme/widgets#7".parse().unwrap();
        assert_eq!(
            p.worktree_for(&pr),
            PathBuf::from("/tmp/c/worktrees/acme~widgets~7")
        );
    }

    #[test]
    fn review_and_activity_files() {
        let p = Paths::new("/tmp/c");
        let pr: crate::PrRef = "acme/widgets#7".parse().unwrap();
        assert_eq!(p.reviews_dir(), PathBuf::from("/tmp/c/reviews"));
        assert_eq!(
            p.review_file(&pr),
            PathBuf::from("/tmp/c/reviews/acme~widgets~7.json")
        );
        assert_eq!(p.activity_file(), PathBuf::from("/tmp/c/activity.jsonl"));
    }

    #[test]
    fn hidden_comments_live_beside_the_reviews_not_in_them() {
        let p = Paths::new("/tmp/c");
        let pr: crate::PrRef = "acme/widgets#7".parse().unwrap();
        assert_eq!(
            p.hidden_file(&pr),
            PathBuf::from("/tmp/c/hidden/acme~widgets~7.json")
        );
    }

    #[test]
    fn relative_clusia_home_is_made_absolute() {
        let p = Paths::resolve(Some("rel/home".into()), Some("/Users/me".into())).unwrap();
        assert!(p.root().is_absolute());
        assert!(p.root().ends_with("rel/home"));
    }

    #[test]
    fn review_cache_file_lives_under_cache() {
        let p = Paths::new("/tmp/c");
        let pr: crate::PrRef = "acme/widgets#7".parse().unwrap();
        assert_eq!(
            p.review_cache_file(&pr),
            PathBuf::from("/tmp/c/cache/reviews/acme~widgets~7.json")
        );
    }

    #[test]
    fn lock_inbox_and_login_item_locations() {
        let p = Paths::new("/tmp/c");
        assert_eq!(p.daemon_lock(), PathBuf::from("/tmp/c/clusiad.lock"));
        assert_eq!(p.inbox(), PathBuf::from("/tmp/c/inbox.json"));
        assert_eq!(
            p.launch_agent(),
            PathBuf::from("/tmp/c/LaunchAgents/io.github.rzorzal.clusia.daemon.plist")
        );
        let real = Paths::resolve(None, Some("/Users/me".into())).unwrap();
        assert_eq!(
            real.launch_agent(),
            PathBuf::from("/Users/me/Library/LaunchAgents/io.github.rzorzal.clusia.daemon.plist")
        );
        assert_eq!(
            real.launch_agents_dir(),
            Path::new("/Users/me/Library/LaunchAgents")
        );
    }

    #[test]
    fn test_files_are_keyed_by_pull_request() {
        let p = Paths::new("/tmp/c");
        let pr = crate::PrRef::new("acme", "widgets", 7).unwrap();
        assert_eq!(p.tests_dir(), PathBuf::from("/tmp/c/tests"));
        assert_eq!(
            p.tests_file(&pr),
            PathBuf::from("/tmp/c/tests/acme~widgets~7.json")
        );
        assert_eq!(
            p.test_logs_for(&pr),
            PathBuf::from("/tmp/c/tests/logs/acme~widgets~7")
        );
        assert_eq!(
            p.test_log(&pr, "r3"),
            PathBuf::from("/tmp/c/tests/logs/acme~widgets~7/r3.log")
        );
        assert_eq!(
            p.test_cache_for("acme", "widgets"),
            PathBuf::from("/tmp/c/tests/cache/acme~widgets")
        );
    }

    #[test]
    fn the_review_temp_folder_is_short_and_stable() {
        let p = Paths::new("/tmp/c");
        let pr = crate::PrRef::new("acme", "widgets", 7).unwrap();
        // sha256("acme~widgets~7") starts 941b4433b49d.
        assert_eq!(
            p.test_tmp_for(&pr),
            PathBuf::from("/tmp/c/tests/tmp/941b4433b49d")
        );
        let other = crate::PrRef::new("acme", "widgets", 8).unwrap();
        assert_ne!(p.test_tmp_for(&pr), p.test_tmp_for(&other));
    }
}
