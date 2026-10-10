//! What is worth telling the user. Every source (the notifications feed, saved reviews that
//! went stale, check results, sync problems, files that had to be reset) turns what it saw
//! into a `NotifyEvent`; `Engine::process` dedupes it, routes it by the settings (tray list,
//! macOS banner, sound) and remembers it, and `deliver` publishes the result to the tray.
//! The tray is the only process that posts banners.

use std::path::Path;

use clusia_core::config::{EventKind, Notifications};
use clusia_core::logging::local_date;
use clusia_core::notify::{LocalTime, NotifyEvent, Recent, SyncProblem, decide, subtitle};
use clusia_core::time::parse_rfc3339;
use clusia_core::{CommitInfo, OpenTarget, PrRef, Review};
use clusia_protocol::{Event, InboxItem, PermissionStatus, SyncState, topics};
use clusia_provider::{CheckState, GitHub, Notification};

use crate::inbox::InboxData;
use crate::state::Shared;
use crate::sync::{github_client, now_unix};

/// Offline this long is worth a banner; shorter outages sort themselves out.
const OFFLINE_TOLD_AFTER_SECS: i64 = 600;
/// The feed is read from this far back at most, however long the daemon was away.
const FEED_LOOKBACK_SECS: i64 = 7 * 86_400;
/// Each read starts this much before the previous one ended; the dedupe keys absorb the overlap.
const FEED_OVERLAP_SECS: i64 = 60;
/// The feed is read at most this often, whatever the sync interval or the refresh button do:
/// GitHub asks clients to poll it no faster than once a minute.
const FEED_MIN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
/// Pull requests whose checks are looked at after each sync.
const MAX_CHECKED: usize = 15;

/// `1970-01-01T00:00:00Z` for `unix` seconds.
pub(crate) fn format_rfc3339(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    // Days since 1970-01-01 to a proleptic Gregorian date.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs / 3_600,
        secs % 3_600 / 60,
        secs % 60
    )
}

// ---- events ---------------------------------------------------------------------------

/// Stands in for a name that could not be looked up.
const SOMEONE: &str = "someone";

/// An entry of the notifications feed as an event; `by` is who asked for the review or wrote
/// the comment, when known. Only unread entries about pull requests, for the reasons Clúsia
/// tells about.
pub(crate) fn from_notification(
    n: &Notification,
    by: Option<&str>,
    now: i64,
) -> Option<NotifyEvent> {
    if !n.unread {
        return None;
    }
    let pr = n.pr()?;
    let at = parse_rfc3339(&n.updated_at).unwrap_or(now);
    let comment = n.comment_id();
    let by = by.unwrap_or(SOMEONE);
    // A comment is announced once, whichever way it reached the feed.
    let comment_key = |id: u64| format!("comment:{pr}:{id}");
    Some(match n.reason.as_str() {
        "review_requested" => NotifyEvent::review_requested(
            &pr,
            &n.title,
            by,
            format!("review_requested:{pr}:{}", n.id),
            at,
        ),
        // A mention outside a comment (the description, say) has no id of its own: the thread
        // is announced once, whatever else later changes on it.
        "mention" | "team_mention" => {
            let key = match comment {
                Some(id) => comment_key(id),
                None => format!("mentioned:{pr}:{}", n.id),
            };
            NotifyEvent::mentioned(&pr, &n.title, by, key, at)
        }
        // Without a comment the thread changed some other way, which is no reply.
        "comment" | "author" => {
            let id = comment?;
            NotifyEvent::reply_to_you(&pr, &n.title, by, Some(id.to_string()), comment_key(id), at)
        }
        _ => return None,
    })
}

/// How many commits a pull request has beyond `old_head`; at least one, since a rewritten
/// history no longer contains it.
pub(crate) fn commits_since(commits: &[CommitInfo], old_head: &str) -> u32 {
    match commits.iter().position(|c| c.sha == old_head) {
        Some(at) => (commits.len() - at - 1).max(1) as u32,
        None => 1,
    }
}

/// A saved review whose pull request moved on.
pub(crate) fn commits_after_review_event(review: &Review, commits: u32, now: i64) -> NotifyEvent {
    NotifyEvent::commits_after_review(
        &review.pr,
        &review.title,
        commits,
        format!("commits_after_review:{}:{}", review.pr, review.head_sha),
        now,
    )
}

fn sync_problem_event(problem: SyncProblem, now: i64) -> NotifyEvent {
    let name = match problem {
        SyncProblem::Unauthorized => "unauthorized",
        SyncProblem::Offline => "offline",
        SyncProblem::RateLimited => "rate_limited",
    };
    // One banner per kind of problem per day, so a restart or a flapping link stays quiet.
    NotifyEvent::sync_problem(
        problem,
        format!("sync_problem:{name}:{}", local_date(now)),
        now,
    )
}

/// `original` is the file that was unreadable, `set_aside` the name it was moved to.
fn state_recovered_event(original: &str, set_aside: &str, now: i64) -> NotifyEvent {
    NotifyEvent::state_recovered(original, format!("state_recovered:{set_aside}"), now)
}

// ---- sync problems ----------------------------------------------------------------------

/// Turns the sync states the daemon passes through into problems: a rejected token and a rate
/// limit when they begin, being offline once it has lasted a while.
#[derive(Debug, Default)]
pub(crate) struct SyncWatch {
    offline_since: Option<i64>,
    offline_told: bool,
}

