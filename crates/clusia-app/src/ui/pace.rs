//! How often the event loop wakes. Marks that move on their own (the spinner, the sway of a
//! loading leaf, a toast waiting to expire) do not redraw every frame: each frame they `ask`
//! for the wake they need, and `settle` writes the shortest of those asks into the
//! `WinitSettings`. The settings change only at the edges (an ask appears, the last one goes),
//! never per frame, because a change makes Bevy redraw at once.
//!
//! While the window is minimized or fully covered nothing is asked for: the settings go back
//! to the desktop ones and the loop sleeps until the window is shown again.

use bevy::prelude::*;
use bevy::window::WindowOccluded;
use bevy::winit::{UpdateMode, WinitSettings};
use std::time::Duration;

/// How long a mark that only decorates may sleep when the window has no focus.
pub const UNFOCUSED_WAIT: Duration = Duration::from_millis(500);

/// Whether the window is minimized or fully covered. Winit reports it as an occlusion, and a
/// minimized window can keep its size, so the layout alone cannot tell.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WindowCovered(pub bool);

fn track_occlusion(mut events: MessageReader<WindowOccluded>, mut covered: ResMut<WindowCovered>) {
    // The app has a single window, so the last report is the state.
    if let Some(last) = events.read().last()
        && covered.0 != last.occluded
    {
        covered.0 = last.occluded;
    }
}

/// The waits a mark asks for, with and without focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wake {
    pub focused: Duration,
    pub unfocused: Duration,
}

impl Wake {
    /// A mark that moves (virtual time advances 250 ms at most per wake, so a longer wait runs
    /// it slower than the clock): it steps at `wait` with focus and at `UNFOCUSED_WAIT` (or `wait`,
    /// when that is longer) without it.
    pub fn paced(wait: Duration) -> Self {
        Self {
            focused: wait,
            unfocused: wait.max(UNFOCUSED_WAIT),
        }
    }

    /// A mark that must be looked at on time whatever the focus (a toast's deadline).
    pub fn steady(wait: Duration) -> Self {
        Self {
            focused: wait,
            unfocused: wait,
        }
    }

    fn sooner(self, other: Self) -> Self {
        Self {
            focused: self.focused.min(other.focused),
            unfocused: self.unfocused.min(other.unfocused),
        }
    }
}

/// The asks of this frame, and the focused wait the marks last wrote into the event loop's
/// settings, if they hold one: a wait is theirs to change or restore only while the settings
/// still equal it.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Pace {
    pub written: Option<Duration>,
    asked: Option<Wake>,
}

impl Pace {
    /// Asks for the loop to wake within `wake`; the shortest ask of the frame wins.
    pub fn ask(&mut self, wake: Wake) {
        self.asked = Some(self.asked.map_or(wake, |had| had.sooner(wake)));
    }
}

/// The systems that call `Pace::ask` belong to this set; `settle` runs after it in `PostUpdate`.
/// An ask must come from `Update`, or from `PostUpdate` inside this set: one made later in the
/// frame, or in `PostUpdate` outside the set, is dropped for that frame.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct Asking;

/// Writes the asks of the frame into the event loop's settings. A faster pace someone else
/// asked for (a GIF) is left alone, and screenshot runs keep their continuous mode.
fn settle(
    covered: Res<WindowCovered>,
    mut pace: ResMut<Pace>,
    settings: Option<ResMut<WinitSettings>>,
) {
    let asked = pace.asked.take();
    let Some(mut settings) = settings else { return };
    if matches!(settings.focused_mode, UpdateMode::Continuous) {
        return;
    }
    let want = asked.filter(|_| !covered.0);
    let ours = pace
        .written
        .is_some_and(|written| settings.focused_mode == UpdateMode::reactive(written));
    match want {
        Some(wake) => {
            let faster_than_us = !ours
                && matches!(
                    settings.focused_mode,
                    UpdateMode::Reactive { wait, .. } if wait < wake.focused
                );
            if faster_than_us {
                // Someone else (a GIF) wakes sooner: leave it, and hold nothing to restore.
                pace.written = None;
                return;
            }
            if settings.focused_mode != UpdateMode::reactive(wake.focused)
                || settings.unfocused_mode != UpdateMode::reactive(wake.unfocused)
            {
                settings.focused_mode = UpdateMode::reactive(wake.focused);
                settings.unfocused_mode = UpdateMode::reactive(wake.unfocused);
            }
            if pace.written != Some(wake.focused) {
                pace.written = Some(wake.focused);
            }
        }
        None => {
            if ours {
                *settings = WinitSettings::desktop_app();
            }
            if pace.written.is_some() {
                pace.written = None;
            }
        }
    }
}

