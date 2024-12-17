use actix::prelude::*;
use irys_database::Ledger;
use irys_storage::{ii, StorageModule};
use irys_types::{DatabaseProvider, LedgerChunkOffset, StorageConfig};
use nodit::InclusiveInterval;
use std::sync::Arc;

use crate::chunk_storage::get_overlapped_storage_module;

/// Provides chunks to actix::web front end (mostly)
#[derive(Debug)]
pub struct ChunkProviderActor {
    /// Configuration parameters for storage system
    pub storage_config: StorageConfig,
    /// Collection of storage modules for distributing chunk data
    pub storage_modules: Vec<Arc<StorageModule>>,
    /// Persistent database for storing chunk metadata and indices
    pub db: DatabaseProvider,
}

impl Actor for ChunkProviderActor {
    type Context = Context<Self>;
}

impl ChunkProviderActor {
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
}

/// Retrieves a specific chunk from a specific ledger
#[derive(Message, Debug, Clone)]
#[rtype(result = "Option<(Vec<u8>, irys_storage::ChunkType)>")]
pub struct GetChunkMessage {
    /// Ledger to read the chunk from
    pub ledger: Ledger,
    /// Ledger relative chunk offset to read
    pub offset: LedgerChunkOffset,
}

impl Handler<GetChunkMessage> for ChunkProviderActor {
    type Result = Option<(Vec<u8>, irys_storage::ChunkType)>;
    fn handle(&mut self, msg: GetChunkMessage, _ctx: &mut Context<Self>) -> Self::Result {
        // Retrieve the chunk
        let chunk_offset: LedgerChunkOffset = msg.offset;

        let storage_module =
            get_overlapped_storage_module(&self.storage_modules, msg.ledger, chunk_offset);

        storage_module.and_then(|module| {
            let range = module.get_storage_module_range().ok()?;
            let partition_offset = (chunk_offset - range.start()) as u32;

            let chunks = module
                .read_chunks(ii(partition_offset, partition_offset))
                .ok()?;
            let chunk_info = chunks.get(&partition_offset)?;

            println!(
                "Get Chunk - Ledger: {:?} Offset: {} {}",
                msg.ledger, msg.offset, module.id
            );
            println!("{:?}", chunk_info);

            Some((chunk_info.0.clone(), chunk_info.1.clone()))
        })
    }
}
