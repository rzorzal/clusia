//! The test list of the Auto Tests layout: a windowed column of fixed-height rows. Only the
//! rows in view (and a margin) exist; two spacers weigh the rest. A row is kept by the index of
//! its result in the source, so a result arriving, or one that sorts above the rows drawn, never
//! respawns a row that is already there.

use std::collections::{BTreeSet, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, Button as WidgetButton, ScrollArea, observe};
use clusia_core::PrRef;
use clusia_core::printable::printable;
use clusia_core::tests::{TestScope, TestStatus};

use super::body::ListRegion;
use super::{
    TEST_ROW_HEIGHT, TabState, TestCaseResult, TestFilter, TestsModel, TestsModels, row_duration,
    status_rank,
};
use crate::bridge::{Ask, Asks, Model, Toasts};
use crate::fonts::UiFonts;
use crate::review_state::ReviewTabs;
use crate::screens::review::editor::{not_sent, read_only_reason};
use crate::screens::review::rows;
use crate::theme::Swatch;
use crate::ui::kit::{
    Clickable, Fill, HoverFill, Preview, Stroke, Type, Variant, button, card, chip, divider, text,
    wrapped_text,
};

/// The longest name a row shows, in characters; the rest is cut with `…`.
pub const NAME_COLUMNS: usize = 80;

const NO_RUN: &str = "No run yet. Run the tests near this change, or all of them.";
const NO_MATCH: &str = "No tests match this filter";

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct TestRow {
    pub pr: PrRef,
    pub name: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RowRun {
    pub pr: PrRef,
    pub name: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FilterChip {
    pub pr: PrRef,
    pub filter: TestFilter,
}

/// Which results the rows are drawn from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Source {
    Nothing,
    Run(String),
    Live(String),
}

/// What the header shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Head {
    pub failed: usize,
    pub near: usize,
    pub filter: TestFilter,
}

/// What decides the rows and their order. A source's results only grow (a live run) or never
/// change (a stored run), so `(source, total)` names the row set exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Shape {
    pub source: Source,
    pub total: usize,
    pub filter: TestFilter,
    /// A hash of the names in `view.near`.
    pub near_key: u64,
    pub can_run: bool,
    pub note: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ListView {
    pub head: Head,
    pub shape: Shape,
    pub selected: Option<String>,
}

/// What a row looks like besides its name: when it changes the row is filled again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowKey {
    status: TestStatus,
    duration_ms: Option<u64>,
    selected: bool,
    can_run: bool,
}

impl RowKey {
    fn of(result: &TestCaseResult, selected: bool, can_run: bool) -> Self {
        Self {
            status: result.status,
            duration_ms: result.duration_ms,
            selected,
            can_run,
        }
    }
}

struct Slot {
    index: u32,
    entity: Entity,
    key: RowKey,
}

pub(super) struct Built {
    /// Indices into the source's results, in list order.
    pub order: Vec<u32>,
    heights: Vec<f32>,
    pub window: Range<usize>,
    slots: Vec<Slot>,
    /// Top and bottom spacer, and the weights they hold.
    spacers: [Entity; 2],
    spaced: [f32; 2],
    /// The body's children as last set.
    children: Vec<Entity>,
    /// Counts the passes that changed something; a settled list does not count.
    pub passes: u32,
}

/// The state kept on the `ListRegion` entity.
#[derive(Component)]
pub(super) struct ListState {
    view: ListView,
    pub body: Entity,
    header: Entity,
    note: Entity,
    pub built: Built,
}

/// The results the rows are drawn from, the same choice `TestsModel::rows` makes.
pub(super) fn source_results(model: &TestsModel) -> Option<(Source, &[TestCaseResult])> {
    if let Some(live) = &model.live {
        return Some((Source::Live(live.id.clone()), &live.results));
    }
    let run = model.listed_run()?;
    Some((Source::Run(run.id.clone()), &run.results))
}

/// The selected test, by default the first failure of the listed run (never while a run goes).
pub(super) fn selected_name(model: &TestsModel) -> Option<String> {
    if model.selected.is_some() {
        return model.selected.clone();
    }
    if model.live.is_some() {
        return None;
    }
    model
        .listed_run()?
        .results
        .iter()
        .find(|r| r.status == TestStatus::Failed)
        .map(|r| r.name.clone())
}

fn accepts(result: &TestCaseResult, filter: TestFilter, near: &BTreeSet<&str>) -> bool {
    match filter {
        TestFilter::All => true,
        TestFilter::Failed => result.status == TestStatus::Failed,
        TestFilter::Near => near.contains(result.name.as_str()),
    }
}

/// The indices of the results the filter accepts, failed first, then passed, then skipped, each
/// in arrival order.
pub(super) fn order_of(
    results: &[TestCaseResult],
    filter: TestFilter,
    near: &BTreeSet<&str>,
) -> Vec<u32> {
    let mut order: Vec<u32> = results
        .iter()
        .enumerate()
        .filter(|(_, r)| accepts(r, filter, near))
        .map(|(i, _)| i as u32)
        .collect();
    order.sort_by_key(|&i| status_rank(results[i as usize].status));
    order
}

/// The name as one escaped line, cut to `NAME_COLUMNS` characters.
pub(super) fn cut_name(name: &str) -> String {
    let name = printable(name);
    if name.chars().count() <= NAME_COLUMNS {
        return name;
    }
    let mut cut: String = name.chars().take(NAME_COLUMNS - 1).collect();
    cut.push('…');
    cut
}

