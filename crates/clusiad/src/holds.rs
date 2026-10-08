//! Which reviews a connected client has open, so the daemon knows when no window shows one.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use clusia_core::PrRef;

#[derive(Default)]
pub(crate) struct Holds {
    by_review: Mutex<HashMap<PrRef, HashSet<u64>>>,
    next_holder: AtomicU64,
}

impl Holds {
    /// A number for a new connection, to tell its reviews from another one's.
    pub(crate) fn new_holder(&self) -> u64 {
        self.next_holder.fetch_add(1, Ordering::SeqCst)
    }

    /// `holder` opened (`true`) or closed (`false`) `pr`.
    pub(crate) fn set(&self, pr: &PrRef, holder: u64, held: bool) {
        let mut reviews = self.by_review.lock().unwrap_or_else(|p| p.into_inner());
        if held {
            reviews.entry(pr.clone()).or_default().insert(holder);
        } else if let Some(holders) = reviews.get_mut(pr) {
            holders.remove(&holder);
            if holders.is_empty() {
                reviews.remove(pr);
            }
        }
    }

    /// The connection `holder` went away: everything it had open is let go.
    pub(crate) fn release_all(&self, holder: u64) {
        let mut reviews = self.by_review.lock().unwrap_or_else(|p| p.into_inner());
        reviews.retain(|_, holders| {
            holders.remove(&holder);
            !holders.is_empty()
        });
    }

    pub(crate) fn is_held(&self, pr: &PrRef) -> bool {
        self.by_review
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(pr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(n: u64) -> PrRef {
        format!("acme/widgets#{n}").parse().unwrap()
    }

    #[test]
    fn a_review_is_held_until_every_holder_lets_go() {
        let holds = Holds::default();
        let (window, other) = (holds.new_holder(), holds.new_holder());
        assert_ne!(window, other);
        assert!(!holds.is_held(&pr(7)));
        holds.set(&pr(7), window, true);
        holds.set(&pr(7), other, true);
        holds.set(&pr(7), window, false);
        assert!(holds.is_held(&pr(7)), "the other one still has it");
        holds.set(&pr(7), other, false);
        assert!(!holds.is_held(&pr(7)));
    }

    #[test]
    fn a_connection_that_goes_away_releases_all_its_reviews() {
        let holds = Holds::default();
        let window = holds.new_holder();
        holds.set(&pr(7), window, true);
        holds.set(&pr(8), window, true);
        holds.release_all(window);
        assert!(!holds.is_held(&pr(7)) && !holds.is_held(&pr(8)));
        holds.set(&pr(9), window, false);
        assert!(
            !holds.is_held(&pr(9)),
            "closing what was never open is harmless"
        );
    }
}
