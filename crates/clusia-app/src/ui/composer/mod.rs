//! The composer: the one place where a comment is written, wherever it goes (a line in the
//! diff, a reply, a general note, a draft item, the review summary).
//!
//! It draws a toolbar (Write / Preview, formatting buttons), the text area, the rendered
//! preview and a chip for every image or GIF in the text; the caller draws the frame and the
//! footer around it. The text area is spawned once and stays: switching to Preview only hides
//! it, so what was typed, the cursor and the caller's markers on the area survive any switch
//! and any rebuild of the region around it. Everything the composer shows is derived from the
//! area's text, so nothing is stored twice.

use std::ops::Range;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input::ButtonInput;
use bevy::input_focus::{FocusCause, InputFocus};
use bevy::prelude::*;
use bevy::text::{EditableText, FontCx, LayoutCx};
use bevy::ui_widgets::{Activate, observe};
use bevy::window::{FileDragAndDrop, RequestRedraw};
use clusia_core::{FileDiff, PrRef, Side};
use clusia_view::diff::{RowKind, parse_patch};

use crate::bridge::{Model, Toast, Toasts};
use crate::fonts::UiFonts;
use crate::review_state::{EditTarget, Phase, ReviewTabs};
use crate::theme::{Swatch, Theme};
use crate::ui::kit::{Fill, HoverFill, Ink, Stroke, Type, panel, segment, segments, text};
use crate::ui::markdown::MdImage;
use crate::ui::markdown::build::{RenderOpts, is_external, markdown};
use crate::ui::markdown::parse::parse;
use crate::ui::text_area::{Caret, TextArea, TextSubmitted, growing_text_area, text_area};

mod emoji_picker;
mod gif_picker;
pub mod popover;
mod reveal;
pub mod toolbar;

#[cfg(test)]
pub(crate) mod testkit;

use toolbar::{
    Chip, ToolbarAction, ToolbarFor, action_buttons, chips_in, cursor_after_removal,
    insert_suggestion, remove_chip, tool_button,
};

/// Write shows the text area, Preview the rendered comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ComposerMode {
    #[default]
    Write,
    Preview,
}

/// What a composer writes: an editor target, or one of the Finalize fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Slot {
    Edit(EditTarget),
    FinalizeItem(String),
    FinalizeSummary,
}

/// Names one composer: the review it belongs to and what it writes.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ComposerKey(pub PrRef, pub Slot);

/// How tall the text area is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AreaSize {
    Lines(f32),
    Grow { min: f32, max: f32 },
}

/// What `composer` needs to draw.
#[derive(Debug, Clone, Copy)]
pub struct ComposerView<'a> {
    pub text: &'a str,
    /// The mode to start in; `sync_modes` keeps it right afterwards.
    pub mode: ComposerMode,
    /// The `TextArea::id` of the text area.
    pub id: u64,
    pub size: AreaSize,
    /// A narrow toolbar: no list, quote or suggestion buttons and no hint.
    pub compact: bool,
    /// Offers **±** (a comment on a line of the head side).
    pub suggest: bool,
    /// The text area has no border of its own (the caller's frame is the border).
    pub flat: bool,
}

/// The root of one composer.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct Composer(pub ComposerKey);

/// The text area of a composer.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ComposerArea(pub ComposerKey);

/// The Write / Preview buttons.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ComposerModeButton {
    pub key: ComposerKey,
    pub mode: ComposerMode,
}

/// The rendered comment of a composer in Preview.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct PreviewBody {
    pub key: ComposerKey,
    rendered: Option<String>,
    /// The text as of the previous frame.
    seen: Option<String>,
    pending: Option<f64>,
}

/// The chips under the text.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ChipRow {
    pub key: ComposerKey,
    built: Option<Vec<Chip>>,
}

/// The × of a chip: removes `raw` from the text.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ChipRemove {
    pub key: ComposerKey,
    pub raw: String,
}

/// Modes of composers that are not an editor (Finalize items and summary).
#[derive(Resource, Debug, Default)]
pub struct ExtraModes(Vec<(ComposerKey, ComposerMode)>);

impl ExtraModes {
    /// Forgets every mode of `pr`'s Finalize fields.
    pub fn forget(&mut self, pr: &PrRef) {
        self.0.retain(|(k, _)| &k.0 != pr);
    }
}

/// How long a changed text waits before the preview is drawn again.
pub const PREVIEW_DEBOUNCE: f64 = 0.12;

/// How long the image-file hint stays.
const HINT_SECS: f64 = 10.0;

/// Shown when an image file is dropped on the window: images go in by link only.
pub const IMAGE_FILE_HINT: &str = "GitHub only accepts image links. Upload the file somewhere first and paste its URL. Dragging it into any GitHub comment box in the browser gives you a link to copy.";

