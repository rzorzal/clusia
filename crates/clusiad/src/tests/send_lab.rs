//! Send: only to a mapped port of this review's live system, never redirected or proxied, and
//! every answer bounded and cleaned. The helpers here install a system without starting one.

use std::collections::BTreeMap;
use std::time::Duration;

use clusia_core::PrRef;
use clusia_core::cockpit::RunSide;
use clusia_core::system::{
    Endpoint, HttpMethod, MAIN_SYSTEM_LATER, NOT_UP, PortView, SEND_BODY_MAX, SEND_GUARD,
    SEND_TIMEOUT_TEXT, SendRequest, StoredEndpoints, SystemState,
};
use clusia_protocol::{ErrorCode, Event, Outcome, Reply};
use clusia_testrun::names::repo_slug;
use clusia_testrun::system::system_names;
use clusia_testrun::testkit::{FakeExec, FakeReply};
use wiremock::matchers::{body_string, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::lab::{Lab, Machine, lab, pr, refused};
use super::system::SystemRun;
use super::system_lab::{
    start, system_lab_with_endpoints, system_of, until_state, with_system_recipe,
};
use super::*;

/// Puts a `SystemRun` of `pr(n)` in `state` with `ports` (name, container port, host port).
pub(super) fn install(
    lab: &Lab,
    n: u64,
    state: SystemState,
    keep: bool,
    ports: &[(&str, u16, Option<u16>)],
) {
    install_pr(lab, &pr(n), state, keep, ports);
}

/// `install` for any review, of any repository.
pub(super) fn install_pr(
    lab: &Lab,
    review: &PrRef,
    state: SystemState,
    keep: bool,
    ports: &[(&str, u16, Option<u16>)],
) {
    let names = system_names(
        &repo_slug(&review.owner, &review.repo),
        review.number,
        RunSide::Pr,
        None,
    );
    let mut run = SystemRun::new(
        u64::from(lab.shared.tests.seq()),
        names,
        "h1".into(),
        "recipe-hash".into(),
        keep,
    );
    run.state = state;
    run.ports = ports
        .iter()
        .map(|(name, port, host_port)| PortView {
            name: (*name).to_string(),
            port: *port,
            path: "/".into(),
            host_port: *host_port,
            url: None,
        })
        .collect();
    lab.shared
        .tests
        .with(review, |entry| entry.system = Some(run));
}

/// Every teardown command succeeds.
pub(super) fn teardown_rules(exec: &FakeExec) {
    exec.on("docker", &["stop"], FakeReply::ok(""));
    exec.on("docker", &["rm"], FakeReply::ok(""));
    exec.on("docker", &["network", "rm"], FakeReply::ok(""));
    exec.on("docker", &["compose"], FakeReply::ok(""));
}

/// Writes review `n`'s endpoints file as the daemon does.
pub(super) fn stored_endpoints(lab: &Lab, n: u64, endpoints: &[Endpoint]) {
    let stored = StoredEndpoints {
        written_head: "h1".into(),
        written_at: 1_762_300_000,
        endpoints: endpoints.to_vec(),
    };
    super::endpoints::write_endpoints_file(&lab.worktree(n), &stored).unwrap();
}

pub(super) fn endpoint(method: HttpMethod, path: &str) -> Endpoint {
    Endpoint {
        method,
        path: path.into(),
        port: "api".into(),
        why: "Changed in this PR".into(),
        new: false,
        headers: BTreeMap::new(),
        body: None,
        params: BTreeMap::new(),
    }
}

/// A wiremock server that fails the test when anything reaches it.
async fn untouched() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    server
}

fn running_on(host: u16) -> (Lab, u16) {
    let lab = lab(
        Machine::Colima { running: true },
        Vec::new(),
        &[7],
        |x, _| teardown_rules(x),
    );
    with_system_recipe(&lab, 7);
    install(
        &lab,
        7,
        SystemState::Running,
        false,
        &[("api", 8080, Some(host))],
    );
    (lab, host)
}

fn mock_lab(host: u16) -> Lab {
    running_on(host).0
}

fn stored_for(lab: &Lab) {
    stored_endpoints(lab, 7, &[endpoint(HttpMethod::Get, "/health")]);
}

fn request(port: u16, target: &str) -> SendRequest {
    SendRequest {
        method: HttpMethod::Get,
        port,
        target: target.into(),
        headers: vec![],
        body: None,
    }
}

trait UnwrapReply {
    fn unwrap_reply(self) -> Reply;
}

