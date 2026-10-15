//! Reading a file as it was at a commit.

use std::path::Path;

use crate::diff::require_object_id;
use crate::run::{GitError, git};

fn refused(why: &str) -> GitError {
    GitError::Failed {
        args: String::new(),
        stderr: why.to_string(),
    }
}

/// `path` as a text file at commit `sha` (a full object id): at most `max_bytes` long, with no NUL
/// byte. The trailing line break is not part of what comes back. A path that is not a plain
/// relative one (empty, with a `.` or `..` part, or ending in `/`) is refused, so `sha:path`
/// always names a file of the commit's own tree.
pub async fn file_at(
    repo: &Path,
    sha: &str,
    path: &str,
    max_bytes: u64,
) -> Result<String, GitError> {
    require_object_id(sha)?;
    if path.is_empty()
        || path.starts_with('/')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(refused(&format!("invalid path {path:?}")));
    }
    let spec = format!("{sha}:{path}");
    let size = git(repo, &["cat-file", "-s", &spec]).await?;
    if size.parse::<u64>().is_ok_and(|size| size > max_bytes) {
        return Err(refused("the file is too big to show"));
    }
    let text = git(repo, &["cat-file", "blob", &spec]).await?;
    if text.contains('\0') {
        return Err(refused("the file is not text"));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repository whose first commit has `src/a.rs`, `big.txt` and `bin.dat`, and whose second
    /// rewrites `src/a.rs`; returns the two commits.
    fn repo() -> (tempfile::TempDir, std::path::PathBuf, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("r");
        sh(
            tmp.path(),
            &["init", "-q", "-b", "main", repo.to_str().unwrap()],
        );
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/a.rs"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(repo.join("big.txt"), "x".repeat(100)).unwrap();
        std::fs::write(repo.join("bin.dat"), b"a\0b").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-q", "-m", "one"]);
        let first = sh(&repo, &["rev-parse", "HEAD"]);
        std::fs::write(repo.join("src/a.rs"), "zero\none\ntwo\nthree\n").unwrap();
        sh(&repo, &["commit", "-q", "-am", "two"]);
        let second = sh(&repo, &["rev-parse", "HEAD"]);
        (tmp, repo, first, second)
    }

    #[tokio::test]
    async fn a_file_is_read_as_it_was_at_each_commit() {
        let (_t, repo, first, second) = repo();
        assert_eq!(
            file_at(&repo, &first, "src/a.rs", 1_000).await.unwrap(),
            "one\ntwo\nthree"
        );
        assert_eq!(
            file_at(&repo, &second, "src/a.rs", 1_000).await.unwrap(),
            "zero\none\ntwo\nthree"
        );
    }

    #[tokio::test]
    async fn what_is_not_a_small_text_file_of_the_commit_is_refused() {
        let (_t, repo, first, _second) = repo();
        for (sha, path) in [
            (first.as_str(), "missing.rs"),
            (first.as_str(), "src"),
            (first.as_str(), "src/"),
            (first.as_str(), "../a.rs"),
            (first.as_str(), "./src/a.rs"),
            (first.as_str(), "/src/a.rs"),
            (first.as_str(), ""),
            (first.as_str(), "bin.dat"),
            ("HEAD", "src/a.rs"),
            ("--output=/tmp/x", "src/a.rs"),
        ] {
            assert!(
                file_at(&repo, sha, path, 1_000).await.is_err(),
                "{sha}:{path}"
            );
        }
        assert!(file_at(&repo, &first, "big.txt", 50).await.is_err());
        assert!(file_at(&repo, &first, "big.txt", 100).await.is_ok());
    }
}
