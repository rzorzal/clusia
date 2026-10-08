//! A multi-line plain-text field for comments.
//!
//! Enter inserts a new line; ⌘↵ submits (`TextSubmitted`). It does not carry `kit::Field`, whose
//! Enter-commits behavior is for one-line fields.

use bevy::input::ButtonInput;
use bevy::input_focus::InputFocus;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::prelude::*;
use bevy::text::{EditableText, TextCursorStyle, TextEdit};
use bevy::ui::UiSystems;
use bevy::window::RequestRedraw;

use crate::fonts::UiFonts;
use crate::theme::Swatch;
use crate::ui::kit::{Fill, Ink, Stroke, Type, font};

/// Marks a text area; `id` lets its owner tell several apart.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextArea {
    pub id: u64,
}

/// ⌘↵ in a focused text area.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct TextSubmitted {
    pub entity: Entity,
    pub id: u64,
    pub value: String,
}

/// Makes a text area as tall as its laid-out text, between `min` and `max` lines; past `max`
/// it scrolls.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Grow {
    pub min: f32,
    pub max: f32,
}

/// A text area as wide as its parent, `lines` lines tall, starting with `value`.
pub fn text_area(fonts: &UiFonts, value: &str, lines: f32, id: u64) -> impl Bundle {
    let mut editable = EditableText::new(value);
    editable.allow_newlines = true;
    editable.visible_lines = Some(lines);
    area(fonts, editable, id)
}

/// Where the cursor of a growing area starts. A text taller than the area opens scrolled to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caret {
    Start,
    End,
}

/// A text area that grows with `value` from `min` to `max` lines, with the cursor at `caret`.
pub fn growing_text_area(
    fonts: &UiFonts,
    value: &str,
    min: f32,
    max: f32,
    caret: Caret,
    id: u64,
) -> impl Bundle {
    let mut editable = EditableText::new(value);
    editable.allow_newlines = true;
    editable.visible_lines = Some(min);
    editable.queue_edit(match caret {
        Caret::Start => TextEdit::TextStart(false),
        Caret::End => TextEdit::TextEnd(false),
    });
    (area(fonts, editable, id), Grow { min, max })
}

/// Like `growing_text_area`, but Enter does not insert a line break: the owner sends on Enter
/// and queues the break itself for Shift+Enter.
pub fn growing_line_area(
    fonts: &UiFonts,
    value: &str,
    min: f32,
    max: f32,
    caret: Caret,
    id: u64,
) -> impl Bundle {
    let mut editable = EditableText::new(value);
    editable.allow_newlines = false;
    editable.visible_lines = Some(min);
    editable.queue_edit(match caret {
        Caret::Start => TextEdit::TextStart(false),
        Caret::End => TextEdit::TextEnd(false),
    });
    (area(fonts, editable, id), Grow { min, max })
}

fn area(fonts: &UiFonts, editable: EditableText, id: u64) -> impl Bundle {
    (
        Node {
            width: percent(100),
            padding: UiRect::axes(px(10), px(8)),
            border: px(1).all(),
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        editable,
        TextLayout {
            linebreak: LineBreak::WordOrCharacter,
            ..default()
        },
        TextCursorStyle::default(),
        font(fonts, Type::BODY),
        TextColor::default(),
        Ink(Swatch::Fg),
        TabIndex(0),
        BackgroundColor::default(),
        Fill(Swatch::Surface),
        BorderColor::default(),
        Stroke(Swatch::Line),
        TextArea { id },
    )
}

/// Replaces the text and puts the cursor at the end.
pub fn set_text(editable: &mut EditableText, value: &str) {
    editable.editor_mut().set_text(value);
    editable.queue_edit(TextEdit::TextEnd(false));
}

pub struct TextAreaPlugin;

impl Plugin for TextAreaPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<TextSubmitted>()
            .add_systems(Update, submit_on_cmd_enter)
            .add_systems(PostUpdate, grow_to_content.after(UiSystems::PostLayout));
    }
}

/// Sets each growing area's height from its last layout; the next layout pass applies it.
fn grow_to_content(
    mut areas: Query<(&mut EditableText, &Grow)>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    for (mut editable, grow) in &mut areas {
        let Some(layout) = editable.editor().try_layout() else {
            continue;
        };
        let lines = Some((layout.len() as f32).clamp(grow.min, grow.max));
        if editable.visible_lines != lines {
            editable.visible_lines = lines;
            redraw.write(RequestRedraw);
        }
    }
}

