//! Config (spec §8, mockups `Config*.png`): the section list on the left and one page per
//! section. Every control writes one config key through the daemon. A refusal shows under its
//! field, and the page rebuilds from the saved config.

pub mod about;
pub mod appearance;
pub mod editor;
pub mod general;
pub mod git;
pub mod media;
pub mod notifications;
mod placeholders;
pub mod repos;

use std::collections::HashMap;

use bevy::clipboard::Clipboard;
use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::InputFocus;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy::ui_widgets::{Activate, Button as WidgetButton, ScrollArea, observe};
use clusia_core::Paths;

use crate::app::AppPaths;
use crate::bridge::{Ask, Asks, Model, TOAST_SECS, Toast, Toasts, set_config};
use crate::clock::Clock;
use crate::fonts::UiFonts;
use crate::nav::{ConfigScreen, Nav, NavSystems, Screen, Section};
use crate::platform_open::{OpenUrls, visit};
use crate::snapshot::Snapshot;
use crate::theme::Swatch;
use crate::ui::kit::{
    Field, FieldCommitted, Fill, HoverFill, Stroke, Type, Variant, button, segment, segments, text,
    text_field,
};

#[derive(Debug, Clone, PartialEq)]
pub enum PageView {
    General(general::GeneralView),
    Appearance(appearance::AppearanceView),
    Git(git::GitView),
    Repos(repos::ReposView),
    Editor(editor::EditorView),
    Media(media::MediaView),
    About(about::AboutView),
    Notifications(notifications::NotificationsView),
    Harness,
    Plugins,
}

pub fn page_view(
    section: Section,
    snap: &Snapshot,
    rejected: &HashMap<String, String>,
    paths: &Paths,
) -> PageView {
    match section {
        Section::General => PageView::General(general::view(snap, rejected)),
        Section::Appearance => PageView::Appearance(appearance::view(snap)),
        Section::GitServer => PageView::Git(git::view(snap, rejected)),
        Section::Repositories => PageView::Repos(repos::view(snap, rejected, paths)),
        Section::Editor => PageView::Editor(editor::view(snap, rejected, paths)),
        Section::Media => PageView::Media(media::view(snap, rejected)),
        Section::About => PageView::About(about::view(snap)),
        Section::Notifications => PageView::Notifications(notifications::view(snap, rejected)),
        Section::Harness => PageView::Harness,
        Section::Plugins => PageView::Plugins,
    }
}

/// Writes `key = value` when activated.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct SetValue {
    pub key: &'static str,
    pub value: String,
}

/// Sends an `Ask` when activated.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct Sends(pub Ask);

/// A text field bound to a config key (written on Enter or blur).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigField(pub &'static str);

/// The daemon's refusal for this key, under its field.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldError(pub &'static str);

#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigNavItem(pub Section);

/// The page container, tagged with its section.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageOf(pub Section);

#[derive(Component)]
struct NavPart {
    built: Option<Section>,
}

#[derive(Component)]
struct PagePart {
    built: Option<PageView>,
}

pub struct ConfigPlugin;

impl Plugin for ConfigPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (
                build_config,
                commit_fields,
                repos::commit_roots,
                rebuild_config,
                git::refresh_last_sync,
                notifications::poll_permission,
            )
                .chain()
                .after(NavSystems),
        );
    }
}

/// Opens `0` in the browser when activated.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenLink(pub &'static str);

/// A quiet button that opens `url` in the browser.
pub fn link(fonts: &UiFonts, label: &str, url: &'static str) -> impl Bundle {
    (
        button(fonts, label, Variant::Ghost),
        OpenLink(url),
        observe(on_link),
    )
}

pub(crate) fn on_link(
    activate: On<Activate>,
    links: Query<&OpenLink>,
    urls: Option<ResMut<OpenUrls>>,
) {
    if let Ok(OpenLink(url)) = links.get(activate.entity) {
        visit(urls, url);
    }
}

/// The clipboard's text, or why it cannot be read.
pub fn clipboard_text(clipboard: Option<ResMut<Clipboard>>) -> Result<String, &'static str> {
    const UNREADABLE: &str = "Could not read the clipboard.";
    let mut clipboard = clipboard.ok_or(UNREADABLE)?;
    match clipboard.fetch_text().poll_result() {
        Some(Ok(text)) => Ok(text),
        _ => Err(UNREADABLE),
    }
}

/// A warning toast that lasts `TOAST_SECS` from `now` (`Time::elapsed_secs_f64`).
pub fn warn(toasts: &mut Toasts, now: f64, message: &str) {
    toasts.0.push(Toast {
        text: message.to_string(),
        warning: true,
        until: now + TOAST_SECS,
    });
}

