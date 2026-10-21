//! Finalize (spec §6.4, mockup `Finalize.png`): every draft item, the summary and the verdict,
//! published to GitHub as one review. A failure keeps the modal open with everything in it.

use std::collections::HashMap;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input::ButtonInput;
use bevy::input_focus::FocusLost;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::{EditableText, RemSize};
use bevy::ui::{ComputedNode, UiGlobalTransform};
use bevy::ui_widgets::{Activate, Button as WidgetButton, ScrollArea, observe};
use clusia_core::revalidation::{blocking_obsolete, pending};
use clusia_core::{DraftKind, ItemStatus, PrRef, Verdict};
use clusia_protocol::{ErrorCode, WindowTarget};

use crate::bridge::{Ask, Asks, Connection, Model, TOAST_SECS, Toast, Toasts};
use crate::fonts::UiFonts;
use crate::nav::Nav;
use crate::review_state::{FinalizeForm, Modal, Phase, Ready, ReviewEvent, ReviewTabs, Tickets};
use crate::screens::review::ReviewSystems;
use crate::screens::review::editor::{not_sent, read_only_reason};
use crate::screens::review::leave::close_tab;
use crate::screens::review::revalidation::{Revalidations, pending_of};
use crate::screens::review::shell::ModalFor;
use crate::theme::Swatch;
use crate::ui::composer::{
    AreaSize, ComposerKey, ComposerMode, ComposerView, ExtraModes, Slot, composer, mode_of,
};
use crate::ui::kit::{
    Clickable, Fill, HoverFill, Stroke, Tone, Type, Variant, badge, button, card, disabled_button,
    panel, text, wrapped_text,
};
use crate::ui::modal::{BackdropClicked, escape_pressed, modal_card, modal_root};

