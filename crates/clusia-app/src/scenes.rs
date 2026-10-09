//! `--demo --scene <name>`: stages the demo review in one state, so every review screen can be
//! rendered (screenshots) without a daemon.

use bevy::prelude::*;
use bevy::ui_widgets::ScrollArea;
use clusia_core::draft::{DraftKind, ThreadRef};
use clusia_core::{Side, Verdict};
use clusia_protocol::{
    AuthInfo, LoadStep, LoadStepKind, SessionStateKind, StepStatus, SyncState, SyncStatus,
    WindowTarget,
};

use crate::args::Scene;
use crate::bridge::{Model, ProbeState};
use crate::clock::Clock;
use crate::fixture;
use crate::nav::{Nav, Section};
use crate::review_state::{
    DiffMode, EditTarget, Editor, Modal, Phase, Ready, ReviewSection, ReviewTabs, Tab, TabUi,
};
use crate::screens::open_pr::Palette;
use crate::screens::review::agent::{Chats, PanelTab};
use crate::ui::composer::popover::{PopoverKind, Popovers};
use crate::ui::composer::{ComposerKey, ComposerMode, Slot};

/// Stages `scene` once at startup.
pub struct ScenePlugin(pub Scene);

impl Plugin for ScenePlugin {
    fn build(&self, app: &mut App) {
        let scene = self.0;
        app.add_systems(Startup, move |world: &mut World| stage(world, scene))
            // The demo snapshot is installed during `Startup`; a scene that changes it waits.
            .add_systems(PostStartup, move |world: &mut World| {
                stage_snapshot(world, scene);
            });
        add_scroll(app, scene);
    }
}

/// Only the scene that shows the end of the Notifications page keeps its scroll area scrolled.
fn add_scroll(app: &mut App, scene: Scene) {
    if scene == Scene::ConfigNotificationsBottom {
        app.add_systems(Update, scroll_to_bottom);
    }
}

/// Keeps the page's scroll area at its end; the layout clamps the position to the content.
fn scroll_to_bottom(mut areas: Query<&mut ScrollPosition, With<ScrollArea>>) {
    for mut position in &mut areas {
        position.y = f32::MAX;
    }
}

fn steps(pr: &clusia_core::PrRef) -> Vec<LoadStep> {
    let step = |step, status, message: Option<&str>| LoadStep {
        pr: pr.clone(),
        step,
        status,
        message: message.map(String::from),
    };
    vec![
        step(LoadStepKind::Repo, StepStatus::Done, Some("~/Repos/clusia")),
        step(
            LoadStepKind::Branch,
            StepStatus::Done,
            Some("~/Library/Application Support/Clusia/worktrees/rzorzal~clusia~123"),
        ),
        step(LoadStepKind::Pr, StepStatus::Running, None),
    ]
}

/// The comment the composer scenes have typed (mockup `Composer.png`).
const COMPOSER_TEXT: &str = "**Holding the lock across the network call** is what bit us on CI :turtle:\n\nCould we:\n1. re-check `expires_at` after taking the lock\n2. release it before `exchange`?\n\n_Trace from the race test below_ :point_down:\n![waiting](https://media.giphy.com/media/demo-party/giphy.gif)";

fn composer_target() -> EditTarget {
    EditTarget::Line {
        path: "src/auth/refresh.rs".into(),
        side: Side::Right,
        start: None,
        line: 44,
    }
}

fn composer_editor(mode: ComposerMode) -> Editor {
    Editor {
        target: composer_target(),
        text: COMPOSER_TEXT.into(),
        error: None,
        ticket: None,
        mode,
    }
}

