//! How the daemon reaches GitHub and stores secrets. Tests replace every outside dependency.

use std::path::PathBuf;
use std::sync::Arc;

use clusia_platform::{Keychain, MemoryStore, SecretStore};

pub struct DaemonOptions {
    /// GitHub API base URL override. Env: `CLUSIA_GITHUB_API`.
    pub github_api: Option<String>,
    /// Token that wins over gh and the Keychain. Env: `CLUSIA_GITHUB_TOKEN`.
    pub github_token: Option<String>,
    /// The `gh` executable. Env: `CLUSIA_GH_BIN`.
    pub gh_program: PathBuf,
    pub secrets: Arc<dyn SecretStore>,
    /// Run the periodic GitHub sync.
    pub background_sync: bool,
    /// The menu bar tray to spawn and supervise. Env: `CLUSIA_TRAY_BIN` (`none` disables);
    /// default: `clusia-tray` next to this executable, when present.
    pub tray_program: Option<PathBuf>,
}

impl DaemonOptions {
    pub fn from_env() -> Self {
        let var = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
        let secrets: Arc<dyn SecretStore> =
            if var("CLUSIA_SECRET_STORE").as_deref() == Some("memory") {
                Arc::new(MemoryStore::default())
            } else {
                Arc::new(Keychain::new(Keychain::GITHUB_SERVICE))
            };
        Self {
            github_api: var("CLUSIA_GITHUB_API"),
            github_token: var("CLUSIA_GITHUB_TOKEN"),
            gh_program: var("CLUSIA_GH_BIN")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("gh")),
            secrets,
            background_sync: true,
            tray_program: tray_program_from(var("CLUSIA_TRAY_BIN"), std::env::current_exe().ok()),
        }
    }
}

/// `none` disables the tray, any other value is the program to run; without the variable,
/// `clusia-tray` next to the daemon's own executable is used when it exists.
pub fn tray_program_from(env: Option<String>, exe: Option<PathBuf>) -> Option<PathBuf> {
    match env.as_deref() {
        Some("none") => None,
        Some(program) => Some(PathBuf::from(program)),
        None => exe
            .map(|e| e.with_file_name("clusia-tray"))
            .filter(|p| p.is_file()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_program_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("clusiad");
        assert_eq!(
            tray_program_from(Some("none".into()), Some(exe.clone())),
            None
        );
        assert_eq!(
            tray_program_from(Some("/opt/tray".into()), Some(exe.clone())),
            Some(PathBuf::from("/opt/tray"))
        );
        // No sibling binary: no tray (e.g. a bare `cargo run -p clusiad`).
        assert_eq!(tray_program_from(None, Some(exe.clone())), None);
        std::fs::write(dir.path().join("clusia-tray"), "").unwrap();
        assert_eq!(
            tray_program_from(None, Some(exe)),
            Some(dir.path().join("clusia-tray"))
        );
        assert_eq!(tray_program_from(None, None), None);
    }
}
