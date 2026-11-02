//! The Run Function tests sub-tab's model: the changed functions of a review, the selection and
//! the open case, the reviewer's edits on their way to the daemon, and what is busy. Everything
//! the daemon sent is text here: nothing in this file draws it, parses it as a path or opens it.

use std::collections::{BTreeMap, HashMap};

use bevy::prelude::*;
use clusia_core::PrRef;
use clusia_core::functions::{
    CaseInput, CaseResult, CaseScope, CaseStatus, CaseView, DriverCode, FunctionView,
    FunctionsActivity, FunctionsView, Support, check_value,
};

use super::{TestsModel, TestsModels};
use crate::bridge::{
    Ask, Asks, Connection, FunctionsAskKind, FunctionsTell, Model, TOAST_SECS, Toast, Toasts,
};
use crate::review_state::{ReviewSection, ReviewTabs};

/// The height of one case row; the open case's fields sit under it.
pub const CASE_ROW_HEIGHT: f32 = 35.0;

/// The root of the Run Function tests sub-tab of a review.
#[derive(Component)]
pub struct FunctionsBody(pub PrRef);

/// The functions of every open review, by review.
#[derive(Resource, Default)]
pub struct FunctionsModels(pub HashMap<PrRef, FunctionsModel>);

#[derive(Default)]
pub struct FunctionsModel {
    pub view: Option<FunctionsView>,
    /// Whether the view was asked for and not lost with a connection.
    pub asked: bool,
    pub loaded: bool,
    /// The review head the view was last asked for.
    pub asked_head: Option<String>,
    /// The selected function's slug.
    pub selected: Option<String>,
    pub open_case: Option<String>,
    /// Why a field's text was not sent, by function slug, case id and parameter.
    pub field_errors: BTreeMap<(String, String, String), String>,
    /// The values of the save in flight: function slug, case id and the case's whole values.
    pub saving: Option<(String, String, BTreeMap<String, String>)>,
    /// A run that waits for its save to land.
    pub queued_run: Option<QueuedRun>,
    pub pending: Option<FunctionsAskKind>,
    pub driver: Option<DriverModal>,
    /// The run this window started, until the daemon's activity ends.
    pub live: Option<String>,
    /// Bumped by every change, so a region can tell its view may differ.
    pub version: u64,
    /// The write's activity has been seen, so the next idle view is the write's end.
    pub write_started: bool,
}

/// A run of one case that waits for the save of the values it was clicked with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedRun {
    pub function: String,
    pub case: String,
    pub values: BTreeMap<String, String>,
}

/// The driver modal: what it waits for, and the code once it came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverModal {
    pub function: String,
    pub code: Option<DriverCode>,
    pub loading: bool,
}

/// What `release_queued` decided about a run that waited for its save.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)] // short-lived, and the caller matches on the ask itself
pub enum Queued {
    Nothing,
    Run(Ask),
    Dropped,
}

impl FunctionsModel {
    /// Bumps `version` for the callers that change a field directly.
    pub fn touch(&mut self) {
        self.version += 1;
    }

    pub fn selected_view(&self) -> Option<&FunctionView> {
        let slug = self.selected.as_deref()?;
        self.view
            .as_ref()?
            .functions
            .iter()
            .find(|f| f.slug == slug)
    }

    pub fn open_view(&self) -> Option<&CaseView> {
        let id = self.open_case.as_deref()?;
        self.selected_view()?.cases.iter().find(|c| c.id == id)
    }