/// What the Config › Notifications mockup shows: Do not disturb on weekday evenings, and the two
/// switches under it on.
const NOTIFICATION_SETTINGS: [(&str, &str); 6] = [
    ("notifications.dnd.enabled", "true"),
    ("notifications.dnd.from", "19:00"),
    ("notifications.dnd.to", "09:00"),
    (
        "notifications.dnd.days",
        r#"["mon","tue","wed","thu","fri"]"#,
    ),
    ("notifications.follow_focus", "true"),
    ("notifications.group_bursts", "true"),
];

/// Writes demo settings into the snapshot the way the daemon would parse them.
fn set_demo_config(world: &mut World, settings: &[(&str, &str)]) {
    let mut model = world.resource_mut::<Model>();
    for (key, value) in settings {
        crate::snapshot::apply_config_locally(&mut model.snapshot.config, key, value)
            .expect("the demo setting is valid");
    }
}

/// Signs the demo out and gives the first-run screen its answers.
fn stage_snapshot(world: &mut World, scene: Scene) {
    match scene {
        Scene::ConfigGeneral => set_demo_config(world, &[("general.start_at_login", "true")]),
        Scene::ConfigNotifications | Scene::ConfigNotificationsBottom => {
            set_demo_config(world, &NOTIFICATION_SETTINGS);
        }
        Scene::ConfigHarness => {
            set_demo_config(
                world,
                &[
                    ("harness.program", "/opt/homebrew/bin/claude"),
                    ("harness.extra_args", "--model claude-opus-5-5"),
                ],
            );
            world.resource_mut::<Model>().probe = ProbeState::Done(fixture::demo_probe());
        }
        Scene::AgentChat => {
            set_demo_config(world, &[("harness.program", "/opt/homebrew/bin/claude")])
        }
        _ => {}
    }
    if scene != Scene::FirstRun {
        return;
    }
    let mut model = world.resource_mut::<Model>();
    model.snapshot.auth = Some(AuthInfo {
        source: None,
        login: None,
        scopes: Vec::new(),
        error: Some("no GitHub token".into()),
    });
    model.snapshot.sync = Some(SyncStatus {
        state: SyncState::Unauthorized,
        ..SyncStatus::default()
    });
    model.snapshot.first_run = Some(fixture::demo_first_run());
}

