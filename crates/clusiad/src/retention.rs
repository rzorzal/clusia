//! Removing worktrees nobody needs any more (spec §5.2: on publish/discard, and after N idle days).

use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use clusia_core::{PrRef, ReviewState};
use clusia_git::{remove_worktree, repo_of_worktree};
use clusia_store::{ReviewLoad, load_review};

use crate::reviews::{cleanup_checkout, lock};
use crate::state::Shared;
use crate::sync::now_unix;

pub(crate) const SWEEP_EVERY: Duration = Duration::from_secs(6 * 3600);

/// Whether `dir` was modified at or after `cutoff` (unknown times count as recent).
fn recently_modified(dir: &Path, cutoff: i64) -> bool {
    let Ok(modified) = std::fs::metadata(dir).and_then(|m| m.modified()) else {
        return true;
    };
    match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX) >= cutoff,
        Err(_) => false,
    }
}

/// Whether the review of `pr` still needs its worktree at `dir`.
fn needed(shared: &Shared, pr: &PrRef, dir: &Path, cutoff: i64) -> bool {
    match load_review(&shared.paths, pr) {
        // Work in flight is never swept, however long it has been idle.
        Ok(ReviewLoad::Found(r)) => {
            matches!(r.state, ReviewState::Active | ReviewState::Publishing)
                || (!r.state.is_terminal() && r.updated_at >= cutoff)
        }
        // No review claims it: an orphan ages out like any other worktree.
        Ok(_) => recently_modified(dir, cutoff),
        Err(e) => {
            tracing::warn!(error = %e, pr = %pr, "cannot read a review; keeping its worktree");
            true
        }
    }
}

pub(crate) async fn sweep(shared: &Shared) -> usize {
    let days = i64::from(
        shared
            .config
            .read()
            .await
            .repositories
            .worktree_retention_days,
    );
    let cutoff = now_unix() - days * 86_400;
    let Ok(mut entries) = tokio::fs::read_dir(shared.paths.worktrees_dir()).await else {
        return 0;
    };
    let mut removed = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || shared.is_touched(&name) {
            continue;
        }
        let path = entry.path();
        if let Some(pr) = PrRef::from_file_key(&name) {
            // Same order as publish and the saved-review check: review lock, then worktree lock.
            let _guard = lock(shared, &pr).await;
            // Decide under the lock: the review may have been opened or saved meanwhile.
            if shared.is_touched(&name) || needed(shared, &pr, &path, cutoff) {
                continue;
            }
            cleanup_checkout(shared, &pr).await;
            if path.exists() {
                tracing::warn!(path = %path.display(), "cannot remove a stale worktree");
                continue;
            }
            removed += 1;
            continue;
        }
        // Not one of ours (e.g. an old key format): no review can claim it, so only age counts.
        if recently_modified(&path, cutoff) {
            continue;
        }
        let _serialized = shared.worktree_lock.lock().await;
        let done = match repo_of_worktree(&path).await {
            Ok(repo) => remove_worktree(&repo, &path).await.is_ok(),
            Err(_) => false,
        };
        if !done && tokio::fs::remove_dir_all(&path).await.is_err() {
            tracing::warn!(path = %path.display(), "cannot remove a stale worktree");
            continue;
        }
        removed += 1;
    }
    removed
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use clusia_core::{Config, Paths, Review};
    use clusia_platform::MemoryStore;
    use clusia_store::save_review;

    use super::*;
    use crate::options::DaemonOptions;

    fn shared() -> (tempfile::TempDir, Shared) {
        let dir = tempfile::tempdir().unwrap();
        let options = DaemonOptions {
            github_api: Some("http://127.0.0.1:9".into()),
            github_token: None,
            gh_program: "/nonexistent/gh".into(),
            secrets: Arc::new(MemoryStore::default()),
            background_sync: false,
        };
        let shared = Shared::new(Paths::new(dir.path()), Config::default(), options);
        (dir, shared)
    }

    /// A review last touched 100 days ago, with a worktree directory.
    fn old_review(shared: &Shared, n: u64, state: ReviewState) -> PrRef {
        let pr: PrRef = format!("acme/widgets#{n}").parse().unwrap();
        let mut review = Review::new(
            pr.clone(),
            "t".into(),
            "b".into(),
            "h".into(),
            now_unix() - 100 * 86_400,
        );
        review.state = state;
        save_review(&shared.paths, &review).unwrap();
        std::fs::create_dir_all(shared.paths.worktree_for(&pr)).unwrap();
        pr
    }

    /// Backdates `dir`'s modification time by `days`.
    fn age(dir: &std::path::Path, days: u64) {
        let when = std::time::SystemTime::now() - Duration::from_secs(days * 86_400);
        std::fs::File::open(dir)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }

    #[tokio::test]
    async fn sweep_keeps_recent_orphans_and_removes_old_ones() {
        let (_dir, shared) = shared();
        let fresh: PrRef = "acme/widgets#5".parse().unwrap();
        let old: PrRef = "acme/widgets#6".parse().unwrap();
        for pr in [&fresh, &old] {
            std::fs::create_dir_all(shared.paths.worktree_for(pr)).unwrap();
        }
        age(&shared.paths.worktree_for(&old), 100);
        let fresh_foreign = shared.paths.worktrees_dir().join("acme__widgets__8");
        let old_foreign = shared.paths.worktrees_dir().join("acme__widgets__9");
        for dir in [&fresh_foreign, &old_foreign] {
            std::fs::create_dir_all(dir).unwrap();
        }
        age(&old_foreign, 100);

        assert_eq!(sweep(&shared).await, 2);
        assert!(shared.paths.worktree_for(&fresh).exists());
        assert!(!shared.paths.worktree_for(&old).exists());
        assert!(fresh_foreign.exists());
        assert!(!old_foreign.exists());
    }

    #[tokio::test]
    async fn sweep_keeps_worktrees_touched_this_session() {
        let (_dir, shared) = shared();
        let pr = old_review(&shared, 1, ReviewState::Saved);
        shared.touch(&pr);
        assert_eq!(sweep(&shared).await, 0);
        assert!(shared.paths.worktree_for(&pr).exists());
    }

    #[tokio::test]
    async fn sweep_removes_stale_saved_but_keeps_old_active() {
        let (_dir, shared) = shared();
        let stale = old_review(&shared, 2, ReviewState::Saved);
        let active = old_review(&shared, 3, ReviewState::Active);
        let publishing = old_review(&shared, 4, ReviewState::Publishing);
        assert_eq!(sweep(&shared).await, 1);
        assert!(!shared.paths.worktree_for(&stale).exists());
        assert!(shared.paths.worktree_for(&active).exists());
        assert!(shared.paths.worktree_for(&publishing).exists());
    }
}
