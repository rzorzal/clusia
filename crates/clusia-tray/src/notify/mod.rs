//! Notifications: the daemon decides, the tray posts. This module holds the decisions that
//! belong to the poster (once per id, permission reporting, the click payload); `un` is the
//! system implementation.

use std::collections::VecDeque;
use std::time::Duration;

pub use clusia_protocol::message::{OpenTarget, PermissionStatus};

mod un;
pub use un::UNPoster;

/// Key of the `userInfo` entry that carries the click target as JSON.
pub const OPEN_KEY: &str = "open";
/// The permission prompt waits this long after start, so the banner is noticed.
pub const AUTH_DELAY: Duration = Duration::from_secs(15);
/// Ids remembered for dropping a repeated `Notify` (a reconnecting daemon may resend one).
const REMEMBERED: usize = 64;

/// One banner, as the daemon sent it.
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub body: String,
    /// A bundled sound id (`leaf`, `drop`, …); `None` is silent.
    pub sound: Option<String>,
    pub open: OpenTarget,
    /// The user does not follow macOS Focus: the banner may break through one.
    pub time_sensitive: bool,
}

/// How strongly a banner interrupts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interruption {
    /// Silenced by a Focus like any app.
    Active,
    /// Delivered through a Focus.
    TimeSensitive,
}

impl Notification {
    pub fn interruption(&self) -> Interruption {
        if self.time_sensitive {
            Interruption::TimeSensitive
        } else {
            Interruption::Active
        }
    }
}

/// The file of a bundled sound, looked up by name in the bundle's `Resources` folder.
pub fn sound_file(id: &str) -> String {
    format!("{id}.aiff")
}

/// The system's notification center, or a fake.
pub trait Poster {
    /// Shows the banner; `open_json` travels in `userInfo` under [`OPEN_KEY`].
    fn post(&self, n: &Notification, open_json: &str);
    /// Asks macOS for permission (it prompts only the first time).
    fn request_authorization(&self);
    /// The last permission status heard from the system.
    fn status(&self) -> PermissionStatus;
    /// Reads the current permission from the system; the answer arrives through the poster's
    /// status callback.
    fn refresh_status(&self);
}

/// The click target of a notification, from its `userInfo` JSON.
pub fn parse_open(json: &str) -> Option<OpenTarget> {
    serde_json::from_str(json).ok()
}

pub struct Notifier<P: Poster> {
    poster: P,
    asked: bool,
    reported: Option<PermissionStatus>,
    posted: VecDeque<String>,
}

impl<P: Poster> Notifier<P> {
    pub fn new(poster: P) -> Self {
        Self {
            poster,
            asked: false,
            reported: None,
            posted: VecDeque::new(),
        }
    }

    /// Posts the banner unless this id was already posted. Returns whether it was posted.
    pub fn notify(&mut self, n: &Notification) -> bool {
        if self.posted.iter().any(|id| *id == n.id) {
            return false;
        }
        let Ok(json) = serde_json::to_string(&n.open) else {
            return false;
        };
        if self.posted.len() == REMEMBERED {
            self.posted.pop_front();
        }
        self.posted.push_back(n.id.clone());
        self.poster.post(n, &json);
        true
    }

    /// Asks for permission once, however often it is called.
    pub fn authorize(&mut self) {
        if !std::mem::replace(&mut self.asked, true) {
            self.poster.request_authorization();
        }
    }

    /// `Some(status)` when it differs from what was last reported to the daemon.
    pub fn status_changed(&mut self, status: PermissionStatus) -> Option<PermissionStatus> {
        (self.reported.replace(status) != Some(status)).then_some(status)
    }

    pub fn refresh_status(&self) {
        self.poster.refresh_status();
    }

    pub fn status(&self) -> PermissionStatus {
        self.poster.status()
    }
}

/// Records what it is asked; used by tests and by `--render` style runs without a bundle.
#[derive(Default)]
pub struct FakePoster {
    pub posted: std::cell::RefCell<Vec<(Notification, String)>>,
    pub authorizations: std::cell::Cell<u32>,
    pub refreshes: std::cell::Cell<u32>,
    pub current: std::cell::Cell<Option<PermissionStatus>>,
}

