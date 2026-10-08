//! Reads Claude Code's `--output-format stream-json` output, one line at a time.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use clusia_protocol::AgentErrorKind;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentEvent {
    /// Part of the answer, in the order it was written.
    Text(String),
    /// A tool call that ran, e.g. "Read src/auth/store.rs".
    ToolUse(String),
    /// A tool call the agent was not allowed to make.
    Denied { tool: String, detail: String },
    /// The turn ended normally. `text` is the whole answer, suggestion blocks included.
    Final {
        text: String,
        duration_ms: u64,
        is_error: bool,
    },
    /// The turn failed; no `Final` follows.
    Error {
        kind: AgentErrorKind,
        message: String,
    },
    /// The session this turn runs in, from the first line of the output.
    SessionId(String),
}

/// What the parser remembers between the lines of one turn.
#[derive(Debug, Default)]
pub struct ParseState {
    /// The worktree: paths inside it are shown relative to it.
    pub root: Option<PathBuf>,
    session_seen: bool,
    /// The answer so far, for when the result line carries none.
    text: String,
    /// Whether text arrived as partial deltas; if so the full message repeats it.
    streamed: bool,
    /// A new message began after some text: its first text starts a new paragraph.
    paragraph_break: bool,
    /// Tool calls waiting for their result: id -> (what to show, what was asked).
    pending: HashMap<String, (String, String)>,
    denied: HashSet<String>,
}

impl ParseState {
    /// A fresh state for a turn that runs in the worktree `root`.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: Some(root.into()),
            ..Self::default()
        }
    }
}

const DETAIL_CHARS: usize = 200;

/// The events one output line stands for. A line that is not JSON, or whose type is not
/// known, becomes its own text so nothing the CLI says is lost.
pub fn parse_line(line: &str, state: &mut ParseState) -> Vec<AgentEvent> {
    let line = line.trim_end_matches(['\n', '\r']);
    if line.trim().is_empty() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return raw(line);
    };
    match value.get("type").and_then(Value::as_str) {
        Some("system") => system(&value, state),
        Some("stream_event") => stream_event(&value, state),
        Some("assistant") => assistant(&value, state),
        Some("user") => user(&value, state),
        Some("result") => result(&value, state),
        Some("rate_limit_event") => Vec::new(),
        _ => raw(line),
    }
}

fn raw(line: &str) -> Vec<AgentEvent> {
    vec![AgentEvent::Text(format!("{line}\n"))]
}

fn system(value: &Value, state: &mut ParseState) -> Vec<AgentEvent> {
    match value.get("subtype").and_then(Value::as_str) {
        Some("init") if !state.session_seen => {
            state.session_seen = true;
            match value.get("session_id").and_then(Value::as_str) {
                Some(id) => vec![AgentEvent::SessionId(id.to_string())],
                None => Vec::new(),
            }
        }
        Some("permission_denied") => {
            let id = str_of(value, "tool_use_id");
            let tool = str_of(value, "tool_name");
            let detail = state
                .pending
                .remove(id)
                .map(|(_, asked)| asked)
                .unwrap_or_default();
            state.denied.insert(id.to_string());
            vec![AgentEvent::Denied {
                tool: tool.to_string(),
                detail,
            }]
        }
        _ => Vec::new(),
    }
}

fn stream_event(value: &Value, state: &mut ParseState) -> Vec<AgentEvent> {
    let Some(event) = value.get("event") else {
        return Vec::new();
    };
    match event.get("type").and_then(Value::as_str) {
        Some("message_start") => {
            state.paragraph_break = !state.text.is_empty();
            Vec::new()
        }
        Some("content_block_delta") => {
            let delta = &event["delta"];
            if delta.get("type").and_then(Value::as_str) != Some("text_delta") {
                return Vec::new();
            }
            let text = str_of(delta, "text");
            if text.is_empty() {
                return Vec::new();
            }
            state.streamed = true;
            vec![AgentEvent::Text(take_text(state, text))]
        }
        _ => Vec::new(),
    }
}