#[derive(Debug, Clone, PartialEq)]
pub struct FinalizeItem {
    pub id: String,
    /// "refresh.rs:44", "General note", "Reply to @mona · refresh.rs:41", "Resolve thread by @mona".
    pub label: String,
    /// The full `path:line` of a cut label, for the hover title.
    pub title: Option<String>,
    pub body: String,
    /// Resolves have no text.
    pub editable: bool,
    /// Obsolete and still a problem: it blocks Publish.
    pub obsolete: bool,
    /// Obsolete, but removed in the revalidation: kept in the draft, shown as settled.
    pub removed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VerdictCard {
    pub verdict: Verdict,
    pub title: &'static str,
    pub detail: &'static str,
    pub on: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FinalizeView {
    pub title: &'static str,
    /// "rzorzal/clusia #123 · feat: auth refresh".
    pub subject: String,
    pub note: &'static str,
    pub items: Vec<FinalizeItem>,
    pub warning: Option<String>,
    /// Suggestions of the revalidation that nobody decided: Publish sends the draft as it is.
    pub suggestions: usize,
    pub verdicts: Vec<VerdictCard>,
    /// Why Publish is disabled (`None`: it is enabled).
    pub blocked: Option<&'static str>,
    pub error: Option<String>,
    pub busy: bool,
    /// The cached copy: nothing is sent from it, so Discard is off too.
    pub cached: bool,
}

/// `2 suggestions not reviewed: your draft is published as it is.`
pub fn suggestions_note(waiting: usize) -> String {
    let what = if waiting == 1 {
        "1 suggestion".to_string()
    } else {
        format!("{waiting} suggestions")
    };
    format!("{what} not reviewed: your draft is published as it is.")
}

pub fn verdict_text(v: Verdict) -> (&'static str, &'static str) {
    match v {
        Verdict::Comment => ("Comment", "Feedback without a verdict"),
        Verdict::Approve => ("Approve", "Ready to merge"),
        Verdict::RequestChanges => ("Request changes", "Needs another pass"),
        Verdict::ClosePr => ("Close PR", "Comment, then close it"),
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The width a place label gets, in characters: the label column holds this many at the largest
/// text size.
const PLACE_CHARS: usize = 17;

/// The label column's width in pixels at the medium text size: room for `PLACE_CHARS` monospace
/// characters. `size_place_labels` scales it with the text.
const PLACE_WIDTH: f32 = 144.0;

/// The label column of an item row.
#[derive(Component)]
struct PlaceLabel;

fn size_place_labels(
    rem: Option<Res<RemSize>>,
    added: Query<(), Added<PlaceLabel>>,
    mut labels: Query<&mut Node, With<PlaceLabel>>,
) {
    let changed = rem.as_ref().is_some_and(|r| r.is_changed());
    if !changed && added.is_empty() {
        return;
    }
    let scale = rem.map_or(1.0, |r| r.0);
    for mut node in &mut labels {
        node.width = px(PLACE_WIDTH * scale);
    }
}

/// The popup that names the whole path of a cut label while the pointer is on it.
#[derive(Component)]
struct PlaceTitle;

/// Shows the popup of a hovered label under it, or over it for a row in the lower half of the
/// body, where the body's edge would cut it off.
fn show_place_titles(
    labels: Query<(&Hovered, &Children, &ComputedNode, &UiGlobalTransform), Changed<Hovered>>,
    body: Query<(&ComputedNode, &UiGlobalTransform), With<FinalizeBody>>,
    mut titles: Query<&mut Node, With<PlaceTitle>>,
) {
    let middle = body.iter().next().map(|(_, at)| at.translation.y);
    for (hovered, children, _, at) in &labels {
        let low = middle.is_some_and(|m| at.translation.y > m);
        for child in children {
            if let Ok(mut node) = titles.get_mut(*child) {
                node.display = if hovered.get() {
                    Display::Flex
                } else {
                    Display::None
                };
                (node.top, node.bottom) = if low {
                    (Val::Auto, percent(100))
                } else {
                    (percent(100), Val::Auto)
                };
            }
        }
    }
}

fn place(path: Option<&str>, line: Option<u32>) -> String {
    match (path, line) {
        (Some(p), Some(l)) => fit_place(p, Some(l), PLACE_CHARS),
        (Some(p), None) => fit_place(p, None, PLACE_CHARS),
        _ => String::new(),
    }
}

/// The whole `path:line`, for the hover title of a cut label.
fn full_place(path: Option<&str>, line: Option<u32>) -> Option<String> {
    match (path, line) {
        (Some(p), Some(l)) => Some(format!("{p}:{l}")),
        (Some(p), None) => Some(p.to_string()),
        _ => None,
    }
}

pub fn finalize_view(ready: &Ready, form: &FinalizeForm) -> FinalizeView {
    let view = &ready.view;
    let items: Vec<FinalizeItem> = view
        .review
        .draft
        .items
        .iter()
        .filter(|i| i.accepted)
        .map(|i| {
            let thread = i.thread.as_ref();
            let title = match i.kind {
                DraftKind::LineComment => i
                    .anchor
                    .as_ref()
                    .and_then(|a| full_place(Some(&a.path), Some(a.line))),
                DraftKind::Reply => thread.and_then(|t| full_place(t.path.as_deref(), t.line)),
                _ => None,
            };
            let label = match i.kind {
                DraftKind::LineComment => i
                    .anchor
                    .as_ref()
                    .map(|a| place(Some(&a.path), Some(a.line)))
                    .unwrap_or_default(),
                DraftKind::General => "General note".to_string(),
                DraftKind::Reply => {
                    let author = thread.map_or("", |t| t.author.as_str());
                    let at = thread
                        .map(|t| place(t.path.as_deref(), t.line))
                        .unwrap_or_default();
                    if at.is_empty() {
                        format!("Reply to @{author}")
                    } else {
                        format!("Reply to @{author} · {at}")
                    }
                }
                DraftKind::Resolve => format!(
                    "Resolve thread by @{}",
                    thread.map_or("", |t| t.author.as_str())
                ),
            };
            FinalizeItem {
                id: i.id.clone(),
                label,
                title,
                body: i.body.clone(),
                editable: i.kind != DraftKind::Resolve,
                obsolete: matches!(i.status, ItemStatus::Obsolete { .. })
                    && !view.review.removed.contains(&i.id),
                removed: view.review.removed.contains(&i.id),
            }
        })
        .collect();
    // An obsolete comment the reviewer already removed in the revalidation is left out of the
    // publish and blocks nothing.
    let obsolete = blocking_obsolete(&view.review);
    let warning = match obsolete {
        0 => None,
        1 => Some(
            "1 comment no longer fits the diff. Remove it or re-add it on a current line before publishing."
                .to_string(),
        ),
        n => Some(format!(
            "{n} comments no longer fit the diff. Remove them or re-add them on a current line before publishing."
        )),
    };
    let allowed = Verdict::allowed_for(view.role);
    let verdicts = [
        Verdict::Comment,
        Verdict::Approve,
        Verdict::RequestChanges,
        Verdict::ClosePr,
    ]
    .into_iter()
    .filter(|v| allowed.contains(v))
    .map(|v| {
        let (title, detail) = verdict_text(v);
        VerdictCard {
            verdict: v,
            title,
            detail,
            on: form.verdict == Some(v),
        }
    })
    .collect();
    let blocked = if form.busy {
        Some("Publishing…")
    } else if view.pr.merged {
        Some("This pull request is merged — publishing is off")
    } else if view.pr.closed {
        Some("This pull request is closed — publishing is off")
    } else if ready.cached_at.is_some() {
        Some("This is the cached copy — reconnect to publish")
    } else if obsolete > 0 {
        Some("Fix the comments that no longer fit the diff first")
    } else if form.verdict.is_none_or(|v| !v.is_allowed_for(view.role)) {
        Some("Choose a verdict")
    } else {
        None
    };
    FinalizeView {
        title: "Finalize review",
        subject: format!(
            "{} #{} · {}",
            view.review.pr.slug(),
            view.review.pr.number,
            view.pr.summary.title
        ),
        note: "Everything below goes to GitHub as one review. You can still edit any comment.",
        items,
        warning,
        suggestions: view
            .review
            .revalidation
            .as_ref()
            .map_or(0, |r| pending(r, &view.review.draft)),
        verdicts,
        blocked,
        error: form.error.clone(),
        busy: form.busy,
        cached: ready.cached_at.is_some(),
    }
}
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FinalizeModal(pub PrRef);

/// The part of the dialog between the title block and the footer that scrolls.
#[derive(Component)]
pub struct FinalizeBody;

/// The verdict-and-publish row at the dialog's foot; it never scrolls away.
#[derive(Component)]
pub struct FinalizeFooter;

/// An item's text area; `index` is its position in the list.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FinalizeItemArea {
    pub pr: PrRef,
    pub id: String,
    pub index: usize,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct SummaryArea(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct VerdictButton {
    pub pr: PrRef,
    pub verdict: Verdict,
}

/// **Remove** on an item (an obsolete one blocks Publish until it goes).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FinalizeRemove {
    pub pr: PrRef,
    pub id: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct DiscardButton(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct SaveButton(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct PublishButton(pub PrRef);

/// Item texts typed but not saved yet, so a rebuild (a verdict click) keeps them.
#[derive(Resource, Debug, Default)]
pub struct FinalizeEdits(pub HashMap<(PrRef, String), String>);

#[derive(Component)]
struct FinalizePart {
    pr: PrRef,
    built: Option<FinalizeView>,
}

pub struct FinalizePlugin;

impl Plugin for FinalizePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FinalizeEdits>()
            .add_observer(save_on_blur)
            .add_systems(
                Update,
                (
                    outcomes,
                    spawn_modal,
                    rebuild_modal,
                    mirror_texts,
                    escape_closes,
                    size_place_labels,
                    show_place_titles,
                )
                    .chain()
                    .after(ReviewSystems),
            );
    }
}

fn spawn_modal(
    mut commands: Commands,
    nav: Res<Nav>,
    tabs: Res<ReviewTabs>,
    modals: Query<&ModalFor>,
) {
    let crate::nav::Screen::Review(pr) = &nav.screen else {
        return;
    };
    let Some(tab) = tabs.0.get(pr) else { return };
    if tab.ui.modal != Some(Modal::Finalize) || !matches!(tab.phase, Phase::Ready(_)) {
        return;
    }
    if modals
        .iter()
        .any(|m| &m.pr == pr && m.modal == Modal::Finalize)
    {
        return;
    }
    commands
        .spawn((
            modal_root(),
            ModalFor {
                pr: pr.clone(),
                modal: Modal::Finalize,
            },
            FinalizeModal(pr.clone()),
        ))
        .with_children(|root| {
            root.spawn((
                modal_card(680.0),
                FinalizePart {
                    pr: pr.clone(),
                    built: None,
                },
            ));
        });
}

fn rebuild_modal(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    edits: Res<FinalizeEdits>,
    extra: Res<ExtraModes>,
    revalidations: Res<Revalidations>,
    fonts: Res<UiFonts>,
    mut parts: Query<(Entity, &mut FinalizePart)>,
) {
    for (entity, mut part) in &mut parts {
        if part.built.is_some()
            && !tabs.is_changed()
            && !model.is_changed()
            && !revalidations.is_changed()
        {
            continue;
        }
        let Some(tab) = tabs.0.get(&part.pr) else {
            continue;
        };
        let Phase::Ready(ready) = &tab.phase else {
            continue;
        };
        let mut v = finalize_view(ready, &tab.ui.finalize);
        // The round the window holds may be newer than the one in the review file.
        v.suggestions = pending_of(tab, revalidations.0.get(&part.pr));
        if v.blocked.is_none() && model.connection != Connection::Live {
            v.blocked = Some("Not connected to clusiad");
        }
        if part.built.as_ref() == Some(&v) {
            continue;
        }
        let pr = part.pr.clone();
        let summary = tab.ui.finalize.summary.clone();
        let fonts = &*fonts;
        commands.entity(entity).despawn_related::<Children>();
        commands.entity(entity).with_children(|p| {
            let mode = |key: &ComposerKey| mode_of(key, &tabs, &extra);
            card_content(p, fonts, &pr, &v, &summary, &edits, &mode);
        });
        part.built = Some(v);
    }
}

fn card_content(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    v: &FinalizeView,
    summary: &str,
    edits: &FinalizeEdits,
    mode: &dyn Fn(&ComposerKey) -> ComposerMode,
) {
    p.spawn((
        Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(12),
            padding: UiRect::axes(px(24), px(20)),
            // The footer takes its height first; the body gives way and scrolls.
            flex_shrink: 1.0,
            min_height: px(0),
            overflow: Overflow::scroll_y(),
            ..default()
        },
        ScrollArea,
        ScrollPosition::default(),
        FinalizeBody,
    ))
    .with_children(|c| {
        c.spawn(Node {
            column_gap: px(10),
            row_gap: px(2),
            flex_wrap: FlexWrap::Wrap,
            align_items: AlignItems::Baseline,
            ..default()
        })
        .with_children(|h| {
            h.spawn(text(fonts, v.title, Type::HEADING.size(18.0)));
            h.spawn((
                wrapped_text(fonts, v.subject.clone(), Type::MUTED),
                Node {
                    max_width: percent(100),
                    ..default()
                },
            ));
        });
        c.spawn(text(fonts, v.note, Type::MUTED));
        for (index, item) in v.items.iter().enumerate() {
            c.spawn((
                panel(
                    Node {
                        column_gap: px(16),
                        align_items: AlignItems::FlexStart,
                        padding: UiRect::axes(px(12), px(10)),
                        border: px(1).all(),
                        border_radius: BorderRadius::all(px(6)),
                        ..default()
                    },
                    Swatch::Chrome,
                ),
                BorderColor::default(),
                Stroke(if item.obsolete {
                    Swatch::Orange
                } else {
                    Swatch::Line
                }),
            ))
            .with_children(|row| {
                let mut label = row.spawn((
                    Node {
                        width: px(PLACE_WIDTH),
                        flex_shrink: 0.0,
                        ..default()
                    },
                    PlaceLabel,
                    // Above the editor beside it, which the full path would otherwise sit under.
                    ZIndex(1),
                    children![text(fonts, item.label.clone(), Type::MONO.ink(Swatch::Fg))],
                ));
                if let Some(title) = &item.title {
                    label.insert(Hovered::default());
                    label.with_children(|l| {
                        l.spawn((
                            PlaceTitle,
                            panel(
                                Node {
                                    display: Display::None,
                                    position_type: PositionType::Absolute,
                                    top: percent(100),
                                    left: px(0),
                                    padding: UiRect::axes(px(8), px(4)),
                                    border: px(1).all(),
                                    border_radius: BorderRadius::all(px(4)),
                                    ..default()
                                },
                                Swatch::Surface,
                            ),
                            BorderColor::default(),
                            Stroke(Swatch::Line),
                            children![text(fonts, title.clone(), Type::MONO.ink(Swatch::Fg))],
                        ));
                    });
                }
                if item.editable {
                    let value = edits
                        .0
                        .get(&(pr.clone(), item.id.clone()))
                        .unwrap_or(&item.body);
                    row.spawn(card(Node {
                        flex_grow: 1.0,
                        min_width: px(0),
                        ..default()
                    }))
                    .with_children(|frame| {
                        let key = ComposerKey(pr.clone(), Slot::FinalizeItem(item.id.clone()));
                        composer(
                            frame,
                            fonts,
                            &ComposerView {
                                text: value,
                                mode: mode(&key),
                                id: index as u64 + 1,
                                size: AreaSize::Grow { min: 1.0, max: 6.0 },
                                compact: true,
                                suggest: false,
                                flat: true,
                            },
                            key,
                            FinalizeItemArea {
                                pr: pr.clone(),
                                id: item.id.clone(),
                                index,
                            },
                        );
                    });
                } else {
                    row.spawn(text(fonts, "Marked to resolve on publish", Type::MUTED));
                }
                if item.obsolete {
                    row.spawn(badge(fonts, "obsolete", Tone::Orange));
                } else if item.removed {
                    row.spawn(text(fonts, "Removed in the revalidation", Type::MUTED));
                }
                if !v.cached {
                    row.spawn((
                        button(fonts, "Remove", Variant::Ghost),
                        FinalizeRemove {
                            pr: pr.clone(),
                            id: item.id.clone(),
                        },
                        observe(on_remove),
                    ));
                }
            });
        }
        if let Some(warning) = &v.warning {
            c.spawn(panel(
                Node {
                    padding: UiRect::axes(px(12), px(8)),
                    border_radius: BorderRadius::all(px(6)),
                    ..default()
                },
                Swatch::OrangeSoft,
            ))
            .with_children(|w| {
                w.spawn(text(
                    fonts,
                    format!("⚠ {warning}"),
                    Type::BODY.ink(Swatch::Orange),
                ));
            });
        }
        if v.suggestions > 0 {
            c.spawn(panel(
                Node {
                    padding: UiRect::axes(px(12), px(8)),
                    border_radius: BorderRadius::all(px(6)),
                    ..default()
                },
                Swatch::Chrome,
            ))
            .with_children(|w| {
                w.spawn(text(fonts, suggestions_note(v.suggestions), Type::BODY));
            });
        }
        c.spawn(text(fonts, "Summary", Type::STRONG));
        c.spawn(card(Node::default())).with_children(|frame| {
            let key = ComposerKey(pr.clone(), Slot::FinalizeSummary);
            composer(
                frame,
                fonts,
                &ComposerView {
                    text: summary,
                    mode: mode(&key),
                    id: 0,
                    size: AreaSize::Grow { min: 3.0, max: 6.0 },
                    compact: false,
                    suggest: false,
                    flat: true,
                },
                key,
                SummaryArea(pr.clone()),
            );
        });
        c.spawn(text(fonts, "Verdict", Type::STRONG));
        c.spawn(Node {
            column_gap: px(8),
            ..default()
        })
        .with_children(|row| {
            for card in &v.verdicts {
                verdict_card(row, fonts, pr, card);
            }
        });
        if let Some(error) = &v.error {
            c.spawn(text(fonts, error.clone(), Type::BODY.ink(Swatch::Orange)));
        }
    });
    p.spawn((
        panel(
            Node {
                padding: UiRect::axes(px(24), px(14)),
                column_gap: px(10),
                align_items: AlignItems::Center,
                flex_shrink: 0.0,
                border: UiRect::top(px(1)),
                ..default()
            },
            Swatch::Chrome,
        ),
        BorderColor::default(),
        Stroke(Swatch::Line),
        FinalizeFooter,
    ))
    .with_children(|f| {
        if v.cached {
            f.spawn(disabled_button(fonts, "Discard review"));
        } else {
            f.spawn((
                button(fonts, "Discard review", Variant::Danger),
                DiscardButton(pr.clone()),
                observe(on_discard),
            ));
        }
        f.spawn(Node {
            flex_grow: 1.0,
            ..default()
        });
        f.spawn((
            button(fonts, "Save for later", Variant::Secondary),
            SaveButton(pr.clone()),
            observe(on_save),
        ));
        let label = if v.busy {
            "Publishing…"
        } else {
            "Publish to GitHub"
        };
        match v.blocked {
            None => {
                f.spawn((
                    button(fonts, label, Variant::Primary),
                    PublishButton(pr.clone()),
                    observe(on_publish),
                ));
            }
            Some(reason) => {
                if !v.busy {
                    f.spawn(text(fonts, reason, Type::META));
                }
                f.spawn(disabled_button(fonts, label));
            }
        }
    });
}

fn verdict_card(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, card: &VerdictCard) {
    let warm = matches!(card.verdict, Verdict::RequestChanges | Verdict::ClosePr);
    let (fill, stroke, ink) = match (card.on, warm) {
        (true, true) => (Swatch::OrangeSoft, Swatch::Orange, Swatch::Orange),
        (true, false) => (Swatch::GreenSoft, Swatch::Green, Swatch::Green),
        (false, _) => (Swatch::Surface, Swatch::Line, Swatch::Fg),
    };
    p.spawn((
        Node {
            flex_grow: 1.0,
            flex_basis: px(0),
            column_gap: px(10),
            align_items: AlignItems::FlexStart,
            padding: UiRect::axes(px(12), px(10)),
            border: px(1).all(),
            border_radius: BorderRadius::all(px(8)),
            ..default()
        },
        WidgetButton,
        Hovered::default(),
        TabIndex(0),
        Clickable,
        BackgroundColor::default(),
        Fill(fill),
        HoverFill(if card.on { fill } else { Swatch::Hover }),
        BorderColor::default(),
        Stroke(stroke),
        VerdictButton {
            pr: pr.clone(),
            verdict: card.verdict,
        },
        observe(on_verdict),
    ))
    .with_children(|c| {
        c.spawn((
            Node {
                width: px(14),
                height: px(14),
                margin: UiRect::top(px(2)),
                border: px(1).all(),
                border_radius: BorderRadius::MAX,
                padding: px(3).all(),
                flex_shrink: 0.0,
                ..default()
            },
            BorderColor::default(),
            Stroke(if card.on { ink } else { Swatch::Faint }),
            children![panel(
                Node {
                    width: percent(100),
                    height: percent(100),
                    border_radius: BorderRadius::MAX,
                    ..default()
                },
                if card.on { ink } else { Swatch::Clear },
            )],
        ));
        c.spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(2),
            ..default()
        })
        .with_children(|t| {
            t.spawn(text(fonts, card.title, Type::STRONG.ink(ink)));
            t.spawn(text(fonts, card.detail, Type::META));
        });
    });
}

/// Keeps the summary in the form and typed item texts in `FinalizeEdits`. Neither is part of
/// the view, so typing never rebuilds the modal.
fn mirror_texts(
    summaries: Query<(&EditableText, &SummaryArea), Changed<EditableText>>,
    items: Query<(&EditableText, &FinalizeItemArea), Changed<EditableText>>,
    mut tabs: ResMut<ReviewTabs>,
    mut edits: ResMut<FinalizeEdits>,
) {
    for (editable, SummaryArea(pr)) in &summaries {
        let value = editable.value().to_string();
        if let Some(tab) = tabs.0.get_mut(pr)
            && tab.ui.finalize.summary != value
        {
            tab.ui.finalize.summary = value;
        }
    }
    for (editable, area) in &items {
        edits.0.insert(
            (area.pr.clone(), area.id.clone()),
            editable.value().to_string(),
        );
    }
}

fn item_body<'a>(tabs: &'a ReviewTabs, pr: &PrRef, id: &str) -> Option<&'a str> {
    match &tabs.0.get(pr)?.phase {
        Phase::Ready(r) => r.view.review.draft.get(id).map(|i| i.body.as_str()),
        _ => None,
    }
}

