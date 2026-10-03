//! Local git repositories. Clúsia never changes the user's working tree or branches (spec §5.2).

pub mod diff;
pub mod discover;
pub mod remote;
pub mod run;
pub mod worktree;

pub use diff::{
    base_pin_ref, diff_between, fetch_branch, merge_base, pin_commit, repo_of_worktree,
    reviewed_ref, unpin,
};
pub use discover::{
    DISCOVERY_DEPTH, LocalClone, Remote, expand_root, find_clones, find_local_clone, remotes,
};
pub use remote::{RepoId, parse_remote};
pub use run::{GitError, git, git_with_timeout};
pub use worktree::{clone_partial, ensure_worktree, fetch_pr, pr_ref_name, remove_worktree};
