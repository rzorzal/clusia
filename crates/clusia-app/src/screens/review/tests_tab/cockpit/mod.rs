//! The QA cockpit's model: the view the daemon sent, the selection, the reviewer's edits on their
//! way to the daemon, the run that waits for them, and the live samples of the round that goes
//! now. Everything the daemon sent is text here: nothing in this module draws it, parses it as a
//! path or opens it.

pub mod center;
pub mod chart;
pub mod compare;
pub mod copilot;
pub mod frame;
pub mod list;
pub mod modals;
pub mod views;

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use bevy::prelude::*;
use clusia_core::PrRef;
use clusia_core::cockpit::{
    BenchView, COCKPIT_CHANGED, CockpitActivity, CockpitScope, CockpitView, ItemKind, ItemRef,
    ItemSupport, MAIN_FIRST, MainPhase, NEW_FUNCTION, NO_SCENARIOS, RunSide, SERIES_MAX, Sample,
    ScenarioApproval, ScenarioScript, ScenarioView,
};
use clusia_core::functions::FunctionChange;
use clusia_core::printable::printable;

use super::functions::{self, FunctionsModels};
use super::{TestsModels, body};
use crate::bridge::{
    Ask, Asks, CockpitAskKind, CockpitTell, DEMO_NOT_SENT, NOT_SENT, TOAST_SECS, Toast, Toasts,
};
use crate::screens::review::ReviewSystems;
use crate::ui::kit::FieldCommitted;

pub use views::*;

/// One field of an item: an input or a parameter by name, or one of the loop's settings.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Field {
    /// An input's or a parameter's name.
    Value(String),
    Rounds,
    Warmup,
    Secs,
}

/// A field of one item: where an error shows and which save an answer belongs to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FieldKey {
    pub item: ItemRef,
    pub field: Field,
}

/// A save in flight, or waiting behind one.
#[derive(Debug, Clone, PartialEq)]
pub struct Saving {
    pub field: FieldKey,
    pub ask: Ask,
}

/// A run waiting for the saves in front of it.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedRun {
    pub scope: CockpitScope,
    pub on_main: bool,
}

/// What the queue lets go of next. It is built once per release, so the size of `Ask` is no
/// cost worth a box that every caller would have to open.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Released {
    Nothing,
    Send(Ask),
    Dropped,
}

/// The modal the cockpit has open.
#[derive(Debug, Clone, PartialEq)]
pub enum CockpitModal {
    Approve { scenario: String },
    PrepareMain { checked: BTreeSet<String> },
    Remove { scenario: String },
}

/// The samples of the round that goes now, by (side, round). The next view that has a finished
/// round's result takes its points out.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveSamples {
    pub run: String,
    pub points: BTreeMap<(RunSide, u32), Vec<Sample>>,
    pub done: BTreeSet<(RunSide, u32)>,
}

#[derive(Default)]
pub struct CockpitModel {
    pub view: Option<CockpitView>,
    /// Asked and not lost with a connection or a `COCKPIT_CHANGED` refusal.
    pub asked: bool,
    pub loaded: bool,
    pub asked_head: Option<String>,
    pub selected: Option<ItemRef>,
    pub field_errors: BTreeMap<FieldKey, String>,
    /// The save in flight.
    pub saving: Option<Saving>,
    /// The one commit waiting behind it; the last one wins.
    pub queued_save: Option<Saving>,
    /// A run waiting for the saves.
    pub queued_run: Option<QueuedRun>,
    /// The asks whose answer is a view, in send order: the daemon answers in that order.
    pub awaiting: VecDeque<CockpitAskKind>,
    pub pending: Option<CockpitAskKind>,
    pub script: Option<ScenarioScript>,
    /// The (scenario, approval hash) the script was asked for.
    pub script_asked: Option<(String, String)>,
    pub script_error: Option<String>,
    pub script_tab: usize,
    pub on_main: bool,
    pub live: Option<LiveSamples>,
    /// The run this window started, until the activity ends.
    pub live_run: Option<String>,
    /// A stop went out and the run has not ended yet.
    pub stopping: bool,
    pub modal: Option<CockpitModal>,
    /// The scenario **Approve and run** runs once its approval lands.
    pub run_after_approve: Option<ItemRef>,
    pub copilot_focus: bool,
    /// Bumped when a copilot ask was taken, so its input can clear itself.
    pub copilot_sent: u64,
    /// The scenario whose **Suggest as a test in a comment** waits for its script. Selecting
    /// another item drops it.
    pub suggest_for: Option<String>,
    /// Why the copilot's input did not send what it holds; cleared by the next send.
    pub copilot_error: Option<String>,
    /// The `failed` text the reviewer dismissed.
    pub dismissed_failed: Option<String>,
    /// Mirrored: the Auto Tests or the function cases are busy.
    pub tests_busy: bool,
    /// Bumped by every change but samples.
    pub version: u64,
    /// Bumped by every change, samples included.
    pub chart_version: u64,
}

impl CockpitModel {
    pub fn touch(&mut self) {
        self.version += 1;
        self.chart_version += 1;
    }

    pub fn selected_scenario(&self) -> Option<&ScenarioView> {
        let item = self
            .selected
            .as_ref()
            .filter(|i| i.kind == ItemKind::Scenario)?;
        self.view
            .as_ref()?
            .scenarios
            .iter()
            .find(|s| s.slug == item.id)
    }

    pub fn selected_bench(&self) -> Option<&BenchView> {
        let item = self
            .selected
            .as_ref()
            .filter(|i| i.kind == ItemKind::Bench)?;
        self.view
            .as_ref()?
            .benches
            .iter()
            .find(|b| b.slug == item.id)
    }

