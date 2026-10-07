//! Config › Notifications: the Do not disturb toggle and read-only poll interval.

use std::collections::HashMap;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;

use super::{page_header, row, setter};
use crate::fonts::UiFonts;
use crate::snapshot::Snapshot;
use crate::theme::Swatch;
use crate::ui::kit::{Type, card, text, toggle};
#[derive(Debug, Clone, PartialEq)]
pub struct NotificationsView {
    pub dnd: bool,
    pub dnd_error: Option<String>,
    pub poll_secs: u64,
}

pub fn view(snap: &Snapshot, rejected: &HashMap<String, String>) -> NotificationsView {
    NotificationsView {
        dnd: snap.config.notifications.dnd.enabled,
        dnd_error: rejected.get("notifications.dnd.enabled").cloned(),
        poll_secs: snap.config.github.poll_interval_secs,
    }
}

pub fn build(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &NotificationsView) {
    page_header(
        p,
        fonts,
        "Notifications",
        "What reaches you, and where. The tray dot always shows the total.",
    );
    row(
        p,
        fonts,
        "Do not disturb",
        |r| {
            r.spawn((
                toggle(v.dnd),
                setter("notifications.dnd.enabled", (!v.dnd).to_string()),
            ));
        },
        "No macOS notifications or sounds; the tray still counts",
        v.dnd_error
            .as_deref()
            .map(|m| ("notifications.dnd.enabled", m)),
    );
    row(
        p,
        fonts,
        "Checks GitHub every",
        |r| {
            r.spawn(text(
                fonts,
                format!("{} seconds (Git server)", v.poll_secs),
                Type::BODY,
            ));
        },
        "",
        None,
    );
    p.spawn(card(Node {
        padding: UiRect::axes(px(16), px(12)),
        max_width: px(640),
        ..default()
    }))
    .with_children(|c| {
        c.spawn(text(
            fonts,
            "Per-event choices (tray, macOS, sound), the sound itself and a quiet-hours schedule arrive with notifications in M6 (#20).",
            Type::MUTED.ink(Swatch::Muted),
        ));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_reads_dnd_and_poll() {
        let mut snap = Snapshot::default();
        snap.config.notifications.dnd.enabled = true;
        let v = view(&snap, &HashMap::new());
        assert!(v.dnd);
        assert_eq!(v.poll_secs, 60);
        assert_eq!(v.dnd_error, None);
    }
}
