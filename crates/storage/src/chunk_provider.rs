use std::sync::Arc;

use irys_database::Ledger;
use irys_types::{DatabaseProvider, LedgerChunkOffset, StorageConfig};
use nodit::interval::ii;

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

    pub fn get_chunk(
        &self,
        ledger: Ledger,
        ledger_offset: LedgerChunkOffset,
    ) -> Option<(Vec<u8>, ChunkType)> {
        // Retrieve the chunk
        let storage_module =
            get_overlapped_storage_module(&self.storage_modules, ledger, ledger_offset);

        storage_module.and_then(|module| {
            let range = module.get_storage_module_range().ok()?;
            let partition_offset = (ledger_offset - range.start()) as u32;

            let chunks = module
                .read_chunks(ii(partition_offset, partition_offset))
                .ok()?;
            let chunk_info = chunks.get(&partition_offset)?;

            println!(
                "Get Chunk - Ledger: {:?} Offset: {} {}",
                ledger, ledger_offset, module.id
            );
            println!("{:?}", chunk_info);

            Some((chunk_info.0.clone(), chunk_info.1.clone()))
        })
    }
}
