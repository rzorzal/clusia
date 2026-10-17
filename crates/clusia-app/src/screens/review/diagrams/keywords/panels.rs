//! The panels laid over the 3D scene. This file holds the header: the title, the hint, the view
//! switch (3D and the three flat views, named after the plan's axes) and the link to the
//! setting that turns the view off. `spawn_overlays` is where the panels that sit on the scene
//! itself are added.

use std::collections::HashMap;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::Pickable;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui::ComputedNode;
use bevy::ui_widgets::{Activate, Button as WidgetButton, observe};

use clusia_core::PrRef;
use clusia_core::keywords::{Axis, KeywordMap, place_node};
use clusia_core::printable::printable;
use clusia_diagram::keywords::{ELSEWHERE, PointShape, Scene3D};
use clusia_protocol::DiagramCode;

use super::render::view_pose;
use super::{KeepOut, KeywordsCanvas, KeywordsRuntime, SceneImage, render, use_case_name};
use crate::bridge::{Ask, Asks, Connection, Model};
use crate::fonts::UiFonts;
use crate::review_state::{Ready, ReviewTabs, View3D};
use crate::screens::review::diagrams::Diagrams;
use crate::screens::review::diagrams::modal::{
    NodeBody, PLACE_NOT_IN_DIFF, body_block, code_rows, line_in_diff,
};
use crate::screens::review::shell::setting_link;
use crate::theme::Swatch;
use crate::ui::kit::{
    Clickable, Fill, HoverFill, Stroke, Tone, Type, Variant, badge, button, segment, segments, text,
};

pub const HINT: &str = "Drag to turn · scroll to zoom · click a point to read its code";
pub const CHIP: &str = "Keywords 3D";
/// The longest an axis name gets in the view switch.
const SWITCH_NAME_MAX: usize = 18;

/// Everything the header draws, already printable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderView {
    pub title: String,
    /// The labels of `[Free, XY, XZ, ZY]`.
    pub switch: [String; 4],
    pub view: View3D,
}

fn cut(s: &str, max: usize) -> String {
    let s = printable(s);
    if s.chars().count() <= max {
        return s;
    }
    let mut short: String = s.chars().take(max).collect();
    short.push('…');
    short
}

/// `3D`, then the three planes by axis name: `X × Y`, `X × Z`, `Z × Y`.
pub fn switch_labels(axes: &[Axis; 3]) -> [String; 4] {
    let n = |i: usize| cut(&axes[i].name, SWITCH_NAME_MAX);
    [
        "3D".to_string(),
        format!("{} × {}", n(0), n(1)),
        format!("{} × {}", n(0), n(2)),
        format!("{} × {}", n(2), n(1)),
    ]
}

pub fn header_view(map: &KeywordMap, view: View3D) -> HeaderView {
    HeaderView {
        title: cut(&map.plan.title, 80),
        switch: switch_labels(&map.plan.axes),
        view,
    }
}

pub const VIEWS: [View3D; 4] = [View3D::Free, View3D::XY, View3D::XZ, View3D::ZY];

/// The header's node; `fill_header` fills it while its view differs from what was built.
#[derive(Component, Debug)]
pub struct HeaderRegion {
    pub pr: PrRef,
    pub built: Option<HeaderView>,
}

/// One segment of the view switch.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ViewButton {
    pub pr: PrRef,
    pub view: View3D,
}

pub(super) fn fill_header(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    fonts: Res<UiFonts>,
    runtimes: Query<(&KeywordsCanvas, &KeywordsRuntime)>,
    mut regions: Query<(Entity, &mut HeaderRegion)>,
) {
    for (entity, mut region) in &mut regions {
        let Some(tab) = tabs.0.get(&region.pr) else {
            continue;
        };
        let Some((_, rt)) = runtimes.iter().find(|(c, _)| c.0 == region.pr) else {
            continue;
        };
        let view = header_view(&rt.map, tab.ui.keywords.view);
        if region.built.as_ref() == Some(&view) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        let pr = region.pr.clone();
        commands
            .entity(entity)
            .with_children(|p| draw_header(p, &fonts, &pr, &view));
        region.built = Some(view);
    }
}

/// The header's hint; it is shown only while the whole line fits beside the title and the chip.
#[derive(Component, Debug, Default)]
pub(super) struct HeaderHint {
    /// The width it takes when shown, remembered while it is hidden.
    width: f32,
}

/// Hides the hint when it does not fit whole (a word is never cut), shows it again when it does.
pub(super) fn fit_hint(
    mut hints: Query<(&mut HeaderHint, &mut Node, &ComputedNode, &ChildOf)>,
    sizes: Query<&ComputedNode>,
    children: Query<&Children>,
) {
    const GAP: f32 = 10.0;
    for (mut hint, mut node, own, parent) in &mut hints {
        if node.display != Display::None && own.size.x > 0.0 {
            hint.width = own.size.x * own.inverse_scale_factor;
        }
        let (Ok(row), Ok(siblings)) = (sizes.get(parent.parent()), children.get(parent.parent()))
        else {
            continue;
        };
        if hint.width <= 0.0 || row.size.x <= 0.0 {
            continue;
        }
        let others: f32 = siblings
            .iter()
            .filter_map(|c| sizes.get(c).ok())
            .map(|n| n.size.x * n.inverse_scale_factor)
            .sum::<f32>()
            - if node.display == Display::None {
                0.0
            } else {
                hint.width
            };
        let needed = others + hint.width + GAP * siblings.len().saturating_sub(1) as f32;
        let display = if needed <= row.size.x * row.inverse_scale_factor {
            Display::Flex
        } else {
            Display::None
        };
        if node.display != display {
            node.display = display;
        }
    }
}

