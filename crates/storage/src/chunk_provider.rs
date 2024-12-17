use std::sync::Arc;

use eyre::OptionExt;
use irys_database::Ledger;
use irys_types::{Chunk, DatabaseProvider, LedgerChunkOffset, StorageConfig};

use crate::{get_storage_module_at_offset, ChunkType, StorageModule};

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
    pub fn get_chunk(
        &self,
        ledger: Ledger,
        ledger_offset: LedgerChunkOffset,
    ) -> eyre::Result<Option<Chunk>> {
        // Get basic chunk info
        let module = get_storage_module_at_offset(&self.storage_modules, ledger, ledger_offset)
            .ok_or_eyre("No storage module contains this chunk")?;
        module.generate_full_chunk(ledger_offset)
    }
}
