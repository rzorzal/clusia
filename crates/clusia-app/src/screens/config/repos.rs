//! Config › Repositories: folders to scan for clones, worktree retention and location.

use std::collections::HashMap;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, observe};
use clusia_core::Paths;

use super::{FieldError, field_row, heading, page_header, row};
use crate::bridge::{Asks, Model, set_config};
use crate::fonts::UiFonts;
use crate::snapshot::Snapshot;
use crate::theme::Swatch;
use crate::ui::kit::{FieldCommitted, Type, Variant, button, card, text, text_field};

/// The empty field that adds a folder on Enter.
#[derive(Component, Debug)]
pub struct AddRoot;

#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct RemoveRoot(pub String);

#[derive(Debug, Clone, PartialEq)]
pub struct ReposView {
    pub roots: Vec<String>,
    pub roots_error: Option<String>,
    pub retention: String,
    pub retention_error: Option<String>,
    pub worktrees: String,
}

pub fn view(snap: &Snapshot, rejected: &HashMap<String, String>, paths: &Paths) -> ReposView {
    let r = &snap.config.repositories;
    ReposView {
        roots: r.roots.clone(),
        roots_error: rejected.get("repositories.roots").cloned(),
        retention: r.worktree_retention_days.to_string(),
        retention_error: rejected
            .get("repositories.worktree_retention_days")
            .cloned(),
        worktrees: paths.worktrees_dir().display().to_string(),
    }
}

/// `roots` plus `new` (trimmed); `None` when empty or already there.
pub fn with_root(roots: &[String], new: &str) -> Option<Vec<String>> {
    let new = new.trim();
    if new.is_empty() || roots.iter().any(|r| r == new) {
        return None;
    }
    let mut v = roots.to_vec();
    v.push(new.to_string());
    Some(v)
}

pub fn without_root(roots: &[String], gone: &str) -> Vec<String> {
    roots.iter().filter(|r| *r != gone).cloned().collect()
}

/// The list as a config value (a JSON string array is a valid TOML array).
pub fn roots_value(roots: &[String]) -> String {
    serde_json::to_string(roots).expect("a list of strings serializes")
}

pub fn build(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &ReposView) {
    page_header(
        p,
        fonts,
        "Repositories",
        "Where Clúsia looks for your clones. It never checks out branches in them: each review gets its own worktree.",
    );
    heading(p, fonts, "Folders to scan");
    p.spawn(card(Node {
        width: px(640),
        flex_direction: FlexDirection::Column,
        ..default()
    }))
    .with_children(|c| {
        for root in &v.roots {
            c.spawn(Node {
                padding: UiRect::axes(px(14), px(8)),
                align_items: AlignItems::Center,
                ..default()
            })
            .with_children(|r| {
                r.spawn((
                    Node {
                        flex_grow: 1.0,
                        ..default()
                    },
                    children![text(fonts, root.clone(), Type::MONO.ink(Swatch::Fg))],
                ));
                r.spawn((
                    button(fonts, "Remove", Variant::Ghost),
                    RemoveRoot(root.clone()),
                    observe(on_remove),
                ));
            });
        }
        c.spawn(Node {
            padding: UiRect::axes(px(14), px(10)),
            column_gap: px(10),
            align_items: AlignItems::Center,
            ..default()
        })
        .with_children(|r| {
            r.spawn((text_field(fonts, "", 360.0, true), AddRoot));
            r.spawn(text(fonts, "Type a folder and press Enter", Type::META));
        });
    });
    if let Some(message) = &v.roots_error {
        p.spawn((
            FieldError("repositories.roots"),
            children![text(fonts, message.clone(), Type::BODY.ink(Swatch::Orange))],
        ));
    }
    field_row(
        p,
        fonts,
        "Keep worktrees for",
        "repositories.worktree_retention_days",
        &v.retention,
        64.0,
        "days after a review closes",
        &v.retention_error,
    );
    row(
        p,
        fonts,
        "Worktrees live in",
        |r| {
            r.spawn(text(fonts, v.worktrees.clone(), Type::MONO));
        },
        "",
        None,
    );
}

/// The add field: Enter appends the folder.
pub fn commit_roots(
    mut commits: MessageReader<FieldCommitted>,
    adds: Query<(), With<AddRoot>>,
    mut asks: ResMut<Asks>,
    mut model: ResMut<Model>,
) {
    for c in commits.read() {
        if adds.get(c.entity).is_err() {
            continue;
        }
        if let Some(roots) = with_root(&model.snapshot.config.repositories.roots, &c.value) {
            set_config(
                &mut asks,
                &mut model,
                "repositories.roots",
                roots_value(&roots),
            );
        }
    }
}

fn on_remove(
    activate: On<Activate>,
    buttons: Query<&RemoveRoot>,
    mut asks: ResMut<Asks>,
    mut model: ResMut<Model>,
) {
    if let Ok(RemoveRoot(root)) = buttons.get(activate.entity) {
        let roots = without_root(&model.snapshot.config.repositories.roots, root);
        set_config(
            &mut asks,
            &mut model,
            "repositories.roots",
            roots_value(&roots),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_edit_helpers() {
        let roots = vec!["~/Repos".to_string(), "~/src".to_string()];
        assert_eq!(
            with_root(&roots, " ~/work "),
            Some(vec!["~/Repos".into(), "~/src".into(), "~/work".into()])
        );
        assert_eq!(with_root(&roots, "~/src"), None, "no duplicates");
        assert_eq!(with_root(&roots, "  "), None);
        assert_eq!(without_root(&roots, "~/src"), vec!["~/Repos".to_string()]);
        assert_eq!(roots_value(&["~/a b".to_string()]), r#"["~/a b"]"#);
    }

    #[test]
    fn view_names_the_worktrees_folder() {
        let v = view(
            &Snapshot::default(),
            &HashMap::new(),
            &Paths::new("/tmp/clusia-test-home"),
        );
        assert_eq!(v.roots.len(), 4);
        assert_eq!(v.retention, "14");
        assert_eq!(v.worktrees, "/tmp/clusia-test-home/worktrees");
    }
}
