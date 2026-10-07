//! What GitHub says needs attention: the notifications feed, a pull request's reviews and its
//! combined check state.

use clusia_core::{ChecksSummary, PrRef, ReviewInfo};
use serde::Deserialize;

use crate::github::{GitHub, ProviderError, RawReview, decode, decode_list, submitted_reviews};

/// One entry of `GET /notifications`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub id: String,
    /// `review_requested`, `mention`, `team_mention`, `comment`, `author`, …
    pub reason: String,
    pub unread: bool,
    pub updated_at: String,
    /// `PullRequest`, `Issue`, `Commit`, …
    pub subject_type: String,
    pub title: String,
    pub subject_url: Option<String>,
    pub latest_comment_url: Option<String>,
    /// `owner/repo`.
    pub repository: String,
}

impl Notification {
    /// The pull request the notification is about; `None` for issues, commits and the like.
    pub fn pr(&self) -> Option<PrRef> {
        if self.subject_type != "PullRequest" {
            return None;
        }
        let mut segments = self.subject_url.as_deref()?.rsplit('/');
        let number = segments.next()?.parse().ok()?;
        if segments.next()? != "pulls" {
            return None;
        }
        let repo = segments.next()?;
        let owner = segments.next()?;
        PrRef::new(owner, repo, number).ok()
    }

    /// The id of the comment that caused the notification, when it points at one. When a thread
    /// changed without a new comment (a push, a review, a label), GitHub points
    /// `latest_comment_url` at the pull request itself, whose number is no comment id.
    pub fn comment_id(&self) -> Option<u64> {
        let mut segments = self.latest_comment_url.as_deref()?.rsplit('/');
        let id = segments.next()?.parse().ok()?;
        (segments.next()? == "comments").then_some(id)
    }
}

#[derive(Deserialize)]
struct RawNotification {
    id: String,
    reason: String,
    unread: bool,
    updated_at: String,
    subject: RawSubject,
    repository: RawRepository,
}

#[derive(Deserialize)]
struct RawSubject {
    title: String,
    url: Option<String>,
    latest_comment_url: Option<String>,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct RawRepository {
    full_name: String,
}

#[derive(Deserialize)]
struct RawCombinedStatus {
    state: String,
    #[serde(default)]
    total_count: u32,
}

/// Where a commit's checks stand, counting check runs and the older commit statuses together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    /// Neither check runs nor statuses.
    NoChecks,
    Pending,
    Passing,
    Failing,
}

/// The combined state and how many checks are failing (a failing commit status counts as
/// one, GitHub does not say how many it folded together).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckReport {
    pub state: CheckState,
    pub failed: u32,
}

/// A failure anywhere is `Failing`, even while other checks still run.
fn combine(runs: ChecksSummary, status: &RawCombinedStatus) -> CheckReport {
    let has_status = status.total_count > 0;
    let status_failed = has_status && matches!(status.state.as_str(), "failure" | "error");
    let state = if runs.failed > 0 || status_failed {
        CheckState::Failing
    } else if runs.pending > 0 || (has_status && status.state == "pending") {
        CheckState::Pending
    } else if runs.total > 0 || has_status {
        CheckState::Passing
    } else {
        CheckState::NoChecks
    };
    CheckReport {
        state,
        failed: runs.failed.max(u32::from(status_failed)),
    }
}

impl GitHub {
    /// Notifications you take part in (`participating=true`), read ones included, updated
    /// after `since` (an RFC 3339 time). An entry GitHub sends in an unexpected shape is
    /// skipped, not fatal.
    pub async fn list_notifications(
        &self,
        since: Option<&str>,
    ) -> Result<Vec<Notification>, ProviderError> {
        let mut query = vec![("participating", "true"), ("all", "true")];
        if let Some(since) = since {
            query.push(("since", since));
        }
        let raw = self
            .get_paginated_with("/notifications", &query, false)
            .await?;
        Ok(raw
            .into_iter()
            .filter_map(|value| {
                serde_json::from_value::<RawNotification>(value)
                    .inspect_err(|e| tracing::warn!(error = %e, "skipping a notification"))
                    .ok()
            })
            .map(|n| Notification {
                id: n.id,
                reason: n.reason,
                unread: n.unread,
                updated_at: n.updated_at,
                subject_type: n.subject.kind,
                title: n.subject.title,
                subject_url: n.subject.url,
                latest_comment_url: n.subject.latest_comment_url,
                repository: n.repository.full_name,
            })
            .collect())
    }

    /// Every submitted review of `pr`, oldest first; pending ones (always yours) are left out.
    pub async fn list_reviews(&self, pr: &PrRef) -> Result<Vec<ReviewInfo>, ProviderError> {
        let raw: Vec<RawReview> = decode_list(
            self.get_paginated(&format!(
                "{}/pulls/{}/reviews",
                Self::repo_path(pr),
                pr.number
            ))
            .await?,
        )?;
        Ok(submitted_reviews(raw))
    }

