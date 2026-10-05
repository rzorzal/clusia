//! Pull request lists shared by the tray and the window: one entry type for open PRs and saved
//! reviews, plus filtering, sorting, repository chips and pages (spec §7.2 Home, §7.3 tray).

use std::cmp::Reverse;

use clusia_core::time::parse_rfc3339;
use clusia_core::{ListSort, PrRef, PrSummary, ReviewState};
use clusia_protocol::ReviewSummary;

/// A list entry: an open pull request or a saved review, so both lists share one pipeline.
#[derive(Debug, Clone, Copy)]
pub enum Entry<'a> {
    Pr(&'a PrSummary),
    Review(&'a ReviewSummary),
}

impl<'a> Entry<'a> {
    pub fn pr(&self) -> &'a PrRef {
        match self {
            Entry::Pr(p) => &p.pr,
            Entry::Review(r) => &r.pr,
        }
    }

    pub fn title(&self) -> &'a str {
        match self {
            Entry::Pr(p) => &p.title,
            Entry::Review(r) => &r.title,
        }
    }

    /// Unix seconds; an unparsable GitHub timestamp counts as 0 (oldest).
    pub fn updated(&self) -> i64 {
        match self {
            Entry::Pr(p) => parse_rfc3339(&p.updated_at).unwrap_or(0),
            Entry::Review(r) => r.updated_at,
        }
    }
}

/// Reviews kept for later (they appear in the Saved lists).
pub fn is_saved(state: ReviewState) -> bool {
    matches!(
        state,
        ReviewState::Saved | ReviewState::Outdated | ReviewState::Revalidated
    )
}

/// Case-insensitive `query` against title, `owner/repo` and the whole `#number`; `repo`, when
/// not empty, must equal `owner/repo`.
pub fn matches(e: &Entry, query: &str, repo: &str) -> bool {
    let pr = e.pr();
    let full = pr.slug();
    if !repo.is_empty() && full != repo {
        return false;
    }
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    let number = q.strip_prefix('#').unwrap_or(&q);
    e.title().to_lowercase().contains(&q)
        || full.to_lowercase().contains(&q)
        || pr.number.to_string() == number
}

pub fn sort_entries(v: &mut [Entry], sort: ListSort) {
    match sort {
        ListSort::Updated => v.sort_by_key(|e| Reverse(e.updated())),
        ListSort::Oldest => v.sort_by_key(|e| e.updated()),
        ListSort::Repository => v.sort_by(|a, b| {
            a.pr()
                .slug()
                .cmp(&b.pr().slug())
                .then(b.updated().cmp(&a.updated()))
        }),
        ListSort::Number => v.sort_by_key(|e| Reverse(e.pr().number)),
    }
}

/// Filters, then sorts.
pub fn narrow<'a>(
    entries: impl IntoIterator<Item = Entry<'a>>,
    query: &str,
    repo: &str,
    sort: ListSort,
) -> Vec<Entry<'a>> {
    let mut v: Vec<Entry<'a>> = entries
        .into_iter()
        .filter(|e| matches(e, query, repo))
        .collect();
    sort_entries(&mut v, sort);
    v
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoChip {
    pub label: String,
    /// `None` is the "All" chip.
    pub repo: Option<String>,
    pub selected: bool,
}

/// "All" plus one chip per repository, A–Z by short name. No chips for fewer than two
/// repositories, unless one is selected (so the user can always clear it). A short name used
/// under two owners shows the full `owner/repo` for both.
pub fn repo_chips<'a>(prs: impl IntoIterator<Item = &'a PrRef>, selected: &str) -> Vec<RepoChip> {
    let mut repos: Vec<(String, String)> = prs
        .into_iter()
        .map(|pr| (pr.repo.clone(), pr.slug()))
        .collect();
    repos.sort();
    repos.dedup_by(|a, b| a.1 == b.1);
    if repos.len() < 2 && selected.is_empty() {
        return Vec::new();
    }
    let labels: Vec<String> = repos
        .iter()
        .map(|(short, full)| {
            if repos.iter().filter(|(s, _)| s == short).count() > 1 {
                full.clone()
            } else {
                short.clone()
            }
        })
        .collect();
    let mut chips = vec![RepoChip {
        label: "All".into(),
        repo: None,
        selected: selected.is_empty(),
    }];
    chips.extend(
        repos
            .into_iter()
            .zip(labels)
            .map(|((_, full), label)| RepoChip {
                selected: selected == full,
                repo: Some(full),
                label,
            }),
    );
    chips
}

/// At least one page, even when empty.
pub fn page_count(total: usize, size: usize) -> usize {
    total.div_ceil(size.max(1)).max(1)
}

pub fn clamp_page(page: usize, total: usize, size: usize) -> usize {
    page.min(page_count(total, size) - 1)
}

