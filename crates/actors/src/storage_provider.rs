// this is here due to dependency cycle issues if it were kept in the storage crate

use std::{collections::HashMap, sync::Arc};

use crate::chunk_storage::{ChunkStorageActor, ReadChunks, WriteChunks};
use actix::Addr;
use eyre::eyre;
use irys_types::{block_production::PartitionId, ChunkBin, ChunkState, Interval, IntervalState};

#[derive(Debug, Clone)]
/// Storage provider struct - top level structure, used to interact with other storage components
pub struct StorageProvider {
    /// Map of partition IDs to storage actors
    partition_providers: HashMap<PartitionId, Addr<ChunkStorageActor>>,
}

impl StorageProvider {
    /// create a new storage provider
    pub fn new(partition_providers: HashMap<PartitionId, Addr<ChunkStorageActor>>) -> Self {
        Self {
            partition_providers,
        }
    }
    /// read an interval of chunks from a partition
    pub async fn read_chunks(
        &self,
        partition_id: PartitionId,
        read_interval: Interval<u32>,
        expected_state: Option<ChunkState>,
    ) -> eyre::Result<Arc<Vec<ChunkBin>>> {
        let addr = self.get_provider_address(partition_id)?;

        addr.send(ReadChunks {
            interval: read_interval,
            expected_state,
        })
        .await?
    }
    /// get the chunk storage actor address associated with a partition_id
    pub fn get_provider_address(
        &self,
        partition_id: PartitionId,
    ) -> eyre::Result<Addr<ChunkStorageActor>> {
        Ok(self
            .partition_providers
            .get(&partition_id)
            .ok_or(eyre!(
                "Can't find storage provider for partition {:?}",
                &partition_id
            ))?
            .clone())
    }

    /// write a vec of chunks to an interval in a partition
    pub async fn write_chunks(
        &self,
        partition_id: PartitionId,
        write_interval: Interval<u32>,
        chunks: Vec<ChunkBin>,
        expected_state: ChunkState,
        new_state: IntervalState,
    ) -> eyre::Result<()> {
        let addr = self.get_provider_address(partition_id)?;

        addr.send(WriteChunks {
            interval: write_interval,
            chunks,
            expected_state,
            new_state,
        })
        .await?
    }
}
