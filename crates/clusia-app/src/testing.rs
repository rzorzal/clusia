//! A headless app for unit tests: MinimalPlugins, no window, no GPU, no daemon. Asks are
//! recorded in `Asks::recorded`. Each task that adds a logic plugin registers it in `app`.

use bevy::camera::NormalizedRenderTarget;
use bevy::clipboard::Clipboard;
use bevy::input::ButtonInput;
use bevy::input::mouse::MouseScrollUnit;
use bevy::input::touch::TouchPhase;
use bevy::input_focus::InputFocus;
use bevy::picking::backend::HitData;
use bevy::picking::events::{Click, Drag, DragEnd, Move, Out, Pointer, Press, Scroll};
use bevy::picking::pointer::{Location, PointerButton, PointerId};
use bevy::prelude::*;
use bevy::text::{EditableText, FontCx, LayoutCx, TextEdit, TextSpan};
use bevy::ui::{ComputedNode, UiGlobalTransform};
use bevy::ui_widgets::Activate;
use bevy::window::{RequestRedraw, WindowThemeChanged};
use clusia_core::keywords::{KEYWORDS_KEY, KeywordMap};
use clusia_core::revalidation::{
    CommitLine, Decision, Proposal, Revalidation, RoundState, Verdict, fingerprint,
};
use clusia_core::{
    Anchor, Density, DraftItem, DraftKind, ItemStatus, Origin, Paths, PrRef, Review, Side,
};
use clusia_diagram::keywords::Scene3D;
use clusia_protocol::WindowTarget;
use std::time::Duration;

use crate::app::{AppPaths, StartTarget};
use crate::bridge::{
    self, Ask, Asks, ChecksTell, Connection, Model, Outbox, RevalidationTell, ShowRequested, Tell,
    Toasts,
};
use crate::clock::Clock;
use crate::fixture;
use crate::fonts::UiFonts;
use crate::nav::{Nav, NavPlugin};
use crate::platform_open::OpenUrls;
use crate::review_state::{
    KeywordsUi, Phase, Ready, ReviewSection, ReviewStatePlugin, ReviewTabs, Tab,
};
use crate::screens::config::ConfigPlugin;
use crate::screens::first_run::FirstRunPlugin;
use crate::screens::home::HomePlugin;
use crate::screens::open_pr::OpenPrPlugin;
use crate::screens::review::ReviewPlugin;
use crate::screens::review::diagrams::keywords::{
    KeywordsCanvas, KeywordsRuntime, SceneImage, input, render,
};
use crate::snapshot::{self, Snapshot};
use crate::theme::{LIGHT, Theme, ThemePlugin};
use crate::ui::emoji::EmojiPlugin;
use crate::ui::kit::KitPlugin;

pub const NOW: i64 = 1_790_000_000;

pub fn app(snapshot: Snapshot) -> App {
    let (inbox, outbox) = bridge::local_link();
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, AssetPlugin::default()))
        .init_asset::<Image>()
        .init_asset::<Mesh>()
        .init_asset::<StandardMaterial>()
        .init_resource::<ButtonInput<KeyCode>>()
        .init_resource::<InputFocus>()
        .add_message::<WindowThemeChanged>()
        .add_message::<ShowRequested>()
        .add_message::<RequestRedraw>()
        .insert_resource(UiFonts::default())
        .insert_resource(Clock(Some(NOW)))
        .insert_resource(AppPaths(Paths::new("/tmp/clusia-test-home")))
        .insert_resource(StartTarget(WindowTarget::Home))
        .insert_resource(Theme::new(false, 13, Density::Comfortable))
        .insert_resource(ClearColor(LIGHT.bg))
        .insert_resource(Model {
            snapshot,
            connection: Connection::Live,
            ..Model::default()
        })
        .init_resource::<Asks>()
        .init_resource::<Toasts>()
        .insert_resource(inbox)
        .insert_resource(outbox)
        .add_systems(PreUpdate, bridge::pump)
        .add_plugins((
            ThemePlugin,
            KitPlugin,
            EmojiPlugin,
            NavPlugin,
            HomePlugin,
            ConfigPlugin,
        ))
        .add_plugins(FirstRunPlugin)
        .add_plugins(crate::fonts::FaceFollowPlugin)
        .add_plugins(ReviewStatePlugin)
        .add_plugins(crate::tab_memory::TabMemoryPlugin)
        .add_plugins(ReviewPlugin)
        .add_plugins(OpenPrPlugin)
        .init_resource::<OpenUrls>()
        .init_resource::<crate::platform_sound::PlayedSounds>()
        .init_resource::<crate::platform_window::Foregrounds>()
        .init_resource::<crate::ui::markdown::CopiedText>();
    app.update();
    app
}

