//! `clusiad permission-bridge`: the stdio MCP server that `claude` starts for each turn. It
//! offers one tool, `approve`, which `--permission-prompt-tool` calls whenever the agent wants
//! something it is not already allowed to do. The bridge decides nothing: it forwards the
//! request to the daemon over its socket and returns the daemon's answer. Whatever goes wrong
//! (no daemon, a refusal, a broken reply) is a denial, never an approval.

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use clusia_core::PrRef;
use clusia_protocol::{Client, ClientError, Command, MAX_LINE_BYTES, Reply};
use serde_json::{Value, json};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use tokio::sync::Mutex;
use tokio::task::JoinSet;

/// The name this connection gives the daemon in its hello.
const CLIENT_NAME: &str = "permission-bridge";
/// The MCP protocol version answered when the client names none.
const FALLBACK_PROTOCOL: &str = "2025-06-18";
/// The one tool the bridge offers; `--permission-prompt-tool mcp__clusia__approve` names it.
const TOOL: &str = "approve";
/// Said to the agent when a request is denied without a reason of its own.
const DENIED: &str = "The reviewer denied this request.";

/// What the daemon decided about one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub allow: bool,
    pub message: Option<String>,
}

impl Verdict {
    fn deny(message: String) -> Self {
        Self {
            allow: false,
            message: Some(message),
        }
    }
}

/// Where a request goes to be decided.
pub(crate) trait Forward: Send + Sync + 'static {
    fn forward(&self, tool: &str, input: &Value) -> impl Future<Output = Verdict> + Send;
}

/// Asks the daemon over its Unix socket, on a connection of its own for each request.
pub(crate) struct SocketForward {
    pub socket: PathBuf,
    pub pr: PrRef,
    pub turn: u64,
}

impl Forward for SocketForward {
    async fn forward(&self, tool: &str, input: &Value) -> Verdict {
        let unreachable = |why: String| {
            Verdict::deny(format!(
                "Clúsia could not ask the reviewer ({why}), so the request is denied."
            ))
        };
        let mut client = match Client::connect(&self.socket, CLIENT_NAME).await {
            Ok(client) => client,
            Err(e) => return unreachable(e.to_string()),
        };
        let ask = Command::PermissionAsk {
            pr: self.pr.clone(),
            turn: self.turn,
            tool: tool.to_string(),
            input: input.clone(),
        };
        match client.request(ask).await {
            Ok(Reply::PermissionDecision { allow, message }) => Verdict { allow, message },
            Ok(_) => unreachable("the daemon gave an unexpected answer".to_string()),
            Err(ClientError::Server(e)) => {
                Verdict::deny(format!("Clúsia refused the request: {}", e.message))
            }
            Err(e) => unreachable(e.to_string()),
        }
    }
}

/// What one line from the agent asks the bridge to do.
#[derive(Debug, PartialEq)]
enum Step {
    /// A notification, a response or something that is not JSON-RPC: nothing to say.
    Skip,
    Reply(Value),
    /// A `tools/call`: the answer waits for the daemon.
    Call {
        id: Value,
        params: Value,
    },
}

fn result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn failure(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn tool_list() -> Value {
    json!({"tools": [{
        "name": TOOL,
        "description": "Asks the reviewer whether the agent may use a tool.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "tool_name": {"type": "string"},
                "input": {"type": "object"},
                "tool_use_id": {"type": "string"},
            },
            "required": ["tool_name", "input"],
        },
    }]})
}

fn route(message: &Value) -> Step {
    let (Some(method), Some(id)) = (
        message.get("method").and_then(Value::as_str),
        message.get("id"),
    ) else {
        return Step::Skip;
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => {
            let protocol = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(FALLBACK_PROTOCOL);
            Step::Reply(result(
                id,
                json!({
                    "protocolVersion": protocol,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "clusia", "version": crate::VERSION},
                }),
            ))
        }
        "tools/list" => Step::Reply(result(id, tool_list())),
        "tools/call" => Step::Call {
            id: id.clone(),
            params,
        },
        // `server/discover` and anything else a client may ask: an empty answer is enough.
        _ => Step::Reply(result(id, json!({}))),
    }
}

