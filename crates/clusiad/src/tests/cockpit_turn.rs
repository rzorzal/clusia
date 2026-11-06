//! The cockpit's agent turns: scenarios, benchmarks and the QA copilot. Each one is forked and
//! read-only, and answers with fenced blocks the daemon checks before it writes anything.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use clusia_core::PrRef;
use clusia_core::cockpit::{
    BenchAsk, BenchBlock, BenchPromptContext, COPILOT_ASK_MAX, COPILOT_BUSY, COPILOT_KEPT,
    CockpitActivity, CopilotAsk, CopilotFrom, CopilotMessage, CopilotPromptContext, ItemKind,
    ItemRef, ItemSupport, LOAD_LATER, SCENARIOS_FIRST, SCENARIOS_MAX, SUGGESTIONS_MAX, ScenarioAsk,
    ScenarioBlock, ScenarioPromptContext, Suggestion, bench_item_text, bench_prompt,
    copilot_prompt, copilot_text, describe_ok, scenario_item_text, scenario_prompt, scenario_slug,
    valid_scenario_slug,
};
use clusia_core::functions::{Support, valid_slug};
use clusia_core::printable::printable;
use clusia_core::tests::{RunnerKind, TestsPhase, TestsRecord};
use clusia_harness::extract_cockpit_blocks;
use clusia_protocol::{Outcome, Reply, TurnOrigin};

use super::bench::{self, NewBench};
use super::cases;
use super::compare::results_of;
use super::env::{self, RecipeState};
use super::recipe_turn::{RecipeSink, note_log};
use super::scenarios::{self, E2E_ROOT_REFUSED, tell_cockpit};
use super::{Doing, Live, STOPPING};
use crate::checks;
use crate::reviews;
use crate::sessions;
use crate::state::Shared;
use crate::sync::now_unix;
use crate::turns::{self, SessionSource, Stop, TurnEnd, TurnRun};

/// What one turn is asked, in both forms: continuing the review's session, and fresh.
struct Spec {
    origin: TurnOrigin,
    prompt: String,
    fresh_prompt: String,
    /// Finishes "<agent> could not …" when the program fails without a message.
    failure: &'static str,
}

/// Runs one forked, read-only turn and gives the text it answered. `Err` is why no answer came
/// (the program failed, the limit passed, a stop).
async fn run_text(
    shared: &Arc<Shared>,
    pr: &PrRef,
    spec: Spec,
    stop: Arc<Stop>,
) -> Result<String, String> {
    let name = sessions::name_for(shared, pr).await;
    let limit_secs = shared.config.read().await.harness.check_timeout_secs;
    let limit = Duration::from_secs(u64::from(limit_secs));
    let turn = shared.sessions.next_turn(shared, pr).await;
    note_log(shared, pr, turn, "waiting");
    let mut sink = RecipeSink {
        shared: shared.clone(),
        pr: pr.clone(),
        turn,
        streamed: String::new(),
        answer: None,
        saw_final: false,
        failed: false,
        message: None,
        activity: &mut |_| {},
    };
    let run = TurnRun {
        pr: pr.clone(),
        turn,
        prompt: spec.prompt,
        fresh_prompt: Some(spec.fresh_prompt),
        session: SessionSource::Fork,
        origin: spec.origin,
        places: checks::places(shared),
        limit: Some(limit),
        stop,
    };
    match turns::run(shared, run, &mut sink).await {
        TurnEnd::Done => {
            note_log(shared, pr, turn, "done");
            Ok(sink.answer.unwrap_or(sink.streamed))
        }
        TurnEnd::Failed => {
            note_log(shared, pr, turn, "failed");
            Err(sink
                .message
                .unwrap_or_else(|| format!("{name} could not {}", spec.failure)))
        }
        TurnEnd::TimedOut => {
            note_log(shared, pr, turn, "failed");
            Err(format!(
                "The turn ran past its limit of {}s and was stopped",
                limit.as_secs()
            ))
        }
        TurnEnd::Stopped(why) => {
            note_log(shared, pr, turn, "stopped");
            Err(why.message().to_string())
        }
    }
}

