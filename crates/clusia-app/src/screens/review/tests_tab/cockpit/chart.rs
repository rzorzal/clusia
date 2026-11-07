//! Memory and CPU during the run: two small charts under the selected item, this PR's line solid
//! and main's dashed. The lines are plain UI nodes, one thin rotated node per segment, so the
//! chart costs nothing while it does not change. The region keeps the whole-pixel plots it drew
//! and redraws only when one of them moves, so a sample that moves no pixel redraws nothing.

use bevy::prelude::*;
use clusia_core::cockpit::{Metric, RunSide, Sample, metric_value_text};

use super::frame::CockpitChart;
use super::views::{ChartView, chart_view};
use super::{CockpitModel, CockpitModels};
use crate::agent_name;
use crate::bridge::Model;
use crate::clock::Clock;
use crate::fonts::UiFonts;
use crate::review_state::ReviewTabs;
use crate::screens::review::tests_tab::plan::Spawner;
use crate::theme::Swatch;
use crate::ui::kit::{Fill, Type, panel, text};
use crate::ui::spinner::spinner;

/// The plot area of one chart, in pixels.
pub const PLOT_W: f32 = 220.0;
pub const PLOT_H: f32 = 56.0;

/// The thickness of this PR's line and of main's.
const PR_LINE: f32 = 2.0;
const MAIN_LINE: f32 = 1.6;
/// Main's dashes: this long, with a gap as long between two.
const DASH: f32 = 5.0;

/// One straight piece of a line: its center, its length and its angle (radians, y up).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub x: f32,
    pub y: f32,
    pub len: f32,
    pub angle: f32,
}

/// One chart as drawn: its points in whole pixels of the plot area, y up from 0.
#[derive(Debug, Clone, PartialEq)]
pub struct Plot {
    pub label: &'static str,
    pub legend: String,
    pub worse: bool,
    pub pr: Vec<(i32, i32)>,
    pub main: Vec<(i32, i32)>,
}

/// What the region drew: the header's state and both plots.
#[derive(Debug, Clone, PartialEq)]
pub struct ChartDrawn {
    pub live: bool,
    pub spinner: bool,
    pub rounds: u32,
    pub plots: Vec<Plot>,
}

#[derive(Component)]
pub struct ChartBuilt(ChartDrawn);

/// A segment node of a chart, for the side and the metric it draws.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChartSegment {
    pub metric: Metric,
    pub side: RunSide,
}

/// Points scaled into the plot area, rounded half up to whole pixels.
pub fn plot_points(points: &[(u32, u64)], x_max_ms: u32, y_max: u64) -> Vec<(i32, i32)> {
    let x_max = f64::from(x_max_ms.max(1));
    let y_max = y_max.max(1) as f64;
    points
        .iter()
        .map(|&(t, v)| {
            let x = (f64::from(t) * f64::from(PLOT_W) / x_max + 0.5).floor();
            let y = (v as f64 * f64::from(PLOT_H) / y_max + 0.5).floor();
            (x as i32, y as i32)
        })
        .collect()
}

/// The top of the y scale: the larger side's highest value with a tenth of room above it.
pub(super) fn y_max(pr: &[u64], main: &[u64]) -> u64 {
    let top = pr.iter().chain(main).copied().max().unwrap_or(0);
    top.saturating_mul(11).div_ceil(10).max(1)
}

/// The segments between consecutive points.
pub fn segments(px: &[(i32, i32)]) -> Vec<Segment> {
    px.windows(2)
        .map(|w| {
            let (a, b) = (w[0], w[1]);
            let (dx, dy) = ((b.0 - a.0) as f32, (b.1 - a.1) as f32);
            Segment {
                x: (a.0 + b.0) as f32 / 2.0,
                y: (a.1 + b.1) as f32 / 2.0,
                len: dx.hypot(dy),
                angle: dy.atan2(dx),
            }
        })
        .collect()
}

/// Every other `DASH`-long piece of `seg`, from its start: the dashes of main's line.
pub fn dashed(seg: &Segment) -> Vec<Segment> {
    dashed_from(seg, 0.0).0
}

