//! A headless app for unit tests: MinimalPlugins, no window, no GPU, no daemon. Asks are
//! recorded in `Asks::recorded`. Each task that adds a logic plugin registers it in `app`.

use bevy::input::ButtonInput;
use bevy::input_focus::InputFocus;
use bevy::prelude::*;
use bevy::ui_widgets::Activate;
use bevy::window::{RequestRedraw, WindowThemeChanged};
use clusia_core::{Density, Paths};
use clusia_protocol::WindowTarget;

use crate::app::{AppPaths, StartTarget};
use crate::bridge::{Ask, Asks, Connection, Model, ShowRequested, Toasts};
use crate::clock::Clock;
use crate::fonts::UiFonts;
use crate::nav::NavPlugin;
use crate::screens::config::ConfigPlugin;
use crate::screens::home::HomePlugin;
use crate::snapshot::{self, Snapshot};
use crate::theme::{LIGHT, Theme, ThemePlugin};
use crate::ui::kit::KitPlugin;

pub const NOW: i64 = 1_790_000_000;

pub fn app(snapshot: Snapshot) -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .init_resource::<ButtonInput<KeyCode>>()
        .init_resource::<InputFocus>()
        .add_message::<WindowThemeChanged>()
        .add_message::<ShowRequested>()
        .add_message::<RequestRedraw>()
        .insert_resource(UiFonts::default())
        .insert_resource(Clock(Some(NOW)))
        .insert_resource(AppPaths(Paths::new("/tmp/clusia-test-home")))
        .insert_resource(StartTarget(WindowTarget::Home))
        .insert_resource(Theme::new(false, 13, Density::Comfortable))
        .insert_resource(ClearColor(LIGHT.bg))
        .insert_resource(Model {
            snapshot,
            connection: Connection::Live,
            ..Model::default()
        })
        .init_resource::<Asks>()
        .init_resource::<Toasts>()
        .add_plugins((ThemePlugin, KitPlugin, NavPlugin, HomePlugin, ConfigPlugin));
    app.update();
    app
}

/// Takes the asks recorded since the last call.
pub fn recorded(app: &mut App) -> Vec<Ask> {
    std::mem::take(&mut app.world_mut().resource_mut::<Asks>().recorded)
}

pub fn count<C: Component>(app: &mut App) -> usize {
    app.world_mut().query::<&C>().iter(app.world()).count()
}

/// The first entity whose `C` satisfies `want`.
pub fn find<C: Component>(app: &mut App, want: impl Fn(&C) -> bool) -> Entity {
    let mut q = app.world_mut().query::<(Entity, &C)>();
    q.iter(app.world())
        .find(|(_, c)| want(c))
        .map(|(e, _)| e)
        .expect("a matching entity")
}

/// Clicks `entity` (as `bevy_ui_widgets` would) and runs a frame.
pub fn activate(app: &mut App, entity: Entity) {
    app.world_mut().trigger(Activate { entity });
    app.update();
}

/// Changes the config as the daemon would and runs a frame.
pub fn set_config_locally(app: &mut App, key: &str, value: &str) {
    let mut model = app.world_mut().resource_mut::<Model>();
    snapshot::apply_config_locally(&mut model.snapshot.config, key, value).unwrap();
    app.update();
}

/// Runs frames until rebuilt screens have settled.
pub fn settle(app: &mut App) {
    for _ in 0..3 {
        app.update();
    }
}
