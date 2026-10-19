//! The Diff's Calls view: a changed function in a top-down call graph. `model` holds what the
//! daemon said; the view itself fills `CallsRoot`.

use bevy::prelude::*;
use clusia_calls::{CallIndex, ChangeKind, ChangedFunction};
use clusia_core::PrRef;
use clusia_core::Review;
use clusia_core::commentable_lines;
use clusia_core::draft::{Anchor, DraftKind, Origin, Side};
use clusia_view::diff::{RowKind, parse_patch};

use crate::review_state::TabUi;

pub mod model;

pub use model::{Calls, CallsModel, CallsRequest};

/// The Calls frame's root inside the section body.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct CallsRoot(pub PrRef);

pub struct CallsPlugin;

impl Plugin for CallsPlugin {
    fn build(&self, _app: &mut App) {}
}

/// The Calls frame: an empty root that the view fills.
pub(crate) fn frame(p: &mut bevy::ecs::hierarchy::ChildSpawnerCommands, pr: &PrRef) {
    p.spawn((
        Node {
            flex_grow: 1.0,
            min_width: px(0),
            min_height: px(0),
            ..default()
        },
        CallsRoot(pr.clone()),
    ));
}

/// The status bar's middle text while the Calls view shows; `None` keeps the draft summary.
pub(crate) fn status_text(
    _ui: &TabUi,
    _model: Option<&CallsModel>,
    _draft: &str,
) -> Option<String> {
    None
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

    #[test]
    fn the_calls_status_is_left_to_the_draft_summary_until_the_view_writes_one() {
        assert_eq!(
            status_text(&TabUi::default(), None, "Draft saved · 4 items"),
            None
        );
    }
}
