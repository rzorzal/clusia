//! The security-and-audits owner-check script parses, lists its steps, asks before it acts,
//! answers nothing for the owner and puts back what it changed.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sp2-m3-check.sh")
}

fn source() -> String {
    std::fs::read_to_string(script()).unwrap()
}

#[test]
fn the_script_is_executable_bash_that_parses() {
    let mode = std::fs::metadata(script()).unwrap().permissions().mode();
    assert!(
        mode & 0o111 != 0,
        "scripts/sp2-m3-check.sh is not executable"
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
        "open:",
        "accept:",
        "dismiss:",
        "custom:",
        "off:",
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
fn the_script_never_widens_what_the_agent_may_do_or_removes_files() {
    let source = source();
    for forbidden in [
        "sudo",
        "rm -r",
        "dangerously-skip-permissions",
        "bypassPermissions",
        "review publish",
        "--allowedTools",
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
    // The agent's questions run detached from the keyboard, so a request for a command waits
    // for the window; the script never feeds an answer in.
    assert!(source.contains("</dev/null"));
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
        r#"config set harness.audit_areas "$OLD_AREAS""#,
    ] {
        assert!(source.contains(restored), "{restored} is not put back");
    }
    assert!(source.contains("Skipped: the review was kept"));
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
        said.contains("twice"),
        "it says it discards the review twice: {said}"
    );
    assert!(said.contains("answer every request for a"), "{said}");
    assert!(said.contains("Nothing was run"), "{said}");
}

#[test]
fn the_cli_step_checks_the_exit_code_and_the_finding_line() {
    let source = source();
    let step = source
        .split("step_cli() {")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("the cli step");
    assert!(step.contains("clusia check \"$PR\" --security"), "{step}");
    assert!(step.contains("-eq 0"), "{step}");
    assert!(step.contains("(HIGH|MEDIUM|LOW)"), "{step}");
    assert!(step.contains("found no security issues"), "{step}");
    assert!(step.contains("-eq 2"), "a usage error exits 2: {step}");
}

#[test]
fn the_custom_area_is_named_after_the_run_so_it_can_be_found_and_removed() {
    let source = source();
    assert!(source.contains("clusia-check-$RUN"));
    assert!(source.contains("remove clusia-check-$RUN if it is still there"));
}
