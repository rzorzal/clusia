//! `config.toml`: load (with recovery), save, and dotted-key get/set for the CLI.

use std::fs;
use std::io;
use std::path::PathBuf;

use clusia_core::{Config, Paths};

use crate::atomic::{quarantine, write_atomic};
use crate::now_unix;

/// How a state file was obtained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loaded<T> {
    /// No file yet: defaults, nothing written.
    Fresh(T),
    /// Parsed and valid.
    Read(T),
    /// The file was unreadable; it was moved to `quarantined` and defaults are in use.
    Recovered {
        value: T,
        quarantined: PathBuf,
        error: String,
    },
}

impl<T> Loaded<T> {
    pub fn into_value(self) -> T {
        match self {
            Loaded::Fresh(v) | Loaded::Read(v) | Loaded::Recovered { value: v, .. } => v,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigKeyError {
    #[error("unknown config key {0:?}")]
    Unknown(String),
    #[error("invalid value for {key}: {message}")]
    Invalid { key: String, message: String },
}

pub fn load_config(paths: &Paths) -> io::Result<Loaded<Config>> {
    let path = paths.config_file();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Loaded::Fresh(Config::default()));
        }
        Err(e) => {
            return Err(io::Error::new(
                e.kind(),
                format!("cannot read {}: {e}", path.display()),
            ));
        }
    };
    let parsed = String::from_utf8(bytes)
        .map_err(|e| format!("not valid UTF-8: {e}"))
        .and_then(|text| {
            let mut file = toml::from_str::<toml::Table>(&text).map_err(|e| e.to_string())?;
            migrate_legacy_keys(&mut file);
            let config = toml::Value::Table(file.clone())
                .try_into::<Config>()
                .map_err(|e| e.to_string())?;
            config.validate()?;
            let unknown = unknown_keys(&file, &to_table(&config));
            if !unknown.is_empty() {
                tracing::warn!("ignoring unknown config keys: {}", unknown.join(", "));
            }
            Ok(config)
        });
    match parsed {
        Ok(c) => Ok(Loaded::Read(c)),
        Err(error) => {
            let quarantined = quarantine(&path, now_unix())?;
            tracing::warn!(%error, file = %quarantined.display(), "config.toml was unreadable; using defaults");
            Ok(Loaded::Recovered {
                value: Config::default(),
                quarantined,
                error,
            })
        }
    }
}

/// Moves keys written by older versions to where they live now: `notifications.do_not_disturb`
/// became `notifications.dnd.enabled`, unless the file already has a `dnd` section.
fn migrate_legacy_keys(file: &mut toml::Table) {
    let Some(notifications) = file
        .get_mut("notifications")
        .and_then(toml::Value::as_table_mut)
    else {
        return;
    };
    let Some(legacy) = notifications.remove("do_not_disturb") else {
        return;
    };
    if let toml::Value::Boolean(enabled) = legacy
        && !notifications.contains_key("dnd")
    {
        let mut dnd = toml::Table::new();
        dnd.insert("enabled".into(), toml::Value::Boolean(enabled));
        notifications.insert("dnd".into(), toml::Value::Table(dnd));
    }
}

/// Dotted paths of keys present in `file` but absent from `known` (typos that serde would silently ignore).
pub fn unknown_keys(file: &toml::Table, known: &toml::Table) -> Vec<String> {
    fn walk(file: &toml::Table, known: &toml::Table, prefix: &str, out: &mut Vec<String>) {
        for (key, value) in file {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match known.get(key) {
                None => out.push(path),
                Some(toml::Value::Table(known_sub)) => {
                    if let toml::Value::Table(sub) = value {
                        walk(sub, known_sub, &path, out);
                    }
                }
                Some(_) => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(file, known, "", &mut out);
    out
}

pub fn save_config(paths: &Paths, cfg: &Config) -> io::Result<()> {
    let text = toml::to_string_pretty(cfg).map_err(io::Error::other)?;
    write_atomic(&paths.config_file(), text.as_bytes())
}

fn to_table(cfg: &Config) -> toml::Table {
    match toml::Value::try_from(cfg) {
        Ok(toml::Value::Table(t)) => t,
        other => unreachable!("Config always serializes to a TOML table, got {other:?}"),
    }
}

fn lookup<'a>(table: &'a toml::Table, key: &str) -> Option<&'a toml::Value> {
    let mut parts = key.split('.');
    let mut current = table.get(parts.next()?)?;
    for part in parts {
        current = current.as_table()?.get(part)?;
    }
    Some(current)
}

fn render(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => s.clone(),
        toml::Value::Table(t) => toml::to_string_pretty(t).unwrap_or_default(),
        other => other.to_string(),
    }
}

