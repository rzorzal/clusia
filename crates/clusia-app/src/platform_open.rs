//! Opens web pages in the default browser (`/usr/bin/open`). Tests insert `OpenUrls`, which
//! records the URLs instead.

use bevy::prelude::*;

/// When present, URLs are recorded here instead of opened (tests).
#[derive(Resource, Debug, Default, Clone, PartialEq, Eq)]
pub struct OpenUrls(pub Vec<String>);

/// The Notifications pane of System Settings.
pub const NOTIFICATION_SETTINGS: &str =
    "x-apple.systempreferences:com.apple.Notifications-Settings.extension";

/// Opens an `https://` URL, or the Notifications pane of System Settings; anything else is
/// refused (and logged).
pub fn open_url(url: &str) {
    if !url.starts_with("https://") && url != NOTIFICATION_SETTINGS {
        tracing::warn!(url, "not opening a non-https URL");
        return;
    }
    match std::process::Command::new("/usr/bin/open").arg(url).spawn() {
        Ok(mut child) => {
            // Reap it, so no zombie is left behind.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::warn!(error = %e, "cannot run /usr/bin/open"),
    }
}

/// Records `url` when `OpenUrls` exists, otherwise opens it.
pub fn visit(recorder: Option<ResMut<OpenUrls>>, url: &str) {
    match recorder {
        Some(mut urls) => urls.0.push(url.to_string()),
        None => open_url(url),
    }
}