/// The mode of `key`: an editor's `mode`, or what `ExtraModes` holds.
pub fn mode_of(key: &ComposerKey, tabs: &ReviewTabs, extra: &ExtraModes) -> ComposerMode {
    match &key.1 {
        Slot::Edit(target) => tabs
            .0
            .get(&key.0)
            .and_then(|t| t.ui.editor.as_ref())
            .filter(|e| &e.target == target)
            .map_or(ComposerMode::Write, |e| e.mode),
        _ => extra
            .0
            .iter()
            .find(|(k, _)| k == key)
            .map_or(ComposerMode::Write, |(_, m)| *m),
    }
}

pub(crate) fn set_mode(
    key: &ComposerKey,
    mode: ComposerMode,
    tabs: &mut ReviewTabs,
    extra: &mut ExtraModes,
) {
    match &key.1 {
        Slot::Edit(target) => {
            if let Some(editor) = tabs
                .0
                .get_mut(&key.0)
                .and_then(|t| t.ui.editor.as_mut())
                .filter(|e| &e.target == target)
                && editor.mode != mode
            {
                editor.mode = mode;
            }
        }
        _ => match extra.0.iter_mut().find(|(k, _)| k == key) {
            Some((_, m)) => *m = mode,
            None => extra.0.push((key.clone(), mode)),
        },
    }
}

/// The text of an area and its selection (clamped into the text).
pub fn read_area(editable: &EditableText) -> (String, Range<usize>) {
    let text = editable.value().to_string();
    let sel = editable.editor().raw_selection().text_range();
    let end = sel.end.min(text.len());
    (text, sel.start.min(end)..end)
}

/// Replaces the text of an area and selects `sel`.
pub fn write_area(
    editable: &mut EditableText,
    fonts: &mut FontCx,
    layout: &mut LayoutCx,
    text: &str,
    sel: Range<usize>,
) {
    let editor = editable.editor_mut();
    editor.set_text(text);
    editor
        .driver(&mut fonts.context, &mut layout.0)
        .select_byte_range(sel.start, sel.end);
}

/// The head-side lines a comment on a line (or a range of lines) is about, for **±**.
pub fn suggestion_source(
    files: &[FileDiff],
    path: &str,
    start: Option<u32>,
    line: u32,
) -> Option<String> {
    let patch = files.iter().find(|f| f.path == path)?.patch.as_deref()?;
    let from = start.unwrap_or(line).min(line);
    let lines: Vec<String> = parse_patch(patch)
        .into_iter()
        .filter(|r| matches!(r.kind, RowKind::Added | RowKind::Context))
        .filter(|r| r.new.is_some_and(|n| (from..=line).contains(&n)))
        .map(|r| r.text)
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The lines **±** would suggest for `key`, when it is a comment on the head side.
fn source_for(tabs: &ReviewTabs, key: &ComposerKey) -> Option<String> {
    let Slot::Edit(EditTarget::Line {
        path,
        side: Side::Right,
        start,
        line,
    }) = &key.1
    else {
        return None;
    };
    let Phase::Ready(ready) = &tabs.0.get(&key.0)?.phase else {
        return None;
    };
    suggestion_source(&ready.view.diff, path, *start, *line)
}

/// Whether to draw the preview now, and when the text last changed (the debounce clock).
///
/// The first drawing is immediate; later changes wait for `PREVIEW_DEBOUNCE` of quiet, so every
/// change since the previous frame (`seen`) starts the wait over.
pub fn preview_due(
    rendered: Option<&str>,
    text: &str,
    seen: Option<&str>,
    pending: Option<f64>,
    now: f64,
) -> (bool, Option<f64>) {
    match (rendered, pending) {
        (Some(r), _) if r == text => (false, None),
        (None, _) => (true, None),
        (Some(_), _) if seen != Some(text) => (false, Some(now)),
        (Some(_), None) => (false, Some(now)),
        (Some(_), Some(since)) if now - since >= PREVIEW_DEBOUNCE => (true, None),
        (Some(_), Some(since)) => (false, Some(since)),
    }
}

/// Draws a composer into `p`. `area` goes on the text area, once (the caller's markers).
pub fn composer<B: Bundle>(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    view: &ComposerView,
    key: ComposerKey,
    area: B,
) {
    p.spawn((
        Node {
            flex_direction: FlexDirection::Column,
            flex_grow: 1.0,
            min_width: px(0),
            width: percent(100),
            ..default()
        },
        Composer(key.clone()),
    ))
    .with_children(|c| {
        toolbar_row(c, fonts, view, &key);
        let previewing = view.mode == ComposerMode::Preview;
        let mut area_entity = match view.size {
            AreaSize::Lines(lines) => c.spawn((
                text_area(fonts, view.text, lines, view.id),
                ComposerArea(key.clone()),
                area,
            )),
            AreaSize::Grow { min, max } => c.spawn((
                growing_text_area(fonts, view.text, min, max, caret(&key.1), view.id),
                ComposerArea(key.clone()),
                area,
            )),
        };
        if view.flat {
            area_entity.insert((Stroke(Swatch::Clear), Fill(Swatch::Clear)));
        }
        if previewing {
            area_entity
                .entry::<Node>()
                .and_modify(|mut n| n.display = Display::None);
        }
        c.spawn((
            Node {
                display: if previewing {
                    Display::Flex
                } else {
                    Display::None
                },
                flex_direction: FlexDirection::Column,
                row_gap: px(6),
                padding: UiRect::axes(px(12), px(10)),
                min_height: px(80),
                ..default()
            },
            PreviewBody {
                key: key.clone(),
                rendered: None,
                seen: None,
                pending: None,
            },
        ));
        c.spawn((
            Node {
                flex_wrap: FlexWrap::Wrap,
                column_gap: px(8),
                row_gap: px(8),
                padding: UiRect::horizontal(px(8)),
                ..default()
            },
            ChipRow { key, built: None },
        ));
    });
}

/// An editor goes on from where its text ends; a Finalize field is read from its beginning.
fn caret(slot: &Slot) -> Caret {
    match slot {
        Slot::Edit(_) => Caret::End,
        Slot::FinalizeItem(_) | Slot::FinalizeSummary => Caret::Start,
    }
}

fn toolbar_row(
    c: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    view: &ComposerView,
    key: &ComposerKey,
) {
    c.spawn((
        Node {
            flex_wrap: FlexWrap::Wrap,
            align_items: AlignItems::Center,
            column_gap: px(4),
            row_gap: px(4),
            padding: UiRect::axes(px(8), px(6)),
            border: UiRect::bottom(px(1)),
            ..default()
        },
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|t| {
        t.spawn(segments()).with_children(|s| {
            for (label, mode) in [
                ("Write", ComposerMode::Write),
                ("Preview", ComposerMode::Preview),
            ] {
                s.spawn((
                    segment(fonts, label, mode == view.mode),
                    ComposerModeButton {
                        key: key.clone(),
                        mode,
                    },
                    observe(on_mode_button),
                ));
            }
        });
        t.spawn(Node {
            width: px(1),
            height: px(18),
            margin: UiRect::horizontal(px(6)),
            ..default()
        })
        .insert((BackgroundColor::default(), Fill(Swatch::Line)));
        action_buttons(t, fonts, key, view.compact, view.suggest);
        t.spawn(Node {
            width: px(1),
            height: px(18),
            margin: UiRect::horizontal(px(6)),
            ..default()
        })
        .insert((BackgroundColor::default(), Fill(Swatch::Line)));
        popover::popover_buttons(t, fonts, key);
        if !view.compact {
            t.spawn(Node {
                flex_grow: 1.0,
                ..default()
            });
            t.spawn(text(fonts, "Markdown works · images by link", Type::META));
        }
    });
}

pub struct ComposerPlugin;

impl Plugin for ComposerPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            popover::PopoverPlugin,
            reveal::RevealPlugin,
            emoji_picker::EmojiPickerPlugin,
            gif_picker::GifPickerPlugin,
        ))
        .init_resource::<ExtraModes>()
        .init_resource::<FontCx>()
        .init_resource::<LayoutCx>()
        .add_message::<TextSubmitted>()
        .add_message::<FileDragAndDrop>()
        .add_systems(
            Update,
            (
                sync_modes,
                style_mode_buttons,
                fill_previews,
                fill_chips,
                submit_from_preview,
                hint_dropped_files,
            )
                .chain(),
        );
    }
}

