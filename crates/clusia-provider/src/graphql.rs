//! GitHub GraphQL v4: review threads and the review writes REST cannot do (spec §5.1).
//!
//! A review is written as: `addPullRequestReview` (pending, with the line comments) →
//! `addPullRequestReviewThreadReply` per reply → `submitPullRequestReview` →
//! `resolveReviewThread` per resolve; `deletePullRequestReview` cleans up a failed attempt.

use clusia_core::{PrRef, ReviewComment, ReviewThread, Side, ThreadPost};
use reqwest::{StatusCode, header};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::github::{
    GitHub, MAX_PAGES, ProviderError, PublishedReview, decode, error_message, rate_limit_wait,
    unauthorized,
};

/// The GraphQL endpoint next to a REST base: `https://api.github.com` → `…/graphql`,
/// Enterprise `https://HOST/api/v3` → `https://HOST/api/graphql`, anything else → `<base>/graphql`.
pub fn graphql_url(api: &str) -> String {
    let api = api.trim_end_matches('/');
    match api.strip_suffix("/api/v3") {
        Some(host) => format!("{host}/api/graphql"),
        None => format!("{api}/graphql"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReview {
    /// GraphQL node id (`PRR_…`).
    pub id: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrNode {
    /// GraphQL node id of the pull request (`PR_…`).
    pub id: String,
    pub head_oid: String,
    /// The viewer's own pending review, if one was started (e.g. on github.com).
    pub pending: Option<PendingReview>,
}

const PR_NODE: &str = "query PrNode($owner: String!, $repo: String!, $number: Int!) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      id
      headRefOid
      reviews(states: [PENDING], first: 100) { nodes { id url viewerDidAuthor } }
    }
  }
}";

const REVIEW_THREADS: &str =
    "query ReviewThreads($owner: String!, $repo: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id isResolved isOutdated path line startLine diffSide viewerCanReply viewerCanResolve
          comments(first: 100) { nodes { databaseId author { login } body createdAt url } }
        }
      }
    }
  }
}";

const START_REVIEW: &str = "mutation StartReview($input: AddPullRequestReviewInput!) {
  addPullRequestReview(input: $input) { pullRequestReview { id databaseId url state } }
}";

const REPLY_IN_REVIEW: &str =
    "mutation ReplyInReview($input: AddPullRequestReviewThreadReplyInput!) {
  addPullRequestReviewThreadReply(input: $input) { comment { id } }
}";

const SUBMIT_REVIEW: &str = "mutation SubmitReview($input: SubmitPullRequestReviewInput!) {
  submitPullRequestReview(input: $input) { pullRequestReview { id databaseId url state } }
}";

const DELETE_PENDING_REVIEW: &str =
    "mutation DeletePendingReview($input: DeletePullRequestReviewInput!) {
  deletePullRequestReview(input: $input) { pullRequestReview { id } }
}";

