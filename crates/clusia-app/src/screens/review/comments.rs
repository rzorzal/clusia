//! Comments (spec §7.2, mockup `Comments.png`): the general discussion and the file threads,
//! filtered Open / Resolved / All. Replies and resolves join the draft; nothing is posted
//! until the review is published.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy::ui_widgets::{Activate, ScrollArea, observe};
use clusia_core::draft::{DraftKind, ThreadRef};
use clusia_core::hidden::{issue_comment_key, review_key, thread_key};
use clusia_core::prdata::ReviewThread;
use clusia_core::time::parse_rfc3339;
use clusia_core::{HiddenState, PrRef};
use clusia_view::status::format_age;

use crate::bridge::{Ask, Asks, Connection, Model, Toasts};
use crate::clock::Clock;
use crate::fonts::UiFonts;
use crate::review_state::{
    CommentsFilter, EditTarget, Editor, Phase, Ready, ReviewSection, ReviewTabs, TabUi, Tickets,
};
use crate::screens::review::ReviewSystems;
use crate::screens::review::diff::{DraftEdit, DraftRemove, on_edit, on_remove, unsent};
use crate::screens::review::editor::{EditorArea, editor_box, not_sent, read_only_reason};
use crate::screens::review::shell::SectionBody;
use crate::theme::{Swatch, Theme};
use crate::ui::composer::ComposerMode;
use crate::ui::kit::{
    Stroke, Tone, Type, Variant, avatar, badge, button, card, chip, divider, panel, text,
    wrapped_text,
};
use crate::ui::markdown::parse::parse;
use crate::ui::markdown::{RenderOpts, copy_button, markdown};
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
    /// Markdown source.
    pub body: String,
    pub badge: Option<(&'static str, Tone)>,
    /// What *Hide* hides: this comment's key, or its thread's.
    pub key: String,
    /// How many posts hiding `key` covers now (a thread's, else 1).
    pub posts: u32,
    pub hidden: HiddenState,
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
    pub hidden: HiddenState,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommentsView {
    pub chips: Vec<ChipView>,
    /// Empty under the Resolved filter.
    pub general: Vec<PostView>,
    pub threads: Vec<ThreadView>,
    pub empty: Option<&'static str>,
    /// The keys of everything collapsed, in the whole review: what *Show all* shows.
    pub collapsed: Vec<String>,
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
                key: review_key(r.id),
                posts: 1,
                hidden: ui.hidden.state(&review_key(r.id), 1),
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
                key: issue_comment_key(c.id),
                posts: 1,
                hidden: ui.hidden.state(&issue_comment_key(c.id), 1),
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
            let hidden = ui.hidden.state(&thread_key(&t.id), t.comments.len() as u32);
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
                        key: thread_key(&t.id),
                        posts: t.comments.len() as u32,
                        hidden,
                    })
                    .collect(),
                hidden,
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
    let mut collapsed: Vec<String> = Vec::new();
    for r in &conversation.reviews {
        collapsed.push(review_key(r.id));
    }
    for c in &conversation.comments {
        collapsed.push(issue_comment_key(c.id));
    }
    for t in &conversation.review_threads {
        collapsed.push(thread_key(&t.id));
    }
    let sizes: std::collections::HashMap<String, u32> = conversation
        .review_threads
        .iter()
        .map(|t| (thread_key(&t.id), t.comments.len() as u32))
        .collect();
    collapsed.retain(|key| {
        ui.hidden.state(key, sizes.get(key).copied().unwrap_or(1)) == HiddenState::Hidden
    });
    CommentsView {
        chips,
        general,
        threads: shown,
        empty,
        collapsed,
    }
}

/// The Comments section's root inside `SectionBody`; its absence means "fill me".
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

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct HideButton {
    pub pr: PrRef,
    pub key: String,
    pub posts: u32,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ShowButton {
    pub pr: PrRef,
    pub key: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct ShowAll {
    pub pr: PrRef,
    pub keys: Vec<String>,
}

#[derive(Component)]
struct ListPart {
    pr: PrRef,
    /// The view, the editor's error, whether it waits for the daemon (**Saving…**) and how
    /// bodies are drawn.
    built: Option<(CommentsView, Option<String>, bool, RenderOpts)>,
}

pub struct CommentsPlugin;

impl Plugin for CommentsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (load_hidden, fill_region, rebuild_comments)
                .chain()
                .after(ReviewSystems),
        );
    }
}

