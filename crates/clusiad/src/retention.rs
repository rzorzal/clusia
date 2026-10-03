//! Removing worktrees nobody needs any more (spec §5.2: on publish/discard, and after N idle days).

use std::time::Duration;

use clusia_core::{PrRef, ReviewState};
use clusia_git::{remove_worktree, repo_of_worktree};
use clusia_store::{ReviewLoad, load_review};

use crate::state::Shared;
use crate::sync::now_unix;

pub(crate) const SWEEP_EVERY: Duration = Duration::from_secs(6 * 3600);

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
        if name.starts_with('.') {
            continue;
        }
        let keep = PrRef::from_file_key(&name).is_some_and(|pr| match load_review(&shared.paths, &pr) {
            // Work in flight is never swept, however long it has been idle.
            Ok(ReviewLoad::Found(r)) => {
                matches!(r.state, ReviewState::Active | ReviewState::Publishing)
                    || (!r.state.is_terminal() && r.updated_at >= cutoff)
            }
            Ok(_) => false,
            Err(e) => {
                tracing::warn!(error = %e, pr = %pr, "cannot read a review; keeping its worktree");
                true
            }
        });
        if keep {
            continue;
        }
        let path = entry.path();
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
