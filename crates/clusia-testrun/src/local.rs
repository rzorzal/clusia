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

/// `run_local`, also handing `started` the process group's id right after the start.
pub async fn run_local_group(
    exec: &dyn Exec,
    cmd: &CommandSpec,
    limit: Duration,
    stop: &StopFlag,
    chunk: &mut (dyn FnMut(Stream, &[u8]) + Send),
    started: &mut (dyn FnMut(u32) + Send),
) -> io::Result<Ended> {
    if stop.fired() {
        return Ok(Ended::Stopped);
    }
    exec.stream_group(cmd, limit, stop, chunk, started).await
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

    #[tokio::test]
    async fn run_local_group_reports_the_group_and_a_stop_first_runs_nothing() {
        let fake = FakeExec::new();
        fake.on(
            "sandbox-exec",
            &["-f"],
            FakeReply::chunks(vec![(Stream::Out, "ok\n".into())], 0),
        );
        fake.group(4242, vec![None]);
        let stop = StopFlag::new();
        let mut groups = Vec::new();
        let mut chunk = |_: Stream, _: &[u8]| {};
        let ended = run_local_group(
            &fake,
            &spec(),
            Duration::from_secs(5),
            &stop,
            &mut chunk,
            &mut |g| groups.push(g),
        )
        .await
        .unwrap();
        assert_eq!(
            (ended, groups.clone()),
            (Ended::Exited(Some(0)), vec![4242])
        );
        let stopped = StopFlag::new();
        stopped.fire();
        let ended = run_local_group(
            &fake,
            &spec(),
            Duration::from_secs(5),
            &stopped,
            &mut chunk,
            &mut |g| groups.push(g),
        )
        .await
        .unwrap();
        assert_eq!(ended, Ended::Stopped);
        assert_eq!((groups.len(), fake.calls().len()), (1, 1));
    }
}