/// Puts the demo review `rzorzal/clusia#123` in the state `scene` shows (Config pages and
/// the first run need no review).
pub fn stage(world: &mut World, scene: Scene) {
    let section = match scene {
        Scene::ConfigGeneral => Some(Section::General),
        Scene::ConfigNotifications | Scene::ConfigNotificationsBottom => {
            Some(Section::Notifications)
        }
        Scene::ConfigMedia => Some(Section::Media),
        Scene::ConfigAbout => Some(Section::About),
        Scene::ConfigHarness => Some(Section::Harness),
        _ => None,
    };
    if let Some(section) = section {
        world.resource_mut::<Nav>().open_section(section);
        return;
    }
    if scene == Scene::FirstRun {
        return;
    }
    let now = world.resource::<Clock>().now();
    let (view, news) = fixture::demo_review(now);
    let pr = view.review.pr.clone();
    let mut ready = Ready {
        view,
        news,
        cached_at: None,
    };
    if scene != Scene::WhatsNew {
        // Only the What's new scene shows the What's new modal (it opens on a fresh review
        // with news); the other scenes clear the news.
        ready.news.clear();
    }
    let mut ui = TabUi::default();
    match scene {
        Scene::Diff | Scene::Loading | Scene::Failed | Scene::Palette => {}
        Scene::FirstRun
        | Scene::ConfigGeneral
        | Scene::ConfigNotifications
        | Scene::ConfigNotificationsBottom
        | Scene::ConfigMedia
        | Scene::ConfigAbout
        | Scene::ConfigHarness => {}
        Scene::AgentChat => ui.file = Some("src/auth/refresh.rs".into()),
        Scene::Composer | Scene::Emoji | Scene::Gif => {
            ui.editor = Some(composer_editor(ComposerMode::Write));
        }
        Scene::ComposerPreview => ui.editor = Some(composer_editor(ComposerMode::Preview)),
        Scene::Rendered => ui.section = ReviewSection::Comments,
        Scene::Split => ui.mode = DiffMode::Split,
        Scene::Comments => {
            ui.section = ReviewSection::Comments;
            let thread = ready.view.conversation.as_ref().and_then(|c| {
                c.review_threads
                    .iter()
                    .find(|t| !t.is_resolved && !t.comments.is_empty())
                    .map(|t| ThreadRef {
                        id: t.id.clone(),
                        author: t.comments[0].author.clone(),
                        path: Some(t.path.clone()),
                        line: t.line,
                    })
            });
            if let Some(thread) = thread {
                let _ = ready.view.review.draft.add(
                    DraftKind::Reply,
                    None,
                    Some(thread),
                    "Agree with 30 seconds; the skew we saw on CI was under 10.",
                    now,
                );
            }
        }
        Scene::Finalize => {
            ui.modal = Some(Modal::Finalize);
            ui.finalize.verdict = Some(Verdict::RequestChanges);
            ui.finalize.summary =
                "Nice change overall. Two things on the token store before this goes in.".into();
        }
        Scene::WhatsNew => ui.modal = Some(Modal::WhatsNew),
        Scene::Leave => ui.modal = Some(Modal::Leave { window: false }),
    }
    if matches!(scene, Scene::Loading | Scene::Failed) {
        // The copy under a load is the cached one: read-only, nothing is sent from it.
        ready.cached_at = Some(now - 3600);
    }
    let phase = match scene {
        Scene::Loading => Phase::Loading {
            steps: steps(&pr),
            cached: Some(Box::new(ready)),
        },
        Scene::Failed => Phase::Failed {
            step: Some(LoadStepKind::Branch),
            message: "fatal: couldn't find remote ref refs/pull/123/head".into(),
            cached: Some(Box::new(ready)),
        },
        _ => Phase::Ready(Box::new(ready)),
    };
    world
        .resource_mut::<ReviewTabs>()
        .0
        .insert(pr.clone(), Tab { phase, ui });
    {
        let mut nav = world.resource_mut::<Nav>();
        nav.go(&WindowTarget::Review { pr: pr.clone() });
        if scene == Scene::Palette {
            nav.go(&WindowTarget::Home);
        }
    }
    if scene == Scene::AgentChat {
        // As in the mockup, only the question turn: the log is not asked for again.
        let question = fixture::demo_agent_turns(now).remove(1);
        let mut chats = world.resource_mut::<Chats>();
        let chat = chats.entry(&pr);
        chat.show(PanelTab::Agent);
        chat.log_asked = true;
        chats.replay(&pr, &question);
        chats.entry(&pr).state = SessionStateKind::Running;
    }
    if scene == Scene::Palette {
        *world.resource_mut::<Palette>() = Palette {
            open: true,
            query: "site".into(),
            cursor: 0,
        };
    }
    let popover = match scene {
        Scene::Emoji => Some(PopoverKind::Emoji),
        Scene::Gif => Some(PopoverKind::Gif),
        _ => None,
    };
    if let Some(kind) = popover {
        world.resource_mut::<Popovers>().open =
            Some((ComposerKey(pr, Slot::Edit(composer_target())), kind));
    }
}

#[cfg(test)]
mod tests {
    fn scroll_after_update(scene: Scene) -> f32 {
        let mut app = App::new();
        add_scroll(&mut app, scene);
        let area = app
            .world_mut()
            .spawn((ScrollArea, ScrollPosition::default()))
            .id();
        app.update();
        app.world().get::<ScrollPosition>(area).unwrap().y
    }

    #[test]
    fn only_the_bottom_scene_scrolls_to_the_end() {
        assert_eq!(
            scroll_after_update(Scene::ConfigNotificationsBottom),
            f32::MAX
        );
        for scene in [Scene::ConfigNotifications, Scene::ConfigGeneral] {
            assert_eq!(scroll_after_update(scene), 0.0, "{scene:?}");
        }
    }

