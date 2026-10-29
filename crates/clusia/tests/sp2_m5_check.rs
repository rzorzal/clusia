//! The Codex owner-check script parses, lists its steps, asks before it acts, refuses a machine
//! where `codex --version` fails without changing anything, answers nothing for the owner, pushes
//! and publishes nothing itself, and puts back every setting it changed. No test runs a real
//! `codex`: the programs the script finds are fakes.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sp2-m5-check.sh")
}

fn source() -> String {
    std::fs::read_to_string(script()).unwrap()
}

fn fake(dir: &Path, name: &str, body: &str) {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn the_script_is_executable_bash_that_parses() {
    let mode = std::fs::metadata(script()).unwrap().permissions().mode();
    assert!(
        mode & 0o111 != 0,
        "scripts/sp2-m5-check.sh is not executable"
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
        "switch:",
        "open:",
        "chat:",
        "isolation:",
        "approve:",
        "checks:",
        "revalidate:",
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
fn the_script_never_pushes_publishes_or_widens_what_the_agent_may_do() {
    let source = source();
    for forbidden in [
        "sudo",
        "rm -r",
        "git push",
        "review publish",
        "dangerously-bypass-approvals-and-sandbox",
        "--yolo",
        "--full-auto",
        "acceptForSession",
        "CODEX_HOME",
    ] {
        assert!(!source.contains(forbidden), "{forbidden} in the script");
    }
    for line in source.lines() {
        let line = line.trim_start();
        assert!(
            !line.starts_with("curl ") && !line.starts_with("rm "),
            "the script runs {line:?} itself"
        );
    }
}

#[test]
fn the_script_answers_nothing_for_the_owner() {
    let source = source();
    assert!(source.contains("</dev/null"));
    assert!(!source.contains("printf 'o"));
    assert!(!source.contains("echo o |"));
    assert!(!source.contains("yes |"));
}

#[test]
fn every_setting_it_changes_comes_back() {
    let source = source();
    for restored in [
        r#"config set harness.kind "$OLD_KIND""#,
        r#"config set harness.program "$OLD_PROGRAM""#,
        r#"config set harness.revalidate "$OLD_REVALIDATE""#,
        r#"config set harness.revalidate_quiet_secs "$OLD_QUIET""#,
        r#"config set harness.on_open "$OLD_ON_OPEN""#,
        r#"config set harness.check_security "$OLD_SECURITY""#,
        r#"config set harness.audit "$OLD_AUDIT""#,
    ] {
        assert!(source.contains(restored), "{restored} is not put back");
    }
    assert!(source.contains("put back when it ends"));
    let kind = source
        .find(r#"config set harness.kind "$OLD_KIND""#)
        .unwrap();
    let program = source
        .find(r#"config set harness.program "$OLD_PROGRAM""#)
        .unwrap();
    assert!(
        kind < program,
        "the program is restored after the kind: a change of kind clears it"
    );
}

#[test]
fn the_preflight_compares_the_schema_and_prints_a_line_per_name() {
    let source = source();
    for needle in [
        "generate-json-schema",
        "approvalsReviewer",
        "ThreadStartParams",
        "grantRoot",
        "item/commandExecution/requestApproval",
        "the schema has $name",
    ] {
        assert!(source.contains(needle), "{needle} is not checked");
    }
}

#[test]
fn the_isolation_and_second_message_checks_exist() {
    let source = source();
    for needle in [
        "a second message in the same process",
        "step_isolation",
        "MCP server",
        "hook",
    ] {
        assert!(source.contains(needle), "{needle}");
    }
}

#[test]
fn the_run_asks_before_it_does_anything_and_stops_on_no() {
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
    assert!(said.contains("Codex"), "{said}");
    assert!(said.contains("answer every request for a"), "{said}");
    assert!(said.contains("you push"), "{said}");
    assert!(said.contains("publishes nothing"), "{said}");
    assert!(said.contains("asks you before it discards"), "{said}");
    assert!(said.contains("Nothing was run"), "{said}");
}

#[test]
fn it_refuses_a_machine_where_codex_does_not_run_and_changes_nothing() {
    for codex in [None, Some("echo 'killed' >&2; exit 137")] {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let log = dir.path().join("clusia.log");
        // `clusia` records what it is asked and answers every command.
        fake(
            &bin,
            "clusia",
            &format!("echo \"$*\" >> {}\necho old\nexit 0", log.display()),
        );
        if let Some(body) = codex {
            fake(&bin, "codex", body);
        }
        let out = Command::new(script())
            .args(["--yes", "--pr", "rzorzal/clusia#123"])
            .env_clear()
            .env("HOME", dir.path())
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("CLUSIA_CODEX_BIN", "/nonexistent/codex")
            .env("CLUSIA_DOCKER_BIN", "/nonexistent/docker")
            .env("CLUSIA_SANDBOX_EXEC_BIN", "/nonexistent/sandbox-exec")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout).to_string();
        assert_eq!(out.status.code(), Some(1), "{said}");
        assert!(said.contains("codex --version"), "{said}");
        assert!(said.contains("Nothing was changed"), "{said}");
        let asked = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(!asked.contains("config set"), "settings changed: {asked}");
        assert!(!asked.contains("open "), "a review was opened: {asked}");
    }
}

#[test]
fn a_no_to_the_settings_question_keeps_everything_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let log = dir.path().join("clusia.log");
    fake(
        &bin,
        "clusia",
        &format!("echo \"$*\" >> {}\necho old\nexit 0", log.display()),
    );
    fake(
        &bin,
        "codex",
        "case \"$*\" in \"--version\") echo 'codex-cli 0.162.1' ;; esac\nexit 0",
    );
    let mut child = Command::new(script())
        .args(["--yes", "--pr", "rzorzal/clusia#123"])
        .env_clear()
        .env("HOME", dir.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("CLUSIA_CODEX_BIN", "/nonexistent/codex")
        .env("CLUSIA_DOCKER_BIN", "/nonexistent/docker")
        .env("CLUSIA_SANDBOX_EXEC_BIN", "/nonexistent/sandbox-exec")
        .env("SP2_CHECK_WAIT", "2")
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
}

#[test]
fn the_review_is_discarded_only_after_the_owner_says_so() {
    let source = source();
    let question = source
        .find("yes_or_no \"Discard the review now?")
        .expect("the discard is not asked");
    let discard = source
        .find("clusia review discard")
        .expect("the review is never discarded");
    assert!(question < discard, "the review is discarded before asking");
    assert!(source.contains("asks you before it discards the review"));
}

#[test]
fn it_refuses_when_an_old_setting_cannot_be_read_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let log = dir.path().join("clusia.log");
    // `harness.audit` has no readable value: the daemon is down for that key.
    fake(
        &bin,
        "clusia",
        &format!(
            "echo \"$*\" >> {}\ncase \"$*\" in \"config get harness.audit\") exit 1 ;; esac\necho old\nexit 0",
            log.display()
        ),
    );
    fake(
        &bin,
        "codex",
        "case \"$*\" in \"--version\") echo 'codex-cli 0.162.1' ;; esac\nexit 0",
    );
    let mut child = Command::new(script())
        .args(["--yes", "--pr", "rzorzal/clusia#123"])
        .env_clear()
        .env("HOME", dir.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("SP2_CHECK_WAIT", "2")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A yes to the settings question must not matter: the script refuses before it asks.
    child.stdin.take().unwrap().write_all(b"y\ny\ny\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(said.contains("harness.audit"), "{said}");
    assert!(said.contains("Nothing was changed"), "{said}");
    assert!(!said.contains("Change those seven settings"), "{said}");
    let asked = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(!asked.contains("config set"), "settings changed: {asked}");
}

#[test]
fn every_settings_write_and_the_one_process_proof_are_checked() {
    let source = source();
    assert!(source.contains("set_setting"), "writes are not checked");
    assert!(
        source.contains("the same codex app-server"),
        "the one-process proof is missing"
    );
}
