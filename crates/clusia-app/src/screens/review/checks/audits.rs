//! The Audits tab (mockup `Audits.png`): the areas the run was asked about on the left, the
//! selected area's findings and passes on the right. A finding is the agent's proposal and
//! waits for the human's OK.

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::input_focus::tab_navigation::TabIndex;
use bevy::picking::hover::Hovered;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, Button as WidgetButton, ScrollArea, observe};
use std::collections::BTreeSet;

use clusia_core::PrRef;
use clusia_core::checks::{AreaStatus, AuditArea, CheckKind, area_status};
use clusia_core::printable::{printable, printable_lines};
use clusia_protocol::CheckState;

use super::{
    CheckAction, Checks, ChecksModel, Notice, check_button, dismiss_button, finding_buttons,
    finding_editor, notice_of, place_of, proposed_comment, setup_button, show_button,
};
use crate::bridge::Model;
use crate::clock::Clock;
use crate::fonts::UiFonts;
use crate::review_state::{EditTarget, Editor, Phase, ReviewSection, ReviewTabs};
use crate::screens::home::long_age;
use crate::screens::review::agent::{Chats, harness_ready};
use crate::screens::review::editor::editor_box;
use crate::screens::review::shell::SectionBody;
use crate::theme::Swatch;
use crate::ui::kit::{
    Clickable, Fill, HoverFill, Stroke, Tone, Type, Variant, badge, card, disabled_button, panel,
    text,
};

/// What the area's row says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowStatus {
    Findings(u32),
    Ok,
    NotChecked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AreaRow {
    pub id: String,
    pub name: String,
    pub status: RowStatus,
    pub selected: bool,
}

