//! Comparing commits for relocation, and keeping reviewed commits reachable.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::run::{GitError, git, git_with_timeout};

const NETWORK_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) fn reject_dash(kind: &str, name: &str) -> Result<(), GitError> {
    if name.starts_with('-') {
        return Err(GitError::Failed {
            args: String::new(),
            stderr: format!("invalid {kind} name {name:?}"),
        });
    }
    Ok(())
}

/// A full SHA-1 (40) or SHA-256 (64) hex object name.
pub fn is_object_id(name: &str) -> bool {
    matches!(name.len(), 40 | 64) && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Rejects anything but a full object id before it reaches git's argv as a positional argument.
pub(crate) fn require_object_id(name: &str) -> Result<(), GitError> {
    if is_object_id(name) {
        return Ok(());
    }
    Err(GitError::Failed {
        args: String::new(),
        stderr: format!("invalid commit id {name:?}"),
    })
}

/// Zero-context diff with rename detection, the input of `clusia_core::DiffMap::parse`.
pub async fn diff_between(repo: &Path, old: &str, new: &str) -> Result<String, GitError> {
    require_object_id(old)?;
    require_object_id(new)?;
    git(
        repo,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "-U0",
            "-M",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            old,
            new,
            "--",
        ],
    )
    .await
}

/// The line that ends a diff `diff_for_prompt` cut.
pub const DIFF_CUT_NOTE: &str = "[diff truncated — read the files in the worktree]";

/// The diff from `old` to `new` with 3 lines of context and rename detection, for a prompt: cut
/// at the last whole line within `cap` bytes, with `DIFF_CUT_NOTE` as its last line when cut.
pub async fn diff_for_prompt(
    repo: &Path,
    old: &str,
    new: &str,
    cap: usize,
) -> Result<String, GitError> {
    require_object_id(old)?;
    require_object_id(new)?;
    let diff = git(
        repo,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "-U3",
            "-M",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            old,
            new,
            "--",
        ],
    )
    .await?;
    Ok(cut_diff(diff, cap))
}

fn cut_diff(diff: String, cap: usize) -> String {
    if diff.len() <= cap {
        return diff;
    }
    let mut end = cap;
    while !diff.is_char_boundary(end) {
        end -= 1;
    }
    let end = diff[..end].rfind('\n').unwrap_or(end);
    format!("{}\n{DIFF_CUT_NOTE}", &diff[..end])
}

pub fn reviewed_ref(number: u64) -> String {
    format!("refs/clusia/reviewed/pr-{number}")
}

/// The pin that keeps the commit of version `version` of the diagrams of pull request `number`
/// reachable.
pub fn diagram_ref(number: u64, version: u32) -> String {
    format!("refs/clusia/diagrams/pr-{number}-v{version}")
}

/// The pin that keeps the commit of version `version` of the story of pull request `number`
/// reachable.
pub fn story_ref(number: u64, version: u32) -> String {
    format!("refs/clusia/story/pr-{number}-v{version}")
}

/// Removes every diagram pin of pull request `number`: each one is tried, and the first
/// failure is returned after the rest are gone.
pub async fn unpin_diagrams(repo: &Path, number: u64) -> Result<(), GitError> {
    unpin_matching(repo, &format!("refs/clusia/diagrams/pr-{number}-v*")).await
}

/// Removes every story pin of pull request `number`, as `unpin_diagrams` does.
pub async fn unpin_stories(repo: &Path, number: u64) -> Result<(), GitError> {
    unpin_matching(repo, &format!("refs/clusia/story/pr-{number}-v*")).await
}

async fn unpin_matching(repo: &Path, pattern: &str) -> Result<(), GitError> {
    let listed = git(repo, &["for-each-ref", "--format=%(refname)", pattern]).await?;
    let mut first = None;
    for name in listed.lines().filter(|name| !name.is_empty()) {
        if let Err(e) = unpin(repo, name).await {
            first.get_or_insert(e);
        }
    }
    first.map_or(Ok(()), Err)
}

/// Keeps `sha` reachable (a force-push would otherwise let gc drop the reviewed commit).
pub async fn pin_commit(repo: &Path, refname: &str, sha: &str) -> Result<(), GitError> {
    require_object_id(sha)?;
    git(repo, &["update-ref", refname, sha]).await.map(|_| ())
}

