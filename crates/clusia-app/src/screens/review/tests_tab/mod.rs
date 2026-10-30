//! The Tests tab's state: the per-review model the bridge fills, the tab state derived from it,
//! the machine's engine and disk, and the systems that ask the daemon for what the tab shows.
//!
//! Everything here comes from the pull request, its agent or its tooling in the end, so the
//! text helpers make it printable and nothing in this module acts on its content.

pub mod body;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use bevy::prelude::*;
use bevy::window::{RequestRedraw, WindowFocused};
use clusia_core::PrRef;
use clusia_core::printable::{plain_relative, printable};
use clusia_core::tests::{
    DiskReport, Engine, EngineKind, OUTPUT_EVERY_MS, RECIPE_CHANGED, RESULTS_MAX, RunnerKind,
    SetupPlan, StepState, TAIL_MAX, TestCaseResult, TestEngineInfo, TestRun, TestScope, TestStatus,
    TestsPhase, TestsView, keep_runs,
};

use crate::bridge::{Ask, Asks, Connection, Model, TestsAskKind, TestsTell};
use crate::review_state::{ReviewSection, ReviewTabs};
use crate::screens::review::ReviewSystems;
use crate::screens::review::activity::MARK;
use crate::screens::review::shell::SectionBody;

pub const TEST_ROW_HEIGHT: f32 = 31.0;
pub const CHECKS_SHOWN: usize = 12;

/// The tests of every open review, by review.
#[derive(Resource, Default)]
pub struct TestsModels(pub HashMap<PrRef, TestsModel>);

/// What the machine offers and keeps for tests, as Config › Tests shows it.
#[derive(Resource, Default)]
pub struct TestMachine {
    pub engine: Option<Result<TestEngineInfo, String>>,
    pub disk: Option<Result<DiskReport, String>>,
    pub checking: bool,
    pub cleaning: bool,
    pub asked: bool,
    pub confirm: bool,
}

#[derive(Default)]
pub struct TestsModel {
    pub view: Option<TestsView>,
    /// Whether the view was asked for and not lost with a connection.
    pub asked: bool,
    pub loaded: bool,
    pub live: Option<LiveRun>,
    /// The state and the output tail of each setup step the daemon told while it ran.
    pub steps: BTreeMap<String, (StepState, Option<String>)>,
    pub pending: Option<TestsAskKind>,
    pub sub: SubTab,
    pub filter: TestFilter,
    pub selected: Option<String>,
    pub plan: Option<PlanModal>,
    pub plan_wanted: bool,
    /// The recipe hashes whose setup plan opened on its own already.
    pub auto_opened: BTreeSet<String>,
    /// A setup this window started has not ended yet.
    pub prepared_here: bool,
    pub proposal_open: bool,
    pub reread_on_focus: bool,
    /// Bumped by every change, so a region can tell its view may differ.
    pub version: u64,
    /// The review head the view was last asked for.
    pub asked_head: Option<String>,
    pub list_wanted: bool,
    /// The scope of the run this window asked for, until the daemon says it started.
    pub run_scope: Option<TestScope>,
}

/// The run that goes now: its results so far and the end of its output.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveRun {
    pub id: String,
    pub scope: Option<TestScope>,
    pub results: Vec<TestCaseResult>,
    pub tail: String,
    pub tail_at: f64,
    /// The newest output, waiting for `tail` to take it at most every `OUTPUT_EVERY_MS`.
    pub next_tail: Option<String>,
}

