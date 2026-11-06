//! `clusia tests` for the QA cockpit: scenarios, benchmarks, main and the QA copilot.

use clusia_core::PrRef;
use clusia_core::cockpit::*;
use clusia_core::functions::valid_slug;
use clusia_core::printable::{printable, printable_lines};
use clusia_core::tests::RunnerKind;
use clusia_protocol::{Client, ClientError, Command as Request, Event, Reply};
use serde_json::json;

use crate::cases_cmd::{pick_function, read_functions};
use crate::check::line_of;
use crate::run::{CliError, unexpected};
use crate::tests_cmd::{
    Interrupts, approval_of, confirm, duration_text, follow, plan_lines, runner_text,
};

/// How many lines of a failed round's output are shown.
const TAIL_LINES: usize = 20;

/// The cockpit flags of one `clusia tests` call, as clap read them.
#[derive(Debug, Default, Clone)]
pub(crate) struct CockpitFlags {
    pub scenarios: bool,
    pub write_scenarios: bool,
    pub describe: Option<String>,
    pub approve_scenario: Option<String>,
    pub run_scenarios: bool,
    pub scenario: Option<String>,
    pub on_main: bool,
    pub bench: Option<String>,
    pub write: bool,
    pub copilot: Option<String>,
    pub as_comment: Option<String>,
    pub yes: bool,
    pub setup: bool,
}

/// What was asked, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CockpitWanted {
    pub list: bool,
    /// `Some(None)` writes fresh scenarios, `Some(Some(text))` one from the text.
    pub write_scenarios: Option<Option<String>>,
    pub approve_scenario: Option<String>,
    /// `Some(None)` runs every approved scenario, `Some(Some(slug))` one.
    pub run_scenarios: Option<Option<String>>,
    pub bench: Option<BenchWanted>,
    pub copilot: Option<CopilotWanted>,
    pub as_comment: Option<String>,
    pub on_main: bool,
    pub yes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BenchWanted {
    pub name: String,
    pub write: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CopilotWanted {
    pub text: String,
    pub item: Option<CopilotItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CopilotItem {
    Scenario(String),
    Bench(String),
}

fn usage(text: &str) -> CliError {
    CliError::Usage(text.into())
}

impl CockpitWanted {
    /// Checks the flags against each other, before any connection. `None` when no cockpit flag
    /// is set: `--yes` and `--setup` alone belong to the setup step.
    pub(crate) fn from_flags(flags: CockpitFlags) -> Result<Option<CockpitWanted>, CliError> {
        // With `--copilot`, `--bench` only names the item to ask about.
        let runs = flags.run_scenarios || (flags.bench.is_some() && flags.copilot.is_none());
        if flags.on_main && !runs {
            return Err(usage("--on-main needs --run-scenarios or --bench"));
        }
        if flags.scenario.is_some() && !(flags.run_scenarios || flags.copilot.is_some()) {
            return Err(usage("--scenario needs --run-scenarios or --copilot"));
        }
        if flags.describe.is_some() && !flags.write_scenarios {
            return Err(usage("--describe needs --write-scenarios"));
        }
        if flags.write && flags.bench.is_none() {
            return Err(usage("--write needs --bench"));
        }
        if let Some(text) = &flags.describe
            && !describe_ok(text)
        {
            return Err(usage(
                "The description must be between 1 and 600 characters on one line",
            ));
        }
        if flags.yes
            && !(flags.setup || flags.approve_scenario.is_some() || (flags.on_main && runs))
        {
            return Err(usage(
                "--yes needs --setup, --approve-scenario or --on-main",
            ));
        }
        let copilot = match flags.copilot {
            None => None,
            Some(text) => {
                if !copilot_ask_ok(&text) {
                    return Err(usage(
                        "The question must be between 1 and 2000 characters on one line",
                    ));
                }
                let item = match (&flags.scenario, &flags.bench) {
                    (Some(slug), _) => Some(CopilotItem::Scenario(slug.clone())),
                    (None, Some(name)) => Some(CopilotItem::Bench(name.clone())),
                    (None, None) => None,
                };
                if text == "explain" && item.is_none() {
                    return Err(usage("explain needs --scenario or --bench"));
                }
                Some(CopilotWanted { text, item })
            }
        };
        let bench = match flags.bench {
            Some(name) if copilot.is_none() => Some(BenchWanted {
                name,
                write: flags.write,
            }),
            _ => None,
        };
        let wanted = CockpitWanted {
            list: flags.scenarios,
            write_scenarios: flags.write_scenarios.then_some(flags.describe),
            approve_scenario: flags.approve_scenario,
            run_scenarios: flags.run_scenarios.then_some(flags.scenario),
            bench,
            copilot,
            as_comment: flags.as_comment,
            on_main: flags.on_main,
            yes: flags.yes,
        };
        let any = wanted.list
            || wanted.write_scenarios.is_some()
            || wanted.approve_scenario.is_some()
            || wanted.run_scenarios.is_some()
            || wanted.bench.is_some()
            || wanted.copilot.is_some()
            || wanted.as_comment.is_some();
        Ok(any.then_some(wanted))
    }
}

/// What one cockpit step adds to the command's output and exit code.
pub(crate) struct Done {
    pub human: Vec<String>,
    pub json: serde_json::Map<String, serde_json::Value>,
    /// A non-zero exit code and its message; the first failing step's.
    pub failed: Option<(u8, String)>,
}

fn running_item(view: &CockpitView, kind: ItemKind, id: &str) -> bool {
    matches!(&view.activity, Some(CockpitActivity::Running { item, .. })
        if item.kind == kind && item.id == id)
}

fn reason_of(support: &ItemSupport) -> Option<&str> {
    match support {
        ItemSupport::Unsupported { reason }
        | ItemSupport::Stale { reason }
        | ItemSupport::NoCommand { reason } => Some(reason),
        ItemSupport::Ready | ItemSupport::NeedsApproval => None,
    }
}

fn input_lines<'a>(pairs: impl Iterator<Item = (&'a str, &'a str)>) -> Vec<String> {
    pairs
        .map(|(name, value)| format!("  {} = {}", line_of(name), line_of(value)))
        .collect()
}

fn scenario_head(view: &CockpitView, s: &ScenarioView) -> String {
    format!(
        "{}  {}  · {}",
        line_of(&s.slug),
        line_of(&s.title),
        scenario_status(s, running_item(view, ItemKind::Scenario, &s.slug))
    )
}

fn bench_head(view: &CockpitView, b: &BenchView) -> String {
    format!(
        "{}  {}  · {}",
        line_of(&b.slug),
        line_of(&b.qualified),
        bench_status(b, running_item(view, ItemKind::Bench, &b.slug))
    )
}

/// The scenarios and benchmarks with their status, inputs, command and last comparison.
pub(crate) fn cockpit_lines(view: &CockpitView) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(note) = &view.note {
        lines.push(line_of(note));
    }
    lines.push(format!("SCENARIOS · {}", view.scenarios.len()));
    if view.scenarios.is_empty() {
        lines.push("No scenarios yet. Ask your agent to write them from the diff.".into());
    }
    for s in &view.scenarios {
        lines.push(scenario_head(view, s));
        lines.extend(input_lines(
            s.inputs
                .iter()
                .map(|i| (i.input.name.as_str(), i.value.as_str())),
        ));
        if let Some(command) = &s.command {
            lines.push(format!("  run: {}", line_of(command)));
        }
        if let Some(reason) = reason_of(&s.support) {
            lines.push(format!("  Not run: {}", line_of(reason)));
        }
        if let Some(c) = &s.comparison {
            lines.extend(comparison_lines(c));
        }
    }
    lines.push(format!("BENCHMARKS · ON DEMAND · {}", view.benches.len()));
    for b in &view.benches {
        lines.push(bench_head(view, b));
        lines.extend(input_lines(
            b.values.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        ));
        lines.push(format!(
            "  {} warm-up calls · {} s · {} rounds",
            b.warmup, b.secs, b.rounds
        ));
        if let Some(reason) = reason_of(&b.support) {
            lines.push(format!("  Not run: {}", line_of(reason)));
        }
        if let Some(c) = &b.comparison {
            lines.extend(comparison_lines(c));
        }
    }
    lines
}

