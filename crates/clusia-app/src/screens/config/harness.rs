//! Config › Harness (mockup `ConfigHarness.png`): the agent tool Clúsia drives, its settings, a
//! test, and what the agent may do without asking.

use std::collections::HashMap;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, observe};
use clusia_core::checks::AuditArea;
use clusia_core::config::{Harness, HarnessKind, OnOpen};
use clusia_harness::Agent;
use clusia_protocol::ProbeResult;

use super::{ConfigField, areas, field_row, option_card, page_header, row, setter};
use crate::bridge::{Ask, Asks, Model, ProbeState, set_config};
use crate::fonts::UiFonts;
use crate::nav::{Nav, Screen, Section};
use crate::snapshot::Snapshot;
use crate::theme::Swatch;
use crate::ui::kit::{
    Fill, Stroke, Type, Variant, button, card, checkbox, disabled_button, disabled_checkbox, text,
    text_field,
};

/// What the probe card shows.
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeCard {
    Idle,
    Testing,
    Ok {
        name: String,
        version: String,
        program: String,
        seconds: String,
        account: Option<String>,
    },
    Failed {
        name: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct HarnessView {
    /// The configured path; empty when Clúsia looks on the PATH.
    pub program: String,
    /// The agent the settings name.
    pub kind: HarnessKind,
    pub name: &'static str,
    /// The program Clúsia looks for on the PATH when none is configured.
    pub program_name: &'static str,
    /// The agent just chosen in this visit, for the note under the cards.
    pub switched: Option<&'static str>,
    pub extra_args: String,
    pub on_open: OnOpen,
    pub timeout: String,
    pub use_cli_permissions: bool,
    /// Commands run in the OS sandbox (no network, writes only in the worktree).
    pub sandbox: bool,
    /// Seconds a permission request waits before it is denied.
    pub permission_timeout: String,
    pub program_error: Option<String>,
    pub args_error: Option<String>,
    pub on_open_error: Option<String>,
    pub timeout_error: Option<String>,
    pub permissions_error: Option<String>,
    pub sandbox_error: Option<String>,
    pub permission_timeout_error: Option<String>,
    pub probe: ProbeCard,
    pub open: OpenChecks,
    pub check_timeout: String,
    pub check_timeout_error: Option<String>,
    /// *Revalidate my draft*: check a saved review's comments when its pull request gets new
    /// commits.
    pub revalidate: bool,
    pub revalidate_error: Option<String>,
    /// Seconds without a new commit before the check starts.
    pub quiet: String,
    pub quiet_error: Option<String>,
    pub areas: Vec<AuditArea>,
    pub areas_error: Option<String>,
    /// The add/edit form, when it is open.
    pub form: Option<areas::AreaForm>,
}

/// What the three *When I open a review* checkboxes show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenChecks {
    pub summarize: bool,
    pub security: bool,
    pub audit: bool,
}

pub fn open_checks(h: &Harness) -> OpenChecks {
    OpenChecks {
        summarize: h.on_open == OnOpen::Summarize,
        security: h.check_security,
        audit: h.audit,
    }
}

/// The note under the checks.
pub fn cost_note(name: &str) -> String {
    format!("Each check is one {name} turn on your account.")
}

/// The note under *Revalidate my draft*.
pub fn revalidate_note(name: &str) -> String {
    format!(
        "{name} reads each draft comment against the new commits and proposes keeping, moving, removing or rewording it. Nothing changes until you accept it. It is one {name} turn per round, on your account."
    )
}

/// What the page shows right after the user chose another agent: the kind chosen, until the
/// page is left.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Switched(pub Option<HarnessKind>);

/// An agent card: a click makes it the one Clúsia uses for reviews opened from then on.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindCard(pub HarnessKind);

fn kind_value(kind: HarnessKind) -> &'static str {
    match kind {
        HarnessKind::ClaudeCode => "claude-code",
        HarnessKind::Codex => "codex",
    }
}

fn on_kind(
    activate: On<Activate>,
    cards: Query<&KindCard>,
    mut model: ResMut<Model>,
    mut asks: ResMut<Asks>,
    mut switched: ResMut<Switched>,
) {
    let Ok(KindCard(kind)) = cards.get(activate.entity) else {
        return;
    };
    if model.snapshot.config.harness.kind == *kind {
        return;
    }
    switched.0 = Some(*kind);
    write_kind(&mut asks, &mut model, kind_value(*kind));
}

/// Writes `harness.kind`. When the kind changes while a program is configured, that program
/// served the other agent (`claude` run as `codex app-server` is wrong), so it is cleared too,
/// after the kind. The daemon clears it as well; the second write is what the window shows at
/// once and what demo mode keeps.
pub fn write_kind(asks: &mut Asks, model: &mut Model, value: &'static str) {
    let changes = kind_value(model.snapshot.config.harness.kind) != value;
    let program_set = model.snapshot.config.harness.program.is_some();
    set_config(asks, model, "harness.kind", value);
    if changes && program_set {
        set_config(asks, model, "harness.program", "");
    }
}