/// The dashes of `seg` when the line before it ended `phase` pixels into a dash and its gap,
/// and the phase it ends at: a line of many short segments stays dashed.
fn dashed_from(seg: &Segment, phase: f32) -> (Vec<Segment>, f32) {
    let period = 2.0 * DASH;
    let (dx, dy) = (seg.angle.cos(), seg.angle.sin());
    let (sx, sy) = (seg.x - dx * seg.len / 2.0, seg.y - dy * seg.len / 2.0);
    let mut out = Vec::new();
    // Where along the segment the dash under way, or the next one, starts.
    let mut at = -phase;
    while at < seg.len {
        let start = at.max(0.0);
        let end = (at + DASH).min(seg.len);
        if end > start {
            let mid = (start + end) / 2.0;
            out.push(Segment {
                x: sx + dx * mid,
                y: sy + dy * mid,
                len: end - start,
                angle: seg.angle,
            });
        }
        at += period;
    }
    (out, (phase + seg.len) % period)
}

/// The dashes of a whole line of points, its phase carried from one segment to the next.
pub fn dashed_line(px: &[(i32, i32)]) -> Vec<Segment> {
    let mut phase = 0.0;
    let mut out = Vec::new();
    for seg in segments(px) {
        let (pieces, next) = dashed_from(&seg, phase);
        out.extend(pieces);
        phase = next;
    }
    out
}

/// The values a metric reads from samples, with their time; samples without one are skipped.
fn values(metric: Metric, samples: &[Sample]) -> Vec<(u32, u64)> {
    samples
        .iter()
        .filter_map(|s| {
            let v = match metric {
                Metric::PeakMemory => s.rss_kb,
                Metric::AvgCpu => s.cpu_milli.map(u64::from),
                _ => None,
            }?;
            Some((s.t_ms, v))
        })
        .collect()
}

/// A side's number for the legend: the peak for memory, the mean for the CPU.
fn summary(metric: Metric, vals: &[(u32, u64)]) -> Option<String> {
    if vals.is_empty() {
        return None;
    }
    let v = match metric {
        Metric::AvgCpu => vals.iter().map(|(_, v)| v).sum::<u64>() / vals.len() as u64,
        _ => vals.iter().map(|(_, v)| *v).max().unwrap_or(0),
    };
    Some(metric_value_text(metric, v))
}

/// Kilobytes as whole megabytes, rounded half up.
fn whole_mb(kb: u64) -> u64 {
    kb.saturating_add(512) / 1024
}

