//! The LaunchAgent that keeps `clusiad` alive. The installer and the daemon both render it
//! here, so the file never depends on who wrote it last.

use std::path::Path;

use plist::{Dictionary, Value};

/// The launchd label, also the file name without `.plist`.
pub const LABEL: &str = "io.github.rzorzal.clusia.daemon";

/// `PATH` for the daemon under launchd, which starts with a bare one: Homebrew first, so `gh`
/// is found.
pub const AGENT_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

#[derive(Debug, thiserror::Error)]
pub enum LaunchAgentError {
    #[error("not a LaunchAgent plist: {0}")]
    Invalid(String),
}

/// What goes into the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchAgent<'a> {
    /// The `clusiad` executable inside the bundle.
    pub daemon: &'a Path,
    /// Where launchd sends the daemon's stdout and stderr.
    pub log: &'a Path,
    /// Start the daemon when the user logs in.
    pub start_at_login: bool,
}

fn to_xml(dict: Dictionary) -> Result<String, LaunchAgentError> {
    let mut out = Vec::new();
    plist::to_writer_xml(&mut out, &Value::Dictionary(dict))
        .map_err(|e| LaunchAgentError::Invalid(e.to_string()))?;
    String::from_utf8(out).map_err(|e| LaunchAgentError::Invalid(e.to_string()))
}

/// The plist text: restart after a crash (not after a clean stop), interactive priority.
pub fn render(agent: &LaunchAgent<'_>) -> String {
    let mut keep_alive = Dictionary::new();
    keep_alive.insert("SuccessfulExit".into(), Value::Boolean(false));
    let mut env = Dictionary::new();
    env.insert("PATH".into(), Value::String(AGENT_PATH.into()));
    let log = Value::String(agent.log.display().to_string());
    let mut dict = Dictionary::new();
    dict.insert("Label".into(), Value::String(LABEL.into()));
    dict.insert(
        "ProgramArguments".into(),
        Value::Array(vec![Value::String(agent.daemon.display().to_string())]),
    );
    dict.insert("RunAtLoad".into(), Value::Boolean(agent.start_at_login));
    dict.insert("KeepAlive".into(), Value::Dictionary(keep_alive));
    dict.insert("ProcessType".into(), Value::String("Interactive".into()));
    dict.insert("StandardOutPath".into(), log.clone());
    dict.insert("StandardErrorPath".into(), log);
    dict.insert("EnvironmentVariables".into(), Value::Dictionary(env));
    to_xml(dict).unwrap_or_default()
}

fn parse(text: &str) -> Result<Dictionary, LaunchAgentError> {
    match Value::from_reader_xml(text.as_bytes()) {
        Ok(Value::Dictionary(dict)) => Ok(dict),
        Ok(_) => Err(LaunchAgentError::Invalid(
            "the root is not a dictionary".into(),
        )),
        Err(e) => Err(LaunchAgentError::Invalid(e.to_string())),
    }
}

/// Whether `text` is a property list whose `Label` is ours.
pub fn is_ours(text: &str) -> bool {
    parse(text).is_ok_and(|dict| dict.get("Label").and_then(Value::as_string) == Some(LABEL))
}

/// `existing` with `RunAtLoad` set to `on`; every other key is kept as it was.
pub fn with_start_at_login(existing: &str, on: bool) -> Result<String, LaunchAgentError> {
    let mut dict = parse(existing)?;
    if dict.get("Label").and_then(Value::as_string) != Some(LABEL) {
        return Err(LaunchAgentError::Invalid("the label is not ours".into()));
    }
    dict.insert("RunAtLoad".into(), Value::Boolean(on));
    to_xml(dict)
}

/// The `RunAtLoad` of a rendered agent, `None` when `text` is not one of ours.
pub fn start_at_login(text: &str) -> Option<bool> {
    let Value::Dictionary(dict) = Value::from_reader_xml(text.as_bytes()).ok()? else {
        return None;
    };
    dict.get("RunAtLoad").and_then(Value::as_boolean)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn agent(start_at_login: bool) -> String {
        let daemon = PathBuf::from("/Applications/Clusia.app/Contents/MacOS/clusiad");
        let log = PathBuf::from("/Users/maria/Library/Logs/Clusia/daemon.launchd.log");
        render(&LaunchAgent {
            daemon: &daemon,
            log: &log,
            start_at_login,
        })
    }

    #[test]
    fn the_label_names_the_agent_file() {
        assert_eq!(format!("{LABEL}.plist"), crate::LAUNCH_AGENT_FILE);
    }

    #[test]
    fn the_agent_restarts_after_a_crash_only() {
        let text = agent(true);
        let Value::Dictionary(d) = Value::from_reader_xml(text.as_bytes()).unwrap() else {
            panic!("not a dictionary");
        };
        assert_eq!(d.get("Label").and_then(Value::as_string), Some(LABEL));
        assert_eq!(d.get("RunAtLoad").and_then(Value::as_boolean), Some(true));
        assert_eq!(
            d.get("KeepAlive")
                .and_then(Value::as_dictionary)
                .and_then(|k| k.get("SuccessfulExit"))
                .and_then(Value::as_boolean),
            Some(false)
        );
        assert_eq!(
            d.get("ProcessType").and_then(Value::as_string),
            Some("Interactive")
        );
        let args = d.get("ProgramArguments").and_then(Value::as_array).unwrap();
        assert_eq!(
            args[0].as_string(),
            Some("/Applications/Clusia.app/Contents/MacOS/clusiad")
        );
        let path = d
            .get("EnvironmentVariables")
            .and_then(Value::as_dictionary)
            .and_then(|e| e.get("PATH"))
            .and_then(Value::as_string)
            .unwrap();
        assert!(path.starts_with("/opt/homebrew/bin:/usr/local/bin:"));
        for key in ["StandardOutPath", "StandardErrorPath"] {
            assert_eq!(
                d.get(key).and_then(Value::as_string),
                Some("/Users/maria/Library/Logs/Clusia/daemon.launchd.log")
            );
        }
    }

    #[test]
    fn start_at_login_only_changes_run_at_load() {
        let on = agent(true);
        let off = with_start_at_login(&on, false).unwrap();
        assert_eq!(start_at_login(&off), Some(false));
        assert_eq!(off, agent(false), "everything else is untouched");
        assert_eq!(with_start_at_login(&off, true).unwrap(), on);
    }

    #[test]
    fn only_our_label_is_ours() {
        assert!(is_ours(&agent(true)));
        let other = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>com.example.other</string></dict></plist>";
        assert!(!is_ours(other));
        assert!(!is_ours("not a plist"));
    }

    #[test]
    fn a_foreign_plist_is_refused() {
        let other = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>com.example.other</string></dict></plist>";
        assert!(with_start_at_login(other, true).is_err());
        assert!(with_start_at_login("not a plist", true).is_err());
        assert_eq!(start_at_login("not a plist"), None);
    }
}
