//! The JSON-lines protocol of a custom command: the lines Clúsia writes, the lines it reads, the
//! caps on what a program may write in one turn, and the fold of those lines into
//! `AgentEvent`s. It starts no process and reads no file; the daemon runs the program.

use std::path::Path;
use std::time::{Duration, Instant};

use clusia_protocol::AgentErrorKind;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

use crate::AgentEvent;

/// The protocol version both sides name on the first line.
pub const PROTOCOL: u64 = 1;
/// The longest line a program may write, in bytes, without its newline.
pub const MAX_LINE: usize = 1_048_576;
/// The most lines a program may write in one turn.
pub const MAX_LINES: usize = 10_000;
/// The most bytes a program may write in one turn, newlines included.
pub const MAX_TURN_BYTES: usize = 8 * 1_048_576;
/// How long a program has to answer a probe.
pub const PROBE_LIMIT: Duration = Duration::from_secs(5);
/// How long a program has to leave after it is asked to, before it is killed.
pub const STOP_GRACE: Duration = Duration::from_secs(10);
/// How long one write to a program's standard input may take.
pub const SEND_LIMIT: Duration = Duration::from_secs(10);

/// The line that starts a turn.
pub fn turn_line(
    prompt: &str,
    role: &str,
    session_id: Option<&str>,
    worktree: &Path,
    timeout_secs: u64,
) -> String {
    let line = json!({
        "type": "turn",
        "protocol": PROTOCOL,
        "prompt": prompt,
        "role": role,
        "session_id": session_id,
        "worktree": worktree.to_string_lossy(),
        "timeout_secs": timeout_secs,
    });
    format!("{line}\n")
}

/// The line that asks a program who it is, with no model call.
pub fn probe_line() -> String {
    format!("{}\n", json!({"type": "probe", "protocol": PROTOCOL}))
}

/// The line that answers a program's permission request.
pub fn reply_line(id: &str, allow: bool) -> String {
    let decision = if allow { "allow" } else { "deny" };
    format!(
        "{}\n",
        json!({"type": "permission_reply", "id": id, "decision": decision})
    )
}

/// What a permission request asks to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskKind {
    /// Run a command; `detail` is the command line.
    Command,
    /// Change files; `paths` names them.
    Edit,
    /// Anything else; `detail` says what.
    Other,
    /// A kind this version does not know, or a request whose fields cannot be read as the kind
    /// asks. It is never shown to the reviewer.
    Unknown(String),
}

/// How a program says a turn failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomErrorKind {
    Auth,
    Limit,
    Session,
    Other,
}

/// One line a program wrote, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// The session this turn runs in.
    Session(String),
    /// A piece of the answer.
    Text(String),
    /// A tool call that ran.
    Tool(String),
    /// A request to do something, which the reviewer decides. An empty `id` is a request that
    /// has none.
    Permission {
        id: String,
        kind: AskKind,
        detail: String,
        reason: Option<String>,
        paths: Vec<String>,
    },
    /// The whole answer; the turn is over.
    Final(String),
    /// The turn failed.
    Error {
        kind: CustomErrorKind,
        message: String,
    },
    /// The answer to a probe.
    Probe {
        name: String,
        version: Option<String>,
        protocol: Option<u64>,
    },
    /// A line that is not a JSON object; it is shown as text.
    NotJson(String),
    /// A JSON object this version does not know, or one without what it needs, or a blank line.
    Ignored,
}

