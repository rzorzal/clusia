//! What the review's checks said (mockups `Security.png`, `Audits.png`): the per-review model
//! the bridge fills, the whole-tab states both tabs share, and the buttons on a finding. The
//! Security tab is `security`; the Audits tab is `audits`.
//!
//! A finding is the agent's proposal, so everything the window shows of it is made printable
//! and drawn as plain text. Nothing here sends a finding to the draft without a click.

use std::collections::{BTreeSet, HashMap};

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, observe};
use clusia_core::PrRef;
use clusia_core::checks::{CheckKind, CheckResult, Finding, Severity};
use clusia_core::printable::{printable, printable_lines};
use clusia_protocol::CheckState;

use crate::bridge::{Ask, Asks, ChecksTell, Connection, Model, Toasts};
use crate::fonts::UiFonts;
use crate::review_state::{EditTarget, Editor, ReviewSection, ReviewTabs, Tab};
use crate::screens::review::ReviewSystems;
use crate::screens::review::agent::suggestion::show_file;
use crate::screens::review::diff::unsent;
use crate::screens::review::editor::{not_sent, read_only_reason};
use crate::screens::review::shell::on_harness;
use crate::ui::composer::ComposerMode;
use crate::ui::kit::{Variant, button};

pub mod audits;
pub mod security;

/// One kind's state and what its last run found.
#[derive(Debug, Clone, PartialEq)]
pub struct KindModel {
    pub state: CheckState,
    pub result: Option<CheckResult>,
    /// Findings streamed so far by the run in progress.
    pub found: u32,
}

impl Default for KindModel {
    fn default() -> Self {
        Self {
            state: CheckState::NotRun,
            result: None,
            found: 0,
        }
    }
}

/// One review's checks.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ChecksModel {
    pub security: KindModel,
    pub audit: KindModel,
    /// Finding ids that are in the draft.
    pub accepted: BTreeSet<String>,
    /// Finding ids the user dismissed: never shown again.
    pub dismissed: BTreeSet<String>,
    /// Ids whose accept or dismiss waits for the daemon.
    pub pending: BTreeSet<String>,
    /// `GetChecks` was asked for.
    pub asked: bool,
    /// The daemon's answer arrived.
    pub loaded: bool,
}

fn rank(severity: Option<&Severity>) -> u8 {
    match severity {
        Some(Severity::High) => 0,
        Some(Severity::Medium) => 1,
        Some(Severity::Low) => 2,
        None => 3,
    }
}

fn active(state: &CheckState) -> bool {
    matches!(state, CheckState::Waiting | CheckState::Running { .. })
}

impl ChecksModel {
    pub fn kind(&self, kind: CheckKind) -> &KindModel {
        match kind {
            CheckKind::Security => &self.security,
            CheckKind::Audit => &self.audit,
        }
    }

    pub fn kind_mut(&mut self, kind: CheckKind) -> &mut KindModel {
        match kind {
            CheckKind::Security => &mut self.security,
            CheckKind::Audit => &mut self.audit,
        }
    }

    pub fn apply(&mut self, tell: &ChecksTell) {
        match tell {
            ChecksTell::Loaded {
                results,
                states,
                accepted,
                dismissed,
                ..
            } => {
                for kind in [CheckKind::Security, CheckKind::Audit] {
                    let k = self.kind_mut(kind);
                    k.result = results.iter().find(|r| r.kind == kind).cloned();
                    k.state = states
                        .iter()
                        .find(|s| s.kind == kind)
                        .map_or(CheckState::NotRun, |s| s.state.clone());
                    k.found = 0;
                }
                self.accepted = accepted.iter().cloned().collect();
                self.dismissed = dismissed.iter().cloned().collect();
                self.pending.clear();
                self.asked = true;
                self.loaded = true;
            }
            ChecksTell::State { kind, state, .. } => {
                let k = self.kind_mut(*kind);
                // A run that starts has nothing to show from the one before it.
                if active(state) && !active(&k.state) {
                    k.result = None;
                    k.found = 0;
                }
                k.state = state.clone();
                // A stop or a failure may have left the daemon's old result in place: read it.
                if matches!(state, CheckState::NotRun | CheckState::Failed { .. }) {
                    self.asked = false;
                }
            }
            ChecksTell::Finding { kind, .. } => {
                let k = self.kind_mut(*kind);
                if active(&k.state) {
                    k.found += 1;
                }
            }
            ChecksTell::Pass { .. } => {}
            ChecksTell::Done { kind, result, .. } => {
                let k = self.kind_mut(*kind);
                k.result = Some(result.clone());
                k.state = CheckState::Done;
                k.found = 0;
            }
            ChecksTell::Settled { id, accepted, .. } => {
                self.pending.remove(id);
                if *accepted {
                    self.accepted.insert(id.clone());
                    self.dismissed.remove(id);
                } else {
                    self.dismissed.insert(id.clone());
                }
            }
            ChecksTell::Refused { id, .. } => {
                self.pending.remove(id);
            }
        }
    }

