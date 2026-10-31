//! The top row of Auto Tests: the Environment, Last run and Run cards, and the banner of a
//! proposed recipe. `top_view` is a pure function of the model; the region is redrawn only when
//! its view differs from the one it drew.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, ScrollArea, observe};
use clusia_core::PrRef;
use clusia_core::printable::{printable, printable_lines};
use clusia_core::tests::{RunOutcome, RunnerKind, StepState, TestRun, TestScope, TestsView};
use clusia_view::status::format_age;

use super::body::{CheckAgain, EditRecipe, GenerateButton, StopButton, TopRegion};
use super::body::{on_check_again, on_edit_recipe, on_generate, on_stop};
use super::{
    TabState, TestsModel, TestsModels, engine_label, run_duration, runner_label, scope_label,
    tab_state,
};
use crate::agent_name;
use crate::bridge::{Ask, Asks, Model, TestsAskKind, Toasts};
use crate::clock::Clock;
use crate::fonts::UiFonts;
use crate::review_state::ReviewTabs;
use crate::screens::review::editor::{not_sent, read_only_reason};
use crate::theme::Swatch;
use crate::ui::kit::{
    Fill, Tone, Type, Variant, badge, button, card, disabled_button, disabled_wrapping_button,
    text, wrapped_text, wrapping_button,
};

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RunButton {
    pub pr: PrRef,
    pub scope: TestScope,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct SetUpButton(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ListTestsButton(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct AcceptProposal(pub PrRef, pub String);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ProposalToggle(pub PrRef);

/// The view a `TopRegion` was drawn from.
#[derive(Component)]
pub(super) struct TopBuilt(Option<TopView>);

/// The most problems the Environment card lists, and the longest line of one.
const PROBLEMS_SHOWN: usize = 3;
const PROBLEM_CHARS: usize = 160;

pub(super) struct Inputs<'a> {
    pub model: &'a TestsModel,
    pub view: &'a TestsView,
    pub agent: &'a str,
    pub read_only: Option<&'static str>,
    pub can_edit: bool,
    pub now: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TopView {
    banner: Option<Banner>,
    env: EnvCard,
    last: LastCard,
    run: RunCard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Banner {
    line: String,
    open: bool,
    /// The proposal's whole text, escaped, only while it is open.
    text: Option<String>,
    /// The hash of the proposal the banner was drawn from.
    hash: String,
    sending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    label: &'static str,
    value: String,
    mono: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EnvCard {
    badge: (String, Tone),
    rows: Vec<Row>,
    /// `env.note`, shown under "Runs in" when no stopped-engine note shows.
    note: Option<String>,
    problems: Vec<String>,
    /// The recipe line and whether its commands are approved.
    recipe: Option<(String, bool)>,
    engine_note: Option<String>,
    edit: bool,
    refresh: bool,
    set_up: bool,
    check_again: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Count {
    n: u32,
    word: &'static str,
    ink: Swatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LastRun {
    scope: String,
    age: String,
    stale: bool,
    counts: Option<[Count; 3]>,
    lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LastCard(Option<LastRun>);

#[derive(Debug, Clone, PartialEq, Eq)]
enum RunCard {
    SetUpFirst,
    Busy {
        line: String,
        stopping: bool,
    },
    Ready {
        near: String,
        all: String,
        near_on: bool,
        all_on: bool,
        /// The reasons a button is off, under both.
        notes: Vec<String>,
        /// `Some(clickable)` while no listing is known.
        list: Option<bool>,
        hint: bool,
    },
}

/// The badge of the Environment card: what is left before the tests can run.
pub(super) fn env_badge(view: &TestsView, state: &TabState) -> (String, Tone) {
    if matches!(state, TabState::Preparing { .. }) {
        return ("Setting up".into(), Tone::Neutral);
    }
    let plan = view.plan.as_ref();
    if plan.is_some_and(|p| p.blocked.is_some()) {
        return ("Blocked".into(), Tone::Orange);
    }
    if *state != TabState::NeedsSetup {
        return ("Ready".into(), Tone::Green);
    }
    let steps = plan.map_or(0, |p| {
        p.steps
            .iter()
            .filter(|s| matches!(s.state, StepState::Missing | StepState::Failed { .. }))
            .count()
    });
    let label = match steps {
        0 => "Test commands to approve".to_string(),
        1 => "1 step to set up".to_string(),
        n => format!("{n} steps to set up"),
    };
    (label, Tone::Orange)
}

fn cut(line: &str) -> String {
    if line.chars().count() <= PROBLEM_CHARS {
        return line.to_string();
    }
    let head: String = line.chars().take(PROBLEM_CHARS - 1).collect();
    format!("{head}…")
}

fn env_card(i: &Inputs, state: &TabState) -> EnvCard {
    let view = i.view;
    let recipe = view.recipe.as_ref();
    let plan = view.plan.as_ref();
    let mut rows = Vec::new();
    let mut engine_note = None;
    if let Some(env) = &view.env {
        let runs_in = match (env.runner, &env.engine) {
            (RunnerKind::Container, Some(engine)) => {
                let label = engine_label(engine);
                if !engine.running {
                    let start = engine.start.as_deref().map(printable);
                    engine_note = Some(match start {
                        Some(start) => {
                            format!("{label} is stopped. Start it with {start}, then check again.")
                        }
                        None => "Start it, then check again.".to_string(),
                    });
                }
                let word = if engine.running { "running" } else { "stopped" };
                Some(format!("{label} · {word}"))
            }
            (RunnerKind::Container, None) => Some("container · no engine found".to_string()),
            (RunnerKind::Local, _) => Some("macOS sandbox · no container engine".to_string()),
        };
        rows.extend(runs_in.map(|value| Row {
            label: "Runs in",
            value,
            mono: false,
        }));
        rows.push(match env.runner {
            RunnerKind::Container => {
                let built = env.existing.images.first().map(String::as_str);
                let value = match (built, recipe.and_then(|r| r.image.as_deref())) {
                    (Some(image), _) | (None, Some(image)) => printable(image),
                    (None, None) => "not built yet".to_string(),
                };
                Row {
                    label: "Image",
                    value,
                    mono: built.is_some(),
                }
            }
            RunnerKind::Local => {
                let tools = if env.signals.toolchains.is_empty() {
                    &env.existing.host_tools
                } else {
                    &env.signals.toolchains
                };
                Row {
                    label: "Toolchain",
                    value: if tools.is_empty() {
                        "not detected".to_string()
                    } else {
                        printable(&tools.join(", "))
                    },
                    mono: false,
                }
            }
        });
        let mut reused = Vec::new();
        if !env.existing.images.is_empty() {
            reused.push("image".to_string());
        }
        reused.extend(env.existing.local_dirs.iter().map(|d| printable(d)));
        rows.push(Row {
            label: "Reused",
            value: if reused.is_empty() {
                "nothing yet".to_string()
            } else {
                reused.join(", ")
            },
            mono: false,
        });
    }
    let services = recipe.map(|r| r.services.as_slice()).unwrap_or_default();
    rows.push(Row {
        label: "Services",
        value: if services.is_empty() {
            "none needed".to_string()
        } else {
            printable(&services.join(", "))
        },
        mono: false,
    });
    let approved = plan.is_some_and(|p| p.commands_approved);
    let mut problems: Vec<String> = recipe
        .map(|r| {
            r.problems
                .iter()
                .take(PROBLEMS_SHOWN)
                .map(|p| cut(&printable(p)))
                .collect()
        })
        .unwrap_or_default();
    let more = recipe.map_or(0, |r| r.problems.len().saturating_sub(PROBLEMS_SHOWN));
    if more > 0 {
        problems.push(format!("+ {more} more"));
    }
    EnvCard {
        badge: env_badge(view, state),
        note: view
            .env
            .as_ref()
            .and_then(|e| e.note.as_deref())
            .filter(|_| engine_note.is_none())
            .map(printable),
        rows,
        problems,
        recipe: recipe.map(|r| {
            let by = if r.by_hand {
                "Recipe edited by you".to_string()
            } else {
                format!("Recipe by {}", i.agent)
            };
            let tail = if approved {
                "approved"
            } else {
                "not approved yet"
            };
            (format!("{by} · {tail}"), approved)
        }),
        check_again: engine_note.is_some(),
        engine_note,
        edit: i.can_edit,
        refresh: view.refresh_available,
        set_up: *state == TabState::NeedsSetup,
    }
}

fn last_card(i: &Inputs) -> LastCard {
    let view = i.view;
    let Some(run) = view.runs.first() else {
        return LastCard(None);
    };
    let age = match format_age(i.now, run.started_at).as_str() {
        "now" => "just now".to_string(),
        age => format!("{age} ago"),
    };
    let mut lines = vec![format!(
        "{} in the {} · no network",
        run_duration(run.duration_ms),
        runner_label(run.runner)
    )];
    match &run.outcome {
        RunOutcome::TimedOut => lines.push(format!(
            "Ran past the limit of {} min",
            (run.duration_ms / 60_000).max(1)
        )),
        RunOutcome::Stopped => lines.push("Stopped".into()),
        RunOutcome::Error { message } => {
            lines.push(format!("Could not run: {}", printable(message)))
        }
        RunOutcome::Passed | RunOutcome::Failed => {}
    }
    let near = view.near.len();
    match view.listed {
        Some(listed) => lines.push(format!(
            "{listed} tests in the repo · {near} near this change"
        )),
        None if near > 0 => lines.push(format!("{near} near this change")),
        None => {}
    }
    LastCard(Some(LastRun {
        scope: scope_label(&run.scope),
        age,
        stale: run.head != view.head,
        counts: counts(run),
        lines,
    }))
}

fn counts(run: &TestRun) -> Option<[Count; 3]> {
    if matches!(run.outcome, RunOutcome::Error { .. }) {
        return None;
    }
    let t = &run.totals;
    let ink = |n: u32, on: Swatch| if n > 0 { on } else { Swatch::Muted };
    Some([
        Count {
            n: t.passed,
            word: "passed",
            ink: ink(t.passed, Swatch::Green),
        },
        Count {
            n: t.failed,
            word: "failed",
            ink: ink(t.failed, Swatch::Orange),
        },
        Count {
            n: t.skipped,
            word: "skipped",
            ink: Swatch::Muted,
        },
    ])
}

fn run_card(i: &Inputs, state: &TabState) -> RunCard {
    let pending = i.model.pending;
    match state {
        TabState::NeedsSetup => RunCard::SetUpFirst,
        TabState::Preparing { .. } => busy("Setting up the tests…".into(), pending),
        TabState::Listing => busy("Listing the tests…".into(), pending),
        TabState::Running => {
            let k = i.model.live.as_ref().map_or(0, |l| l.results.len());
            busy(format!("Running · {k} results so far"), pending)
        }
        _ => {
            let view = i.view;
            let near = view.near.len();
            let sending = pending == Some(TestsAskKind::Run);
            let off = i.read_only.is_some() || sending;
            let near_reason = match view.listed {
                None => Some("List the tests first to find the ones near this change"),
                Some(_) if near == 0 => Some("No tests near this change were found"),
                Some(_) => None,
            };
            let mut notes: Vec<String> = i.read_only.map(str::to_string).into_iter().collect();
            notes.extend(near_reason.map(str::to_string));
            RunCard::Ready {
                near: match near {
                    0 => "Run tests near this change".to_string(),
                    n => format!("Run tests near this change · {n}"),
                },
                all: match view.listed {
                    Some(n) => format!("Run all tests · {n}"),
                    None => "Run all tests".to_string(),
                },
                near_on: !off && near_reason.is_none(),
                all_on: !off,
                notes,
                list: view.listed.is_none().then(|| {
                    i.read_only.is_none()
                        && !matches!(pending, Some(TestsAskKind::Run | TestsAskKind::List))
                }),
                hint: view.recipe.as_ref().is_some_and(|r| r.one.is_some()),
            }
        }
    }
}

fn busy(line: String, pending: Option<TestsAskKind>) -> RunCard {
    RunCard::Busy {
        line,
        stopping: pending == Some(TestsAskKind::Stop),
    }
}

fn banner(i: &Inputs) -> Option<Banner> {
    let proposal = i.view.proposal.as_ref()?;
    let open = i.model.proposal_open;
    Some(Banner {
        line: format!(
            "{} proposed a new recipe. Yours stays until you accept it.",
            i.agent
        ),
        open,
        text: open.then(|| printable_lines(&proposal.text)),
        hash: proposal.hash.clone(),
        sending: i.model.pending == Some(TestsAskKind::Accept),
    })
}

pub(super) fn top_view(i: &Inputs) -> Option<TopView> {
    let state = tab_state(i.view, i.model.live.as_ref());
    if !matches!(
        state,
        TabState::NeedsSetup
            | TabState::Preparing { .. }
            | TabState::Ready
            | TabState::Listing
            | TabState::Running
            | TabState::Done
            | TabState::Stale
    ) {
        return None;
    }
    Some(TopView {
        banner: banner(i),
        env: env_card(i, &state),
        last: last_card(i),
        run: run_card(i, &state),
    })
}

/// Draws each `TopRegion` of a ready tab whose view moved.
pub(super) fn fill_top(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    models: Res<TestsModels>,
    model: Res<Model>,
    clock: Res<Clock>,
    fonts: Res<UiFonts>,
    regions: Query<(Entity, &TopRegion, Option<&TopBuilt>)>,
) {
    for (entity, region, built) in &regions {
        let pr = &region.0;
        let Some((tab, ready)) = tabs.0.get(pr).and_then(|t| Some((t, t.ready()?))) else {
            continue;
        };
        let Some(m) = models.0.get(pr) else {
            continue;
        };
        let Some(view) = m.view.as_ref() else {
            continue;
        };
        let agent = agent_name::of_tab(&model.snapshot, tab);
        let top = top_view(&Inputs {
            model: m,
            view,
            agent: &agent,
            read_only: read_only_reason(&tabs, &model, pr),
            can_edit: ready.view.worktree.is_some(),
            now: clock.now(),
        });
        if built.is_some_and(|b| b.0 == top) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        if let Some(top) = &top {
            commands
                .entity(entity)
                .with_children(|p| draw(p, &fonts, pr, top));
        }
        commands.entity(entity).insert(TopBuilt(top));
    }
}

type Spawner<'a> = ChildSpawnerCommands<'a>;

fn draw(p: &mut Spawner, fonts: &UiFonts, pr: &PrRef, top: &TopView) {
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(14),
        ..default()
    })
    .with_children(|col| {
        if let Some(banner) = &top.banner {
            draw_banner(col, fonts, pr, banner);
        }
        col.spawn(Node {
            column_gap: px(16),
            align_items: AlignItems::Stretch,
            ..default()
        })
        .with_children(|row| {
            draw_env(row, fonts, pr, &top.env);
            draw_last(row, fonts, &top.last);
            draw_run(row, fonts, pr, &top.run);
        });
    });
}

fn card_node(grow: f32) -> Node {
    Node {
        flex_grow: grow,
        flex_basis: px(0),
        min_width: px(0),
        padding: px(16).all(),
        flex_direction: FlexDirection::Column,
        row_gap: px(8),
        ..default()
    }
}

fn line() -> Node {
    Node {
        align_items: AlignItems::Center,
        column_gap: px(8),
        flex_wrap: FlexWrap::Wrap,
        ..default()
    }
}

fn draw_banner(p: &mut Spawner, fonts: &UiFonts, pr: &PrRef, banner: &Banner) {
    p.spawn(card(Node {
        padding: px(12).all(),
        flex_direction: FlexDirection::Column,
        row_gap: px(10),
        ..default()
    }))
    .with_children(|c| {
        c.spawn(line()).with_children(|row| {
            row.spawn(Node {
                flex_grow: 1.0,
                flex_basis: px(0),
                ..default()
            })
            .with_child(wrapped_text(fonts, banner.line.clone(), Type::BODY));
            row.spawn((
                button(
                    fonts,
                    if banner.open {
                        "Hide proposal"
                    } else {
                        "Show proposal"
                    },
                    Variant::Link,
                ),
                ProposalToggle(pr.clone()),
                observe(on_proposal_toggle),
            ));
            if banner.sending {
                row.spawn(disabled_button(fonts, "Accept"));
            } else {
                row.spawn((
                    button(fonts, "Accept", Variant::Primary),
                    AcceptProposal(pr.clone(), banner.hash.clone()),
                    observe(on_accept),
                ));
            }
        });
        if let Some(body) = &banner.text {
            c.spawn((
                Node {
                    max_height: px(240),
                    overflow: Overflow::scroll_y(),
                    padding: px(10).all(),
                    ..default()
                },
                BackgroundColor::default(),
                Fill(Swatch::Chrome),
                ScrollArea,
                ScrollPosition::default(),
            ))
            .with_child(wrapped_text(fonts, body.clone(), Type::MONO));
        }
    });
}

fn draw_env(p: &mut Spawner, fonts: &UiFonts, pr: &PrRef, env: &EnvCard) {
    p.spawn(card(card_node(1.0))).with_children(|c| {
        c.spawn(line()).with_children(|head| {
            head.spawn(text(fonts, "Environment", Type::HEADING));
            head.spawn(badge(fonts, &env.badge.0, env.badge.1));
        });
        for (i, row) in env.rows.iter().enumerate() {
            c.spawn(line()).with_children(|r| {
                r.spawn(text(fonts, row.label, Type::META));
                let t = if row.mono { Type::MONO } else { Type::BODY };
                r.spawn(wrapped_text(fonts, row.value.clone(), t));
            });
            if i == 0
                && row.label == "Runs in"
                && let Some(note) = &env.note
            {
                c.spawn(wrapped_text(fonts, note.clone(), Type::MUTED));
            }
        }
        for problem in &env.problems {
            c.spawn(wrapped_text(
                fonts,
                problem.clone(),
                Type::BODY.ink(Swatch::Orange),
            ));
        }
        if let Some(note) = &env.engine_note {
            c.spawn(wrapped_text(fonts, note.clone(), Type::MUTED));
        }
        c.spawn(line()).with_children(|r| {
            if let Some((label, approved)) = &env.recipe {
                let ink = if *approved {
                    Swatch::Green
                } else {
                    Swatch::Muted
                };
                r.spawn(text(fonts, label.clone(), Type::BODY.ink(ink)));
            }
            if env.edit {
                r.spawn((
                    button(fonts, "Edit recipe", Variant::Link),
                    EditRecipe(pr.clone()),
                    observe(on_edit_recipe),
                ));
            }
            if env.refresh {
                r.spawn((
                    button(fonts, "Refresh recipe", Variant::Link),
                    GenerateButton(pr.clone()),
                    observe(on_generate),
                ));
            }
        });
        if env.check_again || env.set_up {
            c.spawn(line()).with_children(|r| {
                if env.check_again {
                    r.spawn((
                        button(fonts, "Check again", Variant::Secondary),
                        CheckAgain(Some(pr.clone())),
                        observe(on_check_again),
                    ));
                }
                if env.set_up {
                    r.spawn((
                        button(fonts, "Set up…", Variant::Primary),
                        SetUpButton(pr.clone()),
                        observe(on_set_up),
                    ));
                }
            });
        }
    });
}

fn draw_last(p: &mut Spawner, fonts: &UiFonts, last: &LastCard) {
    p.spawn(card(card_node(1.0))).with_children(|c| {
        let Some(run) = &last.0 else {
            c.spawn(text(fonts, "Last run", Type::HEADING));
            c.spawn(text(fonts, "No run yet", Type::MUTED));
            return;
        };
        c.spawn(line()).with_children(|head| {
            head.spawn(text(fonts, "Last run", Type::HEADING));
            head.spawn(text(
                fonts,
                format!("· {} · {}", run.scope, run.age),
                Type::MUTED,
            ));
            if run.stale {
                head.spawn(badge(fonts, "Stale", Tone::Orange));
            }
        });
        if run.stale {
            c.spawn(wrapped_text(
                fonts,
                "From an older commit; run the tests again to check this one.",
                Type::MUTED,
            ));
        }
        if let Some(counts) = &run.counts {
            c.spawn(Node {
                column_gap: px(18),
                align_items: AlignItems::Baseline,
                flex_wrap: FlexWrap::Wrap,
                ..default()
            })
            .with_children(|row| {
                for count in counts {
                    row.spawn(Node {
                        column_gap: px(8),
                        align_items: AlignItems::Baseline,
                        ..default()
                    })
                    .with_children(|n| {
                        n.spawn(text(
                            fonts,
                            count.n.to_string(),
                            Type::NUMBER.ink(count.ink),
                        ));
                        n.spawn(text(fonts, count.word, Type::MUTED));
                    });
                }
            });
        }
        for l in &run.lines {
            c.spawn(wrapped_text(fonts, l.clone(), Type::MUTED));
        }
    });
}

fn draw_run(p: &mut Spawner, fonts: &UiFonts, pr: &PrRef, run: &RunCard) {
    p.spawn(card(card_node(0.78)))
        .insert(Fill(Swatch::GreenTint))
        .with_children(|c| {
            c.spawn(text(fonts, "Run the tests", Type::HEADING));
            match run {
                RunCard::SetUpFirst => {
                    c.spawn(text(fonts, "Set up the tests first", Type::MUTED));
                }
                RunCard::Busy { line: l, stopping } => {
                    c.spawn(text(fonts, l.clone(), Type::MUTED));
                    if *stopping {
                        c.spawn(disabled_button(fonts, "Stopping…"));
                    } else {
                        c.spawn((
                            button(fonts, "Stop", Variant::Secondary),
                            StopButton(pr.clone()),
                            observe(on_stop),
                        ));
                    }
                }
                RunCard::Ready {
                    near,
                    all,
                    near_on,
                    all_on,
                    notes,
                    list,
                    hint,
                } => {
                    run_button(
                        c,
                        fonts,
                        pr,
                        near,
                        TestScope::NearChange,
                        *near_on,
                        Variant::Primary,
                    );
                    run_button(
                        c,
                        fonts,
                        pr,
                        all,
                        TestScope::All,
                        *all_on,
                        Variant::Secondary,
                    );
                    for note in notes {
                        c.spawn(wrapped_text(fonts, note.clone(), Type::META));
                    }
                    match list {
                        Some(true) => {
                            c.spawn((
                                button(fonts, "List tests", Variant::Link),
                                ListTestsButton(pr.clone()),
                                observe(on_list_tests),
                            ));
                        }
                        Some(false) => {
                            c.spawn(text(fonts, "List tests", Type::MUTED));
                        }
                        None => {}
                    }
                    if *hint {
                        c.spawn(wrapped_text(
                            fonts,
                            "Or click ▶ on a test to run only that one.",
                            Type::META,
                        ));
                    }
                }
            }
        });
}

fn run_button(
    p: &mut Spawner,
    fonts: &UiFonts,
    pr: &PrRef,
    label: &str,
    scope: TestScope,
    on: bool,
    variant: Variant,
) {
    if on {
        p.spawn((
            wrapping_button(fonts, label, variant),
            RunButton {
                pr: pr.clone(),
                scope,
            },
            observe(on_run),
        ));
    } else {
        p.spawn(disabled_wrapping_button(fonts, label));
    }
}

fn on_run(
    activate: On<Activate>,
    buttons: Query<&RunButton>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut models: ResMut<TestsModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(RunButton { pr, scope }) = buttons.get(activate.entity) else {
        return;
    };
    if models.0.get(pr).is_some_and(|m| m.pending.is_some()) {
        return;
    }
    if let Some(reason) = read_only_reason(&tabs, &model, pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    asks.send(Ask::RunTests {
        pr: pr.clone(),
        scope: scope.clone(),
    });
    models
        .0
        .entry(pr.clone())
        .or_default()
        .sent_run(scope.clone());
}

fn on_list_tests(
    activate: On<Activate>,
    buttons: Query<&ListTestsButton>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut models: ResMut<TestsModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(ListTestsButton(pr)) = buttons.get(activate.entity) else {
        return;
    };
    if models.0.get(pr).is_some_and(|m| m.pending.is_some()) {
        return;
    }
    if let Some(reason) = read_only_reason(&tabs, &model, pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    asks.send(Ask::ListTests(pr.clone()));
    models
        .0
        .entry(pr.clone())
        .or_default()
        .sent(TestsAskKind::List);
}

fn on_accept(
    activate: On<Activate>,
    buttons: Query<&AcceptProposal>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut models: ResMut<TestsModels>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(AcceptProposal(pr, recipe_hash)) = buttons.get(activate.entity) else {
        return;
    };
    if models.0.get(pr).is_some_and(|m| m.pending.is_some()) {
        return;
    }
    if let Some(reason) = read_only_reason(&tabs, &model, pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    asks.send(Ask::AcceptRecipe {
        pr: pr.clone(),
        recipe_hash: recipe_hash.clone(),
    });
    models
        .0
        .entry(pr.clone())
        .or_default()
        .sent(TestsAskKind::Accept);
}

fn on_set_up(
    activate: On<Activate>,
    buttons: Query<&SetUpButton>,
    mut models: ResMut<TestsModels>,
) {
    let Ok(SetUpButton(pr)) = buttons.get(activate.entity) else {
        return;
    };
    models.0.entry(pr.clone()).or_default().open_plan();
}

fn on_proposal_toggle(
    activate: On<Activate>,
    buttons: Query<&ProposalToggle>,
    mut models: ResMut<TestsModels>,
) {
    let Ok(ProposalToggle(pr)) = buttons.get(activate.entity) else {
        return;
    };
    let model = models.0.entry(pr.clone()).or_default();
    model.proposal_open = !model.proposal_open;
    model.touch();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Tell;
    use crate::fixture;
    use crate::review_state::ReviewSection;
    use crate::screens::review::tests_tab::LiveRun;
    use crate::testing::{self, NOW};
    use crate::ui::markdown::{MdBody, MdLink};
    use clusia_core::tests::{
        NearTest, NearWhy, PlanStep, RecipeProposal, RunOutcome, StepPlace, StepState,
        TestCaseResult, TestScope, TestStatus, TestsPhase, Totals,
    };

    fn tests_app() -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        // The demo review has a worktree; the tests that need one set it themselves.
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .and_then(|t| t.ready_mut())
            .expect("a ready tab")
            .view
            .worktree = None;
        testing::settle(&mut app);
        (app, pr)
    }

    /// A ready view with the demo engine, recipe and plan, 38 tests near the change and
    /// 412 listed; `edit` changes it before it is shown.
    fn show(edit: impl FnOnce(&mut TestsView)) -> (App, PrRef, TestsView) {
        let (mut app, pr) = tests_app();
        let mut view = testing::tests_view(&app, &pr, TestsPhase::Ready);
        view.env = Some(testing::tests_env(true));
        view.recipe = Some(testing::tests_recipe());
        view.plan = Some(testing::tests_plan(Vec::new(), true));
        view.listed = Some(412);
        view.near = (0..38)
            .map(|i| NearTest {
                name: format!("store::refresh::t{i}"),
                why: NearWhy::File,
            })
            .collect();
        edit(&mut view);
        testing::open_tests(&mut app, &pr, view.clone());
        testing::recorded(&mut app);
        (app, pr, view)
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

    fn passed(n: u32) -> Totals {
        Totals {
            passed: n,
            failed: 0,
            skipped: 0,
        }
    }

    fn set_worktree(app: &mut App, pr: &PrRef) {
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(pr)
            .and_then(|t| t.ready_mut())
            .expect("a ready tab")
            .view
            .worktree = Some("/work/rzorzal-clusia-123".into());
        testing::settle(app);
    }

    fn model(app: &App, pr: &PrRef) -> TestsModel {
        let m = &app.world().resource::<TestsModels>().0[pr];
        TestsModel {
            pending: m.pending,
            proposal_open: m.proposal_open,
            plan: m.plan.clone(),
            ..TestsModel::default()
        }
    }

    #[test]
    fn the_badge_follows_what_is_left_to_set_up() {
        let mut view = TestsView::new("4f2a91c");
        view.plan = Some(testing::tests_plan(
            vec![
                step("engine", StepState::Done),
                step("image", StepState::Missing),
                step(
                    "stack",
                    StepState::Failed {
                        message: "no".into(),
                    },
                ),
                step(
                    "services",
                    StepState::Skipped {
                        why: "not needed".into(),
                    },
                ),
            ],
            false,
        ));
        assert_eq!(
            env_badge(&view, &TabState::NeedsSetup),
            ("2 steps to set up".to_string(), Tone::Orange)
        );
        view.plan.as_mut().unwrap().steps.remove(2);
        assert_eq!(
            env_badge(&view, &TabState::NeedsSetup),
            ("1 step to set up".to_string(), Tone::Orange)
        );
        view.plan.as_mut().unwrap().steps.remove(1);
        assert_eq!(
            env_badge(&view, &TabState::NeedsSetup),
            ("Test commands to approve".to_string(), Tone::Orange)
        );
        assert_eq!(
            env_badge(&view, &TabState::Preparing { step: None }),
            ("Setting up".to_string(), Tone::Neutral)
        );
        for state in [
            TabState::Ready,
            TabState::Listing,
            TabState::Running,
            TabState::Done,
            TabState::Stale,
        ] {
            assert_eq!(env_badge(&view, &state), ("Ready".to_string(), Tone::Green));
        }
        view.plan.as_mut().unwrap().blocked = Some("no engine".into());
        assert_eq!(
            env_badge(&view, &TabState::NeedsSetup),
            ("Blocked".to_string(), Tone::Orange)
        );
    }

    #[test]
    fn the_environment_card_reads_the_machine_and_the_recipe() {
        let (mut app, pr, _) = show(|v| {
            let env = v.env.as_mut().unwrap();
            env.existing.images = vec!["clusia/rzorzal-clusia:4f2a91c".into()];
            env.existing.local_dirs = vec![".venv".into()];
        });
        for needle in [
            "Environment",
            "Ready",
            "Runs in",
            "Docker · Colima · running",
            "Image",
            "clusia/rzorzal-clusia:4f2a91c",
            "Reused",
            "image, .venv",
            "Services",
            "none needed",
            "Recipe by Claude Code · approved",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        assert_eq!(testing::count::<EditRecipe>(&mut app), 0, "no worktree yet");
        set_worktree(&mut app, &pr);
        assert_eq!(testing::count::<EditRecipe>(&mut app), 1);
    }

    #[test]
    fn the_environment_card_says_what_is_missing() {
        let (mut app, _, _) = show(|v| {
            v.recipe.as_mut().unwrap().by_hand = true;
            v.plan.as_mut().unwrap().commands_approved = false;
            let env = v.env.as_mut().unwrap();
            env.runner = clusia_core::tests::RunnerKind::Local;
            env.signals.toolchains = vec!["cargo 1.99".into(), "node 24".into()];
            v.recipe.as_mut().unwrap().services = vec!["db".into(), "cache".into()];
        });
        for needle in [
            "macOS sandbox · no container engine",
            "Toolchain",
            "cargo 1.99, node 24",
            "nothing yet",
            "db, cache",
            "Recipe edited by you · not approved yet",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        assert!(!testing::shows(&mut app, "Image"));
    }

    #[test]
    fn a_stopped_engine_is_noted_with_its_start_command_and_check_again_asks() {
        let (mut app, pr, _) = show(|v| v.env = Some(testing::tests_env(false)));
        assert!(testing::shows(&mut app, "Docker · Colima · stopped"));
        assert!(testing::shows(
            &mut app,
            "Docker · Colima is stopped. Start it with colima start, then check again."
        ));
        let again = testing::find::<CheckAgain>(&mut app, |b| b.0.as_ref() == Some(&pr));
        testing::activate(&mut app, again);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::CheckTestEngine {
                pr: Some(pr.clone())
            }]
        );
    }

    #[test]
    fn the_environment_card_says_how_to_get_an_engine() {
        let (mut app, ..) = show(|v| {
            let env = v.env.as_mut().unwrap();
            env.runner = clusia_core::tests::RunnerKind::Local;
            env.engine = None;
            env.note = Some("No container engine found. Install Colima…\nthen check again".into());
        });
        assert!(testing::shows(
            &mut app,
            "No container engine found. Install Colima…\\nthen check again"
        ));
        assert_eq!(testing::count::<CheckAgain>(&mut app), 0);
    }

    #[test]
    fn set_up_opens_the_plan_and_the_card_counts_the_steps() {
        let (mut app, pr, _) = show(|v| {
            v.phase = TestsPhase::NeedsSetup;
            v.plan = Some(testing::tests_plan(
                vec![step("image", StepState::Missing)],
                false,
            ));
        });
        assert!(testing::shows(&mut app, "1 step to set up"));
        assert!(testing::shows(&mut app, "Set up the tests first"));
        assert_eq!(testing::count::<RunButton>(&mut app), 0);
        let set_up = testing::find::<SetUpButton>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, set_up);
        testing::settle(&mut app);
        assert_eq!(
            model(&app, &pr).plan.map(|p| p.recipe_hash),
            Some("h1".to_string())
        );
        assert!(
            testing::recorded(&mut app).is_empty(),
            "opening asks nothing"
        );
    }

    #[test]
    fn refresh_recipe_is_the_same_ask_as_generate() {
        let (mut app, pr, _) = show(|v| v.refresh_available = true);
        assert!(testing::shows(&mut app, "Refresh recipe"));
        let refresh = testing::find::<GenerateButton>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, refresh);
        assert_eq!(testing::recorded(&mut app), [Ask::GenerateTests(pr)]);
    }

    #[test]
    fn problems_and_the_proposal_are_plain_text() {
        let nasty = "unknown key `tests.\u{1b}[31m`\n[click](https://evil.example)\u{202e}";
        let (mut app, pr, _) = show(|v| {
            v.recipe.as_mut().unwrap().problems = vec![
                nasty.into(),
                "second".into(),
                "third".into(),
                "fourth".into(),
                "fifth".into(),
            ];
            v.proposal = Some(RecipeProposal {
                text: format!("[tests]\nall = \"echo\" # {nasty}\n"),
                hash: "h2".into(),
                at: NOW - 60,
            });
        });
        assert!(testing::shows(
            &mut app,
            "unknown key `tests.\\u{1b}[31m`\\n[click](https://evil.example)\\u{202e}"
        ));
        assert!(testing::shows(&mut app, "+ 2 more"));
        assert!(!testing::shows(&mut app, "fourth"));
        let toggle = testing::find::<ProposalToggle>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, toggle);
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "Hide proposal"));
        assert!(testing::shows(
            &mut app,
            "all = \"echo\" # unknown key `tests.\\u{1b}[31m`\n[click](https://evil.example)\\u{202e}"
        ));
        assert_eq!(testing::count::<MdBody>(&mut app), 0, "no markdown node");
        assert_eq!(testing::count::<MdLink>(&mut app), 0, "no link");
        assert!(
            app.world()
                .get_resource::<crate::platform_open::OpenUrls>()
                .is_none_or(|u| u.0.is_empty())
        );
    }

    #[test]
    fn a_proposal_banner_toggles_and_accept_asks_once() {
        let (mut app, pr, _) = show(|v| {
            v.proposal = Some(RecipeProposal {
                text: "[tests]\nall = \"cargo test\"\n".into(),
                hash: "h2".into(),
                at: NOW - 60,
            });
        });
        assert!(testing::shows(
            &mut app,
            "Claude Code proposed a new recipe. Yours stays until you accept it."
        ));
        assert!(testing::shows(&mut app, "Show proposal"));
        assert!(!testing::shows(&mut app, "all = \"cargo test\""));
        let toggle = testing::find::<ProposalToggle>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, toggle);
        testing::settle(&mut app);
        assert!(model(&app, &pr).proposal_open);
        assert!(testing::shows(&mut app, "all = \"cargo test\""));
        let accept = testing::find::<AcceptProposal>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, accept);
        testing::settle(&mut app);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::AcceptRecipe {
                pr: pr.clone(),
                recipe_hash: "h2".into()
            }]
        );
        assert_eq!(model(&app, &pr).pending, Some(TestsAskKind::Accept));
        assert_eq!(
            testing::count::<AcceptProposal>(&mut app),
            0,
            "disabled while it waits"
        );
    }

    #[test]
    fn a_proposal_shows_from_needs_setup_on_and_not_before() {
        let proposal = RecipeProposal {
            text: "x".into(),
            hash: "h2".into(),
            at: NOW,
        };
        let (mut app, ..) = show(|v| {
            v.phase = TestsPhase::NeedsSetup;
            v.proposal = Some(proposal.clone());
        });
        assert!(testing::shows(&mut app, "proposed a new recipe"));
        let (mut app, ..) = show(|v| {
            v.phase = TestsPhase::NotGenerated;
            v.proposal = Some(proposal);
        });
        assert!(!testing::shows(&mut app, "proposed a new recipe"));
    }

    #[test]
    fn the_last_run_card_reads_the_newest_run() {
        let (mut app, ..) = show(|v| {
            let head = v.head.clone();
            v.runs = vec![testing::tests_run(
                &head,
                NOW - 240,
                RunOutcome::Failed,
                Totals {
                    passed: 37,
                    failed: 1,
                    skipped: 0,
                },
            )];
        });
        for needle in [
            "Last run",
            "near this change · 4m ago",
            "37",
            "passed",
            "failed",
            "skipped",
            "48.2 s in the container · no network",
            "412 tests in the repo · 38 near this change",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        assert!(!testing::shows(&mut app, "Stale"));
    }

    #[test]
    fn a_run_of_another_head_is_stale_not_done() {
        let (mut app, ..) = show(|v| {
            v.runs = vec![testing::tests_run(
                "0ld0000",
                NOW - 7200,
                RunOutcome::Passed,
                passed(412),
            )];
        });
        assert!(testing::shows(&mut app, "Stale"));
        assert!(testing::shows(
            &mut app,
            "From an older commit; run the tests again to check this one."
        ));
        assert!(testing::shows(&mut app, "near this change · 2h ago"));
        assert!(
            testing::shows(&mut app, "Run tests near this change · 38"),
            "the new commit can still be run"
        );
        let (mut app, ..) = show(|v| {
            let head = v.head.clone();
            v.runs = vec![testing::tests_run(
                &head,
                NOW - 7200,
                RunOutcome::Passed,
                passed(412),
            )];
        });
        assert!(!testing::shows(&mut app, "From an older commit"));
    }

    #[test]
    fn a_run_that_did_not_finish_says_how_it_ended() {
        let timed_out = |v: &mut TestsView| {
            let head = v.head.clone();
            let mut run = testing::tests_run(&head, NOW - 30, RunOutcome::TimedOut, passed(5));
            run.duration_ms = 600_000;
            v.runs = vec![run];
        };
        let (mut app, ..) = show(timed_out);
        assert!(testing::shows(&mut app, "Ran past the limit of 10 min"));
        let (mut app, ..) = show(|v| {
            let head = v.head.clone();
            v.runs = vec![testing::tests_run(
                &head,
                NOW - 30,
                RunOutcome::Stopped,
                passed(2),
            )];
        });
        assert!(testing::shows(&mut app, "Stopped"));
        assert!(testing::shows(&mut app, "just now"));
        let (mut app, ..) = show(|v| {
            let head = v.head.clone();
            v.runs = vec![testing::tests_run(
                &head,
                NOW - 30,
                RunOutcome::Error {
                    message: "docker: no space\nleft".into(),
                },
                Totals::default(),
            )];
        });
        assert!(testing::shows(
            &mut app,
            "Could not run: docker: no space\\nleft"
        ));
        assert!(
            !testing::shows(&mut app, "skipped"),
            "an error has no counts"
        );
    }

    #[test]
    fn with_no_run_the_card_says_so_and_without_a_listing_the_counts_are_left_out() {
        let (mut app, ..) = show(|v| v.listed = None);
        assert!(testing::shows(&mut app, "No run yet"));
        assert!(!testing::shows(&mut app, "tests in the repo"));
        let (mut app, ..) = show(|v| {
            v.listed = None;
            let head = v.head.clone();
            v.runs = vec![testing::tests_run(
                &head,
                NOW - 60,
                RunOutcome::Passed,
                passed(3),
            )];
        });
        assert!(testing::shows(&mut app, "38 near this change"));
        assert!(!testing::shows(&mut app, "tests in the repo"));
    }

    #[test]
    fn the_run_buttons_send_their_scope_once() {
        let (mut app, pr, _) = show(|_| {});
        assert!(testing::shows(&mut app, "Run the tests"));
        assert!(testing::shows(&mut app, "Run tests near this change · 38"));
        assert!(testing::shows(&mut app, "Run all tests · 412"));
        assert!(testing::shows(
            &mut app,
            "Or click ▶ on a test to run only that one."
        ));
        assert_eq!(testing::count::<ListTestsButton>(&mut app), 0);
        let near = testing::find::<RunButton>(&mut app, |b| b.scope == TestScope::NearChange);
        testing::activate(&mut app, near);
        testing::settle(&mut app);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::RunTests {
                pr: pr.clone(),
                scope: TestScope::NearChange
            }]
        );
        assert_eq!(model(&app, &pr).pending, Some(TestsAskKind::Run));
        assert_eq!(
            testing::count::<RunButton>(&mut app),
            0,
            "both wait for the answer"
        );
    }

    #[test]
    fn a_second_activation_before_the_redraw_sends_nothing() {
        let (mut app, pr, _) = show(|_| {});
        let near = testing::find::<RunButton>(&mut app, |b| b.scope == TestScope::NearChange);
        testing::activate(&mut app, near);
        testing::activate(&mut app, near);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::RunTests {
                pr,
                scope: TestScope::NearChange
            }]
        );
    }

    #[test]
    fn accept_names_the_proposal_the_banner_showed() {
        let (mut app, pr, _) = show(|v| {
            v.proposal = Some(RecipeProposal {
                text: "a".into(),
                hash: "h2".into(),
                at: NOW,
            });
        });
        let accept = testing::find::<AcceptProposal>(&mut app, |b| b.0 == pr);
        let mut models = app.world_mut().resource_mut::<TestsModels>();
        let view = models.0.get_mut(&pr).and_then(|m| m.view.as_mut());
        view.unwrap().proposal.as_mut().unwrap().hash = "h3".into();
        testing::activate(&mut app, accept);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::AcceptRecipe {
                pr,
                recipe_hash: "h2".into()
            }],
            "the hash of what was drawn, not of what came after"
        );
    }

    #[test]
    fn run_all_sends_the_all_scope() {
        let (mut app, pr, _) = show(|_| {});
        let all = testing::find::<RunButton>(&mut app, |b| b.scope == TestScope::All);
        testing::activate(&mut app, all);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::RunTests {
                pr,
                scope: TestScope::All
            }]
        );
    }

    #[test]
    fn without_a_listing_the_near_button_waits_for_list_tests() {
        let (mut app, pr, _) = show(|v| v.listed = None);
        assert!(testing::shows(
            &mut app,
            "List the tests first to find the ones near this change"
        ));
        assert!(testing::shows(&mut app, "Run tests near this change"));
        assert!(testing::shows(&mut app, "Run all tests"));
        assert!(!testing::shows(&mut app, "Run all tests ·"));
        assert_eq!(
            testing::count::<RunButton>(&mut app),
            1,
            "only Run all can be clicked"
        );
        let list = testing::find::<ListTestsButton>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, list);
        testing::settle(&mut app);
        assert_eq!(testing::recorded(&mut app), [Ask::ListTests(pr.clone())]);
        assert_eq!(model(&app, &pr).pending, Some(TestsAskKind::List));
        assert_eq!(testing::count::<ListTestsButton>(&mut app), 0);
    }

    #[test]
    fn a_listing_with_nothing_near_disables_only_the_near_button() {
        let (mut app, ..) = show(|v| v.near.clear());
        assert!(testing::shows(
            &mut app,
            "No tests near this change were found"
        ));
        assert!(testing::shows(&mut app, "Run all tests · 412"));
        assert_eq!(testing::count::<RunButton>(&mut app), 1);
    }

    #[test]
    fn the_one_test_hint_needs_a_one_command() {
        let (mut app, ..) = show(|v| v.recipe.as_mut().unwrap().one = None);
        assert!(!testing::shows(&mut app, "to run only that one"));
    }

    #[test]
    fn offline_the_run_card_gives_the_reason_and_sends_nothing() {
        let (mut app, ..) = show(|_| {});
        testing::tell(&mut app, Tell::Lost("gone".into()));
        testing::settle(&mut app);
        assert_eq!(testing::count::<RunButton>(&mut app), 0);
        assert!(testing::shows(&mut app, "Not connected to clusiad"));
    }

    #[test]
    fn stop_shows_while_running_listing_and_preparing() {
        for (phase, line) in [
            (
                TestsPhase::Running { run: "r1".into() },
                "Running · 2 results so far",
            ),
            (
                TestsPhase::Running { run: "list".into() },
                "Listing the tests…",
            ),
            (
                TestsPhase::Preparing {
                    step: "image".into(),
                },
                "Setting up the tests…",
            ),
        ] {
            let (mut app, pr, _) = show(|v| v.phase = phase);
            {
                let mut models = app.world_mut().resource_mut::<TestsModels>();
                let m = models.0.get_mut(&pr).expect("a model");
                if matches!(m.view.as_ref().map(|v| &v.phase), Some(TestsPhase::Running { run }) if run != "list")
                {
                    m.live = Some(LiveRun {
                        id: "r1".into(),
                        scope: None,
                        results: ["a", "b"]
                            .map(|n| TestCaseResult {
                                name: n.into(),
                                file: None,
                                status: TestStatus::Passed,
                                duration_ms: None,
                                message: None,
                            })
                            .to_vec(),
                        tail: String::new(),
                        tail_at: 0.0,
                        next_tail: None,
                    });
                }
            }
            testing::settle(&mut app);
            assert!(testing::shows(&mut app, line), "{line}");
            assert_eq!(testing::count::<RunButton>(&mut app), 0, "{line}");
            let stop = testing::find::<StopButton>(&mut app, |b| b.0 == pr);
            testing::activate(&mut app, stop);
            assert_eq!(testing::recorded(&mut app), [Ask::StopTests(pr)], "{line}");
        }
    }

    #[test]
    fn a_settled_top_row_rebuilds_nothing() {
        let (mut app, pr, _) = show(|_| {});
        let region = testing::find::<TopRegion>(&mut app, |r| r.0 == pr);
        let signature = testing::tree_signature(&app, region);
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(testing::tree_signature(&app, region), signature);
    }
}
