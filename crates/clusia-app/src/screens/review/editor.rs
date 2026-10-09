//! The comment editor shared by every place that writes into the draft: a line in the diff, a
//! reply in Comments, a general note in the right panel, or an existing item being edited.
//!
//! `editor_box` draws it from `TabUi.editor`; `submit_editor` is the one path from a submit
//! (⌘↵ or **Add to draft**) to the right `Ask`. The daemon's answer comes back through the
//! bridge: `Tell::Saved` closes the editor, `Tell::Refused` keeps the text and shows why.
//! `sync_editor_text` copies what was typed into `TabUi.editor.text` at the start of every
//! frame (before any region rebuilds), so a redrawn editor keeps it; the region views ignore
//! the text, so typing rebuilds nothing.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::{FocusCause, FocusLost, InputFocus};
use bevy::prelude::*;
use bevy::text::EditableText;
use bevy::ui_widgets::{Activate, observe};
use clusia_core::{DraftKind, PrRef, Side};
use clusia_protocol::AnchorInput;

use crate::bridge::{Ask, Asks, Connection, Model, TOAST_SECS, Toast, Toasts};
use crate::fonts::UiFonts;
use crate::review_state::{EditTarget, Editor, ReviewTabs, Tab, Tickets};
use crate::screens::review::ReviewSystems;
use crate::theme::Swatch;
use crate::ui::composer::{AreaSize, ComposerKey, ComposerView, Slot, composer};
use crate::ui::kit::{Stroke, Type, Variant, button, card, disabled_button, text};
use crate::ui::text_area::TextSubmitted;

/// The `TextArea::id` of every comment editor.
pub const EDITOR_AREA: u64 = 1;

/// The text area of `pr`'s editor.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct EditorArea(pub PrRef);

/// **Add to draft** (or **Save** when editing an item).
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct EditorSubmit(pub PrRef);

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct EditorCancel(pub PrRef);

/// **Ask the agent about this line** in a line comment's editor.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct AskAgentButton(pub PrRef);

/// What the chat input starts with when the user asks about a line (or a range).
pub fn ask_prompt(path: &str, start: Option<u32>, line: u32) -> String {
    match start.filter(|s| *s < line) {
        Some(start) => format!("About `{path}:{start}-{line}`: "),
        None => format!("About `{path}:{line}`: "),
    }
}

/// What the text area was drawn for. Its text is copied back only into an editor with the same
/// target, so opening another editor (an item's body, say) is not overwritten by the old area.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub(crate) struct AreaTarget(EditTarget);

