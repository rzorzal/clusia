//! Config › Harness (mockup `ConfigHarness.png`): the agent tool Clúsia drives, its settings, a
//! test, and what the agent may do without asking.

use std::collections::HashMap;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, observe};
use clusia_core::checks::AuditArea;
use clusia_core::config::{Harness, OnOpen};
use clusia_protocol::ProbeResult;

use super::{ConfigField, areas, field_row, option_card, page_header, row, setter};
use crate::bridge::{Ask, Asks, Model, ProbeState};
use crate::fonts::UiFonts;
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
        version: String,
        program: String,
        seconds: String,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct HarnessView {
    /// The configured path; empty when Clúsia looks on the PATH.
    pub program: String,
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
pub const COST_NOTE: &str = "Each check is one Claude Code turn on your account.";

/// The three checkboxes of *When I open a review*, in a column. Config and the first run
/// show the same ones.
pub fn open_checkboxes(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &OpenChecks) {
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(8),
        ..default()
    })
    .with_children(|c| {
        let on_open = if v.summarize { "wait" } else { "summarize" };
        let choices = [
            (
                "Summarize it",
                "harness.on_open",
                on_open.to_string(),
                v.summarize,
            ),
            (
                "Check security",
                "harness.check_security",
                (!v.security).to_string(),
                v.security,
            ),
            (
                "Audit the change",
                "harness.audit",
                (!v.audit).to_string(),
                v.audit,
            ),
        ];
        for (label, key, value, on) in choices {
            c.spawn(Node {
                column_gap: px(10),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|r| {
                r.spawn((checkbox(on), setter(key, value)));
                r.spawn(text(fonts, label, Type::BODY));
            });
        }
    });
}

impl HarnessView {
    /// The same view with the add/edit form, which lives in a resource of its own.
    pub fn with_form(mut self, form: &areas::AreaForm) -> Self {
        self.form = form.target.is_some().then(|| form.clone());
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
        probe: probe_card(probe),
        open: open_checks(h),
        check_timeout: h.check_timeout_secs.to_string(),
        check_timeout_error: refusal("harness.check_timeout_secs"),
        areas: h.audit_areas.clone(),
        areas_error: refusal(areas::AREAS_KEY),
        form: None,
    }
}

pub fn probe_card(state: &ProbeState) -> ProbeCard {
    match state {
        ProbeState::Idle => ProbeCard::Idle,
        ProbeState::Testing => ProbeCard::Testing,
        ProbeState::Done(ProbeResult {
            ok: true,
            version,
            program,
            elapsed_ms,
            ..
        }) => ProbeCard::Ok {
            version: version.clone().unwrap_or_default(),
            program: program.clone(),
            seconds: format!("{:.1}", *elapsed_ms as f64 / 1000.0),
        },
        ProbeState::Done(result) => ProbeCard::Failed {
            message: result
                .error
                .clone()
                .unwrap_or_else(|| "Claude Code did not answer".into()),
        },
    }
}