/// The bundle that makes a button write `key = value`.
pub fn setter(key: &'static str, value: impl Into<String>) -> impl Bundle {
    (
        SetValue {
            key,
            value: value.into(),
        },
        observe(on_set),
    )
}

/// The bundle that makes a button send `ask`.
pub fn sends(ask: Ask) -> impl Bundle {
    (Sends(ask), observe(on_send))
}

fn on_set(
    activate: On<Activate>,
    setters: Query<&SetValue>,
    mut asks: ResMut<Asks>,
    mut model: ResMut<Model>,
) {
    if let Ok(s) = setters.get(activate.entity) {
        set_config(&mut asks, &mut model, s.key, s.value.clone());
    }
}

fn on_send(activate: On<Activate>, senders: Query<&Sends>, mut asks: ResMut<Asks>) {
    if let Ok(Sends(ask)) = senders.get(activate.entity) {
        asks.send(ask.clone());
    }
}

fn on_nav(activate: On<Activate>, items: Query<&ConfigNavItem>, mut nav: ResMut<Nav>) {
    if let Ok(ConfigNavItem(section)) = items.get(activate.entity) {
        nav.open_section(*section);
    }
}

fn commit_fields(
    mut commits: MessageReader<FieldCommitted>,
    fields: Query<&ConfigField>,
    mut asks: ResMut<Asks>,
    mut model: ResMut<Model>,
) {
    for c in commits.read() {
        if let Ok(ConfigField(key)) = fields.get(c.entity) {
            set_config(&mut asks, &mut model, key, c.value.trim());
        }
    }
}

fn build_config(mut commands: Commands, screens: Query<Entity, Added<ConfigScreen>>) {
    for screen in &screens {
        commands.entity(screen).with_children(|p| {
            p.spawn((
                Node {
                    width: px(220),
                    flex_shrink: 0.0,
                    flex_direction: FlexDirection::Column,
                    row_gap: px(2),
                    padding: UiRect::axes(px(12), px(20)),
                    border: UiRect::right(px(1)),
                    ..default()
                },
                BorderColor::default(),
                Stroke(Swatch::Line),
                NavPart { built: None },
            ));
            p.spawn((
                Node {
                    flex_grow: 1.0,
                    min_width: px(0),
                    flex_direction: FlexDirection::Column,
                    padding: UiRect::axes(px(40), px(24)),
                    overflow: Overflow::scroll_y(),
                    ..default()
                },
                ScrollArea,
                PagePart { built: None },
            ));
        });
    }
}

fn refill(commands: &mut Commands, entity: Entity, build: impl FnOnce(&mut ChildSpawnerCommands)) {
    commands.entity(entity).despawn_related::<Children>();
    commands.entity(entity).with_children(build);
}

fn rebuild_config(
    mut commands: Commands,
    nav: Res<Nav>,
    model: Res<Model>,
    paths: Res<AppPaths>,
    clock: Res<Clock>,
    fonts: Res<UiFonts>,
    mut navs: Query<(Entity, &mut NavPart)>,
    mut pages: Query<(Entity, &mut PagePart)>,
    focus: Res<InputFocus>,
    fields: Query<(&Field, &EditableText)>,
) {
    let Screen::Config(section) = nav.screen else {
        return;
    };
    let fonts = &*fonts;
    for (e, mut part) in &mut navs {
        if part.built == Some(section) {
            continue;
        }
        refill(&mut commands, e, |p| {
            for s in Section::ALL {
                let on = s == section;
                p.spawn((
                    Node {
                        padding: UiRect::axes(px(10), px(8)),
                        border_radius: BorderRadius::all(px(6)),
                        ..default()
                    },
                    WidgetButton,
                    Hovered::default(),
                    TabIndex(0),
                    BackgroundColor::default(),
                    Fill(if on { Swatch::Surface } else { Swatch::Clear }),
                    HoverFill(if on { Swatch::Surface } else { Swatch::Hover }),
                    ConfigNavItem(s),
                    observe(on_nav),
                ))
                .with_children(|i| {
                    i.spawn(text(
                        fonts,
                        s.label(),
                        if on { Type::STRONG } else { Type::MUTED },
                    ));
                });
            }
        });
        part.built = Some(section);
    }
    let view = page_view(section, &model.snapshot, &model.rejected, &paths.0);
    // Half-typed input survives: a rebuild of the same page waits until the focused field is
    // committed (or reverted) so the field entity is not despawned under the cursor.
    let typing = focus
        .get()
        .and_then(|e| fields.get(e).ok())
        .is_some_and(|(f, t)| t.value().to_string() != f.committed);
    for (e, mut part) in &mut pages {
        if part.built.as_ref() == Some(&view) {
            continue;
        }
        if typing
            && part
                .built
                .as_ref()
                .is_some_and(|b| std::mem::discriminant(b) == std::mem::discriminant(&view))
        {
            continue;
        }
        refill(&mut commands, e, |p| {
            p.spawn((
                Node {
                    flex_direction: FlexDirection::Column,
                    row_gap: px(18),
                    max_width: px(900),
                    ..default()
                },
                PageOf(section),
            ))
            .with_children(|c| match &view {
                PageView::General(v) => general::build(c, fonts, v),
                PageView::Appearance(v) => appearance::build(c, fonts, v),
                PageView::Git(v) => git::build(
                    c,
                    fonts,
                    v,
                    &git::last_sync(model.snapshot.sync.as_ref(), clock.now()),
                ),
                PageView::Repos(v) => repos::build(c, fonts, v),
                PageView::Editor(v) => editor::build(c, fonts, v),
                PageView::Media(v) => media::build(c, fonts, v),
                PageView::About(v) => about::build(c, fonts, v),
                PageView::Notifications(v) => notifications::build(c, fonts, v),
                PageView::Harness => placeholders::harness(c, fonts),
                PageView::Plugins => placeholders::plugins(c, fonts),
            });
        });
        part.built = Some(view.clone());
    }
}

