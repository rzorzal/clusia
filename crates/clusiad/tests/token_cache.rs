mod common;

use std::path::Path;

use clusia_protocol::{Command, Reply};
use common::{TestDaemon, test_options};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A `gh` that notes each run in `runs` and answers a token numbered by how many runs there
/// have been: `tok-1`, `tok-2`, ...
fn counting_gh(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let runs = dir.join("runs");
    let gh = dir.join("gh");
    clusia_core::testkit::write_executable(
        &gh,
        &format!(
            "echo run >> '{runs}'\nprintf 'tok-%s\\n' \"$(wc -l < '{runs}' | tr -d ' ')\"",
            runs = runs.display()
        ),
    );
    (gh, runs)
}

fn gh_runs(runs: &Path) -> usize {
    std::fs::read_to_string(runs).map_or(0, |s| s.lines().count())
}

async fn daemon_with(server: &MockServer, gh: std::path::PathBuf) -> TestDaemon {
    let mut options = test_options();
    options.github_api = Some(server.uri());
    options.gh_program = gh;
    TestDaemon::start_with(tempfile::tempdir().unwrap(), options).await
}

async fn auth(c: &mut clusia_protocol::Client) -> clusia_protocol::AuthInfo {
    match c.request(Command::AuthStatus).await.unwrap() {
        Reply::Auth(info) => info,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn gh_is_asked_for_the_token_once_for_many_requests() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "maria" })))
        .mount(&server)
        .await;
    let tools = tempfile::tempdir().unwrap();
    let (gh, runs) = counting_gh(tools.path());
    let d = daemon_with(&server, gh).await;
    let mut c = d.client().await;

    for _ in 0..4 {
        assert_eq!(auth(&mut c).await.login.as_deref(), Some("maria"));
    }

    assert_eq!(gh_runs(&runs), 1);
    d.stop().await;
}

#[tokio::test]
async fn a_rejected_token_is_asked_for_again() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", "Bearer tok-1"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({ "message": "Bad credentials" })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", "Bearer tok-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "maria" })))
        .mount(&server)
        .await;
    let tools = tempfile::tempdir().unwrap();
    let (gh, runs) = counting_gh(tools.path());
    let d = daemon_with(&server, gh).await;
    let mut c = d.client().await;

    assert!(
        auth(&mut c).await.error.is_some(),
        "the first token is rejected"
    );
    assert_eq!(auth(&mut c).await.login.as_deref(), Some("maria"));
    assert_eq!(auth(&mut c).await.login.as_deref(), Some("maria"));

    assert_eq!(
        gh_runs(&runs),
        2,
        "one more run after the 401, none after that"
    );
    d.stop().await;
}

#[tokio::test]
async fn changing_the_github_settings_asks_gh_again() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "maria" })))
        .mount(&server)
        .await;
    let tools = tempfile::tempdir().unwrap();
    let (gh, runs) = counting_gh(tools.path());
    let d = daemon_with(&server, gh).await;
    let mut c = d.client().await;
    auth(&mut c).await;

    c.request(Command::SetConfigValue {
        key: "github.poll_interval_secs".into(),
        value: "120".into(),
    })
    .await
    .unwrap();
    auth(&mut c).await;

    assert_eq!(gh_runs(&runs), 2);
    d.stop().await;
}

#[tokio::test]
async fn a_gh_account_switch_is_followed_at_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "maria" })))
        .mount(&server)
        .await;
    let tools = tempfile::tempdir().unwrap();
    let (gh, runs) = counting_gh(tools.path());
    let config = tempfile::tempdir().unwrap();
    let hosts = config.path().join("hosts.yml");
    std::fs::write(&hosts, "github.com:\n    user: maria\n").unwrap();
    let mut options = test_options();
    options.github_api = Some(server.uri());
    options.gh_program = gh;
    options.gh_config_dir = Some(config.path().to_path_buf());
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), options).await;
    let mut c = d.client().await;

    auth(&mut c).await;
    auth(&mut c).await;
    assert_eq!(gh_runs(&runs), 1, "nothing changed: the token is kept");

    // `gh auth switch` rewrites the file; the size differs, so the stamp does whatever the clock says.
    std::fs::write(&hosts, "github.com:\n    user: maria-work\n").unwrap();
    auth(&mut c).await;
    assert_eq!(gh_runs(&runs), 2);

    std::fs::remove_file(&hosts).unwrap();
    auth(&mut c).await;
    assert_eq!(gh_runs(&runs), 3, "a missing file is a key of its own");
    auth(&mut c).await;
    assert_eq!(gh_runs(&runs), 3);
    d.stop().await;
}
