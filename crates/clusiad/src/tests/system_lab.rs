//! Helpers for the system tests: a review whose recipe has `[system]`, a fake engine that
//! answers every system command, and a fake server on 127.0.0.1 for health checks and Send.

use std::time::Duration;

use clusia_core::Paths;
use clusia_core::cockpit::RunSide;
use clusia_core::system::{PortView, SystemLive, SystemState, SystemView};
use clusia_core::tests::{Approve, STEP_SYSTEM, STEP_TESTS, SetupPlan, TestsPhase};
use clusia_harness::testkit::{FakeClaude, Turn, endpoints_block, recipe_block};
use clusia_protocol::{Event, Outcome, Reply};
use clusia_testrun::exec::Stream;
use clusia_testrun::names::{compose_project, repo_slug};
use clusia_testrun::system::{forwarder_tag, system_names};
use clusia_testrun::testkit::{FakeExec, FakeReply};
use serde_json::json;
use tokio::sync::broadcast;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::functions_lab::index;
use super::lab::{self, Lab, Machine, approve, pr, settled};
use super::system::{SystemRun, system_view};
use super::*;

/// The `[system]` of the lab's recipe: two ports `api` 8080 and `admin` 9090, an HTTP health
/// check on `api`, a start command and one `env` entry.
pub(super) fn system_section() -> serde_json::Value {
    json!({
        "start": "cargo run -q --bin auth-api -- --port 8080",
        "ports": [
            {"name": "api", "port": 8080, "path": "/"},
            {"name": "admin", "port": 9090, "path": "/admin"}
        ],
        "health": {"port": "api", "path": "/health", "timeout_secs": 60},
        "env": {"RUST_LOG": "info"}
    })
}

/// The recipe of `lab::recipe()` with `system_section()`.
pub(super) fn system_recipe() -> serde_json::Value {
    let mut recipe = lab::recipe();
    recipe["system"] = system_section();
    recipe
}

/// `system_recipe()` with another start command, as a fix would write it.
pub(super) fn system_recipe_fixed() -> serde_json::Value {
    let mut recipe = system_recipe();
    recipe["system"]["start"] =
        json!("cargo run -q --bin auth-api -- --port 8080 --listen 0.0.0.0");
    recipe
}

/// The generate answer with `recipe` and, when given, the `clusia-endpoints` block.
pub(super) fn generate_answer_with(recipe: serde_json::Value, endpoints: Option<String>) -> Turn {
    let mut blocks = vec![recipe_block(recipe)];
    blocks.extend(endpoints);
    Turn::answer_blocks("Here is the recipe.", &blocks)
}

/// An endpoints turn answering `endpoints`.
pub(super) fn endpoints_turn(endpoints: Vec<serde_json::Value>) -> Turn {
    Turn::answer_blocks(
        "Here are the endpoints.",
        &[endpoints_block(json!({ "endpoints": endpoints }))],
    )
}

pub(super) fn valid_endpoint() -> serde_json::Value {
    json!({"method": "POST", "path": "/widgets", "port": "api", "why": "Creating a widget now checks its name."})
}

pub(super) fn endpoint_with_host_header() -> serde_json::Value {
    json!({"method": "GET", "path": "/widgets", "port": "api", "why": "Lists widgets.", "headers": {"host": "evil.example"}})
}

/// The system's base name of review `n`.
pub(super) fn system_base(n: u64) -> String {
    system_names(&repo_slug("acme", "widgets"), n, RunSide::Pr, None).base
}

/// The review's stack network.
pub(super) fn stack_net(n: u64) -> String {
    clusia_testrun::compose::internal_network(&compose_project(&repo_slug("acme", "widgets"), n))
}

pub(super) fn project(n: u64) -> String {
    compose_project(&repo_slug("acme", "widgets"), n)
}

