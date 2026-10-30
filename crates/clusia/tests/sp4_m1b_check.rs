//! The Tests-tab owner-check script parses, lists its steps, keeps to the owner's own `rzorzal/*`
//! pull requests, asks before it spends anything, clicks, generates, approves, runs, stops and
//! cleans nothing for the owner, installs, starts, removes and publishes nothing, and changes no
//! setting. No test runs a real agent or engine through the script: the `clusia` it finds is a
//! fake that records what it was asked.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const DEFAULT_PR: &str = "rzorzal/blog#37";

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sp4-m1b-check.sh")
}

fn source() -> String {
    std::fs::read_to_string(script()).unwrap()
}

/// A `clusia` and a `docker` that write every command they get to `log`.
fn recording_clusia(dir: &Path, log: &Path) {
    let path = dir.join("clusia");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\necho \"$*\" >> {}\necho old\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The script asks `docker ps` whether a container is left; this one answers nothing and
    // logs the question, so no test reaches a real engine through the `PATH`.
    let docker = dir.join("docker");
    std::fs::write(
        &docker,
        format!(
            "#!/bin/sh\necho \"docker $*\" >> {}\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Runs the script with `args`, `stdin` typed in, and a recording `clusia` first on the `PATH`;
/// gives back what it printed and the commands `clusia` got.
fn run(args: &[&str], stdin: &[u8]) -> (Output, Option<String>) {
    let home = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let log = bin.path().join("calls.log");
    recording_clusia(bin.path(), &log);
    let mut child = Command::new(script())
        .args(args)
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    (out, std::fs::read_to_string(&log).ok())
}

#[test]
fn the_script_is_executable_bash_that_parses() {
    let mode = std::fs::metadata(script()).unwrap().permissions().mode();
    assert!(
        mode & 0o111 != 0,
        "scripts/sp4-m1b-check.sh is not executable"
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
    let (out, calls) = run(&["--list"], b"");
    assert!(out.status.success());
    let steps = String::from_utf8(out.stdout).unwrap();
    for step in [
        "open:",
        "generate:",
        "approve:",
        "near:",
        "all:",
        "one:",
        "stop:",
        "comment:",
        "config:",
        "end:",
    ] {
        assert!(steps.lines().any(|l| l.starts_with(step)), "{step} missing");
    }
    assert_eq!(steps.lines().count(), 10);
    assert_eq!(calls, None, "listing ran clusia");
}

#[test]
fn bad_options_are_refused() {
    for args in [
        &["--nope"][..],
        &["rzorzal/blog"][..],
        &["--public"][..],
        &["rzorzal/blog#x"][..],
        &["rzorzal/blog#1", "rzorzal/blog#2"][..],
    ] {
        let (out, calls) = run(args, b"");
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(!out.stderr.is_empty(), "{args:?}");
        assert_eq!(calls, None, "{args:?}");
    }
}

#[test]
fn only_rzorzal_pull_requests_are_accepted() {
    for args in [
        &["acme/widgets#7"][..],
        &["octo/hello-world#1"][..],
        &["rzorzalx/blog#37"][..],
    ] {
        let (out, calls) = run(args, b"y\ny\n");
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("rzorzal"), "{err}");
        assert_eq!(calls, None, "{args:?}");
    }
}

#[test]
fn with_no_argument_it_checks_the_default_pull_request_after_asking() {
    let (out, calls) = run(&[], b"n\n");
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(said.contains(DEFAULT_PR), "{said}");
    assert!(said.contains("YOUR agent"), "{said}");
    assert!(said.contains("You do every click yourself"), "{said}");
    assert!(said.contains("approves nothing"), "{said}");
    assert!(said.contains("publishes nothing"), "{said}");
    assert_eq!(
        calls, None,
        "it asked nothing of clusia before the first yes"
    );
}

#[test]
fn a_no_before_a_step_stops_the_check_there() {
    // Yes to the check, yes to what the opening tab shows, no before the recipe is generated.
    let (out, calls) = run(&[DEFAULT_PR], b"y\ny\nn\n");
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(
        said.contains("Stopped before: click Generate tests"),
        "{said}"
    );
    let calls = calls.expect("the read-only opening ran");
    let tests: Vec<&str> = calls.lines().filter(|l| l.contains("tests")).collect();
    assert!(
        tests.iter().all(|l| !l.contains("--json")),
        "nothing was cross-checked before the generation: {tests:?}"
    );
}

#[test]
fn a_full_run_only_reads_opens_and_discards() {
    // Yes to everything, then no more input.
    let (out, calls) = run(&[DEFAULT_PR], "y\n".repeat(80).as_bytes());
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(said.contains("passed,"), "{said}");
    let calls = calls.expect("it ran");
    let allowed = [
        "daemon status".to_string(),
        format!("open {DEFAULT_PR}"),
        format!("tests {DEFAULT_PR}"),
        format!("--json tests {DEFAULT_PR}"),
        "config get tests.runner".to_string(),
        format!("review discard {DEFAULT_PR}"),
        "docker ps -aq --filter label=io.clusia.review=rzorzal~blog~37".to_string(),
    ];
    for call in calls.lines() {
        assert!(
            allowed.iter().any(|a| a == call),
            "the script ran clusia {call:?}"
        );
    }
    assert!(
        calls
            .lines()
            .any(|l| l == format!("review discard {DEFAULT_PR}")),
        "the discard at the end was asked and said yes to"
    );
    assert!(
        calls.lines().any(|l| l.starts_with("docker ")),
        "the fake docker was the one asked"
    );
}

#[test]
fn the_script_never_clicks_approves_installs_starts_removes_or_publishes() {
    let source = source();
    for forbidden in [
        "--generate",
        "--setup",
        "--run",
        "--yes",
        "--clean",
        "config set",
        "sudo",
        "rm -r",
        "git push",
        "review publish",
        "brew ",
        "colima start",
        "machine start",
        "open -a",
        "docker rm",
        "docker kill",
        "docker rmi",
        "docker volume rm",
        "docker system",
        "docker compose",
        "osascript",
        "cliclick",
        "yes |",
    ] {
        assert!(!source.contains(forbidden), "{forbidden} in the script");
    }
    for line in source.lines() {
        let line = line.trim_start();
        for program in ["curl ", "rm ", "git ", "claude ", "codex ", "sandbox-exec "] {
            assert!(
                !line.starts_with(program),
                "the script runs {line:?} itself"
            );
        }
    }
}

#[test]
fn it_reads_the_runner_twice_and_asks_before_it_spends_or_stops_anything() {
    let source = source();
    assert!(source.contains(r#"OLD_RUNNER="$(clusia config get tests.runner"#));
    assert!(source.contains(r#""$(clusia config get tests.runner"#));
    assert!(source.contains("trap 'exit 130' INT TERM HUP"));
    assert!(source.contains("label=io.clusia.review=$KEY"));
    assert!(source.contains("Skipped: the review was kept"));
    assert!(source.contains(r#"DATA="${CLUSIA_HOME:-$HOME/Library/Application Support/Clusia}""#));
    assert!(source.contains(r#"[ "$OWNER" != rzorzal ]"#));
    for before in [
        "\ngo_on \"click Generate tests",
        "\ngo_on \"click Run tests near",
        "\ngo_on \"click Run all tests",
        "\ngo_on \"click ▶",
        "\ngo_on \"start a run and click Stop",
    ] {
        assert!(source.contains(before), "no question before {before:?}");
    }
}
