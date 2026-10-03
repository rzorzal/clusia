//! Fetching PR heads and keeping Clúsia's own worktrees, without touching the user's tree.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::run::{GitError, git, git_with_timeout};

const FETCH_TIMEOUT: Duration = Duration::from_secs(120);
const CLONE_TIMEOUT: Duration = Duration::from_secs(600);

pub fn pr_ref_name(number: u64) -> String {
    format!("refs/clusia/pr-{number}")
}

/// Fetches the PR head into `refs/clusia/pr-<n>` (never a branch) and returns its SHA.
pub async fn fetch_pr(repo: &Path, remote: &str, number: u64) -> Result<String, GitError> {
    crate::diff::reject_dash("remote", remote)?;
    let refname = pr_ref_name(number);
    let refspec = format!("+refs/pull/{number}/head:{refname}");
    // No FETCH_HEAD and no auto-gc/maintenance: the user's clone only gains the ref above.
    git_with_timeout(
        repo,
        &[
            "fetch",
            "--no-tags",
            "--quiet",
            "--no-write-fetch-head",
            "--no-auto-gc",
            remote,
            &refspec,
        ],
        FETCH_TIMEOUT,
    )
    .await?;
    git(
        repo,
        &["rev-parse", "--verify", &format!("{refname}^{{commit}}")],
    )
    .await
}

/// Creates, or moves, Clúsia's detached worktree at `path` to `sha`. A worktree that belongs to
/// another repository (or whose repository is gone) is recreated from `repo`; `.clusia/` survives.
pub async fn ensure_worktree(repo: &Path, path: &Path, sha: &str) -> Result<(), GitError> {
    if path.join(".git").exists() {
        let owner = common_dir(path).await.ok();
        if owner.is_some() && owner == common_dir(repo).await.ok() {
            git(path, &["checkout", "--quiet", "--detach", "--force", sha]).await?;
            return exclude_clusia_dir(path).await;
        }
        discard_worktree(repo, path).await?;
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let target = path.to_string_lossy();
    git(
        repo,
        &["worktree", "add", "--quiet", "--detach", &target, sha],
    )
    .await?;
    restore_clusia_dir(path).await?;
    exclude_clusia_dir(path).await
}

/// The repository's shared git dir, canonicalized so equal repositories compare equal.
async fn common_dir(dir: &Path) -> Result<PathBuf, GitError> {
    let common = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    Ok(tokio::fs::canonicalize(common).await?)
}

/// Where `.clusia/` waits while the worktree at `path` is recreated.
fn kept_clusia_dir(path: &Path) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{name}.clusia-kept"))
}

/// Removes the worktree at `path` (owned by another or a missing repository), keeping `.clusia/` aside.
async fn discard_worktree(repo: &Path, path: &Path) -> Result<(), GitError> {
    let notes = path.join(".clusia");
    let kept = kept_clusia_dir(path);
    if tokio::fs::try_exists(&notes).await? {
        if tokio::fs::try_exists(&kept).await? {
            tokio::fs::remove_dir_all(&kept).await?;
        }
        tokio::fs::rename(&notes, &kept).await?;
    }
    let target = path.to_string_lossy();
    let old_repo = common_dir(path).await.ok();
    let removed = match &old_repo {
        Some(old) => git(old, &["worktree", "remove", "--force", &target])
            .await
            .is_ok(),
        None => false,
    };
    if !removed && tokio::fs::try_exists(path).await? {
        tokio::fs::remove_dir_all(path).await?;
    }
    if let Some(old) = &old_repo {
        let _ = git(old, &["worktree", "prune"]).await;
    }
    git(repo, &["worktree", "prune"]).await.map(|_| ())
}

/// Moves a `.clusia/` kept aside by `discard_worktree` back into the worktree.
async fn restore_clusia_dir(path: &Path) -> Result<(), GitError> {
    let kept = kept_clusia_dir(path);
    let notes = path.join(".clusia");
    if tokio::fs::try_exists(&kept).await? && !tokio::fs::try_exists(&notes).await? {
        tokio::fs::rename(&kept, &notes).await?;
    }
    Ok(())
}

