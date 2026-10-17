//! The panels that sit on the 3D scene: the use-case pills and the focus card of the one that is
//! picked, the filters and the legend. Each is a pure view of the map and the tab's state, compared
//! with what was built so a region is redrawn only when what it says changes. The actions below
//! write the tab's `KeywordsUi`; the camera follows by itself (`KeywordsUi.camera` is only the
//! pose it is asked to be at).

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::Pickable;
use bevy::picking::events::{Click, Drag, DragEnd, DragStart, Pointer, Press, Scroll};
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::Strikethrough;
use bevy::ui_widgets::{Activate, Button as WidgetButton, observe};

use clusia_core::PrRef;
use clusia_core::diagram::History;
use clusia_core::keywords::{KeywordMap, SEARCH_BUDGET_SECS};
use clusia_diagram::keywords::Scene3D;

use super::render::{focus_pose, view_pose, visible};
use super::{CloudLabel, KeepOut, KeywordsCanvas, KeywordsRuntime, keys};
use crate::fonts::UiFonts;
use crate::review_state::{CameraPose, KeywordsUi, ReviewTabs};
use crate::screens::review::ReviewSystems;
use crate::screens::review::diagrams::Diagrams;
use crate::theme::Swatch;
use crate::ui::kit::{Clickable, Fill, HoverFill, Stroke, Type, checkbox_box, text};

/// The most word rows the focus card lists before it says how many more there are.
pub const MAX_FOCUS_WORDS: usize = 12;

