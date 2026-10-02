//! Running the git CLI.

use std::io;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git is not installed or not on PATH")]
    Missing,
    #[error("`git {args}` failed: {stderr}")]
    Failed { args: String, stderr: String },
    #[error("`git {args}` did not finish within {secs}s")]
    Timeout { args: String, secs: u64 },
    #[error("i/o error running git: {0}")]
    Io(#[from] io::Error),
}

/// Variables that would point git at a repository other than `dir`.
const REPOSITORY_ENV: [&str; 5] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_COMMON_DIR",
];

/// `git -C <dir>`, isolated from the caller's repository variables and never prompting.
pub(crate) fn command(dir: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(dir);
    for key in REPOSITORY_ENV {
        cmd.env_remove(key);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    cmd
}

/// `git -C <dir> <args>`. Never prompts for credentials.
pub async fn git(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    run(dir, args).await
}

/// Like [`git`], but the process is killed after `timeout` (for network operations).
pub async fn git_with_timeout(
    dir: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<String, GitError> {
    // Dropping the future on timeout drops the child, which `kill_on_drop` kills.
    tokio::time::timeout(timeout, run(dir, args))
        .await
        .map_err(|_| GitError::Timeout {
            args: args.join(" "),
            secs: timeout.as_secs(),
        })?
}

async fn run(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    let out = command(dir).args(args).output().await.map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            GitError::Missing
        } else {
            GitError::Io(e)
        }
    })?;
    if !out.status.success() {
        return Err(GitError::Failed {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn command_strips_repository_env() {
        let cmd = command(Path::new("/tmp"));
        let envs: Vec<_> = cmd.as_std().get_envs().collect();
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_COMMON_DIR",
        ] {
            assert!(
                envs.contains(&(OsStr::new(key), None)),
                "{key} is not removed: {envs:?}"
            );
        }
        assert!(envs.contains(&(OsStr::new("GIT_TERMINAL_PROMPT"), Some(OsStr::new("0")))));
    }

    #[tokio::test]
    async fn slow_git_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        let err = git_with_timeout(
            dir.path(),
            &["-c", "alias.slow=!sleep 5", "slow"],
            Duration::from_millis(200),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, GitError::Timeout { .. }), "{err}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
