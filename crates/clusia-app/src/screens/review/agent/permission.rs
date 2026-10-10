//! Permission requests (mockup `Permission.png`): the queue of what the daemon is waiting to
//! have answered, the modal that asks, and the chat lines for how each request ended.
//!
//! The daemon decides and always denies on silence; this side only shows the exact request and
//! forwards the reviewer's answer. The first answer is the only one sent, and the modal closes
//! on the daemon's `PermissionResolved`, wherever the answer came from. What it shows comes
//! from the agent, so its control and invisible characters are shown as escapes: what the
//! reviewer reads is what would run.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input::ButtonInput;
use bevy::input_focus::InputFocus;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, observe};
use clusia_core::PrRef;
use clusia_core::printable::{printable, printable_lines};
use clusia_protocol::{PermissionAnswerKind, PermissionOutcome};

use super::model::Chats;
use crate::bridge::{Ask, Asks, Connection, Model, PermissionRequest, PermissionTell};
use crate::clock::Clock;
use crate::fonts::UiFonts;
use crate::nav::{Nav, Screen};
use crate::review_state::ReviewTabs;
use crate::screens::review::ReviewSystems;
use crate::theme::Swatch;
use crate::ui::kit::{Type, Variant, button, disabled_button, panel, text};
use crate::ui::modal::{escape_pressed, modal_card, modal_root};

/// Enter allows once only after the modal has been on screen this long, so an Enter meant for
/// something else (sending a chat message, a dialog that just closed) cannot allow a command.
pub const ENTER_GRACE_SECS: f64 = 0.7;

/// No answer counts in this long after a modal appears (a click, Esc), so a press aimed at the
/// request before it, or at what was on screen, never lands on the new one.
pub const ANSWER_QUIET_SECS: f64 = 0.3;

/// A request waiting for an answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub request: PermissionRequest,
    /// When this window first saw it (Unix ms): the bar's full length runs from here.
    pub asked_ms: i64,
    /// An answer was sent; the modal waits for the daemon to resolve it.
    pub answered: bool,
}

/// Every request this window has heard of and not seen resolved, oldest first.
#[derive(Resource, Debug, Default)]
pub struct PermissionQueue(pub Vec<Pending>);

impl PermissionQueue {
    pub fn push(&mut self, request: PermissionRequest, now_ms: i64) {
        if self.0.iter().any(|p| p.request.id == request.id) {
            return;
        }
        self.0.push(Pending {
            request,
            asked_ms: now_ms,
            answered: false,
        });
    }

    pub fn get(&self, id: &str) -> Option<&Pending> {
        self.0.iter().find(|p| p.request.id == id)
    }

    /// The oldest request of `pr`.
    pub fn front(&self, pr: &PrRef) -> Option<&Pending> {
        self.0.iter().find(|p| &p.request.pr == pr)
    }

    /// How many requests of `pr` wait behind the front one.
    pub fn others(&self, pr: &PrRef) -> usize {
        self.0
            .iter()
            .filter(|p| &p.request.pr == pr)
            .count()
            .saturating_sub(1)
    }

    /// Marks `id` answered; false when it is unknown or was already answered.
    pub fn mark_answered(&mut self, id: &str) -> bool {
        match self.0.iter_mut().find(|p| p.request.id == id) {
            Some(p) if !p.answered => {
                p.answered = true;
                true
            }
            _ => false,
        }
    }

    pub fn resolve(&mut self, id: &str) -> Option<Pending> {
        let at = self.0.iter().position(|p| p.request.id == id)?;
        Some(self.0.remove(at))
    }

    /// Applies what the daemon said; a finished request becomes a chat line, and the list of what
    /// waits for a review replaces what this window held for it (a repeated id is one request).
    pub fn apply(&mut self, tell: PermissionTell, chats: &mut Chats, now_ms: i64) {
        match tell {
            PermissionTell::Requested(request) => self.push(request, now_ms),
            PermissionTell::Resolved {
                id,
                pr,
                tool,
                summary,
                outcome,
                ..
            } => {
                // A request that was answered here is gone from the queue; one that was decided
                // at once (a rule covered it, or it was outside the worktree) never was in it.
                // The line comes from the event either way.
                self.resolve(&id);
                chats.permission(&pr, &tool, &summary, outcome);
            }
            PermissionTell::Rules { pr, rules } => chats.set_rules(&pr, rules),
            // The daemon's list is the truth for its review, in its order: a request it no longer
            // lists is over, and one heard live before the list keeps what happened to it here.
            PermissionTell::Pending { pr, requests } => {
                let (held, mut kept): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.0)
                    .into_iter()
                    .partition(|p| p.request.pr == pr);
                for request in requests {
                    if kept.iter().any(|p| p.request.id == request.id) {
                        continue;
                    }
                    let before = held.iter().find(|p| p.request.id == request.id);
                    kept.push(Pending {
                        asked_ms: before.map_or(now_ms, |p| p.asked_ms),
                        answered: before.is_some_and(|p| p.answered),
                        request,
                    });
                }
                self.0 = kept;
            }
            PermissionTell::AnswerRefused { id } => {
                if let Some(p) = self.0.iter_mut().find(|p| p.request.id == id) {
                    p.answered = false;
                }
            }
        }
    }
}

/// The root layer of the modal asking for `id`.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct PermissionModal {
    pub id: String,
    /// `Time::elapsed_secs_f64` when it appeared.
    pub shown_at: f64,
}

/// The card of the modal: its content is rebuilt when what it shows changes.
#[derive(Component, Debug)]
pub struct PermissionBody {
    id: String,
    built: Option<(ModalView, bool)>,
}

/// A button of the modal: Deny, Allow for this review, Allow once.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct AnswerButton {
    pub id: String,
    pub answer: PermissionAnswerKind,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CountdownText(pub String);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CountdownBar(pub String);

/// What the modal shows for one request.
#[derive(Debug, Clone, PartialEq)]
pub struct ModalView {
    pub title: String,
    pub reason: Option<String>,
    pub code: String,
    pub context: String,
    /// What the tool would change: Edit's old → new, Write's first lines.
    pub detail: Option<String>,
    pub allow_review: Option<String>,
    pub more: usize,
}

pub fn title_for(tool: &str) -> String {
    match tool {
        "Bash" => "Claude Code wants to run a command".into(),
        "Edit" | "MultiEdit" | "NotebookEdit" => "Claude Code wants to edit a file".into(),
        "Write" => "Claude Code wants to write a file".into(),
        other => format!("Claude Code wants to use {}", printable(other)),
    }
}

/// The middle button's label; none when the request has nothing to grant for the review.
pub fn review_label(tool: &str, prefix: Option<&str>) -> Option<String> {
    let prefix = prefix?;
    Some(if tool == "Bash" {
        format!("Allow {prefix} … for this review")
    } else {
        format!("Allow {prefix} for this review")
    })
}