    /// The item the activity is running.
    pub fn running_item(&self) -> Option<&ItemRef> {
        match self.view.as_ref()?.activity.as_ref()? {
            CockpitActivity::Running { item, .. } => Some(item),
            _ => None,
        }
    }

    /// Whether the daemon is doing something this window must wait for.
    pub fn busy(&self) -> bool {
        self.view.as_ref().is_some_and(|v| v.activity.is_some())
            || self.live_run.is_some()
            || self.stopping
            || matches!(
                self.pending,
                Some(CockpitAskKind::Run | CockpitAskKind::Write)
            )
    }

    fn has_item(&self, item: &ItemRef) -> bool {
        self.view.as_ref().is_some_and(|v| match item.kind {
            ItemKind::Scenario => v.scenarios.iter().any(|s| s.slug == item.id),
            ItemKind::Bench => v.benches.iter().any(|b| b.slug == item.id),
            ItemKind::Load => false,
        })
    }

    /// Selects `item` when the view has it.
    pub fn select(&mut self, item: ItemRef) {
        if self.selected.as_ref() != Some(&item) && self.has_item(&item) {
            self.selected = Some(item);
            self.suggest_for = None;
            self.touch();
        }
    }

    /// Notes that an ask of `kind` goes out. Every ask a region sends goes with this, so the
    /// answers keep their order.
    pub fn sent(&mut self, kind: CockpitAskKind) {
        if matches!(
            kind,
            CockpitAskKind::Load
                | CockpitAskKind::Save
                | CockpitAskKind::Reset
                | CockpitAskKind::Remove
                | CockpitAskKind::Approve
        ) {
            self.awaiting.push_back(kind);
        }
        if !matches!(kind, CockpitAskKind::Load | CockpitAskKind::Script) {
            self.pending = Some(kind);
        }
        self.touch();
    }

    /// The only way to build `GetCockpit`.
    pub fn ask_load(&mut self, pr: &PrRef, head: &str) -> Ask {
        self.asked = true;
        self.asked_head = Some(head.to_string());
        self.sent(CockpitAskKind::Load);
        Ask::GetCockpit(pr.clone())
    }

    /// `GetScenarioScript` when the selected scenario's script is not the one held and was not
    /// asked for already.
    pub fn want_script(&mut self, pr: &PrRef) -> Option<Ask> {
        let scenario = self.selected_scenario()?;
        let key = (scenario.slug.clone(), scenario.approval_hash.clone());
        let held = self
            .script
            .as_ref()
            .is_some_and(|s| s.scenario == key.0 && s.approval_hash == key.1);
        if held || self.script_asked.as_ref() == Some(&key) {
            return None;
        }
        self.script_asked = Some(key.clone());
        self.script_error = None;
        self.sent(CockpitAskKind::Script);
        Some(Ask::GetScenarioScript {
            pr: pr.clone(),
            scenario: key.0,
        })
    }

    /// A committed field: the save goes now, or waits behind the one in flight, replacing any
    /// that waited. `ask` must be a save.
    pub fn send_save(&mut self, field: FieldKey, ask: Ask) -> Option<Ask> {
        if !matches!(ask, Ask::SaveScenario { .. } | Ask::SaveBench { .. }) {
            return None;
        }
        let saving = Saving {
            field,
            ask: ask.clone(),
        };
        if self.saving.is_some() {
            self.queued_save = Some(saving);
            self.touch();
            return None;
        }
        self.saving = Some(saving);
        self.sent(CockpitAskKind::Save);
        Some(ask)
    }

    /// A click on a run button; `seen` is the hash the caller drew. The run waits for saves.
    pub fn click_run(
        &mut self,
        pr: &PrRef,
        scope: CockpitScope,
        on_main: bool,
        seen: String,
    ) -> Option<Ask> {
        if self.busy() {
            return None;
        }
        if self.saving.is_some() || self.queued_save.is_some() {
            self.queued_run = Some(QueuedRun { scope, on_main });
            self.touch();
            return None;
        }
        self.sent(CockpitAskKind::Run);
        Some(Ask::RunCockpit {
            pr: pr.clone(),
            scope,
            on_main,
            seen,
        })
    }

    /// A click on Stop: the tests' own stop, which also stops a cockpit run.
    pub fn click_stop(&mut self, pr: &PrRef) -> Option<Ask> {
        if self.running_item().is_none() || self.stopping {
            return None;
        }
        self.stopping = true;
        self.pending = Some(CockpitAskKind::Run);
        self.touch();
        Some(Ask::StopTests(pr.clone()))
    }

    /// What the queue sends next, once the save in flight has landed. A waiting run re-reads the
    /// hash it carries from the view after the saves, so it never carries the old one.
    pub fn release_queued(&mut self, pr: &PrRef) -> Released {
        if self.saving.is_some() {
            return Released::Nothing;
        }
        if let Some(next) = self.queued_save.take() {
            let ask = next.ask.clone();
            self.saving = Some(next);
            self.sent(CockpitAskKind::Save);
            return Released::Send(ask);
        }
        let Some(run) = self.queued_run.take() else {
            return Released::Nothing;
        };
        self.touch();
        let seen = self.view.as_ref().and_then(|v| match &run.scope {
            CockpitScope::Scenarios => Some(v.hash.clone()),
            CockpitScope::Item { item } => match item.kind {
                ItemKind::Scenario => v
                    .scenarios
                    .iter()
                    .find(|s| s.slug == item.id)
                    .map(|s| s.hash.clone()),
                ItemKind::Bench => v
                    .benches
                    .iter()
                    .find(|b| b.slug == item.id)
                    .map(|b| b.hash.clone()),
                ItemKind::Load => None,
            },
        });
        let Some(seen) = seen.filter(|_| !self.busy()) else {
            return Released::Dropped;
        };
        self.sent(CockpitAskKind::Run);
        Released::Send(Ask::RunCockpit {
            pr: pr.clone(),
            scope: run.scope,
            on_main: run.on_main,
            seen,
        })
    }

