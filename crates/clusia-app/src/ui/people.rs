//! People on a pull request: an author's name and round avatar that open the profile on
//! GitHub, and the card that follows the pointer to them.
//!
//! The avatar comes through the media cache, so it keeps that cache's limits and host checks;
//! until it loads, or when it never does, the initial stays. The profile is asked for the first
//! time the pointer rests on a person and kept for the session. The card is a layer of its own
//! at the root of the window, so no scroll area clips it, and it takes the pointer over its whole area: a press on its
//! padding does not reach the diff underneath.

use std::collections::HashMap;
use std::time::Duration;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::picking::Pickable;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui::{ComputedNode, UiGlobalTransform};
use bevy::ui_widgets::{Activate, Button as WidgetButton, observe};
use bevy::window::PrimaryWindow;
use clusia_core::Profile;
use clusia_core::people::{avatar_url, is_login, profile_url};

use crate::bridge::{Ask, Asks, MediaError, Model};
use crate::fonts::UiFonts;
use crate::platform_open::{OpenUrls, visit};
use crate::theme::{Swatch, Theme};
use crate::ui::composer::popover::panel_spot;
use crate::ui::kit::{
    Clickable, Fill, HoverFill, Ink, Stroke, Type, Variant, button, initial, text, wrapped_text,
};
use crate::ui::media::{MediaCache, MediaState};
use crate::ui::spinner::Motion;
use crate::ui::tween::{Ease, Look, Tween, start_now};

/// The width of the card.
const CARD_WIDTH: f32 = 280.0;
const CARD_AVATAR: f32 = 44.0;
/// The most characters of a bio the card shows.
const BIO_CHARS: usize = 160;
const CARD_Z: i32 = 130;

/// How long the pointer rests on a person before the card opens, and how long it may be away
/// (on its way to the card) before the card closes.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delays {
    pub open: Duration,
    pub close: Duration,
}

impl Default for Delays {
    fn default() -> Self {
        Self {
            open: Duration::from_millis(300),
            close: Duration::from_millis(250),
        }
    }
}

/// A person on screen the pointer can rest on: the node it rests on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub login: String,
    pub anchor: Entity,
}

/// Which person's card is open, and who the pointer is resting on. Pure: `step` is told what is
/// hovered and for how long.
#[derive(Resource, Debug, Default, Clone, PartialEq, Eq)]
pub struct HoverCard {
    shown: Option<Target>,
    pending: Option<Target>,
    /// A login whose card is held open without the pointer (scenes).
    pinned: Option<String>,
    /// Time spent on `pending` (or away from `shown`).
    waited: Duration,
}

impl HoverCard {
    /// Holds the card of `login` open, as if the pointer rested on that name.
    pub fn pin(&mut self, login: &str) {
        self.pinned = Some(login.to_string());
    }

    /// The person whose card is open.
    pub fn shown(&self) -> Option<&Target> {
        self.shown.as_ref()
    }

    /// The person whose card is open or about to open, whose profile is wanted.
    pub fn wanted(&self) -> Option<&Target> {
        self.pending.as_ref().or(self.shown.as_ref())
    }

    /// Moves `dt` forward with `hovered` under the pointer (the card itself counts as its
    /// person).
    pub fn step(&mut self, hovered: Option<Target>, dt: Duration, delays: Delays) {
        match hovered {
            Some(h) if self.shown.as_ref() == Some(&h) => {
                self.pending = None;
                self.waited = Duration::ZERO;
            }
            Some(h) => {
                if self.pending.as_ref() != Some(&h) {
                    self.pending = Some(h);
                    self.waited = Duration::ZERO;
                }
                self.waited += dt;
                if self.waited >= delays.open {
                    self.shown = self.pending.take();
                    self.waited = Duration::ZERO;
                }
            }
            None => {
                // The time spent on a name that never opened is not time spent away.
                if self.pending.take().is_some() {
                    self.waited = Duration::ZERO;
                }
                if self.shown.is_some() {
                    self.waited += dt;
                    if self.waited >= delays.close {
                        self.shown = None;
                        self.waited = Duration::ZERO;
                    }
                } else {
                    self.waited = Duration::ZERO;
                }
            }
        }
    }
}

/// What the window knows of one person's profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileState {
    Loading,
    Loaded(Profile),
    /// GitHub has no such user; asking again would get the same answer.
    Missing(String),
    /// The connection failed; the next hover asks again.
    Failed(String),
}

