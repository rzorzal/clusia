//! The Full view of a Calls node (`Modal::CallsNode`): the whole function, a comment field, and
//! **Add to draft**. Like the dialog of a diagram box, its comment is written in `TabUi.editor`
//! (target `EditTarget::CallsNode`), so the ticket and the daemon's `Saved` and `Refused` work as
//! in every other editor. Closing it leaves the node selected.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input::ButtonInput;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, observe};
use clusia_calls::NodeKind;
use clusia_core::PrRef;

use super::model::{Calls, CallsModel};
use super::panel::{CardView, FINISH_FIRST, card_view};
use crate::bridge::{Connection, Model, Toasts};
use crate::fonts::UiFonts;
use crate::nav::{Nav, Screen};
use crate::review_state::{EditTarget, Editor, Modal, Phase, ReviewTabs, TabUi};
use crate::screens::review::diagrams::modal::{PLACE_NOT_IN_DIFF, body_block};
use crate::screens::review::diff::unsent;
use crate::screens::review::editor::{
    AreaTarget, EDITOR_AREA, EditorArea, EditorSubmit, not_sent, on_submit_button, read_only_reason,
};
use crate::screens::review::shell::ModalFor;
use crate::theme::Swatch;
use crate::ui::composer::ComposerMode;
use crate::ui::kit::{Fill, Stroke, Type, Variant, button, disabled_button, panel, text};
use crate::ui::modal::{BackdropClicked, escape_pressed, modal_card, modal_root};
use crate::ui::text_area::text_area;

/// The root of `pr`'s Full view.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FullModal(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct FullCancel(pub PrRef);

/// Everything the Full view draws but the typed text.
#[derive(Debug, Clone, PartialEq)]
struct FullView {
    card: CardView,
    kind: NodeKind,
    /// `src/auth/store.rs:20–30` once the code has arrived.
    place: String,
    hint: Option<&'static str>,
    error: Option<String>,
    saving: bool,
    blocked: Option<&'static str>,
}

#[derive(Component)]
pub struct FullPart {
    pub pr: PrRef,
    built: Option<FullView>,
}

/// The function a Calls node stands for, by `FunctionId.0`.
fn function_of(model: &CallsModel, key: &str) -> Option<(String, NodeKind)> {
    let node = model.graph.as_ref()?.nodes.iter().find(|n| n.key == key)?;
    Some((node.function.as_ref()?.0.clone(), node.kind))
}

/// Puts the Full view away: the modal and its comment. The node stays selected.
fn close(ui: &mut TabUi) {
    ui.modal = None;
    if matches!(
        ui.editor.as_ref().map(|e| &e.target),
        Some(EditTarget::CallsNode { .. })
    ) {
        ui.editor = None;
    }
}

pub(super) fn open_full(
    mut commands: Commands,
    nav: Res<Nav>,
    mut tabs: ResMut<ReviewTabs>,
    calls: Res<Calls>,
    modals: Query<&ModalFor>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Screen::Review(pr) = &nav.screen else {
        return;
    };
    let Some(tab) = tabs.0.get(pr) else { return };
    if tab.ui.modal != Some(Modal::CallsNode) || !matches!(tab.phase, Phase::Ready(_)) {
        return;
    }
    if modals
        .iter()
        .any(|m| &m.pr == pr && m.modal == Modal::CallsNode)
    {
        return;
    }
    let function = calls
        .0
        .get(pr)
        .zip(tab.ui.calls_node.as_deref())
        .and_then(|(m, key)| function_of(m, key))
        .map(|(f, _)| f);
    let Some(tab) = tabs.0.get_mut(pr) else {
        return;
    };
    let Some(function) = function else {
        tab.ui.modal = None;
        return;
    };
    let target = EditTarget::CallsNode { function };
    let same = tab.ui.editor.as_ref().is_some_and(|e| e.target == target);
    if !same && unsent(tab.ui.editor.as_ref()) {
        not_sent(&mut toasts, &time, FINISH_FIRST);
        tab.ui.modal = None;
        return;
    }
    if !same {
        tab.ui.editor = Some(Editor {
            target,
            text: String::new(),
            error: None,
            ticket: None,
            mode: ComposerMode::Write,
        });
    }
    commands
        .spawn((
            modal_root(),
            ModalFor {
                pr: pr.clone(),
                modal: Modal::CallsNode,
            },
            FullModal(pr.clone()),
        ))
        .with_children(|root| {
            root.spawn((
                modal_card(620.0),
                FullPart {
                    pr: pr.clone(),
                    built: None,
                },
            ));
        });
}

