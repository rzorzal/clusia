//! Demo data for `--demo` and screenshots: only `rzorzal` repositories and generic people.

use clusia_core::time::civil_from_days;
use clusia_core::{ActivitySummary, Config, DayCount, PrRef, PrSummary, ReviewState};
use clusia_protocol::{AuthInfo, ReviewSummary, SyncState, SyncStatus, TokenSource};

use crate::snapshot::Snapshot;

pub fn demo(now: i64) -> Snapshot {
    let pr =
        |repo: &str, number: u64, title: &str, author: &str, age: i64, draft: bool| PrSummary {
            pr: PrRef::new("rzorzal", repo, number).expect("valid demo ref"),
            title: title.into(),
            author: author.into(),
            url: format!("https://github.com/rzorzal/{repo}/pull/{number}"),
            draft,
            updated_at: rfc3339(now - age),
            comments: 2,
        };
    let review = |repo: &str, number: u64, title: &str, state, items, age: i64| ReviewSummary {
        pr: PrRef::new("rzorzal", repo, number).expect("valid demo ref"),
        title: title.into(),
        state,
        items,
        updated_at: now - age,
    };
    let day = 86_400;
    Snapshot {
        config: Config::default(),
        assigned: vec![
            pr("clusia", 123, "feat: auth refresh", "octo", 300, false),
            pr(
                "clusia",
                98,
                "api pagination for long pull request lists",
                "mona",
                day,
                false,
            ),
            pr("blog", 36, "Write the M3 post", "hubot", 3 * day, true),
            pr("site", 12, "Dark mode for the docs", "octo", 4 * day, false),
            pr(
                "clusia",
                61,
                "fix: sync backoff on rate limits",
                "mona",
                5 * day,
                false,
            ),
            pr("site", 14, "Fix the footer links", "hubot", 6 * day, false),
            pr(
                "clusia",
                57,
                "docs: config reference",
                "octo",
                8 * day,
                false,
            ),
        ],
        mine: vec![
            pr(
                "clusia",
                140,
                "feat(tray): menu bar popover",
                "rzorzal",
                600,
                false,
            ),
            pr(
                "clusia",
                131,
                "fix(git): never prune the user clone",
                "rzorzal",
                2 * day,
                false,
            ),
            pr(
                "blog",
                40,
                "New post: the autograph tree",
                "rzorzal",
                5 * day,
                true,
            ),
        ],
        reviews: vec![
            review(
                "clusia",
                77,
                "fix: cache invalidation",
                ReviewState::Outdated,
                2,
                3 * 3600,
            ),
            review("blog", 31, "New theme", ReviewState::Saved, 1, day),
        ],
        activity: Some(ActivitySummary {
            heatmap: heat(now),
            published_this_week: 12,
            published_total: 140,
            avg_review_secs: Some(45 * 60),
        }),
        sync: Some(SyncStatus {
            state: SyncState::Online,
            last_sync_unix: Some(now - 60),
            next_sync_unix: Some(now),
            ..SyncStatus::default()
        }),
        auth: Some(AuthInfo {
            source: Some(TokenSource::GhCli),
            login: Some("rzorzal".into()),
            scopes: vec!["repo".into(), "read:org".into()],
            error: None,
        }),
        lists_loaded: true,
        daemon_version: "demo".into(),
    }
}

fn rfc3339(unix: i64) -> String {
    let (y, m, d) = civil_from_days(unix.div_euclid(86_400));
    let s = unix.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// 182 days ending today: busy weekdays, quiet weekends.
fn heat(now: i64) -> Vec<DayCount> {
    let today = now.div_euclid(86_400);
    (0..182)
        .map(|i| {
            let day = today - 181 + i;
            let (y, m, d) = civil_from_days(day);
            let weekday = (day + 4).rem_euclid(7); // 1970-01-01 was a Thursday; 0 = Sunday
            let count = if weekday == 0 || weekday == 6 {
                0
            } else {
                (day * 7919).rem_euclid(5) as u32
            };
            DayCount {
                date: format!("{y:04}-{m:02}-{d:02}"),
                count,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_uses_only_generic_data() {
        let s = demo(1_790_000_000);
        let owners = s
            .assigned
            .iter()
            .chain(&s.mine)
            .map(|p| &p.pr)
            .chain(s.reviews.iter().map(|r| &r.pr));
        for pr in owners {
            assert_eq!(pr.owner, "rzorzal");
        }
        for p in s.assigned.iter().chain(&s.mine) {
            assert!(["octo", "mona", "hubot", "rzorzal"].contains(&p.author.as_str()));
        }
        assert_eq!(s.activity.as_ref().unwrap().heatmap.len(), 182);
        assert!(s.lists_loaded);
        assert_eq!(s.config, Config::default());
    }
}
