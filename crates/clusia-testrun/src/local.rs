//! Runs one command in the local sandbox: the program is `sandbox-exec`, built by
//! [`crate::sandbox::local_command`].

use std::io;
use std::time::Duration;

use crate::exec::{CommandSpec, Ended, Exec, StopFlag, Stream};

pub async fn run_local(
    exec: &dyn Exec,
    cmd: &CommandSpec,
    limit: Duration,
    stop: &StopFlag,
    chunk: &mut (dyn FnMut(Stream, &[u8]) + Send),
) -> io::Result<Ended> {
    if stop.fired() {
        return Ok(Ended::Stopped);
    }
    exec.stream(cmd, limit, stop, chunk).await
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::exec::CommandSpec;
    use crate::testkit::{FakeExec, FakeReply};

    use super::*;

    fn spec() -> CommandSpec {
        CommandSpec::new("/usr/bin/sandbox-exec")
            .args(["-f", "/p.sb", "/bin/sh", "-c", "cargo test"])
            .clear_env()
            .cwd(Path::new("/w"))
    }

    #[tokio::test]
    async fn output_streams_and_the_exit_code_comes_back() {
        let fake = FakeExec::new();
        fake.on(
            "sandbox-exec",
            &["-f"],
            FakeReply::chunks(
                vec![
                    (Stream::Out, "test a ... ok\n".into()),
                    (Stream::Err, "warning\n".into()),
                ],
                101,
            ),
        );
        let stop = StopFlag::new();
        let mut seen = Vec::new();
        let mut chunk =
            |s: Stream, b: &[u8]| seen.push((s, String::from_utf8_lossy(b).into_owned()));
        let ended = run_local(&fake, &spec(), Duration::from_secs(5), &stop, &mut chunk)
            .await
            .unwrap();
        assert_eq!(ended, Ended::Exited(Some(101)));
        assert_eq!(
            seen,
            [
                (Stream::Out, "test a ... ok\n".to_string()),
                (Stream::Err, "warning\n".to_string())
            ]
        );
        assert_eq!(fake.calls(), [spec()]);
    }

    #[tokio::test]
    async fn a_stop_fired_before_the_start_runs_nothing() {
        let fake = FakeExec::new();
        fake.on("sandbox-exec", &[], FakeReply::ok(""));
        let stop = StopFlag::new();
        stop.fire();
        let mut chunk = |_: Stream, _: &[u8]| {};
        let ended = run_local(&fake, &spec(), Duration::from_secs(5), &stop, &mut chunk)
            .await
            .unwrap();
        assert_eq!(ended, Ended::Stopped);
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn stop_and_limit_end_a_run_that_hangs() {
        let fake = FakeExec::new();
        fake.on("sandbox-exec", &[], FakeReply::hang());
        let stop = StopFlag::new();
        let flag = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            flag.fire();
        });
        let mut chunk = |_: Stream, _: &[u8]| {};
        let ended = run_local(&fake, &spec(), Duration::from_secs(30), &stop, &mut chunk)
            .await
            .unwrap();
        assert_eq!(ended, Ended::Stopped);

        let stop = StopFlag::new();
        let ended = run_local(&fake, &spec(), Duration::from_millis(50), &stop, &mut chunk)
            .await
            .unwrap();
        assert_eq!(ended, Ended::TimedOut);
    }
}