impl SyncWatch {
    pub fn observe(
        &mut self,
        previous: SyncState,
        state: SyncState,
        now: i64,
    ) -> Option<SyncProblem> {
        if state != SyncState::Offline {
            self.offline_since = None;
            self.offline_told = false;
        }
        match state {
            SyncState::Unauthorized => {
                (previous != SyncState::Unauthorized).then_some(SyncProblem::Unauthorized)
            }
            SyncState::RateLimited => {
                (previous != SyncState::RateLimited).then_some(SyncProblem::RateLimited)
            }
            SyncState::Offline => {
                let since = *self.offline_since.get_or_insert(now);
                if now - since >= OFFLINE_TOLD_AFTER_SECS && !self.offline_told {
                    self.offline_told = true;
                    Some(SyncProblem::Offline)
                } else {
                    None
                }
            }
            SyncState::Online | SyncState::NotYet => None,
        }
    }
}

// ---- the engine -------------------------------------------------------------------------

/// What `Engine::process` decided for one event.
pub(crate) struct Processed {
    pub notify: Option<Event>,
    /// The inbox or the remembered keys changed and must be saved.
    pub changed: bool,
    /// A new event that would make a banner were it not part of a burst.
    pub posts: bool,
}

/// What `Engine::process_batch` made of several events.
pub(crate) struct Batch {
    /// The banners to publish, a burst on one pull request already summed up in one.
    pub banners: Vec<Event>,
    /// The inbox or the remembered keys changed and must be saved.
    pub changed: bool,
}

/// The persisted inbox plus the short-term memory that routing needs.
pub(crate) struct Engine {
    pub data: InboxData,
    recent: Recent,
    watch: SyncWatch,
    /// When the feed was last asked for. In memory only: a restart may read it once more.
    feed_read_at: Option<std::time::Instant>,
}

impl Engine {
    pub fn new(data: InboxData) -> Self {
        Self {
            data,
            recent: Recent::default(),
            watch: SyncWatch::default(),
            feed_read_at: None,
        }
    }

    /// Dedupes `event` by its key, routes it by `cfg` for a banner shown at `local`, files it
    /// in the inbox when the settings say so, and remembers it.
    pub fn process(
        &mut self,
        event: NotifyEvent,
        cfg: &Notifications,
        local: LocalTime,
    ) -> Processed {
        if self.data.has_key(&event.key) {
            return Processed {
                notify: None,
                changed: false,
                posts: false,
            };
        }
        self.data.remember(&event.key, event.at);
        let decision = decide(&event, cfg, local, &self.recent);
        let posts = decide(&event, cfg, local, &Recent::default()).macos;
        if decision.tray {
            self.data.push(InboxItem {
                id: event.key.clone(),
                kind: event.kind,
                pr: event.pr.clone(),
                title: event.title.clone(),
                body: event.body.clone(),
                at: event.at,
                seen: false,
            });
        }
        let notify = decision.macos.then(|| {
            if let Some(group) = &decision.group {
                self.recent.record(group, event.at);
            }
            Event::Notify {
                id: event.key.clone(),
                title: event.title.clone(),
                subtitle: subtitle(&event),
                body: event.body.clone(),
                sound: decision.sound.map(|s| s.as_str().to_string()),
                time_sensitive: event.is_urgent() || !cfg.follow_focus,
                open: event.open.clone(),
            }
        });
        self.recent.prune(event.at);
        Processed {
            notify,
            changed: true,
            posts,
        }
    }

    /// `process` for each of `events`, oldest first. Several new events about one pull request
    /// that would each make a banner make one, which says how many there are.
    pub fn process_batch(
        &mut self,
        mut events: Vec<NotifyEvent>,
        cfg: &Notifications,
        local: LocalTime,
    ) -> Batch {
        events.sort_by_key(|e| (e.at, priority(e.kind)));
        let mut changed = false;
        // Each banner with the titles of the events it stands for: its own, then those that
        // the grouping window folded into it.
        let mut banners: Vec<(Event, Option<PrRef>, Vec<String>)> = Vec::new();
        // The latest banner of each pull request in this batch.
        let mut latest: std::collections::HashMap<PrRef, usize> = Default::default();
        for event in events {
            let pr = event.pr.clone();
            let title = event.title.clone();
            let processed = self.process(event, cfg, local);
            changed |= processed.changed;
            match processed.notify {
                Some(banner) => {
                    if let Some(pr) = &pr {
                        latest.insert(pr.clone(), banners.len());
                    }
                    banners.push((banner, pr, vec![title]));
                }
                None if cfg.group_bursts && processed.posts => {
                    if let Some(&i) = pr.as_ref().and_then(|pr| latest.get(pr)) {
                        banners[i].2.push(title);
                    }
                }
                None => {}
            }
        }
        let banners = banners
            .into_iter()
            .map(|(mut banner, pr, titles)| {
                if let (Event::Notify { title, body, .. }, Some(pr)) = (&mut banner, &pr)
                    && titles.len() > 1
                {
                    (*title, *body) = group_banner(&titles, pr.number);
                }
                banner
            })
            .collect();
        Batch { banners, changed }
    }

    /// Whether the feed may be read at `now`, which then counts as its last read.
    pub fn may_read_feed(&mut self, now: std::time::Instant) -> bool {
        if self
            .feed_read_at
            .is_some_and(|at| now.saturating_duration_since(at) < FEED_MIN_INTERVAL)
        {
            return false;
        }
        self.feed_read_at = Some(now);
        true
    }
}

