//! What an agent session needs from the review around it: the notes file in the worktree, the
//! summary on open, accepting and dismissing suggestions, the notification when a turn ends
//! unseen, and the end of the session.

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clusia_core::config::OnOpen;
use clusia_core::notify::NotifyEvent;
use clusia_core::{AgentState, DraftKind, Origin, PrConversation, PrDetail, PrRef, Review, Side};
use clusia_protocol::{
    AgentLogEntry, AnchorInput, ErrorCode, Outcome, ProtocolError, Reply, StepStatus, Suggestion,
};
use clusia_store::agent::{load_agent_state, save_agent_state};
use clusia_store::load_review_cache;

use crate::agent_log;
use crate::notifications;
use crate::reviews;
use crate::sessions::{Prompt, Refusal, Sessions};
use crate::state::Shared;
use crate::sync::now_unix;

/// The most of the description the agent is given.
const MAX_DESCRIPTION: usize = 4000;
/// How many review threads the summary prompt lists, and how much of each.
const MAX_THREADS: usize = 20;
const MAX_THREAD_TEXT: usize = 240;

const FIRST_SUMMARY: &str = "Summarize this pull request for the reviewer.";
const NEW_COMMITS_SUMMARY: &str = "The pull request has new commits since your last summary. Say what changed, whether it affects the draft, and what deserves a careful look now.";
const WHAT_TO_READ: &str = "Read .clusia/review.md for the draft, then the changed files you need. Say what the pull request does, which files matter most and what deserves a careful look. Be concise.";

/// The text cut to `max` characters, with an ellipsis when something was left out.
fn clipped(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{}…", kept.trim_end())
}

/// What the agent is asked when the review opens: the pull request as GitHub describes it (title,
/// author, branches, description) and the review threads already there, so its first answer does
/// not start from nothing. `again` is for a pull request that has new commits since the last one.
pub(crate) fn summary_prompt(
    again: bool,
    detail: &PrDetail,
    conversation: &PrConversation,
) -> String {
    let mut out = String::from(if again {
        NEW_COMMITS_SUMMARY
    } else {
        FIRST_SUMMARY
    });
    let _ = write!(
        out,
        "\n\nTitle: {}\nAuthor: @{}\nBranches: {} -> {}\n\nDescription:\n",
        detail.summary.title, detail.summary.author, detail.head_ref, detail.base_ref
    );
    match detail.body.trim() {
        "" => out.push_str("(none)"),
        body => out.push_str(&clipped(body, MAX_DESCRIPTION)),
    }
    out.push_str("\n\nReview threads so far:\n");
    let threads: Vec<_> = conversation
        .review_threads
        .iter()
        .filter(|thread| !thread.comments.is_empty())
        .collect();
    if threads.is_empty() {
        out.push_str("(none)\n");
    }
    for thread in threads.iter().take(MAX_THREADS) {
        let state = if thread.is_resolved {
            "resolved"
        } else if thread.is_outdated {
            "outdated"
        } else {
            "open"
        };
        let place = match thread.line {
            Some(line) => format!("{}:{line}", thread.path),
            None => thread.path.clone(),
        };
        let first = &thread.comments[0];
        let _ = writeln!(
            out,
            "- {place} ({state}) @{}: {}",
            first.author,
            clipped(&first.body, MAX_THREAD_TEXT)
        );
    }
    if threads.len() > MAX_THREADS {
        let _ = writeln!(out, "… and {} more", threads.len() - MAX_THREADS);
    }
    let _ = write!(out, "\n{WHAT_TO_READ}");
    out
}

/// The same question without the pull request's details, for a review whose cached copy cannot
/// be read.
fn plain_summary_prompt(again: bool) -> String {
    let intro = if again {
        NEW_COMMITS_SUMMARY
    } else {
        FIRST_SUMMARY
    };
    format!("{intro}\n\n{WHAT_TO_READ}")
}

