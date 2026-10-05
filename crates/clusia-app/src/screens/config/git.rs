//! Config › Git server: host, sign-in source and status, token paste/removal, sync interval.

use std::collections::HashMap;

use bevy::clipboard::Clipboard;
use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, observe};
use clusia_core::config::AuthSource;
use clusia_protocol::{Secret, SyncState, SyncStatus, TokenSource};
use clusia_view::status::{format_age, status_line};

use super::{field_row, heading, option_card, page_header, row, sends, setter};
use crate::bridge::{Ask, Asks, TOAST_SECS, Toast, Toasts};
use crate::fonts::UiFonts;
use crate::snapshot::Snapshot;
use crate::theme::Swatch;
use crate::ui::kit::{Type, Variant, button, text};

#[derive(Component, Debug)]
pub struct PasteToken;

/// The "Last sync" value; kept current without rebuilding the page.
#[derive(Component, Debug)]
pub struct LastSyncText;

pub fn refresh_last_sync(
    model: Res<crate::bridge::Model>,
    clock: Res<crate::clock::Clock>,
    mut texts: Query<&mut Text, With<LastSyncText>>,
) {
    for mut t in &mut texts {
        let now = last_sync(model.snapshot.sync.as_ref(), clock.now());
        if t.0 != now {
            t.0 = now;
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct GitView {
    pub host: String,
    pub host_error: Option<String>,
    pub auth: AuthSource,
    pub auth_error: Option<String>,
    /// Where the daemon actually got a token (env, gh or the Keychain).
    pub source: Option<TokenSource>,
    pub login: Option<String>,
    pub scopes: Vec<String>,
    pub problem: Option<String>,
    pub poll: String,
    pub poll_error: Option<String>,
}

pub fn view(snap: &Snapshot, rejected: &HashMap<String, String>) -> GitView {
    let g = &snap.config.github;
    let auth = snap.auth.as_ref();
    GitView {
        host: g.host.clone(),
        host_error: rejected.get("github.host").cloned(),
        auth: g.auth,
        auth_error: rejected.get("github.auth").cloned(),
        source: auth.and_then(|a| a.source),
        login: auth.and_then(|a| a.login.clone()),
        scopes: auth.map(|a| a.scopes.clone()).unwrap_or_default(),
        problem: auth.and_then(|a| a.error.clone()),
        poll: g.poll_interval_secs.to_string(),
        poll_error: rejected.get("github.poll_interval_secs").cloned(),
    }
}

/// The text beside "Last sync". It lives outside `GitView` (it changes with the clock), so
/// a rebuild never hangs on it: `refresh_last_sync` rewrites just this text.
pub fn last_sync(sync: Option<&SyncStatus>, now: i64) -> String {
    if let Some(s) = sync
        && s.state == SyncState::Unauthorized
    {
        return "Not signed in to GitHub — use a sign-in option above".to_string();
    }
    if let Some(s) = sync
        && s.state == SyncState::Online
    {
        return match s.last_sync_unix {
            Some(t) if now - t < 60 => "Online · synced just now".to_string(),
            Some(t) => format!("Online · synced {} ago", format_age(now, t)),
            None => "Online".to_string(),
        };
    }
    status_line(sync, now)
        .map(|l| l.text)
        .unwrap_or_else(|| "Online".to_string())
}

/// The status under a sign-in option, when that is where the daemon's token comes from.
pub fn signed_in_line(v: &GitView, source: TokenSource) -> Option<String> {
    if v.source != Some(source) {
        return None;
    }
    Some(match &v.login {
        Some(login) if v.scopes.is_empty() => format!("Signed in as @{login}"),
        Some(login) => format!("Signed in as @{login} · scopes {}", v.scopes.join(", ")),
        None => v
            .problem
            .clone()
            .unwrap_or_else(|| "A token is set, but GitHub has not confirmed it yet".into()),
    })
}

/// The clipboard must hold exactly one token-like word.
pub fn token_from_clipboard(text: &str) -> Result<Secret, &'static str> {
    let token = text.trim();
    if token.is_empty() {
        return Err("The clipboard is empty. Copy a GitHub token first.");
    }
    if token.chars().any(char::is_whitespace) {
        return Err("The clipboard does not hold a single token.");
    }
    Ok(Secret::from(token))
}

pub fn build(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &GitView, last_sync: &str) {
    page_header(
        p,
        fonts,
        "Git server",
        "Where your pull requests live. Clúsia only reads, and writes a review when you press Publish.",
    );
    p.spawn(Node {
        column_gap: px(10),
        ..default()
    })
    .with_children(|r| {
        for (name, hint, on) in [
            ("GitHub", "github.com and Enterprise", true),
            ("GitLab", "later", false),
            ("Bitbucket", "later", false),
        ] {
            r.spawn(option_card(on, 220.0)).with_children(|c| {
                let t = if on {
                    Type::STRONG
                } else {
                    Type::STRONG.ink(Swatch::Faint)
                };
                c.spawn(text(fonts, name, t));
                c.spawn(text(fonts, hint, Type::META));
            });
        }
    });
    field_row(
        p,
        fonts,
        "Host",
        "github.host",
        &v.host,
        300.0,
        "Enterprise: your server's address, e.g. github.acme.dev",
        &v.host_error,
    );
    row(
        p,
        fonts,
        "Sign in with",
        |r| {
            r.spawn(Node {
                flex_direction: FlexDirection::Column,
                row_gap: px(8),
                ..default()
            })
            .with_children(|c| {
                for (label, about, value, source) in [
                    ("GitHub CLI (gh)", "Uses `gh auth token` when gh is signed in.", "gh-cli", TokenSource::GhCli),
                    ("Personal access token", "Kept in the macOS Keychain and never shown again. Needs repo and read:org.", "pat", TokenSource::Pat),
                ] {
                    let selected = matches!(
                        (v.auth, value),
                        (AuthSource::GhCli, "gh-cli") | (AuthSource::Pat, "pat")
                    );
                    c.spawn((option_card(selected, 520.0), setter("github.auth", value)))
                        .with_children(|card| {
                            card.spawn(text(fonts, label, Type::STRONG));
                            card.spawn(text(fonts, about, Type::META));
                            if let Some(line) = signed_in_line(v, source) {
                                let ink = if v.login.is_some() { Swatch::Green } else { Swatch::Orange };
                                card.spawn(text(fonts, line, Type::BODY.ink(ink)));
                            }
                            if source == TokenSource::Pat {
                                card.spawn((button(fonts, "Paste a token", Variant::Secondary), PasteToken, observe(on_paste)));
                            }
                        });
                }
                if v.source == Some(TokenSource::Env) {
                    c.spawn(text(fonts, "Using CLUSIA_GITHUB_TOKEN from the environment.", Type::META));
                }
                if v.source.is_none()
                    && let Some(problem) = &v.problem
                {
                    c.spawn(text(fonts, problem.clone(), Type::BODY.ink(Swatch::Orange)));
                }
            });
        },
        "",
        v.auth_error.as_deref().map(|m| ("github.auth", m)),
    );
    heading(p, fonts, "Syncing");
    field_row(
        p,
        fonts,
        "Check for updates every",
        "github.poll_interval_secs",
        &v.poll,
        64.0,
        "seconds · 15 to 3600",
        &v.poll_error,
    );
    row(
        p,
        fonts,
        "Last sync",
        |r| {
            r.spawn((text(fonts, last_sync.to_string(), Type::BODY), LastSyncText));
        },
        "",
        None,
    );
    p.spawn(Node {
        column_gap: px(8),
        ..default()
    })
    .with_children(|r| {
        r.spawn((
            button(fonts, "Test connection", Variant::Secondary),
            sends(Ask::RefreshAuth),
        ));
        r.spawn((
            button(fonts, "Sync now", Variant::Secondary),
            sends(Ask::SyncNow),
        ));
        if v.source == Some(TokenSource::Pat) {
            r.spawn((
                button(fonts, "Remove the stored token", Variant::Danger),
                sends(Ask::ClearToken),
            ));
        }
    });
}

/// Reads the clipboard straight into `SetToken`. The token is never displayed or kept.
fn on_paste(
    _activate: On<Activate>,
    clipboard: Option<ResMut<Clipboard>>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    time: Res<Time>,
) {
    let read = match clipboard {
        Some(mut c) => match c.fetch_text().poll_result() {
            Some(Ok(text)) => Ok(text),
            _ => Err("Could not read the clipboard."),
        },
        None => Err("Could not read the clipboard."),
    };
    match read.and_then(|text| token_from_clipboard(&text)) {
        Ok(token) => asks.send(Ask::SetToken(token)),
        Err(message) => toasts.0.push(Toast {
            text: message.to_string(),
            warning: true,
            until: time.elapsed_secs_f64() + TOAST_SECS,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::testing::NOW;
    use clusia_protocol::AuthInfo;

    #[test]
    fn view_reads_config_status_and_refusals() {
        let snap = fixture::demo(NOW);
        let mut rejected = HashMap::new();
        rejected.insert(
            "github.host".to_string(),
            "github.host must not be empty".to_string(),
        );
        let v = view(&snap, &rejected);
        assert_eq!(v.host, "github.com");
        assert_eq!(
            v.host_error.as_deref(),
            Some("github.host must not be empty")
        );
        assert_eq!(v.auth, AuthSource::GhCli);
        assert_eq!(v.poll, "60");
        assert_eq!(last_sync(snap.sync.as_ref(), NOW), "Online · synced 1m ago");
        assert_eq!(
            signed_in_line(&v, TokenSource::GhCli).as_deref(),
            Some("Signed in as @rzorzal · scopes repo, read:org")
        );
        assert_eq!(signed_in_line(&v, TokenSource::Pat), None);
    }

    #[test]
    fn view_without_a_token() {
        let mut snap = fixture::demo(NOW);
        snap.auth = Some(AuthInfo {
            source: None,
            login: None,
            scopes: vec![],
            error: Some("no GitHub token".into()),
        });
        snap.sync = Some(SyncStatus {
            state: SyncState::Unauthorized,
            ..SyncStatus::default()
        });
        let v = view(&snap, &HashMap::new());
        assert_eq!(v.source, None);
        assert_eq!(v.problem.as_deref(), Some("no GitHub token"));
        assert_eq!(
            last_sync(snap.sync.as_ref(), NOW),
            "Not signed in to GitHub — use a sign-in option above"
        );
        assert_eq!(signed_in_line(&v, TokenSource::GhCli), None);
    }

    #[test]
    fn clipboard_tokens() {
        assert_eq!(
            token_from_clipboard(" ghp_abc123 \n").unwrap().expose(),
            "ghp_abc123"
        );
        assert!(token_from_clipboard("  ").is_err());
        assert!(token_from_clipboard("two words").is_err());
        assert!(token_from_clipboard("line1\nline2").is_err());
    }
}
