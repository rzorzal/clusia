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
use crate::ui::spinner::Motion;
use crate::ui::tween::{DespawnAfter, Ease, Look, Tween};

/// A click landed on the dimmed layer itself, not on the card or anything in it. Each dialog
/// that may be dismissed that way reads this next to its Esc key and applies the same rule.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackdropClicked(pub Entity);

pub struct ModalPlugin;

impl Plugin for ModalPlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<BackdropClicked>()
            .add_observer(open_layer)
            .add_observer(open_card)
            .add_observer(close_layer);
    }
}

/// How long a dialog takes to come in and to go, and the size its card grows from.
pub const MODAL_IN_MS: u64 = 160;
pub const MODAL_OUT_MS: u64 = 100;
pub const CARD_FROM: f32 = 0.98;

/// The dimmed layer of a dialog (`modal_root`).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModalLayer;

/// A dialog's card (`modal_card`).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModalCard;

/// The dim left behind by a closed dialog while it fades. It blocks nothing.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct FadingScrim;

/// The layer dims in.
fn open_layer(add: On<Add, ModalLayer>, motion: Res<Motion>, mut commands: Commands) {
    if !motion.reduced {
        commands.entity(add.entity).insert(Tween::new(
            MODAL_IN_MS,
            Ease::OutCubic,
            Look::REST.faded(0.0),
            Look::REST,
        ));
    }
}

/// A card on a dialog's layer grows into place as its surface and border fade in (a card used
/// elsewhere, as the loading one, does not).
fn open_card(
    add: On<Add, ModalCard>,
    motion: Res<Motion>,
    parents: Query<&ChildOf>,
    layers: Query<(), With<ModalLayer>>,
    mut commands: Commands,
) {
    let on_layer = parents
        .get(add.entity)
        .is_ok_and(|c| layers.contains(c.parent()));
    if on_layer && !motion.reduced {
        commands.entity(add.entity).insert(Tween::new(
            MODAL_IN_MS,
            Ease::OutCubic,
            Look::REST.scaled(CARD_FROM).faded(0.0),
            Look::REST,
        ));
    }
}

/// Whoever closes a dialog despawns it at once (so nothing reads a dialog that has gone); its
/// dim fades out on its own over what was under it.
fn close_layer(
    gone: On<Despawn, ModalLayer>,
    motion: Res<Motion>,
    layers: Query<&GlobalZIndex>,
    mut commands: Commands,
) {
    if motion.reduced {
        return;
    }
    // Just under the layer's own height, so a dialog opened in the same frame (the next
    // permission) dims in over the old dim, not under it.
    let z = layers.get(gone.entity).map_or(100, |z| z.0) - 1;
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: px(0),
            top: px(0),
            width: percent(100),
            height: percent(100),
            ..default()
        },
        GlobalZIndex(z),
        BackgroundColor::default(),
        Fill(Swatch::Scrim),
        Pickable::IGNORE,
        FadingScrim,
        Tween::new(
            MODAL_OUT_MS,
            Ease::InCubic,
            Look::REST,
            Look::REST.faded(0.0),
        ),
        DespawnAfter::new(MODAL_OUT_MS),
    ));
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
        ModalLayer,
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
        ModalCard,
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
        // The colours it rests on, once it has dimmed in.
        testing::run_tweens(&mut app);
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

    fn dialog(app: &mut App) -> (Entity, Entity) {
        let root = app.world_mut().spawn(modal_root()).id();
        app.world_mut().entity_mut(root).insert(GlobalZIndex(110));
        let card = app
            .world_mut()
            .spawn((modal_card(560.0), ChildOf(root)))
            .id();
        (root, card)
    }

    #[test]
    fn a_dialog_dims_in_and_its_card_grows_into_place() {
        use crate::ui::tween::Tween;
        let mut app = testing::app(Snapshot::default());
        let (root, card) = dialog(&mut app);
        app.update();
        assert_eq!(
            app.world().get::<BackgroundColor>(root).unwrap().0.alpha(),
            0.0
        );
        assert_eq!(
            app.world().get::<UiTransform>(card).unwrap().scale,
            Vec2::splat(CARD_FROM)
        );
        assert_eq!(
            app.world().get::<BackgroundColor>(card).unwrap().0.alpha(),
            0.0
        );
        assert_eq!(
            app.world().get::<BorderColor>(card).unwrap().top.alpha(),
            0.0
        );
        app.update();
        let scale = app.world().get::<UiTransform>(card).unwrap().scale.x;
        assert!(scale > CARD_FROM && scale < 1.0, "{scale}");
        let frames = testing::run_tweens(&mut app);
        assert!(frames * 16 <= MODAL_IN_MS as usize + 32, "{frames} frames");
        assert_eq!(
            app.world().get::<BackgroundColor>(root).unwrap().0,
            LIGHT.scrim
        );
        assert_eq!(
            app.world().get::<UiTransform>(card).unwrap().scale,
            Vec2::ONE
        );
        assert_eq!(
            app.world().get::<BackgroundColor>(card).unwrap().0,
            LIGHT.surface
        );
        assert_eq!(
            app.world().get::<BorderColor>(card).unwrap().top,
            LIGHT.line
        );
        assert!(app.world().get::<Tween>(root).is_none());
    }

    #[test]
    fn a_card_off_a_dialog_layer_does_not_grow() {
        let mut app = testing::app(Snapshot::default());
        let card = app.world_mut().spawn(modal_card(540.0)).id();
        app.update();
        assert!(app.world().get::<crate::ui::tween::Tween>(card).is_none());
    }

    #[test]
    fn a_closed_dialog_goes_at_once_and_its_dim_fades_out_blocking_nothing() {
        let mut app = testing::app(Snapshot::default());
        let (root, _) = dialog(&mut app);
        testing::run_tweens(&mut app);
        app.world_mut().entity_mut(root).despawn();
        app.update();
        assert_eq!(
            testing::count::<ModalLayer>(&mut app),
            0,
            "nothing reads a gone dialog"
        );
        let scrim = testing::find::<FadingScrim>(&mut app, |_| true);
        let w = app.world();
        assert_eq!(*w.get::<Pickable>(scrim).unwrap(), Pickable::IGNORE);
        assert_eq!(
            w.get::<GlobalZIndex>(scrim).unwrap().0,
            109,
            "under a dialog opened in its place"
        );
        let alpha = w.get::<BackgroundColor>(scrim).unwrap().0.alpha();
        assert!(alpha > 0.0, "{alpha}");
        testing::run_tweens(&mut app);
        assert_eq!(testing::count::<FadingScrim>(&mut app), 0);
    }

    #[test]
    fn with_reduce_motion_a_dialog_just_opens_and_closes() {
        let mut app = testing::app(Snapshot::default());
        app.insert_resource(Motion { reduced: true });
        let (root, card) = dialog(&mut app);
        app.update();
        assert_eq!(
            app.world().get::<BackgroundColor>(root).unwrap().0,
            LIGHT.scrim
        );
        assert_eq!(
            app.world().get::<UiTransform>(card).unwrap().scale,
            Vec2::ONE
        );
        app.world_mut().entity_mut(root).despawn();
        app.update();
        assert_eq!(testing::count::<FadingScrim>(&mut app), 0);
        assert_eq!(testing::count::<crate::ui::tween::Tween>(&mut app), 0);
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