/// One finding of the selected area. Every text is already printable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingView {
    pub id: String,
    pub title: String,
    pub place: String,
    pub body: String,
    /// The proposed comment.
    pub comment: String,
    /// The file, when the pull request changes it (**Show in diff**).
    pub show: Option<String>,
    pub accepted: bool,
    pub busy: bool,
    /// What waits is a dismiss.
    pub dismissing: bool,
    pub general: bool,
    /// The result is of an older head: only **Dismiss** and **Show in diff** are offered.
    pub stale: bool,
    /// The card's editor is open.
    pub editing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassView {
    pub text: String,
    /// What the agent says it looked at (`checked 4 call sites`, `tests/refresh.rs:12`).
    pub place: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detail {
    pub id: String,
    pub name: String,
    /// The line for an area with nothing to list (`Not checked: …`).
    pub empty: Option<String>,
    /// `No findings in this area.`: the area was checked and nothing is open.
    pub clean: Option<String>,
    pub findings: Vec<FindingView>,
    pub passes: Vec<PassView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditsView {
    /// Whole-tab state (no harness, not run, waiting, running, failed): no list then.
    pub notice: Option<Notice>,
    /// `AUDIT · 6 AREAS`
    pub heading: String,
    pub rows: Vec<AreaRow>,
    pub action: Option<CheckAction>,
    /// A stale result and unreadable blocks, above the detail.
    pub banner: Option<String>,
    pub detail: Option<Detail>,
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

fn empty_view(notice: Option<Notice>) -> AuditsView {
    AuditsView {
        notice,
        heading: String::new(),
        rows: Vec::new(),
        action: None,
        banner: None,
        detail: None,
    }
}

/// What the Audits tab shows. `areas` are the configured audit areas (they name the ids the run
/// reports), `files` the pull request's changed paths, `selected` the chosen area's id,
/// `editing` the id of the finding whose editor is open and `offline` says the tab is a cached
/// copy.
#[allow(clippy::too_many_arguments)] // each is one fact the view is drawn from
pub fn audits_view(
    model: Option<&ChecksModel>,
    areas: &[AuditArea],
    harness: bool,
    files: &[&str],
    now: i64,
    selected: Option<&str>,
    editing: Option<&str>,
    offline: bool,
) -> AuditsView {
    let default = ChecksModel::default();
    let m = model.unwrap_or(&default);
    let k = m.kind(CheckKind::Audit);
    // Nothing to run: the daemon refuses an audit with every area off.
    if m.loaded
        && k.result.is_none()
        && k.state == CheckState::NotRun
        && !areas.iter().any(|a| a.enabled)
    {
        return empty_view(Some(Notice {
            title: "Audit".into(),
            line: "No audit areas are switched on. Switch one on in Config › Harness.".into(),
            action: None,
            setup: true,
        }));
    }
    if let Some(notice) = notice_of(CheckKind::Audit, k, harness, m.loaded, offline) {
        return empty_view(Some(notice));
    }
    let Some(result) = &k.result else {
        return empty_view(None);
    };
    let name_of = |id: &str| {
        areas
            .iter()
            .find(|a| a.id == id)
            .map_or_else(|| printable(id), |a| printable(&a.name))
    };
    // The marks count what is still open: dismissed and accepted findings both leave them.
    let settled: BTreeSet<String> = m.dismissed.union(&m.accepted).cloned().collect();
    let stale = k.state == CheckState::Stale;
    let mut rows: Vec<AreaRow> = result
        .areas
        .iter()
        .map(|id| AreaRow {
            id: id.clone(),
            name: name_of(id),
            status: match area_status(result, id, &settled) {
                AreaStatus::Findings(n) => RowStatus::Findings(n),
                AreaStatus::Ok => RowStatus::Ok,
                // The user settled every block of the area: it was checked.
                AreaStatus::NotChecked if result.findings.iter().any(|f| f.area == *id) => {
                    RowStatus::Ok
                }
                AreaStatus::NotChecked => RowStatus::NotChecked,
            },
            selected: false,
        })
        .collect();
    let chosen = selected
        .filter(|s| rows.iter().any(|r| r.id == *s))
        .map(String::from)
        .or_else(|| {
            rows.iter()
                .find(|r| matches!(r.status, RowStatus::Findings(_)))
                .or(rows.first())
                .map(|r| r.id.clone())
        });
    for row in &mut rows {
        row.selected = chosen.as_deref() == Some(row.id.as_str());
    }
    let detail = chosen.map(|id| {
        let status = rows.iter().find(|r| r.id == id).map(|r| r.status.clone());
        let findings = m
            .findings(CheckKind::Audit)
            .into_iter()
            .filter(|f| f.area == id)
            .map(|f| FindingView {
                id: f.id.clone(),
                title: printable(&f.title),
                place: printable(&place_of(f)),
                body: printable_lines(&f.body),
                comment: printable_lines(&proposed_comment(f)),
                show: files.contains(&f.file.as_str()).then(|| f.file.clone()),
                accepted: m.accepted.contains(&f.id),
                busy: m.pending.contains_key(&f.id),
                dismissing: m.pending.get(&f.id) == Some(&false),
                general: !f.anchored,
                stale,
                editing: editing == Some(f.id.as_str()),
            })
            .collect::<Vec<FindingView>>();
        let passes = result
            .passes
            .iter()
            .filter(|p| p.area == id)
            .map(|p| PassView {
                text: printable(&p.text),
                place: p.place.as_deref().map(printable).filter(|s| !s.is_empty()),
            })
            .collect();
        Detail {
            name: name_of(&id),
            empty: (status == Some(RowStatus::NotChecked))
                .then(|| "Not checked: Claude Code returned nothing for this area.".to_string()),
            clean: (status == Some(RowStatus::Ok) && findings.is_empty())
                .then(|| "No findings in this area.".to_string()),
            id,
            findings,
            passes,
        }
    });
    let mut banner = Vec::new();
    if stale {
        banner.push(format!(
            "Checked an older commit, {} ago. Check again to add these to your draft.",
            long_age(now - result.at)
        ));
    }
    if result.unreadable > 0 {
        banner.push(format!(
            "{} could not be read.",
            plural(result.unreadable as usize, "result")
        ));
    }
    AuditsView {
        notice: None,
        heading: format!(
            "AUDIT · {} {}",
            rows.len(),
            if rows.len() == 1 { "AREA" } else { "AREAS" }
        ),
        rows,
        action: Some(CheckAction::CheckAgain),
        banner: (!banner.is_empty()).then(|| banner.join(" ")),
        detail,
    }
}

/// A row of the area list.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct AreaButton {
    pub pr: PrRef,
    pub area: String,
}

/// **Ask the agent about this**: the area's name is already printable.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct AskAreaButton {
    pub pr: PrRef,
    pub name: String,
}

/// The scrolling list of areas on the left.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct AreaList(pub PrRef);

/// The scrolling detail of the selected area on the right.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct AuditDetail(pub PrRef);

/// The tab's content inside `SectionBody`, rebuilt when its view or its editor's state changes.
#[derive(Component, Debug)]
pub struct AuditsRoot {
    pub pr: PrRef,
    built: Option<(AuditsView, Option<String>, bool)>,
}

/// Keeps `SectionBody` filled with the Audits tab while that section is selected. The other
/// sections' fillers clear it when they take over.
pub(super) fn fill_audits(
    mut commands: Commands,
    tabs: Res<ReviewTabs>,
    checks: Res<Checks>,
    model: Res<Model>,
    clock: Res<Clock>,
    fonts: Res<UiFonts>,
    bodies: Query<(Entity, &SectionBody, Option<&Children>)>,
    mut roots: Query<(Entity, &mut AuditsRoot)>,
    lists: Query<(&AreaList, &ScrollPosition)>,
    details: Query<(&AuditDetail, &ScrollPosition)>,
) {
    for (entity, body, children) in &bodies {
        let Some(tab) = tabs.0.get(&body.pr) else {
            continue;
        };
        let Phase::Ready(ready) = &tab.phase else {
            continue;
        };
        if tab.ui.section != ReviewSection::Audits {
            continue;
        }
        let editor = finding_editor(tab);
        let files: Vec<&str> = ready.view.files.iter().map(|f| f.path.as_str()).collect();
        let view = audits_view(
            checks.0.get(&body.pr),
            &model.snapshot.config.harness.audit_areas,
            harness_ready(&model.snapshot),
            &files,
            clock.now(),
            tab.ui.audit_area.as_deref(),
            editor.and_then(|e| match &e.target {
                EditTarget::Finding(id) => Some(id.as_str()),
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
                // The panes are spawned again: they keep where the user had scrolled them.
                let scrolled = Scrolled {
                    list: lists
                        .iter()
                        .find(|(list, _)| list.0 == pr)
                        .map(|(_, at)| at.clone())
                        .unwrap_or_default(),
                    detail: details
                        .iter()
                        .find(|(detail, _)| detail.0 == pr)
                        .map(|(_, at)| at.clone())
                        .unwrap_or_default(),
                };
                commands.entity(root_entity).despawn_related::<Children>();
                commands.entity(root_entity).with_children(|p| {
                    draw(p, &fonts, &pr, &key.0, editor.as_ref(), scrolled);
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
                            ..default()
                        },
                        AuditsRoot {
                            pr: pr.clone(),
                            built: Some(key.clone()),
                        },
                    ))
                    .with_children(|c| {
                        draw(c, &fonts, &pr, &key.0, editor.as_ref(), Scrolled::default());
                    });
                });
            }
        }
    }
}