/// Rules for `FakeExec`, added before the lab's own so they win: every system command
/// succeeds, `docker port` maps 8080 to `host_port` and 9090 to `host_port + 1`, the log
/// streams and `docker wait` hang until stopped, and the forwarder image exists.
pub(super) fn fake_system(exec: &FakeExec, host_port: u16) {
    let tag = forwarder_tag();
    exec.on(
        "docker",
        &["image", "inspect", "--format", "{{.Id}}", &tag],
        FakeReply::ok("sha256:2\n"),
    );
    fake_engine(exec, host_port);
}

/// `fake_system` without the forwarder image rule.
fn fake_engine(exec: &FakeExec, host_port: u16) {
    exec.on("docker", &["network", "create"], FakeReply::ok(""));
    exec.on("docker", &["network", "connect"], FakeReply::ok(""));
    exec.on("docker", &["network", "rm"], FakeReply::ok(""));
    exec.on("docker", &["volume", "create"], FakeReply::ok(""));
    exec.on("docker", &["create"], FakeReply::ok("c0ffee\n"));
    exec.on("docker", &["start"], FakeReply::ok(""));
    exec.on(
        "docker",
        &["port"],
        FakeReply::ok(format!(
            "8080/tcp -> 127.0.0.1:{host_port}\n9090/tcp -> 127.0.0.1:{}\n",
            host_port + 1
        )),
    );
    exec.on("docker", &["logs"], FakeReply::hang());
    exec.on("docker", &["wait"], FakeReply::hang());
    exec.on("docker", &["exec"], FakeReply::ok(""));
    exec.on("docker", &["stop"], FakeReply::ok(""));
    exec.on("docker", &["rm"], FakeReply::ok(""));
    exec.on("docker", &["build"], FakeReply::ok(""));
}

/// A wiremock server on 127.0.0.1 answering `GET /health` with 200.
pub(super) async fn mock_api() -> MockServer {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/health"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&api)
        .await;
    api
}

/// How a system lab is built.
pub(super) struct Spec {
    pub recipe: serde_json::Value,
    pub endpoints: Option<String>,
    pub turns: Vec<Turn>,
    pub reviews: Vec<u64>,
    /// Generated and set up, with the system approved.
    pub prepare: bool,
    /// The forwarder image exists from the start.
    pub forwarder: bool,
}

impl Spec {
    pub(super) fn new(recipe: serde_json::Value) -> Self {
        Self {
            recipe,
            endpoints: None,
            turns: Vec::new(),
            reviews: vec![7],
            prepare: true,
            forwarder: true,
        }
    }
}

/// A system lab as `spec` says; `script` adds rules before the lab's own, so they win.
pub(super) async fn build(spec: Spec, script: impl FnOnce(&FakeExec, &Paths)) -> (Lab, MockServer) {
    let api = mock_api().await;
    let host_port = api.address().port();
    let mut turns: Vec<Turn> = spec
        .reviews
        .iter()
        .map(|_| generate_answer_with(spec.recipe.clone(), spec.endpoints.clone()))
        .collect();
    turns.extend(spec.turns);
    let forwarder = spec.forwarder;
    let lab = lab::lab(
        Machine::Colima { running: true },
        turns,
        &spec.reviews,
        |x, paths| {
            script(x, paths);
            if !forwarder {
                x.on(
                    "docker",
                    &["image", "inspect", "--format", "{{.Id}}", &forwarder_tag()],
                    FakeReply::fail(1, "Error: No such image"),
                );
            }
            fake_system(x, host_port);
            lab::built(x);
            x.on("docker", &["run"], FakeReply::ok(""));
        },
    );
    for &n in &spec.reviews {
        index(&lab, n, "h1", super::functions_lab::REFRESH_SIG);
        if spec.prepare {
            lab::generated(&lab, n).await;
            approve_system(&lab, n).await;
        }
    }
    (lab, api)
}

