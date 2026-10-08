//! "What's new" since the user last looked (spec §6.2), and noticing saved reviews that went stale (§6.6).

use std::collections::BTreeMap;
use std::path::PathBuf;

use clusia_core::time::parse_rfc3339;
use clusia_core::{
    Activity, ActivityKind, CommitInfo, PrConversation, PrRef, PrState, ReviewEvent, ReviewState,
};
use clusia_git::{pin_commit, reviewed_ref};
use clusia_protocol::{Event, NewsItem, NewsKind, Outcome, Reply, SyncState, topics};
use clusia_store::{list_reviews, read_activity};

use crate::handlers::{no_token, provider_error};
use crate::reviews::{announce, files_for, load_existing, load_stored, lock, record, save};
use crate::state::Shared;
use crate::sync::{github_client, now_unix};
use crate::{relocate, worktrees};

pub(crate) struct NewsInput {
    pub since: i64,
    pub now: i64,
    pub viewer: Option<String>,
    pub commits: Vec<CommitInfo>,
    pub conversation: PrConversation,
    pub checks_label: Option<String>,
    pub last_checks: Option<String>,
    pub local: Vec<Activity>,
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

pub(crate) fn news_items(input: &NewsInput) -> Vec<NewsItem> {
    let mut items = Vec::new();
    let after = |ts: &str| parse_rfc3339(ts).filter(|t| *t > input.since);
    let is_viewer = |who: &str| {
        input
            .viewer
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case(who))
    };

    let mut authors: Vec<String> = Vec::new();
    let mut latest = None;
    let mut count = 0;
    for c in &input.commits {
        if let Some(at) = after(&c.date) {
            count += 1;
            latest = latest.max(Some(at));
            if !authors.contains(&c.author) {
                authors.push(c.author.clone());
            }
        }
    }
    if let Some(at) = latest {
        items.push(NewsItem {
            kind: NewsKind::Commits,
            source: "GitHub".into(),
            who: Some(authors.join(", ")),
            at,
            summary: plural(count, "new commit"),
            url: None,
        });
    }

    // author → (count, latest at, url of latest)
    let mut by_author: BTreeMap<String, (usize, i64, String)> = BTreeMap::new();
    let comments = input
        .conversation
        .threads
        .iter()
        .map(|t| (&t.author, &t.created_at, &t.url))
        .chain(
            input
                .conversation
                .comments
                .iter()
                .map(|c| (&c.author, &c.created_at, &c.url)),
        );
    for (author, created, url) in comments {
        if is_viewer(author) {
            continue;
        }
        if let Some(at) = after(created) {
            let entry = by_author
                .entry(author.clone())
                .or_insert((0, i64::MIN, String::new()));
            entry.0 += 1;
            if at >= entry.1 {
                entry.1 = at;
                entry.2 = url.clone();
            }
        }
    }
    for (author, (n, at, url)) in by_author {
        items.push(NewsItem {
            kind: NewsKind::Comment,
            source: "GitHub".into(),
            who: Some(author),
            at,
            summary: plural(n, "comment"),
            url: Some(url),
        });
    }

    for r in &input.conversation.reviews {
        if is_viewer(&r.author) {
            continue;
        }
        if let Some(at) = r.submitted_at.as_deref().and_then(after) {
            let summary = match r.state.as_str() {
                "APPROVED" => "approved".to_string(),
                "CHANGES_REQUESTED" => "requested changes".to_string(),
                "COMMENTED" => "commented".to_string(),
                other => other.to_lowercase(),
            };
            items.push(NewsItem {
                kind: NewsKind::Review,
                source: "GitHub".into(),
                who: Some(r.author.clone()),
                at,
                summary,
                url: Some(r.url.clone()),
            });
        }
    }

    if let (Some(prev), Some(cur)) = (&input.last_checks, &input.checks_label)
        && prev != cur
        && cur != "none"
    {
        items.push(NewsItem {
            kind: NewsKind::Checks,
            source: "GitHub Actions".into(),
            who: None,
            at: input.now,
            summary: format!("CI {cur}"),
            url: None,
        });
    }