    fn support_of(&self, item: &ItemRef) -> Option<&ItemSupport> {
        let view = self.view.as_ref()?;
        match item.kind {
            ItemKind::Scenario => view
                .scenarios
                .iter()
                .find(|s| s.slug == item.id)
                .map(|s| &s.support),
            ItemKind::Bench => view
                .benches
                .iter()
                .find(|b| b.slug == item.id)
                .map(|b| &b.support),
            ItemKind::Load => None,
        }
    }

    /// Why a run button is off, the first that holds; `item` is `None` for Run all. A main that
    /// is not created is no reason: the click creates it.
    pub fn run_disabled_reason(
        &self,
        item: Option<&ItemRef>,
        on_main: bool,
        tests_busy: bool,
    ) -> Option<String> {
        if tests_busy {
            return Some("Tests are running".into());
        }
        if self.busy() {
            return Some("The QA cockpit is running".into());
        }
        match item {
            None => {
                let any = self.view.as_ref().is_some_and(|v| {
                    v.scenarios.iter().any(|s| {
                        s.approval == ScenarioApproval::Approved && s.support == ItemSupport::Ready
                    })
                });
                if !any {
                    return Some(NO_SCENARIOS.into());
                }
            }
            Some(item) => {
                if let Some(
                    ItemSupport::NoCommand { reason }
                    | ItemSupport::Stale { reason }
                    | ItemSupport::Unsupported { reason },
                ) = self.support_of(item)
                {
                    return Some(reason.clone());
                }
                if on_main
                    && self.view.as_ref().is_some_and(|v| {
                        v.benches.iter().any(|b| {
                            item.kind == ItemKind::Bench
                                && b.slug == item.id
                                && b.change == FunctionChange::New
                        })
                    })
                {
                    return Some(NEW_FUNCTION.into());
                }
            }
        }
        if !on_main {
            return None;
        }
        match self.view.as_ref().map(|v| &v.main.phase) {
            Some(MainPhase::Unavailable { reason }) => Some(reason.clone()),
            Some(MainPhase::NotCreated) | None => None,
            Some(MainPhase::Ready) => None,
            Some(_) => Some(MAIN_FIRST.into()),
        }
    }

    fn fall_back(&mut self) {
        if self.selected.as_ref().is_some_and(|i| self.has_item(i)) {
            return;
        }
        self.suggest_for = None;
        self.selected = self.view.as_ref().and_then(|v| {
            v.scenarios
                .first()
                .map(|s| ItemRef {
                    kind: ItemKind::Scenario,
                    id: s.slug.clone(),
                })
                .or_else(|| {
                    v.benches.first().map(|b| ItemRef {
                        kind: ItemKind::Bench,
                        id: b.slug.clone(),
                    })
                })
        });
    }

    fn take_view(&mut self, view: &CockpitView) {
        self.view = Some(view.clone());
        self.loaded = true;
        if let Some(live) = self.live.as_mut() {
            for key in std::mem::take(&mut live.done) {
                live.points.remove(&key);
            }
        }
        match &view.activity {
            None => {
                self.live_run = None;
                self.stopping = false;
                if self.pending == Some(CockpitAskKind::Run) {
                    self.pending = None;
                }
                self.live = None;
            }
            Some(CockpitActivity::Running { run, .. }) => {
                if self.live.as_ref().is_some_and(|l| &l.run != run) {
                    self.live = None;
                }
            }
            Some(_) => {}
        }
        if view.failed.is_none() {
            self.dismissed_failed = None;
        }
        self.fall_back();
    }

    /// The answer of the oldest ask that waits for a view: what its kind was waiting on is free.
    fn landed(&mut self, kind: CockpitAskKind) {
        match kind {
            CockpitAskKind::Save => {
                if let Some(saving) = self.saving.take() {
                    self.field_errors.remove(&saving.field);
                }
                if self.pending == Some(CockpitAskKind::Save) {
                    self.pending = None;
                }
            }
            CockpitAskKind::Reset | CockpitAskKind::Remove | CockpitAskKind::Approve
                if self.pending == Some(kind) =>
            {
                self.pending = None;
            }
            _ => {}
        }
    }

    fn of_this_head(&self, view: &CockpitView) -> bool {
        self.asked_head.as_deref().is_none_or(|h| view.head == h)
    }

