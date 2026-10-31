//! Config › Tests (mockup `Tests-config.png`): where a review's tests run, how much they may
//! use, the container engine Clúsia found and the disk the test environments keep.
//!
//! The engine and the disk come from the machine and its tools, so what they say is made
//! printable and drawn as plain text, never as a link.

use std::collections::HashMap;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, observe};
use clusia_core::printable::printable;
use clusia_core::tests::{DiskKind, RunnerChoice};

use super::{FieldError, field_row, option_card, page_header, row, setter};
use crate::bridge::{Ask, Asks, Connection, Model, set_config};
use crate::fonts::UiFonts;
use crate::nav::{Nav, Screen, Section};
use crate::screens::review::tests_tab::{TestMachine, engine_label, short_endpoint, size_label};
use crate::snapshot::Snapshot;
use crate::theme::Swatch;
use crate::ui::kit::{
    FieldCommitted, Type, Variant, button, card, disabled_button, divider, text, text_field,
    wrapped_text,
};
use crate::ui::modal::{BackdropClicked, escape_pressed, modal_card, modal_root};

const KEY_RUNNER: &str = "tests.runner";
const KEY_CPUS: &str = "tests.cpus";
const KEY_MEMORY: &str = "tests.memory_mb";
const KEY_TIMEOUT: &str = "tests.run_timeout_secs";
const KEY_PARALLEL: &str = "tests.parallel_runs";

const CLEAN_UP_TITLE: &str = "Clean up test environments?";
const CLEAN_UP_BODY: &str = "Removes the test images, cache volumes and caches Clúsia made for every repository, and test containers left behind. Base images and worktree outputs stay; worktree outputs go when their review is published or discarded.";

#[derive(Debug, Clone, PartialEq)]
pub struct TestsPageView {
    pub runner: RunnerChoice,
    pub runner_error: Option<String>,
    pub engine: EngineLine,
    pub checking: bool,
    pub cpus: String,
    pub cores: Option<u32>,
    pub cpus_error: Option<String>,
    pub memory: String,
    pub memory_error: Option<String>,
    pub minutes: String,
    pub minutes_error: Option<String>,
    pub parallel: String,
    pub parallel_error: Option<String>,
    pub disk: DiskLine,
    pub cleaning: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EngineLine {
    Checking,
    Found {
        label: String,
        endpoint: String,
        running: bool,
        note: Option<String>,
    },
    NotFound {
        sandbox_ok: bool,
        note: Option<String>,
    },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiskLine {
    Checking,
    Failed(String),
    Empty,
    Kept {
        parts: Vec<String>,
        total: String,
        note: Option<String>,
    },
}

/// A runner card: a click writes its choice to `tests.runner`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunnerCard(pub RunnerChoice);

/// The "Stop a run after" field: its text is minutes, the key holds seconds.
#[derive(Component)]
pub struct MinutesField;

/// The page's own "Check again" for the container engine.
#[derive(Component)]
pub struct CheckEngineAgain;

#[derive(Component)]
pub struct CleanUpButton;

#[derive(Component)]
pub struct ConfirmCleanUp;

#[derive(Component)]
pub struct CancelCleanUp;

/// The confirm's full-window layer.
#[derive(Component)]
pub struct CleanUpModal;

pub fn view(
    snap: &Snapshot,
    rejected: &HashMap<String, String>,
    machine: &TestMachine,
    home: Option<&str>,
) -> TestsPageView {
    let tests = &snap.config.tests;
    let cores = match &machine.engine {
        Some(Ok(info)) => Some(info.cores),
        _ => None,
    };
    let cpus = match cores {
        Some(cores) => tests.cpus_for(cores).to_string(),
        None if tests.cpus == 0 => "auto".to_string(),
        None => tests.cpus.to_string(),
    };
    let engine = match &machine.engine {
        None => EngineLine::Checking,
        Some(Err(message)) => EngineLine::Failed(printable(message)),
        Some(Ok(info)) => {
            let note = info.note.as_deref().map(printable);
            match &info.engine {
                Some(e) => EngineLine::Found {
                    label: engine_label(e),
                    endpoint: printable(&short_endpoint(&e.endpoint, home)),
                    running: e.running,
                    note,
                },
                None => EngineLine::NotFound {
                    sandbox_ok: info.sandbox_ok,
                    note,
                },
            }
        }
    };
    let error = |key: &str| rejected.get(key).cloned();
    TestsPageView {
        runner: tests.runner,
        runner_error: error(KEY_RUNNER),
        engine,
        checking: machine.checking,
        cpus,
        cores,
        cpus_error: error(KEY_CPUS),
        memory: tests.memory_mb.to_string(),
        memory_error: error(KEY_MEMORY),
        minutes: ((tests.run_timeout_secs + 30) / 60).to_string(),
        minutes_error: error(KEY_TIMEOUT),
        parallel: tests.parallel_runs.to_string(),
        parallel_error: error(KEY_PARALLEL),
        disk: disk_line(machine),
        cleaning: machine.cleaning,
    }
}

fn disk_line(machine: &TestMachine) -> DiskLine {
    let report = match &machine.disk {
        None => return DiskLine::Checking,
        Some(Err(message)) => return DiskLine::Failed(printable(message)),
        Some(Ok(report)) => report,
    };
    if report.total_bytes == 0 {
        return DiskLine::Empty;
    }
    let (mut images, mut image_bytes, mut cache_bytes, mut output_bytes) = (0u64, 0u64, 0u64, 0u64);
    for item in &report.items {
        match item.kind {
            DiskKind::Image => {
                images += 1;
                image_bytes = image_bytes.saturating_add(item.bytes);
            }
            DiskKind::Volume | DiskKind::Cache => {
                cache_bytes = cache_bytes.saturating_add(item.bytes);
            }
            DiskKind::WorktreeDir => output_bytes = output_bytes.saturating_add(item.bytes),
        }
    }
    let mut parts = Vec::new();
    if image_bytes > 0 {
        let noun = if images == 1 { "image" } else { "images" };
        parts.push(format!("{images} {noun} {}", size_label(image_bytes)));
    }
    if cache_bytes > 0 {
        parts.push(format!("caches {}", size_label(cache_bytes)));
    }
    if output_bytes > 0 {
        parts.push(format!("worktree outputs {}", size_label(output_bytes)));
    }
    DiskLine::Kept {
        parts,
        total: size_label(report.total_bytes),
        note: report.note.as_deref().map(printable),
    }
}

pub fn build(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &TestsPageView) {
    page_header(
        p,
        fonts,
        "Tests",
        "How a review's tests run, how much they may use, and what they leave on this machine.",
    );
    let panel = || Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(16),
        padding: UiRect::all(px(16)),
        ..default()
    };
    p.spawn(card(panel()))
        .with_children(|c| where_card(c, fonts, v));
    p.spawn(card(panel()))
        .with_children(|c| limits_card(c, fonts, v));
    p.spawn(card(panel()))
        .with_children(|c| disk_card(c, fonts, v));
}

fn where_card(c: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &TestsPageView) {
    c.spawn(text(fonts, "Where tests run", Type::STRONG));
    c.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(4),
        ..default()
    })
    .with_children(|e| {
        e.spawn(Node {
            column_gap: px(12),
            row_gap: px(12),
            flex_wrap: FlexWrap::Wrap,
            ..default()
        })
        .with_children(|cards| {
            for (choice, name, about, value) in [
                (
                    RunnerChoice::Auto,
                    "Automatic",
                    "A container when an engine is found, else the macOS sandbox",
                    "auto",
                ),
                (
                    RunnerChoice::Container,
                    "Always a container",
                    "Refuse to run without an engine",
                    "container",
                ),
                (
                    RunnerChoice::Local,
                    "Always local",
                    "macOS sandbox in the worktree",
                    "local",
                ),
            ] {
                cards
                    .spawn((
                        option_card(v.runner == choice, 270.0),
                        setter(KEY_RUNNER, value),
                        RunnerCard(choice),
                    ))
                    .with_children(|k| {
                        k.spawn(text(fonts, name, Type::STRONG));
                        k.spawn(wrapped_text(fonts, about, Type::META));
                    });
            }
        });
        if let Some(message) = &v.runner_error {
            e.spawn((
                FieldError(KEY_RUNNER),
                text(fonts, message.clone(), Type::BODY.ink(Swatch::Orange)),
            ));
        }
    });
    engine_line(c, fonts, v);
}

