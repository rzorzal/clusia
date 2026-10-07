//! The Emoji popover: a search field, the recently used emoji, then every group. Picking one
//! puts the character at the cursor of the composer's text and remembers it.
//!
//! The groups are built one per frame so opening the popover never stalls on the whole table;
//! a search shows its matches at once. Emoji are drawn as Twemoji images so they look the same
//! everywhere.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::InputFocus;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::{EditableText, FontCx, LayoutCx};
use bevy::ui_widgets::{Activate, Button as WidgetButton, ScrollArea, observe};
use bevy::window::RequestRedraw;

use crate::bridge::{Ask, Asks, Connection, Model};
use crate::fonts::UiFonts;
use crate::theme::Swatch;
use crate::ui::composer::popover::{Popovers, hinted_field, insert_into_area, sync_popovers};
use crate::ui::composer::{ComposerArea, ComposerKey};
use crate::ui::emoji::{EMOJI, EmojiEntry, EmojiImages, Group, by_group, search};
use crate::ui::kit::{Clickable, Fill, HoverFill, Type, text};

/// How many emoji the "Frequently used" row remembers.
pub const MAX_RECENT: usize = 16;

/// How many matches a search shows.
pub const RESULT_CAP: usize = 96;

/// The config key the recent emoji are kept under.
pub const RECENT_KEY: &str = "composer.recent_emoji";

/// The search field of the Emoji popover.
#[derive(Component)]
pub struct EmojiSearch;

/// One emoji in the grid.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct EmojiCell {
    pub key: ComposerKey,
    pub emoji: &'static str,
}

#[derive(Component)]
struct EmojiBody {
    key: ComposerKey,
    search: Entity,
    built: Option<String>,
    groups_done: usize,
}

/// `emoji` moved to the front of `recent`, without repeats, at most `MAX_RECENT` long.
pub fn push_recent(recent: &[String], emoji: &str) -> Vec<String> {
    std::iter::once(emoji.to_string())
        .chain(recent.iter().filter(|e| *e != emoji).cloned())
        .take(MAX_RECENT)
        .collect()
}

fn entry_of(emoji: &str) -> Option<&'static EmojiEntry> {
    EMOJI.iter().find(|e| e.emoji == emoji)
}

pub fn emoji_panel(p: &mut ChildSpawnerCommands, fonts: &UiFonts, key: &ComposerKey) {
    let search = hinted_field(p, fonts, "", "Search: turtle, ship, eyes…", EmojiSearch);
    p.spawn((
        Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(6),
            max_height: px(264),
            min_height: px(0),
            overflow: Overflow::scroll_y(),
            ..default()
        },
        ScrollArea,
        EmojiBody {
            key: key.clone(),
            search,
            built: None,
            groups_done: 0,
        },
    ));
    p.spawn(text(
        fonts,
        "Twemoji, so they look the same on every Mac",
        Type::META,
    ));
}

pub struct EmojiPickerPlugin;

impl Plugin for EmojiPickerPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, fill_emoji.after(sync_popovers));
    }
}

type Pictures<'a> = Option<(&'a mut EmojiImages, &'a mut Assets<Image>)>;

fn grid<'a>(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    key: &ComposerKey,
    entries: impl Iterator<Item = &'a EmojiEntry>,
    pictures: &mut Pictures,
) {
    p.spawn(Node {
        flex_wrap: FlexWrap::Wrap,
        column_gap: px(2),
        row_gap: px(2),
        ..default()
    })
    .with_children(|g| {
        for entry in entries {
            let mut cell = g.spawn((
                Node {
                    width: px(34),
                    height: px(34),
                    align_items: AlignItems::Center,
                    justify_content: JustifyContent::Center,
                    border_radius: BorderRadius::all(px(6)),
                    ..default()
                },
                (WidgetButton, Clickable, Hovered::default()),
                BackgroundColor::default(),
                Fill(Swatch::Clear),
                HoverFill(Swatch::Hover),
                EmojiCell {
                    key: key.clone(),
                    emoji: entry.emoji,
                },
                observe(on_emoji_cell),
            ));
            match pictures {
                Some((emoji, images)) => {
                    let handle = emoji.get(entry.file, images);
                    cell.with_children(|c| {
                        c.spawn((
                            Node {
                                width: px(24),
                                height: px(24),
                                ..default()
                            },
                            ImageNode::new(handle),
                        ));
                    });
                }
                None => {
                    cell.with_children(|c| {
                        c.spawn(text(fonts, entry.emoji, Type::BODY));
                    });
                }
            }
        }
    });
}

