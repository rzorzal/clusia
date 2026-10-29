//! Running containers: the image build, runs and their kill, markers, the mount check and
//! the compose stack.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use clusia_core::printable::printable;
use clusia_core::tests::{Engine, EngineKind};

use crate::docker::{
    ContainerJob, ContainerNet, Docker, RunMounts, git_file_problem, kill_args, mount_problem,
    run_args,
};
use crate::exec::{CommandSpec, Ended, Exec, Output, StopFlag, Stream};
use crate::image::ImagePlan;
use crate::names::{LabelKind, Labels, sha256_hex};

pub const KILL_TIMEOUT: Duration = Duration::from_secs(15);
pub const CLIENT_GRACE: Duration = Duration::from_secs(10);
pub const MOUNT_TIMEOUT: Duration = Duration::from_secs(60);
pub const COMPOSE_TIMEOUT: Duration = Duration::from_secs(120);

/// How much longer than the run the `docker run` client may live, so the run's own kill
/// comes first.
const CLIENT_SLACK: Duration = Duration::from_secs(30);
/// The limit of a quick engine call: an inspect or a volume create.
const QUICK_TIMEOUT: Duration = Duration::from_secs(30);

type Chunk<'a> = &'a mut (dyn FnMut(Stream, &[u8]) + Send);

pub fn engine_name(kind: EngineKind) -> &'static str {
    match kind {
        EngineKind::Colima => "Colima",
        EngineKind::OrbStack => "OrbStack",
        EngineKind::DockerDesktop => "Docker Desktop",
        EngineKind::Podman => "Podman",
        EngineKind::Other => "The container engine",
    }
}

pub async fn image_present(exec: &dyn Exec, docker: &Docker, tag: &str) -> bool {
    let cmd = docker.command(["image", "inspect", "--format", "{{.Id}}", tag]);
    exec.output(&cmd, QUICK_TIMEOUT)
        .await
        .is_ok_and(|out| out.code == Some(0) && !out.timed_out)
}

/// Builds the plan's base (when it has one), then the generated Dockerfile from `build_dir`,
/// emptied first so the build context holds that file alone and no repo file reaches an
/// image step.
#[allow(clippy::too_many_arguments)]
pub async fn build_image(
    exec: &dyn Exec,
    docker: &Docker,
    plan: &ImagePlan,
    build_dir: &Path,
    labels: &Labels,
    limit: Duration,
    stop: &StopFlag,
    chunk: Chunk<'_>,
) -> io::Result<Ended> {
    if let Some(base) = &plan.base_build {
        let ended = exec
            .stream(&docker.command(base), limit, stop, &mut *chunk)
            .await?;
        if ended != Ended::Exited(Some(0)) {
            return Ok(ended);
        }
    }
    // `remove_dir_all` removes a planted symlink itself and never follows it.
    match std::fs::remove_dir_all(build_dir) {
        Err(e) if e.kind() == io::ErrorKind::NotADirectory => std::fs::remove_file(build_dir)?,
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    std::fs::create_dir_all(build_dir)?;
    let file = build_dir.join("Dockerfile");
    std::fs::write(&file, &plan.dockerfile)?;

    let mut args = vec!["build".to_string()];
    if !plan.network {
        args.extend(["--network".to_string(), "none".to_string()]);
    }
    let image_labels = Labels {
        kind: LabelKind::Image,
        ..labels.clone()
    };
    args.extend(image_labels.args());
    args.extend(["-t".to_string(), plan.tag.clone(), "-f".to_string()]);
    args.push(file.display().to_string());
    args.push(build_dir.display().to_string());
    exec.stream(&docker.command(args), limit, stop, chunk).await
}

/// Runs `job` to its end, its limit or `stop`. The container outlives its `docker run`
/// client, so a stop or a timeout kills the container by name, then the client.
pub async fn run_container(
    exec: &dyn Exec,
    docker: &Docker,
    job: &ContainerJob,
    mounts: &RunMounts,
    limit: Duration,
    stop: &StopFlag,
    chunk: Chunk<'_>,
) -> io::Result<Ended> {
    if let Some(problem) = mount_problem(mounts).or_else(|| git_file_problem(&mounts.worktree)) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, problem));
    }
    if stop.fired() {
        return Ok(Ended::Stopped);
    }
    create_caches(exec, docker, mounts, &job.labels).await?;
    let cmd = docker.command(run_args(job, mounts));
    let client_stop = StopFlag::new();
    let client = exec.stream(&cmd, limit + CLIENT_SLACK, &client_stop, chunk);
    tokio::pin!(client);
    let why = tokio::select! {
        ended = &mut client => return ended,
        _ = stop.notified() => Ended::Stopped,
        _ = tokio::time::sleep(limit) => Ended::TimedOut,
    };
    // After a kill that worked the client ends on its own; after a failed one there is
    // nothing to wait for. A client still alive gets a second kill and is stopped at once.
    let grace = if kill(exec, docker, &job.name).await {
        CLIENT_GRACE
    } else {
        Duration::ZERO
    };
    if tokio::time::timeout(grace, &mut client).await.is_err() {
        kill(exec, docker, &job.name).await;
        client_stop.fire();
        let _ = client.await;
    }
    Ok(why)
}

