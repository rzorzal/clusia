//! A stand-in for the `codex` program, for tests of code that runs it.
//!
//! [`FakeCodex::install`] writes an executable shell script named `codex` into a folder. It
//! answers `--version` and `login status` and, as `codex app-server`, speaks the JSON-RPC lines
//! of Codex 0.162.1 over standard input and output:
//!
//! - `initialize`, `thread/start`, `thread/resume`, `thread/fork`, `turn/start` and
//!   `turn/interrupt` get their results; any other request gets an error.
//! - Thread ids are `thr-1`, `thr-2`, … for `thread/start` and `thread/fork` from one counter
//!   shared by every process of the install; `thread/resume` hands back the id it was given.
//!   Turn ids are `turn-<n>` from a counter shared the same way.
//! - The n-th `turn/start` of the install plays the n-th [`CodexTurn`] of the script (the last
//!   one repeats): notification lines in order, with `__THREAD__` and `__TURN__` replaced.
//! - A turn can ask first: the fake sends the approval request, waits for the client's reply
//!   and records its decision. While it waits, it answers other requests (a fork, an interrupt).
//! - The process ends when its standard input closes.
//!
//! One limit: while a turn waits for the reply to its ask, a second turn that starts in the
//! same process and asks too would take the first one's reply from the input, and the first
//! would wait forever. Script one asking turn at a time per process.
//!
//! Every `app-server` run records its argument list, environment, working folder, process id,
//! every line it read and every decision it received under `calls/<n>/`.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// One thing a turn asks before it goes on.
#[derive(Debug, Clone)]
enum Ask {
    Command(String),
    Files(Vec<String>, Option<String>),
    Other(String),
}

/// How the turn ends.
#[derive(Debug, Clone)]
enum Body {
    Answer(String),
    Fail(String),
    Crash,
    Hang,
    Raw(Vec<String>),
}

/// What the fake plays for one `turn/start`.
#[derive(Debug, Clone)]
pub struct CodexTurn {
    body: Body,
    asks: Vec<Ask>,
    delay_ms: u64,
    ignore_interrupt: bool,
    stderr: String,
}

fn line(method: &str, params: Value) -> String {
    json!({ "method": method, "params": params }).to_string()
}

fn turn_started() -> String {
    line(
        "turn/started",
        json!({
            "threadId": "__THREAD__",
            "turn": { "id": "__TURN__", "items": [], "status": "inProgress", "error": null },
        }),
    )
}

fn turn_completed(status: &str, error: Option<&str>) -> String {
    let error = error.map(|m| json!({ "message": m, "codexErrorInfo": null }));
    line(
        "turn/completed",
        json!({
            "threadId": "__THREAD__",
            "turn": { "id": "__TURN__", "items": [], "status": status, "error": error },
        }),
    )
}

fn item(method: &str, item: Value) -> String {
    line(
        method,
        json!({ "threadId": "__THREAD__", "turnId": "__TURN__", "item": item }),
    )
}

/// A server request; `__ASK__` becomes its id when it is sent.
fn request(method: &str, params: Value) -> String {
    format!(
        r#"{{"id":__ASK__,"method":{},"params":{}}}"#,
        json!(method),
        params
    )
}

impl CodexTurn {
    fn with(body: Body) -> Self {
        Self {
            body,
            asks: Vec::new(),
            delay_ms: 0,
            ignore_interrupt: false,
            stderr: String::new(),
        }
    }

    /// A normal turn: `text` streams in one piece and the turn completes.
    pub fn answer(text: &str) -> Self {
        Self::with(Body::Answer(text.to_string()))
    }

    /// A normal turn whose answer is `prose` followed by `blocks`, each set apart by a blank
    /// line. Build the blocks with [`finding_block`](super::finding_block) and its siblings.
    pub fn answer_blocks(prose: &str, blocks: &[String]) -> Self {
        let mut text = prose.trim_end().to_string();
        for block in blocks {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(block);
        }
        Self::answer(&text)
    }

    /// The turn starts and then fails with `message`.
    pub fn fail(message: &str) -> Self {
        Self::with(Body::Fail(message.to_string()))
    }

    /// The turn starts and then the process exits with status 1, saying nothing more.
    pub fn crash() -> Self {
        Self::with(Body::Crash)
    }

    /// The turn starts and then never ends; a `turn/interrupt` ends it as `interrupted`, unless
    /// [`ignore_interrupt`](Self::ignore_interrupt) says otherwise.
    pub fn hang() -> Self {
        Self::with(Body::Hang)
    }

    /// Prints `lines` as they are (after the placeholder swap), one per line, and nothing else.
    pub fn lines(lines: &[&str]) -> Self {
        Self::with(Body::Raw(lines.iter().map(|l| l.to_string()).collect()))
    }

    /// Before the rest of the turn, asks to run `command`. `accept` completes its item,
    /// `decline` declines it and the turn goes on, `cancel` ends the turn as `interrupted`.
    pub fn ask_command(mut self, command: &str) -> Self {
        self.asks.push(Ask::Command(command.to_string()));
        self
    }

    /// Before the rest of the turn, asks to change `paths`, which are sent as given. The
    /// decisions mean what they mean for [`ask_command`](Self::ask_command).
    pub fn ask_files(mut self, paths: &[&str]) -> Self {
        self.asks.push(Ask::Files(
            paths.iter().map(|p| p.to_string()).collect(),
            None,
        ));
        self
    }

    /// Like [`ask_files`](Self::ask_files), and the request also asks for write access to the
    /// whole folder `grant_root` (`params.grantRoot`).
    pub fn ask_files_with_root(mut self, paths: &[&str], grant_root: &str) -> Self {
        self.asks.push(Ask::Files(
            paths.iter().map(|p| p.to_string()).collect(),
            Some(grant_root.to_string()),
        ));
        self
    }

