//! `--demo --scene <name>`: stages the demo review in one state, so every review screen can be
//! rendered (screenshots) without a daemon.

use bevy::prelude::*;
use bevy::ui_widgets::ScrollArea;
use clusia_core::checks::{AuditArea, default_areas};
use clusia_core::draft::{DraftKind, ThreadRef};
use clusia_core::{Anchor, Origin, Side, Verdict};
use clusia_protocol::{
    AuthInfo, LoadStep, LoadStepKind, SessionStateKind, StepStatus, SyncState, SyncStatus,
    WindowTarget,
};

use crate::args::Scene;
use crate::bridge::{Model, Outbox, ProbeState};
use crate::clock::Clock;
use crate::fixture;
use crate::nav::{Nav, Section};
use crate::review_state::{
    DiffMode, EditTarget, Editor, Modal, Phase, Ready, ReviewSection, ReviewTabs, Tab, TabUi,
};
use crate::screens::config::areas::areas_value;
use crate::screens::open_pr::Palette;
use crate::screens::review::agent::{Chats, PanelTab, PermissionQueue};
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
        step(
            LoadStepKind::Pr,
            StepStatus::Done,
            Some("7 files, 5 comments and the checks"),
        ),
        step(
            LoadStepKind::Agent,
            StepStatus::Done,
            Some("Summarizing · Checking security · Auditing (6 areas)"),
        ),
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

/// The six built-in audit areas and one custom area, as the value of `harness.audit_areas`.
fn demo_areas() -> String {
    let mut areas = default_areas();
    areas.push(AuditArea {
        id: "migrations".into(),
        name: "Migrations".into(),
        instruction: "Check that every migration can be undone and that none locks a large table."
            .into(),
        enabled: true,
        builtin: false,
    });
    areas_value(&areas)
}

/// The finding the Security mockup already has in the draft: the demo's MEDIUM finding, as
/// accepting it would add it.
fn accept_demo_finding(ready: &mut Ready, now: i64) {
    let finding = fixture::demo_checks(now).0[0].findings[1].clone();
    let anchor = Anchor {
        commit: ready.view.review.head_sha.clone(),
        path: finding.file.clone(),
        line: finding.line.expect("the demo finding has a line"),
        start_line: None,
        side: Side::Right,
    };
    let _ = ready.view.review.draft.add_as(
        Origin::Security,
        DraftKind::LineComment,
        Some(anchor),
        None,
        &finding.comment,
        now,
    );
}