// ---- the views

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PillView {
    pub use_case: u16,
    pub name: String,
    pub places: u32,
    pub selected: bool,
    pub mark: Option<History>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PillsView {
    pub all_selected: bool,
    pub pills: Vec<PillView>,
}

/// What a comparison with the older version says about an entry; `Same` says nothing.
fn mark_of(history: Option<History>, compare: bool) -> Option<History> {
    history.filter(|h| compare && *h != History::Same)
}

fn index(i: usize) -> u16 {
    u16::try_from(i).unwrap_or(u16::MAX)
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The places of each plan use case, by index. Places of no use case (Elsewhere) are in none.
fn places_by_use_case(map: &KeywordMap) -> Vec<u32> {
    let mut counts = vec![0u32; map.plan.use_cases.len()];
    for place in &map.places {
        if let Some(n) = map.use_case_of(place).and_then(|u| counts.get_mut(u)) {
            *n += 1;
        }
    }
    counts
}

pub fn pills_view(map: &KeywordMap, ui: &KeywordsUi, compare: bool) -> PillsView {
    let counts = places_by_use_case(map);
    PillsView {
        all_selected: ui.use_case.is_none(),
        pills: map
            .plan
            .use_cases
            .iter()
            .enumerate()
            .map(|(i, u)| PillView {
                use_case: index(i),
                name: u.name.clone(),
                places: counts.get(i).copied().unwrap_or(0),
                selected: ui.use_case == Some(index(i)),
                mark: mark_of(u.history, compare),
            })
            .collect(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordRow {
    pub text: String,
    pub family: u16,
    pub places: u32,
    pub changed: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusView {
    pub name: String,
    pub note: Option<String>,
    pub mark: Option<History>,
    pub places: u32,
    pub changed: u32,
    pub words: Vec<WordRow>,
    pub more_words: u32,
    /// The other use cases that hold a changed place of a family this one uses.
    pub also_reach: Vec<String>,
}

pub fn focus_view(map: &KeywordMap, use_case: u16, compare: bool) -> Option<FocusView> {
    let at = usize::from(use_case);
    let case = map.plan.use_cases.get(at)?;
    let mut by_word: BTreeMap<u16, (u32, u32)> = BTreeMap::new();
    let mut families = BTreeSet::new();
    let (mut places, mut changed) = (0u32, 0u32);
    for place in &map.places {
        if map.use_case_of(place) != Some(at) {
            continue;
        }
        places += 1;
        let entry = by_word.entry(place.word).or_default();
        entry.0 += 1;
        if place.changed() {
            changed += 1;
            entry.1 += 1;
        }
        families.insert(map.family_of(place));
    }
    let mut words: Vec<WordRow> = by_word
        .into_iter()
        .filter_map(|(word, (places, changed))| {
            let w = map.words.get(usize::from(word))?;
            Some(WordRow {
                text: w.text.clone(),
                family: w.family,
                places,
                changed,
            })
        })
        .collect();
    words.sort_by(|a, b| (Reverse(a.places), &a.text).cmp(&(Reverse(b.places), &b.text)));
    let more_words = count(words.len().saturating_sub(MAX_FOCUS_WORDS));
    words.truncate(MAX_FOCUS_WORDS);
    let reaching: BTreeSet<usize> = map
        .places
        .iter()
        .filter(|p| p.changed() && families.contains(&map.family_of(p)))
        .filter_map(|p| map.use_case_of(p))
        .filter(|u| *u != at)
        .collect();
    let also_reach = map
        .plan
        .use_cases
        .iter()
        .enumerate()
        .filter(|(i, _)| reaching.contains(i))
        .map(|(_, u)| u.name.clone())
        .collect();
    Some(FocusView {
        name: case.name.clone(),
        note: case.note.clone(),
        mark: mark_of(case.history, compare),
        places,
        changed,
        words,
        more_words,
        also_reach,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterRow {
    pub index: u16,
    pub label: String,
    pub places: u32,
    pub on: bool,
    pub mark: Option<History>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiltersView {
    pub families: Vec<FilterRow>,
    pub use_cases: Vec<FilterRow>,
    pub only_changed: bool,
}

fn bit(mask: u16, index: u16) -> bool {
    index < 16 && mask & (1 << index) != 0
}

pub fn filters_view(map: &KeywordMap, ui: &KeywordsUi, compare: bool) -> FiltersView {
    let mut in_family = vec![0u32; map.plan.families.len()];
    for place in &map.places {
        if let Some(n) = in_family.get_mut(map.family_of(place)) {
            *n += 1;
        }
    }
    let in_case = places_by_use_case(map);
    FiltersView {
        families: map
            .plan
            .families
            .iter()
            .enumerate()
            .map(|(i, f)| FilterRow {
                index: index(i),
                label: f.label.clone(),
                places: in_family.get(i).copied().unwrap_or(0),
                on: !bit(ui.hidden_families, index(i)),
                mark: mark_of(f.history, compare),
            })
            .collect(),
        use_cases: map
            .plan
            .use_cases
            .iter()
            .enumerate()
            .map(|(i, u)| FilterRow {
                index: index(i),
                label: u.name.clone(),
                places: in_case.get(i).copied().unwrap_or(0),
                on: !bit(ui.hidden_use_cases, index(i)),
                mark: mark_of(u.history, compare),
            })
            .collect(),
        only_changed: ui.only_changed,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegendView {
    pub more: Option<String>,
    pub cut: Option<String>,
}

pub fn legend_view(more: u32, search_cut: bool) -> LegendView {
    LegendView {
        more: match more {
            0 => None,
            1 => Some("+1 more place".to_string()),
            n => Some(format!("+{n} more places")),
        },
        cut: search_cut.then(|| {
            format!("Search stopped after {SEARCH_BUDGET_SECS} s; some places are missing.")
        }),
    }
}

fn places_text(n: u32) -> String {
    if n == 1 {
        "1 place".to_string()
    } else {
        format!("{n} places")
    }
}

// ---- the actions

/// Focuses `use_case` (or lets go of the focus with `None`): the commanded camera goes to the
/// cloud, or back to the view's own pose. The displayed camera follows on its own.
pub fn set_use_case(ui: &mut KeywordsUi, scene: &Scene3D, use_case: Option<u16>) {
    ui.use_case = use_case;
    ui.card = None;
    let home = view_pose(ui.view, &CameraPose::default());
    ui.camera = match use_case {
        Some(u) if scene.points.iter().any(|p| p.use_case == u) => focus_pose(scene, u),
        _ => home,
    };
    if let Some(u) = use_case
        && u < 16
    {
        ui.hidden_use_cases &= !(1 << u);
    }
}

/// A pill, *All use cases* (`None`) or a cloud's name: the focused one and *All* let go.
pub fn toggle_pill(ui: &mut KeywordsUi, scene: &Scene3D, target: Option<u16>) {
    match target {
        Some(t) if ui.use_case != Some(t) => set_use_case(ui, scene, Some(t)),
        _ => set_use_case(ui, scene, None),
    }
}

/// Closes the card of a point the filters no longer show.
fn close_hidden_card(ui: &mut KeywordsUi, scene: &Scene3D) {
    let hidden = ui
        .card
        .is_some_and(|i| scene.points.get(i as usize).is_none_or(|p| !visible(ui, p)));
    if hidden {
        ui.card = None;
    }
}

pub fn toggle_family(ui: &mut KeywordsUi, scene: &Scene3D, family: u16) {
    if family >= 16 {
        return;
    }
    ui.hidden_families ^= 1 << family;
    close_hidden_card(ui, scene);
}

pub fn toggle_use_case(ui: &mut KeywordsUi, scene: &Scene3D, use_case: u16) {
    if use_case >= 16 {
        return;
    }
    ui.hidden_use_cases ^= 1 << use_case;
    if ui.use_case == Some(use_case) {
        set_use_case(ui, scene, None);
    }
    close_hidden_card(ui, scene);
}

pub fn toggle_only_changed(ui: &mut KeywordsUi, scene: &Scene3D) {
    ui.only_changed = !ui.only_changed;
    close_hidden_card(ui, scene);
}

// ---- the regions

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PillBar(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct UseCasePill {
    pub pr: PrRef,
    pub use_case: Option<u16>,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FocusCard(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FilterBox(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FamilyToggle {
    pub pr: PrRef,
    pub family: u16,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct UseCaseToggle {
    pub pr: PrRef,
    pub use_case: u16,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct OnlyChangedToggle(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct LegendBox(pub PrRef);

/// What a region (or a row of the filters) was last built from.
#[derive(Component, Debug)]
struct Built<V: Send + Sync + 'static>(Option<V>);

impl<V: Send + Sync + 'static> Default for Built<V> {
    fn default() -> Self {
        Self(None)
    }
}

/// How many rows the filters panel has; the panel is redrawn when this changes, and each row
/// redraws itself, so a row a click lands on survives the click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FiltersShape {
    families: usize,
    use_cases: usize,
}

/// The pointer events of an interactive panel stop here: they must not reach the viewport under
/// it, whose click deselects and whose drag turns the camera.
fn block_pointer() -> impl Bundle {
    (
        (
            observe(|mut e: On<Pointer<Press>>| e.propagate(false)),
            observe(|mut e: On<Pointer<Click>>| e.propagate(false)),
            observe(|mut e: On<Pointer<DragStart>>| e.propagate(false)),
        ),
        (
            observe(|mut e: On<Pointer<Drag>>| e.propagate(false)),
            observe(|mut e: On<Pointer<DragEnd>>| e.propagate(false)),
            observe(|mut e: On<Pointer<Scroll>>| e.propagate(false)),
        ),
    )
}

fn panel() -> Node {
    Node {
        flex_direction: FlexDirection::Column,
        border: UiRect::all(px(1)),
        border_radius: BorderRadius::all(px(10)),
        ..default()
    }
}

/// Adds the four regions, empty: the left column (pills, then the focus card under them), the
/// filters and the legend.
pub(super) fn spawn_regions(p: &mut ChildSpawnerCommands, pr: &PrRef) {
    p.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(20),
            top: px(44),
            right: px(260),
            flex_direction: FlexDirection::Column,
            align_items: AlignItems::FlexStart,
            row_gap: px(8),
            ..default()
        },
        Pickable::IGNORE,
    ))
    .with_children(|c| {
        c.spawn((
            Node {
                max_width: percent(100),
                flex_wrap: FlexWrap::Wrap,
                column_gap: px(6),
                row_gap: px(6),
                ..default()
            },
            Pickable::IGNORE,
            PillBar(pr.clone()),
            Built::<PillsView>::default(),
            KeepOut(pr.clone()),
        ));
        c.spawn((
            Node {
                display: Display::None,
                width: px(250),
                padding: UiRect::all(px(12)),
                row_gap: px(6),
                ..panel()
            },
            BackgroundColor::default(),
            Fill(Swatch::Surface),
            BorderColor::default(),
            Stroke(Swatch::Line),
            FocusCard(pr.clone()),
            Built::<Option<FocusView>>::default(),
            KeepOut(pr.clone()),
            block_pointer(),
        ));
    });
    p.spawn((
        Node {
            position_type: PositionType::Absolute,
            right: px(16),
            top: px(52),
            width: px(212),
            padding: UiRect::axes(px(12), px(10)),
            row_gap: px(4),
            ..panel()
        },
        BackgroundColor::default(),
        Fill(Swatch::Surface),
        BorderColor::default(),
        Stroke(Swatch::Line),
        FilterBox(pr.clone()),
        Built::<FiltersShape>::default(),
        KeepOut(pr.clone()),
        block_pointer(),
    ));
    p.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(20),
            right: px(396),
            bottom: px(14),
            flex_wrap: FlexWrap::Wrap,
            align_items: AlignItems::Center,
            column_gap: px(16),
            row_gap: px(4),
            padding: UiRect::axes(px(12), px(7)),
            border: UiRect::all(px(1)),
            border_radius: BorderRadius::all(px(8)),
            ..default()
        },
        Pickable::IGNORE,
        BackgroundColor::default(),
        Fill(Swatch::Surface),
        BorderColor::default(),
        Stroke(Swatch::Line),
        LegendBox(pr.clone()),
        Built::<LegendView>::default(),
        KeepOut(pr.clone()),
    ));
}

type Runtimes<'w, 's> = Query<'w, 's, (&'static KeywordsCanvas, &'static KeywordsRuntime)>;

/// What a region needs to draw itself: the tab's state, the map's runtime and whether the entry
/// is shown against an older version.
struct Source<'a> {
    ui: &'a KeywordsUi,
    runtime: &'a KeywordsRuntime,
    compare: bool,
}

fn source<'a>(
    tabs: &'a ReviewTabs,
    diagrams: &Diagrams,
    runtimes: &'a Runtimes,
    pr: &PrRef,
) -> Option<Source<'a>> {
    let tab = tabs.0.get(pr)?;
    let (_, runtime) = runtimes.iter().find(|(c, _)| c.0 == *pr)?;
    let compare = tab.ui.diagram_compare
        && diagrams
            .0
            .get(pr)
            .and_then(|m| m.compare_with(runtime.number, &runtime.key))
            .is_some();
    Some(Source {
        ui: &tab.ui.keywords,
        runtime,
        compare,
    })
}

/// The name of an entry as a mark says: struck through when gone, in amber when changed, with a
/// `new` tag when added.
fn name_text(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    name: &str,
    style: Type,
    mark: Option<History>,
) {
    let style = match mark {
        Some(History::Removed) => style.ink(Swatch::Muted),
        Some(History::Changed) => style.ink(Swatch::Amber),
        _ => style,
    };
    {
        let mut e = p.spawn((text(fonts, name, style), TextLayout::no_wrap()));
        if mark == Some(History::Removed) {
            e.insert(Strikethrough);
        }
    }
    if mark == Some(History::Added) {
        p.spawn(text(fonts, "new", Type::META.size(10.0).ink(Swatch::Green)));
    }
}

fn dot(family: u16) -> impl Bundle {
    (
        Node {
            width: px(10),
            height: px(10),
            flex_shrink: 0.0,
            border_radius: BorderRadius::all(px(5)),
            ..default()
        },
        BackgroundColor::default(),
        Fill(Swatch::Family(u8::try_from(family).unwrap_or(u8::MAX))),
    )
}

fn muted(fonts: &UiFonts, s: impl Into<String>) -> impl Bundle {
    text(fonts, s, Type::MUTED.size(12.0))
}

fn section(p: &mut ChildSpawnerCommands, fonts: &UiFonts, title: &str) {
    p.spawn((
        Node {
            margin: UiRect::top(px(4)),
            ..default()
        },
        children![text(fonts, title, Type::MUTED.size(11.0))],
    ));
}

// ---- the pills

fn pill(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    use_case: Option<u16>,
    name: &str,
    places: Option<u32>,
    selected: bool,
    mark: Option<History>,
) {
    let (fill, stroke) = if selected {
        (Swatch::GreenSoft, Swatch::Green)
    } else {
        match mark {
            Some(History::Changed) => (Swatch::AmberSoft, Swatch::Amber),
            Some(History::Added) => (Swatch::Surface, Swatch::Green),
            _ => (Swatch::Surface, Swatch::Line),
        }
    };
    p.spawn((
        Node {
            height: px(26),
            padding: UiRect::axes(px(11), px(4)),
            border: UiRect::all(px(1)),
            border_radius: BorderRadius::all(px(999)),
            align_items: AlignItems::Center,
            column_gap: px(5),
            flex_shrink: 0.0,
            ..default()
        },
        BackgroundColor::default(),
        Fill(fill),
        BorderColor::default(),
        Stroke(stroke),
        (WidgetButton, Clickable),
        Hovered::default(),
        TabIndex(0),
        UseCasePill {
            pr: pr.clone(),
            use_case,
        },
        block_pointer(),
        observe(on_pill),
    ))
    .with_children(|c| {
        name_text(c, fonts, name, Type::BODY.size(12.0), mark);
        if let Some(n) = places {
            c.spawn(muted(fonts, n.to_string()));
        }
    });
}

fn draw_pills(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, v: &PillsView) {
    pill(
        p,
        fonts,
        pr,
        None,
        "All use cases",
        None,
        v.all_selected,
        None,
    );
    for pill_view in &v.pills {
        pill(
            p,
            fonts,
            pr,
            Some(pill_view.use_case),
            &pill_view.name,
            Some(pill_view.places),
            pill_view.selected,
            pill_view.mark,
        );
    }
}

/// Runs `act` on the tab's keyword state and the scene of its canvas; the tab is written only
/// when the state changed, so an action that does nothing does not wake what watches the tabs.
fn write(
    tabs: &mut ResMut<ReviewTabs>,
    runtimes: &Runtimes,
    pr: &PrRef,
    act: impl FnOnce(&mut KeywordsUi, &Scene3D),
) {
    let Some((_, runtime)) = runtimes.iter().find(|(c, _)| c.0 == *pr) else {
        return;
    };
    let Some(current) = tabs.0.get(pr).map(|t| t.ui.keywords.clone()) else {
        return;
    };
    let mut ui = current.clone();
    act(&mut ui, &runtime.scene);
    if ui != current
        && let Some(tab) = tabs.0.get_mut(pr)
    {
        tab.ui.keywords = ui;
    }
}

fn on_pill(
    activate: On<Activate>,
    pills: Query<&UseCasePill>,
    runtimes: Runtimes,
    mut tabs: ResMut<ReviewTabs>,
) {
    if let Ok(p) = pills.get(activate.entity) {
        write(&mut tabs, &runtimes, &p.pr, |ui, scene| {
            toggle_pill(ui, scene, p.use_case);
        });
    }
}

fn rebuild_pills(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    diagrams: Res<Diagrams>,
    fonts: Res<UiFonts>,
    runtimes: Runtimes,
    mut bars: Query<(Entity, &PillBar, &mut Built<PillsView>)>,
) {
    let dirty = tabs.is_changed() || diagrams.is_changed();
    for (entity, bar, mut built) in &mut bars {
        if !dirty && built.0.is_some() {
            continue;
        }
        let Some(s) = source(&tabs, &diagrams, &runtimes, &bar.0) else {
            continue;
        };
        let view = pills_view(&s.runtime.map, s.ui, s.compare);
        if built.0.as_ref() == Some(&view) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        let pr = bar.0.clone();
        commands
            .entity(entity)
            .with_children(|c| draw_pills(c, &fonts, &pr, &view));
        built.0 = Some(view);
    }
}

// ---- the focus card

fn draw_focus(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &FocusView) {
    p.spawn(Node {
        column_gap: px(6),
        align_items: AlignItems::Center,
        ..default()
    })
    .with_children(|h| name_text(h, fonts, &v.name, Type::STRONG.size(14.0), v.mark));
    p.spawn(muted(
        fonts,
        format!(
            "{} · {} changed in this PR",
            places_text(v.places),
            v.changed
        ),
    ));
    if let Some(note) = &v.note {
        p.spawn(text(fonts, note.clone(), Type::BODY.size(12.0)));
    }
    if v.places == 0 {
        p.spawn(muted(fonts, "No places yet."));
    } else {
        p.spawn((
            Node {
                margin: UiRect::top(px(4)),
                ..default()
            },
            children![text(fonts, "WORDS HERE", Type::MUTED.size(11.0))],
        ));
    }
    for row in &v.words {
        p.spawn(Node {
            column_gap: px(6),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|r| {
            r.spawn(dot(row.family));
            r.spawn((
                Node {
                    flex_grow: 1.0,
                    flex_shrink: 1.0,
                    min_width: px(0),
                    ..default()
                },
                children![(
                    text(
                        fonts,
                        row.text.clone(),
                        Type::MONO.size(12.0).ink(Swatch::Fg)
                    ),
                    TextLayout::no_wrap()
                )],
            ));
            r.spawn(muted(fonts, row.places.to_string()));
            if row.changed > 0 {
                r.spawn(text(
                    fonts,
                    format!("· {} changed", row.changed),
                    Type::META.ink(Swatch::Green),
                ));
            }
        });
    }
    if v.more_words > 0 {
        p.spawn(muted(fonts, format!("+{} more words", v.more_words)));
    }
    if !v.also_reach.is_empty() {
        p.spawn((
            Node {
                margin: UiRect::top(px(4)),
                ..default()
            },
            children![text(
                fonts,
                "THE SAME CHANGES ALSO REACH",
                Type::MUTED.size(11.0)
            )],
        ));
        p.spawn(text(fonts, v.also_reach.join(" · "), Type::BODY.size(12.0)));
    }
}

fn rebuild_focus(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    diagrams: Res<Diagrams>,
    fonts: Res<UiFonts>,
    runtimes: Runtimes,
    mut cards: Query<(Entity, &FocusCard, &mut Node, &mut Built<Option<FocusView>>)>,
) {
    let dirty = tabs.is_changed() || diagrams.is_changed();
    for (entity, card, mut node, mut built) in &mut cards {
        if !dirty && built.0.is_some() {
            continue;
        }
        let Some(s) = source(&tabs, &diagrams, &runtimes, &card.0) else {
            continue;
        };
        let view =
            s.ui.use_case
                .and_then(|u| focus_view(&s.runtime.map, u, s.compare));
        if built.0.as_ref() == Some(&view) {
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
            commands
                .entity(entity)
                .with_children(|c| draw_focus(c, &fonts, v));
        }
        built.0 = Some(view);
    }
}

// ---- the filters

/// One checkbox row of the filters: the row is the button, so the whole line toggles.
fn filter_row(on: bool) -> impl Bundle {
    (
        Node {
            min_height: px(22),
            column_gap: px(8),
            align_items: AlignItems::Center,
            border_radius: BorderRadius::all(px(4)),
            ..default()
        },
        BackgroundColor::default(),
        Fill(Swatch::Clear),
        HoverFill(Swatch::Hover),
        (WidgetButton, Clickable),
        Hovered::default(),
        TabIndex(0),
        Built::<bool>(Some(on)),
    )
}

fn draw_filter_row(p: &mut ChildSpawnerCommands, fonts: &UiFonts, row: &FilterRow, family: bool) {
    p.spawn(checkbox_box(row.on, true));
    if family {
        p.spawn(dot(row.index));
    }
    p.spawn((
        Node {
            flex_grow: 1.0,
            flex_shrink: 1.0,
            min_width: px(0),
            column_gap: px(6),
            overflow: Overflow::clip(),
            ..default()
        },
        Pickable::IGNORE,
    ))
    .with_children(|l| name_text(l, fonts, &row.label, Type::BODY.size(12.0), row.mark));
    p.spawn(muted(fonts, row.places.to_string()));
}

fn draw_only_changed(p: &mut ChildSpawnerCommands, fonts: &UiFonts, on: bool) {
    p.spawn(checkbox_box(on, true));
    p.spawn(text(
        fonts,
        "Only what the PR changed",
        Type::BODY.size(12.0),
    ));
}

fn draw_filters(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, v: &FiltersView) {
    section(p, fonts, "WORD FAMILIES");
    for row in &v.families {
        p.spawn((
            Node {
                min_height: px(22),
                column_gap: px(8),
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(px(4)),
                ..default()
            },
            BackgroundColor::default(),
            Fill(Swatch::Clear),
            HoverFill(Swatch::Hover),
            (WidgetButton, Clickable),
            Hovered::default(),
            TabIndex(0),
            Built::<FilterRow>::default(),
            FamilyToggle {
                pr: pr.clone(),
                family: row.index,
            },
            observe(on_family),
        ));
    }
    section(p, fonts, "USE CASES");
    for row in &v.use_cases {
        p.spawn((
            Node {
                min_height: px(22),
                column_gap: px(8),
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(px(4)),
                ..default()
            },
            BackgroundColor::default(),
            Fill(Swatch::Clear),
            HoverFill(Swatch::Hover),
            (WidgetButton, Clickable),
            Hovered::default(),
            TabIndex(0),
            Built::<FilterRow>::default(),
            UseCaseToggle {
                pr: pr.clone(),
                use_case: row.index,
            },
            observe(on_use_case),
        ));
    }
    section(p, fonts, "SHOW");
    p.spawn((
        filter_row(v.only_changed),
        OnlyChangedToggle(pr.clone()),
        observe(on_only_changed),
    ))
    .with_children(|c| draw_only_changed(c, fonts, v.only_changed));
}

fn on_family(
    activate: On<Activate>,
    toggles: Query<&FamilyToggle>,
    runtimes: Runtimes,
    mut tabs: ResMut<ReviewTabs>,
) {
    if let Ok(t) = toggles.get(activate.entity) {
        write(&mut tabs, &runtimes, &t.pr, |ui, scene| {
            toggle_family(ui, scene, t.family);
        });
    }
}

fn on_use_case(
    activate: On<Activate>,
    toggles: Query<&UseCaseToggle>,
    runtimes: Runtimes,
    mut tabs: ResMut<ReviewTabs>,
) {
    if let Ok(t) = toggles.get(activate.entity) {
        write(&mut tabs, &runtimes, &t.pr, |ui, scene| {
            toggle_use_case(ui, scene, t.use_case);
        });
    }
}

fn on_only_changed(
    activate: On<Activate>,
    toggles: Query<&OnlyChangedToggle>,
    runtimes: Runtimes,
    mut tabs: ResMut<ReviewTabs>,
) {
    if let Ok(t) = toggles.get(activate.entity) {
        write(&mut tabs, &runtimes, &t.0, |ui, scene| {
            toggle_only_changed(ui, scene);
        });
    }
}

type FamilyRows<'w, 's> = Query<
    'w,
    's,
    (Entity, &'static FamilyToggle, &'static mut Built<FilterRow>),
    Without<UseCaseToggle>,
>;
type CaseRows<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static UseCaseToggle,
        &'static mut Built<FilterRow>,
    ),
    Without<FamilyToggle>,
>;

fn rebuild_filters(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    diagrams: Res<Diagrams>,
    fonts: Res<UiFonts>,
    runtimes: Runtimes,
    mut boxes: Query<(Entity, &FilterBox, &mut Built<FiltersShape>)>,
    mut families: FamilyRows,
    mut cases: CaseRows,
    mut only: Query<(Entity, &OnlyChangedToggle, &mut Built<bool>)>,
) {
    let dirty = tabs.is_changed() || diagrams.is_changed();
    let pending = families.iter().any(|(_, _, b)| b.0.is_none())
        || cases.iter().any(|(_, _, b)| b.0.is_none());
    for (entity, panel, mut built) in &mut boxes {
        if !(dirty || pending || built.0.is_none()) {
            continue;
        }
        let Some(s) = source(&tabs, &diagrams, &runtimes, &panel.0) else {
            continue;
        };
        let view = filters_view(&s.runtime.map, s.ui, s.compare);
        let shape = FiltersShape {
            families: view.families.len(),
            use_cases: view.use_cases.len(),
        };
        if built.0 != Some(shape) {
            commands.entity(entity).despawn_related::<Children>();
            let pr = panel.0.clone();
            commands
                .entity(entity)
                .with_children(|c| draw_filters(c, &fonts, &pr, &view));
            built.0 = Some(shape);
            continue;
        }
        for (row_entity, toggle, mut row_built) in &mut families {
            let Some(row) = view
                .families
                .iter()
                .find(|r| toggle.pr == panel.0 && r.index == toggle.family)
            else {
                continue;
            };
            if row_built.0.as_ref() != Some(row) {
                commands.entity(row_entity).despawn_related::<Children>();
                commands
                    .entity(row_entity)
                    .with_children(|c| draw_filter_row(c, &fonts, row, true));
                row_built.0 = Some(row.clone());
            }
        }
        for (row_entity, toggle, mut row_built) in &mut cases {
            let Some(row) = view
                .use_cases
                .iter()
                .find(|r| toggle.pr == panel.0 && r.index == toggle.use_case)
            else {
                continue;
            };
            if row_built.0.as_ref() != Some(row) {
                commands.entity(row_entity).despawn_related::<Children>();
                commands
                    .entity(row_entity)
                    .with_children(|c| draw_filter_row(c, &fonts, row, false));
                row_built.0 = Some(row.clone());
            }
        }
        for (row_entity, toggle, mut row_built) in &mut only {
            if toggle.0 == panel.0 && row_built.0 != Some(view.only_changed) {
                commands.entity(row_entity).despawn_related::<Children>();
                commands
                    .entity(row_entity)
                    .with_children(|c| draw_only_changed(c, &fonts, view.only_changed));
                row_built.0 = Some(view.only_changed);
            }
        }
    }
}

// ---- the legend

fn legend_item(p: &mut ChildSpawnerCommands, fonts: &UiFonts, shape: impl Bundle, label: &str) {
    p.spawn(Node {
        column_gap: px(6),
        align_items: AlignItems::Center,
        ..default()
    })
    .with_children(|c| {
        c.spawn(shape);
        c.spawn(muted(fonts, label));
    });
}

fn draw_legend(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &LegendView) {
    p.spawn(text(fonts, "SHAPE", Type::MUTED.size(11.0)));
    // Shapes are nodes, not glyphs: the bundled fonts have none of them.
    legend_item(
        p,
        fonts,
        (
            Node {
                width: px(8),
                height: px(8),
                ..default()
            },
            UiTransform::from_rotation(Rot2::degrees(45.0)),
            BackgroundColor::default(),
            Fill(Swatch::Muted),
        ),
        "Declared",
    );
    legend_item(
        p,
        fonts,
        (
            Node {
                width: px(8),
                height: px(8),
                border_radius: BorderRadius::all(px(4)),
                ..default()
            },
            BackgroundColor::default(),
            Fill(Swatch::Muted),
        ),
        "Used",
    );
    legend_item(
        p,
        fonts,
        (
            Node {
                width: px(16),
                height: px(16),
                border_radius: BorderRadius::all(px(8)),
                ..default()
            },
            BackgroundColor::default(),
            Fill(Swatch::Faint),
        ),
        "Many uses in one file",
    );
    p.spawn((
        Node {
            width: px(1),
            height: px(14),
            ..default()
        },
        BackgroundColor::default(),
        Fill(Swatch::Line),
    ));
    legend_item(
        p,
        fonts,
        (
            Node {
                width: px(14),
                height: px(14),
                border: UiRect::all(px(2)),
                border_radius: BorderRadius::all(px(7)),
                ..default()
            },
            BorderColor::default(),
            Stroke(Swatch::Green),
        ),
        "Changed in this PR",
    );
    if let Some(more) = &v.more {
        p.spawn(muted(fonts, more.clone()));
    }
    if let Some(cut) = &v.cut {
        p.spawn(text(fonts, cut.clone(), Type::META.ink(Swatch::Amber)));
    }
}

fn rebuild_legend(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    diagrams: Res<Diagrams>,
    fonts: Res<UiFonts>,
    runtimes: Runtimes,
    mut legends: Query<(Entity, &LegendBox, &mut Built<LegendView>)>,
) {
    let dirty = tabs.is_changed() || diagrams.is_changed();
    for (entity, legend, mut built) in &mut legends {
        if !dirty && built.0.is_some() {
            continue;
        }
        let Some(s) = source(&tabs, &diagrams, &runtimes, &legend.0) else {
            continue;
        };
        let view = legend_view(s.runtime.scene.more, s.runtime.map.search_cut);
        if built.0.as_ref() == Some(&view) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        commands
            .entity(entity)
            .with_children(|c| draw_legend(c, &fonts, &view));
        built.0 = Some(view);
    }
}

// ---- the clouds' names

/// Makes the name of a use case on the scene a button, as its pill is.
fn wire_cloud_labels(mut commands: Commands, added: Query<Entity, Added<CloudLabel>>) {
    for label in &added {
        commands
            .entity(label)
            .insert((WidgetButton, Clickable, Hovered::default()))
            .observe(on_cloud_label);
    }
}

fn on_cloud_label(
    activate: On<Activate>,
    labels: Query<&CloudLabel>,
    runtimes: Runtimes,
    mut tabs: ResMut<ReviewTabs>,
) {
    if let Ok(label) = labels.get(activate.entity) {
        write(&mut tabs, &runtimes, &label.pr, |ui, scene| {
            toggle_pill(ui, scene, Some(label.use_case));
        });
    }
}

/// Adds the systems of the panels and of the keyboard.
pub struct OverlaysPlugin;

impl Plugin for OverlaysPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (
                keys::wire_canvas,
                wire_cloud_labels,
                rebuild_pills,
                rebuild_focus,
                rebuild_filters,
                rebuild_legend,
            )
                .chain()
                .after(ReviewSystems)
                .after(super::fill_keywords),
        );
    }
}

#[cfg(test)]
pub(super) mod fixture_map {
    use clusia_core::keywords::{
        AddedLines, Axis, Family, Hit, KeywordMap, KeywordPlacement, KeywordPlan, Region, Seed,
        UseCase, build_map,
    };

    fn axis(name: &str) -> Axis {
        Axis {
            name: name.into(),
            low: "low".into(),
            high: "high".into(),
        }
    }

    fn hit(path: &str, line: u32, text: &str) -> Hit {
        Hit {
            path: path.into(),
            line,
            text: text.into(),
        }
    }

    fn region(path: &str, use_case: &str, at: [f32; 3]) -> Region {
        Region {
            path: path.into(),
            start_line: None,
            end_line: None,
            use_case: use_case.into(),
            at,
        }
    }

    /// Two families (`refresh` with 6 places, `lock` with 2 or, with `bundle`, 16), three use
    /// cases (`Token refresh` 5 places and 2 changed, `Request signing` 2, `Login` 0) and
    /// one place in a file no region covers (Elsewhere). With `bundle`, 14 more unchanged uses
    /// of `lock` in that same file make one bundle.
    pub fn small_map(bundle: bool) -> KeywordMap {
        let plan = KeywordPlan {
            title: "Where the words live".into(),
            caption: String::new(),
            axes: [axis("Layer"), axis("Moment"), axis("Distance")],
            families: vec![
                Family {
                    id: "f1".into(),
                    label: "refresh".into(),
                    seed: Seed {
                        word: "refresh_token".into(),
                        path: "src/token/store.rs".into(),
                        line: 10,
                    },
                    words: vec!["refresh_token".into(), "refresh".into()],
                    history: None,
                },
                Family {
                    id: "f2".into(),
                    label: "lock".into(),
                    seed: Seed {
                        word: "refresh_lock".into(),
                        path: "src/token/lock.rs".into(),
                        line: 5,
                    },
                    words: vec!["refresh_lock".into(), "lock".into()],
                    history: None,
                },
            ],
            use_cases: ["Token refresh", "Request signing", "Login"]
                .iter()
                .enumerate()
                .map(|(i, name)| UseCase {
                    id: format!("u{}", i + 1),
                    name: (*name).into(),
                    note: None,
                    history: None,
                })
                .collect(),
        };
        let placement = KeywordPlacement {
            regions: vec![
                region("src/token/store.rs", "u1", [0.8, 0.5, 0.2]),
                region("src/client/http.rs", "u2", [0.2, 0.3, 0.7]),
                region("src/token/lock.rs", "u1", [0.7, 0.4, 0.2]),
            ],
        };
        let mut hits = vec![
            hit(
                "src/token/store.rs",
                10,
                "pub fn refresh_token(&self) -> Token {",
            ),
            hit(
                "src/token/store.rs",
                20,
                "    let t = self.refresh_token();",
            ),
            hit("src/token/store.rs", 30, "    refresh();"),
            hit("src/client/http.rs", 5, "let t = store.refresh_token();"),
            hit("src/client/http.rs", 9, "    refresh();"),
            hit("src/token/lock.rs", 5, "pub fn refresh_lock(&self) {"),
            hit("src/token/lock.rs", 8, "    self.lock();"),
            hit("src/ui/menu.rs", 3, "    refresh();"),
        ];
        if bundle {
            hits.extend((101..=114).map(|l| hit("src/ui/menu.rs", l, "    self.lock();")));
        }
        let mut added = AddedLines::new();
        added
            .entry("src/token/store.rs".into())
            .or_default()
            .insert(10);
        added
            .entry("src/token/lock.rs".into())
            .or_default()
            .insert(5);
        build_map(plan, placement, &hits, vec![0; 4], false, &added)
    }
}

#[cfg(test)]
mod tests {
    use super::fixture_map::small_map;
    use super::*;
    use crate::fixture;
    use crate::review_state::{CameraPose, KeywordsUi, View3D};
    use crate::screens::review::diagrams::keywords::render::{focus_pose, view_pose};
    use crate::screens::review::diagrams::keywords::{CloudLabel, KeywordsRuntime};
    use crate::testing;
    use clusia_core::diagram::History;
    use clusia_diagram::keywords::scene;

    fn names(rows: &[FilterRow]) -> Vec<(&str, u32, bool)> {
        rows.iter()
            .map(|r| (r.label.as_str(), r.places, r.on))
            .collect()
    }

    #[test]
    fn pills_count_the_places_of_each_use_case() {
        let map = small_map(false);
        let v = pills_view(&map, &KeywordsUi::default(), false);
        assert!(v.all_selected);
        let got: Vec<(&str, u32, bool)> = v
            .pills
            .iter()
            .map(|p| (p.name.as_str(), p.places, p.selected))
            .collect();
        assert_eq!(
            got,
            [
                ("Token refresh", 5, false),
                ("Request signing", 2, false),
                ("Login", 0, false)
            ]
        );
        let focused = KeywordsUi {
            use_case: Some(1),
            ..Default::default()
        };
        let v = pills_view(&map, &focused, false);
        assert!(!v.all_selected);
        assert_eq!(
            v.pills.iter().map(|p| p.selected).collect::<Vec<_>>(),
            [false, true, false]
        );
    }

    #[test]
    fn marks_show_only_when_comparing() {
        let mut map = small_map(false);
        map.plan.use_cases[1].history = Some(History::Added);
        map.plan.use_cases[0].history = Some(History::Same);
        map.plan.families[1].history = Some(History::Removed);
        let ui = KeywordsUi::default();
        let marks = |compare| {
            pills_view(&map, &ui, compare)
                .pills
                .iter()
                .map(|p| p.mark)
                .collect::<Vec<_>>()
        };
        assert_eq!(marks(false), [None, None, None]);
        assert_eq!(marks(true), [None, Some(History::Added), None]);
        let f = filters_view(&map, &ui, true);
        assert_eq!(
            f.families.iter().map(|r| r.mark).collect::<Vec<_>>(),
            [None, Some(History::Removed)]
        );
        assert_eq!(filters_view(&map, &ui, false).families[1].mark, None);
    }

    #[test]
    fn the_focus_card_lists_words_and_where_the_changes_also_reach() {
        let map = small_map(false);
        let card = focus_view(&map, 0, false).expect("a use case");
        assert_eq!(
            (card.name.as_str(), card.places, card.changed),
            ("Token refresh", 5, 2)
        );
        let rows: Vec<(&str, u16, u32, u32)> = card
            .words
            .iter()
            .map(|w| (w.text.as_str(), w.family, w.places, w.changed))
            .collect();
        assert_eq!(
            rows,
            [
                ("refresh_token", 0, 2, 1),
                ("lock", 1, 1, 0),
                ("refresh", 0, 1, 0),
                ("refresh_lock", 1, 1, 1)
            ],
            "most places first, then by name"
        );
        assert_eq!(card.more_words, 0);
        assert!(card.also_reach.is_empty(), "nothing changed outside it");
        let card = focus_view(&map, 1, false).unwrap();
        assert_eq!((card.places, card.changed), (2, 0));
        assert_eq!(card.also_reach, ["Token refresh"]);
        let empty = focus_view(&map, 2, false).unwrap();
        assert_eq!((empty.places, empty.words.len()), (0, 0));
        assert!(empty.also_reach.is_empty());
        assert!(focus_view(&map, 3, false).is_none(), "no such use case");
    }

    #[test]
    fn the_focus_card_cuts_a_long_word_list() {
        let mut map = small_map(false);
        for i in 0..20 {
            map.words.push(clusia_core::keywords::MapWord {
                text: format!("extra_{i:02}"),
                family: 0,
            });
            map.places.push(clusia_core::keywords::Place {
                path: 0,
                line: 100 + i,
                word: (map.words.len() - 1) as u16,
                region: 0,
                flags: 0,
            });
        }
        // Region 0 is the first use case's, so the new places join it.
        let card = focus_view(&map, 0, false).unwrap();
        assert_eq!(card.words.len(), MAX_FOCUS_WORDS);
        assert_eq!(card.more_words as usize, 4 + 20 - MAX_FOCUS_WORDS);
    }

    #[test]
    fn filters_count_places_and_show_what_is_off() {
        let map = small_map(false);
        let mut ui = KeywordsUi::default();
        let f = filters_view(&map, &ui, false);
        assert_eq!(
            names(&f.families),
            [("refresh", 6, true), ("lock", 2, true)]
        );
        assert_eq!(
            names(&f.use_cases),
            [
                ("Token refresh", 5, true),
                ("Request signing", 2, true),
                ("Login", 0, true)
            ]
        );
        assert!(!f.only_changed);
        ui.hidden_families = 0b10;
        ui.hidden_use_cases = 0b100;
        ui.only_changed = true;
        let f = filters_view(&map, &ui, false);
        assert_eq!(
            f.families.iter().map(|r| r.on).collect::<Vec<_>>(),
            [true, false]
        );
        assert_eq!(
            f.use_cases.iter().map(|r| r.on).collect::<Vec<_>>(),
            [true, true, false]
        );
        assert!(f.only_changed);
    }

    #[test]
    fn the_legend_counts_what_is_not_drawn() {
        assert_eq!(
            legend_view(0, false),
            LegendView {
                more: None,
                cut: None
            }
        );
        assert_eq!(
            legend_view(212, false).more.as_deref(),
            Some("+212 more places")
        );
        assert_eq!(legend_view(1, false).more.as_deref(), Some("+1 more place"));
        assert_eq!(
            legend_view(0, true).cut.as_deref(),
            Some("Search stopped after 10 s; some places are missing.")
        );
    }

    #[test]
    fn focusing_flies_the_commanded_camera_and_deselecting_returns_it() {
        let map = small_map(false);
        let scene = scene(&map);
        let mut ui = KeywordsUi {
            card: Some(1),
            hidden_use_cases: 0b1,
            ..Default::default()
        };
        set_use_case(&mut ui, &scene, Some(0));
        assert_eq!(ui.use_case, Some(0));
        assert_eq!(ui.camera, focus_pose(&scene, 0));
        assert_eq!(
            (ui.card, ui.hidden_use_cases),
            (None, 0),
            "the card closes, the use case shows"
        );
        set_use_case(&mut ui, &scene, None);
        assert_eq!(ui.use_case, None);
        assert_eq!(ui.camera, CameraPose::default());
        ui.view = View3D::XY;
        set_use_case(&mut ui, &scene, None);
        assert_eq!(ui.camera, view_pose(View3D::XY, &CameraPose::default()));
        set_use_case(&mut ui, &scene, Some(2));
        assert_eq!(ui.use_case, Some(2));
        assert_eq!(
            ui.camera,
            view_pose(View3D::XY, &CameraPose::default()),
            "no point to fly to: the view's own pose"
        );
    }

    #[test]
    fn a_pill_on_the_focused_use_case_or_all_deselects() {
        let scene = scene(&small_map(false));
        let mut ui = KeywordsUi::default();
        toggle_pill(&mut ui, &scene, Some(0));
        assert_eq!(ui.use_case, Some(0));
        toggle_pill(&mut ui, &scene, Some(1));
        assert_eq!(ui.use_case, Some(1));
        toggle_pill(&mut ui, &scene, Some(1));
        assert_eq!(ui.use_case, None);
        toggle_pill(&mut ui, &scene, Some(0));
        toggle_pill(&mut ui, &scene, None);
        assert_eq!(ui.use_case, None);
        assert_eq!(ui.camera, CameraPose::default());
    }

    #[test]
    fn filters_write_their_bits_and_close_a_card_that_would_be_hidden() {
        let scene = scene(&small_map(false));
        let lock = scene
            .points
            .iter()
            .position(|p| p.family == 1)
            .expect("a lock point") as u32;
        let refresh = scene
            .points
            .iter()
            .position(|p| p.family == 0)
            .expect("a refresh point") as u32;
        let mut ui = KeywordsUi {
            card: Some(lock),
            ..Default::default()
        };
        toggle_family(&mut ui, &scene, 0);
        assert_eq!(ui.hidden_families, 0b01);
        assert_eq!(ui.card, Some(lock), "the card's own family is still shown");
        toggle_family(&mut ui, &scene, 1);
        assert_eq!(ui.hidden_families, 0b11);
        assert_eq!(ui.card, None);
        toggle_family(&mut ui, &scene, 0);
        toggle_family(&mut ui, &scene, 1);
        assert_eq!(ui.hidden_families, 0);
        ui.card = Some(refresh);
        toggle_use_case(&mut ui, &scene, 0);
        assert_eq!(ui.hidden_use_cases, 0b1);
        ui.use_case = Some(1);
        toggle_use_case(&mut ui, &scene, 1);
        assert_eq!(
            (ui.hidden_use_cases, ui.use_case),
            (0b11, None),
            "hiding the focused use case deselects it"
        );
        toggle_use_case(&mut ui, &scene, 1);
        toggle_use_case(&mut ui, &scene, 0);
        let plain = scene
            .points
            .iter()
            .position(|p| !p.changed && p.shape == clusia_diagram::keywords::PointShape::Used)
            .unwrap() as u32;
        ui.card = Some(plain);
        toggle_only_changed(&mut ui, &scene);
        assert!(ui.only_changed);
        assert_eq!(
            ui.card, None,
            "an unchanged use is hidden by Only what the PR changed"
        );
        toggle_only_changed(&mut ui, &scene);
        assert!(!ui.only_changed);
    }

    #[test]
    fn the_panels_follow_the_tab() {
        let mut app = testing::app(fixture::demo(testing::NOW));
        let pr = testing::open_keywords(&mut app);
        let map = fixture::keywords_map();
        let cases = map.plan.use_cases.len();
        assert_eq!(testing::count::<UseCasePill>(&mut app), cases + 1);
        assert_eq!(
            testing::count::<FamilyToggle>(&mut app),
            map.plan.families.len()
        );
        assert_eq!(testing::count::<UseCaseToggle>(&mut app), cases);
        assert_eq!(testing::count::<OnlyChangedToggle>(&mut app), 1);
        assert_eq!(testing::count::<LegendBox>(&mut app), 1);
        for text in [
            "All use cases",
            "WORD FAMILIES",
            "USE CASES",
            "Only what the PR changed",
            "SHAPE",
            "Declared",
            "Used",
            "Many uses in one file",
            "Changed in this PR",
        ] {
            assert!(testing::shows(&mut app, text), "{text}");
        }
        assert!(!testing::shows(&mut app, "WORDS HERE"));
        let pill = testing::find::<UseCasePill>(&mut app, |p| p.use_case == Some(0));
        testing::activate(&mut app, pill);
        testing::settle(&mut app);
        assert_eq!(testing::keywords_ui(&app, &pr).use_case, Some(0));
        assert!(testing::shows(&mut app, "WORDS HERE"));
        assert!(testing::shows(&mut app, &map.plan.use_cases[0].name));
        let again = testing::find::<UseCasePill>(&mut app, |p| p.use_case == Some(0));
        testing::activate(&mut app, again);
        testing::settle(&mut app);
        assert_eq!(testing::keywords_ui(&app, &pr).use_case, None);
        assert!(!testing::shows(&mut app, "WORDS HERE"));
    }

    #[test]
    fn filter_checkboxes_write_the_tab_state() {
        let mut app = testing::app(fixture::demo(testing::NOW));
        let pr = testing::open_keywords(&mut app);
        let family = testing::find::<FamilyToggle>(&mut app, |t| t.family == 1);
        testing::activate(&mut app, family);
        assert_eq!(testing::keywords_ui(&app, &pr).hidden_families, 0b10);
        let case = testing::find::<UseCaseToggle>(&mut app, |t| t.use_case == 2);
        testing::activate(&mut app, case);
        assert_eq!(testing::keywords_ui(&app, &pr).hidden_use_cases, 0b100);
        let only = testing::find::<OnlyChangedToggle>(&mut app, |_| true);
        testing::activate(&mut app, only);
        assert!(testing::keywords_ui(&app, &pr).only_changed);
        testing::activate(&mut app, family);
        testing::settle(&mut app);
        assert_eq!(testing::keywords_ui(&app, &pr).hidden_families, 0);
    }

    #[test]
    fn a_cloud_name_is_a_button_that_focuses_and_the_camera_snaps_when_motion_is_reduced() {
        let mut app = testing::app(fixture::demo(testing::NOW));
        app.world_mut()
            .insert_resource(crate::ui::spinner::Motion { reduced: true });
        let pr = testing::open_keywords(&mut app);
        let label = app
            .world_mut()
            .spawn(CloudLabel {
                pr: pr.clone(),
                use_case: 0,
            })
            .id();
        testing::settle(&mut app);
        testing::activate(&mut app, label);
        testing::settle(&mut app);
        assert_eq!(testing::keywords_ui(&app, &pr).use_case, Some(0));
        let (scene, _) = testing::keywords_runtime(&app, &pr);
        let pose = app
            .world_mut()
            .query::<&KeywordsRuntime>()
            .single(app.world())
            .expect("one canvas")
            .pose;
        assert_eq!(
            pose,
            focus_pose(&scene, 0),
            "reduced motion: no fly, the pose is there at once"
        );
        testing::activate(&mut app, label);
        testing::settle(&mut app);
        assert_eq!(
            testing::keywords_ui(&app, &pr).use_case,
            None,
            "the same name again deselects"
        );
    }

    #[test]
    fn the_legend_says_how_many_places_are_not_drawn() {
        let mut app = testing::app(fixture::demo(testing::NOW));
        let pr = testing::open_keywords(&mut app);
        let more = testing::keywords_runtime(&app, &pr).0.more;
        assert_eq!(
            testing::shows(&mut app, &format!("+{more} more places")),
            more > 1
        );
        assert!(!testing::shows(&mut app, "Search stopped after 10 s"));
    }

    #[test]
    fn a_theme_switch_keeps_the_focus() {
        let mut app = testing::app(fixture::demo(testing::NOW));
        let pr = testing::open_keywords(&mut app);
        testing::set_keywords(&mut app, &pr, |ui| ui.use_case = Some(1));
        testing::settle(&mut app);
        *app.world_mut().resource_mut::<crate::theme::Theme>() =
            crate::theme::Theme::new(true, 13, clusia_core::Density::Comfortable);
        testing::settle(&mut app);
        assert_eq!(testing::keywords_ui(&app, &pr).use_case, Some(1));
        assert!(testing::shows(&mut app, "WORDS HERE"));
    }

    #[test]
    fn with_every_family_hidden_the_panels_still_work() {
        let mut app = testing::app(fixture::demo(testing::NOW));
        let pr = testing::open_keywords(&mut app);
        testing::set_keywords(&mut app, &pr, |ui| ui.hidden_families = u16::MAX);
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "WORD FAMILIES"));
        assert!(testing::shows(&mut app, "All use cases"));
    }
}