/// Profiles by login. One entry per login, so a person on every comment is asked for once.
#[derive(Resource, Debug, Default)]
pub struct People {
    pub by_login: HashMap<String, ProfileState>,
    arrived: Vec<(String, Result<Profile, MediaError>)>,
}

impl People {
    /// Queues the daemon's answer for `login`.
    pub fn arrive(&mut self, login: String, profile: Result<Profile, MediaError>) {
        self.arrived.push((login, profile));
    }

    /// Forgets the failures that may have passed, so the next hover asks again.
    pub fn retry_failed(&mut self) {
        self.by_login
            .retain(|_, state| !matches!(state, ProfileState::Failed(_)));
    }
}

/// A node that stands for `login` under the pointer: the name and the avatar. Only a plain
/// login (`is_login`) has a card and a profile to open; any other name is a target the hover
/// ignores.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PersonTarget(pub String);

/// The round avatar of `login`, initials until the picture is in.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PersonAvatar(pub String);

/// Set once the picture replaced the initials.
#[derive(Component)]
struct PictureShown;

/// The open card.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CardRoot {
    pub login: String,
    /// What the card was last built from.
    built: Option<ProfileState>,
}

/// The **Open on GitHub** button of the card of this login.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CardOpen(pub String);

fn github_host(model: &Model) -> &str {
    &model.snapshot.config.github.host
}

/// The `@login` that opens the profile, drawn in the ink colour at weight 600. A name that is
/// not a plain login is only text.
pub fn person_name(fonts: &UiFonts, login: &str) -> impl Bundle {
    (
        Node {
            padding: UiRect::horizontal(px(3)),
            border_radius: BorderRadius::all(px(4)),
            // A login is at most 39 characters: it keeps its width beside other text and breaks
            // only when it is wider than the box that holds it.
            flex_shrink: 0.0,
            max_width: percent(100),
            ..default()
        },
        (WidgetButton, Clickable),
        Hovered::default(),
        BackgroundColor::default(),
        Fill(Swatch::Clear),
        HoverFill(Swatch::Hover),
        PersonTarget(login.to_string()),
        observe(open_profile),
        children![(
            wrapped_text(fonts, format!("@{login}"), Type::STRONG),
            Node {
                max_width: percent(100),
                ..default()
            },
        )],
    )
}

/// A round avatar `side` px wide: the person's initial until the picture loads.
pub fn person_avatar(fonts: &UiFonts, login: &str, side: f32) -> impl Bundle {
    (
        Node {
            width: px(side),
            height: px(side),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            border_radius: BorderRadius::MAX,
            flex_shrink: 0.0,
            ..default()
        },
        (WidgetButton, Clickable),
        Hovered::default(),
        BackgroundColor::default(),
        Fill(Swatch::Chrome),
        PersonTarget(login.to_string()),
        PersonAvatar(login.to_string()),
        observe(open_profile),
        children![text(
            fonts,
            initial(login),
            Type {
                size: (side / 2.0).max(11.0),
                weight: 600,
                ink: Swatch::Muted,
                mono: false,
            }
        )],
    )
}

/// The avatar and the name of `login`, side by side.
pub fn person(p: &mut ChildSpawnerCommands, fonts: &UiFonts, login: &str, side: f32) {
    p.spawn(person_avatar(fonts, login, side));
    p.spawn(person_name(fonts, login));
}

fn open_profile(
    activate: On<Activate>,
    targets: Query<&PersonTarget>,
    model: Res<Model>,
    urls: Option<ResMut<OpenUrls>>,
) {
    let Ok(PersonTarget(login)) = targets.get(activate.entity) else {
        return;
    };
    if let Some(url) = profile_url(github_host(&model), login) {
        visit(urls, &url);
    }
}

fn open_card_profile(
    activate: On<Activate>,
    buttons: Query<&CardOpen>,
    model: Res<Model>,
    urls: Option<ResMut<OpenUrls>>,
) {
    let Ok(CardOpen(login)) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(url) = profile_url(github_host(&model), login) {
        visit(urls, &url);
    }
}