/// Reads one line (without its newline) a program wrote.
pub fn read_line(line: &str) -> Output {
    if line.trim().is_empty() {
        return Output::Ignored;
    }
    let value = match serde_json::from_str::<Value>(line) {
        Ok(value @ Value::Object(_)) => value,
        _ => return Output::NotJson(line.to_string()),
    };
    let text = |key: &str| value.get(key).and_then(Value::as_str);
    match text("type") {
        Some("session") => match text("id").filter(|id| !id.is_empty()) {
            Some(id) => Output::Session(id.to_string()),
            None => Output::Ignored,
        },
        Some("text") => text("text").map_or(Output::Ignored, |t| Output::Text(t.to_string())),
        Some("tool") => text("summary").map_or(Output::Ignored, |s| Output::Tool(s.to_string())),
        Some("permission") => permission(&value),
        Some("final") => text("text").map_or(Output::Ignored, |t| Output::Final(t.to_string())),
        Some("error") => Output::Error {
            kind: match text("kind") {
                Some("auth") => CustomErrorKind::Auth,
                Some("limit") => CustomErrorKind::Limit,
                Some("session") => CustomErrorKind::Session,
                _ => CustomErrorKind::Other,
            },
            message: text("message").unwrap_or_default().to_string(),
        },
        Some("probe") => match text("name").map(str::trim).filter(|name| !name.is_empty()) {
            Some(name) => Output::Probe {
                name: name.to_string(),
                version: text("version")
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string),
                protocol: value.get("protocol").and_then(Value::as_u64),
            },
            None => Output::Ignored,
        },
        _ => Output::Ignored,
    }
}

fn permission(value: &Value) -> Output {
    let text = |key: &str| value.get(key).and_then(Value::as_str);
    let kind_name = text("kind").unwrap_or_default();
    let mut kind = match kind_name {
        "command" => AskKind::Command,
        "edit" => AskKind::Edit,
        "other" => AskKind::Other,
        other => AskKind::Unknown(other.to_string()),
    };
    // A list that is not all strings is not read: dropping an entry could let a request about
    // several files pass for one about fewer.
    let paths = match value.get("paths") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            let paths: Vec<String> = items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect();
            if paths.len() != items.len() {
                kind = AskKind::Unknown(kind_name.to_string());
            }
            paths
        }
        Some(_) => {
            kind = AskKind::Unknown(kind_name.to_string());
            Vec::new()
        }
    };
    Output::Permission {
        id: text("id").unwrap_or_default().to_string(),
        kind,
        detail: text("detail").unwrap_or_default().to_string(),
        reason: text("reason")
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(str::to_string),
        paths,
    }
}

/// Which cap a program went over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overflow {
    /// One line was longer than `MAX_LINE`.
    Line,
    /// The turn had more than `MAX_LINES` lines.
    Lines,
    /// The turn had more than `MAX_TURN_BYTES` bytes.
    Bytes,
}

/// Reads a program's standard output line by line and enforces the caps. A line is never held
/// in memory past `MAX_LINE` bytes, whatever the program writes.
pub struct LineReader<R> {
    reader: R,
    lines: usize,
    bytes: usize,
}

impl<R: AsyncBufRead + Unpin> LineReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            lines: 0,
            bytes: 0,
        }
    }

    /// The next non-blank line, without its line ending, or `None` at the end of the output.
    /// Blank lines count toward the caps and are skipped.
    pub async fn next(&mut self) -> Result<Option<String>, Overflow> {
        while let Some(line) = self.read_one().await? {
            if !line.trim().is_empty() {
                return Ok(Some(line));
            }
        }
        Ok(None)
    }

    async fn read_one(&mut self) -> Result<Option<String>, Overflow> {
        let mut line: Vec<u8> = Vec::new();
        let mut ended_by_newline = false;
        loop {
            let available = match self.reader.fill_buf().await {
                Ok(available) => available,
                Err(err) => {
                    // A broken pipe ends the output like EOF; the runner reports a turn that
                    // never reached its `final`.
                    tracing::debug!(%err, "reading a custom command's output failed");
                    break;
                }
            };
            if available.is_empty() {
                break;
            }
            let newline = available.iter().position(|b| *b == b'\n');
            let take = newline.unwrap_or(available.len());
            if line.len() + take > MAX_LINE {
                return Err(Overflow::Line);
            }
            line.extend_from_slice(&available[..take]);
            let used = take + usize::from(newline.is_some());
            self.reader.consume(used);
            self.bytes += used;
            if newline.is_some() {
                ended_by_newline = true;
                break;
            }
            if self.bytes > MAX_TURN_BYTES {
                return Err(Overflow::Bytes);
            }
        }
        if line.is_empty() && !ended_by_newline {
            return Ok(None);
        }
        self.lines += 1;
        if self.lines > MAX_LINES {
            return Err(Overflow::Lines);
        }
        if self.bytes > MAX_TURN_BYTES {
            return Err(Overflow::Bytes);
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Ok(Some(String::from_utf8_lossy(&line).into_owned()))
    }
}

