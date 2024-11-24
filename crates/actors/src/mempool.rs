use actix::{Actor, Context, Handler, Message};
use eyre::eyre;
use irys_database::db_cache::{chunk_offset_to_index, data_size_to_chunk_count, CachedChunk};
use irys_database::tables::{CachedChunks, CachedChunksIndex, IngressProofs};
use irys_types::ingress::generate_ingress_proof_tree;
use irys_types::irys::IrysSigner;
use irys_types::ChunkBin;
use irys_types::DataRoot;
use irys_types::{
    app_state::DatabaseProvider, chunk::Chunk, hash_sha256, validate_path, IrysTransactionHeader,
    CHUNK_SIZE, H256,
};
use irys_types::{Address, DataChunks};
use reth::tasks::TaskExecutor;
use reth::tasks::TaskManager;
use reth_db::cursor::DbCursorRO;
use reth_db::cursor::DbDupCursorRO;
use reth_db::transaction::DbTx;
use reth_db::transaction::DbTxMut;
use reth_db::Database;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::info;

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
}

impl Actor for MempoolActor {
    type Context = Context<Self>;
}

impl MempoolActor {
    /// Create a new instance of the mempool actor passing in a reference
    /// counted reference to a DatabaseEnv, a copy of reth's task executor and the miner's signer
    pub fn new(db: DatabaseProvider, task_exec: TaskExecutor, signer: IrysSigner) -> Self {
        Self {
            db,
            valid_tx: BTreeMap::new(),
            invalid_tx: Vec::new(),
            signer,
            task_exec,
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
pub struct ChunkIngressMessage(pub Chunk);

impl ChunkIngressMessage {
    fn into_inner(self) -> Chunk {
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
        let _ = irys_database::cache_data_root(&self.db, &tx);

        Ok(())
    }
}

impl Handler<ChunkIngressMessage> for MempoolActor {
    type Result = Result<(), ChunkIngressError>;

    fn handle(&mut self, chunk_msg: ChunkIngressMessage, _ctx: &mut Context<Self>) -> Self::Result {
        // TODO: maintain a shared read transaction so we have read isolation
        let chunk = chunk_msg.0;
        // Check to see if we have a cached data_root for this chunk
        let result = irys_database::cached_data_root_by_data_root(&self.db, chunk.data_root);

        let cached_data_root = result
            .map_err(|_| ChunkIngressError::DatabaseError)? // Convert DatabaseError to ChunkIngressError
            .ok_or(ChunkIngressError::InvalidDataHash)?; // Handle None case by converting it to an error

        // Next validate the data_path/proof for the chunk, linking
        // data_root->chunk_hash
        let root_hash = chunk.data_root.0;
        let target_offset = chunk.offset as u128;
        let path_buff = &chunk.data_path;

        let path_result = match validate_path(root_hash, path_buff, target_offset) {
            Ok(result) => result,
            Err(_) => {
                return Err(ChunkIngressError::InvalidProof);
            }
        };

        // Validate that the data_size for this chunk matches the data_size
        // recorded in the transaction header.
        if cached_data_root.data_size != chunk.data_size {
            return Err(ChunkIngressError::InvalidDataHash);
        }

        // Use that data_Size to identify  and validate that only the last chunk
        // can be less than 256KB
        let chunk_len = chunk.bytes.len() as u64;
        if (chunk.offset as u64) < chunk.data_size - 1 {
            // Ensure prefix chunks are all exactly CHUNK_SIZE
            if chunk_len != CHUNK_SIZE {
                return Err(ChunkIngressError::InvalidChunkSize);
            }
        } else {
            // Ensure the last chunk is no larger than CHUNK_SIZE
            if chunk_len > CHUNK_SIZE {
                return Err(ChunkIngressError::InvalidChunkSize);
            }
        }

        // TODO: Mark the data_root as invalid if the chunk is an incorrect size

        // Check that the leaf hash on the data_path matches the chunk_hash
        if path_result.leaf_hash == hash_sha256(&chunk.bytes.0).unwrap() {
            // TODO: fix all these unwraps!
            // Finally write the chunk to CachedChunks, this will succeed even if the chunk is one that's already inserted
            irys_database::cache_chunk(&self.db, chunk).unwrap();

            let tx = self.db.tx().unwrap();
            let root_hash: H256 = root_hash.into();

            // check if we have generated an ingress proof for this tx already
            if tx.get::<IngressProofs>(root_hash).unwrap().is_some() {
                info!(
                    "We've already generated an ingress proof for data root {}",
                    &root_hash
                );
                return Ok(());
            };

            // check if we have all the chunks for this tx
            let mut cursor = tx.cursor_dup_read::<CachedChunksIndex>().unwrap();

            // get the number of dupsort values (aka the number of chunks)
            // this ASSUMES that the index isn't corrupt (no double values etc)
            // the ingress proof generation task does a more thorough check
            let chunk_count = cursor.dup_count(root_hash).unwrap().unwrap();
            // data size is the offset of the last chunk
            // add one as index is 0-indexed
            let expected_chunk_count =
                data_size_to_chunk_count(cached_data_root.data_size).unwrap();
            if chunk_count == expected_chunk_count {
                // we *should* have all the chunks
                // dispatch a ingress proof task
                let db1 = self.db.clone();
                let signer1 = self.signer.clone();
                self.task_exec.spawn_blocking(async move {
                    generate_ingress_proof(db1, root_hash, cached_data_root.data_size, signer1)
                        // TODO: handle results instead of unwrapping
                        .unwrap();
                });
            }

            Ok(())
        } else {
            Err(ChunkIngressError::InvalidDataHash)
        }
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

pub fn generate_ingress_proof(
    db: DatabaseProvider,
    data_root: DataRoot,
    size: u64,
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
    let expected_chunk_count = data_size_to_chunk_count(size).unwrap();
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
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{irys::IrysSigner, Base64, MAX_CHUNK_SIZE};
    use rand::Rng;
    use tokio::time::{sleep, timeout};

    use super::*;

    use actix::prelude::*;

    #[actix::test]
    async fn post_transaction_and_chunks() -> eyre::Result<()> {
        let tmpdir = setup_tracing_and_temp_dir(Some("post_transaction_and_chunks"), false);

        let db = open_or_create_db(tmpdir, IrysTables::ALL, None).unwrap();
        let arc_db1 = DatabaseProvider(Arc::new(db));
        let arc_db2 = DatabaseProvider(Arc::clone(&arc_db1));

        // Create an instance of the mempool actor
        let task_manager = TaskManager::current();

        let mempool = MempoolActor::new(
            arc_db1,
            task_manager.executor(),
            IrysSigner::random_signer(),
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

        // Wrap the transaction in a TxIngressMessage
        let data_root = tx.header.data_root;
        let data_size = tx.header.data_size;
        let tx_ingress_msg = TxIngressMessage(tx.header);

        // Post the TxIngressMessage to the handle method on the mempool actor
        let result = addr.send(tx_ingress_msg).await.unwrap();

        // Verify the transaction was added
        assert_matches!(result, Ok(()));

        // Verify the data_root was added to the cache
        let result = irys_database::cached_data_root_by_data_root(&arc_db2, data_root).unwrap();
        assert_matches!(result, Some(_));

        // Loop though each of the transaction chunks
        for (index, chunk_node) in tx.chunks.iter().enumerate() {
            let min = chunk_node.min_byte_range;
            let max = chunk_node.max_byte_range;
            let offset = tx.proofs[index].offset as u32;
            let data_path = Base64(tx.proofs[index].proof.to_vec());
            let key: H256 = hash_sha256(&data_path.0).unwrap().into();
            let chunk_bytes = Base64(data_bytes[min..max].to_vec());
            // Create a ChunkIngressMessage for each chunk
            let chunk_ingress_msg = ChunkIngressMessage {
                0: Chunk {
                    data_root,
                    data_size,
                    data_path: data_path.clone(),
                    bytes: chunk_bytes.clone(),
                    offset,
                },
            };

            // Post the ChunkIngressMessage to the handle method on the mempool
            let result = addr.send(chunk_ingress_msg).await.unwrap();

            // Verify the chunk was added
            assert_matches!(result, Ok(()));

            // Verify the chunk is added to the ChunksCache
            let (meta, chunk) = irys_database::cached_chunk_by_offset(&arc_db2, data_root, offset)
                .unwrap()
                .unwrap();
            assert_eq!(meta.chunk_path_hash, key);
            assert_eq!(chunk.data_path, data_path);
            assert_eq!(chunk.chunk, Some(chunk_bytes));

            let result = irys_database::cached_chunk_by_chunk_key(&arc_db2, key).unwrap();
            assert_matches!(result, Some(_));
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
