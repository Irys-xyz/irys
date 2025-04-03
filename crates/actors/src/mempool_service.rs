use crate::block_producer::BlockConfirmedMessage;
use crate::block_tree_service::BlockTreeReadGuard;
use actix::{Actor, Context, Handler, Message, Supervised, SystemService};
use base58::ToBase58 as _;
use core::fmt::Display;
use eyre::eyre;
use irys_database::db::RethDbWrapper;
use irys_database::db_cache::data_size_to_chunk_count;
use irys_database::db_cache::DataRootLRUEntry;
use irys_database::submodule::get_data_size_by_data_root;
use irys_database::tables::DataRootLRU;
use irys_database::tables::{CachedChunks, CachedChunksIndex, IngressProofs};
use irys_database::{insert_tx_header, tx_header_by_txid, DataLedger};
use irys_storage::StorageModuleVec;
use irys_types::irys::IrysSigner;
use irys_types::{
    app_state::DatabaseProvider, chunk::UnpackedChunk, hash_sha256, validate_path, GossipData,
    IrysTransactionHeader, H256,
};
use irys_types::{Config, DataRoot, StorageConfig, U256};
use reth::tasks::TaskExecutor;
use reth_db::cursor::DbDupCursorRO as _;
use reth_db::transaction::DbTx as _;
use reth_db::transaction::DbTxMut as _;
use reth_db::Database as _;
use std::collections::HashSet;
use std::collections::{BTreeMap, HashMap};
use tracing::{debug, error, info, warn};
/// The Mempool oversees pending transactions and validation of incoming tx.
#[derive(Default, Debug)]
pub struct MempoolService {
    irys_db: Option<DatabaseProvider>,
    reth_db: Option<RethDbWrapper>,
    /// Temporary mempool stubs - will replace with proper data models - `DMac`
    valid_tx: BTreeMap<H256, IrysTransactionHeader>,
    /// `task_exec` is used to spawn background jobs on reth's MT tokio runtime
    /// instead of the actor executor runtime, while also providing some `QoL`
    task_exec: Option<TaskExecutor>,
    /// The miner's signer instance, used to sign ingress proofs
    signer: Option<IrysSigner>,
    invalid_tx: Vec<H256>,
    storage_config: StorageConfig,
    anchor_expiry_depth: u64,
    max_data_txs_per_block: u64,
    storage_modules: StorageModuleVec,
    block_tree_read_guard: Option<BlockTreeReadGuard>,
    gossip_tx: Option<tokio::sync::mpsc::Sender<GossipData>>,
}

impl Actor for MempoolService {
    type Context = Context<Self>;
}

/// Allows this actor to live in the the local service registry
impl Supervised for MempoolService {}

impl SystemService for MempoolService {}

impl MempoolService {
    /// Create a new instance of the mempool actor passing in a reference
    /// counted reference to a `DatabaseEnv`, a copy of reth's task executor and the miner's signer
    pub fn new(
        irys_db: DatabaseProvider,
        reth_db: RethDbWrapper,
        task_exec: TaskExecutor,
        signer: IrysSigner,
        storage_config: StorageConfig,
        storage_modules: StorageModuleVec,
        block_tree_read_guard: BlockTreeReadGuard,
        config: &Config,
        gossip_tx: tokio::sync::mpsc::Sender<GossipData>,
    ) -> Self {
        info!("service started");
        Self {
            irys_db: Some(irys_db),
            reth_db: Some(reth_db),
            valid_tx: BTreeMap::new(),
            invalid_tx: Vec::new(),
            signer: Some(signer),
            task_exec: Some(task_exec),
            storage_config,
            storage_modules,
            max_data_txs_per_block: config.max_data_txs_per_block,
            anchor_expiry_depth: config.anchor_expiry_depth.into(),
            block_tree_read_guard: Some(block_tree_read_guard),
            gossip_tx: Some(gossip_tx),
        }
    }
}

/// Message for when a new TX is discovered by the node, either though
/// synchronization with peers, or by a user posting the tx.
#[derive(Message, Debug)]
#[rtype(result = "Result<(),TxIngressError>")]
pub struct TxIngressMessage(pub IrysTransactionHeader);

/// Reasons why Transaction Ingress might fail
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxIngressError {
    /// The transaction's signature is invalid
    InvalidSignature,
    /// The account does not have enough tokens to fund this transaction
    Unfunded,
    /// This transaction id is already in the cache
    Skipped,
    /// Invalid anchor value (unknown or too old)
    InvalidAnchor,
    /// Some database error occurred
    DatabaseError,
    /// The service is uninitialized
    ServiceUninitialized,
    /// Catch-all variant for other errors.
    Other(String),
}

