//! The QA cockpit's scenarios: what Clúsia keeps in `.clusia/e2e/<slug>/`, the reviewer's edits,
//! the approval of each command and script text, and the view the clients get.

use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use clusia_core::PrRef;
use clusia_core::cockpit::{
    COCKPIT_CHANGED, CockpitActivity, CockpitView, CopilotView, E2E_DIR, EDITS_MAX_BYTES, Input,
    InputView, ItemKind, ItemRef, ItemSupport, NO_REVIEW, ROUNDS_MAX, SCENARIO_FILE_MAX_BYTES,
    SCENARIO_METRICS, SCENARIO_ROUNDS_MIN, SCENARIO_TIMEOUT_MAX, SCENARIO_TIMEOUT_MIN,
    SCENARIOS_MAX, SCRIPT_MAX_BYTES, ScenarioApproval, ScenarioBlock, ScenarioEdits, ScenarioInput,
    ScenarioScript, ScenarioView, ScriptMeta, StoredScenario, approval_hash, check_input,
    scenario_hash, scenario_input_hash, scenario_status, scenarios_hash, script_sha,
    valid_scenario_slug, valid_script_name,
};
use clusia_core::functions::{FUNCTIONS_MAX, FunctionBrief};
use clusia_core::printable::printable;
use clusia_core::tests::{RunnerKind, TestsRecord};
use clusia_protocol::{ErrorCode, Event, Outcome, ProtocolError, Reply, topics};

use super::STOPPING;
use super::bench::{self, BenchItem};
use super::cases::{self, Refusal};
use super::compare::{self, Now};
use super::env;
use crate::reviews;
use crate::state::Shared;

pub(super) const E2E_FOLDER_REFUSED: &str =
    "Clúsia's folder for this scenario is not a plain folder";
pub(super) const E2E_ROOT_REFUSED: &str = "Clúsia's folder for scenarios is not a plain folder";

const SCENARIO_FILE: &str = "scenario.toml";
const EDITS_FILE: &str = "edits.json";
const E2E_PARTS: [&str; 2] = [".clusia", "e2e"];
/// The longest list of reasons a scenario that cannot be used shows, in characters.
const REASON_MAX: usize = 300;

/// What is stored for one scenario.
#[derive(Default)]
pub(super) struct Loaded {
    pub stored: Option<StoredScenario>,
    /// The script texts that could be read, by name.
    pub scripts: BTreeMap<String, String>,
    pub edits: ScenarioEdits,
    /// Its folder or one of its files is not plain: nothing more was read.
    pub refused: bool,
    /// Why its folder could not be opened, when that is not a link.
    pub problem: Option<String>,
}

/// Reads what is stored for `slug`, never following a link. A `scenario.toml` that does not parse
/// is no stored scenario; an `edits.json` that does not parse is no edits.
pub(super) fn load_scenario(worktree: &Path, slug: &str) -> Loaded {
    let refused = Loaded {
        refused: true,
        ..Loaded::default()
    };
    let parts = [E2E_PARTS[0], E2E_PARTS[1], slug];
    let dir = match cases::dir_at(worktree, &parts, false) {
        Ok(Some(dir)) => dir,
        Ok(None) => return Loaded::default(),
        Err(Refusal::NotPlain) => return refused,
        Err(Refusal::Io(message)) => {
            tracing::warn!(slug, message, "cannot open the folder of a scenario");
            return Loaded {
                problem: Some(message),
                ..Loaded::default()
            };
        }
    };
    let stored = match cases::read_capped(&dir, SCENARIO_FILE, SCENARIO_FILE_MAX_BYTES) {
        Ok(Some(bytes)) => std::str::from_utf8(&bytes).ok().and_then(|text| {
            match toml::from_str::<StoredScenario>(text) {
                Ok(stored) => Some(stored),
                Err(e) => {
                    tracing::warn!(slug, error = %e, "a scenario.toml could not be read");
                    None
                }
            }
        }),
        Ok(None) => None,
        Err(Refusal::NotPlain) => return refused,
        Err(Refusal::Io(message)) => {
            tracing::warn!(slug, message, "a scenario.toml could not be read");
            None
        }
    };
    let edits = match cases::read_capped(&dir, EDITS_FILE, EDITS_MAX_BYTES) {
        Ok(Some(bytes)) => serde_json::from_slice::<ScenarioEdits>(&bytes).unwrap_or_else(|e| {
            tracing::warn!(slug, error = %e, "an edits.json could not be read");
            ScenarioEdits::default()
        }),
        Ok(None) => ScenarioEdits::default(),
        Err(Refusal::NotPlain) => return refused,
        Err(Refusal::Io(message)) => {
            tracing::warn!(slug, message, "an edits.json could not be read");
            ScenarioEdits::default()
        }
    };
    let mut scripts = BTreeMap::new();
    for name in stored.iter().flat_map(|s| s.scripts.iter()) {
        if !valid_script_name(name) {
            continue;
        }
        match cases::read_capped(&dir, name, SCRIPT_MAX_BYTES) {
            Ok(Some(bytes)) => {
                if let Ok(text) = String::from_utf8(bytes) {
                    scripts.insert(name.clone(), text);
                }
            }
            Ok(None) => {}
            Err(Refusal::NotPlain) => return refused,
            Err(Refusal::Io(message)) => {
                tracing::warn!(slug, script = %name, message, "a script could not be read");
            }
        }
    }
    Loaded {
        stored,
        scripts,
        edits,
        refused: false,
        problem: None,
    }
}

