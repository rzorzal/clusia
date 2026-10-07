//! The GIF popover: a search field on top (Giphy's trending GIFs while it is empty), a grid of
//! two columns of animated previews, "Powered by GIPHY" and a field to paste any GIF link.
//!
//! Searching is debounced and one search is in flight at a time, so an answer always belongs to
//! the last question. Without a Giphy key only the link field stays, with a line saying where to
//! add the key. Errors show as a line in the popover; the composer's text is not touched.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::InputFocus;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::{EditableText, FontCx, LayoutCx};
use bevy::ui_widgets::{Activate, Button as WidgetButton, ScrollArea, observe};
use bevy::window::RequestRedraw;
use clusia_protocol::{ErrorCode, GifItem};

use crate::bridge::{Ask, Asks, GifsArrived};
use crate::fonts::UiFonts;
use crate::theme::Swatch;
use crate::ui::composer::popover::{
    LinkField, PANEL_WIDTH, PopoverKind, Popovers, hinted_field, insert_into_area, note_line,
    sync_popovers,
};
use crate::ui::composer::{ComposerArea, ComposerKey};
use crate::ui::kit::{Clickable, Type, text};
use crate::ui::markdown::MdImage;

/// How long the search text must stay unchanged before it is sent.
pub const GIF_DEBOUNCE: f64 = 0.3;

/// The search field.
#[derive(Component)]
pub struct GifSearch;

/// One GIF in the grid.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct GifTile {
    pub key: ComposerKey,
    pub item: GifItem,
}

/// The search field and the grid; hidden without a Giphy key.
#[derive(Component)]
pub struct GifSearchBox;

/// The line shown instead of the search when there is no key.
#[derive(Component)]
pub struct GifNeedsKey;

#[derive(Component)]
struct GifGrid {
    key: ComposerKey,
    built: Option<GifState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum GifStatus {
    #[default]
    Idle,
    Loading,
    Ready,
    NotConfigured,
    Failed(String),
}

/// What the popover shows.
#[derive(Resource, Debug, Clone, PartialEq, Default)]
pub struct GifState {
    pub status: GifStatus,
    pub items: Vec<GifItem>,
    /// The search the items answer.
    pub query: String,
}

/// The search text over time and the questions asked.
#[derive(Resource, Debug, Default)]
pub struct GifTyping {
    typed: String,
    typed_at: f64,
    asked: Option<String>,
    outstanding: u32,
    /// Searches asked in a popover that has since closed and not yet answered. The worker
    /// answers in order, so the next `stale` answers are theirs and are dropped.
    stale: u32,
}

impl GifTyping {
    /// Pretends the text was last edited `secs` ago (tests).
    #[cfg(test)]
    pub fn edited_ago(&mut self, now: f64, secs: f64) {
        self.typed_at = now - secs;
    }
}

/// Whether to send the search now: it changed, has been still for `GIF_DEBOUNCE`, and no
/// question is waiting for its answer.
pub fn gif_search_due(
    asked: Option<&str>,
    typed: &str,
    typed_at: f64,
    outstanding: u32,
    now: f64,
) -> bool {
    outstanding == 0 && asked != Some(typed) && now - typed_at >= GIF_DEBOUNCE
}

/// What the popover says about a failed search.
pub fn gif_error_text(code: ErrorCode, message: &str) -> String {
    match code {
        ErrorCode::RateLimited => "Giphy is busy — try again in a moment".into(),
        ErrorCode::Offline => "Can't reach Giphy — check your connection".into(),
        _ => message.to_string(),
    }
}

fn tile_size(item: &GifItem) -> Vec2 {
    let width = (PANEL_WIDTH - 24.0 - 8.0) / 2.0;
    let ratio = if item.width == 0 {
        1.0
    } else {
        item.height as f32 / item.width as f32
    };
    Vec2::new(width, (width * ratio).clamp(60.0, 150.0))
}

pub fn gif_panel(p: &mut ChildSpawnerCommands, fonts: &UiFonts, key: &ComposerKey, seed: &str) {
    p.spawn((
        Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(8),
            ..default()
        },
        GifSearchBox,
    ))
    .with_children(|b| {
        hinted_field(b, fonts, seed, "Search GIFs", GifSearch);
        b.spawn((
            Node {
                flex_wrap: FlexWrap::Wrap,
                column_gap: px(8),
                row_gap: px(8),
                max_height: px(280),
                overflow: Overflow::scroll_y(),
                ..default()
            },
            ScrollArea,
            GifGrid {
                key: key.clone(),
                built: None,
            },
        ));
    });
    p.spawn((
        Node {
            display: Display::None,
            ..default()
        },
        GifNeedsKey,
        text(
            fonts,
            "Add a Giphy key in Config › Media to search GIFs.",
            Type::MUTED,
        ),
    ));
    p.spawn(text(fonts, "Powered by GIPHY", Type::META));
    hinted_field(p, fonts, "", "Paste a link", LinkField(key.clone()));
    note_line(p, fonts);
    p.spawn(text(
        fonts,
        "Or paste any GIF link. Clúsia fetches it for you; GitHub shows it in the review.",
        Type::META,
    ));
}

