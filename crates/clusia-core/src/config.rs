//! User configuration (`config.toml`). Pure data: loading and saving live in `clusia-store`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

pub const MIN_POLL_SECS: u64 = 15;
pub const MAX_POLL_SECS: u64 = 3600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub appearance: Appearance,
    pub github: Github,
    pub repositories: Repositories,
    pub editor: Editor,
    pub notifications: Notifications,
    pub lists: Lists,
    pub media: Media,
    pub composer: Composer,
    pub harness: Harness,
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

/// Start-up behaviour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// Whether the daemon's login item runs at login.
    pub start_at_login: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            start_at_login: true,
        }
    }
}

/// What Clúsia tells you about, and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Notifications {
    /// One route per kind; a file that lists only some kinds keeps the defaults of the rest.
    #[serde(deserialize_with = "events_over_defaults")]
    pub events: BTreeMap<EventKind, Route>,
    pub sound: SoundId,
    pub dnd: Dnd,
    /// Silent while a macOS Focus is on (interruption level `active`); off, notifications
    /// break through (`timeSensitive`).
    pub follow_focus: bool,
    /// Several events for one pull request within two minutes become one notification.
    pub group_bursts: bool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self {
            events: EventKind::ALL
                .into_iter()
                .map(|kind| (kind, Route::default_for(kind)))
                .collect(),
            sound: SoundId::default(),
            dnd: Dnd::default(),
            follow_focus: true,
            group_bursts: true,
        }
    }
}

impl Notifications {
    /// The route for `kind`, falling back to its default when the map lacks it.
    pub fn route(&self, kind: EventKind) -> Route {
        self.events
            .get(&kind)
            .copied()
            .unwrap_or_else(|| Route::default_for(kind))
    }
}

fn events_over_defaults<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<EventKind, Route>, D::Error> {
    #[derive(Deserialize)]
    struct Patch {
        tray: Option<bool>,
        macos: Option<bool>,
        sound: Option<bool>,
    }
    let patches = BTreeMap::<String, Patch>::deserialize(deserializer)?;
    let mut events = Notifications::default().events;
    for (name, patch) in patches {
        let Some(route) = EventKind::parse(&name).and_then(|kind| events.get_mut(&kind)) else {
            continue;
        };
        route.tray = patch.tray.unwrap_or(route.tray);
        route.macos = patch.macos.unwrap_or(route.macos);
        route.sound = patch.sound.unwrap_or(route.sound);
    }
    Ok(events)
}

/// What can be notified about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    ReviewRequested,
    CommitsAfterReview,
    ReplyToYou,
    Mentioned,
    ChecksFailed,
    SyncProblem,
    StateRecovered,
    AgentFinished,
    AgentPermission,
}

impl EventKind {
    pub const ALL: [EventKind; 9] = [
        Self::ReviewRequested,
        Self::CommitsAfterReview,
        Self::ReplyToYou,
        Self::Mentioned,
        Self::ChecksFailed,
        Self::SyncProblem,
        Self::StateRecovered,
        Self::AgentFinished,
        Self::AgentPermission,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReviewRequested => "review_requested",
            Self::CommitsAfterReview => "commits_after_review",
            Self::ReplyToYou => "reply_to_you",
            Self::Mentioned => "mentioned",
            Self::ChecksFailed => "checks_failed",
            Self::SyncProblem => "sync_problem",
            Self::StateRecovered => "state_recovered",
            Self::AgentFinished => "agent_finished",
            Self::AgentPermission => "agent_permission",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where one kind of event shows up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    /// The tray's inbox and its dot.
    pub tray: bool,
    /// A macOS notification.
    pub macos: bool,
    /// The notification plays the chosen sound.
    pub sound: bool,
}

impl Route {
    pub fn default_for(kind: EventKind) -> Self {
        let (tray, macos, sound) = match kind {
            EventKind::ReviewRequested | EventKind::Mentioned | EventKind::AgentPermission => {
                (true, true, true)
            }
            EventKind::CommitsAfterReview
            | EventKind::ReplyToYou
            | EventKind::SyncProblem
            | EventKind::StateRecovered
            | EventKind::AgentFinished => (true, true, false),
            EventKind::ChecksFailed => (true, false, false),
        };
        Self { tray, macos, sound }
    }
}

/// The bundled notification sounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SoundId {
    #[default]
    Leaf,
    Drop,
    Chime,
    Tick,
}