/// A scenario as it would run: the agent's with the reviewer's edits laid over.
#[derive(Debug)]
pub(super) struct Effective {
    pub title: String,
    pub purpose: String,
    pub steps: Vec<String>,
    pub inputs: Vec<Input>,
    pub run: String,
    pub files: Vec<(String, String)>,
    pub values: BTreeMap<String, String>,
    pub rounds: u32,
    pub timeout_secs: u32,
    /// The rewritten command, which holds no value and no path.
    pub command: String,
}

/// The scenario as it would run, checked as a whole: the overlay goes back through the block's
/// rules, so the limits and character rules of a block apply to an edit and to a file that PR code
/// changed on disk. `Err` holds every reason it cannot run.
pub(super) fn effective(
    stored: &StoredScenario,
    scripts: &BTreeMap<String, String>,
    edits: &ScenarioEdits,
) -> Result<Effective, Vec<String>> {
    let mut files = Vec::new();
    for name in &stored.scripts {
        match edits.files.get(name).or_else(|| scripts.get(name)) {
            Some(text) => files.push((name.clone(), text.clone())),
            None => {
                return Err(vec![format!(
                    "The script {name} is missing or larger than {} KiB",
                    SCRIPT_MAX_BYTES / 1024
                )]);
            }
        }
    }
    let run = edits.run.clone().unwrap_or_else(|| stored.run.clone());
    let rounds = edits.rounds.unwrap_or(stored.rounds);
    let timeout_secs = edits.timeout_secs.unwrap_or(stored.timeout_secs);
    let inputs: Vec<serde_json::Value> = stored
        .inputs
        .iter()
        .map(|input| {
            let mut value = serde_json::json!({
                "name": input.name, "label": input.label,
                "kind": input.kind, "default": input.default,
            });
            // A block holds choices only for `one_of`.
            if !input.choices.is_empty() {
                value["choices"] = serde_json::json!(input.choices);
            }
            value
        })
        .collect();
    // `scripts` is an ordered array: the first one is `{script}`.
    let scripts_json: Vec<serde_json::Value> = files
        .iter()
        .map(|(name, text)| serde_json::json!({"name": name, "text": text}))
        .collect();
    let block = ScenarioBlock::from_json(&serde_json::json!({
        "title": stored.title, "purpose": stored.purpose, "steps": stored.steps,
        "inputs": inputs, "run": run, "scripts": scripts_json,
        "rounds": rounds, "timeout_secs": timeout_secs,
    }))?;
    clusia_testrun::scenario::check_run(&block.run, &block.inputs).map_err(|e| vec![e])?;
    let command = clusia_testrun::scenario::rewrite_run(&block.run, &block.inputs, &files[0].0)
        .map_err(|e| vec![e])?;
    let mut values: BTreeMap<String, String> = block
        .inputs
        .iter()
        .map(|i| (i.name.clone(), i.default.clone()))
        .collect();
    for input in &block.inputs {
        if let Some(value) = edits.values.get(&input.name)
            && check_input(input.kind, &input.choices, value).is_ok()
        {
            values.insert(input.name.clone(), value.clone());
        }
    }
    Ok(Effective {
        title: block.title,
        purpose: block.purpose,
        steps: block.steps,
        inputs: block.inputs,
        run: block.run,
        files,
        values,
        rounds,
        timeout_secs,
        command,
    })
}

/// A listed scenario with what a run or an approval needs of it.
#[allow(dead_code)]
pub(super) struct ScenarioItem {
    pub slug: String,
    pub stored: Option<StoredScenario>,
    pub edits: ScenarioEdits,
    pub inputs: Vec<Input>,
    pub values: BTreeMap<String, String>,
    pub files: Vec<(String, Vec<u8>)>,
    pub command: Option<String>,
    pub approval_hash: String,
    pub input_hash: String,
    pub approved: bool,
    pub rounds: u32,
    pub timeout_secs: u32,
}