pub struct GifPickerPlugin;

impl Plugin for GifPickerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<GifState>()
            .init_resource::<GifTyping>()
            .add_message::<GifsArrived>()
            .add_systems(
                Update,
                (drive_gifs, fill_gifs, show_sections)
                    .chain()
                    .after(sync_popovers),
            );
    }
}

/// Sends the searches and takes the answers.
#[allow(clippy::too_many_arguments)]
fn drive_gifs(
    time: Res<Time>,
    popovers: Res<Popovers>,
    fields: Query<&EditableText, With<GifSearch>>,
    mut state: ResMut<GifState>,
    mut typing: ResMut<GifTyping>,
    mut asks: ResMut<Asks>,
    mut arrived: MessageReader<GifsArrived>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let now = time.elapsed_secs_f64();
    let open = matches!(&popovers.open, Some((_, PopoverKind::Gif)));
    for GifsArrived(answer) in arrived.read() {
        if typing.stale > 0 {
            typing.stale -= 1;
            continue;
        }
        typing.outstanding = typing.outstanding.saturating_sub(1);
        if typing.outstanding > 0 || !open || typing.asked.is_none() {
            continue;
        }
        let query = typing.asked.clone().unwrap_or_default();
        match answer {
            Ok(page) => {
                state.status = GifStatus::Ready;
                state.items = page.items.clone();
                state.query = query;
            }
            Err((ErrorCode::NotConfigured, _)) => {
                state.status = GifStatus::NotConfigured;
                state.items.clear();
            }
            Err((code, message)) => {
                state.status = GifStatus::Failed(gif_error_text(*code, message));
                state.items.clear();
            }
        }
    }
    if !open {
        if *state != GifState::default() {
            *state = GifState::default();
        }
        typing.asked = None;
        typing.typed.clear();
        // A search still unanswered must not block the next popover, and its late answer must
        // not be taken for the next popover's.
        typing.stale += typing.outstanding;
        typing.outstanding = 0;
        return;
    }
    if state.status == GifStatus::NotConfigured {
        return;
    }
    let typed = match fields.iter().next() {
        Some(field) => field.value().to_string().trim().to_string(),
        None if typing.asked.is_none() => popovers.gif_query.trim().to_string(),
        None => return,
    };
    if typing.asked.is_none() && typing.outstanding == 0 {
        typing.typed = typed.clone();
        typing.typed_at = f64::NEG_INFINITY;
    } else if typed != typing.typed {
        typing.typed = typed.clone();
        typing.typed_at = now;
    }
    if gif_search_due(
        typing.asked.as_deref(),
        &typed,
        typing.typed_at,
        typing.outstanding,
        now,
    ) {
        asks.send(Ask::SearchGifs {
            query: typed.clone(),
            offset: 0,
        });
        typing.asked = Some(typed);
        typing.outstanding += 1;
        if state.status != GifStatus::Loading {
            state.status = GifStatus::Loading;
        }
    } else if typing.asked.as_deref() != Some(&typed) {
        redraw.write(RequestRedraw);
    }
}

/// Whether `a` and `b` draw the same grid. Tiles do not depend on the status, so a search
/// going Ready → Loading → Ready with the same items keeps its tiles and their scroll position.
fn same_grid(a: &GifState, b: &GifState) -> bool {
    a == b || (!a.items.is_empty() && a.items == b.items && a.query == b.query)
}

