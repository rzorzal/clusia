//! The Diff's Calls view: a changed function in a top-down call graph, with the functions of the
//! pull request on the left and the selected node's callers on the right. `model` holds what the
//! daemon said; the systems here ask for it, and `list`, `canvas`, `panel` and `full` draw it.
//!
//! What the view shows (the centre, the selected node, the expanded levels, the depth) lives in
//! `TabUi`, so the canvas is redrawn only when the daemon sends another graph.

use std::collections::HashSet;

use bevy::prelude::*;
use bevy::text::EditableText;
use clusia_calls::{CallIndex, ChangeKind, ChangedFunction};
use clusia_core::PrRef;
use clusia_core::Review;
use clusia_core::commentable_lines;
use clusia_core::config::CALLS_DEPTH_RANGE;
use clusia_core::draft::{Anchor, DraftKind, Origin, Side};
use clusia_protocol::CallsIndexState;
use clusia_view::diff::{RowKind, parse_patch};

use crate::bridge::{Ask, Asks, Connection, Model};
use crate::review_state::{DiffMode, EditTarget, ReviewSection, ReviewTabs, TabUi};
use crate::screens::review::ReviewSystems;

pub mod canvas;
pub mod full;
pub mod graph;
pub mod list;
pub mod model;
pub mod panel;
#[cfg(test)]
mod testkit;

pub use canvas::{CallsCanvas, CardClose, CardFull, DepthButton, DepthItem, ExpansionCard};
pub use full::{FullCancel, FullModal};
pub use list::{FindField, LeftRow};
pub use model::{Calls, CallsModel, CallsRequest};
pub use panel::{PanelAction, PanelButton, UnsupportedAction, UnsupportedWhat};

/// The Calls frame's root inside the section body.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CallsRoot(pub PrRef);

pub struct CallsPlugin;

impl Plugin for CallsPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (
                load_calls,
                keep_find,
                ask_codes,
                keep_selection,
                list::fill_left,
                list::sync_hint,
                canvas::fill_canvas,
                canvas::place_card,
                canvas::outline_selected,
                panel::fill_panel,
                full::open_full,
                full::rebuild_full,
                full::close_when_done,
                full::escape_closes,
            )
                .chain()
                .after(ReviewSystems)
                .after(crate::screens::review::diff::fill_region),
        );
    }
}

/// The Calls frame: the left column, the canvas and the side panel.
pub(crate) fn frame(p: &mut bevy::ecs::hierarchy::ChildSpawnerCommands, pr: &PrRef) {
    p.spawn((
        Node {
            flex_grow: 1.0,
            min_width: px(0),
            min_height: px(0),
            ..default()
        },
        CallsRoot(pr.clone()),
    ))
    .with_children(|root| {
        list::column(root, pr);
        root.spawn((
            Node {
                flex_grow: 1.0,
                min_width: px(0),
                min_height: px(0),
                overflow: Overflow::clip(),
                ..default()
            },
            CallsCanvas(pr.clone()),
        ));
        panel::column(root, pr);
    });
}

/// The depth the tab's graph is drawn at: its own choice, else the config's, within 1–5.
pub fn effective_depth(ui: &TabUi, config: u8) -> u8 {
    ui.calls_depth
        .unwrap_or(config)
        .clamp(*CALLS_DEPTH_RANGE.start(), *CALLS_DEPTH_RANGE.end())
}

/// The `GetCalls` the tab wants answered.
pub fn wanted_request(ui: &TabUi, config_depth: u8) -> CallsRequest {
    CallsRequest {
        center: ui.calls_center.clone(),
        depth: effective_depth(ui, config_depth),
        expand: ui.calls_expand.clone(),
    }
}

/// Whether the tab shows the Calls view: the Diff section in the Calls mode.
fn shows_calls(ui: &TabUi) -> bool {
    ui.section == ReviewSection::Diff && ui.mode == DiffMode::Calls
}