fn engine_line(c: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &TestsPageView) {
    c.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(4),
        ..default()
    })
    .with_children(|col| {
        let mut note = None;
        col.spawn(Node {
            column_gap: px(8),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|r| {
            match &v.engine {
                EngineLine::Checking => {
                    r.spawn(text(fonts, "Checking…", Type::MUTED));
                }
                EngineLine::Found {
                    label,
                    endpoint,
                    running,
                    note: n,
                } => {
                    let (word, ink) = if *running {
                        ("running", Swatch::Green)
                    } else {
                        ("stopped", Swatch::Orange)
                    };
                    r.spawn(text(fonts, "✓", Type::STRONG.ink(Swatch::Green)));
                    r.spawn(text(fonts, "Found", Type::BODY));
                    r.spawn(text(fonts, label.clone(), Type::STRONG));
                    r.spawn(text(fonts, endpoint.clone(), Type::MONO));
                    r.spawn(text(fonts, word, Type::BODY.ink(ink)));
                    note = n.clone();
                }
                EngineLine::NotFound {
                    sandbox_ok,
                    note: n,
                } => {
                    r.spawn(text(fonts, "No container engine found", Type::STRONG));
                    if *sandbox_ok {
                        r.spawn(text(fonts, "macOS sandbox ready", Type::MUTED));
                    } else {
                        note = n.clone();
                    }
                }
                EngineLine::Failed(message) => {
                    r.spawn(text(
                        fonts,
                        format!("Could not check: {message}"),
                        Type::BODY.ink(Swatch::Orange),
                    ));
                }
            }
            r.spawn(Node {
                flex_grow: 1.0,
                ..default()
            });
            if v.checking {
                r.spawn(disabled_button(fonts, "Checking…"));
            } else {
                r.spawn((
                    button(fonts, "Check again", Variant::Secondary),
                    CheckEngineAgain,
                    observe(on_check_again),
                ));
            }
        });
        if let Some(note) = note {
            col.spawn(wrapped_text(fonts, note, Type::META));
        }
    });
}