    /// Before the rest of the turn, sends the server request `method` with the params
    /// `{threadId, turnId}` and waits for the reply. The reply is recorded as `error` when it
    /// carries an error object, else as its decision string, else as `other`.
    pub fn ask_other(mut self, method: &str) -> Self {
        self.asks.push(Ask::Other(method.to_string()));
        self
    }

    /// The `turn/interrupt` of this turn is answered with `{}` and nothing else happens.
    pub fn ignore_interrupt(mut self) -> Self {
        self.ignore_interrupt = true;
        self
    }

    /// Pauses `ms` after every line of the turn. A `turn/interrupt` is answered only after the
    /// scripted lines end, so a test of an interrupted end uses `hang()`, which has none.
    pub fn delay_ms(mut self, ms: u64) -> Self {
        self.delay_ms = ms;
        self
    }

    /// Writes `text` to standard error when the turn starts.
    pub fn stderr(mut self, text: &str) -> Self {
        self.stderr = text.to_string();
        self
    }

    /// The tagged lines the script plays: `L` prints, `A` prints a request and waits for its
    /// reply, `Y`, `N` print after an accept or a decline, `C` ends the turn after a cancel,
    /// `H` waits for an interrupt, `X` ends the process, `I` is the line that ends the turn
    /// when it is interrupted.
    fn script(&self) -> String {
        let mut out = vec![format!("I {}", turn_completed("interrupted", None))];
        if !matches!(self.body, Body::Raw(_)) {
            out.push(format!("L {}", turn_started()));
        }
        for (n, ask) in self.asks.iter().enumerate() {
            match ask {
                Ask::Command(command) => {
                    let id = format!("cmd-{n}");
                    let running = json!({
                        "type": "commandExecution", "id": id, "command": command,
                        "cwd": "/", "status": "inProgress", "commandActions": [],
                    });
                    let finished = |status: &str| {
                        json!({
                            "type": "commandExecution", "id": id, "command": command,
                            "cwd": "/", "status": status, "commandActions": [],
                        })
                    };
                    out.push(format!("L {}", item("item/started", running)));
                    out.push(format!(
                        "A {}",
                        request(
                            "item/commandExecution/requestApproval",
                            json!({
                                "threadId": "__THREAD__", "turnId": "__TURN__",
                                "itemId": id, "startedAtMs": 0, "command": command, "cwd": "/",
                            }),
                        )
                    ));
                    out.push(format!(
                        "Y {}",
                        item("item/completed", finished("completed"))
                    ));
                    out.push(format!(
                        "N {}",
                        item("item/completed", finished("declined"))
                    ));
                    out.push("C -".to_string());
                }
                Ask::Files(paths, root) => {
                    let id = format!("fc-{n}");
                    let changes = |with_diff: bool| -> Vec<Value> {
                        let diff = if with_diff {
                            "@@ -1 +1 @@\n-a\n+b\n"
                        } else {
                            ""
                        };
                        paths
                            .iter()
                            .map(|p| {
                                json!({
                                    "path": p, "kind": { "type": "update", "move_path": null },
                                    "diff": diff,
                                })
                            })
                            .collect()
                    };
                    let file = |status: &str, with_diff: bool| {
                        json!({
                            "type": "fileChange", "id": id, "changes": changes(with_diff),
                            "status": status,
                        })
                    };
                    out.push(format!(
                        "L {}",
                        item("item/started", file("inProgress", true))
                    ));
                    let mut params = json!({
                        "threadId": "__THREAD__", "turnId": "__TURN__",
                        "itemId": id, "startedAtMs": 0,
                    });
                    if let (Some(root), Some(map)) = (root, params.as_object_mut()) {
                        map.insert("grantRoot".into(), json!(root));
                    }
                    out.push(format!(
                        "A {}",
                        request("item/fileChange/requestApproval", params)
                    ));
                    out.push(format!(
                        "Y {}",
                        item("item/completed", file("completed", false))
                    ));
                    out.push(format!(
                        "N {}",
                        item("item/completed", file("declined", false))
                    ));
                    out.push("C -".to_string());
                }
                Ask::Other(method) => {
                    out.push(format!(
                        "A {}",
                        request(
                            method,
                            json!({ "threadId": "__THREAD__", "turnId": "__TURN__" }),
                        )
                    ));
                }
            }
        }
        match &self.body {
            Body::Answer(text) => {
                let message = |text: &str| json!({ "type": "agentMessage", "id": "msg-__TURN__", "text": text });
                out.push(format!("L {}", item("item/started", message(""))));
                out.push(format!(
                    "L {}",
                    line(
                        "item/agentMessage/delta",
                        json!({
                            "threadId": "__THREAD__", "turnId": "__TURN__",
                            "itemId": "msg-__TURN__", "delta": text,
                        }),
                    )
                ));
                out.push(format!("L {}", item("item/completed", message(text))));
                out.push(format!("L {}", turn_completed("completed", None)));
            }
            Body::Fail(message) => {
                out.push(format!("L {}", turn_completed("failed", Some(message))));
            }
            Body::Crash => out.push("X -".to_string()),
            Body::Hang => out.push("H -".to_string()),
            Body::Raw(lines) => out.extend(lines.iter().map(|l| format!("L {l}"))),
        }
        let mut text = out.join("\n");
        text.push('\n');
        text
    }
}

/// The turns the fake plays, one per `turn/start`, and what its other commands say.
#[derive(Debug, Clone)]
pub struct CodexScript {
    turns: Vec<CodexTurn>,
    version: String,
    account: String,
    signed_out: bool,
}

impl CodexScript {
    fn with(turns: Vec<CodexTurn>) -> Self {
        Self {
            turns,
            version: "0.162.1".into(),
            account: "Logged in using ChatGPT".into(),
            signed_out: false,
        }
    }

    /// Every `turn/start` plays `turn`.
    pub fn one(turn: CodexTurn) -> Self {
        Self::with(vec![turn])
    }