/// Asks for the hidden comments of each ready tab once the connection is live; a reconnect
/// asks again (the bridge clears `hidden_asked`).
fn load_hidden(mut tabs: ResMut<ReviewTabs>, model: Res<Model>, mut asks: ResMut<Asks>) {
    if model.connection != Connection::Live {
        return;
    }
    let due: Vec<PrRef> = tabs
        .0
        .iter()
        .filter(|(_, tab)| !tab.ui.hidden_asked && tab.ready().is_some())
        .map(|(pr, _)| pr.clone())
        .collect();
    for pr in due {
        if let Some(tab) = tabs.0.get_mut(&pr) {
            tab.ui.hidden_asked = true;
        }
        asks.send(Ask::GetHiddenComments(pr));
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
                region_node(),
                ScrollArea,
                ScrollPosition::default(),
                CommentsRegion(pr.clone()),
            ))
            .with_children(|r| {
                r.spawn((list_node(), ListPart { pr, built: None }));
            });
        });
    }
}

/// The scrolling region that holds the list.
fn region_node() -> Node {
    Node {
        flex_grow: 1.0,
        width: percent(100),
        min_height: px(0),
        flex_direction: FlexDirection::Column,
        overflow: Overflow::scroll_y(),
        ..default()
    }
}

/// The list's column. It has no width of its own: it stretches across the region.
fn list_node() -> Node {
    Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(12),
        padding: UiRect::axes(px(28), px(20)),
        ..default()
    }
}

