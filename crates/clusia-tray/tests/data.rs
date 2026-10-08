//! The data loop against a scripted fake daemon (no real clusiad, no GitHub).

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clusia_core::config::EventKind;
use clusia_core::{ActivitySummary, Config, PrFilter, PrRef, PrSummary, ReviewState};
use clusia_protocol::message::{InboxItem, OpenTarget, PermissionStatus};
use clusia_protocol::{
    ClientMessage, Command, ErrorCode, Event, MessageReader, Outcome, PROTOCOL_VERSION,
    ProtocolError, Reply, ServerMessage, SyncState, SyncStatus, write_message,
};
use clusia_tray::data::{self, Update};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

enum Push {
    Event(&'static str, Box<Event>),
    Close,
}

struct Fake {
    socket: PathBuf,
    _dir: tempfile::TempDir,
    log: Arc<Mutex<Vec<String>>>,
    topics: Arc<Mutex<Vec<String>>>,
    push: mpsc::UnboundedSender<Push>,
}

impl Fake {
    fn requests(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// The requests once at least `n` have arrived; panics after a generous deadline.
    async fn wait_for(&self, n: usize) -> Vec<String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let r = self.requests();
            if r.len() >= n {
                return r;
            }
            assert!(std::time::Instant::now() < deadline, "{r:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn clear(&self) {
        self.log.lock().unwrap().clear();
    }

    fn event(&self, topic: &'static str, e: Event) {
        self.push.send(Push::Event(topic, Box::new(e))).unwrap();
    }
}

fn name(cmd: &Command) -> String {
    let s = format!("{cmd:?}");
    s.split(|c: char| !c.is_alphanumeric())
        .next()
        .unwrap()
        .to_string()
}

fn pr(n: u64) -> PrSummary {
    PrSummary {
        pr: PrRef {
            owner: "rzorzal".into(),
            repo: "clusia".into(),
            number: n,
        },
        title: "feat: tray".into(),
        author: "octo".into(),
        url: format!("https://ghe.example.com/rzorzal/clusia/pull/{n}"),
        draft: false,
        updated_at: "2026-10-02T10:00:00Z".into(),
        comments: 1,
    }
}

fn inbox_item(id: &str, seen: bool) -> InboxItem {
    InboxItem {
        id: id.into(),
        kind: EventKind::ReviewRequested,
        pr: Some(pr(7).pr),
        title: "Review requested".into(),
        body: "@octo asked for your review".into(),
        at: 1,
        seen,
    }
}

fn online() -> SyncStatus {
    SyncStatus {
        state: SyncState::Online,
        last_sync_unix: Some(1),
        next_sync_unix: None,
        message: None,
        paused: false,
    }
}

fn reply(cmd: &Command, fail: &[&str]) -> Outcome {
    if fail.contains(&name(cmd).as_str()) {
        return Outcome::Err(ProtocolError::new(ErrorCode::Internal, "scripted failure"));
    }
    Outcome::Ok(match cmd {
        Command::GetConfig => {
            let mut c = Config::default();
            c.github.host = "ghe.example.com".into();
            c.lists.saved_sort = clusia_core::ListSort::Repository;
            Reply::Config(c)
        }
        Command::ListPrs {
            filter: PrFilter::Assigned,
        } => Reply::Prs(vec![pr(7)]),
        Command::ListPrs { .. } => Reply::Prs(vec![]),
        Command::ListReviews => Reply::Reviews(vec![]),
        Command::GetInbox => Reply::Inbox(vec![inbox_item("n1", false)]),
        Command::GetActivity => Reply::Activity(ActivitySummary {
            heatmap: vec![],
            published_this_week: 2,
            published_total: 9,
            avg_review_secs: None,
        }),
        Command::GetSyncStatus | Command::SyncNow => Reply::Sync(online()),
        Command::SetConfigValue { value, .. } if value == "bogus" => {
            return Outcome::Err(ProtocolError::new(
                ErrorCode::InvalidConfigValue,
                "not a list sort",
            ));
        }
        Command::SetConfigValue { value, .. } => Reply::Value(value.clone()),
        _ => Reply::Ack,
    })
}

fn fake() -> Fake {
    fake_failing(&[])
}

/// A fake daemon that answers the named commands with an error.
fn fake_failing(fail: &'static [&'static str]) -> Fake {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let (push, mut rx) = mpsc::unbounded_channel();
    let seen = log.clone();
    let topics = Arc::new(Mutex::new(Vec::new()));
    let subscribed = topics.clone();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut reader = MessageReader::new(r);
        let _hello: ClientMessage = reader.next().await.unwrap().unwrap();
        write_message(
            &mut w,
            &ServerMessage::Welcome {
                protocol: PROTOCOL_VERSION,
                daemon: "test".into(),
            },
        )
        .await
        .unwrap();
        loop {
            tokio::select! {
                msg = reader.next::<ClientMessage>() => {
                    let Ok(Some(ClientMessage::Request { id, cmd })) = msg else { return };
                    seen.lock().unwrap().push(name(&cmd));
                    if let Command::Subscribe { topics } = &cmd {
                        subscribed.lock().unwrap().extend(topics.iter().cloned());
                    }
                    let response = ServerMessage::Response { id, result: reply(&cmd, fail) };
                    if write_message(&mut w, &response).await.is_err() { return; }
                    // Like the real daemon: reply to Shutdown, then close the connection.
                    if matches!(cmd, Command::Shutdown) { return; }
                }
                p = rx.recv() => match p {
                    Some(Push::Event(topic, event)) => {
                        let msg = ServerMessage::Event { topic: topic.into(), event: *event };
                        if write_message(&mut w, &msg).await.is_err() { return; }
                    }
                    _ => return,
                }
            }
        }
    });
    Fake {
        socket,
        _dir: dir,
        log,
        topics,
        push,
    }
}