pub fn page_header(p: &mut ChildSpawnerCommands, fonts: &UiFonts, title: &str, subtitle: &str) {
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(4),
        ..default()
    })
    .with_children(|c| {
        c.spawn(text(fonts, title.to_string(), Type::HEADING));
        c.spawn(text(fonts, subtitle.to_string(), Type::MUTED));
    });
}

pub fn heading(p: &mut ChildSpawnerCommands, fonts: &UiFonts, title: &str) {
    p.spawn(Node {
        padding: UiRect::top(px(8)),
        ..default()
    })
    .with_children(|c| {
        c.spawn(text(fonts, title.to_string(), Type::STRONG));
    });
}

/// `label` (190 px) · control · hint, with the refusal for `error_key` underneath.
pub fn row(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    label: &str,
    control: impl FnOnce(&mut ChildSpawnerCommands),
    hint: &str,
    error: Option<(&'static str, &str)>,
) {
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(4),
        ..default()
    })
    .with_children(|c| {
        c.spawn(Node {
            column_gap: px(16),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|r| {
            r.spawn((
                Node {
                    width: px(190),
                    flex_shrink: 0.0,
                    ..default()
                },
                children![text(fonts, label.to_string(), Type::MUTED)],
            ));
            control(r);
            if !hint.is_empty() {
                r.spawn(text(fonts, hint.to_string(), Type::META));
            }
        });
        if let Some((key, message)) = error {
            c.spawn((
                Node {
                    margin: UiRect::left(px(206)),
                    ..default()
                },
                FieldError(key),
                children![text(
                    fonts,
                    message.to_string(),
                    Type::BODY.ink(Swatch::Orange)
                )],
            ));
        }
    });
}

/// A text field bound to `key`.
pub fn field_row(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    label: &str,
    key: &'static str,
    value: &str,
    width: f32,
    hint: &str,
    error: &Option<String>,
) {
    row(
        p,
        fonts,
        label,
        |r| {
            r.spawn((text_field(fonts, value, width, false), ConfigField(key)));
        },
        hint,
        error.as_deref().map(|m| (key, m)),
    );
}

/// A segmented control writing `key`; `options` are `(label, value)`.
pub fn segment_row(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    label: &str,
    key: &'static str,
    options: &[(&str, &str)],
    current: &str,
    hint: &str,
) {
    row(
        p,
        fonts,
        label,
        |r| {
            r.spawn(segments()).with_children(|s| {
                for (text_label, value) in options {
                    s.spawn((
                        segment(fonts, text_label, *value == current),
                        setter(key, *value),
                    ));
                }
            });
        },
        hint,
        None,
    );
}