/// `UpdateItem` for an item area whose text differs from the saved body.
fn save_item(
    area: &FinalizeItemArea,
    value: &str,
    tabs: &ReviewTabs,
    tickets: &mut Tickets,
    asks: &mut Asks,
) {
    // Nothing is sent from the cached copy.
    let cached = tabs
        .0
        .get(&area.pr)
        .and_then(|t| t.ready())
        .is_some_and(|r| r.cached_at.is_some());
    if !cached && item_body(tabs, &area.pr, &area.id).is_some_and(|body| body != value.trim()) {
        asks.send(Ask::UpdateItem {
            pr: area.pr.clone(),
            id: area.id.clone(),
            body: value.trim().to_string(),
            ticket: tickets.issue(),
        });
    }
}

fn save_on_blur(
    lost: On<FocusLost>,
    areas: Query<(&EditableText, &FinalizeItemArea)>,
    tabs: Res<ReviewTabs>,
    mut tickets: ResMut<Tickets>,
    mut asks: ResMut<Asks>,
) {
    if let Ok((editable, area)) = areas.get(lost.entity) {
        save_item(
            area,
            &editable.value().to_string(),
            &tabs,
            &mut tickets,
            &mut asks,
        );
    }
}

fn on_verdict(
    activate: On<Activate>,
    buttons: Query<&VerdictButton>,
    mut tabs: ResMut<ReviewTabs>,
) {
    if let Ok(b) = buttons.get(activate.entity)
        && let Some(tab) = tabs.0.get_mut(&b.pr)
        && !tab.ui.finalize.busy
    {
        tab.ui.finalize.verdict = Some(b.verdict);
        tab.ui.finalize.error = None;
    }
}