/// Whether a ▶ or Run again click may start a run: the recipe has a command for one test, the
/// tests are in a state that can run, and nothing is busy or asked already.
pub(super) fn can_run(model: &TestsModel) -> bool {
    model
        .view
        .as_ref()
        .is_some_and(|v| v.recipe.as_ref().is_some_and(|r| r.one.is_some()))
        && matches!(
            model.state(),
            Some(TabState::Ready | TabState::Done | TabState::Stale)
        )
        && !model.busy()
        && model.pending.is_none()
}

/// Sends a run of one test, unless the model is busy, or tells why it cannot.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_one(
    pr: &PrRef,
    name: &str,
    tabs: &ReviewTabs,
    model: &Model,
    models: &mut TestsModels,
    asks: &mut Asks,
    toasts: &mut Toasts,
    time: &Time,
) {
    if models
        .0
        .get(pr)
        .is_none_or(|m| m.busy() || m.pending.is_some())
    {
        return;
    }
    if let Some(reason) = read_only_reason(tabs, model, pr) {
        return not_sent(toasts, time, reason);
    }
    let scope = TestScope::One {
        name: name.to_string(),
    };
    asks.send(Ask::RunTests {
        pr: pr.clone(),
        scope: scope.clone(),
    });
    if let Some(m) = models.0.get_mut(pr) {
        m.sent_run(scope);
    }
}

impl TestsModel {
    pub(super) fn select(&mut self, name: &str) {
        if self.selected.as_deref() != Some(name) {
            self.selected = Some(name.to_string());
            self.touch();
        }
    }

    pub(super) fn set_filter(&mut self, filter: TestFilter) {
        if self.filter != filter {
            self.filter = filter;
            self.touch();
        }
    }
}

fn near_of(model: &TestsModel) -> BTreeSet<&str> {
    model
        .view
        .as_ref()
        .map(|v| v.near.iter().map(|n| n.name.as_str()).collect())
        .unwrap_or_default()
}

fn list_view(model: &TestsModel, can_run: bool) -> Option<ListView> {
    let view = model.view.as_ref()?;
    let near = near_of(model);
    let (source, results) = source_results(model).unwrap_or((Source::Nothing, &[]));
    let (mut failed, mut near_count, mut shown) = (0, 0, 0);
    for r in results {
        failed += usize::from(r.status == TestStatus::Failed);
        near_count += usize::from(near.contains(r.name.as_str()));
        shown += usize::from(accepts(r, model.filter, &near));
    }
    let note = match (&source, shown) {
        (Source::Nothing, _) => Some(NO_RUN),
        (_, 0) => Some(NO_MATCH),
        _ => None,
    };
    let mut hasher = DefaultHasher::new();
    view.near.iter().for_each(|n| n.name.hash(&mut hasher));
    Some(ListView {
        head: Head {
            failed,
            near: near_count,
            filter: model.filter,
        },
        shape: Shape {
            source,
            total: results.len(),
            filter: model.filter,
            near_key: hasher.finish(),
            can_run,
            note,
        },
        selected: selected_name(model),
    })
}

type Spawner<'a> = ChildSpawnerCommands<'a>;

pub(super) fn fill_list(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    models: Res<TestsModels>,
    fonts: Res<UiFonts>,
    mut regions: Query<(Entity, &ListRegion, Option<&mut ListState>)>,
    mut bodies: Query<(&mut ScrollPosition, &ComputedNode)>,
) {
    for (entity, region, state) in &mut regions {
        let pr = &region.0;
        if tabs.0.get(pr).and_then(|t| t.ready()).is_none() {
            continue;
        }
        let Some(model) = models.0.get(pr) else {
            continue;
        };
        let Some(view) = list_view(model, can_run(model)) else {
            continue;
        };
        let Some(results) = source_results(model).map(|(_, r)| r).or(Some(&[])) else {
            continue;
        };
        match state {
            Some(mut state) => {
                if state.view == view {
                    continue;
                }
                let (scroll, view_h) = bodies.get(state.body).map_or((0.0, 0.0), |(p, n)| {
                    (p.0.y, n.size().y * n.inverse_scale_factor)
                });
                let reset = refresh(
                    &mut commands,
                    &mut state,
                    view,
                    model,
                    results,
                    (scroll, view_h),
                    &fonts,
                    pr,
                );
                if reset && let Ok((mut pos, _)) = bodies.get_mut(state.body) {
                    pos.0.y = 0.0;
                }
            }
            None => {
                commands.entity(entity).despawn_related::<Children>();
                let mut state = shell(&mut commands, entity);
                refresh(
                    &mut commands,
                    &mut state,
                    view,
                    model,
                    results,
                    (0.0, 0.0),
                    &fonts,
                    pr,
                );
                commands.entity(entity).insert(state);
            }
        }
    }
}

