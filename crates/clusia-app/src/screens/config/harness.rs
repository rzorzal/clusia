//! Config › Harness (mockup `ConfigHarness.png`): the agent tool Clúsia drives, its settings, a
//! test, and what the agent may do without asking.

use std::collections::HashMap;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, observe};
use clusia_core::config::OnOpen;
use clusia_protocol::ProbeResult;

use super::{ConfigField, field_row, option_card, page_header, row, segment_row, setter};
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
    pub program_error: Option<String>,
    pub args_error: Option<String>,
    pub on_open_error: Option<String>,
    pub timeout_error: Option<String>,
    pub permissions_error: Option<String>,
    pub probe: ProbeCard,
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
        probe: probe_card(probe),
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
    segment_row(
        p,
        fonts,
        "When I open a review",
        "harness.on_open",
        &[
            ("Summarize it", "summarize"),
            ("Wait for my first question", "wait"),
        ],
        match v.on_open {
            OnOpen::Summarize => "summarize",
            OnOpen::Wait => "wait",
        },
        "The summary uses your Claude Code usage",
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
    });
}

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

        let mut snap = Snapshot::default();
        snap.config.harness.program = Some("/opt/homebrew/bin/claude".into());
        snap.config.harness.extra_args = "--model claude-opus-5-5".into();
        snap.config.harness.on_open = OnOpen::Wait;
        snap.config.harness.turn_timeout_secs = 900;
        snap.config.harness.use_cli_permissions = false;
        let v = view(&snap, &HashMap::new(), &ProbeState::Testing);
        assert_eq!(v.program, "/opt/homebrew/bin/claude");
        assert_eq!(v.extra_args, "--model claude-opus-5-5");
        assert_eq!(v.on_open, OnOpen::Wait);
        assert_eq!(v.timeout, "900");
        assert!(!v.use_cli_permissions);
        assert_eq!(v.probe, ProbeCard::Testing);
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
}
