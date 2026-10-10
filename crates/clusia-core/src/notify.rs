//! What a notification is about, whether it reaches you, and where a click on it goes.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::PrRef;
use crate::config::{Dnd, EventKind, Notifications, SoundId, Weekday};

/// Events for one pull request closer together than this share one macOS notification.
pub const GROUP_WINDOW_SECS: i64 = 120;

/// Where the window goes when a notification or an inbox row is opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenTarget {
    /// The review, optionally at one thread.
    Review {
        pr: PrRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<String>,
    },
    /// Home, optionally with one pull request in view.
    Home {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pr: Option<PrRef>,
    },
    /// A Config page, by its name (`git`, `notifications`).
    Config { page: String },
}

/// Events that group together: the ones about one pull request.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GroupKey(pub PrRef);

/// A wall-clock moment in the user's time zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub weekday: Weekday,
    /// Minutes since local midnight, 0..1440.
    pub minutes: u16,
}

impl LocalTime {
    pub fn new(weekday: Weekday, hour: u8, minute: u8) -> Self {
        Self {
            weekday,
            minutes: u16::from(hour) * 60 + u16::from(minute),
        }
    }

    /// The local time of a Unix timestamp in a zone `utc_offset_secs` east of UTC.
    pub fn from_unix_with_offset(ts: i64, utc_offset_secs: i64) -> Self {
        let local = ts + utc_offset_secs;
        let days = local.div_euclid(86_400);
        let secs = local.rem_euclid(86_400);
        // 1970-01-01 was a Thursday.
        let weekday = Weekday::ALL[(days + 3).rem_euclid(7) as usize];
        Self {
            weekday,
            minutes: (secs / 60) as u16,
        }
    }

    /// The local time of a Unix timestamp in the machine's time zone.
    pub fn from_unix(ts: i64) -> Self {
        Self::from_unix_with_offset(ts, local_utc_offset(ts))
    }
}

/// Seconds east of UTC the machine's time zone is at `ts`; 0 when the zone cannot be read.
fn local_utc_offset(ts: i64) -> i64 {
    let t: libc::time_t = ts;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: `t` and `tm` are valid for the call; `localtime_r` fills `tm` and returns null
    // on failure, in which case `tm` is not read.
    let filled = unsafe { libc::localtime_r(&t, tm.as_mut_ptr()) };
    if filled.is_null() {
        return 0;
    }
    // SAFETY: a non-null result means `localtime_r` initialised `tm`.
    unsafe { tm.assume_init() }.tm_gmtoff
}

/// Whether `now` falls inside the quiet hours. The range is half open (`from` is quiet, `to`
/// is not). When `from` is not earlier than `to` it runs overnight and belongs to the day it
/// starts on, so Friday 19:00 to 09:00 also covers Saturday morning.
pub fn in_dnd(dnd: &Dnd, now: LocalTime) -> bool {
    if !dnd.enabled {
        return false;
    }
    let (from, to) = (dnd.from.minutes(), dnd.to.minutes());
    let today = dnd.days.contains(&now.weekday);
    if from < to {
        return today && (from..to).contains(&now.minutes);
    }
    (today && now.minutes >= from)
        || (dnd.days.contains(&now.weekday.previous()) && now.minutes < to)
}

/// When the macOS notifications for each group were last posted.
#[derive(Debug, Clone, Default)]
pub struct Recent {
    posted: HashMap<GroupKey, i64>,
}

impl Recent {
    /// Notes that a macOS notification for `group` was posted at `at`.
    pub fn record(&mut self, group: &GroupKey, at: i64) {
        let slot = self.posted.entry(group.clone()).or_insert(at);
        *slot = (*slot).max(at);
    }

    pub fn last(&self, group: &GroupKey) -> Option<i64> {
        self.posted.get(group).copied()
    }

    /// Forgets groups whose window is over at `now`.
    pub fn prune(&mut self, now: i64) {
        self.posted
            .retain(|_, at| now.saturating_sub(*at) < GROUP_WINDOW_SECS);
    }
}