/// `app` on an 800 × 600 target that lays UI out as the window does (no GPU), for tests that
/// measure where nodes land. `size` is the target in logical pixels.
pub fn layout_app(snapshot: Snapshot, size: UVec2) -> App {
    use bevy::camera::{Camera, ComputedCameraValues, RenderTargetInfo, Viewport};

    let mut app = app(snapshot);
    app.add_plugins((
        bevy::input::InputPlugin,
        bevy::window::WindowPlugin {
            primary_window: None,
            exit_condition: bevy::window::ExitCondition::DontExit,
            ..default()
        },
        bevy::transform::TransformPlugin,
        bevy::image::ImagePlugin::default(),
        bevy::text::TextPlugin,
        bevy::picking::DefaultPickingPlugins,
        bevy::ui::UiPlugin,
    ))
    .init_asset::<bevy::image::TextureAtlasLayout>();
    app.world_mut().spawn((
        Camera2d,
        Camera {
            computed: ComputedCameraValues {
                target_info: Some(RenderTargetInfo {
                    physical_size: size,
                    scale_factor: 1.0,
                }),
                ..default()
            },
            viewport: Some(Viewport {
                physical_size: size,
                ..default()
            }),
            ..default()
        },
    ));
    app
}

/// The laid-out box of `entity`: (left, top, width, height) in logical pixels.
pub fn frame(app: &App, entity: Entity) -> (f32, f32, f32, f32) {
    use bevy::ui::{ComputedNode, UiGlobalTransform};
    let node = app.world().get::<ComputedNode>(entity).expect("laid out");
    let at = app
        .world()
        .get::<UiGlobalTransform>(entity)
        .expect("positioned")
        .translation;
    let scale = node.inverse_scale_factor;
    let (w, h) = (node.size.x * scale, node.size.y * scale);
    (at.x * scale - w / 2.0, at.y * scale - h / 2.0, w, h)
}

/// Every `Text` that runs past the right edge of an ancestor with nothing between them that
/// scrolls sideways (or past the bottom edge with nothing that scrolls down), as
/// `"<text> ..."` lines. Clipped text is lost, so it counts as overflowing too. Hidden nodes
/// (`Display::None`, zero size) are skipped.
pub fn overflowing_text(app: &mut App) -> Vec<String> {
    use bevy::ui::OverflowAxis;
    const SLACK: f32 = 1.5;
    let texts: Vec<Entity> = {
        let mut q = app.world_mut().query_filtered::<Entity, With<Text>>();
        q.iter(app.world()).collect()
    };
    let mut out = Vec::new();
    for text in texts {
        let Some(node) = app.world().get::<ComputedNode>(text) else {
            continue;
        };
        if node.size.x <= 0.0 || node.size.y <= 0.0 {
            continue;
        }
        // A text can sit in a box stretched to its parent while its lines run past it.
        let (x, y, w, h) = frame(app, text);
        let (w, h) = (
            w.max(node.content_size.x * node.inverse_scale_factor),
            h.max(node.content_size.y * node.inverse_scale_factor),
        );
        let (mut scrolls_x, mut scrolls_y) = (false, false);
        let mut at = text;
        let mut hidden = false;
        let mut found = None;
        while let Some(parent) = app.world().get::<ChildOf>(at).map(|c| c.parent()) {
            at = parent;
            let Some(node) = app.world().get::<Node>(at) else {
                continue;
            };
            if node.display == Display::None {
                hidden = true;
                break;
            }
            scrolls_x |= node.overflow.x == OverflowAxis::Scroll;
            scrolls_y |= node.overflow.y == OverflowAxis::Scroll;
            if app.world().get::<ComputedNode>(at).is_none() {
                continue;
            }
            let (px, py, pw, ph) = frame(app, at);
            if found.is_none() && !scrolls_x && x + w > px + pw + SLACK {
                found = Some(format!("right by {:.0}px of {at:?}", x + w - px - pw));
            }
            if found.is_none() && !scrolls_y && y + h > py + ph + 4.0 && ph > 0.0 {
                found = Some(format!("below by {:.0}px of {at:?}", y + h - py - ph));
            }
        }
        if hidden {
            continue;
        }
        if let Some(why) = found {
            let mut shown = app
                .world()
                .get::<Text>(text)
                .map(|t| t.0.clone())
                .unwrap_or_default();
            for span in app.world().get::<Children>(text).into_iter().flatten() {
                shown.push_str(&app.world().get::<TextSpan>(*span).map_or("", |s| &s.0));
            }
            out.push(format!(
                "{:?} {why}",
                shown.chars().take(60).collect::<String>()
            ));
        }
    }
    out
}

