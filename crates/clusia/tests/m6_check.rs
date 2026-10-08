//! The owner-check script parses, lists its steps and touches nothing when only listing.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/m6-check.sh")
}

#[test]
fn the_script_is_executable_bash_that_parses() {
    let mode = std::fs::metadata(script()).unwrap().permissions().mode();
    assert!(mode & 0o111 != 0, "scripts/m6-check.sh is not executable");
    let status = Command::new("bash")
        .arg("-n")
        .arg(script())
        .status()
        .unwrap();
    assert!(status.success());
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
        "install:",
        "open:",
        "second-launch:",
        "terminal:",
        "crash:",
        "notification:",
        "reinstall:",
        "icons:",
        "hardening:",
        "login:",
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
fn an_unknown_option_is_refused_and_brew_needs_a_prefix() {
    for args in [&["--nope"][..], &["--brew"][..]] {
        let out = Command::new(script()).args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(!out.stderr.is_empty());
    }
}

#[test]
fn the_script_never_asks_for_admin_rights_or_deletes_files() {
    let source = std::fs::read_to_string(script()).unwrap();
    for forbidden in ["sudo", "rm -", "launchctl bootout", "launchctl unload"] {
        assert!(!source.contains(forbidden), "{forbidden} in the script");
    }
}

#[test]
fn the_default_run_asks_before_stopping_clusia() {
    for args in [&[][..], &["--skip-install"][..]] {
        let mut child = Command::new(script())
            .args(args)
            .env_clear()
            .env("HOME", tempfile::tempdir().unwrap().path())
            .env("PATH", "/usr/bin:/bin")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // Anything but "y" aborts before a single step runs.
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), b"n\n").unwrap();
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert_eq!(out.status.code(), Some(1), "{args:?}: {stdout}");
        assert!(stdout.contains("[y/N]"), "{args:?}: {stdout}");
        assert!(
            !stdout.contains("PASS") && !stdout.contains("FAIL"),
            "{stdout}"
        );
    }
}

#[test]
fn the_other_modes_do_not_ask() {
    let source = std::fs::read_to_string(script()).unwrap();
    assert!(source.contains("--yes"), "no --yes for non-interactive use");
    let out = Command::new(script())
        .arg("--list")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(!String::from_utf8(out.stdout).unwrap().contains("[y/N]"));
}

/// The body of the shell function `name` in the script.
fn function_body(name: &str) -> String {
    let text = std::fs::read_to_string(script()).unwrap();
    let start = text
        .find(&format!("\n{name}() {{\n"))
        .unwrap_or_else(|| panic!("{name} is missing"));
    let body = &text[start..];
    body[..body.find("\n}\n").unwrap()].to_string()
}

#[test]
fn the_crash_step_kills_a_daemon_the_app_started() {
    let body = function_body("step_crash");
    assert!(
        !body.contains("kickstart"),
        "a daemon launchctl started hides how the app starts it: {body}"
    );
    let opened = body.find("open -b").expect("the app is opened");
    let killed = body.find("kill -9").expect("the daemon is killed");
    assert!(opened < killed, "{body}");
}

#[test]
fn the_notification_step_reads_the_daemons_real_answers() {
    let body = function_body("step_notification");
    assert!(body.contains(r#""ok":"ack""#), "{body}");
    assert!(body.contains("tray is not running"), "{body}");
    assert!(
        !body.contains("delivered"),
        "the daemon never answers that: {body}"
    );
}
