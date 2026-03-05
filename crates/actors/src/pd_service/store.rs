use super::cache::{ChunkCache, ChunkKey};
use super::provisioning::{ProvisioningState, ProvisioningTracker};
use irys_types::chunk_provider::{ChunkConfig, ChunkTable, RethChunkProvider};
use irys_types::range_specifier::ChunkRangeSpecifier;
use reth::revm::primitives::{B256, bytes::Bytes};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{debug, trace, warn};

/// Synchronous wrapper that encapsulates the PD chunk provisioning logic
/// (cache + tracker + storage provider) without any async runtime dependency.
///
/// This is the core state extracted from `PdService` — all mutation goes through
/// `&mut self` methods, making it easy to wrap in `RwLock`/`Mutex` later.
pub struct PdChunkStore {
    cache: ChunkCache,
    tracker: ProvisioningTracker,
    storage_provider: Arc<dyn RethChunkProvider>,
    current_height: Option<u64>,
}

impl PdChunkStore {
    /// Create a new `PdChunkStore` backed by the given storage provider.
    pub fn new(storage_provider: Arc<dyn RethChunkProvider>) -> Self {
        Self {
            cache: ChunkCache::with_default_capacity(),
            tracker: ProvisioningTracker::new(),
            storage_provider,
            current_height: None,
        }
    }

    /// Check if chunks for a transaction are ready for execution.
    ///
    /// Returns `true` for unknown transactions (non-PD pass-through).
    pub fn is_ready(&self, tx_hash: &B256) -> bool {
        self.tracker.is_ready(tx_hash)
    }

    /// Batch-fetch chunks by `(ledger, offset)`, loading from storage on cache miss.
    ///
    /// Missing chunks are silently omitted — the caller (precompile) handles the absence.
    pub fn get_chunks_batch(&mut self, keys: &[(u32, u64)]) -> ChunkTable {
        let requested = keys.len();
        let mut table = HashMap::with_capacity(requested);
        for &(ledger, offset) in keys {
            if let Some(data) = self.get_chunk(ledger, offset) {
                table.insert((ledger, offset), data);
            }
        }
        debug!(
            requested,
            found = table.len(),
            missing = requested - table.len(),
            ?keys,
            "PdChunkStore::get_chunks_batch complete"
        );
        table
    }

    /// Provision chunks for a new PD transaction.
    ///
    /// Fetches required chunks from the cache or storage and registers the
    /// transaction in the provisioning tracker.
    pub fn provision_chunks(&mut self, tx_hash: B256, chunk_specs: Vec<ChunkRangeSpecifier>) {
        // Guard against duplicate provision requests.
        if self.tracker.get(&tx_hash).is_some() {
            debug!(tx_hash = %tx_hash, "PD transaction already registered, skipping");
            return;
        }

        let required_chunks = self.specs_to_keys(&chunk_specs);
        let total_chunks = required_chunks.len();

        debug!(
            tx_hash = %tx_hash,
            total_chunks,
            "Starting PD chunk provisioning"
        );

        let tx_state = self
            .tracker
            .register(tx_hash, required_chunks.clone(), self.current_height);

        let mut fetched = 0;
        let mut missing = HashSet::new();

        for key in &required_chunks {
            if self.cache.contains(key) {
                self.cache.add_reference(key, tx_hash);
                fetched += 1;
                trace!(
                    tx_hash = %tx_hash,
                    ledger = key.ledger,
                    offset = key.offset,
                    "Chunk already cached, added reference"
                );
            } else {
                match self
                    .storage_provider
                    .get_unpacked_chunk_by_ledger_offset(key.ledger, key.offset)
                {
                    Ok(Some(chunk)) => {
                        self.cache.insert(*key, Arc::new(chunk), tx_hash);
                        fetched += 1;
                        trace!(
                            tx_hash = %tx_hash,
                            ledger = key.ledger,
                            offset = key.offset,
                            fetched,
                            total = total_chunks,
                            "Fetched and cached chunk"
                        );
                    }
                    Ok(None) => {
                        warn!(
                            tx_hash = %tx_hash,
                            ledger = key.ledger,
                            offset = key.offset,
                            "Chunk not found locally"
                        );
                        missing.insert(*key);
                    }
                    Err(e) => {
                        warn!(
                            tx_hash = %tx_hash,
                            ledger = key.ledger,
                            offset = key.offset,
                            error = %e,
                            "Failed to fetch chunk from storage"
                        );
                        missing.insert(*key);
                    }
                }
            }
        }

        tx_state.missing_chunks = missing;
        if tx_state.missing_chunks.is_empty() {
            tx_state.state = ProvisioningState::Ready;
        } else {
            tx_state.state = ProvisioningState::PartiallyReady {
                found: fetched,
                total: total_chunks,
            };
        }

        debug!(
            tx_hash = %tx_hash,
            fetched,
            missing = tx_state.missing_chunks.len(),
            total = total_chunks,
            cached_chunks = self.cache.len(),
            state = ?tx_state.state,
            "PD chunk provisioning complete"
        );
    }

