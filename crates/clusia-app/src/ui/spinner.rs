//! The marks that say the agent is working: a ring with a dot orbiting it (`spinner`) and a dot
//! that beats (`beat`). Both are driven by a frame index taken from `Time`, so a rebuilt mark
//! carries on where the old one was and nothing is allocated per frame: the ring changes one
//! `UiTransform`, the dot one `Fill`, and only when the index moves. With `Motion::reduced` they
//! stay still: the ring shows its dot at the top, the beat shows its lit colour.

use bevy::prelude::*;
use bevy::ui::UiSystems;
use bevy::window::RequestRedraw;

use crate::theme::Swatch;
use crate::ui::kit::{Fill, Stroke};

/// Positions of the orbiting dot in one turn of the ring.
pub const STEPS: u32 = 8;
/// One turn of the ring.
pub const TURN_SECS: f64 = 0.8;
/// One on-off cycle of the beating dot.
pub const BEAT_SECS: f64 = 1.2;

/// Whether the marks hold still (the user asked the system for less motion, or a test wants a
/// frame that does not depend on the clock).
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Motion {
    pub reduced: bool,
}

/// The ring: its `UiTransform` turns by `step` of `STEPS`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spinner {
    pub step: u32,
}

/// A dot that alternates between `dim` and `lit` (lit during the first half of the cycle).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Beat {
    pub dim: Swatch,
    pub lit: Swatch,
}

/// The step of the ring at `secs` of the clock.
pub fn step_at(secs: f64) -> u32 {
    ((secs % TURN_SECS) / TURN_SECS * f64::from(STEPS)) as u32 % STEPS
}

/// Whether the beat is on its lit half at `secs`.
pub fn lit_at(secs: f64) -> bool {
    secs % BEAT_SECS < BEAT_SECS / 2.0
}

fn rotation_of(step: u32) -> Rot2 {
    Rot2::degrees(360.0 / STEPS as f32 * step as f32)
}

/// A 12 px ring with a 3 px dot on its rim that turns around it.
pub fn spinner() -> impl Bundle {
    (
        Node {
            width: px(12),
            height: px(12),
            border: px(2).all(),
            border_radius: BorderRadius::MAX,
            flex_shrink: 0.0,
            justify_content: JustifyContent::Center,
            ..default()
        },
        BorderColor::default(),
        Stroke(Swatch::Line),
        UiTransform::IDENTITY,
        Spinner { step: 0 },
        children![(
            Node {
                width: px(4),
                height: px(4),
                // Centred on the ring's top edge: the border is 2 px, the dot 4.
                margin: UiRect::top(px(-4)),
                border_radius: BorderRadius::MAX,
                flex_shrink: 0.0,
                ..default()
            },
            BackgroundColor::default(),
            Fill(Swatch::Green),
        )],
    )
}

/// A 7 px dot that beats between a soft and the full green.
pub fn beat() -> impl Bundle {
    let beat = Beat {
        dim: Swatch::GreenSoft,
        lit: Swatch::Green,
    };
    (
        Node {
            width: px(7),
            height: px(7),
            border_radius: BorderRadius::MAX,
            flex_shrink: 0.0,
            ..default()
        },
        BackgroundColor::default(),
        Fill(beat.lit),
        beat,
    )
}

pub struct SpinnerPlugin;

impl Plugin for SpinnerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Motion>()
            .add_systems(PostUpdate, animate.before(UiSystems::Layout));
    }
}