/// The centre of `entity`'s laid-out box.
pub fn center(app: &App, entity: Entity) -> Vec2 {
    let (x, y, w, h) = frame(app, entity);
    Vec2::new(x + w / 2.0, y + h / 2.0)
}

/// Takes the asks recorded since the last call.
pub fn recorded(app: &mut App) -> Vec<Ask> {
    std::mem::take(&mut app.world_mut().resource_mut::<Asks>().recorded)
}

pub fn count<C: Component>(app: &mut App) -> usize {
    app.world_mut().query::<&C>().iter(app.world()).count()
}

/// The first entity whose `C` satisfies `want`.
pub fn find<C: Component>(app: &mut App, want: impl Fn(&C) -> bool) -> Entity {
    let mut q = app.world_mut().query::<(Entity, &C)>();
    q.iter(app.world())
        .find(|(_, c)| want(c))
        .map(|(e, _)| e)
        .expect("a matching entity")
}

/// Clicks `entity` (as `bevy_ui_widgets` would) and runs a frame.
pub fn activate(app: &mut App, entity: Entity) {
    app.world_mut().trigger(Activate { entity });
    app.update();
}

/// A primary-button click on `entity` that bubbles as a real one does, and a frame.
pub fn click(app: &mut App, entity: Entity) {
    use bevy::camera::NormalizedRenderTarget;
    use bevy::picking::backend::HitData;
    use bevy::picking::events::{Click, Pointer};
    use bevy::picking::pointer::{Location, PointerButton, PointerId};

    let location = Location {
        target: NormalizedRenderTarget::None {
            width: 1,
            height: 1,
        },
        position: Vec2::ZERO,
    };
    app.world_mut().trigger(Pointer::new(
        PointerId::Mouse,
        location,
        Click {
            button: PointerButton::Primary,
            hit: HitData::new(Entity::PLACEHOLDER, 0.0, None, None),
            duration: std::time::Duration::ZERO,
            count: 1,
        },
        entity,
    ));
    app.update();
}

/// Changes the config as the daemon would and runs a frame.
pub fn set_config_locally(app: &mut App, key: &str, value: &str) {
    let mut model = app.world_mut().resource_mut::<Model>();
    snapshot::apply_config_locally(&mut model.snapshot.config, key, value).unwrap();
    app.update();
}

/// Types `text` into the `EditableText` on `entity` at its cursor, as a keyboard would.
pub fn type_into(app: &mut App, entity: Entity, text: &str) {
    app.world_mut()
        .get_mut::<EditableText>(entity)
        .expect("an editable text")
        .queue_edit(TextEdit::Insert(text.into()));
    apply_edits(app, entity);
}

