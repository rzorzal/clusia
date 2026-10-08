//! Config › General: whether Clúsia starts when you log in.

use std::collections::HashMap;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;

use super::{page_header, row, sends};
use crate::bridge::{Ask, START_AT_LOGIN};
use crate::fonts::UiFonts;
use crate::snapshot::Snapshot;
use crate::ui::kit::toggle;

#[derive(Debug, Clone, PartialEq)]
pub struct GeneralView {
    pub start_at_login: bool,
    pub error: Option<String>,
}

pub fn view(snap: &Snapshot, rejected: &HashMap<String, String>) -> GeneralView {
    GeneralView {
        start_at_login: snap.config.general.start_at_login,
        error: rejected.get(START_AT_LOGIN).cloned(),
    }
}

pub fn build(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &GeneralView) {
    page_header(
        p,
        fonts,
        "General",
        "How Clúsia starts. Quitting from the menu bar always stops it until you open it again.",
    );
    row(
        p,
        fonts,
        "Start at login",
        |r| {
            r.spawn((
                toggle(v.start_at_login),
                sends(Ask::SetStartAtLogin {
                    on: !v.start_at_login,
                }),
            ));
        },
        "The menu bar tray and the daemon start when you log in",
        v.error.as_deref().map(|m| (START_AT_LOGIN, m)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_view_reads_the_setting_and_its_refusal() {
        let mut snap = Snapshot::default();
        assert!(view(&snap, &HashMap::new()).start_at_login, "on by default");
        snap.config.general.start_at_login = false;
        let rejected = HashMap::from([(START_AT_LOGIN.to_string(), "cannot write".to_string())]);
        let v = view(&snap, &rejected);
        assert!(!v.start_at_login);
        assert_eq!(v.error.as_deref(), Some("cannot write"));
    }
}
