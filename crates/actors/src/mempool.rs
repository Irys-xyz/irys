use actix::{Actor, Context, Handler, Message};
use eyre::eyre;
use irys_database::db_cache::{data_size_to_chunk_count, CachedChunk};
use irys_database::tables::{CachedChunks, CachedChunksIndex, IngressProofs};
use irys_storage::StorageModuleVec;
use irys_types::ingress::generate_ingress_proof_tree;
use irys_types::irys::IrysSigner;
use irys_types::{
    app_state::DatabaseProvider, chunk::UnpackedChunk, hash_sha256, validate_path,
    IrysTransactionHeader, CHUNK_SIZE, H256,
};
use irys_types::{Address, DataChunks};
use irys_types::{DataRoot, StorageConfig};
use reth::tasks::TaskExecutor;
use reth::tasks::TaskManager;
use reth_db::cursor::DbCursorRO;
use reth_db::cursor::DbDupCursorRO;
use reth_db::transaction::DbTx;
use reth_db::transaction::DbTxMut;
use reth_db::Database;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fmt::Display;
use std::sync::Arc;
use tracing::{error, info};

use crate::block_producer::BlockConfirmedMessage;
/// The Mempool oversees pending transactions and validation of incoming tx.
#[derive(Debug)]
pub struct MempoolActor {
    db: DatabaseProvider,
    /// Temporary mempool stubs - will replace with proper data models - dmac
    valid_tx: BTreeMap<H256, IrysTransactionHeader>,
    /// task_exec is used to spawn background jobs on reth's MT tokio runtime
    /// instead of the actor executor runtime, while also providing some QoL
    task_exec: TaskExecutor,
    /// The miner's signer instance, used to sign ingress proofs
    signer: IrysSigner,
    invalid_tx: Vec<H256>,
    storage_config: StorageConfig,
    storage_modules: StorageModuleVec,
}

impl Actor for MempoolActor {
    type Context = Context<Self>;
}

impl MempoolActor {
    /// Create a new instance of the mempool actor passing in a reference
    /// counted reference to a DatabaseEnv, a copy of reth's task executor and the miner's signer
    pub fn new(
        db: DatabaseProvider,
        task_exec: TaskExecutor,
        signer: IrysSigner,
        storage_config: StorageConfig,
        storage_modules: StorageModuleVec,
    ) -> Self {
        Self {
            db,
            valid_tx: BTreeMap::new(),
            invalid_tx: Vec::new(),
            signer,
            task_exec,
            storage_config,
            storage_modules,
        }
    }
}

/// Message for when a new TX is discovered by the node, either though
/// synchronization with peers, or by a user posting the tx.
#[derive(Message, Debug)]
#[rtype(result = "Result<(),TxIngressError>")]
pub struct TxIngressMessage(pub IrysTransactionHeader);

impl TxIngressMessage {
    fn into_inner(self) -> IrysTransactionHeader {
        self.0
    }
}

/// Reasons why Transaction Ingress might fail
#[derive(Debug)]
pub enum TxIngressError {
    /// The transaction's signature is invalid
    InvalidSignature,
    /// The account does not have enough tokens to fund this transaction
    Unfunded,
    /// This transaction id is already in the cache
    Skipped,
}

/// Message for when a new chunk is discovered by the node, either though
/// synchronization with peers, or by a user posting the chunk.
#[derive(Message, Debug)]
#[rtype(result = "Result<(),ChunkIngressError>")]
pub struct ChunkIngressMessage(pub UnpackedChunk);

impl ChunkIngressMessage {
    fn into_inner(self) -> UnpackedChunk {
        self.0
    }
}

/// Reasons why Transaction Ingress might fail
#[derive(Debug)]
pub enum ChunkIngressError {
    /// The data_path/proof provided with the chunk data is invalid
    InvalidProof,
    /// The data hash does not match the chunk data
    InvalidDataHash,
    /// Only the last chunk in a data_root tree can be less than CHUNK_SIZE
    InvalidChunkSize,
    /// Some database error occurred when reading or writing the chunk
    DatabaseError,
    // Catch-all variant for other errors.
    Other(String),
}

impl ChunkIngressError {
    /// Returns an other error with the given message.
    pub fn other(err: impl Into<String>) -> Self {
        Self::Other(err.into())
    }
    /// Allows converting an error that implements Display into an Other error
    pub fn other_display(err: impl Display) -> Self {
        Self::Other(err.to_string())
    }
}

impl Handler<TxIngressMessage> for MempoolActor {
    type Result = Result<(), TxIngressError>;

