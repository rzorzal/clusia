//! The `+` palette (mockup `OpenPR.png`): open any pull request by URL, `owner/repo#n` or a
//! title from the lists, saved reviews and open tabs.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input::ButtonInput;
use bevy::input_focus::AutoFocus;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy::ui_widgets::{Activate, Button as WidgetButton, observe};
use clusia_core::PrRef;
use clusia_protocol::WindowTarget;
use clusia_view::lists::is_saved;

use crate::bridge::Model;
use crate::fonts::UiFonts;
use crate::nav::{Nav, NavSystems, pr_title};
use crate::screens::review::agent::permission::PermissionModal;
use crate::snapshot::Snapshot;
use crate::theme::Swatch;
use crate::ui::kit::{Clickable, Fill, HoverFill, Stroke, Type, panel, text, text_field};
use crate::ui::modal::{escape_pressed, modal_card, modal_root};

pub const MAX_ROWS: usize = 8;

/// The palette's state; `open` spawns it, closing despawns it.
#[derive(Resource, Debug, Clone, Default, PartialEq, Eq)]
pub struct Palette {
    pub open: bool,
    pub query: String,
    /// The highlighted row.
    pub cursor: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteRow {
    pub pr: PrRef,
    /// Empty when no list knows the pull request yet.
    pub title: String,
    pub note: &'static str,
}

pub fn palette_matches(snap: &Snapshot, nav: &Nav, query: &str) -> Vec<PaletteRow> {
    let q = query.trim();
    let needle = q.to_lowercase();
    let mut rows: Vec<PaletteRow> = Vec::new();
    if let Ok(pr) = q.parse::<PrRef>() {
        rows.push(PaletteRow {
            title: pr_title(snap, &pr).unwrap_or("").to_string(),
            pr,
            note: "open",
        });
    }
    let tabs: Vec<(&PrRef, &str, &'static str)> = nav
        .reviews
        .iter()
        .map(|pr| (pr, pr_title(snap, pr).unwrap_or(""), "in your tabs"))
        .collect();
    let candidates = snap
        .assigned
        .iter()
        .map(|p| (&p.pr, p.title.as_str(), "assigned to you"))
        .chain(snap.mine.iter().map(|p| (&p.pr, p.title.as_str(), "yours")))
        .chain(
            snap.reviews
                .iter()
                .filter(|r| is_saved(r.state))
                .map(|r| (&r.pr, r.title.as_str(), "saved review")),
        )
        .chain(tabs);
    for (pr, title, note) in candidates {
        if rows.len() >= MAX_ROWS {
            break;
        }
        if rows.iter().any(|r| &r.pr == pr) {
            continue;
        }
        let hit = needle.is_empty()
            || title.to_lowercase().contains(&needle)
            || pr.to_string().to_lowercase().contains(&needle);
        if hit {
            rows.push(PaletteRow {
                pr: pr.clone(),
                title: title.to_string(),
                note,
            });
        }
    }
    rows
}

#[derive(Component, Debug)]
pub struct PaletteRoot;

/// The hint beside the palette field; it keeps one line and the field gives way.
#[derive(Component, Debug)]
pub struct PaletteHint;

#[derive(Component, Debug)]
pub struct PaletteField;

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PaletteRowButton(pub PrRef);

#[derive(Component)]
struct RowsPart {
    built: Option<(Vec<PaletteRow>, usize, bool)>,
}

pub struct OpenPrPlugin;

impl Plugin for OpenPrPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Palette>().add_systems(
            Update,
            (sync_root, live_query, palette_keys, rebuild_rows)
                .chain()
                .after(NavSystems),
        );
    }
}

fn open_row(palette: &mut Palette, nav: &mut Nav, pr: &PrRef) {
    nav.go(&WindowTarget::Review { pr: pr.clone() });
    *palette = Palette::default();
}

