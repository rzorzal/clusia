//! `clusia revalidate`: has Claude Code read the draft of a review against the commits its pull
//! request got, and prints one line for each comment.

use std::io;
use std::path::Path;

use clusia_core::revalidation::{Revalidation, RoundState, RowKind, Verdict, row_kind};
use clusia_core::{Anchor, DraftItem, ItemStatus, Paths, PrRef, Review, ReviewState};
use clusia_protocol::{Client, Command as Request, Event, Reply, TurnOrigin, topics};
use serde_json::json;
use tokio::signal::unix::{Signal, SignalKind, signal};

use crate::agent::printable;
use crate::check::line_of;
use crate::review::parse_pr;
use crate::run::{CliError, Output, connect};

/// How a row of the answer starts.
fn label(kind: RowKind) -> &'static str {
    match kind {
        RowKind::Kept => "KEPT",
        RowKind::Moved => "MOVED",
        RowKind::Obsolete => "OBSOLETE",
        RowKind::NewWording => "REWORD",
        RowKind::NotChecked => "NOT CHECKED",
    }
}

/// `file:line`, `file:start-end` for a range, `general` for a comment with no line. The text is
/// the agent's or the pull request's, so it is filtered.
fn place_of(anchor: Option<&Anchor>) -> String {
    match anchor {
        Some(a) => {
            let path = printable(&a.path);
            match a.start_line {
                Some(start) => format!("{path}:{start}-{}", a.line),
                None => format!("{path}:{}", a.line),
            }
        }
        None => "general".to_string(),
    }
}

/// What the line says about the comment: the agent's reason or new wording, else the comment.
fn text_of(verdict: &Verdict, item: &DraftItem) -> String {
    let said = match verdict {
        Verdict::Moved { reason, .. } | Verdict::Obsolete { reason } => reason.as_str(),
        Verdict::Reword { text, .. } => text.as_str(),
        Verdict::Kept | Verdict::NotChecked => match &item.status {
            ItemStatus::Obsolete { reason } if !reason.is_empty() => reason.as_str(),
            _ => item.body.as_str(),
        },
    };
    line_of(said)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// The report of a finished round: what it covered, then `KEPT|MOVED|OBSOLETE|REWORD|NOT CHECKED
/// file:line  text` for each comment, then how many answers could not be read. It says what Claude
/// Code returned; it never says the draft is right.
pub(crate) fn report_lines(round: &Revalidation, review: &Review, harness: &str) -> Vec<String> {
    let mut lines = vec![format!(
        "{harness} checked your {} against {}.",
        plural(round.asked as usize, "comment", "comments"),
        plural(round.commits.len(), "new commit", "new commits"),
    )];
    for proposal in &round.proposals {
        let Some(item) = review.draft.get(&proposal.item) else {
            continue;
        };
        let kind = row_kind(proposal, item);
        // A move shows the place it goes to; every other row, the place the comment has now.
        let place = match &proposal.verdict {
            Verdict::Moved { anchor, .. } => place_of(Some(anchor)),
            _ => place_of(item.anchor.as_ref()),
        };
        lines.push(format!(
            "{}  {place}  {}",
            label(kind),
            text_of(&proposal.verdict, item)
        ));
    }
    match round.unreadable {
        0 => {}
        1 => lines.push("1 result could not be read".to_string()),
        n => lines.push(format!("{n} results could not be read")),
    }
    lines
}

/// What a state says as a line of progress; `None` for the ones that say nothing new.
pub(crate) fn progress_line(state: &RoundState) -> Option<String> {
    match state {
        RoundState::Waiting => Some("Revalidation: waiting for another run".to_string()),
        RoundState::Running { activity: None } => Some("Revalidation: running".to_string()),
        RoundState::Running {
            activity: Some(activity),
        } => Some(format!("Revalidation: {}", line_of(activity))),
        RoundState::Done => Some("Revalidation: done".to_string()),
        RoundState::Failed { message } => {
            Some(format!("Revalidation: failed: {}", line_of(message)))
        }
        RoundState::Scheduled { .. } => None,
    }
}

fn io_error(e: io::Error) -> CliError {
    CliError::Other(e.to_string())
}

fn no_answer(what: &str) -> CliError {
    CliError::Other(format!("the daemon did not answer {what}"))
}

async fn round_of(client: &mut Client, pr: &PrRef) -> Result<Option<Revalidation>, CliError> {
    match client
        .request(Request::GetRevalidation { pr: pr.clone() })
        .await?
    {
        Reply::Revalidation(round) => Ok(round),
        _ => Err(no_answer("GetRevalidation")),
    }
}

/// The review goes back to where it was: one the command opened from the saved list is closed
/// again, so the daemon keeps following its pull request in the background.
async fn leave(client: &mut Client, pr: &PrRef, was_active: bool) {
    if was_active {
        return;
    }
    let closing = client.request(Request::CloseReview { pr: pr.clone() });
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), closing).await;
}

