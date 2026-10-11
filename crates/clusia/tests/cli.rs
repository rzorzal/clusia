//! End-to-end: the real `clusia` binary driving the real `clusiad` binary.
//! Requires `cargo test --workspace` (which builds `clusiad` next to `clusia`).

use std::path::PathBuf;
use std::process::{Command, Output};

fn clusiad_bin() -> PathBuf {
    // target/<profile>/deps/cli-<hash> → target/<profile>/clusiad
    let exe = std::env::current_exe().unwrap();
    let bin = exe.parent().unwrap().parent().unwrap().join("clusiad");
    assert!(
        bin.exists(),
        "clusiad not built at {}; run `cargo test --workspace`",
        bin.display()
    );
    bin
}

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn clusia(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_clusia"))
            .arg("--home")
            .arg(self.dir.path())
            .args(args)
            .env("CLUSIA_DAEMON_BIN", clusiad_bin())
            .env("CLUSIA_GITHUB_API", "http://127.0.0.1:9")
            .env("CLUSIA_GH_BIN", "/nonexistent/gh")
            .env("CLUSIA_SECRET_STORE", "memory")
            .env("CLUSIA_TRAY_BIN", "none")
            .env("CLUSIA_CLAUDE_BIN", "/nonexistent/claude")
            .env_remove("CLUSIA_GITHUB_TOKEN")
            .output()
            .unwrap()
    }
}

/// The pids of every clusiad serving `home`, whichever client started it.
fn daemons_of(home: &std::path::Path) -> Vec<String> {
    let name = home.file_name().unwrap().to_string_lossy();
    let out = Command::new("pgrep")
        .args(["-f", &format!("clusiad --home .*{name}")])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(String::from)
        .collect()
}

