//! Leaving a review without deciding (spec §6.5, mockup `LeavePrompt.png`): closing a review
//! tab, or the window, while the draft has items asks first. Quit from the tray never asks.

use bevy::input::ButtonInput;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, observe};
use bevy::window::WindowCloseRequested;
use clusia_core::{PrRef, ReviewState};
use clusia_protocol::WindowTarget;

use crate::bridge::{Ask, Asks, TOAST_SECS, Toast, Toasts};
use crate::fonts::UiFonts;
use crate::nav::{Nav, Screen, TabCloseRequested};
use crate::review_state::{Modal, Phase, ReviewTabs, Tab};
use crate::screens::review::ReviewSystems;
use crate::screens::review::diff::unsent;
use crate::screens::review::shell::ModalFor;
use crate::ui::composer::ExtraModes;
use crate::ui::kit::{Type, Variant, button, divider, text};
use crate::ui::modal::{escape_pressed, modal_card, modal_root};

/// Why a tab or the window did not close: a comment is still being written.
pub const FINISH_COMMENT: &str = "Finish or cancel the comment you're writing first";

/// Why the window did not close: a publish is still running.
pub const WAIT_PUBLISH: &str = "Wait for the publish to finish";

/// A window close in progress: the window, the tabs still to ask about, and the tabs left on
/// the daemon (`CloseReview`) only once the close goes ahead.
#[derive(Resource, Debug, Default)]
pub struct WindowClosing {
    pub window: Option<Entity>,
    pub queue: Vec<PrRef>,
    pub close: Vec<PrRef>,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct LeaveModal(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct LeaveDiscard(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct LeaveKeep(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct LeaveFinalize(pub PrRef);

/// Asks first only for an *Active* review that has draft items. The cached copy never asks:
/// nothing can be decided (or discarded) from it.
pub fn needs_prompt(tab: &Tab) -> bool {
    match &tab.phase {
        Phase::Ready(r) => {
            r.cached_at.is_none()
                && r.view.review.state == ReviewState::Active
                && !r.view.review.draft.items.is_empty()
        }
        _ => false,
    }
}

/// A comment is being written and not sent yet: the tab stays until it is finished or
/// cancelled (its text is never dropped).
fn writing(tab: &Tab) -> bool {
    unsent(tab.ui.editor.as_ref())
}

/// Why the window cannot close over this tab: a comment being written, or a publish running.
fn window_blocker(tab: &Tab) -> Option<&'static str> {
    if writing(tab) {
        Some(FINISH_COMMENT)
    } else if tab.ui.finalize.busy {
        Some(WAIT_PUBLISH)
    } else {
        None
    }
}

/// The first open tab that keeps the window from closing, and why.
fn window_held(nav: &Nav, tabs: &ReviewTabs) -> Option<(PrRef, &'static str)> {
    nav.reviews.iter().find_map(|pr| {
        let why = window_blocker(tabs.0.get(pr)?)?;
        Some((pr.clone(), why))
    })
}

/// Shows `pr` and says why it did not close.
fn hold(pr: &PrRef, why: &str, nav: &mut Nav, toasts: &mut Toasts, time: &Time) {
    nav.go(&WindowTarget::Review { pr: pr.clone() });
    toasts.0.push(Toast {
        text: why.to_string(),
        warning: true,
        until: time.elapsed_secs_f64() + TOAST_SECS,
    });
}

pub fn leave_text(pr: &PrRef, items: usize) -> (String, String) {
    let comments = if items == 1 {
        "1 comment".to_string()
    } else {
        format!("{items} comments")
    };
    (
        format!("Close #{} without deciding?", pr.number),
        format!(
            "Your draft has {comments} and is already saved. If you keep it for later, the tray reminds you about it."
        ),
    )
}

/// Removes the tab from the top bar and forgets its state, including the modes of its Finalize
/// composers: every way of dropping a review comes through here.
pub fn close_tab(pr: &PrRef, tabs: &mut ReviewTabs, nav: &mut Nav, modes: &mut ExtraModes) {
    nav.close_review(pr);
    tabs.0.remove(pr);
    modes.forget(pr);
}

pub struct LeavePlugin;

impl Plugin for LeavePlugin {
    fn build(&self, app: &mut App) {
        // `WindowPlugin` registers it in the real app; headless tests need it too.
        app.add_message::<WindowCloseRequested>()
            .init_resource::<WindowClosing>()
            .add_systems(
                Update,
                (
                    tab_close,
                    window_close,
                    advance_closing,
                    spawn_modal,
                    escape_cancels,
                )
                    .chain()
                    .after(ReviewSystems),
            );
    }
}

fn tab_close(
    mut requests: MessageReader<TabCloseRequested>,
    mut tabs: ResMut<ReviewTabs>,
    mut nav: ResMut<Nav>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    mut modes: ResMut<ExtraModes>,
    time: Res<Time>,
) {
    for TabCloseRequested(pr) in requests.read() {
        let Some(tab) = tabs.0.get_mut(pr) else {
            nav.close_review(pr);
            continue;
        };
        if writing(tab) {
            hold(pr, FINISH_COMMENT, &mut nav, &mut toasts, &time);
            continue;
        }
        if needs_prompt(tab) {
            tab.ui.modal = Some(Modal::Leave { window: false });
            nav.go(&WindowTarget::Review { pr: pr.clone() });
            continue;
        }
        if matches!(tab.phase, Phase::Ready(_)) {
            asks.send(Ask::CloseReview(pr.clone()));
        }
        close_tab(pr, &mut tabs, &mut nav, &mut modes);
    }
}

fn window_close(
    mut requests: MessageReader<WindowCloseRequested>,
    mut closing: ResMut<WindowClosing>,
    tabs: Res<ReviewTabs>,
    mut nav: ResMut<Nav>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    for request in requests.read() {
        if closing.window.is_some() {
            continue; // already asking
        }
        // A comment being written or a publish running cancels the close: nothing is sent.
        if let Some((pr, why)) = window_held(&nav, &tabs) {
            hold(&pr, why, &mut nav, &mut toasts, &time);
            continue;
        }
        *closing = WindowClosing {
            window: Some(request.window),
            ..default()
        };
        for pr in &nav.reviews {
            let Some(tab) = tabs.0.get(pr) else { continue };
            if needs_prompt(tab) {
                closing.queue.push(pr.clone());
            } else if matches!(tab.phase, Phase::Ready(_)) {
                closing.close.push(pr.clone());
            }
        }
    }
}

/// Asks about the next tab, or closes the window when none is left.
fn advance_closing(
    mut commands: Commands,
    mut closing: ResMut<WindowClosing>,
    mut tabs: ResMut<ReviewTabs>,
    mut nav: ResMut<Nav>,
    windows: Query<(), With<Window>>,
    mut exit: MessageWriter<AppExit>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Some(window) = closing.window else {
        return;
    };
    let asking = tabs
        .0
        .values()
        .any(|t| t.ui.modal == Some(Modal::Leave { window: true }));
    if asking {
        return;
    }
    while !closing.queue.is_empty() {
        let pr = closing.queue.remove(0);
        if let Some(tab) = tabs.0.get_mut(&pr)
            && needs_prompt(tab)
        {
            tab.ui.modal = Some(Modal::Leave { window: true });
            nav.go(&WindowTarget::Review { pr });
            return;
        }
    }
    // Re-checked before leaving anything: the close is cancelled as it would have been at first.
    if let Some((pr, why)) = window_held(&nav, &tabs) {
        hold(&pr, why, &mut nav, &mut toasts, &time);
        *closing = WindowClosing::default();
        return;
    }
    // The close is sure now: the tabs that needed no answer are left on the daemon.
    for pr in std::mem::take(&mut closing.close) {
        if tabs
            .0
            .get(&pr)
            .is_some_and(|t| matches!(t.phase, Phase::Ready(_)))
        {
            asks.send(Ask::CloseReview(pr));
        }
    }
    if windows.contains(window) {
        commands.entity(window).despawn();
    }
    exit.write(AppExit::Success);
    *closing = WindowClosing::default();
}

fn spawn_modal(
    mut commands: Commands,
    nav: Res<Nav>,
    tabs: Res<ReviewTabs>,
    fonts: Res<UiFonts>,
    modals: Query<&ModalFor>,
) {
    let Screen::Review(pr) = &nav.screen else {
        return;
    };
    let Some(tab) = tabs.0.get(pr) else { return };
    let Some(modal @ Modal::Leave { .. }) = tab.ui.modal else {
        return;
    };
    if modals.iter().any(|m| &m.pr == pr && m.modal == modal) {
        return;
    }
    let items = match &tab.phase {
        Phase::Ready(r) => r.view.review.draft.items.len(),
        _ => 0,
    };
    let (title, body) = leave_text(pr, items);
    let fonts = &*fonts;
    commands
        .spawn((
            modal_root(),
            ModalFor {
                pr: pr.clone(),
                modal,
            },
            LeaveModal(pr.clone()),
        ))
        .with_children(|root| {
            root.spawn(modal_card(460.0)).with_children(|card| {
                card.spawn(Node {
                    flex_direction: FlexDirection::Column,
                    row_gap: px(8),
                    padding: UiRect::axes(px(24), px(20)),
                    ..default()
                })
                .with_children(|c| {
                    c.spawn(text(fonts, title, Type::HEADING));
                    c.spawn(text(fonts, body, Type::MUTED));
                });
                card.spawn(divider());
                card.spawn(Node {
                    padding: UiRect::axes(px(24), px(14)),
                    column_gap: px(10),
                    align_items: AlignItems::Center,
                    ..default()
                })
                .with_children(|f| {
                    f.spawn((
                        button(fonts, "Discard", Variant::Danger),
                        LeaveDiscard(pr.clone()),
                        observe(on_discard),
                    ));
                    f.spawn(Node {
                        flex_grow: 1.0,
                        ..default()
                    });
                    f.spawn((
                        button(fonts, "Keep for later", Variant::Secondary),
                        LeaveKeep(pr.clone()),
                        observe(on_keep),
                    ));
                    f.spawn((
                        button(fonts, "Finalize now", Variant::Primary),
                        LeaveFinalize(pr.clone()),
                        observe(on_finalize_now),
                    ));
                });
            });
        });
}

fn on_discard(
    activate: On<Activate>,
    buttons: Query<&LeaveDiscard>,
    mut tabs: ResMut<ReviewTabs>,
    mut nav: ResMut<Nav>,
    mut asks: ResMut<Asks>,
    mut modes: ResMut<ExtraModes>,
) {
    if let Ok(LeaveDiscard(pr)) = buttons.get(activate.entity) {
        asks.send(Ask::Discard(pr.clone()));
        close_tab(pr, &mut tabs, &mut nav, &mut modes);
    }
}

fn on_keep(
    activate: On<Activate>,
    buttons: Query<&LeaveKeep>,
    mut tabs: ResMut<ReviewTabs>,
    mut nav: ResMut<Nav>,
    mut asks: ResMut<Asks>,
    mut modes: ResMut<ExtraModes>,
) {
    if let Ok(LeaveKeep(pr)) = buttons.get(activate.entity) {
        asks.send(Ask::CloseReview(pr.clone()));
        close_tab(pr, &mut tabs, &mut nav, &mut modes);
    }
}

fn on_finalize_now(
    activate: On<Activate>,
    buttons: Query<&LeaveFinalize>,
    mut tabs: ResMut<ReviewTabs>,
    mut closing: ResMut<WindowClosing>,
) {
    if let Ok(LeaveFinalize(pr)) = buttons.get(activate.entity)
        && let Some(tab) = tabs.0.get_mut(pr)
    {
        tab.ui.modal = Some(Modal::Finalize);
        *closing = WindowClosing::default(); // deciding now: the window stays
    }
}

fn escape_cancels(
    keys: Res<ButtonInput<KeyCode>>,
    nav: Res<Nav>,
    mut tabs: ResMut<ReviewTabs>,
    mut closing: ResMut<WindowClosing>,
) {
    if !escape_pressed(&keys) {
        return;
    }
    let Screen::Review(pr) = &nav.screen else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(pr)
        && matches!(tab.ui.modal, Some(Modal::Leave { .. }))
    {
        tab.ui.modal = None;
        *closing = WindowClosing::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Tell, Toasts};
    use crate::fixture;
    use crate::nav::CloseTab;
    use crate::review_state::{EditTarget, Editor, Ready};
    use crate::testing::{self, NOW};
    use crate::ui::composer::ComposerMode;
    use bevy::window::PrimaryWindow;

    /// An `AppExit` was written in this frame or the one before (messages live two frames,
    /// so check right after the frame that should exit).
    fn exits(app: &App) -> bool {
        !app.world().resource::<Messages<AppExit>>().is_empty()
    }

    /// The demo review open and ready, without What's new (`open_ready(.., false)`).
    fn review_app() -> App {
        let mut app = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut app, false);
        app
    }

    fn window(app: &mut App) -> Entity {
        app.world_mut()
            .spawn((Window::default(), PrimaryWindow))
            .id()
    }

    /// One frame: the request is read, the first tab is asked about (or the window closes).
    fn close_window(app: &mut App, window: Entity) {
        app.world_mut()
            .write_message(WindowCloseRequested { window });
        app.update();
    }

    fn set_active(app: &mut App, pr: &PrRef) {
        let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
        if let Phase::Ready(r) = &mut tabs.0.get_mut(pr).unwrap().phase {
            r.view.review.state = ReviewState::Active;
        }
    }

    fn empty_draft(app: &mut App, pr: &PrRef) {
        let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
        if let Phase::Ready(r) = &mut tabs.0.get_mut(pr).unwrap().phase {
            r.view.review.draft.items.clear();
        }
    }

    #[test]
    fn leave_copy() {
        let pr = fixture::demo_pr();
        assert_eq!(
            leave_text(&pr, 3),
            (
                "Close #123 without deciding?".to_string(),
                "Your draft has 3 comments and is already saved. If you keep it for later, the tray reminds you about it."
                    .to_string()
            )
        );
        assert!(
            leave_text(&pr, 1)
                .1
                .starts_with("Your draft has 1 comment and")
        );
    }

    #[test]
    fn closing_a_tab_with_items_asks() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        set_active(&mut app, &pr);
        let close = testing::find::<CloseTab>(&mut app, |c| c.0 == pr);
        testing::activate(&mut app, close);
        testing::settle(&mut app);
        assert_eq!(
            testing::tab(&app, &pr).ui.modal,
            Some(Modal::Leave { window: false })
        );
        assert!(
            app.world().resource::<Nav>().reviews.contains(&pr),
            "still open"
        );
        assert_eq!(testing::count::<LeaveModal>(&mut app), 1);
        assert!(testing::recorded(&mut app).is_empty(), "nothing sent yet");
        let keep = testing::find::<LeaveKeep>(&mut app, |_| true);
        testing::activate(&mut app, keep);
        assert_eq!(testing::recorded(&mut app), [Ask::CloseReview(pr.clone())]);
        testing::settle(&mut app);
        let nav = app.world().resource::<Nav>();
        assert_eq!(nav.screen, Screen::Home);
        assert!(!nav.reviews.contains(&pr));
        assert!(!app.world().resource::<ReviewTabs>().0.contains_key(&pr));
        assert_eq!(testing::count::<LeaveModal>(&mut app), 0);
    }

    #[test]
    fn every_way_of_leaving_forgets_the_finalize_modes() {
        use crate::ui::composer::{ComposerKey, ExtraModes, Slot, mode_of, set_mode};
        for how in ["close", "keep", "discard"] {
            let mut app = review_app();
            let pr = fixture::demo_pr();
            set_active(&mut app, &pr);
            if how == "close" {
                empty_draft(&mut app, &pr);
            }
            let key = ComposerKey(pr.clone(), Slot::FinalizeSummary);
            {
                let world = app.world_mut();
                let mut extra = world.remove_resource::<ExtraModes>().unwrap();
                let mut tabs = world.remove_resource::<ReviewTabs>().unwrap();
                set_mode(&key, ComposerMode::Preview, &mut tabs, &mut extra);
                world.insert_resource(extra);
                world.insert_resource(tabs);
            }
            let close = testing::find::<CloseTab>(&mut app, |c| c.0 == pr);
            testing::activate(&mut app, close);
            testing::settle(&mut app);
            match how {
                "keep" => {
                    let b = testing::find::<LeaveKeep>(&mut app, |_| true);
                    testing::activate(&mut app, b);
                }
                "discard" => {
                    let b = testing::find::<LeaveDiscard>(&mut app, |_| true);
                    testing::activate(&mut app, b);
                }
                _ => {}
            }
            testing::settle(&mut app);
            assert!(!app.world().resource::<ReviewTabs>().0.contains_key(&pr));
            let mode = mode_of(
                &key,
                app.world().resource::<ReviewTabs>(),
                app.world().resource::<ExtraModes>(),
            );
            assert_eq!(mode, ComposerMode::Write, "{how}: forgotten");
        }
    }

    #[test]
    fn closing_an_empty_tab_does_not_ask() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        set_active(&mut app, &pr);
        empty_draft(&mut app, &pr);
        let close = testing::find::<CloseTab>(&mut app, |c| c.0 == pr);
        testing::activate(&mut app, close);
        testing::settle(&mut app);
        assert_eq!(testing::count::<LeaveModal>(&mut app), 0);
        assert_eq!(testing::recorded(&mut app), [Ask::CloseReview(pr.clone())]);
        assert!(!app.world().resource::<Nav>().reviews.contains(&pr));
    }