    fn handle(&mut self, tx_msg: TxIngressMessage, _ctx: &mut Context<Self>) -> Self::Result {
        let tx = &tx_msg.0;

        // Early out if we already know about this transaction
        if self.invalid_tx.contains(&tx.id) || self.valid_tx.contains_key(&tx.id) {
            // Skip tx reprocessing if already verified (valid or invalid) to prevent
            // CPU-intensive signature verification spam attacks
            return Err(TxIngressError::Skipped);
        }

        // Validate the transaction signature
        if tx.is_signature_valid() {
            println!("Signature is valid");
            self.valid_tx.insert(tx.id, tx.clone());
        } else {
            self.invalid_tx.push(tx.id);
            println!("Signature is NOT valid");
            return Err(TxIngressError::InvalidSignature);
        }

        // TODO: Check if the signer has funds to post the tx
        //return Err(TxIngressError::Unfunded);

        // Cache the data_root in the database
        let _ = self.db.update_eyre(|db_tx| {
            irys_database::cache_data_root(db_tx, &tx)?;
            irys_database::insert_tx_header(db_tx, &tx)?;
            Ok(())
        });

        Ok(())
    }
}

impl Handler<ChunkIngressMessage> for MempoolActor {
    type Result = Result<(), ChunkIngressError>;

    fn handle(&mut self, chunk_msg: ChunkIngressMessage, _ctx: &mut Context<Self>) -> Self::Result {
        // TODO: maintain a shared read transaction so we have read isolation
        let chunk: UnpackedChunk = chunk_msg.0;

        // Check to see if we have a cached data_root for this chunk
        let read_tx = self.db.tx().map_err(|_| ChunkIngressError::DatabaseError)?;

        let cached_data_root =
            irys_database::cached_data_root_by_data_root(&read_tx, chunk.data_root)
                .map_err(|_| ChunkIngressError::DatabaseError)? // Convert DatabaseError to ChunkIngressError
                .ok_or(ChunkIngressError::InvalidDataHash)?; // Handle None case by converting it to an error

        // Next validate the data_path/proof for the chunk, linking
        // data_root->chunk_hash
        let root_hash = chunk.data_root.0;
        let target_offset = chunk.byte_offset(self.storage_config.chunk_size) as u128;
        let path_buff = &chunk.data_path;

        println!(
            "chunk_offset:{} data_size:{} offset:{}",
            chunk.tx_offset, chunk.data_size, target_offset
        );

        let path_result = validate_path(root_hash, path_buff, target_offset)
            .map_err(|_| ChunkIngressError::InvalidProof)?;

        // Validate that the data_size for this chunk matches the data_size
        // recorded in the transaction header.
        if cached_data_root.data_size != chunk.data_size {
            error!(
                "InvalidChunkSize: expected: {} got:{}",
                cached_data_root.data_size, chunk.data_size
            );
            return Err(ChunkIngressError::InvalidChunkSize);
        }

        // Use data_size to identify and validate that only the last chunk
        // can be less than chunk_size
        let chunk_len = chunk.bytes.len() as u64;

        // TODO: Mark the data_root as invalid if the chunk is an incorrect size
        // Someone may have created a data_root that seemed valid, but if the
        // data_path is valid but the chunk size doesn't mach the protocols
        // consensus size, then the data_root is actually invalid and no future
        // chunks from that data_root should be ingressed.
        let chunk_size = self.storage_config.chunk_size;

        // Is this chunk index any of the chunks before the last in the tx?
        let num_chunks_in_tx = cached_data_root.data_size.div_ceil(chunk_size);
        if (chunk.tx_offset as u64) < num_chunks_in_tx - 1 {
            // Ensure prefix chunks are all exactly chunk_size
            if chunk_len != chunk_size {
                error!(
                    "InvalidChunkSize: incomplete not last chunk, chunk index: {} chunklen {}",
                    chunk.chunk_index, chunk_len
                );
                return Err(ChunkIngressError::InvalidChunkSize);
            }
        } else {
            // Ensure the last chunk is no larger than chunk_size
            if chunk_len > chunk_size {
                error!(
                    "InvalidChunkSize: chunk bigger than max. chunk size, chunk index: {} chunklen {}",
                    chunk.chunk_index, chunk_len
                );
                return Err(ChunkIngressError::InvalidChunkSize);
            }
        }

        if path_result.leaf_hash
            != hash_sha256(&chunk.bytes.0).map_err(|_| ChunkIngressError::InvalidDataHash)?
        {
            return Err(ChunkIngressError::InvalidDataHash);
        }
        // Check that the leaf hash on the data_path matches the chunk_hash

        // TODO: fix all these unwraps!
        // Finally write the chunk to CachedChunks, this will succeed even if the chunk is one that's already inserted

        self.db
            .update_eyre(|tx| irys_database::cache_chunk(tx, &chunk))
            .map_err(|_| ChunkIngressError::DatabaseError)?;

        for sm in &self.storage_modules {
            if sm.get_write_offsets(&chunk).unwrap_or(vec![]).len() != 0 {
                info!(target: "irys::mempool::chunk_ingress", "Writing chunk with offset {} for data_root {} to sm {}", &chunk.tx_offset, &chunk.data_root, &sm.id );
                sm.write_data_chunk(&chunk)
                    .map_err(|_| ChunkIngressError::Other("Internal error".to_owned()))?;
            }
        }

        // ==== INGRESS PROOFS ====
        let root_hash: H256 = root_hash.into();

        // check if we have generated an ingress proof for this tx already
        // TODO: hook into whatever manages ingress proofs
        if read_tx
            .get::<IngressProofs>(root_hash)
            .map_err(|_| ChunkIngressError::DatabaseError)?
            .is_some()
        {
            info!(
                "We've already generated an ingress proof for data root {}",
                &root_hash
            );
            return Ok(());
        };

        // check if we have all the chunks for this tx
        let read_tx = self.db.tx().map_err(|_| ChunkIngressError::DatabaseError)?;

        let mut cursor = read_tx
            .cursor_dup_read::<CachedChunksIndex>()
            .map_err(|_| ChunkIngressError::DatabaseError)?;
        // get the number of dupsort values (aka the number of chunks)
        // this ASSUMES that the index isn't corrupt (no double values etc)
        // the ingress proof generation task does a more thorough check
        let chunk_count = cursor
            .dup_count(root_hash)
            .map_err(|_| ChunkIngressError::DatabaseError)?
            .ok_or(ChunkIngressError::DatabaseError)?;

        // data size is the offset of the last chunk
        // add one as index is 0-indexed
        let expected_chunk_count =
            data_size_to_chunk_count(cached_data_root.data_size, chunk_size).unwrap();

        if chunk_count == expected_chunk_count {
            // we *should* have all the chunks
            // dispatch a ingress proof task
            let db1 = self.db.clone();
            let signer1 = self.signer.clone();
            self.task_exec.spawn_blocking(async move {
                generate_ingress_proof(
                    db1,
                    root_hash,
                    cached_data_root.data_size,
                    chunk_size,
                    signer1,
                )
                // TODO: handle results instead of unwrapping
                .unwrap();
            });
        }

        Ok(())
    }
}

