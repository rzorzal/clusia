//! A stand-in for the `claude` program, for tests of code that runs it.
//!
//! [`FakeClaude::install`] writes an executable shell script into a folder. Every run of the
//! script records its arguments, environment, working folder and process id under
//! `calls/<n>/`, then replays the [`Turn`] of that call (the n-th call plays the n-th turn, and
//! the last turn repeats). `--version` is answered without being recorded.
//!
//! A turn can ask permissions: when the run has `--mcp-config`, the fake starts the MCP server
//! named there, calls its `approve` tool once per ask and records every answer under
//! `calls/<n>/answers`. Asks that were not allowed are listed in the result line's
//! `permission_denials`, as the real program does.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Stands in for the session id in replayed lines; the script swaps in the id it was given
/// through `--session-id` or `--resume`.
const SESSION_PLACEHOLDER: &str = "__SESSION__";

/// What one run of the fake prints and how it ends.
#[derive(Debug, Clone)]
pub struct Turn {
    lines: Vec<String>,
    delay_ms: u64,
    exit: i32,
    stderr: String,
    hang: bool,
    ignore_term: bool,
    asks: Vec<(String, serde_json::Value)>,
}

impl Turn {
    /// Prints `lines` as they are, one per line.
    pub fn lines(lines: &[&str]) -> Self {
        Self {
            lines: lines.iter().map(|l| l.to_string()).collect(),
            delay_ms: 0,
            exit: 0,
            stderr: String::new(),
            hang: false,
            ignore_term: false,
            asks: Vec::new(),
        }
    }

    /// Replays `tests/fixtures/<name>.jsonl` of this crate.
    pub fn fixture(name: &str) -> Self {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(format!("{name}.jsonl"));
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read fixture {}: {e}", path.display()));
        Self {
            lines: text.lines().map(str::to_string).collect(),
            ..Self::lines(&[])
        }
    }

    /// A normal turn: the session starts, `text` streams in one piece and the turn succeeds.
    pub fn answer(text: &str) -> Self {
        let init = serde_json::json!({
            "type": "system", "subtype": "init", "session_id": SESSION_PLACEHOLDER,
        });
        let delta = serde_json::json!({
            "type": "stream_event",
            "event": {
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": text},
            },
            "session_id": SESSION_PLACEHOLDER,
        });
        let result = serde_json::json!({
            "type": "result", "subtype": "success", "is_error": false,
            "duration_ms": 5, "num_turns": 1, "result": text,
            "session_id": SESSION_PLACEHOLDER, "permission_denials": [],
        });
        Self {
            lines: [init, delta, result]
                .iter()
                .map(|l| l.to_string())
                .collect(),
            ..Self::lines(&[])
        }
    }

    /// Starts the session and then never ends: the process sleeps until it is killed.
    pub fn hanging() -> Self {
        let init = serde_json::json!({
            "type": "system", "subtype": "init", "session_id": SESSION_PLACEHOLDER,
        });
        Self {
            lines: vec![init.to_string()],
            hang: true,
            ..Self::lines(&[])
        }
    }

    /// Waits this long after each printed line.
    pub fn delay_ms(mut self, ms: u64) -> Self {
        self.delay_ms = ms;
        self
    }

    /// Ends with this exit code once every line is printed.
    pub fn exit(mut self, code: i32) -> Self {
        self.exit = code;
        self
    }

    /// Writes `text` to standard error before the first line.
    pub fn stderr(mut self, text: &str) -> Self {
        self.stderr = text.to_string();
        self
    }

    /// Sleeps until killed after the last line instead of ending.
    pub fn then_hang(mut self) -> Self {
        self.hang = true;
        self
    }

    /// Ignores SIGTERM, so only SIGKILL stops it.
    pub fn ignore_term(mut self) -> Self {
        self.ignore_term = true;
        self
    }