/// The text of the tool result `claude` reads: `allow` with the input it may run, or `deny`
/// with what to tell the agent.
fn decision_text(verdict: &Verdict, input: &Value) -> String {
    if verdict.allow {
        json!({"behavior": "allow", "updatedInput": input}).to_string()
    } else {
        let message = verdict.message.as_deref().unwrap_or(DENIED);
        json!({"behavior": "deny", "message": message}).to_string()
    }
}

async fn answer_call<F: Forward>(forward: &F, id: Value, params: Value) -> Value {
    if params.get("name").and_then(Value::as_str) != Some(TOOL) {
        return failure(&id, -32602, "unknown tool");
    }
    let arguments = params.get("arguments").unwrap_or(&Value::Null);
    let tool = arguments.get("tool_name").and_then(Value::as_str);
    let (verdict, input) = match (tool, arguments.get("input")) {
        (Some(tool), Some(input)) => (forward.forward(tool, input).await, input.clone()),
        (None, _) => (
            Verdict::deny("The request did not say which tool it is for.".into()),
            json!({}),
        ),
        (Some(_), None) => (
            Verdict::deny("The request did not carry the tool's input.".into()),
            json!({}),
        ),
    };
    let text = decision_text(&verdict, &input);
    result(&id, json!({"content": [{"type": "text", "text": text}]}))
}

async fn write_line<W: AsyncWrite + Unpin>(out: &Mutex<W>, value: &Value) -> io::Result<()> {
    let mut line = value.to_string();
    line.push('\n');
    let mut out = out.lock().await;
    out.write_all(line.as_bytes()).await?;
    out.flush().await
}

/// Reads one line into `line`, at most `MAX_LINE_BYTES` of it. `None` at the end of the
/// input; `Some(false)` for a line that was too long, which is read to its end and dropped.
async fn read_line<R: AsyncBufRead + Unpin>(
    input: &mut R,
    line: &mut Vec<u8>,
) -> io::Result<Option<bool>> {
    line.clear();
    let n = (&mut *input)
        .take(MAX_LINE_BYTES + 1)
        .read_until(b'\n', line)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    // A newline within the limit ends the line; so does the end of the input.
    if line.ends_with(b"\n") || line.len() as u64 <= MAX_LINE_BYTES {
        return Ok(Some(true));
    }
    while !line.ends_with(b"\n") {
        line.clear();
        if (&mut *input)
            .take(MAX_LINE_BYTES)
            .read_until(b'\n', line)
            .await?
            == 0
        {
            break;
        }
    }
    line.clear();
    Ok(Some(false))
}

/// Serves one MCP client until its input ends. Calls run side by side, so a request waiting
/// for the reviewer never holds up the others; when the input ends, the ones still waiting are
/// dropped with the bridge.
pub(crate) async fn serve<R, W, F>(mut input: R, output: W, forward: F) -> io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
    F: Forward,
{
    let forward = Arc::new(forward);
    let output = Arc::new(Mutex::new(output));
    let mut calls = JoinSet::new();
    let mut line = Vec::new();
    while let Some(fits) = read_line(&mut input, &mut line).await? {
        if !fits {
            continue;
        }
        let Ok(message) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        match route(&message) {
            Step::Skip => {}
            Step::Reply(reply) => write_line(&output, &reply).await?,
            Step::Call { id, params } => {
                let (forward, output) = (forward.clone(), output.clone());
                calls.spawn(async move {
                    let reply = answer_call(&*forward, id, params).await;
                    let _ = write_line(&output, &reply).await;
                });
            }
        }
    }
    calls.abort_all();
    Ok(())
}

