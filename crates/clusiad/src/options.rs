//! How the daemon reaches GitHub and stores secrets. Tests replace every outside dependency.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clusia_platform::{Keychain, MemoryStore, SecretStore};

use crate::spawner::{ProcessSpawner, Spawner};

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
    /// Starts the editor for `OpenInEditor`.
    pub spawner: Arc<dyn Spawner>,
    /// Exact host names media may also be fetched from, over plain http (tests point it at a
    /// local server; production leaves it empty).
    pub media_extra_hosts: Vec<String>,
    /// Lets fetches of images from other sites reach plain-http and local addresses, which they
    /// never may in production. Tests set it to use a local server.
    pub media_allow_local: bool,
    /// Host names that media fetches send to a fixed address instead of asking DNS (tests
    /// point `github.com` at a local server; production leaves it empty). The port comes
    /// from the URL.
    pub media_resolve: Vec<(String, SocketAddr)>,
    /// Giphy API base URL override. Env: `CLUSIA_GIPHY_API`.
    pub giphy_api: Option<String>,
    /// Folders searched, in order, for the `claude` and `codex` commands. A window started from
    /// the Dock has a bare `PATH`, so `from_env` adds the usual install folders after it.
    pub harness_search_paths: Vec<PathBuf>,
    /// The `claude` program used when `harness.program` is empty. Env: `CLUSIA_CLAUDE_BIN`.
    pub claude_program: Option<PathBuf>,
    /// The `clusiad` that `claude` starts as the permission bridge. Env: `CLUSIA_BRIDGE_BIN`;
    /// default: this executable.
    pub bridge_program: Option<PathBuf>,
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
        let search_paths = harness_dirs(
            std::env::var_os("PATH"),
            std::env::var_os("HOME").map(PathBuf::from),
        );
        Self {
            github_api: var("CLUSIA_GITHUB_API"),
            github_token: var("CLUSIA_GITHUB_TOKEN"),
            gh_program: gh_program_from(var("CLUSIA_GH_BIN"), &search_paths),
            secrets,
            background_sync: true,
            tray_program: tray_program_from(var("CLUSIA_TRAY_BIN"), std::env::current_exe().ok()),
            spawner: Arc::new(ProcessSpawner),
            media_extra_hosts: Vec::new(),
            media_allow_local: false,
            media_resolve: Vec::new(),
            giphy_api: var("CLUSIA_GIPHY_API"),
            harness_search_paths: search_paths,
            claude_program: var("CLUSIA_CLAUDE_BIN").map(PathBuf::from),
            bridge_program: var("CLUSIA_BRIDGE_BIN").map(PathBuf::from),
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

/// `CLUSIA_GH_BIN` wins; otherwise the first `gh` found in `dirs`, which are searched in order
/// (a window started from the Dock or by launchd has a bare `PATH`, so the usual install
/// folders follow it); otherwise plain `gh`, left to the system's lookup.
pub fn gh_program_from(env: Option<String>, dirs: &[PathBuf]) -> PathBuf {
    if let Some(program) = env {
        return PathBuf::from(program);
    }
    dirs.iter()
        .map(|dir| dir.join("gh"))
        .find(|candidate| crate::first_run::is_executable(candidate))
        .unwrap_or_else(|| PathBuf::from("gh"))
}

/// The `PATH` folders, then the folders agent tools are usually installed in.
pub fn harness_dirs(path: Option<std::ffi::OsString>, home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = path
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let usual = [
        home.as_ref().map(|h| h.join(".local/bin")),
        home.as_ref().map(|h| h.join(".claude/local")),
        Some(PathBuf::from("/opt/homebrew/bin")),
        Some(PathBuf::from("/usr/local/bin")),
    ];
    for dir in usual.into_iter().flatten() {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_dirs_follow_path_then_the_usual_folders() {
        let dirs = harness_dirs(
            Some("/usr/bin:/opt/homebrew/bin".into()),
            Some(PathBuf::from("/Users/me")),
        );
        assert_eq!(
            dirs,
            [
                "/usr/bin",
                "/opt/homebrew/bin",
                "/Users/me/.local/bin",
                "/Users/me/.claude/local",
                "/usr/local/bin"
            ]
            .map(PathBuf::from)
        );
        assert_eq!(
            harness_dirs(None, None),
            ["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from)
        );
    }

    #[test]
    fn gh_is_taken_from_the_first_folder_that_has_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let (bare, brew, local) = (
            dir.path().join("bare"),
            dir.path().join("brew"),
            dir.path().join("local"),
        );
        for folder in [&bare, &brew, &local] {
            std::fs::create_dir_all(folder).unwrap();
        }
        let install = |folder: &PathBuf, mode: u32| {
            let gh = folder.join("gh");
            std::fs::write(&gh, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(mode)).unwrap();
            gh
        };
        let dirs = [bare.clone(), brew.clone(), local.clone()];
        assert_eq!(gh_program_from(None, &dirs), PathBuf::from("gh"));
        install(&bare, 0o644); // present but not runnable
        let in_local = install(&local, 0o755);
        assert_eq!(gh_program_from(None, &dirs), in_local);
        let in_brew = install(&brew, 0o755);
        assert_eq!(
            gh_program_from(None, &dirs),
            in_brew,
            "the earlier folder wins"
        );
        assert_eq!(
            gh_program_from(Some("/opt/custom/gh".into()), &dirs),
            PathBuf::from("/opt/custom/gh"),
            "the override wins over every folder"
        );
    }

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
