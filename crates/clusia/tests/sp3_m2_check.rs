//! The 3D keyword view owner-check script parses, lists its steps, asks before it spends anything,
//! answers nothing for the owner, pushes and publishes nothing, stops the drawing it started and
//! puts back every setting it changed. No test runs a real agent through the script: the
//! programs the script finds are fakes.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sp3-m2-check.sh")
}

fn source() -> String {
    std::fs::read_to_string(script()).unwrap()
}

fn fake(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A `clusia` that records every command and answers like a settled review: old settings,
/// a ready flow entry and a ready keywords entry in version 2, one item from a diagram box.
/// `review discard` fails.
fn recording_clusia(dir: &Path, log: &Path) {
    fake(
        dir,
        "clusia",
        &format!(
            r#"echo "$*" >> {log}
case "$*" in
  "diagrams "*"--json"*) echo '{{"version": {{"number": 2}}, "state": "ready_map"}}' ;;
  "diagrams "*) printf 'flow  flow  Refresh flow  ready\nkeywords  keywords  Where the words live  ready · 4 families · 64 places\nVersion 2 · drawn by Claude Code from 9be1d07\n' ;;
  "--json review status"*) echo '"origin": "diagram"' ;;
  "config get"*) echo old ;;
  "review discard"*) exit 1 ;;
  *) echo old ;;
esac
exit 0"#,
            log = log.display()
        ),
    );
}

