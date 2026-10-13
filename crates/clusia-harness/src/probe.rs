//! Finding out whether the configured program is a working Claude Code.

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
