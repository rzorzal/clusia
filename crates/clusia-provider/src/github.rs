//! GitHub REST v3: PR lists, PR detail and the current user, with conditional requests.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clusia_core::{
    ChecksSummary, CommitInfo, FileDiff, IssueComment, PrConversation, PrDetail, PrFilter, PrRef,
    PrSummary, ReviewInfo, Side, ThreadComment,
};
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
    cache: Mutex<HashMap<String, CacheEntry>>,
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

/// ETag, body and the `Link` header (a 304 carries none).
type CacheEntry = (String, serde_json::Value, Option<header::HeaderValue>);

#[derive(Deserialize)]
struct UserRef {
    login: String,
}

const MAX_PAGES: usize = 30;

/// Whether a pagination link points back into the API (same origin, under the API path).
fn same_api(api: &reqwest::Url, next: &str) -> bool {
    let Ok(next) = reqwest::Url::parse(next) else {
        return false;
    };
    let prefix = format!("{}/", api.path().trim_end_matches('/'));
    next.scheme() == api.scheme()
        && next.host() == api.host()
        && next.port_or_known_default() == api.port_or_known_default()
        && next.path().starts_with(&prefix)
}

fn next_link(headers: &header::HeaderMap) -> Option<String> {
    headers
        .get(header::LINK)?
        .to_str()
        .ok()?
        .split(',')
        .find_map(|part| {
            let (url, rel) = part.split_once(';')?;
            rel.contains("rel=\"next\"").then(|| {
                url.trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string()
            })
        })
}

#[derive(Deserialize)]
struct RawFile {
    filename: String,
    previous_filename: Option<String>,
    status: String,
    #[serde(default)]
    additions: u64,
    #[serde(default)]
    deletions: u64,
    patch: Option<String>,
}

#[derive(Deserialize)]
struct RawThread {
    id: u64,
    in_reply_to_id: Option<u64>,
    path: String,
    line: Option<u32>,
    side: Option<String>,
    user: Option<UserRef>,
    body: String,
    created_at: String,
    html_url: String,
}

#[derive(Deserialize)]
struct RawIssueComment {
    id: u64,
    user: Option<UserRef>,
    body: String,
    created_at: String,
    html_url: String,
}

#[derive(Deserialize)]
struct RawReview {
    id: u64,
    user: Option<UserRef>,
    state: String,
    #[serde(default)]
    body: Option<String>,
    submitted_at: Option<String>,
    html_url: String,
}

#[derive(Deserialize)]
struct RawCommit {
    sha: String,
    author: Option<UserRef>,
    commit: RawCommitInner,
}

#[derive(Deserialize)]
struct RawCommitInner {
    message: String,
    author: RawGitAuthor,
}

#[derive(Deserialize)]
struct RawGitAuthor {
    name: String,
    date: String,
}

#[derive(Deserialize)]
struct RawCheckRuns {
    check_runs: Vec<RawCheckRun>,
}

#[derive(Deserialize)]
struct RawCheckRun {
    status: String,
    conclusion: Option<String>,
}

fn login(user: Option<UserRef>) -> String {
    user.map(|u| u.login).unwrap_or_else(|| "ghost".to_string())
}

