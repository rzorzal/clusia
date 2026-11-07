//! This PR vs main: the table of medians, spreads and deltas of the selected item, its verdict and
//! the notes under it, or what main needs before there can be one. A color says better or worse
//! only through `verdict_tone` and `delta_tone`, which give none while the comparison is no
//! verdict or either side is about something else than what is under review now. Every text the
//! daemon sent goes through `shown` before it is drawn.

use bevy::prelude::*;
use bevy::ui_widgets::{Activate, observe};
use clusia_core::PrRef;
use clusia_core::cockpit::{
    ComparisonView, DeltaLabel, Freshness, MAIN_FIRST, MainPhase, MetricRow, PassCount, SideStat,
    SideView, Verdict, delta_text, metric_value_text,
};

use super::frame::CockpitCompare;
use super::list::open_prepare_main;
use super::views::{CompareView, PURPOSE_SHOWN, compare_view, shown};
use super::{CockpitModel, CockpitModels};
use crate::agent_name;
use crate::bridge::Model;
use crate::clock::Clock;
use crate::fonts::UiFonts;
use crate::review_state::ReviewTabs;
use crate::screens::review::tests_tab::plan::Spawner;
use crate::screens::review::tests_tab::runner_label;
use crate::theme::Swatch;
use crate::ui::kit::{Fill, Stroke, Tone, Type, Variant, button, text, wrapped_text};

/// The sentence of a side that is not about what is under review now.
pub fn freshness_text(f: Freshness) -> Option<&'static str> {
    match f {
        Freshness::Current => None,
        Freshness::OlderCommit => Some("From an older commit."),
        Freshness::OlderBase => Some("From an older base."),
        Freshness::ValuesChanged => Some("These values changed since this ran."),
        Freshness::OtherRunner => Some("From another runner."),
    }
}

/// A scenario whose result and outputs are the same on both sides.
pub const ANTI_GOAL: &str = "Doesn't exercise the change: same result and outputs on main.";

pub const COMPARE_HINT: &str = "Run it on this PR and main to compare.";

/// Whether the comparison may say better or worse: a verdict that is one, between two sides that
/// are both about what is under review now.
fn is_a_verdict(c: &ComparisonView) -> bool {
    c.pr.freshness == Freshness::Current
        && c.main.freshness == Freshness::Current
        && !matches!(
            c.verdict,
            Verdict::Running | Verdict::Stopped | Verdict::NotEnoughRounds
        )
}

/// The verdict box's tone: orange for worse, green for better, neutral for everything that is no
/// verdict.
pub fn verdict_tone(c: &ComparisonView) -> Tone {
    if !is_a_verdict(c) {
        return Tone::Neutral;
    }
    match c.verdict {
        Verdict::Worse { .. } | Verdict::Mixed { .. } | Verdict::PassDiffers { pr: false, .. } => {
            Tone::Orange
        }
        Verdict::Better { .. } | Verdict::PassDiffers { pr: true, .. } => Tone::Green,
        _ => Tone::Neutral,
    }
}

