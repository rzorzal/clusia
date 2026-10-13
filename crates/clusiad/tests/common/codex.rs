#![allow(dead_code)]

//! A fake `codex` for the tests that run the daemon on Codex.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clusia_harness::testkit::{CodexCall, CodexScript, FakeCodex};

use super::agent::set_config;
use super::review_world::World;

/// Installs a fake `codex`, makes it the daemon's harness and keeps the agent from speaking
/// first. Returns the folder the fake records its processes in.
pub async fn use_fake_codex(w: &World, script: CodexScript) -> PathBuf {
    let dir = w.tmp.path().join("fake-codex");
    std::fs::create_dir_all(&dir).unwrap();
    let program = FakeCodex::install(&dir, script);
    set_config(w, "harness.kind", "codex").await;
    set_config(w, "harness.program", &program.display().to_string()).await;
    set_config(w, "harness.on_open", "wait").await;
    dir
}

/// Waits for `count` processes of the fake without blocking the runtime the daemon runs on.
pub async fn codex_calls(dir: &Path, count: usize) -> Vec<CodexCall> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        FakeCodex::wait_for_calls(&dir, count, Duration::from_secs(5))
    })
    .await
    .unwrap()
}

/// Whether the process left within a few seconds.
pub async fn codex_gone(pid: u32) -> bool {
    for _ in 0..80 {
        if !FakeCodex::is_running(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Waits until process `index` of the fake was sent `count` requests called `method`, and gives
/// the process back.
pub async fn codex_after(dir: &Path, index: usize, method: &str, count: usize) -> CodexCall {
    for _ in 0..100 {
        let dir = dir.to_path_buf();
        let calls = tokio::task::spawn_blocking(move || FakeCodex::calls(&dir))
            .await
            .unwrap();
        if let Some(call) = calls.into_iter().nth(index)
            && call.requests(method).len() >= count
        {
            return call;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("process {index} never got {count} x {method}");
}