/// Approves every step of the plan, the tests, the system and its steps, and waits for the
/// setup to end with the plan ready.
pub(super) async fn approve_system(lab: &Lab, n: u64) {
    let plan = plan_of_n(lab, n).await;
    let mut ids: Vec<String> = plan.steps.iter().map(|s| s.id.clone()).collect();
    if let Some(system) = &plan.system {
        ids.extend(system.steps.iter().map(|s| s.id.clone()));
    }
    ids.extend([STEP_TESTS.to_string(), STEP_SYSTEM.to_string()]);
    assert!(matches!(
        approve(lab, n, Approve::Steps(ids)).await,
        Outcome::Ok(Reply::Ack)
    ));
    let view = settled(lab, n).await;
    assert_eq!(view.phase, TestsPhase::Ready, "{view:#?}");
}

/// Review 7 with `system_recipe()`, generated, set up and approved.
pub(super) async fn system_lab(turns: Vec<Turn>) -> (Lab, MockServer) {
    let mut spec = Spec::new(system_recipe());
    spec.turns = turns;
    build(spec, |_, _| {}).await
}

/// `system_lab` with the `clusia-endpoints` block `endpoints` in the generate answer.
pub(super) async fn system_lab_with_endpoints(endpoints: serde_json::Value) -> (Lab, MockServer) {
    let mut spec = Spec::new(system_recipe());
    spec.endpoints = Some(endpoints_block(endpoints));
    build(spec, |_, _| {}).await
}

/// `system_recipe()` with no health check.
pub(super) fn no_health_recipe() -> serde_json::Value {
    let mut recipe = system_recipe();
    recipe["system"].as_object_mut().unwrap().remove("health");
    recipe
}

/// As `system_lab`, with a recipe that has no `health`: Running as soon as the ports are
/// mapped, and no HTTP at all.
pub(super) async fn system_lab_without_health() -> (Lab, MockServer) {
    system_lab_without_health_with(vec![], |_| {}).await
}

pub(super) async fn system_lab_without_health_with(
    turns: Vec<Turn>,
    script: impl FnOnce(&FakeExec),
) -> (Lab, MockServer) {
    let mut spec = Spec::new(no_health_recipe());
    spec.turns = turns;
    build(spec, |x, _| script(x)).await
}

/// `system_lab` whose rules `script` adds first.
pub(super) async fn system_lab_with(
    turns: Vec<Turn>,
    script: impl FnOnce(&FakeExec),
) -> (Lab, MockServer) {
    let mut spec = Spec::new(system_recipe());
    spec.turns = turns;
    build(spec, |x, _| script(x)).await
}

/// Writes `system_recipe()` as review `n`'s recipe file, Clúsia's own.
pub(super) fn with_system_recipe(lab: &Lab, n: u64) {
    let recipe = clusia_testrun::recipe::Recipe::from_json(&system_recipe()).unwrap();
    write_recipe(lab, n, &recipe);
}

/// Writes `recipe` as review `n`'s recipe file and records it as Clúsia's.
pub(super) fn write_recipe(lab: &Lab, n: u64, recipe: &clusia_testrun::recipe::Recipe) {
    let hash = super::recipe_file::write_recipe(&lab.worktree(n), &recipe.to_toml()).unwrap();
    let mut record = env::load(&lab.shared, &pr(n));
    record.written_hash = Some(hash);
    env::save(&lab.shared, &pr(n), &record);
    lab.shared.tests.with(&pr(n), |entry| entry.facts = None);
}

/// Changes review `n`'s recipe file through `change` of its JSON form.
pub(super) fn edit_recipe(lab: &Lab, n: u64, change: impl FnOnce(&mut serde_json::Value)) {
    let text = std::fs::read_to_string(lab.recipe_file(n)).unwrap();
    let recipe = clusia_testrun::recipe::Recipe::parse_toml(&text).unwrap();
    let mut json = recipe_json(&recipe);
    change(&mut json);
    let recipe = clusia_testrun::recipe::Recipe::from_json(&json).unwrap();
    write_recipe(lab, n, &recipe);
}

/// The JSON form of a recipe the lab wrote: the lab's recipes, matched by their start.
fn recipe_json(recipe: &clusia_testrun::recipe::Recipe) -> serde_json::Value {
    let mut json = system_recipe();
    match &recipe.system {
        Some(system) => {
            json["system"]["start"] = json!(system.start);
            if system.health.is_none() {
                json["system"].as_object_mut().unwrap().remove("health");
            }
        }
        None => {
            json.as_object_mut().unwrap().remove("system");
        }
    }
    if recipe.runner == clusia_core::tests::RunnerChoice::Local {
        json["runner"] = json!("local");
    }
    json
}

