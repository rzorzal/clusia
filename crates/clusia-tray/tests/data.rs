//! The data loop against a scripted fake daemon (no real clusiad, no GitHub).

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clusia_core::{ActivitySummary, Config, PrFilter, PrRef, PrSummary, ReviewState};
use clusia_protocol::{
    ClientMessage, Command, Event, MessageReader, Outcome, PROTOCOL_VERSION, Reply, ServerMessage,
    SyncState, SyncStatus, write_message,
};
use clusia_tray::data::{self, Update};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

enum Push {
    Event(&'static str, Event),
    Close,
}

struct Fake {
    socket: PathBuf,
    _dir: tempfile::TempDir,
    log: Arc<Mutex<Vec<String>>>,
    push: mpsc::UnboundedSender<Push>,
}

impl Fake {
    fn requests(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn clear(&self) {
        self.log.lock().unwrap().clear();
    }

    fn event(&self, topic: &'static str, e: Event) {
        self.push.send(Push::Event(topic, e)).unwrap();
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

fn online() -> SyncStatus {
    SyncStatus {
        state: SyncState::Online,
        last_sync_unix: Some(1),
        next_sync_unix: None,
        message: None,
    }
}

fn reply(cmd: &Command) -> Reply {
    match cmd {
        Command::GetConfig => {
            let mut c = Config::default();
            c.github.host = "ghe.example.com".into();
            Reply::Config(c)
        }
        Command::ListPrs {
            filter: PrFilter::Assigned,
        } => Reply::Prs(vec![pr(7)]),
        Command::ListPrs { .. } => Reply::Prs(vec![]),
        Command::ListReviews => Reply::Reviews(vec![]),
        Command::GetActivity => Reply::Activity(ActivitySummary {
            heatmap: vec![],
            published_this_week: 2,
            published_total: 9,
            avg_review_secs: None,
        }),
        Command::GetSyncStatus => Reply::Sync(online()),
        _ => Reply::Ack,
    }
}

fn fake() -> Fake {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let (push, mut rx) = mpsc::unbounded_channel();
    let seen = log.clone();
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
                    let response = ServerMessage::Response { id, result: Outcome::Ok(reply(&cmd)) };
                    if write_message(&mut w, &response).await.is_err() { return; }
                }
                p = rx.recv() => match p {
                    Some(Push::Event(topic, event)) => {
                        let msg = ServerMessage::Event { topic: topic.into(), event };
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
        push,
    }
}

fn start(socket: PathBuf) -> Receiver<Update> {
    let (tx, rx) = std::sync::mpsc::channel();
    tokio::spawn(data::run(socket, move |u| {
        let _ = tx.send(u);
    }));
    rx
}

fn next(rx: &Receiver<Update>) -> Update {
    rx.recv_timeout(Duration::from_secs(5)).expect("an update")
}

fn snapshot(rx: &Receiver<Update>) -> clusia_tray::model::Snapshot {
    match next(rx) {
        Update::Snapshot(s) => s,
        other => panic!("expected a snapshot, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshots_then_quit_when_the_daemon_hangs_up() {
    let fake = fake();
    let rx = start(fake.socket.clone());
    let early = snapshot(&rx);
    assert_eq!(early.host, "ghe.example.com");
    assert!(
        !early.lists_loaded,
        "lists come second: ListPrs waits for the first sync"
    );
    assert_eq!(early.activity.as_ref().unwrap().published_this_week, 2);
    assert_eq!(early.sync, Some(online()));
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
            "GetSyncStatus",
            "ListPrs",
            "ListPrs"
        ]
    );
    fake.push.send(Push::Close).unwrap();
    assert_eq!(
        next(&rx),
        Update::Quit("clusiad closed the connection".into())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_daemon_quits_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let rx = start(dir.path().join("missing.sock"));
    match next(&rx) {
        Update::Quit(reason) => assert!(reason.starts_with("cannot reach clusiad"), "{reason}"),
        other => panic!("expected Quit, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_burst_is_one_refresh() {
    let fake = fake();
    let rx = start(fake.socket.clone());
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
    let rx = start(fake.socket.clone());
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
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(fake.requests(), ["ListReviews", "GetActivity"]);
    fake.clear();
    let offline = SyncStatus {
        state: SyncState::Offline,
        ..online()
    };
    fake.event("sync", Event::SyncChanged(offline.clone()));
    assert_eq!(snapshot(&rx).sync, Some(offline));
    assert!(fake.requests().is_empty(), "{:?}", fake.requests());
}