/// Applies the edits queued on the `EditableText` on `entity`, as the window's text plugin does
/// each frame (the headless app has no such plugin).
pub fn apply_edits(app: &mut App, entity: Entity) {
    // The app's own text contexts, when it has them, so moves that need a layout (to the start
    // of the text) work as in the window.
    let own_fonts = app.world_mut().remove_resource::<FontCx>();
    let own_layout = app.world_mut().remove_resource::<LayoutCx>();
    let (had_fonts, had_layout) = (own_fonts.is_some(), own_layout.is_some());
    let mut fonts = own_fonts.unwrap_or_default();
    let mut layout = own_layout.unwrap_or_default();
    app.world_mut()
        .get_mut::<EditableText>(entity)
        .expect("an editable text")
        .apply_pending_edits(&mut fonts, &mut layout.0, &mut Clipboard::default(), |_| {
            true
        });
    if had_fonts {
        app.world_mut().insert_resource(fonts);
    }
    if had_layout {
        app.world_mut().insert_resource(layout);
    }
}

/// The screen the window shows and the setting (config key) it was asked to bring into view.
pub fn config_focus(app: &App) -> (crate::nav::Screen, Option<&'static str>) {
    let nav = app.world().resource::<crate::nav::Nav>();
    (nav.screen.clone(), nav.focus)
}

/// Presses the button that opens Config at the setting `key`.
pub fn open_setting(app: &mut App, key: &str) {
    let button = find::<crate::screens::review::shell::OpenSetting>(app, |s| s.0 == key);
    activate(app, button);
}

/// Runs frames until rebuilt screens have settled.
pub fn settle(app: &mut App) {
    for _ in 0..3 {
        app.update();
    }
}

/// Delivers `tell` as the bridge would (through `pump`) and runs a frame.
pub fn tell(app: &mut App, tell: Tell) {
    let _ = app.world().resource::<Outbox>().0.send(tell);
    app.update();
}

/// The demo app with the demo review open and ready (`fixture::demo_review` without its
/// What's new rows, so no modal opens), and no recorded asks.
pub fn demo_review_app() -> App {
    let mut app = app(crate::fixture::demo(NOW));
    let pr = crate::fixture::demo_pr();
    app.world_mut()
        .resource_mut::<Nav>()
        .go(&WindowTarget::Review { pr: pr.clone() });
    app.update();
    let (view, _) = crate::fixture::demo_review(NOW);
    tell(
        &mut app,
        Tell::Opened {
            pr,
            view: Box::new(view),
            news: Vec::new(),
        },
    );
    settle(&mut app);
    recorded(&mut app);
    app
}

/// A copy of the tab of `pr`.
pub fn tab(app: &App, pr: &clusia_core::PrRef) -> Tab {
    app.world().resource::<ReviewTabs>().0[pr].clone()
}

/// The ready review of `pr` (panics when it is not ready).
pub fn ready(app: &App, pr: &clusia_core::PrRef) -> Ready {
    match tab(app, pr).phase {
        Phase::Ready(r) => *r,
        other => panic!("not ready: {other:?}"),
    }
}

/// Shows `rzorzal/clusia#123` and opens it with `fixture::demo_review` (its What's new rows
/// only when `news`), settles, and drops the asks recorded on the way.
pub fn open_ready(app: &mut App, news: bool) -> PrRef {
    let pr: PrRef = "rzorzal/clusia#123".parse().expect("valid ref");
    app.world_mut()
        .write_message(ShowRequested(WindowTarget::Review { pr: pr.clone() }));
    app.update();
    let (view, items) = fixture::demo_review(NOW);
    tell(
        app,
        Tell::Opened {
            pr: pr.clone(),
            view: Box::new(view),
            news: if news { items } else { Vec::new() },
        },
    );
    settle(app);
    recorded(app);
    pr
}

