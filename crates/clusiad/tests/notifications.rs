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

fn pr_item(updated_at: &str) -> Value {
    json!({
        "number": 7,
        "title": "Add feature",
        "html_url": "https://github.com/acme/widgets/pull/7",
        "user": { "login": "maria" },
        "draft": false,
        "updated_at": updated_at,
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

/// Your pull request #7, last changed at `updated_at`, with `runs` as its check runs.
async fn mount_ci(server: &MockServer, updated_at: &str, runs: Value) {
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
    mount_lists(server, &[pr_item(updated_at)]).await;
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

/// How long a test waits for something the daemon should do almost at once.
const PATIENCE: Duration = Duration::from_secs(10);

/// What the tray hears until `done` holds for an event (included).
async fn heard_until(c: &mut Client, done: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        let (_, event) = tokio::time::timeout(PATIENCE, c.next_event())
            .await
            .unwrap_or_else(|_| panic!("still waiting; heard so far: {events:?}"))
            .unwrap();
        let last = done(&event);
        events.push(event);
        if last {
            return events;
        }
    }
}

fn is_test_banner(e: &Event) -> bool {
    matches!(e, Event::Notify { id, .. } if id.starts_with("test-"))
}

/// Everything the tray heard before a test banner asked for now. Every event goes through one
/// broadcast channel and reaches a listener in the order it was published, so whatever the
/// daemon said before answering this request is in the list. Tests that ask for a test banner
/// themselves wait for theirs with `heard_until` instead.
async fn heard(c: &mut Client) -> Vec<Event> {
    c.request(Command::TestNotification).await.unwrap();
    let mut events = heard_until(c, is_test_banner).await;
    events.pop();
    events
}

/// Waits until `inbox.json` holds `entry` as the last look at the checks of acme/widgets#7.
/// The checks are looked at by a task of their own, after the sync has answered.
async fn checks_seen(d: &TestDaemon, entry: &str) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let stored = std::fs::read(d.paths.inbox())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|v| v["checks"]["acme~widgets~7"].as_str().map(String::from));
        if stored.as_deref() == Some(entry) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "checks entry is {stored:?}, waiting for {entry}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn check_runs(server_requests: &[wiremock::Request]) -> usize {
    server_requests
        .iter()
        .filter(|r| r.url.path() == "/repos/acme/widgets/commits/h/check-runs")
        .count()
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
    let failing = || runs(json!("failure"), "completed");
    mount_ci(
        &server,
        "2026-10-01T12:00:00Z",
        runs(Value::Null, "in_progress"),
    )
    .await;
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), options(&server)).await;
    let mut listener = tray(&d).await;
    let mut c = d.client().await;
    sync(&mut c).await;
    checks_seen(&d, "2026-10-01T12:00:00Z|h:pending").await;

    server.reset().await;
    mount_ci(&server, "2026-10-01T12:05:00Z", failing()).await;
    sync(&mut c).await;
    let events = heard_until(&mut listener, |e| matches!(e, Event::InboxChanged { .. })).await;
    assert!(
        banners(&events).is_empty(),
        "macOS is off for this event by default: {events:?}"
    );
    assert_eq!(
        events.last(),
        Some(&Event::InboxChanged { unseen: 1 }),
        "the first look was a baseline and told nothing"
    );
    let items = inbox(&mut c).await;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, EventKind::ChecksFailed);
    assert_eq!(items[0].title, "Checks failed on #7");

    // Two more looks at the same red commit. The checks are looked at one sync after another,
    // so once the second is stored the first has told whatever it had to tell.
    for updated_at in ["2026-10-01T12:10:00Z", "2026-10-01T12:15:00Z"] {
        server.reset().await;
        mount_ci(&server, updated_at, failing()).await;
        sync(&mut c).await;
        checks_seen(&d, &format!("{updated_at}|h:failing")).await;
    }
    let events = heard(&mut listener).await;
    assert!(events.is_empty(), "still red is not news: {events:?}");
    assert_eq!(inbox(&mut c).await.len(), 1);
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
    match banners(&heard_until(&mut listener, is_test_banner).await)[..] {
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
async fn a_test_notification_with_no_tray_is_refused() {
    let d = TestDaemon::start().await;
    let mut c = d.client().await;
    let err = c
        .request(Command::TestNotification)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("the menu bar tray is not running"), "{err}");
    // A tray that went away does not count either.
    drop(tray(&d).await);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(c.request(Command::TestNotification).await.is_err());
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
    let events = heard_until(&mut listener, is_test_banner).await;
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
    let passing = json!({ "total_count": 1, "check_runs": [{ "status": "completed", "conclusion": "success" }] });
    mount_ci(&server, "2026-10-01T12:00:00Z", passing).await;
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), options(&server)).await;
    let mut c = d.client().await;
    sync(&mut c).await;
    checks_seen(&d, "2026-10-01T12:00:00Z|h:passing").await;
    sync(&mut c).await;
    sync(&mut c).await;

    // The pull request changes: the look this causes comes after the two syncs' turns.
    Mock::given(method("GET"))
        .and(path("/search/issues"))
        .and(query_param("q", "is:pr is:open archived:false author:@me"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({ "total_count": 1, "items": [pr_item("2026-10-01T12:30:00Z")] }),
            ),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    sync(&mut c).await;
    checks_seen(&d, "2026-10-01T12:30:00Z|h:passing").await;
    assert_eq!(
        check_runs(&server.received_requests().await.unwrap()),
        2,
        "one look at the start, none while nothing changed, one when it did"
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

#[tokio::test]
async fn your_own_comments_are_not_announced() {
    let server = github(vec![
        entry("1", "mention", Some(900)),
        entry("2", "mention", Some(901)),
    ])
    .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/comments/900"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "user": { "login": "me" } })),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), options(&server)).await;
    let mut listener = tray(&d).await;
    let mut c = d.client().await;
    sync(&mut c).await;
    sync(&mut c).await;
    let events = heard(&mut listener).await;
    match banners(&events)[..] {
        [Event::Notify { title, .. }] => assert_eq!(title, "@octo mentioned you"),
        ref other => panic!("{other:?}"),
    }
    assert_eq!(inbox(&mut c).await.len(), 1);
    d.stop().await;
}

#[tokio::test]
async fn a_comment_whose_author_cannot_be_read_is_not_announced() {
    let server = github(vec![
        entry("1", "mention", Some(900)),
        entry("2", "mention", Some(901)),
    ])
    .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/comments/900"))
        .respond_with(ResponseTemplate::new(502))
        .with_priority(1)
        .mount(&server)
        .await;
    let d = TestDaemon::start_with(tempfile::tempdir().unwrap(), options(&server)).await;
    let mut listener = tray(&d).await;
    let mut c = d.client().await;
    sync(&mut c).await;
    sync(&mut c).await;
    let events = heard(&mut listener).await;
    match banners(&events)[..] {
        [Event::Notify { title, .. }] => assert_eq!(title, "@octo mentioned you"),
        ref other => panic!("it may be your own comment: {other:?}"),
    }
    assert_eq!(inbox(&mut c).await.len(), 1);
    let saved = std::fs::read_to_string(d.paths.inbox()).unwrap();
    assert!(
        !saved.contains("900"),
        "it is looked at again next time: {saved}"
    );
    d.stop().await;
}

#[tokio::test]
async fn a_saved_review_whose_base_alone_moved_is_not_new_commits() {
    let w = world().await;
    w.server.reset().await;
    let files = json!([
        { "filename": "feature.txt", "status": "added", "additions": 3, "deletions": 0, "patch": "@@ -0,0 +1,3 @@\n+one\n+two\n+three" },
        { "filename": "README.md", "status": "modified", "additions": 1, "deletions": 1, "patch": "@@ -1 +1 @@\n-hello\n+hello!" }
    ]);
    let mut mock = PrMock::new(&w.head, &w.base, &w.origin);
    mock.files = files.clone();
    mount_pr(&w.server, &mock).await;
    let mut listener = tray(&w.daemon).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    c.request(Command::AddDraftItem {
        pr: pr7(),
        thread: None,
        kind: clusia_core::DraftKind::LineComment,
        anchor: Some(clusia_protocol::AnchorInput {
            path: "README.md".into(),
            line: 1,
            start_line: None,
            side: clusia_core::Side::Left,
        }),
        body: "why?".into(),
    })
    .await
    .unwrap();
    c.request(Command::CloseReview { pr: pr7() }).await.unwrap();

    // Retargeted to a branch the remote does not have: the comment on the old base is obsolete,
    // the head is where it was.
    w.server.reset().await;
    let mut mock = PrMock::new(&w.head, &"f".repeat(40), &w.origin);
    mock.base_ref = "gone".into();
    mock.files = files;
    mount_pr(&w.server, &mock).await;
    mount_lists(&w.server, &[]).await;
    sync(&mut c).await;

    match c.request(Command::GetReview { pr: pr7() }).await.unwrap() {
        Reply::ReviewFile(r) => {
            assert_eq!(r.state, clusia_core::ReviewState::Outdated);
            assert_eq!(r.head_sha, w.head);
        }
        other => panic!("{other:?}"),
    }
    let events = heard(&mut listener).await;
    assert!(banners(&events).is_empty(), "{events:?}");
    assert!(
        inbox(&mut c)
            .await
            .iter()
            .all(|item| item.kind != EventKind::CommitsAfterReview)
    );
    w.daemon.stop().await;
}