/// Reviews and mentions first, so that when a burst is grouped the banner is the one that
/// asks for something.
fn priority(kind: EventKind) -> u8 {
    match kind {
        EventKind::ReviewRequested => 0,
        EventKind::Mentioned => 1,
        EventKind::ReplyToYou => 2,
        EventKind::CommitsAfterReview => 3,
        EventKind::ChecksFailed => 4,
        _ => 5,
    }
}

/// The one banner for several events about one pull request that arrived together.
fn group_banner(titles: &[String], number: u64) -> (String, String) {
    let mut distinct: Vec<&str> = Vec::new();
    for title in titles {
        if !distinct.contains(&title.as_str()) {
            distinct.push(title);
        }
    }
    let shown = distinct
        .iter()
        .take(3)
        .copied()
        .collect::<Vec<_>>()
        .join(" · ");
    let body = match distinct.len().saturating_sub(3) {
        0 => shown,
        more => format!("{shown} · and {more} more"),
    };
    (format!("{} updates on #{number}", titles.len()), body)
}

/// Files and announces `events`: the inbox is saved once, then each banner goes to the tray
/// topic, then the new unseen count.
pub(crate) async fn deliver(shared: &Shared, events: Vec<NotifyEvent>) {
    if events.is_empty() {
        return;
    }
    let cfg = shared.config.read().await.notifications.clone();
    let local = LocalTime::from_unix(now_unix());
    let mut engine = shared.engine.lock().await;
    let batch = engine.process_batch(events, &cfg, local);
    if !batch.changed {
        return;
    }
    if let Err(e) = engine.data.save(&shared.paths) {
        tracing::warn!(error = %e, "cannot save the inbox");
    }
    let unseen = engine.data.unseen();
    drop(engine);
    for banner in batch.banners {
        shared.publish(topics::TRAY, banner);
    }
    shared.publish(topics::TRAY, Event::InboxChanged { unseen });
}

// ---- sources ----------------------------------------------------------------------------

/// Runs after every sync, once the saved reviews were checked: the notifications feed, and a
/// wake-up for the task that looks at the checks of the pull requests you opened (so a refresh
/// never waits on those). Failures are logged and never touch the sync state.
pub(crate) async fn after_sync(shared: &Shared) {
    flush_recovered(shared).await;
    if shared.sync.read().await.state != SyncState::Online {
        return;
    }
    let Ok(Some(gh)) = github_client(shared).await else {
        return;
    };
    let now = now_unix();
    poll_feed(shared, &gh, now).await;
    shared.checks_wake.notify_one();
}

async fn poll_feed(shared: &Shared, gh: &GitHub, now: i64) {
    let since = {
        let mut engine = shared.engine.lock().await;
        let Some(since) = engine.data.feed_since.clone() else {
            drop(engine);
            // The first read only marks the starting point: nothing that happened before the
            // daemon first looked is announced.
            set_feed_since(shared, now).await;
            return;
        };
        if !engine.may_read_feed(std::time::Instant::now()) {
            return;
        }
        since
    };
    let floor = now - FEED_LOOKBACK_SECS;
    let since = match parse_rfc3339(&since) {
        Some(t) if t >= floor => since,
        _ => format_rfc3339(floor),
    };
    let feed = match gh.list_notifications(Some(&since)).await {
        Ok(feed) => feed,
        Err(e) => {
            tracing::debug!(error = %e, "cannot read the notifications feed");
            return;
        }
    };
    let mut events = Vec::new();
    // Asked for once, and only when someone has to be named.
    let mut viewer: Option<Option<String>> = None;
    // An entry left for later keeps the feed's starting point, so the next read lists it again.
    let mut left_for_later = false;
    for n in &feed {
        let Some(mut candidate) = from_notification(n, None, now) else {
            continue;
        };
        if shared.engine.lock().await.data.has_key(&candidate.key) {
            continue;
        }
        let by = match who(gh, n, candidate.kind).await {
            Who::Named(by) => Some(by),
            Who::Nobody => None,
            // It may be your own comment: not announced, and not remembered either.
            Who::Unknown => {
                left_for_later = true;
                continue;
            }
        };
        if let Some(by) = by {
            if viewer.is_none() {
                viewer = Some(gh.viewer().await.ok().map(|v| v.login));
            }
            let Some(Some(me)) = &viewer else {
                left_for_later = true;
                continue;
            };
            if *me == by {
                // What you wrote yourself is no news to you; remembered so it is not looked
                // up again.
                shared
                    .engine
                    .lock()
                    .await
                    .data
                    .remember(&candidate.key, candidate.at);
                continue;
            }
            if let Some(named) = from_notification(n, Some(&by), now) {
                candidate = named;
            }
        }
        events.push(candidate);
    }
    deliver(shared, events).await;
    if !left_for_later {
        set_feed_since(shared, now - FEED_OVERLAP_SECS).await;
    }
}

enum Who {
    Named(String),
    /// The entry names no one.
    Nobody,
    /// It names someone GitHub could not tell us about now.
    Unknown,
}

/// Who asked for the review (the author of the pull request) or wrote the comment.
async fn who(gh: &GitHub, n: &Notification, kind: EventKind) -> Who {
    let found = match kind {
        EventKind::ReviewRequested => match n.pr() {
            Some(pr) => gh.get_pr(&pr).await.map(|d| d.summary.author),
            None => return Who::Nobody,
        },
        EventKind::Mentioned | EventKind::ReplyToYou => {
            match (n.comment_id(), n.latest_comment_url.as_deref()) {
                (Some(_), Some(url)) => gh.comment_author(url).await,
                _ => return Who::Nobody,
            }
        }
        _ => return Who::Nobody,
    };
    found.map_or(Who::Unknown, Who::Named)
}

