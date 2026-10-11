//! The Security tab (mockup `Security.png`): the checker's findings as cards, each one a
//! proposal the human accepts, edits or dismisses.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::ui_widgets::ScrollArea;
use clusia_core::PrRef;
use clusia_core::checks::{CheckKind, Finding, Severity};
use clusia_core::printable::{printable, printable_lines};

use super::{
    CheckAction, ChecksModel, check_button, dismiss_button, finding_buttons, finding_editor,
    notice_of, place_of, proposed_comment, setup_button, show_button,
};
use crate::bridge::Model;
use crate::clock::Clock;
use crate::fonts::UiFonts;
use crate::review_state::{Editor, Phase, ReviewSection, ReviewTabs};
use crate::screens::home::long_age;
use crate::screens::review::agent::harness_ready;
use crate::screens::review::editor::editor_box;
use crate::screens::review::shell::SectionBody;
use crate::theme::Swatch;
use crate::ui::kit::{Stroke, Tone, Type, Variant, badge, card, panel, text};

/// One finding as its card shows it. Every text is already printable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardView {
    pub id: String,
    /// `HIGH`, `MEDIUM`, `LOW`
    pub severity: String,
    pub tone: Tone,
    pub title: String,
    /// `src/client/http.rs:52`
    pub place: String,
    pub body: String,
    pub code: Option<String>,
    /// The comment **Add to draft** will add, as the card shows it.
    pub comment: String,
    /// The file, when it is one the pull request changes (**Show in diff**).
    pub show: Option<String>,
    pub accepted: bool,
    /// An accept or dismiss waits for the daemon.
    pub busy: bool,
    /// What waits is a dismiss.
    pub dismissing: bool,
    /// No place in the diff: it joins the draft as a general comment.
    pub general: bool,
    /// The result is of an older head: only **Dismiss** and **Show in diff** are offered.
    pub stale: bool,
    /// The card's editor is open.
    pub editing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityView {
    pub title: String,
    pub line: String,
    pub chips: Vec<(String, Tone)>,
    pub action: Option<CheckAction>,
    /// Offer **Set up a harness**.
    pub setup: bool,
    pub cards: Vec<CardView>,
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

fn severity_of(f: &Finding) -> (&'static str, Tone) {
    match &f.severity {
        Some(Severity::High) => ("HIGH", Tone::Orange),
        Some(Severity::Medium) => ("MEDIUM", Tone::Neutral),
        Some(Severity::Low) => ("LOW", Tone::Neutral),
        None => ("", Tone::Neutral),
    }
}

/// What the Security tab shows. `files` are the pull request's changed paths; `editing` is the
/// id of the finding whose editor is open; `offline` says the tab is a cached copy.
pub fn security_view(
    model: Option<&ChecksModel>,
    harness: bool,
    files: &[&str],
    now: i64,
    editing: Option<&str>,
    offline: bool,
) -> SecurityView {
    let default = ChecksModel::default();
    let m = model.unwrap_or(&default);
    let k = m.kind(CheckKind::Security);
    if let Some(n) = notice_of(CheckKind::Security, k, harness, m.loaded, offline) {
        return SecurityView {
            title: n.title,
            line: n.line,
            chips: Vec::new(),
            action: n.action,
            setup: n.setup,
            cards: Vec::new(),
        };
    }
    let Some(result) = &k.result else {
        return SecurityView {
            title: "Security check".into(),
            line: String::new(),
            chips: Vec::new(),
            action: Some(CheckAction::Run),
            setup: false,
            cards: Vec::new(),
        };
    };
    let shown = m.findings(CheckKind::Security);
    let open = m.open(CheckKind::Security);
    let stale = k.state == clusia_protocol::CheckState::Stale;
    let age = long_age(now - result.at);
    let mut line = if stale {
        if shown.is_empty() {
            format!("Checked an older commit, {age} ago. Check again to look at the latest one.")
        } else {
            format!("Checked an older commit, {age} ago. Check again to add these to your draft.")
        }
    } else if result.findings.is_empty() && result.unreadable == 0 {
        // A claim about what the agent found, so only for a result with nothing in it.
        format!(
            "Claude Code found no security issues in {}.",
            plural(result.files as usize, "file")
        )
    } else if result.findings.is_empty() {
        format!(
            "Claude Code checked {}; {} could not be read.",
            plural(result.files as usize, "file"),
            plural(result.unreadable as usize, "result")
        )
    } else if shown.is_empty() {
        format!(
            "Claude Code checked the {} {age} ago. No findings wait for your OK.",
            plural(result.files as usize, "changed file")
        )
    } else {
        format!(
            "Claude Code checked the {} {age} ago. Accepted findings become draft comments.",
            plural(result.files as usize, "changed file")
        )
    };
    if result.unreadable > 0 && !result.findings.is_empty() {
        line.push_str(&format!(
            " {} could not be read.",
            plural(result.unreadable as usize, "result")
        ));
    }
    let count = |want: Severity| {
        open.iter()
            .filter(|f| f.severity.as_ref().is_some_and(|s| *s == want))
            .count()
    };
    let chips = [
        (count(Severity::High), "high", Tone::Orange),
        (count(Severity::Medium), "medium", Tone::Neutral),
        (count(Severity::Low), "low", Tone::Neutral),
    ]
    .into_iter()
    .filter(|(n, _, _)| *n > 0)
    .map(|(n, label, tone)| (format!("{n} {label}"), tone))
    .collect();
    let cards = shown
        .iter()
        .map(|f| {
            let (severity, tone) = severity_of(f);
            CardView {
                id: f.id.clone(),
                severity: severity.to_string(),
                tone,
                title: printable(&f.title),
                place: printable(&place_of(f)),
                body: printable_lines(&f.body),
                code: f
                    .code
                    .as_deref()
                    .map(printable_lines)
                    .filter(|c| !c.trim().is_empty()),
                comment: printable_lines(&proposed_comment(f)),
                show: files.contains(&f.file.as_str()).then(|| f.file.clone()),
                accepted: m.accepted.contains(&f.id),
                busy: m.pending.contains_key(&f.id),
                dismissing: m.pending.get(&f.id) == Some(&false),
                general: !f.anchored,
                stale,
                editing: editing == Some(f.id.as_str()),
            }
        })
        .collect();
    SecurityView {
        title: match (open.len(), shown.len()) {
            (0, 0) if result.findings.is_empty() => "No security findings".to_string(),
            (0, 0) => "No open security findings".to_string(),
            (0, _) => "All security findings are in your draft".to_string(),
            (n, _) => plural(n, "security finding"),
        },
        line,
        chips,
        action: Some(CheckAction::CheckAgain),
        setup: false,
        cards,
    }
}

/// The tab's content inside `SectionBody`, rebuilt when its view or its editor's state changes.
#[derive(Component, Debug)]
pub struct SecurityRoot {
    pub pr: PrRef,
    built: Option<(SecurityView, Option<String>, bool)>,
}

/// Keeps `SectionBody` filled with the Security tab while that section is selected. The other
/// sections' fillers clear it when they take over.
pub(super) fn fill_security(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    checks: Res<super::Checks>,
    model: Res<Model>,
    clock: Res<Clock>,
    fonts: Res<UiFonts>,
    bodies: Query<(Entity, &SectionBody, Option<&Children>)>,
    mut roots: Query<(Entity, &mut SecurityRoot)>,
) {
    for (entity, body, children) in &bodies {
        let Some(tab) = tabs.0.get(&body.pr) else {
            continue;
        };
        let Phase::Ready(ready) = &tab.phase else {
            continue;
        };
        if tab.ui.section != ReviewSection::Security {
            continue;
        }
        let editor = finding_editor(tab);
        let files: Vec<&str> = ready.view.files.iter().map(|f| f.path.as_str()).collect();
        let view = security_view(
            checks.0.get(&body.pr),
            harness_ready(&model.snapshot),
            &files,
            clock.now(),
            editor.and_then(|e| match &e.target {
                crate::review_state::EditTarget::Finding(id) => Some(id.as_str()),
                _ => None,
            }),
            ready.cached_at.is_some(),
        );
        let key = (
            view,
            editor.and_then(|e| e.error.clone()),
            editor.is_some_and(|e| e.ticket.is_some()),
        );
        let existing = children
            .into_iter()
            .flatten()
            .copied()
            .find(|c| roots.contains(*c))
            .and_then(|c| roots.get_mut(c).ok());
        let pr = body.pr.clone();
        let editor = editor.cloned();
        match existing {
            Some((_, root)) if root.built.as_ref() == Some(&key) => {}
            Some((root_entity, mut root)) => {
                commands.entity(root_entity).despawn_related::<Children>();
                commands.entity(root_entity).with_children(|p| {
                    draw(p, &fonts, &pr, &key.0, editor.as_ref());
                });
                root.built = Some(key);
            }
            None => {
                commands.entity(entity).despawn_related::<Children>();
                commands.entity(entity).with_children(|p| {
                    p.spawn((
                        Node {
                            flex_grow: 1.0,
                            width: percent(100),
                            min_height: px(0),
                            flex_direction: FlexDirection::Column,
                            row_gap: px(12),
                            padding: UiRect::axes(px(28), px(20)),
                            overflow: Overflow::scroll_y(),
                            ..default()
                        },
                        ScrollArea,
                        ScrollPosition::default(),
                        SecurityRoot {
                            pr: pr.clone(),
                            built: Some(key.clone()),
                        },
                    ))
                    .with_children(|c| draw(c, &fonts, &pr, &key.0, editor.as_ref()));
                });
            }
        }
    }
}

fn draw(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    v: &SecurityView,
    editor: Option<&Editor>,
) {
    p.spawn(Node {
        column_gap: px(12),
        align_items: AlignItems::Start,
        flex_shrink: 0.0,
        ..default()
    })
    .with_children(|h| {
        h.spawn(Node {
            flex_grow: 1.0,
            min_width: px(0),
            flex_direction: FlexDirection::Column,
            row_gap: px(4),
            ..default()
        })
        .with_children(|t| {
            t.spawn(text(fonts, v.title.clone(), Type::HEADING));
            if !v.line.is_empty() {
                t.spawn(text(fonts, v.line.clone(), Type::MUTED));
            }
        });
        for (label, tone) in &v.chips {
            h.spawn(badge(fonts, label, *tone));
        }
        if let Some(action) = v.action {
            check_button(h, fonts, pr, CheckKind::Security, action);
        }
        if v.setup {
            setup_button(h, fonts);
        }
    });
    for c in &v.cards {
        finding_card(p, fonts, pr, c, editor);
    }
}

fn finding_card(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    c: &CardView,
    editor: Option<&Editor>,
) {
    let high = c.tone == Tone::Orange;
    p.spawn(card(Node {
        flex_direction: FlexDirection::Column,
        flex_shrink: 0.0,
        row_gap: px(10),
        padding: px(16).all(),
        ..default()
    }))
    .insert(Stroke(if high { Swatch::Orange } else { Swatch::Line }))
    .with_children(|k| {
        k.spawn(Node {
            column_gap: px(10),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|t| {
            t.spawn(badge(fonts, &c.severity, c.tone));
            t.spawn(Node {
                flex_grow: 1.0,
                min_width: px(0),
                ..default()
            })
            .with_children(|n| {
                n.spawn(text(fonts, c.title.clone(), Type::STRONG));
            });
            t.spawn(text(fonts, c.place.clone(), Type::MONO));
        });
        k.spawn(text(fonts, c.body.clone(), Type::BODY));
        if let Some(code) = &c.code {
            k.spawn(panel(
                Node {
                    padding: UiRect::axes(px(12), px(10)),
                    border_radius: BorderRadius::all(px(6)),
                    ..default()
                },
                Swatch::Chrome,
            ))
            .with_children(|b| {
                b.spawn(text(fonts, code.clone(), Type::MONO));
            });
        }
        if c.general {
            k.spawn(text(
                fonts,
                "Not on a changed line: it joins your draft as a general comment.",
                Type::META,
            ));
        }
        if let (true, Some(editor)) = (c.editing, editor) {
            editor_box(k, fonts, editor, pr);
            return;
        }
        k.spawn(text(fonts, "Proposed comment", Type::META));
        k.spawn(panel(
            Node {
                padding: UiRect::axes(px(12), px(10)),
                border_radius: BorderRadius::all(px(6)),
                ..default()
            },
            Swatch::Chrome,
        ))
        .with_children(|b| {
            b.spawn(text(fonts, c.comment.clone(), Type::BODY));
        });
        k.spawn(Node {
            column_gap: px(8),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|row| {
            if c.accepted {
                row.spawn(badge(fonts, "✓ In your draft", Tone::Green));
            } else if c.busy {
                let label = if c.dismissing {
                    "Dismissing…"
                } else {
                    "Adding…"
                };
                row.spawn(crate::ui::kit::disabled_button(fonts, label));
            } else if c.stale {
                dismiss_button(row, fonts, pr, &c.id, Variant::Secondary);
            } else {
                finding_buttons(row, fonts, pr, &c.id, "Add to draft", Variant::Secondary);
            }
            row.spawn(Node {
                flex_grow: 1.0,
                ..default()
            });
            if let Some(file) = &c.show {
                show_button(row, fonts, pr, file);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::ChecksTell;
    use crate::fixture;
    use crate::testing::NOW;
    use clusia_protocol::CheckState;

    const FILES: [&str; 3] = ["src/client/http.rs", "src/auth/store.rs", "src/config.rs"];

    fn model() -> ChecksModel {
        let (results, states) = fixture::demo_checks(NOW);
        let mut m = ChecksModel::default();
        m.apply(&ChecksTell::Loaded {
            pr: fixture::demo_pr(),
            results,
            states,
            accepted: vec![],
            dismissed: vec![],
        });
        m
    }

    #[test]
    fn the_mockup_view() {
        let v = security_view(Some(&model()), true, &FILES, NOW, None, false);
        assert_eq!(v.title, "3 security findings");
        assert_eq!(
            v.line,
            "Claude Code checked the 7 changed files 2 minutes ago. Accepted findings become draft comments."
        );
        assert_eq!(
            v.chips,
            [
                ("1 high".to_string(), Tone::Orange),
                ("1 medium".to_string(), Tone::Neutral),
                ("1 low".to_string(), Tone::Neutral),
            ]
        );
        assert_eq!(v.action, Some(CheckAction::CheckAgain));
        let high = &v.cards[0];
        assert_eq!(
            (high.severity.as_str(), high.tone, high.title.as_str()),
            (
                "HIGH",
                Tone::Orange,
                "Refresh token written to the debug log"
            )
        );
        assert_eq!(high.place, "src/client/http.rs:17");
        assert!(high.code.is_some());
        assert!(high.comment.starts_with("This logs the whole request body"));
        assert!(!high.stale);
        assert_eq!(high.show.as_deref(), Some("src/client/http.rs"));
        assert!(!high.accepted && !high.busy && !high.general);
        assert_eq!(v.cards[1].severity, "MEDIUM");
        assert_eq!(v.cards[2].severity, "LOW");
    }

    #[test]
    fn a_file_outside_the_pull_request_has_no_show_in_diff() {
        let v = security_view(Some(&model()), true, &["src/config.rs"], NOW, None, false);
        assert!(v.cards.iter().all(|c| c.show.is_none()));
    }

    #[test]
    fn one_finding_is_singular() {
        let mut m = model();
        let ids: Vec<String> = m
            .findings(CheckKind::Security)
            .iter()
            .map(|f| f.id.clone())
            .collect();
        for id in &ids[1..] {
            m.dismissed.insert(id.clone());
        }
        let v = security_view(Some(&m), true, &FILES, NOW, None, false);
        assert_eq!(v.title, "1 security finding");
        assert_eq!(v.chips, [("1 high".to_string(), Tone::Orange)]);
    }

    #[test]
    fn accepted_and_waiting_cards_say_so() {
        let mut m = model();
        let id = m.findings(CheckKind::Security)[1].id.clone();
        m.accepted.insert(id.clone());
        m.pending
            .insert(m.findings(CheckKind::Security)[0].id.clone(), true);
        let v = security_view(Some(&m), true, &FILES, NOW, None, false);
        assert!(v.cards[1].accepted);
        assert!(v.cards[0].busy);
    }

    #[test]
    fn an_unanchored_finding_joins_as_a_general_comment() {
        let mut m = model();
        m.security.result.as_mut().unwrap().findings[0].anchored = false;
        let v = security_view(Some(&m), true, &FILES, NOW, None, false);
        assert!(v.cards[0].general);
        assert!(
            v.cards[0].comment.starts_with("src/client/http.rs:17: "),
            "the card shows what the daemon will add"
        );
    }

    #[test]
    fn accepted_findings_leave_the_count_but_keep_their_card() {
        let mut m = model();
        let ids: Vec<String> = m
            .findings(CheckKind::Security)
            .iter()
            .map(|f| f.id.clone())
            .collect();
        m.accepted.insert(ids[1].clone());
        let v = security_view(Some(&m), true, &FILES, NOW, None, false);
        assert_eq!(v.title, "2 security findings");
        assert_eq!(
            v.chips,
            [
                ("1 high".to_string(), Tone::Orange),
                ("1 low".to_string(), Tone::Neutral)
            ]
        );
        assert_eq!(v.cards.len(), 3);
        assert!(v.cards[1].accepted);
        m.accepted.extend(ids);
        let v = security_view(Some(&m), true, &FILES, NOW, None, false);
        assert_eq!(v.title, "All security findings are in your draft");
        assert!(v.chips.is_empty());
    }

    #[test]
    fn an_unanchored_range_is_prefixed_as_the_daemon_writes_it() {
        let mut m = model();
        let f = &mut m.security.result.as_mut().unwrap().findings[0];
        f.anchored = false;
        f.line = None;
        f.start_line = Some(5);
        f.end_line = Some(7);
        let v = security_view(Some(&m), true, &FILES, NOW, None, false);
        assert!(
            v.cards[0].comment.starts_with("src/client/http.rs:5-7: "),
            "{}",
            v.cards[0].comment
        );
        let f = &mut m.security.result.as_mut().unwrap().findings[0];
        f.start_line = Some(5);
        f.end_line = Some(5);
        let v = security_view(Some(&m), true, &FILES, NOW, None, false);
        assert!(v.cards[0].comment.starts_with("src/client/http.rs:5-5: "));
    }

    #[test]
    fn a_stale_result_marks_every_card_stale() {
        let mut m = model();
        m.security.state = CheckState::Stale;
        let v = security_view(Some(&m), true, &FILES, NOW, None, false);
        assert!(v.cards.iter().all(|c| c.stale));
    }

    #[test]
    fn a_clean_stale_or_unreadable_result_is_worded_for_what_it_is() {
        let mut m = model();
        m.security.result.as_mut().unwrap().findings.clear();
        let v = security_view(Some(&m), true, &FILES, NOW, None, false);
        assert_eq!(v.title, "No security findings");
        assert_eq!(v.line, "Claude Code found no security issues in 7 files.");
        assert!(v.cards.is_empty() && v.chips.is_empty());
        m.security.state = CheckState::Stale;
        let v = security_view(Some(&m), true, &FILES, NOW, None, false);
        assert_eq!(
            v.line,
            "Checked an older commit, 2 minutes ago. Check again to look at the latest one.",
            "nothing to add: no tail about the draft"
        );
        assert_eq!(v.action, Some(CheckAction::CheckAgain));
        m.security.result = model().security.result;
        let v = security_view(Some(&m), true, &FILES, NOW, None, false);
        assert_eq!(
            v.line,
            "Checked an older commit, 2 minutes ago. Check again to add these to your draft."
        );
        m.security.state = CheckState::Done;
        m.security.result.as_mut().unwrap().unreadable = 1;
        let v = security_view(Some(&m), true, &FILES, NOW, None, false);
        assert!(
            v.line.ends_with("1 result could not be read."),
            "{}",
            v.line
        );
    }

    #[test]
    fn untrusted_text_is_made_printable() {
        let mut m = model();
        let f = &mut m.security.result.as_mut().unwrap().findings[0];
        f.title = "Looks fine\u{1b}[2J".into();
        f.body = "line one\nline\u{202e}two".into();
        f.code = Some("+ ok\u{7}".into());
        f.comment = "Please\u{1b}[0m fix".into();
        let v = security_view(Some(&m), true, &FILES, NOW, None, false);
        let c = &v.cards[0];
        for text in [&c.title, &c.body, &c.comment, c.code.as_ref().unwrap()] {
            assert!(
                !text.contains('\u{1b}') && !text.contains('\u{202e}') && !text.contains('\u{7}'),
                "{text:?}"
            );
        }
        assert!(c.body.contains('\n'), "line breaks stay");
    }

    #[test]
    fn states_without_a_result_show_the_notice() {
        let read = ChecksModel {
            loaded: true,
            ..ChecksModel::default()
        };
        let v = security_view(Some(&read), false, &FILES, NOW, None, false);
        assert_eq!(v.line, "Set up a harness in Config › Harness");
        assert!(v.setup && v.cards.is_empty() && v.action.is_none());
        let v = security_view(Some(&read), true, &FILES, NOW, None, false);
        assert_eq!(v.action, Some(CheckAction::Run));
        assert_eq!(v.title, "Security check");
    }
}
