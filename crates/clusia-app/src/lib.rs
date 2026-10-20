//! The Clúsia window (spec §7.1–7.2, §8): Home, Config, and the review screen.
// Bevy systems take many queries and parameters by design.
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

pub mod agent_name;
pub mod app;
pub mod args;
pub mod bridge;
pub mod clock;
pub mod fixture;
pub mod fonts;
pub mod instance;
pub mod logging;
pub mod nav;
pub mod platform_fonts;
pub mod platform_open;
pub mod platform_sound;
pub mod platform_window;
pub mod render_check;
pub mod review_state;
pub mod scenes;
pub mod screens;
pub mod snapshot;
pub mod tab_memory;
pub mod theme;
pub mod ui;
pub mod window_frame;

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod overflow;

/// How the window introduces itself to the daemon.
pub const CLIENT_NAME: &str = "clusia-app";