    for a in input.local.iter().filter(|a| a.ts > input.since) {
        match a.kind {
            ActivityKind::ItemAdded if !a.client.is_empty() => items.push(NewsItem {
                kind: NewsKind::Local,
                source: format!("You via {}", a.client),
                who: None,
                at: a.ts,
                summary: "Item added".into(),
                url: None,
            }),
            ActivityKind::ReviewOutdated => items.push(NewsItem {
                kind: NewsKind::Moved,
                source: "Clúsia".into(),
                who: None,
                at: a.ts,
                summary: format!("Draft re-anchored: {}", a.note.as_deref().unwrap_or("")),
                url: None,
            }),
            _ => {}
        }
    }
    items.sort_by_key(|i| i.at);
    items
}

pub(crate) async fn whats_new(shared: &Shared, pr: &PrRef) -> Outcome {
    let review = match load_existing(shared, pr) {
        Ok(r) => r,
        Err(out) => return out,
    };
    let Some(since) = review.last_seen_at else {
        return Outcome::Ok(Reply::WhatsNew(Vec::new()));
    };
    let gh = match github_client(shared).await {
        Ok(Some(gh)) => gh,
        Ok(None) => return no_token(),
        Err(e) => return provider_error(e),
    };
    let commits = match gh.get_commits(pr).await {
        Ok(c) => c,
        Err(e) => return provider_error(e),
    };
    let conversation = match gh.get_conversation(pr).await {
        Ok(c) => c,
        Err(e) => return provider_error(e),
    };
    let checks_label = gh
        .get_checks(pr, &review.head_sha)
        .await
        .ok()
        .map(|c| c.label().to_string());
    let viewer = gh.viewer().await.ok().map(|v| v.login);
    let local = match read_activity(&shared.paths) {
        Ok((all, _)) => all.into_iter().filter(|a| &a.pr == pr).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "cannot read the activity log");
            Vec::new()
        }
    };
    let input = NewsInput {
        since,
        now: now_unix(),
        viewer,
        commits,
        conversation,
        checks_label,
        last_checks: review.last_checks.clone(),
        local,
    };
    Outcome::Ok(Reply::WhatsNew(news_items(&input)))
}

pub(crate) async fn mark_seen(shared: &Shared, pr: &PrRef) -> Outcome {
    let gh = github_client(shared).await.unwrap_or_default();
    let _guard = lock(shared, pr).await;
    let mut review = match load_existing(shared, pr) {
        Ok(r) => r,
        Err(out) => return out,
    };
    // Read under the lock, so the label belongs to the head we store it for.
    let label = match gh {
        Some(gh) => gh
            .get_checks(pr, &review.head_sha)
            .await
            .ok()
            .map(|c| c.label().to_string()),
        None => None,
    };
    review.last_seen_at = Some(now_unix());
    if label.is_some() {
        review.last_checks = label;
    }
    if let Err(out) = save(shared, &review) {
        return out;
    }
    Outcome::Ok(Reply::Ack)
}

const BACKOFF_FIRST_SECS: u64 = 60;
const BACKOFF_MAX_SECS: u64 = 3600;

/// Spaces out the attempts to update the worktree of a saved review that keeps failing: a
/// minute, then double each time, never more than an hour.
#[derive(Debug, Default)]
pub(crate) struct Backoff {
    failures: u32,
    retry_at: u64,
}

impl Backoff {
    pub fn due(&self, now: u64) -> bool {
        now >= self.retry_at
    }

    pub fn failed(&mut self, now: u64) {
        self.failures = self.failures.saturating_add(1);
        let delay = BACKOFF_FIRST_SECS
            .saturating_mul(1 << (self.failures - 1).min(6))
            .min(BACKOFF_MAX_SECS);
        self.retry_at = now + delay;
    }
}

/// Remembers that a saved review's pull request is closed or merged, so later syncs leave it
/// alone.
async fn note_pr_ended(shared: &Shared, pr: &PrRef, state: PrState) {
    let _guard = lock(shared, pr).await;
    let Ok(Some(mut review)) = load_stored(shared, pr) else {
        return;
    };
    if review.pr_state == state {
        return;
    }
    review.pr_state = state;
    if save(shared, &review).is_err() {
        tracing::warn!(pr = %pr, "cannot record that a pull request ended");
    }
}

