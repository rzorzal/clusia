//! The Custom command owner-check script parses, lists its steps, asks before it acts, refuses a
//! machine where `python3` does not run without changing anything, answers nothing for the owner,
//! pushes and publishes nothing itself, runs no program but the example's wrapper, and puts back
//! every setting it changed. No test runs a real agent or `echo.py` through the script: the
//! programs the script finds are fakes.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sp2-m6-check.sh")
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
        "scripts/sp2-m6-check.sh is not executable"
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
fn the_script_never_pushes_publishes_or_runs_a_real_agent() {
    let source = source();
    for forbidden in [
        "sudo",
        "rm -r",
        "git push",
        "review publish",
        "dangerously",
        "--yolo",
        "--full-auto",
        "CODEX_HOME",
    ] {
        assert!(!source.contains(forbidden), "{forbidden} in the script");
    }
    for line in source.lines() {
        let line = line.trim_start();
        for program in ["curl ", "rm ", "claude ", "codex ", "ollama "] {
            assert!(
                !line.starts_with(program),
                "the script runs {line:?} itself"
            );
        }
    }
    assert!(
        source.contains("examples/harness/echo.py"),
        "the only program it runs is the example"
    );
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
        r#"config set harness.custom_name "$OLD_NAME""#,
        r#"config set harness.revalidate "$OLD_REVALIDATE""#,
        r#"config set harness.revalidate_quiet_secs "$OLD_QUIET""#,
        r#"config set harness.on_open "$OLD_ON_OPEN""#,
        r#"config set harness.check_security "$OLD_SECURITY""#,
        r#"config set harness.audit "$OLD_AUDIT""#,
    ] {
        assert!(source.contains(restored), "{restored} is not put back");
    }
    assert!(source.contains("put back when it ends"));
    let at = |needle: &str| source.find(needle).unwrap();
    let kind = at(r#"config set harness.kind "$OLD_KIND""#);
    let program = at(r#"config set harness.program "$OLD_PROGRAM""#);
    let name = at(r#"config set harness.custom_name "$OLD_NAME""#);
    assert!(
        kind < program && program < name,
        "the program and the name are restored after the kind: a change of kind clears them"
    );
}

#[test]
fn the_isolation_permission_and_process_checks_exist() {
    let source = source();
    for needle in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "CLUSIA_",
        "Permission denied",
        "Permission allowed",
        "a second message in the same session",
        "clusia harness test",
        "no echo.py process is left running",
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
    assert!(said.contains("echo.py"), "{said}");
    assert!(said.contains("answer every request for a"), "{said}");
    assert!(said.contains("you push"), "{said}");
    assert!(said.contains("you discard"), "{said}");
    assert!(said.contains("Nothing was run"), "{said}");
}

#[test]
fn it_refuses_a_machine_where_python_does_not_run_and_changes_nothing() {
    {
        let python = "echo 'killed' >&2; exit 137";
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
        fake(&bin, "python3", python);
        let out = Command::new(script())
            .args(["--yes", "--pr", "rzorzal/clusia#123"])
            .env_clear()
            .env("HOME", dir.path())
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout).to_string();
        assert_eq!(out.status.code(), Some(1), "{said}");
        assert!(said.contains("python3 --version"), "{said}");
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
        "python3",
        "case \"$*\" in \"--version\") echo 'Python 3.12.0' ;; esac\nexit 0",
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
    child.stdin.take().unwrap().write_all(b"n\n").unwrap();
    let out = child.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(said.contains("Nothing was changed"), "{said}");
    let asked = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(!asked.contains("config set"), "settings changed: {asked}");
}
