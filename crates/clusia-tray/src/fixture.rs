//! Demo data for `--render` (generic and `rzorzal/*` only).

use clusia_core::config::EventKind;
use clusia_core::{ActivitySummary, DayCount, Lists, PrRef, PrSummary, ReviewState};
use clusia_protocol::message::InboxItem;
use clusia_protocol::{ReviewSummary, SyncState, SyncStatus};

use crate::model::Snapshot;

fn rfc3339(unix: i64) -> String {
    let (y, m, d) = clusia_core::time::civil_from_days(unix.div_euclid(86_400));
    let s = unix.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

fn pr(repo: &str, n: u64, title: &str, updated: i64, draft: bool) -> PrSummary {
    PrSummary {
        pr: PrRef {
            owner: "rzorzal".into(),
            repo: repo.into(),
            number: n,
        },
        title: title.into(),
        author: "octo".into(),
        url: format!("https://github.com/rzorzal/{repo}/pull/{n}"),
        draft,
        updated_at: rfc3339(updated),
        comments: 2,
    }
}

/// The demo state: `#123` has a new-activity dot (an unseen inbox item).
pub fn demo(now: i64) -> Snapshot {
    Snapshot {
        assigned: vec![
            pr("clusia", 123, "feat: auth refresh", now - 300, false),
            pr(
                "clusia",
                98,
                "api pagination for long pull request lists",
                now - 86_400,
                false,
            ),
            pr("blog", 36, "Write the M3 post", now - 3 * 86_400, true),
            pr(
                "site",
                12,
                "Dark mode for the docs",
                now - 4 * 86_400,
                false,
            ),
            pr(
                "clusia",
                91,
                "refactor: split the protocol codec",
                now - 6 * 86_400,
                false,
            ),
            pr("blog", 30, "Fix broken RSS links", now - 9 * 86_400, false),
        ],
        mine: vec![pr(
            "clusia",
            140,
            "feat(tray): menu bar popover",
            now - 600,
            false,
        )],
        reviews: vec![
            ReviewSummary {
                pr: PrRef {
                    owner: "rzorzal".into(),
                    repo: "clusia".into(),
                    number: 77,
                },
                title: "fix: cache invalidation".into(),
                state: ReviewState::Outdated,
                items: 2,
                updated_at: now - 3 * 3600,
            },
            ReviewSummary {
                pr: PrRef {
                    owner: "rzorzal".into(),
                    repo: "blog".into(),
                    number: 31,
                },
                title: "New theme".into(),
                state: ReviewState::Saved,
                items: 1,
                updated_at: now - 86_400,
            },
        ],
        activity: Some(ActivitySummary {
            heatmap: (0..182)
                .map(|i: u32| DayCount {
                    date: String::new(),
                    count: (i * 7 + i / 5) % 6 * u32::from(i % 7 < 5),
                })
                .collect(),
            published_this_week: 12,
            published_total: 140,
            avg_review_secs: Some(2700),
        }),
        sync: Some(SyncStatus {
            state: SyncState::Online,
            last_sync_unix: Some(now - 60),
            next_sync_unix: Some(now),
            message: None,
            paused: false,
        }),
        host: "github.com".into(),
        lists_loaded: true,
        lists: Lists {
            filter: String::new(),
            ..Lists::default()
        },
        inbox: vec![InboxItem {
            id: "demo-1".into(),
            kind: EventKind::ReviewRequested,
            pr: Some(PrRef {
                owner: "rzorzal".into(),
                repo: "clusia".into(),
                number: 123,
            }),
            title: "Review requested".into(),
            body: "@octo asked for your review".into(),
            at: now - 300,
            seen: false,
        }],
    }
}