/// Each reason cut to 300 characters after `printable`.
fn reasons_text(reasons: &[String]) -> String {
    reasons
        .iter()
        .map(|reason| printable(reason).chars().take(300).collect::<String>())
        .collect::<Vec<_>>()
        .join("; ")
}

/// Runs `job` in a task of its own, so a panic still ends the cockpit activity `id`, then tells
/// the windows. `job` answers why the activity failed, if it did.
fn spawn_activity(
    shared: &Arc<Shared>,
    pr: &PrRef,
    id: u64,
    live: Live,
    job: impl Future<Output = Option<String>> + Send + 'static,
) {
    let (shared, pr) = (shared.clone(), pr.clone());
    tokio::spawn(async move {
        let failed = match tokio::spawn(job).await {
            Ok(failed) => failed,
            Err(_) => {
                tracing::error!(pr = %pr, "a cockpit turn failed inside the daemon");
                Some("The turn stopped unexpectedly".to_string())
            }
        };
        scenarios::end(&shared, &pr, id, failed);
        drop(live);
        tell_cockpit(&shared, &pr).await;
    });
}

/// Sets what the cockpit activity `id` is doing.
fn set_activity(shared: &Shared, pr: &PrRef, id: u64, what: String) {
    shared.tests.with(pr, |entry| {
        if let Some(activity) = entry.activity.as_mut().filter(|a| a.id == id) {
            activity.cockpit = Some(CockpitActivity::Writing { what });
        }
    });
}

/// The files the pull request changes, as the review caches know them.
async fn changed_files(shared: &Shared, pr: &PrRef) -> Vec<String> {
    shared
        .review_caches
        .get(pr)
        .await
        .map(|cache| cache.files.iter().map(|f| f.path.clone()).collect())
        .unwrap_or_default()
}

/// `WriteScenarios`: has the agent write scenarios, in the background.
pub(crate) async fn write_scenarios(shared: &Arc<Shared>, pr: &PrRef, ask: ScenarioAsk) -> Outcome {
    match &ask {
        ScenarioAsk::Describe { text } if !describe_ok(text) => {
            return reviews::bad_request(
                "Describe the scenario in one line of at most 600 characters",
            );
        }
        ScenarioAsk::Values { scenario } if !valid_scenario_slug(scenario) => {
            return reviews::bad_request("invalid scenario id");
        }
        _ => {}
    }
    let read = match scenarios::writable(shared, pr).await {
        Ok(read) => read,
        Err(out) => return out,
    };
    let review = match super::active(shared, pr) {
        Ok(review) => review,
        Err(out) => return out,
    };
    let Some(worktree) = read.worktree.clone() else {
        return reviews::invalid_state(cases::OPEN_FIRST);
    };
    let mut inputs = Vec::new();
    match &ask {
        ScenarioAsk::Values { scenario } => {
            let Some(at) = read.view.scenarios.iter().position(|s| s.slug == *scenario) else {
                return reviews::invalid_state(format!("No scenario {scenario} in {pr}"));
            };
            if let ItemSupport::Unsupported { reason } = &read.view.scenarios[at].support {
                return reviews::invalid_state(reason.clone());
            }
            inputs = read.scenarios[at].inputs.clone();
        }
        _ if read.view.scenarios.len() >= SCENARIOS_MAX => {
            return reviews::invalid_state(format!(
                "This review has {SCENARIOS_MAX} scenarios, the most it lists"
            ));
        }
        _ => {}
    }
    let name = sessions::name_for(shared, pr).await;
    let record = env::load(shared, pr);
    let tests_all = match env::recipe_state(&worktree) {
        RecipeState::Valid { recipe, .. } => Some(recipe.tests.all.clone()),
        _ => None,
    };
    let mut ctx = ScenarioPromptContext {
        pr_title: review.title.clone(),
        forked: true,
        functions: read.briefs.clone(),
        files: changed_files(shared, pr).await,
        runner: read.runner.unwrap_or(RunnerKind::Container),
        ecosystems: record
            .env
            .as_ref()
            .map(|env| env.signals.ecosystems.clone())
            .unwrap_or_default(),
        tests_all,
        existing: read
            .view
            .scenarios
            .iter()
            .map(|s| (s.slug.clone(), s.title.clone()))
            .collect(),
        ask: ask.clone(),
        inputs,
    };
    let prompt = scenario_prompt(&ctx);
    ctx.forked = false;
    let spec = Spec {
        origin: TurnOrigin::Scenarios,
        prompt,
        fresh_prompt: scenario_prompt(&ctx),
        failure: "write the scenarios",
    };
    let started = match super::begin(shared, pr, Doing::Scenarios, TestsPhase::Waiting) {
        Ok(started) => started,
        Err(refused) => return refused.outcome(pr),
    };
    let what = match &ask {
        ScenarioAsk::Values { scenario } => format!("values:{scenario}"),
        _ => "scenarios".to_string(),
    };
    set_activity(shared, pr, started.id, what);
    tell_cockpit(shared, pr).await;
    let (id, live, turn) = (started.id, started.live, started.turn);
    let job = {
        let (shared, pr) = (shared.clone(), pr.clone());
        let head = review.head_sha.clone();
        async move {
            let text = match run_text(&shared, &pr, spec, turn.clone()).await {
                Ok(text) => text,
                Err(_) if turn.fired() => return None,
                Err(message) => return Some(message),
            };
            let blocks = extract_cockpit_blocks(&text);
            if blocks.scenarios.is_empty() {
                return Some(format!("{name}'s answer holds no clusia-scenario block"));
            }
            let _guard = reviews::lock(&shared, &pr).await;
            if turn.fired() || !env::alive(&shared, &pr) {
                return None;
            }
            apply_scenarios(
                &shared,
                &pr,
                &worktree,
                &ask,
                &blocks.scenarios,
                &head,
                &name,
            )
        }
    };
    spawn_activity(shared, pr, id, live, job);
    Outcome::Ok(Reply::Ack)
}