    pub fn apply(&mut self, tell: &CockpitTell) {
        match tell {
            CockpitTell::Loaded { view, .. } => {
                let kind = self.awaiting.pop_front().unwrap_or(CockpitAskKind::Load);
                self.landed(kind);
                if self.of_this_head(view) {
                    self.take_view(view);
                }
                self.touch();
            }
            CockpitTell::Changed { view, .. } => {
                if self.of_this_head(view) {
                    self.take_view(view);
                }
                self.touch();
            }
            CockpitTell::Samples {
                run,
                item,
                side,
                round,
                points,
                ..
            } => {
                let running = self.view.as_ref().and_then(|v| match &v.activity {
                    Some(CockpitActivity::Running {
                        run: r, item: i, ..
                    }) => Some((r, i)),
                    _ => None,
                });
                if running != Some((run, item)) {
                    return;
                }
                if self.live.as_ref().is_none_or(|l| &l.run != run) {
                    self.live = Some(LiveSamples {
                        run: run.clone(),
                        points: BTreeMap::new(),
                        done: BTreeSet::new(),
                    });
                }
                if let Some(live) = self.live.as_mut() {
                    let list = live.points.entry((*side, *round)).or_default();
                    list.extend(points.iter().cloned());
                    if list.len() > SERIES_MAX {
                        let cut = list.len() - SERIES_MAX;
                        list.drain(..cut);
                    }
                }
                self.chart_version += 1;
            }
            CockpitTell::Round { run, result, .. } => {
                if let Some(live) = self.live.as_mut().filter(|l| &l.run == run) {
                    live.done.insert((result.side, result.round));
                    self.chart_version += 1;
                }
            }
            CockpitTell::Script { script, .. } => {
                if self.script.as_ref().map(|s| &s.scenario) != Some(&script.scenario) {
                    self.script_tab = 0;
                }
                self.script = Some((**script).clone());
                self.script_error = None;
                self.touch();
            }
            CockpitTell::Started { run, .. } => {
                if self.pending == Some(CockpitAskKind::Run) {
                    self.pending = None;
                }
                if self.view.as_ref().is_some_and(|v| v.activity.is_some()) {
                    self.live_run = Some(run.clone());
                }
                self.touch();
            }
            CockpitTell::Acked { kind, .. } => {
                if self.pending == Some(*kind) {
                    self.pending = None;
                }
                if *kind == CockpitAskKind::Copilot {
                    self.copilot_sent += 1;
                }
                self.touch();
            }
            CockpitTell::Refused { kind, message, .. } => {
                if self.pending == Some(*kind) {
                    self.pending = None;
                }
                if let Some(at) = self.awaiting.iter().position(|k| k == kind) {
                    self.awaiting.remove(at);
                }
                match kind {
                    CockpitAskKind::Save => {
                        if let Some(saving) = self.saving.take() {
                            self.field_errors.insert(saving.field, printable(message));
                        }
                        self.queued_run = None;
                    }
                    CockpitAskKind::Run => {
                        self.queued_run = None;
                        if message == COCKPIT_CHANGED {
                            self.asked = false;
                        }
                    }
                    CockpitAskKind::Load => {
                        if message != NOT_SENT && message != DEMO_NOT_SENT {
                            self.asked = false;
                        }
                    }
                    CockpitAskKind::Approve => self.run_after_approve = None,
                    CockpitAskKind::Script => self.script_error = Some(printable(message)),
                    _ => {}
                }
                self.touch();
            }
        }
    }
}

/// The cockpits of every open review.
#[derive(Resource, Default)]
pub struct CockpitModels(pub HashMap<PrRef, CockpitModel>);

impl CockpitModels {
    pub fn apply(&mut self, tell: &CockpitTell) {
        self.0.entry(tell.pr().clone()).or_default().apply(tell);
    }

    /// The answers still owed are lost with the connection: ask again once it is back.
    pub fn lost(&mut self) {
        for model in self.0.values_mut() {
            model.asked = false;
            model.pending = None;
            model.awaiting.clear();
            model.saving = None;
            model.queued_save = None;
            model.queued_run = None;
            model.live_run = None;
            model.stopping = false;
            model.run_after_approve = None;
            model.suggest_for = None;
            model.script_asked = None;
            model.touch();
        }
    }
}

/// Whether a region of the cockpit has anything to compare.
pub fn cockpit_changed(models: Res<CockpitModels>) -> bool {
    models.is_changed()
}

/// Sends what waited for a save, or says it cannot go. Written around change detection and
/// announced only when something was sent.
pub fn send_queued(
    mut models: ResMut<CockpitModels>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let mut done = false;
    for (pr, model) in &mut models.bypass_change_detection().0 {
        loop {
            match model.release_queued(pr) {
                Released::Nothing => break,
                Released::Send(ask) => {
                    asks.send(ask);
                    done = true;
                }
                Released::Dropped => {
                    toasts.0.push(Toast {
                        text: COCKPIT_CHANGED.into(),
                        warning: true,
                        until: time.elapsed_secs_f64() + TOAST_SECS,
                    });
                    done = true;
                }
            }
        }
    }
    if done {
        models.set_changed();
    }
}

/// Copies what is busy into the models that wait for each other, and wakes a resource only when
/// a flag changed.
pub fn mirror_busy(
    mut cockpit: ResMut<CockpitModels>,
    functions: Res<FunctionsModels>,
    mut tests: ResMut<TestsModels>,
) {
    let mut wrote_tests = false;
    for (pr, m) in &mut tests.bypass_change_detection().0 {
        let busy = cockpit.0.get(pr).is_some_and(CockpitModel::busy);
        if m.cockpit_busy != busy {
            m.cockpit_busy = busy;
            m.touch();
            wrote_tests = true;
        }
    }
    if wrote_tests {
        tests.set_changed();
    }
    let mut wrote_cockpit = false;
    for (pr, m) in &mut cockpit.bypass_change_detection().0 {
        let other = tests.0.get(pr);
        let busy = other.is_some_and(|t| t.tests_busy() || t.functions_busy)
            || functions
                .0
                .get(pr)
                .is_some_and(functions::FunctionsModel::busy);
        if m.tests_busy != busy {
            m.tests_busy = busy;
            m.touch();
            wrote_cockpit = true;
        }
    }
    if wrote_cockpit {
        cockpit.set_changed();
    }
}