impl Drop for Home {
    /// Stops the daemon the test started; one that does not stop in time is killed and waited
    /// for, so no test leaves a daemon running on a deleted home. One that outlives the kill
    /// too (a zombie, an unkillable process) fails the test instead of hanging it.
    fn drop(&mut self) {
        let _ = self.clusia(&["daemon", "stop"]);
        let start = std::time::Instant::now();
        let kill_after = std::time::Duration::from_secs(5);
        let give_up = std::time::Duration::from_secs(10);
        loop {
            let left = daemons_of(self.dir.path());
            if left.is_empty() {
                return;
            }
            if start.elapsed() > give_up {
                let message = format!("daemons still running after kill -9: {left:?}");
                // A panic while already unwinding would abort the whole test binary.
                if std::thread::panicking() {
                    eprintln!("{message}");
                    return;
                }
                panic!("{message}");
            }
            if start.elapsed() > kill_after {
                for pid in &left {
                    let _ = Command::new("kill").args(["-9", pid]).status();
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
}

#[test]
fn a_dropped_home_leaves_no_daemon_behind() {
    let h = Home::new();
    assert!(h.clusia(&["daemon", "start"]).status.success());
    let dir = h.dir.path().to_path_buf();
    assert_eq!(daemons_of(&dir).len(), 1, "the started daemon is found");
    drop(h);
    assert!(daemons_of(&dir).is_empty());
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).trim().to_string()
}

#[test]
fn status_without_daemon_exits_3_with_hint() {
    let h = Home::new();
    let o = h.clusia(&["daemon", "status"]);
    assert_eq!(o.status.code(), Some(3));
    assert!(stderr(&o).contains("clusia daemon start"), "{}", stderr(&o));
}

#[test]
fn json_errors_are_machine_readable() {
    let h = Home::new();
    let o = h.clusia(&["--json", "daemon", "status"]);
    let v: serde_json::Value = serde_json::from_str(&stderr(&o)).unwrap();
    assert_eq!(v["error"]["kind"], "not_running");
}

#[test]
fn full_lifecycle() {
    let h = Home::new();
    let o = h.clusia(&["daemon", "start"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("started"), "{}", stdout(&o));

    let o = h.clusia(&["--json", "daemon", "status"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert!(v["pid"].as_u64().unwrap() > 0);

    let o = h.clusia(&["config", "set", "github.poll_interval_secs", "120"]);
    assert_eq!(
        stdout(&o),
        "github.poll_interval_secs = 120",
        "{}",
        stderr(&o)
    );
    assert_eq!(
        stdout(&h.clusia(&["config", "get", "github.poll_interval_secs"])),
        "120"
    );
    assert!(stdout(&h.clusia(&["config", "show"])).contains("poll_interval_secs = 120"));

    let o = h.clusia(&["daemon", "stop"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o), "Clúsia daemon stopped");
    assert_eq!(h.clusia(&["daemon", "status"]).status.code(), Some(3));
}

#[test]
fn invalid_config_value_exits_1() {
    let h = Home::new();
    assert!(h.clusia(&["daemon", "start"]).status.success());
    let o = h.clusia(&["config", "set", "github.poll_interval_secs", "abc"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("github.poll_interval_secs"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn start_twice_reports_already_running() {
    let h = Home::new();
    assert!(h.clusia(&["daemon", "start"]).status.success());
    let o = h.clusia(&["daemon", "start"]);
    assert!(o.status.success());
    assert!(stdout(&o).contains("already running"), "{}", stdout(&o));
}

#[test]
fn commands_autostart_the_daemon() {
    let h = Home::new();
    let o = h.clusia(&["config", "get", "github.host"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o), "github.com");
    assert!(
        stderr(&o).contains("started the Clúsia daemon"),
        "{}",
        stderr(&o)
    );
    assert!(h.clusia(&["daemon", "status"]).status.success());
}

impl Home {
    /// Like `clusia`, but the auto-started daemon talks to `api` with `token`.
    fn clusia_github(
        &self,
        api: &str,
        token: Option<&str>,
        args: &[&str],
        stdin: Option<&str>,
    ) -> Output {
        use std::io::Write;
        let mut child = self.spawn_github(api, token, args);
        {
            let mut input = child.stdin.take().unwrap();
            if let Some(s) = stdin {
                input.write_all(s.as_bytes()).unwrap();
            }
        }
        child.wait_with_output().unwrap()
    }

    /// Starts `clusia args` against `api` with piped stdio and returns it still running.
    fn spawn_github(&self, api: &str, token: Option<&str>, args: &[&str]) -> std::process::Child {
        self.spawn_with_stdin(api, token, args, std::process::Stdio::piped())
    }

    /// Like `spawn_github`, with `stdin` as the process's standard input.
    fn spawn_with_stdin(
        &self,
        api: &str,
        token: Option<&str>,
        args: &[&str],
        stdin: std::process::Stdio,
    ) -> std::process::Child {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_clusia"));
        cmd.arg("--home")
            .arg(self.dir.path())
            .args(args)
            .env("CLUSIA_DAEMON_BIN", clusiad_bin())
            .env("CLUSIA_GITHUB_API", api)
            .env("CLUSIA_GH_BIN", "/nonexistent/gh")
            .env("CLUSIA_SECRET_STORE", "memory")
            .env("CLUSIA_TRAY_BIN", "none")
            .env("CLUSIA_CLAUDE_BIN", "/nonexistent/claude")
            .env_remove("CLUSIA_GITHUB_TOKEN")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(stdin);
        if let Some(t) = token {
            cmd.env("CLUSIA_GITHUB_TOKEN", t);
        }
        cmd.spawn().unwrap()
    }
}

fn mock_github() -> (tokio::runtime::Runtime, wiremock::MockServer) {
    mock_github_with_delay(std::time::Duration::ZERO)
}

/// Every search response is delayed by `delay`, so a slow first sync is observable.
fn mock_github_with_delay(
    delay: std::time::Duration,
) -> (tokio::runtime::Runtime, wiremock::MockServer) {
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, ResponseTemplate};
    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = rt.block_on(async {
        let server = wiremock::MockServer::start().await;
        let issue = |n: u64, repo: &str, draft: bool| json!({
            "number": n, "title": format!("PR {n}"), "html_url": format!("https://github.com/{repo}/pull/{n}"),
            "user": { "login": "maria" }, "draft": draft, "updated_at": "2026-10-01T12:00:00Z",
            "comments": 0, "repository_url": format!("https://api.github.com/repos/{repo}")
        });
        Mock::given(method("GET")).and(path("/search/issues"))
            .and(query_param("q", "is:pr is:open archived:false review-requested:@me"))
            .respond_with(ResponseTemplate::new(200).set_delay(delay).set_body_json(json!({ "items": [issue(7, "acme/widgets", false)] })))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/search/issues"))
            .and(query_param("q", "is:pr is:open archived:false author:@me"))
            .respond_with(ResponseTemplate::new(200).set_delay(delay).set_body_json(json!({ "items": [issue(3, "me/tool", true)] })))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/user"))
            .respond_with(ResponseTemplate::new(200).insert_header("x-oauth-scopes", "repo").set_body_json(json!({ "login": "octo" })))
            .mount(&server).await;
        server
    });
    (rt, server)
}

#[test]
fn prs_lists_both_sections() {
    let (_rt, gh) = mock_github();
    let h = Home::new();
    assert!(
        h.clusia_github(&gh.uri(), Some("tok"), &["sync"], None)
            .status
            .success()
    );
    let o = h.clusia_github(&gh.uri(), Some("tok"), &["prs"], None);
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("Assigned to me (1)"), "{out}");
    assert!(out.contains("acme/widgets#7  PR 7  @maria"), "{out}");
    assert!(out.contains("Mine (1)"), "{out}");
    assert!(out.contains("me/tool#3  PR 3  @maria [draft]"), "{out}");
}

#[test]
fn prs_lists_on_a_cold_start() {
    let (_rt, gh) = mock_github_with_delay(std::time::Duration::from_millis(500));
    let h = Home::new();
    let o = h.clusia_github(&gh.uri(), Some("tok"), &["prs"], None);
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains("Assigned to me (1)"), "{out}");
    assert!(out.contains("acme/widgets#7"), "{out}");
}

#[test]
fn prs_json_respects_filters() {
    let (_rt, gh) = mock_github();
    let h = Home::new();
    assert!(
        h.clusia_github(&gh.uri(), Some("tok"), &["sync"], None)
            .status
            .success()
    );
    let o = h.clusia_github(&gh.uri(), Some("tok"), &["--json", "prs", "--mine"], None);
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert!(v.get("assigned").is_none());
    assert_eq!(v["mine"][0]["pr"], "me/tool#3");
    assert_eq!(v["sync"]["state"], "online");
}

#[test]
fn prs_without_token_explains_sync_state() {
    let (_rt, gh) = mock_github();
    let h = Home::new();
    let o = h.clusia_github(&gh.uri(), None, &["prs"], None);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("(none)"));
    let o = h.clusia_github(&gh.uri(), None, &["sync"], None);
    assert!(stdout(&o).contains("sync: unauthorized"), "{}", stdout(&o));
}

#[test]
fn auth_login_reads_piped_stdin_and_status_never_prints_token() {
    let (_rt, gh) = mock_github();
    let h = Home::new();
    let o = h.clusia_github(
        &gh.uri(),
        None,
        &["auth", "login"],
        Some("ghp_secret_value\n"),
    );
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o), "Token saved to the Keychain");
    let o = h.clusia_github(&gh.uri(), None, &["auth", "status"], None);
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    assert!(
        out.contains("Logged in to github.com as @octo (token from pat)"),
        "{out}"
    );
    assert!(out.contains("scopes: repo"));
    assert!(!out.contains("ghp_secret_value") && !stderr(&o).contains("ghp_secret_value"));
    let o = h.clusia_github(&gh.uri(), None, &["auth", "logout"], None);
    assert_eq!(stdout(&o), "Token removed");
    assert_eq!(
        h.clusia_github(&gh.uri(), None, &["auth", "status"], None)
            .status
            .code(),
        Some(1)
    );
}

#[test]
fn auth_login_with_empty_stdin_fails() {
    let (_rt, gh) = mock_github();
    let h = Home::new();
    let o = h.clusia_github(&gh.uri(), None, &["auth", "login"], Some(""));
    assert_eq!(o.status.code(), Some(1));
}

#[test]
fn worktree_rejects_invalid_pr() {
    let h = Home::new();
    let o = h.clusia(&["worktree", "not-a-pr"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("owner/repo#number"), "{}", stderr(&o));
}

#[test]
fn too_long_home_fails_before_running() {
    let h = Home::new();
    let base = h.dir.path().display().to_string();
    let long = format!(
        "{base}/{}",
        "h".repeat(110usize.saturating_sub(base.len() + 1))
    );
    assert!(long.len() >= 110);
    for args in [
        &["daemon", "status"][..],
        &["config", "get", "github.host"][..],
    ] {
        let o = Command::new(env!("CARGO_BIN_EXE_clusia"))
            .arg("--home")
            .arg(&long)
            .args(args)
            .env("CLUSIA_DAEMON_BIN", clusiad_bin())
            .env("CLUSIA_TRAY_BIN", "none")
            .env("CLUSIA_CLAUDE_BIN", "/nonexistent/claude")
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
        assert!(stderr(&o).contains("at most 103"), "{}", stderr(&o));
    }
}

mod review_flow {
    use super::*;
    use clusia_harness::testkit::{FakeClaude, Script, Turn, finding_block, pass_block};
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use wiremock::matchers::{body_partial_json, body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Answers GraphQL requests whose body contains `contains` (and whose `input` includes
    /// `input`, when given) with `data`.
    fn graphql(contains: &str, input: Option<serde_json::Value>, data: serde_json::Value) -> Mock {
        let mut mock = Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains(contains));
        if let Some(input) = input {
            mock = mock.and(body_partial_json(
                json!({ "variables": { "input": input } }),
            ));
        }
        mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": data })))
    }

    fn git(dir: &Path, args: &[&str]) -> String {
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

    /// origin.git with main, plus PR #7 whose feature.txt is "one\ntwo\nthree\n". Returns (origin, base, head).
    fn origin(root: &Path) -> (PathBuf, String, String) {
        let origin = root.join("origin.git");
        let seed = root.join("seed");
        git(
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
        git(root, &["init", "-q", "-b", "main", seed.to_str().unwrap()]);
        std::fs::write(seed.join("README.md"), "hi\n").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-q", "-m", "init"]);
        git(
            &seed,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&seed, &["push", "-q", "origin", "main"]);
        let base = git(&seed, &["rev-parse", "HEAD"]);
        git(&seed, &["checkout", "-q", "-b", "feature"]);
        let head = advance(root, "one\ntwo\nthree\n");
        (origin, base, head)
    }

    fn advance(root: &Path, content: &str) -> String {
        let seed = root.join("seed");
        std::fs::write(seed.join("feature.txt"), content).unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-q", "-m", "change"]);
        git(
            &seed,
            &["push", "-q", "-f", "origin", "feature:refs/pull/7/head"],
        );
        git(&seed, &["rev-parse", "HEAD"])
    }

    async fn mount(server: &MockServer, head: &str, base: &str, clone_url: &str) {
        let ok = |body: serde_json::Value| ResponseTemplate::new(200).set_body_json(body);
        let p = "/repos/acme/widgets";
        Mock::given(method("GET")).and(path(format!("{p}/pulls/7"))).respond_with(ok(json!({
            "number": 7, "title": "Add feature", "html_url": "https://github.com/acme/widgets/pull/7",
            "user": { "login": "maria" }, "updated_at": "2026-10-01T12:00:00Z",
            "additions": 3, "deletions": 0, "changed_files": 1,
            "base": { "ref": "main", "sha": base, "repo": { "clone_url": clone_url } },
            "head": { "ref": "feature", "sha": head, "repo": null }
        }))).mount(server).await;
        Mock::given(method("GET")).and(path(format!("{p}/pulls/7/files"))).respond_with(ok(json!([
            { "filename": "feature.txt", "status": "added", "additions": 3, "deletions": 0, "patch": "@@ -0,0 +1,4 @@\n+zero\n+one\n+two\n+three" }
        ]))).mount(server).await;
        for list in [
            "pulls/7/comments",
            "issues/7/comments",
            "pulls/7/reviews",
            "pulls/7/commits",
        ] {
            Mock::given(method("GET"))
                .and(path(format!("{p}/{list}")))
                .respond_with(ok(json!([])))
                .mount(server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path(format!("{p}/commits/{head}/check-runs")))
            .respond_with(ok(json!({ "total_count": 0, "check_runs": [] })))
            .mount(server)
            .await;
        // `sync` polls the search API; an empty inbox is enough to go online.
        Mock::given(method("GET"))
            .and(path("/search/issues"))
            .respond_with(ok(json!({ "items": [] })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(ok(json!({ "login": "me" })))
            .mount(server)
            .await;
        graphql(
            "headRefOid",
            None,
            json!({ "repository": { "pullRequest": {
                "id": "PR_7", "headRefOid": head, "reviews": { "nodes": [] }
            } } }),
        )
        .mount(server)
        .await;
        graphql(
            "reviewThreads",
            None,
            json!({ "repository": { "pullRequest": { "reviewThreads": {
                "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": []
            } } } }),
        )
        .mount(server)
        .await;
    }

    #[test]
    fn open_failure_shows_the_failed_step() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, base, head) = origin(tmp.path());
        let missing = tmp.path().join("no-such-origin.git");
        let server = rt.block_on(async {
            let s = MockServer::start().await;
            mount(&s, &head, &base, missing.to_str().unwrap()).await;
            s
        });
        let h = Home::new();
        let roots = tmp.path().join("roots");
        std::fs::create_dir_all(&roots).unwrap();
        let api = server.uri();
        let run = |args: &[&str]| h.clusia_github(&api, Some("tok"), args, None);
        let o = run(&[
            "config",
            "set",
            "repositories.roots",
            &format!("[\"{}\"]", roots.display()),
        ]);
        assert!(o.status.success(), "{}", stderr(&o));

        let o = run(&["open", "acme/widgets#7"]);
        assert!(!o.status.success());
        assert!(stderr(&o).contains("✗ repo"), "stderr={:?}", stderr(&o));
    }

    #[test]
    fn open_comment_relocate_publish() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let (origin, base, head) = origin(tmp.path());
        let clone_url = origin.to_str().unwrap().to_string();
        let server = rt.block_on(async {
            let s = MockServer::start().await;
            mount(&s, &head, &base, &clone_url).await;
            s
        });
        let h = Home::new();
        let roots = tmp.path().join("roots");
        std::fs::create_dir_all(&roots).unwrap();
        let api = server.uri();
        let run = |args: &[&str]| h.clusia_github(&api, Some("tok"), args, None);

        let o = run(&[
            "config",
            "set",
            "repositories.roots",
            &format!("[\"{}\"]", roots.display()),
        ]);
        assert!(o.status.success(), "{}", stderr(&o));

        let o = run(&["open", "acme/widgets#7"]);
        assert!(o.status.success(), "{}", stderr(&o));
        assert!(
            stdout(&o).contains("acme/widgets#7 · Add feature (@maria)"),
            "{}",
            stdout(&o)
        );
        assert!(stdout(&o).contains("worktree: "), "{}", stdout(&o));
        assert!(stderr(&o).contains("✓ branch"), "stderr={:?}", stderr(&o));

        let o = run(&[
            "review",
            "comment",
            "acme/widgets#7",
            "feature.txt:2",
            "rename this",
        ]);
        assert_eq!(stdout(&o), "i1 added", "{}", stderr(&o));
        assert_eq!(
            run(&["review", "comment", "acme/widgets#7", "feature.txt:40", "x"])
                .status
                .code(),
            Some(1)
        );
        assert!(run(&["review", "close", "acme/widgets#7"]).status.success());

        // A new commit lands above the commented line.
        let new_head = advance(tmp.path(), "zero\none\ntwo\nthree\n");
        rt.block_on(async {
            server.reset().await;
            mount(&server, &new_head, &base, &clone_url).await;
        });
        assert!(run(&["sync"]).status.success());
        let status = stdout(&run(&["review", "status", "acme/widgets#7"]));
        assert!(status.contains("outdated"), "{status}");
        assert!(
            status.contains("feature.txt:3 (right)  rename this [moved from feature.txt:2]"),
            "{status}"
        );

        assert!(run(&["open", "acme/widgets#7"]).status.success());
        rt.block_on(async {
            let url = "https://github.com/acme/widgets/pull/7#pullrequestreview-1";
            graphql(
                "addPullRequestReview(",
                Some(json!({
                    "commitOID": new_head,
                    "threads": [{ "path": "feature.txt", "line": 3, "side": "RIGHT", "body": "rename this" }]
                })),
                json!({ "addPullRequestReview": { "pullRequestReview": {
                    "id": "PRR_1", "databaseId": 1, "url": url, "state": "PENDING"
                } } }),
            )
            .expect(1)
            .mount(&server)
            .await;
            graphql(
                "submitPullRequestReview",
                Some(json!({
                    "pullRequestReviewId": "PRR_1", "event": "REQUEST_CHANGES", "body": "Please rename."
                })),
                json!({ "submitPullRequestReview": { "pullRequestReview": {
                    "id": "PRR_1", "databaseId": 1, "url": url, "state": "CHANGES_REQUESTED"
                } } }),
            )
            .expect(1)
            .mount(&server)
            .await;
        });
        let o = run(&[
            "review",
            "publish",
            "acme/widgets#7",
            "--verdict",
            "request-changes",
            "--summary",
            "Please rename.",
        ]);
        assert!(o.status.success(), "{}", stderr(&o));
        assert_eq!(
            stdout(&o),
            "Published review: https://github.com/acme/widgets/pull/7#pullrequestreview-1"
        );
        assert_eq!(stdout(&run(&["review", "status"])), "No saved reviews");
        assert!(stdout(&run(&["activity"])).contains("Reviews published: 1 this week · 1 total"));
    }

    #[test]
    fn bad_location_fails_before_the_daemon() {
        let h = Home::new();
        let o = h.clusia(&["review", "comment", "acme/widgets#7", "nocolon", "x"]);
        assert_eq!(o.status.code(), Some(1));
        assert!(stderr(&o).contains("path:line"), "{}", stderr(&o));
        assert_eq!(
            h.clusia(&["daemon", "status"]).status.code(),
            Some(3),
            "no daemon was started"
        );
    }

    /// The fake `claude` answer: some text and one valid suggestion block.
    const ANSWER: &str = "The rename is needed.\n\n```clusia-suggestion\n{\"file\":\"feature.txt\",\"line\":2,\"body\":\"Rename this variable.\"}\n```\n";

    /// acme/widgets#7 on a mock GitHub and a local origin, a daemon that waits for the first
    /// question (no summary turn), and a fake `claude` running `script`.
    struct AgentWorld {
        home: Home,
        _server: MockServer,
        _rt: tokio::runtime::Runtime,
        tmp: tempfile::TempDir,
        api: String,
    }

    impl AgentWorld {
        fn new(script: Script) -> Self {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let tmp = tempfile::tempdir().unwrap();
            let (origin, base, head) = origin(tmp.path());
            let clone_url = origin.to_str().unwrap().to_string();
            let server = rt.block_on(async {
                let s = MockServer::start().await;
                mount(&s, &head, &base, &clone_url).await;
                s
            });
            let roots = tmp.path().join("roots");
            std::fs::create_dir_all(&roots).unwrap();
            let fake = FakeClaude::install(&tmp.path().join("fake"), script);
            let world = Self {
                home: Home::new(),
                api: server.uri(),
                _server: server,
                _rt: rt,
                tmp,
            };
            for (key, value) in [
                ("repositories.roots", format!("[\"{}\"]", roots.display())),
                ("harness.on_open", "wait".to_string()),
                ("harness.check_security", "false".to_string()),
                ("harness.audit", "false".to_string()),
                ("harness.program", fake.display().to_string()),
            ] {
                let o = world.run(&["config", "set", key, &value]);
                assert!(o.status.success(), "{key}: {}", stderr(&o));
            }
            world
        }

        fn run(&self, args: &[&str]) -> Output {
            self.home.clusia_github(&self.api, Some("tok"), args, None)
        }

        /// Starts `clusia args` the way `run` does and leaves it running.
        fn spawn(&self, args: &[&str]) -> std::process::Child {
            self.home.spawn_github(&self.api, Some("tok"), args)
        }

        fn fake_dir(&self) -> PathBuf {
            self.tmp.path().join("fake")
        }

        /// Every GitHub read now takes `secs` and then fails.
        fn slow_github(&self, secs: u64) {
            self._rt.block_on(
                Mock::given(method("GET"))
                    .respond_with(
                        ResponseTemplate::new(500).set_delay(std::time::Duration::from_secs(secs)),
                    )
                    .with_priority(1)
                    .mount(&self._server),
            );
        }
    }

    #[test]
    fn ask_streams_the_answer_and_lists_the_suggestions() {
        let w = AgentWorld::new(Script::one(Turn::answer(ANSWER)));
        let o = w.run(&["ask", "acme/widgets#7", "Is", "the", "rename", "needed?"]);
        assert!(o.status.success(), "{}", stderr(&o));
        let out = stdout(&o);
        assert!(out.contains("The rename is needed."), "{out}");
        assert!(out.contains("Suggested comments:"), "{out}");
        assert!(
            out.contains("feature.txt:2  Rename this variable."),
            "{out}"
        );

        let calls = FakeClaude::calls(&w.fake_dir());
        assert_eq!(calls.len(), 1, "one turn, one process");
        assert!(calls[0].argv.iter().any(|a| a == "Is the rename needed?"));
        assert!(calls[0].argv.iter().any(|a| a == "--session-id"));
        assert!(
            calls[0].cwd.to_string_lossy().contains("worktrees"),
            "{:?}",
            calls[0].cwd
        );

        let log = stdout(&w.run(&["agent", "log", "acme/widgets#7"]));
        assert!(log.contains("you: Is the rename needed?"), "{log}");
        assert!(log.contains("The rename is needed."), "{log}");
        assert!(
            log.contains("suggestion feature.txt:2  Rename this variable."),
            "{log}"
        );
    }

    #[test]
    fn ask_json_prints_one_event_per_line() {
        let w = AgentWorld::new(Script::one(Turn::answer(ANSWER)));
        let o = w.run(&["--json", "ask", "acme/widgets#7", "Is it needed?"]);
        assert!(o.status.success(), "{}", stderr(&o));
        let events: Vec<serde_json::Value> = stdout(&o)
            .lines()
            .map(|l| serde_json::from_str(l).expect(l))
            .collect();
        let has = |name: &str| events.iter().any(|e| e.get(name).is_some());
        assert!(has("agent_chunk") && has("agent_suggestion"), "{events:?}");
        assert!(
            events.last().unwrap().get("agent_done").is_some(),
            "{events:?}"
        );
    }

    #[test]
    fn stop_ends_a_turn_that_hangs() {
        let w = AgentWorld::new(Script::one(Turn::hanging()));
        std::thread::scope(|s| {
            let asking = s.spawn(|| w.run(&["ask", "acme/widgets#7", "wait for me"]));
            let start = std::time::Instant::now();
            while FakeClaude::calls(&w.fake_dir()).is_empty() {
                assert!(
                    start.elapsed().as_secs() < 30,
                    "the fake claude never started"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let o = w.run(&["agent", "stop", "acme/widgets#7"]);
            assert!(o.status.success(), "{}", stderr(&o));
            assert_eq!(stdout(&o), "Asked the agent to stop");
            let asked = asking.join().unwrap();
            assert!(!asked.status.success());
            assert!(
                stderr(&asked).contains("Stopped by you"),
                "{}",
                stderr(&asked)
            );
        });
    }

    #[test]
    fn ctrl_c_on_ask_only_detaches_and_says_how_to_stop() {
        let w = AgentWorld::new(Script::one(Turn::hanging()));
        let asking = w.spawn(&["ask", "acme/widgets#7", "wait for me"]);
        let start = std::time::Instant::now();
        while FakeClaude::calls(&w.fake_dir()).is_empty() {
            assert!(
                start.elapsed().as_secs() < 30,
                "the fake claude never started"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // SAFETY: signals a child this test spawned and still owns.
        assert_eq!(unsafe { libc::kill(asking.id() as i32, libc::SIGINT) }, 0);
        let asked = asking.wait_with_output().unwrap();
        let hint = stderr(&asked);
        assert_eq!(
            hint.matches("clusia agent stop acme/widgets#7").count(),
            1,
            "{hint}"
        );
        assert!(hint.contains("keeps working"), "{hint}");
        assert_eq!(asked.status.code(), Some(130), "{hint}");
        // The turn lives in the daemon: it is still running until it is stopped.
        let o = w.run(&["agent", "stop", "acme/widgets#7"]);
        assert!(o.status.success(), "{}", stderr(&o));
    }

    #[test]
    fn ctrl_c_while_the_review_opens_says_nothing_was_asked() {
        let w = AgentWorld::new(Script::one(Turn::hanging()));
        w.slow_github(8);
        let asking = w.spawn(&["ask", "acme/widgets#7", "wait for me"]);
        std::thread::sleep(std::time::Duration::from_millis(1500));
        // SAFETY: signals a child this test spawned and still owns.
        assert_eq!(unsafe { libc::kill(asking.id() as i32, libc::SIGINT) }, 0);
        let asked = asking.wait_with_output().unwrap();
        let said = stderr(&asked);
        assert_eq!(asked.status.code(), Some(130), "{said}");
        assert!(said.contains("nothing was asked"), "{said}");
        assert!(FakeClaude::calls(&w.fake_dir()).is_empty());
    }

    /// What the fake `claude` answers to a check: two Security findings (the second with a
    /// control character in its title), one Audit finding and one pass. Each run keeps the blocks
    /// of its own kind and counts the others as unreadable.
    fn check_answer() -> Turn {
        Turn::answer_blocks(
            "Found.",
            &[
                finding_block(json!({
                    "area": "security", "severity": "high", "title": "Token in the log",
                    "file": "feature.txt", "line": 2, "body": "b"
                })),
                finding_block(json!({
                    "area": "security", "severity": "low", "title": "Bad\u{1b}[2K title",
                    "file": "feature.txt", "line": 3, "body": "b"
                })),
                finding_block(json!({
                    "area": "correctness", "title": "Off by one",
                    "file": "feature.txt", "line": 3, "body": "b"
                })),
                pass_block(json!({
                    "area": "concurrency", "text": "No shared state",
                    "where": "checked 2 call sites"
                })),
            ],
        )
    }

    fn has_line(text: &str, line: &str) -> bool {
        text.lines().any(|l| l == line)
    }

    #[test]
    fn findings_are_printed_filtered() {
        let w = AgentWorld::new(Script::one(check_answer()));
        let o = w.run(&["check", "acme/widgets#7"]);
        assert!(o.status.success(), "{}", stderr(&o));
        let out = stdout(&o);
        assert!(has_line(&out, "Security"), "{out}");
        assert!(
            has_line(&out, "HIGH  feature.txt:2  Token in the log"),
            "{out}"
        );
        assert!(
            has_line(&out, "LOW  feature.txt:3  Bad\\u{1b}[2K title"),
            "a control character is shown as an escape: {out}"
        );
        assert!(!out.contains('\u{1b}'), "{out:?}");
        assert!(has_line(&out, "Audit · Correctness"), "{out}");
        assert!(has_line(&out, "feature.txt:3  Off by one"), "{out}");
        assert!(has_line(&out, "Audit · Concurrency"), "{out}");
        assert!(has_line(&out, "No findings in this area."), "{out}");
        assert!(
            has_line(&out, "ok  No shared state  (checked 2 call sites)"),
            "{out}"
        );
        assert_eq!(
            out.lines().filter(|l| *l == "Not checked").count(),
            4,
            "an area the agent wrote nothing about is never ok: {out}"
        );
        let progress = stderr(&o);
        assert!(progress.contains("Security: done"), "{progress}");
        assert!(progress.contains("Audit: done"), "{progress}");

        let calls = FakeClaude::calls(&w.fake_dir());
        assert_eq!(calls.len(), 2, "one process for each kind");
        assert!(
            calls
                .iter()
                .all(|c| c.argv.iter().any(|a| a == "--session-id")),
            "a review with no chat has no session to fork"
        );
    }

    #[test]
    fn check_with_a_flag_runs_only_that_kind() {
        let w = AgentWorld::new(Script::one(check_answer()));
        let o = w.run(&["check", "acme/widgets#7", "--security"]);
        assert!(o.status.success(), "{}", stderr(&o));
        let out = stdout(&o);
        assert!(
            has_line(&out, "HIGH  feature.txt:2  Token in the log"),
            "{out}"
        );
        assert!(!out.contains("Audit"), "{out}");
        assert_eq!(FakeClaude::calls(&w.fake_dir()).len(), 1);
    }

    #[test]
    fn check_json_prints_the_results_once() {
        let w = AgentWorld::new(Script::one(check_answer()));
        let o = w.run(&["--json", "check", "acme/widgets#7", "--audit"]);
        assert!(o.status.success(), "{}", stderr(&o));
        let value: serde_json::Value = serde_json::from_str(&stdout(&o)).expect("one JSON value");
        let results = value["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["kind"], "audit");
        assert_eq!(results[0]["findings"][0]["title"], "Off by one");
        assert_eq!(value["dismissed"], json!([]));
        assert_eq!(value["accepted"], json!([]));
        assert!(
            stderr(&o).is_empty(),
            "no progress with --json: {}",
            stderr(&o)
        );
    }

    #[test]
    fn a_failed_check_exits_1_and_says_so() {
        let w = AgentWorld::new(Script::one(Turn::lines(&[]).exit(1)));
        let o = w.run(&["check", "acme/widgets#7", "--security"]);
        assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
        assert!(
            stderr(&o).contains("Security check failed"),
            "{}",
            stderr(&o)
        );
    }

    #[test]
    fn check_refuses_both_flags_and_a_bad_pull_request_before_the_daemon() {
        let h = Home::new();
        let both = h.clusia(&["check", "acme/widgets#7", "--security", "--audit"]);
        assert_eq!(both.status.code(), Some(2), "{}", stderr(&both));
        let bad = h.clusia(&["check", "nocolon"]);
        assert_eq!(bad.status.code(), Some(1), "{}", stderr(&bad));
        assert_eq!(
            h.clusia(&["daemon", "status"]).status.code(),
            Some(3),
            "no daemon was started"
        );
    }

    #[test]
    fn ctrl_c_on_check_stops_the_checks_it_started() {
        let w = AgentWorld::new(Script::one(Turn::hanging()));
        let checking = w.spawn(&["check", "acme/widgets#7", "--security"]);
        let start = std::time::Instant::now();
        let call = loop {
            if let Some(call) = FakeClaude::calls(&w.fake_dir()).into_iter().next() {
                break call;
            }
            assert!(
                start.elapsed().as_secs() < 30,
                "the fake claude never started"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        // SAFETY: signals a child this test spawned and still owns.
        assert_eq!(unsafe { libc::kill(checking.id() as i32, libc::SIGINT) }, 0);
        let checked = checking.wait_with_output().unwrap();
        assert_eq!(checked.status.code(), Some(130), "{}", stderr(&checked));
        assert!(
            stderr(&checked).contains("Stopped the checks"),
            "{}",
            stderr(&checked)
        );
        let give_up = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while FakeClaude::is_running(call.pid) {
            assert!(
                std::time::Instant::now() < give_up,
                "the check's process outlived the command"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[test]
    fn an_empty_question_fails_before_the_daemon() {
        let h = Home::new();
        let o = h.clusia(&["ask", "acme/widgets#7", " "]);
        assert_eq!(o.status.code(), Some(1));
        assert!(stderr(&o).contains("question"), "{}", stderr(&o));
        assert_eq!(h.clusia(&["daemon", "status"]).status.code(), Some(3));
    }

    /// The command the fake `claude` asks permission for, on turn 1 of a question.
    fn asks_to_run_the_tests() -> Script {
        Script::one(
            Turn::answer("Done.")
                .ask_permission("Bash", json!({"command": "cargo test -p clusia-core"})),
        )
    }

    /// A pseudo-terminal: the master end the test types on, and the slave end the process
    /// reads as its terminal.
    fn pty() -> (std::fs::File, std::process::Stdio) {
        use std::os::fd::{FromRawFd, OwnedFd};
        let (mut master, mut slave) = (0, 0);
        // SAFETY: `openpty` fills the two descriptors; each is wrapped exactly once below.
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0, "no pseudo-terminal");
        // SAFETY: both descriptors are open and owned by nobody else.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        (
            std::fs::File::from(master),
            std::process::Stdio::from(slave),
        )
    }

    /// Collects what `reader` produces, as it comes (a prompt has no line end to wait for).
    fn collect(
        reader: impl std::io::Read + Send + 'static,
    ) -> std::sync::Arc<std::sync::Mutex<String>> {
        collect_joinable(reader).0
    }

    /// Like `collect`, with the thread to join once the pipe is closed.
    fn collect_joinable(
        mut reader: impl std::io::Read + Send + 'static,
    ) -> (
        std::sync::Arc<std::sync::Mutex<String>>,
        std::thread::JoinHandle<()>,
    ) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = seen.clone();
        let reading = std::thread::spawn(move || {
            let mut buffer = [0u8; 512];
            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 {
                    return;
                }
                sink.lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&buffer[..n]));
            }
        });
        (seen, reading)
    }

    /// A line typed at once after a question appears is ignored on purpose: wait it out, as a
    /// reader who has read the question would.
    fn let_the_question_settle() {
        std::thread::sleep(std::time::Duration::from_millis(400));
    }

    fn wait_until(seen: &std::sync::Mutex<String>, needle: &str) {
        let start = std::time::Instant::now();
        while !seen.lock().unwrap().contains(needle) {
            assert!(
                start.elapsed().as_secs() < 30,
                "never saw {needle:?}; got {:?}",
                seen.lock().unwrap()
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Runs `clusia ask` on a terminal, types `typed` once the question is on the screen, and
    /// returns (standard output, standard error) when it ends.
    fn ask_on_a_terminal(w: &AgentWorld, typed: &str) -> (String, String) {
        use std::io::Write;
        let (mut master, slave) = pty();
        let mut child = w.home.spawn_with_stdin(
            &w.api,
            Some("tok"),
            &["ask", "acme/widgets#7", "run the tests"],
            slave,
        );
        let (out, out_thread) = collect_joinable(child.stdout.take().unwrap());
        let (err, err_thread) = collect_joinable(child.stderr.take().unwrap());
        wait_until(&err, "/ [d]eny? ");
        let_the_question_settle();
        master.write_all(typed.as_bytes()).unwrap();
        let status = child.wait().unwrap();
        assert!(status.success(), "{}", err.lock().unwrap());
        out_thread.join().unwrap();
        err_thread.join().unwrap();
        let (out, err) = (out.lock().unwrap().clone(), err.lock().unwrap().clone());
        (out, err)
    }

    #[test]
    fn ask_on_a_terminal_asks_and_allow_once_runs_the_command() {
        let w = AgentWorld::new(asks_to_run_the_tests());
        let (out, err) = ask_on_a_terminal(&w, "o\n");
        assert!(
            err.contains(
                "Claude Code wants to run: cargo test -p clusia-core  [o]nce / [r]eview (cargo test) / [d]eny? "
            ),
            "{err}"
        );
        assert!(
            err.contains("✓ ran cargo test -p clusia-core (you allowed it)"),
            "{err}"
        );
        assert!(out.contains("Done."), "{out}");
        let answers = FakeClaude::permission_answers(&w.fake_dir());
        assert_eq!(answers.len(), 1);
        assert!(answers[0].allow);
        let log = stdout(&w.run(&["agent", "log", "acme/widgets#7"]));
        assert!(
            log.contains("  ✓ ran cargo test -p clusia-core (you allowed it)"),
            "{log}"
        );
    }

    #[test]
    fn ask_on_a_terminal_review_saves_the_prefix_and_enter_alone_denies() {
        let w = AgentWorld::new(asks_to_run_the_tests());
        let (_, err) = ask_on_a_terminal(&w, "r\n");
        assert!(
            err.contains("✓ ran cargo test -p clusia-core (allowed for this review)"),
            "{err}"
        );
        assert!(FakeClaude::permission_answers(&w.fake_dir())[0].allow);

        let denying = AgentWorld::new(asks_to_run_the_tests());
        let (out, err) = ask_on_a_terminal(&denying, "\n");
        assert!(err.contains("⊘ Denied: cargo test -p clusia-core"), "{err}");
        assert!(out.contains("Done."), "the agent goes on without it: {out}");
        assert!(!FakeClaude::permission_answers(&denying.fake_dir())[0].allow);
    }

    /// What a fake daemon was told: the answers that reached it, as (request id, answer).
    type Answers =
        std::sync::Arc<std::sync::Mutex<Vec<(String, clusia_protocol::PermissionAnswerKind)>>>;

    /// A daemon that asks permission once for `cargo test -p clusia-core` as soon as a question
    /// arrives, then ends the turn when the answer comes. It stands where the real one would,
    /// so the terminal side is tested on its own.
    fn daemon_that_asks(rt: &tokio::runtime::Runtime, socket: &Path) -> Answers {
        daemon_that_asks_after(rt, socket, None, None)
    }

    /// Like `daemon_that_asks`, but the request is only announced once `gate` is released.
    /// `connected` fires when the question has been accepted: the client is connected and about
    /// to read its terminal.
    fn daemon_that_asks_after(
        rt: &tokio::runtime::Runtime,
        socket: &Path,
        gate: Option<tokio::sync::oneshot::Receiver<()>>,
        connected: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Answers {
        let (mut gate, mut connected) = (gate, connected);
        use clusia_protocol::{
            ClientMessage, Command as Request, Event, MessageReader, Outcome, PROTOCOL_VERSION,
            PermissionAnswerKind, PermissionOutcome, Reply, ServerMessage, write_message,
        };
        let listener = {
            let _guard = rt.enter();
            tokio::net::UnixListener::bind(socket).unwrap()
        };
        let answers = Answers::default();
        let told = answers.clone();
        rt.spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (read, mut write) = stream.into_split();
            let mut read = MessageReader::new(read);
            let Ok(Some(ClientMessage::Hello { .. })) = read.next::<ClientMessage>().await else {
                return;
            };
            let welcome = ServerMessage::Welcome {
                protocol: PROTOCOL_VERSION,
                daemon: "9.9.9".into(),
            };
            write_message(&mut write, &welcome).await.unwrap();
            let pr: clusia_core::PrRef = "acme/widgets#7".parse().unwrap();
            let event = |event| ServerMessage::Event {
                topic: "agent".into(),
                event,
            };
            while let Ok(Some(ClientMessage::Request { id, cmd })) =
                read.next::<ClientMessage>().await
            {
                let is_send = matches!(cmd, Request::AgentSend { .. });
                let (reply, then) = match cmd {
                    Request::AgentSend { .. } => (
                        Reply::AgentTurn { turn: 1 },
                        vec![event(Event::PermissionRequested {
                            id: "perm-1".into(),
                            pr: pr.clone(),
                            turn: 1,
                            tool: "Bash".into(),
                            summary: "cargo test -p clusia-core".into(),
                            reason: None,
                            prefix: Some("cargo test".into()),
                            sandbox: true,
                            deadline: 0,
                            detail: None,
                            origin: clusia_protocol::TurnOrigin::Chat,
                        })],
                    ),
                    Request::PermissionAnswer { id: asked, answer } => {
                        told.lock().unwrap().push((asked.clone(), answer));
                        let outcome = match answer {
                            PermissionAnswerKind::Once => PermissionOutcome::Allowed,
                            PermissionAnswerKind::Review => PermissionOutcome::AllowedForReview,
                            PermissionAnswerKind::Deny => PermissionOutcome::Denied,
                        };
                        (
                            Reply::Ack,
                            vec![
                                event(Event::PermissionResolved {
                                    origin: clusia_protocol::TurnOrigin::Chat,
                                    id: asked,
                                    pr: pr.clone(),
                                    tool: "Bash".into(),
                                    summary: "cargo test -p clusia-core".into(),
                                    outcome,
                                }),
                                event(Event::AgentChunk {
                                    pr: pr.clone(),
                                    turn: 1,
                                    text: "Done.".into(),
                                }),
                                event(Event::AgentDone {
                                    pr: pr.clone(),
                                    turn: 1,
                                    duration_ms: 1,
                                }),
                            ],
                        )
                    }
                    _ => (Reply::Ack, Vec::new()),
                };
                let response = ServerMessage::Response {
                    id,
                    result: Outcome::Ok(reply),
                };
                write_message(&mut write, &response).await.unwrap();
                if is_send && let Some(connected) = connected.take() {
                    let _ = connected.send(());
                }
                if is_send && let Some(gate) = gate.take() {
                    let _ = gate.await;
                }
                for message in then {
                    write_message(&mut write, &message).await.unwrap();
                }
            }
        });
        answers
    }

    /// `clusia ask` on a terminal against `daemon_that_asks`, with `typed` typed at the prompt.
    /// Returns (standard output, standard error, what the daemon was told).
    fn ask_the_fake_daemon(
        typed: &str,
    ) -> (
        String,
        String,
        Vec<(String, clusia_protocol::PermissionAnswerKind)>,
    ) {
        ask_the_fake_daemon_after(None, typed)
    }

    /// Like `ask_the_fake_daemon`, with `stray` typed while no question is open yet.
    fn ask_the_fake_daemon_after(
        stray: Option<&str>,
        typed: &str,
    ) -> (
        String,
        String,
        Vec<(String, clusia_protocol::PermissionAnswerKind)>,
    ) {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (release, gate) = tokio::sync::oneshot::channel();
        let (connected, is_connected) = tokio::sync::oneshot::channel();
        let answers = daemon_that_asks_after(
            &rt,
            &clusia_core::Paths::new(dir.path()).socket(),
            stray.map(|_| gate),
            stray.map(|_| connected),
        );
        let (mut master, slave) = pty();
        let mut child = fake_daemon_ask(dir.path(), slave);
        let (out, out_thread) = collect_joinable(child.stdout.take().unwrap());
        let (err, err_thread) = collect_joinable(child.stderr.take().unwrap());
        if let Some(stray) = stray {
            is_connected.blocking_recv().unwrap();
            master.write_all(stray.as_bytes()).unwrap();
            release.send(()).unwrap();
        }
        wait_until(&err, "/ [d]eny? ");
        let_the_question_settle();
        master.write_all(typed.as_bytes()).unwrap();
        let status = child.wait().unwrap();
        out_thread.join().unwrap();
        err_thread.join().unwrap();
        let (out, err) = (out.lock().unwrap().clone(), err.lock().unwrap().clone());
        assert!(status.success(), "{err}");
        let told = answers.lock().unwrap().clone();
        (out, err, told)
    }

    fn fake_daemon_ask(home: &Path, stdin: std::process::Stdio) -> std::process::Child {
        Command::new(env!("CARGO_BIN_EXE_clusia"))
            .arg("--home")
            .arg(home)
            .args(["ask", "acme/widgets#7", "run the tests"])
            .env("CLUSIA_CLAUDE_BIN", "/nonexistent/claude")
            .stdin(stdin)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    }

    #[test]
    fn a_line_typed_before_the_question_is_shown_never_answers_it() {
        use clusia_protocol::PermissionAnswerKind::Deny;
        let (_, err, told) = ask_the_fake_daemon_after(Some("o\n"), "\n");
        assert_eq!(told, [("perm-1".to_string(), Deny)], "{err}");
    }

    #[test]
    fn ctrl_c_at_the_prompt_detaches_and_answers_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let answers = daemon_that_asks(&rt, &clusia_core::Paths::new(dir.path()).socket());
        let (_master, slave) = pty();
        let mut child = fake_daemon_ask(dir.path(), slave);
        let (err, err_thread) = collect_joinable(child.stderr.take().unwrap());
        wait_until(&err, "/ [d]eny? ");
        // SAFETY: the child is ours and still running (it waits at the prompt).
        unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
        let status = child.wait().unwrap();
        err_thread.join().unwrap();
        assert_eq!(status.code(), Some(130), "{}", err.lock().unwrap());
        assert!(
            err.lock()
                .unwrap()
                .contains("Detached: the agent keeps working"),
            "{}",
            err.lock().unwrap()
        );
        assert!(answers.lock().unwrap().is_empty());
    }

    #[test]
    fn the_terminal_sends_the_line_typed_at_the_prompt() {
        use clusia_protocol::PermissionAnswerKind::{Deny, Once, Review};
        for (typed, answer, line) in [
            (
                "o\n",
                Once,
                "✓ ran cargo test -p clusia-core (you allowed it)",
            ),
            (
                "review\n",
                Review,
                "✓ ran cargo test -p clusia-core (allowed for this review)",
            ),
            ("\n", Deny, "⊘ Denied: cargo test -p clusia-core"),
            ("sure\n", Deny, "⊘ Denied: cargo test -p clusia-core"),
        ] {
            let (out, err, told) = ask_the_fake_daemon(typed);
            assert_eq!(told, [("perm-1".to_string(), answer)], "{typed:?}");
            assert!(err.contains(line), "{typed:?}: {err}");
            assert!(
                err.contains("Claude Code wants to run: cargo test -p clusia-core  [o]nce / [r]eview (cargo test) / [d]eny? "),
                "{err}"
            );
            assert!(out.contains("Done."), "{out}");
        }
    }

    #[test]
    fn ask_without_a_terminal_waits_for_the_window() {
        use clusia_protocol::{Client, Command as Request, Event, PermissionAnswerKind, topics};
        let w = AgentWorld::new(asks_to_run_the_tests());
        let socket = clusia_core::Paths::new(w.home.dir.path()).socket();
        // A window that is already listening, as a real one is.
        let mut window = w._rt.block_on(async {
            let mut client = Client::connect(&socket, "test-window").await.unwrap();
            client
                .request(Request::Subscribe {
                    topics: vec![topics::AGENT.into()],
                })
                .await
                .unwrap();
            client
        });
        let mut asking = w.spawn(&["ask", "acme/widgets#7", "run the tests"]);
        let err = collect(asking.stderr.take().unwrap());
        wait_until(
            &err,
            "waiting for an answer in the window: Claude Code wants to run: cargo test -p clusia-core",
        );
        w._rt.block_on(async {
            loop {
                let (_, event) =
                    tokio::time::timeout(std::time::Duration::from_secs(10), window.next_event())
                        .await
                        .expect("the request is announced")
                        .unwrap();
                if let Event::PermissionRequested { id, .. } = event {
                    window
                        .request(Request::PermissionAnswer {
                            id,
                            answer: PermissionAnswerKind::Once,
                        })
                        .await
                        .unwrap();
                    return;
                }
            }
        });
        let done = asking.wait_with_output().unwrap();
        assert!(done.status.success(), "{}", stderr(&done));
        assert!(stdout(&done).contains("Done."), "{}", stdout(&done));
        assert!(FakeClaude::permission_answers(&w.fake_dir())[0].allow);
        assert!(
            err.lock()
                .unwrap()
                .contains("✓ ran cargo test -p clusia-core (you allowed it)"),
            "{}",
            err.lock().unwrap()
        );
    }

    #[test]
    fn the_prompt_closes_when_the_request_is_answered_in_the_window() {
        use clusia_protocol::{Client, Command as Request, Event, PermissionAnswerKind, topics};
        let w = AgentWorld::new(asks_to_run_the_tests());
        let socket = clusia_core::Paths::new(w.home.dir.path()).socket();
        let mut window = w._rt.block_on(async {
            let mut client = Client::connect(&socket, "test-window").await.unwrap();
            client
                .request(Request::Subscribe {
                    topics: vec![topics::AGENT.into()],
                })
                .await
                .unwrap();
            client
        });
        let (_master, slave) = pty();
        let mut child = w.home.spawn_with_stdin(
            &w.api,
            Some("tok"),
            &["ask", "acme/widgets#7", "run the tests"],
            slave,
        );
        let (out, out_thread) = collect_joinable(child.stdout.take().unwrap());
        let (err, err_thread) = collect_joinable(child.stderr.take().unwrap());
        wait_until(&err, "/ [d]eny? ");
        w._rt.block_on(async {
            loop {
                let (_, event) =
                    tokio::time::timeout(std::time::Duration::from_secs(10), window.next_event())
                        .await
                        .expect("the request is announced")
                        .unwrap();
                if let Event::PermissionRequested { id, .. } = event {
                    window
                        .request(Request::PermissionAnswer {
                            id,
                            answer: PermissionAnswerKind::Once,
                        })
                        .await
                        .unwrap();
                    return;
                }
            }
        });
        assert!(child.wait().unwrap().success());
        out_thread.join().unwrap();
        err_thread.join().unwrap();
        let (out, err) = (out.lock().unwrap().clone(), err.lock().unwrap().clone());
        assert!(out.contains("Done."), "{out}");
        assert!(
            err.contains("/ [d]eny? \n✓ ran cargo test -p clusia-core (you allowed it)"),
            "{err}"
        );
    }
}

#[test]
fn install_dry_run_prints_the_plan_and_changes_nothing() {
    let home = Home::new();
    let apps = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let out = home.clusia(&[
        "install",
        "--dry-run",
        "--applications",
        apps.path().to_str().unwrap(),
        "--bin-dir",
        bin.path().to_str().unwrap(),
        "--from",
        "/tmp/built",
    ]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("main executable clusia-tray"), "{text}");
    assert!(text.contains("<key>LSUIElement</key>"), "{text}");
    assert!(text.contains("<key>RunAtLoad</key>"), "{text}");
    assert!(
        text.contains(&format!("{}/clusia", bin.path().display())),
        "{text}"
    );
    assert_eq!(std::fs::read_dir(apps.path()).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(bin.path()).unwrap().count(), 0);
}

#[test]
fn install_for_another_data_folder_is_refused_unless_isolated() {
    let home = Home::new();
    let dir = tempfile::tempdir().unwrap();
    let (apps, bin) = (dir.path().join("apps"), dir.path().join("bin"));
    // A folder with no binaries: should the refusal ever go missing, nothing gets installed.
    let from = dir.path().join("built");
    let out = home.clusia(&[
        "install",
        "--applications",
        apps.to_str().unwrap(),
        "--bin-dir",
        bin.to_str().unwrap(),
        "--from",
        from.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(stderr(&out).contains("--no-launchctl"), "{}", stderr(&out));
    assert!(!apps.exists() && !bin.exists());
}

#[test]
fn uninstall_removes_the_bundle_the_agent_and_the_link_in_the_given_folders() {
    let home = Home::new();
    let dir = tempfile::tempdir().unwrap();
    let (apps, bin, agents) = (
        dir.path().join("apps"),
        dir.path().join("bin"),
        dir.path().join("agents"),
    );
    let contents = apps.join("Clusia.app/Contents");
    std::fs::create_dir_all(contents.join("MacOS")).unwrap();
    std::fs::write(
        contents.join("Info.plist"),
        r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>io.github.rzorzal.clusia</string></dict></plist>"#,
    )
    .unwrap();
    std::fs::write(contents.join("MacOS/clusia"), "").unwrap();
    std::fs::create_dir_all(&agents).unwrap();
    let agent = agents.join("io.github.rzorzal.clusia.daemon.plist");
    std::fs::write(
        &agent,
        r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>Label</key><string>io.github.rzorzal.clusia.daemon</string></dict></plist>"#,
    )
    .unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(contents.join("MacOS/clusia"), bin.join("clusia")).unwrap();
    std::fs::write(bin.join("other"), "x").unwrap();

    let out = home.clusia(&[
        "--json",
        "uninstall",
        "--applications",
        apps.to_str().unwrap(),
        "--bin-dir",
        bin.to_str().unwrap(),
        "--agents-dir",
        agents.to_str().unwrap(),
        "--no-launchctl",
    ]);
    assert!(out.status.success(), "{out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["removed"].as_array().unwrap().len(), 3, "{v}");
    assert!(
        v["kept"].as_array().unwrap().iter().any(|k| k
            .as_str()
            .unwrap()
            .contains(home.dir.path().to_str().unwrap())),
        "the data folder is the one given with --home: {v}"
    );
    assert!(!apps.join("Clusia.app").exists());
    assert!(!agent.exists());
    assert!(bin.join("clusia").symlink_metadata().is_err());
    assert!(bin.join("other").exists(), "nothing else is touched");
}

/// `clusia check` against a daemon whose every answer the test writes.
mod check_against_a_scripted_daemon {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use clusia_core::checks::{CheckKind, CheckResult, Finding};
    use clusia_protocol::{
        CheckState, CheckStatus, ClientMessage, Command as Request, Event, MessageReader, Outcome,
        PROTOCOL_VERSION, Reply, ServerMessage, write_message,
    };
    use tokio::sync::mpsc::UnboundedSender;

    type Say = UnboundedSender<ServerMessage>;

    fn pr() -> clusia_core::PrRef {
        "acme/widgets#7".parse().unwrap()
    }

    fn state(kind: CheckKind, state: CheckState) -> ServerMessage {
        ServerMessage::Event {
            topic: "agent".into(),
            event: Event::CheckState {
                pr: pr(),
                kind,
                state,
            },
        }
    }

    fn result(title: &str, at: i64) -> CheckResult {
        CheckResult {
            kind: CheckKind::Security,
            head: "h".into(),
            files: 1,
            findings: vec![Finding {
                id: format!("id-{title}"),
                kind: CheckKind::Security,
                area: "security".into(),
                severity: Some(clusia_core::checks::Severity::High),
                title: title.into(),
                file: "feature.txt".into(),
                line: Some(2),
                start_line: None,
                end_line: None,
                body: "b".into(),
                comment: "c".into(),
                code: None,
                anchored: true,
            }],
            passes: Vec::new(),
            unreadable: 0,
            areas: Vec::new(),
            at,
        }
    }

    fn checks(results: Vec<CheckResult>, now: CheckState) -> Option<Reply> {
        Some(Reply::Checks {
            results,
            states: vec![CheckStatus {
                kind: CheckKind::Security,
                state: now,
            }],
            accepted: Vec::new(),
            dismissed: Vec::new(),
        })
    }

    /// Serves one client on `socket`: each request gets what `answer` returns, or no answer at
    /// all for `None`. `answer` may send events, now or later, through the `Say` it is given.
    fn daemon(
        rt: &tokio::runtime::Runtime,
        socket: &Path,
        mut answer: impl FnMut(&Request, &Say) -> Option<Reply> + Send + 'static,
    ) {
        let listener = {
            let _guard = rt.enter();
            tokio::net::UnixListener::bind(socket).unwrap()
        };
        rt.spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (read, mut write) = stream.into_split();
            let mut read = MessageReader::new(read);
            let Ok(Some(ClientMessage::Hello { .. })) = read.next::<ClientMessage>().await else {
                return;
            };
            let (say, mut said) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();
            tokio::spawn(async move {
                while let Some(message) = said.recv().await {
                    if write_message(&mut write, &message).await.is_err() {
                        return;
                    }
                }
            });
            let _ = say.send(ServerMessage::Welcome {
                protocol: PROTOCOL_VERSION,
                daemon: "9.9.9".into(),
            });
            while let Ok(Some(ClientMessage::Request { id, cmd })) =
                read.next::<ClientMessage>().await
            {
                if let Some(reply) = answer(&cmd, &say) {
                    let _ = say.send(ServerMessage::Response {
                        id,
                        result: Outcome::Ok(reply),
                    });
                }
            }
        });
    }

    fn check(home: &Path) -> std::process::Child {
        std::process::Command::new(env!("CARGO_BIN_EXE_clusia"))
            .arg("--home")
            .arg(home)
            .args(["check", "acme/widgets#7", "--security"])
            .env("CLUSIA_CLAUDE_BIN", "/nonexistent/claude")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// Waits up to `limit` for `child`; kills it and returns `None` when it is still running.
    fn ended_within(child: &mut std::process::Child, limit: Duration) -> Option<i32> {
        let give_up = Instant::now() + limit;
        while Instant::now() < give_up {
            if let Some(status) = child.try_wait().unwrap() {
                return status.code();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        None
    }

    fn stderr_of(child: &mut std::process::Child) -> String {
        use std::io::Read;
        let mut text = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        text
    }

    fn stdout_of(child: &mut std::process::Child) -> String {
        use std::io::Read;
        let mut text = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        text
    }

    #[test]
    fn check_reports_the_run_it_started_not_an_older_ending() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let finished = Arc::new(AtomicBool::new(false));
        let done = finished.clone();
        daemon(
            &rt,
            &clusia_core::Paths::new(dir.path()).socket(),
            move |cmd, say| match cmd {
                Request::GetChecks { .. } if done.load(Ordering::SeqCst) => {
                    checks(vec![result("New finding", 1)], CheckState::Done)
                }
                Request::GetChecks { .. } => {
                    checks(vec![result("Old finding", 1)], CheckState::Done)
                }
                Request::RunCheck { kind, .. } => {
                    // What the open told about the old result arrives before the new run.
                    let _ = say.send(state(*kind, CheckState::Stale));
                    let (say, done, kind) = (say.clone(), done.clone(), *kind);
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        let _ = say.send(state(kind, CheckState::Waiting));
                        let _ = say.send(state(kind, CheckState::Running { activity: None }));
                        done.store(true, Ordering::SeqCst);
                        let _ = say.send(state(kind, CheckState::Done));
                    });
                    Some(Reply::Ack)
                }
                _ => Some(Reply::Ack),
            },
        );
        let mut child = check(dir.path());
        let code = ended_within(&mut child, Duration::from_secs(20));
        let (out, err) = (stdout_of(&mut child), stderr_of(&mut child));
        assert_eq!(code, Some(0), "{err}");
        assert!(out.contains("New finding"), "{out}");
        assert!(!out.contains("Old finding"), "{out}");
    }

    /// A daemon that runs the Security check and never answers its Stop.
    fn deaf_to_stop(rt: &tokio::runtime::Runtime, home: &Path) -> std::sync::mpsc::Receiver<()> {
        let (running, is_running) = std::sync::mpsc::channel();
        daemon(
            rt,
            &clusia_core::Paths::new(home).socket(),
            move |cmd, say| match cmd {
                Request::GetChecks { .. } => checks(Vec::new(), CheckState::NotRun),
                Request::RunCheck { kind, .. } => {
                    let _ = say.send(state(*kind, CheckState::Waiting));
                    let _ = say.send(state(*kind, CheckState::Running { activity: None }));
                    let _ = running.send(());
                    Some(Reply::Ack)
                }
                Request::StopCheck { .. } => None,
                _ => Some(Reply::Ack),
            },
        );
        is_running
    }

    fn interrupt(child: &std::process::Child) {
        // SAFETY: signals a child this test spawned and still owns.
        assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    }

    #[test]
    fn a_second_ctrl_c_ends_check_while_the_daemon_does_not_answer() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let running = deaf_to_stop(&rt, dir.path());
        let mut child = check(dir.path());
        running.recv_timeout(Duration::from_secs(30)).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        interrupt(&child);
        std::thread::sleep(Duration::from_millis(500));
        interrupt(&child);
        let code = ended_within(&mut child, Duration::from_secs(3));
        let err = stderr_of(&mut child);
        assert_eq!(code, Some(130), "{err}");
        assert!(
            err.contains(
                "The daemon did not answer; the checks may still run (Stop in the window)."
            ),
            "{err}"
        );
    }

    #[test]
    fn a_stop_the_daemon_does_not_answer_gives_up() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let running = deaf_to_stop(&rt, dir.path());
        let mut child = check(dir.path());
        running.recv_timeout(Duration::from_secs(30)).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        interrupt(&child);
        let code = ended_within(&mut child, Duration::from_secs(12));
        let err = stderr_of(&mut child);
        assert_eq!(code, Some(130), "{err}");
        assert!(err.contains("The daemon did not answer"), "{err}");
    }
}