pub fn get_value(cfg: &Config, key: &str) -> Result<String, ConfigKeyError> {
    let table = to_table(cfg);
    lookup(&table, key)
        .map(render)
        .ok_or_else(|| ConfigKeyError::Unknown(key.to_string()))
}

fn parse_like(current: &toml::Value, raw: &str) -> toml::Value {
    if current.is_str() {
        return toml::Value::String(raw.to_string());
    }
    toml::from_str::<toml::Table>(&format!("v = {raw}"))
        .ok()
        .and_then(|mut t| t.remove("v"))
        .unwrap_or_else(|| toml::Value::String(raw.to_string()))
}

pub fn set_value(cfg: &Config, key: &str, raw: &str) -> Result<Config, ConfigKeyError> {
    let unknown = || ConfigKeyError::Unknown(key.to_string());
    let invalid = |message: String| ConfigKeyError::Invalid {
        key: key.to_string(),
        message,
    };

    let mut table = to_table(cfg);
    let (section, leaf) = key.rsplit_once('.').unwrap_or(("", key));
    let parent: &mut toml::Table = if section.is_empty() {
        &mut table
    } else {
        let mut current = &mut table;
        for part in section.split('.') {
            current = current
                .get_mut(part)
                .and_then(toml::Value::as_table_mut)
                .ok_or_else(unknown)?;
        }
        current
    };
    let existing = parent.get(leaf).ok_or_else(unknown)?;
    if existing.is_table() {
        return Err(invalid("is a section; set one of its keys instead".into()));
    }
    let value = parse_like(existing, raw);
    parent.insert(leaf.to_string(), value);

    let updated: Config = toml::Value::Table(table)
        .try_into()
        .map_err(|e: toml::de::Error| invalid(e.to_string()))?;
    updated.validate().map_err(invalid)?;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::config::{Dnd, EventKind, OnOpen, SoundId, Theme, Weekday};

    #[test]
    fn recent_emoji_are_set_as_a_list() {
        let c = Config::default();
        let set = set_value(&c, "composer.recent_emoji", r#"["🐢","🚀"]"#).unwrap();
        assert_eq!(set.composer.recent_emoji, ["🐢", "🚀"]);
        assert_eq!(
            get_value(&set, "composer.recent_emoji").as_deref(),
            Ok(r#"["🐢", "🚀"]"#)
        );
        let many = format!("[{}]", vec![r#""x""#; 17].join(","));
        assert!(set_value(&c, "composer.recent_emoji", &many).is_err());
    }

    fn paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::new(dir.path());
        (dir, p)
    }

    #[test]
    fn new_appearance_and_list_keys_round_trip() {
        let c = Config::default();
        assert_eq!(get_value(&c, "appearance.code_size").unwrap(), "13");
        let c = set_value(&c, "appearance.code_size", "16").unwrap();
        assert_eq!(c.appearance.code_size, 16);
        assert!(set_value(&c, "appearance.code_size", "15").is_err());
        let c = set_value(&c, "appearance.density", "compact").unwrap();
        assert_eq!(get_value(&c, "appearance.density").unwrap(), "compact");
        assert!(set_value(&c, "appearance.diff_view", "sideways").is_err());
        let c = set_value(&c, "lists.mine_sort", "oldest").unwrap();
        assert_eq!(get_value(&c, "lists.mine_sort").unwrap(), "oldest");
    }

    #[test]
    fn missing_file_gives_fresh_defaults_without_writing() {
        let (_d, p) = paths();
        assert_eq!(load_config(&p).unwrap(), Loaded::Fresh(Config::default()));
        assert!(!p.config_file().exists());
    }

    #[test]
    fn save_then_load_round_trips() {
        let (_d, p) = paths();
        let mut c = Config::default();
        c.github.poll_interval_secs = 120;
        save_config(&p, &c).unwrap();
        assert_eq!(load_config(&p).unwrap(), Loaded::Read(c));
    }

    #[test]
    fn partial_file_fills_defaults() {
        let (_d, p) = paths();
        fs::create_dir_all(p.root()).unwrap();
        fs::write(p.config_file(), "[appearance]\ntheme = \"dark\"\n").unwrap();
        let c = load_config(&p).unwrap().into_value();
        assert_eq!(c.appearance.theme, Theme::Dark);
        assert_eq!(c.github.poll_interval_secs, 60);
    }

    #[test]
    fn corrupt_file_is_quarantined() {
        let (_d, p) = paths();
        fs::create_dir_all(p.root()).unwrap();
        fs::write(p.config_file(), "[github\nhost = ").unwrap();
        match load_config(&p).unwrap() {
            Loaded::Recovered {
                value,
                quarantined,
                error,
            } => {
                assert_eq!(value, Config::default());
                assert!(
                    quarantined
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("config.toml.corrupt-")
                );
                assert!(quarantined.exists());
                assert!(!error.is_empty());
            }
            other => panic!("expected Recovered, got {other:?}"),
        }
        assert!(!p.config_file().exists());
    }

    #[test]
    fn non_utf8_file_is_quarantined() {
        let (_d, p) = paths();
        fs::create_dir_all(p.root()).unwrap();
        fs::write(
            p.config_file(),
            b"[editor]\ncustom_command = \"\xe9 {path}\"\n",
        )
        .unwrap();
        match load_config(&p).unwrap() {
            Loaded::Recovered { quarantined, .. } => {
                assert!(
                    quarantined
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("config.toml.corrupt-")
                );
            }
            other => panic!("expected Recovered, got {other:?}"),
        }
        assert!(!p.config_file().exists());
    }

    #[test]
    fn unreadable_path_error_names_the_file() {
        let (_d, p) = paths();
        fs::create_dir_all(p.config_file()).unwrap();
        let err = load_config(&p).unwrap_err();
        assert!(err.to_string().contains("config.toml"), "{err}");
    }

    #[test]
    fn unknown_keys_are_reported_with_dotted_paths() {
        let file: toml::Table =
            toml::from_str("[github]\npoll_intervall_secs = 5\nhost = \"x\"\n").unwrap();
        let known = to_table(&Config::default());
        assert_eq!(
            unknown_keys(&file, &known),
            vec!["github.poll_intervall_secs".to_string()]
        );
        assert!(unknown_keys(&known, &known).is_empty());
    }

    #[test]
    fn invalid_value_in_file_is_quarantined() {
        let (_d, p) = paths();
        fs::create_dir_all(p.root()).unwrap();
        fs::write(p.config_file(), "[github]\npoll_interval_secs = 1\n").unwrap();
        assert!(matches!(load_config(&p).unwrap(), Loaded::Recovered { .. }));
    }

    #[test]
    fn get_value_renders_scalars() {
        let c = Config::default();
        assert_eq!(get_value(&c, "github.poll_interval_secs").unwrap(), "60");
        assert_eq!(get_value(&c, "github.host").unwrap(), "github.com");
        assert_eq!(get_value(&c, "appearance.theme").unwrap(), "system");
        assert_eq!(get_value(&c, "notifications.dnd.enabled").unwrap(), "false");
        assert_eq!(get_value(&c, "notifications.dnd.from").unwrap(), "19:00");
        assert_eq!(get_value(&c, "notifications.sound").unwrap(), "leaf");
        assert_eq!(get_value(&c, "general.start_at_login").unwrap(), "true");
        assert_eq!(
            get_value(&c, "notifications.events.mentioned.sound").unwrap(),
            "true"
        );
        assert!(
            get_value(&c, "repositories.roots")
                .unwrap()
                .contains("~/Repos")
        );
        assert!(
            get_value(&c, "github")
                .unwrap()
                .contains("poll_interval_secs = 60")
        );
    }

    #[test]
    fn get_value_unknown_key() {
        let c = Config::default();
        assert_eq!(
            get_value(&c, "github.nope"),
            Err(ConfigKeyError::Unknown("github.nope".into()))
        );
        assert!(matches!(get_value(&c, ""), Err(ConfigKeyError::Unknown(_))));
    }

    #[test]
    fn set_value_parses_by_existing_type() {
        let c = Config::default();
        let c2 = set_value(&c, "github.poll_interval_secs", "120").unwrap();
        assert_eq!(c2.github.poll_interval_secs, 120);
        assert_eq!(c.github.poll_interval_secs, 60, "input is not mutated");
        assert_eq!(
            set_value(&c, "appearance.theme", "dark")
                .unwrap()
                .appearance
                .theme,
            Theme::Dark
        );
        assert_eq!(
            set_value(&c, "github.host", "123").unwrap().github.host,
            "123"
        );
        assert!(
            set_value(&c, "notifications.dnd.enabled", "true")
                .unwrap()
                .notifications
                .dnd
                .enabled
        );
        assert_eq!(
            set_value(&c, "repositories.roots", r#"["~/a"]"#)
                .unwrap()
                .repositories
                .roots,
            vec!["~/a"]
        );
    }

    #[test]
    fn the_media_toggle_is_settable_and_readable() {
        let c = Config::default();
        assert_eq!(
            get_value(&c, "media.load_external_images").unwrap(),
            "false"
        );
        let on = set_value(&c, "media.load_external_images", "true").unwrap();
        assert!(on.media.load_external_images);
        assert_eq!(
            get_value(&on, "media.load_external_images").unwrap(),
            "true"
        );
        assert!(matches!(
            set_value(&c, "media.load_external_images", "maybe"),
            Err(ConfigKeyError::Invalid { .. })
        ));
    }

    #[test]
    fn set_value_rejects_wrong_types_and_rule_violations() {
        let c = Config::default();
        assert!(matches!(
            set_value(&c, "github.poll_interval_secs", "abc"),
            Err(ConfigKeyError::Invalid { .. })
        ));
        assert!(matches!(
            set_value(&c, "github.poll_interval_secs", "5"),
            Err(ConfigKeyError::Invalid { .. })
        ));
        assert!(matches!(
            set_value(&c, "appearance.theme", "purple"),
            Err(ConfigKeyError::Invalid { .. })
        ));
        assert!(matches!(
            set_value(&c, "github", "x"),
            Err(ConfigKeyError::Invalid { .. })
        ));
        assert_eq!(
            set_value(&c, "nope.x", "1"),
            Err(ConfigKeyError::Unknown("nope.x".into()))
        );
    }

    #[test]
    fn list_preferences_round_trip_through_keys() {
        let c = Config::default();
        let c = set_value(&c, "lists.assigned_sort", "repository").unwrap();
        assert_eq!(get_value(&c, "lists.assigned_sort").unwrap(), "repository");
        let c = set_value(&c, "lists.filter", "auth refresh").unwrap();
        assert_eq!(get_value(&c, "lists.filter").unwrap(), "auth refresh");
        assert!(set_value(&c, "lists.saved_sort", "sideways").is_err());
        assert!(set_value(&c, "lists.repository", "nope").is_err());
    }

    #[test]
    fn notification_settings_are_set_leaf_by_leaf() {
        let c = Config::default();
        let c = set_value(&c, "notifications.events.checks_failed.macos", "true").unwrap();
        assert!(c.notifications.route(EventKind::ChecksFailed).macos);
        assert!(!c.notifications.route(EventKind::ChecksFailed).sound);
        let c = set_value(&c, "notifications.sound", "chime").unwrap();
        assert_eq!(c.notifications.sound, SoundId::Chime);
        assert!(set_value(&c, "notifications.sound", "bell").is_err());
        let c = set_value(&c, "notifications.dnd.from", "22:30").unwrap();
        assert_eq!(c.notifications.dnd.from.to_string(), "22:30");
        assert!(set_value(&c, "notifications.dnd.to", "25:00").is_err());
        let c = set_value(&c, "notifications.dnd.days", r#"["sat","sun"]"#).unwrap();
        assert_eq!(
            c.notifications.dnd.days.iter().copied().collect::<Vec<_>>(),
            [Weekday::Sat, Weekday::Sun]
        );
        assert!(set_value(&c, "notifications.dnd.days", r#"["funday"]"#).is_err());
        let c = set_value(&c, "notifications.follow_focus", "false").unwrap();
        assert!(!c.notifications.follow_focus);
        let c = set_value(&c, "general.start_at_login", "false").unwrap();
        assert!(!c.general.start_at_login);
        assert!(matches!(
            set_value(&c, "notifications.events.nope.tray", "true"),
            Err(ConfigKeyError::Unknown(_))
        ));
        assert!(matches!(
            set_value(&c, "notifications.events", "1"),
            Err(ConfigKeyError::Invalid { .. })
        ));
    }

    #[test]
    fn saved_notification_settings_round_trip() {
        let (_d, p) = paths();
        let mut c = Config::default();
        c.notifications.dnd.enabled = true;
        c.notifications.sound = SoundId::Drop;
        c.notifications
            .events
            .get_mut(&EventKind::Mentioned)
            .unwrap()
            .sound = false;
        save_config(&p, &c).unwrap();
        assert_eq!(load_config(&p).unwrap(), Loaded::Read(c));
    }

    #[test]
    fn the_old_do_not_disturb_switch_becomes_dnd_enabled() {
        let (_d, p) = paths();
        fs::create_dir_all(p.root()).unwrap();
        fs::write(p.config_file(), "[notifications]\ndo_not_disturb = true\n").unwrap();
        let loaded = load_config(&p).unwrap();
        assert!(matches!(loaded, Loaded::Read(_)), "not a recovery");
        let n = loaded.into_value().notifications;
        assert!(n.dnd.enabled);
        assert_eq!(n.dnd.from, Dnd::default().from, "the rest keeps defaults");

        fs::write(p.config_file(), "[notifications]\ndo_not_disturb = false\n").unwrap();
        assert!(
            !load_config(&p)
                .unwrap()
                .into_value()
                .notifications
                .dnd
                .enabled
        );

        fs::write(
            p.config_file(),
            "[notifications]\ndo_not_disturb = true\n[notifications.dnd]\nfrom = \"20:00\"\n",
        )
        .unwrap();
        let n = load_config(&p).unwrap().into_value().notifications;
        assert!(!n.dnd.enabled, "an explicit dnd section wins");
        assert_eq!(n.dnd.from.to_string(), "20:00");
    }

    #[test]
    fn a_bad_time_in_the_file_is_recovered() {
        let (_d, p) = paths();
        fs::create_dir_all(p.root()).unwrap();
        fs::write(p.config_file(), "[notifications.dnd]\nfrom = \"late\"\n").unwrap();
        assert!(matches!(load_config(&p).unwrap(), Loaded::Recovered { .. }));
    }

    #[test]
    fn harness_keys_are_read_and_set_by_name() {
        let c = Config::default();
        assert_eq!(get_value(&c, "harness.kind").unwrap(), "claude-code");
        assert_eq!(get_value(&c, "harness.program").unwrap(), "");
        assert_eq!(get_value(&c, "harness.extra_args").unwrap(), "");
        assert_eq!(get_value(&c, "harness.on_open").unwrap(), "summarize");
        assert_eq!(
            get_value(&c, "harness.use_cli_permissions").unwrap(),
            "true"
        );
        assert_eq!(get_value(&c, "harness.turn_timeout_secs").unwrap(), "600");

        let c = set_value(&c, "harness.program", "/opt/bin/claude").unwrap();
        assert_eq!(c.harness.program.as_deref(), Some("/opt/bin/claude"));
        let c = set_value(&c, "harness.program", "").unwrap();
        assert_eq!(c.harness.program, None, "an empty value goes back to PATH");
        let c = set_value(&c, "harness.extra_args", "--model opus").unwrap();
        assert_eq!(c.harness.extra_args, "--model opus");
        let c = set_value(&c, "harness.on_open", "wait").unwrap();
        assert_eq!(c.harness.on_open, OnOpen::Wait);
        let c = set_value(&c, "harness.use_cli_permissions", "false").unwrap();
        assert!(!c.harness.use_cli_permissions);
        let c = set_value(&c, "harness.turn_timeout_secs", "120").unwrap();
        assert_eq!(c.harness.turn_timeout_secs, 120);
    }

    #[test]
    fn harness_values_are_validated() {
        let c = Config::default();
        for (key, value) in [
            ("harness.turn_timeout_secs", "59"),
            ("harness.turn_timeout_secs", "3601"),
            ("harness.turn_timeout_secs", "soon"),
            ("harness.on_open", "sometimes"),
            ("harness.kind", "codex"),
            ("harness.extra_args", "--x 'open"),
            ("harness.extra_args", "--dangerously-skip-permissions"),
            ("harness.use_cli_permissions", "maybe"),
        ] {
            assert!(
                matches!(
                    set_value(&c, key, value),
                    Err(ConfigKeyError::Invalid { .. })
                ),
                "{key}={value}"
            );
        }
        assert!(matches!(
            set_value(&c, "harness.nope", "1"),
            Err(ConfigKeyError::Unknown(_))
        ));
    }

    #[test]
    fn harness_section_survives_save_and_load() {
        let (_d, p) = paths();
        let mut c = Config::default();
        c.harness.program = Some("/opt/bin/claude".into());
        c.harness.on_open = OnOpen::Wait;
        save_config(&p, &c).unwrap();
        assert_eq!(load_config(&p).unwrap(), Loaded::Read(c));
        let defaults = Config::default();
        save_config(&p, &defaults).unwrap();
        let text = fs::read_to_string(p.config_file()).unwrap();
        assert!(text.contains("program = \"\""), "{text}");
        assert_eq!(load_config(&p).unwrap(), Loaded::Read(defaults));
    }
}
