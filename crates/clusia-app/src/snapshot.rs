//! What the window knows from the daemon, and what each event changes.

use clusia_core::{ActivitySummary, Config, PrSummary, ReviewState};
use clusia_protocol::{
    AuthInfo, Event, FirstRun, PermissionStatus, ReviewSummary, SyncState, SyncStatus, WindowTarget,
};

/// What the window knows about the Giphy key. The daemon's status gives `Missing` or `Set`;
/// `Rejected` is only ever derived by the Media page from a refusal the window saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GiphyKey {
    /// Not asked yet, or the answer was neither yes nor no (offline, rate limit).
    #[default]
    Unknown,
    Missing,
    Rejected,
    Set,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    pub config: Config,
    pub assigned: Vec<PrSummary>,
    pub mine: Vec<PrSummary>,
    pub reviews: Vec<ReviewSummary>,
    pub activity: Option<ActivitySummary>,
    pub sync: Option<SyncStatus>,
    pub auth: Option<AuthInfo>,
    pub giphy_key: GiphyKey,
    /// What the first-run screen shows; `None` until the daemon has answered.
    pub first_run: Option<FirstRun>,
    /// The first-run screen may still be on screen (the login was missing and **Continue** has
    /// not been pressed), so its status keeps being asked for.
    pub first_run_open: bool,
    /// `assigned` and `mine` hold real lists, not the empty defaults before the first sync.
    pub lists_loaded: bool,
    pub daemon_version: String,
    /// Whether macOS lets the tray post notifications, as the tray last reported it.
    pub notifications_permission: PermissionStatus,
}

impl Snapshot {
    /// GitHub refused the token or there is none. Unknown (nothing fetched yet) is not signed out.
    pub fn signed_out(&self) -> bool {
        self.sync
            .as_ref()
            .is_some_and(|s| s.state == SyncState::Unauthorized)
            || self.auth.as_ref().is_some_and(|a| a.source.is_none())
    }

    /// Whether the first-run status is worth asking the daemon for: while the login is missing
    /// (it opens the screen) and until **Continue** closes it. The answer costs a `gh` call and
    /// a folder scan, so it is not asked for once the screen is gone.
    pub fn first_run_wanted(&mut self) -> bool {
        if self.signed_out() {
            self.first_run_open = true;
        }
        self.first_run_open
    }
}

/// What must be fetched again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Refresh {
    pub config: bool,
    pub lists: bool,
    pub reviews: bool,
    pub activity: bool,
    pub auth: bool,
    pub giphy: bool,
    pub first_run: bool,
    pub status: bool,
}

impl Refresh {
    /// Everything but the lists (`ListPrs` waits for the first sync, so they come second).
    pub const STARTUP: Refresh = Refresh {
        config: true,
        lists: false,
        reviews: true,
        activity: true,
        auth: true,
        giphy: false,
        first_run: false,
        status: true,
    };

    pub fn merge(&mut self, other: Refresh) {
        self.config |= other.config;
        self.lists |= other.lists;
        self.reviews |= other.reviews;
        self.activity |= other.activity;
        self.auth |= other.auth;
        self.giphy |= other.giphy;
        self.first_run |= other.first_run;
        self.status |= other.status;
    }

    pub fn any(self) -> bool {
        self.config
            || self.lists
            || self.reviews
            || self.activity
            || self.auth
            || self.giphy
            || self.first_run
            || self.status
    }
}

/// Applies what `event` carries and returns what must be fetched, plus a window request.
pub fn apply(snap: &mut Snapshot, event: Event) -> (Refresh, Option<WindowTarget>) {
    let mut r = Refresh::default();
    match event {
        Event::ConfigChanged { key, .. } => {
            r.config = true;
            r.auth = key.starts_with("github.");
            r.first_run = r.auth || key == "repositories.roots";
        }
        Event::GiphyKeyChanged => r.giphy = true,
        Event::PrsUpdated { .. } => r.lists = true,
        Event::SyncChanged(status) => {
            r.auth = snap.sync.as_ref().map(|s| s.state) != Some(status.state);
            r.first_run = r.auth;
            snap.sync = Some(status);
        }
        Event::ReviewChanged { state, .. } => {
            r.reviews = true;
            r.activity = state == ReviewState::Published;
        }
        Event::ReviewOutdated { .. } => r.reviews = true,
        Event::WindowRequested { target } => return (r, Some(target)),
        // `Stopping` is handled by the bridge before `apply` (it closes the window).
        Event::LoadStep(_)
        | Event::Stopping
        | Event::Notify { .. }
        | Event::InboxChanged { .. }
        // The chat keeps its own state; nothing in the snapshot depends on it.
        | Event::AgentChunk { .. }
        | Event::AgentToolUse { .. }
        | Event::AgentDenied { .. }
        | Event::AgentSuggestion { .. }
        | Event::AgentDone { .. }
        | Event::AgentError { .. }
        | Event::SessionState { .. }
        | Event::PermissionRequested { .. }
        | Event::PermissionResolved { .. }
        | Event::RulesChanged { .. } => {}
    }
    (r, None)
}

