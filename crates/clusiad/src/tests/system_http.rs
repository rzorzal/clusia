//! The HTTP client of a review's system: health checks now, Send too. It reaches only a mapped
//! `127.0.0.1` port of this review's live system, follows no redirect and uses no proxy.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use clusia_core::PrRef;
use clusia_core::cockpit::RunSide;
use clusia_core::printable::{is_invisible, printable};
use clusia_core::system::{
    HEALTH_REQUEST_SECS, MAIN_SYSTEM_LATER, RESERVED_HEADERS, SEND_BODY_MAX, SEND_CONNECT_SECS,
    SEND_GUARD, SEND_HEADER_TEXT_MAX, SEND_HEADERS_MAX, SEND_TIMEOUT_TEXT, SendRequest, SendResult,
};

use super::system::mapped_host_ports;
use crate::state::Shared;
use crate::sync::now_unix;

/// The one builder of the system's client: no proxy (the daemon's `HTTP_PROXY` must never see
/// a system's traffic), no redirect, HTTP/1 only, and a bounded connect. Every request sets
/// its own timeout.
pub(super) fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .connect_timeout(Duration::from_secs(SEND_CONNECT_SECS))
        .build()
        .expect("a plain HTTP client builds")
}

pub(super) fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(build_client)
}

/// Whether `target` may follow `http://127.0.0.1:<port>` as it is: a path from the root, with
/// nothing that could move the authority or be read differently by the URL parser.
fn plain_target(target: &str) -> bool {
    target.starts_with('/')
        && !target.starts_with("//")
        && !target
            .chars()
            .any(|c| c == '\\' || c == '#' || c == ' ' || c.is_control())
}

/// The URL of `target` on `port`, when `port` is a mapped host port of this review's live
/// system. `target` may come from agent text, so it is checked before it is parsed and the
/// parsed URL is checked again.
pub(super) fn guard(
    shared: &Shared,
    pr: &PrRef,
    side: RunSide,
    port: u16,
    target: &str,
) -> Result<reqwest::Url, String> {
    if side == RunSide::Main {
        return Err(MAIN_SYSTEM_LATER.to_string());
    }
    if !plain_target(target) {
        return Err(SEND_GUARD.to_string());
    }
    let url = reqwest::Url::parse(&format!("http://127.0.0.1:{port}{target}"))
        .map_err(|_| SEND_GUARD.to_string())?;
    let loopback = url.host_str() == Some("127.0.0.1")
        && url.port() == Some(port)
        && url.username().is_empty()
        && url.password().is_none();
    if !loopback || !mapped_host_ports(shared, pr).contains(&port) {
        return Err(SEND_GUARD.to_string());
    }
    Ok(url)
}