    /// The combined state of the check runs and commit statuses of `sha`.
    pub async fn get_check_report(
        &self,
        pr: &PrRef,
        sha: &str,
    ) -> Result<CheckReport, ProviderError> {
        let runs = self.get_checks(pr, sha).await?;
        let path = format!("{}/commits/{sha}/status", Self::repo_path(pr));
        let (body, _) = self.get(&path, &[("per_page", "100")]).await?;
        let status: RawCombinedStatus = decode(body)?;
        Ok(combine(runs, &status))
    }

    /// The login of whoever wrote the comment at `url`, a `latest_comment_url` of a
    /// notification. Only the path of `url` is used, so the request always goes to this
    /// client's own API host.
    pub async fn comment_author(&self, url: &str) -> Result<String, ProviderError> {
        let given = reqwest::Url::parse(url).map_err(|e| ProviderError::Decode(e.to_string()))?;
        let base =
            reqwest::Url::parse(self.api()).map_err(|e| ProviderError::Decode(e.to_string()))?;
        let prefix = base.path().trim_end_matches('/');
        let path = given.path().strip_prefix(prefix).unwrap_or(given.path());
        let (body, _) = self.get(path, &[]).await?;
        body.get("user")
            .and_then(|user| user.get("login"))
            .and_then(|login| login.as_str())
            .map(str::to_string)
            .ok_or_else(|| ProviderError::Decode("the comment has no author".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Token, TokenOrigin};
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn gh(server: &MockServer) -> GitHub {
        GitHub::new(&server.uri(), Token::new("tok123", TokenOrigin::Pat)).unwrap()
    }

    fn pr7() -> PrRef {
        PrRef::new("acme", "widgets", 7).unwrap()
    }

    fn entry(id: &str, reason: &str, subject_type: &str, number: u64) -> serde_json::Value {
        json!({
            "id": id,
            "reason": reason,
            "unread": true,
            "updated_at": "2026-10-01T12:00:00Z",
            "subject": {
                "title": "Add feature",
                "url": format!("https://api.github.com/repos/acme/widgets/pulls/{number}"),
                "latest_comment_url": "https://api.github.com/repos/acme/widgets/issues/comments/991",
                "type": subject_type
            },
            "repository": { "full_name": "acme/widgets" }
        })
    }

    #[tokio::test]
    async fn notifications_ask_for_what_you_take_part_in_since_a_time() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/notifications"))
            .and(query_param("participating", "true"))
            .and(query_param("all", "true"))
            .and(query_param("since", "2026-10-01T00:00:00Z"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                entry("1", "review_requested", "PullRequest", 7),
                entry("2", "mention", "Issue", 8),
                { "id": "broken" }
            ])))
            .expect(1)
            .mount(&server)
            .await;
        let list = gh(&server)
            .list_notifications(Some("2026-10-01T00:00:00Z"))
            .await
            .unwrap();
        assert_eq!(list.len(), 2, "the malformed entry is skipped");
        assert_eq!(list[0].reason, "review_requested");
        assert_eq!(list[0].repository, "acme/widgets");
        assert_eq!(list[0].pr(), Some(pr7()));
        assert_eq!(list[0].comment_id(), Some(991));
        assert_eq!(list[1].pr(), None, "an issue is not a pull request");
    }

    #[tokio::test]
    async fn notifications_without_since_send_no_since() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/notifications"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        assert!(
            gh(&server)
                .list_notifications(None)
                .await
                .unwrap()
                .is_empty()
        );
        let requests = server.received_requests().await.unwrap();
        assert!(!requests[0].url.query().unwrap_or("").contains("since"));
    }

    #[tokio::test]
    async fn notifications_map_http_failures() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/notifications"))
            .respond_with(ResponseTemplate::new(403).set_body_json(
                json!({ "message": "Resource not accessible by personal access token" }),
            ))
            .mount(&server)
            .await;
        assert_eq!(
            gh(&server).list_notifications(None).await,
            Err(ProviderError::Http {
                status: 403,
                message: "Resource not accessible by personal access token".into()
            })
        );
    }

    #[tokio::test]
    async fn the_feed_is_asked_afresh_every_time() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/notifications"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"abc\"")
                    .set_body_json(json!([])),
            )
            .mount(&server)
            .await;
        let client = gh(&server);
        client.list_notifications(None).await.unwrap();
        client.list_notifications(None).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|r| !r.headers.contains_key("if-none-match")),
            "a revalidated feed would repeat entries the daemon already told about"
        );
    }

    #[test]
    fn a_notification_names_its_pull_request_only_when_it_is_one() {
        let mut n = Notification {
            id: "1".into(),
            reason: "mention".into(),
            unread: true,
            updated_at: String::new(),
            subject_type: "PullRequest".into(),
            title: String::new(),
            subject_url: Some("https://api.github.com/repos/rzorzal/clusia/pulls/123".into()),
            latest_comment_url: None,
            repository: "rzorzal/clusia".into(),
        };
        assert_eq!(n.pr(), PrRef::new("rzorzal", "clusia", 123).ok());
        assert_eq!(n.comment_id(), None);
        n.latest_comment_url =
            Some("https://api.github.com/repos/rzorzal/clusia/pulls/comments/42".into());
        assert_eq!(n.comment_id(), Some(42), "a review comment");
        n.latest_comment_url = Some("https://api.github.com/repos/rzorzal/clusia/pulls/123".into());
        assert_eq!(
            n.comment_id(),
            None,
            "the thread changed without a new comment: the URL is the pull request's"
        );
        n.latest_comment_url =
            Some("https://api.github.com/repos/rzorzal/clusia/issues/123".into());
        assert_eq!(n.comment_id(), None);
        n.subject_url = Some("https://api.github.com/repos/rzorzal/clusia/issues/5".into());
        assert_eq!(n.pr(), None);
        n.subject_url = None;
        assert_eq!(n.pr(), None);
        n.subject_type = "Issue".into();
        n.subject_url = Some("https://api.github.com/repos/rzorzal/clusia/pulls/9".into());
        assert_eq!(n.pr(), None);
    }

    #[tokio::test]
    async fn a_comment_author_is_read_through_this_clients_own_host() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/issues/comments/991"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "id": 991, "user": { "login": "octo" } })),
            )
            .expect(2)
            .mount(&server)
            .await;
        let client = gh(&server);
        // The URL GitHub sends names its own host; the request still goes to ours.
        let from_github = "https://api.github.com/repos/acme/widgets/issues/comments/991";
        assert_eq!(client.comment_author(from_github).await.unwrap(), "octo");
        let local = format!("{}/repos/acme/widgets/issues/comments/991", server.uri());
        assert_eq!(client.comment_author(&local).await.unwrap(), "octo");
        assert!(matches!(
            client.comment_author("not a url").await,
            Err(ProviderError::Decode(_))
        ));
    }

    #[tokio::test]
    async fn a_comment_without_an_author_is_a_decode_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/issues/comments/1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "id": 1, "user": null })),
            )
            .mount(&server)
            .await;
        let url = format!("{}/repos/acme/widgets/issues/comments/1", server.uri());
        assert!(matches!(
            gh(&server).comment_author(&url).await,
            Err(ProviderError::Decode(_))
        ));
    }

    #[tokio::test]
    async fn reviews_carry_the_commit_they_were_made_on() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/7/reviews"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "id": 5, "user": { "login": "mona" }, "state": "APPROVED", "body": "ok",
                  "submitted_at": "2026-10-01T12:30:00Z", "html_url": "u5", "commit_id": "abc123" },
                { "id": 6, "user": { "login": "joao" }, "state": "PENDING", "body": "",
                  "html_url": "u6", "commit_id": "abc123" },
                { "id": 7, "user": null, "state": "COMMENTED", "body": null,
                  "submitted_at": "2026-10-01T13:00:00Z", "html_url": "u7" }
            ])))
            .mount(&server)
            .await;
        let reviews = gh(&server).list_reviews(&pr7()).await.unwrap();
        assert_eq!(reviews.len(), 2, "pending reviews are dropped");
        assert_eq!(reviews[0].commit_id.as_deref(), Some("abc123"));
        assert_eq!(reviews[0].author, "mona");
        assert_eq!(reviews[1].commit_id, None);
        assert_eq!(reviews[1].author, "ghost");
    }

    async fn checks(server: &MockServer, runs: serde_json::Value, status: (&str, u32)) {
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/commits/c2/check-runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "check_runs": runs })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/commits/c2/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({ "state": status.0, "total_count": status.1, "statuses": [] }),
            ))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn check_state_combines_runs_and_statuses() {
        let ok = json!({ "status": "completed", "conclusion": "success" });
        let bad = json!({ "status": "completed", "conclusion": "failure" });
        let running = json!({ "status": "in_progress", "conclusion": null });
        let cases = [
            (json!([]), ("pending", 0), CheckState::NoChecks, 0),
            (json!([ok]), ("pending", 0), CheckState::Passing, 0),
            (json!([]), ("success", 2), CheckState::Passing, 0),
            (json!([ok, running]), ("pending", 0), CheckState::Pending, 0),
            (json!([ok]), ("pending", 1), CheckState::Pending, 0),
            (
                json!([ok, bad, bad, running]),
                ("pending", 0),
                CheckState::Failing,
                2,
            ),
            (json!([ok]), ("failure", 1), CheckState::Failing, 1),
            (json!([ok]), ("error", 1), CheckState::Failing, 1),
        ];
        for (runs, status, state, failed) in cases {
            let server = MockServer::start().await;
            checks(&server, runs.clone(), status).await;
            assert_eq!(
                gh(&server).get_check_report(&pr7(), "c2").await.unwrap(),
                CheckReport { state, failed },
                "{runs} {status:?}"
            );
        }
    }
}
