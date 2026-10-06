//! Finalize (spec §6.4, mockup `Finalize.png`): every draft item, the summary and the verdict,
//! published to GitHub as one review. A failure keeps the modal open with everything in it.

use std::collections::HashMap;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input::ButtonInput;
use bevy::input_focus::FocusLost;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy::ui_widgets::{Activate, Button as WidgetButton, observe};
use clusia_core::{DraftKind, ItemStatus, PrRef, Verdict};
use clusia_protocol::{ErrorCode, WindowTarget};

use crate::bridge::{Ask, Asks, Connection, Model, TOAST_SECS, Toast, Toasts};
use crate::fonts::UiFonts;
use crate::nav::Nav;
use crate::review_state::{FinalizeForm, Modal, Phase, Ready, ReviewEvent, ReviewTabs, Tickets};
use crate::screens::review::ReviewSystems;
use crate::screens::review::leave::close_tab;
use crate::screens::review::shell::ModalFor;
use crate::theme::Swatch;
use crate::ui::kit::{
    Clickable, Fill, HoverFill, Stroke, Tone, Type, Variant, badge, button, disabled_button, panel,
    text,
};
use crate::ui::modal::{escape_pressed, modal_card, modal_root};
use crate::ui::text_area::text_area;

#[derive(Debug, Clone, PartialEq)]
pub struct FinalizeItem {
    pub id: String,
    /// "refresh.rs:44", "General note", "Reply to @mona · refresh.rs:41", "Resolve thread by @mona".
    pub label: String,
    pub body: String,
    /// Resolves have no text.
    pub editable: bool,
    pub obsolete: bool,
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
    pub verdicts: Vec<VerdictCard>,
    /// Why Publish is disabled (`None`: it is enabled).
    pub blocked: Option<&'static str>,
    pub error: Option<String>,
    pub busy: bool,
    /// The cached copy: nothing is sent from it, so Discard is off too.
    pub cached: bool,
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

fn place(path: Option<&str>, line: Option<u32>) -> String {
    match (path, line) {
        (Some(p), Some(l)) => format!("{}:{l}", file_name(p)),
        (Some(p), None) => file_name(p).to_string(),
        _ => String::new(),
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
                body: i.body.clone(),
                editable: i.kind != DraftKind::Resolve,
                obsolete: matches!(i.status, ItemStatus::Obsolete { .. }),
            }
        })
        .collect();
    let obsolete = items.iter().filter(|i| i.obsolete).count();
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
        verdicts,
        blocked,
        error: form.error.clone(),
        busy: form.busy,
        cached: ready.cached_at.is_some(),
    }
}
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FinalizeModal(pub PrRef);

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
    fonts: Res<UiFonts>,
    mut parts: Query<(Entity, &mut FinalizePart)>,
) {
    for (entity, mut part) in &mut parts {
        if part.built.is_some() && !tabs.is_changed() && !model.is_changed() {
            continue;
        }
        let Some(tab) = tabs.0.get(&part.pr) else {
            continue;
        };
        let Phase::Ready(ready) = &tab.phase else {
            continue;
        };
        let mut v = finalize_view(ready, &tab.ui.finalize);
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
            card_content(p, fonts, &pr, &v, &summary, &edits);
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
) {
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(12),
        padding: UiRect::axes(px(24), px(20)),
        ..default()
    })
    .with_children(|c| {
        c.spawn(Node {
            column_gap: px(10),
            align_items: AlignItems::Baseline,
            ..default()
        })
        .with_children(|h| {
            h.spawn(text(fonts, v.title, Type::HEADING.size(18.0)));
            h.spawn(text(fonts, v.subject.clone(), Type::MUTED));
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
                row.spawn((
                    Node {
                        width: px(112),
                        flex_shrink: 0.0,
                        ..default()
                    },
                    children![text(fonts, item.label.clone(), Type::MONO.ink(Swatch::Fg))],
                ));
                if item.editable {
                    let value = edits
                        .0
                        .get(&(pr.clone(), item.id.clone()))
                        .unwrap_or(&item.body);
                    row.spawn((
                        text_area(fonts, value, 2.0, index as u64 + 1),
                        FinalizeItemArea {
                            pr: pr.clone(),
                            id: item.id.clone(),
                            index,
                        },
                    ));
                } else {
                    row.spawn(text(fonts, "Marked to resolve on publish", Type::MUTED));
                }
                if item.obsolete {
                    row.spawn(badge(fonts, "obsolete", Tone::Orange));
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
        c.spawn(text(fonts, "Summary", Type::STRONG));
        c.spawn((text_area(fonts, summary, 3.0, 0), SummaryArea(pr.clone())));
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
                border: UiRect::top(px(1)),
                ..default()
            },
            Swatch::Chrome,
        ),
        BorderColor::default(),
        Stroke(Swatch::Line),
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
    close_tab(pr, &mut tabs, &mut nav);
}

fn on_discard(
    activate: On<Activate>,
    buttons: Query<&DiscardButton>,
    mut tabs: ResMut<ReviewTabs>,
    mut nav: ResMut<Nav>,
    mut asks: ResMut<Asks>,
    mut edits: ResMut<FinalizeEdits>,
) {
    let Ok(DiscardButton(pr)) = buttons.get(activate.entity) else {
        return;
    };
    asks.send(Ask::Discard(pr.clone()));
    edits.0.retain(|(p, _), _| p != pr);
    close_tab(pr, &mut tabs, &mut nav);
}

fn escape_closes(keys: Res<ButtonInput<KeyCode>>, nav: Res<Nav>, mut tabs: ResMut<ReviewTabs>) {
    if !escape_pressed(&keys) {
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

/// What the daemon said about a publish, a discard or a close (`ReviewEvent`, A1).
fn outcomes(
    mut events: MessageReader<ReviewEvent>,
    mut tabs: ResMut<ReviewTabs>,
    mut nav: ResMut<Nav>,
    mut toasts: ResMut<Toasts>,
    mut edits: ResMut<FinalizeEdits>,
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
                close_tab(pr, &mut tabs, &mut nav);
                nav.go(&WindowTarget::Home);
            }
            ReviewEvent::PublishFailed { pr, code, message } => {
                let text = if *code == ErrorCode::Conflict {
                    "The pull request moved; your comments were re-anchored — check them"
                        .to_string()
                } else {
                    message.clone()
                };
                match tabs.0.get_mut(pr) {
                    Some(tab) if tab.ui.modal == Some(Modal::Finalize) => {
                        tab.ui.finalize.error = Some(text);
                        tab.ui.finalize.busy = false;
                    }
                    _ => toast(&mut toasts, &time, text, true),
                }
            }
            ReviewEvent::Left(pr) => {
                edits.0.retain(|(p, _), _| p != pr);
                close_tab(pr, &mut tabs, &mut nav);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::Tell;
    use crate::fixture;
    use crate::nav::Screen;
    use crate::testing::{self, NOW};
    use clusia_core::Role;
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
        testing::type_into(&mut app, first, " Please.");
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
            (&pr, &id, format!("{before} Please.").as_str())
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
        testing::tell(
            &mut app,
            Tell::PublishFailed {
                pr: pr.clone(),
                code: ErrorCode::Conflict,
                message: "the pull request has new commits".into(),
            },
        );
        testing::settle(&mut app);
        assert_eq!(
            form(&app).error.as_deref(),
            Some("The pull request moved; your comments were re-anchored — check them")
        );
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
        testing::type_into(&mut app, area, " Thanks!");
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
            (&pr, &id, format!("{before} Thanks!").as_str())
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
}
