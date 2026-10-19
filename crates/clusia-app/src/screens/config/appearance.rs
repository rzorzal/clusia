//! Config › Appearance: theme (with previews), the text font and size, code size, density,
//! default Diff view.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy::ui_widgets::{Button as WidgetButton, ScrollArea};
use clusia_core::config::{INTER, SYSTEM_FONT, Theme as ThemeChoice};
use clusia_core::{Density, DiffView, TextSize};

use super::{heading, page_header, segment_row, setter};
use crate::bridge::Model;
use crate::fonts::{InstalledFonts, UiFonts, Unusable};
use crate::snapshot::Snapshot;
use crate::theme::Swatch;
use crate::ui::kit::{Clickable, Fill, HoverFill, Stroke, Type, card, panel, text, text_field};

#[derive(Debug, Clone, PartialEq)]
pub struct AppearanceView {
    pub theme: ThemeChoice,
    pub code_size: u8,
    pub density: Density,
    pub diff_view: DiffView,
    // The configured family is not here: the font list shows it (`FontListPart`), so choosing
    // one leaves the page, its search and the list's scroll in place.
    pub text_size: TextSize,
    /// The configured family when it cannot be used, and why.
    pub missing_font: Option<(String, Unusable)>,
}

pub fn view(snap: &Snapshot, missing_font: Option<&(String, Unusable)>) -> AppearanceView {
    let a = &snap.config.appearance;
    AppearanceView {
        theme: a.theme,
        code_size: a.code_size,
        density: a.density,
        diff_view: a.diff_view,
        text_size: a.text_size,
        missing_font: missing_font.cloned(),
    }
}

pub fn text_size_value(size: TextSize) -> &'static str {
    match size {
        TextSize::Small => "small",
        TextSize::Medium => "medium",
        TextSize::Large => "large",
    }
}

/// The segment of *Open the Diff as* a stored view selects; Story and Calls select none.
pub fn diff_view_value(view: DiffView) -> &'static str {
    match view {
        DiffView::Unified => "unified",
        DiffView::Split => "split",
        DiffView::Story | DiffView::Calls => "",
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
    heading(p, fonts, "Text");
    font_row(p, fonts, v);
    segment_row(
        p,
        fonts,
        "Text size",
        "appearance.text_size",
        &[("Small", "small"), ("Medium", "medium"), ("Large", "large")],
        text_size_value(v.text_size),
        "Every text, code too",
    );
    heading(p, fonts, "Reading code");
    segment_row(
        p,
        fonts,
        "Code size",
        "appearance.code_size",
        &[("12", "12"), ("13", "13"), ("14", "14"), ("16", "16")],
        &v.code_size.to_string(),
        "Diffs, previews and code blocks, at the medium text size",
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
        diff_view_value(v.diff_view),
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

/// The search field over the font list.
#[derive(Component, Debug)]
pub struct FontSearch;

/// The font list, rebuilt on its own as the search or the installed families change.
#[derive(Component, Debug, Default)]
pub struct FontListPart {
    built: Option<FontList>,
}

/// What the font search holds.
#[derive(Resource, Debug, Default)]
pub struct FontQuery(pub String);

/// The rows the list shows at most; a search narrows the rest.
pub const MAX_FONT_ROWS: usize = 60;

/// The families the list offers for `query`: Inter and the system font first, then the
/// installed families (`None` while they are being read), each once.
#[derive(Debug, Clone, PartialEq)]
pub struct FontList {
    pub rows: Vec<String>,
    /// Matches past `MAX_FONT_ROWS`.
    pub more: usize,
    /// The installed families are still being read.
    pub reading: bool,
    pub current: String,
}

pub fn font_list(installed: Option<&[String]>, query: &str, current: &str) -> FontList {
    let query = query.trim().to_lowercase();
    let mut seen: Vec<String> = Vec::new();
    let mut matches: Vec<String> = Vec::new();
    let all = [INTER, SYSTEM_FONT]
        .into_iter()
        .chain(installed.unwrap_or_default().iter().map(String::as_str));
    for name in all {
        let key = name.to_lowercase();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key.clone());
        if key.contains(&query) {
            matches.push(name.to_string());
        }
    }
    let more = matches.len().saturating_sub(MAX_FONT_ROWS);
    matches.truncate(MAX_FONT_ROWS);
    FontList {
        rows: matches,
        more,
        reading: installed.is_none(),
        current: current.to_string(),
    }
}

/// What a row says beside its name.
fn font_note(name: &str) -> &'static str {
    match name {
        INTER => "Bundled · default",
        SYSTEM_FONT => "The macOS system font",
        _ => "",
    }
}

