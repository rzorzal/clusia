//! What a review's agent session remembers, and the context file the agent reads first.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::{DraftItem, DraftKind, PrDetail, Review};

/// Kept between turns and daemon restarts, in `Paths::agent_state`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentState {
    /// Suggestion ids the human dismissed; they are never shown again.
    pub dismissed: BTreeSet<String>,
    /// Suggestion ids the human accepted into the draft.
    pub accepted: BTreeSet<String>,
    /// The head commit the last summary was written for.
    pub last_summary_head: Option<String>,
}

impl AgentState {
    /// Whether the suggestion was already answered, either way.
    pub fn is_settled(&self, id: &str) -> bool {
        self.dismissed.contains(id) || self.accepted.contains(id)
    }

    /// Records a dismissal; `false` when it was already recorded.
    pub fn dismiss(&mut self, id: &str) -> bool {
        self.dismissed.insert(id.to_string())
    }

    /// Records an acceptance; `false` when it was already recorded.
    pub fn accept(&mut self, id: &str) -> bool {
        self.accepted.insert(id.to_string())
    }
}

const BODY_PREVIEW_CHARS: usize = 200;

/// The text of `.clusia/review.md`: the pull request, the running summary, the draft so far
/// and which suggestions were already answered. Pure, so the same input gives the same file.
pub fn review_md(
    review: &Review,
    detail: &PrDetail,
    summary: Option<&str>,
    state: &AgentState,
) -> String {
    let mut out = String::new();
    out.push_str("# Review context\n\n");
    out.push_str(
        "Written by Clúsia for the agent that assists this review. It is rewritten whenever \
         the review changes: read it, do not edit it.\n\n",
    );
    out.push_str("## Pull request\n\n");
    let _ = writeln!(out, "- Repository: {}", review.pr.slug());
    let _ = writeln!(
        out,
        "- Pull request: #{} {}",
        review.pr.number, review.title
    );
    let _ = writeln!(out, "- Author: @{}", detail.summary.author);
    let _ = writeln!(
        out,
        "- Branches: {} -> {}",
        detail.head_ref, detail.base_ref
    );
    let _ = writeln!(out, "- Head commit: {}", short(&review.head_sha));
    let _ = writeln!(out, "- Base commit: {}", short(&review.base_sha));
    let _ = writeln!(
        out,
        "- Size: +{} -{} across {} files",
        detail.additions, detail.deletions, detail.changed_files
    );
    let status = if detail.merged {
        "merged"
    } else if detail.closed {
        "closed"
    } else {
        "open"
    };
    let _ = writeln!(out, "- State: {status}");
    let _ = writeln!(out, "- URL: {}", detail.summary.url);

    out.push_str("\n## Summary\n\n");
    match summary.map(str::trim).filter(|s| !s.is_empty()) {
        Some(text) => {
            out.push_str(text);
            out.push('\n');
        }
        None => out.push_str("No summary yet.\n"),
    }

    out.push_str("\n## Draft so far\n\n");
    let items: Vec<&DraftItem> = review.draft.items.iter().filter(|i| i.accepted).collect();
    if items.is_empty() {
        out.push_str("Nothing yet.\n");
    }
    for item in items {
        let _ = writeln!(out, "- {}", draft_line(item));
    }

    out.push_str("\n## Suggestions already answered\n\n");
    if state.accepted.is_empty() && state.dismissed.is_empty() {
        out.push_str("None.\n");
    } else {
        out.push_str("Do not suggest these again.\n\n");
        for id in &state.accepted {
            let _ = writeln!(out, "- {id} (accepted)");
        }
        for id in &state.dismissed {
            let _ = writeln!(out, "- {id} (dismissed)");
        }
    }
    out
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

fn draft_line(item: &DraftItem) -> String {
    let place = |path: &str, line: Option<u32>| match line {
        Some(line) => format!("{path}:{line}"),
        None => path.to_string(),
    };
    match item.kind {
        DraftKind::LineComment => {
            let at = item.anchor.as_ref().map_or_else(String::new, |a| {
                let range = match a.start_line {
                    Some(start) if start != a.line => format!("{}-{}", start, a.line),
                    _ => a.line.to_string(),
                };
                format!("{}:{range}", a.path)
            });
            format!("`{at}`: {}", preview(&item.body))
        }
        DraftKind::General => format!("General note: {}", preview(&item.body)),
        DraftKind::Reply => {
            let (author, at) = thread_context(item, place);
            format!("Reply to @{author}{at}: {}", preview(&item.body))
        }
        DraftKind::Resolve => {
            let (author, at) = thread_context(item, place);
            format!("Resolve the thread by @{author}{at}")
        }
    }
}

fn thread_context(
    item: &DraftItem,
    place: impl Fn(&str, Option<u32>) -> String,
) -> (String, String) {
    match &item.thread {
        Some(t) => (
            t.author.clone(),
            t.path
                .as_deref()
                .map_or_else(String::new, |p| format!(" ({})", place(p, t.line))),
        ),
        None => (String::from("someone"), String::new()),
    }
}

/// The first line of `body`, cut to a readable length.
fn preview(body: &str) -> String {
    let line = body.lines().next().unwrap_or("").trim();
    if line.chars().count() > BODY_PREVIEW_CHARS {
        let cut: String = line.chars().take(BODY_PREVIEW_CHARS).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Anchor, Draft, ItemStatus, Origin, PrRef, PrSummary, Side, ThreadRef};

    fn pr() -> PrRef {
        PrRef::new("acme", "widgets", 7).unwrap()
    }

    fn detail() -> PrDetail {
        PrDetail {
            summary: PrSummary {
                pr: pr(),
                title: "feat: auth refresh".into(),
                author: "octo".into(),
                url: "https://github.com/acme/widgets/pull/7".into(),
                draft: false,
                updated_at: "2026-10-08T12:00:00Z".into(),
                comments: 2,
            },
            base_ref: "main".into(),
            head_ref: "octo:auth-refresh".into(),
            base_sha: "b".repeat(40),
            head_sha: "a1b2c3d4e5f6a7b8c9d0a1b2c3d4e5f6a7b8c9d0".into(),
            additions: 120,
            deletions: 34,
            changed_files: 7,
            clone_url: "https://github.com/acme/widgets.git".into(),
            closed: false,
            merged: false,
        }
    }

    fn review() -> Review {
        Review::new(
            pr(),
            "feat: auth refresh".into(),
            "b".repeat(40),
            "a1b2c3d4e5f6a7b8c9d0a1b2c3d4e5f6a7b8c9d0".into(),
            100,
        )
    }

    fn item(kind: DraftKind, body: &str) -> DraftItem {
        DraftItem {
            id: "i1".into(),
            kind,
            origin: Origin::Human,
            anchor: None,
            thread: None,
            body: body.into(),
            status: ItemStatus::Ok,
            accepted: true,
            created_at: 100,
        }
    }

    fn anchor(line: u32, start: Option<u32>) -> Anchor {
        Anchor {
            path: "src/auth/store.rs".into(),
            line,
            start_line: start,
            side: Side::Right,
            commit: "a".repeat(40),
        }
    }

    #[test]
    fn state_defaults_are_empty_and_settle_both_ways() {
        let mut s = AgentState::default();
        assert!(s.dismissed.is_empty() && s.accepted.is_empty());
        assert_eq!(s.last_summary_head, None);
        assert!(!s.is_settled("sug-1"));
        assert!(s.dismiss("sug-1"));
        assert!(!s.dismiss("sug-1"), "a second dismissal changes nothing");
        assert!(s.accept("sug-2"));
        assert!(s.is_settled("sug-1") && s.is_settled("sug-2"));
        assert!(!s.is_settled("sug-3"));
    }

    #[test]
    fn state_reads_a_partial_file() {
        let s: AgentState = serde_json::from_str(r#"{"dismissed":["sug-1"]}"#).unwrap();
        assert!(s.dismissed.contains("sug-1"));
        assert!(s.accepted.is_empty());
        assert_eq!(
            serde_json::to_string(&AgentState::default()).unwrap(),
            r#"{"dismissed":[],"accepted":[],"last_summary_head":null}"#
        );
    }

    #[test]
    fn review_md_describes_the_pull_request() {
        let md = review_md(&review(), &detail(), None, &AgentState::default());
        assert_eq!(
            md,
            "# Review context\n\n\
             Written by Clúsia for the agent that assists this review. It is rewritten whenever \
             the review changes: read it, do not edit it.\n\n\
             ## Pull request\n\n\
             - Repository: acme/widgets\n\
             - Pull request: #7 feat: auth refresh\n\
             - Author: @octo\n\
             - Branches: octo:auth-refresh -> main\n\
             - Head commit: a1b2c3d4e5f6\n\
             - Base commit: bbbbbbbbbbbb\n\
             - Size: +120 -34 across 7 files\n\
             - State: open\n\
             - URL: https://github.com/acme/widgets/pull/7\n\n\
             ## Summary\n\n\
             No summary yet.\n\n\
             ## Draft so far\n\n\
             Nothing yet.\n\n\
             ## Suggestions already answered\n\n\
             None.\n"
        );
    }

    #[test]
    fn review_md_is_deterministic() {
        let state = AgentState::default();
        assert_eq!(
            review_md(&review(), &detail(), Some("A summary."), &state),
            review_md(&review(), &detail(), Some("A summary."), &state)
        );
    }

    #[test]
    fn review_md_shows_state_and_the_summary() {
        let mut d = detail();
        d.merged = true;
        let md = review_md(
            &review(),
            &d,
            Some("  Refreshes tokens.\n\nTouches 7 files.  "),
            &{
                let mut s = AgentState::default();
                s.dismiss("sug-bbb");
                s.accept("sug-aaa");
                s
            },
        );
        assert!(md.contains("- State: merged\n"));
        assert!(md.contains("## Summary\n\nRefreshes tokens.\n\nTouches 7 files.\n"));
        assert!(md.contains(
            "Do not suggest these again.\n\n- sug-aaa (accepted)\n- sug-bbb (dismissed)\n"
        ));
        d.merged = false;
        d.closed = true;
        assert!(
            review_md(&review(), &d, None, &AgentState::default()).contains("- State: closed\n")
        );
    }

    #[test]
    fn review_md_lists_only_accepted_draft_items() {
        let mut r = review();
        let mut line = item(DraftKind::LineComment, "Check the expiry.\nSecond line.");
        line.anchor = Some(anchor(44, Some(40)));
        let mut single = item(DraftKind::LineComment, "Typo");
        single.anchor = Some(anchor(9, None));
        let general = item(DraftKind::General, "Looks good overall.");
        let mut reply = item(DraftKind::Reply, "Agreed.");
        reply.thread = Some(ThreadRef {
            id: "PRRT_1".into(),
            author: "mona".into(),
            path: Some("src/lib.rs".into()),
            line: Some(3),
        });
        let mut resolve = item(DraftKind::Resolve, "");
        resolve.thread = Some(ThreadRef {
            id: "PRRT_2".into(),
            author: "hubot".into(),
            path: None,
            line: None,
        });
        let mut waiting = item(DraftKind::LineComment, "Not accepted yet");
        waiting.origin = Origin::Agent;
        waiting.accepted = false;
        waiting.anchor = Some(anchor(1, None));
        r.draft = Draft {
            items: vec![line, single, general, reply, resolve, waiting],
            next_id: 6,
        };
        let md = review_md(&r, &detail(), None, &AgentState::default());
        let draft = md
            .split("## Draft so far\n\n")
            .nth(1)
            .and_then(|s| s.split("\n## ").next())
            .unwrap();
        assert_eq!(
            draft,
            "- `src/auth/store.rs:40-44`: Check the expiry.\n\
             - `src/auth/store.rs:9`: Typo\n\
             - General note: Looks good overall.\n\
             - Reply to @mona (src/lib.rs:3): Agreed.\n\
             - Resolve the thread by @hubot\n"
        );
    }

    #[test]
    fn long_bodies_are_cut() {
        let mut r = review();
        let mut long = item(DraftKind::General, &"x".repeat(300));
        long.anchor = None;
        r.draft.items.push(long);
        let md = review_md(&r, &detail(), None, &AgentState::default());
        let line = md
            .lines()
            .find(|l| l.starts_with("- General note"))
            .unwrap();
        assert_eq!(line.chars().count(), "- General note: ".len() + 200 + 1);
        assert!(line.ends_with('…'));
    }
}
