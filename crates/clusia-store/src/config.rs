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
            let config = toml::from_str::<Config>(&text).map_err(|e| e.to_string())?;
            config.validate()?;
            if let Ok(file) = toml::from_str::<toml::Table>(&text) {
                let unknown = unknown_keys(&file, &to_table(&config));
                if !unknown.is_empty() {
                    tracing::warn!("ignoring unknown config keys: {}", unknown.join(", "));
                }
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
    use clusia_core::config::Theme;

    fn paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::new(dir.path());
        (dir, p)
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
        assert_eq!(
            get_value(&c, "notifications.do_not_disturb").unwrap(),
            "false"
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
            set_value(&c, "notifications.do_not_disturb", "true")
                .unwrap()
                .notifications
                .do_not_disturb
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
}
