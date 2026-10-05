//! The bridge against an in-process clusiad (no GitHub, no Keychain).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use clusia_app::bridge::{self, Ask, Link, Tell};
use clusia_app::snapshot::Snapshot;
use clusia_core::Config;
use clusia_core::config::Theme as ThemeChoice;
use clusia_protocol::{Command, Reply, WindowTarget};

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
