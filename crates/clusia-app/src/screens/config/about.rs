//! Config › About: the version, the licence and the credits the licences ask for.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;

use super::{heading, link, page_header, row};
use crate::fonts::UiFonts;
use crate::snapshot::Snapshot;
use crate::theme::Swatch;
use crate::ui::kit::{Type, card, divider, text};

/// `(name, what it is used for, licence, home page)`.
pub const CREDITS: [(&str, &str, &str, &str); 5] = [
    (
        "Twemoji",
        "Emoji graphics, version 17.0.3 (jdecked/twemoji)",
        "CC-BY 4.0",
        "https://github.com/jdecked/twemoji",
    ),
    (
        "emojibase",
        "Emoji names, tags and shortcodes",
        "MIT",
        "https://github.com/milesj/emojibase",
    ),
    (
        "Inter",
        "The interface typeface",
        "SIL Open Font License 1.1",
        "https://rsms.me/inter/",
    ),
    (
        "JetBrains Mono",
        "The code typeface",
        "SIL Open Font License 1.1",
        "https://www.jetbrains.com/lp/mono/",
    ),
    (
        "GIPHY",
        "GIF search. Powered by GIPHY.",
        "GIPHY terms of service",
        "https://giphy.com/",
    ),
];

#[derive(Debug, Clone, PartialEq)]
pub struct AboutView {
    pub version: String,
    pub daemon: String,
}

pub fn view(snap: &Snapshot) -> AboutView {
    AboutView {
        version: env!("CARGO_PKG_VERSION").to_string(),
        daemon: if snap.daemon_version.is_empty() {
            "not connected".to_string()
        } else {
            snap.daemon_version.clone()
        },
    }
}

pub fn build(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &AboutView) {
    page_header(
        p,
        fonts,
        "About",
        "Clúsia reads your pull requests and writes a review only when you press Publish.",
    );
    row(
        p,
        fonts,
        "Version",
        |r| {
            r.spawn(text(fonts, v.version.clone(), Type::BODY));
        },
        "",
        None,
    );
    row(
        p,
        fonts,
        "Daemon",
        |r| {
            r.spawn(text(fonts, v.daemon.clone(), Type::BODY));
        },
        "",
        None,
    );
    row(
        p,
        fonts,
        "Licence",
        |r| {
            r.spawn(text(fonts, "Apache License 2.0", Type::BODY));
        },
        "",
        None,
    );
    heading(p, fonts, "Credits");
    p.spawn(card(Node {
        max_width: px(720),
        flex_direction: FlexDirection::Column,
        ..default()
    }))
    .with_children(|c| {
        for (i, (name, what, licence, url)) in CREDITS.iter().enumerate() {
            if i > 0 {
                c.spawn(divider());
            }
            c.spawn(Node {
                padding: UiRect::axes(px(16), px(10)),
                column_gap: px(12),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|r| {
                r.spawn(Node {
                    flex_grow: 1.0,
                    flex_direction: FlexDirection::Column,
                    row_gap: px(2),
                    ..default()
                })
                .with_children(|t| {
                    t.spawn(text(fonts, name.to_string(), Type::STRONG));
                    t.spawn(text(fonts, format!("{what} · {licence}"), Type::MUTED));
                });
                r.spawn(link(fonts, "Open", url));
            });
        }
    });
    p.spawn(text(
        fonts,
        "Twemoji graphics © Twitter, Inc. and other contributors, used under CC-BY 4.0.",
        Type::META.ink(Swatch::Faint),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::testing::NOW;

    #[test]
    fn view_names_both_versions() {
        let v = view(&fixture::demo(NOW));
        assert_eq!(v.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(v.daemon, "demo");
        let mut snap = fixture::demo(NOW);
        snap.daemon_version.clear();
        assert_eq!(view(&snap).daemon, "not connected");
    }

    #[test]
    fn credits_carry_the_attributions() {
        let names: Vec<&str> = CREDITS.iter().map(|c| c.0).collect();
        assert_eq!(
            names,
            ["Twemoji", "emojibase", "Inter", "JetBrains Mono", "GIPHY"]
        );
        let twemoji = CREDITS[0];
        assert_eq!(twemoji.2, "CC-BY 4.0");
        assert!(CREDITS.iter().all(|c| c.3.starts_with("https://")));
    }
}
