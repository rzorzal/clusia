//! The revalidation screen (mockup `Revalidated.png`): what Claude Code said about each comment
//! of a saved review once its pull request got new commits.
//!
//! The round comes from the daemon; everything in it is the agent's proposal, so what the
//! screen shows of it is made printable and drawn as plain text. Nothing here changes the draft
//! without a click: each button asks the daemon to decide one proposal.

use std::collections::{BTreeSet, HashMap};

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, ScrollArea, observe};
use clusia_core::printable::{printable, printable_lines};
use clusia_core::revalidation::{
    DecideError, Decision, Revalidation, RoundState, RowKind, Verdict, decidable, decide,
    fingerprint, pending, row_kind,
};
use clusia_core::{Anchor, DraftItem, FileDiff, ItemStatus, PrRef, Review};
use clusia_protocol::AnchorInput;

use crate::bridge::{Ask, Asks, Connection, Model, RevalidationTell, Toasts};
use crate::fonts::UiFonts;
use crate::review_state::{RevalView, ReviewTabs, Tab};
use crate::screens::review::ReviewSystems;
use crate::screens::review::agent::harness_ready;
use crate::screens::review::agent::suggestion::show_file;
use crate::screens::review::editor::{not_sent, read_only_reason};
use crate::screens::review::shell::{draft_card, file_name};
use crate::theme::Swatch;
use crate::ui::kit::{
    Stroke, Tone, Type, Variant, badge, button, card, disabled_button, panel, text,
};
use crate::ui::modal::escape_pressed;

/// One review's round as the window knows it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RevalidationModel {
    /// The round as the daemon last said it; `None` once the daemon says there is none.
    pub round: Option<Revalidation>,
    /// The daemon's answer arrived: until then the round stored in the review file is shown.
    pub loaded: bool,
    /// `GetRevalidation` was asked for.
    pub asked: bool,
    /// Items whose decision waits for the daemon.
    pub pending: BTreeSet<String>,
    /// *Accept all* waits for the daemon.
    pub all_pending: bool,
    /// Items whose *Keep and re-anchor* waits for the daemon: the Diff opens on its re-anchor
    /// mode once the decision is settled, not before.
    pub reanchor: BTreeSet<String>,
    /// Items whose new place waits for the daemon. Kept apart from `pending`: a round told
    /// meanwhile trims `pending` to the undecided proposals, and this mark must outlive it.
    pub reanchoring: BTreeSet<String>,
}

impl RevalidationModel {
    /// A `Settled` or `Refused` answer does not touch `round`: the daemon sends the new round
    /// (`Round`) before it settles a decision, and until one arrives `round_of` keeps showing
    /// what it showed.
    pub fn apply(&mut self, tell: &RevalidationTell) {
        match tell {
            RevalidationTell::Round { round, .. } => {
                self.round = round.clone();
                self.loaded = true;
                self.asked = true;
                // A re-anchor waits only for a decision still in flight; the round may arrive
                // before that decision's own answer, so what is in flight is checked first.
                let in_flight = self.pending.clone();
                self.reanchor.retain(|item| in_flight.contains(item));
                // A decision the round already shows is no longer in flight.
                let waiting: BTreeSet<String> = round
                    .iter()
                    .flat_map(|r| r.proposals.iter())
                    .filter(|p| p.decision == Decision::Pending)
                    .map(|p| p.item.clone())
                    .collect();
                self.pending.retain(|item| waiting.contains(item));
            }
            RevalidationTell::Settled { item, .. } => match item {
                Some(item) => {
                    self.pending.remove(item);
                }
                None => self.all_pending = false,
            },
            RevalidationTell::Refused { item, .. } => match item {
                Some(item) => {
                    self.pending.remove(item);
                    self.reanchor.remove(item);
                    // A refused re-anchor is a refusal too: the Diff takes another click.
                    self.reanchoring.remove(item);
                }
                None => self.all_pending = false,
            },
            RevalidationTell::Reanchored { item, .. } => {
                self.reanchoring.remove(item);
            }
        }
    }
}

/// Every review's round.
#[derive(Resource, Debug, Default)]
pub struct Revalidations(pub HashMap<PrRef, RevalidationModel>);

impl Revalidations {
    pub fn apply(&mut self, tell: &RevalidationTell) {
        self.0.entry(tell.pr().clone()).or_default().apply(tell);
    }

    /// Whether `item`'s *Keep and re-anchor* was waiting; it no longer is.
    pub fn take_reanchor(&mut self, pr: &PrRef, item: &str) -> bool {
        self.0.get_mut(pr).is_some_and(|m| m.reanchor.remove(item))
    }
}

/// Leaves the screen for the Diff on `item`'s file, where its new place is asked for.
pub fn enter_reanchor(tab: &mut Tab, item: &str) {
    let Some(ready) = tab.ready() else { return };
    let path = ready
        .view
        .review
        .draft
        .items
        .iter()
        .find(|i| i.id == item)
        .and_then(|i| i.anchor.as_ref())
        .map(|a| a.path.clone());
    tab.ui.reanchor = Some(item.to_string());
    tab.ui.revalidation = RevalView::Closed;
    match path {
        Some(path) => show_file(&mut tab.ui, &path),
        None => tab.ui.section = crate::review_state::ReviewSection::Diff,
    }
}

/// The round the window shows for `tab`: the daemon's once it answered, else the one stored in
/// the review file.
pub fn round_of<'a>(
    tab: &'a Tab,
    model: Option<&'a RevalidationModel>,
) -> Option<&'a Revalidation> {
    match model {
        Some(m) if m.loaded => m.round.as_ref(),
        _ => tab.ready()?.view.review.revalidation.as_ref(),
    }
}

/// Suggestions of the round that wait for a click.
pub fn pending_of(tab: &Tab, model: Option<&RevalidationModel>) -> usize {
    match (round_of(tab, model), tab.ready()) {
        (Some(round), Some(ready)) => pending(round, &ready.view.review.draft),
        _ => 0,
    }
}

/// Whether the screen takes the place of the review's tabs: a suggestion waits, or a round is
/// on its way (a scheduled or waiting one only with a harness to run it). A failed round keeps
/// the screen that already shows it, so *Try again* does not vanish. *Back to review* and
/// *Review them* override all of it.
pub fn screen_wanted(
    tab: &Tab,
    model: Option<&RevalidationModel>,
    mounted: bool,
    harness: bool,
) -> bool {
    let Some(round) = round_of(tab, model) else {
        return false;
    };
    match tab.ui.revalidation {
        RevalView::Closed => false,
        RevalView::Open => true,
        RevalView::Auto => {
            pending_of(tab, model) > 0
                || match round.state {
                    RoundState::Running { .. } => true,
                    RoundState::Scheduled { .. } | RoundState::Waiting => harness,
                    RoundState::Failed { .. } => mounted,
                    RoundState::Done => false,
                }
        }
    }
}

/// Whether a round told by the daemon brings back a screen the user closed: its head moved,
/// or it started running or waiting again (a new round, *Try again*, *Revalidate now*). The
/// first round seen never does, and neither does the activity line of a run in progress.
pub fn reopens(old: Option<&Revalidation>, new: &Revalidation) -> bool {
    let Some(old) = old else { return false };
    old.to_head != new.to_head
        || (std::mem::discriminant(&old.state) != std::mem::discriminant(&new.state)
            && matches!(
                new.state,
                RoundState::Running { .. } | RoundState::Scheduled { .. } | RoundState::Waiting
            ))
}

/// What a row's button asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowAction {
    pub label: &'static str,
    /// `true`: the agent's proposal is taken; `false`: it is refused.
    pub accept: bool,
    pub enabled: bool,
}

/// One comment of the round, as its row shows it. Every text is already printable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowView {
    pub item: String,
    pub kind: RowKind,
    pub label: &'static str,
    pub tone: Tone,
    /// `cache.rs:12`, or `cache.rs:40 → 44` for a move.
    pub place: String,
    pub body: String,
    /// The reason the agent gave, or why nothing could be checked.
    pub note: Option<String>,
    /// The agent's new wording.
    pub suggested: Option<String>,
    pub actions: Vec<RowAction>,
    /// What was decided, or why the buttons are off.
    pub outcome: Option<String>,
}

/// What a top button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopAction {
    SeeCommits,
    AcceptAll,
    Back,
    Stop,
    RevalidateNow,
    TryAgain,
    SetUp,
}

