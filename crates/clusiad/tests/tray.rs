mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use common::{TestDaemon, test_options};

#[tokio::test]
async fn daemon_spawns_the_tray_with_its_home_and_stops_it() {
    let dir = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let record = bin.path().join("record");
    let prog = bin.path().join("fake-tray");
    std::fs::write(
        &prog,
        format!(
            "#!/bin/sh\necho \"$$ $@\" >> '{}'\nexec sleep 30\n",
            record.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(0o755)).unwrap();

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
