//! The cockpit's frame: the screen it takes over while it is open, the header bar, the three
//! empty column shells the regions fill, and what the center shows before any item can: loading,
//! offline and the empty card. Everything the daemon sent is drawn as plain text.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::ecs::system::EntityCommands;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, ScrollArea, observe};
use clusia_core::PrRef;
use clusia_core::printable::printable;

use super::views::{EmptyCenter, MainStatus, center_view, header_view, list_view};
use super::{CockpitModel, CockpitModels};
use crate::agent_name;
use crate::bridge::{Asks, CockpitAskKind, Connection, Model};
use crate::clock::Clock;
use crate::fonts::UiFonts;
use crate::review_state::{ReviewSection, ReviewTabs};
use crate::screens::review::agent::Chats;
use crate::screens::review::editor::read_only_reason;
use crate::screens::review::shell::{CockpitHidden, ShellTop, StatusBar};
use crate::screens::review::tests_tab::body::cockpit_open;
use crate::screens::review::tests_tab::{SubTab, TestsModels};
use crate::theme::Swatch;
use crate::ui::kit::{
    Stroke, Type, Variant, button, card, disabled_button, text, wrapped_text, wrapping_button,
};
use crate::ui::spinner::spinner;

pub const COLUMN_LEFT: f32 = 262.0;
pub const COLUMN_RIGHT: f32 = 340.0;

type Spawner<'a> = ChildSpawnerCommands<'a>;

/// The root of the cockpit's screen, one per review.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CockpitBody(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CockpitHeaderBar(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CockpitList(pub PrRef);

/// The item at the top of the center card.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CockpitCenter(pub PrRef);

/// The part of the center card that scrolls: the item, its chart and its errors.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CockpitCenterScroll(pub PrRef);

/// The errors of the selected item, under its chart.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CockpitErrors(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CockpitChart(pub PrRef);

/// The bottom of the center card, pinned.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CockpitCenterFoot(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CockpitCompare(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CockpitCopilot(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CockpitBack(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CockpitDismissFailed(pub PrRef);

/// The `HeaderView` the header bar was drawn from.
#[derive(Component)]
pub struct HeaderBuilt(HeaderDrawn);

type HeaderDrawn = super::views::HeaderView;

/// What the center shows while no item is selected. A center that shows an item belongs to the
/// item's own drawer, which removes this when it draws.
#[derive(Component)]
pub struct FrameBuilt(pub CenterFrame);

#[derive(Debug, Clone, PartialEq)]
pub enum CenterKind {
    Loading,
    Offline(String),
    Empty {
        card: EmptyCenter,
        /// "<agent> is writing scenarios…" while that goes on.
        writing: Option<String>,
        /// The write button is off: a write was sent or goes on.
        write_off: bool,
    },
    Blank,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CenterFrame {
    pub kind: CenterKind,
    pub failed: Option<String>,
}

/// What the center draws itself, or `None` when an item is selected and the item's drawer has it.
pub fn center_frame(
    model: &CockpitModel,
    read_only: Option<&str>,
    agent: &str,
    now: i64,
) -> Option<CenterFrame> {
    let center = center_view(model, agent, now);
    if center.item.is_some() {
        return None;
    }
    let list = list_view(model, agent, now);
    let kind = if !model.loaded {
        match read_only {
            Some(reason) => CenterKind::Offline(printable(reason)),
            None => CenterKind::Loading,
        }
    } else if let Some(card) = list.empty_center {
        CenterKind::Empty {
            card,
            write_off: model.pending == Some(CockpitAskKind::Write)
                || list.scenarios_writing.is_some(),
            writing: list.scenarios_writing,
        }
    } else {
        CenterKind::Blank
    };
    Some(CenterFrame {
        kind,
        failed: center.failed,
    })
}

/// Main's status line and the ink it is drawn in.
pub fn main_status_text(status: &MainStatus) -> (String, Swatch) {
    let prefix = |at: &str| {
        let at = printable(at);
        if at.is_empty() {
            "main ".to_string()
        } else {
            format!("main · {at} ")
        }
    };
    match status {
        MainStatus::Ready { at } => (format!("{}● ready", prefix(at)), Swatch::Green),
        MainStatus::NeedsSetup { at } => (format!("{}● needs setup", prefix(at)), Swatch::Amber),
        MainStatus::Preparing { at } => (format!("{}● preparing…", prefix(at)), Swatch::Muted),
        MainStatus::Failed { at } => (format!("{}● failed", prefix(at)), Swatch::Orange),
        MainStatus::NotSet => ("main not set up".into(), Swatch::Faint),
        MainStatus::Unavailable { reason } => (printable(reason), Swatch::Faint),
        MainStatus::Unknown => (String::new(), Swatch::Faint),
    }
}

/// A card of the screen that scrolls its own content when it is made to scroll (`overflow`).
pub fn cockpit_card<'a>(p: &'a mut Spawner, node: Node) -> EntityCommands<'a> {
    let scrolls = node.overflow.y == OverflowAxis::Scroll;
    let mut card = p.spawn(card(Node {
        flex_direction: FlexDirection::Column,
        min_height: px(0),
        min_width: px(0),
        ..node
    }));
    if scrolls {
        card.insert((ScrollArea, ScrollPosition::default()));
    }
    card
}

