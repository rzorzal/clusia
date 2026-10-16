//! The marks that say the agent is working: a ring with a dot orbiting it (`spinner`) and a dot
//! that beats (`beat`). Both are driven by a frame index taken from `Time`, so a rebuilt mark
//! carries on where the old one was and nothing is allocated per frame: the ring changes one
//! `UiTransform`, the dot one `Fill`, and only when the index moves. While one is on screen the
//! event loop wakes once per step (`Pace`) instead of every frame. With `Motion::reduced` they
//! stay still and need no wake: the ring shows its dot at the top, the beat its lit colour.

use bevy::prelude::*;
use bevy::ui::UiSystems;
use bevy::winit::{UpdateMode, WinitSettings};
use std::time::Duration;

use crate::theme::Swatch;
use crate::ui::kit::{Fill, Stroke};

/// Positions of the orbiting dot in one turn of the ring.
pub const STEPS: u32 = 8;
/// One turn of the ring.
pub const TURN_SECS: f64 = 0.8;
/// One on-off cycle of the beating dot.
pub const BEAT_SECS: f64 = 1.2;

/// Whether the marks hold still: macOS's Reduce Motion is on (`platform_window` keeps this up to
/// date), or a test wants a frame that does not depend on the clock.
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

/// The wait the marks last wrote into the event loop's settings, if they hold one: a wait is
/// theirs to change or restore only while the settings still equal it.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Pace(pub Option<Duration>);

/// How long a mark that moves can sleep: until its next step.
fn wait_of(rings: bool) -> Duration {
    Duration::from_secs_f64(if rings {
        TURN_SECS / f64::from(STEPS)
    } else {
        BEAT_SECS / 2.0
    })
}

pub struct SpinnerPlugin;

impl Plugin for SpinnerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<Motion>()
            .init_resource::<Pace>()
            .add_systems(PostUpdate, animate.before(UiSystems::Layout));
    }
}

