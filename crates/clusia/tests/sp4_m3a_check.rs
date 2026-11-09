//! The system owner-check script parses, lists its steps, keeps to the owner's own `rzorzal/*`
//! pull requests, asks before it spends anything, and approves, installs, starts an engine,
//! removes a Docker object or publishes nothing. No test runs a real agent or engine through the
//! script: the `clusia` and `docker` it finds are fakes that record what they were asked.
//! The README section that documents the feature is checked here too.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const DEFAULT_PR: &str = "rzorzal/blog#37";

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/sp4-m3a-check.sh")
}

fn source() -> String {
    std::fs::read_to_string(script()).unwrap()
}

/// A `clusia` and a `docker` that write every command they get to `log`. The fake `docker`
/// answers nothing, so no test reaches a real engine through the `PATH`.
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
/// gives back what it printed and the commands `clusia` and `docker` got.
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
        "scripts/sp4-m3a-check.sh is not executable"
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
        "plan:",
        "start:",
        "loopback:",
        "curl:",
        "send:",
        "logs:",
        "network:",
        "edit-guard:",
        "stop:",
        "end:",
    ] {
        assert!(steps.lines().any(|l| l.starts_with(step)), "{step} missing");
    }
    assert_eq!(steps.lines().count(), 12);
    assert_eq!(calls, None, "listing ran clusia");
}

#[test]
fn bad_options_are_refused() {
    for args in [
        &["--nope"][..],
        &["rzorzal/blog"][..],
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
    assert!(said.contains("approves nothing"), "{said}");
    assert!(said.contains("publishes nothing"), "{said}");
    assert_eq!(
        calls, None,
        "it asked nothing of clusia before the first yes"
    );

    // With a yes, the first thing it asks of clusia is to open the default review, and every
    // later command that names a pull request names that one.
    let (_, calls) = run(&[], b"y\nn\n");
    let calls = calls.expect("the opening ran");
    assert_eq!(
        calls.lines().next(),
        Some(format!("open {DEFAULT_PR}").as_str())
    );
    for call in calls.lines().filter(|l| l.contains('#')) {
        assert!(call.contains(DEFAULT_PR), "{call}");
    }
}

#[test]
fn a_no_before_a_step_stops_the_check_there() {
    // Yes to the check, yes to what the opening shows, no before the recipe is generated.
    let (out, calls) = run(&[DEFAULT_PR], b"y\ny\nn\n");
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(
        said.contains(&format!(
            "Stopped before: run clusia tests {DEFAULT_PR} --generate"
        )),
        "{said}"
    );
    let calls = calls.expect("the read-only opening ran");
    assert!(
        calls.lines().all(|l| !l.contains("--generate")),
        "nothing was generated: {calls}"
    );
}

#[test]
fn the_script_never_approves_installs_starts_an_engine_removes_or_publishes() {
    let text = source();
    for banned in [
        "--yes",
        "config set",
        "daemon start",
        "daemon stop",
        "brew ",
        "colima",
        "docker rm",
        "docker stop",
        "docker kill",
        "docker network rm",
        "review publish",
        "gh ",
        "eval ",
        "bash -c",
        "$(clusia system status",
        "sudo",
        "git push",
    ] {
        assert!(!text.contains(banned), "{banned}");
    }
    // The discard is asked, once, and only after a yes.
    assert_eq!(text.matches("review discard").count(), 1);
    let lines: Vec<&str> = text.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.contains("review discard"))
        .unwrap();
    assert!(
        lines[at - 1].contains("yes_or_no \"Discard the review"),
        "{}",
        lines[at - 1]
    );
}

#[test]
fn it_asks_before_it_spends_and_reads_docker_only_to_look() {
    let text = source();
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let spends = [
            "clusia tests \"$PR\" --generate",
            "clusia system start",
            "clusia system send",
            "clusia system network",
            "clusia system logs",
            "clusia system stop",
        ]
        .iter()
        .any(|p| line.starts_with(p));
        if spends {
            assert!(
                lines[i.saturating_sub(3)..i]
                    .iter()
                    .any(|l| l.starts_with("go_on ")),
                "no question before {line:?}"
            );
        }
    }
    // The second start of the last step is asked about too.
    assert!(text.contains("go_on \"run clusia system start $PR again\""));
    // The only Docker words are the ones that look.
    for part in text.split("docker ").skip(1) {
        let word: String = part
            .chars()
            .take_while(|c| c.is_ascii_lowercase())
            .collect();
        match word.as_str() {
            "" | "ps" | "port" | "inspect" => {}
            "network" => assert!(part.starts_with("network ls"), "{part:.30}"),
            other => panic!("docker {other} in the script"),
        }
    }
    assert!(text.contains("trap 'true' INT"));
    assert!(text.contains("trap 'exit 130' INT TERM HUP"));
    assert!(text.contains("restart_refused() {"));
    assert!(text.contains(r#"[ "$OWNER" != rzorzal ]"#));
}

#[test]
fn a_full_run_only_reads_asks_and_discards() {
    // Yes to everything, then no more input.
    let (out, calls) = run(&[DEFAULT_PR], "y\n".repeat(80).as_bytes());
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(said.contains("passed,"), "{said}");
    let calls = calls.expect("it ran");
    let allowed = [
        format!("open {DEFAULT_PR}"),
        format!("system status {DEFAULT_PR}"),
        format!("--json system status {DEFAULT_PR}"),
        format!("tests {DEFAULT_PR} --generate"),
        format!("system endpoints {DEFAULT_PR}"),
        format!("system start {DEFAULT_PR}"),
        format!("system send {DEFAULT_PR} 1"),
        format!("system logs {DEFAULT_PR} --follow"),
        format!("system network {DEFAULT_PR} on"),
        format!("system network {DEFAULT_PR} off"),
        format!("system restart {DEFAULT_PR}"),
        format!("system stop {DEFAULT_PR}"),
        format!("review discard {DEFAULT_PR}"),
    ];
    for call in calls.lines() {
        let docker = call.starts_with("docker ps ")
            || call.starts_with("docker port ")
            || call.starts_with("docker network ls ")
            || call.starts_with("docker inspect ");
        assert!(
            docker || allowed.iter().any(|a| a == call),
            "the script ran {call:?}"
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
fn the_readme_documents_start_the_system() {
    let readme =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../README.md"))
            .unwrap();
    let section = readme
        .split("### Start the system")
        .nth(1)
        .expect("the section exists");
    let section = section.split("\n## ").next().unwrap();
    for needle in [
        "clusia system start",
        "127.0.0.1",
        "forwarder",
        "socat",
        "GPL-2.0",
        "tests.systems_max",
        "Where to test",
        "Send",
        "curl",
        "--data-raw",
        "10 MiB",
        "2000 lines",
        "Ctrl-C",
        "clusia system stop",
        "Docker or Colima",
        "#326",
        "Never",
    ] {
        assert!(section.contains(needle), "{needle}");
    }
}
