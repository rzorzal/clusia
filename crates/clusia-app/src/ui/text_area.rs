//! A multi-line plain-text field for comments (the rich composer is M5c).
//!
//! Enter inserts a new line; ⌘↵ submits (`TextSubmitted`). It does not carry `kit::Field`, whose
//! Enter-commits behavior is for one-line fields.

use bevy::input::ButtonInput;
use bevy::input_focus::InputFocus;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::prelude::*;
use bevy::text::{EditableText, TextCursorStyle, TextEdit};

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

/// A text area as wide as its parent, `lines` lines tall, starting with `value`.
pub fn text_area(fonts: &UiFonts, value: &str, lines: f32, id: u64) -> impl Bundle {
    let mut editable = EditableText::new(value);
    editable.allow_newlines = true;
    editable.visible_lines = Some(lines);
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
            .add_systems(Update, submit_on_cmd_enter);
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
    use bevy::input_focus::FocusCause;

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