    /// The case's result when it is the case's: it ran with these values, on this head.
    pub fn current_result<'a>(&self, case: &'a CaseView) -> Option<&'a CaseResult> {
        case.result
            .as_ref()
            .filter(|r| r.hash == case.hash && self.view.as_ref().is_some_and(|v| v.head == r.head))
    }

    /// The cases of `f` that passed and that did not, among the ones with a current result.
    pub fn counts(&self, f: &FunctionView) -> (u32, u32) {
        let (mut passed, mut failed) = (0, 0);
        for result in f.cases.iter().filter_map(|c| self.current_result(c)) {
            if result.status == CaseStatus::Passed {
                passed += 1;
            } else {
                failed += 1;
            }
        }
        (passed, failed)
    }

    /// The cases a run of everything would run.
    pub fn runnable_total(&self) -> u32 {
        self.view.as_ref().map_or(0, |v| {
            v.functions
                .iter()
                .filter(|f| f.support == Support::Ready)
                .map(|f| f.cases.len() as u32)
                .sum()
        })
    }

    /// Whether cases are being written or run. A save, a reset, a removal or a driver in flight
    /// is not: a click on ▶ meanwhile queues its run.
    pub fn busy(&self) -> bool {
        self.view.as_ref().is_some_and(|v| v.activity.is_some())
            || self.live.is_some()
            || matches!(
                self.pending,
                Some(FunctionsAskKind::Run | FunctionsAskKind::Write)
            )
    }

    /// What the activity is doing, for the line that says so.
    pub fn activity_text(&self) -> Option<String> {
        match self.view.as_ref().and_then(|v| v.activity.as_ref()) {
            Some(FunctionsActivity::Writing { .. }) => Some("Writing cases…".into()),
            Some(FunctionsActivity::Running { total, done, .. }) => {
                Some(format!("Running · {done} of {total} cases"))
            }
            None => match self.pending {
                Some(FunctionsAskKind::Write) => Some("Writing cases…".into()),
                Some(FunctionsAskKind::Run) => Some("Starting the run…".into()),
                _ => None,
            },
        }
    }

    /// Records that `kind` went out and its answer is awaited.
    pub fn sent(&mut self, kind: FunctionsAskKind) {
        self.pending = Some(kind);
        if kind == FunctionsAskKind::Write {
            self.write_started = false;
        }
        self.touch();
    }

    pub fn select(&mut self, slug: &str) {
        if self
            .view
            .as_ref()
            .is_some_and(|v| v.functions.iter().any(|f| f.slug == slug))
        {
            self.selected = Some(slug.to_string());
            self.open_case = None;
            self.defaults();
            self.touch();
        }
    }

    pub fn open(&mut self, case: &str) {
        if self
            .selected_view()
            .is_some_and(|f| f.cases.iter().any(|c| c.id == case))
        {
            self.open_case = Some(case.to_string());
            self.touch();
        }
    }

    /// Keeps the selection and the open case while they exist; otherwise the first function that
    /// can run, and its first case that has not passed.
    fn defaults(&mut self) {
        let Some(view) = self.view.as_ref() else {
            self.selected = None;
            self.open_case = None;
            return;
        };
        let kept = self
            .selected
            .clone()
            .filter(|slug| view.functions.iter().any(|f| f.slug == *slug));
        let selected = kept.or_else(|| {
            view.functions
                .iter()
                .find(|f| f.support == Support::Ready)
                .or(view.functions.first())
                .map(|f| f.slug.clone())
        });
        let open = selected.as_deref().and_then(|slug| {
            let f = view.functions.iter().find(|f| f.slug == slug)?;
            self.open_case
                .clone()
                .filter(|id| f.cases.iter().any(|c| c.id == *id))
                .or_else(|| {
                    f.cases
                        .iter()
                        .find(|c| {
                            self.current_result(c)
                                .is_none_or(|r| r.status != CaseStatus::Passed)
                        })
                        .or(f.cases.first())
                        .map(|c| c.id.clone())
                })
        });
        self.selected = selected;
        self.open_case = open;
    }

    /// A field's text was committed. A text that fails its check is not sent; otherwise the
    /// case's whole values go out with this one laid over the saved ones.
    pub fn commit_field(
        &mut self,
        pr: &PrRef,
        function: &str,
        case: &str,
        param: &str,
        text: &str,
    ) -> Option<Ask> {
        let view = self.view.as_ref()?;
        let f = view.functions.iter().find(|f| f.slug == function)?;
        let kind = f.params.iter().find(|p| p.name == param)?.kind;
        let c = f.cases.iter().find(|c| c.id == case)?;
        let key = (function.to_string(), case.to_string(), param.to_string());
        if let Err(message) = check_value(kind, text) {
            self.field_errors.insert(key, message.to_string());
            self.touch();
            return None;
        }
        let name = c.name.clone();
        let mut values = c.values.clone();
        let unchanged = values.get(param).map(String::as_str) == Some(text);
        values.insert(param.to_string(), text.to_string());
        let cleared = self.field_errors.remove(&key).is_some();
        let carried = self
            .saving
            .as_ref()
            .is_some_and(|(f, c, v)| f == function && c == case && *v == values);
        if unchanged || carried {
            if cleared {
                self.touch();
            }
            return None;
        }
        Some(self.save(pr, function, case, name, values))
    }

    fn save(
        &mut self,
        pr: &PrRef,
        function: &str,
        case: &str,
        name: String,
        values: BTreeMap<String, String>,
    ) -> Ask {
        self.saving = Some((function.to_string(), case.to_string(), values.clone()));
        self.sent(FunctionsAskKind::Save);
        Ask::SaveCase {
            pr: pr.clone(),
            function: function.to_string(),
            case: CaseInput {
                id: Some(case.to_string()),
                name,
                values,
            },
        }
    }

    /// A click on ▶ or **Run this case**; `shown` is what the fields say now. Saved values run
    /// at once, under the function's hash. Edited ones are saved first, and the run waits for the
    /// save so it carries the hash of what was saved.
    pub fn click_run(
        &mut self,
        pr: &PrRef,
        function: &str,
        case: &str,
        shown: BTreeMap<String, String>,
    ) -> Option<Ask> {
        if self.busy() {
            return None;
        }
        let f = self
            .view
            .as_ref()?
            .functions
            .iter()
            .find(|f| f.slug == function)?;
        if f.support != Support::Ready {
            return None;
        }
        let c = f.cases.iter().find(|c| c.id == case)?;
        if shown == c.values && self.saving.is_none() {
            let seen = f.hash.clone();
            self.sent(FunctionsAskKind::Run);
            return Some(Ask::RunCases {
                pr: pr.clone(),
                scope: CaseScope::Case {
                    function: function.to_string(),
                    case: case.to_string(),
                },
                seen,
            });
        }
        let name = c.name.clone();
        let mut refused = false;
        let mut errors = Vec::new();
        for param in &f.params {
            let key = (function.to_string(), case.to_string(), param.name.clone());
            let text = shown.get(&param.name).map_or("", String::as_str);
            errors.push((key, check_value(param.kind, text).err()));
        }
        for (key, error) in errors {
            match error {
                Some(message) => {
                    self.field_errors.insert(key, message.to_string());
                    refused = true;
                }
                None => {
                    self.field_errors.remove(&key);
                }
            }
        }
        if refused {
            self.touch();
            return None;
        }
        self.queued_run = Some(QueuedRun {
            function: function.to_string(),
            case: case.to_string(),
            values: shown.clone(),
        });
        let carried = self
            .saving
            .as_ref()
            .is_some_and(|(f, c, v)| f == function && c == case && *v == shown);
        if carried {
            self.touch();
            return None;
        }
        Some(self.save(pr, function, case, name, shown))
    }

    /// After a save landed (`saving` is none): the run that waited goes out when the case now
    /// holds the values it was clicked with, under the new hash; otherwise it is dropped.
    pub fn release_queued(&mut self, pr: &PrRef) -> Queued {
        if self.saving.is_some() {
            return Queued::Nothing;
        }
        let Some(queued) = self.queued_run.take() else {
            return Queued::Nothing;
        };
        let landed = self
            .view
            .as_ref()
            .and_then(|v| v.functions.iter().find(|f| f.slug == queued.function))
            .filter(|f| f.support == Support::Ready)
            .and_then(|f| {
                let c = f.cases.iter().find(|c| c.id == queued.case)?;
                (c.values == queued.values).then(|| f.hash.clone())
            });
        self.touch();
        match landed {
            Some(seen) if !self.busy() => {
                self.sent(FunctionsAskKind::Run);
                Queued::Run(Ask::RunCases {
                    pr: pr.clone(),
                    scope: CaseScope::Case {
                        function: queued.function,
                        case: queued.case,
                    },
                    seen,
                })
            }
            _ => Queued::Dropped,
        }
    }

    /// Asks for a function's driver and opens its modal, waiting.
    pub fn open_driver(&mut self, pr: &PrRef, function: &str) -> Ask {
        self.driver = Some(DriverModal {
            function: function.to_string(),
            code: None,
            loading: true,
        });
        self.touch();
        Ask::GetDriver {
            pr: pr.clone(),
            function: function.to_string(),
        }
    }

    pub fn close_driver(&mut self) {
        self.driver = None;
        self.touch();
    }

    pub fn apply(&mut self, tell: &FunctionsTell) {
        match tell {
            FunctionsTell::Loaded { view, .. } => self.loaded_view(view),
            FunctionsTell::CaseDone {
                function, result, ..
            } => {
                let case = self
                    .view
                    .as_mut()
                    .and_then(|v| v.functions.iter_mut().find(|f| f.slug == *function))
                    .and_then(|f| f.cases.iter_mut().find(|c| c.id == result.case));
                if let Some(case) = case {
                    case.result = Some((**result).clone());
                    self.touch();
                }
            }
            FunctionsTell::Started { run, .. } => {
                self.live = Some(run.clone());
                if self.pending == Some(FunctionsAskKind::Run) {
                    self.pending = None;
                }
                self.touch();
            }
            FunctionsTell::Driver { code, .. } => {
                if let Some(modal) = self
                    .driver
                    .as_mut()
                    .filter(|m| m.loading && m.function == code.function)
                {
                    modal.code = Some((**code).clone());
                    modal.loading = false;
                    self.touch();
                }
            }
            FunctionsTell::Refused { ask, .. } => {
                if self.pending == Some(*ask) {
                    self.pending = None;
                }
                match ask {
                    FunctionsAskKind::Save => {
                        self.saving = None;
                        self.queued_run = None;
                        self.asked = false;
                    }
                    FunctionsAskKind::Reset | FunctionsAskKind::Remove => self.asked = false,
                    FunctionsAskKind::Write => self.write_started = false,
                    FunctionsAskKind::Driver => self.driver = None,
                    FunctionsAskKind::Run => {}
                }
                self.touch();
            }
        }
    }

    fn loaded_view(&mut self, view: &FunctionsView) {
        self.view = Some(view.clone());
        self.asked = true;
        self.loaded = true;
        match self.pending {
            Some(FunctionsAskKind::Write) => {
                if view.activity.is_some() {
                    self.write_started = true;
                } else if self.write_started {
                    self.pending = None;
                    self.write_started = false;
                }
            }
            Some(FunctionsAskKind::Save | FunctionsAskKind::Reset | FunctionsAskKind::Remove) => {
                self.pending = None;
            }
            _ => {}
        }
        if let Some((function, case, values)) = &self.saving {
            let held = view
                .functions
                .iter()
                .find(|f| f.slug == *function)
                .and_then(|f| f.cases.iter().find(|c| c.id == *case));
            if held.is_none_or(|c| c.values == *values) {
                self.saving = None;
            }
        }
        if view.activity.is_none() {
            self.live = None;
        }
        self.defaults();
        self.touch();
    }
}

