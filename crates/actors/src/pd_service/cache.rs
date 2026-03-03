use lru::LruCache;
use reth::revm::primitives::{B256, bytes::Bytes};
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Instant;

/// Default LRU cache capacity — enough for a few blocks' worth of PD chunks.
const DEFAULT_CACHE_CAPACITY: usize = 16_384;

/// Key for identifying a chunk globally by ledger and offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkKey {
    pub ledger: u32,
    pub offset: u64,
}

/// A cached chunk entry with reference tracking and lock support.
pub struct CachedChunkEntry {
    /// The unpacked chunk data.
    pub data: Arc<Bytes>,
    /// Transactions currently referencing this chunk (prevents eviction when non-empty).
    pub referencing_txs: HashSet<B256>,
    /// Number of active execution locks on this chunk.
    pub lock_count: u32,
    /// When this chunk was first cached.
    pub cached_at: Instant,
}

/// LRU-backed chunk cache with reference counting and lock pinning.
///
/// Chunks referenced by transactions or locked for execution are pinned —
/// the LRU only evicts unreferenced, unlocked entries.
pub struct ChunkCache {
    chunks: LruCache<ChunkKey, CachedChunkEntry>,
}

impl ChunkCache {
    /// Create a new chunk cache with the given capacity.
    pub fn new(capacity: NonZeroUsize) -> Self {
        Self {
            chunks: LruCache::new(capacity),
        }
    }

    /// Create a new chunk cache with the default capacity.
    pub fn with_default_capacity() -> Self {
        Self::new(
            NonZeroUsize::new(DEFAULT_CACHE_CAPACITY)
                .expect("DEFAULT_CACHE_CAPACITY must be non-zero"),
        )
    }

    /// Get a chunk from the cache, promoting it in the LRU order.
    pub fn get(&mut self, key: &ChunkKey) -> Option<Arc<Bytes>> {
        self.chunks.get(key).map(|entry| Arc::clone(&entry.data))
    }

    /// Peek at a chunk without promoting it in LRU order.
    pub fn peek(&self, key: &ChunkKey) -> Option<Arc<Bytes>> {
        self.chunks.peek(key).map(|entry| Arc::clone(&entry.data))
    }

    /// Check if a chunk is present in the cache.
    pub fn contains(&self, key: &ChunkKey) -> bool {
        self.chunks.contains(key)
    }

    /// Insert a chunk into the cache without any transaction reference.
    ///
    /// Used for on-demand storage loading (e.g., during block validation via GetChunk).
    /// The chunk has no pinning references, so it can be evicted by LRU at any time.
    /// This is safe because the caller holds an `Arc<Bytes>` to the data.
    ///
    /// Returns `true` if the chunk was newly inserted (not already present).
    pub fn insert_unreferenced(&mut self, key: ChunkKey, data: Arc<Bytes>) -> bool {
        if self.chunks.contains(&key) {
            return false;
        }

        self.make_room_for_insert();

        self.chunks.push(
            key,
            CachedChunkEntry {
                data,
                referencing_txs: HashSet::new(),
                lock_count: 0,
                cached_at: Instant::now(),
            },
        );
        true
    }

    /// Insert a chunk into the cache, referenced by the given transaction.
    ///
    /// If the cache is at capacity, unreferenced and unlocked entries may be
    /// evicted first. If the chunk already exists, just adds the tx reference.
    ///
    /// Returns `true` if the chunk was newly inserted (not already present).
    pub fn insert(&mut self, key: ChunkKey, data: Arc<Bytes>, tx_hash: B256) -> bool {
        if let Some(entry) = self.chunks.get_mut(&key) {
            entry.referencing_txs.insert(tx_hash);
            return false;
        }

        // Before inserting, evict unreferenced/unlocked entries if at capacity.
        // If all entries are pinned, temporarily grow capacity so push() doesn't
        // evict a pinned entry.
        self.make_room_for_insert();

        let mut referencing_txs = HashSet::new();
        referencing_txs.insert(tx_hash);

        self.chunks.push(
            key,
            CachedChunkEntry {
                data,
                referencing_txs,
                lock_count: 0,
                cached_at: Instant::now(),
            },
        );
        true
    }

    /// Add a transaction reference to an existing cached chunk.
    /// Returns `false` if the chunk is not in the cache.
    pub fn add_reference(&mut self, key: &ChunkKey, tx_hash: B256) -> bool {
        if let Some(entry) = self.chunks.get_mut(key) {
            entry.referencing_txs.insert(tx_hash);
            true
        } else {
            false
        }
    }

    /// Remove a transaction reference from a cached chunk.
    /// Returns `true` if the chunk has no remaining references and is not locked.
    pub fn remove_reference(&mut self, key: &ChunkKey, tx_hash: &B256) -> bool {
        if let Some(entry) = self.chunks.get_mut(key) {
            entry.referencing_txs.remove(tx_hash);
            entry.referencing_txs.is_empty() && entry.lock_count == 0
        } else {
            false
        }
    }

    /// Increment the lock count for all given chunks.
    pub fn lock_chunks(&mut self, keys: &HashSet<ChunkKey>) {
        for key in keys {
            if let Some(entry) = self.chunks.get_mut(key) {
                entry.lock_count = entry.lock_count.saturating_add(1);
            }
        }
    }

