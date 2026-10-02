//! Running the git CLI.

use std::io;
use std::path::Path;
use std::process::Stdio;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git is not installed or not on PATH")]
    Missing,
    #[error("`git {args}` failed: {stderr}")]
    Failed { args: String, stderr: String },
    #[error("i/o error running git: {0}")]
    Io(#[from] io::Error),
}

/// `git -C <dir> <args>`. Never prompts for credentials.
pub async fn git(dir: &Path, args: &[&str]) -> Result<String, GitError> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| {
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
