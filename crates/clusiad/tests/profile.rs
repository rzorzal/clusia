mod common;

use clusia_protocol::{ClientError, Command, ErrorCode, Reply};
use common::{TestDaemon, test_options};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn daemon(server: &MockServer, token: Option<&str>) -> TestDaemon {
    let mut options = test_options();
    options.github_api = Some(server.uri());
    options.github_token = token.map(str::to_string);
    TestDaemon::start_with(tempfile::tempdir().unwrap(), options).await
}

fn get(login: &str) -> Command {
    Command::GetProfile {
        login: login.into(),
    }
}

#[tokio::test]
async fn a_profile_comes_from_github_with_the_token() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users/mona"))
        .and(header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "login": "mona", "name": "Mona Lisa", "company": "@acme", "bio": "Reviews code",
            "avatar_url": "https://avatars.githubusercontent.com/u/2?v=4"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let d = daemon(&server, Some("tok")).await;
    let mut c = d.client().await;
    let Ok(Reply::Profile(p)) = c.request(get("mona")).await else {
        panic!("expected a profile");
    };
    assert_eq!(p.name.as_deref(), Some("Mona Lisa"));
    assert_eq!(p.company.as_deref(), Some("@acme"));
    assert_eq!(p.bio.as_deref(), Some("Reviews code"));
}

#[tokio::test]
async fn a_bad_login_and_a_missing_token_are_answered_without_a_request() {
    let server = MockServer::start().await;
    let d = daemon(&server, None).await;
    let mut c = d.client().await;
    match c.request(get("mona")).await {
        Err(ClientError::Server(e)) => assert_eq!(e.code, ErrorCode::Unauthorized),
        other => panic!("expected an auth error, got {other:?}"),
    }
    let d = daemon(&server, Some("tok")).await;
    let mut c = d.client().await;
    match c.request(get("../user")).await {
        Err(ClientError::Server(e)) => assert_eq!(e.code, ErrorCode::NotFound),
        other => panic!("expected not found, got {other:?}"),
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}