fn decode_list<T: serde::de::DeserializeOwned>(
    values: Vec<serde_json::Value>,
) -> Result<Vec<T>, ProviderError> {
    values.into_iter().map(decode).collect()
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedReview {
    pub id: u64,
    pub url: String,
}

#[derive(Deserialize)]
struct RawPublished {
    id: u64,
    html_url: String,
}

/// `message` plus the details of `errors[]` (strings or `{ "message": … }` objects).
fn error_message(body: &serde_json::Value) -> String {
    let message = body
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let details: Vec<String> = body
        .get("errors")
        .and_then(|e| e.as_array())
        .map(|errors| {
            errors
                .iter()
                .filter_map(|e| {
                    e.as_str()
                        .map(str::to_string)
                        .or_else(|| e.get("message")?.as_str().map(str::to_string))
                })
                .collect()
        })
        .unwrap_or_default();
    if details.is_empty() {
        message
    } else {
        format!("{message}: {}", details.join("; "))
    }
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

    /// Every page of a list endpoint (`per_page=100`, following `Link: rel="next"`).
    async fn get_paginated(&self, path: &str) -> Result<Vec<serde_json::Value>, ProviderError> {
        let (first, mut headers) = self.request(path, &[("per_page", "100")], true).await?;
        let mut items = match first {
            serde_json::Value::Array(a) => a,
            _ => {
                return Err(ProviderError::Decode(format!(
                    "{path} did not return a list"
                )));
            }
        };
        for _ in 1..MAX_PAGES {
            let Some(next) = next_link(&headers) else {
                break;
            };
            let api = reqwest::Url::parse(&self.api).ok();
            if !api.is_some_and(|api| same_api(&api, &next)) {
                tracing::warn!(path, "ignoring pagination link to a different host");
                break;
            }
            let (page, h) = self.request_url(next, &[], true).await?;
            headers = h;
            match page {
                serde_json::Value::Array(a) => items.extend(a),
                _ => {
                    return Err(ProviderError::Decode(format!(
                        "{path} did not return a list"
                    )));
                }
            }
        }
        Ok(items)
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, ProviderError> {
        let response = self
            .http
            .request(method, format!("{}{}", self.api, path))
            .bearer_auth(self.token.secret())
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(body)
            .send()
            .await
            .map_err(|e| ProviderError::Offline(e.to_string()))?;
        let status = response.status();
        let headers = response.headers().clone();
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
        let parsed = if text.trim().is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
        };
        if !status.is_success() {
            return Err(ProviderError::Http {
                status: status.as_u16(),
                message: error_message(&parsed),
            });
        }
        Ok(parsed)
    }

    pub async fn create_review(
        &self,
        pr: &PrRef,
        payload: &clusia_core::ReviewPayload,
    ) -> Result<PublishedReview, ProviderError> {
        let body =
            serde_json::to_value(payload).map_err(|e| ProviderError::Decode(e.to_string()))?;
        let path = format!("{}/pulls/{}/reviews", Self::repo_path(pr), pr.number);
        let raw: RawPublished = decode(self.send(reqwest::Method::POST, &path, &body).await?)?;
        Ok(PublishedReview {
            id: raw.id,
            url: raw.html_url,
        })
    }

    pub async fn close_pr(&self, pr: &PrRef) -> Result<(), ProviderError> {
        let path = format!("{}/pulls/{}", Self::repo_path(pr), pr.number);
        self.send(
            reqwest::Method::PATCH,
            &path,
            &serde_json::json!({ "state": "closed" }),
        )
        .await
        .map(|_| ())
    }

    fn repo_path(pr: &PrRef) -> String {
        format!("/repos/{}/{}", pr.owner, pr.repo)
    }

    pub async fn get_files(&self, pr: &PrRef) -> Result<Vec<FileDiff>, ProviderError> {
        let raw: Vec<RawFile> = decode_list(
            self.get_paginated(&format!(
                "{}/pulls/{}/files",
                Self::repo_path(pr),
                pr.number
            ))
            .await?,
        )?;
        Ok(raw
            .into_iter()
            .map(|f| FileDiff {
                path: f.filename,
                previous_path: f.previous_filename,
                status: f.status,
                additions: f.additions,
                deletions: f.deletions,
                patch: f.patch,
            })
            .collect())
    }

    pub async fn get_conversation(&self, pr: &PrRef) -> Result<PrConversation, ProviderError> {
        let base = Self::repo_path(pr);
        let threads: Vec<RawThread> = decode_list(
            self.get_paginated(&format!("{base}/pulls/{}/comments", pr.number))
                .await?,
        )?;
        let comments: Vec<RawIssueComment> = decode_list(
            self.get_paginated(&format!("{base}/issues/{}/comments", pr.number))
                .await?,
        )?;
        let reviews: Vec<RawReview> = decode_list(
            self.get_paginated(&format!("{base}/pulls/{}/reviews", pr.number))
                .await?,
        )?;
        Ok(PrConversation {
            threads: threads
                .into_iter()
                .map(|t| ThreadComment {
                    id: t.id,
                    in_reply_to: t.in_reply_to_id,
                    path: t.path,
                    line: t.line,
                    side: match t.side.as_deref() {
                        Some("LEFT") => Some(Side::Left),
                        Some("RIGHT") => Some(Side::Right),
                        _ => None,
                    },
                    author: login(t.user),
                    body: t.body,
                    created_at: t.created_at,
                    url: t.html_url,
                })
                .collect(),
            comments: comments
                .into_iter()
                .map(|c| IssueComment {
                    id: c.id,
                    author: login(c.user),
                    body: c.body,
                    created_at: c.created_at,
                    url: c.html_url,
                })
                .collect(),
            reviews: reviews
                .into_iter()
                .filter(|r| r.state != "PENDING")
                .map(|r| ReviewInfo {
                    id: r.id,
                    author: login(r.user),
                    state: r.state,
                    body: r.body.unwrap_or_default(),
                    submitted_at: r.submitted_at,
                    url: r.html_url,
                })
                .collect(),
        })
    }

    pub async fn get_commits(&self, pr: &PrRef) -> Result<Vec<CommitInfo>, ProviderError> {
        let raw: Vec<RawCommit> = decode_list(
            self.get_paginated(&format!(
                "{}/pulls/{}/commits",
                Self::repo_path(pr),
                pr.number
            ))
            .await?,
        )?;
        Ok(raw
            .into_iter()
            .map(|c| CommitInfo {
                sha: c.sha,
                author: c.author.map(|u| u.login).unwrap_or(c.commit.author.name),
                message: c
                    .commit
                    .message
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string(),
                date: c.commit.author.date,
            })
            .collect())
    }

    pub async fn get_checks(&self, pr: &PrRef, sha: &str) -> Result<ChecksSummary, ProviderError> {
        let path = format!("{}/commits/{sha}/check-runs", Self::repo_path(pr));
        let (body, _) = self.get(&path, &[("per_page", "100")]).await?;
        let runs: RawCheckRuns = decode(body)?;
        let mut summary = ChecksSummary::default();
        for run in runs.check_runs {
            summary.total += 1;
            if run.status != "completed" {
                summary.pending += 1;
            } else if matches!(
                run.conclusion.as_deref(),
                Some("success" | "neutral" | "skipped")
            ) {
                summary.passed += 1;
            } else {
                summary.failed += 1;
            }
        }
        Ok(summary)
    }

    async fn get(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<(serde_json::Value, header::HeaderMap), ProviderError> {
        self.request(path, query, true).await
    }

    /// GET without the ETag cache, for responses whose headers matter (a 304 carries none).
    async fn get_uncached(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<(serde_json::Value, header::HeaderMap), ProviderError> {
        self.request(path, query, false).await
    }

    async fn request(
        &self,
        path: &str,
        query: &[(&str, &str)],
        use_cache: bool,
    ) -> Result<(serde_json::Value, header::HeaderMap), ProviderError> {
        self.request_url(format!("{}{}", self.api, path), query, use_cache)
            .await
    }

    async fn request_url(
        &self,
        url: String,
        query: &[(&str, &str)],
        use_cache: bool,
    ) -> Result<(serde_json::Value, header::HeaderMap), ProviderError> {
        let mut request = self
            .http
            .get(url)
            .query(query)
            .bearer_auth(self.token.secret())
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .build()
            .map_err(|e| ProviderError::Decode(e.to_string()))?;
        let request_path = request.url().path().to_string();
        let key = request.url().to_string();
        let cached = if use_cache {
            self.cache
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&key)
                .cloned()
        } else {
            None
        };
        if let Some((etag, _, _)) = &cached
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
        let mut headers = response.headers().clone();
        if status == StatusCode::NOT_MODIFIED {
            return match cached {
                Some((_, body, link)) => {
                    // A 304 carries no `Link`; restore the cached one so pagination continues.
                    if let Some(link) = link {
                        headers.insert(header::LINK, link);
                    }
                    Ok((body, headers))
                }
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
            return Err(ProviderError::NotFound(request_path));
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
        if use_cache && let Some(etag) = headers.get(header::ETAG).and_then(|v| v.to_str().ok()) {
            self.cache.lock().unwrap_or_else(|p| p.into_inner()).insert(
                key,
                (
                    etag.to_string(),
                    body.clone(),
                    headers.get(header::LINK).cloned(),
                ),
            );
        }
        Ok((body, headers))
    }

    pub async fn viewer(&self) -> Result<Viewer, ProviderError> {
        let (body, headers) = self.get_uncached("/user", &[]).await?;
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
        // One malformed item must not hide the others.
        Ok(parsed
            .items
            .into_iter()
            .filter_map(|item| {
                let number = item.number;
                summary_from_issue(item)
                    .inspect_err(|e| tracing::warn!(number, error = %e, "skipping search result"))
                    .ok()
            })
            .collect())
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
    async fn one_bad_search_item_is_skipped() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({ "items": [issue(7, "acme/widgets"), issue(8, "bad.owner/widgets")] }),
            ))
            .mount(&server)
            .await;
        let prs = gh(&server).list_prs(PrFilter::Assigned).await.unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].pr, "acme/widgets#7".parse().unwrap());
    }

    #[tokio::test]
    async fn viewer_never_sends_if_none_match() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .and(wiremock::matchers::header_exists("if-none-match"))
            .respond_with(ResponseTemplate::new(304))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/user"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"u1\"")
                    .insert_header("x-oauth-scopes", "repo")
                    .set_body_json(json!({ "login": "octo" })),
            )
            .mount(&server)
            .await;
        let client = gh(&server);
        client.viewer().await.unwrap();
        let second = client.viewer().await.unwrap();
        assert_eq!(second.scopes, vec!["repo".to_string()]);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|r| !r.headers.contains_key("if-none-match")),
            "viewer() must bypass the ETag cache"
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

    fn pr7() -> PrRef {
        "acme/widgets#7".parse().unwrap()
    }

    #[tokio::test]
    async fn files_follow_pagination() {
        let server = MockServer::start().await;
        let next = format!(
            "<{}/repos/acme/widgets/pulls/7/files?per_page=100&page=2>; rel=\"next\"",
            server.uri()
        );
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/7/files"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "filename": "b.rs", "status": "renamed", "previous_filename": "old_b.rs", "additions": 0, "deletions": 0 }
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/7/files"))
            .and(query_param("per_page", "100"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("link", next.as_str())
                    .set_body_json(json!([
                        { "filename": "a.rs", "status": "modified", "additions": 2, "deletions": 1, "patch": "@@ -1 +1,2 @@\n-a\n+b\n+c" }
                    ])),
            )
            .with_priority(10)
            .mount(&server)
            .await;
        let files = gh(&server).get_files(&pr7()).await.unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(
            (
                files[0].path.as_str(),
                files[0].additions,
                files[0].patch.is_some()
            ),
            ("a.rs", 2, true)
        );
        assert_eq!(
            (files[1].path.as_str(), files[1].previous_path.as_deref()),
            ("b.rs", Some("old_b.rs"))
        );
    }

    #[tokio::test]
    async fn conversation_collects_threads_comments_and_reviews() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/repos/acme/widgets/pulls/7/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "id": 1, "path": "a.rs", "line": 3, "side": "RIGHT", "user": { "login": "joao" }, "body": "why?", "created_at": "2026-10-01T10:00:00Z", "html_url": "u1" },
                { "id": 2, "in_reply_to_id": 1, "path": "a.rs", "line": null, "side": null, "user": { "login": "maria" }, "body": "because", "created_at": "2026-10-01T11:00:00Z", "html_url": "u2" }
            ]))).mount(&server).await;
        Mock::given(method("GET")).and(path("/repos/acme/widgets/issues/7/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "id": 9, "user": { "login": "ana" }, "body": "LGTM", "created_at": "2026-10-01T12:00:00Z", "html_url": "u9" }
            ]))).mount(&server).await;
        Mock::given(method("GET")).and(path("/repos/acme/widgets/pulls/7/reviews"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "id": 5, "user": { "login": "ana" }, "state": "APPROVED", "body": "", "submitted_at": "2026-10-01T12:30:00Z", "html_url": "u5" },
                { "id": 6, "user": { "login": "me" }, "state": "PENDING", "body": "", "html_url": "u6" }
            ]))).mount(&server).await;
        let c = gh(&server).get_conversation(&pr7()).await.unwrap();
        assert_eq!(c.threads.len(), 2);
        assert_eq!(
            (
                c.threads[0].side,
                c.threads[1].in_reply_to,
                c.threads[1].line
            ),
            (Some(Side::Right), Some(1), None)
        );
        assert_eq!(c.comments[0].author, "ana");
        assert_eq!(c.reviews.len(), 1, "pending reviews are dropped");
        assert_eq!(c.reviews[0].state, "APPROVED");
    }

    #[tokio::test]
    async fn commits_and_checks() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/repos/acme/widgets/pulls/7/commits"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "sha": "c1", "author": { "login": "maria" }, "commit": { "message": "fix: x\n\nbody", "author": { "name": "Maria", "date": "2026-10-01T09:00:00Z" } } },
                { "sha": "c2", "author": null, "commit": { "message": "wip", "author": { "name": "Bot", "date": "2026-10-01T09:30:00Z" } } }
            ]))).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/commits/c2/check-runs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total_count": 3,
                "check_runs": [
                    { "status": "completed", "conclusion": "success" },
                    { "status": "completed", "conclusion": "failure" },
                    { "status": "in_progress", "conclusion": null }
                ]
            })))
            .mount(&server)
            .await;
        let client = gh(&server);
        let commits = client.get_commits(&pr7()).await.unwrap();
        assert_eq!(
            (commits[0].author.as_str(), commits[0].message.as_str()),
            ("maria", "fix: x")
        );
        assert_eq!(commits[1].author, "Bot");
        let checks = client.get_checks(&pr7(), "c2").await.unwrap();
        assert_eq!(
            checks,
            ChecksSummary {
                total: 3,
                passed: 1,
                failed: 1,
                pending: 1
            }
        );
    }

    #[tokio::test]
    async fn pagination_survives_304_revalidation() {
        let server = MockServer::start().await;
        let next = format!(
            "<{}/repos/acme/widgets/pulls/7/files?per_page=100&page=2>; rel=\"next\"",
            server.uri()
        );
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/7/files"))
            .and(query_param("page", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "filename": "b.rs", "status": "added", "additions": 1, "deletions": 0 }
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/7/files"))
            .and(query_param("per_page", "100"))
            .and(wiremock::matchers::header_exists("if-none-match"))
            .respond_with(ResponseTemplate::new(304))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widgets/pulls/7/files"))
            .and(query_param("per_page", "100"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"p1\"")
                    .insert_header("link", next.as_str())
                    .set_body_json(json!([
                        { "filename": "a.rs", "status": "modified", "additions": 1, "deletions": 1 }
                    ])),
            )
            .up_to_n_times(1)
            .with_priority(5)
            .mount(&server)
            .await;
        let client = gh(&server);
        assert_eq!(client.get_files(&pr7()).await.unwrap().len(), 2);
        assert_eq!(client.get_files(&pr7()).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn foreign_next_link_is_not_followed() {
        let server = MockServer::start().await;
        let base = server.uri();
        let port = base.rsplit(':').next().unwrap().to_string();
        for link in [
            "http://evil.example/x?page=2".to_string(),
            // String-prefix look-alikes of the API base.
            format!("{base}.evil.example/x?page=2"),
            format!("{base}@evil.example/x?page=2"),
            format!("https://127.0.0.1:{port}/x?page=2"),
        ] {
            server.reset().await;
            Mock::given(method("GET"))
                .and(path("/repos/acme/widgets/pulls/7/files"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("link", format!("<{link}>; rel=\"next\"").as_str())
                        .set_body_json(json!([
                            { "filename": "a.rs", "status": "modified", "additions": 1, "deletions": 1 }
                        ])),
                )
                .mount(&server)
                .await;
            let files = gh(&server).get_files(&pr7()).await.unwrap();
            assert_eq!(files.len(), 1, "{link}");
            assert_eq!(server.received_requests().await.unwrap().len(), 1, "{link}");
        }
    }

    #[test]
    fn same_api_requires_matching_origin_and_path() {
        let api = reqwest::Url::parse("https://ghe.example/api/v3").unwrap();
        let ok = |s: &str| same_api(&api, s);
        assert!(ok("https://ghe.example/api/v3/repos/a/b/pulls?page=2"));
        assert!(!ok("https://ghe.example/api/v30/repos?page=2"));
        assert!(!ok("https://ghe.example/other?page=2"));
        assert!(!ok("http://ghe.example/api/v3/repos?page=2"));
        assert!(!ok("https://ghe.example:8443/api/v3/repos?page=2"));
        assert!(!ok("https://ghe.example.evil.com/api/v3/repos?page=2"));
        assert!(!ok("https://ghe.example@evil.example/api/v3/repos?page=2"));
        assert!(!ok("not a url"));
    }

    fn payload() -> clusia_core::ReviewPayload {
        clusia_core::ReviewPayload {
            commit_id: "h1".into(),
            event: "REQUEST_CHANGES".into(),
            body: "Please fix".into(),
            comments: vec![clusia_core::ReviewComment {
                path: "a.rs".into(),
                body: "nit".into(),
                line: 3,
                side: "RIGHT".into(),
                start_line: None,
                start_side: None,
            }],
        }
    }

    #[tokio::test]
    async fn create_review_posts_the_payload() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/acme/widgets/pulls/7/reviews"))
            .and(h("authorization", "Bearer tok123"))
            .and(wiremock::matchers::body_json(json!({
                "commit_id": "h1", "event": "REQUEST_CHANGES", "body": "Please fix",
                "comments": [{ "path": "a.rs", "body": "nit", "line": 3, "side": "RIGHT" }]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 42, "html_url": "https://github.com/acme/widgets/pull/7#pullrequestreview-42" })))
            .expect(1)
            .mount(&server)
            .await;
        let published = gh(&server).create_review(&pr7(), &payload()).await.unwrap();
        assert_eq!(
            published,
            PublishedReview {
                id: 42,
                url: "https://github.com/acme/widgets/pull/7#pullrequestreview-42".into()
            }
        );
    }

    #[tokio::test]
    async fn validation_errors_carry_github_details() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(422).set_body_json(json!({
                "message": "Unprocessable Entity",
                "errors": ["Line could not be resolved", { "message": "pull_request_review_thread.line must be part of the diff" }]
            })))
            .mount(&server)
            .await;
        match gh(&server).create_review(&pr7(), &payload()).await {
            Err(ProviderError::Http {
                status: 422,
                message,
            }) => {
                assert!(message.starts_with("Unprocessable Entity: "), "{message}");
                assert!(
                    message.contains("Line could not be resolved; pull_request_review_thread.line"),
                    "{message}"
                );
            }
            other => panic!("expected 422, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn close_pr_patches_state() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/repos/acme/widgets/pulls/7"))
            .and(wiremock::matchers::body_json(json!({ "state": "closed" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "state": "closed" })))
            .expect(1)
            .mount(&server)
            .await;
        gh(&server).close_pr(&pr7()).await.unwrap();
    }

    #[tokio::test]
    async fn writes_map_unauthorized() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        assert_eq!(
            gh(&server).close_pr(&pr7()).await,
            Err(ProviderError::Unauthorized)
        );
    }
}