pub async fn unpin(repo: &Path, refname: &str) -> Result<(), GitError> {
    if git(repo, &["rev-parse", "--verify", "--quiet", refname])
        .await
        .is_err()
    {
        return Ok(());
    }
    git(repo, &["update-ref", "-d", refname]).await.map(|_| ())
}

/// Fetches `branch` into `refs/clusia/base/<branch>` and returns its SHA.
pub async fn fetch_branch(repo: &Path, remote: &str, branch: &str) -> Result<String, GitError> {
    reject_dash("remote", remote)?;
    reject_dash("branch", branch)?;
    let refspec = format!("+refs/heads/{branch}:refs/clusia/base/{branch}");
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
        NETWORK_TIMEOUT,
    )
    .await?;
    git(
        repo,
        &[
            "rev-parse",
            "--verify",
            &format!("refs/clusia/base/{branch}^{{commit}}"),
        ],
    )
    .await
}

pub fn base_pin_ref(number: u64) -> String {
    format!("{}-base", reviewed_ref(number))
}

/// The merge base of two commits: the version a PR's left-side line numbers refer to.
pub async fn merge_base(repo: &Path, a: &str, b: &str) -> Result<String, GitError> {
    require_object_id(a)?;
    require_object_id(b)?;
    git(repo, &["merge-base", a, b]).await
}