/// Signs the demo out and gives the first-run screen its answers.
fn stage_snapshot(world: &mut World, scene: Scene) {
    match scene {
        Scene::ConfigGeneral => set_demo_config(world, &[("general.start_at_login", "true")]),
        Scene::ConfigNotifications | Scene::ConfigNotificationsBottom => {
            set_demo_config(world, &NOTIFICATION_SETTINGS);
        }
        Scene::ConfigHarness => {
            let areas = demo_areas();
            set_demo_config(
                world,
                &[
                    ("harness.program", "/opt/homebrew/bin/claude"),
                    ("harness.extra_args", "--model claude-opus-5-5"),
                    ("harness.audit_areas", &areas),
                ],
            );
            world.resource_mut::<Model>().probe = ProbeState::Done(fixture::demo_probe());
        }
        Scene::AgentChat | Scene::Permission | Scene::Security | Scene::Audits => {
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
        Scene::AgentChat | Scene::Permission => ui.file = Some("src/auth/refresh.rs".into()),
        Scene::Security => {
            ui.section = ReviewSection::Security;
            accept_demo_finding(&mut ready, now);
        }
        Scene::Audits => {
            ui.section = ReviewSection::Audits;
            ui.audit_area = Some("concurrency".into());
            accept_demo_finding(&mut ready, now);
        }
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
    let checks_tell = matches!(scene, Scene::Security | Scene::Audits)
        .then(|| fixture::demo_checks_tell(&pr, now, &ready.view.review.draft));
    let phase = match scene {
        Scene::Loading => Phase::Loading { steps: steps(&pr) },
        Scene::Failed => Phase::Failed {
            step: Some(LoadStepKind::Branch),
            message: "fatal: couldn't find remote ref refs/pull/123/head".into(),
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
    if matches!(scene, Scene::AgentChat | Scene::Permission) {
        // As in the mockup, only the question turn: the log is not asked for again.
        let question = fixture::demo_agent_turns(now).remove(1);
        let mut chats = world.resource_mut::<Chats>();
        let chat = chats.entry(&pr);
        chat.show(PanelTab::Agent);
        chat.log_asked = true;
        chats.replay(&pr, &question);
        chats.entry(&pr).state = SessionStateKind::Running;
    }
    if let Some(tell) = checks_tell {
        world.resource_mut::<Chats>().entry(&pr).state = SessionStateKind::Running;
        // The same way a daemon's answer arrives: `pump` fills the checks of the review.
        let _ = world.resource::<Outbox>().0.send(tell);
    }
    if scene == Scene::Permission {
        // A frozen clock keeps the countdown at 1:52 for as long as the render takes.
        *world.resource_mut::<Clock>() = Clock(Some(now));
        // Asked 8 s ago of a 2-minute wait, so the bar is a little short of full, as the
        // mockup's is.
        let now_ms = now * 1000;
        world
            .resource_mut::<PermissionQueue>()
            .push(fixture::demo_permission_request(now_ms), now_ms - 8_000);
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
                Scene::Permission => {
                    assert_eq!(tab.ui.file.as_deref(), Some("src/auth/refresh.rs"));
                    assert_eq!(
                        testing::count::<crate::screens::review::agent::PermissionModal>(&mut app),
                        1
                    );
                }
                Scene::Security => assert_eq!(tab.ui.section, ReviewSection::Security),
                Scene::Audits => {
                    assert_eq!(tab.ui.section, ReviewSection::Audits);
                    assert_eq!(tab.ui.audit_area.as_deref(), Some("concurrency"));
                }
                Scene::Composer | Scene::ComposerPreview | Scene::Emoji | Scene::Gif => {
                    let editor = tab.ui.editor.as_ref().expect("an open composer");
                    assert!(matches!(editor.target, EditTarget::Line { line: 44, .. }));
                }
                Scene::Rendered => assert_eq!(tab.ui.section, ReviewSection::Comments),
                Scene::Loading => {
                    let Phase::Loading { steps } = &tab.phase else {
                        panic!("loading")
                    };
                    assert_eq!(steps.len(), 4);
                    let agent = steps.last().unwrap();
                    assert_eq!(
                        (agent.step, agent.status),
                        (LoadStepKind::Agent, StepStatus::Done)
                    );
                    assert_eq!(
                        agent.message.as_deref(),
                        Some("Summarizing · Checking security · Auditing (6 areas)")
                    );
                }
                Scene::Failed => {
                    let Phase::Failed { step, message } = &tab.phase else {
                        panic!("failed")
                    };
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
    fn the_permission_scene_shows_the_mockup_modal() {
        let mut app = staged(Scene::Permission);
        for needle in [
            "Claude Code wants to run a command",
            "“To check that two refreshes at once don't exchange the token twice.”",
            "cargo test -p clusia-auth refresh_race -- --nocapture",
            "no network · reads and writes only the worktree",
            "Denied automatically in 1:52",
            "Deny",
            "Allow cargo test … for this review",
            "Allow once",
            "Is the new lock needed at all, or would re-checking the expiry be enough?",
            "Claude Code · thinking…",
            "session resumed · knows this review",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        assert!(
            !testing::shows(&mut app, "Summary"),
            "only the question turn, as in the mockup"
        );
        let clock = *app.world().resource::<crate::clock::Clock>();
        assert_eq!(
            clock,
            crate::clock::Clock(Some(NOW)),
            "the countdown must not drift in a render"
        );
        let queue = app.world().resource::<PermissionQueue>();
        let pending = &queue.0[0];
        let left = crate::screens::review::agent::permission::countdown_fraction(
            pending.request.deadline,
            pending.asked_ms,
            NOW * 1000,
        );
        assert!(
            (left - 112.0 / 120.0).abs() < 0.001,
            "the bar shows time already passed, not a full bar: {left}"
        );
    }

    #[test]
    fn the_harness_scene_shows_the_mockup_settings() {
        let config = demo_config(Scene::ConfigHarness);
        assert_eq!(config["harness"]["program"], "/opt/homebrew/bin/claude");
        assert_eq!(config["harness"]["extra_args"], "--model claude-opus-5-5");
        assert_eq!(config["harness"]["sandbox"], true);
        assert_eq!(config["harness"]["permission_timeout_secs"], 120);
        let mut app = staged_outside_review(Scene::ConfigHarness);
        assert!(testing::shows(&mut app, "Deny unanswered requests after"));
        assert!(testing::shows(
            &mut app,
            "Run commands in a sandbox: no network, writes only in the review worktree"
        ));
        assert!(testing::shows(
            &mut app,
            "Claude Code 2.1.294 answered in 1.8 s"
        ));
        assert!(testing::shows(
            &mut app,
            "Found at /opt/homebrew/bin/claude"
        ));
        assert!(testing::shows(&mut app, "Test again"));
        let areas = config["harness"]["audit_areas"].as_array().unwrap();
        assert_eq!(areas.len(), 7, "the six built-in areas and a custom one");
        assert_eq!(areas[6]["id"], "migrations");
        assert_eq!(areas[6]["builtin"], false);
        for needle in [
            "When I open a review",
            "Check security",
            "Audit areas",
            "Docs and changelog",
            "Migrations",
            "+ Add area",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
    }

    #[test]
    fn the_security_scene_shows_the_mockup_findings() {
        let mut app = staged(Scene::Security);
        for needle in [
            "2 security findings",
            "Claude Code checked the 7 changed files 2 minutes ago. Accepted findings become draft comments.",
            "1 high",
            "1 low",
            "Proposed comment",
            "Show in diff",
            "Refresh token written to the debug log",
            "No limit on refresh retries",
            "Token file created with default permissions",
            "In your draft",
            "Check again",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        let draft = testing::ready(&app, &fixture::demo_pr()).view.review.draft;
        assert_eq!(
            draft.items.len(),
            4,
            "the demo draft plus the accepted finding"
        );
        assert!(
            !testing::shows(&mut app, "1 medium"),
            "an accepted finding is not counted as open"
        );
        let last = draft.items.last().unwrap();
        assert_eq!(last.origin, clusia_core::Origin::Security);
        assert_eq!(
            last.anchor.as_ref().map(|a| (a.path.as_str(), a.line)),
            Some(("src/client/http.rs", 24))
        );
        let chats = app
            .world()
            .resource::<crate::screens::review::agent::Chats>();
        assert_eq!(
            chats.0[&fixture::demo_pr()].state,
            clusia_protocol::SessionStateKind::Running
        );
    }

    #[test]
    fn the_audits_scene_shows_the_mockup_area() {
        let mut app = staged(Scene::Audits);
        for needle in [
            "Correctness",
            "Concurrency",
            "Error handling",
            "Performance",
            "Tests",
            "Docs and changelog",
            "The refresh lock is held across a network call",
            "Proposed comment",
            "Accept into draft",
            "Edit first",
            "Dismiss",
            "No shared state written outside the lock",
            "The new test covers two tasks refreshing at once",
            "Ask the agent about this",
            "Needs your OK",
            "Run by Claude Code with your audit areas. Findings the agent proposes wait for your OK before they join the draft.",
            "3 waiting for your OK",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        assert!(
            testing::shown_text(&mut app)
                .iter()
                .any(|t| t.to_uppercase().contains("AUDIT · 6 AREAS")),
            "the list says how many areas it audits"
        );
    }

    #[test]
    fn the_loading_scene_names_the_agent_work() {
        let mut app = staged(Scene::Loading);
        assert!(testing::shows(
            &mut app,
            "Summarizing · Checking security · Auditing (6 areas)"
        ));
    }

    #[test]
    fn the_first_run_scene_shows_step_three() {
        let mut app = staged_outside_review(Scene::FirstRun);
        for needle in [
            "Connect your AI harness",
            "Found · /opt/homebrew/bin/claude",
            "Coming soon",
            "When I open a review",
            "Check security",
            "Audit the change",
            "Skip for now",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        assert!(!testing::shows(&mut app, "Connecting a harness arrives"));
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
