//! The bridge against an in-process clusiad (no GitHub, no Keychain).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use clusia_app::bridge::{self, AgentTell, Ask, Link, PermissionTell, Tell};
use clusia_app::fixture;
use clusia_app::snapshot::{GiphyKey, Snapshot};
use clusia_core::config::Theme as ThemeChoice;
use clusia_core::{Config, DraftKind, PrRef, Review, ReviewCache, ReviewState, Verdict};
use clusia_harness::testkit::{FakeClaude, Script, Turn};
use clusia_protocol::{
    Command, Event, GithubLogin, LoadStepKind, PermissionAnswerKind, Reply, Secret, StepStatus,
    WindowTarget, topics,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The next tell matching `want`, within 10 s (others are skipped).
fn next(link: &Link, want: impl Fn(&Tell) -> bool) -> Tell {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match link.tell.recv_timeout(left) {
            Ok(t) if want(&t) => return t,
            Ok(_) => continue,
            Err(_) => panic!("timed out waiting for a tell"),
        }
    }
}

fn snapshot_where(link: &Link, want: impl Fn(&Snapshot) -> bool) -> Snapshot {
    match next(link, |t| matches!(t, Tell::Snapshot(s) if want(s))) {
        Tell::Snapshot(s) => *s,
        _ => unreachable!(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_snapshots_then_lists() {
    let d = common::Daemon::start().await;
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = wakes.clone();
    let link = bridge::spawn(d.paths.clone(), None, move || {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    let first = snapshot_where(&link, |_| true);
    assert_eq!(first.config, Config::default());
    assert!(first.sync.is_some());
    assert!(
        first.auth.as_ref().unwrap().error.is_some(),
        "no token in tests"
    );
    assert!(!first.daemon_version.is_empty());
    let second = snapshot_where(&link, |s| s.lists_loaded);
    assert!(second.assigned.is_empty() && second.mine.is_empty());
    assert!(wakes.load(Ordering::SeqCst) >= 2, "every tell wakes Bevy");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_write_round_trips() {
    let d = common::Daemon::start().await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    link.ask
        .send(Ask::SetConfig {
            key: "appearance.theme".into(),
            value: "dark".into(),
        })
        .unwrap();
    let s = snapshot_where(&link, |s| s.config.appearance.theme == ThemeChoice::Dark);
    assert_eq!(s.config.appearance.theme, ThemeChoice::Dark);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_write_is_reported() {
    let d = common::Daemon::start().await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    link.ask
        .send(Ask::SetConfig {
            key: "github.poll_interval_secs".into(),
            value: "5".into(),
        })
        .unwrap();
    match next(&link, |t| matches!(t, Tell::Rejected { .. })) {
        Tell::Rejected { key, message } => {
            assert_eq!(key, "github.poll_interval_secs");
            assert!(message.contains("poll_interval_secs"), "{message}");
        }
        _ => unreachable!(),
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn show_requests_arrive() {
    let d = common::Daemon::start().await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |_| true); // subscribed by now
    let mut other = d.client().await;
    let reply = other
        .request(Command::OpenWindow {
            target: WindowTarget::Config,
        })
        .await
        .unwrap();
    assert_eq!(reply, Reply::Delivered(1));
    assert_eq!(
        next(&link, |t| matches!(t, Tell::Show(_))),
        Tell::Show(WindowTarget::Config)
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editor_refusals_become_notices() {
    let d = common::Daemon::start().await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    link.ask
        .send(Ask::OpenInEditor {
            path: "/etc/hosts".into(),
            line: None,
        })
        .unwrap();
    match next(&link, |t| matches!(t, Tell::Notice { .. })) {
        Tell::Notice { warning, text } => assert!(warning && text.contains("only files"), "{text}"),
        _ => unreachable!(),
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quit_from_the_tray_closes_the_window() {
    let d = common::Daemon::start().await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    let mut tray = d.client().await;
    assert_eq!(tray.request(Command::Shutdown).await.unwrap(), Reply::Ack);
    assert_eq!(next(&link, |t| matches!(t, Tell::Quit)), Tell::Quit);
    d.wait().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_connection_then_reconnect() {
    let d = common::Daemon::start().await;
    let paths = d.paths.clone();
    let link = bridge::spawn(paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    let dir = d.stop().await;
    assert!(matches!(
        next(&link, |t| matches!(t, Tell::Lost(_))),
        Tell::Lost(_)
    ));
    let d = common::Daemon::start_in(dir).await;
    link.ask.send(Ask::Reconnect).unwrap();
    snapshot_where(&link, |_| true);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn asks_while_disconnected_are_answered() {
    let d = common::Daemon::start().await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    d.stop().await;
    next(&link, |t| matches!(t, Tell::Lost(_)));
    link.ask
        .send(Ask::SetConfig {
            key: "appearance.theme".into(),
            value: "dark".into(),
        })
        .unwrap();
    match next(&link, |t| matches!(t, Tell::Rejected { .. })) {
        Tell::Rejected { key, message } => {
            assert_eq!(key, "appearance.theme");
            assert!(message.contains("not saved"), "{message}");
        }
        _ => unreachable!(),
    }
    link.ask.send(Ask::SyncNow).unwrap();
    match next(&link, |t| matches!(t, Tell::Notice { .. })) {
        Tell::Notice { warning, text } => assert!(warning && text.contains("reconnecting")),
        _ => unreachable!(),
    }
}

fn pr7() -> PrRef {
    "rzorzal/clusia#7".parse().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_steps_stream_while_opening() {
    let server = MockServer::start().await;
    let slow = Duration::from_millis(1500);
    Mock::given(method("GET"))
        .and(path("/repos/rzorzal/clusia/pulls/7"))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(serde_json::json!({ "message": "Not Found" }))
                .set_delay(slow),
        )
        .mount(&server)
        .await;
    let d = common::Daemon::start_with_github(server.uri()).await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    let asked = Instant::now();
    link.ask.send(Ask::OpenReview(pr7())).unwrap();
    let first = next(&link, |t| {
        matches!(
            t,
            Tell::Step(_) | Tell::Opened { .. } | Tell::OpenFailed { .. }
        )
    });
    match &first {
        Tell::Step(s) => {
            assert_eq!(
                (&s.pr, s.step, s.status),
                (&pr7(), LoadStepKind::Repo, StepStatus::Running)
            );
        }
        other => panic!("a step comes first, got {other:?}"),
    }
    assert!(
        asked.elapsed() < slow,
        "the step arrived while GitHub was still answering ({:?})",
        asked.elapsed()
    );
    match next(&link, |t| matches!(t, Tell::OpenFailed { .. })) {
        Tell::OpenFailed { pr, message, cache } => {
            assert_eq!(pr, pr7());
            assert!(!message.is_empty());
            assert!(!cache, "nothing cached yet");
        }
        _ => unreachable!(),
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn draft_writes_answer_with_their_ticket() {
    let d = common::Daemon::start().await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    link.ask
        .send(Ask::AddItem {
            pr: pr7(),
            kind: DraftKind::General,
            anchor: None,
            thread: None,
            body: "Looks good".into(),
            ticket: 41,
        })
        .unwrap();
    match next(&link, |t| {
        matches!(t, Tell::Refused { .. } | Tell::Saved { .. })
    }) {
        Tell::Refused {
            pr,
            ticket,
            message,
        } => {
            assert_eq!((pr, ticket), (pr7(), 41));
            assert!(!message.is_empty(), "the daemon's message");
        }
        other => panic!("no review is open for #7: {other:?}"),
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish_failure_and_leaving_reach_the_window() {
    let d = common::Daemon::start().await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    link.ask
        .send(Ask::Publish {
            pr: pr7(),
            verdict: Verdict::Comment,
            summary: String::new(),
        })
        .unwrap();
    match next(&link, |t| {
        matches!(t, Tell::PublishFailed { .. } | Tell::Published { .. })
    }) {
        Tell::PublishFailed { pr, message, .. } => {
            assert_eq!(pr, pr7());
            assert!(!message.is_empty());
        }
        other => panic!("nothing to publish: {other:?}"),
    }
    link.ask.send(Ask::CloseReview(pr7())).unwrap();
    assert_eq!(
        next(&link, |t| matches!(t, Tell::Left(_))),
        Tell::Left(pr7()),
        "the tab closes even when the daemon had nothing open"
    );
    d.stop().await;
}

/// Writes the demo review and its cache into `dir`, as a daemon that opened it would have.
fn seed_demo_review(dir: &std::path::Path) -> PrRef {
    let paths = clusia_core::Paths::new(dir);
    let (view, _) = fixture::demo_review(1_790_000_000);
    let pr = view.review.pr.clone();
    let cache = ReviewCache {
        pr: view.pr,
        files: view.diff,
        conversation: view.conversation.unwrap_or_default(),
        checks: view.checks,
        role: view.role,
        viewer: view.viewer,
        worktree: view.worktree,
        fetched_at: 1_790_000_000 - 3600,
    };
    for (file, json) in [
        (
            paths.review_file(&pr),
            serde_json::to_vec(&view.review).unwrap(),
        ),
        (
            paths.review_cache_file(&pr),
            serde_json::to_vec(&cache).unwrap(),
        ),
    ] {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, json).unwrap();
    }
    pr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cached_review_opens_ready_without_loading() {
    let dir = tempfile::tempdir().unwrap();
    let pr = seed_demo_review(dir.path());
    let d = common::Daemon::start_in(dir).await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    link.ask.send(Ask::OpenReview(pr.clone())).unwrap();
    // The daemon has no token, so the refresh fails; the cached copy came first.
    let first = next(&link, |t| {
        matches!(t, Tell::OpenedFromCache { .. } | Tell::OpenFailed { .. })
    });
    assert!(
        matches!(first, Tell::OpenedFromCache { pr: ref got, .. } if *got == pr),
        "the cached copy opens the tab, got {first:?}"
    );
    assert!(matches!(
        next(&link, |t| matches!(t, Tell::OpenFailed { .. })),
        Tell::OpenFailed { cache: true, .. }
    ));
    d.stop().await;
}

fn general(pr: &PrRef, body: &str, ticket: u64) -> Ask {
    Ask::AddItem {
        pr: pr.clone(),
        kind: DraftKind::General,
        anchor: None,
        thread: None,
        body: body.into(),
        ticket,
    }
}

fn review_file(link: &Link) -> Review {
    match next(link, |t| matches!(t, Tell::ReviewFile(_))) {
        Tell::ReviewFile(r) => *r,
        _ => unreachable!(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_reviews_follow_changes_across_reconnects() {
    let dir = tempfile::tempdir().unwrap();
    let pr = seed_demo_review(dir.path());
    let d = common::Daemon::start_in(dir).await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    link.ask.send(Ask::OpenCached(pr.clone())).unwrap();
    assert!(matches!(
        next(&link, |t| matches!(t, Tell::OpenedFromCache { .. })),
        Tell::OpenedFromCache { .. }
    ));
    link.ask.send(general(&pr, "Ship it", 1)).unwrap();
    assert_eq!(
        review_file(&link).draft.items.len(),
        4,
        "ReviewChanged refetches the open review"
    );
    let dir = d.stop().await;
    next(&link, |t| matches!(t, Tell::Lost(_)));
    let d = common::Daemon::start_in(dir).await;
    link.ask.send(Ask::Reconnect).unwrap();
    assert_eq!(
        review_file(&link).draft.items.len(),
        4,
        "a reconnect refetches every open review"
    );
    link.ask.send(general(&pr, "One more thing", 2)).unwrap();
    assert_eq!(
        review_file(&link).draft.items.len(),
        5,
        "still followed after the reconnect"
    );
    d.stop().await;
}

/// Writes the demo review (active, with its draft items) as `rzorzal/clusia#number`.
fn seed_active_review(dir: &std::path::Path, number: u64) -> PrRef {
    let paths = clusia_core::Paths::new(dir);
    let mut review = fixture::demo_review(1_790_000_000).0.review;
    review.pr = format!("rzorzal/clusia#{number}").parse().unwrap();
    assert_eq!(review.state, ReviewState::Active);
    assert!(!review.draft.items.is_empty());
    let file = paths.review_file(&review.pr);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, serde_json::to_vec(&review).unwrap()).unwrap();
    review.pr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leave_choices_reach_the_daemon_before_the_window_exits() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (
        seed_active_review(dir.path(), 123),
        seed_active_review(dir.path(), 124),
    );
    let d = common::Daemon::start_in(dir).await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    let Link { tell, ask, thread } = link;
    // The window closes with both tabs kept for later: the asks are queued, then the window's
    // world (and its `Asks` sender) goes away at once.
    ask.send(Ask::CloseReview(a.clone())).unwrap();
    ask.send(Ask::CloseReview(b.clone())).unwrap();
    drop(ask);
    let finished = tokio::task::spawn_blocking(move || thread.finish(Duration::from_secs(3)))
        .await
        .unwrap();
    assert!(finished, "the bridge answered what was queued, then ended");
    for pr in [a, b] {
        let file = std::fs::read(d.paths.review_file(&pr)).unwrap();
        let review: Review = serde_json::from_slice(&file).unwrap();
        assert_eq!(review.state, ReviewState::Saved, "{pr}");
    }
    drop(tell);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn giphy_key_status_follows_the_daemon() {
    let d = common::Daemon::start().await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    let first = snapshot_where(&link, |s| s.lists_loaded);
    assert_eq!(first.giphy_key, GiphyKey::Missing, "no key in a new home");
    link.ask
        .send(Ask::SetGiphyKey(Secret::from(
            "dc6zaTOxFJmzC1234567890abcdefghi",
        )))
        .unwrap();
    snapshot_where(&link, |s| s.giphy_key == GiphyKey::Set);
    link.ask.send(Ask::ClearGiphyKey).unwrap();
    snapshot_where(&link, |s| s.giphy_key == GiphyKey::Missing);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_run_status_reaches_the_window() {
    let d = common::Daemon::start().await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    let s = snapshot_where(&link, |s| s.first_run.is_some());
    let status = s.first_run.expect("answered");
    assert_eq!(
        status.github,
        GithubLogin::Unknown,
        "the test daemon has no gh to ask"
    );
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_at_login_goes_through_the_daemon() {
    let d = common::Daemon::start().await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    link.ask.send(Ask::SetStartAtLogin { on: false }).unwrap();
    let s = snapshot_where(&link, |s| !s.config.general.start_at_login);
    assert!(!s.config.general.start_at_login);
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_snapshot_carries_the_notification_permission() {
    let d = common::Daemon::start().await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    let first = snapshot_where(&link, |_| true);
    assert_eq!(
        first.notifications_permission,
        clusia_protocol::PermissionStatus::NotDetermined
    );
    let mut tray = d.client().await;
    tray.request(Command::NotificationPermission {
        status: clusia_protocol::PermissionStatus::Allowed,
    })
    .await
    .unwrap();
    link.ask.send(Ask::RefreshStatus).unwrap();
    let read = snapshot_where(&link, |s| {
        s.notifications_permission == clusia_protocol::PermissionStatus::Allowed
    });
    assert_eq!(
        read.notifications_permission,
        clusia_protocol::PermissionStatus::Allowed
    );
    let notice = |link: &Link| {
        link.ask.send(Ask::TestNotification).unwrap();
        match next(link, |t| matches!(t, Tell::Notice { .. })) {
            Tell::Notice { text, warning } => (text, warning),
            _ => unreachable!(),
        }
    };
    let (text, warning) = notice(&link);
    assert!(text.contains("tray is not running"), "{text}");
    assert!(warning);
    tray.request(Command::Subscribe {
        topics: vec![clusia_protocol::topics::TRAY.into()],
    })
    .await
    .unwrap();
    assert_eq!(notice(&link), ("Test notification sent".into(), false));
    d.stop().await;
}

/// A stored review whose worktree is a real, empty git repository, so the agent can run in it.
fn seed_agent_review(dir: &std::path::Path, number: u64) -> PrRef {
    let pr = seed_active_review(dir, number);
    let worktree = clusia_core::Paths::new(dir).worktree_for(&pr);
    std::fs::create_dir_all(&worktree).unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(&worktree)
        .status()
        .unwrap();
    assert!(status.success());
    pr
}

async fn use_fake_claude(d: &common::Daemon, script: Script) {
    let program = FakeClaude::install(&d.dir.path().join("fake"), script);
    for (key, value) in [
        ("harness.program", program.display().to_string()),
        ("harness.on_open", "wait".to_string()),
    ] {
        d.client()
            .await
            .request(Command::SetConfigValue {
                key: key.into(),
                value,
            })
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_turn_streams_to_the_window_and_the_log_replays_it() {
    let dir = tempfile::tempdir().unwrap();
    let pr = seed_agent_review(dir.path(), 9);
    let d = common::Daemon::start_in(dir).await;
    use_fake_claude(&d, Script::one(Turn::answer("The lock is needed."))).await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    link.ask
        .send(Ask::AgentSend {
            pr: pr.clone(),
            text: "Is the lock needed?".into(),
        })
        .unwrap();
    let chunk = next(&link, |t| matches!(t, Tell::Agent(AgentTell::Chunk { .. })));
    match chunk {
        Tell::Agent(AgentTell::Chunk { pr: got, text, .. }) => {
            assert_eq!(got, pr);
            assert!(!text.is_empty());
        }
        _ => unreachable!(),
    }
    next(&link, |t| matches!(t, Tell::Agent(AgentTell::Done { .. })));
    next(&link, |t| {
        matches!(
            t,
            Tell::Agent(AgentTell::State {
                state: clusia_protocol::SessionStateKind::Ready,
                ..
            })
        )
    });
    link.ask.send(Ask::AgentLog { pr: pr.clone() }).unwrap();
    match next(&link, |t| matches!(t, Tell::AgentLog { .. })) {
        Tell::AgentLog { entries, .. } => {
            use clusia_protocol::AgentLogEntry;
            assert!(entries.iter().any(
                |e| matches!(e, AgentLogEntry::User { text, .. } if text == "Is the lock needed?")
            ));
            assert!(
                entries
                    .iter()
                    .any(|e| matches!(e, AgentLogEntry::Text { .. }))
            );
        }
        _ => unreachable!(),
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_for_a_review_that_is_not_open_is_refused_in_the_chat() {
    let d = common::Daemon::start().await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    let pr: PrRef = "rzorzal/clusia#999".parse().unwrap();
    link.ask
        .send(Ask::AgentSend {
            pr: pr.clone(),
            text: "hello".into(),
        })
        .unwrap();
    match next(&link, |t| {
        matches!(t, Tell::Agent(AgentTell::Refused { .. }))
    }) {
        Tell::Agent(AgentTell::Refused { pr: got, message }) => {
            assert_eq!(got, pr);
            assert!(message.contains("open it first"), "{message}");
        }
        _ => unreachable!(),
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_probe_that_cannot_run_answers_with_the_reason() {
    let d = common::Daemon::start().await;
    d.client()
        .await
        .request(Command::SetConfigValue {
            key: "harness.program".into(),
            value: "/nonexistent/claude".into(),
        })
        .await
        .unwrap();
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    link.ask.send(Ask::Probe).unwrap();
    match next(&link, |t| matches!(t, Tell::Probe(_))) {
        Tell::Probe(result) => {
            assert!(!result.ok);
            assert!(result.error.is_some(), "{result:?}");
        }
        _ => unreachable!(),
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn asks_for_the_agent_while_disconnected_are_answered() {
    let d = common::Daemon::start().await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    d.stop().await;
    next(&link, |t| matches!(t, Tell::Lost(_)));
    let pr = pr7();
    link.ask
        .send(Ask::AgentSend {
            pr: pr.clone(),
            text: "hello".into(),
        })
        .unwrap();
    match next(&link, |t| {
        matches!(t, Tell::Agent(AgentTell::Refused { .. }))
    }) {
        Tell::Agent(AgentTell::Refused { message, .. }) => {
            assert!(message.contains("Not connected"), "{message}")
        }
        _ => unreachable!(),
    }
    link.ask.send(Ask::Probe).unwrap();
    match next(&link, |t| matches!(t, Tell::Probe(_))) {
        Tell::Probe(result) => assert!(!result.ok),
        _ => unreachable!(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepting_a_suggestion_that_is_no_longer_waiting_adds_nothing_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let pr = seed_agent_review(dir.path(), 9);
    let d = common::Daemon::start_in(dir).await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    link.ask
        .send(Ask::AcceptSuggestion {
            pr: pr.clone(),
            id: "sug-000000000000".into(),
            body: Some("Edited text".into()),
        })
        .unwrap();
    let answer = next(&link, |t| {
        matches!(
            t,
            Tell::Agent(AgentTell::Handled { .. }) | Tell::Notice { .. }
        )
    });
    match answer {
        Tell::Notice { text, warning } => {
            assert!(warning);
            assert!(text.contains("no longer waiting"), "{text}");
        }
        other => panic!("nothing was added, yet: {other:?}"),
    }
    match next(&link, |t| matches!(t, Tell::AgentLog { .. })) {
        Tell::AgentLog { pr: got, .. } => assert_eq!(got, pr, "the chat is read again"),
        _ => unreachable!(),
    }
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reading_one_reviews_log_keeps_another_reviews_live_answer() {
    const CHUNKS: usize = 150;
    let dir = tempfile::tempdir().unwrap();
    let streaming = seed_agent_review(dir.path(), 9);
    let other = seed_agent_review(dir.path(), 10);
    let d = common::Daemon::start_in(dir).await;
    let mut lines =
        vec![r#"{"type":"system","subtype":"init","session_id":"__SESSION__"}"#.to_string()];
    let mut expected = String::new();
    for i in 0..CHUNKS {
        let text = format!("{i},");
        expected.push_str(&text);
        lines.push(
            serde_json::json!({
                "type": "stream_event",
                "event": {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": text},
                },
                "session_id": "__SESSION__",
            })
            .to_string(),
        );
    }
    lines.push(
        serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false,
            "duration_ms": 5, "num_turns": 1, "result": expected,
            "session_id": "__SESSION__", "permission_denials": [],
        })
        .to_string(),
    );
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    use_fake_claude(&d, Script::one(Turn::lines(&lines).delay_ms(4))).await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    link.ask
        .send(Ask::AgentSend {
            pr: streaming.clone(),
            text: "Count for me".into(),
        })
        .unwrap();
    let asks = link.ask.clone();
    let reader = std::thread::spawn(move || {
        for _ in 0..400 {
            if asks.send(Ask::AgentLog { pr: other.clone() }).is_err() {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    });
    let mut shown = String::new();
    loop {
        match next(
            &link,
            |t| matches!(t, Tell::Agent(a) if *a.pr() == streaming),
        ) {
            Tell::Agent(AgentTell::Chunk { text, .. }) => shown.push_str(&text),
            Tell::Agent(AgentTell::Done { .. }) => break,
            _ => {}
        }
    }
    reader.join().unwrap();
    assert_eq!(shown, expected, "every chunk of the other review arrived");
    d.stop().await;
}

/// Every tray event `watcher` received within `quiet` of the last one.
async fn tray_events(watcher: &mut clusia_protocol::Client, quiet: Duration) -> Vec<Event> {
    let mut seen = Vec::new();
    while let Ok(Ok((_, event))) = tokio::time::timeout(quiet, watcher.next_event()).await {
        seen.push(event);
    }
    seen
}

/// One turn of the agent for `pr` in the window: it asks a permission as the bridge would, the
/// window answers it, and the turn ends.
async fn turn_with_a_request(d: &common::Daemon, link: &Link, pr: &PrRef, turn: u64) {
    link.ask
        .send(Ask::AgentSend {
            pr: pr.clone(),
            text: "Run the tests".into(),
        })
        .unwrap();
    next(link, |t| {
        matches!(
            t,
            Tell::Agent(AgentTell::State {
                state: clusia_protocol::SessionStateKind::Running,
                ..
            })
        )
    });
    let mut asker = d.client().await;
    let ask = Command::PermissionAsk {
        pr: pr.clone(),
        turn,
        tool: "Bash".into(),
        input: serde_json::json!({"command": "cargo test"}),
    };
    let asked = tokio::spawn(async move { asker.request(ask).await });
    let id = match next(link, |t| {
        matches!(t, Tell::Permission(PermissionTell::Requested(_)))
    }) {
        Tell::Permission(PermissionTell::Requested(request)) => request.id,
        _ => unreachable!(),
    };
    link.ask
        .send(Ask::PermissionAnswer {
            id,
            answer: PermissionAnswerKind::Once,
        })
        .unwrap();
    assert!(matches!(
        asked.await.unwrap(),
        Ok(Reply::PermissionDecision { allow: true, .. })
    ));
    next(link, |t| matches!(t, Tell::Agent(AgentTell::Done { .. })));
}

async fn tray_watcher(d: &common::Daemon) -> clusia_protocol::Client {
    let mut watcher = d.client().await;
    watcher
        .request(Command::Subscribe {
            topics: vec![topics::TRAY.into()],
        })
        .await
        .unwrap();
    watcher
}

/// The notifications among `events`: a banner, or an entry the inbox counts.
fn notified(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Notify { title, .. } => Some(title.clone()),
            Event::InboxChanged { unseen } => Some(format!("inbox: {unseen}")),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_review_open_in_the_window_sends_nothing_to_the_tray() {
    let dir = tempfile::tempdir().unwrap();
    let pr = seed_agent_review(dir.path(), 9);
    let d = common::Daemon::start_in(dir).await;
    let slow = || Turn::answer("Ran them.").delay_ms(700);
    use_fake_claude(&d, Script::turns(vec![slow(), slow()])).await;
    let link = bridge::spawn(d.paths.clone(), None, || {});
    snapshot_where(&link, |s| s.lists_loaded);
    link.ask.send(Ask::OpenCached(pr.clone())).unwrap();

    let mut watcher = tray_watcher(&d).await;
    turn_with_a_request(&d, &link, &pr, 1).await;
    let seen = tray_events(&mut watcher, Duration::from_millis(500)).await;
    assert_eq!(
        notified(&seen),
        Vec::<String>::new(),
        "neither the request nor the end goes to the tray"
    );

    // The daemon restarts: the window holds its reviews again.
    let dir = d.stop().await;
    next(&link, |t| matches!(t, Tell::Lost(_)));
    let d = common::Daemon::start_in(dir).await;
    link.ask.send(Ask::Reconnect).unwrap();
    snapshot_where(&link, |s| s.lists_loaded);
    let mut watcher = tray_watcher(&d).await;
    turn_with_a_request(&d, &link, &pr, 2).await;
    let seen = tray_events(&mut watcher, Duration::from_millis(500)).await;
    assert_eq!(
        notified(&seen),
        Vec::<String>::new(),
        "still held after a reconnect"
    );
    d.stop().await;
}