impl UnwrapReply for Outcome {
    fn unwrap_reply(self) -> Reply {
        match self {
            Outcome::Ok(reply) => reply,
            Outcome::Err(e) => panic!("refused: {e:?}"),
        }
    }
}

/// The message of a bad-request refusal.
fn bad(out: Outcome) -> String {
    match out {
        Outcome::Err(e) => {
            assert_eq!(e.code, ErrorCode::BadRequest, "{}", e.message);
            e.message
        }
        other => panic!("not refused: {other:?}"),
    }
}

#[tokio::test]
async fn send_reaches_only_the_mapped_port() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/widgets"))
        .and(header("content-type", "application/json"))
        .and(header("user-agent", "clusia"))
        .and(body_string("{\"name\":\"fake\"}"))
        .respond_with(
            ResponseTemplate::new(201)
                .insert_header("x-test", "1")
                .set_body_string("created"),
        )
        .expect(1)
        .mount(&mock)
        .await;
    let elsewhere = untouched().await;
    let host = mock.address().port();
    let (lab, _) = running_on(host);
    let mut events = lab.shared.events.subscribe();

    let mut post = endpoint(HttpMethod::Post, "/widgets");
    post.headers
        .insert("content-type".into(), "application/json".into());
    post.body = Some("{\"name\":\"fake\"}".into());
    stored_endpoints(&lab, 7, &[post, endpoint(HttpMethod::Get, "/health")]);

    let Reply::Sent(result) =
        send_endpoint(&lab.shared, &pr(7), RunSide::Pr, "POST /widgets".into())
            .await
            .unwrap_reply()
    else {
        panic!("a Sent reply")
    };
    assert_eq!(
        (result.status, result.reason.as_deref()),
        (Some(201), Some("Created"))
    );
    assert_eq!(result.url, format!("http://127.0.0.1:{host}/widgets"));
    assert_eq!(
        (
            result.body.as_str(),
            result.body_bytes,
            result.truncated,
            result.error.clone()
        ),
        ("created", 7, false, None)
    );
    assert!(
        result
            .headers
            .iter()
            .any(|(k, v)| k == "x-test" && v == "1")
    );
    assert_eq!(result.endpoint, "POST /widgets");
    assert!(elsewhere.received_requests().await.unwrap().is_empty());

    // The result is the endpoint's `last`, and `SystemChanged` carried it.
    let view = system_of(system(&lab.shared, &pr(7)).await);
    let listed = view
        .endpoints
        .list
        .iter()
        .find(|e| e.id == "POST /widgets")
        .unwrap();
    assert_eq!(listed.last.as_ref(), Some(&*result));
    let mut changed = false;
    while let Ok((_, event)) = events.try_recv() {
        if let Event::SystemChanged { view, .. } = event {
            changed |= view
                .endpoints
                .list
                .iter()
                .any(|e| e.last.as_ref().is_some_and(|r| r.status == Some(201)));
        }
    }
    assert!(changed, "SystemChanged went out with the result");
}

#[tokio::test]
async fn the_guard_refuses_every_port_and_target_that_leaves_the_mapped_one() {
    let (lab, host) = running_on(49_152);
    let other = if host == 50_000 { 50_001 } else { 50_000 };
    for (port, target) in [
        (other, "/x"),
        (host, "@evil.example/x"),
        (host, ":80/x"),
        (host, "\\@evil.example/"),
        (host, ""),
        (host, "x"),
        (host, "/a b"),
        (host, "/a#b"),
    ] {
        assert_eq!(
            system_http::guard(&lab.shared, &pr(7), RunSide::Pr, port, target).unwrap_err(),
            SEND_GUARD,
            "{port} {target}"
        );
    }
    assert!(system_http::guard(&lab.shared, &pr(7), RunSide::Pr, host, "/ok?a=b").is_ok());
    assert_eq!(
        system_http::guard(&lab.shared, &pr(7), RunSide::Main, host, "/ok").unwrap_err(),
        MAIN_SYSTEM_LATER
    );
    // Another review's system does not open this review's guard.
    assert!(system_http::guard(&lab.shared, &pr(8), RunSide::Pr, host, "/ok").is_err());
    // `send` reports the refusal as a result and sends nothing.
    let server = untouched().await;
    let result = system_http::send(
        &lab.shared,
        &pr(7),
        "GET /x",
        request(server.address().port(), "/x"),
    )
    .await;
    assert_eq!(
        (result.status, result.error.as_deref()),
        (None, Some(SEND_GUARD))
    );
}