/// The card with its header, divider, note and scrolling body, all empty. The state starts
/// with a view no real view equals, so `refresh` draws everything.
fn shell(commands: &mut Commands, region: Entity) -> ListState {
    let header = commands
        .spawn(Node {
            width: percent(100),
            justify_content: JustifyContent::SpaceBetween,
            align_items: AlignItems::Center,
            flex_wrap: FlexWrap::Wrap,
            row_gap: px(6),
            flex_shrink: 0.0,
            padding: UiRect::axes(px(14), px(10)),
            ..default()
        })
        .id();
    let note = commands
        .spawn(Node {
            flex_shrink: 0.0,
            ..default()
        })
        .id();
    let spacers = [(); 2].map(|_| {
        commands
            .spawn(Node {
                width: percent(100),
                flex_shrink: 0.0,
                height: px(0),
                ..default()
            })
            .id()
    });
    let body = commands
        .spawn((
            Node {
                flex_grow: 1.0,
                flex_basis: px(0),
                min_height: px(0),
                flex_direction: FlexDirection::Column,
                overflow: Overflow::scroll_y(),
                ..default()
            },
            ScrollArea,
            ScrollPosition::default(),
        ))
        .add_children(&spacers)
        .id();
    let divide = commands.spawn(divider()).id();
    let wrap = commands
        .spawn(card(Node {
            flex_grow: 1.0,
            flex_basis: px(0),
            min_height: px(0),
            flex_direction: FlexDirection::Column,
            overflow: Overflow::clip(),
            ..default()
        }))
        .add_children(&[header, divide, note, body])
        .id();
    commands.entity(region).add_child(wrap);
    ListState {
        view: ListView {
            head: Head {
                failed: usize::MAX,
                near: usize::MAX,
                filter: TestFilter::All,
            },
            shape: Shape {
                source: Source::Run(String::new()),
                total: usize::MAX,
                filter: TestFilter::All,
                near_key: 0,
                can_run: false,
                note: None,
            },
            selected: None,
        },
        body,
        header,
        note,
        built: Built {
            order: Vec::new(),
            heights: Vec::new(),
            window: 0..0,
            slots: Vec::new(),
            spacers,
            spaced: [0.0; 2],
            children: spacers.to_vec(),
            passes: 0,
        },
    }
}

/// Brings the state to `new`: the header, the order and the note as far as they changed, then
/// the rows. Says whether the scroll must go back to the top (another filter or source).
#[allow(clippy::too_many_arguments)]
fn refresh(
    commands: &mut Commands,
    state: &mut ListState,
    new: ListView,
    model: &TestsModel,
    results: &[TestCaseResult],
    (mut scroll, view_h): (f32, f32),
    fonts: &UiFonts,
    pr: &PrRef,
) -> bool {
    let old = &state.view;
    let mut reset = false;
    if new.head != old.head {
        commands
            .entity(state.header)
            .despawn_related::<Children>()
            .with_children(|h| draw_header(h, fonts, pr, &new.head));
    }
    if new.shape != old.shape {
        reset = new.shape.filter != old.shape.filter || new.shape.source != old.shape.source;
        let built = &mut state.built;
        if reset {
            for slot in built.slots.drain(..) {
                commands.entity(slot.entity).despawn();
            }
            built.window = 0..0;
            scroll = 0.0;
        }
        built.order = order_of(results, new.shape.filter, &near_of(model));
        built.heights.clear();
        built.heights.resize(built.order.len(), TEST_ROW_HEIGHT);
        if new.shape.note != old.shape.note {
            let note = commands.entity(state.note);
            let mut note = note;
            note.despawn_related::<Children>();
            if let Some(line) = new.shape.note {
                note.with_children(|n| {
                    n.spawn((
                        wrapped_text(fonts, line, Type::MUTED),
                        Node {
                            margin: UiRect::all(px(14)),
                            ..default()
                        },
                    ));
                });
            }
        }
    }
    let selected = new.selected.clone();
    place(
        commands,
        &mut state.built,
        &Placing {
            body: state.body,
            pr,
            results,
            selected: selected.as_deref(),
            can_run: new.shape.can_run,
        },
        scroll,
        view_h,
        fonts,
    );
    state.view = new;
    reset
}

fn draw_header(h: &mut Spawner, fonts: &UiFonts, pr: &PrRef, head: &Head) {
    h.spawn(text(fonts, "Tests", Type::HEADING));
    h.spawn(Node {
        column_gap: px(6),
        row_gap: px(6),
        flex_wrap: FlexWrap::Wrap,
        ..default()
    })
    .with_children(|chips| {
        for (filter, label) in [
            (TestFilter::All, "All".to_string()),
            (TestFilter::Failed, format!("Failed · {}", head.failed)),
            (
                TestFilter::Near,
                format!("Near this change · {}", head.near),
            ),
        ] {
            chips.spawn((
                chip(fonts, &label, head.filter == filter),
                FilterChip {
                    pr: pr.clone(),
                    filter,
                },
                observe(on_filter),
            ));
        }
    });
}

/// What `place` needs besides the rows' bookkeeping.
struct Placing<'a> {
    body: Entity,
    pr: &'a PrRef,
    results: &'a [TestCaseResult],
    selected: Option<&'a str>,
    can_run: bool,
}