impl TopAction {
    pub fn label(self) -> &'static str {
        match self {
            TopAction::SeeCommits => "See the new commits",
            TopAction::AcceptAll => "Accept all",
            TopAction::Back => "Back to review",
            TopAction::Stop => "Stop",
            TopAction::RevalidateNow => "Revalidate now",
            TopAction::TryAgain => "Try again",
            TopAction::SetUp => "Set up a harness",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BannerView {
    pub title: String,
    pub line: String,
    pub actions: Vec<TopAction>,
    /// *Accept all* waits for the daemon.
    pub accepting: bool,
    /// The new commits of a done round: (short sha, title), at most `COMMITS_SHOWN`.
    pub commits: Vec<(String, String)>,
    /// How many more commits the round has than the list shows.
    pub more: usize,
}

/// Commits listed under the banner before `+ n more`.
pub const COMMITS_SHOWN: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenView {
    pub repo: String,
    pub number: String,
    pub title: String,
    pub chip: (&'static str, Tone),
    pub banner: BannerView,
    pub rows: Vec<RowView>,
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

fn anchor_place(a: &Anchor) -> String {
    match a.start_line {
        Some(start) if start != a.line => format!("{}:{start}–{}", file_name(&a.path), a.line),
        _ => format!("{}:{}", file_name(&a.path), a.line),
    }
}

/// `cache.rs:40 → 44`; the file name repeats only when the comment changed file.
fn move_place(from: Option<&Anchor>, to: &Anchor) -> String {
    match from {
        Some(from) if from.path == to.path && from.line == to.line => anchor_place(to),
        Some(from) if from.path == to.path => format!("{} → {}", anchor_place(from), to.line),
        Some(from) => format!("{} → {}", anchor_place(from), anchor_place(to)),
        None => anchor_place(to),
    }
}

/// What a decided row says.
fn decided_text(kind: RowKind, accepted: bool) -> &'static str {
    match (kind, accepted) {
        (RowKind::Moved, true) => "You kept the move",
        (RowKind::Moved, false) => "You put it back",
        (RowKind::Obsolete, true) => "Marked obsolete: it stays out of the publish",
        (RowKind::Obsolete, false) => "You kept the comment",
        (RowKind::NewWording, true) => "You used the new wording",
        (RowKind::NewWording, false) => "You kept your wording",
        (RowKind::Kept | RowKind::NotChecked, _) => "",
    }
}

fn label_of(kind: RowKind) -> (&'static str, Tone) {
    match kind {
        RowKind::Kept => ("Kept", Tone::Neutral),
        RowKind::Moved => ("Moved", Tone::Green),
        RowKind::Obsolete => ("Obsolete", Tone::Orange),
        RowKind::NewWording => ("New wording", Tone::Green),
        RowKind::NotChecked => ("Not checked", Tone::Neutral),
    }
}

/// Why *Put it back* is refused, when it is: asked of the daemon's own rule on a copy.
fn put_back_blocked(
    review: &Review,
    round: &Revalidation,
    item: &str,
    files: &[FileDiff],
) -> Option<String> {
    let mut copy = review.clone();
    copy.revalidation = Some(round.clone());
    match decide(&mut copy, item, false, files) {
        Err(DecideError::CannotPutBack(reason)) => Some(printable(&reason)),
        _ => None,
    }
}

/// The rows of `round`, in the order the agent's proposals were stored. A proposal whose item
/// left the draft has no row.
pub fn row_views(
    round: &Revalidation,
    review: &Review,
    files: &[FileDiff],
    waiting: &BTreeSet<String>,
) -> Vec<RowView> {
    let mut rows = Vec::new();
    for p in &round.proposals {
        let Some(item) = review.draft.items.iter().find(|i| i.id == p.item) else {
            continue;
        };
        let kind = row_kind(p, item);
        let (label, tone) = label_of(kind);
        // The daemon's own rule: a move with no anchor to move from or to has nothing to click.
        let has_buttons = decidable(p, item);
        let undecided = p.decision == Decision::Pending;
        let mut place = draft_card(item).title;
        let mut note = None;
        let mut suggested = None;
        match (&p.verdict, kind) {
            (Verdict::Moved { anchor, reason }, _) => {
                // A proposal that is still open moves to the agent's place; a taken one is there.
                let to = if undecided {
                    anchor
                } else {
                    item.anchor.as_ref().unwrap_or(anchor)
                };
                place = move_place(p.before.as_ref(), to);
                note = Some(printable(reason));
            }
            (_, RowKind::Moved) => {
                if let (Some(to), true) = (item.anchor.as_ref(), item.anchor != p.before) {
                    place = move_place(p.before.as_ref(), to);
                }
            }
            (Verdict::Obsolete { reason }, _) => note = Some(printable(reason)),
            (_, RowKind::Obsolete) => {
                if let ItemStatus::Obsolete { reason } = &item.status {
                    note = Some(printable(reason));
                }
            }
            (Verdict::Reword { text, reason }, _) => {
                suggested = Some(printable_lines(text));
                note = reason.as_deref().map(printable);
            }
            (Verdict::NotChecked, _) => {
                note = Some("Claude Code did not return a result for this comment".into());
            }
            _ => {}
        }
        let mut actions = Vec::new();
        let mut outcome = None;
        if has_buttons && undecided {
            let obsolete_item = matches!(item.status, ItemStatus::Obsolete { .. });
            let (reject, accept): (&'static str, &'static str) = match kind {
                RowKind::Moved => ("Put it back", "Keep the move"),
                RowKind::Obsolete if obsolete_item => ("Keep and re-anchor", "Remove comment"),
                RowKind::Obsolete => ("Keep comment", "Remove comment"),
                _ => ("Keep mine", "Use new wording"),
            };
            let mut on = true;
            if waiting.contains(&p.item) {
                on = false;
                outcome = Some("Working…".to_string());
            } else if fingerprint(item) != p.fingerprint {
                on = false;
                outcome = Some(DecideError::Changed.to_string());
            }
            let mut reject_on = on;
            if on
                && kind == RowKind::Moved
                && let Some(why) = put_back_blocked(review, round, &p.item, files)
            {
                reject_on = false;
                outcome = Some(why);
            }
            actions.push(RowAction {
                label: reject,
                accept: false,
                enabled: reject_on,
            });
            actions.push(RowAction {
                label: accept,
                accept: true,
                enabled: on,
            });
        } else if has_buttons {
            outcome = Some(decided_text(kind, p.decision == Decision::Accepted).to_string());
        }
        rows.push(RowView {
            item: p.item.clone(),
            kind,
            label,
            tone,
            place,
            body: printable_lines(item.body.trim()),
            note,
            suggested,
            actions,
            outcome,
        });
    }
    rows
}

/// The counts line: `1 kept · 1 moved · 1 obsolete · 1 new wording suggested`.
fn counts_line(rows: &[RowView], unreadable: u32) -> String {
    let n = |kind| rows.iter().filter(|r| r.kind == kind).count();
    let mut parts = Vec::new();
    for (count, what) in [
        (n(RowKind::Kept), "kept"),
        (n(RowKind::Moved), "moved"),
        (n(RowKind::Obsolete), "obsolete"),
        (n(RowKind::NewWording), "new wording suggested"),
        (n(RowKind::NotChecked), "not checked"),
    ] {
        if count > 0 {
            parts.push(format!("{count} {what}"));
        }
    }
    if unreadable > 0 {
        parts.push(format!(
            "{} could not be read",
            plural(unreadable as usize, "result")
        ));
    }
    if parts.is_empty() {
        return "Nothing to report.".into();
    }
    format!(
        "{}. Moves are already applied; suggestions change nothing until you accept them.",
        parts.join(" · ")
    )
}

/// What SP1's relocation did without the agent: `Your draft followed the code: 1 moved, …`.
fn sp1_summary(round: &Revalidation) -> String {
    let mut parts = Vec::new();
    if round.sp1_moved > 0 {
        parts.push(format!("{} moved", round.sp1_moved));
    }
    if round.sp1_obsolete > 0 {
        parts.push(format!("{} obsolete", round.sp1_obsolete));
    }
    if parts.is_empty() {
        "Your draft followed the code.".into()
    } else {
        format!("Your draft followed the code: {}", parts.join(", "))
    }
}

/// How many comments the round looks at: what the agent was asked, or what the round opened with.
fn comments_in_play(round: &Revalidation) -> usize {
    if round.asked > 0 {
        round.asked as usize
    } else {
        round.before.len()
    }
}

fn banner_view(
    round: &Revalidation,
    rows: &[RowView],
    harness: bool,
    pending_rows: usize,
    accepting: bool,
) -> BannerView {
    let commits = round.commits.len();
    let new_commits = if commits == 0 {
        "New commits".to_string()
    } else {
        format!(
            "{} since you saved this review",
            plural(commits, "new commit")
        )
    };
    let comments = plural(comments_in_play(round), "comment");
    let banner = |title: String, line: String, actions: Vec<TopAction>| BannerView {
        title,
        line,
        actions,
        accepting,
        commits: Vec::new(),
        more: 0,
    };
    match &round.state {
        _ if !harness && !matches!(round.state, RoundState::Done | RoundState::Running { .. }) => {
            banner(
                sp1_summary(round),
                "Set up a harness in Config › Harness to have Claude Code check your comments."
                    .into(),
                vec![TopAction::SetUp, TopAction::Back],
            )
        }
        RoundState::Scheduled { .. } => banner(
            "Waiting for pushes to settle".into(),
            format!(
                "Claude Code checks your {comments} once no new commit has arrived for a while."
            ),
            vec![TopAction::RevalidateNow, TopAction::Back],
        ),
        RoundState::Waiting => banner(
            "Waiting for another check to finish".into(),
            format!("Claude Code checks your {comments} as soon as a place is free."),
            vec![TopAction::Stop, TopAction::Back],
        ),
        RoundState::Running { activity } => banner(
            format!(
                "Claude Code is checking your {comments} against {}…",
                plural(commits.max(1), "new commit")
            ),
            activity
                .as_deref()
                .map(printable)
                .filter(|a| !a.trim().is_empty())
                .unwrap_or_else(|| "Nothing in your draft changes until you accept it.".into()),
            vec![TopAction::Stop, TopAction::Back],
        ),
        RoundState::Failed { message } => banner(
            if message == "Stopped" {
                "Stopped".into()
            } else {
                "Claude Code could not check your comments".into()
            },
            if message == "Stopped" {
                "Your comments followed the code, and nothing else changed.".into()
            } else {
                printable(message)
            },
            vec![TopAction::TryAgain, TopAction::Back],
        ),
        RoundState::Done => {
            let mut actions = vec![TopAction::SeeCommits];
            if pending_rows > 0 {
                actions.push(TopAction::AcceptAll);
            }
            actions.push(TopAction::Back);
            let mut done = banner(
                format!(
                    "{new_commits}. Claude Code checked your {comments} against {}.",
                    if commits == 1 { "it" } else { "them" }
                ),
                counts_line(rows, round.unreadable),
                actions,
            );
            done.commits = round
                .commits
                .iter()
                .take(COMMITS_SHOWN)
                .map(|c| {
                    (
                        printable(&c.sha).chars().take(7).collect(),
                        printable(&c.title),
                    )
                })
                .collect();
            done.more = commits.saturating_sub(COMMITS_SHOWN);
            done
        }
    }
}

/// The screen of a ready tab, or `None` when it has no round.
pub fn screen_view(
    tab: &Tab,
    model: Option<&RevalidationModel>,
    harness: bool,
) -> Option<ScreenView> {
    let ready = tab.ready()?;
    let round = round_of(tab, model)?;
    let review = &ready.view.review;
    let waiting = model.map(|m| m.pending.clone()).unwrap_or_default();
    let rows = if matches!(round.state, RoundState::Done) {
        row_views(round, review, &ready.view.diff, &waiting)
    } else {
        Vec::new()
    };
    let pending_rows = pending(round, &review.draft);
    let accepting = model.is_some_and(|m| m.all_pending);
    let pr = &review.pr;
    Some(ScreenView {
        repo: format!("{}/{}", pr.owner, pr.repo),
        number: format!("#{}", pr.number),
        title: printable(&review.title),
        chip: if matches!(round.state, RoundState::Done) {
            ("revalidated", Tone::Green)
        } else {
            ("outdated", Tone::Orange)
        },
        banner: banner_view(round, &rows, harness, pending_rows, accepting),
        rows,
    })
}

/// The screen's root; `fill_revalidation` keeps it drawn.
#[derive(Component, Debug)]
pub struct RevalidationRoot {
    pub pr: PrRef,
    built: Option<ScreenView>,
}

/// The new commits listed under the banner's counts.
#[derive(Component, Debug, Clone, Copy)]
pub struct CommitLines;

/// The card on top: what happened, and its buttons.
#[derive(Component, Debug, Clone, Copy)]
pub struct RevalidationBannerCard;

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RevalidationRow {
    pub pr: PrRef,
    pub item: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RevalidationButton {
    pub pr: PrRef,
    pub action: TopAction,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ProposalButton {
    pub pr: PrRef,
    pub item: String,
    pub accept: bool,
}

/// The empty root `mount` spawns in place of the shell.
pub fn revalidation_root(pr: &PrRef) -> impl Bundle {
    (
        Node {
            flex_grow: 1.0,
            width: percent(100),
            min_height: px(0),
            flex_direction: FlexDirection::Column,
            row_gap: px(12),
            padding: UiRect::axes(px(20), px(16)),
            overflow: Overflow::scroll_y(),
            ..default()
        },
        ScrollArea,
        ScrollPosition::default(),
        RevalidationRoot {
            pr: pr.clone(),
            built: None,
        },
    )
}

pub struct RevalidationPlugin;

impl Plugin for RevalidationPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (load_revalidations, fill_revalidation, end_reanchor)
                .chain()
                .after(ReviewSystems),
        );
    }
}

/// Asks for the round of each ready tab once the connection is live, and forgets the rounds of
/// closed tabs. A cached copy asks when the fresh one arrives.
fn load_revalidations(
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut rounds: ResMut<Revalidations>,
    mut asks: ResMut<Asks>,
) {
    if rounds.0.keys().any(|pr| !tabs.0.contains_key(pr)) {
        rounds.0.retain(|pr, _| tabs.0.contains_key(pr));
    }
    if model.connection != Connection::Live {
        return;
    }
    for (pr, tab) in &tabs.0 {
        if !tab.ready().is_some_and(|r| r.cached_at.is_none()) {
            continue;
        }
        if rounds.0.get(pr).is_some_and(|m| m.asked) {
            continue;
        }
        rounds.0.entry(pr.clone()).or_default().asked = true;
        asks.send(Ask::GetRevalidation(pr.clone()));
    }
}

fn fill_revalidation(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    rounds: Res<Revalidations>,
    model: Res<Model>,
    fonts: Res<UiFonts>,
    mut roots: Query<(Entity, &mut RevalidationRoot)>,
) {
    for (entity, mut root) in &mut roots {
        let Some(tab) = tabs.0.get(&root.pr) else {
            continue;
        };
        let Some(view) = screen_view(tab, rounds.0.get(&root.pr), harness_ready(&model.snapshot))
        else {
            continue;
        };
        if root.built.as_ref() == Some(&view) {
            continue;
        }
        let pr = root.pr.clone();
        commands.entity(entity).despawn_related::<Children>();
        commands
            .entity(entity)
            .with_children(|p| draw(p, &fonts, &pr, &view));
        root.built = Some(view);
    }
}

fn draw(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, v: &ScreenView) {
    p.spawn(Node {
        column_gap: px(10),
        align_items: AlignItems::Baseline,
        flex_shrink: 0.0,
        padding: UiRect::bottom(px(4)),
        ..default()
    })
    .with_children(|h| {
        h.spawn(text(fonts, v.repo.clone(), Type::MUTED));
        h.spawn(text(fonts, v.number.clone(), Type::MUTED));
        h.spawn(text(fonts, v.title.clone(), Type::TITLE.size(17.0)));
        h.spawn(badge(fonts, v.chip.0, v.chip.1));
    });
    banner_card(p, fonts, pr, &v.banner);
    for row in &v.rows {
        row_card(p, fonts, pr, row);
    }
}

fn banner_card(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, b: &BannerView) {
    p.spawn((
        card(Node {
            flex_shrink: 0.0,
            column_gap: px(14),
            align_items: AlignItems::Center,
            padding: px(16).all(),
            ..default()
        }),
        RevalidationBannerCard,
    ))
    .with_children(|c| {
        c.spawn(panel(
            Node {
                width: px(34),
                height: px(34),
                flex_shrink: 0.0,
                border_radius: BorderRadius::all(px(8)),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            Swatch::GreenSoft,
        ))
        .with_children(|i| {
            i.spawn(text(fonts, "↻", Type::STRONG.ink(Swatch::Green).size(15.0)));
        });
        c.spawn(Node {
            flex_grow: 1.0,
            flex_basis: px(0),
            min_width: px(0),
            flex_direction: FlexDirection::Column,
            row_gap: px(3),
            ..default()
        })
        .with_children(|t| {
            t.spawn(text(fonts, b.title.clone(), Type::STRONG));
            t.spawn(text(fonts, b.line.clone(), Type::META));
            if !b.commits.is_empty() {
                t.spawn((
                    Node {
                        flex_direction: FlexDirection::Column,
                        row_gap: px(2),
                        padding: UiRect::top(px(4)),
                        ..default()
                    },
                    CommitLines,
                ))
                .with_children(|list| {
                    for (sha, title) in &b.commits {
                        list.spawn(Node {
                            column_gap: px(8),
                            ..default()
                        })
                        .with_children(|row| {
                            row.spawn(text(fonts, sha.clone(), Type::MONO));
                            row.spawn(text(fonts, title.clone(), Type::META));
                        });
                    }
                    if b.more > 0 {
                        list.spawn(text(fonts, format!("+ {} more", b.more), Type::META));
                    }
                });
            }
        });
        c.spawn(Node {
            column_gap: px(8),
            flex_shrink: 0.0,
            ..default()
        })
        .with_children(|buttons| {
            for action in &b.actions {
                if *action == TopAction::AcceptAll && b.accepting {
                    buttons.spawn(disabled_button(fonts, "Accepting…"));
                    continue;
                }
                let variant = match action {
                    TopAction::AcceptAll | TopAction::RevalidateNow | TopAction::TryAgain => {
                        Variant::Primary
                    }
                    _ => Variant::Secondary,
                };
                buttons.spawn((
                    button(fonts, action.label(), variant),
                    RevalidationButton {
                        pr: pr.clone(),
                        action: *action,
                    },
                    observe(on_top),
                ));
            }
        });
    });
}

fn row_card(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, r: &RowView) {
    p.spawn((
        card(Node {
            flex_shrink: 0.0,
            column_gap: px(20),
            align_items: AlignItems::Start,
            padding: px(16).all(),
            ..default()
        }),
        RevalidationRow {
            pr: pr.clone(),
            item: r.item.clone(),
        },
    ))
    .insert(Stroke(if r.kind == RowKind::Obsolete {
        Swatch::Orange
    } else {
        Swatch::Line
    }))
    .with_children(|c| {
        c.spawn(Node {
            width: px(150),
            flex_shrink: 0.0,
            flex_direction: FlexDirection::Column,
            row_gap: px(8),
            ..default()
        })
        .with_children(|l| {
            l.spawn(badge(fonts, r.label, r.tone));
            l.spawn(text(fonts, r.place.clone(), Type::MONO));
        });
        c.spawn(Node {
            flex_grow: 1.0,
            flex_basis: px(0),
            min_width: px(0),
            flex_direction: FlexDirection::Column,
            row_gap: px(8),
            ..default()
        })
        .with_children(|m| {
            m.spawn(text(fonts, r.body.clone(), Type::BODY));
            if let Some(note) = &r.note {
                m.spawn(text(fonts, note.clone(), Type::META));
            }
            if let Some(suggested) = &r.suggested {
                m.spawn(panel(
                    Node {
                        padding: UiRect::axes(px(10), px(8)),
                        border_radius: BorderRadius::all(px(6)),
                        column_gap: px(4),
                        flex_wrap: FlexWrap::Wrap,
                        ..default()
                    },
                    Swatch::GreenSoft,
                ))
                .with_children(|s| {
                    s.spawn(text(fonts, "Suggested:", Type::STRONG.ink(Swatch::Green)));
                    s.spawn(text(fonts, suggested.clone(), Type::BODY));
                });
            }
            if let Some(outcome) = &r.outcome {
                m.spawn(text(fonts, outcome.clone(), Type::META.ink(Swatch::Muted)));
            }
        });
        if !r.actions.is_empty() {
            c.spawn(Node {
                column_gap: px(8),
                flex_shrink: 0.0,
                ..default()
            })
            .with_children(|buttons| {
                for a in &r.actions {
                    if !a.enabled {
                        buttons.spawn(disabled_button(fonts, a.label));
                        continue;
                    }
                    buttons.spawn((
                        button(
                            fonts,
                            a.label,
                            if a.accept {
                                Variant::Primary
                            } else {
                                Variant::Secondary
                            },
                        ),
                        ProposalButton {
                            pr: pr.clone(),
                            item: r.item.clone(),
                            accept: a.accept,
                        },
                        observe(on_proposal),
                    ));
                }
            });
        }
    });
}

fn on_top(
    activate: On<Activate>,
    buttons: Query<&RevalidationButton>,
    mut tabs: ResMut<ReviewTabs>,
    model: Res<Model>,
    mut rounds: ResMut<Revalidations>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
    mut nav: ResMut<crate::nav::Nav>,
) {
    let Ok(RevalidationButton { pr, action }) = buttons.get(activate.entity) else {
        return;
    };
    let guarded = matches!(
        action,
        TopAction::AcceptAll | TopAction::Stop | TopAction::RevalidateNow | TopAction::TryAgain
    );
    if guarded && let Some(reason) = read_only_reason(&tabs, &model, pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    match action {
        TopAction::SeeCommits => {
            // The window has no view of the range between the two heads: it shows the diff at
            // the new one, which holds every change of the new commits.
            if let Some(tab) = tabs.0.get_mut(pr) {
                tab.ui.revalidation = RevalView::Closed;
                tab.ui.section = crate::review_state::ReviewSection::Diff;
            }
        }
        TopAction::Back => {
            if let Some(tab) = tabs.0.get_mut(pr) {
                tab.ui.revalidation = RevalView::Closed;
            }
        }
        TopAction::AcceptAll => {
            rounds.0.entry(pr.clone()).or_default().all_pending = true;
            asks.send(Ask::AcceptAllProposals(pr.clone()));
        }
        TopAction::Stop => asks.send(Ask::StopRevalidation(pr.clone())),
        TopAction::RevalidateNow | TopAction::TryAgain => asks.send(Ask::Revalidate(pr.clone())),
        TopAction::SetUp => nav.open_section(crate::nav::Section::Harness),
    }
}

fn on_proposal(
    activate: On<Activate>,
    buttons: Query<&ProposalButton>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut rounds: ResMut<Revalidations>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(ProposalButton { pr, item, accept }) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &model, pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    let waiting = &mut rounds.0.entry(pr.clone()).or_default().pending;
    if !waiting.insert(item.clone()) {
        return;
    }
    asks.send(Ask::DecideProposal {
        pr: pr.clone(),
        item: item.clone(),
        accept: *accept,
    });
    // Refusing an obsolete item asks the Diff for its new place, once the daemon settles it.
    let obsolete = tabs.0.get(pr).and_then(|t| t.ready()).is_some_and(|ready| {
        ready
            .view
            .review
            .draft
            .items
            .iter()
            .any(|i| i.id == *item && matches!(i.status, ItemStatus::Obsolete { .. }))
    });
    if !*accept && obsolete {
        rounds
            .0
            .entry(pr.clone())
            .or_default()
            .reanchor
            .insert(item.clone());
    }
}

/// The compact line on the normal tabs while suggestions wait.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RevalidationBanner(pub PrRef);

/// **Review them**: brings the revalidation screen back.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ReviewThem(pub PrRef);

/// What the banner says before its button: `2 suggestions from the revalidation ·`.
pub fn banner_text(waiting: usize) -> String {
    format!("{} from the revalidation ·", plural(waiting, "suggestion"))
}

pub fn banner_strip(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, waiting: usize) {
    p.spawn((
        panel(
            Node {
                padding: UiRect::axes(px(20), px(6)),
                column_gap: px(8),
                align_items: AlignItems::Center,
                ..default()
            },
            Swatch::GreenSoft,
        ),
        RevalidationBanner(pr.clone()),
    ))
    .with_children(|s| {
        s.spawn(text(fonts, "↻", Type::STRONG.ink(Swatch::Green)));
        s.spawn(text(fonts, banner_text(waiting), Type::BODY));
        s.spawn((
            button(fonts, "Review them", Variant::Ghost),
            ReviewThem(pr.clone()),
            observe(on_review_them),
        ));
    });
}

/// The strip while the Diff waits for a click on the new place of a comment.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ReanchorStrip(pub PrRef);

/// **Make it a general comment**.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ReanchorGeneral {
    pub pr: PrRef,
    pub item: String,
}

/// **Cancel** (the same as Esc).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ReanchorCancel(pub PrRef);

/// The comment the strip names: its place and the start of its text.
pub fn reanchor_preview(item: &DraftItem) -> String {
    let body = printable_lines(item.body.trim()).replace('\n', " ");
    let mut short: String = body.chars().take(80).collect();
    if body.chars().count() > 80 {
        short.push('…');
    }
    format!("{} · {short}", draft_card(item).title)
}

pub fn reanchor_strip(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    item: &str,
    preview: &str,
) {
    p.spawn((
        panel(
            Node {
                padding: UiRect::axes(px(20), px(8)),
                column_gap: px(12),
                align_items: AlignItems::Center,
                ..default()
            },
            Swatch::OrangeSoft,
        ),
        ReanchorStrip(pr.clone()),
    ))
    .with_children(|s| {
        s.spawn(Node {
            flex_grow: 1.0,
            flex_basis: px(0),
            min_width: px(0),
            flex_direction: FlexDirection::Column,
            row_gap: px(2),
            ..default()
        })
        .with_children(|t| {
            t.spawn(text(
                fonts,
                "Click a line to anchor this comment",
                Type::STRONG.ink(Swatch::Orange),
            ));
            t.spawn(text(fonts, preview.to_string(), Type::META));
        });
        s.spawn((
            button(fonts, "Make it a general comment", Variant::Secondary),
            ReanchorGeneral {
                pr: pr.clone(),
                item: item.to_string(),
            },
            observe(on_general),
        ));
        s.spawn((
            button(fonts, "Cancel", Variant::Ghost),
            ReanchorCancel(pr.clone()),
            observe(on_cancel),
        ));
    });
}

fn on_review_them(
    activate: On<Activate>,
    buttons: Query<&ReviewThem>,
    mut tabs: ResMut<ReviewTabs>,
) {
    let Ok(ReviewThem(pr)) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(pr) {
        tab.ui.revalidation = RevalView::Open;
    }
}

fn on_cancel(
    activate: On<Activate>,
    buttons: Query<&ReanchorCancel>,
    mut tabs: ResMut<ReviewTabs>,
) {
    let Ok(ReanchorCancel(pr)) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(pr) {
        tab.ui.reanchor = None;
    }
}

fn on_general(
    activate: On<Activate>,
    buttons: Query<&ReanchorGeneral>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut rounds: ResMut<Revalidations>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(ReanchorGeneral { pr, item }) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &model, pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    if rounds
        .0
        .entry(pr.clone())
        .or_default()
        .reanchoring
        .insert(item.clone())
    {
        asks.send(Ask::ReanchorDraftItem {
            pr: pr.clone(),
            id: item.clone(),
            anchor: None,
        });
    }
}

/// A click on a line while the Diff waits for the new place of `item`: asks the daemon to move
/// it there. Returns `false` when the tab is not in that mode, so the click means what it
/// always meant.
pub(crate) fn reanchor_click(
    tabs: &ReviewTabs,
    model: &Model,
    rounds: &mut Revalidations,
    toasts: &mut Toasts,
    time: &Time,
    asks: &mut Asks,
    pr: &PrRef,
    place: AnchorInput,
) -> bool {
    let Some(item) = tabs.0.get(pr).and_then(|t| t.ui.reanchor.clone()) else {
        return false;
    };
    if let Some(reason) = read_only_reason(tabs, model, pr) {
        not_sent(toasts, time, reason);
        return true;
    }
    if rounds
        .0
        .entry(pr.clone())
        .or_default()
        .reanchoring
        .insert(item.clone())
    {
        asks.send(Ask::ReanchorDraftItem {
            pr: pr.clone(),
            id: item,
            anchor: Some(place),
        });
    }
    true
}

/// Esc ends the re-anchor mode, and so does the comment going away.
fn end_reanchor(
    keys: Res<ButtonInput<KeyCode>>,
    nav: Res<crate::nav::Nav>,
    mut tabs: ResMut<ReviewTabs>,
) {
    let esc = escape_pressed(&keys);
    let shown = match &nav.screen {
        crate::nav::Screen::Review(pr) => Some(pr.clone()),
        _ => None,
    };
    for (pr, tab) in tabs.0.iter_mut() {
        let Some(item) = tab.ui.reanchor.clone() else {
            continue;
        };
        let gone = tab
            .ready()
            .is_some_and(|r| !r.view.review.draft.items.iter().any(|i| i.id == item));
        // An open comment editor takes the Esc first: one press closes one thing.
        let escaped =
            esc && shown.as_ref() == Some(pr) && tab.ui.modal.is_none() && tab.ui.editor.is_none();
        if gone || escaped {
            tab.ui.reanchor = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Tell;
    use crate::fixture;
    use crate::review_state::ReviewSection;
    use crate::testing::{self, NOW};
    use clusia_core::revalidation::CommitLine;

    fn pr() -> PrRef {
        fixture::demo_pr()
    }

    fn done_app() -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_round(&mut app, RoundState::Done);
        (app, pr)
    }

    fn round_of_app(app: &App) -> Revalidation {
        app.world().resource::<Revalidations>().0[&pr()]
            .round
            .clone()
            .expect("a round")
    }

    fn rows_of(app: &App) -> Vec<RowView> {
        let ready = testing::ready(app, &pr());
        row_views(
            &round_of_app(app),
            &ready.view.review,
            &ready.view.diff,
            &BTreeSet::new(),
        )
    }

    fn click(app: &mut App, item: &str, accept: bool) {
        let button = testing::find::<ProposalButton>(app, |b| b.item == item && b.accept == accept);
        testing::activate(app, button);
    }

    fn top(app: &mut App, action: TopAction) {
        let button = testing::find::<RevalidationButton>(app, |b| b.action == action);
        testing::activate(app, button);
    }

    #[test]
    fn the_rows_follow_the_mockup() {
        let (app, _) = done_app();
        let rows = rows_of(&app);
        let shape: Vec<(&str, &str, Vec<&str>)> = rows
            .iter()
            .map(|r| {
                (
                    r.label,
                    r.place.as_str(),
                    r.actions.iter().map(|a| a.label).collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("Kept", "refresh.rs:44", vec![]),
                (
                    "Moved",
                    "store.rs:81 → 92",
                    vec!["Put it back", "Keep the move"]
                ),
                (
                    "Obsolete",
                    "http.rs:20",
                    vec!["Keep and re-anchor", "Remove comment"]
                ),
                (
                    "New wording",
                    "refresh.rs:64",
                    vec!["Keep mine", "Use new wording"]
                ),
                ("Not checked", "refresh.rs:46", vec![]),
            ]
        );
        assert_eq!(
            rows[1].note.as_deref(),
            Some("The function gained four lines above this one.")
        );
        assert_eq!(
            rows[3].suggested.as_deref(),
            Some("The new test covers two writers; a reader is still missing.")
        );
        assert_eq!(
            rows[4].note.as_deref(),
            Some("Claude Code did not return a result for this comment")
        );
        assert!(rows.iter().all(|r| r.outcome.is_none()));
    }

    #[test]
    fn the_screen_says_what_was_checked() {
        let (mut app, _) = done_app();
        assert_eq!(testing::count::<RevalidationRow>(&mut app), 5);
        assert!(testing::shows(
            &mut app,
            "2 new commits since you saved this review. Claude Code checked your 5 comments against them."
        ));
        assert!(testing::shows(
            &mut app,
            "1 kept · 1 moved · 1 obsolete · 1 new wording suggested · 1 not checked. Moves are already applied; suggestions change nothing until you accept them."
        ));
        for label in [
            "See the new commits",
            "Accept all",
            "Back to review",
            "Put it back",
        ] {
            assert!(testing::shows(&mut app, label), "{label}");
        }
        assert!(testing::shows(&mut app, "revalidated"));
        // The tabs are not drawn while the screen shows.
        assert!(!testing::shows(&mut app, "Open in editor"));
    }

    #[test]
    fn what_the_agent_said_is_printable() {
        let (mut app, pr) = done_app();
        let mut round = round_of_app(&app);
        round.proposals[1].verdict = Verdict::Moved {
            anchor: round.proposals[1].before.clone().unwrap(),
            reason: "gone\u{1b}[31m red".into(),
        };
        testing::tell(
            &mut app,
            Tell::Revalidation(RevalidationTell::Round {
                pr,
                round: Some(round),
            }),
        );
        testing::settle(&mut app);
        let rows = rows_of(&app);
        let note = rows[1].note.clone().unwrap();
        assert!(!note.contains('\u{1b}'), "{note:?}");
        assert!(testing::shows(&mut app, &note));
    }

    #[test]
    fn a_click_asks_for_one_decision_and_waits_for_the_answer() {
        let (mut app, pr) = done_app();
        click(&mut app, "i2", true);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::DecideProposal {
                pr: pr.clone(),
                item: "i2".into(),
                accept: true,
            }]
        );
        // The row waits: no buttons to click twice.
        assert!(testing::shows(&mut app, "Working…"));
        let again = testing::count::<ProposalButton>(&mut app);
        assert_eq!(again, 4, "the other rows keep their buttons");
        testing::tell(
            &mut app,
            Tell::Revalidation(RevalidationTell::Refused {
                pr,
                item: Some("i2".into()),
            }),
        );
        testing::settle(&mut app);
        assert_eq!(testing::count::<ProposalButton>(&mut app), 6);
    }

    #[test]
    fn a_decided_row_says_what_was_decided() {
        let (mut app, pr) = done_app();
        let mut round = round_of_app(&app);
        round.proposals[3].decision = Decision::Accepted;
        testing::tell(
            &mut app,
            Tell::Revalidation(RevalidationTell::Round {
                pr,
                round: Some(round),
            }),
        );
        testing::settle(&mut app);
        let rows = rows_of(&app);
        assert!(rows[3].actions.is_empty());
        assert_eq!(rows[3].outcome.as_deref(), Some("You used the new wording"));
    }

    #[test]
    fn a_comment_edited_after_the_suggestion_offers_no_buttons() {
        let (mut app, pr) = done_app();
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            let ready = tabs.0.get_mut(&pr).and_then(Tab::ready_mut).unwrap();
            ready.view.review.draft.items[3].body = "Edited since.".into();
        }
        testing::settle(&mut app);
        let rows = rows_of(&app);
        assert!(rows[3].actions.iter().all(|a| !a.enabled));
        assert_eq!(
            rows[3].outcome.as_deref(),
            Some("This comment changed since the suggestion")
        );
    }

    #[test]
    fn put_it_back_needs_a_line_in_the_new_diff() {
        let (app, pr) = done_app();
        let ready = testing::ready(&app, &pr);
        let mut round = round_of_app(&app);
        let rows = |round: &Revalidation| {
            row_views(
                round,
                &ready.view.review,
                &ready.view.diff,
                &BTreeSet::new(),
            )
        };
        assert!(rows(&round)[1].actions.iter().all(|a| a.enabled));
        // The line the comment came from is no longer in the diff.
        round.proposals[1].before.as_mut().unwrap().line = 900;
        let moved = &rows(&round)[1];
        assert!(!moved.actions[0].enabled, "Put it back is off");
        assert!(moved.actions[1].enabled, "Keep the move stays");
        assert!(moved.outcome.is_some());
    }

    #[test]
    fn a_proposal_whose_item_left_the_draft_has_no_row() {
        let (app, pr) = done_app();
        let mut ready = testing::ready(&app, &pr);
        ready.view.review.draft.items.retain(|i| i.id != "i3");
        let rows = row_views(
            &round_of_app(&app),
            &ready.view.review,
            &ready.view.diff,
            &BTreeSet::new(),
        );
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn keep_and_re_anchor_leaves_for_the_diff_on_that_file() {
        let (mut app, pr) = done_app();
        click(&mut app, "i3", false);
        let asks = testing::recorded(&mut app);
        assert_eq!(
            asks,
            [Ask::DecideProposal {
                pr: pr.clone(),
                item: "i3".into(),
                accept: false,
            }]
        );
        // The screen stays until the daemon takes the decision.
        let tab = testing::tab(&app, &pr);
        assert_eq!(
            (tab.ui.reanchor, tab.ui.revalidation),
            (None, RevalView::Auto)
        );
        testing::tell(
            &mut app,
            Tell::Revalidation(RevalidationTell::Settled {
                pr: pr.clone(),
                item: Some("i3".into()),
            }),
        );
        testing::settle(&mut app);
        let tab = testing::tab(&app, &pr);
        assert_eq!(tab.ui.reanchor.as_deref(), Some("i3"));
        assert_eq!(tab.ui.revalidation, RevalView::Closed);
        assert_eq!(tab.ui.section, ReviewSection::Diff);
        assert_eq!(tab.ui.file.as_deref(), Some("src/client/http.rs"));
    }

    #[test]
    fn a_refused_re_anchor_keeps_the_screen() {
        let (mut app, pr) = done_app();
        click(&mut app, "i3", false);
        testing::recorded(&mut app);
        testing::tell(
            &mut app,
            Tell::Revalidation(RevalidationTell::Refused {
                pr: pr.clone(),
                item: Some("i3".into()),
            }),
        );
        // A later answer for the same item must not open the Diff.
        testing::tell(
            &mut app,
            Tell::Revalidation(RevalidationTell::Settled {
                pr: pr.clone(),
                item: Some("i3".into()),
            }),
        );
        testing::settle(&mut app);
        let tab = testing::tab(&app, &pr);
        assert_eq!(
            (tab.ui.reanchor, tab.ui.revalidation),
            (None, RevalView::Auto)
        );
        assert_eq!(testing::count::<RevalidationRow>(&mut app), 5);
    }

    #[test]
    fn a_move_with_nothing_to_move_from_has_no_buttons() {
        let (app, pr) = done_app();
        let ready = testing::ready(&app, &pr);
        let mut round = round_of_app(&app);
        round.proposals[1].before = None;
        let rows = row_views(
            &round,
            &ready.view.review,
            &ready.view.diff,
            &BTreeSet::new(),
        );
        assert!(rows[1].actions.is_empty());
        assert_eq!(pending(&round, &ready.view.review.draft), 2);
    }

    #[test]
    fn stop_is_refused_on_a_cached_copy() {
        let (mut app, pr) = state_app(RoundState::Running { activity: None });
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            tabs.0
                .get_mut(&pr)
                .and_then(Tab::ready_mut)
                .unwrap()
                .cached_at = Some(NOW - 60);
        }
        testing::settle(&mut app);
        top(&mut app, TopAction::Stop);
        assert!(testing::recorded(&mut app).is_empty());
        assert!(!app.world().resource::<Toasts>().0.is_empty());
    }

    #[test]
    fn remove_comment_is_an_accept_and_stays_on_the_screen() {
        let (mut app, pr) = done_app();
        click(&mut app, "i3", true);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::DecideProposal {
                pr: pr.clone(),
                item: "i3".into(),
                accept: true,
            }]
        );
        let tab = testing::tab(&app, &pr);
        assert_eq!(
            (tab.ui.reanchor, tab.ui.revalidation),
            (None, RevalView::Auto)
        );
    }

