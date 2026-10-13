//! The agents Clúsia can run for a review, and what differs between them: names, programs,
//! version output, login check and the environment a child gets.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, Stdio};

use clusia_core::config::HarnessKind;
use clusia_protocol::AgentErrorKind;

use crate::command::set_env as claude_env;
use crate::probe::parse_probe;

/// Keys a Codex child never inherits, on top of `CLUSIA_*`: GitHub tokens, the keys Codex
/// reads from the environment instead of the user's login, and Claude Code's MCP timeout.
const CODEX_DROPPED: [&str; 6] = [
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "CODEX_API_KEY",
    "OPENAI_API_KEY",
    "MCP_TOOL_TIMEOUT",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    ClaudeCode,
    Codex,
}

impl From<HarnessKind> for Agent {
    fn from(kind: HarnessKind) -> Self {
        match kind {
            HarnessKind::ClaudeCode => Self::ClaudeCode,
            HarnessKind::Codex => Self::Codex,
        }
    }
}

impl From<Agent> for HarnessKind {
    fn from(agent: Agent) -> Self {
        match agent {
            Agent::ClaudeCode => HarnessKind::ClaudeCode,
            Agent::Codex => HarnessKind::Codex,
        }
    }
}

impl Agent {
    /// What the pages and messages call it.
    pub fn name(self) -> &'static str {
        HarnessKind::from(self).name()
    }

    /// The command looked up on `PATH` when Config sets no program.
    pub fn program(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn kind(self) -> HarnessKind {
        self.into()
    }

    /// The version number out of what `--version` printed.
    pub fn parse_version(self, stdout: &str) -> Result<String, String> {
        match self {
            Self::ClaudeCode => parse_probe(stdout),
            Self::Codex => parse_codex_version(stdout),
        }
    }

    /// The oldest version Clúsia works with, when there is one.
    pub fn min_version(self) -> Option<&'static str> {
        match self {
            Self::ClaudeCode => None,
            Self::Codex => Some("0.162.0"),
        }
    }

    /// The command that says whether the user is signed in, when the agent has one. It makes
    /// no model call; its output is piped.
    pub fn signed_in_command(
        self,
        program: &Path,
        base_env: &[(OsString, OsString)],
    ) -> Option<Command> {
        match self {
            Self::ClaudeCode => None,
            Self::Codex => {
                let mut cmd = Command::new(program);
                cmd.args(["login", "status"]);
                self.set_env(&mut cmd, base_env);
                cmd.stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                Some(cmd)
            }
        }
    }

    pub fn not_signed_in_hint(self) -> &'static str {
        self.kind().sign_in_hint()
    }

    /// The chat line for `kind` when the program said nothing more specific.
    pub fn message_for(self, kind: AgentErrorKind) -> String {
        kind.message_for(self.name(), self.not_signed_in_hint())
    }

    /// Gives `cmd` exactly `base_env` as its environment, minus what this agent must not
    /// inherit.
    pub fn set_env(self, cmd: &mut Command, base_env: &[(OsString, OsString)]) {
        match self {
            Self::ClaudeCode => claude_env(cmd, base_env),
            Self::Codex => {
                cmd.env_clear();
                for (key, value) in base_env {
                    if !codex_drops(key) {
                        cmd.env(key, value);
                    }
                }
            }
        }
    }
}

fn codex_drops(key: &OsStr) -> bool {
    let key = key.to_string_lossy();
    CODEX_DROPPED.contains(&key.as_ref()) || key.starts_with("CLUSIA_")
}

/// `0.162.1` out of `codex-cli 0.162.1` or a bare `0.162.1`.
fn parse_codex_version(stdout: &str) -> Result<String, String> {
    let line = stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .ok_or_else(|| "the program printed nothing for --version".to_string())?;
    let mut words = line.split_whitespace();
    let first = words.next().unwrap_or_default();
    let version = if first == "codex-cli" {
        words.next().unwrap_or_default()
    } else {
        first
    };
    let numeric = version.starts_with(|c: char| c.is_ascii_digit())
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    if numeric && version.contains('.') {
        Ok(version.to_string())
    } else {
        Err(format!("unexpected output for --version: {line}"))
    }
}

