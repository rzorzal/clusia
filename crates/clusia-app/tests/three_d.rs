//! The 3D pieces of Bevy the keyword view is built from, and the keyboard route it relies on.

use bevy::camera::{Camera, Projection};
use bevy::core_pipeline::tonemapping::Tonemapping;
use bevy::input::keyboard::{Key, KeyCode, KeyboardInput};
use bevy::input::{ButtonState, InputPlugin};
use bevy::input_focus::tab_navigation::{TabGroup, TabIndex, TabNavigationPlugin};
use bevy::input_focus::{
    FocusCause, FocusedInput, InputDispatchPlugin, InputFocus, InputFocusPlugin,
};
use bevy::pbr::StandardMaterial;
use bevy::prelude::*;
use bevy::window::{PrimaryWindow, WindowPlugin};

#[test]
fn the_3d_pipeline_types_are_built_in() {
    let material = StandardMaterial {
        unlit: true,
        alpha_mode: AlphaMode::Blend,
        perceptual_roughness: 1.0,
        ..default()
    };
    assert!(material.unlit);
    let sphere = Sphere::new(1.0).mesh().ico(3).expect("an icosphere");
    assert!(sphere.count_vertices() > 0);
    let target = Image::new_target_texture(
        64,
        32,
        bevy::render::render_resource::TextureFormat::Rgba8UnormSrgb,
        None,
    );
    assert_eq!((target.width(), target.height()), (64, 32));
    let mut world = World::new();
    let camera = world
        .spawn((
            Camera3d::default(),
            Camera::default(),
            Tonemapping::None,
            Projection::Orthographic(OrthographicProjection::default_3d()),
        ))
        .id();
    assert!(world.get::<Camera3d>(camera).is_some());
}

/// The keys the view's own observer saw.
#[derive(Resource, Default)]
struct Seen(Vec<KeyCode>);

fn press(app: &mut App, window: Entity, code: KeyCode) {
    app.world_mut().write_message(KeyboardInput {
        key_code: code,
        logical_key: Key::Tab,
        state: ButtonState::Pressed,
        text: None,
        repeat: false,
        window,
    });
    app.update();
}

/// A window with one tab group holding the view (tab index 0) and another stop (index 1), the
/// focus on the view. The view's observer stops Tab from bubbling on to the window when `stop`.
fn tab_world(stop: bool) -> (App, Entity, Entity, Entity) {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        InputPlugin,
        WindowPlugin::default(),
        InputFocusPlugin,
        InputDispatchPlugin,
        TabNavigationPlugin,
    ))
    .init_resource::<Seen>();
    let window = app
        .world_mut()
        .query_filtered::<Entity, With<PrimaryWindow>>()
        .single(app.world())
        .expect("a primary window");
    let group = app.world_mut().spawn(TabGroup::default()).id();
    let view = app
        .world_mut()
        .spawn((TabIndex(0), ChildOf(group)))
        .observe(
            move |mut event: On<FocusedInput<KeyboardInput>>, mut seen: ResMut<Seen>| {
                seen.0.push(event.input.key_code);
                if stop && event.input.key_code == KeyCode::Tab {
                    event.propagate(false);
                }
            },
        )
        .id();
    let other = app.world_mut().spawn((TabIndex(1), ChildOf(group))).id();
    app.update();
    app.world_mut()
        .resource_mut::<InputFocus>()
        .set(view, FocusCause::Navigated);
    (app, window, view, other)
}

#[test]
fn tab_moves_the_focus_on_when_the_view_lets_it_bubble() {
    let (mut app, window, _view, other) = tab_world(false);
    press(&mut app, window, KeyCode::Tab);
    assert_eq!(app.world().resource::<Seen>().0, [KeyCode::Tab]);
    assert_eq!(app.world().resource::<InputFocus>().get(), Some(other));
}

#[test]
fn a_view_that_stops_tab_keeps_the_focus_and_still_gets_every_key() {
    let (mut app, window, view, _other) = tab_world(true);
    press(&mut app, window, KeyCode::Tab);
    assert_eq!(app.world().resource::<InputFocus>().get(), Some(view));
    press(&mut app, window, KeyCode::ArrowLeft);
    assert_eq!(
        app.world().resource::<Seen>().0,
        [KeyCode::Tab, KeyCode::ArrowLeft]
    );
    assert_eq!(app.world().resource::<InputFocus>().get(), Some(view));
}