    use super::*;
    use crate::nav::{FirstRunScreen, Screen};
    use crate::screens::open_pr::PaletteRoot;
    use crate::screens::review::comments::CommentsRegion;
    use crate::screens::review::diff::DiffRegion;
    use crate::screens::review::finalize::FinalizeModal;
    use crate::screens::review::leave::LeaveModal;
    use crate::screens::review::shell::ModalFor;
    use crate::testing::{self, NOW};
    use clap::ValueEnum;

    fn staged(scene: Scene) -> App {
        let mut app = testing::app(fixture::demo(NOW));
        stage(app.world_mut(), scene);
        stage_snapshot(app.world_mut(), scene);
        testing::settle(&mut app);
        app
    }

    fn modal_shown(app: &mut App, modal: Modal) -> bool {
        let mut q = app.world_mut().query::<&ModalFor>();
        q.iter(app.world()).any(|m| m.modal == modal)
    }

    #[test]
    fn every_scene_stages_the_demo_review() {
        let pr = fixture::demo_pr();
        for &scene in Scene::value_variants() {
            if matches!(
                scene,
                Scene::FirstRun
                    | Scene::ConfigGeneral
                    | Scene::ConfigNotifications
                    | Scene::ConfigNotificationsBottom
                    | Scene::ConfigMedia
                    | Scene::ConfigAbout
                    | Scene::ConfigHarness
            ) {
                continue;
            }
            let mut app = staged(scene);
            let tab = testing::tab(&app, &pr);
            let screen = app.world().resource::<Nav>().screen.clone();
            match scene {
                Scene::Palette => {
                    assert_eq!(screen, Screen::Home);
                    assert_eq!(testing::count::<PaletteRoot>(&mut app), 1);
                }
                _ => assert_eq!(screen, Screen::Review(pr.clone()), "{scene:?}"),
            }
            match scene {
                Scene::Diff | Scene::Palette => {
                    assert_eq!(tab.ui.mode, DiffMode::Unified);
                    if scene == Scene::Diff {
                        assert_eq!(testing::count::<DiffRegion>(&mut app), 1);
                    }
                }
                Scene::Split => {
                    assert_eq!(tab.ui.mode, DiffMode::Split);
                    assert_eq!(testing::count::<DiffRegion>(&mut app), 1);
                }
                Scene::Comments => {
                    assert_eq!(testing::count::<CommentsRegion>(&mut app), 1);
                    let Phase::Ready(r) = &tab.phase else {
                        panic!("ready")
                    };
                    assert!(
                        r.view
                            .review
                            .draft
                            .items
                            .iter()
                            .any(|i| i.kind == DraftKind::Reply),
                        "the mockup's draft reply"
                    );
                }
                Scene::Finalize => {
                    assert_eq!(testing::count::<FinalizeModal>(&mut app), 1);
                    assert_eq!(tab.ui.finalize.verdict, Some(Verdict::RequestChanges));
                }
                Scene::WhatsNew => assert!(modal_shown(&mut app, Modal::WhatsNew)),
                Scene::Leave => assert_eq!(testing::count::<LeaveModal>(&mut app), 1),
                Scene::FirstRun
                | Scene::ConfigGeneral
                | Scene::ConfigNotifications
                | Scene::ConfigNotificationsBottom
                | Scene::ConfigMedia
                | Scene::ConfigAbout
                | Scene::ConfigHarness => {
                    unreachable!("skipped above")
                }
                Scene::AgentChat => {
                    assert_eq!(tab.ui.file.as_deref(), Some("src/auth/refresh.rs"));
                    let chats = app
                        .world()
                        .resource::<crate::screens::review::agent::Chats>();
                    let chat = &chats.0[&pr];
                    assert!(
                        chat.tab == crate::screens::review::agent::PanelTab::Agent
                            && chat.log_asked
                    );
                    assert_eq!(chat.state, clusia_protocol::SessionStateKind::Running);
                }
                Scene::Composer | Scene::ComposerPreview | Scene::Emoji | Scene::Gif => {
                    let editor = tab.ui.editor.as_ref().expect("an open composer");
                    assert!(matches!(editor.target, EditTarget::Line { line: 44, .. }));
                }
                Scene::Rendered => assert_eq!(tab.ui.section, ReviewSection::Comments),
                Scene::Loading => {
                    let Phase::Loading { steps, cached } = &tab.phase else {
                        panic!("loading")
                    };
                    assert_eq!(steps.len(), 3);
                    assert!(cached.as_ref().is_some_and(|c| c.cached_at.is_some()));
                }
                Scene::Failed => {
                    let Phase::Failed {
                        step,
                        message,
                        cached,
                    } = &tab.phase
                    else {
                        panic!("failed")
                    };
                    assert!(cached.as_ref().is_some_and(|c| c.cached_at.is_some()));
                    assert_eq!(*step, Some(LoadStepKind::Branch));
                    assert!(message.starts_with("fatal: couldn't find remote ref"));
                }
            }
        }
    }

