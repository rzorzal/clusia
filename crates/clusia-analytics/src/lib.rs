//! Activity heatmap and review statistics. Pure functions over `activity.jsonl` entries.

use std::collections::HashMap;

use clusia_core::time::{day_number, format_day};
use clusia_core::{Activity, ActivityKind, ActivitySummary, DayCount, PrRef};

pub const HEATMAP_WEEKS: u32 = 16;

pub fn heatmap(activities: &[Activity], now: i64, offset_secs: i64, weeks: u32) -> Vec<DayCount> {
    let today = day_number(now, offset_secs);
    let days = i64::from(weeks) * 7;
    let first = today - days + 1;
    let mut counts: HashMap<i64, u32> = HashMap::new();
    for a in activities
        .iter()
        .filter(|a| a.kind == ActivityKind::ReviewPublished)
    {
        let day = day_number(a.ts, offset_secs);
        if (first..=today).contains(&day) {
            *counts.entry(day).or_default() += 1;
        }
    }
    (first..=today)
        .map(|day| DayCount {
            date: format_day(day),
            count: counts.get(&day).copied().unwrap_or(0),
        })
        .collect()
}

pub fn summary(activities: &[Activity], now: i64, offset_secs: i64) -> ActivitySummary {
    let today = day_number(now, offset_secs);
    let mut first_open: HashMap<&PrRef, i64> = HashMap::new();
    let mut durations: Vec<u64> = Vec::new();
    let (mut total, mut this_week) = (0u32, 0u32);
    let mut ordered: Vec<&Activity> = activities.iter().collect();
    ordered.sort_by_key(|a| a.ts);
    for a in ordered {
        match a.kind {
            ActivityKind::ReviewOpened => {
                first_open.entry(&a.pr).or_insert(a.ts);
            }
            ActivityKind::ReviewPublished => {
                total += 1;
                if today - day_number(a.ts, offset_secs) < 7 {
                    this_week += 1;
                }
                if let Some(opened) = first_open.remove(&a.pr)
                    && a.ts >= opened
                {
                    durations.push((a.ts - opened) as u64);
                }
            }
            _ => {}
        }
    }
    let avg_review_secs =
        (!durations.is_empty()).then(|| durations.iter().sum::<u64>() / durations.len() as u64);
    ActivitySummary {
        heatmap: heatmap(activities, now, offset_secs, HEATMAP_WEEKS),
        published_this_week: this_week,
        published_total: total,
        avg_review_secs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;
    // 2000-01-10T12:00:00Z
    const NOW: i64 = 946_684_800 + 9 * DAY + 12 * 3600;

    fn act(ts: i64, kind: ActivityKind, pr: &str) -> Activity {
        Activity {
            ts,
            kind,
            pr: pr.parse().unwrap(),
            client: String::new(),
            url: None,
            note: None,
        }
    }

    #[test]
    fn heatmap_counts_published_per_local_day() {
        let acts = vec![
            act(NOW, ActivityKind::ReviewPublished, "a/b#1"),
            act(NOW - 3600, ActivityKind::ReviewPublished, "a/b#2"),
            act(NOW - DAY, ActivityKind::ReviewPublished, "a/b#3"),
            act(NOW - DAY, ActivityKind::ItemAdded, "a/b#3"),
            act(NOW - 30 * DAY, ActivityKind::ReviewPublished, "a/b#4"),
        ];
        let map = heatmap(&acts, NOW, 0, 2);
        assert_eq!(map.len(), 14);
        assert_eq!(
            map.last().unwrap(),
            &DayCount {
                date: "2000-01-10".into(),
                count: 2
            }
        );
        assert_eq!(
            map[12],
            DayCount {
                date: "2000-01-09".into(),
                count: 1
            }
        );
        assert_eq!(map[0].date, "1999-12-28");
        assert_eq!(
            map.iter().map(|d| d.count).sum::<u32>(),
            3,
            "old events fall outside the window"
        );
    }

    #[test]
    fn heatmap_uses_the_local_offset() {
        let acts = vec![act(NOW - 13 * 3600, ActivityKind::ReviewPublished, "a/b#1")]; // 2000-01-09T23:00Z
        let utc = heatmap(&acts, NOW, 0, 1);
        assert_eq!(
            utc.iter().find(|d| d.count == 1).unwrap().date,
            "2000-01-09"
        );
        let tokyo = heatmap(&acts, NOW, 9 * 3600, 1);
        assert_eq!(
            tokyo.iter().find(|d| d.count == 1).unwrap().date,
            "2000-01-10"
        );
    }

    #[test]
    fn summary_stats() {
        let acts = vec![
            act(NOW - 10 * DAY, ActivityKind::ReviewOpened, "a/b#1"),
            act(NOW - 10 * DAY + 600, ActivityKind::ReviewPublished, "a/b#1"),
            act(NOW - 2 * DAY, ActivityKind::ReviewOpened, "a/b#2"),
            act(NOW - DAY, ActivityKind::ReviewOpened, "a/b#2"),
            act(NOW - 2 * DAY + 1800, ActivityKind::ReviewPublished, "a/b#2"),
            act(NOW, ActivityKind::ReviewPublished, "a/b#9"),
        ];
        let s = summary(&acts, NOW, 0);
        assert_eq!(s.heatmap.len(), (HEATMAP_WEEKS * 7) as usize);
        assert_eq!(s.published_total, 3);
        assert_eq!(s.published_this_week, 2);
        assert_eq!(s.avg_review_secs, Some((600 + 1800) / 2));
        assert_eq!(summary(&[], NOW, 0).avg_review_secs, None);
    }
}
