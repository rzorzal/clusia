//! `clusia tests`: the environment, the setup plan and the test runs of a review.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use clusia_core::printable::printable_lines;
use clusia_core::tests::{
    Approve, EngineKind, EnvState, KnownTest, RunOutcome, RunnerKind, SetupPlan, StepPlace,
    StepState, TestCaseResult, TestRun, TestScope, TestStatus, TestsPhase, TestsView,
};
use clusia_core::{Paths, PrRef};
use clusia_protocol::{Client, Command as Request, Event, Reply, topics};
use serde_json::json;
use tokio::signal::unix::{Signal, SignalKind, signal};

use crate::check::line_of;
use crate::review::parse_pr;
use crate::run::{CliError, Output, connect, unexpected};

/// How long a stop is waited for before the command gives up on the daemon.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// How long `run_tests` waits when no stop is pending: a year, since `Duration::MAX` overflows
/// the timer.
const NO_DEADLINE: Duration = Duration::from_secs(86_400 * 365);

/// What one invocation asks for; the steps run in this order.
pub(crate) struct Wanted {
    pub generate: bool,
    pub setup: bool,
    pub yes: bool,
    pub list: bool,
    pub run: Option<TestScope>,
}

pub(crate) fn duration_text(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", ms as f64 / 1_000.0)
    }
}

pub(crate) fn engine_label(kind: EngineKind) -> &'static str {
    match kind {
        EngineKind::Colima => "Colima",
        EngineKind::OrbStack => "OrbStack",
        EngineKind::DockerDesktop => "Docker Desktop",
        EngineKind::Podman => "Podman",
        EngineKind::Other => "container engine",
    }
}

pub(crate) fn runner_text(runner: RunnerKind, env: Option<&EnvState>) -> String {
    match runner {
        RunnerKind::Local => "local sandbox".into(),
        RunnerKind::Container => match env.and_then(|e| e.engine.as_ref()) {
            Some(engine) => format!("container · {}", engine_label(engine.kind)),
            None => "container".into(),
        },
    }
}

pub(crate) fn env_line(env: Option<&EnvState>) -> String {
    let Some(env) = env else {
        return "Environment not probed yet".into();
    };
    let Some(engine) = &env.engine else {
        return "No container engine · local sandbox".into();
    };
    let label = engine_label(engine.kind);
    if engine.running {
        return format!("Docker · {label} · running");
    }
    match &engine.start {
        Some(start) => format!(
            "Docker · {label} · stopped (start it with: {})",
            line_of(start)
        ),
        None => format!("Docker · {label} · stopped (start your container engine)"),
    }
}

pub(crate) fn phase_text(phase: &TestsPhase) -> String {
    match phase {
        TestsPhase::NotGenerated => "Not generated yet; run with --generate".into(),
        TestsPhase::Generating { activity: Some(a) } => format!("Generating: {}", line_of(a)),
        TestsPhase::Generating { activity: None } => "Generating".into(),
        TestsPhase::NeedsSetup => "Needs setup; run with --setup".into(),
        TestsPhase::Waiting => "Waiting for another test run".into(),
        TestsPhase::Preparing { step } => format!("Setting up: {}", line_of(step)),
        TestsPhase::Ready => "Ready".into(),
        TestsPhase::Running { run } => format!("Running {}", line_of(run)),
        TestsPhase::Failed { message } => format!("Failed: {}", line_of(message)),
    }
}

pub(crate) fn result_line(result: &TestCaseResult) -> String {
    let status = match result.status {
        TestStatus::Passed => "PASS",
        TestStatus::Failed => "FAIL",
        TestStatus::Skipped => "SKIP",
    };
    let mut line = format!("{status}  {}", line_of(&result.name));
    if let Some(ms) = result.duration_ms {
        line.push_str(&format!("  {}", duration_text(ms)));
    }
    line
}

pub(crate) fn totals_line(run: &TestRun, env: Option<&EnvState>) -> String {
    format!(
        "{} passed, {} failed, {} skipped in {} ({})",
        run.totals.passed,
        run.totals.failed,
        run.totals.skipped,
        duration_text(run.duration_ms),
        runner_text(run.runner, env)
    )
}

