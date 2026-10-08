//! What the first-run screen shows: whether `gh` is signed in, how many repositories sit in
//! the configured folders, and which agent command-line tools are installed. Nothing here
//! changes anything.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use clusia_git::expand_root;
use clusia_protocol::{FirstRun, GithubLogin, Harness, HarnessKind, RepoFolder};
use serde_json::Value;

use crate::state::Shared;

const GH_TIMEOUT: Duration = Duration::from_secs(5);
/// A Node-based harness started cold on a busy machine can take a few seconds to answer.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// Repositories are looked for this many levels below a folder.
const REPO_DEPTH: usize = 2;
const SKIPPED_DIRS: [&str; 2] = ["node_modules", "target"];
const HARNESSES: [(HarnessKind, &str); 2] = [
    (HarnessKind::ClaudeCode, "claude"),
    (HarnessKind::Codex, "codex"),
];

pub(crate) async fn status(shared: &Shared) -> FirstRun {
    let (host, roots) = {
        let config = shared.config.read().await;
        (
            config.github.host.clone(),
            config.repositories.roots.clone(),
        )
    };
    let github = github_login(&shared.gh_program, &host, GH_TIMEOUT).await;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let folders = tokio::task::spawn_blocking(move || {
        roots
            .iter()
            .map(|root| folder(root, home.as_deref()))
            .collect()
    })
    .await
    .unwrap_or_default();
    let mut harnesses = Vec::new();
    for (kind, name) in HARNESSES {
        harnesses.push(harness(kind, name, &shared.harness_search_paths).await);
    }
    FirstRun {
        github,
        folders,
        harnesses,
    }
}

/// `gh auth status --hostname <host> --json hosts`. It exits 0 whatever the state, so the JSON
/// decides; a missing, slow or incomprehensible `gh` is `Unknown`, never "signed out".
async fn github_login(program: &Path, host: &str, timeout: Duration) -> GithubLogin {
    let run = tokio::process::Command::new(program)
        .args(["auth", "status", "--hostname", host, "--json", "hosts"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(timeout, run).await {
        Ok(Ok(out)) => parse_login(&out.stdout, host),
        _ => GithubLogin::Unknown,
    }
}

fn parse_login(stdout: &[u8], host: &str) -> GithubLogin {
    let Ok(json) = serde_json::from_slice::<Value>(stdout) else {
        return GithubLogin::Unknown;
    };
    let Some(hosts) = json.get("hosts").and_then(Value::as_object) else {
        return GithubLogin::Unknown;
    };
    let accounts = hosts
        .get(host)
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty());
    let Some(accounts) = accounts else {
        return GithubLogin::SignedOut;
    };
    let account = accounts
        .iter()
        .find(|a| a.get("active").and_then(Value::as_bool) == Some(true))
        .unwrap_or(&accounts[0]);
    let text = |key: &str| account.get(key).and_then(Value::as_str);
    match text("state") {
        Some("success") => GithubLogin::SignedIn {
            login: text("login").unwrap_or_default().to_string(),
            scopes: text("scopes")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect(),
        },
        Some(_) => GithubLogin::Error {
            message: text("error")
                .unwrap_or("gh could not check this login")
                .to_string(),
        },
        None => GithubLogin::Unknown,
    }
}

fn folder(root: &str, home: Option<&Path>) -> RepoFolder {
    let expanded = match home {
        Some(home) => expand_root(root, home),
        None if root.starts_with('~') => PathBuf::new(),
        None => PathBuf::from(root),
    };
    let exists = expanded.is_dir();
    RepoFolder {
        path: root.to_string(),
        exists,
        repos: if exists { count_repos(&expanded, 1) } else { 0 },
    }
}

/// Repositories among the sub-folders of `dir`, which sit `depth` levels below the folder.
/// A repository (its `.git` may be a file) is counted and not entered; symlinks, dot-folders
/// and dependency folders are skipped.
fn count_repos(dir: &Path, depth: usize) -> u32 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut count = 0;
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !kind.is_dir() || name.starts_with('.') || SKIPPED_DIRS.contains(&name.as_ref()) {
            continue;
        }
        let path = entry.path();
        if path.join(".git").exists() {
            count += 1;
        } else if depth < REPO_DEPTH {
            count += count_repos(&path, depth + 1);
        }
    }
    count
}

pub(crate) fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The first `name` in `dirs` that runs and reports a version. A stale link or a script that
/// fails does not count.
async fn harness(kind: HarnessKind, name: &str, dirs: &[PathBuf]) -> Harness {
    for dir in dirs {
        let path = dir.join(name);
        if !is_executable(&path) {
            continue;
        }
        if let Some(version) = version_of(&path).await {
            return Harness {
                kind,
                path: Some(path.display().to_string()),
                version: Some(version),
            };
        }
    }
    Harness {
        kind,
        path: None,
        version: None,
    }
}

/// The first word that starts with a digit in the first line of `--version`
/// (`2.1.291 (Claude Code)` → `2.1.291`), else the whole line.
async fn version_of(program: &Path) -> Option<String> {
    let run = tokio::process::Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(VERSION_TIMEOUT, run)
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?.trim();
    let word = line
        .split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or(line);
    (!word.is_empty()).then(|| word.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logins_from_gh_json() {
        let ok = br#"{"hosts":{"github.com":[
            {"state":"success","active":false,"login":"maria","scopes":"gist"},
            {"state":"success","active":true,"login":"octo","scopes":"read:org, repo"}]}}"#;
        assert_eq!(
            parse_login(ok, "github.com"),
            GithubLogin::SignedIn {
                login: "octo".into(),
                scopes: vec!["read:org".into(), "repo".into()]
            }
        );
        assert_eq!(
            parse_login(br#"{"hosts":{}}"#, "github.com"),
            GithubLogin::SignedOut
        );
        assert_eq!(parse_login(ok, "ghe.example.com"), GithubLogin::SignedOut);
        assert_eq!(
            parse_login(
                br#"{"hosts":{"github.com":[{"state":"error","active":true,"error":"401 Unauthorized"}]}}"#,
                "github.com"
            ),
            GithubLogin::Error {
                message: "401 Unauthorized".into()
            }
        );
        assert_eq!(parse_login(b"", "github.com"), GithubLogin::Unknown);
        assert_eq!(parse_login(b"[]", "github.com"), GithubLogin::Unknown);
    }

    #[tokio::test]
    async fn a_slow_gh_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let gh = dir.path().join("gh");
        std::fs::write(&gh, "#!/bin/sh\nexec sleep 5\n").unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        let login = github_login(&gh, "github.com", Duration::from_millis(200)).await;
        assert_eq!(login, GithubLogin::Unknown);
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