/// The report of a finished round.
async fn finish(client: &mut Client, pr: &PrRef, round: Revalidation) -> Result<Output, CliError> {
    let Reply::ReviewFile(review) = client
        .request(Request::GetReview { pr: pr.clone() })
        .await?
    else {
        return Err(no_answer("GetReview"));
    };
    Ok(Output {
        human: report_lines(&round, &review, crate::harness::name_of(client).await).join("\n"),
        json: json!({ "revalidation": round }),
    })
}

pub(crate) async fn revalidate(
    paths: &Paths,
    home: Option<&Path>,
    pr: &str,
    again: bool,
    json: bool,
) -> Result<Output, CliError> {
    let pr = parse_pr(pr)?;
    // Caught from here on: a Ctrl-C before the first poll of a handler would end the process
    // without a word.
    let mut interrupt = signal(SignalKind::interrupt()).map_err(io_error)?;
    let mut client = connect(paths, home).await?;
    let was_active = matches!(
        client.request(Request::GetReview { pr: pr.clone() }).await,
        Ok(Reply::ReviewFile(review)) if review.state == ReviewState::Active
    );
    let out = follow(&mut client, &mut interrupt, &pr, again, was_active, json).await;
    leave(&mut client, &pr, was_active).await;
    out
}

async fn follow(
    client: &mut Client,
    interrupt: &mut Signal,
    pr: &PrRef,
    again: bool,
    was_active: bool,
    json: bool,
) -> Result<Output, CliError> {
    let opened = async {
        client
            .request(Request::Subscribe {
                topics: vec![topics::REVIEWS.into(), topics::AGENT.into()],
            })
            .await?;
        // Opening reads the pull request, moves the draft to its new head and starts the round
        // when there is one to start.
        client.request(Request::OpenReview { pr: pr.clone() }).await
    };
    tokio::select! {
        opened = opened => { opened?; }
        _ = interrupt.recv() => {
            eprintln!("Interrupted: nothing was revalidated.");
            leave(client, pr, was_active).await;
            std::process::exit(130);
        }
    }
    let Some(round) = round_of(client, pr).await? else {
        return Err(CliError::Other(
            "There are no new commits to revalidate: the pull request is where you left it"
                .to_string(),
        ));
    };
    // A finished round is printed as it is: running it again replaces what the window shows.
    if round.state == RoundState::Done && !again {
        return finish(client, pr, round).await;
    }
    // A round that waits or runs is followed; any other is run now.
    let mut begun = matches!(
        round.state,
        RoundState::Waiting | RoundState::Running { .. }
    );
    if !begun {
        client
            .request(Request::Revalidate { pr: pr.clone() })
            .await?;
    }
    let harness = crate::harness::name_of(client).await;
    let mut asked: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let finished = loop {
        let (_, event) = tokio::select! {
            event = client.next_event() => event?,
            _ = interrupt.recv() => {
                let stopped = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    client.request(Request::StopRevalidation { pr: pr.clone() }),
                )
                .await;
                if matches!(stopped, Ok(Ok(_))) {
                    eprintln!("Stopped the revalidation.");
                } else {
                    eprintln!(
                        "The daemon did not answer; the revalidation may still run (Stop in the window)."
                    );
                }
                leave(client, pr, was_active).await;
                std::process::exit(130);
            }
        };
        match event {
            Event::RevalidationChanged {
                pr: of,
                revalidation: Some(round),
            } if of == *pr => {
                let running = matches!(
                    round.state,
                    RoundState::Waiting | RoundState::Running { .. }
                );
                // An ending told before this run was seen waiting or running is an older run's.
                if !running && !begun {
                    continue;
                }
                begun |= running;
                if !json && let Some(line) = progress_line(&round.state) {
                    eprintln!("{line}");
                }
                match round.state {
                    RoundState::Done => break round,
                    RoundState::Failed { message } => {
                        return Err(CliError::Other(format!(
                            "The revalidation failed: {}",
                            line_of(&message)
                        )));
                    }
                    _ => {}
                }
            }
            ref gone if review_gone(gone, pr, begun) => {
                return Err(CliError::Other(
                    "The review was published or discarded; nothing was revalidated".into(),
                ));
            }
            Event::PermissionRequested {
                pr: of,
                id,
                tool,
                summary,
                origin: TurnOrigin::Revalidation,
                ..
            } if of == *pr => {
                if asked.insert(id) && !json {
                    eprintln!(
                        "Revalidation: waiting for an answer in the window: {harness} wants to use {}: {}",
                        line_of(&tool),
                        line_of(&summary)
                    );
                }
            }
            _ => {}
        }
    };
    finish(client, pr, finished).await
}