/// Puts the picture of each avatar in place of its initial once the media cache has it, asking
/// for it the first time.
fn fill_avatars(
    mut commands: Commands,
    model: Res<Model>,
    people: Res<People>,
    mut cache: ResMut<MediaCache>,
    mut asks: ResMut<Asks>,
    faces: Query<(Entity, &PersonAvatar), Without<PictureShown>>,
) {
    for (entity, PersonAvatar(login)) in &faces {
        let url =
            avatar_url(github_host(&model), login).or_else(|| match people.by_login.get(login) {
                Some(ProfileState::Loaded(p)) => p.avatar_url.clone(),
                _ => None,
            });
        let Some(url) = url else { continue };
        let handle = match cache.want(&url, &mut asks) {
            MediaState::Image(handle) => handle.clone(),
            MediaState::Gif(gif) => gif.first.clone(),
            _ => continue,
        };
        commands
            .entity(entity)
            .despawn_related::<Children>()
            .insert((ImageNode::new(handle), PictureShown));
    }
}

fn decode_profiles(mut people: ResMut<People>) {
    for (login, answer) in std::mem::take(&mut people.arrived) {
        let state = match answer {
            Ok(profile) => ProfileState::Loaded(profile),
            Err(e) if e.transient => ProfileState::Failed(e.message),
            Err(e) => ProfileState::Missing(e.message),
        };
        people.by_login.insert(login, state);
    }
}

/// Feeds the pointer's whereabouts to the card and asks for the profile of whoever it is
/// resting on.
fn track_hover(
    time: Res<Time>,
    delays: Res<Delays>,
    mut card: ResMut<HoverCard>,
    mut people: ResMut<People>,
    mut asks: ResMut<Asks>,
    targets: Query<(Entity, &PersonTarget, &Hovered, Has<PersonAvatar>)>,
    cards: Query<(&CardRoot, &Hovered)>,
) {
    // A pinned person counts as hovered; the name is the better anchor than the avatar.
    let on_person = targets
        .iter()
        .filter(|(_, PersonTarget(login), h, _)| {
            is_login(login) && (h.get() || card.pinned.as_deref() == Some(login))
        })
        .min_by_key(|(_, _, _, avatar)| *avatar)
        .map(|(anchor, PersonTarget(login), _, _)| Target {
            login: login.clone(),
            anchor,
        });
    // The card stands for its person, so crossing the gap onto it keeps it open.
    let on_card = cards
        .iter()
        .find(|(_, h)| h.get())
        .and_then(|(root, _)| card.shown().filter(|t| t.login == root.login).cloned());
    let hovered = on_person.or(on_card);
    let before = card.shown().cloned();
    // A pinned card has no pointer to wait for.
    let delays = if card.pinned.is_some() {
        Delays {
            open: Duration::ZERO,
            ..*delays
        }
    } else {
        *delays
    };
    card.step(hovered, time.delta(), delays);
    if before.is_some() && card.shown().is_none() {
        people.retry_failed();
    }
    if let Some(want) = card.wanted()
        && is_login(&want.login)
        && !people.by_login.contains_key(&want.login)
    {
        people
            .by_login
            .insert(want.login.clone(), ProfileState::Loading);
        asks.send(Ask::FetchProfile(want.login.clone()));
    }
}

fn card_text(p: &mut ChildSpawnerCommands, fonts: &UiFonts, s: &str, t: Type) {
    p.spawn((
        wrapped_text(fonts, s.to_string(), t),
        Node {
            min_width: px(0),
            ..default()
        },
    ));
}

fn bio_line(bio: &str) -> String {
    if bio.chars().count() <= BIO_CHARS {
        return bio.to_string();
    }
    let cut: String = bio.chars().take(BIO_CHARS).collect();
    format!("{}…", cut.trim_end())
}

fn build_card(p: &mut ChildSpawnerCommands, fonts: &UiFonts, login: &str, state: &ProfileState) {
    let profile = match state {
        ProfileState::Loaded(p) => Some(p),
        _ => None,
    };
    p.spawn(Node {
        column_gap: px(12),
        align_items: AlignItems::Center,
        ..default()
    })
    .with_children(|head| {
        head.spawn(person_avatar(fonts, login, CARD_AVATAR));
        head.spawn(Node {
            flex_direction: FlexDirection::Column,
            flex_grow: 1.0,
            flex_shrink: 1.0,
            min_width: px(0),
            row_gap: px(2),
            ..default()
        })
        .with_children(|names| {
            if let Some(name) = profile.and_then(|p| p.name.as_deref()) {
                card_text(names, fonts, name, Type::HEADING);
            }
            card_text(names, fonts, &format!("@{login}"), Type::META);
        });
    });
    match state {
        ProfileState::Loading => card_text(p, fonts, "Loading…", Type::META),
        ProfileState::Missing(why) | ProfileState::Failed(why) => {
            card_text(p, fonts, why, Type::META);
        }
        ProfileState::Loaded(profile) => {
            if let Some(company) = &profile.company {
                card_text(p, fonts, company, Type::MUTED);
            }
            if let Some(bio) = &profile.bio {
                card_text(p, fonts, &bio_line(bio), Type::MUTED);
            }
        }
    }
    p.spawn((
        button(fonts, "Open on GitHub", Variant::Secondary),
        CardOpen(login.to_string()),
        observe(open_card_profile),
    ));
}

