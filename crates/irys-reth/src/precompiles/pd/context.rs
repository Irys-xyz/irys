//! Shared context for PD precompile execution.

use alloy_eips::eip2930::AccessListItem;
use alloy_primitives::B256;
use bytes::Bytes;
use irys_types::chunk_provider::ChunkConfig;
use parking_lot::RwLock;
use std::sync::Arc;

pub struct PdContext {
    /// Current transaction hash being executed.
    /// Set before EVM runs a transaction, cleared after.
    tx_hash: Arc<RwLock<Option<B256>>>,
    /// Access list for the current transaction.
    /// Arc<RwLock> allows sharing within a single EVM instance.
    access_list: Arc<RwLock<Vec<AccessListItem>>>,
    /// Shared chunk data index for lock-free chunk reads.
    chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
    /// Chunk configuration (size, etc.)
    chunk_config: ChunkConfig,
}

// Default Clone uses Arc::clone to share the same access_list storage
// This is needed so precompile and EVM share the same Arc<RwLock>
impl Clone for PdContext {
    fn clone(&self) -> Self {
        Self {
            tx_hash: Arc::clone(&self.tx_hash),
            access_list: Arc::clone(&self.access_list),
            chunk_data_index: self.chunk_data_index.clone(),
            chunk_config: self.chunk_config,
        }
    }
}

impl PdContext {
    /// Create a PdContext that uses the shared ChunkDataIndex for chunk retrieval.
    pub fn new(
        chunk_config: ChunkConfig,
        chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
    ) -> Self {
        Self {
            tx_hash: Arc::new(RwLock::new(None)),
            access_list: Arc::new(RwLock::new(Vec::new())),
            chunk_data_index,
            chunk_config,
        }
    }

    /// Creates an independent clone with a NEW Arc<RwLock> for use in a new EVM instance.
    /// This ensures each EVM has its own access list storage (no cross-EVM contamination).
    #[must_use = "cloned context should be used for new EVM instance"]
    pub fn clone_for_new_evm(&self) -> Self {
        Self {
            tx_hash: Arc::new(RwLock::new(*self.tx_hash.read())),
            access_list: Arc::new(RwLock::new(self.access_list.read().clone())),
            chunk_data_index: self.chunk_data_index.clone(),
            chunk_config: self.chunk_config,
        }
    }

    /// Returns the chunk configuration.
    pub fn chunk_config(&self) -> ChunkConfig {
        self.chunk_config
    }

    /// Set the current transaction hash before EVM execution.
    pub fn set_current_tx(&self, tx_hash: B256) {
        *self.tx_hash.write() = Some(tx_hash);
    }

    /// Clear the current transaction hash after EVM execution.
    pub fn clear_current_tx(&self) {
        *self.tx_hash.write() = None;
    }

    /// Get the current transaction hash.
    pub fn current_tx(&self) -> Option<B256> {
        *self.tx_hash.read()
    }

    /// Get chunk configuration.
    pub fn config(&self) -> ChunkConfig {
        self.chunk_config
    }

    /// Get chunk from the shared ChunkDataIndex.
    ///
    /// Chunks are pre-populated in the shared ChunkDataIndex DashMap by PdService.
    /// Lock-free read directly from the index by (ledger, offset).
    pub fn get_chunk(&self, ledger: u32, offset: u64) -> eyre::Result<Option<Bytes>> {
        match self.chunk_data_index.get(&(ledger, offset)) {
            Some(data) => Ok(Some((**data).clone())),
            None => {
                tracing::warn!(ledger, offset, "chunk not found in chunk_data_index");
                Ok(None)
            }
        }
    }

    pub fn update_access_list(&self, access_list: Vec<AccessListItem>) {
        *self.access_list.write() = access_list;
    }

    pub fn read_access_list(&self) -> Vec<AccessListItem> {
        self.access_list.read().clone()
    }
}

impl std::fmt::Debug for PdContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PdContext")
            .field("tx_hash", &self.tx_hash)
            .field("access_list", &self.access_list)
            .field("chunk_config", &self.chunk_config)
            .finish()
    }
}