    /// The finding `id` of either kind's result.
    pub fn find(&self, id: &str) -> Option<&Finding> {
        [&self.security, &self.audit]
            .into_iter()
            .filter_map(|k| k.result.as_ref())
            .flat_map(|r| r.findings.iter())
            .find(|f| f.id == id)
    }

    /// The findings of `kind` that were not dismissed; Security's come high to low.
    pub fn findings(&self, kind: CheckKind) -> Vec<&Finding> {
        let Some(result) = &self.kind(kind).result else {
            return Vec::new();
        };
        let mut shown: Vec<&Finding> = result
            .findings
            .iter()
            .filter(|f| !self.dismissed.contains(&f.id))
            .collect();
        shown.sort_by_key(|f| rank(f.severity.as_ref()));
        shown
    }

    /// The findings still waiting for the human: shown, and neither accepted nor dismissed. The
    /// tab badge, the header's count and chips, and the audit areas' marks count these.
    pub fn open(&self, kind: CheckKind) -> Vec<&Finding> {
        self.findings(kind)
            .into_iter()
            .filter(|f| !self.accepted.contains(&f.id))
            .collect()
    }

    /// The tab label's mark: `●` while the check waits or runs, else the open findings of a
    /// result the tab lists (done, or stale). A failed or stopped check shows its notice.
    pub fn badge(&self, kind: CheckKind) -> Option<String> {
        let state = &self.kind(kind).state;
        if active(state) {
            return Some("●".to_string());
        }
        if !matches!(state, CheckState::Done | CheckState::Stale) {
            return None;
        }
        let n = self.open(kind).len();
        (n > 0).then(|| n.to_string())
    }

    /// Audit findings the user has not accepted or dismissed yet.
    pub fn waiting_ok(&self) -> usize {
        self.open(CheckKind::Audit).len()
    }
}

/// Every review's checks.
#[derive(Resource, Debug, Default)]
pub struct Checks(pub HashMap<PrRef, ChecksModel>);

impl Checks {
    pub fn apply(&mut self, tell: &ChecksTell) {
        self.0.entry(tell.pr().clone()).or_default().apply(tell);
    }
}

/// `Security ●` / `Security 3` on the section tabs (the tab strip splits the label back into
/// the name and the mark).
pub fn badge_sections(sections: &mut [(ReviewSection, String, bool)], model: Option<&ChecksModel>) {
    let Some(model) = model else { return };
    for (section, label, _) in sections {
        let kind = match section {
            ReviewSection::Security => CheckKind::Security,
            ReviewSection::Audits => CheckKind::Audit,
            _ => continue,
        };
        if let Some(mark) = model.badge(kind) {
            *label = format!("{} {mark}", section.label());
        }
    }
}

/// A finding accepted or dismissed closes the editor that was editing it.
pub(crate) fn close_edited(tabs: &mut ReviewTabs, tell: &ChecksTell) {
    let ChecksTell::Settled { pr, id, .. } = tell else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(pr)
        && matches!(
            tab.ui.editor.as_ref().map(|e| &e.target),
            Some(EditTarget::Finding(editing)) if editing == id
        )
    {
        tab.ui.editor = None;
    }
}

/// The comment **Add to draft** adds for `f`: its `comment`, behind `file:line: ` when it has no
/// place in the diff (a general comment must say where it is about, as the daemon writes it).
pub fn proposed_comment(f: &Finding) -> String {
    if f.anchored {
        return f.comment.clone();
    }
    // The daemon's own prefix, character for character: `file:line`, `file:start-end`, `file`.
    let place = match (f.line, f.start_line, f.end_line) {
        (Some(line), _, _) => format!("{}:{line}", f.file),
        (None, Some(start), Some(end)) => format!("{}:{start}-{end}", f.file),
        _ => f.file.clone(),
    };
    format!("{place}: {}", f.comment)
}

/// `file:line` or `file:start–end` of a finding.
pub fn place_of(f: &Finding) -> String {
    match (f.line, f.start_line, f.end_line) {
        (Some(line), _, _) => format!("{}:{line}", f.file),
        (None, Some(start), Some(end)) if end != start => format!("{}:{start}–{end}", f.file),
        (None, Some(line), _) => format!("{}:{line}", f.file),
        _ => f.file.clone(),
    }
}

/// What a check's button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckAction {
    Run,
    Stop,
    CheckAgain,
    TryAgain,
}