fn freshness_text(freshness: Freshness) -> Option<&'static str> {
    match freshness {
        Freshness::Current => None,
        Freshness::OlderCommit => Some("From an older commit."),
        Freshness::OlderBase => Some("From an older base."),
        Freshness::ValuesChanged => Some("These values changed since this ran."),
        Freshness::OtherRunner => Some("From another runner."),
    }
}

fn stat_text(metric: Metric, stat: Option<&SideStat>) -> String {
    stat.map_or_else(|| "—".into(), |s| metric_value_text(metric, s.median))
}

fn median_text(row: &MetricRow) -> String {
    match (&row.main, &row.pr) {
        (Some(m), Some(p)) if m.n != p.n => {
            format!("median of {} on main, {} on this PR", m.n, p.n)
        }
        (Some(s), _) | (_, Some(s)) => format!("median of {}", s.n),
        (None, None) => "median of 0".into(),
    }
}

/// The error lines of a comparison: each of this PR's with the count on main, then main's own.
fn error_lines(pr: &[ErrorLine], main: &[ErrorLine]) -> Vec<String> {
    let mut lines = Vec::new();
    for e in pr {
        let on_main = main
            .iter()
            .find(|m| m.text == e.text)
            .map_or(0, |m| m.count);
        lines.push(format!(
            "  error: {} · {on_main} on main",
            error_text(e, RunSide::Pr)
        ));
    }
    for e in main.iter().filter(|m| pr.iter().all(|p| p.text != m.text)) {
        lines.push(format!("  error: {}", error_text(e, RunSide::Main)));
    }
    lines
}

/// The table of one comparison, what is stale about it, its errors and the verdict.
pub(crate) fn comparison_lines(view: &ComparisonView) -> Vec<String> {
    let mut lines = Vec::new();
    for row in &view.rows {
        lines.push(format!(
            "  {:<12}  main {}  this PR {}  {}  ({})",
            row.metric.label(),
            stat_text(row.metric, row.main.as_ref()),
            stat_text(row.metric, row.pr.as_ref()),
            delta_text(&row.delta, row.main.as_ref()),
            median_text(row)
        ));
    }
    for (name, side) in [("this PR", &view.pr), ("main", &view.main)] {
        if let Some(text) = freshness_text(side.freshness) {
            lines.push(format!("  {name}: {text}"));
        }
    }
    if view.exercises == Some(false) {
        lines.push("  Doesn't exercise the change: same result and outputs on main.".into());
    }
    lines.extend(error_lines(&view.pr.errors, &view.main.errors));
    lines.push(format!("  {}", line_of(&view.line)));
    lines
}

fn side_name(side: RunSide) -> &'static str {
    match side {
        RunSide::Pr => "this PR",
        RunSide::Main => "main",
    }
}

fn round_name(result: &RoundResult) -> String {
    match result.round {
        0 => "warm-up".into(),
        k => format!("round {k}"),
    }
}

fn round_word(result: &RoundResult) -> &'static str {
    match (result.end, result.passed) {
        (RoundEnd::Finished, Some(true)) => "passed",
        (RoundEnd::Finished, Some(false)) => "failed",
        (RoundEnd::Finished, None) => "done",
        (RoundEnd::TimedOut, _) => "timed out",
        (RoundEnd::Stopped, _) => "stopped",
        (RoundEnd::Error, _) => "error",
    }
}

/// One finished round as a line of the stream.
pub(crate) fn round_line(result: &RoundResult) -> String {
    let mut line = format!(
        "  {} {}  {}  {}",
        side_name(result.side),
        round_name(result),
        round_word(result),
        duration_text(result.took_ms)
    );
    if let Some(message) = &result.message {
        line.push_str(&format!(" · {}", line_of(message)));
    }
    if let Some(p95) = result.metrics.get(&Metric::P95) {
        line.push_str(&format!("  p95 {}", metric_value_text(Metric::P95, *p95)));
    }
    line
}

/// What a round that did not pass shows: how it ended, its errors and outputs, and the end of
/// its output.
pub(crate) fn round_failure_lines(result: &RoundResult) -> Vec<String> {
    let mut head = format!(
        "  {} {} {}",
        side_name(result.side),
        round_name(result),
        round_word(result)
    );
    if let (RoundEnd::Finished, Some(code)) = (result.end, result.exit_code) {
        head.push_str(&format!(" (exit {code})"));
    }
    let mut lines = vec![head];
    if let Some(message) = &result.message {
        lines.push(format!("    {}", line_of(message)));
    }
    for e in &result.errors {
        lines.push(format!("    error: {}", error_text(e, result.side)));
    }
    for (name, value) in &result.outputs {
        lines.push(format!("    output {} = {}", line_of(name), line_of(value)));
    }
    let tail: Vec<&str> = result.tail.lines().collect();
    for line in &tail[tail.len().saturating_sub(TAIL_LINES)..] {
        lines.push(format!("    | {}", printable(line)));
    }
    lines
}

/// What the reviewer reads before approving a scenario.
pub(crate) fn approval_lines(
    view: &CockpitView,
    scenario: &ScenarioView,
    script: &ScenarioScript,
) -> Vec<String> {
    let mut lines = vec![
        "Run this scenario?".to_string(),
        line_of(&scenario.title),
        format!(
            "Runs in {} · no network",
            runner_text(view.runner.unwrap_or(RunnerKind::Container), None)
        ),
        "Command".into(),
    ];
    if let Some(command) = &scenario.command {
        lines.extend(indented(command));
    }
    for (name, text) in &script.files {
        lines.push(format!("Script · {}", line_of(name)));
        lines.extend(indented(text));
    }
    lines.push("Values reach the script only as environment variables (CLUSIA_ARG_<NAME>).".into());
    lines.push("Main runs the same files from a read-only copy.".into());
    lines
}

fn indented(text: &str) -> Vec<String> {
    printable_lines(text)
        .lines()
        .map(|l| format!("  {l}"))
        .collect()
}

/// The scenario `wanted` names, by its slug.
pub(crate) fn pick_scenario<'a>(
    view: &'a CockpitView,
    wanted: &str,
    pr: &PrRef,
) -> Result<&'a ScenarioView, CliError> {
    view.scenarios
        .iter()
        .find(|s| s.slug == wanted)
        .ok_or_else(|| CliError::Usage(format!("No scenario {} in {pr}", printable(wanted))))
}

