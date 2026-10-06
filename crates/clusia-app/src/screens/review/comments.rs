//! Comments (spec §7.2, mockup `Comments.png`): the general discussion and the file threads,
//! filtered Open / Resolved / All. Replies and resolves join the draft; nothing is posted
//! until the review is published.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy::ui_widgets::{Activate, ScrollArea, observe};
use clusia_core::PrRef;
use clusia_core::draft::{DraftKind, ThreadRef};
use clusia_core::prdata::ReviewThread;
use clusia_core::time::parse_rfc3339;
use clusia_view::status::format_age;

use crate::bridge::{Ask, Asks, Model, Toasts};
use crate::clock::Clock;
use crate::fonts::UiFonts;
use crate::review_state::{
    CommentsFilter, EditTarget, Editor, Phase, Ready, ReviewSection, ReviewTabs, TabUi, Tickets,
};
use crate::screens::review::ReviewSystems;
use crate::screens::review::diff::{DraftEdit, DraftRemove, on_edit, on_remove, unsent};
use crate::screens::review::editor::{EditorArea, editor_box, not_sent, read_only_reason};
use crate::screens::review::shell::SectionBody;
use crate::theme::Swatch;
use crate::ui::kit::{
    Stroke, Tone, Type, Variant, avatar, badge, button, card, chip, divider, panel, text,
};
use crate::ui::text_area::set_text;

