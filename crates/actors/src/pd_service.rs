pub mod cache;
pub mod provisioning;
pub mod store;

use cache::{ChunkCache, ChunkKey};
use irys_types::chunk_provider::{ChunkTable, PdChunkMessage, PdChunkReceiver, RethChunkProvider};
use irys_types::range_specifier::ChunkRangeSpecifier;
use provisioning::{ProvisioningState, ProvisioningTracker};
use reth::revm::primitives::{B256, bytes::Bytes};
use reth::tasks::shutdown::Shutdown;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{Instrument as _, debug, info, trace, warn};

use crate::block_tree_service::BlockStateUpdated;
use irys_types::TokioServiceHandle;

/// The PD (Programmable Data) Service manages chunk provisioning for PD transactions.
///
/// It handles the full lifecycle:
/// 1. New PD transaction → fetch required chunks from storage into LRU cache
/// 2. Transaction removal → release references, let LRU evict unused chunks
/// 3. Block state updates → expire stale provisioning entries
pub struct PdService {
    shutdown: Shutdown,
    msg_rx: PdChunkReceiver,
    cache: ChunkCache,
    tracker: ProvisioningTracker,
    storage_provider: Arc<dyn RethChunkProvider>,
    block_state_rx: broadcast::Receiver<BlockStateUpdated>,
    current_height: Option<u64>,
}

impl PdService {
    /// Spawn the PD service as a tokio task.
    pub fn spawn_service(
        msg_rx: PdChunkReceiver,
        storage_provider: Arc<dyn RethChunkProvider>,
        block_state_rx: broadcast::Receiver<BlockStateUpdated>,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        let (shutdown_signal, shutdown) = reth::tasks::shutdown::signal();

        let service = Self {
            shutdown,
            msg_rx,
            cache: ChunkCache::with_default_capacity(),
            tracker: ProvisioningTracker::new(),
            storage_provider,
            block_state_rx,
            current_height: None,
        };

        let join_handle = runtime_handle.spawn(
            async move {
                service.start().await;
            }
            .in_current_span(),
        );

        TokioServiceHandle {
            name: "pd_service".to_string(),
            handle: join_handle,
            shutdown_signal,
        }
    }