/// The cockpit view and what it was read from; `scenarios[i]` belongs to `view.scenarios[i]`
/// and `benches[i]` to `view.benches[i]`.
#[allow(dead_code)]
pub(super) struct Read {
    pub view: CockpitView,
    pub worktree: Option<PathBuf>,
    /// The problem of a `.clusia/` the pull request ships.
    pub shipped: Option<String>,
    pub head: String,
    pub base: Option<String>,
    pub runner: Option<RunnerKind>,
    pub scenarios: Vec<ScenarioItem>,
    pub benches: Vec<BenchItem>,
    pub briefs: Vec<FunctionBrief>,
    pub record: TestsRecord,
}

impl Read {
    fn empty(view: CockpitView, head: String) -> Self {
        Read {
            view,
            worktree: None,
            shipped: None,
            head,
            base: None,
            runner: None,
            scenarios: Vec::new(),
            benches: Vec::new(),
            briefs: Vec::new(),
            record: TestsRecord::default(),
        }
    }
}

/// A reason that cannot be listed in full: printable, cut to `REASON_MAX` characters.
pub(super) fn reasons_text(reasons: &[String]) -> String {
    printable(&reasons.join("; "))
        .chars()
        .take(REASON_MAX)
        .collect()
}

struct Unusable<'a> {
    slug: &'a str,
    reason: String,
}

/// A scenario that cannot be listed as one: its slug as the title and why.
fn unusable_view(unusable: &Unusable<'_>, running: bool) -> (ScenarioView, ScenarioItem) {
    let support = ItemSupport::Unsupported {
        reason: unusable.reason.clone(),
    };
    let mut view = ScenarioView {
        slug: unusable.slug.to_string(),
        title: unusable.slug.to_string(),
        purpose: String::new(),
        steps: Vec::new(),
        inputs: Vec::new(),
        run: String::new(),
        command: None,
        scripts: Vec::new(),
        rounds: 0,
        timeout_secs: 0,
        edited: false,
        approval: ScenarioApproval::NotApproved,
        approval_hash: String::new(),
        input_hash: String::new(),
        support,
        last: None,
        comparison: None,
        status: String::new(),
        hash: String::new(),
    };
    view.hash = scenario_hash(&view.input_hash, false, 0, 0, &view.support);
    view.status = scenario_status(&view, running);
    let item = ScenarioItem {
        slug: unusable.slug.to_string(),
        stored: None,
        edits: ScenarioEdits::default(),
        inputs: Vec::new(),
        values: BTreeMap::new(),
        files: Vec::new(),
        command: None,
        approval_hash: String::new(),
        input_hash: String::new(),
        approved: false,
        rounds: 0,
        timeout_secs: 0,
    };
    (view, item)
}

/// One scenario's view and item, read from the worktree and held against the record.
fn read_one(
    worktree: &Path,
    slug: &str,
    record: &TestsRecord,
    now: &Now<'_>,
    running: bool,
) -> (ScenarioView, ScenarioItem) {
    let loaded = load_scenario(worktree, slug);
    let unusable = |reason: String| unusable_view(&Unusable { slug, reason }, running);
    if loaded.refused {
        return unusable(E2E_FOLDER_REFUSED.to_string());
    }
    if let Some(problem) = loaded.problem {
        return unusable(problem);
    }
    let Some(stored) = loaded.stored else {
        return unusable("scenario.toml is missing or not valid".to_string());
    };
    let eff = match effective(&stored, &loaded.scripts, &loaded.edits) {
        Ok(eff) => eff,
        Err(reasons) => return unusable(reasons_text(&reasons)),
    };
    let files: BTreeMap<String, String> = eff
        .files
        .iter()
        .map(|(name, text)| (name.clone(), script_sha(text.as_bytes())))
        .collect();
    let approval_hash = approval_hash(&eff.command, &files);
    let input_hash = scenario_input_hash(&approval_hash, &eff.values);
    let approval = match record.scenario_approvals.get(slug) {
        None => ScenarioApproval::NotApproved,
        Some(hash) if *hash == approval_hash => ScenarioApproval::Approved,
        Some(_) => ScenarioApproval::Changed,
    };
    let approved = approval == ScenarioApproval::Approved;
    let support = if approved {
        ItemSupport::Ready
    } else {
        ItemSupport::NeedsApproval
    };
    let inputs: Vec<InputView> = eff
        .inputs
        .iter()
        .map(|input| {
            let value = eff.values[&input.name].clone();
            InputView {
                edited: value != input.default,
                input: input.clone(),
                value,
            }
        })
        .collect();
    let scripts: Vec<ScriptMeta> = eff
        .files
        .iter()
        .map(|(name, text)| ScriptMeta {
            name: name.clone(),
            bytes: u32::try_from(text.len()).unwrap_or(u32::MAX),
            sha: script_sha(text.as_bytes()),
            edited: loaded.edits.files.contains_key(name) && loaded.scripts.get(name) != Some(text),
        })
        .collect();
    let edited = inputs.iter().any(|i| i.edited)
        || scripts.iter().any(|s| s.edited)
        || eff.run != stored.run
        || eff.rounds != stored.rounds
        || eff.timeout_secs != stored.timeout_secs;
    let item_ref = ItemRef {
        kind: ItemKind::Scenario,
        id: slug.to_string(),
    };
    let (last, comparison) =
        compare::shown(record, &item_ref, &input_hash, now, &SCENARIO_METRICS, true);
    let mut view = ScenarioView {
        slug: slug.to_string(),
        title: eff.title,
        purpose: eff.purpose,
        steps: eff.steps,
        inputs,
        run: eff.run,
        command: Some(eff.command.clone()),
        scripts,
        rounds: eff.rounds,
        timeout_secs: eff.timeout_secs,
        edited,
        approval,
        approval_hash: approval_hash.clone(),
        input_hash: input_hash.clone(),
        support,
        last,
        comparison,
        status: String::new(),
        hash: String::new(),
    };
    view.hash = scenario_hash(
        &input_hash,
        approved,
        view.rounds,
        view.timeout_secs,
        &view.support,
    );
    view.status = scenario_status(&view, running);
    let item = ScenarioItem {
        slug: slug.to_string(),
        stored: Some(stored),
        edits: loaded.edits,
        inputs: eff.inputs,
        values: eff.values,
        files: eff
            .files
            .into_iter()
            .map(|(name, text)| (name, text.into_bytes()))
            .collect(),
        command: Some(eff.command),
        approval_hash,
        input_hash,
        approved,
        rounds: view.rounds,
        timeout_secs: view.timeout_secs,
    };
    (view, item)
}

