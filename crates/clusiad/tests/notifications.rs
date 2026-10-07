mod common;

use std::time::Duration;

use clusia_core::OpenTarget;
use clusia_core::config::EventKind;
use clusia_protocol::{Client, Command, Event, PermissionStatus, Reply, topics};
use clusiad::DaemonOptions;
use common::git_fixture::advance_pr;
use common::github_mock::{PrMock, mount_pr};
use common::review_world::{open, pr7, world};
use common::{TestDaemon, test_options};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn options(server: &MockServer) -> DaemonOptions {
    let mut options = test_options();
    options.github_api = Some(server.uri());
    options.github_token = Some("tok".into());
    options
}

fn pr_item() -> Value {
    json!({
        "number": 7,
        "title": "Add feature",
        "html_url": "https://github.com/acme/widgets/pull/7",
        "user": { "login": "maria" },
        "draft": false,
        "updated_at": "2026-10-01T12:00:00Z",
        "comments": 0,
        "repository_url": "https://api.github.com/repos/acme/widgets"
    })
}

/// A feed entry about acme/widgets#7.
fn entry(id: &str, reason: &str, comment: Option<u64>) -> Value {
    json!({
        "id": id,
        "reason": reason,
        "unread": true,
        "updated_at": "2026-10-01T12:00:00Z",
        "subject": {
            "title": "Add feature",
            "url": "https://api.github.com/repos/acme/widgets/pulls/7",
            "latest_comment_url": comment.map(|id| format!("https://api.github.com/repos/acme/widgets/issues/comments/{id}")),
            "type": "PullRequest"
        },
        "repository": { "full_name": "acme/widgets" }
    })
}

/// A mock GitHub whose PR lists are empty and whose feed is `feed`.
async fn github(feed: Vec<Value>) -> MockServer {
    let server = MockServer::start().await;
    mount_lists(&server, &[]).await;
    mount_feed(&server, feed).await;
    mount_pr(&server, &PrMock::new("h", "b", "/nonexistent")).await;
    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/acme/widgets/issues/comments/\d+$"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "user": { "login": "octo" } })),
        )
        .mount(&server)
        .await;
    server
}

/// `/search/issues`: `mine` for your own pull requests, nothing assigned.
async fn mount_lists(server: &MockServer, mine: &[Value]) {
    Mock::given(method("GET"))
        .and(path("/search/issues"))
        .and(query_param("q", "is:pr is:open archived:false author:@me"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "total_count": mine.len(), "items": mine })),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/search/issues"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "total_count": 0, "items": [] })),
        )
        .mount(server)
        .await;
}

async fn mount_feed(server: &MockServer, feed: Vec<Value>) {
    Mock::given(method("GET"))
        .and(path("/notifications"))
        .respond_with(ResponseTemplate::new(200).set_body_json(feed))
        .mount(server)
        .await;
}

/// Your pull request #7 with `runs` as its check runs.
async fn mount_ci(server: &MockServer, runs: Value) {
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/commits/h/check-runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(runs))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/commits/h/status"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "state": "pending", "total_count": 0, "statuses": [] })),
        )
        .mount(server)
        .await;
    mount_lists(server, &[pr_item()]).await;
    mount_feed(server, vec![]).await;
    mount_pr(server, &PrMock::new("h", "b", "/nonexistent")).await;
}

/// A client that listens to what the tray would hear.
async fn tray(d: &TestDaemon) -> Client {
    let mut c = d.client().await;
    c.request(Command::Subscribe {
        topics: vec![topics::TRAY.into()],
    })
    .await
    .unwrap();
    c
}

/// Everything the tray hears until the daemon has been quiet for a moment.
async fn heard(c: &mut Client) -> Vec<Event> {
    let mut events = Vec::new();
    while let Ok(Ok((_, event))) =
        tokio::time::timeout(Duration::from_millis(400), c.next_event()).await
    {
        events.push(event);
    }
    events
}

fn banners(events: &[Event]) -> Vec<&Event> {
    events
        .iter()
        .filter(|e| matches!(e, Event::Notify { .. }))
        .collect()
}

async fn sync(c: &mut Client) {
    c.request(Command::SyncNow).await.unwrap();
}

async fn inbox(c: &mut Client) -> Vec<clusia_protocol::InboxItem> {
    match c.request(Command::GetInbox).await.unwrap() {
        Reply::Inbox(items) => items,
        other => panic!("{other:?}"),
    }
}

async fn feed_requests(server: &MockServer) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == "/notifications")
        .collect()
}

