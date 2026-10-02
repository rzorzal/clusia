//! Local git repositories. Clúsia never changes the user's working tree or branches (spec §5.2).

pub mod discover;
pub mod remote;
pub mod run;

pub use discover::{
    DISCOVERY_DEPTH, LocalClone, Remote, expand_root, find_clones, find_local_clone, remotes,
};
pub use remote::{RepoId, parse_remote};
pub use run::{GitError, git};