/// Draws `editor` for `pr`: the composer, the daemon's refusal (orange) and the buttons, in one
/// green frame.
pub fn editor_box(p: &mut ChildSpawnerCommands, fonts: &UiFonts, editor: &Editor, pr: &PrRef) {
    let key = ComposerKey(pr.clone(), Slot::Edit(editor.target.clone()));
    let view = ComposerView {
        text: &editor.text,
        mode: editor.mode,
        id: EDITOR_AREA,
        size: AreaSize::Grow {
            min: 4.0,
            max: 10.0,
        },
        compact: false,
        suggest: matches!(
            editor.target,
            EditTarget::Line {
                side: Side::Right,
                ..
            }
        ),
        flat: true,
    };
    p.spawn(card(Node {
        flex_direction: FlexDirection::Column,
        ..default()
    }))
    .insert(Stroke(Swatch::Green))
    .with_children(|c| {
        composer(
            c,
            fonts,
            &view,
            key,
            (EditorArea(pr.clone()), AreaTarget(editor.target.clone())),
        );
        c.spawn(Node {
            flex_direction: FlexDirection::Column,
            row_gap: px(8),
            padding: UiRect::axes(px(10), px(8)),
            ..default()
        })
        .with_children(|f| {
            if let Some(error) = &editor.error {
                f.spawn(text(fonts, error.clone(), Type::BODY.ink(Swatch::Orange)));
            }
            f.spawn(Node {
                column_gap: px(8),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|row| {
                row.spawn(text(fonts, "⌘↵ to add", Type::META));
                row.spawn(Node {
                    flex_grow: 1.0,
                    ..default()
                });
                if matches!(editor.target, EditTarget::Line { .. }) {
                    row.spawn((
                        button(fonts, "Ask the agent about this line", Variant::Ghost),
                        AskAgentButton(pr.clone()),
                        observe(on_ask_agent),
                    ));
                }
                row.spawn((
                    button(fonts, "Cancel", Variant::Ghost),
                    EditorCancel(pr.clone()),
                    observe(on_cancel),
                ));
                let label = match editor.target {
                    EditTarget::Item(_) => "Save",
                    EditTarget::Suggestion { .. } => "Accept into draft",
                    _ => "Add to draft",
                };
                if editor.ticket.is_some() {
                    row.spawn(disabled_button(fonts, "Saving…"));
                } else {
                    row.spawn((
                        button(fonts, label, Variant::Primary),
                        EditorSubmit(pr.clone()),
                        observe(on_submit_button),
                    ));
                }
            });
        });
    });
}

/// Sends `text` for the tab's open editor. Returns whether an `Ask` went out.
///
/// Nothing is sent while an earlier submit waits for the daemon, for empty text (the editor
/// says so) or without a live connection (the editor says it was not sent).
pub fn submit_editor(
    tab: &mut Tab,
    pr: &PrRef,
    text: &str,
    live: bool,
    tickets: &mut Tickets,
    asks: &mut Asks,
) -> bool {
    let Some(editor) = tab.ui.editor.as_mut() else {
        return false;
    };
    if editor.ticket.is_some() {
        return false;
    }
    editor.text = text.to_string();
    let body = text.trim().to_string();
    if body.is_empty() {
        editor.error = Some("The comment is empty".into());
        return false;
    }
    if !live {
        editor.error = Some(NOT_CONNECTED.into());
        return false;
    }
    let ticket = tickets.issue();
    editor.error = None;
    editor.ticket = Some(ticket);
    let pr = pr.clone();
    let ask = match &editor.target {
        EditTarget::Line {
            path,
            side,
            start,
            line,
        } => Ask::AddItem {
            pr,
            kind: DraftKind::LineComment,
            anchor: Some(AnchorInput {
                path: path.clone(),
                line: *line,
                start_line: *start,
                side: *side,
            }),
            thread: None,
            body,
            ticket,
        },
        EditTarget::Reply(thread) => Ask::AddItem {
            pr,
            kind: DraftKind::Reply,
            anchor: None,
            thread: Some(thread.clone()),
            body,
            ticket,
        },
        EditTarget::General => Ask::AddItem {
            pr,
            kind: DraftKind::General,
            anchor: None,
            thread: None,
            body,
            ticket,
        },
        EditTarget::Item(id) => Ask::UpdateItem {
            pr,
            id: id.clone(),
            body,
            ticket,
        },
        EditTarget::Suggestion { id, .. } => {
            // The daemon answers an accept with `Handled`, not with this ticket: the editor
            // stays (with its text) until then, and its button is not left on "Saving…".
            editor.ticket = None;
            Ask::AcceptSuggestion {
                pr,
                id: id.clone(),
                body: Some(body),
            }
        }
    };
    asks.send(ask);
    true
}

pub struct EditorPlugin;

impl Plugin for EditorPlugin {
    fn build(&self, app: &mut App) {
        app.add_observer(keep_text_on_blur).add_systems(
            Update,
            (submit_on_cmd_enter, focus_new_editor).in_set(ReviewSystems),
        );
    }
}

/// Why nothing is sent while the tab shows the cached copy.
pub const CACHED_COPY: &str = "You're viewing the cached copy — try again to make changes";

/// Why nothing is sent without a live connection.
pub const NOT_CONNECTED: &str = "Not connected to clusiad — not sent";

/// Why no draft change can be sent for `pr` right now (the cached copy, or no live
/// connection); `None` when it can.
pub(crate) fn read_only_reason(
    tabs: &ReviewTabs,
    model: &Model,
    pr: &PrRef,
) -> Option<&'static str> {
    let cached = tabs
        .0
        .get(pr)
        .and_then(Tab::ready)
        .is_some_and(|r| r.cached_at.is_some());
    if cached {
        Some(CACHED_COPY)
    } else if model.connection != Connection::Live {
        Some(NOT_CONNECTED)
    } else {
        None
    }
}