fn limits_card(c: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &TestsPageView) {
    c.spawn(text(fonts, "Limits", Type::STRONG));
    let of_cores = v.cores.map(|n| format!("of {n}")).unwrap_or_default();
    field_row(
        c,
        fonts,
        "CPUs per run",
        KEY_CPUS,
        &v.cpus,
        140.0,
        &of_cores,
        &v.cpus_error,
    );
    field_row(
        c,
        fonts,
        "Memory per run",
        KEY_MEMORY,
        &v.memory,
        140.0,
        "MB",
        &v.memory_error,
    );
    row(
        c,
        fonts,
        "Stop a run after",
        |r| {
            r.spawn((text_field(fonts, &v.minutes, 140.0, false), MinutesField));
        },
        "min",
        v.minutes_error.as_deref().map(|m| (KEY_TIMEOUT, m)),
    );
    field_row(
        c,
        fonts,
        "Runs at the same time",
        KEY_PARALLEL,
        &v.parallel,
        140.0,
        "",
        &v.parallel_error,
    );
}

fn disk_card(c: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &TestsPageView) {
    c.spawn(Node {
        column_gap: px(16),
        align_items: AlignItems::Center,
        ..default()
    })
    .with_children(|r| {
        r.spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(4),
            flex_grow: 1.0,
            flex_shrink: 1.0,
            ..default()
        })
        .with_children(|l| {
            l.spawn(text(fonts, "Disk used by test environments", Type::STRONG));
            match &v.disk {
                DiskLine::Checking => {
                    l.spawn(text(fonts, "Checking…", Type::MUTED));
                }
                DiskLine::Failed(message) => {
                    l.spawn(wrapped_text(
                        fonts,
                        message.clone(),
                        Type::BODY.ink(Swatch::Orange),
                    ));
                }
                DiskLine::Empty => {
                    l.spawn(text(fonts, "Nothing kept yet", Type::MUTED));
                }
                DiskLine::Kept { parts, note, .. } => {
                    l.spawn(wrapped_text(fonts, parts.join(" · "), Type::MUTED));
                    if let Some(note) = note {
                        l.spawn(wrapped_text(fonts, note.clone(), Type::META));
                    }
                }
            }
        });
        if let DiskLine::Kept { total, .. } = &v.disk {
            r.spawn(text(fonts, total.clone(), Type::HEADING));
        }
        if matches!(v.disk, DiskLine::Kept { .. }) && !v.cleaning {
            r.spawn((
                button(fonts, "Clean up…", Variant::Secondary),
                CleanUpButton,
                observe(on_clean_up),
            ));
        } else {
            let label = if v.cleaning {
                "Cleaning…"
            } else {
                "Clean up…"
            };
            r.spawn(disabled_button(fonts, label));
        }
    });
}

fn on_check_again(
    activate: On<Activate>,
    buttons: Query<(), With<CheckEngineAgain>>,
    mut machine: ResMut<TestMachine>,
    mut asks: ResMut<Asks>,
) {
    if !buttons.contains(activate.entity) || machine.checking {
        return;
    }
    machine.checking = true;
    asks.send(Ask::CheckTestEngine { pr: None });
}

fn on_clean_up(
    activate: On<Activate>,
    buttons: Query<(), With<CleanUpButton>>,
    mut machine: ResMut<TestMachine>,
) {
    if buttons.contains(activate.entity) {
        machine.confirm = true;
    }
}

fn on_confirm(
    activate: On<Activate>,
    buttons: Query<(), With<ConfirmCleanUp>>,
    mut machine: ResMut<TestMachine>,
    mut asks: ResMut<Asks>,
) {
    if !buttons.contains(activate.entity) || !machine.confirm {
        return;
    }
    machine.confirm = false;
    machine.cleaning = true;
    asks.send(Ask::CleanUpTests { repo: None });
}

fn on_cancel(
    activate: On<Activate>,
    buttons: Query<(), With<CancelCleanUp>>,
    mut machine: ResMut<TestMachine>,
) {
    if buttons.contains(activate.entity) {
        machine.confirm = false;
    }
}

/// Asks the machine for its engine and its disk the first time the page is shown in this
/// session, and again after a reconnect (which clears `asked`).
pub fn ask_machine(
    nav: Res<Nav>,
    model: Res<Model>,
    mut machine: ResMut<TestMachine>,
    mut asks: ResMut<Asks>,
) {
    if nav.screen != Screen::Config(Section::Tests)
        || model.connection != Connection::Live
        || machine.asked
    {
        return;
    }
    machine.asked = true;
    machine.checking = true;
    asks.send(Ask::CheckTestEngine { pr: None });
    asks.send(Ask::GetTestDisk);
}

