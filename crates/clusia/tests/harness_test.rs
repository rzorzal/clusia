//! `clusia harness test` against fake programs and the example: what it checks, what it prints,
//! its exit codes, and that nothing of the user's environment reaches the program. No test runs
//! a real model or a third-party program.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use clusia_harness::testkit::{CustomScript, CustomTurnScript, FakeCustom};

fn turn() -> CustomTurnScript {
    CustomTurnScript::new()
}

/// Runs `clusia --home <home> <args>` with the environment a developer's shell has: GitHub tokens
/// that the program must never see, a variable it may see, and the overrides that keep every
/// real agent out.
fn clusia(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_clusia"))
        .arg("--home")
        .arg(home)
        .args(args)
        .env("CLUSIA_CLAUDE_BIN", "/nonexistent/claude")
        .env("CLUSIA_CODEX_BIN", "/nonexistent/codex")
        .env("CLUSIA_SECRET_STORE", "memory")
        .env("GH_TOKEN", "ghp_secret_one")
        .env("GITHUB_TOKEN", "ghp_secret_two")
        .env("GH_ENTERPRISE_TOKEN", "ghp_secret_three")
        .env("KEEP_ME", "1")
        .output()
        .unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn passed(text: &str, label: &str) -> bool {
    text.lines()
        .any(|l| l.starts_with("ok") && l.trim_end().ends_with(label))
}

fn failed(text: &str, label: &str) -> bool {
    text.lines()
        .any(|l| l.starts_with("FAIL") && l.trim_end().ends_with(label))
}

/// A folder with a fake installed in it, and the fake's path.
fn fake(script: CustomScript) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let program = FakeCustom::install(dir.path(), script);
    (dir, program)
}

fn conforming() -> CustomScript {
    CustomScript::new()
        .probe("Fake", "1.0", 1)
        .turn(turn().session("s-1").text("ok").final_("ok"))
}

#[test]
fn a_conforming_program_passes_every_check() {
    let home = tempfile::tempdir().unwrap();
    let (dir, program) = fake(conforming());
    let o = clusia(home.path(), &["harness", "test", program.to_str().unwrap()]);
    let text = out(&o);
    assert_eq!(o.status.code(), Some(0), "{text}\n{}", err(&o));
    assert_eq!(
        text.lines().next(),
        Some("This runs one turn of Fake's model.")
    );
    for label in [
        "answers the probe",
        "speaks protocol 1",
        "every line is JSON",
        "returns a session id",
        "ends with final",
    ] {
        assert!(passed(&text, label), "{label}: {text}");
    }
    assert!(!text.contains("FAIL"), "{text}");

    let calls = FakeCustom::calls(dir.path());
    assert_eq!(calls.len(), 2, "one probe and one turn");
    assert!(calls[0].is_probe());
    let turn_line = calls[1].turn_line().expect("a turn line");
    assert_eq!(turn_line["prompt"], "Reply with the word ok");
    assert!(turn_line["session_id"].is_null());
    assert_eq!(turn_line["protocol"], 1);
    for call in &calls {
        assert!(!FakeCustom::is_running(call.pid), "no process is left");
    }
}