/// A button that would change the draft on a read-only review: says why nothing was sent.
pub(crate) fn not_sent(toasts: &mut Toasts, time: &Time, reason: &str) {
    toasts.0.push(Toast {
        text: reason.to_string(),
        warning: true,
        until: time.elapsed_secs_f64() + TOAST_SECS,
    });
}

fn submit(
    pr: &PrRef,
    text: &str,
    tabs: &mut ReviewTabs,
    model: &Model,
    tickets: &mut Tickets,
    asks: &mut Asks,
) {
    if let Some(tab) = tabs.0.get_mut(pr) {
        let live = model.connection == Connection::Live;
        let cached = tab.ready().is_some_and(|r| r.cached_at.is_some());
        if cached
            && let Some(editor) = tab.ui.editor.as_mut()
            && editor.ticket.is_none()
        {
            editor.text = text.to_string();
            editor.error = Some(CACHED_COPY.into());
            return;
        }
        submit_editor(tab, pr, text, live && !cached, tickets, asks);
    }
}

fn submit_on_cmd_enter(
    mut submitted: MessageReader<TextSubmitted>,
    areas: Query<&EditorArea>,
    mut tabs: ResMut<ReviewTabs>,
    model: Res<Model>,
    mut tickets: ResMut<Tickets>,
    mut asks: ResMut<Asks>,
) {
    for s in submitted.read() {
        if let Ok(EditorArea(pr)) = areas.get(s.entity) {
            submit(pr, &s.value, &mut tabs, &model, &mut tickets, &mut asks);
        }
    }
}

fn on_submit_button(
    activate: On<Activate>,
    buttons: Query<&EditorSubmit>,
    areas: Query<(&EditorArea, &EditableText)>,
    mut tabs: ResMut<ReviewTabs>,
    model: Res<Model>,
    mut tickets: ResMut<Tickets>,
    mut asks: ResMut<Asks>,
) {
    let Ok(EditorSubmit(pr)) = buttons.get(activate.entity) else {
        return;
    };
    if let Some((_, editable)) = areas.iter().find(|(a, _)| &a.0 == pr) {
        let value = editable.value().to_string();
        submit(pr, &value, &mut tabs, &model, &mut tickets, &mut asks);
    }
}

fn on_cancel(activate: On<Activate>, buttons: Query<&EditorCancel>, mut tabs: ResMut<ReviewTabs>) {
    if let Ok(EditorCancel(pr)) = buttons.get(activate.entity)
        && let Some(tab) = tabs.0.get_mut(pr)
    {
        tab.ui.editor = None;
    }
}

/// Puts a question about the editor's line in the chat input, on the Agent tab. Nothing is
/// sent; an editor with nothing typed in it closes.
fn on_ask_agent(
    activate: On<Activate>,
    buttons: Query<&AskAgentButton>,
    mut tabs: ResMut<ReviewTabs>,
    mut chats: ResMut<crate::screens::review::agent::Chats>,
) {
    let Ok(AskAgentButton(pr)) = buttons.get(activate.entity) else {
        return;
    };
    let Some(tab) = tabs.0.get_mut(pr) else {
        return;
    };
    let Some(editor) = tab.ui.editor.as_ref() else {
        return;
    };
    let EditTarget::Line {
        path, start, line, ..
    } = &editor.target
    else {
        return;
    };
    let prompt = ask_prompt(path, *start, *line);
    let empty = editor.text.trim().is_empty();
    chats.entry(pr).ask_about(prompt);
    if empty {
        tab.ui.editor = None;
    }
}

