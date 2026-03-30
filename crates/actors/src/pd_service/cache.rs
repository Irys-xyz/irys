use irys_types::Base64;
use lru::LruCache;
use reth::revm::primitives::{B256, bytes::Bytes};
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Default LRU cache capacity — enough for a few blocks' worth of PD chunks.
const DEFAULT_CACHE_CAPACITY: usize = 16_384;

/// Key for identifying a chunk globally by ledger and offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkKey {
    pub ledger: u32,
    pub offset: u64,
}

/// A cached chunk entry with reference tracking.
pub struct CachedChunkEntry {
    /// The unpacked chunk data.
    pub data: Arc<Bytes>,
    /// The Merkle data path for this chunk, if known.
    pub data_path: Option<Base64>,
    /// Transactions currently referencing this chunk (prevents eviction when non-empty).
    pub referencing_txs: HashSet<B256>,
    /// When this chunk was first cached.
    pub cached_at: Instant,
}

/// LRU-backed chunk cache with reference counting.
///
/// Chunks referenced by transactions are pinned —
/// the LRU only evicts unreferenced entries.
pub struct ChunkCache {
    chunks: LruCache<ChunkKey, CachedChunkEntry>,
    /// Shared index for lock-free chunk reads. Mirrors data stored in the LRU.
    shared_index: irys_types::chunk_provider::ChunkDataIndex,
    /// The capacity the cache was created with — used by `try_shrink_to_fit()`.
    initial_capacity: NonZeroUsize,
    /// Counter incremented each time an optimistic push hits the cache-hit shortcut.
    push_cache_hit_count: Arc<AtomicU64>,
}

impl ChunkCache {
    /// Create a new chunk cache with the given capacity.
    pub fn new(
        capacity: NonZeroUsize,
        shared_index: irys_types::chunk_provider::ChunkDataIndex,
        push_cache_hit_count: Arc<AtomicU64>,
    ) -> Self {
        Self {
            chunks: LruCache::new(capacity),
            shared_index,
            initial_capacity: capacity,
            push_cache_hit_count,
        }
    }

    /// Create a new chunk cache with the default capacity.
    pub fn with_default_capacity(
        shared_index: irys_types::chunk_provider::ChunkDataIndex,
        push_cache_hit_count: Arc<AtomicU64>,
    ) -> Self {
        Self::new(
            NonZeroUsize::new(DEFAULT_CACHE_CAPACITY)
                .expect("DEFAULT_CACHE_CAPACITY must be non-zero"),
            shared_index,
            push_cache_hit_count,
        )
    }