/// The cockpit of `pr`: its scenarios and benchmarks, the activity and the copilot.
pub(super) async fn read(shared: &Arc<Shared>, pr: &PrRef) -> Read {
    let review = match super::active(shared, pr) {
        Ok(review) => review,
        Err(_) => {
            let head = reviews::load_stored(shared, pr)
                .ok()
                .flatten()
                .map(|review| review.head_sha)
                .unwrap_or_default();
            let mut view = CockpitView::new(&head);
            view.note = Some(NO_REVIEW.into());
            return Read::empty(view, head);
        }
    };
    let head = review.head_sha;
    let snap = env::snapshot(shared, pr).await;
    let mut view = CockpitView::new(&head);
    let runner = snap.env.as_ref().map(|env| env.runner);
    view.runner = runner;
    let main = super::main_tree::main_state_of(shared, pr, &snap).await;
    let base = main.record.as_ref().map(|record| record.base.clone());
    view.base = base.clone();
    view.main = main.view;
    let (activity, failed, copilot_busy, copilot_failed) = shared.tests.with(pr, |entry| {
        (
            entry.activity.as_ref().and_then(|a| a.cockpit.clone()),
            entry.cockpit_failed.clone(),
            entry.copilot.is_some(),
            entry.copilot_failed.clone(),
        )
    });
    view.activity = activity;
    view.failed = failed;
    view.copilot = CopilotView {
        messages: snap.record.copilot.clone(),
        busy: copilot_busy,
        failed: copilot_failed,
    };
    let mut read = Read::empty(view, head.clone());
    read.record = snap.record.clone();
    read.base = base.clone();
    read.runner = runner;
    let Some(ctx) = snap.ctx else {
        read.view.note = Some(
            snap.shipped
                .unwrap_or_else(|| "Cannot read the git folder of this review".to_string()),
        );
        return read;
    };
    read.worktree = Some(ctx.worktree.clone());
    if let Some(problem) = snap.shipped {
        read.view.note = Some(problem.clone());
        read.shipped = Some(problem);
        return read;
    }
    let now_runner = runner.unwrap_or(RunnerKind::Container);
    let now = Now {
        head: &head,
        base: base.as_deref(),
        runner: now_runner,
    };
    let running: Vec<ItemRef> = match &read.view.activity {
        Some(CockpitActivity::Running { item, .. }) => vec![item.clone()],
        _ => Vec::new(),
    };
    list_scenarios(&mut read, &ctx.worktree, &now, &running);
    read.view.hash = scenarios_hash(&read.view.scenarios);

    // The benchmarks sit next to the cases of the changed functions.
    let functions = cases::read(shared, pr).await;
    read.briefs = functions
        .items
        .iter()
        .zip(&functions.view.functions)
        .filter(|(_, view)| view.lang.is_some())
        .take(FUNCTIONS_MAX)
        .map(|(item, _)| item.brief.clone())
        .collect();
    for (item, function) in functions.items.iter().zip(&functions.view.functions) {
        let is_running = running
            .iter()
            .any(|r| r.kind == ItemKind::Bench && r.id == function.slug);
        if let Some((view, bench_item)) = bench::read_one(
            &ctx.worktree,
            item,
            function,
            &snap.record,
            &now,
            is_running,
        ) {
            read.view.benches.push(view);
            read.benches.push(bench_item);
        }
    }
    read
}

