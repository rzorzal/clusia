//! The Story and Calls owner-check script parses, lists its steps, asks before it spends anything,
//! answers nothing for the owner, pushes and publishes nothing, stops the story it started and
//! puts back every setting it changed. No test runs a real agent through the script: the
//! programs the script finds are fakes.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sp3-m3-check.sh")
}

fn source() -> String {
    std::fs::read_to_string(script()).unwrap()
}

fn fake(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A `clusia` that records every command and answers like a settled review: old settings, a
/// static story that a write turns into version 2, a call graph with a centre and three levels,
/// and one item in the draft. `review discard` fails.
fn recording_clusia(dir: &Path, log: &Path) {
    fake(
        dir,
        "clusia",
        &format!(
            r#"echo "$*" >> {log}
case "$*" in
  "--json story "*) echo '{{"latest": {{"number": 2, "commit": "9be1d07"}}, "head": "9be1d07", "steps": []}}' ;;
  "--json calls "*) echo '{{"center": "src/auth/store.rs#TokenStore::refresh#38", "depth": 3}}' ;;
  "story "*"--write"*) exit 0 ;;
  "story "*) printf '1  TokenStore::refresh\n2  Other files\nWritten by Claude Code from commit 9be1d07\n' ;;
  "calls "*"--depth 1"*) printf '↑1  TokenStore::token  not touched\n●  TokenStore::refresh\n↓1  HttpClient::exchange  changed\n' ;;
  "calls "*) printf '↑3  auth_middleware  not touched\n↑1  TokenStore::token  not touched\n●  TokenStore::refresh\n↓1  HttpClient::exchange  changed\n' ;;
  "--json review status"*) echo '"body": "x"' ;;
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
        "scripts/sp3-m3-check.sh is not executable"
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
        "static:",
        "views:",
        "write:",
        "comment:",
        "push:",
        "rewrite:",
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
    // The story is written detached from the keyboard, so a request for a command waits for the
    // window; the script never feeds an answer in.
    assert!(source.contains(r#"clusia story "$PR" --write </dev/null"#));
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
        r#"config set harness.story "$OLD_STORY""#,
    ] {
        assert!(source.contains(restored), "{restored} is not put back");
    }
    assert!(source.contains("put back when it ends"));
    assert!(source.contains("Skipped: the review was kept"));
}

#[test]
fn the_story_it_started_is_stopped_when_the_check_ends() {
    let source = source();
    assert!(source.contains(r#"kill -INT "$STORY_PID""#));
    assert!(source.contains("trap 'stop_story; close_window; restore_settings; drop_err' EXIT"));
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
    assert!(said.contains("one small commit"), "{said}");
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
        .write_all("y\n".repeat(60).as_bytes())
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
    let sets = |part: &[&str]| -> Vec<String> {
        part.iter()
            .filter(|l| l.starts_with("config set"))
            .map(|l| l.to_string())
            .collect()
    };
    assert_eq!(
        sets(&lines[failed + 1..]),
        [
            "config set harness.on_open old",
            "config set harness.check_security old",
            "config set harness.audit old",
            "config set harness.diagrams old",
            "config set harness.keywords_3d old",
            "config set harness.story old",
        ],
        "{asked}"
    );
    assert_eq!(
        sets(&lines[..failed]),
        [
            "config set harness.on_open wait",
            "config set harness.check_security false",
            "config set harness.audit false",
            "config set harness.diagrams false",
            "config set harness.keywords_3d false",
            "config set harness.story false",
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
fn the_static_and_cli_steps_check_the_output_and_the_depth() {
    let source = source();
    let step = |name: &str| -> String {
        source
            .split(&format!("{name}() {{"))
            .nth(1)
            .and_then(|rest| rest.split("\n}\n").next())
            .unwrap_or_else(|| panic!("the {name} step"))
            .to_string()
    };
    let stat = step("step_static");
    assert!(stat.contains("Static order"), "{stat}");
    assert!(
        stat.contains("--depth 3") && stat.contains("--depth 1"),
        "{stat}"
    );
    assert!(stat.contains("↑2"), "depth 1 shows no second level: {stat}");
    let cli = step("step_cli");
    assert!(cli.contains("Written by"), "{cli}");
    assert!(cli.contains("--json"), "{cli}");
    assert!(cli.contains("-eq 2"), "a usage error exits 2: {cli}");
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

/// A `clusia` for the comment step: each read of the draft shows one more item, and a story
/// write ignores Ctrl-C (as a job in the background of a script does) and writes its pid down.
fn growing_clusia(dir: &Path) {
    let counter = dir.join("count");
    let pid = dir.join("write.pid");
    fake(
        dir,
        "clusia",
        &format!(
            r#"case "$*" in
  *"--write"*) echo $$ > {pid}; exec sleep 60 ;;
  "--json story "*) echo '{{"latest": {{"number": 1}}, "head": "9be1d07"}}' ;;
  "story "*) echo 'Written by Claude Code from commit 9be1d07' ;;
  "--json review status"*)
    n=$(( $(cat {counter} 2>/dev/null || echo 0) + 1 ))
    echo $n > {counter}
    i=0; while [ $i -lt $n ]; do echo '"body": "x"'; i=$((i+1)); done ;;
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
fn the_comment_step_sees_draft_items_appear_and_a_write_that_ignores_ctrl_c_is_ended() {
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
        .write_all("y\n".repeat(60).as_bytes())
        .unwrap();
    let started = std::time::Instant::now();
    let out = child.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    for item in [
        "a draft item from the step comment appeared",
        "a draft item from the node comment appeared",
    ] {
        assert!(
            said.lines().any(|l| l.contains("PASS") && l.contains(item)),
            "{item}: {said}"
        );
    }
    assert!(
        started.elapsed() < std::time::Duration::from_secs(60),
        "the script hung at the story"
    );
    let pid: i32 = std::fs::read_to_string(bin.join("write.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only asks whether the process exists.
    assert_ne!(
        unsafe { libc::kill(pid, 0) },
        0,
        "the story write is still running"
    );
}

#[test]
fn the_readme_names_the_check_the_two_commands_and_the_setting() {
    let readme =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../README.md"))
            .unwrap();
    for needle in [
        "sp3-m3-check.sh --pr owner/repo#N",
        "`clusia story owner/repo#123",
        "`clusia calls owner/repo#123",
        "**Story and Calls.**",
        "Write the story when I open Story",
    ] {
        assert!(readme.contains(needle), "{needle} is not in the README");
    }
}