/// Adds the cockpit's resource and systems. Every region's fill system joins here, after the
/// Tests body that holds its nodes.
pub fn register(app: &mut App) {
    app.init_resource::<CockpitModels>()
        .add_systems(
            Update,
            (
                send_queued.run_if(resource_changed::<CockpitModels>),
                mirror_busy
                    .run_if(
                        resource_changed::<CockpitModels>
                            .or_eager(resource_changed::<TestsModels>)
                            .or_eager(resource_changed::<FunctionsModels>),
                    )
                    .after(functions::mirror_busy),
            )
                .chain()
                .after(ReviewSystems),
        )
        .add_systems(
            Update,
            (
                frame::load_cockpit.run_if(frame::load_changed),
                frame::hide_for_cockpit
                    .run_if(frame::screen_changed)
                    .before(crate::screens::review::agent::panel::layout),
                frame::fill_header.run_if(frame::regions_changed),
                frame::fill_center_frame.run_if(frame::regions_changed),
                list::fill_list.run_if(frame::regions_changed),
            )
                .after(body::fill_tests)
                .after(ReviewSystems),
        )
        .add_systems(
            Update,
            (
                center::fill_center.run_if(frame::regions_changed),
                center::fill_foot.run_if(frame::regions_changed),
                center::fill_errors.run_if(frame::regions_changed),
                center::ask_script_when_needed.run_if(resource_changed::<CockpitModels>),
                center::commit_scenario_fields.run_if(on_message::<FieldCommitted>),
                center::commit_bench_fields.run_if(on_message::<FieldCommitted>),
                modals::run_after_approval.run_if(modals::run_after_due),
                modals::open_cockpit_modals.run_if(frame::regions_changed),
                modals::fill_approve.run_if(frame::regions_changed),
                modals::fill_main.run_if(frame::regions_changed),
                modals::fill_remove.run_if(frame::regions_changed),
                modals::cancel_cockpit_modal.run_if(modals::cockpit_modal_up),
            )
                .chain()
                .after(frame::fill_center_frame)
                .after(list::fill_list),
        );
    chart::register(app);
    compare::register(app);
    copilot::register(app);
}

#[cfg(test)]
mod tests {
    use clusia_core::cockpit::{
        RoundEnd, RoundResult, SERVICES_ON_MAIN, ScenarioApproval, ScenarioInput,
    };

    use super::*;
    use crate::fixture::{self, DemoCockpit, demo_cockpit};
    use crate::testing::{self, NOW};

    fn pr() -> PrRef {
        fixture::demo_pr()
    }

    fn scenario() -> CockpitView {
        demo_cockpit(NOW, DemoCockpit::Scenario)
    }

    fn running() -> CockpitView {
        demo_cockpit(NOW, DemoCockpit::BenchRunning)
    }

    fn loaded(view: CockpitView) -> CockpitTell {
        CockpitTell::Loaded {
            pr: pr(),
            view: Box::new(view),
        }
    }

    fn changed(view: CockpitView) -> CockpitTell {
        CockpitTell::Changed {
            pr: pr(),
            view: Box::new(view),
        }
    }

    fn item(kind: ItemKind, id: &str) -> ItemRef {
        ItemRef {
            kind,
            id: id.into(),
        }
    }

    const REFRESH: &str = "refresh-under-steady-traffic";

    fn refresh() -> ItemRef {
        item(ItemKind::Scenario, REFRESH)
    }

    fn backoff_slug(view: &CockpitView) -> String {
        view.benches[0].slug.clone()
    }

    fn model_of(view: CockpitView) -> CockpitModel {
        let mut m = CockpitModel::default();
        m.apply(&loaded(view));
        m
    }

    fn key(name: &str) -> FieldKey {
        FieldKey {
            item: refresh(),
            field: Field::Value(name.into()),
        }
    }

    fn save_ask(callers: &str) -> Ask {
        Ask::SaveScenario {
            pr: pr(),
            scenario: REFRESH.into(),
            input: ScenarioInput {
                values: [("callers".to_string(), callers.to_string())].into(),
                ..ScenarioInput::default()
            },
        }
    }

    fn hash_of(view: &CockpitView, slug: &str) -> String {
        view.scenarios
            .iter()
            .find(|s| s.slug == slug)
            .map(|s| s.hash.clone())
            .expect("scenario")
    }

    #[test]
    fn the_first_scenario_is_selected_then_the_first_bench_and_a_vanished_one_falls_back() {
        let mut m = CockpitModel::default();
        assert_eq!(m.selected, None);
        m.apply(&loaded(scenario()));
        assert_eq!(
            m.selected,
            Some(item(ItemKind::Scenario, "two-requests-refresh-at-once"))
        );
        let view = scenario();
        let bench = item(ItemKind::Bench, &backoff_slug(&view));
        m.select(bench.clone());
        assert_eq!(m.selected, Some(bench));
        let mut without = scenario();
        without.benches.remove(0);
        m.apply(&changed(without));
        assert_eq!(
            m.selected,
            Some(item(ItemKind::Scenario, "two-requests-refresh-at-once"))
        );
        m.select(item(ItemKind::Scenario, "nope"));
        assert_eq!(
            m.selected,
            Some(item(ItemKind::Scenario, "two-requests-refresh-at-once")),
            "only an item the view has"
        );
        let mut only_bench = scenario();
        only_bench.scenarios.clear();
        let slug = backoff_slug(&only_bench);
        let mut m = model_of(only_bench);
        assert_eq!(m.selected, Some(item(ItemKind::Bench, &slug)));
        m.apply(&changed(CockpitView::new(&scenario().head)));
        assert_eq!(m.selected, None);
    }

