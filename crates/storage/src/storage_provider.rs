use std::collections::BTreeMap;

use crate::{ChunkType, StorageModules};
use eyre::eyre;
use irys_types::{
    ChunkBytes, ChunkEnum, ChunkPathHash, IrysTransactionId, LedgerChunkOffset, LedgerChunkRange,
    LedgerId, PackedChunk, PartitionChunkOffset, UnpackedChunk,
};
use nodit::{InclusiveInterval, Interval, NoditMap};

#[derive(Debug, Clone)]
/// Top-level storage abstraction
pub struct StorageProvider {
    /// Storage modules
    pub storage_modules: StorageModules,
}

pub type FullChunkMap = BTreeMap<PartitionChunkOffset, (ChunkBytes, ChunkType)>;
// pub type LedgerOffsetChunks = Vec<Option<

impl StorageProvider {
    pub fn get_by_ledger_offset(
        &self,
        ledger_id: LedgerId,
        offset_range: LedgerChunkRange,
    ) -> eyre::Result<Option<FullChunkMap>> {
        // map that tracks what intervals we've been able to find, and which we still need to
        let mut request_map: NoditMap<u64, Interval<u64>, Option<ChunkEnum>> = NoditMap::new();
        request_map
            .insert_strict(offset_range.0, None)
            .map_err(|_| eyre!("error building chunk map"))?;

        // try to find these chunks
        // TODO: look through any caches
        // search through storage modules
        for sm in self.storage_modules.iter() {
            if sm.ledger_num() != Some(ledger_id as u64) {
                continue;
            }
            let range = sm.get_storage_module_range()?;
            let intersect = match offset_range.intersection(&range.0) {
                Some(i) => i,
                None => continue,
            };

            // if !offset_range.overlaps(&range.0) {
            //     continue;
            // }

            // let part_relative_interval = sm.make_range_partition_relative(offset_range)?;

            let map = sm.read_ledger_chunks(offset_range)?;
            todo!()
        }
        Ok(None)
    }
    pub fn get_by_tx_offset(
        &self,
        tx_id: IrysTransactionId,
        offset_range: LedgerChunkRange,
    ) -> eyre::Result<Option<FullChunkMap>> {
        // try to find this packed chunk
        // TODO: look through any caches
        // search through storage modules
        // for sm in self.storage_modules.iter() {
        //     if sm.ledger_num() != Some(ledger_id as u64) {
        //         continue;
        //     }
        //     let map = sm.read_ledger_chunks(offset_range)?;
        //     return Ok(Some(map));
        // }
        // Ok(None)

        todo!()
    }

    // pub fn get_by_path_hash(
    //     &self,
    //     chunk_path_hash: ChunkPathHash,
    // ) -> eyre::Result<Option<FullChunkMap>> {
    //     // try to find the unpacked chunk
    //     // check the global chunk cache

    //     // for sm in self.storage_modules.iter() {
    //     //     if sm.ledger_num() != Some(ledger_id as u64) {
    //     //         continue;
    //     //     }

    //     // }
    //     todo!()
    // }
}

#[test]
fn test_storage_provider() -> eyre::Result<()> {
    Ok(())
}
