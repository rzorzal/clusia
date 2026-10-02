//! User configuration (`config.toml`). Pure data: loading and saving live in `clusia-store`.

use serde::{Deserialize, Serialize};

pub const MIN_POLL_SECS: u64 = 15;
pub const MAX_POLL_SECS: u64 = 3600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub appearance: Appearance,
    pub github: Github,
    pub repositories: Repositories,
    pub editor: Editor,
    pub notifications: Notifications,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Appearance {
    pub theme: Theme,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    Light,
    Dark,
    /// Follow the macOS appearance, including Auto.
    #[default]
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Github {
    pub host: String,
    pub auth: AuthSource,
    pub poll_interval_secs: u64,
}

impl Default for Github {
    fn default() -> Self {
        Self {
            host: "github.com".into(),
            auth: AuthSource::GhCli,
            poll_interval_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AuthSource {
    /// Reuse `gh auth token`.
    #[default]
    GhCli,
    /// Personal access token stored in the macOS Keychain.
    Pat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Repositories {
    pub roots: Vec<String>,
    pub worktree_retention_days: u32,
}

impl Default for Repositories {
    fn default() -> Self {
        Self {
            roots: ["~/Repos", "~/Projects", "~/src", "~/code"]
                .map(String::from)
                .to_vec(),
            worktree_retention_days: 14,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Editor {
    pub kind: EditorKind,
    /// Used when `kind = "custom"`; `{path}` and `{line}` are substituted.
    pub custom_command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EditorKind {
    #[default]
    VsCode,
    Zed,
    Cursor,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Notifications {
    pub do_not_disturb: bool,
}

impl Config {
    /// Rules serde can't express. The message names the offending key.
    pub fn validate(&self) -> Result<(), String> {
        let poll = self.github.poll_interval_secs;
        if !(MIN_POLL_SECS..=MAX_POLL_SECS).contains(&poll) {
            return Err(format!(
                "github.poll_interval_secs must be between {MIN_POLL_SECS} and {MAX_POLL_SECS}, got {poll}"
            ));
        }
        if self.github.host.trim().is_empty() {
            return Err("github.host must not be empty".into());
        }
        if self.editor.kind == EditorKind::Custom && !self.editor.custom_command.contains("{path}")
        {
            return Err(
                "editor.custom_command must contain {path} when editor.kind is custom".into(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!(c.appearance.theme, Theme::System);
        assert_eq!(c.github.host, "github.com");
        assert_eq!(c.github.auth, AuthSource::GhCli);
        assert_eq!(c.github.poll_interval_secs, 60);
        assert_eq!(
            c.repositories.roots,
            vec!["~/Repos", "~/Projects", "~/src", "~/code"]
        );
        assert_eq!(c.repositories.worktree_retention_days, 14);
        assert_eq!(c.editor.kind, EditorKind::VsCode);
        assert_eq!(c.editor.custom_command, "");
        assert!(!c.notifications.do_not_disturb);
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn partial_input_fills_defaults() {
        let c: Config = serde_json::from_str(r#"{"github":{"host":"ghe.example.com"}}"#).unwrap();
        assert_eq!(c.github.host, "ghe.example.com");
        assert_eq!(c.github.poll_interval_secs, 60);
        assert_eq!(c.appearance.theme, Theme::System);
    }

    #[test]
    fn enum_wire_names() {
        let c: Config = serde_json::from_str(
            r#"{"appearance":{"theme":"dark"},"github":{"auth":"pat"},"editor":{"kind":"zed"}}"#,
        )
        .unwrap();
        assert_eq!(c.appearance.theme, Theme::Dark);
        assert_eq!(c.github.auth, AuthSource::Pat);
        assert_eq!(c.editor.kind, EditorKind::Zed);
        assert_eq!(
            serde_json::to_string(&EditorKind::VsCode).unwrap(),
            r#""vscode""#
        );
        assert_eq!(
            serde_json::to_string(&AuthSource::GhCli).unwrap(),
            r#""gh-cli""#
        );
    }

    #[test]
    fn validate_rejects_bad_poll_interval() {
        let mut c = Config::default();
        c.github.poll_interval_secs = 5;
        assert!(c.validate().unwrap_err().contains("poll_interval_secs"));
        c.github.poll_interval_secs = 4000;
        assert!(c.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_host() {
        let mut c = Config::default();
        c.github.host = "  ".into();
        assert!(c.validate().unwrap_err().contains("host"));
    }

    #[test]
    fn validate_requires_path_placeholder_for_custom_editor() {
        let mut c = Config::default();
        c.editor.kind = EditorKind::Custom;
        c.editor.custom_command = "myeditor".into();
        assert!(c.validate().unwrap_err().contains("{path}"));
        c.editor.custom_command = "myeditor {path}:{line}".into();
        assert_eq!(c.validate(), Ok(()));
    }
}
