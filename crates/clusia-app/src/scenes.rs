//! `--demo --scene <name>`: stages the demo review in one state, so every review screen can be
//! rendered (screenshots) without a daemon.

use bevy::prelude::*;
use bevy::ui_widgets::ScrollArea;
use clusia_core::checks::{AuditArea, default_areas};
use clusia_core::diagram::{Change, DiagramKind, DiagramVersion, EntryState, VersionPhase};
use clusia_core::draft::{DraftKind, ThreadRef};
use clusia_core::hidden::{issue_comment_key, thread_key};
use clusia_core::keywords::KeywordMap;
use clusia_core::{Anchor, Origin, ReviewState, Side, Verdict};
use clusia_diagram::keywords::{Point3, PointShape, Scene3D};
use clusia_protocol::{
    AuthInfo, DiagramsState, LoadStep, LoadStepKind, SessionStateKind, StepStatus, SyncState,
    SyncStatus, WindowTarget,
};

use crate::args::Scene;
use crate::bridge::{DiagramsTell, Model, Outbox, ProbeState, RevalidationTell, Tell};
use crate::clock::Clock;
use crate::fixture;
use crate::nav::{Nav, Section};
use crate::review_state::{
    CameraPose, DiffMode, EditTarget, Editor, KeywordsUi, Modal, Phase, Ready, RevalView,
    ReviewSection, ReviewTabs, Tab, TabUi, View3D,
};
use crate::screens::config::areas::areas_value;
use crate::screens::open_pr::Palette;
use crate::screens::review::agent::{Chats, PanelTab, PermissionQueue};
use crate::screens::review::diagrams::Diagrams;
use crate::screens::review::diagrams::keywords::render::{focus_pose, view_pose};
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