pub(super) fn rebuild_full(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    calls: Res<Calls>,
    model: Res<Model>,
    fonts: Res<UiFonts>,
    mut parts: Query<(Entity, &mut FullPart)>,
) {
    let live = model.connection == Connection::Live;
    for (entity, mut part) in &mut parts {
        let (Some(tab), Some(m)) = (tabs.0.get(&part.pr), calls.0.get(&part.pr)) else {
            continue;
        };
        let Some(card) = card_view(m, &tab.ui, live) else {
            continue;
        };
        let Some((function, kind)) = function_of(m, &card.key) else {
            continue;
        };
        let target = EditTarget::CallsNode { function };
        let editor = tab.ui.editor.as_ref().filter(|e| e.target == target);
        let place = m
            .codes
            .get(&card.key)
            .and_then(|c| Some((c.lines.first()?.number, c.lines.last()?.number)))
            .zip(card.place.split_once(':'))
            .map_or(card.place.clone(), |((first, last), (path, _))| {
                format!("{path}:{first}–{last}")
            });
        let view = FullView {
            hint: (!matches!(
                kind,
                NodeKind::Changed | NodeKind::Signature | NodeKind::Moved | NodeKind::New
            ))
            .then_some(PLACE_NOT_IN_DIFF),
            kind,
            place,
            error: editor.and_then(|e| e.error.clone()),
            saving: editor.is_some_and(|e| e.ticket.is_some()),
            blocked: read_only_reason(&tabs, &model, &part.pr),
            card,
        };
        if part.built.as_ref() == Some(&view) {
            continue;
        }
        let typed = editor.map(|e| e.text.clone()).unwrap_or_default();
        let pr = part.pr.clone();
        commands.entity(entity).despawn_related::<Children>();
        commands.entity(entity).with_children(|p| {
            full_content(p, &fonts, &pr, &view, &typed, AreaTarget::new(target))
        });
        part.built = Some(view);
    }
}

/// The dot before the name: the node's fill and ring.
fn dot_of(kind: NodeKind) -> (Swatch, Swatch) {
    match kind {
        NodeKind::Changed | NodeKind::Moved => (Swatch::ChangedBg, Swatch::Green),
        NodeKind::New => (Swatch::GreenSoft, Swatch::Green),
        NodeKind::Signature => (Swatch::RiskBg, Swatch::Orange),
        NodeKind::Untouched | NodeKind::Outside | NodeKind::More => (Swatch::Chrome, Swatch::Line),
    }
}