/// `worktrees/rzorzal~clusia~123` for a worktree path, whatever its parent folders.
pub fn worktree_label(path: Option<&str>) -> String {
    path.and_then(|p| p.trim_end_matches('/').rsplit('/').next())
        .filter(|name| !name.is_empty())
        .map_or_else(|| "the worktree".to_string(), |n| format!("worktrees/{n}"))
}

/// Where the command runs and what it can reach, as the daemon really runs it.
pub fn context_line(sandbox: bool, worktree: Option<&str>) -> String {
    let at = worktree_label(worktree);
    if sandbox {
        format!("in {at} · no network · reads and writes only the worktree")
    } else {
        format!("in {at} · network allowed · can write outside the worktree")
    }
}

/// `1:52`: whole seconds left, rounded up so the text never says zero before the daemon denies.
pub fn countdown_text(remaining_ms: i64) -> String {
    let secs = (remaining_ms.max(0) + 999) / 1000;
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// How much of the bar is left, from 1 (just asked) to 0 (the deadline).
pub fn countdown_fraction(deadline: i64, asked_ms: i64, now_ms: i64) -> f32 {
    let total = (deadline - asked_ms).max(1) as f32;
    (((deadline - now_ms).max(0)) as f32 / total).clamp(0.0, 1.0)
}

/// The chat line of a finished request: its mark, whether it went through, and its text. For
/// the tools that have no command or path, `summary` is the tool's name.
pub fn permission_line(
    tool: &str,
    summary: &str,
    outcome: PermissionOutcome,
) -> (&'static str, bool, String) {
    // The words of the terminal's `clusia ask` and `clusia agent log` (`clusia::agent::permission_line`).
    let summary = &printable(summary);
    let verb = match tool {
        "Bash" => "ran",
        "Write" => "wrote",
        "Edit" | "MultiEdit" | "NotebookEdit" => "edited",
        _ => "used",
    };
    match outcome {
        PermissionOutcome::Allowed => ("✓", true, format!("{verb} {summary} (you allowed it)")),
        PermissionOutcome::AllowedForReview => (
            "✓",
            true,
            format!("{verb} {summary} (allowed for this review)"),
        ),
        PermissionOutcome::Denied => ("⊘", false, format!("Denied: {summary}")),
        PermissionOutcome::Expired => ("⊘", false, format!("denied {summary}: no answer in time")),
        PermissionOutcome::Cancelled => {
            ("⊘", false, format!("{summary} was not run: the turn ended"))
        }
    }
}

/// A rule as the footer's chip says it: `Bash(cargo test:*)` is `cargo test`.
pub fn rule_label(rule: &str) -> String {
    if let Some(prefix) = rule
        .strip_prefix("Bash(")
        .and_then(|r| r.strip_suffix(":*)"))
    {
        return printable(prefix);
    }
    match rule {
        "Edit" => "Edits in the worktree".into(),
        "Write" => "Writes in the worktree".into(),
        other => format!("{} in the worktree", printable(other)),
    }
}

/// Whether an Enter now allows once: the modal has been up a moment, and nobody is typing.
pub fn enter_allows(now_secs: f64, shown_at: f64, typing: bool) -> bool {
    !typing && now_secs - shown_at >= ENTER_GRACE_SECS
}

/// Whether a click or Esc now answers the modal shown at `shown_at`.
pub fn answer_counts(now_secs: f64, shown_at: f64) -> bool {
    now_secs - shown_at >= ANSWER_QUIET_SECS
}

pub fn modal_view(pending: &Pending, worktree: Option<&str>, more: usize) -> ModalView {
    let r = &pending.request;
    ModalView {
        title: title_for(&r.tool),
        reason: r
            .reason
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(printable),
        code: printable(&r.summary),
        context: context_line(r.sandbox, worktree),
        detail: r
            .detail
            .as_deref()
            .filter(|d| !d.trim().is_empty())
            .map(printable_lines),
        allow_review: review_label(
            &printable(&r.tool),
            r.prefix.as_deref().map(printable).as_deref(),
        ),
        more,
    }
}

pub struct PermissionPlugin;

impl Plugin for PermissionPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PermissionQueue>()
            // Before the dialogs and the palette that close on Esc, so the press that answers
            // is theirs no more.
            .add_systems(
                Update,
                escape_denies
                    .before(ReviewSystems)
                    .before(crate::screens::open_pr::palette_keys),
            )
            .add_systems(
                Update,
                (
                    drop_when_lost,
                    sync_modal,
                    rebuild_body,
                    tick_countdown,
                    allow_with_enter,
                )
                    .chain()
                    .after(ReviewSystems),
            );
    }
}

/// Sends `answer` for `id` unless one was already sent.
fn send_answer(
    queue: &mut PermissionQueue,
    asks: &mut Asks,
    id: &str,
    answer: PermissionAnswerKind,
) {
    if queue.mark_answered(id) {
        asks.send(Ask::PermissionAnswer {
            id: id.to_string(),
            answer,
        });
    }
}

/// Without the daemon nobody can answer: its bridge is gone and it denies what it held.
fn drop_when_lost(model: Res<Model>, mut queue: ResMut<PermissionQueue>) {
    if model.connection != Connection::Live && !queue.0.is_empty() {
        queue.0.clear();
    }
}

/// One modal, for the front request of the review on screen.
fn sync_modal(
    mut commands: Commands,
    nav: Res<Nav>,
    queue: Res<PermissionQueue>,
    time: Res<Time>,
    modals: Query<(Entity, &PermissionModal)>,
) {
    let want = match &nav.screen {
        Screen::Review(pr) => queue.front(pr).map(|p| p.request.id.clone()),
        _ => None,
    };
    for (entity, modal) in &modals {
        if Some(&modal.id) != want.as_ref() {
            commands.entity(entity).despawn();
        }
    }
    let Some(id) = want else { return };
    if modals.iter().any(|(_, m)| m.id == id) {
        return;
    }
    commands
        .spawn((
            modal_root(),
            PermissionModal {
                id: id.clone(),
                shown_at: time.elapsed_secs_f64(),
            },
        ))
        // Above the other modals: a request must not hide behind a dialog the reviewer opened.
        .insert(GlobalZIndex(110))
        .with_children(|root| {
            root.spawn((modal_card(560.0), PermissionBody { id, built: None }));
        });
}

fn rebuild_body(
    mut commands: Commands,
    queue: Res<PermissionQueue>,
    tabs: Res<ReviewTabs>,
    clock: Res<Clock>,
    fonts: Res<UiFonts>,
    mut bodies: Query<(Entity, &mut PermissionBody)>,
) {
    for (entity, mut body) in &mut bodies {
        let Some(pending) = queue.get(&body.id) else {
            continue;
        };
        let pr = &pending.request.pr;
        let worktree = tabs
            .0
            .get(pr)
            .and_then(|t| t.ready())
            .and_then(|r| r.view.worktree.as_deref());
        let view = modal_view(pending, worktree, queue.others(pr));
        let key = (view, pending.answered);
        if body.built.as_ref() == Some(&key) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        commands.entity(entity).with_children(|card| {
            content(card, &fonts, pending, &key.0, clock.now_ms());
        });
        body.built = Some(key);
    }
}