/// Sends `request` to this review's system and answers what came back; every failure is a
/// result with no status and its error.
pub(super) async fn send(
    shared: &Shared,
    pr: &PrRef,
    endpoint: &str,
    request: SendRequest,
) -> SendResult {
    let mut result = SendResult {
        endpoint: endpoint.to_string(),
        method: request.method,
        url: format!("http://127.0.0.1:{}{}", request.port, request.target),
        status: None,
        reason: None,
        took_ms: 0,
        headers: Vec::new(),
        body: String::new(),
        body_bytes: 0,
        truncated: false,
        error: None,
        at: now_unix(),
    };
    let url = match guard(shared, pr, RunSide::Pr, request.port, &request.target) {
        Ok(url) => url,
        Err(refusal) => {
            result.error = Some(refusal);
            return result;
        }
    };
    let method = reqwest::Method::from_bytes(request.method.as_str().as_bytes())
        .unwrap_or(reqwest::Method::GET);
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in &request.headers {
        if RESERVED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
            continue;
        }
        // The endpoint checks refuse what no header can hold; anything left is skipped.
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
    let mut builder = client()
        .request(method, url)
        .headers(headers)
        .timeout(shared.tests.send_timeout());
    if let Some(body) = request.body {
        builder = builder.body(body);
    }
    let began = Instant::now();
    let read = async {
        let mut response = builder.send().await?;
        let status = response.status();
        let headers = response
            .headers()
            .iter()
            .take(SEND_HEADERS_MAX)
            .map(|(name, value)| {
                (
                    header_text(name.as_str()),
                    header_text(&String::from_utf8_lossy(value.as_bytes())),
                )
            })
            .collect::<Vec<_>>();
        let mut body: Vec<u8> = Vec::new();
        let mut truncated = false;
        while let Some(chunk) = response.chunk().await? {
            let room = SEND_BODY_MAX - body.len();
            if chunk.len() > room {
                body.extend_from_slice(&chunk[..room]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        Ok::<_, reqwest::Error>((status, headers, body, truncated))
    };
    match read.await {
        Ok((status, headers, body, truncated)) => {
            result.status = Some(status.as_u16());
            result.reason = status.canonical_reason().map(printable);
            result.headers = headers;
            result.body_bytes = body.len() as u64;
            result.body = clean_body(&body);
            result.truncated = truncated;
        }
        Err(e) if e.is_timeout() => result.error = Some(SEND_TIMEOUT_TEXT.to_string()),
        Err(e) => {
            let reason: String = printable(&error_chain(&e)).chars().take(200).collect();
            result.error = Some(format!("Could not connect: {reason}"));
        }
    }
    result.took_ms = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
    result
}

/// A response header's name or value, safe to show and bounded.
fn header_text(text: &str) -> String {
    printable(text).chars().take(SEND_HEADER_TEXT_MAX).collect()
}

/// The body as text: invalid UTF-8 replaced, and every control character but a line break or
/// a tab, and every invisible one, dropped.
fn clean_body(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|&c| !((c.is_control() && c != '\n' && c != '\t') || is_invisible(c)))
        .collect()
}

/// An error with its causes, which say what went wrong with the connection.
fn error_chain(e: &reqwest::Error) -> String {
    let mut text = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// One health check: `GET` on `path` of the mapped `host_port`. Healthy is 200 to 399; the text
/// is the status, or "no answer" when nothing answered in time. The body is never read.
pub(super) async fn check(
    shared: &Shared,
    pr: &PrRef,
    host_port: u16,
    path: &str,
) -> (bool, String) {
    let no_answer = || (false, "no answer".to_string());
    let Ok(url) = guard(shared, pr, RunSide::Pr, host_port, path) else {
        return no_answer();
    };
    let sent = client()
        .get(url)
        .timeout(Duration::from_secs(HEALTH_REQUEST_SECS))
        .send()
        .await;
    match sent {
        Ok(response) => {
            let status = response.status().as_u16();
            ((200..=399).contains(&status), status.to_string())
        }
        Err(_) => no_answer(),
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::tests::lab::pr;
    use crate::tests::system_lab::{stop_live_system, store_live_system, system_lab};

    /// Runs this test binary again for `no_proxy_child`, with every proxy variable pointing at
    /// a listener held here: the child's request must never reach it. The variables live only
    /// in the child's environment.
    #[test]
    fn the_client_ignores_the_proxy_environment() {
        let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        proxy.set_nonblocking(true).unwrap();
        let url = format!("http://{}", proxy.local_addr().unwrap());
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "tests::system_http::tests::no_proxy_child",
                "--ignored",
                "--nocapture",
            ])
            .env("CLUSIA_NO_PROXY_CHILD", "1")
            .env_remove("NO_PROXY")
            .env_remove("no_proxy");
        for name in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
            child.env(name, &url);
        }
        let out = child.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("1 passed"), "the child ran: {stdout}");
        assert_eq!(
            proxy.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "the fake proxy never saw a connection"
        );
    }

    /// The child half of `the_client_ignores_the_proxy_environment`: does nothing unless that
    /// test started it.
    #[tokio::test]
    #[ignore]
    async fn no_proxy_child() {
        if std::env::var("CLUSIA_NO_PROXY_CHILD").as_deref() != Ok("1") {
            return;
        }
        let mock = MockServer::start().await;
        Mock::given(path("/x"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock)
            .await;
        let answer = build_client()
            .get(format!("http://127.0.0.1:{}/x", mock.address().port()))
            .timeout(Duration::from_secs(5))
            .send()
            .await;
        assert_eq!(answer.unwrap().status(), 200);
    }

    #[tokio::test]
    async fn the_guard_accepts_only_the_mapped_loopback_port_of_this_review() {
        let (lab, api) = system_lab(vec![]).await;
        let port = api.address().port();
        store_live_system(&lab, 7, &[port]);
        let ok = guard(&lab.shared, &pr(7), RunSide::Pr, port, "/health").unwrap();
        assert_eq!(ok.as_str(), format!("http://127.0.0.1:{port}/health"));
        // Another port of this machine, another review, main's side and a down system.
        assert_eq!(
            guard(&lab.shared, &pr(7), RunSide::Pr, port + 1, "/").unwrap_err(),
            SEND_GUARD
        );
        assert_eq!(
            guard(&lab.shared, &pr(5), RunSide::Pr, port, "/").unwrap_err(),
            SEND_GUARD
        );
        assert_eq!(
            guard(&lab.shared, &pr(7), RunSide::Main, port, "/").unwrap_err(),
            MAIN_SYSTEM_LATER
        );
        stop_live_system(&lab, 7);
        assert_eq!(
            guard(&lab.shared, &pr(7), RunSide::Pr, port, "/").unwrap_err(),
            SEND_GUARD
        );
    }

    #[tokio::test]
    async fn a_target_cannot_change_the_host_or_the_port() {
        // `target` is agent-derived text: every one of these is refused before any URL is
        // parsed, and the parsed URL is checked for host, port and user info too.
        let (lab, api) = system_lab(vec![]).await;
        let port = api.address().port();
        store_live_system(&lab, 7, &[port]);
        for target in [
            "@evil.example/",
            "//evil.example/",
            ":9/",
            "/\\@evil.example/",
            ".evil.example/",
            "",
            "x",
            "/a b",
            "/a#b",
            "/a\u{7}b",
            "/a\\b",
        ] {
            let r = guard(&lab.shared, &pr(7), RunSide::Pr, port, target);
            assert_eq!(r.unwrap_err(), SEND_GUARD, "{target:?}");
        }
        let ok = guard(&lab.shared, &pr(7), RunSide::Pr, port, "/a/b?x=1").unwrap();
        assert_eq!((ok.host_str(), ok.port()), (Some("127.0.0.1"), Some(port)));
        assert!(ok.username().is_empty() && ok.password().is_none());
    }

    #[tokio::test]
    async fn the_client_follows_no_redirect() {
        let (lab, api) = system_lab(vec![]).await;
        let port = api.address().port();
        store_live_system(&lab, 7, &[port]);
        let other = MockServer::start().await;
        Mock::given(path("/redirect"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", format!("{}/x", other.uri())),
            )
            .mount(&api)
            .await;
        // A 3xx is shown as it is, never followed; 200 to 399 counts as healthy.
        let (healthy, text) = check(&lab.shared, &pr(7), port, "/redirect").await;
        assert!(healthy);
        assert_eq!(text, "302");
        assert_eq!(other.received_requests().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn check_reports_the_status_or_no_answer() {
        let (lab, api) = system_lab(vec![]).await;
        let port = api.address().port();
        // A port nothing listens on any more.
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        store_live_system(&lab, 7, &[port, closed]);
        Mock::given(path("/down"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&api)
            .await;
        assert_eq!(
            check(&lab.shared, &pr(7), port, "/health").await,
            (true, "200".to_string())
        );
        assert_eq!(
            check(&lab.shared, &pr(7), port, "/down").await,
            (false, "503".to_string())
        );
        // Nothing listens on the port: refused.
        assert_eq!(
            check(&lab.shared, &pr(7), closed, "/health").await,
            (false, "no answer".to_string())
        );
        // A port that is not mapped is never asked.
        assert_eq!(
            check(&lab.shared, &pr(7), port + 1, "/health").await,
            (false, "no answer".to_string())
        );
    }
}