impl FunctionsModels {
    pub fn apply(&mut self, tell: &FunctionsTell) {
        self.0.entry(tell.pr().clone()).or_default().apply(tell);
    }

    /// The answers still owed are lost with the connection: ask again once it is back.
    pub fn lost(&mut self) {
        for model in self.0.values_mut() {
            model.asked = false;
            model.pending = None;
            model.saving = None;
            model.queued_run = None;
            model.live = None;
            model.write_started = false;
            if let Some(modal) = model.driver.as_mut() {
                modal.loading = false;
            }
            model.touch();
        }
    }
}

/// Whether a region of the sub-tab has anything to compare: one of the resources it reads
/// moved, or the body was just spawned and has nothing drawn yet.
pub fn functions_changed(
    tabs: Res<ReviewTabs>,
    models: Res<FunctionsModels>,
    model: Res<Model>,
    fresh: Query<(), Added<FunctionsBody>>,
) -> bool {
    tabs.is_changed() || models.is_changed() || model.is_changed() || !fresh.is_empty()
}

/// The model when what it shows is about `head`: the tab strip never shows work of an older
/// head, even while the Tests section is not the one shown and nothing asked again.
pub fn of_head<'a>(
    model: Option<&'a FunctionsModel>,
    head: Option<&str>,
) -> Option<&'a FunctionsModel> {
    model.filter(|m| match (&m.view, head) {
        (Some(view), Some(head)) => view.head == head,
        _ => true,
    })
}