/// What a failed run shows: the command, where it ran, how it ended and the output.
pub(crate) fn failure_lines(run: &TestRun, env: Option<&EnvState>) -> Vec<String> {
    let code = match (run.exit_code, &run.outcome) {
        (Some(code), _) => code.to_string(),
        (None, RunOutcome::TimedOut) => "none (timed out)".into(),
        (None, RunOutcome::Stopped) => "none (stopped)".into(),
        (None, _) => "none".into(),
    };
    let mut lines = vec![
        format!("Command: {}", line_of(&run.command)),
        format!("Runner: {}", runner_text(run.runner, env)),
        format!("Exit code: {code}"),
    ];
    if let RunOutcome::Error { message } = &run.outcome {
        lines.push(format!("Error: {}", line_of(message)));
    }
    for result in &run.results {
        let (TestStatus::Failed, Some(message)) = (result.status, &result.message) else {
            continue;
        };
        lines.push(format!("FAIL  {}", line_of(&result.name)));
        lines.extend(printable_lines(message).lines().map(|l| format!("    {l}")));
    }
    if !run.tail.is_empty() {
        lines.push("Output:".into());
        lines.extend(printable_lines(&run.tail).lines().map(|l| format!("  {l}")));
    }
    lines
}

/// The code `clusia tests --run` ends with: 0 when it passed, the run's own code when it
/// failed, 124 for a timeout, 130 when stopped.
pub(crate) fn exit_code_of(run: &TestRun) -> u8 {
    match &run.outcome {
        RunOutcome::Passed => 0,
        RunOutcome::Failed => run
            .exit_code
            .and_then(|code| u8::try_from(code).ok())
            .filter(|code| *code != 0)
            .unwrap_or(1),
        RunOutcome::TimedOut => 124,
        RunOutcome::Stopped => 130,
        RunOutcome::Error { .. } => 1,
    }
}

/// A command as lines: `$` before the first, the rest lined up under it. Line breaks stay, so
/// what is approved is what runs.
fn command_lines(indent: &str, text: &str) -> Vec<String> {
    printable_lines(text)
        .lines()
        .enumerate()
        .map(|(i, l)| format!("{indent}{}{l}", if i == 0 { "$ " } else { "  " }))
        .collect()
}

pub(crate) fn plan_lines(plan: &SetupPlan) -> Vec<String> {
    const DETAIL: &str = "           ";
    let mut lines = Vec::new();
    if let Some(blocked) = &plan.blocked {
        lines.push(format!("Blocked: {}", line_of(blocked)));
    }
    lines.push("Setup plan:".into());
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
        lines.push(format!("  {mark:<8} {}  ({place})", line_of(&step.title)));
        lines.extend(command_lines(DETAIL, &step.command));
        if let Some(hint) = &step.hint {
            lines.push(format!("{DETAIL}{}", line_of(hint)));
        }
        match &step.state {
            StepState::Failed { message } => lines.push(format!("{DETAIL}{}", line_of(message))),
            StepState::Skipped { why } => lines.push(format!("{DETAIL}{}", line_of(why))),
            _ => {}
        }
    }
    if !plan.commands.is_empty() {
        lines.push(
            if plan.commands_approved {
                "Test commands (approved):"
            } else {
                "Test commands (they ran nothing yet):"
            }
            .into(),
        );
        for command in &plan.commands {
            let head = format!("  {}  ", line_of(&command.key));
            let mut shown = command_lines("", &command.command).into_iter();
            if let Some(first) = shown.next() {
                lines.push(format!("{head}{first}"));
            }
            let pad = " ".repeat(head.chars().count());
            lines.extend(shown.map(|l| format!("{pad}{l}")));
        }
    }
    lines
}

fn io_error(e: io::Error) -> CliError {
    CliError::Other(e.to_string())
}

/// What a Ctrl-C needs: where to reach the daemon again, which review, and whether a request
/// that starts an activity has gone out.
struct Interrupts<'a> {
    signal: Signal,
    paths: &'a Paths,
    home: Option<&'a Path>,
    pr: PrRef,
    acting: bool,
}

impl Interrupts<'_> {
    /// Ends the command for a Ctrl-C. Once an activity may have started, it is stopped over a
    /// fresh connection: the request the Ctrl-C cut short may still be answered on the old one.
    async fn end(&self, what: &str) -> ! {
        if !self.acting {
            eprintln!("Interrupted: nothing ran.");
            std::process::exit(130);
        }
        let stopped = tokio::time::timeout(STOP_WAIT, async {
            let mut client = connect(self.paths, self.home).await?;
            client
                .request(Request::StopTests {
                    pr: self.pr.clone(),
                })
                .await
                .map_err(CliError::from)
        })
        .await;
        if matches!(stopped, Ok(Ok(_))) {
            eprintln!("Stopped {what}.");
        } else {
            eprintln!("The daemon did not answer; {what} may still go on (Stop in the window).");
        }
        std::process::exit(130);
    }

    /// `request` on `client`, ended by a Ctrl-C. `acts` marks a request that starts an activity.
    async fn ask(
        &mut self,
        client: &mut Client,
        request: Request,
        acts: bool,
        what: &str,
    ) -> Result<Reply, CliError> {
        self.acting |= acts;
        tokio::select! {
            reply = client.request(request) => Ok(reply?),
            _ = self.signal.recv() => self.end(what).await,
        }
    }

    /// The next event, ended by a Ctrl-C.
    async fn event(&mut self, client: &mut Client, what: &str) -> Result<Event, CliError> {
        tokio::select! {
            event = client.next_event() => Ok(event?.1),
            _ = self.signal.recv() => self.end(what).await,
        }
    }
}

