//! A small least-recently-used map bounded by entry count and by bytes,
//! for the preview's render caches (formulas, diagrams, Typst's named
//! files). Caches here hold at most a few hundred entries, so eviction
//! scans for the oldest rather than keeping a linked list.

use std::{collections::HashMap, hash::Hash};

pub struct SizedLru<K, V> {
    map: HashMap<K, Slot<V>>,
    tick: u64,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}

struct Slot<V> {
    value: V,
    bytes: usize,
    used: u64,
}

impl<K: Eq + Hash + Clone, V> SizedLru<K, V> {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            tick: 0,
            bytes: 0,
            max_entries: max_entries.max(1),
            max_bytes,
        }
    }

    /// The value for `key`, marking it most recently used.
    pub fn get(&mut self, key: &K) -> Option<&V> {
        self.tick += 1;
        let slot = self.map.get_mut(key)?;
        slot.used = self.tick;
        Some(&slot.value)
    }

    #[cfg(test)]
    pub fn contains(&self, key: &K) -> bool {
        self.map.contains_key(key)
    }

    /// Insert or replace `key` (most recently used), then drop the least
    /// recently used entries until both limits hold. The new entry itself
    /// is kept even when it alone is over the byte limit. Returns the keys
    /// dropped.
    pub fn insert(&mut self, key: K, value: V, bytes: usize) -> Vec<K> {
        self.tick += 1;
        let slot = Slot {
            value,
            bytes,
            used: self.tick,
        };
        if let Some(old) = self.map.insert(key.clone(), slot) {
            self.bytes -= old.bytes;
        }
        self.bytes += bytes;
        let mut dropped = Vec::new();
        while self.map.len() > self.max_entries
            || (self.bytes > self.max_bytes && self.map.len() > 1)
        {
            let Some(oldest) = self
                .map
                .iter()
                .filter(|(k, _)| **k != key)
                .min_by_key(|(_, slot)| slot.used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(slot) = self.map.remove(&oldest) {
                self.bytes -= slot.bytes;
            }
            dropped.push(oldest);
        }
        dropped
    }

    #[cfg(test)]
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let slot = self.map.remove(key)?;
        self.bytes -= slot.bytes;
        Some(slot.value)
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.bytes = 0;
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    #[cfg(test)]
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_the_least_recently_used_past_the_entry_cap() {
        let mut lru = SizedLru::new(3, usize::MAX);
        for k in 0..3 {
            assert!(lru.insert(k, k * 10, 1).is_empty());
        }
        // Touch 0: 1 is now the oldest.
        assert_eq!(lru.get(&0), Some(&0));
        assert_eq!(lru.insert(3, 30, 1), [1]);
        assert!(lru.contains(&0) && !lru.contains(&1));
        assert_eq!(lru.len(), 3);
    }

    #[test]
    fn drops_by_bytes_and_keeps_an_oversized_newcomer() {
        let mut lru = SizedLru::new(100, 100);
        lru.insert("a", (), 40);
        lru.insert("b", (), 40);
        assert_eq!(lru.insert("c", (), 40), ["a"]);
        assert_eq!(lru.bytes(), 80);
        // Bigger than the whole budget: everything else goes, it stays.
        let mut dropped = lru.insert("huge", (), 500);
        dropped.sort();
        assert_eq!(dropped, ["b", "c"]);
        assert_eq!((lru.len(), lru.bytes()), (1, 500));
        // Replacing an entry accounts for the old size.
        lru.insert("huge", (), 10);
        assert_eq!(lru.bytes(), 10);
        assert_eq!(lru.remove(&"huge"), Some(()));
        assert_eq!(lru.bytes(), 0);
    }

    #[test]
    fn stays_bounded_under_a_stream_of_new_keys() {
        let mut lru = SizedLru::new(64, 1000);
        for k in 0..10_000u32 {
            lru.insert(k, k, (k % 50) as usize);
            assert!(lru.len() <= 64);
            assert!(lru.bytes() <= 1000 || lru.len() == 1);
        }
        // The most recent keys survive.
        assert!(lru.contains(&9_999) && !lru.contains(&0));
    }
}
