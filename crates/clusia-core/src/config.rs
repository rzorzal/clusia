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
    pub lists: Lists,
    pub media: Media,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub theme: Theme,
    /// Code font size in points: one of `CODE_SIZES`.
    pub code_size: u8,
    pub density: Density,
    /// How the Diff opens; each review can still switch.
    pub diff_view: DiffView,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: Theme::System,
            code_size: 13,
            density: Density::Comfortable,
            diff_view: DiffView::Unified,
        }
    }
}

pub const CODE_SIZES: [u8; 4] = [12, 13, 14, 16];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    #[default]
    Comfortable,
    Compact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DiffView {
    #[default]
    Unified,
    Split,
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

/// Images and GIFs in comments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Media {
    /// Show images hosted outside GitHub and Giphy; off, they appear as links.
    pub load_external_images: bool,
}

/// How the pull request lists (tray and Home) are ordered, filtered and narrowed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Lists {
    pub assigned_sort: ListSort,
    pub saved_sort: ListSort,
    /// The window's "Mine" list (the tray does not show it).
    pub mine_sort: ListSort,
    /// Case-insensitive text matched against title, repository and #number, in both lists.
    pub filter: String,
    /// Only this repository (`owner/repo`); empty shows all.
    pub repository: String,
}

pub const MAX_FILTER_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ListSort {
    /// Most recently updated first.
    #[default]
    Updated,
    /// Least recently updated first.
    Oldest,
    /// `owner/repo` A-Z, then most recently updated.
    Repository,
    /// Highest pull request number first.
    Number,
}

impl ListSort {
    pub fn next(self) -> Self {
        match self {
            Self::Updated => Self::Oldest,
            Self::Oldest => Self::Repository,
            Self::Repository => Self::Number,
            Self::Number => Self::Updated,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Updated => "Updated",
            Self::Oldest => "Oldest",
            Self::Repository => "Repository",
            Self::Number => "Number",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Updated => "updated",
            Self::Oldest => "oldest",
            Self::Repository => "repository",
            Self::Number => "number",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [Self::Updated, Self::Oldest, Self::Repository, Self::Number]
            .into_iter()
            .find(|v| v.as_str() == s)
    }
}

impl Lists {
    /// Applies one `ConfigChanged { key, value }`; `true` when a field changed.
    pub fn apply(&mut self, key: &str, value: &str) -> bool {
        let before = self.clone();
        match key {
            "lists.assigned_sort" => {
                if let Some(s) = ListSort::parse(value) {
                    self.assigned_sort = s;
                }
            }
            "lists.saved_sort" => {
                if let Some(s) = ListSort::parse(value) {
                    self.saved_sort = s;
                }
            }
            "lists.mine_sort" => {
                if let Some(s) = ListSort::parse(value) {
                    self.mine_sort = s;
                }
            }
            "lists.filter" => self.filter = value.to_string(),
            "lists.repository" => self.repository = value.to_string(),
            _ => {}
        }
        *self != before
    }
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
        if self.lists.filter.chars().count() > MAX_FILTER_CHARS {
            return Err(format!(
                "lists.filter must be at most {MAX_FILTER_CHARS} characters"
            ));
        }
        let repo = &self.lists.repository;
        let owner_repo = repo.split_once('/').is_some_and(|(owner, name)| {
            !owner.is_empty() && !name.is_empty() && !name.contains('/')
        });
        if !repo.is_empty() && !owner_repo {
            return Err("lists.repository must be empty or owner/repo".into());
        }
        let size = self.appearance.code_size;
        if !CODE_SIZES.contains(&size) {
            return Err(format!(
                "appearance.code_size must be one of 12, 13, 14, 16, got {size}"
            ));
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
        assert_eq!(c.appearance.code_size, 13);
        assert_eq!(c.appearance.density, Density::Comfortable);
        assert_eq!(c.appearance.diff_view, DiffView::Unified);
        assert_eq!(c.lists.mine_sort, ListSort::Updated);
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
        assert!(!c.media.load_external_images);
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn appearance_wire_names_and_code_size_rule() {
        let c: Config = serde_json::from_str(
            r#"{"appearance":{"code_size":16,"density":"compact","diff_view":"split"}}"#,
        )
        .unwrap();
        assert_eq!(c.appearance.code_size, 16);
        assert_eq!(c.appearance.density, Density::Compact);
        assert_eq!(c.appearance.diff_view, DiffView::Split);
        assert_eq!(
            c.appearance.theme,
            Theme::System,
            "missing keys keep defaults"
        );
        assert_eq!(c.validate(), Ok(()));
        let mut bad = Config::default();
        bad.appearance.code_size = 15;
        assert_eq!(
            bad.validate().unwrap_err(),
            "appearance.code_size must be one of 12, 13, 14, 16, got 15"
        );
    }

    #[test]
    fn mine_sort_applies() {
        let mut l = Lists::default();
        assert!(l.apply("lists.mine_sort", "number"));
        assert_eq!(l.mine_sort, ListSort::Number);
        assert!(
            !l.apply("lists.mine_sort", "sideways"),
            "unknown values are ignored"
        );
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

    #[test]
    fn list_preferences_default_and_cycle() {
        let c = Config::default();
        assert_eq!(c.lists.assigned_sort, ListSort::Updated);
        assert_eq!(c.lists.saved_sort, ListSort::Updated);
        assert_eq!(c.lists.filter, "");
        assert_eq!(c.lists.repository, "");
        let mut s = ListSort::Updated;
        let mut seen = vec![];
        for _ in 0..4 {
            seen.push(s.label());
            s = s.next();
        }
        assert_eq!(seen, ["Updated", "Oldest", "Repository", "Number"]);
        assert_eq!(s, ListSort::Updated);
        assert_eq!(ListSort::parse("repository"), Some(ListSort::Repository));
        assert_eq!(ListSort::parse("bogus"), None);
        assert_eq!(ListSort::Oldest.as_str(), "oldest");
    }

    #[test]
    fn lists_apply_config_changes() {
        let mut l = Lists::default();
        assert!(l.apply("lists.assigned_sort", "number"));
        assert_eq!(l.assigned_sort, ListSort::Number);
        assert!(
            !l.apply("lists.assigned_sort", "number"),
            "same value is not a change"
        );
        assert!(l.apply("lists.filter", "auth"));
        assert!(l.apply("lists.repository", "rzorzal/clusia"));
        assert!(!l.apply("lists.saved_sort", "bogus"));
        assert!(!l.apply("github.host", "x"));
        assert_eq!(
            l,
            Lists {
                assigned_sort: ListSort::Number,
                saved_sort: ListSort::Updated,
                mine_sort: ListSort::Updated,
                filter: "auth".into(),
                repository: "rzorzal/clusia".into(),
            }
        );
    }

    #[test]
    fn list_preferences_are_validated() {
        let mut c = Config::default();
        c.lists.filter = "x".repeat(201);
        assert!(c.validate().unwrap_err().contains("lists.filter"));
        c.lists.filter.clear();
        for bad in ["no-slash", "a//b", "/a/b", "a/b/", "/b", "a/"] {
            c.lists.repository = bad.into();
            assert!(
                c.validate().unwrap_err().contains("lists.repository"),
                "{bad}"
            );
        }
        c.lists.repository = "rzorzal/clusia".into();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn media_section_defaults_and_round_trips() {
        let c: Config = serde_json::from_str(r#"{"media":{"load_external_images":true}}"#).unwrap();
        assert!(c.media.load_external_images);
        let none: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(none.media, Media::default());
        assert_eq!(c.validate(), Ok(()));
    }
}
