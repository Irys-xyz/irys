use irys_types::{DataRoot, TxChunkOffset, UnpackedChunk};
use lru::LruCache;
use std::collections::HashMap;
use std::num::NonZeroUsize;

/// A pending chunks cache that prioritizes entries with more chunks.
///
/// When at capacity, evicts the entry with the fewest chunks (not LRU).
/// This prevents attackers from evicting legitimate uploads by flooding
/// with single-chunk fake entries.
#[derive(Debug)]
pub struct PriorityPendingChunks {
    entries: HashMap<DataRoot, LruCache<TxChunkOffset, UnpackedChunk>>,
    max_entries: usize,
    max_chunks_per_entry: usize,
}

impl PriorityPendingChunks {
    pub fn new(max_entries: usize, max_chunks_per_entry: usize) -> Self {
        Self {
            entries: HashMap::with_capacity(max_entries),
            max_entries,
            max_chunks_per_entry,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Insert a chunk into the cache.
    ///
    /// If the data_root doesn't exist and the cache is at capacity,
    /// evicts the entry with the fewest chunks first.
    pub fn put(&mut self, chunk: UnpackedChunk) {
        let data_root = chunk.data_root;
        let tx_offset = chunk.tx_offset;

        if let Some(chunks) = self.entries.get_mut(&data_root) {
            chunks.put(tx_offset, chunk);
            return;
        }

        if self.entries.len() >= self.max_entries {
            self.evict_lowest_priority();
        }

        let mut new_cache = LruCache::new(
            NonZeroUsize::new(self.max_chunks_per_entry).expect("max_chunks_per_entry must be > 0"),
        );
        new_cache.put(tx_offset, chunk);
        self.entries.insert(data_root, new_cache);
    }

    #[must_use]
    pub fn pop(&mut self, data_root: &DataRoot) -> Option<LruCache<TxChunkOffset, UnpackedChunk>> {
        self.entries.remove(data_root)
    }

    pub fn get_mut(
        &mut self,
        data_root: &DataRoot,
    ) -> Option<&mut LruCache<TxChunkOffset, UnpackedChunk>> {
        self.entries.get_mut(data_root)
    }

    /// Get read-only access to the chunks for a given data_root without modifying LRU order.
    pub fn get(&self, data_root: &DataRoot) -> Option<&LruCache<TxChunkOffset, UnpackedChunk>> {
        self.entries.get(data_root)
    }

    /// Evict the entry with the fewest chunks.
    fn evict_lowest_priority(&mut self) {
        if self.entries.is_empty() {
            return;
        }

        // Find entry with fewest chunks
        let victim = self
            .entries
            .iter()
            .min_by_key(|(_, chunks)| chunks.len())
            .map(|(data_root, _)| *data_root);

        if let Some(data_root) = victim {
            tracing::debug!(?data_root, "Evicting lowest-priority pending chunks entry");
            self.entries.remove(&data_root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{DataRoot, TxChunkOffset};
    use proptest::prelude::*;

    fn make_chunk(data_root: [u8; 32], offset: u32, data_size: u64) -> UnpackedChunk {
        UnpackedChunk {
            data_root: DataRoot::from(data_root),
            data_size,
            tx_offset: TxChunkOffset::from(offset),
            data_path: Default::default(),
            bytes: Default::default(),
        }
    }

    #[test]
    fn evicts_entry_with_fewest_chunks() {
        let mut cache = PriorityPendingChunks::new(3, 10);

        let root_a = [1_u8; 32];
        cache.put(make_chunk(root_a, 0, 1000));
        cache.put(make_chunk(root_a, 1, 1000));
        cache.put(make_chunk(root_a, 2, 1000));

        let root_b = [2_u8; 32];
        cache.put(make_chunk(root_b, 0, 1000));

        let root_c = [3_u8; 32];
        cache.put(make_chunk(root_c, 0, 1000));
        cache.put(make_chunk(root_c, 1, 1000));

        assert_eq!(cache.len(), 3);

        let root_d = [4_u8; 32];
        cache.put(make_chunk(root_d, 0, 1000));

        assert_eq!(cache.len(), 3);
        assert!(cache.entries.contains_key(&DataRoot::from(root_a))); // 3 chunks - kept
        assert!(!cache.entries.contains_key(&DataRoot::from(root_b))); // 1 chunk - evicted
        assert!(cache.entries.contains_key(&DataRoot::from(root_c))); // 2 chunks - kept
        assert!(cache.entries.contains_key(&DataRoot::from(root_d))); // new entry
    }

    proptest! {
        #[test]
        fn multi_chunk_entries_survive_single_chunk_flooding(
            cache_size in 5..50_usize,
            legitimate_chunk_count in 2..10_usize,
            attacker_count in 10..100_usize,
        ) {
            let mut cache = PriorityPendingChunks::new(cache_size, 100);

            let legitimate_root = [0xAA_u8; 32];
            for i in 0..legitimate_chunk_count as u32 {
                cache.put(make_chunk(legitimate_root, i, 1000));
            }

            for i in 0..attacker_count {
                let attacker_root = [i as u8; 32];
                cache.put(make_chunk(attacker_root, 0, 1000));
            }

            // Property: multi-chunk entry survives single-chunk flooding
            prop_assert!(cache.entries.contains_key(&DataRoot::from(legitimate_root)));
            prop_assert_eq!(
                cache.entries.get(&DataRoot::from(legitimate_root)).unwrap().len(),
                legitimate_chunk_count
            );
        }
    }

    #[test]
    fn respects_max_chunks_per_entry() {
        let mut cache = PriorityPendingChunks::new(10, 3);

        let root = [1_u8; 32];
        for i in 0..5 {
            cache.put(make_chunk(root, i, 1000));
        }

        // Only 3 chunks should be kept (max_chunks_per_entry = 3)
        assert_eq!(cache.entries.get(&DataRoot::from(root)).unwrap().len(), 3);
    }
}