/// Moves `delta` pages from `page` (clamped first), staying within the first and last page.
pub fn step_page(page: usize, delta: isize, total: usize, size: usize) -> usize {
    clamp_page(
        clamp_page(page, total, size).saturating_add_signed(delta),
        total,
        size,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(slug: &str, number: u64, title: &str, updated: &str) -> PrSummary {
        let (owner, repo) = slug.split_once('/').unwrap();
        PrSummary {
            pr: PrRef::new(owner, repo, number).unwrap(),
            title: title.into(),
            author: "octo".into(),
            url: format!("https://github.com/{slug}/pull/{number}"),
            draft: false,
            updated_at: updated.into(),
            comments: 0,
        }
    }

    fn review(slug: &str, number: u64, state: ReviewState, updated_at: i64) -> ReviewSummary {
        let (owner, repo) = slug.split_once('/').unwrap();
        ReviewSummary {
            pr: PrRef::new(owner, repo, number).unwrap(),
            title: "fix: cache invalidation".into(),
            state,
            items: 2,
            updated_at,
        }
    }

    #[test]
    fn filter_matches_title_repository_and_whole_numbers() {
        let a = pr(
            "rzorzal/clusia",
            123,
            "feat: auth refresh",
            "2026-10-01T10:00:00Z",
        );
        let e = Entry::Pr(&a);
        assert!(matches(&e, "", ""));
        assert!(matches(&e, "AUTH", ""));
        assert!(matches(&e, " rzorzal/clu ", ""));
        assert!(matches(&e, "#123", ""));
        assert!(matches(&e, "123", ""));
        assert!(!matches(&e, "12", ""), "numbers match whole");
        assert!(matches(&e, "", "rzorzal/clusia"));
        assert!(!matches(&e, "", "rzorzal/site"));
        assert!(!matches(&e, "auth", "rzorzal/site"));
    }

    #[test]
    fn entries_expose_both_kinds() {
        let r = review("rzorzal/blog", 31, ReviewState::Saved, 5);
        let e = Entry::Review(&r);
        assert_eq!(e.pr().number, 31);
        assert_eq!(e.title(), "fix: cache invalidation");
        assert_eq!(e.updated(), 5);
        let p = pr("rzorzal/blog", 2, "x", "not a date");
        assert_eq!(
            Entry::Pr(&p).updated(),
            0,
            "unparsable dates sort as oldest"
        );
    }

    #[test]
    fn sorts_by_each_order() {
        let a = pr("rzorzal/site", 7, "a", "2026-10-01T10:00:00Z");
        let b = pr("rzorzal/blog", 9, "b", "2026-10-02T10:00:00Z");
        let c = pr("rzorzal/clusia", 3, "c", "2026-09-30T10:00:00Z");
        let order = |sort| {
            let mut v = vec![Entry::Pr(&a), Entry::Pr(&b), Entry::Pr(&c)];
            sort_entries(&mut v, sort);
            v.iter().map(|e| e.pr().number).collect::<Vec<_>>()
        };
        assert_eq!(order(ListSort::Updated), vec![9, 7, 3]);
        assert_eq!(order(ListSort::Oldest), vec![3, 7, 9]);
        assert_eq!(order(ListSort::Repository), vec![9, 3, 7]);
        assert_eq!(order(ListSort::Number), vec![9, 7, 3]);
        let narrowed = narrow(
            [Entry::Pr(&a), Entry::Pr(&b), Entry::Pr(&c)],
            "",
            "rzorzal/blog",
            ListSort::Updated,
        );
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].pr().number, 9);
    }

    #[test]
    fn saved_states() {
        assert!(is_saved(ReviewState::Saved));
        assert!(is_saved(ReviewState::Outdated));
        assert!(is_saved(ReviewState::Revalidated));
        assert!(!is_saved(ReviewState::Active));
        assert!(!is_saved(ReviewState::Published));
        assert!(!is_saved(ReviewState::Discarded));
        assert!(!is_saved(ReviewState::Publishing));
    }

    #[test]
    fn chips_need_two_repositories_and_disambiguate_short_names() {
        let p = |s: &str| s.parse::<PrRef>().unwrap();
        let refs = [
            p("rzorzal/site#1"),
            p("rzorzal/blog#2"),
            p("rzorzal/site#3"),
        ];
        let chips = repo_chips(&refs, "");
        let labels: Vec<&str> = chips.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, ["All", "blog", "site"]);
        assert!(chips[0].selected);
        assert_eq!(chips[0].repo, None);
        assert_eq!(chips[2].repo.as_deref(), Some("rzorzal/site"));
        assert!(
            repo_chips(&refs[..1], "").is_empty(),
            "one repository: no chips"
        );
        let kept = repo_chips(&refs[..1], "rzorzal/site");
        assert_eq!(kept.len(), 2, "a selected repository keeps its chip");
        assert!(kept[1].selected && !kept[0].selected);
        let twins = [p("octo/site#1"), p("acme/site#2")];
        let labels: Vec<String> = repo_chips(&twins, "")
            .into_iter()
            .map(|c| c.label)
            .collect();
        assert_eq!(labels, ["All", "acme/site", "octo/site"]);
    }

    #[test]
    fn pages_are_clamped() {
        assert_eq!(page_count(0, 4), 1);
        assert_eq!(page_count(4, 4), 1);
        assert_eq!(page_count(5, 4), 2);
        assert_eq!(page_count(3, 0), 3, "a zero page size counts as one");
        assert_eq!(clamp_page(5, 5, 4), 1);
        assert_eq!(clamp_page(0, 0, 4), 0);
        assert_eq!(step_page(0, -1, 9, 4), 0);
        assert_eq!(step_page(0, 1, 9, 4), 1);
        assert_eq!(step_page(0, 5, 9, 4), 2);
        assert_eq!(
            step_page(7, -1, 9, 4),
            1,
            "an out-of-range page is clamped first"
        );
    }
}