    /// The n-th `turn/start` plays `turns[n - 1]`; turns past the end repeat the last one.
    pub fn turns(turns: Vec<CodexTurn>) -> Self {
        assert!(!turns.is_empty(), "a script needs at least one turn");
        Self::with(turns)
    }

    /// `--version` prints `codex-cli {version}`.
    pub fn version(mut self, version: &str) -> Self {
        self.version = version.to_string();
        self
    }

    /// `login status` exits 1 and says `Not logged in` on standard error.
    pub fn signed_out(mut self) -> Self {
        self.signed_out = true;
        self
    }

    /// `login status` exits 0 and says `line` on standard error.
    pub fn account(mut self, line: &str) -> Self {
        self.account = line.to_string();
        self
    }
}

/// One recorded `codex app-server` run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexCall {
    /// The arguments after the program name, such as `["app-server", "--search"]`.
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub pid: u32,
    /// Every line the client sent, as written (JSON with `id` and `method`).
    pub lines: Vec<String>,
    /// What each answer to a request of the fake said, in order: `accept`, `decline`,
    /// `cancel`, `error` or `other`.
    pub decisions: Vec<String>,
}

impl CodexCall {
    /// The `method` of every request and notification the client sent, in order.
    pub fn methods(&self) -> Vec<String> {
        self.lines
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|v| v["method"].as_str().map(str::to_string))
            .collect()
    }

    /// The lines the client sent for `method`, parsed.
    pub fn requests(&self, method: &str) -> Vec<Value> {
        self.lines
            .iter()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["method"] == method)
            .collect()
    }
}

pub struct FakeCodex;

impl FakeCodex {
    /// Writes the fake into `dir` (which must exist) and returns the path of its executable.
    pub fn install(dir: &Path, script: CodexScript) -> PathBuf {
        fs::create_dir_all(dir.join("calls")).expect("create calls dir");
        fs::write(dir.join("version"), &script.version).expect("write the version");
        fs::write(dir.join("account"), format!("{}\n", script.account)).expect("write the account");
        let signed_out = dir.join("signed-out");
        if script.signed_out {
            fs::write(&signed_out, "").expect("write the signed-out mark");
        } else {
            let _ = fs::remove_file(&signed_out);
        }
        for (i, turn) in script.turns.iter().enumerate() {
            let k = i + 1;
            fs::write(dir.join(format!("turn-{k}.script")), turn.script())
                .expect("write the turn script");
            fs::write(dir.join(format!("turn-{k}.stderr")), &turn.stderr)
                .expect("write the turn stderr");
            let conf = format!(
                "{}.{:03}\n{}\n",
                turn.delay_ms / 1000,
                turn.delay_ms % 1000,
                if turn.ignore_interrupt { "1" } else { "" },
            );
            fs::write(dir.join(format!("turn-{k}.conf")), conf).expect("write the turn conf");
        }
        let program = dir.join("codex");
        let text = SCRIPT.replace("__TOTAL__", &script.turns.len().to_string());
        fs::write(&program, text).expect("write the fake");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).expect("chmod the fake");
        // The first run of a new executable can be slow while macOS checks it; take that
        // delay now, so a test with a short time limit does not pay it.
        let _ = std::process::Command::new(&program)
            .arg("--version")
            .output();
        program
    }

    /// The `app-server` runs recorded in `dir` so far, oldest first.
    pub fn calls(dir: &Path) -> Vec<CodexCall> {
        let mut numbers: Vec<u32> = fs::read_dir(dir.join("calls"))
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        numbers.sort_unstable();
        numbers
            .into_iter()
            .filter_map(|n| read_call(&dir.join("calls").join(n.to_string())))
            .collect()
    }

    /// Waits until at least `count` runs are recorded; panics after `limit`.
    pub fn wait_for_calls(dir: &Path, count: usize, limit: Duration) -> Vec<CodexCall> {
        let give_up = Instant::now() + limit;
        loop {
            let calls = Self::calls(dir);
            if calls.len() >= count {
                return calls;
            }
            assert!(
                Instant::now() < give_up,
                "expected {count} runs of the fake, saw {}",
                calls.len()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Every decision the client answered with, across all runs, oldest first.
    pub fn decisions(dir: &Path) -> Vec<String> {
        Self::calls(dir)
            .into_iter()
            .flat_map(|call| call.decisions)
            .collect()
    }

    /// Waits until at least `count` decisions are recorded; panics after `limit`.
    pub fn wait_for_decisions(dir: &Path, count: usize, limit: Duration) -> Vec<String> {
        let give_up = Instant::now() + limit;
        loop {
            let decisions = Self::decisions(dir);
            if decisions.len() >= count {
                return decisions;
            }
            assert!(
                Instant::now() < give_up,
                "expected {count} decisions, saw {}",
                decisions.len()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Whether process `pid` is still alive. A child that ended but was not yet waited for (a
    /// zombie) still counts as running, so wait for it before asserting that it is gone.
    pub fn is_running(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

/// `None` while the run is still writing its record.
fn read_call(dir: &Path) -> Option<CodexCall> {
    let pid = fs::read_to_string(dir.join("pid"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let argv = fs::read(dir.join("argv")).ok()?;
    let argv = argv
        .split(|b| *b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect::<Vec<_>>();
    // Every argument ends with a NUL, so the split leaves one empty piece at the end.
    let argv = argv[..argv.len().saturating_sub(1)].to_vec();
    let env = fs::read_to_string(dir.join("env"))
        .ok()?
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let cwd = PathBuf::from(fs::read_to_string(dir.join("cwd")).ok()?.trim_end());
    let lines_of = |name: &str| -> Vec<String> {
        fs::read_to_string(dir.join(name))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    };
    Some(CodexCall {
        argv,
        env,
        cwd,
        pid,
        lines: lines_of("lines"),
        decisions: lines_of("decisions"),
    })
}

const SCRIPT: &str = r#"#!/bin/bash
dir=$(cd "$(dirname "$0")" && pwd)
case "$1" in
  --version)
    printf 'codex-cli %s\n' "$(cat "$dir/version")"
    exit 0 ;;
  login)
    if [ -f "$dir/signed-out" ]; then
      echo "Not logged in" >&2
      exit 1
    fi
    cat "$dir/account" >&2
    exit 0 ;;
  app-server) ;;
  *)
    echo "the fake codex does not know: $*" >&2
    exit 2 ;;
esac
shopt -u patsub_replacement 2>/dev/null

# Claims the next number of a counter that every run of the fake shares.
next() {
  local c=1
  mkdir -p "$dir/$1"
  while ! mkdir "$dir/$1/$c" 2>/dev/null; do c=$((c + 1)); done
  echo "$c"
}

n=$(next calls)
rec="$dir/calls/$n"
: > "$rec/argv"
[ $# -gt 0 ] && printf '%s\0' "$@" > "$rec/argv"
env > "$rec/env"
pwd > "$rec/cwd"
: > "$rec/lines"
: > "$rec/decisions"
echo $$ > "$rec/pid"

re_id='"id":([0-9]+)'
re_method='"method":"([^"]+)"'
re_thread='"threadId":"([^"]+)"'
re_turn='"turnId":"([^"]+)"'
re_decision='"decision":"([a-zA-Z]+)"'
interrupted=" "
srv=0

emit() { printf '%s\n' "$1"; }

# Reads the next line of the client into LINE and records it; the process ends with its input.
read_client() {
  IFS= read -r LINE || exit 0
  printf '%s\n' "$LINE" >> "$rec/lines"
}

reply() { printf '{"id":%s,"result":%s}\n' "$1" "$2"; }

# Whether the turn $1 was interrupted and $2 (the turn's ignore-interrupt mark) allows it to end.
stopped() {
  [ -z "$2" ] || return 1
  case "$interrupted" in *" $1 "*) return 0 ;; esac
  return 1
}

handle_line() {
  local text=$1 id="" method="" thread="" turn="" k tid idx
  [[ $text =~ $re_id ]] && id=${BASH_REMATCH[1]}
  [[ $text =~ $re_method ]] && method=${BASH_REMATCH[1]}
  [[ $text =~ $re_thread ]] && thread=${BASH_REMATCH[1]}
  case "$method" in
    initialize) reply "$id" '{"userAgent":"codex-cli/fake","codexHome":"/nonexistent"}' ;;
    initialized) ;;
    thread/start|thread/fork)
      k=$(next thr)
      tid="thr-$k"
      reply "$id" "{\"thread\":{\"id\":\"$tid\"}}"
      emit "{\"method\":\"thread/started\",\"params\":{\"thread\":{\"id\":\"$tid\"}}}" ;;
    thread/resume)
      reply "$id" "{\"thread\":{\"id\":\"$thread\"}}"
      emit "{\"method\":\"thread/started\",\"params\":{\"thread\":{\"id\":\"$thread\"}}}" ;;
    turn/start)
      k=$(next turns)
      tid="turn-$k"
      idx=$k
      [ "$idx" -gt __TOTAL__ ] && idx=__TOTAL__
      reply "$id" "{\"turn\":{\"id\":\"$tid\",\"items\":[],\"status\":\"inProgress\",\"error\":null}}"
      play "$thread" "$tid" "$idx" ;;
    turn/interrupt)
      [[ $text =~ $re_turn ]] && turn=${BASH_REMATCH[1]}
      reply "$id" '{}'
      interrupted="$interrupted$turn " ;;
    *)
      if [ -n "$id" ] && [ -n "$method" ]; then
        printf '{"id":%s,"error":{"code":-32601,"message":"Method not found"}}\n' "$id"
      fi ;;
  esac
}