/// Where the two panes were scrolled before a rebuild.
#[derive(Debug, Clone, Default)]
struct Scrolled {
    list: ScrollPosition,
    detail: ScrollPosition,
}

fn draw(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    v: &AuditsView,
    editor: Option<&Editor>,
    scrolled: Scrolled,
) {
    if let Some(n) = &v.notice {
        notice_pane(p, fonts, pr, n);
        return;
    }
    p.spawn((
        Node {
            width: px(280),
            flex_shrink: 0.0,
            min_height: px(0),
            flex_direction: FlexDirection::Column,
            row_gap: px(4),
            padding: UiRect::axes(px(12), px(16)),
            border: UiRect::right(px(1)),
            ..default()
        },
        BorderColor::default(),
        Stroke(Swatch::Line),
    ))
    .with_children(|left| {
        left.spawn(Node {
            align_items: AlignItems::Center,
            padding: UiRect::bottom(px(6)),
            ..default()
        })
        .with_children(|h| {
            h.spawn(text(fonts, v.heading.clone(), Type::META));
            h.spawn(Node {
                flex_grow: 1.0,
                ..default()
            });
            if let Some(action) = v.action {
                check_button(h, fonts, pr, CheckKind::Audit, action);
            }
        });
        left.spawn((
            Node {
                flex_direction: FlexDirection::Column,
                row_gap: px(4),
                flex_grow: 1.0,
                min_height: px(0),
                overflow: Overflow::scroll_y(),
                ..default()
            },
            ScrollArea,
            scrolled.list,
            AreaList(pr.clone()),
        ))
        .with_children(|list| {
            for row in &v.rows {
                area_row(list, fonts, pr, row);
            }
        });
        left.spawn(text(
            fonts,
            "Run by Claude Code with your audit areas. Findings the agent proposes wait for your OK before they join the draft.",
            Type::META,
        ));
    });
    p.spawn((
        Node {
            flex_grow: 1.0,
            min_width: px(0),
            min_height: px(0),
            flex_direction: FlexDirection::Column,
            row_gap: px(12),
            padding: UiRect::axes(px(28), px(20)),
            overflow: Overflow::scroll_y(),
            ..default()
        },
        ScrollArea,
        scrolled.detail,
        AuditDetail(pr.clone()),
    ))
    .with_children(|right| {
        if let Some(d) = &v.detail {
            right
                .spawn(Node {
                    align_items: AlignItems::Center,
                    flex_shrink: 0.0,
                    ..default()
                })
                .with_children(|h| {
                    h.spawn(Node {
                        flex_grow: 1.0,
                        min_width: px(0),
                        ..default()
                    })
                    .with_children(|n| {
                        n.spawn(text(fonts, d.name.clone(), Type::HEADING));
                    });
                    h.spawn((
                        button_ask(fonts),
                        AskAreaButton {
                            pr: pr.clone(),
                            name: d.name.clone(),
                        },
                        observe(on_ask),
                    ));
                });
            if let Some(banner) = &v.banner {
                right.spawn(text(fonts, banner.clone(), Type::MUTED));
            }
            if let Some(line) = &d.empty {
                right.spawn(text(fonts, line.clone(), Type::MUTED));
            }
            if let Some(line) = &d.clean {
                right.spawn(text(fonts, line.clone(), Type::MUTED));
            }
            for f in &d.findings {
                finding_card(right, fonts, pr, f, editor);
            }
            for pass in &d.passes {
                pass_row(right, fonts, pass);
            }
        }
    });
}

fn button_ask(fonts: &UiFonts) -> impl Bundle {
    crate::ui::kit::button(fonts, "Ask the agent about this", Variant::Secondary)
}

fn notice_pane(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, n: &Notice) {
    p.spawn(Node {
        flex_grow: 1.0,
        min_width: px(0),
        column_gap: px(12),
        align_items: AlignItems::Start,
        padding: UiRect::axes(px(28), px(20)),
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
            t.spawn(text(fonts, n.title.clone(), Type::HEADING));
            t.spawn(text(fonts, n.line.clone(), Type::MUTED));
        });
        if let Some(action) = n.action {
            check_button(h, fonts, pr, CheckKind::Audit, action);
        }
        if n.setup {
            setup_button(h, fonts);
        }
    });
}

fn area_row(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pr: &PrRef, row: &AreaRow) {
    let (mark, mark_fill, mark_ink) = match row.status {
        RowStatus::Findings(_) => ("!", Swatch::OrangeSoft, Swatch::Orange),
        RowStatus::Ok => ("✓", Swatch::GreenSoft, Swatch::Green),
        RowStatus::NotChecked => ("–", Swatch::Chrome, Swatch::Faint),
    };
    p.spawn((
        Node {
            height: px(36),
            padding: UiRect::horizontal(px(8)),
            column_gap: px(10),
            align_items: AlignItems::Center,
            flex_shrink: 0.0,
            border_radius: BorderRadius::all(px(6)),
            ..default()
        },
        WidgetButton,
        Clickable,
        Hovered::default(),
        TabIndex(0),
        BackgroundColor::default(),
        Fill(if row.selected {
            Swatch::GreenSoft
        } else {
            Swatch::Clear
        }),
        HoverFill(Swatch::Hover),
        AreaButton {
            pr: pr.clone(),
            area: row.id.clone(),
        },
        observe(on_area),
    ))
    .with_children(|r| {
        r.spawn(panel(
            Node {
                width: px(22),
                height: px(22),
                flex_shrink: 0.0,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                border_radius: BorderRadius::MAX,
                ..default()
            },
            mark_fill,
        ))
        .with_children(|d| {
            d.spawn(text(fonts, mark, Type::STRONG.ink(mark_ink)));
        });
        r.spawn(Node {
            flex_grow: 1.0,
            min_width: px(0),
            ..default()
        })
        .with_children(|n| {
            n.spawn(text(fonts, row.name.clone(), Type::BODY));
        });
        match &row.status {
            RowStatus::Findings(n) => {
                r.spawn(text(fonts, n.to_string(), Type::BODY.ink(Swatch::Orange)));
            }
            RowStatus::Ok => {
                r.spawn(text(fonts, "ok", Type::META));
            }
            RowStatus::NotChecked => {
                r.spawn(text(fonts, "Not checked", Type::META.ink(Swatch::Faint)));
            }
        }
    });
}