/// Lists the scenarios of `.clusia/e2e` in slug order into `read`.
fn list_scenarios(read: &mut Read, worktree: &Path, now: &Now<'_>, running: &[ItemRef]) {
    let dir = match cases::dir_at(worktree, &E2E_PARTS, false) {
        Ok(Some(dir)) => dir,
        Ok(None) => return,
        Err(Refusal::NotPlain) => {
            read.view.note = Some(E2E_ROOT_REFUSED.into());
            return;
        }
        Err(Refusal::Io(message)) => {
            read.view.note = Some(message);
            return;
        }
    };
    let names = match cases::list_names(&dir) {
        Ok(names) => names,
        Err(e) => {
            read.view.note = Some(format!("cannot read {E2E_DIR}: {e}"));
            return;
        }
    };
    let slugs: Vec<String> = names
        .into_iter()
        .filter(|name| valid_scenario_slug(name))
        .collect();
    if slugs.len() > SCENARIOS_MAX {
        read.view.note = Some(format!(
            "{} more scenarios are not listed",
            slugs.len() - SCENARIOS_MAX
        ));
    }
    for slug in slugs.into_iter().take(SCENARIOS_MAX) {
        let is_running = running
            .iter()
            .any(|r| r.kind == ItemKind::Scenario && r.id == slug);
        let (view, item) = match cases::sub_dir(&dir, &slug, false) {
            Ok(Some(_)) => read_one(worktree, &slug, &read.record, now, is_running),
            Ok(None) => continue,
            Err(Refusal::NotPlain) => unusable_view(
                &Unusable {
                    slug: &slug,
                    reason: E2E_FOLDER_REFUSED.to_string(),
                },
                is_running,
            ),
            Err(Refusal::Io(message)) => unusable_view(
                &Unusable {
                    slug: &slug,
                    reason: message,
                },
                is_running,
            ),
        };
        read.view.scenarios.push(view);
        read.scenarios.push(item);
    }
}

/// Publishes the cockpit of `pr` to every window.
pub(super) async fn tell_cockpit(shared: &Arc<Shared>, pr: &PrRef) {
    let view = read(shared, pr).await.view;
    shared.publish(
        topics::TESTS,
        Event::CockpitChanged {
            pr: pr.clone(),
            view: Box::new(view),
        },
    );
}

/// Ends cockpit activity `id`, keeping `failed` until the next one begins.
pub(super) fn end(shared: &Shared, pr: &PrRef, id: u64, failed: Option<String>) {
    shared.tests.with(pr, |entry| {
        if entry.activity.as_ref().is_some_and(|a| a.id == id) {
            entry.activity = None;
            entry.cockpit_failed = failed;
        }
    });
}

/// Drops the results stored under `key`.
pub(super) fn remove_results(record: &mut TestsRecord, key: &str) {
    record.cockpit_results.remove(key);
}

/// The checks every write makes first: an open review, no shutdown, no shipped `.clusia/` and a
/// plain folder for the scenarios.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
pub(super) async fn writable(shared: &Arc<Shared>, pr: &PrRef) -> Result<Read, Outcome> {
    super::active(shared, pr)?;
    if shared.tests.closing.load(Ordering::SeqCst) {
        return Err(reviews::invalid_state(STOPPING));
    }
    let read = read(shared, pr).await;
    if let Some(problem) = &read.shipped {
        return Err(reviews::invalid_state(problem.clone()));
    }
    if read.view.note.as_deref() == Some(E2E_ROOT_REFUSED) {
        return Err(reviews::invalid_state(E2E_ROOT_REFUSED));
    }
    Ok(read)
}

/// The position of scenario `slug` in `read`, refusing an unknown one and a refused folder.
#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
fn find(read: &Read, pr: &PrRef, slug: &str) -> Result<usize, Outcome> {
    let Some(at) = read.view.scenarios.iter().position(|s| s.slug == slug) else {
        return Err(reviews::invalid_state(format!(
            "No scenario {slug} in {pr}"
        )));
    };
    if matches!(
        &read.view.scenarios[at].support,
        ItemSupport::Unsupported { reason } if reason == E2E_FOLDER_REFUSED
    ) {
        return Err(reviews::invalid_state(E2E_FOLDER_REFUSED));
    }
    Ok(at)
}

#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
fn slug_ok(slug: &str) -> Result<(), Outcome> {
    if valid_scenario_slug(slug) {
        Ok(())
    } else {
        Err(reviews::bad_request("invalid scenario id"))
    }
}

fn write_failed(message: String) -> Outcome {
    if message == E2E_FOLDER_REFUSED {
        reviews::invalid_state(message)
    } else {
        Outcome::Err(ProtocolError::new(ErrorCode::Internal, message))
    }
}