#[tokio::test]
async fn a_review_request_reaches_the_tray_and_the_inbox() {
    let server = github(vec![entry("1", "review_requested", None)]).await;
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), options(&server)).await;
    let mut listener = tray(&d).await;
    let mut c = d.client().await;

    sync(&mut c).await;
    assert!(
        heard(&mut listener).await.is_empty(),
        "the first look only marks the starting point"
    );
    assert!(feed_requests(&server).await.is_empty());

    sync(&mut c).await;
    let events = heard(&mut listener).await;
    match banners(&events)[..] {
        [
            Event::Notify {
                title,
                subtitle,
                body,
                sound,
                open,
                ..
            },
        ] => {
            assert_eq!(title, "Review requested");
            assert_eq!(subtitle, "acme/widgets #7");
            assert_eq!(
                body,
                "@maria asked you to review acme/widgets #7 · Add feature"
            );
            assert_eq!(sound.as_deref(), Some("leaf"));
            assert_eq!(
                open,
                &OpenTarget::Review {
                    pr: pr7(),
                    thread: None
                }
            );
        }
        ref other => panic!("{other:?}"),
    }
    assert!(events.contains(&Event::InboxChanged { unseen: 1 }));
    let requests = feed_requests(&server).await;
    assert_eq!(requests.len(), 1);
    let query = requests[0].url.query().unwrap();
    assert!(
        query.contains("participating=true") && query.contains("since="),
        "{query}"
    );

    let items = inbox(&mut c).await;
    assert_eq!(items.len(), 1);
    assert_eq!(
        (items[0].kind, items[0].seen),
        (EventKind::ReviewRequested, false)
    );
    c.request(Command::MarkInboxSeen { ids: vec![] })
        .await
        .unwrap();
    assert!(
        heard(&mut listener)
            .await
            .contains(&Event::InboxChanged { unseen: 0 })
    );
    assert!(inbox(&mut c).await[0].seen);
    d.stop().await;
}

#[tokio::test]
async fn restart_never_renotifies() {
    let server = github(vec![entry("1", "review_requested", None)]).await;
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), options(&server)).await;
    let mut c = d.client().await;
    sync(&mut c).await;
    sync(&mut c).await;
    assert_eq!(inbox(&mut c).await.len(), 1);
    drop(c);
    let dir = d.stop().await;

    let d = TestDaemon::start_with(dir, options(&server)).await;
    let mut listener = tray(&d).await;
    let mut c = d.client().await;
    sync(&mut c).await;
    sync(&mut c).await;
    let events = heard(&mut listener).await;
    assert!(events.is_empty(), "nothing is said twice: {events:?}");
    assert_eq!(
        feed_requests(&server).await.len(),
        2,
        "the feed was read again after the restart, once within the minute"
    );
    let items = inbox(&mut c).await;
    assert_eq!(items.len(), 1, "the inbox came back from the file");
    d.stop().await;
}

#[tokio::test]
async fn a_burst_becomes_one_notification() {
    let feed: Vec<Value> = (0..20)
        .map(|n| entry(&format!("t{n}"), "mention", Some(100 + n)))
        .collect();
    let server = github(feed).await;
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), options(&server)).await;
    let mut listener = tray(&d).await;
    let mut c = d.client().await;
    sync(&mut c).await;
    sync(&mut c).await;
    let events = heard(&mut listener).await;
    match banners(&events)[..] {
        [Event::Notify { title, body, .. }] => {
            assert_eq!(title, "20 updates on #7");
            assert_eq!(body, "@octo mentioned you");
        }
        ref other => panic!("{other:?}"),
    }
    assert_eq!(inbox(&mut c).await.len(), 20);
    assert!(events.contains(&Event::InboxChanged { unseen: 20 }));
    d.stop().await;
}

#[tokio::test]
async fn the_same_notification_in_every_poll_is_announced_once() {
    let server = github(vec![entry("1", "mention", Some(500))]).await;
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), options(&server)).await;
    let mut listener = tray(&d).await;
    let mut c = d.client().await;
    for _ in 0..4 {
        sync(&mut c).await;
    }
    let events = heard(&mut listener).await;
    assert_eq!(banners(&events).len(), 1, "{events:?}");
    assert_eq!(inbox(&mut c).await.len(), 1);
    d.stop().await;
}

