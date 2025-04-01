use eyre::OptionExt;
use irys_database::DataLedger;
use irys_types::{
    ChunkFormat, DataRoot, LedgerChunkOffset, PackedChunk, StorageConfig, TxChunkOffset,
};
use std::sync::Arc;

use tracing::debug;

use crate::{checked_add_i32_u64, get_storage_module_at_offset, StorageModule};
use base58::ToBase58;

/// Provides chunks to `actix::web` front end (mostly)
#[derive(Debug, Clone)]
pub struct ChunkProvider {
    /// Configuration parameters for storage system
    pub storage_config: StorageConfig,
    /// Collection of storage modules for distributing chunk data
    pub storage_modules: Vec<Arc<StorageModule>>,
}

impl ChunkProvider {
    /// Creates a new chunk storage actor
    pub const fn new(
        storage_config: StorageConfig,
        storage_modules: Vec<Arc<StorageModule>>,
    ) -> Self {
        Self {
            storage_config,
            storage_modules,
        }
    }

    /// Retrieves a chunk from a ledger
    pub fn get_chunk_by_ledger_offset(
        &self,
        ledger: DataLedger,
        ledger_offset: LedgerChunkOffset,
    ) -> eyre::Result<Option<PackedChunk>> {
        // Get basic chunk info
        let module = get_storage_module_at_offset(&self.storage_modules, ledger, ledger_offset)
            .ok_or_eyre("No storage module contains this chunk")?;
        module.generate_full_chunk_ledger_offset(ledger_offset)
    }

    /// Retrieves a chunk by [`DataRoot`]
    pub fn get_chunk_by_data_root(
        &self,
        ledger: DataLedger,
        data_root: DataRoot,
        data_tx_offset: TxChunkOffset,
    ) -> eyre::Result<Option<ChunkFormat>> {
        // TODO: read from the cache

        debug!(
            "getting ledger: {:?}, data_root: {}, offset: {}",
            &ledger,
            &data_root.0.to_base58(),
            &data_tx_offset
        );
        // map hashes to SMs
        let sms = self
            .storage_modules
            .iter()
            .filter(|sm| {
                sm.partition_assignment
                    .and_then(|sm| sm.ledger_id)
                    .map_or(false, |ledger_id| ledger_id == ledger as u32)
            })
            .collect::<Vec<_>>();

        for sm in sms {
            let start_offsets1 = sm.collect_start_offsets(data_root)?;
            let offsets = start_offsets1
                .0
                .iter()
                .map(|mapped_start| *mapped_start + (*data_tx_offset as i32))
                .collect::<Vec<_>>();

            for part_relative_offset in offsets {
                // try other offsets and sm's if we get an Error or a None
                if let Ok(Some(r)) = sm.generate_full_chunk(part_relative_offset.into()) {
                    return Ok(Some(ChunkFormat::Packed(r)));
                }
            }
        }

        Ok(None)
    }