// Message for getting txs for block building
#[derive(Message, Debug)]
#[rtype(result = "Vec<IrysTransactionHeader>")]
pub struct GetBestMempoolTxs;

impl Handler<GetBestMempoolTxs> for MempoolActor {
    type Result = Vec<IrysTransactionHeader>;

    fn handle(&mut self, msg: GetBestMempoolTxs, ctx: &mut Self::Context) -> Self::Result {
        self.valid_tx
            .iter()
            .take(10)
            .map(|(_, header)| header.clone())
            .collect()
    }
}

impl Handler<BlockConfirmedMessage> for MempoolActor {
    type Result = ();
    fn handle(&mut self, msg: BlockConfirmedMessage, _ctx: &mut Context<Self>) -> Self::Result {
        // Access the block header through msg.0
        let block = &msg.0;
        let data_tx = &msg.1;

        // Loop through the storage tx in each ledger
        for tx in data_tx.iter() {
            // Remove them from the pending valid_tx pool
            self.valid_tx.remove(&tx.id);
        }

        info!(
            "Removing confirmed tx - Block height: {} num tx: {}",
            block.height,
            data_tx.len()
        );
    }
}

/// Generates an ingress proof for a specific data_root
/// pulls required data from all sources
pub fn generate_ingress_proof(
    db: DatabaseProvider,
    data_root: DataRoot,
    size: u64,
    chunk_size: u64,
    signer: IrysSigner,
) -> eyre::Result<()> {
    // load the chunks from the DB
    // TODO: for now we assume the chunks all all in the DB chunk cache
    // in future, we'll need access to whatever unified storage provider API we have to get chunks
    // regardless of actual location
    // TODO: allow for "streaming" the tree chunks, instead of having to read them all into memory
    let ro_tx = db.tx()?;
    let mut dup_cursor = ro_tx.cursor_dup_read::<CachedChunksIndex>()?;
    // start from first duplicate entry for this root_hash
    let dup_walker = dup_cursor.walk_dup(Some(data_root), None)?;
    // we need to validate that the index is valid
    // we do this by constructing a set over the chunk hashes, checking if we've seen this hash before
    // if we have, we *must* error
    let mut set = HashSet::<H256>::new();
    let expected_chunk_count = data_size_to_chunk_count(size, chunk_size).unwrap();
    let mut data: DataChunks = Vec::with_capacity(expected_chunk_count as usize);
    let mut data_size: u64 = 0;
    for entry in dup_walker {
        let (root_hash2, index_entry) = entry?;
        // make sure we haven't traversed into the wrong key
        assert_eq!(data_root, root_hash2);

        let chunk_path_hash = index_entry.meta.chunk_path_hash;
        if set.contains(&chunk_path_hash) {
            return Err(eyre!(
                "Chunk with hash {} has been found twice for index entry {} of data_root {}",
                &chunk_path_hash,
                &index_entry.index,
                &data_root
            ));
        }
        set.insert(chunk_path_hash);

        let chunk = ro_tx
            .get::<CachedChunks>(index_entry.meta.chunk_path_hash)?
            .expect(
                &format!(
                    "unable to get chunk {} for data root {} from DB",
                    chunk_path_hash, data_root
                )
                .as_str(),
            );
        // TODO validate chunk length
        let chunk_bin = chunk.chunk.unwrap().0;
        data_size += chunk_bin.len() as u64;
        data.push(chunk_bin);
    }
    assert_eq!(data.len() as u32, expected_chunk_count);
    assert_eq!(data_size, size);

    // generate the ingress proof hash
    let proof = irys_types::ingress::generate_ingress_proof(signer, data_root, data)?;
    info!(
        "generated ingress proof {} for data root {}",
        &proof.proof, &data_root
    );

    ro_tx.commit()?;

    let rw_tx = db.tx_mut()?;
    rw_tx.put::<IngressProofs>(data_root, proof)?;
    rw_tx.commit()?;

    Ok(())
}

