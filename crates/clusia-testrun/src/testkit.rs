//! A scripted stand-in for [`Exec`]: rules answer by program suffix and argument prefix, and
//! every call is recorded.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use crate::exec::{BoxFut, CommandSpec, Ended, Exec, Output, StopFlag, Stream};

#[derive(Debug, Clone)]
enum Reply {
    Done {
        code: i32,
        stdout: String,
        stderr: String,
    },
    Chunks {
        chunks: Vec<(Stream, String)>,
        code: i32,
    },
    Hang,
    Missing,
}

#[derive(Debug, Clone)]
pub struct FakeReply(Reply);

impl FakeReply {
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self(Reply::Done {
            code: 0,
            stdout: stdout.into(),
            stderr: String::new(),
        })
    }

    pub fn fail(code: i32, stderr: impl Into<String>) -> Self {
        Self(Reply::Done {
            code,
            stdout: String::new(),
            stderr: stderr.into(),
        })
    }

    pub fn chunks(chunks: Vec<(Stream, String)>, code: i32) -> Self {
        Self(Reply::Chunks { chunks, code })
    }

    /// Never ends on its own: only the limit or a stop does.
    pub fn hang() -> Self {
        Self(Reply::Hang)
    }

    /// The program is not there.
    pub fn missing() -> Self {
        Self(Reply::Missing)
    }
}

struct Rule {
    suffix: String,
    prefix: Vec<String>,
    replies: Vec<FakeReply>,
    used: usize,
}

#[derive(Default)]
struct State {
    rules: Vec<Rule>,
    executable: BTreeSet<PathBuf>,
    calls: Vec<CommandSpec>,
}

#[derive(Default)]
pub struct FakeExec {
    state: Mutex<State>,
}

impl FakeExec {
    pub fn new() -> Self {
        Self::default()
    }

    /// Answers a call whose program path ends with `suffix` and whose arguments start with
    /// `args_prefix`. The first matching rule answers.
    pub fn on(&self, suffix: &str, args_prefix: &[&str], reply: FakeReply) -> &Self {
        self.on_sequence(suffix, args_prefix, vec![reply])
    }

    /// Like `on`, answering with each reply in turn and then with the last one again.
    pub fn on_sequence(
        &self,
        suffix: &str,
        args_prefix: &[&str],
        replies: Vec<FakeReply>,
    ) -> &Self {
        assert!(!replies.is_empty(), "a rule needs a reply");
        self.lock().rules.push(Rule {
            suffix: suffix.to_string(),
            prefix: args_prefix.iter().map(|a| a.to_string()).collect(),
            replies,
            used: 0,
        });
        self
    }

    pub fn executable_at(&self, path: impl AsRef<Path>) -> &Self {
        self.lock().executable.insert(path.as_ref().to_path_buf());
        self
    }

    pub fn calls(&self) -> Vec<CommandSpec> {
        self.lock().calls.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records the call and picks its answer. No rule is a program that is not there.
    fn answer(&self, cmd: &CommandSpec) -> Reply {
        let mut state = self.lock();
        state.calls.push(cmd.clone());
        let program = cmd.program.to_string_lossy();
        let args: Vec<String> = cmd
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        for rule in &mut state.rules {
            if program.ends_with(&rule.suffix) && args.starts_with(&rule.prefix) {
                let reply = rule.replies[rule.used.min(rule.replies.len() - 1)].clone();
                rule.used += 1;
                return reply.0;
            }
        }
        Reply::Missing
    }
}

fn not_found(cmd: &CommandSpec) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{} is not there", cmd.program.display()),
    )
}

impl Exec for FakeExec {
    fn executable(&self, path: &Path) -> bool {
        self.lock().executable.contains(path)
    }