/// Writes the valid scenario blocks of an answer, or the values of one. Answers why nothing was
/// written when no block was valid. The caller holds the review lock.
fn apply_scenarios(
    shared: &Shared,
    pr: &PrRef,
    worktree: &Path,
    ask: &ScenarioAsk,
    blocks: &[serde_json::Value],
    head: &str,
    name: &str,
) -> Option<String> {
    let not_valid = |reasons: &[String]| {
        Some(format!(
            "{name}'s scenario is not valid: {}",
            reasons_text(reasons)
        ))
    };
    let now = now_unix();
    if let ScenarioAsk::Values { scenario } = ask {
        let block = match ScenarioBlock::from_json(&blocks[0]) {
            Ok(block) => block,
            Err(reasons) => return not_valid(&reasons),
        };
        return match scenarios::apply_values(worktree, scenario, &block) {
            Ok(()) => None,
            Err(reasons) => not_valid(&reasons),
        };
    }
    let mut taken: BTreeSet<String> = BTreeSet::new();
    match cases::dir_at(worktree, &[".clusia", "e2e"], false) {
        Ok(Some(dir)) => match cases::list_names(&dir) {
            Ok(names) => taken.extend(names),
            Err(e) => return Some(format!("cannot read the scenarios: {e}")),
        },
        Ok(None) => {}
        Err(cases::Refusal::NotPlain) => return Some(E2E_ROOT_REFUSED.to_string()),
        Err(cases::Refusal::Io(message)) => return Some(message),
    }
    let existing = taken.iter().filter(|s| valid_scenario_slug(s)).count();
    let room = SCENARIOS_MAX.saturating_sub(existing);
    let limit = match ask {
        ScenarioAsk::Fresh => SCENARIOS_FIRST.min(room),
        ScenarioAsk::More => room,
        _ => 1.min(room),
    };
    let mut written = Vec::new();
    let mut invalid: Vec<String> = Vec::new();
    for raw in blocks {
        if written.len() >= limit {
            break;
        }
        let block = match ScenarioBlock::from_json(raw).and_then(|block| {
            clusia_testrun::scenario::check_run(&block.run, &block.inputs)
                .and_then(|()| {
                    clusia_testrun::scenario::rewrite_run(
                        &block.run,
                        &block.inputs,
                        &block.scripts[0].0,
                    )
                })
                .map_err(|reason| vec![reason])?;
            Ok(block)
        }) {
            Ok(block) => block,
            Err(reasons) => {
                invalid.extend(reasons);
                continue;
            }
        };
        let slug = scenario_slug(&block.title, &taken);
        if let Err(message) = scenarios::adopt_scenario(worktree, &slug, &block, head, now) {
            return Some(message);
        }
        taken.insert(slug.clone());
        written.push(slug);
    }
    if !written.is_empty() {
        // A new scenario never inherits what a removed one with the same slug left.
        let mut record = env::load(shared, pr);
        for slug in &written {
            record.scenario_approvals.remove(slug);
            scenarios::remove_results(&mut record, &format!("scenario:{slug}"));
        }
        env::save(shared, pr, &record);
    }
    if !invalid.is_empty() {
        if written.is_empty() {
            return not_valid(&invalid);
        }
        tracing::warn!(reasons = ?invalid, "some clusia-scenario blocks could not be used");
    }
    None
}

