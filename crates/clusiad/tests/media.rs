mod common;

use clusia_protocol::{ClientError, Command, ErrorCode, MediaFile, Reply};
use clusiad::DaemonOptions;
use common::{TestDaemon, test_options};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0";

fn options(github_api: &str, token: Option<&str>) -> DaemonOptions {
    let mut o = test_options();
    o.github_api = Some(github_api.to_string());
    o.github_token = token.map(str::to_string);
    o.media_extra_hosts = vec!["127.0.0.1".into(), "localhost".into()];
    o
}

async fn daemon(github_api: &str, token: Option<&str>) -> TestDaemon {
    TestDaemon::start_with(tempfile::tempdir().unwrap(), options(github_api, token)).await
}

fn media(r: Result<Reply, ClientError>) -> MediaFile {
    match r {
        Ok(Reply::Media(m)) => m,
        other => panic!("expected Media, got {other:?}"),
    }
}

fn refused(r: Result<Reply, ClientError>) -> String {
    match r {
        Err(ClientError::Server(e)) => {
            assert_eq!(e.code, ErrorCode::Refused, "{}", e.message);
            e.message
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn fetch(url: impl Into<String>) -> Command {
    Command::FetchMedia { url: url.into() }
}

async fn png_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(PNG))
        .mount(&server)
        .await;
    server
}

fn auth_of(r: &Request) -> Option<String> {
    r.headers
        .get("authorization")
        .map(|v| v.to_str().unwrap().to_string())
}

fn cached_files(d: &TestDaemon) -> usize {
    std::fs::read_dir(d.paths.media_dir()).map_or(0, |rd| rd.count())
}

#[tokio::test]
async fn fetches_and_caches_a_png() {
    let server = png_server().await;
    let d = daemon("http://127.0.0.1:9", None).await;
    let mut c = d.client().await;
    let url = format!("{}/a.png", server.uri());
    let first = media(c.request(fetch(&url)).await);
    assert_eq!(first.kind, clusia_core::MediaKind::Png);
    assert_eq!(first.bytes, PNG.len() as u64);
    assert!(
        first
            .path
            .starts_with(&d.paths.media_dir().display().to_string())
    );
    assert!(first.path.ends_with(".png"));
    assert_eq!(std::fs::read(&first.path).unwrap(), PNG);
    let second = media(c.request(fetch(&url)).await);
    assert_eq!(second, first);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "the second answer comes from the cache"
    );
    d.stop().await;
}

#[tokio::test]
async fn token_never_follows_a_redirect() {
    let github = MockServer::start().await;
    let storage = png_server().await;
    Mock::given(method("GET"))
        .and(path("/user-attachments/assets/x"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "location",
            format!("http://localhost:{}/a.png", storage.address().port()),
        ))
        .mount(&github)
        .await;
    let d = daemon(&github.uri(), Some("ghp_test")).await;
    let mut c = d.client().await;
    media(
        c.request(fetch(format!("{}/user-attachments/assets/x", github.uri())))
            .await,
    );
    let at_github = github.received_requests().await.unwrap();
    assert_eq!(auth_of(&at_github[0]).as_deref(), Some("Bearer ghp_test"));
    let at_storage = storage.received_requests().await.unwrap();
    assert_eq!(at_storage.len(), 1);
    assert_eq!(
        auth_of(&at_storage[0]),
        None,
        "no credentials after the hop"
    );
    d.stop().await;
}

#[tokio::test]
async fn only_github_gets_the_token() {
    let github = png_server().await;
    let other = png_server().await;
    let d = daemon(&github.uri(), Some("ghp_test")).await;
    let mut c = d.client().await;
    media(c.request(fetch(format!("{}/a.png", github.uri()))).await);
    media(
        c.request(fetch(format!(
            "http://localhost:{}/a.png",
            other.address().port()
        )))
        .await,
    );
    let at_github = github.received_requests().await.unwrap();
    assert_eq!(auth_of(&at_github[0]).as_deref(), Some("Bearer ghp_test"));
    let at_other = other.received_requests().await.unwrap();
    assert_eq!(auth_of(&at_other[0]), None);
    d.stop().await;
}