/// The review's round was removed while this run was followed: the review was published or
/// discarded, so no `Done` or `Failed` will come.
fn review_gone(event: &Event, pr: &PrRef, begun: bool) -> bool {
    begun && matches!(event, Event::RevalidationChanged { pr: of, revalidation: None } if of == pr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::revalidation::{CommitLine, Decision, Proposal};
    use clusia_core::{DraftKind, Origin, Side};
    use std::collections::BTreeMap;

    #[test]
    fn a_removed_round_ends_a_followed_run() {
        let pr: PrRef = "acme/widgets#7".parse().unwrap();
        let other: PrRef = "acme/widgets#8".parse().unwrap();
        let gone = |of: &PrRef| Event::RevalidationChanged {
            pr: of.clone(),
            revalidation: None,
        };
        assert!(review_gone(&gone(&pr), &pr, true));
        assert!(!review_gone(&gone(&pr), &pr, false), "an older run's end");
        assert!(!review_gone(&gone(&other), &pr, true));
    }

    fn anchor(line: u32) -> Anchor {
        Anchor {
            path: "src/cache.rs".into(),
            line,
            start_line: None,
            side: Side::Right,
            commit: "c1".into(),
        }
    }

    fn item(id: &str, kind: DraftKind, anchor: Option<Anchor>, body: &str) -> DraftItem {
        DraftItem {
            id: id.into(),
            kind,
            origin: Origin::Human,
            anchor,
            body: body.into(),
            status: ItemStatus::Ok,
            accepted: true,
            created_at: 0,
            thread: None,
        }
    }

    fn proposal(item: &str, before: Option<Anchor>, verdict: Verdict) -> Proposal {
        Proposal {
            item: item.into(),
            before,
            verdict,
            decision: Decision::Pending,
            fingerprint: String::new(),
        }
    }

    fn fixture() -> (Revalidation, Review) {
        let mut review = Review::new(
            "acme/widgets#7".parse().unwrap(),
            "t".into(),
            "b".into(),
            "h2".into(),
            0,
        );
        review.draft.items = vec![
            item("k", DraftKind::LineComment, Some(anchor(10)), "Keep me."),
            item(
                "m",
                DraftKind::LineComment,
                Some(anchor(40)),
                "Rename this.",
            ),
            item("o", DraftKind::LineComment, Some(anchor(50)), "Drop this."),
            item("w", DraftKind::General, None, "Looks good"),
            item("x", DraftKind::LineComment, Some(anchor(60)), "Unchecked."),
        ];
        let round = Revalidation {
            from_head: "h1".into(),
            to_head: "h2".into(),
            commits: vec![
                CommitLine {
                    sha: "abc1234".into(),
                    title: "Add cache".into(),
                },
                CommitLine {
                    sha: "def5678".into(),
                    title: "Fix cache".into(),
                },
            ],
            before: BTreeMap::new(),
            sp1_moved: 0,
            sp1_obsolete: 0,
            state: RoundState::Done,
            proposals: vec![
                proposal("k", Some(anchor(10)), Verdict::Kept),
                proposal(
                    "m",
                    Some(anchor(40)),
                    Verdict::Moved {
                        anchor: anchor(44),
                        reason: "The function gained\u{1b}[2K 4 lines.".into(),
                    },
                ),
                proposal(
                    "o",
                    Some(anchor(50)),
                    Verdict::Obsolete {
                        reason: "The line was deleted.".into(),
                    },
                ),
                proposal(
                    "w",
                    None,
                    Verdict::Reword {
                        text: "Looks good;\none nit".into(),
                        reason: None,
                    },
                ),
                proposal("x", Some(anchor(60)), Verdict::NotChecked),
            ],
            unreadable: 2,
            asked: 5,
            finished_at: Some(1),
            notified: false,
        };
        (round, review)
    }

    #[test]
    fn cli_prints_filtered_verdicts() {
        let (round, review) = fixture();
        assert_eq!(
            report_lines(&round, &review, "Claude Code"),
            [
                "Claude Code checked your 5 comments against 2 new commits.",
                "KEPT  src/cache.rs:10  Keep me.",
                "MOVED  src/cache.rs:44  The function gained\\u{1b}[2K 4 lines.",
                "OBSOLETE  src/cache.rs:50  The line was deleted.",
                "REWORD  general  Looks good; one nit",
                "NOT CHECKED  src/cache.rs:60  Unchecked.",
                "2 results could not be read",
            ]
        );
    }

    #[test]
    fn the_report_names_the_harness_in_use() {
        let (round, review) = fixture();
        let lines = report_lines(&round, &review, "Codex");
        assert!(lines[0].starts_with("Codex checked your "), "{}", lines[0]);
    }

    #[test]
    fn a_comment_sp1_moved_and_the_agent_kept_reads_moved() {
        let (mut round, mut review) = fixture();
        review.draft.items[0].anchor = Some(anchor(12));
        round.proposals.truncate(1);
        round.unreadable = 0;
        round.asked = 1;
        round.commits.truncate(1);
        let lines = report_lines(&round, &review, "Claude Code");
        assert_eq!(
            lines[0],
            "Claude Code checked your 1 comment against 1 new commit."
        );
        assert_eq!(lines[1], "MOVED  src/cache.rs:12  Keep me.");
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn a_comment_that_left_the_draft_is_not_listed() {
        let (round, mut review) = fixture();
        review.draft.items.retain(|i| i.id != "k");
        assert!(
            !report_lines(&round, &review, "Claude Code")
                .iter()
                .any(|l| l.starts_with("KEPT"))
        );
    }

    #[test]
    fn progress_says_what_the_agent_does_without_repainting_the_terminal() {
        assert_eq!(
            progress_line(&RoundState::Running {
                activity: Some("Reading\u{1b}[2K src/a.rs".into())
            })
            .as_deref(),
            Some("Revalidation: Reading\\u{1b}[2K src/a.rs")
        );
        assert_eq!(progress_line(&RoundState::Scheduled { at: 1 }), None);
        assert_eq!(
            progress_line(&RoundState::Failed {
                message: "boom".into()
            })
            .as_deref(),
            Some("Revalidation: failed: boom")
        );
    }
}