/// The benchmark `wanted` names: a slug, else an exact qualified name. Two of one name are
/// refused with their ids.
pub(crate) fn pick_bench<'a>(
    view: &'a CockpitView,
    wanted: &str,
    pr: &PrRef,
) -> Result<&'a BenchView, CliError> {
    if valid_slug(wanted)
        && let Some(bench) = view.benches.iter().find(|b| b.slug == wanted)
    {
        return Ok(bench);
    }
    let named: Vec<&BenchView> = view
        .benches
        .iter()
        .filter(|b| b.qualified == wanted)
        .collect();
    match named.as_slice() {
        [one] => Ok(one),
        [] => Err(CliError::Usage(format!(
            "No benchmark for {} in {pr}",
            printable(wanted)
        ))),
        many => {
            let mut text = format!(
                "More than one benchmark is named {}; use one of these ids:",
                printable(wanted)
            );
            for bench in many {
                text.push_str(&format!("\n{}", printable(&bench.slug)));
            }
            Err(CliError::Usage(text))
        }
    }
}

/// The code of a run, over this PR's measured rounds only: a stop is 130, a timeout 124, an
/// error or a failed round 1. Main's rounds and the warm-up never count.
pub(crate) fn cockpit_exit_code(rounds: &[RoundResult], stopped: bool) -> u8 {
    if stopped {
        return 130;
    }
    let mine = || {
        rounds
            .iter()
            .filter(|r| r.side == RunSide::Pr && r.round >= 1)
    };
    if mine().any(|r| r.end == RoundEnd::Stopped) {
        130
    } else if mine().any(|r| r.end == RoundEnd::TimedOut) {
        124
    } else if mine().any(|r| r.end == RoundEnd::Error || r.passed == Some(false)) {
        1
    } else {
        0
    }
}

fn exit_message(code: u8, bench: bool) -> &'static str {
    match code {
        130 => "the run was stopped",
        124 => "a round passed its time limit and was stopped",
        _ if bench => "the benchmark failed",
        _ => "scenarios failed",
    }
}

async fn read_cockpit(
    client: &mut Client,
    pr: &PrRef,
    stop: &mut Interrupts<'_>,
) -> Result<CockpitView, CliError> {
    match stop
        .ask(
            client,
            Request::GetCockpit { pr: pr.clone() },
            false,
            "reading the cockpit",
        )
        .await?
    {
        Reply::Cockpit(view) => Ok(*view),
        other => Err(unexpected(other)),
    }
}

/// The next cockpit view told for this review.
async fn next_view(
    client: &mut Client,
    pr: &PrRef,
    stop: &mut Interrupts<'_>,
    what: &str,
) -> Result<CockpitView, CliError> {
    loop {
        if let Event::CockpitChanged { pr: of, view } = stop.event(client, what).await?
            && of == *pr
        {
            return Ok(*view);
        }
    }
}

fn refused(message: impl Into<String>) -> CliError {
    CliError::Exit {
        code: 1,
        message: message.into(),
    }
}

/// Waits for an agent turn to end: the view told without activity after one that showed it
/// writing. What was told before the `Ack` comes first.
async fn wait_for_writing(
    client: &mut Client,
    pr: &PrRef,
    stop: &mut Interrupts<'_>,
) -> Result<CockpitView, CliError> {
    let mut writing = false;
    follow(client, pr, stop, |event| match event {
        Event::CockpitChanged { pr: of, view } if of == *pr => match view.activity {
            Some(CockpitActivity::Writing { .. }) => {
                writing = true;
                None
            }
            None if writing => Some(*view),
            _ => None,
        },
        _ => None,
    })
    .await
}

fn writing_failed(view: &CockpitView, what: &str) -> Option<CliError> {
    view.failed
        .as_ref()
        .map(|text| refused(format!("Writing the {what} failed: {}", line_of(text))))
}

async fn write_scenarios(
    client: &mut Client,
    pr: &PrRef,
    text: Option<&String>,
    stop: &mut Interrupts<'_>,
) -> Result<CockpitView, CliError> {
    let ask = match text {
        Some(text) => ScenarioAsk::Describe { text: text.clone() },
        None => ScenarioAsk::Fresh,
    };
    let request = Request::WriteScenarios {
        pr: pr.clone(),
        ask,
    };
    stop.ask(client, request, true, "writing the scenarios")
        .await?;
    let ended = wait_for_writing(client, pr, stop).await?;
    match writing_failed(&ended, "scenarios") {
        Some(e) => Err(e),
        None => Ok(ended),
    }
}

fn why_not_runnable(support: &ItemSupport) -> String {
    match support {
        ItemSupport::Ready => "This item can run".into(),
        ItemSupport::NeedsApproval => "The tests are not approved yet; run with --setup".into(),
        ItemSupport::Unsupported { reason }
        | ItemSupport::Stale { reason }
        | ItemSupport::NoCommand { reason } => line_of(reason),
    }
}

async fn approve_scenario(
    client: &mut Client,
    pr: &PrRef,
    slug: &str,
    yes: bool,
    stop: &mut Interrupts<'_>,
) -> Result<Done, CliError> {
    let view = read_cockpit(client, pr, stop).await?;
    let scenario = pick_scenario(&view, slug, pr)?;
    if scenario.command.is_none() {
        return Err(refused(why_not_runnable(&scenario.support)));
    }
    let script = match stop
        .ask(
            client,
            Request::GetScenarioScript {
                pr: pr.clone(),
                scenario: scenario.slug.clone(),
            },
            false,
            "reading the scenario",
        )
        .await?
    {
        Reply::ScenarioScript(script) => *script,
        other => return Err(unexpected(other)),
    };
    for line in approval_lines(&view, scenario, &script) {
        eprintln!("{line}");
    }
    if !yes && !confirm("Approve this scenario? [y/N] ").await? {
        return Err(refused("Not approved; nothing ran."));
    }
    let after = match stop
        .ask(
            client,
            Request::ApproveScenario {
                pr: pr.clone(),
                scenario: scenario.slug.clone(),
                hash: script.approval_hash.clone(),
            },
            false,
            "approving",
        )
        .await?
    {
        Reply::Cockpit(view) => *view,
        other => return Err(unexpected(other)),
    };
    let mut done = Done::new();
    done.human.push("Approved.".into());
    if let Some(s) = after.scenarios.iter().find(|s| s.slug == scenario.slug) {
        done.human.push(scenario_head(&after, s));
    }
    done.json.insert("cockpit".into(), json!(after));
    Ok(done)
}

impl Done {
    fn new() -> Done {
        Done {
            human: Vec::new(),
            json: serde_json::Map::new(),
            failed: None,
        }
    }
}

fn main_failure(view: &CockpitView) -> String {
    match &view.main.phase {
        MainPhase::Failed { message } => line_of(message),
        MainPhase::Unavailable { reason } => line_of(reason),
        _ => view
            .main
            .plan
            .iter()
            .flat_map(|p| &p.steps)
            .find_map(|s| match &s.state {
                clusia_core::tests::StepState::Failed { message } => Some(line_of(message)),
                _ => None,
            })
            .unwrap_or_else(|| "a step still needs your approval".into()),
    }
}