fn assistant(value: &Value, state: &mut ParseState) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let blocks = value["message"]["content"].as_array().into_iter().flatten();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                let (summary, asked) = describe_tool(block, state.root.as_deref());
                state
                    .pending
                    .insert(str_of(block, "id").to_string(), (summary, asked));
            }
            Some("text") if !state.streamed => {
                let text = str_of(block, "text");
                if !text.is_empty() {
                    state.paragraph_break = !state.text.is_empty();
                    events.push(AgentEvent::Text(take_text(state, text)));
                }
            }
            _ => {}
        }
    }
    events
}

/// Adds `text` to the answer so far and returns what to show: the paragraph break that a new
/// message needs, then the text.
fn take_text(state: &mut ParseState, text: &str) -> String {
    let shown = if std::mem::take(&mut state.paragraph_break) {
        format!("\n\n{text}")
    } else {
        text.to_string()
    };
    state.text.push_str(&shown);
    shown
}

/// A tool call is reported when its result arrives, unless the CLI denied it.
fn user(value: &Value, state: &mut ParseState) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let blocks = value["message"]["content"].as_array().into_iter().flatten();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let Some((summary, _)) = state.pending.remove(str_of(block, "tool_use_id")) else {
            continue;
        };
        // A denial whose own line did not come shows up here as a failed result; the
        // `Denied` event follows with the final result.
        let refused = block.get("is_error").and_then(Value::as_bool) == Some(true)
            && result_text(block).contains("has been denied");
        if !refused {
            events.push(AgentEvent::ToolUse(summary));
        }
    }
    events
}

fn result(value: &Value, state: &mut ParseState) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    let text = str_of(value, "result");
    let failed = value.get("is_error").and_then(Value::as_bool) == Some(true)
        || value
            .get("subtype")
            .and_then(Value::as_str)
            .is_some_and(|s| s.starts_with("error"));
    if failed {
        let message = if text.is_empty() {
            AgentErrorKind::Crashed.default_message().to_string()
        } else {
            text.to_string()
        };
        return vec![AgentEvent::Error {
            kind: classify(&message),
            message,
        }];
    }
    // Denials the stream did not announce on their own line.
    for denial in value["permission_denials"].as_array().into_iter().flatten() {
        let id = str_of(denial, "tool_use_id");
        if state.denied.insert(id.to_string()) {
            let (_, asked) = describe_tool(
                &serde_json::json!({
                    "name": denial.get("tool_name"),
                    "input": denial.get("tool_input"),
                }),
                state.root.as_deref(),
            );
            events.push(AgentEvent::Denied {
                tool: str_of(denial, "tool_name").to_string(),
                detail: asked,
            });
        }
    }
    events.push(AgentEvent::Final {
        text: if text.is_empty() {
            state.text.clone()
        } else {
            text.to_string()
        },
        duration_ms: value
            .get("duration_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        is_error: false,
    });
    events
}

