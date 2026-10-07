//! Builds and runs the Bevy app. Later tasks register their plugins in `run`.

use std::path::PathBuf;
use std::time::Duration;

use bevy::input_focus::tab_navigation::TabNavigationPlugin;
use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, save_to_disk};
use bevy::window::WindowResizeConstraints;
use bevy::winit::WinitSettings;
use clusia_core::{Density, Paths};
use clusia_protocol::WindowTarget;

use crate::args::Scene;
use crate::bridge::BridgeSlot;
use crate::clock::Clock;
use crate::fonts::FontsPlugin;
use crate::theme::{LIGHT, Theme};

#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    /// Connected to the daemon.
    Live,
    /// Demo data; `theme` forces a theme (`None` follows the macOS appearance).
    Demo {
        theme: Option<clusia_core::config::Theme>,
    },
}

#[derive(Debug, Clone)]
pub struct Launch {
    pub paths: Paths,
    /// `--home`, passed on when the daemon has to be started.
    pub home: Option<PathBuf>,
    pub target: WindowTarget,
    pub mode: Mode,
    pub screenshot: Option<PathBuf>,
    /// How many consecutive frames the screenshot saves.
    pub frames: u32,
    /// A fixed clock step per frame, so saved frames sample animations evenly.
    pub frame_step: Option<Duration>,
    /// `--demo --scene`: stage the demo review (screenshots).
    pub scene: Option<Scene>,
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
    frames: u32,
}

impl ScreenshotPlan {
    /// The file for frame `i`: the path itself for a single frame, `<stem>-<i>.<ext>` otherwise.
    fn path(&self, i: u32) -> PathBuf {
        if self.frames <= 1 {
            return self.path.clone();
        }
        let stem = self.path.file_stem().unwrap_or_default().to_string_lossy();
        let ext = self.path.extension().unwrap_or_default().to_string_lossy();
        self.path.with_file_name(format!("{stem}-{i}.{ext}"))
    }
}

/// How long the window waits, once closed, for the bridge to deliver what is still queued.
const BRIDGE_EXIT_WAIT: Duration = Duration::from_secs(3);

pub fn run(launch: Launch) {
    let bridge = BridgeSlot::default();
    let mut app = App::new();
    app.add_plugins(
        DefaultPlugins
            .build()
            .disable::<LogPlugin>()
            .set(WindowPlugin {
                primary_window: Some(window()),
                // The window asks before closing review tabs with a draft (`screens::review::leave`).
                close_when_requested: false,
                ..default()
            }),
    )
    .insert_resource(StartTarget(launch.target.clone()))
    .add_plugins((
        TabNavigationPlugin,
        FontsPlugin,
        crate::theme::ThemePlugin,
        crate::ui::kit::KitPlugin,
        crate::ui::emoji::EmojiPlugin,
        crate::nav::NavPlugin,
        crate::review_state::ReviewStatePlugin,
        crate::screens::home::HomePlugin,
        crate::screens::first_run::FirstRunPlugin,
        crate::screens::config::ConfigPlugin,
        crate::screens::review::ReviewPlugin,
        crate::screens::open_pr::OpenPrPlugin,
    ))
    .insert_resource(if launch.screenshot.is_some() {
        WinitSettings::continuous()
    } else {
        WinitSettings::desktop_app()
    })
    .insert_resource(AppPaths(launch.paths.clone()))
    .insert_resource(Clock::default())
    .insert_resource(Theme::new(false, 13, Density::Comfortable))
    .insert_resource(ClearColor(LIGHT.bg))
    .add_plugins(crate::bridge::BridgePlugin {
        mode: launch.mode.clone(),
        paths: launch.paths.clone(),
        home: launch.home.clone(),
        thread: bridge.clone(),
    })
    .add_systems(Startup, |mut commands: Commands| {
        commands.spawn(Camera2d);
    });
    if let Some(scene) = launch.scene {
        app.add_plugins(crate::scenes::ScenePlugin(scene));
    }
    if let Some(path) = &launch.screenshot {
        app.insert_resource(ScreenshotPlan {
            path: path.clone(),
            after_frames: 20,
            frames: launch.frames.max(1),
        })
        .add_systems(Update, take_screenshot);
        if let Some(step) = launch.frame_step {
            app.insert_resource(bevy::time::TimeUpdateStrategy::ManualDuration(step));
        }
    }
    app.run();
    // Leaving the window sends `CloseReview` / `Discard` for its tabs and exits in the same
    // frame. The app (and its `Asks` sender) is gone now, so the bridge answers what is queued
    // and ends; wait for it, so those choices reach the daemon before the process exits.
    if let Some(thread) = bridge.take()
        && !thread.finish(BRIDGE_EXIT_WAIT)
    {
        tracing::warn!("clusiad did not answer the last requests before the window closed");
    }
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

/// Waits for fonts and layout to settle, saves the frames, and exits once they are written.
fn take_screenshot(
    mut commands: Commands,
    plan: Res<ScreenshotPlan>,
    mut frame: Local<u32>,
    mut exit: MessageWriter<AppExit>,
) {
    *frame += 1;
    if let Some(i) = frame.checked_sub(plan.after_frames)
        && i < plan.frames
    {
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(plan.path(i)));
    }
    if *frame == plan.after_frames + plan.frames + 30 {
        exit.write(AppExit::Success);
    }
}
