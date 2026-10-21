//! Short animations of properties that only change how a node is drawn: its `UiTransform`
//! (shift and scale), the alpha of its swatches (`Fill`, `Stroke`, `Ink`) and its `Outline`
//! colour. Nothing here writes a `Node`, so a running tween never lays the window out again.
//!
//! A `Tween` is written once, when the animation starts; `animate` takes its start time from
//! the first frame that sees it, eases it each frame with `set_if_neq`, writes the end value
//! exactly and removes it. While one runs the event loop wakes at about 60 Hz (through `Pace`);
//! when the last one ends nothing is asked and the desktop waits come back. With
//! `Motion::reduced`, or while the window is covered, a tween jumps to its end in the frame that
//! sees it, and nothing wakes for it.

use bevy::prelude::*;
use bevy::ui::UiSystems;
use std::time::Duration;

use crate::theme::{Swatch, Theme};
use crate::ui::kit::{Fill, HoverFill, Ink, Painting, Stroke};
use crate::ui::pace::{Asking, Pace, Wake, WindowCovered};
use crate::ui::spinner::Motion;

/// How often the loop wakes while something animates.
pub const FRAME_WAIT: Duration = Duration::from_millis(16);

/// How a tween moves from its start to its end, as a share of the way at a share of the time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ease {
    Linear,
    OutCubic,
    OutQuart,
    InCubic,
}

impl Ease {
    /// The share of the way at `t` (0 to 1) of the time.
    pub fn at(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Ease::Linear => t,
            Ease::OutCubic => 1.0 - (1.0 - t).powi(3),
            Ease::OutQuart => 1.0 - (1.0 - t).powi(4),
            Ease::InCubic => t.powi(3),
        }
    }
}

/// What a tween moves: a shift in logical pixels, a uniform scale and an alpha that multiplies
/// the alpha of the entity's swatches.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Look {
    pub shift: Vec2,
    pub scale: f32,
    pub alpha: f32,
}

impl Look {
    /// Where every node rests: in place, full size, opaque.
    pub const REST: Look = Look {
        shift: Vec2::ZERO,
        scale: 1.0,
        alpha: 1.0,
    };

    pub const fn shifted(self, x: f32, y: f32) -> Look {
        Look {
            shift: Vec2::new(x, y),
            ..self
        }
    }

    pub const fn scaled(self, scale: f32) -> Look {
        Look { scale, ..self }
    }

    pub const fn faded(self, alpha: f32) -> Look {
        Look { alpha, ..self }
    }

    fn lerp(self, to: Look, k: f32) -> Look {
        Look {
            shift: self.shift.lerp(to.shift, k),
            scale: self.scale + (to.scale - self.scale) * k,
            alpha: self.alpha + (to.alpha - self.alpha) * k,
        }
    }
}

/// One animation of one entity. Only what differs between `from` and `to` is written: a tween
/// that only moves leaves the entity's colours to the systems that own them (`hover`).
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Tween {
    pub from: Look,
    pub to: Look,
    /// The `Outline` colour at the start and at the end.
    pub ring: Option<(Color, Color)>,
    pub secs: f32,
    pub ease: Ease,
    /// `Time::elapsed_secs_f64` of the first frame that saw it.
    start: Option<f64>,
}

impl Tween {
    pub fn new(ms: u64, ease: Ease, from: Look, to: Look) -> Self {
        Self {
            from,
            to,
            ring: None,
            secs: ms as f32 / 1000.0,
            ease,
            start: None,
        }
    }

    /// A tween of the `Outline` colour alone.
    pub fn ring(ms: u64, ease: Ease, from: Color, to: Color) -> Self {
        Self {
            ring: Some((from, to)),
            ..Self::new(ms, ease, Look::REST, Look::REST)
        }
    }

    fn moves(&self) -> bool {
        self.from.shift != self.to.shift || self.from.scale != self.to.scale
    }

    fn fades(&self) -> bool {
        self.from.alpha != self.to.alpha
    }
}

/// Despawns its entity (and what it holds) once `secs` have passed since the first frame that
/// saw it: what fades out goes when the fade ends.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct DespawnAfter {
    pub secs: f32,
    start: Option<f64>,
}

impl DespawnAfter {
    pub fn new(ms: u64) -> Self {
        Self {
            secs: ms as f32 / 1000.0,
            start: None,
        }
    }
}

