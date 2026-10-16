//! Where the window is (Home, a Config section or a review tab, spec §7.2) and the chrome
//! around it: the top bar, the connection banner and the toasts.

use bevy::asset::RenderAssetUsages;
use bevy::ecs::system::NonSendMarker;
use bevy::image::{CompressedImageFormats, ImageSampler, ImageType};
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::events::{Pointer, Press};
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, Button as WidgetButton, observe};
use bevy::window::{PrimaryWindow, RequestRedraw};
use clusia_core::PrRef;
use clusia_protocol::WindowTarget;

use crate::app::StartTarget;
use crate::bridge::{Ask, Asks, Connection, Model, ShowRequested, Toast, Toasts};
use crate::fonts::UiFonts;
use crate::platform_window::{Foregrounds, bring_forward};
use crate::screens::review::activity::Work;
use crate::screens::review::agent::model::Chats;
use crate::screens::review::checks::Checks;
use crate::screens::review::diagrams::Diagrams;
use crate::screens::review::revalidation::Revalidations;
use crate::snapshot::Snapshot;
use crate::theme::Swatch;
use crate::ui::kit::{
    Fill, HoverFill, Icon, Stroke, Type, Variant, button, icon_button, panel, text,
};
use crate::ui::spinner::spinner;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    General,
    Appearance,
    GitServer,
    Repositories,
    Harness,
    Editor,
    Notifications,
    Plugins,
    Media,
    About,
}

impl Section {
    pub const ALL: [Section; 10] = [
        Section::General,
        Section::Appearance,
        Section::GitServer,
        Section::Repositories,
        Section::Harness,
        Section::Editor,
        Section::Media,
        Section::Notifications,
        Section::Plugins,
        Section::About,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Section::General => "General",
            Section::Appearance => "Appearance",
            Section::GitServer => "Git server",
            Section::Repositories => "Repositories",
            Section::Harness => "Harness",
            Section::Editor => "Editor",
            Section::Notifications => "Notifications",
            Section::Plugins => "Plugins & skills",
            Section::Media => "Media",
            Section::About => "About",
        }
    }
}

impl Section {
    /// The id a deep link uses for this section.
    pub fn page_id(self) -> &'static str {
        match self {
            Section::General => "general",
            Section::Appearance => "appearance",
            Section::GitServer => "git",
            Section::Repositories => "repositories",
            Section::Harness => "harness",
            Section::Editor => "editor",
            Section::Media => "media",
            Section::Notifications => "notifications",
            Section::Plugins => "plugins",
            Section::About => "about",
        }
    }

    /// The section a deep link names; an id nobody knows opens Appearance.
    pub fn from_page(page: &str) -> Section {
        Section::ALL
            .into_iter()
            .find(|s| s.page_id() == page)
            .unwrap_or(Section::Appearance)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    Home,
    /// Stands in for Home while the GitHub login is missing (`screens::first_run`).
    FirstRun,
    Config(Section),
    Review(PrRef),
}

#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct Nav {
    pub screen: Screen,
    /// Open review tabs, left to right.
    pub reviews: Vec<PrRef>,
    /// The Config section shown when Config opens.
    pub section: Section,
}

impl Nav {
    pub fn new(target: &WindowTarget) -> Self {
        let mut nav = Nav {
            screen: Screen::Home,
            reviews: Vec::new(),
            section: Section::Appearance,
        };
        nav.go(target);
        nav
    }

    pub fn go(&mut self, target: &WindowTarget) {
        self.screen = match target {
            WindowTarget::Home => Screen::Home,
            WindowTarget::Config => Screen::Config(self.section),
            WindowTarget::ConfigPage { page } => {
                self.section = Section::from_page(page);
                Screen::Config(self.section)
            }
            WindowTarget::Review { pr } => {
                if !self.reviews.contains(pr) {
                    self.reviews.push(pr.clone());
                }
                Screen::Review(pr.clone())
            }
        };
    }

    pub fn open_section(&mut self, section: Section) {
        self.section = section;
        self.screen = Screen::Config(section);
    }

    /// Brings back the tabs the window had: theirs first, in their order, then any opened since
    /// it started (`--review`, another launch). The one they showed is shown again, unless the
    /// window was asked for something else.
    pub fn restore(&mut self, saved: &clusia_core::OpenTabs) {
        let opened = std::mem::take(&mut self.reviews);
        for pr in saved.tabs.iter().map(|t| &t.pr).chain(&opened) {
            if !self.reviews.contains(pr) {
                self.reviews.push(pr.clone());
            }
        }
        if self.screen == Screen::Home
            && let Some(pr) = saved
                .selected
                .as_ref()
                .filter(|pr| self.reviews.contains(pr))
        {
            self.screen = Screen::Review(pr.clone());
        }
    }

