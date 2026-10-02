//! GitHub REST v3: PR lists, PR detail and the current user, with conditional requests.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clusia_core::{PrDetail, PrFilter, PrRef, PrSummary};
use reqwest::{StatusCode, header};
use serde::Deserialize;

use crate::auth::Token;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProviderError {
    #[error("GitHub rejected the token (401); check `clusia auth status`")]
    Unauthorized,
    #[error("GitHub rate limit reached; retrying in {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },
    #[error("not found on GitHub: {0}")]
    NotFound(String),
    #[error("cannot reach GitHub: {0}")]
    Offline(String),
    #[error("GitHub returned {status}: {message}")]
    Http { status: u16, message: String },
    #[error("unexpected response from GitHub: {0}")]
    Decode(String),
}

pub fn api_base_for_host(host: &str) -> String {
    if host == "github.com" {
        "https://api.github.com".to_string()
    } else {
        format!("https://{host}/api/v3")
    }
}

pub fn search_query(filter: PrFilter) -> &'static str {
    match filter {
        PrFilter::Assigned => "is:pr is:open archived:false review-requested:@me",
        PrFilter::Mine => "is:pr is:open archived:false author:@me",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Viewer {
    pub login: String,
    pub scopes: Vec<String>,
}

pub struct GitHub {
    http: reqwest::Client,
    api: String,
    token: Token,
    /// Full URL → (ETag, body) for conditional requests.
    cache: Mutex<HashMap<String, (String, serde_json::Value)>>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Seconds to wait when the response is a rate-limit rejection.
fn rate_limit_wait(status: StatusCode, headers: &header::HeaderMap) -> Option<u64> {
    if status != StatusCode::FORBIDDEN && status != StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    let num = |name: &str| headers.get(name)?.to_str().ok()?.trim().parse::<u64>().ok();
    if let Some(secs) = num("retry-after") {
        return Some(secs.max(1));
    }
    if num("x-ratelimit-remaining") == Some(0) {
        return Some(
            num("x-ratelimit-reset").map_or(60, |reset| reset.saturating_sub(now_secs()).max(1)),
        );
    }
    (status == StatusCode::TOO_MANY_REQUESTS).then_some(60)
}

#[derive(Deserialize)]
struct UserRef {
    login: String,
}

#[derive(Deserialize)]
struct SearchResponse {
    items: Vec<IssueItem>,
}

#[derive(Deserialize)]
struct IssueItem {
    number: u64,
    title: String,
    html_url: String,
    user: UserRef,
    #[serde(default)]
    draft: bool,
    updated_at: String,
    #[serde(default)]
    comments: u64,
    repository_url: String,
}

#[derive(Deserialize)]
struct PullResponse {
    title: String,
    html_url: String,
    user: UserRef,
    #[serde(default)]
    draft: bool,
    updated_at: String,
    #[serde(default)]
    comments: u64,
    #[serde(default)]
    review_comments: u64,
    additions: u64,
    deletions: u64,
    changed_files: u64,
    base: Branch,
    head: Branch,
}

#[derive(Deserialize)]
struct Branch {
    #[serde(rename = "ref")]
    name: String,
    sha: String,
    repo: Option<RepoInfo>,
}

#[derive(Deserialize)]
struct RepoInfo {
    clone_url: String,
}

fn decode<T: serde::de::DeserializeOwned>(body: serde_json::Value) -> Result<T, ProviderError> {
    serde_json::from_value(body).map_err(|e| ProviderError::Decode(e.to_string()))
}

impl GitHub {
    pub fn new(api_base: &str, token: Token) -> Result<Self, ProviderError> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("clusia/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|e| ProviderError::Offline(e.to_string()))?;
        Ok(Self {
            http,
            api: api_base.trim_end_matches('/').to_string(),
            token,
            cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn api(&self) -> &str {
        &self.api
    }

    pub fn token(&self) -> &Token {
        &self.token
    }

    async fn get(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<(serde_json::Value, header::HeaderMap), ProviderError> {
        let mut request = self
            .http
            .get(format!("{}{}", self.api, path))
            .query(query)
            .bearer_auth(self.token.secret())
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .build()
            .map_err(|e| ProviderError::Decode(e.to_string()))?;
        let key = request.url().to_string();
        let cached = self
            .cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .cloned();
        if let Some((etag, _)) = &cached
            && let Ok(value) = header::HeaderValue::from_str(etag)
        {
            request.headers_mut().insert(header::IF_NONE_MATCH, value);
        }

        let response = self
            .http
            .execute(request)
            .await
            .map_err(|e| ProviderError::Offline(e.to_string()))?;
        let status = response.status();
        let headers = response.headers().clone();
        if status == StatusCode::NOT_MODIFIED {
            return match cached {
                Some((_, body)) => Ok((body, headers)),
                None => Err(ProviderError::Decode(
                    "304 without a cached response".into(),
                )),
            };
        }
        if status == StatusCode::UNAUTHORIZED {
            return Err(ProviderError::Unauthorized);
        }
        if let Some(retry_after_secs) = rate_limit_wait(status, &headers) {
            return Err(ProviderError::RateLimited { retry_after_secs });
        }
        let text = response
            .text()
            .await
            .map_err(|e| ProviderError::Offline(e.to_string()))?;
        if status == StatusCode::NOT_FOUND {
            return Err(ProviderError::NotFound(path.to_string()));
        }
        if !status.is_success() {
            let message = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("message")?.as_str().map(str::to_string))
                .unwrap_or_default();
            return Err(ProviderError::Http {
                status: status.as_u16(),
                message,
            });
        }
        let body: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| ProviderError::Decode(e.to_string()))?;
        if let Some(etag) = headers.get(header::ETAG).and_then(|v| v.to_str().ok()) {
            self.cache
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(key, (etag.to_string(), body.clone()));
        }
        Ok((body, headers))
    }

    pub async fn viewer(&self) -> Result<Viewer, ProviderError> {
        let (body, headers) = self.get("/user", &[]).await?;
        let login = body
            .get("login")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ProviderError::Decode("missing login".into()))?
            .to_string();
        let scopes = headers
            .get("x-oauth-scopes")
            .and_then(|v| v.to_str().ok())
            .map(|s| {
                s.split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Ok(Viewer { login, scopes })
    }

    pub async fn list_prs(&self, filter: PrFilter) -> Result<Vec<PrSummary>, ProviderError> {
        let query = [
            ("q", search_query(filter)),
            ("sort", "updated"),
            ("order", "desc"),
            ("per_page", "50"),
        ];
        let (body, _) = self.get("/search/issues", &query).await?;
        let parsed: SearchResponse = decode(body)?;
        parsed.items.into_iter().map(summary_from_issue).collect()
    }

    pub async fn get_pr(&self, pr: &PrRef) -> Result<PrDetail, ProviderError> {
        let path = format!("/repos/{}/{}/pulls/{}", pr.owner, pr.repo, pr.number);
        let (body, _) = self.get(&path, &[]).await?;
        let p: PullResponse = decode(body)?;
        let clone_url =
            p.base.repo.map(|r| r.clone_url).ok_or_else(|| {
                ProviderError::Decode("pull request has no base repository".into())
            })?;
        Ok(PrDetail {
            summary: PrSummary {
                pr: pr.clone(),
                title: p.title,
                author: p.user.login,
                url: p.html_url,
                draft: p.draft,
                updated_at: p.updated_at,
                comments: p.comments + p.review_comments,
            },
            base_ref: p.base.name,
            head_ref: p.head.name,
            base_sha: p.base.sha,
            head_sha: p.head.sha,
            additions: p.additions,
            deletions: p.deletions,
            changed_files: p.changed_files,
            clone_url,
        })
    }
}

fn summary_from_issue(item: IssueItem) -> Result<PrSummary, ProviderError> {
    let mut segments = item.repository_url.trim_end_matches('/').rsplit('/');
    let repo = segments.next().unwrap_or_default();
    let owner = segments.next().unwrap_or_default();
    let pr =
        PrRef::new(owner, repo, item.number).map_err(|e| ProviderError::Decode(e.to_string()))?;
    Ok(PrSummary {
        pr,
        title: item.title,
        author: item.user.login,
        url: item.html_url,
        draft: item.draft,
        updated_at: item.updated_at,
        comments: item.comments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::TokenOrigin;
    use serde_json::json;
    use wiremock::matchers::{header as h, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn gh(server: &MockServer) -> GitHub {
        GitHub::new(&server.uri(), Token::new("tok123", TokenOrigin::Pat)).unwrap()
    }

    fn issue(number: u64, repo: &str) -> serde_json::Value {
        json!({
            "number": number,
            "title": format!("PR {number}"),
            "html_url": format!("https://github.com/{repo}/pull/{number}"),
            "user": { "login": "maria" },
            "draft": false,
            "updated_at": "2026-10-01T12:00:00Z",
            "comments": 3,
            "repository_url": format!("https://api.github.com/repos/{repo}")
        })
    }

    #[test]
    fn api_base_for_hosts() {
        assert_eq!(api_base_for_host("github.com"), "https://api.github.com");
        assert_eq!(
            api_base_for_host("ghe.corp.example"),
            "https://ghe.corp.example/api/v3"
        );
    }

    #[tokio::test]
    async fn list_assigned_parses_results_and_sends_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search/issues"))
            .and(query_param(
                "q",
                "is:pr is:open archived:false review-requested:@me",
            ))
            .and(query_param("per_page", "50"))
            .and(h("authorization", "Bearer tok123"))
            .and(h("accept", "application/vnd.github+json"))
            .and(h("x-github-api-version", "2022-11-28"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "items": [issue(7, "acme/widgets")] })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let prs = gh(&server).list_prs(PrFilter::Assigned).await.unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].pr, "acme/widgets#7".parse().unwrap());
        assert_eq!(prs[0].author, "maria");
        assert_eq!(prs[0].comments, 3);
        assert_eq!(prs[0].url, "https://github.com/acme/widgets/pull/7");
    }

    #[tokio::test]
    async fn list_mine_uses_author_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search/issues"))
            .and(query_param("q", "is:pr is:open archived:false author:@me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [] })))
            .expect(1)
            .mount(&server)
            .await;
        assert!(
            gh(&server)
                .list_prs(PrFilter::Mine)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn etag_304_reuses_the_cached_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search/issues"))
            .and(h("if-none-match", "\"abc\""))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/search/issues"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"abc\"")
                    .set_body_json(json!({ "items": [issue(7, "acme/widgets")] })),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        let client = gh(&server);
        let first = client.list_prs(PrFilter::Assigned).await.unwrap();
        let second = client.list_prs(PrFilter::Assigned).await.unwrap();
        assert_eq!(first, second);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn unauthorized_maps_to_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(json!({ "message": "Bad credentials" })),
            )
            .mount(&server)
            .await;
        assert_eq!(
            gh(&server).list_prs(PrFilter::Assigned).await,
            Err(ProviderError::Unauthorized)
        );
    }

    #[tokio::test]
    async fn rate_limit_waits_until_reset() {
        let server = MockServer::start().await;
        let reset = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 120;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(403)
                    .insert_header("x-ratelimit-remaining", "0")
                    .insert_header("x-ratelimit-reset", reset.to_string().as_str())
                    .set_body_json(json!({ "message": "API rate limit exceeded" })),
            )
            .mount(&server)
            .await;
        match gh(&server).list_prs(PrFilter::Assigned).await {
            Err(ProviderError::RateLimited { retry_after_secs }) => {
                assert!((110..=121).contains(&retry_after_secs))
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retry_after_header_wins() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30"))
            .mount(&server)
            .await;
        assert_eq!(
            gh(&server).list_prs(PrFilter::Assigned).await,
            Err(ProviderError::RateLimited {
                retry_after_secs: 30
            })
        );
    }

    #[tokio::test]
    async fn plain_403_is_an_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(json!({ "message": "Resource not accessible" })),
            )
            .mount(&server)
            .await;
        assert_eq!(
            gh(&server).list_prs(PrFilter::Assigned).await,
            Err(ProviderError::Http {
                status: 403,
                message: "Resource not accessible".into()
            })
        );
    }

    #[tokio::test]
    async fn get_pr_parses_detail() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/7"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "number": 7, "title": "Fix cache", "html_url": "https://github.com/acme/widgets/pull/7",
                "user": { "login": "maria" }, "draft": true, "updated_at": "2026-10-01T12:00:00Z",
                "comments": 2, "review_comments": 5, "additions": 120, "deletions": 34, "changed_files": 7,
                "base": { "ref": "main", "sha": "aaa", "repo": { "clone_url": "https://github.com/acme/widgets.git" } },
                "head": { "ref": "fix-cache", "sha": "bbb", "repo": null }
            })))
            .mount(&server)
            .await;
        let d = gh(&server)
            .get_pr(&"acme/widgets#7".parse().unwrap())
            .await
            .unwrap();
        assert_eq!(
            (d.base_ref.as_str(), d.head_ref.as_str()),
            ("main", "fix-cache")
        );
        assert_eq!((d.base_sha.as_str(), d.head_sha.as_str()), ("aaa", "bbb"));
        assert_eq!((d.additions, d.deletions, d.changed_files), (120, 34, 7));
        assert_eq!(d.summary.comments, 7);
        assert!(d.summary.draft);
        assert_eq!(d.clone_url, "https://github.com/acme/widgets.git");
    }

    #[tokio::test]
    async fn missing_pr_is_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;
        assert!(matches!(
            gh(&server).get_pr(&"acme/widgets#9".parse().unwrap()).await,
            Err(ProviderError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn viewer_reads_login_and_scopes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-oauth-scopes", "repo, read:org")
                    .set_body_json(json!({ "login": "octo" })),
            )
            .mount(&server)
            .await;
        let v = gh(&server).viewer().await.unwrap();
        assert_eq!(
            v,
            Viewer {
                login: "octo".into(),
                scopes: vec!["repo".into(), "read:org".into()]
            }
        );
    }

    #[tokio::test]
    async fn unreachable_server_is_offline() {
        let client = GitHub::new("http://127.0.0.1:9", Token::new("t", TokenOrigin::Pat)).unwrap();
        assert!(matches!(
            client.list_prs(PrFilter::Assigned).await,
            Err(ProviderError::Offline(_))
        ));
    }
}