/// The status bar's middle text while the Calls view shows: what the graph was drawn from, or
/// that it is being built; `None` keeps the draft summary.
pub fn status_text(ui: &TabUi, model: Option<&CallsModel>, _draft: &str) -> Option<String> {
    if !shows_calls(ui) {
        return None;
    }
    let m = model?;
    if let Some(g) = &m.graph {
        if g.cut {
            return Some("Partial: stopped after 10 s".into());
        }
        let at = match &m.index {
            CallsIndexState::Ready { commit, .. } => {
                format!(" at {}", commit.get(..7).unwrap_or(commit))
            }
            _ => String::new(),
        };
        return Some(format!(
            "Call graph from the worktree{at} · {} changed, {} touched by calls · depth {}",
            g.changed, g.touched, g.depth
        ));
    }
    if !m.unsupported.is_empty() {
        return Some(
            "Call graph from the worktree · Rust, TypeScript, JavaScript and Python".into(),
        );
    }
    (m.index == CallsIndexState::Building).then(|| "Building the call graph…".into())
}

/// Asks for the graph each tab that shows the Calls view wants, once per request: when it shows
/// another request, or when the model was told to read it again (a finished index, a
/// reconnect). Asks once per tab for the changed functions (the empty search). Forgets the
/// models of closed tabs.
fn load_calls(
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut calls: ResMut<Calls>,
    mut asks: ResMut<Asks>,
    mut found: Local<HashSet<PrRef>>,
) {
    if calls.0.keys().any(|pr| !tabs.0.contains_key(pr)) {
        calls.0.retain(|pr, _| tabs.0.contains_key(pr));
    }
    found.retain(|pr| tabs.0.contains_key(pr));
    if model.connection != Connection::Live {
        found.clear();
        return;
    }
    let config_depth = model.snapshot.config.diff.calls_depth;
    for (pr, tab) in &tabs.0 {
        if !shows_calls(&tab.ui) || !tab.ready().is_some_and(|r| r.cached_at.is_none()) {
            continue;
        }
        let want = wanted_request(&tab.ui, config_depth);
        let head = tab.ready().map_or("", |r| r.view.review.head_sha.as_str());
        let stale = calls.0.get(pr).is_none_or(|m| {
            !m.asked
                || m.shown.as_ref() != Some(&want)
                || (!m.asked_head.is_empty() && m.asked_head != head)
        });
        let asking = calls
            .0
            .get(pr)
            .is_some_and(|m| m.asking.as_ref() == Some(&want));
        if stale && !asking {
            let m = calls.0.entry(pr.clone()).or_default();
            m.asking = Some(want.clone());
            m.asked_head = head.to_string();
            asks.send(Ask::GetCalls {
                pr: pr.clone(),
                center: want.center,
                depth: Some(want.depth),
                expand: want.expand,
            });
        }
        if found.insert(pr.clone()) {
            let query = calls.0.get(pr).map(|m| m.find.clone()).unwrap_or_default();
            asks.send(Ask::FindFunction {
                pr: pr.clone(),
                query,
            });
        }
    }
}

/// Searches again each time the find box changes.
fn keep_find(
    fields: Query<(&list::FindField, &EditableText), Changed<EditableText>>,
    model: Res<Model>,
    mut calls: ResMut<Calls>,
    mut asks: ResMut<Asks>,
) {
    for (field, editable) in &fields {
        let query = editable.value().to_string();
        let same = calls.0.get(&field.0).is_some_and(|m| m.find == query)
            || (query.is_empty() && !calls.0.contains_key(&field.0));
        if same {
            continue;
        }
        calls.0.entry(field.0.clone()).or_default().find = query.clone();
        if model.connection == Connection::Live {
            asks.send(Ask::FindFunction {
                pr: field.0.clone(),
                query,
            });
        }
    }
}