#[derive(Debug, Clone, PartialEq)]
pub struct ChipView {
    pub filter: CommentsFilter,
    pub label: String,
    pub on: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PostView {
    pub author: String,
    /// "reviewed" / "commented" in the general discussion, empty in threads.
    pub verb: &'static str,
    /// "3h ago", "just now", or empty when GitHub sent no usable time.
    pub age: String,
    /// Plain text (Markdown rendering is M5c).
    pub body: String,
    pub badge: Option<(&'static str, Tone)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveState {
    /// Resolved already, or GitHub does not let the viewer resolve it.
    Hidden,
    Offer,
    /// The draft resolves it on publish; `item` is the Resolve item to remove on Undo.
    Marked {
        item: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftReply {
    pub id: String,
    pub body: String,
    /// The shared editor replaces this card.
    pub editing: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThreadView {
    pub thread: ThreadRef,
    /// "src/auth/refresh.rs · line 41".
    pub heading: String,
    pub posts: Vec<PostView>,
    pub resolved: bool,
    pub outdated: bool,
    pub can_reply: bool,
    pub resolve: ResolveState,
    pub drafts: Vec<DraftReply>,
    /// The reply editor sits under this thread.
    pub editing: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommentsView {
    pub chips: Vec<ChipView>,
    /// Empty under the Resolved filter.
    pub general: Vec<PostView>,
    pub threads: Vec<ThreadView>,
    pub empty: Option<&'static str>,
}

/// "3h ago" from an RFC 3339 time; "just now" under a minute; empty when unparsable.
pub fn age(now: i64, at: &str) -> String {
    match parse_rfc3339(at) {
        Some(t) => match format_age(now, t).as_str() {
            "now" => "just now".to_string(),
            a => format!("{a} ago"),
        },
        None => String::new(),
    }
}

fn verdict_badge(state: &str) -> Option<(&'static str, Tone)> {
    match state {
        "APPROVED" => Some(("approved", Tone::Green)),
        "CHANGES_REQUESTED" => Some(("changes requested", Tone::Orange)),
        "COMMENTED" => Some(("commented", Tone::Neutral)),
        "DISMISSED" => Some(("dismissed", Tone::Neutral)),
        _ => None,
    }
}

fn thread_ref(t: &ReviewThread) -> ThreadRef {
    ThreadRef {
        id: t.id.clone(),
        author: t
            .comments
            .first()
            .map_or_else(|| "ghost".to_string(), |c| c.author.clone()),
        path: Some(t.path.clone()),
        line: t.line,
    }
}

pub fn comments_view(ready: &Ready, ui: &TabUi, now: i64) -> CommentsView {
    let empty_conversation = Default::default();
    let conversation = ready
        .view
        .conversation
        .as_ref()
        .unwrap_or(&empty_conversation);
    let draft = &ready.view.review.draft;
    let target = ui.editor.as_ref().map(|e| &e.target);

    // General discussion: reviews with something to say, and issue comments, oldest first.
    let mut general: Vec<(i64, PostView)> = Vec::new();
    for r in &conversation.reviews {
        let verdict = matches!(r.state.as_str(), "APPROVED" | "CHANGES_REQUESTED");
        if r.body.trim().is_empty() && !verdict {
            continue;
        }
        let at = r.submitted_at.as_deref().unwrap_or("");
        general.push((
            parse_rfc3339(at).unwrap_or(0),
            PostView {
                author: r.author.clone(),
                verb: "reviewed",
                age: age(now, at),
                body: r.body.trim().to_string(),
                badge: verdict_badge(&r.state),
            },
        ));
    }
    for c in &conversation.comments {
        general.push((
            parse_rfc3339(&c.created_at).unwrap_or(0),
            PostView {
                author: c.author.clone(),
                verb: "commented",
                age: age(now, &c.created_at),
                body: c.body.trim().to_string(),
                badge: None,
            },
        ));
    }
    general.sort_by_key(|(t, _)| *t);
    let general: Vec<PostView> = general.into_iter().map(|(_, p)| p).collect();

    let mut threads: Vec<&ReviewThread> = conversation
        .review_threads
        .iter()
        .filter(|t| !t.comments.is_empty())
        .collect();
    threads.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    let open_threads = threads.iter().filter(|t| !t.is_resolved).count();
    let resolved_threads = threads.len() - open_threads;
    let chips = [
        (
            CommentsFilter::Open,
            format!("Open · {}", general.len() + open_threads),
        ),
        (
            CommentsFilter::Resolved,
            format!("Resolved · {resolved_threads}"),
        ),
        (CommentsFilter::All, "All".to_string()),
    ]
    .into_iter()
    .map(|(filter, label)| ChipView {
        filter,
        label,
        on: ui.filter == filter,
    })
    .collect();

    let shown: Vec<ThreadView> = threads
        .into_iter()
        .filter(|t| match ui.filter {
            CommentsFilter::Open => !t.is_resolved,
            CommentsFilter::Resolved => t.is_resolved,
            CommentsFilter::All => true,
        })
        .map(|t| {
            let of_thread = |kind: DraftKind| {
                draft.items.iter().filter(move |i| {
                    i.kind == kind && i.thread.as_ref().is_some_and(|r| r.id == t.id)
                })
            };
            let resolve = match of_thread(DraftKind::Resolve).next() {
                Some(item) => ResolveState::Marked {
                    item: item.id.clone(),
                },
                None if t.viewer_can_resolve && !t.is_resolved => ResolveState::Offer,
                None => ResolveState::Hidden,
            };
            let drafts = of_thread(DraftKind::Reply)
                .map(|i| DraftReply {
                    id: i.id.clone(),
                    body: i.body.clone(),
                    editing: matches!(target, Some(EditTarget::Item(id)) if *id == i.id),
                })
                .collect();
            ThreadView {
                thread: thread_ref(t),
                heading: match t.line {
                    Some(line) => format!("{} · line {line}", t.path),
                    None => t.path.clone(),
                },
                posts: t
                    .comments
                    .iter()
                    .map(|c| PostView {
                        author: c.author.clone(),
                        verb: "",
                        age: age(now, &c.created_at),
                        body: c.body.trim().to_string(),
                        badge: None,
                    })
                    .collect(),
                resolved: t.is_resolved,
                outdated: t.is_outdated,
                can_reply: t.viewer_can_reply,
                resolve,
                drafts,
                editing: matches!(target, Some(EditTarget::Reply(r)) if r.id == t.id),
            }
        })
        .collect();
    let general = if ui.filter == CommentsFilter::Resolved {
        Vec::new()
    } else {
        general
    };
    let empty = (general.is_empty() && shown.is_empty()).then_some(match ui.filter {
        CommentsFilter::Open => "No open conversations",
        CommentsFilter::Resolved => "No resolved threads",
        CommentsFilter::All => "No comments yet",
    });
    CommentsView {
        chips,
        general,
        threads: shown,
        empty,
    }
}

/// The Comments section's root inside `SectionBody` (A1: its absence means "fill me").
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CommentsRegion(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FilterChip {
    pub pr: PrRef,
    pub filter: CommentsFilter,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct NewThread(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct GeneralReply {
    pub pr: PrRef,
    pub author: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CommentReply {
    pub pr: PrRef,
    pub thread: ThreadRef,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ResolveButton {
    pub pr: PrRef,
    pub thread: ThreadRef,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct UndoResolve {
    pub pr: PrRef,
    pub id: String,
}

#[derive(Component)]
struct ListPart {
    pr: PrRef,
    built: Option<(CommentsView, Option<String>)>,
}

pub struct CommentsPlugin;

impl Plugin for CommentsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (fill_region, rebuild_comments).chain().after(ReviewSystems),
        );
    }
}

fn fill_region(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    bodies: Query<(Entity, &SectionBody)>,
    regions: Query<&ChildOf, With<CommentsRegion>>,
) {
    for (entity, body) in &bodies {
        let Some(tab) = tabs.0.get(&body.pr) else {
            continue;
        };
        if tab.ui.section != ReviewSection::Comments || !matches!(tab.phase, Phase::Ready(_)) {
            continue;
        }
        if regions.iter().any(|child_of| child_of.parent() == entity) {
            continue;
        }
        commands.entity(entity).despawn_related::<Children>();
        let pr = body.pr.clone();
        commands.entity(entity).with_children(|p| {
            p.spawn((
                Node {
                    flex_grow: 1.0,
                    width: percent(100),
                    min_height: px(0),
                    flex_direction: FlexDirection::Column,
                    overflow: Overflow::scroll_y(),
                    ..default()
                },
                ScrollArea,
                ScrollPosition::default(),
                CommentsRegion(pr.clone()),
            ))
            .with_children(|r| {
                r.spawn((
                    Node {
                        flex_direction: FlexDirection::Column,
                        row_gap: px(12),
                        padding: UiRect::axes(px(28), px(20)),
                        max_width: px(920),
                        ..default()
                    },
                    ListPart { pr, built: None },
                ));
            });
        });
    }
}

fn rebuild_comments(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    clock: Res<Clock>,
    fonts: Res<UiFonts>,
    mut parts: Query<(Entity, &mut ListPart)>,
) {
    for (entity, mut part) in &mut parts {
        if part.built.is_some() && !tabs.is_changed() {
            continue;
        }
        let Some(tab) = tabs.0.get(&part.pr) else {
            continue;
        };
        let Phase::Ready(ready) = &tab.phase else {
            continue;
        };
        let v = comments_view(ready, &tab.ui, clock.now());
        let error = tab.ui.editor.as_ref().and_then(|e| e.error.clone());
        let key = (v, error);
        if part.built.as_ref() == Some(&key) {
            continue;
        }
        let pr = part.pr.clone();
        let editor = tab.ui.editor.clone();
        let fonts = &*fonts;
        commands.entity(entity).despawn_related::<Children>();
        commands.entity(entity).with_children(|p| {
            list(p, fonts, &pr, &key.0, editor.as_ref());
        });
        part.built = Some(key);
    }
}

fn list(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    v: &CommentsView,
    editor: Option<&Editor>,
) {
    p.spawn(Node {
        column_gap: px(8),
        align_items: AlignItems::Center,
        ..default()
    })
    .with_children(|row| {
        for c in &v.chips {
            row.spawn((
                chip(fonts, &c.label, c.on),
                FilterChip {
                    pr: pr.clone(),
                    filter: c.filter,
                },
                observe(on_chip),
            ));
        }
        row.spawn(Node {
            flex_grow: 1.0,
            ..default()
        });
        row.spawn((
            button(fonts, "New thread", Variant::Secondary),
            NewThread(pr.clone()),
            observe(on_new_thread),
        ));
    });
    if let Some(empty) = v.empty {
        p.spawn(Node {
            padding: UiRect::vertical(px(24)),
            ..default()
        })
        .with_children(|e| {
            e.spawn(text(fonts, empty, Type::MUTED));
        });
    }
    if !v.general.is_empty() {
        p.spawn(text(
            fonts,
            "General discussion",
            Type::STRONG.size(12.0).ink(Swatch::Muted),
        ));
        for post in &v.general {
            p.spawn(card(Node {
                flex_direction: FlexDirection::Column,
                ..default()
            }))
            .with_children(|c| {
                post_row(c, fonts, post);
                c.spawn(divider());
                c.spawn(footer()).with_children(|f| {
                    f.spawn((
                        button(fonts, "Reply", Variant::Ghost),
                        GeneralReply {
                            pr: pr.clone(),
                            author: post.author.clone(),
                        },
                        observe(on_general_reply),
                    ));
                });
            });
        }
    }
    for t in &v.threads {
        p.spawn(Node {
            column_gap: px(8),
            align_items: AlignItems::Center,
            margin: UiRect::top(px(6)),
            ..default()
        })
        .with_children(|h| {
            h.spawn(text(
                fonts,
                t.heading.clone(),
                Type::MONO.ink(Swatch::Muted),
            ));
            if t.outdated {
                h.spawn(badge(fonts, "outdated", Tone::Orange));
            }
            if t.resolved {
                h.spawn(badge(fonts, "resolved", Tone::Neutral));
            }
        });
        p.spawn(card(Node {
            flex_direction: FlexDirection::Column,
            ..default()
        }))
        .with_children(|c| {
            for (i, post) in t.posts.iter().enumerate() {
                if i > 0 {
                    c.spawn(divider());
                }
                post_row(c, fonts, post);
            }
            for d in &t.drafts {
                c.spawn(divider());
                if d.editing {
                    if let Some(editor) = editor {
                        c.spawn(Node {
                            padding: UiRect::axes(px(16), px(12)),
                            flex_direction: FlexDirection::Column,
                            ..default()
                        })
                        .with_children(|e| editor_box(e, fonts, editor, pr));
                    }
                } else {
                    draft_reply(c, fonts, pr, d);
                }
            }
            if t.editing
                && let Some(editor) = editor
            {
                c.spawn(divider());
                c.spawn(Node {
                    padding: UiRect::axes(px(16), px(12)),
                    flex_direction: FlexDirection::Column,
                    ..default()
                })
                .with_children(|e| editor_box(e, fonts, editor, pr));
            }
            c.spawn(divider());
            c.spawn(footer()).with_children(|f| {
                if t.can_reply {
                    f.spawn((
                        button(fonts, "Reply", Variant::Ghost),
                        CommentReply {
                            pr: pr.clone(),
                            thread: t.thread.clone(),
                        },
                        observe(on_reply),
                    ));
                }
                match &t.resolve {
                    ResolveState::Hidden => {}
                    ResolveState::Offer => {
                        f.spawn((
                            button(fonts, "Resolve", Variant::Ghost),
                            ResolveButton {
                                pr: pr.clone(),
                                thread: t.thread.clone(),
                            },
                            observe(on_resolve),
                        ));
                    }
                    ResolveState::Marked { item } => {
                        f.spawn(text(
                            fonts,
                            "Will resolve on publish",
                            Type::BODY.ink(Swatch::Green),
                        ));
                        f.spawn((
                            button(fonts, "Undo", Variant::Ghost),
                            UndoResolve {
                                pr: pr.clone(),
                                id: item.clone(),
                            },
                            observe(on_undo),
                        ));
                    }
                }
            });
        });
    }
}

fn footer() -> impl Bundle {
    Node {
        padding: UiRect::new(px(52), px(16), px(6), px(6)),
        column_gap: px(4),
        align_items: AlignItems::Center,
        ..default()
    }
}

fn post_row(p: &mut ChildSpawnerCommands, fonts: &UiFonts, post: &PostView) {
    p.spawn(Node {
        padding: UiRect::axes(px(14), px(12)),
        column_gap: px(12),
        align_items: AlignItems::FlexStart,
        ..default()
    })
    .with_children(|r| {
        r.spawn(avatar(fonts, &post.author));
        r.spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(4),
            flex_grow: 1.0,
            min_width: px(0),
            ..default()
        })
        .with_children(|c| {
            c.spawn(Node {
                column_gap: px(8),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|h| {
                h.spawn(text(fonts, format!("@{}", post.author), Type::STRONG));
                let meta = [post.verb, post.age.as_str()]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                if !meta.is_empty() {
                    h.spawn(text(fonts, meta, Type::META));
                }
                if let Some((label, tone)) = post.badge {
                    h.spawn(badge(fonts, label, tone));
                }
            });
            c.spawn(text(fonts, post.body.clone(), Type::BODY));
        });
    });
}

fn draft_reply(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, d: &DraftReply) {
    p.spawn((
        panel(
            Node {
                margin: UiRect::axes(px(14), px(10)),
                padding: UiRect::axes(px(12), px(10)),
                flex_direction: FlexDirection::Column,
                row_gap: px(6),
                border: px(1).all(),
                border_radius: BorderRadius::all(px(8)),
                ..default()
            },
            Swatch::Chrome,
        ),
        BorderColor::default(),
        Stroke(Swatch::Green),
    ))
    .with_children(|c| {
        c.spawn(Node {
            column_gap: px(8),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|h| {
            h.spawn(badge(fonts, "Draft", Tone::Green));
            h.spawn(text(fonts, "You", Type::MUTED));
            h.spawn(Node {
                flex_grow: 1.0,
                ..default()
            });
            h.spawn((
                button(fonts, "Edit", Variant::Ghost),
                DraftEdit {
                    pr: pr.clone(),
                    id: d.id.clone(),
                },
                observe(on_edit),
            ));
            h.spawn((
                button(fonts, "Remove", Variant::Ghost),
                DraftRemove {
                    pr: pr.clone(),
                    id: d.id.clone(),
                },
                observe(on_remove),
            ));
        });
        c.spawn(text(fonts, d.body.clone(), Type::BODY));
    });
}

/// Opens the shared editor for `target`, unless one holds unsent text (as in the diff). When
/// the same target is already open its text area stays drawn, so `text` goes into it too
/// (otherwise the area would copy its old text back over the prefill).
fn open_editor(
    tabs: &mut ReviewTabs,
    areas: &mut Query<(&EditorArea, &mut EditableText)>,
    pr: &PrRef,
    target: EditTarget,
    text: String,
) {
    let Some(tab) = tabs.0.get_mut(pr) else {
        return;
    };
    if unsent(tab.ui.editor.as_ref()) {
        return;
    }
    if tab.ui.editor.as_ref().is_some_and(|e| e.target == target) {
        for (_, mut editable) in areas.iter_mut().filter(|(a, _)| &a.0 == pr) {
            set_text(&mut editable, &text);
        }
    }
    tab.ui.editor = Some(Editor {
        target,
        text,
        error: None,
        ticket: None,
    });
}

fn on_chip(activate: On<Activate>, chips: Query<&FilterChip>, mut tabs: ResMut<ReviewTabs>) {
    if let Ok(c) = chips.get(activate.entity)
        && let Some(tab) = tabs.0.get_mut(&c.pr)
    {
        tab.ui.filter = c.filter;
    }
}

fn on_new_thread(
    activate: On<Activate>,
    buttons: Query<&NewThread>,
    mut tabs: ResMut<ReviewTabs>,
    mut areas: Query<(&EditorArea, &mut EditableText)>,
) {
    if let Ok(NewThread(pr)) = buttons.get(activate.entity) {
        open_editor(
            &mut tabs,
            &mut areas,
            pr,
            EditTarget::General,
            String::new(),
        );
    }
}

fn on_general_reply(
    activate: On<Activate>,
    buttons: Query<&GeneralReply>,
    mut tabs: ResMut<ReviewTabs>,
    mut areas: Query<(&EditorArea, &mut EditableText)>,
) {
    if let Ok(b) = buttons.get(activate.entity) {
        let text = format!("@{} ", b.author);
        open_editor(&mut tabs, &mut areas, &b.pr, EditTarget::General, text);
    }
}

fn on_reply(
    activate: On<Activate>,
    buttons: Query<&CommentReply>,
    mut tabs: ResMut<ReviewTabs>,
    mut areas: Query<(&EditorArea, &mut EditableText)>,
) {
    if let Ok(b) = buttons.get(activate.entity) {
        let target = EditTarget::Reply(b.thread.clone());
        open_editor(&mut tabs, &mut areas, &b.pr, target, String::new());
    }
}

fn on_resolve(
    activate: On<Activate>,
    buttons: Query<&ResolveButton>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut tickets: ResMut<Tickets>,
    mut asks: ResMut<Asks>,
) {
    if let Ok(b) = buttons.get(activate.entity) {
        if let Some(reason) = read_only_reason(&tabs, &model, &b.pr) {
            return not_sent(&mut toasts, &time, reason);
        }
        asks.send(Ask::AddItem {
            pr: b.pr.clone(),
            kind: DraftKind::Resolve,
            anchor: None,
            thread: Some(b.thread.clone()),
            body: String::new(),
            ticket: tickets.issue(),
        });
    }
}

fn on_undo(
    activate: On<Activate>,
    buttons: Query<&UndoResolve>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    if let Ok(b) = buttons.get(activate.entity) {
        if let Some(reason) = read_only_reason(&tabs, &model, &b.pr) {
            return not_sent(&mut toasts, &time, reason);
        }
        asks.send(Ask::RemoveItem {
            pr: b.pr.clone(),
            id: b.id.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::testing::{self, NOW};

    fn ready() -> Ready {
        let (view, news) = fixture::demo_review(NOW);
        Ready {
            view,
            news,
            cached_at: None,
        }
    }

    fn threads_mut(r: &mut Ready) -> &mut Vec<ReviewThread> {
        &mut r
            .view
            .conversation
            .as_mut()
            .expect("demo conversation")
            .review_threads
    }

    fn mona_thread(r: &Ready) -> &ReviewThread {
        r.view
            .conversation
            .as_ref()
            .unwrap()
            .review_threads
            .iter()
            .find(|t| !t.is_resolved && t.comments[0].author == "mona")
            .expect("mona's open thread")
    }

    fn with_ready(app: &mut App, change: impl FnOnce(&mut Ready, &mut TabUi)) {
        let pr = fixture::demo_pr();
        let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
        let tab = tabs.0.get_mut(&pr).unwrap();
        let Phase::Ready(ready) = &mut tab.phase else {
            panic!("demo tab is ready")
        };
        change(ready, &mut tab.ui);
    }

    fn comments_app() -> App {
        let mut app = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut app, false);
        with_ready(&mut app, |r, ui| {
            for t in threads_mut(r) {
                t.viewer_can_reply = true;
                t.viewer_can_resolve = true;
            }
            ui.section = ReviewSection::Comments;
        });
        testing::settle(&mut app);
        app
    }

    fn has_text(app: &mut App, needle: &str) -> bool {
        let mut q = app.world_mut().query::<&Text>();
        q.iter(app.world()).any(|t| t.0.contains(needle))
    }

    #[test]
    fn ages() {
        assert_eq!(age(NOW, "not a date"), "");
        let at = |secs: i64| {
            let (y, m, d) = clusia_core::time::civil_from_days((NOW - secs).div_euclid(86_400));
            let s = (NOW - secs).rem_euclid(86_400);
            format!(
                "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
                s / 3600,
                s / 60 % 60,
                s % 60
            )
        };
        assert_eq!(age(NOW, &at(10)), "just now");
        assert_eq!(age(NOW, &at(3 * 3600)), "3h ago");
        assert_eq!(age(NOW, &at(2 * 86_400)), "2d ago");
    }

    #[test]
    fn demo_comments_view() {
        let mut r = ready();
        for t in threads_mut(&mut r) {
            t.viewer_can_resolve = true;
            t.viewer_can_reply = true;
        }
        let v = comments_view(&r, &TabUi::default(), NOW);
        let labels: Vec<&str> = v.chips.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            labels,
            ["Open · 4", "Resolved · 1", "All"],
            "joao, octo and hubot in the general discussion + mona's thread; ana's thread resolved"
        );
        assert!(v.chips[0].on && !v.chips[1].on && !v.chips[2].on);
        let order: Vec<(&str, &str)> = v
            .general
            .iter()
            .map(|p| (p.author.as_str(), p.verb))
            .collect();
        assert_eq!(
            order,
            [
                ("joao", "commented"),
                ("octo", "commented"),
                ("hubot", "reviewed")
            ],
            "oldest first"
        );
        assert_eq!(v.general[0].badge, None);
        let hubot = &v.general[2];
        assert_eq!((hubot.author.as_str(), hubot.verb), ("hubot", "reviewed"));
        assert_eq!(hubot.badge, Some(("approved", Tone::Green)));
        assert_eq!(hubot.body, "Looks good once the lock change lands.");
        assert!(hubot.age.ends_with(" ago"), "{}", hubot.age);
        assert_eq!(v.threads.len(), 1, "Open hides the resolved thread");
        let t = &v.threads[0];
        assert_eq!(t.heading, "src/auth/refresh.rs · line 41");
        let bodies: Vec<&str> = t.posts.iter().map(|p| p.body.as_str()).collect();
        assert_eq!(
            bodies,
            [
                "Why one minute? The CLI uses 30 seconds.",
                "Clock skew on the CI runners. 30 seconds would work too.",
                "A minute is safer while the runners drift."
            ]
        );
        assert_eq!(t.posts[0].author, "mona");
        assert_eq!(t.posts[0].verb, "");
        assert!(t.can_reply && !t.resolved && !t.outdated);
        assert_eq!(t.resolve, ResolveState::Offer);
        assert_eq!(t.thread.author, "mona");
        assert_eq!(
            (t.thread.path.as_deref(), t.thread.line),
            (Some("src/auth/refresh.rs"), Some(41))
        );
        assert!(t.drafts.is_empty() && !t.editing);
        assert_eq!(v.empty, None);
    }

    #[test]
    fn filters_split_open_and_resolved() {
        let r = ready();
        let resolved = comments_view(
            &r,
            &TabUi {
                filter: CommentsFilter::Resolved,
                ..TabUi::default()
            },
            NOW,
        );
        assert!(resolved.general.is_empty());
        assert!(!resolved.threads.is_empty());
        assert!(resolved.threads.iter().all(|t| t.resolved));
        assert_eq!(resolved.threads[0].resolve, ResolveState::Hidden);
        let all = comments_view(
            &r,
            &TabUi {
                filter: CommentsFilter::All,
                ..TabUi::default()
            },
            NOW,
        );
        assert_eq!(all.general.len(), 3);
        assert_eq!(all.threads.len(), 2);
        assert!(all.chips[2].on);
        let mut empty = ready();
        empty.view.conversation = None;
        let v = comments_view(&empty, &TabUi::default(), NOW);
        assert_eq!(v.empty, Some("No open conversations"));
        assert_eq!(v.chips[0].label, "Open · 0");
    }

    #[test]
    fn outdated_and_unresolvable_threads() {
        let mut r = ready();
        let open = threads_mut(&mut r)
            .iter_mut()
            .find(|t| !t.is_resolved)
            .unwrap();
        open.is_outdated = true;
        open.viewer_can_resolve = false;
        open.viewer_can_reply = false;
        let t = &comments_view(&r, &TabUi::default(), NOW).threads[0];
        assert!(t.outdated && !t.can_reply);
        assert_eq!(t.resolve, ResolveState::Hidden);
    }

    #[test]
    fn draft_replies_and_resolves_show_under_their_thread() {
        let mut r = ready();
        let thread = {
            let t = mona_thread(&r);
            ThreadRef {
                id: t.id.clone(),
                author: "mona".into(),
                path: Some(t.path.clone()),
                line: t.line,
            }
        };
        let draft = &mut r.view.review.draft;
        let reply = draft
            .add(
                DraftKind::Reply,
                None,
                Some(thread.clone()),
                "Agree with 30 seconds; the skew we saw on CI was under 10.",
                NOW,
            )
            .unwrap()
            .id
            .clone();
        let resolve = draft
            .add(DraftKind::Resolve, None, Some(thread.clone()), "", NOW)
            .unwrap()
            .id
            .clone();
        let v = comments_view(&r, &TabUi::default(), NOW);
        let t = &v.threads[0];
        assert_eq!(
            t.drafts,
            [DraftReply {
                id: reply.clone(),
                body: "Agree with 30 seconds; the skew we saw on CI was under 10.".into(),
                editing: false,
            }]
        );
        assert_eq!(t.resolve, ResolveState::Marked { item: resolve });
        let editing = TabUi {
            editor: Some(Editor {
                target: EditTarget::Item(reply),
                text: String::new(),
                error: None,
                ticket: None,
            }),
            ..TabUi::default()
        };
        assert!(comments_view(&r, &editing, NOW).threads[0].drafts[0].editing);
        let replying = TabUi {
            editor: Some(Editor {
                target: EditTarget::Reply(thread),
                text: String::new(),
                error: None,
                ticket: None,
            }),
            ..TabUi::default()
        };
        assert!(comments_view(&r, &replying, NOW).threads[0].editing);
    }

    #[test]
    fn chips_new_thread_and_general_reply() {
        let mut app = comments_app();
        let pr = fixture::demo_pr();
        assert_eq!(testing::count::<CommentsRegion>(&mut app), 1);
        assert!(has_text(&mut app, "General discussion"));
        assert!(has_text(
            &mut app,
            "Why one minute? The CLI uses 30 seconds."
        ));
        let resolved =
            testing::find::<FilterChip>(&mut app, |c| c.filter == CommentsFilter::Resolved);
        testing::activate(&mut app, resolved);
        assert_eq!(testing::tab(&app, &pr).ui.filter, CommentsFilter::Resolved);
        testing::settle(&mut app);
        assert!(!has_text(&mut app, "General discussion"));
        let all = testing::find::<FilterChip>(&mut app, |c| c.filter == CommentsFilter::All);
        testing::activate(&mut app, all);
        testing::settle(&mut app);
        // New thread first: the prefilled "@hubot " is unsent text, which no other click drops.
        let new = testing::find::<NewThread>(&mut app, |_| true);
        testing::activate(&mut app, new);
        let editor = testing::tab(&app, &pr).ui.editor.unwrap();
        assert_eq!(
            (editor.target, editor.text.as_str()),
            (EditTarget::General, "")
        );
        let general = testing::find::<GeneralReply>(&mut app, |g| g.author == "hubot");
        testing::activate(&mut app, general);
        let editor = testing::tab(&app, &pr).ui.editor.unwrap();
        assert_eq!(
            (editor.target, editor.text.as_str()),
            (EditTarget::General, "@hubot ")
        );
        testing::settle(&mut app);
        let area = testing::find::<EditorArea>(&mut app, |_| true);
        assert_eq!(
            app.world()
                .get::<EditableText>(area)
                .unwrap()
                .value()
                .to_string(),
            "@hubot ",
            "the open text area shows the prefill"
        );
    }

    #[test]
    fn another_click_never_drops_unsent_text() {
        let mut app = comments_app();
        let pr = fixture::demo_pr();
        let general = testing::find::<GeneralReply>(&mut app, |g| g.author == "hubot");
        testing::activate(&mut app, general);
        testing::settle(&mut app);
        let new = testing::find::<NewThread>(&mut app, |_| true);
        testing::activate(&mut app, new);
        let reply = testing::find::<CommentReply>(&mut app, |r| r.thread.author == "mona");
        testing::activate(&mut app, reply);
        let joao = testing::find::<GeneralReply>(&mut app, |g| g.author == "joao");
        testing::activate(&mut app, joao);
        let editor = testing::tab(&app, &pr).ui.editor.expect("still open");
        assert_eq!(
            (editor.target, editor.text.as_str()),
            (EditTarget::General, "@hubot "),
            "unsent text is never dropped"
        );
        with_ready(&mut app, |_, ui| ui.editor = None);
        let reply = testing::find::<CommentReply>(&mut app, |r| r.thread.author == "mona");
        testing::activate(&mut app, reply);
        assert!(matches!(
            testing::tab(&app, &pr).ui.editor.map(|e| e.target),
            Some(EditTarget::Reply(_))
        ));
    }

    #[test]
    fn reply_opens_the_editor_under_the_thread() {
        let mut app = comments_app();
        let pr = fixture::demo_pr();
        let reply = testing::find::<CommentReply>(&mut app, |r| r.thread.author == "mona");
        testing::activate(&mut app, reply);
        let target = testing::tab(&app, &pr).ui.editor.map(|e| e.target);
        let Some(EditTarget::Reply(thread)) = target else {
            panic!("a reply editor, got {target:?}")
        };
        assert_eq!(thread.line, Some(41));
        testing::settle(&mut app);
        assert_eq!(testing::count::<EditorArea>(&mut app), 1);
    }

    #[test]
    fn resolve_marks_and_undo_removes() {
        let mut app = comments_app();
        let pr = fixture::demo_pr();
        let resolve = testing::find::<ResolveButton>(&mut app, |r| r.thread.author == "mona");
        let thread = app
            .world()
            .get::<ResolveButton>(resolve)
            .unwrap()
            .thread
            .clone();
        testing::activate(&mut app, resolve);
        let asks = testing::recorded(&mut app);
        let [
            Ask::AddItem {
                pr: to,
                kind,
                anchor,
                thread: sent,
                body,
                ticket,
            },
        ] = &asks[..]
        else {
            panic!("one AddItem, got {asks:?}")
        };
        assert_eq!(
            (to, *kind, anchor, sent.as_ref(), body.as_str()),
            (&pr, DraftKind::Resolve, &None, Some(&thread), "")
        );
        assert!(*ticket > 0);
        // The daemon saves it; the draft comes back with the Resolve item.
        let mut id = String::new();
        with_ready(&mut app, |r, _| {
            id = r
                .view
                .review
                .draft
                .add(DraftKind::Resolve, None, Some(thread.clone()), "", NOW)
                .unwrap()
                .id
                .clone();
        });
        testing::settle(&mut app);
        assert!(has_text(&mut app, "Will resolve on publish"));
        assert_eq!(testing::count::<ResolveButton>(&mut app), 0);
        let undo = testing::find::<UndoResolve>(&mut app, |_| true);
        testing::activate(&mut app, undo);
        assert_eq!(testing::recorded(&mut app), [Ask::RemoveItem { pr, id }]);
    }

    #[test]
    fn draft_replies_can_be_edited_in_place() {
        let mut app = comments_app();
        let pr = fixture::demo_pr();
        let mut id = String::new();
        with_ready(&mut app, |r, _| {
            let t = mona_thread(r);
            let thread = ThreadRef {
                id: t.id.clone(),
                author: "mona".into(),
                path: Some(t.path.clone()),
                line: t.line,
            };
            id = r
                .view
                .review
                .draft
                .add(
                    DraftKind::Reply,
                    None,
                    Some(thread),
                    "Agree with 30 seconds.",
                    NOW,
                )
                .unwrap()
                .id
                .clone();
        });
        testing::settle(&mut app);
        assert!(has_text(&mut app, "Agree with 30 seconds."));
        // Remove first: while the item is edited, the editor takes its card's place.
        let remove = testing::find::<DraftRemove>(&mut app, |d| d.id == id);
        testing::activate(&mut app, remove);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::RemoveItem {
                pr: pr.clone(),
                id: id.clone()
            }]
        );
        let edit = testing::find::<DraftEdit>(&mut app, |d| d.id == id);
        testing::activate(&mut app, edit);
        let editor = testing::tab(&app, &pr).ui.editor.unwrap();
        assert_eq!(editor.target, EditTarget::Item(id.clone()));
        assert_eq!(editor.text, "Agree with 30 seconds.");
        testing::settle(&mut app);
        assert_eq!(testing::count::<EditorArea>(&mut app), 1);
        assert_eq!(testing::count::<DraftEdit>(&mut app), 0, "edited in place");
    }

    #[test]
    fn the_cached_copy_sends_no_resolve_or_undo() {
        let mut app = comments_app();
        with_ready(&mut app, |r, _| r.cached_at = Some(NOW - 3600));
        testing::settle(&mut app);
        let resolve = testing::find::<ResolveButton>(&mut app, |r| r.thread.author == "mona");
        testing::activate(&mut app, resolve);
        assert!(testing::recorded(&mut app).is_empty(), "no AddItem");
        with_ready(&mut app, |r, _| {
            let thread = thread_ref(mona_thread(r));
            r.view
                .review
                .draft
                .add(DraftKind::Resolve, None, Some(thread), "", NOW)
                .unwrap();
        });
        testing::settle(&mut app);
        let undo = testing::find::<UndoResolve>(&mut app, |_| true);
        testing::activate(&mut app, undo);
        assert!(testing::recorded(&mut app).is_empty(), "no RemoveItem");
        let toasts = &app.world().resource::<crate::bridge::Toasts>().0;
        assert_eq!(toasts.len(), 2);
        assert!(
            toasts
                .iter()
                .all(|t| t.warning && t.text == crate::screens::review::editor::CACHED_COPY)
        );
    }
}