async fn view_of(
    client: &mut Client,
    pr: &PrRef,
    stop: &mut Interrupts<'_>,
) -> Result<TestsView, CliError> {
    match stop
        .ask(
            client,
            Request::GetTests { pr: pr.clone() },
            false,
            "reading the tests",
        )
        .await?
    {
        Reply::Tests(view) => Ok(*view),
        other => Err(unexpected(other)),
    }
}

fn generating_failed(message: &str) -> CliError {
    CliError::Other(format!("Generating the tests failed: {}", line_of(message)))
}

async fn generate(
    client: &mut Client,
    pr: &PrRef,
    json: bool,
    stop: &mut Interrupts<'_>,
) -> Result<TestsView, CliError> {
    stop.ask(
        client,
        Request::GenerateTests { pr: pr.clone() },
        true,
        "generating",
    )
    .await?;
    // What was told before the `Ack` may be the end itself. The daemon set the phase before it
    // answered, so the view read now is never the one from before the request.
    client.take_events();
    let now = view_of(client, pr, stop).await?;
    match &now.phase {
        TestsPhase::Generating { .. } => {}
        TestsPhase::Failed { message } => return Err(generating_failed(message)),
        _ => return Ok(now),
    }
    // Events told while the view was read are in the view.
    client.take_events();
    loop {
        let Event::TestEnv { pr: of, view } = stop.event(client, "generating").await? else {
            continue;
        };
        if of != *pr {
            continue;
        }
        match &view.phase {
            TestsPhase::Generating { .. } => {
                if !json {
                    eprintln!("{}", phase_text(&view.phase));
                }
            }
            TestsPhase::Failed { message } => return Err(generating_failed(message)),
            _ => return Ok(*view),
        }
    }
}

/// Asks at the terminal; anything but a yes, and the end of input, is a no. A line that comes
/// sooner than `QUIET_AFTER_PROMPT` after the question was typed before it, and is not an answer.
async fn approved_at_terminal(plan: &SetupPlan) -> Result<bool, CliError> {
    let steps = plan
        .steps
        .iter()
        .filter(|s| !matches!(s.state, StepState::Done | StepState::Skipped { .. }))
        .count();
    loop {
        eprint!("Approve all {steps} steps and the test commands? [y/N] ");
        let asked = Instant::now();
        let (read, line) = tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            io::stdin().read_line(&mut line).map(|read| (read, line))
        })
        .await
        .map_err(|e| CliError::Other(e.to_string()))?
        .map_err(io_error)?;
        if read == 0 {
            eprintln!();
            return Ok(false);
        }
        if asked.elapsed() < crate::agent::QUIET_AFTER_PROMPT {
            eprintln!("(A line typed before the question was ignored.)");
            continue;
        }
        return Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"));
    }
}

/// A step as the setup tells it: by the title the plan gave it, else by its id.
fn step_line(plan: &SetupPlan, step: &str, state: &StepState) -> Option<String> {
    let title = plan
        .steps
        .iter()
        .find(|s| s.id == step)
        .map_or_else(|| line_of(step), |s| line_of(&s.title));
    let said = match state {
        StepState::Missing => return None,
        StepState::Running => "running".to_string(),
        StepState::Done => "done".to_string(),
        StepState::Skipped { why } => format!("skipped: {}", line_of(why)),
        StepState::Failed { message } => format!("failed: {}", line_of(message)),
    };
    Some(format!("{title}: {said}"))
}

fn setting_up(phase: &TestsPhase) -> bool {
    matches!(phase, TestsPhase::Waiting | TestsPhase::Preparing { .. })
}

/// Whether a view read right after the approval is the end of the setup. A plan that still has a
/// step nobody approved is the view from before the approval landed.
fn setup_ended(view: &TestsView) -> bool {
    if setting_up(&view.phase) {
        return false;
    }
    let unapproved = view.plan.as_ref().is_some_and(|plan| {
        plan.steps
            .iter()
            .any(|s| !s.approved && !matches!(s.state, StepState::Done | StepState::Skipped { .. }))
    });
    !(matches!(view.phase, TestsPhase::NeedsSetup) && unapproved)
}