type WriteQueue = mpsc::UnboundedSender<data::Outgoing>;

fn start(socket: PathBuf) -> (Receiver<Update>, WriteQueue) {
    let (tx, rx) = std::sync::mpsc::channel();
    let (writes, queue) = mpsc::unbounded_channel();
    tokio::spawn(data::run(
        socket,
        move |u| {
            let _ = tx.send(u);
        },
        queue,
    ));
    (rx, writes)
}

fn start_with_agent(socket: PathBuf, agent: PathBuf) -> (Receiver<Update>, WriteQueue) {
    let (tx, rx) = std::sync::mpsc::channel();
    let (writes, queue) = mpsc::unbounded_channel();
    tokio::spawn(data::run_with(
        socket,
        move |u| {
            let _ = tx.send(u);
        },
        queue,
        Some(agent),
    ));
    (rx, writes)
}

fn next(rx: &Receiver<Update>) -> Update {
    rx.recv_timeout(Duration::from_secs(5)).expect("an update")
}

fn snapshot(rx: &Receiver<Update>) -> clusia_tray::model::Snapshot {
    match next(rx) {
        Update::Snapshot(s) => *s,
        other => panic!("expected a snapshot, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshots_then_quit_when_the_daemon_hangs_up() {
    let fake = fake();
    let (rx, _writes) = start(fake.socket.clone());
    let early = snapshot(&rx);
    assert_eq!(early.host, "ghe.example.com");
    assert_eq!(early.lists.saved_sort, clusia_core::ListSort::Repository);
    assert!(
        !early.lists_loaded,
        "lists come second: ListPrs waits for the first sync"
    );
    assert_eq!(early.activity.as_ref().unwrap().published_this_week, 2);
    assert_eq!(early.sync, Some(online()));
    assert_eq!(early.inbox, vec![inbox_item("n1", false)]);
    let full = snapshot(&rx);
    assert!(full.lists_loaded);
    assert_eq!(full.assigned, vec![pr(7)]);
    assert_eq!(
        fake.requests(),
        [
            "Subscribe",
            "GetConfig",
            "ListReviews",
            "GetActivity",
            "GetInbox",
            "GetSyncStatus",
            "ListPrs",
            "ListPrs"
        ]
    );
    assert_eq!(
        *fake.topics.lock().unwrap(),
        ["prs", "sync", "reviews", "config", "tray"]
    );
    fake.push.send(Push::Close).unwrap();
    assert_eq!(
        next(&rx),
        Update::Quit {
            reason: "clusiad closed the connection".into(),
            failure: false,
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_daemon_quits_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let (rx, _writes) = start(dir.path().join("missing.sock"));
    match next(&rx) {
        Update::Quit { reason, failure } => {
            assert!(reason.starts_with("cannot reach clusiad"), "{reason}");
            assert!(!failure, "no daemon is a clean exit");
        }
        other => panic!("expected Quit, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_burst_is_one_refresh() {
    let fake = fake();
    let (rx, _writes) = start(fake.socket.clone());
    snapshot(&rx);
    snapshot(&rx);
    fake.clear();
    for _ in 0..20 {
        fake.event(
            "prs",
            Event::PrsUpdated {
                assigned: 1,
                mine: 0,
            },
        );
    }
    fake.wait_for(2).await;
    // Past the coalesce window: no second refresh follows.
    tokio::time::sleep(Duration::from_millis(800)).await;
    let lists = fake.requests().iter().filter(|r| *r == "ListPrs").count();
    assert_eq!(
        lists,
        2,
        "one refresh (Assigned + Mine): {:?}",
        fake.requests()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn published_review_refreshes_activity_and_sync_changes_need_no_request() {
    let fake = fake();
    let (rx, _writes) = start(fake.socket.clone());
    snapshot(&rx);
    snapshot(&rx);
    fake.clear();
    fake.event(
        "reviews",
        Event::ReviewChanged {
            pr: PrRef {
                owner: "rzorzal".into(),
                repo: "clusia".into(),
                number: 7,
            },
            state: ReviewState::Published,
            items: 0,
        },
    );
    assert_eq!(fake.wait_for(2).await, ["ListReviews", "GetActivity"]);
    fake.clear();
    let offline = SyncStatus {
        state: SyncState::Offline,
        ..online()
    };
    fake.event("sync", Event::SyncChanged(offline.clone()));
    assert_eq!(snapshot(&rx).sync, Some(offline));
    assert!(fake.requests().is_empty(), "{:?}", fake.requests());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_become_config_sets_and_config_events_update_lists() {
    let fake = fake();
    let (rx, writes) = start(fake.socket.clone());
    snapshot(&rx);
    snapshot(&rx);
    fake.clear();
    writes
        .send(data::Outgoing::Config(
            "lists.assigned_sort".into(),
            "oldest".into(),
        ))
        .unwrap();
    assert_eq!(fake.wait_for(1).await, ["SetConfigValue"]);
    fake.event(
        "config",
        Event::ConfigChanged {
            key: "lists.assigned_sort".into(),
            value: "oldest".into(),
        },
    );
    let s = snapshot(&rx);
    assert_eq!(s.lists.assigned_sort, clusia_core::ListSort::Oldest);
    fake.event(
        "config",
        Event::ConfigChanged {
            key: "github.poll_interval_secs".into(),
            value: "90".into(),
        },
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        rx.try_recv().is_err(),
        "unrelated config keys don't produce snapshots"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_runs_sync_now_and_reports_done() {
    let fake = fake();
    let (rx, writes) = start(fake.socket.clone());
    snapshot(&rx);
    snapshot(&rx);
    fake.clear();
    writes.send(data::Outgoing::SyncNow).unwrap();
    // The fresh snapshot goes out before `Refreshed`, so the caption never shows a stale age.
    let mut snapshots = 0;
    loop {
        match next(&rx) {
            Update::Refreshed => break,
            Update::Snapshot(_) => snapshots += 1,
            _ => {}
        }
    }
    assert_eq!(snapshots, 1, "a snapshot precedes Refreshed");
    assert!(fake.requests().contains(&"SyncNow".to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_refresh_still_reports_done() {
    let fake = fake_failing(&["SyncNow"]);
    let (rx, writes) = start(fake.socket.clone());
    snapshot(&rx);
    snapshot(&rx);
    writes.send(data::Outgoing::SyncNow).unwrap();
    loop {
        if next(&rx) == Update::Refreshed {
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_resume_and_quit_are_sent() {
    let fake = fake();
    let (rx, writes) = start(fake.socket.clone());
    snapshot(&rx);
    snapshot(&rx);
    fake.clear();
    writes.send(data::Outgoing::PauseSync).unwrap();
    writes.send(data::Outgoing::ResumeSync).unwrap();
    writes.send(data::Outgoing::Shutdown).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let r = fake.requests();
        if r.contains(&"Shutdown".to_string()) {
            assert!(r.contains(&"PauseSync".to_string()) && r.contains(&"ResumeSync".to_string()));
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{r:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    // The daemon closes after replying: the session ends cleanly (no supervisor restart).
    loop {
        if let Update::Quit { failure, .. } = next(&rx) {
            assert!(!failure, "a requested shutdown is a clean exit");
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_write_is_reported_and_the_session_goes_on() {
    let fake = fake();
    let (rx, writes) = start(fake.socket.clone());
    snapshot(&rx);
    snapshot(&rx);
    writes
        .send(data::Outgoing::Config(
            "lists.assigned_sort".into(),
            "bogus".into(),
        ))
        .unwrap();
    assert_eq!(next(&rx), Update::WriteFailed("lists.assigned_sort".into()));
    let offline = SyncStatus {
        state: SyncState::Offline,
        ..online()
    };
    fake.event("sync", Event::SyncChanged(offline.clone()));
    assert_eq!(snapshot(&rx).sync, Some(offline));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closed_writes_channel_ends_the_session() {
    let fake = fake();
    let (rx, writes) = start(fake.socket.clone());
    snapshot(&rx);
    snapshot(&rx);
    drop(writes);
    assert_eq!(
        next(&rx),
        Update::Quit {
            reason: "the tray UI is gone".into(),
            failure: false,
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_subscribe_is_a_failure() {
    let fake = fake_failing(&["Subscribe"]);
    let (rx, _writes) = start(fake.socket.clone());
    match next(&rx) {
        Update::Quit { reason, failure } => {
            assert!(failure, "the supervisor must restart the tray: {reason}");
            assert!(reason.contains("scripted failure"), "{reason}");
        }
        other => panic!("expected Quit, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_list_requests_leave_the_lists_unloaded() {
    let fake = fake_failing(&["ListPrs"]);
    let (rx, _writes) = start(fake.socket.clone());
    snapshot(&rx);
    let after = snapshot(&rx);
    assert!(!after.lists_loaded, "no list arrived");
    assert!(after.assigned.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_config_echo_always_sends_a_snapshot() {
    let fake = fake();
    let (rx, _writes) = start(fake.socket.clone());
    snapshot(&rx);
    let full = snapshot(&rx);
    assert_eq!(full.lists.assigned_sort, clusia_core::ListSort::Updated);
    // The echo of a write that went back to the last snapshot's value: nothing changed,
    // but the UI must hear it so its pending write clears.
    fake.event(
        "config",
        Event::ConfigChanged {
            key: "lists.assigned_sort".into(),
            value: "updated".into(),
        },
    );
    assert_eq!(snapshot(&rx), full);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_notify_event_reaches_the_ui_without_a_refresh() {
    let fake = fake();
    let (rx, _writes) = start(fake.socket.clone());
    snapshot(&rx);
    snapshot(&rx);
    fake.clear();
    let open = OpenTarget::Review {
        pr: pr(7).pr,
        thread: None,
    };
    fake.event(
        "tray",
        Event::Notify {
            id: "n2".into(),
            title: "Review requested".into(),
            subtitle: "rzorzal/clusia #7".into(),
            body: "@octo asked for your review".into(),
            sound: Some("leaf".into()),
            open: open.clone(),
            time_sensitive: true,
        },
    );
    match next(&rx) {
        Update::Notify(n) => {
            assert_eq!(n.id, "n2");
            assert_eq!(n.sound.as_deref(), Some("leaf"));
            assert_eq!(n.open, open);
            assert!(
                n.time_sensitive,
                "the interruption level is carried through"
            );
        }
        other => panic!("expected Notify, got {other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(fake.requests().is_empty(), "{:?}", fake.requests());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_inbox_change_refetches_only_the_inbox() {
    let fake = fake();
    let (rx, _writes) = start(fake.socket.clone());
    snapshot(&rx);
    snapshot(&rx);
    fake.clear();
    for unseen in 1..=5 {
        fake.event("tray", Event::InboxChanged { unseen });
    }
    assert_eq!(fake.wait_for(1).await, ["GetInbox"]);
    // Past the coalesce window: nothing more arrives.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(fake.requests(), ["GetInbox"], "a burst is one fetch");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permission_and_seen_become_commands() {
    let fake = fake();
    let (rx, writes) = start(fake.socket.clone());
    snapshot(&rx);
    snapshot(&rx);
    fake.clear();
    writes
        .send(data::Outgoing::Permission(PermissionStatus::Allowed))
        .unwrap();
    writes
        .send(data::Outgoing::MarkInboxSeen(vec!["a".into()]))
        .unwrap();
    assert_eq!(
        fake.wait_for(2).await,
        ["NotificationPermission", "MarkInboxSeen"]
    );
}

#[test]
fn a_login_agent_is_needed_only_when_wanted_and_missing() {
    assert!(data::needs_login_agent(true, false));
    assert!(!data::needs_login_agent(true, true), "already installed");
    assert!(
        !data::needs_login_agent(false, false),
        "start at login is off"
    );
    assert!(!data::needs_login_agent(false, true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opening_the_app_installs_a_missing_login_agent() {
    let fake = fake();
    let dir = tempfile::tempdir().unwrap();
    let (rx, _writes) = start_with_agent(fake.socket.clone(), dir.path().join("agent.plist"));
    snapshot(&rx);
    snapshot(&rx);
    assert_eq!(
        fake.requests(),
        [
            "Subscribe",
            "GetConfig",
            "SetStartAtLogin",
            "ListReviews",
            "GetActivity",
            "GetInbox",
            "GetSyncStatus",
            "ListPrs",
            "ListPrs"
        ],
        "the daemon is asked once, right after the config is read"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_installed_login_agent_is_left_alone() {
    let fake = fake();
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent.plist");
    std::fs::write(&agent, "").unwrap();
    let (rx, _writes) = start_with_agent(fake.socket.clone(), agent);
    snapshot(&rx);
    snapshot(&rx);
    assert!(
        !fake.requests().iter().any(|r| r == "SetStartAtLogin"),
        "{:?}",
        fake.requests()
    );
}
