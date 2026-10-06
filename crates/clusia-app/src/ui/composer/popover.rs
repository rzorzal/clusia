//! The toolbar's popovers: Emoji, GIF and Image.
//!
//! A popover is a layer of its own at the root of the window, so no scroll area or modal clips
//! it; a transparent backdrop closes it on any click outside. It sits under the toolbar button
//! that opened it. Only one is open at a time and it closes when something is picked, when the
//! composer goes away, on Esc (which is consumed, so a modal behind stays open) and when the
//! composer switches to Preview. Whatever happens inside a popover — an error, a refused link —
//! the text of the composer is never touched except by the pick itself.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input::{ButtonInput, InputSystems};
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::input_focus::{FocusCause, InputFocus};
use bevy::picking::Pickable;
use bevy::picking::events::{Pointer, Press};
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::{EditableText, FontCx, LayoutCx};
use bevy::ui::{ComputedNode, UiGlobalTransform};
use bevy::ui_widgets::{Activate, Button as WidgetButton, observe};
use bevy::window::PrimaryWindow;

use crate::fonts::UiFonts;
use crate::review_state::ReviewTabs;
use crate::theme::Swatch;
use crate::ui::composer::toolbar::insert_at;
use crate::ui::composer::{
    Composer, ComposerArea, ComposerKey, ComposerMode, ExtraModes, mode_of, read_area, set_mode,
    write_area,
};
use crate::ui::kit::{Clickable, FieldCommitted, Fill, HoverFill, Stroke, Type, text, text_field};
use crate::ui::markdown::build::https_host;

/// Which popover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopoverKind {
    Emoji,
    Gif,
    Image,
}

impl PopoverKind {
    fn label(self) -> &'static str {
        match self {
            PopoverKind::Emoji => "☺ Emoji",
            PopoverKind::Gif => "GIF",
            PopoverKind::Image => "▣ Image",
        }
    }
}

/// The open popover, if any. Tests and scenes open one by setting `open`.
#[derive(Resource, Debug, Default, Clone, PartialEq)]
pub struct Popovers {
    pub open: Option<(ComposerKey, PopoverKind)>,
    /// The GIF search field starts with this text when the GIF popover opens.
    pub gif_query: String,
    /// The composer whose popover is open has been on screen at least once. Only then does its
    /// absence close the popover, so a popover staged before its composer is drawn waits for it.
    seen: Option<ComposerKey>,
}

impl Popovers {
    pub fn close(&mut self) {
        self.open = None;
        self.gif_query.clear();
        self.seen = None;
    }
}

/// The line a popover shows under its link field when a link is refused.
#[derive(Resource, Debug, Default, Clone, PartialEq, Eq)]
pub struct PopoverNote(pub Option<String>);

/// A toolbar button that opens `kind` for `key`.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PopoverButton {
    pub key: ComposerKey,
    pub kind: PopoverKind,
}

/// The window-sized layer of an open popover.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PopoverLayer {
    pub key: ComposerKey,
    pub kind: PopoverKind,
}

/// The card of an open popover.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PopoverPanel {
    pub key: ComposerKey,
}

/// A field that takes the link of an image or GIF.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct LinkField(pub ComposerKey);

#[derive(Component)]
struct NoteText;

/// A text node that shows while its field is empty.
#[derive(Component)]
pub(crate) struct Placeholder(pub Entity);

pub const PANEL_WIDTH: f32 = 340.0;

const LAYER_Z: i32 = 120;

/// The three toolbar buttons that open a popover.
pub fn popover_buttons(p: &mut ChildSpawnerCommands, fonts: &UiFonts, key: &ComposerKey) {
    for kind in [PopoverKind::Emoji, PopoverKind::Gif, PopoverKind::Image] {
        p.spawn((
            Node {
                height: px(26),
                padding: UiRect::horizontal(px(8)),
                align_items: AlignItems::Center,
                border_radius: BorderRadius::all(px(6)),
                ..default()
            },
            (WidgetButton, Clickable, Hovered::default(), TabIndex(0)),
            BackgroundColor::default(),
            Fill(Swatch::Clear),
            HoverFill(Swatch::Hover),
            PopoverButton {
                key: key.clone(),
                kind,
            },
            observe(on_popover_button),
            children![text(fonts, kind.label(), Type::MUTED.size(12.5))],
        ));
    }
}