/// The step lines told before the `Ack`, and the setup's end when it was told then too.
fn told_before_ack(
    events: Vec<(String, Event)>,
    pr: &PrRef,
    plan: &SetupPlan,
) -> (Vec<String>, Option<TestsView>) {
    let mut lines = Vec::new();
    let mut last = None;
    for (_, event) in events {
        match event {
            Event::TestStep {
                pr: of,
                step,
                state,
                ..
            } if of == *pr => lines.extend(step_line(plan, &step, &state)),
            Event::TestEnv { pr: of, view } if of == *pr => last = Some(*view),
            _ => {}
        }
    }
    (lines, last.filter(|view| !setting_up(&view.phase)))
}

fn setup_failure(view: &TestsView) -> String {
    let failed = view
        .plan
        .iter()
        .flat_map(|p| &p.steps)
        .find_map(|s| match &s.state {
            StepState::Failed { message } => Some(line_of(message)),
            _ => None,
        });
    failed.unwrap_or_else(|| "a step still needs your approval".into())
}

async fn setup(
    client: &mut Client,
    pr: &PrRef,
    view: TestsView,
    yes: bool,
    stop: &mut Interrupts<'_>,
) -> Result<TestsView, CliError> {
    let Some(plan) = view.plan.clone() else {
        return Err(CliError::Other(format!(
            "There is no setup plan yet; run clusia tests {pr} --generate first"
        )));
    };
    if let Some(blocked) = &plan.blocked {
        return Err(CliError::Other(format!("Blocked: {}", line_of(blocked))));
    }
    if plan.ready && plan.commands_approved {
        eprintln!("Ready: nothing to set up.");
        return Ok(view);
    }
    for line in plan_lines(&plan) {
        eprintln!("{line}");
    }
    if !yes && !approved_at_terminal(&plan).await? {
        return Err(CliError::Exit {
            code: 1,
            message: "Setup not approved; nothing ran.".into(),
        });
    }
    let request = Request::PrepareTests {
        pr: pr.clone(),
        recipe_hash: plan.recipe_hash.clone(),
        approve: Approve::All,
    };
    stop.ask(client, request, true, "the setup").await?;
    let (lines, told_end) = told_before_ack(client.take_events(), pr, &plan);
    for line in lines {
        eprintln!("{line}");
    }
    // The view read now may already be the end; when it looks like the one from before the
    // approval, an end told before the `Ack` is the end, since no other will come.
    let now = Some(view_of(client, pr, stop).await?)
        .filter(setup_ended)
        .or(told_end);
    let ended = if now.is_some() {
        now
    } else {
        loop {
            match stop.event(client, "the setup").await? {
                Event::TestStep {
                    pr: of,
                    step,
                    state,
                    ..
                } if of == *pr => {
                    if let Some(line) = step_line(&plan, &step, &state) {
                        eprintln!("{line}");
                    }
                }
                Event::TestEnv { pr: of, view } if of == *pr && !setting_up(&view.phase) => {
                    break Some(*view);
                }
                _ => {}
            }
        }
    };
    match ended {
        Some(view) if matches!(view.phase, TestsPhase::Ready) => Ok(view),
        Some(view) => Err(CliError::Other(format!(
            "Setup did not finish: {}",
            setup_failure(&view)
        ))),
        None => unreachable!("the loop only leaves with a view"),
    }
}

async fn list(
    client: &mut Client,
    pr: &PrRef,
    stop: &mut Interrupts<'_>,
) -> Result<Vec<KnownTest>, CliError> {
    match stop
        .ask(
            client,
            Request::ListTests { pr: pr.clone() },
            true,
            "listing",
        )
        .await?
    {
        Reply::TestList(tests) => Ok(tests),
        other => Err(unexpected(other)),
    }
}

async fn run_tests(
    client: &mut Client,
    pr: &PrRef,
    scope: TestScope,
    json: bool,
    stop: &mut Interrupts<'_>,
) -> Result<TestRun, CliError> {
    let id = match stop
        .ask(
            client,
            Request::RunTests {
                pr: pr.clone(),
                scope,
            },
            true,
            "the run",
        )
        .await?
    {
        Reply::TestRunStarted { run } => run,
        other => return Err(unexpected(other)),
    };
    // After a Ctrl-C: the stop was asked for, and the run's end is awaited until then.
    let mut stopping: Option<Instant> = None;
    loop {
        let wait = stopping.map_or(NO_DEADLINE, |at| STOP_WAIT.saturating_sub(at.elapsed()));
        let event = tokio::select! {
            event = client.next_event() => event?.1,
            _ = stop.signal.recv() => {
                if stopping.is_some() {
                    eprintln!("The daemon did not answer; the run may still go on (Stop in the window).");
                    std::process::exit(130);
                }
                let asked = tokio::time::timeout(
                    STOP_WAIT,
                    client.request(Request::StopTests { pr: pr.clone() }),
                )
                .await;
                if !matches!(asked, Ok(Ok(_))) {
                    eprintln!("The daemon did not answer; the run may still go on (Stop in the window).");
                    std::process::exit(130);
                }
                stopping = Some(Instant::now());
                continue;
            }
            _ = tokio::time::sleep(wait) => {
                eprintln!("The daemon did not answer; the run may still go on (Stop in the window).");
                std::process::exit(130);
            }
        };
        match event {
            Event::TestResult {
                pr: of,
                run,
                result,
            } if of == *pr && run == id => {
                if !json {
                    println!("{}", result_line(&result));
                }
            }
            Event::TestRunDone { pr: of, run } if of == *pr && run.id == id => return Ok(*run),
            _ => {}
        }
    }
}