pub(super) fn edit_recipe_start(lab: &Lab, n: u64, start: &str) {
    edit_recipe(lab, n, |json| json["system"]["start"] = json!(start));
}

pub(super) fn set_recipe_without_system(lab: &Lab, n: u64) {
    edit_recipe(lab, n, |json| {
        json.as_object_mut().unwrap().remove("system");
    });
}

/// Writes `system_recipe()` with the local runner.
pub(super) fn set_recipe_runner_local(lab: &Lab, n: u64) {
    let mut json = system_recipe();
    json["runner"] = json!("local");
    let recipe = clusia_testrun::recipe::Recipe::from_json(&json).unwrap();
    write_recipe(lab, n, &recipe);
}

/// The plan of review `n` as a client sees it now.
pub(super) async fn plan_of_n(lab: &Lab, n: u64) -> SetupPlan {
    env::view(&lab.shared, &pr(n)).await.plan.expect("a plan")
}

pub(super) async fn plan_of(lab: &Lab, n: u64) -> SetupPlan {
    plan_of_n(lab, n).await
}

pub(super) fn system_of(out: Outcome) -> SystemView {
    match out {
        Outcome::Ok(Reply::System(view)) => *view,
        other => panic!("not a system view: {other:?}"),
    }
}

/// Starts review 7's system with the plan's recipe hash.
pub(super) async fn start(lab: &Lab, keep: bool) -> SystemView {
    start_n(lab, 7, keep).await
}

pub(super) async fn start_n(lab: &Lab, n: u64, keep: bool) -> SystemView {
    let hash = plan_of_n(lab, n).await.recipe_hash;
    system_of(start_system(&lab.shared, &pr(n), RunSide::Pr, &hash, keep).await)
}

