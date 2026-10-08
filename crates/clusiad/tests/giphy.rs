mod common;

use std::path::Path;
use std::sync::Arc;

use clusia_platform::{MemoryStore, SecretStore};
use clusia_protocol::{Client, ClientError, Command, ErrorCode, Event, GifPage, Reply, topics};
use common::{TestDaemon, test_options};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "gk_test_123";

fn gif(id: &str, title: &str) -> Value {
    json!({
        "id": id,
        "title": title,
        "images": {
            "fixed_width_small": {
                "url": format!("https://media0.giphy.com/media/{id}/100w.gif"),
                "width": "100",
                "height": "75"
            },
            "downsized": {
                "url": format!("https://media0.giphy.com/media/{id}/giphy-downsized.gif"),
                "width": "300",
                "height": "225"
            }
        }
    })
}

async fn daemon(api: &str) -> TestDaemon {
    let mut o = test_options();
    o.giphy_api = Some(api.to_string());
    TestDaemon::start_with(tempfile::tempdir().unwrap(), o).await
}

async fn with_key(d: &TestDaemon) -> Client {
    let mut c = d.client().await;
    c.request(Command::SetGiphyKey { key: KEY.into() })
        .await
        .unwrap();
    c
}

fn search(query: &str, offset: u32) -> Command {
    Command::SearchGifs {
        query: query.into(),
        offset,
    }
}

fn page(r: Result<Reply, ClientError>) -> GifPage {
    match r {
        Ok(Reply::Gifs(p)) => p,
        other => panic!("expected Gifs, got {other:?}"),
    }
}

async fn configured(c: &mut Client) -> bool {
    match c.request(Command::GiphyKeyStatus).await {
        Ok(Reply::GiphyKeyStatus(s)) => s.configured,
        other => panic!("expected GiphyKeyStatus, got {other:?}"),
    }
}

fn error(r: Result<Reply, ClientError>) -> (ErrorCode, String) {
    match r {
        Err(ClientError::Server(e)) => (e.code, e.message),
        other => panic!("expected a server error, got {other:?}"),
    }
}

#[tokio::test]
async fn search_maps_renditions() {
    let server = MockServer::start().await;
    let first: Vec<Value> = (0..24)
        .map(|i| gif(&format!("g{i}"), " Happy cat "))
        .collect();
    Mock::given(method("GET"))
        .and(path("/v1/gifs/search"))
        .and(query_param("api_key", KEY))
        .and(query_param("q", "happy cat"))
        .and(query_param("limit", "24"))
        .and(query_param("offset", "0"))
        .and(query_param("rating", "pg-13"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": first,
            "pagination": { "total_count": 60, "count": 24, "offset": 0 }
        })))
        .mount(&server)
        .await;
    let mut last: Vec<Value> = (48..59).map(|i| gif(&format!("g{i}"), "End")).collect();
    last.push(json!({ "id": "broken", "title": "no renditions", "images": {} }));
    Mock::given(method("GET"))
        .and(path("/v1/gifs/search"))
        .and(query_param("offset", "48"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": last,
            "pagination": { "total_count": 60, "count": 12, "offset": 48 }
        })))
        .mount(&server)
        .await;
    let d = daemon(&server.uri()).await;
    let mut c = with_key(&d).await;
    let p = page(c.request(search("  happy cat ", 0)).await);
    assert_eq!(p.items.len(), 24);
    assert_eq!(p.next_offset, Some(24));
    let g = &p.items[0];
    assert_eq!(g.id, "g0");
    assert_eq!(g.title, "Happy cat");
    assert_eq!(g.preview_url, "https://media0.giphy.com/media/g0/100w.gif");
    assert_eq!(
        g.url,
        "https://media0.giphy.com/media/g0/giphy-downsized.gif"
    );
    assert_eq!((g.width, g.height), (100, 75));
    let end = page(c.request(search("happy cat", 48)).await);
    assert_eq!(
        end.items.len(),
        11,
        "an entry without renditions is dropped"
    );
    assert_eq!(end.next_offset, None);
    d.stop().await;
}

#[tokio::test]
async fn empty_query_is_trending() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/gifs/trending"))
        .and(query_param("api_key", KEY))
        .and(query_param("rating", "pg-13"))
        .and(query_param_is_missing("q"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [gif("t1", "Trending")],
            "pagination": { "total_count": 1, "count": 1, "offset": 0 }
        })))
        .mount(&server)
        .await;
    let d = daemon(&server.uri()).await;
    let mut c = with_key(&d).await;
    for q in ["", "   "] {
        let p = page(c.request(search(q, 0)).await);
        assert_eq!(p.items[0].id, "t1");
        assert_eq!(p.next_offset, None);
    }
    d.stop().await;
}

#[tokio::test]
async fn no_key_is_not_configured() {
    let server = MockServer::start().await;
    let d = daemon(&server.uri()).await;
    let mut c = d.client().await;
    let (code, message) = error(c.request(search("cat", 0)).await);
    assert_eq!(code, ErrorCode::NotConfigured);
    assert!(message.contains("Config › Media"), "{message}");
    assert!(server.received_requests().await.unwrap().is_empty());
    c.request(Command::SetGiphyKey { key: KEY.into() })
        .await
        .unwrap();
    c.request(Command::ClearGiphyKey).await.unwrap();
    assert_eq!(
        error(c.request(search("cat", 0)).await).0,
        ErrorCode::NotConfigured
    );
    d.stop().await;
}