pub(crate) fn notes_path(shared: &Shared, pr: &PrRef) -> PathBuf {
    shared
        .paths
        .worktree_for(pr)
        .join(".clusia")
        .join("review.md")
}

/// Writes the notes without following a symlink: the worktree is the pull request's, so a
/// tracked `.clusia` (or a file in it) can point anywhere the reviewer can write, such as a
/// slash-command folder. A `.clusia` that is not a real folder is refused, and the notes are not
/// written for that review.
fn write_notes(path: &Path, text: &str) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("the notes have no folder"))?;
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(io::Error::other(format!(
                "{} is not a folder",
                dir.display()
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(dir)?,
        Err(e) => return Err(e),
    }
    let temporary = path.with_extension("md.tmp");
    // Removing a symlink removes the link, never its target.
    match fs::remove_file(&temporary) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)?;
    file.write_all(text.as_bytes())?;
    drop(file);
    // A rename replaces a symlink at `path` itself instead of writing through it.
    fs::rename(&temporary, path)
}

/// Writes `.clusia/review.md` in the worktree: what any agent reads first to know the pull
/// request, the draft, the summary and what was accepted or dismissed. Best effort: without a
/// worktree or a cached copy of the pull request there is nothing to write yet.
pub(crate) fn refresh_review_md(shared: &Shared, review: &Review) {
    if !shared.paths.worktree_for(&review.pr).is_dir() {
        return;
    }
    let cache = match load_review_cache(&shared.paths, &review.pr) {
        Ok(Some(cache)) => cache,
        _ => return,
    };
    let state = load_agent_state(&shared.paths, &review.pr).unwrap_or_default();
    let summary = agent_log::last_summary(&agent_log::read(&shared.paths.agent_log(&review.pr)));
    let text = clusia_core::review_md(review, &cache.pr, summary.as_deref(), &state);
    if let Err(e) = write_notes(&notes_path(shared, &review.pr), &text) {
        tracing::warn!(error = %e, pr = %review.pr, "cannot write the review notes");
    }
}

/// [`refresh_review_md`] for the stored review of `pr`.
pub(crate) fn refresh(shared: &Shared, pr: &PrRef) {
    if let Ok(Some(review)) = reviews::load_stored(shared, pr) {
        refresh_review_md(shared, &review);
    }
}

/// Changes the agent state of `pr` and saves it. The caller holds the review lock.
fn write_state(shared: &Shared, pr: &PrRef, change: impl FnOnce(&mut AgentState)) {
    let mut state = load_agent_state(&shared.paths, pr).unwrap_or_default();
    change(&mut state);
    if let Err(e) = save_agent_state(&shared.paths, pr, &state) {
        tracing::warn!(error = %e, pr = %pr, "cannot save the agent state");
    }
}

/// [`write_state`] under the review lock.
pub(crate) async fn update_state(
    shared: &Shared,
    pr: &PrRef,
    change: impl FnOnce(&mut AgentState),
) {
    let _guard = reviews::lock(shared, pr).await;
    write_state(shared, pr, change);
}

/// Starts the summary turn when the review opens, unless the agent already read this head or
/// the user chose to ask first. Returns how the "agent" load step went.
pub(crate) async fn on_open(shared: &Arc<Shared>, pr: &PrRef, head: &str) -> (StepStatus, String) {
    let skipped = |why: &str| (StepStatus::Skipped, why.to_string());
    let wait = matches!(shared.config.read().await.harness.on_open, OnOpen::Wait);
    if wait {
        return skipped("Waiting for your first question");
    }
    let state = load_agent_state(&shared.paths, pr).unwrap_or_default();
    if state.last_summary_head.as_deref() == Some(head) {
        return skipped("Claude Code already read this version");
    }
    let again = state.last_summary_head.is_some();
    let text = match load_review_cache(&shared.paths, pr) {
        Ok(Some(cache)) => summary_prompt(again, &cache.pr, &cache.conversation),
        _ => plain_summary_prompt(again),
    };
    let prompt = Prompt {
        text,
        shown: false,
        summary_for: Some(head.to_string()),
    };
    match Sessions::submit(shared, pr, prompt).await {
        Ok(_) => (StepStatus::Done, "Claude Code is reading it".to_string()),
        Err(Refusal::Summarizing) => skipped("Claude Code is already reading this version"),
        Err(_) => skipped("Claude Code is busy with this review"),
    }
}