/// Why the buttons that run function cases are off, in the order that matters most; none when
/// they work. Window-only: the daemon refuses a busy review on its own.
pub fn run_disabled_reason(
    functions: Option<&FunctionsModel>,
    tests: Option<&TestsModel>,
) -> Option<&'static str> {
    let set_up = tests
        .and_then(|t| t.view.as_ref())
        .and_then(|v| v.plan.as_ref())
        .is_some_and(|p| p.ready);
    if !set_up {
        return Some("Set up the tests first");
    }
    if tests.is_some_and(TestsModel::tests_busy) {
        return Some("The tests are busy");
    }
    if functions.is_some_and(FunctionsModel::busy) {
        return Some("Function cases are running");
    }
    None
}

/// Asks for the functions the first time a ready, live tab shows its Tests section, when the
/// head moved, after a connection came back, and after a refused edit. A tab that closed forgets
/// its model. Changes are written around change detection and announced only when something was
/// written, so a settled tab wakes nothing.
pub fn load_functions(
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut functions: ResMut<FunctionsModels>,
    mut asks: ResMut<Asks>,
) {
    let mut wrote = false;
    let models = functions.bypass_change_detection();
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
            if tab.ui.section != ReviewSection::Tests {
                continue;
            }
            let head = &ready.view.review.head_sha;
            let m = models.0.entry(pr.clone()).or_default();
            if m.asked && m.asked_head.as_deref() == Some(head.as_str()) {
                continue;
            }
            m.asked = true;
            m.asked_head = Some(head.clone());
            m.touch();
            asks.send(Ask::GetFunctions(pr.clone()));
            wrote = true;
        }
    }
    if wrote {
        functions.set_changed();
    }
}

/// Sends the run that waited for its save, or says it cannot go.
pub fn send_queued(
    mut functions: ResMut<FunctionsModels>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let mut done = false;
    let models = functions.bypass_change_detection();
    for (pr, m) in &mut models.0 {
        if m.queued_run.is_none() || m.saving.is_some() {
            continue;
        }
        match m.release_queued(pr) {
            Queued::Nothing => {}
            Queued::Run(ask) => {
                asks.send(ask);
                done = true;
            }
            Queued::Dropped => {
                toasts.0.push(Toast {
                    text: "The case changed; run it again".into(),
                    warning: true,
                    until: time.elapsed_secs_f64() + TOAST_SECS,
                });
                done = true;
            }
        }
    }
    if done {
        functions.set_changed();
    }
}

/// Copies whether function cases are busy into each tab's tests model, which the Auto Tests run
/// buttons read, and wakes the tests' regions only when it changed.
pub fn mirror_busy(functions: Res<FunctionsModels>, mut tests: ResMut<TestsModels>) {
    let mut wrote = false;
    let models = tests.bypass_change_detection();
    for (pr, m) in &mut models.0 {
        let busy = functions.0.get(pr).is_some_and(FunctionsModel::busy);
        if m.functions_busy != busy {
            m.functions_busy = busy;
            m.touch();
            wrote = true;
        }
    }
    if wrote {
        tests.set_changed();
    }
}

#[cfg(test)]
mod tests {
    use clusia_core::PrRef;
    use clusia_core::functions::{
        CaseInput, CaseResult, CaseScope, CaseStatus, FunctionsActivity, FunctionsView, Support,
        function_slug,
    };
    use clusia_core::tests::TestsPhase;

    use super::*;
    use crate::bridge::{Ask, FunctionsAskKind, FunctionsTell, Tell};
    use crate::fixture::{self, DemoFunctions};
    use crate::review_state::ReviewSection;
    use crate::testing::{self, NOW};

    fn pr() -> PrRef {
        fixture::demo_pr()
    }

    fn mockup() -> FunctionsView {
        fixture::demo_functions(NOW, DemoFunctions::Mockup)
    }

    fn loaded(view: FunctionsView) -> FunctionsTell {
        FunctionsTell::Loaded {
            pr: pr(),
            view: Box::new(view),
        }
    }

    fn model() -> FunctionsModel {
        let mut m = FunctionsModel::default();
        m.apply(&loaded(mockup()));
        m
    }

    fn refresh_slug() -> String {
        function_slug("store/refresh.rs", "TokenStore::refresh")
    }

    #[test]
    fn the_first_ready_function_and_its_failed_case_are_open() {
        let m = model();
        assert_eq!(m.selected.as_deref(), Some(refresh_slug().as_str()));
        assert_eq!(
            m.open_case.as_deref(),
            Some("a1"),
            "the failed one, not the first passed"
        );
        assert_eq!(m.selected_view().unwrap().qualified, "TokenStore::refresh");
        assert_eq!(m.open_view().unwrap().id, "a1");
    }