async fn set_feed_since(shared: &Shared, unix: i64) {
    let mut engine = shared.engine.lock().await;
    engine.data.feed_since = Some(format_rfc3339(unix));
    if let Err(e) = engine.data.save(&shared.paths) {
        tracing::warn!(error = %e, "cannot save the inbox");
    }
}

fn check_label(state: CheckState) -> &'static str {
    match state {
        CheckState::NoChecks => "none",
        CheckState::Pending => "pending",
        CheckState::Passing => "passing",
        CheckState::Failing => "failing",
    }
}

/// Whether CI just went red: the pull request was seen before, and now fails on a commit
/// where it did not (or did not fail) at the last look.
pub(crate) fn turned_red(previous: Option<&str>, sha: &str, state: CheckState) -> bool {
    state == CheckState::Failing
        && previous.is_some_and(|prev| prev != format!("{sha}:{}", check_label(state)))
}

/// Whether a pull request's checks are worth asking about again: it changed since the last
/// look (any push or comment moves `updated_at`), or its checks had not finished.
fn needs_look(known: Option<&str>, updated_at: &str) -> bool {
    match known.and_then(|entry| entry.split_once('|')) {
        Some((seen_at, rest)) => seen_at != updated_at || rest.ends_with(":pending"),
        None => true,
    }
}

/// The `<sha>:<state>` half of a stored entry.
fn last_state(known: Option<&str>) -> Option<&str> {
    known?.split_once('|').map(|(_, rest)| rest)
}

/// Waits for `after_sync` to call and then looks at the checks of your pull requests. It runs
/// on its own so that a sync, and the refresh button, never wait for the requests it makes.
pub(crate) async fn run_checks(shared: std::sync::Arc<Shared>) {
    let mut shutdown = shared.shutdown.subscribe();
    loop {
        tokio::select! {
            _ = shared.checks_wake.notified() => {}
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
                continue;
            }
        }
        if shared.sync.read().await.state != SyncState::Online {
            continue;
        }
        if let Ok(Some(gh)) = github_client(&shared).await {
            poll_checks(&shared, &gh, now_unix()).await;
        }
    }
}

async fn poll_checks(shared: &Shared, gh: &GitHub, now: i64) {
    let mine: Vec<_> = shared
        .prs
        .read()
        .await
        .mine
        .iter()
        .take(MAX_CHECKED)
        .cloned()
        .collect();
    let before = shared.engine.lock().await.data.checks.clone();
    let mut after = std::collections::BTreeMap::new();
    let mut events = Vec::new();
    for summary in mine {
        let key = summary.pr.file_key();
        let known = before.get(&key).map(String::as_str);
        if !needs_look(known, &summary.updated_at)
            && let Some(known) = known
        {
            after.insert(key, known.to_string());
            continue;
        }
        let looked = async {
            let detail = gh.get_pr(&summary.pr).await.ok()?;
            let report = gh
                .get_check_report(&summary.pr, &detail.head_sha)
                .await
                .ok()?;
            Some((detail.head_sha, report))
        }
        .await;
        let Some((sha, report)) = looked else {
            // Not seen this time: keep what was known.
            if let Some(known) = known {
                after.insert(key, known.to_string());
            }
            continue;
        };
        if turned_red(last_state(known), &sha, report.state) {
            events.push(NotifyEvent::checks_failed(
                &summary.pr,
                &summary.title,
                report.failed,
                format!("checks_failed:{}:{sha}", summary.pr),
                now,
            ));
        }
        after.insert(
            key,
            format!("{}|{sha}:{}", summary.updated_at, check_label(report.state)),
        );
    }
    {
        let mut engine = shared.engine.lock().await;
        if engine.data.checks != after {
            engine.data.checks = after;
            if let Err(e) = engine.data.save(&shared.paths) {
                tracing::warn!(error = %e, "cannot save the inbox");
            }
        }
    }
    deliver(shared, events).await;
}

/// A saved review went out of date because its pull request got new commits after
/// `old_head`.
pub(crate) async fn review_outdated(shared: &Shared, review: &Review, old_head: &str) {
    let commits = match github_client(shared).await {
        Ok(Some(gh)) => gh
            .get_commits(&review.pr)
            .await
            .map(|all| commits_since(&all, old_head))
            .unwrap_or(1),
        _ => 1,
    };
    deliver(
        shared,
        vec![commits_after_review_event(review, commits, now_unix())],
    )
    .await;
}

/// Called with every sync status the daemon computes.
pub(crate) async fn sync_observed(shared: &Shared, previous: SyncState, state: SyncState) {
    let now = now_unix();
    let problem = shared
        .engine
        .lock()
        .await
        .watch
        .observe(previous, state, now);
    if let Some(problem) = problem {
        deliver(shared, vec![sync_problem_event(problem, now)]).await;
    }
}

/// Records that `file`, which could not be read, was moved to its `.corrupt-<time>` name. The
/// banner goes out with the next sync, when the tray is surely there to show it.
pub(crate) fn note_recovered(shared: &Shared, file: &Path) {
    let set_aside = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| file.display().to_string());
    let original = set_aside
        .split(".corrupt-")
        .next()
        .unwrap_or(&set_aside)
        .to_string();
    shared
        .recovered
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push((original, set_aside));
}