    #[test]
    fn a_view_of_another_head_is_ignored() {
        let mut m = CockpitModel::default();
        let _ = m.ask_load(&pr(), &scenario().head);
        assert_eq!(m.awaiting.len(), 1);
        let mut other = scenario();
        other.head = "other".into();
        m.apply(&loaded(other.clone()));
        assert!(m.view.is_none());
        assert!(m.awaiting.is_empty());
        m.apply(&changed(other));
        assert!(m.view.is_none());
    }

    #[test]
    fn run_after_an_unsaved_edit_waits_for_the_save_and_carries_the_new_hash() {
        let mut m = model_of(scenario());
        let old = hash_of(&scenario(), REFRESH);
        let save = save_ask("80");
        assert_eq!(m.send_save(key("callers"), save.clone()), Some(save));
        let scope = CockpitScope::Item { item: refresh() };
        assert_eq!(m.click_run(&pr(), scope.clone(), false, old), None);
        assert_eq!(m.release_queued(&pr()), Released::Nothing, "the save flies");
        let mut after = scenario();
        after
            .scenarios
            .iter_mut()
            .find(|s| s.slug == REFRESH)
            .expect("refresh")
            .hash = "new".into();
        m.apply(&loaded(after));
        assert_eq!(
            m.release_queued(&pr()),
            Released::Send(Ask::RunCockpit {
                pr: pr(),
                scope,
                on_main: false,
                seen: "new".into()
            })
        );
        assert_eq!(m.release_queued(&pr()), Released::Nothing);
    }

    #[test]
    fn a_reply_to_an_earlier_load_does_not_land_the_save() {
        let mut m = model_of(scenario());
        let _ = m.ask_load(&pr(), &scenario().head);
        let _ = m.send_save(key("callers"), save_ask("80"));
        let scope = CockpitScope::Item { item: refresh() };
        assert_eq!(m.click_run(&pr(), scope.clone(), false, "old".into()), None);
        let mut stale = scenario();
        stale.hash = "stale".into();
        m.apply(&loaded(stale));
        assert!(m.saving.is_some(), "the first answer was the read's");
        assert_eq!(m.release_queued(&pr()), Released::Nothing);
        let mut after = scenario();
        after
            .scenarios
            .iter_mut()
            .find(|s| s.slug == REFRESH)
            .expect("refresh")
            .hash = "new".into();
        m.apply(&loaded(after));
        assert!(m.saving.is_none());
        assert_eq!(
            m.release_queued(&pr()),
            Released::Send(Ask::RunCockpit {
                pr: pr(),
                scope,
                on_main: false,
                seen: "new".into()
            })
        );
    }

    #[test]
    fn a_second_commit_while_saving_replaces_the_queued_one_and_goes_after() {
        let mut m = model_of(scenario());
        let (a, b, c) = (save_ask("1"), save_ask("2"), save_ask("3"));
        assert_eq!(m.send_save(key("callers"), a.clone()), Some(a));
        assert_eq!(m.send_save(key("duration"), b), None);
        assert_eq!(m.send_save(key("token_lifetime"), c.clone()), None);
        assert_eq!(m.queued_save.as_ref().map(|s| &s.ask), Some(&c));
        m.apply(&loaded(scenario()));
        assert_eq!(m.release_queued(&pr()), Released::Send(c));
        assert_eq!(
            m.saving.as_ref().map(|s| s.field.clone()),
            Some(key("token_lifetime"))
        );
        assert_eq!(
            m.send_save(key("x"), Ask::StopTests(pr())),
            None,
            "only a save"
        );
    }

    #[test]
    fn a_refused_save_shows_under_its_field_and_drops_the_waiting_run() {
        let mut m = model_of(scenario());
        let _ = m.send_save(key("callers"), save_ask("x"));
        let scope = CockpitScope::Item { item: refresh() };
        assert_eq!(m.click_run(&pr(), scope, false, "h".into()), None);
        assert!(m.queued_run.is_some());
        m.apply(&CockpitTell::Refused {
            pr: pr(),
            kind: CockpitAskKind::Save,
            message: "Not a whole number".into(),
        });
        assert_eq!(m.field_errors[&key("callers")], "Not a whole number");
        assert!(m.queued_run.is_none());
        assert!(m.saving.is_none());
        assert!(m.awaiting.is_empty());
        // The next landed save of that field clears its error.
        let _ = m.send_save(key("callers"), save_ask("8"));
        m.apply(&loaded(scenario()));
        assert!(!m.field_errors.contains_key(&key("callers")));
    }

    #[test]
    fn a_changed_refusal_reasks_and_never_resends() {
        let mut m = model_of(scenario());
        let _ = m.ask_load(&pr(), &scenario().head);
        m.apply(&loaded(scenario()));
        let scope = CockpitScope::Item { item: refresh() };
        let run = m.click_run(&pr(), scope, false, "h".into());
        assert!(matches!(run, Some(Ask::RunCockpit { .. })));
        assert_eq!(m.pending, Some(CockpitAskKind::Run));
        m.apply(&CockpitTell::Refused {
            pr: pr(),
            kind: CockpitAskKind::Run,
            message: COCKPIT_CHANGED.into(),
        });
        assert!(!m.asked);
        assert_eq!(m.pending, None);
        assert_eq!(m.release_queued(&pr()), Released::Nothing);
    }