/// Something worth telling the user about, already worded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyEvent {
    pub kind: EventKind,
    pub pr: Option<PrRef>,
    /// Identifies the occurrence so it is told only once; opaque here.
    pub key: String,
    pub title: String,
    pub body: String,
    pub open: OpenTarget,
    /// Unix seconds.
    pub at: i64,
}

/// Where an event goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// Add it to the tray inbox.
    pub tray: bool,
    /// Post a macOS notification.
    pub macos: bool,
    /// The sound that notification plays; `None` is silent.
    pub sound: Option<SoundId>,
    /// The group to `Recent::record` once the notification is posted.
    pub group: Option<GroupKey>,
}

/// Routes `event` by the user's settings. Quiet hours and grouping only ever silence the macOS
/// notification and its sound; the tray inbox always gets what its route says.
pub fn decide(
    event: &NotifyEvent,
    cfg: &Notifications,
    now: LocalTime,
    recent: &Recent,
) -> Decision {
    let route = cfg.route(event.kind);
    let group = event.pr.clone().filter(|_| cfg.group_bursts).map(GroupKey);
    // A permission request expires: it is told even in quiet hours and inside a burst.
    let urgent = event.is_urgent();
    let in_burst = !urgent
        && group
            .as_ref()
            .and_then(|g| recent.last(g))
            .is_some_and(|last| event.at.saturating_sub(last) < GROUP_WINDOW_SECS);
    let macos = route.macos && (urgent || !in_dnd(&cfg.dnd, now)) && !in_burst;
    Decision {
        tray: route.tray,
        macos,
        sound: (macos && route.sound).then_some(cfg.sound),
        group,
    }
}

/// The line under a notification's title: the repository and number, or nothing.
pub fn subtitle(event: &NotifyEvent) -> String {
    event
        .pr
        .as_ref()
        .map(|pr| format!("{}/{} #{}", pr.owner, pr.repo, pr.number))
        .unwrap_or_default()
}

/// What went wrong with syncing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncProblem {
    /// The token was refused.
    Unauthorized,
    /// GitHub has been unreachable for a while.
    Offline,
    RateLimited,
}