async fn flush_recovered(shared: &Shared) {
    let pending = std::mem::take(&mut *shared.recovered.lock().unwrap_or_else(|p| p.into_inner()));
    let now = now_unix();
    let events = pending
        .iter()
        .map(|(original, set_aside)| state_recovered_event(original, set_aside, now))
        .collect();
    deliver(shared, events).await;
}

// ---- commands ---------------------------------------------------------------------------

pub(crate) async fn inbox(shared: &Shared) -> Vec<InboxItem> {
    shared.engine.lock().await.data.list()
}

pub(crate) async fn mark_seen(shared: &Shared, ids: &[String]) {
    let mut engine = shared.engine.lock().await;
    if !engine.data.mark_seen(ids) {
        return;
    }
    if let Err(e) = engine.data.save(&shared.paths) {
        tracing::warn!(error = %e, "cannot save the inbox");
    }
    let unseen = engine.data.unseen();
    drop(engine);
    shared.publish(topics::TRAY, Event::InboxChanged { unseen });
}

pub(crate) fn set_permission(shared: &Shared, status: PermissionStatus) {
    *shared.permission.lock().unwrap_or_else(|p| p.into_inner()) = status;
}

pub(crate) fn permission(shared: &Shared) -> PermissionStatus {
    *shared.permission.lock().unwrap_or_else(|p| p.into_inner())
}