fn finding_card(
    p: &mut ChildSpawnerCommands,
    fonts: &UiFonts,
    pr: &PrRef,
    f: &FindingView,
    editor: Option<&Editor>,
) {
    p.spawn(card(Node {
        flex_direction: FlexDirection::Column,
        flex_shrink: 0.0,
        row_gap: px(10),
        padding: px(16).all(),
        ..default()
    }))
    .with_children(|k| {
        k.spawn(Node {
            column_gap: px(10),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|t| {
            if f.accepted {
                t.spawn(badge(fonts, "✓ In your draft", Tone::Green));
            } else {
                t.spawn(badge(fonts, "Needs your OK", Tone::Orange));
            }
            t.spawn(Node {
                flex_grow: 1.0,
                min_width: px(0),
                ..default()
            })
            .with_children(|n| {
                n.spawn(text(fonts, f.title.clone(), Type::STRONG));
            });
            t.spawn(text(fonts, f.place.clone(), Type::MONO));
        });
        k.spawn(text(fonts, f.body.clone(), Type::BODY));
        if f.general {
            k.spawn(text(
                fonts,
                "Not on a changed line: it joins your draft as a general comment.",
                Type::META,
            ));
        }
        if let (true, Some(editor)) = (f.editing, editor) {
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
            b.spawn(text(fonts, f.comment.clone(), Type::BODY));
        });
        k.spawn(Node {
            column_gap: px(8),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|row| {
            if f.accepted {
                // Nothing to decide: it is in the draft.
            } else if f.busy {
                let label = if f.dismissing {
                    "Dismissing…"
                } else {
                    "Adding…"
                };
                row.spawn(disabled_button(fonts, label));
            } else if f.stale {
                dismiss_button(row, fonts, pr, &f.id, Variant::Ghost);
            } else {
                finding_buttons(row, fonts, pr, &f.id, "Accept into draft", Variant::Ghost);
            }
            row.spawn(Node {
                flex_grow: 1.0,
                ..default()
            });
            if let Some(file) = &f.show {
                show_button(row, fonts, pr, file);
            }
        });
    });
}

fn pass_row(p: &mut ChildSpawnerCommands, fonts: &UiFonts, pass: &PassView) {
    p.spawn(card(Node {
        flex_shrink: 0.0,
        column_gap: px(12),
        align_items: AlignItems::Center,
        padding: UiRect::axes(px(16), px(12)),
        ..default()
    }))
    .with_children(|r| {
        r.spawn(text(fonts, "✓", Type::STRONG.ink(Swatch::Green)));
        r.spawn(Node {
            flex_grow: 1.0,
            min_width: px(0),
            ..default()
        })
        .with_children(|n| {
            n.spawn(text(fonts, pass.text.clone(), Type::BODY));
        });
        if let Some(place) = &pass.place {
            r.spawn(text(fonts, place.clone(), Type::META));
        }
    });
}

fn on_area(activate: On<Activate>, buttons: Query<&AreaButton>, mut tabs: ResMut<ReviewTabs>) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    if let Some(tab) = tabs.0.get_mut(&b.pr)
        && tab.ui.audit_area.as_deref() != Some(b.area.as_str())
    {
        tab.ui.audit_area = Some(b.area.clone());
    }
}