#[tokio::test]
async fn refuses_svg_video_and_oversize() {
    let server = MockServer::start().await;
    let mut big = PNG.to_vec();
    big.resize(10 * 1024 * 1024 + 1, 0);
    for (route, body) in [
        (
            "/x.png",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".to_vec(),
        ),
        ("/v.png", b"\0\0\0\x18ftypmp42".to_vec()),
        ("/big.png", big),
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
    }
    let d = daemon("http://127.0.0.1:9", None).await;
    let mut c = d.client().await;
    let svg = refused(c.request(fetch(format!("{}/x.png", server.uri()))).await);
    assert!(svg.contains("PNG, GIF and JPEG"), "{svg}");
    refused(c.request(fetch(format!("{}/v.png", server.uri()))).await);
    let big = refused(c.request(fetch(format!("{}/big.png", server.uri()))).await);
    assert!(big.contains("10 MiB"), "{big}");
    assert_eq!(cached_files(&d), 0, "nothing refused is kept");
    d.stop().await;
}

#[tokio::test]
async fn refuses_hosts_off_the_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/hop"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", "http://evil.test/a.png"),
        )
        .mount(&server)
        .await;
    let d = daemon("http://127.0.0.1:9", None).await;
    let mut c = d.client().await;
    let host = refused(c.request(fetch("https://evil.example/a.png")).await);
    assert!(host.contains("evil.example"), "{host}");
    let plain = refused(c.request(fetch("http://github.com/a.png")).await);
    assert!(plain.contains("https"), "{plain}");
    refused(c.request(fetch("ftp://github.com/a.png")).await);
    refused(c.request(fetch("https://user:pw@github.com/a.png")).await);
    refused(c.request(fetch(format!("{}/hop", server.uri()))).await);
    match c.request(fetch("not a url")).await {
        Err(ClientError::Server(e)) => assert_eq!(e.code, ErrorCode::BadRequest),
        other => panic!("expected BadRequest, got {other:?}"),
    }
    d.stop().await;
}

#[tokio::test]
async fn redirect_loops_stop_after_five_hops() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/loop"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/loop"))
        .mount(&server)
        .await;
    let d = daemon("http://127.0.0.1:9", None).await;
    let mut c = d.client().await;
    let msg = refused(c.request(fetch(format!("{}/loop", server.uri()))).await);
    assert!(msg.contains("too many"), "{msg}");
    assert_eq!(server.received_requests().await.unwrap().len(), 6);
    d.stop().await;
}

#[tokio::test]
async fn jwt_urls_share_one_cache_entry_per_asset() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/assets/one"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(PNG))
        .mount(&server)
        .await;
    let d = daemon("http://127.0.0.1:9", None).await;
    let mut c = d.client().await;
    let a = media(
        c.request(fetch(format!("{}/assets/one?jwt=aaa", server.uri())))
            .await,
    );
    let b = media(
        c.request(fetch(format!("{}/assets/one?jwt=bbb", server.uri())))
            .await,
    );
    assert_eq!(a.path, b.path);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(cached_files(&d), 1);
    d.stop().await;
}

fn off_list_options(github_api: &str) -> DaemonOptions {
    let mut o = test_options();
    o.github_api = Some(github_api.to_string());
    o.github_token = Some("ghp_test".into());
    o.media_allow_local = true;
    o
}

