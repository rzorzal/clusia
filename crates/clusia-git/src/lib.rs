//! Local git repositories. Clúsia never changes the user's working tree or branches (spec §5.2).

pub mod diff;
pub mod discover;
pub mod grep;
pub mod remote;
pub mod run;
pub mod show;
pub mod worktree;

pub use diff::{
    DIFF_CUT_NOTE, base_pin_ref, diagram_ref, diff_between, diff_for_prompt, fetch_branch,
    is_object_id, merge_base, pin_commit, repo_of_worktree, reviewed_ref, unpin, unpin_diagrams,
};
pub use discover::{
    DISCOVERY_DEPTH, LocalClone, Remote, expand_root, find_clones, find_local_clone, remotes,
};
pub use grep::{GrepCaps, GrepHit, GrepResult, grep_words};
pub use remote::{RepoId, parse_remote};
pub use run::{GitError, git, git_with_timeout};
pub use show::file_at;
pub use worktree::{clone_partial, ensure_worktree, fetch_pr, pr_ref_name, remove_worktree};