fn on_popover_button(
    activate: On<Activate>,
    buttons: Query<&PopoverButton>,
    mut popovers: ResMut<Popovers>,
    mut tabs: ResMut<ReviewTabs>,
    mut extra: ResMut<ExtraModes>,
) {
    let Ok(button) = buttons.get(activate.entity) else {
        return;
    };
    let want = (button.key.clone(), button.kind);
    if popovers.open.as_ref() == Some(&want) {
        popovers.close();
    } else {
        set_mode(&button.key, ComposerMode::Write, &mut tabs, &mut extra);
        popovers.open = Some(want);
    }
}

/// Puts `s` at the cursor of `key`'s text area, gives the area the keyboard and closes the
/// popover.
pub(crate) fn insert_into_area(
    key: &ComposerKey,
    s: &str,
    areas: &mut Query<(Entity, &ComposerArea, &mut EditableText)>,
    fonts: &mut FontCx,
    layout: &mut LayoutCx,
    focus: &mut InputFocus,
    popovers: &mut Popovers,
) {
    if let Some((area, _, mut editable)) = areas.iter_mut().find(|(_, a, _)| &a.0 == key) {
        let (text, sel) = read_area(&editable);
        let (new, sel) = insert_at(&text, sel, s);
        write_area(&mut editable, fonts, layout, &new, sel);
        focus.set(area, FocusCause::Navigated);
    }
    popovers.close();
}

/// Whether `link` can go into a comment as an image.
pub fn link_problem(link: &str) -> Option<&'static str> {
    let link = link.trim();
    if link.is_empty() {
        Some("Paste a link first")
    } else if https_host(link).is_none_or(str::is_empty) || link.contains(char::is_whitespace) {
        Some("Only https links work — GitHub does not show other images")
    } else if link.contains(['(', ')', '<', '>']) {
        // They would end the `![](…)` the link goes into.
        Some("This link has a ( ) < or > in it — use its plain address")
    } else {
        None
    }
}

/// A search or link field with a hint that shows while it is empty.
pub(crate) fn hinted_field(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    value: &str,
    hint: &str,
    extra: impl Bundle,
) -> Entity {
    let mut field = Entity::PLACEHOLDER;
    p.spawn(Node {
        width: percent(100),
        ..default()
    })
    .with_children(|wrap| {
        field = wrap
            .spawn((text_field(fonts, value, PANEL_WIDTH - 24.0, false), extra))
            .id();
        wrap.spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(11),
                top: px(7),
                display: if value.is_empty() {
                    Display::Flex
                } else {
                    Display::None
                },
                ..default()
            },
            Pickable::IGNORE,
            Placeholder(field),
            text(fonts, hint.to_string(), Type::MUTED.ink(Swatch::Faint)),
        ));
    });
    field
}

pub(crate) fn note_line(p: &mut ChildSpawnerCommands, fonts: &UiFonts) {
    p.spawn((text(fonts, "", Type::META.ink(Swatch::Orange)), NoteText));
}

pub struct PopoverPlugin;

impl Plugin for PopoverPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Popovers>()
            .init_resource::<PopoverNote>()
            .add_systems(PreUpdate, close_on_escape.after(InputSystems))
            .add_systems(
                Update,
                (
                    sync_popovers,
                    show_placeholders,
                    show_note,
                    style_buttons,
                    commit_links,
                )
                    .chain(),
            )
            .add_systems(
                PostUpdate,
                place_panels.after(bevy::ui::UiSystems::PostLayout),
            );
    }
}

/// Esc closes the popover first; the key press is used up so nothing behind reacts to it.
fn close_on_escape(mut keys: ResMut<ButtonInput<KeyCode>>, mut popovers: ResMut<Popovers>) {
    if popovers.open.is_some() && keys.just_pressed(KeyCode::Escape) {
        keys.clear_just_pressed(KeyCode::Escape);
        popovers.close();
    }
}

fn on_backdrop_press(_press: On<Pointer<Press>>, mut popovers: ResMut<Popovers>) {
    popovers.close();
}