# Plays script $3 for the turn $2 of thread $1.
play() {
  local thread=$1 turn=$2 idx=$3
  local delay ignore all entry tag rest intr="" decision="" aid
  delay=$(sed -n 1p "$dir/turn-$idx.conf")
  ignore=$(sed -n 2p "$dir/turn-$idx.conf")
  [ -s "$dir/turn-$idx.stderr" ] && cat "$dir/turn-$idx.stderr" >&2
  all=$(cat "$dir/turn-$idx.script")
  while [ -n "$all" ]; do
    entry=${all%%$'\n'*}
    if [[ $all == *$'\n'* ]]; then all=${all#*$'\n'}; else all=""; fi
    tag=${entry:0:1}
    rest=${entry:2}
    rest=${rest//__THREAD__/$thread}
    rest=${rest//__TURN__/$turn}
    case "$tag" in
      I) intr=$rest ;;
      L) emit "$rest" ;;
      A)
        srv=$((srv + 1))
        aid=$((9000 + srv))
        emit "${rest//__ASK__/$aid}"
        decision=""
        while [ -z "$decision" ]; do
          read_client
          if [[ $LINE =~ $re_id ]] && [ "${BASH_REMATCH[1]}" = "$aid" ] && [[ $LINE != *'"method"'* ]]; then
            if [[ $LINE == *'"error"'* ]]; then
              decision=error
            elif [[ $LINE =~ $re_decision ]]; then
              decision=${BASH_REMATCH[1]}
            else
              decision=other
            fi
            echo "$decision" >> "$rec/decisions"
          else
            handle_line "$LINE"
          fi
          if stopped "$turn" "$ignore"; then emit "$intr"; return 0; fi
        done ;;
      Y) [ "$decision" = accept ] && emit "$rest" ;;
      N) [ "$decision" = decline ] && emit "$rest" ;;
      C) if [ "$decision" = cancel ]; then emit "$intr"; return 0; fi ;;
      H)
        while true; do
          read_client
          handle_line "$LINE"
          if stopped "$turn" "$ignore"; then emit "$intr"; return 0; fi
        done ;;
      X) exit 1 ;;
    esac
    [ "$delay" != "0.000" ] && sleep "$delay"
  done
  return 0
}