    async fn start(mut self) {
        info!("PdService started");

        loop {
            tokio::select! {
                biased;

                _ = &mut self.shutdown => {
                    info!("PdService received shutdown signal");
                    break;
                }

                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(message) => self.handle_message(message),
                        None => {
                            info!("PdService message channel closed");
                            break;
                        }
                    }
                }

                result = self.block_state_rx.recv() => {
                    match result {
                        Ok(event) if !event.discarded => {
                            self.handle_block_state_update(event.height);
                        }
                        Ok(_) => {
                            // Discarded block — don't advance height or expire entries.
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(skipped = n, "PdService lagged on block state events");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            info!("PdService block state channel closed");
                            break;
                        }
                    }
                }
            }
        }

        info!("PdService stopped");
    }

    fn handle_message(&mut self, msg: PdChunkMessage) {
        match msg {
            PdChunkMessage::NewTransaction {
                tx_hash,
                chunk_specs,
            } => {
                self.handle_provision_chunks(tx_hash, chunk_specs);
            }
            PdChunkMessage::TransactionRemoved { tx_hash } => {
                self.handle_release_chunks(&tx_hash);
            }
            PdChunkMessage::IsReady { tx_hash, response } => {
                let ready = self.handle_is_ready(&tx_hash);
                let _ = response.send(ready);
            }
            PdChunkMessage::GetChunksBatch { keys, response } => {
                let table = self.handle_get_chunks_batch(keys);
                let _ = response.send(table);
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

    /// Provision chunks for a new PD transaction.
    fn handle_provision_chunks(&mut self, tx_hash: B256, chunk_specs: Vec<ChunkRangeSpecifier>) {
        // Guard against duplicate NewTransaction messages — don't regress an already-tracked tx.
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

        // Register the transaction
        let tx_state = self
            .tracker
            .register(tx_hash, required_chunks.clone(), self.current_height);

        // Fetch missing chunks
        let mut fetched = 0;
        let mut missing = HashSet::new();

        for key in &required_chunks {
            if self.cache.contains(key) {
                // Already cached — just add reference
                self.cache.add_reference(key, tx_hash);
                fetched += 1;
                trace!(
                    tx_hash = %tx_hash,
                    ledger = key.ledger,
                    offset = key.offset,
                    "Chunk already cached, added reference"
                );
            } else {
                // Fetch from storage
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
                            "Chunk not found locally. P2P gossip not yet implemented — chunk will be unavailable."
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

        // Update state based on what we found
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
    fn handle_release_chunks(&mut self, tx_hash: &B256) {
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

    /// Check if chunks for a transaction are ready.
    fn handle_is_ready(&self, tx_hash: &B256) -> bool {
        self.tracker.is_ready(tx_hash)
    }

    /// Get a chunk from the cache by ledger and offset.
    ///
    /// Falls back to loading from storage on cache miss (on-demand loading).
    /// This enables block validation without pre-provisioning — the EVM's PD
    /// precompile requests chunks via `GetChunk`, and the PD service transparently
    /// loads them from storage if not already cached.
    fn handle_get_chunk(&mut self, ledger: u32, offset: u64) -> Option<Arc<Bytes>> {
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
                    offset, "Chunk not found in storage during GetChunk (not available locally)"
                );
                None
            }
            Err(e) => {
                warn!(
                    ledger,
                    offset,
                    error = %e,
                    "Error loading chunk from storage during GetChunk"
                );
                None
            }
        }
    }

    /// Batch-fetch chunks by (ledger, offset), reusing the per-key `handle_get_chunk` logic.
    /// Missing chunks are silently omitted — the precompile will return `ChunkNotFound`.
    fn handle_get_chunks_batch(&mut self, keys: Vec<(u32, u64)>) -> ChunkTable {
        let requested = keys.len();
        let mut table = HashMap::with_capacity(requested);
        for (ledger, offset) in &keys {
            if let Some(data) = self.handle_get_chunk(*ledger, *offset) {
                table.insert((*ledger, *offset), data);
            }
        }
        debug!(
            requested,
            found = table.len(),
            missing = requested - table.len(),
            ?keys,
            "GetChunksBatch: batch fetch complete"
        );
        table
    }

    fn handle_block_state_update(&mut self, height: u64) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::chunk_provider::MockChunkProvider;
    use irys_types::range_specifier::ChunkRangeSpecifier;
    use tokio::sync::mpsc;

    /// Create a PdService for testing with a mock provider.
    fn test_service() -> PdService {
        let (_tx, rx) = mpsc::unbounded_channel();
        let (_, block_state_rx) = tokio::sync::broadcast::channel(16);
        let provider = Arc::new(MockChunkProvider::new());
        let (_, shutdown) = reth::tasks::shutdown::signal();
        PdService {
            shutdown,
            msg_rx: rx,
            cache: ChunkCache::with_default_capacity(),
            tracker: ProvisioningTracker::new(),
            storage_provider: provider,
            block_state_rx,
            current_height: None,
        }
    }

    #[test]
    fn test_provision_chunks_loads_into_cache() {
        let mut service = test_service();
        let tx_hash = B256::with_last_byte(0x01);
        let specs = vec![ChunkRangeSpecifier {
            partition_index: Default::default(),
            offset: 0,
            chunk_count: 3,
        }];

        service.handle_provision_chunks(tx_hash, specs);

        // Verify chunks are in cache
        assert!(service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 0
        }));
        assert!(service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 1
        }));
        assert!(service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 2
        }));
    }

    #[test]
    fn test_release_chunks_removes_references() {
        let mut service = test_service();
        let tx_hash = B256::with_last_byte(0x01);
        let specs = vec![ChunkRangeSpecifier {
            partition_index: Default::default(),
            offset: 0,
            chunk_count: 2,
        }];

        service.handle_provision_chunks(tx_hash, specs);

        // Verify chunks are cached
        assert!(service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 0
        }));
        assert!(service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 1
        }));

        // Release
        service.handle_release_chunks(&tx_hash);

        // Chunks should be removed (no other references)
        assert!(!service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 0
        }));
        assert!(!service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 1
        }));
    }

    #[test]
    fn test_provision_chunks_shared_references() {
        let mut service = test_service();
        let tx_hash1 = B256::with_last_byte(0x01);
        let tx_hash2 = B256::with_last_byte(0x02);

        let specs = vec![ChunkRangeSpecifier {
            partition_index: Default::default(),
            offset: 0,
            chunk_count: 2,
        }];
        service.handle_provision_chunks(tx_hash1, specs.clone());
        service.handle_provision_chunks(tx_hash2, specs);

        // Release tx1 — tx2 still references them, so they should stay
        service.handle_release_chunks(&tx_hash1);
        assert!(service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 0
        }));
        assert!(service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 1
        }));

        // Release tx2 — now they should be gone
        service.handle_release_chunks(&tx_hash2);
        assert!(!service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 0
        }));
        assert!(!service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 1
        }));
    }
}