impl TxIngressError {
    /// Returns an other error with the given message.
    pub fn other(err: impl Into<String>) -> Self {
        Self::Other(err.into())
    }
    /// Allows converting an error that implements Display into an Other error
    pub fn other_display(err: impl Display) -> Self {
        Self::Other(err.to_string())
    }
}

/// Message for when a new chunk is discovered by the node, either though
/// synchronization with peers, or by a user posting the chunk.
#[derive(Message, Debug)]
#[rtype(result = "Result<(),ChunkIngressError>")]
pub struct ChunkIngressMessage(pub UnpackedChunk);

impl ChunkIngressMessage {
    #[must_use]
    pub fn into_inner(self) -> UnpackedChunk {
        self.0
    }
}

/// Reasons why Transaction Ingress might fail
#[derive(Debug, Clone)]
pub enum ChunkIngressError {
    /// The `data_path/proof` provided with the chunk data is invalid
    InvalidProof,
    /// The data hash does not match the chunk data
    InvalidDataHash,
    /// This chunk is for an unknown transaction
    UnknownTransaction,
    /// Only the last chunk in a `data_root` tree can be less than `CHUNK_SIZE`
    InvalidChunkSize,
    /// Chunks should have the same data_size field as their parent tx
    InvalidDataSize,
    /// Some database error occurred when reading or writing the chunk
    DatabaseError,
    /// The service is uninitialized
    ServiceUninitialized,
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

impl Handler<TxIngressMessage> for MempoolService {
    type Result = Result<(), TxIngressError>;

    fn handle(&mut self, tx_msg: TxIngressMessage, _ctx: &mut Context<Self>) -> Self::Result {
        if self.irys_db.is_none() {
            return Err(TxIngressError::ServiceUninitialized);
        }

        if self.reth_db.is_none() {
            return Err(TxIngressError::ServiceUninitialized);
        }

        let tx = &tx_msg.0;
        debug!(
            "received tx {:?} (data_root {:?})",
            &tx.id.0.to_base58(),
            &tx.data_root.0.to_base58()
        );
        // Early out if we already know about this transaction
        if self.invalid_tx.contains(&tx.id) || self.valid_tx.contains_key(&tx.id) {
            // Skip tx reprocessing if already verified (valid or invalid) to prevent
            // CPU-intensive signature verification spam attacks
            return Err(TxIngressError::Skipped);
        }

        let db = self
            .irys_db
            .clone()
            .ok_or(TxIngressError::ServiceUninitialized)?;
        let read_tx = &db.tx().map_err(|_| TxIngressError::DatabaseError)?; // we use `&` here to make this a `temporary`, which means rust will automatically drop it when we're done using it, instead of at the end of a block like usual
        let reth_db = self
            .reth_db
            .clone()
            .ok_or(TxIngressError::ServiceUninitialized)?;
        let read_reth_tx = &reth_db.tx().map_err(|_| TxIngressError::DatabaseError)?;

        // validate the `anchor` value
        // it should be a block hash for a known, confirmed block (TODO: add tx hash support!)

        let canon_chain = self
            .block_tree_read_guard
            .clone()
            .ok_or(TxIngressError::ServiceUninitialized)?
            .read()
            .get_canonical_chain();

        let (_, latest_height, _, _) = canon_chain.0.last().ok_or(TxIngressError::Other(
            "unable to get canonical chain from block tree".to_owned(),
        ))?;

        match irys_database::block_header_by_hash(read_tx, &tx.anchor, false) {
            // note: we use addition here as it's safer
            Ok(Some(hdr)) if hdr.height + self.anchor_expiry_depth >= *latest_height => {
                debug!("valid block hash anchor {} for tx {}", &tx.anchor, &tx.id);
                // update any associated ingress proofs
                if let Ok(Some(old_expiry)) = read_tx.get::<DataRootLRU>(tx.data_root) {
                    let new_expiry = hdr.height + self.anchor_expiry_depth;
                    debug!(
                        "Updating ingress proof for data root {} expiry from {} -> {}",
                        &tx.data_root, &old_expiry.last_height, &new_expiry
                    );
                    db.update(|write_tx| write_tx.put::<DataRootLRU>(tx.data_root, old_expiry))
                        .map_err(|e| {
                            error!(
                                "Error updating ingress proof expiry for {} - {}",
                                &tx.data_root, &e
                            );
                            TxIngressError::DatabaseError
                        })?
                        .map_err(|e| {
                            error!(
                                "Error updating ingress proof expiry for {} - {}",
                                &tx.data_root, &e
                            );
                            TxIngressError::DatabaseError
                        })?;
                }
            }
            _ => {
                self.invalid_tx.push(tx.id);
                warn!("Invalid anchor value {} for tx {}", &tx.anchor, &tx.id);
                return Err(TxIngressError::InvalidAnchor);
            }
        };

        if irys_database::get_account_balance(read_reth_tx, tx_msg.0.signer)
            .map_err(|_| TxIngressError::DatabaseError)?
            < U256::from(tx_msg.0.total_fee())
        {
            return Err(TxIngressError::Unfunded);
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

        // Cache the data_root in the database

        let _ = db.update_eyre(|db_tx| {
            irys_database::cache_data_root(db_tx, tx)?;
            irys_database::insert_tx_header(db_tx, tx)?;
            Ok(())
        });

        let gossip_sender = self.gossip_tx.clone();
        let gossip_data = GossipData::Transaction(tx.clone());

        let _ = tokio::task::spawn(async move {
            if let Err(error) = gossip_sender.unwrap().send(gossip_data).await {
                tracing::error!("Failed to send gossip data: {:?}", error);
            }
        });

        Ok(())
    }
}

impl Handler<ChunkIngressMessage> for MempoolService {
    type Result = Result<(), ChunkIngressError>;