/// The text a rendered markdown body shows: each wrapping row of words read as one sentence,
/// and every other `Text` as it is.
pub fn shown_text(app: &mut App) -> Vec<String> {
    let mut out = Vec::new();
    let mut texts = app.world_mut().query::<&Text>();
    out.extend(texts.iter(app.world()).map(|t| t.0.clone()));
    let mut rows = app.world_mut().query::<(&Node, &Children)>();
    for (node, children) in rows.iter(app.world()) {
        if node.flex_wrap != FlexWrap::Wrap {
            continue;
        }
        let words: Vec<&str> = children
            .iter()
            .filter_map(|c| app.world().get::<Text>(c))
            .map(|t| t.0.as_str())
            .collect();
        out.push(
            words
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    out
}

/// Whether `needle` appears in what the window shows (`shown_text`).
pub fn shows(app: &mut App, needle: &str) -> bool {
    shown_text(app).iter().any(|t| t.contains(needle))
}

/// A readable outline of the tree under `root`: texts, emoji and image slots, weights, fills
/// and links, children in parentheses. Two bodies built from the same markdown have the same
/// outline.
pub fn tree_signature(app: &App, root: Entity) -> String {
    fn walk(world: &World, e: Entity, out: &mut String) {
        if let Some(t) = world.get::<Text>(e) {
            out.push_str(&format!("{:?}", t.0));
            if let Some(f) = world.get::<TextFont>(e) {
                out.push_str(&format!(" w{}", f.weight.0));
            }
        } else {
            out.push('N');
        }
        if let Some(e) = world.get::<crate::ui::markdown::MdEmoji>(e) {
            out.push_str(&format!(" emoji:{}", e.0));
        }
        if let Some(i) = world.get::<crate::ui::markdown::MdImage>(e) {
            out.push_str(&format!(" image:{}", i.0));
        }
        if let Some(l) = world.get::<crate::ui::markdown::MdLink>(e) {
            out.push_str(&format!(" link:{}", l.0));
        }
        if let Some(f) = world.get::<crate::ui::kit::Fill>(e) {
            out.push_str(&format!(" fill:{:?}", f.0));
        }
        if let Some(children) = world.get::<Children>(e) {
            out.push('(');
            for c in children {
                walk(world, *c, out);
                out.push(',');
            }
            out.push(')');
        }
    }
    let mut out = String::new();
    walk(app.world(), root, &mut out);
    out
}

/// Shows `rzorzal/clusia#123` (its tab starts loading) without answering the open.
pub fn app_show_review(app: &mut App) {
    let pr = fixture::demo_pr();
    app.world_mut()
        .write_message(ShowRequested(WindowTarget::Review { pr }));
    app.update();
    recorded(app);
}

/// Tells the demo's two check results for `pr` and settles.
pub fn open_checks(app: &mut App, pr: &PrRef) {
    let (results, states) = fixture::demo_checks(NOW);
    tell(
        app,
        Tell::Checks(ChecksTell::Loaded {
            pr: pr.clone(),
            results,
            states,
            accepted: Vec::new(),
            dismissed: Vec::new(),
        }),
    );
    settle(app);
}

/// Tells the demo's two drawings (`fixture::demo_diagrams_tell`) for `pr` and settles.
pub fn open_diagrams(app: &mut App, pr: &PrRef) {
    tell(app, fixture::demo_diagrams_tell(pr, NOW));
    settle(app);
}

/// Selects `section` in the tab of `pr` and settles.
pub fn set_section(app: &mut App, pr: &PrRef, section: ReviewSection) {
    app.world_mut()
        .resource_mut::<ReviewTabs>()
        .0
        .get_mut(pr)
        .expect("an open tab")
        .ui
        .section = section;
    settle(app);
}

fn right(path: &str, line: u32, commit: &str) -> Anchor {
    Anchor {
        path: path.into(),
        line,
        start_line: None,
        side: Side::Right,
        commit: commit.into(),
    }
}

/// A round over the demo draft, which gains `i4` and `i5`: `i1` kept, `i2` moved (the agent
/// proposes line 92; `before` is line 81), `i3` obsolete, `i4` with new wording and `i5` with
/// no result. The comments of `review` are left as they are but for the two new ones.
pub fn demo_round(review: &mut Review, state: RoundState) -> Revalidation {
    let head = review.head_sha.clone();
    review.draft.next_id = review.draft.next_id.max(5);
    for (id, line) in [("i4", 64), ("i5", 46)] {
        review.draft.items.push(DraftItem {
            id: id.into(),
            kind: DraftKind::LineComment,
            origin: Origin::Human,
            anchor: Some(right("src/auth/refresh.rs", line, &head)),
            thread: None,
            body: format!("Comment {id}."),
            status: ItemStatus::Ok,
            accepted: true,
            created_at: NOW,
        });
    }
    let item = |id: &str| {
        review
            .draft
            .items
            .iter()
            .find(|i| i.id == id)
            .expect("a demo item")
    };
    let proposal = |id: &str, before: Anchor, verdict: Verdict| Proposal {
        item: id.into(),
        before: Some(before),
        verdict,
        decision: Decision::Pending,
        fingerprint: fingerprint(item(id)),
    };
    let proposals = vec![
        proposal("i1", right("src/auth/refresh.rs", 44, &head), Verdict::Kept),
        proposal(
            "i2",
            right("src/auth/store.rs", 81, &head),
            Verdict::Moved {
                anchor: right("src/auth/store.rs", 92, &head),
                reason: "The function gained four lines above this one.".into(),
            },
        ),
        proposal(
            "i3",
            right("src/client/http.rs", 20, &head),
            Verdict::Obsolete {
                reason: "The retry loop was removed.".into(),
            },
        ),
        proposal(
            "i4",
            right("src/auth/refresh.rs", 64, &head),
            Verdict::Reword {
                text: "The new test covers two writers; a reader is still missing.".into(),
                reason: None,
            },
        ),
        proposal(
            "i5",
            right("src/auth/refresh.rs", 46, &head),
            Verdict::NotChecked,
        ),
    ];
    Revalidation {
        from_head: "1111111".into(),
        to_head: head,
        commits: vec![
            CommitLine {
                sha: "a1b2c3d".into(),
                title: "refactor: one retry path".into(),
            },
            CommitLine {
                sha: "e4f5a6b".into(),
                title: "fix: refresh early".into(),
            },
        ],
        before: proposals
            .iter()
            .map(|p| (p.item.clone(), p.before.clone()))
            .collect(),
        sp1_moved: 1,
        sp1_obsolete: 1,
        state,
        proposals,
        unreadable: 0,
        asked: 5,
        finished_at: Some(NOW),
        notified: false,
        diagrams_stale: None,
    }
}

/// How many working marks (rings and beating dots) `root` holds, itself included.
pub fn marks_in(app: &App, root: Entity) -> usize {
    let world = app.world();
    let mut count = 0;
    let mut stack = vec![root];
    while let Some(e) = stack.pop() {
        count += usize::from(
            world.get::<crate::ui::spinner::Spinner>(e).is_some()
                || world.get::<crate::ui::spinner::Beat>(e).is_some(),
        );
        if let Some(children) = world.get::<Children>(e) {
            stack.extend(children.iter());
        }
    }
    count
}

/// Opens the demo review with `demo_round` in `state` told by the daemon, and settles.
pub fn open_round(app: &mut App, state: RoundState) -> PrRef {
    let pr = open_ready(app, false);
    let round = {
        let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
        let ready = tabs
            .0
            .get_mut(&pr)
            .and_then(Tab::ready_mut)
            .expect("a ready tab");
        demo_round(&mut ready.view.review, state)
    };
    tell(
        app,
        Tell::Revalidation(RevalidationTell::Round {
            pr: pr.clone(),
            round: Some(round),
        }),
    );
    settle(app);
    recorded(app);
    pr
}

// ---- the 3D keyword view

/// Where the keyword view's image node sits on the window in tests.
pub const KEYWORDS_ORIGIN: Vec2 = Vec2::new(300.0, 120.0);

/// Opens the demo review on the keyword view of `fixture::keywords_map()`.
pub fn open_keywords(app: &mut App) -> PrRef {
    open_keywords_with(app, fixture::keywords_map(), |_| {})
}

/// Opens the demo review on the keyword view of `map`, after `edit` set the tab's keyword state
/// (so the scene is built from it).
pub fn open_keywords_with(
    app: &mut App,
    map: KeywordMap,
    edit: impl FnOnce(&mut KeywordsUi),
) -> PrRef {
    let pr = open_ready(app, false);
    {
        let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
        let ui = &mut tabs.0.get_mut(&pr).expect("an open tab").ui;
        ui.section = ReviewSection::Diagrams;
        ui.diagram = Some(KEYWORDS_KEY.into());
        edit(&mut ui.keywords);
    }
    let versions = fixture::with_keywords_map(fixture::demo_diagrams(NOW, &pr), map);
    tell(app, fixture::diagrams_tell_of(&pr, versions));
    settle(app);
    pr
}

/// Edits the keyword state of the tab of `pr` and runs a frame.
pub fn set_keywords(app: &mut App, pr: &PrRef, edit: impl FnOnce(&mut KeywordsUi)) {
    {
        let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
        edit(&mut tabs.0.get_mut(pr).expect("an open tab").ui.keywords);
    }
    settle(app);
}

pub fn keywords_ui(app: &App, pr: &PrRef) -> KeywordsUi {
    tab(app, pr).ui.keywords
}

/// The runtime of the keyword canvas of `pr`.
pub fn runtime<'a>(app: &'a App, pr: &PrRef) -> &'a KeywordsRuntime {
    app.world()
        .iter_entities()
        .find_map(|e| {
            let canvas = e.get::<KeywordsCanvas>()?;
            (canvas.0 == *pr).then(|| e.get::<KeywordsRuntime>())?
        })
        .expect("a keyword canvas with its runtime")
}