impl SoundId {
    pub const ALL: [SoundId; 4] = [Self::Leaf, Self::Drop, Self::Chime, Self::Tick];

    /// The id, also the file name (`<id>.aiff`) in the bundle.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Leaf => "leaf",
            Self::Drop => "drop",
            Self::Chime => "chime",
            Self::Tick => "tick",
        }
    }

    /// The name shown in the sound menu.
    pub fn label(self) -> &'static str {
        match self {
            Self::Leaf => "Leaf (Clúsia)",
            Self::Drop => "Drop",
            Self::Chime => "Chime",
            Self::Tick => "Tick",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

/// Quiet hours: no macOS notification or sound inside the range on the chosen days. A range
/// whose `from` is later than `to` runs overnight and belongs to the day it starts on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Dnd {
    pub enabled: bool,
    pub from: HourMinute,
    pub to: HourMinute,
    pub days: BTreeSet<Weekday>,
}

impl Default for Dnd {
    fn default() -> Self {
        Self {
            enabled: false,
            from: HourMinute::new(19, 0).expect("19:00 is a time"),
            to: HourMinute::new(9, 0).expect("09:00 is a time"),
            days: [
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri,
            ]
            .into(),
        }
    }
}

/// A time of day in local time, written `HH:MM` in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HourMinute(u16);

impl HourMinute {
    pub fn new(hour: u8, minute: u8) -> Option<Self> {
        (hour < 24 && minute < 60).then(|| Self(u16::from(hour) * 60 + u16::from(minute)))
    }

    /// Minutes since midnight, 0..1440.
    pub fn from_minutes(minutes: u16) -> Option<Self> {
        (minutes < 24 * 60).then_some(Self(minutes))
    }

    pub fn minutes(self) -> u16 {
        self.0
    }

    pub fn hour(self) -> u8 {
        (self.0 / 60) as u8
    }

    pub fn minute(self) -> u8 {
        (self.0 % 60) as u8
    }
}

impl fmt::Display for HourMinute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.hour(), self.minute())
    }
}

impl std::str::FromStr for HourMinute {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || format!("{s:?} is not a time like 19:00");
        let (h, m) = s.trim().split_once(':').ok_or_else(bad)?;
        let digits = |t: &str| t.chars().all(|c| c.is_ascii_digit());
        if h.is_empty() || h.len() > 2 || m.len() != 2 || !digits(h) || !digits(m) {
            return Err(bad());
        }
        let hour: u8 = h.parse().map_err(|_| bad())?;
        let minute: u8 = m.parse().map_err(|_| bad())?;
        Self::new(hour, minute).ok_or_else(bad)
    }
}

impl TryFrom<String> for HourMinute {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        s.parse()
    }
}

