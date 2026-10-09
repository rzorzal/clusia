//! `clusia ask`, `clusia agent log` and `clusia agent stop`.

use std::io::{self, Write};
use std::path::Path;

use clusia_core::{Paths, PrRef};
use clusia_protocol::{
    AgentErrorKind, AgentLogEntry, ClientError, Command as Request, ErrorCode, Event,
    PermissionOutcome, Reply, SessionStateKind, Suggestion, topics,
};
use serde_json::json;
use tokio::signal::unix::{SignalKind, signal};

use crate::review::parse_pr;
use crate::run::{CliError, Output, connect, unexpected};

/// What `Turn::feed` says about the stream.
pub(crate) enum Flow {
    More,
    Finished,
}

/// One question's turn: filters the daemon's events down to this review and this turn, prints
/// them, and remembers the suggestions and the failure for the end.
pub(crate) struct Turn {
    pr: PrRef,
    turn: u64,
    json: bool,
    suggestions: Vec<Suggestion>,
    failure: Option<String>,
    wrote: bool,
    at_line_start: bool,
    said_waiting: bool,
}

impl Turn {
    pub(crate) fn new(pr: PrRef, turn: u64, json: bool) -> Self {
        Self {
            pr,
            turn,
            json,
            suggestions: Vec::new(),
            failure: None,
            wrote: false,
            at_line_start: true,
            said_waiting: false,
        }
    }

    pub(crate) fn pr(&self) -> &PrRef {
        &self.pr
    }

    pub(crate) fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Prints what `event` adds: the answer's text to `out`, tool and denied lines to `err`
    /// (or, with `--json`, the event itself as one line on `out`).
    pub(crate) fn feed(
        &mut self,
        event: &Event,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> io::Result<Flow> {
        if let Event::SessionState {
            pr,
            state: SessionStateKind::Queued,
        } = event
            && *pr == self.pr
            && !self.json
            && !self.said_waiting
        {
            self.said_waiting = true;
            writeln!(
                err,
                "waiting for its turn: another question is still running…"
            )?;
        }
        let ours = match event {
            Event::AgentChunk { pr, turn, .. }
            | Event::AgentToolUse { pr, turn, .. }
            | Event::AgentDenied { pr, turn, .. }
            | Event::AgentSuggestion { pr, turn, .. }
            | Event::AgentDone { pr, turn, .. }
            | Event::AgentError { pr, turn, .. } => *pr == self.pr && *turn == self.turn,
            _ => false,
        };
        if !ours {
            return Ok(Flow::More);
        }
        if self.json {
            writeln!(out, "{}", serde_json::to_string(event).unwrap_or_default())?;
            out.flush()?;
        }
        match event {
            Event::AgentChunk { text, .. } if !self.json => {
                out.write_all(text.as_bytes())?;
                out.flush()?;
                if !text.is_empty() {
                    self.wrote = true;
                    self.at_line_start = text.ends_with('\n');
                }
            }
            Event::AgentToolUse { summary, .. } if !self.json => {
                writeln!(err, "✓ {summary}")?;
            }
            Event::AgentDenied { tool, detail, .. } if !self.json => {
                writeln!(err, "⊘ wanted to use {tool}: {detail} (denied)")?;
            }
            Event::AgentSuggestion { suggestion, .. } => {
                self.suggestions.push(suggestion.clone());
            }
            Event::AgentDone { .. } => return Ok(Flow::Finished),
            Event::AgentError { kind, message, .. } => {
                self.failure = Some(failure_message(*kind, message));
                return Ok(Flow::Finished);
            }
            _ => {}
        }
        Ok(Flow::More)
    }

    /// Ends the last line of the answer and lists the suggestions (not with `--json`, where
    /// they already went by as events).
    pub(crate) fn finish(&self, out: &mut dyn Write) -> io::Result<()> {
        if self.json {
            return Ok(());
        }
        if self.wrote && !self.at_line_start {
            writeln!(out)?;
        }
        if !self.suggestions.is_empty() {
            writeln!(out, "\nSuggested comments:")?;
            for s in &self.suggestions {
                writeln!(out, "{}", suggestion_line(s))?;
            }
        }
        out.flush()
    }
}

fn failure_message(kind: AgentErrorKind, message: &str) -> String {
    if message.trim().is_empty() {
        kind.to_string()
    } else {
        message.to_string()
    }
}

/// `file:line  body`, `file:3-5  body` for a range, `file  body` for none; the body on one line.
pub(crate) fn suggestion_line(s: &Suggestion) -> String {
    let place = match (s.line, s.start_line, s.end_line) {
        (Some(line), _, _) => format!("{}:{line}", s.file),
        (None, Some(from), Some(to)) => format!("{}:{from}-{to}", s.file),
        _ => s.file.clone(),
    };
    let body = s.body.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{place}  {body}")
}

/// The chat as lines: consecutive text entries of one turn are one answer.
pub(crate) fn log_lines(entries: &[AgentLogEntry]) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut joining: Option<u64> = None;
    for entry in entries {
        if let AgentLogEntry::Text { turn, text, .. } = entry {
            match (joining, lines.last_mut()) {
                (Some(open), Some(last)) if open == *turn => last.push_str(text),
                _ => lines.push(text.clone()),
            }
            joining = Some(*turn);
            continue;
        }
        joining = None;
        lines.push(match entry {
            AgentLogEntry::User { text, .. } => format!("you: {text}"),
            AgentLogEntry::ToolUse { summary, .. } => format!("  ✓ {summary}"),
            AgentLogEntry::Denied { tool, detail, .. } => {
                format!("  ⊘ wanted to use {tool}: {detail}")
            }
            AgentLogEntry::Suggestion { suggestion, .. } => {
                format!("  suggestion {}", suggestion_line(suggestion))
            }
            AgentLogEntry::Done { duration_ms, .. } => {
                format!("  done in {:.1} s", *duration_ms as f64 / 1000.0)
            }
            AgentLogEntry::Error { kind, message, .. } => {
                format!("  error: {}", failure_message(*kind, message))
            }
            AgentLogEntry::Permission {
                tool,
                summary,
                outcome,
                ..
            } => permission_line(tool, summary, *outcome),
            AgentLogEntry::Text { .. } => unreachable!("handled above"),
        });
    }
    lines
}

