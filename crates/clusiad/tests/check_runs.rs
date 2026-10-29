//! An open keeps GitHub's check runs, by name, beside the summary: the Tests tab shows them.

mod common;

use clusia_core::{CheckRun, ChecksSummary};
use common::github_mock::{PrMock, mount_pr};
use common::review_world::{open, pr7, world};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn mount_runs(server: &MockServer, head: &str, runs: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/acme/widgets/commits/{head}/check-runs"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "total_count": runs.as_array().map_or(0, Vec::len),
            "check_runs": runs
        })))
        .with_priority(1)
        .mount(server)
        .await;
}

fn build(conclusion: &str) -> serde_json::Value {
    json!({
        "name": "build",
        "status": "completed",
        "conclusion": conclusion,
        "started_at": "2026-10-01T10:00:00Z",
        "completed_at": "2026-10-01T10:05:00Z",
        "details_url": "https://github.com/acme/widgets/actions/runs/1"
    })
}

fn expected_build(conclusion: &str) -> CheckRun {
    CheckRun {
        name: "build".into(),
        status: "completed".into(),
        conclusion: Some(conclusion.into()),
        started_at: Some("2026-10-01T10:00:00Z".into()),
        completed_at: Some("2026-10-01T10:05:00Z".into()),
        details_url: Some("https://github.com/acme/widgets/actions/runs/1".into()),
    }
}

#[tokio::test]
async fn a_full_open_stores_the_runs_and_the_summary_from_one_read() {
    let w = world().await;
    mount_runs(
        &w.server,
        &w.head,
        json!([build("success"), { "name": "e2e", "status": "in_progress", "conclusion": null }]),
    )
    .await;
    let mut c = w.daemon.client().await;
    let view = open(&mut c).await;

    let cache = clusia_store::load_review_cache(&w.daemon.paths, &pr7())
        .unwrap()
        .expect("a successful open writes the cache");
    let names: Vec<&str> = cache.check_runs.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["build", "e2e"]);
    assert_eq!(cache.check_runs[0], expected_build("success"));
    let summary = ChecksSummary {
        total: 2,
        passed: 1,
        failed: 0,
        pending: 1,
    };
    assert_eq!(cache.checks, Some(summary));
    assert_eq!(view.checks, Some(summary));
    let requests = w.server.received_requests().await.unwrap();
    let reads = requests
        .iter()
        .filter(|r| r.url.path().ends_with("/check-runs"))
        .count();
    assert_eq!(reads, 1, "the runs and the summary come from one request");
    w.daemon.stop().await;
}

#[tokio::test]
async fn an_unchanged_head_reopen_refreshes_the_runs() {
    let w = world().await;
    mount_runs(&w.server, &w.head, json!([build("failure")])).await;
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let first = clusia_store::load_review_cache(&w.daemon.paths, &pr7())
        .unwrap()
        .unwrap();
    assert_eq!(first.check_runs, vec![expected_build("failure")]);

    // Only the fast path can open now: git is gone, GitHub answers again, and the build passes.
    std::fs::remove_dir_all(&w.origin).unwrap();
    w.server.reset().await;
    mount_pr(&w.server, &PrMock::new(&w.head, &w.base, &w.origin)).await;
    mount_runs(&w.server, &w.head, json!([build("success")])).await;
    let view = open(&mut c).await;

    let again = clusia_store::load_review_cache(&w.daemon.paths, &pr7())
        .unwrap()
        .unwrap();
    assert_eq!(again.check_runs, vec![expected_build("success")]);
    assert_eq!(
        view.checks,
        Some(ChecksSummary {
            total: 1,
            passed: 1,
            failed: 0,
            pending: 0
        })
    );
    w.daemon.stop().await;
}

#[tokio::test]
async fn a_failed_check_read_still_opens_with_no_runs() {
    let w = world().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/acme/widgets/commits/{}/check-runs",
            w.head
        )))
        .respond_with(ResponseTemplate::new(500))
        .with_priority(1)
        .mount(&w.server)
        .await;
    let mut c = w.daemon.client().await;
    let view = open(&mut c).await;
    assert_eq!(view.checks, None);
    let cache = clusia_store::load_review_cache(&w.daemon.paths, &pr7())
        .unwrap()
        .unwrap();
    assert!(cache.check_runs.is_empty());
    assert_eq!(cache.checks, None);
    w.daemon.stop().await;
}