    fn flood(run: &str, item: &ItemRef, side: RunSide, round: u32, n: u32) -> CockpitTell {
        CockpitTell::Samples {
            pr: pr(),
            run: run.into(),
            item: item.clone(),
            side,
            round,
            points: (0..n)
                .map(|t| Sample {
                    t_ms: t,
                    rss_kb: Some(1),
                    cpu_milli: None,
                })
                .collect(),
        }
    }

    #[test]
    fn a_flood_of_samples_keeps_the_newest_series_max_and_moves_only_the_chart() {
        let view = running();
        let bench = item(ItemKind::Bench, &backoff_slug(&view));
        let mut m = model_of(view);
        let (v, c) = (m.version, m.chart_version);
        m.apply(&flood("demo-bench-run", &bench, RunSide::Main, 3, 10_000));
        let live = m.live.as_ref().expect("live");
        let points = &live.points[&(RunSide::Main, 3)];
        assert_eq!(points.len(), SERIES_MAX);
        assert_eq!(points[0].t_ms, 10_000 - SERIES_MAX as u32);
        assert_eq!(m.version, v);
        assert!(m.chart_version > c);
        let before = m.live.clone();
        m.apply(&flood("other", &bench, RunSide::Main, 3, 5));
        m.apply(&flood("demo-bench-run", &refresh(), RunSide::Main, 3, 5));
        assert_eq!(
            m.live, before,
            "samples of another run or item change nothing"
        );
        m.apply(&changed(scenario()));
        assert!(m.live.is_none());
        assert!(m.live_run.is_none());
    }

    #[test]
    fn a_finished_round_leaves_the_live_buffer_once_the_view_has_it() {
        let view = running();
        let bench = item(ItemKind::Bench, &backoff_slug(&view));
        let mut m = model_of(view.clone());
        m.apply(&flood("demo-bench-run", &bench, RunSide::Pr, 1, 3));
        m.apply(&CockpitTell::Round {
            pr: pr(),
            run: "demo-bench-run".into(),
            result: Box::new(RoundResult {
                item: bench,
                side: RunSide::Pr,
                round: 1,
                sha: "s".into(),
                passed: None,
                end: RoundEnd::Finished,
                exit_code: None,
                metrics: Default::default(),
                outputs: Default::default(),
                errors: Vec::new(),
                message: None,
                tail: String::new(),
                took_ms: 1,
                at: 0,
            }),
        });
        assert!(
            m.live
                .as_ref()
                .expect("live")
                .points
                .contains_key(&(RunSide::Pr, 1))
        );
        m.apply(&changed(view));
        assert!(
            !m.live
                .as_ref()
                .expect("still live")
                .points
                .contains_key(&(RunSide::Pr, 1))
        );
    }

    #[test]
    fn stop_sends_stop_tests_and_holds_until_the_run_ends() {
        let mut m = model_of(scenario());
        assert_eq!(m.click_stop(&pr()), None, "nothing runs");
        m.apply(&changed(running()));
        assert_eq!(m.click_stop(&pr()), Some(Ask::StopTests(pr())));
        assert!(m.busy());
        assert_eq!(m.pending, Some(CockpitAskKind::Run));
        assert_eq!(m.click_stop(&pr()), None, "a second click sends nothing");
        m.apply(&changed(scenario()));
        assert!(!m.stopping);
        assert_eq!(m.pending, None);
        assert!(!m.busy());
    }

    #[test]
    fn an_acked_copilot_ask_frees_the_input() {
        let mut m = model_of(scenario());
        m.sent(CockpitAskKind::Copilot);
        assert_eq!(m.pending, Some(CockpitAskKind::Copilot));
        m.apply(&CockpitTell::Acked {
            pr: pr(),
            kind: CockpitAskKind::Copilot,
        });
        assert_eq!(m.pending, None);
        assert_eq!(m.copilot_sent, 1);
    }

    #[test]
    fn the_script_is_asked_once_per_approval_hash() {
        let mut m = model_of(scenario());
        m.select(item(ItemKind::Scenario, "token-endpoint-is-down"));
        assert_eq!(
            m.want_script(&pr()),
            Some(Ask::GetScenarioScript {
                pr: pr(),
                scenario: "token-endpoint-is-down".into()
            })
        );
        assert_eq!(m.want_script(&pr()), None);
        m.apply(&CockpitTell::Script {
            pr: pr(),
            script: Box::new(fixture::demo_scenario_script()),
        });
        assert_eq!(m.want_script(&pr()), None);
        m.select(refresh());
        assert!(m.want_script(&pr()).is_some());
        m.apply(&CockpitTell::Refused {
            pr: pr(),
            kind: CockpitAskKind::Script,
            message: "x".into(),
        });
        assert_eq!(m.script_error.as_deref(), Some("x"));
        assert_eq!(m.want_script(&pr()), None, "no loop on a refusal");
    }

    fn view_with(edit: impl FnOnce(&mut CockpitView)) -> CockpitView {
        let mut view = scenario();
        edit(&mut view);
        view
    }