fn state_lines(view: &TestsView) -> Vec<String> {
    let mut lines = vec![env_line(view.env.as_ref()), phase_text(&view.phase)];
    if let Some(recipe) = &view.recipe
        && recipe.by_hand
    {
        lines.push("Recipe: .clusia/recipe.toml (edited by you)".into());
    }
    if view.proposal.is_some() {
        lines.push("A new recipe proposal waits; accept it in the window".into());
    }
    if view.refresh_available {
        lines.push("Refresh available: the repository's setup files changed".into());
    }
    if let Some(last) = view.runs.first() {
        let stale = if last.head == view.head {
            ""
        } else {
            "Stale · "
        };
        lines.push(format!(
            "{stale}Last run: {}",
            totals_line(last, view.env.as_ref())
        ));
    }
    for check in &view.github_checks {
        let state = check.conclusion.as_deref().unwrap_or(&check.status);
        lines.push(format!("{}  {}", line_of(state), line_of(&check.name)));
    }
    lines
}

pub(crate) async fn tests(
    paths: &Paths,
    home: Option<&Path>,
    pr: &str,
    wanted: Wanted,
    json: bool,
) -> Result<Output, CliError> {
    let pr = parse_pr(pr)?;
    // Caught from here on: a Ctrl-C before the first poll of a handler would end the process
    // without a word.
    let mut stop = Interrupts {
        signal: signal(SignalKind::interrupt()).map_err(io_error)?,
        paths,
        home,
        pr: pr.clone(),
        acting: false,
    };
    let mut client = connect(paths, home).await?;
    let acts = wanted.generate || wanted.setup || wanted.list || wanted.run.is_some();
    if acts {
        // The requests read the review's worktree, which opening prepares.
        let topics = vec![topics::TESTS.into()];
        stop.ask(
            &mut client,
            Request::Subscribe { topics },
            false,
            "subscribing",
        )
        .await?;
        stop.ask(
            &mut client,
            Request::OpenReview { pr: pr.clone() },
            false,
            "opening the review",
        )
        .await?;
    }
    let mut view = view_of(&mut client, &pr, &mut stop).await?;
    if !acts {
        return Ok(Output {
            human: state_lines(&view).join("\n"),
            json: json!({ "view": view }),
        });
    }
    if wanted.generate {
        view = generate(&mut client, &pr, json, &mut stop).await?;
    }
    if wanted.setup {
        view = setup(&mut client, &pr, view, wanted.yes, &mut stop).await?;
    }
    let mut human: Vec<String> = Vec::new();
    let mut object = serde_json::Map::new();
    if wanted.list {
        let tests = list(&mut client, &pr, &mut stop).await?;
        for test in &tests {
            let mut line = line_of(&test.name);
            if let Some(file) = &test.file {
                line.push_str(&format!("  {}", line_of(file)));
            }
            human.push(line);
        }
        object.insert("tests".into(), json!(tests));
    }
    let mut code = 0;
    let mut ended = None;
    if let Some(scope) = wanted.run {
        let run = run_tests(&mut client, &pr, scope, json, &mut stop).await?;
        view = view_of(&mut client, &pr, &mut stop).await?;
        if !matches!(run.outcome, RunOutcome::Passed) {
            human.extend(failure_lines(&run, view.env.as_ref()));
        }
        human.push(totals_line(&run, view.env.as_ref()));
        code = exit_code_of(&run);
        object.insert("run".into(), json!(run));
        ended = Some(run);
    }
    object.insert("view".into(), json!(view));
    let output = Output {
        human: human.join("\n"),
        json: serde_json::Value::Object(object),
    };
    let Some(run) = ended.filter(|_| code != 0) else {
        return Ok(output);
    };
    if json {
        println!("{}", output.json);
    } else {
        println!("{}", crate::agent::streamed(&output.human));
    }
    let message = match &run.outcome {
        RunOutcome::Failed => format!("tests failed (exit code {code})"),
        RunOutcome::TimedOut => "the run passed its time limit and was stopped".into(),
        RunOutcome::Stopped => "the run was stopped".into(),
        RunOutcome::Error { message } => line_of(message),
        RunOutcome::Passed => unreachable!("a passed run has exit code 0"),
    };
    Err(CliError::Exit { code, message })
}