fn plural(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

impl NotifyEvent {
    /// Whether the event may not wait: it bypasses quiet hours and is time-sensitive on
    /// macOS, because what it asks for expires.
    pub fn is_urgent(&self) -> bool {
        self.kind == EventKind::AgentPermission
    }

    fn about(
        kind: EventKind,
        pr: &PrRef,
        key: impl Into<String>,
        at: i64,
        title: String,
        body: String,
        open: OpenTarget,
    ) -> Self {
        Self {
            kind,
            pr: Some(pr.clone()),
            key: key.into(),
            title,
            body,
            open,
            at,
        }
    }

    fn review_of(pr: &PrRef, thread: Option<String>) -> OpenTarget {
        OpenTarget::Review {
            pr: pr.clone(),
            thread,
        }
    }

    pub fn review_requested(
        pr: &PrRef,
        pr_title: &str,
        by: &str,
        key: impl Into<String>,
        at: i64,
    ) -> Self {
        Self::about(
            EventKind::ReviewRequested,
            pr,
            key,
            at,
            "Review requested".into(),
            format!(
                "@{by} asked you to review {}/{} #{} · {pr_title}",
                pr.owner, pr.repo, pr.number
            ),
            Self::review_of(pr, None),
        )
    }

    pub fn commits_after_review(
        pr: &PrRef,
        pr_title: &str,
        commits: u32,
        key: impl Into<String>,
        at: i64,
    ) -> Self {
        Self::about(
            EventKind::CommitsAfterReview,
            pr,
            key,
            at,
            format!("#{} is out of date", pr.number),
            format!(
                "{} on {pr_title}.",
                plural(commits, "new commit", "new commits")
            ),
            Self::review_of(pr, None),
        )
    }

    pub fn reply_to_you(
        pr: &PrRef,
        pr_title: &str,
        by: &str,
        thread: Option<String>,
        key: impl Into<String>,
        at: i64,
    ) -> Self {
        Self::about(
            EventKind::ReplyToYou,
            pr,
            key,
            at,
            format!("@{by} replied to you"),
            format!("On #{} · {pr_title}", pr.number),
            Self::review_of(pr, thread),
        )
    }

    pub fn mentioned(
        pr: &PrRef,
        pr_title: &str,
        by: &str,
        key: impl Into<String>,
        at: i64,
    ) -> Self {
        Self::about(
            EventKind::Mentioned,
            pr,
            key,
            at,
            format!("@{by} mentioned you"),
            format!("On #{} · {pr_title}", pr.number),
            Self::review_of(pr, None),
        )
    }

    pub fn checks_failed(
        pr: &PrRef,
        pr_title: &str,
        failed: u32,
        key: impl Into<String>,
        at: i64,
    ) -> Self {
        Self::about(
            EventKind::ChecksFailed,
            pr,
            key,
            at,
            format!("Checks failed on #{}", pr.number),
            format!(
                "{} on {pr_title}.",
                plural(failed, "check is failing", "checks are failing")
            ),
            OpenTarget::Home {
                pr: Some(pr.clone()),
            },
        )
    }

    pub fn agent_finished(pr: &PrRef, pr_title: &str, key: impl Into<String>, at: i64) -> Self {
        Self::about(
            EventKind::AgentFinished,
            pr,
            key,
            at,
            format!("Claude Code finished on #{}", pr.number),
            format!("{pr_title}. Its answer is ready to read."),
            Self::review_of(pr, None),
        )
    }

    /// The agent waits for the reviewer's decision on `summary` (the command or the file),
    /// which `verb` already says ("run", "edit", "write").
    pub fn agent_permission(
        pr: &PrRef,
        pr_title: &str,
        verb: &str,
        summary: &str,
        key: impl Into<String>,
        at: i64,
    ) -> Self {
        Self::about(
            EventKind::AgentPermission,
            pr,
            key,
            at,
            format!("Claude Code needs your permission on #{}", pr.number),
            format!("It wants to {verb}: {summary} · {pr_title}"),
            Self::review_of(pr, None),
        )
    }

    pub fn sync_problem(problem: SyncProblem, key: impl Into<String>, at: i64) -> Self {
        let (title, body) = match problem {
            SyncProblem::Unauthorized => (
                "GitHub refused the token",
                "Open Git server to sign in again.",
            ),
            SyncProblem::Offline => (
                "Clúsia is offline",
                "GitHub has been out of reach for more than 10 minutes.",
            ),
            SyncProblem::RateLimited => (
                "GitHub is limiting Clúsia",
                "Syncing resumes by itself when the limit resets.",
            ),
        };
        Self {
            kind: EventKind::SyncProblem,
            pr: None,
            key: key.into(),
            title: title.into(),
            body: body.into(),
            open: OpenTarget::Config { page: "git".into() },
            at,
        }
    }

    /// `what` names the repaired file, like `config.toml`.
    pub fn state_recovered(what: &str, key: impl Into<String>, at: i64) -> Self {
        Self {
            kind: EventKind::StateRecovered,
            pr: None,
            key: key.into(),
            title: format!("{what} was repaired"),
            body: "The unreadable copy was kept next to it; Clúsia started from a clean one."
                .into(),
            open: OpenTarget::Home { pr: None },
            at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HourMinute;
    use crate::time::days_from_civil;

    fn pr(n: u64) -> PrRef {
        PrRef::new("acme", "widgets", n).unwrap()
    }

    fn at(y: i64, mo: u32, d: u32, h: i64, mi: i64) -> i64 {
        days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60
    }

    fn time(weekday: Weekday, h: u8, m: u8) -> LocalTime {
        LocalTime::new(weekday, h, m)
    }

    fn event(kind: EventKind, pr_number: Option<u64>, at: i64) -> NotifyEvent {
        NotifyEvent {
            kind,
            pr: pr_number.map(pr),
            key: format!("{kind}:{at}"),
            title: "t".into(),
            body: "b".into(),
            open: OpenTarget::Home { pr: None },
            at,
        }
    }

    fn workdays(from: (u8, u8), to: (u8, u8)) -> Dnd {
        Dnd {
            enabled: true,
            from: HourMinute::new(from.0, from.1).unwrap(),
            to: HourMinute::new(to.0, to.1).unwrap(),
            days: [
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri,
            ]
            .into(),
        }
    }

    const NOON: LocalTime = LocalTime {
        weekday: Weekday::Wed,
        minutes: 12 * 60,
    };

    #[test]
    fn an_agent_finishing_opens_its_review() {
        let e = NotifyEvent::agent_finished(&pr(7), "Add feature", "k", 5);
        assert_eq!(e.kind, EventKind::AgentFinished);
        assert_eq!(e.pr, Some(pr(7)));
        assert_eq!(e.title, "Claude Code finished on #7");
        assert_eq!(e.body, "Add feature. Its answer is ready to read.");
        assert_eq!(
            e.open,
            OpenTarget::Review {
                pr: pr(7),
                thread: None
            }
        );
        let d = decide(&e, &Notifications::default(), NOON, &Recent::default());
        assert!(d.tray && d.macos && d.sound.is_none());
    }

    #[test]
    fn a_permission_request_bypasses_quiet_hours_and_bursts() {
        let e = NotifyEvent::agent_permission(
            &pr(7),
            "Add feature",
            "run",
            "cargo test -p clusia-core",
            "k",
            100,
        );
        assert_eq!(e.kind, EventKind::AgentPermission);
        assert_eq!(e.title, "Claude Code needs your permission on #7");
        assert_eq!(
            e.body,
            "It wants to run: cargo test -p clusia-core · Add feature"
        );
        assert_eq!(
            e.open,
            OpenTarget::Review {
                pr: pr(7),
                thread: None
            }
        );
        assert!(e.is_urgent());
        let cfg = Notifications {
            dnd: Dnd {
                enabled: true,
                from: HourMinute::new(0, 0).unwrap(),
                to: HourMinute::new(23, 59).unwrap(),
                days: Weekday::ALL.into(),
            },
            ..Notifications::default()
        };
        let mut recent = Recent::default();
        recent.record(&GroupKey(pr(7)), 90);
        let d = decide(&e, &cfg, NOON, &recent);
        assert!(d.tray && d.macos && d.sound.is_some(), "{d:?}");
        let finished = NotifyEvent::agent_finished(&pr(7), "Add feature", "k2", 100);
        assert!(!finished.is_urgent());
        assert!(
            !decide(&finished, &cfg, NOON, &recent).macos,
            "everything else still keeps quiet"
        );
    }

    #[test]
    fn local_time_reads_weekday_and_minutes() {
        // 2026-10-07 is a Wednesday.
        let ts = at(2026, 10, 7, 9, 30);
        assert_eq!(
            LocalTime::from_unix_with_offset(ts, 0),
            time(Weekday::Wed, 9, 30)
        );
        assert_eq!(
            LocalTime::from_unix_with_offset(0, 0),
            time(Weekday::Thu, 0, 0)
        );
        // Monday 23:30 UTC is already Tuesday 00:30 an hour east, and Monday 20:30 three west.
        let monday = at(2024, 1, 1, 23, 30);
        assert_eq!(
            LocalTime::from_unix_with_offset(monday, 3600),
            time(Weekday::Tue, 0, 30)
        );
        assert_eq!(
            LocalTime::from_unix_with_offset(monday, -3 * 3600),
            time(Weekday::Mon, 20, 30)
        );
        // Before 1970 still lands on the right day: 1969-12-31 was a Wednesday.
        assert_eq!(
            LocalTime::from_unix_with_offset(-60, 0),
            time(Weekday::Wed, 23, 59)
        );
    }

    #[test]
    fn the_machine_zone_is_a_whole_quarter_hour_from_utc() {
        let ts = at(2026, 10, 7, 9, 30);
        let utc = LocalTime::from_unix_with_offset(ts, 0);
        let local = LocalTime::from_unix(ts);
        assert!(local.minutes < 24 * 60);
        let shift = (i32::from(local.minutes) - i32::from(utc.minutes)).rem_euclid(15);
        assert_eq!(shift, 0, "zones move in quarter hours");
    }

    #[test]
    fn dnd_same_day_range_is_half_open() {
        let dnd = workdays((9, 0), (17, 0));
        assert!(!in_dnd(&dnd, time(Weekday::Wed, 8, 59)));
        assert!(in_dnd(&dnd, time(Weekday::Wed, 9, 0)), "from is quiet");
        assert!(in_dnd(&dnd, time(Weekday::Wed, 16, 59)));
        assert!(!in_dnd(&dnd, time(Weekday::Wed, 17, 0)), "to is not");
        assert!(!in_dnd(&dnd, time(Weekday::Sat, 12, 0)), "not a chosen day");
    }

    #[test]
    fn dnd_overnight_and_weekdays() {
        let dnd = workdays((19, 0), (9, 0));
        // Evening of a chosen day, and the next morning.
        assert!(!in_dnd(&dnd, time(Weekday::Wed, 18, 59)));
        assert!(in_dnd(&dnd, time(Weekday::Wed, 19, 0)));
        assert!(in_dnd(&dnd, time(Weekday::Wed, 23, 59)));
        assert!(
            in_dnd(&dnd, time(Weekday::Thu, 0, 0)),
            "midnight keeps the night going"
        );
        assert!(in_dnd(&dnd, time(Weekday::Thu, 8, 59)));
        assert!(!in_dnd(&dnd, time(Weekday::Thu, 9, 0)));
        // The week ends on Friday night and runs into Saturday morning only.
        assert!(in_dnd(&dnd, time(Weekday::Fri, 22, 0)));
        assert!(in_dnd(&dnd, time(Weekday::Sat, 8, 59)), "Friday's night");
        assert!(
            !in_dnd(&dnd, time(Weekday::Sat, 19, 0)),
            "Saturday is not chosen"
        );
        assert!(!in_dnd(&dnd, time(Weekday::Sun, 3, 0)));
        // Monday morning is quiet only if Sunday were chosen, and it is not.
        assert!(!in_dnd(&dnd, time(Weekday::Mon, 8, 0)));
        assert!(in_dnd(&dnd, time(Weekday::Mon, 19, 0)));
    }

    #[test]
    fn dnd_off_empty_days_and_equal_times() {
        let mut dnd = workdays((19, 0), (9, 0));
        dnd.enabled = false;
        assert!(!in_dnd(&dnd, time(Weekday::Wed, 23, 0)));
        dnd.enabled = true;
        dnd.days.clear();
        assert!(!in_dnd(&dnd, time(Weekday::Wed, 23, 0)));
        let all_day = workdays((8, 0), (8, 0));
        assert!(in_dnd(&all_day, time(Weekday::Wed, 8, 0)));
        assert!(
            in_dnd(&all_day, time(Weekday::Wed, 3, 0)),
            "yesterday's day-long range"
        );
        assert!(in_dnd(&all_day, time(Weekday::Wed, 7, 59)));
        assert!(
            in_dnd(&all_day, time(Weekday::Sat, 7, 59)),
            "Friday's range runs to Saturday 08:00"
        );
        assert!(!in_dnd(&all_day, time(Weekday::Sat, 8, 0)));
        assert!(!in_dnd(&all_day, time(Weekday::Sun, 12, 0)));
    }

    #[test]
    fn default_routes_decide_per_kind() {
        let cfg = Notifications::default();
        let recent = Recent::default();
        let go = |kind| decide(&event(kind, Some(7), 1000), &cfg, NOON, &recent);
        assert_eq!(
            go(EventKind::ReviewRequested),
            Decision {
                tray: true,
                macos: true,
                sound: Some(SoundId::Leaf),
                group: Some(GroupKey(pr(7)))
            }
        );
        assert_eq!(go(EventKind::Mentioned).sound, Some(SoundId::Leaf));
        for quiet in [
            EventKind::CommitsAfterReview,
            EventKind::ReplyToYou,
            EventKind::SyncProblem,
        ] {
            let d = go(quiet);
            assert!(d.tray && d.macos && d.sound.is_none(), "{quiet}");
        }
        let checks = go(EventKind::ChecksFailed);
        assert!(checks.tray && !checks.macos && checks.sound.is_none());
    }

    #[test]
    fn the_chosen_sound_and_routes_are_honoured() {
        let mut cfg = Notifications {
            sound: SoundId::Tick,
            ..Notifications::default()
        };
        let recent = Recent::default();
        let e = event(EventKind::Mentioned, Some(7), 1000);
        assert_eq!(decide(&e, &cfg, NOON, &recent).sound, Some(SoundId::Tick));
        cfg.events.get_mut(&EventKind::Mentioned).unwrap().macos = false;
        let d = decide(&e, &cfg, NOON, &recent);
        assert!(
            d.tray && !d.macos && d.sound.is_none(),
            "no notification, no sound"
        );
        cfg.events.get_mut(&EventKind::Mentioned).unwrap().tray = false;
        assert!(!decide(&e, &cfg, NOON, &recent).tray);
    }

    #[test]
    fn quiet_hours_silence_macos_and_sound_but_not_the_tray() {
        let cfg = Notifications {
            dnd: workdays((19, 0), (9, 0)),
            ..Notifications::default()
        };
        let e = event(EventKind::ReviewRequested, Some(7), 1000);
        let night = time(Weekday::Wed, 22, 0);
        let d = decide(&e, &cfg, night, &Recent::default());
        assert!(d.tray);
        assert!(!d.macos);
        assert_eq!(d.sound, None);
        assert!(decide(&e, &cfg, NOON, &Recent::default()).macos);
    }

    #[test]
    fn a_burst_for_one_pr_posts_once_per_window() {
        let cfg = Notifications::default();
        let mut recent = Recent::default();
        let mut posted = 0;
        let mut in_tray = 0;
        for i in 0..20 {
            let e = event(EventKind::ReplyToYou, Some(7), 1000 + i * 5);
            let d = decide(&e, &cfg, NOON, &recent);
            in_tray += usize::from(d.tray);
            if d.macos {
                posted += 1;
                recent.record(d.group.as_ref().unwrap(), e.at);
            }
        }
        assert_eq!(posted, 1);
        assert_eq!(in_tray, 20, "the tray keeps every one");
    }

    #[test]
    fn the_group_window_is_120_seconds() {
        let cfg = Notifications::default();
        let mut recent = Recent::default();
        recent.record(&GroupKey(pr(7)), 1000);
        let at = |t| {
            decide(
                &event(EventKind::Mentioned, Some(7), t),
                &cfg,
                NOON,
                &recent,
            )
        };
        assert!(!at(1000).macos);
        assert!(!at(1119).macos);
        assert!(at(1120).macos, "120 s later is a new notification");
        assert!(!at(900).macos, "an older event is part of the same burst");
        let other = decide(
            &event(EventKind::Mentioned, Some(8), 1001),
            &cfg,
            NOON,
            &recent,
        );
        assert!(other.macos, "another PR is its own group");
        let none = decide(
            &event(EventKind::SyncProblem, None, 1001),
            &cfg,
            NOON,
            &recent,
        );
        assert!(none.macos && none.group.is_none(), "no PR, no group");
    }

    #[test]
    fn grouping_can_be_turned_off() {
        let cfg = Notifications {
            group_bursts: false,
            ..Notifications::default()
        };
        let mut recent = Recent::default();
        recent.record(&GroupKey(pr(7)), 1000);
        let d = decide(
            &event(EventKind::Mentioned, Some(7), 1001),
            &cfg,
            NOON,
            &recent,
        );
        assert!(d.macos);
        assert_eq!(d.group, None);
    }

    #[test]
    fn recent_remembers_the_latest_and_forgets_old_groups() {
        let mut recent = Recent::default();
        let g = GroupKey(pr(7));
        assert_eq!(recent.last(&g), None);
        recent.record(&g, 1000);
        recent.record(&g, 900);
        assert_eq!(recent.last(&g), Some(1000));
        recent.record(&g, 1050);
        assert_eq!(recent.last(&g), Some(1050));
        recent.record(&GroupKey(pr(8)), 1200);
        recent.prune(1169);
        assert_eq!(recent.last(&g), Some(1050), "still inside its window");
        recent.prune(1170);
        assert_eq!(recent.last(&g), None);
        assert_eq!(recent.last(&GroupKey(pr(8))), Some(1200));
    }

    #[test]
    fn every_event_has_its_words_and_target() {
        let p = pr(123);
        let e = NotifyEvent::review_requested(&p, "feat: auth refresh", "octo", "k1", 5);
        assert_eq!(e.kind, EventKind::ReviewRequested);
        assert_eq!(e.title, "Review requested");
        assert_eq!(
            e.body,
            "@octo asked you to review acme/widgets #123 · feat: auth refresh"
        );
        assert_eq!(
            e.open,
            OpenTarget::Review {
                pr: p.clone(),
                thread: None
            }
        );
        assert_eq!((e.key.as_str(), e.at, e.pr.as_ref()), ("k1", 5, Some(&p)));
        assert_eq!(subtitle(&e), "acme/widgets #123");

        let e = NotifyEvent::commits_after_review(&p, "fix: cache invalidation", 2, "k", 0);
        assert_eq!(e.title, "#123 is out of date");
        assert_eq!(e.body, "2 new commits on fix: cache invalidation.");
        assert_eq!(
            NotifyEvent::commits_after_review(&p, "x", 1, "k", 0).body,
            "1 new commit on x."
        );

        let e = NotifyEvent::reply_to_you(&p, "t", "mona", Some("PRRT_1".into()), "k", 0);
        assert_eq!(e.title, "@mona replied to you");
        assert_eq!(e.body, "On #123 · t");
        assert_eq!(
            e.open,
            OpenTarget::Review {
                pr: p.clone(),
                thread: Some("PRRT_1".into())
            }
        );

        let e = NotifyEvent::mentioned(&p, "t", "hubot", "k", 0);
        assert_eq!(e.title, "@hubot mentioned you");
        assert_eq!(
            e.open,
            OpenTarget::Review {
                pr: p.clone(),
                thread: None
            }
        );

        let e = NotifyEvent::checks_failed(&p, "t", 3, "k", 0);
        assert_eq!(e.title, "Checks failed on #123");
        assert_eq!(e.body, "3 checks are failing on t.");
        assert_eq!(
            NotifyEvent::checks_failed(&p, "t", 1, "k", 0).body,
            "1 check is failing on t."
        );
        assert_eq!(e.open, OpenTarget::Home { pr: Some(p) });
    }

    #[test]
    fn sync_and_recovery_events_have_no_pull_request() {
        for (problem, title) in [
            (SyncProblem::Unauthorized, "GitHub refused the token"),
            (SyncProblem::Offline, "Clúsia is offline"),
            (SyncProblem::RateLimited, "GitHub is limiting Clúsia"),
        ] {
            let e = NotifyEvent::sync_problem(problem, "k", 9);
            assert_eq!(e.kind, EventKind::SyncProblem);
            assert_eq!(e.title, title);
            assert_eq!(e.pr, None);
            assert_eq!(e.open, OpenTarget::Config { page: "git".into() });
            assert_eq!(subtitle(&e), "");
        }
        let e = NotifyEvent::state_recovered("config.toml", "k", 9);
        assert_eq!(e.kind, EventKind::StateRecovered);
        assert_eq!(e.title, "config.toml was repaired");
        assert_eq!(e.open, OpenTarget::Home { pr: None });
    }
}