/// Puts `tween`'s start look on `entity` through `commands`, for a tween begun after `animate`
/// has run in the frame (one revealed after layout): the frame it shows would otherwise draw it
/// at rest before the tween's first frame.
pub fn start_now(
    commands: &mut Commands,
    theme: &Theme,
    entity: Entity,
    tween: Tween,
    (fill, ink, stroke): (Option<&Fill>, Option<&Ink>, Option<&Stroke>),
) {
    let mut e = commands.entity(entity);
    if tween.moves() {
        e.insert(UiTransform {
            translation: Val2::px(tween.from.shift.x, tween.from.shift.y),
            scale: Vec2::splat(tween.from.scale),
            rotation: Rot2::IDENTITY,
        });
    }
    if tween.fades() {
        let alpha = tween.from.alpha;
        if let Some(fill) = fill {
            e.insert(BackgroundColor(faded(theme, fill.0, alpha)));
        }
        if let Some(ink) = ink {
            e.insert(TextColor(faded(theme, ink.0, alpha)));
        }
        if let Some(stroke) = stroke {
            e.insert(BorderColor::all(faded(theme, stroke.0, alpha)));
        }
        e.queue(move |mut entity: EntityWorldMut| {
            if let Some(mut image) = entity.get_mut::<ImageNode>() {
                fade_image(image.bypass_change_detection(), alpha);
            }
        });
    }
    e.insert(tween);
}

/// Whether a tween or a pending despawn is running. Written only when that changes.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Animating(pub bool);

/// A layer of `swatch` over its parent that clears in `ms`: the parent's contents seem to fade
/// in over that colour without a single text being touched. Picking passes through it. Once
/// clear it is hidden, not despawned: taking a node out lays its parent out again, and that
/// re-measures every text under it. It goes with the parent's next rebuild.
pub fn veil(swatch: Swatch, ms: u64, ease: Ease) -> impl Bundle {
    veil_around(swatch, ms, ease, 0.0)
}

/// `veil` reaching `outset` px past its parent's padding box: over a border that wide.
pub fn veil_around(swatch: Swatch, ms: u64, ease: Ease, outset: f32) -> impl Bundle {
    (
        layer(outset),
        ZIndex(VEIL_Z),
        BackgroundColor::default(),
        Fill(swatch),
        bevy::picking::Pickable::IGNORE,
        Veil,
        Tween::new(ms, ease, Look::REST, Look::REST.faded(0.0)),
    )
}

/// A layer of `swatch` over its parent that thickens from clear to opaque in `ms` and stays:
/// what it covers seems to fade out. Picking passes through it.
pub fn shroud(swatch: Swatch, ms: u64, ease: Ease) -> impl Bundle {
    shroud_around(swatch, ms, ease, 0.0)
}

/// `shroud` reaching `outset` px past its parent's padding box.
pub fn shroud_around(swatch: Swatch, ms: u64, ease: Ease, outset: f32) -> impl Bundle {
    (
        layer(outset),
        ZIndex(VEIL_Z),
        BackgroundColor::default(),
        Fill(swatch),
        bevy::picking::Pickable::IGNORE,
        Shroud,
        Tween::new(ms, ease, Look::REST.faded(0.0), Look::REST),
    )
}

fn layer(outset: f32) -> Node {
    Node {
        position_type: PositionType::Absolute,
        left: px(-outset),
        right: px(-outset),
        top: px(-outset),
        bottom: px(-outset),
        ..default()
    }
}

/// A layer spawned by `shroud`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shroud;

/// How long a new screen or section takes to come in, and how far below its place it starts.
pub const ENTER_MS: u64 = 140;
pub const ENTER_RISE: f32 = 4.0;

/// Brings in the contents of `root` that just replaced others: it rises `ENTER_RISE` px into
/// place while a veil of the window's background over `cover` (`root` or the node that holds
/// it) clears. Nothing starts with Reduce Motion.
pub fn enter(commands: &mut Commands, motion: &Motion, root: Entity, cover: Entity) {
    if motion.reduced {
        return;
    }
    commands.entity(root).insert(Tween::new(
        ENTER_MS,
        Ease::OutCubic,
        Look::REST.shifted(0.0, ENTER_RISE),
        Look::REST,
    ));
    commands.spawn((veil(Swatch::Bg, ENTER_MS, Ease::OutCubic), ChildOf(cover)));
}