#[cfg(test)]
mod tests {
    use clusia_core::tests::{
        Engine, EngineKind, EnvState, Existing, PlanCommand, PlanStep, RepoSignals, RunOutcome,
        RunnerKind, SetupPlan, StepPlace, StepState, TestCaseResult, TestRun, TestScope,
        TestStatus, TestsPhase, Totals,
    };

    use super::*;

    fn colima(running: bool) -> Engine {
        Engine {
            kind: EngineKind::Colima,
            cli: "/opt/homebrew/bin/docker".into(),
            endpoint: "unix:///Users/octo/.colima/default/docker.sock".into(),
            context: Some("colima".into()),
            running,
            start: Some("colima start".into()),
            compose: Some("2.29.1".into()),
        }
    }

    fn env(engine: Option<Engine>, runner: RunnerKind) -> EnvState {
        EnvState {
            head: "9be1d07".into(),
            probed_at: 1_761_600_000,
            engine,
            runner,
            note: None,
            sandbox_ok: true,
            signals: RepoSignals::default(),
            existing: Existing::default(),
            inputs_hash: "ab".into(),
        }
    }

    fn case(
        name: &str,
        status: TestStatus,
        ms: Option<u64>,
        message: Option<&str>,
    ) -> TestCaseResult {
        TestCaseResult {
            name: name.into(),
            file: None,
            status,
            duration_ms: ms,
            message: message.map(String::from),
        }
    }

    fn run(outcome: RunOutcome, exit_code: Option<i32>) -> TestRun {
        TestRun {
            id: "run-1".into(),
            head: "9be1d07".into(),
            scope: TestScope::All,
            runner: RunnerKind::Container,
            command: "cargo test --workspace".into(),
            started_at: 1_761_600_000,
            duration_ms: 4_200,
            exit_code,
            outcome,
            totals: Totals {
                passed: 3,
                failed: 1,
                skipped: 0,
            },
            results: vec![
                case("store::refresh", TestStatus::Passed, Some(12), None),
                case(
                    "store::expired\u{1b}[2J",
                    TestStatus::Failed,
                    None,
                    Some("assertion failed\u{202e}\nleft: 1"),
                ),
            ],
            log: Some("/tmp/run-1.log".into()),
            tail: "running 4 tests\n\u{1b}[31mFAILED\u{1b}[0m\n".into(),
        }
    }

    #[test]
    fn durations_read_in_ms_then_seconds() {
        assert_eq!(duration_text(0), "0 ms");
        assert_eq!(duration_text(12), "12 ms");
        assert_eq!(duration_text(999), "999 ms");
        assert_eq!(duration_text(1_000), "1.0 s");
        assert_eq!(duration_text(4_200), "4.2 s");
        assert_eq!(duration_text(61_049), "61.0 s");
    }

    #[test]
    fn a_result_is_one_filtered_line() {
        assert_eq!(
            result_line(&case("store::refresh", TestStatus::Passed, Some(12), None)),
            "PASS  store::refresh  12 ms"
        );
        assert_eq!(
            result_line(&case("store::expired", TestStatus::Failed, None, None)),
            "FAIL  store::expired"
        );
        assert_eq!(
            result_line(&case("slow", TestStatus::Skipped, Some(4_200), None)),
            "SKIP  slow  4.2 s"
        );
        let shown = result_line(&case(
            "a\u{1b}[2J\u{202e}b\nc",
            TestStatus::Passed,
            None,
            None,
        ));
        assert!(
            !shown.contains('\u{1b}') && !shown.contains('\u{202e}'),
            "{shown:?}"
        );
        assert!(!shown.contains('\n'), "{shown:?}");
    }

    #[test]
    fn the_runner_names_the_engine() {
        let container = env(Some(colima(true)), RunnerKind::Container);
        assert_eq!(
            runner_text(RunnerKind::Container, Some(&container)),
            "container · Colima"
        );
        assert_eq!(runner_text(RunnerKind::Container, None), "container");
        assert_eq!(
            runner_text(RunnerKind::Local, Some(&container)),
            "local sandbox"
        );
        assert_eq!(engine_label(EngineKind::DockerDesktop), "Docker Desktop");
        assert_eq!(engine_label(EngineKind::OrbStack), "OrbStack");
        assert_eq!(engine_label(EngineKind::Podman), "Podman");
        assert_eq!(engine_label(EngineKind::Other), "container engine");
    }

