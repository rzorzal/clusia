//! The HTTP client of a review's system: health checks now, Send too. It reaches only a mapped
//! `127.0.0.1` port of this review's live system, follows no redirect and uses no proxy.

use std::sync::OnceLock;
use std::time::Duration;

use clusia_core::PrRef;
use clusia_core::cockpit::RunSide;
use clusia_core::system::{
    HEALTH_REQUEST_SECS, MAIN_SYSTEM_LATER, SEND_CONNECT_SECS, SEND_GUARD, SEND_TIMEOUT_SECS,
};

use super::system::mapped_host_ports;
use crate::state::Shared;

/// The one builder of the system's client: no proxy (the daemon's `HTTP_PROXY` must never see
/// a system's traffic), no redirect, HTTP/1 only, and bounded waits.
pub(super) fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .connect_timeout(Duration::from_secs(SEND_CONNECT_SECS))
        .timeout(Duration::from_secs(SEND_TIMEOUT_SECS))
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