fn refusal_text(refusal: Refusal) -> String {
    match refusal {
        Refusal::NotPlain => E2E_FOLDER_REFUSED.to_string(),
        Refusal::Io(message) => message,
    }
}

/// The folder of scenario `slug`, made when `create`.
fn scenario_folder(worktree: &Path, slug: &str, create: bool) -> Result<Option<OwnedFd>, String> {
    let parts = [E2E_PARTS[0], E2E_PARTS[1], slug];
    cases::dir_at(worktree, &parts, create).map_err(refusal_text)
}

/// Writes `edits.json`, or removes it when nothing is left of the overlay.
fn store_edits(worktree: &Path, slug: &str, edits: &ScenarioEdits) -> Result<(), String> {
    let Some(dir) = scenario_folder(worktree, slug, false)? else {
        return Ok(());
    };
    if *edits == ScenarioEdits::default() {
        return cases::remove_at(&dir, EDITS_FILE)
            .map_err(|e| format!("cannot remove {EDITS_FILE}: {e}"));
    }
    let json = serde_json::to_vec_pretty(edits).map_err(|e| e.to_string())?;
    cases::write_whole(&dir, EDITS_FILE, &json)
        .map_err(|e| format!("cannot write {EDITS_FILE}: {e}"))
}

/// Writes a new scenario: the scripts first and `scenario.toml` last, so a half-written scenario
/// is never listed as valid. The caller holds the review lock.
pub(super) fn adopt_scenario(
    worktree: &Path,
    slug: &str,
    block: &ScenarioBlock,
    head: &str,
    now: i64,
) -> Result<(), String> {
    let dir = scenario_folder(worktree, slug, true)?
        .ok_or_else(|| format!("cannot make the folder of {slug}"))?;
    for (name, text) in &block.scripts {
        cases::write_whole(&dir, name, text.as_bytes())
            .map_err(|e| format!("cannot write {name}: {e}"))?;
    }
    let stored = StoredScenario {
        title: block.title.clone(),
        purpose: block.purpose.clone(),
        steps: block.steps.clone(),
        inputs: block.inputs.clone(),
        run: block.run.clone(),
        scripts: block.scripts.iter().map(|(name, _)| name.clone()).collect(),
        rounds: block
            .rounds
            .unwrap_or(clusia_core::cockpit::SCENARIO_ROUNDS_DEFAULT),
        timeout_secs: block
            .timeout_secs
            .unwrap_or(clusia_core::cockpit::SCENARIO_TIMEOUT_DEFAULT),
        written_head: head.to_string(),
        written_at: now,
    };
    write_stored(&dir, &stored)
}

fn write_stored(dir: &OwnedFd, stored: &StoredScenario) -> Result<(), String> {
    let value = toml::Value::try_from(stored).map_err(|e| e.to_string())?;
    let text = toml::to_string(&value).map_err(|e| e.to_string())?;
    cases::write_whole(dir, SCENARIO_FILE, text.as_bytes())
        .map_err(|e| format!("cannot write {SCENARIO_FILE}: {e}"))
}

/// Replaces the defaults of scenario `slug` by the ones of `block`, which must name exactly its
/// inputs and give each a value that fits; the reviewer's value overlay goes, its other overlays
/// stay. The caller holds the review lock.
pub(super) fn apply_values(
    worktree: &Path,
    slug: &str,
    block: &ScenarioBlock,
) -> Result<(), Vec<String>> {
    let loaded = load_scenario(worktree, slug);
    let Some(mut stored) = loaded.stored else {
        return Err(vec![format!("No scenario {slug} to give values to")]);
    };
    let mut bad = Vec::new();
    let names =
        |inputs: &[Input]| -> BTreeSet<String> { inputs.iter().map(|i| i.name.clone()).collect() };
    if names(&stored.inputs) != names(&block.inputs) {
        bad.push("the inputs must be the scenario's own".to_string());
    }
    for input in &stored.inputs {
        if let Some(given) = block.inputs.iter().find(|i| i.name == input.name)
            && let Err(message) = check_input(input.kind, &input.choices, &given.default)
        {
            bad.push(format!("{}: {message}", input.name));
        }
    }
    if !bad.is_empty() {
        return Err(bad);
    }
    for input in &mut stored.inputs {
        if let Some(given) = block.inputs.iter().find(|i| i.name == input.name) {
            input.default.clone_from(&given.default);
        }
    }
    let dir = scenario_folder(worktree, slug, false)
        .map_err(|e| vec![e])?
        .ok_or_else(|| vec![format!("No scenario {slug} to give values to")])?;
    write_stored(&dir, &stored).map_err(|e| vec![e])?;
    let mut edits = loaded.edits;
    edits.values.clear();
    store_edits(worktree, slug, &edits).map_err(|e| vec![e])
}