/// Whether `found` is at least `min`, comparing the numbers of their first three
/// dot-separated parts (`0.163.0-alpha.1` counts as `0.163.0`). A version that cannot be read
/// is not at least anything.
pub fn version_at_least(found: &str, min: &str) -> bool {
    fn triple(v: &str) -> Option<[u64; 3]> {
        let mut parts = v.split('.');
        let mut out = [0u64; 3];
        for slot in &mut out {
            let part = parts.next()?;
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            *slot = digits.parse().ok()?;
        }
        Some(out)
    }
    match (triple(found), triple(min)) {
        (Some(found), Some(min)) => found >= min,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_programs_and_kinds() {
        assert_eq!(Agent::ClaudeCode.name(), "Claude Code");
        assert_eq!(Agent::Codex.name(), "Codex");
        assert_eq!(Agent::ClaudeCode.program(), "claude");
        assert_eq!(Agent::Codex.program(), "codex");
        assert_eq!(Agent::from(HarnessKind::Codex), Agent::Codex);
        assert_eq!(Agent::from(HarnessKind::ClaudeCode), Agent::ClaudeCode);
        assert_eq!(Agent::Codex.kind(), HarnessKind::Codex);
        assert_eq!(Agent::ClaudeCode.min_version(), None);
        assert_eq!(Agent::Codex.min_version(), Some("0.162.0"));
        assert_eq!(
            Agent::ClaudeCode.not_signed_in_hint(),
            "Run `claude` once in a terminal"
        );
        assert_eq!(
            Agent::Codex.not_signed_in_hint(),
            "Run `codex login` once in a terminal"
        );
    }

    #[test]
    fn messages_carry_the_name_and_the_hint() {
        assert_eq!(
            Agent::Codex.message_for(AgentErrorKind::Crashed),
            "Codex stopped unexpectedly"
        );
        assert_eq!(
            Agent::Codex.message_for(AgentErrorKind::NotSignedIn),
            "Run `codex login` once in a terminal"
        );
        assert_eq!(
            Agent::ClaudeCode.message_for(AgentErrorKind::NotInstalled),
            "Install Claude Code or set its path"
        );
    }

    #[test]
    fn claude_keeps_its_version_parser() {
        assert_eq!(
            Agent::ClaudeCode.parse_version("2.1.294 (Claude Code)\n"),
            Ok("2.1.294".into())
        );
        assert!(
            Agent::ClaudeCode
                .parse_version("codex-cli 0.162.1")
                .is_err()
        );
    }

    #[test]
    fn codex_versions_are_read_with_or_without_the_name() {
        for (said, want) in [
            ("codex-cli 0.162.1\n", "0.162.1"),
            ("\n codex-cli 0.163.0-alpha.1 \n", "0.163.0-alpha.1"),
            ("0.162.1", "0.162.1"),
            ("codex-cli 0.162.1 (abc123)", "0.162.1"),
        ] {
            assert_eq!(Agent::Codex.parse_version(said), Ok(want.into()), "{said}");
        }
        for bad in [
            "",
            "  \n",
            "codex-cli",
            "command not found: codex",
            "codex 0.1.2",
        ] {
            assert!(Agent::Codex.parse_version(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            Agent::Codex.parse_version("command not found: codex"),
            Err("unexpected output for --version: command not found: codex".into())
        );
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert!(version_at_least("0.162.0", "0.162.0"));
        assert!(version_at_least("0.162.1", "0.162.0"));
        assert!(version_at_least("0.170.0", "0.162.0"));
        assert!(version_at_least("1.0.0", "0.162.0"));
        assert!(version_at_least("0.163.0-alpha.1", "0.162.0"));
        assert!(!version_at_least("0.150.0", "0.162.0"));
        assert!(!version_at_least("0.99.9", "0.162.0"));
        assert!(!version_at_least("0.9.0", "0.162.0"), "9 is less than 162");
        assert!(!version_at_least("codex", "0.162.0"));
        assert!(!version_at_least("0.162", "0.162.0"));
    }

    #[test]
    fn only_codex_has_a_login_check() {
        let env: Vec<(OsString, OsString)> = vec![("PATH".into(), "/usr/bin".into())];
        assert!(
            Agent::ClaudeCode
                .signed_in_command(Path::new("claude"), &env)
                .is_none()
        );
        let cmd = Agent::Codex
            .signed_in_command(Path::new("/opt/bin/codex"), &env)
            .expect("codex has one");
        assert_eq!(cmd.get_program(), "/opt/bin/codex");
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["login", "status"]);
    }

    fn printed_env(agent: Agent) -> String {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("print-env");
        std::fs::write(&script, "#!/bin/sh\nexec /usr/bin/env\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let base: Vec<(OsString, OsString)> = [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "CODEX_API_KEY",
            "OPENAI_API_KEY",
            "MCP_TOOL_TIMEOUT",
            "CLUSIA_HOME",
            "CODEX_HOME",
            "KEEP",
        ]
        .map(|k| (OsString::from(k), OsString::from("x")))
        .to_vec();
        let mut cmd = Command::new(&script);
        agent.set_env(&mut cmd, &base);
        let out = cmd.output().unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn codex_children_get_no_tokens_and_keep_their_home() {
        let text = printed_env(Agent::Codex);
        assert!(text.contains("KEEP=x"), "{text}");
        assert!(
            text.contains("CODEX_HOME=x"),
            "the user's home is left alone: {text}"
        );
        for gone in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "CODEX_API_KEY",
            "OPENAI_API_KEY",
            "MCP_TOOL_TIMEOUT",
            "CLUSIA_HOME",
        ] {
            assert!(!text.contains(&format!("{gone}=")), "{gone}: {text}");
        }
    }

    #[test]
    fn claude_children_keep_the_mcp_timeout() {
        let text = printed_env(Agent::ClaudeCode);
        assert!(text.contains("MCP_TOOL_TIMEOUT=x"), "{text}");
        assert!(text.contains("OPENAI_API_KEY=x"), "{text}");
        assert!(!text.contains("GH_TOKEN="), "{text}");
        assert!(!text.contains("CLUSIA_HOME="), "{text}");
    }
}
