//! Real clusiad + real tray. Shows a menu bar item for a few seconds, so it is ignored by default:
//! cargo test -p clusia-tray --test lifecycle -- --ignored

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Kills and reaps the daemon when the test ends, including on a failed assertion.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn clusiad_bin() -> PathBuf {
    // target/<profile>/deps/lifecycle-<hash> → target/<profile>/clusiad
    let exe = std::env::current_exe().unwrap();
    let bin = exe.parent().unwrap().parent().unwrap().join("clusiad");
    assert!(
        bin.is_file(),
        "build clusiad first: cargo build -p clusiad ({})",
        bin.display()
    );
    bin
}

fn tray_pids(home: &Path) -> Vec<String> {
    let o = Command::new("pgrep")
        .arg("-f")
        .arg(format!("clusia-tray --home {}", home.display()))
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(String::from)
        .collect()
}

fn wait_until(limit: Duration, mut ok: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    ok()
}

#[test]
#[ignore = "shows a real menu bar item; run explicitly with --ignored"]
fn tray_follows_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    // Declared after `dir`, so it drops (and kills the daemon) before the home is removed.
    let mut daemon = KillOnDrop(
        Command::new(clusiad_bin())
            .arg("--home")
            .arg(home)
            .env("CLUSIA_TRAY_BIN", env!("CARGO_BIN_EXE_clusia-tray"))
            .env("CLUSIA_APP_BIN", "none")
            .env("CLUSIA_GITHUB_API", "http://127.0.0.1:9")
            .env("CLUSIA_GH_BIN", "/nonexistent/gh")
            .env("CLUSIA_SECRET_STORE", "memory")
            .env_remove("CLUSIA_GITHUB_TOKEN")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert!(
        wait_until(Duration::from_secs(10), || tray_pids(home).len() == 1),
        "tray never started"
    );
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(tray_pids(home).len(), 1, "exactly one tray");
    // SIGKILL: no shutdown path runs, so the tray must notice the closed socket by itself.
    daemon.0.kill().unwrap();
    daemon.0.wait().unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || tray_pids(home).is_empty()),
        "tray outlived a killed daemon: {:?}",
        tray_pids(home)
    );
}