impl CheckAction {
    pub fn label(self, kind: CheckKind) -> &'static str {
        match (self, kind) {
            (CheckAction::Run, CheckKind::Security) => "Run security check",
            (CheckAction::Run, CheckKind::Audit) => "Run audit",
            (CheckAction::Stop, _) => "Stop",
            (CheckAction::CheckAgain, _) => "Check again",
            (CheckAction::TryAgain, _) => "Try again",
        }
    }

    pub fn variant(self) -> Variant {
        match self {
            CheckAction::Run => Variant::Primary,
            _ => Variant::Secondary,
        }
    }
}

/// What a tab says when its kind has no result to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub line: String,
    pub action: Option<CheckAction>,
    /// Offer **Set up a harness**.
    pub setup: bool,
}

/// The tab's state when there is nothing to list: no harness, not run, waiting, running or
/// failed. `None` when the kind has a result (a run in progress or a failure comes first).
pub fn notice_of(kind: CheckKind, k: &KindModel, harness: bool) -> Option<Notice> {
    let (name, working, failed, not_run) = match kind {
        CheckKind::Security => (
            "Security check",
            "Checking security…",
            "The security check failed",
            "Claude Code has not checked this change for security problems.",
        ),
        CheckKind::Audit => (
            "Audit",
            "Auditing the change…",
            "The audit failed",
            "Claude Code has not audited this change yet.",
        ),
    };
    let notice = |title: &str, line: String, action, setup| {
        Some(Notice {
            title: title.to_string(),
            line,
            action,
            setup,
        })
    };
    match &k.state {
        CheckState::Waiting => notice(working, "Waiting for another check".into(), None, false),
        CheckState::Running { activity } => {
            let mut line = activity
                .as_deref()
                .map(printable)
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "Claude Code is reading the change…".to_string());
            if k.found > 0 {
                line.push_str(&format!(" · {} found so far", k.found));
            }
            notice(working, line, Some(CheckAction::Stop), false)
        }
        CheckState::Failed { message } => notice(
            failed,
            printable(message),
            Some(CheckAction::TryAgain),
            false,
        ),
        _ if k.result.is_some() => None,
        _ if !harness => notice(
            name,
            "Set up a harness in Config › Harness".into(),
            None,
            true,
        ),
        _ => notice(name, not_run.into(), Some(CheckAction::Run), false),
    }
}

/// **Run security check**, **Check again**, **Try again** and **Stop**.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CheckButton {
    pub pr: PrRef,
    pub kind: CheckKind,
    pub action: CheckAction,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FindingAccept {
    pub pr: PrRef,
    pub id: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FindingEdit {
    pub pr: PrRef,
    pub id: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FindingDismiss {
    pub pr: PrRef,
    pub id: String,
}

/// **Show in diff**: the Diff on this file.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FindingShow {
    pub pr: PrRef,
    pub file: String,
}

/// **Accept**, **Edit first** and **Dismiss** for finding `id`.
pub fn finding_buttons(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    id: &str,
    accept_label: &str,
    dismiss: Variant,
) {
    p.spawn((
        button(fonts, accept_label, Variant::Primary),
        FindingAccept {
            pr: pr.clone(),
            id: id.to_string(),
        },
        observe(on_accept),
    ));
    p.spawn((
        button(fonts, "Edit first", Variant::Secondary),
        FindingEdit {
            pr: pr.clone(),
            id: id.to_string(),
        },
        observe(on_edit),
    ));
    p.spawn((
        button(fonts, "Dismiss", dismiss),
        FindingDismiss {
            pr: pr.clone(),
            id: id.to_string(),
        },
        observe(on_dismiss),
    ));
}

/// Only **Dismiss**: what a stale card offers.
pub fn dismiss_button(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    id: &str,
    variant: Variant,
) {
    p.spawn((
        button(fonts, "Dismiss", variant),
        FindingDismiss {
            pr: pr.clone(),
            id: id.to_string(),
        },
        observe(on_dismiss),
    ));
}

/// The **Show in diff** link of a card and the **Set up a harness** button are spawned by the
/// tabs; their observers are here.
pub(super) fn show_button(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, file: &str) {
    p.spawn((
        button(fonts, "Show in diff", Variant::Ghost),
        FindingShow {
            pr: pr.clone(),
            file: file.to_string(),
        },
        observe(on_show),
    ));
}

/// The check button of a tab's header.
pub(super) fn check_button(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    kind: CheckKind,
    action: CheckAction,
) {
    p.spawn((
        button(fonts, action.label(kind), action.variant()),
        CheckButton {
            pr: pr.clone(),
            kind,
            action,
        },
        observe(on_check_button),
    ));
}

/// The **Set up a harness** button.
pub(super) fn setup_button(p: &mut ChildSpawnerCommands, fonts: &UiFonts) {
    p.spawn((
        button(fonts, "Set up a harness", Variant::Secondary),
        observe(on_harness),
    ));
}

fn on_check_button(
    activate: On<Activate>,
    buttons: Query<&CheckButton>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    if b.action == CheckAction::Stop {
        asks.send(Ask::StopCheck {
            pr: b.pr.clone(),
            kind: b.kind,
        });
        return;
    }
    if let Some(reason) = read_only_reason(&tabs, &model, &b.pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    asks.send(Ask::RunCheck {
        pr: b.pr.clone(),
        kind: b.kind,
    });
}

fn on_accept(
    activate: On<Activate>,
    buttons: Query<&FindingAccept>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut checks: ResMut<Checks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &model, &b.pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    checks
        .0
        .entry(b.pr.clone())
        .or_default()
        .pending
        .insert(b.id.clone());
    asks.send(Ask::AcceptFinding {
        pr: b.pr.clone(),
        id: b.id.clone(),
        body: None,
    });
}

fn on_dismiss(
    activate: On<Activate>,
    buttons: Query<&FindingDismiss>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut checks: ResMut<Checks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &model, &b.pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    checks
        .0
        .entry(b.pr.clone())
        .or_default()
        .pending
        .insert(b.id.clone());
    asks.send(Ask::DismissFinding {
        pr: b.pr.clone(),
        id: b.id.clone(),
    });
}

/// Opens the shared comment editor under the card, with the proposed comment.
fn on_edit(
    activate: On<Activate>,
    buttons: Query<&FindingEdit>,
    mut tabs: ResMut<ReviewTabs>,
    checks: Res<Checks>,
    model: Res<Model>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Ok(FindingEdit { pr, id }) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &model, pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    let Some(comment) = checks
        .0
        .get(pr)
        .and_then(|m| m.find(id))
        .map(|f| printable_lines(&proposed_comment(f)))
    else {
        return;
    };
    let Some(tab) = tabs.0.get_mut(pr) else {
        return;
    };
    if unsent(tab.ui.editor.as_ref()) {
        return not_sent(
            &mut toasts,
            &time,
            "Finish or cancel your open comment first",
        );
    }
    tab.ui.editor = Some(Editor {
        target: EditTarget::Finding(id.clone()),
        text: comment,
        error: None,
        ticket: None,
        mode: ComposerMode::Write,
    });
}

fn on_show(activate: On<Activate>, buttons: Query<&FindingShow>, mut tabs: ResMut<ReviewTabs>) {
    let Ok(FindingShow { pr, file }) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(pr) {
        show_file(&mut tab.ui, file);
    }
}

pub struct ChecksPlugin;

impl Plugin for ChecksPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (load_checks, security::fill_security, audits::fill_audits)
                .chain()
                .after(ReviewSystems),
        );
    }
}