fn not_waiting(id: &str) -> Outcome {
    Outcome::Err(ProtocolError::new(
        ErrorCode::NotFound,
        format!("there is no waiting suggestion {id}"),
    ))
}

/// The suggestion `id` if it is still waiting: in the log and neither accepted nor dismissed.
fn waiting(shared: &Shared, pr: &PrRef, id: &str) -> Option<Suggestion> {
    let state = load_agent_state(&shared.paths, pr).unwrap_or_default();
    if state.accepted.contains(id) || state.dismissed.contains(id) {
        return None;
    }
    agent_log::read(&shared.paths.agent_log(pr))
        .into_iter()
        .rev()
        .find_map(|entry| match entry {
            AgentLogEntry::Suggestion { suggestion, .. } if suggestion.id == id => Some(suggestion),
            _ => None,
        })
}

/// Where a suggestion points: its line, or the end of its range with the start.
fn anchor_of(suggestion: &Suggestion) -> Option<AnchorInput> {
    let (line, start_line) = suggestion.anchor_lines()?;
    Some(AnchorInput {
        path: suggestion.file.clone(),
        line,
        start_line,
        side: Side::Right,
    })
}

/// Turns a waiting suggestion into a draft item written by the agent. The text may be edited
/// first. A suggestion the draft refuses (its line left the diff) stays waiting. The check, the
/// new item and the record that it was accepted happen under one review lock, so two accepts
/// add one item.
pub(crate) async fn accept(
    shared: &Shared,
    client: &str,
    pr: &PrRef,
    id: &str,
    body: Option<String>,
) -> Outcome {
    let guard = reviews::lock(shared, pr).await;
    let Some(suggestion) = waiting(shared, pr, id) else {
        return not_waiting(id);
    };
    let Some(anchor) = anchor_of(&suggestion) else {
        return Outcome::Err(ProtocolError::new(
            ErrorCode::BadRequest,
            "the suggestion names no line",
        ));
    };
    let body = body.unwrap_or_else(|| suggestion.body.clone());
    let item = reviews::NewItem {
        kind: DraftKind::LineComment,
        anchor: Some(anchor),
        thread: None,
        body: &body,
    };
    let outcome = reviews::add_item_locked(shared, client, pr, Origin::Agent, item).await;
    if matches!(outcome, Outcome::Ok(Reply::DraftItem(_))) {
        write_state(shared, pr, |state| {
            state.accepted.insert(id.to_string());
        });
        drop(guard);
        refresh(shared, pr);
    }
    outcome
}

/// Records that a waiting suggestion was dismissed: it is not shown or proposed again.
pub(crate) async fn dismiss(shared: &Shared, pr: &PrRef, id: &str) -> Outcome {
    if waiting(shared, pr, id).is_none() {
        return not_waiting(id);
    }
    update_state(shared, pr, |state| {
        state.dismissed.insert(id.to_string());
    })
    .await;
    refresh(shared, pr);
    Outcome::Ok(Reply::Ack)
}

/// A turn ended well. Tells the user when no window shows the review.
pub(crate) async fn notify_finished(shared: &Shared, pr: &PrRef, turn: u64) {
    if shared.holds.is_held(pr) {
        return;
    }
    let title = match reviews::load_stored(shared, pr) {
        Ok(Some(review)) => review.title,
        _ => String::new(),
    };
    let at = now_unix();
    let event =
        NotifyEvent::agent_finished(pr, &title, format!("agent_finished:{pr}:{turn}:{at}"), at);
    notifications::deliver(shared, vec![event]).await;
}