#[tokio::test]
async fn giphy_errors_map_to_codes() {
    let server = MockServer::start().await;
    for (q, status, body) in [
        ("denied", 401, "{}"),
        ("forbidden", 403, "{}"),
        ("slow", 429, "{}"),
        ("boom", 500, "{}"),
        ("junk", 200, "<html>not json</html>"),
        ("shape", 200, r#"{"data":"nope"}"#),
    ] {
        Mock::given(method("GET"))
            .and(path("/v1/gifs/search"))
            .and(query_param("q", q))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .mount(&server)
            .await;
    }
    let d = daemon(&server.uri()).await;
    let mut c = with_key(&d).await;
    for (q, code) in [
        ("denied", ErrorCode::Unauthorized),
        ("forbidden", ErrorCode::Unauthorized),
        ("slow", ErrorCode::RateLimited),
        ("boom", ErrorCode::Upstream),
        ("junk", ErrorCode::Upstream),
        ("shape", ErrorCode::Upstream),
    ] {
        assert_eq!(error(c.request(search(q, 0)).await).0, code, "{q}");
    }
    assert_eq!(
        error(c.request(search("denied", 0)).await).1,
        "Giphy rejected the key"
    );
    let long = "x".repeat(201);
    assert_eq!(
        error(c.request(search(&long, 0)).await).0,
        ErrorCode::BadRequest
    );
    d.stop().await;
}

#[tokio::test]
async fn setting_and_clearing_the_key_tells_subscribers() {
    let store = Arc::new(MemoryStore::default());
    let mut o = test_options();
    o.secrets = store.clone();
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), o).await;
    let mut watcher = d.client().await;
    watcher
        .request(Command::Subscribe {
            topics: vec![topics::CONFIG.into()],
        })
        .await
        .unwrap();
    let mut c = d.client().await;
    c.request(Command::SetGiphyKey {
        key: " gk_a ".into(),
    })
    .await
    .unwrap();
    assert_eq!(store.get("giphy").unwrap().as_deref(), Some("gk_a"));
    assert!(configured(&mut c).await);
    let heard = |event| {
        let (topic, event): (String, Event) = event;
        assert_eq!(topic, topics::CONFIG);
        assert_eq!(event, Event::GiphyKeyChanged);
    };
    heard(
        tokio::time::timeout(common::EVENT_WAIT, watcher.next_event())
            .await
            .unwrap()
            .unwrap(),
    );
    for bad in ["", "  ", "two words"] {
        assert_eq!(
            error(c.request(Command::SetGiphyKey { key: bad.into() }).await).0,
            ErrorCode::BadRequest,
            "{bad:?}"
        );
    }
    assert_eq!(store.get("giphy").unwrap().as_deref(), Some("gk_a"));
    c.request(Command::ClearGiphyKey).await.unwrap();
    assert_eq!(store.get("giphy").unwrap(), None);
    assert!(!configured(&mut c).await);
    heard(
        tokio::time::timeout(common::EVENT_WAIT, watcher.next_event())
            .await
            .unwrap()
            .unwrap(),
    );
    d.stop().await;
}

fn files_under(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.is_dir() {
            files_under(&p, out);
        } else {
            out.push(p);
        }
    }
}

#[tokio::test]
async fn key_never_appears_in_replies_or_logs() {
    let secret = "gk_secret_987";
    let broken = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&broken)
        .await;
    let mut seen = Vec::new();
    for api in [broken.uri(), "http://127.0.0.1:9".to_string()] {
        let d = daemon(&api).await;
        let mut c = d.client().await;
        seen.push(format!(
            "{:?}",
            c.request(Command::SetGiphyKey { key: secret.into() }).await
        ));
        seen.push(format!("{:?}", c.request(search("cat", 0)).await));
        seen.push(format!("{:?}", c.request(Command::GetConfig).await));
        let dir = d.stop().await;
        let mut files = Vec::new();
        files_under(dir.path(), &mut files);
        for f in files {
            let bytes = std::fs::read(&f).unwrap_or_default();
            assert!(
                !String::from_utf8_lossy(&bytes).contains(secret),
                "the key leaked into {}",
                f.display()
            );
        }
    }
    for text in seen {
        assert!(!text.contains(secret), "{text}");
    }
}

#[tokio::test]
async fn giphy_key_status_follows_the_stored_key() {
    let store = Arc::new(MemoryStore::default());
    let mut o = test_options();
    o.secrets = store.clone();
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), o).await;
    let mut c = d.client().await;
    assert!(!configured(&mut c).await, "nothing stored");
    store.set("giphy", "gk_status_1").unwrap();
    assert!(configured(&mut c).await);
    let reply = c.request(Command::GiphyKeyStatus).await;
    assert!(
        !format!("{reply:?}").contains("gk_status_1"),
        "the key is never sent"
    );
    store.set("giphy", "   ").unwrap();
    assert!(!configured(&mut c).await, "a blank key is no key");
    store.delete("giphy").unwrap();
    assert!(!configured(&mut c).await);
    d.stop().await;
}
