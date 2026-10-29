//! The tests owner-check script parses, lists its steps, keeps to the owner's own `rzorzal/*`
//! pull requests, asks before it spends anything, answers nothing for the owner,
//! installs, starts, removes and publishes nothing, and puts back the runner it changes. No test
//! runs a real agent or engine through the script: the `clusia` it finds is a fake.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sp4-m1a-check.sh")
}

fn source() -> String {
    std::fs::read_to_string(script()).unwrap()
}

/// A `clusia` that writes every command it gets to `log`.
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
        "scripts/sp4-m1a-check.sh is not executable"
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
        "preflight:",
        "generate:",
        "unapproved:",
        "setup:",
        "list:",
        "near:",
        "one:",
        "stop:",
        "local:",
        "cli:",
        "end:",
    ] {
        assert!(steps.lines().any(|l| l.starts_with(step)), "{step} missing");
    }
    assert_eq!(calls, None, "listing ran clusia");
}

#[test]
fn bad_options_are_refused() {
    for args in [
        &[][..],
        &["--nope"][..],
        &["rzorzal/clusia"][..],
        &["--public"][..],
        &["rzorzal/clusia#x"][..],
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
        &["--public", "octo/hello-world#1"][..],
    ] {
        let (out, calls) = run(args, b"y\ny\n");
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("rzorzal"), "{err}");
        assert_eq!(calls, None, "{args:?}");
    }
}

#[test]
fn a_no_before_a_step_stops_the_check_there() {
    // Yes to the check, no before the recipe is generated.
    let (out, calls) = run(&["rzorzal/clusia#123"], b"y\nn\n");
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(
        said.contains("Stopped before: your agent writes the recipe"),
        "{said}"
    );
    let calls = calls.expect("the read-only preflight ran");
    let tests: Vec<&str> = calls.lines().filter(|l| l.starts_with("tests ")).collect();
    assert_eq!(
        tests,
        ["tests rzorzal/clusia#123"],
        "only the bare, read-only tests ran"
    );
}

#[test]
fn the_run_asks_before_it_spends_anything_and_stops_on_no() {
    let (out, calls) = run(&["rzorzal/clusia#123"], b"n\n");
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(said.contains("YOUR agent"), "{said}");
    assert!(
        said.contains("runs the pull request's tests on this Mac"),
        "{said}"
    );
    assert!(said.contains("installs nothing"), "{said}");
    assert!(said.contains("publishes nothing"), "{said}");
    assert_eq!(calls, None);
}

#[test]
fn the_script_never_approves_installs_starts_removes_or_publishes() {
    let source = source();
    for forbidden in [
        "--yes",
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
    // The setup asks the owner at the terminal; the script never feeds it an answer.
    assert!(source.contains(r#"clusia tests "$PR" --setup </dev/tty"#));
}

#[test]
fn the_runner_comes_back_and_the_stop_is_checked() {
    let source = source();
    assert!(source.contains(r#"config set tests.runner "$OLD_RUNNER""#));
    assert!(source.contains("trap 'stop_run; restore_settings; drop_err' EXIT"));
    assert!(source.contains("trap 'exit 130' INT TERM HUP"));
    assert!(source.contains(r#"kill -INT "$RUN_PID""#));
    assert!(source.contains("label=io.clusia.review=$KEY"));
    assert!(source.contains("Skipped: the review was kept"));
    assert!(source.contains(r#"DATA="${CLUSIA_HOME:-$HOME/Library/Application Support/Clusia}""#));
    for before in [
        "\ngo_on \"your agent writes",
        "\ngo_on \"run the tests near",
        "go_on \"run $FIRST_TEST",
        "\ngo_on \"run all the tests",
        "\ngo_on \"set tests.runner",
    ] {
        assert!(source.contains(before), "no question before {before:?}");
    }
}
