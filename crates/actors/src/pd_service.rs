pub mod cache;
pub mod provisioning;

use cache::{ChunkCache, ChunkKey};
use irys_types::chunk_provider::{PdChunkMessage, PdChunkReceiver, RethChunkProvider};
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
/// 2. Payload building → check readiness, lock chunks during execution
/// 3. Transaction removal → release references, let LRU evict unused chunks
/// 4. Block state updates → expire stale provisioning entries
pub struct PdService {
    shutdown: Shutdown,
    msg_rx: PdChunkReceiver,
    cache: ChunkCache,
    tracker: ProvisioningTracker,
    storage_provider: Arc<dyn RethChunkProvider>,
    block_state_rx: broadcast::Receiver<BlockStateUpdated>,
    current_height: Option<u64>,
    block_tracker: HashMap<B256, Vec<ChunkKey>>,
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
            block_tracker: HashMap::new(),
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
            PdChunkMessage::Lock { tx_hash, response } => {
                let locked = self.handle_lock(&tx_hash);
                let _ = response.send(locked);
            }
            PdChunkMessage::Unlock { tx_hash } => {
                self.handle_unlock(&tx_hash);
            }
            PdChunkMessage::GetChunk {
                ledger,
                offset,
                response,
            } => {
                let chunk = self.handle_get_chunk(ledger, offset);
                let _ = response.send(chunk);
            }
            PdChunkMessage::ProvisionBlockChunks {
                block_hash,
                chunk_specs,
                response,
            } => {
                self.handle_provision_block_chunks(block_hash, chunk_specs, response);
            }
            PdChunkMessage::ReleaseBlockChunks { block_hash } => {
                self.handle_release_block_chunks(&block_hash);
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
            total = total_chunks,
            cached_chunks = self.cache.len(),
            "PD chunk provisioning complete"
        );
    }

    /// Release chunks when a transaction is removed from the mempool.
    fn handle_release_chunks(&mut self, tx_hash: &B256) {
        if let Some(tx_state) = self.tracker.remove(tx_hash) {
            // If this transaction was locked, unlock its cache entries first
            // to prevent lock_count from leaking and pinning chunks forever.
            if tx_state.state == ProvisioningState::Locked {
                self.cache.unlock_chunks(&tx_state.required_chunks);
            }

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

    /// Lock chunks for EVM execution.
    fn handle_lock(&mut self, tx_hash: &B256) -> bool {
        if !self.tracker.lock(tx_hash) {
            return false;
        }

        // If tracker had state, lock the chunks in cache
        if let Some(state) = self.tracker.get(tx_hash) {
            self.cache.lock_chunks(&state.required_chunks);
        }
        true
    }

    /// Unlock chunks after execution.
    fn handle_unlock(&mut self, tx_hash: &B256) {
        // Unlock chunks in cache before changing tracker state
        if let Some(state) = self.tracker.get(tx_hash)
            && state.state == ProvisioningState::Locked
        {
            self.cache.unlock_chunks(&state.required_chunks);
        }
        self.tracker.unlock(tx_hash);
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

    /// Handle block state update — expire stale provisioning entries and track height.
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

    /// Provision chunks needed for validating a peer block.
    /// Loads chunks from local storage into cache, pins them with block_hash as reference.
    fn handle_provision_block_chunks(
        &mut self,
        block_hash: B256,
        chunk_specs: Vec<ChunkRangeSpecifier>,
        response: tokio::sync::oneshot::Sender<Result<(), Vec<(u32, u64)>>>,
    ) {
        let required_chunks = self.specs_to_keys(&chunk_specs);
        let chunk_keys: Vec<ChunkKey> = required_chunks.into_iter().collect();

        debug!(
            block_hash = %block_hash,
            total_chunks = chunk_keys.len(),
            "Provisioning PD chunks for block validation"
        );

        let mut missing = Vec::new();

        for key in &chunk_keys {
            if self.cache.contains(key) {
                self.cache.add_reference(key, block_hash);
            } else {
                match self
                    .storage_provider
                    .get_unpacked_chunk_by_ledger_offset(key.ledger, key.offset)
                {
                    Ok(Some(chunk)) => {
                        self.cache.insert(*key, Arc::new(chunk), block_hash);
                    }
                    Ok(None) => {
                        warn!(
                            block_hash = %block_hash,
                            ledger = key.ledger,
                            offset = key.offset,
                            "Chunk not found locally for block validation"
                        );
                        missing.push((key.ledger, key.offset));
                    }
                    Err(e) => {
                        warn!(
                            block_hash = %block_hash,
                            ledger = key.ledger,
                            offset = key.offset,
                            error = %e,
                            "Failed to fetch chunk from storage for block validation"
                        );
                        missing.push((key.ledger, key.offset));
                    }
                }
            }
        }

        if missing.is_empty() {
            self.block_tracker.insert(block_hash, chunk_keys);
            let _ = response.send(Ok(()));
        } else {
            // Don't track a failed provisioning — the caller will bail and the
            // partially-loaded chunks will be cleaned up by their absence from
            // block_tracker (they still have the block_hash reference in the
            // cache, but a subsequent ReleaseBlockChunks is a no-op, and the
            // reference will be cleaned up when the cache entry is evicted via
            // LRU or when we re-provision the same block later).
            //
            // Remove references for the chunks that WERE loaded, since we won't
            // track them for later release.
            for key in &chunk_keys {
                let unreferenced = self.cache.remove_reference(key, &block_hash);
                if unreferenced {
                    self.cache.remove(key);
                }
            }
            let _ = response.send(Err(missing));
        }

        debug!(
            block_hash = %block_hash,
            cached_chunks = self.cache.len(),
            "Block chunk provisioning complete"
        );
    }

    /// Release chunks provisioned for a block after validation completes.
    fn handle_release_block_chunks(&mut self, block_hash: &B256) {
        if let Some(chunk_keys) = self.block_tracker.remove(block_hash) {
            for key in &chunk_keys {
                let unreferenced = self.cache.remove_reference(key, block_hash);
                if unreferenced {
                    self.cache.remove(key);
                }
            }
            trace!(
                block_hash = %block_hash,
                released_keys = chunk_keys.len(),
                "Released block validation chunks"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::chunk_provider::MockChunkProvider;
    use irys_types::range_specifier::ChunkRangeSpecifier;
    use tokio::sync::{mpsc, oneshot};

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
            block_tracker: HashMap::new(),
        }
    }

    #[test]
    fn test_provision_block_chunks_loads_into_cache() {
        let mut service = test_service();
        let block_hash = B256::with_last_byte(0xAA);
        let specs = vec![ChunkRangeSpecifier {
            partition_index: Default::default(),
            offset: 0,
            chunk_count: 3,
        }];

        let (resp_tx, resp_rx) = oneshot::channel();
        service.handle_provision_block_chunks(block_hash, specs, resp_tx);

        let result = resp_rx.blocking_recv().unwrap();
        assert!(
            result.is_ok(),
            "All chunks should be available from mock provider"
        );

        // Verify chunks are in cache
        let key0 = ChunkKey {
            ledger: 0,
            offset: 0,
        };
        let key1 = ChunkKey {
            ledger: 0,
            offset: 1,
        };
        let key2 = ChunkKey {
            ledger: 0,
            offset: 2,
        };
        assert!(service.cache.contains(&key0));
        assert!(service.cache.contains(&key1));
        assert!(service.cache.contains(&key2));

        // Verify block is tracked
        assert!(service.block_tracker.contains_key(&block_hash));
    }

    #[test]
    fn test_release_block_chunks_removes_references() {
        let mut service = test_service();
        let block_hash = B256::with_last_byte(0xBB);
        let specs = vec![ChunkRangeSpecifier {
            partition_index: Default::default(),
            offset: 0,
            chunk_count: 2,
        }];

        // Provision
        let (resp_tx, _) = oneshot::channel();
        service.handle_provision_block_chunks(block_hash, specs, resp_tx);

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
        service.handle_release_block_chunks(&block_hash);

        // Block tracker should be empty
        assert!(!service.block_tracker.contains_key(&block_hash));

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
    fn test_provision_block_chunks_shared_with_tx() {
        let mut service = test_service();
        let tx_hash = B256::with_last_byte(0x01);
        let block_hash = B256::with_last_byte(0xCC);

        // First, provision via a tx (simulating mempool monitor)
        let tx_specs = vec![ChunkRangeSpecifier {
            partition_index: Default::default(),
            offset: 0,
            chunk_count: 2,
        }];
        service.handle_provision_chunks(tx_hash, tx_specs);

        // Now provision same chunks for a block
        let block_specs = vec![ChunkRangeSpecifier {
            partition_index: Default::default(),
            offset: 0,
            chunk_count: 2,
        }];
        let (resp_tx, resp_rx) = oneshot::channel();
        service.handle_provision_block_chunks(block_hash, block_specs, resp_tx);

        let result = resp_rx.blocking_recv().unwrap();
        assert!(result.is_ok());

        // Release block chunks — tx still references them, so they should stay
        service.handle_release_block_chunks(&block_hash);
        assert!(service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 0
        }));
        assert!(service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 1
        }));

        // Release tx chunks — now they should be gone
        service.handle_release_chunks(&tx_hash);
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