#[tokio::test]
async fn send_follows_no_redirect() {
    let target = untouched().await;
    let first = MockServer::start().await;
    Mock::given(path("/go"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/secret", target.uri())),
        )
        .expect(1)
        .mount(&first)
        .await;
    let (lab, _) = running_on(first.address().port());
    let result = system_http::send(
        &lab.shared,
        &pr(7),
        "GET /go",
        request(first.address().port(), "/go"),
    )
    .await;
    assert_eq!(result.status, Some(302));
    assert!(
        result
            .headers
            .iter()
            .any(|(k, v)| k == "location" && v.ends_with("/secret"))
    );
    assert_eq!(result.body, "");
    assert!(
        target.received_requests().await.unwrap().is_empty(),
        "the redirect target is reached zero times"
    );
}

#[tokio::test]
async fn send_refuses_when_the_system_is_down() {
    let mock = untouched().await;
    let host = mock.address().port();
    for state in [
        SystemState::NotStarted,
        SystemState::Starting {
            stage: clusia_core::system::StartStage::Health,
        },
        SystemState::Stopping,
        SystemState::Stopped {
            why: clusia_core::system::StopWhy::Reviewer,
        },
        SystemState::Failed,
    ] {
        let lab = mock_lab(host);
        stored_for(&lab);
        lab.shared
            .tests
            .with(&pr(7), |e| e.system.as_mut().unwrap().state = state);
        assert_eq!(
            refused(send_endpoint(&lab.shared, &pr(7), RunSide::Pr, "GET /health".into()).await),
            NOT_UP,
            "{state:?}"
        );
    }
    // No system at all, and a port that is not mapped (the run is up, the port has no host port).
    let bare = lab(
        Machine::Colima { running: true },
        Vec::new(),
        &[7],
        |_, _| {},
    );
    with_system_recipe(&bare, 7);
    stored_endpoints(&bare, 7, &[endpoint(HttpMethod::Get, "/health")]);
    assert_eq!(
        refused(send_endpoint(&bare.shared, &pr(7), RunSide::Pr, "GET /health".into()).await),
        NOT_UP
    );
    let unmapped = lab(
        Machine::Colima { running: true },
        Vec::new(),
        &[7],
        |_, _| {},
    );
    with_system_recipe(&unmapped, 7);
    install(
        &unmapped,
        7,
        SystemState::Running,
        false,
        &[("api", 8080, None)],
    );
    stored_endpoints(&unmapped, 7, &[endpoint(HttpMethod::Get, "/health")]);
    assert_eq!(
        refused(send_endpoint(&unmapped.shared, &pr(7), RunSide::Pr, "GET /health".into()).await),
        NOT_UP
    );
    // Main, and an id nobody listed, refuse before any state is read.
    assert_eq!(
        refused(
            send_endpoint(
                &unmapped.shared,
                &pr(7),
                RunSide::Main,
                "GET /health".into()
            )
            .await
        ),
        MAIN_SYSTEM_LATER
    );
    assert_eq!(
        bad(send_endpoint(&unmapped.shared, &pr(7), RunSide::Pr, "GET /nope".into()).await),
        "No endpoint GET /nope in acme/widgets#7"
    );
    assert!(
        mock.received_requests().await.unwrap().is_empty(),
        "nothing was sent"
    );
}

#[tokio::test]
async fn an_unhealthy_system_still_takes_a_send() {
    let mock = MockServer::start().await;
    Mock::given(path("/health"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&mock)
        .await;
    let (lab, _) = running_on(mock.address().port());
    lab.shared.tests.with(&pr(7), |e| {
        e.system.as_mut().unwrap().state = SystemState::Unhealthy
    });
    stored_endpoints(&lab, 7, &[endpoint(HttpMethod::Get, "/health")]);
    let Reply::Sent(result) = send_endpoint(&lab.shared, &pr(7), RunSide::Pr, "GET /health".into())
        .await
        .unwrap_reply()
    else {
        panic!("a Sent reply")
    };
    assert_eq!(
        (result.status, result.reason.as_deref()),
        (Some(503), Some("Service Unavailable"))
    );
}

#[tokio::test]
async fn a_reserved_header_never_reaches_the_wire() {
    let mock = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .mount(&mock)
        .await;
    let host = mock.address().port();
    let (lab, _) = running_on(host);
    let mut req = request(host, "/x");
    req.headers = [
        "host",
        "Host",
        "content-length",
        "transfer-encoding",
        "connection",
        "upgrade",
        "te",
        "expect",
    ]
    .iter()
    .map(|n| (n.to_string(), "evil.example".to_string()))
    .chain([("x-a".to_string(), "1".to_string())])
    .collect();
    let result = system_http::send(&lab.shared, &pr(7), "GET /x", req).await;
    assert_eq!(result.status, Some(200));
    let seen = &mock.received_requests().await.unwrap()[0];
    assert_eq!(
        seen.headers.get("host").unwrap().to_str().unwrap(),
        format!("127.0.0.1:{host}")
    );
    assert_eq!(seen.headers.get("x-a").unwrap().to_str().unwrap(), "1");
    for reserved in ["upgrade", "te", "expect", "transfer-encoding"] {
        assert!(seen.headers.get(reserved).is_none(), "{reserved}");
    }
}

#[tokio::test]
async fn a_response_is_bounded_and_cleaned() {
    let mock = MockServer::start().await;
    let mut big = ResponseTemplate::new(200).set_body_bytes(vec![b'a'; SEND_BODY_MAX + 5_000]);
    for i in 0..60 {
        big = big.insert_header(format!("x-h-{i:02}").as_str(), "v");
    }
    Mock::given(path("/big"))
        .respond_with(big)
        .mount(&mock)
        .await;
    Mock::given(path("/dirty"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-long", "z".repeat(300).as_str())
                .set_body_bytes(b"ok\x1b[31m red\x00\r\n\ttab \xe2\x80\xaeevil \xff".to_vec()),
        )
        .mount(&mock)
        .await;
    let host = mock.address().port();
    let (lab, _) = running_on(host);

    let big = system_http::send(&lab.shared, &pr(7), "GET /big", request(host, "/big")).await;
    assert!(big.truncated);
    assert_eq!(
        (big.body_bytes, big.body.len()),
        (SEND_BODY_MAX as u64, SEND_BODY_MAX)
    );
    assert_eq!(big.headers.len(), 50, "at most 50 headers");
    assert!(big.took_ms < 5_000);

    let dirty = system_http::send(&lab.shared, &pr(7), "GET /dirty", request(host, "/dirty")).await;
    assert!(!dirty.truncated);
    assert_eq!(dirty.body, "ok[31m red\n\ttab evil \u{fffd}");
    let long = dirty.headers.iter().find(|(k, _)| k == "x-long").unwrap();
    assert_eq!(long.1.chars().count(), 200);
}

#[tokio::test]
async fn a_refused_connection_and_a_silent_one_are_results_not_failures() {
    // A port nobody listens on.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let (lab, _) = running_on(port);
    let refused_result =
        system_http::send(&lab.shared, &pr(7), "GET /x", request(port, "/x")).await;
    assert_eq!(refused_result.status, None);
    assert!(
        refused_result
            .error
            .as_deref()
            .unwrap()
            .starts_with("Could not connect: "),
        "{:?}",
        refused_result.error
    );
}

#[tokio::test]
async fn a_silent_server_times_out() {
    // A real socket never meets a paused clock, so the timeout is shortened through the knob.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    // Accepts and never answers.
    let _silent = tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            held.push(listener.accept().await);
        }
    });
    let (lab, _) = running_on(port);
    lab.shared
        .tests
        .set_send_timeout(Duration::from_millis(200));
    let result = system_http::send(&lab.shared, &pr(7), "GET /x", request(port, "/x")).await;
    assert_eq!(
        (result.status, result.error.as_deref()),
        (None, Some(SEND_TIMEOUT_TEXT))
    );
}

#[tokio::test]
async fn a_started_system_answers_send_through_the_real_start() {
    let (lab, mock) = system_lab_with_endpoints(serde_json::json!({"endpoints": [
        {"method": "GET", "path": "/widgets", "port": "api", "why": "Lists widgets."}
    ]}))
    .await;
    Mock::given(path("/widgets"))
        .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
        .expect(1)
        .mount(&mock)
        .await;
    start(&lab, false).await;
    until_state(&lab, |v| v.state == SystemState::Running).await;
    let Reply::Sent(result) =
        send_endpoint(&lab.shared, &pr(7), RunSide::Pr, "GET /widgets".into())
            .await
            .unwrap_reply()
    else {
        panic!("a Sent reply")
    };
    assert_eq!((result.status, result.body.as_str()), (Some(200), "[]"));
}