    /// Before the first printed line, asks permission for `tool` with `input` through the MCP
    /// server of `--mcp-config` and waits for the answer. Asks run in the order they were added.
    /// An allowed ask changes nothing; a denied one adds itself to the result's
    /// `permission_denials`. Without `--mcp-config` the asks are skipped.
    pub fn ask_permission(mut self, tool: &str, input: serde_json::Value) -> Self {
        self.asks.push((tool.to_string(), input));
        self
    }
}

/// The turns the fake plays, one per call.
#[derive(Debug, Clone)]
pub struct Script {
    turns: Vec<Turn>,
}

impl Script {
    /// Every call plays `turn`.
    pub fn one(turn: Turn) -> Self {
        Self { turns: vec![turn] }
    }

    /// Call n plays `turns[n - 1]`; calls past the end repeat the last turn.
    pub fn turns(turns: Vec<Turn>) -> Self {
        assert!(!turns.is_empty(), "a script needs at least one turn");
        Self { turns }
    }
}

/// One recorded run of the fake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub pid: u32,
}

/// What the permission bridge answered to one ask of a [`Turn`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionAnswer {
    pub tool: String,
    pub allow: bool,
    /// The refusal the agent reads; `None` when allowed. A bridge that never answered counts
    /// as a denial with the message `no answer from the bridge`.
    pub message: Option<String>,
}

pub struct FakeClaude;

