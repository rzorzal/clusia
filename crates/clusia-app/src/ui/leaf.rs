//! The loading leaves (mockups `Loading.png`, `LoadFailed.png`): one stalk per load step with a
//! teardrop leaf on top. Done is green, a running step grows a pale leaf, a failed step has an
//! orange leaf, and a step not reached (or skipped) is a short dashed stalk. While its step runs
//! the sprout sways on its foot and the leaf pulses (and so does the step's status dot) until
//! the step ends.

use bevy::prelude::*;
use bevy::ui::UiSystems;
use bevy::window::RequestRedraw;

use crate::theme::Swatch;
use crate::ui::kit::Fill;

/// How long a growing stalk takes to reach its height.
pub const GROW_SECS: f32 = 0.6;
/// One sway and pulse loop of a running leaf.
pub const SWAY_SECS: f64 = 1.2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeafState {
    Waiting,
    Growing,
    Done,
    Failed,
    Skipped,
}

/// The state a leaf shows (for tests and screens that look it up).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sprout(pub LeafState);

/// A stalk: `height` px when grown; `progress` runs 0 → 1 over `GROW_SECS` while growing.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Stalk {
    pub height: f32,
    pub progress: f32,
}

/// The blade's rest pose: a square turned 45° so its sharp corner points up (a teardrop).
const TEARDROP: Rot2 = Rot2 {
    cos: std::f32::consts::FRAC_1_SQRT_2,
    sin: std::f32::consts::FRAC_1_SQRT_2,
};

/// The height of a leaf's box: a running sprout tilts around the bottom of it (the stalk's foot).
const BOX: f32 = 120.0;

/// Tilts a running sprout `degrees` either way around its foot. Like `Pulse` and `Blink`, its
/// phase comes from `Time`, so a rebuilt leaf carries on where the old one was.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Sway {
    pub degrees: f32,
}

/// Scales between `low` and `high` (largest at the start of the loop), turned by `rest`.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Pulse {
    pub rest: Rot2,
    pub low: f32,
    pub high: f32,
}

/// Fills with `high` while the pulse is on its larger half, with `low` otherwise.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blink {
    pub low: Swatch,
    pub high: Swatch,
}

const SPROUT_SWAY: Sway = Sway { degrees: 9.0 };

const BEAT: Pulse = Pulse {
    rest: Rot2::IDENTITY,
    low: 0.85,
    high: 1.05,
};

/// The running step's status dot: pulses and blinks with its leaf.
pub fn pulsing_dot() -> impl Bundle {
    (
        UiTransform::IDENTITY,
        BEAT,
        Blink {
            low: Swatch::GreenSoft,
            high: Swatch::Green,
        },
    )
}

/// (leaf size, stalk height, leaf color) for states that have a leaf.
fn shape(state: LeafState) -> Option<(f32, f32, Swatch)> {
    match state {
        LeafState::Done => Some((30.0, 62.0, Swatch::Green)),
        LeafState::Growing => Some((20.0, 40.0, Swatch::Heat(2))),
        LeafState::Failed => Some((24.0, 46.0, Swatch::Orange)),
        LeafState::Waiting | LeafState::Skipped => None,
    }
}

/// One leaf, 36 px wide, standing on the bottom of a 120 px box.
pub fn leaf(state: LeafState) -> impl Bundle {
    (
        Node {
            width: px(36),
            height: px(BOX),
            flex_direction: FlexDirection::Column,
            justify_content: JustifyContent::End,
            align_items: AlignItems::Center,
            flex_shrink: 0.0,
            ..default()
        },
        Sprout(state),
        UiTransform::IDENTITY,
        Children::spawn(SpawnWith(move |p: &mut ChildSpawner| match shape(state) {
            Some((size, height, color)) => {
                let growing = state == LeafState::Growing;
                if growing {
                    let sprout = p.target_entity();
                    p.world_mut().entity_mut(sprout).insert(SPROUT_SWAY);
                }
                let mut blade = p.spawn((
                    Node {
                        width: px(size),
                        height: px(size),
                        margin: UiRect::bottom(px(-4)),
                        border_radius: BorderRadius {
                            top_left: px(0),
                            top_right: percent(50),
                            bottom_right: percent(50),
                            bottom_left: percent(50),
                        },
                        flex_shrink: 0.0,
                        ..default()
                    },
                    // The sharp corner points up: a teardrop.
                    UiTransform::from_rotation(TEARDROP),
                    BackgroundColor::default(),
                    Fill(color),
                ));
                if growing {
                    blade.insert(Pulse {
                        rest: TEARDROP,
                        ..BEAT
                    });
                }
                p.spawn((
                    Node {
                        width: Val::Px(2.5),
                        height: px(if growing { 0.0 } else { height }),
                        border_radius: BorderRadius::MAX,
                        flex_shrink: 0.0,
                        ..default()
                    },
                    Stalk {
                        height,
                        progress: if growing { 0.0 } else { 1.0 },
                    },
                    BackgroundColor::default(),
                    Fill(Swatch::Muted),
                ));
            }
            None => {
                p.spawn(Node {
                    flex_direction: FlexDirection::Column,
                    row_gap: px(4),
                    ..default()
                })
                .with_children(|dashes| {
                    for _ in 0..3 {
                        dashes.spawn((
                            Node {
                                width: Val::Px(2.5),
                                height: px(3),
                                ..default()
                            },
                            BackgroundColor::default(),
                            Fill(Swatch::Line),
                        ));
                    }
                });
            }
        })),
    )
}