fn full_content(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    v: &FullView,
    typed: &str,
    area: AreaTarget,
) {
    p.spawn(Node {
        align_items: AlignItems::Center,
        column_gap: px(10),
        padding: UiRect::new(px(24), px(24), px(20), px(12)),
        ..default()
    })
    .with_children(|h| {
        let (fill, ring) = dot_of(v.kind);
        h.spawn((
            Node {
                width: px(10),
                height: px(10),
                flex_shrink: 0.0,
                border: UiRect::all(px(1.5)),
                border_radius: BorderRadius::all(px(3)),
                ..default()
            },
            BackgroundColor::default(),
            Fill(fill),
            BorderColor::default(),
            Stroke(ring),
        ));
        h.spawn(text(
            fonts,
            v.card.title.clone(),
            Type {
                mono: true,
                ..Type::HEADING
            },
        ));
        h.spawn(text(fonts, v.place.clone(), Type::MONO));
    });
    body_block(p, fonts, &v.card.body, 24.0);
    p.spawn(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(8),
        padding: UiRect::new(px(24), px(24), px(16), px(8)),
        ..default()
    })
    .with_children(|c| {
        c.spawn(text(fonts, "Comment", Type::STRONG));
        c.spawn((
            text_area(fonts, typed, 3.0, EDITOR_AREA),
            EditorArea(pr.clone()),
            area,
        ))
        .insert(Stroke(Swatch::Green));
        if let Some(error) = &v.error {
            c.spawn(text(fonts, error.clone(), Type::BODY.ink(Swatch::Orange)));
        }
        if let Some(reason) = v.blocked {
            c.spawn(text(fonts, reason, Type::META.ink(Swatch::Orange)));
        }
        if let Some(hint) = v.hint {
            c.spawn(text(fonts, hint, Type::META));
        }
    });
    p.spawn((
        panel(
            Node {
                justify_content: JustifyContent::FlexEnd,
                column_gap: px(8),
                padding: UiRect::axes(px(24), px(14)),
                border: UiRect::top(px(1)),
                ..default()
            },
            Swatch::Chrome,
        ),
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|f| {
        f.spawn((
            button(fonts, "Cancel", Variant::Secondary),
            FullCancel(pr.clone()),
            observe(on_cancel),
        ));
        if v.saving {
            f.spawn(disabled_button(fonts, "Saving…"));
        } else if v.blocked.is_some() {
            f.spawn(disabled_button(fonts, "Add to draft"));
        } else {
            f.spawn((
                button(fonts, "Add to draft", Variant::Primary),
                EditorSubmit(pr.clone()),
                observe(on_submit_button),
            ));
        }
    });
}

fn on_cancel(activate: On<Activate>, buttons: Query<&FullCancel>, mut tabs: ResMut<ReviewTabs>) {
    if let Ok(FullCancel(pr)) = buttons.get(activate.entity)
        && let Some(tab) = tabs.0.get_mut(pr)
    {
        close(&mut tab.ui);
    }
}

/// Ends the Full view once its comment is saved (the editor is gone) or its editor was replaced.
pub(super) fn close_when_done(modals: Query<&FullModal>, mut tabs: ResMut<ReviewTabs>) {
    let done: Vec<PrRef> = modals
        .iter()
        .filter(|FullModal(pr)| {
            tabs.0.get(pr).is_some_and(|t| {
                t.ui.modal == Some(Modal::CallsNode)
                    && !matches!(
                        t.ui.editor.as_ref().map(|e| &e.target),
                        Some(EditTarget::CallsNode { .. })
                    )
            })
        })
        .map(|FullModal(pr)| pr.clone())
        .collect();
    for pr in done {
        if let Some(tab) = tabs.0.get_mut(&pr) {
            close(&mut tab.ui);
        }
    }
}

/// Esc and a click outside close it alike, unless the comment holds typed text or is being saved.
pub(super) fn escape_closes(
    keys: Res<ButtonInput<KeyCode>>,
    mut clicks: MessageReader<BackdropClicked>,
    roots: Query<&ModalFor>,
    nav: Res<Nav>,
    mut tabs: ResMut<ReviewTabs>,
) {
    let outside = clicks
        .read()
        .any(|c| roots.get(c.0).is_ok_and(|m| m.modal == Modal::CallsNode));
    if !(escape_pressed(&keys) || outside) {
        return;
    }
    let Screen::Review(pr) = &nav.screen else {
        return;
    };
    // Read first: every Esc on any screen would otherwise mark the tabs changed.
    let closes = tabs.0.get(pr).is_some_and(|tab| {
        tab.ui.modal == Some(Modal::CallsNode)
            && tab
                .ui
                .editor
                .as_ref()
                .is_none_or(|e| e.ticket.is_none() && e.text.trim().is_empty())
    });
    if closes && let Some(tab) = tabs.0.get_mut(pr) {
        close(&mut tab.ui);
    }
}