    fn handle(&mut self, chunk_msg: ChunkIngressMessage, _ctx: &mut Context<Self>) -> Self::Result {
        // TODO: maintain a shared read transaction so we have read isolation
        let chunk: UnpackedChunk = chunk_msg.0;

        if self.irys_db.is_none()
            || self.task_exec.is_none()
            || self.signer.is_none()
            || self.reth_db.is_none()
        {
            return Err(ChunkIngressError::Other(
                "mempool_service not initialized".to_owned(),
            ));
        }

        let db = self.irys_db.clone().unwrap();

        // Check to see if we have a cached data_root for this chunk
        let read_tx = db.tx().map_err(|_| ChunkIngressError::DatabaseError)?;

        let candidate_sms = self
            .storage_modules
            .iter()
            .filter_map(|sm| {
                sm.get_writeable_offsets(&chunk)
                    .ok()
                    .map(|write_offsets| (sm, write_offsets))
            })
            .collect::<Vec<_>>();

        let data_size = irys_database::cached_data_root_by_data_root(&read_tx, chunk.data_root)
            .map_err(|_| ChunkIngressError::DatabaseError)?
            .map(|cdr| cdr.data_size)
            .or_else(|| {
                candidate_sms.iter().find_map(|(sm, write_offsets)| {
                    write_offsets.iter().find_map(|wo| {
                        sm.query_submodule_db_by_offset(*wo, |tx| {
                            get_data_size_by_data_root(tx, chunk.data_root)
                        })
                        .ok()
                        .flatten()
                    })
                })
            })
            .ok_or(ChunkIngressError::UnknownTransaction)?; // Handle None case by converting it to an error

        // Validate that the data_size for this chunk matches the data_size
        // recorded in the transaction header.
        if data_size != chunk.data_size {
            error!(
                "Invalid data_size for data_root: expected: {} got:{}",
                data_size, chunk.data_size
            );
            return Err(ChunkIngressError::InvalidDataSize);
        }

        // Next validate the data_path/proof for the chunk, linking
        // data_root->chunk_hash
        let root_hash = chunk.data_root.0;
        let target_offset = u128::from(chunk.byte_offset(self.storage_config.chunk_size));
        let path_buff = &chunk.data_path;

        info!(
            "chunk_offset:{} data_size:{} offset:{}",
            chunk.tx_offset, chunk.data_size, target_offset
        );

        let path_result = validate_path(root_hash, path_buff, target_offset)
            .map_err(|_| ChunkIngressError::InvalidProof)?;

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
        let num_chunks_in_tx = data_size.div_ceil(chunk_size);
        if u64::from(*chunk.tx_offset) < num_chunks_in_tx - 1 {
            // Ensure prefix chunks are all exactly chunk_size
            if chunk_len != chunk_size {
                error!(
                    "InvalidChunkSize: incomplete not last chunk, tx offset: {} chunk len: {}",
                    chunk.tx_offset, chunk_len
                );
                return Err(ChunkIngressError::InvalidChunkSize);
            }
        } else {
            // Ensure the last chunk is no larger than chunk_size
            if chunk_len > chunk_size {
                error!(
                    "InvalidChunkSize: chunk bigger than max. chunk size, tx offset: {} chunk len: {}",
                    chunk.tx_offset, chunk_len
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

        db.update_eyre(|tx| irys_database::cache_chunk(tx, &chunk))
            .map_err(|_| ChunkIngressError::DatabaseError)?;

        for sm in &self.storage_modules {
            if !sm
                .get_writeable_offsets(&chunk)
                .unwrap_or_default()
                .is_empty()
            {
                info!(target: "irys::mempool::chunk_ingress", "Writing chunk with offset {} for data_root {} to sm {}", &chunk.tx_offset, &chunk.data_root, &sm.id );
                sm.write_data_chunk(&chunk)
                    .map_err(|_| ChunkIngressError::Other("Internal error".to_owned()))?;
            }
        }

        // ==== INGRESS PROOFS ====
        let root_hash: H256 = root_hash.into();

        // check if we have generated an ingress proof for this tx already
        // if we have, update it's expiry height

        //  TODO: hook into whatever manages ingress proofs
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
        let read_tx = db.tx().map_err(|_| ChunkIngressError::DatabaseError)?;

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
        let expected_chunk_count = data_size_to_chunk_count(data_size, chunk_size).unwrap();

        if chunk_count == expected_chunk_count {
            // we *should* have all the chunks
            // dispatch a ingress proof task

            let canon_chain = self
                .block_tree_read_guard
                .clone()
                .ok_or(ChunkIngressError::ServiceUninitialized)?
                .read()
                .get_canonical_chain();

            let (_, latest_height, _, _) = canon_chain
                .0
                .last()
                .ok_or(ChunkIngressError::ServiceUninitialized)?;

            let db = self.irys_db.clone().unwrap();
            let signer = self.signer.clone().unwrap();
            let latest_height = *latest_height;
            self.task_exec.clone().unwrap().spawn_blocking(async move {
                generate_ingress_proof(db.clone(), root_hash, data_size, chunk_size, signer)
                    // TODO: handle results instead of unwrapping
                    .unwrap();
                db.update(|wtx| {
                    wtx.put::<DataRootLRU>(
                        root_hash,
                        DataRootLRUEntry {
                            last_height: latest_height,
                            ingress_proof: true,
                        },
                    )
                })
                .unwrap()
                .unwrap();
            });
        }

        let gossip_sender = self.gossip_tx.clone();
        let gossip_data = GossipData::Chunk(chunk);

        let _ = tokio::task::spawn(async move {
            if let Err(error) = gossip_sender.unwrap().send(gossip_data).await {
                tracing::error!("Failed to send gossip data: {:?}", error);
            }
        });

        Ok(())
    }
}

/// Message for getting txs for block building
#[derive(Message, Debug)]
#[rtype(result = "Vec<IrysTransactionHeader>")]
pub struct GetBestMempoolTxs;

impl Handler<GetBestMempoolTxs> for MempoolService {
    type Result = Vec<IrysTransactionHeader>;

    fn handle(&mut self, _msg: GetBestMempoolTxs, _ctx: &mut Self::Context) -> Self::Result {
        let reth_db = self.reth_db.clone().unwrap();
        let mut fees_spent_per_address = HashMap::new();

        // TODO sort by fee
        self.valid_tx
            .iter()
            .filter(|(_, tx)| {
                let current_spent = *fees_spent_per_address.get(&tx.signer).unwrap_or(&0_u64);
                let valid = irys_database::get_account_balance(&reth_db.tx().unwrap(), tx.signer)
                    .unwrap()
                    >= U256::from(current_spent + tx.total_fee());
                match fees_spent_per_address.get_mut(&tx.signer) {
                    Some(val) => {
                        *val += tx.total_fee();
                    }
                    None => {
                        fees_spent_per_address.insert(tx.signer, tx.total_fee());
                    }
                };
                valid
            })
            .take(self.max_data_txs_per_block.try_into().unwrap())
            .map(|(_, header)| header.clone())
            .collect()
    }
}

impl Handler<BlockConfirmedMessage> for MempoolService {
    type Result = eyre::Result<()>;
    fn handle(&mut self, msg: BlockConfirmedMessage, _ctx: &mut Context<Self>) -> Self::Result {
        || -> eyre::Result<()> {
            let db = self.irys_db.clone().ok_or_else(|| {
                error!("mempool_service is uninitialized");
                eyre!("mempool_service is uninitialized")
            })?;

            // Access the block header through msg.0
            let block = &msg.0;
            let all_txs = &msg.1;

            for txid in block.data_ledgers[DataLedger::Submit].tx_ids.iter() {
                // Remove the submit tx from the pending valid_tx pool
                self.valid_tx.remove(txid);
            }

            let published_txids = &block.data_ledgers[DataLedger::Publish].tx_ids.0;

            // Loop though the promoted transactions and remove their ingress proofs
            // from the mempool. In the future on a multi node network we may keep
            // ingress proofs around longer to account for re-orgs, but for now
            // we just remove them.
            if !published_txids.is_empty() {
                let mut_tx = db
                    .tx_mut()
                    .map_err(|e| {
                        error!("Failed to create mdbx transaction: {}", e);
                    })
                    .unwrap();

                for (i, txid) in block.data_ledgers[DataLedger::Publish]
                    .tx_ids
                    .0
                    .iter()
                    .enumerate()
                {
                    // Retrieve the promoted transactions header
                    let mut tx_header = match tx_header_by_txid(&mut_tx, txid) {
                        Ok(Some(header)) => header,
                        Ok(None) => {
                            error!("No transaction header found for txid: {}", txid);
                            continue;
                        }
                        Err(e) => {
                            error!("Error fetching transaction header for txid {}: {}", txid, e);
                            continue;
                        }
                    };

                    // TODO: In a single node world there is only one ingress proof
                    // per promoted tx, but in the future there will be multiple proofs.
                    let proofs = block.data_ledgers[DataLedger::Publish]
                        .proofs
                        .as_ref()
                        .unwrap();
                    let proof = proofs.0[i].clone();
                    tx_header.ingress_proofs = Some(proof);

                    // Update the header record in the database to include the ingress
                    // proof, indicating it is promoted
                    if let Err(err) = insert_tx_header(&mut_tx, &tx_header) {
                        error!(
                            "Could not update transactions with ingress proofs - txid: {} err: {}",
                            txid, err
                        );
                    }

                    info!("Promoted tx:\n{:?}", tx_header);
                }

                let _ = mut_tx.commit();
            }

            info!(
                "Removing confirmed tx - Block height: {} num tx: {}",
                block.height,
                all_txs.len()
            );
            Ok(())
        }()
        // closure so we can "catch" and log all errs, so we don't need to log and return an err everywhere
        .inspect_err(|e| {
            error!(
                "Unexpected Mempool error while processing BlockConfirmedMessage: {}",
                e
            );
        })
    }
}

/// Message to check whether a transaction exists in the mempool or on disk
#[derive(Message, Debug)]
#[rtype(result = "Result<bool, TxIngressError>")]
pub struct TxExistenceQuery(pub H256);

impl TxExistenceQuery {
    #[must_use]
    pub fn into_inner(self) -> H256 {
        self.0
    }
}

impl Handler<TxExistenceQuery> for MempoolService {
    type Result = Result<bool, TxIngressError>;

    fn handle(&mut self, tx_msg: TxExistenceQuery, _ctx: &mut Context<Self>) -> Self::Result {
        if self.irys_db.is_none() {
            return Err(TxIngressError::ServiceUninitialized);
        }

        if self.valid_tx.contains_key(&tx_msg.0) {
            return Ok(true);
        }

        // Still has it, just invalid
        if self.invalid_tx.contains(&tx_msg.0) {
            return Ok(true);
        }

        let read_tx = &self
            .irys_db
            .as_ref()
            .ok_or(TxIngressError::ServiceUninitialized)?
            .tx()
            .map_err(|_| TxIngressError::DatabaseError)?;

        let txid = tx_msg.0;
        let tx_header =
            tx_header_by_txid(read_tx, &txid).map_err(|_| TxIngressError::DatabaseError)?;

        Ok(tx_header.is_some())
    }
}

/// Generates an ingress proof for a specific `data_root`
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

    let mut chunks = Vec::with_capacity(expected_chunk_count as usize);
    let mut owned_chunks = Vec::with_capacity(expected_chunk_count as usize);
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
            .unwrap_or_else(|| {
                panic!("unable to get chunk {chunk_path_hash} for data root {data_root} from DB")
            });
        let chunk_bin = chunk.chunk.unwrap().0;
        data_size += chunk_bin.len() as u64;
        owned_chunks.push(chunk_bin);
    }

    // Now create the slice references
    chunks.extend(owned_chunks.iter().map(std::vec::Vec::as_slice));

    assert_eq!(chunks.len() as u32, expected_chunk_count);
    assert_eq!(data_size, size);

    // generate the ingress proof hash
    let proof = irys_types::ingress::generate_ingress_proof(signer, data_root, &chunks)?;
    info!(
        "generated ingress proof {} for data root {}",
        &proof.proof, &data_root
    );

    ro_tx.commit()?;

    db.update(|rw_tx| rw_tx.put::<IngressProofs>(data_root, proof))??;

    Ok(())
}
