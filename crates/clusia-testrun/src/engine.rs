//! Finds the container engine the reviewer has, without ever installing or starting one.
//!
//! The daemon runs under launchd with a thin `PATH`, so the CLI is searched in the usual places
//! too, and every child gets its own CLI's directory on `PATH` (a `docker` that finds its
//! credential helpers) and nothing else of the daemon's environment.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clusia_core::tests::{Engine, EngineKind};

use crate::exec::{CommandSpec, Exec, Output};

/// Where `docker` is looked for after the `PATH`; `~` is the home.
pub const SEARCH_DIRS: [&str; 5] = [
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "~/.orbstack/bin",
    "~/.docker/bin",
    "/Applications/Docker.app/Contents/Resources/bin",
];
const PODMAN_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];

pub const INFO_TIMEOUT: Duration = Duration::from_secs(2);

/// What detection needs from the daemon's own environment.
#[derive(Debug, Clone, Default)]
pub struct HostEnv {
    pub path: Option<OsString>,
    pub home: PathBuf,
    /// `CLUSIA_DOCKER_BIN`: the only candidate when set.
    pub docker_override: Option<PathBuf>,
    pub docker_host: Option<String>,
}

fn path_dirs(host: &HostEnv) -> Vec<PathBuf> {
    host.path
        .as_ref()
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default()
}

fn expand(dir: &str, home: &Path) -> PathBuf {
    match dir.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(dir),
    }
}

fn find_cli(exec: &dyn Exec, host: &HostEnv) -> Option<PathBuf> {
    if let Some(path) = &host.docker_override {
        return exec.executable(path).then(|| path.clone());
    }
    let on_path = path_dirs(host);
    let usual: Vec<PathBuf> = SEARCH_DIRS.iter().map(|d| expand(d, &host.home)).collect();
    let docker = on_path.iter().chain(&usual).map(|d| d.join("docker"));
    let podman_dirs: Vec<PathBuf> = PODMAN_DIRS.iter().map(PathBuf::from).collect();
    let podman = on_path.iter().chain(&podman_dirs).map(|d| d.join("podman"));
    docker.chain(podman).find(|p| exec.executable(p))
}

pub fn cli_env(cli: &Path, host: &HostEnv) -> Vec<(OsString, OsString)> {
    let dir = cli.parent().unwrap_or(Path::new("/usr/bin"));
    let mut path = dir.as_os_str().to_owned();
    path.push(":/usr/bin:/bin:/usr/sbin:/sbin");
    let mut env = vec![
        (OsString::from("PATH"), path),
        (OsString::from("HOME"), host.home.clone().into_os_string()),
    ];
    if let Some(docker_host) = &host.docker_host {
        env.push((OsString::from("DOCKER_HOST"), OsString::from(docker_host)));
    }
    env
}

fn command(cli: &Path, host: &HostEnv, args: &[&str]) -> CommandSpec {
    CommandSpec {
        env: cli_env(cli, host),
        clear_env: true,
        ..CommandSpec::new(cli).args(args)
    }
}

async fn run(exec: &dyn Exec, cli: &Path, host: &HostEnv, args: &[&str]) -> Option<Output> {
    let out = exec
        .output(&command(cli, host, args), INFO_TIMEOUT)
        .await
        .ok()?;
    (out.code == Some(0) && !out.timed_out).then_some(out)
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn is_podman(cli: &Path) -> bool {
    cli.file_name().is_some_and(|n| n == "podman")
}

fn info_args(kind: EngineKind) -> &'static [&'static str] {
    if kind == EngineKind::Podman {
        &["info"]
    } else {
        &["info", "--format", "{{.Name}}"]
    }
}

pub async fn detect(exec: &dyn Exec, host: &HostEnv) -> Option<Engine> {
    let cli = find_cli(exec, host)?;
    let (kind, endpoint, context) = if is_podman(&cli) {
        let socket = run(
            exec,
            &cli,
            host,
            &[
                "machine",
                "inspect",
                "--format",
                "{{.ConnectionInfo.PodmanSocket.Path}}",
            ],
        )
        .await
        .map(|o| text(&o))
        .filter(|s| !s.is_empty());
        let endpoint = socket.map(|s| format!("unix://{s}")).unwrap_or_default();
        (EngineKind::Podman, endpoint, None)
    } else {
        let (context, host_of_context) = read_context(exec, &cli, host).await;
        // `DOCKER_HOST` outranks the context for the endpoint and for the kind.
        let (endpoint, for_kind) = match &host.docker_host {
            Some(docker_host) => (docker_host.clone(), None),
            None => (host_of_context.unwrap_or_default(), context.as_deref()),
        };
        let kind = classify(&endpoint, for_kind);
        (kind, endpoint, context)
    };
    let running = run(exec, &cli, host, info_args(kind)).await.is_some();
    let compose = if kind == EngineKind::Podman {
        None
    } else {
        run(exec, &cli, host, &["compose", "version", "--short"])
            .await
            .map(|o| text(&o).trim_start_matches('v').to_string())
            .filter(|v| !v.is_empty())
    };
    Some(Engine {
        kind,
        cli: cli.to_string_lossy().into_owned(),
        start: start_command(kind, &endpoint),
        endpoint,
        context,
        running,
        compose,
    })
}

