//! This PR vs main: the table of medians, spreads and deltas of the selected item, its verdict and
//! the notes under it, or what main needs before there can be one. A color says better or worse
//! only through `verdict_tone` and `delta_tone`, which give none while the comparison is no
//! verdict or either side is about something else than what is under review now. Every text the
//! daemon sent goes through `shown` before it is drawn.

use bevy::prelude::*;
use bevy::ui_widgets::{Activate, observe};
use clusia_core::PrRef;
use clusia_core::cockpit::{
    ComparisonView, DeltaLabel, Freshness, MAIN_FIRST, MainPhase, Metric, MetricRow, PassCount,
    SideStat, SideView, Verdict, delta_text, metric_value_text,
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

/// A cell of the table, for the side that measures it.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableCell;

/// The step a calls/s value is written in: (thousandths per unit shown, decimals, suffix).
fn calls_tier(milli: u64) -> (f64, usize, &'static str) {
    let calls = milli as f64 / 1000.0;
    if (calls * 10.0).round() < 10_000.0 {
        (1_000.0, 1, "")
    } else if calls.round() < 10_000.0 {
        (1_000.0, 0, "")
    } else if (calls / 1000.0).round() < 1000.0 {
        (1_000_000.0, 0, " K")
    } else {
        (1_000_000_000.0, 1, " M")
    }
}

/// `n` with a space between each group of three digits: "1 840".
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// A number with `decimals` decimals, a ".0" left out.
fn decimal(value: f64, decimals: usize) -> String {
    let text = format!("{value:.decimals$}");
    match text.strip_suffix(".0") {
        Some(whole) => whole.to_string(),
        None => text,
    }
}

/// Calls a second, from the thousandths `metric_value_text` takes, short enough for a cell:
/// "12.5", "1 840", "950 K", "21.9 M". The row's label says the unit.
pub fn calls_text(milli: u64) -> String {
    let (per, decimals, suffix) = calls_tier(milli);
    let value = milli as f64 / per;
    if decimals == 0 && suffix.is_empty() {
        return grouped(value.round() as u64);
    }
    if suffix.is_empty() {
        decimal(value, decimals)
    } else {
        format!("{value:.decimals$}{suffix}")
    }
}

/// A value of `metric` as a cell shows it.
fn value_text(metric: Metric, value: u64) -> String {
    match metric {
        Metric::CallsPerSec => calls_text(value),
        _ => metric_value_text(metric, value),
    }
}

/// The unit a median of `metric` is written in, as (raw units per unit shown, decimals): what
/// `metric_value_text` (or `calls_text`) picks for that median.
fn unit_of(metric: Metric, median: u64) -> (f64, usize) {
    match metric {
        Metric::Duration if median < 1000 => (1.0, 0),
        Metric::Duration => (1000.0, 1),
        Metric::P50 | Metric::P95 => match median {
            0..1_000 => (1.0, 0),
            1_000..999_950 => (1e3, 1),
            999_950..999_950_000 => (1e6, 1),
            _ => (1e9, 2),
        },
        Metric::CallsPerSec => {
            let (per, decimals, _) = calls_tier(median);
            (per, decimals)
        }
        Metric::PeakMemory => (1024.0, 1),
        Metric::AvgCpu => (10.0, 0),
    }
}

/// The verdict box, for the side that looks at its fill.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerdictBox;

/// **Try again** of a main that could not be prepared: opens the Prepare main modal.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct TryMainAgain(pub PrRef);

/// " ±<n>": the spread rounded on its own in the median's unit and decimals ("20.4 s ±0.2",
/// "190.0 MB ±6"), a ".0" left out; `None` when it rounds to nothing.
fn spread_text(row: &MetricRow, stat: &SideStat) -> Option<String> {
    let (per, decimals) = unit_of(row.metric, stat.median);
    let text = decimal(stat.spread as f64 / per, decimals);
    (text.chars().any(|c| c.is_ascii_digit() && c != '0')).then(|| format!(" ±{text}"))
}

fn stat_cell(row: &MetricRow, stat: Option<&SideStat>) -> Cell {
    match stat {
        Some(s) => Cell {
            value: value_text(row.metric, s.median),
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

/// A run on this PR alone: its pass count and the numbers of its last round, in the PR column.
fn last_table(last: &SideView) -> CompareBody {
    let mut rows = Vec::new();
    if let Some(pass) = &last.pass {
        rows.push(result_row(Some(pass), None));
    }
    let points = &last.series.points;
    let peak = points.iter().filter_map(|p| p.rss_kb).max();
    let cpu: Vec<u64> = points
        .iter()
        .filter_map(|p| p.cpu_milli.map(u64::from))
        .collect();
    let mean = (!cpu.is_empty()).then(|| cpu.iter().sum::<u64>() / cpu.len() as u64);
    for (metric, value) in [(Metric::PeakMemory, peak), (Metric::AvgCpu, mean)] {
        if let Some(value) = value {
            rows.push(TableRow {
                label: metric.label().into(),
                main: Cell::none(),
                pr: Cell::plain(value_text(metric, value), Swatch::Fg),
                delta: Cell::plain("—".into(), Swatch::Muted),
                faint: false,
            });
        }
    }
    CompareBody::Table {
        rows,
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
const VALUE_W: f32 = 90.0;
const DELTA_W: f32 = 46.0;
const NOTE: Type = Type::META.size(11.5);

fn draw_compare(p: &mut Spawner, fonts: &UiFonts, pr: &PrRef, drawn: &CompareDrawn) {
    p.spawn(Node {
        width: percent(100),
        flex_direction: FlexDirection::Column,
        row_gap: px(10),
        padding: UiRect::new(px(12), px(12), px(16), px(8)),
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
        r.spawn((
            Node {
                flex_grow: 1.0,
                min_width: px(0),
                ..default()
            },
            TableCell,
        ))
        .with_child(wrapped_text(
            fonts,
            row.label.clone(),
            Type::BODY
                .size(size)
                .ink(if row.faint { Swatch::Faint } else { Swatch::Fg }),
        ));
        for (cell, width) in [
            (&row.main, VALUE_W),
            (&row.pr, VALUE_W),
            (&row.delta, DELTA_W),
        ] {
            r.spawn((
                Node {
                    width: px(width),
                    flex_shrink: 0.0,
                    justify_content: JustifyContent::FlexEnd,
                    align_items: AlignItems::Baseline,
                    ..default()
                },
                TableCell,
            ))
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
    use bevy::math::UVec2;
    use clusia_core::cockpit::{CockpitView, ItemKind, ItemRef};

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
        // Then the last run's own numbers, PR column only.
        assert_eq!(rows[1].label, "Peak memory");
        assert_eq!(rows[1].pr.value, "410.0 MB");
        assert_eq!(rows[2].label, "CPU (avg)");
        assert_eq!(rows[2].pr.value, "41%");
        assert!(
            rows[1..]
                .iter()
                .all(|r| r.main.value == "—" && r.delta.value == "—")
        );
        assert_eq!(verdict_fill(&mut app), None);
    }

    fn stat(median: u64, spread: u64) -> SideStat {
        SideStat {
            median,
            spread,
            n: 3,
        }
    }

    fn spread_of(metric: Metric, median: u64, spread: u64) -> Option<String> {
        let s = stat(median, spread);
        let row = MetricRow {
            metric,
            main: Some(s),
            pr: Some(s),
            delta: clusia_core::cockpit::Delta {
                label: DeltaLabel::Same,
                percent: Some(0),
            },
        };
        spread_text(&row, &s)
    }

    #[test]
    fn a_spread_is_rounded_on_its_own_in_the_medians_unit() {
        assert_eq!(
            spread_of(Metric::Duration, 20_400, 200).as_deref(),
            Some(" ±0.2")
        );
        assert_eq!(
            spread_of(Metric::Duration, 20_400, 40),
            None,
            "0.04 s is ±0"
        );
        assert_eq!(spread_of(Metric::Duration, 20_440, 20), None);
        assert_eq!(
            spread_of(Metric::Duration, 20_440, 60).as_deref(),
            Some(" ±0.1")
        );
        assert_eq!(
            spread_of(Metric::Duration, 900, 200).as_deref(),
            Some(" ±200")
        );
        assert_eq!(
            spread_of(Metric::PeakMemory, 194_560, 6_144).as_deref(),
            Some(" ±6")
        );
        assert_eq!(
            spread_of(Metric::PeakMemory, 419_840, 14_336).as_deref(),
            Some(" ±14")
        );
        assert_eq!(
            spread_of(Metric::PeakMemory, 3_072, 300).as_deref(),
            Some(" ±0.3")
        );
        assert_eq!(spread_of(Metric::AvgCpu, 380, 30).as_deref(), Some(" ±3"));
        assert_eq!(spread_of(Metric::AvgCpu, 380, 4), None);
        assert_eq!(spread_of(Metric::P50, 41, 2).as_deref(), Some(" ±2"));
        assert_eq!(spread_of(Metric::P50, 2_500, 300).as_deref(), Some(" ±0.3"));
        assert_eq!(spread_of(Metric::Duration, 20_400, 0), None);
    }

    #[test]
    fn calls_per_second_read_short() {
        assert_eq!(calls_text(21_900_000_000), "21.9 M");
        assert_eq!(calls_text(21_100_000_000), "21.1 M");
        assert_eq!(calls_text(1_840_000), "1 840");
        assert_eq!(calls_text(950_000_000), "950 K");
        assert_eq!(calls_text(12_500), "12.5");
        assert_eq!(calls_text(12_000), "12");
        assert_eq!(calls_text(999_499_999), "999 K");
        assert_eq!(calls_text(999_500_000), "1.0 M");
        assert_eq!(calls_text(9_999_000), "9 999");
        assert_eq!(calls_text(10_000_000), "10 K");
    }

    #[test]
    fn no_table_cell_runs_past_its_column() {
        for state in [DemoCockpit::Scenario, DemoCockpit::BenchRunning] {
            let mut app = testing::layout_app(crate::fixture::demo(NOW), UVec2::new(1280, 800));
            let pr = testing::open_ready(&mut app, false);
            testing::set_section(&mut app, &pr, crate::review_state::ReviewSection::Tests);
            let view = demo_cockpit(NOW, state);
            let item = match state {
                DemoCockpit::BenchRunning => ItemRef {
                    kind: ItemKind::Bench,
                    id: view.benches[0].slug.clone(),
                },
                _ => scenario_item(REFRESH),
            };
            testing::open_cockpit(&mut app, &pr, view);
            support::select(&mut app, &pr, item);
            testing::settle(&mut app);
            if state == DemoCockpit::BenchRunning {
                assert!(has(&mut app, "21.9 M") && has(&mut app, "21.1 M"));
            }
            let region = testing::find::<CockpitCompare>(&mut app, |_| true);
            let cells = cell_boxes(&app, region);
            assert!(!cells.is_empty());
            for (text, (tx, tw), (cx, cw)) in cells {
                assert!(
                    tx >= cx - 0.5 && tx + tw <= cx + cw + 0.5,
                    "{state:?}: {text:?} at {tx}+{tw} in {cx}+{cw}"
                );
            }
        }
    }

    /// Every text of the table with the box of the cell it sits in, as (left, width).
    fn cell_boxes(app: &App, region: Entity) -> Vec<(String, (f32, f32), (f32, f32))> {
        let mut out = Vec::new();
        for e in support::descendants(app, region) {
            if app.world().get::<TableCell>(e).is_none() {
                continue;
            }
            let (cx, _, cw, _) = testing::frame(app, e);
            for t in support::descendants(app, e) {
                let Some(text) = app.world().get::<Text>(t) else {
                    continue;
                };
                let (tx, _, tw, _) = testing::frame(app, t);
                out.push((text.0.clone(), (tx, tw), (cx, cw)));
            }
        }
        out
    }

    #[test]
    fn a_bench_with_only_a_last_run_shows_its_numbers_in_the_pr_column() {
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        let slug = view.benches[0].slug.clone();
        view.benches[0].comparison = None;
        let (mut app, pr) = support::app_with(view);
        support::select(
            &mut app,
            &pr,
            ItemRef {
                kind: ItemKind::Bench,
                id: slug,
            },
        );
        let drawn = compare_drawn_of(&mut app);
        let CompareBody::Table { rows, verdict, .. } = drawn.body else {
            panic!("a table: {drawn:?}");
        };
        assert_eq!(verdict, None);
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["Peak memory", "CPU (avg)"]);
        assert!(!labels.contains(&"Rounds"));
        assert_eq!(rows[0].pr.value, "3.0 MB");
        assert_eq!(rows[1].pr.value, "99%");
        for r in &rows {
            assert_eq!((r.main.value.as_str(), r.delta.value.as_str()), ("—", "—"));
        }
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