impl From<HourMinute> for String {
    fn from(t: HourMinute) -> String {
        t.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Weekday {
    pub const ALL: [Weekday; 7] = [
        Self::Mon,
        Self::Tue,
        Self::Wed,
        Self::Thu,
        Self::Fri,
        Self::Sat,
        Self::Sun,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mon => "mon",
            Self::Tue => "tue",
            Self::Wed => "wed",
            Self::Thu => "thu",
            Self::Fri => "fri",
            Self::Sat => "sat",
            Self::Sun => "sun",
        }
    }

    /// Monday is 0.
    pub fn index(self) -> usize {
        match self {
            Self::Mon => 0,
            Self::Tue => 1,
            Self::Wed => 2,
            Self::Thu => 3,
            Self::Fri => 4,
            Self::Sat => 5,
            Self::Sun => 6,
        }
    }

    pub fn previous(self) -> Self {
        Self::ALL[(self.index() + 6) % 7]
    }
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

/// How many emoji the composer remembers as recently used.
pub const MAX_RECENT_EMOJI: usize = 16;

/// What the comment composer remembers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Composer {
    /// The emoji picked last, newest first (at most `MAX_RECENT_EMOJI`).
    pub recent_emoji: Vec<String>,
}

pub const MIN_TURN_TIMEOUT_SECS: u32 = 60;
pub const MAX_TURN_TIMEOUT_SECS: u32 = 3600;

/// The agent behind the review chat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Harness {
    pub kind: HarnessKind,
    /// The program to run instead of the one found on `PATH`. It is written as an empty
    /// string in `config.toml` when unset, so the key can always be read and set by name.
    #[serde(with = "empty_is_none")]
    pub program: Option<String>,
    /// Appended to every turn's command line, split like a shell would.
    pub extra_args: String,
    pub on_open: OnOpen,
    /// Also allow what the user's own Claude Code settings already allow.
    pub use_cli_permissions: bool,
    pub turn_timeout_secs: u32,
}

impl Harness {
    pub const DEFAULT_TIMEOUT_SECS: u32 = 600;