/// Keeps exactly the popover `Popovers::open` names on screen.
fn sync_popovers(
    mut commands: Commands,
    fonts: Res<UiFonts>,
    mut popovers: ResMut<Popovers>,
    mut note: ResMut<PopoverNote>,
    tabs: Res<ReviewTabs>,
    extra: Res<ExtraModes>,
    composers: Query<&Composer>,
    layers: Query<(Entity, &PopoverLayer)>,
) {
    if let Some((key, _)) = popovers.open.clone() {
        let present = composers.iter().any(|c| c.0 == key);
        if present && popovers.seen.as_ref() != Some(&key) {
            popovers.seen = Some(key.clone());
        }
        let gone = !present && popovers.seen.as_ref() == Some(&key);
        if gone || mode_of(&key, &tabs, &extra) == ComposerMode::Preview {
            popovers.close();
        }
    }
    let want = popovers.open.clone();
    let mut present = false;
    for (entity, layer) in &layers {
        if want.as_ref() == Some(&(layer.key.clone(), layer.kind)) {
            present = true;
        } else {
            commands.entity(entity).despawn();
        }
    }
    if want.is_none() && note.0.is_some() {
        note.0 = None;
    }
    let Some((key, kind)) = want else { return };
    if present || !composers.iter().any(|c| c.0 == key) {
        return;
    }
    note.0 = None;
    let seed = popovers.gif_query.clone();
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                top: px(0),
                width: percent(100),
                height: percent(100),
                ..default()
            },
            GlobalZIndex(LAYER_Z),
            Pickable {
                should_block_lower: true,
                is_hoverable: true,
            },
            PopoverLayer {
                key: key.clone(),
                kind,
            },
            observe(on_backdrop_press),
        ))
        .with_children(|layer| {
            layer
                .spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: px(0),
                        top: px(0),
                        width: px(PANEL_WIDTH),
                        flex_direction: FlexDirection::Column,
                        row_gap: px(8),
                        padding: px(12).all(),
                        border: px(1).all(),
                        border_radius: BorderRadius::all(px(12)),
                        ..default()
                    },
                    BackgroundColor::default(),
                    Fill(Swatch::Surface),
                    BorderColor::default(),
                    Stroke(Swatch::Line),
                    // Shown by `place_panels` once the card has been laid out where it belongs.
                    Visibility::Hidden,
                    Pickable {
                        should_block_lower: true,
                        is_hoverable: true,
                    },
                    PopoverPanel { key: key.clone() },
                    observe(|mut press: On<Pointer<Press>>| press.propagate(false)),
                ))
                .with_children(|panel| match kind {
                    PopoverKind::Emoji => super::emoji_picker::emoji_panel(panel, &fonts, &key),
                    PopoverKind::Gif => {
                        super::gif_picker::gif_panel(panel, &fonts, &key, &seed);
                    }
                    PopoverKind::Image => image_panel(panel, &fonts, &key),
                });
        });
}

fn image_panel(p: &mut ChildSpawnerCommands, fonts: &UiFonts, key: &ComposerKey) {
    p.spawn(text(fonts, "Image link", Type::STRONG));
    hinted_field(
        p,
        fonts,
        "",
        "https://…/screenshot.png",
        LinkField(key.clone()),
    );
    note_line(p, fonts);
    p.spawn(text(
        fonts,
        "GitHub only accepts image links. Upload the file somewhere first — dragging it into any GitHub comment box in the browser gives you a link — then paste the link here and press Enter.",
        Type::META,
    ));
}

/// Shows each hint while its field is empty.
fn show_placeholders(fields: Query<&EditableText>, mut hints: Query<(&Placeholder, &mut Node)>) {
    for (Placeholder(field), mut node) in &mut hints {
        let empty = fields
            .get(*field)
            .is_ok_and(|e| e.value().to_string().is_empty());
        let want = if empty { Display::Flex } else { Display::None };
        if node.display != want {
            node.display = want;
        }
    }
}

fn show_note(note: Res<PopoverNote>, mut lines: Query<&mut Text, With<NoteText>>) {
    if !note.is_changed() {
        return;
    }
    for mut t in &mut lines {
        let want = note.0.clone().unwrap_or_default();
        if t.0 != want {
            t.0 = want;
        }
    }
}

