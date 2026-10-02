mod common;

use std::path::Path;

use clusia_protocol::{Client, ClientError, Command, ErrorCode, Reply, WorktreeInfo};
use common::git_fixture::{origin_with_pr, sh, user_clone};
use common::{TestDaemon, test_options};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn github_pull(server: &MockServer, head_sha: &str, clone_url: &str) {
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 7, "title": "Fix", "html_url": "https://github.com/acme/widgets/pull/7",
            "user": { "login": "maria" }, "updated_at": "2026-10-01T12:00:00Z",
            "additions": 1, "deletions": 0, "changed_files": 1,
            "base": { "ref": "main", "sha": "aaa", "repo": { "clone_url": clone_url } },
            "head": { "ref": "feature", "sha": head_sha, "repo": null }
        })))
        .mount(server)
        .await;
}

async fn daemon(server: &MockServer) -> TestDaemon {
    let mut o = test_options();
    o.github_api = Some(server.uri());
    o.github_token = Some("tok".into());
    TestDaemon::start_with(tempfile::tempdir().unwrap(), o).await
}

async fn set_roots(c: &mut Client, root: &Path) {
    let value = format!("[\"{}\"]", root.display());
    c.request(Command::SetConfigValue {
        key: "repositories.roots".into(),
        value,
    })
    .await
    .unwrap();
}

fn worktree_of(r: Reply) -> WorktreeInfo {
    match r {
        Reply::Worktree(w) => w,
        other => panic!("expected Worktree, got {other:?}"),
    }
}

#[tokio::test]
async fn prepare_worktree_uses_local_clone_and_leaves_it_untouched() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = origin_with_pr(tmp.path(), 7);
    let roots = tmp.path().join("Repos");
    let clone = user_clone(
        &origin,
        &roots.join("acme/widgets"),
        "https://github.com/acme/widgets.git",
    );
    let snapshot = |dir: &Path| {
        (
            sh(dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
            sh(dir, &["branch", "--list"]),
            sh(dir, &["status", "--porcelain"]),
        )
    };
    let before = snapshot(&clone);

    let server = MockServer::start().await;
    github_pull(
        &server,
        &origin.pr_sha,
        "https://github.com/acme/widgets.git",
    )
    .await;
    let d = daemon(&server).await;
    let mut c = d.client().await;
    set_roots(&mut c, &roots).await;
    let pr: clusia_core::PrRef = "acme/widgets#7".parse().unwrap();
    let w = worktree_of(
        c.request(Command::PrepareWorktree { pr: pr.clone() })
            .await
            .unwrap(),
    );

    assert!(!w.cloned);
    assert_eq!(w.clone, clone.display().to_string());
    assert_eq!(w.head_sha, origin.pr_sha);
    assert_eq!(w.path, d.paths.worktree_for(&pr).display().to_string());
    assert!(Path::new(&w.path).join("feature.txt").exists());
    assert_eq!(snapshot(&clone), before, "the user's clone must not change");
    d.stop().await;
}

#[tokio::test]
async fn prepare_worktree_clones_when_no_local_clone() {
    let tmp = tempfile::tempdir().unwrap();
    let origin = origin_with_pr(tmp.path(), 7);
    let empty_roots = tmp.path().join("Empty");
    std::fs::create_dir_all(&empty_roots).unwrap();

    let server = MockServer::start().await;
    github_pull(&server, &origin.pr_sha, origin.path.to_str().unwrap()).await;
    let d = daemon(&server).await;
    let mut c = d.client().await;
    set_roots(&mut c, &empty_roots).await;
    let w = worktree_of(
        c.request(Command::PrepareWorktree {
            pr: "acme/widgets#7".parse().unwrap(),
        })
        .await
        .unwrap(),
    );

    assert!(w.cloned);
    assert_eq!(
        w.clone,
        d.paths
            .repos_dir()
            .join("acme/widgets")
            .display()
            .to_string()
    );
    assert!(Path::new(&w.path).join("feature.txt").exists());
    d.stop().await;
}

#[tokio::test]
async fn prepare_worktree_reports_git_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    github_pull(&server, "deadbeef", "/nonexistent/origin.git").await;
    let d = daemon(&server).await;
    let mut c = d.client().await;
    set_roots(&mut c, tmp.path()).await;
    match c
        .request(Command::PrepareWorktree {
            pr: "acme/widgets#7".parse().unwrap(),
        })
        .await
    {
        Err(ClientError::Server(e)) => assert_eq!(e.code, ErrorCode::Git),
        other => panic!("expected a git error, got {other:?}"),
    }
    d.stop().await;
}