/// Asks for the checks of each ready tab once the connection is live, and forgets the checks of
/// closed tabs. A cached copy asks when the fresh one arrives.
fn load_checks(
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut checks: ResMut<Checks>,
    mut asks: ResMut<Asks>,
) {
    if checks.0.keys().any(|pr| !tabs.0.contains_key(pr)) {
        checks.0.retain(|pr, _| tabs.0.contains_key(pr));
    }
    if model.connection != Connection::Live {
        return;
    }
    for (pr, tab) in &tabs.0 {
        if !tab.ready().is_some_and(|r| r.cached_at.is_none()) {
            continue;
        }
        if checks.0.get(pr).is_some_and(|m| m.asked) {
            continue;
        }
        checks.0.entry(pr.clone()).or_default().asked = true;
        asks.send(Ask::GetChecks(pr.clone()));
    }
}

/// The editor of `tab` when it edits a finding, for the tab that draws it.
pub(super) fn finding_editor(tab: &Tab) -> Option<&Editor> {
    tab.ui
        .editor
        .as_ref()
        .filter(|e| matches!(e.target, EditTarget::Finding(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Ask, ChecksTell, Tell};
    use crate::fixture;
    use crate::review_state::{EditTarget, ReviewSection};
    use crate::screens::review::editor::{EditorArea, EditorSubmit};
    use crate::testing::{self, NOW};
    use clusia_core::PrRef;
    use clusia_core::checks::CheckKind;
    use clusia_protocol::CheckState;

    fn pr() -> PrRef {
        fixture::demo_pr()
    }

    fn loaded() -> ChecksModel {
        let (results, states) = fixture::demo_checks(NOW);
        let mut m = ChecksModel::default();
        m.apply(&ChecksTell::Loaded {
            pr: pr(),
            results,
            states,
            accepted: vec![],
            dismissed: vec![],
        });
        m
    }

    fn security_ids() -> Vec<String> {
        fixture::demo_checks(NOW)
            .0
            .into_iter()
            .find(|r| r.kind == CheckKind::Security)
            .unwrap()
            .findings
            .into_iter()
            .map(|f| f.id)
            .collect()
    }

    #[test]
    fn loaded_fills_both_kinds_and_the_id_sets() {
        let ids = security_ids();
        let (results, states) = fixture::demo_checks(NOW);
        let mut m = ChecksModel::default();
        m.apply(&ChecksTell::Loaded {
            pr: pr(),
            results,
            states,
            accepted: vec![ids[1].clone()],
            dismissed: vec![ids[2].clone()],
        });
        assert!(m.loaded && m.asked);
        assert_eq!(m.kind(CheckKind::Security).state, CheckState::Done);
        assert!(m.kind(CheckKind::Audit).result.is_some());
        assert!(m.accepted.contains(&ids[1]));
        let shown: Vec<&str> = m
            .findings(CheckKind::Security)
            .iter()
            .map(|f| f.id.as_str())
            .collect();
        assert_eq!(
            shown,
            [ids[0].as_str(), ids[1].as_str()],
            "dismissed is hidden"
        );
    }

    #[test]
    fn findings_are_ordered_by_severity() {
        let mut m = loaded();
        let result = m.security.result.as_mut().unwrap();
        result.findings.reverse();
        let titles: Vec<String> = m
            .findings(CheckKind::Security)
            .iter()
            .map(|f| f.title.clone())
            .collect();
        assert_eq!(
            titles,
            [
                "Refresh token written to the debug log",
                "No limit on refresh retries",
                "Token file created with default permissions"
            ]
        );
    }

    #[test]
    fn a_new_run_drops_the_old_result() {
        let mut m = loaded();
        m.apply(&ChecksTell::State {
            pr: pr(),
            kind: CheckKind::Security,
            state: CheckState::Waiting,
        });
        assert!(m.security.result.is_none());
        assert!(m.audit.result.is_some(), "the other kind is untouched");
        m.apply(&ChecksTell::State {
            pr: pr(),
            kind: CheckKind::Security,
            state: CheckState::Running { activity: None },
        });
        m.apply(&ChecksTell::Finding {
            pr: pr(),
            kind: CheckKind::Security,
            finding: fixture::demo_checks(NOW).0[0].findings[0].clone(),
        });
        assert_eq!(m.security.found, 1);
        m.apply(&ChecksTell::State {
            pr: pr(),
            kind: CheckKind::Security,
            state: CheckState::Running {
                activity: Some("Reading src/auth/store.rs…".into()),
            },
        });
        assert_eq!(m.security.found, 1, "running again keeps the count");
        let result = fixture::demo_checks(NOW).0.remove(0);
        m.apply(&ChecksTell::Done {
            pr: pr(),
            kind: CheckKind::Security,
            result: result.clone(),
        });
        assert_eq!(m.security.state, CheckState::Done);
        assert_eq!(m.security.result, Some(result));
        assert_eq!(m.security.found, 0);
    }

    #[test]
    fn stopping_and_failing_read_the_checks_again() {
        let mut m = loaded();
        for state in [
            CheckState::NotRun,
            CheckState::Failed {
                message: "timed out".into(),
            },
        ] {
            m.asked = true;
            m.apply(&ChecksTell::State {
                pr: pr(),
                kind: CheckKind::Audit,
                state,
            });
            assert!(!m.asked);
        }
    }

    #[test]
    fn settled_and_refused_move_the_id_sets() {
        let ids = security_ids();
        let mut m = loaded();
        m.pending.insert(ids[0].clone());
        m.pending.insert(ids[1].clone());
        m.apply(&ChecksTell::Settled {
            pr: pr(),
            id: ids[0].clone(),
            accepted: true,
        });
        m.apply(&ChecksTell::Refused {
            pr: pr(),
            id: ids[1].clone(),
        });
        assert!(m.accepted.contains(&ids[0]));
        assert!(m.pending.is_empty());
        m.apply(&ChecksTell::Settled {
            pr: pr(),
            id: ids[2].clone(),
            accepted: false,
        });
        assert!(m.dismissed.contains(&ids[2]));
        assert_eq!(m.findings(CheckKind::Security).len(), 2);
    }

    #[test]
    fn badges_follow_the_state_and_the_open_findings() {
        let ids = security_ids();
        let mut m = loaded();
        assert_eq!(m.badge(CheckKind::Security).as_deref(), Some("3"));
        m.apply(&ChecksTell::Settled {
            pr: pr(),
            id: ids[2].clone(),
            accepted: false,
        });
        assert_eq!(m.badge(CheckKind::Security).as_deref(), Some("2"));
        m.apply(&ChecksTell::Settled {
            pr: pr(),
            id: ids[0].clone(),
            accepted: true,
        });
        assert_eq!(
            m.badge(CheckKind::Security).as_deref(),
            Some("1"),
            "an accepted finding is no longer open"
        );
        assert_eq!(m.findings(CheckKind::Security).len(), 2, "its card stays");
        m.apply(&ChecksTell::State {
            pr: pr(),
            kind: CheckKind::Security,
            state: CheckState::Running { activity: None },
        });
        assert_eq!(m.badge(CheckKind::Security).as_deref(), Some("●"));
        assert_eq!(ChecksModel::default().badge(CheckKind::Audit), None);
        let mut sections = vec![
            (ReviewSection::Security, "Security".to_string(), false),
            (ReviewSection::Diff, "Diff 7".to_string(), true),
        ];
        badge_sections(&mut sections, Some(&m));
        assert_eq!(sections[0].1, "Security ●");
        assert_eq!(sections[1].1, "Diff 7");
    }

    #[test]
    fn waiting_ok_counts_open_audit_findings() {
        let mut m = loaded();
        assert_eq!(m.waiting_ok(), 3);
        let id = m.findings(CheckKind::Audit)[0].id.clone();
        m.apply(&ChecksTell::Settled {
            pr: pr(),
            id: id.clone(),
            accepted: true,
        });
        assert_eq!(m.waiting_ok(), 2);
        let other = m.findings(CheckKind::Audit)[1].id.clone();
        m.apply(&ChecksTell::Settled {
            pr: pr(),
            id: other,
            accepted: false,
        });
        assert_eq!(m.waiting_ok(), 1);
    }

    #[test]
    fn notices_cover_every_state_without_a_result() {
        let mut k = KindModel::default();
        let n = notice_of(CheckKind::Security, &k, false).unwrap();
        assert_eq!(n.line, "Set up a harness in Config › Harness");
        assert!(n.setup && n.action.is_none());
        let n = notice_of(CheckKind::Security, &k, true).unwrap();
        assert_eq!(n.action, Some(CheckAction::Run));
        assert_eq!(
            CheckAction::Run.label(CheckKind::Security),
            "Run security check"
        );
        assert_eq!(CheckAction::Run.label(CheckKind::Audit), "Run audit");
        k.state = CheckState::Waiting;
        assert_eq!(
            notice_of(CheckKind::Audit, &k, true).unwrap().line,
            "Waiting for another check"
        );
        k.state = CheckState::Running {
            activity: Some("Reading src/auth/store.rs…".into()),
        };
        k.found = 2;
        let n = notice_of(CheckKind::Security, &k, true).unwrap();
        assert_eq!(n.line, "Reading src/auth/store.rs… · 2 found so far");
        assert_eq!(n.action, Some(CheckAction::Stop));
        k.state = CheckState::Running {
            activity: Some("Run\u{1b}[31m this".into()),
        };
        assert!(
            !notice_of(CheckKind::Security, &k, true)
                .unwrap()
                .line
                .contains('\u{1b}'),
            "the activity is the agent's text"
        );
        k.state = CheckState::Failed {
            message: "The check took too long".into(),
        };
        let n = notice_of(CheckKind::Security, &k, true).unwrap();
        assert_eq!(n.line, "The check took too long");
        assert_eq!(n.action, Some(CheckAction::TryAgain));
        k.state = CheckState::Done;
        k.result = fixture::demo_checks(NOW).0.into_iter().next();
        assert_eq!(notice_of(CheckKind::Security, &k, true), None);
    }

    fn open_security(app: &mut App) -> PrRef {
        let pr = testing::open_ready(app, false);
        testing::open_checks(app, &pr);
        testing::set_section(app, &pr, ReviewSection::Security);
        pr
    }

    #[test]
    fn a_ready_tab_asks_for_its_checks_once() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        assert!(
            app.world().resource::<Checks>().0[&pr].asked,
            "asked while opening"
        );
        testing::settle(&mut app);
        let again = testing::recorded(&mut app);
        assert!(!again.contains(&Ask::GetChecks(pr.clone())), "only once");
        testing::tell(&mut app, Tell::Lost("gone".into()));
        assert!(!app.world().resource::<Checks>().0[&pr].asked);
    }

    #[test]
    fn the_checks_are_asked_again_after_a_reconnect() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::tell(&mut app, Tell::Lost("gone".into()));
        testing::recorded(&mut app);
        let mut model = app.world_mut().resource_mut::<crate::bridge::Model>();
        model.connection = crate::bridge::Connection::Live;
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).contains(&Ask::GetChecks(pr)));
    }

    #[test]
    fn the_security_tab_shows_the_findings() {
        let mut app = testing::app(fixture::demo(NOW));
        open_security(&mut app);
        for needle in [
            "3 security findings",
            "Claude Code checked the 7 changed files 2 minutes ago.",
            "1 high",
            "Check again",
            "HIGH",
            "Refresh token written to the debug log",
            "src/client/http.rs:17",
            "Proposed comment",
            "This logs the whole request body, refresh token included.",
            "Add to draft",
            "Edit first",
            "Show in diff",
        ] {
            assert!(testing::shows(&mut app, needle), "shows {needle}");
        }
        assert!(
            !testing::shows(&mut app, "Arrives with the harness"),
            "no placeholder"
        );
    }

    #[test]
    fn a_stale_card_offers_only_dismiss_and_show_in_diff() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_security(&mut app);
        testing::tell(
            &mut app,
            Tell::Checks(ChecksTell::State {
                pr,
                kind: CheckKind::Security,
                state: CheckState::Stale,
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(
            &mut app,
            "Check again to add these to your draft."
        ));
        assert_eq!(testing::count::<FindingAccept>(&mut app), 0);
        assert_eq!(testing::count::<FindingEdit>(&mut app), 0);
        assert_eq!(testing::count::<FindingDismiss>(&mut app), 3);
        assert_eq!(testing::count::<FindingShow>(&mut app), 3);
    }

    #[test]
    fn edit_first_on_an_unanchored_finding_names_the_place() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_security(&mut app);
        let id = security_ids()[0].clone();
        app.world_mut()
            .resource_mut::<Checks>()
            .0
            .get_mut(&pr)
            .unwrap()
            .security
            .result
            .as_mut()
            .unwrap()
            .findings[0]
            .anchored = false;
        testing::settle(&mut app);
        let edit = testing::find::<FindingEdit>(&mut app, |b| b.id == id);
        testing::activate(&mut app, edit);
        let editor = testing::tab(&app, &pr).ui.editor.expect("an editor");
        assert!(
            editor.text.starts_with("src/client/http.rs:17: "),
            "{}",
            editor.text
        );
    }

    #[test]
    fn the_security_tab_says_what_the_check_is_doing() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_security(&mut app);
        testing::tell(
            &mut app,
            Tell::Checks(ChecksTell::State {
                pr: pr.clone(),
                kind: CheckKind::Security,
                state: CheckState::Running {
                    activity: Some("Reading src/auth/store.rs…".into()),
                },
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "Reading src/auth/store.rs…"));
        assert!(testing::shows(&mut app, "Stop"));
        assert!(!testing::shows(&mut app, "Refresh token written"));
        let stop = testing::find::<CheckButton>(&mut app, |b| b.action == CheckAction::Stop);
        testing::activate(&mut app, stop);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::StopCheck {
                pr: pr.clone(),
                kind: CheckKind::Security
            }]
        );
        let section = app.world().resource::<ReviewTabs>().0[&pr].ui.section;
        assert_eq!(section, ReviewSection::Security);
    }

    #[test]
    fn a_clean_result_is_worded_as_found_nothing() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_security(&mut app);
        let mut result = fixture::demo_checks(NOW).0.remove(0);
        result.findings.clear();
        testing::tell(
            &mut app,
            Tell::Checks(ChecksTell::Done {
                pr,
                kind: CheckKind::Security,
                result,
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "No security findings"));
        assert!(testing::shows(
            &mut app,
            "Claude Code found no security issues in 7 files."
        ));
        assert!(!testing::shows(&mut app, "safe"));
    }

    #[test]
    fn add_to_draft_asks_and_the_card_waits() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_security(&mut app);
        let id = security_ids()[0].clone();
        let accept = testing::find::<FindingAccept>(&mut app, |b| b.id == id);
        testing::activate(&mut app, accept);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::AcceptFinding {
                pr: pr.clone(),
                id: id.clone(),
                body: None
            }]
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "Adding…"));
        testing::tell(
            &mut app,
            Tell::Checks(ChecksTell::Settled {
                pr,
                id,
                accepted: true,
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "✓ In your draft"));
        assert!(!testing::shows(&mut app, "Adding…"));
    }

    #[test]
    fn dismiss_asks_and_the_card_goes() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_security(&mut app);
        let id = security_ids()[2].clone();
        let dismiss = testing::find::<FindingDismiss>(&mut app, |b| b.id == id);
        testing::activate(&mut app, dismiss);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::DismissFinding {
                pr: pr.clone(),
                id: id.clone()
            }]
        );
        testing::tell(
            &mut app,
            Tell::Checks(ChecksTell::Settled {
                pr,
                id,
                accepted: false,
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "2 security findings"));
        assert!(!testing::shows(&mut app, "Token file created"));
    }

    #[test]
    fn a_refused_accept_frees_the_card() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_security(&mut app);
        let id = security_ids()[0].clone();
        let accept = testing::find::<FindingAccept>(&mut app, |b| b.id == id);
        testing::activate(&mut app, accept);
        testing::tell(
            &mut app,
            Tell::Checks(ChecksTell::Refused { pr, id: id.clone() }),
        );
        testing::settle(&mut app);
        assert!(!testing::shows(&mut app, "Adding…"));
        assert_eq!(testing::count::<FindingAccept>(&mut app), 3);
    }

    #[test]
    fn edit_first_opens_the_composer_with_the_proposed_comment() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_security(&mut app);
        let finding = fixture::demo_checks(NOW).0[0].findings[0].clone();
        let edit = testing::find::<FindingEdit>(&mut app, |b| b.id == finding.id);
        testing::activate(&mut app, edit);
        testing::settle(&mut app);
        let editor = testing::tab(&app, &pr).ui.editor.expect("an editor");
        assert_eq!(editor.target, EditTarget::Finding(finding.id.clone()));
        assert_eq!(editor.text, finding.comment);
        let area = testing::find::<EditorArea>(&mut app, |_| true);
        testing::type_into(&mut app, area, " Thanks!");
        let submit = testing::find::<EditorSubmit>(&mut app, |_| true);
        testing::activate(&mut app, submit);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::AcceptFinding {
                pr: pr.clone(),
                id: finding.id.clone(),
                body: Some(format!("{} Thanks!", finding.comment)),
            }]
        );
        assert!(
            testing::tab(&app, &pr).ui.editor.is_some(),
            "kept until the daemon says it is in the draft"
        );
        testing::tell(
            &mut app,
            Tell::Checks(ChecksTell::Settled {
                pr: pr.clone(),
                id: finding.id,
                accepted: true,
            }),
        );
        assert!(testing::tab(&app, &pr).ui.editor.is_none());
    }

    #[test]
    fn edit_first_prefills_what_the_card_shows() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_security(&mut app);
        let id = security_ids()[0].clone();
        app.world_mut()
            .resource_mut::<Checks>()
            .0
            .get_mut(&pr)
            .unwrap()
            .security
            .result
            .as_mut()
            .unwrap()
            .findings[0]
            .comment = "Log less\u{1b}[31m please\u{202e}.\nThanks".into();
        testing::settle(&mut app);
        let edit = testing::find::<FindingEdit>(&mut app, |b| b.id == id);
        testing::activate(&mut app, edit);
        let editor = testing::tab(&app, &pr).ui.editor.expect("an editor");
        assert!(
            !editor.text.contains('\u{1b}') && !editor.text.contains('\u{202e}'),
            "{:?}",
            editor.text
        );
        assert!(editor.text.starts_with("Log less") && editor.text.contains('\n'));
    }

    #[test]
    fn the_badge_counts_only_a_result_the_tab_lists() {
        let mut m = loaded();
        for (state, want) in [
            (CheckState::Done, Some("3")),
            (CheckState::Stale, Some("3")),
            (CheckState::NotRun, None),
            (
                CheckState::Failed {
                    message: "timed out".into(),
                },
                None,
            ),
        ] {
            m.security.state = state;
            assert_eq!(m.badge(CheckKind::Security).as_deref(), want);
        }
    }

    #[test]
    fn edit_first_waits_for_an_open_comment() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_security(&mut app);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .editor = Some(Editor {
            target: EditTarget::General,
            text: "half a thought".into(),
            error: None,
            ticket: None,
            mode: crate::ui::composer::ComposerMode::Write,
        });
        let edit = testing::find::<FindingEdit>(&mut app, |_| true);
        testing::activate(&mut app, edit);
        assert_eq!(
            testing::tab(&app, &pr).ui.editor.unwrap().target,
            EditTarget::General
        );
    }

    #[test]
    fn show_in_diff_opens_the_file() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_security(&mut app);
        let show = testing::find::<FindingShow>(&mut app, |b| b.file == "src/client/http.rs");
        testing::activate(&mut app, show);
        let ui = testing::tab(&app, &pr).ui;
        assert_eq!(ui.section, ReviewSection::Diff);
        assert_eq!(ui.file.as_deref(), Some("src/client/http.rs"));
    }

    #[test]
    fn a_cached_copy_sends_nothing() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_security(&mut app);
        if let Some(ready) = app
            .world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .and_then(crate::review_state::Tab::ready_mut)
        {
            ready.cached_at = Some(NOW - 3600);
        }
        testing::settle(&mut app);
        let accept = testing::find::<FindingAccept>(&mut app, |_| true);
        testing::activate(&mut app, accept);
        assert!(testing::recorded(&mut app).is_empty());
        assert!(app.world().resource::<Checks>().0[&pr].pending.is_empty());
    }

    #[test]
    fn the_tab_labels_carry_the_badges() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        assert!(!testing::shows(&mut app, "●"), "nothing runs");
        testing::tell(
            &mut app,
            Tell::Checks(ChecksTell::State {
                pr,
                kind: CheckKind::Security,
                state: CheckState::Running { activity: None },
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "●"));
    }

    #[test]
    fn closed_tabs_lose_their_checks() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        app.world_mut()
            .resource_mut::<crate::nav::Nav>()
            .close_review(&pr);
        testing::settle(&mut app);
        assert!(app.world().resource::<Checks>().0.is_empty());
    }
}