fn draw_header(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, v: &HeaderView) {
    p.spawn(Node {
        flex_grow: 1.0,
        flex_shrink: 1.0,
        min_width: px(0),
        column_gap: px(10),
        align_items: AlignItems::Center,
        overflow: Overflow::clip(),
        ..default()
    })
    .with_children(|l| {
        // Each part keeps its own width, so `fit_hint` can tell when the hint has no room.
        let fixed = || Node {
            flex_shrink: 0.0,
            ..default()
        };
        l.spawn(fixed()).with_child((
            text(fonts, v.title.clone(), Type::STRONG.size(14.0)),
            TextLayout::no_wrap(),
        ));
        l.spawn(fixed())
            .with_child(badge(fonts, CHIP, Tone::Neutral));
        l.spawn((fixed(), HeaderHint::default()))
            .with_child((text(fonts, HINT, Type::META), TextLayout::no_wrap()));
    });
    p.spawn(Node {
        flex_shrink: 0.0,
        column_gap: px(12),
        align_items: AlignItems::Center,
        ..default()
    })
    .with_children(|r| {
        r.spawn(segments()).with_children(|s| {
            for (view, label) in VIEWS.into_iter().zip(&v.switch) {
                s.spawn((
                    segment(fonts, label, v.view == view),
                    ViewButton {
                        pr: pr.clone(),
                        view,
                    },
                    observe(on_view),
                ));
            }
        });
        setting_link(r, fonts, "Turn off in Config", "harness.keywords_3d");
    });
}

fn on_view(activate: On<Activate>, buttons: Query<&ViewButton>, mut tabs: ResMut<ReviewTabs>) {
    let Ok(button) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(&button.pr) {
        let ui = &mut tab.ui.keywords;
        ui.camera = view_pose(button.view, &ui.camera);
        ui.view = button.view;
    }
}

/// Adds the panels that sit on the scene (the use-case pills, the focus card, the filters and
/// the legend).
pub fn spawn_overlays(p: &mut ChildSpawnerCommands, _fonts: &UiFonts, pr: &PrRef) {
    super::overlays::spawn_regions(p, pr);
}

// ---- the tooltip

/// What the tooltip of a hovered point says, already printable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TooltipView {
    pub word: String,
    pub changed: bool,
    pub family: u16,
    /// The word of the family the match is named after, shown as `root…`.
    pub root: String,
    /// The word of the pull request the family grew from.
    pub seed: String,
    pub same_name: bool,
    pub what: String,
    pub place: String,
    pub use_case: String,
    /// The place belongs to no use case: its name is drawn in the Elsewhere grey.
    pub elsewhere: bool,
}

/// The shortest word of a family (the first of equals): the root its other words grew from.
pub fn family_root(words: &[String]) -> String {
    words
        .iter()
        .min_by_key(|w| w.chars().count())
        .cloned()
        .unwrap_or_default()
}

/// The tooltip of point `index`; `None` when the scene has no such point or its place is not in
/// the map.
pub fn tooltip_view(map: &KeywordMap, scene: &Scene3D, index: u32) -> Option<TooltipView> {
    let point = scene.points.get(index as usize)?;
    let first = map.places.get(*point.places.first()? as usize)?;
    let family = map.plan.families.get(map.family_of(first))?;
    let path = printable(map.paths.get(first.path as usize)?);
    let (what, place) = match point.shape {
        PointShape::Declared => (
            "◆ Declared here".to_string(),
            format!("{path}:{}", first.line),
        ),
        PointShape::Used => ("● Used here".to_string(), format!("{path}:{}", first.line)),
        PointShape::Bundle => (format!("◉ {} uses in this file", point.places.len()), path),
    };
    Some(TooltipView {
        word: printable(&map.words.get(usize::from(first.word))?.text),
        changed: point.changed,
        family: point.family,
        root: printable(&family_root(&family.words)),
        seed: printable(&family.seed.word),
        same_name: first.same_name(),
        what,
        place,
        use_case: use_case_name(map, point.use_case),
        elsewhere: point.use_case == ELSEWHERE,
    })
}

/// Where the tooltip's top-left corner goes for a pointer at `anchor`: to its lower right, or
/// to the other side where that would leave the canvas.
pub fn tooltip_corner(anchor: Vec2, size: Vec2, canvas: Vec2) -> Vec2 {
    let mut x = anchor.x + 14.0;
    let mut y = anchor.y + 12.0;
    if x + size.x > canvas.x - 8.0 {
        x = anchor.x - 14.0 - size.x;
    }
    if y + size.y > canvas.y - 8.0 {
        y = anchor.y - 12.0 - size.y;
    }
    Vec2::new(x.max(8.0), y.max(8.0))
}

/// The tooltip's node; `sync_tooltip` fills it while a point is hovered.
#[derive(Component, Debug)]
pub struct TooltipRegion {
    pub pr: PrRef,
    pub built: Option<TooltipView>,
}

