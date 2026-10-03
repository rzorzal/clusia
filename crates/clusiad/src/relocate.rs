//! Re-anchoring a stored draft when the pull request moved (spec §6.6; deterministic in SP1).

use std::path::Path;

use clusia_core::{DiffMap, ItemStatus, PrDetail, Relocation, Review, relocate};
use clusia_git::{diff_between, merge_base};

pub(crate) async fn relocate_review(
    review: &mut Review,
    detail: &PrDetail,
    repo: &Path,
) -> Relocation {
    let head_map = match diff_between(repo, &review.head_sha, &detail.head_sha).await {
        Ok(diff) => Some(DiffMap::parse(&diff)),
        Err(e) => {
            tracing::warn!(error = %e, pr = %review.pr, "cannot diff the previously reviewed head");
            None
        }
    };
    let old_mb = merge_base(repo, &review.base_sha, &review.head_sha)
        .await
        .ok();
    let new_mb = merge_base(repo, &detail.base_sha, &detail.head_sha)
        .await
        .ok();
    let base_map = match (old_mb, new_mb) {
        (Some(a), Some(b)) if a == b => Some(DiffMap::default()),
        (Some(a), Some(b)) => diff_between(repo, &a, &b)
            .await
            .ok()
            .map(|d| DiffMap::parse(&d)),
        _ => None,
    };
    let report = match &head_map {
        Some(map) => relocate(
            &mut review.draft,
            map,
            base_map.as_ref(),
            &detail.head_sha,
            &detail.base_sha,
        ),
        None => obsolete_all(
            review,
            "the previously reviewed commit is no longer available",
        ),
    };
    review.head_sha = detail.head_sha.clone();
    review.base_sha = detail.base_sha.clone();
    report
}

fn obsolete_all(review: &mut Review, reason: &str) -> Relocation {
    let mut report = Relocation::default();
    for item in &mut review.draft.items {
        if item.anchor.is_some() && !matches!(item.status, ItemStatus::Obsolete { .. }) {
            item.status = ItemStatus::Obsolete {
                reason: reason.to_string(),
            };
            report.obsolete += 1;
        }
    }
    report
}