/// Moves every ring and beat to the frame of the clock; asks for the next frame only while one
/// moves, so the reactive loop idles once none is left.
fn animate(
    time: Res<Time>,
    motion: Res<Motion>,
    mut rings: Query<(&mut Spinner, &mut UiTransform)>,
    mut beats: Query<(&Beat, &mut Fill)>,
    mut redraw: MessageWriter<RequestRedraw>,
) {
    if rings.is_empty() && beats.is_empty() {
        return;
    }
    let secs = time.elapsed_secs_f64();
    let (step, lit) = if motion.reduced {
        (0, true)
    } else {
        (step_at(secs), lit_at(secs))
    };
    for (mut ring, mut transform) in &mut rings {
        if ring.step != step {
            ring.step = step;
        }
        let turned = rotation_of(step);
        if transform.rotation != turned {
            transform.rotation = turned;
        }
    }
    for (beat, mut fill) in &mut beats {
        fill.set_if_neq(Fill(if lit { beat.lit } else { beat.dim }));
    }
    if !motion.reduced {
        redraw.write(RequestRedraw);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Snapshot;
    use crate::testing;
    use bevy::time::TimeUpdateStrategy;
    use std::time::Duration;

    fn app_with(marks: impl Bundle) -> (App, Entity) {
        let mut app = testing::app(Snapshot::default());
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            100,
        )));
        let mark = app.world_mut().spawn(marks).id();
        app.update();
        (app, mark)
    }

    fn redraws(app: &mut App) -> usize {
        app.world_mut()
            .resource_mut::<Messages<RequestRedraw>>()
            .drain()
            .count()
    }

    #[test]
    fn the_step_follows_the_clock_and_wraps_each_turn() {
        assert_eq!(step_at(0.0), 0);
        assert_eq!(step_at(TURN_SECS / 2.0), STEPS / 2);
        assert_eq!(step_at(TURN_SECS - 0.001), STEPS - 1);
        assert_eq!(step_at(TURN_SECS * 3.0 + 0.0001), 0);
    }

    #[test]
    fn the_beat_is_lit_for_half_its_cycle() {
        assert!(lit_at(0.0));
        assert!(!lit_at(BEAT_SECS / 2.0 + 0.01));
        assert!(lit_at(BEAT_SECS + 0.01));
    }

    #[test]
    fn the_ring_turns_one_step_at_a_time() {
        let (mut app, ring) = app_with(spinner());
        let mut seen = Vec::new();
        for _ in 0..STEPS {
            app.update();
            seen.push(app.world().get::<Spinner>(ring).unwrap().step);
            let want = rotation_of(app.world().get::<Spinner>(ring).unwrap().step);
            assert_eq!(app.world().get::<UiTransform>(ring).unwrap().rotation, want);
        }
        seen.sort_unstable();
        seen.dedup();
        assert!(seen.len() > 1, "it moved: {seen:?}");
    }

    #[test]
    fn a_ring_that_has_not_moved_is_not_touched() {
        let (mut app, ring) = app_with(spinner());
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(1)));
        app.update();
        app.world_mut().clear_trackers();
        app.update();
        let transform = app
            .world_mut()
            .query::<Ref<UiTransform>>()
            .get(app.world(), ring)
            .unwrap()
            .is_changed();
        assert!(!transform, "same step, same transform");
    }

    #[test]
    fn the_beat_alternates_its_fill() {
        let (mut app, dot) = app_with(beat());
        let mut fills = Vec::new();
        for _ in 0..14 {
            app.update();
            fills.push(app.world().get::<Fill>(dot).unwrap().0);
        }
        assert!(fills.contains(&Swatch::Green) && fills.contains(&Swatch::GreenSoft));
    }

    #[test]
    fn reduced_motion_holds_the_marks_still_and_asks_for_no_frames() {
        let (mut app, ring) = app_with(spinner());
        let dot = app.world_mut().spawn(beat()).id();
        app.insert_resource(Motion { reduced: true });
        redraws(&mut app);
        for _ in 0..10 {
            app.update();
            assert_eq!(app.world().get::<Spinner>(ring).unwrap().step, 0);
            assert_eq!(
                app.world().get::<UiTransform>(ring).unwrap().rotation,
                Rot2::IDENTITY
            );
            assert_eq!(app.world().get::<Fill>(dot).unwrap().0, Swatch::Green);
        }
        assert_eq!(redraws(&mut app), 0, "a still mark needs no frames");
    }

    #[test]
    fn a_moving_mark_asks_for_the_next_frame_and_none_means_idle() {
        let (mut app, ring) = app_with(spinner());
        redraws(&mut app);
        app.update();
        assert!(redraws(&mut app) > 0);
        app.world_mut().despawn(ring);
        app.update();
        redraws(&mut app);
        app.update();
        assert_eq!(redraws(&mut app), 0);
    }
}
