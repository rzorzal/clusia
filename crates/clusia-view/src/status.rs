//! One-line sync status, review counts and compact ages, worded the same in every client.

use clusia_protocol::{SyncState, SyncStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub text: String,
    pub tone: Tone,
}

pub fn week_label(published: u32) -> String {
    if published == 1 {
        "1 review this week".into()
    } else {
        format!("{published} reviews this week")
    }
}

pub fn status_line(sync: Option<&SyncStatus>, now: i64) -> Option<StatusLine> {
    let line = |text: &str, tone| {
        Some(StatusLine {
            text: text.to_string(),
            tone,
        })
    };
    let Some(s) = sync else {
        return line("Connecting to GitHub…", Tone::Neutral);
    };
    match s.state {
        SyncState::Online => None,
        SyncState::NotYet => line("Syncing with GitHub…", Tone::Neutral),
        SyncState::Offline => line("Offline — showing the last synced lists", Tone::Warning),
        SyncState::RateLimited => {
            let mins = s
                .next_sync_unix
                .map(|t| ((t - now).max(0) + 59) / 60)
                .unwrap_or(1)
                .max(1);
            Some(StatusLine {
                text: format!("GitHub rate limit — next sync in {mins} min"),
                tone: Tone::Warning,
            })
        }
        SyncState::Unauthorized => line(
            "Not signed in to GitHub — run: clusia auth login",
            Tone::Warning,
        ),
    }
}

/// Compact age: `now`, `5m`, `2h`, `3d`, `4w`.
pub fn format_age(now: i64, then: i64) -> String {
    let s = (now - then).max(0);
    match s {
        0..60 => "now".into(),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h", s / 3600),
        86_400..604_800 => format!("{}d", s / 86_400),
        _ => format!("{}w", s / 604_800),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_790_000_000;

    #[test]
    fn status_lines() {
        let s = |state, next: Option<i64>| SyncStatus {
            state,
            last_sync_unix: None,
            next_sync_unix: next,
            message: None,
            paused: false,
        };
        assert_eq!(
            status_line(None, NOW).unwrap().text,
            "Connecting to GitHub…"
        );
        assert_eq!(status_line(Some(&s(SyncState::Online, None)), NOW), None);
        assert_eq!(
            status_line(Some(&s(SyncState::NotYet, None)), NOW)
                .unwrap()
                .text,
            "Syncing with GitHub…"
        );
        let off = status_line(Some(&s(SyncState::Offline, None)), NOW).unwrap();
        assert_eq!(
            (off.text.as_str(), off.tone),
            ("Offline — showing the last synced lists", Tone::Warning)
        );
        assert_eq!(
            status_line(Some(&s(SyncState::RateLimited, Some(NOW + 61))), NOW)
                .unwrap()
                .text,
            "GitHub rate limit — next sync in 2 min"
        );
        assert_eq!(
            status_line(Some(&s(SyncState::RateLimited, None)), NOW)
                .unwrap()
                .text,
            "GitHub rate limit — next sync in 1 min"
        );
        let auth = status_line(Some(&s(SyncState::Unauthorized, None)), NOW).unwrap();
        assert_eq!(
            (auth.text.as_str(), auth.tone),
            (
                "Not signed in to GitHub — run: clusia auth login",
                Tone::Warning
            )
        );
    }

    #[test]
    fn ages() {
        assert_eq!(
            format_age(NOW, NOW + 30),
            "now",
            "clock skew is not negative"
        );
        assert_eq!(format_age(NOW, NOW - 59), "now");
        assert_eq!(format_age(NOW, NOW - 60), "1m");
        assert_eq!(format_age(NOW, NOW - 3599), "59m");
        assert_eq!(format_age(NOW, NOW - 3600), "1h");
        assert_eq!(format_age(NOW, NOW - 86_399), "23h");
        assert_eq!(format_age(NOW, NOW - 86_400), "1d");
        assert_eq!(format_age(NOW, NOW - 6 * 86_400), "6d");
        assert_eq!(format_age(NOW, NOW - 21 * 86_400), "3w");
    }
}