/// The scene and the hovered point of `pr`'s keyword view.
pub fn keywords_runtime(app: &App, pr: &PrRef) -> (Scene3D, Option<u32>) {
    let rt = runtime(app, pr);
    (rt.scene.clone(), rt.hover)
}

/// Every entity with a `C`.
pub fn entities<C: Component>(app: &mut App) -> Vec<Entity> {
    let mut q = app.world_mut().query_filtered::<Entity, With<C>>();
    q.iter(app.world()).collect()
}

/// Gives `entity` the size and place layout would: `origin` is its top-left corner on the
/// window, in logical pixels. The test app has no window, so its layout leaves a node it was
/// given a size for alone (`a_node_given_a_size_keeps_it_across_frames` pins that).
pub fn place_node(app: &mut App, entity: Entity, origin: Vec2, size: Vec2) {
    app.world_mut().entity_mut(entity).insert((
        ComputedNode {
            size,
            ..ComputedNode::DEFAULT
        },
        UiGlobalTransform::from_translation(origin + size / 2.0),
    ));
}

/// Lays the keyword image out at `size` (logical pixels) at `KEYWORDS_ORIGIN` and runs frames;
/// returns the origin.
pub fn size_keywords(app: &mut App, pr: &PrRef, size: Vec2) -> Vec2 {
    let image = find::<SceneImage>(app, |s| s.0 == *pr);
    place_node(app, image, KEYWORDS_ORIGIN, size);
    settle(app);
    KEYWORDS_ORIGIN
}

