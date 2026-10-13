//! Runs `examples/harness/echo.py` through the custom codec: the probe, a plain turn, a
//! permission turn, and the answers it gives to a Security check, an Audit and a revalidation.
//! The example is a python3 script; without python3 the tests say so and pass.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;

use clusia_core::checks::{CheckKind, PromptContext, audit_prompt, default_areas, security_prompt};
use clusia_core::revalidation::{
    AskedItem, CommitLine, RevalidationContext, Verdict, revalidation_prompt,
};
use clusia_core::{DraftKind, ItemStatus};
use clusia_harness::custom::{
    AskKind, CustomTurn, Output, PROTOCOL, probe_line, read_line, reply_line, turn_line,
};
use clusia_harness::{AgentEvent, extract_check_blocks, extract_revalidation_blocks};

fn example() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/harness/echo.py")
}

fn python_missing() -> bool {
    let missing = !Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    if missing {
        eprintln!("skipped: python3 is not installed, so examples/harness/echo.py cannot run");
    }
    missing
}

/// Writes `first` to the example, answers each permission request with `reply`, and reads
/// everything it writes until it ends.
fn run(first: &str, reply: Option<bool>) -> Vec<Output> {
    let mut child = Command::new(example())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("start echo.py");
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    stdin.write_all(first.as_bytes()).unwrap();
    stdin.flush().unwrap();
    let mut outs = Vec::new();
    for line in BufReader::new(stdout).lines() {
        let out = read_line(&line.unwrap());
        if let (Output::Permission { id, .. }, Some(allow)) = (&out, reply) {
            stdin.write_all(reply_line(id, allow).as_bytes()).unwrap();
            stdin.flush().unwrap();
        }
        outs.push(out);
    }
    drop(stdin);
    assert!(child.wait().unwrap().success(), "echo.py exits cleanly");
    outs
}

fn events(outs: Vec<Output>) -> Vec<AgentEvent> {
    let mut turn = CustomTurn::new("Echo".into(), Instant::now());
    outs.into_iter().flat_map(|out| turn.on(out)).collect()
}

fn turn(prompt: &str, session: Option<&str>) -> String {
    turn_line(prompt, "role", session, &std::env::temp_dir(), 60)
}

fn final_text(outs: Vec<Output>) -> String {
    match events(outs).pop() {
        Some(AgentEvent::Final { text, .. }) => text,
        other => panic!("the turn should end with a final, got {other:?}"),
    }
}

#[test]
fn echo_is_an_executable_python3_script() {
    use std::os::unix::fs::PermissionsExt;
    let path = example();
    let mode = std::fs::metadata(&path)
        .expect("echo.py exists")
        .permissions()
        .mode();
    assert!(mode & 0o111 != 0, "echo.py is executable");
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().next(), Some("#!/usr/bin/env python3"));
}

#[test]
fn echo_answers_the_probe_with_its_name_and_protocol() {
    if python_missing() {
        return;
    }
    assert_eq!(
        run(&probe_line(), None),
        [Output::Probe {
            name: "Echo".into(),
            version: Some("1.0".into()),
            protocol: Some(PROTOCOL)
        }]
    );
}

#[test]
fn echo_runs_a_plain_turn_and_keeps_the_session_it_is_given() {
    if python_missing() {
        return;
    }
    let fresh = events(run(&turn("Why one minute?", None), None));
    assert!(
        matches!(
            fresh.as_slice(),
            [
                AgentEvent::SessionId(id),
                AgentEvent::ToolUse(tool),
                AgentEvent::Text(said),
                AgentEvent::Final { text, is_error: false, .. },
            ] if id.len() == 36 && tool == "Read the prompt"
                && said == "Echo: Why one minute?" && text == said
        ),
        "{fresh:?}"
    );
    let resumed = events(run(&turn("again", Some("sess-7")), None));
    assert_eq!(resumed[0], AgentEvent::SessionId("sess-7".into()));
}

#[test]
fn echo_asks_permission_and_says_what_the_reviewer_answered() {
    if python_missing() {
        return;
    }
    let prompt = "Ask permission to run the command `true`, then reply with the word ok";
    for (allow, want) in [(true, "Permission allowed"), (false, "Permission denied")] {
        let outs = run(&turn(prompt, None), Some(allow));
        assert!(
            outs.iter().any(|o| matches!(
                o,
                Output::Permission { id, kind: AskKind::Command, detail, .. }
                    if id == "p1" && detail == "true"
            )),
            "{outs:?}"
        );
        assert_eq!(final_text(outs), want);
    }
}