/// How the chat reports a request that needed the reviewer: what was done to what, and who
/// decided.
pub(crate) fn permission_line(tool: &str, summary: &str, outcome: PermissionOutcome) -> String {
    let verb = match tool {
        "Bash" => "ran",
        "Edit" | "MultiEdit" | "NotebookEdit" => "edited",
        "Write" => "wrote",
        _ => "used",
    };
    match outcome {
        PermissionOutcome::Allowed => format!("  ✓ {verb} {summary} (you allowed it)"),
        PermissionOutcome::AllowedForReview => {
            format!("  ✓ {verb} {summary} (allowed for this review)")
        }
        PermissionOutcome::Denied => format!("  ⊘ you denied {summary}"),
        PermissionOutcome::Expired => format!("  ⊘ denied {summary}: no answer in time"),
        PermissionOutcome::Cancelled => format!("  ⊘ {summary} was not run: the turn ended"),
    }
}

fn io_error(e: io::Error) -> CliError {
    CliError::Other(e.to_string())
}

/// The turn runs in the daemon: Ctrl-C only stops watching it.
fn detach(pr: &PrRef) -> ! {
    eprintln!(
        "Detached: the agent keeps working on this turn. Stop it with: clusia agent stop {pr}"
    );
    std::process::exit(130);
}