/// The minutes field is not a plain config field: its text is minutes and the key holds seconds.
pub fn commit_minutes(
    mut commits: MessageReader<FieldCommitted>,
    fields: Query<(), With<MinutesField>>,
    mut asks: ResMut<Asks>,
    mut model: ResMut<Model>,
) {
    for commit in commits.read() {
        if !fields.contains(commit.entity) {
            continue;
        }
        let seconds = commit
            .value
            .trim()
            .parse::<u32>()
            .ok()
            .and_then(|minutes| minutes.checked_mul(60));
        match seconds {
            Some(seconds) => set_config(&mut asks, &mut model, KEY_TIMEOUT, seconds.to_string()),
            None => {
                model
                    .rejected
                    .insert(KEY_TIMEOUT.to_string(), "Enter whole minutes".to_string());
            }
        }
    }
}

/// Shows the confirm while `machine.confirm` holds on this page, and takes it away when either
/// stops holding.
pub fn sync_confirm(
    mut commands: Commands,
    nav: Res<Nav>,
    machine: Res<TestMachine>,
    fonts: Res<UiFonts>,
    layers: Query<Entity, With<CleanUpModal>>,
) {
    let wanted = machine.confirm && nav.screen == Screen::Config(Section::Tests);
    match (wanted, layers.iter().next()) {
        (false, Some(layer)) => commands.entity(layer).despawn(),
        (true, None) => spawn_confirm(&mut commands, &fonts),
        _ => {}
    }
}

fn spawn_confirm(commands: &mut Commands, fonts: &UiFonts) {
    commands
        .spawn((modal_root(), CleanUpModal))
        .with_children(|root| {
            root.spawn(modal_card(460.0)).with_children(|card| {
                card.spawn(Node {
                    flex_direction: FlexDirection::Column,
                    row_gap: px(8),
                    padding: UiRect::axes(px(24), px(20)),
                    ..default()
                })
                .with_children(|c| {
                    c.spawn(text(fonts, CLEAN_UP_TITLE, Type::HEADING));
                    c.spawn(wrapped_text(fonts, CLEAN_UP_BODY, Type::MUTED));
                });
                card.spawn(divider());
                card.spawn(Node {
                    padding: UiRect::axes(px(24), px(14)),
                    column_gap: px(10),
                    align_items: AlignItems::Center,
                    ..default()
                })
                .with_children(|f| {
                    f.spawn((
                        button(fonts, "Cancel", Variant::Secondary),
                        CancelCleanUp,
                        observe(on_cancel),
                    ));
                    f.spawn(Node {
                        flex_grow: 1.0,
                        ..default()
                    });
                    f.spawn((
                        button(fonts, "Clean up", Variant::Danger),
                        ConfirmCleanUp,
                        observe(on_confirm),
                    ));
                });
            });
        });
}