/// The review is being published or discarded: its turn is stopped, its queue dropped and its
/// session slot forgotten. The log stays. Called before the review lock is taken, because a turn
/// may be waiting for that lock.
pub(crate) async fn stop(shared: &Shared, pr: &PrRef) {
    shared.sessions.end(shared, pr).await;
}

/// The review is gone: what was still waiting is dismissed and the summary forgotten, so a later
/// review of the same pull request starts clean. The caller may hold the review lock.
pub(crate) fn forget(shared: &Shared, pr: &PrRef) {
    let waiting: Vec<String> = agent_log::read(&shared.paths.agent_log(pr))
        .into_iter()
        .filter_map(|entry| match entry {
            AgentLogEntry::Suggestion { suggestion, .. } => Some(suggestion.id),
            _ => None,
        })
        .collect();
    write_state(shared, pr, |state| {
        state.dismissed.extend(waiting);
        state.last_summary_head = None;
    });
}

#[cfg(test)]
mod tests {
    use clusia_core::{PrSummary, ReviewThread, ThreadPost};

    use super::*;

    fn suggestion(line: Option<u32>, start: Option<u32>, end: Option<u32>) -> Suggestion {
        Suggestion {
            id: "sug-1".into(),
            file: "src/a.rs".into(),
            line,
            start_line: start,
            end_line: end,
            body: "Why?".into(),
        }
    }

