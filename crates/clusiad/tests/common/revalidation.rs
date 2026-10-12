#![allow(dead_code)]

//! The saved review the revalidation tests start from, and what they ask about it.

use std::time::Duration;

use clusia_core::revalidation::RoundState;
use clusia_core::{DraftKind, Review, Side};
use clusia_harness::testkit::{Script, Turn, revalidation_block};
use clusia_protocol::{AnchorInput, Client, Command, Event, Reply, topics};
use serde_json::json;

use super::agent::fake_program;
use super::git_fixture::advance_pr;
use super::github_mock::{PrMock, mount_pr};
use super::review_world::{World, open, pr7};

pub const FILE: &str = "feature.txt";

/// The ids of the five comments of the saved review the tests start from.
pub struct Ids {
    /// A line comment on `two`.
    pub a: String,
    /// A line comment on `three`.
    pub b: String,
    /// A line comment on `one`.
    pub q: String,
    /// A general note.
    pub n: String,
    /// A second general note.
    pub g: String,
}

pub async fn add(c: &mut Client, kind: DraftKind, line: Option<u32>, body: &str) -> String {
    let anchor = line.map(|line| AnchorInput {
        path: FILE.into(),
        line,
        start_line: None,
        side: Side::Right,
    });
    match c
        .request(Command::AddDraftItem {
            pr: pr7(),
            thread: None,
            kind,
            anchor,
            body: body.into(),
        })
        .await
        .unwrap()
    {
        Reply::DraftItem(item) => item.id,
        other => panic!("expected the item, got {other:?}"),
    }
}

/// Opens acme/widgets#7, writes five comments and saves the review.
pub async fn saved(w: &World) -> (Client, Ids) {
    let mut c = w.daemon.client().await;
    open(&mut c).await;
    let ids = Ids {
        a: add(&mut c, DraftKind::LineComment, Some(2), "Rename this.").await,
        b: add(&mut c, DraftKind::LineComment, Some(3), "Drop this line.").await,
        q: add(&mut c, DraftKind::LineComment, Some(1), "Why one?").await,
        n: add(&mut c, DraftKind::General, None, "Looks good overall.").await,
        g: add(&mut c, DraftKind::General, None, "Thanks for the tests.").await,
    };
    c.request(Command::CloseReview { pr: pr7() }).await.unwrap();
    (c, ids)
}

pub async fn mount_search(w: &World) {
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/search/issues"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_json(json!({ "total_count": 0, "items": [] })),
        )
        .mount(&w.server)
        .await;
}

/// Pushes a commit that puts `extra` above the file's old lines, serves it, and lets the daemon
/// poll once. Returns the new head.
pub async fn push(w: &World, c: &mut Client, extra: &str) -> String {
    push_content(w, c, &format!("{extra}one\ntwo\nthree\n"), None).await
}

/// Pushes a commit that makes the file `content`, serves it with `patch` as GitHub's patch of the
/// file (the one that adds all of `content` when `None`), and lets the daemon poll once.
pub async fn push_content(w: &World, c: &mut Client, content: &str, patch: Option<&str>) -> String {
    let head = serve_head(w, content, patch).await;
    c.request(Command::SyncNow).await.unwrap();
    head
}

/// Pushes a commit that makes the file `content` and serves it, without letting the daemon poll.
pub async fn serve_head(w: &World, content: &str, patch: Option<&str>) -> String {
    let head = advance_pr(w.tmp.path(), 7, FILE, content);
    w.server.reset().await;
    let mut mock = PrMock::new(&head, &w.base, &w.origin).adding_feature(content);
    if let Some(patch) = patch {
        mock.files = json!([{
            "filename": FILE, "status": "added", "additions": 1, "deletions": 0, "patch": patch
        }]);
    }
    mount_pr(&w.server, &mock).await;
    mount_search(w).await;
    head
}

/// Runs the round now and waits until it is done.
pub async fn run_round(w: &World, c: &mut Client) {
    let mut watcher = reviews_watcher(w).await;
    c.request(Command::Revalidate { pr: pr7() }).await.unwrap();
    super::agent::until(&mut watcher, round_is(is_done)).await;
}

