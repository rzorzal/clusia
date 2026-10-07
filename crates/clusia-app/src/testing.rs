//! A headless app for unit tests: MinimalPlugins, no window, no GPU, no daemon. Asks are
//! recorded in `Asks::recorded`. Each task that adds a logic plugin registers it in `app`.

use bevy::clipboard::Clipboard;
use bevy::input::ButtonInput;
use bevy::input_focus::InputFocus;
use bevy::prelude::*;
use bevy::text::{EditableText, FontCx, LayoutCx, TextEdit};
use bevy::ui_widgets::Activate;
use bevy::window::{RequestRedraw, WindowThemeChanged};
use clusia_core::{Density, Paths, PrRef};
use clusia_protocol::WindowTarget;

use crate::app::{AppPaths, StartTarget};
use crate::bridge::{self, Ask, Asks, Connection, Model, Outbox, ShowRequested, Tell, Toasts};
use crate::clock::Clock;
use crate::fixture;
use crate::fonts::UiFonts;
use crate::nav::{Nav, NavPlugin};
use crate::platform_open::OpenUrls;
use crate::review_state::{Phase, Ready, ReviewStatePlugin, ReviewTabs, Tab};
use crate::screens::config::ConfigPlugin;
use crate::screens::first_run::FirstRunPlugin;
use crate::screens::home::HomePlugin;
use crate::screens::open_pr::OpenPrPlugin;
use crate::screens::review::ReviewPlugin;
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
        .add_plugins(ReviewStatePlugin)
        .add_plugins(ReviewPlugin)
        .add_plugins(OpenPrPlugin)
        .init_resource::<OpenUrls>()
        .init_resource::<crate::ui::markdown::CopiedText>();
    app.update();
    app
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

/// Changes the config as the daemon would and runs a frame.
pub fn set_config_locally(app: &mut App, key: &str, value: &str) {
    let mut model = app.world_mut().resource_mut::<Model>();
    snapshot::apply_config_locally(&mut model.snapshot.config, key, value).unwrap();
    app.update();
}

/// Types `text` into the `EditableText` on `entity` at its cursor, as a keyboard would.
pub fn type_into(app: &mut App, entity: Entity, text: &str) {
    let mut editable = app
        .world_mut()
        .get_mut::<EditableText>(entity)
        .expect("an editable text");
    editable.queue_edit(TextEdit::Insert(text.into()));
    let mut fonts = FontCx::default();
    let mut layout = LayoutCx::default();
    let mut clipboard = Clipboard::default();
    editable.apply_pending_edits(&mut fonts, &mut layout.0, &mut clipboard, |_| true);
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
