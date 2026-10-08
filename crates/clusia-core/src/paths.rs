//! Where Clúsia keeps its files. Every binary resolves paths through here.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

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

    /// What the last successful open fetched for `pr`, for "Open from cache".
    pub fn review_cache_file(&self, pr: &crate::PrRef) -> PathBuf {
        self.root
            .join("cache/reviews")
            .join(format!("{}.json", pr.file_key()))
    }

    /// Images and GIFs downloaded for comments, named by `media::cache_key`. Only the daemon
    /// writes here.
    pub fn media_dir(&self) -> PathBuf {
        self.root.join("cache/media")
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
}
