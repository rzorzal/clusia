//! The messages of `codex app-server` (JSON-RPC over stdio, one JSON object per line, no
//! `"jsonrpc"` member): builders for what Clúsia sends, a reader for what Codex sends, and a
//! fold of its notifications into the `AgentEvent`s every sink already consumes. It does no
//! I/O and starts no process.
//!
//! What Codex sends is untrusted: an unknown line, method or shape is ignored, never an error.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use clusia_protocol::AgentErrorKind;
use serde_json::{Value, json};

use crate::Agent;
use crate::parse::{AgentEvent, cut, relative};

/// What every thread starts with: the folder it works in and the role it plays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadParams {
    pub cwd: PathBuf,
    pub developer_instructions: String,
}

impl ThreadParams {
    /// The same on `thread/start`, `thread/resume` and `thread/fork`: a read-only sandbox, every
    /// approval asked of Clúsia (never an AI reviewer from the user's config), and none of the
    /// user's hooks, notifications, MCP servers or `AGENTS.md`.
    fn object(&self) -> serde_json::Map<String, Value> {
        let value = json!({
            "cwd": self.cwd.to_string_lossy(),
            "sandbox": "read-only",
            "approvalPolicy": "on-request",
            "approvalsReviewer": "user",
            "developerInstructions": self.developer_instructions,
            "config": {
                "features": { "hooks": false },
                "mcp_servers": {},
                "notify": [],
                "project_doc_max_bytes": 0,
            },
        });
        match value {
            Value::Object(map) => map,
            _ => unreachable!("a JSON object"),
        }
    }
}

/// The answer to an approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Accept,
    Decline,
    /// Deny and stop the turn.
    Cancel,
}

impl Decision {
    fn wire(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Decline => "decline",
            Self::Cancel => "cancel",
        }
    }
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "id": id, "method": method, "params": params })
}

