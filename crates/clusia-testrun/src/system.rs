//! A review's running system: the engine objects that run it and expose it, and the pure
//! pieces around them.
//!
//! A system is one container on an `--internal` network, so it has no way out and nothing
//! can be published from it. A second container, the forwarder, sits on that network and on a
//! plain bridge of its own and publishes `127.0.0.1::<port>` for each port; it forwards only
//! to the system's container. When the reviewer allows outbound network, the system is also
//! connected to that bridge, which is then its only way out.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use clusia_core::cockpit::RunSide;
use clusia_core::printable::printable;
use clusia_core::system::{LOG_LINE_MAX, SYSTEM_STOP_SECS, log_line, off_loopback_text};

use crate::compose::internal_network;
use crate::container::{cache_volume_args, first_line};
use crate::docker::{Docker, RunMounts, common_args, git_file_problem, mount_problem};
use crate::exec::{Exec, Output};
use crate::image::ImagePlan;
use crate::names::{LabelKind, Labels, sha256_hex};

/// The limit of one engine call made for a system.
const SYSTEM_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// The forwarder's image: Alpine with `socat`.
pub const FORWARDER_DOCKERFILE: &str = "FROM alpine:3.20\nRUN apk add --no-cache socat\n";

const UNREADABLE_PORTS: &str = "Could not read the published ports";

/// What one system's engine objects are called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemNames {
    pub base: String,
    pub container: String,
    pub forwarder: String,
    /// The plain bridge the forwarder publishes from and a network-allowed system leaves by.
    pub publish_net: String,
    /// The internal network the system and the forwarder share.
    pub inner_net: String,
    /// Whether Clúsia made `inner_net` itself, and so removes it.
    pub owns_inner: bool,
}

/// `services_project` is the compose project of the review's stack: its internal network is
/// then the system's, and Clúsia does not own it.
pub fn system_names(
    slug: &str,
    number: u64,
    side: RunSide,
    services_project: Option<String>,
) -> SystemNames {
    let mut base = format!("clusia-system-{slug}-{number}");
    if side == RunSide::Main {
        base.push_str("-main");
    }
    let (inner_net, owns_inner) = match services_project {
        Some(project) => (internal_network(&project), false),
        None => (format!("{base}-net"), true),
    };
    SystemNames {
        container: base.clone(),
        forwarder: format!("{base}-fwd"),
        publish_net: format!("{base}-pub"),
        inner_net,
        owns_inner,
        base,
    }
}

