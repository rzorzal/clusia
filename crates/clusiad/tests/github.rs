mod common;

use std::time::Duration;

use clusia_core::PrFilter;
use clusia_protocol::{Command, Event, Reply, SyncState, SyncStatus, topics};
use common::{TestDaemon, test_options};
use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ASSIGNED_Q: &str = "is:pr is:open archived:false review-requested:@me";
const MINE_Q: &str = "is:pr is:open archived:false author:@me";

fn issue(number: u64, repo: &str) -> serde_json::Value {
    json!({
        "number": number, "title": format!("PR {number}"),
        "html_url": format!("https://github.com/{repo}/pull/{number}"),
        "user": { "login": "maria" }, "draft": false, "updated_at": "2026-10-01T12:00:00Z",
        "comments": 0, "repository_url": format!("https://api.github.com/repos/{repo}")
    })
}

async fn mount_lists(
    server: &MockServer,
    assigned: Vec<serde_json::Value>,
    mine: Vec<serde_json::Value>,
    times: Option<u64>,
) {
    for (q, items) in [(ASSIGNED_Q, assigned), (MINE_Q, mine)] {
        let mock = Mock::given(method("GET"))
            .and(path("/search/issues"))
            .and(query_param("q", q))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": items })));
        match times {
            Some(n) => mock.up_to_n_times(n).mount(server).await,
            None => mock.mount(server).await,
        }
    }
}

async fn daemon_for(server: &MockServer, token: Option<&str>) -> TestDaemon {
    let mut options = test_options();
    options.github_api = Some(server.uri());
    options.github_token = token.map(str::to_string);
    TestDaemon::start_with(tempfile::tempdir().unwrap(), options).await
}

fn sync_of(reply: Reply) -> SyncStatus {
    match reply {
        Reply::Sync(s) => s,
        other => panic!("expected Sync, got {other:?}"),
    }
}

fn prs_of(reply: Reply) -> Vec<clusia_core::PrSummary> {
    match reply {
        Reply::Prs(p) => p,
        other => panic!("expected Prs, got {other:?}"),
    }
}

#[tokio::test]
async fn list_prs_is_empty_before_the_first_sync() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    assert!(
        prs_of(
            c.request(Command::ListPrs {
                filter: PrFilter::Assigned
            })
            .await
            .unwrap()
        )
        .is_empty()
    );
    assert_eq!(
        sync_of(c.request(Command::GetSyncStatus).await.unwrap()).state,
        SyncState::NotYet
    );
    d.stop().await;
}

#[tokio::test]
async fn sync_now_fetches_both_lists() {
    let server = MockServer::start().await;
    mount_lists(
        &server,
        vec![issue(7, "acme/widgets")],
        vec![issue(1, "me/a"), issue(2, "me/b")],
        None,
    )
    .await;
    let d = daemon_for(&server, Some("tok")).await;
    let mut c = d.client().await;
    let s = sync_of(c.request(Command::SyncNow).await.unwrap());
    assert_eq!(s.state, SyncState::Online);
    assert_eq!(s.next_sync_unix.unwrap() - s.last_sync_unix.unwrap(), 60);
    let assigned = prs_of(
        c.request(Command::ListPrs {
            filter: PrFilter::Assigned,
        })
        .await
        .unwrap(),
    );
    assert_eq!(assigned.len(), 1);
    assert_eq!(assigned[0].pr, "acme/widgets#7".parse().unwrap());
    assert_eq!(
        prs_of(
            c.request(Command::ListPrs {
                filter: PrFilter::Mine
            })
            .await
            .unwrap()
        )
        .len(),
        2
    );
    d.stop().await;
}

#[tokio::test]
async fn prs_updated_is_published_only_on_change() {
    let server = MockServer::start().await;
    mount_lists(
        &server,
        vec![issue(7, "acme/widgets")],
        vec![issue(1, "me/a"), issue(2, "me/b")],
        None,
    )
    .await;
    let d = daemon_for(&server, Some("tok")).await;
    let mut watcher = d.client().await;
    watcher
        .request(Command::Subscribe {
            topics: vec![topics::PRS.into()],
        })
        .await
        .unwrap();
    let mut c = d.client().await;
    c.request(Command::SyncNow).await.unwrap();
    let (topic, event) = tokio::time::timeout(Duration::from_secs(2), watcher.next_event())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (topic.as_str(), event),
        (
            topics::PRS,
            Event::PrsUpdated {
                assigned: 1,
                mine: 2
            }
        )
    );
    c.request(Command::SyncNow).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), watcher.next_event())
            .await
            .is_err()
    );
    d.stop().await;
}

#[tokio::test]
async fn unauthorized_without_token() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let s = sync_of(c.request(Command::SyncNow).await.unwrap());
    assert_eq!(s.state, SyncState::Unauthorized);
    assert!(s.message.unwrap().contains("auth login"));
    c.request(Command::DaemonStatus).await.unwrap();
    d.stop().await;
}

#[tokio::test]
async fn github_401_is_unauthorized() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let d = daemon_for(&server, Some("bad")).await;
    let s = sync_of(d.client().await.request(Command::SyncNow).await.unwrap());
    assert_eq!(s.state, SyncState::Unauthorized);
    d.stop().await;
}

#[tokio::test]
async fn rate_limited_sets_next_sync() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "90"))
        .mount(&server)
        .await;
    let d = daemon_for(&server, Some("tok")).await;
    let s = sync_of(d.client().await.request(Command::SyncNow).await.unwrap());
    assert_eq!(s.state, SyncState::RateLimited);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!((85..=91).contains(&(s.next_sync_unix.unwrap() - now)));
    d.stop().await;
}

#[tokio::test]
async fn cached_lists_survive_failures() {
    let server = MockServer::start().await;
    mount_lists(
        &server,
        vec![issue(7, "acme/widgets")],
        vec![issue(1, "me/a")],
        Some(1),
    )
    .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({ "message": "boom" })))
        .with_priority(10)
        .mount(&server)
        .await;
    let d = daemon_for(&server, Some("tok")).await;
    let mut c = d.client().await;
    assert_eq!(
        sync_of(c.request(Command::SyncNow).await.unwrap()).state,
        SyncState::Online
    );
    assert_eq!(
        sync_of(c.request(Command::SyncNow).await.unwrap()).state,
        SyncState::Offline
    );
    assert_eq!(
        prs_of(
            c.request(Command::ListPrs {
                filter: PrFilter::Assigned
            })
            .await
            .unwrap()
        )
        .len(),
        1
    );
    assert_eq!(
        prs_of(
            c.request(Command::ListPrs {
                filter: PrFilter::Mine
            })
            .await
            .unwrap()
        )
        .len(),
        1
    );
    d.stop().await;
}

#[tokio::test]
async fn sync_changes_are_published() {
    let d = TestDaemon::start().await;
    let mut watcher = d.client().await;
    watcher
        .request(Command::Subscribe {
            topics: vec![topics::SYNC.into()],
        })
        .await
        .unwrap();
    d.client().await.request(Command::SyncNow).await.unwrap();
    let (topic, event) = tokio::time::timeout(Duration::from_secs(2), watcher.next_event())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(topic, topics::SYNC);
    assert!(matches!(
        event,
        Event::SyncChanged(SyncStatus {
            state: SyncState::Unauthorized,
            ..
        })
    ));
    d.stop().await;
}
