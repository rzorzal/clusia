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