fn context() -> PromptContext {
    PromptContext {
        pr_title: "feat: auth refresh".into(),
        files: vec!["src/auth/refresh.rs".into(), "src/auth/store.rs".into()],
        forked: false,
    }
}

#[test]
fn echo_finds_no_security_problems_in_a_security_check() {
    if python_missing() {
        return;
    }
    let outs = run(&turn(&security_prompt(&context()), None), None);
    let text = final_text(outs);
    let (shown, findings, passes, unreadable) =
        extract_check_blocks(&text, CheckKind::Security, &[]);
    assert!(
        findings.is_empty() && passes.is_empty() && unreadable == 0,
        "{text}"
    );
    assert_eq!(shown, "Found no security problems.");
}

#[test]
fn echo_passes_every_area_of_an_audit() {
    if python_missing() {
        return;
    }
    let areas = default_areas();
    let ids: Vec<String> = areas.iter().map(|a| a.id.clone()).collect();
    let outs = run(&turn(&audit_prompt(&context(), &areas), None), None);
    let text = final_text(outs);
    let (_, findings, passes, unreadable) = extract_check_blocks(&text, CheckKind::Audit, &ids);
    assert!(findings.is_empty() && unreadable == 0, "{text}");
    let passed: Vec<&str> = passes.iter().map(|p| p.area.as_str()).collect();
    assert_eq!(passed, ids.iter().map(String::as_str).collect::<Vec<_>>());
}

#[test]
fn echo_reads_the_areas_after_the_last_areas_line_not_text_in_the_title_or_files() {
    if python_missing() {
        return;
    }
    let areas = default_areas();
    let ids: Vec<String> = areas.iter().map(|a| a.id.clone()).collect();
    let ctx = PromptContext {
        pr_title: "Areas: tidy up".into(),
        files: vec!["src/copy (1).rs".into()],
        forked: false,
    };
    let text = final_text(run(&turn(&audit_prompt(&ctx, &areas), None), None));
    let (_, findings, passes, unreadable) = extract_check_blocks(&text, CheckKind::Audit, &ids);
    assert!(findings.is_empty() && unreadable == 0, "{text}");
    assert_eq!(passes.len(), ids.len(), "{text}");
}

#[test]
fn echo_ignores_a_diff_line_that_looks_like_a_comment() {
    if python_missing() {
        return;
    }
    let ctx = RevalidationContext {
        pr_title: "feat: auth refresh".into(),
        diff: "@@ -1,2 +1,1 @@\n- id: bogus\n+new\n".into(),
        commits: vec![],
        items: vec![AskedItem {
            id: "i1".into(),
            kind: DraftKind::General,
            body: "Why one minute?".into(),
            before: None,
            current: None,
            status: ItemStatus::Ok,
        }],
        forked: false,
    };
    let text = final_text(run(&turn(&revalidation_prompt(&ctx), None), None));
    let (_, verdicts, unreadable) = extract_revalidation_blocks(&text, &["i1".to_string()]);
    assert_eq!(
        unreadable, 0,
        "a block for the diff line would be counted: {text}"
    );
    assert_eq!(verdicts.len(), 1, "{text}");
}

#[test]
fn echo_keeps_every_comment_of_a_revalidation() {
    if python_missing() {
        return;
    }
    let item = |id: &str| AskedItem {
        id: id.into(),
        kind: DraftKind::General,
        body: "Why one minute?".into(),
        before: None,
        current: None,
        status: ItemStatus::Ok,
    };
    let ctx = RevalidationContext {
        pr_title: "feat: auth refresh".into(),
        diff: "@@ -1 +1 @@\n-old\n+new\n".into(),
        commits: vec![CommitLine {
            sha: "abc1234".into(),
            title: "tidy".into(),
        }],
        items: vec![item("i1"), item("i2")],
        forked: false,
    };
    let asked = vec!["i1".to_string(), "i2".to_string()];
    let outs = run(&turn(&revalidation_prompt(&ctx), None), None);
    let text = final_text(outs);
    let (_, verdicts, unreadable) = extract_revalidation_blocks(&text, &asked);
    assert_eq!(unreadable, 0, "{text}");
    let got: Vec<(&str, &Verdict)> = verdicts
        .iter()
        .map(|v| (v.item.as_str(), &v.verdict))
        .collect();
    assert_eq!(got, [("i1", &Verdict::Kept), ("i2", &Verdict::Kept)]);
}