#[tokio::test]
async fn a_mention_names_who_wrote_it() {
    let server = github(vec![entry("1", "mention", Some(991))]).await;
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), options(&server)).await;
    let mut listener = tray(&d).await;
    let mut c = d.client().await;
    sync(&mut c).await;
    sync(&mut c).await;
    match banners(&heard(&mut listener).await)[..] {
        [Event::Notify { title, open, .. }] => {
            assert_eq!(title, "@octo mentioned you");
            assert_eq!(
                open,
                &OpenTarget::Review {
                    pr: pr7(),
                    thread: None
                }
            );
        }
        ref other => panic!("{other:?}"),
    }
    d.stop().await;
}

#[tokio::test]
async fn checks_failed_goes_to_the_tray_list_when_ci_turns_red() {
    let server = MockServer::start().await;
    let runs = |conclusion: Value, status: &str| json!({ "total_count": 1, "check_runs": [{ "status": status, "conclusion": conclusion }] });
    mount_ci(&server, runs(Value::Null, "in_progress")).await;
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), options(&server)).await;
    let mut listener = tray(&d).await;
    let mut c = d.client().await;
    sync(&mut c).await;
    assert!(
        heard(&mut listener).await.is_empty(),
        "the first look is a baseline"
    );

    server.reset().await;
    mount_ci(&server, runs(json!("failure"), "completed")).await;
    sync(&mut c).await;
    let events = heard(&mut listener).await;
    assert!(
        banners(&events).is_empty(),
        "macOS is off for this event by default"
    );
    assert!(
        events.contains(&Event::InboxChanged { unseen: 1 }),
        "{events:?}"
    );
    let items = inbox(&mut c).await;
    assert_eq!(items[0].kind, EventKind::ChecksFailed);
    assert_eq!(items[0].title, "Checks failed on #7");

    sync(&mut c).await;
    assert!(
        heard(&mut listener).await.is_empty(),
        "still red is not news"
    );
    d.stop().await;
}

#[tokio::test]
async fn a_rejected_token_is_told_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search/issues"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({ "message": "Bad credentials" })),
        )
        .mount(&server)
        .await;
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), options(&server)).await;
    let mut listener = tray(&d).await;
    let mut c = d.client().await;
    sync(&mut c).await;
    match banners(&heard(&mut listener).await)[..] {
        [Event::Notify { title, open, .. }] => {
            assert_eq!(title, "GitHub refused the token");
            assert_eq!(open, &OpenTarget::Config { page: "git".into() });
        }
        ref other => panic!("{other:?}"),
    }
    sync(&mut c).await;
    assert!(heard(&mut listener).await.is_empty());
    d.stop().await;
}

#[tokio::test]
async fn a_reset_config_is_told_after_the_first_sync() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), "[github\nhost = ").unwrap();
    let server = github(vec![]).await;
    let d = TestDaemon::start_with(dir, options(&server)).await;
    let mut listener = tray(&d).await;
    let mut c = d.client().await;
    sync(&mut c).await;
    let events = heard(&mut listener).await;
    match banners(&events)[..] {
        [
            Event::Notify {
                title, body, open, ..
            },
        ] => {
            assert_eq!(title, "config.toml was repaired");
            assert!(body.contains("clean one"), "{body}");
            assert_eq!(open, &OpenTarget::Home { pr: None });
        }
        ref other => panic!("{other:?}"),
    }
    assert_eq!(inbox(&mut c).await[0].kind, EventKind::StateRecovered);
    sync(&mut c).await;
    assert!(heard(&mut listener).await.is_empty(), "told once");
    d.stop().await;
}

#[tokio::test]
async fn a_corrupt_inbox_is_set_aside_and_told() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("inbox.json"), "{ not json").unwrap();
    let server = github(vec![]).await;
    let d = TestDaemon::start_with(dir, options(&server)).await;
    let mut c = d.client().await;
    sync(&mut c).await;
    let items = inbox(&mut c).await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "inbox.json was repaired");
    assert!(std::fs::read_dir(d.paths.root()).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("inbox.json.corrupt-")
    }));
    d.stop().await;
}

#[tokio::test]
async fn a_test_notification_goes_to_the_tray_and_stays_out_of_the_inbox() {
    let d = TestDaemon::start().await;
    let mut listener = tray(&d).await;
    let mut c = d.client().await;
    assert_eq!(
        c.request(Command::TestNotification).await.unwrap(),
        Reply::Ack
    );
    match banners(&heard(&mut listener).await)[..] {
        [
            Event::Notify {
                title, sound, open, ..
            },
        ] => {
            assert_eq!(title, "Test notification");
            assert_eq!(sound.as_deref(), Some("leaf"));
            assert_eq!(
                open,
                &OpenTarget::Config {
                    page: "notifications".into()
                }
            );
        }
        ref other => panic!("{other:?}"),
    }
    assert!(inbox(&mut c).await.is_empty());
    d.stop().await;
}