/// Makes main ready, with its own approval, and returns the view as it ends. Nothing is sent
/// when main is ready.
async fn ensure_main(
    client: &mut Client,
    pr: &PrRef,
    mut view: CockpitView,
    yes: bool,
    stop: &mut Interrupts<'_>,
) -> Result<CockpitView, CliError> {
    if matches!(view.main.phase, MainPhase::Ready) {
        return Ok(view);
    }
    if !matches!(
        view.main.phase,
        MainPhase::Creating | MainPhase::Preparing { .. }
    ) {
        stop.ask(
            client,
            Request::OpenMain { pr: pr.clone() },
            true,
            "opening main",
        )
        .await?;
        client.take_events();
        view = read_cockpit(client, pr, stop).await?;
        client.take_events();
    }
    while matches!(view.main.phase, MainPhase::Creating) {
        view = next_view(client, pr, stop, "opening main").await?;
    }
    match &view.main.phase {
        MainPhase::Ready => return Ok(view),
        MainPhase::NeedsSetup => {}
        MainPhase::Preparing { .. } => {}
        _ => return Err(refused(main_failure(&view))),
    }
    if matches!(view.main.phase, MainPhase::NeedsSetup) {
        let Some(plan) = view.main.plan.clone() else {
            return Err(refused("Main has no setup plan"));
        };
        if let Some(blocked) = &plan.blocked {
            return Err(refused(format!("Blocked: {}", line_of(blocked))));
        }
        eprintln!("Prepare main");
        eprintln!(
            "main · {} · this pull request's image {}",
            line_of(
                view.main
                    .base
                    .as_deref()
                    .map_or("", |b| &b[..b.len().min(7)])
            ),
            line_of(view.main.image.as_deref().unwrap_or("—"))
        );
        for line in plan_lines(&plan) {
            eprintln!("{line}");
        }
        if !yes && !confirm("Approve every step and the commands on main? [y/N] ").await? {
            return Err(refused("Main not approved; nothing ran."));
        }
        let request = Request::PrepareMain {
            pr: pr.clone(),
            main_hash: plan.recipe_hash.clone(),
            approve: approval_of(&plan),
        };
        stop.ask(client, request, true, "preparing main").await?;
        client.take_events();
        view = read_cockpit(client, pr, stop).await?;
        client.take_events();
    }
    while matches!(view.main.phase, MainPhase::Preparing { .. }) {
        view = next_view(client, pr, stop, "preparing main").await?;
    }
    if matches!(view.main.phase, MainPhase::Ready) {
        Ok(view)
    } else {
        Err(refused(format!(
            "Preparing main did not finish: {}",
            main_failure(&view)
        )))
    }
}