pub struct LeafPlugin;

impl Plugin for LeafPlugin {
    fn build(&self, app: &mut App) {
        // After Update, so a leaf a screen spawns is seen in the frame that spawns it.
        app.add_systems(PostUpdate, (grow, sway).before(UiSystems::Layout));
    }
}

/// Grows stalks with an ease-out; asks for redraws only while one is still growing. A new
/// stalk does not move in its first frame (whose delta may hold seconds of idle time under
/// `desktop_app`): it asks for the next frame and starts from there.
fn grow(
    time: Res<Time>,
    mut stalks: Query<(&mut Stalk, &mut Node)>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    let mut moving = false;
    for (mut stalk, mut node) in &mut stalks {
        if stalk.progress >= 1.0 {
            continue;
        }
        if stalk.is_added() {
            moving = true;
            continue;
        }
        stalk.progress = (stalk.progress + time.delta_secs() / GROW_SECS).min(1.0);
        let eased = 1.0 - (1.0 - stalk.progress).powi(3);
        node.height = px(stalk.height * eased);
        moving |= stalk.progress < 1.0;
    }
    if moving {
        redraw.write(RequestRedraw);
    }
}

/// Moves every running sprout, leaf and dot along one shared loop; asks for redraws only while
/// one is alive, so the reactive loop idles once every step has ended.
fn sway(
    time: Res<Time>,
    mut sprouts: Query<(&Sway, &mut UiTransform), Without<Pulse>>,
    mut pulses: Query<(&Pulse, &mut UiTransform), Without<Sway>>,
    mut blinks: Query<(&Blink, &mut Fill)>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if sprouts.is_empty() && pulses.is_empty() && blinks.is_empty() {
        return;
    }
    let phase = (time.elapsed_secs_f64() % SWAY_SECS / SWAY_SECS) as f32 * std::f32::consts::TAU;
    // 1 at the start of the loop, -1 halfway.
    let beat = phase.cos();
    for (sway, mut transform) in &mut sprouts {
        let tilt = Rot2::degrees(sway.degrees * phase.sin());
        // `UiTransform` turns around the node's centre; shift it back so the foot stays put.
        let foot = Vec2::new(0.0, BOX / 2.0);
        let shift = foot - tilt * foot;
        transform.rotation = tilt;
        transform.translation = Val2::px(shift.x, shift.y);
    }
    for (pulse, mut transform) in &mut pulses {
        let scale = pulse.low + (pulse.high - pulse.low) * (beat + 1.0) / 2.0;
        transform.scale = Vec2::splat(scale);
        transform.rotation = pulse.rest;
    }
    for (blink, mut fill) in &mut blinks {
        fill.set_if_neq(Fill(if beat >= 0.0 { blink.high } else { blink.low }));
    }
    redraw.write(RequestRedraw);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Snapshot;
    use crate::testing;
    use crate::theme::LIGHT;
    use bevy::time::TimeUpdateStrategy;
    use std::time::Duration;

    fn redraws(app: &mut App) -> usize {
        app.world_mut()
            .resource_mut::<Messages<RequestRedraw>>()
            .drain()
            .count()
    }

    fn parts(app: &mut App, leaf: Entity) -> Vec<Entity> {
        app.world().get::<Children>(leaf).unwrap().to_vec()
    }

    #[test]
    fn states_draw_their_leaf() {
        let mut app = testing::app(Snapshot::default());
        let done = app.world_mut().spawn(leaf(LeafState::Done)).id();
        let failed = app.world_mut().spawn(leaf(LeafState::Failed)).id();
        let skipped = app.world_mut().spawn(leaf(LeafState::Skipped)).id();
        app.update();
        let [blade, stalk] = parts(&mut app, done)[..] else {
            panic!("leaf and stalk")
        };
        assert_eq!(
            app.world().get::<BackgroundColor>(blade).unwrap().0,
            LIGHT.green
        );
        assert_eq!(app.world().get::<Node>(stalk).unwrap().height, px(62));
        let blade = parts(&mut app, failed)[0];
        assert_eq!(
            app.world().get::<BackgroundColor>(blade).unwrap().0,
            LIGHT.orange
        );
        let dashes = parts(&mut app, skipped);
        assert_eq!(dashes.len(), 1);
        assert_eq!(app.world().get::<Children>(dashes[0]).unwrap().len(), 3);
        assert_eq!(
            app.world().get::<Sprout>(skipped),
            Some(&Sprout(LeafState::Skipped))
        );
    }

    #[test]
    fn a_leaf_spawned_by_a_screen_starts_from_zero_after_a_long_idle() {
        let mut app = testing::app(Snapshot::default());
        // Under `desktop_app` the frame that spawns a leaf can follow seconds of idle time:
        // `Time` hands it up to 250 ms.
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            250,
        )));
        app.add_systems(Update, |mut commands: Commands, mut done: Local<bool>| {
            if !*done {
                commands.spawn(leaf(LeafState::Growing));
                *done = true;
            }
        });
        redraws(&mut app);
        app.update();
        let mut q = app.world_mut().query::<(&Stalk, &Node)>();
        let (stalk, node) = q.single(app.world()).expect("one stalk");
        assert_eq!((stalk.progress, node.height), (0.0, px(0)), "no jump");
        assert!(redraws(&mut app) > 0, "the next frame is asked for at once");
    }

    #[test]
    fn growing_animates_then_stops_redrawing() {
        let mut app = testing::app(Snapshot::default());
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            100,
        )));
        app.update();
        redraws(&mut app);
        let growing = app.world_mut().spawn(leaf(LeafState::Growing)).id();
        app.update();
        let stalk = parts(&mut app, growing)[1];
        let height = |app: &App| match app.world().get::<Node>(stalk).unwrap().height {
            Val::Px(h) => h,
            other => panic!("{other:?}"),
        };
        assert_eq!(height(&app), 0.0, "the first frame only asks for the next");
        assert!(redraws(&mut app) > 0);
        app.update();
        let first = height(&app);
        assert!(first > 0.0 && first < 40.0, "{first}");
        assert!(
            redraws(&mut app) > 0,
            "keeps the loop ticking while it grows"
        );
        for _ in 0..10 {
            app.update();
        }
        assert_eq!(height(&app), 40.0);
        redraws(&mut app);
        app.update();
        assert_eq!(height(&app), 40.0, "the stalk stays grown");
        assert!(redraws(&mut app) > 0, "the leaf still sways while running");
        let blade = parts(&mut app, growing)[0];
        assert_eq!(
            app.world().get::<BackgroundColor>(blade).unwrap().0,
            LIGHT.heat[2]
        );
    }

    fn transform(app: &App, e: Entity) -> UiTransform {
        *app.world().get::<UiTransform>(e).unwrap()
    }

    /// Where a sprout's transform puts the bottom centre of its box (its foot), relative to the
    /// box centre, through the same affine Bevy's layout builds.
    fn foot(app: &App, e: Entity) -> Vec2 {
        let size = Vec2::new(36.0, BOX);
        transform(app, e)
            .compute_affine(1.0, size, size)
            .transform_point2(Vec2::new(0.0, BOX / 2.0))
    }

    #[test]
    fn a_running_leaf_sways_until_it_goes() {
        let mut app = testing::app(Snapshot::default());
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            100,
        )));
        app.update();
        let growing = app.world_mut().spawn(leaf(LeafState::Growing)).id();
        app.update();
        let blade = parts(&mut app, growing)[0];
        redraws(&mut app);
        let ground = Vec2::new(0.0, BOX / 2.0);
        let mut seen = vec![(transform(&app, growing), transform(&app, blade))];
        let (mut widest, mut smallest, mut largest) = (0.0f32, f32::MAX, 0.0f32);
        // Well past the growth: the sway goes on.
        for _ in 0..20 {
            app.update();
            assert!(redraws(&mut app) > 0, "a redraw every frame while running");
            let now = (transform(&app, growing), transform(&app, blade));
            assert_ne!(Some(&now), seen.last(), "moves every frame");
            let (sprout, leaf) = now;
            let tilt = sprout.rotation.as_degrees();
            assert!(tilt.abs() <= 10.0, "{tilt}");
            widest = widest.max(tilt.abs());
            assert!(
                (0.85 - 1e-4..=1.05 + 1e-4).contains(&leaf.scale.x),
                "{leaf:?}"
            );
            smallest = smallest.min(leaf.scale.x);
            largest = largest.max(leaf.scale.x);
            assert_eq!(leaf.rotation, TEARDROP, "the leaf keeps its point up");
            assert!(
                foot(&app, growing).distance(ground) < 0.01,
                "the stalk's foot stays on the ground"
            );
            seen.push(now);
        }
        assert!(widest >= 8.0, "a real sway: {widest}");
        assert!(
            largest - smallest >= 0.15,
            "a real pulse: {smallest}..{largest}"
        );
        app.world_mut().entity_mut(growing).despawn();
        app.update();
        redraws(&mut app);
        app.update();
        assert_eq!(redraws(&mut app), 0, "idle once no leaf is alive");
    }

    #[test]
    fn still_leaves_never_sway() {
        let mut app = testing::app(Snapshot::default());
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            100,
        )));
        app.update();
        let leaves: Vec<Entity> = [
            LeafState::Waiting,
            LeafState::Skipped,
            LeafState::Done,
            LeafState::Failed,
        ]
        .into_iter()
        .map(|s| app.world_mut().spawn(leaf(s)).id())
        .collect();
        app.update();
        redraws(&mut app);
        let before = transforms(&app, &leaves);
        for _ in 0..5 {
            app.update();
            assert_eq!(redraws(&mut app), 0);
        }
        assert_eq!(before, transforms(&app, &leaves));
        assert!(
            leaves
                .iter()
                .all(|&l| transform(&app, l) == UiTransform::IDENTITY)
        );
        let mut q = app
            .world_mut()
            .query_filtered::<Entity, Or<(With<Sway>, With<Pulse>, With<Blink>)>>();
        assert_eq!(q.iter(app.world()).count(), 0);
    }

    fn transforms(app: &App, leaves: &[Entity]) -> Vec<Option<UiTransform>> {
        leaves
            .iter()
            .flat_map(|&l| app.world().get::<Children>(l).unwrap().iter())
            .map(|e| app.world().get::<UiTransform>(e).copied())
            .collect()
    }

    #[test]
    fn a_rebuilt_leaf_keeps_the_phase() {
        let mut app = testing::app(Snapshot::default());
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            70,
        )));
        app.update();
        let old = app.world_mut().spawn(leaf(LeafState::Growing)).id();
        for _ in 0..7 {
            app.update();
        }
        // A screen rebuild: the same leaf spawned again mid-loop.
        let new = app.world_mut().spawn(leaf(LeafState::Growing)).id();
        app.update();
        let (a, b) = (parts(&mut app, old)[0], parts(&mut app, new)[0]);
        assert_eq!(transform(&app, old), transform(&app, new), "no restart");
        assert_eq!(transform(&app, a), transform(&app, b), "no restart");
        assert_ne!(transform(&app, new), UiTransform::IDENTITY);
    }

    #[test]
    fn a_pulsing_dot_blinks_with_the_leaf() {
        let mut app = testing::app(Snapshot::default());
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            130,
        )));
        app.update();
        let sprout = app.world_mut().spawn(leaf(LeafState::Growing)).id();
        let dot = app
            .world_mut()
            .spawn((Node::default(), Fill(Swatch::GreenSoft), pulsing_dot()))
            .id();
        app.update();
        let blade = parts(&mut app, sprout)[0];
        let mut fills = Vec::new();
        // Over a loop, off the quarter points where the fill flips.
        for _ in 0..10 {
            app.update();
            let scale = transform(&app, dot).scale;
            assert_eq!(scale, transform(&app, blade).scale, "one shared phase");
            let fill = app.world().get::<Fill>(dot).unwrap().0;
            assert_eq!(
                fill,
                if scale.x >= 0.95 {
                    Swatch::Green
                } else {
                    Swatch::GreenSoft
                },
                "{scale:?}"
            );
            fills.push(fill);
        }
        assert!(fills.contains(&Swatch::Green) && fills.contains(&Swatch::GreenSoft));
    }
}