#[tokio::test]
async fn the_permission_the_tray_reports_is_kept_and_shown() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let status = |reply| match reply {
        Reply::Status(s) => s.notifications_permission,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        status(c.request(Command::DaemonStatus).await.unwrap()),
        PermissionStatus::NotDetermined
    );
    c.request(Command::NotificationPermission {
        status: PermissionStatus::Denied,
    })
    .await
    .unwrap();
    assert_eq!(
        status(c.request(Command::DaemonStatus).await.unwrap()),
        PermissionStatus::Denied
    );
    d.stop().await;
}

#[tokio::test]
async fn a_saved_review_that_went_stale_is_announced() {
    let w = world().await;
    let mut listener = tray(&w.daemon).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(Command::AddDraftItem {
        pr: pr7(),
        thread: None,
        kind: clusia_core::DraftKind::LineComment,
        anchor: Some(clusia_protocol::AnchorInput {
            path: "feature.txt".into(),
            line: 3,
            start_line: None,
            side: clusia_core::Side::Right,
        }),
        body: "x".into(),
    })
    .await
    .unwrap();
    c.request(Command::CloseReview { pr: pr7() }).await.unwrap();

    let new_head = advance_pr(w.tmp.path(), 7, "feature.txt", "zero\none\ntwo\nthree\n");
    w.server.reset().await;
    mount_pr(
        &w.server,
        &PrMock::new(&new_head, &w.base, &w.origin).adding_feature("zero\none\ntwo\nthree\n"),
    )
    .await;
    mount_lists(&w.server, &[]).await;
    sync(&mut c).await;

    let events = heard(&mut listener).await;
    match banners(&events)[..] {
        [Event::Notify { title, open, .. }] => {
            assert_eq!(title, "#7 is out of date");
            assert_eq!(
                open,
                &OpenTarget::Review {
                    pr: pr7(),
                    thread: None
                }
            );
        }
        ref other => panic!("{other:?}"),
    }
    assert_eq!(inbox(&mut c).await[0].kind, EventKind::CommitsAfterReview);
    w.daemon.stop().await;
}

#[tokio::test]
async fn turning_off_follow_focus_makes_banners_time_sensitive() {
    let server = github(vec![entry("1", "mention", Some(500))]).await;
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), options(&server)).await;
    let mut listener = tray(&d).await;
    let mut c = d.client().await;
    c.request(Command::SetConfigValue {
        key: "notifications.follow_focus".into(),
        value: "false".into(),
    })
    .await
    .unwrap();
    sync(&mut c).await;
    sync(&mut c).await;
    c.request(Command::TestNotification).await.unwrap();
    let events = heard(&mut listener).await;
    let levels: Vec<bool> = banners(&events)
        .into_iter()
        .map(|e| match e {
            Event::Notify { time_sensitive, .. } => *time_sensitive,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        levels,
        [true, true],
        "the feed banner and the test banner: {events:?}"
    );
    d.stop().await;
}

#[tokio::test]
async fn settled_checks_are_not_asked_about_again() {
    let server = MockServer::start().await;
    mount_ci(
        &server,
        json!({ "total_count": 1, "check_runs": [{ "status": "completed", "conclusion": "success" }] }),
    )
    .await;
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), options(&server)).await;
    let mut listener = tray(&d).await;
    let mut c = d.client().await;
    for _ in 0..3 {
        sync(&mut c).await;
        heard(&mut listener).await;
    }
    let looks = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == "/repos/acme/widgets/commits/h/check-runs")
        .count();
    assert_eq!(
        looks, 1,
        "one look at the start, none while nothing changed"
    );
    d.stop().await;
}

#[tokio::test]
async fn the_feed_is_read_at_most_once_a_minute() {
    let server = github(vec![entry("1", "mention", Some(500))]).await;
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), options(&server)).await;
    let mut c = d.client().await;
    for _ in 0..4 {
        sync(&mut c).await;
    }
    assert_eq!(
        feed_requests(&server).await.len(),
        1,
        "the first sync marks the starting point, the second reads, the rest wait a minute"
    );
    assert_eq!(inbox(&mut c).await.len(), 1);
    d.stop().await;
}
