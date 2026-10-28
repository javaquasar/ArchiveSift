use std::collections::HashMap;
use std::hash::Hash;

struct CacheEntry<V> {
    value: V,
    estimated_bytes: usize,
    last_used: u64,
}

/// A small LRU cache bounded by both entry count and estimated memory usage.
pub(super) struct SizedLruCache<K, V> {
    entries: HashMap<K, CacheEntry<V>>,
    max_entries: usize,
    max_bytes: usize,
    total_bytes: usize,
    clock: u64,
}

impl<K, V> SizedLruCache<K, V>
where
    K: Clone + Eq + Hash,
{
    pub(super) fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries: max_entries.max(1),
            max_bytes: max_bytes.max(1),
            total_bytes: 0,
            clock: 0,
        }
    }

    pub(super) fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    pub(super) fn get(&mut self, key: &K) -> Option<&V> {
        self.clock = self.clock.wrapping_add(1);
        let entry = self.entries.get_mut(key)?;
        entry.last_used = self.clock;
        Some(&entry.value)
    }

    pub(super) fn insert(&mut self, key: K, value: V, estimated_bytes: usize) {
        if let Some(previous) = self.entries.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(previous.estimated_bytes);
        }

        self.clock = self.clock.wrapping_add(1);
        self.total_bytes = self.total_bytes.saturating_add(estimated_bytes);
        self.entries.insert(
            key,
            CacheEntry {
                value,
                estimated_bytes,
                last_used: self.clock,
            },
        );
        self.evict_to_limits();
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.total_bytes = 0;
    }

    fn evict_to_limits(&mut self) {
        while self.entries.len() > self.max_entries || self.total_bytes > self.max_bytes {
            let Some(oldest_key) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };

            if let Some(removed) = self.entries.remove(&oldest_key) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.estimated_bytes);
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn total_bytes(&self) -> usize {
        self.total_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_limit_evicts_the_least_recently_used_value() {
        let mut cache = SizedLruCache::new(2, 1_000);
        cache.insert("a", 1, 10);
        cache.insert("b", 2, 10);
        assert_eq!(cache.get(&"a"), Some(&1));

        cache.insert("c", 3, 10);

        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&"a"), Some(&1));
        assert_eq!(cache.get(&"b"), None);
        assert_eq!(cache.get(&"c"), Some(&3));
    }

    #[test]
    fn byte_limit_keeps_estimated_memory_bounded() {
        let mut cache = SizedLruCache::new(10, 100);
        cache.insert("a", 1, 60);
        cache.insert("b", 2, 60);

        assert_eq!(cache.len(), 1);
        assert!(cache.total_bytes() <= 100);
        assert_eq!(cache.get(&"a"), None);
        assert_eq!(cache.get(&"b"), Some(&2));
    }

    #[test]
    fn replacing_an_entry_updates_its_size_without_growing_count() {
        let mut cache = SizedLruCache::new(2, 100);
        cache.insert("a", 1, 80);
        cache.insert("a", 2, 20);

        assert_eq!(cache.len(), 1);
        assert_eq!(cache.total_bytes(), 20);
        assert_eq!(cache.get(&"a"), Some(&2));
    }

    #[test]
    fn clear_releases_entries_and_resets_byte_accounting() {
        let mut cache = SizedLruCache::new(2, 100);
        cache.insert("a", 1, 40);
        cache.insert("b", 2, 30);

        cache.clear();

        assert_eq!(cache.len(), 0);
        assert_eq!(cache.total_bytes(), 0);
        assert_eq!(cache.get(&"a"), None);
    }
}
