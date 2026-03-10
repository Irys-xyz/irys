//! Shared context for PD precompile execution.

use alloy_eips::eip2930::AccessListItem;
use alloy_primitives::B256;
use bytes::Bytes;
use irys_types::chunk_provider::{ChunkConfig, RethChunkProvider};
use parking_lot::RwLock;
use std::sync::Arc;

/// Chunk source for PD precompile - either from shared index or direct storage.
enum ChunkSource {
    /// Fetch chunks via shared index (payload building with cached chunks).
    Manager {
        chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
    },
    /// Fetch chunks directly from storage (for block validation).
    Storage(Arc<dyn RethChunkProvider>),
}

impl Clone for ChunkSource {
    fn clone(&self) -> Self {
        match self {
            Self::Manager { chunk_data_index } => Self::Manager {
                chunk_data_index: chunk_data_index.clone(),
            },
            Self::Storage(provider) => Self::Storage(Arc::clone(provider)),
        }
    }
}

pub struct PdContext {
    /// Current transaction hash being executed.
    /// Set before EVM runs a transaction, cleared after.
    tx_hash: Arc<RwLock<Option<B256>>>,
    /// Access list for the current transaction.
    /// Arc<RwLock> allows sharing within a single EVM instance.
    access_list: Arc<RwLock<Vec<AccessListItem>>>,
    /// Where to get chunks from
    chunk_source: ChunkSource,
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
            chunk_source: self.chunk_source.clone(),
            chunk_config: self.chunk_config,
        }
    }
}

impl PdContext {
    /// Create a PdContext that uses the shared ChunkDataIndex for chunk retrieval.
    /// Use this for payload building where chunks are pre-cached.
    pub fn new_with_manager(
        chunk_config: ChunkConfig,
        chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
    ) -> Self {
        Self {
            tx_hash: Arc::new(RwLock::new(None)),
            access_list: Arc::new(RwLock::new(Vec::new())),
            chunk_source: ChunkSource::Manager { chunk_data_index },
            chunk_config,
        }
    }

    /// Create a PdContext that fetches chunks directly from storage.
    /// Use this for block validation where we don't have a PdChunkManager.
    pub fn new(chunk_provider: Arc<dyn RethChunkProvider>) -> Self {
        let chunk_config = chunk_provider.config();
        Self {
            tx_hash: Arc::new(RwLock::new(None)),
            access_list: Arc::new(RwLock::new(Vec::new())),
            chunk_source: ChunkSource::Storage(chunk_provider),
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
            chunk_source: self.chunk_source.clone(),
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

    /// Get chunk from the appropriate source (shared index or direct storage).
    ///
    /// When using manager mode (for payload building):
    /// - Chunks are pre-populated in the shared ChunkDataIndex DashMap by PdService
    /// - Lock-free read directly from the index by (ledger, offset)
    ///
    /// When using storage mode (for block validation):
    /// - Fetches directly from storage provider
    pub fn get_chunk(&self, ledger: u32, offset: u64) -> eyre::Result<Option<Bytes>> {
        match &self.chunk_source {
            ChunkSource::Manager { chunk_data_index } => {
                match chunk_data_index.get(&(ledger, offset)) {
                    Some(data) => Ok(Some((**data).clone())),
                    None => {
                        tracing::warn!(ledger, offset, "Chunk not found in shared index");
                        Ok(None)
                    }
                }
            }
            ChunkSource::Storage(provider) => {
                provider.get_unpacked_chunk_by_ledger_offset(ledger, offset)
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
