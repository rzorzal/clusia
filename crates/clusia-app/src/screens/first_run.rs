//! First run (mockup `FirstRun.png`): connect GitHub, point at the folders with your clones,
//! see which AI harness is installed. It stands in for Home while the GitHub login is
//! missing, and stays until **Continue** once it has been shown, so the later steps can be
//! read after the login works.

use std::path::PathBuf;

use bevy::clipboard::Clipboard;
use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::ecs::system::NonSendMarker;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, ScrollArea, observe};
use clusia_protocol::{FirstRun, GithubLogin, Harness, HarnessKind, RepoFolder};

use crate::bridge::{Ask, Asks, Model, Toasts};
use crate::fonts::UiFonts;
use crate::nav::{FirstRunScreen, Leaf, Nav, NavSystems, Screen};
use crate::screens::config::git::token_from_clipboard;
use crate::screens::config::{clipboard_text, link, repos, warn};
use crate::snapshot::Snapshot;
use crate::theme::Swatch;
use crate::ui::kit::{Fill, Stroke, Type, Variant, button, card, disabled_button, text};

/// What this window has done with the first-run screen.
#[derive(Resource, Debug, Default, Clone, PartialEq, Eq)]
pub struct FirstRunUi {
    /// It was needed and **Continue** has not been pressed yet.
    pub pending: bool,
    /// "use a token instead" was pressed.
    pub token_open: bool,
}

/// The user's home folder, to write `~/…` the way the config does.
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct UserHome(pub Option<String>);

impl Default for UserHome {
    fn default() -> Self {
        Self(std::env::var("HOME").ok().filter(|h| !h.is_empty()))
    }
}

type Pick = Box<dyn Fn() -> Option<PathBuf> + Send + Sync>;

/// Asks the user for a folder. Tests put their own answer here.
#[derive(Resource)]
pub struct FolderPicker(pub Pick);

impl Default for FolderPicker {
    fn default() -> Self {
        Self(Box::new(|| rfd::FileDialog::new().pick_folder()))
    }
}

/// **+ Add folder** was pressed.
#[derive(Message, Debug)]
pub struct PickFolder;

#[derive(Component, Debug)]
pub struct UseGh;

#[derive(Component, Debug)]
pub struct UseToken;

#[derive(Component, Debug)]
pub struct PasteToken;

#[derive(Component, Debug)]
pub struct AddFolder;

#[derive(Component, Debug)]
pub struct Continue;

/// A harness card; the harness step only reports what was found.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct HarnessCard(pub &'static str);

