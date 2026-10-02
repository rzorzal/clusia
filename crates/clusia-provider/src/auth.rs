//! Which token to use, and where it comes from. The token value is never printed.

use std::fmt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use clusia_core::config::AuthSource;
use clusia_platform::{SecretError, SecretStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenOrigin {
    /// `CLUSIA_GITHUB_TOKEN`.
    Env,
    /// `gh auth token`.
    GhCli,
    /// Personal access token from the Keychain.
    Pat,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Token {
    secret: String,
    pub origin: TokenOrigin,
}

impl Token {
    pub fn new(secret: impl Into<String>, origin: TokenOrigin) -> Self {
        Self {
            secret: secret.into(),
            origin,
        }
    }

    pub fn secret(&self) -> &str {
        &self.secret
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Token({:?}, ***)", self.origin)
    }
}

pub struct TokenSources<'a> {
    pub env: Option<&'a str>,
    pub gh_program: &'a Path,
    pub secrets: &'a dyn SecretStore,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error(transparent)]
    Secrets(#[from] SecretError),
}

/// Env override → (`gh-cli` mode) `gh auth token` → PAT stored for `host`.
pub async fn resolve_token(
    auth: AuthSource,
    host: &str,
    src: &TokenSources<'_>,
) -> Result<Option<Token>, AuthError> {
    if let Some(t) = src.env.map(str::trim).filter(|t| !t.is_empty()) {
        return Ok(Some(Token::new(t, TokenOrigin::Env)));
    }
    if auth == AuthSource::GhCli
        && let Some(t) = gh_token(src.gh_program, host).await
    {
        return Ok(Some(Token::new(t, TokenOrigin::GhCli)));
    }
    Ok(src
        .secrets
        .get(host)?
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .map(|t| Token::new(t, TokenOrigin::Pat)))
}

/// `gh auth token --hostname <host>`; `None` when gh is missing, logged out, silent or slower than 5 s.
pub async fn gh_token(program: &Path, host: &str) -> Option<String> {
    let run = tokio::process::Command::new(program)
        .args(["auth", "token", "--hostname", host])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let token = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_platform::MemoryStore;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn fake_gh(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("gh");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn sources<'a>(
        env: Option<&'a str>,
        gh: &'a Path,
        secrets: &'a MemoryStore,
    ) -> TokenSources<'a> {
        TokenSources {
            env,
            gh_program: gh,
            secrets,
        }
    }

    #[tokio::test]
    async fn env_override_wins() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), "echo from-gh");
        let store = MemoryStore::default();
        store.set("github.com", "from-pat").unwrap();
        let t = resolve_token(
            AuthSource::GhCli,
            "github.com",
            &sources(Some(" from-env "), &gh, &store),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!((t.secret(), t.origin), ("from-env", TokenOrigin::Env));
    }

    #[tokio::test]
    async fn gh_cli_token_is_used_for_the_configured_host() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(
            dir.path(),
            r#"[ "$1 $2 $3 $4" = "auth token --hostname ghe.example.com" ] && echo gho_abc || exit 1"#,
        );
        let store = MemoryStore::default();
        let t = resolve_token(
            AuthSource::GhCli,
            "ghe.example.com",
            &sources(None, &gh, &store),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!((t.secret(), t.origin), ("gho_abc", TokenOrigin::GhCli));
    }

    #[tokio::test]
    async fn falls_back_to_pat_when_gh_is_missing_or_logged_out() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::default();
        store.set("github.com", "ghp_pat").unwrap();
        let missing = dir.path().join("no-such-gh");
        let logged_out = fake_gh(dir.path(), "echo 'not logged in' >&2; exit 1");
        for gh in [missing.as_path(), logged_out.as_path()] {
            let t = resolve_token(AuthSource::GhCli, "github.com", &sources(None, gh, &store))
                .await
                .unwrap()
                .unwrap();
            assert_eq!((t.secret(), t.origin), ("ghp_pat", TokenOrigin::Pat));
        }
    }

    #[tokio::test]
    async fn pat_source_skips_gh() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), "echo from-gh");
        let store = MemoryStore::default();
        store.set("github.com", "ghp_pat").unwrap();
        let t = resolve_token(AuthSource::Pat, "github.com", &sources(None, &gh, &store))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(t.origin, TokenOrigin::Pat);
    }

    #[tokio::test]
    async fn none_when_nothing_is_available() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), "exit 0");
        let store = MemoryStore::default();
        store.set("github.com", "   ").unwrap();
        assert!(
            resolve_token(
                AuthSource::GhCli,
                "github.com",
                &sources(Some(""), &gh, &store)
            )
            .await
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn debug_redacts_secret() {
        let t = Token::new("ghp_supersecret", TokenOrigin::Pat);
        let shown = format!("{t:?}");
        assert!(!shown.contains("supersecret"), "{shown}");
        assert!(shown.contains("Pat"));
    }
}