/// The toolbar button of the open popover looks pressed.
fn style_buttons(
    popovers: Res<Popovers>,
    mut buttons: Query<(&PopoverButton, &mut Fill, &mut HoverFill)>,
) {
    for (button, mut fill, mut hover) in &mut buttons {
        let open = popovers.open.as_ref() == Some(&(button.key.clone(), button.kind));
        let want = if open {
            Swatch::GreenSoft
        } else {
            Swatch::Clear
        };
        if fill.0 != want {
            fill.0 = want;
        }
        let want = if open {
            Swatch::GreenSoft
        } else {
            Swatch::Hover
        };
        if hover.0 != want {
            hover.0 = want;
        }
    }
}

/// Enter in a link field: a good link goes into the text as an image, a bad one says why.
#[allow(clippy::too_many_arguments)]
fn commit_links(
    mut committed: MessageReader<FieldCommitted>,
    links: Query<&LinkField>,
    mut areas: Query<(Entity, &ComposerArea, &mut EditableText)>,
    mut fonts: ResMut<FontCx>,
    mut layout: ResMut<LayoutCx>,
    mut focus: ResMut<InputFocus>,
    mut popovers: ResMut<Popovers>,
    mut note: ResMut<PopoverNote>,
) {
    for c in committed.read() {
        let Ok(LinkField(key)) = links.get(c.entity) else {
            continue;
        };
        if let Some(problem) = link_problem(&c.value) {
            note.0 = Some(problem.to_string());
            continue;
        }
        let link = format!("![]({})", c.value.trim());
        insert_into_area(
            key,
            &link,
            &mut areas,
            &mut fonts,
            &mut layout,
            &mut focus,
            &mut popovers,
        );
    }
}