/// Whether `docker kill <name>` reported success.
async fn kill(exec: &dyn Exec, docker: &Docker, name: &str) -> bool {
    let out = exec
        .output(&docker.command(kill_args(name)), KILL_TIMEOUT)
        .await;
    out.is_ok_and(|out| out.code == Some(0) && !out.timed_out)
}

/// Creates each cache volume with its labels: a volume `docker run -v` creates on the fly has
/// none, and cleanup finds volumes by label only.
async fn create_caches(
    exec: &dyn Exec,
    docker: &Docker,
    mounts: &RunMounts,
    labels: &Labels,
) -> io::Result<()> {
    let cache = Labels {
        kind: LabelKind::Cache,
        ..labels.clone()
    };
    for (volume, _) in &mounts.caches {
        let mut args = vec!["volume".to_string(), "create".to_string()];
        args.extend(cache.args());
        args.push(volume.clone());
        let out = exec.output(&docker.command(args), QUICK_TIMEOUT).await?;
        if out.code != Some(0) || out.timed_out {
            return Err(io::Error::other(format!(
                "docker volume create failed: {}",
                first_line(&out)
            )));
        }
    }
    Ok(())
}

pub struct Marker<'a> {
    pub worktree: &'a Path,
    pub step: &'a str,
    pub text: &'a str,
}

/// A work step: the run, then its marker, written by Clúsia after an exit 0 only. The step
/// cannot write it: `.clusia` is read-only in the container.
#[allow(clippy::too_many_arguments)]
pub async fn run_work_step(
    exec: &dyn Exec,
    docker: &Docker,
    job: &ContainerJob,
    mounts: &RunMounts,
    marker: Marker<'_>,
    limit: Duration,
    stop: &StopFlag,
    chunk: Chunk<'_>,
) -> io::Result<Ended> {
    let ended = run_container(exec, docker, job, mounts, limit, stop, chunk).await?;
    if ended == Ended::Exited(Some(0)) {
        write_marker(marker.worktree, marker.step, marker.text)?;
    }
    Ok(ended)
}

fn state_dir(worktree: &Path) -> PathBuf {
    worktree.join(".clusia/state")
}

pub fn marker_path(worktree: &Path, step: &str) -> PathBuf {
    state_dir(worktree).join(format!("{step}.done"))
}

/// Writes the marker atomically: a `.tmp` sibling, then a rename.
pub fn write_marker(worktree: &Path, step: &str, text: &str) -> io::Result<()> {
    let dir = state_dir(worktree);
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".{step}.done.tmp"));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, marker_path(worktree, step))
}

pub fn marker_matches(worktree: &Path, step: &str, text: &str) -> bool {
    std::fs::read_to_string(marker_path(worktree, step)).is_ok_and(|found| found == text)
}

