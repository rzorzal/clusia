//! `clusia harness test`: puts a program through the probe and one short turn of Clúsia's
//! JSON-lines protocol, and says line by line what passed. It never talks to the daemon, and it
//! runs the program as a turn would: in a folder of its own, without the GitHub tokens or any
//! `CLUSIA_*` variable.

use std::ffi::OsString;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use clusia_core::Paths;
use clusia_core::config::HarnessKind;
use clusia_harness::custom::{
    self, AskKind, LineReader, Output as Line, PROBE_LIMIT, PROTOCOL, SEND_LIMIT, STOP_GRACE,
};
use clusia_harness::{Agent, ROLE_PROMPT};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;

use crate::agent::printable;
use crate::run::{CliError, Output};

/// How long the turn may take.
const STEP_LIMIT: Duration = Duration::from_secs(60);
const TURN_PROMPT: &str = "Reply with the word ok";
const PERMISSION_PROMPT: &str =
    "Ask permission to run the command `true`, then reply with the word ok";

/// A folder the program runs in, removed when the test ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self, CliError> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "clusia-harness-test-{}-{nanos}",
            std::process::id()
        ));
        Self::create(path)
    }

    /// Makes `path` and only `path`: a folder that already exists is somebody else's, and the
    /// program must not run in it. Only the user may enter it.
    fn create(path: PathBuf) -> Result<Self, CliError> {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|e| CliError::Other(format!("cannot create {}: {e}", path.display())))?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn signal_group(pid: Option<u32>, signal: i32) {
    if let Some(pid) = pid {
        // SAFETY: `killpg` only sends a signal, to the group the child leads
        // (`process_group(0)` at spawn).
        unsafe {
            libc::killpg(pid as i32, signal);
        }
    }
}

/// How much of what the program wrote to its standard error a failed check shows.
const STDERR_TAIL: usize = 2048;

/// Whether the child ended, without reaping it: until it is reaped its pid, and so the id of the
/// group it leads, cannot be reused, so the group can still be signalled safely.
fn has_exited(pid: u32) -> bool {
    loop {
        // SAFETY: `waitid` only writes into `info`, which is zeroed and owned here; `WNOWAIT`
        // leaves the child waitable for `Child::wait`.
        let (found, info) = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let found = libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            );
            (found, info)
        };
        if found == 0 {
            return info.si_signo == libc::SIGCHLD;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::ECHILD) => return true,
            _ => return false,
        }
    }
}

/// Reads the program's standard error to its end, keeping the last `STDERR_TAIL` bytes.
fn collect_tail(mut stderr: ChildStderr) -> JoinHandle<Vec<u8>> {
    tokio::spawn(async move {
        let mut tail: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        while let Ok(n) = stderr.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&chunk[..n]);
            if tail.len() > STDERR_TAIL {
                tail.drain(..tail.len() - STDERR_TAIL);
            }
        }
        tail
    })
}

/// The last lines of what the program wrote to its standard error, as one line safe to print.
fn shown_tail(tail: &[u8]) -> String {
    let text = String::from_utf8_lossy(tail);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let from = lines.len().saturating_sub(3);
    printable(&lines[from..].join(" / "))
}

/// `detail` of a failed check with what the program said on its standard error, when it said
/// anything.
fn with_stderr(detail: Option<String>, stderr: &str) -> Option<String> {
    match (detail, stderr.is_empty()) {
        (detail, true) => detail,
        (Some(detail), false) => Some(format!("{detail}; it wrote to stderr: {stderr}")),
        (None, false) => Some(format!("it wrote to stderr: {stderr}")),
    }
}

/// One process of the program, with its pipes.
struct Running {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: LineReader<BufReader<ChildStdout>>,
    stderr: JoinHandle<Vec<u8>>,
}