/// The setup plan modal as the reviewer sees it: the plan it drew and what is checked.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanModal {
    pub recipe_hash: String,
    pub shown: Vec<String>,
    pub checked: BTreeSet<String>,
    pub changed_note: bool,
    pub sending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubTab {
    #[default]
    Auto,
    Functions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TestFilter {
    #[default]
    All,
    Failed,
    Near,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabState {
    Empty,
    Generating { activity: Option<String> },
    NeedsSetup,
    Preparing { step: Option<String> },
    Ready,
    Listing,
    Running,
    Done,
    Stale,
    Failed { message: String },
    Refused { reason: String },
}

/// What the tab shows for `view`; the first match wins.
pub fn tab_state(view: &TestsView, live: Option<&LiveRun>) -> TabState {
    let blocked = view.plan.as_ref().and_then(|p| p.blocked.as_deref());
    match (&view.phase, blocked) {
        (TestsPhase::Generating { activity }, _) => TabState::Generating {
            activity: activity.clone(),
        },
        (TestsPhase::Failed { message }, Some(b)) if b == message => TabState::Refused {
            reason: message.clone(),
        },
        (TestsPhase::Failed { message }, _) => TabState::Failed {
            message: message.clone(),
        },
        (TestsPhase::NotGenerated, _) => TabState::Empty,
        (_, Some(reason)) => TabState::Refused {
            reason: reason.to_string(),
        },
        (TestsPhase::Running { run }, None) if run == "list" => TabState::Listing,
        (TestsPhase::Running { .. }, None) => TabState::Running,
        (TestsPhase::Waiting, None) if live.is_some() => TabState::Running,
        (TestsPhase::Waiting, None) => TabState::Preparing { step: None },
        (TestsPhase::Preparing { step }, None) => TabState::Preparing {
            step: Some(step.clone()),
        },
        (TestsPhase::NeedsSetup, None) => TabState::NeedsSetup,
        (TestsPhase::Ready, None) => match view.runs.first() {
            Some(run) if run.head == view.head => TabState::Done,
            Some(_) => TabState::Stale,
            None => TabState::Ready,
        },
    }
}

/// Whether the plan has something the reviewer can approve now.
fn approvable(plan: &SetupPlan) -> bool {
    plan.blocked.is_none() && (!plan.commands_approved || !approvable_ids(plan).is_empty())
}

/// The steps a setup would run: the ones missing or failed.
fn approvable_ids(plan: &SetupPlan) -> BTreeSet<String> {
    plan.steps
        .iter()
        .filter(|s| matches!(s.state, StepState::Missing | StepState::Failed { .. }))
        .map(|s| s.id.clone())
        .collect()
}

/// The end of `text`, at most `TAIL_MAX` bytes, starting on a char boundary.
fn keep_tail(text: &str) -> String {
    if text.len() <= TAIL_MAX {
        return text.to_string();
    }
    let mut start = text.len() - TAIL_MAX;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

fn fresh_live(id: &str, scope: Option<TestScope>) -> LiveRun {
    LiveRun {
        id: id.to_string(),
        scope,
        results: Vec::new(),
        tail: String::new(),
        tail_at: 0.0,
        next_tail: None,
    }
}

/// The live run of `id`: the one that goes, or a new one when another (or none) was told.
fn live_for<'a>(live: &'a mut Option<LiveRun>, id: &str) -> &'a mut LiveRun {
    if live.as_ref().is_none_or(|l| l.id != id) {
        *live = Some(fresh_live(id, None));
    }
    live.as_mut().expect("a live run was just made")
}

fn status_rank(status: TestStatus) -> u8 {
    match status {
        TestStatus::Failed => 0,
        TestStatus::Passed => 1,
        TestStatus::Skipped => 2,
    }
}

impl TestsModel {
    fn touch(&mut self) {
        self.version += 1;
    }

    pub fn state(&self) -> Option<TabState> {
        self.view
            .as_ref()
            .map(|view| tab_state(view, self.live.as_ref()))
    }

    /// `●` while the tab works, else the failures of a done run.
    pub fn badge(&self) -> Option<String> {
        match self.state()? {
            TabState::Generating { .. }
            | TabState::Preparing { .. }
            | TabState::Listing
            | TabState::Running => Some(MARK.to_string()),
            TabState::Done => {
                let failed = self
                    .listed_run()?
                    .results
                    .iter()
                    .filter(|r| r.status == TestStatus::Failed)
                    .count();
                (failed > 0).then(|| failed.to_string())
            }
            _ => None,
        }
    }

    /// Whether the daemon is doing something for this review.
    pub fn busy(&self) -> bool {
        self.view.as_ref().is_some_and(|v| {
            matches!(
                v.phase,
                TestsPhase::Generating { .. }
                    | TestsPhase::Waiting
                    | TestsPhase::Preparing { .. }
                    | TestsPhase::Running { .. }
            )
        })
    }

    /// The newest run, whatever its head.
    pub fn listed_run(&self) -> Option<&TestRun> {
        self.view.as_ref().and_then(|v| v.runs.first())
    }

    /// The rows of the list: the live results while a run goes, else the newest run's. Failed
    /// first, then passed, then skipped, each in the order they came in.
    pub fn rows(&self, filter: TestFilter) -> Vec<&TestCaseResult> {
        let results: &[TestCaseResult] = match (&self.live, self.listed_run()) {
            (Some(live), _) => &live.results,
            (None, Some(run)) => &run.results,
            (None, None) => &[],
        };
        let near: BTreeSet<&str> = self
            .view
            .as_ref()
            .map(|v| v.near.iter().map(|n| n.name.as_str()).collect())
            .unwrap_or_default();
        let mut rows: Vec<&TestCaseResult> = results
            .iter()
            .filter(|r| match filter {
                TestFilter::All => true,
                TestFilter::Failed => r.status == TestStatus::Failed,
                TestFilter::Near => near.contains(r.name.as_str()),
            })
            .collect();
        rows.sort_by_key(|r| status_rank(r.status));
        rows
    }

    /// Opens the setup plan modal on the plan the view holds.
    pub fn open_plan(&mut self) {
        self.plan_wanted = false;
        if self.plan.is_none()
            && let Some(plan) = self.view.as_ref().and_then(|v| v.plan.as_ref())
        {
            self.plan = Some(PlanModal {
                recipe_hash: plan.recipe_hash.clone(),
                shown: plan.steps.iter().map(|s| s.id.clone()).collect(),
                checked: approvable_ids(plan),
                ..PlanModal::default()
            });
        }
        self.touch();
    }

    /// Records that a tests ask went out, so its button stays off until the answer comes.
    pub fn sent(&mut self, kind: TestsAskKind) {
        self.touch();
        self.pending = Some(kind);
        if kind == TestsAskKind::Prepare {
            self.prepared_here = true;
            if let Some(modal) = self.plan.as_mut() {
                modal.sending = true;
                modal.changed_note = false;
            }
        }
    }

    /// `sent` for a run, which also remembers what it was asked to run.
    pub fn sent_run(&mut self, scope: TestScope) {
        self.touch();
        self.pending = Some(TestsAskKind::Run);
        self.run_scope = Some(scope);
    }

    pub fn apply(&mut self, tell: &TestsTell) {
        match tell {
            TestsTell::Loaded { view, .. } => self.loaded(view),
            TestsTell::Step {
                step, state, tail, ..
            } => {
                self.steps
                    .insert(step.clone(), (state.clone(), tail.clone()));
            }
            TestsTell::RunStarted { run, .. } => {
                let scope = self.run_scope.take();
                match self.live.as_mut() {
                    Some(live) if live.id == *run => live.scope = scope,
                    _ => self.live = Some(fresh_live(run, scope)),
                }
                self.selected = None;
                self.pending = None;
            }
            TestsTell::Result { run, result, .. } => {
                let live = live_for(&mut self.live, run);
                if live.results.len() < RESULTS_MAX {
                    live.results.push(result.clone());
                }
            }
            TestsTell::Output { run, tail, .. } => {
                live_for(&mut self.live, run).next_tail = Some(keep_tail(tail));
            }
            TestsTell::RunDone { run, .. } => {
                if let Some(view) = self.view.as_mut() {
                    view.runs.retain(|r| r.id != run.id);
                    view.runs.insert(0, (**run).clone());
                    keep_runs(&mut view.runs);
                }
                self.live = None;
                self.selected = run
                    .results
                    .iter()
                    .find(|r| r.status == TestStatus::Failed)
                    .map(|r| r.name.clone());
            }
            TestsTell::Listed { .. } => {
                self.pending = None;
                self.asked = false;
            }
            TestsTell::Refused { ask, message, .. } => {
                self.pending = None;
                if *ask == TestsAskKind::Prepare {
                    // A setup this window did not start is not one it lists after.
                    self.prepared_here = false;
                    if let Some(modal) = self.plan.as_mut() {
                        modal.sending = false;
                    }
                }
                if *ask == TestsAskKind::Run {
                    self.run_scope = None;
                }
                if message == RECIPE_CHANGED {
                    self.asked = false;
                    if let Some(modal) = self.plan.as_mut() {
                        modal.changed_note = true;
                    }
                }
            }
        }
        self.touch();
    }

    fn loaded(&mut self, view: &TestsView) {
        let before = self.view.as_ref().map(|v| v.phase.clone());
        self.loaded = true;
        self.asked = true;
        self.pending = None;
        if self.asked_head.is_none() {
            self.asked_head = Some(view.head.clone());
        }
        match &view.phase {
            TestsPhase::Running { run } if run != "list" && self.live.is_none() => {
                self.live = Some(fresh_live(run, None));
            }
            TestsPhase::Running { .. } | TestsPhase::Waiting | TestsPhase::Preparing { .. } => {}
            _ => {
                self.live = None;
                self.steps.clear();
            }
        }
        if let (Some(modal), Some(plan)) = (self.plan.as_mut(), view.plan.as_ref()) {
            let changed = modal.recipe_hash != plan.recipe_hash;
            let fits = approvable_ids(plan);
            modal.recipe_hash = plan.recipe_hash.clone();
            modal.shown = plan.steps.iter().map(|s| s.id.clone()).collect();
            modal.checked = modal.checked.intersection(&fits).cloned().collect();
            modal.changed_note |= changed;
        }
        if matches!(before, Some(TestsPhase::Generating { .. }))
            && view.phase == TestsPhase::NeedsSetup
            && let Some(plan) = view.plan.as_ref()
            && approvable(plan)
            && self.auto_opened.insert(plan.recipe_hash.clone())
        {
            self.plan_wanted = true;
        }
        if self.prepared_here
            && !matches!(
                view.phase,
                TestsPhase::Waiting | TestsPhase::Preparing { .. }
            )
        {
            self.prepared_here = false;
            if view.phase == TestsPhase::Ready && view.listed.is_none() {
                self.list_wanted = true;
            }
        }
        self.view = Some(view.clone());
    }
}

impl TestsModels {
    pub fn apply(&mut self, tell: &TestsTell) {
        self.0.entry(tell.pr().clone()).or_default().apply(tell);
    }

    /// Moves each waiting log tail into view, at most every `OUTPUT_EVERY_MS`. Says whether one
    /// still waits, so the window asks for another frame.
    pub fn flush(&mut self, now: f64) -> bool {
        let mut waiting = false;
        for model in self.0.values_mut() {
            let Some(live) = model.live.as_mut() else {
                continue;
            };
            if live.next_tail.is_none() {
                continue;
            }
            if now - live.tail_at >= OUTPUT_EVERY_MS as f64 / 1000.0 {
                live.tail = live.next_tail.take().unwrap_or_default();
                live.tail_at = now;
                model.version += 1;
            } else {
                waiting = true;
            }
        }
        waiting
    }

    pub fn pending_tail(&self) -> bool {
        self.0
            .values()
            .any(|m| m.live.as_ref().is_some_and(|l| l.next_tail.is_some()))
    }

    /// The answers still owed are lost with the connection: ask again once it is back.
    pub fn lost(&mut self) {
        for model in self.0.values_mut() {
            model.asked = false;
            model.pending = None;
            if let Some(modal) = model.plan.as_mut() {
                modal.sending = false;
            }
            model.touch();
        }
    }
}

impl TestMachine {
    pub fn engine_arrived(&mut self, info: Result<TestEngineInfo, String>) {
        self.checking = false;
        self.engine = Some(info);
    }

    /// A failed report keeps the last good one.
    pub fn disk_arrived(&mut self, report: Result<DiskReport, String>) {
        self.cleaning = false;
        if report.is_ok() || !matches!(self.disk, Some(Ok(_))) {
            self.disk = Some(report);
        }
    }

    pub fn lost(&mut self) {
        self.asked = false;
        self.checking = false;
        self.cleaning = false;
    }
}

/// The model when what it shows is about `head`: the tab strip never counts the failures of an
/// older head, even while the Tests section is not the one shown and nothing asked again.
pub fn of_head<'a>(model: Option<&'a TestsModel>, head: Option<&str>) -> Option<&'a TestsModel> {
    model.filter(|m| match (&m.view, head) {
        (Some(view), Some(head)) => view.head == head,
        _ => true,
    })
}