/// What a run is asked to do.
enum Target<'a> {
    Scenarios(Option<&'a str>),
    Bench(&'a str),
}

/// What a run will cover, checked against a view: the scope, the hash it was read at and the
/// items that will run.
struct Plan {
    scope: CockpitScope,
    seen: String,
    items: Vec<ItemRef>,
}

fn scenario_ref(slug: &str) -> ItemRef {
    ItemRef {
        kind: ItemKind::Scenario,
        id: slug.into(),
    }
}

fn plan_run(view: &CockpitView, target: &Target<'_>, pr: &PrRef) -> Result<Plan, CliError> {
    match target {
        Target::Scenarios(Some(slug)) => {
            let s = pick_scenario(view, slug, pr)?;
            if s.approval != ScenarioApproval::Approved {
                return Err(refused(format!(
                    "{NOT_APPROVED}; run with --approve-scenario {}",
                    line_of(&s.slug)
                )));
            }
            if s.support != ItemSupport::Ready {
                return Err(refused(why_not_runnable(&s.support)));
            }
            Ok(Plan {
                scope: CockpitScope::Item {
                    item: scenario_ref(&s.slug),
                },
                seen: s.hash.clone(),
                items: vec![scenario_ref(&s.slug)],
            })
        }
        Target::Scenarios(None) => {
            let items: Vec<ItemRef> = view
                .scenarios
                .iter()
                .filter(|s| {
                    s.approval == ScenarioApproval::Approved && s.support == ItemSupport::Ready
                })
                .map(|s| scenario_ref(&s.slug))
                .collect();
            if items.is_empty() {
                return Err(refused(NO_SCENARIOS));
            }
            Ok(Plan {
                scope: CockpitScope::Scenarios,
                seen: view.hash.clone(),
                items,
            })
        }
        Target::Bench(name) => {
            let b = pick_bench(view, name, pr)?;
            if b.support != ItemSupport::Ready {
                return Err(refused(why_not_runnable(&b.support)));
            }
            let item = ItemRef {
                kind: ItemKind::Bench,
                id: b.slug.clone(),
            };
            Ok(Plan {
                scope: CockpitScope::Item { item: item.clone() },
                seen: b.hash.clone(),
                items: vec![item],
            })
        }
    }
}

fn stopped_in(view: &CockpitView, item: &ItemRef, on_main: bool) -> bool {
    let (last, comparison) = match item.kind {
        ItemKind::Scenario => view
            .scenarios
            .iter()
            .find(|s| s.slug == item.id)
            .map(|s| (&s.last, &s.comparison)),
        _ => view
            .benches
            .iter()
            .find(|b| b.slug == item.id)
            .map(|b| (&b.last, &b.comparison)),
    }
    .unwrap_or((&None, &None));
    if on_main {
        comparison
            .as_ref()
            .is_some_and(|c| c.outcome == CompareOutcome::Stopped || c.pr.stopped)
    } else {
        last.as_ref().is_some_and(|l| l.stopped)
    }
}

/// Runs the items, streams the rounds and prints each item's result. Returns the exit code's
/// failure, if any.
#[allow(clippy::too_many_arguments)]
async fn run_items(
    client: &mut Client,
    pr: &PrRef,
    target: Target<'_>,
    on_main: bool,
    yes: bool,
    json_out: bool,
    stop: &mut Interrupts<'_>,
    done: &mut Done,
) -> Result<(), CliError> {
    let mut view = read_cockpit(client, pr, stop).await?;
    let mut retried = false;
    let (id, skipped, plan) = loop {
        let mut plan = plan_run(&view, &target, pr)?;
        if on_main && !matches!(view.main.phase, MainPhase::Ready) {
            view = ensure_main(client, pr, view, yes, stop).await?;
            plan = plan_run(&view, &target, pr)?;
        }
        let request = Request::RunCockpit {
            pr: pr.clone(),
            scope: plan.scope.clone(),
            on_main,
            seen: plan.seen.clone(),
        };
        match stop.ask(client, request, true, "the run").await {
            Ok(Reply::CockpitStarted { run, skipped }) => break (run, skipped, plan),
            Ok(other) => return Err(unexpected(other)),
            Err(CliError::Client(ClientError::Server(e)))
                if e.message == COCKPIT_CHANGED && !retried =>
            {
                retried = true;
                view = read_cockpit(client, pr, stop).await?;
            }
            Err(e) => return Err(e),
        }
    };
    for name in &skipped {
        eprintln!("Skipped: {}", line_of(name));
    }
    let mut rounds: Vec<RoundResult> = Vec::new();
    let mut started = false;
    // The view told at the run's end already holds its results, so it is not read again.
    let after = follow(client, pr, stop, |event| match event {
        Event::CockpitRound {
            pr: of,
            run,
            result,
        } if of == *pr && run == id => {
            if json_out {
                if let Ok(line) = serde_json::to_string(&*result) {
                    println!("{line}");
                }
            } else {
                println!("{}", round_line(&result));
            }
            rounds.push(*result);
            None
        }
        Event::CockpitChanged { pr: of, view } if of == *pr => match &view.activity {
            Some(CockpitActivity::Running { run, .. }) if *run == id => {
                started = true;
                None
            }
            None if started => Some(*view),
            _ => None,
        },
        _ => None,
    })
    .await?;
    let ran: Vec<&ItemRef> = plan
        .items
        .iter()
        .filter(|i| !skipped.contains(&i.id))
        .collect();
    let mut items = Vec::new();
    for item in &ran {
        let (head, comparison) = match item.kind {
            ItemKind::Scenario => {
                let Some(s) = after.scenarios.iter().find(|s| s.slug == item.id) else {
                    continue;
                };
                (scenario_head(&after, s), s.comparison.as_ref())
            }
            _ => {
                let Some(b) = after.benches.iter().find(|b| b.slug == item.id) else {
                    continue;
                };
                (bench_head(&after, b), b.comparison.as_ref())
            }
        };
        done.human.push(head.clone());
        for r in rounds
            .iter()
            .filter(|r| r.item == **item && r.side == RunSide::Pr)
            .filter(|r| {
                r.passed == Some(false) || matches!(r.end, RoundEnd::Error | RoundEnd::TimedOut)
            })
        {
            done.human.extend(round_failure_lines(r));
        }
        if let Some(c) = comparison {
            done.human.extend(comparison_lines(c));
        }
        items.push(json!({
            "item": item,
            "verdict": comparison.map(|c| &c.verdict),
            "line": comparison.map_or(head, |c| c.line.clone()),
        }));
    }
    let stopped = ran.iter().any(|i| stopped_in(&after, i, on_main));
    let code = cockpit_exit_code(&rounds, stopped);
    if code != 0 && done.failed.is_none() {
        let bench = matches!(target, Target::Bench(_));
        done.failed = Some((code, exit_message(code, bench).into()));
    }
    done.json.insert("cockpit".into(), json!(after));
    let slot = done.json.entry("items").or_insert_with(|| json!([]));
    if let Some(all) = slot.as_array_mut() {
        all.extend(items);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_bench(
    client: &mut Client,
    pr: &PrRef,
    bench: &BenchWanted,
    on_main: bool,
    yes: bool,
    json_out: bool,
    stop: &mut Interrupts<'_>,
    done: &mut Done,
) -> Result<(), CliError> {
    if bench.write {
        let functions = read_functions(client, pr, stop).await?;
        let function = pick_function(&functions, &bench.name).map_err(|e| match e {
            CliError::Other(text) => CliError::Other(format!("{text} in {pr}")),
            other => other,
        })?;
        let request = Request::WriteBench {
            pr: pr.clone(),
            function: function.slug.clone(),
            ask: BenchAsk::Fresh,
        };
        stop.ask(client, request, true, "writing the benchmark")
            .await?;
        let ended = wait_for_writing(client, pr, stop).await?;
        if let Some(e) = writing_failed(&ended, "benchmark") {
            return Err(e);
        }
        // The benchmark just written is named by the function's slug, which the pick accepts.
        return run_items(
            client,
            pr,
            Target::Bench(&function.slug.clone()),
            on_main,
            yes,
            json_out,
            stop,
            done,
        )
        .await;
    }
    run_items(
        client,
        pr,
        Target::Bench(&bench.name),
        on_main,
        yes,
        json_out,
        stop,
        done,
    )
    .await
}

async fn ask_copilot(
    client: &mut Client,
    pr: &PrRef,
    copilot: &CopilotWanted,
    stop: &mut Interrupts<'_>,
    done: &mut Done,
) -> Result<(), CliError> {
    let before = read_cockpit(client, pr, stop).await?;
    let item = match &copilot.item {
        Some(CopilotItem::Scenario(slug)) => Some(scenario_ref(
            pick_scenario(&before, slug, pr)?.slug.as_str(),
        )),
        Some(CopilotItem::Bench(name)) => Some(ItemRef {
            kind: ItemKind::Bench,
            id: pick_bench(&before, name, pr)?.slug.clone(),
        }),
        None => None,
    };
    let ask = if copilot.text == "explain" {
        CopilotAsk::Explain
    } else {
        CopilotAsk::Free {
            text: copilot.text.clone(),
        }
    };
    let seen_id = before
        .copilot
        .messages
        .iter()
        .map(|m| m.id)
        .max()
        .unwrap_or(0);
    stop.ask(
        client,
        Request::AskCopilot {
            pr: pr.clone(),
            item,
            ask,
        },
        true,
        "the answer",
    )
    .await?;
    let mut busy = false;
    let ended = follow(client, pr, stop, |event| match event {
        Event::CockpitChanged { pr: of, view } if of == *pr => {
            if view.copilot.busy {
                busy = true;
                None
            } else {
                busy.then_some(*view)
            }
        }
        _ => None,
    })
    .await?;
    if let Some(text) = &ended.copilot.failed {
        return Err(refused(line_of(text)));
    }
    for message in ended
        .copilot
        .messages
        .iter()
        .filter(|m| m.id > seen_id && m.from == CopilotFrom::Agent)
    {
        done.human
            .extend(printable_lines(&message.text).lines().map(String::from));
        for s in &message.suggestions {
            let kind = match s.kind {
                ItemKind::Bench => "bench",
                _ => "scenario",
            };
            done.human.push(format!(
                "Suggestion · {kind}: {} — {}",
                line_of(&s.title),
                line_of(&s.why)
            ));
        }
    }
    done.json.insert("copilot".into(), json!(ended.copilot));
    Ok(())
}

async fn as_comment(
    client: &mut Client,
    pr: &PrRef,
    slug: &str,
    stop: &mut Interrupts<'_>,
    done: &mut Done,
) -> Result<(), CliError> {
    let view = read_cockpit(client, pr, stop).await?;
    let scenario = pick_scenario(&view, slug, pr)?;
    let script = match stop
        .ask(
            client,
            Request::GetScenarioScript {
                pr: pr.clone(),
                scenario: scenario.slug.clone(),
            },
            false,
            "reading the scenario",
        )
        .await?
    {
        Reply::ScenarioScript(script) => *script,
        other => return Err(unexpected(other)),
    };
    let text = scenario_comment(scenario, &script);
    done.json.insert("comment".into(), json!(text));
    done.human.push(text);
    Ok(())
}

/// Runs the cockpit steps that were asked, in order: write, approve, list, run scenarios,
/// benchmark, copilot, comment. A step that refuses ends the command; a run that did not pass
/// leaves its code in `failed` and the later steps still run.
pub(crate) async fn run(
    client: &mut Client,
    pr: &PrRef,
    wanted: &CockpitWanted,
    json_out: bool,
    stop: &mut Interrupts<'_>,
) -> Result<Done, CliError> {
    let mut done = Done::new();
    if let Some(text) = &wanted.write_scenarios {
        let ended = write_scenarios(client, pr, text.as_ref(), stop).await?;
        done.human.extend(cockpit_lines(&ended));
        done.json.insert("cockpit".into(), json!(ended));
    }
    if let Some(slug) = &wanted.approve_scenario {
        let step = approve_scenario(client, pr, slug, wanted.yes, stop).await?;
        done.human.extend(step.human);
        done.json.extend(step.json);
    }
    if wanted.list {
        let view = read_cockpit(client, pr, stop).await?;
        done.human.extend(cockpit_lines(&view));
        done.json.insert("cockpit".into(), json!(view));
    }
    if let Some(slug) = &wanted.run_scenarios {
        let target = Target::Scenarios(slug.as_deref());
        run_items(
            client,
            pr,
            target,
            wanted.on_main,
            wanted.yes,
            json_out,
            stop,
            &mut done,
        )
        .await?;
    }
    if let Some(bench) = &wanted.bench {
        run_bench(
            client,
            pr,
            bench,
            wanted.on_main,
            wanted.yes,
            json_out,
            stop,
            &mut done,
        )
        .await?;
    }
    if let Some(copilot) = &wanted.copilot {
        ask_copilot(client, pr, copilot, stop, &mut done).await?;
    }
    if let Some(slug) = &wanted.as_comment {
        as_comment(client, pr, slug, stop, &mut done).await?;
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use clusia_core::functions::{FunctionChange, FunctionLang, Param, ParamKind};
    use clusia_core::tests::RunnerKind;

    use super::*;

    const SLUG: &str = "two-requests-refresh-at-once";
    const BENCH: &str = "tokenstore_refresh-0123456789";

    fn pr() -> PrRef {
        "acme/widgets#7".parse().unwrap()
    }

    fn input(name: &str, value: &str) -> InputView {
        InputView {
            input: Input {
                name: name.into(),
                label: name.into(),
                kind: InputKind::String,
                choices: Vec::new(),
                default: value.into(),
            },
            value: value.into(),
            edited: false,
        }
    }

    fn scenario(approval: ScenarioApproval) -> ScenarioView {
        ScenarioView {
            slug: SLUG.into(),
            title: "Two requests refresh at once".into(),
            purpose: "Only one exchange may happen.".into(),
            steps: vec!["Store an expired token".into()],
            inputs: vec![input("callers", "2"), input("mode", "fast")],
            run: "python3 {script}".into(),
            command: Some("python3 \"$CLUSIA_SCENARIO_DIR/run.py\"".into()),
            scripts: vec![ScriptMeta {
                name: "run.py".into(),
                bytes: 12,
                sha: "s".into(),
                edited: false,
            }],
            rounds: 3,
            timeout_secs: 120,
            edited: false,
            approval,
            approval_hash: "a".into(),
            input_hash: "i".into(),
            support: ItemSupport::Ready,
            last: None,
            comparison: None,
            status: String::new(),
            hash: "sh".into(),
        }
    }

    fn bench() -> BenchView {
        BenchView {
            slug: BENCH.into(),
            qualified: "TokenStore::refresh".into(),
            file: "store/refresh.rs".into(),
            lang: FunctionLang::Rust,
            change: FunctionChange::Changed,
            params: vec![Param {
                name: "stored_token".into(),
                label: "stored token".into(),
                kind: ParamKind::Text,
            }],
            values: [("stored_token".to_string(), "old".to_string())].into(),
            warmup: 10,
            secs: 6,
            rounds: 5,
            edited: false,
            path: format!(".clusia/functions/{BENCH}/bench.rs"),
            support: ItemSupport::Ready,
            input_hash: "bi".into(),
            last: None,
            comparison: None,
            status: String::new(),
            hash: "bh".into(),
        }
    }

    fn view(scenarios: Vec<ScenarioView>, benches: Vec<BenchView>) -> CockpitView {
        let mut view = CockpitView::new("9be1d07");
        view.runner = Some(RunnerKind::Container);
        view.scenarios = scenarios;
        view.benches = benches;
        view.hash = scenarios_hash(&view.scenarios);
        view
    }

    fn side(freshness: Freshness) -> SideView {
        SideView {
            freshness,
            sha: "9be1d07".into(),
            rounds: 3,
            pass: Some(PassCount {
                passed: 3,
                rounds: 3,
            }),
            series: Series::default(),
            errors: Vec::new(),
            outputs: BTreeMap::new(),
            stopped: false,
        }
    }

    fn row(
        metric: Metric,
        main: u64,
        pr: u64,
        label: DeltaLabel,
        percent: Option<i64>,
    ) -> MetricRow {
        let stat = |median| {
            Some(SideStat {
                median,
                spread: 2,
                n: 3,
            })
        };
        MetricRow {
            metric,
            main: stat(main),
            pr: stat(pr),
            delta: Delta { label, percent },
        }
    }

    fn comparison(rows: Vec<MetricRow>, verdict: Verdict) -> ComparisonView {
        ComparisonView {
            run: "run-1".into(),
            pr: side(Freshness::Current),
            main: side(Freshness::Current),
            rows,
            line: verdict_line(&verdict),
            verdict,
            exercises: None,
            outcome: CompareOutcome::Done,
            at: 1,
        }
    }

    fn round(side: RunSide, k: u32, end: RoundEnd, passed: Option<bool>, ms: u64) -> RoundResult {
        RoundResult {
            item: ItemRef {
                kind: ItemKind::Scenario,
                id: SLUG.into(),
            },
            side,
            round: k,
            sha: "9be1d07".into(),
            passed,
            end,
            exit_code: Some(0),
            metrics: BTreeMap::new(),
            outputs: BTreeMap::new(),
            errors: Vec::new(),
            message: None,
            tail: String::new(),
            took_ms: ms,
            at: 1,
        }
    }

    #[test]
    fn the_list_shows_each_item_with_its_status_inputs_and_command() {
        let mut ran = scenario(ScenarioApproval::Approved);
        ran.comparison = Some(comparison(
            vec![
                row(Metric::Duration, 100, 130, DeltaLabel::Worse, Some(30)),
                row(
                    Metric::PeakMemory,
                    51_200,
                    51_200,
                    DeltaLabel::Same,
                    Some(0),
                ),
            ],
            Verdict::Worse {
                metrics: vec![Metric::Duration],
            },
        ));
        let mut waiting = scenario(ScenarioApproval::NotApproved);
        waiting.slug = "token-endpoint-is-down".into();
        waiting.title = "Token endpoint is down".into();
        waiting.support = ItemSupport::NeedsApproval;
        let mut bad = scenario(ScenarioApproval::Approved);
        bad.slug = "bad-run".into();
        bad.title = "Bad run".into();
        bad.command = None;
        bad.support = ItemSupport::Unsupported {
            reason: "The command uses {x}, which is not an input".into(),
        };
        let lines = cockpit_lines(&view(vec![ran, waiting, bad], vec![bench()]));
        assert_eq!(
            lines,
            vec![
                "SCENARIOS · 3".to_string(),
                format!("{SLUG}  Two requests refresh at once  · duration +30% · worse than main"),
                "  callers = 2".into(),
                "  mode = fast".into(),
                "  run: python3 \"$CLUSIA_SCENARIO_DIR/run.py\"".into(),
                "  Duration      main 100 ms  this PR 130 ms  +30%  (median of 3)".into(),
                "  Peak memory   main 50.0 MB  this PR 50.0 MB  ≈  (median of 3)".into(),
                "  Worse than main: duration".into(),
                "token-endpoint-is-down  Token endpoint is down  · not approved".into(),
                "  callers = 2".into(),
                "  mode = fast".into(),
                "  run: python3 \"$CLUSIA_SCENARIO_DIR/run.py\"".into(),
                "bad-run  Bad run  · not run".into(),
                "  callers = 2".into(),
                "  mode = fast".into(),
                "  Not run: The command uses {x}, which is not an input".into(),
                "BENCHMARKS · ON DEMAND · 1".into(),
                format!("{BENCH}  TokenStore::refresh  · not run"),
                "  stored_token = old".into(),
                "  10 warm-up calls · 6 s · 5 rounds".into(),
            ]
        );
    }

    #[test]
    fn an_empty_cockpit_says_how_to_start_and_prints_its_note() {
        let mut empty = view(vec![], vec![]);
        empty.note = Some("Open the review to use the QA cockpit".into());
        assert_eq!(
            cockpit_lines(&empty),
            vec![
                "Open the review to use the QA cockpit".to_string(),
                "SCENARIOS · 0".into(),
                "No scenarios yet. Ask your agent to write them from the diff.".into(),
                "BENCHMARKS · ON DEMAND · 0".into(),
            ]
        );
    }

    #[test]
    fn hostile_text_in_the_list_is_filtered() {
        let mut s = scenario(ScenarioApproval::Approved);
        s.title = "Two\u{202e} requests\n## Approve".into();
        s.inputs[0].value = "a\u{1b}[2Kb".into();
        s.command = Some("python3 x\u{202e}".into());
        let lines = cockpit_lines(&view(vec![s], vec![]));
        let all = lines.join("\n");
        assert!(
            !all.contains('\u{202e}') && !all.contains('\u{1b}'),
            "{all}"
        );
        assert!(all.contains("Two\\u{202e} requests ## Approve"), "{all}");
        assert!(all.contains("callers = a\\u{1b}[2Kb"), "{all}");
        assert!(all.contains("run: python3 x\\u{202e}"), "{all}");
        assert_eq!(
            lines.iter().filter(|l| l.starts_with(SLUG)).count(),
            1,
            "a newline never starts a line"
        );
    }

    #[test]
    fn a_comparison_shows_stale_sides_errors_and_the_anti_goal() {
        let mut c = comparison(
            vec![MetricRow {
                metric: Metric::P95,
                main: Some(SideStat {
                    median: 3_200_000,
                    spread: 1,
                    n: 5,
                }),
                pr: Some(SideStat {
                    median: 4_400_000,
                    spread: 1,
                    n: 4,
                }),
                delta: Delta {
                    label: DeltaLabel::Worse,
                    percent: Some(37),
                },
            }],
            Verdict::Worse {
                metrics: vec![Metric::P95],
            },
        );
        c.main.freshness = Freshness::OlderBase;
        c.pr.freshness = Freshness::ValuesChanged;
        c.exercises = Some(false);
        c.pr.errors = vec![
            ErrorLine {
                text: "refresh failed: 500".into(),
                count: 3,
                first_ms: 1_250,
            },
            ErrorLine {
                text: "other\u{1b}[31m".into(),
                count: 1,
                first_ms: 10,
            },
        ];
        c.main.errors = vec![
            ErrorLine {
                text: "refresh failed: 500".into(),
                count: 1,
                first_ms: 5,
            },
            ErrorLine {
                text: "main only".into(),
                count: 2,
                first_ms: 7,
            },
        ];
        assert_eq!(
            comparison_lines(&c),
            vec![
                "  Latency p95   main 3.2 ms  this PR 4.4 ms  +37%  (median of 5 on main, 4 on this PR)".to_string(),
                "  this PR: These values changed since this ran.".into(),
                "  main: From an older base.".into(),
                "  Doesn't exercise the change: same result and outputs on main.".into(),
                "  error: 3 × refresh failed: 500 after 1.2 s (this PR) · 1 on main".into(),
                "  error: 1 × other\\u{1b}[31m after 0.0 s (this PR) · 0 on main".into(),
                "  error: 2 × main only after 0.0 s (main)".into(),
                "  Worse than main: p95".into(),
            ]
        );
        let mut one_side = comparison(
            vec![MetricRow {
                metric: Metric::AvgCpu,
                main: None,
                pr: Some(SideStat {
                    median: 1_250,
                    spread: 0,
                    n: 3,
                }),
                delta: Delta {
                    label: DeltaLabel::NotMeasured,
                    percent: None,
                },
            }],
            Verdict::NotEnoughRounds,
        );
        one_side.pr.errors.clear();
        assert_eq!(
            comparison_lines(&one_side),
            vec![
                "  CPU (avg)     main —  this PR 125%  not measured  (median of 3)".to_string(),
                "  No verdict: each side needs 3 finished rounds".into(),
            ]
        );
    }

    #[test]
    fn a_round_is_one_line() {
        let r = |s, k, end, passed, ms| round_line(&round(s, k, end, passed, ms));
        assert_eq!(
            r(RunSide::Pr, 1, RoundEnd::Finished, Some(true), 210),
            "  this PR round 1  passed  210 ms"
        );
        assert_eq!(
            r(RunSide::Main, 2, RoundEnd::Finished, Some(false), 1_234),
            "  main round 2  failed  1.2 s"
        );
        assert_eq!(
            r(RunSide::Pr, 0, RoundEnd::Finished, Some(true), 3_000),
            "  this PR warm-up  passed  3.0 s"
        );
        assert_eq!(
            r(RunSide::Pr, 3, RoundEnd::TimedOut, None, 120_000),
            "  this PR round 3  timed out  120.0 s"
        );
        assert_eq!(
            r(RunSide::Main, 1, RoundEnd::Stopped, None, 40),
            "  main round 1  stopped  40 ms"
        );
        let mut bench = round(RunSide::Pr, 1, RoundEnd::Finished, None, 6_000);
        bench.metrics.insert(Metric::P95, 12_500);
        assert_eq!(
            round_line(&bench),
            "  this PR round 1  done  6.0 s  p95 12.5 µs"
        );
        let mut failed = round(RunSide::Pr, 1, RoundEnd::Error, None, 50);
        failed.message = Some("The benchmark ended without a result (exit 1)\u{1b}[31m".into());
        assert_eq!(
            round_line(&failed),
            "  this PR round 1  error  50 ms · The benchmark ended without a result (exit 1)\\u{1b}[31m"
        );
    }

    #[test]
    fn a_failed_round_shows_its_message_errors_outputs_and_a_filtered_tail() {
        let mut failed = round(RunSide::Pr, 2, RoundEnd::Finished, Some(false), 80);
        failed.exit_code = Some(3);
        failed.errors = vec![ErrorLine {
            text: "refresh failed: 500".into(),
            count: 2,
            first_ms: 300,
        }];
        failed.outputs = [("exchanges".to_string(), "2".to_string())].into();
        failed.tail = "line one\nline two\u{1b}[2J\u{202e}\n".into();
        assert_eq!(
            round_failure_lines(&failed),
            vec![
                "  this PR round 2 failed (exit 3)".to_string(),
                "    error: 2 × refresh failed: 500 after 0.3 s (this PR)".into(),
                "    output exchanges = 2".into(),
                "    | line one".into(),
                "    | line two\\u{1b}[2J\\u{202e}".into(),
            ]
        );
        let long = RoundResult {
            tail: (1..=40).map(|i| format!("l{i}\n")).collect(),
            ..failed
        };
        let lines = round_failure_lines(&long);
        assert_eq!(
            lines.iter().filter(|l| l.starts_with("    | ")).count(),
            20,
            "the last 20 lines"
        );
        assert_eq!(lines.last().unwrap(), "    | l40");
    }

    #[test]
    fn the_approval_text_names_the_command_the_scripts_and_the_env_rule() {
        let s = scenario(ScenarioApproval::NotApproved);
        let script = ScenarioScript {
            scenario: SLUG.into(),
            files: vec![
                ("run.py".into(), "print('ok')\nx = '\u{202e}'\n".into()),
                ("helper.py".into(), "X = 1\n".into()),
            ],
            approval_hash: "a".into(),
        };
        assert_eq!(
            approval_lines(&view(vec![s.clone()], vec![]), &s, &script),
            vec![
                "Run this scenario?".to_string(),
                "Two requests refresh at once".into(),
                "Runs in container · no network".into(),
                "Command".into(),
                "  python3 \"$CLUSIA_SCENARIO_DIR/run.py\"".into(),
                "Script · run.py".into(),
                "  print('ok')".into(),
                "  x = '\\u{202e}'".into(),
                "Script · helper.py".into(),
                "  X = 1".into(),
                "Values reach the script only as environment variables (CLUSIA_ARG_<NAME>).".into(),
                "Main runs the same files from a read-only copy.".into(),
            ]
        );
        let mut local = view(vec![s.clone()], vec![]);
        local.runner = Some(RunnerKind::Local);
        assert_eq!(
            approval_lines(&local, &s, &script)[2],
            "Runs in local sandbox · no network"
        );
    }

    #[test]
    fn a_name_picks_one_item_or_is_a_usage_error() {
        let cockpit = view(vec![scenario(ScenarioApproval::Approved)], vec![bench()]);
        assert_eq!(pick_scenario(&cockpit, SLUG, &pr()).unwrap().slug, SLUG);
        let Err(CliError::Usage(text)) = pick_scenario(&cockpit, "nope\u{202e}", &pr()) else {
            panic!("a usage error");
        };
        assert_eq!(text, "No scenario nope\\u{202e} in acme/widgets#7");
        assert_eq!(pick_bench(&cockpit, BENCH, &pr()).unwrap().slug, BENCH);
        assert_eq!(
            pick_bench(&cockpit, "TokenStore::refresh", &pr())
                .unwrap()
                .slug,
            BENCH
        );
        let Err(CliError::Usage(text)) = pick_bench(&cockpit, "Other::fn", &pr()) else {
            panic!("a usage error");
        };
        assert_eq!(text, "No benchmark for Other::fn in acme/widgets#7");
        let mut twin = bench();
        twin.slug = "tokenstore_refresh-ffffffffff".into();
        twin.file = "other/refresh.rs".into();
        let two = view(vec![], vec![bench(), twin]);
        let Err(CliError::Usage(text)) = pick_bench(&two, "TokenStore::refresh", &pr()) else {
            panic!("a usage error");
        };
        assert_eq!(
            text,
            format!(
                "More than one benchmark is named TokenStore::refresh; use one of these ids:\n{BENCH}\ntokenstore_refresh-ffffffffff"
            )
        );
    }

    #[test]
    fn the_exit_code_counts_the_pr_side_only_and_ranks_a_stop_first() {
        let pr_ok = round(RunSide::Pr, 1, RoundEnd::Finished, Some(true), 10);
        let main_bad = round(RunSide::Main, 1, RoundEnd::Error, Some(false), 10);
        let warm_bad = round(RunSide::Pr, 0, RoundEnd::Error, None, 10);
        assert_eq!(cockpit_exit_code(&[], false), 0);
        assert_eq!(
            cockpit_exit_code(&[warm_bad, pr_ok.clone(), main_bad], false),
            0,
            "main and the warm-up never count"
        );
        let failed = round(RunSide::Pr, 2, RoundEnd::Finished, Some(false), 10);
        let erred = round(RunSide::Pr, 2, RoundEnd::Error, None, 10);
        let timed = round(RunSide::Pr, 3, RoundEnd::TimedOut, None, 10);
        let stopped = round(RunSide::Pr, 3, RoundEnd::Stopped, None, 10);
        assert_eq!(
            cockpit_exit_code(&[pr_ok.clone(), failed.clone()], false),
            1
        );
        assert_eq!(cockpit_exit_code(&[pr_ok.clone(), erred.clone()], false), 1);
        assert_eq!(
            cockpit_exit_code(&[failed.clone(), timed.clone()], false),
            124
        );
        assert_eq!(
            cockpit_exit_code(&[timed, stopped.clone(), failed], false),
            130
        );
        assert_eq!(
            cockpit_exit_code(std::slice::from_ref(&pr_ok), true),
            130,
            "a stopped outcome, whatever the rounds say"
        );
        assert_eq!(cockpit_exit_code(&[stopped], false), 130);
        let bench_done = round(RunSide::Pr, 1, RoundEnd::Finished, None, 10);
        assert_eq!(
            cockpit_exit_code(&[bench_done], false),
            0,
            "a benchmark round that finished is not a failure"
        );
        let _ = pr_ok;
    }

    fn flags() -> CockpitFlags {
        CockpitFlags::default()
    }

    fn usage(flags: CockpitFlags) -> String {
        match CockpitWanted::from_flags(flags) {
            Err(CliError::Usage(text)) => text,
            other => panic!("a usage error, got {other:?}"),
        }
    }

    #[test]
    fn the_flags_that_need_another_are_usage_errors() {
        assert_eq!(
            CockpitWanted::from_flags(flags()).unwrap(),
            None,
            "no cockpit flag, no cockpit step"
        );
        assert_eq!(
            usage(CockpitFlags {
                yes: true,
                ..flags()
            }),
            "--yes needs --setup, --approve-scenario or --on-main"
        );
        assert_eq!(
            usage(CockpitFlags {
                on_main: true,
                ..flags()
            }),
            "--on-main needs --run-scenarios or --bench"
        );
        assert_eq!(
            usage(CockpitFlags {
                scenario: Some("x".into()),
                ..flags()
            }),
            "--scenario needs --run-scenarios or --copilot"
        );
        assert_eq!(
            usage(CockpitFlags {
                copilot: Some("explain".into()),
                ..flags()
            }),
            "explain needs --scenario or --bench"
        );
        assert_eq!(
            usage(CockpitFlags {
                copilot: Some("a\nb".into()),
                ..flags()
            }),
            "The question must be between 1 and 2000 characters on one line"
        );
        assert_eq!(
            usage(CockpitFlags {
                write_scenarios: true,
                describe: Some("x".repeat(601)),
                ..flags()
            }),
            "The description must be between 1 and 600 characters on one line"
        );
        // A setup alone does not make a cockpit step.
        assert_eq!(
            CockpitWanted::from_flags(CockpitFlags {
                yes: true,
                setup: true,
                ..flags()
            })
            .unwrap(),
            None
        );
    }

    #[test]
    fn the_flags_make_the_steps_that_were_asked() {
        let wanted = CockpitWanted::from_flags(CockpitFlags {
            write_scenarios: true,
            describe: Some("Token endpoint is down".into()),
            run_scenarios: true,
            scenario: Some(SLUG.into()),
            on_main: true,
            yes: true,
            ..flags()
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            wanted.write_scenarios,
            Some(Some("Token endpoint is down".to_string()))
        );
        assert_eq!(wanted.run_scenarios, Some(Some(SLUG.to_string())));
        assert!(wanted.on_main && wanted.yes && !wanted.list);
        let bench = CockpitWanted::from_flags(CockpitFlags {
            bench: Some("TokenStore::refresh".into()),
            write: true,
            on_main: true,
            ..flags()
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            bench.bench,
            Some(BenchWanted {
                name: "TokenStore::refresh".into(),
                write: true
            })
        );
        let ask = CockpitWanted::from_flags(CockpitFlags {
            copilot: Some("explain".into()),
            scenario: Some(SLUG.into()),
            ..flags()
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            ask.copilot,
            Some(CopilotWanted {
                text: "explain".into(),
                item: Some(CopilotItem::Scenario(SLUG.into()))
            })
        );
        let list = CockpitWanted::from_flags(CockpitFlags {
            scenarios: true,
            ..flags()
        })
        .unwrap()
        .unwrap();
        assert!(list.list);
        let plain = CockpitWanted::from_flags(CockpitFlags {
            write_scenarios: true,
            ..flags()
        })
        .unwrap()
        .unwrap();
        assert_eq!(plain.write_scenarios, Some(None), "Fresh");
    }
}