#[test]
fn harness_test_env_has_no_tokens() {
    let home = tempfile::tempdir().unwrap();
    let (dir, program) = fake(conforming());
    let o = clusia(home.path(), &["harness", "test", program.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    let calls = FakeCustom::calls(dir.path());
    assert_eq!(calls.len(), 2);
    for call in &calls {
        for token in ["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN"] {
            assert_eq!(call.env_value(token), None, "{token} reached the program");
        }
        assert!(
            !call.env.iter().any(|(key, _)| key.starts_with("CLUSIA_")),
            "a CLUSIA_ variable reached the program: {:?}",
            call.env
        );
        assert_eq!(call.env_value("KEEP_ME"), Some("1"), "the rest is kept");
    }
}

#[test]
fn the_program_runs_in_a_fresh_folder_that_is_removed_after() {
    let home = tempfile::tempdir().unwrap();
    let (dir, program) = fake(conforming());
    let o = clusia(home.path(), &["harness", "test", program.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(0));
    for call in FakeCustom::calls(dir.path()) {
        let cwd = Path::new(&call.cwd);
        assert!(!cwd.exists(), "{} was left behind", call.cwd);
        assert_ne!(
            cwd.canonicalize().ok(),
            home.path().canonicalize().ok(),
            "not the user's home"
        );
    }
}

#[test]
fn the_permission_round_trip_asks_and_is_answered_deny() {
    let home = tempfile::tempdir().unwrap();
    let (dir, program) = fake(
        CustomScript::new().probe("Fake", "1.0", 1).turn(
            turn()
                .session("s-1")
                .ask_command("p1", "true")
                .text("denied")
                .final_("ok"),
        ),
    );
    let o = clusia(
        home.path(),
        &["harness", "test", program.to_str().unwrap(), "--permission"],
    );
    let text = out(&o);
    assert_eq!(o.status.code(), Some(0), "{text}\n{}", err(&o));
    assert!(
        passed(&text, "asks permission and reads the reply"),
        "{text}"
    );
    assert_eq!(
        FakeCustom::decisions(dir.path()),
        [("p1".to_string(), "deny".to_string())]
    );
    let calls = FakeCustom::calls(dir.path());
    assert!(
        calls[1].turn_line().unwrap()["prompt"]
            .as_str()
            .unwrap()
            .contains("Ask permission to run the command `true`")
    );
}

#[test]
fn a_program_that_never_asks_fails_the_permission_check_only() {
    let home = tempfile::tempdir().unwrap();
    let (_dir, program) = fake(conforming());
    let o = clusia(
        home.path(),
        &["harness", "test", program.to_str().unwrap(), "--permission"],
    );
    let text = out(&o);
    assert_eq!(o.status.code(), Some(1), "{text}");
    assert!(
        failed(&text, "asks permission and reads the reply"),
        "{text}"
    );
    assert!(passed(&text, "ends with final"), "{text}");
}

#[test]
fn each_failure_is_named_and_exits_1() {
    let cases: Vec<(&str, CustomScript, &str)> = vec![
        (
            "another protocol",
            CustomScript::new()
                .probe("Fake", "1.0", 2)
                .turn(turn().session("s").final_("ok")),
            "speaks protocol 1",
        ),
        (
            "text that is not json",
            CustomScript::new()
                .probe("Fake", "1.0", 1)
                .turn(turn().session("s").raw("hello there").final_("ok")),
            "every line is JSON",
        ),
        (
            "no session",
            CustomScript::new()
                .probe("Fake", "1.0", 1)
                .turn(turn().text("ok").final_("ok")),
            "returns a session id",
        ),
        (
            "a crash",
            CustomScript::new()
                .probe("Fake", "1.0", 1)
                .turn(turn().session("s").crash(3)),
            "ends with final",
        ),
        (
            "an error line",
            CustomScript::new()
                .probe("Fake", "1.0", 1)
                .turn(turn().session("s").error("auth", "please log in")),
            "ends with final",
        ),
        (
            "an oversized line",
            CustomScript::new()
                .probe("Fake", "1.0", 1)
                .turn(turn().session("s").oversized_line().hang()),
            "ends with final",
        ),
    ];
    for (what, script, label) in cases {
        let home = tempfile::tempdir().unwrap();
        let (dir, program) = fake(script);
        let o = clusia(home.path(), &["harness", "test", program.to_str().unwrap()]);
        let text = out(&o);
        assert_eq!(o.status.code(), Some(1), "{what}: {text}\n{}", err(&o));
        assert!(failed(&text, label), "{what}: {text}");
        for call in FakeCustom::calls(dir.path()) {
            assert!(
                !FakeCustom::is_running(call.pid),
                "{what}: a process is left"
            );
        }
    }
}

#[test]
fn an_error_line_shows_what_the_program_said() {
    let home = tempfile::tempdir().unwrap();
    let (_dir, program) = fake(
        CustomScript::new()
            .probe("Fake", "1.0", 1)
            .turn(turn().session("s").error("auth", "please log in")),
    );
    let o = clusia(home.path(), &["harness", "test", program.to_str().unwrap()]);
    assert!(out(&o).contains("please log in"), "{}", out(&o));
}

#[test]
fn a_silent_probe_fails_in_five_seconds_and_the_turn_still_runs() {
    let home = tempfile::tempdir().unwrap();
    let (dir, program) = fake(
        CustomScript::new()
            .silent_probe()
            .turn(turn().session("s-1").final_("ok")),
    );
    let started = std::time::Instant::now();
    let o = clusia(home.path(), &["harness", "test", program.to_str().unwrap()]);
    let text = out(&o);
    assert_eq!(o.status.code(), Some(1), "{text}");
    assert!(failed(&text, "answers the probe"), "{text}");
    assert!(started.elapsed() >= std::time::Duration::from_secs(5));
    assert!(passed(&text, "ends with final"), "{text}");
    assert!(
        text.starts_with("This runs one turn of "),
        "the program's file name stands in for a name it never gave: {text}"
    );
    assert_eq!(FakeCustom::calls(dir.path()).len(), 2);
}

#[test]
fn a_program_that_cannot_start_fails_every_check() {
    let home = tempfile::tempdir().unwrap();
    let o = clusia(home.path(), &["harness", "test", "/nonexistent/harness"]);
    let text = out(&o);
    assert_eq!(o.status.code(), Some(1), "{text}\n{}", err(&o));
    for label in [
        "answers the probe",
        "speaks protocol 1",
        "every line is JSON",
        "returns a session id",
        "ends with final",
    ] {
        assert!(failed(&text, label), "{label}: {text}");
    }
}

#[test]
fn without_a_program_it_says_what_to_pass_and_exits_2() {
    let home = tempfile::tempdir().unwrap();
    let o = clusia(home.path(), &["harness", "test"]);
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(
        err(&o).contains("Pass the program to test, or set harness.kind to custom"),
        "{}",
        err(&o)
    );

    std::fs::write(
        home.path().join("config.toml"),
        "[harness]\nkind = \"custom\"\n",
    )
    .unwrap();
    let o = clusia(home.path(), &["harness", "test"]);
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(
        err(&o).contains("Set harness.program, or pass the program to test"),
        "{}",
        err(&o)
    );
}

#[test]
fn a_bare_program_name_is_refused_with_exit_2() {
    let home = tempfile::tempdir().unwrap();
    let o = clusia(home.path(), &["harness", "test", "echo-harness"]);
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(
        err(&o).contains("echo-harness is not an absolute path. Pass the path of the program"),
        "{}",
        err(&o)
    );

    std::fs::write(
        home.path().join("config.toml"),
        "[harness]\nkind = \"custom\"\nprogram = \"echo-harness\"\n",
    )
    .unwrap();
    let o = clusia(home.path(), &["harness", "test"]);
    assert_eq!(o.status.code(), Some(2), "{}", err(&o));
    assert!(
        err(&o).contains("echo-harness is not an absolute path"),
        "{}",
        err(&o)
    );
}

#[test]
fn the_configured_extra_arguments_go_to_the_program_as_in_a_turn() {
    let home = tempfile::tempdir().unwrap();
    let (dir, program) = fake(conforming());
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[harness]\nkind = \"custom\"\nprogram = \"{}\"\nextra_args = \"--model small\"\n",
            program.display()
        ),
    )
    .unwrap();
    let o = clusia(home.path(), &["harness", "test"]);
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    for call in FakeCustom::calls(dir.path()) {
        assert_eq!(call.argv, ["--model", "small"], "probe and turn alike");
    }
}

#[test]
fn the_configured_custom_program_is_the_default() {
    let home = tempfile::tempdir().unwrap();
    let (_dir, program) = fake(conforming());
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "[harness]\nkind = \"custom\"\nprogram = \"{}\"\n",
            program.display()
        ),
    )
    .unwrap();
    let o = clusia(home.path(), &["harness", "test"]);
    assert_eq!(o.status.code(), Some(0), "{}\n{}", out(&o), err(&o));
    assert!(out(&o).starts_with("This runs one turn of Fake's model."));
}

