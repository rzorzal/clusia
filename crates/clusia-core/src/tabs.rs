//! The review tabs the window had open, kept by the daemon so they come back with the window.

use serde::{Deserialize, Serialize};

use crate::PrRef;

/// A tab nobody has looked at for this long, with nothing in its draft, is closed when the
/// window starts.
pub const TAB_IDLE_DAYS: i64 = 14;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenTab {
    pub pr: PrRef,
    /// The section the tab shows, by the window's id for it (`diff`, `comments`, …); empty or
    /// unknown: the window's default.
    #[serde(default)]
    pub section: String,
    /// When the tab was last the one shown (Unix seconds). Kept by the daemon: what a window
    /// sends here is ignored.
    #[serde(default)]
    pub seen_at: i64,
}

/// The tabs left to right, and the one shown (`None`: Home or Config was showing).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OpenTabs {
    #[serde(default)]
    pub tabs: Vec<OpenTab>,
    #[serde(default)]
    pub selected: Option<PrRef>,
}

impl OpenTabs {
    pub fn tab(pr: PrRef, section: impl Into<String>) -> OpenTab {
        OpenTab {
            pr,
            section: section.into(),
            seen_at: 0,
        }
    }

    /// The window's new set, keeping when each tab was last seen: a new tab, and the one shown,
    /// are seen `now`. A pull request listed twice keeps its first place.
    pub fn replaced_by(&self, next: OpenTabs, now: i64) -> OpenTabs {
        let mut tabs: Vec<OpenTab> = Vec::with_capacity(next.tabs.len());
        for mut tab in next.tabs {
            if tabs.iter().any(|t| t.pr == tab.pr) {
                continue;
            }
            let shown = next.selected.as_ref() == Some(&tab.pr);
            tab.seen_at = match self.tabs.iter().find(|t| t.pr == tab.pr) {
                Some(old) if !shown => old.seen_at,
                _ => now,
            };
            tabs.push(tab);
        }
        let selected = next.selected.filter(|pr| tabs.iter().any(|t| &t.pr == pr));
        OpenTabs { tabs, selected }
    }

    /// The tabs not seen for `TAB_IDLE_DAYS`, left to right.
    pub fn idle(&self, now: i64) -> Vec<PrRef> {
        let cutoff = now - TAB_IDLE_DAYS * 86_400;
        self.tabs
            .iter()
            .filter(|t| t.seen_at < cutoff)
            .map(|t| t.pr.clone())
            .collect()
    }

    /// Drops the tabs of `prs`; the one shown goes too if it was among them.
    pub fn without(mut self, prs: &[PrRef]) -> OpenTabs {
        self.tabs.retain(|t| !prs.contains(&t.pr));
        if self.selected.as_ref().is_some_and(|pr| prs.contains(pr)) {
            self.selected = None;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(n: u64) -> PrRef {
        format!("acme/widgets#{n}").parse().unwrap()
    }

    fn seen(tabs: &OpenTabs) -> Vec<(u64, i64)> {
        tabs.tabs.iter().map(|t| (t.pr.number, t.seen_at)).collect()
    }

    #[test]
    fn a_new_set_keeps_when_each_tab_was_last_seen() {
        let old = OpenTabs {
            tabs: vec![
                OpenTab {
                    seen_at: 10,
                    ..OpenTabs::tab(pr(1), "diff")
                },
                OpenTab {
                    seen_at: 20,
                    ..OpenTabs::tab(pr(2), "diff")
                },
            ],
            selected: Some(pr(1)),
        };
        let next = OpenTabs {
            tabs: vec![
                OpenTabs::tab(pr(2), "comments"),
                OpenTabs::tab(pr(3), "diff"),
                OpenTabs::tab(pr(1), "security"),
                OpenTabs::tab(pr(2), "diff"),
            ],
            selected: Some(pr(1)),
        };
        let merged = old.replaced_by(next, 99);
        assert_eq!(
            seen(&merged),
            [(2, 20), (3, 99), (1, 99)],
            "kept, new, shown; the repeat is dropped"
        );
        assert_eq!(merged.tabs[0].section, "comments");
        assert_eq!(merged.selected, Some(pr(1)));
    }

    #[test]
    fn a_selection_outside_the_tabs_is_none() {
        let next = OpenTabs {
            tabs: vec![OpenTabs::tab(pr(1), "diff")],
            selected: Some(pr(9)),
        };
        assert_eq!(OpenTabs::default().replaced_by(next, 5).selected, None);
    }

    #[test]
    fn idle_tabs_are_the_ones_not_seen_for_fourteen_days() {
        let day = 86_400;
        let now = 100 * day;
        let tabs = OpenTabs {
            tabs: vec![
                OpenTab {
                    seen_at: now - 15 * day,
                    ..OpenTabs::tab(pr(1), "")
                },
                OpenTab {
                    seen_at: now - 13 * day,
                    ..OpenTabs::tab(pr(2), "")
                },
                OpenTab {
                    seen_at: now - 14 * day,
                    ..OpenTabs::tab(pr(3), "")
                },
            ],
            selected: Some(pr(1)),
        };
        assert_eq!(tabs.idle(now), [pr(1)]);
        let left = tabs.without(&[pr(1)]);
        assert_eq!(seen(&left).len(), 2);
        assert_eq!(left.selected, None, "the shown tab went");
    }

    #[test]
    fn the_file_reads_with_missing_fields() {
        let tabs: OpenTabs = serde_json::from_str(r#"{"tabs":[{"pr":"acme/widgets#7"}]}"#).unwrap();
        assert_eq!(tabs.tabs, [OpenTabs::tab(pr(7), "")]);
        assert_eq!(tabs.selected, None);
    }
}