    #[test]
    fn without_a_ready_function_the_first_is_selected_and_its_first_case_opens() {
        let mut view = mockup();
        for f in &mut view.functions {
            f.support = Support::NoCommand;
        }
        let mut m = FunctionsModel::default();
        m.apply(&loaded(view.clone()));
        assert_eq!(m.selected.as_deref(), Some(view.functions[0].slug.as_str()));
        assert_eq!(m.open_case.as_deref(), Some("a1"));
    }

    #[test]
    fn a_new_view_keeps_the_selection_and_the_open_case_while_they_exist() {
        let mut m = model();
        let backoff = function_slug("http/retry.rs", "backoff_delay");
        m.select(&backoff);
        assert_eq!(
            m.open_case.as_deref(),
            Some("a1"),
            "no failed case: the first"
        );
        m.open("a3");
        m.apply(&loaded(mockup()));
        assert_eq!(m.selected.as_deref(), Some(backoff.as_str()));
        assert_eq!(m.open_case.as_deref(), Some("a3"));
        let mut gone = mockup();
        gone.functions.retain(|f| f.qualified != "backoff_delay");
        m.apply(&loaded(gone));
        assert_eq!(
            m.selected.as_deref(),
            Some(refresh_slug().as_str()),
            "back to the default"
        );
    }

    #[test]
    fn only_a_result_of_these_values_and_this_head_is_the_cases() {
        let mut m = model();
        let f = m.selected_view().unwrap().clone();
        let a2 = f.cases[1].clone();
        assert!(m.current_result(&a2).is_some());
        assert_eq!(m.counts(&f), (3, 1));

        let mut other_values = mockup();
        other_values.functions[0].cases[1].hash = "changed".into();
        m.apply(&loaded(other_values));
        let a2 = m.selected_view().unwrap().cases[1].clone();
        assert!(
            m.current_result(&a2).is_none(),
            "a result of other values shows not run"
        );
        assert_eq!(m.counts(&m.selected_view().unwrap().clone()), (2, 1));

        let mut older = mockup();
        older.head = "3a9c1e7".into();
        m.apply(&loaded(older));
        let f = m.selected_view().unwrap().clone();
        assert!(
            f.cases.iter().all(|c| m.current_result(c).is_none()),
            "every result is of another head"
        );
        assert_eq!(m.counts(&f), (0, 0));
    }

    #[test]
    fn errors_timeouts_and_stops_count_as_failed() {
        let mut view = mockup();
        for (i, status) in [CaseStatus::Error, CaseStatus::TimedOut, CaseStatus::Stopped]
            .into_iter()
            .enumerate()
        {
            view.functions[0].cases[i + 1]
                .result
                .as_mut()
                .unwrap()
                .status = status;
        }
        let mut m = FunctionsModel::default();
        m.apply(&loaded(view));
        assert_eq!(m.counts(&m.selected_view().unwrap().clone()), (0, 4));
    }

    #[test]
    fn the_runnable_total_counts_the_cases_of_ready_functions() {
        let mut m = model();
        assert_eq!(m.runnable_total(), 14);
        let mut view = mockup();
        view.functions[3].support = Support::Stale {
            reason: "The signature changed since these cases were written".into(),
        };
        m.apply(&loaded(view));
        assert_eq!(m.runnable_total(), 12);
    }

    #[test]
    fn a_value_that_fails_its_check_is_not_sent() {
        let mut m = model();
        let slug = refresh_slug();
        assert_eq!(
            m.commit_field(&pr(), &slug, "a1", "endpoint_status", "five hundred"),
            None
        );
        assert_eq!(
            m.field_errors[&(
                slug.clone(),
                "a1".to_string(),
                "endpoint_status".to_string()
            )],
            "Enter a whole number"
        );
        assert_eq!(
            m.commit_field(&pr(), &slug, "a1", "endpoint_body", "{nope"),
            None
        );
        assert_eq!(
            m.field_errors[&(slug.clone(), "a1".to_string(), "endpoint_body".to_string())],
            "Enter valid JSON"
        );
        assert!(m.saving.is_none() && m.pending.is_none());
    }

    #[test]
    fn a_committed_value_saves_the_cases_whole_values() {
        let mut m = model();
        let slug = refresh_slug();
        let ask = m
            .commit_field(&pr(), &slug, "a1", "endpoint_status", "503")
            .unwrap();
        let mut values = m.open_view().unwrap().values.clone();
        values.insert("endpoint_status".into(), "503".into());
        assert_eq!(
            ask,
            Ask::SaveCase {
                pr: pr(),
                function: slug.clone(),
                case: CaseInput {
                    id: Some("a1".into()),
                    name: "Endpoint fails, old token kept".into(),
                    values: values.clone(),
                },
            }
        );
        assert_eq!(m.saving, Some((slug.clone(), "a1".into(), values.clone())));
        assert_eq!(m.pending, Some(FunctionsAskKind::Save));
        // The same value again, and the saved value, send nothing.
        assert_eq!(
            m.commit_field(&pr(), &slug, "a1", "endpoint_status", "503"),
            None
        );
        assert_eq!(
            m.commit_field(&pr(), &slug, "a1", "endpoint_latency", "200 ms"),
            None
        );
    }