/// Demo mode only: sets `key` the way the daemon does (parsed by the current value's type,
/// then validated). Live windows always go through the daemon.
pub fn apply_config_locally(config: &mut Config, key: &str, value: &str) -> Result<(), String> {
    let mut json = serde_json::to_value(&*config).map_err(|e| e.to_string())?;
    let mut slot = &mut json;
    for part in key.split('.') {
        slot = slot
            .get_mut(part)
            .ok_or_else(|| format!("unknown config key {key:?}"))?;
    }
    if slot.is_object() {
        return Err(format!("{key} is a section, not a value"));
    }
    *slot = if slot.is_string() {
        serde_json::Value::String(value.to_string())
    } else {
        serde_json::from_str(value).map_err(|_| format!("invalid value for {key}: {value:?}"))?
    };
    let updated: Config =
        serde_json::from_value(json).map_err(|e| format!("invalid value for {key}: {e}"))?;
    updated.validate()?;
    *config = updated;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::config::Theme as ThemeChoice;
    use clusia_protocol::SyncState;

    #[test]
    fn recent_emoji_apply_locally_as_json() {
        let mut c = Config::default();
        apply_config_locally(&mut c, "composer.recent_emoji", r#"["🐢","🚀"]"#).unwrap();
        assert_eq!(c.composer.recent_emoji, ["🐢", "🚀"]);
    }

    fn sync(state: SyncState) -> SyncStatus {
        SyncStatus {
            state,
            ..SyncStatus::default()
        }
    }

    #[test]
    fn the_giphy_key_follows_its_event() {
        let mut s = Snapshot::default();
        let (r, _) = apply(&mut s, Event::GiphyKeyChanged);
        assert_eq!(
            r,
            Refresh {
                giphy: true,
                ..Refresh::default()
            }
        );
        assert!(!Refresh::default().any());
        assert!(
            Refresh {
                giphy: true,
                ..Refresh::default()
            }
            .any()
        );
    }

    #[test]
    fn first_run_follows_the_login_and_the_folders() {
        let mut s = Snapshot::default();
        let (r, _) = apply(
            &mut s,
            Event::ConfigChanged {
                key: "repositories.roots".into(),
                value: "[]".into(),
            },
        );
        assert!(
            r.first_run && !r.auth,
            "folders change only the first-run answer"
        );
        let (r, _) = apply(
            &mut s,
            Event::ConfigChanged {
                key: "github.auth".into(),
                value: "pat".into(),
            },
        );
        assert!(r.first_run && r.auth);
        let (r, _) = apply(&mut s, Event::SyncChanged(sync(SyncState::Unauthorized)));
        assert!(r.first_run && r.auth, "the login state changed");
        let (r, _) = apply(&mut s, Event::SyncChanged(sync(SyncState::Unauthorized)));
        assert!(!r.first_run, "the same state again asks nothing");
    }

    #[test]
    fn events_say_what_to_fetch() {
        let mut s = Snapshot::default();
        let (r, show) = apply(
            &mut s,
            Event::ConfigChanged {
                key: "appearance.theme".into(),
                value: "dark".into(),
            },
        );
        assert_eq!(
            r,
            Refresh {
                config: true,
                ..Refresh::default()
            }
        );
        assert_eq!(show, None);
        let (r, _) = apply(
            &mut s,
            Event::ConfigChanged {
                key: "github.host".into(),
                value: "ghe.acme.dev".into(),
            },
        );
        assert!(r.config && r.auth, "a host change re-checks the login");
        let (r, _) = apply(
            &mut s,
            Event::PrsUpdated {
                assigned: 1,
                mine: 0,
            },
        );
        assert!(r.lists && !r.config);
        let (r, _) = apply(
            &mut s,
            Event::ReviewChanged {
                pr: "acme/widgets#7".parse().unwrap(),
                state: ReviewState::Published,
                items: 0,
            },
        );
        assert!(r.reviews && r.activity);
        let (r, _) = apply(
            &mut s,
            Event::ReviewChanged {
                pr: "acme/widgets#7".parse().unwrap(),
                state: ReviewState::Saved,
                items: 1,
            },
        );
        assert!(r.reviews && !r.activity);
    }

    #[test]
    fn sync_changes_apply_directly() {
        let mut s = Snapshot::default();
        let (r, _) = apply(&mut s, Event::SyncChanged(sync(SyncState::Unauthorized)));
        assert_eq!(s.sync.as_ref().unwrap().state, SyncState::Unauthorized);
        assert!(r.auth, "a new sync state re-checks the login");
        let (r, _) = apply(&mut s, Event::SyncChanged(sync(SyncState::Unauthorized)));
        assert!(!r.any(), "same state: nothing to fetch");
    }

    #[test]
    fn window_requests_come_out() {
        let mut s = Snapshot::default();
        let (r, show) = apply(
            &mut s,
            Event::WindowRequested {
                target: WindowTarget::Config,
            },
        );
        assert!(!r.any());
        assert_eq!(show, Some(WindowTarget::Config));
    }

    #[test]
    fn refresh_merges() {
        let mut a = Refresh {
            lists: true,
            ..Refresh::default()
        };
        a.merge(Refresh {
            auth: true,
            ..Refresh::default()
        });
        assert!(a.lists && a.auth && !a.config);
        let startup = Refresh::STARTUP;
        assert!(startup.config && !startup.lists);
    }

    #[test]
    fn notification_keys_apply_locally_like_the_daemon_does() {
        let mut c = Config::default();
        apply_config_locally(&mut c, "notifications.events.mentioned.sound", "false").unwrap();
        assert!(!c.notifications.events[&clusia_core::config::EventKind::Mentioned].sound);
        apply_config_locally(&mut c, "notifications.dnd.days", r#"["sat","sun"]"#).unwrap();
        assert_eq!(c.notifications.dnd.days.len(), 2);
        apply_config_locally(&mut c, "notifications.dnd.from", "20:30").unwrap();
        assert_eq!(c.notifications.dnd.from.to_string(), "20:30");
        assert!(apply_config_locally(&mut c, "notifications.dnd.to", "25:00").is_err());
        apply_config_locally(&mut c, "notifications.sound", "tick").unwrap();
        apply_config_locally(&mut c, "general.start_at_login", "false").unwrap();
        assert!(!c.general.start_at_login);
    }

    #[test]
    fn local_config_writes_follow_the_daemon_rules() {
        let mut c = Config::default();
        apply_config_locally(&mut c, "appearance.theme", "dark").unwrap();
        assert_eq!(c.appearance.theme, ThemeChoice::Dark);
        apply_config_locally(&mut c, "appearance.code_size", "16").unwrap();
        assert_eq!(c.appearance.code_size, 16);
        apply_config_locally(&mut c, "repositories.roots", r#"["~/a","~/b c"]"#).unwrap();
        assert_eq!(c.repositories.roots, ["~/a", "~/b c"]);
        apply_config_locally(&mut c, "notifications.dnd.enabled", "true").unwrap();
        assert!(c.notifications.dnd.enabled);
        let before = c.clone();
        for (key, value) in [
            ("appearance.code_size", "15"),
            ("appearance.theme", "purple"),
            ("github.poll_interval_secs", "abc"),
            ("github.poll_interval_secs", "5"),
            ("github", "x"),
            ("nope.key", "1"),
        ] {
            assert!(
                apply_config_locally(&mut c, key, value).is_err(),
                "{key} = {value}"
            );
        }
        assert_eq!(c, before, "refused writes change nothing");
    }

    fn signed_out_snapshot() -> Snapshot {
        Snapshot {
            auth: Some(AuthInfo {
                source: None,
                login: None,
                scopes: Vec::new(),
                error: Some("no GitHub token".into()),
            }),
            ..Snapshot::default()
        }
    }

    #[test]
    fn first_run_is_asked_only_while_it_is_needed() {
        let mut s = Snapshot::default();
        assert!(!s.first_run_wanted(), "nothing says the login is missing");

        let mut s = signed_out_snapshot();
        assert!(s.first_run_wanted(), "no login opens the first run");
        s.auth = None;
        assert!(
            s.first_run_wanted(),
            "it stays wanted after the login works, until Continue"
        );
        s.first_run_open = false;
        assert!(!s.first_run_wanted(), "Continue ends it");

        s.auth = signed_out_snapshot().auth;
        assert!(s.first_run_wanted(), "a login lost later opens it again");
    }
}
