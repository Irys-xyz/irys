//! Shared context for PD precompile execution.

use alloy_eips::eip2930::AccessListItem;
use bytes::Bytes;
use irys_types::chunk_provider::{ChunkConfig, ChunkTable, PdChunkSender};
use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::Arc;

pub struct PdContext {
    /// Access list for the current transaction.
    /// Arc<RwLock> allows sharing within a single EVM instance.
    access_list: Arc<RwLock<Vec<AccessListItem>>>,
    /// Pre-loaded chunk data for the current block (set once, immutable during execution).
    /// Outer Arc<RwLock>: shared between IrysEvm and the precompile closure (same pattern as access_list).
    /// Inner Arc<ChunkTable>: the immutable table itself, swappable via set_chunk_table.
    chunk_table: Arc<RwLock<Arc<ChunkTable>>>,
    /// Chunk configuration (size, etc.)
    chunk_config: ChunkConfig,
    /// Optional sender for fetching chunks on demand (used during payload building).
    /// Shared via Arc so it can be set after construction.
    pd_chunk_sender: Arc<RwLock<Option<PdChunkSender>>>,
}

// Default Clone uses Arc::clone to share the same access_list storage
// This is needed so precompile and EVM share the same Arc<RwLock>
impl Clone for PdContext {
    fn clone(&self) -> Self {
        Self {
            access_list: Arc::clone(&self.access_list),
            chunk_table: Arc::clone(&self.chunk_table),
            chunk_config: self.chunk_config,
            pd_chunk_sender: Arc::clone(&self.pd_chunk_sender),
        }
    }
}

impl PdContext {
    /// Create a PdContext with an empty chunk table.
    /// The table will be populated before block execution via `set_chunk_table`.
    pub fn new(chunk_config: ChunkConfig) -> Self {
        Self {
            access_list: Arc::new(RwLock::new(Vec::new())),
            chunk_table: Arc::new(RwLock::new(Arc::new(ChunkTable::new()))),
            chunk_config,
            pd_chunk_sender: Arc::new(RwLock::new(None)),
        }
    }

    /// Creates an independent clone with a NEW Arc<RwLock> for use in a new EVM instance.
    /// This ensures each EVM has its own access list storage (no cross-EVM contamination).
    /// The chunk table is cloned as a new Arc<RwLock> wrapping a clone of the inner Arc.
    /// The pd_chunk_sender is shared (cloned Arc) so all EVMs use the same channel.
    #[must_use = "cloned context should be used for new EVM instance"]
    pub fn clone_for_new_evm(&self) -> Self {
        Self {
            access_list: Arc::new(RwLock::new(self.access_list.read().clone())),
            chunk_table: Arc::new(RwLock::new(Arc::clone(&*self.chunk_table.read()))),
            chunk_config: self.chunk_config,
            pd_chunk_sender: Arc::clone(&self.pd_chunk_sender),
        }
    }

    /// Returns the chunk configuration.
    pub fn chunk_config(&self) -> ChunkConfig {
        self.chunk_config
    }

    /// Get chunk configuration.
    pub fn config(&self) -> ChunkConfig {
        self.chunk_config
    }

    /// Replace the entire chunk table.
    pub fn set_chunk_table(&self, table: Arc<ChunkTable>) {
        *self.chunk_table.write() = table;
    }

    /// Extend the current chunk table with additional entries (for payload builder incremental loading).
    pub fn extend_chunk_table(&self, additional: ChunkTable) {
        let mut guard = self.chunk_table.write();
        let mut table = (**guard).clone();
        table.extend(additional);
        *guard = Arc::new(table);
    }

    /// Return the set of keys currently in the chunk table (for dedup in payload builder).
    pub fn chunk_table_keys(&self) -> HashSet<(u32, u64)> {
        self.chunk_table.read().keys().copied().collect()
    }

    /// Set the PD chunk sender for on-demand chunk fetching during payload building.
    pub fn set_pd_chunk_sender(&self, sender: PdChunkSender) {
        *self.pd_chunk_sender.write() = Some(sender);
    }

    /// Get a clone of the PD chunk sender if one is set.
    pub fn pd_chunk_sender(&self) -> Option<PdChunkSender> {
        self.pd_chunk_sender.read().clone()
    }

    /// Get chunk from the pre-loaded chunk table.
    ///
    /// This is a pure table lookup with no blocking I/O.
    pub fn get_chunk(&self, ledger: u32, offset: u64) -> eyre::Result<Option<Bytes>> {
        Ok(self
            .chunk_table
            .read()
            .get(&(ledger, offset))
            .map(|arc| (**arc).clone()))
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
            .field("access_list", &self.access_list)
            .field("chunk_config", &self.chunk_config)
            .finish()
    }
}