/// The current context's name and docker endpoint, from `docker context inspect`.
async fn read_context(
    exec: &dyn Exec,
    cli: &Path,
    host: &HostEnv,
) -> (Option<String>, Option<String>) {
    let Some(out) = run(exec, cli, host, &["context", "inspect"]).await else {
        return (None, None);
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return (None, None);
    };
    let first = &json[0];
    let name = first["Name"].as_str().map(str::to_string);
    let endpoint = first["Endpoints"]["docker"]["Host"]
        .as_str()
        .map(str::to_string);
    (name, endpoint)
}

/// Whether the engine answers right now.
pub async fn is_running(exec: &dyn Exec, engine: &Engine, host: &HostEnv) -> bool {
    run(exec, Path::new(&engine.cli), host, info_args(engine.kind))
        .await
        .is_some()
}

/// The socket path an endpoint names, with symlinks resolved: `/var/run/docker.sock` is a link
/// to whichever engine installed it, and the link's own name says nothing about which.
fn resolve(endpoint: &str) -> String {
    let Some(path) = endpoint.strip_prefix("unix://") else {
        return endpoint.to_string();
    };
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// The kind comes from the endpoint's path first and from the context's name second.
pub fn classify(endpoint: &str, context: Option<&str>) -> EngineKind {
    let path = resolve(endpoint);
    if path.contains("/.colima/") {
        return EngineKind::Colima;
    }
    if path.contains("/.orbstack/") {
        return EngineKind::OrbStack;
    }
    if path.contains("/.docker/run/") {
        return EngineKind::DockerDesktop;
    }
    if path.contains("podman") {
        return EngineKind::Podman;
    }
    match context {
        Some(c) if c == "colima" || c.starts_with("colima-") => EngineKind::Colima,
        Some("orbstack") => EngineKind::OrbStack,
        Some("desktop-linux") => EngineKind::DockerDesktop,
        _ => EngineKind::Other,
    }
}

/// The command that starts the engine, as shown to the reviewer; `None` when Clúsia cannot
/// name one.
pub fn start_command(kind: EngineKind, endpoint: &str) -> Option<String> {
    match kind {
        EngineKind::Colima => {
            let path = resolve(endpoint);
            let profile = path
                .split_once("/.colima/")
                .and_then(|(_, rest)| rest.split('/').next())
                .filter(|p| !p.is_empty() && *p != "default");
            Some(match profile {
                Some(profile) => format!("colima start --profile {profile}"),
                None => "colima start".to_string(),
            })
        }
        EngineKind::OrbStack => Some("open -a OrbStack".to_string()),
        EngineKind::DockerDesktop => Some("open -a Docker".to_string()),
        EngineKind::Podman => Some("podman machine start".to_string()),
        EngineKind::Other => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::testkit::{FakeExec, FakeReply};

    const COLIMA_CONTEXT: &str = r#"[{"Name":"colima","Metadata":{},"Endpoints":{"docker":{"Host":"unix:///Users/octo/.colima/default/docker.sock","SkipTLSVerify":false}}}]"#;

    fn host() -> HostEnv {
        HostEnv {
            path: Some(OsString::from("/usr/bin:/bin")),
            home: PathBuf::from("/Users/octo"),
            docker_override: None,
            docker_host: None,
        }
    }

    fn colima_fake() -> Arc<FakeExec> {
        let fake = Arc::new(FakeExec::new());
        fake.executable_at("/opt/homebrew/bin/docker");
        fake.on(
            "docker",
            &["context", "inspect"],
            FakeReply::ok(COLIMA_CONTEXT),
        )
        .on("docker", &["info"], FakeReply::ok("colima\n"))
        .on("docker", &["compose", "version"], FakeReply::ok("v5.4.0\n"));
        fake
    }

    #[tokio::test]
    async fn colima_is_found_in_a_place_the_path_lacks() {
        let fake = colima_fake();
        let engine = detect(fake.as_ref(), &host()).await.unwrap();
        assert_eq!(
            engine,
            Engine {
                kind: EngineKind::Colima,
                cli: "/opt/homebrew/bin/docker".into(),
                endpoint: "unix:///Users/octo/.colima/default/docker.sock".into(),
                context: Some("colima".into()),
                running: true,
                start: Some("colima start".into()),
                compose: Some("5.4.0".into()),
            }
        );
    }

    #[tokio::test]
    async fn every_child_gets_the_thin_path_fix_and_nothing_else() {
        let fake = colima_fake();
        detect(fake.as_ref(), &host()).await.unwrap();
        let calls = fake.calls();
        let args: Vec<Vec<String>> = calls
            .iter()
            .map(|c| {
                c.args
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect()
            })
            .collect();
        assert_eq!(
            args,
            [
                vec!["context", "inspect"],
                vec!["info", "--format", "{{.Name}}"],
                vec!["compose", "version", "--short"],
            ]
        );
        for call in &calls {
            assert!(call.clear_env);
            assert_eq!(
                call.env,
                vec![
                    (
                        OsString::from("PATH"),
                        OsString::from("/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin")
                    ),
                    (OsString::from("HOME"), OsString::from("/Users/octo")),
                ]
            );
        }
    }

    #[tokio::test]
    async fn a_stopped_engine_is_found_and_not_running() {
        let fake = Arc::new(FakeExec::new());
        fake.executable_at("/opt/homebrew/bin/docker");
        fake.on(
            "docker",
            &["context", "inspect"],
            FakeReply::ok(COLIMA_CONTEXT),
        )
        .on(
            "docker",
            &["info"],
            FakeReply::fail(1, "failed to connect to the docker API"),
        );
        let engine = detect(fake.as_ref(), &host()).await.unwrap();
        assert!(!engine.running);
        assert_eq!(engine.kind, EngineKind::Colima);
        assert_eq!(engine.compose, None, "compose version failed");
        assert!(!is_running(fake.as_ref(), &engine, &host()).await);
    }

    #[tokio::test]
    async fn is_running_asks_the_engine_again() {
        let fake = Arc::new(FakeExec::new());
        fake.executable_at("/opt/homebrew/bin/docker");
        fake.on_sequence(
            "docker",
            &["info"],
            vec![FakeReply::fail(1, "down"), FakeReply::ok("colima")],
        );
        let engine = Engine {
            kind: EngineKind::Colima,
            cli: "/opt/homebrew/bin/docker".into(),
            endpoint: String::new(),
            context: None,
            running: false,
            start: None,
            compose: None,
        };
        assert!(!is_running(fake.as_ref(), &engine, &host()).await);
        assert!(is_running(fake.as_ref(), &engine, &host()).await);
    }

    #[tokio::test]
    async fn no_cli_means_no_engine() {
        let fake = FakeExec::new();
        assert_eq!(detect(&fake, &host()).await, None);
        assert!(fake.calls().is_empty());
    }

    #[tokio::test]
    async fn the_path_comes_before_the_usual_places_and_docker_before_podman() {
        let fake = FakeExec::new();
        fake.executable_at("/opt/x/bin/docker")
            .executable_at("/opt/homebrew/bin/docker")
            .executable_at("/opt/homebrew/bin/podman");
        let mut env = host();
        env.path = Some(OsString::from("/opt/x/bin:/usr/bin"));
        detect(&fake, &env).await.unwrap();
        assert_eq!(fake.calls()[0].program, PathBuf::from("/opt/x/bin/docker"));

        let only_podman = FakeExec::new();
        only_podman.executable_at("/opt/homebrew/bin/podman");
        only_podman
            .on(
                "podman",
                &["machine", "inspect"],
                FakeReply::ok("/var/folders/x/podman.sock\n"),
            )
            .on("podman", &["info"], FakeReply::ok("host"));
        let engine = detect(&only_podman, &host()).await.unwrap();
        assert_eq!(engine.kind, EngineKind::Podman);
        assert_eq!(engine.cli, "/opt/homebrew/bin/podman");
        assert_eq!(engine.endpoint, "unix:///var/folders/x/podman.sock");
        assert_eq!(engine.start.as_deref(), Some("podman machine start"));
        assert!(engine.running);
        assert_eq!(engine.compose, None);
    }

    #[tokio::test]
    async fn the_home_is_expanded_in_the_usual_places() {
        let fake = FakeExec::new();
        fake.executable_at("/Users/octo/.orbstack/bin/docker");
        fake.on("docker", &["context", "inspect"], FakeReply::ok(
            r#"[{"Name":"orbstack","Endpoints":{"docker":{"Host":"unix:///Users/octo/.orbstack/run/docker.sock"}}}]"#,
        ));
        let engine = detect(&fake, &host()).await.unwrap();
        assert_eq!(engine.kind, EngineKind::OrbStack);
        assert_eq!(engine.start.as_deref(), Some("open -a OrbStack"));
    }

    #[tokio::test]
    async fn an_override_is_the_only_candidate() {
        let fake = FakeExec::new();
        fake.executable_at("/opt/homebrew/bin/docker");
        let mut env = host();
        env.docker_override = Some(PathBuf::from("/nonexistent/docker"));
        assert_eq!(
            detect(&fake, &env).await,
            None,
            "a missing override means no engine"
        );
        assert!(fake.calls().is_empty());

        fake.executable_at("/tmp/fake/docker");
        env.docker_override = Some(PathBuf::from("/tmp/fake/docker"));
        detect(&fake, &env).await.unwrap();
        assert_eq!(fake.calls()[0].program, PathBuf::from("/tmp/fake/docker"));
    }

    #[tokio::test]
    async fn docker_host_wins_over_the_context_and_reaches_the_children() {
        let fake = colima_fake();
        let mut env = host();
        env.docker_host = Some("unix:///Users/octo/.orbstack/run/docker.sock".into());
        let engine = detect(fake.as_ref(), &env).await.unwrap();
        assert_eq!(engine.kind, EngineKind::OrbStack);
        assert_eq!(
            engine.endpoint,
            "unix:///Users/octo/.orbstack/run/docker.sock"
        );
        let call = &fake.calls()[0];
        assert!(call.env.contains(&(
            OsString::from("DOCKER_HOST"),
            OsString::from("unix:///Users/octo/.orbstack/run/docker.sock")
        )));
    }

    #[tokio::test]
    async fn a_context_that_cannot_be_read_is_an_unknown_engine() {
        let fake = FakeExec::new();
        fake.executable_at("/usr/local/bin/docker");
        fake.on(
            "docker",
            &["context", "inspect"],
            FakeReply::fail(1, "boom"),
        )
        .on("docker", &["info"], FakeReply::ok("x"));
        let engine = detect(&fake, &host()).await.unwrap();
        assert_eq!(engine.kind, EngineKind::Other);
        assert_eq!(engine.context, None);
        assert_eq!(engine.endpoint, "");
        assert_eq!(engine.start, None);
        assert!(engine.running);
    }

    #[test]
    fn cli_env_is_the_cli_directory_and_the_home() {
        let env = cli_env(Path::new("/opt/homebrew/bin/docker"), &host());
        assert_eq!(
            env,
            vec![
                (
                    OsString::from("PATH"),
                    OsString::from("/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin")
                ),
                (OsString::from("HOME"), OsString::from("/Users/octo")),
            ]
        );
    }

    #[test]
    fn the_kind_comes_from_the_endpoint_then_the_context() {
        let cases = [
            (
                "unix:///Users/octo/.colima/default/docker.sock",
                None,
                EngineKind::Colima,
            ),
            (
                "unix:///Users/octo/.colima/work/docker.sock",
                Some("colima-work"),
                EngineKind::Colima,
            ),
            (
                "unix:///Users/octo/.orbstack/run/docker.sock",
                None,
                EngineKind::OrbStack,
            ),
            (
                "unix:///Users/octo/.docker/run/docker.sock",
                None,
                EngineKind::DockerDesktop,
            ),
            (
                "unix:///nonexistent/docker.sock",
                Some("desktop-linux"),
                EngineKind::DockerDesktop,
            ),
            (
                "unix:///var/folders/x/podman/podman-machine-default-api.sock",
                None,
                EngineKind::Podman,
            ),
            ("tcp://10.0.0.5:2375", Some("remote"), EngineKind::Other),
            ("", None, EngineKind::Other),
        ];
        for (endpoint, context, kind) in cases {
            assert_eq!(classify(endpoint, context), kind, "{endpoint} {context:?}");
        }
    }

    #[test]
    fn a_symlinked_socket_is_classified_by_what_it_points_at() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join(".colima/default");
        std::fs::create_dir_all(&real_dir).unwrap();
        let real = real_dir.join("docker.sock");
        std::fs::write(&real, "").unwrap();
        let link = dir.path().join("docker.sock");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let endpoint = format!("unix://{}", link.display());
        assert_eq!(classify(&endpoint, Some("default")), EngineKind::Colima);
        assert_eq!(
            start_command(EngineKind::Colima, &endpoint).as_deref(),
            Some("colima start")
        );
    }

    #[test]
    fn each_engine_says_how_to_start() {
        let cases = [
            (
                EngineKind::Colima,
                "unix:///Users/octo/.colima/default/docker.sock",
                Some("colima start"),
            ),
            (
                EngineKind::Colima,
                "unix:///Users/octo/.colima/work/docker.sock",
                Some("colima start --profile work"),
            ),
            (EngineKind::OrbStack, "", Some("open -a OrbStack")),
            (EngineKind::DockerDesktop, "", Some("open -a Docker")),
            (EngineKind::Podman, "", Some("podman machine start")),
            (EngineKind::Other, "tcp://10.0.0.5:2375", None),
        ];
        for (kind, endpoint, expected) in cases {
            assert_eq!(
                start_command(kind, endpoint).as_deref(),
                expected,
                "{kind:?} {endpoint}"
            );
        }
    }
}