fn content(
    card: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pending: &Pending,
    v: &ModalView,
    now_ms: i64,
) {
    let id = &pending.request.id;
    card.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(14),
        padding: UiRect::axes(px(24), px(22)),
        ..default()
    })
    .with_children(|b| {
        b.spawn(Node {
            column_gap: px(14),
            align_items: AlignItems::FlexStart,
            ..default()
        })
        .with_children(|head| {
            head.spawn(panel(
                Node {
                    width: px(36),
                    height: px(36),
                    flex_shrink: 0.0,
                    align_items: AlignItems::Center,
                    justify_content: JustifyContent::Center,
                    border_radius: BorderRadius::all(px(8)),
                    ..default()
                },
                Swatch::OrangeSoft,
            ))
            .with_children(|icon| {
                icon.spawn(text(fonts, "!", Type::HEADING.ink(Swatch::Orange)));
            });
            head.spawn(Node {
                flex_grow: 1.0,
                min_width: px(0),
                flex_direction: FlexDirection::Column,
                row_gap: px(2),
                ..default()
            })
            .with_children(|t| {
                t.spawn(text(fonts, v.title.clone(), Type::HEADING));
                if let Some(reason) = &v.reason {
                    t.spawn(text(fonts, format!("“{reason}”"), Type::MUTED));
                }
            });
        });
        b.spawn(panel(
            Node {
                padding: UiRect::axes(px(12), px(10)),
                border_radius: BorderRadius::all(px(6)),
                ..default()
            },
            Swatch::Chrome,
        ))
        .with_children(|code| {
            code.spawn(text(fonts, v.code.clone(), Type::MONO));
        });
        if let Some(detail) = &v.detail {
            b.spawn((
                panel(
                    Node {
                        max_height: px(160),
                        padding: UiRect::axes(px(12), px(10)),
                        border_radius: BorderRadius::all(px(6)),
                        overflow: Overflow::clip(),
                        ..default()
                    },
                    Swatch::Chrome,
                ),
                children![text(fonts, detail.clone(), Type::MONO)],
            ));
        }
        b.spawn(text(fonts, v.context.clone(), Type::META));
        b.spawn(Node {
            column_gap: px(12),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|bar| {
            bar.spawn(panel(
                Node {
                    flex_grow: 1.0,
                    height: px(4),
                    border_radius: BorderRadius::all(px(2)),
                    ..default()
                },
                Swatch::Line,
            ))
            .with_children(|track| {
                let left = countdown_fraction(pending.request.deadline, pending.asked_ms, now_ms);
                track.spawn((
                    CountdownBar(id.clone()),
                    panel(
                        Node {
                            width: percent(left * 100.0),
                            height: percent(100),
                            border_radius: BorderRadius::all(px(2)),
                            ..default()
                        },
                        Swatch::Orange,
                    ),
                ));
            });
            bar.spawn((
                CountdownText(id.clone()),
                text(
                    fonts,
                    format!(
                        "Denied automatically in {}",
                        countdown_text(pending.request.deadline - now_ms)
                    ),
                    Type::META,
                ),
            ));
        });
    });
    card.spawn((
        Node {
            column_gap: px(8),
            align_items: AlignItems::Center,
            padding: UiRect::axes(px(24), px(14)),
            border: UiRect::top(px(1)),
            ..default()
        },
        BackgroundColor::default(),
        crate::ui::kit::Fill(Swatch::Chrome),
        BorderColor::default(),
        crate::ui::kit::Stroke(Swatch::Line),
    ))
    .with_children(|f| {
        let answered = pending.answered;
        let make = |f: &mut ChildSpawnerCommands, label: &str, variant, answer| {
            if answered {
                f.spawn(disabled_button(fonts, label));
            } else {
                f.spawn((
                    button(fonts, label, variant),
                    AnswerButton {
                        id: id.clone(),
                        answer,
                    },
                    observe(on_answer),
                ));
            }
        };
        make(f, "Deny", Variant::Danger, PermissionAnswerKind::Deny);
        if v.more > 0 {
            f.spawn(text(fonts, format!("{} more waiting", v.more), Type::META));
        }
        f.spawn(Node {
            flex_grow: 1.0,
            ..default()
        });
        if let Some(label) = &v.allow_review {
            make(f, label, Variant::Secondary, PermissionAnswerKind::Review);
        }
        make(
            f,
            "Allow once",
            Variant::Primary,
            PermissionAnswerKind::Once,
        );
    });
}

/// Keeps the text and the bar on the daemon's deadline; only a second's change touches the text.
fn tick_countdown(
    clock: Res<Clock>,
    queue: Res<PermissionQueue>,
    mut texts: Query<(&CountdownText, &mut Text)>,
    mut bars: Query<(&CountdownBar, &mut Node)>,
) {
    let now = clock.now_ms();
    for (CountdownText(id), mut label) in &mut texts {
        let Some(p) = queue.get(id) else { continue };
        let shown = format!(
            "Denied automatically in {}",
            countdown_text(p.request.deadline - now)
        );
        if label.0 != shown {
            label.0 = shown;
        }
    }
    for (CountdownBar(id), mut node) in &mut bars {
        let Some(p) = queue.get(id) else { continue };
        let width = percent(countdown_fraction(p.request.deadline, p.asked_ms, now) * 100.0);
        if node.width != width {
            node.width = width;
        }
    }
}

/// Esc denies. The press is taken even when it does not count (the quiet moments), so it never
/// also closes the dialog under the modal.
fn escape_denies(
    mut keys: ResMut<ButtonInput<KeyCode>>,
    time: Res<Time>,
    modals: Query<&PermissionModal>,
    mut queue: ResMut<PermissionQueue>,
    mut asks: ResMut<Asks>,
) {
    let Some(modal) = modals.iter().next() else {
        return;
    };
    if !escape_pressed(&keys) {
        return;
    }
    keys.clear_just_pressed(KeyCode::Escape);
    if answer_counts(time.elapsed_secs_f64(), modal.shown_at) {
        send_answer(&mut queue, &mut asks, &modal.id, PermissionAnswerKind::Deny);
    }
}

