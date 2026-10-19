//! The command that opens a file at a line in the configured editor (spec §8 Editor).
//!
//! VS Code and Cursor are opened through their URL schemes with `/usr/bin/open`. That works
//! from a launchd daemon whose `PATH` lacks the editors' command-line tools. Zed's URL handler
//! does not reliably open a file at a line, so Zed runs the `cli` binary inside its app bundle
//! (also found without `PATH`) with `path:line`. A custom
//! command is split into words (quotes group words, no escapes, no shell); `{line}` and then
//! `{path}` are substituted inside each word.

use std::fmt::Write;

use crate::config::{Editor, EditorKind};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditorError {
    #[error("the custom editor command is empty")]
    Empty,
    #[error("the custom editor command needs {{path}}")]
    NoPath,
    #[error("the custom editor command has an unclosed quote")]
    Quote,
    #[error(
        "Zed's command line tool was not found in /Applications or ~/Applications; install Zed there or set a custom command"
    )]
    NoZed,
}

/// Where the app bundles keep Zed's command-line tool, most likely first. `~` is `$HOME`.
const ZED_CLIS: [&str; 4] = [
    "/Applications/Zed.app/Contents/MacOS/cli",
    "~/Applications/Zed.app/Contents/MacOS/cli",
    "/Applications/Zed Preview.app/Contents/MacOS/cli",
    "~/Applications/Zed Preview.app/Contents/MacOS/cli",
];

/// Program and arguments that open `path` at `line` (1 when absent or 0).
pub fn editor_argv(
    editor: &Editor,
    path: &str,
    line: Option<u32>,
) -> Result<Vec<String>, EditorError> {
    editor_argv_where(editor, path, line, |p| std::path::Path::new(p).is_file())
}

/// [`editor_argv`] with `exists` deciding which Zed command-line tool is installed.
fn editor_argv_where(
    editor: &Editor,
    path: &str,
    line: Option<u32>,
    exists: impl Fn(&str) -> bool,
) -> Result<Vec<String>, EditorError> {
    let line = line.unwrap_or(1).max(1);
    let open = |scheme: &str| {
        Ok(vec![
            "/usr/bin/open".to_string(),
            file_url(scheme, path, line),
        ])
    };
    match editor.kind {
        EditorKind::VsCode => open("vscode"),
        EditorKind::Cursor => open("cursor"),
        EditorKind::Zed => {
            let home = std::env::var("HOME").unwrap_or_default();
            let cli = ZED_CLIS
                .iter()
                .map(|c| match c.strip_prefix("~/") {
                    Some(rest) => format!("{home}/{rest}"),
                    None => (*c).to_string(),
                })
                .find(|c| exists(c))
                .ok_or(EditorError::NoZed)?;
            Ok(vec![cli, format!("{path}:{line}")])
        }
        EditorKind::Custom => {
            let words = split_words(&editor.custom_command)?;
            if words.is_empty() {
                return Err(EditorError::Empty);
            }
            if !words.iter().any(|w| w.contains("{path}")) {
                return Err(EditorError::NoPath);
            }
            let line = line.to_string();
            Ok(words
                .into_iter()
                .map(|w| w.replace("{line}", &line).replace("{path}", path))
                .collect())
        }
    }
}

/// `scheme://file<percent-encoded path>:<line>`; unreserved characters and `/` stay as they are.
fn file_url(scheme: &str, path: &str, line: u32) -> String {
    let mut out = format!("{scheme}://file");
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    let _ = write!(out, ":{line}");
    out
}

/// Whitespace-separated words; single or double quotes group (and are removed). No escapes.
fn split_words(s: &str) -> Result<Vec<String>, EditorError> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            None => {
                current.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return Err(EditorError::Quote);
    }
    if in_word {
        words.push(current);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ed(kind: EditorKind, cmd: &str) -> Editor {
        Editor {
            kind,
            custom_command: cmd.into(),
        }
    }

    #[test]
    fn known_editors_use_url_schemes() {
        assert_eq!(
            editor_argv(&ed(EditorKind::VsCode, ""), "/w/a b.rs", Some(7)).unwrap(),
            ["/usr/bin/open", "vscode://file/w/a%20b.rs:7"]
        );
        assert_eq!(
            editor_argv(&ed(EditorKind::Cursor, ""), "/w/x.rs", Some(2)).unwrap(),
            ["/usr/bin/open", "cursor://file/w/x.rs:2"]
        );
    }

    #[test]
    fn zed_runs_the_cli_in_its_app_bundle_with_path_and_line() {
        let zed = ed(EditorKind::Zed, "");
        let installed = |p: &str| p == ZED_CLIS[0];
        assert_eq!(
            editor_argv_where(&zed, "/w/a b.rs", Some(7), installed).unwrap(),
            [ZED_CLIS[0], "/w/a b.rs:7"],
            "the path stays one argument, spaces and all"
        );
        assert_eq!(
            editor_argv_where(&zed, "/w/ç.rs", None, installed).unwrap()[1],
            "/w/ç.rs:1",
            "no line opens line 1"
        );
        assert_eq!(
            editor_argv_where(&zed, "/w/x.rs", Some(0), installed).unwrap()[1],
            "/w/x.rs:1",
            "line 0 is treated as 1"
        );
        let preview = |p: &str| p == ZED_CLIS[2];
        assert_eq!(
            editor_argv_where(&zed, "/w/x.rs", None, preview).unwrap()[0],
            ZED_CLIS[2],
            "Zed Preview is used when Zed is not installed"
        );
        assert_eq!(
            editor_argv_where(&zed, "/w/x.rs", None, |_| false),
            Err(EditorError::NoZed)
        );
    }

    #[test]
    fn custom_commands_are_split_without_a_shell() {
        assert_eq!(
            editor_argv(
                &ed(EditorKind::Custom, "nvim +{line} {path}"),
                "/w/a b.rs",
                Some(7)
            )
            .unwrap(),
            ["nvim", "+7", "/w/a b.rs"]
        );
        assert_eq!(
            editor_argv(
                &ed(
                    EditorKind::Custom,
                    "'/Applications/My Editor.app/bin/e' \"{path}:{line}\""
                ),
                "/w/x.rs",
                Some(3)
            )
            .unwrap(),
            ["/Applications/My Editor.app/bin/e", "/w/x.rs:3"]
        );
        assert_eq!(
            editor_argv(&ed(EditorKind::Custom, "e {path}"), "/w/{line}.rs", Some(9)).unwrap(),
            ["e", "/w/{line}.rs"],
            "placeholders inside the path are not substituted"
        );
        assert_eq!(
            editor_argv(&ed(EditorKind::Custom, "e $(rm) {path};"), "/w/x", None).unwrap(),
            ["e", "$(rm)", "/w/x;"],
            "shell syntax is passed through as plain words"
        );
    }

    #[test]
    fn bad_custom_commands_are_errors() {
        let path = "/w/x.rs";
        assert_eq!(
            editor_argv(&ed(EditorKind::Custom, "   "), path, None),
            Err(EditorError::Empty)
        );
        assert_eq!(
            editor_argv(&ed(EditorKind::Custom, "nvim"), path, None),
            Err(EditorError::NoPath)
        );
        assert_eq!(
            editor_argv(&ed(EditorKind::Custom, "nvim '{path}"), path, None),
            Err(EditorError::Quote)
        );
        assert_eq!(
            EditorError::NoPath.to_string(),
            "the custom editor command needs {path}"
        );
    }
}