fn region(grow: bool) -> Node {
    Node {
        width: percent(100),
        flex_grow: if grow { 1.0 } else { 0.0 },
        flex_shrink: if grow { 1.0 } else { 0.0 },
        min_height: px(0),
        flex_direction: FlexDirection::Column,
        ..default()
    }
}

/// The orange line of a failed turn, with the × that hides it until the text changes.
pub fn failed_line(p: &mut Spawner, fonts: &UiFonts, pr: &PrRef, text_: &str) {
    p.spawn(Node {
        width: percent(100),
        flex_shrink: 0.0,
        align_items: AlignItems::FlexStart,
        column_gap: px(8),
        padding: UiRect::axes(px(14), px(8)),
        ..default()
    })
    .with_children(|row| {
        row.spawn(Node {
            flex_grow: 1.0,
            flex_basis: px(0),
            min_width: px(0),
            ..default()
        })
        .with_child(wrapped_text(
            fonts,
            printable(text_),
            Type::BODY.ink(Swatch::Orange),
        ));
        row.spawn((
            button(fonts, "×", Variant::Ghost),
            CockpitDismissFailed(pr.clone()),
            observe(on_dismiss_failed),
        ));
    });
}

fn on_dismiss_failed(
    activate: On<Activate>,
    buttons: Query<&CockpitDismissFailed>,
    mut models: ResMut<CockpitModels>,
) {
    let Ok(CockpitDismissFailed(pr)) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(model) = models.0.get_mut(pr) {
        model.dismissed_failed = model.view.as_ref().and_then(|v| v.failed.clone());
        model.touch();
    }
}

/// The screen: the header bar over the three columns, every region empty.
pub fn draw_body(p: &mut Spawner, _fonts: &UiFonts, pr: &PrRef) {
    p.spawn((
        Node {
            flex_grow: 1.0,
            width: percent(100),
            min_height: px(0),
            flex_direction: FlexDirection::Column,
            ..default()
        },
        CockpitBody(pr.clone()),
    ))
    .with_children(|body| {
        body.spawn((
            Node {
                width: percent(100),
                height: px(44),
                flex_shrink: 0.0,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::SpaceBetween,
                padding: UiRect::horizontal(px(14)),
                border: UiRect::bottom(px(1)),
                ..default()
            },
            BorderColor::default(),
            Stroke(Swatch::Line),
            CockpitHeaderBar(pr.clone()),
        ));
        body.spawn(Node {
            flex_grow: 1.0,
            width: percent(100),
            min_height: px(0),
            column_gap: px(12),
            padding: UiRect::all(px(12)),
            ..default()
        })
        .with_children(|cols| {
            cockpit_card(
                cols,
                Node {
                    width: px(COLUMN_LEFT),
                    flex_shrink: 0.0,
                    ..default()
                },
            )
            .with_child((region(true), CockpitList(pr.clone())));
            cockpit_card(
                cols,
                Node {
                    flex_grow: 1.0,
                    flex_basis: px(0),
                    ..default()
                },
            )
            .with_children(|center| {
                // The item, its chart and its errors scroll together; the action row stays.
                center
                    .spawn((
                        Node {
                            overflow: Overflow::scroll_y(),
                            ..region(true)
                        },
                        ScrollArea,
                        ScrollPosition::default(),
                        CockpitCenterScroll(pr.clone()),
                    ))
                    .with_children(|scroll| {
                        // Grows to the card's height so the loading line and the empty card
                        // can center in it; never shrinks under its content.
                        scroll.spawn((
                            Node {
                                flex_shrink: 0.0,
                                ..region(true)
                            },
                            CockpitCenter(pr.clone()),
                        ));
                        scroll.spawn((region(false), CockpitChart(pr.clone())));
                        scroll.spawn((region(false), CockpitErrors(pr.clone())));
                    });
                center.spawn((region(false), CockpitCenterFoot(pr.clone())));
            });
            // One card: the comparison over the copilot, as the mockup draws them.
            cockpit_card(
                cols,
                Node {
                    width: px(COLUMN_RIGHT),
                    flex_shrink: 0.0,
                    ..default()
                },
            )
            .with_children(|right| {
                right.spawn((region(false), CockpitCompare(pr.clone())));
                right.spawn((region(true), CockpitCopilot(pr.clone())));
            });
        });
    });
}

pub fn on_back(
    activate: On<Activate>,
    buttons: Query<&CockpitBack>,
    mut models: ResMut<TestsModels>,
) {
    let Ok(CockpitBack(pr)) = buttons.get(activate.entity) else {
        return;
    };
    let Some(model) = models.0.get_mut(pr) else {
        return;
    };
    model.sub = match model.last_sub {
        SubTab::Functions => SubTab::Functions,
        _ => SubTab::Auto,
    };
    model.touch();
}

/// Whether the screen of `pr` is the cockpit's.
fn covers(tabs: &ReviewTabs, tests: &TestsModels, pr: &PrRef) -> bool {
    tabs.0.get(pr).is_some_and(|t| {
        t.ready().is_some() && t.ui.section == ReviewSection::Tests && cockpit_open(tests.0.get(pr))
    })
}

