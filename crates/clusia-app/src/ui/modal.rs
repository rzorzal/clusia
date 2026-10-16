//! Modal dialogs: a dimmed root layer over the whole window with a card in the middle.
//!
//! The layer is a **root** entity (no parent) so it covers the top bar too: whoever spawns it
//! also despawns it. Its background blocks picking below, and `TabGroup::modal()` keeps Tab
//! navigation inside it.

use bevy::input::ButtonInput;
use bevy::input_focus::tab_navigation::TabGroup;
use bevy::picking::Pickable;
use bevy::picking::events::{Click, Pointer};
use bevy::prelude::*;
use bevy::ui_widgets::observe;

use crate::theme::Swatch;
use crate::ui::kit::{Fill, Stroke};

/// A click landed on the dimmed layer itself, not on the card or anything in it. Each dialog
/// that may be dismissed that way reads this next to its Esc key and applies the same rule.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackdropClicked(pub Entity);

pub struct ModalPlugin;

impl Plugin for ModalPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<BackdropClicked>();
    }
}

/// Clicks inside the card bubble up to the layer; only the ones that began on the layer count.
fn on_backdrop_click(click: On<Pointer<Click>>, mut out: MessageWriter<BackdropClicked>) {
    if click.original_event_target() == click.event_target() {
        out.write(BackdropClicked(click.event_target()));
    }
}

/// The full-window layer. Spawn the card (`modal_card`) as its child.
pub fn modal_root() -> impl Bundle {
    (
        Node {
            position_type: PositionType::Absolute,
            left: px(0),
            top: px(0),
            width: percent(100),
            height: percent(100),
            justify_content: JustifyContent::Center,
            align_items: AlignItems::Center,
            ..default()
        },
        GlobalZIndex(100),
        BackgroundColor::default(),
        Fill(Swatch::Scrim),
        Pickable {
            should_block_lower: true,
            is_hoverable: true,
        },
        TabGroup::modal(),
        observe(on_backdrop_click),
    )
}

/// The dialog: `width` px, surface, hairline border, 10 px corners, contents in a column.
pub fn modal_card(width: f32) -> impl Bundle {
    (
        Node {
            width: px(width),
            max_height: percent(90),
            flex_direction: FlexDirection::Column,
            border: px(1).all(),
            border_radius: BorderRadius::all(px(10)),
            overflow: Overflow::clip(),
            ..default()
        },
        BackgroundColor::default(),
        Fill(Swatch::Surface),
        BorderColor::default(),
        Stroke(Swatch::Line),
    )
}

/// Esc was pressed this frame (closes the open modal).
pub fn escape_pressed(keys: &ButtonInput<KeyCode>) -> bool {
    keys.just_pressed(KeyCode::Escape)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Snapshot;
    use crate::testing;
    use crate::theme::{DARK, LIGHT};

    #[test]
    fn modal_layer_dims_blocks_and_traps_focus() {
        let mut app = testing::app(Snapshot::default());
        let root = app.world_mut().spawn(modal_root()).id();
        let card = app
            .world_mut()
            .spawn((modal_card(560.0), ChildOf(root)))
            .id();
        app.update();
        let w = app.world();
        assert!(w.get::<ChildOf>(root).is_none(), "a root entity");
        assert_eq!(w.get::<GlobalZIndex>(root).unwrap().0, 100);
        assert!(w.get::<Pickable>(root).unwrap().should_block_lower);
        assert!(w.get::<TabGroup>(root).unwrap().modal);
        assert_eq!(w.get::<BackgroundColor>(root).unwrap().0, LIGHT.scrim);
        assert_eq!(w.get::<Node>(card).unwrap().width, px(560));
        assert_eq!(w.get::<BackgroundColor>(card).unwrap().0, LIGHT.surface);
        testing::set_config_locally(&mut app, "appearance.theme", "dark");
        assert_eq!(
            app.world().get::<BackgroundColor>(root).unwrap().0,
            DARK.scrim
        );
    }

    #[test]
    fn only_a_click_on_the_layer_itself_is_a_backdrop_click() {
        let mut app = testing::app(Snapshot::default());
        let root = app.world_mut().spawn(modal_root()).id();
        let card = app
            .world_mut()
            .spawn((modal_card(560.0), ChildOf(root)))
            .id();
        let inner = app.world_mut().spawn((Node::default(), ChildOf(card))).id();
        app.update();
        let clicks = |app: &mut App| -> Vec<BackdropClicked> {
            let mut messages = app.world_mut().resource_mut::<Messages<BackdropClicked>>();
            messages.drain().collect()
        };
        testing::click(&mut app, inner);
        testing::click(&mut app, card);
        assert!(clicks(&mut app).is_empty(), "the card is not the backdrop");
        testing::click(&mut app, root);
        assert_eq!(clicks(&mut app), [BackdropClicked(root)]);
    }

    #[test]
    fn escape_is_a_fresh_press() {
        let mut keys = ButtonInput::<KeyCode>::default();
        assert!(!escape_pressed(&keys));
        keys.press(KeyCode::Escape);
        assert!(escape_pressed(&keys));
        keys.clear();
        assert!(!escape_pressed(&keys), "held, not pressed again");
    }
}