/// Every name the sweep must leave alone while the system lives. `<base>-net` is listed even
/// when the system uses the stack's network: the name then matches nothing.
pub fn live_names(names: &SystemNames) -> Vec<String> {
    vec![
        names.container.clone(),
        names.forwarder.clone(),
        names.publish_net.clone(),
        format!("{}-net", names.base),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemJob {
    pub names: SystemNames,
    pub image: String,
    pub start: String,
    pub env: Vec<(String, String)>,
    pub ports: Vec<u16>,
    /// Whether the system may reach out.
    pub network: bool,
    pub cpus: Option<u32>,
    pub memory_mb: Option<u32>,
    pub labels: Labels,
}

/// The review's labels with the kind of a system's objects, whatever kind the caller built.
fn system_labels(labels: &Labels) -> Labels {
    Labels {
        kind: LabelKind::System,
        ..labels.clone()
    }
}

/// `docker create` for the system container. It joins the internal network only, has no `-p`
/// and no `--rm` (`remove_system` removes it). Without network, `--dns 127.0.0.1` leaves the
/// embedded resolver answering the names of its own networks and nothing outside.
pub fn system_create_args(job: &SystemJob, mounts: &RunMounts) -> Vec<String> {
    let labels = system_labels(&job.labels);
    let mut args: Vec<String> = [
        "create",
        "--name",
        &job.names.container,
        "--network",
        &job.names.inner_net,
    ]
    .map(String::from)
    .to_vec();
    if !job.network {
        args.extend(["--dns", "127.0.0.1"].map(String::from));
    }
    args.extend(common_args(
        &labels,
        &job.env,
        mounts,
        job.cpus,
        job.memory_mb,
    ));
    args.extend(["--entrypoint", "/bin/sh", &job.image, "-c", &job.start].map(String::from));
    args
}

/// `docker create` for the forwarder: read-only, no capabilities, and the only thing that
/// publishes a port, each on `127.0.0.1` with a host port the engine picks.
pub fn forwarder_create_args(job: &SystemJob) -> Vec<String> {
    let mut args: Vec<String> = [
        "create",
        "--name",
        &job.names.forwarder,
        "--network",
        &job.names.publish_net,
        "--pull",
        "never",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--user",
        "65534:65534",
        "--pids-limit",
        "64",
        "--memory",
        "64m",
    ]
    .map(String::from)
    .to_vec();
    args.extend(system_labels(&job.labels).args());
    for port in &job.ports {
        args.extend(["-p".to_string(), format!("127.0.0.1::{port}")]);
    }
    args.extend(
        [
            "--entrypoint",
            "/bin/sh",
            &forwarder_tag(),
            "-c",
            &forward_script(&job.names.container, &job.ports),
        ]
        .map(String::from),
    );
    args
}

/// What the forwarder runs. `target` is a name Clúsia built (`[a-z0-9-]`), so it is safe
/// unquoted, and only numbers and literals go around it.
pub fn forward_script(target: &str, ports: &[u16]) -> String {
    let one = |p: &u16| format!("socat TCP-LISTEN:{p},fork,reuseaddr TCP:{target}:{p}");
    match ports {
        [] => "exit 1".to_string(),
        [port] => format!("exec {}", one(port)),
        many => {
            let all: Vec<String> = many.iter().map(one).collect();
            format!("{} & wait", all.join(" & "))
        }
    }
}

pub fn forwarder_tag() -> String {
    let hash = sha256_hex(FORWARDER_DOCKERFILE.as_bytes());
    format!("clusia/system-forwarder:{}", &hash[..16])
}

pub fn forwarder_plan() -> ImagePlan {
    ImagePlan {
        base_build: None,
        dockerfile: FORWARDER_DOCKERFILE.to_string(),
        tag: forwarder_tag(),
        hash: sha256_hex(FORWARDER_DOCKERFILE.as_bytes()),
        notes: vec![],
        network: true,
    }
}

/// One engine call. An error is a call that could not run or timed out; the exit code is the
/// caller's to read.
async fn run(
    exec: &dyn Exec,
    docker: &Docker,
    args: Vec<String>,
    what: &str,
    limit: Duration,
) -> Result<Output, String> {
    let out = exec
        .output(&docker.command(args), limit)
        .await
        .map_err(|e| format!("docker {what} failed: {e}"))?;
    if out.timed_out {
        return Err(format!("docker {what} failed: it timed out"));
    }
    Ok(out)
}

/// One engine call that must exit 0.
async fn run_ok(
    exec: &dyn Exec,
    docker: &Docker,
    args: Vec<String>,
    what: &str,
) -> Result<Output, String> {
    let out = run(exec, docker, args, what, SYSTEM_CALL_TIMEOUT).await?;
    if out.code != Some(0) {
        return Err(format!("docker {what} failed: {}", first_line(&out)));
    }
    Ok(out)
}

/// What an engine call is named in its failure: `network create`, `volume create`, `start`…
fn what(args: &[String]) -> String {
    match args.first().map(String::as_str) {
        Some("network" | "volume") => args.iter().take(2).cloned().collect::<Vec<_>>().join(" "),
        Some(verb) => verb.to_string(),
        None => String::new(),
    }
}

/// Runs each call of `steps` in turn; the first that does not exit 0 stops them.
async fn run_steps(
    exec: &dyn Exec,
    docker: &Docker,
    steps: Vec<Vec<String>>,
) -> Result<(), String> {
    for args in steps {
        let verb = what(&args);
        run_ok(exec, docker, args, &verb).await?;
    }
    Ok(())
}

/// The calls that make a system's networks: the internal one (only when Clúsia owns it),
/// then the publish bridge, each with the review's labels of kind `system`.
pub fn network_steps(names: &SystemNames, labels: &Labels) -> Vec<Vec<String>> {
    let labels = system_labels(labels);
    let make = |internal: bool, name: &str| {
        let mut args = vec!["network".to_string(), "create".to_string()];
        if internal {
            args.push("--internal".to_string());
        }
        args.extend(labels.args());
        args.push(name.to_string());
        args
    };
    let mut steps = Vec::new();
    if names.owns_inner {
        steps.push(make(true, &names.inner_net));
    }
    steps.push(make(false, &names.publish_net));
    steps
}

/// The calls that make and start the system container: its cache volumes, the create, the
/// publish network when allowed, then the start.
pub fn system_steps(job: &SystemJob, mounts: &RunMounts) -> Vec<Vec<String>> {
    let cache = Labels {
        kind: LabelKind::Cache,
        ..job.labels.clone()
    };
    let mut steps: Vec<Vec<String>> = mounts
        .caches
        .iter()
        .map(|(volume, _)| cache_volume_args(&cache, volume))
        .collect();
    steps.push(system_create_args(job, mounts));
    if job.network {
        steps.push(
            [
                "network",
                "connect",
                &job.names.publish_net,
                &job.names.container,
            ]
            .map(String::from)
            .to_vec(),
        );
    }
    steps.push(vec!["start".into(), job.names.container.clone()]);
    steps
}

/// The calls that make the forwarder, put it on the internal network too, and start it.
pub fn forwarder_steps(job: &SystemJob) -> Vec<Vec<String>> {
    let names = &job.names;
    vec![
        forwarder_create_args(job),
        ["network", "connect", &names.inner_net, &names.forwarder]
            .map(String::from)
            .to_vec(),
        vec!["start".into(), names.forwarder.clone()],
    ]
}

/// `docker port` of the forwarder.
pub fn port_args(names: &SystemNames) -> Vec<String> {
    vec!["port".into(), names.forwarder.clone()]
}

/// The host ports of `ports` in a `docker port` answer, only those; any line off loopback,
/// or a port missing, refuses the whole answer.
pub fn pick_ports(text: &str, ports: &[u16]) -> Result<BTreeMap<u16, u16>, String> {
    let found = parse_port_lines(text)?;
    ports
        .iter()
        .map(|port| {
            found
                .get(port)
                .map(|host| (*port, *host))
                .ok_or_else(|| format!("Port {port} was not published"))
        })
        .collect()
}

/// The internal network (only when Clúsia owns it), then the publish bridge.
pub async fn create_networks(
    exec: &dyn Exec,
    docker: &Docker,
    names: &SystemNames,
    labels: &Labels,
) -> Result<(), String> {
    run_steps(exec, docker, network_steps(names, labels)).await
}

/// The cache volumes, then create, connect to the publish bridge when allowed, then start.
pub async fn create_system(
    exec: &dyn Exec,
    docker: &Docker,
    job: &SystemJob,
    mounts: &RunMounts,
) -> Result<(), String> {
    if let Some(problem) = mount_problem(mounts).or_else(|| git_file_problem(&mounts.worktree)) {
        return Err(problem);
    }
    run_steps(exec, docker, system_steps(job, mounts)).await
}

/// Create the forwarder, put it on the internal network too, then start it.
pub async fn start_forwarder(
    exec: &dyn Exec,
    docker: &Docker,
    job: &SystemJob,
) -> Result<(), String> {
    run_steps(exec, docker, forwarder_steps(job)).await
}

/// Reads `docker port` lines (`8080/tcp -> 127.0.0.1:49153`) into container port → host port.
/// One line on any other address refuses the whole answer, whatever the other lines say.
pub fn parse_port_lines(text: &str) -> Result<BTreeMap<u16, u16>, String> {
    let mut ports = BTreeMap::new();
    let mut unreadable = false;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Some((container, bound)) = line.split_once(" -> ") else {
            unreadable = true;
            continue;
        };
        let Some((address, host)) = bound.trim().rsplit_once(':') else {
            unreadable = true;
            continue;
        };
        let address = address
            .strip_prefix('[')
            .and_then(|a| a.strip_suffix(']'))
            .unwrap_or(address);
        if address != "127.0.0.1" {
            let shown: String = printable(address).chars().take(64).collect();
            return Err(off_loopback_text(&shown));
        }
        let container = container.trim().strip_suffix("/tcp").unwrap_or(container);
        match (container.parse::<u16>(), host.parse::<u16>()) {
            (Ok(container), Ok(host)) => {
                ports.insert(container, host);
            }
            _ => unreadable = true,
        }
    }
    if unreadable {
        return Err(UNREADABLE_PORTS.to_string());
    }
    Ok(ports)
}

/// The host ports the forwarder was given for `ports`, only those.
pub async fn mapped_ports(
    exec: &dyn Exec,
    docker: &Docker,
    names: &SystemNames,
    ports: &[u16],
) -> Result<BTreeMap<u16, u16>, String> {
    let out = run_ok(exec, docker, port_args(names), "port").await?;
    pick_ports(&String::from_utf8_lossy(&out.stdout), ports)
}

pub fn health_exec_args(names: &SystemNames, command: &str) -> Vec<String> {
    ["exec", &names.container, "/bin/sh", "-c", command]
        .map(String::from)
        .to_vec()
}

pub fn wait_args(names: &SystemNames) -> Vec<String> {
    vec!["wait".to_string(), names.container.clone()]
}

/// A compose service name: it is the only agent text that reaches an argument list, and never
/// starts with `-`.
fn valid_service(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c))
}