/// Folds the lines of one turn into the events the chat, the checks and the revalidation read.
#[derive(Debug)]
pub struct CustomTurn {
    name: String,
    started: Instant,
    seen_json: bool,
    ended: bool,
}

impl CustomTurn {
    /// `name` is what the pages call the program; `started` is when the turn began.
    pub fn new(name: String, started: Instant) -> Self {
        Self {
            name,
            started,
            seen_json: false,
            ended: false,
        }
    }

    /// The events `out` means. Nothing after the turn's `final` or `error` counts.
    pub fn on(&mut self, out: Output) -> Vec<AgentEvent> {
        if self.ended {
            return Vec::new();
        }
        if !matches!(out, Output::NotJson(_)) {
            self.seen_json = true;
        }
        match out {
            Output::Session(id) => vec![AgentEvent::SessionId(id)],
            Output::Text(text) => vec![AgentEvent::Text(text)],
            Output::Tool(summary) => vec![AgentEvent::ToolUse(summary)],
            Output::NotJson(line) => vec![AgentEvent::Text(format!("{line}\n"))],
            Output::Final(text) => {
                self.ended = true;
                vec![AgentEvent::Final {
                    text,
                    duration_ms: self.started.elapsed().as_millis() as u64,
                    is_error: false,
                }]
            }
            Output::Error { kind, message } => {
                self.ended = true;
                vec![self.error(kind, &message)]
            }
            Output::Permission { .. } | Output::Probe { .. } | Output::Ignored => Vec::new(),
        }
    }

    fn error(&self, kind: CustomErrorKind, message: &str) -> AgentEvent {
        let name = &self.name;
        let message = message.trim();
        let (kind, message) = match kind {
            CustomErrorKind::Auth if message.is_empty() => (
                AgentErrorKind::NotSignedIn,
                format!("{name} is not signed in"),
            ),
            CustomErrorKind::Auth => (AgentErrorKind::NotSignedIn, format!("{name}: {message}")),
            CustomErrorKind::Limit => (
                AgentErrorKind::UsageLimit,
                format!("{name} reached its usage limit"),
            ),
            CustomErrorKind::Session => (
                AgentErrorKind::Crashed,
                format!("{name} no longer knows this session; the next turn starts a new one"),
            ),
            CustomErrorKind::Other if message.is_empty() => {
                (AgentErrorKind::Crashed, format!("{name} reported an error"))
            }
            CustomErrorKind::Other => (AgentErrorKind::Crashed, format!("{name}: {message}")),
        };
        AgentEvent::Error { kind, message }
    }

    /// Whether any line was a JSON object, which is what tells a program that speaks the
    /// protocol from one that does not.
    pub fn seen_json(&self) -> bool {
        self.seen_json
    }

    /// Whether the turn has its `final` or `error`.
    pub fn ended(&self) -> bool {
        self.ended
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::{AsyncReadExt, BufReader};

    fn path() -> &'static Path {
        Path::new("/work/acme-widgets")
    }

    fn json_of(line: &str) -> Value {
        assert!(line.ends_with('\n'), "{line:?}");
        assert_eq!(line.matches('\n').count(), 1, "one line: {line:?}");
        serde_json::from_str(line).unwrap()
    }

    #[test]
    fn the_constants_are_the_protocols() {
        assert_eq!(PROTOCOL, 1);
        assert_eq!(MAX_LINE, 1_048_576);
        assert_eq!(MAX_LINES, 10_000);
        assert_eq!(MAX_TURN_BYTES, 8 * 1_048_576);
        assert_eq!(PROBE_LIMIT, Duration::from_secs(5));
        assert_eq!(STOP_GRACE, Duration::from_secs(10));
        assert_eq!(SEND_LIMIT, Duration::from_secs(10));
    }

    #[test]
    fn a_turn_is_one_compact_line() {
        let line = turn_line("Why?\nBecause.", "be brief", Some("abc"), path(), 600);
        assert_eq!(
            json_of(&line),
            json!({
                "type": "turn", "protocol": 1, "prompt": "Why?\nBecause.",
                "role": "be brief", "session_id": "abc",
                "worktree": "/work/acme-widgets", "timeout_secs": 600
            })
        );
        let fresh = json_of(&turn_line("hi", "r", None, path(), 60));
        assert_eq!(fresh["session_id"], Value::Null);
        assert!(fresh.get("session_id").is_some(), "null, not absent");
    }