/// A banner with the chosen sound, straight to the tray: it skips the settings, the inbox and
/// the dedupe, because its whole point is to show that banners arrive.
pub(crate) async fn send_test(shared: &Shared) {
    let notifications = shared.config.read().await.notifications.clone();
    // Nanoseconds, so two tests a second apart still have different ids.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    shared.publish(
        topics::TRAY,
        Event::Notify {
            id: format!("test-{nanos}"),
            title: "Test notification".into(),
            subtitle: String::new(),
            body: "If you can read this, notifications work.".into(),
            sound: Some(notifications.sound.as_str().to_string()),
            open: OpenTarget::Config {
                page: "notifications".into(),
            },
            time_sensitive: !notifications.follow_focus,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::PrRef;
    use clusia_core::config::{Dnd, HourMinute, Weekday};
    use std::time::Duration;

    fn pr() -> PrRef {
        "rzorzal/clusia#123".parse().unwrap()
    }

    fn entry(reason: &str, comment: Option<u64>) -> Notification {
        Notification {
            id: "77".into(),
            reason: reason.into(),
            unread: true,
            updated_at: "2026-10-01T12:00:00Z".into(),
            subject_type: "PullRequest".into(),
            title: "feat: auth refresh".into(),
            subject_url: Some("https://api.github.com/repos/rzorzal/clusia/pulls/123".into()),
            latest_comment_url: comment.map(|id| {
                format!("https://api.github.com/repos/rzorzal/clusia/issues/comments/{id}")
            }),
            repository: "rzorzal/clusia".into(),
        }
    }

    fn at(minutes: u16) -> LocalTime {
        LocalTime {
            weekday: Weekday::Wed,
            minutes,
        }
    }

    fn engine() -> Engine {
        Engine::new(InboxData::default())
    }

    fn mention(n: u64, at_secs: i64, pr: &str) -> NotifyEvent {
        let pr: PrRef = pr.parse().unwrap();
        NotifyEvent::mentioned(&pr, "t", "octo", format!("comment:{pr}:{n}"), at_secs)
    }

    #[test]
    fn rfc3339_formatting_round_trips_with_the_parser() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        for t in [946_684_800, 1_790_000_123, 4_102_444_799, 86_399] {
            assert_eq!(parse_rfc3339(&format_rfc3339(t)), Some(t), "{t}");
        }
    }

    #[test]
    fn a_review_request_names_who_asked() {
        let e = from_notification(&entry("review_requested", None), Some("octo"), 0).unwrap();
        assert_eq!(e.kind, EventKind::ReviewRequested);
        assert_eq!(e.title, "Review requested");
        assert_eq!(
            e.body,
            "@octo asked you to review rzorzal/clusia #123 · feat: auth refresh"
        );
        assert_eq!(e.key, "review_requested:rzorzal/clusia#123:77");
        assert_eq!(
            e.open,
            OpenTarget::Review {
                pr: pr(),
                thread: None
            }
        );
        assert_eq!(e.at, parse_rfc3339("2026-10-01T12:00:00Z").unwrap());
        let unnamed = from_notification(&entry("review_requested", None), None, 0).unwrap();
        assert!(
            unnamed.body.starts_with("@someone asked you"),
            "{}",
            unnamed.body
        );
    }

    #[test]
    fn mentions_and_replies_share_one_key_per_comment() {
        let mention = from_notification(&entry("mention", Some(991)), Some("octo"), 0).unwrap();
        let team = from_notification(&entry("team_mention", Some(991)), Some("octo"), 0).unwrap();
        let reply = from_notification(&entry("comment", Some(991)), Some("octo"), 0).unwrap();
        assert_eq!(mention.kind, EventKind::Mentioned);
        assert_eq!(team.kind, EventKind::Mentioned);
        assert_eq!(reply.kind, EventKind::ReplyToYou);
        assert_eq!(mention.title, "@octo mentioned you");
        assert_eq!(reply.title, "@octo replied to you");
        assert_eq!(mention.key, "comment:rzorzal/clusia#123:991");
        assert_eq!(reply.key, mention.key, "one comment, one announcement");
        assert_eq!(
            reply.open,
            OpenTarget::Review {
                pr: pr(),
                thread: Some("991".into())
            }
        );
        let author = from_notification(&entry("author", Some(992)), None, 0).unwrap();
        assert_eq!(author.kind, EventKind::ReplyToYou);
        assert_eq!(author.key, "comment:rzorzal/clusia#123:992");
    }

    #[test]
    fn a_thread_update_that_is_not_a_comment_is_not_a_reply() {
        for reason in ["comment", "author"] {
            let mut n = entry(reason, None);
            assert!(from_notification(&n, None, 0).is_none(), "{reason}");
            n.updated_at = "2026-10-01T13:00:00Z".into();
            assert!(
                from_notification(&n, None, 0).is_none(),
                "{reason}: a later push, review or label"
            );
            n.latest_comment_url =
                Some("https://api.github.com/repos/rzorzal/clusia/pulls/123".into());
            assert!(from_notification(&n, None, 0).is_none(), "{reason}");
        }
    }

    #[test]
    fn a_mention_without_a_comment_is_announced_once_per_thread() {
        let mut n = entry("mention", None);
        n.latest_comment_url = Some("https://api.github.com/repos/rzorzal/clusia/pulls/123".into());
        let first = from_notification(&n, None, 0).unwrap();
        assert_eq!(first.kind, EventKind::Mentioned);
        assert_eq!(first.key, "mentioned:rzorzal/clusia#123:77");
        n.updated_at = "2026-10-01T13:00:00Z".into();
        n.latest_comment_url = None;
        assert_eq!(from_notification(&n, None, 0).unwrap().key, first.key);
    }

    #[test]
    fn the_feed_is_read_again_only_after_a_minute() {
        let mut engine = engine();
        let t0 = std::time::Instant::now();
        assert!(engine.may_read_feed(t0), "never read");
        assert!(!engine.may_read_feed(t0 + Duration::from_secs(30)));
        assert!(!engine.may_read_feed(t0 + Duration::from_secs(59)));
        assert!(engine.may_read_feed(t0 + Duration::from_secs(60)));
        assert!(!engine.may_read_feed(t0 + Duration::from_secs(61)));
        assert!(engine.may_read_feed(t0 + Duration::from_secs(125)));
    }

    #[test]
    fn a_grouped_title_counts_only_the_events_that_would_make_a_banner() {
        let cfg = Notifications::default();
        let mut engine = engine();
        let failed =
            NotifyEvent::checks_failed(&pr(), "feat: auth refresh", 2, "checks_failed:x:c1", 1_000);
        let events = vec![
            mention(1, 1_000, "rzorzal/clusia#123"),
            mention(2, 1_001, "rzorzal/clusia#123"),
            failed,
        ];
        let batch = engine.process_batch(events, &cfg, at(600));
        assert!(batch.changed);
        assert_eq!(
            engine.data.items.len(),
            3,
            "every event is in the tray list"
        );
        match &batch.banners[..] {
            [Event::Notify { title, body, .. }] => {
                assert_eq!(title, "2 updates on #123");
                assert_eq!(body, "@octo mentioned you");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn each_banner_counts_only_its_own_burst() {
        let cfg = Notifications::default();
        let mut engine = engine();
        // One poll after a sleep: two bursts on one pull request, 200 s apart.
        let events = vec![
            mention(1, 1_000, "rzorzal/clusia#123"),
            mention(2, 1_001, "rzorzal/clusia#123"),
            mention(3, 1_200, "rzorzal/clusia#123"),
        ];
        let batch = engine.process_batch(events, &cfg, at(600));
        let titles: Vec<&str> = batch
            .banners
            .iter()
            .map(|b| match b {
                Event::Notify { title, .. } => title.as_str(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(titles, ["2 updates on #123", "@octo mentioned you"]);
    }

    #[test]
    fn other_reasons_issues_and_read_entries_are_ignored() {
        assert!(from_notification(&entry("subscribed", None), None, 0).is_none());
        assert!(from_notification(&entry("state_change", None), None, 0).is_none());
        let mut issue = entry("mention", Some(1));
        issue.subject_type = "Issue".into();
        assert!(from_notification(&issue, None, 0).is_none());
        let mut read = entry("mention", Some(1));
        read.unread = false;
        assert!(from_notification(&read, None, 0).is_none());
    }

    #[test]
    fn the_same_event_twice_is_announced_once() {
        let cfg = Notifications::default();
        let mut engine = engine();
        let first = engine.process(mention(1, 1_000, "rzorzal/clusia#123"), &cfg, at(600));
        assert!(first.changed && first.notify.is_some());
        let again = engine.process(mention(1, 1_001, "rzorzal/clusia#123"), &cfg, at(600));
        assert!(!again.changed && again.notify.is_none());
        assert_eq!(engine.data.items.len(), 1);
    }

    #[test]
    fn a_banner_carries_title_subtitle_sound_and_target() {
        let cfg = Notifications::default();
        let mut engine = engine();
        let e = from_notification(&entry("review_requested", None), Some("octo"), 0).unwrap();
        match engine.process(e, &cfg, at(600)).notify.unwrap() {
            Event::Notify {
                id,
                title,
                subtitle,
                sound,
                open,
                ..
            } => {
                assert_eq!(id, "review_requested:rzorzal/clusia#123:77");
                assert_eq!(title, "Review requested");
                assert_eq!(subtitle, "rzorzal/clusia #123");
                assert_eq!(sound.as_deref(), Some("leaf"));
                assert_eq!(
                    open,
                    OpenTarget::Review {
                        pr: pr(),
                        thread: None
                    }
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_burst_on_one_pull_request_is_one_banner_and_every_item_in_the_tray() {
        let cfg = Notifications::default();
        let mut engine = engine();
        let banners = (0..20)
            .filter_map(|n| {
                engine
                    .process(
                        mention(n, 1_000 + n as i64, "rzorzal/clusia#123"),
                        &cfg,
                        at(600),
                    )
                    .notify
            })
            .count();
        assert_eq!(banners, 1);
        assert_eq!(engine.data.items.len(), 20);
        assert_eq!(engine.data.unseen(), 20);
    }

    #[test]
    fn grouping_is_per_pull_request_and_ends_after_two_minutes() {
        let cfg = Notifications::default();
        let mut engine = engine();
        let post = |engine: &mut Engine, n, secs, pr| {
            engine
                .process(mention(n, secs, pr), &cfg, at(600))
                .notify
                .is_some()
        };
        assert!(post(&mut engine, 1, 1_000, "rzorzal/clusia#1"));
        assert!(!post(&mut engine, 2, 1_060, "rzorzal/clusia#1"));
        assert!(
            post(&mut engine, 3, 1_061, "rzorzal/clusia#2"),
            "another PR"
        );
        assert!(
            post(&mut engine, 4, 1_000 + 121, "rzorzal/clusia#1"),
            "window over"
        );
    }

    #[test]
    fn without_grouping_every_event_is_a_banner() {
        let cfg = Notifications {
            group_bursts: false,
            ..Notifications::default()
        };
        let mut engine = engine();
        let banners = (0..3)
            .filter_map(|n| {
                engine
                    .process(
                        mention(n, 1_000 + n as i64, "rzorzal/clusia#1"),
                        &cfg,
                        at(600),
                    )
                    .notify
            })
            .count();
        assert_eq!(banners, 3);
    }

    #[test]
    fn do_not_disturb_silences_the_banner_but_not_the_tray() {
        let cfg = Notifications {
            dnd: Dnd {
                enabled: true,
                from: HourMinute::new(19, 0).unwrap(),
                to: HourMinute::new(9, 0).unwrap(),
                days: [Weekday::Wed].into_iter().collect(),
            },
            ..Notifications::default()
        };
        let mut engine = engine();
        let during = engine.process(mention(1, 1_000, "rzorzal/clusia#1"), &cfg, at(22 * 60));
        assert!(during.notify.is_none());
        assert_eq!(engine.data.items.len(), 1, "the tray list still gets it");
        let outside = engine.process(mention(2, 5_000, "rzorzal/clusia#2"), &cfg, at(12 * 60));
        assert!(outside.notify.is_some());
        let silenced = engine.process(mention(3, 9_000, "rzorzal/clusia#3"), &cfg, at(22 * 60));
        assert!(silenced.notify.is_none());
    }

    #[test]
    fn an_event_routed_to_the_tray_only_makes_no_banner() {
        let cfg = Notifications::default();
        let mut engine = engine();
        let e =
            NotifyEvent::checks_failed(&pr(), "feat: auth refresh", 2, "checks_failed:x:c1", 1_000);
        let processed = engine.process(e, &cfg, at(600));
        assert!(processed.notify.is_none());
        assert_eq!(engine.data.items.len(), 1);
        assert_eq!(engine.data.items[0].kind, EventKind::ChecksFailed);
    }

    #[test]
    fn an_event_switched_off_everywhere_is_remembered_but_shows_nowhere() {
        let mut cfg = Notifications::default();
        cfg.events.insert(
            EventKind::Mentioned,
            clusia_core::config::Route {
                tray: false,
                macos: false,
                sound: false,
            },
        );
        let mut engine = engine();
        let processed = engine.process(mention(1, 1_000, "rzorzal/clusia#1"), &cfg, at(600));
        assert!(processed.notify.is_none() && engine.data.items.is_empty());
        assert!(engine.data.has_key("comment:rzorzal/clusia#1:1"));
    }

    #[test]
    fn checks_turn_red_only_after_a_first_look() {
        use CheckState::*;
        assert!(!turned_red(None, "c1", Failing), "first look is a baseline");
        assert!(turned_red(Some("c1:pending"), "c1", Failing));
        assert!(turned_red(Some("c1:passing"), "c1", Failing));
        assert!(
            turned_red(Some("c1:failing"), "c2", Failing),
            "a new red commit"
        );
        assert!(!turned_red(Some("c1:failing"), "c1", Failing), "still red");
        assert!(!turned_red(Some("c1:failing"), "c1", Passing));
        assert!(!turned_red(Some("c1:passing"), "c2", Pending));
    }

    #[test]
    fn sync_problems_are_told_when_they_begin_and_offline_after_ten_minutes() {
        use SyncState::*;
        let mut watch = SyncWatch::default();
        assert_eq!(
            watch.observe(Online, Unauthorized, 0),
            Some(SyncProblem::Unauthorized)
        );
        assert_eq!(
            watch.observe(Unauthorized, Unauthorized, 30),
            None,
            "already told"
        );
        assert_eq!(watch.observe(Unauthorized, Online, 60), None);
        assert_eq!(
            watch.observe(Online, RateLimited, 100),
            Some(SyncProblem::RateLimited)
        );
        assert_eq!(watch.observe(RateLimited, RateLimited, 130), None);

        assert_eq!(
            watch.observe(Online, Offline, 1_000),
            None,
            "just went offline"
        );
        assert_eq!(
            watch.observe(Offline, Offline, 1_599),
            None,
            "not ten minutes yet"
        );
        assert_eq!(
            watch.observe(Offline, Offline, 1_600),
            Some(SyncProblem::Offline)
        );
        assert_eq!(watch.observe(Offline, Offline, 2_000), None, "told once");
        assert_eq!(watch.observe(Offline, Online, 2_100), None);
        assert_eq!(
            watch.observe(Online, Offline, 3_000),
            None,
            "a new outage starts the clock again"
        );
        assert_eq!(
            watch.observe(Offline, Offline, 3_600),
            Some(SyncProblem::Offline)
        );
    }

    #[test]
    fn a_sync_problem_is_announced_once_a_day() {
        let cfg = Notifications::default();
        let mut engine = engine();
        let now = 1_790_000_000;
        let first = engine.process(
            sync_problem_event(SyncProblem::Unauthorized, now),
            &cfg,
            at(600),
        );
        assert!(first.notify.is_some());
        let restart_later_that_day = engine.process(
            sync_problem_event(SyncProblem::Unauthorized, now + 60),
            &cfg,
            at(600),
        );
        assert!(restart_later_that_day.notify.is_none());
        let next_week = engine.process(
            sync_problem_event(SyncProblem::Unauthorized, now + 7 * 86_400),
            &cfg,
            at(600),
        );
        assert!(next_week.notify.is_some());
        let first_text = sync_problem_event(SyncProblem::Unauthorized, now);
        assert_eq!(first_text.open, OpenTarget::Config { page: "git".into() });
    }

    #[test]
    fn a_reset_file_is_announced_with_its_new_name() {
        let e = state_recovered_event("config.toml", "config.toml.corrupt-1790000000", 5);
        assert_eq!(e.key, "state_recovered:config.toml.corrupt-1790000000");
        assert_eq!(e.title, "config.toml was repaired");
        assert_eq!(e.open, OpenTarget::Home { pr: None });
    }

    #[test]
    fn an_outdated_review_counts_the_new_commits() {
        let review = Review::new(
            pr(),
            "fix: cache invalidation".into(),
            "b".into(),
            "h2".into(),
            1,
        );
        let e = commits_after_review_event(&review, 2, 9);
        assert_eq!(e.title, "#123 is out of date");
        assert_eq!(e.body, "2 new commits on fix: cache invalidation.");
        assert_eq!(e.key, "commits_after_review:rzorzal/clusia#123:h2");
        assert_eq!(
            commits_after_review_event(&review, 1, 9).body,
            "1 new commit on fix: cache invalidation."
        );
    }

    #[test]
    fn new_commits_are_counted_after_the_head_that_was_reviewed() {
        let commit = |sha: &str| CommitInfo {
            sha: sha.into(),
            author: "maria".into(),
            message: "m".into(),
            date: "2026-10-01T00:00:00Z".into(),
        };
        let all = [commit("a"), commit("b"), commit("c"), commit("d")];
        assert_eq!(commits_since(&all, "b"), 2);
        assert_eq!(commits_since(&all, "d"), 1, "never fewer than one");
        assert_eq!(commits_since(&all, "gone"), 1, "a rewritten history");
        assert_eq!(commits_since(&[], "a"), 1);
    }

    #[test]
    fn several_events_about_one_pull_request_make_one_summary() {
        let titles: Vec<String> = [
            "@octo mentioned you",
            "@octo replied to you",
            "@octo mentioned you",
        ]
        .map(String::from)
        .into();
        assert_eq!(
            group_banner(&titles, 123),
            (
                "3 updates on #123".to_string(),
                "@octo mentioned you · @octo replied to you".to_string()
            )
        );
        let many: Vec<String> = (0..6).map(|n| format!("t{n}")).collect();
        let (title, body) = group_banner(&many, 7);
        assert_eq!(title, "6 updates on #7");
        assert_eq!(body, "t0 · t1 · t2 · and 3 more");
    }

    #[test]
    fn the_focus_setting_decides_the_interruption_level() {
        let level = |follow_focus| {
            let cfg = Notifications {
                follow_focus,
                ..Notifications::default()
            };
            match engine()
                .process(mention(1, 1_000, "rzorzal/clusia#1"), &cfg, at(600))
                .notify
                .unwrap()
            {
                Event::Notify { time_sensitive, .. } => time_sensitive,
                other => panic!("{other:?}"),
            }
        };
        assert!(!level(true), "following Focus is the quiet level");
        assert!(level(false), "ignoring Focus breaks through it");
    }

    #[test]
    fn checks_are_asked_again_only_when_the_pull_request_changed_or_was_unfinished() {
        let now = "2026-10-01T12:00:00Z";
        assert!(needs_look(None, now), "never looked");
        assert!(
            needs_look(Some("c1:passing"), now),
            "an entry without a time"
        );
        let settled = format!("{now}|c1:passing");
        assert!(!needs_look(Some(&settled), now));
        assert!(
            needs_look(Some(&settled), "2026-10-01T12:05:00Z"),
            "it moved"
        );
        let running = format!("{now}|c1:pending");
        assert!(
            needs_look(Some(&running), now),
            "unfinished checks are watched"
        );
        assert_eq!(last_state(Some(&settled)), Some("c1:passing"));
        assert_eq!(last_state(Some("c1:passing")), None);
        assert_eq!(last_state(None), None);
    }
}