pub(crate) async fn ask(
    paths: &Paths,
    home: Option<&Path>,
    pr: &str,
    text: &[String],
    json: bool,
) -> Result<Output, CliError> {
    let pr = parse_pr(pr)?;
    let question = text.join(" ");
    if question.trim().is_empty() {
        return Err(CliError::Other("a question is required".into()));
    }
    // Caught from here on: a Ctrl-C that came before the first poll of a handler would end the
    // process without a word.
    let mut interrupt = signal(SignalKind::interrupt()).map_err(io_error)?;
    let mut client = connect(paths, home).await?;
    let opened = async {
        client
            .request(Request::Subscribe {
                topics: vec![topics::AGENT.into()],
            })
            .await?;
        // The agent reads the review's worktree, which opening prepares.
        client.request(Request::OpenReview { pr: pr.clone() }).await
    };
    tokio::select! {
        opened = opened => { opened?; }
        _ = interrupt.recv() => {
            eprintln!("Interrupted: nothing was asked.");
            std::process::exit(130);
        }
    }
    let sent = tokio::select! {
        sent = client.request(Request::AgentSend { pr: pr.clone(), text: question }) => sent,
        _ = interrupt.recv() => detach(&pr),
    };
    let turn = match sent {
        Ok(Reply::AgentTurn { turn }) => turn,
        Ok(other) => return Err(unexpected(other)),
        Err(ClientError::Server(e)) if e.code == ErrorCode::Busy => {
            return Err(CliError::Other(format!(
                "The agent is busy with another question. Wait for it, or run clusia agent stop {pr}."
            )));
        }
        Err(e) => return Err(e.into()),
    };
    let mut run = Turn::new(pr, turn, json);
    let (mut out, mut err) = (io::stdout(), io::stderr());
    loop {
        let (_, event) = tokio::select! {
            event = client.next_event() => event?,
            _ = interrupt.recv() => detach(run.pr()),
        };
        if let Flow::Finished = run.feed(&event, &mut out, &mut err).map_err(io_error)? {
            break;
        }
    }
    run.finish(&mut out).map_err(io_error)?;
    match run.failure() {
        Some(message) => Err(CliError::Other(message.to_string())),
        None => Ok(Output {
            human: String::new(),
            json: serde_json::Value::Null,
        }),
    }
}

pub(crate) async fn log(paths: &Paths, home: Option<&Path>, pr: &str) -> Result<Output, CliError> {
    let pr = parse_pr(pr)?;
    let mut client = connect(paths, home).await?;
    match client
        .request(Request::GetAgentLog { pr: pr.clone() })
        .await?
    {
        Reply::AgentLog(entries) => Ok(Output {
            human: if entries.is_empty() {
                format!("No agent log for {pr}")
            } else {
                log_lines(&entries).join("\n")
            },
            json: serde_json::to_value(&entries).unwrap_or_default(),
        }),
        other => Err(unexpected(other)),
    }
}

