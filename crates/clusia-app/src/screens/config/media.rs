//! Config › Media: the Giphy key and whether pictures from other sites load.

use std::collections::HashMap;

use bevy::clipboard::Clipboard;
use bevy::ecs::hierarchy::ChildSpawnerCommands;
use bevy::prelude::*;
use bevy::ui_widgets::{Activate, observe};
use clusia_protocol::Secret;

use super::{clipboard_text, heading, link, page_header, row, sends, setter, warn};
use crate::bridge::{Ask, Asks, GIPHY_KEY_REFUSAL, Model, Toasts};
use crate::fonts::UiFonts;
use crate::snapshot::{GiphyKey, Snapshot};
use crate::theme::Swatch;
use crate::ui::kit::{Type, Variant, button, card, text, toggle};

/// Reads the clipboard into `SetGiphyKey`.
#[derive(Component, Debug)]
pub struct PasteGiphyKey;

#[derive(Debug, Clone, PartialEq)]
pub struct MediaView {
    pub key: GiphyKey,
    pub load_external: bool,
    pub load_external_error: Option<String>,
}

/// `Rejected` only when the key is stored and a search the window ran came back refused.
pub fn view(snap: &Snapshot, rejected: &HashMap<String, String>) -> MediaView {
    let key = match snap.giphy_key {
        GiphyKey::Set if rejected.contains_key(GIPHY_KEY_REFUSAL) => GiphyKey::Rejected,
        other => other,
    };
    MediaView {
        key,
        load_external: snap.config.media.load_external_images,
        load_external_error: rejected.get("media.load_external_images").cloned(),
    }
}

/// The clipboard must hold one key-like word (Giphy keys are 32 letters and digits).
pub fn giphy_key_from_clipboard(text: &str) -> Result<Secret, &'static str> {
    let key = text.trim();
    if key.is_empty() {
        return Err("The clipboard is empty. Copy your Giphy API key first.");
    }
    if !(16..=64).contains(&key.len()) || !key.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("The clipboard does not hold a Giphy API key.");
    }
    Ok(Secret::from(key))
}

/// The status under the Giphy key, with its color.
pub fn key_line(key: GiphyKey) -> (&'static str, Swatch) {
    match key {
        GiphyKey::Set => ("Key saved in the macOS Keychain", Swatch::Green),
        GiphyKey::Missing => (
            "No key yet: the GIF button only takes a pasted link",
            Swatch::Muted,
        ),
        GiphyKey::Rejected => ("Giphy rejected the key. Paste a new one.", Swatch::Orange),
        GiphyKey::Unknown => ("The key could not be checked right now", Swatch::Muted),
    }
}

/// Sends a pasted key, or warns when the text is not one. `now` is `Time::elapsed_secs_f64`.
pub fn send_pasted_key(text: &str, asks: &mut Asks, toasts: &mut Toasts, now: f64) -> bool {
    match giphy_key_from_clipboard(text) {
        Ok(key) => {
            asks.send(Ask::SetGiphyKey(key));
            true
        }
        Err(message) => {
            warn(toasts, now, message);
            false
        }
    }
}

pub fn build(p: &mut ChildSpawnerCommands, fonts: &UiFonts, v: &MediaView) {
    page_header(
        p,
        fonts,
        "Media",
        "GIFs from Giphy and the pictures that comments show.",
    );
    heading(p, fonts, "Giphy");
    let (line, ink) = key_line(v.key);
    row(
        p,
        fonts,
        "Giphy key",
        |r| {
            r.spawn(Node {
                flex_direction: FlexDirection::Column,
                row_gap: px(8),
                ..default()
            })
            .with_children(|c| {
                c.spawn(text(fonts, line, Type::BODY.ink(ink)));
                c.spawn(Node {
                    column_gap: px(8),
                    ..default()
                })
                .with_children(|b| {
                    b.spawn((
                        button(fonts, "Paste key from clipboard", Variant::Secondary),
                        PasteGiphyKey,
                        observe(on_paste),
                    ));
                    if matches!(v.key, GiphyKey::Set | GiphyKey::Rejected) {
                        b.spawn((
                            button(fonts, "Remove key", Variant::Danger),
                            sends(Ask::ClearGiphyKey),
                        ));
                    }
                    b.spawn(link(
                        fonts,
                        "How to get a key",
                        "https://developers.giphy.com/",
                    ));
                });
            });
        },
        "",
        None,
    );
    p.spawn(card(Node {
        padding: UiRect::axes(px(16), px(12)),
        max_width: px(640),
        flex_direction: FlexDirection::Column,
        row_gap: px(4),
        ..default()
    }))
    .with_children(|c| {
        c.spawn(text(
            fonts,
            "The key stays in the Keychain and is never shown again. Results are rated PG-13.",
            Type::MUTED,
        ));
        c.spawn(text(fonts, "Powered by GIPHY", Type::META));
    });
    heading(p, fonts, "Pictures");
    row(
        p,
        fonts,
        "Load images from other sites",
        |r| {
            r.spawn((
                toggle(v.load_external),
                setter("media.load_external_images", (!v.load_external).to_string()),
            ));
        },
        "Off: pictures from sites other than GitHub and Giphy appear as links",
        v.load_external_error
            .as_deref()
            .map(|m| ("media.load_external_images", m)),
    );
}

