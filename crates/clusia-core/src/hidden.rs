//! The comments a reviewer hid, per pull request. Kept on this machine only: nothing here is
//! ever sent to GitHub.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The comments hidden in one pull request, by key (see the `*_key` functions), each with how
/// many posts it held when it was hidden.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HiddenComments {
    #[serde(default)]
    pub hidden: BTreeMap<String, u32>,
}

/// A comment in the general discussion. Ids of issue comments and of reviews are numbered
/// apart on GitHub, so the kind is part of the key.
pub fn issue_comment_key(id: u64) -> String {
    format!("comment:{id}")
}

pub fn review_key(id: u64) -> String {
    format!("review:{id}")
}

/// A review thread, by its GraphQL node id.
pub fn thread_key(id: &str) -> String {
    format!("thread:{id}")
}

/// What the window shows of a comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HiddenState {
    Visible,
    /// Collapsed to one line.
    Hidden,
    /// Hidden, but it has more posts than when it was hidden: shown again, marked.
    NewReply,
}

impl HiddenComments {
    /// Hides `key`, which holds `posts` posts now. Hiding again counts those posts as seen.
    pub fn hide(&mut self, key: &str, posts: u32) {
        self.hidden.insert(key.to_string(), posts);
    }

    /// Whether `key` was hidden.
    pub fn show(&mut self, key: &str) -> bool {
        self.hidden.remove(key).is_some()
    }

    /// An edited post keeps its place in the set: only a post that was not there counts.
    pub fn state(&self, key: &str, posts: u32) -> HiddenState {
        match self.hidden.get(key) {
            None => HiddenState::Visible,
            Some(&seen) if posts > seen => HiddenState::NewReply,
            Some(_) => HiddenState::Hidden,
        }
    }

    /// How many of `keys` (each with its post count) are collapsed now.
    pub fn collapsed<'a>(&self, keys: impl IntoIterator<Item = (&'a str, u32)>) -> usize {
        keys.into_iter()
            .filter(|(key, posts)| self.state(key, *posts) == HiddenState::Hidden)
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.hidden.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hidden_comment_stays_hidden_and_unhide_removes_it() {
        let mut set = HiddenComments::default();
        let key = issue_comment_key(7);
        assert_eq!(set.state(&key, 1), HiddenState::Visible);
        set.hide(&key, 1);
        assert_eq!(set.state(&key, 1), HiddenState::Hidden);
        assert!(set.show(&key));
        assert_eq!(set.state(&key, 1), HiddenState::Visible);
        assert!(!set.show(&key));
    }

    #[test]
    fn a_new_reply_shows_the_thread_again_until_it_is_hidden_again() {
        let mut set = HiddenComments::default();
        let key = thread_key("PRRT_1");
        set.hide(&key, 2);
        assert_eq!(set.state(&key, 2), HiddenState::Hidden);
        assert_eq!(set.state(&key, 3), HiddenState::NewReply);
        set.hide(&key, 3);
        assert_eq!(set.state(&key, 3), HiddenState::Hidden);
    }

    #[test]
    fn keys_of_different_kinds_never_collide() {
        assert_ne!(issue_comment_key(5), review_key(5));
    }

    #[test]
    fn collapsed_counts_only_the_ones_still_collapsed() {
        let mut set = HiddenComments::default();
        set.hide("a", 1);
        set.hide("b", 1);
        assert_eq!(set.collapsed([("a", 1), ("b", 2), ("c", 1)]), 1);
    }

    #[test]
    fn it_reads_back_from_json() {
        let mut set = HiddenComments::default();
        set.hide("thread:PRRT_1", 2);
        let json = serde_json::to_string(&set).unwrap();
        assert_eq!(json, r#"{"hidden":{"thread:PRRT_1":2}}"#);
        assert_eq!(serde_json::from_str::<HiddenComments>(&json).unwrap(), set);
        assert!(
            serde_json::from_str::<HiddenComments>("{}")
                .unwrap()
                .is_empty()
        );
    }
}
