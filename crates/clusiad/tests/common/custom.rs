//! A fake custom program for the tests that run the daemon on Custom command.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clusia_harness::testkit::{CustomCall, CustomScript, FakeCustom};

use super::agent::set_config;
use super::review_world::World;

/// Installs a fake custom program, makes it the daemon's harness under the name "Echo" and keeps
/// the agent from speaking first. Returns the folder the fake records its processes in.
pub async fn use_fake_custom(w: &World, script: CustomScript) -> PathBuf {
    let dir = w.tmp.path().join("fake-custom");
    std::fs::create_dir_all(&dir).unwrap();
    let program = FakeCustom::install(&dir, script);
    set_config(w, "harness.kind", "custom").await;
    set_config(w, "harness.program", &program.display().to_string()).await;
    set_config(w, "harness.custom_name", "Echo").await;
    set_config(w, "harness.on_open", "wait").await;
    dir
}

/// Waits for `count` runs of the fake without blocking the runtime the daemon runs on.
pub async fn custom_calls(dir: &Path, count: usize) -> Vec<CustomCall> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        FakeCustom::wait_for_calls(&dir, count, Duration::from_secs(5))
    })
    .await
    .unwrap()
}

/// Waits for `count` answers to the fake's permission requests.
pub async fn custom_decisions(dir: &Path, count: usize) -> Vec<(String, String)> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        FakeCustom::wait_for_decisions(&dir, count, Duration::from_secs(5))
    })
    .await
    .unwrap()
}

/// Whether the process group `pid` leads still has a member.
pub fn group_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks whether the group exists; nothing is sent.
    unsafe { libc::killpg(pid as i32, 0) == 0 }
}

/// Waits until run `index` of the fake ignores SIGTERM: `hang()` records the pid of the child it
/// starts only after its trap is set, so a stop sent before that would still kill it.
pub async fn custom_hung(dir: &Path, index: usize) -> CustomCall {
    for _ in 0..100 {
        let dir = dir.to_path_buf();
        let calls = tokio::task::spawn_blocking(move || FakeCustom::calls(&dir))
            .await
            .unwrap();
        if let Some(call) = calls.into_iter().nth(index)
            && call.child.is_some()
        {
            return call;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("run {index} of the fake never started ignoring SIGTERM");
}

/// Whether the whole group is gone within `seconds`.
pub async fn group_gone(pid: u32, seconds: u64) -> bool {
    for _ in 0..seconds * 10 {
        if !group_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}