fn on_mode_button(
    activate: On<Activate>,
    buttons: Query<&ComposerModeButton>,
    areas: Query<(Entity, &ComposerArea)>,
    mut tabs: ResMut<ReviewTabs>,
    mut extra: ResMut<ExtraModes>,
    mut focus: ResMut<InputFocus>,
) {
    let Ok(button) = buttons.get(activate.entity) else {
        return;
    };
    set_mode(&button.key, button.mode, &mut tabs, &mut extra);
    let area = areas
        .iter()
        .find(|(_, a)| a.0 == button.key)
        .map(|(e, _)| e);
    match (button.mode, area) {
        (ComposerMode::Write, Some(area)) => focus.set(area, FocusCause::Navigated),
        (ComposerMode::Preview, Some(area)) if focus.get() == Some(area) => focus.clear(),
        _ => {}
    }
}

pub(crate) fn on_toolbar_button(
    activate: On<Activate>,
    buttons: Query<(&ToolbarAction, &ToolbarFor)>,
    mut areas: Query<(Entity, &ComposerArea, &mut EditableText)>,
    mut tabs: ResMut<ReviewTabs>,
    mut extra: ResMut<ExtraModes>,
    mut fonts: ResMut<FontCx>,
    mut layout: ResMut<LayoutCx>,
    mut focus: ResMut<InputFocus>,
) {
    let Ok((action, ToolbarFor(key))) = buttons.get(activate.entity) else {
        return;
    };
    let source = source_for(&tabs, key);
    set_mode(key, ComposerMode::Write, &mut tabs, &mut extra);
    let Some((area, _, mut editable)) = areas.iter_mut().find(|(_, a, _)| &a.0 == key) else {
        return;
    };
    let (text, sel) = read_area(&editable);
    let (new, sel) = match (action, source) {
        (ToolbarAction::Suggest, Some(source)) if sel.is_empty() => {
            insert_suggestion(&text, sel, &source)
        }
        _ => toolbar::apply(*action, &text, sel),
    };
    write_area(&mut editable, &mut fonts, &mut layout, &new, sel);
    focus.set(area, FocusCause::Navigated);
}

