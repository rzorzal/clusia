#![allow(dead_code)]

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// What the mocked GitHub says about acme/widgets#7.
pub struct PrMock {
    pub head: String,
    pub base: String,
    pub author: String,
    pub viewer: String,
    pub clone_url: String,
    /// The `/files` response (GitHub shape).
    pub files: Value,
    /// The `/commits` response (GitHub shape).
    pub commits: Value,
}

impl PrMock {
    pub fn new(head: &str, base: &str, clone_url: &str) -> Self {
        Self {
            head: head.into(),
            base: base.into(),
            author: "maria".into(),
            viewer: "me".into(),
            clone_url: clone_url.into(),
            files: json!([{ "filename": "feature.txt", "status": "added", "additions": 1, "deletions": 0, "patch": "@@ -0,0 +1,3 @@\n+one\n+two\n+three" }]),
            commits: json!([]),
        }
    }
}

pub async fn mount_pr(server: &MockServer, pr: &PrMock) {
    let base = "/repos/acme/widgets";
    let ok = |body: Value| ResponseTemplate::new(200).set_body_json(body);
    Mock::given(method("GET")).and(path(format!("{base}/pulls/7"))).respond_with(ok(json!({
        "number": 7, "title": "Add feature", "html_url": "https://github.com/acme/widgets/pull/7",
        "user": { "login": pr.author }, "draft": false, "updated_at": "2026-10-01T12:00:00Z",
        "comments": 0, "review_comments": 0, "additions": 3, "deletions": 0, "changed_files": 1,
        "base": { "ref": "main", "sha": pr.base, "repo": { "clone_url": pr.clone_url } },
        "head": { "ref": "feature", "sha": pr.head, "repo": null }
    }))).mount(server).await;
    Mock::given(method("GET"))
        .and(path(format!("{base}/pulls/7/files")))
        .respond_with(ok(pr.files.clone()))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{base}/pulls/7/comments")))
        .respond_with(ok(json!([])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{base}/issues/7/comments")))
        .respond_with(ok(json!([])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{base}/pulls/7/reviews")))
        .respond_with(ok(json!([])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{base}/pulls/7/commits")))
        .respond_with(ok(pr.commits.clone()))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{base}/commits/{}/check-runs", pr.head)))
        .respond_with(ok(json!({ "total_count": 0, "check_runs": [] })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ok(json!({ "login": pr.viewer })))
        .mount(server)
        .await;
}
