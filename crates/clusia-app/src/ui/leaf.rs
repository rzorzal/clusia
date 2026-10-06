//! The loading leaves (mockups `Loading.png`, `LoadFailed.png`): one stalk per load step with a
//! teardrop leaf on top. Done is green, a running step grows a pale leaf, a failed step has an
//! orange leaf, and a step not reached (or skipped) is a short dashed stalk. While its step runs
//! the leaf sways and pulses (and so does the step's status dot) until the step ends.

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

/// Sways and pulses while its step runs (scale 0.92 ↔ 1, `degrees` either way around `rest`).
/// The phase comes from `Time`, so a rebuilt leaf carries on where the old one was.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct Sway {
    pub rest: Rot2,
    pub degrees: f32,
}

/// The running leaf's blade: a teardrop at 45° that tilts a few degrees.
const BLADE: Sway = Sway {
    rest: Rot2 {
        cos: std::f32::consts::FRAC_1_SQRT_2,
        sin: std::f32::consts::FRAC_1_SQRT_2,
    },
    degrees: 4.0,
};

/// A status dot that pulses with the running leaf.
pub const PULSE: Sway = Sway {
    rest: Rot2::IDENTITY,
    degrees: 0.0,
};

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
            height: px(120),
            flex_direction: FlexDirection::Column,
            justify_content: JustifyContent::End,
            align_items: AlignItems::Center,
            flex_shrink: 0.0,
            ..default()
        },
        Sprout(state),
        Children::spawn(SpawnWith(move |p: &mut ChildSpawner| match shape(state) {
            Some((size, height, color)) => {
                let growing = state == LeafState::Growing;
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
                    UiTransform::from_rotation(Rot2::degrees(45.0)),
                    BackgroundColor::default(),
                    Fill(color),
                ));
                if growing {
                    blade.insert(BLADE);
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

/// Sways every running leaf (and its dot) along one shared loop; asks for redraws only while
/// one is alive, so the reactive loop idles once every step has ended.
fn sway(
    time: Res<Time>,
    mut swaying: Query<(&Sway, &mut UiTransform)>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if swaying.is_empty() {
        return;
    }
    let phase = (time.elapsed_secs_f64() % SWAY_SECS / SWAY_SECS) as f32 * std::f32::consts::TAU;
    let scale = 0.96 + 0.04 * phase.cos();
    for (sway, mut transform) in &mut swaying {
        transform.scale = Vec2::splat(scale);
        transform.rotation = sway.rest * Rot2::degrees(sway.degrees * phase.sin());
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
        let mut seen = vec![transform(&app, blade)];
        // Well past the growth: the sway goes on.
        for _ in 0..20 {
            app.update();
            assert!(redraws(&mut app) > 0, "a redraw every frame while running");
            let t = transform(&app, blade);
            assert_ne!(Some(&t), seen.last(), "moves every frame");
            assert!((0.92 - 1e-4..=1.0 + 1e-4).contains(&t.scale.x), "{t:?}");
            let tilt = (t.rotation.as_degrees() - 45.0).abs();
            assert!(tilt <= 6.0, "{tilt}");
            seen.push(t);
        }
        app.world_mut().entity_mut(growing).despawn();
        app.update();
        redraws(&mut app);
        app.update();
        assert_eq!(redraws(&mut app), 0, "idle once no leaf is alive");
    }

    fn transforms(app: &App, leaves: &[Entity]) -> Vec<Option<UiTransform>> {
        leaves
            .iter()
            .flat_map(|&l| app.world().get::<Children>(l).unwrap().iter())
            .map(|e| app.world().get::<UiTransform>(e).copied())
            .collect()
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
        let after = transforms(&app, &leaves);
        assert_eq!(before, after);
        assert_eq!(
            app.world_mut().query::<&Sway>().iter(app.world()).count(),
            0
        );
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
        assert_eq!(transform(&app, a), transform(&app, b), "no restart");
        assert_ne!(
            transform(&app, b),
            UiTransform::from_rotation(Rot2::degrees(45.0))
        );
    }
}