/// Shows the text area or the preview, whichever the mode says.
fn sync_modes(
    tabs: Res<ReviewTabs>,
    extra: Res<ExtraModes>,
    mut areas: Query<(&ComposerArea, &mut Node), Without<PreviewBody>>,
    mut previews: Query<(&PreviewBody, &mut Node), Without<ComposerArea>>,
) {
    let show = |on: bool| if on { Display::Flex } else { Display::None };
    for (ComposerArea(key), mut node) in &mut areas {
        let want = show(mode_of(key, &tabs, &extra) == ComposerMode::Write);
        if node.display != want {
            node.display = want;
        }
    }
    for (PreviewBody { key, .. }, mut node) in &mut previews {
        let want = show(mode_of(key, &tabs, &extra) == ComposerMode::Preview);
        if node.display != want {
            node.display = want;
        }
    }
}

/// The selected mode button looks selected.
fn style_mode_buttons(
    tabs: Res<ReviewTabs>,
    extra: Res<ExtraModes>,
    mut buttons: Query<(&ComposerModeButton, &mut Fill, &mut HoverFill, &Children)>,
    mut labels: Query<(&mut Ink, &mut TextFont)>,
) {
    for (button, mut fill, mut hover, children) in &mut buttons {
        let on = mode_of(&button.key, &tabs, &extra) == button.mode;
        let (want_fill, want_hover) = if on {
            (Swatch::Raised, Swatch::Raised)
        } else {
            (Swatch::Clear, Swatch::Hover)
        };
        if fill.0 != want_fill {
            fill.0 = want_fill;
        }
        if hover.0 != want_hover {
            hover.0 = want_hover;
        }
        for child in children {
            if let Ok((mut ink, mut face)) = labels.get_mut(*child) {
                let (want_ink, weight) = if on {
                    (Swatch::Fg, 600)
                } else {
                    (Swatch::Muted, 400)
                };
                if ink.0 != want_ink {
                    ink.0 = want_ink;
                }
                if face.weight.0 != weight {
                    face.weight = FontWeight(weight);
                }
            }
        }
    }
}

/// Draws the comment of each composer in Preview with the shared markdown renderer; a changed
/// text is drawn again after `PREVIEW_DEBOUNCE`.
#[allow(clippy::too_many_arguments)]
fn fill_previews(
    mut commands: Commands,
    time: Res<Time>,
    fonts: Res<UiFonts>,
    model: Res<Model>,
    theme: Res<Theme>,
    tabs: Res<ReviewTabs>,
    extra: Res<ExtraModes>,
    mut previews: Query<(Entity, &mut PreviewBody)>,
    areas: Query<(&ComposerArea, &EditableText)>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let now = time.elapsed_secs_f64();
    for (entity, mut body) in &mut previews {
        if mode_of(&body.key, &tabs, &extra) != ComposerMode::Preview {
            if body.rendered.is_some() || body.seen.is_some() {
                body.rendered = None;
                body.seen = None;
                body.pending = None;
            }
            continue;
        }
        let Some((_, editable)) = areas.iter().find(|(a, _)| a.0 == body.key) else {
            continue;
        };
        let text = editable.value().to_string();
        let (draw, pending) = preview_due(
            body.rendered.as_deref(),
            &text,
            body.seen.as_deref(),
            body.pending,
            now,
        );
        if body.seen.as_deref() != Some(&text) {
            body.seen = Some(text.clone());
        }
        if body.pending != pending {
            body.pending = pending;
        }
        if pending.is_some() {
            redraw.write(RequestRedraw);
        }
        if !draw {
            continue;
        }
        let opts = RenderOpts {
            suggestion_base: source_for(&tabs, &body.key),
            ..RenderOpts::from_config(theme.code_size, &model.snapshot.config)
        };
        let blocks = parse(&text);
        commands
            .entity(entity)
            .despawn_related::<Children>()
            .with_children(|p| {
                if text.trim().is_empty() {
                    p.spawn(text_node(&fonts, "Nothing to preview"));
                } else {
                    markdown(p, &fonts, &blocks, &opts);
                }
            });
        body.rendered = Some(text);
    }
}

fn text_node(fonts: &UiFonts, s: &str) -> impl Bundle {
    text(fonts, s.to_string(), Type::MUTED)
}

/// Keeps each composer's chips in step with the images and GIFs in its text.
fn fill_chips(
    mut commands: Commands,
    fonts: Res<UiFonts>,
    mut rows: Query<(Entity, &mut ChipRow)>,
    areas: Query<(&ComposerArea, &EditableText)>,
) {
    for (entity, mut row) in &mut rows {
        let Some((_, editable)) = areas.iter().find(|(a, _)| a.0 == row.key) else {
            continue;
        };
        let chips = chips_in(&editable.value().to_string());
        if row.built.as_ref() == Some(&chips) {
            continue;
        }
        let key = row.key.clone();
        commands
            .entity(entity)
            .despawn_related::<Children>()
            .with_children(|p| {
                for chip in &chips {
                    chip_node(p, &fonts, &key, chip);
                }
            });
        let padding = if chips.is_empty() { px(0) } else { px(8) };
        commands
            .entity(entity)
            .entry::<Node>()
            .and_modify(move |mut n| {
                n.padding = UiRect::axes(px(8), padding);
            });
        row.built = Some(chips);
    }
}