    #[test]
    fn a_clean_run_sends_the_functions_hash_and_nothing_is_saved() {
        let mut m = model();
        let slug = refresh_slug();
        let shown = m.open_view().unwrap().values.clone();
        let hash = m.selected_view().unwrap().hash.clone();
        let ask = m.click_run(&pr(), &slug, "a1", shown).unwrap();
        assert_eq!(
            ask,
            Ask::RunCases {
                pr: pr(),
                scope: CaseScope::Case {
                    function: slug,
                    case: "a1".into()
                },
                seen: hash,
            }
        );
        assert_eq!(m.pending, Some(FunctionsAskKind::Run));
        assert!(m.saving.is_none() && m.queued_run.is_none());
    }

    #[test]
    fn run_after_an_unsaved_edit_saves_then_runs_with_the_new_hash() {
        let mut m = model();
        let slug = refresh_slug();
        let mut shown = m.open_view().unwrap().values.clone();
        shown.insert("endpoint_status".into(), "503".into());

        // Enter saves the value; the click while that save is in flight sends nothing more
        // and queues the run.
        let first = m.commit_field(&pr(), &slug, "a1", "endpoint_status", "503");
        assert!(matches!(first, Some(Ask::SaveCase { .. })), "{first:?}");
        assert!(!m.busy(), "a save in flight is not busy");
        assert_eq!(
            m.click_run(&pr(), &slug, "a1", shown.clone()),
            None,
            "the save in flight carries these values"
        );
        assert_eq!(
            m.queued_run,
            Some(QueuedRun {
                function: slug.clone(),
                case: "a1".into(),
                values: shown.clone()
            })
        );
        assert_eq!(
            m.release_queued(&pr()),
            Queued::Nothing,
            "the save has not landed"
        );

        // The save's reply carries a new hash and the new value.
        let mut view = mockup();
        view.functions[0].cases[0].values = shown.clone();
        view.functions[0].cases[0].edited = true;
        view.functions[0].hash = "new-function-hash".into();
        m.apply(&loaded(view));
        assert!(m.saving.is_none());
        assert_eq!(
            m.release_queued(&pr()),
            Queued::Run(Ask::RunCases {
                pr: pr(),
                scope: CaseScope::Case {
                    function: slug,
                    case: "a1".into()
                },
                seen: "new-function-hash".into(),
            })
        );
        assert!(m.queued_run.is_none());
        assert_eq!(m.pending, Some(FunctionsAskKind::Run));
    }

    #[test]
    fn a_queued_run_whose_values_did_not_land_is_dropped() {
        let mut m = model();
        let slug = refresh_slug();
        let mut shown = m.open_view().unwrap().values.clone();
        shown.insert("endpoint_status".into(), "503".into());
        m.click_run(&pr(), &slug, "a1", shown).unwrap();
        // The reply holds other values: something else changed the case.
        let mut view = mockup();
        view.functions[0].cases[0]
            .values
            .insert("endpoint_status".into(), "418".into());
        m.apply(&loaded(view));
        assert!(
            m.saving.is_some(),
            "still waiting: these are not the saved values"
        );
        m.apply(&FunctionsTell::Refused {
            pr: pr(),
            ask: FunctionsAskKind::Save,
            message: "No case a1 in TokenStore::refresh".into(),
        });
        assert!(
            m.saving.is_none() && m.queued_run.is_none(),
            "a refused save drops the queue"
        );
        assert_eq!(m.release_queued(&pr()), Queued::Nothing);
    }

    #[test]
    fn a_queued_run_is_dropped_when_the_case_vanished() {
        let mut m = model();
        let slug = refresh_slug();
        let mut shown = m.open_view().unwrap().values.clone();
        shown.insert("endpoint_status".into(), "503".into());
        m.click_run(&pr(), &slug, "a1", shown).unwrap();
        let mut view = mockup();
        view.functions[0].cases.remove(0);
        m.apply(&loaded(view));
        assert_eq!(m.release_queued(&pr()), Queued::Dropped);
    }

    #[test]
    fn a_function_that_is_not_ready_never_sends_a_run() {
        let mut view = mockup();
        view.functions[0].support = Support::Stale {
            reason: "The signature changed since these cases were written".into(),
        };
        let mut m = FunctionsModel::default();
        m.apply(&loaded(view));
        let slug = refresh_slug();
        let shown = m.selected_view().unwrap().cases[0].values.clone();
        assert_eq!(m.click_run(&pr(), &slug, "a1", shown), None);
    }

    #[test]
    fn an_acked_write_keeps_pending_until_the_activity_ends() {
        let mut m = model();
        m.sent(FunctionsAskKind::Write);
        // An older write's event, while no activity has shown yet.
        m.apply(&loaded(mockup()));
        assert_eq!(m.pending, Some(FunctionsAskKind::Write));
        let mut writing = mockup();
        writing.activity = Some(FunctionsActivity::Writing {
            function: Some(refresh_slug()),
            ask: None,
        });
        m.apply(&loaded(writing));
        assert_eq!(m.pending, Some(FunctionsAskKind::Write));
        m.apply(&loaded(mockup()));
        assert_eq!(m.pending, None, "the activity ended");
        assert!(!m.write_started);
    }

    #[test]
    fn a_reconnect_frees_a_pending_write() {
        let mut models = FunctionsModels::default();
        models.apply(&loaded(mockup()));
        models
            .0
            .get_mut(&pr())
            .unwrap()
            .sent(FunctionsAskKind::Write);
        models.lost();
        let m = &models.0[&pr()];
        assert!(m.pending.is_none() && !m.asked && m.saving.is_none() && m.queued_run.is_none());
    }

