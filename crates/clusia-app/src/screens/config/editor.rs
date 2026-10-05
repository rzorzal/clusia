//! Config › Editor: which editor "Open in editor" uses, with a preview of the exact command.

use std::collections::HashMap;

use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, observe};
use clusia_core::config::EditorKind;
use clusia_core::{Paths, editor_argv};

use super::{field_row, option_card, page_header, row, sends, setter};
use crate::bridge::{Ask, Asks, Model, set_config};
use crate::fonts::UiFonts;
use crate::snapshot::Snapshot;
use crate::theme::Swatch;
use crate::ui::kit::{Type, Variant, button, text};

/// Written before switching to Custom when no command is set yet.
pub const DEFAULT_CUSTOM: &str = "code --goto {path}:{line}";

/// The Custom card (its click needs the template rule above).
#[derive(Component, Debug)]
pub struct ChooseCustom;

#[derive(Debug, Clone, PartialEq)]
pub struct EditorView {
    pub kind: EditorKind,
    pub custom: String,
    pub custom_error: Option<String>,
    pub kind_error: Option<String>,
    /// The command for an example file at line 44, or why there is none.
    pub preview: Result<String, String>,
    /// What the test button opens.
    pub test_path: String,
}

pub fn view(snap: &Snapshot, rejected: &HashMap<String, String>, paths: &Paths) -> EditorView {
    let editor = &snap.config.editor;
    let example = paths
        .worktrees_dir()
        .join("rzorzal~clusia~61/crates/clusiad/src/sync.rs");
    EditorView {
        kind: editor.kind,
        custom: editor.custom_command.clone(),
        custom_error: rejected.get("editor.custom_command").cloned(),
        kind_error: rejected.get("editor.kind").cloned(),
        preview: editor_argv(editor, &example.display().to_string(), Some(44))
            .map(|argv| shell_words(&argv))
            .map_err(|e| e.to_string()),
        test_path: paths.config_file().display().to_string(),
    }
}

/// Words joined for display; words with spaces are single-quoted. Never executed.
pub fn shell_words(argv: &[String]) -> String {
    argv.iter()
        .map(|w| {
            if w.contains(char::is_whitespace) {
                format!("'{w}'")
            } else {
                w.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn build(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &EditorView) {
    page_header(
        p,
        fonts,
        "Editor",
        "\"Open in editor\" on any line of a diff or a finding opens the file at that line, in the review's worktree.",
    );
    p.spawn(Node {
        column_gap: px(10),
        ..default()
    })
    .with_children(|r| {
        for (name, hint, value, kind) in [
            (
                "VS Code",
                "Opens vscode:// links",
                "vscode",
                EditorKind::VsCode,
            ),
            ("Zed", "Opens zed:// links", "zed", EditorKind::Zed),
            (
                "Cursor",
                "Opens cursor:// links",
                "cursor",
                EditorKind::Cursor,
            ),
        ] {
            r.spawn((
                option_card(v.kind == kind, 190.0),
                setter("editor.kind", value),
            ))
            .with_children(|c| {
                c.spawn(text(fonts, name, Type::STRONG));
                c.spawn(text(fonts, hint, Type::META));
            });
        }
        r.spawn((
            option_card(v.kind == EditorKind::Custom, 190.0),
            ChooseCustom,
            observe(on_custom),
        ))
        .with_children(|c| {
            c.spawn(text(fonts, "Custom", Type::STRONG));
            c.spawn(text(fonts, "Any command", Type::META));
        });
    });
    if let Some(message) = &v.kind_error {
        p.spawn((
            super::FieldError("editor.kind"),
            children![text(fonts, message.clone(), Type::BODY.ink(Swatch::Orange))],
        ));
    }
    if v.kind == EditorKind::Custom {
        field_row(
            p,
            fonts,
            "Command",
            "editor.custom_command",
            &v.custom,
            420.0,
            "{path} and {line} are replaced; no shell",
            &v.custom_error,
        );
    }
    row(
        p,
        fonts,
        "Will run",
        |r| match &v.preview {
            Ok(cmd) => {
                r.spawn(text(fonts, cmd.clone(), Type::MONO.ink(Swatch::Fg)));
            }
            Err(e) => {
                r.spawn(text(fonts, e.clone(), Type::BODY.ink(Swatch::Orange)));
            }
        },
        "",
        None,
    );
    p.spawn((
        button(fonts, "Test: open config.toml", Variant::Primary),
        sends(Ask::OpenInEditor {
            path: v.test_path.clone(),
            line: Some(1),
        }),
    ));
}

fn on_custom(_activate: On<Activate>, mut asks: ResMut<Asks>, mut model: ResMut<Model>) {
    if model
        .snapshot
        .config
        .editor
        .custom_command
        .trim()
        .is_empty()
    {
        set_config(
            &mut asks,
            &mut model,
            "editor.custom_command",
            DEFAULT_CUSTOM,
        );
    }
    set_config(&mut asks, &mut model, "editor.kind", "custom");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        Paths::new("/tmp/clusia-test-home")
    }

    #[test]
    fn preview_shows_the_exact_command() {
        let v = view(&Snapshot::default(), &HashMap::new(), &paths());
        assert_eq!(v.kind, EditorKind::VsCode);
        assert_eq!(
            v.preview,
            Ok("/usr/bin/open vscode://file/tmp/clusia-test-home/worktrees/rzorzal~clusia~61/crates/clusiad/src/sync.rs:44".to_string())
        );
        assert_eq!(v.test_path, "/tmp/clusia-test-home/config.toml");
        let mut snap = Snapshot::default();
        snap.config.editor.kind = EditorKind::Custom;
        snap.config.editor.custom_command = "'/Applications/My Editor' {path}".into();
        let v = view(&snap, &HashMap::new(), &paths());
        assert_eq!(
            v.preview,
            Ok("'/Applications/My Editor' /tmp/clusia-test-home/worktrees/rzorzal~clusia~61/crates/clusiad/src/sync.rs".to_string())
        );
        snap.config.editor.custom_command = "nvim".into();
        assert_eq!(
            view(&snap, &HashMap::new(), &paths()).preview,
            Err("the custom editor command needs {path}".to_string())
        );
    }
}