/// Draws the grid for the current state.
fn fill_gifs(
    mut commands: Commands,
    fonts: Res<UiFonts>,
    state: Res<GifState>,
    mut grids: Query<(Entity, &mut GifGrid)>,
) {
    for (entity, mut grid) in &mut grids {
        if grid
            .built
            .as_ref()
            .is_some_and(|built| same_grid(built, &state))
        {
            continue;
        }
        let key = grid.key.clone();
        commands
            .entity(entity)
            .despawn_related::<Children>()
            .with_children(|p| match &state.status {
                GifStatus::Idle | GifStatus::Loading if state.items.is_empty() => {
                    p.spawn(text(fonts.as_ref(), "Searching…", Type::MUTED));
                }
                GifStatus::Failed(message) => {
                    p.spawn(text(
                        fonts.as_ref(),
                        message.clone(),
                        Type::BODY.ink(Swatch::Orange),
                    ));
                }
                _ if state.items.is_empty() => {
                    let said = if state.query.is_empty() {
                        "No GIFs to show".to_string()
                    } else {
                        format!("No GIFs for “{}”", state.query)
                    };
                    p.spawn(text(fonts.as_ref(), said, Type::MUTED));
                }
                _ => {
                    for item in &state.items {
                        let size = tile_size(item);
                        p.spawn((
                            Node {
                                width: px(size.x),
                                height: px(size.y),
                                overflow: Overflow::clip(),
                                border_radius: BorderRadius::all(px(8)),
                                ..default()
                            },
                            (WidgetButton, Clickable, Hovered::default()),
                            BackgroundColor::default(),
                            crate::ui::kit::Fill(Swatch::Chrome),
                            GifTile {
                                key: key.clone(),
                                item: item.clone(),
                            },
                            observe(on_gif_tile),
                            children![(
                                Node {
                                    width: percent(100),
                                    height: percent(100),
                                    ..default()
                                },
                                MdImage(item.preview_url.clone()),
                            )],
                        ));
                    }
                }
            });
        grid.built = Some(state.clone());
    }
}

/// Without a key only the link field is left, with the line that says where to add one.
fn show_sections(
    state: Res<GifState>,
    mut search: Query<&mut Node, (With<GifSearchBox>, Without<GifNeedsKey>)>,
    mut needs_key: Query<&mut Node, (With<GifNeedsKey>, Without<GifSearchBox>)>,
) {
    let none = state.status == GifStatus::NotConfigured;
    let show = |on: bool| if on { Display::Flex } else { Display::None };
    for mut node in &mut search {
        if node.display != show(!none) {
            node.display = show(!none);
        }
    }
    for mut node in &mut needs_key {
        if node.display != show(none) {
            node.display = show(none);
        }
    }
}

/// The markdown a GIF goes into the text as: its title as the description.
pub fn gif_markdown(item: &GifItem) -> String {
    let title: String = item
        .title
        .chars()
        .map(|c| {
            if matches!(c, '[' | ']' | '\n' | '\r') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = if title.is_empty() { "GIF" } else { &title };
    format!("![{title}]({})", item.url)
}

#[allow(clippy::too_many_arguments)]
fn on_gif_tile(
    activate: On<Activate>,
    tiles: Query<&GifTile>,
    mut areas: Query<(Entity, &ComposerArea, &mut EditableText)>,
    mut fonts: ResMut<FontCx>,
    mut layout: ResMut<LayoutCx>,
    mut focus: ResMut<InputFocus>,
    mut popovers: ResMut<Popovers>,
) {
    let Ok(tile) = tiles.get(activate.entity) else {
        return;
    };
    insert_into_area(
        &tile.key,
        &gif_markdown(&tile.item),
        &mut areas,
        &mut fonts,
        &mut layout,
        &mut focus,
        &mut popovers,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(status: GifStatus, ids: &[&str], query: &str) -> GifState {
        GifState {
            status,
            items: ids
                .iter()
                .map(|id| GifItem {
                    id: (*id).into(),
                    title: String::new(),
                    preview_url: String::new(),
                    url: String::new(),
                    width: 1,
                    height: 1,
                })
                .collect(),
            query: query.into(),
        }
    }

    #[test]
    fn a_status_change_alone_keeps_the_tiles() {
        let ready = state(GifStatus::Ready, &["a", "b"], "cat");
        assert!(same_grid(
            &ready,
            &state(GifStatus::Loading, &["a", "b"], "cat")
        ));
        assert!(!same_grid(
            &ready,
            &state(GifStatus::Ready, &["a", "c"], "cat")
        ));
        assert!(!same_grid(
            &ready,
            &state(GifStatus::Ready, &["a", "b"], "dog")
        ));
        let none = state(GifStatus::Loading, &[], "");
        assert!(
            !same_grid(&none, &state(GifStatus::Ready, &[], "")),
            "the line changes"
        );
    }
}