/// Asks for the code the card and the panel show: the subject (the selected node, else the
/// centre) and its nearest caller, each once until it arrives.
fn ask_codes(
    tabs: Res<ReviewTabs>,
    model: Res<Model>,
    mut calls: ResMut<Calls>,
    mut asks: ResMut<Asks>,
) {
    if model.connection != Connection::Live {
        return;
    }
    for (pr, tab) in &tabs.0 {
        if !shows_calls(&tab.ui) {
            continue;
        }
        let Some(m) = calls.0.get(pr) else { continue };
        let Some(g) = &m.graph else { continue };
        let subject = tab
            .ui
            .calls_node
            .as_deref()
            .filter(|k| g.nodes.iter().any(|n| n.key == *k))
            .unwrap_or(&g.center);
        // An outside node shows no caller's code.
        let outside = g
            .nodes
            .iter()
            .any(|n| n.key == subject && n.kind == clusia_calls::NodeKind::Outside);
        let caller = graph::nearest_caller(g, subject)
            .filter(|_| !outside)
            .map(|(n, _)| n.key.as_str());
        let wanted: Vec<String> = std::iter::once(subject)
            .chain(caller)
            .filter_map(|key| g.nodes.iter().find(|n| n.key == key))
            .filter_map(|n| n.function.as_ref())
            .map(|f| f.0.clone())
            .filter(|f| !m.codes.contains_key(f) && !m.requested_code.contains(f))
            .collect();
        if wanted.is_empty() {
            continue;
        }
        let Some(m) = calls.0.get_mut(pr) else {
            continue;
        };
        for function in wanted {
            m.requested_code.insert(function.clone());
            asks.send(Ask::ReadFunctionCode {
                pr: pr.clone(),
                function,
            });
        }
    }
}

/// Drops the selection a new graph no longer holds (`graph::kept_selection`).
fn keep_selection(calls: Res<Calls>, mut tabs: ResMut<ReviewTabs>) {
    if !calls.is_changed() {
        return;
    }
    let mut dropped = Vec::new();
    for (pr, tab) in &tabs.0 {
        let Some(m) = calls.0.get(pr) else { continue };
        let Some(g) = &m.graph else { continue };
        let editing = match tab.ui.editor.as_ref().map(|e| &e.target) {
            Some(EditTarget::CallsNode { function }) => Some(function.as_str()),
            _ => None,
        };
        let kept = graph::kept_selection(&tab.ui.calls_node, g, &m.unsupported, editing);
        if kept != tab.ui.calls_node {
            dropped.push((pr.clone(), kept));
        }
    }
    for (pr, kept) in dropped {
        if let Some(tab) = tabs.0.get_mut(&pr) {
            tab.ui.calls_node = kept;
        }
    }
}