/// Lists `.clusia/` in the repository's shared `info/exclude`, so review notes never show up as changes.
async fn exclude_clusia_dir(worktree: &Path) -> Result<(), GitError> {
    let common = git(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    let exclude = Path::new(&common).join("info/exclude");
    let current = match tokio::fs::read_to_string(&exclude).await {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    if current.lines().any(|l| l.trim() == ".clusia/") {
        return Ok(());
    }
    if let Some(parent) = exclude.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let mut updated = current;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(".clusia/\n");
    tokio::fs::write(&exclude, updated).await?;
    Ok(())
}

pub async fn remove_worktree(repo: &Path, path: &Path) -> Result<(), GitError> {
    if path.exists() {
        let target = path.to_string_lossy();
        git(repo, &["worktree", "remove", "--force", &target]).await?;
    }
    git(repo, &["worktree", "prune"]).await.map(|_| ())
}

/// Blob-less clone used when the user has no local clone of the repository.
pub async fn clone_partial(url: &str, dest: &Path) -> Result<(), GitError> {
    let parent = dest.parent().unwrap_or(Path::new("."));
    tokio::fs::create_dir_all(parent).await?;
    let target = dest.to_string_lossy();
    git_with_timeout(
        parent,
        &[
            "clone",
            "--quiet",
            "--filter=blob:none",
            "--no-checkout",
            url,
            &target,
        ],
        CLONE_TIMEOUT,
    )
    .await
    .map(|_| ())
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

    /// origin.git with `main` and `refs/pull/1/head`, plus the user's clone on `main`.
    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        seed: PathBuf,
        clone: PathBuf,
        pr_sha: String,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let origin = root.join("origin.git");
        let seed = root.join("seed");
        sh(
            &root,
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                origin.to_str().unwrap(),
            ],
        );
        sh(&root, &["init", "-q", "-b", "main", seed.to_str().unwrap()]);
        std::fs::write(seed.join("README.md"), "hello\n").unwrap();
        sh(&seed, &["add", "."]);
        sh(&seed, &["commit", "-q", "-m", "init"]);
        sh(
            &seed,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        sh(&seed, &["push", "-q", "origin", "main"]);
        sh(&seed, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(seed.join("feature.txt"), "new\n").unwrap();
        sh(&seed, &["add", "."]);
        sh(&seed, &["commit", "-q", "-m", "feature"]);
        sh(&seed, &["push", "-q", "origin", "feature:refs/pull/1/head"]);
        let pr_sha = sh(&seed, &["rev-parse", "HEAD"]);
        let clone = root.join("clone");
        sh(
            &root,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        Fixture {
            _tmp: tmp,
            root,
            seed,
            clone,
            pr_sha,
        }
    }

    #[tokio::test]
    async fn fetch_pr_returns_head_sha_without_creating_branches() {
        let f = fixture();
        let before = sh(&f.clone, &["branch", "--list"]);
        let sha = fetch_pr(&f.clone, "origin", 1).await.unwrap();
        assert_eq!(sha, f.pr_sha);
        assert_eq!(sh(&f.clone, &["branch", "--list"]), before);
        assert_eq!(sh(&f.clone, &["rev-parse", "refs/clusia/pr-1"]), f.pr_sha);
        assert!(
            !f.clone.join(".git/FETCH_HEAD").exists(),
            "fetch_pr must not write FETCH_HEAD into the user's clone"
        );
    }

    #[tokio::test]
    async fn ensure_worktree_creates_detached_and_leaves_user_tree() {
        let f = fixture();
        let sha = fetch_pr(&f.clone, "origin", 1).await.unwrap();
        let wt = f.root.join("worktrees/acme~widgets~1");
        ensure_worktree(&f.clone, &wt, &sha).await.unwrap();
        assert!(wt.join("feature.txt").exists());
        assert_eq!(sh(&wt, &["rev-parse", "HEAD"]), sha);
        assert_eq!(
            sh(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "HEAD",
            "worktree must be detached"
        );
        assert_eq!(sh(&f.clone, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
        assert_eq!(sh(&f.clone, &["status", "--porcelain"]), "");
        assert_eq!(sh(&f.clone, &["branch", "--list"]), "* main");
    }

    #[tokio::test]
    async fn ensure_worktree_moves_to_new_sha_and_keeps_clusia_dir() {
        let f = fixture();
        let sha = fetch_pr(&f.clone, "origin", 1).await.unwrap();
        let wt = f.root.join("worktrees/acme~widgets~1");
        ensure_worktree(&f.clone, &wt, &sha).await.unwrap();
        std::fs::create_dir_all(wt.join(".clusia")).unwrap();
        std::fs::write(wt.join(".clusia/review.md"), "notes\n").unwrap();

        std::fs::write(f.seed.join("feature.txt"), "newer\n").unwrap();
        sh(&f.seed, &["commit", "-q", "-am", "more"]);
        sh(
            &f.seed,
            &["push", "-q", "-f", "origin", "feature:refs/pull/1/head"],
        );
        let new_sha = fetch_pr(&f.clone, "origin", 1).await.unwrap();
        assert_ne!(new_sha, sha);

        ensure_worktree(&f.clone, &wt, &new_sha).await.unwrap();
        assert_eq!(sh(&wt, &["rev-parse", "HEAD"]), new_sha);
        assert_eq!(
            std::fs::read_to_string(wt.join(".clusia/review.md")).unwrap(),
            "notes\n"
        );
        assert_eq!(
            sh(&wt, &["status", "--porcelain"]),
            "",
            ".clusia/ must be excluded"
        );
    }

    fn second_clone(f: &Fixture) -> PathBuf {
        let clone = f.root.join("clone-b");
        sh(
            &f.root,
            &[
                "clone",
                "-q",
                f.root.join("origin.git").to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        clone
    }

    fn common_dir(worktree: &Path) -> PathBuf {
        let dir = sh(
            worktree,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        );
        std::fs::canonicalize(dir).unwrap()
    }

    #[tokio::test]
    async fn ensure_worktree_recreates_when_repo_changes() {
        let f = fixture();
        let sha = fetch_pr(&f.clone, "origin", 1).await.unwrap();
        let wt = f.root.join("worktrees/acme~widgets~1");
        ensure_worktree(&f.clone, &wt, &sha).await.unwrap();
        std::fs::create_dir_all(wt.join(".clusia")).unwrap();
        std::fs::write(wt.join(".clusia/review.md"), "notes\n").unwrap();

        let b = second_clone(&f);
        assert_eq!(fetch_pr(&b, "origin", 1).await.unwrap(), sha);
        ensure_worktree(&b, &wt, &sha).await.unwrap();

        assert_eq!(
            common_dir(&wt),
            std::fs::canonicalize(b.join(".git")).unwrap()
        );
        assert_eq!(sh(&wt, &["rev-parse", "HEAD"]), sha);
        assert_eq!(
            std::fs::read_to_string(wt.join(".clusia/review.md")).unwrap(),
            "notes\n"
        );
        assert_eq!(sh(&wt, &["status", "--porcelain"]), "");
        assert_eq!(
            sh(&f.clone, &["worktree", "list"]).lines().count(),
            1,
            "clone A no longer lists the worktree"
        );
    }

    #[tokio::test]
    async fn ensure_worktree_recovers_from_deleted_owner() {
        let f = fixture();
        let sha = fetch_pr(&f.clone, "origin", 1).await.unwrap();
        let wt = f.root.join("worktrees/acme~widgets~1");
        ensure_worktree(&f.clone, &wt, &sha).await.unwrap();
        std::fs::create_dir_all(wt.join(".clusia")).unwrap();
        std::fs::write(wt.join(".clusia/review.md"), "notes\n").unwrap();
        let b = second_clone(&f);
        assert_eq!(fetch_pr(&b, "origin", 1).await.unwrap(), sha);
        std::fs::remove_dir_all(&f.clone).unwrap();

        ensure_worktree(&b, &wt, &sha).await.unwrap();
        assert_eq!(
            common_dir(&wt),
            std::fs::canonicalize(b.join(".git")).unwrap()
        );
        assert_eq!(sh(&wt, &["rev-parse", "HEAD"]), sha);
        assert_eq!(
            std::fs::read_to_string(wt.join(".clusia/review.md")).unwrap(),
            "notes\n"
        );
    }

    #[tokio::test]
    async fn remove_worktree_cleans_up() {
        let f = fixture();
        let sha = fetch_pr(&f.clone, "origin", 1).await.unwrap();
        let wt = f.root.join("worktrees/acme~widgets~1");
        ensure_worktree(&f.clone, &wt, &sha).await.unwrap();
        remove_worktree(&f.clone, &wt).await.unwrap();
        assert!(!wt.exists());
        assert_eq!(sh(&f.clone, &["worktree", "list"]).lines().count(), 1);
        remove_worktree(&f.clone, &wt).await.unwrap(); // idempotent
    }

    #[tokio::test]
    async fn clone_partial_clones_from_url() {
        let f = fixture();
        let dest = f.root.join("cache/acme/widgets");
        clone_partial(f.root.join("origin.git").to_str().unwrap(), &dest)
            .await
            .unwrap();
        assert!(dest.join(".git").exists());
        assert_eq!(fetch_pr(&dest, "origin", 1).await.unwrap(), f.pr_sha);
    }

    #[tokio::test]
    async fn fetch_of_unknown_pr_fails_cleanly() {
        let f = fixture();
        assert!(matches!(
            fetch_pr(&f.clone, "origin", 99).await,
            Err(GitError::Failed { .. })
        ));
    }

    #[tokio::test]
    async fn dash_remote_is_rejected_without_running_git() {
        let f = fixture();
        assert!(matches!(
            fetch_pr(&f.clone, "--upload-pack=evil", 1).await,
            Err(GitError::Failed { .. })
        ));
    }
}
