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

/// A stand-in `clusia` that records its calls and answers as a review with a chat would: the
/// log is the file `log`, and opening the review writes `$STUB_SUMMARY` into it when set.
const STUB_CLUSIA: &str = r#"#!/bin/sh
echo "$*" >> "$STUB_DIR/calls"
case "$*" in
  "config get harness.on_open") printf '%s\n' "$STUB_ON_OPEN" ;;
  "--json agent log "*) cat "$STUB_DIR/log" ;;
  "agent log "*) echo "you: hello" ;;
  "open "*) if [ -n "$STUB_SUMMARY" ]; then printf '%s' "$STUB_SUMMARY" > "$STUB_DIR/log"; fi ;;
  "ask "*)
    if [ -n "$STUB_ASK_EXIT" ]; then exit "$STUB_ASK_EXIT"; fi
    sed -i '' 's/]$/,{"type":"user","at":9,"turn":9,"text":"asked"}]/' "$STUB_DIR/log"
    echo "An answer."
    echo "✓ Read Cargo.toml" >&2 ;;
  "review discard "*)
    if [ -n "$STUB_DISCARD_ENDS" ]; then
      sed -i '' 's/]$/,{"type":"error","at":9,"turn":9,"kind":"interrupted","message":"Stopped because the review ended"}]/' "$STUB_DIR/log"
    fi ;;
esac
exit 0
"#;

/// A stand-in window that stays up until it is killed, under its own path (short sleeps, so
/// nothing outlives it for long).
const STUB_APP: &str = r#"#!/bin/sh
while :; do sleep 0.1; done
"#;