/// The note after a change of agent lasts until the user leaves Config › Harness.
pub fn forget_switch(nav: Res<Nav>, mut switched: ResMut<Switched>) {
    if switched.0.is_some() && nav.screen != Screen::Config(Section::Harness) {
        switched.0 = None;
    }
}

/// The first line of the Test card when the agent answered.
pub fn ok_title(name: &str, version: &str, seconds: &str, account: Option<&str>) -> String {
    match account {
        Some(account) => format!("{name} {version} answered in {seconds} s · {account}"),
        None => format!("{name} {version} answered in {seconds} s"),
    }
}

/// A *When I open a review* checkbox (and *Revalidate my draft*): the key it flips.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct OpenToggle(pub &'static str);

/// Flips a *When I open a review* setting from its value now, not from the value the page
/// was drawn with, so a change made since then is not written back.
fn on_open_toggle(
    activate: On<Activate>,
    toggles: Query<&OpenToggle>,
    mut model: ResMut<Model>,
    mut asks: ResMut<Asks>,
) {
    let Ok(OpenToggle(key)) = toggles.get(activate.entity) else {
        return;
    };
    let h = &model.snapshot.config.harness;
    let value = match *key {
        "harness.on_open" if h.on_open == OnOpen::Summarize => "wait".to_string(),
        "harness.on_open" => "summarize".to_string(),
        "harness.check_security" => (!h.check_security).to_string(),
        "harness.audit" => (!h.audit).to_string(),
        "harness.revalidate" => (!h.revalidate).to_string(),
        _ => return,
    };
    set_config(&mut asks, &mut model, key, value);
}

/// The three checkboxes of *When I open a review*, in a column. Config and the first run
/// show the same ones.
pub fn open_checkboxes(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &OpenChecks) {
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(8),
        ..default()
    })
    .with_children(|c| {
        let choices = [
            ("Summarize it", "harness.on_open", v.summarize),
            ("Check security", "harness.check_security", v.security),
            ("Audit the change", "harness.audit", v.audit),
        ];
        for (label, key, on) in choices {
            c.spawn(Node {
                column_gap: px(10),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|r| {
                r.spawn((checkbox(on), OpenToggle(key), observe(on_open_toggle)));
                r.spawn(text(fonts, label, Type::BODY));
            });
        }
    });
}

impl HarnessView {
    /// The same view with the note that follows a change of agent.
    /// The note shows once the write was accepted: until the configured kind is the one chosen,
    /// nothing has changed and nothing is said.
    pub fn with_switched(mut self, switched: &Switched) -> Self {
        self.switched = switched
            .0
            .filter(|kind| *kind == self.kind)
            .map(HarnessKind::name);
        self
    }

    /// The same view with the add/edit form, which lives in a resource of its own.
    pub fn with_form(mut self, form: &areas::AreaForm) -> Self {
        self.form = form.target.is_some().then(|| form.clone());
        if let Some(writing) = &form.writing {
            self.areas = writing.clone();
        }
        self
    }
}

/// The **Test** / **Test again** button.
#[derive(Component, Debug)]
pub struct TestHarness;

pub fn view(
    snap: &Snapshot,
    rejected: &HashMap<String, String>,
    probe: &ProbeState,
) -> HarnessView {
    let h = &snap.config.harness;
    let refusal = |key: &str| rejected.get(key).cloned();
    HarnessView {
        program: h.program.clone().unwrap_or_default(),
        kind: h.kind,
        name: h.kind.name(),
        program_name: Agent::from(h.kind).program(),
        switched: None,
        extra_args: h.extra_args.clone(),
        on_open: h.on_open,
        timeout: h.turn_timeout_secs.to_string(),
        use_cli_permissions: h.use_cli_permissions,
        program_error: refusal("harness.program"),
        args_error: refusal("harness.extra_args"),
        on_open_error: refusal("harness.on_open"),
        timeout_error: refusal("harness.turn_timeout_secs"),
        permissions_error: refusal("harness.use_cli_permissions"),
        sandbox: h.sandbox,
        permission_timeout: h.permission_timeout_secs.to_string(),
        sandbox_error: refusal("harness.sandbox"),
        permission_timeout_error: refusal("harness.permission_timeout_secs"),
        probe: probe_card(probe, h.kind.name()),
        open: open_checks(h),
        check_timeout: h.check_timeout_secs.to_string(),
        check_timeout_error: refusal("harness.check_timeout_secs"),
        revalidate: h.revalidate,
        revalidate_error: refusal("harness.revalidate"),
        quiet: h.revalidate_quiet_secs.to_string(),
        quiet_error: refusal("harness.revalidate_quiet_secs"),
        areas: h.audit_areas.clone(),
        areas_error: refusal(areas::AREAS_KEY),
        form: None,
    }
}