/// Where point `index` of `pr`'s scene is on the window (the image must have been sized).
pub fn point_screen_pos(app: &mut App, pr: &PrRef, index: u32) -> Vec2 {
    let image = find::<SceneImage>(app, |s| s.0 == *pr);
    let size = app
        .world()
        .get::<ComputedNode>(image)
        .expect("a sized image")
        .size;
    let rt = runtime(app, pr);
    let point = &rt.scene.points[index as usize];
    let (at, _) = render::project(&rt.pose, rt.lens, size, render::world(point.pos))
        .expect("the point is in front of the camera");
    KEYWORDS_ORIGIN + at
}

// ---- the pointer on the 3D keyword view

fn location(position: Vec2) -> Location {
    Location {
        target: NormalizedRenderTarget::None {
            width: 1,
            height: 1,
        },
        position,
    }
}

fn nowhere() -> HitData {
    HitData::new(Entity::PLACEHOLDER, 0.0, None, None)
}

fn image_of(app: &mut App, pr: &PrRef) -> Entity {
    find::<SceneImage>(app, |s| s.0 == *pr)
}

fn press_at(app: &mut App, image: Entity, position: Vec2) {
    app.world_mut().trigger(Pointer::new(
        PointerId::Mouse,
        location(position),
        Press {
            button: PointerButton::Primary,
            hit: nowhere(),
            count: 1,
        },
        image,
    ));
}