/// `WriteBench`: has the agent write the benchmark of one function, in the background.
pub(crate) async fn write_bench(
    shared: &Arc<Shared>,
    pr: &PrRef,
    function: &str,
    ask: BenchAsk,
) -> Outcome {
    let (read, at) = match cases::writable(shared, pr, function).await {
        Ok(found) => found,
        Err(out) => return out,
    };
    let review = match super::active(shared, pr) {
        Ok(review) => review,
        Err(out) => return out,
    };
    let (view, item) = (&read.view.functions[at], &read.items[at]);
    let Some(worktree) = read.worktree.clone() else {
        return reviews::invalid_state(cases::OPEN_FIRST);
    };
    let Some(lang) = view.lang else {
        return reviews::invalid_state(format!(
            "Clúsia cannot run {}: no driver can be written in its language",
            view.qualified
        ));
    };
    if let Support::Unsupported { reason } | Support::Stale { reason } = &view.support {
        return reviews::invalid_state(reason.clone());
    }
    let Some(stored_function) = item.stored.as_ref() else {
        return reviews::invalid_state(format!(
            "Write cases for {} first: a benchmark takes its parameters from them",
            view.qualified
        ));
    };
    if item.command.is_none() {
        return reviews::invalid_state(format!(
            "The recipe has no command to run {} functions",
            lang.label()
        ));
    }
    let params = stored_function.params.clone();
    let current = bench::stored_of(&worktree, &view.slug, &params);
    if ask == BenchAsk::Values && current.is_none() {
        return reviews::invalid_state(format!("There is no benchmark for {} yet", view.qualified));
    }
    let name = sessions::name_for(shared, pr).await;
    let mut ctx = BenchPromptContext {
        pr_title: review.title.clone(),
        forked: true,
        function: item.brief.clone(),
        params: params.clone(),
        driver: item.driver.clone(),
        command: item.command.clone(),
        ask,
        values: current.map(|s| s.values).unwrap_or_default(),
    };
    let prompt = bench_prompt(&ctx);
    ctx.forked = false;
    let spec = Spec {
        origin: TurnOrigin::Bench,
        prompt,
        fresh_prompt: bench_prompt(&ctx),
        failure: "write the benchmark",
    };
    let started = match super::begin(shared, pr, Doing::Bench, TestsPhase::Waiting) {
        Ok(started) => started,
        Err(refused) => return refused.outcome(pr),
    };
    set_activity(shared, pr, started.id, format!("bench:{}", view.slug));
    tell_cockpit(shared, pr).await;
    let (id, live, turn) = (started.id, started.live, started.turn);
    let (slug, qualified, file) = (
        view.slug.clone(),
        item.brief.qualified.clone(),
        item.brief.file.clone(),
    );
    let signature = item.brief.signature.clone();
    let head = review.head_sha.clone();
    let job = {
        let (shared, pr) = (shared.clone(), pr.clone());
        async move {
            let text = match run_text(&shared, &pr, spec, turn.clone()).await {
                Ok(text) => text,
                Err(_) if turn.fired() => return None,
                Err(message) => return Some(message),
            };
            let blocks = extract_cockpit_blocks(&text);
            let Some(raw) = blocks.benches.first() else {
                return Some(format!("{name}'s answer holds no clusia-bench block"));
            };
            let block = match BenchBlock::from_json(raw, &params) {
                Ok(block) => block,
                Err(reasons) => {
                    return Some(format!(
                        "{name}'s benchmark is not valid: {}",
                        reasons_text(&reasons)
                    ));
                }
            };
            if block.function != qualified || block.file != file {
                return Some(format!("{name}'s benchmark is for another function"));
            }
            let _guard = reviews::lock(&shared, &pr).await;
            if turn.fired() || !env::alive(&shared, &pr) {
                return None;
            }
            let written = match ask {
                BenchAsk::Fresh => bench::adopt_bench(
                    &worktree,
                    &NewBench {
                        slug: &slug,
                        lang,
                        qualified: &qualified,
                        file: &file,
                        signature: &signature,
                        head: &head,
                        now: now_unix(),
                    },
                    &block,
                ),
                BenchAsk::Values => bench::apply_values(&worktree, &slug, &params, &block),
            };
            written.err()
        }
    };
    spawn_activity(shared, pr, id, live, job);
    Outcome::Ok(Reply::Ack)
}

