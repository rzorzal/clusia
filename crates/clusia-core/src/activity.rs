//! What the user did, for the heatmap and stats (spec §7, §4.1 `activity.jsonl`).

use serde::{Deserialize, Serialize};

use crate::PrRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    ReviewOpened,
    ItemAdded,
    ReviewSaved,
    ReviewPublished,
    ReviewDiscarded,
    ReviewOutdated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activity {
    pub ts: i64,
    pub kind: ActivityKind,
    pub pr: PrRef,
    /// Which client caused it (`clusia`, `clusia-app`, …); empty for daemon-initiated events.
    #[serde(default)]
    pub client: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Short human detail, e.g. `2 moved, 1 obsolete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DayCount {
    /// Local date, `YYYY-MM-DD`.
    pub date: String,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivitySummary {
    /// Oldest first, one entry per day, zero-filled.
    pub heatmap: Vec<DayCount>,
    pub published_this_week: u32,
    pub published_total: u32,
    /// Mean time from first opening a PR to publishing its review.
    pub avg_review_secs: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_wire_format_and_defaults() {
        let a = Activity {
            ts: 5,
            kind: ActivityKind::ReviewPublished,
            pr: "acme/widgets#7".parse().unwrap(),
            client: "clusia".into(),
            url: Some("https://github.com/acme/widgets/pull/7#pullrequestreview-1".into()),
            note: None,
        };
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            r#"{"ts":5,"kind":"review_published","pr":"acme/widgets#7","client":"clusia","url":"https://github.com/acme/widgets/pull/7#pullrequestreview-1"}"#
        );
        let old: Activity =
            serde_json::from_str(r#"{"ts":1,"kind":"item_added","pr":"acme/widgets#7"}"#).unwrap();
        assert_eq!((old.client.as_str(), old.url, old.note), ("", None, None));
    }
}