/// Spawns the rows the view needs, keeps the ones already drawn, despawns the ones left behind,
/// then puts the children in order between the spacers.
fn place(
    commands: &mut Commands,
    built: &mut Built,
    at: &Placing,
    scroll: f32,
    view_h: f32,
    fonts: &UiFonts,
) {
    let view_h = if view_h > 0.0 {
        view_h
    } else {
        rows::VIEW_GUESS
    };
    let want = rows::page(&built.heights, scroll, view_h, built.window.clone());
    let mut old: HashMap<u32, Slot> = built.slots.drain(..).map(|s| (s.index, s)).collect();
    let mut changed = false;
    let mut slots = Vec::with_capacity(want.len());
    for pos in want.clone() {
        let index = built.order[pos];
        let Some(result) = at.results.get(index as usize) else {
            continue;
        };
        let selected = at.selected == Some(result.name.as_str());
        let key = RowKey::of(result, selected, at.can_run);
        match old.remove(&index) {
            Some(slot) if slot.key == key => slots.push(slot),
            Some(slot) => {
                changed = true;
                commands
                    .entity(slot.entity)
                    .despawn_related::<Children>()
                    .insert(Fill(row_fill(selected)))
                    .with_children(|r| row_content(r, fonts, at.pr, result, key));
                slots.push(Slot { key, ..slot });
            }
            None => {
                changed = true;
                let entity = commands
                    .spawn(row(fonts, at.pr, result, key))
                    .with_children(|r| row_content(r, fonts, at.pr, result, key))
                    .id();
                slots.push(Slot { index, entity, key });
            }
        }
    }
    for slot in old.into_values() {
        changed = true;
        commands.entity(slot.entity).despawn();
    }
    let mut children = Vec::with_capacity(slots.len() + 2);
    children.push(built.spacers[0]);
    children.extend(slots.iter().map(|s| s.entity));
    children.push(built.spacers[1]);
    if children != built.children {
        changed = true;
        commands.entity(at.body).replace_children(&children);
        built.children = children;
    }
    let spaced = [
        rows::top(&built.heights, want.start),
        built.heights[want.end.min(built.heights.len())..]
            .iter()
            .sum(),
    ];
    if spaced != built.spaced {
        changed = true;
        for (spacer, height) in built.spacers.into_iter().zip(spaced) {
            commands
                .entity(spacer)
                .entry::<Node>()
                .and_modify(move |mut n| {
                    n.height = px(height);
                });
        }
        built.spaced = spaced;
    }
    built.slots = slots;
    built.window = want;
    if changed {
        built.passes += 1;
    }
}

fn row_fill(selected: bool) -> Swatch {
    if selected {
        Swatch::Selected
    } else {
        Swatch::Clear
    }
}

fn row(_fonts: &UiFonts, pr: &PrRef, result: &TestCaseResult, key: RowKey) -> impl Bundle {
    (
        Node {
            width: percent(100),
            height: px(TEST_ROW_HEIGHT),
            flex_shrink: 0.0,
            align_items: AlignItems::Center,
            column_gap: px(10),
            padding: UiRect::horizontal(px(14)),
            border: UiRect::bottom(px(1)),
            ..default()
        },
        (WidgetButton, Clickable),
        Hovered::default(),
        TabIndex(0),
        BackgroundColor::default(),
        Fill(row_fill(key.selected)),
        HoverFill(Swatch::Hover),
        BorderColor::default(),
        Stroke(Swatch::Line),
        TestRow {
            pr: pr.clone(),
            name: result.name.clone(),
        },
        observe(on_row),
    )
}

fn row_content(r: &mut Spawner, fonts: &UiFonts, pr: &PrRef, result: &TestCaseResult, key: RowKey) {
    let (glyph, ink) = match key.status {
        TestStatus::Failed => ("×", Swatch::Orange),
        TestStatus::Passed => ("✓", Swatch::Green),
        TestStatus::Skipped => ("–", Swatch::Muted),
    };
    r.spawn(text(fonts, glyph, Type::BODY.ink(ink)));
    // A name wider than the row is cut by the row; selecting it shows it whole in the detail.
    r.spawn((
        Node {
            flex_grow: 1.0,
            min_width: px(0),
            overflow: Overflow::clip(),
            ..default()
        },
        Preview,
    ))
    .with_children(|name| {
        name.spawn((
            text(fonts, cut_name(&result.name), Type::MONO.ink(Swatch::Fg)),
            TextLayout::no_wrap(),
        ));
    });
    r.spawn((
        text(
            fonts,
            key.duration_ms.map(row_duration).unwrap_or_default(),
            Type::MONO.ink(Swatch::Faint),
        ),
        TextLayout::no_wrap(),
    ));
    if key.can_run {
        r.spawn((
            button(fonts, "▶", Variant::Ghost),
            RowRun {
                pr: pr.clone(),
                name: result.name.clone(),
            },
            observe(on_row_run),
        ));
    }
}

/// Places the rows again when the body scrolls or is laid out at another size.
pub(super) fn place_on_scroll(
    mut commands: Commands,
    models: Res<TestsModels>,
    fonts: Res<UiFonts>,
    mut regions: Query<(&ListRegion, &mut ListState)>,
    bodies: Query<
        (&ScrollPosition, &ComputedNode),
        Or<(Changed<ScrollPosition>, Changed<ComputedNode>)>,
    >,
) {
    for (region, mut state) in &mut regions {
        let Ok((pos, node)) = bodies.get(state.body) else {
            continue;
        };
        let Some(model) = models.0.get(&region.0) else {
            continue;
        };
        let Some((source, results)) = source_results(model) else {
            continue;
        };
        // A view the list has not been refreshed to is `fill_list`'s to place.
        if state.view.shape.source != source || state.view.shape.total != results.len() {
            continue;
        }
        let state = &mut *state;
        let selected = state.view.selected.clone();
        place(
            &mut commands,
            &mut state.built,
            &Placing {
                body: state.body,
                pr: &region.0,
                results,
                selected: selected.as_deref(),
                can_run: state.view.shape.can_run,
            },
            pos.0.y,
            node.size().y * node.inverse_scale_factor,
            &fonts,
        );
    }
}