/// What the demo daemon does with a comment on a function: a line comment on the first line the
/// patch adds inside a changed function, else a general comment that names `path:lines`.
pub(crate) fn demo_node_comment(
    review: &mut Review,
    index: &CallIndex,
    changed: &[ChangedFunction],
    function: &str,
    patch: Option<&str>,
    body: &str,
    now: i64,
) -> Result<(), String> {
    let Some(f) = index.functions.iter().find(|f| f.id.0 == function) else {
        return review
            .draft
            .add_as(Origin::Human, DraftKind::General, None, None, body, now)
            .map(|_| ())
            .map_err(|e| e.to_string());
    };
    let touched = changed
        .iter()
        .any(|c| c.id == f.id && c.kind != ChangeKind::Removed);
    let first_added = patch.filter(|_| touched).and_then(|p| {
        let (_, right) = commentable_lines(p);
        parse_patch(p)
            .iter()
            .filter(|r| r.kind == RowKind::Added)
            .filter_map(|r| r.new)
            .find(|n| (f.start_line..=f.end_line).contains(n) && right.contains(n))
    });
    let added = match first_added {
        Some(line) => review.draft.add_as(
            Origin::Human,
            DraftKind::LineComment,
            Some(Anchor {
                commit: review.head_sha.clone(),
                path: f.path.clone(),
                line,
                start_line: None,
                side: Side::Right,
            }),
            None,
            body,
            now,
        ),
        None => review.draft.add_as(
            Origin::Human,
            DraftKind::General,
            None,
            None,
            &format!("{}:{}-{}: {body}", f.path, f.start_line, f.end_line),
            now,
        ),
    };
    added.map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::model::{CallsModel, CallsRequest};
    use super::testkit::sample;
    use clusia_protocol::CallsIndexState;

    use super::*;
    use crate::fixture;
    use crate::testing::NOW;
    use clusia_core::draft::{DraftKind, Origin, Side};

    fn patch_of(path: &str) -> Option<String> {
        let (view, _) = fixture::demo_review(NOW);
        view.diff
            .into_iter()
            .find(|f| f.path == path)
            .and_then(|f| f.patch)
    }

    fn id_of(index: &CallIndex, path: &str, name: &str) -> String {
        index
            .functions
            .iter()
            .find(|f| f.path == path && f.name == name)
            .unwrap_or_else(|| panic!("{path} {name}"))
            .id
            .0
            .clone()
    }

    #[test]
    fn a_comment_on_a_changed_function_is_a_line_comment_on_its_first_added_line() {
        let index = fixture::demo_calls();
        let changed = fixture::demo_changed(&index);
        let function = id_of(&index, "src/auth/refresh.rs", "refresh");
        let (view, _) = fixture::demo_review(NOW);
        let mut review = view.review;
        demo_node_comment(
            &mut review,
            &index,
            &changed,
            &function,
            patch_of("src/auth/refresh.rs").as_deref(),
            "Who else calls this?",
            NOW,
        )
        .expect("saved");
        let item = review.draft.items.last().expect("an item");
        assert_eq!(
            (item.kind, item.origin),
            (DraftKind::LineComment, Origin::Human)
        );
        let anchor = item.anchor.as_ref().expect("an anchor");
        assert_eq!(
            (anchor.path.as_str(), anchor.line, anchor.side),
            ("src/auth/refresh.rs", 39, Side::Right)
        );
        assert_eq!(item.body, "Who else calls this?");
    }

    #[test]
    fn a_comment_on_an_untouched_function_is_general_and_names_its_lines() {
        let index = fixture::demo_calls();
        let changed = fixture::demo_changed(&index);
        let function = id_of(&index, "src/auth/store.rs", "cached");
        let f = index.functions.iter().find(|f| f.id.0 == function).unwrap();
        let (view, _) = fixture::demo_review(NOW);
        let mut review = view.review;
        demo_node_comment(
            &mut review,
            &index,
            &changed,
            &function,
            patch_of("src/auth/store.rs").as_deref(),
            "Is this still needed?",
            NOW,
        )
        .expect("saved");
        let item = review.draft.items.last().expect("an item");
        assert_eq!(
            (item.kind, item.origin),
            (DraftKind::General, Origin::Human)
        );
        assert!(item.anchor.is_none());
        assert_eq!(
            item.body,
            format!(
                "src/auth/store.rs:{}-{}: Is this still needed?",
                f.start_line, f.end_line
            )
        );
    }

    #[test]
    fn an_unknown_function_still_saves_the_words() {
        let index = fixture::demo_calls();
        let (view, _) = fixture::demo_review(NOW);
        let mut review = view.review;
        demo_node_comment(&mut review, &index, &[], "nowhere#x#1", None, "Hello", NOW)
            .expect("saved");
        assert_eq!(review.draft.items.last().expect("an item").body, "Hello");
    }

    fn calls() -> TabUi {
        TabUi {
            mode: DiffMode::Calls,
            ..TabUi::default()
        }
    }

    fn ready(commit: &str) -> CallsModel {
        CallsModel {
            graph: Some(sample()),
            index: CallsIndexState::Ready {
                commit: commit.into(),
                cut: false,
            },
            ..CallsModel::default()
        }
    }

    #[test]
    fn the_wanted_request_is_the_tabs_choice_over_the_configs_depth() {
        let mut ui = calls();
        assert_eq!(
            wanted_request(&ui, 3),
            CallsRequest {
                center: None,
                depth: 3,
                expand: vec![]
            }
        );
        ui.calls_center = Some("src/auth/store.rs#TokenStore::token#20".into());
        ui.calls_expand = vec![1, -1];
        ui.calls_depth = Some(5);
        assert_eq!(
            wanted_request(&ui, 3),
            CallsRequest {
                center: Some("src/auth/store.rs#TokenStore::token#20".into()),
                depth: 5,
                expand: vec![1, -1],
            }
        );
        ui.calls_depth = Some(9);
        assert_eq!(effective_depth(&ui, 3), 5, "never above 5");
        ui.calls_depth = Some(0);
        assert_eq!(effective_depth(&ui, 3), 1, "never under 1");
        assert_eq!(effective_depth(&calls(), 9), 5);
    }

    #[test]
    fn the_status_bar_names_the_commit_the_counts_and_the_depth() {
        let m = ready("9b0c7de4a1c2e3f4");
        assert_eq!(
            status_text(&calls(), Some(&m), "Draft saved · 2 items").as_deref(),
            Some(
                "Call graph from the worktree at 9b0c7de · 4 changed, 21 touched by calls · depth 3"
            )
        );
    }

    #[test]
    fn a_cut_index_says_partial() {
        let mut m = ready("9b0c7de4a1c2e3f4");
        m.graph.as_mut().unwrap().cut = true;
        assert_eq!(
            status_text(&calls(), Some(&m), "").as_deref(),
            Some("Partial: stopped after 10 s")
        );
    }

    #[test]
    fn unsupported_files_alone_name_the_languages_and_a_build_says_so() {
        let m = CallsModel {
            unsupported: vec!["server/handler.go".into()],
            index: CallsIndexState::Ready {
                commit: "9b0c7de4a1c2".into(),
                cut: false,
            },
            ..CallsModel::default()
        };
        assert_eq!(
            status_text(&calls(), Some(&m), "").as_deref(),
            Some("Call graph from the worktree · Rust, TypeScript, JavaScript and Python")
        );
        let building = CallsModel {
            index: CallsIndexState::Building,
            ..CallsModel::default()
        };
        assert_eq!(
            status_text(&calls(), Some(&building), "").as_deref(),
            Some("Building the call graph…")
        );
    }

    #[test]
    fn the_draft_text_stays_outside_calls_or_without_an_answer() {
        let m = ready("9b0c7de4a1c2e3f4");
        assert_eq!(
            status_text(&TabUi::default(), Some(&m), "Draft saved"),
            None
        );
        assert_eq!(status_text(&calls(), None, "Draft saved"), None);
        assert_eq!(
            status_text(&calls(), Some(&CallsModel::default()), "Draft saved"),
            None
        );
        let mut other = calls();
        other.section = crate::review_state::ReviewSection::Comments;
        assert_eq!(
            status_text(&other, Some(&m), ""),
            None,
            "only on the Diff section"
        );
    }

    #[test]
    fn a_new_head_asks_for_the_graph_again() {
        let mut app = crate::testing::app(fixture::demo(NOW));
        let pr = crate::testing::open_ready(&mut app, false);
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ui
            .mode = crate::review_state::DiffMode::Calls;
        crate::testing::settle(&mut app);
        let graphs = |app: &mut App| {
            crate::testing::recorded(app)
                .iter()
                .filter(|a| matches!(a, Ask::GetCalls { .. }))
                .count()
        };
        assert_eq!(graphs(&mut app), 1);
        crate::testing::tell(&mut app, fixture::demo_calls_graph(None, 3, &[]));
        crate::testing::settle(&mut app);
        assert_eq!(graphs(&mut app), 0, "the graph matches the head");
        app.world_mut()
            .resource_mut::<ReviewTabs>()
            .0
            .get_mut(&pr)
            .unwrap()
            .ready_mut()
            .unwrap()
            .view
            .review
            .head_sha = "aaaaaaa3a2f4e5d6c7b8a9f0e1d2c3b4a5f6e7d0".into();
        crate::testing::settle(&mut app);
        assert_eq!(graphs(&mut app), 1, "the head moved");
    }
}
