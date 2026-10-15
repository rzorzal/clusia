//! `clusia diagrams`: lists the latest version of a review's diagrams, or draws them first.

use std::collections::HashMap;
use std::path::Path;

use clusia_core::diagram::{DiagramVersion, EntryState};
use clusia_core::{Paths, PrRef};
use clusia_protocol::{
    ClientError, Command as Request, DiagramsState, Event, Reply, TurnOrigin, topics,
};
use serde_json::json;
use tokio::signal::unix::{Signal, SignalKind, signal};

use crate::check::line_of;
use crate::review::parse_pr;
use crate::run::{CliError, Output, connect};

/// What the daemon says when a drawing already runs.
const BUSY: &str = "Diagrams are already being drawn";

fn seven(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// Where an entry stands, in a few words.
fn state_text(state: &EntryState) -> String {
    match state {
        EntryState::Waiting => "waiting".into(),
        EntryState::Drawing => "drawing".into(),
        EntryState::Ready { notes, .. } if notes.is_empty() => "ready".into(),
        EntryState::Ready { notes, .. } => format!("ready · {} noted", notes.len()),
        EntryState::Failed { reason } => format!("failed · {}", line_of(reason)),
        EntryState::NotDrawn => "not drawn".into(),
    }
}

/// How many entries of `version` could not be drawn.
pub(crate) fn failed_count(version: &DiagramVersion) -> usize {
    version
        .entries
        .iter()
        .filter(|entry| matches!(entry.state, EntryState::Failed { .. }))
        .count()
}

/// The lines of a listing: one for each entry (`key  kind  title  state`), then the version
/// and, when the review moved on, the stale line. Everything the agent wrote is one printable
/// line.
pub(crate) fn list_lines(
    version: Option<&DiagramVersion>,
    stale: bool,
    head: &str,
    stale_reason: Option<&str>,
) -> Vec<String> {
    let Some(version) = version else {
        return vec!["No diagrams yet. Draw them with: clusia diagrams <pr> --draw".into()];
    };
    let mut lines: Vec<String> = version
        .entries
        .iter()
        .map(|entry| {
            format!(
                "{}  {}  {}  {}",
                line_of(&entry.plan.key),
                entry.plan.kind.label(),
                line_of(&entry.plan.title),
                state_text(&entry.state)
            )
        })
        .collect();
    lines.push(format!(
        "Version {} · drawn by {} from {}",
        version.number,
        line_of(&version.harness),
        seven(&version.commit)
    ));
    if stale {
        let mut line = format!(
            "Out of date: the pull request is at {} now. Draw again with: clusia diagrams <pr> --draw",
            seven(head)
        );
        if let Some(reason) = stale_reason {
            line.push_str(&format!(" ({})", line_of(reason)));
        }
        lines.push(line);
    }
    lines
}

/// A line of progress for a state of the run; `titles` maps the keys of the plan to their titles.
pub(crate) fn progress_line(
    state: &DiagramsState,
    titles: &HashMap<String, String>,
) -> Option<String> {
    match state {
        DiagramsState::Planning => Some("planning the diagrams".into()),
        DiagramsState::Drawing { done, total, key } => Some(format!(
            "drawing {} of {total}: {}",
            done + 1,
            line_of(titles.get(key).map_or(key.as_str(), String::as_str))
        )),
        DiagramsState::Idle | DiagramsState::Failed { .. } => None,
    }
}

/// What `GetDiagrams` says.
struct Listing {
    latest: Option<DiagramVersion>,
    stale: bool,
    head: String,
    reason: Option<String>,
}

async fn listing(client: &mut clusia_protocol::Client, pr: &PrRef) -> Result<Listing, CliError> {
    let Reply::Diagrams {
        latest,
        stale,
        head,
        stale_reason,
        ..
    } = client
        .request(Request::GetDiagrams { pr: pr.clone() })
        .await?
    else {
        return Err(CliError::Other(
            "the daemon did not answer GetDiagrams".into(),
        ));
    };
    Ok(Listing {
        latest: latest.map(|v| *v),
        stale,
        head,
        reason: stale_reason,
    })
}

fn output(found: &Listing) -> Output {
    let human = list_lines(
        found.latest.as_ref(),
        found.stale,
        &found.head,
        found.reason.as_deref(),
    )
    .join("\n");
    Output {
        human,
        json: json!({ "version": found.latest, "stale": found.stale }),
    }
}

/// The listing, then an error when an entry failed: what was drawn is printed before it.
fn finish(found: &Listing, json: bool) -> Result<Output, CliError> {
    let out = output(found);
    let failed = found.latest.as_ref().map_or(0, failed_count);
    if failed == 0 {
        return Ok(out);
    }
    if json {
        println!("{}", out.json);
    } else {
        println!("{}", crate::agent::streamed(&out.human));
    }
    let noun = if failed == 1 { "diagram" } else { "diagrams" };
    Err(CliError::Other(format!(
        "{failed} {noun} could not be drawn"
    )))
}

pub(crate) async fn diagrams(
    paths: &Paths,
    home: Option<&Path>,
    pr: &str,
    draw: bool,
    json: bool,
) -> Result<Output, CliError> {
    let pr = parse_pr(pr)?;
    if !draw {
        let mut client = connect(paths, home).await?;
        return finish(&listing(&mut client, &pr).await?, json);
    }
    draw_all(paths, home, &pr, json).await
}

/// Starts the drawing, follows it on standard error and lists what it made.
async fn draw_all(
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
    // Set before `DrawDiagrams` goes out: from then on a run may exist that Ctrl-C must stop.
    let sent = std::cell::Cell::new(false);
    let opened = async {
        client
            .request(Request::Subscribe {
                topics: vec![topics::AGENT.into()],
            })
            .await?;
        // The drawing reads the review's worktree, which opening prepares.
        client
            .request(Request::OpenReview { pr: pr.clone() })
            .await?;
        sent.set(true);
        match client
            .request(Request::DrawDiagrams { pr: pr.clone() })
            .await
        {
            Err(ClientError::Server(e)) if e.message == BUSY => Ok(true),
            other => other.map(|_| false),
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
                eprintln!("Interrupted: nothing was drawn.");
            }
            std::process::exit(130);
        }
    };
    let harness = crate::harness::name_of(&mut client, pr).await;
    let mut titles: HashMap<String, String> = HashMap::new();
    let mut asked = std::collections::BTreeSet::new();
    loop {
        let (_, event) = tokio::select! {
            event = client.next_event() => event?,
            _ = interrupt.recv() => {
                stop(&mut client, pr, &mut interrupt).await;
                std::process::exit(130);
            }
        };
        match event {
            Event::DiagramsPlanned { pr: of, version } if of == *pr => {
                for entry in &version.entries {
                    titles.insert(entry.plan.key.clone(), entry.plan.title.clone());
                }
            }
            Event::DiagramsChanged { pr: of, state } if of == *pr => {
                begun |= matches!(
                    state,
                    DiagramsState::Planning | DiagramsState::Drawing { .. }
                );
                if !json && let Some(line) = progress_line(&state, &titles) {
                    eprintln!("{line}");
                }
                match state {
                    // An ending told before the run was seen is an older run's.
                    DiagramsState::Idle if begun => break,
                    DiagramsState::Failed { message } => {
                        return Err(CliError::Other(format!(
                            "the diagrams could not be planned: {}",
                            line_of(&message)
                        )));
                    }
                    _ => {}
                }
            }
            Event::PermissionRequested {
                pr: of,
                id,
                tool,
                summary,
                origin: TurnOrigin::Diagrams,
                ..
            } if of == *pr && asked.insert(id.clone()) && !json => {
                eprintln!(
                    "diagrams: waiting for an answer in the window: {harness} wants to {}: {}",
                    crate::agent::wants_to(&tool, &line_of(&tool)),
                    line_of(&summary)
                );
            }
            _ => {}
        }
    }
    finish(&listing(&mut client, pr).await?, json)
}

