//! The command line of `codex app-server`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use clusia_core::config::{HarnessKind, strip_reserved_flags_for};

use crate::Agent;

/// What it takes to start one `codex app-server`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexServerSpec {
    /// The `codex` to run.
    pub program: PathBuf,
    /// The pull request's worktree.
    pub cwd: PathBuf,
    /// The user's extra arguments, already split into words.
    pub extra_args: Vec<String>,
    /// The environment to start from, usually the daemon's.
    pub base_env: Vec<(OsString, OsString)>,
}

/// The `codex app-server` process of one review. Reading its output and ending it are up to
/// the caller; it leads its own process group so a kill reaches the tools it started too.
///
/// Nothing here widens what Codex may do: the reserved flags are dropped from the extra
/// arguments, so no sandbox, approval or config flag reaches the command line (every thread
/// carries its own), and the environment loses the GitHub and OpenAI credentials. `CODEX_HOME`
/// is whatever `base_env` holds, so the user's login is the one Codex finds.
pub fn codex_server_command(spec: &CodexServerSpec) -> Command {
    let mut cmd = Command::new(&spec.program);
    cmd.arg("app-server");
    cmd.args(strip_reserved_flags_for(
        HarnessKind::Codex,
        &spec.extra_args,
    ));
    cmd.current_dir(&spec.cwd);
    Agent::Codex.set_env(&mut cmd, &spec.base_env);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn spec() -> CodexServerSpec {
        CodexServerSpec {
            program: PathBuf::from("/usr/local/bin/codex"),
            cwd: PathBuf::from("/tmp/acme-widgets"),
            extra_args: Vec::new(),
            base_env: vec![
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("HOME".into(), "/tmp/maria".into()),
            ],
        }
    }

    fn argv(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_command_is_app_server_in_the_worktree() {
        let cmd = codex_server_command(&spec());
        assert_eq!(cmd.get_program(), OsStr::new("/usr/local/bin/codex"));
        assert_eq!(argv(&cmd), ["app-server"]);
        assert_eq!(
            cmd.get_current_dir(),
            Some(std::path::Path::new("/tmp/acme-widgets"))
        );
    }

    #[test]
    fn the_extra_arguments_come_after_app_server_without_the_reserved_ones() {
        let mut spec = spec();
        spec.extra_args = [
            "--search",
            "--dangerously-bypass-approvals-and-sandbox",
            "--yolo",
            "--listen",
            "ws://127.0.0.1:9",
        ]
        .map(String::from)
        .to_vec();
        let words = argv(&codex_server_command(&spec));
        assert_eq!(words[0], "app-server");
        assert_eq!(words[1], "--search");
        for reserved in [
            "--dangerously-bypass-approvals-and-sandbox",
            "--yolo",
            "--listen",
        ] {
            assert!(
                !words.iter().any(|w| w == reserved),
                "{reserved}: {words:?}"
            );
        }
        assert!(!words.iter().any(|w| w.starts_with("ws://")), "{words:?}");
    }

    /// The environment a program started by `codex_server_command` sees.
    fn env_seen(base: &[(&str, &str)]) -> String {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("print-env");
        std::fs::write(&script, "#!/bin/sh\nexec /usr/bin/env\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut spec = spec();
        spec.program = script;
        spec.cwd = dir.path().to_path_buf();
        spec.base_env = base
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect();
        let out = codex_server_command(&spec)
            .spawn()
            .unwrap()
            .wait_with_output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    #[test]
    fn the_environment_has_no_github_or_openai_credentials() {
        let seen = env_seen(&[
            ("KEEP", "x"),
            ("GH_TOKEN", "a"),
            ("GITHUB_TOKEN", "b"),
            ("GH_ENTERPRISE_TOKEN", "c"),
            ("CODEX_API_KEY", "d"),
            ("OPENAI_API_KEY", "e"),
            ("MCP_TOOL_TIMEOUT", "1000"),
            ("CLUSIA_HOME", "/tmp/clusia"),
        ]);
        assert!(seen.contains("KEEP=x"), "{seen}");
        for gone in [
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GH_ENTERPRISE_TOKEN",
            "CODEX_API_KEY",
            "OPENAI_API_KEY",
            "MCP_TOOL_TIMEOUT",
            "CLUSIA_HOME",
        ] {
            assert!(!seen.contains(gone), "{gone}: {seen}");
        }
        assert!(
            !seen.contains("CARGO"),
            "the test's own variables are not passed"
        );
    }

    #[test]
    fn codex_home_is_never_set_by_clusia() {
        assert!(!env_seen(&[("KEEP", "x")]).contains("CODEX_HOME"));
        let kept = env_seen(&[("CODEX_HOME", "/tmp/maria/.codex")]);
        assert!(kept.contains("CODEX_HOME=/tmp/maria/.codex"), "{kept}");
    }

    #[test]
    fn the_process_leads_its_own_group() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("show-group");
        std::fs::write(&script, "#!/bin/sh\nps -o pgid= -p $$\nps -o pid= -p $$\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut spec = spec();
        spec.program = script;
        spec.cwd = dir.path().to_path_buf();
        let out = codex_server_command(&spec)
            .spawn()
            .unwrap()
            .wait_with_output()
            .unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        let ids: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(ids.len(), 2, "{text}");
        assert_eq!(ids[0], ids[1], "the group id is its own pid");
    }
}
