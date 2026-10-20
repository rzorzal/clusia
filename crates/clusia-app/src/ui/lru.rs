//! A map bounded by the bytes its values hold: once over its cap, it forgets the entries used
//! least recently, skipping the ones the caller says are still in use.

use std::collections::HashMap;
use std::hash::Hash;

#[derive(Debug)]
struct Entry<V> {
    value: V,
    bytes: usize,
    used: u64,
}

#[derive(Debug)]
pub struct ByteLru<K, V> {
    entries: HashMap<K, Entry<V>>,
    /// Bumped on every use; an entry's `used` is the clock at its last use.
    clock: u64,
    bytes: usize,
    cap: usize,
}

impl<K: Eq + Hash + Clone, V> ByteLru<K, V> {
    pub fn new(cap: usize) -> Self {
        Self {
            entries: HashMap::new(),
            clock: 0,
            bytes: 0,
            cap,
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// The value, marked as just used.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        let now = self.tick();
        let entry = self.entries.get_mut(key)?;
        entry.used = now;
        Some(&entry.value)
    }

    /// The value, without counting it as used.
    pub fn peek(&self, key: &K) -> Option<&V> {
        self.entries.get(key).map(|e| &e.value)
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// Stores `value` as just used, replacing what `key` held. It does not evict: the caller
    /// knows which entries are in use, so it calls `evict`.
    pub fn insert(&mut self, key: K, value: V, bytes: usize) -> Option<V> {
        let used = self.tick();
        self.bytes += bytes;
        let old = self.entries.insert(key, Entry { value, bytes, used })?;
        self.bytes -= old.bytes;
        Some(old.value)
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        let old = self.entries.remove(key)?;
        self.bytes -= old.bytes;
        Some(old.value)
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        let mut freed = 0;
        self.entries.retain(|k, e| {
            let kept = keep(k, &e.value);
            if !kept {
                freed += e.bytes;
            }
            kept
        });
        self.bytes -= freed;
    }

    /// Forgets the least recently used entries that `keep` does not hold on to, until the
    /// bytes are back under the cap. Entries in use stay even when they alone pass it.
    pub fn evict(&mut self, mut keep: impl FnMut(&K, &V) -> bool) -> Vec<(K, V)> {
        if self.bytes <= self.cap {
            return Vec::new();
        }
        let mut order: Vec<(u64, K)> = self
            .entries
            .iter()
            .filter(|(_, e)| e.bytes > 0)
            .map(|(k, e)| (e.used, k.clone()))
            .collect();
        order.sort_unstable_by_key(|(used, _)| *used);
        let mut gone = Vec::new();
        for (_, key) in order {
            if self.bytes <= self.cap {
                break;
            }
            if keep(&key, &self.entries[&key].value) {
                continue;
            }
            if let Some(value) = self.remove(&key) {
                gone.push((key, value));
            }
        }
        gone
    }

    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.entries.keys()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The bytes the values hold, as their callers counted them.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn cap(&self) -> usize {
        self.cap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_stays_under_its_cap_and_evicts_the_least_recently_used() {
        let mut lru = ByteLru::new(100);
        for (k, b) in [("a", 40), ("b", 40), ("c", 10)] {
            lru.insert(k, (), b);
        }
        assert!(lru.evict(|_, _| false).is_empty(), "90 bytes fit");
        lru.get(&"a");
        lru.insert("d", (), 40);
        let gone: Vec<_> = lru
            .evict(|_, _| false)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(gone, ["b"], "a was used after b");
        assert_eq!(lru.bytes(), 90);
        assert!(lru.bytes() <= lru.cap());
        lru.insert("e", (), 70);
        let gone: Vec<_> = lru
            .evict(|_, _| false)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(gone, ["c", "a", "d"], "oldest first");
        assert_eq!(lru.bytes(), 70, "only as many as needed");
    }

    #[test]
    fn entries_in_use_stay_even_past_the_cap() {
        let mut lru = ByteLru::new(50);
        lru.insert("shown", (), 60);
        lru.insert("old", (), 30);
        let gone: Vec<_> = lru
            .evict(|k, _| *k == "shown")
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(gone, ["old"]);
        assert!(lru.contains_key(&"shown"));
        assert_eq!(lru.bytes(), 60);
    }

    #[test]
    fn replacing_and_removing_keep_the_count() {
        let mut lru = ByteLru::new(100);
        lru.insert("a", 1, 30);
        lru.insert("a", 2, 50);
        assert_eq!((lru.bytes(), lru.len(), lru.peek(&"a")), (50, 1, Some(&2)));
        lru.insert("b", 3, 20);
        lru.retain(|k, _| *k != "a");
        assert_eq!(lru.bytes(), 20);
        lru.remove(&"b");
        assert_eq!((lru.bytes(), lru.is_empty()), (0, true));
    }
}