    /// Release chunks when a transaction is removed from the mempool.
    ///
    /// Decrements reference counts and evicts unreferenced chunks.
    pub fn release_chunks(&mut self, tx_hash: &B256) {
        if let Some(tx_state) = self.tracker.remove(tx_hash) {
            let mut evicted = 0;
            for key in &tx_state.required_chunks {
                let unreferenced = self.cache.remove_reference(key, tx_hash);
                if unreferenced {
                    self.cache.remove(key);
                    evicted += 1;
                }
            }
            trace!(
                tx_hash = %tx_hash,
                evicted_chunks = evicted,
                remaining_cached = self.cache.len(),
                "PD transaction removed, references decremented"
            );
        }
    }

    /// Expire stale provisioning entries at the given block height.
    ///
    /// Updates the current height and removes any tracker entries whose TTL
    /// has been reached, cleaning up their cache references.
    pub fn expire_at_height(&mut self, height: u64) {
        self.current_height = Some(height);

        let expired = self.tracker.expire_at_height(height);
        if !expired.is_empty() {
            debug!(
                expired_count = expired.len(),
                height, "Expiring stale PD provisioning entries"
            );

            for (tx_hash, required_chunks) in &expired {
                for key in required_chunks {
                    let unreferenced = self.cache.remove_reference(key, tx_hash);
                    if unreferenced {
                        self.cache.remove(key);
                    }
                }
            }
        }
    }

    /// Return the chunk configuration from the underlying storage provider.
    pub fn config(&self) -> ChunkConfig {
        self.storage_provider.config()
    }

    /// Get a single chunk by ledger and offset, loading from storage on cache miss.
    fn get_chunk(&mut self, ledger: u32, offset: u64) -> Option<Arc<Bytes>> {
        let key = ChunkKey { ledger, offset };

        // Fast path: cache hit
        if let Some(chunk) = self.cache.get(&key) {
            return Some(chunk);
        }

        // Cache miss — load from storage on demand
        match self
            .storage_provider
            .get_unpacked_chunk_by_ledger_offset(ledger, offset)
        {
            Ok(Some(chunk)) => {
                let data = Arc::new(chunk);
                self.cache.insert_unreferenced(key, data.clone());
                trace!(ledger, offset, "Loaded chunk from storage on cache miss");
                Some(data)
            }
            Ok(None) => {
                warn!(
                    ledger,
                    offset, "Chunk not found in storage during get_chunk"
                );
                None
            }
            Err(e) => {
                warn!(
                    ledger,
                    offset,
                    error = %e,
                    "Error loading chunk from storage during get_chunk"
                );
                None
            }
        }
    }

    /// Convert chunk range specifiers to chunk keys using checked arithmetic.
    fn specs_to_keys(&self, chunk_specs: &[ChunkRangeSpecifier]) -> HashSet<ChunkKey> {
        let config = self.storage_provider.config();
        let mut keys = HashSet::new();

        for spec in chunk_specs {
            let partition_index: u64 = match spec.partition_index.try_into() {
                Ok(v) => v,
                Err(_) => {
                    warn!(
                        partition_index = %spec.partition_index,
                        "Partition index exceeds u64::MAX, skipping spec"
                    );
                    continue;
                }
            };

            let base_offset = match config.num_chunks_in_partition.checked_mul(partition_index) {
                Some(v) => v,
                None => {
                    warn!(
                        num_chunks_in_partition = config.num_chunks_in_partition,
                        partition_index, "Base offset overflow, skipping spec"
                    );
                    continue;
                }
            };

            for i in 0..spec.chunk_count {
                let ledger_offset = base_offset
                    .checked_add(spec.offset as u64)
                    .and_then(|v| v.checked_add(i as u64));

                match ledger_offset {
                    Some(offset) => {
                        keys.insert(ChunkKey { ledger: 0, offset });
                    }
                    None => {
                        warn!(
                            base_offset,
                            spec_offset = spec.offset,
                            chunk_index = i,
                            "Ledger offset overflow, skipping chunk"
                        );
                    }
                }
            }
        }
        keys
    }
}

