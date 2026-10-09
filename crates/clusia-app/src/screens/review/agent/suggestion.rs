//! The suggestion card of the chat: **Accept into draft**, **Edit**, **Dismiss**, and the line it
//! becomes once the daemon has handled it. A suggestion changes nothing until one of the
//! buttons is clicked.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, Button as WidgetButton, observe};
use clusia_core::{PrRef, Side, can_comment};
use clusia_protocol::Suggestion;
use clusia_view::diff::{parse_patch, row_of};

use super::model::{SuggestionState, place_of};
use super::panel::pill;
use crate::bridge::{AgentTell, Ask, Asks, Model, Toasts};
use crate::fonts::UiFonts;
use crate::review_state::{EditTarget, Editor, ReviewSection, ReviewTabs, SHOW_STEP, TabUi};
use crate::screens::review::diff::unsent;
use crate::screens::review::editor::{not_sent, read_only_reason};
use crate::theme::Swatch;
use crate::ui::composer::ComposerMode;
use crate::ui::kit::{
    Clickable, Fill, HoverFill, Stroke, Tone, Type, Variant, badge, button, text,
};
use crate::ui::markdown::parse::parse;
use crate::ui::markdown::{RenderOpts, markdown};

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct SuggestionAccept {
    pub pr: PrRef,
    pub id: String,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct SuggestionEdit {
    pub pr: PrRef,
    pub suggestion: Suggestion,
}

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct SuggestionDismiss {
    pub pr: PrRef,
    pub id: String,
}

/// The `file:line` of a card: it opens the Diff on that file.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct SuggestionPlace {
    pub pr: PrRef,
    pub file: String,
}

/// A suggestion as the transcript shows it for its `state`.
pub(super) fn card(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    s: &Suggestion,
    state: SuggestionState,
    opts: &RenderOpts,
) {
    match state {
        SuggestionState::Accepted => pill(
            p,
            fonts,
            "✓",
            Swatch::Green,
            &format!("Added to your draft · {}", place_of(s)),
            Swatch::Chrome,
            Swatch::Muted,
        ),
        SuggestionState::Dismissed => pill(
            p,
            fonts,
            "×",
            Swatch::Faint,
            &format!("Dismissed · {}", place_of(s)),
            Swatch::Chrome,
            Swatch::Faint,
        ),
        SuggestionState::Waiting => waiting(p, fonts, pr, s, opts),
    }
}

