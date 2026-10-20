//! An idle frame, and a tell that changes nothing, wake no view builder: every builder's run
//! condition stays false until a resource it reads changes or its minute ticks.

use bevy::prelude::*;
use clusia_core::PrRef;
use clusia_protocol::WindowTarget;

use crate::bridge::{self, Tell};
use crate::clock::Clock;
use crate::fixture;
use crate::nav::{MinuteTick, Nav, top_bar_changed};
use crate::review_state::{ReviewSection, ReviewTabs};
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

// The header's branch names the review on screen; a title is also in the tab strip and on Home.
const FIRST_TITLE: &str = "octo:auth-refresh";
const SECOND_TITLE: &str = "octo:widget-retry";

fn first() -> PrRef {
    fixture::demo_pr()
}

fn second() -> PrRef {
    PrRef::new("acme", "widgets", 7).expect("valid ref")
}

/// Opens `fixture::demo_review` as `pr`, titled `title`, in the tab the nav opens for it.
fn open_as(app: &mut App, pr: &PrRef, title: &str, branch: &str) {
    app.world_mut()
        .resource_mut::<Nav>()
        .go(&WindowTarget::Review { pr: pr.clone() });
    app.update();
    let mut view = fixture::demo_review(NOW).0;
    view.pr.summary.pr = pr.clone();
    view.pr.summary.title = title.into();
    view.pr.head_ref = branch.into();
    view.review.pr = pr.clone();
    view.review.title = title.into();
    testing::tell(
        app,
        Tell::Opened {
            pr: pr.clone(),
            view: Box::new(view),
            news: Vec::new(),
        },
    );
    testing::settle(app);
}

fn go(app: &mut App, target: WindowTarget) {
    app.world_mut().resource_mut::<Nav>().go(&target);
    testing::settle(app);
}

#[test]
fn switching_between_two_ready_reviews_shows_the_one_switched_to() {
    let mut app = testing::app(fixture::demo(NOW));
    open_as(&mut app, &first(), "feat: auth refresh", FIRST_TITLE);
    open_as(&mut app, &second(), "fix: widget retry", SECOND_TITLE);
    assert!(testing::shows(&mut app, SECOND_TITLE));
    assert!(!testing::shows(&mut app, FIRST_TITLE));
    watch(&mut app);
    // Only the nav changes: the shell's gate has to see the screen built anew.
    go(&mut app, WindowTarget::Review { pr: first() });
    assert!(testing::shows(&mut app, FIRST_TITLE));
    assert!(!testing::shows(&mut app, SECOND_TITLE));
    go(&mut app, WindowTarget::Review { pr: second() });
    assert!(testing::shows(&mut app, SECOND_TITLE));
    assert!(!testing::shows(&mut app, FIRST_TITLE));
}

#[test]
fn going_review_home_review_builds_each_screen_again() {
    let mut app = testing::app(fixture::demo(NOW));
    open_as(&mut app, &first(), "feat: auth refresh", FIRST_TITLE);
    watch(&mut app);
    go(&mut app, WindowTarget::Home);
    assert!(testing::shows(&mut app, "Dark mode for the docs"));
    assert!(!testing::shows(&mut app, FIRST_TITLE));
    go(&mut app, WindowTarget::Review { pr: first() });
    assert!(testing::shows(&mut app, FIRST_TITLE));
    assert!(!testing::shows(&mut app, "Dark mode for the docs"));
    go(&mut app, WindowTarget::Home);
    assert!(testing::shows(&mut app, "Dark mode for the docs"));
}

#[test]
fn an_agent_turn_updates_the_working_status_without_a_tab_change() {
    let mut app = testing::demo_review_app();
    watch(&mut app);
    assert!(!testing::shows(&mut app, "thinking…"));
    testing::tell(
        &mut app,
        Tell::Agent(bridge::AgentTell::State {
            pr: first(),
            state: clusia_protocol::SessionStateKind::Running,
        }),
    );
    testing::settle(&mut app);
    assert!(testing::shows(&mut app, "thinking…"), "the status bar");
    testing::tell(
        &mut app,
        Tell::Agent(bridge::AgentTell::State {
            pr: first(),
            state: clusia_protocol::SessionStateKind::Ready,
        }),
    );
    testing::settle(&mut app);
    assert!(!testing::shows(&mut app, "thinking…"));
}

#[test]
fn a_checks_tell_updates_the_security_and_audits_views() {
    for (section, kind) in [
        (
            ReviewSection::Security,
            clusia_core::checks::CheckKind::Security,
        ),
        (ReviewSection::Audits, clusia_core::checks::CheckKind::Audit),
    ] {
        let mut app = testing::demo_review_app();
        testing::open_checks(&mut app, &first());
        testing::set_section(&mut app, &first(), section);
        watch(&mut app);
        let id = fixture::demo_checks(NOW)
            .0
            .into_iter()
            .find(|r| r.kind == kind)
            .and_then(|r| r.findings.first().map(|f| f.id.clone()))
            .expect("a finding of the kind");
        let before = testing::shown_text(&mut app);
        // A settled finding changes no tab; only `Checks` moves.
        testing::tell(
            &mut app,
            Tell::Checks(bridge::ChecksTell::Settled {
                pr: first(),
                id,
                accepted: false,
            }),
        );
        testing::settle(&mut app);
        assert_ne!(before, testing::shown_text(&mut app), "{section:?}");
    }
}

#[test]
fn a_home_filter_updates_the_lists() {
    let mut app = testing::app(fixture::demo(NOW));
    watch(&mut app);
    assert!(testing::shows(&mut app, "Dark mode for the docs"));
    {
        let mut state = app
            .world_mut()
            .resource_mut::<crate::screens::home::HomeState>();
        state.seeded = true;
        state.query = "footer".into();
    }
    testing::settle(&mut app);
    assert!(!testing::shows(&mut app, "Dark mode for the docs"));
    assert!(testing::shows(&mut app, "Fix the footer links"));
}

#[test]
fn a_lost_connection_updates_the_right_panel_and_a_theme_change_leaves_it_as_it_was() {
    use crate::screens::review::shell::RightPanel;
    let mut app = testing::demo_review_app();
    watch(&mut app);
    let right = |app: &mut App| {
        let entity = testing::find::<RightPanel>(app, |_| true);
        testing::tree_signature(app, entity)
    };
    let live = right(&mut app);
    // A theme change reaches no right view: the panel is not rebuilt for it.
    app.world_mut()
        .resource_mut::<crate::theme::Theme>()
        .code_size += 4.0;
    testing::settle(&mut app);
    assert_eq!(right(&mut app), live);
    app.world_mut()
        .resource_mut::<crate::bridge::Model>()
        .connection = crate::bridge::Connection::Lost("gone".into());
    testing::settle(&mut app);
    assert_ne!(right(&mut app), live, "read-only once the daemon is gone");
}

#[test]
fn the_minute_turn_rewrites_the_age_labels() {
    let mut app = testing::app(fixture::demo(NOW));
    watch(&mut app);
    let minute_start = NOW - NOW.rem_euclid(60);
    assert!(testing::shows(&mut app, "5m"));
    app.world_mut()
        .insert_resource(Clock(Some(minute_start + 59)));
    testing::settle(&mut app);
    assert!(testing::shows(&mut app, "5m"), "inside the minute");
    app.world_mut().insert_resource(Clock(Some(NOW + 60)));
    testing::settle(&mut app);
    assert!(testing::shows(&mut app, "6m"));
    assert!(!testing::shows(&mut app, "5m"));
}
