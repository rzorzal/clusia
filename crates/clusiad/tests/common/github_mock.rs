#![allow(dead_code)]

use serde_json::{Value, json};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// What the mocked GitHub says about acme/widgets#7.
pub struct PrMock {
    pub head: String,
    pub base: String,
    /// The base branch name.
    pub base_ref: String,
    pub author: String,
    pub viewer: String,
    pub clone_url: String,
    /// The `/files` response (GitHub shape).
    pub files: Value,
    /// The `/commits` response (GitHub shape).
    pub commits: Value,
    /// GraphQL `reviewThreads.nodes` (GitHub shape).
    pub threads: Value,
    /// The viewer's pending review on GitHub: (node id, url).
    pub pending: Option<(String, String)>,
    /// The pull request description.
    pub body: String,
    pub title: String,
    pub draft: bool,
    pub closed: bool,
    pub merged: bool,
}

impl PrMock {
    pub fn new(head: &str, base: &str, clone_url: &str) -> Self {
        Self {
            head: head.into(),
            base: base.into(),
            base_ref: "main".into(),
            author: "maria".into(),
            viewer: "me".into(),
            clone_url: clone_url.into(),
            files: json!([{ "filename": "feature.txt", "status": "added", "additions": 1, "deletions": 0, "patch": "@@ -0,0 +1,3 @@\n+one\n+two\n+three" }]),
            commits: json!([]),
            threads: json!([]),
            pending: None,
            body: String::new(),
            title: "Add feature".into(),
            draft: false,
            closed: false,
            merged: false,
        }
    }
}

impl PrMock {
    /// GitHub's `/files` for a PR that adds `feature.txt` with `content`.
    pub fn adding_feature(mut self, content: &str) -> Self {
        let lines: Vec<String> = content.lines().map(|l| format!("+{l}")).collect();
        let patch = format!("@@ -0,0 +1,{} @@\n{}", lines.len(), lines.join("\n"));
        self.files = json!([{ "filename": "feature.txt", "status": "added", "additions": lines.len(), "deletions": 0, "patch": patch }]);
        self
    }

    pub fn titled(mut self, title: &str) -> Self {
        self.title = title.into();
        self
    }

    pub fn drafted(mut self) -> Self {
        self.draft = true;
        self
    }

    pub fn closed(mut self) -> Self {
        self.closed = true;
        self
    }

    pub fn merged(mut self) -> Self {
        self.closed = true;
        self.merged = true;
        self
    }

    /// The same pull request with a description.
    pub fn described(mut self, body: &str) -> Self {
        self.body = body.into();
        self
    }
}

pub async fn mount_pr(server: &MockServer, pr: &PrMock) {
    let base = "/repos/acme/widgets";
    let ok = |body: Value| ResponseTemplate::new(200).set_body_json(body);
    Mock::given(method("GET")).and(path(format!("{base}/pulls/7"))).respond_with(ok(json!({
        "number": 7, "title": pr.title, "body": pr.body,
        "state": if pr.closed { "closed" } else { "open" }, "merged": pr.merged, "html_url": "https://github.com/acme/widgets/pull/7",
        "user": { "login": pr.author }, "draft": pr.draft, "updated_at": "2026-10-01T12:00:00Z",
        "comments": 0, "review_comments": 0, "additions": 3, "deletions": 0, "changed_files": 1,
        "base": { "ref": pr.base_ref, "sha": pr.base, "repo": { "clone_url": pr.clone_url } },
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
    let pending: Vec<Value> = pr
        .pending
        .iter()
        .map(|(id, url)| json!({ "id": id, "url": url, "viewerDidAuthor": true }))
        .collect();
    graphql(
        server,
        "headRefOid",
        json!({ "repository": { "pullRequest": {
            "id": "PR_7", "headRefOid": pr.head, "reviews": { "nodes": pending }
        } } }),
    )
    .await;
    graphql(
        server,
        "reviewThreads",
        json!({ "repository": { "pullRequest": { "reviewThreads": {
            "pageInfo": { "hasNextPage": false, "endCursor": null },
            "nodes": pr.threads
        } } } }),
    )
    .await;
}

/// Answers GraphQL requests whose body contains `contains` with `data`.
pub async fn graphql(server: &MockServer, contains: &str, data: Value) {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains(contains))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": data })))
        .mount(server)
        .await;
}

/// A GraphQL refusal (HTTP 200 with `errors`) for requests whose body contains `contains`,
/// winning over the default answers.
pub async fn graphql_error(server: &MockServer, contains: &str, message: &str) {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains(contains))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "data": null, "errors": [{ "message": message }] })),
        )
        .with_priority(1)
        .mount(server)
        .await;
}

/// Successful answers to every review write. `addPullRequestReview(` (with the parenthesis)
/// does not match `addPullRequestReviewThreadReply`.
pub async fn mount_publish(server: &MockServer, review_url: &str) {
    graphql(
        server,
        "addPullRequestReview(",
        json!({ "addPullRequestReview": { "pullRequestReview": {
            "id": "PRR_1", "databaseId": 42, "url": review_url, "state": "PENDING"
        } } }),
    )
    .await;
    graphql(
        server,
        "addPullRequestReviewThreadReply",
        json!({ "addPullRequestReviewThreadReply": { "comment": { "id": "PRRC_1" } } }),
    )
    .await;
    graphql(
        server,
        "submitPullRequestReview",
        json!({ "submitPullRequestReview": { "pullRequestReview": {
            "id": "PRR_1", "databaseId": 42, "url": review_url, "state": "COMMENTED"
        } } }),
    )
    .await;
    graphql(
        server,
        "resolveReviewThread",
        json!({ "resolveReviewThread": { "thread": { "id": "PRRT_1", "isResolved": true } } }),
    )
    .await;
    graphql(
        server,
        "deletePullRequestReview",
        json!({ "deletePullRequestReview": { "pullRequestReview": { "id": "PRR_1" } } }),
    )
    .await;
}

/// The GraphQL request bodies the server received whose text contains `contains`.
pub async fn graphql_requests(server: &MockServer, contains: &str) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.url.path() == "/graphql")
        .filter(|r| String::from_utf8_lossy(&r.body).contains(contains))
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}