/// The legend of a chart: "<first> → <last> MB" for a PR memory that grows on a worse row,
/// otherwise "<pr> · main <main>".
pub fn legend_text(metric: Metric, pr: &[Sample], main: &[Sample], worse: bool) -> String {
    let pr_vals = values(metric, pr);
    if metric == Metric::PeakMemory
        && worse
        && let (Some(first), Some(last)) = (pr_vals.first(), pr_vals.last())
        && last.1 > first.1
    {
        return format!("{} → {} MB", whole_mb(first.1), whole_mb(last.1));
    }
    let main_vals = values(metric, main);
    let parts: Vec<String> = [
        summary(metric, &pr_vals),
        summary(metric, &main_vals).map(|m| format!("main {m}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    parts.join(" · ")
}

/// Whole-pixel points with no point drawn twice in a row: a repeated pixel is no segment.
fn pixels(vals: &[(u32, u64)], x_max: u32, top: u64) -> Vec<(i32, i32)> {
    let mut px = plot_points(vals, x_max, top);
    px.dedup();
    px
}

fn plot(metric: Metric, label: &'static str, worse: bool, view: &ChartView) -> Plot {
    let pr = values(metric, &view.pr);
    let main = values(metric, &view.main);
    let x_max = pr
        .iter()
        .chain(&main)
        .map(|(t, _)| *t)
        .max()
        .unwrap_or(0)
        .max(1);
    let only = |v: &[(u32, u64)]| v.iter().map(|(_, v)| *v).collect::<Vec<_>>();
    let top = y_max(&only(&pr), &only(&main));
    Plot {
        label,
        legend: legend_text(metric, &view.pr, &view.main, worse),
        worse,
        pr: pixels(&pr, x_max, top),
        main: pixels(&main, x_max, top),
    }
}

/// Both charts of a chart view.
pub fn plots(view: &ChartView) -> Vec<Plot> {
    vec![
        plot(Metric::PeakMemory, "Memory", view.memory_worse, view),
        plot(Metric::AvgCpu, "CPU", view.cpu_worse, view),
    ]
}

/// What the chart of the selected item draws, or `None` with no item selected.
pub fn chart_drawn(model: &CockpitModel, agent: &str, now: i64) -> Option<ChartDrawn> {
    if model.selected_scenario().is_none() && model.selected_bench().is_none() {
        return None;
    }
    let view = chart_view(model, agent, now);
    let running_here = model.selected.is_some() && model.running_item() == model.selected.as_ref();
    let live = view.live || running_here;
    let active = model.view.as_ref().is_some_and(|v| v.activity.is_some());
    Some(ChartDrawn {
        live,
        spinner: live && active,
        rounds: view.rounds,
        plots: plots(&view),
    })
}

/// Redraws the chart when one of its plots or its header changed.
pub fn fill_chart(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    clock: Res<Clock>,
    models: Res<CockpitModels>,
    fonts: Res<UiFonts>,
    regions: Query<(Entity, &CockpitChart, Option<&ChartBuilt>)>,
) {
    let now = clock.now();
    for (entity, region, built) in &regions {
        let pr = &region.0;
        let Some(tab) = tabs.0.get(pr).filter(|t| t.ready().is_some()) else {
            continue;
        };
        let agent = agent_name::of_tab(&app_model.snapshot, tab);
        let drawn = models.0.get(pr).and_then(|m| chart_drawn(m, &agent, now));
        let Some(drawn) = drawn else {
            if built.is_some() {
                commands.entity(entity).despawn_related::<Children>();
                commands.entity(entity).remove::<ChartBuilt>();
            }
            continue;
        };
        if built.is_some_and(|b| b.0 == drawn) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        commands
            .entity(entity)
            .with_children(|p| draw_chart(p, &fonts, &drawn));
        commands.entity(entity).insert(ChartBuilt(drawn));
    }
}

fn draw_chart(p: &mut Spawner, fonts: &UiFonts, drawn: &ChartDrawn) {
    p.spawn(Node {
        width: percent(100),
        flex_direction: FlexDirection::Column,
        row_gap: px(8),
        padding: UiRect::new(px(18), px(18), px(14), px(4)),
        ..default()
    })
    .with_children(|c| {
        c.spawn(Node {
            width: percent(100),
            justify_content: JustifyContent::SpaceBetween,
            align_items: AlignItems::Center,
            column_gap: px(8),
            ..default()
        })
        .with_children(|head| {
            head.spawn(text(fonts, "Memory and CPU during the run", Type::STRONG));
            head.spawn(Node {
                align_items: AlignItems::Center,
                column_gap: px(12),
                ..default()
            })
            .with_children(|right| {
                right.spawn(text(fonts, "— this PR", Type::META.ink(Swatch::Muted)));
                right.spawn(text(fonts, "- - main", Type::META.ink(Swatch::Muted)));
                if drawn.live {
                    right
                        .spawn(Node {
                            align_items: AlignItems::Center,
                            column_gap: px(6),
                            ..default()
                        })
                        .with_children(|live| {
                            if drawn.spinner {
                                live.spawn(spinner());
                            }
                            live.spawn(text(fonts, "live", Type::META.ink(Swatch::Green)));
                        });
                } else if drawn.rounds > 0 {
                    right.spawn(text(
                        fonts,
                        format!("median round of {}", drawn.rounds),
                        Type::META,
                    ));
                }
            });
        });
        c.spawn(Node {
            width: percent(100),
            column_gap: px(12),
            ..default()
        })
        .with_children(|row| {
            for (plot, metric) in drawn.plots.iter().zip([Metric::PeakMemory, Metric::AvgCpu]) {
                draw_plot(row, fonts, plot, metric);
            }
        });
    });
}

fn draw_plot(p: &mut Spawner, fonts: &UiFonts, plot: &Plot, metric: Metric) {
    p.spawn(panel(
        Node {
            flex_grow: 1.0,
            flex_basis: px(0),
            min_width: px(0),
            flex_direction: FlexDirection::Column,
            row_gap: px(8),
            padding: UiRect::all(px(10)),
            border_radius: BorderRadius::all(px(8)),
            ..default()
        },
        Swatch::Chrome,
    ))
    .with_children(|c| {
        c.spawn(Node {
            width: percent(100),
            justify_content: JustifyContent::SpaceBetween,
            column_gap: px(8),
            ..default()
        })
        .with_children(|legend| {
            legend.spawn(text(fonts, plot.label, Type::META.ink(Swatch::Muted)));
            let ink = if plot.worse {
                Swatch::Orange
            } else {
                Swatch::Muted
            };
            legend.spawn(text(fonts, plot.legend.clone(), Type::META.ink(ink)));
        });
        c.spawn(Node {
            width: px(PLOT_W),
            height: px(PLOT_H),
            align_self: AlignSelf::Center,
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|area| {
            if plot.pr.len() < 2 && plot.main.len() < 2 {
                area.spawn(text(fonts, "not measured", Type::META));
                return;
            }
            let pr_ink = if plot.worse {
                Swatch::Orange
            } else {
                Swatch::Fg
            };
            for seg in dashed_line(&plot.main) {
                line(area, seg, MAIN_LINE, Swatch::Faint, metric, RunSide::Main);
            }
            for seg in segments(&plot.pr) {
                line(area, seg, PR_LINE, pr_ink, metric, RunSide::Pr);
            }
        });
    });
}

/// One segment as a thin node, centered on the segment and turned to its angle. The plot's y
/// goes up and the screen's down, so the node turns the other way.
fn line(p: &mut Spawner, seg: Segment, thick: f32, ink: Swatch, metric: Metric, side: RunSide) {
    p.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(seg.x - seg.len / 2.0),
            top: px(PLOT_H - seg.y - thick / 2.0),
            width: px(seg.len),
            height: px(thick),
            ..default()
        },
        UiTransform::from_rotation(Rot2::radians(-seg.angle)),
        BackgroundColor::default(),
        Fill(ink),
        Pickable::IGNORE,
        ChartSegment { metric, side },
    ));
}

/// The chart's system: it runs after the center's regions, when anything they read changed.
pub fn register(app: &mut App) {
    app.add_systems(
        Update,
        fill_chart
            .run_if(super::frame::regions_changed)
            .after(super::modals::cancel_cockpit_modal),
    );
}

#[cfg(test)]
mod tests {
    use clusia_core::cockpit::{CockpitView, DeltaLabel, ItemKind, ItemRef, SERIES_MAX, Series};

    use clusia_core::PrRef;

    use super::super::center::support::{self, REFRESH, scenario_item};
    use super::super::frame::{CockpitCenter, CockpitCompare, CockpitCopilot, CockpitList};
    use super::*;
    use crate::bridge::CockpitTell;
    use crate::fixture::{DemoCockpit, demo_cockpit};
    use crate::testing::{self, NOW};
    use crate::ui::spinner::Spinner;

    fn app_with(view: CockpitView) -> (App, PrRef) {
        support::app_with(view)
    }

    fn sample(t_ms: u32, rss_kb: u64, cpu_milli: u32) -> Sample {
        Sample {
            t_ms,
            rss_kb: Some(rss_kb),
            cpu_milli: Some(cpu_milli),
        }
    }

    fn children_of<C: Component>(app: &mut App) -> Vec<Entity> {
        let e = testing::find::<C>(app, |_| true);
        app.world()
            .get::<Children>(e)
            .map(|c| c.iter().collect())
            .unwrap_or_default()
    }

    fn segments_of(app: &mut App, metric: Metric, side: RunSide) -> Vec<Swatch> {
        let mut q = app.world_mut().query::<(&ChartSegment, &Fill)>();
        q.iter(app.world())
            .filter(|(s, _)| s.metric == metric && s.side == side)
            .map(|(_, f)| f.0)
            .collect()
    }

    fn texts(app: &mut App) -> Vec<String> {
        testing::shown_text(app)
    }

    #[test]
    fn points_scale_to_the_plot_in_whole_pixels() {
        assert_eq!(
            plot_points(&[(0, 0), (20_000, 410)], 20_000, 451),
            vec![(0, 0), (220, 51)]
        );
    }

    #[test]
    fn y_max_is_the_larger_side_times_one_point_one() {
        assert_eq!(y_max(&[100, 300], &[250]), 330);
        assert_eq!(y_max(&[], &[]), 1);
    }

    #[test]
    fn a_segment_has_its_midpoint_length_and_angle() {
        let s = segments(&[(0, 0), (30, 40)]);
        assert_eq!(s.len(), 1);
        assert_eq!((s[0].x, s[0].y), (15.0, 20.0));
        assert_eq!(s[0].len, 50.0);
        assert_eq!(s[0].angle, 40f32.atan2(30.0));
        assert!(segments(&[(0, 0)]).is_empty());
    }

    #[test]
    fn a_dashed_segment_is_every_other_five_pixel_piece() {
        let seg = Segment {
            x: 50.0,
            y: 0.0,
            len: 100.0,
            angle: 0.0,
        };
        let pieces = dashed(&seg);
        assert_eq!(pieces.len(), 10);
        assert!(pieces.iter().all(|p| p.len == 5.0));
        assert_eq!(pieces[0].x, 2.5);
        assert_eq!(pieces[1].x, 12.5);
    }

    #[test]
    fn a_line_of_short_segments_stays_dashed() {
        // Twenty 2 px segments: one solid run of 40 px if each restarted its dash.
        let px: Vec<(i32, i32)> = (0..=20).map(|i| (i * 2, 0)).collect();
        let pieces = dashed_line(&px);
        let drawn: f32 = pieces.iter().map(|p| p.len).sum();
        assert!((drawn - 20.0).abs() < 0.01, "{drawn}");
        assert!(
            pieces.iter().all(|p| p.x < 5.0
                || (10.0..15.0).contains(&p.x)
                || (20.0..25.0).contains(&p.x)
                || (30.0..35.0).contains(&p.x)),
            "{pieces:?}"
        );
    }

    #[test]
    fn growing_pr_memory_reads_first_arrow_last() {
        let pr = [sample(0, 184_320, 400), sample(20_000, 419_840, 400)];
        let main = [sample(0, 194_560, 380), sample(20_000, 194_560, 380)];
        assert_eq!(
            legend_text(Metric::PeakMemory, &pr, &main, true),
            "180 → 410 MB"
        );
    }

    #[test]
    fn legend_without_growth_reads_pr_then_main() {
        let pr = [sample(0, 3_174, 410), sample(500, 3_174, 410)];
        let main = [sample(0, 3_072, 380)];
        assert_eq!(
            legend_text(Metric::AvgCpu, &pr, &main, false),
            format!(
                "{} · main {}",
                metric_value_text(Metric::AvgCpu, 410),
                metric_value_text(Metric::AvgCpu, 380)
            )
        );
        assert_eq!(
            legend_text(Metric::PeakMemory, &pr, &main, false),
            "3.1 MB · main 3.0 MB"
        );
        assert_eq!(legend_text(Metric::PeakMemory, &pr, &[], false), "3.1 MB");
        assert_eq!(legend_text(Metric::PeakMemory, &[], &[], true), "");
    }

    #[test]
    fn the_scenario_with_a_comparison_draws_both_charts_and_the_header() {
        let (mut app, pr) = app_with(demo_cockpit(NOW, DemoCockpit::Scenario));
        support::select(&mut app, &pr, scenario_item(REFRESH));
        let shown = texts(&mut app);
        for want in [
            "Memory",
            "CPU",
            "Memory and CPU during the run",
            "— this PR",
            "- - main",
            "median round of 3",
            "180 → 410 MB",
        ] {
            assert!(shown.iter().any(|t| t == want), "{want}: {shown:?}");
        }
        assert!(!segments_of(&mut app, Metric::PeakMemory, RunSide::Pr).is_empty());
        assert!(!segments_of(&mut app, Metric::AvgCpu, RunSide::Main).is_empty());
    }

    #[test]
    fn a_metric_without_points_says_not_measured() {
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        let c = view.scenarios[1].comparison.as_mut().expect("comparison");
        c.pr.series = Series::default();
        c.main.series = Series::default();
        let (mut app, pr) = app_with(view);
        support::select(&mut app, &pr, scenario_item(REFRESH));
        let shown = texts(&mut app);
        assert_eq!(shown.iter().filter(|t| *t == "not measured").count(), 2);
        assert_eq!(testing::count::<ChartSegment>(&mut app), 0);
    }

    #[test]
    fn pr_memory_is_orange_only_when_its_row_is_worse() {
        let (mut app, pr) = app_with(demo_cockpit(NOW, DemoCockpit::Scenario));
        support::select(&mut app, &pr, scenario_item(REFRESH));
        let memory = segments_of(&mut app, Metric::PeakMemory, RunSide::Pr);
        assert!(!memory.is_empty() && memory.iter().all(|s| *s == Swatch::Orange));
        let cpu = segments_of(&mut app, Metric::AvgCpu, RunSide::Pr);
        assert!(!cpu.is_empty() && cpu.iter().all(|s| *s == Swatch::Fg));
        for metric in [Metric::PeakMemory, Metric::AvgCpu] {
            let main = segments_of(&mut app, metric, RunSide::Main);
            assert!(main.iter().all(|s| *s == Swatch::Faint), "{metric:?}");
        }
        let mut same = demo_cockpit(NOW, DemoCockpit::Scenario);
        let c = same.scenarios[1].comparison.as_mut().expect("comparison");
        for row in &mut c.rows {
            row.delta.label = DeltaLabel::Same;
        }
        support::changed(&mut app, same);
        let memory = segments_of(&mut app, Metric::PeakMemory, RunSide::Pr);
        assert!(!memory.is_empty() && memory.iter().all(|s| *s == Swatch::Fg));
    }

    fn bench_app() -> (App, PrRef, ItemRef) {
        let view = demo_cockpit(NOW, DemoCockpit::BenchRunning);
        let item = ItemRef {
            kind: ItemKind::Bench,
            id: view.benches[0].slug.clone(),
        };
        let (mut app, pr) = app_with(view);
        support::select(&mut app, &pr, item.clone());
        (app, pr, item)
    }

    fn samples(pr: &PrRef, item: &ItemRef, points: Vec<Sample>) -> CockpitTell {
        CockpitTell::Samples {
            pr: pr.clone(),
            run: "demo-bench-run".into(),
            item: item.clone(),
            side: RunSide::Main,
            round: 3,
            points,
        }
    }

    #[test]
    fn a_running_item_shows_live_and_its_live_points() {
        let (mut app, pr, item) = bench_app();
        let points = vec![
            sample(0, 1_000, 100),
            sample(1_000, 3_000, 990),
            sample(2_000, 3_000, 990),
        ];
        support::tell(&mut app, samples(&pr, &item, points));
        let shown = texts(&mut app);
        assert!(shown.iter().any(|t| t == "live"), "{shown:?}");
        assert!(!shown.iter().any(|t| t.starts_with("median round of")));
        assert_eq!(
            segments_of(&mut app, Metric::PeakMemory, RunSide::Main).len(),
            // Two segments, each dashed: 5 px on, 5 px off over the plot's width.
            dashed_count(&mut app, Metric::PeakMemory)
        );
        let drawn = app
            .world_mut()
            .query::<&ChartBuilt>()
            .iter(app.world())
            .next()
            .map(|b| b.0.clone())
            .expect("drawn");
        let memory = &drawn.plots[0];
        assert_eq!(memory.main.len(), 3, "the live points: {memory:?}");
        assert_eq!(memory.main[0].0, 0);
        assert_eq!(memory.main[2].0, PLOT_W as i32);
    }

    /// The dashes main's memory line is made of, from the plot it drew.
    fn dashed_count(app: &mut App, metric: Metric) -> usize {
        let drawn = app
            .world_mut()
            .query::<&ChartBuilt>()
            .iter(app.world())
            .next()
            .map(|b| b.0.clone())
            .expect("drawn");
        let index = usize::from(metric == Metric::AvgCpu);
        dashed_line(&drawn.plots[index].main).len()
    }

    #[test]
    fn ten_thousand_samples_keep_the_model_bounded_and_rebuild_only_the_chart() {
        let (mut app, pr, item) = bench_app();
        let list = children_of::<CockpitList>(&mut app);
        let center = children_of::<CockpitCenter>(&mut app);
        let copilot = children_of::<CockpitCopilot>(&mut app);
        let compare = children_of::<CockpitCompare>(&mut app);
        let chart = children_of::<CockpitChart>(&mut app);
        for k in 0..10u32 {
            let points = (0..1_000u32)
                .map(|i| {
                    let t = k * 1_000 + i;
                    sample(t * 10, 3_000 + u64::from(t % 97), 990)
                })
                .collect();
            support::tell(&mut app, samples(&pr, &item, points));
        }
        let model = support::model(&app, &pr);
        let live = model.live.as_ref().expect("live");
        assert!(live.points[&(RunSide::Main, 3)].len() <= SERIES_MAX);
        assert_eq!(children_of::<CockpitList>(&mut app), list);
        assert_eq!(children_of::<CockpitCenter>(&mut app), center);
        assert_eq!(children_of::<CockpitCopilot>(&mut app), copilot);
        assert_eq!(children_of::<CockpitCompare>(&mut app), compare);
        assert_ne!(children_of::<CockpitChart>(&mut app), chart);
    }

    #[test]
    fn a_sample_that_moves_no_pixel_rebuilds_nothing() {
        let (mut app, pr, item) = bench_app();
        let first = vec![sample(0, 1_000, 990), sample(2_000, 3_000, 990)];
        support::tell(&mut app, samples(&pr, &item, first));
        let chart = children_of::<CockpitChart>(&mut app);
        assert!(!chart.is_empty());
        // The same time and value again: the same pixel, so the same plots.
        support::tell(
            &mut app,
            samples(&pr, &item, vec![sample(2_000, 3_000, 990)]),
        );
        assert_eq!(
            support::model(&app, &pr)
                .live
                .as_ref()
                .expect("live")
                .points[&(RunSide::Main, 3)]
                .len(),
            3
        );
        assert_eq!(children_of::<CockpitChart>(&mut app), chart);
    }

    #[test]
    fn no_cockpit_activity_spawns_no_spinner_and_no_timer() {
        let (mut app, pr) = app_with(demo_cockpit(NOW, DemoCockpit::Scenario));
        support::select(&mut app, &pr, scenario_item(REFRESH));
        let chart = children_of::<CockpitChart>(&mut app);
        assert!(!chart.is_empty());
        for _ in 0..5 {
            testing::settle(&mut app);
        }
        assert_eq!(testing::count::<Spinner>(&mut app), 0);
        assert_eq!(children_of::<CockpitChart>(&mut app), chart);
        // A running item spins only while the activity is there.
        let (mut app, _pr, _item) = bench_app();
        assert!(testing::count::<Spinner>(&mut app) >= 1);
    }

    #[test]
    fn no_item_draws_no_chart() {
        let (mut app, _pr) = app_with(demo_cockpit(NOW, DemoCockpit::Empty));
        assert!(children_of::<CockpitChart>(&mut app).is_empty());
    }
}