fn heading(p: &mut ChildSpawnerCommands, fonts: &UiFonts, label: &str) {
    p.spawn(text(fonts, label.to_string(), Type::META));
}

/// Draws what the search field asks for: the recent emoji and the groups (one more group each
/// frame), or the matches of a search.
fn fill_emoji(
    mut commands: Commands,
    fonts: Res<UiFonts>,
    model: Res<Model>,
    emoji: Option<ResMut<EmojiImages>>,
    images: Option<ResMut<Assets<Image>>>,
    fields: Query<&EditableText, With<EmojiSearch>>,
    mut bodies: Query<(Entity, &mut EmojiBody)>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let mut emoji = emoji;
    let mut images = images;
    for (entity, mut body) in &mut bodies {
        let query = fields
            .get(body.search)
            .map(|f| f.value().to_string().trim().to_string())
            .unwrap_or_default();
        let mut pictures: Pictures = match (emoji.as_deref_mut(), images.as_deref_mut()) {
            (Some(e), Some(i)) => Some((e, i)),
            _ => None,
        };
        let key = body.key.clone();
        if body.built.as_ref() != Some(&query) {
            body.built = Some(query.clone());
            body.groups_done = 0;
            commands
                .entity(entity)
                .despawn_related::<Children>()
                .with_children(|p| {
                    if query.is_empty() {
                        let recent: Vec<&EmojiEntry> = model
                            .snapshot
                            .config
                            .composer
                            .recent_emoji
                            .iter()
                            .filter_map(|e| entry_of(e))
                            .collect();
                        if !recent.is_empty() {
                            heading(p, &fonts, "Frequently used");
                            grid(p, &fonts, &key, recent.into_iter(), &mut pictures);
                        }
                    } else {
                        let found = search(&query);
                        if found.is_empty() {
                            p.spawn(text(fonts.as_ref(), "No emoji match", Type::MUTED));
                        } else {
                            grid(
                                p,
                                &fonts,
                                &key,
                                found.into_iter().take(RESULT_CAP),
                                &mut pictures,
                            );
                        }
                    }
                });
            redraw.write(RequestRedraw);
            continue;
        }
        if query.is_empty() && body.groups_done < Group::PICKER.len() {
            let group = Group::PICKER[body.groups_done];
            body.groups_done += 1;
            if by_group(group).next().is_some() {
                commands.entity(entity).with_children(|p| {
                    heading(p, &fonts, group.label());
                    grid(p, &fonts, &key, by_group(group), &mut pictures);
                });
            }
            redraw.write(RequestRedraw);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn on_emoji_cell(
    activate: On<Activate>,
    cells: Query<&EmojiCell>,
    mut areas: Query<(Entity, &ComposerArea, &mut EditableText)>,
    mut fonts: ResMut<FontCx>,
    mut layout: ResMut<LayoutCx>,
    mut focus: ResMut<InputFocus>,
    mut popovers: ResMut<Popovers>,
    model: Res<Model>,
    mut asks: ResMut<Asks>,
) {
    let Ok(cell) = cells.get(activate.entity) else {
        return;
    };
    insert_into_area(
        &cell.key,
        cell.emoji,
        &mut areas,
        &mut fonts,
        &mut layout,
        &mut focus,
        &mut popovers,
    );
    if model.connection == Connection::Live {
        let recent = push_recent(&model.snapshot.config.composer.recent_emoji, cell.emoji);
        if let Ok(value) = serde_json::to_string(&recent) {
            asks.send(Ask::SetConfig {
                key: RECENT_KEY.into(),
                value,
            });
        }
    }
}