struct Stubbed {
    dir: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl Stubbed {
    fn new(log: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        for (name, text) in [("clusia", STUB_CLUSIA), ("clusia-app", STUB_APP)] {
            let path = bin.join(name);
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(dir.path().join("log"), log).unwrap();
        Self {
            dir,
            home: tempfile::tempdir().unwrap(),
        }
    }

    /// Runs the whole check with `--yes` and no terminal: every question is answered no.
    fn run(&self, env: &[(&str, &str)]) -> std::process::Output {
        let bin = self.dir.path().join("bin");
        let mut cmd = Command::new(script());
        cmd.args(["--yes", "--pr", "rzorzal/clusia#123"])
            .env_clear()
            .env("HOME", self.home.path())
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("STUB_DIR", self.dir.path())
            .env("STUB_ON_OPEN", "wait")
            .env("SP2_CHECK_APP", bin.join("clusia-app"))
            .env("SP2_CHECK_WAIT", "2")
            .stdin(Stdio::null());
        for (key, value) in env {
            cmd.env(key, value);
        }
        cmd.output().unwrap()
    }

    /// The same run with `answers` typed on the terminal, one per question.
    fn run_answering(&self, answers: &str, env: &[(&str, &str)]) -> std::process::Output {
        let input = self.dir.path().join("answers");
        std::fs::write(&input, answers).unwrap();
        let bin = self.dir.path().join("bin");
        let mut cmd = Command::new(script());
        cmd.args(["--yes", "--pr", "rzorzal/clusia#123"])
            .env_clear()
            .env("HOME", self.home.path())
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("STUB_DIR", self.dir.path())
            .env("STUB_ON_OPEN", "wait")
            .env("SP2_CHECK_APP", bin.join("clusia-app"))
            .env("SP2_CHECK_WAIT", "2")
            .stdin(std::fs::File::open(&input).unwrap());
        for (key, value) in env {
            cmd.env(key, value);
        }
        cmd.output().unwrap()
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

const OLD_SUMMARY: &str = r#"[{"type":"user","at":1,"turn":1,"text":"hi"},{"type":"text","at":2,"turn":1,"text":"Old."}]"#;
const NEW_SUMMARY: &str = r#"[{"type":"user","at":1,"turn":1,"text":"hi"},{"type":"text","at":2,"turn":1,"text":"Old."},{"type":"text","at":3,"turn":2,"text":"New."}]"#;

#[test]
fn a_summary_from_an_earlier_run_does_not_pass() {
    let stub = Stubbed::new(OLD_SUMMARY);
    let out = stub.run(&[]);
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(said.contains("FAIL  the agent wrote its summary"), "{said}");
    let stub = Stubbed::new(OLD_SUMMARY);
    let out = stub.run(&[("STUB_SUMMARY", NEW_SUMMARY)]);
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(said.contains("PASS  the agent wrote its summary"), "{said}");
}

#[test]
fn a_no_keeps_the_review_and_the_setting_it_changed_comes_back() {
    let stub = Stubbed::new(OLD_SUMMARY);
    let out = stub.run(&[("STUB_SUMMARY", NEW_SUMMARY)]);
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(said.contains("Skipped: the review was kept"), "{said}");
    let calls = stub.calls();
    assert!(
        !calls.iter().any(|c| c.starts_with("review discard")),
        "{calls:?}"
    );
    let at = |call: &str| calls.iter().position(|c| c == call).expect(call);
    assert!(
        at("config set harness.on_open summarize") < at("open rzorzal/clusia#123"),
        "the summary is turned on before the review opens: {calls:?}"
    );
    assert_eq!(
        calls.last().map(String::as_str),
        Some("config set harness.on_open wait"),
        "{calls:?}"
    );
}

#[test]
fn a_setting_that_cannot_be_read_is_not_left_changed_silently() {
    let stub = Stubbed::new(OLD_SUMMARY);
    let out = stub.run(&[("STUB_ON_OPEN", ""), ("STUB_SUMMARY", NEW_SUMMARY)]);
    let said =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("could not be read"), "{said}");
}

#[test]
fn ctrl_c_in_a_question_ends_the_check_and_puts_the_setting_back() {
    let stub = Stubbed::new(OLD_SUMMARY);
    let out = stub.run(&[("STUB_SUMMARY", NEW_SUMMARY), ("STUB_ASK_EXIT", "130")]);
    assert_eq!(out.status.code(), Some(130));
    let calls = stub.calls();
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("ask ")).count(),
        1,
        "{calls:?}"
    );
    assert_eq!(
        calls.last().map(String::as_str),
        Some("config set harness.on_open wait"),
        "{calls:?}"
    );
}

#[test]
fn the_window_it_started_is_closed_when_it_ends() {
    let stub = Stubbed::new(OLD_SUMMARY);
    let out = stub.run(&[("STUB_SUMMARY", NEW_SUMMARY)]);
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(said.contains("PASS  the window was started"), "{said}");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let window = stub.dir.path().join("bin/clusia-app");
    let running = Command::new("pgrep")
        .arg("-f")
        .arg(&window)
        .output()
        .unwrap();
    let left = String::from_utf8_lossy(&running.stdout).to_string();
    // Cleans up whatever the assertion below finds.
    let _ = Command::new("pkill").arg("-f").arg(&window).status();
    assert!(
        left.trim().is_empty(),
        "the window is still running: {left}"
    );
}

#[test]
fn a_discard_checks_that_the_running_turn_was_stopped() {
    // Six step questions answered no, then yes to the discard.
    let answers = "n\nn\nn\nn\nn\nn\ny\n";
    let stub = Stubbed::new(OLD_SUMMARY);
    let out = stub.run_answering(
        answers,
        &[("STUB_SUMMARY", NEW_SUMMARY), ("STUB_DISCARD_ENDS", "1")],
    );
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    for line in [
        "PASS  a long question is running",
        "PASS  the review was discarded",
        "PASS  the session ended: the running turn was stopped",
        "PASS  the review is gone",
    ] {
        assert!(said.contains(line), "{line}: {said}");
    }
    assert!(
        stub.calls()
            .iter()
            .any(|c| c == "review discard rzorzal/clusia#123")
    );

    // An end left by an earlier run is not this run's.
    let ended_before = r#"[{"type":"user","at":1,"turn":1,"text":"hi"},{"type":"error","at":2,"turn":1,"kind":"interrupted","message":"Stopped because the review ended"}]"#;
    let stub = Stubbed::new(ended_before);
    let out = stub.run_answering(answers, &[("STUB_SUMMARY", NEW_SUMMARY)]);
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        said.contains("FAIL  the session ended: the running turn was stopped"),
        "{said}"
    );
}