/// The view after a write, published and answered.
pub(super) async fn changed(shared: &Arc<Shared>, pr: &PrRef) -> Outcome {
    let view = read(shared, pr).await.view;
    shared.publish(
        topics::TESTS,
        Event::CockpitChanged {
            pr: pr.clone(),
            view: Box::new(view.clone()),
        },
    );
    Outcome::Ok(Reply::Cockpit(Box::new(view)))
}

/// `GetCockpit`.
pub(crate) async fn cockpit(shared: &Arc<Shared>, pr: &PrRef) -> Outcome {
    Outcome::Ok(Reply::Cockpit(Box::new(read(shared, pr).await.view)))
}

/// What an edit works on, read again under the review lock.
struct Editing {
    worktree: PathBuf,
    stored: StoredScenario,
    scripts: BTreeMap<String, String>,
}

#[allow(clippy::result_large_err)] // `Outcome` is the handlers' error currency
fn editing(read: &Read, slug: &str) -> Result<Editing, Outcome> {
    let Some(worktree) = read.worktree.clone() else {
        return Err(reviews::invalid_state(NO_REVIEW));
    };
    let loaded = load_scenario(&worktree, slug);
    if loaded.refused {
        return Err(reviews::invalid_state(E2E_FOLDER_REFUSED));
    }
    if let Some(problem) = loaded.problem {
        return Err(reviews::invalid_state(problem));
    }
    let Some(stored) = loaded.stored else {
        return Err(reviews::invalid_state(format!(
            "The scenario {slug} cannot be read"
        )));
    };
    Ok(Editing {
        worktree,
        stored,
        scripts: loaded.scripts,
    })
}

/// `SaveScenario`: the reviewer's whole overlay of one scenario. Only what differs from the
/// agent's is kept.
pub(crate) async fn save_scenario(
    shared: &Arc<Shared>,
    pr: &PrRef,
    scenario: &str,
    input: ScenarioInput,
) -> Outcome {
    if let Err(out) = slug_ok(scenario) {
        return out;
    }
    let read = match writable(shared, pr).await {
        Ok(read) => read,
        Err(out) => return out,
    };
    if let Err(out) = find(&read, pr, scenario) {
        return out;
    }
    if let Some(rounds) = input.rounds
        && !(SCENARIO_ROUNDS_MIN..=ROUNDS_MAX).contains(&rounds)
    {
        return reviews::bad_request(format!(
            "Rounds must be between {SCENARIO_ROUNDS_MIN} and {ROUNDS_MAX}"
        ));
    }
    if let Some(secs) = input.timeout_secs
        && !(SCENARIO_TIMEOUT_MIN..=SCENARIO_TIMEOUT_MAX).contains(&secs)
    {
        return reviews::bad_request(format!(
            "The timeout must be between {SCENARIO_TIMEOUT_MIN} and {SCENARIO_TIMEOUT_MAX} seconds"
        ));
    }
    {
        let _guard = reviews::lock(shared, pr).await;
        let Editing {
            worktree,
            stored,
            scripts,
            ..
        } = match editing(&read, scenario) {
            Ok(editing) => editing,
            Err(out) => return out,
        };
        let mut edits = ScenarioEdits::default();
        for (name, value) in &input.values {
            let Some(declared) = stored.inputs.iter().find(|i| i.name == *name) else {
                return reviews::bad_request(format!("No input {name} in this scenario"));
            };
            if let Err(message) = check_input(declared.kind, &declared.choices, value) {
                return reviews::bad_request(message);
            }
            if *value != declared.default {
                edits.values.insert(name.clone(), value.clone());
            }
        }
        for (name, text) in &input.files {
            if !stored.scripts.contains(name) {
                return reviews::bad_request(format!("No script {name} in this scenario"));
            }
            if scripts.get(name) != Some(text) {
                edits.files.insert(name.clone(), text.clone());
            }
        }
        edits.run = input.run.clone().filter(|run| *run != stored.run);
        edits.rounds = input.rounds.filter(|r| *r != stored.rounds);
        edits.timeout_secs = input.timeout_secs.filter(|t| *t != stored.timeout_secs);
        if let Err(reasons) = effective(&stored, &scripts, &edits) {
            return reviews::bad_request(reasons.join("; "));
        }
        if let Err(message) = store_edits(&worktree, scenario, &edits) {
            return write_failed(message);
        }
    }
    changed(shared, pr).await
}

