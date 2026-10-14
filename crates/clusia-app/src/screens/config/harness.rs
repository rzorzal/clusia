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

use super::{ConfigField, areas, field_row, link, option_card, page_header, row, setter};
use crate::bridge::{Ask, Asks, Model, ProbeState, set_config};
use crate::fonts::UiFonts;
use crate::nav::{Nav, Screen, Section};
use crate::snapshot::Snapshot;
use crate::theme::Swatch;
use crate::ui::kit::{
    Fill, Stroke, Type, Variant, button, checkbox, disabled_button, disabled_checkbox, text,
    text_field,
};

/// The warning under a custom command: Clúsia cannot sandbox what it does not own.
pub const SANDBOX_NOTICE: &str = "Clúsia cannot sandbox a custom command. It runs with your permissions, like any program you start. Clúsia removes your GitHub token from its environment and asks you before anything the program asks to run.";

/// Where "How to write one" leads.
pub const PROTOCOL_DOCS_URL: &str =
    "https://github.com/rzorzal/clusia/blob/main/docs/harness-protocol.md";

/// The amber notice of a custom command.
#[derive(Component, Debug)]
pub struct CustomNotice;

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
    pub name: String,
    /// The program Clúsia looks for on the PATH when none is configured.
    pub program_name: &'static str,
    /// The agent just chosen in this visit, for the note under the cards.
    pub switched: Option<String>,
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
    /// What the Name row shows; a custom command's own name, empty when unset.
    pub custom_name: String,
    pub custom_name_error: Option<String>,
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