    #[test]
    fn a_case_done_sets_one_result_in_place() {
        let mut m = model();
        let before = m.view.clone().unwrap();
        let version = m.version;
        let slug = function_slug("store/refresh.rs", "TokenStore::exchange");
        let result = CaseResult {
            case: "a2".into(),
            head: before.head.clone(),
            hash: before.functions[1].cases[1].hash.clone(),
            status: CaseStatus::Failed,
            returned: Some("Err(RefreshError::Http(500))".into()),
            returned_expected: None,
            error: None,
            state: Vec::new(),
            output: String::new(),
            took_ms: 77,
            runner: clusia_core::tests::RunnerKind::Container,
            exit_code: Some(1),
            message: None,
            at: NOW,
        };
        m.apply(&FunctionsTell::CaseDone {
            pr: pr(),
            run: "run-1".into(),
            function: slug,
            result: Box::new(result.clone()),
        });
        let after = m.view.clone().unwrap();
        assert_eq!(after.functions[1].cases[1].result, Some(result));
        let mut expected = before;
        expected.functions[1].cases[1].result = after.functions[1].cases[1].result.clone();
        assert_eq!(after, expected, "nothing else moved");
        assert!(m.version > version);
        // An unknown function or case is ignored.
        let v = m.version;
        m.apply(&FunctionsTell::CaseDone {
            pr: pr(),
            run: "run-1".into(),
            function: "nope-0000000000".into(),
            result: Box::new(after.functions[1].cases[1].result.clone().unwrap()),
        });
        assert_eq!(m.version, v);
    }

    #[test]
    fn a_refused_save_reads_the_view_again_and_a_refused_driver_closes_the_modal() {
        let mut m = model();
        m.asked = true;
        m.sent(FunctionsAskKind::Reset);
        m.apply(&FunctionsTell::Refused {
            pr: pr(),
            ask: FunctionsAskKind::Reset,
            message: "x".into(),
        });
        assert!(!m.asked && m.pending.is_none());
        let ask = m.open_driver(&pr(), &refresh_slug());
        assert_eq!(
            ask,
            Ask::GetDriver {
                pr: pr(),
                function: refresh_slug()
            }
        );
        assert!(m.driver.as_ref().unwrap().loading);
        m.apply(&FunctionsTell::Refused {
            pr: pr(),
            ask: FunctionsAskKind::Driver,
            message: "x".into(),
        });
        assert!(m.driver.is_none());
        m.open_driver(&pr(), &refresh_slug());
        m.apply(&FunctionsTell::Driver {
            pr: pr(),
            code: Box::new(fixture::demo_driver()),
        });
        let d = m.driver.as_ref().unwrap();
        assert!(!d.loading && d.code.is_some());
        m.close_driver();
        m.apply(&FunctionsTell::Driver {
            pr: pr(),
            code: Box::new(fixture::demo_driver()),
        });
        assert!(
            m.driver.is_none(),
            "a driver nobody waits for opens nothing"
        );
    }

    #[test]
    fn the_tests_badge_is_a_dot_while_cases_run_and_never_a_number() {
        let mut sections = vec![(ReviewSection::Tests, "Tests".to_string(), false)];
        let mut m = model();
        crate::screens::review::tests_tab::badge_tests(&mut sections, None, Some(&m));
        assert_eq!(sections[0].1, "Tests", "a failed case sets no number");
        let mut running = mockup();
        running.activity = Some(FunctionsActivity::Writing {
            function: None,
            ask: None,
        });
        m.apply(&loaded(running));
        crate::screens::review::tests_tab::badge_tests(&mut sections, None, Some(&m));
        assert_eq!(
            sections[0].1,
            format!("Tests {}", crate::screens::review::activity::MARK)
        );
    }

    #[test]
    fn the_run_buttons_say_why_they_are_off() {
        let mut f = model();
        let mut t = crate::screens::review::tests_tab::TestsModel::default();
        let tests_view = |phase, ready| {
            let mut view = clusia_core::tests::TestsView::new("h");
            view.phase = phase;
            view.plan = Some(testing::tests_plan(Vec::new(), ready));
            view
        };
        t.view = Some(tests_view(TestsPhase::NeedsSetup, false));
        assert_eq!(
            run_disabled_reason(Some(&f), Some(&t)),
            Some("Set up the tests first")
        );
        t.view = Some(tests_view(TestsPhase::Ready, true));
        assert_eq!(run_disabled_reason(Some(&f), Some(&t)), None);
        t.view = Some(tests_view(TestsPhase::Running { run: "r".into() }, true));
        assert_eq!(
            run_disabled_reason(Some(&f), Some(&t)),
            Some("The tests are busy")
        );
        t.view = Some(tests_view(TestsPhase::Ready, true));
        f.sent(FunctionsAskKind::Run);
        assert_eq!(
            run_disabled_reason(Some(&f), Some(&t)),
            Some("Function cases are running")
        );
        t.functions_busy = true;
        assert!(t.busy(), "the Auto Tests run buttons follow busy()");
    }

    // The app-level tests: loading and the plugin's systems.

    fn app_on_tests() -> (App, PrRef) {
        let mut app = testing::demo_review_app();
        let pr = fixture::demo_pr();
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        (app, pr)
    }

