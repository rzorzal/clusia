mod common;

use std::sync::Arc;

use clusia_platform::{MemoryStore, SecretStore};
use clusia_protocol::{AuthInfo, ClientError, Command, ErrorCode, Reply, TokenSource};
use common::{TestDaemon, test_options};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn code(r: Result<Reply, ClientError>) -> ErrorCode {
    match r {
        Err(ClientError::Server(e)) => e.code,
        other => panic!("expected a server error, got {other:?}"),
    }
}

fn auth_of(r: Reply) -> AuthInfo {
    match r {
        Reply::Auth(a) => a,
        other => panic!("expected Auth, got {other:?}"),
    }
}

async fn daemon(server: &MockServer, token: Option<&str>, store: Arc<MemoryStore>) -> TestDaemon {
    let mut o = test_options();
    o.github_api = Some(server.uri());
    o.github_token = token.map(str::to_string);
    o.secrets = store;
    TestDaemon::start_with(tempfile::tempdir().unwrap(), o).await
}

fn pull_json() -> serde_json::Value {
    json!({
        "number": 7, "title": "Fix cache", "html_url": "https://github.com/acme/widgets/pull/7",
        "user": { "login": "maria" }, "draft": false, "updated_at": "2026-10-01T12:00:00Z",
        "comments": 1, "review_comments": 1, "additions": 3, "deletions": 1, "changed_files": 1,
        "base": { "ref": "main", "sha": "aaa", "repo": { "clone_url": "https://github.com/acme/widgets.git" } },
        "head": { "ref": "fix", "sha": "bbb", "repo": null }
    })
}

#[tokio::test]
async fn get_pr_returns_detail() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(pull_json()))
        .mount(&server)
        .await;
    let d = daemon(&server, Some("tok"), Arc::default()).await;
    match d
        .client()
        .await
        .request(Command::GetPr {
            pr: "acme/widgets#7".parse().unwrap(),
        })
        .await
        .unwrap()
    {
        Reply::Pr(p) => assert_eq!((p.head_sha.as_str(), p.summary.comments), ("bbb", 2)),
        other => panic!("{other:?}"),
    }
    d.stop().await;
}

#[tokio::test]
async fn get_pr_errors_map_to_codes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let d = daemon(&server, Some("tok"), Arc::default()).await;
    let pr = "acme/widgets#9".parse().unwrap();
    assert_eq!(
        code(d.client().await.request(Command::GetPr { pr }).await),
        ErrorCode::NotFound
    );
    d.stop().await;

    let d = TestDaemon::start().await;
    let pr = "acme/widgets#9".parse().unwrap();
    assert_eq!(
        code(d.client().await.request(Command::GetPr { pr }).await),
        ErrorCode::Unauthorized
    );
    d.stop().await;
}

#[tokio::test]
async fn auth_status_reports_source_login_and_scopes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-oauth-scopes", "repo")
                .set_body_json(json!({ "login": "octo" })),
        )
        .mount(&server)
        .await;
    let d = daemon(&server, Some("tok"), Arc::default()).await;
    let a = auth_of(d.client().await.request(Command::AuthStatus).await.unwrap());
    assert_eq!(
        a,
        AuthInfo {
            source: Some(TokenSource::Env),
            login: Some("octo".into()),
            scopes: vec!["repo".into()],
            error: None
        }
    );
    d.stop().await;
}

#[tokio::test]
async fn auth_status_without_token_explains() {
    let d = TestDaemon::start().await;
    let a = auth_of(d.client().await.request(Command::AuthStatus).await.unwrap());
    assert_eq!(a.source, None);
    assert!(a.error.unwrap().contains("auth login"));
    d.stop().await;
}

#[tokio::test]
async fn set_token_stores_a_pat_that_auth_then_uses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", "Bearer ghp_new"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "login": "octo" })))
        .mount(&server)
        .await;
    let store = Arc::new(MemoryStore::default());
    let d = daemon(&server, None, store.clone()).await;
    let mut c = d.client().await;
    assert_eq!(
        c.request(Command::SetToken {
            token: " ghp_new\n".into()
        })
        .await
        .unwrap(),
        Reply::Ack
    );
    assert_eq!(store.get("github.com").unwrap().as_deref(), Some("ghp_new"));
    let a = auth_of(c.request(Command::AuthStatus).await.unwrap());
    assert_eq!(
        (a.source, a.login.as_deref()),
        (Some(TokenSource::Pat), Some("octo"))
    );
    assert_eq!(c.request(Command::ClearToken).await.unwrap(), Reply::Ack);
    assert_eq!(store.get("github.com").unwrap(), None);
    d.stop().await;
}

#[tokio::test]
async fn empty_token_is_rejected() {
    let d = TestDaemon::start().await;
    assert_eq!(
        code(
            d.client()
                .await
                .request(Command::SetToken { token: "  ".into() })
                .await
        ),
        ErrorCode::BadRequest
    );
    d.stop().await;
}