/// `Tests ●` / `Tests 3` on the section tabs.
pub fn badge_tests(sections: &mut [(ReviewSection, String, bool)], model: Option<&TestsModel>) {
    let Some(mark) = model.and_then(TestsModel::badge) else {
        return;
    };
    if let Some((section, label, _)) = sections
        .iter_mut()
        .find(|(s, ..)| *s == ReviewSection::Tests)
    {
        *label = format!("{} {mark}", section.label());
    }
}

/// `4096` bytes of decimal sizes: whole `B`, `KB` and `MB`, then `GB` with one decimal.
pub fn size_label(bytes: u64) -> String {
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let kb = (bytes + 500) / 1000;
    if kb < 1000 {
        return format!("{kb} KB");
    }
    let mb = (bytes + 500_000) / 1_000_000;
    if mb < 1000 {
        return format!("{mb} MB");
    }
    format!("{:.1} GB", bytes as f64 / 1e9)
}

fn minutes(ms: u64) -> String {
    let s = (ms + 500) / 1000;
    format!("{} min {} s", s / 60, s % 60)
}

/// The duration of one test: two decimals under 10 s, one under a minute.
pub fn row_duration(ms: u64) -> String {
    let hundredths = (ms + 5) / 10;
    if hundredths < 1000 {
        return format!("{}.{:02} s", hundredths / 100, hundredths % 100);
    }
    run_duration(ms)
}

/// The duration of a run: one decimal under a minute.
pub fn run_duration(ms: u64) -> String {
    let tenths = (ms + 50) / 100;
    if tenths < 600 {
        format!("{}.{} s", tenths / 10, tenths % 10)
    } else {
        minutes(ms)
    }
}

pub fn engine_label(engine: &Engine) -> String {
    match engine.kind {
        EngineKind::Colima => "Docker · Colima".into(),
        EngineKind::OrbStack => "Docker · OrbStack".into(),
        EngineKind::DockerDesktop => "Docker · Docker Desktop".into(),
        EngineKind::Podman => "Podman".into(),
        EngineKind::Other => format!("Docker · {}", printable(&engine.endpoint)),
    }
}

/// An endpoint without its `unix://`, with the home folder written as `~`.
pub fn short_endpoint(endpoint: &str, home: Option<&str>) -> String {
    let path = endpoint.strip_prefix("unix://").unwrap_or(endpoint);
    let home = home
        .map(|h| h.trim_end_matches('/'))
        .filter(|h| !h.is_empty());
    match home.and_then(|h| path.strip_prefix(h)) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

pub fn runner_label(runner: RunnerKind) -> &'static str {
    match runner {
        RunnerKind::Container => "container",
        RunnerKind::Local => "macOS sandbox",
    }
}

pub fn scope_label(scope: &TestScope) -> String {
    match scope {
        TestScope::All => "all tests",
        TestScope::NearChange => "near this change",
        TestScope::One { .. } => "one test",
    }
    .to_string()
}

/// `(path, line)` of a `path:line` or `path:line:col` token.
fn place_in(token: &str) -> Option<(&str, u32)> {
    let mut parts = token.trim_end_matches(':').split(':');
    let path = parts.next().filter(|p| !p.is_empty())?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let line = parts.next().filter(|l| digits(l))?;
    match (parts.next(), parts.next()) {
        (None, _) => {}
        (Some(col), None) if digits(col) => {}
        _ => return None,
    }
    Some((path, line.parse().ok()?))
}

/// Where a test failed: the first line of the test's own file named in the message, else the
/// first plain relative `path:line`, else the file alone.
pub fn failure_place(result: &TestCaseResult) -> Option<(String, Option<u32>)> {
    let message = result.message.as_deref().unwrap_or("");
    let places: Vec<(&str, u32)> = message
        .split(|c: char| !(c.is_ascii_alphanumeric() || "_./:-".contains(c)))
        .filter_map(place_in)
        .collect();
    let own = result
        .file
        .as_deref()
        .and_then(|file| places.iter().find(|(path, _)| *path == file));
    let plain = places.iter().find(|(path, _)| plain_relative(path, 256));
    match (own.or(plain), &result.file) {
        (Some((path, line)), _) => Some((path.to_string(), Some(*line))),
        (None, Some(file)) => Some((file.clone(), None)),
        (None, None) => None,
    }
}

/// The text a comment on a failing test starts with.
pub fn comment_prefill(name: &str, place: Option<(&str, Option<u32>)>, in_diff: bool) -> String {
    let name = printable(name);
    let name = if name.contains('`') {
        name
    } else {
        format!("`{name}`")
    };
    if in_diff {
        return format!("{name} fails here.");
    }
    match place {
        Some((file, Some(line))) => format!("{name} fails at {}:{line}.", printable(file)),
        Some((file, None)) => format!("{name} fails in {}.", printable(file)),
        None => format!("{name} fails."),
    }
}

/// Whether a filler of the Tests tab has anything to compare: one of the three resources it
/// reads moved, or a section body was just spawned and has nothing drawn yet.
pub(crate) fn tests_views_changed(
    tabs: Res<ReviewTabs>,
    models: Res<TestsModels>,
    model: Res<Model>,
    fresh: Query<(), Added<SectionBody>>,
) -> bool {
    tabs.is_changed() || models.is_changed() || model.is_changed() || !fresh.is_empty()
}

pub struct TestsPlugin;

impl Plugin for TestsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<TestsModels>()
            .init_resource::<TestMachine>()
            .add_message::<WindowFocused>()
            .add_systems(
                Update,
                (
                    load_tests.run_if(
                        resource_changed::<ReviewTabs>
                            .or_eager(resource_changed::<Model>)
                            .or_eager(resource_changed::<TestsModels>)
                            .or_eager(on_message::<WindowFocused>),
                    ),
                    flush_tails.run_if(|models: Res<TestsModels>| models.pending_tail()),
                    body::fill_tests.run_if(tests_views_changed),
                )
                    .chain()
                    .after(ReviewSystems),
            );
    }
}

/// Asks for the tests the first time a ready tab shows its Tests section, when the head moved,
/// after a connection came back, after a listing, and when the window comes back after a hand
/// edit of the recipe. A tab that closed forgets its model. Changes are written around change
/// detection and announced only when something was written, so a settled tab wakes nothing.
fn load_tests(
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut tests: ResMut<TestsModels>,
    mut asks: ResMut<Asks>,
    mut focus: MessageReader<WindowFocused>,
) {
    let focused = focus.read().any(|f| f.focused);
    let mut wrote = false;
    let models = tests.bypass_change_detection();
    // A tab that closed, or that opens again from scratch, has no tests to remember.
    let shown = |pr: &PrRef| tabs.0.get(pr).is_some_and(|t| t.ready().is_some());
    if models.0.keys().any(|pr| !shown(pr)) {
        models.0.retain(|pr, _| shown(pr));
        wrote = true;
    }
    if model.connection == Connection::Live {
        for (pr, tab) in &tabs.0 {
            let Some(ready) = tab.ready().filter(|r| r.cached_at.is_none()) else {
                continue;
            };
            if let Some(m) = models.0.get_mut(pr)
                && m.list_wanted
            {
                m.list_wanted = false;
                m.sent(TestsAskKind::List);
                asks.send(Ask::ListTests(pr.clone()));
                wrote = true;
            }
            if tab.ui.section != ReviewSection::Tests {
                continue;
            }
            let head = &ready.view.review.head_sha;
            let m = models.0.entry(pr.clone()).or_default();
            let reread = focused && m.reread_on_focus;
            if m.asked && m.asked_head.as_deref() == Some(head.as_str()) && !reread {
                continue;
            }
            m.asked = true;
            m.asked_head = Some(head.clone());
            if reread {
                m.reread_on_focus = false;
            }
            m.touch();
            asks.send(Ask::GetTests(pr.clone()));
            wrote = true;
        }
    }
    if wrote {
        tests.set_changed();
    }
}