/// `clusiad permission-bridge`: serves standard input and output, asking the daemon at
/// `socket` on behalf of turn `turn` of `pr`.
pub async fn run_permission_bridge(socket: PathBuf, pr: PrRef, turn: u64) -> io::Result<()> {
    serve(
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
        SocketForward { socket, pr, turn },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_protocol::{
        ClientMessage, ErrorCode, MessageReader, Outcome, ProtocolError, ServerMessage,
        write_message,
    };
    use tokio::io::{AsyncReadExt, DuplexStream, duplex};
    use tokio::net::UnixListener;

    /// Answers by a rule, remembering what it was asked.
    struct Scripted {
        asked: std::sync::Mutex<Vec<(String, Value)>>,
        verdict: Verdict,
    }

    impl Scripted {
        fn new(allow: bool, message: Option<&str>) -> Arc<Self> {
            Arc::new(Self {
                asked: Default::default(),
                verdict: Verdict {
                    allow,
                    message: message.map(str::to_string),
                },
            })
        }
    }

    impl Forward for Arc<Scripted> {
        async fn forward(&self, tool: &str, input: &Value) -> Verdict {
            self.asked
                .lock()
                .unwrap()
                .push((tool.to_string(), input.clone()));
            self.verdict.clone()
        }
    }

    /// Never answers, like a reviewer who is away.
    struct Silent;

    impl Forward for Silent {
        async fn forward(&self, _: &str, _: &Value) -> Verdict {
            std::future::pending().await
        }
    }

    /// A bridge served over in-memory pipes: write requests to `send`, read replies from
    /// `replies`, shut `send` down to end the input.
    struct Pipes {
        send: DuplexStream,
        replies: BufReader<DuplexStream>,
        done: tokio::task::JoinHandle<io::Result<()>>,
    }

    fn pipes<F: Forward>(forward: F) -> Pipes {
        let (send, bridge_in) = duplex(64 * 1024);
        let (bridge_out, replies) = duplex(64 * 1024);
        let done = tokio::spawn(serve(BufReader::new(bridge_in), bridge_out, forward));
        Pipes {
            send,
            replies: BufReader::new(replies),
            done,
        }
    }

    impl Pipes {
        async fn say(&mut self, message: Value) {
            self.send
                .write_all(format!("{message}\n").as_bytes())
                .await
                .unwrap();
        }

        async fn hear(&mut self) -> Value {
            let mut line = String::new();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.replies.read_line(&mut line),
            )
            .await
            .expect("a reply arrives")
            .unwrap();
            serde_json::from_str(&line).expect("a JSON line")
        }
    }

    fn call(id: u64, tool: &str, input: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
            "name": "approve",
            "arguments": {"tool_name": tool, "input": input, "tool_use_id": "toolu_1"},
        }})
    }

    /// The JSON the tool result carries as text.
    fn decision(reply: &Value) -> Value {
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    }

    #[tokio::test]
    async fn the_handshake_says_what_claude_needs() {
        let mut p = pipes(Scripted::new(true, None));
        p.say(json!({"jsonrpc": "2.0", "id": 0, "method": "server/discover"}))
            .await;
        assert_eq!(
            p.hear().await,
            json!({"jsonrpc": "2.0", "id": 0, "result": {}})
        );
        p.say(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25", "capabilities": {}}}))
            .await;
        let init = p.hear().await;
        assert_eq!(init["id"], 1);
        assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(init["result"]["capabilities"], json!({"tools": {}}));
        assert_eq!(init["result"]["serverInfo"]["name"], "clusia");
        p.say(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        p.say(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
            .await;
        let list = p.hear().await;
        assert_eq!(list["id"], 2, "the notification got no answer");
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "approve");
        assert_eq!(
            tools[0]["inputSchema"]["required"],
            json!(["tool_name", "input"])
        );
        p.say(json!({"jsonrpc": "2.0", "id": 3, "method": "ping"}))
            .await;
        assert_eq!(
            p.hear().await,
            json!({"jsonrpc": "2.0", "id": 3, "result": {}}),
            "an unknown method with an id gets an empty result"
        );
    }

    #[tokio::test]
    async fn initialize_without_a_version_answers_a_default() {
        let mut p = pipes(Scripted::new(true, None));
        p.say(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"}))
            .await;
        assert_eq!(p.hear().await["result"]["protocolVersion"], "2025-06-18");
    }

    #[tokio::test]
    async fn an_allowed_call_returns_the_input_to_run() {
        let scripted = Scripted::new(true, None);
        let mut p = pipes(scripted.clone());
        let input = json!({"command": "cargo test -p clusia-core", "description": "run tests"});
        p.say(call(7, "Bash", input.clone())).await;
        let reply = p.hear().await;
        assert_eq!(reply["id"], 7);
        assert_eq!(
            decision(&reply),
            json!({"behavior": "allow", "updatedInput": input})
        );
        assert_eq!(
            scripted.asked.lock().unwrap().as_slice(),
            [("Bash".to_string(), input)]
        );
    }

    #[tokio::test]
    async fn a_denied_call_carries_the_message() {
        let mut p = pipes(Scripted::new(false, Some("The reviewer said no.")));
        p.say(call(8, "Bash", json!({"command": "rm -rf /"}))).await;
        assert_eq!(
            decision(&p.hear().await),
            json!({"behavior": "deny", "message": "The reviewer said no."})
        );
        let mut plain = pipes(Scripted::new(false, None));
        plain.say(call(9, "Bash", json!({"command": "ls"}))).await;
        assert_eq!(
            decision(&plain.hear().await),
            json!({"behavior": "deny", "message": DENIED})
        );
    }

    #[tokio::test]
    async fn a_malformed_call_is_denied_not_forwarded() {
        let scripted = Scripted::new(true, None);
        let mut p = pipes(scripted.clone());
        p.say(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "approve", "arguments": {"input": {"command": "ls"}}}}))
            .await;
        assert_eq!(decision(&p.hear().await)["behavior"], "deny");
        p.say(json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": {"name": "other", "arguments": {}}}))
            .await;
        assert_eq!(p.hear().await["error"]["code"], -32602);
        assert!(scripted.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn lines_that_are_not_json_are_ignored() {
        let mut p = pipes(Scripted::new(true, None));
        p.send
            .write_all(b"not json at all\n\xff\xfe\n")
            .await
            .unwrap();
        p.say(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
            .await;
        assert_eq!(p.hear().await["id"], 1);
    }

    #[tokio::test]
    async fn a_line_too_long_is_skipped() {
        let mut p = pipes(Scripted::new(true, None));
        // A request that would be answered, were it not too long.
        let pad = "a".repeat(MAX_LINE_BYTES as usize);
        let long =
            json!({"jsonrpc": "2.0", "id": 9, "method": "tools/list", "params": {"pad": pad}});
        let writer = tokio::spawn(async move {
            p.say(long).await;
            p
        });
        let mut p = writer.await.unwrap();
        p.say(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
            .await;
        assert_eq!(p.hear().await["id"], 1, "the long line got no answer");
    }

    #[tokio::test]
    async fn a_call_without_input_is_denied_not_forwarded() {
        let scripted = Scripted::new(true, None);
        let mut p = pipes(scripted.clone());
        p.say(json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call",
            "params": {"name": "approve", "arguments": {"tool_name": "Bash"}}}))
            .await;
        assert_eq!(decision(&p.hear().await)["behavior"], "deny");
        assert!(scripted.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_call_waiting_for_the_reviewer_holds_nothing_else_up() {
        let mut p = pipes(Silent);
        p.say(call(1, "Bash", json!({"command": "make"}))).await;
        p.say(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}))
            .await;
        assert_eq!(p.hear().await["id"], 2);
    }

    #[tokio::test]
    async fn bridge_exits_on_eof() {
        let mut p = pipes(Silent);
        p.say(call(1, "Bash", json!({"command": "make"}))).await;
        p.send.shutdown().await.unwrap();
        let ended = tokio::time::timeout(std::time::Duration::from_secs(5), p.done)
            .await
            .expect("the bridge ends with its input, though a call still waits");
        assert!(ended.unwrap().is_ok());
        let mut rest = Vec::new();
        p.replies.read_to_end(&mut rest).await.unwrap();
        assert!(
            rest.is_empty(),
            "the waiting call was dropped, not answered"
        );
    }

    /// A one-connection-at-a-time daemon: answers every request with `answer` and reports it.
    async fn daemon_that(
        answer: fn(Command) -> Outcome,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        tokio::sync::mpsc::UnboundedReceiver<Command>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (seen, commands) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (r, mut w) = stream.into_split();
                let mut r = MessageReader::new(r);
                let _hello: ClientMessage = r.next().await.unwrap().unwrap();
                let welcome = ServerMessage::Welcome {
                    protocol: clusia_protocol::PROTOCOL_VERSION,
                    daemon: "9.9.9".into(),
                };
                write_message(&mut w, &welcome).await.unwrap();
                if let Ok(Some(ClientMessage::Request { id, cmd })) = r.next().await {
                    let _ = seen.send(cmd.clone());
                    let result = answer(cmd);
                    write_message(&mut w, &ServerMessage::Response { id, result })
                        .await
                        .unwrap();
                }
            }
        });
        (dir, path, commands)
    }

    fn forwarding(socket: PathBuf) -> SocketForward {
        SocketForward {
            socket,
            pr: "acme/widgets#7".parse().unwrap(),
            turn: 3,
        }
    }

    #[tokio::test]
    async fn the_socket_carries_the_ask_and_brings_the_decision_back() {
        let (_dir, socket, mut commands) = daemon_that(|_| {
            Outcome::Ok(Reply::PermissionDecision {
                allow: true,
                message: None,
            })
        })
        .await;
        let input = json!({"command": "cargo test"});
        let verdict = forwarding(socket).forward("Bash", &input).await;
        assert_eq!(
            verdict,
            Verdict {
                allow: true,
                message: None
            }
        );
        assert_eq!(
            commands.recv().await.unwrap(),
            Command::PermissionAsk {
                pr: "acme/widgets#7".parse().unwrap(),
                turn: 3,
                tool: "Bash".into(),
                input,
            }
        );
    }

    #[tokio::test]
    async fn a_refused_ask_is_a_denial_with_the_reason() {
        let (_dir, socket, _commands) = daemon_that(|_| {
            Outcome::Err(ProtocolError::new(
                ErrorCode::InvalidState,
                "turn 3 is not running",
            ))
        })
        .await;
        let verdict = forwarding(socket).forward("Bash", &json!({})).await;
        assert!(!verdict.allow);
        assert!(verdict.message.unwrap().contains("turn 3 is not running"));
    }

    #[tokio::test]
    async fn a_reply_that_is_not_a_decision_is_a_denial() {
        let (_dir, socket, _commands) = daemon_that(|_| Outcome::Ok(Reply::Ack)).await;
        let verdict = forwarding(socket).forward("Bash", &json!({})).await;
        assert!(!verdict.allow);
        let message = verdict.message.unwrap();
        assert!(
            message.contains("the daemon gave an unexpected answer"),
            "{message}"
        );
        assert!(!message.contains("Ack"), "no debug output: {message}");
    }

    #[tokio::test]
    async fn unreachable_daemon_denies() {
        let dir = tempfile::tempdir().unwrap();
        let verdict = forwarding(dir.path().join("none.sock"))
            .forward("Bash", &json!({"command": "ls"}))
            .await;
        assert!(!verdict.allow);
        assert!(verdict.message.unwrap().contains("denied"));
    }

    #[tokio::test]
    async fn a_daemon_that_hangs_up_mid_ask_denies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = stream.into_split();
            let mut r = MessageReader::new(r);
            let _hello: ClientMessage = r.next().await.unwrap().unwrap();
            let welcome = ServerMessage::Welcome {
                protocol: clusia_protocol::PROTOCOL_VERSION,
                daemon: "9.9.9".into(),
            };
            write_message(&mut w, &welcome).await.unwrap();
            let _ask: Option<ClientMessage> = r.next().await.unwrap();
            // Dropping both halves closes the connection without a response.
        });
        let verdict = forwarding(path).forward("Bash", &json!({})).await;
        assert!(!verdict.allow);
    }
}