/// How long a row put into a list takes to come in, how far below its place it starts, and how
/// long a row taken out takes to fade.
pub const ROW_IN_MS: u64 = 160;
pub const ROW_RISE: f32 = 6.0;
pub const ROW_OUT_MS: u64 = 120;

/// Brings in `row`, new in its list: it rises `ROW_RISE` px into place while a veil of
/// `swatch` (what is around the row) over it and `outset` px of border clears. It is spawned
/// at its full size, so the rows after it move once, not along. Nothing starts with Reduce
/// Motion.
pub fn arrive(commands: &mut Commands, motion: &Motion, row: Entity, swatch: Swatch, outset: f32) {
    if motion.reduced {
        return;
    }
    commands.entity(row).insert(Tween::new(
        ROW_IN_MS,
        Ease::OutCubic,
        Look::REST.shifted(0.0, ROW_RISE),
        Look::REST,
    ));
    commands.spawn((
        veil_around(swatch, ROW_IN_MS, Ease::OutCubic, outset),
        ChildOf(row),
    ));
}

/// Fades in `row`, new inside a box that stays (a reply in its thread's card), under a veil
/// of `swatch`, without moving it. Nothing starts with Reduce Motion.
pub fn clear_in(commands: &mut Commands, motion: &Motion, row: Entity, swatch: Swatch) {
    if !motion.reduced {
        commands.spawn((veil(swatch, ROW_IN_MS, Ease::OutCubic), ChildOf(row)));
    }
}

/// Fades out `row`, a copy of a row its list no longer shows, under a shroud of `swatch`, then
/// despawns it; the rows after it move up once, when it goes. With Reduce Motion it goes at
/// once.
pub fn depart(commands: &mut Commands, motion: &Motion, row: Entity, swatch: Swatch, outset: f32) {
    if motion.reduced {
        commands.entity(row).despawn();
        return;
    }
    commands
        .entity(row)
        .insert((DespawnAfter::new(ROW_OUT_MS), Inert));
    commands.spawn((
        shroud_around(swatch, ROW_OUT_MS, Ease::OutCubic, outset),
        ChildOf(row),
    ));
}

/// How long a focus ring takes to come on.
pub const RING_MS: u64 = 100;

/// Shows `want` as `entity`'s outline. A ring coming on fades in over `RING_MS` (linear); one
/// going off or changing colour is written at once. `fading` is the tween `entity` carries.
/// Call it from `Update`, so a fade it stops ends before `animate` runs.
pub fn ring_to(
    commands: &mut Commands,
    motion: &Motion,
    entity: Entity,
    outline: &mut Outline,
    fading: Option<&Tween>,
    want: Color,
) {
    if let Some(fading) = fading {
        if fading.ring.is_some_and(|(_, to)| to == want) {
            return;
        }
        commands.entity(entity).try_remove::<Tween>();
    }
    if outline.color == want {
        return;
    }
    if outline.color == Color::NONE && want != Color::NONE && !motion.reduced {
        commands
            .entity(entity)
            .insert(Tween::ring(RING_MS, Ease::Linear, Color::NONE, want));
    } else {
        outline.color = want;
    }
}

/// Above every sibling of the node it covers.
const VEIL_Z: i32 = 1000;

/// A row on its way out: neither it nor anything in it answers the pointer (`Pickable` is not
/// inherited, so `make_inert` puts it on every part once they are spawned). An inert row also
/// carries `DespawnAfter`, so `make_inert` runs only while something animates.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inert;

fn make_inert(
    mut commands: Commands,
    rows: Query<Entity, Added<Inert>>,
    children: Query<&Children>,
) {
    // It runs before `animate`, which despawns a row at once while the window is covered; an
    // insert into a row gone all the same is skipped, not a panic.
    for row in &rows {
        commands
            .entity(row)
            .try_insert(bevy::picking::Pickable::IGNORE);
        for part in children.iter_descendants(row) {
            commands
                .entity(part)
                .try_insert(bevy::picking::Pickable::IGNORE);
        }
    }
}

/// A layer spawned by `veil`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Veil;

pub struct TweenPlugin;

impl Plugin for TweenPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Animating>()
            .add_systems(
                PostUpdate,
                (animate.run_if(running), idle.run_if(not(running)))
                    .in_set(Asking)
                    .after(Painting)
                    .before(UiSystems::Layout),
            )
            .add_systems(PostUpdate, make_inert.run_if(running).before(animate));
    }
}