fn click_event(app: &mut App, image: Entity, position: Vec2) {
    app.world_mut().trigger(Pointer::new(
        PointerId::Mouse,
        location(position),
        Click {
            button: PointerButton::Primary,
            hit: nowhere(),
            duration: Duration::from_millis(80),
            count: 1,
        },
        image,
    ));
}

/// The pointer moves to `position` (window pixels) over the 3D image.
pub fn hover_at(app: &mut App, pr: &PrRef, position: Vec2) {
    let image = image_of(app, pr);
    app.world_mut().trigger(Pointer::new(
        PointerId::Mouse,
        location(position),
        Move {
            hit: nowhere(),
            delta: Vec2::ZERO,
        },
        image,
    ));
    app.update();
}

/// The pointer leaves the 3D image.
pub fn leave(app: &mut App, pr: &PrRef) {
    let image = image_of(app, pr);
    app.world_mut().trigger(Pointer::new(
        PointerId::Mouse,
        location(Vec2::ZERO),
        Out { hit: nowhere() },
        image,
    ));
    app.update();
}

/// A press and a click at `position`.
pub fn click_at(app: &mut App, pr: &PrRef, position: Vec2) {
    let image = image_of(app, pr);
    press_at(app, image, position);
    click_event(app, image, position);
    settle(app);
}

/// A press at `from`, one drag of `delta`, the release, and the click the picking backend
/// sends when the button is released over what was pressed.
pub fn drag_by(app: &mut App, pr: &PrRef, from: Vec2, delta: Vec2) {
    let image = image_of(app, pr);
    press_at(app, image, from);
    app.world_mut().trigger(Pointer::new(
        PointerId::Mouse,
        location(from + delta),
        Drag {
            button: PointerButton::Primary,
            distance: delta,
            delta,
        },
        image,
    ));
    app.world_mut().trigger(Pointer::new(
        PointerId::Mouse,
        location(from + delta),
        DragEnd {
            button: PointerButton::Primary,
            distance: delta,
        },
        image,
    ));
    click_event(app, image, from + delta);
    settle(app);
}

/// The wheel turns `lines` (positive: up) over the 3D image.
pub fn scroll_at(app: &mut App, pr: &PrRef, position: Vec2, lines: f32) {
    let image = image_of(app, pr);
    app.world_mut().trigger(Pointer::new(
        PointerId::Mouse,
        location(position),
        Scroll {
            unit: MouseScrollUnit::Line,
            x: 0.0,
            y: lines,
            hit: nowhere(),
            phase: TouchPhase::Moved,
        },
        image,
    ));
    settle(app);
}

/// The points a click at their own centre would pick (nothing nearer covers them).
pub fn pickable(app: &mut App, pr: &PrRef) -> Vec<u32> {
    let image = image_of(app, pr);
    let size = app
        .world()
        .get::<ComputedNode>(image)
        .expect("a sized image")
        .size;
    let ui = keywords_ui(app, pr);
    let rt = runtime(app, pr);
    (0..rt.scene.points.len() as u32)
        .filter(|&i| {
            let point = &rt.scene.points[i as usize];
            let Some((at, _)) = render::project(&rt.pose, rt.lens, size, render::world(point.pos))
            else {
                return false;
            };
            input::pick(&rt.scene, &ui, &rt.pose, rt.lens, size, at) == Some(i)
        })
        .collect()
}