/// `ResetScenario`: drops the reviewer's edits of one scenario.
pub(crate) async fn reset_scenario(shared: &Arc<Shared>, pr: &PrRef, scenario: &str) -> Outcome {
    if let Err(out) = slug_ok(scenario) {
        return out;
    }
    let read = match writable(shared, pr).await {
        Ok(read) => read,
        Err(out) => return out,
    };
    if let Err(out) = find(&read, pr, scenario) {
        return out;
    }
    {
        let _guard = reviews::lock(shared, pr).await;
        let Some(worktree) = read.worktree.clone() else {
            return reviews::invalid_state(NO_REVIEW);
        };
        if let Err(message) = store_edits(&worktree, scenario, &ScenarioEdits::default()) {
            return write_failed(message);
        }
    }
    changed(shared, pr).await
}

/// `RemoveScenario`: removes the folder, its approval and its results.
pub(crate) async fn remove_scenario(shared: &Arc<Shared>, pr: &PrRef, scenario: &str) -> Outcome {
    if let Err(out) = slug_ok(scenario) {
        return out;
    }
    let read = match writable(shared, pr).await {
        Ok(read) => read,
        Err(out) => return out,
    };
    if let Err(out) = find(&read, pr, scenario) {
        return out;
    }
    {
        let _guard = reviews::lock(shared, pr).await;
        let Some(worktree) = read.worktree.clone() else {
            return reviews::invalid_state(NO_REVIEW);
        };
        if let Err(message) = remove_folder(&worktree, scenario) {
            return write_failed(message);
        }
        let mut record = env::load(shared, pr);
        record.scenario_approvals.remove(scenario);
        remove_results(&mut record, &format!("scenario:{scenario}"));
        env::save(shared, pr, &record);
    }
    changed(shared, pr).await
}

/// Removes what Clúsia keeps of a scenario, file by file, then the folder. Anything else in the
/// folder leaves it in place, with a warning.
fn remove_folder(worktree: &Path, slug: &str) -> Result<(), String> {
    let loaded = load_scenario(worktree, slug);
    let Some(dir) = scenario_folder(worktree, slug, false)? else {
        return Ok(());
    };
    let mut names: Vec<String> = loaded
        .stored
        .iter()
        .flat_map(|s| s.scripts.iter().cloned())
        .filter(|name| valid_script_name(name))
        .collect();
    names.push(SCENARIO_FILE.to_string());
    names.push(EDITS_FILE.to_string());
    for name in &names {
        cases::remove_at(&dir, name).map_err(|e| format!("cannot remove {name}: {e}"))?;
    }
    let Some(parent) = cases::dir_at(worktree, &E2E_PARTS, false).map_err(refusal_text)? else {
        return Ok(());
    };
    if let Err(e) = cases::remove_dir_at(&parent, slug) {
        tracing::warn!(slug, error = %e, "the folder of a removed scenario is not empty");
    }
    Ok(())
}

/// `ApproveScenario`: the reviewer read the command and scripts that hash to `hash`.
pub(crate) async fn approve_scenario(
    shared: &Arc<Shared>,
    pr: &PrRef,
    scenario: &str,
    hash: &str,
) -> Outcome {
    if let Err(out) = slug_ok(scenario) {
        return out;
    }
    if let Err(out) = writable(shared, pr).await {
        return out;
    }
    {
        let _guard = reviews::lock(shared, pr).await;
        // Read again under the lock: the approval is of what is on disk now.
        let read = match writable(shared, pr).await {
            Ok(read) => read,
            Err(out) => return out,
        };
        let at = match find(&read, pr, scenario) {
            Ok(at) => at,
            Err(out) => return out,
        };
        let view = &read.view.scenarios[at];
        if let ItemSupport::Unsupported { reason } = &view.support {
            return reviews::invalid_state(reason.clone());
        }
        if view.approval_hash != hash {
            return reviews::invalid_state(COCKPIT_CHANGED);
        }
        let mut record = env::load(shared, pr);
        record
            .scenario_approvals
            .insert(scenario.to_string(), hash.to_string());
        env::save(shared, pr, &record);
    }
    changed(shared, pr).await
}

/// `GetScenarioScript`: the effective scripts and the approval hash of one scenario.
pub(crate) async fn scenario_script(shared: &Arc<Shared>, pr: &PrRef, scenario: &str) -> Outcome {
    if let Err(out) = slug_ok(scenario) {
        return out;
    }
    let read = match writable(shared, pr).await {
        Ok(read) => read,
        Err(out) => return out,
    };
    let at = match find(&read, pr, scenario) {
        Ok(at) => at,
        Err(out) => return out,
    };
    if let ItemSupport::Unsupported { reason } = &read.view.scenarios[at].support {
        return reviews::invalid_state(reason.clone());
    }
    let item = &read.scenarios[at];
    Outcome::Ok(Reply::ScenarioScript(Box::new(ScenarioScript {
        scenario: scenario.to_string(),
        files: item
            .files
            .iter()
            .map(|(name, bytes)| (name.clone(), String::from_utf8_lossy(bytes).into_owned()))
            .collect(),
        approval_hash: item.approval_hash.clone(),
    })))
}
