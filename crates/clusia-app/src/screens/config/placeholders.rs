//! Sections whose features belong to later sub-projects. The layout is final from day one
//! (spec §7.2), so the pages exist and say when they arrive.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;

use super::page_header;
use crate::fonts::UiFonts;
use crate::theme::Swatch;
use crate::ui::kit::{Type, card, text};

fn dimmed_cards(p: &mut ChildSpawnerCommands, fonts: &UiFonts, items: &[(&str, &str)]) {
    p.spawn(Node {
        column_gap: px(10),
        ..default()
    })
    .with_children(|r| {
        for (name, about) in items {
            r.spawn(card(Node {
                width: px(220),
                flex_direction: FlexDirection::Column,
                row_gap: px(4),
                padding: UiRect::axes(px(14), px(12)),
                ..default()
            }))
            .with_children(|c| {
                c.spawn(text(
                    fonts,
                    name.to_string(),
                    Type::STRONG.ink(Swatch::Faint),
                ));
                c.spawn(text(fonts, about.to_string(), Type::META));
            });
        }
    });
}

fn arrives(p: &mut ChildSpawnerCommands, fonts: &UiFonts, message: &str) {
    p.spawn(card(Node {
        padding: UiRect::axes(px(16), px(12)),
        max_width: px(680),
        ..default()
    }))
    .with_children(|c| {
        c.spawn(text(fonts, message.to_string(), Type::MUTED));
    });
}

pub fn plugins(p: &mut ChildSpawnerCommands, fonts: &UiFonts) {
    page_header(
        p,
        fonts,
        "Plugins & skills",
        "Skills run with your harness and write findings; renderers draw them in a tab you choose.",
    );
    dimmed_cards(
        p,
        fonts,
        &[
            ("house-rules", "Team checklist → Audits"),
            ("migration-check", "Locks in SQL → new tab"),
            ("lock-timeline", "Renderer for migrations"),
        ],
    );
    arrives(p, fonts, "Arrives with SP5 (#72).");
}