    fn staged_outside_review(scene: Scene) -> App {
        let mut app = testing::app(fixture::demo(NOW));
        stage(app.world_mut(), scene);
        stage_snapshot(app.world_mut(), scene);
        testing::settle(&mut app);
        app
    }

    #[test]
    fn first_run_scene_signs_the_demo_out() {
        let mut app = staged_outside_review(Scene::FirstRun);
        assert_eq!(app.world().resource::<Nav>().screen, Screen::FirstRun);
        assert_eq!(testing::count::<FirstRunScreen>(&mut app), 1);
        testing::find::<crate::screens::first_run::UseGh>(&mut app, |_| true);
        let v = crate::screens::first_run::view(
            &app.world().resource::<Model>().snapshot,
            &crate::screens::first_run::FirstRunUi::default(),
            None,
        );
        assert_eq!(v.folders.map(|f| f.len()), Some(4));
        assert!(v.harnesses[0].found && !v.harnesses[1].found);
    }

    #[test]
    fn config_scenes_open_their_sections() {
        for (scene, section) in [
            (Scene::ConfigGeneral, Section::General),
            (Scene::ConfigNotifications, Section::Notifications),
            (Scene::ConfigNotificationsBottom, Section::Notifications),
            (Scene::ConfigMedia, Section::Media),
            (Scene::ConfigAbout, Section::About),
            (Scene::ConfigHarness, Section::Harness),
        ] {
            let mut app = staged_outside_review(scene);
            assert_eq!(
                app.world().resource::<Nav>().screen,
                Screen::Config(section)
            );
            testing::find::<crate::screens::config::PageOf>(&mut app, |p| p.0 == section);
        }
    }

    fn demo_config(scene: Scene) -> serde_json::Value {
        let app = staged_outside_review(scene);
        let config = &app.world().resource::<Model>().snapshot.config;
        serde_json::to_value(config).unwrap()
    }