/// Reads the clipboard straight into `SetGiphyKey`. The key is never displayed or kept.
fn on_paste(
    _activate: On<Activate>,
    clipboard: Option<ResMut<Clipboard>>,
    mut asks: ResMut<Asks>,
    mut toasts: ResMut<Toasts>,
    mut model: ResMut<Model>,
    time: Res<Time>,
) {
    let now = time.elapsed_secs_f64();
    match clipboard_text(clipboard) {
        Ok(text) => {
            if send_pasted_key(&text, &mut asks, &mut toasts, now) {
                // A new key gets a fresh verdict.
                model.rejected.remove(GIPHY_KEY_REFUSAL);
            }
        }
        Err(message) => warn(&mut toasts, now, message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::testing::NOW;

    const KEY: &str = "dc6zaTOxFJmzC1234567890abcdefghi";

    #[test]
    fn view_reads_key_status_and_setting() {
        let mut snap = fixture::demo(NOW);
        snap.config.media.load_external_images = true;
        let mut rejected = HashMap::new();
        rejected.insert("media.load_external_images".to_string(), "no".to_string());
        let v = view(&snap, &rejected);
        assert_eq!(v.key, GiphyKey::Set);
        assert!(v.load_external);
        assert_eq!(v.load_external_error.as_deref(), Some("no"));
    }

    #[test]
    fn only_a_refusal_the_window_saw_makes_a_key_rejected() {
        let mut snap = fixture::demo(NOW);
        let mut refused = HashMap::new();
        refused.insert(
            GIPHY_KEY_REFUSAL.to_string(),
            "Giphy rejected the key".to_string(),
        );
        assert_eq!(view(&snap, &refused).key, GiphyKey::Rejected);
        assert_eq!(view(&snap, &HashMap::new()).key, GiphyKey::Set);
        snap.giphy_key = GiphyKey::Missing;
        assert_eq!(
            view(&snap, &refused).key,
            GiphyKey::Missing,
            "no stored key, nothing to reject"
        );
    }

    #[test]
    fn key_status_lines() {
        assert_eq!(key_line(GiphyKey::Set).1, Swatch::Green);
        assert_eq!(key_line(GiphyKey::Rejected).1, Swatch::Orange);
        assert!(key_line(GiphyKey::Missing).0.starts_with("No key yet"));
        assert!(
            key_line(GiphyKey::Unknown)
                .0
                .contains("could not be checked")
        );
    }

    #[test]
    fn clipboard_keys() {
        assert_eq!(
            giphy_key_from_clipboard(&format!(" {KEY}\n"))
                .unwrap()
                .expose(),
            KEY
        );
        for bad in [
            "",
            "  ",
            "short",
            "two words in one line....",
            "key-with-dashes-0123456789abcdef",
            &"a".repeat(65),
        ] {
            assert!(giphy_key_from_clipboard(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_pasted_key_is_sent_and_a_bad_one_warns() {
        let mut asks = Asks::default();
        let mut toasts = Toasts::default();
        send_pasted_key("nope nope", &mut asks, &mut toasts, 10.0);
        assert!(asks.recorded.is_empty());
        assert_eq!(toasts.0.len(), 1);
        assert!(toasts.0[0].warning);
        assert_eq!(toasts.0[0].until, 10.0 + crate::bridge::TOAST_SECS);
        send_pasted_key(KEY, &mut asks, &mut toasts, 10.0);
        assert_eq!(asks.recorded, [Ask::SetGiphyKey(Secret::from(KEY))]);
        assert_eq!(toasts.0.len(), 1);
    }
}
