//! End-to-end: the real `clusia` binary driving the real `clusiad` binary.
//! Requires `cargo test --workspace` (which builds `clusiad` next to `clusia`).

use std::path::PathBuf;
use std::process::{Command, Output};

fn clusiad_bin() -> PathBuf {
    // target/<profile>/deps/cli-<hash> → target/<profile>/clusiad
    let exe = std::env::current_exe().unwrap();
    let bin = exe.parent().unwrap().parent().unwrap().join("clusiad");
    assert!(
        bin.exists(),
        "clusiad not built at {}; run `cargo test --workspace`",
        bin.display()
    );
    bin
}

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn clusia(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_clusia"))
            .arg("--home")
            .arg(self.dir.path())
            .args(args)
            .env("CLUSIA_DAEMON_BIN", clusiad_bin())
            .output()
            .unwrap()
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.clusia(&["daemon", "stop"]);
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).trim().to_string()
}

#[test]
fn status_without_daemon_exits_3_with_hint() {
    let h = Home::new();
    let o = h.clusia(&["daemon", "status"]);
    assert_eq!(o.status.code(), Some(3));
    assert!(stderr(&o).contains("clusia daemon start"), "{}", stderr(&o));
}

#[test]
fn json_errors_are_machine_readable() {
    let h = Home::new();
    let o = h.clusia(&["--json", "daemon", "status"]);
    let v: serde_json::Value = serde_json::from_str(&stderr(&o)).unwrap();
    assert_eq!(v["error"]["kind"], "not_running");
}

#[test]
fn full_lifecycle() {
    let h = Home::new();
    let o = h.clusia(&["daemon", "start"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("started"), "{}", stdout(&o));

    let o = h.clusia(&["--json", "daemon", "status"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let v: serde_json::Value = serde_json::from_str(&stdout(&o)).unwrap();
    assert!(v["pid"].as_u64().unwrap() > 0);

    let o = h.clusia(&["config", "set", "github.poll_interval_secs", "120"]);
    assert_eq!(
        stdout(&o),
        "github.poll_interval_secs = 120",
        "{}",
        stderr(&o)
    );
    assert_eq!(
        stdout(&h.clusia(&["config", "get", "github.poll_interval_secs"])),
        "120"
    );
    assert!(stdout(&h.clusia(&["config", "show"])).contains("poll_interval_secs = 120"));

    let o = h.clusia(&["daemon", "stop"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o), "Clúsia daemon stopped");
    assert_eq!(h.clusia(&["daemon", "status"]).status.code(), Some(3));
}

#[test]
fn invalid_config_value_exits_1() {
    let h = Home::new();
    assert!(h.clusia(&["daemon", "start"]).status.success());
    let o = h.clusia(&["config", "set", "github.poll_interval_secs", "abc"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("github.poll_interval_secs"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn start_twice_reports_already_running() {
    let h = Home::new();
    assert!(h.clusia(&["daemon", "start"]).status.success());
    let o = h.clusia(&["daemon", "start"]);
    assert!(o.status.success());
    assert!(stdout(&o).contains("already running"), "{}", stdout(&o));
}

#[test]
fn commands_autostart_the_daemon() {
    let h = Home::new();
    let o = h.clusia(&["config", "get", "github.host"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o), "github.com");
    assert!(
        stderr(&o).contains("started the Clúsia daemon"),
        "{}",
        stderr(&o)
    );
    assert!(h.clusia(&["daemon", "status"]).status.success());
}
