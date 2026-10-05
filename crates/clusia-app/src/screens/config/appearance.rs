//! Config › Appearance: theme (with previews), code size, density, default Diff view.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui_widgets::Button as WidgetButton;
use clusia_core::config::Theme as ThemeChoice;
use clusia_core::{Density, DiffView};

use super::{heading, page_header, segment_row, setter};
use crate::fonts::UiFonts;
use crate::snapshot::Snapshot;
use crate::theme::Swatch;
use crate::ui::kit::{Stroke, Type, card, panel, text};

#[derive(Debug, Clone, PartialEq)]
pub struct AppearanceView {
    pub theme: ThemeChoice,
    pub code_size: u8,
    pub density: Density,
    pub diff_view: DiffView,
}

pub fn view(snap: &Snapshot) -> AppearanceView {
    let a = &snap.config.appearance;
    AppearanceView {
        theme: a.theme,
        code_size: a.code_size,
        density: a.density,
        diff_view: a.diff_view,
    }
}

pub fn theme_value(t: ThemeChoice) -> &'static str {
    match t {
        ThemeChoice::Light => "light",
        ThemeChoice::Dark => "dark",
        ThemeChoice::System => "system",
    }
}

pub fn build(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &AppearanceView) {
    page_header(
        p,
        fonts,
        "Appearance",
        "How Clúsia looks. The menu bar popover always follows macOS.",
    );
    heading(p, fonts, "Theme");
    p.spawn(Node {
        column_gap: px(14),
        ..default()
    })
    .with_children(|r| {
        for (label, value, hint, halves) in [
            ("Light", "light", "", [false, false]),
            ("Dark", "dark", "", [true, true]),
            ("System", "system", "follows macOS, Auto too", [false, true]),
        ] {
            theme_card(
                r,
                fonts,
                label,
                value,
                hint,
                halves,
                theme_value(v.theme) == value,
            );
        }
    });
    heading(p, fonts, "Reading code");
    segment_row(
        p,
        fonts,
        "Code size",
        "appearance.code_size",
        &[("12", "12"), ("13", "13"), ("14", "14"), ("16", "16")],
        &v.code_size.to_string(),
        "Diffs, previews and code blocks",
    );
    segment_row(
        p,
        fonts,
        "Density",
        "appearance.density",
        &[("Comfortable", "comfortable"), ("Compact", "compact")],
        match v.density {
            Density::Comfortable => "comfortable",
            Density::Compact => "compact",
        },
        "Applies from the review screen",
    );
    segment_row(
        p,
        fonts,
        "Open the Diff as",
        "appearance.diff_view",
        &[("Unified", "unified"), ("Split", "split")],
        match v.diff_view {
            DiffView::Unified => "unified",
            DiffView::Split => "split",
        },
        "Each review can switch",
    );
    heading(p, fonts, "Preview");
    let code = Type::MONO.ink(Swatch::Fg).size(f32::from(v.code_size));
    p.spawn(card(Node {
        width: px(640),
        flex_direction: FlexDirection::Column,
        padding: UiRect::vertical(px(8)),
        ..default()
    }))
    .with_children(|c| {
        for (n, line, added) in [
            ("43", "    }", false),
            (
                "44",
                "    let _guard = self.refresh_lock.lock().await;",
                true,
            ),
            (
                "45",
                "    let fresh = self.client.exchange(&token.refresh).await?;",
                false,
            ),
        ] {
            c.spawn(panel(
                Node {
                    column_gap: px(10),
                    padding: UiRect::horizontal(px(10)),
                    ..default()
                },
                if added {
                    Swatch::AddedBg
                } else {
                    Swatch::Clear
                },
            ))
            .with_children(|l| {
                l.spawn(text(fonts, n, code.ink(Swatch::Faint)));
                l.spawn(text(fonts, line, code));
            });
        }
    });
}

fn theme_card(
    r: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    label: &str,
    value: &'static str,
    hint: &str,
    halves: [bool; 2],
    on: bool,
) {
    r.spawn((
        Node {
            width: px(200),
            flex_direction: FlexDirection::Column,
            row_gap: px(8),
            ..default()
        },
        WidgetButton,
        Hovered::default(),
        TabIndex(0),
        setter("appearance.theme", value),
    ))
    .with_children(|c| {
        c.spawn((
            Node {
                height: px(112),
                border: px(if on { 2 } else { 1 }).all(),
                border_radius: BorderRadius::all(px(8)),
                overflow: Overflow::clip(),
                ..default()
            },
            BorderColor::default(),
            Stroke(if on { Swatch::Green } else { Swatch::Line }),
        ))
        .with_children(|preview| {
            for dark in halves {
                preview
                    .spawn(panel(
                        Node {
                            flex_grow: 1.0,
                            flex_direction: FlexDirection::Column,
                            row_gap: px(6),
                            padding: px(12).all(),
                            ..default()
                        },
                        Swatch::PreviewBg(dark),
                    ))
                    .with_children(|half| {
                        for (h, w) in [(8, 60), (6, 80)] {
                            half.spawn(panel(
                                Node {
                                    height: px(h),
                                    width: percent(w),
                                    border_radius: BorderRadius::all(px(3)),
                                    ..default()
                                },
                                Swatch::PreviewInk(dark),
                            ));
                        }
                        half.spawn(panel(
                            Node {
                                height: px(18),
                                width: percent(50),
                                margin: UiRect::top(auto()),
                                border_radius: BorderRadius::all(px(4)),
                                ..default()
                            },
                            Swatch::PreviewGreen(dark),
                        ));
                    });
            }
        });
        c.spawn(Node {
            column_gap: px(8),
            align_items: AlignItems::Baseline,
            ..default()
        })
        .with_children(|l| {
            l.spawn(text(
                fonts,
                label.to_string(),
                if on { Type::STRONG } else { Type::BODY },
            ));
            if !hint.is_empty() {
                l.spawn(text(fonts, hint.to_string(), Type::META));
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_mirrors_the_config() {
        let mut snap = Snapshot::default();
        snap.config.appearance.code_size = 14;
        snap.config.appearance.density = Density::Compact;
        let v = view(&snap);
        assert_eq!(v.theme, ThemeChoice::System);
        assert_eq!(v.code_size, 14);
        assert_eq!(v.density, Density::Compact);
        assert_eq!(v.diff_view, DiffView::Unified);
        assert_eq!(theme_value(ThemeChoice::Dark), "dark");
    }
}