/// The log stream of `start` or `service:<name>`; empty for anything else.
pub fn log_stream_args(names: &SystemNames, project: Option<&str>, process: &str) -> Vec<String> {
    if process == "start" {
        return ["logs", "-f", "--since", "0", &names.container]
            .map(String::from)
            .to_vec();
    }
    match (project, process.strip_prefix("service:")) {
        (Some(project), Some(service)) if valid_service(service) => [
            "compose",
            "-p",
            project,
            "logs",
            "-f",
            "--no-color",
            "--no-log-prefix",
            service,
        ]
        .map(String::from)
        .to_vec(),
        _ => vec![],
    }
}

/// Asks compose how a service's container stands; empty for a name that is not a service name.
pub fn service_ps_args(project: &str, service: &str) -> Vec<String> {
    if !valid_service(service) {
        return vec![];
    }
    [
        "compose", "-p", project, "ps", "-a", "--format", "json", service,
    ]
    .map(String::from)
    .to_vec()
}

/// `None` while the container runs or the answer cannot be read; `Some(code)` once it exited
/// (`code` is `None` when the answer holds none). Compose prints an array or one object per
/// line, depending on its version; the first object is read.
pub fn parse_service_exit(json: &str) -> Option<Option<i32>> {
    let first = serde_json::Deserializer::from_str(json)
        .into_iter::<serde_json::Value>()
        .next()?
        .ok()?;
    let object = match first {
        serde_json::Value::Array(items) => items.into_iter().next()?,
        other => other,
    };
    match object.get("State")?.as_str()? {
        "exited" | "dead" => Some(
            object
                .get("ExitCode")
                .and_then(serde_json::Value::as_i64)
                .and_then(|code| i32::try_from(code).ok()),
        ),
        _ => None,
    }
}

/// Stops the system, removes both containers and drops the networks Clúsia made. Every step
/// runs whatever the one before gave; a thing already gone is not a failure.
pub async fn remove_system(
    exec: &dyn Exec,
    docker: &Docker,
    names: &SystemNames,
) -> Result<(), String> {
    let limit = Duration::from_secs(SYSTEM_STOP_SECS + 20);
    let stop = SYSTEM_STOP_SECS.to_string();
    let mut steps: Vec<Vec<String>> = vec![
        ["stop", "-t", &stop, &names.container]
            .map(String::from)
            .to_vec(),
        ["rm", "-f", &names.container, &names.forwarder]
            .map(String::from)
            .to_vec(),
        ["network", "rm", &names.publish_net]
            .map(String::from)
            .to_vec(),
    ];
    if names.owns_inner {
        steps.push(
            ["network", "rm", &format!("{}-net", names.base)]
                .map(String::from)
                .to_vec(),
        );
    }
    let mut notes = Vec::new();
    for args in steps {
        let verb = args[0].clone();
        match run(exec, docker, args, &verb, limit).await {
            Err(note) => notes.push(note),
            Ok(out) if out.code != Some(0) && !is_gone(&out) => {
                notes.push(format!("docker {verb} failed: {}", real_failure(&out)));
            }
            Ok(_) => {}
        }
    }
    if notes.is_empty() {
        Ok(())
    } else {
        Err(notes.join("; "))
    }
}

/// The first line that is not a "gone" one, or the first line.
fn real_failure(out: &Output) -> String {
    let text = String::from_utf8_lossy(&out.stderr);
    let line = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .find(|l| !l.contains("No such container") && !l.contains("No such network"))
        .unwrap_or("");
    printable(line)
}

/// Whether every line the engine printed says a thing is already gone; one other line is a
/// real failure that must not hide behind it.
fn is_gone(out: &Output) -> bool {
    let text = String::from_utf8_lossy(&out.stderr);
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .peekable();
    lines.peek().is_some()
        && lines.all(|line| {
            ["No such container", "No such network", "not found"]
                .iter()
                .any(|gone| line.contains(gone))
        })
}