/// Moves every ring and beat to the frame of the clock, and asks the event loop to wake at the
/// next step while one moves. The settings change only at the edges (a mark appears, the last
/// one goes), never per frame, and a faster pace someone else asked for (a GIF) is left alone.
/// Screenshot runs keep their continuous mode.
fn animate(
    time: Res<Time>,
    motion: Res<Motion>,
    mut pace: ResMut<Pace>,
    mut rings: Query<(&mut Spinner, &mut UiTransform)>,
    mut beats: Query<(&Beat, &mut Fill)>,
    settings: Option<ResMut<WinitSettings>>,
) {
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
    let Some(mut settings) = settings else { return };
    if matches!(settings.focused_mode, UpdateMode::Continuous) {
        return;
    }
    let want = (!motion.reduced && (!rings.is_empty() || !beats.is_empty()))
        .then(|| wait_of(!rings.is_empty()));
    let ours = pace
        .0
        .is_some_and(|written| settings.focused_mode == UpdateMode::reactive(written));
    match want {
        Some(wait) => {
            let faster_than_us = !ours
                && matches!(
                    settings.focused_mode,
                    UpdateMode::Reactive { wait: w, .. } if w < wait
                );
            if faster_than_us {
                // Someone else (a GIF) wakes sooner: leave it, and hold nothing to restore.
                pace.0 = None;
            } else {
                if settings.focused_mode != UpdateMode::reactive(wait) {
                    settings.focused_mode = UpdateMode::reactive(wait);
                    settings.unfocused_mode = UpdateMode::reactive(wait);
                }
                if pace.0 != Some(wait) {
                    pace.0 = Some(wait);
                }
            }
        }
        None => {
            if ours {
                *settings = WinitSettings::desktop_app();
            }
            if pace.0.is_some() {
                pace.0 = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Snapshot;
    use crate::testing;
    use bevy::time::TimeUpdateStrategy;

    fn app_with(marks: impl Bundle) -> (App, Entity) {
        let mut app = testing::app(Snapshot::default());
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(
            100,
        )));
        let mark = app.world_mut().spawn(marks).id();
        app.update();
        (app, mark)
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
    fn the_ring_shows_the_step_of_the_clock_in_its_transform() {
        let (mut app, ring) = app_with(spinner());
        let mut seen = Vec::new();
        for _ in 0..16 {
            app.update();
            let step = app.world().get::<Spinner>(ring).unwrap().step;
            let clock = app.world().resource::<Time>().elapsed_secs_f64();
            assert_eq!(step, step_at(clock));
            assert_eq!(
                app.world().get::<UiTransform>(ring).unwrap().rotation,
                rotation_of(step)
            );
            seen.push(step);
        }
        seen.dedup();
        assert!(seen.len() >= STEPS as usize, "it went round: {seen:?}");
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
    fn reduced_motion_holds_the_marks_still() {
        let (mut app, ring) = app_with(spinner());
        let dot = app.world_mut().spawn(beat()).id();
        app.insert_resource(Motion { reduced: true });
        for _ in 0..10 {
            app.update();
            assert_eq!(app.world().get::<Spinner>(ring).unwrap().step, 0);
            assert_eq!(
                app.world().get::<UiTransform>(ring).unwrap().rotation,
                Rot2::IDENTITY
            );
            assert_eq!(app.world().get::<Fill>(dot).unwrap().0, Swatch::Green);
        }
    }

    fn mode(app: &App) -> UpdateMode {
        app.world().resource::<WinitSettings>().focused_mode
    }

    fn desktop(app: &mut App) {
        app.insert_resource(WinitSettings::desktop_app());
    }

    #[test]
    fn a_ring_wakes_the_loop_once_per_step_and_the_default_returns_with_it() {
        let (mut app, ring) = app_with(spinner());
        desktop(&mut app);
        app.update();
        assert_eq!(mode(&app), UpdateMode::reactive(Duration::from_millis(100)));
        assert_eq!(
            app.world().resource::<WinitSettings>().unfocused_mode,
            UpdateMode::reactive(Duration::from_millis(100))
        );
        app.world_mut().despawn(ring);
        app.update();
        assert_eq!(mode(&app), WinitSettings::desktop_app().focused_mode);
        assert_eq!(app.world().resource::<Pace>().0, None);
    }

    #[test]
    fn a_beat_alone_wakes_the_loop_at_each_half_cycle() {
        let (mut app, _) = app_with(beat());
        desktop(&mut app);
        app.update();
        assert_eq!(mode(&app), UpdateMode::reactive(Duration::from_millis(600)));
    }

    #[test]
    fn reduced_motion_asks_for_no_wake() {
        let (mut app, _) = app_with(spinner());
        app.insert_resource(Motion { reduced: true });
        desktop(&mut app);
        app.update();
        assert_eq!(mode(&app), WinitSettings::desktop_app().focused_mode);
    }

    #[test]
    fn a_faster_wake_someone_else_asked_for_is_kept() {
        let (mut app, ring) = app_with(spinner());
        app.insert_resource(WinitSettings {
            focused_mode: UpdateMode::reactive(Duration::from_millis(30)),
            unfocused_mode: UpdateMode::reactive(Duration::from_millis(30)),
        });
        app.update();
        assert_eq!(mode(&app), UpdateMode::reactive(Duration::from_millis(30)));
        app.world_mut().despawn(ring);
        app.update();
        assert_eq!(
            mode(&app),
            UpdateMode::reactive(Duration::from_millis(30)),
            "not ours to reset"
        );
    }

    #[test]
    fn a_screenshot_run_stays_continuous() {
        let (mut app, _) = app_with(spinner());
        app.insert_resource(WinitSettings::continuous());
        app.update();
        assert_eq!(mode(&app), UpdateMode::Continuous);
    }

    #[test]
    fn a_ring_then_a_beat_then_nothing_restores_the_default() {
        let (mut app, ring) = app_with(spinner());
        let dot = app.world_mut().spawn(beat()).id();
        desktop(&mut app);
        app.update();
        assert_eq!(mode(&app), UpdateMode::reactive(Duration::from_millis(100)));
        app.world_mut().despawn(ring);
        app.update();
        assert_eq!(
            mode(&app),
            UpdateMode::reactive(Duration::from_millis(600)),
            "its own 100 ms is not someone else's"
        );
        app.world_mut().despawn(dot);
        app.update();
        assert_eq!(mode(&app), WinitSettings::desktop_app().focused_mode);
        assert_eq!(
            app.world().resource::<WinitSettings>().unfocused_mode,
            WinitSettings::desktop_app().unfocused_mode
        );
        assert_eq!(app.world().resource::<Pace>().0, None);
    }

    #[test]
    fn a_beat_then_a_ring_then_nothing_restores_the_default() {
        let (mut app, dot) = app_with(beat());
        desktop(&mut app);
        app.update();
        let ring = app.world_mut().spawn(spinner()).id();
        app.update();
        assert_eq!(mode(&app), UpdateMode::reactive(Duration::from_millis(100)));
        app.world_mut().despawn(ring);
        app.world_mut().despawn(dot);
        app.update();
        assert_eq!(mode(&app), WinitSettings::desktop_app().focused_mode);
    }

    #[test]
    fn a_default_restored_by_someone_else_is_taken_back_on_the_next_frame() {
        let (mut app, _ring) = app_with(spinner());
        desktop(&mut app);
        app.update();
        assert_eq!(mode(&app), UpdateMode::reactive(Duration::from_millis(100)));
        // The GIF clock goes back to the desktop settings while the ring is still shown.
        desktop(&mut app);
        app.update();
        assert_eq!(mode(&app), UpdateMode::reactive(Duration::from_millis(100)));
    }

    #[test]
    fn a_faster_wake_set_after_ours_is_kept_and_not_reset() {
        let (mut app, ring) = app_with(spinner());
        desktop(&mut app);
        app.update();
        app.insert_resource(WinitSettings {
            focused_mode: UpdateMode::reactive(Duration::from_millis(30)),
            unfocused_mode: UpdateMode::reactive(Duration::from_millis(30)),
        });
        app.update();
        assert_eq!(mode(&app), UpdateMode::reactive(Duration::from_millis(30)));
        app.world_mut().despawn(ring);
        app.update();
        assert_eq!(mode(&app), UpdateMode::reactive(Duration::from_millis(30)));
    }
}