fn on_publish(
    activate: On<Activate>,
    buttons: Query<&PublishButton>,
    areas: Query<(&EditableText, &FinalizeItemArea)>,
    summaries: Query<(&EditableText, &SummaryArea)>,
    mut tabs: ResMut<ReviewTabs>,
    mut tickets: ResMut<Tickets>,
    mut asks: ResMut<Asks>,
) {
    let Ok(PublishButton(pr)) = buttons.get(activate.entity) else {
        return;
    };
    // Unsaved edits go first: the main connection handles them before the worker publishes.
    for (editable, area) in areas.iter().filter(|(_, a)| &a.pr == pr) {
        save_item(
            area,
            &editable.value().to_string(),
            &tabs,
            &mut tickets,
            &mut asks,
        );
    }
    let summary = summaries
        .iter()
        .find(|(_, s)| &s.0 == pr)
        .map(|(e, _)| e.value().to_string());
    let Some(tab) = tabs.0.get_mut(pr) else {
        return;
    };
    let form = &mut tab.ui.finalize;
    if let Some(summary) = summary {
        form.summary = summary;
    }
    let Some(verdict) = form.verdict else {
        return;
    };
    if form.busy {
        return;
    }
    form.busy = true;
    form.error = None;
    asks.send(Ask::Publish {
        pr: pr.clone(),
        verdict,
        summary: form.summary.trim().to_string(),
    });
}

fn on_save(
    activate: On<Activate>,
    buttons: Query<&SaveButton>,
    areas: Query<(&EditableText, &FinalizeItemArea)>,
    mut tabs: ResMut<ReviewTabs>,
    mut nav: ResMut<Nav>,
    mut tickets: ResMut<Tickets>,
    mut asks: ResMut<Asks>,
    mut edits: ResMut<FinalizeEdits>,
    mut modes: ResMut<ExtraModes>,
) {
    let Ok(SaveButton(pr)) = buttons.get(activate.entity) else {
        return;
    };
    for (editable, area) in areas.iter().filter(|(_, a)| &a.pr == pr) {
        save_item(
            area,
            &editable.value().to_string(),
            &tabs,
            &mut tickets,
            &mut asks,
        );
    }
    asks.send(Ask::CloseReview(pr.clone()));
    edits.0.retain(|(p, _), _| p != pr);
    close_tab(pr, &mut tabs, &mut nav, &mut modes);
}

fn on_remove(
    activate: On<Activate>,
    buttons: Query<&FinalizeRemove>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
    mut edits: ResMut<FinalizeEdits>,
) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &model, &b.pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    edits.0.remove(&(b.pr.clone(), b.id.clone()));
    asks.send(Ask::RemoveItem {
        pr: b.pr.clone(),
        id: b.id.clone(),
    });
}

fn on_discard(
    activate: On<Activate>,
    buttons: Query<&DiscardButton>,
    mut tabs: ResMut<ReviewTabs>,
    mut nav: ResMut<Nav>,
    mut asks: ResMut<Asks>,
    mut edits: ResMut<FinalizeEdits>,
    mut modes: ResMut<ExtraModes>,
) {
    let Ok(DiscardButton(pr)) = buttons.get(activate.entity) else {
        return;
    };
    asks.send(Ask::Discard(pr.clone()));
    edits.0.retain(|(p, _), _| p != pr);
    close_tab(pr, &mut tabs, &mut nav, &mut modes);
}

fn escape_closes(
    keys: Res<ButtonInput<KeyCode>>,
    mut clicks: MessageReader<BackdropClicked>,
    roots: Query<&ModalFor>,
    nav: Res<Nav>,
    mut tabs: ResMut<ReviewTabs>,
) {
    let outside = clicks
        .read()
        .any(|c| roots.get(c.0).is_ok_and(|m| m.modal == Modal::Finalize));
    if !(escape_pressed(&keys) || outside) {
        return;
    }
    let crate::nav::Screen::Review(pr) = &nav.screen else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(pr)
        && tab.ui.modal == Some(Modal::Finalize)
        && !tab.ui.finalize.busy
    {
        tab.ui.modal = None;
    }
}

fn toast(toasts: &mut Toasts, time: &Time, text: String, warning: bool) {
    toasts.0.push(Toast {
        text,
        warning,
        until: time.elapsed_secs_f64() + TOAST_SECS,
    });
}

/// What the daemon said about a publish, a discard or a close (from `ReviewEvent`).
fn outcomes(
    mut events: MessageReader<ReviewEvent>,
    mut tabs: ResMut<ReviewTabs>,
    mut nav: ResMut<Nav>,
    mut toasts: ResMut<Toasts>,
    mut edits: ResMut<FinalizeEdits>,
    mut modes: ResMut<ExtraModes>,
    mut asks: ResMut<Asks>,
    time: Res<Time>,
) {
    for event in events.read() {
        match event {
            ReviewEvent::Published { pr, result } => {
                let text = if result.closed {
                    "Published to GitHub and closed the pull request"
                } else {
                    "Published to GitHub"
                };
                toast(&mut toasts, &time, text.to_string(), false);
                if let Some(e) = &result.close_error {
                    let text = format!("Could not close the pull request: {e}");
                    toast(&mut toasts, &time, text, true);
                }
                match result.unresolved.len() {
                    0 => {}
                    1 => toast(
                        &mut toasts,
                        &time,
                        "1 thread stayed open on GitHub".to_string(),
                        true,
                    ),
                    n => toast(
                        &mut toasts,
                        &time,
                        format!("{n} threads stayed open on GitHub"),
                        true,
                    ),
                }
                edits.0.retain(|(p, _), _| p != pr);
                close_tab(pr, &mut tabs, &mut nav, &mut modes);
                nav.go(&WindowTarget::Home);
            }
            // The pull request moved: open it again, so the diff, the header and the relocated
            // draft all come from the new head. The daemon's message says what to check.
            ReviewEvent::PublishFailed {
                pr,
                code: ErrorCode::Conflict,
                message,
            } => {
                if let Some(tab) = tabs.0.get_mut(pr) {
                    tab.ui.modal = None;
                    tab.ui.finalize.busy = false;
                    tab.ui.finalize.error = None;
                    tabs.retry(pr, &mut asks);
                }
                toast(&mut toasts, &time, message.clone(), true);
            }
            ReviewEvent::PublishFailed { pr, message, .. } => match tabs.0.get_mut(pr) {
                Some(tab) if tab.ui.modal == Some(Modal::Finalize) => {
                    tab.ui.finalize.error = Some(message.clone());
                    tab.ui.finalize.busy = false;
                }
                _ => toast(&mut toasts, &time, message.clone(), true),
            },
            ReviewEvent::Left(pr) => {
                edits.0.retain(|(p, _), _| p != pr);
                close_tab(pr, &mut tabs, &mut nav, &mut modes);
            }
        }
    }
}

