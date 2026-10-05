//! The tray's view model: everything the popover shows, computed without AppKit (spec §7.3).

use std::collections::{HashMap, HashSet};

use clusia_core::time::parse_rfc3339;
use clusia_core::{ActivitySummary, ListSort, Lists, PrSummary, ReviewState};
use clusia_protocol::{ReviewSummary, SyncState, SyncStatus};
use clusia_view::heatmap;
use clusia_view::lists::{self, Entry, is_saved};
pub use clusia_view::status::{StatusLine, Tone, format_age, status_line, week_label};

use crate::actions::Action;

/// Rows per page of each list.
pub const PAGE_SIZE: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ListId {
    Assigned,
    Saved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chip {
    pub label: String,
    pub selected: bool,
    pub action: Action,
}

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
    /// The `lists` preferences from the config.
    pub lists: Lists,
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
            lists: Lists::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counter {
    pub value: usize,
    pub label: &'static str,
    pub action: Option<Action>,
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
    pub id: ListId,
    pub title: &'static str,
    /// The rows of the current page.
    pub rows: Vec<Row>,
    pub sort_label: &'static str,
    pub page: usize,
    pub pages: usize,
    /// Rows after the filter, across all pages.
    pub total: usize,
    /// Shown when `rows` is empty ("No matches" while a filter is active).
    pub empty: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayView {
    pub counters: Vec<Counter>,
    /// Heatmap levels 0–4, oldest day first (empty before activity arrives).
    pub heat: Vec<u8>,
    pub week_label: String,
    /// A refresh is running.
    pub syncing: bool,
    pub paused: bool,
    /// Right side of the caption over the heatmap.
    pub sync_caption: Option<StatusLine>,
    pub status: Option<StatusLine>,
    pub sections: Vec<Section>,
    /// The window is installed, so ⤢ can open Home.
    pub show_home: bool,
    /// Drives the dot on the menu bar icon.
    pub has_news: bool,
    pub chips: Vec<Chip>,
    pub query: String,
}

#[derive(Debug, Default)]
pub struct TrayModel {
    snapshot: Snapshot,
    /// `updated_at` per PR at the last loaded lists; `None` until the first sync finished.
    known: Option<HashMap<String, String>>,
    /// PRs that changed since the user last opened the popover.
    fresh: HashSet<String>,
    app_available: bool,
    /// The live list preferences (the model's own; the daemon config follows them).
    prefs: Lists,
    /// The `lists` section in the last snapshot, to spot external changes.
    seen: Option<Lists>,
    /// Writes not echoed back yet: key -> value.
    pending: HashMap<String, String>,
    /// Writes to send to the daemon (drained by the UI).
    writes: Vec<(String, String)>,
    /// Query value last written to the config.
    saved_query: String,
    pages: HashMap<ListId, usize>,
    /// A `SyncNow` asked for by the UI is running.
    refreshing: bool,
}

impl TrayModel {
    pub fn new(app_available: bool) -> Self {
        Self {
            app_available,
            ..Self::default()
        }
    }

    /// Returns whether a refresh began; one already running makes this a no-op (a double click).
    pub fn refresh_started(&mut self) -> bool {
        !std::mem::replace(&mut self.refreshing, true)
    }

    pub fn refresh_done(&mut self) {
        self.refreshing = false;
    }

    pub fn is_paused(&self) -> bool {
        self.snapshot.sync.as_ref().is_some_and(|s| s.paused)
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
        self.adopt(&snapshot.lists);
        self.snapshot = snapshot;
        for id in [ListId::Assigned, ListId::Saved] {
            let total = self.filtered(id).len();
            if let Some(p) = self.pages.get_mut(&id) {
                *p = lists::clamp_page(*p, total, PAGE_SIZE);
            }
        }
    }

    /// Takes preferences from the snapshot: the first one seeds them, later ones only
    /// bring external changes (not the echo of our own writes).
    fn adopt(&mut self, incoming: &Lists) {
        let Some(seen) = self.seen.replace(incoming.clone()) else {
            self.prefs = incoming.clone();
            self.saved_query = incoming.filter.clone();
            return;
        };
        let before = lists_fields(&seen);
        for ((key, value), (_, old)) in lists_fields(incoming).into_iter().zip(before) {
            match self.pending.get(&key) {
                Some(sent) if *sent == value => {
                    self.pending.remove(&key);
                }
                Some(_) => {}              // our newer value is still on its way
                None if value == old => {} // unchanged since the last snapshot
                None => {
                    self.prefs.apply(&key, &value);
                    self.pages.clear();
                    if key == "lists.filter" {
                        self.saved_query = value;
                    }
                }
            }
        }
    }

    /// The daemon rejected a write: forget it so snapshots are adopted again.
    pub fn write_failed(&mut self, key: &str) {
        self.pending.remove(key);
    }

    pub fn prefs(&self) -> &Lists {
        &self.prefs
    }

    fn queue(&mut self, key: &str, value: String) {
        self.pending.insert(key.to_string(), value.clone());
        self.writes.push((key.to_string(), value));
    }

    pub fn take_writes(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.writes)
    }

    /// Preferences are seeded by the first snapshot; changes before it would be lost.
    fn seeded(&self) -> bool {
        self.seen.is_some()
    }

    pub fn set_query(&mut self, query: &str) {
        if !self.seeded() {
            return;
        }
        let query: String = query
            .chars()
            .take(clusia_core::config::MAX_FILTER_CHARS)
            .collect();
        if query != self.prefs.filter {
            self.prefs.filter = query;
            self.pages.clear();
        }
    }

    /// Saves the query (on popover close), only when it changed since the last save.
    pub fn flush_query(&mut self) {
        if self.prefs.filter != self.saved_query {
            self.saved_query = self.prefs.filter.clone();
            self.queue("lists.filter", self.prefs.filter.clone());
        }
    }

    pub fn cycle_sort(&mut self, id: ListId) {
        let (key, sort) = match id {
            ListId::Assigned => ("lists.assigned_sort", &mut self.prefs.assigned_sort),
            ListId::Saved => ("lists.saved_sort", &mut self.prefs.saved_sort),
        };
        *sort = sort.next();
        let value = sort.as_str().to_string();
        self.pages.remove(&id);
        self.queue(key, value);
    }

    pub fn set_repository(&mut self, repo: Option<String>) {
        let repo = repo.unwrap_or_default();
        if repo != self.prefs.repository {
            self.prefs.repository = repo.clone();
            self.pages.clear();
            self.queue("lists.repository", repo);
        }
    }

    /// Moves a list by `delta` pages, staying within its first and last page.
    pub fn page(&mut self, id: ListId, delta: i8) {
        let total = self.filtered(id).len();
        let current = self.pages.get(&id).copied().unwrap_or(0);
        let next = lists::step_page(current, isize::from(delta), total, PAGE_SIZE);
        self.pages.insert(id, next);
    }

    /// Applies a local action; `false` for actions that launch something, and for
    /// preference changes before the first snapshot (ignored: seeding would overwrite them).
    pub fn handle(&mut self, action: &Action) -> bool {
        match action {
            Action::CycleSort(_) | Action::Repository(_) if !self.seeded() => return false,
            Action::CycleSort(id) => self.cycle_sort(*id),
            Action::Page(id, delta) => self.page(*id, *delta),
            Action::Repository(repo) => self.set_repository(repo.clone()),
            _ => return false,
        }
        true
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
        let saved_count = s.reviews.iter().filter(|r| is_saved(r.state)).count();
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
                    value: saved_count,
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
            syncing: self.refreshing,
            paused: self.is_paused(),
            sync_caption: sync_caption(s.sync.as_ref(), self.refreshing, now),
            status: status_line(s.sync.as_ref(), now),
            sections: vec![
                self.section(ListId::Assigned, now, &web),
                self.section(ListId::Saved, now, &web),
            ],
            show_home: self.app_available,
            has_news: self.has_news(),
            chips: self.chips(),
            query: self.prefs.filter.clone(),
        }
    }

    fn filtered(&self, id: ListId) -> Vec<Entry<'_>> {
        let s = &self.snapshot;
        let (v, sort): (Vec<Entry>, ListSort) = match id {
            ListId::Assigned => (
                s.assigned.iter().map(Entry::Pr).collect(),
                self.prefs.assigned_sort,
            ),
            ListId::Saved => (
                s.reviews
                    .iter()
                    .filter(|r| is_saved(r.state))
                    .map(Entry::Review)
                    .collect(),
                self.prefs.saved_sort,
            ),
        };
        lists::narrow(v, &self.prefs.filter, &self.prefs.repository, sort)
    }

    fn section(&self, id: ListId, now: i64, web: &str) -> Section {
        let entries = self.filtered(id);
        let total = entries.len();
        let pages = lists::page_count(total, PAGE_SIZE);
        let page = lists::clamp_page(self.pages.get(&id).copied().unwrap_or(0), total, PAGE_SIZE);
        let rows = entries
            .iter()
            .skip(page * PAGE_SIZE)
            .take(PAGE_SIZE)
            .map(|e| match e {
                Entry::Pr(p) => self.pr_row(p, now),
                Entry::Review(r) => review_row(r, web, now),
            })
            .collect();
        let filtering = !self.prefs.filter.trim().is_empty() || !self.prefs.repository.is_empty();
        let (title, sort, empty) = match id {
            ListId::Assigned => (
                "Assigned to me",
                self.prefs.assigned_sort,
                "Nothing waiting for your review",
            ),
            ListId::Saved => ("Saved reviews", self.prefs.saved_sort, "No saved reviews"),
        };
        Section {
            id,
            title,
            rows,
            sort_label: sort.label(),
            page,
            pages,
            total,
            empty: if filtering { "No matches" } else { empty },
        }
    }

    fn chips(&self) -> Vec<Chip> {
        let s = &self.snapshot;
        let refs = s.assigned.iter().map(|p| &p.pr).chain(
            s.reviews
                .iter()
                .filter(|r| is_saved(r.state))
                .map(|r| &r.pr),
        );
        lists::repo_chips(refs, &self.prefs.repository)
            .into_iter()
            .map(|c| Chip {
                label: c.label,
                selected: c.selected,
                action: Action::Repository(c.repo),
            })
            .collect()
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

fn lists_fields(l: &Lists) -> Vec<(String, String)> {
    vec![
        (
            "lists.assigned_sort".into(),
            l.assigned_sort.as_str().into(),
        ),
        ("lists.saved_sort".into(), l.saved_sort.as_str().into()),
        ("lists.filter".into(), l.filter.clone()),
        ("lists.repository".into(), l.repository.clone()),
    ]
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

/// "Syncing…" during a refresh, "Syncing paused", "Synced 2m ago", or nothing.
pub fn sync_caption(sync: Option<&SyncStatus>, refreshing: bool, now: i64) -> Option<StatusLine> {
    let line = |text: String, tone| Some(StatusLine { text, tone });
    if refreshing {
        return line("Syncing…".into(), Tone::Neutral);
    }
    let s = sync?;
    if s.paused {
        return line("Syncing paused".into(), Tone::Warning);
    }
    match (s.state, s.last_sync_unix) {
        (SyncState::Online, Some(t)) if now - t < 60 => {
            line("Synced just now".into(), Tone::Neutral)
        }
        (SyncState::Online, Some(t)) => {
            line(format!("Synced {} ago", format_age(now, t)), Tone::Neutral)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::{ActivitySummary, DayCount, ListSort, Lists, PrRef, PrSummary, ReviewState};
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
            paused: false,
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
    fn sync_captions() {
        let s = |paused, last| SyncStatus {
            state: SyncState::Online,
            last_sync_unix: last,
            next_sync_unix: None,
            message: None,
            paused,
        };
        assert_eq!(sync_caption(None, false, NOW), None);
        assert_eq!(sync_caption(None, true, NOW).unwrap().text, "Syncing…");
        assert_eq!(
            sync_caption(Some(&s(false, Some(NOW - 120))), false, NOW)
                .unwrap()
                .text,
            "Synced 2m ago"
        );
        assert_eq!(
            sync_caption(Some(&s(false, Some(NOW - 10))), false, NOW)
                .unwrap()
                .text,
            "Synced just now"
        );
        let paused = sync_caption(Some(&s(true, Some(NOW - 10))), false, NOW).unwrap();
        assert_eq!(
            (paused.text.as_str(), paused.tone),
            ("Syncing paused", Tone::Warning)
        );
        assert_eq!(
            sync_caption(Some(&s(true, None)), true, NOW).unwrap().text,
            "Syncing…",
            "a manual refresh while paused still shows progress"
        );
    }

    #[test]
    fn refresh_shows_syncing_until_the_reply() {
        let mut m = TrayModel::new(true);
        m.apply(snap(many(1)));
        assert!(!m.view(NOW).syncing);
        assert!(m.refresh_started());
        assert!(
            !m.refresh_started(),
            "already refreshing: a second click is ignored"
        );
        assert!(m.view(NOW).syncing);
        m.apply(snap(many(1)));
        assert!(m.view(NOW).syncing, "snapshots alone do not end it");
        m.refresh_done();
        assert!(!m.view(NOW).syncing);
        assert!(m.refresh_started(), "can start again once done");
    }

    #[test]
    fn paused_comes_from_the_snapshot() {
        let mut m = TrayModel::new(true);
        let mut first = snap(many(1));
        first.sync.as_mut().unwrap().paused = true;
        m.apply(first);
        assert!(m.is_paused());
        assert!(m.view(NOW).paused);
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
        assert_eq!(v.sections[0].rows.len(), PAGE_SIZE);
        assert_eq!((v.sections[0].pages, v.sections[0].total), (2, 6));
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

    fn many(n: u64) -> Vec<PrSummary> {
        (1..=n).map(|i| pr(i, NOW - i as i64 * 60)).collect()
    }

    #[test]
    fn pages_replace_more() {
        let mut m = TrayModel::new(false);
        m.apply(snap(many(10)));
        let s = &m.view(NOW).sections[0];
        assert_eq!(
            (s.page, s.pages, s.total, s.rows.len()),
            (0, 3, 10, PAGE_SIZE)
        );
        assert!(m.handle(&Action::Page(ListId::Assigned, 1)));
        assert!(m.handle(&Action::Page(ListId::Assigned, 1)));
        let s = &m.view(NOW).sections[0];
        assert_eq!((s.page, s.rows.len()), (2, 2));
        assert_eq!(s.rows[0].number, "#9");
        m.handle(&Action::Page(ListId::Assigned, 1));
        assert_eq!(m.view(NOW).sections[0].page, 2, "stays on the last page");
        m.apply(snap(many(3)));
        assert_eq!(
            m.view(NOW).sections[0].page,
            0,
            "clamps when the list shrinks"
        );
        assert!(m.take_writes().is_empty(), "pages are not saved");
    }

    #[test]
    fn preferences_wait_for_the_first_snapshot() {
        let mut m = TrayModel::new(false);
        assert!(!m.handle(&Action::CycleSort(ListId::Assigned)));
        assert!(!m.handle(&Action::Repository(Some("rzorzal/blog".into()))));
        m.set_query("auth");
        m.flush_query();
        assert!(m.take_writes().is_empty(), "nothing to overwrite yet");
        let mut first = snap(many(3));
        first.lists.assigned_sort = ListSort::Number;
        m.apply(first);
        assert_eq!(m.prefs().assigned_sort, ListSort::Number, "seeded as-is");
        assert_eq!(m.prefs().repository, "");
        assert_eq!(m.prefs().filter, "");
        assert!(m.handle(&Action::CycleSort(ListId::Assigned)));
        assert_eq!(
            m.take_writes(),
            vec![("lists.assigned_sort".to_string(), "updated".to_string())]
        );
    }

    #[test]
    fn sort_cycles_and_is_saved() {
        let mut m = TrayModel::new(false);
        let mut prs = many(3);
        prs[0].pr.repo = "zeta".into();
        m.apply(snap(prs));
        assert_eq!(m.view(NOW).sections[0].sort_label, "Updated");
        m.handle(&Action::CycleSort(ListId::Assigned));
        let v = m.view(NOW);
        assert_eq!(v.sections[0].sort_label, "Oldest");
        assert_eq!(v.sections[0].rows[0].number, "#3");
        m.handle(&Action::CycleSort(ListId::Assigned));
        assert_eq!(
            m.view(NOW).sections[0].rows.last().unwrap().number,
            "#1",
            "zeta sorts last"
        );
        m.handle(&Action::CycleSort(ListId::Assigned));
        assert_eq!(
            m.view(NOW).sections[0].rows[0].number,
            "#3",
            "number: highest first"
        );
        assert_eq!(
            m.take_writes(),
            vec![
                ("lists.assigned_sort".to_string(), "oldest".to_string()),
                ("lists.assigned_sort".to_string(), "repository".to_string()),
                ("lists.assigned_sort".to_string(), "number".to_string()),
            ]
        );
        assert!(m.take_writes().is_empty());
    }

    #[test]
    fn query_filters_both_lists_and_saves_on_flush() {
        let mut m = TrayModel::new(false);
        let mut prs = many(3);
        prs[1].title = "feat: Auth refresh".into();
        m.apply(Snapshot {
            reviews: vec![
                review(7, ReviewState::Saved, 1, NOW),
                review(8, ReviewState::Saved, 1, NOW),
            ],
            ..snap(prs)
        });
        m.set_query("auth");
        let v = m.view(NOW);
        assert_eq!(v.query, "auth");
        assert_eq!(v.sections[0].rows.len(), 1);
        assert_eq!(v.sections[0].rows[0].number, "#2");
        assert_eq!(v.sections[1].total, 0);
        assert_eq!(v.sections[1].empty, "No matches");
        m.set_query("#8");
        assert_eq!(m.view(NOW).sections[1].rows[0].number, "#8");
        m.set_query("BLOG");
        assert_eq!(m.view(NOW).sections[1].total, 2, "repository name matches");
        assert!(
            m.take_writes().is_empty(),
            "typing is not saved per keystroke"
        );
        m.flush_query();
        assert_eq!(
            m.take_writes(),
            vec![("lists.filter".to_string(), "BLOG".to_string())]
        );
        m.flush_query();
        assert!(
            m.take_writes().is_empty(),
            "unchanged query is not written again"
        );
    }

    #[test]
    fn repository_chips_narrow_both_lists() {
        let mut m = TrayModel::new(false);
        let mut prs = many(2);
        prs[1].pr.repo = "blog".into();
        m.apply(Snapshot {
            reviews: vec![review(7, ReviewState::Saved, 1, NOW)],
            ..snap(prs)
        });
        let chips: Vec<_> = m
            .view(NOW)
            .chips
            .iter()
            .map(|c| (c.label.clone(), c.selected))
            .collect();
        assert_eq!(
            chips,
            vec![
                ("All".into(), true),
                ("blog".into(), false),
                ("clusia".into(), false)
            ]
        );
        m.handle(&Action::Repository(Some("rzorzal/blog".into())));
        let v = m.view(NOW);
        assert_eq!(
            v.sections[0]
                .rows
                .iter()
                .map(|r| r.number.as_str())
                .collect::<Vec<_>>(),
            ["#2"]
        );
        assert_eq!(v.sections[1].total, 1);
        assert!(v.chips.iter().any(|c| c.label == "blog" && c.selected));
        assert_eq!(
            m.take_writes(),
            vec![("lists.repository".to_string(), "rzorzal/blog".to_string())]
        );
        m.handle(&Action::Repository(None));
        assert_eq!(
            m.take_writes(),
            vec![("lists.repository".to_string(), String::new())]
        );
    }

    #[test]
    fn single_repository_shows_no_chips() {
        let mut m = TrayModel::new(false);
        m.apply(snap(many(3)));
        assert!(m.view(NOW).chips.is_empty());
    }

    #[test]
    fn config_echo_is_not_adopted_but_external_changes_are() {
        let mut m = TrayModel::new(false);
        m.apply(snap(many(2)));
        m.handle(&Action::CycleSort(ListId::Saved)); // local: Oldest, write pending
        let _ = m.take_writes();
        // A stale snapshot from before the echo still says Updated: ignored (write pending).
        m.apply(snap(many(2)));
        assert_eq!(m.prefs().saved_sort, ListSort::Oldest);
        // The echo arrives: adopted silently, pending cleared.
        let mut echo = snap(many(2));
        echo.lists.saved_sort = ListSort::Oldest;
        m.apply(echo.clone());
        assert_eq!(m.prefs().saved_sort, ListSort::Oldest);
        // Another client (Home, CLI) changes it: adopted.
        echo.lists.saved_sort = ListSort::Number;
        m.apply(echo);
        assert_eq!(m.prefs().saved_sort, ListSort::Number);
    }

    #[test]
    fn saved_preferences_are_restored_on_start() {
        let mut m = TrayModel::new(false);
        let mut first = snap(many(3));
        first.lists = Lists {
            assigned_sort: ListSort::Oldest,
            saved_sort: ListSort::Updated,
            mine_sort: ListSort::Updated,
            filter: "change 2".into(),
            repository: String::new(),
        };
        m.apply(first);
        let v = m.view(NOW);
        assert_eq!(v.query, "change 2");
        assert_eq!(v.sections[0].sort_label, "Oldest");
        assert_eq!(
            v.sections[0]
                .rows
                .iter()
                .map(|r| r.number.as_str())
                .collect::<Vec<_>>(),
            ["#2"]
        );
    }

    #[test]
    fn chips_only_for_listed_repositories() {
        let mut m = TrayModel::new(false);
        m.apply(Snapshot {
            reviews: vec![review(7, ReviewState::Active, 1, NOW)],
            ..snap(many(2))
        });
        assert!(m.view(NOW).chips.is_empty());
    }

    #[test]
    fn same_short_name_uses_full_labels() {
        let mut m = TrayModel::new(false);
        let mut prs = many(2);
        prs[0].pr.owner = "octo".into();
        prs[0].pr.repo = "app".into();
        prs[1].pr.owner = "acme".into();
        prs[1].pr.repo = "app".into();
        m.apply(snap(prs));
        let labels: Vec<_> = m.view(NOW).chips.iter().map(|c| c.label.clone()).collect();
        assert_eq!(labels, ["All", "acme/app", "octo/app"]);
    }

    #[test]
    fn pages_reset_on_query_sort_and_repository_changes() {
        let mut m = TrayModel::new(false);
        let mut prs = many(10);
        prs[0].pr.repo = "other".into();
        m.apply(snap(prs));
        let page = |m: &TrayModel| m.view(NOW).sections[0].page;
        m.handle(&Action::Page(ListId::Assigned, 1));
        m.set_query("change");
        assert_eq!(page(&m), 0);
        m.handle(&Action::Page(ListId::Assigned, 1));
        m.cycle_sort(ListId::Assigned);
        assert_eq!(page(&m), 0);
        m.handle(&Action::Page(ListId::Assigned, 1));
        m.set_repository(Some("rzorzal/clusia".into()));
        assert_eq!(page(&m), 0);
    }

    #[test]
    fn query_is_capped() {
        let mut m = TrayModel::new(false);
        m.apply(snap(many(1)));
        m.set_query(&"a".repeat(500));
        assert_eq!(m.prefs().filter.chars().count(), 200);
    }

    #[test]
    fn external_change_resets_pages_and_failed_writes_unblock() {
        let mut m = TrayModel::new(false);
        m.apply(snap(many(10)));
        m.handle(&Action::Page(ListId::Assigned, 1));
        let mut ext = snap(many(10));
        ext.lists.saved_sort = ListSort::Number;
        m.apply(ext);
        assert_eq!(m.view(NOW).sections[0].page, 0);
        m.cycle_sort(ListId::Saved);
        m.write_failed("lists.saved_sort");
        let mut s = snap(many(10));
        s.lists.saved_sort = ListSort::Repository;
        m.apply(s);
        assert_eq!(m.prefs().saved_sort, ListSort::Repository);
    }
}