/// Asks the daemon to stop the drawing, given up on at a second Ctrl-C or after a few seconds.
async fn stop(client: &mut clusia_protocol::Client, pr: &PrRef, interrupt: &mut Signal) {
    let stopped = tokio::select! {
        done = client.request(Request::StopDiagrams { pr: pr.clone() }) => done.is_ok(),
        _ = interrupt.recv() => false,
        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => false,
    };
    if stopped {
        eprintln!("Stopped the drawing.");
    } else {
        eprintln!("The daemon did not answer; the drawing may still run (Stop in the window).");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::diagram::{DiagramEntry, DiagramKind, PlanEntry, VersionPhase};

    fn version(entries: Vec<(&str, EntryState)>) -> DiagramVersion {
        DiagramVersion {
            number: 2,
            commit: "a1b2c3d4e5f6".repeat(3),
            harness: "Claude Code".into(),
            drawn_at: 1,
            phase: VersionPhase::Done,
            entries: entries
                .into_iter()
                .map(|(key, state)| DiagramEntry {
                    plan: PlanEntry {
                        key: key.into(),
                        kind: DiagramKind::EntityRelationship,
                        title: format!("The {key}\u{1b}[2J"),
                        purpose: "p".into(),
                    },
                    state,
                })
                .collect(),
        }
    }

    #[test]
    fn a_listing_has_a_line_for_each_entry_then_the_version() {
        let ready = EntryState::Ready {
            spec: serde_json::from_value(json!({
                "key": "flow", "kind": "flow", "title": "t", "caption": "c",
                "nodes": [{"id": "a", "label": "A", "shape": "box"}]
            }))
            .unwrap(),
            notes: vec!["1 arrow dropped".into()],
        };
        let v = version(vec![
            ("flow", ready),
            (
                "graph",
                EntryState::Failed {
                    reason: "no\nboxes".into(),
                },
            ),
            ("states", EntryState::NotDrawn),
            ("sequence", EntryState::Waiting),
        ]);
        let lines = list_lines(Some(&v), false, "a1b2c3d4e5f6", None);
        assert_eq!(
            lines,
            [
                "flow  Entity–relationship  The flow\\u{1b}[2J  ready · 1 noted",
                "graph  Entity–relationship  The graph\\u{1b}[2J  failed · no boxes",
                "states  Entity–relationship  The states\\u{1b}[2J  not drawn",
                "sequence  Entity–relationship  The sequence\\u{1b}[2J  waiting",
                "Version 2 · drawn by Claude Code from a1b2c3d",
            ]
        );
        assert!(lines.iter().all(|l| !l.contains('\u{1b}')));
        assert_eq!(failed_count(&v), 1);
    }

    #[test]
    fn a_stale_listing_says_so_with_the_reason_when_there_is_one() {
        let v = version(vec![("flow", EntryState::NotDrawn)]);
        let lines = list_lines(Some(&v), true, &"f".repeat(40), Some("Same\nfiles"));
        assert_eq!(
            lines.last().unwrap(),
            "Out of date: the pull request is at fffffff now. Draw again with: clusia diagrams <pr> --draw (Same files)"
        );
        assert!(
            !list_lines(Some(&v), true, "ffffffff", None)
                .last()
                .unwrap()
                .contains('(')
        );
        assert_eq!(
            list_lines(None, false, "", None),
            ["No diagrams yet. Draw them with: clusia diagrams <pr> --draw"]
        );
    }

    #[test]
    fn progress_names_the_diagram_being_drawn() {
        let titles = HashMap::from([("flow".to_string(), "Token\nrefresh".to_string())]);
        assert_eq!(
            progress_line(
                &DiagramsState::Drawing {
                    done: 1,
                    total: 5,
                    key: "flow".into()
                },
                &titles
            )
            .as_deref(),
            Some("drawing 2 of 5: Token refresh")
        );
        assert_eq!(
            progress_line(
                &DiagramsState::Drawing {
                    done: 0,
                    total: 1,
                    key: "x".into()
                },
                &titles
            )
            .as_deref(),
            Some("drawing 1 of 1: x")
        );
        assert_eq!(
            progress_line(&DiagramsState::Planning, &titles).as_deref(),
            Some("planning the diagrams")
        );
        assert_eq!(progress_line(&DiagramsState::Idle, &titles), None);
    }
}