    #[test]
    fn accept_all_asks_once() {
        let (mut app, pr) = done_app();
        top(&mut app, TopAction::AcceptAll);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::AcceptAllProposals(pr.clone())]
        );
        assert!(testing::shows(&mut app, "Accepting…"));
        assert_eq!(testing::count::<RevalidationButton>(&mut app), 2);
        testing::tell(
            &mut app,
            Tell::Revalidation(RevalidationTell::Settled { pr, item: None }),
        );
        testing::settle(&mut app);
        assert_eq!(testing::count::<RevalidationButton>(&mut app), 3);
    }

    #[test]
    fn a_cached_copy_changes_nothing() {
        let (mut app, pr) = done_app();
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            tabs.0
                .get_mut(&pr)
                .and_then(Tab::ready_mut)
                .unwrap()
                .cached_at = Some(NOW - 60);
        }
        testing::settle(&mut app);
        click(&mut app, "i4", true);
        top(&mut app, TopAction::AcceptAll);
        assert!(testing::recorded(&mut app).is_empty());
        assert!(!app.world().resource::<Toasts>().0.is_empty());
    }

    #[test]
    fn back_to_review_shows_the_tabs_and_see_the_new_commits_the_diff() {
        let (mut app, pr) = done_app();
        top(&mut app, TopAction::Back);
        assert!(testing::shows(&mut app, "Open in editor"));
        assert_eq!(testing::tab(&app, &pr).ui.revalidation, RevalView::Closed);

        let (mut app, pr) = done_app();
        top(&mut app, TopAction::SeeCommits);
        let tab = testing::tab(&app, &pr);
        assert_eq!(tab.ui.section, ReviewSection::Diff);
        assert!(testing::shows(&mut app, "Open in editor"));
    }

    #[test]
    fn the_screen_comes_back_for_review_them() {
        let (mut app, pr) = done_app();
        top(&mut app, TopAction::Back);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .revalidation = RevalView::Open;
        testing::settle(&mut app);
        assert_eq!(testing::count::<RevalidationRow>(&mut app), 5);
    }

    fn state_app(state: RoundState) -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        testing::set_config_locally(&mut app, "harness.program", "/usr/local/bin/claude");
        let pr = testing::open_round(&mut app, state);
        (app, pr)
    }

    #[test]
    fn a_running_round_offers_stop() {
        let (mut app, pr) = state_app(RoundState::Running {
            activity: Some("Reading src/cache.rs…".into()),
        });
        assert!(testing::shows(
            &mut app,
            "Claude Code is checking your 5 comments against 2 new commits…"
        ));
        assert!(testing::shows(&mut app, "Reading src/cache.rs…"));
        assert_eq!(testing::count::<RevalidationRow>(&mut app), 0);
        top(&mut app, TopAction::Stop);
        assert_eq!(testing::recorded(&mut app), [Ask::StopRevalidation(pr)]);
    }

    #[test]
    fn a_scheduled_round_offers_revalidate_now() {
        let (mut app, pr) = state_app(RoundState::Scheduled { at: NOW + 300 });
        assert!(testing::shows(&mut app, "Waiting for pushes to settle"));
        top(&mut app, TopAction::RevalidateNow);
        assert_eq!(testing::recorded(&mut app), [Ask::Revalidate(pr)]);
    }

    #[test]
    fn a_failed_round_offers_try_again_and_keeps_the_screen() {
        let (mut app, pr) = state_app(RoundState::Running { activity: None });
        let failed = Revalidation {
            state: RoundState::Failed {
                message: "The harness timed out".into(),
            },
            ..round_of_app(&app)
        };
        testing::tell(
            &mut app,
            Tell::Revalidation(RevalidationTell::Round {
                pr: pr.clone(),
                round: Some(failed),
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "The harness timed out"));
        top(&mut app, TopAction::TryAgain);
        assert_eq!(testing::recorded(&mut app), [Ask::Revalidate(pr)]);
    }

    #[test]
    fn with_no_harness_only_the_relocation_is_told() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_round(&mut app, RoundState::Scheduled { at: NOW + 300 });
        // A round that waits has no proposals yet.
        let waiting = Revalidation {
            proposals: Vec::new(),
            ..round_of_app(&app)
        };
        testing::tell(
            &mut app,
            Tell::Revalidation(RevalidationTell::Round {
                pr: pr.clone(),
                round: Some(waiting),
            }),
        );
        {
            let mut model = app.world_mut().resource_mut::<Model>();
            model.snapshot.config.harness.program = None;
            model.snapshot.first_run = None;
        }
        testing::settle(&mut app);
        // Nothing runs the round: the review opens on its tabs.
        assert!(testing::shows(&mut app, "Open in editor"));
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .revalidation = RevalView::Open;
        testing::settle(&mut app);
        assert!(testing::shows(
            &mut app,
            "Your draft followed the code: 1 moved, 1 obsolete"
        ));
        assert!(testing::shows(
            &mut app,
            "Set up a harness in Config › Harness"
        ));
    }

    #[test]
    fn the_screen_shows_first_only_when_something_waits() {
        let (app, pr) = done_app();
        let tab = testing::tab(&app, &pr);
        let model = app.world().resource::<Revalidations>().0.get(&pr).cloned();
        assert!(screen_wanted(&tab, model.as_ref(), false, true));
        // Everything decided: back to the tabs.
        let mut decided = model.clone().unwrap();
        for p in decided.round.as_mut().unwrap().proposals.iter_mut() {
            p.decision = Decision::Accepted;
        }
        assert!(!screen_wanted(&tab, Some(&decided), true, true));
        // A failure stays only where it is already shown.
        let mut failed = model.unwrap();
        let round = failed.round.as_mut().unwrap();
        round.proposals.clear();
        round.state = RoundState::Failed {
            message: "x".into(),
        };
        assert!(!screen_wanted(&tab, Some(&failed), false, true));
        assert!(screen_wanted(&tab, Some(&failed), true, true));
        // A tab with no round never shows it.
        assert!(!screen_wanted(
            &tab,
            Some(&RevalidationModel::default()),
            true,
            true
        ));
    }

    #[test]
    fn the_round_is_asked_for_once() {
        let mut app = testing::app(fixture::demo(NOW));
        app.world_mut().write_message(crate::bridge::ShowRequested(
            clusia_protocol::WindowTarget::Review { pr: pr() },
        ));
        app.update();
        let (view, _) = fixture::demo_review(NOW);
        testing::tell(
            &mut app,
            Tell::Opened {
                pr: pr(),
                view: Box::new(view),
                news: Vec::new(),
            },
        );
        testing::settle(&mut app);
        let asks = testing::recorded(&mut app);
        let asked = asks
            .iter()
            .filter(|a| **a == Ask::GetRevalidation(pr()))
            .count();
        assert_eq!(asked, 1);
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn the_stored_round_shows_until_the_daemon_answers() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            let ready = tabs.0.get_mut(&pr).and_then(Tab::ready_mut).unwrap();
            let round = testing::demo_round(&mut ready.view.review, RoundState::Done);
            ready.view.review.revalidation = Some(round);
        }
        testing::settle(&mut app);
        assert_eq!(testing::count::<RevalidationRow>(&mut app), 5);
        // The daemon says there is none: the tabs come back.
        testing::tell(
            &mut app,
            Tell::Revalidation(RevalidationTell::Round {
                pr: pr.clone(),
                round: None,
            }),
        );
        testing::settle(&mut app);
        assert_eq!(testing::count::<RevalidationRow>(&mut app), 0);
        assert!(testing::shows(&mut app, "Open in editor"));
    }

    fn demo_decisions_app() -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        app.add_systems(Update, crate::bridge::demo_answers);
        let pr = testing::open_round(&mut app, RoundState::Done);
        (app, pr)
    }

    fn body_of(app: &App, pr: &PrRef, id: &str) -> String {
        testing::ready(app, pr)
            .view
            .review
            .draft
            .items
            .iter()
            .find(|i| i.id == id)
            .expect("an item")
            .body
            .clone()
    }

    #[test]
    fn in_demo_mode_a_decision_changes_the_draft_and_the_row() {
        let (mut app, pr) = demo_decisions_app();
        assert_eq!(body_of(&app, &pr, "i4"), "Comment i4.");
        click(&mut app, "i4", true);
        testing::settle(&mut app);
        assert_eq!(
            body_of(&app, &pr, "i4"),
            "The new test covers two writers; a reader is still missing."
        );
        let rows = rows_of(&app);
        assert_eq!(rows[3].outcome.as_deref(), Some("You used the new wording"));
        assert!(
            !testing::shows(&mut app, "Working…"),
            "the row is free again"
        );
    }

    #[test]
    fn in_demo_mode_nothing_changes_until_a_click() {
        let (mut app, pr) = demo_decisions_app();
        testing::settle(&mut app);
        assert_eq!(body_of(&app, &pr, "i4"), "Comment i4.");
        assert!(rows_of(&app).iter().all(|r| r.outcome.is_none()));
        assert!(testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn in_demo_mode_accept_all_takes_every_suggestion() {
        let (mut app, pr) = demo_decisions_app();
        top(&mut app, TopAction::AcceptAll);
        testing::settle(&mut app);
        let ready = testing::ready(&app, &pr);
        let item = |id: &str| {
            ready
                .view
                .review
                .draft
                .items
                .iter()
                .find(|i| i.id == id)
                .expect("an item")
        };
        assert_eq!(item("i2").anchor.as_ref().map(|a| a.line), Some(92));
        assert!(matches!(item("i3").status, ItemStatus::Obsolete { .. }));
        assert_eq!(
            item("i4").body,
            "The new test covers two writers; a reader is still missing."
        );
        assert_eq!(testing::count::<ProposalButton>(&mut app), 0);
        assert!(!testing::shows(&mut app, "Accepting…"));
        // Nothing is left to decide: the tabs come back.
        assert!(testing::shows(&mut app, "Open in editor"));
    }

    #[test]
    fn in_demo_mode_a_stale_decision_is_refused_with_a_warning() {
        let (mut app, pr) = demo_decisions_app();
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            let ready = tabs.0.get_mut(&pr).and_then(Tab::ready_mut).unwrap();
            ready.view.review.draft.items[3].body = "Edited since.".into();
        }
        app.world_mut()
            .resource_mut::<Asks>()
            .send(Ask::DecideProposal {
                pr: pr.clone(),
                item: "i4".into(),
                accept: true,
            });
        testing::settle(&mut app);
        assert_eq!(body_of(&app, &pr, "i4"), "Edited since.");
        let toasts = &app.world().resource::<Toasts>().0;
        assert!(
            toasts
                .iter()
                .any(|t| t.warning && t.text.contains("This comment changed since the suggestion"))
        );
    }

    #[test]
    fn the_new_commits_are_listed_under_the_banner() {
        let (mut app, _) = done_app();
        assert_eq!(testing::count::<CommitLines>(&mut app), 1);
        for text in [
            "a1b2c3d",
            "refactor: one retry path",
            "e4f5a6b",
            "fix: refresh early",
        ] {
            assert!(testing::shows(&mut app, text), "{text}");
        }
        assert!(!testing::shows(&mut app, "more"));
    }

    #[test]
    fn a_long_list_of_commits_stops_at_five() {
        let (mut app, pr) = done_app();
        let mut round = round_of_app(&app);
        round.commits = (0..7)
            .map(|n| CommitLine {
                sha: format!("c0ffee{n}1234"),
                title: format!("commit {n}\u{1b}[31m"),
            })
            .collect();
        testing::tell(
            &mut app,
            Tell::Revalidation(RevalidationTell::Round {
                pr,
                round: Some(round),
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "c0ffee0"));
        assert!(testing::shows(&mut app, "c0ffee4"));
        assert!(!testing::shows(&mut app, "c0ffee5"));
        assert!(testing::shows(&mut app, "+ 2 more"));
        assert!(testing::shows(
            &mut app,
            "7 new commits since you saved this review."
        ));
        let titles = testing::shown_text(&mut app);
        assert!(
            titles.iter().all(|t| !t.contains('\u{1b}')),
            "plain text only"
        );
    }

    #[test]
    fn a_round_that_starts_again_brings_the_screen_back() {
        let (mut app, pr) = done_app();
        top(&mut app, TopAction::Back);
        assert_eq!(testing::tab(&app, &pr).ui.revalidation, RevalView::Closed);
        let tell_round = |app: &mut App, round: Revalidation| {
            testing::tell(
                app,
                Tell::Revalidation(RevalidationTell::Round {
                    pr: pr.clone(),
                    round: Some(round),
                }),
            );
        };
        // The same round told again changes nothing.
        let same = round_of_app(&app);
        tell_round(&mut app, same.clone());
        assert_eq!(testing::tab(&app, &pr).ui.revalidation, RevalView::Closed);
        // It runs again (Try again, Revalidate now).
        let running = Revalidation {
            state: RoundState::Running { activity: None },
            proposals: Vec::new(),
            ..same
        };
        tell_round(&mut app, running.clone());
        assert_eq!(testing::tab(&app, &pr).ui.revalidation, RevalView::Auto);
        // Back again; the run's activity line does not bring it back.
        top(&mut app, TopAction::Back);
        let reading = Revalidation {
            state: RoundState::Running {
                activity: Some("Reading src/cache.rs…".into()),
            },
            ..running
        };
        tell_round(&mut app, reading.clone());
        assert_eq!(testing::tab(&app, &pr).ui.revalidation, RevalView::Closed);
        // A new head does.
        tell_round(
            &mut app,
            Revalidation {
                to_head: "2222222".into(),
                ..reading
            },
        );
        assert_eq!(testing::tab(&app, &pr).ui.revalidation, RevalView::Auto);
    }

    #[test]
    fn the_first_round_seen_never_reopens_anything() {
        let (app, _) = done_app();
        let round = round_of_app(&app);
        assert!(!reopens(None, &round));
        assert!(!reopens(Some(&round), &round));
    }

    #[test]
    fn the_demo_round_leaves_room_for_the_next_comment() {
        let mut view = fixture::demo_review(NOW).0;
        testing::demo_round(&mut view.review, RoundState::Done);
        let id = view
            .review
            .draft
            .add(clusia_core::DraftKind::General, None, None, "A note.", NOW)
            .expect("adds")
            .id
            .clone();
        assert_eq!(
            view.review
                .draft
                .items
                .iter()
                .filter(|i| i.id == id)
                .count(),
            1
        );
    }

    #[test]
    fn a_round_forgets_a_re_anchor_nobody_waits_for() {
        let (app, _) = done_app();
        let round = round_of_app(&app);
        let told = RevalidationTell::Round {
            pr: pr(),
            round: Some(round),
        };
        let mut m = RevalidationModel::default();
        m.reanchor.insert("i3".into());
        m.apply(&told);
        assert!(m.reanchor.is_empty(), "a stale mark is dropped");
        // A decision in flight keeps its mark although the round already shows it.
        m.pending.insert("i3".into());
        m.reanchor.insert("i3".into());
        m.apply(&told);
        assert!(m.reanchor.contains("i3"));
    }

    #[test]
    fn the_model_clears_what_the_round_already_decided() {
        let mut m = RevalidationModel::default();
        m.pending.extend(["i2".to_string(), "i3".to_string()]);
        m.all_pending = true;
        let (app, _) = done_app();
        let mut round = round_of_app(&app);
        round.proposals[1].decision = Decision::Accepted;
        m.apply(&RevalidationTell::Round {
            pr: pr(),
            round: Some(round),
        });
        assert_eq!(m.pending.iter().collect::<Vec<_>>(), ["i3"]);
        assert!(m.all_pending, "accept all waits for its own answer");
        m.apply(&RevalidationTell::Settled {
            pr: pr(),
            item: None,
        });
        assert!(!m.all_pending);
    }

    fn press(app: &mut App, key: KeyCode) {
        app.world_mut()
            .resource_mut::<bevy::input::ButtonInput<KeyCode>>()
            .press(key);
        app.update();
        let mut keys = app
            .world_mut()
            .resource_mut::<bevy::input::ButtonInput<KeyCode>>();
        keys.release(key);
        keys.reset_all();
    }

    fn reanchoring(item: &str) -> (App, PrRef) {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            let ui = &mut tabs.0.get_mut(&pr).unwrap().ui;
            ui.reanchor = Some(item.into());
            ui.section = ReviewSection::Diff;
            ui.file = Some("src/client/http.rs".into());
        }
        testing::settle(&mut app);
        (app, pr)
    }

    fn line_button(app: &mut App) -> Entity {
        testing::find::<crate::screens::review::diff::LineButton>(app, |b| {
            b.path == "src/client/http.rs"
        })
    }

    #[test]
    fn suggestions_that_wait_show_a_compact_banner_on_the_tabs() {
        let (mut app, pr) = done_app();
        top(&mut app, TopAction::Back);
        assert_eq!(testing::count::<RevalidationBanner>(&mut app), 1);
        assert!(testing::shows(
            &mut app,
            "3 suggestions from the revalidation ·"
        ));
        let review_them = testing::find::<ReviewThem>(&mut app, |_| true);
        testing::activate(&mut app, review_them);
        assert_eq!(testing::tab(&app, &pr).ui.revalidation, RevalView::Open);
        assert_eq!(testing::count::<RevalidationRow>(&mut app), 5);
    }

    #[test]
    fn the_banner_counts_what_is_left_and_goes_when_nothing_is() {
        let (mut app, pr) = done_app();
        top(&mut app, TopAction::Back);
        let mut round = round_of_app(&app);
        round.proposals[1].decision = Decision::Accepted;
        testing::tell(
            &mut app,
            Tell::Revalidation(RevalidationTell::Round {
                pr: pr.clone(),
                round: Some(round.clone()),
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(
            &mut app,
            "2 suggestions from the revalidation ·"
        ));
        for p in round.proposals.iter_mut() {
            p.decision = Decision::Rejected;
        }
        testing::tell(
            &mut app,
            Tell::Revalidation(RevalidationTell::Round {
                pr,
                round: Some(round),
            }),
        );
        testing::settle(&mut app);
        assert_eq!(testing::count::<RevalidationBanner>(&mut app), 0);
    }

    #[test]
    fn a_review_with_no_round_has_no_banner() {
        let mut app = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut app, false);
        assert_eq!(testing::count::<RevalidationBanner>(&mut app), 0);
        assert_eq!(testing::count::<ReanchorStrip>(&mut app), 0);
    }

    #[test]
    fn an_obsolete_draft_card_offers_restore() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        assert_eq!(
            testing::count::<crate::screens::review::shell::DraftCardRestore>(&mut app),
            1,
            "only the demo's obsolete comment"
        );
        let restore =
            testing::find::<crate::screens::review::shell::DraftCardRestore>(&mut app, |r| {
                r.id == "i3"
            });
        testing::activate(&mut app, restore);
        let tab = testing::tab(&app, &pr);
        assert_eq!(tab.ui.reanchor.as_deref(), Some("i3"));
        assert_eq!(tab.ui.section, ReviewSection::Diff);
        assert_eq!(tab.ui.file.as_deref(), Some("src/client/http.rs"));
        assert!(
            testing::recorded(&mut app).is_empty(),
            "nothing is sent yet"
        );
        assert!(testing::shows(
            &mut app,
            "Click a line to anchor this comment"
        ));
    }

    fn with_general_comment_obsolete(app: &mut App, pr: &PrRef) {
        let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
        let ready = tabs.0.get_mut(pr).and_then(Tab::ready_mut).unwrap();
        let review = &mut ready.view.review;
        let id = review
            .draft
            .add(
                clusia_core::DraftKind::General,
                None,
                None,
                "A general note.",
                NOW,
            )
            .expect("adds")
            .id
            .clone();
        review
            .draft
            .items
            .iter_mut()
            .find(|i| i.id == id)
            .unwrap()
            .status = ItemStatus::Obsolete {
            reason: "removed in the revalidation".into(),
        };
    }

    #[test]
    fn restoring_an_obsolete_general_comment_clears_its_mark() {
        let mut app = testing::app(fixture::demo(NOW));
        app.add_systems(Update, crate::bridge::demo_answers);
        let pr = testing::open_ready(&mut app, false);
        with_general_comment_obsolete(&mut app, &pr);
        testing::settle(&mut app);
        let id = testing::ready(&app, &pr)
            .view
            .review
            .draft
            .items
            .iter()
            .find(|i| i.kind == clusia_core::DraftKind::General)
            .unwrap()
            .id
            .clone();
        let restore =
            testing::find::<crate::screens::review::shell::DraftCardRestore>(&mut app, |r| {
                r.id == id
            });
        assert_eq!(
            app.world()
                .get::<crate::screens::review::shell::DraftCardRestore>(restore)
                .unwrap()
                .path,
            None,
            "a general comment has no place to ask for"
        );
        testing::activate(&mut app, restore);
        testing::settle(&mut app);
        let ready = testing::ready(&app, &pr);
        let item = ready
            .view
            .review
            .draft
            .items
            .iter()
            .find(|i| i.id == id)
            .unwrap();
        assert_eq!(item.status, ItemStatus::Ok);
        assert_eq!(item.kind, clusia_core::DraftKind::General);
        assert_eq!(
            testing::tab(&app, &pr).ui.reanchor,
            None,
            "no Diff mode for it"
        );
    }

    #[test]
    fn restoring_a_general_comment_sends_one_ask_while_it_waits() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        with_general_comment_obsolete(&mut app, &pr);
        testing::settle(&mut app);
        let restore =
            testing::find::<crate::screens::review::shell::DraftCardRestore>(&mut app, |r| {
                r.path.is_none()
            });
        testing::activate(&mut app, restore);
        let asks = testing::recorded(&mut app);
        assert!(matches!(
            asks.as_slice(),
            [Ask::ReanchorDraftItem { anchor: None, .. }]
        ));
        testing::activate(&mut app, restore);
        assert!(testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn a_round_told_meanwhile_does_not_free_a_waiting_re_anchor() {
        let (mut app, pr) = reanchoring("i3");
        let line = line_button(&mut app);
        testing::activate(&mut app, line);
        assert_eq!(testing::recorded(&mut app).len(), 1);
        // The daemon's round event for the same change comes before the reply.
        testing::tell(
            &mut app,
            Tell::Revalidation(RevalidationTell::Round {
                pr: pr.clone(),
                round: None,
            }),
        );
        testing::activate(&mut app, line);
        assert!(testing::recorded(&mut app).is_empty(), "still waiting");
        testing::tell(
            &mut app,
            Tell::Revalidation(RevalidationTell::Refused {
                pr,
                item: Some("i3".into()),
            }),
        );
        testing::activate(&mut app, line);
        assert_eq!(testing::recorded(&mut app).len(), 1, "refused: a new try");
    }

    #[test]
    fn a_cached_copy_has_no_restore() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            tabs.0
                .get_mut(&pr)
                .and_then(Tab::ready_mut)
                .unwrap()
                .cached_at = Some(NOW - 60);
        }
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<crate::screens::review::shell::DraftCardRestore>(&mut app),
            0
        );
    }

    #[test]
    fn the_strip_names_the_comment() {
        let (mut app, _) = reanchoring("i3");
        assert_eq!(testing::count::<ReanchorStrip>(&mut app), 1);
        assert!(testing::shows(
            &mut app,
            "Click a line to anchor this comment"
        ));
        assert!(testing::shows(
            &mut app,
            "http.rs:20 · This retry loop is gone in the new version."
        ));
        assert!(testing::shows(&mut app, "Make it a general comment"));
    }

    #[test]
    fn a_click_on_a_line_moves_the_comment_there() {
        let (mut app, pr) = reanchoring("i3");
        let line = line_button(&mut app);
        let (path, row_line, side) = {
            let b = app
                .world()
                .get::<crate::screens::review::diff::LineButton>(line)
                .unwrap();
            (b.path.clone(), b.line, b.side)
        };
        testing::activate(&mut app, line);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::ReanchorDraftItem {
                pr: pr.clone(),
                id: "i3".into(),
                anchor: Some(AnchorInput {
                    path,
                    line: row_line,
                    start_line: None,
                    side,
                }),
            }]
        );
        assert_eq!(
            testing::tab(&app, &pr).ui.editor,
            None,
            "the click is not a new comment"
        );
        // A second click while it waits is ignored.
        testing::activate(&mut app, line);
        assert!(testing::recorded(&mut app).is_empty());
        // The daemon answers: the mode ends.
        testing::tell(
            &mut app,
            Tell::Revalidation(RevalidationTell::Reanchored {
                pr: pr.clone(),
                item: "i3".into(),
            }),
        );
        assert_eq!(testing::tab(&app, &pr).ui.reanchor, None);
        assert_eq!(testing::count::<ReanchorStrip>(&mut app), 0);
    }

    #[test]
    fn a_refused_place_keeps_the_mode_and_can_be_tried_again() {
        let (mut app, pr) = reanchoring("i3");
        let line = line_button(&mut app);
        testing::activate(&mut app, line);
        testing::recorded(&mut app);
        testing::tell(
            &mut app,
            Tell::Revalidation(RevalidationTell::Refused {
                pr: pr.clone(),
                item: Some("i3".into()),
            }),
        );
        assert_eq!(testing::tab(&app, &pr).ui.reanchor.as_deref(), Some("i3"));
        testing::activate(&mut app, line);
        assert_eq!(testing::recorded(&mut app).len(), 1);
    }

    #[test]
    fn escape_and_cancel_end_the_mode_without_sending() {
        let (mut app, pr) = reanchoring("i3");
        press(&mut app, KeyCode::Escape);
        assert_eq!(testing::tab(&app, &pr).ui.reanchor, None);
        let (mut app, pr) = reanchoring("i3");
        let cancel = testing::find::<ReanchorCancel>(&mut app, |_| true);
        testing::activate(&mut app, cancel);
        assert_eq!(testing::tab(&app, &pr).ui.reanchor, None);
        assert!(testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn escape_with_a_comment_editor_open_keeps_the_mode() {
        let (mut app, pr) = reanchoring("i3");
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .editor = Some(crate::review_state::Editor {
            target: crate::review_state::EditTarget::General,
            text: String::new(),
            error: None,
            ticket: None,
            mode: crate::ui::composer::ComposerMode::Write,
        });
        press(&mut app, KeyCode::Escape);
        assert_eq!(testing::tab(&app, &pr).ui.reanchor.as_deref(), Some("i3"));
    }

    #[test]
    fn make_it_a_general_comment_sends_no_place() {
        let (mut app, pr) = reanchoring("i3");
        let general = testing::find::<ReanchorGeneral>(&mut app, |_| true);
        testing::activate(&mut app, general);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::ReanchorDraftItem {
                pr,
                id: "i3".into(),
                anchor: None,
            }]
        );
    }

    #[test]
    fn a_cached_copy_does_not_re_anchor() {
        let (mut app, pr) = reanchoring("i3");
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            tabs.0
                .get_mut(&pr)
                .and_then(Tab::ready_mut)
                .unwrap()
                .cached_at = Some(NOW - 60);
        }
        testing::settle(&mut app);
        let line = line_button(&mut app);
        testing::activate(&mut app, line);
        assert!(testing::recorded(&mut app).is_empty());
        assert!(!app.world().resource::<Toasts>().0.is_empty());
        assert_eq!(testing::tab(&app, &pr).ui.reanchor.as_deref(), Some("i3"));
    }

    #[test]
    fn a_comment_that_leaves_the_draft_ends_the_mode() {
        let (mut app, pr) = reanchoring("i3");
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            let ready = tabs.0.get_mut(&pr).and_then(Tab::ready_mut).unwrap();
            ready.view.review.draft.items.retain(|i| i.id != "i3");
        }
        testing::settle(&mut app);
        assert_eq!(testing::tab(&app, &pr).ui.reanchor, None);
        assert_eq!(testing::count::<ReanchorStrip>(&mut app), 0);
    }

    #[test]
    fn in_demo_mode_a_click_re_anchors_the_comment() {
        let mut app = testing::app(fixture::demo(NOW));
        app.add_systems(Update, crate::bridge::demo_answers);
        let pr = testing::open_ready(&mut app, false);
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            let ui = &mut tabs.0.get_mut(&pr).unwrap().ui;
            ui.reanchor = Some("i3".into());
            ui.section = ReviewSection::Diff;
            ui.file = Some("src/client/http.rs".into());
        }
        testing::settle(&mut app);
        let line = line_button(&mut app);
        let target = app
            .world()
            .get::<crate::screens::review::diff::LineButton>(line)
            .unwrap()
            .line;
        testing::activate(&mut app, line);
        testing::settle(&mut app);
        let ready = testing::ready(&app, &pr);
        let item = ready
            .view
            .review
            .draft
            .items
            .iter()
            .find(|i| i.id == "i3")
            .unwrap();
        assert_eq!(item.anchor.as_ref().map(|a| a.line), Some(target));
        assert_eq!(item.status, ItemStatus::Ok);
        assert_eq!(testing::tab(&app, &pr).ui.reanchor, None);
    }

    #[test]
    fn in_demo_mode_a_general_comment_loses_its_anchor() {
        let mut app = testing::app(fixture::demo(NOW));
        app.add_systems(Update, crate::bridge::demo_answers);
        let pr = testing::open_ready(&mut app, false);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .reanchor = Some("i3".into());
        testing::settle(&mut app);
        let general = testing::find::<ReanchorGeneral>(&mut app, |_| true);
        testing::activate(&mut app, general);
        testing::settle(&mut app);
        let ready = testing::ready(&app, &pr);
        let item = ready
            .view
            .review
            .draft
            .items
            .iter()
            .find(|i| i.id == "i3")
            .unwrap();
        assert_eq!(
            (item.kind, item.anchor.clone(), item.status.clone()),
            (clusia_core::DraftKind::General, None, ItemStatus::Ok)
        );
    }
}