async fn set_external(c: &mut clusia_protocol::Client, on: bool) {
    c.request(Command::SetConfigValue {
        key: "media.load_external_images".into(),
        value: on.to_string(),
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn external_images_follow_the_setting() {
    let site = png_server().await;
    let d = TestDaemon::start_with(
        tempfile::tempdir().unwrap(),
        off_list_options("http://localhost:9"),
    )
    .await;
    let mut c = d.client().await;
    let url = format!("{}/a.png", site.uri());
    let off = refused(c.request(fetch(&url)).await);
    assert!(off.contains("127.0.0.1"), "{off}");
    assert!(site.received_requests().await.unwrap().is_empty());
    set_external(&mut c, true).await;
    let file = media(c.request(fetch(&url)).await);
    assert_eq!(std::fs::read(&file.path).unwrap(), PNG);
    let at_site = site.received_requests().await.unwrap();
    assert_eq!(at_site.len(), 1);
    assert_eq!(
        auth_of(&at_site[0]),
        None,
        "other sites never get the token"
    );
    set_external(&mut c, false).await;
    refused(c.request(fetch(&url)).await);
    d.stop().await;
}

#[tokio::test]
async fn external_images_keep_every_other_rule() {
    let site = MockServer::start().await;
    let mut big = PNG.to_vec();
    big.resize(10 * 1024 * 1024 + 1, 0);
    for (route, body) in [
        (
            "/x.png",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".to_vec(),
        ),
        ("/big.png", big),
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&site)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/loop"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/loop"))
        .mount(&site)
        .await;
    let d = TestDaemon::start_with(
        tempfile::tempdir().unwrap(),
        off_list_options("http://localhost:9"),
    )
    .await;
    let mut c = d.client().await;
    set_external(&mut c, true).await;
    assert!(refused(c.request(fetch(format!("{}/x.png", site.uri()))).await).contains("PNG"));
    assert!(refused(c.request(fetch(format!("{}/big.png", site.uri()))).await).contains("10 MiB"));
    assert!(refused(c.request(fetch(format!("{}/loop", site.uri()))).await).contains("too many"));
    assert_eq!(cached_files(&d), 0);
    d.stop().await;
}

#[tokio::test]
async fn external_images_must_be_public_https_on_every_hop() {
    let hop = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/to-http"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", "http://images.example.test/a.png"),
        )
        .mount(&hop)
        .await;
    Mock::given(method("GET"))
        .and(path("/to-local"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", "https://127.0.0.1/a.png"),
        )
        .mount(&hop)
        .await;
    // Production rules: no plain http or local addresses for other sites.
    let mut o = test_options();
    o.media_extra_hosts = vec!["localhost".into()];
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), o).await;
    let mut c = d.client().await;
    set_external(&mut c, true).await;
    for url in [
        "http://images.example.test/a.png",
        "https://127.0.0.1/a.png",
        "https://[::1]/a.png",
        "https://nas.local/a.png",
        "https://images.example.test:8443/a.png",
        "https://user:pw@images.example.test/a.png",
    ] {
        refused(c.request(fetch(url)).await);
    }
    for route in ["/to-http", "/to-local"] {
        let port = hop.address().port();
        refused(
            c.request(fetch(format!("http://localhost:{port}{route}")))
                .await,
        );
    }
    assert_eq!(
        hop.received_requests().await.unwrap().len(),
        2,
        "one request per redirect test"
    );
    d.stop().await;
}

async fn set_github_host(c: &mut clusia_protocol::Client, host: &str) {
    c.request(Command::SetConfigValue {
        key: "github.host".into(),
        value: host.into(),
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn an_enterprise_token_never_goes_to_github_com() {
    let server = MockServer::start().await;
    for route in ["/a.png", "/b.png"] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(PNG))
            .mount(&server)
            .await;
    }
    let mut o = test_options();
    o.github_token = Some("ghp_test".into());
    o.media_extra_hosts = vec!["github.com".into()];
    o.media_resolve = vec![("github.com".into(), *server.address())];
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), o).await;
    let mut c = d.client().await;
    let port = server.address().port();
    set_github_host(&mut c, "ghe.example.com").await;
    media(
        c.request(fetch(format!("http://github.com:{port}/a.png")))
            .await,
    );
    set_github_host(&mut c, "github.com").await;
    media(
        c.request(fetch(format!("http://github.com:{port}/b.png")))
            .await,
    );
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 2);
    assert_eq!(
        auth_of(&received[0]),
        None,
        "the token belongs to ghe.example.com"
    );
    assert_eq!(auth_of(&received[1]).as_deref(), Some("Bearer ghp_test"));
    d.stop().await;
}