/// The text of a `tool_result` block, whose content is a string or a list of text blocks.
fn result_text(block: &Value) -> String {
    match &block["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// What the CLI's own error text says about why the turn failed.
fn classify(message: &str) -> AgentErrorKind {
    let lower = message.to_lowercase();
    if [
        "not logged in",
        "/login",
        "invalid api key",
        "authentication",
    ]
    .iter()
    .any(|s| lower.contains(s))
    {
        AgentErrorKind::NotSignedIn
    } else if ["usage limit", "limit reached", "rate limit"]
        .iter()
        .any(|s| lower.contains(s))
    {
        AgentErrorKind::UsageLimit
    } else {
        AgentErrorKind::Crashed
    }
}

/// `(what the chat shows, what was asked)` for one `tool_use` block.
fn describe_tool(block: &Value, root: Option<&Path>) -> (String, String) {
    let name = str_of(block, "name");
    let input = &block["input"];
    let path = |key: &str| relative(str_of(input, key), root);
    let (summary, asked) = match name {
        "Read" => (format!("Read {}", path("file_path")), path("file_path")),
        "LS" => (format!("Listed {}", path("path")), path("path")),
        "Grep" => (
            format!("Searched for {}", str_of(input, "pattern")),
            str_of(input, "pattern").to_string(),
        ),
        "Glob" => (
            format!("Searched files matching {}", str_of(input, "pattern")),
            str_of(input, "pattern").to_string(),
        ),
        "Bash" => (
            format!("Ran `{}`", str_of(input, "command")),
            str_of(input, "command").to_string(),
        ),
        _ => {
            let asked = input.to_string();
            (name.to_string(), asked)
        }
    };
    (cut(&summary), cut(&asked))
}

fn relative(path: &str, root: Option<&Path>) -> String {
    root.and_then(|root| Path::new(path).strip_prefix(root).ok())
        .map_or_else(
            || path.to_string(),
            |p| match p.display().to_string() {
                shown if shown.is_empty() => ".".to_string(),
                shown => shown,
            },
        )
}

fn cut(text: &str) -> String {
    if text.chars().count() > DETAIL_CHARS {
        let head: String = text.chars().take(DETAIL_CHARS).collect();
        format!("{head}…")
    } else {
        text.to_string()
    }
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d";

    fn run(fixture: &str) -> Vec<AgentEvent> {
        let mut state = ParseState::with_root("/tmp/acme-widgets");
        fixture
            .lines()
            .flat_map(|l| parse_line(l, &mut state))
            .collect()
    }

    fn text_of(events: &[AgentEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_streamed_answer_arrives_in_pieces_once() {
        let events = run(include_str!("../tests/fixtures/text_turn.jsonl"));
        assert_eq!(
            events,
            [
                AgentEvent::SessionId(SESSION.into()),
                AgentEvent::Text("This pull request refreshes the auth token ".into()),
                AgentEvent::Text("before it expires.\n\nIt touches `src/auth/store.rs` ".into()),
                AgentEvent::Text("and its tests.".into()),
                AgentEvent::Final {
                    text: "This pull request refreshes the auth token before it expires.\n\nIt touches `src/auth/store.rs` and its tests."
                        .into(),
                    duration_ms: 6000,
                    is_error: false,
                },
            ]
        );
    }

    #[test]
    fn tools_that_ran_are_listed_and_the_denied_one_is_not() {
        let events = run(include_str!("../tests/fixtures/tool_and_denied.jsonl"));
        let tools: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolUse(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tools, ["Read src/auth/store.rs", "Searched for expires_at"]);
        let denied: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::Denied { .. }))
            .collect();
        assert_eq!(
            denied,
            [&AgentEvent::Denied {
                tool: "Bash".into(),
                detail: "cargo --version".into()
            }],
            "announced once, even though the result repeats it"
        );
    }

    #[test]
    fn the_final_text_keeps_the_suggestion_block_for_the_caller() {
        let events = run(include_str!("../tests/fixtures/tool_and_denied.jsonl"));
        let Some(AgentEvent::Final { text, is_error, .. }) = events.last() else {
            panic!("no final: {events:?}");
        };
        assert!(!is_error);
        assert!(text.contains("```clusia-suggestion"));
        assert!(text.starts_with("This is a Rust workspace"));
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::SessionId(_)))
                .count(),
            1
        );
        assert!(text_of(&events).contains("the shell command was denied"));
    }

    #[test]
    fn without_partial_messages_the_full_message_is_the_text() {
        let events = run(include_str!("../tests/fixtures/resume_turn.jsonl"));
        assert_eq!(
            events,
            [
                AgentEvent::SessionId(SESSION.into()),
                AgentEvent::Text("Yes: `Store` keeps `expires_at` as a Unix timestamp.".into()),
                AgentEvent::Final {
                    text: "Yes: `Store` keeps `expires_at` as a Unix timestamp.".into(),
                    duration_ms: 1400,
                    is_error: false,
                },
            ]
        );
    }

    #[test]
    fn a_result_without_text_falls_back_to_what_was_streamed() {
        let mut state = ParseState::default();
        let mut events = Vec::new();
        for line in [
            r#"{"type":"stream_event","event":{"type":"message_start"}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"One."}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_start"}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Two."}}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":5,"result":""}"#,
        ] {
            events.extend(parse_line(line, &mut state));
        }
        assert_eq!(
            events,
            [
                AgentEvent::Text("One.".into()),
                AgentEvent::Text("\n\nTwo.".into()),
                AgentEvent::Final {
                    text: "One.\n\nTwo.".into(),
                    duration_ms: 5,
                    is_error: false
                }
            ],
            "the break between messages is shown as well as kept"
        );
    }

    #[test]
    fn not_signed_in_is_an_error_without_a_final() {
        let events = run(include_str!("../tests/fixtures/not_signed_in.jsonl"));
        assert_eq!(
            events,
            [AgentEvent::Error {
                kind: AgentErrorKind::NotSignedIn,
                message: "Not logged in · Please run /login".into()
            }]
        );
    }

    #[test]
    fn a_usage_limit_is_an_error_with_the_cli_text() {
        let events = run(include_str!("../tests/fixtures/usage_limit.jsonl"));
        assert_eq!(
            events.last(),
            Some(&AgentEvent::Error {
                kind: AgentErrorKind::UsageLimit,
                message: "5-hour limit reached ∙ resets 3pm".into()
            })
        );
        assert!(!events.iter().any(|e| matches!(e, AgentEvent::Final { .. })));
    }

    #[test]
    fn other_failures_are_crashes_and_an_empty_one_has_a_default_text() {
        let mut state = ParseState::default();
        let events = parse_line(
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":""}"#,
            &mut state,
        );
        assert_eq!(
            events,
            [AgentEvent::Error {
                kind: AgentErrorKind::Crashed,
                message: AgentErrorKind::Crashed.default_message().into()
            }]
        );
        let events = parse_line(
            r#"{"type":"result","subtype":"error_max_turns","is_error":false,"result":"Stopped"}"#,
            &mut state,
        );
        assert!(matches!(
            events.as_slice(),
            [AgentEvent::Error {
                kind: AgentErrorKind::Crashed,
                ..
            }]
        ));
    }

    #[test]
    fn a_stream_that_stops_has_no_final() {
        let events = run(include_str!("../tests/fixtures/crash_mid_stream.jsonl"));
        assert_eq!(
            events,
            [
                AgentEvent::SessionId(SESSION.into()),
                AgentEvent::Text("Looking at the diff, the first".into()),
            ]
        );
    }

    #[test]
    fn unknown_lines_become_raw_text() {
        let events = run(include_str!("../tests/fixtures/unknown_lines.jsonl"));
        assert_eq!(
            events,
            [
                AgentEvent::Text("warning: something the CLI printed\n".into()),
                AgentEvent::SessionId(SESSION.into()),
                AgentEvent::Text("{\"type\":\"telemetry_ping\",\"payload\":{\"n\":1}}\n".into()),
                AgentEvent::Text("{ not json\n".into()),
                AgentEvent::Text("{\"type\":\"hook_event\",\"name\":\"x\"}\n".into()),
                AgentEvent::Text("Done.".into()),
                AgentEvent::Final {
                    text: "Done.".into(),
                    duration_ms: 6000,
                    is_error: false
                },
            ]
        );
    }

    #[test]
    fn a_tool_only_message_adds_no_break() {
        let mut state = ParseState::default();
        let mut events = Vec::new();
        for line in [
            r#"{"type":"stream_event","event":{"type":"message_start"}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Reading."}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_start"}}"#,
            r#"{"type":"stream_event","event":{"type":"message_start"}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Done."}}}"#,
        ] {
            events.extend(parse_line(line, &mut state));
        }
        assert_eq!(
            events,
            [
                AgentEvent::Text("Reading.".into()),
                AgentEvent::Text("\n\nDone.".into())
            ]
        );
    }

    #[test]
    fn a_denial_without_its_own_line_is_not_also_a_tool_call() {
        let mut state = ParseState::default();
        let mut events = Vec::new();
        for line in [
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"Permission to use Bash has been denied because Claude Code is running in don't ask mode."}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2","is_error":true,"content":[{"type":"text","text":"File not found"}]}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1,"result":"ok","permission_denials":[{"tool_name":"Bash","tool_use_id":"t1","tool_input":{"command":"ls"}}]}"#,
        ] {
            events.extend(parse_line(line, &mut state));
        }
        assert_eq!(
            events,
            [
                AgentEvent::Denied {
                    tool: "Bash".into(),
                    detail: "ls".into()
                },
                AgentEvent::Final {
                    text: "ok".into(),
                    duration_ms: 1,
                    is_error: false
                }
            ]
        );
    }

    #[test]
    fn a_tool_that_failed_for_another_reason_is_still_listed() {
        let mut state = ParseState::default();
        let mut events = Vec::new();
        for line in [
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/tmp/acme-widgets/missing.rs"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":[{"type":"text","text":"File does not exist"}]}]}}"#,
        ] {
            events.extend(parse_line(line, &mut state));
        }
        assert_eq!(
            events,
            [AgentEvent::ToolUse(
                "Read /tmp/acme-widgets/missing.rs".into()
            )]
        );
    }

    #[test]
    fn with_root_sets_only_the_root() {
        let state = ParseState::with_root("/tmp/acme-widgets");
        assert_eq!(state.root, Some(PathBuf::from("/tmp/acme-widgets")));
        assert!(state.text.is_empty() && state.pending.is_empty());
    }

    #[test]
    fn blank_lines_and_known_noise_say_nothing() {
        let mut state = ParseState::default();
        for line in [
            "",
            "   ",
            r#"{"type":"system","subtype":"status","status":"requesting"}"#,
            r#"{"type":"system","subtype":"hook_started","hook_name":"x"}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{}"}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_stop"}}"#,
        ] {
            assert_eq!(parse_line(line, &mut state), [], "{line}");
        }
    }

    #[test]
    fn denials_only_in_the_result_are_still_shown() {
        let mut state = ParseState::default();
        let events = parse_line(
            r#"{"type":"result","subtype":"success","is_error":false,"duration_ms":1,"result":"ok","permission_denials":[{"tool_name":"Bash","tool_use_id":"t9","tool_input":{"command":"ls"}}]}"#,
            &mut state,
        );
        assert_eq!(
            events[0],
            AgentEvent::Denied {
                tool: "Bash".into(),
                detail: "ls".into()
            }
        );
        assert!(matches!(events[1], AgentEvent::Final { .. }));
    }

    #[test]
    fn tool_summaries_are_short_and_relative() {
        let block = |name: &str, input: Value| serde_json::json!({"name": name, "input": input});
        let root = Some(Path::new("/tmp/acme-widgets"));
        let show = |b: Value| describe_tool(&b, root).0;
        assert_eq!(
            show(block(
                "Read",
                serde_json::json!({"file_path": "/tmp/acme-widgets/src/a.rs"})
            )),
            "Read src/a.rs"
        );
        assert_eq!(
            show(block(
                "Read",
                serde_json::json!({"file_path": "/etc/hosts"})
            )),
            "Read /etc/hosts"
        );
        assert_eq!(
            show(block(
                "LS",
                serde_json::json!({"path": "/tmp/acme-widgets/src"})
            )),
            "Listed src"
        );
        assert_eq!(
            show(block(
                "LS",
                serde_json::json!({"path": "/tmp/acme-widgets"})
            )),
            "Listed ."
        );
        assert_eq!(
            show(block("Glob", serde_json::json!({"pattern": "**/*.rs"}))),
            "Searched files matching **/*.rs"
        );
        assert_eq!(
            show(block(
                "WebFetch",
                serde_json::json!({"url": "https://example.com"})
            )),
            "WebFetch"
        );
        let long = "x".repeat(300);
        let cut = show(block("Bash", serde_json::json!({"command": long})));
        assert_eq!(cut.chars().count(), 201);
        assert!(cut.ends_with('…'));
    }
}