    fn output<'a>(
        &'a self,
        cmd: &'a CommandSpec,
        limit: Duration,
    ) -> BoxFut<'a, io::Result<Output>> {
        Box::pin(async move {
            match self.answer(cmd) {
                Reply::Missing => Err(not_found(cmd)),
                Reply::Hang => {
                    tokio::time::sleep(limit).await;
                    Ok(Output {
                        timed_out: true,
                        ..Output::default()
                    })
                }
                Reply::Done {
                    code,
                    stdout,
                    stderr,
                } => Ok(Output {
                    code: Some(code),
                    stdout: stdout.into_bytes(),
                    stderr: stderr.into_bytes(),
                    timed_out: false,
                }),
                Reply::Chunks { chunks, code } => {
                    let mut out = Output {
                        code: Some(code),
                        ..Output::default()
                    };
                    for (stream, text) in chunks {
                        match stream {
                            Stream::Out => out.stdout.extend_from_slice(text.as_bytes()),
                            Stream::Err => out.stderr.extend_from_slice(text.as_bytes()),
                        }
                    }
                    Ok(out)
                }
            }
        })
    }

    fn stream<'a>(
        &'a self,
        cmd: &'a CommandSpec,
        limit: Duration,
        stop: &'a StopFlag,
        chunk: &'a mut (dyn FnMut(Stream, &[u8]) + Send),
    ) -> BoxFut<'a, io::Result<Ended>> {
        Box::pin(async move {
            let (pieces, code) = match self.answer(cmd) {
                Reply::Missing => return Err(not_found(cmd)),
                Reply::Hang => {
                    return Ok(tokio::select! {
                        _ = stop.notified() => Ended::Stopped,
                        _ = tokio::time::sleep(limit) => Ended::TimedOut,
                    });
                }
                Reply::Chunks { chunks, code } => (chunks, code),
                Reply::Done {
                    code,
                    stdout,
                    stderr,
                } => {
                    let mut pieces = Vec::new();
                    if !stdout.is_empty() {
                        pieces.push((Stream::Out, stdout));
                    }
                    if !stderr.is_empty() {
                        pieces.push((Stream::Err, stderr));
                    }
                    (pieces, code)
                }
            };
            for (stream, text) in pieces {
                if stop.fired() {
                    return Ok(Ended::Stopped);
                }
                chunk(stream, text.as_bytes());
            }
            Ok(Ended::Exited(Some(code)))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    fn docker(args: &[&str]) -> CommandSpec {
        CommandSpec::new("/opt/homebrew/bin/docker").args(args)
    }

    #[tokio::test]
    async fn the_first_matching_rule_answers() {
        let fake = FakeExec::new();
        fake.on("docker", &["info"], FakeReply::ok("first"))
            .on("docker", &[], FakeReply::ok("any docker"))
            .on("docker", &["info"], FakeReply::ok("never"));
        let limit = Duration::from_secs(1);
        let info = fake
            .output(&docker(&["info", "--format", "x"]), limit)
            .await
            .unwrap();
        assert_eq!((info.code, info.stdout), (Some(0), b"first".to_vec()));
        let other = fake.output(&docker(&["ps"]), limit).await.unwrap();
        assert_eq!(other.stdout, b"any docker");
    }

    #[tokio::test]
    async fn an_unmatched_call_is_not_found_and_is_still_recorded() {
        let fake = FakeExec::new();
        let err = fake
            .output(&docker(&["ps"]), Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(fake.calls().len(), 1);
        fake.on("docker", &["ps"], FakeReply::missing());
        let err = fake
            .output(&docker(&["ps"]), Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn a_failing_reply_carries_code_and_stderr() {
        let fake = FakeExec::new();
        fake.on("docker", &["info"], FakeReply::fail(1, "cannot connect"));
        let out = fake
            .output(&docker(&["info"]), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(
            (out.code, out.stderr),
            (Some(1), b"cannot connect".to_vec())
        );
        assert!(out.stdout.is_empty());
    }

    #[tokio::test]
    async fn a_sequence_answers_in_turn_and_repeats_its_last() {
        let fake = FakeExec::new();
        fake.on_sequence(
            "docker",
            &["info"],
            vec![FakeReply::fail(1, "down"), FakeReply::ok("up")],
        );
        let limit = Duration::from_secs(1);
        let codes = [
            fake.output(&docker(&["info"]), limit).await.unwrap().code,
            fake.output(&docker(&["info"]), limit).await.unwrap().code,
            fake.output(&docker(&["info"]), limit).await.unwrap().code,
        ];
        assert_eq!(codes, [Some(1), Some(0), Some(0)]);
    }

    #[tokio::test]
    async fn chunks_reach_stream_in_order_and_stop_cuts_them() {
        let fake = FakeExec::new();
        fake.on(
            "sh",
            &[],
            FakeReply::chunks(
                vec![
                    (Stream::Out, "a".into()),
                    (Stream::Err, "b".into()),
                    (Stream::Out, "c".into()),
                ],
                5,
            ),
        );
        let cmd = CommandSpec::new("/bin/sh").arg("-c").arg("x");
        let stop = StopFlag::new();
        let mut seen = Vec::new();
        let mut chunk =
            |s: Stream, b: &[u8]| seen.push((s, String::from_utf8_lossy(b).into_owned()));
        let ended = fake
            .stream(&cmd, Duration::from_secs(1), &stop, &mut chunk)
            .await
            .unwrap();
        assert_eq!(ended, Ended::Exited(Some(5)));
        assert_eq!(
            seen,
            vec![
                (Stream::Out, "a".into()),
                (Stream::Err, "b".into()),
                (Stream::Out, "c".into())
            ]
        );

        let stop = StopFlag::new();
        let flag = stop.clone();
        let mut count = 0;
        let mut chunk = |_: Stream, _: &[u8]| {
            count += 1;
            flag.fire();
        };
        let ended = fake
            .stream(&cmd, Duration::from_secs(1), &stop, &mut chunk)
            .await
            .unwrap();
        assert_eq!((ended, count), (Ended::Stopped, 1));
    }

    #[tokio::test(start_paused = true)]
    async fn hang_waits_for_the_limit_or_a_stop() {
        let fake = FakeExec::new();
        fake.on("docker", &[], FakeReply::hang());
        let out = fake
            .output(&docker(&["run"]), Duration::from_secs(30))
            .await
            .unwrap();
        assert!(out.timed_out);
        assert_eq!(out.code, None);

        let stop = StopFlag::new();
        let mut chunk = |_: Stream, _: &[u8]| {};
        let ended = fake
            .stream(
                &docker(&["run"]),
                Duration::from_secs(30),
                &stop,
                &mut chunk,
            )
            .await
            .unwrap();
        assert_eq!(ended, Ended::TimedOut);

        let stop = StopFlag::new();
        stop.fire();
        let ended = fake
            .stream(
                &docker(&["run"]),
                Duration::from_secs(30),
                &stop,
                &mut chunk,
            )
            .await
            .unwrap();
        assert_eq!(ended, Ended::Stopped);
    }

    #[tokio::test]
    async fn calls_hold_output_and_stream_calls_in_order() {
        let fake = FakeExec::new();
        fake.on("docker", &[], FakeReply::ok(""));
        let limit = Duration::from_secs(1);
        fake.output(&docker(&["one"]), limit).await.unwrap();
        let stop = StopFlag::new();
        let mut chunk = |_: Stream, _: &[u8]| {};
        fake.stream(&docker(&["two"]), limit, &stop, &mut chunk)
            .await
            .unwrap();
        let args: Vec<_> = fake.calls().into_iter().map(|c| c.args).collect();
        assert_eq!(
            args,
            vec![vec![OsString::from("one")], vec![OsString::from("two")]]
        );
    }

    #[test]
    fn only_listed_paths_are_executable() {
        let fake = FakeExec::new();
        fake.on("docker", &[], FakeReply::ok(""));
        assert!(
            !fake.executable(Path::new("/opt/homebrew/bin/docker")),
            "a rule is not a program"
        );
        fake.executable_at("/opt/homebrew/bin/docker");
        assert!(fake.executable(Path::new("/opt/homebrew/bin/docker")));
        assert!(!fake.executable(Path::new("/usr/local/bin/docker")));
    }
}