//==============================================================================
// Tests
//------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use assert_matches::assert_matches;
    use irys_database::{config::get_data_dir, open_or_create_db, tables::IrysTables};
    use irys_packing::xor_vec_u8_arrays_in_place;
    use irys_storage::{ii, initialize_storage_files, ChunkType, StorageModule, StorageModuleInfo};
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{
        irys::IrysSigner,
        partition::{PartitionAssignment, PartitionHash},
        storage_config, Base64, MAX_CHUNK_SIZE,
    };
    use rand::Rng;
    use tokio::time::{sleep, timeout};

    use super::*;

    use actix::prelude::*;

    #[actix::test]
    async fn post_transaction_and_chunks() -> eyre::Result<()> {
        let tmp_dir = setup_tracing_and_temp_dir(Some("post_transaction_and_chunks"), false);
        let base_path = tmp_dir.path().to_path_buf();

        let db = open_or_create_db(tmp_dir, IrysTables::ALL, None).unwrap();
        let arc_db1 = DatabaseProvider(Arc::new(db));
        let arc_db2 = DatabaseProvider(Arc::clone(&arc_db1));

        // Create an instance of the mempool actor
        let task_manager = TaskManager::current();

        let storage_config = StorageConfig::default();
        let chunk_size = storage_config.chunk_size;

        let storage_module_info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: PartitionHash::zero(),
                miner_address: Address::random(),
                ledger_num: Some(0),
                slot_index: Some(0),
            }),
            submodules: vec![
                (ii(0, 4), "hdd0-4TB".to_string()), // 0 to 4 inclusive
            ],
        };
        initialize_storage_files(&base_path, &vec![storage_module_info.clone()])?;

        // Override the default StorageModule config for testing
        let config = StorageConfig {
            min_writes_before_sync: 1,
            chunk_size,
            num_chunks_in_partition: 5,
            ..Default::default()
        };

        let storage_module = Arc::new(StorageModule::new(
            &base_path,
            &storage_module_info,
            config,
        )?);

        storage_module.pack_with_zeros();

        let mempool = MempoolActor::new(
            arc_db1,
            task_manager.executor(),
            IrysSigner::random_signer(),
            storage_config,
            vec![storage_module.clone()],
        );
        let addr: Addr<MempoolActor> = mempool.start();

        // Create 2.5 chunks worth of data *  fill the data with random bytes
        let data_size = (MAX_CHUNK_SIZE as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        // Create a new Irys API instance & a signed transaction
        let irys = IrysSigner::random_signer();
        let tx = irys.create_transaction(data_bytes.clone(), None).unwrap();
        let tx = irys.sign_transaction(tx).unwrap();

        println!("{:?}", tx.header);
        println!("{}", serde_json::to_string_pretty(&tx.header).unwrap());

        for proof in tx.proofs.iter() {
            println!("offset: {}", proof.offset);
        }

        // Wrap the transaction in a TxIngressMessage
        let data_root = tx.header.data_root;
        let data_size = tx.header.data_size;
        let tx_ingress_msg = TxIngressMessage(tx.header);

        // Post the TxIngressMessage to the handle method on the mempool actor
        let result = addr.send(tx_ingress_msg).await.unwrap();

        // Verify the transaction was added
        assert_matches!(result, Ok(()));

        let db_tx = arc_db2.tx()?;

        // Verify the data_root was added to the cache
        let result = irys_database::cached_data_root_by_data_root(&db_tx, data_root).unwrap();
        assert_matches!(result, Some(_));
        let last_index = tx.chunks.len() - 1;
        // Loop though each of the transaction chunks
        for (chunk_offset, chunk_node) in tx.chunks.iter().enumerate() {
            let min = chunk_node.min_byte_range;
            let max = chunk_node.max_byte_range;
            let offset = tx.proofs[chunk_offset].offset as u32;
            let data_path = Base64(tx.proofs[chunk_offset].proof.to_vec());
            let key: H256 = hash_sha256(&data_path.0).unwrap().into();
            let chunk_bytes = Base64(data_bytes[min..max].to_vec());
            // Create a ChunkIngressMessage for each chunk
            let chunk_ingress_msg = ChunkIngressMessage {
                0: UnpackedChunk {
                    data_root,
                    data_size,
                    data_path: data_path.clone(),
                    bytes: chunk_bytes.clone(),
                    tx_offset: chunk_offset as u32,
                },
            };

            let is_last_chunk = chunk_offset == last_index;
            let interval = ii(0, last_index as u64);
            if is_last_chunk {
                // artificially index the chunk with the submodule
                // this will cause the last chunk to show up in cache & on disk
                storage_module.index_transaction_data(vec![0], data_root, interval.into())?;
            }

            // Post the ChunkIngressMessage to the handle method on the mempool
            let result = addr.send(chunk_ingress_msg).await.unwrap();

            // Verify the chunk was added
            assert_matches!(result, Ok(()));

            // Verify the chunk is added to the ChunksCache
            // use a new read tx so we can see the writes
            let db_tx = arc_db2.tx()?;

            let (meta, chunk) =
                irys_database::cached_chunk_by_chunk_offset(&db_tx, data_root, chunk_offset as u32)
                    .unwrap()
                    .unwrap();
            assert_eq!(meta.chunk_path_hash, key);
            assert_eq!(chunk.data_path, data_path);
            assert_eq!(chunk.chunk, Some(chunk_bytes.clone()));

            let result = irys_database::cached_chunk_by_chunk_path_hash(&db_tx, &key).unwrap();
            assert_matches!(result, Some(_));

            storage_module.sync_pending_chunks()?;

            if is_last_chunk {
                // read the set of chunks
                // only offset 2 (last chunk) should have data
                let res = storage_module.read_chunks(ii(0, last_index as u32))?;
                let r = res.get(&2).unwrap();
                let mut packed_bytes = r.0.clone();
                // unpack the data (packing was all 0's)
                xor_vec_u8_arrays_in_place(&mut packed_bytes, &vec![0u8; chunk_size as usize]);
                let packed_bytes_slice = &packed_bytes[0..chunk_bytes.0.len()];
                let cbytes = chunk_bytes.0;
                assert_eq!(packed_bytes_slice.len(), cbytes.len());
                assert_eq!(packed_bytes_slice, cbytes);
                assert_eq!(r.1, ChunkType::Data);
            }

            ()
        }

        // Modify one of the chunks

        // Attempt to post the chunk

        // Verify there chunk is not accepted

        task_manager.graceful_shutdown_with_timeout(Duration::from_secs(5));
        // check the ingress proof is in the DB
        let timed_get = timeout(Duration::from_secs(5), async {
            loop {
                // don't reuse the tx! it has read isolation (won't see anything commited after it's creation)
                let ro_tx = &arc_db2.tx().unwrap();
                match ro_tx.get::<IngressProofs>(data_root).unwrap() {
                    Some(ip) => break ip,
                    None => sleep(Duration::from_millis(100)).await,
                }
            }
        })
        .await?;
        assert_eq!(&timed_get.data_root, &data_root);

        Ok(())
    }
}