    /// Closes a review tab. When it was showing, the tab on its left takes over (else the
    /// first tab, else Home).
    pub fn close_review(&mut self, pr: &PrRef) {
        let Some(i) = self.reviews.iter().position(|p| p == pr) else {
            return;
        };
        self.reviews.remove(i);
        if self.screen == Screen::Review(pr.clone()) {
            let next = i
                .checked_sub(1)
                .and_then(|j| self.reviews.get(j))
                .or_else(|| self.reviews.first());
            self.screen = match next {
                Some(p) => Screen::Review(p.clone()),
                None => Screen::Home,
            };
        }
    }
}

/// The title of `pr` if it appears in any list.
pub fn pr_title<'a>(snap: &'a Snapshot, pr: &PrRef) -> Option<&'a str> {
    snap.assigned
        .iter()
        .chain(&snap.mine)
        .find(|p| &p.pr == pr)
        .map(|p| p.title.as_str())
        .or_else(|| {
            snap.reviews
                .iter()
                .find(|r| &r.pr == pr)
                .map(|r| r.title.as_str())
        })
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub enum TabTarget {
    Home,
    Config,
    Review(PrRef),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TabView {
    pub target: TabTarget,
    pub label: String,
    pub on: bool,
    /// Agent work runs for this review: the tab carries a spinner.
    pub working: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TopBarView {
    pub tabs: Vec<TabView>,
    pub config_on: bool,
}

pub fn top_bar_view(nav: &Nav, snap: &Snapshot) -> TopBarView {
    let mut tabs = vec![TabView {
        target: TabTarget::Home,
        label: "Home".into(),
        on: matches!(nav.screen, Screen::Home | Screen::FirstRun),
        working: false,
    }];
    for pr in &nav.reviews {
        let label = match pr_title(snap, pr) {
            Some(title) => format!("#{} {}", pr.number, truncate(title, 30)),
            None => format!("#{}", pr.number),
        };
        tabs.push(TabView {
            target: TabTarget::Review(pr.clone()),
            label,
            on: nav.screen == Screen::Review(pr.clone()),
            working: false,
        });
    }
    TopBarView {
        tabs,
        config_on: matches!(nav.screen, Screen::Config(_)),
    }
}

/// Containers the screen plugins fill.
#[derive(Component, Debug)]
pub struct HomeScreen;

#[derive(Component, Debug)]
pub struct ConfigScreen;

#[derive(Component, Debug)]
pub struct FirstRunScreen;

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ReviewScreen(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CloseTab(pub PrRef);

/// The `+` after the review tabs: opens the palette.
#[derive(Component, Debug)]
pub struct NewTabButton;

/// A review tab's × was pressed; `screens::review::leave` decides whether to ask first.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct TabCloseRequested(pub PrRef);

#[derive(Component, Debug)]
pub struct RetryButton;

#[derive(Component, Debug)]
pub struct ToastCard;

#[derive(Component)]
struct TopBar {
    built: Option<TopBarView>,
}

#[derive(Component)]
struct BannerSlot {
    built: Option<Connection>,
}

#[derive(Component)]
struct ScreenRoot {
    built: Option<Screen>,
}

#[derive(Component)]
struct ToastSlot {
    built: Option<Vec<Toast>>,
}

/// The brand mark for the Home tab (empty in headless tests).
#[derive(Resource, Debug, Clone, Default)]
pub struct Leaf(pub Handle<Image>);

pub struct NavPlugin;

impl Plugin for NavPlugin {
    fn build(&self, app: &mut App) {
        let start = app
            .world()
            .get_resource::<StartTarget>()
            .map(|s| s.0.clone())
            .unwrap_or(WindowTarget::Home);
        app.insert_resource(Nav::new(&start))
            .add_message::<TabCloseRequested>()
            .add_systems(Startup, (load_leaf, spawn_chrome))
            .add_systems(
                Update,
                (
                    show,
                    expire_toasts,
                    rebuild_top_bar,
                    rebuild_banner,
                    rebuild_screen,
                    rebuild_toasts,
                )
                    .chain()
                    .in_set(NavSystems),
            );
    }
}

/// The navigation and chrome systems. Screen plugins run `.after(NavSystems)`, so a freshly
/// spawned screen container is filled in the same frame.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct NavSystems;

fn load_leaf(mut commands: Commands, images: Option<ResMut<Assets<Image>>>) {
    let handle = images
        .and_then(|mut images| {
            Image::from_buffer(
                include_bytes!("../assets/leaf.png"),
                ImageType::Extension("png"),
                CompressedImageFormats::NONE,
                true,
                ImageSampler::default(),
                RenderAssetUsages::default(),
            )
            .ok()
            .map(|image| images.add(image))
        })
        .unwrap_or_default();
    commands.insert_resource(Leaf(handle));
}

fn spawn_chrome(mut commands: Commands) {
    commands
        .spawn(panel(
            Node {
                width: percent(100),
                height: percent(100),
                flex_direction: FlexDirection::Column,
                ..default()
            },
            Swatch::Bg,
        ))
        .with_children(|root| {
            root.spawn((
                panel(
                    Node {
                        height: px(44),
                        flex_shrink: 0.0,
                        align_items: AlignItems::Center,
                        column_gap: px(4),
                        padding: UiRect {
                            left: px(84),
                            right: px(12),
                            ..default()
                        },
                        border: UiRect::bottom(px(1)),
                        ..default()
                    },
                    Swatch::Chrome,
                ),
                BorderColor::default(),
                Stroke(Swatch::Line),
                TopBar { built: None },
                observe(drag_window),
            ));
            root.spawn((
                Node {
                    flex_direction: FlexDirection::Column,
                    flex_shrink: 0.0,
                    ..default()
                },
                BannerSlot { built: None },
            ));
            root.spawn((
                Node {
                    flex_grow: 1.0,
                    min_height: px(0),
                    ..default()
                },
                ScreenRoot { built: None },
            ));
            root.spawn((
                Node {
                    position_type: PositionType::Absolute,
                    right: px(16),
                    bottom: px(16),
                    flex_direction: FlexDirection::Column,
                    row_gap: px(8),
                    ..default()
                },
                GlobalZIndex(10),
                ToastSlot { built: None },
            ));
        });
}

/// The top bar is the title bar: pressing on its empty parts moves the window.
fn drag_window(_press: On<Pointer<Press>>, mut windows: Query<&mut Window, With<PrimaryWindow>>) {
    if let Ok(mut window) = windows.single_mut() {
        window.start_drag_move();
    }
}

fn show(
    mut requests: MessageReader<ShowRequested>,
    mut nav: ResMut<Nav>,
    mut windows: Query<&mut Window, With<PrimaryWindow>>,
    mut foregrounds: Option<ResMut<Foregrounds>>,
    main_thread: NonSendMarker,
) {
    if requests.is_empty() {
        return;
    }
    // One activation per frame covers any number of requests.
    bring_forward(foregrounds.as_deref_mut(), main_thread);
    for ShowRequested(target) in requests.read() {
        nav.go(target);
        if let Ok(mut window) = windows.single_mut() {
            window.set_minimized(false);
            window.focused = true;
        }
    }
}

fn tab_node(on: bool) -> impl Bundle {
    (
        Node {
            height: px(30),
            padding: UiRect::horizontal(px(12)),
            align_items: AlignItems::Center,
            column_gap: px(8),
            border: px(1).all(),
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        WidgetButton,
        Hovered::default(),
        TabIndex(0),
        BackgroundColor::default(),
        Fill(if on { Swatch::Surface } else { Swatch::Clear }),
        HoverFill(if on { Swatch::Surface } else { Swatch::Hover }),
        BorderColor::default(),
        Stroke(if on { Swatch::Line } else { Swatch::Clear }),
    )
}

fn rebuild_top_bar(
    mut commands: Commands,
    nav: Res<Nav>,
    model: Res<Model>,
    fonts: Res<UiFonts>,
    leaf: Res<Leaf>,
    chats: Res<Chats>,
    checks: Res<Checks>,
    diagrams: Res<Diagrams>,
    revalidations: Res<Revalidations>,
    mut bars: Query<(Entity, &mut TopBar)>,
) {
    let mut view = top_bar_view(&nav, &model.snapshot);
    for tab in &mut view.tabs {
        if let TabTarget::Review(pr) = &tab.target {
            tab.working = Work::of(pr, &chats, &checks, &diagrams, &revalidations).any();
        }
    }
    for (entity, mut bar) in &mut bars {
        if bar.built.as_ref() == Some(&view) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        commands.entity(entity).with_children(|p| {
            for tab in &view.tabs {
                let label = if tab.on { Type::STRONG } else { Type::MUTED };
                let mut e = p.spawn((tab_node(tab.on), tab.target.clone(), observe(on_tab)));
                match &tab.target {
                    TabTarget::Home => {
                        e.with_children(|t| {
                            t.spawn((
                                Node {
                                    width: px(16),
                                    height: px(16),
                                    ..default()
                                },
                                ImageNode::new(leaf.0.clone()),
                            ));
                            t.spawn(text(&fonts, tab.label.clone(), label));
                        });
                    }
                    TabTarget::Review(pr) => {
                        e.with_children(|t| {
                            if tab.working {
                                t.spawn(spinner());
                            }
                            t.spawn(text(&fonts, tab.label.clone(), label));
                            t.spawn((
                                icon_button(Icon::Close, Variant::Ghost, 22.0),
                                CloseTab(pr.clone()),
                                observe(on_close),
                            ));
                        });
                    }
                    TabTarget::Config => {}
                }
            }
            // Buttons sit at the top of their row; this one is centered like the tabs.
            p.spawn((
                icon_button(Icon::Plus, Variant::Ghost, 30.0),
                NewTabButton,
                observe(on_new_tab),
            ))
            .entry::<Node>()
            .and_modify(|mut node| node.align_self = AlignSelf::Center);
            p.spawn(Node {
                flex_grow: 1.0,
                ..default()
            });
            // A secondary button, not a tab: it is a place to go, and it must read as pressable.
            let mut config = p.spawn((
                button(&fonts, "Config", Variant::Secondary),
                TabTarget::Config,
                observe(on_tab),
            ));
            config
                .entry::<Node>()
                .and_modify(|mut node| node.align_self = AlignSelf::Center);
            if view.config_on {
                config.insert((Fill(Swatch::Raised), HoverFill(Swatch::Raised)));
            }
        });
        bar.built = Some(view.clone());
    }
}

fn on_tab(activate: On<Activate>, tabs: Query<&TabTarget>, mut nav: ResMut<Nav>) {
    let Ok(tab) = tabs.get(activate.entity) else {
        return;
    };
    let target = match tab {
        TabTarget::Home => WindowTarget::Home,
        TabTarget::Config => WindowTarget::Config,
        TabTarget::Review(pr) => WindowTarget::Review { pr: pr.clone() },
    };
    nav.go(&target);
}

fn on_close(
    activate: On<Activate>,
    close: Query<&CloseTab>,
    mut out: MessageWriter<TabCloseRequested>,
) {
    if let Ok(CloseTab(pr)) = close.get(activate.entity) {
        out.write(TabCloseRequested(pr.clone()));
    }
}

fn on_new_tab(_activate: On<Activate>, mut palette: ResMut<crate::screens::open_pr::Palette>) {
    *palette = crate::screens::open_pr::Palette {
        open: true,
        ..Default::default()
    };
}

fn rebuild_banner(
    mut commands: Commands,
    model: Res<Model>,
    fonts: Res<UiFonts>,
    mut slots: Query<(Entity, &mut BannerSlot)>,
) {
    for (entity, mut slot) in &mut slots {
        if slot.built.as_ref() == Some(&model.connection) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        let strip = |fill| {
            panel(
                Node {
                    padding: UiRect::axes(px(20), px(8)),
                    column_gap: px(12),
                    align_items: AlignItems::Center,
                    ..default()
                },
                fill,
            )
        };
        commands
            .entity(entity)
            .with_children(|p| match &model.connection {
                Connection::Live => {}
                Connection::Connecting => {
                    p.spawn(strip(Swatch::Chrome)).with_children(|s| {
                        s.spawn(text(&fonts, "Connecting to clusiad…", Type::MUTED));
                    });
                }
                Connection::Lost(reason) => {
                    p.spawn(strip(Swatch::OrangeSoft)).with_children(|s| {
                        s.spawn(text(
                            &fonts,
                            format!(
                                "Lost the connection to clusiad: {reason}. Showing the last data."
                            ),
                            Type::BODY.ink(Swatch::Orange),
                        ));
                        s.spawn((
                            button(&fonts, "Retry", Variant::Secondary),
                            RetryButton,
                            observe(on_retry),
                        ));
                    });
                }
            });
        slot.built = Some(model.connection.clone());
    }
}

fn on_retry(_activate: On<Activate>, mut asks: ResMut<Asks>, mut model: ResMut<Model>) {
    asks.send(Ask::Reconnect);
    model.connection = Connection::Connecting;
}

fn rebuild_screen(
    mut commands: Commands,
    nav: Res<Nav>,
    mut roots: Query<(Entity, &mut ScreenRoot)>,
) {
    // Config sections switch inside `ConfigScreen`; here only the kind of screen matters.
    let kind = match &nav.screen {
        Screen::Config(_) => Screen::Config(Section::Appearance),
        other => other.clone(),
    };
    for (entity, mut root) in &mut roots {
        if root.built.as_ref() == Some(&kind) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        let fill = Node {
            flex_grow: 1.0,
            width: percent(100),
            min_height: px(0),
            ..default()
        };
        // The review plugin fills `ReviewScreen` according to the tab's phase.
        commands
            .entity(entity)
            .with_children(|p| match &nav.screen {
                Screen::Home => {
                    p.spawn((fill, HomeScreen));
                }
                Screen::FirstRun => {
                    p.spawn((fill, FirstRunScreen));
                }
                Screen::Config(_) => {
                    p.spawn((fill, ConfigScreen));
                }
                Screen::Review(pr) => {
                    p.spawn((fill, ReviewScreen(pr.clone())));
                }
            });
        root.built = Some(kind.clone());
    }
}

fn expire_toasts(
    time: Res<Time>,
    mut toasts: ResMut<Toasts>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let now = time.elapsed_secs_f64();
    if toasts.0.iter().any(|t| t.until <= now) {
        toasts.0.retain(|t| t.until > now);
    }
    if !toasts.0.is_empty() {
        // Keep the reactive loop ticking until the last toast has gone.
        redraw.write(RequestRedraw);
    }
}

fn rebuild_toasts(
    mut commands: Commands,
    toasts: Res<Toasts>,
    fonts: Res<UiFonts>,
    mut slots: Query<(Entity, &mut ToastSlot)>,
) {
    for (entity, mut slot) in &mut slots {
        if slot.built.as_ref() == Some(&toasts.0) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        commands.entity(entity).with_children(|p| {
            for toast in &toasts.0 {
                let t = if toast.warning {
                    Type::BODY.ink(Swatch::Orange)
                } else {
                    Type::BODY
                };
                p.spawn((
                    crate::ui::kit::card(Node {
                        padding: UiRect::axes(px(14), px(10)),
                        max_width: px(420),
                        ..default()
                    }),
                    ToastCard,
                ))
                .with_children(|c| {
                    c.spawn(text(&fonts, toast.text.clone(), t));
                });
            }
        });
        slot.built = Some(toasts.0.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::testing::{self, NOW};

    fn tab_frames(app: &mut App) -> (Entity, Entity, Entity) {
        let pr = testing::open_ready(app, false);
        testing::settle(app);
        let close = testing::find::<CloseTab>(app, |c| c.0 == pr);
        let tab = app.world().get::<ChildOf>(close).unwrap().parent();
        (close, tab, testing::find::<NewTabButton>(app, |_| true))
    }

    #[test]
    fn the_tab_icons_sit_on_the_middle_of_the_tab_row() {
        let mut app = testing::layout_app(fixture::demo(NOW), UVec2::new(1280, 800));
        let (close, tab, new_tab) = tab_frames(&mut app);
        let row = testing::center(&app, tab).y;
        for (name, e) in [("×", close), ("+", new_tab)] {
            let at = testing::center(&app, e);
            assert!(
                (at.y - row).abs() < 0.01,
                "{name} at {}, tab at {row}",
                at.y
            );
        }
        let config = testing::find::<TabTarget>(&mut app, |t| *t == TabTarget::Config);
        assert!((testing::center(&app, config).y - row).abs() < 0.01);
    }

    #[test]
    fn config_is_a_button_with_a_border_and_states() {
        use bevy::ui::Pressed;
        let mut app = testing::app(fixture::demo(NOW));
        testing::settle(&mut app);
        let config = testing::find::<TabTarget>(&mut app, |t| *t == TabTarget::Config);
        let w = app.world();
        assert!(w.get::<WidgetButton>(config).is_some());
        assert_eq!(w.get::<Fill>(config), Some(&Fill(Swatch::Surface)));
        assert_eq!(w.get::<Stroke>(config), Some(&Stroke(Swatch::Line)));
        assert_eq!(w.get::<HoverFill>(config), Some(&HoverFill(Swatch::Hover)));
        app.world_mut().entity_mut(config).insert(Pressed);
        app.update();
        let color = app.world().get::<BackgroundColor>(config).unwrap().0;
        assert_eq!(color, crate::theme::LIGHT.line, "pressed");
    }

    fn pr(s: &str) -> PrRef {
        s.parse().unwrap()
    }

    #[test]
    fn a_config_page_target_shows_config() {
        let mut nav = Nav::new(&WindowTarget::Home);
        nav.go(&WindowTarget::ConfigPage { page: "git".into() });
        assert_eq!(nav.screen, Screen::Config(nav.section));
    }

    #[test]
    fn nav_follows_targets() {
        assert_eq!(Nav::new(&WindowTarget::Home).screen, Screen::Home);
        assert_eq!(
            Nav::new(&WindowTarget::Config).screen,
            Screen::Config(Section::Appearance)
        );
        let mut nav = Nav::new(&WindowTarget::Review {
            pr: pr("rzorzal/clusia#1"),
        });
        assert_eq!(nav.reviews, [pr("rzorzal/clusia#1")]);
        nav.go(&WindowTarget::Review {
            pr: pr("rzorzal/clusia#1"),
        });
        assert_eq!(nav.reviews.len(), 1, "no duplicate tabs");
        nav.open_section(Section::Editor);
        nav.go(&WindowTarget::Home);
        nav.go(&WindowTarget::Config);
        assert_eq!(
            nav.screen,
            Screen::Config(Section::Editor),
            "Config reopens the last section"
        );
    }

    #[test]
    fn restoring_keeps_the_saved_order_and_a_review_asked_for_at_start() {
        use clusia_core::OpenTabs;
        let saved = OpenTabs {
            tabs: vec![
                OpenTabs::tab(pr("rzorzal/clusia#1"), "diff"),
                OpenTabs::tab(pr("rzorzal/clusia#2"), "diff"),
                OpenTabs::tab(pr("rzorzal/clusia#3"), "diff"),
            ],
            selected: Some(pr("rzorzal/clusia#3")),
        };
        // `--review #2` (one of them) and `--review #9` (a new one): selected, the others back.
        for (asked, order) in [
            ("rzorzal/clusia#2", vec![1, 2, 3]),
            ("rzorzal/clusia#9", vec![1, 2, 3, 9]),
        ] {
            let mut nav = Nav::new(&WindowTarget::Review { pr: pr(asked) });
            nav.restore(&saved);
            let numbers: Vec<u64> = nav.reviews.iter().map(|p| p.number).collect();
            assert_eq!(numbers, order, "{asked}");
            assert_eq!(nav.screen, Screen::Review(pr(asked)), "{asked}");
        }
        let mut nav = Nav::new(&WindowTarget::Home);
        nav.restore(&saved);
        assert_eq!(nav.screen, Screen::Review(pr("rzorzal/clusia#3")));
        let mut nav = Nav::new(&WindowTarget::Config);
        nav.restore(&saved);
        assert_eq!(
            nav.screen,
            Screen::Config(Section::Appearance),
            "Config was asked for"
        );
    }

    #[test]
    fn closing_tabs_picks_a_neighbor() {
        let mut nav = Nav::new(&WindowTarget::Home);
        for n in 1..=3 {
            nav.go(&WindowTarget::Review {
                pr: pr(&format!("rzorzal/clusia#{n}")),
            });
        }
        nav.close_review(&pr("rzorzal/clusia#3"));
        assert_eq!(nav.screen, Screen::Review(pr("rzorzal/clusia#2")));
        nav.go(&WindowTarget::Home);
        nav.close_review(&pr("rzorzal/clusia#1"));
        assert_eq!(
            nav.screen,
            Screen::Home,
            "closing a background tab stays put"
        );
        nav.go(&WindowTarget::Review {
            pr: pr("rzorzal/clusia#2"),
        });
        nav.close_review(&pr("rzorzal/clusia#2"));
        assert_eq!(nav.screen, Screen::Home);
        assert!(nav.reviews.is_empty());
        nav.close_review(&pr("rzorzal/clusia#9")); // unknown: no-op
    }

    #[test]
    fn top_bar_names_tabs_from_the_lists() {
        let snap = fixture::demo(NOW);
        let mut nav = Nav::new(&WindowTarget::Home);
        nav.go(&WindowTarget::Review {
            pr: pr("rzorzal/clusia#98"),
        });
        nav.go(&WindowTarget::Review {
            pr: pr("acme/widgets#5"),
        });
        let v = top_bar_view(&nav, &snap);
        let labels: Vec<&str> = v.tabs.iter().map(|t| t.label.as_str()).collect();
        assert_eq!(labels, ["Home", "#98 api pagination for long pull…", "#5"]);
        assert!(v.tabs[2].on && !v.tabs[0].on && !v.config_on);
    }

    fn review_tab_marks(app: &mut App, pr: &PrRef) -> usize {
        let tab = testing::find::<TabTarget>(app, |t| *t == TabTarget::Review(pr.clone()));
        testing::marks_in(app, tab)
    }

    #[test]
    fn the_review_tab_spins_while_any_agent_work_runs_for_it() {
        use crate::bridge::{AgentTell, ChecksTell, DiagramsTell, RevalidationTell, Tell};
        use clusia_core::revalidation::RoundState;
        use clusia_protocol::{CheckState, DiagramsState, SessionStateKind};
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        let review = testing::ready(&app, &pr).view.review.clone();
        assert_eq!(review_tab_marks(&mut app, &pr), 0, "idle");
        let home = testing::find::<TabTarget>(&mut app, |t| *t == TabTarget::Home);
        let run = |app: &mut App, on: Tell, off: Tell| {
            testing::tell(app, on);
            testing::settle(app);
            assert_eq!(review_tab_marks(app, &pr), 1, "while it runs");
            testing::tell(app, off);
            testing::settle(app);
            assert_eq!(review_tab_marks(app, &pr), 0, "when it ends");
        };
        let state = |state| {
            Tell::Agent(AgentTell::State {
                pr: pr.clone(),
                state,
            })
        };
        run(
            &mut app,
            state(SessionStateKind::Running),
            state(SessionStateKind::Ready),
        );
        let check = |state| {
            Tell::Checks(ChecksTell::State {
                pr: pr.clone(),
                kind: clusia_core::checks::CheckKind::Audit,
                state,
            })
        };
        run(
            &mut app,
            check(CheckState::Running { activity: None }),
            check(CheckState::Failed {
                message: "It stopped".into(),
            }),
        );
        let drawing = |state| {
            Tell::Diagrams(DiagramsTell::State {
                pr: pr.clone(),
                state,
            })
        };
        run(
            &mut app,
            drawing(DiagramsState::Planning),
            drawing(DiagramsState::Idle),
        );
        let round = |state| {
            Tell::Revalidation(RevalidationTell::Round {
                pr: pr.clone(),
                round: Some(testing::demo_round(&mut review.clone(), state)),
            })
        };
        run(
            &mut app,
            round(RoundState::Running { activity: None }),
            round(RoundState::Done),
        );
        assert_eq!(testing::marks_in(&app, home), 0, "the Home tab never spins");
    }

    #[test]
    fn the_home_tab_stays_lit_during_first_run() {
        let mut nav = Nav::new(&WindowTarget::Home);
        nav.screen = Screen::FirstRun;
        let v = top_bar_view(&nav, &fixture::demo(NOW));
        assert!(v.tabs[0].on && !v.config_on);
    }

    #[test]
    fn chrome_switches_screens() {
        let mut app = testing::app(fixture::demo(NOW));
        assert_eq!(testing::count::<HomeScreen>(&mut app), 1);
        let config = testing::find::<TabTarget>(&mut app, |t| *t == TabTarget::Config);
        testing::activate(&mut app, config);
        app.update();
        assert_eq!(testing::count::<HomeScreen>(&mut app), 0);
        assert_eq!(testing::count::<ConfigScreen>(&mut app), 1);
    }

    #[test]
    fn show_request_switches_and_focuses() {
        let mut app = testing::app(fixture::demo(NOW));
        let window = app
            .world_mut()
            .spawn((
                Window {
                    focused: false,
                    ..default()
                },
                PrimaryWindow,
            ))
            .id();
        let target = WindowTarget::Review {
            pr: pr("rzorzal/clusia#123"),
        };
        app.world_mut().write_message(ShowRequested(target));
        app.update();
        app.update();
        assert_eq!(
            app.world().resource::<Nav>().screen,
            Screen::Review(pr("rzorzal/clusia#123"))
        );
        assert!(app.world().get::<Window>(window).unwrap().focused);
        assert_eq!(
            app.world_mut()
                .get_mut::<Window>(window)
                .unwrap()
                .internal
                .take_minimize_request(),
            Some(false),
            "a minimized window is restored"
        );
        assert_eq!(testing::count::<ReviewScreen>(&mut app), 1);
        let close = testing::find::<CloseTab>(&mut app, |_| true);
        testing::activate(&mut app, close);
        app.update();
        assert_eq!(app.world().resource::<Nav>().screen, Screen::Home);
        assert_eq!(testing::count::<HomeScreen>(&mut app), 1);
    }

    #[test]
    fn second_launch_brings_the_window_forward() {
        let mut app = testing::app(fixture::demo(NOW));
        assert_eq!(app.world().resource::<Foregrounds>().0, 0);
        app.update();
        assert_eq!(
            app.world().resource::<Foregrounds>().0,
            0,
            "nothing asked: the app stays where it is"
        );
        // Two quick launches hand over two targets in one frame: one activation.
        app.world_mut()
            .write_message(ShowRequested(WindowTarget::Config));
        app.world_mut()
            .write_message(ShowRequested(WindowTarget::Home));
        app.update();
        assert_eq!(app.world().resource::<Foregrounds>().0, 1);
        assert_eq!(app.world().resource::<Nav>().screen, Screen::Home);
        app.world_mut()
            .write_message(ShowRequested(WindowTarget::Config));
        app.update();
        assert_eq!(app.world().resource::<Foregrounds>().0, 2);
    }

    #[test]
    fn a_config_page_target_opens_that_section_and_unknown_pages_open_appearance() {
        let page = |id: &str| WindowTarget::ConfigPage { page: id.into() };
        for (id, section) in [
            ("git", Section::GitServer),
            ("notifications", Section::Notifications),
            ("general", Section::General),
            ("repositories", Section::Repositories),
            ("about", Section::About),
        ] {
            let mut app = testing::app(fixture::demo(NOW));
            app.world_mut().write_message(ShowRequested(page(id)));
            app.update();
            assert_eq!(
                app.world().resource::<Nav>().screen,
                Screen::Config(section),
                "{id}"
            );
            assert_eq!(app.world().resource::<Nav>().section, section, "{id}");
        }
        let mut app = testing::app(fixture::demo(NOW));
        app.world_mut()
            .resource_mut::<Nav>()
            .open_section(Section::Media);
        app.world_mut()
            .write_message(ShowRequested(page("no-such-page")));
        app.update();
        assert_eq!(
            app.world().resource::<Nav>().screen,
            Screen::Config(Section::Appearance)
        );
        for section in Section::ALL {
            assert_eq!(
                Section::from_page(Section::page_id(section)),
                section,
                "every section round-trips"
            );
        }
    }

    #[test]
    fn lost_connection_shows_a_banner_with_retry() {
        let mut app = testing::app(fixture::demo(NOW));
        assert_eq!(testing::count::<RetryButton>(&mut app), 0);
        app.world_mut().resource_mut::<Model>().connection =
            Connection::Lost("clusiad closed the connection".into());
        app.update();
        let retry = testing::find::<RetryButton>(&mut app, |_| true);
        testing::activate(&mut app, retry);
        app.update();
        assert_eq!(testing::recorded(&mut app), [Ask::Reconnect]);
        assert_eq!(
            app.world().resource::<Model>().connection,
            Connection::Connecting
        );
        assert_eq!(testing::count::<RetryButton>(&mut app), 0);
    }

    #[test]
    fn toasts_show_and_expire() {
        let mut app = testing::app(fixture::demo(NOW));
        app.world_mut().resource_mut::<Toasts>().0 = vec![
            Toast {
                text: "Token saved in the Keychain".into(),
                warning: false,
                until: 1e9,
            },
            Toast {
                text: "old".into(),
                warning: true,
                until: -1.0,
            },
        ];
        app.update();
        app.update();
        assert_eq!(testing::count::<ToastCard>(&mut app), 1, "expired ones go");
    }

    #[test]
    fn the_new_tab_button_matches_the_tabs_and_is_centered() {
        let mut app = testing::app(fixture::demo(testing::NOW));
        let plus = testing::find::<NewTabButton>(&mut app, |_| true);
        let home = testing::find::<TabTarget>(&mut app, |t| *t == TabTarget::Home);
        let node = |e| app.world().get::<Node>(e).unwrap().clone();
        assert_eq!(node(plus).height, node(home).height, "as tall as a tab");
        assert_eq!(
            node(plus).align_self,
            AlignSelf::Center,
            "centered in the bar"
        );
    }
}
