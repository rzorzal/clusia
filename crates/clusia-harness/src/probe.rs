//! Finding out whether the configured program is a working Claude Code.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::command::set_env;

/// `<program> --version` with the same environment rules as a turn, and with nothing to read
/// and its output piped.
pub fn probe_command(program: &Path, base_env: &[(OsString, OsString)]) -> Command {
    let mut cmd = Command::new(program);
    cmd.arg("--version");
    set_env(&mut cmd, base_env);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// The version number (`2.1.294`) from what `--version` printed (`2.1.294 (Claude Code)`).
pub fn parse_probe(stdout: &str) -> Result<String, String> {
    let line = stdout.lines().map(str::trim).find(|l| !l.is_empty());
    match line {
        None => Err("the program printed nothing for --version".into()),
        Some(l) => match l.split_whitespace().next() {
            Some(version) if version.starts_with(|c: char| c.is_ascii_digit()) => {
                Ok(version.to_string())
            }
            _ => Err(format!("unexpected output for --version: {l}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Vec<(OsString, OsString)> {
        vec![("PATH".into(), "/usr/bin:/bin".into())]
    }

    #[test]
    fn the_probe_asks_for_the_version_only() {
        let cmd = probe_command(Path::new("/usr/local/bin/claude"), &env());
        assert_eq!(cmd.get_program(), "/usr/local/bin/claude");
        assert_eq!(cmd.get_args().collect::<Vec<_>>(), ["--version"]);
    }

    #[test]
    fn the_probe_drops_tokens_like_a_turn() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("print-env");
        std::fs::write(&script, "#!/bin/sh\nexec /usr/bin/env\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let base: Vec<(OsString, OsString)> = ["GH_TOKEN", "GITHUB_TOKEN", "CLUSIA_HOME", "KEEP"]
            .map(|k| (OsString::from(k), OsString::from("x")))
            .to_vec();
        let out = probe_command(&script, &base)
            .spawn()
            .unwrap()
            .wait_with_output()
            .unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.contains("KEEP=x"), "{text}");
        for gone in ["GH_TOKEN", "GITHUB_TOKEN", "CLUSIA_HOME"] {
            assert!(!text.contains(gone), "{gone}: {text}");
        }
        assert!(
            !text.contains("CARGO"),
            "the test process's own variables are not passed"
        );
    }

    #[test]
    fn the_version_number_is_read() {
        assert_eq!(parse_probe("2.1.294 (Claude Code)\n"), Ok("2.1.294".into()));
        assert_eq!(parse_probe("\n  2.0.1 \n"), Ok("2.0.1".into()));
        assert_eq!(parse_probe("2.1.294"), Ok("2.1.294".into()));
    }

    #[test]
    fn anything_else_is_not_a_version() {
        assert!(parse_probe("").unwrap_err().contains("nothing"));
        assert!(parse_probe("  \n").is_err());
        assert_eq!(
            parse_probe("command not found: claude"),
            Err("unexpected output for --version: command not found: claude".into())
        );
    }
}