pub struct PacePlugin;

impl Plugin for PacePlugin {
    fn build(&self, app: &mut App) {
        app.add_message::<WindowOccluded>()
            .init_resource::<WindowCovered>()
            .init_resource::<Pace>()
            .add_systems(PreUpdate, track_occlusion)
            .add_systems(PostUpdate, settle.after(Asking));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Snapshot;
    use crate::testing;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn focused(app: &App) -> UpdateMode {
        app.world().resource::<WinitSettings>().focused_mode
    }

    fn unfocused(app: &App) -> UpdateMode {
        app.world().resource::<WinitSettings>().unfocused_mode
    }

    fn occlude(app: &mut App, occluded: bool) {
        app.world_mut().write_message(WindowOccluded {
            window: Entity::PLACEHOLDER,
            occluded,
        });
    }

    fn asker(wake: Wake) -> impl FnMut(ResMut<Pace>) {
        move |mut pace| pace.ask(wake)
    }

    fn app_asking(wake: Wake) -> App {
        let mut app = testing::app(Snapshot::default());
        app.insert_resource(WinitSettings::desktop_app());
        app.add_systems(PostUpdate, asker(wake).in_set(Asking));
        app
    }

    #[test]
    fn a_paced_wake_is_slower_without_focus_and_never_faster_than_with_it() {
        assert_eq!(
            Wake::paced(ms(100)),
            Wake {
                focused: ms(100),
                unfocused: UNFOCUSED_WAIT
            }
        );
        assert_eq!(Wake::paced(ms(900)).unfocused, ms(900));
        assert_eq!(Wake::steady(ms(250)).unfocused, ms(250));
    }

    #[test]
    fn the_settings_follow_the_ask_for_focus_and_without_it() {
        let mut app = app_asking(Wake::paced(ms(100)));
        app.update();
        assert_eq!(focused(&app), UpdateMode::reactive(ms(100)));
        assert_eq!(unfocused(&app), UpdateMode::reactive(UNFOCUSED_WAIT));
        assert_eq!(app.world().resource::<Pace>().written, Some(ms(100)));
    }

    #[test]
    fn the_shortest_ask_of_the_frame_wins() {
        let mut app = app_asking(Wake::paced(ms(200)));
        app.add_systems(PostUpdate, asker(Wake::steady(ms(250))).in_set(Asking));
        app.update();
        assert_eq!(focused(&app), UpdateMode::reactive(ms(200)));
        assert_eq!(unfocused(&app), UpdateMode::reactive(ms(250)));
    }

    #[test]
    fn no_ask_restores_the_desktop_settings() {
        let mut app = testing::app(Snapshot::default());
        app.insert_resource(WinitSettings::desktop_app());
        app.update();
        assert_eq!(focused(&app), WinitSettings::desktop_app().focused_mode);
        assert_eq!(app.world().resource::<Pace>().written, None);
    }

    #[test]
    fn a_covered_window_asks_for_no_wake_and_gets_it_back_when_shown() {
        let mut app = app_asking(Wake::paced(ms(100)));
        app.update();
        occlude(&mut app, true);
        for _ in 0..5 {
            app.update();
            assert!(app.world().resource::<WindowCovered>().0);
            assert_eq!(focused(&app), WinitSettings::desktop_app().focused_mode);
            assert_eq!(unfocused(&app), WinitSettings::desktop_app().unfocused_mode);
            assert_eq!(app.world().resource::<Pace>().written, None);
        }
        occlude(&mut app, false);
        app.update();
        assert_eq!(focused(&app), UpdateMode::reactive(ms(100)));
    }

    #[test]
    fn a_faster_wake_someone_else_asked_for_is_left_alone() {
        let mut app = app_asking(Wake::paced(ms(100)));
        app.insert_resource(WinitSettings {
            focused_mode: UpdateMode::reactive(ms(30)),
            unfocused_mode: UpdateMode::reactive(ms(30)),
        });
        app.update();
        assert_eq!(focused(&app), UpdateMode::reactive(ms(30)));
        assert_eq!(unfocused(&app), UpdateMode::reactive(ms(30)));
        assert_eq!(app.world().resource::<Pace>().written, None);
    }

    #[test]
    fn a_screenshot_run_stays_continuous() {
        let mut app = app_asking(Wake::paced(ms(100)));
        app.insert_resource(WinitSettings::continuous());
        app.update();
        assert_eq!(focused(&app), UpdateMode::Continuous);
    }
}