/// Copies the live text of each editor into its tab, so a region that rebuilds (and despawns
/// the text area before any `FocusLost` could read it) draws it again with what was typed.
/// Writes only when the text differs, so `ReviewTabs` is not changed every frame.
pub(crate) fn sync_editor_text(
    areas: Query<(&EditorArea, &AreaTarget, &EditableText)>,
    mut tabs: ResMut<ReviewTabs>,
) {
    for (EditorArea(pr), AreaTarget(target), editable) in &areas {
        let value = editable.value().to_string();
        let differs = tabs
            .0
            .get(pr)
            .and_then(|t| t.ui.editor.as_ref())
            .is_some_and(|e| e.target == *target && e.text != value);
        if differs && let Some(editor) = tabs.0.get_mut(pr).and_then(|t| t.ui.editor.as_mut()) {
            editor.text = value;
        }
    }
}

/// Remembers what was typed when the text area loses focus, so a rebuild keeps it.
fn keep_text_on_blur(
    lost: On<FocusLost>,
    areas: Query<(&EditorArea, &AreaTarget, &EditableText)>,
    mut tabs: ResMut<ReviewTabs>,
) {
    let Ok((EditorArea(pr), AreaTarget(target), editable)) = areas.get(lost.entity) else {
        return;
    };
    let value = editable.value().to_string();
    let stale = tabs
        .0
        .get(pr)
        .and_then(|t| t.ui.editor.as_ref())
        .is_none_or(|e| e.target != *target || e.text == value);
    if !stale && let Some(editor) = tabs.0.get_mut(pr).and_then(|t| t.ui.editor.as_mut()) {
        editor.text = value;
    }
}

