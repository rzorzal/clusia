//! The container runner against a real engine. Runs only with `CLUSIA_TEST_DOCKER=1`; pulling
//! `alpine:3.20` also needs `CLUSIA_TEST_NETWORK=1`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clusia_core::cockpit::RunSide;
use clusia_core::tests::{Ecosystem, ResultsFormat, RunnerChoice};
use clusia_testrun::cleanup;
use clusia_testrun::container::{self, Marker};
use clusia_testrun::docker::{ContainerJob, ContainerNet, Docker, RunMounts};
use clusia_testrun::engine::{self, HostEnv};
use clusia_testrun::exec::{Ended, Exec, StopFlag, Stream, SystemExec};
use clusia_testrun::image;
use clusia_testrun::names::{self, LabelKind, Labels};
use clusia_testrun::recipe::{ImageSource, Recipe, SetupStep, StepInto, TestsSpec};
use clusia_testrun::system::{self, SystemJob};

const REPO: &str = "rzorzal/clusia-docker-test";
const LIMIT: Duration = Duration::from_secs(600);

fn labels(kind: LabelKind) -> Labels {
    Labels {
        review: "rzorzal~clusia-docker-test~1".into(),
        repo: REPO.into(),
        kind,
    }
}

async fn listed(exec: &SystemExec, docker: &Docker, args: &[&str]) -> String {
    let out = exec
        .output(&docker.command(args), Duration::from_secs(30))
        .await
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
async fn a_real_engine_builds_runs_stops_and_cleans_up() {
    if std::env::var("CLUSIA_TEST_DOCKER").as_deref() != Ok("1") {
        eprintln!("skipped: set CLUSIA_TEST_DOCKER=1 to use the real container engine");
        return;
    }
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let host = HostEnv {
        path: std::env::var_os("PATH"),
        home: home.clone(),
        docker_override: None,
        docker_host: std::env::var("DOCKER_HOST").ok(),
    };
    let exec = SystemExec;
    let engine = engine::detect(&exec, &host)
        .await
        .expect("CLUSIA_TEST_DOCKER=1 needs a container engine");
    assert!(engine.running, "start the engine first");
    let docker = Docker {
        cli: PathBuf::from(&engine.cli),
        env: engine::cli_env(Path::new(&engine.cli), &host),
    };
    if !container::image_present(&exec, &docker, "alpine:3.20").await {
        if std::env::var("CLUSIA_TEST_NETWORK").as_deref() != Ok("1") {
            eprintln!(
                "skipped: alpine:3.20 is not on the engine; set CLUSIA_TEST_NETWORK=1 to pull it"
            );
            return;
        }
        // A build pulls `FROM` into the build cache only, with no image record, so the base
        // is pulled here: the last check must find it whatever the build did.
        let pulled = exec
            .output(&docker.command(["pull", "alpine:3.20"]), LIMIT)
            .await
            .unwrap();
        assert_eq!(
            pulled.code,
            Some(0),
            "{}",
            String::from_utf8_lossy(&pulled.stderr)
        );
    }
    assert!(
        container::image_present(&exec, &docker, "alpine:3.20").await,
        "the base is in the image store before the build"
    );

    // Under the home folder, which every engine shares by default (Colima shares only ~).
    let dir = tempfile::Builder::new()
        .prefix(".clusia-docker-test-")
        .tempdir_in(&home)
        .unwrap();
    let top = dir.path().canonicalize().unwrap();
    let worktree = top.join("wt");
    let git = top.join("git");
    std::fs::create_dir_all(worktree.join(".clusia/state")).unwrap();
    std::fs::create_dir_all(&git).unwrap();
    std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", git.display()),
    )
    .unwrap();
    let slug = names::repo_slug("rzorzal", "clusia-docker-test");

    let recipe = Recipe {
        functions: None,
        runner: RunnerChoice::Container,
        image: Some(ImageSource::Base {
            image: "alpine:3.20".into(),
        }),
        setup: vec![SetupStep {
            name: "Mark the image".into(),
            run: "echo built > /built".into(),
            into: StepInto::Image,
            network: false,
        }],
        services: None,
        tests: TestsSpec {
            all: "true".into(),
            one: None,
            some: None,
            list: None,
            results: ResultsFormat::Plain,
            env: BTreeMap::new(),
        },
        system: None,
    };
    let plan = image::plan_image(
        &recipe,
        &worktree,
        &slug,
        &labels(LabelKind::Image),
        &"0".repeat(64),
    )
    .unwrap();
    let stop = StopFlag::new();
    let mut log = String::new();
    let mut chunk = |_: Stream, b: &[u8]| log.push_str(&String::from_utf8_lossy(b));
    let ended = container::build_image(
        &exec,
        &docker,
        &plan,
        &top.join("image"),
        &labels(LabelKind::Image),
        LIMIT,
        &stop,
        &mut chunk,
    )
    .await
    .unwrap();
    assert_eq!(ended, Ended::Exited(Some(0)), "{log}");
    assert!(container::image_present(&exec, &docker, &plan.tag).await);

    let mounts = RunMounts {
        worktree: worktree.clone(),
        git_common: git.clone(),
        scenario: None,
        caches: vec![(
            names::cache_volume(&slug, Ecosystem::Cargo),
            "/cache/cargo".into(),
        )],
    };
    let probe = names::run_name(&slug, 1, 0);
    assert_eq!(
        container::mount_check(
            &exec,
            &docker,
            &engine,
            &plan.tag,
            &mounts,
            &labels(LabelKind::Probe),
            &probe
        )
        .await,
        Ok(())
    );

    let job = ContainerJob {
        name: names::run_name(&slug, 1, 1),
        image: plan.tag.clone(),
        command: "cat /built && echo out > /work/out.txt && test -d /cache/cargo \
                  && ! touch /work/.clusia/x 2>/dev/null \
                  && ! sh -c 'echo x > /work/.git' 2>/dev/null \
                  && ! wget -q -T 3 -O /dev/null http://example.com 2>/dev/null \
                  && test -z \"$SSH_AUTH_SOCK$GITHUB_TOKEN$DOCKER_HOST\""
            .into(),
        env: vec![],
        net: ContainerNet::None,
        cpus: Some(1),
        memory_mb: Some(512),
        labels: labels(LabelKind::Run),
    };
    let mut out = String::new();
    let mut chunk = |_: Stream, b: &[u8]| out.push_str(&String::from_utf8_lossy(b));
    let ended = container::run_container(&exec, &docker, &job, &mounts, LIMIT, &stop, &mut chunk)
        .await
        .unwrap();
    assert_eq!(ended, Ended::Exited(Some(0)), "{out}");
    assert!(out.contains("built"), "{out}");
    assert_eq!(
        std::fs::read_to_string(worktree.join("out.txt")).unwrap(),
        "out\n"
    );
    assert!(!worktree.join(".clusia/x").exists());

    let marker = Marker {
        worktree: &worktree,
        step: "work:0",
        text: "w0",
    };
    let mut quiet = |_: Stream, _: &[u8]| {};
    let mut work = job.clone();
    work.name = names::run_name(&slug, 1, 2);
    work.command = "true".into();
    let ended = container::run_work_step(
        &exec, &docker, &work, &mounts, marker, LIMIT, &stop, &mut quiet,
    )
    .await
    .unwrap();
    assert_eq!(ended, Ended::Exited(Some(0)));
    assert!(container::marker_matches(&worktree, "work:0", "w0"));

    let mut long = job.clone();
    long.name = names::run_name(&slug, 1, 3);
    long.command = "sleep 600".into();
    let stopping = StopFlag::new();
    let flag = stopping.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        flag.fire();
    });
    let ended =
        container::run_container(&exec, &docker, &long, &mounts, LIMIT, &stopping, &mut quiet)
            .await
            .unwrap();
    assert_eq!(ended, Ended::Stopped);
    let left = listed(
        &exec,
        &docker,
        &["ps", "-aq", "--filter", &format!("name=^/{}$", long.name)],
    )
    .await;
    assert_eq!(left, "", "a stopped run leaves no container");

    assert!(
        container::image_present(&exec, &docker, "alpine:3.20").await,
        "the base is still there before the cleanup"
    );
    assert_eq!(cleanup::remove_repo(&exec, &docker, REPO).await, Ok(()));
    assert!(!container::image_present(&exec, &docker, &plan.tag).await);
    let filter = format!("label=io.clusia.repo={REPO}");
    assert_eq!(
        listed(&exec, &docker, &["volume", "ls", "-q", "--filter", &filter]).await,
        ""
    );
    assert!(
        container::image_present(&exec, &docker, "alpine:3.20").await,
        "base images stay"
    );
}