/// Keeps the card of `HoverCard::shown` on screen, built from what is known of the person.
fn sync_card(
    mut commands: Commands,
    fonts: Res<UiFonts>,
    card: Res<HoverCard>,
    people: Res<People>,
    mut roots: Query<(Entity, &mut CardRoot)>,
) {
    let want = card.shown().map(|t| t.login.as_str());
    let mut present = false;
    for (entity, mut root) in &mut roots {
        if Some(root.login.as_str()) != want || present {
            commands.entity(entity).despawn();
            continue;
        }
        present = true;
        let state = people.by_login.get(&root.login).cloned();
        if root.built != state {
            let login = root.login.clone();
            root.built = state.clone();
            commands
                .entity(entity)
                .despawn_related::<Children>()
                .with_children(|c| {
                    build_card(c, &fonts, &login, &state.unwrap_or(ProfileState::Loading));
                });
        }
    }
    let Some(login) = want else { return };
    if present {
        return;
    }
    let state = people
        .by_login
        .get(login)
        .cloned()
        .unwrap_or(ProfileState::Loading);
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: px(0),
                top: px(0),
                width: px(CARD_WIDTH),
                flex_direction: FlexDirection::Column,
                row_gap: px(10),
                padding: px(14).all(),
                border: px(1).all(),
                border_radius: BorderRadius::all(px(12)),
                overflow: Overflow::clip(),
                ..default()
            },
            BackgroundColor::default(),
            Fill(Swatch::Surface),
            BorderColor::default(),
            Stroke(Swatch::Line),
            GlobalZIndex(CARD_Z),
            // Shown by `place_card` once it has been laid out where it belongs.
            Visibility::Hidden,
            Pickable {
                should_block_lower: true,
                is_hoverable: true,
            },
            Hovered::default(),
            CardRoot {
                login: login.to_string(),
                built: Some(state.clone()),
            },
        ))
        .with_children(|c| build_card(c, &fonts, login, &state));
}

/// Puts the card under the node it opened from (or above it when there is no room), inside the
/// window.
fn place_card(
    mut commands: Commands,
    card: Res<HoverCard>,
    motion: Res<Motion>,
    anchors: Query<(&ComputedNode, &UiGlobalTransform)>,
    mut roots: Query<(Entity, &mut Node, &mut Visibility, &ComputedNode), With<CardRoot>>,
    theme: Res<Theme>,
    children: Query<&Children>,
    painted: Query<(Option<&Fill>, Option<&Ink>, Option<&Stroke>, Has<ImageNode>)>,
    windows: Query<&Window, With<PrimaryWindow>>,
) {
    let Some(target) = card.shown() else { return };
    let Ok((anchor, at)) = anchors.get(target.anchor) else {
        return;
    };
    let window = windows
        .iter()
        .next()
        .map_or(Vec2::splat(f32::MAX), |w| Vec2::new(w.width(), w.height()));
    let scale = anchor.inverse_scale_factor;
    let rect = Rect::from_center_size(at.translation * scale, anchor.size * scale);
    for (entity, mut node, mut visibility, size) in &mut roots {
        let spot = panel_spot(rect, size.size * size.inverse_scale_factor, window);
        let (l, t, h) = (px(spot.at.x), px(spot.at.y), px(spot.max_height));
        if node.left != l || node.top != t || node.max_height != h {
            node.left = l;
            node.top = t;
            node.max_height = h;
        } else if *visibility != Visibility::Inherited {
            *visibility = Visibility::Inherited;
            if !motion.reduced {
                appear(&mut commands, &theme, entity, &children, &painted);
            }
        }
    }
}

/// How long the card takes to appear, and the size it grows from.
pub const CARD_IN_MS: u64 = 120;
pub const CARD_FROM: f32 = 0.97;