/// Whether the screen's parts could need another display.
pub(crate) fn screen_changed(
    tabs: Res<ReviewTabs>,
    tests: Res<TestsModels>,
    fresh: Query<
        (),
        Or<(
            Added<ShellTop>,
            Added<StatusBar>,
            Added<crate::screens::review::agent::PanelColumn>,
        )>,
    >,
) -> bool {
    tabs.is_changed() || tests.is_changed() || !fresh.is_empty()
}

fn display_of(shown: bool) -> Display {
    if shown { Display::Flex } else { Display::None }
}

/// Hides the review header and its section tabs, the right column and the status bar while the
/// cockpit is open, and brings them back when it is not. It writes only a display that differs.
#[allow(clippy::type_complexity)]
pub(crate) fn hide_for_cockpit(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    tests: Res<TestsModels>,
    chats: Res<Chats>,
    mut tops: Query<
        (&ShellTop, &mut Node),
        (
            Without<StatusBar>,
            Without<crate::screens::review::agent::PanelColumn>,
        ),
    >,
    mut bars: Query<
        (&StatusBar, &mut Node),
        (
            Without<ShellTop>,
            Without<crate::screens::review::agent::PanelColumn>,
        ),
    >,
    mut columns: Query<
        (
            Entity,
            &crate::screens::review::agent::PanelColumn,
            &mut Node,
            Has<CockpitHidden>,
        ),
        (Without<ShellTop>, Without<StatusBar>),
    >,
) {
    for (top, mut node) in &mut tops {
        let display = display_of(!covers(&tabs, &tests, &top.pr));
        if node.display != display {
            node.display = display;
        }
    }
    for (bar, mut node) in &mut bars {
        let display = display_of(!covers(&tabs, &tests, &bar.pr));
        if node.display != display {
            node.display = display;
        }
    }
    for (entity, column, mut node, hidden) in &mut columns {
        if covers(&tabs, &tests, &column.pr) {
            if !hidden {
                commands.entity(entity).insert(CockpitHidden);
            }
            if node.display != Display::None {
                node.display = Display::None;
            }
        } else if hidden {
            commands.entity(entity).remove::<CockpitHidden>();
            // The column's own layout takes over again; it shows only if the reviewer left it open.
            let open = chats.0.get(&column.pr).is_none_or(|c| c.open);
            let display = display_of(open);
            if node.display != display {
                node.display = display;
            }
        }
    }
}

/// Whether the loader has anything to look at.
pub fn load_changed(
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    tests: Res<TestsModels>,
    cockpit: Res<CockpitModels>,
) -> bool {
    tabs.is_changed() || model.is_changed() || tests.is_changed() || cockpit.is_changed()
}

/// Asks for the cockpit when it is what the Tests tab shows, the tab is live and connected, and
/// it was not asked for at this head: after a new head and after a connection came back. A model
/// whose ask was lost to a refusal while offline asks again once it is back. Written around
/// change detection and announced only when something was written.
pub fn load_cockpit(
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    tests: Res<TestsModels>,
    mut cockpit: ResMut<CockpitModels>,
    mut asks: ResMut<Asks>,
) {
    let mut wrote = false;
    let models = cockpit.bypass_change_detection();
    // A review that is only not ready right now keeps what it asked.
    if models.0.keys().any(|pr| !tabs.0.contains_key(pr)) {
        models.0.retain(|pr, _| tabs.0.contains_key(pr));
        wrote = true;
    }
    if model.connection != Connection::Live {
        for m in models.0.values_mut() {
            if m.asked && !m.loaded {
                m.asked = false;
                wrote = true;
            }
        }
    } else {
        for (pr, tab) in &tabs.0 {
            let Some(ready) = tab.ready().filter(|r| r.cached_at.is_none()) else {
                continue;
            };
            if !cockpit_open(tests.0.get(pr)) {
                continue;
            }
            let head = &ready.view.review.head_sha;
            let m = models.0.entry(pr.clone()).or_default();
            if m.asked && m.asked_head.as_deref() == Some(head.as_str()) {
                continue;
            }
            asks.send(m.ask_load(pr, head));
            wrote = true;
        }
    }
    if wrote {
        cockpit.set_changed();
    }
}

/// Whether a region of the screen has anything to compare.
pub fn regions_changed(
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    tests: Res<TestsModels>,
    cockpit: Res<CockpitModels>,
) -> bool {
    tabs.is_changed() || model.is_changed() || tests.is_changed() || cockpit.is_changed()
}

pub fn fill_header(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    clock: Res<Clock>,
    models: Res<CockpitModels>,
    fonts: Res<UiFonts>,
    bars: Query<(Entity, &CockpitHeaderBar, Option<&HeaderBuilt>)>,
) {
    let now = clock.now();
    for (entity, bar, built) in &bars {
        let pr = &bar.0;
        let Some(tab) = tabs.0.get(pr).filter(|t| t.ready().is_some()) else {
            continue;
        };
        let agent = agent_name::of_tab(&app_model.snapshot, tab);
        let view = models
            .0
            .get(pr)
            .map(|m| header_view(m, &agent, now))
            .unwrap_or_else(|| header_view(&CockpitModel::default(), &agent, now));
        if built.is_some_and(|b| b.0 == view) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        commands
            .entity(entity)
            .with_children(|p| draw_header(p, &fonts, pr, &view));
        commands.entity(entity).insert(HeaderBuilt(view));
    }
}