#[derive(Component)]
struct FirstRunPart {
    built: Option<FirstRunView>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GithubStep {
    /// The daemon has a working login.
    Connected { login: Option<String> },
    /// `gh` is signed in and could be reused.
    GhFound { login: String, host: String },
    /// Nothing to reuse; `problem` is what `gh` reported, if anything.
    Manual { problem: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct FolderChip {
    pub path: String,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HarnessView {
    pub name: &'static str,
    pub found: bool,
    pub line: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FirstRunView {
    pub github: GithubStep,
    pub token_open: bool,
    /// `None` while the daemon has not answered.
    pub folders: Option<Vec<FolderChip>>,
    pub harnesses: Vec<HarnessView>,
    /// Why **Continue** is off, or `None` when it is on.
    pub blocked: Option<&'static str>,
}

/// The login works: GitHub accepted a token (offline counts as signed in).
pub fn signed_in(snap: &Snapshot) -> bool {
    snap.auth.is_some() && !snap.signed_out()
}

/// `path` with the home folder written as `~`.
pub fn tilde(path: &str, home: Option<&str>) -> String {
    let Some(home) = home
        .map(|h| h.trim_end_matches('/'))
        .filter(|h| !h.is_empty())
    else {
        return path.to_string();
    };
    if path == home {
        return "~".to_string();
    }
    match path.strip_prefix(home) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

fn folder_note(f: &RepoFolder) -> String {
    match (f.exists, f.repos) {
        (false, _) => "not found".to_string(),
        (true, 0) => "empty".to_string(),
        (true, 1) => "1 repo".to_string(),
        (true, n) => format!("{n} repos"),
    }
}

fn harness_view(kind: HarnessKind, found: Option<&Harness>) -> HarnessView {
    let name = match kind {
        HarnessKind::ClaudeCode => "Claude Code",
        HarnessKind::Codex => "Codex",
    };
    match found {
        Some(h) => HarnessView {
            name,
            found: true,
            line: match &h.path {
                Some(path) => format!("Found · {path}"),
                None => "Found".to_string(),
            },
        },
        None => HarnessView {
            name,
            found: false,
            line: "Not found on your PATH".to_string(),
        },
    }
}

pub fn view(snap: &Snapshot, ui: &FirstRunUi, home: Option<&str>) -> FirstRunView {
    let status: Option<&FirstRun> = snap.first_run.as_ref();
    let connected = signed_in(snap);
    let github = if connected {
        GithubStep::Connected {
            login: snap.auth.as_ref().and_then(|a| a.login.clone()),
        }
    } else {
        match status.map(|s| &s.github) {
            Some(GithubLogin::SignedIn { login, .. }) => GithubStep::GhFound {
                login: login.clone(),
                host: snap.config.github.host.clone(),
            },
            Some(GithubLogin::Error { message }) => GithubStep::Manual {
                problem: Some(message.clone()),
            },
            _ => GithubStep::Manual { problem: None },
        }
    };
    let folders = status.map(|s| {
        s.folders
            .iter()
            .map(|f| FolderChip {
                path: tilde(&f.path, home),
                note: folder_note(f),
            })
            .collect::<Vec<_>>()
    });
    let found = |kind| {
        status.and_then(|s| {
            s.harnesses
                .iter()
                .find(|h| h.kind == kind && h.path.is_some())
        })
    };
    let any_folder = status.is_some_and(|s| s.folders.iter().any(|f| f.exists));
    let blocked = if !connected {
        Some("Connect GitHub to continue.")
    } else if !any_folder {
        Some("Add a folder with your clones to continue.")
    } else {
        None
    };
    FirstRunView {
        github,
        token_open: ui.token_open,
        folders,
        harnesses: vec![
            harness_view(HarnessKind::ClaudeCode, found(HarnessKind::ClaudeCode)),
            harness_view(HarnessKind::Codex, found(HarnessKind::Codex)),
        ],
        blocked,
    }
}

/// `path` with a leading `~` written out and no trailing slash, so two spellings of one folder
/// compare equal.
fn expanded(path: &str, home: Option<&str>) -> String {
    let path = path.trim();
    let full = match (path.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            format!("{}{rest}", home.trim_end_matches('/'))
        }
        _ => path.to_string(),
    };
    match full.trim_end_matches('/') {
        "" if full.starts_with('/') => "/".to_string(),
        trimmed => trimmed.to_string(),
    }
}

/// The folder list with `picked` added, as the config value; `None` when nothing changes.
pub fn roots_with(roots: &[String], picked: &str, home: Option<&str>) -> Option<String> {
    let same = |a: &str| expanded(a, home) == expanded(picked, home);
    if roots.iter().any(|r| same(r)) {
        return None;
    }
    let new = tilde(picked, home);
    repos::with_root(roots, &new).map(|all| repos::roots_value(&all))
}

pub struct FirstRunPlugin;

impl Plugin for FirstRunPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FirstRunUi>()
            .init_resource::<UserHome>()
            .init_resource::<FolderPicker>()
            .add_systems(Update, gate.before(NavSystems))
            .add_message::<PickFolder>()
            .add_systems(
                Update,
                (build_first_run, rebuild_first_run, pick_folder)
                    .chain()
                    .after(NavSystems),
            );
    }
}

/// Swaps Home and First run as the login comes and goes.
fn gate(
    model: Res<Model>,
    mut ui: ResMut<FirstRunUi>,
    mut nav: ResMut<Nav>,
    mut asks: ResMut<Asks>,
) {
    if model.snapshot.signed_out() {
        ui.pending = true;
    } else if nav.screen != Screen::FirstRun && ui.pending {
        // The login came back somewhere else (Config): nothing is left to show.
        ui.pending = false;
        asks.send(Ask::FirstRunDone);
    }
    match (&nav.screen, ui.pending) {
        (Screen::Home, true) => nav.screen = Screen::FirstRun,
        (Screen::FirstRun, false) => nav.screen = Screen::Home,
        _ => {}
    }
}

fn build_first_run(mut commands: Commands, screens: Query<Entity, Added<FirstRunScreen>>) {
    for screen in &screens {
        commands.entity(screen).with_children(|p| {
            p.spawn((
                Node {
                    flex_grow: 1.0,
                    min_width: px(0),
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::Center,
                    padding: UiRect::axes(px(40), px(24)),
                    overflow: Overflow::scroll_y(),
                    ..default()
                },
                ScrollArea,
                FirstRunPart { built: None },
            ));
        });
    }
}

fn rebuild_first_run(
    mut commands: Commands,
    model: Res<Model>,
    ui: Res<FirstRunUi>,
    home: Res<UserHome>,
    fonts: Res<UiFonts>,
    leaf: Res<Leaf>,
    mut parts: Query<(Entity, &mut FirstRunPart)>,
) {
    let view = view(&model.snapshot, &ui, home.0.as_deref());
    for (entity, mut part) in &mut parts {
        if part.built.as_ref() == Some(&view) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        commands.entity(entity).with_children(|p| {
            build(p, &fonts, &leaf, &view);
        });
        part.built = Some(view.clone());
    }
}

fn step_badge(p: &mut ChildSpawnerCommands, fonts: &UiFonts, n: u8, on: bool) {
    let (fill, ink) = if on {
        (Swatch::Green, Swatch::OnGreen)
    } else {
        (Swatch::Chrome, Swatch::Muted)
    };
    p.spawn((
        Node {
            width: px(22),
            height: px(22),
            align_items: AlignItems::Center,
            justify_content: JustifyContent::Center,
            border_radius: BorderRadius::MAX,
            flex_shrink: 0.0,
            ..default()
        },
        BackgroundColor::default(),
        Fill(fill),
        children![text(fonts, n.to_string(), Type::STRONG.ink(ink))],
    ));
}

fn step_card(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    n: u8,
    on: bool,
    title: &str,
    optional: bool,
    body: impl FnOnce(&mut ChildSpawnerCommands),
) {
    p.spawn(card(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(12),
        padding: UiRect::axes(px(20), px(18)),
        ..default()
    }))
    .with_children(|c| {
        c.spawn(Node {
            column_gap: px(10),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|h| {
            step_badge(h, fonts, n, on);
            h.spawn((
                Node {
                    flex_grow: 1.0,
                    ..default()
                },
                children![text(fonts, title.to_string(), Type::STRONG)],
            ));
            if optional {
                h.spawn(text(fonts, "optional", Type::META));
            }
        });
        body(c);
    });
}

/// A tinted box with a title, a line and an optional action.
fn callout(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    fill: Swatch,
    stroke: Swatch,
    title: &str,
    line: &str,
    action: impl FnOnce(&mut ChildSpawnerCommands),
) {
    p.spawn((
        Node {
            padding: UiRect::axes(px(14), px(12)),
            column_gap: px(12),
            align_items: AlignItems::Center,
            border: px(2).all(),
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        BackgroundColor::default(),
        Fill(fill),
        BorderColor::default(),
        Stroke(stroke),
    ))
    .with_children(|c| {
        c.spawn(Node {
            flex_grow: 1.0,
            flex_shrink: 1.0,
            flex_direction: FlexDirection::Column,
            row_gap: px(2),
            ..default()
        })
        .with_children(|t| {
            t.spawn(text(fonts, title.to_string(), Type::STRONG));
            t.spawn(text(fonts, line.to_string(), Type::MUTED.size(12.0)));
        });
        action(c);
    });
}

fn build(p: &mut ChildSpawnerCommands, fonts: &UiFonts, leaf: &Leaf, v: &FirstRunView) {
    p.spawn(Node {
        width: px(640),
        max_width: percent(100),
        flex_direction: FlexDirection::Column,
        row_gap: px(22),
        margin: UiRect::vertical(auto()),
        ..default()
    })
    .with_children(|c| {
        c.spawn(Node {
            column_gap: px(16),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|h| {
            h.spawn((
                Node {
                    width: px(56),
                    height: px(56),
                    flex_shrink: 0.0,
                    ..default()
                },
                ImageNode::new(leaf.0.clone()),
            ));
            h.spawn(Node {
                flex_direction: FlexDirection::Column,
                row_gap: px(4),
                ..default()
            })
            .with_children(|t| {
                t.spawn(text(fonts, "Welcome to Clúsia", Type::TITLE.size(24.0)));
                t.spawn(text(
                    fonts,
                    "Three quick steps and your pull requests show up here and in the menu bar.",
                    Type::MUTED,
                ));
            });
        });
        github_step(c, fonts, v);
        folders_step(c, fonts, v);
        harness_step(c, fonts, v);
        c.spawn(Node {
            column_gap: px(12),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|r| match v.blocked {
            None => {
                r.spawn((
                    button(fonts, "Continue", Variant::Primary),
                    Continue,
                    observe(on_continue),
                ));
            }
            Some(why) => {
                r.spawn(disabled_button(fonts, "Continue"));
                r.spawn(text(fonts, why, Type::META));
            }
        });
    });
}

fn github_step(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &FirstRunView) {
    let connected = matches!(v.github, GithubStep::Connected { .. });
    step_card(p, fonts, 1, true, "Connect GitHub", false, |c| {
        match &v.github {
            GithubStep::Connected { login } => {
                let line = match login {
                    Some(login) => format!("Signed in as @{login}."),
                    None => "A token is set; GitHub has not confirmed it yet.".to_string(),
                };
                callout(
                    c,
                    fonts,
                    Swatch::GreenSoft,
                    Swatch::Green,
                    "GitHub is connected",
                    &line,
                    |_| {},
                );
            }
            GithubStep::GhFound { login, host } => {
                callout(
                    c,
                    fonts,
                    Swatch::GreenSoft,
                    Swatch::Green,
                    "Found the GitHub CLI",
                    &format!("Signed in as @{login} on {host}. Clúsia reuses that login."),
                    |a| {
                        a.spawn((
                            button(fonts, "Use gh", Variant::Primary),
                            UseGh,
                            observe(on_use_gh),
                        ));
                    },
                );
            }
            GithubStep::Manual { problem } => {
                let line = problem.clone().unwrap_or_else(|| {
                    "Install it from cli.github.com and run gh auth login, or use a token."
                        .to_string()
                });
                callout(
                    c,
                    fonts,
                    Swatch::Chrome,
                    Swatch::Line,
                    "No GitHub CLI login found",
                    &line,
                    |a| {
                        a.spawn(link(fonts, "cli.github.com", "https://cli.github.com/"));
                    },
                );
            }
        }
        if connected {
            return;
        }
        c.spawn(Node {
            column_gap: px(6),
            align_items: AlignItems::Center,
            flex_wrap: FlexWrap::Wrap,
            ..default()
        })
        .with_children(|r| {
            r.spawn(text(
                fonts,
                "Or paste a personal access token (stored in the Keychain):",
                Type::MUTED.size(12.0),
            ));
            if v.token_open {
                r.spawn((
                    button(fonts, "Paste token from clipboard", Variant::Secondary),
                    PasteToken,
                    observe(on_paste_token),
                ));
                r.spawn(text(fonts, "needs repo and read:org", Type::META));
            } else {
                r.spawn((
                    button(fonts, "use a token instead", Variant::Ghost),
                    UseToken,
                    observe(on_use_token),
                ));
            }
        });
    });
}

fn folders_step(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &FirstRunView) {
    let connected = matches!(v.github, GithubStep::Connected { .. });
    step_card(
        p,
        fonts,
        2,
        connected,
        "Where are your repositories?",
        false,
        |c| {
            c.spawn(text(
                fonts,
                "Clúsia looks for your clones here so it can review in a separate worktree, never touching your branch.",
                Type::MUTED,
            ));
            c.spawn(Node {
                flex_wrap: FlexWrap::Wrap,
                column_gap: px(8),
                row_gap: px(8),
                ..default()
            })
            .with_children(|r| {
                match &v.folders {
                    None => {
                        r.spawn(text(fonts, "Looking for your clones…", Type::META));
                    }
                    Some(chips) => {
                        for chip in chips {
                            r.spawn((
                                Node {
                                    height: px(28),
                                    padding: UiRect::horizontal(px(12)),
                                    column_gap: px(6),
                                    align_items: AlignItems::Center,
                                    border_radius: BorderRadius::MAX,
                                    ..default()
                                },
                                BackgroundColor::default(),
                                Fill(Swatch::Chrome),
                            ))
                            .with_children(|b| {
                                b.spawn(text(fonts, chip.path.clone(), Type::MONO.ink(Swatch::Fg)));
                                b.spawn(text(fonts, chip.note.clone(), Type::META));
                            });
                        }
                    }
                }
                r.spawn((
                    button(fonts, "+ Add folder", Variant::Secondary),
                    AddFolder,
                    observe(on_add_folder),
                ));
            });
        },
    );
}

fn harness_step(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &FirstRunView) {
    let connected = matches!(v.github, GithubStep::Connected { .. });
    step_card(
        p,
        fonts,
        3,
        connected,
        "Connect your AI harness",
        true,
        |c| {
            c.spawn(text(
            fonts,
            "The agent draws diagrams, checks security and audits each review with the tool you already use. Reviewing works without it.",
            Type::MUTED,
        ));
            c.spawn(Node {
                column_gap: px(8),
                ..default()
            })
            .with_children(|r| {
                let cards = v
                    .harnesses
                    .iter()
                    .map(|h| (h.name, h.found, h.line.as_str()))
                    .chain([("Custom command", false, "Any tool that speaks JSON")]);
                for (name, found, line) in cards {
                    let (fill, stroke, ink) = if found {
                        (Swatch::GreenSoft, Swatch::Green, Swatch::Green)
                    } else {
                        (Swatch::Surface, Swatch::Line, Swatch::Faint)
                    };
                    r.spawn((
                        Node {
                            flex_basis: px(0),
                            flex_grow: 1.0,
                            flex_direction: FlexDirection::Column,
                            row_gap: px(4),
                            padding: UiRect::axes(px(14), px(12)),
                            border: px(if found { 2 } else { 1 }).all(),
                            border_radius: BorderRadius::all(px(6)),
                            ..default()
                        },
                        BackgroundColor::default(),
                        Fill(fill),
                        BorderColor::default(),
                        Stroke(stroke),
                        HarnessCard(name),
                    ))
                    .with_children(|b| {
                        b.spawn(text(fonts, name, Type::STRONG));
                        b.spawn(text(fonts, line.to_string(), Type::META.ink(ink)));
                    });
                }
            });
            c.spawn(text(
            fonts,
            "Connecting a harness arrives with the agent. You can set it up later in Config › Harness.",
            Type::META,
        ));
        },
    );
}

fn on_use_gh(_activate: On<Activate>, mut asks: ResMut<Asks>) {
    asks.send(Ask::SetConfig {
        key: "github.auth".into(),
        value: "gh-cli".into(),
    });
    asks.send(Ask::RefreshAuth);
    // Without a token the daemon waits out its idle retry; this makes it look again now.
    asks.send(Ask::SyncNow);
}

fn on_use_token(_activate: On<Activate>, mut ui: ResMut<FirstRunUi>) {
    ui.token_open = true;
}

/// Sends a pasted token as the stored sign-in, or warns when the text is not one. `now` is
/// `Time::elapsed_secs_f64`.
pub fn send_pasted_token(text: &str, asks: &mut Asks, toasts: &mut Toasts, now: f64) {
    match token_from_clipboard(text) {
        Ok(token) => {
            asks.send(Ask::SetConfig {
                key: "github.auth".into(),
                value: "pat".into(),
            });
            asks.send(Ask::SetToken(token));
        }
        Err(message) => warn(toasts, now, message),
    }
}

fn on_paste_token(
    _activate: On<Activate>,
    clipboard: Option<ResMut<Clipboard>>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let now = time.elapsed_secs_f64();
    match clipboard_text(clipboard) {
        Ok(text) => send_pasted_token(&text, &mut asks, &mut toasts, now),
        Err(message) => warn(&mut toasts, now, message),
    }
}

fn on_add_folder(_activate: On<Activate>, mut picks: MessageWriter<PickFolder>) {
    picks.write(PickFolder);
}

fn on_continue(_activate: On<Activate>, mut ui: ResMut<FirstRunUi>, mut asks: ResMut<Asks>) {
    ui.pending = false;
    asks.send(Ask::FirstRunDone);
}

/// The native dialog needs the main thread, which `NonSendMarker` pins this system to.
fn pick_folder(
    _main: NonSendMarker,
    mut picks: MessageReader<PickFolder>,
    picker: Res<FolderPicker>,
    model: Res<Model>,
    home: Res<UserHome>,
    mut asks: ResMut<Asks>,
) {
    for _ in picks.read() {
        let Some(path) = (picker.0)() else { continue };
        let roots = &model.snapshot.config.repositories.roots;
        if let Some(value) = roots_with(roots, &path.to_string_lossy(), home.0.as_deref()) {
            asks.send(Ask::SetConfig {
                key: "repositories.roots".into(),
                value,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::nav::{HomeScreen, Section};
    use crate::testing::{self, NOW};
    use clusia_protocol::{AuthInfo, SyncState, SyncStatus};

    fn folder(path: &str, exists: bool, repos: u32) -> RepoFolder {
        RepoFolder {
            path: path.into(),
            exists,
            repos,
        }
    }

    fn status(github: GithubLogin, folders: Vec<RepoFolder>, harnesses: Vec<Harness>) -> FirstRun {
        FirstRun {
            github,
            folders,
            harnesses,
        }
    }

    /// The daemon always reports both kinds; one that was not found has no path.
    fn absent_codex() -> Harness {
        Harness {
            kind: HarnessKind::Codex,
            path: None,
            version: None,
        }
    }

    fn claude() -> Harness {
        Harness {
            kind: HarnessKind::ClaudeCode,
            path: Some("/opt/homebrew/bin/claude".into()),
            version: Some("2.1.0".into()),
        }
    }

    /// The demo snapshot with no GitHub login and `first_run` as the daemon would report it.
    fn signed_out_snap(first_run: Option<FirstRun>) -> Snapshot {
        let mut snap = fixture::demo(NOW);
        snap.auth = Some(AuthInfo {
            source: None,
            login: None,
            scopes: Vec::new(),
            error: Some("no GitHub token".into()),
        });
        snap.sync = Some(SyncStatus {
            state: SyncState::Unauthorized,
            ..SyncStatus::default()
        });
        snap.first_run = first_run;
        snap
    }

    fn with_clones() -> FirstRun {
        status(
            GithubLogin::SignedOut,
            vec![folder("/Users/maria/Repos", true, 12)],
            vec![claude(), absent_codex()],
        )
    }

    fn set_snapshot(app: &mut App, snap: Snapshot) {
        app.world_mut().resource_mut::<Model>().snapshot = snap;
        testing::settle(app);
    }

    fn screen(app: &App) -> Screen {
        app.world().resource::<Nav>().screen.clone()
    }

    #[test]
    fn home_paths_are_written_with_a_tilde() {
        let home = Some("/Users/maria");
        assert_eq!(tilde("/Users/maria/Repos", home), "~/Repos");
        assert_eq!(tilde("/Users/maria", home), "~");
        assert_eq!(tilde("/Users/mariana/Repos", home), "/Users/mariana/Repos");
        assert_eq!(tilde("/Volumes/Code", home), "/Volumes/Code");
        assert_eq!(tilde("~/src", home), "~/src");
        assert_eq!(tilde("/Users/maria/Repos", None), "/Users/maria/Repos");
    }

    #[test]
    fn folder_chips_say_what_was_found() {
        let first_run = status(
            GithubLogin::SignedOut,
            vec![
                folder("/Users/maria/Repos", true, 12),
                folder("/Users/maria/Projects", true, 1),
                folder("/Users/maria/src", true, 0),
                folder("/Users/maria/code", false, 0),
            ],
            Vec::new(),
        );
        let v = view(
            &signed_out_snap(Some(first_run)),
            &FirstRunUi::default(),
            Some("/Users/maria"),
        );
        let chips: Vec<(String, String)> = v
            .folders
            .unwrap()
            .into_iter()
            .map(|c| (c.path, c.note))
            .collect();
        assert_eq!(
            chips,
            [
                ("~/Repos".to_string(), "12 repos".to_string()),
                ("~/Projects".to_string(), "1 repo".to_string()),
                ("~/src".to_string(), "empty".to_string()),
                ("~/code".to_string(), "not found".to_string()),
            ]
        );
        let waiting = view(&signed_out_snap(None), &FirstRunUi::default(), None);
        assert_eq!(waiting.folders, None, "no answer yet is not an empty list");
    }

    #[test]
    fn github_step_follows_what_gh_and_the_daemon_say() {
        let ui = FirstRunUi::default();
        let gh = status(
            GithubLogin::SignedIn {
                login: "maria".into(),
                scopes: vec!["repo".into()],
            },
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(
            view(&signed_out_snap(Some(gh)), &ui, None).github,
            GithubStep::GhFound {
                login: "maria".into(),
                host: "github.com".into()
            }
        );
        let broken = status(
            GithubLogin::Error {
                message: "gh: token expired".into(),
            },
            Vec::new(),
            Vec::new(),
        );
        assert_eq!(
            view(&signed_out_snap(Some(broken)), &ui, None).github,
            GithubStep::Manual {
                problem: Some("gh: token expired".into())
            }
        );
        assert_eq!(
            view(&signed_out_snap(None), &ui, None).github,
            GithubStep::Manual { problem: None }
        );
        let connected = view(&fixture::demo(NOW), &ui, None).github;
        assert_eq!(
            connected,
            GithubStep::Connected {
                login: Some("rzorzal".into())
            }
        );
    }

    #[test]
    fn first_run_follows_the_login() {
        let mut app = testing::app(signed_out_snap(Some(with_clones())));
        testing::settle(&mut app);
        assert_eq!(screen(&app), Screen::FirstRun);
        assert_eq!(testing::count::<FirstRunScreen>(&mut app), 1);
        assert_eq!(testing::count::<HomeScreen>(&mut app), 0);
        assert_eq!(testing::count::<Continue>(&mut app), 0, "no login yet");

        // The login works: the steps stay readable until Continue.
        let mut signed_in = fixture::demo(NOW);
        signed_in.first_run = Some(with_clones());
        set_snapshot(&mut app, signed_in.clone());
        assert_eq!(screen(&app), Screen::FirstRun);
        let go = testing::find::<Continue>(&mut app, |_| true);
        testing::activate(&mut app, go);
        testing::settle(&mut app);
        assert_eq!(screen(&app), Screen::Home);
        assert_eq!(testing::count::<FirstRunScreen>(&mut app), 0);
        assert_eq!(testing::count::<HomeScreen>(&mut app), 1);

        // The login is lost later: it comes back.
        set_snapshot(&mut app, signed_out_snap(Some(with_clones())));
        assert_eq!(screen(&app), Screen::FirstRun);
    }

    #[test]
    fn continue_tells_the_bridge_first_run_is_done() {
        let mut app = testing::app(signed_out_snap(Some(with_clones())));
        testing::settle(&mut app);
        let mut signed_in = fixture::demo(NOW);
        signed_in.first_run = Some(with_clones());
        set_snapshot(&mut app, signed_in);
        assert!(
            !testing::recorded(&mut app).contains(&Ask::FirstRunDone),
            "the screen is still open"
        );
        let go = testing::find::<Continue>(&mut app, |_| true);
        testing::activate(&mut app, go);
        testing::settle(&mut app);
        assert_eq!(testing::recorded(&mut app), [Ask::FirstRunDone]);
    }

    #[test]
    fn a_login_made_in_config_tells_the_bridge_too() {
        let mut app = testing::app(signed_out_snap(Some(with_clones())));
        testing::settle(&mut app);
        app.world_mut()
            .resource_mut::<Nav>()
            .open_section(Section::GitServer);
        let mut signed_in = fixture::demo(NOW);
        signed_in.first_run = Some(with_clones());
        set_snapshot(&mut app, signed_in);
        assert_eq!(testing::recorded(&mut app), [Ask::FirstRunDone]);
        testing::settle(&mut app);
        assert!(
            testing::recorded(&mut app).is_empty(),
            "it is said once, not every frame"
        );
    }

    #[test]
    fn an_unknown_login_never_flashes_first_run() {
        let mut snap = fixture::demo(NOW);
        snap.auth = None;
        snap.sync = None;
        let mut app = testing::app(snap);
        testing::settle(&mut app);
        assert_eq!(screen(&app), Screen::Home);
        assert_eq!(testing::count::<FirstRunScreen>(&mut app), 0);
    }

    #[test]
    fn a_login_made_elsewhere_ends_first_run() {
        let mut app = testing::app(signed_out_snap(None));
        testing::settle(&mut app);
        assert_eq!(screen(&app), Screen::FirstRun);
        app.world_mut()
            .resource_mut::<Nav>()
            .open_section(crate::nav::Section::GitServer);
        testing::settle(&mut app);
        set_snapshot(&mut app, fixture::demo(NOW));
        app.world_mut()
            .resource_mut::<Nav>()
            .go(&clusia_protocol::WindowTarget::Home);
        testing::settle(&mut app);
        assert_eq!(screen(&app), Screen::Home);
    }

    #[test]
    fn continue_needs_a_folder_with_clones() {
        let nowhere = status(
            GithubLogin::SignedOut,
            vec![folder("/Users/maria/code", false, 0)],
            Vec::new(),
        );
        let mut snap = fixture::demo(NOW);
        snap.first_run = Some(nowhere);
        let blocked = view(&snap, &FirstRunUi::default(), None).blocked;
        assert_eq!(blocked, Some("Add a folder with your clones to continue."));
        snap.first_run = Some(with_clones());
        assert_eq!(view(&snap, &FirstRunUi::default(), None).blocked, None);
        assert_eq!(
            view(
                &signed_out_snap(Some(with_clones())),
                &FirstRunUi::default(),
                None
            )
            .blocked,
            Some("Connect GitHub to continue.")
        );
    }

    #[test]
    fn use_gh_selects_gh_and_checks_again() {
        let gh = status(
            GithubLogin::SignedIn {
                login: "maria".into(),
                scopes: Vec::new(),
            },
            Vec::new(),
            Vec::new(),
        );
        let mut app = testing::app(signed_out_snap(Some(gh)));
        testing::settle(&mut app);
        let use_gh = testing::find::<UseGh>(&mut app, |_| true);
        testing::activate(&mut app, use_gh);
        assert_eq!(
            testing::recorded(&mut app),
            [
                Ask::SetConfig {
                    key: "github.auth".into(),
                    value: "gh-cli".into()
                },
                Ask::RefreshAuth,
                Ask::SyncNow
            ]
        );
    }

    #[test]
    fn a_token_is_pasted_after_choosing_it() {
        let mut app = testing::app(signed_out_snap(Some(with_clones())));
        testing::settle(&mut app);
        assert_eq!(testing::count::<PasteToken>(&mut app), 0);
        let open = testing::find::<UseToken>(&mut app, |_| true);
        testing::activate(&mut app, open);
        testing::settle(&mut app);
        assert_eq!(testing::count::<PasteToken>(&mut app), 1);
        assert_eq!(testing::count::<UseToken>(&mut app), 0);
        assert!(testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn pasted_tokens_select_pat_then_store() {
        let mut asks = Asks::default();
        let mut toasts = Toasts::default();
        send_pasted_token("two words", &mut asks, &mut toasts, 1.0);
        assert!(asks.recorded.is_empty());
        assert_eq!(toasts.0.len(), 1);
        send_pasted_token(" ghp_abc123 \n", &mut asks, &mut toasts, 1.0);
        assert_eq!(
            asks.recorded,
            [
                Ask::SetConfig {
                    key: "github.auth".into(),
                    value: "pat".into()
                },
                Ask::SetToken(clusia_protocol::Secret::from("ghp_abc123")),
            ]
        );
    }

    #[test]
    fn a_folder_already_listed_is_not_added_again() {
        let home = Some("/Users/maria");
        let tilde_roots = ["~/Repos".to_string()];
        let absolute_roots = ["/Users/maria/Repos".to_string()];
        for roots in [&tilde_roots, &absolute_roots] {
            assert_eq!(roots_with(roots, "/Users/maria/Repos", home), None);
            assert_eq!(roots_with(roots, "/Users/maria/Repos/", home), None);
        }
        assert_eq!(
            roots_with(&tilde_roots, "/Users/maria/Work", home),
            Some(r#"["~/Repos","~/Work"]"#.to_string())
        );
    }

    #[test]
    fn add_folder_writes_roots() {
        let mut app = testing::app(signed_out_snap(Some(with_clones())));
        app.insert_resource(UserHome(Some("/Users/maria".into())));
        testing::settle(&mut app);
        let add = testing::find::<AddFolder>(&mut app, |_| true);

        app.insert_resource(FolderPicker(Box::new(|| {
            Some(PathBuf::from("/Users/maria/Clones"))
        })));
        testing::activate(&mut app, add);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::SetConfig {
                key: "repositories.roots".into(),
                value: r#"["~/Repos","~/Projects","~/src","~/code","~/Clones"]"#.into()
            }]
        );

        // Cancelling the dialog and picking a folder that is already listed change nothing.
        app.insert_resource(FolderPicker(Box::new(|| None)));
        testing::activate(&mut app, add);
        app.insert_resource(FolderPicker(Box::new(|| {
            Some(PathBuf::from("/Users/maria/Repos"))
        })));
        testing::activate(&mut app, add);
        assert!(testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn harness_step_is_later() {
        let first_run = status(
            GithubLogin::SignedOut,
            Vec::new(),
            vec![claude(), absent_codex()],
        );
        let snap = signed_out_snap(Some(first_run));
        let v = view(&snap, &FirstRunUi::default(), None);
        assert_eq!(
            v.harnesses,
            [
                HarnessView {
                    name: "Claude Code",
                    found: true,
                    line: "Found · /opt/homebrew/bin/claude".into()
                },
                HarnessView {
                    name: "Codex",
                    found: false,
                    line: "Not found on your PATH".into()
                },
            ]
        );
        let mut app = testing::app(snap);
        testing::settle(&mut app);
        let mut names: Vec<&str> = {
            let mut q = app.world_mut().query::<&HarnessCard>();
            q.iter(app.world()).map(|c| c.0).collect()
        };
        names.sort_unstable();
        assert_eq!(names, ["Claude Code", "Codex", "Custom command"]);
        let card = testing::find::<HarnessCard>(&mut app, |c| c.0 == "Claude Code");
        testing::activate(&mut app, card);
        assert!(
            testing::recorded(&mut app).is_empty(),
            "nothing connects before the agent exists"
        );
    }
}
