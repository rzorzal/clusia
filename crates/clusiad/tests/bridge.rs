//! The real `clusiad permission-bridge` process, as `claude` would start it.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

fn bridge(socket: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_clusiad"));
    command
        .args(["permission-bridge", "--socket"])
        .arg(socket)
        .args(["--pr", "acme/widgets#7", "--turn", "1"])
        .env("CLUSIA_CLAUDE_BIN", "/nonexistent/claude")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn the_process_denies_without_a_daemon_and_exits_when_stdin_closes() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = bridge(&dir.path().join("none.sock")).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let say = |value: Value| format!("{value}\n");
    stdin
        .write_all(
            say(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-11-25"}}))
            .as_bytes(),
        )
        .await
        .unwrap();
    stdin
        .write_all(
            say(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                "name": "approve",
                "arguments": {"tool_name": "Bash", "input": {"command": "ls"}, "tool_use_id": "t"},
            }}))
            .as_bytes(),
        )
        .await
        .unwrap();
    let wait = Duration::from_secs(10);
    let init: Value = serde_json::from_str(
        &tokio::time::timeout(wait, lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "clusia");
    let answer: Value = serde_json::from_str(
        &tokio::time::timeout(wait, lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let text = answer["result"]["content"][0]["text"].as_str().unwrap();
    let decision: Value = serde_json::from_str(text).unwrap();
    assert_eq!(decision["behavior"], "deny");
    drop(stdin);
    let status = tokio::time::timeout(wait, child.wait())
        .await
        .expect("the bridge leaves when its input closes")
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn a_bad_flag_set_is_refused() {
    let output = Command::new(env!("CARGO_BIN_EXE_clusiad"))
        .args(["permission-bridge", "--turn", "x"])
        .env("CLUSIA_CLAUDE_BIN", "/nonexistent/claude")
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
}