    fn gets(asks: &[Ask], pr: &PrRef) -> usize {
        asks.iter()
            .filter(|a| matches!(a, Ask::GetFunctions(p) if p == pr))
            .count()
    }

    #[test]
    fn functions_are_asked_once_per_head_and_again_after_a_reconnect_or_a_refusal() {
        let (mut app, pr) = app_on_tests();
        assert_eq!(
            gets(&testing::recorded(&mut app), &pr),
            1,
            "the first time the section shows"
        );
        testing::settle(&mut app);
        assert_eq!(gets(&testing::recorded(&mut app), &pr), 0, "only once");

        testing::tell(
            &mut app,
            Tell::Functions(FunctionsTell::Refused {
                pr: pr.clone(),
                ask: FunctionsAskKind::Save,
                message: "x".into(),
            }),
        );
        testing::settle(&mut app);
        assert_eq!(
            gets(&testing::recorded(&mut app), &pr),
            1,
            "a refused save reads again"
        );

        testing::tell(&mut app, Tell::Lost("gone".into()));
        testing::settle(&mut app);
        testing::tell(&mut app, Tell::Snapshot(Box::new(fixture::demo(NOW))));
        testing::settle(&mut app);
        assert_eq!(
            gets(&testing::recorded(&mut app), &pr),
            1,
            "after the connection came back"
        );

        let mut app = testing::demo_review_app();
        let pr = fixture::demo_pr();
        testing::set_section(&mut app, &pr, ReviewSection::Diff);
        assert_eq!(
            gets(&testing::recorded(&mut app), &pr),
            0,
            "not before the Tests section"
        );
    }

    #[test]
    fn a_failed_case_turn_toasts_once() {
        let (mut app, pr) = app_on_tests();
        let mut view = mockup();
        view.failed = Some("Claude's cases are not valid: a case holds a control character".into());
        for _ in 0..2 {
            testing::tell(
                &mut app,
                Tell::Functions(FunctionsTell::Loaded {
                    pr: pr.clone(),
                    view: Box::new(view.clone()),
                }),
            );
        }
        let toasts = &app.world().resource::<crate::bridge::Toasts>().0;
        let texts: Vec<&str> = toasts.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(
            texts
                .iter()
                .filter(|t| t.starts_with("Claude's cases are not valid"))
                .count(),
            1,
            "{texts:?}"
        );
        assert!(
            toasts
                .iter()
                .all(|t| t.warning || !t.text.starts_with("Claude's cases"))
        );
    }

    #[test]
    fn a_queued_run_goes_out_when_its_save_lands_and_toasts_when_it_cannot() {
        let (mut app, pr) = app_on_tests();
        testing::recorded(&mut app);
        testing::tell(
            &mut app,
            Tell::Functions(FunctionsTell::Loaded {
                pr: pr.clone(),
                view: Box::new(mockup()),
            }),
        );
        let slug = refresh_slug();
        let mut shown = {
            let m = &app.world().resource::<FunctionsModels>().0[&pr];
            m.open_view().unwrap().values.clone()
        };
        shown.insert("endpoint_status".into(), "503".into());
        let ask = app
            .world_mut()
            .resource_mut::<FunctionsModels>()
            .0
            .get_mut(&pr)
            .unwrap()
            .click_run(&pr, &slug, "a1", shown.clone());
        assert!(matches!(ask, Some(Ask::SaveCase { .. })));
        let mut view = mockup();
        view.functions[0].cases[0].values = shown;
        view.functions[0].hash = "new-hash".into();
        testing::tell(
            &mut app,
            Tell::Functions(FunctionsTell::Loaded {
                pr: pr.clone(),
                view: Box::new(view),
            }),
        );
        testing::settle(&mut app);
        let sent = testing::recorded(&mut app);
        assert!(
            sent.iter()
                .any(|a| matches!(a, Ask::RunCases { seen, .. } if seen == "new-hash")),
            "{sent:?}"
        );
    }

    #[test]
    fn the_auto_run_buttons_are_off_while_function_cases_run() {
        use crate::screens::review::tests_tab::top::RunButton;
        let (mut app, pr) = app_on_tests();
        let mut ready = testing::tests_view(&app, &pr, TestsPhase::Ready);
        ready.plan = Some(testing::tests_plan(Vec::new(), true));
        testing::open_tests(&mut app, &pr, ready);
        let run_all_buttons = |app: &mut App| {
            app.world_mut()
                .query::<&RunButton>()
                .iter(app.world())
                .filter(|b| b.scope == clusia_core::tests::TestScope::All)
                .count()
        };
        assert_eq!(run_all_buttons(&mut app), 1);
        let mut running = mockup();
        running.activity = Some(FunctionsActivity::Writing {
            function: None,
            ask: None,
        });
        testing::tell(
            &mut app,
            Tell::Functions(FunctionsTell::Loaded {
                pr: pr.clone(),
                view: Box::new(running),
            }),
        );
        testing::settle(&mut app);
        assert!(
            app.world()
                .resource::<crate::screens::review::tests_tab::TestsModels>()
                .0[&pr]
                .functions_busy
        );
        assert!(testing::shows(&mut app, "Function cases are running"));
        assert_eq!(
            run_all_buttons(&mut app),
            0,
            "a disabled button carries no marker, so no click reaches it"
        );
    }
}