/// A freshly drawn editor takes the keyboard, unless it is drawn hidden (in Preview), where
/// keys would edit text nobody sees.
fn focus_new_editor(
    areas: Query<(Entity, &Node), Added<EditorArea>>,
    mut focus: ResMut<InputFocus>,
) {
    let shown = areas.iter().filter(|(_, n)| n.display != Display::None);
    if let Some((entity, _)) = shown.last() {
        focus.set(entity, FocusCause::Navigated);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::review_state::ReviewSection;
    use crate::testing::{self, NOW};
    use crate::ui::composer::ComposerMode;
    use bevy::input::ButtonInput;
    use clusia_core::{Side, ThreadRef};

    fn tab_with(target: EditTarget) -> Tab {
        let mut tab = Tab {
            phase: crate::review_state::Phase::Loading {
                steps: vec![],
                cached: None,
            },
            ui: Default::default(),
        };
        tab.ui.editor = Some(Editor {
            target,
            text: String::new(),
            error: None,
            ticket: None,
            mode: ComposerMode::Write,
        });
        tab
    }

    fn pr() -> PrRef {
        "rzorzal/clusia#123".parse().unwrap()
    }

    #[test]
    fn every_target_makes_its_ask() {
        let thread = ThreadRef {
            id: "PRRT_1".into(),
            author: "mona".into(),
            path: Some("src/auth/refresh.rs".into()),
            line: Some(41),
        };
        let cases = [
            (
                EditTarget::Line {
                    path: "src/auth/refresh.rs".into(),
                    side: Side::Right,
                    start: Some(40),
                    line: 44,
                },
                Ask::AddItem {
                    pr: pr(),
                    kind: DraftKind::LineComment,
                    anchor: Some(AnchorInput {
                        path: "src/auth/refresh.rs".into(),
                        line: 44,
                        start_line: Some(40),
                        side: Side::Right,
                    }),
                    thread: None,
                    body: "Hold the lock?".into(),
                    ticket: 1,
                },
            ),
            (
                EditTarget::Reply(thread.clone()),
                Ask::AddItem {
                    pr: pr(),
                    kind: DraftKind::Reply,
                    anchor: None,
                    thread: Some(thread),
                    body: "Hold the lock?".into(),
                    ticket: 2,
                },
            ),
            (
                EditTarget::General,
                Ask::AddItem {
                    pr: pr(),
                    kind: DraftKind::General,
                    anchor: None,
                    thread: None,
                    body: "Hold the lock?".into(),
                    ticket: 3,
                },
            ),
            (
                EditTarget::Item("i2".into()),
                Ask::UpdateItem {
                    pr: pr(),
                    id: "i2".into(),
                    body: "Hold the lock?".into(),
                    ticket: 4,
                },
            ),
        ];
        let mut tickets = Tickets::default();
        for (target, want) in cases {
            let mut tab = tab_with(target);
            let mut asks = Asks::default();
            assert!(submit_editor(
                &mut tab,
                &pr(),
                " Hold the lock?\n",
                true,
                &mut tickets,
                &mut asks
            ));
            assert_eq!(asks.recorded, std::slice::from_ref(&want));
            let editor = tab.ui.editor.as_ref().unwrap();
            assert_eq!(editor.text, " Hold the lock?\n", "kept as typed");
            let (Ask::AddItem { ticket, .. } | Ask::UpdateItem { ticket, .. }) = want else {
                unreachable!()
            };
            assert_eq!(editor.ticket, Some(ticket));
            assert!(
                !submit_editor(&mut tab, &pr(), "again", true, &mut tickets, &mut asks),
                "one submit at a time"
            );
            assert_eq!(asks.recorded.len(), 1);
        }
    }

    #[test]
    fn empty_or_offline_submits_explain_and_keep_the_text() {
        let mut tickets = Tickets::default();
        let mut asks = Asks::default();
        let mut tab = tab_with(EditTarget::General);
        assert!(!submit_editor(
            &mut tab,
            &pr(),
            "  \n",
            true,
            &mut tickets,
            &mut asks
        ));
        assert_eq!(
            tab.ui.editor.as_ref().unwrap().error.as_deref(),
            Some("The comment is empty")
        );
        assert!(!submit_editor(
            &mut tab,
            &pr(),
            "Nice",
            false,
            &mut tickets,
            &mut asks
        ));
        let editor = tab.ui.editor.as_ref().unwrap();
        assert_eq!(
            editor.error.as_deref(),
            Some("Not connected to clusiad — not sent")
        );
        assert_eq!((editor.text.as_str(), editor.ticket), ("Nice", None));
        assert!(asks.recorded.is_empty());
        let mut closed = tab_with(EditTarget::General);
        closed.ui.editor = None;
        assert!(!submit_editor(
            &mut closed,
            &pr(),
            "x",
            true,
            &mut tickets,
            &mut asks
        ));
    }

    fn open_general_note(app: &mut App) -> PrRef {
        let pr = testing::open_ready(app, false);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .editor = Some(Editor {
            target: EditTarget::General,
            text: "Ask about ".into(),
            error: None,
            ticket: None,
            mode: ComposerMode::Write,
        });
        testing::settle(app);
        pr
    }

    #[test]
    fn cmd_enter_and_the_button_submit_then_saved_closes() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_general_note(&mut app);
        let area = testing::find::<EditorArea>(&mut app, |_| true);
        assert_eq!(
            app.world().resource::<InputFocus>().get(),
            Some(area),
            "a new editor takes the keyboard"
        );
        testing::type_into(&mut app, area, "the token cache size");
        {
            let mut keys = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            keys.press(KeyCode::SuperLeft);
            keys.press(KeyCode::Enter);
        }
        app.update();
        app.world_mut()
            .resource_mut::<ButtonInput<KeyCode>>()
            .reset_all();
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::AddItem {
                pr: pr.clone(),
                kind: DraftKind::General,
                anchor: None,
                thread: None,
                body: "Ask about the token cache size".into(),
                ticket: 1
            }]
        );
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<EditorSubmit>(&mut app),
            0,
            "Saving… while it waits"
        );
        testing::tell(
            &mut app,
            crate::bridge::Tell::Refused {
                pr: pr.clone(),
                ticket: 1,
                message: "the comment is empty".into(),
            },
        );
        testing::settle(&mut app);
        let area = testing::find::<EditorArea>(&mut app, |_| true);
        assert_eq!(
            app.world()
                .get::<EditableText>(area)
                .unwrap()
                .value()
                .to_string(),
            "Ask about the token cache size",
            "a refusal keeps the text"
        );
        let has_error = {
            let mut q = app.world_mut().query::<&Text>();
            q.iter(app.world()).any(|t| t.0 == "the comment is empty")
        };
        assert!(has_error);
        let add = testing::find::<EditorSubmit>(&mut app, |_| true);
        testing::activate(&mut app, add);
        assert!(matches!(
            &testing::recorded(&mut app)[..],
            [Ask::AddItem { ticket: 2, .. }]
        ));
        testing::tell(
            &mut app,
            crate::bridge::Tell::Saved {
                pr: pr.clone(),
                ticket: 2,
            },
        );
        testing::settle(&mut app);
        assert_eq!(testing::count::<EditorArea>(&mut app), 0);
        assert!(
            app.world().resource::<ReviewTabs>().0[&pr]
                .ui
                .editor
                .is_none()
        );
    }

    #[test]
    fn a_rebuild_keeps_what_was_typed() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_general_note(&mut app);
        let area = testing::find::<EditorArea>(&mut app, |_| true);
        testing::type_into(&mut app, area, "the token cache");
        let value = |app: &mut App| {
            let area = testing::find::<EditorArea>(app, |_| true);
            let text = app
                .world()
                .get::<EditableText>(area)
                .unwrap()
                .value()
                .to_string();
            (area, text)
        };
        for connection in [
            Connection::Lost("clusiad closed the connection".into()),
            Connection::Live,
        ] {
            let before = testing::find::<EditorArea>(&mut app, |_| true);
            app.world_mut().resource_mut::<Model>().connection = connection;
            testing::settle(&mut app);
            let (after, text) = value(&mut app);
            assert_ne!(before, after, "the right panel was redrawn");
            assert_eq!(text, "Ask about the token cache");
        }
        assert_eq!(
            app.world().resource::<ReviewTabs>().0[&pr]
                .ui
                .editor
                .as_ref()
                .map(|e| e.text.as_str()),
            Some("Ask about the token cache")
        );
    }

    #[test]
    fn the_cached_copy_takes_no_comments() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_general_note(&mut app);
        if let Some(ready) = app
            .world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .and_then(Tab::ready_mut)
        {
            ready.cached_at = Some(NOW - 3600);
        }
        testing::settle(&mut app);
        let area = testing::find::<EditorArea>(&mut app, |_| true);
        testing::type_into(&mut app, area, "retries");
        let add = testing::find::<EditorSubmit>(&mut app, |_| true);
        testing::activate(&mut app, add);
        assert!(testing::recorded(&mut app).is_empty());
        let editor = app.world().resource::<ReviewTabs>().0[&pr]
            .ui
            .editor
            .clone()
            .unwrap();
        assert_eq!(
            (editor.text.as_str(), editor.error.as_deref(), editor.ticket),
            (
                "Ask about retries",
                Some("You're viewing the cached copy — try again to make changes"),
                None
            )
        );
    }

    #[test]
    fn cancel_closes_and_blur_keeps_the_text() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_general_note(&mut app);
        let area = testing::find::<EditorArea>(&mut app, |_| true);
        testing::type_into(&mut app, area, "retries");
        app.world_mut().trigger(FocusLost { entity: area });
        app.update();
        let text = |app: &App| {
            app.world().resource::<ReviewTabs>().0[&pr]
                .ui
                .editor
                .as_ref()
                .map(|e| e.text.clone())
        };
        assert_eq!(text(&app).as_deref(), Some("Ask about retries"));
        // Switching sections rebuilds nothing in the right panel; the text survives anyway.
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .section = ReviewSection::Audits;
        testing::settle(&mut app);
        let cancel = testing::find::<EditorCancel>(&mut app, |_| true);
        testing::activate(&mut app, cancel);
        assert_eq!(text(&app), None);
        assert!(testing::recorded(&mut app).is_empty());
    }

    #[test]
    fn the_ask_prompt_names_the_place() {
        assert_eq!(
            ask_prompt("src/auth/refresh.rs", None, 44),
            "About `src/auth/refresh.rs:44`: "
        );
        assert_eq!(
            ask_prompt("src/auth/refresh.rs", Some(40), 44),
            "About `src/auth/refresh.rs:40-44`: "
        );
        assert_eq!(
            ask_prompt("src/a.rs", Some(7), 7),
            "About `src/a.rs:7`: ",
            "a one-line range is one line"
        );
    }

    #[test]
    fn asking_about_a_line_prefills_the_chat_and_keeps_typed_comments() {
        use crate::screens::review::agent::{ChatInput, Chats};
        use bevy::text::EditableText;
        let mut app = testing::app(crate::fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        let line = |text: &str| Editor {
            target: EditTarget::Line {
                path: "src/auth/refresh.rs".into(),
                side: Side::Right,
                start: None,
                line: 44,
            },
            text: text.into(),
            error: None,
            ticket: None,
            mode: crate::ui::composer::ComposerMode::Write,
        };
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .editor = Some(line(""));
        testing::settle(&mut app);
        let ask = testing::find::<AskAgentButton>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, ask);
        testing::settle(&mut app);
        let input = testing::find::<ChatInput>(&mut app, |i| i.0 == pr);
        assert_eq!(
            app.world()
                .get::<EditableText>(input)
                .unwrap()
                .value()
                .to_string(),
            "About `src/auth/refresh.rs:44`: "
        );
        assert_eq!(
            app.world().resource::<Chats>().0[&pr].tab,
            crate::screens::review::agent::PanelTab::Agent,
            "the Agent tab is selected"
        );
        assert!(
            testing::tab(&app, &pr).ui.editor.is_none(),
            "an empty editor closes"
        );
        assert!(testing::recorded(&mut app).is_empty(), "nothing is sent");

        // With a comment typed, the editor stays; a question already typed is kept.
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .editor = Some(line("rename this"));
        testing::settle(&mut app);
        let ask = testing::find::<AskAgentButton>(&mut app, |b| b.0 == pr);
        testing::activate(&mut app, ask);
        testing::settle(&mut app);
        assert!(testing::tab(&app, &pr).ui.editor.is_some());
        assert_eq!(
            app.world()
                .get::<EditableText>(input)
                .unwrap()
                .value()
                .to_string(),
            "About `src/auth/refresh.rs:44`: \nAbout `src/auth/refresh.rs:44`: "
        );
    }

    #[test]
    fn only_line_comments_offer_to_ask_the_agent() {
        let mut app = testing::app(crate::fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        let editor = |target| Editor {
            target,
            text: String::new(),
            error: None,
            ticket: None,
            mode: crate::ui::composer::ComposerMode::Write,
        };
        for target in [EditTarget::General, EditTarget::Item("i1".into())] {
            app.world_mut()
                .resource_mut::<ReviewTabs>()
                .0
                .get_mut(&pr)
                .unwrap()
                .ui
                .editor = Some(editor(target));
            testing::settle(&mut app);
            assert_eq!(testing::count::<AskAgentButton>(&mut app), 0);
        }
    }
}