fn draw_header(p: &mut Spawner, fonts: &UiFonts, pr: &PrRef, view: &HeaderDrawn) {
    p.spawn(Node {
        align_items: AlignItems::Center,
        column_gap: px(10),
        ..default()
    })
    .with_children(|left| {
        left.spawn((
            button(fonts, "‹ Tests", Variant::Link),
            CockpitBack(pr.clone()),
            observe(on_back),
        ));
        left.spawn(text(fonts, "QA cockpit", Type::STRONG.size(15.0)));
        left.spawn(text(
            fonts,
            "scenarios and benchmarks for this change — this PR vs main",
            Type::META.ink(Swatch::Faint),
        ));
    });
    p.spawn(Node {
        align_items: AlignItems::Center,
        column_gap: px(16),
        min_width: px(0),
        flex_shrink: 1.0,
        overflow: Overflow::clip(),
        ..default()
    })
    .with_children(|right| {
        let (line, ink) = main_status_text(&view.main);
        if !line.is_empty() {
            right
                .spawn(Node {
                    align_items: AlignItems::Center,
                    column_gap: px(6),
                    ..default()
                })
                .with_children(|main| match line.split_once('●') {
                    Some((prefix, rest)) => {
                        main.spawn(text(fonts, prefix.trim_end().to_string(), Type::META));
                        main.spawn(text(fonts, format!("●{rest}"), Type::META.ink(ink)));
                        if matches!(view.main, MainStatus::Preparing { .. }) {
                            main.spawn(spinner());
                        }
                    }
                    None if matches!(view.main, MainStatus::NotSet) => {
                        main.spawn(text(fonts, "main", Type::META));
                        main.spawn(text(fonts, "not set up", Type::META.ink(ink)));
                    }
                    None => {
                        main.spawn(text(fonts, line.clone(), Type::META.ink(ink)));
                    }
                });
        }
        right
            .spawn(Node {
                align_items: AlignItems::Center,
                column_gap: px(6),
                ..default()
            })
            .with_children(|copilot| {
                copilot.spawn(text(fonts, "●", Type::META.ink(Swatch::Green)));
                copilot.spawn(text(
                    fonts,
                    format!("QA copilot · {}", view.agent),
                    Type::META,
                ));
            });
    });
}

/// Draws the center while no item is selected: loading, offline, or the empty card.
pub fn fill_center_frame(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    app_model: Res<Model>,
    clock: Res<Clock>,
    models: Res<CockpitModels>,
    fonts: Res<UiFonts>,
    regions: Query<(Entity, &CockpitCenter, Option<&FrameBuilt>)>,
) {
    let now = clock.now();
    for (entity, region, built) in &regions {
        let pr = &region.0;
        let Some(tab) = tabs.0.get(pr).filter(|t| t.ready().is_some()) else {
            continue;
        };
        let agent = agent_name::of_tab(&app_model.snapshot, tab);
        let default = CockpitModel::default();
        let model = models.0.get(pr).unwrap_or(&default);
        let frame = center_frame(model, read_only_reason(&tabs, &app_model, pr), &agent, now);
        let Some(frame) = frame else {
            // An item is selected: its own drawer fills the center, and takes over from here.
            if built.is_some() {
                commands.entity(entity).despawn_related::<Children>();
                commands.entity(entity).remove::<FrameBuilt>();
            }
            continue;
        };
        if built.is_some_and(|b| b.0 == frame) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        commands
            .entity(entity)
            .with_children(|p| draw_center_frame(p, &fonts, pr, &frame));
        commands.entity(entity).insert(FrameBuilt(frame));
    }
}

fn centered(mut layout: Mut<TextLayout>) {
    layout.justify = Justify::Center;
}