impl FakeClaude {
    /// Writes the fake into `dir` (which must exist) and returns the path of its executable.
    pub fn install(dir: &Path, script: Script) -> PathBuf {
        fs::create_dir_all(dir.join("calls")).expect("create calls dir");
        for (i, turn) in script.turns.iter().enumerate() {
            let k = i + 1;
            let body: String = turn.lines.iter().map(|l| format!("{l}\n")).collect();
            fs::write(dir.join(format!("turn-{k}.jsonl")), body).expect("write turn lines");
            fs::write(dir.join(format!("turn-{k}.stderr")), &turn.stderr).expect("write stderr");
            let conf = format!(
                "delay={}.{:03}\ncode={}\nhang={}\nignore_term={}\nasks={}\n",
                turn.delay_ms / 1000,
                turn.delay_ms % 1000,
                turn.exit,
                if turn.hang { "1" } else { "" },
                if turn.ignore_term { "1" } else { "" },
                turn.asks.len(),
            );
            for (n, (tool, input)) in turn.asks.iter().enumerate() {
                let n = n + 1;
                let call = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": n + 1,
                    "method": "tools/call",
                    "params": {
                        "name": "approve",
                        "arguments": {
                            "tool_name": tool,
                            "input": input,
                            "tool_use_id": format!("toolu_fake_{k}_{n}"),
                        },
                    },
                });
                fs::write(dir.join(format!("turn-{k}.ask-{n}.call")), call.to_string())
                    .expect("write ask call");
                fs::write(dir.join(format!("turn-{k}.ask-{n}.tool")), tool)
                    .expect("write ask tool");
                let denial = serde_json::json!({
                    "tool_name": tool,
                    "tool_use_id": format!("toolu_fake_{k}_{n}"),
                    "tool_input": input,
                });
                fs::write(
                    dir.join(format!("turn-{k}.ask-{n}.denial")),
                    denial.to_string(),
                )
                .expect("write ask denial");
            }
            fs::write(dir.join(format!("turn-{k}.conf")), conf).expect("write turn conf");
        }
        let program = dir.join("claude");
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

    /// The runs recorded in `dir` so far, oldest first.
    pub fn calls(dir: &Path) -> Vec<Call> {
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

    /// What the bridge answered, in run order and then ask order.
    pub fn permission_answers(dir: &Path) -> Vec<PermissionAnswer> {
        let mut numbers: Vec<u32> = fs::read_dir(dir.join("calls"))
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        numbers.sort_unstable();
        let mut answers = Vec::new();
        for n in numbers {
            let text = fs::read_to_string(dir.join("calls").join(n.to_string()).join("answers"))
                .unwrap_or_default();
            answers.extend(text.lines().filter_map(read_answer));
        }
        answers
    }

    /// Waits until at least `count` permission answers are recorded; panics after `limit`.
    pub fn wait_for_answers(dir: &Path, count: usize, limit: Duration) -> Vec<PermissionAnswer> {
        let give_up = Instant::now() + limit;
        loop {
            let answers = Self::permission_answers(dir);
            if answers.len() >= count {
                return answers;
            }
            assert!(
                Instant::now() < give_up,
                "expected {count} permission answers, saw {}",
                answers.len()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits until at least `count` runs are recorded; panics after `limit`.
    pub fn wait_for_calls(dir: &Path, count: usize, limit: Duration) -> Vec<Call> {
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
fn read_call(dir: &Path) -> Option<Call> {
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
    Some(Call {
        argv,
        env,
        cwd,
        pid,
    })
}

/// One `answers` line: the tool, a tab, and the raw reply of the `tools/call` (or `null`).
fn read_answer(line: &str) -> Option<PermissionAnswer> {
    let (tool, raw) = line.split_once('\t')?;
    let verdict = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|reply| {
            let text = reply["result"]["content"][0]["text"].as_str()?.to_string();
            serde_json::from_str::<serde_json::Value>(&text).ok()
        });
    Some(match verdict {
        Some(v) if v["behavior"] == "allow" => PermissionAnswer {
            tool: tool.to_string(),
            allow: true,
            message: None,
        },
        Some(v) => PermissionAnswer {
            tool: tool.to_string(),
            allow: false,
            message: v["message"].as_str().map(str::to_string),
        },
        None => PermissionAnswer {
            tool: tool.to_string(),
            allow: false,
            message: Some("no answer from the bridge".into()),
        },
    })
}

const SCRIPT: &str = r#"#!/bin/bash
dir=$(cd "$(dirname "$0")" && pwd)
if [ "$1" = "--version" ]; then
  echo "2.1.294 (Claude Code)"
  exit 0
fi
n=1
while ! mkdir "$dir/calls/$n" 2>/dev/null; do n=$((n + 1)); done
: > "$dir/calls/$n/argv"
[ $# -gt 0 ] && printf '%s\0' "$@" > "$dir/calls/$n/argv"
env > "$dir/calls/$n/env"
pwd > "$dir/calls/$n/cwd"
echo $$ > "$dir/calls/$n/pid"
k=$n
[ "$k" -gt __TOTAL__ ] && k=__TOTAL__
sid=""
prev=""
cfg=""
for a in "$@"; do
  if [ "$prev" = "--session-id" ] || [ "$prev" = "--resume" ]; then sid=$a; fi
  if [ "$prev" = "--mcp-config" ]; then cfg=$a; fi
  prev=$a
done
. "$dir/turn-$k.conf"
[ -n "$ignore_term" ] && trap '' TERM
[ -s "$dir/turn-$k.stderr" ] && cat "$dir/turn-$k.stderr" >&2
shopt -u patsub_replacement 2>/dev/null
denials=""
denial_pat='"permission_denials":[]'
if [ "${asks:-0}" -gt 0 ] && [ -n "$cfg" ]; then
  bcmd=${cfg#*'"command":"'}
  bcmd=${bcmd%%'"'*}
  bargs=${cfg#*'"args":["'}
  bargs=${bargs%%'"]'*}
  bargs=${bargs//'","'/$'\t'}
  IFS=$'\t' read -r -a bargv <<< "$bargs"
  trap '' PIPE
  mkfifo "$dir/calls/$n/to-bridge" "$dir/calls/$n/from-bridge"
  "$bcmd" "${bargv[@]}" < "$dir/calls/$n/to-bridge" > "$dir/calls/$n/from-bridge" 2> "$dir/calls/$n/bridge.stderr" &
  bridge=$!
  exec 3> "$dir/calls/$n/to-bridge"
  exec 4< "$dir/calls/$n/from-bridge"
  ask() {
    printf '%s\n' "$1" >&3 2>/dev/null
    reply=""
    read -r -t 30 reply <&4 || reply=""
  }
  ask '{"jsonrpc":"2.0","id":"server-discover-probe-1","method":"server/discover","params":{}}'
  ask '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"roots":{"listChanged":true}},"clientInfo":{"name":"claude-code","version":"2.1.295"}}}'
  printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}' >&3 2>/dev/null
  ask '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
  i=1
  while [ "$i" -le "$asks" ]; do
    ask "$(cat "$dir/turn-$k.ask-$i.call")"
    printf '%s\t%s\n' "$(cat "$dir/turn-$k.ask-$i.tool")" "${reply:-null}" >> "$dir/calls/$n/answers"
    case "$reply" in
      *'\"behavior\":\"allow\"'*) ;;
      *) denials="${denials:+$denials,}$(cat "$dir/turn-$k.ask-$i.denial")" ;;
    esac
    i=$((i + 1))
  done
  exec 3>&-
  exec 4<&-
  wait "$bridge" 2>/dev/null
fi
denial_repl="\"permission_denials\":[$denials]"
while IFS= read -r line; do
  line=${line//__SESSION__/$sid}
  line=${line//"$denial_pat"/$denial_repl}
  printf '%s\n' "$line"
  [ "$delay" != "0.000" ] && sleep "$delay"
done < "$dir/turn-$k.jsonl"
[ -n "$hang" ] && exec sleep 3600
exit "$code"
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};

    /// Kills the child when dropped, so a failing assertion never leaves a fake running.
    struct Running(Child);

    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn run(program: &Path, args: &[&str], cwd: &Path) -> std::process::Output {
        Command::new(program)
            .args(args)
            .current_dir(cwd)
            .env("FAKE_MARK", "present")
            .output()
            .expect("run the fake")
    }

    #[test]
    fn records_argv_env_cwd_and_pid() {
        let dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let program = FakeClaude::install(dir.path(), Script::one(Turn::answer("hi there")));
        let out = run(
            &program,
            &["-p", "two words\nand a line", "--session-id", "abc-123"],
            work.path(),
        );
        assert!(out.status.success());
        let calls = FakeClaude::calls(dir.path());
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].argv,
            ["-p", "two words\nand a line", "--session-id", "abc-123"]
        );
        assert_eq!(
            calls[0].env.get("FAKE_MARK").map(String::as_str),
            Some("present")
        );
        assert_eq!(
            calls[0].cwd.canonicalize().unwrap(),
            work.path().canonicalize().unwrap()
        );
        assert!(calls[0].pid > 1);
    }

    #[test]
    fn answer_echoes_the_session_id_it_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeClaude::install(dir.path(), Script::one(Turn::answer("hi")));
        let out = run(&program, &["--resume", "sess-9"], dir.path());
        let text = String::from_utf8(out.stdout).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(
            lines
                .iter()
                .all(|l| l.contains("\"session_id\":\"sess-9\""))
        );
        assert!(lines[2].contains("\"result\":\"hi\""));
    }

    #[test]
    fn each_call_plays_its_own_turn_and_the_last_one_repeats() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeClaude::install(
            dir.path(),
            Script::turns(vec![Turn::lines(&["first"]), Turn::lines(&["second"])]),
        );
        let said = |program: &Path| {
            String::from_utf8(run(program, &["-p", "x"], Path::new("/")).stdout).unwrap()
        };
        assert_eq!(said(&program), "first\n");
        assert_eq!(said(&program), "second\n");
        assert_eq!(said(&program), "second\n");
        assert_eq!(FakeClaude::calls(dir.path()).len(), 3);
    }

    #[test]
    fn exit_code_and_stderr_come_through() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeClaude::install(
            dir.path(),
            Script::one(Turn::lines(&["partial"]).exit(3).stderr("boom\n")),
        );
        let out = run(&program, &["-p", "x"], dir.path());
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "partial\n");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "boom\n");
    }

    #[test]
    fn a_hanging_turn_runs_until_it_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeClaude::install(dir.path(), Script::one(Turn::hanging()));
        let mut child = Running(
            Command::new(&program)
                .args(["-p", "x", "--session-id", "s1"])
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let call = FakeClaude::wait_for_calls(dir.path(), 1, Duration::from_secs(5)).remove(0);
        assert!(FakeClaude::is_running(call.pid));
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        assert!(!FakeClaude::is_running(call.pid));
    }

    #[test]
    fn a_hanging_turn_dies_on_sigterm() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeClaude::install(dir.path(), Script::one(Turn::hanging()));
        let mut child = Running(
            Command::new(&program)
                .args(["-p", "x"])
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let call = FakeClaude::wait_for_calls(dir.path(), 1, Duration::from_secs(5)).remove(0);
        // The recorded pid is the one that hangs (the script `exec`s its sleep).
        assert_eq!(call.pid, child.0.id());
        let sent = Command::new("kill")
            .args(["-TERM", &call.pid.to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
        let status = child.0.wait().unwrap();
        assert!(!status.success());
        assert!(!FakeClaude::is_running(call.pid));
    }

    #[test]
    fn a_silent_turn_prints_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeClaude::install(dir.path(), Script::one(Turn::lines(&[]).exit(1)));
        let out = run(&program, &["-p", "x"], dir.path());
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    }

    #[test]
    fn a_run_without_arguments_records_none() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeClaude::install(dir.path(), Script::one(Turn::lines(&[])));
        run(&program, &[], dir.path());
        assert_eq!(FakeClaude::calls(dir.path())[0].argv, Vec::<String>::new());
    }

    #[test]
    fn fixtures_replay_as_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeClaude::install(dir.path(), Script::one(Turn::fixture("text_turn")));
        let out = run(&program, &["--session-id", "s2"], dir.path());
        let recorded = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/text_turn.jsonl"),
        )
        .unwrap()
        .replace(SESSION_PLACEHOLDER, "s2");
        assert_eq!(String::from_utf8(out.stdout).unwrap(), recorded);
    }

    #[test]
    fn the_version_probe_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeClaude::install(dir.path(), Script::one(Turn::answer("x")));
        let out = run(&program, &["--version"], dir.path());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "2.1.294 (Claude Code)\n"
        );
        assert!(FakeClaude::calls(dir.path()).is_empty());
    }

    #[test]
    fn parallel_runs_each_get_their_own_record() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeClaude::install(dir.path(), Script::one(Turn::answer("x")));
        let children: Vec<_> = (0..4)
            .map(|_| {
                Command::new(&program)
                    .args(["-p", "x"])
                    .stdout(Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for mut child in children {
            child.wait().unwrap();
        }
        let calls = FakeClaude::calls(dir.path());
        assert_eq!(calls.len(), 4);
        let mut pids: Vec<u32> = calls.iter().map(|c| c.pid).collect();
        pids.sort_unstable();
        pids.dedup();
        assert_eq!(pids.len(), 4);
    }

    /// A stand-in for the bridge: answers every request with an empty result, except
    /// `tools/call`, which it allows when the request mentions `cargo test` and denies with
    /// "The reviewer said no." otherwise. It ends when its input does.
    fn stub_bridge(dir: &Path) -> PathBuf {
        let script = dir.join("bridge");
        fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in *'"id":'*) ;; *) continue ;; esac
  id=${line#*'"id":'}
  id=${id%%,*}
  case "$line" in
    *'"method":"tools/call"'*)
      case "$line" in
        *'cargo test'*) text='{\"behavior\":\"allow\",\"updatedInput\":{}}' ;;
        *) text='{\"behavior\":\"deny\",\"message\":\"The reviewer said no.\"}' ;;
      esac
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$id" "$text" ;;
    *) printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
  esac