impl Poster for FakePoster {
    fn post(&self, n: &Notification, open_json: &str) {
        self.posted
            .borrow_mut()
            .push((n.clone(), open_json.to_string()));
    }

    fn request_authorization(&self) {
        self.authorizations.set(self.authorizations.get() + 1);
    }

    fn status(&self) -> PermissionStatus {
        self.current.get().unwrap_or_default()
    }

    fn refresh_status(&self) {
        self.refreshes.set(self.refreshes.get() + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::PrRef;

    fn pr() -> PrRef {
        PrRef {
            owner: "rzorzal".into(),
            repo: "clusia".into(),
            number: 7,
        }
    }

    fn note(id: &str) -> Notification {
        Notification {
            id: id.into(),
            title: "Review requested".into(),
            subtitle: "rzorzal/clusia #7".into(),
            body: "@octo asked for your review".into(),
            sound: Some("leaf".into()),
            open: OpenTarget::Review {
                pr: pr(),
                thread: None,
            },
            time_sensitive: false,
        }
    }

    #[test]
    fn a_notification_is_posted_once_per_id() {
        let mut n = Notifier::new(FakePoster::default());
        assert!(n.notify(&note("a")));
        assert!(!n.notify(&note("a")), "a resend of the same id is dropped");
        assert!(n.notify(&note("b")));
        assert_eq!(n.poster.posted.borrow().len(), 2);
    }

    #[test]
    fn old_ids_are_forgotten_after_the_window() {
        let mut n = Notifier::new(FakePoster::default());
        for i in 0..=REMEMBERED {
            assert!(n.notify(&note(&format!("n{i}"))));
        }
        assert!(n.notify(&note("n0")), "n0 fell out of the window");
        assert!(!n.notify(&note(&format!("n{REMEMBERED}"))));
    }

    #[test]
    fn the_click_target_travels_as_json_and_comes_back() {
        let mut n = Notifier::new(FakePoster::default());
        n.notify(&note("a"));
        let (_, json) = n.poster.posted.borrow()[0].clone();
        assert_eq!(
            parse_open(&json),
            Some(OpenTarget::Review {
                pr: pr(),
                thread: None
            })
        );
        assert_eq!(parse_open("not json"), None);
        assert_eq!(parse_open(r#"{"nope":1}"#), None);
    }

    #[test]
    fn not_following_focus_is_time_sensitive() {
        let mut n = note("a");
        assert_eq!(n.interruption(), Interruption::Active);
        n.time_sensitive = true;
        assert_eq!(n.interruption(), Interruption::TimeSensitive);
    }

    #[test]
    fn sounds_are_found_at_the_top_of_resources() {
        assert_eq!(sound_file("leaf"), "leaf.aiff");
        assert_eq!(sound_file("tick"), "tick.aiff");
        assert!(!sound_file("drop").contains('/'));
    }

    #[test]
    fn authorization_is_requested_once() {
        let mut n = Notifier::new(FakePoster::default());
        n.authorize();
        n.authorize();
        assert_eq!(n.poster.authorizations.get(), 1);
        assert_eq!(AUTH_DELAY, Duration::from_secs(15));
    }

    #[test]
    fn a_permission_status_is_reported_only_when_it_changes() {
        let mut n = Notifier::new(FakePoster::default());
        assert_eq!(
            n.status_changed(PermissionStatus::NotDetermined),
            Some(PermissionStatus::NotDetermined),
            "the first status always goes to the daemon"
        );
        assert_eq!(n.status_changed(PermissionStatus::NotDetermined), None);
        assert_eq!(
            n.status_changed(PermissionStatus::Allowed),
            Some(PermissionStatus::Allowed)
        );
        assert_eq!(n.status_changed(PermissionStatus::Allowed), None);
        assert_eq!(
            n.status_changed(PermissionStatus::Denied),
            Some(PermissionStatus::Denied)
        );
    }
}