pub fn initialize(id: u64) -> Value {
    request(
        id,
        "initialize",
        json!({
            "clientInfo": {
                "name": "clusia",
                "title": "Clúsia",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": { "experimentalApi": false },
        }),
    )
}

pub fn initialized() -> Value {
    json!({ "method": "initialized" })
}

pub fn thread_start(id: u64, params: &ThreadParams) -> Value {
    request(id, "thread/start", Value::Object(params.object()))
}

pub fn thread_resume(id: u64, thread_id: &str, params: &ThreadParams) -> Value {
    let mut object = params.object();
    object.insert("threadId".into(), json!(thread_id));
    request(id, "thread/resume", Value::Object(object))
}

pub fn thread_fork(id: u64, thread_id: &str, params: &ThreadParams) -> Value {
    let mut object = params.object();
    object.insert("threadId".into(), json!(thread_id));
    request(id, "thread/fork", Value::Object(object))
}

pub fn turn_start(id: u64, thread_id: &str, prompt: &str) -> Value {
    request(
        id,
        "turn/start",
        json!({
            "threadId": thread_id,
            "input": [{ "type": "text", "text": prompt }],
        }),
    )
}

pub fn turn_interrupt(id: u64, thread_id: &str, turn_id: &str) -> Value {
    request(
        id,
        "turn/interrupt",
        json!({ "threadId": thread_id, "turnId": turn_id }),
    )
}

/// The reply to a server request: `request_id` is the `id` it came with, of any JSON type.
pub fn approval_reply(request_id: &Value, decision: Decision) -> Value {
    json!({ "id": request_id, "result": { "decision": decision.wire() } })
}

/// The reply to a server request that has no decision to give: a JSON-RPC error.
pub fn error_reply(id: &Value, code: i64, message: &str) -> Value {
    json!({ "id": id, "error": { "code": code, "message": message } })
}

/// `message` as one line, ready to write to the process.
pub fn to_line(message: &Value) -> String {
    format!("{message}\n")
}

/// The thread id in the result of `thread/start`, `thread/resume` or `thread/fork`.
pub fn thread_id_of(result: &Value) -> Option<String> {
    result["thread"]["id"].as_str().map(str::to_string)
}

/// The turn id in the result of `turn/start`.
pub fn turn_id_of(result: &Value) -> Option<String> {
    result["turn"]["id"].as_str().map(str::to_string)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// The answer to a request of ours.
    Response {
        id: u64,
        /// The `result`, or the message of the `error`.
        result: Result<Value, String>,
    },
    Notification {
        thread_id: Option<String>,
        note: Note,
    },
    /// Codex asks us something and waits for the reply, which carries `id` back.
    ServerRequest {
        id: Value,
        thread_id: Option<String>,
        ask: ServerAsk,
    },
    /// Not JSON, not an object, or a notification Clúsia does not use.
    Ignored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    ThreadStarted {
        thread_id: String,
    },
    TurnStarted {
        turn_id: String,
    },
    /// A piece of the agent's message (`item/agentMessage/delta`).
    TextDelta(String),
    ItemStarted(ItemInfo),
    ItemCompleted(ItemInfo),
    TurnCompleted {
        status: TurnStatus,
        error: Option<String>,
    },
    /// An error the turn does not recover from.
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemInfo {
    pub id: String,
    pub kind: ItemKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemKind {
    Command {
        command: Option<String>,
    },
    FileChange {
        paths: Vec<String>,
    },
    /// The whole message; empty while it starts.
    AgentMessage {
        text: String,
    },
    /// Any other item type (`userMessage`, `reasoning`, `webSearch`, …).
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerAsk {
    Command {
        turn_id: String,
        item_id: String,
        command: Option<String>,
        cwd: Option<String>,
        reason: Option<String>,
    },
    FileChange {
        turn_id: String,
        item_id: String,
        reason: Option<String>,
        /// A request for write access to a whole folder, not only the item's files.
        grant_root: Option<String>,
    },
    /// Any other request, by method: it gets a refusal.
    Other(String),
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Reads one line Codex wrote. Never fails: what is not understood is `Ignored`, except a
/// request that expects a reply, which always comes back as a `ServerRequest`.
pub fn read_line(line: &str) -> Incoming {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        tracing::debug!("a line from codex is not JSON");
        return Incoming::Ignored;
    };
    let Some(object) = value.as_object() else {
        tracing::debug!("a line from codex is not an object");
        return Incoming::Ignored;
    };
    let params = object.get("params").unwrap_or(&Value::Null);
    let thread_id = text(params, "threadId");
    let method = object.get("method").and_then(Value::as_str);
    let id = object.get("id").filter(|id| !id.is_null());
    match (method, id) {
        (Some(method), Some(id)) => Incoming::ServerRequest {
            id: id.clone(),
            thread_id,
            ask: server_ask(method, params),
        },
        (Some(method), None) => match note(method, params) {
            Some((own_thread, note)) => Incoming::Notification {
                thread_id: own_thread.or(thread_id),
                note,
            },
            None => {
                tracing::debug!(method, "ignored a codex notification");
                Incoming::Ignored
            }
        },
        (None, Some(id)) => {
            let Some(id) = id.as_u64() else {
                tracing::debug!("ignored a codex answer with an id that is not a number");
                return Incoming::Ignored;
            };
            let result = match object.get("error") {
                Some(error) => Err(text(error, "message").unwrap_or_else(|| error.to_string())),
                None => Ok(object.get("result").cloned().unwrap_or(Value::Null)),
            };
            Incoming::Response { id, result }
        }
        (None, None) => {
            tracing::debug!("ignored a codex line with neither a method nor an id");
            Incoming::Ignored
        }
    }
}

fn server_ask(method: &str, params: &Value) -> ServerAsk {
    let turn_id = text(params, "turnId").unwrap_or_default();
    let item_id = text(params, "itemId").unwrap_or_default();
    match method {
        "item/commandExecution/requestApproval" => ServerAsk::Command {
            turn_id,
            item_id,
            command: text(params, "command"),
            cwd: text(params, "cwd"),
            reason: text(params, "reason"),
        },
        "item/fileChange/requestApproval" => ServerAsk::FileChange {
            turn_id,
            item_id,
            reason: text(params, "reason"),
            grant_root: text(params, "grantRoot"),
        },
        other => ServerAsk::Other(other.to_string()),
    }
}

/// The note for `method` and the thread it names itself, when it names one.
fn note(method: &str, params: &Value) -> Option<(Option<String>, Note)> {
    match method {
        "thread/started" => {
            let thread_id = text(&params["thread"], "id")?;
            Some((Some(thread_id.clone()), Note::ThreadStarted { thread_id }))
        }
        "turn/started" => Some((
            None,
            Note::TurnStarted {
                turn_id: text(&params["turn"], "id")?,
            },
        )),
        "item/agentMessage/delta" => Some((None, Note::TextDelta(text(params, "delta")?))),
        "item/started" => Some((None, Note::ItemStarted(item(&params["item"])?))),
        "item/completed" => Some((None, Note::ItemCompleted(item(&params["item"])?))),
        "turn/completed" => {
            let turn = &params["turn"];
            // A status Clúsia does not know ends the turn as a failure: waiting for a better
            // one would run the turn to its deadline.
            let status = match turn["status"].as_str().unwrap_or_default() {
                "completed" => TurnStatus::Completed,
                "interrupted" => TurnStatus::Interrupted,
                _ => TurnStatus::Failed,
            };
            Some((
                None,
                Note::TurnCompleted {
                    status,
                    error: text(&turn["error"], "message"),
                },
            ))
        }
        "error" => {
            if params["willRetry"].as_bool() == Some(true) {
                return None;
            }
            let message = text(&params["error"], "message")?;
            Some((None, Note::Error(message)))
        }
        _ => None,
    }
}

fn item(item: &Value) -> Option<ItemInfo> {
    let id = text(item, "id")?;
    let kind = match item["type"].as_str()? {
        "agentMessage" => ItemKind::AgentMessage {
            text: text(item, "text").unwrap_or_default(),
        },
        "commandExecution" => ItemKind::Command {
            command: text(item, "command"),
        },
        "fileChange" => ItemKind::FileChange {
            paths: item["changes"]
                .as_array()
                .map(|changes| changes.iter().filter_map(|c| text(c, "path")).collect())
                .unwrap_or_default(),
        },
        other => ItemKind::Other(other.to_string()),
    };
    Some(ItemInfo { id, kind })
}

/// What a failed turn's message means: not signed in, a usage limit, or anything else.
pub fn classify_codex(message: &str) -> AgentErrorKind {
    let text = message.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    if has(&[
        "not logged in",
        "not signed in",
        "log in",
        "login",
        "sign in",
        "unauthorized",
        "invalid api key",
        "authentication",
    ]) {
        AgentErrorKind::NotSignedIn
    } else if has(&[
        "usage limit",
        "rate limit",
        "rate_limit",
        "limit reached",
        "quota",
    ]) {
        AgentErrorKind::UsageLimit
    } else {
        AgentErrorKind::Crashed
    }
}

/// Folds the notifications of one turn into the events the sinks read.
#[derive(Debug)]
pub struct CodexTurn {
    root: PathBuf,
    started: Instant,
    /// Every delta so far, in order.
    streamed: String,
    /// The last agent message that completed whole.
    last_message: Option<String>,
    /// A message started after text was already said: its first delta opens a new paragraph.
    paragraph_break: bool,
    /// Deltas seen since the current agent message started.
    deltas_in_message: u32,
    announced: Vec<String>,
    paths: HashMap<String, Vec<String>>,
    errored: bool,
}

impl CodexTurn {
    /// `root` is the review's worktree: paths under it are shown relative to it.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            started: Instant::now(),
            streamed: String::new(),
            last_message: None,
            paragraph_break: false,
            deltas_in_message: 0,
            announced: Vec::new(),
            paths: HashMap::new(),
            errored: false,
        }
    }

    /// The events `note` makes. `thread/started`, `turn/started` and an interrupted turn make
    /// none: the caller knows what it asked for.
    pub fn on(&mut self, note: Note) -> Vec<AgentEvent> {
        match note {
            Note::ThreadStarted { .. } | Note::TurnStarted { .. } => Vec::new(),
            Note::TextDelta(delta) => {
                let mut said = String::new();
                if std::mem::take(&mut self.paragraph_break) {
                    said.push_str("\n\n");
                }
                said.push_str(&delta);
                self.deltas_in_message += 1;
                self.streamed.push_str(&said);
                vec![AgentEvent::Text(said)]
            }
            Note::ItemStarted(info) => self.item_started(info),
            Note::ItemCompleted(info) => self.item_completed(info),
            Note::TurnCompleted { status, error } => self.turn_completed(status, error),
            Note::Error(_) if self.errored => Vec::new(),
            Note::Error(message) => {
                self.errored = true;
                vec![AgentEvent::Error {
                    kind: classify_codex(&message),
                    message,
                }]
            }
        }
    }

    /// The paths of the file change `item_id`, relative to the worktree where they are under it.
    pub fn paths_of(&self, item_id: &str) -> Vec<String> {
        self.paths.get(item_id).cloned().unwrap_or_default()
    }

    fn shown(&self, paths: &[String]) -> Vec<String> {
        paths
            .iter()
            .map(|p| relative(p, Some(&self.root)))
            .collect()
    }

    fn announce(&mut self, id: &str, summary: String) -> Vec<AgentEvent> {
        if self.announced.iter().any(|seen| seen == id) {
            return Vec::new();
        }
        self.announced.push(id.to_string());
        vec![AgentEvent::ToolUse(cut(&summary))]
    }

    fn item_started(&mut self, info: ItemInfo) -> Vec<AgentEvent> {
        match info.kind {
            ItemKind::AgentMessage { .. } => {
                self.paragraph_break = !self.streamed.is_empty();
                self.deltas_in_message = 0;
                Vec::new()
            }
            ItemKind::Command { command } => {
                let summary = ran(command);
                self.announce(&info.id, summary)
            }
            ItemKind::FileChange { paths } => {
                let shown = self.shown(&paths);
                if !shown.is_empty() {
                    self.paths.insert(info.id.clone(), shown.clone());
                }
                self.announce(&info.id, edited(&shown))
            }
            ItemKind::Other(_) => Vec::new(),
        }
    }

    fn item_completed(&mut self, info: ItemInfo) -> Vec<AgentEvent> {
        match info.kind {
            ItemKind::AgentMessage { text } => {
                if text.trim().is_empty() {
                    return Vec::new();
                }
                let mut events = Vec::new();
                if self.deltas_in_message == 0 {
                    let mut said = String::new();
                    if std::mem::take(&mut self.paragraph_break) {
                        said.push_str("\n\n");
                    }
                    said.push_str(&text);
                    self.streamed.push_str(&said);
                    events.push(AgentEvent::Text(said));
                }
                self.last_message = Some(text);
                events
            }
            ItemKind::FileChange { paths } => {
                let shown = self.shown(&paths);
                if !shown.is_empty() && !self.paths.contains_key(&info.id) {
                    self.paths.insert(info.id.clone(), shown);
                }
                let seen = self.paths_of(&info.id);
                self.announce(&info.id, edited(&seen))
            }
            ItemKind::Command { command } => {
                let summary = ran(command);
                self.announce(&info.id, summary)
            }
            ItemKind::Other(_) => Vec::new(),
        }
    }

    fn turn_completed(&mut self, status: TurnStatus, error: Option<String>) -> Vec<AgentEvent> {
        match status {
            TurnStatus::Completed => vec![AgentEvent::Final {
                text: self
                    .last_message
                    .clone()
                    .unwrap_or_else(|| self.streamed.clone()),
                duration_ms: self.started.elapsed().as_millis() as u64,
                is_error: false,
            }],
            TurnStatus::Interrupted => Vec::new(),
            TurnStatus::Failed if self.errored => Vec::new(),
            TurnStatus::Failed => {
                let message = error
                    .filter(|m| !m.trim().is_empty())
                    .unwrap_or_else(|| Agent::Codex.message_for(AgentErrorKind::Crashed));
                vec![AgentEvent::Error {
                    kind: classify_codex(&message),
                    message,
                }]
            }
        }
    }
}

/// `Ran `cargo test``, or `Ran a command` when Codex did not say which.
fn ran(command: Option<String>) -> String {
    match command {
        Some(command) => format!("Ran `{command}`"),
        None => "Ran a command".to_string(),
    }
}

/// `Edited a.rs, b.rs` for the paths of one file change.
fn edited(paths: &[String]) -> String {
    if paths.is_empty() {
        "Edited files".to_string()
    } else {
        format!("Edited {}", paths.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn fixture(name: &str) -> Vec<String> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex")
            .join(format!("{name}.jsonl"));
        fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read fixture {}: {e}", path.display()))
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// What `lines` make, with the thread ids dropped.
    fn events(turn: &mut CodexTurn, lines: &[String]) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        for line in lines {
            if let Incoming::Notification { note, .. } = read_line(line) {
                out.extend(turn.on(note));
            }
        }
        out
    }

    fn params() -> ThreadParams {
        ThreadParams {
            cwd: PathBuf::from("/work/acme-widgets"),
            developer_instructions: "Be brief.".into(),
        }
    }

    #[test]
    fn the_handshake_names_clusia() {
        let init = initialize(0);
        assert_eq!(init["id"], 0);
        assert_eq!(init["method"], "initialize");
        assert_eq!(init["params"]["clientInfo"]["name"], "clusia");
        assert_eq!(
            init["params"]["clientInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(init["params"]["capabilities"]["experimentalApi"], false);
        assert_eq!(initialized(), json!({ "method": "initialized" }));
    }

    #[test]
    fn every_thread_is_read_only_asks_clusia_and_leaves_the_users_setup_out() {
        let expected = |extra: Value| {
            let mut base = json!({
                "cwd": "/work/acme-widgets",
                "sandbox": "read-only",
                "approvalPolicy": "on-request",
                "approvalsReviewer": "user",
                "developerInstructions": "Be brief.",
                "config": {
                    "features": { "hooks": false },
                    "mcp_servers": {},
                    "notify": [],
                    "project_doc_max_bytes": 0,
                },
            });
            if let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
                base.extend(extra.clone());
            }
            base
        };
        let start = thread_start(1, &params());
        assert_eq!(start["id"], 1);
        assert_eq!(start["method"], "thread/start");
        assert_eq!(start["params"], expected(json!({})));
        let resume = thread_resume(2, "thr-1", &params());
        assert_eq!(resume["method"], "thread/resume");
        assert_eq!(resume["params"], expected(json!({ "threadId": "thr-1" })));
        let fork = thread_fork(3, "thr-1", &params());
        assert_eq!(fork["method"], "thread/fork");
        assert_eq!(fork["params"], expected(json!({ "threadId": "thr-1" })));
    }

    #[test]
    fn a_turn_carries_the_prompt_and_an_interrupt_names_both_ids() {
        let turn = turn_start(4, "thr-1", "What changed?");
        assert_eq!(turn["method"], "turn/start");
        assert_eq!(turn["params"]["threadId"], "thr-1");
        assert_eq!(turn["params"]["input"][0]["type"], "text");
        assert_eq!(
            turn["params"]["input"],
            json!([{ "type": "text", "text": "What changed?" }]),
            "no member the research does not name"
        );
        let stop = turn_interrupt(5, "thr-1", "turn-9");
        assert_eq!(stop["method"], "turn/interrupt");
        assert_eq!(
            stop["params"],
            json!({ "threadId": "thr-1", "turnId": "turn-9" })
        );
    }

    #[test]
    fn replies_echo_the_request_id_and_only_use_the_three_decisions() {
        for (decision, wire) in [
            (Decision::Accept, "accept"),
            (Decision::Decline, "decline"),
            (Decision::Cancel, "cancel"),
        ] {
            assert_eq!(
                approval_reply(&json!(7), decision),
                json!({ "id": 7, "result": { "decision": wire } })
            );
        }
        assert_eq!(
            approval_reply(&json!("srv-3"), Decision::Decline)["id"],
            "srv-3"
        );
        let line = to_line(&initialized());
        assert!(line.ends_with('\n') && !line[..line.len() - 1].contains('\n'));
    }

    #[test]
    fn responses_carry_a_result_or_an_error_message() {
        let lines = fixture("responses");
        let Incoming::Response { id, result } = read_line(&lines[1]) else {
            panic!("a response");
        };
        assert_eq!(id, 1);
        assert_eq!(
            thread_id_of(&result.expect("a result")).as_deref(),
            Some("thr-1")
        );
        let Incoming::Response { result, .. } = read_line(&lines[2]) else {
            panic!("a response");
        };
        assert_eq!(turn_id_of(&result.unwrap()).as_deref(), Some("turn-1"));
        assert_eq!(
            read_line(&lines[3]),
            Incoming::Response {
                id: 3,
                result: Err("thread not found: thr-9".into())
            }
        );
        assert_eq!(
            read_line(&lines[4]),
            Incoming::Response {
                id: 4,
                result: Ok(json!({}))
            }
        );
        assert_eq!(
            read_line(&lines[5]),
            Incoming::Ignored,
            "a string id is not ours"
        );
    }

    #[test]
    fn a_chat_turn_streams_text_and_ends_with_the_whole_message() {
        let mut turn = CodexTurn::new(PathBuf::from("/work/acme-widgets"));
        let lines = fixture("chat_turn");
        let got = events(&mut turn, &lines);
        let Some(AgentEvent::Final { text, is_error, .. }) = got.last().cloned() else {
            panic!("ends with Final: {got:?}");
        };
        assert_eq!(
            (text.as_str(), is_error),
            ("The change refreshes tokens.", false)
        );
        assert_eq!(
            &got[..got.len() - 1],
            [
                AgentEvent::ToolUse("Ran `rg -n refresh src`".into()),
                AgentEvent::Text("The change ".into()),
                AgentEvent::Text("refreshes tokens.".into()),
            ]
        );
    }

    #[test]
    fn the_first_note_of_a_thread_names_it() {
        let lines = fixture("chat_turn");
        assert_eq!(
            read_line(&lines[0]),
            Incoming::Notification {
                thread_id: Some("0199a213-81c0-7800-8aa1-bbab2a035a53".into()),
                note: Note::ThreadStarted {
                    thread_id: "0199a213-81c0-7800-8aa1-bbab2a035a53".into()
                }
            }
        );
        assert_eq!(
            read_line(&lines[1]),
            Incoming::Notification {
                thread_id: Some("0199a213-81c0-7800-8aa1-bbab2a035a53".into()),
                note: Note::TurnStarted {
                    turn_id: "turn-1".into()
                }
            }
        );
        assert!(matches!(
            read_line(&lines[7]),
            Incoming::Notification {
                note: Note::ItemCompleted(ItemInfo {
                    kind: ItemKind::Command { .. },
                    ..
                }),
                ..
            }
        ));
    }

    #[test]
    fn a_message_that_never_streamed_is_said_when_it_completes() {
        let mut turn = CodexTurn::new(PathBuf::from("/w"));
        let done = |text: &str| {
            Note::ItemCompleted(ItemInfo {
                id: "m1".into(),
                kind: ItemKind::AgentMessage { text: text.into() },
            })
        };
        assert_eq!(turn.on(done("Hello.")), [AgentEvent::Text("Hello.".into())]);
        let end = turn.on(Note::TurnCompleted {
            status: TurnStatus::Completed,
            error: None,
        });
        assert!(matches!(
            &end[..],
            [AgentEvent::Final { text, is_error: false, .. }] if text == "Hello."
        ));
    }

    #[test]
    fn a_second_message_opens_a_new_paragraph_and_the_last_one_is_the_answer() {
        let mut turn = CodexTurn::new(PathBuf::from("/w"));
        let started = |id: &str| {
            Note::ItemStarted(ItemInfo {
                id: id.into(),
                kind: ItemKind::AgentMessage {
                    text: String::new(),
                },
            })
        };
        turn.on(started("m1"));
        turn.on(Note::TextDelta("First.".into()));
        turn.on(started("m2"));
        assert_eq!(
            turn.on(Note::TextDelta("Second.".into())),
            [AgentEvent::Text("\n\nSecond.".into())]
        );
        turn.on(Note::ItemCompleted(ItemInfo {
            id: "m2".into(),
            kind: ItemKind::AgentMessage {
                text: "Second.".into(),
            },
        }));
        let end = turn.on(Note::TurnCompleted {
            status: TurnStatus::Completed,
            error: None,
        });
        assert!(matches!(
            &end[..],
            [AgentEvent::Final { text, .. }] if text == "Second."
        ));
    }

    #[test]
    fn deltas_alone_make_the_answer_when_no_message_completes() {
        let mut turn = CodexTurn::new(PathBuf::from("/w"));
        turn.on(Note::TextDelta("Half an ".into()));
        turn.on(Note::TextDelta("answer".into()));
        let end = turn.on(Note::TurnCompleted {
            status: TurnStatus::Completed,
            error: None,
        });
        assert!(matches!(
            &end[..],
            [AgentEvent::Final { text, .. }] if text == "Half an answer"
        ));
    }

    #[test]
    fn a_command_approval_names_the_turn_the_item_and_the_command() {
        let lines = fixture("command_approval");
        let Incoming::ServerRequest { id, thread_id, ask } = read_line(&lines[2]) else {
            panic!("a server request");
        };
        assert_eq!(id, json!(0));
        assert_eq!(thread_id.as_deref(), Some("thr-1"));
        assert_eq!(
            ask,
            ServerAsk::Command {
                turn_id: "turn-2".into(),
                item_id: "call-7".into(),
                command: Some("cargo test -p widgets-core".into()),
                cwd: Some("/work/acme-widgets".into()),
                reason: Some("Needs to run the tests".into()),
            }
        );
        // The rest of the exchange makes a tool use, then a normal end.
        let mut turn = CodexTurn::new(PathBuf::from("/work/acme-widgets"));
        let got = events(&mut turn, &lines);
        assert_eq!(
            got[0],
            AgentEvent::ToolUse("Ran `cargo test -p widgets-core`".into())
        );
        assert!(matches!(
            &got[1],
            AgentEvent::Final {
                is_error: false,
                ..
            }
        ));
        assert_eq!(got.len(), 2, "serverRequest/resolved says nothing");
    }

    #[test]
    fn a_file_change_approval_finds_its_paths_on_the_item() {
        let lines = fixture("file_change_approval");
        let Incoming::ServerRequest { id, ask, .. } = read_line(&lines[1]) else {
            panic!("a server request");
        };
        assert_eq!(id, json!(1));
        assert_eq!(
            ask,
            ServerAsk::FileChange {
                turn_id: "turn-3".into(),
                item_id: "call-9".into(),
                reason: Some("Write the fix".into()),
                grant_root: None,
            }
        );
        let mut turn = CodexTurn::new(PathBuf::from("/work/acme-widgets"));
        let got = events(&mut turn, &lines);
        assert_eq!(
            got,
            [AgentEvent::ToolUse(
                "Edited src/auth/refresh.rs, notes.md".into()
            )]
        );
        assert_eq!(
            turn.paths_of("call-9"),
            ["src/auth/refresh.rs", "notes.md"],
            "the completed item with one change does not replace the first sight"
        );
        assert!(turn.paths_of("nope").is_empty());
    }

    #[test]
    fn a_file_change_that_asks_for_a_whole_folder_says_which() {
        let Incoming::ServerRequest { ask, .. } = read_line(
            r#"{"id":5,"method":"item/fileChange/requestApproval","params":{"threadId":"t","turnId":"u","itemId":"i","startedAtMs":1,"grantRoot":"/etc"}}"#,
        ) else {
            panic!("a server request");
        };
        assert_eq!(
            ask,
            ServerAsk::FileChange {
                turn_id: "u".into(),
                item_id: "i".into(),
                reason: None,
                grant_root: Some("/etc".into()),
            }
        );
    }

    #[test]
    fn a_turn_that_ends_with_a_status_nobody_knows_has_failed() {
        let Incoming::Notification { note, .. } = read_line(
            r#"{"method":"turn/completed","params":{"threadId":"t","turn":{"id":"u","items":[],"status":"exploded","error":{"message":"boom"}}}}"#,
        ) else {
            panic!("a notification");
        };
        assert_eq!(
            note,
            Note::TurnCompleted {
                status: TurnStatus::Failed,
                error: Some("boom".into())
            }
        );
        let Incoming::Notification { note, .. } =
            read_line(r#"{"method":"turn/completed","params":{"threadId":"t","turn":{"id":"u"}}}"#)
        else {
            panic!("a notification");
        };
        assert_eq!(
            note,
            Note::TurnCompleted {
                status: TurnStatus::Failed,
                error: None
            },
            "no status at all reads as a failure, like an unknown one"
        );
    }

    #[test]
    fn paths_outside_the_worktree_stay_as_they_are() {
        let mut turn = CodexTurn::new(PathBuf::from("/work/acme-widgets"));
        turn.on(Note::ItemStarted(ItemInfo {
            id: "c".into(),
            kind: ItemKind::FileChange {
                paths: vec!["/etc/hosts".into(), "/work/acme-widgets".into()],
            },
        }));
        assert_eq!(turn.paths_of("c"), ["/etc/hosts", "."]);
    }

    #[test]
    fn requests_with_no_decision_come_back_as_other_so_they_can_be_refused() {
        let lines = fixture("unsupported_requests");
        let asks: Vec<(Value, ServerAsk)> = lines
            .iter()
            .map(|line| match read_line(line) {
                Incoming::ServerRequest { id, ask, .. } => (id, ask),
                other => panic!("a server request, got {other:?}"),
            })
            .collect();
        assert_eq!(
            asks,
            [
                (
                    json!(2),
                    ServerAsk::Other("item/permissions/requestApproval".into())
                ),
                (
                    json!("srv-3"),
                    ServerAsk::Other("mcpServer/elicitation/request".into())
                ),
                (json!(4), ServerAsk::Other("tool/requestUserInput".into())),
            ]
        );
    }

    #[test]
    fn a_request_without_ids_inside_is_still_a_request() {
        let Incoming::ServerRequest { id, ask, thread_id } =
            read_line(r#"{"id":9,"method":"item/commandExecution/requestApproval"}"#)
        else {
            panic!("a server request");
        };
        assert_eq!((id, thread_id), (json!(9), None));
        assert_eq!(
            ask,
            ServerAsk::Command {
                turn_id: String::new(),
                item_id: String::new(),
                command: None,
                cwd: None,
                reason: None,
            }
        );
    }

    #[test]
    fn unknown_and_broken_lines_are_ignored() {
        for line in fixture("unknown_lines") {
            assert_eq!(read_line(&line), Incoming::Ignored, "{line}");
        }
        assert_eq!(read_line(""), Incoming::Ignored);
        assert_eq!(read_line("   "), Incoming::Ignored);
    }

    #[test]
    fn unknown_items_say_nothing_and_a_known_one_without_an_id_is_ignored() {
        let mut turn = CodexTurn::new(PathBuf::from("/w"));
        let lines = fixture("unknown_lines");
        assert!(events(&mut turn, &lines).is_empty());
        assert_eq!(
            read_line(
                r#"{"method":"item/started","params":{"threadId":"t","turnId":"u","item":{"type":"webSearch","id":"i","query":"x"}}}"#
            ),
            Incoming::Notification {
                thread_id: Some("t".into()),
                note: Note::ItemStarted(ItemInfo {
                    id: "i".into(),
                    kind: ItemKind::Other("webSearch".into())
                })
            }
        );
    }

    #[test]
    fn a_usage_limit_ends_the_turn_with_one_error() {
        let mut turn = CodexTurn::new(PathBuf::from("/w"));
        let got = events(&mut turn, &fixture("failed_usage_limit"));
        assert_eq!(
            got,
            [AgentEvent::Error {
                kind: AgentErrorKind::UsageLimit,
                message: "You've hit your usage limit. Try again later.".into()
            }],
            "the error notification and the failed turn are one failure"
        );
    }

    #[test]
    fn a_failed_turn_without_a_message_or_a_login_says_what_it_can() {
        let mut turn = CodexTurn::new(PathBuf::from("/w"));
        let got = events(&mut turn, &fixture("not_signed_in"));
        assert_eq!(
            got,
            [AgentEvent::Error {
                kind: AgentErrorKind::NotSignedIn,
                message: "Not logged in. Run codex login.".into()
            }]
        );
        let mut turn = CodexTurn::new(PathBuf::from("/w"));
        assert_eq!(
            turn.on(Note::TurnCompleted {
                status: TurnStatus::Failed,
                error: None
            }),
            [AgentEvent::Error {
                kind: AgentErrorKind::Crashed,
                message: "Codex stopped unexpectedly".into()
            }]
        );
    }

    #[test]
    fn an_interrupted_turn_makes_no_final() {
        let mut turn = CodexTurn::new(PathBuf::from("/w"));
        let got = events(&mut turn, &fixture("interrupted"));
        assert_eq!(got, [AgentEvent::Text("Looking at".into())]);
    }

    #[test]
    fn a_second_error_of_the_turn_says_nothing() {
        let mut turn = CodexTurn::new(PathBuf::from("/w"));
        assert_eq!(turn.on(Note::Error("first".into())).len(), 1);
        assert!(turn.on(Note::Error("second".into())).is_empty());
    }

    #[test]
    fn a_file_change_that_starts_without_paths_does_not_hide_the_ones_that_complete_it() {
        let mut turn = CodexTurn::new(PathBuf::from("/work/acme-widgets"));
        turn.on(Note::ItemStarted(ItemInfo {
            id: "i".into(),
            kind: ItemKind::FileChange { paths: Vec::new() },
        }));
        turn.on(Note::ItemCompleted(ItemInfo {
            id: "i".into(),
            kind: ItemKind::FileChange {
                paths: vec!["/work/acme-widgets/src/a.rs".into()],
            },
        }));
        assert_eq!(turn.paths_of("i"), ["src/a.rs"]);
    }

    #[test]
    fn a_command_without_its_text_is_a_command() {
        let mut turn = CodexTurn::new(PathBuf::from("/w"));
        let got = turn.on(Note::ItemStarted(ItemInfo {
            id: "c".into(),
            kind: ItemKind::Command { command: None },
        }));
        assert_eq!(got, [AgentEvent::ToolUse("Ran a command".into())]);
    }

    #[test]
    fn an_error_reply_carries_the_code_and_the_message() {
        assert_eq!(
            error_reply(&json!(7), -32601, "no such method"),
            json!({"id": 7, "error": {"code": -32601, "message": "no such method"}})
        );
    }

    #[test]
    fn messages_are_classified_by_what_they_say() {
        for (said, kind) in [
            ("Not logged in", AgentErrorKind::NotSignedIn),
            ("Please run codex login again", AgentErrorKind::NotSignedIn),
            ("401 Unauthorized", AgentErrorKind::NotSignedIn),
            ("Invalid API key", AgentErrorKind::NotSignedIn),
            ("You've hit your usage limit", AgentErrorKind::UsageLimit),
            ("Rate limit exceeded", AgentErrorKind::UsageLimit),
            ("quota exceeded", AgentErrorKind::UsageLimit),
            ("the model crashed", AgentErrorKind::Crashed),
            ("", AgentErrorKind::Crashed),
        ] {
            assert_eq!(classify_codex(said), kind, "{said}");
        }
    }

    #[test]
    fn a_long_command_is_cut_like_a_claude_tool_use() {
        let mut turn = CodexTurn::new(PathBuf::from("/w"));
        let long = "x".repeat(500);
        let got = turn.on(Note::ItemStarted(ItemInfo {
            id: "c".into(),
            kind: ItemKind::Command {
                command: Some(long),
            },
        }));
        let [AgentEvent::ToolUse(summary)] = &got[..] else {
            panic!("one tool use");
        };
        assert_eq!(summary.chars().count(), 201);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn a_command_is_announced_once_however_many_times_it_is_seen() {
        let mut turn = CodexTurn::new(PathBuf::from("/w"));
        let info = ItemInfo {
            id: "c".into(),
            kind: ItemKind::Command {
                command: Some("ls".into()),
            },
        };
        assert_eq!(turn.on(Note::ItemStarted(info.clone())).len(), 1);
        assert!(turn.on(Note::ItemCompleted(info)).is_empty());
    }
}
