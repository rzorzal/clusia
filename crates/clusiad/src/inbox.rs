//! `inbox.json`: what the tray lists, which events were already announced and how far each
//! detector had got, so a restart neither repeats an announcement nor misses one.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use clusia_core::Paths;
use clusia_protocol::InboxItem;
use clusia_store::atomic::{quarantine, write_atomic};
use serde::{Deserialize, Serialize};

/// Items kept; older ones fall off the end of the list.
pub(crate) const MAX_ITEMS: usize = 200;
/// How long a dedupe key is remembered.
const KEY_TTL_SECS: i64 = 30 * 86_400;
const MAX_KEYS: usize = 4_000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InboxData {
    /// Oldest first.
    #[serde(default)]
    pub items: Vec<InboxItem>,
    /// Dedupe key → when it was announced.
    #[serde(default)]
    pub keys: BTreeMap<String, i64>,
    /// RFC 3339 time up to which the notifications feed has been read.
    #[serde(default)]
    pub feed_since: Option<String>,
    /// Pull request (`PrRef::file_key`) → `<updated_at>|<head sha>:<state>` at the last look.
    #[serde(default)]
    pub checks: BTreeMap<String, String>,
}

/// The file's content, and where an unreadable file was moved to.
pub(crate) struct Loaded {
    pub data: InboxData,
    pub quarantined: Option<PathBuf>,
}

impl InboxData {
    /// A missing file is an empty inbox; an unreadable one is set aside and replaced by an
    /// empty inbox.
    pub fn load(paths: &Paths) -> Loaded {
        let path = paths.inbox();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Loaded {
                    data: Self::default(),
                    quarantined: None,
                };
            }
            Err(e) => {
                tracing::warn!(error = %e, "cannot read the inbox; starting empty");
                return Loaded {
                    data: Self::default(),
                    quarantined: None,
                };
            }
        };
        match serde_json::from_slice::<Self>(&bytes) {
            Ok(data) => Loaded {
                data,
                quarantined: None,
            },
            Err(e) => {
                let moved = quarantine(&path, clusia_store::now_unix()).ok();
                tracing::warn!(error = %e, "the inbox was unreadable and was set aside");
                Loaded {
                    data: Self::default(),
                    quarantined: moved,
                }
            }
        }
    }

    pub fn save(&self, paths: &Paths) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        write_atomic(&paths.inbox(), &json)
    }

    pub fn has_key(&self, key: &str) -> bool {
        self.keys.contains_key(key)
    }

    pub fn remember(&mut self, key: &str, at: i64) {
        self.keys.insert(key.to_string(), at);
        self.keys.retain(|_, seen| at - *seen <= KEY_TTL_SECS);
        if self.keys.len() > MAX_KEYS {
            let mut by_age: Vec<(i64, String)> =
                self.keys.iter().map(|(k, at)| (*at, k.clone())).collect();
            by_age.sort();
            for (_, key) in by_age.into_iter().take(self.keys.len() - MAX_KEYS) {
                self.keys.remove(&key);
            }
        }
    }

    pub fn push(&mut self, item: InboxItem) {
        self.items.push(item);
        let excess = self.items.len().saturating_sub(MAX_ITEMS);
        self.items.drain(..excess);
    }

    pub fn unseen(&self) -> u32 {
        self.items.iter().filter(|i| !i.seen).count() as u32
    }

    /// Newest first.
    pub fn list(&self) -> Vec<InboxItem> {
        self.items.iter().rev().cloned().collect()
    }

    /// Marks the items with these ids seen (all of them for an empty list); whether any
    /// changed.
    pub fn mark_seen(&mut self, ids: &[String]) -> bool {
        let mut changed = false;
        for item in &mut self.items {
            if !item.seen && (ids.is_empty() || ids.contains(&item.id)) {
                item.seen = true;
                changed = true;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clusia_core::config::EventKind;

    fn item(id: &str) -> InboxItem {
        InboxItem {
            id: id.into(),
            kind: EventKind::Mentioned,
            pr: None,
            title: "t".into(),
            body: "b".into(),
            at: 1,
            seen: false,
        }
    }

    #[test]
    fn the_inbox_survives_a_save_and_a_load() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let mut data = InboxData::default();
        data.push(item("a"));
        data.remember("k", 5);
        data.feed_since = Some("2026-10-01T00:00:00Z".into());
        data.checks
            .insert("acme~widgets~7".into(), "c1:failing".into());
        data.save(&paths).unwrap();
        let loaded = InboxData::load(&paths);
        assert_eq!(loaded.data, data);
        assert_eq!(loaded.quarantined, None);
    }

    #[test]
    fn a_missing_file_is_empty_and_a_broken_one_is_set_aside() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let loaded = InboxData::load(&paths);
        assert_eq!(
            (loaded.data, loaded.quarantined),
            (InboxData::default(), None)
        );
        fs::write(paths.inbox(), "{ not json").unwrap();
        let loaded = InboxData::load(&paths);
        assert_eq!(loaded.data, InboxData::default());
        let moved = loaded.quarantined.expect("set aside");
        assert!(moved.to_string_lossy().contains("inbox.json.corrupt-"));
        assert!(!paths.inbox().exists());
    }

    #[test]
    fn the_oldest_items_fall_off_and_the_list_is_newest_first() {
        let mut data = InboxData::default();
        for n in 0..MAX_ITEMS + 5 {
            data.push(item(&n.to_string()));
        }
        assert_eq!(data.items.len(), MAX_ITEMS);
        assert_eq!(data.items[0].id, "5");
        assert_eq!(data.list()[0].id, (MAX_ITEMS + 4).to_string());
    }

    #[test]
    fn marking_seen_takes_ids_or_everything() {
        let mut data = InboxData::default();
        for id in ["a", "b", "c"] {
            data.push(item(id));
        }
        assert_eq!(data.unseen(), 3);
        assert!(data.mark_seen(&["b".into(), "zzz".into()]));
        assert_eq!(data.unseen(), 2);
        assert!(!data.mark_seen(&["b".into()]), "already seen");
        assert!(data.mark_seen(&[]));
        assert_eq!(data.unseen(), 0);
        assert!(!data.mark_seen(&[]));
    }

    #[test]
    fn keys_expire_and_the_set_stays_bounded() {
        let mut data = InboxData::default();
        data.remember("old", 0);
        data.remember("new", KEY_TTL_SECS + 10);
        assert!(!data.has_key("old"));
        assert!(data.has_key("new"));
        for n in 0..MAX_KEYS + 10 {
            data.remember(&format!("k{n}"), KEY_TTL_SECS + 11 + n as i64);
        }
        assert_eq!(data.keys.len(), MAX_KEYS);
        assert!(!data.has_key("k0"), "the oldest key went first");
        assert!(data.has_key(&format!("k{}", MAX_KEYS + 9)));
    }
}