    pub fn get_ledger_offsets_for_data_root(
        &self,
        ledger: DataLedger,
        data_root: DataRoot,
    ) -> eyre::Result<Option<Vec<u64>>> {
        debug!(
            "getting ledger: {:?}, data_root: {}",
            &ledger,
            &data_root.0.to_base58(),
        );

        // get all SMs for this ledger
        let sms = self
            .storage_modules
            .iter()
            .filter(|sm| {
                sm.partition_assignment
                    .and_then(|sm| sm.ledger_id)
                    .map_or(false, |ledger_id| ledger_id == ledger as u32)
            })
            .collect::<Vec<_>>();

        // find a SM that contains this data root, return the start_offsets once we find it
        for sm in sms {
            let sm_range_start = sm.get_storage_module_ledger_range().unwrap().start();
            let start_offsets = sm.collect_start_offsets(data_root)?;
            let mapped_offsets = start_offsets
                .0
                .iter()
                .filter_map(|so| {
                    checked_add_i32_u64(**so, sm_range_start.into()) // translate into ledger-relative space
                })
                .collect::<Vec<_>>();

            if !mapped_offsets.is_empty() {
                return Ok(Some(mapped_offsets));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use crate::StorageModuleInfo;

    use super::*;
    use irys_packing::unpack_with_entropy;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{
        irys::IrysSigner, ledger_chunk_offset_ii, partition::PartitionAssignment,
        partition_chunk_offset_ie, Base64, Config, DataTransactionLedger, LedgerChunkRange,
        PartitionChunkOffset, UnpackedChunk,
    };
    use nodit::interval::{ie, ii};
    use rand::Rng as _;

    #[test]
    fn get_by_data_tx_offset_test() -> eyre::Result<()> {
        let testnet_config = Config {
            num_writes_before_sync: 1,
            chunk_size: 32,
            num_chunks_in_partition: 100,
            ..Config::testnet()
        };
        let infos = vec![StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment::default()),
            submodules: vec![
                (partition_chunk_offset_ie!(0, 50), "hdd0".into()),
                (partition_chunk_offset_ie!(50, 100), "hdd1".into()),
            ],
        }];

        let tmp_dir = setup_tracing_and_temp_dir(Some("get_by_data_tx_offset_test"), false);
        let base_path = tmp_dir.path().to_path_buf();

        // Override the default StorageModule config for testing
        let config = StorageConfig::new(&testnet_config);

        // Create a StorageModule with the specified submodules and config
        let storage_module_info = &infos[0];
        let storage_module = StorageModule::new(&base_path, storage_module_info, config.clone())?;

        let data_size = (config.chunk_size as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        let irys = IrysSigner::random_signer(&testnet_config);
        let tx = irys.create_transaction(data_bytes.clone(), None).unwrap();
        let tx = irys.sign_transaction(tx).unwrap();

        // fake the tx_path
        // Create a tx_root (and paths) from the tx
        let (_tx_root, proofs) = DataTransactionLedger::merklize_tx_root(&vec![tx.header.clone()]);

        let tx_path = proofs[0].proof.clone();

        // let data_root = H256::zero();
        let data_root = tx.header.data_root;
        // Pack the storage module

        storage_module.pack_with_zeros();

        let chunk_range = ledger_chunk_offset_ii!(49, 51);
        let _ = storage_module.index_transaction_data(
            tx_path,
            data_root,
            LedgerChunkRange(chunk_range),
            tx.header.data_size,
        );

        let mut unpacked_chunks = vec![];
        for (tx_chunk_offset, chunk_node) in tx.chunks.iter().enumerate() {
            let min = chunk_node.min_byte_range;
            let max = chunk_node.max_byte_range;
            // let offset = tx.proofs[tx_chunk_offset].offset as u32;
            let data_path = Base64(tx.proofs[tx_chunk_offset].proof.clone());
            // let key: H256 = hash_sha256(&data_path.0).unwrap().into();
            let chunk_bytes = Base64(data_bytes[min..max].to_vec());
            let chunk = UnpackedChunk {
                data_root,
                data_size: data_size as u64,
                data_path: data_path.clone(),
                bytes: chunk_bytes.clone(),
                tx_offset: TxChunkOffset::from(tx_chunk_offset as u32),
            };
            storage_module.write_data_chunk(&chunk)?;
            unpacked_chunks.push(chunk);
        }
        storage_module.sync_pending_chunks()?;

        let chunk_provider = ChunkProvider::new(config.clone(), vec![Arc::new(storage_module)]);

        for original_chunk in unpacked_chunks {
            let chunk = chunk_provider
                .get_chunk_by_data_root(DataLedger::Publish, data_root, original_chunk.tx_offset)?
                .unwrap();
            let packed_chunk = chunk.as_packed().unwrap();

            let unpacked_data = unpack_with_entropy(
                &packed_chunk,
                vec![0u8; config.chunk_size as usize],
                config.chunk_size as usize,
            );
            let unpacked_chunk = UnpackedChunk {
                data_root: packed_chunk.data_root,
                data_size: packed_chunk.data_size,
                data_path: packed_chunk.data_path.clone(),
                bytes: Base64(unpacked_data),
                tx_offset: packed_chunk.tx_offset,
            };
            assert_eq!(original_chunk, unpacked_chunk);
            // let d_slice = data_bytes[start..start + chunk_size].to_vec();
        }

        Ok(())
    }
}