fn on_row(activate: On<Activate>, rows: Query<&TestRow>, mut models: ResMut<TestsModels>) {
    let Ok(TestRow { pr, name }) = rows.get(activate.entity) else {
        return;
    };
    if models
        .0
        .get(pr)
        .is_some_and(|m| m.selected.as_deref() != Some(name))
        && let Some(m) = models.0.get_mut(pr)
    {
        m.select(name);
    }
}

#[allow(clippy::too_many_arguments)]
fn on_row_run(
    activate: On<Activate>,
    buttons: Query<&RowRun>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut models: ResMut<TestsModels>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Ok(RowRun { pr, name }) = buttons.get(activate.entity) else {
        return;
    };
    run_one(
        pr,
        name,
        &tabs,
        &model,
        &mut models,
        &mut asks,
        &mut toasts,
        &time,
    );
}

fn on_filter(activate: On<Activate>, chips: Query<&FilterChip>, mut models: ResMut<TestsModels>) {
    let Ok(FilterChip { pr, filter }) = chips.get(activate.entity) else {
        return;
    };
    if models.0.get(pr).is_some_and(|m| m.filter != *filter)
        && let Some(m) = models.0.get_mut(pr)
    {
        m.set_filter(*filter);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Ask, Connection, Tell, TestsAskKind, TestsTell};
    use crate::fixture;
    use crate::review_state::ReviewSection;
    use crate::screens::review::tests_tab::body::StopButton;
    use crate::testing::{self, NOW};
    use clusia_core::tests::{
        NearTest, NearWhy, RunOutcome, TestRun, TestScope, TestStatus, TestsPhase, TestsView,
        Totals,
    };
    use std::collections::HashMap;

    fn tests_app() -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        testing::set_section(&mut app, &pr, ReviewSection::Tests);
        (app, pr)
    }

    fn case(name: &str, status: TestStatus, ms: u64) -> TestCaseResult {
        TestCaseResult {
            name: name.into(),
            file: None,
            status,
            duration_ms: Some(ms),
            message: None,
        }
    }

    /// A finished run at `head` holding `results`.
    fn run_of(head: &str, results: Vec<TestCaseResult>) -> TestRun {
        let count = |s: TestStatus| results.iter().filter(|r| r.status == s).count() as u32;
        let totals = Totals {
            passed: count(TestStatus::Passed),
            failed: count(TestStatus::Failed),
            skipped: count(TestStatus::Skipped),
        };
        let outcome = if totals.failed > 0 {
            RunOutcome::Failed
        } else {
            RunOutcome::Passed
        };
        let mut run = testing::tests_run(head, NOW - 240, outcome, totals);
        run.id = "r1".into();
        run.exit_code = Some(101);
        run.results = results;
        run
    }

    /// A ready view at the review's head with the demo recipe, holding `runs`.
    fn ready_view(app: &App, pr: &PrRef, runs: Vec<TestRun>) -> TestsView {
        let mut view = testing::tests_view(app, pr, TestsPhase::Ready);
        view.env = Some(testing::tests_env(true));
        view.recipe = Some(testing::tests_recipe());
        view.plan = Some(testing::tests_plan(Vec::new(), true));
        view.listed = Some(412);
        view.runs = runs;
        view
    }

    fn show(results: Vec<TestCaseResult>) -> (App, PrRef) {
        let (mut app, pr) = tests_app();
        let head = testing::tests_view(&app, &pr, TestsPhase::Ready).head;
        let view = ready_view(&app, &pr, vec![run_of(&head, results)]);
        testing::open_tests(&mut app, &pr, view);
        testing::recorded(&mut app);
        (app, pr)
    }

    fn five() -> Vec<TestCaseResult> {
        vec![
            case("store::ok_one", TestStatus::Passed, 1_840),
            case("store::bad", TestStatus::Failed, 210),
            case("store::skipped", TestStatus::Skipped, 0),
            case("store::ok_two", TestStatus::Passed, 9_800),
            case("store::ok_three", TestStatus::Passed, 20),
        ]
    }

    fn region(app: &mut App, pr: &PrRef) -> Entity {
        testing::find::<ListRegion>(app, |r| &r.0 == pr)
    }

    fn state(app: &App, region: Entity) -> &ListState {
        app.world().get::<ListState>(region).expect("a built list")
    }

    /// The names of the drawn rows, in the order they sit in the body.
    fn drawn(app: &mut App, pr: &PrRef) -> Vec<String> {
        let region = region(app, pr);
        let body = state(app, region).body;
        let children: Vec<Entity> = app.world().get::<Children>(body).unwrap().to_vec();
        children
            .into_iter()
            .filter_map(|c| app.world().get::<TestRow>(c).map(|r| r.name.clone()))
            .collect()
    }

    fn row(app: &mut App, name: &str) -> Entity {
        testing::find::<TestRow>(app, |r| r.name == name)
    }

    fn model(app: &App, pr: &PrRef) -> TestsModel {
        let m = &app.world().resource::<TestsModels>().0[pr];
        TestsModel {
            selected: m.selected.clone(),
            filter: m.filter,
            pending: m.pending,
            ..TestsModel::default()
        }
    }

    fn send(app: &mut App, tell: TestsTell) {
        testing::tell(app, Tell::Tests(tell));
    }

    #[test]
    fn the_order_is_failed_then_passed_then_skipped_and_the_model_agrees() {
        let results = five();
        let near: BTreeSet<&str> = ["store::ok_two"].into();
        assert_eq!(order_of(&results, TestFilter::All, &near), [1, 0, 3, 4, 2]);
        assert_eq!(order_of(&results, TestFilter::Failed, &near), [1]);
        assert_eq!(order_of(&results, TestFilter::Near, &near), [3]);
        assert!(order_of(&[], TestFilter::All, &near).is_empty());

        let (mut app, pr) = show(five());
        let models = app.world().resource::<TestsModels>();
        let m = &models.0[&pr];
        let names: Vec<String> = m
            .rows(TestFilter::All)
            .iter()
            .map(|r| r.name.clone())
            .collect();
        assert_eq!(
            names,
            [
                "store::bad",
                "store::ok_one",
                "store::ok_two",
                "store::ok_three",
                "store::skipped"
            ]
        );
        assert_eq!(
            drawn(&mut app, &pr),
            names,
            "the list draws what the model orders"
        );
    }

    #[test]
    fn a_row_shows_its_status_name_and_duration() {
        let (mut app, _) = show(five());
        assert!(testing::shows(&mut app, "store::bad"));
        assert!(testing::shows(&mut app, "0.21 s"));
        assert!(testing::shows(&mut app, "9.80 s"));
        assert!(testing::shows(&mut app, "×"));
        assert!(testing::shows(&mut app, "✓"));
        assert!(testing::shows(&mut app, "–"));
        assert!(testing::shows(&mut app, "Tests"));
        assert!(testing::shows(&mut app, "Failed · 1"));
        assert!(testing::shows(&mut app, "Near this change · 0"));
    }

    #[test]
    fn a_name_that_does_not_fit_is_cut_and_whole_in_the_row() {
        let long = "a".repeat(200);
        let (mut app, _) = show(vec![case(&long, TestStatus::Passed, 5)]);
        let cut = format!("{}…", "a".repeat(NAME_COLUMNS - 1));
        assert!(testing::shows(&mut app, &cut));
        assert!(!testing::shows(&mut app, &long));
        let row = row(&mut app, &long);
        assert_eq!(app.world().get::<TestRow>(row).unwrap().name, long);
        assert_eq!(cut_name("short"), "short");
        assert_eq!(cut_name("a\nb"), "a\\nb");
    }

    #[test]
    fn with_no_run_the_list_says_so() {
        let (mut app, pr) = tests_app();
        let view = ready_view(&app, &pr, Vec::new());
        testing::open_tests(&mut app, &pr, view);
        assert!(testing::shows(
            &mut app,
            "No run yet. Run the tests near this change, or all of them."
        ));
        assert_eq!(testing::count::<TestRow>(&mut app), 0);
    }

    #[test]
    fn a_stale_runs_rows_still_list() {
        let (mut app, pr) = tests_app();
        let view = ready_view(
            &app,
            &pr,
            vec![run_of(
                "0ldhead0000",
                vec![case("store::old", TestStatus::Passed, 9)],
            )],
        );
        testing::open_tests(&mut app, &pr, view);
        assert_eq!(drawn(&mut app, &pr), ["store::old"]);
    }

    #[test]
    fn filters_narrow_the_list_and_scroll_it_to_the_top() {
        let (mut app, pr) = show(five());
        let failed = testing::find::<FilterChip>(&mut app, |c| c.filter == TestFilter::Failed);
        testing::activate(&mut app, failed);
        testing::settle(&mut app);
        assert_eq!(model(&app, &pr).filter, TestFilter::Failed);
        assert_eq!(drawn(&mut app, &pr), ["store::bad"]);
        let all = testing::find::<FilterChip>(&mut app, |c| c.filter == TestFilter::All);
        testing::activate(&mut app, all);
        testing::settle(&mut app);
        assert_eq!(drawn(&mut app, &pr).len(), 5);
        let near = testing::find::<FilterChip>(&mut app, |c| c.filter == TestFilter::Near);
        testing::activate(&mut app, near);
        testing::settle(&mut app);
        assert!(drawn(&mut app, &pr).is_empty());
        assert!(testing::shows(&mut app, "No tests match this filter"));
    }

    #[test]
    fn near_tests_come_from_the_view() {
        let (mut app, pr) = tests_app();
        let head = testing::tests_view(&app, &pr, TestsPhase::Ready).head;
        let mut view = ready_view(&app, &pr, vec![run_of(&head, five())]);
        view.near = vec![NearTest {
            name: "store::ok_two".into(),
            why: NearWhy::File,
        }];
        testing::open_tests(&mut app, &pr, view);
        assert!(testing::shows(&mut app, "Near this change · 1"));
        let near = testing::find::<FilterChip>(&mut app, |c| c.filter == TestFilter::Near);
        testing::activate(&mut app, near);
        testing::settle(&mut app);
        assert_eq!(drawn(&mut app, &pr), ["store::ok_two"]);
    }

    #[test]
    fn clicking_a_row_selects_it_and_nothing_else_happens() {
        let (mut app, pr) = show(five());
        let ok = row(&mut app, "store::ok_two");
        testing::activate(&mut app, ok);
        testing::settle(&mut app);
        assert_eq!(model(&app, &pr).selected.as_deref(), Some("store::ok_two"));
        assert!(testing::recorded(&mut app).is_empty());
        let before = app.world().resource::<TestsModels>().0[&pr].version;
        let again = row(&mut app, "store::ok_two");
        testing::activate(&mut app, again);
        assert_eq!(
            app.world().resource::<TestsModels>().0[&pr].version,
            before,
            "the same row again changes nothing"
        );
    }

    #[test]
    fn the_play_button_runs_only_that_test() {
        let (mut app, pr) = show(five());
        let play = testing::find::<RowRun>(&mut app, |r| r.name == "store::ok_one");
        testing::activate(&mut app, play);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::RunTests {
                pr: pr.clone(),
                scope: TestScope::One {
                    name: "store::ok_one".into()
                }
            }]
        );
        assert_eq!(model(&app, &pr).pending, Some(TestsAskKind::Run));
        // Busy now: the ▶ buttons are gone and a second click cannot happen.
        testing::settle(&mut app);
        assert_eq!(testing::count::<RowRun>(&mut app), 0);
    }

    #[test]
    fn no_play_button_without_a_one_command_or_while_busy() {
        let (mut app, pr) = tests_app();
        let head = testing::tests_view(&app, &pr, TestsPhase::Ready).head;
        let mut view = ready_view(&app, &pr, vec![run_of(&head, five())]);
        view.recipe.as_mut().unwrap().one = None;
        testing::open_tests(&mut app, &pr, view);
        assert_eq!(testing::count::<TestRow>(&mut app), 5);
        assert_eq!(testing::count::<RowRun>(&mut app), 0);

        let (mut app, pr) = tests_app();
        let head = testing::tests_view(&app, &pr, TestsPhase::Ready).head;
        let mut view = ready_view(&app, &pr, vec![run_of(&head, five())]);
        view.phase = TestsPhase::Running { run: "r2".into() };
        testing::open_tests(&mut app, &pr, view);
        assert_eq!(
            testing::count::<RowRun>(&mut app),
            0,
            "not while a run goes"
        );
    }

    #[test]
    fn a_play_click_without_a_connection_is_refused() {
        let (mut app, pr) = show(five());
        app.world_mut()
            .resource_mut::<crate::bridge::Model>()
            .connection = Connection::Lost("gone".into());
        let play = testing::find::<RowRun>(&mut app, |r| r.name == "store::ok_one");
        testing::activate(&mut app, play);
        assert!(testing::recorded(&mut app).is_empty());
        assert_eq!(model(&app, &pr).pending, None);
        let toasts = &app.world().resource::<crate::bridge::Toasts>().0;
        assert!(
            toasts
                .iter()
                .any(|t| t.warning && t.text.contains("Not connected"))
        );
    }

    fn big(n: usize) -> Vec<TestCaseResult> {
        (0..n)
            .map(|i| {
                let status = if i % 2_500 == 7 {
                    TestStatus::Failed
                } else {
                    TestStatus::Passed
                };
                case(&format!("suite::case_{i:05}"), status, 3)
            })
            .collect()
    }

    /// The most rows a window may hold for the view guess: what the view needs, `LOW` each
    /// side, then the reach ahead a steady scroll gets.
    fn window_bound() -> usize {
        ((rows::VIEW_GUESS + rows::LOW + rows::AHEAD) / TEST_ROW_HEIGHT).ceil() as usize + 2
    }

    #[test]
    fn ten_thousand_results_spawn_a_window() {
        let (mut app, pr) = show(big(10_000));
        let rows_drawn = testing::count::<TestRow>(&mut app);
        assert!(rows_drawn >= 20, "{rows_drawn} rows");
        assert!(rows_drawn <= window_bound(), "{rows_drawn} rows");
        let names = drawn(&mut app, &pr);
        assert_eq!(names[0], "suite::case_00007", "the failures come first");
        assert_eq!(names[1], "suite::case_02507");
        assert_eq!(names[2], "suite::case_05007");
        assert_eq!(names[3], "suite::case_07507");
        assert_eq!(names[4], "suite::case_00000");
    }

    #[test]
    fn scrolling_moves_the_window_and_keeps_it_small() {
        let (mut app, pr) = show(big(10_000));
        let region = region(&mut app, &pr);
        let body = state(&app, region).body;
        // Row 3,225 of the order (four failures, then the passes in order).
        let y = 3_225.0 * TEST_ROW_HEIGHT;
        app.world_mut()
            .entity_mut(body)
            .get_mut::<ScrollPosition>()
            .unwrap()
            .0
            .y = y;
        testing::settle(&mut app);
        let names = drawn(&mut app, &pr);
        assert!(names.len() <= window_bound(), "{} rows", names.len());
        assert!(
            names.contains(&"suite::case_03221".to_string()),
            "{:?}",
            &names[..3]
        );
        assert!(!names.contains(&"suite::case_00000".to_string()));
        assert_eq!(testing::count::<TestRow>(&mut app), names.len());
    }

    fn running(app: &mut App, pr: &PrRef, results: usize) {
        let mut view = ready_view(app, pr, Vec::new());
        view.phase = TestsPhase::Running { run: "r9".into() };
        testing::open_tests(app, pr, view);
        send(
            app,
            TestsTell::RunStarted {
                pr: pr.clone(),
                run: "r9".into(),
            },
        );
        for i in 0..results {
            send(
                app,
                TestsTell::Result {
                    pr: pr.clone(),
                    run: "r9".into(),
                    result: case(&format!("live::t{i:03}"), TestStatus::Passed, 4),
                },
            );
        }
    }

    #[test]
    fn live_results_list_while_a_run_goes() {
        let (mut app, pr) = tests_app();
        running(&mut app, &pr, 3);
        assert_eq!(
            drawn(&mut app, &pr),
            ["live::t000", "live::t001", "live::t002"]
        );
    }

    #[test]
    fn a_result_arriving_respawns_no_drawn_row() {
        let (mut app, pr) = tests_app();
        // Few enough to sit inside the view guess, so every result has its row.
        running(&mut app, &pr, 20);
        let before = testing::entities::<TestRow>(&mut app);
        assert_eq!(before.len(), 20);
        send(
            &mut app,
            TestsTell::Result {
                pr: pr.clone(),
                run: "r9".into(),
                result: case("live::t020", TestStatus::Passed, 4),
            },
        );
        let after = testing::entities::<TestRow>(&mut app);
        assert_eq!(after.len(), 21);
        for e in &before {
            assert!(after.contains(e), "a drawn row was respawned");
        }
    }

    #[test]
    fn a_failed_result_arriving_goes_first_without_respawning_the_rest() {
        let (mut app, pr) = tests_app();
        running(&mut app, &pr, 10);
        let before = testing::entities::<TestRow>(&mut app);
        send(
            &mut app,
            TestsTell::Result {
                pr: pr.clone(),
                run: "r9".into(),
                result: case("live::broken", TestStatus::Failed, 4),
            },
        );
        let names = drawn(&mut app, &pr);
        assert_eq!(names[0], "live::broken");
        assert_eq!(names[1], "live::t000");
        let after = testing::entities::<TestRow>(&mut app);
        for e in &before {
            assert!(after.contains(e), "a drawn row was respawned");
        }
    }

    #[test]
    fn a_settled_tab_rebuilds_nothing() {
        let (mut app, pr) = show(big(2_000));
        testing::settle(&mut app);
        let region = region(&mut app, &pr);
        let rows_before = testing::entities::<TestRow>(&mut app);
        let all_before = app.world_mut().query::<Entity>().iter(app.world()).count();
        let passes = state(&app, region).built.passes;
        for _ in 0..12 {
            app.update();
        }
        assert_eq!(testing::entities::<TestRow>(&mut app), rows_before);
        assert_eq!(
            app.world_mut().query::<Entity>().iter(app.world()).count(),
            all_before
        );
        assert_eq!(state(&app, region).built.passes, passes);
    }

    #[test]
    fn names_in_the_list_are_plain_text() {
        let nasty = "[x](https://evil.example)\u{202e}\n\u{1b}[2J **b**";
        let (mut app, _pr) = show(vec![case(nasty, TestStatus::Failed, 3)]);
        assert!(testing::shows(
            &mut app,
            "[x](https://evil.example)\\u{202e}\\n\\u{1b}[2J **b**"
        ));
        assert_eq!(testing::count::<crate::ui::markdown::MdBody>(&mut app), 0);
        assert_eq!(testing::count::<crate::ui::markdown::MdLink>(&mut app), 0);
        let nasty_row = row(&mut app, nasty);
        testing::activate(&mut app, nasty_row);
        assert!(testing::recorded(&mut app).is_empty());
        assert!(
            app.world()
                .get_resource::<crate::platform_open::OpenUrls>()
                .is_none_or(|u| u.0.is_empty())
        );
    }

    #[test]
    fn a_full_window_keeps_its_rows_when_results_arrive() {
        let (mut app, pr) = tests_app();
        running(&mut app, &pr, 70);
        let first = drawn(&mut app, &pr);
        assert!(first.len() < 70, "the window is full: {} rows", first.len());
        let by_name = |app: &mut App| -> HashMap<String, Entity> {
            testing::entities::<TestRow>(app)
                .into_iter()
                .map(|e| (app.world().get::<TestRow>(e).unwrap().name.clone(), e))
                .collect()
        };
        let before = by_name(&mut app);

        send(
            &mut app,
            TestsTell::Result {
                pr: pr.clone(),
                run: "r9".into(),
                result: case("live::broken", TestStatus::Failed, 4),
            },
        );
        let names = drawn(&mut app, &pr);
        let after = by_name(&mut app);
        assert_eq!(names[0], "live::broken");
        assert_eq!(names.len(), first.len(), "the window keeps its size");
        for (name, entity) in &after {
            if name != "live::broken" {
                assert_eq!(before[name], *entity, "{name} was respawned");
            }
        }
        let gone: Vec<&String> = before.keys().filter(|n| !after.contains_key(*n)).collect();
        assert_eq!(gone, [first.last().unwrap()], "only the last row left");
        assert_eq!(
            testing::count::<TestRow>(&mut app),
            names.len(),
            "none left behind"
        );

        send(
            &mut app,
            TestsTell::Result {
                pr: pr.clone(),
                run: "r9".into(),
                result: case("live::late", TestStatus::Passed, 4),
            },
        );
        assert_eq!(
            by_name(&mut app),
            after,
            "a result past the window changes no row"
        );
    }

    #[test]
    fn stop_shows_while_running() {
        let (mut app, pr) = tests_app();
        running(&mut app, &pr, 2);
        testing::recorded(&mut app);
        let stop = testing::find::<StopButton>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, stop);
        assert_eq!(testing::recorded(&mut app), [Ask::StopTests(pr.clone())]);
    }
}
