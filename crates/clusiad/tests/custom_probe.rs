mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use clusia_harness::testkit::{CustomScript, FakeCustom};
use clusia_protocol::{Client, Command, Event, ProbeResult, Reply, topics};
use common::agent::{set_config, until};
use common::custom::{group_gone, use_fake_custom};
use common::review_world::{World, world, world_with_harness_env};

async fn probe(w: &World) -> ProbeResult {
    match w
        .daemon
        .client()
        .await
        .request(Command::HarnessProbe)
        .await
        .unwrap()
    {
        Reply::Probe(found) => found,
        other => panic!("expected a probe, got {other:?}"),
    }
}

async fn config_value(w: &World, key: &str) -> String {
    match w
        .daemon
        .client()
        .await
        .request(Command::GetConfigValue { key: key.into() })
        .await
        .unwrap()
    {
        Reply::Value(value) => value,
        other => panic!("expected a value, got {other:?}"),
    }
}

async fn config_watcher(w: &World) -> Client {
    let mut c = w.daemon.client().await;
    c.request(Command::Subscribe {
        topics: vec![topics::CONFIG.into()],
    })
    .await
    .unwrap();
    c
}

#[tokio::test]
async fn the_probe_names_the_program_and_fills_the_empty_name() {
    let w = world().await;
    let dir = use_fake_custom(&w, CustomScript::new().probe("Ollama", "0.3", 1)).await;
    set_config(&w, "harness.custom_name", "").await;
    let mut watcher = config_watcher(&w).await;

    let found = probe(&w).await;
    assert!(found.ok, "{found:?}");
    assert_eq!(found.harness, "Ollama");
    assert_eq!(found.version.as_deref(), Some("0.3"));
    assert!(found.error.is_none());
    assert_eq!(found.signed_in, None);
    assert_eq!(found.account, None);
    assert_eq!(found.program, config_value(&w, "harness.program").await);

    assert_eq!(config_value(&w, "harness.custom_name").await, "Ollama");
    until(&mut watcher, |e| {
        matches!(e, Event::ConfigChanged { key, value }
            if key == "harness.custom_name" && value == "Ollama")
    })
    .await;
    let calls = FakeCustom::calls(&dir);
    assert_eq!(calls.len(), 1, "the probe is one process and no turn");
    assert!(calls[0].is_probe());
    assert_eq!(calls[0].first_line().unwrap()["protocol"], 1);
    assert!(
        group_gone(calls[0].pid, 15).await,
        "the probe leaves no process"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_name_that_is_set_is_kept_and_a_long_one_is_cut() {
    let w = world().await;
    use_fake_custom(&w, CustomScript::new().probe("Ollama", "0.3", 1)).await;
    let found = probe(&w).await;
    assert!(found.ok, "{found:?}");
    assert_eq!(
        config_value(&w, "harness.custom_name").await,
        "Echo",
        "a name the user gave is never replaced"
    );

    set_config(&w, "harness.custom_name", "").await;
    let long = format!("Bell\u{7}{}", "x".repeat(60));
    use_fake_custom(&w, CustomScript::new().probe(&long, "1", 1)).await;
    set_config(&w, "harness.custom_name", "").await;
    let found = probe(&w).await;
    assert!(found.ok, "{found:?}");
    let stored = config_value(&w, "harness.custom_name").await;
    assert_eq!(stored.chars().count(), 40);
    assert!(stored.starts_with("Bellxxx"), "{stored}");
    assert!(!stored.chars().any(char::is_control));
    assert_eq!(found.harness, stored);
    w.daemon.stop().await;
}

#[tokio::test]
async fn the_probe_says_what_is_wrong_with_the_program() {
    let w = world().await;
    set_config(&w, "harness.kind", "custom").await;

    let found = probe(&w).await;
    assert!(!found.ok);
    assert_eq!(
        found.error.as_deref(),
        Some("Set the custom command's program in Config › Harness")
    );
    assert_eq!(found.harness, "Custom command");

    set_config(&w, "harness.program", "/nonexistent/echo").await;
    let found = probe(&w).await;
    assert!(!found.ok);
    assert_eq!(
        found.error.as_deref(),
        Some("/nonexistent/echo was not found or cannot run")
    );

    for relative in ["echo-harness", "bin/echo.py"] {
        set_config(&w, "harness.program", relative).await;
        let found = probe(&w).await;
        assert!(!found.ok);
        assert_eq!(
            found.error,
            Some(format!(
                "{relative} is not an absolute path. Set it in Config › Harness"
            )),
            "no PATH lookup, and nothing out of the checkout"
        );
        assert_eq!(found.program, "");
    }

    let plain = w.tmp.path().join("not-executable");
    std::fs::write(&plain, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
    set_config(&w, "harness.program", &plain.display().to_string()).await;
    let found = probe(&w).await;
    assert!(!found.ok);
    assert_eq!(
        found.error,
        Some(format!("{} was not found or cannot run", plain.display()))
    );
    assert_eq!(
        found.harness, "not-executable",
        "the file name is the fallback name"
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_program_that_does_not_answer_the_probe_can_still_be_used() {
    let w = world().await;
    let dir = use_fake_custom(&w, CustomScript::new().silent_probe()).await;
    let found = probe(&w).await;
    assert!(!found.ok);
    assert_eq!(
        found.error.as_deref(),
        Some("Echo did not answer the probe in 5 s; it can still run turns")
    );
    assert!(found.elapsed_ms >= 5000, "{found:?}");
    assert_eq!(found.harness, "Echo");
    assert_eq!(config_value(&w, "harness.custom_name").await, "Echo");
    let pid = FakeCustom::wait_for_calls(&dir, 1, Duration::from_secs(5))[0].pid;
    assert!(group_gone(pid, 15).await, "the silent program is ended");
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_program_with_another_protocol_is_refused_with_both_numbers() {
    let w = world().await;
    use_fake_custom(&w, CustomScript::new().probe("Ollama", "0.3", 2)).await;
    let found = probe(&w).await;
    assert!(!found.ok);
    assert_eq!(
        found.error.as_deref(),
        Some("Ollama speaks protocol 2; Clúsia knows protocol 1")
    );
    assert_eq!(found.version.as_deref(), Some("0.3"));
    w.daemon.stop().await;
}

#[tokio::test]
async fn the_probe_of_claude_code_and_codex_keeps_its_own_way() {
    let w = world().await;
    set_config(&w, "harness.kind", "claude-code").await;
    set_config(&w, "harness.program", "/nonexistent/claude").await;
    let found = probe(&w).await;
    assert!(!found.ok);
    assert_eq!(found.harness, "Claude Code");
    assert_eq!(
        found.error.as_deref(),
        Some("Install Claude Code or set its path")
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn the_probe_runs_without_tokens_and_with_the_configured_arguments() {
    let w = world_with_harness_env(&[
        ("GH_TOKEN", "x"),
        ("GITHUB_TOKEN", "x"),
        ("GH_ENTERPRISE_TOKEN", "x"),
        ("CLUSIA_FOO", "y"),
        ("KEEP", "z"),
    ])
    .await;
    let dir = use_fake_custom(&w, CustomScript::new().probe("Ollama", "0.3", 1)).await;
    set_config(&w, "harness.extra_args", "--model small").await;
    let found = probe(&w).await;
    assert!(found.ok, "{found:?}");
    let call = &FakeCustom::calls(&dir)[0];
    for key in [
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "GH_ENTERPRISE_TOKEN",
        "CLUSIA_FOO",
    ] {
        assert_eq!(call.env_value(key), None, "{key} reached the probe");
    }
    assert_eq!(call.env_value("KEEP"), Some("z"));
    assert_eq!(call.argv, ["--model", "small"]);
    w.daemon.stop().await;
}