fn running(tweens: Query<(), With<Tween>>, despawns: Query<(), With<DespawnAfter>>) -> bool {
    !tweens.is_empty() || !despawns.is_empty()
}

fn idle(mut animating: ResMut<Animating>) {
    animating.set_if_neq(Animating(false));
}

/// The colour of `swatch` with its alpha multiplied by `alpha`.
fn faded(theme: &Theme, swatch: Swatch, alpha: f32) -> Color {
    let color = theme.tokens.get(swatch);
    if alpha >= 1.0 {
        return color;
    }
    color.with_alpha(color.alpha() * alpha.max(0.0))
}

/// Draws `image` at `alpha` (a picture rests opaque). The tint is written without marking the
/// component changed: the render reads it every frame, while a changed `ImageNode` has its
/// content size set again, which lays the window out.
fn fade_image(image: &mut ImageNode, alpha: f32) {
    let want = image.color.with_alpha(alpha.clamp(0.0, 1.0));
    if image.color != want {
        image.color = want;
    }
}

/// The colour `k` of the way from `a` to `b`. A clear end takes the other end's hue, so a ring
/// fading in from nothing does not pass through black.
fn mix(a: Color, b: Color, k: f32) -> Color {
    let (a, b) = (Srgba::from(a), Srgba::from(b));
    let a = if a.alpha == 0.0 { b.with_alpha(0.0) } else { a };
    let b = if b.alpha == 0.0 { a.with_alpha(0.0) } else { b };
    Color::Srgba(Srgba {
        red: a.red + (b.red - a.red) * k,
        green: a.green + (b.green - a.green) * k,
        blue: a.blue + (b.blue - a.blue) * k,
        alpha: a.alpha + (b.alpha - a.alpha) * k,
    })
}

