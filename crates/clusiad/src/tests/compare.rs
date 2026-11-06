//! What the cockpit has measured, read back from the review's record.

use clusia_core::cockpit::{
    CompareRecord, ComparisonView, Freshness, ItemRef, ItemResults, Metric, SideView,
    comparison_view, freshness, side_view,
};
use clusia_core::tests::{RunnerKind, TestsRecord};

/// The newest results of `item`: the last run of this pull request alone and the last comparison.
pub(super) fn results_of(record: &TestsRecord, item: &ItemRef) -> ItemResults {
    record
        .cockpit_results
        .get(&item.key())
        .cloned()
        .unwrap_or_default()
}

/// What an item's results are held against to tell whether they are still about what is under
/// review.
pub(super) struct Now<'a> {
    pub head: &'a str,
    pub base: Option<&'a str>,
    pub runner: RunnerKind,
}

/// The last single run and the last comparison of `item`, shown as `metrics` say.
pub(super) fn shown(
    record: &TestsRecord,
    item: &ItemRef,
    input_hash: &str,
    now: &Now<'_>,
    metrics: &[Metric],
    scenario: bool,
) -> (Option<SideView>, Option<ComparisonView>) {
    let results = results_of(record, item);
    let fresh = |side| freshness(side, now.head, now.base, input_hash, now.runner);
    let last = results
        .single
        .as_ref()
        .map(|side| side_view(side, fresh(side)));
    let comparison = results.compare.as_ref().map(|c: &CompareRecord| {
        let sides: (Freshness, Freshness) = (fresh(&c.pr), fresh(&c.main));
        comparison_view(c, metrics, sides, scenario)
    });
    (last, comparison)
}