pub(crate) async fn stop(paths: &Paths, home: Option<&Path>, pr: &str) -> Result<Output, CliError> {
    let pr = parse_pr(pr)?;
    let mut client = connect(paths, home).await?;
    match client.request(Request::AgentCancel { pr }).await? {
        Reply::Ack => Ok(Output {
            human: "Asked the agent to stop".into(),
            json: json!({ "stopped": true }),
        }),
        other => Err(unexpected(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::PrRef;
    use clusia_protocol::{AgentErrorKind, AgentLogEntry, Event, Suggestion};

    fn pr() -> PrRef {
        "acme/widgets#7".parse().unwrap()
    }

    fn suggestion(line: Option<u32>, range: Option<(u32, u32)>, body: &str) -> Suggestion {
        Suggestion {
            id: "sug-0123456789ab".into(),
            file: "src/auth/refresh.rs".into(),
            line,
            start_line: range.map(|r| r.0),
            end_line: range.map(|r| r.1),
            body: body.into(),
        }
    }

    fn run(json: bool) -> (Turn, Vec<u8>, Vec<u8>) {
        (Turn::new(pr(), 2, json), Vec::new(), Vec::new())
    }

    fn chunk(turn: u64, text: &str) -> Event {
        Event::AgentChunk {
            pr: pr(),
            turn,
            text: text.into(),
        }
    }

    #[test]
    fn suggestion_lines_name_the_place_and_flatten_the_body() {
        assert_eq!(
            suggestion_line(&suggestion(
                Some(44),
                None,
                "Re-check `expires_at`\nafter the lock."
            )),
            "src/auth/refresh.rs:44  Re-check `expires_at` after the lock."
        );
        assert_eq!(
            suggestion_line(&suggestion(None, Some((3, 5)), "Extract this.")),
            "src/auth/refresh.rs:3-5  Extract this."
        );
        assert_eq!(
            suggestion_line(&suggestion(None, None, "A general remark.")),
            "src/auth/refresh.rs  A general remark."
        );
    }

    #[test]
    fn a_turn_shows_only_its_own_events() {
        let (mut turn, mut out, mut err) = run(false);
        turn.feed(&chunk(1, "the summary"), &mut out, &mut err)
            .unwrap();
        turn.feed(&chunk(2, "The lock "), &mut out, &mut err)
            .unwrap();
        turn.feed(&chunk(2, "is needed."), &mut out, &mut err)
            .unwrap();
        let other: PrRef = "acme/widgets#8".parse().unwrap();
        turn.feed(
            &Event::AgentChunk {
                pr: other,
                turn: 2,
                text: "another review".into(),
            },
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "The lock is needed.");
        assert!(err.is_empty());
    }

    #[test]
    fn a_queued_question_says_it_waits_once() {
        let (mut turn, mut out, mut err) = run(false);
        let queued = Event::SessionState {
            pr: pr(),
            state: clusia_protocol::SessionStateKind::Queued,
        };
        turn.feed(&queued, &mut out, &mut err).unwrap();
        turn.feed(&queued, &mut out, &mut err).unwrap();
        let other = Event::SessionState {
            pr: "acme/widgets#8".parse().unwrap(),
            state: clusia_protocol::SessionStateKind::Queued,
        };
        turn.feed(&other, &mut out, &mut err).unwrap();
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "waiting for its turn: another question is still running…\n"
        );
        assert!(out.is_empty());
        let (mut json, mut out, mut err) = run(true);
        json.feed(&queued, &mut out, &mut err).unwrap();
        assert!(
            out.is_empty() && err.is_empty(),
            "nothing extra in JSON mode"
        );
    }

    #[test]
    fn tools_and_denials_go_to_stderr_and_suggestions_wait_for_the_end() {
        let (mut turn, mut out, mut err) = run(false);
        let events = [
            Event::AgentToolUse {
                pr: pr(),
                turn: 2,
                summary: "Read src/auth/store.rs".into(),
            },
            Event::AgentDenied {
                pr: pr(),
                turn: 2,
                tool: "Bash".into(),
                detail: "cargo test".into(),
            },
            chunk(2, "Done reading."),
            Event::AgentSuggestion {
                pr: pr(),
                turn: 2,
                suggestion: suggestion(Some(44), None, "Re-check after the lock."),
            },
        ];
        for e in &events {
            assert!(matches!(
                turn.feed(e, &mut out, &mut err).unwrap(),
                Flow::More
            ));
        }
        let done = Event::AgentDone {
            pr: pr(),
            turn: 2,
            duration_ms: 1800,
        };
        assert!(matches!(
            turn.feed(&done, &mut out, &mut err).unwrap(),
            Flow::Finished
        ));
        turn.finish(&mut out).unwrap();
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "✓ Read src/auth/store.rs\n⊘ wanted to use Bash: cargo test (denied)\n"
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Done reading.\n\nSuggested comments:\nsrc/auth/refresh.rs:44  Re-check after the lock.\n"
        );
        assert_eq!(turn.failure(), None);
    }

    #[test]
    fn an_error_ends_the_turn_and_is_the_failure() {
        let (mut turn, mut out, mut err) = run(false);
        turn.feed(&chunk(2, "half an ans"), &mut out, &mut err)
            .unwrap();
        let error = Event::AgentError {
            pr: pr(),
            turn: 2,
            kind: AgentErrorKind::Interrupted,
            message: String::new(),
        };
        assert!(matches!(
            turn.feed(&error, &mut out, &mut err).unwrap(),
            Flow::Finished
        ));
        turn.finish(&mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "half an ans\n");
        assert_eq!(turn.failure(), Some("The turn was interrupted"));
    }

    #[test]
    fn json_mode_prints_the_wire_form_of_the_turns_events_only() {
        let (mut turn, mut out, mut err) = run(true);
        turn.feed(&chunk(1, "not mine"), &mut out, &mut err)
            .unwrap();
        turn.feed(&chunk(2, "mine"), &mut out, &mut err).unwrap();
        turn.feed(
            &Event::AgentToolUse {
                pr: pr(),
                turn: 2,
                summary: "Read a.rs".into(),
            },
            &mut out,
            &mut err,
        )
        .unwrap();
        turn.finish(&mut out).unwrap();
        let lines: Vec<serde_json::Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["agent_chunk"]["text"], "mine");
        assert_eq!(lines[1]["agent_tool_use"]["summary"], "Read a.rs");
        assert!(err.is_empty(), "nothing else is printed in JSON mode");
    }

    #[test]
    fn permission_lines_say_who_decided() {
        let line = |tool: &str, summary: &str, outcome| {
            log_lines(&[AgentLogEntry::Permission {
                at: 1,
                turn: 1,
                tool: tool.into(),
                summary: summary.into(),
                outcome,
            }])
            .remove(0)
        };
        let bash = |outcome| line("Bash", "cargo test", outcome);
        assert_eq!(
            bash(PermissionOutcome::Allowed),
            "  ✓ ran cargo test (you allowed it)"
        );
        assert_eq!(
            bash(PermissionOutcome::AllowedForReview),
            "  ✓ ran cargo test (allowed for this review)"
        );
        assert_eq!(bash(PermissionOutcome::Denied), "  ⊘ you denied cargo test");
        assert_eq!(
            bash(PermissionOutcome::Expired),
            "  ⊘ denied cargo test: no answer in time"
        );
        assert_eq!(
            bash(PermissionOutcome::Cancelled),
            "  ⊘ cargo test was not run: the turn ended"
        );
    }

    #[test]
    fn permission_lines_name_the_verb_of_the_tool() {
        let allowed = |tool, summary| permission_line(tool, summary, PermissionOutcome::Allowed);
        assert_eq!(
            allowed("Edit", "src/a.rs"),
            "  ✓ edited src/a.rs (you allowed it)"
        );
        assert_eq!(
            allowed("MultiEdit", "src/a.rs"),
            "  ✓ edited src/a.rs (you allowed it)"
        );
        assert_eq!(
            allowed("NotebookEdit", "n.ipynb"),
            "  ✓ edited n.ipynb (you allowed it)"
        );
        assert_eq!(
            allowed("Write", "notes.md"),
            "  ✓ wrote notes.md (you allowed it)"
        );
        assert_eq!(
            allowed("WebFetch", "WebFetch"),
            "  ✓ used WebFetch (you allowed it)"
        );
    }

    #[test]
    fn the_log_reads_like_a_chat() {
        let entries = vec![
            AgentLogEntry::User {
                at: 1,
                turn: 1,
                text: "Is the lock needed?".into(),
            },
            AgentLogEntry::ToolUse {
                at: 2,
                turn: 1,
                summary: "Read src/auth/store.rs".into(),
            },
            AgentLogEntry::Text {
                at: 3,
                turn: 1,
                text: "The lock ".into(),
            },
            AgentLogEntry::Text {
                at: 4,
                turn: 1,
                text: "is needed.".into(),
            },
            AgentLogEntry::Denied {
                at: 5,
                turn: 1,
                tool: "Bash".into(),
                detail: "cargo test".into(),
            },
            AgentLogEntry::Suggestion {
                at: 6,
                turn: 1,
                suggestion: suggestion(Some(44), None, "Re-check."),
            },
            AgentLogEntry::Done {
                at: 7,
                turn: 1,
                duration_ms: 1800,
            },
            AgentLogEntry::User {
                at: 8,
                turn: 2,
                text: "And the timeout?".into(),
            },
            AgentLogEntry::Text {
                at: 9,
                turn: 2,
                text: "Half".into(),
            },
            AgentLogEntry::Error {
                at: 10,
                turn: 2,
                kind: AgentErrorKind::Interrupted,
                message: "The turn was interrupted".into(),
            },
        ];
        assert_eq!(
            log_lines(&entries),
            [
                "you: Is the lock needed?",
                "  ✓ Read src/auth/store.rs",
                "The lock is needed.",
                "  ⊘ wanted to use Bash: cargo test",
                "  suggestion src/auth/refresh.rs:44  Re-check.",
                "  done in 1.8 s",
                "you: And the timeout?",
                "Half",
                "  error: The turn was interrupted",
            ]
        );
    }
}