/// The card grows into place, and it and every colour and picture in it fade in: what is under a floating
/// card is not one colour a veil could take. It is shown in the frame this runs, after
/// `animate`, so the start is put on at once.
fn appear(
    commands: &mut Commands,
    theme: &Theme,
    card: Entity,
    children: &Query<&Children>,
    painted: &Query<(Option<&Fill>, Option<&Ink>, Option<&Stroke>, Has<ImageNode>)>,
) {
    let grow = Tween::new(
        CARD_IN_MS,
        Ease::OutCubic,
        Look::REST.scaled(CARD_FROM).faded(0.0),
        Look::REST,
    );
    let fade = Tween::new(
        CARD_IN_MS,
        Ease::OutCubic,
        Look::REST.faded(0.0),
        Look::REST,
    );
    let marks = |e| painted.get(e).unwrap_or((None, None, None, false));
    let (fill, ink, stroke, _) = marks(card);
    start_now(commands, theme, card, grow, (fill, ink, stroke));
    for part in children.iter_descendants(card) {
        let (fill, ink, stroke, picture) = marks(part);
        if fill.is_some() || ink.is_some() || stroke.is_some() || picture {
            start_now(commands, theme, part, fade, (fill, ink, stroke));
        }
    }
}

pub struct PeoplePlugin;