while true; do
  read_client
  handle_line "$LINE"
done
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::{
        self, Decision, Incoming, ItemKind, Note, ServerAsk, ThreadParams, TurnStatus,
    };
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::mpsc::{self, Receiver};

    /// A running `codex app-server` of the fake: lines in, lines out. Killed when dropped.
    struct Proc {
        child: Child,
        stdin: Option<ChildStdin>,
        lines: Receiver<String>,
        next_id: u64,
    }

    impl Proc {
        fn start(program: &Path, extra: &[&str], cwd: &Path) -> Self {
            let mut child = Command::new(program)
                .arg("app-server")
                .args(extra)
                .current_dir(cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("start the fake");
            let stdin = child.stdin.take();
            let stdout = child.stdout.take().expect("stdout is piped");
            let (tx, lines) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
            });
            Self {
                child,
                stdin,
                lines,
                next_id: 0,
            }
        }

        fn send(&mut self, message: &Value) {
            let stdin = self.stdin.as_mut().expect("stdin is open");
            stdin
                .write_all(codex::to_line(message).as_bytes())
                .expect("write to the fake");
            stdin.flush().unwrap();
        }

        fn id(&mut self) -> u64 {
            let id = self.next_id;
            self.next_id += 1;
            id
        }

        /// The next line, or `None` when nothing comes within `limit` or the output ended.
        fn line_within(&mut self, limit: Duration) -> Option<String> {
            self.lines.recv_timeout(limit).ok()
        }

        fn line(&mut self) -> String {
            self.line_within(Duration::from_secs(10))
                .expect("a line from the fake")
        }

        /// Reads until the response to `id`; notifications on the way are kept in `seen`.
        fn response(&mut self, id: u64, seen: &mut Vec<Incoming>) -> Value {
            loop {
                let line = self.line();
                match codex::read_line(&line) {
                    Incoming::Response { id: got, result } if got == id => {
                        return result.unwrap_or_else(|e| panic!("an error response: {e}"));
                    }
                    other => seen.push(other),
                }
            }
        }

        /// Handshake and a new thread; returns its id.
        fn open_thread(&mut self) -> String {
            let init = codex::initialize(self.id());
            self.send(&init);
            let mut seen = Vec::new();
            self.response(0, &mut seen);
            self.send(&codex::initialized());
            let id = self.id();
            self.send(&codex::thread_start(id, &params()));
            let result = self.response(id, &mut seen);
            codex::thread_id_of(&result).expect("a thread id")
        }

        fn start_turn(&mut self, thread: &str, prompt: &str) -> String {
            let id = self.id();
            self.send(&codex::turn_start(id, thread, prompt));
            let mut seen = Vec::new();
            let result = self.response(id, &mut seen);
            codex::turn_id_of(&result).expect("a turn id")
        }

        /// Reads notifications until `turn/completed`; returns every note and request seen.
        fn until_completed(&mut self) -> Vec<Incoming> {
            let mut seen = Vec::new();
            loop {
                let incoming = codex::read_line(&self.line());
                let done = matches!(
                    incoming,
                    Incoming::Notification {
                        note: Note::TurnCompleted { .. },
                        ..
                    }
                );
                seen.push(incoming);
                if done {
                    return seen;
                }
            }
        }
    }

    impl Drop for Proc {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn params() -> ThreadParams {
        ThreadParams {
            cwd: PathBuf::from("/work/acme-widgets"),
            developer_instructions: "Be brief.".into(),
        }
    }

    fn lab(script: CodexScript) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeCodex::install(dir.path(), script);
        (dir, program)
    }

    fn status_of(seen: &[Incoming]) -> Option<(TurnStatus, Option<String>)> {
        seen.iter().find_map(|i| match i {
            Incoming::Notification {
                note: Note::TurnCompleted { status, error },
                ..
            } => Some((*status, error.clone())),
            _ => None,
        })
    }

    #[test]
    fn the_version_and_the_login_are_scriptable() {
        let (_d, program) = lab(CodexScript::one(CodexTurn::answer("x")));
        let out = Command::new(&program).arg("--version").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "codex-cli 0.162.1\n");
        let out = Command::new(&program)
            .args(["login", "status"])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            "Logged in using ChatGPT\n"
        );

        let (_d, program) = lab(CodexScript::one(CodexTurn::answer("x"))
            .version("0.150.0")
            .account("Logged in using an API key - sk-abc123"));
        let out = Command::new(&program).arg("--version").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "codex-cli 0.150.0\n");
        let out = Command::new(&program)
            .args(["login", "status"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            "Logged in using an API key - sk-abc123\n"
        );

        let (_d, program) = lab(CodexScript::one(CodexTurn::answer("x")).signed_out());
        let out = Command::new(&program)
            .args(["login", "status"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(String::from_utf8_lossy(&out.stderr), "Not logged in\n");
    }

    #[test]
    fn version_and_login_runs_are_not_recorded() {
        let (dir, program) = lab(CodexScript::one(CodexTurn::answer("x")));
        let _ = Command::new(&program).arg("--version").output().unwrap();
        let _ = Command::new(&program).args(["login", "status"]).output();
        assert!(FakeCodex::calls(dir.path()).is_empty());
    }

    #[test]
    fn a_run_records_argv_env_cwd_pid_and_every_line() {
        let (dir, program) = lab(CodexScript::one(CodexTurn::answer("x")));
        let work = tempfile::tempdir().unwrap();
        let mut proc = Proc::start(&program, &["--search"], work.path());
        let thread = proc.open_thread();
        proc.start_turn(&thread, "hello");
        proc.until_completed();
        let calls = FakeCodex::wait_for_calls(dir.path(), 1, Duration::from_secs(5));
        let call = &calls[0];
        assert_eq!(call.argv, ["app-server", "--search"]);
        assert_eq!(
            call.cwd.canonicalize().unwrap(),
            work.path().canonicalize().unwrap()
        );
        assert!(FakeCodex::is_running(call.pid));
        assert!(call.env.contains_key("PATH"));
        let calls = FakeCodex::calls(dir.path());
        assert_eq!(
            calls[0].methods(),
            ["initialize", "initialized", "thread/start", "turn/start"]
        );
        let turn = &calls[0].requests("turn/start")[0];
        assert_eq!(turn["params"]["input"][0]["text"], "hello");
        let start = &calls[0].requests("thread/start")[0];
        assert_eq!(start["params"]["sandbox"], "read-only");
        assert_eq!(start["params"]["config"]["features"]["hooks"], false);
        let pid = calls[0].pid;
        drop(proc);
        for _ in 0..100 {
            if !FakeCodex::is_running(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the fake outlived its handle");
    }

    #[test]
    fn an_answer_turn_plays_its_notifications_with_the_ids_filled_in() {
        let (_d, program) = lab(CodexScript::one(CodexTurn::answer("Looks fine.")));
        let mut proc = Proc::start(&program, &[], Path::new("/"));
        let thread = proc.open_thread();
        assert_eq!(thread, "thr-1");
        let turn = proc.start_turn(&thread, "q");
        assert_eq!(turn, "turn-1");
        let seen = proc.until_completed();
        let notes: Vec<&Note> = seen
            .iter()
            .filter_map(|i| match i {
                Incoming::Notification { thread_id, note } => {
                    assert_eq!(thread_id.as_deref(), Some("thr-1"));
                    Some(note)
                }
                _ => None,
            })
            .collect();
        assert!(matches!(notes[0], Note::TurnStarted { turn_id } if turn_id == "turn-1"));
        assert!(
            notes
                .iter()
                .any(|n| matches!(n, Note::TextDelta(t) if t == "Looks fine."))
        );
        assert!(notes.iter().any(|n| matches!(
            n,
            Note::ItemCompleted(info) if matches!(&info.kind, ItemKind::AgentMessage { text } if text == "Looks fine.")
        )));
        assert_eq!(status_of(&seen), Some((TurnStatus::Completed, None)));
    }

    #[test]
    fn threads_and_turns_are_numbered_across_every_process_of_the_install() {
        let (_d, program) = lab(CodexScript::turns(vec![
            CodexTurn::answer("one"),
            CodexTurn::answer("two"),
        ]));
        let mut first = Proc::start(&program, &[], Path::new("/"));
        let t1 = first.open_thread();
        assert_eq!(t1, "thr-1");
        assert_eq!(first.start_turn(&t1, "a"), "turn-1");
        let seen = first.until_completed();
        assert!(seen.iter().any(
            |i| matches!(i, Incoming::Notification { note: Note::TextDelta(t), .. } if t == "one")
        ));
        // A fork takes the next number of the same counter, in the same process.
        let id = first.id();
        first.send(&codex::thread_fork(id, &t1, &params()));
        let mut seen = Vec::new();
        let result = first.response(id, &mut seen);
        assert_eq!(codex::thread_id_of(&result).as_deref(), Some("thr-2"));

        let mut second = Proc::start(&program, &[], Path::new("/"));
        let t3 = second.open_thread();
        assert_eq!(t3, "thr-3", "a new process goes on counting");
        assert_eq!(second.start_turn(&t3, "b"), "turn-2");
        let seen = second.until_completed();
        assert!(seen.iter().any(
            |i| matches!(i, Incoming::Notification { note: Note::TextDelta(t), .. } if t == "two")
        ));
        assert_eq!(second.start_turn(&t3, "c"), "turn-3");
        let seen = second.until_completed();
        assert!(
            seen.iter()
                .any(|i| matches!(i, Incoming::Notification { note: Note::TextDelta(t), .. } if t == "two")),
            "the last turn repeats"
        );
        // A resume answers with the id it was given.
        let id = second.id();
        second.send(&codex::thread_resume(id, "thr-1", &params()));
        let mut seen = Vec::new();
        let result = second.response(id, &mut seen);
        assert_eq!(codex::thread_id_of(&result).as_deref(), Some("thr-1"));
    }

    #[test]
    fn a_command_ask_waits_for_the_decision_and_records_it() {
        for (decision, end, items) in [
            (Decision::Accept, TurnStatus::Completed, "completed"),
            (Decision::Decline, TurnStatus::Completed, "declined"),
        ] {
            let (dir, program) = lab(CodexScript::one(
                CodexTurn::answer("done").ask_command("cargo test"),
            ));
            let mut proc = Proc::start(&program, &[], Path::new("/"));
            let thread = proc.open_thread();
            let id = proc.id();
            proc.send(&codex::turn_start(id, &thread, "go"));
            let mut seen = Vec::new();
            proc.response(id, &mut seen);
            // Read until the request comes; nothing after it may arrive before the reply.
            let request = loop {
                match codex::read_line(&proc.line()) {
                    Incoming::ServerRequest { id, ask, thread_id } => break (id, ask, thread_id),
                    other => seen.push(other),
                }
            };
            assert_eq!(request.2.as_deref(), Some(thread.as_str()));
            let ServerAsk::Command {
                command, turn_id, ..
            } = &request.1
            else {
                panic!("a command ask: {:?}", request.1);
            };
            assert_eq!(command.as_deref(), Some("cargo test"));
            assert_eq!(turn_id, "turn-1");
            assert!(
                proc.line_within(Duration::from_millis(300)).is_none(),
                "the turn waits for the answer"
            );
            proc.send(&codex::approval_reply(&request.0, decision));
            let rest = proc.until_completed();
            assert_eq!(status_of(&rest), Some((end, None)));
            let completed = rest.iter().find_map(|i| match i {
                Incoming::Notification {
                    note: Note::ItemCompleted(info),
                    ..
                } if matches!(info.kind, ItemKind::Command { .. }) => Some(info.id.clone()),
                _ => None,
            });
            assert_eq!(completed.as_deref(), Some("cmd-0"), "{items}");
            let wanted = match decision {
                Decision::Accept => "accept",
                _ => "decline",
            };
            let got = FakeCodex::wait_for_decisions(dir.path(), 1, Duration::from_secs(5));
            assert_eq!(got, [wanted]);
            assert!(rest.iter().any(
                |i| matches!(i, Incoming::Notification { note: Note::TextDelta(t), .. } if t == "done")
            ));
        }
    }

    #[test]
    fn a_cancel_ends_the_turn_as_interrupted_without_its_answer() {
        let (dir, program) = lab(CodexScript::one(
            CodexTurn::answer("never said").ask_command("rm -rf build"),
        ));
        let mut proc = Proc::start(&program, &[], Path::new("/"));
        let thread = proc.open_thread();
        proc.start_turn(&thread, "go");
        let request = loop {
            if let Incoming::ServerRequest { id, .. } = codex::read_line(&proc.line()) {
                break id;
            }
        };
        proc.send(&codex::approval_reply(&request, Decision::Cancel));
        let rest = proc.until_completed();
        assert_eq!(status_of(&rest), Some((TurnStatus::Interrupted, None)));
        assert!(!rest.iter().any(|i| matches!(
            i,
            Incoming::Notification {
                note: Note::TextDelta(_),
                ..
            }
        )));
        assert_eq!(
            FakeCodex::wait_for_decisions(dir.path(), 1, Duration::from_secs(5)),
            ["cancel"]
        );
    }

    #[test]
    fn a_file_ask_names_its_paths_and_several_asks_come_in_order() {
        let (dir, program) = lab(CodexScript::one(
            CodexTurn::answer("ok")
                .ask_files(&["/work/acme-widgets/src/a.rs", "/work/acme-widgets/b.md"])
                .ask_command("make"),
        ));
        let mut proc = Proc::start(&program, &[], Path::new("/"));
        let thread = proc.open_thread();
        proc.start_turn(&thread, "go");
        let mut asked = Vec::new();
        let mut paths = Vec::new();
        loop {
            match codex::read_line(&proc.line()) {
                Incoming::ServerRequest { id, ask, .. } => {
                    asked.push(ask);
                    let decision = if asked.len() == 1 {
                        Decision::Accept
                    } else {
                        Decision::Decline
                    };
                    proc.send(&codex::approval_reply(&id, decision));
                }
                Incoming::Notification {
                    note: Note::ItemStarted(info),
                    ..
                } => {
                    if let ItemKind::FileChange { paths: p } = info.kind {
                        paths = p;
                    }
                }
                Incoming::Notification {
                    note: Note::TurnCompleted { status, .. },
                    ..
                } => {
                    assert_eq!(status, TurnStatus::Completed);
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(
            paths,
            ["/work/acme-widgets/src/a.rs", "/work/acme-widgets/b.md"]
        );
        assert!(matches!(&asked[0], ServerAsk::FileChange { item_id, .. } if item_id == "fc-0"));
        assert!(
            matches!(&asked[1], ServerAsk::Command { command, .. } if command.as_deref() == Some("make"))
        );
        assert_eq!(
            FakeCodex::wait_for_decisions(dir.path(), 2, Duration::from_secs(5)),
            ["accept", "decline"]
        );
    }

    #[test]
    fn a_file_ask_can_ask_for_a_whole_folder() {
        let (_d, program) = lab(CodexScript::one(
            CodexTurn::answer("ok").ask_files_with_root(&["/w/a.rs"], "/w"),
        ));
        let mut proc = Proc::start(&program, &[], Path::new("/"));
        let thread = proc.open_thread();
        proc.start_turn(&thread, "go");
        let ask = loop {
            if let Incoming::ServerRequest { ask, .. } = codex::read_line(&proc.line()) {
                break ask;
            }
        };
        assert!(matches!(
            ask,
            ServerAsk::FileChange { grant_root: Some(root), .. } if root == "/w"
        ));
    }

    #[test]
    fn another_ask_records_an_error_reply_as_error() {
        let (dir, program) = lab(CodexScript::one(
            CodexTurn::answer("ok")
                .ask_other("mcpServer/elicitation/request")
                .ask_other("item/permissions/requestApproval"),
        ));
        let mut proc = Proc::start(&program, &[], Path::new("/"));
        let thread = proc.open_thread();
        proc.start_turn(&thread, "go");
        let mut methods = Vec::new();
        loop {
            match codex::read_line(&proc.line()) {
                Incoming::ServerRequest { id, ask, .. } => {
                    let ServerAsk::Other(method) = ask else {
                        panic!("another ask");
                    };
                    methods.push(method);
                    if methods.len() == 1 {
                        proc.send(
                            &json!({ "id": id, "error": { "code": -32000, "message": "no" } }),
                        );
                    } else {
                        proc.send(&codex::approval_reply(&id, Decision::Decline));
                    }
                }
                Incoming::Notification {
                    note: Note::TurnCompleted { .. },
                    ..
                } => break,
                _ => {}
            }
        }
        assert_eq!(
            methods,
            [
                "mcpServer/elicitation/request",
                "item/permissions/requestApproval"
            ]
        );
        assert_eq!(
            FakeCodex::wait_for_decisions(dir.path(), 2, Duration::from_secs(5)),
            ["error", "decline"]
        );
    }

    #[test]
    fn a_hung_turn_ends_as_interrupted_when_asked_to_stop() {
        let (_d, program) = lab(CodexScript::one(CodexTurn::hang()));
        let mut proc = Proc::start(&program, &[], Path::new("/"));
        let thread = proc.open_thread();
        let turn = proc.start_turn(&thread, "go");
        assert!(matches!(
            codex::read_line(&proc.line()),
            Incoming::Notification {
                note: Note::TurnStarted { .. },
                ..
            }
        ));
        assert!(
            proc.line_within(Duration::from_millis(300)).is_none(),
            "nothing comes while it hangs"
        );
        let id = proc.id();
        proc.send(&codex::turn_interrupt(id, &thread, &turn));
        let seen = proc.until_completed();
        assert!(
            seen.iter()
                .any(|i| matches!(i, Incoming::Response { id: got, .. } if *got == id))
        );
        assert_eq!(status_of(&seen), Some((TurnStatus::Interrupted, None)));
    }

    #[test]
    fn a_turn_that_ignores_the_interrupt_stays_until_it_is_killed() {
        let (dir, program) = lab(CodexScript::one(CodexTurn::hang().ignore_interrupt()));
        let mut proc = Proc::start(&program, &[], Path::new("/"));
        let thread = proc.open_thread();
        let turn = proc.start_turn(&thread, "go");
        let _started = proc.line();
        let id = proc.id();
        proc.send(&codex::turn_interrupt(id, &thread, &turn));
        let answer = codex::read_line(&proc.line());
        assert_eq!(
            answer,
            Incoming::Response {
                id,
                result: Ok(json!({}))
            }
        );
        assert!(
            proc.line_within(Duration::from_millis(400)).is_none(),
            "it never ends"
        );
        let pid = FakeCodex::calls(dir.path())[0].pid;
        assert!(FakeCodex::is_running(pid));
        let _ = proc.child.kill();
        let _ = proc.child.wait();
    }

    #[test]
    fn another_request_is_answered_while_a_turn_hangs() {
        let (_d, program) = lab(CodexScript::one(CodexTurn::hang()));
        let mut proc = Proc::start(&program, &[], Path::new("/"));
        let thread = proc.open_thread();
        let _turn = proc.start_turn(&thread, "go");
        let _started = proc.line();
        let id = proc.id();
        proc.send(&codex::thread_fork(id, &thread, &params()));
        let mut seen = Vec::new();
        let result = proc.response(id, &mut seen);
        assert_eq!(codex::thread_id_of(&result).as_deref(), Some("thr-2"));
    }

    #[test]
    fn a_crash_ends_the_process_after_the_turn_starts() {
        let (dir, program) = lab(CodexScript::one(CodexTurn::crash()));
        let mut proc = Proc::start(&program, &[], Path::new("/"));
        let thread = proc.open_thread();
        proc.start_turn(&thread, "go");
        assert!(matches!(
            codex::read_line(&proc.line()),
            Incoming::Notification {
                note: Note::TurnStarted { .. },
                ..
            }
        ));
        let status = proc.child.wait().unwrap();
        assert_eq!(status.code(), Some(1));
        assert!(!FakeCodex::is_running(FakeCodex::calls(dir.path())[0].pid));
    }

    #[test]
    fn a_failed_turn_says_why() {
        let (_d, program) = lab(CodexScript::one(CodexTurn::fail(
            "You've hit your usage limit",
        )));
        let mut proc = Proc::start(&program, &[], Path::new("/"));
        let thread = proc.open_thread();
        proc.start_turn(&thread, "go");
        let seen = proc.until_completed();
        assert_eq!(
            status_of(&seen),
            Some((
                TurnStatus::Failed,
                Some("You've hit your usage limit".into())
            ))
        );
    }

    #[test]
    fn a_turns_stderr_is_written_when_it_starts() {
        let (_d, program) = lab(CodexScript::one(
            CodexTurn::crash().stderr("the model fell over"),
        ));
        let mut child = Command::new(&program)
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        for message in [
            codex::initialize(0),
            codex::initialized(),
            codex::thread_start(1, &params()),
            codex::turn_start(2, "thr-1", "go"),
        ] {
            stdin
                .write_all(codex::to_line(&message).as_bytes())
                .unwrap();
        }
        drop(stdin);
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(String::from_utf8_lossy(&out.stderr), "the model fell over");
    }

    #[test]
    fn raw_lines_are_played_as_they_are() {
        let (_d, program) = lab(CodexScript::one(CodexTurn::lines(&[
            "not json",
            r#"{"method":"warning","params":{"threadId":"__THREAD__"}}"#,
            r#"{"method":"turn/completed","params":{"threadId":"__THREAD__","turn":{"id":"__TURN__","items":[],"status":"completed","error":null}}}"#,
        ])));
        let mut proc = Proc::start(&program, &[], Path::new("/"));
        let thread = proc.open_thread();
        proc.start_turn(&thread, "go");
        assert_eq!(proc.line(), "not json");
        assert_eq!(codex::read_line(&proc.line()), Incoming::Ignored);
        let seen = proc.until_completed();
        assert_eq!(status_of(&seen), Some((TurnStatus::Completed, None)));
    }

    #[test]
    fn closing_the_input_ends_the_process_and_an_unknown_request_gets_an_error() {
        let (dir, program) = lab(CodexScript::one(CodexTurn::answer("x")));
        let mut proc = Proc::start(&program, &[], Path::new("/"));
        let _thread = proc.open_thread();
        let id = proc.id();
        proc.send(&json!({ "id": id, "method": "no/such/method", "params": {} }));
        let reply = loop {
            let incoming = codex::read_line(&proc.line());
            if matches!(incoming, Incoming::Response { .. }) {
                break incoming;
            }
        };
        assert_eq!(
            reply,
            Incoming::Response {
                id,
                result: Err("Method not found".into())
            }
        );
        let pid = FakeCodex::calls(dir.path())[0].pid;
        proc.stdin = None;
        let began = Instant::now();
        let status = proc.child.wait().unwrap();
        assert!(status.success());
        assert!(began.elapsed() < Duration::from_secs(2));
        assert!(!FakeCodex::is_running(pid));
    }

    #[test]
    fn answer_blocks_put_the_blocks_after_the_prose() {
        let turn = CodexTurn::answer_blocks("Prose.\n", &["```a\n```".into(), "```b\n```".into()]);
        let Body::Answer(text) = &turn.body else {
            panic!("an answer");
        };
        assert_eq!(text, "Prose.\n\n```a\n```\n\n```b\n```");
    }
}