#[test]
fn json_prints_the_report_and_keeps_the_exit_code() {
    let home = tempfile::tempdir().unwrap();
    let (_dir, program) = fake(conforming());
    let o = clusia(
        home.path(),
        &["--json", "harness", "test", program.to_str().unwrap()],
    );
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let report: serde_json::Value = serde_json::from_str(out(&o).trim()).unwrap();
    assert_eq!(report["ok"], true);
    assert_eq!(report["name"], "Fake");
    let checks = report["checks"].as_array().unwrap();
    assert_eq!(checks.len(), 5);
    assert!(checks.iter().all(|c| c["ok"] == true));
    assert_eq!(checks[0]["label"], "answers the probe");

    let (_dir, bad) = fake(CustomScript::new().probe("Fake", "1.0", 3));
    let o = clusia(
        home.path(),
        &["--json", "harness", "test", bad.to_str().unwrap()],
    );
    assert_eq!(o.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_str(out(&o).trim()).unwrap();
    assert_eq!(report["ok"], false);
}

#[test]
fn what_the_program_writes_to_stderr_is_shown_with_a_failed_check_only() {
    let home = tempfile::tempdir().unwrap();
    let (_dir, program) = fake(
        CustomScript::new().probe("Fake", "1.0", 1).turn(
            turn()
                .session("s")
                .stderr("\u{1b}[2Kmodel not loaded")
                .crash(3),
        ),
    );
    let o = clusia(home.path(), &["harness", "test", program.to_str().unwrap()]);
    let text = out(&o);
    assert_eq!(o.status.code(), Some(1), "{text}");
    assert!(text.contains("it wrote to stderr: "), "{text}");
    assert!(text.contains("model not loaded"), "{text}");
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(
        !err(&o).contains("model not loaded"),
        "the program's stderr never reaches the terminal raw: {}",
        err(&o)
    );
}

#[test]
fn the_turns_stderr_goes_with_the_first_failed_check_only() {
    let home = tempfile::tempdir().unwrap();
    let (_dir, program) = fake(
        CustomScript::new()
            .probe("Fake", "1.0", 1)
            .turn(turn().stderr("model not loaded").crash(3)),
    );
    let o = clusia(home.path(), &["harness", "test", program.to_str().unwrap()]);
    let text = out(&o);
    assert_eq!(o.status.code(), Some(1), "{text}");
    assert_eq!(text.matches("model not loaded").count(), 1, "{text}");
}

/// A program that answers the probe, then closes its input before it asks for permission, so the
/// `deny` reply cannot be written.
#[test]
fn a_reply_that_cannot_be_written_fails_the_permission_check() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let program = dir.path().join("deaf");
    std::fs::write(
        &program,
        r#"#!/bin/bash
read -r line
case "$line" in
  *'"type":"probe"'*)
    echo '{"type":"probe","name":"Deaf","version":"1","protocol":1}'
    exit 0;;
esac
exec 0<&-
echo '{"type":"session","id":"s"}'
echo '{"type":"permission","id":"p1","kind":"command","detail":"true"}'
sleep 1
echo '{"type":"final","text":"ok"}'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let o = clusia(
        home.path(),
        &["harness", "test", program.to_str().unwrap(), "--permission"],
    );
    let text = out(&o);
    assert_eq!(o.status.code(), Some(1), "{text}\n{}", err(&o));
    assert!(
        failed(&text, "asks permission and reads the reply"),
        "{text}"
    );
    assert!(passed(&text, "ends with final"), "{text}");
}

/// `examples/harness/echo.py` needs python3 only; a machine without it skips the test and says so.
fn echo() -> Option<PathBuf> {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("skipped: python3 is not installed");
        return None;
    }
    Some(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/harness/echo.py")
            .canonicalize()
            .expect("examples/harness/echo.py exists"),
    )
}

#[test]
fn the_example_passes_every_check_with_and_without_permission() {
    let Some(echo) = echo() else {
        return;
    };
    let home = tempfile::tempdir().unwrap();
    let o = clusia(home.path(), &["harness", "test", echo.to_str().unwrap()]);
    let text = out(&o);
    assert_eq!(o.status.code(), Some(0), "{text}\n{}", err(&o));
    assert!(
        text.starts_with("This runs one turn of Echo's model."),
        "{text}"
    );
    assert!(passed(&text, "returns a session id"), "{text}");

    let o = clusia(
        home.path(),
        &["harness", "test", echo.to_str().unwrap(), "--permission"],
    );
    let text = out(&o);
    assert_eq!(o.status.code(), Some(0), "{text}\n{}", err(&o));
    assert!(
        passed(&text, "asks permission and reads the reply"),
        "{text}"
    );
}