impl Running {
    fn start(program: &Path, extra_args: &[String], cwd: &Path) -> std::io::Result<Self> {
        let env: Vec<(OsString, OsString)> = std::env::vars_os().collect();
        let mut command = std::process::Command::new(program);
        command.args(extra_args).current_dir(cwd);
        Agent::Custom.set_env(&mut command, &env);
        let mut command = Command::from(command);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = collect_tail(child.stderr.take().expect("stderr is piped"));
        Ok(Self {
            child,
            stdin,
            lines: LineReader::new(BufReader::new(stdout)),
            stderr,
        })
    }

    /// Writes one line and says whether it was written; a program that does not read it loses
    /// its input.
    async fn send(&mut self, line: &str) -> bool {
        let Some(stdin) = self.stdin.as_mut() else {
            return false;
        };
        let write = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await
        };
        if matches!(tokio::time::timeout(SEND_LIMIT, write).await, Ok(Ok(()))) {
            true
        } else {
            self.stdin = None;
            false
        }
    }

    /// Ends the process: its input closes, SIGTERM goes to its group, and SIGKILL to the group
    /// once the leader left or the grace is over, so a child that ignores SIGTERM does not
    /// outlive the test. The leader is reaped only after the last signal, so its pid, and the
    /// group's, cannot belong to somebody else while it is signalled. Returns what it wrote to
    /// its standard error, ready to print.
    async fn end(mut self) -> String {
        self.stdin = None;
        let pid = self.child.id();
        signal_group(pid, libc::SIGTERM);
        if let Some(pid) = pid {
            let give_up = std::time::Instant::now() + STOP_GRACE;
            while !has_exited(pid) && std::time::Instant::now() < give_up {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        signal_group(pid, libc::SIGKILL);
        let _ = self.child.wait().await;
        // A child of the program may hold the pipe open after the group is gone.
        match tokio::time::timeout(Duration::from_secs(1), &mut self.stderr).await {
            Ok(Ok(tail)) => shown_tail(&tail),
            _ => {
                self.stderr.abort();
                String::new()
            }
        }
    }
}

#[derive(Default)]
struct Probed {
    answered: bool,
    name: Option<String>,
    protocol: Option<u64>,
    detail: Option<String>,
    stderr: String,
}

async fn probe(program: &Path, extra_args: &[String], cwd: &Path) -> Probed {
    let mut running = match Running::start(program, extra_args, cwd) {
        Ok(running) => running,
        Err(e) => {
            return Probed {
                detail: Some(format!("cannot start it: {e}")),
                ..Probed::default()
            };
        }
    };
    running.send(&custom::probe_line()).await;
    let answer = tokio::time::timeout(PROBE_LIMIT, async {
        loop {
            match running.lines.next().await {
                Ok(Some(line)) => {
                    if let Line::Probe { name, protocol, .. } = custom::read_line(&line) {
                        return Some((name, protocol));
                    }
                }
                _ => return None,
            }
        }
    })
    .await;
    let stderr = running.end().await;
    match answer {
        Ok(Some((name, protocol))) => Probed {
            answered: true,
            name: Some(name),
            protocol,
            detail: None,
            stderr,
        },
        Ok(None) => Probed {
            detail: Some("it ended without answering the probe".to_string()),
            stderr,
            ..Probed::default()
        },
        Err(_) => Probed {
            detail: Some(format!("no answer within {} s", PROBE_LIMIT.as_secs())),
            stderr,
            ..Probed::default()
        },
    }
}

struct Turned {
    started: bool,
    every_json: bool,
    session: bool,
    finished: bool,
    /// A command request arrived.
    asked: bool,
    /// Its `deny` reply was written.
    answered: bool,
    detail: Option<String>,
    stderr: String,
}

async fn turn(program: &Path, extra_args: &[String], cwd: &Path, prompt: &str) -> Turned {
    let mut turned = Turned {
        started: false,
        every_json: true,
        session: false,
        finished: false,
        asked: false,
        answered: false,
        detail: None,
        stderr: String::new(),
    };
    let mut running = match Running::start(program, extra_args, cwd) {
        Ok(running) => running,
        Err(e) => {
            turned.detail = Some(format!("cannot start it: {e}"));
            return turned;
        }
    };
    turned.started = true;
    let first = custom::turn_line(prompt, ROLE_PROMPT, None, cwd, STEP_LIMIT.as_secs());
    running.send(&first).await;
    let read = tokio::time::timeout(STEP_LIMIT, async {
        loop {
            match running.lines.next().await {
                Ok(Some(line)) => match custom::read_line(&line) {
                    Line::NotJson(_) => turned.every_json = false,
                    Line::Session(_) => turned.session = true,
                    Line::Permission { id, kind, .. } => {
                        let sent = running.send(&custom::reply_line(&id, false)).await;
                        if kind == AskKind::Command {
                            turned.asked = true;
                            turned.answered |= sent;
                        }
                    }
                    Line::Final(_) => {
                        turned.finished = true;
                        return;
                    }
                    Line::Error { message, .. } => {
                        turned.detail = Some(format!("it reported an error: {message}"));
                        return;
                    }
                    _ => {}
                },
                Ok(None) => {
                    turned.detail = Some("it ended without a final line".to_string());
                    return;
                }
                Err(_) => {
                    turned.detail = Some("it wrote more than Clúsia reads in one turn".to_string());
                    return;
                }
            }
        }
    })
    .await;
    if read.is_err() {
        turned.detail = Some(format!("no final line within {} s", STEP_LIMIT.as_secs()));
    }
    turned.stderr = running.end().await;
    turned
}

/// One line of the report.
struct Check {
    label: &'static str,
    ok: bool,
    detail: Option<String>,
}

/// What to run: the program, and the arguments that go with it.
struct Target {
    program: PathBuf,
    extra_args: Vec<String>,
}

fn not_absolute(program: &str, what: &str) -> CliError {
    CliError::Usage(format!("{program} is not an absolute path. {what}"))
}

/// The program to test: the one passed, made absolute when it is a path (a bare name is refused:
/// a turn never searches `PATH` either), else the configured custom command with the extra
/// arguments Config holds for it.
fn resolve(paths: &Paths, program: Option<String>) -> Result<Target, CliError> {
    if let Some(program) = program {
        if !program.contains('/') {
            return Err(not_absolute(&program, "Pass the path of the program"));
        }
        let program = std::path::absolute(&program)
            .map_err(|e| CliError::Other(format!("cannot resolve {program}: {e}")))?;
        return Ok(Target {
            program,
            extra_args: Vec::new(),
        });
    }
    let config = clusia_store::load_config(paths)
        .map_err(|e| CliError::Other(e.to_string()))?
        .into_value();
    if config.harness.kind != HarnessKind::Custom {
        return Err(CliError::Usage(
            "Pass the program to test, or set harness.kind to custom".to_string(),
        ));
    }
    let Some(program) = config
        .harness
        .program
        .as_deref()
        .map(str::trim)
        .filter(|program| !program.is_empty())
    else {
        return Err(CliError::Usage(
            "Set harness.program, or pass the program to test".to_string(),
        ));
    };
    if !Path::new(program).is_absolute() {
        return Err(not_absolute(program, "Set it in harness.program"));
    }
    let extra_args = config
        .harness
        .extra_args_list()
        .map_err(|e| CliError::Usage(format!("harness.extra_args: {e}")))?;
    Ok(Target {
        program: PathBuf::from(program),
        extra_args,
    })
}

pub(crate) async fn run(
    paths: &Paths,
    program: Option<String>,
    permission: bool,
    json: bool,
) -> Result<Output, CliError> {
    let Target {
        program,
        extra_args,
    } = resolve(paths, program)?;
    let scratch = Scratch::new()?;
    let probed = probe(&program, &extra_args, &scratch.0).await;
    let file_name = program
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "the program".to_string());
    let name = printable(
        probed
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(&file_name),
    );
    if !json {
        println!("This runs one turn of {name}'s model.");
    }
    let prompt = if permission {
        PERMISSION_PROMPT
    } else {
        TURN_PROMPT
    };
    let turned = turn(&program, &extra_args, &scratch.0, prompt).await;

    // What the program wrote to its standard error explains a check only when it failed.
    let explained = |ok: bool, detail: Option<String>, stderr: &str| {
        if ok {
            None
        } else {
            with_stderr(detail, stderr)
        }
    };
    // The turn's standard error goes with the first of its checks that failed, not with each.
    let told = std::cell::Cell::new(false);
    let turn_explained = |ok: bool, detail: Option<String>| {
        if ok {
            None
        } else if told.replace(true) {
            detail
        } else {
            with_stderr(detail, &turned.stderr)
        }
    };
    let mut checks = vec![
        Check {
            label: "answers the probe",
            ok: probed.answered,
            detail: explained(probed.answered, probed.detail.clone(), &probed.stderr),
        },
        Check {
            label: "speaks protocol 1",
            ok: probed.protocol == Some(PROTOCOL),
            detail: match (probed.answered, probed.protocol) {
                (true, Some(other)) if other != PROTOCOL => {
                    Some(format!("it speaks protocol {other}"))
                }
                (true, None) => Some("it did not say which protocol".to_string()),
                _ => None,
            },
        },
        Check {
            label: "every line is JSON",
            ok: turned.started && turned.every_json,
            detail: turn_explained(turned.started && turned.every_json, None),
        },
        Check {
            label: "returns a session id",
            ok: turned.started && turned.session,
            detail: turn_explained(turned.started && turned.session, None),
        },
        Check {
            label: "ends with final",
            ok: turned.finished,
            detail: turn_explained(turned.finished, turned.detail.clone()),
        },
    ];
    if permission {
        let ok = turned.asked && turned.answered && turned.finished;
        let detail = if !turned.asked {
            Some("no command request arrived".to_string())
        } else if !turned.answered {
            Some("it did not read the deny reply".to_string())
        } else {
            None
        };
        checks.push(Check {
            label: "asks permission and reads the reply",
            ok,
            detail: turn_explained(ok, detail),
        });
    }
    let all_ok = checks.iter().all(|check| check.ok);

    if json {
        let checks: Vec<Value> = checks
            .iter()
            .map(|check| {
                let mut line = json!({ "label": check.label, "ok": check.ok });
                if let Some(detail) = &check.detail {
                    line["detail"] = json!(detail);
                }
                line
            })
            .collect();
        println!(
            "{}",
            json!({
                "program": program.display().to_string(),
                "name": name,
                "ok": all_ok,
                "checks": checks,
            })
        );
    } else {
        for check in &checks {
            println!("{:<6}{}", if check.ok { "ok" } else { "FAIL" }, check.label);
            if let (false, Some(detail)) = (check.ok, &check.detail) {
                println!("      {}", printable(detail));
            }
        }
    }
    if all_ok {
        // Everything is printed already; a null `json` keeps `main` from printing again.
        Ok(Output {
            human: String::new(),
            json: Value::Null,
        })
    } else {
        Err(CliError::Other(
            "the custom command did not pass every check".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn the_scratch_folder_is_new_and_private() {
        let base = tempfile::tempdir().unwrap();
        let path = base.path().join("scratch");
        let made = Scratch::create(path.clone()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(
            Scratch::create(path.clone()).is_err(),
            "a folder that exists is never taken"
        );
        drop(made);
        assert!(!path.exists(), "removed at the end");
    }

    #[test]
    fn stderr_is_one_printable_line_of_its_last_lines() {
        assert_eq!(shown_tail(b""), "");
        let shown = shown_tail(b"a\n\nb\n\x1b[2Kc\nd\n");
        assert!(!shown.contains('\u{1b}'), "{shown:?}");
        assert!(
            shown.ends_with("c / d") && !shown.contains('a'),
            "{shown:?}"
        );
        assert_eq!(
            with_stderr(Some("it ended".into()), "boom").as_deref(),
            Some("it ended; it wrote to stderr: boom")
        );
        assert_eq!(with_stderr(None, ""), None);
    }
}