fn row(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    label: &str,
    fill: impl FnOnce(&mut ChildSpawnerCommands),
) {
    p.spawn(Node {
        column_gap: px(6),
        align_items: AlignItems::FlexStart,
        ..default()
    })
    .with_children(|r| {
        r.spawn(Node {
            width: px(62),
            flex_shrink: 0.0,
            ..default()
        })
        .with_children(|l| {
            l.spawn(text(fonts, label, Type::META));
        });
        r.spawn(Node {
            flex_shrink: 1.0,
            min_width: px(0),
            flex_wrap: FlexWrap::Wrap,
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(fill);
    });
}

fn draw_tooltip(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &TooltipView) {
    p.spawn(Node {
        flex_wrap: FlexWrap::Wrap,
        align_items: AlignItems::Center,
        ..default()
    })
    .with_children(|h| {
        h.spawn(text(fonts, v.word.clone(), Type::MONO.size(12.0)));
        if v.changed {
            h.spawn(text(fonts, " · ", Type::META));
            h.spawn(text(
                fonts,
                "changed in this PR",
                Type::META.ink(Swatch::Green),
            ));
        }
    });
    row(p, fonts, "Matched", |r| {
        r.spawn(text(
            fonts,
            format!("{}…", v.root),
            Type::MONO
                .size(12.0)
                .ink(Swatch::Family(u8::try_from(v.family).unwrap_or(11))),
        ));
        r.spawn(text(fonts, " from the PR's ", Type::META));
        r.spawn(text(fonts, v.seed.clone(), Type::MONO.size(12.0)));
        r.spawn(text(
            fonts,
            if v.same_name {
                " · same name"
            } else {
                " · similar name"
            },
            Type::META,
        ));
    });
    row(p, fonts, "What", |r| {
        r.spawn(text(fonts, v.what.clone(), Type::META.ink(Swatch::Fg)));
    });
    row(p, fonts, "Where", |r| {
        r.spawn(text(fonts, v.place.clone(), Type::META.ink(Swatch::Fg)));
    });
    row(p, fonts, "Use case", |r| {
        let ink = if v.elsewhere {
            Swatch::Elsewhere
        } else {
            Swatch::Fg
        };
        r.spawn(text(fonts, v.use_case.clone(), Type::META.ink(ink)));
    });
}

/// Keeps the tooltip on the hovered point: its rows change only when the hovered point does,
/// and it follows the pointer (or, for a keyboard focus, the point) without a rebuild.
pub(super) fn sync_tooltip(
    mut commands: Commands,
    fonts: Res<UiFonts>,
    runtimes: Query<(&KeywordsCanvas, &KeywordsRuntime)>,
    images: Query<(&SceneImage, &ComputedNode)>,
    mut regions: Query<(
        Entity,
        &mut TooltipRegion,
        &mut Node,
        &mut Visibility,
        &ComputedNode,
    )>,
) {
    for (entity, mut region, mut node, mut visibility, own) in &mut regions {
        let Some((_, rt)) = runtimes.iter().find(|(c, _)| c.0 == region.pr) else {
            continue;
        };
        let view = rt.hover.and_then(|i| tooltip_view(&rt.map, &rt.scene, i));
        if region.built != view {
            commands.entity(entity).despawn_related::<Children>();
            if let Some(v) = &view {
                commands
                    .entity(entity)
                    .with_children(|p| draw_tooltip(p, &fonts, v));
            }
            region.built = view;
        }
        let Some(canvas) = images
            .iter()
            .find(|(i, _)| i.0 == region.pr)
            .map(|(_, n)| n.size * n.inverse_scale_factor)
        else {
            continue;
        };
        let anchor = rt.cursor.or_else(|| {
            let point = rt.scene.points.get(rt.hover? as usize)?;
            render::project(&rt.pose, rt.lens, canvas, render::world(point.pos)).map(|(at, _)| at)
        });
        let (Some(anchor), true) = (anchor, region.built.is_some()) else {
            if *visibility != Visibility::Hidden {
                *visibility = Visibility::Hidden;
            }
            continue;
        };
        let corner = tooltip_corner(anchor, own.size * own.inverse_scale_factor, canvas);
        let (left, top) = (px(corner.x), px(corner.y));
        if node.left != left || node.top != top {
            node.left = left;
            node.top = top;
        }
        if *visibility != Visibility::Inherited {
            *visibility = Visibility::Inherited;
        }
    }
}

// ---- the code card

/// How many places of a bundle its card lists.
pub const BUNDLE_ROWS: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct PlaceRow {
    /// The place's index in the map.
    pub index: u32,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CardBody {
    /// A single place: the code the daemon read, or why there is none yet.
    Code(NodeBody),
    /// A bundle: its first places, and how many more it holds.
    Places { rows: Vec<PlaceRow>, more: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardAction {
    /// A bundle has no button of its own: each row opens the dialog.
    None,
    /// **Comment on this line** (`on_line`) or **Comment**, which opens the dialog on `place`.
    Comment { place: u32, on_line: bool },
}

/// What the code card of a point says, already printable.
#[derive(Debug, Clone, PartialEq)]
pub struct CardView {
    pub word: String,
    pub use_case: String,
    pub elsewhere: bool,
    pub place: String,
    pub body: CardBody,
    pub action: CardAction,
    pub hint: Option<&'static str>,
}

/// The card of point `index`. `code` is what `ReadDiagramCode` answered for its place, `live`
/// whether the daemon can be asked.
pub fn card_view(
    map: &KeywordMap,
    scene: &Scene3D,
    index: u32,
    code: Option<&DiagramCode>,
    ready: &Ready,
    live: bool,
) -> Option<CardView> {
    let tip = tooltip_view(map, scene, index)?;
    let point = scene.points.get(index as usize)?;
    let first = *point.places.first()?;
    let place = map.places.get(first as usize)?;
    let path = printable(map.paths.get(place.path as usize)?);
    if point.shape == PointShape::Bundle {
        let rows = point
            .places
            .iter()
            .take(BUNDLE_ROWS)
            .filter_map(|&i| {
                Some(PlaceRow {
                    index: i,
                    line: map.places.get(i as usize)?.line,
                })
            })
            .collect();
        let more =
            u32::try_from(point.places.len().saturating_sub(BUNDLE_ROWS)).unwrap_or(u32::MAX);
        return Some(CardView {
            word: tip.word,
            use_case: tip.use_case,
            elsewhere: tip.elsewhere,
            place: path,
            body: CardBody::Places { rows, more },
            action: CardAction::None,
            hint: None,
        });
    }
    let body = match code {
        Some(c) => match &c.unavailable {
            Some(why) => NodeBody::Unavailable(printable(why)),
            None => {
                let mut rows = code_rows(c);
                for r in &mut rows {
                    r.changed = r.number == place.line;
                }
                NodeBody::Code(rows)
            }
        },
        None if !live => NodeBody::Offline,
        None => NodeBody::Loading,
    };
    let on_line = line_in_diff(ready, &path, place.line);
    Some(CardView {
        word: tip.word,
        use_case: tip.use_case,
        elsewhere: tip.elsewhere,
        place: format!("{path}:{}", place.line),
        body: CardBody::Code(body),
        action: CardAction::Comment {
            place: first,
            on_line,
        },
        hint: (!on_line).then_some(PLACE_NOT_IN_DIFF),
    })
}

/// The card's node; `sync_card` fills it while the tab holds a point.
#[derive(Component, Debug)]
pub struct CardRegion {
    pub pr: PrRef,
    pub built: Option<CardView>,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CardClose(pub PrRef);

/// A button that opens the dialog of the diagram `key` of version `number` on `place`.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CardOpen {
    pub pr: PrRef,
    pub number: u32,
    pub key: String,
    pub place: u32,
}

/// The use case's name as a small chip; the *Elsewhere* chip takes the Elsewhere grey.
fn use_case_chip(p: &mut ChildSpawnerCommands, fonts: &UiFonts, name: &str, elsewhere: bool) {
    if !elsewhere {
        p.spawn(badge(fonts, name, Tone::Neutral));
        return;
    }
    p.spawn((
        Node {
            height: px(18),
            padding: UiRect::horizontal(px(6)),
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(4)),
            flex_shrink: 0.0,
            ..default()
        },
        BackgroundColor::default(),
        Fill(Swatch::Chrome),
        children![text(
            fonts,
            name.to_string(),
            Type::META.size(11.0).ink(Swatch::Elsewhere)
        )],
    ));
}

fn draw_card(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    number: u32,
    key: &str,
    v: &CardView,
) {
    let open = |place: u32| CardOpen {
        pr: pr.clone(),
        number,
        key: key.to_string(),
        place,
    };
    p.spawn((
        Node {
            padding: UiRect::axes(px(12), px(10)),
            border: UiRect::bottom(px(1)),
            column_gap: px(8),
            align_items: AlignItems::Center,
            ..default()
        },
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|h| {
        h.spawn(text(fonts, v.word.clone(), Type::MONO.size(13.0)));
        use_case_chip(h, fonts, &v.use_case, v.elsewhere);
        h.spawn(Node {
            flex_grow: 1.0,
            ..default()
        });
        h.spawn((
            text(fonts, v.place.clone(), Type::META),
            TextLayout::no_wrap(),
        ));
    });
    match &v.body {
        CardBody::Code(body) => body_block(p, fonts, body, 12.0),
        CardBody::Places { rows, more } => {
            p.spawn(Node {
                flex_direction: FlexDirection::Column,
                padding: UiRect::axes(px(12), px(8)),
                ..default()
            })
            .with_children(|l| {
                for row in rows {
                    l.spawn((
                        Node {
                            padding: UiRect::axes(px(6), px(3)),
                            border_radius: BorderRadius::all(px(4)),
                            ..default()
                        },
                        (WidgetButton, Clickable),
                        Hovered::default(),
                        TabIndex(0),
                        BackgroundColor::default(),
                        Fill(Swatch::Clear),
                        HoverFill(Swatch::Hover),
                        open(row.index),
                        observe(on_open),
                        children![text(fonts, format!(":{}", row.line), Type::MONO.size(12.0))],
                    ));
                }
                if *more > 0 {
                    l.spawn(text(fonts, format!("… and {more} more"), Type::META));
                }
            });
        }
    }
    if let Some(hint) = v.hint {
        p.spawn(Node {
            padding: UiRect::axes(px(12), px(0)),
            ..default()
        })
        .with_children(|h| {
            h.spawn(text(fonts, hint, Type::META));
        });
    }
    p.spawn(Node {
        padding: UiRect::axes(px(12), px(8)),
        column_gap: px(8),
        justify_content: JustifyContent::FlexEnd,
        ..default()
    })
    .with_children(|f| {
        f.spawn((
            button(fonts, "Close", Variant::Secondary),
            CardClose(pr.clone()),
            observe(on_close),
        ));
        if let CardAction::Comment { place, on_line } = v.action {
            let label = if on_line {
                "Comment on this line"
            } else {
                "Comment"
            };
            f.spawn((
                button(fonts, label, Variant::Primary),
                open(place),
                observe(on_open),
            ));
        }
    });
}

fn on_close(activate: On<Activate>, buttons: Query<&CardClose>, mut tabs: ResMut<ReviewTabs>) {
    let Ok(CardClose(pr)) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(pr) {
        tab.ui.keywords.card = None;
    }
}

/// Opens the dialog on the place, as a click on a box of a drawing does.
fn on_open(activate: On<Activate>, buttons: Query<&CardOpen>, mut tabs: ResMut<ReviewTabs>) {
    let Ok(open) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(&open.pr) {
        tab.ui.diagram_node = Some((
            open.number,
            open.key.clone(),
            place_node(open.place as usize),
        ));
    }
}

/// Keeps the card on the point the tab holds: rebuilt only when what it says changes (the code
/// arriving, another point), hidden with no point.
pub(super) fn sync_card(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    diagrams: Res<Diagrams>,
    model: Res<Model>,
    fonts: Res<UiFonts>,
    runtimes: Query<(&KeywordsCanvas, &KeywordsRuntime)>,
    mut regions: Query<(Entity, &mut CardRegion, &mut Node)>,
) {
    for (entity, mut region, mut node) in &mut regions {
        let (Some(tab), Some((_, rt))) = (
            tabs.0.get(&region.pr),
            runtimes.iter().find(|(c, _)| c.0 == region.pr),
        ) else {
            continue;
        };
        let view = tab.ui.keywords.card.and_then(|i| {
            let first = *rt.scene.points.get(i as usize)?.places.first()?;
            let code = diagrams.0.get(&region.pr)?.code.get(&(
                rt.number,
                rt.key.clone(),
                place_node(first as usize),
            ));
            card_view(
                &rt.map,
                &rt.scene,
                i,
                code,
                tab.ready()?,
                model.connection == Connection::Live,
            )
        });
        if region.built == view {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        let display = if view.is_some() {
            Display::Flex
        } else {
            Display::None
        };
        if node.display != display {
            node.display = display;
        }
        if let Some(v) = &view {
            let pr = region.pr.clone();
            commands.entity(entity).with_children(|p| {
                draw_card(p, &fonts, &pr, rt.number, &rt.key, v);
            });
        }
        region.built = view;
    }
}

/// Asks the daemon for the code of each open card's place once per card, as soon as there is a
/// connection (the dialog asks for the same key and finds the answer).
pub(super) fn ask_card_code(
    tabs: Res<ReviewTabs>,
    diagrams: Res<Diagrams>,
    model: Res<Model>,
    runtimes: Query<(&KeywordsCanvas, &KeywordsRuntime)>,
    mut asked: Local<HashMap<PrRef, (u32, String, String)>>,
    mut asks: ResMut<Asks>,
) {
    asked.retain(|pr, _| runtimes.iter().any(|(c, _)| c.0 == *pr));
    for (canvas, rt) in &runtimes {
        let wanted = tabs
            .0
            .get(&canvas.0)
            .and_then(|tab| tab.ui.keywords.card)
            .and_then(|i| rt.scene.points.get(i as usize))
            .filter(|point| point.shape != PointShape::Bundle)
            .and_then(|point| point.places.first())
            .map(|&first| (rt.number, rt.key.clone(), place_node(first as usize)));
        let Some(id) = wanted else {
            asked.remove(&canvas.0);
            continue;
        };
        if diagrams
            .0
            .get(&canvas.0)
            .is_some_and(|m| m.code.contains_key(&id))
        {
            asked.remove(&canvas.0);
            continue;
        }
        if model.connection != Connection::Live || asked.get(&canvas.0) == Some(&id) {
            continue;
        }
        asked.insert(canvas.0.clone(), id.clone());
        let (number, key, node) = id;
        asks.send(Ask::ReadDiagramCode {
            pr: canvas.0.clone(),
            number,
            key,
            node,
        });
    }
}

/// Adds the tooltip and the card, above every other panel.
pub fn spawn_floating(p: &mut ChildSpawnerCommands, _fonts: &UiFonts, pr: &PrRef) {
    p.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(0),
            top: px(0),
            max_width: px(300),
            padding: UiRect::axes(px(10), px(8)),
            border: UiRect::all(px(1)),
            border_radius: BorderRadius::all(px(8)),
            flex_direction: FlexDirection::Column,
            row_gap: px(4),
            ..default()
        },
        Visibility::Hidden,
        Pickable::IGNORE,
        BackgroundColor::default(),
        Fill(Swatch::Surface),
        BorderColor::default(),
        Stroke(Swatch::Line),
        TooltipRegion {
            pr: pr.clone(),
            built: None,
        },
    ));
    p.spawn((
        Node {
            display: Display::None,
            position_type: PositionType::Absolute,
            right: px(16),
            bottom: px(14),
            width: px(360),
            flex_direction: FlexDirection::Column,
            border: UiRect::all(px(1)),
            border_radius: BorderRadius::all(px(10)),
            ..default()
        },
        BackgroundColor::default(),
        Fill(Swatch::Surface),
        BorderColor::default(),
        Stroke(Swatch::Line),
        KeepOut(pr.clone()),
        CardRegion {
            pr: pr.clone(),
            built: None,
        },
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::review_state::CameraPose;
    use crate::testing::{self, NOW};
    use clusia_core::keywords::Axis;

    fn axes(a: &str, b: &str, c: &str) -> [Axis; 3] {
        let axis = |name: &str| Axis {
            name: name.into(),
            low: "low".into(),
            high: "high".into(),
        };
        [axis(a), axis(b), axis(c)]
    }

    #[test]
    fn the_switch_is_named_after_the_axes() {
        assert_eq!(
            switch_labels(&axes("Layer", "Moment", "Distance")),
            [
                "3D",
                "Layer × Moment",
                "Layer × Distance",
                "Distance × Moment"
            ]
        );
    }

    #[test]
    fn a_long_axis_name_is_cut_in_the_switch_and_a_control_character_is_shown_escaped() {
        let long = "A very long axis name that would stretch the switch";
        let labels = switch_labels(&axes(long, "Mo\u{202e}ment", "Z"));
        assert!(
            labels[1].starts_with("A very long axis n… × "),
            "{labels:?}"
        );
        assert!(labels.iter().all(|l| !l.contains('\u{202e}')), "{labels:?}");
        assert!(labels.iter().all(|l| l.chars().count() <= 18 * 2 + 5));
    }

    #[test]
    fn the_header_names_the_entry_and_the_view() {
        let map = fixture::keywords_map();
        let v = header_view(&map, View3D::XZ);
        assert_eq!(v.title, "Where the words live");
        assert_eq!(v.view, View3D::XZ);
        assert_eq!(v.switch[0], "3D");
    }

    #[test]
    fn the_hint_is_hidden_whole_when_it_does_not_fit_and_back_when_it_does() {
        let mut app = testing::app(fixture::demo(NOW));
        let _pr = testing::open_keywords(&mut app);
        let hint = testing::find::<HeaderHint>(&mut app, |_| true);
        let row = app.world().get::<ChildOf>(hint).unwrap().parent();
        let siblings: Vec<Entity> = app.world().get::<Children>(row).unwrap().iter().collect();
        for (e, width) in siblings.iter().zip([100.0, 100.0, 400.0]) {
            testing::place_node(&mut app, *e, Vec2::ZERO, Vec2::new(width, 20.0));
        }
        let display = |app: &mut App| app.world().get::<Node>(hint).unwrap().display;
        // 100 + 100 + 400 and two gaps of 10 need 620.
        testing::place_node(&mut app, row, Vec2::ZERO, Vec2::new(500.0, 20.0));
        testing::settle(&mut app);
        assert_eq!(display(&mut app), Display::None, "no room: not cut, gone");
        // A node that is not displayed has no size.
        testing::place_node(&mut app, hint, Vec2::ZERO, Vec2::ZERO);
        testing::place_node(&mut app, row, Vec2::ZERO, Vec2::new(700.0, 20.0));
        testing::settle(&mut app);
        assert_eq!(display(&mut app), Display::Flex, "room again: shown whole");
    }

    #[test]
    fn the_header_shows_its_texts_and_the_setting_link() {
        let mut app = testing::app(fixture::demo(NOW));
        let _pr = testing::open_keywords(&mut app);
        for needle in [
            "Where the words live",
            "Keywords 3D",
            HINT,
            "Layer × Moment",
            "Distance × Moment",
            "Turn off in Config",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        testing::open_setting(&mut app, "harness.keywords_3d");
        assert_eq!(
            testing::config_focus(&app).1,
            Some("harness.keywords_3d"),
            "the link opens Config at the setting"
        );
    }

    #[test]
    fn a_click_on_the_switch_sets_the_view_and_its_pose_and_keeps_the_rest() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_keywords_with(&mut app, fixture::keywords_map(), |ui| {
            ui.use_case = Some(1);
            ui.card = Some(2);
            ui.hidden_families = 0b10;
        });
        let button = testing::find::<ViewButton>(&mut app, |b| b.view == View3D::XZ);
        testing::activate(&mut app, button);
        testing::settle(&mut app);
        let ui = testing::tab(&app, &pr).ui.keywords;
        assert_eq!(ui.view, View3D::XZ);
        assert_eq!(ui.camera, view_pose(View3D::XZ, &CameraPose::default()));
        assert_eq!(
            (ui.use_case, ui.card, ui.hidden_families),
            (Some(1), Some(2), 0b10)
        );
        let back = testing::find::<ViewButton>(&mut app, |b| b.view == View3D::Free);
        testing::activate(&mut app, back);
        testing::settle(&mut app);
        let ui = testing::tab(&app, &pr).ui.keywords;
        assert_eq!(ui.view, View3D::Free);
        assert_eq!(
            ui.camera.yaw,
            CameraPose::default().yaw,
            "the default angles are back"
        );
    }
}

#[cfg(test)]
mod place_tests {
    use super::*;
    use crate::bridge::{DiagramsTell, Tell};
    use crate::fixture;
    use crate::review_state::{Modal, Ready};
    use crate::testing::{self, NOW};
    use clusia_core::keywords::place_node;
    use clusia_diagram::keywords::scene;
    use clusia_protocol::CodeLine;

    fn demo_ready() -> Ready {
        let (view, news) = fixture::demo_review(NOW);
        Ready {
            view,
            news,
            cached_at: None,
        }
    }

    fn first_of(scene: &Scene3D, shape: PointShape, changed: Option<bool>) -> u32 {
        scene
            .points
            .iter()
            .position(|p| p.shape == shape && changed.is_none_or(|c| p.changed == c))
            .unwrap_or_else(|| panic!("a {shape:?} point")) as u32
    }

    fn code_at(path: &str, line: u32) -> DiagramCode {
        DiagramCode {
            path: path.into(),
            commit: "9be1d07c3a2f4e5d6c7b8a9f0e1d2c3b4a5f6e7d".into(),
            start_line: line - 1,
            lines: (0..4)
                .map(|i| CodeLine {
                    number: line - 1 + i,
                    text: format!("let line_{i} = {i};"),
                    changed: i == 0,
                })
                .collect(),
            unavailable: None,
        }
    }

    #[test]
    fn the_tooltip_of_a_changed_declaration_says_what_matched_and_where() {
        let map = fixture::keywords_map();
        let scene = scene(&map);
        let i = first_of(&scene, PointShape::Declared, Some(true));
        let tip = tooltip_view(&map, &scene, i).expect("a tooltip");
        assert!(tip.changed);
        assert_eq!(tip.what, "◆ Declared here");
        let first = &map.places[scene.points[i as usize].places[0] as usize];
        let path = &map.paths[first.path as usize];
        assert_eq!(tip.place, format!("{path}:{}", first.line));
        assert_eq!(tip.word, map.words[usize::from(first.word)].text);
        let family = &map.plan.families[map.family_of(first)];
        assert_eq!(tip.seed, family.seed.word);
        assert_eq!(tip.root, family_root(&family.words));
        assert_eq!(tip.same_name, tip.word == tip.seed);
        assert!(
            [
                "Token refresh",
                "Request signing",
                "Session expiry",
                "Login"
            ]
            .contains(&tip.use_case.as_str())
        );
    }

    #[test]
    fn the_tooltip_of_a_bundle_counts_its_uses_and_names_the_file_only() {
        let map = fixture::keywords_map();
        let scene = scene(&map);
        let mut counts: Vec<usize> = scene
            .points
            .iter()
            .filter(|p| p.shape == PointShape::Bundle)
            .map(|p| p.places.len())
            .collect();
        counts.sort_unstable();
        assert_eq!(counts, [14, 18, 37]);
        let i = scene
            .points
            .iter()
            .position(|p| p.places.len() == 37)
            .expect("the big bundle") as u32;
        let tip = tooltip_view(&map, &scene, i).expect("a tooltip");
        assert_eq!(tip.what, "◉ 37 uses in this file");
        assert_eq!(tip.place, "src/client/http.rs");
        assert_eq!(tip.word, "token");
        assert!(!tip.changed);
    }

    #[test]
    fn same_name_and_similar_name_follow_the_seed_word() {
        let map = fixture::keywords_map();
        let scene = scene(&map);
        let name_match = |word: &str| {
            let i = scene
                .points
                .iter()
                .position(|p| {
                    p.shape != PointShape::Bundle
                        && map.words[usize::from(map.places[p.places[0] as usize].word)].text
                            == word
                })
                .unwrap_or_else(|| panic!("a place of {word}")) as u32;
            tooltip_view(&map, &scene, i).expect("a tooltip").same_name
        };
        assert!(name_match("refresh"), "the seed word itself");
        assert!(!name_match("refresh_token"), "a kin word");
    }

    #[test]
    fn the_root_is_the_shortest_word_and_the_first_of_equals() {
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(family_root(&words(&["refresh_lock", "lock"])), "lock");
        assert_eq!(family_root(&words(&["ab", "cd", "efg"])), "ab");
        assert_eq!(family_root(&[]), "");
    }

    #[test]
    fn a_place_in_no_use_case_is_elsewhere_in_the_tooltip_and_the_card() {
        let mut map = fixture::keywords_map_at("src/auth/refresh.rs", 44, true);
        let inside = scene(&map);
        assert!(!tooltip_view(&map, &inside, 0).expect("a tooltip").elsewhere);
        map.regions.clear();
        let scene = scene(&map);
        let tip = tooltip_view(&map, &scene, 0).expect("a tooltip");
        assert_eq!((tip.use_case.as_str(), tip.elsewhere), ("Elsewhere", true));
        let card = card_view(&map, &scene, 0, None, &demo_ready(), true).expect("a card");
        assert_eq!(
            (card.use_case.as_str(), card.elsewhere),
            ("Elsewhere", true)
        );
    }

    #[test]
    fn no_tooltip_for_a_point_that_is_not_there() {
        let map = fixture::keywords_map();
        assert_eq!(tooltip_view(&map, &scene(&map), 10_000), None);
    }

    #[test]
    fn the_tooltip_sits_at_the_lower_right_of_the_pointer_and_flips_at_the_edges() {
        let size = Vec2::new(200.0, 100.0);
        let canvas = Vec2::new(800.0, 600.0);
        assert_eq!(
            tooltip_corner(Vec2::new(100.0, 100.0), size, canvas),
            Vec2::new(114.0, 112.0)
        );
        let flipped = tooltip_corner(Vec2::new(750.0, 560.0), size, canvas);
        assert_eq!(
            flipped,
            Vec2::new(750.0 - 14.0 - 200.0, 560.0 - 12.0 - 100.0)
        );
        let corner = tooltip_corner(Vec2::new(2.0, 2.0), Vec2::new(900.0, 700.0), canvas);
        assert!(corner.x >= 8.0 && corner.y >= 8.0);
    }

    #[test]
    fn the_card_of_a_place_highlights_its_line_and_offers_a_comment_on_it() {
        let map = fixture::keywords_map_at("src/auth/refresh.rs", 44, true);
        let scene = scene(&map);
        let ready = demo_ready();
        let code = code_at("src/auth/refresh.rs", 44);
        let card = card_view(&map, &scene, 0, Some(&code), &ready, true).expect("a card");
        assert_eq!(card.place, "src/auth/refresh.rs:44");
        assert_eq!(
            card.action,
            CardAction::Comment {
                place: 0,
                on_line: true
            }
        );
        assert_eq!(card.hint, None);
        let CardBody::Code(NodeBody::Code(rows)) = &card.body else {
            panic!("code rows, got {:?}", card.body);
        };
        let lit: Vec<u32> = rows
            .iter()
            .filter(|r| r.changed)
            .map(|r| r.number)
            .collect();
        assert_eq!(
            lit,
            [44],
            "only the place's own line, not the lines the daemon marks"
        );
        let numbers: Vec<u32> = rows.iter().map(|r| r.number).collect();
        assert_eq!(numbers, [43, 44, 45, 46], "one line before and two after");
        assert!(rows[1].changed, "the lit row is the second");
    }

    #[test]
    fn the_card_of_a_place_outside_the_change_says_the_comment_goes_to_the_body() {
        let map = fixture::keywords_map_at("src/session.rs", 9, false);
        let scene = scene(&map);
        let card = card_view(&map, &scene, 0, None, &demo_ready(), true).expect("a card");
        assert_eq!(
            card.action,
            CardAction::Comment {
                place: 0,
                on_line: false
            }
        );
        assert_eq!(
            card.hint,
            Some("Not in the diff: the comment goes to the review body.")
        );
    }

    #[test]
    fn the_card_waits_for_the_code_and_says_when_it_cannot_come() {
        let map = fixture::keywords_map_at("src/auth/refresh.rs", 44, true);
        let scene = scene(&map);
        let ready = demo_ready();
        let body = |code: Option<&DiagramCode>, live: bool| {
            card_view(&map, &scene, 0, code, &ready, live)
                .expect("a card")
                .body
        };
        assert_eq!(body(None, true), CardBody::Code(NodeBody::Loading));
        assert_eq!(body(None, false), CardBody::Code(NodeBody::Offline));
        let gone = DiagramCode {
            unavailable: Some("The code at a1b2c3d is no longer available.".into()),
            lines: Vec::new(),
            ..code_at("src/auth/refresh.rs", 44)
        };
        assert_eq!(
            body(Some(&gone), true),
            CardBody::Code(NodeBody::Unavailable(
                "The code at a1b2c3d is no longer available.".into()
            ))
        );
    }

    #[test]
    fn the_card_of_a_bundle_lists_eight_places_and_counts_the_rest() {
        let map = fixture::keywords_map();
        let scene = scene(&map);
        let i = scene
            .points
            .iter()
            .position(|p| p.places.len() == 37)
            .expect("the big bundle") as u32;
        let card = card_view(&map, &scene, i, None, &demo_ready(), true).expect("a card");
        assert_eq!(card.action, CardAction::None);
        assert_eq!(card.place, "src/client/http.rs");
        let CardBody::Places { rows, more } = &card.body else {
            panic!("places, got {:?}", card.body);
        };
        assert_eq!((rows.len(), *more), (8, 29));
        let first = scene.points[i as usize].places[0];
        assert_eq!(rows[0].index, first);
        assert_eq!(rows[0].line, map.places[first as usize].line);
    }

    fn open() -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_keywords(&mut app);
        testing::size_keywords(&mut app, &pr, Vec2::new(900.0, 600.0));
        testing::recorded(&mut app);
        (app, pr)
    }

    #[test]
    fn a_hover_shows_the_tooltip_rows_and_leaving_removes_them() {
        let (mut app, pr) = open();
        let i = testing::pickable(&mut app, &pr)
            .into_iter()
            .next()
            .expect("a point to hover");
        let at = testing::point_screen_pos(&mut app, &pr, i);
        testing::hover_at(&mut app, &pr, at);
        testing::settle(&mut app);
        for needle in ["Matched", "What", "Where", "Use case", "from the PR's"] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        testing::leave(&mut app, &pr);
        testing::settle(&mut app);
        assert!(!testing::shows(&mut app, "Matched"));
        assert_eq!(testing::keywords_runtime(&app, &pr).1, None);
    }

    #[test]
    fn opening_a_card_asks_for_the_code_once_and_then_shows_it() {
        let (mut app, pr) = open();
        let i = first_of(
            &testing::runtime(&app, &pr).scene,
            PointShape::Declared,
            Some(true),
        );
        testing::set_keywords(&mut app, &pr, |ui| ui.card = Some(i));
        let (number, key) = {
            let rt = testing::runtime(&app, &pr);
            (rt.number, rt.key.clone())
        };
        let place = testing::runtime(&app, &pr).scene.points[i as usize].places[0];
        let node = place_node(place as usize);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::ReadDiagramCode {
                pr: pr.clone(),
                number,
                key: key.clone(),
                node: node.clone()
            }]
        );
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).is_empty(), "asked once");
        assert!(testing::shows(&mut app, "Reading the code…"));
        let path = {
            let rt = testing::runtime(&app, &pr);
            rt.map.paths[rt.map.places[place as usize].path as usize].clone()
        };
        testing::tell(
            &mut app,
            Tell::Diagrams(DiagramsTell::Code {
                pr: pr.clone(),
                number,
                key,
                node,
                code: code_at(&path, 10),
            }),
        );
        testing::settle(&mut app);
        assert!(!testing::shows(&mut app, "Reading the code…"));
        assert!(testing::shows(&mut app, "Close"));
        assert!(testing::shows(&mut app, "Comment"));
    }

    #[test]
    fn close_lets_go_of_the_card_and_comment_opens_the_dialog_on_the_place() {
        let (mut app, pr) = open();
        let i = first_of(
            &testing::runtime(&app, &pr).scene,
            PointShape::Declared,
            Some(true),
        );
        testing::set_keywords(&mut app, &pr, |ui| ui.card = Some(i));
        let close = testing::find::<CardClose>(&mut app, |c| c.0 == pr);
        testing::activate(&mut app, close);
        assert_eq!(testing::keywords_ui(&app, &pr).card, None);
        testing::settle(&mut app);
        assert!(!testing::shows(&mut app, "Close"));
        testing::set_keywords(&mut app, &pr, |ui| ui.card = Some(i));
        let open = testing::find::<CardOpen>(&mut app, |_| true);
        testing::activate(&mut app, open);
        testing::settle(&mut app);
        let place = testing::runtime(&app, &pr).scene.points[i as usize].places[0];
        let tab = testing::tab(&app, &pr);
        assert_eq!(
            tab.ui.diagram_node,
            Some((2, "keywords".to_string(), place_node(place as usize)))
        );
        assert_eq!(tab.ui.modal, Some(Modal::DiagramNode));
    }

    #[test]
    fn a_bundle_card_lists_places_and_a_row_opens_the_dialog_on_it() {
        let (mut app, pr) = open();
        let i = testing::runtime(&app, &pr)
            .scene
            .points
            .iter()
            .position(|p| p.places.len() == 37)
            .expect("the big bundle") as u32;
        testing::set_keywords(&mut app, &pr, |ui| ui.card = Some(i));
        assert!(testing::shows(&mut app, "… and 29 more"));
        assert!(!testing::shows(&mut app, "Comment on this line"));
        let row = testing::find::<CardOpen>(&mut app, |_| true);
        testing::activate(&mut app, row);
        testing::settle(&mut app);
        let place = testing::runtime(&app, &pr).scene.points[i as usize].places[0];
        assert_eq!(
            testing::tab(&app, &pr).ui.diagram_node,
            Some((2, "keywords".to_string(), place_node(place as usize)))
        );
    }

    #[test]
    fn every_canvas_with_a_card_asks_for_its_own_code() {
        let (mut app, pr) = open();
        let other: PrRef = "rzorzal/clusia#124".parse().expect("valid ref");
        let i = first_of(
            &testing::runtime(&app, &pr).scene,
            PointShape::Declared,
            Some(true),
        );
        let mut tab = testing::tab(&app, &pr);
        tab.ui.keywords.card = Some(i);
        let copy = {
            let rt = testing::runtime(&app, &pr);
            KeywordsRuntime {
                number: rt.number,
                key: rt.key.clone(),
                map: rt.map.clone(),
                fingerprint: rt.fingerprint,
                scene: rt.scene.clone(),
                camera: rt.camera,
                image: rt.image.clone(),
                assets: rt.assets.clone(),
                points: rt.points.clone(),
                rings: rt.rings.clone(),
                clouds: rt.clouds.clone(),
                hover: None,
                cursor: None,
                pose: rt.pose,
                tween: None,
                lens: rt.lens,
                dragging: false,
                size: rt.size,
                resized_at: rt.resized_at,
                frames: 0,
                applied: None,
                synced: None,
            }
        };
        app.world_mut().spawn((KeywordsCanvas(other.clone()), copy));
        testing::recorded(&mut app);
        app.world_mut()
            .resource_mut::<crate::nav::Nav>()
            .reviews
            .push(other.clone());
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            tabs.0.insert(other.clone(), tab);
            tabs.0.get_mut(&pr).expect("the tab").ui.keywords.card = Some(i);
        }
        testing::settle(&mut app);
        let mut asked: Vec<PrRef> = testing::recorded(&mut app)
            .into_iter()
            .filter_map(|a| match a {
                Ask::ReadDiagramCode { pr, .. } => Some(pr),
                _ => None,
            })
            .collect();
        asked.sort_by_key(|p| p.to_string());
        assert_eq!(asked, [pr, other]);
    }
}