/// The hint next to the program field.
pub fn program_hint(v: &HarnessView) -> String {
    match &v.probe {
        ProbeCard::Ok { program, .. } if !program.is_empty() => format!("Found at {program}"),
        _ if v.program.is_empty() => "Looks for claude on your PATH".into(),
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
        r.spawn(option_card(true, 250.0)).with_children(|c| {
            title_with_radio(c, fonts, "Claude Code", Type::STRONG, true);
            c.spawn(text(fonts, "claude, with your settings", Type::META));
        });
        for (name, about) in [
            ("Codex", "codex CLI"),
            ("Custom command", "any tool that speaks JSON"),
        ] {
            r.spawn(card(Node {
                width: px(250),
                flex_direction: FlexDirection::Column,
                row_gap: px(4),
                padding: UiRect::axes(px(14), px(12)),
                ..default()
            }))
            .with_children(|c| {
                title_with_radio(c, fonts, name, Type::STRONG.ink(Swatch::Faint), false);
                c.spawn(text(fonts, about, Type::META));
                c.spawn(text(fonts, "Arrives in a later milestone", Type::META));
            });
        }
    });
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
        COST_NOTE,
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
    probe_row(p, fonts, &v.probe);
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
        c.spawn(Node {
            column_gap: px(10),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|r| {
            r.spawn((
                checkbox(v.sandbox),
                setter("harness.sandbox", (!v.sandbox).to_string()),
            ));
            r.spawn(text(
                fonts,
                "Run commands in a sandbox: no network, writes only in the review worktree",
                Type::BODY,
            ));
        });
        if !v.sandbox {
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
    areas::build(p, fonts, &v.areas, &v.areas_error, &v.form);
}

/// What turning the sandbox off lets an allowed command do.
const SANDBOX_OFF: &str = "With the sandbox off, an allowed command can reach the network, write outside the worktree and act through Clúsia as you: answer its own requests, change the draft or publish.";

fn probe_row(p: &mut ChildSpawnerCommands, fonts: &UiFonts, state: &ProbeCard) {
    let (fill, stroke) = match state {
        ProbeCard::Ok { .. } => (Swatch::GreenSoft, Swatch::Green),
        ProbeCard::Failed { .. } => (Swatch::OrangeSoft, Swatch::Orange),
        _ => (Swatch::Surface, Swatch::Line),
    };
    let (title, detail) = match state {
        ProbeCard::Idle => (
            "Not tested yet".to_string(),
            "Press Test to check that Clúsia can find Claude Code and see how fast it answers."
                .to_string(),
        ),
        ProbeCard::Testing => (
            "Testing Claude Code…".to_string(),
            "Waiting for an answer".to_string(),
        ),
        ProbeCard::Ok {
            version,
            program,
            seconds,
        } => (
            format!("Claude Code {version} answered in {seconds} s"),
            format!("Found at {program}"),
        ),
        ProbeCard::Failed { message } => {
            ("Claude Code did not answer".to_string(), message.clone())
        }
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

    fn probe(ok: bool) -> ProbeResult {
        ProbeResult {
            ok,
            version: ok.then(|| "2.1.294".to_string()),
            program: "/opt/homebrew/bin/claude".into(),
            elapsed_ms: 1800,
            error: (!ok).then(|| "Run `claude` once in a terminal".to_string()),
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
            probe_card(&ProbeState::Done(probe(true))),
            ProbeCard::Ok {
                version: "2.1.294".into(),
                program: "/opt/homebrew/bin/claude".into(),
                seconds: "1.8".into()
            }
        );
        assert_eq!(
            probe_card(&ProbeState::Done(probe(false))),
            ProbeCard::Failed {
                message: "Run `claude` once in a terminal".into()
            }
        );
        let mut silent = probe(false);
        silent.error = None;
        assert_eq!(
            probe_card(&ProbeState::Done(silent)),
            ProbeCard::Failed {
                message: "Claude Code did not answer".into()
            }
        );
    }

    #[test]
    fn the_program_hint_says_where_it_looks() {
        let mut v = view(&Snapshot::default(), &HashMap::new(), &ProbeState::Idle);
        assert_eq!(program_hint(&v), "Looks for claude on your PATH");
        v.program = "/usr/local/bin/claude".into();
        assert_eq!(program_hint(&v), "Using this path");
        v.probe = ProbeCard::Ok {
            version: "2.1.294".into(),
            program: "/usr/local/bin/claude".into(),
            seconds: "1.8".into(),
        };
        assert_eq!(program_hint(&v), "Found at /usr/local/bin/claude");
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
        use crate::screens::config::SetValue;
        let mut app = harness_app();
        for needle in [
            "When I open a review",
            "Summarize it",
            "Check security",
            "Audit the change",
            COST_NOTE,
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
            let e = crate::testing::find::<SetValue>(&mut app, |s| s.key == key);
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
        use crate::screens::config::SetValue;
        let mut app = harness_app();
        crate::testing::set_config_locally(&mut app, "harness.audit", "false");
        crate::testing::set_config_locally(&mut app, "harness.on_open", "wait");
        crate::testing::settle(&mut app);
        let audit = crate::testing::find::<SetValue>(&mut app, |s| s.key == "harness.audit");
        crate::testing::activate(&mut app, audit);
        let summarize = crate::testing::find::<SetValue>(&mut app, |s| s.key == "harness.on_open");
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
    fn the_check_timeout_is_a_field() {
        use crate::screens::config::ConfigField;
        use crate::ui::kit::Field;
        let mut app = harness_app();
        let e =
            crate::testing::find::<ConfigField>(&mut app, |f| f.0 == "harness.check_timeout_secs");
        assert_eq!(app.world().get::<Field>(e).unwrap().committed, "600");
    }
}