/// Esc or a click outside closes the confirm and sends nothing.
pub fn cancel_confirm(
    keys: Res<ButtonInput<KeyCode>>,
    mut clicks: MessageReader<BackdropClicked>,
    layers: Query<(), With<CleanUpModal>>,
    mut machine: ResMut<TestMachine>,
) {
    let outside = clicks.read().any(|c| layers.contains(c.0));
    if machine.confirm && (escape_pressed(&keys) || outside) {
        machine.confirm = false;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use bevy::input::ButtonInput;
    use clusia_core::config::Tests;
    use clusia_core::printable::printable;
    use clusia_core::tests::{
        DiskItem, DiskKind, DiskReport, Engine, EngineKind, RunnerChoice, TestEngineInfo,
    };
    use clusia_protocol::WindowTarget;

    use super::*;
    use crate::bridge::{Ask, Tell};
    use crate::fixture;
    use crate::nav::{Nav, Section};
    use crate::screens::config::{ConfigField, FieldError, OpenLink, PageOf};
    use crate::snapshot::Snapshot;
    use crate::testing::{self, NOW};
    use crate::theme::Swatch;
    use crate::ui::kit::{Field, Fill};
    use crate::ui::modal::BackdropClicked;

    const HOME: &str = "/Users/octo";

    fn machine() -> TestMachine {
        TestMachine {
            engine: Some(Ok(fixture::demo_test_engine())),
            disk: Some(Ok(fixture::demo_test_disk())),
            asked: true,
            ..TestMachine::default()
        }
    }

    fn page(machine: &TestMachine) -> TestsPageView {
        view(&Snapshot::default(), &HashMap::new(), machine, Some(HOME))
    }

    fn disk(items: Vec<(DiskKind, u64)>, total: u64, note: Option<&str>) -> TestMachine {
        TestMachine {
            disk: Some(Ok(DiskReport {
                items: items
                    .into_iter()
                    .enumerate()
                    .map(|(n, (kind, bytes))| DiskItem {
                        kind,
                        name: format!("item-{n}"),
                        repo: Some("rzorzal/clusia".into()),
                        bytes,
                    })
                    .collect(),
                total_bytes: total,
                note: note.map(String::from),
            })),
            ..TestMachine::default()
        }
    }

    #[test]
    fn the_page_reads_the_config_and_the_machine() {
        let v = page(&machine());
        assert_eq!(v.runner, RunnerChoice::Auto);
        assert_eq!(v.cores, Some(10));
        assert_eq!(
            (
                v.cpus.as_str(),
                v.memory.as_str(),
                v.minutes.as_str(),
                v.parallel.as_str()
            ),
            ("5", "4096", "30", "1")
        );
        assert_eq!(
            v.engine,
            EngineLine::Found {
                label: "Docker · Colima".into(),
                endpoint: "~/.colima/default/docker.sock".into(),
                running: true,
                note: None,
            }
        );
        assert_eq!(
            v.disk,
            DiskLine::Kept {
                parts: vec![
                    "3 images 4.1 GB".into(),
                    "caches 2.3 GB".into(),
                    "worktree outputs 800 MB".into(),
                ],
                total: "7.2 GB".into(),
                note: None,
            }
        );
        assert!(!v.checking && !v.cleaning);
        assert_eq!(v.minutes_error, None);
    }

    #[test]
    fn nothing_is_known_until_the_machine_answers() {
        let v = page(&TestMachine::default());
        assert_eq!(
            (v.engine, v.disk, v.cores),
            (EngineLine::Checking, DiskLine::Checking, None)
        );
        assert_eq!(v.cpus, "auto", "no cores known and cpus 0 means auto");
        let mut snap = Snapshot::default();
        snap.config.tests.cpus = 3;
        let v = view(&snap, &HashMap::new(), &TestMachine::default(), Some(HOME));
        assert_eq!(v.cpus, "3");
    }

    #[test]
    fn the_minutes_are_the_timeout_rounded() {
        for (secs, shown) in [
            (1800, "30"),
            (60, "1"),
            (90, "2"),
            (61, "1"),
            (14_400, "240"),
        ] {
            let mut snap = Snapshot::default();
            snap.config.tests.run_timeout_secs = secs;
            let v = view(&snap, &HashMap::new(), &machine(), Some(HOME));
            assert_eq!(v.minutes, shown, "{secs} s");
        }
    }

    #[test]
    fn a_pinned_cpu_count_is_shown_as_it_is_and_auto_follows_the_cores() {
        let mut snap = Snapshot::default();
        snap.config.tests = Tests {
            cpus: 3,
            ..Tests::default()
        };
        assert_eq!(
            view(&snap, &HashMap::new(), &machine(), Some(HOME)).cpus,
            "3"
        );
        let mut info = fixture::demo_test_engine();
        info.cores = 1;
        let one = TestMachine {
            engine: Some(Ok(info)),
            ..machine()
        };
        assert_eq!(page(&one).cpus, "1", "at least one");
    }

    #[test]
    fn refusals_show_under_their_own_field() {
        let rejected: HashMap<String, String> = [
            ("tests.runner", "no"),
            ("tests.cpus", "a"),
            ("tests.memory_mb", "b"),
            ("tests.run_timeout_secs", "c"),
            ("tests.parallel_runs", "d"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let v = view(&Snapshot::default(), &rejected, &machine(), Some(HOME));
        assert_eq!(
            (
                v.runner_error,
                v.cpus_error,
                v.memory_error,
                v.minutes_error,
                v.parallel_error
            ),
            (
                Some("no".into()),
                Some("a".into()),
                Some("b".into()),
                Some("c".into()),
                Some("d".into())
            )
        );
    }

    fn engine_of(running: bool, endpoint: &str) -> Engine {
        Engine {
            kind: EngineKind::Colima,
            cli: "/opt/homebrew/bin/docker".into(),
            endpoint: endpoint.into(),
            context: None,
            running,
            start: Some("colima start".into()),
            compose: None,
        }
    }

    #[test]
    fn a_stopped_engine_a_missing_one_and_a_failed_check_each_have_their_line() {
        let stopped = TestMachine {
            engine: Some(Ok(TestEngineInfo {
                engine: Some(engine_of(
                    false,
                    "unix:///Users/octo/.colima/default/docker.sock",
                )),
                sandbox_ok: true,
                note: Some("Colima is stopped. Start it with: colima start".into()),
                cores: 8,
            })),
            ..TestMachine::default()
        };
        assert_eq!(
            page(&stopped).engine,
            EngineLine::Found {
                label: "Docker · Colima".into(),
                endpoint: "~/.colima/default/docker.sock".into(),
                running: false,
                note: Some("Colima is stopped. Start it with: colima start".into()),
            }
        );
        let none = TestMachine {
            engine: Some(Ok(TestEngineInfo {
                engine: None,
                sandbox_ok: true,
                note: None,
                cores: 8,
            })),
            ..TestMachine::default()
        };
        assert_eq!(
            page(&none).engine,
            EngineLine::NotFound {
                sandbox_ok: true,
                note: None
            }
        );
        let failed = TestMachine {
            engine: Some(Err("Not connected to clusiad".into())),
            ..TestMachine::default()
        };
        assert_eq!(
            page(&failed).engine,
            EngineLine::Failed("Not connected to clusiad".into())
        );
        assert_eq!(page(&failed).cores, None);
    }

    #[test]
    fn the_disk_reads_in_parts_and_leaves_out_what_is_zero() {
        let one_image = disk(vec![(DiskKind::Image, 1_200_000_000)], 1_200_000_000, None);
        assert_eq!(
            page(&one_image).disk,
            DiskLine::Kept {
                parts: vec!["1 image 1.2 GB".into()],
                total: "1.2 GB".into(),
                note: None,
            }
        );
        let caches_only = disk(
            vec![(DiskKind::Volume, 400_000), (DiskKind::Cache, 100_000)],
            500_000,
            Some("The engine is stopped: images are not counted"),
        );
        assert_eq!(
            page(&caches_only).disk,
            DiskLine::Kept {
                parts: vec!["caches 500 KB".into()],
                total: "500 KB".into(),
                note: Some("The engine is stopped: images are not counted".into()),
            }
        );
        assert_eq!(page(&disk(Vec::new(), 0, None)).disk, DiskLine::Empty);
        let failed = TestMachine {
            disk: Some(Err("A review is busy".into())),
            ..TestMachine::default()
        };
        assert_eq!(
            page(&failed).disk,
            DiskLine::Failed("A review is busy".into())
        );
    }

    #[test]
    fn machine_text_is_made_printable_before_it_is_drawn() {
        let machine = TestMachine {
            engine: Some(Ok(TestEngineInfo {
                engine: Some(engine_of(false, "unix:///tmp/a\nb.sock")),
                sandbox_ok: true,
                note: Some("Start it: [x](https://evil.example)\n\u{202e}gnp".into()),
                cores: 4,
            })),
            ..disk(vec![(DiskKind::Cache, 1_000)], 1_000, Some("tab\there"))
        };
        let v = page(&machine);
        let EngineLine::Found { endpoint, note, .. } = v.engine else {
            panic!("found");
        };
        assert!(!endpoint.contains('\n') && endpoint.contains(&printable("\n")));
        let note = note.unwrap();
        assert!(!note.contains('\n') && !note.contains('\u{202e}'), "{note}");
        assert!(
            note.contains("[x](https://evil.example)"),
            "the text stays, as text"
        );
        let DiskLine::Kept { note, .. } = v.disk else {
            panic!("kept")
        };
        assert!(!note.unwrap().contains('\t'));
    }

    fn tests_app() -> App {
        let mut app = testing::app(fixture::demo(NOW));
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Config);
        app.world_mut()
            .resource_mut::<Nav>()
            .open_section(Section::Tests);
        testing::settle(&mut app);
        app
    }

    /// The machine's two answers, as the daemon gives them.
    fn answered(app: &mut App) {
        testing::tell(app, Tell::TestEngine(Ok(fixture::demo_test_engine())));
        testing::tell(app, Tell::TestDisk(Ok(fixture::demo_test_disk())));
        testing::settle(app);
    }

    #[test]
    fn entering_the_page_asks_the_machine_once_per_session() {
        let mut app = tests_app();
        let asks = testing::recorded(&mut app);
        assert_eq!(
            asks.iter()
                .filter(|a| matches!(a, Ask::CheckTestEngine { pr: None }))
                .count(),
            1
        );
        assert_eq!(
            asks.iter()
                .filter(|a| matches!(a, Ask::GetTestDisk))
                .count(),
            1
        );
        assert!(app.world().resource::<TestMachine>().checking);
        answered(&mut app);
        app.world_mut()
            .resource_mut::<Nav>()
            .open_section(Section::Harness);
        testing::settle(&mut app);
        app.world_mut()
            .resource_mut::<Nav>()
            .open_section(Section::Tests);
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).is_empty(), "asked once");
        testing::tell(&mut app, Tell::Lost("gone".into()));
        testing::tell(&mut app, Tell::Snapshot(Box::new(fixture::demo(NOW))));
        testing::settle(&mut app);
        let again = testing::recorded(&mut app);
        assert!(
            again.contains(&Ask::GetTestDisk),
            "after a reconnect: {again:?}"
        );
    }

    #[test]
    fn the_page_matches_the_mockup() {
        let mut app = tests_app();
        answered(&mut app);
        testing::find::<PageOf>(&mut app, |p| p.0 == Section::Tests);
        for text in [
            "Tests",
            "Where tests run",
            "Automatic",
            "A container when an engine is found, else the macOS sandbox",
            "Always a container",
            "Refuse to run without an engine",
            "Always local",
            "macOS sandbox in the worktree",
            "Found",
            "Docker · Colima",
            "docker.sock",
            "running",
            "Check again",
            "Limits",
            "CPUs per run",
            "of 10",
            "Memory per run",
            "MB",
            "Stop a run after",
            "min",
            "Runs at the same time",
            "Disk used by test environments",
            "3 images 4.1 GB · caches 2.3 GB · worktree outputs 800 MB",
            "7.2 GB",
            "Clean up…",
        ] {
            assert!(testing::shows(&mut app, text), "{text}");
        }
    }

    #[test]
    fn a_runner_card_writes_its_choice_and_the_configured_one_is_selected() {
        let mut app = tests_app();
        answered(&mut app);
        testing::recorded(&mut app);
        let container = testing::find::<RunnerCard>(&mut app, |c| c.0 == RunnerChoice::Container);
        testing::activate(&mut app, container);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::SetConfig {
                key: "tests.runner".into(),
                value: "container".into()
            }]
        );
        testing::set_config_locally(&mut app, "tests.runner", "local");
        let selected = |app: &mut App, choice: RunnerChoice| {
            let card = testing::find::<RunnerCard>(app, |c| c.0 == choice);
            app.world().get::<Fill>(card).map(|f| f.0)
        };
        assert_eq!(
            selected(&mut app, RunnerChoice::Local),
            Some(Swatch::GreenTint)
        );
        assert_eq!(
            selected(&mut app, RunnerChoice::Auto),
            Some(Swatch::Surface)
        );
    }

    fn commit(app: &mut App, field: Entity, value: &str) {
        app.world_mut()
            .write_message(crate::ui::kit::FieldCommitted {
                entity: field,
                value: value.into(),
            });
        app.update();
    }

    #[test]
    fn the_limit_fields_write_their_keys() {
        let mut app = tests_app();
        answered(&mut app);
        testing::recorded(&mut app);
        for (key, value) in [
            ("tests.cpus", "3"),
            ("tests.memory_mb", "8192"),
            ("tests.parallel_runs", "2"),
        ] {
            let field = testing::find::<ConfigField>(&mut app, |f| f.0 == key);
            commit(&mut app, field, &format!(" {value} "));
            assert_eq!(
                testing::recorded(&mut app),
                [Ask::SetConfig {
                    key: key.into(),
                    value: value.into()
                }],
                "{key}"
            );
        }
    }

    #[test]
    fn an_untouched_cpu_field_writes_nothing_so_auto_stays() {
        let mut app = tests_app();
        answered(&mut app);
        let cpus = testing::find::<ConfigField>(&mut app, |f| f.0 == "tests.cpus");
        let mut field = app.world_mut().get_mut::<Field>(cpus).unwrap();
        assert_eq!(
            field.committed, "5",
            "the field starts as the half of the cores it shows"
        );
        assert_eq!(crate::ui::kit::commit(&mut field, "5".into()), None);
    }

    #[test]
    fn minutes_are_written_as_seconds_and_a_wrong_value_is_not_sent() {
        let mut app = tests_app();
        answered(&mut app);
        testing::recorded(&mut app);
        let minutes = testing::find::<MinutesField>(&mut app, |_| true);
        commit(&mut app, minutes, "45");
        testing::settle(&mut app);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::SetConfig {
                key: "tests.run_timeout_secs".into(),
                value: "2700".into()
            }]
        );
        for bad in ["", "1.5", "abc", "-3", "99999999999", "71582789"] {
            // The error changes the view, so the page and its field are built again.
            let minutes = testing::find::<MinutesField>(&mut app, |_| true);
            commit(&mut app, minutes, bad);
            assert!(
                testing::recorded(&mut app).is_empty(),
                "{bad:?} is not sent"
            );
            testing::settle(&mut app);
            assert!(testing::shows(&mut app, "Enter whole minutes"), "{bad:?}");
        }
        let minutes = testing::find::<MinutesField>(&mut app, |_| true);
        commit(&mut app, minutes, "20");
        testing::settle(&mut app);
        assert_eq!(testing::recorded(&mut app).len(), 1);
        assert!(
            !testing::shows(&mut app, "Enter whole minutes"),
            "a good value clears it"
        );
    }

    #[test]
    fn a_daemon_refusal_shows_under_its_field() {
        let mut app = tests_app();
        answered(&mut app);
        testing::tell(
            &mut app,
            Tell::Rejected {
                key: "tests.memory_mb".into(),
                message: "tests.memory_mb must be between 512 and 262144, got 3".into(),
            },
        );
        testing::settle(&mut app);
        assert!(testing::shows(
            &mut app,
            "tests.memory_mb must be between 512 and 262144, got 3"
        ));
        testing::find::<FieldError>(&mut app, |f| f.0 == "tests.memory_mb");
    }

    #[test]
    fn check_again_asks_once_and_waits_for_the_answer() {
        let mut app = tests_app();
        answered(&mut app);
        testing::recorded(&mut app);
        let button = testing::find::<CheckEngineAgain>(&mut app, |_| true);
        testing::activate(&mut app, button);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::CheckTestEngine { pr: None }]
        );
        testing::settle(&mut app);
        assert!(app.world().resource::<TestMachine>().checking);
        assert_eq!(
            testing::count::<CheckEngineAgain>(&mut app),
            0,
            "no second click while it checks"
        );
        assert!(testing::shows(&mut app, "Checking…"));
        answered(&mut app);
        assert_eq!(testing::count::<CheckEngineAgain>(&mut app), 1);
    }

    #[test]
    fn with_nothing_kept_clean_up_is_disabled() {
        let mut app = tests_app();
        testing::tell(&mut app, Tell::TestEngine(Ok(fixture::demo_test_engine())));
        testing::tell(
            &mut app,
            Tell::TestDisk(Ok(DiskReport {
                items: Vec::new(),
                total_bytes: 0,
                note: None,
            })),
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "Nothing kept yet"));
        assert!(testing::shows(&mut app, "Clean up…"));
        assert_eq!(testing::count::<CleanUpButton>(&mut app), 0);
    }

    fn open_confirm(app: &mut App) {
        let button = testing::find::<CleanUpButton>(app, |_| true);
        testing::activate(app, button);
        testing::settle(app);
    }

    #[test]
    fn clean_up_asks_first_and_only_the_confirm_sends() {
        let mut app = tests_app();
        answered(&mut app);
        testing::recorded(&mut app);
        open_confirm(&mut app);
        assert!(testing::shows(&mut app, "Clean up test environments?"));
        assert!(testing::shows(
            &mut app,
            "Removes the test images, cache volumes and caches Clúsia made for every repository, and test containers left behind. Base images and worktree outputs stay; worktree outputs go when their review is published or discarded."
        ));
        assert!(
            testing::recorded(&mut app).is_empty(),
            "opening the question sends nothing"
        );

        let cancel = testing::find::<CancelCleanUp>(&mut app, |_| true);
        testing::activate(&mut app, cancel);
        testing::settle(&mut app);
        assert!(!testing::shows(&mut app, "Clean up test environments?"));
        assert!(testing::recorded(&mut app).is_empty());
        assert!(!app.world().resource::<TestMachine>().confirm);

        open_confirm(&mut app);
        let confirm = testing::find::<ConfirmCleanUp>(&mut app, |_| true);
        testing::activate(&mut app, confirm);
        testing::settle(&mut app);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::CleanUpTests { repo: None }]
        );
        assert!(!testing::shows(&mut app, "Clean up test environments?"));
        assert!(app.world().resource::<TestMachine>().cleaning);
        assert!(testing::shows(&mut app, "Cleaning…"));
        assert_eq!(testing::count::<CleanUpButton>(&mut app), 0);

        testing::tell(&mut app, Tell::TestDisk(Ok(fixture::demo_test_disk())));
        testing::settle(&mut app);
        assert!(!app.world().resource::<TestMachine>().cleaning);
        assert_eq!(testing::count::<CleanUpButton>(&mut app), 1);
    }

    #[test]
    fn a_refused_clean_up_keeps_the_numbers() {
        let mut app = tests_app();
        answered(&mut app);
        open_confirm(&mut app);
        let confirm = testing::find::<ConfirmCleanUp>(&mut app, |_| true);
        testing::activate(&mut app, confirm);
        testing::tell(&mut app, Tell::TestDisk(Err("A review is busy".into())));
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "7.2 GB"));
        assert!(!app.world().resource::<TestMachine>().cleaning);
    }

    #[test]
    fn escape_and_a_click_outside_close_the_question() {
        let mut app = tests_app();
        answered(&mut app);
        testing::recorded(&mut app);
        open_confirm(&mut app);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);
        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .clear();
        testing::settle(&mut app);
        assert!(!testing::shows(&mut app, "Clean up test environments?"));
        open_confirm(&mut app);
        let layer = testing::find::<CleanUpModal>(&mut app, |_| true);
        app.world_mut().write_message(BackdropClicked(layer));
        testing::settle(&mut app);
        assert!(!testing::shows(&mut app, "Clean up test environments?"));
        assert!(
            testing::recorded(&mut app).is_empty(),
            "closing sends nothing"
        );
    }

    #[test]
    fn the_question_goes_when_the_page_does() {
        let mut app = tests_app();
        answered(&mut app);
        open_confirm(&mut app);
        app.world_mut()
            .resource_mut::<Nav>()
            .open_section(Section::Harness);
        testing::settle(&mut app);
        assert_eq!(testing::count::<CleanUpModal>(&mut app), 0);
    }

    #[test]
    fn machine_text_is_never_a_link_or_a_hidden_character() {
        let mut app = tests_app();
        let info = TestEngineInfo {
            engine: Some(engine_of(false, "unix:///tmp/colima.sock")),
            sandbox_ok: true,
            note: Some("Start it: [x](https://evil.example)\n\u{202e}gnp".into()),
            cores: 4,
        };
        testing::tell(&mut app, Tell::TestEngine(Ok(info)));
        testing::tell(&mut app, Tell::TestDisk(Ok(fixture::demo_test_disk())));
        testing::settle(&mut app);
        assert!(
            testing::shows(&mut app, "[x](https://evil.example)"),
            "drawn as text"
        );
        assert_eq!(testing::count::<OpenLink>(&mut app), 0, "and never a link");
        assert!(
            testing::shown_text(&mut app)
                .iter()
                .all(|t| !t.contains('\u{202e}')),
            "no bidi override reaches the window"
        );
    }

    #[test]
    fn a_settled_page_is_not_rebuilt() {
        let mut app = tests_app();
        answered(&mut app);
        let before = testing::find::<PageOf>(&mut app, |p| p.0 == Section::Tests);
        for _ in 0..5 {
            app.update();
        }
        let after = testing::find::<PageOf>(&mut app, |p| p.0 == Section::Tests);
        assert_eq!(before, after);
    }
}
