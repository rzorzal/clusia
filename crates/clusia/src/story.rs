//! `clusia story`: lists a review's Story, or has the agent write it first and follows the run.

use std::collections::BTreeSet;
use std::path::Path;

use clusia_calls::StaticStep;
use clusia_core::story::{StoryVersion, StoryVersionState};
use clusia_core::{Paths, PrRef};
use clusia_protocol::{
    Client, ClientError, Command as Request, Event, Reply, StoryState, StoryVersionInfo, TurnOrigin,
};
use serde_json::json;
use tokio::signal::unix::{Signal, SignalKind, signal};

use crate::calls::{open, wait_for_index};
use crate::check::line_of;
use crate::review::parse_pr;
use crate::run::{CliError, Output, connect, unexpected};

/// What the daemon says when a run already exists.
const BUSY: &str = "The story is already being written";

fn seven(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// What `GetStory` says.
struct Listing {
    versions: Vec<StoryVersionInfo>,
    latest: Option<StoryVersion>,
    state: StoryState,
    static_steps: Vec<StaticStep>,
    read: Vec<String>,
    head: String,
    new_commits: u32,
}

/// One step as a line: its number and title, then its subtitle, its mark and whether it was read.
fn step_line(n: usize, title: &str, subtitle: &str, mechanical: bool, read: bool) -> String {
    let mut line = format!("{n}. {}", line_of(title));
    let subtitle = line_of(subtitle);
    if !subtitle.is_empty() {
        line.push_str(&format!(" · {subtitle}"));
    }
    if mechanical {
        line.push_str(" · small and mechanical");
    }
    if read {
        line.push_str(" · read");
    }
    line
}

/// The lines of a listing: the steps, the removed steps of a written version, who wrote it, when
/// the pull request moved on, and what the run does.
fn list_lines(l: &Listing) -> Vec<String> {
    let written = l
        .latest
        .as_ref()
        .filter(|v| v.state == StoryVersionState::Written)
        .and_then(|v| v.spec.as_ref().map(|spec| (v, spec)));
    let mut lines: Vec<String> = match written {
        Some((_, spec)) => spec
            .steps
            .iter()
            .enumerate()
            .map(|(i, step)| {
                step_line(
                    i + 1,
                    &step.title,
                    &step.subtitle,
                    step.mechanical,
                    l.read.contains(&step.id),
                )
            })
            .collect(),
        None => l
            .static_steps
            .iter()
            .enumerate()
            .map(|(i, step)| {
                step_line(
                    i + 1,
                    &step.title,
                    &step.subtitle,
                    false,
                    l.read.contains(&step.id),
                )
            })
            .collect(),
    };
    if let Some((version, spec)) = written {
        lines.extend(
            spec.removed
                .iter()
                .map(|step| format!("– {} · gone", line_of(&step.title))),
        );
        lines.push(format!(
            "Written by {} from commit {}",
            line_of(&version.harness),
            seven(&version.commit)
        ));
        if !l.head.is_empty() && version.commit != l.head {
            let moved = match l.new_commits {
                0 => String::new(),
                1 => " (1 new commit)".to_string(),
                n => format!(" ({n} new commits)"),
            };
            lines.push(format!(
                "Out of date: the pull request is at {} now{moved}. Write it again with: clusia story <pr> --write",
                seven(&l.head)
            ));
        }
    } else {
        lines.push("Static order".into());
    }
    match &l.state {
        StoryState::Idle => {}
        StoryState::Waiting => lines.push("Waiting for another check to finish…".into()),
        StoryState::Writing => lines.push("Writing the story…".into()),
        StoryState::Failed { message } => {
            lines.push(format!(
                "The story could not be written: {}",
                line_of(message)
            ));
        }
    }
    lines
}

/// The line of progress for a state of the run, when it is one worth a line.
fn progress_line(state: &StoryState) -> Option<String> {
    match state {
        StoryState::Waiting => Some("waiting for another check to finish".into()),
        StoryState::Writing => Some("writing the story".into()),
        StoryState::Idle | StoryState::Failed { .. } => None,
    }
}

fn output(l: &Listing) -> Output {
    Output {
        human: list_lines(l).join("\n"),
        json: json!({
            "versions": l.versions,
            "latest": l.latest,
            "state": l.state,
            "static_steps": l.static_steps,
            "read": l.read,
            "head": l.head,
            "new_commits": l.new_commits,
        }),
    }
}

async fn listing(client: &mut Client, pr: &PrRef) -> Result<Listing, CliError> {
    let reply = client.request(Request::GetStory { pr: pr.clone() }).await?;
    let Reply::Story {
        versions,
        latest,
        state,
        static_steps,
        read,
        head,
        new_commits,
    } = reply
    else {
        return Err(unexpected(reply));
    };
    Ok(Listing {
        versions,
        latest,
        state,
        static_steps,
        read,
        head,
        new_commits,
    })
}

pub(crate) async fn story(
    paths: &Paths,
    home: Option<&Path>,
    pr: &str,
    write: bool,
    json: bool,
) -> Result<Output, CliError> {
    let pr = parse_pr(pr)?;
    if write {
        return write_and_follow(paths, home, &pr, json).await;
    }
    let mut client = connect(paths, home).await?;
    open(&mut client, &pr).await?;
    // The static order comes from the call index.
    wait_for_index(&mut client, &pr).await;
    Ok(output(&listing(&mut client, &pr).await?))
}

/// Starts the writing, follows it on standard error and lists what it made.
async fn write_and_follow(
    paths: &Paths,
    home: Option<&Path>,
    pr: &PrRef,
    json: bool,
) -> Result<Output, CliError> {
    // Caught from here on: a Ctrl-C before the first poll of a handler would end the process
    // without a word.
    let mut interrupt =
        signal(SignalKind::interrupt()).map_err(|e| CliError::Other(e.to_string()))?;
    let mut client = connect(paths, home).await?;
    // Set before the write goes out: from then on a run may exist that Ctrl-C must stop.
    let sent = std::cell::Cell::new(false);
    let opened = async {
        open(&mut client, pr).await?;
        let found = listing(&mut client, pr).await?;
        let request = if found.latest.is_some() {
            Request::RewriteStory { pr: pr.clone() }
        } else {
            Request::WriteStory { pr: pr.clone() }
        };
        sent.set(true);
        match client.request(request).await {
            Err(ClientError::Server(e)) if e.message == BUSY => Ok(true),
            other => other.map(|_| false).map_err(CliError::from),
        }
    };
    // A run that already exists is followed from the subscription on, so its ending is the one
    // to wait for.
    let mut begun = tokio::select! {
        opened = opened => opened?,
        _ = interrupt.recv() => {
            if sent.get() {
                stop(&mut client, pr, &mut interrupt).await;
            } else {
                eprintln!("Interrupted: nothing was written.");
            }
            std::process::exit(130);
        }
    };
    if begun && !json {
        eprintln!("{BUSY}; following that run.");
    }
    let harness = crate::harness::name_of(&mut client, pr).await;
    let mut asked = BTreeSet::new();
    let mut failure: Option<String> = None;
    loop {
        let (_, event) = tokio::select! {
            event = client.next_event() => event?,
            _ = interrupt.recv() => {
                stop(&mut client, pr, &mut interrupt).await;
                std::process::exit(130);
            }
        };
        match event {
            Event::StoryChanged { pr: of, state } if of == *pr => {
                begun |= matches!(state, StoryState::Waiting | StoryState::Writing);
                if !json && let Some(line) = progress_line(&state) {
                    eprintln!("{line}");
                }
                match state {
                    // An ending told before the run was seen is an older run's.
                    StoryState::Idle if begun => break,
                    StoryState::Failed { message } => {
                        failure = Some(message);
                        break;
                    }
                    _ => {}
                }
            }
            Event::PermissionRequested {
                pr: of,
                id,
                tool,
                summary,
                origin: TurnOrigin::Story,
                ..
            } if of == *pr && asked.insert(id.clone()) && !json => {
                eprintln!(
                    "story: waiting for an answer in the window: {harness} wants to {}: {}",
                    crate::agent::wants_to(&tool, &line_of(&tool)),
                    line_of(&summary)
                );
            }
            _ => {}
        }
    }
    let out = output(&listing(&mut client, pr).await?);
    match failure {
        None => Ok(out),
        Some(message) => {
            if json {
                println!("{}", out.json);
            } else {
                println!("{}", crate::agent::streamed(&out.human));
            }
            Err(CliError::Other(format!(
                "the story could not be written: {}",
                line_of(&message)
            )))
        }
    }
}

/// Asks the daemon to stop the run, given up on at a second Ctrl-C or after a few seconds.
async fn stop(client: &mut Client, pr: &PrRef, interrupt: &mut Signal) {
    let stopped = tokio::select! {
        done = client.request(Request::StopStory { pr: pr.clone() }) => done.is_ok(),
        _ = interrupt.recv() => false,
        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => false,
    };
    if stopped {
        eprintln!("Stopped the story.");
    } else {
        eprintln!("The daemon did not answer; the run may still go on (Stop in the window).");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::hunks::HunkRef;
    use clusia_core::story::StorySpec;
    use serde_json::json;

    fn static_step(title: &str, subtitle: &str, other: bool) -> StaticStep {
        StaticStep {
            id: title.into(),
            title: title.into(),
            subtitle: subtitle.into(),
            hunks: vec![HunkRef {
                path: "src/store.rs".into(),
                start_line: 1,
                end_line: 3,
            }],
            other,
            explanation: String::new(),
        }
    }

    fn listing(latest: Option<StoryVersion>, state: StoryState) -> Listing {
        Listing {
            versions: Vec::new(),
            latest,
            state,
            static_steps: vec![
                static_step("TokenStore::refresh", "src/store.rs", false),
                static_step("Other files", "", true),
            ],
            read: vec!["TokenStore::refresh".into()],
            head: String::new(),
            new_commits: 0,
        }
    }

    fn written() -> StoryVersion {
        let spec: StorySpec = serde_json::from_value(json!({
            "sentence": "Tokens refresh once.",
            "steps": [
                {"id": "a", "title": "The store refreshes under a lock", "subtitle": "src/store.rs", "hunks": []},
                {"id": "b", "title": "Callers use it", "mechanical": true, "hunks": []}
            ],
            "removed": [{"id": "old", "title": "Retry loop"}]
        }))
        .unwrap();
        StoryVersion {
            number: 2,
            commit: "3f2a1c9d".repeat(5),
            harness: "Claude Code".into(),
            started_at: "2026-10-19T12:00:00Z".into(),
            state: StoryVersionState::Written,
            spec: Some(spec),
            notes: Vec::new(),
        }
    }

    #[test]
    fn without_a_written_story_the_static_order_is_listed_with_its_read_marks() {
        let lines = list_lines(&listing(None, StoryState::Idle));
        assert_eq!(
            lines,
            [
                "1. TokenStore::refresh · src/store.rs · read",
                "2. Other files",
                "Static order"
            ]
        );
    }

    #[test]
    fn a_written_story_lists_its_steps_removed_steps_and_who_wrote_it() {
        let mut l = listing(Some(written()), StoryState::Idle);
        l.read = vec!["b".into()];
        assert_eq!(
            list_lines(&l),
            [
                "1. The store refreshes under a lock · src/store.rs",
                "2. Callers use it · small and mechanical · read",
                "– Retry loop · gone",
                "Written by Claude Code from commit 3f2a1c9",
            ]
        );
        l.head = "9be1d07a".repeat(5);
        assert_eq!(
            list_lines(&l).last().unwrap(),
            "Out of date: the pull request is at 9be1d07 now. Write it again with: clusia story <pr> --write"
        );
        l.new_commits = 3;
        assert_eq!(
            list_lines(&l).last().unwrap(),
            "Out of date: the pull request is at 9be1d07 now (3 new commits). Write it again with: clusia story <pr> --write"
        );
        l.new_commits = 1;
        assert!(
            list_lines(&l)
                .last()
                .unwrap()
                .contains("now (1 new commit). Write")
        );
        l.new_commits = 0;
        l.head = "3f2a1c9d".repeat(5);
        assert_eq!(
            list_lines(&l).len(),
            4,
            "the head is the commit: not out of date"
        );
    }

    #[test]
    fn a_version_that_was_not_written_lists_the_static_order() {
        let mut version = written();
        version.state = StoryVersionState::NotWritten;
        version.spec = None;
        let lines = list_lines(&listing(Some(version), StoryState::Idle));
        assert_eq!(lines.last().unwrap(), "Static order");
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn the_state_adds_one_last_line() {
        let state = |s| list_lines(&listing(None, s)).last().unwrap().clone();
        assert_eq!(
            state(StoryState::Waiting),
            "Waiting for another check to finish…"
        );
        assert_eq!(state(StoryState::Writing), "Writing the story…");
        assert_eq!(
            state(StoryState::Failed {
                message: "no\nblock".into()
            }),
            "The story could not be written: no block"
        );
        assert_eq!(state(StoryState::Idle), "Static order");
    }

    #[test]
    fn text_from_the_agent_is_one_printable_line() {
        let mut version = written();
        let spec = version.spec.as_mut().unwrap();
        spec.steps[0].title = "Evil\u{1b}[2K\nline".into();
        spec.steps[0].subtitle = "a\u{202e}b".into();
        let lines = list_lines(&listing(Some(version), StoryState::Idle));
        assert_eq!(lines[0], "1. Evil\\u{1b}[2K line · a\\u{202e}b");
        assert!(
            lines
                .iter()
                .all(|l| !l.contains('\u{1b}') && !l.contains('\u{202e}'))
        );
    }

    #[test]
    fn progress_says_what_the_run_does() {
        assert_eq!(
            progress_line(&StoryState::Waiting).as_deref(),
            Some("waiting for another check to finish")
        );
        assert_eq!(
            progress_line(&StoryState::Writing).as_deref(),
            Some("writing the story")
        );
        assert_eq!(progress_line(&StoryState::Idle), None);
        assert_eq!(
            progress_line(&StoryState::Failed {
                message: "x".into()
            }),
            None
        );
    }

    #[test]
    fn json_holds_the_reply_as_it_came() {
        let out = output(&listing(Some(written()), StoryState::Writing));
        assert_eq!(out.json["state"], json!({"state": "writing"}));
        assert_eq!(out.json["latest"]["state"], json!({"state": "written"}));
        assert_eq!(out.json["static_steps"][1]["other"], json!(true));
        assert_eq!(out.json["read"], json!(["TokenStore::refresh"]));
        assert_eq!(out.json["head"], json!(""));
        assert_eq!(out.json["new_commits"], json!(0));
        assert!(out.human.starts_with("1. The store refreshes under a lock"));
    }
}