/// Polls review `n`'s view until `pred` holds, and panics with the last view after a while.
/// Paused-clock friendly: it yields and sleeps on the tokio clock only.
pub(super) async fn until_state_n(
    lab: &Lab,
    n: u64,
    pred: impl Fn(&SystemView) -> bool,
) -> SystemView {
    let mut last = None;
    for _ in 0..400 {
        let view = system_view(&lab.shared, &pr(n)).await;
        if pred(&view) {
            return view;
        }
        last = Some(view);
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the system never got there: {last:#?}");
}

pub(super) async fn until_state(lab: &Lab, pred: impl Fn(&SystemView) -> bool) -> SystemView {
    until_state_n(lab, 7, pred).await
}

fn docker_args(lab: &Lab) -> Vec<Vec<String>> {
    lab.exec
        .calls()
        .iter()
        .filter(|c| c.program.to_string_lossy().ends_with("docker"))
        .map(lab::args_of)
        .collect()
}

/// The `docker` calls whose first argument is `verb`, as argument lists (an empty `verb`
/// gives every call).
pub(super) fn docker_calls(lab: &Lab, verb: &str) -> Vec<Vec<String>> {
    docker_args(lab)
        .into_iter()
        .filter(|a| verb.is_empty() || a.first().is_some_and(|v| v == verb))
        .collect()
}

/// `docker_calls`, from the first `network create` on: the pre-clean of a start is not
/// counted.
pub(super) fn docker_calls_after_create(lab: &Lab, verb: &str) -> Vec<Vec<String>> {
    let all = docker_args(lab);
    let from = all
        .iter()
        .position(|a| a.starts_with(&["network".to_string(), "create".to_string()]))
        .unwrap_or(all.len());
    all.into_iter()
        .skip(from)
        .filter(|a| verb.is_empty() || a.first().is_some_and(|v| v == verb))
        .collect()
}

/// The verb of a `docker` line of `lab.ran()`.
pub(super) fn docker_verb(line: &str) -> Option<String> {
    let (program, rest) = line.split_once(' ')?;
    program
        .ends_with("docker")
        .then(|| rest.split(' ').next().unwrap_or("").to_string())
}

/// Each `wanted` text is a prefix of a call that comes after the previous one's.
pub(super) fn assert_in_order(calls: &[String], wanted: &[&str]) {
    let mut at = 0;
    for want in wanted {
        match calls[at..].iter().position(|c| c.starts_with(want)) {
            Some(found) => at += found + 1,
            None => panic!("{want:?} does not follow in {calls:#?}"),
        }
    }
}

/// A `SystemRun` of review `n` in `Running` with these mapped host ports.
pub(super) fn store_live_system(lab: &Lab, n: u64, ports: &[u16]) {
    let names = system_names(&repo_slug("acme", "widgets"), n, RunSide::Pr, None);
    let mut run = SystemRun::new(
        u64::from(lab.shared.tests.seq()),
        names,
        "h1".into(),
        "x".into(),
        false,
    );
    run.state = SystemState::Running;
    run.ports = ports
        .iter()
        .enumerate()
        .map(|(at, host)| PortView {
            name: format!("p{at}"),
            port: 8080 + at as u16,
            path: "/".into(),
            host_port: Some(*host),
            url: Some(format!("http://127.0.0.1:{host}/")),
        })
        .collect();
    lab.shared
        .tests
        .with(&pr(n), |entry| entry.system = Some(run));
}

pub(super) fn stop_live_system(lab: &Lab, n: u64) {
    lab.shared.tests.with(&pr(n), |entry| {
        if let Some(run) = entry.system.as_mut() {
            run.state = SystemState::Stopped {
                why: clusia_core::system::StopWhy::Reviewer,
            };
        }
    });
}

/// Fires the run stop of review `n`.
pub(super) fn fire_run_stop(lab: &Lab, n: u64) {
    lab.shared.tests.with(&pr(n), |entry| {
        if let Some(run) = &entry.system {
            run.stop.fire();
        }
    });
}

/// Holds review `n`'s slot until dropped.
pub(super) struct Slot {
    shared: Arc<Shared>,
    n: u64,
    id: u64,
    _live: Live,
}

impl Drop for Slot {
    fn drop(&mut self) {
        end(&self.shared, &pr(self.n), self.id, None);
    }
}

pub(super) fn hold_slot(lab: &Lab, n: u64, doing: Doing) -> Slot {
    let phase = TestsPhase::Running { run: "x".into() };
    let started = match begin(&lab.shared, &pr(n), doing, phase) {
        Ok(started) => started,
        Err(_) => panic!("the slot is taken"),
    };
    Slot {
        shared: lab.shared.clone(),
        n,
        id: started.id,
        _live: started.live,
    }
}

/// A new receiver of every event from now on.
pub(super) fn events(lab: &Lab) -> broadcast::Receiver<(String, Event)> {
    lab.shared.events.subscribe()
}

/// The `SystemChanged` views received so far.
pub(super) fn system_changed(events: &mut broadcast::Receiver<(String, Event)>) -> Vec<SystemView> {
    drain(events)
        .into_iter()
        .filter_map(|e| match e {
            Event::SystemChanged { view, .. } => Some(*view),
            _ => None,
        })
        .collect()
}

/// A `SystemLogs` batch.
#[derive(Debug)]
pub(super) struct Batch {
    pub process: String,
    pub first: u64,
    pub lines: Vec<String>,
    pub skipped: u32,
}

pub(super) fn system_logs_events(events: &mut broadcast::Receiver<(String, Event)>) -> Vec<Batch> {
    drain(events)
        .into_iter()
        .filter_map(|e| match e {
            Event::SystemLogs {
                process,
                first,
                lines,
                skipped,
                ..
            } => Some(Batch {
                process,
                first,
                lines,
                skipped,
            }),
            _ => None,
        })
        .collect()
}

pub(super) fn drain(events: &mut broadcast::Receiver<(String, Event)>) -> Vec<Event> {
    let mut seen = Vec::new();
    while let Ok((_, event)) = events.try_recv() {
        seen.push(event);
    }
    seen
}

/// The prompt of the last agent turn.
pub(super) fn last_prompt(lab: &Lab) -> String {
    let calls = FakeClaude::calls(lab.fake.path());
    let argv = &calls.last().expect("a turn ran").argv;
    argv[argv.iter().position(|a| a == "-p").unwrap() + 1].clone()
}

/// Stops review `n`'s system and waits until it is stopped.
pub(super) async fn stop_and_wait(lab: &Lab, n: u64) {
    stop_system(&lab.shared, &pr(n), RunSide::Pr).await;
    until_state_n(lab, n, |v| matches!(v.state, SystemState::Stopped { .. })).await;
}

/// Writes a record whose system was live in a daemon that is gone.
pub(super) fn set_record_live(lab: &Lab, n: u64, head: &str) {
    let mut record = env::load(&lab.shared, &pr(n));
    record.system.live = Some(SystemLive {
        head: head.into(),
        at: 1_762_300_000,
        keep: false,
    });
    env::save(&lab.shared, &pr(n), &record);
}

/// Sets review `n`'s head.
pub(super) fn set_head(lab: &Lab, n: u64, head: &str) {
    let mut review = crate::reviews::load_stored(&lab.shared, &pr(n))
        .unwrap()
        .unwrap();
    review.head_sha = head.into();
    clusia_store::save_review(&lab.shared.paths, &review).unwrap();
}

/// The lines a `logs` reply sends, then ends.
pub(super) fn log_reply(text: &str) -> FakeReply {
    FakeReply::chunks(vec![(Stream::Out, text.to_string())], 0)
}

/// The stored endpoints the endpoints tests start from: two on `api`.
pub(super) fn stored() -> clusia_core::system::StoredEndpoints {
    use clusia_core::system::{Endpoint, HttpMethod};
    use std::collections::BTreeMap;
    clusia_core::system::StoredEndpoints {
        written_head: "a".repeat(40),
        written_at: 1_762_300_000,
        endpoints: vec![
            Endpoint {
                method: HttpMethod::Post,
                path: "/oauth/token".into(),
                port: "api".into(),
                why: "Refresh goes through the shared queue.".into(),
                new: false,
                headers: BTreeMap::from([(
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded".to_string(),
                )]),
                body: Some("grant_type=refresh_token&refresh_token=fake_rt_7c2e".into()),
                params: BTreeMap::new(),
            },
            Endpoint {
                method: HttpMethod::Delete,
                path: "/oauth/token/{id}".into(),
                port: "api".into(),
                why: "Revoking drops queued waiters.".into(),
                new: false,
                headers: BTreeMap::new(),
                body: None,
                params: BTreeMap::from([("id".to_string(), "fake_tok_1".to_string())]),
            },
        ],
    }
}

/// The compose services of the services labs: one `postgres`.
pub(super) fn with_services(mut recipe: serde_json::Value) -> serde_json::Value {
    recipe["services"] = json!({ "compose": "docker-compose.yml", "names": ["postgres"] });
    recipe
}

const MERGED: &str = r#"{"name":"x","services":{"postgres":{"image":"postgres:16"}}}"#;
pub(super) const PS_RUNNING: &str = r#"{"Service":"postgres","State":"running","ExitCode":0}"#;
pub(super) const PS_EXITED: &str = r#"{"Service":"postgres","State":"exited","ExitCode":1}"#;

/// How many compose calls on the stack's files the setup of a services lab makes.
const SETUP_COMPOSE_CALLS: usize = 4;

/// A services lab: `ps` answers each of `ps` in turn, the service's logs answer `logs`, and the
/// compose calls on the stack's files answer `stack` in turn.
async fn services_lab(
    recipe: serde_json::Value,
    ps: Vec<FakeReply>,
    logs: FakeReply,
    stack: Vec<FakeReply>,
) -> (Lab, MockServer) {
    let project = project(7);
    build(Spec::new(with_services(recipe)), move |x, paths| {
        let file = paths.worktree_for(&pr(7)).join("docker-compose.yml");
        std::fs::write(file, "services:\n  postgres:\n    image: postgres:16\n").unwrap();
        x.on_sequence("docker", &["compose", "-p", &project, "ps"], ps);
        x.on("docker", &["compose", "-p", &project, "logs"], logs);
        x.on_sequence(
            "docker",
            &["compose", "-p", &project, "--project-directory"],
            stack,
        );
    })
    .await
}

/// A lab whose recipe has `[services]`, its stack up and its service running.
pub(super) async fn system_lab_with_services(_turns: Vec<Turn>) -> (Lab, MockServer) {
    services_lab(
        system_recipe(),
        vec![FakeReply::ok(PS_RUNNING)],
        FakeReply::hang(),
        vec![FakeReply::ok(MERGED)],
    )
    .await
}

/// As `system_lab_with_services`, built without health.
pub(super) async fn system_lab_with_services_up() -> (Lab, MockServer) {
    services_lab(
        no_health_recipe(),
        vec![FakeReply::ok(PS_RUNNING)],
        FakeReply::hang(),
        vec![FakeReply::ok(MERGED)],
    )
    .await
}

/// A services lab whose service is not running at the start, and whose `compose up` fails.
pub(super) async fn system_lab_with_services_failing() -> (Lab, MockServer) {
    let mut stack = vec![FakeReply::ok(MERGED); SETUP_COMPOSE_CALLS];
    stack.push(FakeReply::fail(
        1,
        "dependency failed to start: container postgres is unhealthy",
    ));
    services_lab(
        no_health_recipe(),
        vec![FakeReply::ok(PS_EXITED)],
        FakeReply::hang(),
        stack,
    )
    .await
}

/// A services lab, built without health, whose service's log ends once the system runs and
/// whose `ps` then says it exited with `code`.
pub(super) async fn system_lab_with_services_log_end(
    service: &str,
    code: i32,
) -> (Lab, MockServer) {
    assert_eq!(service, "postgres");
    let exited = format!(r#"{{"Service":"postgres","State":"exited","ExitCode":{code}}}"#);
    services_lab(
        no_health_recipe(),
        vec![FakeReply::ok(PS_RUNNING), FakeReply::ok(exited)],
        log_reply("database system is ready\n"),
        vec![FakeReply::ok(MERGED)],
    )
    .await
}

/// `system_recipe()` whose tests have `tests_env` and whose system has `system_env`.
pub(super) async fn system_lab_with_env(
    tests_env: &[(&str, &str)],
    system_env: &[(&str, &str)],
) -> (Lab, MockServer) {
    let mut recipe = system_recipe();
    recipe["tests"]["env"] = json!(
        tests_env
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect::<serde_json::Map<_, _>>()
    );
    recipe["system"]["env"] = json!(
        system_env
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect::<serde_json::Map<_, _>>()
    );
    build(Spec::new(recipe), |_, _| {}).await
}

/// A recipe whose health is `command`, answered by `replies` in turn.
pub(super) async fn system_lab_with_health_command(
    command: &str,
    replies: Vec<FakeReply>,
) -> (Lab, MockServer) {
    let mut recipe = system_recipe();
    recipe["system"]["health"] = json!({"command": command, "timeout_secs": 60});
    build(Spec::new(recipe), move |x, _| {
        x.on_sequence("docker", &["exec"], replies);
    })
    .await
}

/// A recipe whose health check never passes: the API answers 503, with `timeout_secs`.
pub(super) async fn system_lab_always_503(timeout_secs: u32) -> (Lab, MockServer) {
    system_lab_always_503_with_turns(timeout_secs, vec![]).await
}

pub(super) async fn system_lab_always_503_with_turns(
    timeout_secs: u32,
    turns: Vec<Turn>,
) -> (Lab, MockServer) {
    let mut recipe = system_recipe();
    recipe["system"]["health"]["timeout_secs"] = json!(timeout_secs);
    let mut spec = Spec::new(recipe);
    spec.turns = turns;
    let (lab, api) = build(spec, |_, _| {}).await;
    api.reset().await;
    Mock::given(path("/health"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&api)
        .await;
    (lab, api)
}

/// A lab, built without health, whose start command logs one line 100 ms in.
pub(super) async fn system_lab_with_a_chatty_log() -> (Lab, MockServer) {
    system_lab_without_health_with(vec![], |x| {
        x.on(
            "docker",
            &["logs"],
            FakeReply::after(Duration::from_millis(100), log_reply("tick\n")),
        );
    })
    .await
}

/// Review 7 generated with `system_recipe()`, not set up, and no forwarder image yet.
pub(super) async fn system_lab_unprepared() -> Lab {
    let mut spec = Spec::new(system_recipe());
    spec.prepare = false;
    spec.forwarder = false;
    let (lab, _api) = build(spec, |_, _| {}).await;
    lab::generated(&lab, 7).await;
    lab
}

/// Review 7 generated with a recipe that has no `[system]`.
pub(super) async fn generated_without_system_lab() -> Lab {
    let mut spec = Spec::new(lab::recipe());
    spec.prepare = false;
    let (lab, _api) = build(spec, |_, _| {}).await;
    lab::generated(&lab, 7).await;
    lab
}

/// Reviews `reviews`, each generated, set up and approved.
pub(super) async fn system_lab_with_reviews(reviews: &[u64]) -> (Lab, MockServer) {
    let mut spec = Spec::new(no_health_recipe());
    spec.reviews = reviews.to_vec();
    build(spec, |_, _| {}).await
}

/// Review 7 whose pull request ships its own `.clusia/`.
pub(super) async fn lab_with_shipped_clusia(n: u64) -> Lab {
    let mut spec = Spec::new(system_recipe());
    spec.prepare = false;
    spec.reviews = vec![n];
    let (lab, _api) = build(spec, move |x, paths| {
        x.on(
            "git",
            &["-C", super::lab::wt(paths, n).as_str()],
            FakeReply::ok(".clusia/recipe.toml\0"),
        );
    })
    .await;
    lab
}

/// A lab, built without health, whose `docker start` fails with `log` as its output.
pub(super) async fn system_lab_failing_start(log: &str, turns: Vec<Turn>) -> (Lab, MockServer) {
    let log = log.to_string();
    system_lab_without_health_with(turns, move |x| {
        x.on("docker", &["start"], FakeReply::fail(1, log));
    })
    .await
}

/// Takes `STEP_SYSTEM` out of review `n`'s stored approval.
pub(super) fn unapprove_system(lab: &Lab, n: u64) {
    let mut record = env::load(&lab.shared, &pr(n));
    if let Some(approval) = record.approval.as_mut() {
        approval.steps.retain(|s| s != STEP_SYSTEM);
    }
    env::save(&lab.shared, &pr(n), &record);
    lab.shared.tests.with(&pr(n), |entry| entry.facts = None);
}

/// Waits until review 7's endpoints turn ends.
pub(super) async fn settle_endpoints(lab: &Lab) {
    settled(lab, 7).await;
}

/// The compose verbs run on the stack's files (`up`, `down`, …).
pub(super) fn compose_verbs(lab: &Lab) -> Vec<String> {
    docker_calls(lab, "compose")
        .into_iter()
        .filter_map(|a| {
            let at = a.iter().rposition(|w| w.starts_with('/') || w == "-p")?;
            let rest = &a[at + 1..];
            if a.get(at).is_some_and(|w| w == "-p") {
                // `compose -p <project> <verb>`
                rest.get(1).cloned()
            } else {
                rest.first().cloned()
            }
        })
        .collect()
}

/// `until_state` for a wait of several real seconds (a health timeout).
pub(super) async fn until_state_slow(lab: &Lab, pred: impl Fn(&SystemView) -> bool) -> SystemView {
    for _ in 0..40 {
        let view = system_view(&lab.shared, &pr(7)).await;
        if pred(&view) {
            return view;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    until_state(lab, pred).await
}
