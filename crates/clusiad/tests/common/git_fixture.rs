#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn sh(dir: &Path, args: &[&str]) -> String {
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

pub struct Origin {
    pub path: PathBuf,
    pub pr_sha: String,
}

/// A bare `origin.git` under `root` with `main` and `refs/pull/<number>/head` (adds `feature.txt`).
pub fn origin_with_pr(root: &Path, number: u64) -> Origin {
    let origin = root.join("origin.git");
    let seed = root.join("seed");
    sh(
        root,
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    sh(root, &["init", "-q", "-b", "main", seed.to_str().unwrap()]);
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
    sh(
        &seed,
        &[
            "push",
            "-q",
            "origin",
            &format!("feature:refs/pull/{number}/head"),
        ],
    );
    let pr_sha = sh(&seed, &["rev-parse", "HEAD"]);
    Origin {
        path: origin,
        pr_sha,
    }
}

/// The user's clone at `dest`. Its remote *says* `remote_url` (e.g. GitHub), but fetches go to `origin`.
pub fn user_clone(origin: &Origin, dest: &Path, remote_url: &str) -> PathBuf {
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    sh(
        dest.parent().unwrap(),
        &[
            "clone",
            "-q",
            origin.path.to_str().unwrap(),
            dest.to_str().unwrap(),
        ],
    );
    sh(dest, &["remote", "set-url", "origin", remote_url]);
    sh(
        dest,
        &[
            "config",
            &format!("url.{}.insteadOf", origin.path.display()),
            remote_url,
        ],
    );
    dest.to_path_buf()
}