#[test]
fn the_script_is_executable_bash_that_parses() {
    let mode = std::fs::metadata(script()).unwrap().permissions().mode();
    assert!(
        mode & 0o111 != 0,
        "scripts/sp3-m2-check.sh is not executable"
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
        "draw:",
        "view:",
        "usecase:",
        "comment:",
        "setting:",
        "cli:",
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
fn the_script_never_pushes_publishes_or_removes_files() {
    let source = source();
    for forbidden in [
        "sudo",
        "rm -r",
        "git push",
        "review publish",
        "dangerously",
        "bypassPermissions",
        "--yolo",
        "--full-auto",
    ] {
        assert!(!source.contains(forbidden), "{forbidden} in the script");
    }
    for line in source.lines() {
        let line = line.trim_start();
        for program in ["curl ", "rm ", "git ", "claude ", "codex ", "ollama "] {
            assert!(
                !line.starts_with(program),
                "the script runs {line:?} itself"
            );
        }
    }
}

#[test]
fn the_script_answers_nothing_for_the_owner() {
    let source = source();
    // The drawing runs detached from the keyboard, so a request for a command waits for the
    // window; the script never feeds an answer in.
    assert!(source.contains(r#"clusia diagrams "$PR" --draw </dev/null"#));
    assert!(!source.contains("printf 'o"));
    assert!(!source.contains("echo o |"));
    assert!(!source.contains("yes |"));
}

#[test]
fn every_setting_it_changes_comes_back_and_a_no_keeps_the_review() {
    let source = source();
    for restored in [
        r#"config set harness.on_open "$OLD_ON_OPEN""#,
        r#"config set harness.check_security "$OLD_SECURITY""#,
        r#"config set harness.audit "$OLD_AUDIT""#,
        r#"config set harness.diagrams "$OLD_DIAGRAMS""#,
        r#"config set harness.keywords_3d "$OLD_KEYWORDS""#,
    ] {
        assert!(source.contains(restored), "{restored} is not put back");
    }
    assert!(source.contains("put back when it ends"));
    assert!(source.contains("Skipped: the review was kept"));
}

#[test]
fn the_drawing_it_started_is_stopped_when_the_check_ends() {
    let source = source();
    assert!(source.contains(r#"kill -INT "$DRAW_PID""#));
    assert!(source.contains("trap 'stop_draw; close_window; restore_settings; drop_err' EXIT"));
    assert!(
        source.contains("trap 'exit 130' INT TERM HUP"),
        "a Ctrl-C passed on by a command must end the script"
    );
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
    assert!(said.contains("YOUR agent"), "{said}");
    assert!(said.contains("answer every request for a"), "{said}");
    assert!(said.contains("publishes nothing"), "{said}");
    assert!(said.contains("discards the review"), "{said}");
    assert!(said.contains("Nothing was run"), "{said}");
}

#[test]
fn a_no_to_the_settings_question_keeps_everything_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let log = dir.path().join("clusia.log");
    recording_clusia(&bin, &log);
    let mut child = Command::new(script())
        .args(["--yes", "--pr", "rzorzal/clusia#123"])
        .env_clear()
        .env("HOME", dir.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("SP3_CHECK_WAIT", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"n\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(said.contains("Nothing was changed"), "{said}");
    let asked = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(!asked.contains("config set"), "settings changed: {asked}");
    assert!(!asked.contains("open "), "a review was opened: {asked}");
}

#[test]
fn a_yes_puts_the_settings_back_after_a_step_fails() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let log = dir.path().join("clusia.log");
    recording_clusia(&bin, &log);
    let mut child = Command::new(script())
        .args(["--yes", "--pr", "rzorzal/clusia#123"])
        .env_clear()
        .env("HOME", dir.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("SP3_CHECK_WAIT", "1")
        .env("SP3_CHECK_APP", dir.path().join("no-such-window"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Every question is answered yes, so the last step discards the review, which fails.
    child
        .stdin
        .take()
        .unwrap()
        .write_all("y\n".repeat(40).as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(1), "{said}");
    let asked = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = asked.lines().collect();
    let failed = lines
        .iter()
        .position(|l| l.starts_with("review discard"))
        .unwrap_or_else(|| panic!("the last step never ran: {asked}\n{said}"));
    let after: Vec<&str> = lines[failed + 1..]
        .iter()
        .copied()
        .filter(|l| l.starts_with("config set"))
        .collect();
    assert_eq!(
        after,
        [
            "config set harness.on_open old",
            "config set harness.check_security old",
            "config set harness.audit old",
            "config set harness.diagrams old",
            "config set harness.keywords_3d old",
        ],
        "{asked}"
    );
    let before: Vec<&str> = lines[..failed]
        .iter()
        .copied()
        .filter(|l| l.starts_with("config set"))
        .collect();
    assert_eq!(
        before,
        [
            "config set harness.on_open wait",
            "config set harness.check_security false",
            "config set harness.audit false",
            "config set harness.diagrams false",
            "config set harness.keywords_3d true",
        ],
        "{asked}"
    );
}

#[test]
fn a_failing_mktemp_ends_the_script_and_deletes_nothing_around_it() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    fake(&bin, "mktemp", "exit 1");
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();
    let dotfile = work.join(".keep");
    std::fs::write(&dotfile, "mine").unwrap();
    let out = Command::new(script())
        .args(["--yes", "--pr", "rzorzal/clusia#123"])
        .current_dir(&work)
        .env_clear()
        .env("HOME", dir.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(std::fs::read_to_string(&dotfile).unwrap(), "mine");
}

#[test]
fn the_cli_step_checks_the_list_the_json_and_the_usage_error() {
    let source = source();
    let step = source
        .split("step_cli() {")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("the cli step");
    assert!(
        step.contains("ready · [0-9]+ famil(y|ies) · [0-9]+ places?"),
        "{step}"
    );
    assert!(step.contains("ready_map"), "{step}");
    assert!(step.contains("--json"), "{step}");
    assert!(!step.contains("stale"), "{step}");
    assert!(step.contains("-eq 2"), "a usage error exits 2: {step}");
}

#[test]
fn ctrl_c_at_the_question_ends_the_script_instead_of_going_on() {
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
    let mut said = String::new();
    let mut stdout = child.stdout.take().unwrap();
    // The question is on the screen once "Continue?" was printed: the script waits in `read`.
    let mut byte = [0u8; 1];
    while !said.ends_with("Continue? [y/N] ") {
        assert_eq!(stdout.read(&mut byte).unwrap(), 1, "{said}");
        said.push(byte[0] as char);
    }
    // SAFETY: signals a child this test spawned and still owns.
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let status = child.wait().unwrap();
    drop(child.stdin.take());
    assert_eq!(status.code(), Some(130), "{status:?}");
}

/// A `clusia` for the comment step: each read of the draft shows one more item from a diagram
/// box, and a drawing ignores Ctrl-C (as a job in the background of a script does) and writes
/// its pid down.
fn growing_clusia(dir: &Path) {
    let counter = dir.join("count");
    let pid = dir.join("draw.pid");
    fake(
        dir,
        "clusia",
        &format!(
            r#"case "$*" in
  *"--draw"*) echo $$ > {pid}; exec sleep 60 ;;
  "diagrams "*"--json"*) echo '{{"version": {{"number": 2}}, "state": "ready_map"}}' ;;
  "diagrams "*) printf 'flow  flow  Refresh flow  ready
keywords  keywords  Where the words live  ready · 4 families · 64 places
Version 2 · drawn by Claude Code from 9be1d07
' ;;
  "--json review status"*)
    n=$(( $(cat {counter} 2>/dev/null || echo 0) + 1 ))
    echo $n > {counter}
    i=0; while [ $i -lt $n ]; do echo '"origin": "diagram"'; i=$((i+1)); done ;;
  "review discard"*) exit 1 ;;
  *) echo old ;;
esac
exit 0"#,
            counter = counter.display(),
            pid = pid.display()
        ),
    );
}

#[test]
fn the_comment_step_sees_a_draft_item_appear_and_a_drawing_that_ignores_ctrl_c_is_ended() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    growing_clusia(&bin);
    // The fake lives in `bin`; its counter and pid files sit there too.
    let mut child = Command::new(script())
        .args(["--yes", "--pr", "rzorzal/clusia#123"])
        .env_clear()
        .env("HOME", dir.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("SP3_CHECK_WAIT", "3")
        .env("SP3_CHECK_APP", dir.path().join("no-such-window"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all("y\n".repeat(40).as_bytes())
        .unwrap();
    let started = std::time::Instant::now();
    let out = child.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        said.lines()
            .any(|l| l.contains("PASS") && l.contains("a draft item from a diagram box appeared")),
        "{said}"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "the script hung at the drawing"
    );
    let pid: i32 = std::fs::read_to_string(bin.join("draw.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only asks whether the process exists.
    assert_ne!(
        unsafe { libc::kill(pid, 0) },
        0,
        "the drawing is still running"
    );
}