/// A Δ cell's tone, or `None` for the muted one.
pub fn delta_tone(c: &ComparisonView, row: &MetricRow) -> Option<Tone> {
    if !is_a_verdict(c) {
        return None;
    }
    match row.delta.label {
        DeltaLabel::Worse => Some(Tone::Orange),
        DeltaLabel::Better => Some(Tone::Green),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub value: String,
    /// " ±<n>", drawn small and faint.
    pub spread: Option<String>,
    pub ink: Swatch,
}

impl Cell {
    fn none() -> Cell {
        Cell {
            value: "—".into(),
            spread: None,
            ink: Swatch::Muted,
        }
    }

    fn plain(value: String, ink: Swatch) -> Cell {
        Cell {
            value,
            spread: None,
            ink,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableRow {
    pub label: String,
    pub main: Cell,
    pub pr: Cell,
    pub delta: Cell,
    pub faint: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VerdictDrawn {
    pub line: String,
    pub tone: Tone,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CompareBody {
    Table {
        rows: Vec<TableRow>,
        verdict: Option<VerdictDrawn>,
        notes: Vec<String>,
        anti_goal: bool,
    },
    Hint,
    /// Main is not ready and there is no comparison: what it needs.
    Main {
        title: Option<String>,
        lines: Vec<(String, Swatch)>,
        try_again: bool,
    },
}

/// What the comparison card draws.
#[derive(Debug, Clone, PartialEq)]
pub struct CompareDrawn {
    pub subtitle: Option<String>,
    pub body: CompareBody,
}

#[derive(Component)]
pub struct CompareBuilt(CompareDrawn);

/// The verdict box, for the side that looks at its fill.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerdictBox;

/// **Try again** of a main that could not be prepared: opens the Prepare main modal.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct TryMainAgain(pub PrRef);

/// A value's number and its unit: "20.4 s" is ("20.4", "s").
fn split_unit(value: &str) -> (&str, &str) {
    let at = value
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(value.len());
    (&value[..at], value[at..].trim())
}

/// " ±<n>" in the unit and with the decimals of the median's text, so "20.4 s" reads "±0.2"
/// and not "±200 ms", and a ".0" left out. When the spread moves the median into another unit, the spread is
/// written whole, with its own unit.
fn spread_text(row: &MetricRow, stat: &SideStat) -> Option<String> {
    if stat.spread == 0 {
        return None;
    }
    let value = metric_value_text(row.metric, stat.median);
    let upper = metric_value_text(row.metric, stat.median.saturating_add(stat.spread));
    let ((number, unit), (upper_number, upper_unit)) = (split_unit(&value), split_unit(&upper));
    let parsed = number
        .parse::<f64>()
        .ok()
        .zip(upper_number.parse::<f64>().ok());
    match parsed {
        Some((low, high)) if unit == upper_unit => {
            let decimals = number.split_once('.').map_or(0, |(_, d)| d.len());
            let spread = format!("{:.*}", decimals, high - low);
            // "6.0" reads "6": the column is narrow, and the decimal said nothing.
            let spread = match spread.strip_suffix(".0") {
                Some(whole) => whole.to_string(),
                None => spread,
            };
            Some(format!(" ±{spread}"))
        }
        _ => Some(format!(" ±{}", metric_value_text(row.metric, stat.spread))),
    }
}

fn stat_cell(row: &MetricRow, stat: Option<&SideStat>) -> Cell {
    match stat {
        Some(s) => Cell {
            value: metric_value_text(row.metric, s.median),
            spread: spread_text(row, s),
            ink: Swatch::Fg,
        },
        None => Cell::none(),
    }
}

fn pass_cell(pass: Option<&PassCount>) -> Cell {
    match pass {
        Some(p) => Cell::plain(
            format!("{} of {}", p.passed, p.rounds),
            if p.rounds > 0 && p.passed == p.rounds {
                Swatch::Green
            } else {
                Swatch::Orange
            },
        ),
        None => Cell::none(),
    }
}

fn result_row(pr: Option<&PassCount>, main: Option<&PassCount>) -> TableRow {
    TableRow {
        label: "Result".into(),
        main: pass_cell(main),
        pr: pass_cell(pr),
        delta: Cell::plain("—".into(), Swatch::Muted),
        faint: false,
    }
}

/// The freshness sentences of the sides that are not current, each once.
fn freshness_notes(sides: &[&SideView]) -> Vec<String> {
    let mut notes: Vec<String> = Vec::new();
    for side in sides {
        if let Some(line) = freshness_text(side.freshness)
            && !notes.iter().any(|n| n == line)
        {
            notes.push(line.to_string());
        }
    }
    notes
}

fn comparison_table(view: &CompareView, c: &ComparisonView) -> CompareBody {
    let mut rows = Vec::new();
    if c.pr.pass.is_some() || c.main.pass.is_some() {
        rows.push(result_row(c.pr.pass.as_ref(), c.main.pass.as_ref()));
    }
    for row in &c.rows {
        let ink = delta_tone(c, row).map_or(Swatch::Muted, |t| t.swatches().1);
        rows.push(TableRow {
            label: row.metric.label().into(),
            main: stat_cell(row, row.main.as_ref()),
            pr: stat_cell(row, row.pr.as_ref()),
            delta: Cell::plain(delta_text(&row.delta, row.main.as_ref()), ink),
            faint: false,
        });
    }
    if let Some(r) = &view.running {
        rows.push(TableRow {
            label: "Rounds".into(),
            main: Cell::plain(format!("{} of {}", r.main, r.of), Swatch::Faint),
            pr: Cell::plain(format!("{} of {}", r.pr, r.of), Swatch::Faint),
            delta: Cell::plain(String::new(), Swatch::Faint),
            faint: true,
        });
    }
    let mut notes = freshness_notes(&[&c.pr, &c.main]);
    if notes.is_empty() {
        notes.push(match view.runner {
            Some(runner) => format!(
                "Both sides at this head and base · {} · no network",
                runner_label(runner)
            ),
            None => "Both sides at this head and base · no network".into(),
        });
    }
    CompareBody::Table {
        rows,
        verdict: Some(VerdictDrawn {
            line: shown(&c.line, PURPOSE_SHOWN),
            tone: verdict_tone(c),
        }),
        notes,
        anti_goal: c.exercises == Some(false),
    }
}

fn last_table(last: &SideView) -> CompareBody {
    let row = match &last.pass {
        Some(pass) => result_row(Some(pass), None),
        None => TableRow {
            label: "Rounds".into(),
            main: Cell::none(),
            pr: Cell::plain(last.rounds.to_string(), Swatch::Fg),
            delta: Cell::plain("—".into(), Swatch::Muted),
            faint: false,
        },
    };
    CompareBody::Table {
        rows: vec![row],
        verdict: None,
        notes: freshness_notes(&[last]),
        anti_goal: false,
    }
}

fn main_body(view: &CompareView) -> CompareBody {
    let muted = |t: &str| (t.to_string(), Swatch::Muted);
    let (title, lines, try_again) = match &view.main {
        MainPhase::NotCreated => (
            None,
            vec![(
                format!(
                    "Run on main too prepares a second checkout at this PR's base, {}, with \
                     its own caches and its own setup approval. Its first run builds from \
                     scratch.",
                    view.base
                ),
                Swatch::Fg,
            )],
            false,
        ),
        MainPhase::NeedsSetup => (None, vec![muted(MAIN_FIRST)], false),
        MainPhase::Creating => (None, vec![muted("Preparing main…")], false),
        MainPhase::Preparing { .. } if view.first_build => {
            (None, vec![muted("Preparing main… first build")], false)
        }
        MainPhase::Preparing { step } => (
            None,
            vec![
                muted("Preparing main…"),
                (shown(step, PURPOSE_SHOWN), Swatch::Muted),
            ],
            false,
        ),
        MainPhase::Failed { message } => (
            Some("Main could not be prepared".to_string()),
            vec![(shown(message, PURPOSE_SHOWN), Swatch::Muted)],
            true,
        ),
        MainPhase::Unavailable { reason } => (
            None,
            vec![(shown(reason, PURPOSE_SHOWN), Swatch::Faint)],
            false,
        ),
        MainPhase::Ready => (None, Vec::new(), false),
    };
    CompareBody::Main {
        title,
        lines,
        try_again,
    }
}

/// What the card draws for a compare view.
pub fn compare_drawn(view: &CompareView) -> CompareDrawn {
    if let Some(c) = &view.comparison {
        return CompareDrawn {
            subtitle: Some(view.subtitle.clone()),
            body: comparison_table(view, c),
        };
    }
    if view.main != MainPhase::Ready {
        return CompareDrawn {
            subtitle: None,
            body: main_body(view),
        };
    }
    match &view.last {
        Some(last) => CompareDrawn {
            subtitle: Some(view.subtitle.clone()),
            body: last_table(last),
        },
        None => CompareDrawn {
            subtitle: None,
            body: CompareBody::Hint,
        },
    }
}

fn drawn_of(model: &CockpitModel, agent: &str, now: i64) -> Option<CompareDrawn> {
    model
        .loaded
        .then(|| compare_drawn(&compare_view(model, agent, now)))
}

/// Redraws the comparison card when what it draws changed.
pub fn fill_compare(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    clock: Res<Clock>,
    models: Res<CockpitModels>,
    fonts: Res<UiFonts>,
    regions: Query<(Entity, &CockpitCompare, Option<&CompareBuilt>)>,
) {
    let now = clock.now();
    for (entity, region, built) in &regions {
        let pr = &region.0;
        let Some(tab) = tabs.0.get(pr).filter(|t| t.ready().is_some()) else {
            continue;
        };
        let agent = agent_name::of_tab(&app_model.snapshot, tab);
        let Some(drawn) = models.0.get(pr).and_then(|m| drawn_of(m, &agent, now)) else {
            if built.is_some() {
                commands.entity(entity).despawn_related::<Children>();
                commands.entity(entity).remove::<CompareBuilt>();
            }
            continue;
        };
        if built.is_some_and(|b| b.0 == drawn) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        commands
            .entity(entity)
            .with_children(|p| draw_compare(p, &fonts, pr, &drawn));
        commands.entity(entity).insert(CompareBuilt(drawn));
    }
}

const HEADING: Type = Type {
    weight: 700,
    ..Type::STRONG
};
/// The widths of a value column and of the Δ column.
const VALUE_W: f32 = 88.0;
const DELTA_W: f32 = 46.0;
const NOTE: Type = Type::META.size(11.5);

fn draw_compare(p: &mut Spawner, fonts: &UiFonts, pr: &PrRef, drawn: &CompareDrawn) {
    p.spawn(Node {
        width: percent(100),
        flex_direction: FlexDirection::Column,
        row_gap: px(10),
        padding: UiRect::new(px(16), px(16), px(16), px(8)),
        ..default()
    })
    .with_children(|c| {
        c.spawn(Node {
            width: percent(100),
            justify_content: JustifyContent::SpaceBetween,
            align_items: AlignItems::Baseline,
            column_gap: px(8),
            ..default()
        })
        .with_children(|head| {
            head.spawn(text(fonts, "This PR vs main", HEADING));
            if let Some(subtitle) = &drawn.subtitle {
                head.spawn(text(fonts, subtitle.clone(), Type::META));
            }
        });
        match &drawn.body {
            CompareBody::Hint => {
                c.spawn(wrapped_text(fonts, COMPARE_HINT, Type::MUTED));
            }
            CompareBody::Main {
                title,
                lines,
                try_again,
            } => {
                if let Some(title) = title {
                    c.spawn(wrapped_text(
                        fonts,
                        title.clone(),
                        Type::STRONG.ink(Swatch::Orange),
                    ));
                }
                for (line, ink) in lines {
                    c.spawn(wrapped_text(fonts, line.clone(), Type::BODY.ink(*ink)));
                }
                if *try_again {
                    c.spawn(Node::default()).with_child((
                        button(fonts, "Try again", Variant::Secondary),
                        TryMainAgain(pr.clone()),
                        observe(on_try_again),
                    ));
                }
            }
            CompareBody::Table {
                rows,
                verdict,
                notes,
                anti_goal,
            } => {
                draw_table(c, fonts, rows);
                if let Some(v) = verdict {
                    let (fill, ink) = v.tone.swatches();
                    let weight = if v.tone == Tone::Neutral { 400 } else { 600 };
                    c.spawn((
                        Node {
                            width: percent(100),
                            padding: UiRect::axes(px(12), px(9)),
                            border_radius: BorderRadius::all(px(6)),
                            ..default()
                        },
                        BackgroundColor::default(),
                        Fill(fill),
                        VerdictBox,
                    ))
                    .with_child(wrapped_text(
                        fonts,
                        v.line.clone(),
                        Type {
                            weight,
                            ..Type::BODY.ink(ink)
                        },
                    ));
                }
                if !notes.is_empty() || *anti_goal {
                    c.spawn(Node {
                        width: percent(100),
                        flex_direction: FlexDirection::Column,
                        row_gap: px(2),
                        ..default()
                    })
                    .with_children(|n| {
                        for note in notes {
                            n.spawn(wrapped_text(fonts, note.clone(), NOTE));
                        }
                        if *anti_goal {
                            n.spawn(wrapped_text(
                                fonts,
                                ANTI_GOAL,
                                NOTE.ink(Tone::Amber.swatches().1),
                            ));
                        }
                    });
                }
            }
        }
    });
}

fn draw_table(p: &mut Spawner, fonts: &UiFonts, rows: &[TableRow]) {
    p.spawn(Node {
        width: percent(100),
        flex_direction: FlexDirection::Column,
        ..default()
    })
    .with_children(|t| {
        let head = TableRow {
            label: String::new(),
            main: Cell::plain("main".into(), Swatch::Faint),
            pr: Cell::plain("this PR".into(), Swatch::Faint),
            delta: Cell::plain("Δ".into(), Swatch::Faint),
            faint: true,
        };
        table_row(t, fonts, &head, true);
        for row in rows {
            table_row(t, fonts, row, false);
        }
    });
}

/// A row: the label takes what the three fixed columns leave. Labels are the table's own short
/// names and values the core's short texts, so none of them wraps.
fn table_row(p: &mut Spawner, fonts: &UiFonts, row: &TableRow, head: bool) {
    let size = if head { 11.5 } else { 13.0 };
    p.spawn((
        Node {
            width: percent(100),
            align_items: AlignItems::Baseline,
            padding: UiRect::vertical(px(if head { 4.0 } else { 6.0 })),
            border: UiRect::bottom(px(1)),
            ..default()
        },
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|r| {
        r.spawn(Node {
            flex_grow: 1.0,
            min_width: px(0),
            overflow: Overflow::clip_x(),
            ..default()
        })
        .with_child((
            text(
                fonts,
                row.label.clone(),
                Type::BODY
                    .size(size)
                    .ink(if row.faint { Swatch::Faint } else { Swatch::Fg }),
            ),
            TextLayout::no_wrap(),
        ));
        for (cell, width) in [
            (&row.main, VALUE_W),
            (&row.pr, VALUE_W),
            (&row.delta, DELTA_W),
        ] {
            r.spawn(Node {
                width: px(width),
                flex_shrink: 0.0,
                justify_content: JustifyContent::FlexEnd,
                align_items: AlignItems::Baseline,
                ..default()
            })
            .with_children(|c| {
                c.spawn((
                    text(
                        fonts,
                        cell.value.clone(),
                        Type::BODY.size(size).ink(cell.ink),
                    ),
                    TextLayout::no_wrap(),
                ));
                if let Some(spread) = &cell.spread {
                    c.spawn((
                        text(fonts, spread.clone(), Type::META.size(11.0)),
                        TextLayout::no_wrap(),
                    ));
                }
            });
        }
    });
}

fn on_try_again(
    activate: On<Activate>,
    buttons: Query<&TryMainAgain>,
    mut models: ResMut<CockpitModels>,
) {
    let Ok(TryMainAgain(pr)) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(model) = models.0.get_mut(pr) {
        open_prepare_main(model);
    }
}

/// The comparison card's system: it runs after the center's regions, when anything it reads
/// changed.
pub fn register(app: &mut App) {
    app.add_systems(
        Update,
        fill_compare
            .run_if(super::frame::regions_changed)
            .after(super::modals::cancel_cockpit_modal),
    );
}

#[cfg(test)]
mod tests {
    use clusia_core::cockpit::{CockpitView, ItemKind, ItemRef, Metric};

    use super::super::CockpitModal;
    use super::super::center::support::{self, REFRESH, scenario_item};
    use super::*;
    use crate::fixture::{DemoCockpit, demo_cockpit};
    use crate::testing::{self, NOW};

    fn scenario() -> CockpitView {
        demo_cockpit(NOW, DemoCockpit::Scenario)
    }

    fn steady(view: &mut CockpitView) -> &mut ComparisonView {
        view.scenarios[1].comparison.as_mut().expect("comparison")
    }

    fn app_on_refresh(view: CockpitView) -> (App, PrRef) {
        let (mut app, pr) = support::app_with(view);
        support::select(&mut app, &pr, scenario_item(REFRESH));
        (app, pr)
    }

    fn texts(app: &mut App) -> Vec<String> {
        let region = testing::find::<CockpitCompare>(app, |_| true);
        let mut out = Vec::new();
        for e in support::descendants(app, region) {
            if let Some(t) = app.world().get::<Text>(e) {
                out.push(t.0.clone());
            }
        }
        out
    }

    fn has(app: &mut App, want: &str) -> bool {
        texts(app).iter().any(|t| t == want)
    }

    fn verdict_fill(app: &mut App) -> Option<(Swatch, u16)> {
        let mut q = app
            .world_mut()
            .query_filtered::<(Entity, &Fill), With<VerdictBox>>();
        let (e, fill) = q.iter(app.world()).next()?;
        let fill = fill.0;
        let weight = support::descendants(app, e)
            .into_iter()
            .find_map(|c| app.world().get::<TextFont>(c))
            .map(|f| f.weight.0)?;
        Some((fill, weight))
    }

    #[test]
    fn the_worse_scenario_reads_as_the_mockup() {
        let (mut app, _pr) = app_on_refresh(scenario());
        let view = scenario();
        let c = view.scenarios[1].comparison.as_ref().expect("comparison");
        for want in [
            "This PR vs main",
            "median ± spread · 3 rounds",
            "main",
            "this PR",
            "Δ",
            "Result",
            "3 of 3",
            "Duration",
            "Peak memory",
            "CPU (avg)",
            "Worse than main: memory",
            "Both sides at this head and base · container · no network",
        ] {
            assert!(has(&mut app, want), "{want}: {:?}", texts(&mut app));
        }
        for row in &c.rows {
            assert!(has(&mut app, &delta_text(&row.delta, row.main.as_ref())));
        }
        assert!(has(&mut app, &c.line));
        assert!(has(&mut app, "+116%"));
        assert!(has(&mut app, "410.0 MB") && has(&mut app, " ±14"));
        assert!(
            has(&mut app, "20.4 s") && has(&mut app, " ±0.2"),
            "{:?}",
            texts(&mut app)
        );
        assert!(has(&mut app, "38%") && has(&mut app, " ±3"));
        assert_eq!(verdict_fill(&mut app), Some((Swatch::OrangeSoft, 600)));
    }

    #[test]
    fn worse_and_mixed_are_red_better_is_green_the_rest_neutral() {
        let mut view = scenario();
        let c = steady(&mut view).clone();
        let tone = |verdict: Verdict| {
            let mut c = c.clone();
            c.verdict = verdict;
            verdict_tone(&c)
        };
        let m = || vec![Metric::PeakMemory];
        assert_eq!(tone(Verdict::Worse { metrics: m() }), Tone::Orange);
        assert_eq!(
            tone(Verdict::Mixed {
                worse: m(),
                better: m()
            }),
            Tone::Orange
        );
        assert_eq!(
            tone(Verdict::PassDiffers {
                pr: false,
                main: true
            }),
            Tone::Orange
        );
        assert_eq!(tone(Verdict::Better { metrics: m() }), Tone::Green);
        assert_eq!(
            tone(Verdict::PassDiffers {
                pr: true,
                main: false
            }),
            Tone::Green
        );
        for v in [
            Verdict::NoDifference,
            Verdict::NotEnoughRounds,
            Verdict::Stopped,
            Verdict::Running,
        ] {
            assert_eq!(tone(v.clone()), Tone::Neutral, "{v:?}");
        }
        let worse_row = c
            .rows
            .iter()
            .find(|r| r.delta.label == DeltaLabel::Worse)
            .expect("a worse row");
        assert_eq!(delta_tone(&c, worse_row), Some(Tone::Orange));
        let mut better = worse_row.clone();
        better.delta.label = DeltaLabel::Better;
        assert_eq!(delta_tone(&c, &better), Some(Tone::Green));
        let same = c.rows.iter().find(|r| r.delta.label == DeltaLabel::Same);
        assert_eq!(delta_tone(&c, same.expect("a same row")), None);
    }

    #[test]
    fn a_running_stopped_or_thin_comparison_draws_no_colored_delta() {
        for verdict in [Verdict::Running, Verdict::Stopped, Verdict::NotEnoughRounds] {
            let mut view = scenario();
            let c = steady(&mut view);
            c.verdict = verdict.clone();
            c.line = clusia_core::cockpit::verdict_line(&verdict);
            let c = c.clone();
            for row in &c.rows {
                assert_eq!(delta_tone(&c, row), None, "{verdict:?} {:?}", row.metric);
            }
            assert!(c.rows.iter().any(|r| r.delta.label == DeltaLabel::Worse));
            let (mut app, _pr) = app_on_refresh(view);
            assert_eq!(
                verdict_fill(&mut app),
                Some((Swatch::Chrome, 400)),
                "{verdict:?}"
            );
            let drawn = compare_drawn_of(&mut app);
            let CompareBody::Table { rows, .. } = drawn.body else {
                panic!("a table");
            };
            assert!(
                rows.iter()
                    .all(|r| r.delta.ink == Swatch::Muted || r.delta.ink == Swatch::Faint),
                "{verdict:?}: {rows:?}"
            );
        }
    }

    fn compare_drawn_of(app: &mut App) -> CompareDrawn {
        let mut q = app.world_mut().query::<&CompareBuilt>();
        q.iter(app.world()).next().expect("built").0.clone()
    }

    #[test]
    fn a_stale_side_draws_no_colored_delta_and_says_why() {
        for (pr_side, fresh, line) in [
            (true, Freshness::OlderCommit, "From an older commit."),
            (false, Freshness::OlderBase, "From an older base."),
            (
                true,
                Freshness::ValuesChanged,
                "These values changed since this ran.",
            ),
            (false, Freshness::OtherRunner, "From another runner."),
        ] {
            let mut view = scenario();
            let c = steady(&mut view);
            if pr_side {
                c.pr.freshness = fresh;
            } else {
                c.main.freshness = fresh;
            }
            let c = c.clone();
            for row in &c.rows {
                assert_eq!(delta_tone(&c, row), None);
            }
            assert_eq!(verdict_tone(&c), Tone::Neutral);
            let (mut app, _pr) = app_on_refresh(view);
            assert!(has(&mut app, line), "{line}: {:?}", texts(&mut app));
            assert!(!has(
                &mut app,
                "Both sides at this head and base · container · no network"
            ));
            assert_eq!(verdict_fill(&mut app), Some((Swatch::Chrome, 400)));
        }
    }

    #[test]
    fn a_scenario_that_does_not_exercise_the_change_says_so_in_amber() {
        let mut view = scenario();
        steady(&mut view).exercises = Some(false);
        let (mut app, _pr) = app_on_refresh(view);
        assert!(has(&mut app, ANTI_GOAL));
        let region = testing::find::<CockpitCompare>(&mut app, |_| true);
        let ink = support::descendants(&app, region)
            .into_iter()
            .find(|e| {
                app.world()
                    .get::<Text>(*e)
                    .is_some_and(|t| t.0 == ANTI_GOAL)
            })
            .and_then(|e| app.world().get::<crate::ui::kit::Ink>(e).copied());
        assert_eq!(ink.map(|i| i.0), Some(Tone::Amber.swatches().1));
    }

    #[test]
    fn while_running_a_rounds_row_counts_per_side() {
        let view = demo_cockpit(NOW, DemoCockpit::BenchRunning);
        let item = ItemRef {
            kind: ItemKind::Bench,
            id: view.benches[0].slug.clone(),
        };
        let (mut app, pr) = support::app_with(view);
        support::select(&mut app, &pr, item);
        assert!(has(&mut app, "Rounds"));
        assert!(has(&mut app, "2 of 5") && has(&mut app, "3 of 5"));
        assert!(has(&mut app, "median ± spread · rounds so far"));
        assert!(has(&mut app, "Running: no verdict yet"));
        assert_eq!(verdict_fill(&mut app), Some((Swatch::Chrome, 400)));
    }

    #[test]
    fn no_comparison_and_no_last_says_how_to_compare() {
        let (mut app, pr) = support::app_with(scenario());
        support::select(&mut app, &pr, scenario_item("token-endpoint-is-down"));
        assert!(has(&mut app, COMPARE_HINT), "{:?}", texts(&mut app));
        assert_eq!(verdict_fill(&mut app), None);
    }

    #[test]
    fn a_last_without_a_comparison_shows_only_the_pr_column() {
        let mut view = scenario();
        view.scenarios[1].comparison = None;
        let (mut app, _pr) = app_on_refresh(view);
        let drawn = compare_drawn_of(&mut app);
        let CompareBody::Table { rows, verdict, .. } = drawn.body else {
            panic!("a table: {drawn:?}");
        };
        assert_eq!(verdict, None);
        assert_eq!(rows[0].label, "Result");
        assert_eq!(rows[0].pr.value, "3 of 3");
        assert_eq!(rows[0].main.value, "—");
        assert_eq!(rows[0].delta.value, "—");
        assert_eq!(verdict_fill(&mut app), None);
    }

    fn with_main(phase: MainPhase, first_build: bool) -> CockpitView {
        let mut view = scenario();
        view.scenarios[1].comparison = None;
        view.main.phase = phase;
        view.main.first_build = first_build;
        view
    }

    #[test]
    fn main_not_ready_shows_its_text_instead_of_the_table() {
        let base: String = scenario().base.expect("a base").chars().take(7).collect();
        let (mut app, _pr) = app_on_refresh(with_main(MainPhase::NotCreated, false));
        let sentence = format!(
            "Run on main too prepares a second checkout at this PR's base, {base}, with its own \
             caches and its own setup approval. Its first run builds from scratch."
        );
        assert!(has(&mut app, &sentence), "{:?}", texts(&mut app));
        assert!(!has(&mut app, "Result"));
        let (mut app, _pr) = app_on_refresh(with_main(MainPhase::NeedsSetup, true));
        assert!(has(&mut app, MAIN_FIRST));
        let step = || MainPhase::Preparing {
            step: "Fetch dependencies".into(),
        };
        let (mut app, _pr) = app_on_refresh(with_main(step(), true));
        assert!(has(&mut app, "Preparing main… first build"));
        let (mut app, _pr) = app_on_refresh(with_main(step(), false));
        assert!(has(&mut app, "Preparing main…") && has(&mut app, "Fetch dependencies"));
        let failed = MainPhase::Failed {
            message: "cargo fetch failed".into(),
        };
        let (mut app, _pr) = app_on_refresh(with_main(failed, false));
        assert!(has(&mut app, "Main could not be prepared"));
        assert!(has(&mut app, "cargo fetch failed"));
        assert!(has(&mut app, "Try again"));
        assert_eq!(testing::count::<TryMainAgain>(&mut app), 1);
    }

    #[test]
    fn try_again_reopens_the_prepare_main_modal() {
        let failed = MainPhase::Failed {
            message: "boom".into(),
        };
        let (mut app, pr) = app_on_refresh(with_main(failed, false));
        support::click::<TryMainAgain>(&mut app, |b| b.0 == pr);
        assert!(matches!(
            support::model(&app, &pr).modal,
            Some(CockpitModal::PrepareMain { .. })
        ));
        assert_eq!(testing::recorded(&mut app), []);
    }

    #[test]
    fn hostile_text_in_errors_and_notes_is_plain() {
        let hostile = "[x](http://evil) \u{202e}**";
        let mut view = with_main(
            MainPhase::Failed {
                message: format!("{hostile}{}", "m".repeat(10_000)),
            },
            false,
        );
        view.scenarios[0]
            .comparison
            .as_mut()
            .expect("comparison")
            .line = format!("{hostile}{}", "n".repeat(10_000));
        let (mut app, pr) = app_on_refresh(view);
        let check = |app: &mut App| {
            let all = texts(app);
            assert!(
                all.iter().any(|t| t.contains("[x](http://evil)")),
                "{all:?}"
            );
            for t in &all {
                assert!(!t.contains('\u{202e}'), "{t}");
                assert!(t.chars().count() <= PURPOSE_SHOWN + 1, "{}", t.len());
            }
            assert_eq!(testing::count::<crate::ui::markdown::MdLink>(app), 0);
        };
        check(&mut app);
        support::select(&mut app, &pr, scenario_item("two-requests-refresh-at-once"));
        assert!(
            texts(&mut app)
                .iter()
                .any(|t| t.starts_with("[x](http://evil)"))
        );
        check(&mut app);
    }
}
