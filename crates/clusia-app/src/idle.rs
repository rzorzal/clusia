//! An idle frame, and a tell that changes nothing, wake no view builder: every builder's run
//! condition stays false until a resource it reads changes or its minute ticks.

use bevy::prelude::*;

use crate::bridge::{self, Tell};
use crate::clock::Clock;
use crate::fixture;
use crate::nav::{MinuteTick, Nav, top_bar_changed};
use crate::review_state::ReviewTabs;
use crate::screens::home::home_changed;
use crate::screens::review::checks::checks_views_changed;
use crate::screens::review::revalidation::revalidation_changed;
use crate::screens::review::shell::shell_views_changed;
use crate::testing::{self, NOW};

/// How many frames each run condition said yes.
#[derive(Resource, Default, Debug, PartialEq, Eq, Clone, Copy)]
struct Wakes {
    top_bar: u32,
    home: u32,
    shell: u32,
    checks: u32,
    revalidation: u32,
}

/// The same conditions the builders run under, evaluated once per frame after `Update`.
fn watch(app: &mut App) {
    app.init_resource::<Wakes>().add_systems(
        PostUpdate,
        (
            top_bar_changed.pipe(|In(on): In<bool>, mut w: ResMut<Wakes>| w.top_bar += on as u32),
            home_changed.pipe(|In(on): In<bool>, mut w: ResMut<Wakes>| w.home += on as u32),
            shell_views_changed.pipe(|In(on): In<bool>, mut w: ResMut<Wakes>| w.shell += on as u32),
            checks_views_changed
                .pipe(|In(on): In<bool>, mut w: ResMut<Wakes>| w.checks += on as u32),
            revalidation_changed
                .pipe(|In(on): In<bool>, mut w: ResMut<Wakes>| w.revalidation += on as u32),
        ),
    );
    testing::settle(app);
    testing::settle(app);
    app.world_mut().insert_resource(Wakes::default());
}

fn wakes(app: &App) -> Wakes {
    *app.world().resource::<Wakes>()
}

fn reset(app: &mut App) {
    app.world_mut().insert_resource(Wakes::default());
}

fn idle_frames(app: &mut App, n: usize) {
    for _ in 0..n {
        app.update();
    }
}

#[test]
fn idle_frames_on_a_review_wake_no_builder() {
    let mut app = testing::demo_review_app();
    watch(&mut app);
    idle_frames(&mut app, 10);
    assert_eq!(wakes(&app), Wakes::default());
}

#[test]
fn idle_frames_on_home_wake_no_builder() {
    let mut app = testing::app(fixture::demo(NOW));
    watch(&mut app);
    idle_frames(&mut app, 10);
    assert_eq!(wakes(&app), Wakes::default());
}

#[test]
fn a_tell_that_changes_nothing_wakes_no_builder() {
    let mut app = testing::demo_review_app();
    watch(&mut app);
    testing::tell(
        &mut app,
        Tell::Notice {
            text: "Saved".into(),
            warning: false,
        },
    );
    testing::tell(
        &mut app,
        Tell::Media {
            url: "https://example.com/a.png".into(),
            file: Err(bridge::MediaError::refused("gone")),
        },
    );
    idle_frames(&mut app, 3);
    assert_eq!(wakes(&app), Wakes::default());
}

#[test]
fn a_changed_tab_wakes_the_shell_and_the_checks_and_nothing_on_home() {
    let mut app = testing::demo_review_app();
    watch(&mut app);
    app.world_mut().resource_mut::<ReviewTabs>().set_changed();
    app.update();
    let w = wakes(&app);
    assert_eq!((w.shell, w.checks, w.revalidation), (1, 1, 1));
    assert_eq!(w.home, 0);
}

#[test]
fn a_changed_nav_wakes_the_top_bar() {
    let mut app = testing::demo_review_app();
    watch(&mut app);
    app.world_mut().resource_mut::<Nav>().set_changed();
    app.update();
    assert_eq!(wakes(&app).top_bar, 1);
}

#[test]
fn the_minute_ticks_the_age_labels_and_nothing_else_does() {
    let mut app = testing::demo_review_app();
    watch(&mut app);
    let minute_start = NOW - NOW.rem_euclid(60);
    app.world_mut()
        .insert_resource(Clock(Some(minute_start + 59)));
    idle_frames(&mut app, 2);
    assert_eq!(wakes(&app), Wakes::default(), "inside the same minute");
    let minute = app.world().resource::<MinuteTick>().0;
    app.world_mut()
        .insert_resource(Clock(Some(minute_start + 60)));
    app.update();
    assert_ne!(app.world().resource::<MinuteTick>().0, minute);
    let w = wakes(&app);
    assert_eq!(
        (w.shell, w.checks, w.home),
        (1, 1, 1),
        "the next minute rebuilds the age labels once"
    );
    reset(&mut app);
    idle_frames(&mut app, 3);
    assert_eq!(wakes(&app), Wakes::default());
}

#[test]
fn a_new_review_screen_is_built_without_a_resource_changing() {
    let mut app = testing::app(fixture::demo(NOW));
    watch(&mut app);
    testing::open_ready(&mut app, false);
    testing::settle(&mut app);
    assert!(wakes(&app).shell >= 1);
}