    /// `extra_args` split into words; an unbalanced quote is an error.
    pub fn extra_args_list(&self) -> Result<Vec<String>, String> {
        split_args(&self.extra_args)
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self {
            kind: HarnessKind::ClaudeCode,
            program: None,
            extra_args: String::new(),
            on_open: OnOpen::Summarize,
            use_cli_permissions: true,
            turn_timeout_secs: Self::DEFAULT_TIMEOUT_SECS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessKind {
    #[default]
    ClaudeCode,
}

/// What happens when a review opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OnOpen {
    /// The agent summarizes the pull request on its own.
    #[default]
    Summarize,
    /// The agent stays idle until the first question.
    Wait,
}

mod empty_is_none {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<String>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(value.as_deref().unwrap_or(""))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        let text = String::deserialize(d)?;
        let text = text.trim();
        Ok((!text.is_empty()).then(|| text.to_string()))
    }
}

/// Splits `text` into words the way a POSIX shell does for quotes and backslashes (no
/// expansion of any kind).
pub fn split_args(text: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => word.push(c),
                        None => return Err("a single quote is not closed".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\')) => word.push(c),
                            Some(c) => {
                                word.push('\\');
                                word.push(c);
                            }
                            None => return Err("a double quote is not closed".into()),
                        },
                        Some(c) => word.push(c),
                        None => return Err("a double quote is not closed".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.push(chars.next().ok_or("the text ends with a backslash")?);
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// How many values follow a reserved flag written as `--flag value`.
#[derive(Clone, Copy)]
enum Takes {
    /// A switch.
    Nothing,
    /// One value, unless the next word is another flag.
    One,
    /// Every word up to the next flag.
    Many,
}

/// Flags that decide what the agent may do, which settings load, which session it joins, which
/// prompt it follows and how it prints. Clúsia sets them itself, so the user's extra
/// arguments never carry them.
const RESERVED: [(&str, Takes); 23] = [
    ("--dangerously-skip-permissions", Takes::Nothing),
    ("--allow-dangerously-skip-permissions", Takes::Nothing),
    ("--permission-mode", Takes::One),
    ("--permission-prompt-tool", Takes::One),
    ("--allowedTools", Takes::Many),
    ("--allowed-tools", Takes::Many),
    ("--tools", Takes::Many),
    ("--settings", Takes::One),
    ("--setting-sources", Takes::One),
    ("--system-prompt", Takes::One),
    ("--system-prompt-file", Takes::One),
    ("--append-system-prompt", Takes::One),
    ("--append-system-prompt-file", Takes::One),
    ("--resume", Takes::One),
    ("-r", Takes::One),
    ("--session-id", Takes::One),
    ("--continue", Takes::Nothing),
    ("-c", Takes::Nothing),
    ("--fork-session", Takes::Nothing),
    ("-p", Takes::Nothing),
    ("--print", Takes::Nothing),
    ("--output-format", Takes::One),
    ("--input-format", Takes::One),
];

/// The names of the reserved flags.
pub fn reserved_flags() -> impl Iterator<Item = &'static str> {
    RESERVED.iter().map(|(name, _)| *name)
}

fn reserved(arg: &str) -> Option<(&'static str, Takes, bool)> {
    let (name, has_value) = match arg.split_once('=') {
        Some((name, _)) => (name, true),
        None => (arg, false),
    };
    RESERVED
        .iter()
        .find(|(flag, _)| *flag == name)
        .map(|(flag, takes)| (*flag, *takes, has_value))
        .or_else(|| cluster(arg))
}

/// A cluster of short flags such as `-cp` or `-xr`: Claude Code reads each letter as a flag of
/// its own, so one holding a reserved letter is as reserved as the flag alone.
fn cluster(arg: &str) -> Option<(&'static str, Takes, bool)> {
    let letters = arg.strip_prefix('-')?;
    if letters.len() < 2
        || letters.starts_with('-')
        || !letters.chars().all(|c| c.is_ascii_alphabetic())
    {
        return None;
    }
    let (flag, takes) = letters.chars().find_map(|c| match c {
        'p' => Some(("-p", Takes::Nothing)),
        'c' => Some(("-c", Takes::Nothing)),
        'r' => Some(("-r", Takes::Nothing)),
        _ => None,
    })?;
    Some((flag, takes, false))
}

/// The first reserved flag in `args`, if any.
pub fn first_reserved_flag(args: &[String]) -> Option<&'static str> {
    args.iter()
        .find_map(|a| reserved(a).map(|(flag, _, _)| flag))
}

/// `args` without the reserved flags and the values that follow them (`--flag value`; a
/// `--flag=value` is one word).
pub fn strip_reserved_flags(args: &[String]) -> Vec<String> {
    let mut kept = Vec::with_capacity(args.len());
    let mut rest = args.iter().peekable();
    while let Some(arg) = rest.next() {
        let Some((_, takes, has_value)) = reserved(arg) else {
            kept.push(arg.clone());
            continue;
        };
        if has_value {
            continue;
        }
        let more = |next: Option<&&String>| next.is_some_and(|n| !n.starts_with('-'));
        match takes {
            Takes::Nothing => {}
            Takes::One => {
                if more(rest.peek()) {
                    rest.next();
                }
            }
            Takes::Many => {
                while more(rest.peek()) {
                    rest.next();
                }
            }
        }
    }
    kept
}

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
        if self.composer.recent_emoji.len() > MAX_RECENT_EMOJI {
            return Err(format!(
                "composer.recent_emoji must hold at most {MAX_RECENT_EMOJI} emoji"
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
        let timeout = self.harness.turn_timeout_secs;
        if !(MIN_TURN_TIMEOUT_SECS..=MAX_TURN_TIMEOUT_SECS).contains(&timeout) {
            return Err(format!(
                "harness.turn_timeout_secs must be between {MIN_TURN_TIMEOUT_SECS} and {MAX_TURN_TIMEOUT_SECS}, got {timeout}"
            ));
        }
        let args = self
            .harness
            .extra_args_list()
            .map_err(|e| format!("harness.extra_args: {e}"))?;
        if let Some(flag) = first_reserved_flag(&args) {
            return Err(format!(
                "harness.extra_args must not set {flag}: Clúsia sets permissions, settings, session, prompt and output itself"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_remembers_at_most_sixteen_emoji() {
        let mut c = Config::default();
        assert!(c.composer.recent_emoji.is_empty());
        c.composer.recent_emoji = (0..MAX_RECENT_EMOJI).map(|i| i.to_string()).collect();
        assert_eq!(c.validate(), Ok(()));
        c.composer.recent_emoji.push("🐢".into());
        assert!(c.validate().unwrap_err().contains("recent_emoji"));
    }

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
        assert!(c.general.start_at_login);
        assert!(c.notifications.follow_focus);
        assert!(c.notifications.group_bursts);
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

    #[test]
    fn default_routes_match_the_spec() {
        let n = Notifications::default();
        assert_eq!(n.events.len(), 9, "every kind has a route");
        let expected = [
            (EventKind::ReviewRequested, (true, true, true)),
            (EventKind::CommitsAfterReview, (true, true, false)),
            (EventKind::ReplyToYou, (true, true, false)),
            (EventKind::Mentioned, (true, true, true)),
            (EventKind::ChecksFailed, (true, false, false)),
            (EventKind::SyncProblem, (true, true, false)),
            (EventKind::StateRecovered, (true, true, false)),
            (EventKind::AgentFinished, (true, true, false)),
            (EventKind::AgentPermission, (true, true, true)),
        ];
        for (kind, (tray, macos, sound)) in expected {
            assert_eq!(n.route(kind), Route { tray, macos, sound }, "{kind}");
        }
        assert_eq!(n.sound, SoundId::Leaf);
        assert!(!n.dnd.enabled);
        assert_eq!(n.dnd.from.to_string(), "19:00");
        assert_eq!(n.dnd.to.to_string(), "09:00");
        assert_eq!(
            n.dnd.days.iter().copied().collect::<Vec<_>>(),
            [
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri
            ]
        );
    }

    #[test]
    fn event_kinds_have_stable_names() {
        for kind in EventKind::ALL {
            assert_eq!(EventKind::parse(kind.as_str()), Some(kind));
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
            assert_eq!(kind.to_string(), kind.as_str());
        }
        assert_eq!(EventKind::parse("nope"), None);
        assert_eq!(EventKind::ALL.len(), 9);
        assert!(EventKind::ReviewRequested < EventKind::AgentPermission);
    }

    #[test]
    fn sounds_have_ids_and_labels() {
        let ids: Vec<_> = SoundId::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(ids, ["leaf", "drop", "chime", "tick"]);
        assert_eq!(SoundId::Leaf.label(), "Leaf (Clúsia)");
        assert_eq!(SoundId::parse("chime"), Some(SoundId::Chime));
        assert_eq!(SoundId::parse("bell"), None);
        assert_eq!(serde_json::to_string(&SoundId::Tick).unwrap(), r#""tick""#);
    }

    #[test]
    fn times_and_weekdays_have_wire_forms() {
        assert_eq!(HourMinute::new(9, 5).unwrap().to_string(), "09:05");
        assert_eq!(
            "9:05".parse::<HourMinute>(),
            Ok(HourMinute::new(9, 5).unwrap())
        );
        assert_eq!(HourMinute::new(23, 59).unwrap().minutes(), 1439);
        assert_eq!(HourMinute::from_minutes(1440), None);
        assert_eq!(HourMinute::new(24, 0), None);
        assert_eq!(HourMinute::new(0, 60), None);
        for bad in [
            "", "19", "19:5", "7:00pm", "25:00", "12:60", "-1:00", "123:00", "+9:00", "09:+5",
        ] {
            assert!(bad.parse::<HourMinute>().is_err(), "{bad:?}");
        }
        assert_eq!(
            serde_json::to_string(&HourMinute::new(19, 0).unwrap()).unwrap(),
            r#""19:00""#
        );
        assert_eq!(
            serde_json::from_str::<HourMinute>(r#""07:30""#)
                .unwrap()
                .minutes(),
            450
        );
        assert!(serde_json::from_str::<HourMinute>(r#""late""#).is_err());
        assert_eq!(serde_json::to_string(&Weekday::Mon).unwrap(), r#""mon""#);
        assert_eq!(Weekday::Mon.previous(), Weekday::Sun);
        assert_eq!(Weekday::Wed.previous(), Weekday::Tue);
        assert_eq!(Weekday::ALL.map(Weekday::index), [0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(Weekday::Sat.as_str(), "sat");
    }

    #[test]
    fn a_partial_events_table_keeps_the_other_defaults() {
        let c: Config = serde_json::from_str(
            r#"{"notifications":{"events":{"checks_failed":{"macos":true},"from_the_future":{"tray":false}},"sound":"chime"}}"#,
        )
        .unwrap();
        let defaults = Notifications::default();
        assert_eq!(
            c.notifications.route(EventKind::ChecksFailed),
            Route {
                tray: true,
                macos: true,
                sound: false
            }
        );
        assert_eq!(
            c.notifications.route(EventKind::Mentioned),
            defaults.route(EventKind::Mentioned)
        );
        assert_eq!(c.notifications.events.len(), 9, "unknown kinds are dropped");
        assert_eq!(c.notifications.sound, SoundId::Chime);
        assert_eq!(c.notifications.dnd, defaults.dnd);
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn missing_routes_fall_back_to_the_default() {
        let mut n = Notifications::default();
        n.events.clear();
        assert_eq!(
            n.route(EventKind::ReviewRequested),
            Route::default_for(EventKind::ReviewRequested)
        );
    }

    #[test]
    fn general_defaults_to_start_at_login() {
        let c: Config = serde_json::from_str("{}").unwrap();
        assert!(c.general.start_at_login);
        let off: Config = serde_json::from_str(r#"{"general":{"start_at_login":false}}"#).unwrap();
        assert!(!off.general.start_at_login);
    }

    #[test]
    fn harness_defaults_match_spec() {
        let h = Config::default().harness;
        assert_eq!(h.kind, HarnessKind::ClaudeCode);
        assert_eq!(h.program, None);
        assert_eq!(h.extra_args, "");
        assert_eq!(h.on_open, OnOpen::Summarize);
        assert!(h.use_cli_permissions);
        assert_eq!(h.turn_timeout_secs, 600);
        assert_eq!(Harness::DEFAULT_TIMEOUT_SECS, 600);
    }

    #[test]
    fn harness_wire_names_and_empty_program() {
        let c: Config = serde_json::from_str(
            r#"{"harness":{"kind":"claude-code","program":"/opt/bin/claude","on_open":"wait","use_cli_permissions":false,"turn_timeout_secs":90}}"#,
        )
        .unwrap();
        assert_eq!(c.harness.program.as_deref(), Some("/opt/bin/claude"));
        assert_eq!(c.harness.on_open, OnOpen::Wait);
        assert!(!c.harness.use_cli_permissions);
        assert_eq!(c.validate(), Ok(()));
        let blank: Config = serde_json::from_str(r#"{"harness":{"program":"  "}}"#).unwrap();
        assert_eq!(blank.harness.program, None, "a blank program means PATH");
        let json = serde_json::to_value(Config::default()).unwrap();
        assert_eq!(json["harness"]["program"], "");
        assert_eq!(json["harness"]["on_open"], "summarize");
        assert_eq!(json["harness"]["kind"], "claude-code");
    }

    #[test]
    fn harness_timeout_is_bounded() {
        let mut c = Config::default();
        c.harness.turn_timeout_secs = 59;
        assert_eq!(
            c.validate().unwrap_err(),
            "harness.turn_timeout_secs must be between 60 and 3600, got 59"
        );
        c.harness.turn_timeout_secs = 3601;
        assert!(c.validate().is_err());
        c.harness.turn_timeout_secs = 60;
        assert_eq!(c.validate(), Ok(()));
        c.harness.turn_timeout_secs = 3600;
        assert_eq!(c.validate(), Ok(()));
    }

    #[test]
    fn split_args_follows_shell_quoting() {
        let words = |s: &str| split_args(s).unwrap();
        assert_eq!(words(""), Vec::<String>::new());
        assert_eq!(words("  --model   opus "), ["--model", "opus"]);
        assert_eq!(words(r#"--append "two words""#), ["--append", "two words"]);
        assert_eq!(words("--x 'a \"b\" c'"), ["--x", "a \"b\" c"]);
        assert_eq!(words(r#"--x "a \"b\" \\ \n""#), ["--x", "a \"b\" \\ \\n"]);
        assert_eq!(words(r"a\ b"), ["a b"]);
        assert_eq!(words("''"), [""], "an empty quoted word is a word");
        assert_eq!(words("--k='v w'"), ["--k=v w"]);
        for bad in ["--x 'open", "--x \"open", "tail\\"] {
            assert!(split_args(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn extra_args_reject_unbalanced_quotes_and_reserved_flags() {
        let mut c = Config::default();
        c.harness.extra_args = "--model opus --add-dir '../shared dir'".into();
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(
            c.harness.extra_args_list().unwrap(),
            ["--model", "opus", "--add-dir", "../shared dir"]
        );
        c.harness.extra_args = "--model 'opus".into();
        assert!(c.validate().unwrap_err().starts_with("harness.extra_args:"));
        for bad in [
            "--dangerously-skip-permissions",
            "--allow-dangerously-skip-permissions",
            "--permission-mode bypassPermissions",
            "--permission-mode=acceptEdits",
            "--settings {}",
            "--setting-sources user",
            "--allowedTools Bash Edit",
            "--allowed-tools=Bash",
            "--tools Bash",
            "--system-prompt other",
            "--append-system-prompt=other",
            "--resume abc",
            "-r",
            "--session-id abc",
            "--continue",
            "-c",
            "--fork-session",
            "-p",
            "--print",
            "--output-format text",
            "--input-format stream-json",
        ] {
            c.harness.extra_args = bad.into();
            assert!(c.validate().unwrap_err().contains("must not set"), "{bad}");
        }
        c.harness.extra_args = "--model opus --permission-mode plan".into();
        assert_eq!(
            c.validate().unwrap_err(),
            "harness.extra_args must not set --permission-mode: Clúsia sets permissions, settings, session, prompt and output itself"
        );
    }

    #[test]
    fn every_reserved_flag_is_refused_and_stripped() {
        let names: Vec<&str> = reserved_flags().collect();
        assert_eq!(names.len(), 23);
        for name in names {
            let args = vec![name.to_string(), "value".to_string()];
            assert_eq!(first_reserved_flag(&args), Some(name), "{name}");
            let kept = strip_reserved_flags(&args);
            assert!(!kept.contains(&name.to_string()), "{name}: {kept:?}");
            let joined = vec![format!("{name}=value")];
            assert_eq!(
                strip_reserved_flags(&joined),
                Vec::<String>::new(),
                "{name}"
            );
        }
    }

    #[test]
    fn reserved_flags_are_stripped_with_their_values() {
        let args: Vec<String> = [
            "--model",
            "opus",
            "--permission-mode",
            "bypassPermissions",
            "--dangerously-skip-permissions",
            "--settings=x.json",
            "--setting-sources",
            "user",
            "--allowedTools",
            "Bash",
            "Edit(*)",
            "--verbose",
            "--resume",
            "--add-dir",
            "../shared",
            "-c",
            "-p",
            "--tools=Bash",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            strip_reserved_flags(&args),
            ["--model", "opus", "--verbose", "--add-dir", "../shared"],
            "a value-less flag does not swallow the next flag"
        );
        assert_eq!(strip_reserved_flags(&[]), Vec::<String>::new());
    }

    #[test]
    fn short_flag_clusters_with_a_reserved_letter_are_reserved() {
        for cluster in ["-cp", "-rx", "-pc", "-xr", "-vc"] {
            let args = vec![cluster.to_string()];
            assert!(first_reserved_flag(&args).is_some(), "{cluster}");
            assert_eq!(
                strip_reserved_flags(&args),
                Vec::<String>::new(),
                "{cluster}"
            );
        }
        for fine in ["-v", "-xy", "-d", "--model", "-"] {
            let args = vec![fine.to_string()];
            assert_eq!(first_reserved_flag(&args), None, "{fine}");
            assert_eq!(strip_reserved_flags(&args), args, "{fine}");
        }
    }

    #[test]
    fn the_program_is_trimmed_when_read() {
        let c: Config = serde_json::from_str(r#"{"harness":{"program":" /opt/claude "}}"#).unwrap();
        assert_eq!(c.harness.program.as_deref(), Some("/opt/claude"));
    }
}