impl Plugin for PeoplePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<People>()
            .init_resource::<HoverCard>()
            .init_resource::<Delays>()
            .add_systems(
                Update,
                (decode_profiles, track_hover, sync_card, fill_avatars).chain(),
            )
            .add_systems(
                PostUpdate,
                place_card.after(bevy::ui::UiSystems::PostLayout),
            );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bevy::ecs::system::RunSystemOnce;
    use bevy::picking::hover::Hovered;
    use clusia_core::Profile;
    use clusia_core::media::MediaKind;
    use clusia_protocol::MediaFile;

    use super::*;
    use crate::bridge::{Ask, MediaError, Tell};
    use crate::fixture;
    use crate::fonts::UiFonts;
    use crate::platform_open::OpenUrls;
    use crate::snapshot::Snapshot;
    use crate::testing;

    const MS: Duration = Duration::from_millis(1);

    fn target(login: &str, anchor: u32) -> Target {
        Target {
            login: login.into(),
            anchor: Entity::from_raw_u32(anchor).unwrap(),
        }
    }

    #[test]
    fn the_card_opens_after_a_pause_and_closes_after_another() {
        let delays = Delays {
            open: 300 * MS,
            close: 200 * MS,
        };
        let mut card = HoverCard::default();
        let octo = target("octo", 1);
        card.step(Some(octo.clone()), 100 * MS, delays);
        assert_eq!(card.shown(), None, "not yet");
        card.step(Some(octo.clone()), 100 * MS, delays);
        card.step(Some(octo.clone()), 100 * MS, delays);
        assert_eq!(card.shown(), Some(&octo));
        card.step(None, 150 * MS, delays);
        assert_eq!(
            card.shown(),
            Some(&octo),
            "the pointer may be on its way to the card"
        );
        card.step(Some(octo.clone()), 10 * MS, delays);
        card.step(None, 150 * MS, delays);
        assert_eq!(card.shown(), Some(&octo), "coming back restarts the wait");
        card.step(None, 100 * MS, delays);
        assert_eq!(card.shown(), None);
    }

    #[test]
    fn another_person_replaces_the_card_only_after_the_pause() {
        let delays = Delays {
            open: 300 * MS,
            close: 200 * MS,
        };
        let mut card = HoverCard::default();
        let (octo, mona) = (target("octo", 1), target("mona", 2));
        card.step(Some(octo.clone()), 300 * MS, delays);
        assert_eq!(card.shown(), Some(&octo));
        card.step(Some(mona.clone()), 100 * MS, delays);
        assert_eq!(card.shown(), Some(&octo));
        card.step(Some(mona.clone()), 250 * MS, delays);
        assert_eq!(card.shown(), Some(&mona));
        assert_eq!(card.wanted(), Some(&mona));
    }

    #[test]
    fn a_brushed_name_never_opens_the_card() {
        let delays = Delays::default();
        let mut card = HoverCard::default();
        card.step(Some(target("octo", 1)), delays.open / 2, delays);
        card.step(None, delays.close * 2, delays);
        card.step(Some(target("mona", 2)), delays.open / 2, delays);
        assert_eq!(card.shown(), None);
    }

    fn app() -> App {
        let mut app = testing::app(Snapshot::default());
        app.insert_resource(Delays {
            open: Duration::ZERO,
            close: Duration::ZERO,
        });
        app
    }

    fn spawn(app: &mut App, login: &'static str) -> (Entity, Entity) {
        app.world_mut()
            .run_system_once(move |mut commands: Commands, fonts: Res<UiFonts>| {
                let name = commands.spawn(person_name(&fonts, login)).id();
                let face = commands.spawn(person_avatar(&fonts, login, 22.0)).id();
                (name, face)
            })
            .unwrap()
    }

    fn shown(app: &mut App) -> Vec<String> {
        testing::shown_text(app)
    }

    fn png(dir: &std::path::Path) -> MediaFile {
        let path = dir.join("a.png");
        let bytes = fixture::demo_media("https://avatars.githubusercontent.com/octo?s=96")
            .expect("the demo has avatars")
            .1;
        std::fs::write(&path, &bytes).unwrap();
        MediaFile {
            path: path.display().to_string(),
            kind: MediaKind::Png,
            bytes: bytes.len() as u64,
        }
    }

    #[test]
    fn clicking_a_name_or_a_face_opens_the_profile() {
        let mut app = app();
        let (name, face) = spawn(&mut app, "octo");
        app.update();
        testing::activate(&mut app, name);
        testing::activate(&mut app, face);
        let urls = &app.world().resource::<OpenUrls>().0;
        assert_eq!(
            urls,
            &["https://github.com/octo", "https://github.com/octo"]
        );
    }

    #[test]
    fn the_profile_follows_the_configured_host() {
        let mut app = app();
        testing::set_config_locally(&mut app, "github.host", "ghe.example.com");
        let (name, _) = spawn(&mut app, "mona");
        app.update();
        testing::activate(&mut app, name);
        assert_eq!(
            app.world().resource::<OpenUrls>().0,
            ["https://ghe.example.com/mona"]
        );
    }

    #[test]
    fn a_login_that_is_not_a_plain_name_opens_nothing() {
        let mut app = app();
        let (name, _) = spawn(&mut app, "dependabot[bot]");
        app.update();
        testing::activate(&mut app, name);
        assert!(app.world().resource::<OpenUrls>().0.is_empty());
    }

    #[test]
    fn the_name_is_the_login_with_an_at_sign() {
        let mut app = app();
        spawn(&mut app, "octo");
        app.update();
        assert!(shown(&mut app).contains(&"@octo".to_string()));
    }

    #[test]
    fn initials_show_while_the_avatar_loads_and_when_it_fails() {
        let mut app = app();
        let (_, face) = spawn(&mut app, "octo");
        app.update();
        let asked = testing::recorded_with_avatars(&mut app);
        assert_eq!(
            asked,
            [Ask::FetchMedia(
                "https://avatars.githubusercontent.com/octo?s=96".into()
            )]
        );
        assert!(shown(&mut app).contains(&"O".to_string()));
        testing::tell(
            &mut app,
            Tell::Media {
                url: "https://avatars.githubusercontent.com/octo?s=96".into(),
                file: Err(MediaError::refused("no")),
            },
        );
        app.update();
        assert!(app.world().get::<ImageNode>(face).is_none());
        assert!(shown(&mut app).contains(&"O".to_string()));
        assert!(
            testing::recorded_with_avatars(&mut app).is_empty(),
            "it asks once"
        );
    }

    #[test]
    fn a_loaded_avatar_replaces_the_initials() {
        let mut app = app();
        let (_, face) = spawn(&mut app, "octo");
        app.update();
        let dir = tempfile::tempdir().unwrap();
        testing::tell(
            &mut app,
            Tell::Media {
                url: "https://avatars.githubusercontent.com/octo?s=96".into(),
                file: Ok(png(dir.path())),
            },
        );
        testing::decoded(&mut app);
        assert!(app.world().get::<ImageNode>(face).is_some());
        assert!(!shown(&mut app).contains(&"O".to_string()));
    }

    #[test]
    fn other_hosts_start_with_initials_and_ask_for_no_picture() {
        let mut app = app();
        testing::set_config_locally(&mut app, "github.host", "ghe.example.com");
        spawn(&mut app, "octo");
        app.update();
        assert!(testing::recorded_with_avatars(&mut app).is_empty());
    }

    fn hover(app: &mut App, entity: Entity, on: bool) {
        app.world_mut().entity_mut(entity).insert(Hovered(on));
        app.update();
    }

    fn profile(login: &str) -> Profile {
        fixture::demo_profile(login).expect("a demo person")
    }

    #[test]
    fn hovering_a_name_asks_for_the_profile_once_and_fills_the_card() {
        let mut app = app();
        let (name, _) = spawn(&mut app, "mona");
        app.update();
        testing::recorded(&mut app);
        hover(&mut app, name, true);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::FetchProfile("mona".into())]
        );
        assert!(shown(&mut app).contains(&"Loading…".to_string()));
        testing::tell(
            &mut app,
            Tell::Profile {
                login: "mona".into(),
                profile: Ok(profile("mona")),
            },
        );
        app.update();
        let text = shown(&mut app);
        for want in ["Mona Lisa", "@mona", "Acme Widgets", "Open on GitHub"] {
            assert!(text.contains(&want.to_string()), "{want} in {text:?}");
        }
        hover(&mut app, name, false);
        hover(&mut app, name, true);
        assert!(testing::recorded(&mut app).is_empty(), "cached per login");
    }

    #[test]
    fn open_on_github_opens_the_profile() {
        let mut app = app();
        let (name, _) = spawn(&mut app, "hubot");
        app.update();
        hover(&mut app, name, true);
        testing::tell(
            &mut app,
            Tell::Profile {
                login: "hubot".into(),
                profile: Ok(profile("hubot")),
            },
        );
        app.update();
        let open = testing::find::<CardOpen>(&mut app, |c| c.0 == "hubot");
        testing::activate(&mut app, open);
        assert_eq!(
            app.world().resource::<OpenUrls>().0,
            ["https://github.com/hubot"]
        );
    }

    #[test]
    fn a_pinned_card_opens_without_the_pointer_and_fills_from_what_is_known() {
        let mut app = app();
        spawn(&mut app, "octo");
        app.update();
        app.world_mut().resource_mut::<HoverCard>().pin("octo");
        app.world_mut()
            .resource_mut::<People>()
            .by_login
            .insert("octo".into(), ProfileState::Loaded(profile("octo")));
        app.update();
        app.update();
        let text = shown(&mut app);
        for want in ["Octo Cat", "@acme", "Open on GitHub"] {
            assert!(text.contains(&want.to_string()), "{want} in {text:?}");
        }
    }

    #[test]
    fn the_card_sits_just_under_the_name_and_inside_the_window() {
        let mut app = testing::layout_app(fixture::demo(testing::NOW), UVec2::new(1280, 800));
        testing::open_ready(&mut app, false);
        app.world_mut().resource_mut::<HoverCard>().pin("octo");
        app.world_mut()
            .resource_mut::<People>()
            .by_login
            .insert("octo".into(), ProfileState::Loaded(profile("octo")));
        for _ in 0..6 {
            app.update();
        }
        let card = testing::find::<CardRoot>(&mut app, |c| c.login == "octo");
        assert_eq!(
            app.world().get::<Visibility>(card),
            Some(&Visibility::Inherited),
            "shown once placed"
        );
        let anchor = app.world().resource::<HoverCard>().shown().unwrap().anchor;
        let (ax, ay, _, ah) = testing::frame(&app, anchor);
        let (x, y, w, h) = testing::frame(&app, card);
        assert!((x - ax).abs() <= 1.0, "left edges line up: {x} vs {ax}");
        assert!(y >= ay + ah && y - (ay + ah) <= 12.0, "just below: {y}");
        assert!(x >= 0.0 && x + w <= 1280.0 && y + h <= 800.0);
    }

    #[test]
    fn the_card_shows_clear_and_small_then_grows_and_fades_in_with_what_is_in_it() {
        let mut app = testing::layout_app(fixture::demo(testing::NOW), UVec2::new(1280, 800));
        testing::open_ready(&mut app, false);
        testing::run_tweens(&mut app);
        app.world_mut().resource_mut::<HoverCard>().pin("octo");
        app.world_mut()
            .resource_mut::<People>()
            .by_login
            .insert("octo".into(), ProfileState::Loaded(profile("octo")));
        let card = loop {
            app.update();
            let mut q = app.world_mut().query::<(Entity, &CardRoot, &Visibility)>();
            if let Some((e, _, _)) = q
                .iter(app.world())
                .find(|(_, c, v)| c.login == "octo" && **v == Visibility::Inherited)
            {
                break e;
            }
        };
        let w = app.world();
        assert_eq!(
            w.get::<UiTransform>(card).unwrap().scale,
            Vec2::splat(CARD_FROM)
        );
        assert_eq!(w.get::<BackgroundColor>(card).unwrap().0.alpha(), 0.0);
        let texts: Vec<Entity> = {
            let mut q = app.world_mut().query_filtered::<Entity, With<Ink>>();
            q.iter(app.world())
                .filter(|e| {
                    std::iter::successors(Some(*e), |e| {
                        app.world().get::<ChildOf>(*e).map(|c| c.parent())
                    })
                    .any(|a| a == card)
                })
                .collect()
        };
        assert!(!texts.is_empty());
        for t in &texts {
            assert_eq!(app.world().get::<TextColor>(*t).unwrap().0.alpha(), 0.0);
        }
        testing::run_tweens(&mut app);
        assert_eq!(
            app.world().get::<UiTransform>(card).unwrap().scale,
            Vec2::ONE
        );
        assert_eq!(
            app.world().get::<BackgroundColor>(card).unwrap().0,
            crate::theme::LIGHT.surface
        );
        for t in &texts {
            assert_eq!(app.world().get::<TextColor>(*t).unwrap().0.alpha(), 1.0);
        }
    }

    #[test]
    fn a_login_that_is_not_plain_has_no_card_and_asks_nothing() {
        let mut app = app();
        let (name, face) = spawn(&mut app, "dependabot[bot]");
        app.update();
        testing::recorded_with_avatars(&mut app);
        hover(&mut app, name, true);
        hover(&mut app, face, true);
        app.world_mut()
            .resource_mut::<HoverCard>()
            .pin("dependabot[bot]");
        app.update();
        app.update();
        assert_eq!(testing::count::<CardRoot>(&mut app), 0);
        assert!(testing::recorded_with_avatars(&mut app).is_empty());
    }

    #[test]
    fn leaving_after_a_brush_of_another_name_keeps_the_full_close_grace() {
        let delays = Delays {
            open: 300 * MS,
            close: 250 * MS,
        };
        let mut card = HoverCard::default();
        let (octo, mona) = (target("octo", 1), target("mona", 2));
        card.step(Some(octo.clone()), 300 * MS, delays);
        card.step(Some(mona), 200 * MS, delays);
        card.step(None, 200 * MS, delays);
        assert_eq!(card.shown(), Some(&octo), "200 of 250 ms away");
        card.step(None, 50 * MS, delays);
        assert_eq!(card.shown(), None);
    }

    #[test]
    fn the_card_closes_when_the_pointer_leaves() {
        let mut app = app();
        let (name, _) = spawn(&mut app, "octo");
        app.update();
        hover(&mut app, name, true);
        assert_eq!(testing::count::<CardRoot>(&mut app), 1);
        hover(&mut app, name, false);
        app.update();
        assert_eq!(testing::count::<CardRoot>(&mut app), 0);
    }

    #[test]
    fn a_failed_profile_still_offers_the_link_and_is_asked_again_next_time() {
        let mut app = app();
        let (name, _) = spawn(&mut app, "octo");
        app.update();
        hover(&mut app, name, true);
        testing::recorded(&mut app);
        testing::tell(
            &mut app,
            Tell::Profile {
                login: "octo".into(),
                profile: Err(MediaError::transient("Not connected to clusiad")),
            },
        );
        app.update();
        let text = shown(&mut app);
        assert!(text.contains(&"@octo".to_string()), "{text:?}");
        assert!(text.contains(&"Open on GitHub".to_string()));
        assert!(text.contains(&"Not connected to clusiad".to_string()));
        hover(&mut app, name, false);
        app.update();
        hover(&mut app, name, true);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::FetchProfile("octo".into())]
        );
    }

    #[test]
    fn an_unknown_user_is_not_asked_for_again() {
        let mut app = app();
        let (name, _) = spawn(&mut app, "ghost");
        app.update();
        hover(&mut app, name, true);
        testing::recorded(&mut app);
        testing::tell(
            &mut app,
            Tell::Profile {
                login: "ghost".into(),
                profile: Err(MediaError::refused("Not Found")),
            },
        );
        hover(&mut app, name, false);
        app.update();
        hover(&mut app, name, true);
        assert!(testing::recorded(&mut app).is_empty());
    }
}
