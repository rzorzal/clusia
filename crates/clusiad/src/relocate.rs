//! Re-anchoring a stored draft when the pull request moved (spec §6.6; deterministic in SP1).

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use clusia_core::{
    Anchor, DiffMap, FileDiff, ItemStatus, PrDetail, PrRef, Relocation, Review, Side,
    commentable_lines, relocate,
};
use clusia_git::{base_pin_ref, diff_between, fetch_branch, merge_base, pin_commit};

const OUTSIDE_DIFF: &str = "no longer part of the pull request's diff";
const BASE_UNAVAILABLE: &str = "cannot fetch the base branch";

/// Fetches the PR's base branch and pins its tip, so merge bases (left-side line numbers) can
/// be computed. Failures are logged: relocation then marks left-side comments obsolete.
pub(crate) async fn refresh_base(repo: &Path, remote: &str, pr: &PrRef, base_ref: &str) {
    match fetch_branch(repo, remote, base_ref).await {
        Ok(sha) => {
            if let Err(e) = pin_commit(repo, &base_pin_ref(pr.number), &sha).await {
                tracing::warn!(error = %e, pr = %pr, "cannot pin the base commit");
            }
        }
        Err(e) => tracing::warn!(error = %e, pr = %pr, "cannot fetch the base branch"),
    }
}

/// Moves the draft from the review's head/base to `detail`'s. With `files` (the PR's files at
/// the new head), anchored items GitHub would reject are marked obsolete too.
pub(crate) async fn relocate_review(
    review: &mut Review,
    detail: &PrDetail,
    repo: &Path,
    files: Option<&[FileDiff]>,
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
    let mut report = Relocation::default();
    if new_mb.is_none() && head_map.is_some() {
        report.obsolete += obsolete_left(review, BASE_UNAVAILABLE);
    }
    let base_map = match (old_mb, new_mb) {
        (Some(a), Some(b)) if a == b => Some(DiffMap::default()),
        (Some(a), Some(b)) => diff_between(repo, &a, &b)
            .await
            .ok()
            .map(|d| DiffMap::parse(&d)),
        _ => None,
    };
    let before: HashMap<String, Anchor> = review
        .draft
        .items
        .iter()
        .filter_map(|i| Some((i.id.clone(), i.anchor.clone()?)))
        .collect();
    let pass = match &head_map {
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
    report.moved += pass.moved;
    report.obsolete += pass.obsolete;
    review.head_sha = detail.head_sha.clone();
    review.base_sha = detail.base_sha.clone();
    if let Some(files) = files {
        let lost = obsolete_outside_diff(review, files);
        for id in &lost {
            let moved_now = review
                .draft
                .get(id)
                .zip(before.get(id))
                .is_some_and(|(item, old)| {
                    item.anchor.as_ref().is_some_and(|new| {
                        (&new.path, new.line, new.start_line)
                            != (&old.path, old.line, old.start_line)
                    })
                });
            if moved_now {
                report.moved = report.moved.saturating_sub(1);
            }
        }
        report.obsolete += lost.len();
    }
    report
}

/// Whether every line of `anchor` can carry a review comment in `files` (GitHub's patches).
pub(crate) fn fits_diff(files: &[FileDiff], anchor: &Anchor) -> bool {
    let Some(patch) = files
        .iter()
        .find(|f| f.path == anchor.path)
        .and_then(|f| f.patch.as_deref())
    else {
        return false;
    };
    let (left, right) = commentable_lines(patch);
    let lines: &BTreeSet<u32> = match anchor.side {
        Side::Left => &left,
        Side::Right => &right,
    };
    let start = anchor.start_line.unwrap_or(anchor.line);
    if start > anchor.line {
        return false;
    }
    // A range longer than the commentable lines cannot fit; this also bounds the loop.
    let span = u64::from(anchor.line - start) + 1;
    span <= lines.len() as u64 && (start..=anchor.line).all(|l| lines.contains(&l))
}

/// Marks live anchored items outside the diff obsolete; returns their ids.
fn obsolete_outside_diff(review: &mut Review, files: &[FileDiff]) -> Vec<String> {
    let mut lost = Vec::new();
    for item in &mut review.draft.items {
        if matches!(item.status, ItemStatus::Obsolete { .. }) {
            continue;
        }
        let Some(anchor) = &item.anchor else { continue };
        if !fits_diff(files, anchor) {
            item.status = ItemStatus::Obsolete {
                reason: OUTSIDE_DIFF.to_string(),
            };
            lost.push(item.id.clone());
        }
    }
    lost
}

fn obsolete_left(review: &mut Review, reason: &str) -> usize {
    let mut count = 0;
    for item in &mut review.draft.items {
        if item.anchor.as_ref().is_some_and(|a| a.side == Side::Left)
            && !matches!(item.status, ItemStatus::Obsolete { .. })
        {
            item.status = ItemStatus::Obsolete {
                reason: reason.to_string(),
            };
            count += 1;
        }
    }
    count
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