/// `place` cut from the left to `max` characters so the line number stays: `…ures.service.ts:142`.
pub fn fit_place(path: &str, line: Option<u32>, max: usize) -> String {
    let name = file_name(path);
    let suffix = line.map(|l| format!(":{l}")).unwrap_or_default();
    if name.chars().count() + suffix.chars().count() <= max {
        return format!("{name}{suffix}");
    }
    // The line number is the part that locates the comment, so it is never the part that goes.
    let room = max.saturating_sub(suffix.chars().count() + 1);
    let tail: Vec<char> = name.chars().rev().take(room).collect();
    format!("…{}{suffix}", tail.into_iter().rev().collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Tell;
    use crate::fixture;
    use crate::nav::Screen;
    use crate::testing::{self, NOW};
    use crate::ui::composer::testkit::with_fonts;
    use crate::ui::text_area::Grow;
    use bevy::ecs::system::RunSystemOnce;
    use clusia_core::Role;
    use clusia_core::revalidation::{Decision, RoundState};
    use clusia_protocol::PublishResult;

    fn ready() -> Ready {
        let (view, news) = fixture::demo_review(NOW);
        Ready {
            view,
            news,
            cached_at: None,
        }
    }

    fn has_text(app: &mut App, needle: &str) -> bool {
        let mut q = app.world_mut().query::<&Text>();
        q.iter(app.world()).any(|t| t.0.contains(needle))
    }

    /// The demo review open and ready, without What's new (`open_ready(.., false)`).
    fn review_app() -> App {
        let mut app = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut app, false);
        app
    }

    /// The demo review with Finalize open; `fit` drops the obsolete item so Publish can work.
    fn finalize_app(fit: bool) -> App {
        let mut app = review_app();
        with_fonts(&mut app);
        let pr = fixture::demo_pr();
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            let tab = tabs.0.get_mut(&pr).unwrap();
            if fit && let Phase::Ready(r) = &mut tab.phase {
                r.view
                    .review
                    .draft
                    .items
                    .retain(|i| !matches!(i.status, ItemStatus::Obsolete { .. }));
            }
            tab.ui.modal = Some(Modal::Finalize);
        }
        testing::settle(&mut app);
        app
    }

    #[test]
    fn finalize_dims_in_grows_and_leaves_a_fading_dim_when_closed() {
        use crate::ui::modal::{FadingScrim, ModalCard};
        use crate::ui::tween::Tween;
        let mut app = finalize_app(false);
        let root = testing::find::<FinalizeModal>(&mut app, |_| true);
        assert!(
            app.world().get::<Tween>(root).is_some(),
            "the layer dims in"
        );
        let card = testing::find::<ModalCard>(&mut app, |_| true);
        assert_eq!(app.world().get::<ChildOf>(card).unwrap().parent(), root);
        assert!(app.world().get::<Tween>(card).is_some(), "the card grows");
        testing::run_tweens(&mut app);
        assert_eq!(
            app.world().get::<UiTransform>(card).unwrap().scale,
            Vec2::ONE
        );
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&fixture::demo_pr())
            .unwrap()
            .ui
            .modal = None;
        app.update();
        assert_eq!(testing::count::<FinalizeModal>(&mut app), 0);
        assert_eq!(testing::count::<FadingScrim>(&mut app), 1);
        testing::run_tweens(&mut app);
        assert_eq!(testing::count::<FadingScrim>(&mut app), 0);
        assert_eq!(testing::count::<Tween>(&mut app), 0);
    }

    fn form(app: &App) -> FinalizeForm {
        testing::tab(app, &fixture::demo_pr()).ui.finalize
    }

    #[test]
    fn demo_finalize_view() {
        let r = ready();
        let v = finalize_view(&r, &FinalizeForm::default());
        assert_eq!(v.title, "Finalize review");
        assert_eq!(v.subject, "rzorzal/clusia #123 · feat: auth refresh");
        assert_eq!(
            v.note,
            "Everything below goes to GitHub as one review. You can still edit any comment."
        );
        let labels: Vec<&str> = v.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["refresh.rs:44", "store.rs:88", "http.rs:20"]);
        assert!(v.items.iter().all(|i| i.editable));
        assert_eq!(
            v.items.iter().map(|i| i.obsolete).collect::<Vec<_>>(),
            [false, false, true]
        );
        assert_eq!(
            v.warning.as_deref(),
            Some(
                "1 comment no longer fits the diff. Remove it or re-add it on a current line before publishing."
            )
        );
        let titles: Vec<&str> = v.verdicts.iter().map(|c| c.title).collect();
        assert_eq!(titles, ["Comment", "Approve", "Request changes"]);
        assert_eq!(v.verdicts[2].detail, "Needs another pass");
        assert!(v.verdicts.iter().all(|c| !c.on));
        assert_eq!(
            v.blocked,
            Some("Fix the comments that no longer fit the diff first")
        );
    }

    #[test]
    fn suggestions_nobody_decided_are_counted_and_block_nothing() {
        let mut r = ready();
        let round = testing::demo_round(&mut r.view.review, RoundState::Done);
        r.view.review.revalidation = Some(round);
        let chosen = FinalizeForm {
            verdict: Some(Verdict::Comment),
            ..FinalizeForm::default()
        };
        // The moved comment, the obsolete one and the new wording wait.
        let v = finalize_view(&r, &chosen);
        assert_eq!(v.suggestions, 3);
        assert_eq!(
            suggestions_note(v.suggestions),
            "3 suggestions not reviewed: your draft is published as it is."
        );
        assert_eq!(
            suggestions_note(1),
            "1 suggestion not reviewed: your draft is published as it is."
        );
        assert_eq!(
            v.blocked,
            Some("Fix the comments that no longer fit the diff first"),
            "the demo's obsolete comment still blocks"
        );
    }

    #[test]
    fn an_obsolete_comment_removed_in_the_revalidation_blocks_nothing() {
        let mut r = ready();
        let round = testing::demo_round(&mut r.view.review, RoundState::Done);
        r.view.review.revalidation = Some(round);
        // *Remove comment* marks the item removed; the daemon keeps it obsolete in the draft.
        r.view.review.removed.insert("i3".into());
        let chosen = FinalizeForm {
            verdict: Some(Verdict::Comment),
            ..FinalizeForm::default()
        };
        let v = finalize_view(&r, &chosen);
        assert_eq!((v.warning, v.blocked), (None, None));
    }

    #[test]
    fn a_comment_removed_in_the_revalidation_reads_as_settled() {
        let mut r = ready();
        r.view.review.removed.insert("i3".into());
        let chosen = FinalizeForm::default();
        let v = finalize_view(&r, &chosen);
        let item = v.items.iter().find(|i| i.id == "i3").expect("the item");
        assert_eq!((item.obsolete, item.removed), (false, true));
        let mut app = finalize_app(false);
        let pr = fixture::demo_pr();
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            let ready = tabs.0.get_mut(&pr).and_then(|t| t.ready_mut()).unwrap();
            ready.view.review.removed.insert("i3".into());
        }
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "Removed in the revalidation"));
        assert!(!testing::shows(&mut app, "no longer fits the diff"));
    }

    #[test]
    fn an_accepted_proposal_alone_does_not_unblock_an_obsolete_comment() {
        let mut r = ready();
        let mut round = testing::demo_round(&mut r.view.review, RoundState::Done);
        // The user took new wording for the obsolete comment: it is not a removal.
        let wording = round
            .proposals
            .iter_mut()
            .find(|p| p.item == "i3")
            .expect("the obsolete proposal");
        wording.decision = Decision::Accepted;
        r.view.review.revalidation = Some(round);
        let chosen = FinalizeForm {
            verdict: Some(Verdict::Comment),
            ..FinalizeForm::default()
        };
        let v = finalize_view(&r, &chosen);
        assert!(v.warning.is_some());
        assert_eq!(
            v.blocked,
            Some("Fix the comments that no longer fit the diff first")
        );
    }

    #[test]
    fn the_modal_says_how_many_suggestions_wait() {
        let mut app = finalize_app(false);
        let pr = fixture::demo_pr();
        assert!(!testing::shows(&mut app, "not reviewed"));
        let round = {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            let ready = tabs.0.get_mut(&pr).and_then(|t| t.ready_mut()).unwrap();
            testing::demo_round(&mut ready.view.review, RoundState::Done)
        };
        testing::tell(
            &mut app,
            crate::bridge::Tell::Revalidation(crate::bridge::RevalidationTell::Round {
                pr,
                round: Some(round),
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(
            &mut app,
            "3 suggestions not reviewed: your draft is published as it is."
        ));
    }

    #[test]
    fn verdict_and_item_rules() {
        let mut r = ready();
        r.view
            .review
            .draft
            .items
            .retain(|i| !matches!(i.status, ItemStatus::Obsolete { .. }));
        let none = finalize_view(&r, &FinalizeForm::default());
        assert_eq!(
            (none.warning, none.blocked),
            (None, Some("Choose a verdict"))
        );
        let chosen = FinalizeForm {
            verdict: Some(Verdict::RequestChanges),
            ..FinalizeForm::default()
        };
        let v = finalize_view(&r, &chosen);
        assert_eq!(v.blocked, None);
        assert!(v.verdicts[2].on);
        let busy = FinalizeForm {
            busy: true,
            ..chosen.clone()
        };
        assert_eq!(finalize_view(&r, &busy).blocked, Some("Publishing…"));
        r.cached_at = Some(NOW - 3600);
        assert_eq!(
            finalize_view(&r, &chosen).blocked,
            Some("This is the cached copy — reconnect to publish")
        );
        // Two obsolete items read in the plural.
        let mut two = ready();
        for item in &mut two.view.review.draft.items {
            item.status = ItemStatus::Obsolete {
                reason: "gone".into(),
            };
        }
        let items = two.view.review.draft.items.len();
        assert_eq!(
            finalize_view(&two, &chosen).warning,
            Some(format!(
                "{items} comments no longer fit the diff. Remove them or re-add them on a current line before publishing."
            ))
        );
    }

    #[test]
    fn replies_resolves_and_general_notes_are_labeled() {
        let mut r = ready();
        let thread = clusia_core::draft::ThreadRef {
            id: "PRRT_1".into(),
            author: "mona".into(),
            path: Some("src/auth/refresh.rs".into()),
            line: Some(41),
        };
        let draft = &mut r.view.review.draft;
        draft
            .add(DraftKind::Reply, None, Some(thread.clone()), "Agree.", NOW)
            .unwrap();
        draft
            .add(DraftKind::Resolve, None, Some(thread), "", NOW)
            .unwrap();
        draft
            .add(DraftKind::General, None, None, "Nice change overall.", NOW)
            .unwrap();
        let v = finalize_view(&r, &FinalizeForm::default());
        let tail: Vec<(&str, bool)> = v.items[3..]
            .iter()
            .map(|i| (i.label.as_str(), i.editable))
            .collect();
        assert_eq!(
            tail,
            [
                ("Reply to @mona · refresh.rs:41", true),
                ("Resolve thread by @mona", false),
                ("General note", true)
            ]
        );
    }

    #[test]
    fn author_sees_comment_and_close_only() {
        let mut r = ready();
        r.view.role = Role::Author;
        let v = finalize_view(&r, &FinalizeForm::default());
        let cards: Vec<(&str, &str)> = v.verdicts.iter().map(|c| (c.title, c.detail)).collect();
        assert_eq!(
            cards,
            [
                ("Comment", "Feedback without a verdict"),
                ("Close PR", "Comment, then close it")
            ]
        );
        let mut app = finalize_app(true);
        let pr = fixture::demo_pr();
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            if let Phase::Ready(r) = &mut tabs.0.get_mut(&pr).unwrap().phase {
                r.view.role = Role::Author;
            }
        }
        testing::settle(&mut app);
        let mut q = app.world_mut().query::<&VerdictButton>();
        let mut shown: Vec<Verdict> = q.iter(app.world()).map(|b| b.verdict).collect();
        shown.sort_by_key(|v| format!("{v:?}"));
        assert_eq!(shown, [Verdict::ClosePr, Verdict::Comment]);
    }

    #[test]
    fn a_redrawn_finalize_keeps_the_summary_preview() {
        use crate::ui::composer::{ComposerModeButton, Slot};
        let mut app = finalize_app(true);
        let preview = testing::find::<ComposerModeButton>(&mut app, |b| {
            b.key.1 == Slot::FinalizeSummary && b.mode == ComposerMode::Preview
        });
        testing::activate(&mut app, preview);
        testing::settle(&mut app);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&fixture::demo_pr())
            .unwrap()
            .ui
            .finalize
            .verdict = Some(Verdict::Comment);
        app.update();
        let summary = testing::find::<SummaryArea>(&mut app, |_| true);
        assert_eq!(
            app.world().get::<Node>(summary).unwrap().display,
            Display::None,
            "drawn again in Preview, not in Write for a frame"
        );
    }

    #[test]
    fn closed_pr_disables_publish() {
        let mut r = ready();
        r.view.pr.closed = true;
        let chosen = FinalizeForm {
            verdict: Some(Verdict::Comment),
            ..FinalizeForm::default()
        };
        assert_eq!(
            finalize_view(&r, &chosen).blocked,
            Some("This pull request is closed — publishing is off")
        );
        r.view.pr.merged = true;
        assert_eq!(
            finalize_view(&r, &chosen).blocked,
            Some("This pull request is merged — publishing is off")
        );
        let mut app = finalize_app(true);
        let pr = fixture::demo_pr();
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            let tab = tabs.0.get_mut(&pr).unwrap();
            if let Phase::Ready(r) = &mut tab.phase {
                r.view.pr.closed = true;
            }
            tab.ui.finalize.verdict = Some(Verdict::Comment);
        }
        testing::settle(&mut app);
        assert_eq!(testing::count::<PublishButton>(&mut app), 0);
        assert!(has_text(&mut app, "Publish to GitHub"), "shown, disabled");
        let discard = testing::find::<DiscardButton>(&mut app, |_| true);
        testing::activate(&mut app, discard);
        assert_eq!(testing::recorded(&mut app), [Ask::Discard(pr.clone())]);
        testing::settle(&mut app);
        assert!(!app.world().resource::<ReviewTabs>().0.contains_key(&pr));
        assert_eq!(app.world().resource::<Nav>().screen, Screen::Home);
    }

    #[test]
    fn leaving_a_review_forgets_its_composer_modes() {
        use crate::ui::composer::{ComposerKey, ComposerMode, ExtraModes, Slot, mode_of, set_mode};
        let mut app = finalize_app(true);
        let pr = fixture::demo_pr();
        let summary = ComposerKey(pr.clone(), Slot::FinalizeSummary);
        let other = ComposerKey("rzorzal/other#7".parse().unwrap(), Slot::FinalizeSummary);
        {
            let world = app.world_mut();
            let mut extra = world.remove_resource::<ExtraModes>().unwrap();
            let mut tabs = world.remove_resource::<ReviewTabs>().unwrap();
            set_mode(&summary, ComposerMode::Preview, &mut tabs, &mut extra);
            set_mode(&other, ComposerMode::Preview, &mut tabs, &mut extra);
            world.insert_resource(extra);
            world.insert_resource(tabs);
        }
        let mode = |app: &App, key: &ComposerKey| {
            mode_of(
                key,
                app.world().resource::<ReviewTabs>(),
                app.world().resource::<ExtraModes>(),
            )
        };
        assert_eq!(mode(&app, &summary), ComposerMode::Preview);
        let discard = testing::find::<DiscardButton>(&mut app, |_| true);
        testing::activate(&mut app, discard);
        testing::settle(&mut app);
        assert_eq!(mode(&app, &summary), ComposerMode::Write, "forgotten");
        assert_eq!(
            mode(&app, &other),
            ComposerMode::Preview,
            "another review's modes stay"
        );
    }

    #[test]
    fn publish_sends_edits_then_the_review() {
        let mut app = finalize_app(true);
        let pr = fixture::demo_pr();
        let verdict =
            testing::find::<VerdictButton>(&mut app, |b| b.verdict == Verdict::RequestChanges);
        testing::activate(&mut app, verdict);
        testing::settle(&mut app);
        let summary = testing::find::<SummaryArea>(&mut app, |_| true);
        testing::type_into(&mut app, summary, "Two things on the token store.");
        let first = testing::find::<FinalizeItemArea>(&mut app, |a| a.index == 0);
        let id = app
            .world()
            .get::<FinalizeItemArea>(first)
            .unwrap()
            .id
            .clone();
        let before = app
            .world()
            .get::<EditableText>(first)
            .unwrap()
            .value()
            .to_string();
        testing::type_into(&mut app, first, "Please. ");
        let publish = testing::find::<PublishButton>(&mut app, |_| true);
        testing::activate(&mut app, publish);
        let asks = testing::recorded(&mut app);
        let [
            Ask::UpdateItem {
                pr: p1,
                id: edited,
                body,
                ..
            },
            Ask::Publish {
                pr: p2,
                verdict,
                summary,
            },
        ] = &asks[..]
        else {
            panic!("an update then the publish, got {asks:?}")
        };
        assert_eq!(
            (p1, edited, body.as_str()),
            (&pr, &id, format!("Please. {before}").as_str())
        );
        assert_eq!(
            (p2, *verdict, summary.as_str()),
            (
                &pr,
                Verdict::RequestChanges,
                "Two things on the token store."
            )
        );
        assert!(form(&app).busy);
        assert_eq!(
            testing::count::<PublishButton>(&mut app),
            0,
            "disabled while publishing"
        );
    }

    #[test]
    fn publish_failure_keeps_the_modal_open() {
        let mut app = finalize_app(true);
        let pr = fixture::demo_pr();
        let verdict = testing::find::<VerdictButton>(&mut app, |b| b.verdict == Verdict::Approve);
        testing::activate(&mut app, verdict);
        testing::settle(&mut app);
        let summary = testing::find::<SummaryArea>(&mut app, |_| true);
        testing::type_into(&mut app, summary, "Ship it.");
        let publish = testing::find::<PublishButton>(&mut app, |_| true);
        testing::activate(&mut app, publish);
        testing::recorded(&mut app);
        let message = "GitHub refused the request: Review cannot be submitted";
        testing::tell(
            &mut app,
            Tell::PublishFailed {
                pr: pr.clone(),
                code: ErrorCode::Upstream,
                message: message.into(),
            },
        );
        testing::settle(&mut app);
        let tab = testing::tab(&app, &pr);
        assert_eq!(tab.ui.modal, Some(Modal::Finalize));
        assert_eq!(tab.ui.finalize.error.as_deref(), Some(message));
        assert!(!tab.ui.finalize.busy);
        let Phase::Ready(r) = &tab.phase else {
            panic!("still ready")
        };
        assert_eq!(r.view.review.draft.items.len(), 2, "every item kept");
        assert!(has_text(&mut app, message));
        assert_eq!(
            testing::count::<PublishButton>(&mut app),
            1,
            "can try again"
        );
        let summary = testing::find::<SummaryArea>(&mut app, |_| true);
        assert_eq!(
            app.world()
                .get::<EditableText>(summary)
                .unwrap()
                .value()
                .to_string(),
            "Ship it."
        );
    }

    #[test]
    fn a_publish_conflict_reopens_the_review() {
        let mut app = finalize_app(true);
        let pr = fixture::demo_pr();
        let verdict = testing::find::<VerdictButton>(&mut app, |b| b.verdict == Verdict::Comment);
        testing::activate(&mut app, verdict);
        testing::settle(&mut app);
        let publish = testing::find::<PublishButton>(&mut app, |_| true);
        testing::activate(&mut app, publish);
        testing::recorded(&mut app);
        let message = "the pull request has new commits; open it again to check the moved comments";
        testing::tell(
            &mut app,
            Tell::PublishFailed {
                pr: pr.clone(),
                code: ErrorCode::Conflict,
                message: message.into(),
            },
        );
        testing::settle(&mut app);
        let tab = testing::tab(&app, &pr);
        assert_eq!(tab.ui.modal, None, "Finalize closes");
        assert!(!tab.ui.finalize.busy);
        assert_eq!(tab.ui.finalize.error, None);
        assert!(
            matches!(tab.phase, Phase::Loading { .. }),
            "opens again for the new diff and the relocated draft"
        );
        assert_eq!(testing::recorded(&mut app), [Ask::OpenReview(pr.clone())]);
        assert_eq!(testing::count::<FinalizeModal>(&mut app), 0);
        let toast = app.world().resource::<Toasts>().0.last().cloned().unwrap();
        assert_eq!((toast.text.as_str(), toast.warning), (message, true));
    }

    #[test]
    fn published_closes_the_tab_and_goes_home() {
        let mut app = finalize_app(true);
        let pr = fixture::demo_pr();
        testing::tell(
            &mut app,
            Tell::Published {
                pr: pr.clone(),
                result: PublishResult {
                    url: Some(
                        "https://github.com/rzorzal/clusia/pull/123#pullrequestreview-42".into(),
                    ),
                    closed: false,
                    unresolved: vec!["PRRT_1".into()],
                    close_error: None,
                },
            },
        );
        testing::settle(&mut app);
        let nav = app.world().resource::<Nav>();
        assert_eq!(nav.screen, Screen::Home);
        assert!(!nav.reviews.contains(&pr));
        assert!(!app.world().resource::<ReviewTabs>().0.contains_key(&pr));
        let toasts: Vec<(String, bool)> = app
            .world()
            .resource::<Toasts>()
            .0
            .iter()
            .map(|t| (t.text.clone(), t.warning))
            .collect();
        assert_eq!(
            toasts,
            [
                ("Published to GitHub".to_string(), false),
                ("1 thread stayed open on GitHub".to_string(), true)
            ]
        );
        assert_eq!(testing::count::<FinalizeModal>(&mut app), 0);
    }

    #[test]
    fn published_but_closing_failed_warns() {
        let mut app = finalize_app(true);
        let pr = fixture::demo_pr();
        testing::tell(
            &mut app,
            Tell::Published {
                pr: pr.clone(),
                result: PublishResult {
                    url: Some(
                        "https://github.com/rzorzal/clusia/pull/123#pullrequestreview-42".into(),
                    ),
                    closed: false,
                    unresolved: vec![],
                    close_error: Some("GitHub refused the request: boom".into()),
                },
            },
        );
        testing::settle(&mut app);
        assert!(!app.world().resource::<ReviewTabs>().0.contains_key(&pr));
        let toasts: Vec<(String, bool)> = app
            .world()
            .resource::<Toasts>()
            .0
            .iter()
            .map(|t| (t.text.clone(), t.warning))
            .collect();
        assert_eq!(
            toasts,
            [
                ("Published to GitHub".to_string(), false),
                (
                    "Could not close the pull request: GitHub refused the request: boom"
                        .to_string(),
                    true
                )
            ]
        );
    }

    #[test]
    fn obsolete_items_can_be_removed_from_finalize() {
        let mut app = finalize_app(false);
        let pr = fixture::demo_pr();
        let obsolete = finalize_view(&ready(), &FinalizeForm::default())
            .items
            .into_iter()
            .find(|i| i.obsolete)
            .expect("the demo's obsolete item")
            .id;
        assert_eq!(testing::count::<FinalizeRemove>(&mut app), 3, "every item");
        let remove = testing::find::<FinalizeRemove>(&mut app, |r| r.id == obsolete);
        testing::activate(&mut app, remove);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::RemoveItem {
                pr: pr.clone(),
                id: obsolete.clone()
            }]
        );
        // The daemon removes it: nothing blocks Publish any more.
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            let tab = tabs.0.get_mut(&pr).unwrap();
            if let Phase::Ready(r) = &mut tab.phase {
                r.view.review.draft.remove(&obsolete).unwrap();
            }
            tab.ui.finalize.verdict = Some(Verdict::Comment);
        }
        testing::settle(&mut app);
        assert_eq!(testing::count::<FinalizeRemove>(&mut app), 2);
        assert_eq!(testing::count::<PublishButton>(&mut app), 1);
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            if let Phase::Ready(r) = &mut tabs.0.get_mut(&pr).unwrap().phase {
                r.cached_at = Some(NOW - 3600);
            }
        }
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<FinalizeRemove>(&mut app),
            0,
            "nothing is removed from the cached copy"
        );
    }

    #[test]
    fn item_and_summary_areas_grow() {
        let mut app = finalize_app(false);
        let item = testing::find::<FinalizeItemArea>(&mut app, |a| a.index == 0);
        let summary = testing::find::<SummaryArea>(&mut app, |_| true);
        let grow = |e| *app.world().get::<Grow>(e).expect("a growing area");
        assert_eq!(grow(item), Grow { min: 1.0, max: 6.0 });
        assert_eq!(grow(summary), Grow { min: 3.0, max: 6.0 });
    }

    #[test]
    fn item_and_summary_areas_open_at_their_start() {
        let mut app = review_app();
        with_fonts(&mut app);
        let pr = fixture::demo_pr();
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            let tab = tabs.0.get_mut(&pr).unwrap();
            tab.ui.finalize.summary = "on the token store.".into();
            tab.ui.modal = Some(Modal::Finalize);
        }
        testing::settle(&mut app);
        let item = testing::find::<FinalizeItemArea>(&mut app, |a| a.index == 0);
        let summary = testing::find::<SummaryArea>(&mut app, |_| true);
        let value = |app: &App, e| {
            app.world()
                .get::<EditableText>(e)
                .unwrap()
                .value()
                .to_string()
        };
        let before = value(&app, item);
        testing::type_into(&mut app, item, "Also: ");
        testing::type_into(&mut app, summary, "Two things ");
        assert_eq!(value(&app, item), format!("Also: {before}"));
        assert_eq!(value(&app, summary), "Two things on the token store.");
    }

    #[test]
    fn item_edits_save_on_blur() {
        let mut app = finalize_app(false);
        let pr = fixture::demo_pr();
        let area = testing::find::<FinalizeItemArea>(&mut app, |a| a.index == 1);
        let id = app
            .world()
            .get::<FinalizeItemArea>(area)
            .unwrap()
            .id
            .clone();
        let before = app
            .world()
            .get::<EditableText>(area)
            .unwrap()
            .value()
            .to_string();
        testing::type_into(&mut app, area, "Thanks! ");
        app.world_mut().trigger(FocusLost { entity: area });
        app.update();
        let asks = testing::recorded(&mut app);
        let [
            Ask::UpdateItem {
                pr: to,
                id: sent,
                body,
                ticket,
            },
        ] = &asks[..]
        else {
            panic!("one UpdateItem, got {asks:?}")
        };
        assert_eq!(
            (to, sent, body.as_str()),
            (&pr, &id, format!("Thanks! {before}").as_str())
        );
        assert!(*ticket > 0);
        // Once the daemon has the new text, another blur sends nothing.
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            if let Phase::Ready(r) = &mut tabs.0.get_mut(&pr).unwrap().phase {
                r.view.review.draft.update_body(&id, body).unwrap();
            }
        }
        app.world_mut().trigger(FocusLost { entity: area });
        app.update();
        assert!(testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn the_cached_copy_sends_no_edits_and_no_discard() {
        let mut app = finalize_app(false);
        let pr = fixture::demo_pr();
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            if let Phase::Ready(r) = &mut tabs.0.get_mut(&pr).unwrap().phase {
                r.cached_at = Some(NOW - 3600);
            }
        }
        testing::settle(&mut app);
        assert_eq!(testing::count::<DiscardButton>(&mut app), 0, "disabled");
        assert!(has_text(&mut app, "Discard review"), "shown, disabled");
        let area = testing::find::<FinalizeItemArea>(&mut app, |a| a.index == 0);
        testing::type_into(&mut app, area, " Edited.");
        app.world_mut().trigger(FocusLost { entity: area });
        app.update();
        assert!(testing::recorded(&mut app).is_empty(), "no UpdateItem");
        let save = testing::find::<SaveButton>(&mut app, |_| true);
        testing::activate(&mut app, save);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::CloseReview(pr.clone())],
            "only the close"
        );
    }

    #[test]
    fn save_for_later_and_escape() {
        let mut app = finalize_app(false);
        let pr = fixture::demo_pr();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .press(KeyCode::Escape);
        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .release(KeyCode::Escape);
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .clear();
        assert_eq!(
            testing::tab(&app, &pr).ui.modal,
            None,
            "Esc closes Finalize"
        );
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            tabs.0.get_mut(&pr).unwrap().ui.modal = Some(Modal::Finalize);
        }
        testing::settle(&mut app);
        let save = testing::find::<SaveButton>(&mut app, |_| true);
        testing::activate(&mut app, save);
        assert_eq!(testing::recorded(&mut app), [Ask::CloseReview(pr.clone())]);
        testing::settle(&mut app);
        assert!(!app.world().resource::<Nav>().reviews.contains(&pr));
    }

    #[test]
    fn a_long_place_is_cut_from_the_left_and_keeps_its_line() {
        let long = "src/app/fixtures/live-fixtures.service.ts";
        let cut = fit_place(long, Some(142), 17);
        assert!(cut.starts_with('…'), "{cut}");
        assert!(cut.ends_with(".ts:142"), "{cut}");
        assert_eq!(cut.chars().count(), 17);
        assert_eq!(
            fit_place("src/auth/refresh.rs", Some(44), 17),
            "refresh.rs:44"
        );
        assert_eq!(fit_place("src/auth/refresh.rs", None, 17), "refresh.rs");
        let no_line = fit_place(long, None, 17);
        assert_eq!(no_line.chars().count(), 17);
        assert!(no_line.ends_with("service.ts"), "{no_line}");
        let huge_line = fit_place("a-long-name.ts", Some(1_234_567), 6);
        assert!(
            huge_line.ends_with(":1234567"),
            "the line is never cut: {huge_line}"
        );
    }

    #[test]
    fn a_cut_place_has_the_full_path_as_its_hover_title() {
        let mut r = ready();
        let long = "src/app/fixtures/live-fixtures.service.ts";
        r.view.review.draft.items[0].anchor.as_mut().unwrap().path = long.into();
        r.view.review.draft.items[0].anchor.as_mut().unwrap().line = 142;
        let v = finalize_view(&r, &FinalizeForm::default());
        assert!(v.items[0].label.ends_with(":142"));
        assert!(v.items[0].label.starts_with('…'));
        assert_eq!(v.items[0].title.as_deref(), Some(&*format!("{long}:142")));
        assert_eq!(v.items[1].title.as_deref(), Some("src/auth/store.rs:88"));
        assert_eq!(v.items[1].label, "store.rs:88");
    }

    #[test]
    fn a_click_outside_closes_finalize_and_nothing_else_does() {
        let mut app = finalize_app(false);
        let pr = fixture::demo_pr();
        let backdrop = testing::find::<FinalizeModal>(&mut app, |_| true);
        let inside = testing::find::<SaveButton>(&mut app, |_| true);
        testing::click(&mut app, inside);
        assert_eq!(
            testing::tab(&app, &pr).ui.modal,
            Some(Modal::Finalize),
            "a click inside the dialog keeps it"
        );
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            tabs.0.get_mut(&pr).unwrap().ui.finalize.busy = true;
        }
        testing::click(&mut app, backdrop);
        assert_eq!(
            testing::tab(&app, &pr).ui.modal,
            Some(Modal::Finalize),
            "a publish in flight keeps it, as Esc does"
        );
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            tabs.0.get_mut(&pr).unwrap().ui.finalize.busy = false;
        }
        testing::click(&mut app, backdrop);
        assert_eq!(testing::tab(&app, &pr).ui.modal, None);
        assert!(
            testing::recorded(&mut app).is_empty(),
            "the draft is untouched"
        );
    }

    /// The demo review at `size` with Finalize open over `items` draft comments, the first of
    /// them a long path with a fenced code block as long as `code_lines`.
    fn crowded_finalize(size: UVec2, items: usize, code_lines: usize) -> App {
        let mut app = testing::layout_app(fixture::demo(NOW), size);
        let pr = testing::open_ready(&mut app, false);
        with_fonts(&mut app);
        let code: String = (1..=code_lines)
            .map(|n| format!("let step_{n} = {n};\n"))
            .collect();
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            let tab = tabs.0.get_mut(&pr).unwrap();
            if let Phase::Ready(r) = &mut tab.phase {
                let draft = &mut r.view.review.draft;
                draft
                    .items
                    .retain(|i| !matches!(i.status, ItemStatus::Obsolete { .. }));
                let template = draft.items[0].clone();
                draft.items.clear();
                for n in 0..items {
                    let mut item = template.clone();
                    item.id = format!("c{n}");
                    let anchor = item.anchor.as_mut().unwrap();
                    anchor.line = 100 + n as u32;
                    if n == 0 {
                        anchor.path = "src/app/fixtures/live-fixtures.service.ts".into();
                        item.body = format!("Why not this?\n\n```rust\n{code}```");
                    }
                    draft.items.push(item);
                }
            }
            tab.ui.modal = Some(Modal::Finalize);
            tab.ui.finalize.verdict = Some(Verdict::Comment);
        }
        testing::settle(&mut app);
        testing::settle(&mut app);
        app
    }

    fn bottom(app: &App, e: Entity) -> f32 {
        let (_, y, _, h) = testing::frame(app, e);
        y + h
    }

    #[test]
    fn many_comments_scroll_in_the_body_and_the_footer_stays_in_view() {
        for (size, large) in [
            (UVec2::new(1280, 800), false),
            (UVec2::new(1280, 800), true),
            (UVec2::new(1000, 420), false),
            (UVec2::new(1000, 420), true),
        ] {
            let mut app = crowded_finalize(size, 14, 20);
            if large {
                testing::set_config_locally(&mut app, "appearance.text_size", "large");
                testing::settle(&mut app);
            }
            let what = format!("{size} large={large}");
            let height = size.y as f32;
            let body = testing::find::<FinalizeBody>(&mut app, |_| true);
            let footer = testing::find::<FinalizeFooter>(&mut app, |_| true);
            let publish = testing::find::<PublishButton>(&mut app, |_| true);
            assert!(
                bottom(&app, footer) <= height,
                "{what}: the footer is on screen"
            );
            assert!(
                bottom(&app, publish) <= height,
                "{what}: Publish is on screen"
            );
            let modal = testing::find::<FinalizeModal>(&mut app, |_| true);
            let (_, top, _, _) = testing::frame(&app, modal);
            assert!(top >= 0.0, "{what}: the dialog starts on screen");
            let node = app.world().get::<Node>(body).unwrap();
            assert_eq!(node.overflow, Overflow::scroll_y(), "{what}");
            let laid = app.world().get::<bevy::ui::ComputedNode>(body).unwrap();
            assert!(
                laid.size.y > 100.0,
                "{what}: the body keeps a usable height"
            );
            assert!(
                laid.content_size.y > laid.size.y + 1.0,
                "{what}: the content ({}) is taller than the body ({}) and scrolls",
                laid.content_size.y,
                laid.size.y
            );
        }
    }

    #[test]
    fn a_comment_editor_grows_to_a_cap_then_scrolls_inside() {
        let mut app = crowded_finalize(UVec2::new(1280, 800), 3, 40);
        let areas = |app: &mut App| -> Vec<(String, f32)> {
            let mut q = app
                .world_mut()
                .query::<(Entity, &crate::ui::composer::ComposerArea)>();
            let found: Vec<(Entity, ComposerKey)> =
                q.iter(app.world()).map(|(e, a)| (e, a.0.clone())).collect();
            found
                .into_iter()
                .filter_map(|(e, key)| match key.1 {
                    Slot::FinalizeItem(id) => Some((id, testing::frame(app, e).3)),
                    _ => None,
                })
                .collect()
        };
        testing::settle(&mut app);
        let heights = areas(&mut app);
        let tall = heights.iter().find(|(id, _)| id == "c0").unwrap().1;
        let short = heights.iter().find(|(id, _)| id == "c1").unwrap().1;
        assert!(
            tall > short,
            "a longer text has a taller editor ({tall} vs {short})"
        );
        // 6 lines of 13 px text at the editor's line height, plus its padding and border: far
        // less than the 46 lines of the text.
        assert!(tall < 200.0, "the editor stops at its cap: {tall}");
    }

    #[test]
    fn the_place_column_follows_the_text_size_and_holds_a_cut_place() {
        let mut app = crowded_finalize(UVec2::new(1280, 800), 3, 4);
        testing::set_config_locally(&mut app, "appearance.text_size", "large");
        testing::settle(&mut app);
        let label = testing::find::<PlaceLabel>(&mut app, |_| true);
        let (_, _, column, _) = testing::frame(&app, label);
        assert!(
            (column - PLACE_WIDTH * 15.0 / 13.0).abs() <= 1.0,
            "the column is {column} wide"
        );
        let text = app.world().get::<Children>(label).unwrap()[0];
        let cut = app.world().get::<Text>(text).unwrap().0.clone();
        assert_eq!(cut.chars().count(), PLACE_CHARS, "{cut}");
        let (_, _, shown, _) = testing::frame(&app, text);
        assert!(
            shown <= column,
            "{cut} is {shown} wide in a {column} column"
        );
    }

    #[test]
    fn the_title_opens_under_a_high_row_and_over_a_low_one() {
        let mut app = crowded_finalize(UVec2::new(1280, 800), 14, 4);
        let mut q = app
            .world_mut()
            .query_filtered::<(Entity, &Children), With<PlaceLabel>>();
        let labels: Vec<(Entity, Entity)> = q
            .iter(app.world())
            .filter_map(|(e, c)| {
                c.iter()
                    .find(|c| app.world().get::<PlaceTitle>(*c).is_some())
                    .map(|t| (e, t))
            })
            .collect();
        let (first, first_title) = labels[0];
        let (last, last_title) = *labels.last().unwrap();
        for label in [first, last] {
            app.world_mut().entity_mut(label).insert(Hovered(true));
        }
        // The picking systems would reset the hover of a pointer that is not there.
        app.world_mut().run_system_once(show_place_titles).unwrap();
        let node = |app: &App, e: Entity| app.world().get::<Node>(e).unwrap().clone();
        let high = node(&app, first_title);
        assert_eq!(
            (high.display, high.top, high.bottom),
            (Display::Flex, percent(100), Val::Auto)
        );
        let low = node(&app, last_title);
        assert_eq!(
            (low.display, low.top, low.bottom),
            (Display::Flex, Val::Auto, percent(100))
        );
    }
}
