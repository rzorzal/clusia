//! Builds and runs the Bevy app. Later tasks register their plugins in `run`.

use std::path::PathBuf;

use bevy::input_focus::tab_navigation::TabNavigationPlugin;
use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, save_to_disk};
use bevy::window::WindowResizeConstraints;
use bevy::winit::WinitSettings;
use clusia_core::{Density, Paths};
use clusia_protocol::WindowTarget;

use crate::clock::Clock;
use crate::fonts::FontsPlugin;
use crate::theme::{LIGHT, Theme};

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    /// Connected to the daemon.
    Live,
    /// Demo data; `dark` forces the dark theme.
    Demo { dark: bool },
}

#[derive(Debug, Clone)]
pub struct Launch {
    pub paths: Paths,
    /// `--home`, passed on when the daemon has to be started.
    pub home: Option<PathBuf>,
    pub target: WindowTarget,
    pub mode: Mode,
    pub screenshot: Option<PathBuf>,
}

#[derive(Resource, Debug, Clone)]
pub struct AppPaths(pub Paths);

/// Where the window opens (from the command line).
#[derive(Resource, Debug, Clone)]
pub struct StartTarget(pub WindowTarget);

#[derive(Resource, Debug, Clone)]
struct ScreenshotPlan {
    path: PathBuf,
    after_frames: u32,
}

pub fn run(launch: Launch) {
    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .build()
            .disable::<LogPlugin>()
            .set(WindowPlugin {
                primary_window: Some(window()),
                ..default()
            }),
    )
    .add_plugins((TabNavigationPlugin, FontsPlugin))
    .insert_resource(if launch.screenshot.is_some() {
        WinitSettings::continuous()
    } else {
        WinitSettings::desktop_app()
    })
    .insert_resource(AppPaths(launch.paths.clone()))
    .insert_resource(StartTarget(launch.target.clone()))
    .insert_resource(Clock::default())
    .insert_resource(Theme::new(false, 13, Density::Comfortable))
    .insert_resource(ClearColor(LIGHT.bg))
    .add_systems(Startup, |mut commands: Commands| {
        commands.spawn(Camera2d);
    });
    if let Some(path) = &launch.screenshot {
        app.insert_resource(ScreenshotPlan {
            path: path.clone(),
            after_frames: 20,
        })
        .add_systems(Update, take_screenshot);
    }
    app.run();
}

fn window() -> Window {
    Window {
        title: "Clúsia".into(),
        resolution: (1280, 800).into(),
        resize_constraints: WindowResizeConstraints {
            min_width: 960.0,
            min_height: 640.0,
            ..default()
        },
        titlebar_transparent: true,
        titlebar_show_title: false,
        fullsize_content_view: true,
        ..default()
    }
}

/// Waits for fonts and layout to settle, saves the frame, and exits once it is written.
fn take_screenshot(
    mut commands: Commands,
    plan: Res<ScreenshotPlan>,
    mut frame: Local<u32>,
    mut exit: MessageWriter<AppExit>,
) {
    *frame += 1;
    if *frame == plan.after_frames {
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(plan.path.clone()));
    }
    if *frame == plan.after_frames + 30 {
        exit.write(AppExit::Success);
    }
}
