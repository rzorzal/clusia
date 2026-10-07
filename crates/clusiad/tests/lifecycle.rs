mod common;

use std::os::unix::fs::PermissionsExt;

use clusia_core::Paths;
use clusia_protocol::{
    ClientMessage, Command, ErrorCode, MAX_LINE_BYTES, MessageReader, Outcome, PROTOCOL_VERSION,
    Reply, ServerMessage, write_message,
};
use clusiad::{Daemon, StartError};
use common::TestDaemon;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

async fn raw_hello(
    d: &TestDaemon,
    protocol: u32,
) -> (MessageReader<OwnedReadHalf>, OwnedWriteHalf, ServerMessage) {
    let (r, mut w) = UnixStream::connect(d.paths.socket())
        .await
        .unwrap()
        .into_split();
    let hello = ClientMessage::Hello {
        protocol,
        client: "raw".into(),
        version: "0".into(),
    };
    write_message(&mut w, &hello).await.unwrap();
    let mut r = MessageReader::new(r);
    let greeting = r.next::<ServerMessage>().await.unwrap().unwrap();
    (r, w, greeting)
}

fn bad_request_id(m: ServerMessage) -> u64 {
    match m {
        ServerMessage::Response {
            id,
            result: Outcome::Err(e),
        } if e.code == ErrorCode::BadRequest => id,
        other => panic!("expected a BadRequest response, got {other:?}"),
    }
}

#[tokio::test]
async fn status_reports_pid_version_and_socket() {
    let d = TestDaemon::start().await;
    match d
        .client()
        .await
        .request(Command::DaemonStatus)
        .await
        .unwrap()
    {
        Reply::Status(s) => {
            assert_eq!(s.pid, std::process::id());
            assert_eq!(s.version, clusiad::VERSION);
            assert_eq!(s.socket, d.paths.socket().display().to_string());
            assert_eq!(s.clients, 1);
        }
        other => panic!("expected status, got {other:?}"),
    }
    d.stop().await;
}

#[tokio::test]
async fn socket_is_private() {
    let d = TestDaemon::start().await;
    let mode = std::fs::metadata(d.paths.socket())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    d.stop().await;
}

#[tokio::test]
async fn stale_socket_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    drop(std::os::unix::net::UnixListener::bind(dir.path().join("clusiad.sock")).unwrap());
    let d = TestDaemon::start_in(dir).await;
    d.client()
        .await
        .request(Command::DaemonStatus)
        .await
        .unwrap();
    d.stop().await;
}

#[tokio::test]
async fn second_daemon_refuses() {
    let d = TestDaemon::start().await;
    let err = Daemon::bind(d.paths.clone())
        .await
        .err()
        .expect("second bind must fail");
    assert!(matches!(err, StartError::AlreadyRunning(_)), "{err}");
    d.client()
        .await
        .request(Command::DaemonStatus)
        .await
        .unwrap();
    d.stop().await;
}

#[tokio::test]
async fn second_daemon_leaves_config_untouched() {
    let d = TestDaemon::start().await;
    let cfg = d.paths.config_file();
    std::fs::write(&cfg, "[github\nhost = ").unwrap();
    let err = Daemon::bind(d.paths.clone()).await.err().unwrap();
    assert!(matches!(err, StartError::AlreadyRunning(_)), "{err}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "[github\nhost = ");
    let corrupt = std::fs::read_dir(d.paths.root())
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.file_name().to_string_lossy().contains(".corrupt-"));
    assert!(!corrupt, "second daemon must not quarantine");
    d.stop().await;
}

#[tokio::test]
async fn long_socket_path_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let err = Daemon::bind(Paths::new(dir.path().join("x".repeat(100))))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, StartError::PathTooLong { .. }));
    assert!(err.to_string().contains("103"), "{err}");
}

#[tokio::test]
async fn shutdown_acks_then_removes_socket() {
    let d = TestDaemon::start().await;
    let socket = d.paths.socket();
    assert_eq!(
        d.client().await.request(Command::Shutdown).await.unwrap(),
        Reply::Ack
    );
    let _dir = d.wait().await;
    assert!(!socket.exists());
}

#[tokio::test]
async fn wrong_protocol_gets_incompatible_and_is_closed() {
    let d = TestDaemon::start().await;
    let (mut r, _w, greeting) = raw_hello(&d, PROTOCOL_VERSION + 1).await;
    assert!(matches!(
        greeting,
        ServerMessage::Incompatible {
            daemon_protocol: PROTOCOL_VERSION,
            ..
        }
    ));
    assert!(r.next::<ServerMessage>().await.unwrap().is_none());
    d.stop().await;
}