/// Appends a message to the conversation: the next id, and only the last `COPILOT_KEPT` kept.
fn append_message(
    record: &mut TestsRecord,
    from: CopilotFrom,
    item: Option<ItemRef>,
    text: String,
    suggestions: Vec<Suggestion>,
) {
    let id = record.copilot.iter().map(|m| m.id).max().unwrap_or(0) + 1;
    record.copilot.push(CopilotMessage {
        id,
        at: now_unix(),
        from,
        item,
        text,
        suggestions,
    });
    let extra = record.copilot.len().saturating_sub(COPILOT_KEPT);
    record.copilot.drain(..extra);
}

/// `AskCopilot`: has the agent explain, suggest or answer, in the background. It does not take
/// the review's slot, so it can answer while a comparison runs.
pub(crate) async fn ask_copilot(
    shared: &Arc<Shared>,
    pr: &PrRef,
    item: Option<ItemRef>,
    ask: CopilotAsk,
) -> Outcome {
    if let Some(item) = &item {
        match item.kind {
            ItemKind::Load => return reviews::invalid_state(LOAD_LATER),
            ItemKind::Scenario if !valid_scenario_slug(&item.id) => {
                return reviews::bad_request("invalid scenario id");
            }
            ItemKind::Bench if !valid_slug(&item.id) => {
                return reviews::bad_request("invalid function id");
            }
            _ => {}
        }
    }
    match &ask {
        CopilotAsk::Explain if item.is_none() => {
            return reviews::invalid_state("Choose a scenario or a benchmark to explain");
        }
        CopilotAsk::Free { text } if text.trim().is_empty() => {
            return reviews::invalid_state("Write a question first");
        }
        CopilotAsk::Free { text } if text.chars().count() > COPILOT_ASK_MAX => {
            return reviews::invalid_state(format!("At most {COPILOT_ASK_MAX} characters"));
        }
        _ => {}
    }
    let review = match super::active(shared, pr) {
        Ok(review) => review,
        Err(out) => return out,
    };
    if shared.tests.closing.load(Ordering::SeqCst) {
        return reviews::invalid_state(STOPPING);
    }
    let read = scenarios::read(shared, pr).await;
    if let Some(problem) = &read.shipped {
        return reviews::invalid_state(problem.clone());
    }
    let (item_text, own_title) = match &item {
        None => (None, None),
        Some(item) if item.kind == ItemKind::Scenario => {
            let Some(at) = read.view.scenarios.iter().position(|s| s.slug == item.id) else {
                return reviews::invalid_state(format!("No scenario {} in {pr}", item.id));
            };
            let view = &read.view.scenarios[at];
            (Some(scenario_item_text(view)), Some(view.title.clone()))
        }
        Some(item) => {
            let Some(at) = read.view.benches.iter().position(|b| b.slug == item.id) else {
                return reviews::invalid_state(format!("No benchmark {} in {pr}", item.id));
            };
            let view = &read.view.benches[at];
            (Some(bench_item_text(view)), Some(view.qualified.clone()))
        }
    };
    if ask == CopilotAsk::Explain
        && let Some(item) = &item
    {
        let results = results_of(&read.record, item);
        if results.single.is_none() && results.compare.is_none() {
            return reviews::invalid_state("Run this item first: there is nothing to explain yet");
        }
    }
    let stop = Stop::new();
    let busy = shared.tests.with(pr, |entry| {
        if shared.tests.closing.load(Ordering::SeqCst) {
            return Some(STOPPING);
        }
        if entry.copilot.is_some() {
            return Some(COPILOT_BUSY);
        }
        entry.copilot = Some(stop.clone());
        entry.copilot_failed = None;
        None
    });
    if let Some(message) = busy {
        return reviews::invalid_state(message);
    }
    let live = Live::new(&shared.tests);
    let asked = match &ask {
        CopilotAsk::Free { text } => text.trim().to_string(),
        CopilotAsk::Explain => "Explain this comparison".to_string(),
        CopilotAsk::Next => "What should I try next?".to_string(),
    };
    let history: Vec<CopilotMessage> = read.record.copilot.clone();
    let others: Vec<String> = read
        .view
        .scenarios
        .iter()
        .map(|s| s.title.clone())
        .chain(read.view.benches.iter().map(|b| b.qualified.clone()))
        .filter(|title| Some(title) != own_title.as_ref())
        .collect();
    let mut ctx = CopilotPromptContext {
        pr_title: review.title.clone(),
        forked: true,
        ask: ask.clone(),
        item: item_text,
        functions: read.briefs.clone(),
        others,
        history,
    };
    let prompt = copilot_prompt(&ctx);
    ctx.forked = false;
    let spec = Spec {
        origin: TurnOrigin::Copilot,
        prompt,
        fresh_prompt: copilot_prompt(&ctx),
        failure: "answer",
    };
    let reviewer_item = item.clone();
    env::update_record(shared, pr, |record| {
        append_message(
            record,
            CopilotFrom::Reviewer,
            reviewer_item,
            asked,
            Vec::new(),
        );
    })
    .await;
    tell_cockpit(shared, pr).await;
    let briefs: Vec<String> = read.briefs.iter().map(|b| b.qualified.clone()).collect();
    let job = {
        let (shared, pr, stop) = (shared.clone(), pr.clone(), stop.clone());
        async move {
            let text = match run_text(&shared, &pr, spec, stop.clone()).await {
                Ok(text) => text,
                Err(_) if stop.fired() => return Ok(None),
                Err(message) => return Err(message),
            };
            let blocks = extract_cockpit_blocks(&text);
            let suggestions: Vec<Suggestion> = blocks
                .suggestions
                .iter()
                .filter_map(|raw| Suggestion::from_json(raw).ok())
                .filter(|s| {
                    s.kind != ItemKind::Bench
                        || s.function.as_ref().is_some_and(|f| briefs.contains(f))
                })
                .take(SUGGESTIONS_MAX)
                .collect();
            Ok(Some((copilot_text(&blocks.text), suggestions)))
        }
    };
    let (shared, pr) = (shared.clone(), pr.clone());
    tokio::spawn(async move {
        let outcome = match tokio::spawn(job).await {
            Ok(outcome) => outcome,
            Err(_) => {
                tracing::error!(pr = %pr, "a copilot turn failed inside the daemon");
                Err("The QA copilot stopped unexpectedly".to_string())
            }
        };
        if let Ok(Some((text, suggestions))) = &outcome {
            let (text, suggestions) = (text.clone(), suggestions.clone());
            env::update_record(&shared, &pr, |record| {
                append_message(record, CopilotFrom::Agent, item, text, suggestions);
            })
            .await;
        }
        // A stop, a forget or a newer turn has already replaced this turn's slot: the new state
        // stays as it is and nothing is told about a review that may be gone.
        let current = shared.tests.with(&pr, |entry| {
            let current = entry
                .copilot
                .as_ref()
                .is_some_and(|c| Arc::ptr_eq(c, &stop));
            if current {
                entry.copilot = None;
                entry.copilot_failed = outcome.err();
            }
            current
        });
        drop(live);
        if current {
            tell_cockpit(&shared, &pr).await;
        }
    });
    Outcome::Ok(Reply::Ack)
}
