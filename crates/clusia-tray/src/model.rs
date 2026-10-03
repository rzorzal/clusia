//! The tray's view model: everything the popover shows, computed without AppKit (spec §7.3).

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use clusia_core::time::parse_rfc3339;
use clusia_core::{ActivitySummary, PrSummary, ReviewState};
use clusia_protocol::{ReviewSummary, SyncState, SyncStatus};

use crate::actions::Action;
use crate::heatmap;

/// Rows per list; the rest collapse into "+N more".
pub const MAX_ROWS: usize = 4;

/// What the daemon last told the tray (built by `data`).
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub assigned: Vec<PrSummary>,
    pub mine: Vec<PrSummary>,
    pub reviews: Vec<ReviewSummary>,
    pub activity: Option<ActivitySummary>,
    pub sync: Option<SyncStatus>,
    /// `github.host` from the config, for browser links.
    pub host: String,
    /// `assigned` and `mine` hold real lists (not the empty defaults of an early snapshot).
    pub lists_loaded: bool,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            assigned: Vec::new(),
            mine: Vec::new(),
            reviews: Vec::new(),
            activity: None,
            sync: None,
            host: "github.com".into(),
            lists_loaded: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counter {
    pub value: usize,
    pub label: &'static str,
    pub action: Option<Action>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusLine {
    pub text: String,
    pub tone: Tone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge {
    pub text: &'static str,
    pub tone: Tone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub number: String,
    pub title: String,
    pub meta: String,
    /// New activity since the user last opened the popover.
    pub fresh: bool,
    pub badge: Option<Badge>,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub title: &'static str,
    pub rows: Vec<Row>,
    /// Rows left out by [`MAX_ROWS`].
    pub more: usize,
    /// Shown when `rows` is empty.
    pub empty: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayView {
    pub counters: Vec<Counter>,
    /// Heatmap levels 0–4, oldest day first (empty before activity arrives).
    pub heat: Vec<u8>,
    pub week_label: String,
    pub status: Option<StatusLine>,
    pub sections: Vec<Section>,
    /// The window is installed, so ⤢ and "+N more" can open Home.
    pub show_home: bool,
    /// Drives the dot on the menu bar icon.
    pub has_news: bool,
}

#[derive(Debug, Default)]
pub struct TrayModel {
    snapshot: Snapshot,
    /// `updated_at` per PR at the last loaded lists; `None` until the first sync finished.
    known: Option<HashMap<String, String>>,
    /// PRs that changed since the user last opened the popover.
    fresh: HashSet<String>,
    app_available: bool,
}

impl TrayModel {
    pub fn new(app_available: bool) -> Self {
        Self {
            app_available,
            ..Self::default()
        }
    }

    pub fn apply(&mut self, snapshot: Snapshot) {
        if snapshot.lists_loaded {
            let current: HashMap<String, String> = snapshot
                .assigned
                .iter()
                .chain(&snapshot.mine)
                .map(|p| (p.pr.file_key(), p.updated_at.clone()))
                .collect();
            if let Some(known) = &self.known {
                for (key, at) in &current {
                    if known.get(key) != Some(at) {
                        self.fresh.insert(key.clone());
                    }
                }
            }
            self.fresh.retain(|k| current.contains_key(k));
            let synced = snapshot
                .sync
                .as_ref()
                .is_some_and(|s| s.last_sync_unix.is_some());
            if synced {
                self.known = Some(current);
            }
        }
        self.snapshot = snapshot;
    }

    pub fn has_news(&self) -> bool {
        !self.fresh.is_empty()
    }

    /// The user looked at the popover: clear the dots.
    pub fn mark_seen(&mut self) {
        self.fresh.clear();
    }

    pub fn view(&self, now: i64) -> TrayView {
        let s = &self.snapshot;
        let web = format!("https://{}", s.host);
        let mut assigned: Vec<&PrSummary> = s.assigned.iter().collect();
        assigned.sort_by_key(|p| Reverse(parse_rfc3339(&p.updated_at).unwrap_or(0)));
        let mut saved: Vec<&ReviewSummary> = s
            .reviews
            .iter()
            .filter(|r| {
                matches!(
                    r.state,
                    ReviewState::Saved | ReviewState::Outdated | ReviewState::Revalidated
                )
            })
            .collect();
        saved.sort_by_key(|r| Reverse(r.updated_at));
        TrayView {
            counters: vec![
                Counter {
                    value: s.assigned.len(),
                    label: "Assigned",
                    action: Some(Action::OpenUrl(format!("{web}/pulls/review-requested"))),
                },
                Counter {
                    value: s.mine.len(),
                    label: "Mine",
                    action: Some(Action::OpenUrl(format!("{web}/pulls"))),
                },
                Counter {
                    value: saved.len(),
                    label: "Saved",
                    action: self.app_available.then_some(Action::OpenHome),
                },
            ],
            heat: s
                .activity
                .as_ref()
                .map(|a| heatmap::levels(&a.heatmap))
                .unwrap_or_default(),
            week_label: s
                .activity
                .as_ref()
                .map(|a| week_label(a.published_this_week))
                .unwrap_or_default(),
            status: status_line(s.sync.as_ref(), now),
            sections: vec![
                Section {
                    title: "Assigned to me",
                    rows: assigned
                        .iter()
                        .take(MAX_ROWS)
                        .map(|p| self.pr_row(p, now))
                        .collect(),
                    more: assigned.len().saturating_sub(MAX_ROWS),
                    empty: "Nothing waiting for your review",
                },
                Section {
                    title: "Saved reviews",
                    rows: saved
                        .iter()
                        .take(MAX_ROWS)
                        .map(|r| review_row(r, &web, now))
                        .collect(),
                    more: saved.len().saturating_sub(MAX_ROWS),
                    empty: "No saved reviews",
                },
            ],
            show_home: self.app_available,
            has_news: self.has_news(),
        }
    }

    fn pr_row(&self, p: &PrSummary, now: i64) -> Row {
        let age = parse_rfc3339(&p.updated_at)
            .map(|t| format_age(now, t))
            .unwrap_or_default();
        let mut meta = format!("{} · {age}", p.pr.repo);
        if p.draft {
            meta.push_str(" · draft");
        }
        Row {
            number: format!("#{}", p.pr.number),
            title: p.title.clone(),
            meta,
            fresh: self.fresh.contains(&p.pr.file_key()),
            badge: None,
            action: Action::OpenReview {
                pr: p.pr.clone(),
                url: p.url.clone(),
            },
        }
    }
}

fn review_row(r: &ReviewSummary, web: &str, now: i64) -> Row {
    let comments = if r.items == 1 {
        "1 comment".to_string()
    } else {
        format!("{} comments", r.items)
    };
    let badge = match r.state {
        ReviewState::Outdated => Some(Badge {
            text: "outdated",
            tone: Tone::Warning,
        }),
        ReviewState::Revalidated => Some(Badge {
            text: "revalidated",
            tone: Tone::Neutral,
        }),
        _ => None,
    };
    Row {
        number: format!("#{}", r.pr.number),
        title: r.title.clone(),
        meta: format!(
            "{} · {comments} · {}",
            r.pr.repo,
            format_age(now, r.updated_at)
        ),
        fresh: false,
        badge,
        action: Action::OpenReview {
            pr: r.pr.clone(),
            url: format!("{web}/{}/{}/pull/{}", r.pr.owner, r.pr.repo, r.pr.number),
        },
    }
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
    use clusia_core::{ActivitySummary, DayCount, PrRef, PrSummary, ReviewState};
    use clusia_protocol::{ReviewSummary, SyncState, SyncStatus};

    const NOW: i64 = 1_790_000_000;

    fn rfc(unix: i64) -> String {
        let (y, m, d) = clusia_core::time::civil_from_days(unix.div_euclid(86_400));
        let s = unix.rem_euclid(86_400);
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
            s / 3600,
            s / 60 % 60,
            s % 60
        )
    }

    fn pr(n: u64, updated: i64) -> PrSummary {
        PrSummary {
            pr: PrRef {
                owner: "rzorzal".into(),
                repo: "clusia".into(),
                number: n,
            },
            title: format!("change {n}"),
            author: "octo".into(),
            url: format!("https://github.com/rzorzal/clusia/pull/{n}"),
            draft: false,
            updated_at: rfc(updated),
            comments: 0,
        }
    }

    fn review(n: u64, state: ReviewState, items: usize, updated: i64) -> ReviewSummary {
        ReviewSummary {
            pr: PrRef {
                owner: "rzorzal".into(),
                repo: "blog".into(),
                number: n,
            },
            title: format!("post {n}"),
            state,
            items,
            updated_at: updated,
        }
    }

    fn synced() -> Option<SyncStatus> {
        Some(SyncStatus {
            state: SyncState::Online,
            last_sync_unix: Some(NOW - 30),
            next_sync_unix: Some(NOW + 30),
            message: None,
        })
    }

    fn snap(assigned: Vec<PrSummary>) -> Snapshot {
        Snapshot {
            assigned,
            sync: synced(),
            lists_loaded: true,
            ..Snapshot::default()
        }
    }

    #[test]
    fn first_load_marks_nothing_fresh() {
        let mut m = TrayModel::new(false);
        m.apply(snap(vec![pr(1, NOW - 60)]));
        assert!(!m.has_news());
        assert!(!m.view(NOW).sections[0].rows[0].fresh);
    }

    #[test]
    fn lists_arriving_late_mark_nothing_fresh() {
        let mut m = TrayModel::new(false);
        // Early snapshot: synced, but the lists are still being fetched.
        m.apply(Snapshot {
            sync: synced(),
            ..Snapshot::default()
        });
        m.apply(snap(vec![pr(1, NOW - 60), pr(2, NOW - 90)]));
        assert!(!m.has_news());
        // Never synced yet: nothing is compared either.
        let mut m = TrayModel::new(false);
        m.apply(Snapshot {
            assigned: vec![pr(1, NOW)],
            lists_loaded: true,
            ..Snapshot::default()
        });
        m.apply(snap(vec![pr(1, NOW + 5)]));
        assert!(!m.has_news());
    }

    #[test]
    fn changed_or_new_prs_are_fresh_until_seen() {
        let mut m = TrayModel::new(false);
        m.apply(snap(vec![pr(1, NOW - 60), pr(2, NOW - 90)]));
        m.apply(snap(vec![pr(1, NOW - 10), pr(2, NOW - 90), pr(3, NOW - 5)]));
        assert!(m.has_news());
        let v = m.view(NOW);
        let fresh: Vec<_> = v.sections[0]
            .rows
            .iter()
            .filter(|r| r.fresh)
            .map(|r| r.number.clone())
            .collect();
        assert_eq!(fresh, vec!["#3", "#1"], "sorted newest first");
        assert!(v.has_news);
        m.mark_seen();
        assert!(!m.has_news());
        assert!(m.view(NOW).sections[0].rows.iter().all(|r| !r.fresh));
    }

    #[test]
    fn prs_that_leave_the_lists_drop_their_dot() {
        let mut m = TrayModel::new(false);
        m.apply(snap(vec![pr(1, NOW - 60)]));
        m.apply(snap(vec![pr(1, NOW - 1)]));
        assert!(m.has_news());
        m.apply(snap(vec![]));
        assert!(!m.has_news());
    }

    #[test]
    fn counters_sections_and_more() {
        let mut m = TrayModel::new(true);
        let assigned: Vec<_> = (1..=6).map(|n| pr(n, NOW - n as i64 * 60)).collect();
        m.apply(Snapshot {
            mine: vec![pr(40, NOW)],
            reviews: vec![
                review(7, ReviewState::Outdated, 2, NOW - 3600),
                review(8, ReviewState::Saved, 1, NOW - 7200),
                review(9, ReviewState::Active, 3, NOW),
                review(10, ReviewState::Published, 0, NOW),
            ],
            ..snap(assigned)
        });
        let v = m.view(NOW);
        let counters: Vec<_> = v.counters.iter().map(|c| (c.label, c.value)).collect();
        assert_eq!(counters, vec![("Assigned", 6), ("Mine", 1), ("Saved", 2)]);
        assert_eq!(v.sections[0].title, "Assigned to me");
        assert_eq!(v.sections[0].rows.len(), MAX_ROWS);
        assert_eq!(v.sections[0].more, 2);
        assert_eq!(v.sections[0].rows[0].meta, "clusia · 1m");
        let saved = &v.sections[1];
        assert_eq!(saved.title, "Saved reviews");
        assert_eq!(
            saved.rows.len(),
            2,
            "active and published reviews are not listed"
        );
        assert_eq!(saved.rows[0].number, "#7");
        assert_eq!(saved.rows[0].meta, "blog · 2 comments · 1h");
        assert_eq!(
            saved.rows[0].badge,
            Some(Badge {
                text: "outdated",
                tone: Tone::Warning
            })
        );
        assert_eq!(saved.rows[1].meta, "blog · 1 comment · 2h");
        assert_eq!(saved.rows[1].badge, None);
        assert!(v.show_home);
        assert_eq!(v.counters[2].action, Some(Action::OpenHome));
    }

    #[test]
    fn draft_prs_say_so() {
        let mut m = TrayModel::new(false);
        let mut p = pr(5, NOW - 3 * 86_400);
        p.draft = true;
        m.apply(snap(vec![p]));
        assert_eq!(m.view(NOW).sections[0].rows[0].meta, "clusia · 3d · draft");
    }

    #[test]
    fn enterprise_host_builds_review_links() {
        let mut m = TrayModel::new(false);
        m.apply(Snapshot {
            host: "ghe.example.com".into(),
            reviews: vec![review(7, ReviewState::Saved, 1, NOW)],
            ..snap(vec![])
        });
        let v = m.view(NOW);
        assert_eq!(
            v.sections[1].rows[0].action,
            Action::OpenReview {
                pr: PrRef {
                    owner: "rzorzal".into(),
                    repo: "blog".into(),
                    number: 7
                },
                url: "https://ghe.example.com/rzorzal/blog/pull/7".into(),
            }
        );
        assert_eq!(
            v.counters[0].action,
            Some(Action::OpenUrl(
                "https://ghe.example.com/pulls/review-requested".into()
            ))
        );
        assert_eq!(
            v.counters[1].action,
            Some(Action::OpenUrl("https://ghe.example.com/pulls".into()))
        );
        assert_eq!(
            v.counters[2].action, None,
            "no window to open without clusia-app"
        );
        assert!(!v.show_home);
    }

    #[test]
    fn empty_first_run() {
        let v = TrayModel::new(false).view(NOW);
        assert_eq!(
            v.status,
            Some(StatusLine {
                text: "Connecting to GitHub…".into(),
                tone: Tone::Neutral
            })
        );
        assert!(v.sections.iter().all(|s| s.rows.is_empty()));
        assert_eq!(v.sections[0].empty, "Nothing waiting for your review");
        assert_eq!(v.sections[1].empty, "No saved reviews");
        assert!(v.heat.is_empty());
        assert_eq!(v.week_label, "");
    }

    #[test]
    fn activity_feeds_heat_and_week_label() {
        let mut m = TrayModel::new(false);
        m.apply(Snapshot {
            activity: Some(ActivitySummary {
                heatmap: vec![
                    DayCount {
                        date: "2026-10-01".into(),
                        count: 0,
                    },
                    DayCount {
                        date: "2026-10-02".into(),
                        count: 4,
                    },
                ],
                published_this_week: 1,
                published_total: 5,
                avg_review_secs: None,
            }),
            ..snap(vec![])
        });
        let v = m.view(NOW);
        assert_eq!(v.heat, vec![0, 4]);
        assert_eq!(v.week_label, "1 review this week");
        assert_eq!(week_label(12), "12 reviews this week");
    }

    #[test]
    fn status_lines() {
        let s = |state, next: Option<i64>| SyncStatus {
            state,
            last_sync_unix: None,
            next_sync_unix: next,
            message: None,
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