/// The saved review of [`saved`], pushed to a new head with a line on top, and revalidated with
/// the answer of [`answer`].
pub async fn revalidated(w: &World) -> (Client, Ids) {
    let (mut c, ids) = saved(w).await;
    use_script(w, Script::one(answer(&ids))).await;
    push(w, &mut c, "zero\n").await;
    run_round(w, &mut c).await;
    (c, ids)
}

pub async fn review_of(c: &mut Client) -> Review {
    match c.request(Command::GetReview { pr: pr7() }).await.unwrap() {
        Reply::ReviewFile(review) => *review,
        other => panic!("expected the review file, got {other:?}"),
    }
}

pub async fn reviews_watcher(w: &World) -> Client {
    let mut c = w.daemon.client().await;
    c.request(Command::Subscribe {
        topics: vec![topics::REVIEWS.into()],
    })
    .await
    .unwrap();
    c
}

pub async fn agent_watcher(w: &World) -> Client {
    let mut c = w.daemon.client().await;
    c.request(Command::Subscribe {
        topics: vec![topics::AGENT.into()],
    })
    .await
    .unwrap();
    c
}

pub fn round_is(wanted: fn(&RoundState) -> bool) -> impl Fn(&Event) -> bool {
    move |e| {
        matches!(
            e,
            Event::RevalidationChanged { revalidation: Some(r), .. } if wanted(&r.state)
        )
    }
}

pub fn is_done(state: &RoundState) -> bool {
    *state == RoundState::Done
}

pub fn is_running(state: &RoundState) -> bool {
    matches!(state, RoundState::Running { .. })
}

pub fn is_failed(state: &RoundState) -> bool {
    matches!(state, RoundState::Failed { .. })
}

/// The titles in the inbox that tell the user the pull request moved on or was revalidated.
pub async fn round_titles(c: &mut Client) -> Vec<String> {
    match c.request(Command::GetInbox).await.unwrap() {
        Reply::Inbox(items) => items
            .into_iter()
            .map(|i| i.title)
            .filter(|t| t.contains("out of date") || t.contains("revalidated"))
            .collect(),
        other => panic!("expected the inbox, got {other:?}"),
    }
}

/// What the inbox says once the first notice arrives, and a moment later, so a second one would
/// show.
pub async fn notices(c: &mut Client) -> Vec<String> {
    for _ in 0..50 {
        if !round_titles(c).await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    round_titles(c).await
}

pub async fn assert_no_notice(c: &mut Client) {
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(round_titles(c).await, Vec::<String>::new());
}

/// What the fake says about the five comments: `a` moves to line 1, `b` is obsolete, `n` gets new
/// wording, `q` moves to a line the diff does not have and `g`, a general note, to a line at all.
/// An unknown item and a block that is not JSON complete the four blocks that cannot be used.
pub fn answer(ids: &Ids) -> Turn {
    Turn::answer_blocks(
        "Checked.",
        &[
            revalidation_block(json!({
                "item": ids.a, "verdict": "moved", "path": FILE, "line": 1,
                "reason": "The comment follows the same code."
            })),
            revalidation_block(json!({
                "item": ids.b, "verdict": "obsolete", "reason": "The line was deleted."
            })),
            revalidation_block(json!({
                "item": ids.q, "verdict": "moved", "path": FILE, "line": 99, "reason": "x"
            })),
            revalidation_block(json!({
                "item": ids.n, "verdict": "reword", "text": "Looks good; one nit.",
                "reason": "Sharper."
            })),
            revalidation_block(json!({
                "item": ids.g, "verdict": "moved", "path": FILE, "line": 1, "reason": "x"
            })),
            revalidation_block(json!({"item": "zz", "verdict": "kept"})),
            "```clusia-revalidation\nnot json\n```".to_string(),
        ],
    )
}

pub async fn use_script(w: &World, script: Script) -> std::path::PathBuf {
    fake_program(w, script).await
}