pub fn kind_value(kind: HarnessKind) -> &'static str {
    match kind {
        HarnessKind::ClaudeCode => "claude-code",
        HarnessKind::Codex => "codex",
        HarnessKind::Custom => "custom",
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
/// once and what demo mode keeps. The custom command's name goes the same way, and the last
/// Test result is dropped, since it described the other agent.
pub fn write_kind(asks: &mut Asks, model: &mut Model, value: &'static str) {
    let changes = kind_value(model.snapshot.config.harness.kind) != value;
    let program_set = model.snapshot.config.harness.program.is_some();
    let named = !model.snapshot.config.harness.custom_name.is_empty();
    set_config(asks, model, "harness.kind", value);
    if changes {
        model.probe = ProbeState::Idle;
    }
    if changes && program_set {
        set_config(asks, model, "harness.program", "");
    }
    if changes && named {
        set_config(asks, model, "harness.custom_name", "");
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
    let who = if version.is_empty() {
        name.to_string()
    } else {
        format!("{name} {version}")
    };
    match account {
        Some(account) => format!("{who} answered in {seconds} s · {account}"),
        None => format!("{who} answered in {seconds} s"),
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
            .map(|_| self.name.clone());
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
        name: h.display_name(),
        program_name: Agent::from(h.kind).program(),
        switched: None,
        extra_args: h.extra_args.clone(),
        on_open: h.on_open,
        timeout: h.turn_timeout_secs.to_string(),
        use_cli_permissions: h.use_cli_permissions,
        program_error: refusal("harness.program"),
        args_error: refusal("harness.extra_args"),
        custom_name: h.custom_name.clone(),
        custom_name_error: refusal("harness.custom_name"),
        on_open_error: refusal("harness.on_open"),
        timeout_error: refusal("harness.turn_timeout_secs"),
        permissions_error: refusal("harness.use_cli_permissions"),
        sandbox: h.sandbox,
        permission_timeout: h.permission_timeout_secs.to_string(),
        sandbox_error: refusal("harness.sandbox"),
        permission_timeout_error: refusal("harness.permission_timeout_secs"),
        probe: probe_card(probe, &h.display_name(), h.kind == HarnessKind::Custom),
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

pub fn probe_card(state: &ProbeState, active: &str, custom: bool) -> ProbeCard {
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
            // A custom command speaks Clúsia's protocol, which is what its test line vouches for.
            account: if custom {
                Some("protocol 1".to_string())
            } else {
                account.clone()
            },
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
    if v.kind == HarnessKind::Custom {
        return "Required. Runs once per turn".into();
    }
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
            (
                HarnessKind::Custom,
                "any program that speaks Clúsia's JSON lines",
            ),
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
    });
    if let Some(name) = &v.switched {
        p.spawn(text(
            fonts,
            format!(
                "Reviews you open from now on use {name}. Open reviews keep their current agent."
            ),
            Type::META,
        ));
    }
    let custom = v.kind == HarnessKind::Custom;
    if custom {
        p.spawn((
            CustomNotice,
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
            Fill(Swatch::OrangeSoft),
            BorderColor::default(),
            Stroke(Swatch::Orange),
        ))
        .with_children(|c| {
            c.spawn((
                Node {
                    flex_grow: 1.0,
                    min_width: px(0),
                    ..default()
                },
                children![text(fonts, SANDBOX_NOTICE, Type::BODY)],
            ));
            c.spawn(link(fonts, "How to write one", PROTOCOL_DOCS_URL));
        });
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
        if custom {
            "Passed to every turn"
        } else {
            "Passed to every session"
        },
        v.args_error.as_deref().map(|m| ("harness.extra_args", m)),
    );
    if custom {
        field_row(
            p,
            fonts,
            "Name",
            "harness.custom_name",
            &v.custom_name,
            240.0,
            "Shown on the pages. Filled in by Test",
            &v.custom_name_error,
        );
    }
    row(
        p,
        fonts,
        "When I open a review",
        |r| open_checkboxes(r, fonts, &v.open),
        &cost_note(&v.name),
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
    probe_row(p, fonts, &v.probe, &v.name);
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
        c.spawn(text(fonts, revalidate_note(&v.name), Type::META));
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
    if !custom {
        permissions_group(p, fonts, v);
    }
    areas::build(p, fonts, &v.name, &v.areas, &v.areas_error, &v.form);
}

/// *What the agent may do without asking*: Clúsia's rules for Claude Code and Codex. A custom
/// command has none of them, so the page leaves the group out.
fn permissions_group(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &HarnessView) {
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
        if v.kind == HarnessKind::ClaudeCode && !v.sandbox {
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
            probe_card(&ProbeState::Done(probe(true)), "Claude Code", false),
            ProbeCard::Ok {
                name: "Claude Code".into(),
                version: "2.1.294".into(),
                program: "/opt/homebrew/bin/claude".into(),
                seconds: "1.8".into(),
                account: None,
            }
        );
        assert_eq!(
            probe_card(&ProbeState::Done(probe(false)), "Claude Code", false),
            ProbeCard::Failed {
                name: "Claude Code".into(),
                message: "Run `claude` once in a terminal".into()
            }
        );
        let mut silent = probe(false);
        silent.error = None;
        assert_eq!(
            probe_card(&ProbeState::Done(silent), "Codex", false),
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
        let card = probe_card(&ProbeState::Done(answer), "Claude Code", false);
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
        assert_eq!(
            ok_title("Echo", "", "0.2", Some("protocol 1")),
            "Echo answered in 0.2 s · protocol 1"
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
            (v.kind, v.name.as_str(), v.program_name, v.switched.clone()),
            (HarnessKind::ClaudeCode, "Claude Code", "claude", None)
        );
        let mut snap = Snapshot::default();
        snap.config.harness.kind = HarnessKind::Codex;
        let v = view(&snap, &HashMap::new(), &ProbeState::Idle);
        assert_eq!(
            (v.kind, v.name.as_str(), v.program_name),
            (HarnessKind::Codex, "Codex", "codex")
        );
        let v = v.with_switched(&Switched(Some(HarnessKind::Codex)));
        assert_eq!(v.switched.as_deref(), Some("Codex"));
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
    fn the_three_kind_cards_can_be_chosen() {
        let mut app = harness_app();
        for needle in [
            "claude, with your settings",
            "codex app-server, with your login",
            "any program that speaks Clúsia's JSON lines",
        ] {
            assert!(crate::testing::shows(&mut app, needle), "{needle}");
        }
        assert!(!crate::testing::shows(
            &mut app,
            "Arrives in a later milestone"
        ));
        assert_eq!(
            crate::testing::count::<KindCard>(&mut app),
            3,
            "Claude Code, Codex and Custom command"
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

    fn custom_app() -> App {
        let mut app = harness_app();
        crate::testing::set_config_locally(&mut app, "harness.kind", "custom");
        crate::testing::set_config_locally(
            &mut app,
            "harness.program",
            "/usr/local/bin/echo-harness",
        );
        crate::testing::set_config_locally(&mut app, "harness.custom_name", "Echo");
        crate::testing::settle(&mut app);
        app
    }

    #[test]
    fn the_custom_card_writes_its_kind_and_clears_what_belonged_to_the_other_agent() {
        let mut app = harness_app();
        crate::testing::set_config_locally(&mut app, "harness.program", "/opt/homebrew/bin/claude");
        crate::testing::settle(&mut app);
        let custom = crate::testing::find::<KindCard>(&mut app, |c| c.0 == HarnessKind::Custom);
        crate::testing::activate(&mut app, custom);
        assert_eq!(
            crate::testing::recorded(&mut app),
            [
                Ask::SetConfig {
                    key: "harness.kind".into(),
                    value: "custom".into()
                },
                Ask::SetConfig {
                    key: "harness.program".into(),
                    value: String::new()
                },
            ]
        );
    }

    #[test]
    fn leaving_a_named_custom_command_clears_its_name_too() {
        let mut app = custom_app();
        let claude = crate::testing::find::<KindCard>(&mut app, |c| c.0 == HarnessKind::ClaudeCode);
        crate::testing::activate(&mut app, claude);
        assert_eq!(
            crate::testing::recorded(&mut app),
            [
                Ask::SetConfig {
                    key: "harness.kind".into(),
                    value: "claude-code".into()
                },
                Ask::SetConfig {
                    key: "harness.program".into(),
                    value: String::new()
                },
                Ask::SetConfig {
                    key: "harness.custom_name".into(),
                    value: String::new()
                },
            ],
            "the kind first, then what belonged to the custom command"
        );
    }

    #[test]
    fn an_answer_nobody_waits_for_is_dropped() {
        let mut app = harness_app();
        crate::testing::tell(&mut app, crate::bridge::Tell::Probe(probe(true)));
        assert_eq!(
            app.world().resource::<Model>().probe,
            ProbeState::Idle,
            "no Test is running, so a late answer shows nothing"
        );
    }

    #[test]
    fn a_new_program_in_the_snapshot_drops_the_test_result() {
        let mut app = harness_app();
        app.world_mut().resource_mut::<Model>().probe = ProbeState::Done(probe(true));
        let mut snapshot = app.world().resource::<Model>().snapshot.clone();
        crate::testing::tell(
            &mut app,
            crate::bridge::Tell::Snapshot(Box::new(snapshot.clone())),
        );
        assert!(
            matches!(app.world().resource::<Model>().probe, ProbeState::Done(_)),
            "a snapshot that changes nothing keeps the result"
        );
        snapshot.config.harness.program = Some("/Users/octo/bin/mine.py".into());
        crate::testing::tell(&mut app, crate::bridge::Tell::Snapshot(Box::new(snapshot)));
        assert_eq!(app.world().resource::<Model>().probe, ProbeState::Idle);
    }

    #[test]
    fn a_switch_drops_the_test_result_of_the_other_agent() {
        let mut app = harness_app();
        app.world_mut().resource_mut::<Model>().probe = ProbeState::Done(probe(true));
        crate::testing::settle(&mut app);
        assert!(crate::testing::shows(
            &mut app,
            "Claude Code 2.1.294 answered"
        ));
        let custom = crate::testing::find::<KindCard>(&mut app, |c| c.0 == HarnessKind::Custom);
        crate::testing::activate(&mut app, custom);
        crate::testing::set_config_locally(&mut app, "harness.kind", "custom");
        crate::testing::settle(&mut app);
        assert_eq!(
            app.world().resource::<Model>().probe,
            ProbeState::Idle,
            "the Claude Code result is not shown under Custom command"
        );
        assert!(crate::testing::shows(
            &mut app,
            "Press Test to check that Clúsia can find Custom command"
        ));
        assert!(!crate::testing::shows(&mut app, "Claude Code 2.1.294"));
    }

    #[test]
    fn choosing_custom_again_writes_nothing() {
        let mut app = custom_app();
        let custom = crate::testing::find::<KindCard>(&mut app, |c| c.0 == HarnessKind::Custom);
        crate::testing::activate(&mut app, custom);
        assert!(crate::testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn the_custom_page_shows_its_fields_notice_and_hints() {
        let mut app = custom_app();
        for needle in [
            "Required. Runs once per turn",
            "Passed to every turn",
            "Shown on the pages. Filled in by Test",
            SANDBOX_NOTICE,
            "How to write one",
            "Each check is one Echo turn on your account.",
            "Echo reads each draft comment",
            "Press Test to check that Clúsia can find Echo",
        ] {
            assert!(crate::testing::shows(&mut app, needle), "{needle}");
        }
        assert!(!crate::testing::shows(&mut app, "on your PATH"));
        assert!(!crate::testing::shows(&mut app, "Passed to every session"));
        assert_eq!(crate::testing::count::<CustomNotice>(&mut app), 1);
        let link = crate::testing::find::<crate::screens::config::OpenLink>(&mut app, |l| {
            l.0 == PROTOCOL_DOCS_URL
        });
        crate::testing::activate(&mut app, link);
        assert_eq!(
            app.world().resource::<crate::platform_open::OpenUrls>().0,
            [PROTOCOL_DOCS_URL]
        );
        let name = crate::testing::find::<crate::screens::config::ConfigField>(&mut app, |f| {
            f.0 == "harness.custom_name"
        });
        assert_eq!(
            app.world()
                .get::<crate::ui::kit::Field>(name)
                .unwrap()
                .committed,
            "Echo"
        );
    }

    #[test]
    fn the_name_field_writes_its_key() {
        let mut app = custom_app();
        let name = crate::testing::find::<crate::screens::config::ConfigField>(&mut app, |f| {
            f.0 == "harness.custom_name"
        });
        app.world_mut()
            .write_message(crate::ui::kit::FieldCommitted {
                entity: name,
                value: "  Ollama  ".into(),
            });
        app.update();
        assert_eq!(
            crate::testing::recorded(&mut app),
            [Ask::SetConfig {
                key: "harness.custom_name".into(),
                value: "Ollama".into()
            }]
        );
    }

    #[test]
    fn a_custom_command_hides_the_sandbox_and_claude_permission_rows() {
        let mut app = custom_app();
        for gone in [
            "What the agent may do without asking",
            "Read and search the review worktree",
            "Also allow what my Claude Code settings already allow",
            "Run commands in a sandbox: no network, writes only in the review worktree",
            "Codex always runs in its read-only sandbox",
        ] {
            assert!(!crate::testing::shows(&mut app, gone), "{gone}");
        }
        crate::testing::set_config_locally(&mut app, "harness.kind", "claude-code");
        crate::testing::settle(&mut app);
        for back in [
            "What the agent may do without asking",
            "Also allow what my Claude Code settings already allow",
        ] {
            assert!(crate::testing::shows(&mut app, back), "{back}");
        }
        assert_eq!(crate::testing::count::<CustomNotice>(&mut app), 0);
    }

    #[test]
    fn the_notice_belongs_to_custom_only() {
        let mut app = harness_app();
        assert!(!crate::testing::shows(&mut app, SANDBOX_NOTICE));
        assert!(crate::testing::shows(
            &mut app,
            "Looks for claude on your PATH"
        ));
        crate::testing::set_config_locally(&mut app, "harness.kind", "codex");
        crate::testing::settle(&mut app);
        assert!(!crate::testing::shows(&mut app, SANDBOX_NOTICE));
    }

    #[test]
    fn a_good_custom_test_names_the_program_and_the_protocol() {
        let answer = ProbeResult {
            ok: true,
            version: Some("1.0".into()),
            program: "/usr/local/bin/echo-harness".into(),
            elapsed_ms: 200,
            error: None,
            harness: "Echo".into(),
            signed_in: None,
            account: None,
        };
        let card = probe_card(&ProbeState::Done(answer), "Custom command", true);
        let ProbeCard::Ok {
            name,
            version,
            seconds,
            account,
            ..
        } = card
        else {
            panic!("ok")
        };
        assert_eq!(
            ok_title(&name, &version, &seconds, account.as_deref()),
            "Echo 1.0 answered in 0.2 s · protocol 1"
        );
    }

    #[test]
    fn a_failed_custom_test_keeps_the_daemons_words() {
        let answer = ProbeResult {
            ok: false,
            version: None,
            program: "/nope".into(),
            elapsed_ms: 5000,
            error: Some("Echo did not answer the probe in 5 s; it can still run turns".into()),
            harness: "Echo".into(),
            signed_in: None,
            account: None,
        };
        let card = probe_card(&ProbeState::Done(answer), "Custom command", true);
        assert_eq!(
            card,
            ProbeCard::Failed {
                name: "Echo".into(),
                message: "Echo did not answer the probe in 5 s; it can still run turns".into()
            }
        );
    }

    #[test]
    fn the_switched_note_names_the_custom_command() {
        let mut app = harness_app();
        let custom = crate::testing::find::<KindCard>(&mut app, |c| c.0 == HarnessKind::Custom);
        crate::testing::activate(&mut app, custom);
        crate::testing::set_config_locally(&mut app, "harness.kind", "custom");
        crate::testing::settle(&mut app);
        assert!(crate::testing::shows(
            &mut app,
            "Reviews you open from now on use Custom command. Open reviews keep their current agent."
        ));
    }

    #[test]
    fn the_custom_view_reads_its_settings() {
        let mut snap = Snapshot::default();
        snap.config.harness.kind = HarnessKind::Custom;
        snap.config.harness.custom_name = "Echo".into();
        let rejected: HashMap<String, String> = [(
            "harness.custom_name".to_string(),
            "harness.custom_name must be at most 40 characters, with no control characters"
                .to_string(),
        )]
        .into();
        let v = view(&snap, &rejected, &ProbeState::Idle);
        assert_eq!(
            (
                v.kind,
                v.name.as_str(),
                v.program_name,
                v.custom_name.as_str()
            ),
            (HarnessKind::Custom, "Echo", "", "Echo")
        );
        assert!(
            v.custom_name_error
                .as_deref()
                .unwrap()
                .contains("at most 40")
        );
        assert_eq!(program_hint(&v), "Required. Runs once per turn");
    }
}
