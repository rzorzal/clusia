//! A stand-in for a user's custom command: a bash script that speaks Clúsia's JSON lines, so
//! tests run the whole chain, process included, without any real program.
//!
//! `FakeCustom::install` writes the script into a folder and `calls/<n>/` records every run:
//! `argv` (one NUL-ended argument each), `env`, `cwd`, `pid`, `stdin` (every line Clúsia wrote,
//! the first line included) and, for a script that stalls, `child`, the pid of the process it
//! left in its group. `decisions` lists the answers to the permission requests the script sent,
//! `<id> <decision>` per line, across every run.
//!
//! The first line on standard input says what the run is. A probe line plays the probe of the
//! script; anything else plays the next turn of it (the last turn repeats once they run out).

use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::custom::MAX_LINE;

const SCRIPT: &str = r#"#!/bin/bash
dir="$(cd "$(dirname "$0")" && pwd)"
[ -n "$FAKE_CUSTOM_PRIME" ] && exit 0
n=1
while ! mkdir "$dir/calls/$n" 2>/dev/null; do n=$((n+1)); done
c="$dir/calls/$n"
: > "$c/argv"
for a in "$@"; do printf '%s\0' "$a" >> "$c/argv"; done
env -0 > "$c/env"
pwd > "$c/cwd"
IFS= read -r first
printf '%s\n' "$first" >> "$c/stdin"
echo $$ > "$c/pid"
case "$first" in
  *'"type":"probe"'*) . "$dir/probe.sh"; exit 0 ;;
esac
t=1
while ! mkdir "$dir/turns/$t" 2>/dev/null; do t=$((t+1)); done
total=__TOTAL__
[ "$t" -gt "$total" ] && t=$total
. "$dir/turn-$t.sh"
"#;