    #[test]
    fn the_environment_line_says_how_to_start_a_stopped_engine() {
        assert_eq!(
            env_line(Some(&env(Some(colima(true)), RunnerKind::Container))),
            "Docker · Colima · running"
        );
        assert_eq!(
            env_line(Some(&env(Some(colima(false)), RunnerKind::Container))),
            "Docker · Colima · stopped (start it with: colima start)"
        );
        assert_eq!(
            env_line(Some(&env(None, RunnerKind::Local))),
            "No container engine · local sandbox"
        );
        assert_eq!(env_line(None), "Environment not probed yet");
    }

    #[test]
    fn the_totals_line_ends_the_run() {
        let container = env(Some(colima(true)), RunnerKind::Container);
        assert_eq!(
            totals_line(&run(RunOutcome::Failed, Some(101)), Some(&container)),
            "3 passed, 1 failed, 0 skipped in 4.2 s (container · Colima)"
        );
    }

    #[test]
    fn a_failure_shows_command_runner_code_and_filtered_output() {
        let container = env(Some(colima(true)), RunnerKind::Container);
        let lines = failure_lines(&run(RunOutcome::Failed, Some(101)), Some(&container));
        assert_eq!(lines[0], "Command: cargo test --workspace");
        assert_eq!(lines[1], "Runner: container · Colima");
        assert_eq!(lines[2], "Exit code: 101");
        assert!(
            lines.contains(&"FAIL  store::expired\\u{1b}[2J".to_string()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&"    assertion failed\\u{202e}".to_string()),
            "{lines:?}"
        );
        assert!(lines.contains(&"    left: 1".to_string()), "{lines:?}");
        assert!(lines.contains(&"Output:".to_string()), "{lines:?}");
        assert!(
            lines.contains(&"  \\u{1b}[31mFAILED\\u{1b}[0m".to_string()),
            "{lines:?}"
        );
        for line in &lines {
            assert!(
                !line.contains('\u{1b}') && !line.contains('\u{202e}'),
                "{line:?}"
            );
        }
        let timed_out = failure_lines(&run(RunOutcome::TimedOut, None), None);
        assert_eq!(timed_out[2], "Exit code: none (timed out)");
        let stopped = failure_lines(&run(RunOutcome::Stopped, None), None);
        assert_eq!(stopped[2], "Exit code: none (stopped)");
        let broken = failure_lines(
            &run(
                RunOutcome::Error {
                    message: "docker vanished\u{1b}".into(),
                },
                None,
            ),
            None,
        );
        assert_eq!(broken[1], "Runner: container");
        assert!(
            broken.contains(&"Error: docker vanished\\u{1b}".to_string()),
            "{broken:?}"
        );
    }

    #[test]
    fn the_exit_code_follows_the_run() {
        assert_eq!(exit_code_of(&run(RunOutcome::Passed, Some(0))), 0);
        assert_eq!(exit_code_of(&run(RunOutcome::Failed, Some(101))), 101);
        assert_eq!(exit_code_of(&run(RunOutcome::Failed, Some(255))), 255);
        assert_eq!(
            exit_code_of(&run(RunOutcome::Failed, Some(0))),
            1,
            "a failed result list"
        );
        assert_eq!(exit_code_of(&run(RunOutcome::Failed, None)), 1);
        assert_eq!(exit_code_of(&run(RunOutcome::Failed, Some(-9))), 1);
        assert_eq!(exit_code_of(&run(RunOutcome::TimedOut, None)), 124);
        assert_eq!(exit_code_of(&run(RunOutcome::Stopped, None)), 130);
        assert_eq!(
            exit_code_of(&run(
                RunOutcome::Error {
                    message: "x".into()
                },
                None
            )),
            1
        );
    }

    #[test]
    fn the_phase_reads_as_a_sentence() {
        assert_eq!(
            phase_text(&TestsPhase::NotGenerated),
            "Not generated yet; run with --generate"
        );
        assert_eq!(
            phase_text(&TestsPhase::Generating {
                activity: Some("Probing the environment".into())
            }),
            "Generating: Probing the environment"
        );
        assert_eq!(
            phase_text(&TestsPhase::Generating { activity: None }),
            "Generating"
        );
        assert_eq!(
            phase_text(&TestsPhase::NeedsSetup),
            "Needs setup; run with --setup"
        );
        assert_eq!(
            phase_text(&TestsPhase::Waiting),
            "Waiting for another test run"
        );
        assert_eq!(
            phase_text(&TestsPhase::Preparing {
                step: "image".into()
            }),
            "Setting up: image"
        );
        assert_eq!(phase_text(&TestsPhase::Ready), "Ready");
        assert_eq!(
            phase_text(&TestsPhase::Running {
                run: "run-1".into()
            }),
            "Running run-1"
        );
        assert_eq!(
            phase_text(&TestsPhase::Failed {
                message: "no\u{1b}[2J recipe".into()
            }),
            "Failed: no\\u{1b}[2J recipe"
        );
    }