fn submit_on_cmd_enter(
    keys: Res<ButtonInput<KeyCode>>,
    focus: Res<InputFocus>,
    areas: Query<(&TextArea, &EditableText)>,
    mut out: MessageWriter<TextSubmitted>,
) {
    let cmd = keys.any_pressed([KeyCode::SuperLeft, KeyCode::SuperRight]);
    if !(cmd && keys.just_pressed(KeyCode::Enter)) {
        return;
    }
    if let Some(entity) = focus.get()
        && let Ok((area, editable)) = areas.get(entity)
        && !editable.is_composing()
    {
        out.write(TextSubmitted {
            entity,
            id: area.id,
            value: editable.value().to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Snapshot;
    use crate::testing;
    use bevy::clipboard::Clipboard;
    use bevy::input_focus::FocusCause;
    use bevy::text::{FontCx, LayoutCx};

    fn submitted(app: &mut App) -> Vec<TextSubmitted> {
        app.world_mut()
            .resource_mut::<Messages<TextSubmitted>>()
            .drain()
            .collect()
    }

    fn press(app: &mut App, keys: &[KeyCode]) {
        let mut input = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
        input.reset_all();
        for k in keys {
            input.press(*k);
        }
    }

    #[test]
    fn cmd_enter_submits_the_focused_area() {
        let mut app = testing::app(Snapshot::default());
        let fonts = UiFonts::default();
        let area = app
            .world_mut()
            .spawn(text_area(&fonts, "Why ", 4.0, 7))
            .id();
        let other = app.world_mut().spawn(text_area(&fonts, "", 4.0, 8)).id();
        app.update();
        testing::type_into(&mut app, area, "one minute?\nThe CLI uses 30 s.");
        press(&mut app, &[KeyCode::SuperLeft, KeyCode::Enter]);
        app.update();
        assert!(submitted(&mut app).is_empty(), "nothing focused");
        app.world_mut()
            .resource_mut::<InputFocus>()
            .set(area, FocusCause::Navigated);
        press(&mut app, &[KeyCode::Enter]);
        app.update();
        assert!(submitted(&mut app).is_empty(), "Enter alone is a new line");
        press(&mut app, &[KeyCode::SuperRight, KeyCode::Enter]);
        app.update();
        assert_eq!(
            submitted(&mut app),
            [TextSubmitted {
                entity: area,
                id: 7,
                value: "Why one minute?\nThe CLI uses 30 s.".into()
            }]
        );
        assert!(app.world().get::<TextArea>(other).is_some());
    }

    /// Lays the area's text out in Inter, as `bevy_ui` would before `grow_to_content` runs.
    fn lay_out(app: &mut App, area: Entity) {
        let mut fonts = FontCx::default();
        let family = fonts
            .collection
            .register_fonts(Font::from_bytes(crate::fonts::INTER.to_vec()).data, None)[0]
            .0;
        let name = fonts.collection.family_name(family).unwrap().to_string();
        fonts.set_sans_serif_family(&name).unwrap();
        let mut layout = LayoutCx::default();
        let mut editable = app.world_mut().get_mut::<EditableText>(area).unwrap();
        editable.apply_pending_edits(&mut fonts, &mut layout.0, &mut Clipboard::default(), |_| {
            true
        });
        editable.editor_mut().layout(&mut fonts, &mut layout.0);
    }

    fn lines(app: &App, area: Entity) -> Option<f32> {
        app.world().get::<EditableText>(area).unwrap().visible_lines
    }

    #[test]
    fn growing_areas_fit_their_lines_up_to_a_cap_and_start_at_their_caret() {
        let mut app = testing::app(Snapshot::default());
        let fonts = UiFonts::default();
        let mut spawn = |value: &str, id| {
            app.world_mut()
                .spawn(growing_text_area(&fonts, value, 2.0, 6.0, Caret::End, id))
                .id()
        };
        let short = spawn("one line", 1);
        let three = spawn("a\nb\nc", 2);
        let long = spawn("1\n2\n3\n4\n5\n6\n7\n8\n9", 3);
        assert_eq!(lines(&app, short), Some(2.0), "starts at its minimum");
        for area in [short, three, long] {
            lay_out(&mut app, area);
        }
        app.update();
        assert_eq!(lines(&app, short), Some(2.0), "never below the minimum");
        assert_eq!(lines(&app, three), Some(3.0), "grows with its text");
        assert_eq!(lines(&app, long), Some(6.0), "then stops and scrolls");
        testing::type_into(&mut app, short, "z");
        let e = app.world().get::<EditableText>(short).unwrap();
        assert_eq!(
            e.value().to_string(),
            "one linez",
            "the cursor starts at the end"
        );
        let first = app
            .world_mut()
            .spawn(growing_text_area(
                &fonts,
                "one line",
                2.0,
                6.0,
                Caret::Start,
                4,
            ))
            .id();
        lay_out(&mut app, first);
        testing::type_into(&mut app, first, "z");
        let e = app.world().get::<EditableText>(first).unwrap();
        assert_eq!(e.value().to_string(), "zone line", "or at the start");
    }

    #[test]
    fn areas_are_multi_line_and_settable() {
        let mut app = testing::app(Snapshot::default());
        let fonts = UiFonts::default();
        let area = app.world_mut().spawn(text_area(&fonts, "a", 3.0, 1)).id();
        app.update();
        {
            let e = app.world().get::<EditableText>(area).unwrap();
            assert!(e.allow_newlines);
            assert_eq!(e.visible_lines, Some(3.0));
        }
        assert!(app.world().get::<crate::ui::kit::Field>(area).is_none());
        let mut e = app.world_mut().get_mut::<EditableText>(area).unwrap();
        set_text(&mut e, "x\ny");
        assert_eq!(e.value().to_string(), "x\ny");
        testing::type_into(&mut app, area, "z");
        let e = app.world().get::<EditableText>(area).unwrap();
        assert_eq!(e.value().to_string(), "x\nyz", "the cursor went to the end");
    }
}