/// Puts the question's start in the chat input and shows the Agent tab; nothing is sent.
fn on_ask(activate: On<Activate>, buttons: Query<&AskAreaButton>, mut chats: ResMut<Chats>) {
    let Ok(b) = buttons.get(activate.entity) else {
        return;
    };
    chats
        .entry(&b.pr)
        .ask_about(format!("About the {} audit: ", b.name));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Ask, ChecksTell, Tell};
    use crate::fixture;
    use crate::review_state::{EditTarget, ReviewSection, ReviewTabs};
    use crate::screens::review::agent::{ChatInput, Chats, PanelTab};
    use crate::screens::review::checks::{
        CheckButton, FindingAccept, FindingDismiss, FindingEdit, FindingShow,
    };
    use crate::screens::review::editor::{EditorArea, EditorSubmit};
    use crate::testing::{self, NOW};
    use bevy::text::EditableText;
    use clusia_core::PrRef;
    use clusia_core::checks::{AuditArea, default_areas};
    use clusia_protocol::CheckState;

    const FILES: [&str; 3] = ["src/auth/refresh.rs", "tests/refresh.rs", "src/config.rs"];

    /// A model the daemon answered, with nothing in it.
    fn read() -> ChecksModel {
        ChecksModel {
            loaded: true,
            ..ChecksModel::default()
        }
    }

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

    fn view(m: &ChecksModel, selected: Option<&str>) -> AuditsView {
        audits_view(
            Some(m),
            &default_areas(),
            true,
            &FILES,
            NOW,
            selected,
            None,
            false,
        )
    }

    fn rows(v: &AuditsView) -> Vec<(&str, &RowStatus)> {
        v.rows
            .iter()
            .map(|r| (r.name.as_str(), &r.status))
            .collect()
    }

    #[test]
    fn the_mockup_list() {
        let v = view(&model(), Some("concurrency"));
        assert_eq!(v.heading, "AUDIT · 6 AREAS");
        assert_eq!(
            rows(&v),
            [
                ("Correctness", &RowStatus::Findings(1)),
                ("Concurrency", &RowStatus::Findings(1)),
                ("Error handling", &RowStatus::Ok),
                ("Performance", &RowStatus::Ok),
                ("Tests", &RowStatus::Findings(1)),
                ("Docs and changelog", &RowStatus::Ok),
            ]
        );
        assert_eq!(
            v.rows
                .iter()
                .filter(|r| r.selected)
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["concurrency"]
        );
        assert_eq!(v.action, Some(CheckAction::CheckAgain));
        assert!(v.notice.is_none());
    }

    #[test]
    fn the_selection_falls_back_to_the_first_area_with_findings() {
        let m = model();
        for selected in [None, Some("gone")] {
            let v = view(&m, selected);
            assert_eq!(v.detail.as_ref().unwrap().id, "correctness");
        }
        let mut quiet = model();
        for f in quiet.audit.result.as_mut().unwrap().findings.clone() {
            quiet.dismissed.insert(f.id);
        }
        let v = view(&quiet, None);
        assert_eq!(v.detail.unwrap().id, "correctness", "else the first area");
    }

    #[test]
    fn the_concurrency_detail_has_the_finding_and_both_passes() {
        let v = view(&model(), Some("concurrency"));
        let d = v.detail.unwrap();
        assert_eq!(d.name, "Concurrency");
        assert_eq!(d.findings.len(), 1);
        let f = &d.findings[0];
        assert_eq!(f.title, "The refresh lock is held across a network call");
        assert_eq!(f.place, "src/auth/refresh.rs:44");
        assert!(f.body.starts_with("Every request that needs a token"));
        assert!(f.comment.starts_with("Could we re-check the expiry"));
        assert_eq!(f.show.as_deref(), Some("src/auth/refresh.rs"));
        assert!(!f.accepted && !f.busy && !f.general && !f.editing);
        let passes: Vec<(&str, Option<&str>)> = d
            .passes
            .iter()
            .map(|p| (p.text.as_str(), p.place.as_deref()))
            .collect();
        assert_eq!(
            passes,
            [
                (
                    "No shared state written outside the lock",
                    Some("checked 4 call sites")
                ),
                (
                    "The new test covers two tasks refreshing at once",
                    Some("tests/refresh.rs:12")
                ),
            ]
        );
        assert_eq!(d.empty, None);
        assert_eq!(d.clean, None, "it has an open finding");
    }

    #[test]
    fn an_area_without_a_block_is_not_checked() {
        let mut m = model();
        let result = m.audit.result.as_mut().unwrap();
        result.passes.retain(|p| p.area != "docs");
        let v = view(&m, Some("docs"));
        assert_eq!(
            rows(&v).last().map(|(_, s)| (*s).clone()),
            Some(RowStatus::NotChecked)
        );
        let d = v.detail.unwrap();
        assert_eq!(
            d.empty.as_deref(),
            Some("Not checked: Claude Code returned nothing for this area.")
        );
    }

    #[test]
    fn dismissed_findings_leave_the_area() {
        let mut m = model();
        let id = m.findings(CheckKind::Audit)[1].id.clone();
        m.dismissed.insert(id);
        let v = view(&m, Some("concurrency"));
        let d = v.detail.clone().unwrap();
        assert!(d.findings.is_empty());
        assert_eq!(d.clean.as_deref(), Some("No findings in this area."));
        assert_eq!(
            rows(&v)[1],
            ("Concurrency", &RowStatus::Ok),
            "its passes remain"
        );
    }

    #[test]
    fn accepted_findings_leave_the_count_but_keep_their_card() {
        let mut m = model();
        let id = m.findings(CheckKind::Audit)[1].id.clone();
        m.accepted.insert(id);
        let v = view(&m, Some("concurrency"));
        assert_eq!(rows(&v)[1], ("Concurrency", &RowStatus::Ok));
        let d = v.detail.unwrap();
        assert_eq!(d.findings.len(), 1);
        assert!(d.findings[0].accepted);
        assert_eq!(d.clean, None, "the card is still listed");
        let mut only = model();
        let id = only.findings(CheckKind::Audit)[2].id.clone();
        only.accepted.insert(id);
        assert_eq!(
            rows(&view(&only, None))[4],
            ("Tests", &RowStatus::Ok),
            "an area whose every block was settled was still checked"
        );
    }

    #[test]
    fn a_stale_result_marks_its_findings_stale() {
        let mut m = model();
        m.audit.state = CheckState::Stale;
        let d = view(&m, Some("concurrency")).detail.unwrap();
        assert!(d.findings.iter().all(|f| f.stale));
        let d = view(&model(), Some("concurrency")).detail.unwrap();
        assert!(d.findings.iter().all(|f| !f.stale));
    }

    #[test]
    fn an_unanchored_finding_proposes_a_comment_that_names_its_place() {
        let mut m = model();
        m.audit.result.as_mut().unwrap().findings[1].anchored = false;
        let d = view(&m, Some("concurrency")).detail.unwrap();
        assert!(
            d.findings[0]
                .comment
                .starts_with("src/auth/refresh.rs:44: ")
        );
    }

    #[test]
    fn every_area_off_says_so_instead_of_offering_a_run() {
        let mut areas = default_areas();
        for a in &mut areas {
            a.enabled = false;
        }
        let v = audits_view(Some(&read()), &areas, true, &FILES, NOW, None, None, false);
        let n = v.notice.expect("a notice");
        assert_eq!(
            n.line,
            "No audit areas are switched on. Switch one on in Config › Harness."
        );
        assert!(n.setup && n.action.is_none());
        let v = audits_view(
            Some(&read()),
            &default_areas(),
            true,
            &FILES,
            NOW,
            None,
            None,
            false,
        );
        assert_eq!(v.notice.unwrap().action, Some(CheckAction::Run));
    }

    #[test]
    fn accepted_busy_and_unanchored_findings_say_so() {
        let mut m = model();
        let id = m.findings(CheckKind::Audit)[1].id.clone();
        m.accepted.insert(id.clone());
        m.audit.result.as_mut().unwrap().findings[1].anchored = false;
        let d = view(&m, Some("concurrency")).detail.unwrap();
        assert!(d.findings[0].accepted && d.findings[0].general);
        m.accepted.clear();
        m.pending.insert(id.clone(), true);
        let d = view(&m, Some("concurrency")).detail.unwrap();
        assert!(d.findings[0].busy && !d.findings[0].dismissing);
        m.pending.insert(id, false);
        let d = view(&m, Some("concurrency")).detail.unwrap();
        assert!(d.findings[0].busy && d.findings[0].dismissing);
    }

    #[test]
    fn a_removed_area_shows_its_id_and_a_renamed_one_its_name() {
        let mut areas: Vec<AuditArea> = default_areas();
        areas.retain(|a| a.id != "docs");
        areas[0].name = "Is it right?".into();
        let v = audits_view(Some(&model()), &areas, true, &FILES, NOW, None, None, false);
        assert_eq!(v.rows[0].name, "Is it right?");
        assert_eq!(v.rows[5].name, "docs");
    }

    #[test]
    fn stale_and_unreadable_results_say_so_above_the_detail() {
        let mut m = model();
        m.audit.state = CheckState::Stale;
        m.audit.result.as_mut().unwrap().unreadable = 2;
        let v = view(&m, None);
        assert_eq!(
            v.banner.as_deref(),
            Some(
                "Checked an older commit, 2 minutes ago. Check again to add these to your draft. 2 results could not be read."
            )
        );
        assert_eq!(view(&model(), None).banner, None);
    }

    #[test]
    fn untrusted_text_is_made_printable() {
        let mut m = model();
        let f = &mut m.audit.result.as_mut().unwrap().findings[0];
        f.title = "Fine\u{1b}[2J".into();
        f.body = "one\ntwo\u{202e}".into();
        f.comment = "Please\u{7} fix".into();
        m.audit.result.as_mut().unwrap().passes[0].text = "ok\u{1b}[0m".into();
        let mut areas = default_areas();
        areas[0].name = "Cor\u{1b}rectness".into();
        let v = audits_view(
            Some(&m),
            &areas,
            true,
            &FILES,
            NOW,
            Some("correctness"),
            None,
            false,
        );
        let d = v.detail.unwrap();
        let f = &d.findings[0];
        for text in [&f.title, &f.body, &f.comment, &v.rows[0].name] {
            assert!(
                !text.contains('\u{1b}') && !text.contains('\u{202e}') && !text.contains('\u{7}'),
                "{text:?}"
            );
        }
        assert!(f.body.contains('\n'));
    }

    #[test]
    fn states_without_a_result_show_the_notice_and_no_list() {
        let v = audits_view(
            Some(&read()),
            &default_areas(),
            false,
            &FILES,
            NOW,
            None,
            None,
            false,
        );
        let n = v.notice.expect("a notice");
        assert_eq!(n.line, "Set up a harness in Config › Harness");
        assert!(v.rows.is_empty() && v.detail.is_none());
        let v = audits_view(
            Some(&read()),
            &default_areas(),
            true,
            &FILES,
            NOW,
            None,
            None,
            false,
        );
        assert_eq!(v.notice.unwrap().action, Some(CheckAction::Run));
        let mut m = read();
        m.audit.state = CheckState::Running {
            activity: Some("Reading tests/refresh.rs…".into()),
        };
        let v = audits_view(
            Some(&m),
            &default_areas(),
            true,
            &FILES,
            NOW,
            None,
            None,
            false,
        );
        assert_eq!(v.notice.unwrap().line, "Reading tests/refresh.rs…");
    }

    fn open_audits(app: &mut App) -> PrRef {
        let pr = testing::open_ready(app, false);
        testing::open_checks(app, &pr);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .audit_area = Some("concurrency".into());
        testing::set_section(app, &pr, ReviewSection::Audits);
        pr
    }

    fn concurrency_id() -> String {
        fixture::demo_checks(NOW).0[1]
            .findings
            .iter()
            .find(|f| f.area == "concurrency")
            .unwrap()
            .id
            .clone()
    }

    fn scroll_of<T: Component>(app: &mut App) -> f32 {
        let mut q = app.world_mut().query_filtered::<&ScrollPosition, With<T>>();
        let positions: Vec<f32> = q.iter(app.world()).map(|p| p.y).collect();
        assert_eq!(positions.len(), 1, "one pane");
        positions[0]
    }

    fn scroll_to<T: Component>(app: &mut App, y: f32) {
        let mut q = app
            .world_mut()
            .query_filtered::<&mut ScrollPosition, With<T>>();
        for mut p in q.iter_mut(app.world_mut()) {
            p.y = y;
        }
    }

    #[test]
    fn a_rebuild_keeps_where_both_panes_were_scrolled() {
        let mut app = testing::app(fixture::demo(NOW));
        open_audits(&mut app);
        scroll_to::<AuditDetail>(&mut app, 240.0);
        scroll_to::<AreaList>(&mut app, 90.0);
        let id = concurrency_id();
        let edit = testing::find::<FindingEdit>(&mut app, |b| b.id == id);
        testing::activate(&mut app, edit);
        testing::settle(&mut app);
        assert_eq!(
            testing::count::<EditorSubmit>(&mut app),
            1,
            "rebuilt with the editor"
        );
        assert_eq!(scroll_of::<AuditDetail>(&mut app), 240.0);
        assert_eq!(scroll_of::<AreaList>(&mut app), 90.0);
    }

    #[test]
    fn ten_areas_scroll_in_their_list_above_the_footer() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_audits(&mut app);
        let ids: Vec<String> = (1..=10).map(|n| format!("area-{n}")).collect();
        app.world_mut()
            .resource_mut::<Checks>()
            .0
            .get_mut(&pr)
            .unwrap()
            .audit
            .result
            .as_mut()
            .unwrap()
            .areas = ids;
        testing::settle(&mut app);
        assert_eq!(testing::count::<AreaButton>(&mut app), 10);
        let mut lists = app
            .world_mut()
            .query_filtered::<(&Node, &ChildOf), (With<AreaList>, With<ScrollArea>)>();
        let (list, parent) = lists.single(app.world()).expect("one scrolling area list");
        assert_eq!(list.overflow.y, OverflowAxis::Scroll);
        assert_eq!(list.min_height, px(0));
        assert!(list.flex_grow > 0.0);
        let column = app.world().get::<Node>(parent.parent()).unwrap();
        assert_eq!(
            column.min_height,
            px(0),
            "the column may shrink to the pane"
        );
        assert!(testing::shows(
            &mut app,
            "Run by Claude Code with your audit areas. Findings the agent proposes wait for your OK before they join the draft."
        ));
    }

    #[test]
    fn the_audits_tab_matches_the_mockup() {
        let mut app = testing::app(fixture::demo(NOW));
        open_audits(&mut app);
        for needle in [
            "AUDIT · 6 AREAS",
            "Correctness",
            "Concurrency",
            "Error handling",
            "Performance",
            "Tests",
            "Docs and changelog",
            "ok",
            "Run by Claude Code with your audit areas. Findings the agent proposes wait for your OK before they join the draft.",
            "Ask the agent about this",
            "Needs your OK",
            "The refresh lock is held across a network call",
            "src/auth/refresh.rs:44",
            "Proposed comment",
            "Could we re-check the expiry after taking the lock",
            "Accept into draft",
            "Edit first",
            "Dismiss",
            "No shared state written outside the lock",
            "checked 4 call sites",
            "Check again",
        ] {
            assert!(testing::shows(&mut app, needle), "shows {needle}");
        }
        assert!(
            !testing::shows(&mut app, "Arrives with the harness"),
            "no placeholder"
        );
    }

    #[test]
    fn a_click_on_an_area_selects_it() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_audits(&mut app);
        let tests = testing::find::<AreaButton>(&mut app, |b| b.area == "tests");
        testing::activate(&mut app, tests);
        testing::settle(&mut app);
        assert_eq!(
            testing::tab(&app, &pr).ui.audit_area.as_deref(),
            Some("tests")
        );
        assert!(testing::shows(
            &mut app,
            "The retry after a revoked token has no test"
        ));
        assert!(!testing::shows(&mut app, "The refresh lock is held across"));
        let docs = testing::find::<AreaButton>(&mut app, |b| b.area == "docs");
        testing::activate(&mut app, docs);
        testing::settle(&mut app);
        assert!(testing::shows(
            &mut app,
            "The changelog names the early refresh"
        ));
    }

    #[test]
    fn asking_the_agent_about_an_area_fills_the_chat_input() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_audits(&mut app);
        let ask = testing::find::<AskAreaButton>(&mut app, |b| b.pr == pr);
        testing::activate(&mut app, ask);
        testing::settle(&mut app);
        let input = testing::find::<ChatInput>(&mut app, |i| i.0 == pr);
        assert_eq!(
            app.world()
                .get::<EditableText>(input)
                .unwrap()
                .value()
                .to_string(),
            "About the Concurrency audit: "
        );
        assert_eq!(app.world().resource::<Chats>().0[&pr].tab, PanelTab::Agent);
        assert!(testing::recorded(&mut app).is_empty(), "nothing is sent");
    }

    #[test]
    fn accept_asks_and_the_card_waits_then_says_it_is_in_the_draft() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_audits(&mut app);
        let id = concurrency_id();
        let accept = testing::find::<FindingAccept>(&mut app, |b| b.id == id);
        testing::activate(&mut app, accept);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::AcceptFinding {
                pr: pr.clone(),
                id: id.clone(),
                body: None
            }]
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "Adding…"));
        testing::tell(
            &mut app,
            Tell::Checks(ChecksTell::Settled {
                pr,
                id,
                accepted: true,
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "✓ In your draft"));
        assert!(!testing::shows(&mut app, "Needs your OK"));
    }

    #[test]
    fn dismiss_asks_and_the_card_goes() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_audits(&mut app);
        let id = concurrency_id();
        let dismiss = testing::find::<FindingDismiss>(&mut app, |b| b.id == id);
        testing::activate(&mut app, dismiss);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::DismissFinding {
                pr: pr.clone(),
                id: id.clone()
            }]
        );
        testing::tell(
            &mut app,
            Tell::Checks(ChecksTell::Settled {
                pr,
                id,
                accepted: false,
            }),
        );
        testing::settle(&mut app);
        assert!(!testing::shows(&mut app, "The refresh lock is held across"));
        assert!(testing::shows(
            &mut app,
            "No shared state written outside the lock"
        ));
    }

    #[test]
    fn edit_first_draws_the_composer_under_the_card() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_audits(&mut app);
        let id = concurrency_id();
        let edit = testing::find::<FindingEdit>(&mut app, |b| b.id == id);
        testing::activate(&mut app, edit);
        testing::settle(&mut app);
        let editor = testing::tab(&app, &pr).ui.editor.expect("an editor");
        assert_eq!(editor.target, EditTarget::Finding(id.clone()));
        assert!(editor.text.starts_with("Could we re-check the expiry"));
        assert_eq!(
            testing::count::<FindingAccept>(&mut app),
            0,
            "the card's buttons give way to the editor"
        );
        let area = testing::find::<EditorArea>(&mut app, |_| true);
        testing::type_into(&mut app, area, " Thanks!");
        let submit = testing::find::<EditorSubmit>(&mut app, |_| true);
        testing::activate(&mut app, submit);
        let asks = testing::recorded(&mut app);
        let [
            Ask::AcceptFinding {
                id: got,
                body: Some(body),
                ..
            },
        ] = asks.as_slice()
        else {
            panic!("one accept with the edited text: {asks:?}");
        };
        assert_eq!(got, &id);
        assert!(body.ends_with(" Thanks!"));
    }

    #[test]
    fn the_footer_counts_what_waits_for_an_ok() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_audits(&mut app);
        assert!(testing::shows(
            &mut app,
            "Draft saved · 3 items · 3 waiting for your OK"
        ));
        testing::tell(
            &mut app,
            Tell::Checks(ChecksTell::Settled {
                pr: pr.clone(),
                id: concurrency_id(),
                accepted: true,
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "3 items · 2 waiting for your OK"));
        let ids: Vec<String> = fixture::demo_checks(NOW).0[1]
            .findings
            .iter()
            .map(|f| f.id.clone())
            .collect();
        for id in [&ids[0], &ids[2]] {
            testing::tell(
                &mut app,
                Tell::Checks(ChecksTell::Settled {
                    pr: pr.clone(),
                    id: id.clone(),
                    accepted: false,
                }),
            );
        }
        testing::settle(&mut app);
        assert!(!testing::shows(&mut app, "waiting for your OK"));
    }

    #[test]
    fn a_kind_that_never_ran_offers_the_run_button() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = testing::open_ready(&mut app, false);
        // The demo has no harness program; the tab offers a run only once Claude Code is set up.
        testing::set_config_locally(&mut app, "harness.program", "/opt/homebrew/bin/claude");
        testing::tell(
            &mut app,
            Tell::Checks(ChecksTell::Loaded {
                pr: pr.clone(),
                results: vec![],
                states: vec![],
                accepted: vec![],
                dismissed: vec![],
            }),
        );
        testing::set_section(&mut app, &pr, ReviewSection::Audits);
        assert!(testing::shows(&mut app, "Run audit"));
        assert!(!testing::shows(&mut app, "AUDIT · "));
        let run = testing::find::<CheckButton>(&mut app, |b| b.action == CheckAction::Run);
        testing::activate(&mut app, run);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::RunCheck {
                pr,
                kind: CheckKind::Audit
            }]
        );
    }

    #[test]
    fn a_running_audit_shows_its_activity_and_stops() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_audits(&mut app);
        testing::tell(
            &mut app,
            Tell::Checks(ChecksTell::State {
                pr: pr.clone(),
                kind: CheckKind::Audit,
                state: CheckState::Running {
                    activity: Some("Reading tests/refresh.rs…".into()),
                },
            }),
        );
        testing::settle(&mut app);
        assert!(testing::shows(&mut app, "Reading tests/refresh.rs…"));
        assert!(testing::shows(&mut app, "Auditing the change…"));
        let stop = testing::find::<CheckButton>(&mut app, |b| b.action == CheckAction::Stop);
        testing::activate(&mut app, stop);
        assert_eq!(
            testing::recorded(&mut app),
            [Ask::StopCheck {
                pr,
                kind: CheckKind::Audit
            }]
        );
    }

    #[test]
    fn a_stale_card_offers_only_dismiss_and_show_in_diff() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_audits(&mut app);
        testing::tell(
            &mut app,
            Tell::Checks(ChecksTell::State {
                pr,
                kind: CheckKind::Audit,
                state: CheckState::Stale,
            }),
        );
        testing::settle(&mut app);
        assert_eq!(testing::count::<FindingAccept>(&mut app), 0);
        assert_eq!(testing::count::<FindingEdit>(&mut app), 0);
        assert_eq!(testing::count::<FindingDismiss>(&mut app), 1);
        assert_eq!(testing::count::<FindingShow>(&mut app), 1);
    }

    #[test]
    fn show_in_diff_opens_the_file() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_audits(&mut app);
        let show = testing::find::<FindingShow>(&mut app, |b| b.file == "src/auth/refresh.rs");
        testing::activate(&mut app, show);
        let ui = testing::tab(&app, &pr).ui;
        assert_eq!(ui.section, ReviewSection::Diff);
        assert_eq!(ui.file.as_deref(), Some("src/auth/refresh.rs"));
    }

    #[test]
    fn switching_sections_replaces_the_tab() {
        let mut app = testing::app(fixture::demo(NOW));
        let pr = open_audits(&mut app);
        testing::set_section(&mut app, &pr, ReviewSection::Security);
        assert!(testing::shows(&mut app, "3 security findings"));
        assert!(!testing::shows(&mut app, "AUDIT · 6 AREAS"));
        testing::set_section(&mut app, &pr, ReviewSection::Audits);
        assert!(testing::shows(&mut app, "AUDIT · 6 AREAS"));
        assert!(!testing::shows(&mut app, "3 security findings"));
    }
}