/// Whether the engine sees the worktree and the git dir: a file Clúsia writes must show up
/// in the container, and the git dir must be there. It runs no recipe text.
pub async fn mount_check(
    exec: &dyn Exec,
    docker: &Docker,
    engine: &Engine,
    image: &str,
    mounts: &RunMounts,
    labels: &Labels,
    name: &str,
) -> Result<(), String> {
    if let Some(problem) = mount_problem(mounts).or_else(|| git_file_problem(&mounts.worktree)) {
        return Err(problem);
    }
    let seed = format!("{:?} {}", SystemTime::now(), std::process::id());
    let probe = format!("mount-{}", &sha256_hex(seed.as_bytes())[..16]);
    let dir = state_dir(&mounts.worktree);
    let file = dir.join(&probe);
    std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&file, ""))
        .map_err(|e| format!("The mount check could not write {}: {e}", file.display()))?;

    let git = mounts
        .git_common
        .display()
        .to_string()
        .replace('\'', r"'\''");
    let job = ContainerJob {
        name: name.to_string(),
        image: image.to_string(),
        command: format!(
            "test -f /work/.clusia/state/{probe} || exit 3; test -d '{git}' || exit 4"
        ),
        env: vec![],
        net: ContainerNet::None,
        cpus: None,
        memory_mb: None,
        labels: Labels {
            kind: LabelKind::Probe,
            ..labels.clone()
        },
    };
    let probe_mounts = RunMounts {
        caches: vec![],
        ..mounts.clone()
    };
    let out = exec
        .output(
            &docker.command(run_args(&job, &probe_mounts)),
            MOUNT_TIMEOUT,
        )
        .await;
    let _ = std::fs::remove_file(&file);

    let out = out.map_err(|e| format!("The mount check failed: {e}"))?;
    let unseen = |path: &Path| {
        format!(
            "{} can't see {}. Share that folder with the engine (Colima: `mounts` in colima.yaml) and try again.",
            engine_name(engine.kind),
            path.display()
        )
    };
    let why = if out.timed_out {
        kill(exec, docker, name).await;
        "timed out".to_string()
    } else {
        match out.code {
            Some(0) => return Ok(()),
            Some(3) => return Err(unseen(&mounts.worktree)),
            Some(4) => return Err(unseen(&mounts.git_common)),
            Some(code) => format!("exit {code}"),
            None => "killed by a signal".to_string(),
        }
    };
    let line = first_line(&out);
    Err(if line.is_empty() {
        format!("The mount check failed ({why})")
    } else {
        format!("The mount check failed ({why}): {line}")
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stack {
    pub project: String,
    pub worktree: PathBuf,
    /// Relative to the worktree.
    pub compose_file: String,
    pub override_file: PathBuf,
    pub override_text: String,
    pub services: Vec<String>,
}

pub async fn compose_version(exec: &dyn Exec, docker: &Docker) -> Result<String, String> {
    let out = compose_output(exec, docker, ["compose", "version", "--short"], "version").await?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub async fn compose_config(
    exec: &dyn Exec,
    docker: &Docker,
    worktree: &Path,
    compose_file: &str,
    project: &str,
) -> Result<serde_json::Value, String> {
    let mut args = vec![
        "compose".to_string(),
        "-p".into(),
        project.to_string(),
        "--project-directory".into(),
        worktree.display().to_string(),
        "-f".into(),
        worktree.join(compose_file).display().to_string(),
    ];
    args.extend(["config", "--format", "json"].map(String::from));
    let out = compose_output(exec, docker, args, "config").await?;
    serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("docker compose config gave JSON Clúsia can't read: {e}"))
}

/// The `services` step: pull what is not built, then build the rest. A failed pull stops
/// there.
pub async fn stack_services(
    exec: &dyn Exec,
    docker: &Docker,
    stack: &Stack,
    limit: Duration,
    stop: &StopFlag,
    chunk: Chunk<'_>,
) -> io::Result<Ended> {
    write_override(stack)?;
    let pull = stack_command(docker, stack, &["pull", "--ignore-buildable"]);
    let ended = exec.stream(&pull, limit, stop, &mut *chunk).await?;
    if ended != Ended::Exited(Some(0)) {
        return Ok(ended);
    }
    let build = stack_command(docker, stack, &["build"]);
    exec.stream(&build, limit, stop, chunk).await
}

/// The `stack` step. The services' network has no internet, so nothing is pulled or built
/// here.
pub async fn stack_up(
    exec: &dyn Exec,
    docker: &Docker,
    stack: &Stack,
    limit: Duration,
    stop: &StopFlag,
    chunk: Chunk<'_>,
) -> io::Result<Ended> {
    write_override(stack)?;
    let up = stack_command(
        docker,
        stack,
        &["up", "-d", "--wait", "--pull", "never", "--no-build"],
    );
    exec.stream(&up, limit, stop, chunk).await
}

/// Takes the stack down by project name alone, so it works from a stored record.
pub async fn stack_down(exec: &dyn Exec, docker: &Docker, project: &str) -> Result<(), String> {
    let args = ["compose", "-p", project, "down", "-v", "--remove-orphans"];
    compose_output(exec, docker, args, "down").await.map(|_| ())
}

fn stack_command(docker: &Docker, stack: &Stack, verb: &[&str]) -> CommandSpec {
    let mut args = vec![
        "compose".to_string(),
        "-p".into(),
        stack.project.clone(),
        "--project-directory".into(),
        stack.worktree.display().to_string(),
        "-f".into(),
        stack
            .worktree
            .join(&stack.compose_file)
            .display()
            .to_string(),
        "-f".into(),
        stack.override_file.display().to_string(),
    ];
    args.extend(verb.iter().map(|s| s.to_string()));
    args.extend(stack.services.iter().cloned());
    docker.command(args)
}

/// Writes the override atomically, right before the call that reads it.
fn write_override(stack: &Stack) -> io::Result<()> {
    if let Some(parent) = stack.override_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = stack.override_file.clone().into_os_string();
    tmp.push(".tmp");
    std::fs::write(&tmp, &stack.override_text)?;
    std::fs::rename(&tmp, &stack.override_file)
}

async fn compose_output(
    exec: &dyn Exec,
    docker: &Docker,
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
    what: &str,
) -> Result<Output, String> {
    let out = exec
        .output(&docker.command(args), COMPOSE_TIMEOUT)
        .await
        .map_err(|e| format!("docker compose {what} failed: {e}"))?;
    if out.timed_out {
        return Err(format!("docker compose {what} failed: it timed out"));
    }
    if out.code != Some(0) {
        return Err(format!(
            "docker compose {what} failed: {}",
            first_line(&out)
        ));
    }
    Ok(out)
}

/// The first non-empty line of stderr, safe to show.
fn first_line(out: &Output) -> String {
    let text = String::from_utf8_lossy(&out.stderr);
    printable(
        text.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or(""),
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use crate::docker::ContainerNet;
    use crate::names::LabelKind;
    use crate::testkit::{FakeExec, FakeReply};

    use super::*;

    fn docker() -> Docker {
        Docker {
            cli: "/opt/homebrew/bin/docker".into(),
            env: vec![(
                "PATH".into(),
                "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin".into(),
            )],
        }
    }

    fn labels(kind: LabelKind) -> Labels {
        Labels {
            review: "acme~widgets~7".into(),
            repo: "acme/widgets".into(),
            kind,
        }
    }

    fn args(cmd: &CommandSpec) -> Vec<String> {
        cmd.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    struct Tree {
        _dir: tempfile::TempDir,
        top: PathBuf,
        mounts: RunMounts,
    }

    fn tree() -> Tree {
        let dir = tempfile::tempdir().unwrap();
        let top = dir.path().canonicalize().unwrap();
        let mounts = RunMounts {
            worktree: top.join("wt"),
            git_common: top.join("git"),
            caches: vec![(
                "clusia-cache-acme-widgets-abc123-cargo".into(),
                "/cache/cargo".into(),
            )],
        };
        std::fs::create_dir_all(mounts.worktree.join(".clusia/state")).unwrap();
        std::fs::create_dir_all(&mounts.git_common).unwrap();
        let gitdir = format!("gitdir: {}/worktrees/wt\n", mounts.git_common.display());
        std::fs::write(mounts.worktree.join(".git"), gitdir).unwrap();
        Tree {
            _dir: dir,
            top,
            mounts,
        }
    }

    fn job(name: &str) -> ContainerJob {
        ContainerJob {
            name: name.into(),
            image: "clusia/acme-widgets-abc123:0123456789abcdef".into(),
            command: "cargo test".into(),
            env: vec![],
            net: ContainerNet::None,
            cpus: None,
            memory_mb: None,
            labels: labels(LabelKind::Run),
        }
    }

    fn colima() -> Engine {
        Engine {
            kind: EngineKind::Colima,
            cli: "/opt/homebrew/bin/docker".into(),
            endpoint: "unix:///Users/octo/.colima/default/docker.sock".into(),
            context: Some("colima".into()),
            running: true,
            start: Some("colima start".into()),
            compose: Some("2.29.1".into()),
        }
    }

    #[test]
    fn engines_have_names_for_messages() {
        assert_eq!(
            [
                EngineKind::Colima,
                EngineKind::OrbStack,
                EngineKind::DockerDesktop,
                EngineKind::Podman,
                EngineKind::Other
            ]
            .map(engine_name),
            [
                "Colima",
                "OrbStack",
                "Docker Desktop",
                "Podman",
                "The container engine"
            ]
        );
    }

    #[tokio::test]
    async fn image_present_asks_the_engine_for_the_tag() {
        let fake = FakeExec::new();
        fake.on(
            "docker",
            &["image", "inspect"],
            FakeReply::ok("sha256:abc\n"),
        );
        assert!(image_present(&fake, &docker(), "clusia/x:0123").await);
        assert_eq!(
            args(&fake.calls()[0]),
            strings(&["image", "inspect", "--format", "{{.Id}}", "clusia/x:0123"])
        );
        let gone = FakeExec::new();
        gone.on(
            "docker",
            &["image", "inspect"],
            FakeReply::fail(1, "No such image"),
        );
        assert!(!image_present(&gone, &docker(), "clusia/x:0123").await);
    }

    #[tokio::test]
    async fn build_image_builds_the_base_then_the_generated_file_alone() {
        let t = tree();
        let build_dir = t.top.join("tmp/image");
        std::fs::create_dir_all(&build_dir).unwrap();
        std::fs::write(build_dir.join("planted.txt"), "x").unwrap();
        let plan = ImagePlan {
            base_build: Some(strings(&[
                "build",
                "-f",
                "/wt/Dockerfile",
                "-t",
                "clusia/x-base:1",
                "/wt",
            ])),
            dockerfile: "FROM clusia/x-base:1\nRUN [\"/bin/sh\",\"-c\",\"true\"]\n".into(),
            tag: "clusia/x:2".into(),
            hash: "2".repeat(64),
            notes: vec![],
            network: false,
        };
        let fake = FakeExec::new();
        fake.on("docker", &["build"], FakeReply::ok("built\n"));
        let stop = StopFlag::new();
        let mut chunk = |_: Stream, _: &[u8]| {};
        let ended = build_image(
            &fake,
            &docker(),
            &plan,
            &build_dir,
            &labels(LabelKind::Run),
            Duration::from_secs(60),
            &stop,
            &mut chunk,
        )
        .await
        .unwrap();
        assert_eq!(ended, Ended::Exited(Some(0)));
        let calls = fake.calls();
        assert_eq!(args(&calls[0]), plan.base_build.clone().unwrap());
        let file = build_dir.join("Dockerfile");
        assert_eq!(
            args(&calls[1]),
            [
                strings(&[
                    "build",
                    "--network",
                    "none",
                    "--label",
                    "io.clusia.review=acme~widgets~7",
                    "--label",
                    "io.clusia.repo=acme/widgets",
                    "--label",
                    "io.clusia.kind=image",
                    "-t",
                    "clusia/x:2",
                    "-f"
                ]),
                vec![file.display().to_string(), build_dir.display().to_string()],
            ]
            .concat()
        );
        let mut left: Vec<_> = std::fs::read_dir(&build_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["Dockerfile"],
            "the context holds the generated file only"
        );
        assert_eq!(std::fs::read_to_string(file).unwrap(), plan.dockerfile);

        let failing = FakeExec::new();
        failing.on(
            "docker",
            &["build"],
            FakeReply::fail(1, "pull access denied"),
        );
        let ended = build_image(
            &failing,
            &docker(),
            &plan,
            &build_dir,
            &labels(LabelKind::Run),
            Duration::from_secs(60),
            &stop,
            &mut chunk,
        )
        .await
        .unwrap();
        assert_eq!(ended, Ended::Exited(Some(1)));
        assert_eq!(failing.calls().len(), 1, "a failed base stops the build");
    }

    #[tokio::test]
    async fn a_run_creates_labelled_caches_then_runs() {
        let t = tree();
        let fake = FakeExec::new();
        fake.on(
            "docker",
            &["volume", "create"],
            FakeReply::ok("clusia-cache-acme-widgets-abc123-cargo\n"),
        )
        .on(
            "docker",
            &["run"],
            FakeReply::chunks(vec![(Stream::Out, "test a ... FAILED\n".into())], 101),
        );
        let stop = StopFlag::new();
        let mut seen = String::new();
        let mut chunk = |_: Stream, b: &[u8]| seen.push_str(&String::from_utf8_lossy(b));
        let job = job("clusia-run-acme-widgets-abc123-7-1");
        let ended = run_container(
            &fake,
            &docker(),
            &job,
            &t.mounts,
            Duration::from_secs(60),
            &stop,
            &mut chunk,
        )
        .await
        .unwrap();
        assert_eq!(ended, Ended::Exited(Some(101)));
        assert_eq!(seen, "test a ... FAILED\n");
        let calls = fake.calls();
        assert_eq!(
            args(&calls[0]),
            strings(&[
                "volume",
                "create",
                "--label",
                "io.clusia.review=acme~widgets~7",
                "--label",
                "io.clusia.repo=acme/widgets",
                "--label",
                "io.clusia.kind=cache",
                "clusia-cache-acme-widgets-abc123-cargo",
            ])
        );
        assert_eq!(args(&calls[1]), run_args(&job, &t.mounts));
        assert_eq!(calls.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn stop_kills_the_container_not_only_the_client() {
        let t = tree();
        let fake = FakeExec::new();
        fake.on("docker", &["volume", "create"], FakeReply::ok(""))
            .on(
                "docker",
                &["kill"],
                FakeReply::ok("clusia-run-acme-widgets-abc123-7-2\n"),
            )
            .on("docker", &["run"], FakeReply::hang());
        let stop = StopFlag::new();
        let flag = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            flag.fire();
        });
        let mut chunk = |_: Stream, _: &[u8]| {};
        let job = job("clusia-run-acme-widgets-abc123-7-2");
        let started = tokio::time::Instant::now();
        let ended = run_container(
            &fake,
            &docker(),
            &job,
            &t.mounts,
            Duration::from_secs(600),
            &stop,
            &mut chunk,
        )
        .await
        .unwrap();
        assert_eq!(ended, Ended::Stopped);
        let calls: Vec<Vec<String>> = fake.calls().iter().map(args).collect();
        let kills: Vec<&Vec<String>> = calls.iter().filter(|c| c[0] == "kill").collect();
        assert_eq!(
            kills,
            [&strings(&["kill", "clusia-run-acme-widgets-abc123-7-2"]); 2]
        );
        // The client outlived a kill that worked: one grace, then a second kill and the client's
        // own stop at once.
        assert_eq!(started.elapsed(), Duration::from_secs(1) + CLIENT_GRACE);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_kill_is_sent_again_at_once() {
        let t = tree();
        let fake = FakeExec::new();
        fake.on("docker", &["volume", "create"], FakeReply::ok(""))
            .on_sequence(
                "docker",
                &["kill"],
                vec![
                    FakeReply::fail(1, "Error response from daemon: cannot kill container"),
                    FakeReply::ok(""),
                ],
            )
            .on("docker", &["run"], FakeReply::hang());
        let mut chunk = |_: Stream, _: &[u8]| {};
        let job = job("clusia-run-acme-widgets-abc123-7-4");
        let stop = StopFlag::new();
        let flag = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            flag.fire();
        });
        let started = tokio::time::Instant::now();
        let ended = run_container(
            &fake,
            &docker(),
            &job,
            &t.mounts,
            Duration::from_secs(600),
            &stop,
            &mut chunk,
        )
        .await
        .unwrap();
        assert_eq!(ended, Ended::Stopped);
        let kills = fake.calls().iter().filter(|c| args(c)[0] == "kill").count();
        assert_eq!(kills, 2);
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(1),
            "no grace after the failed kill, none after the second"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_limit_kills_the_container_too() {
        let t = tree();
        let fake = FakeExec::new();
        fake.on("docker", &["volume", "create"], FakeReply::ok(""))
            .on("docker", &["kill"], FakeReply::ok(""))
            .on("docker", &["run"], FakeReply::hang());
        let stop = StopFlag::new();
        let mut chunk = |_: Stream, _: &[u8]| {};
        let job = job("clusia-run-acme-widgets-abc123-7-3");
        let ended = run_container(
            &fake,
            &docker(),
            &job,
            &t.mounts,
            Duration::from_secs(5),
            &stop,
            &mut chunk,
        )
        .await
        .unwrap();
        assert_eq!(ended, Ended::TimedOut);
        assert!(
            fake.calls()
                .iter()
                .any(|c| args(c) == strings(&["kill", "clusia-run-acme-widgets-abc123-7-3"]))
        );
    }

    #[tokio::test]
    async fn a_mount_docker_would_split_runs_nothing() {
        let t = tree();
        let fake = FakeExec::new();
        let stop = StopFlag::new();
        let mut chunk = |_: Stream, _: &[u8]| {};
        let bad = RunMounts {
            worktree: t.top.join("a:b"),
            ..t.mounts.clone()
        };
        let err = run_container(
            &fake,
            &docker(),
            &job("n"),
            &bad,
            Duration::from_secs(5),
            &stop,
            &mut chunk,
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        std::fs::remove_file(t.mounts.worktree.join(".git")).unwrap();
        std::fs::create_dir(t.mounts.worktree.join(".git")).unwrap();
        let err = run_container(
            &fake,
            &docker(),
            &job("n"),
            &t.mounts,
            Duration::from_secs(5),
            &stop,
            &mut chunk,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "Clúsia can't mount {}/.git read-only: it is not a plain file.",
                t.mounts.worktree.display()
            )
        );
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn a_work_step_marker_is_written_by_clusia_only_after_success() {
        let t = tree();
        let wt = t.mounts.worktree.clone();
        let ok = FakeExec::new();
        ok.on("docker", &["volume", "create"], FakeReply::ok(""))
            .on("docker", &["run"], FakeReply::ok("fetched\n"));
        let stop = StopFlag::new();
        let mut chunk = |_: Stream, _: &[u8]| {};
        let marker = Marker {
            worktree: &wt,
            step: "work:0",
            text: "work:0 cargo fetch image=clusia/x:2 inputs=ab",
        };
        let ended = run_work_step(
            &ok,
            &docker(),
            &job("w0"),
            &t.mounts,
            marker,
            Duration::from_secs(60),
            &stop,
            &mut chunk,
        )
        .await
        .unwrap();
        assert_eq!(ended, Ended::Exited(Some(0)));
        assert_eq!(
            marker_path(&wt, "work:0"),
            wt.join(".clusia/state/work:0.done")
        );
        assert!(marker_matches(
            &wt,
            "work:0",
            "work:0 cargo fetch image=clusia/x:2 inputs=ab"
        ));
        assert!(!marker_matches(
            &wt,
            "work:0",
            "work:0 cargo fetch image=clusia/x:3 inputs=ab"
        ));

        let failed = FakeExec::new();
        failed
            .on("docker", &["volume", "create"], FakeReply::ok(""))
            .on(
                "docker",
                &["run"],
                FakeReply::fail(1, "error: failed to fetch"),
            );
        let marker = Marker {
            worktree: &wt,
            step: "work:1",
            text: "w1",
        };
        let ended = run_work_step(
            &failed,
            &docker(),
            &job("w1"),
            &t.mounts,
            marker,
            Duration::from_secs(60),
            &stop,
            &mut chunk,
        )
        .await
        .unwrap();
        assert_eq!(ended, Ended::Exited(Some(1)));
        assert!(!marker_path(&wt, "work:1").exists());
        write_marker(&wt, "work:1", "by hand").unwrap();
        assert!(marker_matches(&wt, "work:1", "by hand"));
    }

    #[tokio::test]
    async fn the_mount_check_probes_both_paths_and_cleans_up() {
        let t = tree();
        let fake = FakeExec::new();
        fake.on("docker", &["run"], FakeReply::ok(""));
        let result = mount_check(
            &fake,
            &docker(),
            &colima(),
            "clusia/x:2",
            &t.mounts,
            &labels(LabelKind::Run),
            "clusia-probe-acme-widgets-abc123-7",
        )
        .await;
        assert_eq!(result, Ok(()));
        let call = args(&fake.calls()[0]);
        let command = call.last().unwrap().clone();
        let probe = command
            .strip_prefix("test -f /work/.clusia/state/")
            .and_then(|rest| rest.split(' ').next())
            .unwrap()
            .to_string();
        assert!(
            probe.starts_with("mount-") && probe.len() == "mount-".len() + 16,
            "{probe}"
        );
        assert_eq!(
            command,
            format!(
                "test -f /work/.clusia/state/{probe} || exit 3; test -d '{}' || exit 4",
                t.mounts.git_common.display()
            )
        );
        let probe_job = ContainerJob {
            name: "clusia-probe-acme-widgets-abc123-7".into(),
            image: "clusia/x:2".into(),
            command,
            env: vec![],
            net: ContainerNet::None,
            cpus: None,
            memory_mb: None,
            labels: labels(LabelKind::Probe),
        };
        assert_eq!(
            call,
            run_args(
                &probe_job,
                &RunMounts {
                    caches: vec![],
                    ..t.mounts.clone()
                }
            )
        );
        assert_eq!(
            std::fs::read_dir(t.mounts.worktree.join(".clusia/state"))
                .unwrap()
                .count(),
            0,
            "the probe file is gone"
        );
    }

    #[tokio::test]
    async fn a_mount_the_engine_cannot_see_names_the_path() {
        let t = tree();
        for (code, path) in [
            (3, t.mounts.worktree.clone()),
            (4, t.mounts.git_common.clone()),
        ] {
            let fake = FakeExec::new();
            fake.on("docker", &["run"], FakeReply::fail(code, ""));
            assert_eq!(
                mount_check(
                    &fake,
                    &docker(),
                    &colima(),
                    "clusia/x:2",
                    &t.mounts,
                    &labels(LabelKind::Run),
                    "p"
                )
                .await,
                Err(format!(
                    "Colima can't see {}. Share that folder with the engine (Colima: `mounts` in colima.yaml) and try again.",
                    path.display()
                ))
            );
        }
        let fake = FakeExec::new();
        fake.on(
            "docker",
            &["run"],
            FakeReply::fail(
                125,
                "docker: Error response from daemon: invalid mount config\n",
            ),
        );
        assert_eq!(
            mount_check(&fake, &docker(), &colima(), "clusia/x:2", &t.mounts, &labels(LabelKind::Run), "p").await,
            Err("The mount check failed (exit 125): docker: Error response from daemon: invalid mount config".to_string())
        );
    }

    fn stack(t: &Tree) -> Stack {
        Stack {
            project: "clusia-acme-widgets-abc123-7".into(),
            worktree: t.mounts.worktree.clone(),
            compose_file: "docker-compose.yml".into(),
            override_file: t
                .top
                .join("profiles/clusia-acme-widgets-abc123-7.override.yml"),
            override_text: "services: {}\n".into(),
            services: vec!["postgres".into(), "redis".into()],
        }
    }

    #[tokio::test]
    async fn stack_steps_write_the_override_and_name_their_services() {
        let t = tree();
        let s = stack(&t);
        let fake = FakeExec::new();
        fake.on("docker", &["compose"], FakeReply::ok(""));
        let stop = StopFlag::new();
        let mut chunk = |_: Stream, _: &[u8]| {};
        let limit = Duration::from_secs(600);
        assert_eq!(
            stack_services(&fake, &docker(), &s, limit, &stop, &mut chunk)
                .await
                .unwrap(),
            Ended::Exited(Some(0))
        );
        assert_eq!(
            stack_up(&fake, &docker(), &s, limit, &stop, &mut chunk)
                .await
                .unwrap(),
            Ended::Exited(Some(0))
        );
        stack_down(&fake, &docker(), &s.project).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&s.override_file).unwrap(),
            "services: {}\n"
        );
        let wt = s.worktree.display().to_string();
        let prefix = vec![
            "compose".to_string(),
            "-p".into(),
            s.project.clone(),
            "--project-directory".into(),
            wt.clone(),
            "-f".into(),
            format!("{wt}/docker-compose.yml"),
            "-f".into(),
            s.override_file.display().to_string(),
        ];
        let calls: Vec<Vec<String>> = fake.calls().iter().map(args).collect();
        assert_eq!(
            calls,
            [
                [
                    prefix.clone(),
                    strings(&["pull", "--ignore-buildable", "postgres", "redis"])
                ]
                .concat(),
                [prefix.clone(), strings(&["build", "postgres", "redis"])].concat(),
                [
                    prefix,
                    strings(&[
                        "up",
                        "-d",
                        "--wait",
                        "--pull",
                        "never",
                        "--no-build",
                        "postgres",
                        "redis"
                    ])
                ]
                .concat(),
                strings(&[
                    "compose",
                    "-p",
                    "clusia-acme-widgets-abc123-7",
                    "down",
                    "-v",
                    "--remove-orphans"
                ]),
            ]
        );
    }

    #[tokio::test]
    async fn compose_version_and_config_are_read() {
        let t = tree();
        let fake = FakeExec::new();
        fake.on("docker", &["compose", "version"], FakeReply::ok("2.29.1\n"))
            .on(
                "docker",
                &["compose", "-p"],
                FakeReply::ok(r#"{"services": {"db": {"image": "postgres:16"}}}"#),
            );
        assert_eq!(
            compose_version(&fake, &docker()).await,
            Ok("2.29.1".to_string())
        );
        let merged = compose_config(
            &fake,
            &docker(),
            &t.mounts.worktree,
            "docker-compose.yml",
            "clusia-acme-widgets-abc123-7",
        )
        .await
        .unwrap();
        assert_eq!(merged["services"]["db"]["image"], "postgres:16");
        let wt = t.mounts.worktree.display().to_string();
        assert_eq!(
            args(&fake.calls()[1]),
            [
                strings(&[
                    "compose",
                    "-p",
                    "clusia-acme-widgets-abc123-7",
                    "--project-directory"
                ]),
                vec![wt.clone(), "-f".into(), format!("{wt}/docker-compose.yml")],
                strings(&["config", "--format", "json"]),
            ]
            .concat()
        );
        let broken = FakeExec::new();
        broken.on(
            "docker",
            &["compose"],
            FakeReply::fail(1, "yaml: line 3: did not find expected key\n"),
        );
        assert_eq!(
            compose_config(
                &broken,
                &docker(),
                &t.mounts.worktree,
                "docker-compose.yml",
                "p"
            )
            .await,
            Err(
                "docker compose config failed: yaml: line 3: did not find expected key".to_string()
            )
        );
    }
}