/// The *Text font* row: the search, the list under it and, for a family this Mac does not
/// have, why the text shows in Inter.
fn font_row(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &AppearanceView) {
    p.spawn(Node {
        column_gap: px(16),
        align_items: AlignItems::Start,
        ..default()
    })
    .with_children(|r| {
        r.spawn((
            Node {
                width: px(190),
                flex_shrink: 0.0,
                padding: UiRect::top(px(6)),
                ..default()
            },
            children![text(fonts, "Text font", Type::MUTED)],
        ));
        r.spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(8),
            ..default()
        })
        .with_children(|c| {
            c.spawn(Node {
                column_gap: px(10),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|s| {
                s.spawn((text_field(fonts, "", 300.0, false), FontSearch));
                s.spawn(text(fonts, "Search the installed fonts", Type::META));
            });
            c.spawn((
                card(Node {
                    width: px(420),
                    max_height: px(232),
                    flex_direction: FlexDirection::Column,
                    padding: UiRect::vertical(px(4)),
                    overflow: Overflow::scroll_y(),
                    ..default()
                }),
                ScrollArea,
                FontListPart::default(),
            ));
            if let Some((missing, why)) = &v.missing_font {
                let line = match why {
                    Unusable::NotInstalled => {
                        format!("“{missing}” is not installed on this Mac, so text shows in Inter.")
                    }
                    Unusable::Unreadable => {
                        format!("“{missing}” could not be read, so text shows in Inter.")
                    }
                };
                c.spawn((
                    text(fonts, line, Type::BODY.ink(Swatch::Orange)),
                    MissingFont,
                ));
            }
        });
    });
}

/// The note that the configured family is not installed.
#[derive(Component, Debug)]
pub struct MissingFont;

/// One family of the list; activating it writes `appearance.text_font`.
fn font_rows(p: &mut ChildSpawnerCommands, fonts: &UiFonts, list: &FontList) {
    for name in &list.rows {
        let on = *name == list.current;
        p.spawn((
            Node {
                padding: UiRect::axes(px(12), px(6)),
                column_gap: px(10),
                align_items: AlignItems::Center,
                flex_shrink: 0.0,
                ..default()
            },
            WidgetButton,
            Hovered::default(),
            TabIndex(0),
            Clickable,
            BackgroundColor::default(),
            Fill(if on { Swatch::Selected } else { Swatch::Clear }),
            HoverFill(if on { Swatch::Selected } else { Swatch::Hover }),
            setter("appearance.text_font", name.clone()),
        ))
        .with_children(|r| {
            r.spawn(text(
                fonts,
                name.clone(),
                if on { Type::STRONG } else { Type::BODY },
            ));
            let note = font_note(name);
            if !note.is_empty() {
                r.spawn(text(fonts, note, Type::META));
            }
        });
    }
    let footer = if list.reading {
        Some("Reading the installed fonts…".to_string())
    } else if list.more > 0 {
        Some(format!("{} more · search to narrow the list", list.more))
    } else if list.rows.is_empty() {
        Some("No installed font matches.".to_string())
    } else {
        None
    };
    if let Some(footer) = footer {
        p.spawn(Node {
            padding: UiRect::axes(px(12), px(6)),
            flex_shrink: 0.0,
            ..default()
        })
        .with_children(|f| {
            f.spawn(text(fonts, footer, Type::META));
        });
    }
}

