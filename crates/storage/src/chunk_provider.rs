use std::sync::Arc;

use irys_database::Ledger;
use irys_types::{Chunk, DatabaseProvider, LedgerChunkOffset, StorageConfig};

use crate::{get_overlapped_storage_module, ChunkType, StorageModule};

/// Provides chunks to actix::web front end (mostly)
#[derive(Debug)]
pub struct ChunkProvider {
    /// Configuration parameters for storage system
    pub storage_config: StorageConfig,
    /// Collection of storage modules for distributing chunk data
    pub storage_modules: Vec<Arc<StorageModule>>,
    /// Persistent database for storing chunk metadata and indices
    pub db: DatabaseProvider,
}

impl ChunkProvider {
    /// Creates a new chunk storage actor
    pub fn new(
        storage_config: StorageConfig,
        storage_modules: Vec<Arc<StorageModule>>,
        db: DatabaseProvider,
    ) -> Self {
        Self {
            storage_config,
            storage_modules,
            db,
        }
    }

    /// Retrieves a chunk from a ledger
    pub fn get_chunk(&self, ledger: Ledger, ledger_offset: LedgerChunkOffset) -> Option<Chunk> {
        // Get basic chunk info
        let module = get_overlapped_storage_module(&self.storage_modules, ledger, ledger_offset)?;
        module.get_wrapped_chunk(ledger_offset)
    }
}
