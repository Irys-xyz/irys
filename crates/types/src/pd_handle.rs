//! Shared PD handle — bundles the sync chunk store and the CL→EL chunk table store.

use crate::chunk_provider::{ChunkConfig, ChunkTable, ChunkTableStore};
use alloy_primitives::B256;
use std::sync::Arc;

use crate::range_specifier::ChunkRangeSpecifier;

/// Trait abstracting PdChunkStore for cross-crate use.
/// Implemented by `PdChunkStoreHandle` in `irys-actors`.
pub trait PdStoreApi: Send + Sync + std::fmt::Debug {
    /// Check if chunks for a transaction are ready for execution.
    fn is_ready(&self, tx_hash: &B256) -> bool;

    /// Provision chunks for a new PD transaction.
    fn provision_chunks(&self, tx_hash: B256, chunk_specs: Vec<ChunkRangeSpecifier>);

    /// Release chunks when a transaction is removed from the mempool.
    fn release_chunks(&self, tx_hash: &B256);

    /// Expire stale provisioning entries at the given block height.
    fn expire_at_height(&self, height: u64);

    /// Batch-fetch chunks by `(ledger, offset)`.
    fn get_chunks_batch(&self, keys: &[(u32, u64)]) -> ChunkTable;

    /// Return the chunk configuration.
    fn config(&self) -> ChunkConfig;
}

/// Opaque handle to PD subsystem state.
/// Cloneable and cheap — all fields are Arc-wrapped.
#[derive(Debug, Clone)]
pub struct PdHandle {
    pub chunk_table_store: ChunkTableStore,
    pd_store: Arc<dyn PdStoreApi>,
}

impl PdHandle {
    pub fn new(chunk_table_store: ChunkTableStore, pd_store: Arc<dyn PdStoreApi>) -> Self {
        Self {
            chunk_table_store,
            pd_store,
        }
    }

    pub fn store(&self) -> &dyn PdStoreApi {
        &*self.pd_store
    }

    pub fn chunk_table_store(&self) -> &ChunkTableStore {
        &self.chunk_table_store
    }
}