    #[test]
    fn the_agent_chat_scene_shows_the_mockup_chat() {
        let mut app = staged(Scene::AgentChat);
        for needle in [
            "Is the new lock needed at all, or would re-checking the expiry be enough?",
            "Read src/auth/store.rs and client/http.rs",
            "Searched for refresh_lock · 3 uses",
            "The lock is needed",
            "What's not needed is holding it during the network call",
            "Suggested comment",
            "refresh.rs:44",
            "Accept into draft",
            "Stop",
            "Claude Code · thinking…",
            "session resumed · knows this review",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        assert!(
            !testing::shows(&mut app, "Summary"),
            "only the question turn, as in the mockup"
        );
        let region =
            testing::find::<crate::screens::review::agent::AgentRegion>(&mut app, |_| true);
        assert_eq!(
            app.world().get::<Node>(region).unwrap().display,
            Display::Flex
        );
    }

    #[test]
    fn the_harness_scene_shows_the_mockup_settings() {
        let config = demo_config(Scene::ConfigHarness);
        assert_eq!(config["harness"]["program"], "/opt/homebrew/bin/claude");
        assert_eq!(config["harness"]["extra_args"], "--model claude-opus-5-5");
        let mut app = staged_outside_review(Scene::ConfigHarness);
        assert!(testing::shows(
            &mut app,
            "Claude Code 2.1.294 answered in 1.8 s"
        ));
        assert!(testing::shows(
            &mut app,
            "Found at /opt/homebrew/bin/claude"
        ));
        assert!(testing::shows(&mut app, "Test again"));
    }

    #[test]
    fn the_notifications_scene_shows_the_mockup_settings() {
        let config = demo_config(Scene::ConfigNotifications);
        let notifications = &config["notifications"];
        assert_eq!(
            notifications["dnd"],
            serde_json::json!({
                "enabled": true,
                "from": "19:00",
                "to": "09:00",
                "days": ["mon", "tue", "wed", "thu", "fri"]
            })
        );
        assert_eq!(notifications["follow_focus"], true);
        assert_eq!(notifications["group_bursts"], true);
    }

    #[test]
    fn the_bottom_scene_has_the_notification_settings_too() {
        assert_eq!(
            demo_config(Scene::ConfigNotificationsBottom),
            demo_config(Scene::ConfigNotifications)
        );
    }

    #[test]
    fn the_general_scene_starts_at_login() {
        assert_eq!(
            demo_config(Scene::ConfigGeneral)["general"]["start_at_login"],
            true
        );
    }

    #[test]
    fn the_other_config_scenes_keep_the_default_settings() {
        let default = serde_json::to_value(clusia_core::Config::default()).unwrap();
        assert_eq!(demo_config(Scene::ConfigMedia), default);
        assert_eq!(demo_config(Scene::ConfigAbout), default);
    }

    #[test]
    fn composer_scenes_type_the_mockup_comment() {
        for (scene, mode) in [
            (Scene::Composer, ComposerMode::Write),
            (Scene::ComposerPreview, ComposerMode::Preview),
            (Scene::Emoji, ComposerMode::Write),
            (Scene::Gif, ComposerMode::Write),
        ] {
            let app = staged(scene);
            let tab = testing::tab(&app, &fixture::demo_pr());
            let Editor {
                text, mode: shown, ..
            } = tab.ui.editor.expect("an open composer");
            assert_eq!(shown, mode, "{scene:?}");
            assert!(text.contains("**Holding the lock across the network call**"));
            assert!(text.contains(":turtle:") && text.contains("`expires_at`"));
            assert!(
                text.contains("![waiting](https://media.giphy.com/media/demo-party/giphy.gif)")
            );
        }
    }

    #[test]
    fn popover_scenes_open_their_popover() {
        let open = |scene| staged(scene).world().resource::<Popovers>().open.clone();
        assert!(matches!(
            open(Scene::Emoji),
            Some((_, crate::ui::composer::popover::PopoverKind::Emoji))
        ));
        assert!(matches!(
            open(Scene::Gif),
            Some((_, crate::ui::composer::popover::PopoverKind::Gif))
        ));
        assert!(open(Scene::Composer).is_none());
        assert!(open(Scene::ComposerPreview).is_none());
    }

    #[test]
    fn the_rendered_scene_has_no_draft_reply() {
        let app = staged(Scene::Rendered);
        let ready = testing::ready(&app, &fixture::demo_pr());
        assert!(
            ready
                .view
                .review
                .draft
                .items
                .iter()
                .all(|i| i.kind != DraftKind::Reply)
        );
    }
}