/// A new search field starts empty, so the query does too.
pub(super) fn live_font_query(
    added: Query<(), Added<FontSearch>>,
    fields: Query<&EditableText, (With<FontSearch>, Changed<EditableText>)>,
    mut query: ResMut<FontQuery>,
) {
    if !added.is_empty() && !query.0.is_empty() {
        query.0.clear();
    }
    for editable in &fields {
        let value = editable.value().to_string();
        if value != query.0 {
            query.0 = value;
        }
    }
}

pub(super) fn rebuild_font_list(
    mut commands: Commands,
    query: Res<FontQuery>,
    installed: Res<InstalledFonts>,
    model: Res<Model>,
    fonts: Res<UiFonts>,
    mut parts: Query<(Entity, &mut FontListPart)>,
) {
    let changed = query.is_changed() || installed.is_changed() || model.is_changed();
    if !changed && parts.iter().all(|(_, part)| part.built.is_some()) {
        return;
    }
    for (entity, mut part) in &mut parts {
        let list = font_list(
            installed.families.as_deref(),
            &query.0,
            &model.snapshot.config.appearance.text_font,
        );
        if part.built.as_ref() == Some(&list) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        let fonts = &*fonts;
        let rows = list.clone();
        commands
            .entity(entity)
            .with_children(|p| font_rows(p, fonts, &rows));
        part.built = Some(list);
    }
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

    fn families(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn the_font_list_leads_with_inter_and_the_system_font_and_searches_the_rest() {
        let installed = families(&["Avenir Next", "Helvetica Neue", "inter", "Menlo", "SF Pro"]);
        let all = font_list(Some(&installed), "", "Menlo");
        assert_eq!(
            all.rows,
            ["Inter", "SF Pro", "Avenir Next", "Helvetica Neue", "Menlo"],
            "each name once"
        );
        assert_eq!(
            (all.more, all.reading, all.current.as_str()),
            (0, false, "Menlo")
        );
        let found = font_list(Some(&installed), "  NEUE ", "Inter");
        assert_eq!(found.rows, ["Helvetica Neue"]);
        assert!(font_list(Some(&installed), "zapf", "Inter").rows.is_empty());
        let reading = font_list(None, "", "Inter");
        assert_eq!(reading.rows, ["Inter", "SF Pro"]);
        assert!(reading.reading);
        let many: Vec<String> = (0..100).map(|i| format!("Family {i:03}")).collect();
        let capped = font_list(Some(&many), "family", "Inter");
        assert_eq!(capped.rows.len(), MAX_FONT_ROWS);
        assert_eq!(capped.more, 100 - MAX_FONT_ROWS);
    }

    #[test]
    fn view_mirrors_the_config() {
        let mut snap = Snapshot::default();
        snap.config.appearance.code_size = 14;
        snap.config.appearance.density = Density::Compact;
        snap.config.appearance.text_size = TextSize::Large;
        let missing = ("Nope Sans".to_string(), Unusable::NotInstalled);
        let v = view(&snap, Some(&missing));
        assert_eq!(v.text_size, TextSize::Large);
        assert_eq!(v.missing_font, Some(missing));
        let mut other = snap.clone();
        other.config.appearance.text_font = "Menlo".into();
        assert_eq!(
            view(&other, None),
            view(&snap, None),
            "the list shows the family; the page is the same"
        );
        assert_eq!(v.theme, ThemeChoice::System);
        assert_eq!(v.code_size, 14);
        assert_eq!(v.density, Density::Compact);
        assert_eq!(v.diff_view, DiffView::Unified);
        assert_eq!(theme_value(ThemeChoice::Dark), "dark");
    }

    #[test]
    fn story_and_calls_select_neither_segment_of_the_diff_row() {
        assert_eq!(diff_view_value(DiffView::Unified), "unified");
        assert_eq!(diff_view_value(DiffView::Split), "split");
        assert_eq!(diff_view_value(DiffView::Story), "");
        assert_eq!(diff_view_value(DiffView::Calls), "");
    }
}