/// The repository directory that owns `worktree`.
pub async fn repo_of_worktree(worktree: &Path) -> Result<PathBuf, GitError> {
    let common = PathBuf::from(
        git(
            worktree,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?,
    );
    if common.file_name().is_some_and(|n| n == ".git")
        && let Some(parent) = common.parent()
    {
        return Ok(parent.to_path_buf());
    }
    Ok(common)
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

    fn repo_with_two_commits() -> (tempfile::TempDir, PathBuf, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("r");
        sh(
            tmp.path(),
            &["init", "-q", "-b", "main", repo.to_str().unwrap()],
        );
        std::fs::write(repo.join("a.txt"), "1\n2\n3\n").unwrap();
        std::fs::write(repo.join("old.txt"), "x\ny\n").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-q", "-m", "one"]);
        let first = sh(&repo, &["rev-parse", "HEAD"]);
        std::fs::write(repo.join("a.txt"), "0\n1\n2\n3\n").unwrap();
        sh(&repo, &["mv", "old.txt", "new.txt"]);
        sh(&repo, &["commit", "-q", "-am", "two"]);
        let second = sh(&repo, &["rev-parse", "HEAD"]);
        (tmp, repo, first, second)
    }

    #[tokio::test]
    async fn diagram_pins_are_removed_for_their_pull_request_only() {
        let (_t, repo, first, second) = repo_with_two_commits();
        for (number, version, sha) in [(7, 1, &first), (7, 2, &second), (70, 1, &first)] {
            pin_commit(&repo, &diagram_ref(number, version), sha)
                .await
                .unwrap();
        }
        assert_eq!(diagram_ref(7, 2), "refs/clusia/diagrams/pr-7-v2");
        unpin_diagrams(&repo, 7).await.unwrap();
        let left = sh(
            &repo,
            &["for-each-ref", "--format=%(refname)", "refs/clusia"],
        );
        assert_eq!(left, "refs/clusia/diagrams/pr-70-v1");
        unpin_diagrams(&repo, 7).await.unwrap();
    }

    #[tokio::test]
    async fn a_pin_that_cannot_be_removed_does_not_keep_the_others() {
        let (_t, repo, first, second) = repo_with_two_commits();
        for (version, sha) in [(1, &first), (2, &second), (3, &first)] {
            pin_commit(&repo, &diagram_ref(7, version), sha)
                .await
                .unwrap();
        }
        // A lock left by another git process makes deleting that one ref fail.
        let lock = repo.join(".git/refs/clusia/diagrams/pr-7-v1.lock");
        std::fs::write(&lock, "").unwrap();
        assert!(
            unpin_diagrams(&repo, 7).await.is_err(),
            "the failure is told"
        );
        let left = sh(
            &repo,
            &["for-each-ref", "--format=%(refname)", "refs/clusia"],
        );
        assert_eq!(left, "refs/clusia/diagrams/pr-7-v1", "the others are gone");
        std::fs::remove_file(&lock).unwrap();
        unpin_diagrams(&repo, 7).await.unwrap();
    }

    #[tokio::test]
    async fn story_pins_are_removed_for_their_pull_request_only() {
        let (_t, repo, first, second) = repo_with_two_commits();
        for (number, version, sha) in [(7, 1, &first), (7, 2, &second), (70, 1, &first)] {
            pin_commit(&repo, &story_ref(number, version), sha)
                .await
                .unwrap();
        }
        pin_commit(&repo, &diagram_ref(7, 1), &first).await.unwrap();
        assert_eq!(story_ref(7, 2), "refs/clusia/story/pr-7-v2");
        unpin_stories(&repo, 7).await.unwrap();
        let left = sh(
            &repo,
            &["for-each-ref", "--format=%(refname)", "refs/clusia"],
        );
        assert_eq!(
            left,
            "refs/clusia/diagrams/pr-7-v1\nrefs/clusia/story/pr-70-v1"
        );
        unpin_stories(&repo, 7).await.unwrap();
    }

    #[tokio::test]
    async fn diff_between_is_zero_context_with_renames() {
        let (_t, repo, first, second) = repo_with_two_commits();
        let diff = diff_between(&repo, &first, &second).await.unwrap();
        assert!(diff.contains("@@ -0,0 +1 @@"), "{diff}");
        assert!(
            diff.contains("rename from old.txt") && diff.contains("rename to new.txt"),
            "{diff}"
        );
    }

    #[tokio::test]
    async fn diff_for_prompt_has_three_lines_of_context_and_renames() {
        let (_t, repo, first, second) = repo_with_two_commits();
        let diff = diff_for_prompt(&repo, &first, &second, 100_000)
            .await
            .unwrap();
        assert!(diff.contains("@@ -1,3 +1,4 @@"), "{diff}");
        assert!(diff.contains("+0\n 1\n 2\n 3"), "{diff}");
        assert!(
            diff.contains("rename from old.txt") && diff.contains("rename to new.txt"),
            "{diff}"
        );
        assert!(!diff.contains(DIFF_CUT_NOTE));
    }

    #[tokio::test]
    async fn diff_for_prompt_ignores_user_prefix_config() {
        let (_t, repo, first, second) = repo_with_two_commits();
        sh(&repo, &["config", "diff.noprefix", "true"]);
        let diff = diff_for_prompt(&repo, &first, &second, 100_000)
            .await
            .unwrap();
        assert!(diff.contains("diff --git a/a.txt b/a.txt"), "{diff}");
    }

    #[tokio::test]
    async fn diff_for_prompt_stops_at_a_whole_line_and_says_so() {
        let (_t, repo, first, second) = repo_with_two_commits();
        let whole = diff_for_prompt(&repo, &first, &second, 100_000)
            .await
            .unwrap();
        let cut = diff_for_prompt(&repo, &first, &second, 60).await.unwrap();
        assert!(cut.ends_with(&format!("\n{DIFF_CUT_NOTE}")), "{cut}");
        let body = cut.strip_suffix(&format!("\n{DIFF_CUT_NOTE}")).unwrap();
        assert!(body.len() <= 60 && whole.starts_with(body), "{cut}");
        assert!(whole[body.len()..].starts_with('\n'), "a whole line");
        let exact = diff_for_prompt(&repo, &first, &second, whole.len())
            .await
            .unwrap();
        assert_eq!(exact, whole, "a diff that fits is not cut");
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        let diff = "ação ação ação".to_string();
        let cut = cut_diff(diff, 2);
        assert_eq!(cut, format!("a\n{DIFF_CUT_NOTE}"));
    }

    #[tokio::test]
    async fn diff_for_prompt_validates_its_ids() {
        let (_t, repo, first, _second) = repo_with_two_commits();
        assert!(
            diff_for_prompt(&repo, "--output=/tmp/x", &first, 10)
                .await
                .is_err()
        );
        assert!(diff_for_prompt(&repo, &first, "HEAD", 10).await.is_err());
    }

    #[tokio::test]
    async fn diff_between_ignores_user_prefix_config() {
        let (_t, repo, first, second) = repo_with_two_commits();
        sh(&repo, &["config", "diff.noprefix", "true"]);
        let diff = diff_between(&repo, &first, &second).await.unwrap();
        assert!(diff.contains("diff --git a/a.txt b/a.txt"), "{diff}");
    }

    #[tokio::test]
    async fn pin_keeps_old_head_after_force_push() {
        let (_t, repo, _first, second) = repo_with_two_commits();
        pin_commit(&repo, &reviewed_ref(7), &second).await.unwrap();
        // Rewrite history: `second` is now unreachable from any branch (like after a force-push).
        sh(&repo, &["commit", "-q", "--amend", "-m", "rewritten"]);
        let third = sh(&repo, &["rev-parse", "HEAD"]);
        sh(&repo, &["reflog", "expire", "--expire=now", "--all"]);
        sh(&repo, &["gc", "-q", "--prune=now"]);
        assert_eq!(sh(&repo, &["rev-parse", &reviewed_ref(7)]), second);
        sh(&repo, &["cat-file", "-e", &format!("{second}^{{commit}}")]);
        assert!(diff_between(&repo, &second, &third).await.is_ok());
        unpin(&repo, &reviewed_ref(7)).await.unwrap();
        unpin(&repo, &reviewed_ref(7)).await.unwrap(); // already gone: fine
    }

    #[tokio::test]
    async fn fetch_branch_and_repo_of_worktree() {
        let (t, repo, _first, second) = repo_with_two_commits();
        let clone = t.path().join("clone");
        sh(
            t.path(),
            &[
                "clone",
                "-q",
                repo.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        assert_eq!(
            fetch_branch(&clone, "origin", "main").await.unwrap(),
            second
        );
        assert!(
            fetch_branch(&clone, "origin", "--upload-pack=evil")
                .await
                .is_err()
        );
        let wt = t.path().join("wt");
        sh(
            &clone,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                wt.to_str().unwrap(),
                &second,
            ],
        );
        let owner = repo_of_worktree(&wt).await.unwrap();
        assert_eq!(
            std::fs::canonicalize(owner).unwrap(),
            std::fs::canonicalize(&clone).unwrap()
        );
    }

    #[tokio::test]
    async fn merge_base_of_diverged_commits() {
        let (_t, repo, first, second) = repo_with_two_commits();
        sh(&repo, &["checkout", "-q", "-b", "side", &first]);
        std::fs::write(repo.join("side.txt"), "s\n").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-q", "-m", "side"]);
        let side = sh(&repo, &["rev-parse", "HEAD"]);
        assert_eq!(merge_base(&repo, &second, &side).await.unwrap(), first);
        assert_eq!(base_pin_ref(7), "refs/clusia/reviewed/pr-7-base");
    }

    #[test]
    fn object_ids_are_full_hex_shas() {
        assert!(is_object_id(&"a".repeat(40)));
        assert!(is_object_id(&"0123456789ABCDEFabcdef".repeat(2)[..40]));
        assert!(is_object_id(&"f".repeat(64)));
        for bad in [
            String::new(),
            "abc123".to_string(),
            "g".repeat(40),
            "a".repeat(41),
            "a".repeat(63),
            format!("-{}", "a".repeat(39)),
            "HEAD".to_string(),
            "refs/heads/main".to_string(),
        ] {
            assert!(!is_object_id(&bad), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn positional_shas_are_validated_before_running_git() {
        let (_t, repo, first, second) = repo_with_two_commits();
        let evil = "--output=/tmp/clusia-pwned";
        assert!(diff_between(&repo, evil, &second).await.is_err());
        assert!(diff_between(&repo, &first, evil).await.is_err());
        assert!(merge_base(&repo, evil, &second).await.is_err());
        assert!(merge_base(&repo, &first, "HEAD").await.is_err());
        assert!(pin_commit(&repo, &reviewed_ref(7), evil).await.is_err());
        assert!(
            pin_commit(&repo, &reviewed_ref(7), "HEAD").await.is_err(),
            "symbolic names are not accepted"
        );
        assert!(merge_base(&repo, &first, &second).await.is_ok());
    }

    #[test]
    fn credentials_are_redacted() {
        assert_eq!(
            crate::run::redact_credentials(
                "fatal: unable to access 'https://bob:ghp_x@github.com/a/b.git/'"
            ),
            "fatal: unable to access 'https://***@github.com/a/b.git/'"
        );
        assert_eq!(
            crate::run::redact_credentials("ssh://git@host/x"),
            "ssh://***@host/x"
        );
        assert_eq!(crate::run::redact_credentials("no url here"), "no url here");
    }
}