/// Spawns the palette when it opens and removes it when it closes.
fn sync_root(
    mut commands: Commands,
    palette: Res<Palette>,
    fonts: Res<UiFonts>,
    roots: Query<Entity, With<PaletteRoot>>,
) {
    match (palette.open, roots.iter().next()) {
        (true, None) => {
            let fonts = &*fonts;
            commands
                .spawn((modal_root(), PaletteRoot))
                .insert(Node {
                    position_type: PositionType::Absolute,
                    left: px(0),
                    top: px(0),
                    width: percent(100),
                    height: percent(100),
                    justify_content: JustifyContent::Center,
                    align_items: AlignItems::FlexStart,
                    padding: UiRect::top(px(64)),
                    ..default()
                })
                .with_children(|root| {
                    root.spawn(modal_card(600.0)).with_children(|card| {
                        palette_card(card, fonts, &palette.query);
                    });
                });
        }
        (false, Some(root)) => {
            commands.entity(root).despawn();
        }
        _ => {}
    }
}

fn palette_card(p: &mut ChildSpawnerCommands, fonts: &UiFonts, query: &str) {
    p.spawn((
        Node {
            padding: UiRect::axes(px(18), px(12)),
            column_gap: px(12),
            align_items: AlignItems::Center,
            border: UiRect::bottom(px(1)),
            ..default()
        },
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|row| {
        // The field's width is replaced below: it takes what the hint leaves.
        row.spawn((
            text_field(fonts, query, 0.0, false),
            PaletteField,
            AutoFocus,
        ))
        .entry::<Node>()
        .and_modify(|mut node| {
            node.width = Val::Auto;
            node.min_width = px(0);
            node.flex_grow = 1.0;
            node.flex_shrink = 1.0;
        });
        row.spawn((
            text(fonts, "URL, owner/repo#n or a title", Type::META),
            TextLayout::no_wrap(),
            Node {
                flex_shrink: 0.0,
                ..default()
            },
            PaletteHint,
        ));
    });
    p.spawn(Node {
        padding: UiRect::new(px(18), px(18), px(10), px(4)),
        ..default()
    })
    .with_children(|h| {
        h.spawn(text(fonts, "MATCHES", Type::META));
    });
    p.spawn((
        Node {
            flex_direction: FlexDirection::Column,
            padding: UiRect::bottom(px(6)),
            ..default()
        },
        RowsPart { built: None },
    ));
    p.spawn((
        panel(
            Node {
                padding: UiRect::axes(px(18), px(10)),
                column_gap: px(16),
                align_items: AlignItems::Center,
                border: UiRect::top(px(1)),
                ..default()
            },
            Swatch::Chrome,
        ),
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|f| {
        f.spawn(text(fonts, "↵ open   ↑↓ choose   esc close", Type::META));
        f.spawn(Node {
            flex_grow: 1.0,
            ..default()
        });
        f.spawn(text(
            fonts,
            "Any pull request you can read, not only your lists.",
            Type::META,
        ));
    });
}

fn live_query(
    fields: Query<&EditableText, (With<PaletteField>, Changed<EditableText>)>,
    mut palette: ResMut<Palette>,
) {
    for editable in &fields {
        let value = editable.value().to_string();
        if value != palette.query {
            palette.query = value;
            palette.cursor = 0;
        }
    }
}

/// The palette's keys. A permission modal over it owns the keyboard: Enter there must never
/// open a pull request and hide the question.
pub(crate) fn palette_keys(
    keys: Res<ButtonInput<KeyCode>>,
    model: Res<Model>,
    fields: Query<&EditableText, With<PaletteField>>,
    modals: Query<(), With<PermissionModal>>,
    mut palette: ResMut<Palette>,
    mut nav: ResMut<Nav>,
) {
    // Keys typed into an input method's composition are not commands.
    if !palette.open || fields.iter().any(EditableText::is_composing) || !modals.is_empty() {
        return;
    }
    if escape_pressed(&keys) {
        *palette = Palette::default();
        return;
    }
    let rows = palette_matches(&model.snapshot, &nav, &palette.query);
    let last = rows.len().saturating_sub(1);
    if keys.just_pressed(KeyCode::ArrowDown) && palette.cursor < last {
        palette.cursor += 1;
    }
    if keys.just_pressed(KeyCode::ArrowUp) && palette.cursor > 0 {
        palette.cursor -= 1;
    }
    if keys.just_pressed(KeyCode::Enter)
        && let Some(row) = rows.get(palette.cursor.min(last))
    {
        open_row(&mut palette, &mut nav, &row.pr);
    }
}

fn rebuild_rows(
    mut commands: Commands,
    palette: Res<Palette>,
    model: Res<Model>,
    nav: Res<Nav>,
    fonts: Res<UiFonts>,
    mut parts: Query<(Entity, &mut RowsPart)>,
) {
    for (entity, mut part) in &mut parts {
        let rows = palette_matches(&model.snapshot, &nav, &palette.query);
        let cursor = palette.cursor.min(rows.len().saturating_sub(1));
        let key = (rows, cursor, palette.query.trim().is_empty());
        if part.built.as_ref() == Some(&key) {
            continue;
        }
        let fonts = &*fonts;
        commands.entity(entity).despawn_related::<Children>();
        commands.entity(entity).with_children(|p| {
            rows_list(p, fonts, &key.0, key.1, key.2);
        });
        part.built = Some(key);
    }
}

fn rows_list(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    rows: &[PaletteRow],
    cursor: usize,
    empty_query: bool,
) {
    if rows.is_empty() {
        let message = if empty_query {
            "Nothing in your lists yet. Paste a pull request URL or owner/repo#n."
        } else {
            "No matches. Paste a pull request URL or owner/repo#n."
        };
        p.spawn(Node {
            padding: UiRect::axes(px(18), px(10)),
            ..default()
        })
        .with_children(|e| {
            e.spawn(text(fonts, message, Type::MUTED));
        });
        return;
    }
    for (i, row) in rows.iter().enumerate() {
        let on = i == cursor;
        p.spawn((
            Node {
                padding: UiRect::axes(px(18), px(8)),
                column_gap: px(12),
                align_items: AlignItems::Center,
                ..default()
            },
            WidgetButton,
            Hovered::default(),
            TabIndex(0),
            Clickable,
            BackgroundColor::default(),
            Fill(if on { Swatch::Selected } else { Swatch::Clear }),
            HoverFill(if on { Swatch::Selected } else { Swatch::Hover }),
            PaletteRowButton(row.pr.clone()),
            observe(on_row),
        ))
        .with_children(|r| {
            r.spawn((
                Node {
                    width: px(170),
                    flex_shrink: 0.0,
                    overflow: Overflow::clip_x(),
                    ..default()
                },
                children![text(fonts, row.pr.to_string(), Type::MONO.ink(Swatch::Fg))],
            ));
            r.spawn((
                Node {
                    flex_grow: 1.0,
                    min_width: px(0),
                    overflow: Overflow::clip_x(),
                    ..default()
                },
                children![text(fonts, row.title.clone(), Type::BODY)],
            ));
            r.spawn(text(fonts, row.note, Type::META));
        });
    }
}

fn on_row(
    activate: On<Activate>,
    rows: Query<&PaletteRowButton>,
    mut palette: ResMut<Palette>,
    mut nav: ResMut<Nav>,
) {
    if let Ok(PaletteRowButton(pr)) = rows.get(activate.entity) {
        open_row(&mut palette, &mut nav, pr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::nav::{NewTabButton, Screen};
    use crate::testing::{self, NOW};

    fn pr(s: &str) -> PrRef {
        s.parse().unwrap()
    }

    fn rows(query: &str) -> Vec<(String, String, &'static str)> {
        let snap = fixture::demo(NOW);
        let mut nav = Nav::new(&WindowTarget::Home);
        nav.go(&WindowTarget::Review {
            pr: pr("acme/widgets#9"),
        });
        palette_matches(&snap, &nav, query)
            .into_iter()
            .map(|r| (r.pr.to_string(), r.title, r.note))
            .collect()
    }

    fn press(app: &mut App, key: KeyCode) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(key);
        app.update();
        let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
        keys.release(key);
        keys.clear();
    }

    #[test]
    fn a_ref_comes_first_then_known_titles() {
        let r = rows("rzorzal/site#12");
        assert_eq!(
            r[0],
            (
                "rzorzal/site#12".into(),
                "Dark mode for the docs".into(),
                "open"
            )
        );
        assert!(
            r.iter().skip(1).all(|(p, _, _)| p != "rzorzal/site#12"),
            "no duplicate of the parsed ref: {r:?}"
        );
        let url = rows("https://github.com/acme/widgets/pull/5");
        assert_eq!(url[0], ("acme/widgets#5".into(), String::new(), "open"));
    }

    #[test]
    fn titles_match_every_source_with_its_note() {
        assert_eq!(
            rows("auth"),
            [(
                "rzorzal/clusia#123".into(),
                "feat: auth refresh".into(),
                "assigned to you"
            )]
        );
        assert_eq!(rows("TRAY")[0].2, "yours");
        assert_eq!(
            rows("cache"),
            [(
                "rzorzal/clusia#77".into(),
                "fix: cache invalidation".into(),
                "saved review"
            )]
        );
        assert_eq!(
            rows("widgets"),
            [("acme/widgets#9".into(), String::new(), "in your tabs")]
        );
        assert!(rows("nothing like this").is_empty());
        let all = rows("");
        assert_eq!(all.len(), 8, "at most 8");
        assert_eq!(all[0].2, "assigned to you");
    }

    #[test]
    fn plus_opens_the_palette_and_enter_opens_a_review() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::settle(&mut app);
        assert_eq!(testing::count::<PaletteRoot>(&mut app), 0);
        let plus = testing::find::<NewTabButton>(&mut app, |_| true);
        testing::activate(&mut app, plus);
        testing::settle(&mut app);
        assert!(app.world().resource::<Palette>().open);
        assert_eq!(testing::count::<PaletteRoot>(&mut app), 1);
        let field = testing::find::<PaletteField>(&mut app, |_| true);
        assert_eq!(
            app.world()
                .resource::<bevy::input_focus::InputFocus>()
                .get(),
            Some(field),
            "the field takes the focus"
        );
        app.world_mut()
            .entity_mut(field)
            .insert(EditableText::new("site"));
        testing::settle(&mut app);
        assert_eq!(app.world().resource::<Palette>().query, "site");
        assert_eq!(testing::count::<PaletteRowButton>(&mut app), 2);
        press(&mut app, KeyCode::ArrowDown);
        assert_eq!(app.world().resource::<Palette>().cursor, 1);
        press(&mut app, KeyCode::ArrowDown);
        assert_eq!(
            app.world().resource::<Palette>().cursor,
            1,
            "stays on the last row"
        );
        press(&mut app, KeyCode::ArrowUp);
        press(&mut app, KeyCode::Enter);
        let first = rows("site")[0].0.parse::<PrRef>().unwrap();
        assert_eq!(app.world().resource::<Nav>().screen, Screen::Review(first));
        assert!(!app.world().resource::<Palette>().open);
        testing::settle(&mut app);
        assert_eq!(testing::count::<PaletteRoot>(&mut app), 0);
    }

    #[test]
    fn click_opens_and_escape_closes() {
        let mut app = testing::app(fixture::demo(NOW));
        app.world_mut().resource_mut::<Palette>().open = true;
        testing::settle(&mut app);
        press(&mut app, KeyCode::Escape);
        assert!(!app.world().resource::<Palette>().open);
        app.world_mut().resource_mut::<Palette>().open = true;
        testing::settle(&mut app);
        let row = testing::find::<PaletteRowButton>(&mut app, |r| r.0 == pr("rzorzal/clusia#98"));
        testing::activate(&mut app, row);
        assert_eq!(
            app.world().resource::<Nav>().screen,
            Screen::Review(pr("rzorzal/clusia#98"))
        );
        assert!(!app.world().resource::<Palette>().open);
    }

    #[test]
    fn the_hint_stays_on_one_line_and_the_field_takes_the_rest() {
        let mut app = testing::app(fixture::demo(NOW));
        let plus = testing::find::<NewTabButton>(&mut app, |_| true);
        testing::activate(&mut app, plus);
        testing::settle(&mut app);
        let hint = testing::find::<PaletteHint>(&mut app, |_| true);
        assert_eq!(
            app.world().get::<Text>(hint).unwrap().0,
            "URL, owner/repo#n or a title"
        );
        assert_eq!(
            app.world().get::<TextLayout>(hint).unwrap().linebreak,
            LineBreak::NoWrap
        );
        assert_eq!(app.world().get::<Node>(hint).unwrap().flex_shrink, 0.0);
        let field = testing::find::<PaletteField>(&mut app, |_| true);
        let node = app.world().get::<Node>(field).unwrap();
        assert_eq!(
            (node.flex_grow, node.flex_shrink),
            (1.0, 1.0),
            "the field gives way"
        );
        assert_eq!(node.min_width, px(0));
    }
}