fn waiting(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    s: &Suggestion,
    opts: &RenderOpts,
) {
    p.spawn(crate::ui::kit::card(Node {
        flex_direction: FlexDirection::Column,
        row_gap: px(8),
        padding: px(12).all(),
        ..default()
    }))
    .insert(Stroke(Swatch::Green))
    .with_children(|c| {
        c.spawn(Node {
            column_gap: px(8),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|h| {
            h.spawn(badge(fonts, "Suggested comment", Tone::Green));
            h.spawn((
                Node {
                    padding: UiRect::axes(px(6), px(2)),
                    border_radius: BorderRadius::all(px(4)),
                    ..default()
                },
                (WidgetButton, Clickable),
                Hovered::default(),
                TabIndex(0),
                BackgroundColor::default(),
                Fill(Swatch::Clear),
                HoverFill(Swatch::Hover),
                SuggestionPlace {
                    pr: pr.clone(),
                    file: s.file.clone(),
                },
                observe(on_place),
                children![text(fonts, place_of(s), Type::MONO)],
            ));
        });
        markdown(c, fonts, &parse(&s.body), opts);
        c.spawn(Node {
            column_gap: px(8),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|row| {
            row.spawn((
                button(fonts, "Accept into draft", Variant::Primary),
                SuggestionAccept {
                    pr: pr.clone(),
                    id: s.id.clone(),
                },
                observe(on_accept),
            ));
            row.spawn((
                button(fonts, "Edit", Variant::Secondary),
                SuggestionEdit {
                    pr: pr.clone(),
                    suggestion: s.clone(),
                },
                observe(on_edit),
            ));
            row.spawn((
                button(fonts, "Dismiss", Variant::Ghost),
                SuggestionDismiss {
                    pr: pr.clone(),
                    id: s.id.clone(),
                },
                observe(on_dismiss),
            ));
        });
    });
}

fn on_accept(
    activate: On<Activate>,
    buttons: Query<&SuggestionAccept>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &model, &b.pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    asks.send(Ask::AcceptSuggestion {
        pr: b.pr.clone(),
        id: b.id.clone(),
        body: None,
    });
}

fn on_dismiss(
    activate: On<Activate>,
    buttons: Query<&SuggestionDismiss>,
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
    mut asks: ResMut<Asks>,
) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &model, &b.pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    asks.send(Ask::DismissSuggestion {
        pr: b.pr.clone(),
        id: b.id.clone(),
    });
}

fn on_place(activate: On<Activate>, places: Query<&SuggestionPlace>, mut tabs: ResMut<ReviewTabs>) {
    let Ok(place) = places.get(activate.entity) else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(&place.pr) {
        show_file(&mut tab.ui, &place.file);
    }
}

/// The Diff on `file`; another file starts at its first lines, as from the file list.
fn show_file(ui: &mut TabUi, file: &str) {
    ui.section = ReviewSection::Diff;
    if ui.file.as_deref() != Some(file) {
        ui.file = Some(file.to_string());
        ui.shown = SHOW_STEP;
    }
}

/// Opens the comment composer in the Diff under the suggestion's line, with its body.
fn on_edit(
    activate: On<Activate>,
    buttons: Query<&SuggestionEdit>,
    mut tabs: ResMut<ReviewTabs>,
    model: Res<Model>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let Ok(SuggestionEdit { pr, suggestion: s }) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(reason) = read_only_reason(&tabs, &model, pr) {
        return not_sent(&mut toasts, &time, reason);
    }
    let Some(line) = s.end_line.or(s.line) else {
        return not_sent(
            &mut toasts,
            &time,
            "This suggestion has no line to edit — accept it as it is, or dismiss it",
        );
    };
    let start = s.start_line.filter(|start| *start < line);
    let Some(tab) = tabs.0.get_mut(pr) else {
        return;
    };
    let patch = tab.ready().and_then(|ready| {
        ready
            .view
            .diff
            .iter()
            .find(|f| f.path == s.file)
            .and_then(|f| f.patch.as_deref())
    });
    let in_diff = patch.is_some_and(|patch| {
        can_comment(patch, Side::Right, line)
            && start.is_none_or(|start| can_comment(patch, Side::Right, start))
    });
    // The Diff draws its rows in steps; the composer sits under the row of `line`, so that row
    // (and the ones above it) must be among the drawn ones.
    let row_end = patch.and_then(|patch| row_of(&parse_patch(patch), Side::Right, line));
    if !in_diff {
        return not_sent(
            &mut toasts,
            &time,
            &format!(
                "{} is not in the diff, so it cannot be edited here — accept it as it is, or dismiss it",
                place_of(s)
            ),
        );
    }
    if unsent(tab.ui.editor.as_ref()) {
        return not_sent(
            &mut toasts,
            &time,
            "Finish or cancel your open comment first",
        );
    }
    show_file(&mut tab.ui, &s.file);
    if let Some(row) = row_end {
        tab.ui.shown = tab.ui.shown.max((row + 1).div_ceil(SHOW_STEP) * SHOW_STEP);
    }
    tab.ui.editor = Some(Editor {
        target: EditTarget::Suggestion {
            id: s.id.clone(),
            path: s.file.clone(),
            start,
            line,
        },
        text: s.body.clone(),
        error: None,
        ticket: None,
        mode: ComposerMode::Write,
    });
}

/// An accepted suggestion closes the editor that was editing it (a dismiss, or another
/// suggestion, leaves it alone).
pub(crate) fn close_edited(tabs: &mut ReviewTabs, tell: &AgentTell) {
    let AgentTell::Handled {
        pr,
        id,
        accepted: true,
    } = tell
    else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(pr)
        && matches!(
            tab.ui.editor.as_ref().map(|e| &e.target),
            Some(EditTarget::Suggestion { id: editing, .. }) if editing == id
        )
    {
        tab.ui.editor = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{AgentTell, Connection, Model, Tell, Toasts};
    use crate::fixture;
    use crate::review_state::{EditTarget, ReviewSection, ReviewTabs};
    use crate::screens::review::agent::Chats;
    use crate::screens::review::editor::{EditorArea, EditorSubmit};
    use crate::testing::{self, NOW};
    use clusia_core::PrRef;

    const ID: &str = "sug-0123456789ab";

    fn pr() -> PrRef {
        fixture::demo_pr()
    }

    fn suggestion(line: u32) -> Suggestion {
        Suggestion {
            id: ID.into(),
            file: "src/auth/refresh.rs".into(),
            line: Some(line),
            start_line: None,
            end_line: None,
            body: "Re-check `expires_at` after taking the lock.".into(),
        }
    }

    /// The demo review with its chat open and `s` waiting in it.
    fn app_with(s: Suggestion) -> App {
        let mut app = testing::app(fixture::demo(NOW));
        testing::open_ready(&mut app, false);
        app.world_mut()
            .resource_mut::<Chats>()
            .entry(&pr())
            .show(crate::screens::review::agent::PanelTab::Agent);
        testing::tell(
            &mut app,
            Tell::Agent(AgentTell::Suggestion {
                pr: pr(),
                turn: 1,
                suggestion: s,
            }),
        );
        testing::settle(&mut app);
        testing::recorded(&mut app);
        app
    }

    fn toasts(app: &App) -> Vec<String> {
        app.world()
            .resource::<Toasts>()
            .0
            .iter()
            .map(|t| t.text.clone())
            .collect()
    }

    fn handled(app: &mut App, accepted: bool) {
        testing::tell(
            app,
            Tell::Agent(AgentTell::Handled {
                pr: pr(),
                id: ID.into(),
                accepted,
            }),
        );
        testing::settle(app);
    }

    #[test]
    fn a_waiting_suggestion_has_its_three_actions() {
        let mut app = app_with(suggestion(44));
        for label in [
            "Suggested comment",
            "refresh.rs:44",
            "Accept into draft",
            "Edit",
            "Dismiss",
        ] {
            assert!(testing::shows(&mut app, label), "{label}");
        }
        let accept = testing::find::<SuggestionAccept>(&mut app, |a| a.id == ID);
        testing::activate(&mut app, accept);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::AcceptSuggestion {
                pr: pr(),
                id: ID.into(),
                body: None
            }]
        );
        let dismiss = testing::find::<SuggestionDismiss>(&mut app, |a| a.id == ID);
        testing::activate(&mut app, dismiss);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::DismissSuggestion {
                pr: pr(),
                id: ID.into()
            }]
        );
    }

    #[test]
    fn a_handled_suggestion_becomes_one_line() {
        let mut app = app_with(suggestion(44));
        handled(&mut app, true);
        assert!(testing::shows(
            &mut app,
            "Added to your draft · refresh.rs:44"
        ));
        assert_eq!(testing::count::<SuggestionAccept>(&mut app), 0);

        let mut app = app_with(suggestion(44));
        handled(&mut app, false);
        assert!(testing::shows(&mut app, "Dismissed · refresh.rs:44"));
        assert_eq!(testing::count::<SuggestionDismiss>(&mut app), 0);
    }

    #[test]
    fn nothing_is_sent_when_the_review_cannot_change() {
        let mut app = app_with(suggestion(44));
        app.world_mut().resource_mut::<Model>().connection = Connection::Lost("gone".into());
        let accept = testing::find::<SuggestionAccept>(&mut app, |_| true);
        testing::activate(&mut app, accept);
        let dismiss = testing::find::<SuggestionDismiss>(&mut app, |_| true);
        testing::activate(&mut app, dismiss);
        let edit = testing::find::<SuggestionEdit>(&mut app, |_| true);
        testing::activate(&mut app, edit);
        assert!(testing::recorded(&mut app).is_empty());
        assert_eq!(toasts(&app).len(), 3, "each click says why");
        assert!(
            toasts(&app)[0].contains("Not connected"),
            "{:?}",
            toasts(&app)
        );
    }

    #[test]
    fn the_place_opens_the_diff_on_its_file() {
        let mut app = app_with(suggestion(44));
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            let ui = &mut tabs.0.get_mut(&pr()).unwrap().ui;
            ui.section = ReviewSection::Comments;
            ui.file = Some("tests/refresh.rs".into());
        }
        let place = testing::find::<SuggestionPlace>(&mut app, |_| true);
        testing::activate(&mut app, place);
        let tab = testing::tab(&app, &pr());
        assert_eq!(tab.ui.section, ReviewSection::Diff);
        assert_eq!(tab.ui.file.as_deref(), Some("src/auth/refresh.rs"));
    }

    #[test]
    fn edit_opens_the_composer_in_the_diff_and_accepts_the_edited_text() {
        let mut app = app_with(suggestion(44));
        let edit = testing::find::<SuggestionEdit>(&mut app, |_| true);
        testing::activate(&mut app, edit);
        testing::settle(&mut app);
        let tab = testing::tab(&app, &pr());
        assert_eq!(tab.ui.section, ReviewSection::Diff);
        assert_eq!(tab.ui.file.as_deref(), Some("src/auth/refresh.rs"));
        let editor = tab.ui.editor.expect("an open editor");
        assert_eq!(
            editor.target,
            EditTarget::Suggestion {
                id: ID.into(),
                path: "src/auth/refresh.rs".into(),
                start: None,
                line: 44
            }
        );
        assert_eq!(editor.text, "Re-check `expires_at` after taking the lock.");
        assert_eq!(
            testing::count::<EditorArea>(&mut app),
            1,
            "drawn under the diff line"
        );

        let area = testing::find::<EditorArea>(&mut app, |_| true);
        testing::type_into(&mut app, area, " Thanks.");
        let submit = testing::find::<EditorSubmit>(&mut app, |_| true);
        testing::activate(&mut app, submit);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::AcceptSuggestion {
                pr: pr(),
                id: ID.into(),
                body: Some("Re-check `expires_at` after taking the lock. Thanks.".into())
            }]
        );
        let editor = testing::tab(&app, &pr())
            .ui
            .editor
            .expect("still open until the daemon answers");
        assert_eq!(editor.ticket, None, "the button is not stuck on Saving…");

        handled(&mut app, true);
        assert!(
            testing::tab(&app, &pr()).ui.editor.is_none(),
            "the editor closes"
        );
        assert!(testing::shows(&mut app, "Added to your draft"));
    }

    #[test]
    fn editing_a_suggestion_deep_in_a_long_diff_shows_its_row() {
        let mut s = suggestion(1_203);
        s.file = "src/big.rs".into();
        let mut app = app_with(s);
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            let tab = tabs.0.get_mut(&pr()).unwrap();
            let crate::review_state::Phase::Ready(ready) = &mut tab.phase else {
                panic!("ready")
            };
            let patch: String = std::iter::once("@@ -1,1300 +1,1300 @@\n".to_string())
                .chain((1..=1_300).map(|n| format!(" line {n}\n")))
                .collect();
            let mut file = ready.view.diff[0].clone();
            file.path = "src/big.rs".into();
            file.patch = Some(patch);
            ready.view.diff.push(file);
        }
        let edit = testing::find::<SuggestionEdit>(&mut app, |_| true);
        testing::activate(&mut app, edit);
        let tab = testing::tab(&app, &pr());
        assert!(tab.ui.editor.is_some());
        assert_eq!(
            tab.ui.shown,
            3 * SHOW_STEP,
            "row 1203 is drawn, so the composer under it is"
        );
    }

    #[test]
    fn a_suggestion_outside_the_diff_cannot_be_edited() {
        let mut app = app_with(suggestion(900));
        let edit = testing::find::<SuggestionEdit>(&mut app, |_| true);
        testing::activate(&mut app, edit);
        assert!(testing::tab(&app, &pr()).ui.editor.is_none());
        let shown = toasts(&app);
        assert_eq!(shown.len(), 1);
        assert!(
            shown[0].contains("refresh.rs:900 is not in the diff"),
            "{shown:?}"
        );
    }

    #[test]
    fn a_range_is_edited_as_a_range_and_an_open_comment_blocks_the_edit() {
        let mut s = suggestion(0);
        s.line = None;
        s.start_line = Some(41);
        s.end_line = Some(44);
        let mut app = app_with(s);
        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            tabs.0.get_mut(&pr()).unwrap().ui.editor = Some(crate::review_state::Editor {
                target: EditTarget::General,
                text: "half a note".into(),
                error: None,
                ticket: None,
                mode: crate::ui::composer::ComposerMode::Write,
            });
        }
        let edit = testing::find::<SuggestionEdit>(&mut app, |_| true);
        testing::activate(&mut app, edit);
        assert_eq!(
            testing::tab(&app, &pr()).ui.editor.unwrap().target,
            EditTarget::General,
            "the open note stays"
        );
        assert!(
            toasts(&app)[0].contains("Finish or cancel"),
            "{:?}",
            toasts(&app)
        );

        {
            let mut tabs = app.world_mut().resource_mut::<ReviewTabs>();
            tabs.0.get_mut(&pr()).unwrap().ui.editor = None;
        }
        testing::activate(&mut app, edit);
        assert_eq!(
            testing::tab(&app, &pr()).ui.editor.unwrap().target,
            EditTarget::Suggestion {
                id: ID.into(),
                path: "src/auth/refresh.rs".into(),
                start: Some(41),
                line: 44
            }
        );
    }

    #[test]
    fn the_edited_text_is_dropped_only_by_an_accept_of_the_same_suggestion() {
        let mut app = app_with(suggestion(44));
        let edit = testing::find::<SuggestionEdit>(&mut app, |_| true);
        testing::activate(&mut app, edit);
        testing::tell(
            &mut app,
            Tell::Agent(AgentTell::Handled {
                pr: pr(),
                id: "sug-other0000000".into(),
                accepted: true,
            }),
        );
        assert!(testing::tab(&app, &pr()).ui.editor.is_some());
        testing::tell(
            &mut app,
            Tell::Agent(AgentTell::Handled {
                pr: pr(),
                id: ID.into(),
                accepted: false,
            }),
        );
        assert!(
            testing::tab(&app, &pr()).ui.editor.is_some(),
            "a dismiss does not close the edit"
        );
    }
}
