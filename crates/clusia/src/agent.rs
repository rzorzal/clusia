//! `clusia ask`, `clusia agent log` and `clusia agent stop`.

use std::io::{self, IsTerminal, Write};
use std::path::Path;

use clusia_core::{Paths, PrRef};
use clusia_protocol::{
    AgentErrorKind, AgentLogEntry, Client, ClientError, Command as Request, ErrorCode, Event,
    PermissionAnswerKind, PermissionOutcome, Reply, SessionStateKind, Suggestion, topics,
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

/// A permission request the agent waits on, as the terminal shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Waiting {
    id: String,
    tool: String,
    summary: String,
    /// What `[r]eview` would allow; `None` offers only `[o]nce` and `[d]eny`.
    prefix: Option<String>,
    /// An answer is on its way: the daemon's `PermissionResolved` has not come yet.
    answered: bool,
}

impl Waiting {
    /// What the agent wants to do: `run`, `edit`, `write`.
    fn verb(&self) -> &'static str {
        match self.tool.as_str() {
            "Bash" => "run",
            "Edit" | "MultiEdit" | "NotebookEdit" => "edit",
            "Write" => "write",
            _ => "use",
        }
    }

    /// The question on a terminal, ending where the answer is typed.
    fn prompt(&self) -> String {
        let choices = match &self.prefix {
            Some(prefix) => format!("[o]nce / [r]eview ({}) / [d]eny? ", printable(prefix)),
            None => "[o]nce / [d]eny? ".to_string(),
        };
        format!(
            "Claude Code wants to {}: {}  {choices}",
            self.verb(),
            printable(&self.summary)
        )
    }

    /// What is said when nobody can answer here: the window or the notification does.
    fn elsewhere(&self) -> String {
        format!(
            "waiting for an answer in the window: Claude Code wants to {}: {}",
            self.verb(),
            printable(&self.summary)
        )
    }
}

/// Characters that draw nothing or reorder what is drawn: Unicode categories Cf, Zl and Zp
/// (bidi controls, zero-width characters, the BOM, tags, line and paragraph separators).
/// `char` has no category lookup, so the Cf ranges are listed.
fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{AD}' | '\u{600}'..='\u{605}' | '\u{61C}' | '\u{6DD}' | '\u{70F}'
        | '\u{890}'..='\u{891}' | '\u{8E2}' | '\u{180E}' | '\u{200B}'..='\u{200F}'
        | '\u{2028}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{FEFF}'
        | '\u{FFF9}'..='\u{FFFB}' | '\u{110BD}' | '\u{110CD}' | '\u{13430}'..='\u{1343F}'
        | '\u{1BCA0}'..='\u{1BCA3}' | '\u{1D173}'..='\u{1D17A}' | '\u{E0000}'..='\u{E007F}')
}

