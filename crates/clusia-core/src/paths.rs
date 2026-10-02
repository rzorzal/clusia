//! Where Clúsia keeps its files. Every binary resolves paths through here.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// macOS limits `sun_path` to 104 bytes including the trailing NUL.
pub const MAX_SOCKET_PATH: usize = 103;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    root: PathBuf,
    logs: PathBuf,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PathsError {
    #[error("neither CLUSIA_HOME nor HOME is set")]
    NoHome,
}

impl Paths {
    /// Everything under `root`, logs in `root/logs`. Used by tests and `--home`.
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

    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let logs = root.join("logs");
        Self { root, logs }
    }

    /// `CLUSIA_HOME` when set and non-empty, otherwise the macOS defaults.
    pub fn from_env() -> Result<Self, PathsError> {
        Self::resolve(std::env::var_os("CLUSIA_HOME"), std::env::var_os("HOME"))
    }

    fn resolve(clusia_home: Option<OsString>, home: Option<OsString>) -> Result<Self, PathsError> {
        if let Some(dir) = clusia_home.filter(|d| !d.is_empty()) {
            return Ok(Self::new(dir));
        }
        let home = PathBuf::from(home.filter(|h| !h.is_empty()).ok_or(PathsError::NoHome)?);
        Ok(Self {
            root: home.join("Library/Application Support/Clusia"),
            logs: home.join("Library/Logs/Clusia"),
        })
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
            PathBuf::from("/tmp/c/worktrees/acme__widgets__7")
        );
    }
}