/// The newest lines of one process's output, numbered from 1, each cleaned once as it
/// completes.
#[derive(Debug, Clone)]
pub struct LogRing {
    max: usize,
    lines: VecDeque<String>,
    /// The number of the last completed line.
    last: u64,
    /// The raw bytes of the line being read.
    partial: Vec<u8>,
    /// The partial line outgrew its cap: the rest of it up to the newline is dropped.
    cut: bool,
}

/// How many raw bytes of one line are kept: room for `LOG_LINE_MAX` characters of any width.
const PARTIAL_MAX: usize = LOG_LINE_MAX * 4;

impl LogRing {
    pub fn new(max: usize) -> Self {
        Self {
            max,
            lines: VecDeque::new(),
            last: 0,
            partial: Vec::new(),
            cut: false,
        }
    }

    /// The lines `bytes` completed, as `(number, text)`; a partial line waits for the next push.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<(u64, String)> {
        let mut done = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            let (chunk, newline) = match rest.iter().position(|&b| b == b'\n') {
                Some(i) => (&rest[..i], true),
                None => (rest, false),
            };
            let room = PARTIAL_MAX - self.partial.len();
            if chunk.len() > room {
                self.cut = true;
            }
            self.partial
                .extend_from_slice(&chunk[..chunk.len().min(room)]);
            rest = &rest[chunk.len() + usize::from(newline)..];
            if newline {
                done.push(self.complete());
            }
        }
        done
    }

    fn complete(&mut self) -> (u64, String) {
        let raw = std::mem::take(&mut self.partial);
        let cut = std::mem::take(&mut self.cut);
        let mut text = log_line(&raw);
        if cut || text.chars().count() > LOG_LINE_MAX {
            text = text.chars().take(LOG_LINE_MAX - 1).collect();
            text.push('…');
        }
        self.last += 1;
        self.lines.push_back(text.clone());
        if self.lines.len() > self.max {
            self.lines.pop_front();
        }
        (self.last, text)
    }

    /// The last `min(n, max)` lines and the number of the first of them; `(last + 1, [])`
    /// when there are none.
    pub fn tail(&self, n: usize) -> (u64, Vec<String>) {
        let take = n.min(self.lines.len());
        let lines: Vec<String> = self
            .lines
            .iter()
            .skip(self.lines.len() - take)
            .cloned()
            .collect();
        (self.last + 1 - take as u64, lines)
    }

    pub fn last(&self) -> u64 {
        self.last
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clusia_core::cockpit::RunSide;

    use crate::names::{LabelKind, Labels};
    use crate::testkit::{FakeExec, FakeReply};

    use super::*;

    fn docker() -> Docker {
        Docker {
            cli: "/opt/homebrew/bin/docker".into(),
            env: vec![],
        }
    }

    fn labels() -> Labels {
        Labels {
            review: "acme~widgets~7".into(),
            repo: "acme/widgets".into(),
            kind: LabelKind::System,
        }
    }

    fn names(services: bool) -> SystemNames {
        let net = services.then(|| "clusia-acme-widgets-1a2b3c-7".to_string());
        system_names("acme-widgets-1a2b3c", 7, RunSide::Pr, net)
    }

    fn job(names: SystemNames, ports: &[u16], network: bool) -> SystemJob {
        SystemJob {
            names,
            image: "clusia/acme-widgets-1a2b3c:0123456789abcdef".into(),
            start: "cargo run -q --bin api -- --port 8080".into(),
            env: vec![
                ("RUST_LOG".into(), "info".into()),
                (
                    "DATABASE_URL".into(),
                    "postgres://test:test@postgres/auth".into(),
                ),
            ],
            ports: ports.to_vec(),
            network,
            cpus: Some(2),
            memory_mb: Some(2048),
            labels: labels(),
        }
    }

    fn mounts() -> RunMounts {
        RunMounts {
            worktree: PathBuf::from("/data/wt"),
            git_common: PathBuf::from("/data/clone/.git"),
            scenario: None,
            caches: vec![(
                "clusia-cache-acme-widgets-1a2b3c-cargo".into(),
                "/cache/cargo".into(),
            )],
        }
    }

    fn calls(fake: &FakeExec) -> Vec<Vec<String>> {
        fake.calls()
            .iter()
            .map(|c| {
                c.args
                    .iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect()
            })
            .collect()
    }

    fn values(args: &[String], flag: &str) -> Vec<String> {
        args.windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].clone())
            .collect()
    }

    /// Every docker verb is answered, so a test only overrides what it checks.
    fn answering(fake: &FakeExec) {
        for verb in [
            &["network", "create"][..],
            &["volume", "create"],
            &["create"],
            &["network", "connect"],
            &["start"],
            &["stop"],
            &["rm"],
            &["network", "rm"],
        ] {
            fake.on("docker", verb, FakeReply::ok(""));
        }
    }

    #[test]
    fn system_names_are_derived_from_the_review() {
        let own = names(false);
        assert_eq!(own.base, "clusia-system-acme-widgets-1a2b3c-7");
        assert_eq!(own.container, own.base);
        assert_eq!(own.forwarder, "clusia-system-acme-widgets-1a2b3c-7-fwd");
        assert_eq!(own.publish_net, "clusia-system-acme-widgets-1a2b3c-7-pub");
        assert_eq!(own.inner_net, "clusia-system-acme-widgets-1a2b3c-7-net");
        assert!(own.owns_inner);
        let shared = names(true);
        assert_eq!(
            shared.inner_net,
            "clusia-acme-widgets-1a2b3c-7_clusia_internal"
        );
        assert!(!shared.owns_inner);
        let main = system_names("acme-widgets-1a2b3c", 7, RunSide::Main, None);
        assert_eq!(main.base, "clusia-system-acme-widgets-1a2b3c-7-main");
        assert_eq!(
            main.forwarder,
            "clusia-system-acme-widgets-1a2b3c-7-main-fwd"
        );
        assert_eq!(
            live_names(&shared),
            vec![
                "clusia-system-acme-widgets-1a2b3c-7",
                "clusia-system-acme-widgets-1a2b3c-7-fwd",
                "clusia-system-acme-widgets-1a2b3c-7-pub",
                "clusia-system-acme-widgets-1a2b3c-7-net",
            ]
        );
    }

    #[test]
    fn the_system_container_shares_the_run_container_parts_and_publishes_nothing() {
        let job = job(names(false), &[8080], false);
        let args = system_create_args(&job, &mounts());
        let run = crate::docker::run_args(
            &crate::docker::ContainerJob {
                name: "n".into(),
                image: job.image.clone(),
                command: "c".into(),
                env: job.env.clone(),
                net: crate::docker::ContainerNet::Bridge,
                cpus: job.cpus,
                memory_mb: job.memory_mb,
                labels: job.labels.clone(),
            },
            &mounts(),
        );
        for flag in [
            "-v",
            "-e",
            "--security-opt",
            "--pids-limit",
            "--user",
            "--cpus",
            "--memory",
            "-w",
            "--pull",
        ] {
            assert_eq!(values(&args, flag), values(&run, flag), "{flag}");
        }
        assert_eq!(values(&args, "--label"), values(&run, "--label"));
        assert!(values(&args, "--label").contains(&"io.clusia.kind=system".to_string()));
        assert!(args.contains(&"--init".to_string()));
        assert_eq!(
            &args[..5],
            [
                "create",
                "--name",
                "clusia-system-acme-widgets-1a2b3c-7",
                "--network",
                "clusia-system-acme-widgets-1a2b3c-7-net"
            ]
        );
        assert!(!args.contains(&"--network-alias".to_string()));
        assert_eq!(
            values(&args, "--dns"),
            ["127.0.0.1"],
            "no network allowed: no resolver upstream"
        );
        let open = system_create_args(&self::job(names(false), &[8080], true), &mounts());
        assert!(
            values(&open, "--dns").is_empty(),
            "network allowed: no --dns"
        );
        assert_eq!(
            args[args.len() - 5..],
            [
                "--entrypoint",
                "/bin/sh",
                job.image.as_str(),
                "-c",
                job.start.as_str()
            ]
        );
        for publish in ["-p", "-P", "--publish", "--publish-all", "--rm"] {
            assert!(!args.contains(&publish.to_string()), "{publish}");
        }
    }

    #[test]
    fn the_forwarder_publishes_on_loopback_only() {
        let args = forwarder_create_args(&job(names(false), &[8080, 9090], false));
        assert_eq!(values(&args, "-p"), ["127.0.0.1::8080", "127.0.0.1::9090"]);
        for publish in ["-P", "--publish", "--publish-all"] {
            assert!(!args.contains(&publish.to_string()), "{publish}");
        }
        assert_eq!(
            values(&args, "--network"),
            ["clusia-system-acme-widgets-1a2b3c-7-pub"]
        );
        for flag in [
            "--read-only",
            "--pull",
            "--cap-drop",
            "--security-opt",
            "--user",
            "--pids-limit",
            "--memory",
        ] {
            assert!(args.contains(&flag.to_string()), "{flag}");
        }
        assert_eq!(values(&args, "--user"), ["65534:65534"]);
        assert_eq!(values(&args, "--cap-drop"), ["ALL"]);
        assert_eq!(
            args[..3],
            [
                "create",
                "--name",
                "clusia-system-acme-widgets-1a2b3c-7-fwd"
            ]
        );
        assert_eq!(args[args.len() - 5], "--entrypoint");
        assert_eq!(args[args.len() - 3], forwarder_tag());
        assert_eq!(
            args[args.len() - 1],
            forward_script("clusia-system-acme-widgets-1a2b3c-7", &[8080, 9090])
        );
        assert!(values(&args, "--label").contains(&"io.clusia.kind=system".to_string()));
    }

    #[test]
    fn the_forward_script_is_built_from_port_numbers_only() {
        let target = "clusia-system-acme-widgets-1a2b3c-7";
        assert_eq!(
            forward_script(target, &[80]),
            "exec socat TCP-LISTEN:80,fork,reuseaddr TCP:clusia-system-acme-widgets-1a2b3c-7:80"
        );
        assert_eq!(
            forward_script(target, &[8080, 9090]),
            "socat TCP-LISTEN:8080,fork,reuseaddr TCP:clusia-system-acme-widgets-1a2b3c-7:8080 & socat TCP-LISTEN:9090,fork,reuseaddr TCP:clusia-system-acme-widgets-1a2b3c-7:9090 & wait"
        );
        assert_eq!(forward_script(target, &[]), "exit 1");
    }

    #[test]
    fn the_forwarder_image_is_named_by_its_dockerfile() {
        assert_eq!(
            FORWARDER_DOCKERFILE,
            "FROM alpine:3.20\nRUN apk add --no-cache socat\n"
        );
        let hash = crate::names::sha256_hex(FORWARDER_DOCKERFILE.as_bytes());
        assert_eq!(
            forwarder_tag(),
            format!("clusia/system-forwarder:{}", &hash[..16])
        );
        let plan = forwarder_plan();
        assert_eq!(
            (plan.base_build, plan.dockerfile.as_str(), plan.network),
            (None, FORWARDER_DOCKERFILE, true)
        );
        assert_eq!((plan.tag, plan.hash), (forwarder_tag(), hash));
    }

    #[tokio::test]
    async fn networks_are_made_with_the_system_label_and_the_inner_one_is_internal() {
        let fake = FakeExec::new();
        answering(&fake);
        create_networks(&fake, &docker(), &names(false), &labels())
            .await
            .unwrap();
        let calls = calls(&fake);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0][..3], ["network", "create", "--internal"]);
        assert_eq!(
            calls[0].last().unwrap(),
            "clusia-system-acme-widgets-1a2b3c-7-net"
        );
        assert_eq!(calls[1][..2], ["network", "create"]);
        assert!(!calls[1].contains(&"--internal".to_string()));
        assert_eq!(
            calls[1].last().unwrap(),
            "clusia-system-acme-widgets-1a2b3c-7-pub"
        );
        assert!(values(&calls[1], "--label").contains(&"io.clusia.kind=system".to_string()));

        // With the stack's own network, only the publish network is made.
        let fake = FakeExec::new();
        answering(&fake);
        create_networks(&fake, &docker(), &names(true), &labels())
            .await
            .unwrap();
        assert_eq!(self::calls(&fake).len(), 1);

        // A refusal stops the rest.
        let fake = FakeExec::new();
        fake.on(
            "docker",
            &["network", "create"],
            FakeReply::fail(1, "network already exists"),
        );
        let err = create_networks(&fake, &docker(), &names(false), &labels())
            .await
            .unwrap_err();
        assert_eq!(err, "docker network create failed: network already exists");
        assert_eq!(self::calls(&fake).len(), 1);
    }

    #[tokio::test]
    async fn the_system_joins_the_publish_network_only_when_allowed() {
        let tree = tempfile::tempdir().unwrap();
        let mut mounts = mounts();
        mounts.worktree = tree.path().to_path_buf();
        std::fs::write(
            tree.path().join(".git"),
            "gitdir: /data/clone/.git/worktrees/x\n",
        )
        .unwrap();
        for allowed in [false, true] {
            let fake = FakeExec::new();
            answering(&fake);
            create_system(
                &fake,
                &docker(),
                &job(names(false), &[8080], allowed),
                &mounts,
            )
            .await
            .unwrap();
            let calls = calls(&fake);
            let verbs: Vec<String> = calls
                .iter()
                .map(|c| c.iter().take(2).cloned().collect::<Vec<_>>().join(" "))
                .collect();
            let connect = calls.iter().position(|c| c[..2] == ["network", "connect"]);
            let create = calls.iter().position(|c| c[0] == "create").unwrap();
            let start = calls.iter().position(|c| c[0] == "start").unwrap();
            assert_eq!(verbs[0], "volume create", "the cache volumes come first");
            if allowed {
                let connect = connect.expect("connected when allowed");
                assert!(
                    create < connect && connect < start,
                    "connected before it starts"
                );
                assert_eq!(
                    calls[connect][2..],
                    [
                        "clusia-system-acme-widgets-1a2b3c-7-pub",
                        "clusia-system-acme-widgets-1a2b3c-7"
                    ]
                );
            } else {
                assert_eq!(connect, None, "the internal network is its only network");
            }
            assert_eq!(
                calls[start],
                ["start", "clusia-system-acme-widgets-1a2b3c-7"]
            );
        }
    }

    #[tokio::test]
    async fn create_system_stops_at_the_first_failure_and_refuses_an_unmountable_path() {
        let tree = tempfile::tempdir().unwrap();
        std::fs::write(tree.path().join(".git"), "gitdir: x\n").unwrap();
        let mut mounts = mounts();
        mounts.worktree = tree.path().to_path_buf();
        let fake = FakeExec::new();
        fake.on("docker", &["volume", "create"], FakeReply::ok(""));
        fake.on(
            "docker",
            &["create"],
            FakeReply::fail(125, "Error response from daemon: no such image"),
        );
        let err = create_system(&fake, &docker(), &job(names(false), &[8080], true), &mounts)
            .await
            .unwrap_err();
        assert_eq!(
            err,
            "docker create failed: Error response from daemon: no such image"
        );
        assert!(
            calls(&fake)
                .iter()
                .all(|c| c[0] != "start" && c[..2] != ["network", "connect"])
        );

        let fake = FakeExec::new();
        let mut odd = mounts.clone();
        odd.worktree = PathBuf::from("/data/a:b");
        assert!(
            create_system(&fake, &docker(), &job(names(false), &[8080], false), &odd)
                .await
                .is_err()
        );
        assert!(
            calls(&fake).is_empty(),
            "no docker call for a path that cannot be mounted"
        );
    }

    #[tokio::test]
    async fn start_forwarder_creates_connects_the_inner_network_then_starts() {
        let fake = FakeExec::new();
        answering(&fake);
        start_forwarder(&fake, &docker(), &job(names(false), &[8080], false))
            .await
            .unwrap();
        let calls = calls(&fake);
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0][0], "create");
        assert_eq!(
            calls[1],
            [
                "network",
                "connect",
                "clusia-system-acme-widgets-1a2b3c-7-net",
                "clusia-system-acme-widgets-1a2b3c-7-fwd"
            ]
        );
        assert_eq!(
            calls[2],
            ["start", "clusia-system-acme-widgets-1a2b3c-7-fwd"]
        );

        let fake = FakeExec::new();
        fake.on("docker", &["create"], FakeReply::fail(125, "Conflict"));
        assert!(
            start_forwarder(&fake, &docker(), &job(names(false), &[8080], false))
                .await
                .is_err()
        );
        assert_eq!(self::calls(&fake).len(), 1);
    }

    #[test]
    fn a_mapping_off_loopback_is_refused() {
        assert_eq!(
            parse_port_lines("8080/tcp -> 127.0.0.1:49153\n9090/tcp -> 127.0.0.1:49154\n"),
            Ok(BTreeMap::from([(8080, 49153), (9090, 49154)]))
        );
        assert_eq!(
            parse_port_lines("\n8080/tcp -> 127.0.0.1:49153\n8080/tcp -> 127.0.0.1:49153\n"),
            Ok(BTreeMap::from([(8080, 49153)]))
        );
        for (line, address) in [
            ("8080/tcp -> 0.0.0.0:49153", "0.0.0.0"),
            ("8080/tcp -> [::]:49153", "::"),
            ("8080/tcp -> [::1]:49153", "::1"),
            ("8080/tcp -> 192.168.1.5:49153", "192.168.1.5"),
        ] {
            assert_eq!(
                parse_port_lines(line),
                Err(off_loopback_text(address)),
                "{line}"
            );
        }
        // One bad line refuses the whole answer, even after a good one.
        assert_eq!(
            parse_port_lines("8080/tcp -> 127.0.0.1:49153\n8080/tcp -> 0.0.0.0:49153"),
            Err(off_loopback_text("0.0.0.0"))
        );
        assert_eq!(
            parse_port_lines("garbage"),
            Err("Could not read the published ports".to_string())
        );
    }

    #[tokio::test]
    async fn mapped_ports_names_a_missing_port_and_a_failed_call() {
        let fake = FakeExec::new();
        fake.on(
            "docker",
            &["port", "clusia-system-acme-widgets-1a2b3c-7-fwd"],
            FakeReply::ok("8080/tcp -> 127.0.0.1:49153\n"),
        );
        let n = names(false);
        assert_eq!(
            mapped_ports(&fake, &docker(), &n, &[8080]).await,
            Ok(BTreeMap::from([(8080, 49153)]))
        );
        assert_eq!(
            mapped_ports(&fake, &docker(), &n, &[8080, 9090]).await,
            Err("Port 9090 was not published".to_string())
        );
        let fake = FakeExec::new();
        fake.on(
            "docker",
            &["port"],
            FakeReply::fail(1, "No such container: x"),
        );
        assert_eq!(
            mapped_ports(&fake, &docker(), &n, &[8080]).await,
            Err("docker port failed: No such container: x".to_string())
        );
        let fake = FakeExec::new();
        fake.on(
            "docker",
            &["port"],
            FakeReply::ok("8080/tcp -> 0.0.0.0:49153\n"),
        );
        assert_eq!(
            mapped_ports(&fake, &docker(), &n, &[8080]).await,
            Err(off_loopback_text("0.0.0.0"))
        );
    }

    #[test]
    fn exec_log_and_wait_arguments() {
        let n = names(false);
        assert_eq!(
            health_exec_args(&n, "curl -fs localhost:8080/health"),
            [
                "exec",
                "clusia-system-acme-widgets-1a2b3c-7",
                "/bin/sh",
                "-c",
                "curl -fs localhost:8080/health"
            ]
        );
        assert_eq!(
            wait_args(&n),
            ["wait", "clusia-system-acme-widgets-1a2b3c-7"]
        );
        assert_eq!(
            log_stream_args(&n, None, "start"),
            [
                "logs",
                "-f",
                "--since",
                "0",
                "clusia-system-acme-widgets-1a2b3c-7"
            ]
        );
        assert_eq!(
            log_stream_args(&n, Some("clusia-acme-widgets-1a2b3c-7"), "service:postgres"),
            [
                "compose",
                "-p",
                "clusia-acme-widgets-1a2b3c-7",
                "logs",
                "-f",
                "--no-color",
                "--no-log-prefix",
                "postgres"
            ]
        );
        for (project, process) in [
            (None, "service:postgres"),
            (Some("p"), "service:"),
            (Some("p"), "service:-x"),
            (Some("p"), "service:a b"),
            (Some("p"), "service:$(x)"),
            (Some("p"), "other"),
        ] {
            assert!(
                log_stream_args(&n, project, process).is_empty(),
                "{process}"
            );
        }
    }

    #[test]
    fn a_services_exit_is_asked_and_read() {
        assert_eq!(
            service_ps_args("clusia-acme-widgets-1a2b3c-7", "postgres"),
            [
                "compose",
                "-p",
                "clusia-acme-widgets-1a2b3c-7",
                "ps",
                "-a",
                "--format",
                "json",
                "postgres"
            ]
        );
        for bad in ["", "-x", "a b", "$(x)"] {
            assert!(service_ps_args("p", bad).is_empty(), "{bad}");
        }
        assert_eq!(
            parse_service_exit(r#"{"Service":"postgres","State":"exited","ExitCode":1}"#),
            Some(Some(1))
        );
        assert_eq!(
            parse_service_exit(r#"[{"Service":"postgres","State":"exited","ExitCode":137}]"#),
            Some(Some(137))
        );
        assert_eq!(
            parse_service_exit(r#"{"Service":"postgres","State":"exited"}"#),
            Some(None)
        );
        assert_eq!(
            parse_service_exit(r#"{"Service":"postgres","State":"running","ExitCode":0}"#),
            None
        );
        assert_eq!(parse_service_exit("not json"), None);
        assert_eq!(parse_service_exit(""), None);
    }

    #[test]
    fn the_log_ring_keeps_2000_clean_lines() {
        let mut ring = LogRing::new(2000);
        assert_eq!(ring.tail(10), (1, vec![]));
        assert_eq!(ring.last(), 0);
        let mut pushed = 0;
        for i in 1..=2500 {
            let lines = ring.push(format!("line {i}\n").as_bytes());
            assert_eq!(lines, vec![(i, format!("line {i}"))]);
            pushed += lines.len();
        }
        assert_eq!(pushed, 2500);
        assert_eq!(ring.last(), 2500);
        let (first, lines) = ring.tail(5000);
        assert_eq!((first, lines.len()), (501, 2000));
        assert_eq!(lines[0], "line 501");
        assert_eq!(
            ring.tail(2),
            (2499, vec!["line 2499".to_string(), "line 2500".to_string()])
        );

        // Each line is cleaned once, at the door.
        let mut ring = LogRing::new(2000);
        assert_eq!(
            ring.push(b"\x1b[31mred\x1b[0m\r\n"),
            vec![(1, "red".to_string())]
        );
        assert_eq!(
            ring.push(b"\x1b]0;title\x07tab\there\x00\n"),
            vec![(2, "tab\there".to_string())]
        );
        assert_eq!(
            ring.push(b"bidi \xe2\x80\xaeevil\n"),
            vec![(3, "bidi evil".to_string())]
        );
        assert_eq!(
            ring.push(b"bad \xff byte\n"),
            vec![(4, "bad \u{fffd} byte".to_string())]
        );
        assert_eq!(
            ring.push(b"\n"),
            vec![(5, String::new())],
            "a blank line is a line"
        );
        // A line split over pushes, and several lines in one.
        assert_eq!(ring.push(b"par"), vec![]);
        assert_eq!(
            ring.push(b"tial\nnext\nhalf"),
            vec![(6, "partial".to_string()), (7, "next".to_string())]
        );
        assert_eq!(ring.push(b"\n"), vec![(8, "half".to_string())]);
    }

    #[test]
    fn a_line_without_a_newline_is_capped_and_never_grows_without_bound() {
        let mut ring = LogRing::new(2000);
        for _ in 0..1000 {
            assert!(ring.push(&[b'a'; 1000]).is_empty());
        }
        let lines = ring.push(b"\nok\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].0, 1);
        assert_eq!(lines[0].1.chars().count(), 2000);
        assert!(lines[0].1.ends_with('…'));
        assert_eq!(
            lines[1],
            (2, "ok".to_string()),
            "the rest of the long line was dropped"
        );
        let exact = "b".repeat(2000);
        assert_eq!(
            ring.push(format!("{exact}\n").as_bytes()),
            vec![(3, exact)],
            "a line of exactly 2000 chars is whole"
        );
    }

    #[tokio::test]
    async fn remove_system_stops_removes_and_drops_the_networks() {
        let fake = FakeExec::new();
        answering(&fake);
        remove_system(&fake, &docker(), &names(false))
            .await
            .unwrap();
        assert_eq!(
            calls(&fake),
            vec![
                vec!["stop", "-t", "10", "clusia-system-acme-widgets-1a2b3c-7"],
                vec![
                    "rm",
                    "-f",
                    "clusia-system-acme-widgets-1a2b3c-7",
                    "clusia-system-acme-widgets-1a2b3c-7-fwd"
                ],
                vec!["network", "rm", "clusia-system-acme-widgets-1a2b3c-7-pub"],
                vec!["network", "rm", "clusia-system-acme-widgets-1a2b3c-7-net"],
            ]
        );
        // The stack's own network is never removed.
        let fake = FakeExec::new();
        answering(&fake);
        remove_system(&fake, &docker(), &names(true)).await.unwrap();
        let all = calls(&fake);
        assert_eq!(
            all.iter().filter(|c| c[..2] == ["network", "rm"]).count(),
            1
        );
        assert!(
            all.iter()
                .all(|c| !c.iter().any(|a| a.ends_with("_clusia_internal")))
        );
    }

    #[tokio::test]
    async fn remove_system_ignores_what_is_gone_and_runs_every_step_after_a_failure() {
        let fake = FakeExec::new();
        fake.on(
            "docker",
            &["stop"],
            FakeReply::fail(1, "Error response from daemon: No such container: x"),
        );
        fake.on(
            "docker",
            &["rm"],
            FakeReply::fail(1, "Error: No such container: x"),
        );
        fake.on(
            "docker",
            &["network", "rm"],
            FakeReply::fail(1, "Error response from daemon: network x not found"),
        );
        assert_eq!(remove_system(&fake, &docker(), &names(false)).await, Ok(()));
        assert_eq!(calls(&fake).len(), 4);

        let fake = FakeExec::new();
        fake.on("docker", &["stop"], FakeReply::fail(1, "permission denied"));
        fake.on("docker", &["rm"], FakeReply::ok(""));
        fake.on(
            "docker",
            &["network", "rm"],
            FakeReply::fail(1, "network has active endpoints"),
        );
        let err = remove_system(&fake, &docker(), &names(false))
            .await
            .unwrap_err();
        assert_eq!(calls(&fake).len(), 4, "every step ran");
        assert_eq!(
            err,
            "docker stop failed: permission denied; docker network failed: network has active endpoints; docker network failed: network has active endpoints"
        );
    }

    #[tokio::test]
    async fn a_real_failure_is_not_hidden_behind_a_gone_container() {
        let fake = FakeExec::new();
        fake.on("docker", &["stop"], FakeReply::ok(""));
        fake.on(
            "docker",
            &["rm"],
            FakeReply::fail(
                1,
                "Error: No such container: a\nError: removal of container b is already in progress",
            ),
        );
        fake.on("docker", &["network", "rm"], FakeReply::ok(""));
        let err = remove_system(&fake, &docker(), &names(false))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            "docker rm failed: Error: removal of container b is already in progress"
        );
    }

    #[test]
    fn the_objects_carry_the_system_kind_whatever_the_caller_built() {
        let mut other = job(names(false), &[8080], false);
        other.labels.kind = LabelKind::Run;
        for args in [
            system_create_args(&other, &mounts()),
            forwarder_create_args(&other),
        ] {
            let kinds: Vec<String> = values(&args, "--label")
                .into_iter()
                .filter(|l| l.starts_with("io.clusia.kind="))
                .collect();
            assert_eq!(kinds, ["io.clusia.kind=system"]);
        }
    }

    #[test]
    fn an_odd_address_is_shown_clean_and_short() {
        let long = "x".repeat(200);
        let err = parse_port_lines(&format!("80/tcp -> \u{1b}[31m{long}:1")).unwrap_err();
        assert!(!err.contains('\u{1b}'));
        assert!(err.len() < 200);
    }
}