    /// Record that an optimistic push was short-circuited by a cache hit.
    pub fn record_push_cache_hit(&self) {
        self.push_cache_hit_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Get a chunk from the cache, promoting it in the LRU order.
    pub fn get(&mut self, key: &ChunkKey) -> Option<Arc<Bytes>> {
        self.chunks.get(key).map(|entry| Arc::clone(&entry.data))
    }

    /// Peek at a chunk without promoting it in LRU order.
    pub fn peek(&self, key: &ChunkKey) -> Option<Arc<Bytes>> {
        self.chunks.peek(key).map(|entry| Arc::clone(&entry.data))
    }

    /// Peek at the full cache entry without promoting it in LRU order.
    pub fn peek_entry(&self, key: &ChunkKey) -> Option<&CachedChunkEntry> {
        self.chunks.peek(key)
    }

    /// Check if a chunk is present in the cache.
    pub fn contains(&self, key: &ChunkKey) -> bool {
        self.chunks.contains(key)
    }

    /// Insert a chunk into the cache, referenced by the given transaction.
    ///
    /// If the cache is at capacity, unreferenced entries may be evicted first.
    /// If the chunk already exists, just adds the tx reference.
    ///
    /// Returns `true` if the chunk was newly inserted (not already present).
    pub fn insert(&mut self, key: ChunkKey, data: Arc<Bytes>, tx_hash: B256) -> bool {
        self.insert_with_data_path(key, data, tx_hash, None)
    }

    /// Insert a chunk with an optional data path, referenced by the given transaction.
    ///
    /// If the cache is at capacity, unreferenced entries may be evicted first.
    /// If the chunk already exists, adds the tx reference and backfills `data_path`
    /// if the existing entry doesn't have one.
    ///
    /// Returns `true` if the chunk was newly inserted (not already present).
    pub fn insert_with_data_path(
        &mut self,
        key: ChunkKey,
        data: Arc<Bytes>,
        tx_hash: B256,
        data_path: Option<Base64>,
    ) -> bool {
        if let Some(entry) = self.chunks.get_mut(&key) {
            entry.referencing_txs.insert(tx_hash);
            if entry.data_path.is_none() {
                entry.data_path = data_path;
            }
            return false;
        }

        // Before inserting, evict unreferenced entries if at capacity.
        // If all entries are pinned, temporarily grow capacity so push() doesn't
        // evict a pinned entry.
        self.make_room_for_insert();

        let mut referencing_txs = HashSet::new();
        referencing_txs.insert(tx_hash);

        self.chunks.push(
            key,
            CachedChunkEntry {
                data: Arc::clone(&data),
                data_path,
                referencing_txs,
                cached_at: Instant::now(),
            },
        );

        // Mirror into shared index for lock-free reads
        self.shared_index.insert((key.ledger, key.offset), data);

        true
    }

    /// Insert an unreferenced chunk into the cache (e.g. received via gossip,
    /// not yet claimed by any executing transaction).
    ///
    /// If the key already exists, the entry is left in place and its `data_path`
    /// is filled in if it was previously `None`. Returns `false` without replacing
    /// data or evicting anything.
    ///
    /// If the key is absent, inserts a new entry with an empty `referencing_txs`
    /// set, making it immediately eligible for LRU eviction.
    ///
    /// Returns `true` if the chunk was newly inserted.
    pub fn insert_unreferenced(
        &mut self,
        key: ChunkKey,
        data: Arc<Bytes>,
        data_path: Base64,
    ) -> bool {
        if let Some(existing) = self.chunks.peek_mut(&key) {
            // Fill in data_path if missing
            if existing.data_path.is_none() {
                existing.data_path = Some(data_path);
            }
            return false;
        }

        self.make_room_for_insert();
        self.shared_index
            .insert((key.ledger, key.offset), data.clone());
        self.chunks.push(
            key,
            CachedChunkEntry {
                data,
                data_path: Some(data_path),
                referencing_txs: HashSet::new(),
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
    /// Returns `true` if the chunk has no remaining references.
    pub fn remove_reference(&mut self, key: &ChunkKey, tx_hash: &B256) -> bool {
        if let Some(entry) = self.chunks.get_mut(key) {
            entry.referencing_txs.remove(tx_hash);
            entry.referencing_txs.is_empty()
        } else {
            false
        }
    }

    /// Remove a chunk from the cache entirely (e.g., after all references removed).
    pub fn remove(&mut self, key: &ChunkKey) {
        self.chunks.pop(key);
        self.shared_index.remove(&(key.ledger, key.offset));
    }

    /// Number of entries currently in the cache.
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// Returns `true` if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Make room for one new entry. If at capacity, try to evict an unreferenced
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
            .find(|(_, entry)| entry.referencing_txs.is_empty())
            .map(|(key, _)| *key);

        if let Some(key) = evictable {
            self.chunks.pop(&key);
            self.shared_index.remove(&(key.ledger, key.offset));
        } else {
            // All entries are pinned — grow capacity to avoid evicting a pinned entry.
            // Capacity is shrunk back by `try_shrink_to_fit()` called during release.
            let new_cap =
                NonZeroUsize::new(self.chunks.cap().get() + 1).expect("cap + 1 must be non-zero");
            self.chunks.resize(new_cap);
        }
    }

    /// Shrink the LRU capacity back to the initial capacity if the number
    /// of live entries has fallen at or below it.
    ///
    /// This reverses capacity growth from [`make_room_for_insert`] once the burst
    /// of pinned entries has been released. The `len() <= target` guard ensures
    /// `LruCache::resize()` will not call `pop_lru()` internally, so no
    /// `shared_index` mirroring is bypassed.
    pub fn try_shrink_to_fit(&mut self) {
        if self.chunks.cap() > self.initial_capacity
            && self.chunks.len() <= self.initial_capacity.get()
        {
            self.chunks.resize(self.initial_capacity);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dashmap::DashMap;
    use std::sync::Arc;

    type ChunkDataIndex = irys_types::chunk_provider::ChunkDataIndex;

    fn make_key(offset: u64) -> ChunkKey {
        ChunkKey { ledger: 0, offset }
    }

    fn make_data(byte: u8) -> Arc<Bytes> {
        Arc::new(Bytes::from(vec![byte; 32]))
    }

    fn default_counter() -> Arc<AtomicU64> {
        Arc::new(AtomicU64::new(0))
    }

    #[test]
    fn test_insert_and_get() {
        let shared_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
        let mut cache = ChunkCache::with_default_capacity(shared_index, default_counter());
        let key = make_key(1);
        let data = make_data(0xAA);
        let tx = B256::ZERO;

        assert!(cache.insert(key, data.clone(), tx));
        assert!(cache.contains(&key));
        assert_eq!(cache.get(&key).unwrap().as_ref(), data.as_ref());
    }

    #[test]
    fn test_duplicate_insert_adds_reference() {
        let shared_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
        let mut cache = ChunkCache::with_default_capacity(shared_index, default_counter());
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
        let shared_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
        let mut cache = ChunkCache::with_default_capacity(shared_index, default_counter());
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
        let shared_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
        let mut cache = ChunkCache::new(cap, shared_index, default_counter());
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
        let shared_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
        let mut cache = ChunkCache::new(cap, shared_index, default_counter());
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
    fn test_shared_index_mirrors_insert_and_remove() {
        let shared_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
        let mut cache =
            ChunkCache::with_default_capacity(Arc::clone(&shared_index), default_counter());
        let key = make_key(1);
        let tx = B256::ZERO;

        cache.insert(key, make_data(0xAA), tx);
        assert!(shared_index.contains_key(&(key.ledger, key.offset)));

        cache.remove(&key);
        assert!(!shared_index.contains_key(&(key.ledger, key.offset)));
    }

    #[test]
    fn test_shared_index_mirrors_lru_eviction() {
        let cap = NonZeroUsize::new(2).unwrap();
        let shared_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
        let mut cache = ChunkCache::new(cap, Arc::clone(&shared_index), default_counter());
        let tx = B256::ZERO;

        cache.insert(make_key(1), make_data(1), tx);
        cache.insert(make_key(2), make_data(2), tx);
        cache.remove_reference(&make_key(1), &tx);

        // This eviction should also remove from shared_index
        cache.insert(make_key(3), make_data(3), B256::with_last_byte(1));

        assert!(!shared_index.contains_key(&(0_u32, 1_u64)));
        assert!(shared_index.contains_key(&(0_u32, 2_u64)));
        assert!(shared_index.contains_key(&(0_u32, 3_u64)));
    }

    #[test]
    fn test_capacity_shrinks_after_burst() {
        let cap = NonZeroUsize::new(2).unwrap();
        let shared_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
        let mut cache = ChunkCache::new(cap, shared_index, default_counter());
        let tx1 = B256::ZERO;
        let tx2 = B256::with_last_byte(1);
        let tx3 = B256::with_last_byte(2);

        // Fill to cap with pinned entries
        cache.insert(make_key(1), make_data(1), tx1);
        cache.insert(make_key(2), make_data(2), tx2);
        // Third insert forces capacity growth (both existing entries are pinned)
        cache.insert(make_key(3), make_data(3), tx3);
        assert!(cache.chunks.cap().get() > 2, "capacity should have grown");

        // Release all references and remove entries
        cache.remove_reference(&make_key(1), &tx1);
        cache.remove(&make_key(1));
        cache.remove_reference(&make_key(2), &tx2);
        cache.remove(&make_key(2));
        cache.remove_reference(&make_key(3), &tx3);
        cache.remove(&make_key(3));

        assert_eq!(cache.len(), 0);
        // Cap is still inflated
        assert!(cache.chunks.cap().get() > 2);

        cache.try_shrink_to_fit();
        // Cap is back to initial
        assert_eq!(cache.chunks.cap().get(), 2);
    }

    #[test]
    fn test_shrink_noop_when_not_inflated() {
        let shared_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
        let mut cache = ChunkCache::with_default_capacity(shared_index, default_counter());
        let original_cap = cache.chunks.cap();

        cache.try_shrink_to_fit();
        assert_eq!(cache.chunks.cap(), original_cap, "should be a no-op");
    }

    #[test]
    fn test_shrink_waits_until_len_below_initial() {
        let cap = NonZeroUsize::new(2).unwrap();
        let shared_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
        let mut cache = ChunkCache::new(cap, shared_index, default_counter());
        let tx1 = B256::ZERO;
        let tx2 = B256::with_last_byte(1);
        let tx3 = B256::with_last_byte(2);

        cache.insert(make_key(1), make_data(1), tx1);
        cache.insert(make_key(2), make_data(2), tx2);
        cache.insert(make_key(3), make_data(3), tx3);
        // Cap grew, len is 3
        assert!(cache.chunks.cap().get() > 2);

        // Remove only one entry — len is still 2 = initial cap
        cache.remove_reference(&make_key(3), &tx3);
        cache.remove(&make_key(3));

        cache.try_shrink_to_fit();
        // Should shrink because len (2) <= initial cap (2)
        assert_eq!(cache.chunks.cap().get(), 2);
    }

    #[test]
    fn test_insert_unreferenced_creates_evictable_entry() {
        let shared_index: ChunkDataIndex = Arc::new(DashMap::new());
        let mut cache = ChunkCache::new(
            NonZeroUsize::new(2).unwrap(),
            shared_index.clone(),
            default_counter(),
        );
        let key = ChunkKey {
            ledger: 0,
            offset: 100,
        };
        let data = Arc::new(Bytes::from(vec![1_u8; 256]));
        let data_path = Base64(vec![2_u8; 64]);

        let expected_path = data_path.0.clone();
        let inserted = cache.insert_unreferenced(key, data, data_path);
        assert!(inserted);
        assert!(cache.contains(&key));
        assert!(shared_index.contains_key(&(0, 100)));

        let entry = cache.peek_entry(&key).unwrap();
        assert!(entry.referencing_txs.is_empty());
        assert_eq!(entry.data_path.as_ref().unwrap().0, expected_path);
    }

    #[test]
    fn test_insert_unreferenced_noop_when_exists() {
        let shared_index: ChunkDataIndex = Arc::new(DashMap::new());
        let mut cache = ChunkCache::new(
            NonZeroUsize::new(2).unwrap(),
            shared_index,
            default_counter(),
        );
        let key = ChunkKey {
            ledger: 0,
            offset: 100,
        };
        let data = Arc::new(Bytes::from(vec![1_u8; 256]));
        let tx_hash = B256::from([0xAA; 32]);

        cache.insert(key, data, tx_hash);

        let data_path = Base64(vec![2_u8; 64]);
        let new_data = Arc::new(Bytes::from(vec![1_u8; 256]));
        let inserted = cache.insert_unreferenced(key, new_data, data_path);
        assert!(!inserted);

        let entry = cache.peek_entry(&key).unwrap();
        assert!(entry.referencing_txs.contains(&tx_hash));
    }

    #[test]
    fn test_insert_unreferenced_fills_missing_data_path() {
        let shared_index: ChunkDataIndex = Arc::new(DashMap::new());
        let mut cache = ChunkCache::new(
            NonZeroUsize::new(2).unwrap(),
            shared_index,
            default_counter(),
        );
        let key = ChunkKey {
            ledger: 0,
            offset: 100,
        };
        let data = Arc::new(Bytes::from(vec![1_u8; 256]));
        let tx_hash = B256::from([0xAA; 32]);

        cache.insert(key, data, tx_hash);
        assert!(cache.peek_entry(&key).unwrap().data_path.is_none());

        let data_path = Base64(vec![2_u8; 64]);
        let expected_path = data_path.0.clone();
        let new_data = Arc::new(Bytes::from(vec![1_u8; 256]));
        let inserted = cache.insert_unreferenced(key, new_data, data_path);
        assert!(!inserted);

        let entry = cache.peek_entry(&key).unwrap();
        assert_eq!(entry.data_path.as_ref().unwrap().0, expected_path);
    }

    #[test]
    fn test_insert_with_data_path_stores_it() {
        let shared_index: ChunkDataIndex = Arc::new(DashMap::new());
        let mut cache = ChunkCache::new(
            NonZeroUsize::new(2).unwrap(),
            shared_index,
            default_counter(),
        );
        let key = ChunkKey {
            ledger: 0,
            offset: 100,
        };
        let data = Arc::new(Bytes::from(vec![1_u8; 256]));
        let tx_hash = B256::from([0xAA; 32]);
        let data_path = Base64(vec![2_u8; 64]);
        let expected_path = data_path.0.clone();

        cache.insert_with_data_path(key, data, tx_hash, Some(data_path));
        let entry = cache.peek_entry(&key).unwrap();
        assert_eq!(entry.data_path.as_ref().unwrap().0, expected_path);
    }

    #[test]
    fn test_lru_eviction_prefers_unreferenced() {
        let shared_index: ChunkDataIndex = Arc::new(DashMap::new());
        let mut cache = ChunkCache::new(
            NonZeroUsize::new(2).unwrap(),
            shared_index,
            default_counter(),
        );

        let key1 = ChunkKey {
            ledger: 0,
            offset: 1,
        };
        let key2 = ChunkKey {
            ledger: 0,
            offset: 2,
        };
        let key3 = ChunkKey {
            ledger: 0,
            offset: 3,
        };
        let data = Arc::new(Bytes::from(vec![1_u8; 256]));

        cache.insert(key1, data.clone(), B256::from([0xAA; 32]));
        cache.insert_unreferenced(key2, data.clone(), Base64(vec![]));

        cache.insert(key3, data, B256::from([0xBB; 32]));
        assert!(cache.contains(&key1));
        assert!(!cache.contains(&key2));
        assert!(cache.contains(&key3));
    }
}