fn rebuild_comments(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    clock: Res<Clock>,
    fonts: Res<UiFonts>,
    theme: Res<Theme>,
    model: Res<Model>,
    mut parts: Query<(Entity, &mut ListPart)>,
) {
    let opts = RenderOpts::from_config(theme.code_size, &model.snapshot.config);
    for (entity, mut part) in &mut parts {
        let opts_changed = part.built.as_ref().is_some_and(|b| b.3 != opts);
        if part.built.is_some() && !tabs.is_changed() && !opts_changed {
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
        let saving = tab.ui.editor.as_ref().is_some_and(|e| e.ticket.is_some());
        let key = (v, error, saving, opts.clone());
        if part.built.as_ref() == Some(&key) {
            continue;
        }
        let pr = part.pr.clone();
        let editor = tab.ui.editor.clone();
        let fonts = &*fonts;
        commands.entity(entity).despawn_related::<Children>();
        commands.entity(entity).with_children(|p| {
            list(p, fonts, &pr, &key.0, editor.as_ref(), &key.3);
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
    opts: &RenderOpts,
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
    if !v.collapsed.is_empty() {
        p.spawn(Node {
            column_gap: px(4),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|n| {
            n.spawn(text(
                fonts,
                format!("{} hidden ·", v.collapsed.len()),
                Type::MUTED,
            ));
            n.spawn((
                button(fonts, "Show all", Variant::Ghost),
                ShowAll {
                    pr: pr.clone(),
                    keys: v.collapsed.clone(),
                },
                observe(on_show_all),
            ));
        });
    }
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
            if post.hidden == HiddenState::Hidden {
                p.spawn(card(Node {
                    flex_direction: FlexDirection::Column,
                    ..default()
                }))
                .with_children(|c| collapsed_row(c, fonts, pr, post));
                continue;
            }
            p.spawn(card(Node {
                flex_direction: FlexDirection::Column,
                ..default()
            }))
            .with_children(|c| {
                post_row(c, fonts, pr, post, opts);
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
            row_gap: px(4),
            flex_wrap: FlexWrap::Wrap,
            align_items: AlignItems::Center,
            margin: UiRect::top(px(6)),
            ..default()
        })
        .with_children(|h| {
            h.spawn((
                wrapped_text(fonts, t.heading.clone(), Type::MONO.ink(Swatch::Muted)),
                Node {
                    max_width: percent(100),
                    ..default()
                },
            ));
            if t.outdated {
                h.spawn(badge(fonts, "outdated", Tone::Orange));
            }
            if t.resolved {
                h.spawn(badge(fonts, "resolved", Tone::Neutral));
            }
            if t.hidden == HiddenState::NewReply {
                h.spawn(badge(fonts, "new reply", Tone::Orange));
            }
        });
        if t.hidden == HiddenState::Hidden
            && let Some(first) = t.posts.first()
        {
            p.spawn(card(Node {
                flex_direction: FlexDirection::Column,
                ..default()
            }))
            .with_children(|c| collapsed_row(c, fonts, pr, first));
            continue;
        }
        p.spawn(card(Node {
            flex_direction: FlexDirection::Column,
            ..default()
        }))
        .with_children(|c| {
            for (i, post) in t.posts.iter().enumerate() {
                if i > 0 {
                    c.spawn(divider());
                }
                post_row(c, fonts, pr, post, opts);
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
                    draft_reply(c, fonts, pr, d, opts);
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

/// A hidden comment as one line: who wrote it, when, and a way back.
fn collapsed_row(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, post: &PostView) {
    p.spawn(Node {
        padding: UiRect::axes(px(14), px(6)),
        column_gap: px(8),
        align_items: AlignItems::Center,
        ..default()
    })
    .with_children(|r| {
        let when = if post.age.is_empty() {
            String::new()
        } else {
            format!(" · {}", post.age)
        };
        r.spawn(text(
            fonts,
            format!("Hidden · @{}{when}", post.author),
            Type::MUTED,
        ));
        r.spawn((
            button(fonts, "Show", Variant::Ghost),
            ShowButton {
                pr: pr.clone(),
                key: post.key.clone(),
            },
            observe(on_show),
        ));
    });
}

fn post_row(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    post: &PostView,
    opts: &RenderOpts,
) {
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
                h.spawn(Node {
                    flex_grow: 1.0,
                    ..default()
                });
                h.spawn(copy_button(fonts, &post.body));
                h.spawn((
                    button(fonts, "Hide", Variant::Ghost),
                    HideButton {
                        pr: pr.clone(),
                        key: post.key.clone(),
                        posts: post.posts,
                    },
                    observe(on_hide),
                ));
            });
            markdown(c, fonts, &parse(&post.body), opts);
        });
    });
}

fn draft_reply(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    d: &DraftReply,
    opts: &RenderOpts,
) {
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
            h.spawn(copy_button(fonts, &d.body));
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
        markdown(c, fonts, &parse(&d.body), opts);
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
        mode: ComposerMode::Write,
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

/// Hides (`posts`) or shows (`None`) `key` in the tab and asks the daemon to keep it. The
/// tab changes at once; the daemon's set replaces it when it answers. Without a live daemon
/// nothing would be kept, so it does neither and says why. The set is local data, so a cached
/// copy still loads it (see `load_hidden`); it just cannot change it.
fn set_hidden(
    tabs: &mut ReviewTabs,
    model: &Model,
    toasts: &mut Toasts,
    time: &Time,
    asks: &mut Asks,
    pr: &PrRef,
    changes: &[(String, Option<u32>)],
) {
    if let Some(reason) = read_only_reason(tabs, model, pr) {
        return not_sent(toasts, time, reason);
    }
    let Some(tab) = tabs.0.get_mut(pr) else {
        return;
    };
    for (key, posts) in changes {
        match posts {
            Some(posts) => tab.ui.hidden.hide(key, *posts),
            None => {
                tab.ui.hidden.show(key);
            }
        }
        asks.send(Ask::SetCommentHidden {
            pr: pr.clone(),
            key: key.clone(),
            posts: *posts,
        });
    }
}

fn on_hide(
    activate: On<Activate>,
    buttons: Query<&HideButton>,
    mut tabs: ResMut<ReviewTabs>,
    model: Res<Model>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    if let Ok(b) = buttons.get(activate.entity) {
        let change = [(b.key.clone(), Some(b.posts))];
        set_hidden(
            &mut tabs,
            &model,
            &mut toasts,
            &time,
            &mut asks,
            &b.pr,
            &change,
        );
    }
}

fn on_show(
    activate: On<Activate>,
    buttons: Query<&ShowButton>,
    mut tabs: ResMut<ReviewTabs>,
    model: Res<Model>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    if let Ok(b) = buttons.get(activate.entity) {
        let change = [(b.key.clone(), None)];
        set_hidden(
            &mut tabs,
            &model,
            &mut toasts,
            &time,
            &mut asks,
            &b.pr,
            &change,
        );
    }
}

fn on_show_all(
    activate: On<Activate>,
    buttons: Query<&ShowAll>,
    mut tabs: ResMut<ReviewTabs>,
    model: Res<Model>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    if let Ok(b) = buttons.get(activate.entity) {
        if let Some(reason) = read_only_reason(&tabs, &model, &b.pr) {
            return not_sent(&mut toasts, &time, reason);
        }
        if let Some(tab) = tabs.0.get_mut(&b.pr) {
            for key in &b.keys {
                tab.ui.hidden.show(key);
            }
        }
        asks.send(Ask::ShowComments {
            pr: b.pr.clone(),
            keys: b.keys.clone(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::testing::{self, NOW};
    use crate::ui::kit::Fill;
    use crate::ui::markdown::{CopiedText, CopyMarkdown, MdBody, MdImage};
    use bevy::ecs::system::RunSystemOnce;

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
            // The demo's rich issue comments load a picture; these tests count pictures and asks.
            if let Some(c) = r.view.conversation.as_mut() {
                for comment in &mut c.comments {
                    comment.body = "Thanks!".into();
                }
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
                mode: ComposerMode::Write,
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
                mode: ComposerMode::Write,
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
        assert!(testing::shows(
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
        assert!(testing::shows(&mut app, "Agree with 30 seconds."));
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
    fn the_comment_column_fills_the_region_on_a_wide_window() {
        use bevy::camera::{Camera, ComputedCameraValues, RenderTargetInfo, Viewport};
        use bevy::ui::ComputedNode;

        let mut app = App::new();
        app.add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
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
        let size = UVec2::new(2400, 800);
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
        let (region, list) = {
            let world = app.world_mut();
            let root = world
                .spawn(Node {
                    width: percent(100),
                    height: percent(100),
                    flex_direction: FlexDirection::Column,
                    ..default()
                })
                .id();
            let region = world.spawn((region_node(), ChildOf(root))).id();
            let list = world.spawn((list_node(), ChildOf(region))).id();
            // A card as wide as its text wants, so the column has something to stretch around.
            world.spawn((
                Node {
                    width: px(300),
                    height: px(40),
                    ..default()
                },
                ChildOf(list),
            ));
            (region, list)
        };
        app.update();
        app.update();
        let region = app.world().get::<ComputedNode>(region).unwrap();
        let list = app.world().get::<ComputedNode>(list).unwrap();
        assert!(region.size.x >= 2000.0, "the region spans the window");
        let inner = region.size.x - region.scrollbar_size.x;
        assert_eq!(
            list.size.x, inner,
            "the column reaches the region's inner width"
        );
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

    #[test]
    fn a_waiting_reply_shows_saving() {
        let mut app = comments_app();
        let reply = testing::find::<CommentReply>(&mut app, |r| r.thread.author == "mona");
        testing::activate(&mut app, reply);
        testing::settle(&mut app);
        let area = testing::find::<EditorArea>(&mut app, |_| true);
        testing::type_into(&mut app, area, "Agreed, will do.");
        let submit =
            testing::find::<crate::screens::review::editor::EditorSubmit>(&mut app, |_| true);
        testing::activate(&mut app, submit);
        testing::settle(&mut app);
        assert!(matches!(
            testing::recorded(&mut app).as_slice(),
            [Ask::AddItem { .. }]
        ));
        assert!(has_text(&mut app, "Saving…"));
    }

    /// Replaces the first comment of the open thread and shows Comments again.
    fn set_first_body(app: &mut App, body: &str) {
        let body = body.to_string();
        with_ready(app, |r, _| {
            let thread = threads_mut(r)
                .iter_mut()
                .find(|t| !t.is_resolved)
                .expect("an open thread");
            thread.comments[0].body = body;
        });
        testing::settle(app);
    }

    /// Every node whose whole text is `word`.
    fn word_entities(app: &mut App, word: &str) -> Vec<Entity> {
        let mut q = app.world_mut().query::<(Entity, &Text)>();
        q.iter(app.world())
            .filter(|(_, t)| t.0 == word)
            .map(|(e, _)| e)
            .collect()
    }

    fn weights(app: &mut App, word: &str) -> Vec<u16> {
        word_entities(app, word)
            .into_iter()
            .map(|e| app.world().get::<TextFont>(e).unwrap().weight.0)
            .collect()
    }

    #[test]
    fn comment_bodies_render_as_markdown() {
        let mut app = comments_app();
        let url = "https://github.com/user-attachments/assets/1.png";
        set_first_body(
            &mut app,
            &format!("**Holding** the lock `expires_at` ![graph]({url})"),
        );
        assert!(weights(&mut app, "Holding").contains(&600), "a bold word");
        let chips = word_entities(&mut app, "expires_at");
        assert!(
            chips
                .iter()
                .any(|e| app.world().get::<Fill>(*e).map(|f| f.0) == Some(Swatch::Line)),
            "a code chip"
        );
        assert!(!testing::shows(&mut app, "**"), "no raw markdown");
        assert_eq!(testing::count::<MdImage>(&mut app), 1);
        assert!(testing::recorded(&mut app).contains(&Ask::FetchMedia(url.to_string())));
    }

    #[test]
    fn other_sites_stay_links_until_the_setting_is_on() {
        let mut app = comments_app();
        set_first_body(&mut app, "see ![x](https://example.com/x.png)");
        assert_eq!(testing::count::<MdImage>(&mut app), 0);
        assert!(testing::shows(
            &mut app,
            "Image from another site — open in browser"
        ));
        app.world_mut()
            .resource_mut::<Model>()
            .snapshot
            .config
            .media
            .load_external_images = true;
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<MdImage>(&mut app),
            1,
            "the setting redraws"
        );
    }

    #[test]
    fn every_full_comment_has_copy_markdown() {
        let mut app = comments_app();
        let source = "**Why** one minute?";
        set_first_body(&mut app, source);
        // Three general posts and the three comments of mona's thread.
        assert_eq!(testing::count::<CopyMarkdown>(&mut app), 6);
        let copy = testing::find::<CopyMarkdown>(&mut app, |c| c.0 == source);
        testing::activate(&mut app, copy);
        assert_eq!(app.world().resource::<CopiedText>().0, [source]);
    }

    #[test]
    fn draft_replies_render_markdown_and_copy() {
        let mut app = comments_app();
        let pr = fixture::demo_pr();
        let body = "Agree, `30s` is **fine**";
        with_ready(&mut app, |r, _| {
            let thread = {
                let t = mona_thread(r);
                ThreadRef {
                    id: t.id.clone(),
                    author: "mona".into(),
                    path: Some(t.path.clone()),
                    line: t.line,
                }
            };
            r.view
                .review
                .draft
                .add(DraftKind::Reply, None, Some(thread), body, NOW)
                .unwrap();
        });
        testing::settle(&mut app);
        let fine = weights(&mut app, "fine");
        assert!(
            !fine.is_empty() && fine.iter().all(|w| *w == 600),
            "a bold word in the draft and in its card"
        );
        assert_eq!(testing::count::<CopyMarkdown>(&mut app), 7);
        let copy = testing::find::<CopyMarkdown>(&mut app, |c| c.0 == body);
        testing::activate(&mut app, copy);
        assert_eq!(app.world().resource::<CopiedText>().0, [body]);
        assert!(testing::tab(&app, &pr).ui.editor.is_none());
    }

    #[test]
    fn preview_and_thread_render_the_same() {
        let source = "**Holding the lock** across `exchange` 🐢\n\n- one\n- two\n\n```rust\nlet a = 1;\n```\n\nsee [the race](https://github.com/rzorzal/clusia) ![x](https://github.com/user-attachments/assets/2.png)";
        let mut app = comments_app();
        set_first_body(&mut app, source);
        let (fonts, opts) = {
            let model = app.world().resource::<Model>();
            let theme = app.world().resource::<Theme>();
            (
                app.world().resource::<UiFonts>().clone(),
                RenderOpts::from_config(theme.code_size, &model.snapshot.config),
            )
        };
        let stand_in = app.world_mut().spawn(Node::default()).id();
        app.world_mut()
            .run_system_once(move |mut commands: Commands| {
                commands.entity(stand_in).with_children(|p| {
                    markdown(p, &fonts, &parse(source), &opts);
                });
            })
            .unwrap();
        app.update();
        let preview = testing::tree_signature(&app, stand_in);
        assert!(preview.contains("image:https://github.com/user-attachments/assets/2.png"));
        let mut bodies = app.world_mut().query::<(Entity, &MdBody, &ChildOf)>();
        let signatures: Vec<String> = bodies
            .iter(app.world())
            .filter(|(_, _, parent)| parent.parent() != stand_in)
            .map(|(e, ..)| testing::tree_signature(&app, e))
            .collect();
        let bare = preview
            .strip_prefix("N(")
            .and_then(|s| s.strip_suffix(",)"))
            .expect("the stand-in holds one body");
        assert!(
            signatures.iter().any(|s| s == bare),
            "the thread draws what the preview draws:\n{bare}\nnot among\n{signatures:#?}"
        );
    }

    fn hidden_ui(keys: &[(&str, u32)]) -> TabUi {
        let mut ui = TabUi::default();
        for (key, posts) in keys {
            ui.hidden.hide(key, *posts);
        }
        ui
    }

    #[test]
    fn hidden_comments_collapse_and_are_counted() {
        let r = ready();
        let ui = hidden_ui(&[("comment:2002", 1), ("thread:PRRT_demo_refresh_41", 3)]);
        let v = comments_view(&r, &ui, NOW);
        let states: Vec<(&str, HiddenState)> = v
            .general
            .iter()
            .map(|p| (p.author.as_str(), p.hidden))
            .collect();
        assert_eq!(
            states,
            [
                ("joao", HiddenState::Visible),
                ("octo", HiddenState::Hidden),
                ("hubot", HiddenState::Visible)
            ]
        );
        assert_eq!(v.threads[0].hidden, HiddenState::Hidden);
        assert_eq!(v.collapsed, ["comment:2002", "thread:PRRT_demo_refresh_41"]);
    }

    #[test]
    fn a_hidden_comment_that_is_edited_stays_hidden() {
        let mut r = ready();
        let ui = hidden_ui(&[("comment:2002", 1)]);
        r.view.conversation.as_mut().unwrap().comments[1].body = "Edited on GitHub.".into();
        let v = comments_view(&r, &ui, NOW);
        assert_eq!(v.general[1].hidden, HiddenState::Hidden);
        assert_eq!(v.general[1].body, "Edited on GitHub.");
    }

    #[test]
    fn a_new_reply_shows_a_hidden_thread_again_with_a_mark() {
        let r = ready();
        let ui = hidden_ui(&[("thread:PRRT_demo_refresh_41", 2)]);
        let v = comments_view(&r, &ui, NOW);
        assert_eq!(v.threads[0].hidden, HiddenState::NewReply);
        assert!(v.collapsed.is_empty(), "it is not collapsed any more");
    }

    #[test]
    fn hiding_collapses_at_once_and_asks_the_daemon_to_keep_it() {
        let mut app = comments_app();
        let pr = fixture::demo_pr();
        let hide = testing::find::<HideButton>(&mut app, |h| h.key == "comment:2002");
        testing::activate(&mut app, hide);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::SetCommentHidden {
                pr: pr.clone(),
                key: "comment:2002".into(),
                posts: Some(1)
            }]
        );
        testing::settle(&mut app);
        assert!(has_text(&mut app, "Hidden · @octo"));
        assert!(has_text(&mut app, "1 hidden"));
        let show = testing::find::<ShowButton>(&mut app, |s| s.key == "comment:2002");
        testing::activate(&mut app, show);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::SetCommentHidden {
                pr,
                key: "comment:2002".into(),
                posts: None
            }]
        );
        testing::settle(&mut app);
        assert!(!has_text(&mut app, "Hidden · @octo"));
        assert!(!has_text(&mut app, "1 hidden"));
    }

    #[test]
    fn hiding_a_thread_remembers_how_many_posts_it_had() {
        let mut app = comments_app();
        let hide = testing::find::<HideButton>(&mut app, |h| h.key.starts_with("thread:"));
        testing::activate(&mut app, hide);
        assert!(matches!(
            testing::recorded(&mut app).as_slice(),
            [Ask::SetCommentHidden { key, posts: Some(3), .. }] if key == "thread:PRRT_demo_refresh_41"
        ));
    }

    #[test]
    fn show_all_brings_every_collapsed_comment_back() {
        let mut app = comments_app();
        with_ready(&mut app, |_, ui| {
            ui.hidden.hide("comment:2001", 1);
            ui.hidden.hide("comment:2002", 1);
        });
        testing::settle(&mut app);
        assert!(has_text(&mut app, "2 hidden"));
        let all = testing::find::<ShowAll>(&mut app, |_| true);
        testing::activate(&mut app, all);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::ShowComments {
                pr: fixture::demo_pr(),
                keys: vec!["comment:2001".into(), "comment:2002".into()]
            }],
            "one ask, so one answer and no flicker"
        );
        testing::settle(&mut app);
        assert!(!has_text(&mut app, "hidden ·"));
    }

    #[test]
    fn the_daemons_set_replaces_the_tabs_and_is_asked_again_after_a_reconnect() {
        let mut app = comments_app();
        let pr = fixture::demo_pr();
        assert!(
            testing::tab(&app, &pr).ui.hidden_asked,
            "asked while opening"
        );
        testing::settle(&mut app);
        assert!(!testing::recorded(&mut app).contains(&Ask::GetHiddenComments(pr.clone())));
        testing::tell(&mut app, crate::bridge::Tell::Lost("gone".into()));
        assert!(!testing::tab(&app, &pr).ui.hidden_asked);
        testing::recorded(&mut app);
        app.world_mut().resource_mut::<Model>().connection = crate::bridge::Connection::Live;
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).contains(&Ask::GetHiddenComments(pr.clone())));
        let mut set = clusia_core::HiddenComments::default();
        set.hide("comment:2001", 1);
        testing::tell(
            &mut app,
            crate::bridge::Tell::Hidden {
                pr: pr.clone(),
                set: set.clone(),
            },
        );
        testing::settle(&mut app);
        assert_eq!(testing::tab(&app, &pr).ui.hidden, set);
        assert!(has_text(&mut app, "Hidden · @joao"));
    }

    #[test]
    fn the_cached_copy_does_not_hide() {
        let mut app = comments_app();
        with_ready(&mut app, |r, _| r.cached_at = Some(NOW - 3600));
        testing::settle(&mut app);
        let hide = testing::find::<HideButton>(&mut app, |h| h.key == "comment:2002");
        testing::activate(&mut app, hide);
        assert!(testing::recorded(&mut app).is_empty());
        assert!(!has_text(&mut app, "Hidden · @octo"));
    }

    #[test]
    fn a_hide_without_a_daemon_says_so_and_changes_nothing() {
        let mut app = comments_app();
        let pr = fixture::demo_pr();
        app.world_mut().resource_mut::<Model>().connection =
            crate::bridge::Connection::Lost("gone".into());
        testing::settle(&mut app);
        testing::recorded(&mut app);
        let hide = testing::find::<HideButton>(&mut app, |h| h.key == "comment:2002");
        testing::activate(&mut app, hide);
        assert!(testing::recorded(&mut app).is_empty());
        assert!(testing::tab(&app, &pr).ui.hidden.is_empty());
        let toasts = &app.world().resource::<crate::bridge::Toasts>().0;
        assert!(
            toasts
                .iter()
                .any(|t| t.warning && t.text.contains("Not connected"))
        );
    }

    #[test]
    fn the_cached_copy_loads_the_hidden_set() {
        let mut app = comments_app();
        let pr = fixture::demo_pr();
        with_ready(&mut app, |r, ui| {
            r.cached_at = Some(NOW - 3600);
            ui.hidden_asked = false;
        });
        testing::settle(&mut app);
        assert!(testing::recorded(&mut app).contains(&Ask::GetHiddenComments(pr.clone())));
        let mut set = clusia_core::HiddenComments::default();
        set.hide("comment:2002", 1);
        testing::tell(&mut app, crate::bridge::Tell::Hidden { pr, set });
        testing::settle(&mut app);
        assert!(has_text(&mut app, "Hidden · @octo"));
    }
}