/// One HTTP/1.0 GET over a plain socket; the status line and the body, or why not.
fn get_root(port: u16) -> Result<(String, String), String> {
    use std::io::{Read, Write};
    let mut stream =
        std::net::TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(b"GET / HTTP/1.0\r\n\r\n")
        .map_err(|e| e.to_string())?;
    let mut text = String::new();
    stream
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    let (head, body) = text.split_once("\r\n\r\n").ok_or("no header block")?;
    Ok((
        head.lines().next().unwrap_or("").to_string(),
        body.to_string(),
    ))
}

async fn exec_status(
    exec: &SystemExec,
    docker: &Docker,
    container: &str,
    args: &[&str],
) -> Option<i32> {
    let mut all = vec!["exec", container];
    all.extend(args);
    exec.output(&docker.command(all), Duration::from_secs(30))
        .await
        .unwrap()
        .code
}

async fn start_all(
    exec: &SystemExec,
    docker: &Docker,
    job: &SystemJob,
    mounts: &RunMounts,
) -> std::collections::BTreeMap<u16, u16> {
    system::create_networks(exec, docker, &job.names, &job.labels)
        .await
        .unwrap();
    system::create_system(exec, docker, job, mounts)
        .await
        .unwrap();
    system::start_forwarder(exec, docker, job).await.unwrap();
    system::mapped_ports(exec, docker, &job.names, &job.ports)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_real_system_publishes_on_loopback_and_cannot_reach_out() {
    if std::env::var("CLUSIA_TEST_DOCKER").as_deref() != Ok("1") {
        eprintln!("skipped: set CLUSIA_TEST_DOCKER=1 to use the real container engine");
        return;
    }
    let online = std::env::var("CLUSIA_TEST_NETWORK").as_deref() == Ok("1");
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let host = HostEnv {
        path: std::env::var_os("PATH"),
        home: home.clone(),
        docker_override: None,
        docker_host: std::env::var("DOCKER_HOST").ok(),
    };
    let exec = SystemExec;
    let engine = engine::detect(&exec, &host)
        .await
        .expect("CLUSIA_TEST_DOCKER=1 needs a container engine");
    assert!(engine.running, "start the engine first");
    let docker = Docker {
        cli: PathBuf::from(&engine.cli),
        env: engine::cli_env(Path::new(&engine.cli), &host),
    };
    if !container::image_present(&exec, &docker, "alpine:3.20").await {
        if !online {
            eprintln!(
                "skipped: alpine:3.20 is not on the engine; set CLUSIA_TEST_NETWORK=1 to pull it"
            );
            return;
        }
        let pulled = exec
            .output(&docker.command(["pull", "alpine:3.20"]), LIMIT)
            .await
            .unwrap();
        assert_eq!(
            pulled.code,
            Some(0),
            "{}",
            String::from_utf8_lossy(&pulled.stderr)
        );
    }
    let forwarder = system::forwarder_tag();
    if !container::image_present(&exec, &docker, &forwarder).await {
        if !online {
            eprintln!(
                "skipped: the forwarder image is not on the engine; set CLUSIA_TEST_NETWORK=1 to build it"
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let stop = StopFlag::new();
        let mut log = String::new();
        let mut chunk = |_: Stream, b: &[u8]| log.push_str(&String::from_utf8_lossy(b));
        let ended = container::build_image(
            &exec,
            &docker,
            &system::forwarder_plan(),
            dir.path(),
            &labels(LabelKind::Image),
            LIMIT,
            &stop,
            &mut chunk,
        )
        .await
        .unwrap();
        assert_eq!(ended, Ended::Exited(Some(0)), "{log}");
    }

    let dir = tempfile::Builder::new()
        .prefix(".clusia-docker-test-")
        .tempdir_in(&home)
        .unwrap();
    let top = dir.path().canonicalize().unwrap();
    let worktree = top.join("wt");
    let git = top.join("git");
    std::fs::create_dir_all(worktree.join(".clusia")).unwrap();
    std::fs::create_dir_all(&git).unwrap();
    std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", git.display()),
    )
    .unwrap();
    let slug = names::repo_slug("rzorzal", "clusia-system-test");
    let mounts = RunMounts {
        worktree,
        git_common: git,
        scenario: None,
        caches: vec![],
    };

    let names = system::system_names(&slug, 1, RunSide::Pr, None);
    let job_for = |network: bool| {
        SystemJob {
        names: names.clone(),
        image: "alpine:3.20".into(),
        start: "mkdir -p /tmp/www && echo ok > /tmp/www/index.html && exec busybox httpd -f -p 80 -h /tmp/www".into(),
        env: vec![],
        ports: vec![80],
        network,
        cpus: None,
        memory_mb: None,
        labels: Labels {
            review: "rzorzal~clusia-system-test~1".into(),
            repo: "rzorzal/clusia-system-test".into(),
            kind: LabelKind::System,
        },
    }
    };
    // A left-over of an earlier failed run.
    let _ = system::remove_system(&exec, &docker, &names).await;

    let mapped = start_all(&exec, &docker, &job_for(false), &mounts).await;
    let host_port = mapped[&80];
    let mut answer = Err("never tried".to_string());
    for _ in 0..20 {
        answer = get_root(host_port);
        if answer.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let (status, body) = answer.expect("the system answers on its loopback port");
    assert!(status.contains("200"), "{status}");
    assert_eq!(body.trim(), "ok");
    let wget = ["wget", "-q", "-T", "3", "-O-", "http://1.1.1.1/"];
    assert_ne!(
        exec_status(&exec, &docker, &names.container, &wget).await,
        Some(0),
        "no outbound network"
    );
    assert_ne!(
        exec_status(
            &exec,
            &docker,
            &names.container,
            &["nslookup", "example.com"]
        )
        .await,
        Some(0),
        "no DNS out"
    );

    if online {
        system::remove_system(&exec, &docker, &names).await.unwrap();
        start_all(&exec, &docker, &job_for(true), &mounts).await;
        assert_eq!(
            exec_status(&exec, &docker, &names.container, &wget).await,
            Some(0),
            "outbound network when allowed"
        );
    }

    system::remove_system(&exec, &docker, &names).await.unwrap();
    let review = "label=io.clusia.review=rzorzal~clusia-system-test~1";
    for verb in [["ps", "-a"], ["network", "ls"]] {
        let mut args: Vec<&str> = verb.to_vec();
        args.extend([
            "--filter",
            "label=io.clusia.kind=system",
            "--filter",
            review,
            "--format",
            "{{.Names}}",
        ]);
        if verb[0] == "network" {
            args = vec![
                "network",
                "ls",
                "--filter",
                "label=io.clusia.kind=system",
                "--filter",
                review,
                "--format",
                "{{.Name}}",
            ];
        }
        assert_eq!(listed(&exec, &docker, &args).await, "", "{verb:?}");
    }
}