fn animate(
    mut commands: Commands,
    time: Res<Time>,
    motion: Res<Motion>,
    covered: Res<WindowCovered>,
    theme: Res<Theme>,
    mut pace: ResMut<Pace>,
    mut animating: ResMut<Animating>,
    mut tweens: Query<(
        Entity,
        &mut Tween,
        Option<&mut UiTransform>,
        Option<(&mut Fill, &mut BackgroundColor, Has<HoverFill>)>,
        Option<(&Stroke, &mut BorderColor)>,
        Option<(&Ink, &mut TextColor)>,
        Option<&mut Outline>,
        Option<&mut Visibility>,
        Has<Veil>,
        Option<&mut ImageNode>,
    )>,
    mut despawns: Query<(Entity, &mut DespawnAfter)>,
) {
    let now = time.elapsed_secs_f64();
    let jump = motion.reduced || covered.0;
    let mut ticking = false;
    for (entity, mut tween, transform, fill, stroke, ink, outline, visibility, veiled, image) in
        &mut tweens
    {
        let start = *tween.start.get_or_insert(now);
        let t = if jump || tween.secs <= 0.0 {
            1.0
        } else {
            ((now - start) as f32 / tween.secs).min(1.0)
        };
        let done = t >= 1.0;
        let k = if done { 1.0 } else { tween.ease.at(t) };
        let look = if done {
            tween.to
        } else {
            tween.from.lerp(tween.to, k)
        };
        if tween.moves()
            && let Some(mut transform) = transform
        {
            let want = UiTransform {
                translation: Val2::px(look.shift.x, look.shift.y),
                scale: Vec2::splat(look.scale),
                rotation: transform.rotation,
            };
            transform.set_if_neq(want);
        }
        if tween.fades() {
            if let Some((mut fill, mut color, hovers)) = fill {
                color.set_if_neq(BackgroundColor(faded(&theme, fill.0, look.alpha)));
                // The fade painted `Fill` over whatever hover or press colour the widget has
                // now; `hover` repaints it on the next frame, which it does for a changed fill.
                if done && hovers {
                    fill.set_changed();
                }
            }
            if let Some((stroke, mut color)) = stroke {
                color.set_if_neq(BorderColor::all(faded(&theme, stroke.0, look.alpha)));
            }
            if let Some((ink, mut color)) = ink {
                color.set_if_neq(TextColor(faded(&theme, ink.0, look.alpha)));
            }
            if let Some(mut image) = image {
                fade_image(image.bypass_change_detection(), look.alpha);
            }
        }
        if let (Some((from, to)), Some(mut outline)) = (tween.ring, outline) {
            let want = if done { to } else { mix(from, to, k) };
            if outline.color != want {
                outline.color = want;
            }
        }
        if done {
            if veiled && let Some(mut visibility) = visibility {
                visibility.set_if_neq(Visibility::Hidden);
            }
            commands.entity(entity).try_remove::<Tween>();
        } else {
            ticking = true;
        }
    }
    for (entity, mut despawn) in &mut despawns {
        let start = *despawn.start.get_or_insert(now);
        if jump || (now - start) as f32 >= despawn.secs {
            commands.entity(entity).try_despawn();
        } else {
            ticking = true;
        }
    }
    if ticking {
        pace.ask(Wake::steady(FRAME_WAIT));
    }
    animating.set_if_neq(Animating(ticking));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Snapshot;
    use crate::testing::{self, FRAME};
    use crate::theme::LIGHT;
    use bevy::window::WindowOccluded;
    use bevy::winit::{UpdateMode, WinitSettings};

    fn app() -> App {
        let mut app = testing::app(Snapshot::default());
        app.insert_resource(WinitSettings::desktop_app());
        app.update();
        app
    }

    fn rise() -> Tween {
        Tween::new(
            140,
            Ease::OutCubic,
            Look::REST.shifted(0.0, 4.0),
            Look::REST,
        )
    }

    fn spawn(app: &mut App, bundle: impl Bundle) -> Entity {
        app.world_mut().spawn(bundle).id()
    }

    fn shift(app: &App, e: Entity) -> Val2 {
        app.world().get::<UiTransform>(e).unwrap().translation
    }

    /// Runs frames until `e` carries no tween, and says how many it took (at most 100).
    fn run_out(app: &mut App, e: Entity) -> usize {
        for n in 1..=100 {
            app.update();
            if app.world().get::<Tween>(e).is_none() {
                return n;
            }
        }
        panic!("the tween never ended");
    }

    fn focused(app: &App) -> UpdateMode {
        app.world().resource::<WinitSettings>().focused_mode
    }

    #[test]
    fn the_eases_start_at_nothing_and_end_at_the_whole_way() {
        for ease in [Ease::Linear, Ease::OutCubic, Ease::OutQuart, Ease::InCubic] {
            assert_eq!(ease.at(0.0), 0.0, "{ease:?}");
            assert_eq!(ease.at(1.0), 1.0, "{ease:?}");
            assert_eq!(ease.at(2.0), 1.0, "{ease:?} clamps");
        }
        assert_eq!(Ease::OutCubic.at(0.5), 0.875);
        assert_eq!(Ease::OutQuart.at(0.5), 0.9375);
        assert_eq!(Ease::InCubic.at(0.5), 0.125);
        assert_eq!(Ease::Linear.at(0.25), 0.25);
    }

    #[test]
    fn a_tween_reaches_its_end_exactly_and_removes_itself() {
        let mut app = app();
        let e = spawn(&mut app, (Node::default(), UiTransform::IDENTITY, rise()));
        app.update();
        assert_eq!(
            shift(&app, e),
            Val2::px(0.0, 4.0),
            "the first frame shows the start"
        );
        app.update();
        let mid = shift(&app, e).y;
        assert!(mid != Val::Px(0.0) && mid != Val::Px(4.0), "{mid:?}");
        let frames = run_out(&mut app, e);
        // 140 ms at 16 ms a frame: the ninth frame after the first is past the end.
        assert_eq!(frames, 8, "two frames seen above, then eight more");
        assert_eq!(shift(&app, e), Val2::ZERO);
        assert_eq!(app.world().get::<UiTransform>(e).unwrap().scale, Vec2::ONE);
        assert!(!app.world().resource::<Animating>().0);
    }

    #[test]
    fn a_running_tween_changes_no_node() {
        #[derive(Resource, Default)]
        struct NodeChanges(usize);
        fn count(nodes: Query<(), Changed<Node>>, mut seen: ResMut<NodeChanges>) {
            seen.0 += nodes.iter().count();
        }
        let mut app = app();
        app.init_resource::<NodeChanges>().add_systems(Last, count);
        let e = spawn(
            &mut app,
            (
                Node::default(),
                UiTransform::IDENTITY,
                BackgroundColor::default(),
                Fill(Swatch::Surface),
                Tween::new(
                    160,
                    Ease::OutCubic,
                    Look::REST.faded(0.0).scaled(0.98),
                    Look::REST,
                ),
            ),
        );
        app.update();
        app.world_mut().resource_mut::<NodeChanges>().0 = 0;
        let entities = app.world().entities().len();
        run_out(&mut app, e);
        assert_eq!(app.world().resource::<NodeChanges>().0, 0, "no relayout");
        assert_eq!(app.world().entities().len(), entities, "nothing spawned");
    }

    #[test]
    fn the_alpha_follows_the_swatch_and_ends_on_it() {
        let mut app = app();
        let e = spawn(
            &mut app,
            (
                Node::default(),
                BackgroundColor::default(),
                Fill(Swatch::Surface),
                BorderColor::default(),
                Stroke(Swatch::Line),
                TextColor::default(),
                Ink(Swatch::Fg),
                Tween::new(120, Ease::OutCubic, Look::REST.faded(0.0), Look::REST),
            ),
        );
        app.update();
        let bg = |app: &App| app.world().get::<BackgroundColor>(e).unwrap().0;
        assert_eq!(bg(&app).alpha(), 0.0);
        app.update();
        app.update();
        let a = bg(&app).alpha();
        assert!(a > 0.0 && a < 1.0, "{a}");
        assert_eq!(
            app.world().get::<TextColor>(e).unwrap().0.alpha(),
            a * LIGHT.fg.alpha()
        );
        run_out(&mut app, e);
        assert_eq!(bg(&app), LIGHT.surface);
        assert_eq!(app.world().get::<BorderColor>(e).unwrap().top, LIGHT.line);
        assert_eq!(app.world().get::<TextColor>(e).unwrap().0, LIGHT.fg);
    }

    #[test]
    fn a_widget_hovered_while_it_fades_in_ends_on_its_hover_fill() {
        use bevy::picking::hover::Hovered;
        let mut app = app();
        let e = spawn(
            &mut app,
            (
                Node::default(),
                BackgroundColor::default(),
                Fill(Swatch::Surface),
                HoverFill(Swatch::Hover),
                Hovered::default(),
                Tween::new(120, Ease::OutCubic, Look::REST.faded(0.0), Look::REST),
            ),
        );
        app.update();
        app.update();
        app.world_mut().entity_mut(e).insert(Hovered(true));
        run_out(&mut app, e);
        app.update();
        assert_eq!(
            app.world().get::<BackgroundColor>(e).unwrap().0,
            LIGHT.hover
        );
    }

    #[test]
    fn a_picture_fades_with_its_alpha_and_is_not_measured_again() {
        let mut app = app();
        let e = spawn(
            &mut app,
            (
                Node::default(),
                ImageNode::default(),
                Tween::new(120, Ease::OutCubic, Look::REST.faded(0.0), Look::REST),
            ),
        );
        app.update();
        assert_eq!(app.world().get::<ImageNode>(e).unwrap().color.alpha(), 0.0);
        app.world_mut().clear_trackers();
        app.update();
        let image = app.world_mut().entity_mut(e);
        let image = image.get_ref::<ImageNode>().unwrap();
        let a = image.color.alpha();
        assert!(a > 0.0 && a < 1.0, "{a}");
        assert!(!image.is_changed(), "a changed ImageNode is measured again");
        run_out(&mut app, e);
        assert_eq!(app.world().get::<ImageNode>(e).unwrap().color, Color::WHITE);
    }

    #[test]
    fn a_tween_that_only_moves_leaves_the_colours_alone() {
        let mut app = app();
        let e = spawn(
            &mut app,
            (
                Node::default(),
                UiTransform::IDENTITY,
                BackgroundColor::default(),
                Fill(Swatch::Surface),
                HoverFill(Swatch::Hover),
            ),
        );
        app.update();
        app.world_mut().get_mut::<BackgroundColor>(e).unwrap().0 = LIGHT.hover;
        app.world_mut().entity_mut(e).insert(rise());
        run_out(&mut app, e);
        assert_eq!(
            app.world().get::<BackgroundColor>(e).unwrap().0,
            LIGHT.hover
        );
    }

    #[test]
    fn a_ring_fades_in_from_nothing_in_its_own_hue() {
        let mut app = app();
        let e = spawn(
            &mut app,
            (
                Node::default(),
                Outline::new(px(2), px(3), Color::NONE),
                Tween::ring(100, Ease::Linear, Color::NONE, LIGHT.green),
            ),
        );
        app.update();
        app.update();
        app.update();
        let mid = Srgba::from(app.world().get::<Outline>(e).unwrap().color);
        let green = Srgba::from(LIGHT.green);
        assert!(mid.alpha > 0.0 && mid.alpha < 1.0, "{mid:?}");
        assert!(
            (mid.green - green.green).abs() < 1e-5,
            "no black on the way"
        );
        run_out(&mut app, e);
        assert_eq!(app.world().get::<Outline>(e).unwrap().color, LIGHT.green);
    }

    #[test]
    fn the_loop_wakes_each_frame_only_while_a_tween_runs() {
        let mut app = app();
        assert_eq!(focused(&app), WinitSettings::desktop_app().focused_mode);
        let e = spawn(&mut app, (Node::default(), UiTransform::IDENTITY, rise()));
        app.update();
        assert_eq!(focused(&app), UpdateMode::reactive(FRAME_WAIT));
        assert!(app.world().resource::<Animating>().0);
        run_out(&mut app, e);
        app.update();
        let desktop = WinitSettings::desktop_app();
        assert_eq!(focused(&app), desktop.focused_mode);
        assert_eq!(
            app.world().resource::<WinitSettings>().unfocused_mode,
            desktop.unfocused_mode
        );
        assert!(!app.world().resource::<Animating>().0);
    }

    #[test]
    fn reduce_motion_jumps_to_the_end_in_one_frame_and_asks_no_wake() {
        let mut app = app();
        app.insert_resource(Motion { reduced: true });
        let e = spawn(&mut app, (Node::default(), UiTransform::IDENTITY, rise()));
        let gone = spawn(&mut app, (Node::default(), DespawnAfter::new(120)));
        app.update();
        assert_eq!(shift(&app, e), Val2::ZERO);
        assert!(app.world().get::<Tween>(e).is_none());
        assert!(
            app.world().get_entity(gone).is_err(),
            "a fade-out goes at once"
        );
        assert_eq!(focused(&app), WinitSettings::desktop_app().focused_mode);
        assert_eq!(app.world().resource::<Pace>().written, None);
    }

    #[test]
    fn a_covered_window_does_not_animate_or_wake() {
        let mut app = app();
        app.world_mut().write_message(WindowOccluded {
            window: Entity::PLACEHOLDER,
            occluded: true,
        });
        app.update();
        let e = spawn(&mut app, (Node::default(), UiTransform::IDENTITY, rise()));
        app.update();
        assert_eq!(shift(&app, e), Val2::ZERO);
        assert!(app.world().get::<Tween>(e).is_none());
        assert_eq!(focused(&app), WinitSettings::desktop_app().focused_mode);
    }

    #[test]
    fn a_fade_out_despawns_after_its_duration() {
        let mut app = app();
        let e = spawn(&mut app, (Node::default(), DespawnAfter::new(120)));
        let child = app.world_mut().spawn((Node::default(), ChildOf(e))).id();
        app.update();
        let mut frames = 0;
        while app.world().get_entity(e).is_ok() {
            app.update();
            frames += 1;
            assert!(frames < 100);
        }
        // 120 ms at 16 ms a frame.
        assert_eq!(frames, (120 / FRAME.as_millis()) as usize + 1);
        assert!(app.world().get_entity(child).is_err(), "with what it holds");
        app.update();
        assert_eq!(focused(&app), WinitSettings::desktop_app().focused_mode);
    }

    #[test]
    fn a_veil_clears_and_hides_without_blocking_the_pointer_or_laying_out() {
        let mut app = app();
        let parent = spawn(&mut app, Node::default());
        let v = app
            .world_mut()
            .spawn((veil(Swatch::Bg, 140, Ease::OutCubic), ChildOf(parent)))
            .id();
        app.update();
        assert_eq!(
            app.world().get::<BackgroundColor>(v).unwrap().0,
            LIGHT.bg,
            "it starts opaque"
        );
        assert_eq!(
            *app.world().get::<bevy::picking::Pickable>(v).unwrap(),
            bevy::picking::Pickable::IGNORE
        );
        run_out(&mut app, v);
        assert_eq!(
            app.world().get::<BackgroundColor>(v).unwrap().0.alpha(),
            0.0
        );
        assert_eq!(app.world().get::<Visibility>(v), Some(&Visibility::Hidden));
        assert!(
            app.world().get_entity(v).is_ok(),
            "kept: a despawn would lay out again"
        );
    }
}
