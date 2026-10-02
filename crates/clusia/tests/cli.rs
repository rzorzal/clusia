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
            .env_remove("CLUSIA_GITHUB_TOKEN")
            .output()
            .unwrap()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.clusia(&["daemon", "stop"]);
    }
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
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_clusia"));
        cmd.arg("--home")
            .arg(self.dir.path())
            .args(args)
            .env("CLUSIA_DAEMON_BIN", clusiad_bin())
            .env("CLUSIA_GITHUB_API", api)
            .env("CLUSIA_GH_BIN", "/nonexistent/gh")
            .env("CLUSIA_SECRET_STORE", "memory")
            .env_remove("CLUSIA_GITHUB_TOKEN")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::piped());
        if let Some(t) = token {
            cmd.env("CLUSIA_GITHUB_TOKEN", t);
        }
        let mut child = cmd.spawn().unwrap();
        {
            let mut input = child.stdin.take().unwrap();
            if let Some(s) = stdin {
                input.write_all(s.as_bytes()).unwrap();
            }
        }
        child.wait_with_output().unwrap()
    }
}

fn mock_github() -> (tokio::runtime::Runtime, wiremock::MockServer) {
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
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [issue(7, "acme/widgets", false)] })))
            .mount(&server).await;
        Mock::given(method("GET")).and(path("/search/issues"))
            .and(query_param("q", "is:pr is:open archived:false author:@me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [issue(3, "me/tool", true)] })))
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