const RESOLVE_THREAD: &str = "mutation ResolveThread($input: ResolveReviewThreadInput!) {
  resolveReviewThread(input: $input) { thread { id isResolved } }
}";

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPrNode {
    id: String,
    head_ref_oid: String,
    reviews: Nodes<RawPendingReview>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPendingReview {
    id: String,
    url: String,
    viewer_did_author: bool,
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<Option<T>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadPage {
    page_info: PageInfo,
    nodes: Vec<Option<RawThread>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThread {
    id: String,
    is_resolved: bool,
    is_outdated: bool,
    path: String,
    line: Option<u32>,
    start_line: Option<u32>,
    diff_side: String,
    viewer_can_reply: bool,
    viewer_can_resolve: bool,
    comments: Nodes<RawPost>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPost {
    database_id: Option<u64>,
    author: Option<Login>,
    body: String,
    created_at: String,
    url: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReview {
    id: String,
    database_id: Option<u64>,
    url: String,
}

/// `errors[]` of a GraphQL response as a provider error.
fn graphql_error(errors: &[Value]) -> ProviderError {
    let kind = |t: &str| {
        errors
            .iter()
            .any(|e| e.get("type").and_then(Value::as_str) == Some(t))
    };
    let messages: Vec<&str> = errors
        .iter()
        .filter_map(|e| e.get("message")?.as_str())
        .collect();
    let message = if messages.is_empty() {
        "unknown error".to_string()
    } else {
        messages.join("; ")
    };
    if kind("RATE_LIMITED") {
        ProviderError::RateLimited {
            retry_after_secs: 60,
        }
    } else if kind("NOT_FOUND") {
        ProviderError::NotFound(message)
    } else {
        ProviderError::GraphQl(message)
    }
}

/// `value[key]` as `T`; a missing or null object is a refusal, not a decode error.
fn object<T: serde::de::DeserializeOwned>(
    value: &Value,
    path: &[&str],
    what: &str,
) -> Result<T, ProviderError> {
    let mut at = value;
    for key in path {
        at = at.get(key).unwrap_or(&Value::Null);
        if at.is_null() {
            return Err(ProviderError::EmptyPayload(what.to_string()));
        }
    }
    decode(at.clone())
}

fn side(diff_side: &str) -> Side {
    if diff_side == "LEFT" {
        Side::Left
    } else {
        Side::Right
    }
}

fn thread_input(c: &ReviewComment) -> Value {
    let mut t = json!({ "path": c.path, "body": c.body, "line": c.line, "side": c.side });
    if let Some(start) = c.start_line {
        t["startLine"] = json!(start);
        t["startSide"] = json!(c.start_side.as_deref().unwrap_or(&c.side));
    }
    t
}

impl GitHub {
    /// POSTs one GraphQL operation and returns its `data`. HTTP 200 with `errors` is a failure.
    async fn graphql(&self, query: &str, variables: Value) -> Result<Value, ProviderError> {
        let response = self
            .http
            .post(&self.graphql)
            .bearer_auth(self.token.secret())
            .header(header::ACCEPT, "application/json")
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await
            .map_err(|e| ProviderError::Offline(e.to_string()))?;
        let status = response.status();
        let headers = response.headers().clone();
        let text = response
            .text()
            .await
            .map_err(|e| ProviderError::Offline(e.to_string()))?;
        if status == StatusCode::UNAUTHORIZED {
            return Err(unauthorized(&text));
        }
        if let Some(retry_after_secs) = rate_limit_wait(status, &headers) {
            return Err(ProviderError::RateLimited { retry_after_secs });
        }
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(ProviderError::Http {
                status: status.as_u16(),
                message: error_message(&parsed),
            });
        }
        if let Some(errors) = parsed.get("errors").and_then(Value::as_array)
            && !errors.is_empty()
        {
            return Err(graphql_error(errors));
        }
        match parsed.get("data") {
            Some(data) if !data.is_null() => Ok(data.clone()),
            _ => Err(ProviderError::Decode(
                "GraphQL response without data".into(),
            )),
        }
    }

    fn pr_variables(pr: &PrRef) -> Value {
        json!({ "owner": pr.owner, "repo": pr.repo, "number": pr.number })
    }

    /// The pull request's node id, head and the viewer's own pending review.
    pub async fn pr_node(&self, pr: &PrRef) -> Result<PrNode, ProviderError> {
        let data = self.graphql(PR_NODE, Self::pr_variables(pr)).await?;
        let node = data
            .get("repository")
            .and_then(|r| r.get("pullRequest"))
            .filter(|p| !p.is_null())
            .ok_or_else(|| ProviderError::NotFound(pr.to_string()))?;
        let raw: RawPrNode = decode(node.clone())?;
        let pending = raw
            .reviews
            .nodes
            .into_iter()
            .flatten()
            .find(|r| r.viewer_did_author)
            .map(|r| PendingReview {
                id: r.id,
                url: r.url,
            });
        Ok(PrNode {
            id: raw.id,
            head_oid: raw.head_ref_oid,
            pending,
        })
    }

    /// Every review thread of the pull request (100 per page, at most 30 pages).
    pub async fn review_threads(&self, pr: &PrRef) -> Result<Vec<ReviewThread>, ProviderError> {
        let mut threads = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut variables = Self::pr_variables(pr);
            variables["after"] = json!(after);
            let data = self.graphql(REVIEW_THREADS, variables).await?;
            let page = data
                .get("repository")
                .and_then(|r| r.get("pullRequest"))
                .filter(|p| !p.is_null())
                .and_then(|p| p.get("reviewThreads"))
                .ok_or_else(|| ProviderError::NotFound(pr.to_string()))?;
            let page: ThreadPage = decode(page.clone())?;
            threads.extend(page.nodes.into_iter().flatten().map(|t| {
                ReviewThread {
                    id: t.id,
                    is_resolved: t.is_resolved,
                    is_outdated: t.is_outdated,
                    path: t.path,
                    line: t.line,
                    start_line: t.start_line.filter(|s| Some(*s) != t.line),
                    side: side(&t.diff_side),
                    viewer_can_reply: t.viewer_can_reply,
                    viewer_can_resolve: t.viewer_can_resolve,
                    comments: t
                        .comments
                        .nodes
                        .into_iter()
                        .flatten()
                        .map(|c| ThreadPost {
                            database_id: c.database_id,
                            author: c
                                .author
                                .map(|a| a.login)
                                .unwrap_or_else(|| "ghost".to_string()),
                            body: c.body,
                            created_at: c.created_at,
                            url: c.url,
                        })
                        .collect(),
                }
            }));
            match page.page_info {
                PageInfo {
                    has_next_page: true,
                    end_cursor: Some(cursor),
                } => after = Some(cursor),
                _ => break,
            }
        }
        Ok(threads)
    }

    /// Starts a pending review on `commit` holding the line comments. Nothing is visible to
    /// others until [`GitHub::submit_review`].
    pub async fn start_review(
        &self,
        pr_node_id: &str,
        commit: &str,
        comments: &[ReviewComment],
    ) -> Result<PendingReview, ProviderError> {
        let threads: Vec<Value> = comments.iter().map(thread_input).collect();
        let input = json!({ "pullRequestId": pr_node_id, "commitOID": commit, "threads": threads });
        let data = self
            .graphql(START_REVIEW, json!({ "input": input }))
            .await?;
        let review: RawReview = object(
            &data,
            &["addPullRequestReview", "pullRequestReview"],
            "the pending review",
        )?;
        Ok(PendingReview {
            id: review.id,
            url: review.url,
        })
    }

    /// Adds a reply to an existing thread inside the pending review.
    pub async fn reply_in_review(
        &self,
        review_id: &str,
        thread_id: &str,
        body: &str,
    ) -> Result<(), ProviderError> {
        let input = json!({
            "pullRequestReviewId": review_id,
            "pullRequestReviewThreadId": thread_id,
            "body": body,
        });
        let data = self
            .graphql(REPLY_IN_REVIEW, json!({ "input": input }))
            .await?;
        object::<Value>(
            &data,
            &["addPullRequestReviewThreadReply", "comment"],
            "the reply",
        )
        .map(|_| ())
    }

    /// Submits the pending review with its verdict (`APPROVE`, `REQUEST_CHANGES`, `COMMENT`).
    pub async fn submit_review(
        &self,
        review_id: &str,
        event: &str,
        body: &str,
    ) -> Result<PublishedReview, ProviderError> {
        let input = json!({ "pullRequestReviewId": review_id, "event": event, "body": body });
        let data = self
            .graphql(SUBMIT_REVIEW, json!({ "input": input }))
            .await?;
        let review: RawReview = object(
            &data,
            &["submitPullRequestReview", "pullRequestReview"],
            "the submitted review",
        )?;
        Ok(PublishedReview {
            id: review.database_id.unwrap_or(0),
            url: review.url,
        })
    }

    /// Deletes a pending review (cleanup after a failed publish).
    pub async fn delete_pending_review(&self, review_id: &str) -> Result<(), ProviderError> {
        let input = json!({ "pullRequestReviewId": review_id });
        let data = self
            .graphql(DELETE_PENDING_REVIEW, json!({ "input": input }))
            .await?;
        object::<Value>(
            &data,
            &["deletePullRequestReview", "pullRequestReview"],
            "the deleted review",
        )
        .map(|_| ())
    }

    pub async fn resolve_thread(&self, thread_id: &str) -> Result<(), ProviderError> {
        let input = json!({ "threadId": thread_id });
        let data = self
            .graphql(RESOLVE_THREAD, json!({ "input": input }))
            .await?;
        object::<Value>(&data, &["resolveReviewThread", "thread"], "the thread").map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Token, TokenOrigin};
    use wiremock::matchers::{body_partial_json, body_string_contains, header as h, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn gh(server: &MockServer) -> GitHub {
        GitHub::new(&server.uri(), Token::new("tok123", TokenOrigin::Pat)).unwrap()
    }

    fn pr7() -> PrRef {
        "acme/widgets#7".parse().unwrap()
    }

    fn data(value: Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({ "data": value }))
    }

    async fn answer(server: &MockServer, contains: &str, response: ResponseTemplate) {
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains(contains))
            .respond_with(response)
            .mount(server)
            .await;
    }

    fn comment(path: &str, line: u32, start: Option<u32>, side: &str) -> ReviewComment {
        ReviewComment {
            path: path.into(),
            body: "nit".into(),
            line,
            side: side.into(),
            start_line: start,
            start_side: start.map(|_| side.to_string()),
        }
    }

    #[test]
    fn graphql_endpoint_next_to_the_rest_base() {
        assert_eq!(
            graphql_url("https://api.github.com"),
            "https://api.github.com/graphql"
        );
        assert_eq!(
            graphql_url("https://ghe.example/api/v3/"),
            "https://ghe.example/api/graphql"
        );
        assert_eq!(
            graphql_url("http://127.0.0.1:4000"),
            "http://127.0.0.1:4000/graphql"
        );
        let client = GitHub::new(
            "https://ghe.example/api/v3",
            Token::new("t", TokenOrigin::Pat),
        )
        .unwrap();
        assert_eq!(client.graphql, "https://ghe.example/api/graphql");
    }

    #[tokio::test]
    async fn pr_node_finds_the_viewers_pending_review() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(h("authorization", "Bearer tok123"))
            .and(body_string_contains("headRefOid"))
            .and(body_partial_json(json!({
                "variables": { "owner": "acme", "repo": "widgets", "number": 7 }
            })))
            .respond_with(data(json!({ "repository": { "pullRequest": {
                "id": "PR_7", "headRefOid": "h1",
                "reviews": { "nodes": [
                    { "id": "PRR_other", "url": "u-other", "viewerDidAuthor": false },
                    { "id": "PRR_mine", "url": "u-mine", "viewerDidAuthor": true }
                ] }
            } } })))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            gh(&server).pr_node(&pr7()).await.unwrap(),
            PrNode {
                id: "PR_7".into(),
                head_oid: "h1".into(),
                pending: Some(PendingReview {
                    id: "PRR_mine".into(),
                    url: "u-mine".into()
                })
            }
        );
    }

    #[tokio::test]
    async fn pr_node_without_a_pull_request_is_not_found() {
        let server = MockServer::start().await;
        answer(
            &server,
            "headRefOid",
            data(json!({ "repository": { "pullRequest": null } })),
        )
        .await;
        assert_eq!(
            gh(&server).pr_node(&pr7()).await,
            Err(ProviderError::NotFound("acme/widgets#7".into()))
        );
    }

    #[tokio::test]
    async fn review_threads_follow_pages_and_normalize() {
        let server = MockServer::start().await;
        let thread = |id: &str,
                      line: Option<u32>,
                      start: Option<u32>,
                      side: &str,
                      author: Value| {
            json!({
                "id": id, "isResolved": id == "PRRT_2", "isOutdated": line.is_none(), "path": "src/a.rs",
                "line": line, "startLine": start, "diffSide": side,
                "viewerCanReply": true, "viewerCanResolve": id != "PRRT_3",
                "comments": { "nodes": [{
                    "databaseId": 11, "author": author, "body": "why?",
                    "createdAt": "2026-10-01T10:00:00Z", "url": "https://github.com/acme/widgets/pull/7#discussion_r11"
                }] }
            })
        };
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("reviewThreads"))
            .and(body_partial_json(json!({ "variables": { "after": "c1" } })))
            .respond_with(data(
                json!({ "repository": { "pullRequest": { "reviewThreads": {
                "pageInfo": { "hasNextPage": false, "endCursor": "c2" },
                "nodes": [thread("PRRT_3", None, None, "RIGHT", Value::Null)]
            } } } }),
            ))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("reviewThreads"))
            .respond_with(data(
                json!({ "repository": { "pullRequest": { "reviewThreads": {
                "pageInfo": { "hasNextPage": true, "endCursor": "c1" },
                "nodes": [
                    thread("PRRT_1", Some(41), Some(41), "RIGHT", json!({ "login": "mona" })),
                    thread("PRRT_2", Some(9), Some(7), "LEFT", json!({ "login": "octo" }))
                ]
            } } } }),
            ))
            .mount(&server)
            .await;
        let threads = gh(&server).review_threads(&pr7()).await.unwrap();
        let ids: Vec<&str> = threads.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["PRRT_1", "PRRT_2", "PRRT_3"]);
        assert_eq!(
            (threads[0].line, threads[0].start_line, threads[0].side),
            (Some(41), None, Side::Right),
            "a single-line thread has no start line"
        );
        assert_eq!(
            (
                threads[1].start_line,
                threads[1].side,
                threads[1].is_resolved
            ),
            (Some(7), Side::Left, true)
        );
        assert_eq!(
            (
                threads[2].line,
                threads[2].is_outdated,
                threads[2].viewer_can_resolve
            ),
            (None, true, false)
        );
        assert_eq!(threads[0].comments[0].author, "mona");
        assert_eq!(threads[2].comments[0].author, "ghost");
        assert_eq!(threads[0].comments[0].database_id, Some(11));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn start_review_is_pending_with_the_line_comments() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("addPullRequestReview("))
            .and(body_partial_json(json!({ "variables": { "input": {
                "pullRequestId": "PR_7", "commitOID": "h1",
                "threads": [
                    { "path": "a.rs", "body": "nit", "line": 3, "side": "RIGHT" },
                    { "path": "b.rs", "body": "nit", "line": 9, "side": "LEFT", "startLine": 7, "startSide": "LEFT" }
                ]
            } } })))
            .respond_with(data(json!({ "addPullRequestReview": { "pullRequestReview": {
                "id": "PRR_1", "databaseId": 42, "url": "https://github.com/acme/widgets/pull/7#pullrequestreview-42", "state": "PENDING"
            } } })))
            .expect(1)
            .mount(&server)
            .await;
        let pending = gh(&server)
            .start_review(
                "PR_7",
                "h1",
                &[
                    comment("a.rs", 3, None, "RIGHT"),
                    comment("b.rs", 9, Some(7), "LEFT"),
                ],
            )
            .await
            .unwrap();
        assert_eq!(pending.id, "PRR_1");
        let requests = server.received_requests().await.unwrap();
        let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let input = &sent["variables"]["input"];
        assert!(input.get("event").is_none(), "no event keeps it pending");
        assert!(input["threads"][0].get("startLine").is_none());
    }

    #[tokio::test]
    async fn reply_submit_resolve_and_delete_send_their_inputs() {
        let server = MockServer::start().await;
        let url = "https://github.com/acme/widgets/pull/7#pullrequestreview-42";
        for (contains, input, response) in [
            (
                "addPullRequestReviewThreadReply",
                json!({ "pullRequestReviewId": "PRR_1", "pullRequestReviewThreadId": "PRRT_1", "body": "Agreed." }),
                json!({ "addPullRequestReviewThreadReply": { "comment": { "id": "PRRC_1" } } }),
            ),
            (
                "submitPullRequestReview",
                json!({ "pullRequestReviewId": "PRR_1", "event": "REQUEST_CHANGES", "body": "Please fix." }),
                json!({ "submitPullRequestReview": { "pullRequestReview": { "id": "PRR_1", "databaseId": 42, "url": url, "state": "CHANGES_REQUESTED" } } }),
            ),
            (
                "resolveReviewThread",
                json!({ "threadId": "PRRT_1" }),
                json!({ "resolveReviewThread": { "thread": { "id": "PRRT_1", "isResolved": true } } }),
            ),
            (
                "deletePullRequestReview",
                json!({ "pullRequestReviewId": "PRR_1" }),
                json!({ "deletePullRequestReview": { "pullRequestReview": { "id": "PRR_1" } } }),
            ),
        ] {
            Mock::given(method("POST"))
                .and(path("/graphql"))
                .and(body_string_contains(contains))
                .and(body_partial_json(
                    json!({ "variables": { "input": input } }),
                ))
                .respond_with(data(response))
                .expect(1)
                .mount(&server)
                .await;
        }
        let client = gh(&server);
        client
            .reply_in_review("PRR_1", "PRRT_1", "Agreed.")
            .await
            .unwrap();
        assert_eq!(
            client
                .submit_review("PRR_1", "REQUEST_CHANGES", "Please fix.")
                .await
                .unwrap(),
            PublishedReview {
                id: 42,
                url: url.into()
            }
        );
        client.resolve_thread("PRRT_1").await.unwrap();
        client.delete_pending_review("PRR_1").await.unwrap();
    }

    #[tokio::test]
    async fn errors_in_a_200_are_failures() {
        let server = MockServer::start().await;
        let errors = |errors: Value| {
            ResponseTemplate::new(200).set_body_json(json!({ "data": null, "errors": errors }))
        };
        answer(
            &server,
            "resolveReviewThread",
            errors(json!([{ "type": "NOT_FOUND", "message": "Could not resolve to a node with the global id of 'PRRT_x'" }])),
        )
        .await;
        answer(
            &server,
            "deletePullRequestReview",
            errors(json!([{ "type": "RATE_LIMITED", "message": "API rate limit exceeded" }])),
        )
        .await;
        answer(
            &server,
            "addPullRequestReviewThreadReply",
            ResponseTemplate::new(200).set_body_json(json!({
                "data": { "addPullRequestReviewThreadReply": null },
                "errors": [{ "message": "Thread is locked" }, { "message": "try later" }]
            })),
        )
        .await;
        let client = gh(&server);
        assert!(matches!(
            client.resolve_thread("PRRT_x").await,
            Err(ProviderError::NotFound(m)) if m.contains("PRRT_x")
        ));
        assert_eq!(
            client.delete_pending_review("PRR_1").await,
            Err(ProviderError::RateLimited {
                retry_after_secs: 60
            })
        );
        assert_eq!(
            client.reply_in_review("PRR_1", "PRRT_1", "x").await,
            Err(ProviderError::GraphQl("Thread is locked; try later".into()))
        );
    }

    #[tokio::test]
    async fn a_null_payload_is_a_refusal() {
        let server = MockServer::start().await;
        answer(
            &server,
            "addPullRequestReview(",
            data(json!({ "addPullRequestReview": { "pullRequestReview": null } })),
        )
        .await;
        answer(
            &server,
            "submitPullRequestReview",
            data(json!({ "submitPullRequestReview": null })),
        )
        .await;
        let client = gh(&server);
        let start = client.start_review("PR_7", "h1", &[]).await.unwrap_err();
        assert_eq!(
            start,
            ProviderError::EmptyPayload("the pending review".into())
        );
        assert_eq!(
            start.to_string(),
            "GitHub refused the request: the pending review came back empty"
        );
        assert!(start.is_ambiguous(), "GitHub may have acted anyway");
        assert!(matches!(
            client.submit_review("PRR_1", "COMMENT", "x").await,
            Err(ProviderError::EmptyPayload(_))
        ));
    }

    #[tokio::test]
    async fn http_failures_map_like_rest() {
        let server = MockServer::start().await;
        answer(&server, "headRefOid", ResponseTemplate::new(401)).await;
        answer(
            &server,
            "reviewThreads",
            ResponseTemplate::new(403).insert_header("retry-after", "30"),
        )
        .await;
        answer(
            &server,
            "resolveReviewThread",
            ResponseTemplate::new(502).set_body_json(json!({ "message": "Bad gateway" })),
        )
        .await;
        let client = gh(&server);
        assert_eq!(
            client.pr_node(&pr7()).await,
            Err(ProviderError::Unauthorized)
        );
        assert_eq!(
            client.review_threads(&pr7()).await,
            Err(ProviderError::RateLimited {
                retry_after_secs: 30
            })
        );
        assert_eq!(
            client.resolve_thread("PRRT_1").await,
            Err(ProviderError::Http {
                status: 502,
                message: "Bad gateway".into()
            })
        );
        let offline = GitHub::new("http://127.0.0.1:9", Token::new("t", TokenOrigin::Pat)).unwrap();
        assert!(matches!(
            offline.resolve_thread("PRRT_1").await,
            Err(ProviderError::Offline(_))
        ));
    }

    #[tokio::test]
    async fn graphql_writes_map_403_429_and_404() {
        let server = MockServer::start().await;
        answer(
            &server,
            "resolveReviewThread",
            ResponseTemplate::new(403)
                .set_body_json(json!({ "message": "Resource not accessible" })),
        )
        .await;
        answer(
            &server,
            "submitPullRequestReview",
            ResponseTemplate::new(429).insert_header("retry-after", "7"),
        )
        .await;
        answer(
            &server,
            "deletePullRequestReview",
            ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })),
        )
        .await;
        let client = gh(&server);
        assert_eq!(
            client.resolve_thread("PRRT_1").await,
            Err(ProviderError::Http {
                status: 403,
                message: "Resource not accessible".into()
            })
        );
        assert_eq!(
            client.submit_review("PRR_1", "COMMENT", "ok").await,
            Err(ProviderError::RateLimited {
                retry_after_secs: 7
            })
        );
        assert_eq!(
            client.delete_pending_review("PRR_1").await,
            Err(ProviderError::Http {
                status: 404,
                message: "Not Found".into()
            })
        );
    }
}