/// A selectable card (provider, sign-in source, editor). The caller adds a `setter` and fills
/// it.
pub fn option_card(selected: bool, width: f32) -> impl Bundle {
    (
        Node {
            width: px(width),
            flex_direction: FlexDirection::Column,
            row_gap: px(4),
            padding: UiRect::axes(px(14), px(12)),
            border: px(if selected { 2 } else { 1 }).all(),
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        WidgetButton,
        Hovered::default(),
        TabIndex(0),
        BackgroundColor::default(),
        Fill(if selected {
            Swatch::GreenSoft
        } else {
            Swatch::Surface
        }),
        HoverFill(if selected {
            Swatch::GreenSoft
        } else {
            Swatch::Hover
        }),
        BorderColor::default(),
        Stroke(if selected {
            Swatch::Green
        } else {
            Swatch::Line
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Tell;
    use crate::bridge::Toasts;
    use crate::fixture;
    use crate::platform_open::OpenUrls;
    use crate::testing::{self, NOW};
    use crate::ui::kit::Field;
    use clusia_protocol::{ErrorCode, GifPage, WindowTarget};

    fn config_app(section: Section) -> App {
        let mut app = testing::app(fixture::demo(NOW));
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Config);
        app.world_mut().resource_mut::<Nav>().open_section(section);
        testing::settle(&mut app);
        app
    }

    fn field_value(app: &mut App, key: &str) -> String {
        let e = testing::find::<ConfigField>(app, |f| f.0 == key);
        app.world().get::<Field>(e).unwrap().committed.clone()
    }

    #[test]
    fn a_dirty_focused_field_survives_page_updates() {
        let mut app = config_app(Section::GitServer);
        let poll = testing::find::<ConfigField>(&mut app, |f| f.0 == "github.poll_interval_secs");
        app.world_mut()
            .entity_mut(poll)
            .insert(EditableText::new("9"));
        *app.world_mut().resource_mut::<InputFocus>() = InputFocus::from_entity(poll);
        // Both a sync tick and a config change arrive while the user is typing.
        {
            let mut model = app.world_mut().resource_mut::<Model>();
            model.snapshot.sync.as_mut().unwrap().last_sync_unix = Some(NOW - 5);
            model.snapshot.config.github.host = "github.example.dev".into();
        }
        testing::settle(&mut app);
        let text = app
            .world()
            .get::<EditableText>(poll)
            .unwrap()
            .value()
            .to_string();
        assert_eq!(text, "9", "the field entity was kept with its text");
        // Once the field is committed the page catches up.
        app.world_mut().resource_mut::<InputFocus>().clear();
        app.world_mut().entity_mut(poll).insert(Field {
            committed: "9".into(),
        });
        testing::settle(&mut app);
        assert_eq!(field_value(&mut app, "github.host"), "github.example.dev");
    }

    #[test]
    fn last_sync_updates_without_a_rebuild() {
        let mut app = config_app(Section::GitServer);
        let host = testing::find::<ConfigField>(&mut app, |f| f.0 == "github.host");
        let line = |app: &mut App| {
            let e = testing::find::<git::LastSyncText>(app, |_| true);
            app.world().get::<Text>(e).unwrap().0.clone()
        };
        assert_eq!(line(&mut app), "Online · synced 1m ago");
        app.world_mut()
            .resource_mut::<Model>()
            .snapshot
            .sync
            .as_mut()
            .unwrap()
            .last_sync_unix = Some(NOW - 5);
        testing::settle(&mut app);
        assert_eq!(line(&mut app), "Online · synced just now");
        let still = testing::find::<ConfigField>(&mut app, |f| f.0 == "github.host");
        assert_eq!(still, host, "the page was not rebuilt");
    }

    #[test]
    fn section_list_switches_pages() {
        let mut app = config_app(Section::Appearance);
        assert_eq!(testing::count::<ConfigNavItem>(&mut app), 10);
        testing::find::<PageOf>(&mut app, |p| p.0 == Section::Appearance);
        let git = testing::find::<ConfigNavItem>(&mut app, |i| i.0 == Section::GitServer);
        testing::activate(&mut app, git);
        testing::settle(&mut app);
        assert_eq!(
            app.world().resource::<Nav>().screen,
            Screen::Config(Section::GitServer)
        );
        testing::find::<PageOf>(&mut app, |p| p.0 == Section::GitServer);
        assert_eq!(testing::count::<PageOf>(&mut app), 1);
    }

    #[test]
    fn appearance_controls_write_their_keys() {
        let mut app = config_app(Section::Appearance);
        let dark = testing::find::<SetValue>(&mut app, |s| {
            s.key == "appearance.theme" && s.value == "dark"
        });
        testing::activate(&mut app, dark);
        let size = testing::find::<SetValue>(&mut app, |s| {
            s.key == "appearance.code_size" && s.value == "16"
        });
        testing::activate(&mut app, size);
        assert_eq!(
            testing::recorded(&mut app),
            [
                Ask::SetConfig {
                    key: "appearance.theme".into(),
                    value: "dark".into()
                },
                Ask::SetConfig {
                    key: "appearance.code_size".into(),
                    value: "16".into()
                },
            ]
        );
    }

    #[test]
    fn rejected_poll_interval_shows_the_message() {
        let mut app = config_app(Section::GitServer);
        let poll = testing::find::<ConfigField>(&mut app, |f| f.0 == "github.poll_interval_secs");
        app.world_mut().write_message(FieldCommitted {
            entity: poll,
            value: " 5 ".into(),
        });
        app.update();
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::SetConfig {
                key: "github.poll_interval_secs".into(),
                value: "5".into()
            }]
        );
        app.world_mut().resource_mut::<Model>().rejected.insert(
            "github.poll_interval_secs".into(),
            "github.poll_interval_secs must be between 15 and 3600, got 5".into(),
        );
        testing::settle(&mut app);
        testing::find::<FieldError>(&mut app, |e| e.0 == "github.poll_interval_secs");
        assert_eq!(
            field_value(&mut app, "github.poll_interval_secs"),
            "60",
            "the field shows the saved value again"
        );
    }

    #[test]
    fn git_page_without_auth() {
        let mut snap = fixture::demo(NOW);
        snap.auth = None;
        let mut app = testing::app(snap);
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Config);
        app.world_mut()
            .resource_mut::<Nav>()
            .open_section(Section::GitServer);
        testing::settle(&mut app);
        let paste = testing::find::<git::PasteToken>(&mut app, |_| true);
        testing::activate(&mut app, paste);
        assert!(
            testing::recorded(&mut app).is_empty(),
            "no clipboard: nothing sent"
        );
        assert_eq!(app.world().resource::<Toasts>().0.len(), 1);
        let sync = testing::find::<Sends>(&mut app, |s| s.0 == Ask::SyncNow);
        testing::activate(&mut app, sync);
        assert_eq!(testing::recorded(&mut app), [Ask::SyncNow]);
    }

    #[test]
    fn roots_add_and_remove() {
        let mut app = config_app(Section::Repositories);
        let add = testing::find::<repos::AddRoot>(&mut app, |_| true);
        app.world_mut().write_message(FieldCommitted {
            entity: add,
            value: "~/work".into(),
        });
        app.update();
        let remove = testing::find::<repos::RemoveRoot>(&mut app, |r| r.0 == "~/src");
        testing::activate(&mut app, remove);
        assert_eq!(
            testing::recorded(&mut app),
            [
                Ask::SetConfig {
                    key: "repositories.roots".into(),
                    value: r#"["~/Repos","~/Projects","~/src","~/code","~/work"]"#.into()
                },
                Ask::SetConfig {
                    key: "repositories.roots".into(),
                    value: r#"["~/Repos","~/Projects","~/code"]"#.into()
                },
            ]
        );
    }

    #[test]
    fn custom_editor_writes_a_template_first() {
        let mut app = config_app(Section::Editor);
        let custom = testing::find::<editor::ChooseCustom>(&mut app, |_| true);
        testing::activate(&mut app, custom);
        assert_eq!(
            testing::recorded(&mut app),
            [
                Ask::SetConfig {
                    key: "editor.custom_command".into(),
                    value: editor::DEFAULT_CUSTOM.into()
                },
                Ask::SetConfig {
                    key: "editor.kind".into(),
                    value: "custom".into()
                },
            ]
        );
        testing::set_config_locally(&mut app, "editor.custom_command", "nvim +{line} {path}");
        testing::settle(&mut app);
        let custom = testing::find::<editor::ChooseCustom>(&mut app, |_| true);
        testing::activate(&mut app, custom);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::SetConfig {
                key: "editor.kind".into(),
                value: "custom".into()
            }],
            "an existing command is kept"
        );
    }

    #[test]
    fn editor_test_button_opens_the_config_file() {
        let home = std::env::temp_dir().join(format!("clusia-app-test-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let mut app = testing::app(fixture::demo(NOW));
        app.insert_resource(AppPaths(Paths::new(&home)));
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Config);
        app.world_mut()
            .resource_mut::<Nav>()
            .open_section(Section::Editor);
        testing::settle(&mut app);
        let test = testing::find::<editor::TestOpen>(&mut app, |_| true);
        // No config.toml yet: a toast, no ask.
        testing::activate(&mut app, test);
        assert!(testing::recorded(&mut app).is_empty());
        let toasts = &app.world().resource::<Toasts>().0;
        assert_eq!(toasts.len(), 1);
        assert!(toasts[0].text.contains("has not been written yet"));
        // Once it exists, the ask goes out.
        let file = home.join("config.toml");
        std::fs::write(&file, "").unwrap();
        testing::activate(&mut app, test);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::OpenInEditor {
                path: file.display().to_string(),
                line: Some(1)
            }]
        );
        std::fs::remove_dir_all(&home).ok();
    }

    /// The asks recorded since the last call, without the status refreshes the open page makes
    /// on its own.
    fn asks(app: &mut App) -> Vec<Ask> {
        let mut asks = testing::recorded(app);
        asks.retain(|a| *a != Ask::RefreshStatus);
        asks
    }

    /// The Notifications page with the status refresh it asks for on opening already taken.
    fn notifications_app() -> App {
        let mut app = config_app(Section::Notifications);
        let _ = testing::recorded(&mut app);
        app
    }

    fn set_value_of(app: &mut App, key: &str, value: &str) -> Entity {
        testing::find::<SetValue>(app, |s| s.key == key && s.value == value)
    }

    #[test]
    fn general_is_the_first_section() {
        assert_eq!(Section::ALL[0], Section::General);
        assert_eq!(Section::General.label(), "General");
    }

    #[test]
    fn start_at_login_asks_the_daemon_and_shows_a_refusal() {
        let mut app = config_app(Section::General);
        let toggle =
            testing::find::<Sends>(&mut app, |s| s.0 == Ask::SetStartAtLogin { on: false });
        testing::activate(&mut app, toggle);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::SetStartAtLogin { on: false }]
        );
        app.world_mut().resource_mut::<Model>().rejected.insert(
            crate::bridge::START_AT_LOGIN.into(),
            "cannot write the login agent".into(),
        );
        testing::settle(&mut app);
        testing::find::<FieldError>(&mut app, |e| e.0 == crate::bridge::START_AT_LOGIN);
        assert!(page_texts(&mut app).contains(&"cannot write the login agent".to_string()));
    }

    #[test]
    fn start_at_login_follows_the_saved_setting() {
        let mut app = config_app(Section::General);
        testing::set_config_locally(&mut app, "general.start_at_login", "false");
        testing::find::<Sends>(&mut app, |s| s.0 == Ask::SetStartAtLogin { on: true });
    }

    #[test]
    fn event_checkboxes_write_their_keys() {
        let mut app = notifications_app();
        let checks_macos =
            set_value_of(&mut app, "notifications.events.checks_failed.macos", "true");
        testing::activate(&mut app, checks_macos);
        let reply_sound = set_value_of(&mut app, "notifications.events.reply_to_you.sound", "true");
        testing::activate(&mut app, reply_sound);
        let review_tray = set_value_of(
            &mut app,
            "notifications.events.review_requested.tray",
            "false",
        );
        testing::activate(&mut app, review_tray);
        let set = |key: &str, value: &str| Ask::SetConfig {
            key: key.into(),
            value: value.into(),
        };
        assert_eq!(
            asks(&mut app),
            [
                set("notifications.events.checks_failed.macos", "true"),
                set("notifications.events.reply_to_you.sound", "true"),
                set("notifications.events.review_requested.tray", "false"),
            ]
        );
    }

    #[test]
    fn the_agent_row_is_shown_but_cannot_be_changed() {
        let mut app = notifications_app();
        assert_eq!(
            testing::count::<SetValue>(&mut app),
            7 * 3 + 4 + 1 + 1 + 1 + 7,
            "21 event boxes, 4 sounds, quiet hours on/off, follow focus, group bursts, 7 weekdays"
        );
        let mut q = app.world_mut().query::<&SetValue>();
        assert!(q.iter(app.world()).all(|s| !s.key.contains("agent")));
        let texts = page_texts(&mut app);
        assert!(texts.contains(&"Agent finished a review".to_string()));
        assert!(texts.contains(&"Agent needs your permission".to_string()));
    }

    #[test]
    fn the_sound_is_chosen_and_previewed() {
        let mut app = notifications_app();
        let drop = set_value_of(&mut app, "notifications.sound", "drop");
        testing::activate(&mut app, drop);
        assert_eq!(
            asks(&mut app),
            [Ask::SetConfig {
                key: "notifications.sound".into(),
                value: "drop".into()
            }]
        );
        let play = testing::find::<notifications::PreviewSound>(&mut app, |_| true);
        testing::activate(&mut app, play);
        assert_eq!(
            app.world()
                .resource::<crate::platform_sound::PlayedSounds>()
                .0,
            [clusia_core::config::SoundId::Leaf]
        );
        testing::set_config_locally(&mut app, "notifications.sound", "chime");
        testing::settle(&mut app);
        let play = testing::find::<notifications::PreviewSound>(&mut app, |_| true);
        testing::activate(&mut app, play);
        assert_eq!(
            app.world()
                .resource::<crate::platform_sound::PlayedSounds>()
                .0
                .last(),
            Some(&clusia_core::config::SoundId::Chime),
            "the button plays the sound that is chosen now"
        );
    }

    #[test]
    fn quiet_hours_weekdays_and_times() {
        let mut app = notifications_app();
        // Quiet hours start off, Monday to Friday.
        let off = set_value_of(&mut app, "notifications.dnd.enabled", "true");
        testing::activate(&mut app, off);
        let no_friday = set_value_of(
            &mut app,
            "notifications.dnd.days",
            r#"["mon","tue","wed","thu"]"#,
        );
        testing::activate(&mut app, no_friday);
        let saturday = set_value_of(
            &mut app,
            "notifications.dnd.days",
            r#"["mon","tue","wed","thu","fri","sat"]"#,
        );
        testing::activate(&mut app, saturday);
        let from = testing::find::<ConfigField>(&mut app, |f| f.0 == "notifications.dnd.from");
        app.world_mut().write_message(FieldCommitted {
            entity: from,
            value: " 20:30 ".into(),
        });
        app.update();
        let set = |key: &str, value: &str| Ask::SetConfig {
            key: key.into(),
            value: value.into(),
        };
        assert_eq!(
            asks(&mut app),
            [
                set("notifications.dnd.enabled", "true"),
                set("notifications.dnd.days", r#"["mon","tue","wed","thu"]"#),
                set(
                    "notifications.dnd.days",
                    r#"["mon","tue","wed","thu","fri","sat"]"#
                ),
                set("notifications.dnd.from", "20:30"),
            ]
        );
        app.world_mut().resource_mut::<Model>().rejected.insert(
            "notifications.dnd.from".into(),
            "invalid value for notifications.dnd.from: not a time".into(),
        );
        testing::settle(&mut app);
        testing::find::<FieldError>(&mut app, |e| e.0 == "notifications.dnd.from");
    }

    #[test]
    fn follow_focus_and_group_bursts_toggle() {
        let mut app = notifications_app();
        let focus = set_value_of(&mut app, "notifications.follow_focus", "false");
        testing::activate(&mut app, focus);
        let group = set_value_of(&mut app, "notifications.group_bursts", "false");
        testing::activate(&mut app, group);
        assert_eq!(asks(&mut app).len(), 2);
    }

    #[test]
    fn the_permission_line_follows_the_daemon() {
        let mut app = notifications_app();
        let has = |app: &mut App, text: &str| page_texts(app).iter().any(|t| t == text);
        assert!(has(&mut app, "Allowed"));
        let status = |app: &mut App, s| {
            app.world_mut()
                .resource_mut::<Model>()
                .snapshot
                .notifications_permission = s;
            testing::settle(app);
        };
        status(&mut app, clusia_protocol::PermissionStatus::Denied);
        assert!(has(&mut app, "Off"));
        status(&mut app, clusia_protocol::PermissionStatus::NotDetermined);
        assert!(has(&mut app, "Not asked yet"));
        let settings = testing::find::<OpenLink>(&mut app, |l| {
            l.0 == crate::platform_open::NOTIFICATION_SETTINGS
        });
        testing::activate(&mut app, settings);
        assert_eq!(
            app.world().resource::<OpenUrls>().0,
            [crate::platform_open::NOTIFICATION_SETTINGS]
        );
    }

    #[test]
    fn the_test_button_asks_for_a_notification() {
        let mut app = notifications_app();
        let _ = asks(&mut app);
        let test = testing::find::<Sends>(&mut app, |s| s.0 == Ask::TestNotification);
        testing::activate(&mut app, test);
        assert_eq!(asks(&mut app), [Ask::TestNotification]);
    }

    #[test]
    fn the_open_page_reads_the_permission_again() {
        let mut app = testing::app(fixture::demo(NOW));
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Config);
        let _ = testing::recorded(&mut app);
        app.world_mut()
            .resource_mut::<Nav>()
            .open_section(Section::Notifications);
        testing::settle(&mut app);
        let refreshes = |asks: Vec<Ask>| asks.iter().filter(|a| **a == Ask::RefreshStatus).count();
        assert_eq!(refreshes(testing::recorded(&mut app)), 1, "once on opening");
        testing::settle(&mut app);
        assert_eq!(
            refreshes(testing::recorded(&mut app)),
            0,
            "then every few seconds"
        );
        app.world_mut()
            .resource_mut::<Nav>()
            .open_section(Section::Media);
        testing::settle(&mut app);
        assert_eq!(
            refreshes(testing::recorded(&mut app)),
            0,
            "not on other pages"
        );
    }

    #[test]
    fn media_page_controls() {
        let mut app = config_app(Section::Media);
        let toggle = testing::find::<SetValue>(&mut app, |s| {
            s.key == "media.load_external_images" && s.value == "true"
        });
        testing::activate(&mut app, toggle);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::SetConfig {
                key: "media.load_external_images".into(),
                value: "true".into()
            }]
        );
        testing::find::<media::PasteGiphyKey>(&mut app, |_| true);
        let remove = testing::find::<Sends>(&mut app, |s| s.0 == Ask::ClearGiphyKey);
        testing::activate(&mut app, remove);
        assert_eq!(testing::recorded(&mut app), [Ask::ClearGiphyKey]);
        let how = testing::find::<OpenLink>(&mut app, |l| l.0.contains("developers.giphy.com"));
        testing::activate(&mut app, how);
        assert_eq!(
            app.world().resource::<OpenUrls>().0,
            ["https://developers.giphy.com/"]
        );
    }

    #[test]
    fn media_page_without_a_key_has_nothing_to_remove() {
        let mut snap = fixture::demo(NOW);
        snap.giphy_key = crate::snapshot::GiphyKey::Missing;
        let mut app = testing::app(snap);
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Config);
        app.world_mut()
            .resource_mut::<Nav>()
            .open_section(Section::Media);
        testing::settle(&mut app);
        testing::find::<media::PasteGiphyKey>(&mut app, |_| true);
        assert_eq!(testing::count::<Sends>(&mut app), 0);
    }

    #[test]
    fn paste_without_a_clipboard_warns_and_sends_nothing() {
        let mut app = config_app(Section::Media);
        let paste = testing::find::<media::PasteGiphyKey>(&mut app, |_| true);
        testing::activate(&mut app, paste);
        assert!(testing::recorded(&mut app).is_empty());
        assert_eq!(app.world().resource::<Toasts>().0.len(), 1);
    }

    #[test]
    fn about_page_links_every_credit() {
        let mut app = config_app(Section::About);
        assert_eq!(
            testing::count::<OpenLink>(&mut app),
            about::CREDITS.len(),
            "one link per credit"
        );
        let twemoji = testing::find::<OpenLink>(&mut app, |l| l.0.contains("jdecked/twemoji"));
        testing::activate(&mut app, twemoji);
        assert_eq!(
            app.world().resource::<OpenUrls>().0,
            ["https://github.com/jdecked/twemoji"]
        );
    }

    fn page_texts(app: &mut App) -> Vec<String> {
        let mut q = app.world_mut().query::<&Text>();
        q.iter(app.world()).map(|t| t.0.clone()).collect()
    }

    #[test]
    fn a_search_the_window_saw_refused_marks_the_key_rejected() {
        let mut app = config_app(Section::Media);
        let line = |app: &mut App, text: &str| page_texts(app).iter().any(|t| t == text);
        assert!(line(&mut app, "Key saved in the macOS Keychain"));
        testing::tell(
            &mut app,
            Tell::Gifs(Err((
                ErrorCode::Unauthorized,
                "Giphy rejected the key".into(),
            ))),
        );
        testing::settle(&mut app);
        assert!(line(&mut app, "Giphy rejected the key. Paste a new one."));
        // A search that works again clears it.
        testing::tell(
            &mut app,
            Tell::Gifs(Ok(GifPage {
                items: Vec::new(),
                next_offset: None,
            })),
        );
        testing::settle(&mut app);
        assert!(line(&mut app, "Key saved in the macOS Keychain"));
        assert!(
            !app.world()
                .resource::<Model>()
                .rejected
                .contains_key(crate::bridge::GIPHY_KEY_REFUSAL)
        );
    }

    #[test]
    fn a_new_key_status_clears_an_old_refusal() {
        let mut app = config_app(Section::Media);
        let line = |app: &mut App, text: &str| page_texts(app).iter().any(|t| t == text);
        testing::tell(
            &mut app,
            Tell::Gifs(Err((
                ErrorCode::Unauthorized,
                "Giphy rejected the key".into(),
            ))),
        );
        testing::settle(&mut app);
        assert!(line(&mut app, "Giphy rejected the key. Paste a new one."));
        let mut snap = app.world().resource::<Model>().snapshot.clone();
        snap.giphy_key = crate::snapshot::GiphyKey::Missing;
        testing::tell(&mut app, Tell::Snapshot(Box::new(snap.clone())));
        snap.giphy_key = crate::snapshot::GiphyKey::Set;
        testing::tell(&mut app, Tell::Snapshot(Box::new(snap)));
        testing::settle(&mut app);
        assert!(line(&mut app, "Key saved in the macOS Keychain"));
    }

    #[test]
    fn a_snapshot_with_the_same_key_status_keeps_the_refusal() {
        let mut app = config_app(Section::Media);
        testing::tell(
            &mut app,
            Tell::Gifs(Err((
                ErrorCode::Unauthorized,
                "Giphy rejected the key".into(),
            ))),
        );
        let snap = app.world().resource::<Model>().snapshot.clone();
        testing::tell(&mut app, Tell::Snapshot(Box::new(snap)));
        testing::settle(&mut app);
        assert!(
            page_texts(&mut app).contains(&"Giphy rejected the key. Paste a new one.".to_string())
        );
    }

    #[test]
    fn every_section_has_a_page() {
        let mut app = config_app(Section::Appearance);
        for section in Section::ALL {
            app.world_mut().resource_mut::<Nav>().open_section(section);
            testing::settle(&mut app);
            testing::find::<PageOf>(&mut app, |p| p.0 == section);
        }
        app.world_mut()
            .resource_mut::<Nav>()
            .open_section(Section::Harness);
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<ConfigField>(&mut app),
            0,
            "Harness has no live settings yet"
        );
    }
}