    #[test]
    fn a_probe_and_a_reply_are_one_line_each() {
        assert_eq!(
            json_of(&probe_line()),
            json!({"type": "probe", "protocol": 1})
        );
        assert_eq!(
            json_of(&reply_line("p1", true)),
            json!({"type": "permission_reply", "id": "p1", "decision": "allow"})
        );
        assert_eq!(json_of(&reply_line("p\"2", false))["decision"], "deny");
        assert_eq!(json_of(&reply_line("p\"2", false))["id"], "p\"2");
    }

    #[test]
    fn every_output_type_is_read() {
        let read = |s: &str| read_line(s);
        assert_eq!(
            read(r#"{"type":"session","id":"abc"}"#),
            Output::Session("abc".into())
        );
        assert_eq!(
            read(r#"{"type":"text","text":"Hel"}"#),
            Output::Text("Hel".into())
        );
        assert_eq!(
            read(r#"{"type":"tool","summary":"Read src/x.rs"}"#),
            Output::Tool("Read src/x.rs".into())
        );
        assert_eq!(
            read(r#"{"type":"final","text":"Done."}"#),
            Output::Final("Done.".into())
        );
        assert_eq!(
            read(r#"{"type":"probe","name":"Ollama","version":"0.3","protocol":1}"#),
            Output::Probe {
                name: "Ollama".into(),
                version: Some("0.3".into()),
                protocol: Some(1)
            }
        );
        assert_eq!(
            read(r#"{"type":"probe","name":"Bare"}"#),
            Output::Probe {
                name: "Bare".into(),
                version: None,
                protocol: None
            }
        );
    }

    #[test]
    fn a_permission_request_names_its_kind_and_fields() {
        assert_eq!(
            read_line(
                r#"{"type":"permission","id":"p1","kind":"command","detail":"cargo test","reason":" run them "}"#
            ),
            Output::Permission {
                id: "p1".into(),
                kind: AskKind::Command,
                detail: "cargo test".into(),
                reason: Some("run them".into()),
                paths: vec![]
            }
        );
        assert_eq!(
            read_line(r#"{"type":"permission","id":"p2","kind":"edit","paths":["a.rs","b/c.rs"]}"#),
            Output::Permission {
                id: "p2".into(),
                kind: AskKind::Edit,
                detail: String::new(),
                reason: None,
                paths: vec!["a.rs".into(), "b/c.rs".into()]
            }
        );
        assert!(matches!(
            read_line(r#"{"type":"permission","id":"p3","kind":"other","detail":"x"}"#),
            Output::Permission {
                kind: AskKind::Other,
                ..
            }
        ));
    }

    #[test]
    fn a_request_that_cannot_be_read_as_its_kind_is_unknown_not_half_read() {
        let kind_of = |line: &str| match read_line(line) {
            Output::Permission { id, kind, .. } => (id, kind),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            kind_of(r#"{"type":"permission","id":"p1","kind":"delete"}"#),
            ("p1".into(), AskKind::Unknown("delete".into()))
        );
        assert_eq!(
            kind_of(r#"{"type":"permission","id":"p1"}"#),
            ("p1".into(), AskKind::Unknown(String::new()))
        );
        assert_eq!(
            kind_of(r#"{"type":"permission","id":"p1","kind":"edit","paths":["a.rs",7]}"#),
            ("p1".into(), AskKind::Unknown("edit".into()))
        );
        assert_eq!(
            kind_of(r#"{"type":"permission","id":"p1","kind":"edit","paths":"a.rs"}"#),
            ("p1".into(), AskKind::Unknown("edit".into()))
        );
        assert_eq!(
            kind_of(r#"{"type":"permission","kind":"command","detail":"ls"}"#),
            (String::new(), AskKind::Command),
            "a request with no id comes through with an empty one, for the runner to refuse"
        );
    }

    #[test]
    fn an_error_names_its_kind_and_an_unknown_kind_is_other() {
        for (kind, want) in [
            ("auth", CustomErrorKind::Auth),
            ("limit", CustomErrorKind::Limit),
            ("session", CustomErrorKind::Session),
            ("other", CustomErrorKind::Other),
            ("quota", CustomErrorKind::Other),
        ] {
            let line = format!(r#"{{"type":"error","kind":"{kind}","message":"m"}}"#);
            assert_eq!(
                read_line(&line),
                Output::Error {
                    kind: want,
                    message: "m".into()
                },
                "{kind}"
            );
        }
        assert_eq!(
            read_line(r#"{"type":"error"}"#),
            Output::Error {
                kind: CustomErrorKind::Other,
                message: String::new()
            }
        );
    }

    #[test]
    fn what_is_not_a_json_object_is_text_and_what_is_not_known_is_ignored() {
        for line in [
            "hello there",
            "{not json",
            "42",
            "\"quoted\"",
            "[1,2]",
            "null",
        ] {
            assert_eq!(read_line(line), Output::NotJson(line.into()), "{line}");
        }
        for line in [
            "",
            "   ",
            r#"{"type":"thinking","text":"hm"}"#,
            r#"{"text":"no type"}"#,
            r#"{"type":7}"#,
            r#"{"type":"session"}"#,
            r#"{"type":"session","id":""}"#,
            r#"{"type":"text"}"#,
            r#"{"type":"tool","summary":3}"#,
            r#"{"type":"final"}"#,
            r#"{"type":"probe"}"#,
            r#"{"type":"permission_reply","id":"p1","decision":"allow"}"#,
        ] {
            assert_eq!(read_line(line), Output::Ignored, "{line}");
        }
    }

    fn reader(bytes: Vec<u8>) -> LineReader<BufReader<Cursor<Vec<u8>>>> {
        LineReader::new(BufReader::with_capacity(8192, Cursor::new(bytes)))
    }

    async fn all(bytes: Vec<u8>) -> Result<Vec<String>, Overflow> {
        let mut reader = reader(bytes);
        let mut lines = Vec::new();
        while let Some(line) = reader.next().await? {
            lines.push(line);
        }
        Ok(lines)
    }

    #[tokio::test]
    async fn lines_come_back_without_their_endings_and_blank_ones_are_skipped() {
        assert_eq!(
            all(b"one\r\n\n  \ntwo\nthree".to_vec()).await,
            Ok(vec!["one".into(), "two".into(), "three".into()])
        );
        assert_eq!(all(Vec::new()).await, Ok(vec![]));
        assert_eq!(all(b"\n\n".to_vec()).await, Ok(vec![]));
        assert_eq!(
            all("é\n".as_bytes().to_vec()).await,
            Ok(vec!["é".to_string()])
        );
        let invalid = all(vec![b'a', 0xFF, b'\n']).await.unwrap();
        assert_eq!(invalid, ["a\u{FFFD}"]);
    }

    #[tokio::test]
    async fn a_line_of_exactly_the_cap_is_read_and_one_byte_more_is_not() {
        let mut ok = vec![b'x'; MAX_LINE];
        ok.push(b'\n');
        let lines = all(ok).await.unwrap();
        assert_eq!(lines[0].len(), MAX_LINE);
        let mut over = vec![b'x'; MAX_LINE + 1];
        over.push(b'\n');
        assert_eq!(all(over).await, Err(Overflow::Line));
        assert_eq!(all(vec![b'x'; MAX_LINE + 1]).await, Err(Overflow::Line));
    }

    #[tokio::test]
    async fn an_endless_line_stops_at_the_cap_without_being_read_to_its_end() {
        // 64 MB with no newline: reading it all would be the bug.
        let mut r = LineReader::new(BufReader::with_capacity(
            8192,
            tokio::io::repeat(b'x').take(64 * 1_048_576),
        ));
        assert_eq!(r.next().await, Err(Overflow::Line));
        let read = 64 * 1_048_576 - r.reader.get_ref().limit() as usize;
        assert!(read <= MAX_LINE + 8192, "read {read} bytes");
    }

    #[tokio::test]
    async fn more_lines_than_the_cap_stop_the_turn() {
        let ok = "{\"type\":\"text\",\"text\":\"a\"}\n".repeat(MAX_LINES);
        assert_eq!(all(ok.clone().into_bytes()).await.unwrap().len(), MAX_LINES);
        let over = format!("{ok}{{\"type\":\"text\",\"text\":\"a\"}}\n");
        assert_eq!(all(over.into_bytes()).await, Err(Overflow::Lines));
        let blanks = "\n".repeat(MAX_LINES + 1);
        assert_eq!(all(blanks.into_bytes()).await, Err(Overflow::Lines));
    }

    #[tokio::test]
    async fn more_bytes_than_the_cap_stop_the_turn() {
        let big = |n: usize| {
            let mut one = vec![b'y'; n];
            one.push(b'\n');
            one
        };
        // Eight lines of 1 MiB - 1 byte plus the newline are exactly the cap.
        let exact: Vec<u8> = (0..8).flat_map(|_| big(MAX_LINE - 1)).collect();
        assert_eq!(exact.len(), MAX_TURN_BYTES);
        assert_eq!(all(exact.clone()).await.unwrap().len(), 8);
        let mut over = exact;
        over.extend(b"z\n");
        assert_eq!(all(over).await, Err(Overflow::Bytes));
    }

    fn turn() -> CustomTurn {
        CustomTurn::new("Echo".into(), Instant::now())
    }

    #[test]
    fn a_turn_folds_its_lines_into_events() {
        let mut t = turn();
        let mut events = Vec::new();
        for line in [
            r#"{"type":"session","id":"abc"}"#,
            r#"{"type":"tool","summary":"Read x"}"#,
            r#"{"type":"text","text":"Hel"}"#,
            "plain words",
            r#"{"type":"future","x":1}"#,
            r#"{"type":"final","text":"Hello"}"#,
            r#"{"type":"text","text":"too late"}"#,
        ] {
            events.extend(t.on(read_line(line)));
        }
        assert!(t.ended() && t.seen_json());
        assert!(
            matches!(
                events.as_slice(),
                [
                    AgentEvent::SessionId(id),
                    AgentEvent::ToolUse(tool),
                    AgentEvent::Text(a),
                    AgentEvent::Text(b),
                    AgentEvent::Final { text, is_error: false, .. },
                ] if id == "abc" && tool == "Read x" && a == "Hel" && b == "plain words\n" && text == "Hello"
            ),
            "{events:?}"
        );
    }

    #[test]
    fn a_program_that_prints_no_json_has_not_seen_json() {
        let mut t = turn();
        assert!(!t.seen_json());
        t.on(read_line("hello"));
        assert!(!t.seen_json());
        t.on(read_line(r#"{"type":"future"}"#));
        assert!(
            t.seen_json(),
            "an object counts even when it is not understood"
        );
        assert!(!t.ended());
    }

    #[test]
    fn a_permission_line_and_a_probe_line_make_no_event() {
        let mut t = turn();
        assert!(
            t.on(read_line(
                r#"{"type":"permission","id":"p1","kind":"other"}"#
            ))
            .is_empty()
        );
        assert!(t.on(read_line(r#"{"type":"probe","name":"x"}"#)).is_empty());
        assert!(!t.ended());
    }

    #[test]
    fn errors_read_as_the_spec_words_them() {
        let say = |kind: &str, message: &str| {
            let mut t = turn();
            let line = json!({"type": "error", "kind": kind, "message": message}).to_string();
            let events = t.on(read_line(&line));
            assert!(t.ended());
            assert!(t.on(read_line(r#"{"type":"final","text":"x"}"#)).is_empty());
            events
        };
        let err = |kind, message: &str| {
            vec![AgentEvent::Error {
                kind,
                message: message.into(),
            }]
        };
        assert_eq!(
            say("auth", "token expired"),
            err(AgentErrorKind::NotSignedIn, "Echo: token expired")
        );
        assert_eq!(
            say("auth", ""),
            err(AgentErrorKind::NotSignedIn, "Echo is not signed in")
        );
        assert_eq!(
            say("limit", "ignored"),
            err(AgentErrorKind::UsageLimit, "Echo reached its usage limit")
        );
        assert_eq!(
            say("session", "gone"),
            err(
                AgentErrorKind::Crashed,
                "Echo no longer knows this session; the next turn starts a new one"
            )
        );
        assert_eq!(
            say("other", "boom"),
            err(AgentErrorKind::Crashed, "Echo: boom")
        );
        assert_eq!(
            say("quota", ""),
            err(AgentErrorKind::Crashed, "Echo reported an error")
        );
    }

    #[test]
    fn the_final_carries_how_long_the_turn_took() {
        let mut t = CustomTurn::new("Echo".into(), Instant::now() - Duration::from_secs(3));
        let events = t.on(read_line(r#"{"type":"final","text":"ok"}"#));
        let [AgentEvent::Final { duration_ms, .. }] = events.as_slice() else {
            panic!("{events:?}");
        };
        assert!((3000..4000).contains(duration_ms), "{duration_ms}");
    }

    fn fold(fixture: &str) -> (Vec<AgentEvent>, CustomTurn) {
        let mut t = turn();
        let events = fixture
            .lines()
            .filter(|l| !l.trim().is_empty())
            .flat_map(|l| t.on(read_line(l)))
            .collect();
        (events, t)
    }

    #[test]
    fn a_chat_turn_fixture_streams_and_ends() {
        let (events, t) = fold(include_str!("../tests/fixtures/custom/chat_turn.jsonl"));
        assert!(t.ended());
        assert!(
            matches!(
                events.as_slice(),
                [
                    AgentEvent::SessionId(id),
                    AgentEvent::ToolUse(tool),
                    AgentEvent::Text(a),
                    AgentEvent::Text(b),
                    AgentEvent::Final { text, is_error: false, .. },
                ] if id == "sess-1" && tool == "Read src/auth/refresh.rs"
                    && format!("{a}{b}") == *text
            ),
            "{events:?}"
        );
    }

    #[test]
    fn a_permission_fixture_reads_three_kinds_of_request() {
        let outs: Vec<Output> = include_str!("../tests/fixtures/custom/permission_turn.jsonl")
            .lines()
            .map(read_line)
            .collect();
        let kinds: Vec<_> = outs
            .iter()
            .filter_map(|o| match o {
                Output::Permission {
                    id, kind, paths, ..
                } => Some((id.as_str(), kind.clone(), paths.len())),
                _ => None,
            })
            .collect();
        assert_eq!(
            kinds,
            [
                ("p1", AskKind::Command, 0),
                ("p2", AskKind::Edit, 2),
                ("p3", AskKind::Other, 0)
            ]
        );
    }

    #[test]
    fn the_error_fixtures_end_the_turn_with_the_words_of_the_spec() {
        let (events, t) = fold(include_str!("../tests/fixtures/custom/error_auth.jsonl"));
        assert!(t.ended());
        assert!(
            matches!(
                events.last(),
                Some(AgentEvent::Error { kind: AgentErrorKind::NotSignedIn, message })
                    if message == "Echo: the API key was rejected"
            ),
            "{events:?}"
        );
        let (events, _) = fold(include_str!("../tests/fixtures/custom/error_session.jsonl"));
        assert_eq!(
            events,
            [AgentEvent::Error {
                kind: AgentErrorKind::Crashed,
                message: "Echo no longer knows this session; the next turn starts a new one".into()
            }]
        );
    }

    #[test]
    fn a_messy_turn_shows_the_text_skips_the_unknown_and_ignores_what_follows_the_end() {
        let (events, t) = fold(include_str!("../tests/fixtures/custom/messy_turn.jsonl"));
        assert!(t.ended() && t.seen_json());
        let texts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["loading model...\n", "Answer."]);
        assert!(matches!(events.last(), Some(AgentEvent::Final { text, .. }) if text == "Answer."));
        assert!(
            !events.iter().any(|e| matches!(e, AgentEvent::SessionId(_))),
            "a session line without an id says nothing"
        );
    }

    #[test]
    fn the_probe_fixture_is_a_probe() {
        assert_eq!(
            read_line(include_str!("../tests/fixtures/custom/probe.jsonl").trim()),
            Output::Probe {
                name: "Ollama".into(),
                version: Some("0.3".into()),
                protocol: Some(1)
            }
        );
    }
}