/// The tabs a window left open: the demo review shown in Comments, two more beside it.
pub fn restored_tabs() -> clusia_core::OpenTabs {
    use clusia_core::OpenTabs;
    OpenTabs {
        tabs: vec![
            OpenTabs::tab("rzorzal/clusia#98".parse().expect("valid ref"), "diff"),
            OpenTabs::tab(fixture::demo_pr(), "comments"),
            OpenTabs::tab("acme/widgets#5".parse().expect("valid ref"), "security"),
        ],
        selected: Some(fixture::demo_pr()),
    }
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
pub(crate) fn stage_snapshot(world: &mut World, scene: Scene) {
    match scene {
        Scene::ConfigGeneral => set_demo_config(world, &[("general.start_at_login", "true")]),
        Scene::ConfigNotifications | Scene::ConfigNotificationsBottom => {
            set_demo_config(world, &NOTIFICATION_SETTINGS);
        }
        Scene::ConfigHarness | Scene::ConfigFocus => {
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
        Scene::ConfigHarnessCodex => {
            set_demo_config(world, &fixture::CODEX_DEMO_SETTINGS);
            let areas = demo_areas();
            set_demo_config(world, &[("harness.audit_areas", &areas)]);
            world.resource_mut::<Model>().probe = ProbeState::Done(fixture::demo_codex_probe());
        }
        Scene::AgentChatCodex | Scene::PermissionCodex | Scene::HomeFooter => {
            set_demo_config(world, &fixture::CODEX_DEMO_SETTINGS);
        }
        Scene::ConfigHarnessCustom => {
            set_demo_config(world, &fixture::CUSTOM_DEMO_SETTINGS);
            let areas = demo_areas();
            set_demo_config(world, &[("harness.audit_areas", &areas)]);
            world.resource_mut::<Model>().probe = ProbeState::Done(fixture::demo_custom_probe());
        }
        Scene::AgentChatCustom | Scene::PermissionOther | Scene::FirstRunCustom => {
            set_demo_config(world, &fixture::CUSTOM_DEMO_SETTINGS);
        }
        Scene::AgentChat
        | Scene::Permission
        | Scene::Security
        | Scene::Audits
        | Scene::AuditsAll
        | Scene::Revalidated
        | Scene::RevalidatedBanner => {
            set_demo_config(world, &[("harness.program", "/opt/homebrew/bin/claude")])
        }
        Scene::DiagramsNoAutoDraw => {
            set_demo_config(world, &[("harness.program", "/opt/homebrew/bin/claude")]);
        }
        scene if scene.is_diagram() => {
            set_demo_config(world, &[("harness.program", "/opt/homebrew/bin/claude")]);
        }
        _ => {}
    }
    if !matches!(
        scene,
        Scene::FirstRun | Scene::FirstRunBoth | Scene::FirstRunCustom
    ) {
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
    model.snapshot.first_run = Some(if scene == Scene::FirstRunBoth {
        fixture::demo_first_run_both()
    } else {
        fixture::demo_first_run()
    });
}

/// The key of the first drawing of `kind`.
fn key_of(version: &DiagramVersion, kind: DiagramKind) -> Option<String> {
    version
        .entries
        .iter()
        .find(|e| e.plan.kind == kind)
        .map(|e| e.plan.key.clone())
}

/// The box of the drawing `key` the dialog opens on: the first changed one that stands for code,
/// as in the mockup, else the first that stands for code.
fn anchored_node(version: &DiagramVersion, key: &str) -> Option<String> {
    match &version.entry(key)?.state {
        EntryState::Ready { spec, .. } => {
            let anchored = || spec.nodes.iter().filter(|n| n.anchor.is_some());
            anchored()
                .find(|n| n.change == Change::Changed)
                .or_else(|| anchored().next())
                .map(|n| n.id.clone())
        }
        _ => None,
    }
}

/// A drawing under way: the first five entries of `latest` as version 1, two ready, one being
/// drawn and two waiting.
fn drawing_version(latest: &DiagramVersion) -> (DiagramVersion, DiagramsState) {
    let mut version = latest.clone();
    version.number = 1;
    version.phase = VersionPhase::Drawing;
    version.entries.truncate(5);
    for (i, entry) in version.entries.iter_mut().enumerate() {
        match i {
            0 | 1 => {}
            2 => entry.state = EntryState::Drawing,
            _ => entry.state = EntryState::Waiting,
        }
    }
    let key = version
        .entries
        .get(2)
        .map(|e| e.plan.key.clone())
        .unwrap_or_default();
    let state = DiagramsState::Drawing {
        done: 2,
        total: 5,
        key,
        step: None,
    };
    (version, state)
}

/// What a diagram scene shows: the drawings the demo daemon would send, and the tab's choice
/// among them.
fn stage_diagrams(ui: &mut TabUi, scene: Scene, now: i64) -> (Vec<DiagramVersion>, DiagramsState) {
    let versions = fixture::demo_diagrams(now, &fixture::demo_pr());
    let latest = versions.last().expect("the demo has drawings").clone();
    ui.section = ReviewSection::Diagrams;
    if scene == Scene::DiagramsDrawing {
        let (drawing, state) = drawing_version(&latest);
        ui.diagram = drawing.entries.first().map(|e| e.plan.key.clone());
        return (vec![drawing], state);
    }
    let kind = scene.diagram_kind().unwrap_or(DiagramKind::Flow);
    ui.diagram = key_of(&latest, kind);
    ui.diagram_compare = scene == Scene::DiagramsCompare;
    if scene.is_keywords()
        && let Some(EntryState::ReadyMap { map, .. }) = latest
            .entries
            .iter()
            .find(|e| e.plan.kind == DiagramKind::Keywords)
            .map(|e| &e.state)
    {
        ui.keywords = keywords_staged(scene, map);
    }
    if scene == Scene::DiagramNode
        && let Some(key) = ui.diagram.clone()
        && let Some(node) = anchored_node(&latest, &key)
    {
        ui.modal = Some(Modal::DiagramNode);
        ui.diagram_node = Some((latest.number, key, node));
    }
    (versions, DiagramsState::Idle)
}

/// What each keyword scene shows of the 3D view. It is written straight into the tab, camera
/// included, so the first frame is the final one.
fn keywords_staged(scene: Scene, map: &KeywordMap) -> KeywordsUi {
    let scene3d = clusia_diagram::keywords::scene(map);
    let mut ui = KeywordsUi::default();
    match scene {
        Scene::DiagramsKeywordsUsecase => {
            ui.use_case = Some(0);
            ui.camera = focus_pose(&scene3d, 0);
        }
        Scene::KeywordsCard => ui.card = card_point(map, &scene3d),
        Scene::KeywordsXy => {
            ui.view = View3D::XY;
            ui.camera = view_pose(View3D::XY, &CameraPose::default());
        }
        _ => {}
    }
    ui
}

/// The changed declaration of `refresh` in `src/token/store.rs` (the card of the mockup),
/// else the first changed point.
fn card_point(map: &KeywordMap, scene: &Scene3D) -> Option<u32> {
    let place_of = |p: &Point3| &map.places[p.places[0] as usize];
    scene
        .points
        .iter()
        .position(|p| {
            let place = place_of(p);
            p.changed
                && p.shape == PointShape::Declared
                && map.words[place.word as usize].text == "refresh"
                && map.paths[place.path as usize] == "src/token/store.rs"
        })
        .or_else(|| scene.points.iter().position(|p| p.changed))
        .map(|i| i as u32)
}

/// Fills the Diagrams model of `pr` with the daemon's answer to `GetDiagrams`, and the older
/// versions as answers to `GetDiagramVersion`, so the tab does not ask (and the demo daemon
/// does not replace a drawing that is under way).
fn put_diagrams(
    world: &mut World,
    pr: &clusia_core::PrRef,
    mut versions: Vec<DiagramVersion>,
    state: DiagramsState,
) {
    let latest = versions.pop();
    let loaded = DiagramsTell::Loaded {
        pr: pr.clone(),
        versions: versions
            .iter()
            .chain(latest.iter())
            .map(DiagramVersion::info)
            .collect(),
        head: latest
            .as_ref()
            .map(|v| v.commit.clone())
            .unwrap_or_default(),
        latest: latest.map(Box::new),
        state,
        stale: false,
        stale_reason: None,
    };
    let mut diagrams = world.resource_mut::<Diagrams>();
    let model = diagrams.0.entry(pr.clone()).or_default();
    model.apply(&loaded);
    for version in versions {
        model.apply(&DiagramsTell::Version {
            pr: pr.clone(),
            version: Box::new(version),
        });
    }
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
        Scene::ConfigAppearance => Some(Section::Appearance),
        Scene::ConfigHarness
        | Scene::ConfigHarnessCodex
        | Scene::ConfigHarnessCustom
        | Scene::ConfigFocus => Some(Section::Harness),
        _ => None,
    };
    if let Some(section) = section {
        let mut nav = world.resource_mut::<Nav>();
        if scene == Scene::ConfigFocus {
            nav.open_setting(section, "harness.diagrams");
        } else {
            nav.open_section(section);
        }
        return;
    }
    if matches!(
        scene,
        Scene::FirstRun | Scene::FirstRunBoth | Scene::FirstRunCustom | Scene::HomeFooter
    ) {
        return;
    }
    if scene == Scene::Restored {
        // The window opens again: the daemon hands back the tabs it had, as `RestoreTabs` would.
        let _ = world
            .resource::<Outbox>()
            .0
            .send(Tell::Restored(restored_tabs()));
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
    let mut round = None;
    let mut ui = TabUi::default();
    match scene {
        Scene::Diff | Scene::Loading | Scene::Failed | Scene::Palette => {}
        Scene::FirstRun
        | Scene::FirstRunBoth
        | Scene::FirstRunCustom
        | Scene::HomeFooter
        | Scene::ConfigGeneral
        | Scene::ConfigNotifications
        | Scene::ConfigNotificationsBottom
        | Scene::ConfigMedia
        | Scene::ConfigAbout
        | Scene::ConfigAppearance
        | Scene::ConfigHarness
        | Scene::ConfigHarnessCodex
        | Scene::ConfigHarnessCustom
        | Scene::ConfigFocus
        | Scene::Restored => {}
        Scene::AgentChat
        | Scene::AgentChatCodex
        | Scene::AgentChatCustom
        | Scene::Permission
        | Scene::PermissionCodex
        | Scene::PermissionOther => ui.file = Some("src/auth/refresh.rs".into()),
        Scene::Security => {
            ui.section = ReviewSection::Security;
            accept_demo_finding(&mut ready, now);
        }
        Scene::Audits => {
            ui.section = ReviewSection::Audits;
            ui.audit_area = Some("concurrency".into());
            accept_demo_finding(&mut ready, now);
        }
        Scene::AuditsAll => ui.section = ReviewSection::Audits,
        Scene::Revalidated | Scene::RevalidatedBanner => {
            let (draft, proposals) = fixture::demo_revalidation(now);
            ready.view.review.draft = draft;
            ready.view.review.state = ReviewState::Revalidated;
            // The daemon's answer to `GetRevalidation` reads the review, so both agree.
            ready.view.review.revalidation = Some(proposals.clone());
            round = Some(proposals);
            if scene == Scene::RevalidatedBanner {
                ui.revalidation = RevalView::Closed;
            }
        }
        Scene::Composer | Scene::Emoji | Scene::Gif => {
            ui.editor = Some(composer_editor(ComposerMode::Write));
        }
        Scene::ComposerPreview => ui.editor = Some(composer_editor(ComposerMode::Preview)),
        Scene::Rendered => ui.section = ReviewSection::Comments,
        Scene::Split => ui.mode = DiffMode::Split,
        Scene::CommentsHidden => {
            ui.section = ReviewSection::Comments;
            ui.filter = crate::review_state::CommentsFilter::All;
            // One comment collapsed, and a thread that has grown since it was hidden.
            ui.hidden.hide(&issue_comment_key(2002), 1);
            ui.hidden.hide(&thread_key("PRRT_demo_refresh_41"), 2);
            ui.hidden.hide(&thread_key("PRRT_demo_mod_3"), 2);
            ui.hidden_asked = true;
        }
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
            // Enough comments to run past the window, one on a long path with a code block.
            let template = ready
                .view
                .review
                .draft
                .items
                .first()
                .and_then(|i| i.anchor.clone());
            if let Some(template) = template {
                let notes = [
                    (
                        "src/app/fixtures/live-fixtures.service.ts",
                        142,
                        "Could this be a plain function?\n\n```ts\nexport function load(id: string) {\n  const cached = cache.get(id);\n  if (cached) {\n    return cached;\n  }\n  const fresh = fetchFixture(id);\n  cache.set(id, fresh);\n  return fresh;\n}\n```",
                    ),
                    ("src/auth/session.rs", 12, "Name the timeout as a constant."),
                    (
                        "src/auth/session.rs",
                        57,
                        "This clones the whole token; a reference is enough.",
                    ),
                    (
                        "src/client/retry.rs",
                        9,
                        "Add jitter, or every client retries at the same second.",
                    ),
                    (
                        "src/client/retry.rs",
                        31,
                        "A test for the last attempt would help.",
                    ),
                    ("docs/auth.md", 4, "Mention the 30 second window here too."),
                ];
                for (path, line, body) in notes {
                    let mut anchor = template.clone();
                    anchor.path = path.into();
                    anchor.line = line;
                    let _ = ready.view.review.draft.add(
                        DraftKind::LineComment,
                        Some(anchor),
                        None,
                        body,
                        now,
                    );
                }
            }
        }
        Scene::WhatsNew => ui.modal = Some(Modal::WhatsNew),
        Scene::Leave => ui.modal = Some(Modal::Leave),
        Scene::Diagrams
        | Scene::DiagramsDeps
        | Scene::DiagramsSequence
        | Scene::DiagramsEr
        | Scene::DiagramsUsecase
        | Scene::DiagramsDataflow
        | Scene::DiagramsStates
        | Scene::DiagramsClasses
        | Scene::DiagramNode
        | Scene::DiagramsDrawing
        | Scene::DiagramsCompare
        | Scene::DiagramsKeywords
        | Scene::DiagramsKeywordsUsecase
        | Scene::KeywordsCard
        | Scene::KeywordsXy => {}
        Scene::DiagramsNoHarness | Scene::DiagramsNoAutoDraw => {
            ui.section = ReviewSection::Diagrams;
        }
    }
    let empty_diagrams = matches!(scene, Scene::DiagramsNoHarness | Scene::DiagramsNoAutoDraw)
        .then(|| (Vec::new(), DiagramsState::Idle));
    let diagrams = scene
        .is_diagram()
        .then(|| stage_diagrams(&mut ui, scene, now))
        .or(empty_diagrams);
    let checks_tell = matches!(scene, Scene::Security | Scene::Audits | Scene::AuditsAll)
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
    if let Some((versions, state)) = diagrams {
        put_diagrams(world, &pr, versions, state);
        // The mockups of the Diagrams tab have no right panel.
        world.resource_mut::<Chats>().entry(&pr).open = false;
    }
    if matches!(
        scene,
        Scene::AgentChat
            | Scene::AgentChatCodex
            | Scene::AgentChatCustom
            | Scene::Permission
            | Scene::PermissionCodex
            | Scene::PermissionOther
    ) {
        // As in the mockup, only the question turn: the log is not asked for again.
        let question = fixture::demo_agent_turns(now).remove(1);
        let mut chats = world.resource_mut::<Chats>();
        let chat = chats.entry(&pr);
        chat.show(PanelTab::Agent);
        chat.log_asked = true;
        chats.replay(&pr, &question);
        chats.entry(&pr).state = if scene == Scene::AgentChatCustom {
            // Idle after its answer: the pill reads "Echo · ready".
            SessionStateKind::Ready
        } else {
            SessionStateKind::Running
        };
    }
    if let Some(tell) = checks_tell {
        world.resource_mut::<Chats>().entry(&pr).state = SessionStateKind::Running;
        // The same way a daemon's answer arrives: `pump` fills the checks of the review.
        let _ = world.resource::<Outbox>().0.send(tell);
    }
    if let Some(round) = round {
        // The same way a daemon's answer arrives: `pump` fills the model of the review.
        let _ = world
            .resource::<Outbox>()
            .0
            .send(Tell::Revalidation(RevalidationTell::Round {
                pr: pr.clone(),
                round: Some(round),
            }));
    }
    if matches!(
        scene,
        Scene::Permission | Scene::PermissionCodex | Scene::PermissionOther
    ) {
        // A frozen clock keeps the countdown at 1:52 for as long as the render takes.
        *world.resource_mut::<Clock>() = Clock(Some(now));
        // Asked 8 s ago of a 2-minute wait, so the bar is a little short of full, as the
        // mockup's is.
        let now_ms = now * 1000;
        let request = if scene == Scene::PermissionOther {
            fixture::demo_permission_other(now_ms)
        } else {
            fixture::demo_permission_request(now_ms)
        };
        world
            .resource_mut::<PermissionQueue>()
            .push(request, now_ms - 8_000);
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
    use crate::bridge::Ask;
    use crate::nav::{FirstRunScreen, Screen};
    use crate::review_state::RevalView;
    use crate::screens::open_pr::PaletteRoot;
    use crate::screens::review::comments::CommentsRegion;
    use crate::screens::review::diff::DiffRegion;
    use crate::screens::review::finalize::FinalizeModal;
    use crate::screens::review::leave::LeaveModal;
    use crate::screens::review::revalidation::{
        RevalidationBanner, RevalidationRoot, RevalidationRow,
    };
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

    #[test]
    fn no_demo_scene_starts_with_a_toast() {
        for &scene in Scene::value_variants() {
            let mut app = staged(scene);
            app.add_systems(Update, crate::bridge::demo_answers);
            testing::settle(&mut app);
            let toasts = &app.world().resource::<crate::bridge::Toasts>().0;
            assert!(
                toasts.iter().all(|t| !t.text.starts_with("Demo mode")),
                "{scene:?} shows {:?}",
                toasts.iter().map(|t| &t.text).collect::<Vec<_>>()
            );
        }
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
                    | Scene::FirstRunBoth
                    | Scene::ConfigGeneral
                    | Scene::ConfigNotifications
                    | Scene::ConfigNotificationsBottom
                    | Scene::ConfigMedia
                    | Scene::ConfigAbout
                    | Scene::ConfigAppearance
                    | Scene::ConfigHarness
                    | Scene::ConfigHarnessCodex
                    | Scene::HomeFooter
                    | Scene::FirstRunCustom
                    | Scene::ConfigHarnessCustom
                    | Scene::ConfigFocus
                    | Scene::Restored
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
                Scene::CommentsHidden => {
                    assert_eq!(testing::count::<CommentsRegion>(&mut app), 1);
                    assert!(testing::shows(&mut app, "Hidden · @octo"));
                    assert!(testing::shows(&mut app, "2 hidden"));
                    assert!(testing::shows(&mut app, "new reply"));
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
                | Scene::FirstRunBoth
                | Scene::ConfigGeneral
                | Scene::ConfigNotifications
                | Scene::ConfigNotificationsBottom
                | Scene::ConfigMedia
                | Scene::ConfigAbout
                | Scene::ConfigAppearance
                | Scene::ConfigHarness
                | Scene::ConfigHarnessCodex
                | Scene::HomeFooter
                | Scene::FirstRunCustom
                | Scene::ConfigHarnessCustom
                | Scene::ConfigFocus
                | Scene::Restored => {
                    unreachable!("skipped above")
                }
                Scene::AgentChat | Scene::AgentChatCodex | Scene::AgentChatCustom => {
                    assert_eq!(tab.ui.file.as_deref(), Some("src/auth/refresh.rs"));
                    let chats = app
                        .world()
                        .resource::<crate::screens::review::agent::Chats>();
                    let chat = &chats.0[&pr];
                    assert!(
                        chat.tab == crate::screens::review::agent::PanelTab::Agent
                            && chat.log_asked
                    );
                    let want = if scene == Scene::AgentChatCustom {
                        clusia_protocol::SessionStateKind::Ready
                    } else {
                        clusia_protocol::SessionStateKind::Running
                    };
                    assert_eq!(chat.state, want, "{scene:?}");
                }
                Scene::Permission | Scene::PermissionCodex | Scene::PermissionOther => {
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
                Scene::AuditsAll => {
                    assert_eq!(tab.ui.section, ReviewSection::Audits);
                    assert_eq!(tab.ui.audit_area, None, "All is the default");
                    assert!(testing::shows(
                        &mut app,
                        "Error handling, Performance, Docs and changelog: ok"
                    ));
                }
                Scene::Revalidated => {
                    assert_eq!(tab.ui.revalidation, RevalView::Auto);
                    assert_eq!(testing::count::<RevalidationRoot>(&mut app), 1);
                }
                Scene::RevalidatedBanner => {
                    assert_eq!(tab.ui.revalidation, RevalView::Closed);
                    assert_eq!(testing::count::<RevalidationBanner>(&mut app), 1);
                    assert_eq!(testing::count::<DiffRegion>(&mut app), 1);
                }
                Scene::Composer | Scene::ComposerPreview | Scene::Emoji | Scene::Gif => {
                    let editor = tab.ui.editor.as_ref().expect("an open composer");
                    assert!(matches!(editor.target, EditTarget::Line { line: 44, .. }));
                }
                Scene::Rendered => assert_eq!(tab.ui.section, ReviewSection::Comments),
                Scene::Diagrams
                | Scene::DiagramsDeps
                | Scene::DiagramsSequence
                | Scene::DiagramsEr
                | Scene::DiagramsUsecase
                | Scene::DiagramsDataflow
                | Scene::DiagramsStates
                | Scene::DiagramsClasses
                | Scene::DiagramNode
                | Scene::DiagramsDrawing
                | Scene::DiagramsCompare
                | Scene::DiagramsKeywords
                | Scene::DiagramsKeywordsUsecase
                | Scene::KeywordsCard
                | Scene::KeywordsXy => {
                    assert_eq!(tab.ui.section, ReviewSection::Diagrams, "{scene:?}");
                    assert!(tab.ui.diagram.is_some(), "{scene:?}");
                }
                Scene::DiagramsNoHarness | Scene::DiagramsNoAutoDraw => {
                    assert_eq!(tab.ui.section, ReviewSection::Diagrams, "{scene:?}");
                }
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
    fn the_first_run_both_scene_finds_both_and_selects_claude_code() {
        let mut app = staged_outside_review(Scene::FirstRunBoth);
        assert_eq!(app.world().resource::<Nav>().screen, Screen::FirstRun);
        let v = crate::screens::first_run::view(
            &app.world().resource::<Model>().snapshot,
            &crate::screens::first_run::FirstRunUi::default(),
            None,
        );
        assert!(v.harnesses.iter().all(|h| h.found && h.selectable));
        assert_eq!(
            v.harnesses.iter().map(|h| h.selected).collect::<Vec<_>>(),
            [true, false]
        );
        assert!(testing::shows(&mut app, "Found · /opt/homebrew/bin/codex"));
    }

    #[test]
    fn the_restored_scene_brings_back_three_tabs_with_the_demo_review_in_comments() {
        let mut app = staged_outside_review(Scene::Restored);
        let nav = app.world().resource::<Nav>().clone();
        let prs: Vec<_> = restored_tabs().tabs.into_iter().map(|t| t.pr).collect();
        assert_eq!(nav.reviews, prs);
        assert_eq!(nav.screen, Screen::Review(fixture::demo_pr()));
        assert_eq!(
            testing::tab(&app, &fixture::demo_pr()).ui.section,
            ReviewSection::Comments
        );
        assert_eq!(testing::count::<crate::nav::CloseTab>(&mut app), 3);
    }

    #[test]
    fn the_home_footer_scene_names_codex() {
        let mut app = staged_outside_review(Scene::HomeFooter);
        assert_eq!(app.world().resource::<Nav>().screen, Screen::Home);
        assert!(testing::shows(&mut app, "Agent: Codex"));
        testing::find::<crate::screens::home::AgentConfigButton>(&mut app, |_| true);
    }

    #[test]
    fn the_codex_harness_scene_shows_codex_selected_and_locked() {
        let mut app = staged_outside_review(Scene::ConfigHarnessCodex);
        assert_eq!(
            app.world().resource::<Nav>().screen,
            Screen::Config(Section::Harness)
        );
        for needle in [
            "codex app-server, with your login",
            "Codex 0.162.1 answered in 0.4 s · Logged in using ChatGPT",
            "Found at /opt/homebrew/bin/codex",
            "Codex always runs in its read-only sandbox",
            "Each check or diagram is one Codex turn on your account.",
            "Codex audits the change one area at a time.",
            "Test again",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        for gone in [
            "Also allow what my Claude Code settings already allow",
            "Run commands in a sandbox: no network, writes only in the review worktree",
            "Claude Code 2.1.294 answered in 1.8 s",
        ] {
            assert!(!testing::shows(&mut app, gone), "{gone}");
        }
        let marks: Vec<bool> = app
            .world_mut()
            .query::<&crate::screens::config::harness::RadioMark>()
            .iter(app.world())
            .map(|m| m.selected)
            .collect();
        assert_eq!(marks, [false, true, false]);
        let config = &app.world().resource::<Model>().snapshot.config;
        assert_eq!(config.harness.kind, clusia_core::config::HarnessKind::Codex);
    }

    #[test]
    fn the_codex_chat_and_permission_scenes_say_codex() {
        let mut chat = staged(Scene::AgentChatCodex);
        for needle in [
            "Codex · thinking…",
            "session resumed · knows this review",
            "Is the new lock needed at all, or would re-checking the expiry be enough?",
        ] {
            assert!(testing::shows(&mut chat, needle), "{needle}");
        }
        assert!(!testing::shows(&mut chat, "Claude Code"));

        let mut modal = staged(Scene::PermissionCodex);
        for needle in [
            "Codex wants to run a command",
            "cargo test -p clusia-auth refresh_race -- --nocapture",
            "Allow once",
            "Codex · thinking…",
        ] {
            assert!(testing::shows(&mut modal, needle), "{needle}");
        }
        assert!(!testing::shows(&mut modal, "Claude Code"));
    }

    #[test]
    fn the_custom_harness_scene_shows_the_custom_card_notice_and_fields() {
        let mut app = staged_outside_review(Scene::ConfigHarnessCustom);
        assert_eq!(
            app.world().resource::<Nav>().screen,
            Screen::Config(Section::Harness)
        );
        for needle in [
            "any program that speaks Clúsia's JSON lines",
            crate::screens::config::harness::SANDBOX_NOTICE,
            "How to write one",
            "Required. Runs once per turn",
            "Passed to every turn",
            "Shown on the pages. Filled in by Test",
            "Echo 1.0 answered in 0.2 s · protocol 1",
            "Each check or diagram is one Echo turn on your account.",
            "Echo audits the change one area at a time.",
            "Test again",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        for gone in [
            "Also allow what my Claude Code settings already allow",
            "Run commands in a sandbox: no network, writes only in the review worktree",
            "Codex always runs in its read-only sandbox",
            "on your PATH",
            "Arrives in a later milestone",
        ] {
            assert!(!testing::shows(&mut app, gone), "{gone}");
        }
        let marks: Vec<bool> = app
            .world_mut()
            .query::<&crate::screens::config::harness::RadioMark>()
            .iter(app.world())
            .map(|m| m.selected)
            .collect();
        assert_eq!(marks, [false, false, true]);
        let config = &app.world().resource::<Model>().snapshot.config;
        assert_eq!(
            config.harness.kind,
            clusia_core::config::HarnessKind::Custom
        );
        assert_eq!(config.harness.custom_name, "Echo");
    }

    #[test]
    fn the_custom_chat_and_permission_scenes_say_echo() {
        let mut chat = staged(Scene::AgentChatCustom);
        for needle in [
            "Echo · ready",
            "session resumed · knows this review",
            "Is the new lock needed at all, or would re-checking the expiry be enough?",
        ] {
            assert!(testing::shows(&mut chat, needle), "{needle}");
        }
        assert!(!testing::shows(&mut chat, "Claude Code"));
        assert!(!testing::shows(&mut chat, "Echo · thinking…"));

        let mut modal = staged(Scene::PermissionOther);
        for needle in [
            "Echo asks to do something",
            "Remove the build cache",
            "Deny",
            "Allow once",
            "Echo · thinking…",
        ] {
            assert!(testing::shows(&mut modal, needle), "{needle}");
        }
        assert!(!testing::shows(&mut modal, "for this review"));
        assert!(!testing::shows(&mut modal, "Claude Code"));
        assert_eq!(
            testing::count::<crate::screens::review::agent::PermissionModal>(&mut modal),
            1
        );
    }

    #[test]
    fn the_first_run_custom_scene_selects_the_custom_card_and_shows_the_program() {
        let mut app = staged_outside_review(Scene::FirstRunCustom);
        assert_eq!(app.world().resource::<Nav>().screen, Screen::FirstRun);
        let v = crate::screens::first_run::view(
            &app.world().resource::<Model>().snapshot,
            &crate::screens::first_run::FirstRunUi::default(),
            None,
        );
        assert!(v.custom.selected);
        assert_eq!(v.custom.program, "/usr/local/bin/echo-harness");
        assert!(v.harnesses[0].found && !v.harnesses[0].selected);
        assert!(!v.harnesses[1].found);
        assert!(testing::shows(&mut app, "Required. Runs once per turn"));
        assert!(testing::shows(&mut app, "Echo turn on your account"));
    }

    #[test]
    fn the_focus_scene_opens_config_at_the_diagrams_checkbox() {
        let app = staged_outside_review(Scene::ConfigFocus);
        assert_eq!(
            testing::config_focus(&app),
            (Screen::Config(Section::Harness), Some("harness.diagrams"))
        );
    }

    #[test]
    fn the_empty_diagrams_scenes_offer_their_buttons() {
        let mut app = staged(Scene::DiagramsNoHarness);
        assert!(testing::shows(&mut app, "Set up a harness"));
        assert!(!testing::shows(&mut app, "Draw on every review…"));
        let mut app = staged(Scene::DiagramsNoAutoDraw);
        assert!(testing::shows(&mut app, "Draw on every review…"));
        assert!(!testing::shows(&mut app, "Set up a harness"));
    }

    #[test]
    fn config_scenes_open_their_sections() {
        for (scene, section) in [
            (Scene::ConfigGeneral, Section::General),
            (Scene::ConfigNotifications, Section::Notifications),
            (Scene::ConfigNotificationsBottom, Section::Notifications),
            (Scene::ConfigMedia, Section::Media),
            (Scene::ConfigAbout, Section::About),
            (Scene::ConfigAppearance, Section::Appearance),
            (Scene::ConfigHarness, Section::Harness),
            (Scene::ConfigHarnessCodex, Section::Harness),
            (Scene::ConfigHarnessCustom, Section::Harness),
            (Scene::ConfigFocus, Section::Harness),
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
    fn the_revalidated_scene_shows_every_row_kind() {
        let mut app = staged(Scene::Revalidated);
        for needle in [
            "2 new commits since you saved this review. Claude Code checked your 5 comments against them.",
            "1 kept · 1 moved · 1 obsolete · 1 new wording suggested",
            "See the new commits",
            "Accept all",
            "Kept",
            "Moved",
            "Obsolete",
            "New wording",
            "Not checked",
            "store.rs:81 → 88",
            "save() moved down 7 lines when the temp-file helper was added; the comment follows the same code.",
            "The retry loop was removed in \"refactor: one retry path\".",
            "c41d2e7",
            "refactor: one retry path",
            "a98f03b",
            "fix: write the token through a temp file",
            "Suggested: ",
            "The new test covers two refreshes at once; a refresh racing invalidate() is still missing.",
            "Put it back",
            "Keep the move",
            "Keep and re-anchor",
            "Remove comment",
            "Keep mine",
            "Use new wording",
            "Claude Code did not return a result for this comment",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        assert_eq!(testing::count::<RevalidationRow>(&mut app), 5);
        assert!(
            !testing::shows(&mut app, "verified") && !testing::shows(&mut app, "guaranteed"),
            "the screen says checked, never verified"
        );
    }

    #[test]
    fn the_revalidated_scene_changes_nothing_in_the_draft() {
        let app = staged(Scene::Revalidated);
        let (want, _) = fixture::demo_revalidation(NOW);
        let draft = testing::ready(&app, &fixture::demo_pr()).view.review.draft;
        assert_eq!(draft, want, "a round only proposes");
        assert!(
            testing::ready(&app, &fixture::demo_pr())
                .view
                .review
                .revalidation
                .is_some(),
            "the review carries the round the screen shows"
        );
        assert_eq!(
            testing::ready(&app, &fixture::demo_pr()).view.review.state,
            clusia_core::ReviewState::Revalidated
        );
    }

    #[test]
    fn the_banner_scene_shows_the_diff_under_a_compact_banner() {
        let mut app = staged(Scene::RevalidatedBanner);
        // The strip is a text and a **Review them** button, so each is a node of its own.
        assert!(testing::shows(
            &mut app,
            "3 suggestions from the revalidation ·"
        ));
        assert!(testing::shows(&mut app, "Review them"));
        assert_eq!(testing::count::<RevalidationRoot>(&mut app), 0);
        assert!(
            !testing::shows(&mut app, "Put it back"),
            "the rows are on the other screen"
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
        assert_eq!(config["harness"]["revalidate"], true);
        assert_eq!(config["harness"]["revalidate_quiet_secs"], 300);
        for needle in [
            "When new commits arrive on a saved review",
            "Revalidate my draft",
            "Wait for pushes to settle",
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
            "Not found on your PATH",
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
        assert_eq!(demo_config(Scene::ConfigAppearance), default);
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

    const DIAGRAM_SCENES: [Scene; 15] = [
        Scene::Diagrams,
        Scene::DiagramsDeps,
        Scene::DiagramsSequence,
        Scene::DiagramsEr,
        Scene::DiagramsUsecase,
        Scene::DiagramsDataflow,
        Scene::DiagramsStates,
        Scene::DiagramsClasses,
        Scene::DiagramNode,
        Scene::DiagramsDrawing,
        Scene::DiagramsCompare,
        Scene::DiagramsKeywords,
        Scene::DiagramsKeywordsUsecase,
        Scene::KeywordsCard,
        Scene::KeywordsXy,
    ];

    /// The versions the Diagrams model of `pr` holds, oldest first.
    fn versions_of(app: &App, pr: &clusia_core::PrRef) -> Vec<DiagramVersion> {
        let model = &app.world().resource::<Diagrams>().0[pr];
        let mut versions: Vec<DiagramVersion> = model.loaded.values().cloned().collect();
        versions.sort_by_key(|v| v.number);
        versions
    }

    #[test]
    fn every_diagram_scene_is_listed_and_the_others_are_not() {
        let listed: Vec<Scene> = Scene::value_variants()
            .iter()
            .copied()
            .filter(|s| s.is_diagram())
            .collect();
        assert_eq!(listed, DIAGRAM_SCENES);
    }

    #[test]
    fn each_diagram_scene_selects_a_drawing_of_its_kind_in_the_latest_version() {
        let pr = fixture::demo_pr();
        for scene in DIAGRAM_SCENES {
            let Some(kind) = scene.diagram_kind() else {
                continue;
            };
            let app = staged(scene);
            let tab = testing::tab(&app, &pr);
            assert_eq!(tab.ui.section, ReviewSection::Diagrams, "{scene:?}");
            let versions = versions_of(&app, &pr);
            let latest = versions.last().expect("a version");
            let key = tab.ui.diagram.clone().expect("a selected drawing");
            assert_eq!(
                latest.entry(&key).expect("the drawing").plan.kind,
                kind,
                "{scene:?}"
            );
            assert_eq!(tab.ui.diagram_version, None, "{scene:?}: the latest");
            assert_eq!(
                tab.ui.diagram_compare,
                scene == Scene::DiagramsCompare,
                "{scene:?}"
            );
            assert!(
                latest.entries.iter().all(|e| matches!(
                    e.state,
                    EntryState::Ready { .. } | EntryState::ReadyMap { .. }
                )),
                "{scene:?}"
            );
        }
    }

    #[test]
    fn every_diagram_scene_shows_its_list_and_canvas_instead_of_loading() {
        let pr = fixture::demo_pr();
        for scene in DIAGRAM_SCENES {
            let mut app = staged(scene);
            assert!(
                !testing::shows(&mut app, "Loading diagrams…"),
                "{scene:?} is still loading"
            );
            let key = testing::tab(&app, &pr)
                .ui
                .diagram
                .clone()
                .expect("a drawing");
            let versions = versions_of(&app, &pr);
            let title = versions
                .last()
                .and_then(|v| v.entry(&key))
                .map(|e| e.plan.title.clone())
                .expect("the selected drawing");
            assert!(testing::shows(&mut app, &title), "{scene:?}: {title}");
            let drawn = testing::count::<crate::screens::review::diagrams::DiagramCanvas>(&mut app);
            let keywords = testing::count::<
                crate::screens::review::diagrams::keywords::KeywordsCanvas,
            >(&mut app);
            let (drawn_expected, keywords_expected) =
                if scene.is_keywords() { (0, 1) } else { (1, 0) };
            assert_eq!(
                (drawn, keywords),
                (drawn_expected, keywords_expected),
                "{scene:?}: one canvas of its kind"
            );
        }
        let mut app = staged(Scene::Diagrams);
        assert!(testing::shows(&mut app, "Token refresh flow"));
    }

    #[test]
    fn the_keyword_scenes_stage_the_view_like_the_mockups() {
        use crate::review_state::{CameraPose, KeywordsUi, View3D};
        use crate::screens::review::diagrams::keywords::render::{focus_pose, view_pose};
        let pr = fixture::demo_pr();
        let map = fixture::keywords_map();
        let scene3d = clusia_diagram::keywords::scene(&map);
        for scene in [
            Scene::DiagramsKeywords,
            Scene::DiagramsKeywordsUsecase,
            Scene::KeywordsCard,
            Scene::KeywordsXy,
        ] {
            let app = staged(scene);
            assert_eq!(
                testing::tab(&app, &pr).ui.diagram.as_deref(),
                Some("keywords"),
                "{scene:?}"
            );
            let ui = testing::keywords_ui(&app, &pr);
            match scene {
                Scene::DiagramsKeywords => assert_eq!(
                    ui,
                    KeywordsUi {
                        shown: ui.shown.clone(),
                        ..KeywordsUi::default()
                    }
                ),
                Scene::DiagramsKeywordsUsecase => {
                    assert_eq!(map.plan.use_cases[0].name, "Token refresh");
                    assert_eq!(ui.use_case, Some(0));
                    assert_eq!(ui.camera, focus_pose(&scene3d, 0));
                    assert_eq!((ui.card, ui.view), (None, View3D::Free));
                }
                Scene::KeywordsCard => {
                    let point = &scene3d.points[ui.card.expect("a point") as usize];
                    assert!(point.changed);
                    let place = &map.places[point.places[0] as usize];
                    assert_eq!(map.words[place.word as usize].text, "refresh");
                    assert_eq!(map.paths[place.path as usize], "src/token/store.rs");
                    assert_eq!(point.shape, clusia_diagram::keywords::PointShape::Declared);
                    assert_eq!(ui.use_case, None);
                }
                Scene::KeywordsXy => {
                    assert_eq!(ui.view, View3D::XY);
                    assert_eq!(ui.camera, view_pose(View3D::XY, &CameraPose::default()));
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn the_keyword_scenes_open_with_the_final_camera() {
        use crate::screens::review::diagrams::keywords::KeywordsRuntime;
        let pr = fixture::demo_pr();
        let mut app = staged(Scene::DiagramsKeywordsUsecase);
        testing::settle(&mut app);
        let ui = testing::keywords_ui(&app, &pr);
        let pose = app
            .world_mut()
            .query::<&KeywordsRuntime>()
            .single(app.world())
            .expect("one canvas")
            .pose;
        assert_eq!(pose, ui.camera, "no fly on the first frame");
    }

    #[test]
    fn the_first_run_does_not_offer_the_3d_view() {
        let mut app = staged_outside_review(Scene::FirstRun);
        assert!(!testing::shows(&mut app, "Include the 3D keyword view"));
    }

    #[test]
    fn a_map_drawn_before_keywords_were_turned_off_still_shows() {
        let pr = fixture::demo_pr();
        let mut app = testing::app(fixture::demo(NOW));
        testing::set_config_locally(&mut app, "harness.keywords_3d", "false");
        stage(app.world_mut(), Scene::DiagramsKeywords);
        stage_snapshot(app.world_mut(), Scene::DiagramsKeywords);
        testing::settle(&mut app);
        assert_eq!(
            testing::tab(&app, &pr).ui.diagram.as_deref(),
            Some("keywords")
        );
        assert!(testing::shows(&mut app, "Where the words live"));
        assert_eq!(
            testing::count::<crate::screens::review::diagrams::keywords::KeywordsCanvas>(&mut app),
            1
        );
    }

    #[test]
    fn the_diagram_scenes_hide_the_right_panel_like_the_mockups() {
        let pr = fixture::demo_pr();
        for scene in DIAGRAM_SCENES {
            let app = staged(scene);
            let chats = app.world().resource::<Chats>();
            assert!(!chats.0[&pr].open, "{scene:?}");
        }
        let app = staged(Scene::Diff);
        assert!(
            app.world()
                .resource::<Chats>()
                .0
                .get(&pr)
                .is_none_or(|c| c.open)
        );
    }

    #[test]
    fn the_node_scene_opens_the_dialog_on_a_box_that_stands_for_code() {
        let pr = fixture::demo_pr();
        let mut app = staged(Scene::DiagramNode);
        assert!(modal_shown(&mut app, Modal::DiagramNode));
        let tab = testing::tab(&app, &pr);
        let (number, key, node) = tab.ui.diagram_node.expect("a box");
        let versions = versions_of(&app, &pr);
        let latest = versions.last().expect("a version");
        assert_eq!(number, latest.number);
        assert!(
            crate::screens::review::diagrams::modal::node_anchor(latest, &key, &node).is_some(),
            "the box has code behind it"
        );
        assert!(testing::shows(&mut app, "Add to draft"));
        assert!(testing::recorded(&mut app).iter().any(|a| matches!(
            a,
            Ask::ReadDiagramCode { number: n, .. } if *n == number
        )));
    }

    #[test]
    fn the_node_scene_opens_the_changed_box_like_the_mockup() {
        let pr = fixture::demo_pr();
        let app = staged(Scene::DiagramNode);
        let (_, key, node) = testing::tab(&app, &pr).ui.diagram_node.expect("a box");
        assert_eq!((key.as_str(), node.as_str()), ("flow", "refresh"));
    }

    #[test]
    fn the_drawing_scene_has_a_version_under_way() {
        let pr = fixture::demo_pr();
        let app = staged(Scene::DiagramsDrawing);
        let versions = versions_of(&app, &pr);
        assert_eq!(versions.len(), 1);
        let drawing = &versions[0];
        assert_eq!((drawing.number, drawing.phase), (1, VersionPhase::Drawing));
        let states: Vec<&str> = drawing
            .entries
            .iter()
            .map(|e| match e.state {
                EntryState::Ready { .. } => "ready",
                EntryState::Drawing => "drawing",
                EntryState::Waiting => "waiting",
                _ => "other",
            })
            .collect();
        assert_eq!(states, ["ready", "ready", "drawing", "waiting", "waiting"]);
        let model = &app.world().resource::<Diagrams>().0[&pr];
        assert_eq!(
            model.state,
            DiagramsState::Drawing {
                done: 2,
                total: 5,
                key: drawing.entries[2].plan.key.clone(),
                step: None,
            }
        );
        let tab = testing::tab(&app, &pr);
        assert_eq!(tab.ui.diagram, Some(drawing.entries[0].plan.key.clone()));
    }

    #[test]
    fn the_compare_scene_is_on_a_drawing_that_both_versions_have() {
        let pr = fixture::demo_pr();
        let app = staged(Scene::DiagramsCompare);
        let versions = versions_of(&app, &pr);
        assert!(versions.len() >= 2, "there is a version to compare with");
        let key = testing::tab(&app, &pr).ui.diagram.expect("a drawing");
        assert!(
            versions[0].entry(&key).is_some(),
            "the drawing is in version 1 too"
        );
        assert!(testing::tab(&app, &pr).ui.diagram_compare);
    }
}