    #[test]
    fn the_notes_never_follow_a_symlinked_clusia_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("worktree");
        let elsewhere = tmp.path().join("elsewhere");
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, worktree.join(".clusia")).unwrap();
        let path = worktree.join(".clusia").join("review.md");
        assert!(write_notes(&path, "from the pull request").is_err());
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    #[test]
    fn the_notes_are_skipped_when_clusia_is_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".clusia"), "tracked").unwrap();
        let path = tmp.path().join(".clusia").join("review.md");
        assert!(write_notes(&path, "notes").is_err());
        assert_eq!(
            fs::read_to_string(tmp.path().join(".clusia")).unwrap(),
            "tracked"
        );
    }

    #[test]
    fn the_notes_never_follow_a_symlinked_file_in_the_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside.md");
        fs::write(&outside, "keep").unwrap();
        let dir = tmp.path().join(".clusia");
        fs::create_dir(&dir).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("review.md.tmp")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("review.md")).unwrap();
        write_notes(&dir.join("review.md"), "notes").unwrap();
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
        assert!(
            !fs::symlink_metadata(dir.join("review.md"))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(dir.join("review.md")).unwrap(), "notes");
    }

    #[test]
    fn the_notes_folder_is_made_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".clusia").join("review.md");
        write_notes(&path, "notes").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "notes");
    }

    #[test]
    fn a_suggestion_points_at_its_line_or_the_end_of_its_range() {
        let anchor = anchor_of(&suggestion(Some(44), None, None)).unwrap();
        assert_eq!(
            (anchor.path.as_str(), anchor.line, anchor.start_line),
            ("src/a.rs", 44, None)
        );
        assert_eq!(anchor.side, Side::Right);
        let range = anchor_of(&suggestion(None, Some(40), Some(44))).unwrap();
        assert_eq!((range.line, range.start_line), (44, Some(40)));
        assert!(anchor_of(&suggestion(None, Some(40), None)).is_none());
        assert!(anchor_of(&suggestion(None, None, None)).is_none());
    }

    fn detail(body: &str) -> PrDetail {
        PrDetail {
            summary: PrSummary {
                pr: "acme/widgets#7".parse().unwrap(),
                title: "Add feature".into(),
                author: "maria".into(),
                url: "https://github.com/acme/widgets/pull/7".into(),
                draft: false,
                updated_at: "2026-10-01T12:00:00Z".into(),
                comments: 0,
            },
            base_ref: "main".into(),
            head_ref: "feature".into(),
            base_sha: "b".repeat(40),
            head_sha: "h".repeat(40),
            additions: 3,
            deletions: 0,
            changed_files: 1,
            clone_url: "https://github.com/acme/widgets.git".into(),
            closed: false,
            merged: false,
            body: body.into(),
        }
    }

    fn thread(
        path: &str,
        line: Option<u32>,
        resolved: bool,
        outdated: bool,
        text: &str,
    ) -> ReviewThread {
        ReviewThread {
            id: "PRRT_1".into(),
            is_resolved: resolved,
            is_outdated: outdated,
            path: path.into(),
            line,
            start_line: None,
            side: Side::Right,
            viewer_can_reply: true,
            viewer_can_resolve: true,
            comments: vec![ThreadPost {
                database_id: Some(1),
                author: "mona".into(),
                body: text.into(),
                created_at: "2026-10-01T12:00:00Z".into(),
                url: "https://github.com/acme/widgets/pull/7#discussion_r1".into(),
            }],
        }
    }

    #[test]
    fn the_summary_prompt_carries_the_description_and_the_threads() {
        let conversation = PrConversation {
            review_threads: vec![
                thread(
                    "src/auth/refresh.rs",
                    Some(44),
                    false,
                    false,
                    "Why a minute?",
                ),
                thread("src/auth/store.rs", None, false, true, "Old remark"),
                thread("src/http.rs", Some(9), true, false, "Done"),
            ],
            ..PrConversation::default()
        };
        let prompt = summary_prompt(
            false,
            &detail("Adds the refresh job.\n\nIt keeps tokens fresh."),
            &conversation,
        );
        assert!(prompt.starts_with("Summarize this pull request for the reviewer."));
        assert!(prompt.contains("Title: Add feature\nAuthor: @maria\nBranches: feature -> main"));
        assert!(prompt.contains("Adds the refresh job.\n\nIt keeps tokens fresh."));
        assert!(prompt.contains("- src/auth/refresh.rs:44 (open) @mona: Why a minute?"));
        assert!(prompt.contains("- src/auth/store.rs (outdated) @mona: Old remark"));
        assert!(prompt.contains("- src/http.rs:9 (resolved) @mona: Done"));
        assert!(prompt.ends_with("Be concise."));
        assert!(prompt.contains(".clusia/review.md"));
    }

    #[test]
    fn the_second_summary_asks_about_what_changed() {
        let prompt = summary_prompt(true, &detail("x"), &PrConversation::default());
        assert!(prompt.starts_with("The pull request has new commits since your last summary."));
        assert!(plain_summary_prompt(true).contains("new commits"));
        assert!(plain_summary_prompt(false).starts_with("Summarize this pull request"));
    }

    #[test]
    fn no_description_and_no_threads_say_so() {
        let prompt = summary_prompt(false, &detail("  \n"), &PrConversation::default());
        assert!(prompt.contains("Description:\n(none)"));
        assert!(prompt.contains("Review threads so far:\n(none)"));
    }

    #[test]
    fn a_long_description_and_many_threads_are_cut() {
        let conversation = PrConversation {
            review_threads: (0..25)
                .map(|n| {
                    thread(
                        &format!("src/f{n}.rs"),
                        Some(1),
                        false,
                        false,
                        &"x".repeat(500),
                    )
                })
                .collect(),
            ..PrConversation::default()
        };
        let prompt = summary_prompt(false, &detail(&"d".repeat(9000)), &conversation);
        assert!(prompt.contains(&format!("{}…", "d".repeat(MAX_DESCRIPTION))));
        assert!(!prompt.contains(&"d".repeat(MAX_DESCRIPTION + 1)));
        assert!(prompt.contains("src/f19.rs") && !prompt.contains("src/f20.rs"));
        assert!(prompt.contains("… and 5 more"));
        assert!(prompt.contains(&format!("{}…", "x".repeat(MAX_THREAD_TEXT))));
    }
}