/// Relocates saved reviews whose pull request moved (spec §6.6); deterministic, no agent involved.
pub(crate) async fn check_saved_reviews(shared: &Shared) {
    // The sync status says whether GitHub is reachable; polling every PR while offline only logs noise.
    if shared.sync.read().await.state != SyncState::Online {
        return;
    }
    let Ok(Some(gh)) = github_client(shared).await else {
        return;
    };
    let reviews = match list_reviews(&shared.paths) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "cannot list reviews");
            return;
        }
    };
    for stored in reviews {
        if !matches!(
            stored.state,
            ReviewState::Saved | ReviewState::Revalidated | ReviewState::Outdated
        ) {
            continue;
        }
        // A closed or merged pull request cannot move any more; opening the review looks again.
        if !stored.pr_state.is_open() {
            continue;
        }
        let pr = stored.pr.clone();
        if !shared.checkout_due(&pr) {
            continue;
        }
        let detail = match gh.get_pr(&pr).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, pr = %pr, "cannot check a saved review");
                continue;
            }
        };
        if detail.closed || detail.merged {
            note_pr_ended(shared, &pr, PrState::of(detail.closed, detail.merged)).await;
            continue;
        }
        if detail.head_sha == stored.head_sha && detail.base_sha == stored.base_sha {
            continue;
        }
        let _guard = lock(shared, &pr).await;
        let mut review = match load_stored(shared, &pr) {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(_) => continue, // already logged
        };
        // The review may have changed while we waited for the lock.
        if !matches!(
            review.state,
            ReviewState::Saved | ReviewState::Revalidated | ReviewState::Outdated
        ) || (detail.head_sha == review.head_sha && detail.base_sha == review.base_sha)
        {
            continue;
        }
        shared.touch(&pr);
        let checkout = {
            let _serialized = shared.worktree_lock.lock().await;
            worktrees::checkout(shared, &pr, &detail).await
        };
        let (info, remote) = match checkout {
            Ok(x) => x,
            Err(e) => {
                tracing::warn!(error = %e, pr = %pr, "cannot update the worktree of a saved review");
                shared.checkout_failed(&pr);
                continue;
            }
        };
        shared.checkout_worked(&pr);
        let repo = PathBuf::from(&info.clone);
        relocate::refresh_base(&repo, &remote, &pr, &detail.base_ref).await;
        let files = match files_for(shared, &pr, &detail.head_sha).await {
            Ok(f) => Some(f),
            Err(_) => {
                tracing::warn!(pr = %pr, "cannot load the files of a saved review's pull request");
                None
            }
        };
        let head_changed = review.head_sha != detail.head_sha;
        let old_head = review.head_sha.clone();
        let report = relocate::relocate_review(
            &mut review,
            &detail,
            &repo,
            files.as_deref().map(Vec::as_slice),
        )
        .await;
        if !head_changed && report.moved + report.obsolete == 0 {
            // Only the base moved ahead: nothing for the reviewer to re-check.
            if save(shared, &review).is_err() {
                tracing::warn!(pr = %pr, "cannot save a review's new base");
            }
            continue;
        }
        if let Err(e) = review.apply(ReviewEvent::NewHead, now_unix()) {
            tracing::warn!(error = %e, pr = %pr, "cannot mark a saved review outdated");
            continue;
        }
        if save(shared, &review).is_err() {
            tracing::warn!(pr = %pr, "cannot save a re-anchored review");
            continue;
        }
        if let Err(e) = pin_commit(&repo, &reviewed_ref(pr.number), &review.head_sha).await {
            tracing::warn!(error = %e, pr = %pr, "cannot pin the new head of a saved review");
        }
        let note = format!("{} moved, {} obsolete", report.moved, report.obsolete);
        record(
            shared,
            ActivityKind::ReviewOutdated,
            &pr,
            "",
            None,
            Some(note),
        );
        shared.publish(
            topics::REVIEWS,
            Event::ReviewOutdated {
                pr: pr.clone(),
                moved: report.moved,
                obsolete: report.obsolete,
            },
        );
        announce(shared, &review);
        // A base that moved alone may obsolete comments, but brings no commits to re-read.
        if head_changed {
            crate::notifications::review_outdated(shared, &review, &old_head).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::{
        ActivityKind, CommitInfo, IssueComment, PrConversation, ReviewInfo, ThreadComment,
    };

    const SINCE: i64 = 946_684_800; // 2000-01-01T00:00:00Z

    fn input() -> NewsInput {
        NewsInput {
            since: SINCE,
            now: SINCE + 7200,
            viewer: Some("me".into()),
            commits: vec![
                CommitInfo {
                    sha: "c0".into(),
                    author: "old".into(),
                    message: "m".into(),
                    date: "1999-12-31T23:00:00Z".into(),
                },
                CommitInfo {
                    sha: "c1".into(),
                    author: "maria".into(),
                    message: "m".into(),
                    date: "2000-01-01T00:10:00Z".into(),
                },
                CommitInfo {
                    sha: "c2".into(),
                    author: "joao".into(),
                    message: "m".into(),
                    date: "2000-01-01T00:20:00Z".into(),
                },
            ],
            conversation: PrConversation {
                threads: vec![
                    ThreadComment {
                        id: 1,
                        in_reply_to: None,
                        path: "a".into(),
                        line: Some(1),
                        side: None,
                        author: "joao".into(),
                        body: "b".into(),
                        created_at: "2000-01-01T00:30:00Z".into(),
                        url: "t1".into(),
                    },
                    ThreadComment {
                        id: 2,
                        in_reply_to: Some(1),
                        path: "a".into(),
                        line: None,
                        side: None,
                        author: "me".into(),
                        body: "b".into(),
                        created_at: "2000-01-01T00:31:00Z".into(),
                        url: "t2".into(),
                    },
                ],
                comments: vec![IssueComment {
                    id: 3,
                    author: "joao".into(),
                    body: "b".into(),
                    created_at: "2000-01-01T00:40:00Z".into(),
                    url: "c3".into(),
                }],
                reviews: vec![ReviewInfo {
                    id: 4,
                    author: "ana".into(),
                    state: "APPROVED".into(),
                    body: String::new(),
                    submitted_at: Some("2000-01-01T00:50:00Z".into()),
                    url: "r4".into(),
                    commit_id: None,
                }],
                review_threads: vec![],
            },
            checks_label: Some("passed".into()),
            last_checks: Some("pending".into()),
            local: vec![
                Activity {
                    ts: SINCE + 3600,
                    kind: ActivityKind::ItemAdded,
                    pr: "acme/widgets#7".parse().unwrap(),
                    client: "clusia".into(),
                    url: None,
                    note: None,
                },
                Activity {
                    ts: SINCE + 3700,
                    kind: ActivityKind::ReviewOutdated,
                    pr: "acme/widgets#7".parse().unwrap(),
                    client: String::new(),
                    url: None,
                    note: Some("1 moved, 0 obsolete".into()),
                },
                Activity {
                    ts: SINCE - 10,
                    kind: ActivityKind::ItemAdded,
                    pr: "acme/widgets#7".parse().unwrap(),
                    client: "clusia".into(),
                    url: None,
                    note: None,
                },
            ],
        }
    }

    #[test]
    fn builds_items_in_time_order() {
        let items = news_items(&input());
        let summary: Vec<(NewsKind, &str, Option<&str>)> = items
            .iter()
            .map(|i| (i.kind, i.summary.as_str(), i.who.as_deref()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (NewsKind::Commits, "2 new commits", Some("maria, joao")),
                (NewsKind::Comment, "2 comments", Some("joao")),
                (NewsKind::Review, "approved", Some("ana")),
                (NewsKind::Local, "Item added", None),
                (
                    NewsKind::Moved,
                    "Draft re-anchored: 1 moved, 0 obsolete",
                    None
                ),
                (NewsKind::Checks, "CI passed", None),
            ]
        );
        assert_eq!(items[3].source, "You via clusia");
        assert_eq!(items[5].source, "GitHub Actions");
        assert_eq!(items[1].url.as_deref(), Some("c3"));
    }

    #[test]
    fn nothing_new_is_empty() {
        let mut i = input();
        i.since = SINCE + 100_000;
        i.last_checks = Some("passed".into());
        assert!(news_items(&i).is_empty());
    }

    #[test]
    fn backoff_doubles_from_a_minute_up_to_an_hour() {
        let mut b = Backoff::default();
        assert!(b.due(0), "a review that never failed is always due");
        let mut now = 100;
        let mut waits = Vec::new();
        for _ in 0..9 {
            b.failed(now);
            assert!(!b.due(now), "not due right after a failure");
            waits.push(b.retry_at - now);
            now = b.retry_at;
            assert!(b.due(now), "due again when the wait is over");
        }
        assert_eq!(waits, [60, 120, 240, 480, 960, 1920, 3600, 3600, 3600]);
    }
}
