use eyre::OptionExt;
use irys_database::Ledger;
use irys_types::{
    ChunkFormat, DataRoot, DatabaseProvider, LedgerChunkOffset, PackedChunk, StorageConfig,
    TxRelativeChunkOffset,
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
    /// Persistent database for storing chunk metadata and indices
    pub db: DatabaseProvider,
}

impl ChunkProvider {
    /// Creates a new chunk storage actor
    pub const fn new(
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
    pub fn get_chunk_by_ledger_offset(
        &self,
        ledger: Ledger,
        ledger_offset: LedgerChunkOffset,
    ) -> eyre::Result<Option<PackedChunk>> {
        // Get basic chunk info
        let module = get_storage_module_at_offset(&self.storage_modules, ledger, ledger_offset)
            .ok_or_eyre("No storage module contains this chunk")?;
        module.generate_full_chunk(ledger_offset)
    }

    /// Retrieves a chunk by [`DataRoot`]
    pub fn get_chunk_by_data_root(
        &self,
        ledger: Ledger,
        data_root: DataRoot,
        data_tx_offset: TxRelativeChunkOffset,
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
                    .and_then(|sm| sm.ledger_num)
                    .map_or(false, |ledger_num| ledger_num == ledger as u64)
            })
            .collect::<Vec<_>>();

        for sm in sms {
            let sm_range_start = sm.get_storage_module_range().unwrap().start();
            let start_offsets1 = sm.collect_start_offsets(data_root)?;
            let offsets = start_offsets1
                .0
                .iter()
                .filter_map(|so| {
                    checked_add_i32_u64(*so, sm_range_start) // translate into ledger-relative space
                    .map(|mapped_start| mapped_start + (data_tx_offset as u64))
                })
                .collect::<Vec<_>>();

            for ledger_relative_offset in offsets {
                // try other offsets and sm's if we get an Error or a None
                // TODO: if we keep this resolver, make generate_full_chunk more modular so we can pass in work we've already done (getting the ledger relative offset, etc)
                if let Ok(Some(r)) = sm.generate_full_chunk(ledger_relative_offset) {
                    return Ok(Some(ChunkFormat::Packed(r)));
                }
            }
        }

        Ok(None)
    }

    pub fn get_ledger_relative_starts_for_data_root(
        &self,
        ledger: Ledger,
        data_root: DataRoot,
    ) -> eyre::Result<Option<Vec<u64>>> {
        debug!(
            "getting ledger: {:?}, data_root: {}",
            &ledger,
            &data_root.0.to_base58(),
        );
        // map hashes to SMs
        let sms = self
            .storage_modules
            .iter()
            .filter(|sm| {
                sm.partition_assignment
                    .and_then(|sm| sm.ledger_num)
                    .map_or(false, |ledger_num| ledger_num == ledger as u64)
            })
            .collect::<Vec<_>>();

        for sm in sms {
            let sm_range_start = sm.get_storage_module_range().unwrap().start();
            let start_offsets = sm.collect_start_offsets(data_root)?;
            let mapped_offsets = start_offsets
                .0
                .iter()
                .filter_map(|so| {
                    checked_add_i32_u64(*so, sm_range_start) // translate into ledger-relative space
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
    use super::*;
    use crate::{initialize_storage_files, StorageModuleInfo};
    use irys_database::{open_or_create_db, tables::IrysTables};
    use irys_packing::unpack_with_entropy;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{
        irys::IrysSigner, partition::PartitionAssignment, Base64, LedgerChunkRange,
        TransactionLedger, UnpackedChunk,
    };
    use nodit::interval::{ie, ii};
    use rand::Rng as _;

    #[test]
    fn get_by_data_tx_offset_test() -> eyre::Result<()> {
        let infos = vec![StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment::default()),
            submodules: vec![
                (ie(0, 50), "hdd0".to_string()),
                (ie(50, 100), "hdd1".to_string()),
            ],
        }];

        let tmp_dir = setup_tracing_and_temp_dir(Some("get_by_data_tx_offset_test"), false);
        let base_path = tmp_dir.path().to_path_buf();
        let db = open_or_create_db(tmp_dir, IrysTables::ALL, None).unwrap();
        let arc_db = DatabaseProvider(Arc::new(db));
        initialize_storage_files(&base_path, &infos)?;

        // Override the default StorageModule config for testing
        let config = StorageConfig {
            min_writes_before_sync: 1,
            chunk_size: 32,
            num_chunks_in_partition: 100,
            ..Default::default()
        };

        // Create a StorageModule with the specified submodules and config
        let storage_module_info = &infos[0];
        let storage_module = StorageModule::new(&base_path, storage_module_info, config.clone())?;

        let data_size = (config.chunk_size as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        let irys = IrysSigner::random_signer_with_chunk_size(config.chunk_size);
        let tx = irys.create_transaction(data_bytes.clone(), None).unwrap();
        let tx = irys.sign_transaction(tx).unwrap();

        // fake the tx_path
        // Create a tx_root (and paths) from the tx
        let (_tx_root, proofs) = TransactionLedger::merklize_tx_root(&vec![tx.header.clone()]);

        let tx_path = proofs[0].proof.clone();

        // let data_root = H256::zero();
        let data_root = tx.header.data_root;
        // Pack the storage module

        storage_module.pack_with_zeros();

        let chunk_range = ii(49, 51);
        let _ = storage_module.index_transaction_data(
            tx_path,
            data_root,
            LedgerChunkRange(chunk_range),
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
                tx_offset: tx_chunk_offset as u32,
            };
            storage_module.write_data_chunk(&chunk)?;
            unpacked_chunks.push(chunk);
        }
        storage_module.sync_pending_chunks()?;

        let chunk_provider =
            ChunkProvider::new(config.clone(), vec![Arc::new(storage_module)], arc_db);

        for original_chunk in unpacked_chunks {
            let chunk = chunk_provider
                .get_chunk_by_data_root(Ledger::Publish, data_root, original_chunk.tx_offset)?
                .unwrap();
            // let chunk_size = config.chunk_size as usize;
            // let start = chunk_offset as usize * chunk_size;
            let packed_chunk = chunk.as_packed().unwrap();

            // let unpacked_chunk = unpack(
            //     &packed_chunk,
            //     config.entropy_packing_iterations,
            //     config.chunk_size.try_into().unwrap(),
            // );

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