    #[test]
    fn an_end_told_before_the_ack_ends_the_setup() {
        let acme: PrRef = "acme/widgets#7".parse().unwrap();
        let other: PrRef = "acme/widgets#8".parse().unwrap();
        let plan = SetupPlan {
            recipe_hash: "ab".into(),
            steps: vec![PlanStep {
                id: "image".into(),
                title: "Build the image".into(),
                command: "docker build …".into(),
                place: StepPlace::Container,
                network: true,
                hint: None,
                state: StepState::Missing,
                approved: true,
            }],
            commands: vec![],
            commands_approved: true,
            ready: false,
            blocked: None,
        };
        let env_event = |pr: &PrRef, phase: TestsPhase| {
            let mut view = TestsView::new("9be1d07");
            view.phase = phase;
            (
                topics::TESTS.to_string(),
                Event::TestEnv {
                    pr: pr.clone(),
                    view: Box::new(view),
                },
            )
        };
        let step = (
            topics::TESTS.to_string(),
            Event::TestStep {
                pr: acme.clone(),
                step: "image".into(),
                state: StepState::Running,
                tail: None,
            },
        );
        let events = vec![
            env_event(&acme, TestsPhase::Waiting),
            step.clone(),
            env_event(&acme, TestsPhase::NeedsSetup),
            env_event(&other, TestsPhase::Waiting),
        ];
        let (lines, ended) = told_before_ack(events, &acme, &plan);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(ended.map(|v| v.phase), Some(TestsPhase::NeedsSetup));

        let still_going = vec![env_event(&acme, TestsPhase::Waiting), step];
        let (_, ended) = told_before_ack(still_going, &acme, &plan);
        assert_eq!(ended, None);
    }

    #[test]
    fn the_plan_shows_each_command_where_it_runs_and_marks_host_steps() {
        let plan = SetupPlan {
            recipe_hash: "ab".into(),
            steps: vec![
                PlanStep {
                    id: "engine".into(),
                    title: "Start Colima".into(),
                    command: "colima start".into(),
                    place: StepPlace::Host,
                    network: false,
                    hint: None,
                    state: StepState::Missing,
                    approved: false,
                },
                PlanStep {
                    id: "work:0".into(),
                    title: "Fetch dependencies".into(),
                    command: "cargo fetch\u{1b}[2J".into(),
                    place: StepPlace::Container,
                    network: true,
                    hint: Some("about 1 minute\u{202e}".into()),
                    state: StepState::Failed {
                        message: "exit 101".into(),
                    },
                    approved: true,
                },
                PlanStep {
                    id: "image".into(),
                    title: "Build the image".into(),
                    command: "docker build …".into(),
                    place: StepPlace::Container,
                    network: true,
                    hint: None,
                    state: StepState::Done,
                    approved: true,
                },
            ],
            commands: vec![
                PlanCommand {
                    key: "all".into(),
                    command: "cargo test --workspace".into(),
                },
                PlanCommand {
                    key: "one".into(),
                    command: "cargo test -- --exact {name}".into(),
                },
            ],
            commands_approved: false,
            ready: false,
            blocked: None,
        };
        assert_eq!(
            plan_lines(&plan),
            [
                "Setup plan:",
                "  todo     Start Colima  (on your Mac, outside any sandbox)",
                "           $ colima start",
                "  failed   Fetch dependencies  (container · network)",
                "           $ cargo fetch\\u{1b}[2J",
                "           about 1 minute\\u{202e}",
                "           exit 101",
                "  done     Build the image  (container · network)",
                "           $ docker build …",
                "Test commands (they ran nothing yet):",
                "  all  $ cargo test --workspace",
                "  one  $ cargo test -- --exact {name}",
            ]
        );
        let blocked = SetupPlan {
            blocked: Some(
                "This pull request ships its own .clusia/ folder; Clúsia won't use it.".into(),
            ),
            ..plan
        };
        assert_eq!(
            plan_lines(&blocked)[0],
            "Blocked: This pull request ships its own .clusia/ folder; Clúsia won't use it."
        );
        let hostile = SetupPlan {
            blocked: Some("Colima can't see /x\u{1b}[2J\u{202e}".into()),
            ..blocked
        };
        assert_eq!(
            plan_lines(&hostile)[0],
            "Blocked: Colima can't see /x\\u{1b}[2J\\u{202e}"
        );
    }
}
