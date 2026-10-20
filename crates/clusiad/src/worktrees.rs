//! Preparing a PR worktree: find (or clone) the repository, fetch the PR head, check it out detached.

use std::path::PathBuf;
use std::time::Instant;

use clusia_core::{PrDetail, PrRef};
use clusia_git::{
    DISCOVERY_DEPTH, GitError, LocalClone, RepoId, clone_partial, ensure_worktree, expand_root,
    fetch_pr, find_local_clone, git, parse_remote,
};
use clusia_protocol::{ErrorCode, Outcome, ProtocolError, Reply, WorktreeInfo};

use crate::handlers::{no_token, provider_error};
use crate::state::Shared;
use crate::sync;

pub(crate) async fn prepare(shared: &Shared, pr: &PrRef) -> Outcome {
    let gh = match sync::github_client(shared).await {
        Ok(Some(gh)) => gh,
        Ok(None) => return no_token(),
        Err(e) => return provider_error(e),
    };
    let detail = match gh.get_pr(pr).await {
        Ok(detail) => detail,
        Err(e) => return provider_error(e),
    };
    let _serialized = shared.worktree_lock.lock().await;
    match checkout(shared, pr, &detail).await {
        Ok((info, _)) => Outcome::Ok(Reply::Worktree(info)),
        Err(e) => Outcome::Err(ProtocolError::new(ErrorCode::Git, e.to_string())),
    }
}

/// Whether `local`'s remote still names `target`: one `git config`, far cheaper than a walk.
async fn still_points_at(local: &LocalClone, target: &RepoId) -> bool {
    let key = format!("remote.{}.url", local.remote);
    git(&local.path, &["config", "--get", &key])
        .await
        .is_ok_and(|url| parse_remote(url.trim()).as_ref() == Some(target))
}

pub(crate) fn forget_clone(shared: &Shared, target: &RepoId) {
    shared
        .clone_map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .forget(target);
}

pub(crate) async fn checkout(
    shared: &Shared,
    pr: &PrRef,
    detail: &PrDetail,
) -> Result<(WorktreeInfo, String), GitError> {
    let (host, roots) = {
        let config = shared.config.read().await;
        (
            config.github.host.clone(),
            config.repositories.roots.clone(),
        )
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let roots: Vec<PathBuf> = roots
        .iter()
        .filter_map(|r| match &home {
            Some(h) => Some(expand_root(r, h)),
            None if r.starts_with('~') => None,
            None => Some(PathBuf::from(r)),
        })
        .collect();
    let target = RepoId::new(&host, &pr.owner, &pr.repo);

    let remembered = shared
        .clone_map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&roots, &target, Instant::now());
    // The clone must still point at the repository: its remote may have been re-pointed.
    let remembered = match remembered {
        Some(Some(local)) if !still_points_at(&local, &target).await => {
            forget_clone(shared, &target);
            None
        }
        other => other,
    };
    let local = match remembered {
        Some(local) => local,
        None => {
            let found = find_local_clone(&roots, DISCOVERY_DEPTH, &target).await?;
            shared
                .clone_map
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .put(&roots, &target, found.clone(), Instant::now());
            found
        }
    };

    let (repo, remote, cloned) = match local {
        Some(local) => (local.path, local.remote, false),
        None => {
            let dest = shared.paths.repos_dir().join(&pr.owner).join(&pr.repo);
            if !dest.join(".git").exists() {
                if dest.exists() {
                    // An interrupted clone left a directory behind; start over.
                    tokio::fs::remove_dir_all(&dest).await?;
                }
                clone_partial(&detail.clone_url, &dest).await?;
            }
            (dest, "origin".to_string(), true)
        }
    };
    let sha = match fetch_pr(&repo, &remote, pr.number).await {
        Ok(sha) => sha,
        Err(e) => {
            // The remembered clone may be what is wrong.
            forget_clone(shared, &target);
            return Err(e);
        }
    };
    let path = shared.paths.worktree_for(pr);
    ensure_worktree(&repo, &path, &sha).await?;
    Ok((
        WorktreeInfo {
            path: path.display().to_string(),
            head_sha: sha,
            clone: repo.display().to_string(),
            cloned,
        },
        remote,
    ))
}
