//! Finding the user's existing clones of a repository.

use std::fs;
use std::path::{Path, PathBuf};

use crate::remote::{RepoId, parse_remote};
use crate::run::{GitError, git};

/// How deep below each root to look: `root/org/repo`.
pub const DISCOVERY_DEPTH: usize = 3;

const SKIP: &[&str] = &["node_modules", "target", "vendor", "Library"];

pub fn expand_root(root: &str, home: &Path) -> PathBuf {
    if root == "~" {
        home.to_path_buf()
    } else if let Some(rest) = root.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(root)
    }
}

pub fn find_clones(roots: &[PathBuf], max_depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for root in roots {
        walk(root, 0, max_depth, &mut found);
    }
    found.sort();
    found.dedup();
    found
}

fn walk(dir: &Path, depth: usize, max_depth: usize, found: &mut Vec<PathBuf>) {
    if dir.join(".git").exists() {
        found.push(dir.to_path_buf());
        return;
    }
    if depth == max_depth {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIP.contains(&name.as_ref()) {
            continue;
        }
        // `file_type` does not follow symlinks, so link loops are impossible.
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            walk(&entry.path(), depth + 1, max_depth, found);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub url: String,
}

pub async fn remotes(repo: &Path) -> Result<Vec<Remote>, GitError> {
    match git(repo, &["config", "--get-regexp", r"^remote\..*\.url$"]).await {
        Ok(out) => Ok(out
            .lines()
            .filter_map(|line| {
                let (key, url) = line.split_once(' ')?;
                let name = key.strip_prefix("remote.")?.strip_suffix(".url")?;
                Some(Remote {
                    name: name.to_string(),
                    url: url.trim().to_string(),
                })
            })
            .collect()),
        // `git config --get-regexp` exits 1 when nothing matches.
        Err(GitError::Failed { .. }) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalClone {
    pub path: PathBuf,
    pub remote: String,
}

pub async fn find_local_clone(
    roots: &[PathBuf],
    max_depth: usize,
    target: &RepoId,
) -> Result<Option<LocalClone>, GitError> {
    let roots = roots.to_vec();
    let candidates = tokio::task::spawn_blocking(move || find_clones(&roots, max_depth))
        .await
        .unwrap_or_default();
    for path in candidates {
        let list = match remotes(&path).await {
            Ok(list) => list,
            Err(GitError::Missing) => return Err(GitError::Missing),
            Err(_) => continue,
        };
        if let Some(remote) = list
            .into_iter()
            .find(|r| parse_remote(&r.url).as_ref() == Some(target))
        {
            return Ok(Some(LocalClone {
                path,
                remote: remote.name,
            }));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn init(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
    }

    fn add_remote(dir: &Path, name: &str, url: &str) {
        assert!(
            Command::new("git")
                .args(["remote", "add", name, url])
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
    }

    #[test]
    fn expand_root_handles_tilde() {
        let home = Path::new("/Users/me");
        assert_eq!(expand_root("~", home), PathBuf::from("/Users/me"));
        assert_eq!(
            expand_root("~/Repos", home),
            PathBuf::from("/Users/me/Repos")
        );
        assert_eq!(expand_root("/opt/src", home), PathBuf::from("/opt/src"));
    }

    #[test]
    fn find_clones_walks_roots_and_skips_noise() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init(&root.join("a"));
        init(&root.join("org/b"));
        init(&root.join("a/nested")); // inside a repo: not reported separately
        init(&root.join(".hidden/c"));
        init(&root.join("node_modules/d"));
        init(&root.join("x/y/z/too-deep"));
        let found = find_clones(&[root.to_path_buf()], DISCOVERY_DEPTH);
        assert_eq!(found, vec![root.join("a"), root.join("org/b")]);
    }

    #[tokio::test]
    async fn remotes_lists_names_and_urls() {
        let tmp = tempfile::tempdir().unwrap();
        init(tmp.path());
        assert!(remotes(tmp.path()).await.unwrap().is_empty());
        add_remote(tmp.path(), "origin", "git@github.com:acme/widgets.git");
        add_remote(
            tmp.path(),
            "upstream",
            "https://github.com/upstream/widgets",
        );
        let mut got = remotes(tmp.path()).await.unwrap();
        got.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(
            got,
            vec![
                Remote {
                    name: "origin".into(),
                    url: "git@github.com:acme/widgets.git".into()
                },
                Remote {
                    name: "upstream".into(),
                    url: "https://github.com/upstream/widgets".into()
                },
            ]
        );
    }

    #[tokio::test]
    async fn find_local_clone_matches_by_repo_id() {
        let tmp = tempfile::tempdir().unwrap();
        let other = tmp.path().join("other");
        let mine = tmp.path().join("work/widgets");
        init(&other);
        add_remote(&other, "origin", "https://github.com/acme/gadgets.git");
        init(&mine);
        add_remote(&mine, "fork", "git@github.com:me/widgets.git");
        add_remote(&mine, "up", "git@github.com:ACME/widgets.git");
        let target = RepoId::new("github.com", "acme", "widgets");
        let found = find_local_clone(&[tmp.path().to_path_buf()], DISCOVERY_DEPTH, &target)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            found,
            LocalClone {
                path: mine,
                remote: "up".into()
            }
        );
        let missing = RepoId::new("github.com", "acme", "nothing");
        assert!(
            find_local_clone(&[tmp.path().to_path_buf()], DISCOVERY_DEPTH, &missing)
                .await
                .unwrap()
                .is_none()
        );
    }
}