done
"#,
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn mcp_config(bridge: &Path) -> String {
        serde_json::json!({
            "mcpServers": {"clusia": {"command": bridge, "args": ["permission-bridge", "--turn", "1"]}}
        })
        .to_string()
    }

    #[test]
    fn asks_go_through_the_bridge_in_order_and_denials_reach_the_result() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = stub_bridge(dir.path());
        let program = FakeClaude::install(
            dir.path(),
            Script::one(
                Turn::answer("done")
                    .ask_permission("Bash", serde_json::json!({"command": "cargo test -p x"}))
                    .ask_permission(
                        "Bash",
                        serde_json::json!({"command": "rm -rf target & more"}),
                    )
                    .ask_permission("Edit", serde_json::json!({"file_path": "src/lib.rs"})),
            ),
        );
        let out = run(
            &program,
            &[
                "-p",
                "x",
                "--session-id",
                "s1",
                "--mcp-config",
                &mcp_config(&bridge),
            ],
            dir.path(),
        );
        assert!(out.status.success());
        assert_eq!(
            FakeClaude::permission_answers(dir.path()),
            [
                PermissionAnswer {
                    tool: "Bash".into(),
                    allow: true,
                    message: None
                },
                PermissionAnswer {
                    tool: "Bash".into(),
                    allow: false,
                    message: Some("The reviewer said no.".into())
                },
                PermissionAnswer {
                    tool: "Edit".into(),
                    allow: false,
                    message: Some("The reviewer said no.".into())
                },
            ]
        );
        let text = String::from_utf8(out.stdout).unwrap();
        let result: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(
            result["permission_denials"],
            serde_json::json!([
                {
                    "tool_name": "Bash",
                    "tool_use_id": "toolu_fake_1_2",
                    "tool_input": {"command": "rm -rf target & more"}
                },
                {
                    "tool_name": "Edit",
                    "tool_use_id": "toolu_fake_1_3",
                    "tool_input": {"file_path": "src/lib.rs"}
                },
            ])
        );
        assert_eq!(result["session_id"], "s1");
    }

    #[test]
    fn an_allowed_ask_leaves_the_result_alone_and_the_bridge_ends_with_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = stub_bridge(dir.path());
        let program = FakeClaude::install(
            dir.path(),
            Script::one(
                Turn::answer("done")
                    .ask_permission("Bash", serde_json::json!({"command": "cargo test"})),
            ),
        );
        let out = run(
            &program,
            &["-p", "x", "--mcp-config", &mcp_config(&bridge)],
            dir.path(),
        );
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(
            text.lines()
                .last()
                .unwrap()
                .contains(r#""permission_denials":[]"#)
        );
        assert_eq!(FakeClaude::permission_answers(dir.path()).len(), 1);
    }

    #[test]
    fn a_bridge_that_does_not_exist_is_recorded_as_no_answer() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeClaude::install(
            dir.path(),
            Script::one(
                Turn::answer("done")
                    .ask_permission("Bash", serde_json::json!({"command": "cargo test"})),
            ),
        );
        let config = mcp_config(&dir.path().join("missing-bridge"));
        let out = run(&program, &["-p", "x", "--mcp-config", &config], dir.path());
        assert!(out.status.success());
        let answers = FakeClaude::permission_answers(dir.path());
        assert_eq!(
            answers,
            [PermissionAnswer {
                tool: "Bash".into(),
                allow: false,
                message: Some("no answer from the bridge".into())
            }]
        );
    }

    #[test]
    fn asks_are_skipped_without_an_mcp_config() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeClaude::install(
            dir.path(),
            Script::one(
                Turn::answer("done").ask_permission("Bash", serde_json::json!({"command": "ls"})),
            ),
        );
        let out = run(&program, &["-p", "x"], dir.path());
        assert!(out.status.success());
        assert!(FakeClaude::permission_answers(dir.path()).is_empty());
        assert!(
            String::from_utf8(out.stdout)
                .unwrap()
                .contains(r#""permission_denials":[]"#)
        );
    }
}