#[tokio::test]
async fn request_before_hello_is_rejected() {
    let d = TestDaemon::start().await;
    let (r, mut w) = UnixStream::connect(d.paths.socket())
        .await
        .unwrap()
        .into_split();
    write_message(
        &mut w,
        &ClientMessage::Request {
            id: 5,
            cmd: Command::DaemonStatus,
        },
    )
    .await
    .unwrap();
    let mut r = MessageReader::new(r);
    assert_eq!(bad_request_id(r.next().await.unwrap().unwrap()), 5);
    assert!(r.next::<ServerMessage>().await.unwrap().is_none());
    d.stop().await;
}

#[tokio::test]
async fn malformed_line_gets_bad_request_and_connection_survives() {
    let d = TestDaemon::start().await;
    let (mut r, mut w, greeting) = raw_hello(&d, PROTOCOL_VERSION).await;
    assert!(matches!(greeting, ServerMessage::Welcome { .. }));

    w.write_all(b"{\"id\": 12, \"nonsense\": true}\n")
        .await
        .unwrap();
    assert_eq!(bad_request_id(r.next().await.unwrap().unwrap()), 12);

    w.write_all(b"garbage\n").await.unwrap();
    assert_eq!(bad_request_id(r.next().await.unwrap().unwrap()), 0);

    write_message(
        &mut w,
        &ClientMessage::Request {
            id: 13,
            cmd: Command::DaemonStatus,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        r.next::<ServerMessage>().await.unwrap().unwrap(),
        ServerMessage::Response {
            id: 13,
            result: Outcome::Ok(Reply::Status(_))
        }
    ));
    d.stop().await;
}

#[tokio::test]
async fn oversized_line_drops_only_that_client() {
    let d = TestDaemon::start().await;
    let (mut r, mut w, _) = raw_hello(&d, PROTOCOL_VERSION).await;
    let writer = tokio::spawn(async move {
        let chunk = vec![b'a'; 1 << 16];
        for _ in 0..(MAX_LINE_BYTES / (1 << 16) + 2) {
            if w.write_all(&chunk).await.is_err() {
                break;
            }
        }
    });
    // The daemon closes this connection: we see end-of-stream or a reset.
    assert!(
        r.next::<ServerMessage>()
            .await
            .map(|m| m.is_none())
            .unwrap_or(true)
    );
    writer.await.unwrap();
    d.client()
        .await
        .request(Command::DaemonStatus)
        .await
        .unwrap();
    d.stop().await;
}

#[tokio::test]
async fn a_held_lock_stops_a_second_daemon_before_it_touches_anything() {
    use std::os::fd::AsRawFd;

    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path());
    let holder = std::fs::File::create(paths.daemon_lock()).unwrap();
    // SAFETY: `holder` owns an open descriptor for the whole call.
    assert_eq!(
        unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    std::fs::write(paths.config_file(), "[github\nhost = ").unwrap();
    let err = Daemon::bind_with(paths.clone(), common::test_options())
        .await
        .err()
        .expect("the lock is held");
    assert!(matches!(err, StartError::AlreadyRunning(_)), "{err}");
    assert!(!paths.socket().exists(), "no socket without the lock");
    assert_eq!(
        std::fs::read_to_string(paths.config_file()).unwrap(),
        "[github\nhost = ",
        "a refused daemon leaves the config alone"
    );
    drop(holder);
    Daemon::bind_with(paths, common::test_options())
        .await
        .expect("free again");
}

#[tokio::test]
async fn the_lock_is_free_again_after_a_stop() {
    let d = TestDaemon::start().await;
    let paths = d.paths.clone();
    let dir = d.stop().await;
    let again = Daemon::bind_with(paths, common::test_options())
        .await
        .expect("the stopped daemon released its lock");
    drop(again);
    drop(dir);
}

#[test]
fn a_daemon_that_loses_the_lock_leaves_the_logs_alone() {
    use std::os::fd::AsRawFd;
    use std::time::{Duration, SystemTime};

    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path());
    let holder = std::fs::File::create(paths.daemon_lock()).unwrap();
    // SAFETY: `holder` owns an open descriptor for the whole call.
    assert_eq!(
        unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    std::fs::create_dir_all(paths.logs_dir()).unwrap();
    let log = paths.logs_dir().join("daemon.log");
    std::fs::write(&log, "running daemon\n").unwrap();
    let long_ago = SystemTime::now() - Duration::from_secs(3 * 86_400);
    std::fs::File::options()
        .write(true)
        .open(&log)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();
    let before = std::fs::read_dir(paths.logs_dir()).unwrap().count();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_clusiad"))
        .arg("--home")
        .arg(dir.path())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("already running"));
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "running daemon\n");
    assert_eq!(
        std::fs::metadata(&log).unwrap().modified().unwrap(),
        long_ago
    );
    assert_eq!(std::fs::read_dir(paths.logs_dir()).unwrap().count(), before);
}