/// `text` as one safe line: a control character (a carriage return, an escape sequence) or an
/// invisible one (a bidi override, a zero-width character) would let a command rewrite or
/// disguise what the reviewer reads, so each is shown as an escape.
pub(crate) fn printable(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() || is_invisible(c) {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// The agent's streamed text without what could repaint the terminal or disguise it: it keeps
/// line breaks and tabs and drops every other control and invisible character.
pub(crate) fn streamed(text: &str) -> String {
    text.chars()
        .filter(|&c| c == '\n' || c == '\t' || !(c.is_control() || is_invisible(c)))
        .collect()
}

/// A line typed this soon after a question is shown is not an answer to it: it was aimed at
/// whatever was asked before, or typed before the question was seen.
pub(crate) const QUIET_AFTER_PROMPT: std::time::Duration = std::time::Duration::from_millis(300);

/// What a typed line means for a request. Only a clear yes allows: an empty line, a typo and
/// `[r]eview` for a request that offers no prefix all deny.
fn parse_answer(line: &str, has_prefix: bool) -> PermissionAnswerKind {
    match line.trim().to_lowercase().as_str() {
        "o" | "once" => PermissionAnswerKind::Once,
        "r" | "review" if has_prefix => PermissionAnswerKind::Review,
        _ => PermissionAnswerKind::Deny,
    }
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
    /// Whether this terminal answers permission requests (a TTY, and not `--json`).
    interactive: bool,
    /// The requests that wait for an answer, oldest first: the first one is being asked.
    waiting: Vec<Waiting>,
    /// The review's turn is the one that runs: what it decides at once may be told here.
    running: bool,
    /// The question is on the screen and its line has not been ended yet.
    prompt_open: bool,
    /// When the question now on the screen was printed.
    prompted_at: Option<std::time::Instant>,
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
            interactive: false,
            waiting: Vec::new(),
            running: false,
            prompt_open: false,
            prompted_at: None,
        }
    }

    /// Lets this terminal answer permission requests.
    pub(crate) fn interactive(mut self, yes: bool) -> Self {
        self.interactive = yes && !self.json;
        self
    }

    /// Whether a question is open on the terminal and its answer is still to be typed.
    pub(crate) fn asking(&self) -> bool {
        self.interactive && self.waiting.first().is_some_and(|w| !w.answered)
    }

    /// Whether the question was printed so recently that a line typed now cannot be its answer.
    pub(crate) fn just_asked(&self) -> bool {
        self.prompted_at
            .is_some_and(|at| at.elapsed() < QUIET_AFTER_PROMPT)
    }

    /// Notes that the question's line has been ended by something other than an answer.
    fn end_prompt_line(&mut self) {
        self.prompt_open = false;
    }

    /// Prints the question of the first waiting request.
    fn show_prompt(&mut self, err: &mut dyn Write) -> io::Result<()> {
        if let Some(front) = self.waiting.first() {
            write!(err, "{}", front.prompt())?;
            err.flush()?;
            self.prompt_open = true;
            self.prompted_at = Some(std::time::Instant::now());
        }
        Ok(())
    }

    /// A line typed at the prompt: the answer it gives to the question that is open, or `None`
    /// when none is. The request stays until the daemon says it ended.
    pub(crate) fn answer_line(&mut self, line: &str) -> Option<(String, PermissionAnswerKind)> {
        if !self.interactive {
            return None;
        }
        let front = self.waiting.first_mut().filter(|w| !w.answered)?;
        front.answered = true;
        // The reviewer's Enter ended the question's line.
        self.prompt_open = false;
        Some((front.id.clone(), parse_answer(line, front.prefix.is_some())))
    }

    /// A line typed on the terminal: the answer it gives, as `answer_line`. A line that comes
    /// too soon after the question was shown answers nothing; while that question is still open
    /// the reviewer is told and it is asked again, which starts its quiet moment over.
    pub(crate) fn typed(
        &mut self,
        line: &str,
        err: &mut dyn Write,
    ) -> io::Result<Option<(String, PermissionAnswerKind)>> {
        if !self.just_asked() {
            return Ok(self.answer_line(line));
        }
        if self.asking() {
            writeln!(
                err,
                "\n(answer ignored: typed before the question was shown)"
            )?;
            self.show_prompt(err)?;
        }
        Ok(None)
    }

    /// Whether `event` ends the request that is asked now.
    pub(crate) fn ends_the_question(&self, event: &Event) -> bool {
        matches!(event, Event::PermissionResolved { id, .. }
            if self.waiting.first().is_some_and(|w| w.id == *id))
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
        if let Event::SessionState {
            pr,
            state: SessionStateKind::Running,
        } = event
            && *pr == self.pr
        {
            self.running = true;
        }
        let ours = match event {
            Event::PermissionRequested { pr, turn, .. } => *pr == self.pr && *turn == self.turn,
            // What a rule or the worktree decides at once has no request of ours to match: it
            // belongs to this turn once the review's turn is the one that runs.
            Event::PermissionResolved { pr, .. } => *pr == self.pr && self.running,
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
            Event::PermissionRequested {
                id,
                tool,
                summary,
                prefix,
                ..
            } => {
                self.running = true;
                self.waiting.push(Waiting {
                    id: id.clone(),
                    tool: tool.clone(),
                    summary: summary.clone(),
                    prefix: prefix.clone(),
                    answered: false,
                });
                if !self.json {
                    if !self.interactive {
                        let said = self.waiting.last().map(Waiting::elsewhere);
                        writeln!(err, "{}", said.unwrap_or_default())?;
                    } else if self.waiting.len() == 1 {
                        self.show_prompt(err)?;
                    }
                }
            }
            Event::PermissionResolved {
                id,
                tool,
                summary,
                outcome,
                ..
            } => {
                let was_front = self.waiting.first().is_some_and(|w| w.id == *id);
                self.waiting.retain(|w| w.id != *id);
                if !self.json {
                    // A question whose line is still open ends first; an answer typed here (or
                    // the note that input ended) already ended it.
                    if self.interactive && was_front && self.prompt_open {
                        writeln!(err)?;
                        self.end_prompt_line();
                    }
                    writeln!(
                        err,
                        "{}",
                        permission_line(tool, summary, *outcome).trim_start()
                    )?;
                    if self.interactive && was_front {
                        self.show_prompt(err)?;
                    }
                }
            }
            Event::AgentChunk { text, .. } if !self.json => {
                let text = streamed(text);
                out.write_all(text.as_bytes())?;
                out.flush()?;
                if !text.is_empty() {
                    self.wrote = true;
                    self.at_line_start = text.ends_with('\n');
                }
            }
            Event::AgentToolUse { summary, .. } if !self.json => {
                writeln!(err, "✓ {}", printable(summary))?;
            }
            Event::AgentDenied { tool, detail, .. } if !self.json => {
                writeln!(
                    err,
                    "⊘ wanted to use {}: {} (denied)",
                    printable(tool),
                    printable(detail)
                )?;
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
        printable(message)
    }
}

/// `file:line  body`, `file:3-5  body` for a range, `file  body` for none; the body on one line.
pub(crate) fn suggestion_line(s: &Suggestion) -> String {
    let place = match (s.line, s.start_line, s.end_line) {
        (Some(line), _, _) => format!("{}:{line}", printable(&s.file)),
        (None, Some(from), Some(to)) => format!("{}:{from}-{to}", printable(&s.file)),
        _ => printable(&s.file),
    };
    let body = s.body.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("{place}  {}", printable(&body))
}

/// The chat as lines: consecutive text entries of one turn are one answer.
pub(crate) fn log_lines(entries: &[AgentLogEntry]) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut joining: Option<u64> = None;
    for entry in entries {
        if let AgentLogEntry::Text { turn, text, .. } = entry {
            match (joining, lines.last_mut()) {
                (Some(open), Some(last)) if open == *turn => last.push_str(&streamed(text)),
                _ => lines.push(streamed(text)),
            }
            joining = Some(*turn);
            continue;
        }
        joining = None;
        lines.push(match entry {
            AgentLogEntry::User { text, .. } => format!("you: {}", printable(text)),
            AgentLogEntry::ToolUse { summary, .. } => format!("  ✓ {}", printable(summary)),
            AgentLogEntry::Denied { tool, detail, .. } => {
                format!(
                    "  ⊘ wanted to use {}: {}",
                    printable(tool),
                    printable(detail)
                )
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
    let summary = &printable(summary);
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

/// The lines typed on the terminal, read on a thread of their own: a read cannot be given up,
/// and one left waiting must not keep the process from ending.
fn stdin_lines() -> tokio::sync::mpsc::UnboundedReceiver<String> {
    let (send, lines) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in io::stdin().lines() {
            let Ok(line) = line else { return };
            if send.send(line).is_err() {
                return;
            }
        }
    });
    lines
}

/// Drops the lines already typed.
fn discard_typed(typed: &mut Option<tokio::sync::mpsc::UnboundedReceiver<String>>) {
    if let Some(lines) = typed {
        while lines.try_recv().is_ok() {}
    }
}

/// The next typed line; with no terminal, or once it is closed, never.
async fn typed_line(
    typed: &mut Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
) -> Option<String> {
    match typed {
        Some(lines) => lines.recv().await,
        None => std::future::pending().await,
    }
}

/// Says once that a question is open on a terminal whose input has ended: nothing can answer
/// it, so it will be denied when its time runs out.
fn note_end_of_input(
    run: &mut Turn,
    typed: &Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
    noted: &mut bool,
    err: &mut dyn Write,
) -> io::Result<()> {
    if typed.is_none() && run.asking() && !*noted {
        *noted = true;
        writeln!(
            err,
            "\nno more input: the request is denied when its time runs out"
        )?;
        run.end_prompt_line();
    }
    Ok(())
}

/// Sends the reviewer's answer. A request that already ended (answered in the window, or out
/// of time) is not an error: its ending is on its way as an event.
async fn send_answer(
    client: &mut Client,
    id: String,
    answer: PermissionAnswerKind,
) -> Result<(), CliError> {
    match client
        .request(Request::PermissionAnswer { id, answer })
        .await
    {
        Ok(_) => Ok(()),
        Err(ClientError::Server(e)) if e.code == ErrorCode::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
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
    let interactive = io::stdin().is_terminal() && !json;
    let mut run = Turn::new(pr, turn, json).interactive(interactive);
    let mut typed = interactive.then(stdin_lines);
    let mut noted_end_of_input = false;
    let (mut out, mut err) = (io::stdout(), io::stderr());
    loop {
        let (_, event) = tokio::select! {
            event = client.next_event() => event?,
            line = typed_line(&mut typed) => {
                match line {
                    Some(line) => {
                        if let Some((id, answer)) = run.typed(&line, &mut err).map_err(io_error)? {
                            send_answer(&mut client, id, answer).await?;
                        }
                    }
                    None => typed = None,
                }
                note_end_of_input(&mut run, &typed, &mut noted_end_of_input, &mut err)
                    .map_err(io_error)?;
                continue;
            }
            _ = interrupt.recv() => detach(run.pr()),
        };
        // What was typed before a question is shown is not its answer.
        if !run.asking() || run.ends_the_question(&event) {
            discard_typed(&mut typed);
        }
        if let Flow::Finished = run.feed(&event, &mut out, &mut err).map_err(io_error)? {
            break;
        }
        note_end_of_input(&mut run, &typed, &mut noted_end_of_input, &mut err).map_err(io_error)?;
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

    fn requested(id: &str, turn: u64, tool: &str, summary: &str, prefix: Option<&str>) -> Event {
        requested_in(pr(), id, turn, tool, summary, prefix)
    }

    fn requested_in(
        pr: PrRef,
        id: &str,
        turn: u64,
        tool: &str,
        summary: &str,
        prefix: Option<&str>,
    ) -> Event {
        Event::PermissionRequested {
            id: id.into(),
            pr,
            turn,
            tool: tool.into(),
            summary: summary.into(),
            reason: None,
            prefix: prefix.map(str::to_string),
            sandbox: true,
            deadline: 0,
            detail: None,
        }
    }

    fn resolved(id: &str, outcome: PermissionOutcome) -> Event {
        resolved_for(id, "Bash", "make", outcome)
    }

    fn resolved_for(id: &str, tool: &str, summary: &str, outcome: PermissionOutcome) -> Event {
        Event::PermissionResolved {
            id: id.into(),
            pr: pr(),
            tool: tool.into(),
            summary: summary.into(),
            outcome,
        }
    }

    fn said(bytes: &[u8]) -> String {
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn only_a_clear_yes_allows() {
        use PermissionAnswerKind::{Deny, Once, Review};
        for (line, prefix, want) in [
            ("o", true, Once),
            ("O\n", true, Once),
            (" once ", false, Once),
            ("r", true, Review),
            ("review", true, Review),
            ("r", false, Deny),
            ("d", true, Deny),
            ("deny", false, Deny),
            ("", true, Deny),
            ("yes please", true, Deny),
            ("oo", true, Deny),
        ] {
            assert_eq!(parse_answer(line, prefix), want, "{line:?}");
        }
    }

    #[test]
    fn without_a_terminal_it_says_where_to_answer() {
        let (mut turn, mut out, mut err) = run(false);
        let ask = requested(
            "p1",
            2,
            "Bash",
            "cargo test -p clusia-core",
            Some("cargo test"),
        );
        turn.feed(&ask, &mut out, &mut err).unwrap();
        assert_eq!(
            said(&err),
            "waiting for an answer in the window: Claude Code wants to run: cargo test -p clusia-core\n"
        );
        assert!(!turn.asking());
        assert_eq!(turn.answer_line("o"), None, "nothing to type at");
        err.clear();
        turn.feed(
            &resolved_for(
                "p1",
                "Bash",
                "cargo test -p clusia-core",
                PermissionOutcome::AllowedForReview,
            ),
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!(
            said(&err),
            "✓ ran cargo test -p clusia-core (allowed for this review)\n"
        );
        assert!(out.is_empty());
    }

    #[test]
    fn on_a_terminal_it_asks_and_takes_one_answer() {
        let (turn, mut out, mut err) = run(false);
        let mut turn = turn.interactive(true);
        let ask = requested(
            "p1",
            2,
            "Bash",
            "cargo test -p clusia-core",
            Some("cargo test"),
        );
        turn.feed(&ask, &mut out, &mut err).unwrap();
        assert_eq!(
            said(&err),
            "Claude Code wants to run: cargo test -p clusia-core  [o]nce / [r]eview (cargo test) / [d]eny? "
        );
        assert!(turn.asking());
        assert_eq!(
            turn.answer_line("r"),
            Some(("p1".into(), PermissionAnswerKind::Review))
        );
        assert!(!turn.asking(), "an answer is on its way");
        assert_eq!(turn.answer_line("d"), None, "the first line is the answer");
        err.clear();
        turn.feed(
            &resolved_for(
                "p1",
                "Bash",
                "cargo test -p clusia-core",
                PermissionOutcome::AllowedForReview,
            ),
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!(
            said(&err),
            "✓ ran cargo test -p clusia-core (allowed for this review)\n"
        );
    }

    #[test]
    fn a_request_with_no_prefix_offers_two_choices_and_review_denies() {
        let (turn, mut out, mut err) = run(false);
        let mut turn = turn.interactive(true);
        let ask = requested("p1", 2, "Bash", "cd src && cargo test", None);
        turn.feed(&ask, &mut out, &mut err).unwrap();
        assert_eq!(
            said(&err),
            "Claude Code wants to run: cd src && cargo test  [o]nce / [d]eny? "
        );
        assert_eq!(
            turn.answer_line("r"),
            Some(("p1".into(), PermissionAnswerKind::Deny))
        );
    }

    #[test]
    fn edits_and_writes_say_so() {
        let (turn, mut out, mut err) = run(false);
        let mut turn = turn.interactive(true);
        let edit = requested("p1", 2, "Edit", "src/a.rs", Some("Edit"));
        turn.feed(&edit, &mut out, &mut err).unwrap();
        assert!(said(&err).starts_with("Claude Code wants to edit: src/a.rs  "));
        turn.answer_line("o");
        let allowed = resolved_for("p1", "Edit", "src/a.rs", PermissionOutcome::Allowed);
        turn.feed(&allowed, &mut out, &mut err).unwrap();
        assert!(
            said(&err).ends_with("✓ edited src/a.rs (you allowed it)\n"),
            "{}",
            said(&err)
        );
        err.clear();
        let write = requested("p2", 2, "Write", "notes.md", None);
        turn.feed(&write, &mut out, &mut err).unwrap();
        assert!(said(&err).starts_with("Claude Code wants to write: notes.md  "));
    }

    #[test]
    fn requests_queue_and_the_next_one_is_asked_when_the_first_ends() {
        let (turn, mut out, mut err) = run(false);
        let mut turn = turn.interactive(true);
        let first = requested("p1", 2, "Bash", "make", None);
        let second = requested("p2", 2, "Bash", "make test", None);
        turn.feed(&first, &mut out, &mut err).unwrap();
        turn.feed(&second, &mut out, &mut err).unwrap();
        assert_eq!(
            said(&err),
            "Claude Code wants to run: make  [o]nce / [d]eny? ",
            "only the first is asked"
        );
        err.clear();
        // Answered in the window: the terminal moves on to the next question.
        let denied = resolved("p1", PermissionOutcome::Denied);
        turn.feed(&denied, &mut out, &mut err).unwrap();
        assert_eq!(
            said(&err),
            "\n⊘ you denied make\nClaude Code wants to run: make test  [o]nce / [d]eny? "
        );
        assert_eq!(
            turn.answer_line("o"),
            Some(("p2".into(), PermissionAnswerKind::Once))
        );
    }

    #[test]
    fn a_command_cannot_rewrite_the_question_or_its_ending() {
        let nasty = "make\r\u{1b}[2K";
        let shown = "make\\r\\u{1b}[2K";
        let (turn, mut out, mut err) = run(false);
        let mut turn = turn.interactive(true);
        let ask = requested("p1", 2, "Bash", nasty, Some("make\r"));
        turn.feed(&ask, &mut out, &mut err).unwrap();
        assert_eq!(
            said(&err),
            format!("Claude Code wants to run: {shown}  [o]nce / [r]eview (make\\r) / [d]eny? ")
        );
        err.clear();
        let done = resolved_for("p1", "Bash", nasty, PermissionOutcome::Denied);
        turn.feed(&done, &mut out, &mut err).unwrap();
        assert_eq!(said(&err), format!("\n⊘ you denied {shown}\n"));

        let (mut elsewhere, mut out, mut err) = run(false);
        elsewhere
            .feed(&requested("p2", 2, "Bash", nasty, None), &mut out, &mut err)
            .unwrap();
        assert_eq!(
            said(&err),
            format!("waiting for an answer in the window: Claude Code wants to run: {shown}\n")
        );
        let allowed = permission_line("Bash", nasty, PermissionOutcome::Allowed);
        assert_eq!(allowed, format!("  ✓ ran {shown} (you allowed it)"));
    }

    #[test]
    fn an_answer_typed_here_has_already_ended_the_prompt_line() {
        let (turn, mut out, mut err) = run(false);
        let mut turn = turn.interactive(true);
        turn.feed(
            &requested("p1", 2, "Bash", "make", None),
            &mut out,
            &mut err,
        )
        .unwrap();
        turn.answer_line("o");
        err.clear();
        let allowed = resolved("p1", PermissionOutcome::Allowed);
        turn.feed(&allowed, &mut out, &mut err).unwrap();
        assert_eq!(said(&err), "✓ ran make (you allowed it)\n");
    }

    #[test]
    fn the_ending_is_told_for_every_outcome() {
        for (outcome, line) in [
            (PermissionOutcome::Allowed, "✓ ran make (you allowed it)"),
            (PermissionOutcome::Denied, "⊘ you denied make"),
            (
                PermissionOutcome::Expired,
                "⊘ denied make: no answer in time",
            ),
            (
                PermissionOutcome::Cancelled,
                "⊘ make was not run: the turn ended",
            ),
        ] {
            let (mut turn, mut out, mut err) = run(false);
            let ask = requested("p1", 2, "Bash", "make", None);
            turn.feed(&ask, &mut out, &mut err).unwrap();
            err.clear();
            turn.feed(&resolved("p1", outcome), &mut out, &mut err)
                .unwrap();
            assert_eq!(said(&err), format!("{line}\n"));
        }
    }

    #[test]
    fn requests_of_other_turns_and_reviews_are_not_ours() {
        let (turn, mut out, mut err) = run(false);
        let mut turn = turn.interactive(true);
        let later_turn = requested("p1", 3, "Bash", "make", None);
        turn.feed(&later_turn, &mut out, &mut err).unwrap();
        let other_review = requested_in(
            "acme/widgets#9".parse().unwrap(),
            "p2",
            2,
            "Bash",
            "make",
            None,
        );
        turn.feed(&other_review, &mut out, &mut err).unwrap();
        let unknown = resolved("p1", PermissionOutcome::Denied);
        turn.feed(&unknown, &mut out, &mut err).unwrap();
        assert!(err.is_empty(), "{}", said(&err));
        assert!(!turn.asking());
    }

    #[test]
    fn what_is_decided_at_once_is_told_while_this_turn_runs() {
        let (mut turn, mut out, mut err) = run(false);
        let covered = resolved_for(
            "p9",
            "Bash",
            "cargo test --workspace",
            PermissionOutcome::AllowedForReview,
        );
        // Another turn of the review may be the one running: nothing is said yet.
        turn.feed(&covered, &mut out, &mut err).unwrap();
        assert!(err.is_empty());
        let running = Event::SessionState {
            pr: pr(),
            state: SessionStateKind::Running,
        };
        turn.feed(&running, &mut out, &mut err).unwrap();
        turn.feed(&covered, &mut out, &mut err).unwrap();
        let outside = resolved_for("p10", "Edit", "/etc/hosts", PermissionOutcome::Denied);
        turn.feed(&outside, &mut out, &mut err).unwrap();
        assert_eq!(
            said(&err),
            "✓ ran cargo test --workspace (allowed for this review)\n⊘ you denied /etc/hosts\n"
        );
    }

    #[test]
    fn json_mode_prints_the_events_and_never_asks() {
        let (turn, mut out, mut err) = run(true);
        let mut turn = turn.interactive(true);
        let ask = requested("p1", 2, "Bash", "make", None);
        turn.feed(&ask, &mut out, &mut err).unwrap();
        let allowed = resolved("p1", PermissionOutcome::Allowed);
        turn.feed(&allowed, &mut out, &mut err).unwrap();
        let lines: Vec<serde_json::Value> = said(&out)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["permission_requested"]["summary"], "make");
        assert!(lines[1].get("permission_resolved").is_some());
        assert!(err.is_empty() && !turn.asking());
    }

    #[test]
    fn invisible_characters_are_shown_as_escapes() {
        assert_eq!(printable("echo ok #\u{202e}x"), "echo ok #\\u{202e}x");
        assert_eq!(
            printable("ls\u{200b}-la\u{feff}"),
            "ls\\u{200b}-la\\u{feff}"
        );
        assert_eq!(printable("a\u{2028}b\u{2029}c"), "a\\u{2028}b\\u{2029}c");
        assert_eq!(printable("naïve ✓ 日本"), "naïve ✓ 日本");
    }

    #[test]
    fn what_the_agent_writes_cannot_repaint_the_terminal() {
        let (mut turn, mut out, mut err) = run(false);
        let events = [
            Event::AgentToolUse {
                pr: pr(),
                turn: 2,
                summary: "Read a.rs\r\u{1b}[2K\u{202e}".into(),
            },
            Event::AgentDenied {
                pr: pr(),
                turn: 2,
                tool: "Ba\u{1b}sh".into(),
                detail: "rm\u{9b}2K\u{200b}".into(),
            },
            chunk(2, "a\u{1b}[2Kb\u{9b}c\u{202e}d\u{2028}e\u{200b}f\tg\r\nh\n"),
        ];
        for e in &events {
            turn.feed(e, &mut out, &mut err).unwrap();
        }
        assert_eq!(
            said(&err),
            "✓ Read a.rs\\r\\u{1b}[2K\\u{202e}\n\
             ⊘ wanted to use Ba\\u{1b}sh: rm\\u{9b}2K\\u{200b} (denied)\n"
        );
        assert_eq!(said(&out), "a[2Kbcdef\tg\nh\n");
    }

    const NASTY: &str = "x\u{1b}[2K\u{202e}y";
    const SHOWN: &str = "x\\u{1b}[2K\\u{202e}y";

    #[test]
    fn a_suggestion_cannot_repaint_the_terminal() {
        let mut s = suggestion(Some(4), None, NASTY);
        s.file = NASTY.into();
        assert_eq!(suggestion_line(&s), format!("{SHOWN}:4  {SHOWN}"));

        let (mut turn, mut out, mut err) = run(false);
        let suggested = Event::AgentSuggestion {
            pr: pr(),
            turn: 2,
            suggestion: s,
        };
        turn.feed(&suggested, &mut out, &mut err).unwrap();
        turn.finish(&mut out).unwrap();
        assert_eq!(
            said(&out),
            format!("\nSuggested comments:\n{SHOWN}:4  {SHOWN}\n")
        );
    }

    #[test]
    fn no_log_entry_can_repaint_the_terminal() {
        let mut s = suggestion(None, None, NASTY);
        s.file = NASTY.into();
        let entries = vec![
            AgentLogEntry::User {
                at: 1,
                turn: 1,
                text: NASTY.into(),
            },
            AgentLogEntry::ToolUse {
                at: 2,
                turn: 1,
                summary: NASTY.into(),
            },
            AgentLogEntry::Text {
                at: 3,
                turn: 1,
                text: format!("{NASTY}\n"),
            },
            AgentLogEntry::Denied {
                at: 4,
                turn: 1,
                tool: NASTY.into(),
                detail: NASTY.into(),
            },
            AgentLogEntry::Suggestion {
                at: 5,
                turn: 1,
                suggestion: s,
            },
            AgentLogEntry::Error {
                at: 6,
                turn: 1,
                kind: AgentErrorKind::Interrupted,
                message: NASTY.into(),
            },
        ];
        assert_eq!(
            log_lines(&entries),
            [
                format!("you: {SHOWN}"),
                format!("  ✓ {SHOWN}"),
                "x[2Ky\n".to_string(),
                format!("  ⊘ wanted to use {SHOWN}: {SHOWN}"),
                format!("  suggestion {SHOWN}  {SHOWN}"),
                format!("  error: {SHOWN}"),
            ]
        );
    }

    #[test]
    fn an_error_message_cannot_repaint_the_terminal() {
        let (mut turn, mut out, mut err) = run(false);
        let error = Event::AgentError {
            pr: pr(),
            turn: 2,
            kind: AgentErrorKind::Interrupted,
            message: NASTY.into(),
        };
        turn.feed(&error, &mut out, &mut err).unwrap();
        assert_eq!(turn.failure(), Some(SHOWN));
    }

    #[test]
    fn an_answer_typed_before_the_question_was_shown_is_asked_again() {
        let (turn, mut out, mut err) = run(false);
        let mut turn = turn.interactive(true);
        turn.feed(
            &requested("p1", 2, "Bash", "make", None),
            &mut out,
            &mut err,
        )
        .unwrap();
        err.clear();
        assert_eq!(turn.typed("o", &mut err).unwrap(), None);
        assert_eq!(
            said(&err),
            "\n(answer ignored: typed before the question was shown)\n\
             Claude Code wants to run: make  [o]nce / [d]eny? "
        );
        assert!(
            turn.asking() && turn.just_asked(),
            "the window starts again"
        );
        std::thread::sleep(QUIET_AFTER_PROMPT + std::time::Duration::from_millis(50));
        err.clear();
        assert_eq!(
            turn.typed("o", &mut err).unwrap(),
            Some(("p1".into(), PermissionAnswerKind::Once))
        );
        assert!(err.is_empty());
    }

    #[test]
    fn a_line_typed_just_after_an_answer_is_dropped_quietly() {
        let (turn, mut out, mut err) = run(false);
        let mut turn = turn.interactive(true);
        turn.feed(
            &requested("p1", 2, "Bash", "make", None),
            &mut out,
            &mut err,
        )
        .unwrap();
        turn.answer_line("o");
        err.clear();
        assert_eq!(turn.typed("d", &mut err).unwrap(), None);
        assert!(err.is_empty(), "no question is open: {}", said(&err));
    }

    #[test]
    fn the_ending_after_end_of_input_has_no_blank_line() {
        let (turn, mut out, mut err) = run(false);
        let mut turn = turn.interactive(true);
        turn.feed(
            &requested("p1", 2, "Bash", "make", None),
            &mut out,
            &mut err,
        )
        .unwrap();
        err.clear();
        let mut noted = false;
        let none: Option<tokio::sync::mpsc::UnboundedReceiver<String>> = None;
        note_end_of_input(&mut turn, &none, &mut noted, &mut err).unwrap();
        turn.feed(
            &resolved("p1", PermissionOutcome::Expired),
            &mut out,
            &mut err,
        )
        .unwrap();
        assert_eq!(
            said(&err),
            "\nno more input: the request is denied when its time runs out\n⊘ denied make: no answer in time\n"
        );
    }

    #[test]
    fn a_new_question_is_quiet_for_a_moment() {
        let (turn, mut out, mut err) = run(false);
        let mut turn = turn.interactive(true);
        assert!(!turn.just_asked());
        turn.feed(
            &requested("p1", 2, "Bash", "make", None),
            &mut out,
            &mut err,
        )
        .unwrap();
        assert!(turn.just_asked());
        std::thread::sleep(QUIET_AFTER_PROMPT + std::time::Duration::from_millis(50));
        assert!(!turn.just_asked());
    }
}