    /// Decrement the lock count for all given chunks.
    pub fn unlock_chunks(&mut self, keys: &HashSet<ChunkKey>) {
        for key in keys {
            if let Some(entry) = self.chunks.get_mut(key) {
                entry.lock_count = entry.lock_count.saturating_sub(1);
            }
        }
    }

    /// Remove a chunk from the cache entirely (e.g., after all references removed).
    pub fn remove(&mut self, key: &ChunkKey) {
        self.chunks.pop(key);
    }

    /// Number of entries currently in the cache.
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// Returns `true` if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Make room for one new entry. If at capacity, try to evict an unreferenced/unlocked
    /// entry. If all entries are pinned, temporarily grow the LRU capacity so `push()` won't
    /// evict a pinned entry.
    fn make_room_for_insert(&mut self) {
        if self.chunks.len() < self.chunks.cap().get() {
            return;
        }

        // Find the least-recently-used entry eligible for eviction
        let evictable: Option<ChunkKey> = self
            .chunks
            .iter()
            .rev() // LRU order: least-recently-used first
            .find(|(_, entry)| entry.referencing_txs.is_empty() && entry.lock_count == 0)
            .map(|(key, _)| *key);

        if let Some(key) = evictable {
            self.chunks.pop(&key);
        } else {
            // All entries are pinned — grow capacity to avoid evicting a pinned entry.
            // The capacity will shrink back naturally as pinned entries are released and evicted.
            let new_cap =
                NonZeroUsize::new(self.chunks.cap().get() + 1).expect("cap + 1 must be non-zero");
            self.chunks.resize(new_cap);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_key(offset: u64) -> ChunkKey {
        ChunkKey { ledger: 0, offset }
    }

    fn make_data(byte: u8) -> Arc<Bytes> {
        Arc::new(Bytes::from(vec![byte; 32]))
    }

    #[test]
    fn test_insert_and_get() {
        let mut cache = ChunkCache::with_default_capacity();
        let key = make_key(1);
        let data = make_data(0xAA);
        let tx = B256::ZERO;

        assert!(cache.insert(key, data.clone(), tx));
        assert!(cache.contains(&key));
        assert_eq!(cache.get(&key).unwrap().as_ref(), data.as_ref());
    }

    #[test]
    fn test_duplicate_insert_adds_reference() {
        let mut cache = ChunkCache::with_default_capacity();
        let key = make_key(1);
        let data = make_data(0xAA);
        let tx1 = B256::ZERO;
        let tx2 = B256::with_last_byte(1);

        assert!(cache.insert(key, data.clone(), tx1));
        // Second insert with different tx should not be "new"
        assert!(!cache.insert(key, data, tx2));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn test_reference_removal() {
        let mut cache = ChunkCache::with_default_capacity();
        let key = make_key(1);
        let tx1 = B256::ZERO;
        let tx2 = B256::with_last_byte(1);

        cache.insert(key, make_data(0xAA), tx1);
        cache.add_reference(&key, tx2);

        // Removing one reference — still has another
        assert!(!cache.remove_reference(&key, &tx1));
        // Removing last reference — now eligible for eviction
        assert!(cache.remove_reference(&key, &tx2));
    }

    #[test]
    fn test_lru_eviction_skips_referenced() {
        let cap = NonZeroUsize::new(2).unwrap();
        let mut cache = ChunkCache::new(cap);
        let tx = B256::ZERO;

        // Fill cache
        cache.insert(make_key(1), make_data(1), tx);
        cache.insert(make_key(2), make_data(2), tx);

        // Both are referenced by tx, so inserting a third should not evict either
        let tx2 = B256::with_last_byte(1);
        cache.insert(make_key(3), make_data(3), tx2);

        // All three should still be present (LRU grows beyond cap when all pinned)
        assert!(cache.contains(&make_key(1)));
        assert!(cache.contains(&make_key(2)));
        assert!(cache.contains(&make_key(3)));
    }

    #[test]
    fn test_lru_eviction_removes_unreferenced() {
        let cap = NonZeroUsize::new(2).unwrap();
        let mut cache = ChunkCache::new(cap);
        let tx = B256::ZERO;

        cache.insert(make_key(1), make_data(1), tx);
        cache.insert(make_key(2), make_data(2), tx);

        // Remove references from key 1 (least recently used)
        cache.remove_reference(&make_key(1), &tx);

        // Insert new entry — should evict key 1
        let tx2 = B256::with_last_byte(1);
        cache.insert(make_key(3), make_data(3), tx2);

        assert!(!cache.contains(&make_key(1)));
        assert!(cache.contains(&make_key(2)));
        assert!(cache.contains(&make_key(3)));
    }

    #[test]
    fn test_lock_prevents_eviction() {
        let cap = NonZeroUsize::new(2).unwrap();
        let mut cache = ChunkCache::new(cap);
        let tx = B256::ZERO;

        cache.insert(make_key(1), make_data(1), tx);
        cache.insert(make_key(2), make_data(2), tx);

        // Remove all references from key 1 but lock it
        cache.remove_reference(&make_key(1), &tx);
        let keys = HashSet::from([make_key(1)]);
        cache.lock_chunks(&keys);

        // Insert new entry — key 1 is locked so should not be evicted
        let tx2 = B256::with_last_byte(1);
        cache.insert(make_key(3), make_data(3), tx2);

        assert!(cache.contains(&make_key(1)));
    }
}