/// The bash text of `s` as one single-quoted word.
fn quoted(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn print(line: &str) -> String {
    format!("printf '%s\\n' {}\n", quoted(line))
}

/// What one turn of the fake does, step by step.
#[derive(Debug, Clone, Default)]
pub struct CustomTurnScript {
    code: String,
}

impl CustomTurnScript {
    pub fn new() -> Self {
        Self::default()
    }

    fn json(mut self, value: Value) -> Self {
        self.code.push_str(&print(&value.to_string()));
        self
    }

    fn ask(mut self, line: &str, wait_secs: u32) -> Self {
        let _ = write!(
            self.code,
            r#"{print}if IFS= read -r -t {wait_secs} reply; then
  printf '%s\n' "$reply" >> "$c/stdin"
  rid="${{reply#*\"id\":\"}}"; rid="${{rid%%\"*}}"
  dec="${{reply#*\"decision\":\"}}"; dec="${{dec%%\"*}}"
  printf '%s %s\n' "$rid" "$dec" >> "$dir/decisions"
fi
"#,
            print = print(line),
        );
        self
    }

    /// `{"type":"session","id":…}`
    pub fn session(self, id: &str) -> Self {
        self.json(json!({"type": "session", "id": id}))
    }

    /// `{"type":"text","text":…}`
    pub fn text(self, text: &str) -> Self {
        self.json(json!({"type": "text", "text": text}))
    }

    /// `{"type":"tool","summary":…}`
    pub fn tool(self, summary: &str) -> Self {
        self.json(json!({"type": "tool", "summary": summary}))
    }

    /// A `command` request, then waits up to 30 s for the reply and records it.
    pub fn ask_command(self, id: &str, command: &str) -> Self {
        let request = json!({"type": "permission", "id": id, "kind": "command",
                             "detail": command, "reason": "the fake wants to run it"});
        self.ask(&request.to_string(), 30)
    }

    /// An `edit` request for `paths`, then waits up to 30 s for the reply and records it.
    pub fn ask_edit(self, id: &str, paths: &[&str]) -> Self {
        let request = json!({"type": "permission", "id": id, "kind": "edit", "paths": paths,
                             "reason": "the fake wants to change them"});
        self.ask(&request.to_string(), 30)
    }

    /// An `other` request, then waits up to 30 s for the reply and records it.
    pub fn ask_other(self, id: &str, detail: &str) -> Self {
        let request = json!({"type": "permission", "id": id, "kind": "other",
                             "detail": detail, "reason": "the fake wants to do it"});
        self.ask(&request.to_string(), 30)
    }

    /// Any request line, as written, then waits up to 2 s for a reply and records it if one
    /// comes. For a duplicate id, an unknown kind or a request that is not well formed.
    pub fn ask_raw(self, line: &str) -> Self {
        self.ask(line, 2)
    }

    /// `{"type":"final","text":…}`
    pub fn final_(self, text: &str) -> Self {
        self.json(json!({"type": "final", "text": text}))
    }

    /// `{"type":"error","kind":…,"message":…}`; `kind` is `auth`, `limit`, `session`, `other`
    /// or anything else the fake should say.
    pub fn error(self, kind: &str, message: &str) -> Self {
        self.json(json!({"type": "error", "kind": kind, "message": message}))
    }

    /// One line of standard output, verbatim.
    pub fn raw(mut self, line: &str) -> Self {
        self.code.push_str(&print(line));
        self
    }

    /// One line of standard error.
    pub fn stderr(mut self, text: &str) -> Self {
        let _ = writeln!(self.code, "printf '%s\\n' {} >&2", quoted(text));
        self
    }

    /// Waits before the next step.
    pub fn delay_ms(mut self, ms: u64) -> Self {
        let _ = writeln!(self.code, "sleep {}.{:03}", ms / 1000, ms % 1000);
        self
    }

    /// Exits with `code`, with no `final`.
    pub fn crash(mut self, code: i32) -> Self {
        let _ = writeln!(self.code, "exit {code}");
        self
    }

    /// Ignores SIGTERM from here on, so the steps after it still run when Clúsia asks the
    /// program to stop. Put it first.
    pub fn ignore_term(mut self) -> Self {
        self.code.push_str("trap '' TERM\n");
        self
    }

    /// Never ends, and leaves a `sleep` in its group; both stop on SIGTERM.
    pub fn stall(mut self) -> Self {
        self.code
            .push_str("sleep 600 &\necho $! > \"$c/child\"\nwait\n");
        self
    }

    /// Never ends, ignores SIGTERM and leaves a `sleep` in its group that ignores it too; only
    /// SIGKILL stops them.
    pub fn hang(mut self) -> Self {
        self.code
            .push_str("trap '' TERM\nsleep 600 &\necho $! > \"$c/child\"\nwait\nsleep 600\n");
        self
    }

    /// Two lines that are not JSON.
    pub fn garbage(mut self) -> Self {
        self.code.push_str(&print("this is not json at all"));
        self.code.push_str(&print("{\"type\": \"text\", broken"));
        self
    }

    /// One line one byte longer than `MAX_LINE`.
    pub fn oversized_line(mut self) -> Self {
        let _ = writeln!(
            self.code,
            "head -c {} /dev/zero | tr '\\0' x; echo",
            MAX_LINE + 1
        );
        self
    }
}

/// What the fake does: its turns, in the order the runs ask, and its probe.
#[derive(Debug, Clone)]
pub struct CustomScript {
    turns: Vec<CustomTurnScript>,
    probe: String,
}

impl Default for CustomScript {
    fn default() -> Self {
        Self::new()
    }
}

impl CustomScript {
    /// No turns yet; a probe that answers `Fake 1.0`, protocol 1.
    pub fn new() -> Self {
        Self {
            turns: Vec::new(),
            probe: print(r#"{"type":"probe","name":"Fake","version":"1.0","protocol":1}"#),
        }
    }

    /// Adds a turn: the n-th run that is not a probe plays the n-th turn.
    pub fn turn(mut self, turn: CustomTurnScript) -> Self {
        self.turns.push(turn);
        self
    }

    pub fn turns(mut self, turns: Vec<CustomTurnScript>) -> Self {
        self.turns.extend(turns);
        self
    }

    /// The probe answers with this name, version and protocol.
    pub fn probe(mut self, name: &str, version: &str, protocol: u64) -> Self {
        self.probe = print(
            &json!({"type": "probe", "name": name, "version": version, "protocol": protocol})
                .to_string(),
        );
        self
    }

    /// The probe answers with this line, as written.
    pub fn probe_raw(mut self, line: &str) -> Self {
        self.probe = print(line);
        self
    }

    /// The probe reads its line and never answers; it stops on SIGTERM.
    pub fn silent_probe(mut self) -> Self {
        self.probe = "exec sleep 600\n".into();
        self
    }
}

/// One recorded run of the fake: a probe or a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCall {
    /// The run's number in the folder, from 1.
    pub index: usize,
    pub pid: u32,
    /// The pid of the `sleep` a stalled or hung run left in its group.
    pub child: Option<u32>,
    /// The arguments after the program name.
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: String,
    /// Every line Clúsia wrote, the first one included.
    pub stdin: Vec<String>,
}

impl CustomCall {
    /// Whether the first line was a probe.
    pub fn is_probe(&self) -> bool {
        self.first_line()
            .is_some_and(|line| line["type"] == "probe")
    }

    /// The first line Clúsia wrote, parsed.
    pub fn first_line(&self) -> Option<Value> {
        serde_json::from_str(self.stdin.first()?).ok()
    }

    /// The first line, when it started a turn.
    pub fn turn_line(&self) -> Option<Value> {
        self.first_line().filter(|line| line["type"] == "turn")
    }

    /// The value of environment variable `key` the run saw.
    pub fn env_value(&self, key: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// The lines after the first, parsed: the answers to the requests.
    pub fn replies(&self) -> Vec<Value> {
        self.stdin
            .iter()
            .skip(1)
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

pub struct FakeCustom;

impl FakeCustom {
    /// Writes the fake into `dir` (which must exist) and returns the path of its executable.
    pub fn install(dir: &Path, script: CustomScript) -> PathBuf {
        for sub in ["calls", "turns"] {
            fs::create_dir_all(dir.join(sub)).expect("create the fake's folders");
        }
        let _ = fs::remove_file(dir.join("decisions"));
        let turns = if script.turns.is_empty() {
            vec![CustomTurnScript::new().final_("ok")]
        } else {
            script.turns
        };
        for (i, turn) in turns.iter().enumerate() {
            fs::write(dir.join(format!("turn-{}.sh", i + 1)), &turn.code)
                .expect("write the turn script");
        }
        fs::write(dir.join("probe.sh"), &script.probe).expect("write the probe script");
        let program = dir.join("custom");
        let text = SCRIPT.replace("__TOTAL__", &turns.len().to_string());
        fs::write(&program, text).expect("write the fake");
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).expect("chmod the fake");
        // The first run of a new executable can be slow while macOS checks it; take that
        // delay now, so a test with a short time limit does not pay it.
        let _ = std::process::Command::new(&program)
            .env("FAKE_CUSTOM_PRIME", "1")
            .output();
        program
    }

    /// The runs recorded in `dir` so far, oldest first, probes and turns alike.
    pub fn calls(dir: &Path) -> Vec<CustomCall> {
        let mut numbers: Vec<usize> = fs::read_dir(dir.join("calls"))
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        numbers.sort_unstable();
        numbers
            .into_iter()
            .filter_map(|n| read_call(&dir.join("calls").join(n.to_string()), n))
            .collect()
    }

    /// Waits until at least `count` runs are recorded; panics after `limit`.
    pub fn wait_for_calls(dir: &Path, count: usize, limit: Duration) -> Vec<CustomCall> {
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

    /// The `(id, decision)` of every reply the fake got to its requests, oldest first.
    pub fn decisions(dir: &Path) -> Vec<(String, String)> {
        fs::read_to_string(dir.join("decisions"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.split_once(' '))
            .map(|(id, decision)| (id.to_string(), decision.to_string()))
            .collect()
    }

    /// Waits until at least `count` replies are recorded; panics after `limit`.
    pub fn wait_for_decisions(dir: &Path, count: usize, limit: Duration) -> Vec<(String, String)> {
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

fn read_call(dir: &Path, index: usize) -> Option<CustomCall> {
    let pid = fs::read_to_string(dir.join("pid"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let argv = fs::read(dir.join("argv")).ok()?;
    let mut argv: Vec<String> = argv
        .split(|b| *b == 0)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    // Every argument ends with a NUL, so the split leaves one empty piece at the end.
    argv.pop();
    let env = fs::read(dir.join("env"))
        .ok()?
        .split(|b| *b == 0)
        .map(|entry| String::from_utf8_lossy(entry).into_owned())
        .filter_map(|entry| {
            entry
                .split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    let cwd = fs::read_to_string(dir.join("cwd"))
        .ok()?
        .trim_end()
        .to_string();
    let stdin = fs::read_to_string(dir.join("stdin"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let child = fs::read_to_string(dir.join("child"))
        .ok()
        .and_then(|text| text.trim().parse().ok());
    Some(CustomCall {
        index,
        pid,
        child,
        argv,
        env,
        cwd,
        stdin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custom::{AskKind, Output, PROTOCOL, probe_line, read_line, reply_line, turn_line};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, ChildStdin, Command, Stdio};

    const WAIT: Duration = Duration::from_secs(10);

    struct Run {
        child: Child,
        stdin: Option<ChildStdin>,
        out: BufReader<std::process::ChildStdout>,
    }

    impl Run {
        fn start(program: &Path, args: &[&str], cwd: &Path) -> Self {
            let mut child = Command::new(program)
                .args(args)
                .current_dir(cwd)
                .env("KEEP_ME", "yes")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0)
                .spawn()
                .expect("start the fake");
            let stdin = child.stdin.take();
            let out = BufReader::new(child.stdout.take().expect("stdout is piped"));
            Self { child, stdin, out }
        }

        fn send(&mut self, line: &str) {
            let stdin = self.stdin.as_mut().expect("stdin is open");
            stdin.write_all(line.as_bytes()).expect("write a line");
            stdin.flush().expect("flush");
        }

        fn close_stdin(&mut self) {
            self.stdin = None;
        }

        fn line(&mut self) -> Option<Output> {
            let mut text = String::new();
            match self.out.read_line(&mut text) {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(read_line(text.trim_end_matches('\n'))),
            }
        }

        fn rest(&mut self) -> Vec<Output> {
            std::iter::from_fn(|| self.line()).collect()
        }

        fn stderr(&mut self) -> String {
            let mut text = String::new();
            std::io::Read::read_to_string(self.child.stderr.as_mut().unwrap(), &mut text).ok();
            text
        }

        fn signal(&self, signal: &str, whole_group: bool) {
            let pid = self.child.id();
            let target = if whole_group {
                format!("-{pid}")
            } else {
                pid.to_string()
            };
            let _ = Command::new("kill")
                .args([&format!("-{signal}"), "--", &target])
                .status();
        }
    }

    impl Drop for Run {
        fn drop(&mut self) {
            self.signal("KILL", true);
            let _ = self.child.wait();
        }
    }

    fn turn(prompt: &str) -> String {
        turn_line(prompt, "role", None, Path::new("/work"), 60)
    }

    fn texts(outs: &[Output]) -> Vec<String> {
        outs.iter()
            .filter_map(|o| match o {
                Output::Text(t) | Output::Final(t) | Output::Session(t) | Output::Tool(t) => {
                    Some(t.clone())
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_turn_plays_its_lines_and_the_run_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let program = FakeCustom::install(
            dir.path(),
            CustomScript::new().turn(
                CustomTurnScript::new()
                    .session("abc")
                    .tool("Read x.rs")
                    .text("Hel")
                    .raw("plain words")
                    .final_("Hello"),
            ),
        );
        let mut run = Run::start(&program, &["--fast", "two words"], work.path());
        let line = turn("What changed?");
        run.send(&line);
        let outs = run.rest();
        assert_eq!(
            outs,
            [
                Output::Session("abc".into()),
                Output::Tool("Read x.rs".into()),
                Output::Text("Hel".into()),
                Output::NotJson("plain words".into()),
                Output::Final("Hello".into()),
            ]
        );
        let calls = FakeCustom::wait_for_calls(dir.path(), 1, WAIT);
        let [call] = calls.as_slice() else {
            panic!("{calls:?}")
        };
        assert_eq!(
            (call.index, call.argv.as_slice()),
            (1, ["--fast".into(), "two words".into()].as_slice())
        );
        assert_eq!(call.stdin, [line.trim_end().to_string()]);
        assert!(!call.is_probe());
        assert_eq!(call.turn_line().unwrap()["prompt"], "What changed?");
        assert_eq!(call.turn_line().unwrap()["protocol"], PROTOCOL);
        assert_eq!(call.env_value("KEEP_ME"), Some("yes"));
        assert_eq!(call.env_value("NOT_SET"), None);
        assert_eq!(
            std::fs::canonicalize(&call.cwd).unwrap(),
            std::fs::canonicalize(work.path()).unwrap()
        );
        assert!(call.replies().is_empty());
    }

    #[test]
    fn an_environment_value_with_a_newline_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeCustom::install(dir.path(), CustomScript::new());
        let mut child = Command::new(&program)
            .env("MULTI", "one\ntwo=three")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(turn("x").as_bytes())
            .unwrap();
        child.wait().unwrap();
        let call = &FakeCustom::wait_for_calls(dir.path(), 1, WAIT)[0];
        assert_eq!(call.env_value("MULTI"), Some("one\ntwo=three"));
        assert!(call.env_value("two").is_none(), "no stray key");
    }

    #[test]
    fn the_nth_turn_plays_the_nth_script_and_the_last_repeats() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeCustom::install(
            dir.path(),
            CustomScript::new()
                .turn(CustomTurnScript::new().final_("first"))
                .turn(CustomTurnScript::new().final_("second")),
        );
        let said = |program: &Path| {
            let mut run = Run::start(program, &[], dir.path());
            run.send(&turn("x"));
            texts(&run.rest())
        };
        assert_eq!(said(&program), ["first"]);
        assert_eq!(said(&program), ["second"]);
        assert_eq!(said(&program), ["second"]);
        assert_eq!(FakeCustom::calls(dir.path()).len(), 3);
    }

    #[test]
    fn a_script_with_no_turns_answers_ok() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeCustom::install(dir.path(), CustomScript::new());
        let mut run = Run::start(&program, &[], dir.path());
        run.send(&turn("x"));
        assert_eq!(run.rest(), [Output::Final("ok".into())]);
    }

    #[test]
    fn the_probe_answers_and_is_not_a_turn() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeCustom::install(
            dir.path(),
            CustomScript::new()
                .probe("Ollama", "0.3", 1)
                .turn(CustomTurnScript::new().final_("turn one")),
        );
        let mut probe = Run::start(&program, &[], dir.path());
        probe.send(&probe_line());
        assert_eq!(
            probe.rest(),
            [Output::Probe {
                name: "Ollama".into(),
                version: Some("0.3".into()),
                protocol: Some(1)
            }]
        );
        let mut run = Run::start(&program, &[], dir.path());
        run.send(&turn("x"));
        assert_eq!(texts(&run.rest()), ["turn one"], "the probe took no turn");
        let calls = FakeCustom::wait_for_calls(dir.path(), 2, WAIT);
        assert_eq!(
            calls.iter().map(CustomCall::is_probe).collect::<Vec<_>>(),
            [true, false]
        );
    }

    #[test]
    fn a_default_probe_says_fake_and_a_raw_one_says_what_it_is_given() {
        for (script, want) in [
            (
                CustomScript::new(),
                Output::Probe {
                    name: "Fake".into(),
                    version: Some("1.0".into()),
                    protocol: Some(1),
                },
            ),
            (
                CustomScript::new().probe("Old", "0.1", 2),
                Output::Probe {
                    name: "Old".into(),
                    version: Some("0.1".into()),
                    protocol: Some(2),
                },
            ),
            (
                CustomScript::new().probe_raw(r#"{"type":"probe","name":"Bare"}"#),
                Output::Probe {
                    name: "Bare".into(),
                    version: None,
                    protocol: None,
                },
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let program = FakeCustom::install(dir.path(), script);
            let mut run = Run::start(&program, &[], dir.path());
            run.send(&probe_line());
            assert_eq!(run.rest(), [want]);
        }
    }

    #[test]
    fn a_silent_probe_never_answers_and_stops_on_sigterm() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeCustom::install(dir.path(), CustomScript::new().silent_probe());
        let mut run = Run::start(&program, &[], dir.path());
        run.send(&probe_line());
        let calls = FakeCustom::wait_for_calls(dir.path(), 1, WAIT);
        assert!(calls[0].is_probe());
        std::thread::sleep(Duration::from_millis(200));
        assert!(FakeCustom::is_running(run.child.id()), "still waiting");
        run.signal("TERM", false);
        assert!(run.child.wait().is_ok());
        assert!(run.rest().is_empty());
    }

    #[test]
    fn a_permission_round_trip_records_the_decision_and_the_reply() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeCustom::install(
            dir.path(),
            CustomScript::new().turn(
                CustomTurnScript::new()
                    .ask_command("p1", "cargo test")
                    .ask_edit("p2", &["a.rs", "b/c.rs"])
                    .ask_other("p3", "Delete the cache")
                    .final_("done"),
            ),
        );
        let mut run = Run::start(&program, &[], dir.path());
        run.send(&turn("x"));
        for (id, kind, allow) in [
            ("p1", AskKind::Command, true),
            ("p2", AskKind::Edit, false),
            ("p3", AskKind::Other, true),
        ] {
            let Some(Output::Permission {
                id: got,
                kind: k,
                detail,
                paths,
                reason,
            }) = run.line()
            else {
                panic!("a request for {id}")
            };
            assert_eq!((got.as_str(), &k), (id, &kind));
            assert!(reason.is_some());
            match id {
                "p1" => assert_eq!(detail, "cargo test"),
                "p2" => assert_eq!(paths, ["a.rs", "b/c.rs"]),
                _ => assert_eq!(detail, "Delete the cache"),
            }
            run.send(&reply_line(id, allow));
        }
        assert_eq!(run.rest(), [Output::Final("done".into())]);
        assert_eq!(
            FakeCustom::wait_for_decisions(dir.path(), 3, WAIT),
            [
                ("p1".to_string(), "allow".to_string()),
                ("p2".to_string(), "deny".to_string()),
                ("p3".to_string(), "allow".to_string()),
            ]
        );
        let call = &FakeCustom::calls(dir.path())[0];
        let replies: Vec<_> = call.replies().iter().map(|r| r["id"].clone()).collect();
        assert_eq!(replies, ["p1", "p2", "p3"]);
    }

    #[test]
    fn a_request_goes_on_when_stdin_is_closed_and_records_no_decision() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeCustom::install(
            dir.path(),
            CustomScript::new().turn(
                CustomTurnScript::new()
                    .ask_command("p1", "ls")
                    .ask_raw(r#"{"type":"permission","id":"p9","kind":"delete"}"#)
                    .final_("after"),
            ),
        );
        let mut run = Run::start(&program, &[], dir.path());
        run.send(&turn("x"));
        run.close_stdin();
        let outs = run.rest();
        assert!(matches!(
            outs.as_slice(),
            [
                Output::Permission { id: a, .. },
                Output::Permission { id: b, kind: AskKind::Unknown(k), .. },
                Output::Final(_)
            ] if a == "p1" && b == "p9" && k == "delete"
        ));
        assert!(FakeCustom::decisions(dir.path()).is_empty());
    }

    #[test]
    fn crash_garbage_and_stderr_come_out_as_scripted() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeCustom::install(
            dir.path(),
            CustomScript::new().turn(
                CustomTurnScript::new()
                    .stderr("it broke")
                    .garbage()
                    .error("limit", "slow down")
                    .crash(3),
            ),
        );
        let mut run = Run::start(&program, &[], dir.path());
        run.send(&turn("x"));
        let outs = run.rest();
        assert!(matches!(&outs[0], Output::NotJson(t) if t == "this is not json at all"));
        assert!(matches!(&outs[1], Output::NotJson(_)));
        assert!(matches!(&outs[2], Output::Error { .. }));
        assert_eq!(outs.len(), 3, "nothing after the crash");
        assert_eq!(run.child.wait().unwrap().code(), Some(3));
        assert_eq!(run.stderr().trim(), "it broke");
    }

    #[test]
    fn an_oversized_line_is_one_byte_past_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeCustom::install(
            dir.path(),
            CustomScript::new().turn(CustomTurnScript::new().oversized_line().final_("x")),
        );
        let mut run = Run::start(&program, &[], dir.path());
        run.send(&turn("x"));
        let mut line = String::new();
        run.out.read_line(&mut line).unwrap();
        assert_eq!(line.trim_end_matches('\n').len(), MAX_LINE + 1);
    }

    #[test]
    fn a_delay_waits_before_the_next_step() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeCustom::install(
            dir.path(),
            CustomScript::new().turn(CustomTurnScript::new().delay_ms(300).final_("late")),
        );
        let mut run = Run::start(&program, &[], dir.path());
        let started = Instant::now();
        run.send(&turn("x"));
        assert_eq!(run.rest(), [Output::Final("late".into())]);
        assert!(started.elapsed() >= Duration::from_millis(300));
    }

    #[test]
    fn a_stalled_run_and_its_child_stop_on_sigterm_to_the_group() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeCustom::install(
            dir.path(),
            CustomScript::new().turn(CustomTurnScript::new().text("go").stall()),
        );
        let mut run = Run::start(&program, &[], dir.path());
        run.send(&turn("x"));
        assert_eq!(run.line(), Some(Output::Text("go".into())));
        let call = loop {
            let call = FakeCustom::wait_for_calls(dir.path(), 1, WAIT).remove(0);
            if call.child.is_some() {
                break call;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let child = call.child.unwrap();
        assert!(FakeCustom::is_running(child));
        run.signal("TERM", true);
        run.child.wait().unwrap();
        for _ in 0..100 {
            if !FakeCustom::is_running(child) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!FakeCustom::is_running(child), "the group's child is gone");
    }

    #[test]
    fn ignore_term_lets_a_run_record_the_reply_written_while_it_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeCustom::install(
            dir.path(),
            CustomScript::new().turn(
                CustomTurnScript::new()
                    .ignore_term()
                    .session("s1")
                    .ask_command("p1", "cargo test")
                    .final_("Never said."),
            ),
        );
        let mut run = Run::start(&program, &[], dir.path());
        run.send(&turn("x"));
        assert_eq!(run.line(), Some(Output::Session("s1".into())));
        assert!(matches!(run.line(), Some(Output::Permission { .. })));
        run.signal("TERM", true);
        run.send(&reply_line("p1", false));
        assert_eq!(run.rest(), [Output::Final("Never said.".into())]);
        assert_eq!(
            FakeCustom::wait_for_decisions(dir.path(), 1, WAIT),
            [("p1".to_string(), "deny".to_string())]
        );
        assert_eq!(
            run.child.wait().unwrap().code(),
            Some(0),
            "it ended by itself"
        );
    }

    #[test]
    fn a_hung_run_ignores_sigterm_and_only_sigkill_ends_it_and_its_child() {
        let dir = tempfile::tempdir().unwrap();
        let program = FakeCustom::install(
            dir.path(),
            CustomScript::new().turn(CustomTurnScript::new().text("go").hang()),
        );
        let mut run = Run::start(&program, &[], dir.path());
        run.send(&turn("x"));
        assert_eq!(run.line(), Some(Output::Text("go".into())));
        let call = loop {
            let call = FakeCustom::wait_for_calls(dir.path(), 1, WAIT).remove(0);
            if call.child.is_some() {
                break call;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let child = call.child.unwrap();
        run.signal("TERM", true);
        std::thread::sleep(Duration::from_millis(400));
        assert!(FakeCustom::is_running(call.pid), "the script ignores TERM");
        assert!(FakeCustom::is_running(child), "and so does its child");
        run.signal("KILL", true);
        run.child.wait().unwrap();
        for _ in 0..100 {
            if !FakeCustom::is_running(child) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!FakeCustom::is_running(child));
    }
}