/// Enter allows once, under `enter_allows`. Keyboard focus anywhere but the modal (a chat input,
/// a button behind it that Enter would also activate) counts as someone else using the key.
fn allow_with_enter(
    keys: Res<ButtonInput<KeyCode>>,
    time: Res<Time>,
    focus: Res<InputFocus>,
    parents: Query<&ChildOf>,
    modals: Query<(Entity, &PermissionModal)>,
    mut queue: ResMut<PermissionQueue>,
    mut asks: ResMut<Asks>,
) {
    let Some((root, modal)) = modals.iter().next() else {
        return;
    };
    if !answer_counts(time.elapsed_secs_f64(), modal.shown_at) {
        return;
    }
    let enter = keys.just_pressed(KeyCode::Enter) || keys.just_pressed(KeyCode::NumpadEnter);
    let elsewhere = focus.get().is_some_and(|e| {
        !std::iter::successors(Some(e), |e| parents.get(*e).ok().map(ChildOf::parent))
            .any(|e| e == root)
    });
    if enter && enter_allows(time.elapsed_secs_f64(), modal.shown_at, elsewhere) {
        send_answer(&mut queue, &mut asks, &modal.id, PermissionAnswerKind::Once);
    }
}

fn on_answer(
    activate: On<Activate>,
    buttons: Query<&AnswerButton>,
    modals: Query<&PermissionModal>,
    time: Res<Time>,
    mut queue: ResMut<PermissionQueue>,
    mut asks: ResMut<Asks>,
) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    let seen = modals
        .iter()
        .any(|m| m.id == b.id && answer_counts(time.elapsed_secs_f64(), m.shown_at));
    if seen {
        send_answer(&mut queue, &mut asks, &b.id, b.answer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{PermissionTell, Tell};
    use crate::fixture;
    use crate::screens::review::agent::panel::RevokeButton;
    use crate::screens::review::agent::{ChatLine, Chats, PanelTab};
    use crate::testing::{self, NOW};
    use bevy::input::ButtonInput;
    use bevy::time::TimeUpdateStrategy;
    use std::time::Duration;

    fn pr() -> PrRef {
        fixture::demo_pr()
    }

    fn request(id: &str, tool: &str, summary: &str) -> PermissionRequest {
        PermissionRequest {
            id: id.into(),
            pr: pr(),
            turn: 3,
            tool: tool.into(),
            summary: summary.into(),
            reason: Some(
                "To check that two refreshes at once don't exchange the token twice.".into(),
            ),
            prefix: Some(if tool == "Bash" { "cargo test" } else { tool }.into()),
            sandbox: true,
            deadline: NOW * 1000 + 112_000,
            detail: None,
            origin: clusia_protocol::TurnOrigin::Chat,
        }
    }

    fn bash(id: &str) -> PermissionRequest {
        request(
            id,
            "Bash",
            "cargo test -p clusia-auth refresh_race -- --nocapture",
        )
    }

    fn ask(app: &mut App, request: PermissionRequest) {
        testing::tell(app, Tell::Permission(PermissionTell::Requested(request)));
        testing::settle(app);
    }

    fn open() -> App {
        let mut app = testing::app(fixture::demo(NOW));
        // Frames take no time: the quiet moments after a modal appears last until `age`.
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::ZERO));
        testing::open_ready(&mut app, false);
        app.world_mut().resource_mut::<Chats>().entry(&pr());
        app
    }

    const COMMAND: &str = "cargo test -p clusia-auth refresh_race -- --nocapture";

    fn resolve(app: &mut App, id: &str, outcome: PermissionOutcome) {
        resolve_as(app, id, "Bash", COMMAND, outcome);
    }

    fn resolve_as(app: &mut App, id: &str, tool: &str, summary: &str, outcome: PermissionOutcome) {
        testing::tell(
            app,
            Tell::Permission(PermissionTell::Resolved {
                id: id.into(),
                pr: pr(),
                tool: tool.into(),
                summary: summary.into(),
                outcome,
                origin: clusia_protocol::TurnOrigin::Chat,
            }),
        );
        testing::settle(app);
    }

    fn press(app: &mut App, key: KeyCode) {
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(key);
        app.update();
        let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
        keys.release(key);
        keys.reset_all();
    }

    /// The modal on screen has been there a while: answers to it count.
    fn age(app: &mut App) {
        let modal = testing::find::<PermissionModal>(app, |_| true);
        app.world_mut()
            .get_mut::<PermissionModal>(modal)
            .unwrap()
            .shown_at = -10.0;
    }

    /// The texts inside the modal (the review behind it has its own, `head → base` among them).
    fn modal_texts(app: &mut App) -> Vec<String> {
        let mut texts = app.world_mut().query::<(Entity, &Text)>();
        let world = app.world();
        texts
            .iter(world)
            .filter(|(e, _)| {
                std::iter::successors(Some(*e), |e| world.get::<ChildOf>(*e).map(|c| c.parent()))
                    .any(|e| world.get::<PermissionModal>(e).is_some())
            })
            .map(|(_, t)| t.0.clone())
            .collect()
    }

    fn answer_button(app: &mut App, answer: PermissionAnswerKind) -> Entity {
        testing::find::<AnswerButton>(app, |b| b.answer == answer)
    }

    #[test]
    fn titles_follow_the_tool() {
        assert_eq!(title_for("Bash"), "Claude Code wants to run a command");
        assert_eq!(title_for("Edit"), "Claude Code wants to edit a file");
        assert_eq!(title_for("MultiEdit"), "Claude Code wants to edit a file");
        assert_eq!(
            title_for("NotebookEdit"),
            "Claude Code wants to edit a file"
        );
        assert_eq!(title_for("Write"), "Claude Code wants to write a file");
        assert_eq!(title_for("WebFetch"), "Claude Code wants to use WebFetch");
    }

    #[test]
    fn the_review_button_names_what_it_would_allow() {
        assert_eq!(
            review_label("Bash", Some("cargo test")).as_deref(),
            Some("Allow cargo test … for this review")
        );
        assert_eq!(
            review_label("Edit", Some("Edit")).as_deref(),
            Some("Allow Edit for this review")
        );
        assert_eq!(review_label("Bash", None), None);
    }

    #[test]
    fn the_context_line_tells_the_real_sandbox_state() {
        assert_eq!(
            context_line(true, Some("/data/worktrees/rzorzal~clusia~123")),
            "in worktrees/rzorzal~clusia~123 · no network · reads and writes only the worktree"
        );
        assert_eq!(
            context_line(false, Some("/data/worktrees/rzorzal~clusia~123/")),
            "in worktrees/rzorzal~clusia~123 · network allowed · can write outside the worktree"
        );
        assert_eq!(
            context_line(true, None),
            "in the worktree · no network · reads and writes only the worktree"
        );
        assert_eq!(worktree_label(Some("")), "the worktree");
    }

    #[test]
    fn the_countdown_counts_to_the_daemons_deadline() {
        assert_eq!(countdown_text(112_000), "1:52");
        assert_eq!(countdown_text(111_200), "1:52", "rounds up: never early");
        assert_eq!(countdown_text(59_000), "0:59");
        assert_eq!(countdown_text(600_000), "10:00");
        assert_eq!(countdown_text(0), "0:00");
        assert_eq!(countdown_text(-5_000), "0:00");
        assert_eq!(
            countdown_fraction(1_000_120_000, 1_000_000_000, 1_000_000_000),
            1.0
        );
        assert_eq!(
            countdown_fraction(1_000_120_000, 1_000_000_000, 1_000_060_000),
            0.5
        );
        assert_eq!(
            countdown_fraction(1_000_120_000, 1_000_000_000, 1_000_200_000),
            0.0
        );
        assert_eq!(
            countdown_fraction(5, 5, 0),
            1.0,
            "a zero length is not a division by zero"
        );
    }

    #[test]
    fn every_outcome_has_its_chat_line() {
        use PermissionOutcome::*;
        let line = |tool, outcome| permission_line(tool, "cargo test", outcome);
        assert_eq!(
            line("Bash", Allowed),
            ("✓", true, "ran cargo test (you allowed it)".into())
        );
        assert_eq!(
            line("Bash", AllowedForReview),
            ("✓", true, "ran cargo test (allowed for this review)".into())
        );
        assert_eq!(
            line("Bash", Denied),
            ("⊘", false, "Denied: cargo test".into())
        );
        assert_eq!(
            line("Bash", Expired),
            ("⊘", false, "denied cargo test: no answer in time".into())
        );
        assert_eq!(
            line("Bash", Cancelled),
            ("⊘", false, "cargo test was not run: the turn ended".into())
        );
        assert_eq!(
            permission_line("Edit", "src/lib.rs", Allowed).2,
            "edited src/lib.rs (you allowed it)"
        );
        assert_eq!(
            permission_line("Write", "notes.md", Allowed).2,
            "wrote notes.md (you allowed it)"
        );
        assert_eq!(
            permission_line("WebFetch", "WebFetch", Allowed).2,
            "used WebFetch (you allowed it)"
        );
    }

    #[test]
    fn rules_read_as_what_they_allow() {
        assert_eq!(rule_label("Bash(cargo test:*)"), "cargo test");
        assert_eq!(rule_label("Bash(make:*)"), "make");
        assert_eq!(rule_label("Edit"), "Edits in the worktree");
        assert_eq!(rule_label("Write"), "Writes in the worktree");
        assert_eq!(rule_label("NotebookEdit"), "NotebookEdit in the worktree");
    }

    #[test]
    fn enter_needs_a_quiet_keyboard_and_a_modal_that_has_been_there_a_moment() {
        assert!(enter_allows(10.0, 5.0, false));
        assert!(!enter_allows(10.0, 9.8, false), "inside the grace period");
        assert!(!enter_allows(10.0, 5.0, true), "someone is typing");
        assert!(enter_allows(5.0 + ENTER_GRACE_SECS, 5.0, false));
    }

    #[test]
    fn the_queue_keeps_order_per_review_and_answers_once() {
        let other: PrRef = "rzorzal/other#1".parse().unwrap();
        let mut queue = PermissionQueue::default();
        queue.push(bash("a"), 1_000);
        queue.push(bash("a"), 2_000);
        let mut elsewhere = bash("b");
        elsewhere.pr = other.clone();
        queue.push(elsewhere, 2_000);
        queue.push(bash("c"), 3_000);
        assert_eq!(queue.0.len(), 3, "a repeated id is one request");
        assert_eq!(queue.front(&pr()).unwrap().request.id, "a");
        assert_eq!(queue.front(&other).unwrap().request.id, "b");
        assert_eq!(queue.others(&pr()), 1);
        assert!(queue.mark_answered("a"));
        assert!(!queue.mark_answered("a"), "the first answer wins");
        assert!(!queue.mark_answered("nope"));
        let gone = queue.resolve("a").expect("pending");
        assert_eq!(gone.request.id, "a");
        assert!(queue.resolve("a").is_none());
        assert_eq!(queue.front(&pr()).unwrap().request.id, "c");
        assert_eq!(queue.others(&pr()), 0);
    }

    #[test]
    fn a_request_opens_the_modal_with_the_mockup_text() {
        let mut app = open();
        assert_eq!(testing::count::<PermissionModal>(&mut app), 0);
        ask(&mut app, bash("perm-1"));
        assert_eq!(testing::count::<PermissionModal>(&mut app), 1);
        for needle in [
            "Claude Code wants to run a command",
            "“To check that two refreshes at once don't exchange the token twice.”",
            "cargo test -p clusia-auth refresh_race -- --nocapture",
            "no network · reads and writes only the worktree",
            "Denied automatically in 1:52",
            "Deny",
            "Allow cargo test … for this review",
            "Allow once",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        assert!(!testing::shows(&mut app, "more waiting"));
    }

    #[test]
    fn with_the_sandbox_off_the_modal_says_so() {
        let mut app = open();
        let mut request = bash("perm-1");
        request.sandbox = false;
        ask(&mut app, request);
        assert!(testing::shows(
            &mut app,
            "network allowed · can write outside the worktree"
        ));
        assert!(!testing::shows(&mut app, "no network"));
    }

    #[test]
    fn a_request_without_a_prefix_offers_only_allow_once() {
        let mut app = open();
        let mut request = bash("perm-1");
        request.prefix = None;
        request.reason = None;
        ask(&mut app, request);
        assert!(!testing::shows(&mut app, "for this review"));
        assert_eq!(
            testing::count::<AnswerButton>(&mut app),
            2,
            "Deny and Allow once"
        );
        assert!(!testing::shows(&mut app, "“"), "no quoted reason");
    }

    #[test]
    fn file_tools_get_their_own_titles_and_buttons() {
        let mut app = open();
        ask(&mut app, request("perm-1", "Edit", "src/auth/refresh.rs"));
        assert!(testing::shows(&mut app, "Claude Code wants to edit a file"));
        assert!(testing::shows(&mut app, "src/auth/refresh.rs"));
        assert!(testing::shows(&mut app, "Allow Edit for this review"));
    }

    #[test]
    fn each_button_sends_its_answer_once() {
        for (answer, label) in [
            (PermissionAnswerKind::Deny, "Deny"),
            (
                PermissionAnswerKind::Review,
                "Allow cargo test … for this review",
            ),
            (PermissionAnswerKind::Once, "Allow once"),
        ] {
            let mut app = open();
            ask(&mut app, bash("perm-1"));
            testing::recorded(&mut app);
            assert!(testing::shows(&mut app, label), "{label}");
            age(&mut app);
            let button = answer_button(&mut app, answer);
            testing::activate(&mut app, button);
            assert_eq!(
                testing::recorded(&mut app),
                [Ask::PermissionAnswer {
                    id: "perm-1".into(),
                    answer
                }]
            );
            // The modal stays until the daemon says it is resolved, and cannot be answered twice.
            testing::settle(&mut app);
            assert_eq!(testing::count::<PermissionModal>(&mut app), 1);
            assert_eq!(
                testing::count::<AnswerButton>(&mut app),
                0,
                "disabled look-alikes"
            );
            press(&mut app, KeyCode::Escape);
            assert!(
                testing::recorded(&mut app).is_empty(),
                "{answer:?} sent twice"
            );
        }
    }

    #[test]
    fn escape_denies_and_enter_allows_once_after_the_grace_period() {
        let mut app = open();
        ask(&mut app, bash("perm-1"));
        testing::recorded(&mut app);
        press(&mut app, KeyCode::Enter);
        assert!(
            testing::recorded(&mut app).is_empty(),
            "an Enter in the first moments allows nothing"
        );
        age(&mut app);
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::PermissionAnswer {
                id: "perm-1".into(),
                answer: PermissionAnswerKind::Once
            }]
        );
        press(&mut app, KeyCode::Escape);
        assert!(testing::recorded(&mut app).is_empty(), "already answered");

        let mut app = open();
        ask(&mut app, bash("perm-2"));
        testing::recorded(&mut app);
        age(&mut app);
        press(&mut app, KeyCode::Escape);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::PermissionAnswer {
                id: "perm-2".into(),
                answer: PermissionAnswerKind::Deny
            }]
        );
    }

    #[test]
    fn enter_does_not_allow_while_the_chat_input_has_the_keyboard() {
        let mut app = open();
        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .show(PanelTab::Agent);
        testing::settle(&mut app);
        ask(&mut app, bash("perm-1"));
        testing::recorded(&mut app);
        age(&mut app);
        let input = testing::find::<crate::screens::review::agent::ChatInput>(&mut app, |_| true);
        app.world_mut()
            .resource_mut::<bevy::input_focus::InputFocus>()
            .set(input, bevy::input_focus::FocusCause::Navigated);
        press(&mut app, KeyCode::Enter);
        assert!(
            testing::recorded(&mut app)
                .iter()
                .all(|a| !matches!(a, Ask::PermissionAnswer { .. })),
            "an Enter typed into the chat must never allow a command"
        );
    }

    #[test]
    fn enter_does_not_allow_while_a_widget_behind_the_modal_has_the_keyboard() {
        let mut app = open();
        ask(&mut app, bash("perm-1"));
        testing::recorded(&mut app);
        age(&mut app);
        let behind = app.world_mut().spawn(Node::default()).id();
        app.world_mut()
            .resource_mut::<bevy::input_focus::InputFocus>()
            .set(behind, bevy::input_focus::FocusCause::Navigated);
        press(&mut app, KeyCode::Enter);
        assert!(
            testing::recorded(&mut app).is_empty(),
            "an Enter that activates a button behind the modal must not also allow a command"
        );

        let own = answer_button(&mut app, PermissionAnswerKind::Deny);
        app.world_mut()
            .resource_mut::<bevy::input_focus::InputFocus>()
            .set(own, bevy::input_focus::FocusCause::Navigated);
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::PermissionAnswer {
                id: "perm-1".into(),
                answer: PermissionAnswerKind::Once
            }],
            "focus inside the modal is the modal's own"
        );
    }

    #[test]
    fn the_escape_that_denies_does_not_close_the_dialog_under_the_modal() {
        use crate::review_state::{Modal, ReviewTabs};
        let mut app = open();
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr())
            .unwrap()
            .ui
            .modal = Some(Modal::Finalize);
        testing::settle(&mut app);
        ask(&mut app, bash("perm-1"));
        testing::recorded(&mut app);
        age(&mut app);
        press(&mut app, KeyCode::Escape);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::PermissionAnswer {
                id: "perm-1".into(),
                answer: PermissionAnswerKind::Deny
            }]
        );
        assert_eq!(
            testing::tab(&app, &pr()).ui.modal,
            Some(Modal::Finalize),
            "one Escape answers one thing"
        );
    }

    #[test]
    fn what_the_daemon_lists_replaces_what_this_window_held() {
        let mut app = open();
        // A live request for a newer id is heard before the list that also holds older ones.
        ask(&mut app, request("new", "Write", "notes/new.md"));
        ask(&mut app, request("gone", "Write", "notes/gone.md"));
        let mut other = request("elsewhere", "Write", "notes/other.md");
        other.pr = PrRef::new("rzorzal", "other", 9).unwrap();
        app.world_mut()
            .resource_mut::<PermissionQueue>()
            .push(other, NOW);
        app.world_mut()
            .resource_mut::<PermissionQueue>()
            .mark_answered("new");
        testing::tell(
            &mut app,
            Tell::Permission(PermissionTell::Pending {
                pr: pr(),
                requests: vec![bash("old"), request("new", "Write", "notes/new.md")],
            }),
        );
        let queue = &app.world().resource::<PermissionQueue>().0;
        let held: Vec<(&str, bool)> = queue
            .iter()
            .map(|p| (p.request.id.as_str(), p.answered))
            .collect();
        assert_eq!(
            held,
            [("elsewhere", false), ("old", false), ("new", true)],
            "the daemon's order and its list for this review, the other review untouched"
        );
    }

    #[test]
    fn a_connection_that_drops_and_returns_at_once_leaves_no_dead_request() {
        let mut app = open();
        ask(&mut app, bash("perm-1"));
        let snapshot = app.world().resource::<Model>().snapshot.clone();
        let outbox = app.world().resource::<crate::bridge::Outbox>().0.clone();
        let _ = outbox.send(Tell::Lost("gone".into()));
        let _ = outbox.send(Tell::Snapshot(Box::new(snapshot)));
        app.update();
        assert!(app.world().resource::<PermissionQueue>().0.is_empty());
    }

    #[test]
    fn an_answer_the_daemon_refused_can_be_given_again() {
        let mut app = open();
        ask(&mut app, bash("perm-1"));
        testing::recorded(&mut app);
        age(&mut app);
        let once = answer_button(&mut app, PermissionAnswerKind::Once);
        testing::activate(&mut app, once);
        assert_eq!(testing::recorded(&mut app).len(), 1);
        testing::tell(
            &mut app,
            Tell::Permission(PermissionTell::AnswerRefused {
                id: "perm-1".into(),
            }),
        );
        testing::settle(&mut app);
        assert!(!app.world().resource::<PermissionQueue>().0[0].answered);
        let once = answer_button(&mut app, PermissionAnswerKind::Once);
        testing::activate(&mut app, once);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::PermissionAnswer {
                id: "perm-1".into(),
                answer: PermissionAnswerKind::Once
            }]
        );
    }

    #[test]
    fn a_request_resolved_elsewhere_closes_the_modal() {
        let mut app = open();
        ask(&mut app, bash("perm-1"));
        assert_eq!(testing::count::<PermissionModal>(&mut app), 1);
        resolve(&mut app, "perm-1", PermissionOutcome::Allowed);
        assert_eq!(testing::count::<PermissionModal>(&mut app), 0);
        assert!(app.world().resource::<PermissionQueue>().0.is_empty());
        let chats = app.world().resource::<Chats>();
        assert_eq!(
            chats.0[&pr()].lines.last(),
            Some(&ChatLine::Permission {
                tool: "Bash".into(),
                summary: "cargo test -p clusia-auth refresh_race -- --nocapture".into(),
                outcome: PermissionOutcome::Allowed,
            })
        );
    }

    #[test]
    fn decisions_made_at_once_still_reach_the_chat() {
        let mut app = open();
        // A rule covered it, or it was outside the worktree: the daemon never queued it.
        resolve_as(
            &mut app,
            "covered",
            "Bash",
            "cargo test -p clusia-auth",
            PermissionOutcome::AllowedForReview,
        );
        resolve_as(
            &mut app,
            "outside",
            "Write",
            "/etc/hosts",
            PermissionOutcome::Denied,
        );
        assert_eq!(testing::count::<PermissionModal>(&mut app), 0);
        let lines = &app.world().resource::<Chats>().0[&pr()].lines;
        assert_eq!(
            lines[lines.len() - 2..],
            [
                ChatLine::Permission {
                    tool: "Bash".into(),
                    summary: "cargo test -p clusia-auth".into(),
                    outcome: PermissionOutcome::AllowedForReview,
                },
                ChatLine::Permission {
                    tool: "Write".into(),
                    summary: "/etc/hosts".into(),
                    outcome: PermissionOutcome::Denied,
                },
            ]
        );
    }

    #[test]
    fn a_window_that_was_not_listening_learns_what_is_waiting() {
        let mut app = open();
        assert_eq!(testing::count::<PermissionModal>(&mut app), 0);
        // The daemon's `GetPermissions` answer, asked with the log when the review opened.
        let waiting = vec![bash("perm-1"), request("perm-2", "Write", "notes/plan.md")];
        let tell = |requests| Tell::Permission(PermissionTell::Pending { pr: pr(), requests });
        testing::tell(&mut app, tell(waiting.clone()));
        testing::settle(&mut app);
        assert_eq!(testing::count::<PermissionModal>(&mut app), 1);
        assert!(testing::shows(
            &mut app,
            "Claude Code wants to run a command"
        ));
        assert!(testing::shows(&mut app, "1 more waiting"));
        // Asking again (a reconnect) does not duplicate them, and an answer still works.
        testing::tell(&mut app, tell(waiting));
        testing::settle(&mut app);
        assert_eq!(app.world().resource::<PermissionQueue>().0.len(), 2);
        testing::recorded(&mut app);
        age(&mut app);
        let once = answer_button(&mut app, PermissionAnswerKind::Once);
        testing::activate(&mut app, once);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::PermissionAnswer {
                id: "perm-1".into(),
                answer: PermissionAnswerKind::Once
            }]
        );
    }

    #[test]
    fn the_modal_shows_the_detail_of_an_edit() {
        let mut app = open();
        let mut edit = request("perm-1", "Edit", "src/auth/refresh.rs");
        edit.detail = Some("let _guard = self.refresh_lock.lock().await;\n→\nlet _guard = self.refresh_lock.try_lock()?;".into());
        ask(&mut app, edit);
        assert!(testing::shows(
            &mut app,
            "let _guard = self.refresh_lock.lock().await;"
        ));
        assert!(testing::shows(
            &mut app,
            "let _guard = self.refresh_lock.try_lock()?;"
        ));
        // A command has none: just the command.
        let mut app = open();
        ask(&mut app, bash("perm-2"));
        assert!(
            !modal_texts(&mut app).iter().any(|t| t.contains('→')),
            "{:?}",
            modal_texts(&mut app)
        );
    }

    #[test]
    fn requests_queue_and_the_next_one_takes_over() {
        let mut app = open();
        ask(&mut app, bash("perm-1"));
        ask(&mut app, request("perm-2", "Write", "notes/plan.md"));
        assert_eq!(testing::count::<PermissionModal>(&mut app), 1);
        assert!(testing::shows(&mut app, "1 more waiting"));
        assert!(testing::shows(
            &mut app,
            "Claude Code wants to run a command"
        ));
        resolve(&mut app, "perm-1", PermissionOutcome::Denied);
        assert_eq!(testing::count::<PermissionModal>(&mut app), 1);
        assert!(testing::shows(
            &mut app,
            "Claude Code wants to write a file"
        ));
        assert!(!testing::shows(&mut app, "more waiting"));
        testing::recorded(&mut app);
        age(&mut app);
        let button = answer_button(&mut app, PermissionAnswerKind::Once);
        testing::activate(&mut app, button);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::PermissionAnswer {
                id: "perm-2".into(),
                answer: PermissionAnswerKind::Once
            }]
        );
    }

    #[test]
    fn the_countdown_follows_the_clock() {
        let mut app = open();
        ask(&mut app, bash("perm-1"));
        assert!(testing::shows(&mut app, "Denied automatically in 1:52"));
        let bar = testing::find::<CountdownBar>(&mut app, |_| true);
        assert_eq!(app.world().get::<Node>(bar).unwrap().width, percent(100.0));
        *app.world_mut().resource_mut::<Clock>() = Clock(Some(NOW + 56));
        app.update();
        assert!(testing::shows(&mut app, "Denied automatically in 0:56"));
        assert_eq!(app.world().get::<Node>(bar).unwrap().width, percent(50.0));
        *app.world_mut().resource_mut::<Clock>() = Clock(Some(NOW + 500));
        app.update();
        assert!(testing::shows(&mut app, "Denied automatically in 0:00"));
        assert_eq!(app.world().get::<Node>(bar).unwrap().width, percent(0.0));
    }

    #[test]
    fn a_request_waits_until_its_review_is_on_screen() {
        use crate::bridge::ShowRequested;
        use clusia_protocol::WindowTarget;
        let mut app = open();
        app.world_mut().resource_mut::<crate::nav::Nav>().screen = crate::nav::Screen::Home;
        testing::settle(&mut app);
        ask(&mut app, bash("perm-1"));
        assert_eq!(
            testing::count::<PermissionModal>(&mut app),
            0,
            "Home shows no modal"
        );
        assert_eq!(app.world().resource::<PermissionQueue>().0.len(), 1);
        // The tray notification's click shows the review.
        app.world_mut()
            .write_message(ShowRequested(WindowTarget::Review { pr: pr() }));
        testing::settle(&mut app);
        assert_eq!(testing::count::<PermissionModal>(&mut app), 1);
        app.world_mut().resource_mut::<crate::nav::Nav>().screen = crate::nav::Screen::Home;
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<PermissionModal>(&mut app),
            0,
            "leaving closes it, the queue keeps it"
        );
        assert_eq!(app.world().resource::<PermissionQueue>().0.len(), 1);
    }

    #[test]
    fn a_lost_connection_drops_every_request() {
        let mut app = open();
        ask(&mut app, bash("perm-1"));
        testing::tell(&mut app, Tell::Lost("clusiad closed the connection".into()));
        testing::settle(&mut app);
        assert_eq!(testing::count::<PermissionModal>(&mut app), 0);
        assert!(app.world().resource::<PermissionQueue>().0.is_empty());
    }

    #[test]
    fn the_chat_draws_the_permission_lines() {
        let mut app = open();
        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .show(PanelTab::Agent);
        for (id, outcome) in [
            ("a", PermissionOutcome::Allowed),
            ("b", PermissionOutcome::AllowedForReview),
            ("c", PermissionOutcome::Denied),
            ("d", PermissionOutcome::Expired),
            ("e", PermissionOutcome::Cancelled),
        ] {
            ask(&mut app, bash(id));
            resolve(&mut app, id, outcome);
        }
        for needle in [
            "ran cargo test -p clusia-auth refresh_race -- --nocapture (you allowed it)",
            "(allowed for this review)",
            "Denied: cargo test -p clusia-auth refresh_race -- --nocapture",
            "no answer in time",
            "was not run: the turn ended",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
    }

    #[test]
    fn the_footer_lists_the_rules_and_revokes_them() {
        let mut app = open();
        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .show(PanelTab::Agent);
        testing::settle(&mut app);
        assert!(!testing::shows(&mut app, "Allowed for this review"));
        testing::tell(
            &mut app,
            Tell::Permission(PermissionTell::Rules {
                pr: pr(),
                rules: vec!["Bash(cargo test:*)".into(), "Edit".into()],
            }),
        );
        testing::settle(&mut app);
        for needle in [
            "Allowed for this review",
            "cargo test",
            "Edits in the worktree",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        testing::recorded(&mut app);
        let revoke = testing::find::<RevokeButton>(&mut app, |b| b.rule == "Bash(cargo test:*)");
        testing::activate(&mut app, revoke);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::RevokeRule {
                pr: pr(),
                rule: "Bash(cargo test:*)".into()
            }]
        );
        testing::settle(&mut app);
        assert_eq!(
            app.world().resource::<Chats>().0[&pr()].rules,
            ["Edit"],
            "the chip goes at once; the daemon's RulesChanged confirms"
        );
        testing::tell(
            &mut app,
            Tell::Permission(PermissionTell::Rules {
                pr: pr(),
                rules: vec![],
            }),
        );
        testing::settle(&mut app);
        assert!(!testing::shows(&mut app, "Allowed for this review"));
    }

    #[test]
    fn the_footer_hint_says_it_asks() {
        let mut app = open();
        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .show(PanelTab::Agent);
        testing::settle(&mut app);
        assert!(testing::shows(
            &mut app,
            "Can read the worktree · asks before running commands"
        ));
    }

    #[test]
    fn answers_count_only_once_the_modal_has_been_there_a_moment() {
        assert!(!answer_counts(10.0, 9.8), "inside the quiet period");
        assert!(answer_counts(10.0, 9.0));
        assert!(answer_counts(10.31, 10.0));
        const { assert!(ANSWER_QUIET_SECS <= ENTER_GRACE_SECS) };
    }

    #[test]
    fn an_early_click_or_escape_answers_nothing() {
        let mut app = open();
        ask(&mut app, bash("perm-1"));
        ask(&mut app, request("perm-2", "Write", "notes/plan.md"));
        testing::recorded(&mut app);
        press(&mut app, KeyCode::Escape);
        let once = answer_button(&mut app, PermissionAnswerKind::Once);
        testing::activate(&mut app, once);
        assert!(
            testing::recorded(&mut app).is_empty(),
            "a press meant for what was there before allows nothing"
        );
        assert!(!app.world().resource::<PermissionQueue>().0[0].answered);
        // The modal is still there and answers once it has been seen.
        age(&mut app);
        let deny = answer_button(&mut app, PermissionAnswerKind::Deny);
        testing::activate(&mut app, deny);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::PermissionAnswer {
                id: "perm-1".into(),
                answer: PermissionAnswerKind::Deny
            }]
        );
        // The next request starts its own quiet moment.
        resolve(&mut app, "perm-1", PermissionOutcome::Denied);
        assert!(testing::shows(
            &mut app,
            "Claude Code wants to write a file"
        ));
        press(&mut app, KeyCode::Escape);
        let once = answer_button(&mut app, PermissionAnswerKind::Once);
        testing::activate(&mut app, once);
        assert!(testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn what_the_modal_and_the_chat_show_cannot_be_disguised() {
        let mut app = open();
        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .show(PanelTab::Agent);
        let mut sneaky = bash("perm-1");
        sneaky.summary = "echo ok #\u{202e}x\r\u{1b}[2Krm -rf ~".into();
        sneaky.reason = Some("tests\u{200b}".into());
        sneaky.detail = Some("old\n→\nnew\u{2028}x".into());
        ask(&mut app, sneaky);
        for needle in [
            "echo ok #\\u{202e}x\\r\\u{1b}[2Krm -rf ~",
            "“tests\\u{200b}”",
            "old\n→\nnew\\u{2028}x",
        ] {
            assert!(testing::shows(&mut app, needle), "{needle}");
        }
        assert!(
            !testing::shown_text(&mut app)
                .iter()
                .any(|t| t.contains('\u{202e}') || t.contains('\u{1b}') || t.contains('\r')),
            "nothing raw reaches the screen"
        );
        resolve_as(
            &mut app,
            "perm-1",
            "Bash",
            "echo ok #\u{202e}x",
            PermissionOutcome::Allowed,
        );
        assert!(testing::shows(
            &mut app,
            "ran echo ok #\\u{202e}x (you allowed it)"
        ));
        assert_eq!(
            permission_line("Bash", "a\u{2028}b", PermissionOutcome::Denied).2,
            "Denied: a\\u{2028}b"
        );
        assert_eq!(rule_label("Bash(c\u{200b}:*)"), "c\\u{200b}");
    }

    #[test]
    fn the_palette_under_the_modal_hears_no_keys() {
        use crate::nav::Nav;
        use crate::screens::open_pr::Palette;
        let palette = || Palette {
            open: true,
            query: "rzorzal/site#12".into(),
            cursor: 0,
        };
        let mut app = open();
        *app.world_mut().resource_mut::<Palette>() = palette();
        testing::settle(&mut app);
        ask(&mut app, bash("perm-1"));
        testing::recorded(&mut app);
        age(&mut app);
        let nav = app.world().resource::<Nav>().clone();
        press(&mut app, KeyCode::Escape);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::PermissionAnswer {
                id: "perm-1".into(),
                answer: PermissionAnswerKind::Deny
            }]
        );
        assert!(
            app.world().resource::<Palette>().open,
            "the Esc that denies does not also close the palette"
        );
        for key in [KeyCode::ArrowDown, KeyCode::Enter] {
            press(&mut app, key);
        }
        assert_eq!(*app.world().resource::<Palette>(), palette());
        assert_eq!(
            *app.world().resource::<Nav>(),
            nav,
            "Enter does not open a pull request from under the modal"
        );
        assert_eq!(testing::count::<PermissionModal>(&mut app), 1);
    }
}