use irys_types::pd_handle::PdStoreApi;

/// Thread-safe wrapper implementing `PdStoreApi` for cross-crate use.
pub struct PdChunkStoreHandle {
    inner: parking_lot::Mutex<PdChunkStore>,
}

impl std::fmt::Debug for PdChunkStoreHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PdChunkStoreHandle").finish_non_exhaustive()
    }
}

impl PdChunkStoreHandle {
    pub fn new(store: PdChunkStore) -> Self {
        Self {
            inner: parking_lot::Mutex::new(store),
        }
    }
}

impl PdStoreApi for PdChunkStoreHandle {
    fn is_ready(&self, tx_hash: &B256) -> bool {
        self.inner.lock().is_ready(tx_hash)
    }

    fn provision_chunks(&self, tx_hash: B256, chunk_specs: Vec<ChunkRangeSpecifier>) {
        self.inner.lock().provision_chunks(tx_hash, chunk_specs);
    }

    fn release_chunks(&self, tx_hash: &B256) {
        self.inner.lock().release_chunks(tx_hash);
    }

    fn expire_at_height(&self, height: u64) {
        self.inner.lock().expire_at_height(height);
    }

    fn get_chunks_batch(&self, keys: &[(u32, u64)]) -> ChunkTable {
        self.inner.lock().get_chunks_batch(keys)
    }

    fn config(&self) -> ChunkConfig {
        self.inner.lock().config()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::chunk_provider::MockChunkProvider;

    fn test_store() -> PdChunkStore {
        PdChunkStore::new(Arc::new(MockChunkProvider::new()))
    }

    #[test]
    fn test_get_chunks_batch_from_storage() {
        let mut store = test_store();
        let table = store.get_chunks_batch(&[(0, 0), (0, 1), (0, 2)]);
        assert_eq!(table.len(), 3);
    }

    #[test]
    fn test_provision_and_is_ready() {
        let mut store = test_store();
        let tx_hash = B256::with_last_byte(0x01);
        let specs = vec![ChunkRangeSpecifier {
            partition_index: Default::default(),
            offset: 0,
            chunk_count: 2,
        }];
        store.provision_chunks(tx_hash, specs);
        assert!(store.is_ready(&tx_hash));
    }

    #[test]
    fn test_release_chunks() {
        let mut store = test_store();
        let tx_hash = B256::with_last_byte(0x01);
        let specs = vec![ChunkRangeSpecifier {
            partition_index: Default::default(),
            offset: 0,
            chunk_count: 2,
        }];
        store.provision_chunks(tx_hash, specs);
        store.release_chunks(&tx_hash);
        // After release, unknown tx returns true (pass-through)
        assert!(store.is_ready(&tx_hash));
    }

    #[test]
    fn test_expire_at_height() {
        let mut store = test_store();
        let tx_hash = B256::with_last_byte(0x01);

        // Set current height so TTL is computed
        store.current_height = Some(100);

        let specs = vec![ChunkRangeSpecifier {
            partition_index: Default::default(),
            offset: 0,
            chunk_count: 2,
        }];
        store.provision_chunks(tx_hash, specs);
        assert!(store.is_ready(&tx_hash));

        // Expire at height 120 (TTL_BLOCKS = 20, registered at height 100)
        store.expire_at_height(120);
        // After expiration the tracker entry is removed — unknown tx returns true
        assert!(store.is_ready(&tx_hash));
        // But the cache should be cleaned up too
        assert!(store.cache.is_empty());
    }

    #[test]
    fn test_config() {
        let store = test_store();
        let config = store.config();
        assert_eq!(config.num_chunks_in_partition, 100);
        assert_eq!(config.chunk_size, 256_000);
    }
}