pub fn probe_card(state: &ProbeState, active: &str) -> ProbeCard {
    let named = |result: &ProbeResult| {
        if result.harness.is_empty() {
            active.to_string()
        } else {
            result.harness.clone()
        }
    };
    match state {
        ProbeState::Idle => ProbeCard::Idle,
        ProbeState::Testing => ProbeCard::Testing,
        ProbeState::Done(
            result @ ProbeResult {
                ok: true,
                version,
                program,
                elapsed_ms,
                account,
                ..
            },
        ) => ProbeCard::Ok {
            name: named(result),
            version: version.clone().unwrap_or_default(),
            program: program.clone(),
            seconds: format!("{:.1}", *elapsed_ms as f64 / 1000.0),
            account: account.clone(),
        },
        ProbeState::Done(result) => {
            let name = named(result);
            ProbeCard::Failed {
                message: result
                    .error
                    .clone()
                    .unwrap_or_else(|| format!("{name} did not answer")),
                name,
            }
        }
    }
}

/// The hint next to the program field.
pub fn program_hint(v: &HarnessView) -> String {
    match &v.probe {
        ProbeCard::Ok { program, .. } if !program.is_empty() => format!("Found at {program}"),
        _ if v.program.is_empty() => format!("Looks for {} on your PATH", v.program_name),
        _ => "Using this path".into(),
    }
}

/// A card title led by its radio mark: filled green when selected, an empty grey ring otherwise.
fn title_with_radio(
    c: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    name: &str,
    ty: Type,
    selected: bool,
) {
    c.spawn(Node {
        align_items: AlignItems::Center,
        column_gap: px(8),
        ..default()
    })
    .with_children(|t| {
        t.spawn((
            Node {
                width: px(14),
                height: px(14),
                flex_shrink: 0.0,
                border: px(if selected { 4 } else { 1 }).all(),
                border_radius: BorderRadius::MAX,
                ..default()
            },
            BackgroundColor::default(),
            Fill(Swatch::Surface),
            BorderColor::default(),
            Stroke(if selected {
                Swatch::Green
            } else {
                Swatch::Line
            }),
            RadioMark { selected },
        ));
        t.spawn(text(fonts, name, ty));
    });
}

/// The circle before a harness card's title.
#[derive(Component, Debug)]
pub struct RadioMark {
    pub selected: bool,
}

