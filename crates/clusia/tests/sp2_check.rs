//! The owner-check script parses, lists its steps, asks before it acts and never does more than
//! it says.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sp2-m1-check.sh")
}

#[test]
fn the_script_is_executable_bash_that_parses() {
    let mode = std::fs::metadata(script()).unwrap().permissions().mode();
    assert!(
        mode & 0o111 != 0,
        "scripts/sp2-m1-check.sh is not executable"
    );
    assert!(
        Command::new("bash")
            .arg("-n")
            .arg(script())
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn listing_names_every_step_and_changes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let out = Command::new(script())
        .arg("--list")
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(out.status.success());
    let steps = String::from_utf8(out.stdout).unwrap();
    for step in [
        "preflight:",
        "summary:",
        "question:",
        "suggestion:",
        "resume:",
        "denied:",
        "terminal:",
        "end:",
    ] {
        assert!(steps.lines().any(|l| l.starts_with(step)), "{step} missing");
    }
    assert_eq!(
        std::fs::read_dir(home.path()).unwrap().count(),
        0,
        "listing wrote into HOME"
    );
}

#[test]
fn bad_options_are_refused() {
    for args in [&["--nope"][..], &["--pr"][..], &[][..], &["--yes"][..]] {
        let out = Command::new(script())
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(!out.stderr.is_empty(), "{args:?}");
    }
}

#[test]
fn the_script_never_widens_what_the_agent_may_do_or_removes_files() {
    let source = std::fs::read_to_string(script()).unwrap();
    for forbidden in [
        "sudo",
        "rm -r",
        "dangerously-skip-permissions",
        "bypassPermissions",
        "review publish",
        "curl",
    ] {
        assert!(!source.contains(forbidden), "{forbidden} in the script");
    }
}

#[test]
fn a_no_keeps_the_review_and_the_setting_it_changed_comes_back() {
    let source = std::fs::read_to_string(script()).unwrap();
    assert!(source.contains("Skipped: the review was kept"));
    assert!(source.contains(r#"config set harness.on_open "$OLD_ON_OPEN""#));
    assert!(source.contains("put back when it ends"));
}

#[test]
fn the_run_asks_before_it_spends_anything_and_stops_on_no() {
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(script())
        .args(["--pr", "rzorzal/clusia#123"])
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"n\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(said.contains("Claude Code"), "{said}");
    assert!(
        said.contains("discards"),
        "it says it discards the review: {said}"
    );
    assert!(said.contains("Nothing was run"), "{said}");
}