    #[test]
    fn discard_and_finalize_now() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        set_active(&mut app, &pr);
        let close = testing::find::<CloseTab>(&mut app, |c| c.0 == pr);
        testing::activate(&mut app, close);
        testing::settle(&mut app);
        let now = testing::find::<LeaveFinalize>(&mut app, |_| true);
        testing::activate(&mut app, now);
        assert_eq!(testing::tab(&app, &pr).ui.modal, Some(Modal::Finalize));
        assert!(testing::recorded(&mut app).is_empty());
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            tabs.0.get_mut(&pr).unwrap().ui.modal = None;
        }
        let close = testing::find::<CloseTab>(&mut app, |c| c.0 == pr);
        testing::activate(&mut app, close);
        testing::settle(&mut app);
        let discard = testing::find::<LeaveDiscard>(&mut app, |_| true);
        testing::activate(&mut app, discard);
        assert_eq!(testing::recorded(&mut app), [Ask::Discard(pr.clone())]);
        assert!(!app.world().resource::<Nav>().reviews.contains(&pr));
    }

    #[test]
    fn window_close_asks_then_exits() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        set_active(&mut app, &pr);
        // A second tab that is still loading closes without a prompt or a CloseReview.
        let loading: PrRef = "rzorzal/clusia#98".parse().unwrap();
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Review { pr: loading });
        testing::settle(&mut app);
        testing::recorded(&mut app);
        let w = window(&mut app);
        close_window(&mut app, w);
        assert_eq!(
            testing::tab(&app, &pr).ui.modal,
            Some(Modal::Leave { window: true })
        );
        assert_eq!(
            app.world().resource::<Nav>().screen,
            Screen::Review(pr.clone())
        );
        assert!(
            app.world().get_entity(w).is_ok(),
            "the window waits for the answer"
        );
        assert!(!exits(&app));
        let keep = testing::find::<LeaveKeep>(&mut app, |_| true);
        testing::activate(&mut app, keep);
        assert!(exits(&app));
        assert!(app.world().get_entity(w).is_err(), "the window is gone");
        assert_eq!(testing::recorded(&mut app), [Ask::CloseReview(pr)]);
    }

    #[test]
    fn window_close_without_items_exits_at_once() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        empty_draft(&mut app, &pr);
        let w = window(&mut app);
        close_window(&mut app, w);
        assert_eq!(testing::count::<LeaveModal>(&mut app), 0);
        assert_eq!(testing::recorded(&mut app), [Ask::CloseReview(pr)]);
        assert!(app.world().get_entity(w).is_err());
        assert!(exits(&app));
    }

    #[test]
    fn finalize_now_cancels_the_window_close() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        set_active(&mut app, &pr);
        // A second ready tab without items: closed on the daemon only once the close is sure.
        let quiet: PrRef = "rzorzal/clusia#98".parse().unwrap();
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Review { pr: quiet.clone() });
        app.update();
        {
            let (mut view, _) = fixture::demo_review(NOW);
            view.review.draft.items.clear();
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            tabs.0.get_mut(&quiet).unwrap().phase = Phase::Ready(Box::new(Ready {
                view,
                news: Vec::new(),
                cached_at: None,
            }));
        }
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Review { pr: pr.clone() });
        testing::settle(&mut app);
        testing::recorded(&mut app);
        let w = window(&mut app);
        close_window(&mut app, w);
        assert!(
            testing::recorded(&mut app).is_empty(),
            "nothing sent while asking"
        );
        let now = testing::find::<LeaveFinalize>(&mut app, |_| true);
        testing::activate(&mut app, now);
        assert!(!exits(&app));
        testing::settle(&mut app);
        assert_eq!(testing::tab(&app, &pr).ui.modal, Some(Modal::Finalize));
        assert!(app.world().get_entity(w).is_ok());
        assert_eq!(app.world().resource::<WindowClosing>().window, None);
        assert!(
            testing::recorded(&mut app).is_empty(),
            "the cancelled close sent nothing"
        );
        assert!(app.world().resource::<Nav>().reviews.contains(&quiet));
    }

    #[test]
    fn a_publish_in_flight_cancels_the_window_close() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        set_active(&mut app, &pr);
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            let tab = tabs.0.get_mut(&pr).unwrap();
            tab.ui.modal = Some(Modal::Finalize);
            tab.ui.finalize.busy = true;
        }
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Home);
        app.update();
        let w = window(&mut app);
        close_window(&mut app, w);
        assert!(!exits(&app));
        assert!(app.world().get_entity(w).is_ok(), "the window stays");
        assert!(testing::recorded(&mut app).is_empty(), "nothing sent");
        assert_eq!(app.world().resource::<WindowClosing>().window, None);
        assert_eq!(
            app.world().resource::<Nav>().screen,
            Screen::Review(pr.clone())
        );
        assert_eq!(testing::tab(&app, &pr).ui.modal, Some(Modal::Finalize));
        assert_eq!(toasts(&app), [WAIT_PUBLISH]);
    }

    /// A comment being written on the demo tab, not sent yet.
    fn unsent_editor(app: &mut App, pr: &PrRef) {
        let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
        tabs.0.get_mut(pr).unwrap().ui.editor = Some(Editor {
            target: EditTarget::General,
            text: "Half a thought".into(),
            error: None,
            ticket: None,
            mode: ComposerMode::Write,
        });
    }

    fn toasts(app: &App) -> Vec<String> {
        let toasts = app.world().resource::<Toasts>();
        toasts.0.iter().map(|t| t.text.clone()).collect()
    }

    #[test]
    fn an_unsent_comment_keeps_the_tab_open() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        set_active(&mut app, &pr);
        unsent_editor(&mut app, &pr);
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Home);
        testing::settle(&mut app);
        let close = testing::find::<CloseTab>(&mut app, |c| c.0 == pr);
        testing::activate(&mut app, close);
        testing::settle(&mut app);
        let nav = app.world().resource::<Nav>();
        assert!(nav.reviews.contains(&pr), "still open");
        assert_eq!(nav.screen, Screen::Review(pr.clone()), "shown, to finish");
        assert_eq!(testing::count::<LeaveModal>(&mut app), 0);
        assert!(testing::recorded(&mut app).is_empty(), "nothing sent");
        assert!(
            testing::tab(&app, &pr).ui.editor.is_some(),
            "the text is kept"
        );
        assert_eq!(toasts(&app), [FINISH_COMMENT]);
        // A sent comment (waiting on its ticket) does not hold the tab.
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .editor
            .as_mut()
            .unwrap()
            .ticket = Some(7);
        let close = testing::find::<CloseTab>(&mut app, |c| c.0 == pr);
        testing::activate(&mut app, close);
        testing::settle(&mut app);
        assert_eq!(testing::count::<LeaveModal>(&mut app), 1, "asks as usual");
    }

    #[test]
    fn an_unsent_comment_cancels_the_window_close() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        empty_draft(&mut app, &pr);
        unsent_editor(&mut app, &pr);
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&WindowTarget::Home);
        app.update();
        let w = window(&mut app);
        close_window(&mut app, w);
        assert!(!exits(&app));
        assert!(app.world().get_entity(w).is_ok(), "the window stays");
        assert!(testing::recorded(&mut app).is_empty(), "nothing sent");
        assert_eq!(app.world().resource::<WindowClosing>().window, None);
        assert_eq!(
            app.world().resource::<Nav>().screen,
            Screen::Review(pr.clone())
        );
        assert_eq!(toasts(&app), [FINISH_COMMENT]);
    }

    #[test]
    fn the_cached_copy_closes_without_asking() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        set_active(&mut app, &pr);
        if let Phase::Ready(r) = &mut app
            .world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .phase
        {
            r.cached_at = Some(NOW - 3600);
        }
        assert!(!needs_prompt(&testing::tab(&app, &pr)));
        let close = testing::find::<CloseTab>(&mut app, |c| c.0 == pr);
        testing::activate(&mut app, close);
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<LeaveModal>(&mut app),
            0,
            "no Discard offered"
        );
        assert_eq!(testing::recorded(&mut app), [Ask::CloseReview(pr.clone())]);
        assert!(!app.world().resource::<Nav>().reviews.contains(&pr));
    }

    #[test]
    fn tray_quit_skips_the_prompt() {
        let mut app = review_app();
        let pr = fixture::demo_pr();
        set_active(&mut app, &pr);
        testing::tell(&mut app, Tell::Quit);
        assert!(exits(&app));
        testing::settle(&mut app);
        assert_eq!(testing::count::<LeaveModal>(&mut app), 0);
        assert_eq!(testing::tab(&app, &pr).ui.modal, None);
    }
}