    #[test]
    fn the_run_buttons_say_why_they_are_off() {
        let m = model_of(scenario());
        assert_eq!(
            m.run_disabled_reason(None, false, true).as_deref(),
            Some("Tests are running")
        );
        let busy = model_of(running());
        assert_eq!(
            busy.run_disabled_reason(Some(&refresh()), false, false)
                .as_deref(),
            Some("The QA cockpit is running")
        );
        let empty = model_of(demo_cockpit(NOW, DemoCockpit::Empty));
        assert_eq!(
            empty.run_disabled_reason(None, false, false).as_deref(),
            Some(NO_SCENARIOS)
        );
        assert_eq!(m.run_disabled_reason(None, true, false), None);
        let setup = model_of(demo_cockpit(NOW, DemoCockpit::Main));
        assert_eq!(
            setup.run_disabled_reason(None, true, false).as_deref(),
            Some(MAIN_FIRST)
        );
        let not_created = model_of(view_with(|v| v.main.phase = MainPhase::NotCreated));
        assert_eq!(not_created.run_disabled_reason(None, true, false), None);
        let bench = item(ItemKind::Bench, &backoff_slug(&scenario()));
        let new_fn = model_of(view_with(|v| {
            v.benches[0].change = clusia_core::functions::FunctionChange::New;
        }));
        assert_eq!(
            new_fn
                .run_disabled_reason(Some(&bench), true, false)
                .as_deref(),
            Some(NEW_FUNCTION)
        );
        assert_eq!(new_fn.run_disabled_reason(Some(&bench), false, false), None);
        let unavailable = model_of(view_with(|v| {
            v.main.phase = MainPhase::Unavailable {
                reason: SERVICES_ON_MAIN.into(),
            };
        }));
        assert_eq!(
            unavailable
                .run_disabled_reason(Some(&refresh()), true, false)
                .as_deref(),
            Some(SERVICES_ON_MAIN)
        );
        let stale = model_of(view_with(|v| {
            v.scenarios[1].support = ItemSupport::Stale { reason: "r".into() };
        }));
        assert_eq!(
            stale
                .run_disabled_reason(Some(&refresh()), false, false)
                .as_deref(),
            Some("r")
        );
        let third = item(ItemKind::Scenario, "token-endpoint-is-down");
        assert_eq!(m.run_disabled_reason(Some(&third), false, false), None);
        assert_eq!(
            m.view.as_ref().map(|v| v.scenarios[2].approval),
            Some(ScenarioApproval::NotApproved)
        );
    }

    #[test]
    fn a_lost_connection_frees_everything_owed() {
        let mut models = CockpitModels::default();
        models.apply(&loaded(scenario()));
        let m = models.0.get_mut(&pr()).expect("model");
        let _ = m.ask_load(&pr(), &scenario().head);
        let _ = m.send_save(key("callers"), save_ask("1"));
        let _ = m.send_save(key("duration"), save_ask("2"));
        m.queued_run = Some(QueuedRun {
            scope: CockpitScope::Scenarios,
            on_main: false,
        });
        m.live_run = Some("r".into());
        m.stopping = true;
        m.pending = Some(CockpitAskKind::Run);
        m.script_asked = Some(("a".into(), "b".into()));
        models.lost();
        let m = &models.0[&pr()];
        assert!(!m.asked);
        assert!(m.saving.is_none() && m.queued_save.is_none() && m.queued_run.is_none());
        assert!(m.awaiting.is_empty());
        assert!(m.live_run.is_none() && !m.stopping && m.pending.is_none());
        assert!(m.script_asked.is_none());
    }

    #[test]
    fn a_started_run_is_live_only_while_the_view_has_an_activity() {
        let mut m = model_of(scenario());
        m.apply(&CockpitTell::Started {
            pr: pr(),
            run: "r1".into(),
            skipped: Vec::new(),
        });
        assert_eq!(m.live_run, None, "a run that already ended is not live");
        let mut m = model_of(running());
        m.apply(&CockpitTell::Started {
            pr: pr(),
            run: "demo-bench-run".into(),
            skipped: Vec::new(),
        });
        assert_eq!(m.live_run.as_deref(), Some("demo-bench-run"));
    }

    fn app_on_tests() -> (App, PrRef) {
        let mut app = testing::demo_review_app();
        let pr = pr();
        testing::set_section(&mut app, &pr, crate::review_state::ReviewSection::Tests);
        (app, pr)
    }

    #[test]
    fn the_cockpit_and_the_tests_wait_for_each_other() {
        use crate::bridge::{FunctionsTell, Tell};
        use clusia_core::functions::{FunctionsActivity, FunctionsView};
        let (mut app, pr) = app_on_tests();
        testing::open_cockpit(&mut app, &pr, running());
        let tests = &app.world().resource::<TestsModels>().0[&pr];
        assert!(tests.cockpit_busy && tests.busy());
        assert_eq!(
            functions::run_disabled_reason(None, Some(tests)),
            Some("The QA cockpit is running")
        );
        let mut writing = FunctionsView::new(&running().head);
        writing.activity = Some(FunctionsActivity::Writing {
            function: None,
            ask: None,
        });
        testing::tell(
            &mut app,
            Tell::Functions(FunctionsTell::Loaded {
                pr: pr.clone(),
                view: Box::new(writing),
            }),
        );
        testing::settle(&mut app);
        assert!(app.world().resource::<CockpitModels>().0[&pr].tests_busy);
    }

    #[test]
    fn a_load_that_could_not_be_sent_is_not_asked_again_at_once() {
        for message in [NOT_SENT, DEMO_NOT_SENT] {
            let mut m = model_of(scenario());
            let _ = m.ask_load(&pr(), &scenario().head);
            m.apply(&CockpitTell::Refused {
                pr: pr(),
                kind: CockpitAskKind::Load,
                message: message.into(),
            });
            assert!(m.asked, "{message}");
        }
        let mut m = model_of(scenario());
        let _ = m.ask_load(&pr(), &scenario().head);
        m.apply(&CockpitTell::Refused {
            pr: pr(),
            kind: CockpitAskKind::Load,
            message: "boom".into(),
        });
        assert!(!m.asked);
    }
}
