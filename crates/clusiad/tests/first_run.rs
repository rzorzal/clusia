mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use clusia_protocol::{Command, FirstRun, GithubLogin, Harness, HarnessKind, Reply};
use common::{TestDaemon, test_options};

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

async fn daemon(gh: PathBuf, harness_dirs: Vec<PathBuf>) -> TestDaemon {
    let mut o = test_options();
    o.gh_program = gh;
    o.harness_search_paths = harness_dirs;
    TestDaemon::start_with(tempfile::tempdir().unwrap(), o).await
}

async fn status(d: &TestDaemon) -> FirstRun {
    match d.client().await.request(Command::FirstRunStatus).await {
        Ok(Reply::FirstRun(f)) => f,
        other => panic!("expected FirstRun, got {other:?}"),
    }
}

/// A `gh` that prints `json` only for `auth status --hostname <host> --json hosts`.
fn gh_printing(dir: &Path, json: &str) -> PathBuf {
    script(
        dir,
        "gh",
        &format!(
            r#"[ "$1 $2 $3 $5 $6" = "auth status --hostname --json hosts" ] || exit 2
printf '%s' '{json}' | sed "s/HOST/$4/g""#
        ),
    )
}

#[tokio::test]
async fn signed_in_from_gh_json() {
    let tools = tempfile::tempdir().unwrap();
    let json = r#"{"hosts":{"HOST":[{"state":"success","active":false,"host":"HOST","login":"maria","scopes":"gist"},{"state":"success","active":true,"host":"HOST","login":"octo","tokenSource":"keyring","scopes":"read:org, repo","gitProtocol":"https"}]}}"#;
    let d = daemon(gh_printing(tools.path(), json), vec![]).await;
    assert_eq!(
        status(&d).await.github,
        GithubLogin::SignedIn {
            login: "octo".into(),
            scopes: vec!["read:org".into(), "repo".into()]
        }
    );
    // The configured host is the one asked about.
    d.client()
        .await
        .request(Command::SetConfigValue {
            key: "github.host".into(),
            value: "ghe.example.com".into(),
        })
        .await
        .unwrap();
    assert!(matches!(
        status(&d).await.github,
        GithubLogin::SignedIn { ref login, .. } if login == "octo"
    ));
    d.stop().await;
}

#[tokio::test]
async fn signed_out_and_timeout() {
    let tools = tempfile::tempdir().unwrap();
    let cases = [
        (r#"{"hosts":{}}"#, GithubLogin::SignedOut),
        (
            r#"{"hosts":{"HOST":[{"state":"error","active":true,"error":"401 Unauthorized"}]}}"#,
            GithubLogin::Error {
                message: "401 Unauthorized".into(),
            },
        ),
        ("this is not json", GithubLogin::Unknown),
    ];
    for (json, want) in cases {
        let d = daemon(gh_printing(tools.path(), json), vec![]).await;
        assert_eq!(status(&d).await.github, want, "{json}");
        d.stop().await;
    }
    let d = daemon(PathBuf::from("/nonexistent/gh"), vec![]).await;
    assert_eq!(
        status(&d).await.github,
        GithubLogin::Unknown,
        "no gh at all"
    );
    d.stop().await;
}

fn repo(root: &Path, rel: &str) {
    std::fs::create_dir_all(root.join(rel).join(".git")).unwrap();
}

#[tokio::test]
async fn counts_repos_two_levels_deep() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("code");
    repo(&root, "a");
    repo(&root, "a/inner");
    std::fs::create_dir_all(root.join("b")).unwrap();
    std::fs::write(root.join("b/.git"), "gitdir: ../elsewhere").unwrap();
    repo(&root, "org/c");
    repo(&root, "org/d");
    repo(&root, "deep/x/y");
    repo(&root, ".hidden/z");
    repo(&root, "node_modules/p");
    repo(&root, "target/q");
    std::os::unix::fs::symlink(root.join("a"), root.join("link")).unwrap();
    std::fs::write(root.join("notes.txt"), "x").unwrap();
    let missing = dir.path().join("missing");
    let d = daemon(PathBuf::from("/nonexistent/gh"), vec![]).await;
    let roots = format!(
        "[{:?}, {:?}]",
        root.display().to_string(),
        missing.display().to_string()
    );
    let mut c = d.client().await;
    c.request(Command::SetConfigValue {
        key: "repositories.roots".into(),
        value: roots,
    })
    .await
    .unwrap();
    let folders = status(&d).await.folders;
    assert_eq!(folders.len(), 2);
    assert_eq!(folders[0].path, root.display().to_string());
    assert!(folders[0].exists);
    assert_eq!(folders[0].repos, 4, "a, b, org/c and org/d");
    assert_eq!(
        (folders[1].exists, folders[1].repos),
        (false, 0),
        "a missing folder is listed, empty"
    );
    d.stop().await;
}

#[tokio::test]
async fn finds_harnesses_off_path() {
    let dir = tempfile::tempdir().unwrap();
    let (stale, real) = (dir.path().join("stale"), dir.path().join("real"));
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::create_dir_all(&real).unwrap();
    // Present but not executable, and executable but failing: neither counts.
    std::fs::write(stale.join("claude"), "#!/bin/sh\necho 0.0.1\n").unwrap();
    script(&stale, "codex", "exit 1");
    script(&real, "claude", r#"echo "2.1.291 (Claude Code)""#);
    let d = daemon(PathBuf::from("/nonexistent/gh"), vec![stale, real.clone()]).await;
    let found = status(&d).await.harnesses;
    assert_eq!(
        found,
        vec![
            Harness {
                kind: HarnessKind::ClaudeCode,
                path: Some(real.join("claude").display().to_string()),
                version: Some("2.1.291".into())
            },
            Harness {
                kind: HarnessKind::Codex,
                path: None,
                version: None
            }
        ]
    );
    d.stop().await;
}