fn draw_center_frame(p: &mut Spawner, fonts: &UiFonts, pr: &PrRef, frame: &CenterFrame) {
    if let Some(failed) = &frame.failed {
        failed_line(p, fonts, pr, failed);
    }
    p.spawn(Node {
        margin: UiRect::vertical(Val::Auto),
        width: percent(100),
        flex_shrink: 0.0,
        padding: UiRect::all(px(14)),
        flex_direction: FlexDirection::Column,
        align_items: AlignItems::Center,
        ..default()
    })
    .with_children(|c| match &frame.kind {
        CenterKind::Loading => {
            c.spawn(text(fonts, "Loading the QA cockpit…", Type::MUTED));
        }
        CenterKind::Offline(reason) => {
            c.spawn(wrapped_text(fonts, reason.clone(), Type::MUTED));
        }
        CenterKind::Blank => {}
        CenterKind::Empty {
            card: empty,
            writing,
            write_off,
        } => {
            // A definite width, so the texts measure at the width they wrap to.
            c.spawn(Node {
                width: percent(100),
                max_width: px(460),
                flex_shrink: 0.0,
                // The extra room under the last line keeps a line that wraps at the largest text
                // inside the card, whose height is taken before the wrap.
                padding: UiRect::new(px(0), px(0), px(28), px(44)),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Stretch,
                row_gap: px(12),
                ..default()
            })
            .with_children(|card| {
                card.spawn((
                    text(fonts, empty.title.clone(), Type::HEADING.size(18.0)),
                    TextLayout {
                        justify: Justify::Center,
                        ..default()
                    },
                ));
                card.spawn(wrapped_text(fonts, empty.text.clone(), Type::MUTED))
                    .entry::<TextLayout>()
                    .and_modify(centered);
                card.spawn(Node {
                    width: percent(100),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::Center,
                    column_gap: px(10),
                    flex_wrap: FlexWrap::Wrap,
                    ..default()
                })
                .with_children(|row| {
                    if *write_off {
                        row.spawn(disabled_button(fonts, &empty.button));
                    } else {
                        row.spawn((
                            wrapping_button(fonts, &empty.button, Variant::Primary),
                            super::list::CockpitWriteScenarios(pr.clone()),
                            observe(super::list::on_write_scenarios),
                        ));
                    }
                });
                if let Some(line) = writing {
                    card.spawn(Node {
                        align_items: AlignItems::Center,
                        column_gap: px(8),
                        ..default()
                    })
                    .with_children(|row| {
                        row.spawn(spinner());
                        row.spawn(text(fonts, line.clone(), Type::MUTED));
                    });
                }
                card.spawn(wrapped_text(fonts, empty.foot.clone(), Type::META))
                    .entry::<TextLayout>()
                    .and_modify(centered);
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{DemoCockpit, demo_cockpit};
    use crate::testing::NOW;
    use clusia_core::cockpit::CockpitView;

    fn model_of(view: CockpitView) -> CockpitModel {
        let mut m = CockpitModel::default();
        m.apply(&crate::bridge::CockpitTell::Loaded {
            pr: crate::fixture::demo_pr(),
            view: Box::new(view),
        });
        m
    }

    #[test]
    fn main_status_text_per_phase() {
        assert_eq!(
            main_status_text(&MainStatus::Ready {
                at: "4e1f2a0".into()
            })
            .0,
            "main · 4e1f2a0 ● ready"
        );
        assert_eq!(
            main_status_text(&MainStatus::NeedsSetup {
                at: "4e1f2a0".into()
            })
            .0,
            "main · 4e1f2a0 ● needs setup"
        );
        assert_eq!(
            main_status_text(&MainStatus::Preparing {
                at: "4e1f2a0".into()
            })
            .0,
            "main · 4e1f2a0 ● preparing…"
        );
        assert_eq!(
            main_status_text(&MainStatus::Failed {
                at: "4e1f2a0".into()
            })
            .0,
            "main · 4e1f2a0 ● failed"
        );
        assert_eq!(
            main_status_text(&MainStatus::Ready { at: String::new() }).0,
            "main ● ready"
        );
        assert_eq!(main_status_text(&MainStatus::NotSet).0, "main not set up");
        assert_eq!(
            main_status_text(&MainStatus::Unavailable {
                reason: "x\u{202e}y".into()
            })
            .0,
            "x\\u{202e}y",
        );
        assert_eq!(main_status_text(&MainStatus::Unknown).0, "");
    }

    #[test]
    fn main_status_inks() {
        let ink = |s| main_status_text(&s).1;
        let at = || "4e1f2a0".to_string();
        assert_eq!(ink(MainStatus::Ready { at: at() }), Swatch::Green);
        assert_eq!(ink(MainStatus::NeedsSetup { at: at() }), Swatch::Amber);
        assert_eq!(ink(MainStatus::Preparing { at: at() }), Swatch::Muted);
        assert_eq!(ink(MainStatus::Failed { at: at() }), Swatch::Orange);
        assert_eq!(ink(MainStatus::NotSet), Swatch::Faint);
    }

    #[test]
    fn the_center_frame_is_loading_until_loaded_and_offline_without_a_connection() {
        let m = CockpitModel::default();
        assert_eq!(
            center_frame(&m, None, "Claude", NOW).map(|f| f.kind),
            Some(CenterKind::Loading)
        );
        assert_eq!(
            center_frame(&m, Some("Not connected to clusiad"), "Claude", NOW).map(|f| f.kind),
            Some(CenterKind::Offline("Not connected to clusiad".into()))
        );
    }

    #[test]
    fn the_center_frame_is_the_empty_card_for_an_empty_cockpit_and_nothing_with_an_item() {
        let empty = model_of(demo_cockpit(NOW, DemoCockpit::Empty));
        let frame = center_frame(&empty, None, "Claude", NOW).expect("a frame");
        assert!(matches!(
            frame.kind,
            CenterKind::Empty {
                write_off: false,
                ..
            }
        ));
        let scenario = model_of(demo_cockpit(NOW, DemoCockpit::Scenario));
        assert_eq!(center_frame(&scenario, None, "Claude", NOW), None);
    }

    use crate::bridge::{Ask, CockpitTell, Tell};
    use crate::screens::review::agent::PanelColumn;
    use crate::screens::review::tests_tab::body::OpenCockpit;
    use crate::testing;
    use crate::ui::spinner::Spinner;
    use bevy::input::ButtonInput;
    use bevy::input::keyboard::KeyCode;

    fn entry_app() -> (App, PrRef) {
        let mut app = testing::app(crate::fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        testing::open_tests_with_recipe(&mut app, &pr);
        testing::recorded(&mut app);
        (app, pr)
    }

    fn click_open(app: &mut App, pr: &PrRef) {
        let button = testing::find::<OpenCockpit>(app, |b| b.0 == *pr);
        testing::activate(app, button);
        testing::settle(app);
    }

    fn tell_cockpit(app: &mut App, pr: &PrRef, state: DemoCockpit) {
        testing::tell(
            app,
            Tell::Cockpit(CockpitTell::Loaded {
                pr: pr.clone(),
                view: Box::new(demo_cockpit(NOW, state)),
            }),
        );
        testing::settle(app);
    }

    fn opened(state: DemoCockpit) -> (App, PrRef) {
        let (mut app, pr) = entry_app();
        click_open(&mut app, &pr);
        tell_cockpit(&mut app, &pr, state);
        testing::recorded(&mut app);
        (app, pr)
    }

    fn sub(app: &App, pr: &PrRef) -> SubTab {
        app.world().resource::<TestsModels>().0[pr].sub
    }

    fn display<C: Component>(app: &mut App) -> Display {
        let e = testing::find::<C>(app, |_| true);
        app.world().get::<Node>(e).expect("a node").display
    }

    fn children_of<C: Component>(app: &mut App) -> Vec<Entity> {
        let e = testing::find::<C>(app, |_| true);
        app.world()
            .get::<Children>(e)
            .map(|c| c.iter().collect())
            .unwrap_or_default()
    }

    #[test]
    fn the_button_is_on_both_sub_tabs_and_not_in_the_empty_state() {
        let (mut app, pr) = entry_app();
        assert_eq!(testing::count::<OpenCockpit>(&mut app), 1);
        app.world_mut()
            .resource_mut::<TestsModels>()
            .0
            .get_mut(&pr)
            .expect("a model")
            .sub = SubTab::Functions;
        testing::settle(&mut app);
        assert_eq!(testing::count::<OpenCockpit>(&mut app), 1);
        let view = testing::tests_view(&app, &pr, clusia_core::tests::TestsPhase::NotGenerated);
        testing::open_tests(&mut app, &pr, view);
        assert_eq!(testing::count::<OpenCockpit>(&mut app), 0);
    }

    #[test]
    fn clicking_the_button_opens_the_cockpit_and_asks_once() {
        let (mut app, pr) = entry_app();
        click_open(&mut app, &pr);
        assert_eq!(sub(&app, &pr), SubTab::Cockpit);
        assert_eq!(
            app.world().resource::<TestsModels>().0[&pr].last_sub,
            SubTab::Auto
        );
        assert_eq!(testing::recorded(&mut app), [Ask::GetCockpit(pr.clone())]);
        assert_eq!(testing::count::<CockpitBody>(&mut app), 1);
        let back = testing::find::<CockpitBack>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, back);
        testing::settle(&mut app);
        assert_eq!(sub(&app, &pr), SubTab::Auto);
        click_open(&mut app, &pr);
        assert_eq!(testing::recorded(&mut app), [], "asked already");
    }

    #[test]
    fn back_returns_to_the_sub_tab_the_button_was_clicked_on() {
        let (mut app, pr) = entry_app();
        app.world_mut()
            .resource_mut::<TestsModels>()
            .0
            .get_mut(&pr)
            .expect("a model")
            .sub = SubTab::Functions;
        testing::settle(&mut app);
        click_open(&mut app, &pr);
        let back = testing::find::<CockpitBack>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, back);
        testing::settle(&mut app);
        assert_eq!(sub(&app, &pr), SubTab::Functions);
    }

    #[test]
    fn offline_the_click_only_switches_and_asks_nothing() {
        let (mut app, pr) = entry_app();
        testing::tell(&mut app, Tell::Lost("gone".into()));
        testing::settle(&mut app);
        click_open(&mut app, &pr);
        assert_eq!(sub(&app, &pr), SubTab::Cockpit);
        assert_eq!(testing::recorded(&mut app), []);
        assert!(testing::shows(&mut app, "Not connected to clusiad"));
        assert!(!testing::shows(&mut app, "Loading the QA cockpit"));
    }

    #[test]
    fn the_cockpit_hides_the_top_the_right_panel_and_the_status_bar_and_back_shows_them() {
        let (mut app, pr) = entry_app();
        assert_eq!(display::<ShellTop>(&mut app), Display::Flex);
        click_open(&mut app, &pr);
        assert_eq!(display::<ShellTop>(&mut app), Display::None);
        assert_eq!(display::<StatusBar>(&mut app), Display::None);
        assert_eq!(display::<PanelColumn>(&mut app), Display::None);
        let back = testing::find::<CockpitBack>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, back);
        testing::settle(&mut app);
        assert_eq!(display::<ShellTop>(&mut app), Display::Flex);
        assert_eq!(display::<StatusBar>(&mut app), Display::Flex);
        assert_eq!(display::<PanelColumn>(&mut app), Display::Flex);
        assert_eq!(testing::count::<CockpitBody>(&mut app), 0);
    }

    #[test]
    fn the_screen_comes_back_when_the_section_changes_or_the_recipe_goes() {
        let (mut app, pr) = opened(DemoCockpit::Scenario);
        testing::set_section(&mut app, &pr, ReviewSection::Diff);
        assert_eq!(display::<ShellTop>(&mut app), Display::Flex);
        assert_eq!(display::<PanelColumn>(&mut app), Display::Flex);
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        assert_eq!(display::<ShellTop>(&mut app), Display::None);
        let view = testing::tests_view(&app, &pr, clusia_core::tests::TestsPhase::NotGenerated);
        testing::open_tests(&mut app, &pr, view);
        assert_eq!(sub(&app, &pr), SubTab::Cockpit, "sub is left alone");
        assert_eq!(display::<ShellTop>(&mut app), Display::Flex);
        assert_eq!(display::<StatusBar>(&mut app), Display::Flex);
        assert_eq!(testing::count::<CockpitBody>(&mut app), 0);
    }

    #[test]
    fn a_column_the_reviewer_closed_stays_closed_after_the_cockpit() {
        let (mut app, pr) = entry_app();
        app.world_mut()
            .resource_mut::<crate::screens::review::agent::Chats>()
            .entry(&pr)
            .toggle();
        testing::settle(&mut app);
        testing::run_tweens(&mut app);
        assert_eq!(display::<PanelColumn>(&mut app), Display::None);
        click_open(&mut app, &pr);
        let back = testing::find::<CockpitBack>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, back);
        testing::settle(&mut app);
        testing::settle(&mut app);
        assert_eq!(display::<PanelColumn>(&mut app), Display::None);
    }

    #[test]
    fn escape_without_a_modal_keeps_the_cockpit_open() {
        let (mut app, pr) = opened(DemoCockpit::Scenario);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);
        testing::settle(&mut app);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .release(KeyCode::Escape);
        assert_eq!(sub(&app, &pr), SubTab::Cockpit);
        assert_eq!(testing::count::<CockpitBody>(&mut app), 1);
    }

    #[test]
    fn a_reconnect_asks_again_once() {
        let (mut app, pr) = opened(DemoCockpit::Scenario);
        testing::tell(&mut app, Tell::Lost("gone".into()));
        testing::settle(&mut app);
        assert!(
            !testing::recorded(&mut app).contains(&Ask::GetCockpit(pr.clone())),
            "nothing while offline"
        );
        app.world_mut().resource_mut::<Model>().connection = Connection::Live;
        testing::settle(&mut app);
        testing::settle(&mut app);
        let asked = |app: &mut App| {
            testing::recorded(app)
                .into_iter()
                .filter(|a| *a == Ask::GetCockpit(pr.clone()))
                .count()
        };
        assert_eq!(asked(&mut app), 1);
        testing::settle(&mut app);
        assert_eq!(asked(&mut app), 0, "never every frame");
    }

    #[test]
    fn a_failed_turn_shows_an_orange_line_until_dismissed() {
        let (mut app, pr) = entry_app();
        click_open(&mut app, &pr);
        let with = |text: &str| {
            let mut v = demo_cockpit(NOW, DemoCockpit::Empty);
            v.failed = Some(text.into());
            v
        };
        let tell_changed = |app: &mut App, view: CockpitView| {
            testing::tell(
                app,
                Tell::Cockpit(CockpitTell::Changed {
                    pr: pr.clone(),
                    view: Box::new(view),
                }),
            );
            testing::settle(app);
        };
        tell_cockpit(&mut app, &pr, DemoCockpit::Empty);
        tell_changed(&mut app, with("x"));
        assert!(testing::shows(&mut app, "x"));
        let dismiss = testing::find::<CockpitDismissFailed>(&mut app, |d| d.0 == pr);
        testing::activate(&mut app, dismiss);
        testing::settle(&mut app);
        assert_eq!(testing::count::<CockpitDismissFailed>(&mut app), 0);
        tell_changed(&mut app, with("y"));
        assert_eq!(testing::count::<CockpitDismissFailed>(&mut app), 1);
    }

    #[test]
    fn switching_section_and_back_keeps_the_cockpit_open() {
        let (mut app, pr) = opened(DemoCockpit::Scenario);
        testing::set_section(&mut app, &pr, ReviewSection::Diff);
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        assert_eq!(sub(&app, &pr), SubTab::Cockpit);
        assert_eq!(testing::count::<CockpitBody>(&mut app), 1);
    }

    #[test]
    fn the_three_columns_exist_with_their_widths() {
        let (mut app, _pr) = opened(DemoCockpit::Scenario);
        let width_of_parent = |app: &mut App, e: Entity| {
            let parent = app.world().get::<ChildOf>(e).expect("a parent").parent();
            app.world().get::<Node>(parent).expect("a node").width
        };
        let list = testing::find::<CockpitList>(&mut app, |_| true);
        assert_eq!(width_of_parent(&mut app, list), px(COLUMN_LEFT));
        let compare = testing::find::<CockpitCompare>(&mut app, |_| true);
        assert_eq!(width_of_parent(&mut app, compare), px(COLUMN_RIGHT));
        for c in [
            testing::count::<CockpitCenter>(&mut app),
            testing::count::<CockpitChart>(&mut app),
            testing::count::<CockpitCenterFoot>(&mut app),
            testing::count::<CockpitCopilot>(&mut app),
        ] {
            assert_eq!(c, 1);
        }
    }

    #[test]
    fn loading_text_until_loaded_and_the_view_after() {
        let (mut app, pr) = entry_app();
        click_open(&mut app, &pr);
        assert!(testing::shows(&mut app, "Loading the QA cockpit…"));
        tell_cockpit(&mut app, &pr, DemoCockpit::Scenario);
        assert!(!testing::shows(&mut app, "Loading the QA cockpit…"));
        assert!(testing::shows(&mut app, "SCENARIOS · "));
        assert!(testing::shows(&mut app, "QA cockpit"));
        assert!(testing::shows(&mut app, "QA copilot · "));
    }

    #[test]
    fn no_cockpit_ask_goes_out_without_a_click() {
        let (mut app, pr) = entry_app();
        click_open(&mut app, &pr);
        let sent = testing::recorded(&mut app);
        assert_eq!(sent, [Ask::GetCockpit(pr.clone())]);
        tell_cockpit(&mut app, &pr, DemoCockpit::Empty);
        tell_cockpit(&mut app, &pr, DemoCockpit::BenchRunning);
        testing::tell(
            &mut app,
            Tell::Cockpit(CockpitTell::Changed {
                pr: pr.clone(),
                view: Box::new(demo_cockpit(NOW, DemoCockpit::Main)),
            }),
        );
        testing::settle(&mut app);
        // Reading the selected scenario's script is no run: it goes out without a click.
        let sent: Vec<Ask> = testing::recorded(&mut app)
            .into_iter()
            .filter(|a| !matches!(a, Ask::GetScenarioScript { .. }))
            .collect();
        assert_eq!(sent, []);
    }

    #[test]
    fn an_idle_cockpit_rebuilds_nothing_and_spawns_no_spinner() {
        let (mut app, _pr) = opened(DemoCockpit::Scenario);
        let list = children_of::<CockpitList>(&mut app);
        let header = children_of::<CockpitHeaderBar>(&mut app);
        for _ in 0..5 {
            testing::settle(&mut app);
        }
        assert_eq!(children_of::<CockpitList>(&mut app), list);
        assert_eq!(children_of::<CockpitHeaderBar>(&mut app), header);
        assert_eq!(testing::count::<Spinner>(&mut app), 0);
    }

    #[test]
    fn a_change_that_leaves_the_list_view_equal_does_not_rebuild_the_list() {
        let (mut app, pr) = opened(DemoCockpit::Scenario);
        let list = children_of::<CockpitList>(&mut app);
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        view.copilot.busy = true;
        testing::tell(
            &mut app,
            Tell::Cockpit(CockpitTell::Changed {
                pr: pr.clone(),
                view: Box::new(view),
            }),
        );
        testing::settle(&mut app);
        assert_eq!(children_of::<CockpitList>(&mut app), list);
    }

    #[test]
    fn a_hostile_main_reason_is_bounded_in_the_view_and_the_header_text() {
        let reason = format!("\u{202e}{}", "x".repeat(10_000));
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        view.main.phase = clusia_core::cockpit::MainPhase::Unavailable { reason };
        let model = model_of(view);
        let header = header_view(&model, "Claude", NOW);
        let (line, _) = main_status_text(&header.main);
        assert!(
            line.chars().count() <= super::super::PURPOSE_SHOWN + 1,
            "{}",
            line.len()
        );
        assert!(!line.contains('\u{202e}'));
        let list = list_view(&model, "Claude", NOW);
        let note = list.on_main_note.expect("a note");
        assert!(note.chars().count() <= super::super::PURPOSE_SHOWN + 1);
        assert!(!note.contains('\u{202e}'));
    }

    #[test]
    fn the_right_column_is_one_card() {
        let (mut app, _pr) = opened(DemoCockpit::Scenario);
        let compare = testing::find::<CockpitCompare>(&mut app, |_| true);
        let copilot = testing::find::<CockpitCopilot>(&mut app, |_| true);
        let parent =
            |app: &App, e: Entity| app.world().get::<ChildOf>(e).expect("a parent").parent();
        assert_eq!(parent(&app, compare), parent(&app, copilot));
    }

    #[test]
    fn preparing_main_draws_a_spinner_in_the_header_only_then() {
        let (mut app, pr) = opened(DemoCockpit::Scenario);
        assert_eq!(testing::count::<Spinner>(&mut app), 0);
        let mut view = demo_cockpit(NOW, DemoCockpit::Scenario);
        view.main.phase = clusia_core::cockpit::MainPhase::Preparing {
            step: "build".into(),
        };
        testing::tell(
            &mut app,
            Tell::Cockpit(CockpitTell::Changed {
                pr: pr.clone(),
                view: Box::new(view),
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "● preparing…"));
        assert_eq!(testing::count::<Spinner>(&mut app), 1);
    }

    #[test]
    fn the_write_button_is_off_while_a_write_is_pending() {
        let mut m = model_of(demo_cockpit(NOW, DemoCockpit::Empty));
        m.sent(CockpitAskKind::Write);
        let frame = center_frame(&m, None, "Claude", NOW).expect("a frame");
        assert!(matches!(
            frame.kind,
            CenterKind::Empty {
                write_off: true,
                ..
            }
        ));
    }
}