fn flush_tails(
    time: Res<Time>,
    mut models: ResMut<TestsModels>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if models.flush(time.elapsed_secs_f64()) {
        redraw.write(RequestRedraw);
    }
}

#[cfg(test)]
mod tests {
    use bevy::window::WindowFocused;
    use clusia_core::tests::{
        Engine, EngineKind, NearTest, NearWhy, PlanStep, RECIPE_CHANGED, RESULTS_MAX, RunOutcome,
        RunnerKind, SetupPlan, StepPlace, StepState, TAIL_MAX, TestCaseResult, TestRun, TestScope,
        TestStatus, TestsPhase, TestsView, Totals,
    };

    use super::*;
    use crate::bridge::{Ask, Tell, TestsAskKind, TestsTell};
    use crate::fixture;
    use crate::testing::{self, NOW};

    fn pr() -> PrRef {
        fixture::demo_pr()
    }

    fn view(phase: TestsPhase) -> TestsView {
        let mut view = TestsView::new("a1b2c3d");
        view.phase = phase;
        view
    }

    fn step(id: &str, state: StepState) -> PlanStep {
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

    fn plan(hash: &str, steps: Vec<PlanStep>) -> SetupPlan {
        SetupPlan {
            recipe_hash: hash.into(),
            steps,
            commands: Vec::new(),
            commands_approved: false,
            ready: false,
            blocked: None,
        }
    }

    fn needs_setup(hash: &str) -> TestsView {
        let mut v = view(TestsPhase::NeedsSetup);
        v.plan = Some(plan(
            hash,
            vec![
                step("image", StepState::Missing),
                step("services", StepState::Done),
            ],
        ));
        v
    }

    fn result(name: &str, status: TestStatus) -> TestCaseResult {
        TestCaseResult {
            name: name.into(),
            file: None,
            status,
            duration_ms: Some(10),
            message: None,
        }
    }

    fn run(id: &str, head: &str, results: Vec<TestCaseResult>) -> TestRun {
        let count = |s: TestStatus| results.iter().filter(|r| r.status == s).count() as u32;
        TestRun {
            id: id.into(),
            head: head.into(),
            scope: TestScope::All,
            runner: RunnerKind::Container,
            command: "cargo test --workspace".into(),
            started_at: NOW,
            duration_ms: 3_200,
            exit_code: Some(0),
            outcome: RunOutcome::Passed,
            totals: Totals {
                passed: count(TestStatus::Passed),
                failed: count(TestStatus::Failed),
                skipped: count(TestStatus::Skipped),
            },
            results,
            log: None,
            tail: String::new(),
        }
    }

    fn loaded(view: TestsView) -> TestsTell {
        TestsTell::Loaded {
            pr: pr(),
            view: Box::new(view),
        }
    }

    fn model(view: TestsView) -> TestsModel {
        let mut m = TestsModel::default();
        m.apply(&loaded(view));
        m
    }

    fn live(id: &str) -> LiveRun {
        LiveRun {
            id: id.into(),
            scope: None,
            results: Vec::new(),
            tail: String::new(),
            tail_at: 0.0,
            next_tail: None,
        }
    }

    #[test]
    fn the_tab_state_follows_the_phase_the_plan_and_the_runs_in_order() {
        let head = "a1b2c3d";
        let passed = |id: &str, h: &str| run(id, h, vec![result("t", TestStatus::Passed)]);
        let blocked = |phase: TestsPhase, why: &str| {
            let mut v = view(phase);
            v.plan = Some(SetupPlan {
                blocked: Some(why.into()),
                ..plan("h", Vec::new())
            });
            v
        };
        let with_runs = |phase: TestsPhase, runs: Vec<TestRun>| {
            let mut v = view(phase);
            v.runs = runs;
            v
        };
        let cases: Vec<(TestsView, Option<LiveRun>, TabState)> = vec![
            (view(TestsPhase::NotGenerated), None, TabState::Empty),
            (
                view(TestsPhase::Generating {
                    activity: Some("Writing the recipe".into()),
                }),
                None,
                TabState::Generating {
                    activity: Some("Writing the recipe".into()),
                },
            ),
            (
                view(TestsPhase::Failed {
                    message: "The agent said no".into(),
                }),
                None,
                TabState::Failed {
                    message: "The agent said no".into(),
                },
            ),
            (
                blocked(
                    TestsPhase::Failed {
                        message: "This pull request ships its own .clusia/".into(),
                    },
                    "This pull request ships its own .clusia/",
                ),
                None,
                TabState::Refused {
                    reason: "This pull request ships its own .clusia/".into(),
                },
            ),
            (
                blocked(TestsPhase::NeedsSetup, "Services need a container engine"),
                None,
                TabState::Refused {
                    reason: "Services need a container engine".into(),
                },
            ),
            (
                blocked(TestsPhase::Ready, "The sandbox is broken"),
                None,
                TabState::Refused {
                    reason: "The sandbox is broken".into(),
                },
            ),
            (view(TestsPhase::NeedsSetup), None, TabState::NeedsSetup),
            (
                view(TestsPhase::Waiting),
                None,
                TabState::Preparing { step: None },
            ),
            (
                view(TestsPhase::Waiting),
                Some(live("r1")),
                TabState::Running,
            ),
            (
                view(TestsPhase::Preparing {
                    step: "image".into(),
                }),
                None,
                TabState::Preparing {
                    step: Some("image".into()),
                },
            ),
            (
                view(TestsPhase::Running { run: "list".into() }),
                None,
                TabState::Listing,
            ),
            (
                view(TestsPhase::Running { run: "r1".into() }),
                None,
                TabState::Running,
            ),
            (view(TestsPhase::Ready), None, TabState::Ready),
            (
                with_runs(TestsPhase::Ready, vec![passed("r1", head)]),
                None,
                TabState::Done,
            ),
            (
                with_runs(
                    TestsPhase::Ready,
                    vec![passed("r2", "9999999"), passed("r1", head)],
                ),
                None,
                TabState::Stale,
            ),
        ];
        for (view, live, want) in cases {
            assert_eq!(tab_state(&view, live.as_ref()), want, "{:?}", view.phase);
        }
    }

    #[test]
    fn a_proposal_is_not_a_state() {
        let mut v = view(TestsPhase::Ready);
        v.proposal = Some(clusia_core::tests::RecipeProposal {
            text: "[tests]\nall = \"make test\"\n".into(),
            hash: "p".into(),
            at: NOW,
        });
        assert_eq!(tab_state(&v, None), TabState::Ready);
    }

    #[test]
    fn the_badge_counts_failures_of_a_done_run_only() {
        let failing = vec![
            result("a", TestStatus::Failed),
            result("b", TestStatus::Failed),
            result("c", TestStatus::Failed),
            result("d", TestStatus::Passed),
        ];
        let mut done = view(TestsPhase::Ready);
        done.runs = vec![run("r1", "a1b2c3d", failing.clone())];
        assert_eq!(model(done).badge(), Some("3".to_string()));

        let mut stale = view(TestsPhase::Ready);
        stale.runs = vec![run("r1", "0000000", failing)];
        assert_eq!(
            model(stale).badge(),
            None,
            "an old head's failures are not this head's"
        );

        let mut clean = view(TestsPhase::Ready);
        clean.runs = vec![run("r1", "a1b2c3d", vec![result("a", TestStatus::Passed)])];
        assert_eq!(model(clean).badge(), None);

        for phase in [
            TestsPhase::Generating { activity: None },
            TestsPhase::Waiting,
            TestsPhase::Preparing {
                step: "image".into(),
            },
            TestsPhase::Running { run: "list".into() },
            TestsPhase::Running { run: "r1".into() },
        ] {
            assert_eq!(
                model(view(phase.clone())).badge(),
                Some("●".to_string()),
                "{phase:?}"
            );
        }
        assert_eq!(model(view(TestsPhase::Ready)).badge(), None);
        assert_eq!(TestsModel::default().badge(), None);
    }

    #[test]
    fn the_section_tab_label_carries_the_badge() {
        use crate::review_state::ReviewSection;
        let mut done = view(TestsPhase::Ready);
        done.runs = vec![run("r1", "a1b2c3d", vec![result("a", TestStatus::Failed)])];
        let m = model(done);
        let mut sections: Vec<(ReviewSection, String, bool)> = ReviewSection::ALL
            .iter()
            .map(|s| (*s, s.label().to_string(), false))
            .collect();
        badge_tests(&mut sections, Some(&m));
        let labels: Vec<&str> = sections.iter().map(|(_, l, _)| l.as_str()).collect();
        assert!(labels.contains(&"Tests 1"), "{labels:?}");
        assert!(labels.contains(&"Diff"), "no other tab changes: {labels:?}");
        let mut none: Vec<(ReviewSection, String, bool)> =
            vec![(ReviewSection::Tests, "Tests".into(), false)];
        badge_tests(&mut none, None);
        assert_eq!(none[0].1, "Tests");
    }

    #[test]
    fn a_tab_is_busy_during_every_activity() {
        for (phase, busy) in [
            (TestsPhase::Generating { activity: None }, true),
            (TestsPhase::Waiting, true),
            (
                TestsPhase::Preparing {
                    step: "image".into(),
                },
                true,
            ),
            (TestsPhase::Running { run: "list".into() }, true),
            (TestsPhase::Running { run: "r1".into() }, true),
            (TestsPhase::NotGenerated, false),
            (TestsPhase::NeedsSetup, false),
            (TestsPhase::Ready, false),
        ] {
            assert_eq!(model(view(phase.clone())).busy(), busy, "{phase:?}");
        }
    }

    #[test]
    fn rows_list_failed_then_passed_then_skipped_and_filter() {
        let mut v = view(TestsPhase::Ready);
        v.runs = vec![run(
            "r1",
            "a1b2c3d",
            vec![
                result("p1", TestStatus::Passed),
                result("s1", TestStatus::Skipped),
                result("f1", TestStatus::Failed),
                result("p2", TestStatus::Passed),
                result("f2", TestStatus::Failed),
            ],
        )];
        v.near = vec![
            NearTest {
                name: "p2".into(),
                why: NearWhy::File,
            },
            NearTest {
                name: "f2".into(),
                why: NearWhy::Module,
            },
        ];
        let m = model(v);
        let names =
            |filter| -> Vec<&str> { m.rows(filter).iter().map(|r| r.name.as_str()).collect() };
        assert_eq!(names(TestFilter::All), ["f1", "f2", "p1", "p2", "s1"]);
        assert_eq!(names(TestFilter::Failed), ["f1", "f2"]);
        assert_eq!(names(TestFilter::Near), ["f2", "p2"]);
        assert_eq!(m.listed_run().map(|r| r.id.as_str()), Some("r1"));
        assert!(TestsModel::default().rows(TestFilter::All).is_empty());
    }

    #[test]
    fn while_a_run_goes_the_rows_are_its_live_results() {
        let mut m = model(view(TestsPhase::Running { run: "r2".into() }));
        m.apply(&TestsTell::Result {
            pr: pr(),
            run: "r2".into(),
            result: result("live::one", TestStatus::Passed),
        });
        let names: Vec<&str> = m
            .rows(TestFilter::All)
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(names, ["live::one"]);
    }

    #[test]
    fn a_view_replaces_the_last_one_and_marks_the_model_loaded() {
        let mut m = TestsModel::default();
        assert!(!m.loaded && !m.asked);
        let before = m.version;
        m.apply(&loaded(needs_setup("h1")));
        assert!(m.loaded && m.asked);
        assert_eq!(
            m.view.as_ref().unwrap().plan.as_ref().unwrap().recipe_hash,
            "h1"
        );
        assert!(m.version > before);
        m.apply(&loaded(needs_setup("h2")));
        assert_eq!(
            m.view.as_ref().unwrap().plan.as_ref().unwrap().recipe_hash,
            "h2"
        );
        assert_eq!(m.asked_head.as_deref(), Some("a1b2c3d"));
    }

    #[test]
    fn a_running_view_starts_a_live_run_but_a_list_does_not() {
        let m = model(view(TestsPhase::Running { run: "r7".into() }));
        assert_eq!(m.live.as_ref().map(|l| l.id.as_str()), Some("r7"));
        let m = model(view(TestsPhase::Running { run: "list".into() }));
        assert!(m.live.is_none());
    }

    #[test]
    fn a_view_that_is_not_running_drops_the_live_run_and_the_setup_steps() {
        let mut m = model(view(TestsPhase::Running { run: "r7".into() }));
        m.apply(&TestsTell::Step {
            pr: pr(),
            step: "image".into(),
            state: StepState::Running,
            tail: Some("Step 2/5".into()),
        });
        assert!(m.steps.contains_key("image"));
        m.apply(&loaded(view(TestsPhase::Ready)));
        assert!(m.live.is_none() && m.steps.is_empty());
        let mut waiting = model(view(TestsPhase::Running { run: "r7".into() }));
        waiting.apply(&loaded(view(TestsPhase::Waiting)));
        assert!(
            waiting.live.is_some(),
            "a run that waits for its place is still live"
        );
    }

    #[test]
    fn a_step_keeps_its_state_and_tail() {
        let mut m = model(view(TestsPhase::Preparing {
            step: "image".into(),
        }));
        m.apply(&TestsTell::Step {
            pr: pr(),
            step: "image".into(),
            state: StepState::Running,
            tail: Some("Step 2/5".into()),
        });
        m.apply(&TestsTell::Step {
            pr: pr(),
            step: "image".into(),
            state: StepState::Done,
            tail: None,
        });
        assert_eq!(m.steps["image"], (StepState::Done, None));
    }

    #[test]
    fn a_result_for_an_unknown_run_creates_the_live_run_and_results_are_capped() {
        let mut m = model(view(TestsPhase::Ready));
        m.apply(&TestsTell::Result {
            pr: pr(),
            run: "r9".into(),
            result: result("t0", TestStatus::Passed),
        });
        assert_eq!(
            m.live.as_ref().map(|l| (l.id.as_str(), l.results.len())),
            Some(("r9", 1))
        );
        for n in 0..RESULTS_MAX + 5 {
            m.apply(&TestsTell::Result {
                pr: pr(),
                run: "r9".into(),
                result: result(&format!("t{n}"), TestStatus::Passed),
            });
        }
        assert_eq!(m.live.as_ref().unwrap().results.len(), RESULTS_MAX);
        m.apply(&TestsTell::Result {
            pr: pr(),
            run: "r10".into(),
            result: result("other", TestStatus::Failed),
        });
        let live = m.live.as_ref().unwrap();
        assert_eq!(
            (live.id.as_str(), live.results.len()),
            ("r10", 1),
            "a new run starts over"
        );
    }

    #[test]
    fn the_log_tail_is_cut_on_a_char_boundary_and_applied_at_most_every_250_ms() {
        let mut models = TestsModels::default();
        let output = |tail: &str| TestsTell::Output {
            pr: pr(),
            run: "r1".into(),
            tail: tail.into(),
        };
        models.apply(&output("a"));
        assert!(!models.flush(10.0) && models.0[&pr()].live.as_ref().unwrap().tail == "a");
        models.apply(&output("b"));
        assert!(models.flush(10.1), "too soon: it waits");
        assert_eq!(models.0[&pr()].live.as_ref().unwrap().tail, "a");
        assert!(models.pending_tail());
        assert!(!models.flush(10.26), "due: applied, nothing waits");
        assert_eq!(models.0[&pr()].live.as_ref().unwrap().tail, "b");
        assert!(!models.pending_tail());

        // Cut at 4096 bytes keeping the end, on a char boundary.
        let long = format!("xx{}", "é".repeat(2048));
        assert_eq!(long.len(), 4098);
        models.apply(&output(&long));
        models.flush(20.0);
        let kept = &models.0[&pr()].live.as_ref().unwrap().tail;
        assert_eq!(kept, &"é".repeat(2048));
        assert!(kept.len() <= TAIL_MAX);
        let odd = format!("x{}y", "é".repeat(2048));
        models.apply(&output(&odd));
        models.flush(30.0);
        assert_eq!(
            models.0[&pr()].live.as_ref().unwrap().tail,
            format!("{}y", "é".repeat(2047))
        );
    }

    #[test]
    fn a_run_that_ends_goes_first_drops_the_live_run_and_selects_its_first_failure() {
        let mut v = view(TestsPhase::Ready);
        v.runs = vec![run(
            "r1",
            "a1b2c3d",
            vec![result("old", TestStatus::Passed)],
        )];
        let mut m = model(v);
        m.live = Some(live("r2"));
        let done = run(
            "r2",
            "a1b2c3d",
            vec![
                result("ok", TestStatus::Passed),
                result("auth::refresh", TestStatus::Failed),
            ],
        );
        m.apply(&TestsTell::RunDone {
            pr: pr(),
            run: Box::new(done.clone()),
        });
        assert!(m.live.is_none());
        let ids: Vec<&str> = m
            .view
            .as_ref()
            .unwrap()
            .runs
            .iter()
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(ids, ["r2", "r1"]);
        assert_eq!(m.selected.as_deref(), Some("auth::refresh"));
        // The view that follows the run may already hold it: it is never listed twice.
        m.apply(&TestsTell::RunDone {
            pr: pr(),
            run: Box::new(done),
        });
        assert_eq!(m.view.as_ref().unwrap().runs.len(), 2);
        // A run with no failure selects nothing.
        m.apply(&TestsTell::RunDone {
            pr: pr(),
            run: Box::new(run("r3", "a1b2c3d", vec![result("ok", TestStatus::Passed)])),
        });
        assert_eq!(m.selected, None);
    }

    #[test]
    fn a_listing_and_a_changed_recipe_ask_the_view_again() {
        let mut m = model(view(TestsPhase::Ready));
        m.sent(TestsAskKind::List);
        assert_eq!(m.pending, Some(TestsAskKind::List));
        m.apply(&TestsTell::Listed {
            pr: pr(),
            count: 412,
        });
        assert!(!m.asked && m.pending.is_none());
        let mut m = model(needs_setup("h1"));
        m.sent(TestsAskKind::Prepare);
        m.apply(&TestsTell::Refused {
            pr: pr(),
            ask: TestsAskKind::Prepare,
            message: "busy".into(),
        });
        assert!(
            m.asked && m.pending.is_none(),
            "a busy refusal reads nothing again"
        );
        assert!(!m.prepared_here, "a refused setup is not one started here");
        m.apply(&loaded(view(TestsPhase::Ready)));
        assert!(!m.list_wanted, "a setup done elsewhere never lists");
        let mut m = model(needs_setup("h1"));
        m.sent(TestsAskKind::Prepare);
        m.apply(&TestsTell::Refused {
            pr: pr(),
            ask: TestsAskKind::Prepare,
            message: RECIPE_CHANGED.into(),
        });
        assert!(!m.asked && m.pending.is_none(), "a changed recipe does");
        let mut m = model(needs_setup("h1"));
        m.sent(TestsAskKind::Stop);
        m.apply(&TestsTell::Refused {
            pr: pr(),
            ask: TestsAskKind::Stop,
            message: "Not connected to clusiad — not sent".into(),
        });
        assert!(
            m.asked && m.pending.is_none(),
            "a refused stop frees the button and asks nothing"
        );
    }

    #[test]
    fn a_started_run_takes_its_scope_and_clears_the_selection() {
        let mut m = model(view(TestsPhase::Ready));
        m.selected = Some("old".into());
        m.sent_run(TestScope::NearChange);
        assert_eq!(m.pending, Some(TestsAskKind::Run));
        m.apply(&TestsTell::RunStarted {
            pr: pr(),
            run: "r5".into(),
        });
        let live = m.live.as_ref().unwrap();
        assert_eq!(
            (live.id.as_str(), live.scope.clone()),
            ("r5", Some(TestScope::NearChange))
        );
        assert!(m.selected.is_none() && m.pending.is_none());
    }

    #[test]
    fn a_refused_hash_reads_the_plan_again_and_keeps_the_modal() {
        let mut m = model(needs_setup("h1"));
        m.open_plan();
        let modal = m.plan.as_ref().expect("the modal is open");
        assert_eq!(modal.recipe_hash, "h1");
        assert_eq!(modal.shown, ["image", "services"]);
        assert_eq!(
            modal.checked.iter().map(String::as_str).collect::<Vec<_>>(),
            ["image"]
        );
        m.sent(TestsAskKind::Prepare);
        assert!(m.plan.as_ref().unwrap().sending);
        m.apply(&TestsTell::Refused {
            pr: pr(),
            ask: TestsAskKind::Prepare,
            message: RECIPE_CHANGED.into(),
        });
        assert!(!m.asked, "the model asks GetTests again");
        let modal = m.plan.as_ref().expect("the modal stays open");
        assert!(!modal.sending && modal.changed_note);
        assert_eq!(
            modal.recipe_hash, "h1",
            "nothing is approvable against the new hash yet"
        );

        // The new plan replaces the shown hash and ids; what was checked stays checked.
        let mut next = needs_setup("h2");
        next.plan
            .as_mut()
            .unwrap()
            .steps
            .push(step("stack", StepState::Missing));
        m.apply(&loaded(next));
        let modal = m.plan.as_ref().unwrap();
        assert_eq!(modal.recipe_hash, "h2");
        assert_eq!(modal.shown, ["image", "services", "stack"]);
        assert_eq!(
            modal.checked.iter().map(String::as_str).collect::<Vec<_>>(),
            ["image"],
            "the step the reviewer never saw is not checked for them"
        );
        assert!(modal.changed_note);
    }

    #[test]
    fn the_modal_opens_on_its_own_once_per_recipe_hash() {
        let mut m = model(view(TestsPhase::Generating { activity: None }));
        m.apply(&loaded(needs_setup("h1")));
        assert!(m.plan_wanted);
        assert!(m.auto_opened.contains("h1"));
        m.open_plan();
        assert!(!m.plan_wanted);
        // The same plan again, after another generation: never on its own twice.
        m.plan = None;
        m.apply(&loaded(view(TestsPhase::Generating { activity: None })));
        m.apply(&loaded(needs_setup("h1")));
        assert!(
            !m.plan_wanted,
            "a hash already shown does not open the modal again"
        );
        m.apply(&loaded(view(TestsPhase::Generating { activity: None })));
        m.apply(&loaded(needs_setup("h2")));
        assert!(m.plan_wanted, "a new recipe does");
    }

    #[test]
    fn the_modal_does_not_open_by_itself_for_a_blocked_plan_or_a_view_it_never_saw_generate() {
        let mut m = model(view(TestsPhase::Generating { activity: None }));
        let mut blocked = needs_setup("h1");
        blocked.plan.as_mut().unwrap().blocked = Some("Services need an engine".into());
        m.apply(&loaded(blocked));
        assert!(!m.plan_wanted);
        let mut fresh = TestsModel::default();
        fresh.apply(&loaded(needs_setup("h1")));
        assert!(!fresh.plan_wanted, "the window did not see Generating end");
    }

    #[test]
    fn only_a_setup_started_here_lists_once_it_is_ready() {
        let mut m = model(needs_setup("h1"));
        m.sent(TestsAskKind::Prepare);
        assert!(m.prepared_here);
        m.apply(&loaded(view(TestsPhase::Preparing {
            step: "image".into(),
        })));
        assert!(!m.list_wanted);
        m.apply(&loaded(view(TestsPhase::Ready)));
        assert!(m.list_wanted && !m.prepared_here);

        let mut elsewhere = model(view(TestsPhase::Preparing {
            step: "image".into(),
        }));
        elsewhere.apply(&loaded(view(TestsPhase::Ready)));
        assert!(
            !elsewhere.list_wanted,
            "a setup the CLI started never lists"
        );

        let mut listed = model(needs_setup("h1"));
        listed.sent(TestsAskKind::Prepare);
        let mut ready = view(TestsPhase::Ready);
        ready.listed = Some(12);
        listed.apply(&loaded(ready));
        assert!(!listed.list_wanted, "already listed");

        let mut failed = model(needs_setup("h1"));
        failed.sent(TestsAskKind::Prepare);
        failed.apply(&loaded(view(TestsPhase::NeedsSetup)));
        assert!(!failed.prepared_here, "a setup that did not finish is over");
    }

    #[test]
    fn the_models_and_the_machine_forget_what_a_lost_connection_owed() {
        let mut models = TestsModels::default();
        models.apply(&loaded(view(TestsPhase::Ready)));
        models.0.get_mut(&pr()).unwrap().pending = Some(TestsAskKind::Run);
        models.lost();
        let m = &models.0[&pr()];
        assert!(!m.asked && m.pending.is_none());
        let mut machine = TestMachine {
            asked: true,
            checking: true,
            cleaning: true,
            ..TestMachine::default()
        };
        machine.lost();
        assert!(!machine.asked && !machine.checking && !machine.cleaning);
    }

    #[test]
    fn the_machine_keeps_its_good_disk_report_when_a_later_one_fails() {
        let mut machine = TestMachine {
            cleaning: true,
            ..TestMachine::default()
        };
        machine.disk_arrived(Ok(fixture::demo_test_disk()));
        assert!(!machine.cleaning);
        machine.disk_arrived(Err("A review is busy".into()));
        assert_eq!(machine.disk, Some(Ok(fixture::demo_test_disk())));
        let mut fresh = TestMachine::default();
        fresh.disk_arrived(Err("Not connected to clusiad".into()));
        assert_eq!(fresh.disk, Some(Err("Not connected to clusiad".into())));
        let mut checking = TestMachine {
            checking: true,
            ..TestMachine::default()
        };
        checking.engine_arrived(Ok(fixture::demo_test_engine()));
        assert!(!checking.checking && matches!(checking.engine, Some(Ok(_))));
    }

    #[test]
    fn sizes_are_decimal_with_whole_units_then_one_decimal_gigabytes() {
        for (bytes, want) in [
            (0, "0 B"),
            (999, "999 B"),
            (1_000, "1 KB"),
            (1_500, "2 KB"),
            (999_499, "999 KB"),
            (999_500, "1 MB"),
            (800_000_000, "800 MB"),
            (999_400_000, "999 MB"),
            (999_500_000, "1.0 GB"),
            (1_400_000_000, "1.4 GB"),
            (4_100_000_000, "4.1 GB"),
            (7_200_000_000, "7.2 GB"),
        ] {
            assert_eq!(size_label(bytes), want, "{bytes}");
        }
    }

    #[test]
    fn durations_read_as_the_mockup_does() {
        for (ms, row, whole) in [
            (210, "0.21 s", "0.2 s"),
            (9_800, "9.80 s", "9.8 s"),
            (48_200, "48.2 s", "48.2 s"),
            (72_000, "1 min 12 s", "1 min 12 s"),
            (200_000, "3 min 20 s", "3 min 20 s"),
            (0, "0.00 s", "0.0 s"),
        ] {
            assert_eq!(row_duration(ms), row, "row {ms}");
            assert_eq!(run_duration(ms), whole, "run {ms}");
        }
    }

    fn engine(kind: EngineKind, endpoint: &str) -> Engine {
        Engine {
            kind,
            cli: "/usr/local/bin/docker".into(),
            endpoint: endpoint.into(),
            context: None,
            running: true,
            start: None,
            compose: None,
        }
    }

    #[test]
    fn labels_name_the_engine_the_runner_and_the_scope() {
        assert_eq!(
            engine_label(&engine(EngineKind::Colima, "")),
            "Docker · Colima"
        );
        assert_eq!(
            engine_label(&engine(EngineKind::OrbStack, "")),
            "Docker · OrbStack"
        );
        assert_eq!(
            engine_label(&engine(EngineKind::DockerDesktop, "")),
            "Docker · Docker Desktop"
        );
        assert_eq!(engine_label(&engine(EngineKind::Podman, "")), "Podman");
        assert_eq!(
            engine_label(&engine(EngineKind::Other, "tcp://10.0.0.5:2375")),
            "Docker · tcp://10.0.0.5:2375"
        );
        assert_eq!(runner_label(RunnerKind::Container), "container");
        assert_eq!(runner_label(RunnerKind::Local), "macOS sandbox");
        assert_eq!(scope_label(&TestScope::All), "all tests");
        assert_eq!(scope_label(&TestScope::NearChange), "near this change");
        assert_eq!(
            scope_label(&TestScope::One {
                name: "a::b".into()
            }),
            "one test"
        );
    }

    #[test]
    fn the_endpoint_drops_its_scheme_and_writes_the_home_as_a_tilde() {
        let sock = "unix:///Users/octo/.colima/default/docker.sock";
        assert_eq!(
            short_endpoint(sock, Some("/Users/octo")),
            "~/.colima/default/docker.sock"
        );
        assert_eq!(
            short_endpoint(sock, Some("/Users/octo/")),
            "~/.colima/default/docker.sock"
        );
        assert_eq!(
            short_endpoint(sock, Some("/Users/oct")),
            "/Users/octo/.colima/default/docker.sock"
        );
        assert_eq!(
            short_endpoint(sock, None),
            "/Users/octo/.colima/default/docker.sock"
        );
        assert_eq!(
            short_endpoint(sock, Some("/")),
            "/Users/octo/.colima/default/docker.sock"
        );
        assert_eq!(
            short_endpoint("tcp://10.0.0.5:2375", Some("/Users/octo")),
            "tcp://10.0.0.5:2375"
        );
    }

    fn failing(name: &str, file: Option<&str>, message: Option<&str>) -> TestCaseResult {
        TestCaseResult {
            name: name.into(),
            file: file.map(String::from),
            status: TestStatus::Failed,
            duration_ms: None,
            message: message.map(String::from),
        }
    }

    #[test]
    fn the_failure_place_is_the_line_of_the_tests_file_else_a_plain_path_else_the_file() {
        let place = failure_place;
        assert_eq!(
            place(&failing(
                "auth::refresh",
                Some("src/auth/refresh.rs"),
                Some("assertion failed at src/auth/refresh.rs:44:9\n left: 1"),
            )),
            Some(("src/auth/refresh.rs".into(), Some(44)))
        );
        assert_eq!(
            place(&failing(
                "a",
                Some("tests/a.rs"),
                Some("thread 'a' panicked at src/other.rs:7:1:\nboom"),
            )),
            Some(("src/other.rs".into(), Some(7))),
            "another plain path in the message"
        );
        assert_eq!(
            place(&failing(
                "a",
                Some("tests/a.rs"),
                Some("panicked at /etc/passwd:3:1")
            )),
            Some(("tests/a.rs".into(), None)),
            "an absolute path is never a place"
        );
        assert_eq!(
            place(&failing(
                "a",
                Some("tests/a.rs"),
                Some("panicked at ../../secret.rs:3")
            )),
            Some(("tests/a.rs".into(), None)),
            "a path that leaves the worktree is never a place"
        );
        assert_eq!(
            place(&failing("a", Some("tests/a.rs"), None)),
            Some(("tests/a.rs".into(), None))
        );
        assert_eq!(place(&failing("a", None, Some("no place here"))), None);
        assert_eq!(
            place(&failing("a", None, Some("see (src/x.rs:12)"))),
            Some(("src/x.rs".into(), Some(12)))
        );
    }

    #[test]
    fn the_comment_prefill_names_the_test_and_where_it_fails() {
        assert_eq!(
            comment_prefill("auth::refresh", Some(("src/a.rs", Some(44))), true),
            "`auth::refresh` fails here."
        );
        assert_eq!(
            comment_prefill("auth::refresh", Some(("src/a.rs", Some(44))), false),
            "`auth::refresh` fails at src/a.rs:44."
        );
        assert_eq!(
            comment_prefill("auth::refresh", Some(("src/a.rs", None)), false),
            "`auth::refresh` fails in src/a.rs."
        );
        assert_eq!(
            comment_prefill("auth::refresh", None, false),
            "`auth::refresh` fails."
        );
        assert_eq!(
            comment_prefill("odd`name", Some(("src/a.rs", Some(1))), true),
            "odd`name fails here.",
            "a name with a backtick is written without the code span"
        );
        assert_eq!(
            comment_prefill("a\tb\u{1b}[31m", None, false),
            "`a\\tb\\u{1b}[31m` fails.",
            "the name is made printable first"
        );
    }

    fn tests_app() -> (App, PrRef) {
        (testing::demo_review_app(), fixture::demo_pr())
    }

    fn tab_head(app: &App, pr: &PrRef) -> String {
        testing::ready(app, pr).view.review.head_sha.clone()
    }

    fn gets(asks: &[Ask], pr: &PrRef) -> usize {
        asks.iter()
            .filter(|a| **a == Ask::GetTests(pr.clone()))
            .count()
    }

    #[test]
    fn the_tests_are_asked_the_first_time_the_section_is_shown_and_not_before() {
        let (mut app, pr) = tests_app();
        testing::set_section(&mut app, &pr, ReviewSection::Diff);
        assert_eq!(
            gets(&testing::recorded(&mut app), &pr),
            0,
            "opening a tab costs the daemon nothing"
        );
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        assert_eq!(gets(&testing::recorded(&mut app), &pr), 1);
        testing::set_section(&mut app, &pr, ReviewSection::Diff);
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        assert_eq!(gets(&testing::recorded(&mut app), &pr), 0, "only once");
    }

    #[test]
    fn a_new_head_asks_for_the_tests_again() {
        let (mut app, pr) = tests_app();
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        let head = tab_head(&app, &pr);
        testing::open_tests(&mut app, &pr, {
            let mut v = TestsView::new(head.clone());
            v.phase = TestsPhase::Ready;
            v
        });
        assert_eq!(gets(&testing::recorded(&mut app), &pr), 1, "the first ask");
        assert_eq!(gets(&testing::recorded(&mut app), &pr), 0);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ready_mut()
            .unwrap()
            .view
            .review
            .head_sha = "feedface".into();
        testing::settle(&mut app);
        assert_eq!(gets(&testing::recorded(&mut app), &pr), 1, "the head moved");
        testing::settle(&mut app);
        assert_eq!(
            gets(&testing::recorded(&mut app), &pr),
            0,
            "asked once for that head"
        );
    }

    #[test]
    fn a_changed_recipe_a_listing_and_a_reconnect_ask_again() {
        let (mut app, pr) = tests_app();
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        testing::recorded(&mut app);
        for tell in [
            Tell::Tests(TestsTell::Refused {
                pr: pr.clone(),
                ask: TestsAskKind::Prepare,
                message: RECIPE_CHANGED.into(),
            }),
            Tell::Tests(TestsTell::Listed {
                pr: pr.clone(),
                count: 3,
            }),
        ] {
            testing::tell(&mut app, tell);
            testing::settle(&mut app);
            assert_eq!(gets(&testing::recorded(&mut app), &pr), 1);
        }
        testing::tell(&mut app, Tell::Lost("gone".into()));
        testing::settle(&mut app);
        assert_eq!(
            gets(&testing::recorded(&mut app), &pr),
            0,
            "not while disconnected"
        );
        testing::tell(&mut app, Tell::Snapshot(Box::new(fixture::demo(NOW))));
        testing::settle(&mut app);
        assert_eq!(
            gets(&testing::recorded(&mut app), &pr),
            1,
            "after the reconnect"
        );
    }

    #[test]
    fn a_setup_started_here_lists_once_and_one_started_elsewhere_does_not() {
        let (mut app, pr) = tests_app();
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        let head = tab_head(&app, &pr);
        let with_head = |phase: TestsPhase| {
            let mut v = TestsView::new(head.clone());
            v.phase = phase;
            v
        };
        testing::open_tests(&mut app, &pr, with_head(TestsPhase::NeedsSetup));
        testing::recorded(&mut app);
        app.world_mut()
            .resource_mut::<TestsModels>()
            .0
            .get_mut(&pr)
            .unwrap()
            .sent(TestsAskKind::Prepare);
        testing::open_tests(
            &mut app,
            &pr,
            with_head(TestsPhase::Preparing {
                step: "image".into(),
            }),
        );
        testing::open_tests(&mut app, &pr, with_head(TestsPhase::Ready));
        let asks = testing::recorded(&mut app);
        assert_eq!(
            asks.iter()
                .filter(|a| **a == Ask::ListTests(pr.clone()))
                .count(),
            1
        );
        testing::open_tests(&mut app, &pr, with_head(TestsPhase::NeedsSetup));
        testing::open_tests(&mut app, &pr, with_head(TestsPhase::Ready));
        let asks = testing::recorded(&mut app);
        assert!(
            !asks.contains(&Ask::ListTests(pr.clone())),
            "nothing was started here"
        );
    }

    #[test]
    fn a_hand_edit_is_read_again_when_the_window_gains_focus() {
        let (mut app, pr) = tests_app();
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        let head = tab_head(&app, &pr);
        testing::open_tests(&mut app, &pr, TestsView::new(head));
        testing::recorded(&mut app);
        app.world_mut()
            .resource_mut::<TestsModels>()
            .0
            .get_mut(&pr)
            .unwrap()
            .reread_on_focus = true;
        testing::settle(&mut app);
        assert_eq!(
            gets(&testing::recorded(&mut app), &pr),
            0,
            "not before the window is back"
        );
        app.world_mut().write_message(WindowFocused {
            window: Entity::PLACEHOLDER,
            focused: true,
        });
        app.update();
        assert_eq!(gets(&testing::recorded(&mut app), &pr), 1);
        assert!(!app.world().resource::<TestsModels>().0[&pr].reread_on_focus);
        app.world_mut().write_message(WindowFocused {
            window: Entity::PLACEHOLDER,
            focused: true,
        });
        app.update();
        assert_eq!(gets(&testing::recorded(&mut app), &pr), 0);
    }

    #[test]
    fn a_closed_tab_forgets_its_tests() {
        let (mut app, pr) = tests_app();
        testing::open_tests(&mut app, &pr, TestsView::new("abc"));
        assert!(app.world().resource::<TestsModels>().0.contains_key(&pr));
        app.world_mut().resource_mut::<ReviewTabs>().0.remove(&pr);
        testing::settle(&mut app);
        assert!(!app.world().resource::<TestsModels>().0.contains_key(&pr));
    }

    #[test]
    fn the_tests_tab_label_shows_its_failures_in_the_tab_strip() {
        let (mut app, pr) = tests_app();
        let head = tab_head(&app, &pr);
        let mut v = TestsView::new(head.clone());
        v.phase = TestsPhase::Ready;
        v.runs = vec![run(
            "r1",
            &head,
            (0..11)
                .map(|n| result(&format!("t{n}"), TestStatus::Failed))
                .collect(),
        )];
        testing::open_tests(&mut app, &pr, v);
        assert!(testing::shown_text(&mut app).iter().any(|t| t == "11"));
    }

    #[test]
    fn the_tab_label_never_counts_the_failures_of_an_older_head() {
        let (mut app, pr) = tests_app();
        testing::set_section(&mut app, &pr, ReviewSection::Diff);
        let head = tab_head(&app, &pr);
        let mut v = TestsView::new(head.clone());
        v.phase = TestsPhase::Ready;
        v.runs = vec![run(
            "r1",
            &head,
            vec![
                result("t0", TestStatus::Failed),
                result("t1", TestStatus::Failed),
            ],
        )];
        testing::open_tests(&mut app, &pr, v);
        assert!(testing::shown_text(&mut app).iter().any(|t| t == "2"));
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ready_mut()
            .unwrap()
            .view
            .review
            .head_sha = "feedface".into();
        testing::settle(&mut app);
        assert!(
            !testing::shown_text(&mut app).iter().any(|t| t == "2"),
            "the section is still Diff and the model still holds the old head"
        );
    }

    #[test]
    fn output_keeps_flowing_to_a_wake_while_a_tail_waits() {
        let (mut app, pr) = tests_app();
        for n in 0..3 {
            testing::tell(
                &mut app,
                Tell::Tests(TestsTell::Output {
                    pr: pr.clone(),
                    run: "r1".into(),
                    tail: format!("line {n}"),
                }),
            );
        }
        for _ in 0..30 {
            testing::settle(&mut app);
        }
        let models = app.world().resource::<TestsModels>();
        assert_eq!(
            models.0[&pr].live.as_ref().unwrap().tail,
            "line 2",
            "the newest wins"
        );
        assert!(!models.pending_tail());
    }
}