pub fn build(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &HarnessView) {
    page_header(
        p,
        fonts,
        "Harness",
        "The AI tool you already use. Clúsia starts one session per review and keeps its memory separate from your other work.",
    );
    p.spawn(Node {
        column_gap: px(10),
        ..default()
    })
    .with_children(|r| {
        for (kind, about) in [
            (HarnessKind::ClaudeCode, "claude, with your settings"),
            (HarnessKind::Codex, "codex app-server, with your login"),
        ] {
            let selected = v.kind == kind;
            r.spawn((
                option_card(selected, 250.0),
                KindCard(kind),
                observe(on_kind),
            ))
            .with_children(|c| {
                title_with_radio(c, fonts, kind.name(), Type::STRONG, selected);
                c.spawn(text(fonts, about, Type::META));
            });
        }
        r.spawn(card(Node {
            width: px(250),
            flex_direction: FlexDirection::Column,
            row_gap: px(4),
            padding: UiRect::axes(px(14), px(12)),
            ..default()
        }))
        .with_children(|c| {
            title_with_radio(
                c,
                fonts,
                "Custom command",
                Type::STRONG.ink(Swatch::Faint),
                false,
            );
            c.spawn(text(fonts, "any tool that speaks JSON", Type::META));
            c.spawn(text(fonts, "Arrives in a later milestone", Type::META));
        });
    });
    if let Some(name) = v.switched {
        p.spawn(text(
            fonts,
            format!(
                "Reviews you open from now on use {name}. Open reviews keep their current agent."
            ),
            Type::META,
        ));
    }
    row(
        p,
        fonts,
        "Program",
        |r| {
            r.spawn((
                text_field(fonts, &v.program, 380.0, true),
                ConfigField("harness.program"),
            ));
        },
        &program_hint(v),
        v.program_error.as_deref().map(|m| ("harness.program", m)),
    );
    row(
        p,
        fonts,
        "Extra arguments",
        |r| {
            r.spawn((
                text_field(fonts, &v.extra_args, 380.0, true),
                ConfigField("harness.extra_args"),
            ));
        },
        "Passed to every session",
        v.args_error.as_deref().map(|m| ("harness.extra_args", m)),
    );
    row(
        p,
        fonts,
        "When I open a review",
        |r| open_checkboxes(r, fonts, &v.open),
        &cost_note(v.name),
        None,
    );
    if let Some(message) = &v.on_open_error {
        p.spawn((
            super::FieldError("harness.on_open"),
            children![text(fonts, message.clone(), Type::BODY.ink(Swatch::Orange))],
        ));
    }
    field_row(
        p,
        fonts,
        "Turn timeout",
        "harness.turn_timeout_secs",
        &v.timeout,
        120.0,
        "Seconds, 60 to 3600. A turn that takes longer is stopped",
        &v.timeout_error,
    );
    field_row(
        p,
        fonts,
        "Check timeout",
        "harness.check_timeout_secs",
        &v.check_timeout,
        120.0,
        "Seconds, 60 to 1800. A check that takes longer is stopped",
        &v.check_timeout_error,
    );
    field_row(
        p,
        fonts,
        "Deny unanswered requests after",
        "harness.permission_timeout_secs",
        &v.permission_timeout,
        120.0,
        "Seconds, 30 to 600. No answer means deny, and you get a notification",
        &v.permission_timeout_error,
    );
    probe_row(p, fonts, &v.probe, v.name);
    super::heading(p, fonts, "When new commits arrive on a saved review");
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(6),
        ..default()
    })
    .with_children(|c| {
        c.spawn(Node {
            column_gap: px(10),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|r| {
            r.spawn((
                checkbox(v.revalidate),
                OpenToggle("harness.revalidate"),
                observe(on_open_toggle),
            ));
            r.spawn(text(fonts, "Revalidate my draft", Type::BODY));
        });
        c.spawn(text(fonts, revalidate_note(v.name), Type::META));
        if let Some(message) = &v.revalidate_error {
            c.spawn((
                super::FieldError("harness.revalidate"),
                children![text(fonts, message.clone(), Type::BODY.ink(Swatch::Orange))],
            ));
        }
    });
    field_row(
        p,
        fonts,
        "Wait for pushes to settle",
        "harness.revalidate_quiet_secs",
        &v.quiet,
        120.0,
        "Seconds, 60 to 3600. The check starts once no new commit has arrived for this long",
        &v.quiet_error,
    );
    super::heading(p, fonts, "What the agent may do without asking");
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(10),
        ..default()
    })
    .with_children(|c| {
        c.spawn(Node {
            column_gap: px(10),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|r| {
            r.spawn(disabled_checkbox(true));
            r.spawn(text(
                fonts,
                "Read and search the review worktree",
                Type::BODY,
            ));
        });
        let codex = v.kind == HarnessKind::Codex;
        if !codex {
            c.spawn(Node {
                column_gap: px(10),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|r| {
                r.spawn((
                    checkbox(v.use_cli_permissions),
                    setter(
                        "harness.use_cli_permissions",
                        (!v.use_cli_permissions).to_string(),
                    ),
                ));
                r.spawn(text(
                    fonts,
                    "Also allow what my Claude Code settings already allow",
                    Type::BODY,
                ));
            });
            if let Some(message) = &v.permissions_error {
                c.spawn((
                    super::FieldError("harness.use_cli_permissions"),
                    children![text(fonts, message.clone(), Type::BODY.ink(Swatch::Orange))],
                ));
            }
        }
        c.spawn(Node {
            column_gap: px(10),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|r| {
            if codex {
                r.spawn(disabled_checkbox(true));
                r.spawn(text(
                    fonts,
                    "Codex always runs in its read-only sandbox",
                    Type::BODY,
                ));
            } else {
                r.spawn((
                    checkbox(v.sandbox),
                    setter("harness.sandbox", (!v.sandbox).to_string()),
                ));
                r.spawn(text(
                    fonts,
                    "Run commands in a sandbox: no network, writes only in the review worktree",
                    Type::BODY,
                ));
            }
        });
        if !codex && !v.sandbox {
            // Outside the sandbox a command also reaches clusiad's socket, which trusts any
            // program of the user.
            c.spawn(text(fonts, SANDBOX_OFF, Type::MUTED));
        }
        if let Some(message) = &v.sandbox_error {
            c.spawn((
                super::FieldError("harness.sandbox"),
                children![text(fonts, message.clone(), Type::BODY.ink(Swatch::Orange))],
            ));
        }
    });
    areas::build(p, fonts, v.name, &v.areas, &v.areas_error, &v.form);
}

/// What turning the sandbox off lets an allowed command do.
const SANDBOX_OFF: &str = "With the sandbox off, an allowed command can reach the network, write outside the worktree and act through Clúsia as you: answer its own requests, change the draft or publish.";

fn probe_row(p: &mut ChildSpawnerCommands, fonts: &UiFonts, state: &ProbeCard, active: &str) {
    let (fill, stroke) = match state {
        ProbeCard::Ok { .. } => (Swatch::GreenSoft, Swatch::Green),
        ProbeCard::Failed { .. } => (Swatch::OrangeSoft, Swatch::Orange),
        _ => (Swatch::Surface, Swatch::Line),
    };
    let (title, detail) = match state {
        ProbeCard::Idle => (
            "Not tested yet".to_string(),
            format!(
                "Press Test to check that Clúsia can find {active} and see how fast it answers."
            ),
        ),
        ProbeCard::Testing => (
            format!("Testing {active}…"),
            "Waiting for an answer".to_string(),
        ),
        ProbeCard::Ok {
            name,
            version,
            program,
            seconds,
            account,
        } => (
            ok_title(name, version, seconds, account.as_deref()),
            format!("Found at {program}"),
        ),
        ProbeCard::Failed { name, message } => (format!("{name} did not answer"), message.clone()),
    };
    p.spawn((
        Node {
            max_width: px(760),
            padding: UiRect::axes(px(16), px(12)),
            column_gap: px(16),
            align_items: AlignItems::Center,
            border: px(1).all(),
            border_radius: BorderRadius::all(px(8)),
            ..default()
        },
        BackgroundColor::default(),
        Fill(fill),
        BorderColor::default(),
        Stroke(stroke),
    ))
    .with_children(|c| {
        c.spawn(Node {
            flex_grow: 1.0,
            min_width: px(0),
            flex_direction: FlexDirection::Column,
            row_gap: px(2),
            ..default()
        })
        .with_children(|t| {
            t.spawn(text(fonts, title, Type::STRONG));
            t.spawn(text(fonts, detail, Type::META));
        });
        match state {
            ProbeCard::Testing => {
                c.spawn(disabled_button(fonts, "Testing…"));
            }
            ProbeCard::Idle => {
                c.spawn((
                    button(fonts, "Test", Variant::Secondary),
                    TestHarness,
                    observe(on_test),
                ));
            }
            _ => {
                c.spawn((
                    button(fonts, "Test again", Variant::Secondary),
                    TestHarness,
                    observe(on_test),
                ));
            }
        }
    });
}

fn on_test(_activate: On<Activate>, mut model: ResMut<Model>, mut asks: ResMut<Asks>) {
    if model.probe == ProbeState::Testing {
        return;
    }
    model.probe = ProbeState::Testing;
    asks.send(Ask::Probe);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Snapshot;
    use clusia_core::config::HarnessKind;

    fn probe(ok: bool) -> ProbeResult {
        ProbeResult {
            ok,
            version: ok.then(|| "2.1.294".to_string()),
            program: "/opt/homebrew/bin/claude".into(),
            elapsed_ms: 1800,
            error: (!ok).then(|| "Run `claude` once in a terminal".to_string()),
            harness: "Claude Code".into(),
            signed_in: None,
            account: None,
        }
    }

    #[test]
    fn the_view_reads_the_settings() {
        let v = view(&Snapshot::default(), &HashMap::new(), &ProbeState::Idle);
        assert_eq!(v.program, "");
        assert_eq!(v.extra_args, "");
        assert_eq!(v.on_open, OnOpen::Summarize);
        assert_eq!(v.timeout, "600");
        assert!(v.use_cli_permissions);
        assert_eq!(v.probe, ProbeCard::Idle);
        assert!(v.sandbox, "the sandbox is on by default");
        assert_eq!(v.permission_timeout, "120");

        let mut snap = Snapshot::default();
        snap.config.harness.program = Some("/opt/homebrew/bin/claude".into());
        snap.config.harness.extra_args = "--model claude-opus-5-5".into();
        snap.config.harness.on_open = OnOpen::Wait;
        snap.config.harness.turn_timeout_secs = 900;
        snap.config.harness.use_cli_permissions = false;
        snap.config.harness.sandbox = false;
        snap.config.harness.permission_timeout_secs = 300;
        let v = view(&snap, &HashMap::new(), &ProbeState::Testing);
        assert_eq!(v.program, "/opt/homebrew/bin/claude");
        assert_eq!(v.extra_args, "--model claude-opus-5-5");
        assert_eq!(v.on_open, OnOpen::Wait);
        assert_eq!(v.timeout, "900");
        assert!(!v.use_cli_permissions);
        assert_eq!(v.probe, ProbeCard::Testing);
        assert!(!v.sandbox);
        assert_eq!(v.permission_timeout, "300");
    }

    #[test]
    fn refusals_belong_to_their_rows() {
        let rejected: HashMap<String, String> = [
            ("harness.program".to_string(), "no such file".to_string()),
            (
                "harness.extra_args".to_string(),
                "--dangerously-skip-permissions is not allowed".to_string(),
            ),
            (
                "harness.turn_timeout_secs".to_string(),
                "must be between 60 and 3600".to_string(),
            ),
            (
                "harness.permission_timeout_secs".to_string(),
                "harness.permission_timeout_secs must be between 30 and 600, got 5".to_string(),
            ),
            ("harness.sandbox".to_string(), "not saved".to_string()),
        ]
        .into();
        let v = view(&Snapshot::default(), &rejected, &ProbeState::Idle);
        assert_eq!(v.program_error.as_deref(), Some("no such file"));
        assert!(v.args_error.as_deref().unwrap().contains("not allowed"));
        assert_eq!(
            v.timeout_error.as_deref(),
            Some("must be between 60 and 3600")
        );
        assert_eq!(v.on_open_error, None);
        assert_eq!(
            v.permission_timeout_error.as_deref(),
            Some("harness.permission_timeout_secs must be between 30 and 600, got 5")
        );
        assert_eq!(v.sandbox_error.as_deref(), Some("not saved"));
    }

    #[test]
    fn the_probe_card_follows_the_answer() {
        assert_eq!(
            probe_card(&ProbeState::Done(probe(true)), "Claude Code"),
            ProbeCard::Ok {
                name: "Claude Code".into(),
                version: "2.1.294".into(),
                program: "/opt/homebrew/bin/claude".into(),
                seconds: "1.8".into(),
                account: None,
            }
        );
        assert_eq!(
            probe_card(&ProbeState::Done(probe(false)), "Claude Code"),
            ProbeCard::Failed {
                name: "Claude Code".into(),
                message: "Run `claude` once in a terminal".into()
            }
        );
        let mut silent = probe(false);
        silent.error = None;
        assert_eq!(
            probe_card(&ProbeState::Done(silent), "Codex"),
            ProbeCard::Failed {
                name: "Claude Code".into(),
                message: "Claude Code did not answer".into()
            },
            "the answer names the agent that was tested"
        );
    }

    #[test]
    fn a_codex_answer_shows_its_name_and_login() {
        let answer = ProbeResult {
            ok: true,
            version: Some("0.162.1".into()),
            program: "/opt/homebrew/bin/codex".into(),
            elapsed_ms: 400,
            error: None,
            harness: "Codex".into(),
            signed_in: Some(true),
            account: Some("Logged in using ChatGPT".into()),
        };
        let card = probe_card(&ProbeState::Done(answer), "Claude Code");
        assert_eq!(
            card,
            ProbeCard::Ok {
                name: "Codex".into(),
                version: "0.162.1".into(),
                program: "/opt/homebrew/bin/codex".into(),
                seconds: "0.4".into(),
                account: Some("Logged in using ChatGPT".into()),
            }
        );
        assert_eq!(
            ok_title("Codex", "0.162.1", "0.4", Some("Logged in using ChatGPT")),
            "Codex 0.162.1 answered in 0.4 s · Logged in using ChatGPT"
        );
        assert_eq!(
            ok_title("Claude Code", "2.1.294", "1.8", None),
            "Claude Code 2.1.294 answered in 1.8 s"
        );
    }

    #[test]
    fn the_program_hint_says_where_it_looks() {
        let mut v = view(&Snapshot::default(), &HashMap::new(), &ProbeState::Idle);
        assert_eq!(program_hint(&v), "Looks for claude on your PATH");
        v.program = "/usr/local/bin/claude".into();
        assert_eq!(program_hint(&v), "Using this path");
        v.probe = ProbeCard::Ok {
            name: "Claude Code".into(),
            version: "2.1.294".into(),
            program: "/usr/local/bin/claude".into(),
            seconds: "1.8".into(),
            account: None,
        };
        assert_eq!(program_hint(&v), "Found at /usr/local/bin/claude");

        let mut snap = Snapshot::default();
        snap.config.harness.kind = HarnessKind::Codex;
        let v = view(&snap, &HashMap::new(), &ProbeState::Idle);
        assert_eq!(program_hint(&v), "Looks for codex on your PATH");
    }

    fn harness_app() -> App {
        let mut app = crate::testing::app(crate::fixture::demo(crate::testing::NOW));
        app.world_mut()
            .resource_mut::<crate::nav::Nav>()
            .go(&clusia_protocol::WindowTarget::Config);
        app.world_mut()
            .resource_mut::<crate::nav::Nav>()
            .open_section(crate::nav::Section::Harness);
        crate::testing::settle(&mut app);
        app
    }

    #[test]
    fn the_view_reads_the_review_checks() {
        let v = view(&Snapshot::default(), &HashMap::new(), &ProbeState::Idle);
        assert_eq!(
            v.open,
            OpenChecks {
                summarize: true,
                security: true,
                audit: true
            }
        );
        assert_eq!(v.check_timeout, "600");
        assert_eq!(v.areas.len(), 6);
        assert_eq!((v.areas_error, v.form), (None, None));

        let mut snap = Snapshot::default();
        snap.config.harness.on_open = OnOpen::Wait;
        snap.config.harness.check_security = false;
        snap.config.harness.audit = false;
        snap.config.harness.check_timeout_secs = 900;
        let v = view(&snap, &HashMap::new(), &ProbeState::Idle);
        assert_eq!(
            v.open,
            OpenChecks {
                summarize: false,
                security: false,
                audit: false
            }
        );
        assert_eq!(v.check_timeout, "900");
    }

    #[test]
    fn the_timeout_refusal_belongs_to_its_row() {
        let rejected: HashMap<String, String> = [(
            "harness.check_timeout_secs".to_string(),
            "harness.check_timeout_secs must be between 60 and 1800, got 5".to_string(),
        )]
        .into();
        let v = view(&Snapshot::default(), &rejected, &ProbeState::Idle);
        assert!(
            v.check_timeout_error
                .unwrap()
                .contains("between 60 and 1800")
        );
    }

    #[test]
    fn the_three_checkboxes_write_their_keys() {
        let mut app = harness_app();
        for needle in [
            "When I open a review",
            "Summarize it",
            "Check security",
            "Audit the change",
            &cost_note("Claude Code"),
            "Check timeout",
        ] {
            assert!(crate::testing::shows(&mut app, needle), "{needle}");
        }
        assert!(!crate::testing::shows(
            &mut app,
            "Wait for my first question"
        ));
        for (key, value) in [
            ("harness.on_open", "wait"),
            ("harness.check_security", "false"),
            ("harness.audit", "false"),
        ] {
            let e = crate::testing::find::<OpenToggle>(&mut app, |t| t.0 == key);
            crate::testing::activate(&mut app, e);
            assert_eq!(
                crate::testing::recorded(&mut app),
                [Ask::SetConfig {
                    key: key.into(),
                    value: value.into()
                }]
            );
        }
    }

    #[test]
    fn a_checkbox_that_is_off_turns_the_key_back_on() {
        let mut app = harness_app();
        crate::testing::set_config_locally(&mut app, "harness.audit", "false");
        crate::testing::set_config_locally(&mut app, "harness.on_open", "wait");
        crate::testing::settle(&mut app);
        let audit = crate::testing::find::<OpenToggle>(&mut app, |t| t.0 == "harness.audit");
        crate::testing::activate(&mut app, audit);
        let summarize = crate::testing::find::<OpenToggle>(&mut app, |t| t.0 == "harness.on_open");
        crate::testing::activate(&mut app, summarize);
        assert_eq!(
            crate::testing::recorded(&mut app),
            [
                Ask::SetConfig {
                    key: "harness.audit".into(),
                    value: "true".into()
                },
                Ask::SetConfig {
                    key: "harness.on_open".into(),
                    value: "summarize".into()
                },
            ]
        );
    }

    #[test]
    fn a_checkbox_reads_the_setting_when_it_is_clicked() {
        let mut app = harness_app();
        let security =
            crate::testing::find::<OpenToggle>(&mut app, |t| t.0 == "harness.check_security");
        // Changed elsewhere after the page was drawn, before it is drawn again.
        app.world_mut()
            .resource_mut::<crate::bridge::Model>()
            .snapshot
            .config
            .harness
            .check_security = false;
        crate::testing::activate(&mut app, security);
        assert_eq!(
            crate::testing::recorded(&mut app),
            [Ask::SetConfig {
                key: "harness.check_security".into(),
                value: "true".into()
            }]
        );
    }

    #[test]
    fn the_view_reads_the_revalidation_settings() {
        let v = view(&Snapshot::default(), &HashMap::new(), &ProbeState::Idle);
        assert_eq!((v.revalidate, v.quiet.as_str()), (true, "300"));
        let mut snap = Snapshot::default();
        snap.config.harness.revalidate = false;
        snap.config.harness.revalidate_quiet_secs = 600;
        let rejected: HashMap<String, String> = [(
            "harness.revalidate_quiet_secs".to_string(),
            "harness.revalidate_quiet_secs must be between 60 and 3600, got 5".to_string(),
        )]
        .into();
        let v = view(&snap, &rejected, &ProbeState::Idle);
        assert_eq!((v.revalidate, v.quiet.as_str()), (false, "600"));
        assert_eq!(
            v.quiet_error.as_deref(),
            Some("harness.revalidate_quiet_secs must be between 60 and 3600, got 5")
        );
        assert_eq!(v.revalidate_error, None);
    }

    #[test]
    fn the_group_for_saved_reviews_writes_its_keys() {
        use crate::screens::config::ConfigField;
        use crate::ui::kit::Field;
        let mut app = harness_app();
        for needle in [
            "When new commits arrive on a saved review",
            "Revalidate my draft",
            "Wait for pushes to settle",
            &revalidate_note("Claude Code"),
        ] {
            assert!(crate::testing::shows(&mut app, needle), "{needle}");
        }
        let toggle = crate::testing::find::<OpenToggle>(&mut app, |t| t.0 == "harness.revalidate");
        crate::testing::activate(&mut app, toggle);
        assert_eq!(
            crate::testing::recorded(&mut app),
            [Ask::SetConfig {
                key: "harness.revalidate".into(),
                value: "false".into()
            }]
        );
        let field = crate::testing::find::<ConfigField>(&mut app, |f| {
            f.0 == "harness.revalidate_quiet_secs"
        });
        assert_eq!(app.world().get::<Field>(field).unwrap().committed, "300");
    }

    #[test]
    fn the_check_timeout_is_a_field() {
        use crate::screens::config::ConfigField;
        use crate::ui::kit::Field;
        let mut app = harness_app();
        let e =
            crate::testing::find::<ConfigField>(&mut app, |f| f.0 == "harness.check_timeout_secs");
        assert_eq!(app.world().get::<Field>(e).unwrap().committed, "600");
    }

    #[test]
    fn the_view_names_the_configured_agent() {
        let v = view(&Snapshot::default(), &HashMap::new(), &ProbeState::Idle);
        assert_eq!(
            (v.kind, v.name, v.program_name, v.switched),
            (HarnessKind::ClaudeCode, "Claude Code", "claude", None)
        );
        let mut snap = Snapshot::default();
        snap.config.harness.kind = HarnessKind::Codex;
        let v = view(&snap, &HashMap::new(), &ProbeState::Idle);
        assert_eq!(
            (v.kind, v.name, v.program_name),
            (HarnessKind::Codex, "Codex", "codex")
        );
        let v = v.with_switched(&Switched(Some(HarnessKind::Codex)));
        assert_eq!(v.switched, Some("Codex"));
    }

    #[test]
    fn the_switched_note_waits_for_the_accepted_write() {
        let snap = Snapshot::default();
        let v = view(&snap, &HashMap::new(), &ProbeState::Idle);
        let v = v.with_switched(&Switched(Some(HarnessKind::Codex)));
        assert_eq!(
            v.switched, None,
            "the daemon has not accepted Codex (or refused it): the page still says Claude Code"
        );
    }

    #[test]
    fn the_notes_take_the_agent_name() {
        assert_eq!(
            cost_note("Codex"),
            "Each check is one Codex turn on your account."
        );
        assert!(revalidate_note("Codex").starts_with("Codex reads each draft comment"));
        assert!(revalidate_note("Codex").contains("one Codex turn per round"));
        assert!(!revalidate_note("Codex").contains("Claude"));
    }

    #[test]
    fn both_kind_cards_can_be_chosen_and_custom_waits() {
        let mut app = harness_app();
        for needle in [
            "claude, with your settings",
            "codex app-server, with your login",
        ] {
            assert!(crate::testing::shows(&mut app, needle), "{needle}");
        }
        assert_eq!(
            crate::testing::count::<KindCard>(&mut app),
            2,
            "Claude Code and Codex"
        );
        assert!(!crate::testing::shows(
            &mut app,
            "Reviews you open from now on use"
        ));

        let codex = crate::testing::find::<KindCard>(&mut app, |c| c.0 == HarnessKind::Codex);
        crate::testing::activate(&mut app, codex);
        assert_eq!(
            crate::testing::recorded(&mut app),
            [Ask::SetConfig {
                key: "harness.kind".into(),
                value: "codex".into()
            }]
        );
        crate::testing::set_config_locally(&mut app, "harness.kind", "codex");
        crate::testing::settle(&mut app);
        assert!(crate::testing::shows(
            &mut app,
            "Reviews you open from now on use Codex. Open reviews keep their current agent."
        ));
        let marks: Vec<bool> = app
            .world_mut()
            .query::<&RadioMark>()
            .iter(app.world())
            .map(|m| m.selected)
            .collect();
        assert_eq!(marks, [false, true, false], "Codex is the selected card");

        let same = crate::testing::find::<KindCard>(&mut app, |c| c.0 == HarnessKind::Codex);
        crate::testing::activate(&mut app, same);
        assert!(
            crate::testing::recorded(&mut app).is_empty(),
            "choosing the card already chosen writes nothing"
        );
    }

    #[test]
    fn switching_the_kind_clears_the_program_that_belonged_to_the_other_agent() {
        let mut app = harness_app();
        crate::testing::set_config_locally(&mut app, "harness.program", "/opt/homebrew/bin/claude");
        crate::testing::settle(&mut app);
        let codex = crate::testing::find::<KindCard>(&mut app, |c| c.0 == HarnessKind::Codex);
        crate::testing::activate(&mut app, codex);
        assert_eq!(
            crate::testing::recorded(&mut app),
            [
                Ask::SetConfig {
                    key: "harness.kind".into(),
                    value: "codex".into()
                },
                Ask::SetConfig {
                    key: "harness.program".into(),
                    value: String::new()
                },
            ],
            "the kind first, then the program that served the other agent"
        );
    }

    #[test]
    fn a_kind_change_with_no_program_writes_only_the_kind() {
        let mut app = harness_app();
        let codex = crate::testing::find::<KindCard>(&mut app, |c| c.0 == HarnessKind::Codex);
        crate::testing::activate(&mut app, codex);
        assert_eq!(
            crate::testing::recorded(&mut app),
            [Ask::SetConfig {
                key: "harness.kind".into(),
                value: "codex".into()
            }]
        );
    }

    #[test]
    fn codex_hides_the_claude_switch_and_locks_the_sandbox() {
        let mut app = harness_app();
        assert!(crate::testing::shows(
            &mut app,
            "Also allow what my Claude Code settings already allow"
        ));
        crate::testing::set_config_locally(&mut app, "harness.kind", "codex");
        crate::testing::settle(&mut app);
        assert!(!crate::testing::shows(
            &mut app,
            "Also allow what my Claude Code settings already allow"
        ));
        assert!(crate::testing::shows(
            &mut app,
            "Codex always runs in its read-only sandbox"
        ));
        assert!(!crate::testing::shows(
            &mut app,
            "Run commands in a sandbox: no network, writes only in the review worktree"
        ));
        assert!(crate::testing::shows(
            &mut app,
            "Each check is one Codex turn on your account."
        ));
        assert!(crate::testing::shows(
            &mut app,
            "Codex audits the change one area at a time."
        ));
        assert!(crate::testing::shows(
            &mut app,
            "Looks for codex on your PATH"
        ));
        assert!(
            crate::testing::recorded(&mut app).is_empty(),
            "the locked sandbox row writes nothing"
        );
    }
}
