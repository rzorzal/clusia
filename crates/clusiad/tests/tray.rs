mod common;

use std::time::Duration;

use common::{TestDaemon, test_options};

#[tokio::test]
async fn daemon_spawns_the_tray_with_its_home_and_stops_it() {
    let dir = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let record = bin.path().join("record");
    let prog = bin.path().join("fake-tray");
    clusia_core::testkit::write_executable(
        &prog,
        &format!("echo \"$$ $@\" >> '{}'\nexec sleep 30", record.display()),
    );

    let mut options = test_options();
    options.tray_program = Some(prog);
    let root = dir.path().to_path_buf();
    let daemon = TestDaemon::start_with(dir, options).await;

    let line = {
        let mut waited = Duration::ZERO;
        loop {
            if let Ok(s) = std::fs::read_to_string(&record)
                && let Some(l) = s.lines().next()
            {
                break l.to_string();
            }
            assert!(waited < Duration::from_secs(5), "tray never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
            waited += Duration::from_millis(20);
        }
    };
    let (pid, args) = line.split_once(' ').unwrap();
    assert_eq!(args, format!("--home {}", root.display()));

    daemon.stop().await;
    let alive = std::process::Command::new("kill")
        .args(["-0", pid])
        .status()
        .unwrap()
        .success();
    assert!(!alive, "the tray outlived the daemon");
    assert_eq!(
        std::fs::read_to_string(&record).unwrap().lines().count(),
        1,
        "spawned exactly once"
    );
}

#[tokio::test]
async fn the_tray_is_told_the_daemon_started_it() {
    let dir = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let record = bin.path().join("record");
    let prog = bin.path().join("fake-tray");
    clusia_core::testkit::write_executable(
        &prog,
        &format!(
            "echo \"$CLUSIA_LAUNCHED_BY\" >> '{}'\nexec sleep 30",
            record.display()
        ),
    );

    let mut options = test_options();
    options.tray_program = Some(prog);
    let daemon = TestDaemon::start_with(dir, options).await;

    let mut waited = Duration::ZERO;
    let line = loop {
        if let Ok(s) = std::fs::read_to_string(&record)
            && let Some(l) = s.lines().next()
        {
            break l.to_string();
        }
        assert!(waited < Duration::from_secs(5), "tray never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += Duration::from_millis(20);
    };
    assert_eq!(line, "daemon");
    daemon.stop().await;
}