/// Puts each popover card under its toolbar button, inside the window. A new card stays hidden
/// until a layout pass has used the position, so it never shows at the corner first.
fn place_panels(
    buttons: Query<(&PopoverButton, &ComputedNode, &UiGlobalTransform)>,
    layers: Query<(&PopoverLayer, &Children)>,
    mut panels: Query<(&mut Node, &mut Visibility), With<PopoverPanel>>,
    windows: Query<&Window, With<PrimaryWindow>>,
) {
    for (layer, children) in &layers {
        let Some((_, node, at)) = buttons
            .iter()
            .find(|(b, _, _)| b.key == layer.key && b.kind == layer.kind)
        else {
            continue;
        };
        let scale = node.inverse_scale_factor;
        let left = (at.translation.x - node.size.x / 2.0) * scale;
        let top = (at.translation.y + node.size.y / 2.0) * scale + 6.0;
        let width = windows.iter().next().map(Window::width);
        let max_left = width.map_or(f32::MAX, |w| (w - PANEL_WIDTH - 8.0).max(8.0));
        for child in children {
            if let Ok((mut panel, mut visibility)) = panels.get_mut(*child) {
                let (l, t) = (px(left.clamp(8.0, max_left)), px(top));
                if panel.left != l || panel.top != t {
                    panel.left = l;
                    panel.top = t;
                } else if *visibility != Visibility::Inherited {
                    *visibility = Visibility::Inherited;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Ask, Tell};
    use crate::fixture;
    use crate::review_state::{EditTarget, Editor};
    use crate::testing::{self, NOW};
    use crate::ui::composer::emoji_picker::{EmojiCell, EmojiSearch, RECENT_KEY, push_recent};
    use crate::ui::composer::gif_picker::{
        GifNeedsKey, GifSearch, GifSearchBox, GifState, GifStatus, GifTile, GifTyping,
        gif_error_text, gif_markdown, gif_search_due,
    };
    use crate::ui::composer::testkit::*;
    use crate::ui::composer::{ComposerModeButton, Slot};
    use crate::ui::emoji::{Group, by_group};
    use clusia_protocol::{ErrorCode, GifItem, GifPage};

    fn composer_for(app: &mut App, text: &str) -> (Entity, ComposerKey) {
        with_fonts(app);
        let (pr, area) = open(app, EditTarget::General, text);
        select(app, area, text.len()..text.len());
        (area, ComposerKey(pr, Slot::Edit(EditTarget::General)))
    }

    fn show(app: &mut App, key: &ComposerKey, kind: PopoverKind) {
        app.world_mut().resource_mut::<Popovers>().open = Some((key.clone(), kind));
        for _ in 0..(Group::PICKER.len() + 3) {
            app.update();
        }
    }

    fn has_text(app: &mut App, s: &str) -> bool {
        let mut q = app.world_mut().query::<&Text>();
        q.iter(app.world()).any(|t| t.0 == s)
    }

    fn gif(id: &str, title: &str) -> GifItem {
        GifItem {
            id: id.into(),
            title: title.into(),
            preview_url: format!("https://media.giphy.com/media/{id}/200w_d.gif"),
            url: format!("https://media.giphy.com/media/{id}/giphy.gif"),
            width: 200,
            height: 150,
        }
    }

    fn page() -> GifPage {
        GifPage {
            items: vec![gif("a1", "Turtle slow"), gif("b2", "Waiting")],
            next_offset: Some(24),
        }
    }

    /// The searches asked since the last call; the tiles' picture requests are not searches.
    fn searches(app: &mut App) -> Vec<Ask> {
        testing::recorded(app)
            .into_iter()
            .filter(|a| matches!(a, Ask::SearchGifs { .. }))
            .collect()
    }

    fn open_popover(app: &App) -> Option<PopoverKind> {
        app.world()
            .resource::<Popovers>()
            .open
            .as_ref()
            .map(|(_, k)| *k)
    }

    #[test]
    fn emoji_search_and_insert() {
        let mut app = testing::app(fixture::demo(NOW));
        let (area, key) = composer_for(&mut app, "Nice ");
        show(&mut app, &key, PopoverKind::Emoji);
        let labels: Vec<&str> = Group::PICKER
            .iter()
            .filter(|g| by_group(**g).next().is_some())
            .map(|g| g.label())
            .collect();
        assert!(!labels.is_empty());
        for label in &labels {
            assert!(
                has_text(&mut app, label),
                "{label} is drawn, one group a frame"
            );
        }
        assert!(!has_text(&mut app, "Frequently used"), "nothing used yet");
        let search = testing::find::<EmojiSearch>(&mut app, |_| true);
        testing::type_into(&mut app, search, "turtle");
        testing::settle(&mut app);
        assert!(
            !has_text(&mut app, labels[0]),
            "a search replaces the groups"
        );
        let cell = testing::find::<EmojiCell>(&mut app, |c| c.emoji == "🐢");
        testing::activate(&mut app, cell);
        testing::settle(&mut app);
        assert_eq!(value(&app, area), "Nice 🐢");
        assert_eq!(open_popover(&app), None, "picking closes it");
        assert_eq!(app.world().resource::<InputFocus>().get(), Some(area));
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::SetConfig {
                key: RECENT_KEY.into(),
                value: r#"["🐢"]"#.into()
            }]
        );
        assert_eq!(testing::count::<PopoverLayer>(&mut app), 0);
    }

    #[test]
    fn recent_emoji_saved() {
        assert_eq!(push_recent(&[], "🐢"), ["🐢"]);
        let recent: Vec<String> = ["🚀", "🐢", "👀"].map(String::from).to_vec();
        assert_eq!(
            push_recent(&recent, "🐢"),
            ["🐢", "🚀", "👀"],
            "moves to the front"
        );
        let many: Vec<String> = (0..20).map(|i| format!("e{i}")).collect();
        let pushed = push_recent(&many, "🐢");
        assert_eq!((pushed.len(), pushed[0].as_str()), (16, "🐢"), "at most 16");
        let mut app = testing::app(fixture::demo(NOW));
        testing::set_config_locally(&mut app, RECENT_KEY, r#"["🚀","🐢"]"#);
        let (area, key) = composer_for(&mut app, "");
        show(&mut app, &key, PopoverKind::Emoji);
        assert!(has_text(&mut app, "Frequently used"));
        let first: Vec<&str> = {
            let mut q = app.world_mut().query::<&EmojiCell>();
            q.iter(app.world()).map(|c| c.emoji).take(2).collect()
        };
        assert_eq!(first, ["🚀", "🐢"], "the recent row comes first, in order");
        let cell = testing::find::<EmojiCell>(&mut app, |c| c.emoji == "🐢");
        testing::activate(&mut app, cell);
        assert_eq!(value(&app, area), "🐢");
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::SetConfig {
                key: RECENT_KEY.into(),
                value: r#"["🐢","🚀"]"#.into()
            }]
        );
    }

    #[test]
    fn gif_search_debounced() {
        assert!(
            gif_search_due(None, "", f64::NEG_INFINITY, 0, 0.0),
            "the first search is at once"
        );
        assert!(!gif_search_due(Some(""), "t", 1.0, 0, 1.1), "still typing");
        assert!(gif_search_due(Some(""), "t", 1.0, 0, 1.31));
        assert!(
            !gif_search_due(Some("t"), "t", 1.0, 0, 9.0),
            "already asked"
        );
        assert!(
            !gif_search_due(Some(""), "t", 1.0, 1, 9.0),
            "one question at a time"
        );
        let mut app = testing::app(fixture::demo(NOW));
        let (_, key) = composer_for(&mut app, "");
        show(&mut app, &key, PopoverKind::Gif);
        assert_eq!(
            searches(&mut app),
            [Ask::SearchGifs {
                query: String::new(),
                offset: 0
            }],
            "trending first"
        );
        testing::tell(&mut app, Tell::Gifs(Ok(page())));
        testing::settle(&mut app);
        assert_eq!(testing::count::<GifTile>(&mut app), 2);
        assert!(has_text(&mut app, "Powered by GIPHY"));
        let search = testing::find::<GifSearch>(&mut app, |_| true);
        testing::type_into(&mut app, search, "tur");
        testing::settle(&mut app);
        assert!(searches(&mut app).is_empty(), "debounced");
        let now = app.world().resource::<Time>().elapsed_secs_f64();
        app.world_mut()
            .resource_mut::<GifTyping>()
            .edited_ago(now, 0.31);
        app.update();
        assert_eq!(
            searches(&mut app),
            [Ask::SearchGifs {
                query: "tur".into(),
                offset: 0
            }]
        );
        testing::type_into(&mut app, search, "t");
        testing::settle(&mut app);
        let now = app.world().resource::<Time>().elapsed_secs_f64();
        app.world_mut()
            .resource_mut::<GifTyping>()
            .edited_ago(now, 0.31);
        app.update();
        assert!(searches(&mut app).is_empty(), "the answer is awaited first");
        testing::tell(&mut app, Tell::Gifs(Ok(page())));
        assert_eq!(
            searches(&mut app),
            [Ask::SearchGifs {
                query: "turt".into(),
                offset: 0
            }]
        );
    }

    #[test]
    fn a_late_answer_to_a_closed_popover_is_not_the_new_answer() {
        let mut app = testing::app(fixture::demo(NOW));
        let (_, key) = composer_for(&mut app, "");
        show(&mut app, &key, PopoverKind::Gif);
        assert_eq!(searches(&mut app).len(), 1);
        app.world_mut().resource_mut::<Popovers>().open = None;
        app.update();
        show(&mut app, &key, PopoverKind::Gif);
        assert_eq!(searches(&mut app).len(), 1, "asked again after reopening");
        testing::tell(&mut app, Tell::Gifs(Ok(page())));
        testing::settle(&mut app);
        assert_ne!(
            app.world().resource::<GifState>().status,
            GifStatus::Ready,
            "the first answer belongs to the closed popover"
        );
        assert_eq!(testing::count::<GifTile>(&mut app), 0);
        testing::tell(&mut app, Tell::Gifs(Ok(page())));
        testing::settle(&mut app);
        assert_eq!(app.world().resource::<GifState>().status, GifStatus::Ready);
        assert_eq!(testing::count::<GifTile>(&mut app), 2);
    }

    #[test]
    fn a_popover_card_is_not_shown_before_it_is_placed() {
        let mut app = testing::app(fixture::demo(NOW));
        let (_, key) = composer_for(&mut app, "");
        let visible = |app: &mut App| {
            let panel = testing::find::<PopoverPanel>(app, |_| true);
            app.world().get::<Visibility>(panel).copied().unwrap()
        };
        app.world_mut().resource_mut::<Popovers>().open = Some((key.clone(), PopoverKind::Emoji));
        app.update();
        assert_eq!(visible(&mut app), Visibility::Hidden, "not yet placed");
        testing::settle(&mut app);
        assert_eq!(visible(&mut app), Visibility::Inherited);
        let panel = testing::find::<PopoverPanel>(&mut app, |_| true);
        assert_ne!(app.world().get::<Node>(panel).unwrap().top, px(0));
    }

    #[test]
    fn gif_click_inserts_markdown() {
        let item = GifItem {
            title: "  a [fast]\nturtle ".into(),
            ..gif("c3", "")
        };
        assert_eq!(
            gif_markdown(&item),
            "![a fast turtle](https://media.giphy.com/media/c3/giphy.gif)"
        );
        assert_eq!(
            gif_markdown(&gif("d4", "")),
            "![GIF](https://media.giphy.com/media/d4/giphy.gif)"
        );
        let mut app = testing::app(fixture::demo(NOW));
        let (area, key) = composer_for(&mut app, "Waiting on this: ");
        show(&mut app, &key, PopoverKind::Gif);
        testing::tell(&mut app, Tell::Gifs(Ok(page())));
        testing::settle(&mut app);
        let tile = testing::find::<GifTile>(&mut app, |t| t.item.id == "a1");
        testing::activate(&mut app, tile);
        testing::settle(&mut app);
        assert_eq!(
            value(&app, area),
            "Waiting on this: ![Turtle slow](https://media.giphy.com/media/a1/giphy.gif)"
        );
        assert_eq!(open_popover(&app), None);
        assert_eq!(testing::count::<GifTile>(&mut app), 0);
    }

    #[test]
    fn popover_errors_keep_the_text() {
        let mut app = testing::app(fixture::demo(NOW));
        let (area, key) = composer_for(&mut app, "keep me");
        show(&mut app, &key, PopoverKind::Gif);
        testing::tell(
            &mut app,
            Tell::Gifs(Err((ErrorCode::RateLimited, "429".into()))),
        );
        testing::settle(&mut app);
        assert!(has_text(&mut app, "Giphy is busy — try again in a moment"));
        testing::tell(
            &mut app,
            Tell::Gifs(Err((
                ErrorCode::Unauthorized,
                "Giphy rejected the key".into(),
            ))),
        );
        testing::settle(&mut app);
        assert!(has_text(&mut app, "Giphy rejected the key"));
        assert_eq!(
            gif_error_text(ErrorCode::Offline, "x"),
            "Can't reach Giphy — check your connection"
        );
        assert_eq!(
            open_popover(&app),
            Some(PopoverKind::Gif),
            "the popover stays"
        );
        // A refused link says why and leaves everything as it was.
        show(&mut app, &key, PopoverKind::Image);
        let link = testing::find::<LinkField>(&mut app, |_| true);
        app.world_mut().write_message(FieldCommitted {
            entity: link,
            value: "http://example.com/a.png".into(),
        });
        testing::settle(&mut app);
        assert!(has_text(
            &mut app,
            "Only https links work — GitHub does not show other images"
        ));
        assert_eq!(value(&app, area), "keep me");
        assert_eq!(open_popover(&app), Some(PopoverKind::Image));
        app.world_mut().write_message(FieldCommitted {
            entity: link,
            value: " https://example.com/a.png ".into(),
        });
        testing::settle(&mut app);
        assert_eq!(value(&app, area), "keep me![](https://example.com/a.png)");
        assert_eq!(open_popover(&app), None);
        assert_eq!(link_problem(""), Some("Paste a link first"));
        assert!(link_problem("https://example.com/a.png").is_none());
        assert!(link_problem("HTTPS://example.com/a.png").is_none());
        for bad in [
            "https://",
            "https:///a.png",
            "http://example.com/a.png",
            "https://example.com/a b.png",
            "https://example.com/a).png",
            "https://example.com/(a.png",
            "https://example.com/<a>.png",
        ] {
            assert!(link_problem(bad).is_some(), "{bad}");
        }
    }

    #[test]
    fn not_configured_shows_only_link() {
        let mut app = testing::app(fixture::demo(NOW));
        let (_, key) = composer_for(&mut app, "");
        show(&mut app, &key, PopoverKind::Gif);
        testing::tell(
            &mut app,
            Tell::Gifs(Err((ErrorCode::NotConfigured, "no key".into()))),
        );
        testing::settle(&mut app);
        let display = |app: &mut App, e| app.world().get::<Node>(e).unwrap().display;
        let search = testing::find::<GifSearchBox>(&mut app, |_| true);
        let needs = testing::find::<GifNeedsKey>(&mut app, |_| true);
        assert_eq!(display(&mut app, search), Display::None);
        assert_eq!(display(&mut app, needs), Display::Flex);
        assert!(has_text(
            &mut app,
            "Add a Giphy key in Config › Media to search GIFs."
        ));
        assert_eq!(
            testing::count::<LinkField>(&mut app),
            1,
            "pasting a link still works"
        );
        assert_eq!(testing::count::<GifTile>(&mut app), 0);
        assert_eq!(
            testing::recorded(&mut app).len(),
            1,
            "only the first search went out"
        );
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn a_popover_staged_before_its_composer_waits_for_it() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        let key = ComposerKey(pr.clone(), Slot::Edit(EditTarget::General));
        app.world_mut().resource_mut::<Popovers>().open = Some((key.clone(), PopoverKind::Emoji));
        testing::settle(&mut app);
        assert_eq!(
            open_popover(&app),
            Some(PopoverKind::Emoji),
            "no composer yet"
        );
        assert_eq!(testing::count::<PopoverLayer>(&mut app), 0);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .editor = Some(Editor {
            target: EditTarget::General,
            text: String::new(),
            error: None,
            ticket: None,
            mode: ComposerMode::Write,
        });
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<PopoverLayer>(&mut app),
            1,
            "it opens with its composer"
        );
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .editor = None;
        testing::settle(&mut app);
        assert_eq!(open_popover(&app), None, "and goes with it");
    }

    #[test]
    fn an_unanswered_search_does_not_block_the_next_popover() {
        let mut app = testing::app(fixture::demo(NOW));
        let (_, key) = composer_for(&mut app, "");
        show(&mut app, &key, PopoverKind::Gif);
        assert_eq!(
            testing::recorded(&mut app).len(),
            1,
            "the first search never gets an answer"
        );
        app.world_mut().resource_mut::<Popovers>().close();
        testing::settle(&mut app);
        show(&mut app, &key, PopoverKind::Gif);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::SearchGifs {
                query: String::new(),
                offset: 0
            }],
            "the popover asks again"
        );
    }

    #[test]
    fn escape_and_the_composer_close_the_popover() {
        let mut app = testing::app(fixture::demo(NOW));
        let (_, key) = composer_for(&mut app, "x");
        show(&mut app, &key, PopoverKind::Emoji);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);
        app.update();
        assert_eq!(open_popover(&app), None);
        assert!(
            !app.world()
                .resource::<ButtonInput<KeyCode>>()
                .just_pressed(KeyCode::Escape),
            "the key press is used up"
        );
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset_all();
        // Toggle buttons, Preview and a vanished composer close it too.
        let gif = testing::find::<PopoverButton>(&mut app, |b| b.kind == PopoverKind::Gif);
        testing::activate(&mut app, gif);
        assert_eq!(open_popover(&app), Some(PopoverKind::Gif));
        testing::activate(&mut app, gif);
        assert_eq!(open_popover(&app), None);
        show(&mut app, &key, PopoverKind::Image);
        let preview =
            testing::find::<ComposerModeButton>(&mut app, |b| b.mode == ComposerMode::Preview);
        testing::activate(&mut app, preview);
        testing::settle(&mut app);
        assert_eq!(open_popover(&app), None);
        let write =
            testing::find::<ComposerModeButton>(&mut app, |b| b.mode == ComposerMode::Write);
        testing::activate(&mut app, write);
        show(&mut app, &key, PopoverKind::Emoji);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&key.0)
            .unwrap()
            .ui
            .editor = None;
        testing::settle(&mut app);
        assert_eq!(open_popover(&app), None);
    }
}
