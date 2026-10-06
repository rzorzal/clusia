//! The review screen (spec §7.2, mockups `Review.png`, `Loading.png`, `WhatsNew.png`, …).
//! `nav` spawns an empty `ReviewScreen(pr)` container; `shell::mount` fills it according to the
//! tab's phase (opening or ready), and each region rebuilds only when its view changes.

use bevy::prelude::*;

use crate::nav::NavSystems;
use crate::review_state::open_new_tabs;

pub mod comments;
pub mod diff;
pub mod editor;
pub mod loading;
pub mod shell;
pub mod whats_new;

pub use shell::ModalFor;

/// The review screen's systems. Section fillers (Diff, Comments) run `.after(ReviewSystems)`,
/// so the regions they fill exist in the same frame.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReviewSystems;

pub struct ReviewPlugin;

impl Plugin for ReviewPlugin {
    fn build(&self, app: &mut App) {
        app.configure_sets(Update, ReviewSystems.after(NavSystems).after(open_new_tabs))
            .add_plugins((
                shell::ShellPlugin,
                comments::CommentsPlugin,
                diff::DiffPlugin,
                editor::EditorPlugin,
                loading::LoadingPlugin,
                whats_new::WhatsNewPlugin,
            ));
    }
}