fn chip_node(p: &mut ChildSpawnerCommands, fonts: &UiFonts, key: &ComposerKey, chip: &Chip) {
    let known_host = !is_external(&chip.url);
    p.spawn((
        panel(
            Node {
                align_items: AlignItems::Center,
                column_gap: px(8),
                padding: px(4).all(),
                border: px(1).all(),
                border_radius: BorderRadius::all(px(8)),
                ..default()
            },
            Swatch::Surface,
        ),
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|c| {
        let mut thumb = c.spawn((
            Node {
                width: px(36),
                height: px(36),
                border_radius: BorderRadius::all(px(5)),
                overflow: Overflow::clip(),
                ..default()
            },
            BackgroundColor::default(),
            Fill(Swatch::Chrome),
        ));
        if known_host {
            thumb.with_children(|t| {
                t.spawn((
                    Node {
                        width: px(36),
                        height: px(36),
                        ..default()
                    },
                    MdImage(chip.url.clone()),
                ));
            });
        }
        c.spawn(Node {
            flex_direction: FlexDirection::Column,
            ..default()
        })
        .with_children(|l| {
            l.spawn(text(fonts, chip.label(), Type::STRONG.size(12.0)));
            let note = if chip.on_github() {
                Swatch::Green
            } else {
                Swatch::Faint
            };
            l.spawn(text(fonts, chip.note(), Type::META.ink(note)));
        });
        c.spawn((
            tool_button(fonts, "×", false, false),
            ChipRemove {
                key: key.clone(),
                raw: chip.raw.clone(),
            },
            observe(on_chip_remove),
        ));
    });
}

fn on_chip_remove(
    activate: On<Activate>,
    buttons: Query<&ChipRemove>,
    mut areas: Query<(&ComposerArea, &mut EditableText)>,
    mut fonts: ResMut<FontCx>,
    mut layout: ResMut<LayoutCx>,
) {
    let Ok(button) = buttons.get(activate.entity) else {
        return;
    };
    if let Some((_, mut editable)) = areas.iter_mut().find(|(a, _)| a.0 == button.key) {
        let (text, sel) = read_area(&editable);
        let new = remove_chip(&text, &button.raw);
        let at = cursor_after_removal(&text, &new, sel.start);
        write_area(&mut editable, &mut fonts, &mut layout, &new, at..at);
    }
}

/// ⌘↵ while a composer shows its Preview submits it, as it does from the text area.
fn submit_from_preview(
    keys: Res<ButtonInput<KeyCode>>,
    focus: Res<InputFocus>,
    tabs: Res<ReviewTabs>,
    extra: Res<ExtraModes>,
    composers: Query<(Entity, &Composer)>,
    parents: Query<&ChildOf>,
    areas: Query<(Entity, &ComposerArea, &TextArea, &EditableText)>,
    mut out: MessageWriter<TextSubmitted>,
) {
    let cmd = keys.any_pressed([KeyCode::SuperLeft, KeyCode::SuperRight]);
    if !(cmd && keys.just_pressed(KeyCode::Enter)) {
        return;
    }
    let previewing: Vec<(Entity, &ComposerKey)> = composers
        .iter()
        .filter(|(_, c)| mode_of(&c.0, &tabs, &extra) == ComposerMode::Preview)
        .map(|(e, c)| (e, &c.0))
        .collect();
    let modal_open = tabs.0.values().any(|t| t.ui.modal.is_some());
    let focused = focus
        .get()
        .and_then(|f| parents.iter_ancestors(f).find(|a| composers.contains(*a)));
    let chosen = match (focused, previewing.as_slice()) {
        (Some(root), _) => previewing.iter().find(|(e, _)| *e == root),
        (None, [only]) if !modal_open => Some(only),
        _ => None,
    };
    let Some((_, key)) = chosen else { return };
    if let Some((entity, _, area, editable)) = areas.iter().find(|(_, a, _, _)| &a.0 == *key)
        && focus.get() != Some(entity)
    {
        out.write(TextSubmitted {
            entity,
            id: area.id,
            value: editable.value().to_string(),
        });
    }
}

/// An image file dropped on the window: say how images get into a comment.
fn hint_dropped_files(
    mut dropped: MessageReader<FileDragAndDrop>,
    composers: Query<(), With<Composer>>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    for event in dropped.read() {
        if matches!(event, FileDragAndDrop::DroppedFile { .. }) && !composers.is_empty() {
            toasts.0.push(Toast {
                text: IMAGE_FILE_HINT.to_string(),
                warning: false,
                until: time.elapsed_secs_f64() + HINT_SECS,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;
    use crate::bridge::Ask;
    use crate::fixture;
    use crate::review_state::Editor;
    use crate::testing::{self, NOW};
    use bevy::time::TimeUpdateStrategy;
    use clusia_core::DraftKind;
    use std::time::Duration;

    #[test]
    fn an_opening_editor_types_after_its_text() {
        let mut app = testing::app(fixture::demo(NOW));
        let (_, area) = open(&mut app, EditTarget::General, "Looks");
        testing::type_into(&mut app, area, " good");
        assert_eq!(value(&app, area), "Looks good");
    }

    #[test]
    fn preview_round_trip_keeps_the_text() {
        let mut app = testing::app(fixture::demo(NOW));
        let (pr, area) = open(&mut app, EditTarget::General, "");
        let typed = "**slow** :turtle:\nsecond line";
        testing::type_into(&mut app, area, typed);
        let preview = mode_button(&mut app, ComposerMode::Preview);
        testing::activate(&mut app, preview);
        testing::settle(&mut app);
        assert_eq!(editor_of(&app, &pr).mode, ComposerMode::Preview);
        assert_eq!(
            display(&app, area),
            Display::None,
            "the area is hidden, not removed"
        );
        let body = testing::find::<PreviewBody>(&mut app, |_| true);
        let drawn = app.world().get::<Children>(body).map_or(0, |c| c.len());
        assert!(drawn > 0, "the preview was drawn");
        assert_eq!(value(&app, area), typed);
        // Something around the composer changes and the region is drawn again.
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .editor
            .as_mut()
            .unwrap()
            .error = Some("The daemon said no".into());
        testing::settle(&mut app);
        let area = testing::find::<ComposerArea>(&mut app, |_| true);
        assert_eq!(
            value(&app, area),
            typed,
            "a redrawn composer keeps the text"
        );
        assert_eq!(display(&app, area), Display::None, "and its mode");
        let write = mode_button(&mut app, ComposerMode::Write);
        testing::activate(&mut app, write);
        testing::settle(&mut app);
        assert_eq!(display(&app, area), Display::Flex);
        assert_eq!(value(&app, area), typed);
        assert_eq!(editor_of(&app, &pr).text, typed);
        assert_eq!(app.world().resource::<InputFocus>().get(), Some(area));
    }

    #[test]
    fn a_redrawn_preview_leaves_the_keyboard_alone() {
        let mut app = testing::app(fixture::demo(NOW));
        let (pr, area) = open(&mut app, EditTarget::General, "");
        testing::type_into(&mut app, area, "half");
        let preview = mode_button(&mut app, ComposerMode::Preview);
        testing::activate(&mut app, preview);
        testing::settle(&mut app);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .editor
            .as_mut()
            .unwrap()
            .error = Some("The daemon said no".into());
        testing::settle(&mut app);
        let area = testing::find::<ComposerArea>(&mut app, |_| true);
        assert_eq!(display(&app, area), Display::None);
        assert_ne!(
            app.world().resource::<InputFocus>().get(),
            Some(area),
            "a hidden area does not take the keyboard"
        );
    }

    #[test]
    fn cmd_enter_submits_from_preview() {
        let mut app = testing::app(fixture::demo(NOW));
        let (pr, area) = open(&mut app, EditTarget::General, "Ask about ");
        testing::type_into(&mut app, area, "the cache size");
        let preview = mode_button(&mut app, ComposerMode::Preview);
        testing::activate(&mut app, preview);
        testing::settle(&mut app);
        assert_eq!(app.world().resource::<InputFocus>().get(), None);
        {
            let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            keys.press(KeyCode::SuperLeft);
            keys.press(KeyCode::Enter);
        }
        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset_all();
        testing::settle(&mut app);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::AddItem {
                pr,
                kind: DraftKind::General,
                anchor: None,
                thread: None,
                body: "Ask about the cache size".into(),
                ticket: 1,
            }]
        );
    }

    #[test]
    fn toolbar_buttons_edit_the_area() {
        let mut app = testing::app(fixture::demo(NOW));
        with_fonts(&mut app);
        let (pr, area) = open(&mut app, EditTarget::General, "");
        testing::type_into(&mut app, area, "slow exchange");
        select(&mut app, area, 0..4);
        let bold = testing::find::<ToolbarAction>(&mut app, |a| *a == ToolbarAction::Bold);
        testing::activate(&mut app, bold);
        testing::settle(&mut app);
        let (text, sel) = read_area(app.world().get::<EditableText>(area).unwrap());
        assert_eq!(text, "**slow** exchange");
        assert_eq!(&text[sel], "slow");
        assert_eq!(app.world().resource::<InputFocus>().get(), Some(area));
        assert_eq!(editor_of(&app, &pr).text, "**slow** exchange");
        // From the preview a button first goes back to writing.
        let preview = mode_button(&mut app, ComposerMode::Preview);
        testing::activate(&mut app, preview);
        let code = testing::find::<ToolbarAction>(&mut app, |a| *a == ToolbarAction::Code);
        testing::activate(&mut app, code);
        testing::settle(&mut app);
        assert_eq!(editor_of(&app, &pr).mode, ComposerMode::Write);
        assert_eq!(value(&app, area), "**`slow`** exchange");
    }

    #[test]
    fn the_suggest_button_prefills_the_commented_lines() {
        let mut app = testing::app(fixture::demo(NOW));
        let target = |start| EditTarget::Line {
            path: "src/auth/refresh.rs".into(),
            side: Side::Right,
            start,
            line: 44,
        };
        with_fonts(&mut app);
        let (pr, area) = open(&mut app, target(None), "Could we");
        select(&mut app, area, 8..8);
        let suggest = testing::find::<ToolbarAction>(&mut app, |a| *a == ToolbarAction::Suggest);
        testing::activate(&mut app, suggest);
        testing::settle(&mut app);
        assert_eq!(
            value(&app, area),
            "Could we\n```suggestion\n        let _guard = self.refresh_lock.lock().await;\n```"
        );
        // A range takes every line of it, and a comment on the base side has no button.
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .editor = Some(Editor {
            target: target(Some(42)),
            text: String::new(),
            error: None,
            ticket: None,
            mode: ComposerMode::Write,
        });
        testing::settle(&mut app);
        let suggest = testing::find::<ToolbarAction>(&mut app, |a| *a == ToolbarAction::Suggest);
        testing::activate(&mut app, suggest);
        testing::settle(&mut app);
        let area = testing::find::<ComposerArea>(&mut app, |_| true);
        assert_eq!(
            value(&app, area),
            "```suggestion\n            return Ok(token);\n        }\n        let _guard = self.refresh_lock.lock().await;\n```"
        );
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .editor = Some(Editor {
            target: EditTarget::General,
            text: String::new(),
            error: None,
            ticket: None,
            mode: ComposerMode::Write,
        });
        testing::settle(&mut app);
        let offered = {
            let mut q = app.world_mut().query::<&ToolbarAction>();
            q.iter(app.world()).any(|a| *a == ToolbarAction::Suggest)
        };
        assert!(!offered, "only comments on a line of the new code offer ±");
    }

    #[test]
    fn chips_follow_the_text_in_the_composer() {
        let mut app = testing::app(fixture::demo(NOW));
        let (_, area) = open(&mut app, EditTarget::General, "");
        assert_eq!(testing::count::<ChipRemove>(&mut app), 0);
        testing::type_into(
            &mut app,
            area,
            "Trace:\n![race-trace.png](https://github.com/user-attachments/assets/a1)",
        );
        testing::settle(&mut app);
        assert_eq!(testing::count::<ChipRemove>(&mut app), 1);
        assert_eq!(testing::count::<MdImage>(&mut app), 1, "a thumbnail slot");
        let remove = testing::find::<ChipRemove>(&mut app, |_| true);
        testing::activate(&mut app, remove);
        testing::settle(&mut app);
        assert_eq!(value(&app, area), "Trace:\n");
        assert_eq!(testing::count::<ChipRemove>(&mut app), 0);
    }

    #[test]
    fn a_dropped_file_explains_how_images_go_in() {
        let mut app = testing::app(fixture::demo(NOW));
        app.world_mut().write_message(FileDragAndDrop::DroppedFile {
            window: Entity::PLACEHOLDER,
            path_buf: "/tmp/shot.png".into(),
        });
        app.update();
        assert!(
            app.world().resource::<Toasts>().0.is_empty(),
            "no composer, no hint"
        );
        let (_, area) = open(&mut app, EditTarget::General, "kept");
        app.world_mut().write_message(FileDragAndDrop::DroppedFile {
            window: Entity::PLACEHOLDER,
            path_buf: "/tmp/shot.png".into(),
        });
        app.update();
        let toasts = &app.world().resource::<Toasts>().0;
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].text, IMAGE_FILE_HINT);
        assert_eq!(value(&app, area), "kept");
    }

    #[test]
    fn previews_wait_for_quiet_after_the_first_drawing() {
        assert_eq!(
            preview_due(None, "a", None, None, 1.0),
            (true, None),
            "first: at once"
        );
        assert_eq!(
            preview_due(Some("a"), "a", Some("a"), Some(1.0), 2.0),
            (false, None)
        );
        assert_eq!(
            preview_due(Some("a"), "ab", Some("a"), None, 1.0),
            (false, Some(1.0))
        );
        assert_eq!(
            preview_due(Some("a"), "ab", Some("ab"), Some(1.0), 1.1),
            (false, Some(1.0)),
            "not yet"
        );
        assert_eq!(
            preview_due(Some("a"), "ab", Some("ab"), Some(1.0), 1.13),
            (true, None)
        );
    }

    #[test]
    fn a_text_that_keeps_changing_is_not_drawn_until_it_is_quiet() {
        assert_eq!(
            preview_due(Some("a"), "abc", Some("ab"), Some(1.0), 1.2),
            (false, Some(1.2)),
            "changed again: the clock starts over"
        );
        assert_eq!(
            preview_due(Some("a"), "abc", Some("abc"), Some(1.2), 1.31),
            (false, Some(1.2))
        );
        assert_eq!(
            preview_due(Some("a"), "abc", Some("abc"), Some(1.2), 1.33),
            (true, None)
        );
    }

    #[test]
    fn the_preview_waits_while_the_text_keeps_changing() {
        let mut app = testing::app(fixture::demo(NOW));
        let (_, area) = open(&mut app, EditTarget::General, "one");
        let preview = mode_button(&mut app, ComposerMode::Preview);
        testing::activate(&mut app, preview);
        testing::settle(&mut app);
        let body = testing::find::<PreviewBody>(&mut app, |_| true);
        let drawn = |app: &App| {
            app.world()
                .get::<PreviewBody>(body)
                .unwrap()
                .rendered
                .clone()
        };
        assert_eq!(drawn(&app).as_deref(), Some("one"));
        // Each frame is 70 ms long, so one change per frame never leaves 120 ms of quiet,
        // however long it goes on.
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            70,
        )));
        for more in [" two", " three", " four"] {
            testing::type_into(&mut app, area, more);
            app.update();
            assert_eq!(
                drawn(&app).as_deref(),
                Some("one"),
                "still waiting after {more:?}"
            );
        }
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            150,
        )));
        app.update();
        assert_eq!(drawn(&app).as_deref(), Some("one two three four"));
    }

    #[test]
    fn cmd_enter_ignores_a_preview_behind_a_modal() {
        let mut app = testing::app(fixture::demo(NOW));
        let (pr, area) = open(&mut app, EditTarget::General, "Ask about the cache");
        let preview = mode_button(&mut app, ComposerMode::Preview);
        testing::activate(&mut app, preview);
        testing::settle(&mut app);
        assert_eq!(app.world().resource::<InputFocus>().get(), None);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .modal = Some(crate::review_state::Modal::Finalize);
        {
            let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            keys.press(KeyCode::SuperLeft);
            keys.press(KeyCode::Enter);
        }
        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset_all();
        testing::settle(&mut app);
        assert!(
            !testing::recorded(&mut app)
                .iter()
                .any(|a| matches!(a, Ask::AddItem { .. })),
            "nothing is sent for a composer the modal hides"
        );
        assert_eq!(value(&app, area), "Ask about the cache");
    }

    #[test]
    fn chips_know_hosts_with_userinfo_and_ports() {
        let mut app = testing::app(fixture::demo(NOW));
        let (_, area) = open(&mut app, EditTarget::General, "");
        testing::type_into(
            &mut app,
            area,
            "![a](https://octo@github.com:443/user-attachments/assets/a1)\n![b](https://github.com@evil.example/b.png)",
        );
        testing::settle(&mut app);
        assert_eq!(testing::count::<ChipRemove>(&mut app), 2);
        assert_eq!(
            testing::count::<MdImage>(&mut app),
            1,
            "only the GitHub one gets a thumbnail"
        );
    }

    #[test]
    fn removing_a_chip_before_the_cursor_moves_the_cursor_back() {
        let mut app = testing::app(fixture::demo(NOW));
        with_fonts(&mut app);
        let (_, area) = open(&mut app, EditTarget::General, "");
        testing::type_into(
            &mut app,
            area,
            "![a](https://github.com/user-attachments/assets/a1) then more",
        );
        testing::settle(&mut app);
        let end = value(&app, area).len();
        select(&mut app, area, end - 4..end - 4);
        let remove = testing::find::<ChipRemove>(&mut app, |_| true);
        testing::activate(&mut app, remove);
        testing::settle(&mut app);
        let (text, sel) = read_area(app.world().get::<EditableText>(area).unwrap());
        assert_eq!(text, " then more");
        assert_eq!(&text[sel.start..], "more", "the cursor stays before `more`");
    }

    #[test]
    fn modes_of_finalize_fields_live_apart_from_editors() {
        let pr: PrRef = "rzorzal/clusia#123".parse().unwrap();
        let tabs = ReviewTabs::default();
        let mut extra = ExtraModes::default();
        let summary = ComposerKey(pr.clone(), Slot::FinalizeSummary);
        let item = ComposerKey(pr.clone(), Slot::FinalizeItem("i1".into()));
        assert_eq!(mode_of(&summary, &tabs, &extra), ComposerMode::Write);
        let mut tabs = tabs;
        set_mode(&summary, ComposerMode::Preview, &mut tabs, &mut extra);
        assert_eq!(mode_of(&summary, &tabs, &extra), ComposerMode::Preview);
        assert_eq!(mode_of(&item, &tabs, &extra), ComposerMode::Write);
    }

    #[test]
    fn suggestion_source_takes_the_new_side_lines() {
        let files = fixture::demo_review(NOW).0.diff;
        let path = "src/auth/refresh.rs";
        assert_eq!(
            suggestion_source(&files, path, Some(40), 41).as_deref(),
            Some(
                "        // Refresh one minute early so a request never races the expiry.\n        if token.expires_at > now() + Duration::from_secs(60) {"
            )
        );
        assert_eq!(suggestion_source(&files, path, None, 9999), None);
        assert_eq!(suggestion_source(&files, "nope.rs", None, 44), None);
    }
}
