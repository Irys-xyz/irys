use crate::block_tree_service::BlockTreeReadGuard;
use crate::services::ServiceSenders;
use crate::{CommitmentCacheMessage, CommitmentStateReadGuard, CommitmentStatus};
use actix::{MailboxError, Message, MessageResponse};
use base58::ToBase58 as _;
use core::fmt::Display;
use eyre::eyre;
use futures::future::Either;
use irys_database::{
    db::RethDbWrapper,
    db_cache::{data_size_to_chunk_count, DataRootLRUEntry},
    submodule::get_data_size_by_data_root,
    tables::{CachedChunks, CachedChunksIndex, DataRootLRU, IngressProofs},
    {insert_tx_header, tx_header_by_txid, SystemLedger},
};
use irys_primitives::CommitmentType;
use irys_storage::StorageModulesReadGuard;
use irys_types::{
    app_state::DatabaseProvider, chunk::UnpackedChunk, hash_sha256, irys::IrysSigner,
    validate_path, Address, CommitmentTransaction, Config, DataLedger, DataRoot, GossipData,
    IrysBlockHeader, IrysTransactionCommon, IrysTransactionHeader, IrysTransactionId,
    TxChunkOffset, H256, U256,
};
use lru::LruCache;
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
use reth_db::{
    cursor::DbDupCursorRO as _, transaction::DbTx as _, transaction::DbTxMut as _, Database as _,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    num::NonZeroUsize,
    pin::pin,
    sync::{Arc, RwLock},
};
use tokio::{sync::mpsc::UnboundedReceiver, task::JoinHandle};
use tracing::{debug, error, info, warn};

impl From<MailboxError> for TxIngressError {
    fn from(value: MailboxError) -> Self {
        TxIngressError::Other(format!(
            "Failed to send a message to MempoolService: {:?}",
            value
        ))
    }
}

impl From<MailboxError> for ChunkIngressError {
    fn from(value: MailboxError) -> Self {
        ChunkIngressError::Other(format!(
            "Failed to send a message to MempoolService: {:?}",
            value
        ))
    }
}

#[derive(Debug)]
pub struct MempoolState {
    irys_db: DatabaseProvider,
    reth_db: RethDbWrapper,
    /// Temporary mempool stubs - will replace with proper data models - `DMac`
    valid_tx: BTreeMap<H256, IrysTransactionHeader>,
    valid_commitment_tx: BTreeMap<Address, Vec<CommitmentTransaction>>,
    /// `task_exec` is used to spawn background jobs on reth's MT tokio runtime
    /// instead of the actor executor runtime, while also providing some `QoL`
    task_exec: TaskExecutor,
    /// The miner's signer instance, used to sign ingress proofs
    invalid_tx: Vec<H256>,
    /// Tracks recent valid txids from either storage or commitment
    recent_valid_tx: HashSet<H256>,
    config: Config,
    storage_modules_guard: StorageModulesReadGuard,
    block_tree_read_guard: BlockTreeReadGuard,
    commitment_state_guard: CommitmentStateReadGuard,
    /// LRU caches for out of order gossip data
    pending_chunks: LruCache<DataRoot, LruCache<TxChunkOffset, UnpackedChunk>>,
    pending_pledges: LruCache<Address, LruCache<IrysTransactionId, CommitmentTransaction>>,

    /// Reference to all the services we can send messages to
    service_senders: ServiceSenders,
    gossip_tx: tokio::sync::mpsc::Sender<GossipData>,
}

pub type AtomicMempoolState = Arc<RwLock<MempoolState>>;

/// Messages that the Mempool Service supports
#[derive(Debug)]
pub enum MempoolServiceMessage {
    BlockConfirmedMessage(Arc<IrysBlockHeader>, Arc<Vec<IrysTransactionHeader>>),
    GetTransaction(
        H256,
        tokio::sync::oneshot::Sender<Option<IrysTransactionHeader>>,
    ),
    ChunkIngressMessage(UnpackedChunk),
    CommitmentTxIngressMessage(
        CommitmentTransaction,
        tokio::sync::oneshot::Sender<Result<bool, TxIngressError>>,
    ),
    GetBestMempoolTxs(tokio::sync::oneshot::Sender<MempoolTxs>),
    TxExistenceQuery(
        H256,
        tokio::sync::oneshot::Sender<Result<bool, TxIngressError>>,
    ),
    TxIngressMessage(IrysTransactionHeader),
}

#[derive(Debug)]
struct Inner {
    mempool_state: AtomicMempoolState,
}

/// The Mempool oversees pending transactions and validation of incoming tx.
#[derive(Debug)]
pub struct MempoolService {
    shutdown: GracefulShutdown,
    msg_rx: UnboundedReceiver<MempoolServiceMessage>,
    inner: Inner,
}

impl Default for MempoolService {
    fn default() -> Self {
        unimplemented!("don't rely on the default implementation of the `MempoolService`");
    }
}

impl MempoolService {
    /// Spawn a new Mempool service
    pub fn spawn_service(
        exec: &TaskExecutor,
        irys_db: DatabaseProvider,
        reth_db: RethDbWrapper,
        storage_modules_guard: StorageModulesReadGuard,
        block_tree_guard: BlockTreeReadGuard,
        commitment_state_guard: CommitmentStateReadGuard,
        rx: UnboundedReceiver<MempoolServiceMessage>,
        config: &Config,
        service_senders: ServiceSenders,
        gossip_tx: tokio::sync::mpsc::Sender<GossipData>,
    ) -> JoinHandle<()> {
        let mempool_state = create_state(
            irys_db,
            reth_db,
            exec.clone(),
            block_tree_guard.clone(),
            commitment_state_guard.clone(),
            storage_modules_guard.clone(),
            config,
            service_senders,
            gossip_tx,
        );
        exec.spawn_critical_with_graceful_shutdown_signal(
            "Mempool Service",
            |shutdown| async move {
                let mempool_service = Self {
                    shutdown,
                    msg_rx: rx,
                    inner: Inner {
                        mempool_state: Arc::new(RwLock::new(mempool_state)),
                    },
                };
                mempool_service
                    .start()
                    .await
                    .expect("Mempool service encountered an irrecoverable error")
            },
        )
    }

    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting Mempool service");

        let mut shutdown_future = pin!(self.shutdown);
        let shutdown_guard = loop {
            let mut msg_rx = pin!(self.msg_rx.recv());
            match futures::future::select(&mut msg_rx, &mut shutdown_future).await {
                Either::Left((Some(msg), _)) => {
                    self.inner.handle_message(msg).await?;
                }
                Either::Left((None, _)) => {
                    tracing::warn!("receiver channel closed");
                    break None;
                }
                Either::Right((shutdown, _)) => {
                    tracing::warn!("shutdown signal received");
                    break Some(shutdown);
                }
            }
        };

        tracing::debug!(amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdwon");
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.inner.handle_message(msg).await?;
        }

        // explicitly inform the TaskManager that we're shutting down
        drop(shutdown_guard);

        tracing::info!("shutting down Mempool service");
        Ok(())
    }
}

/// Message for when a new TX is discovered by the node, either though
/// synchronization with peers, or by a user posting the tx.
#[derive(Message, Debug)]
#[rtype(result = "Result<(),TxIngressError>")]
pub struct TxIngressMessage(pub IrysTransactionHeader);

#[derive(Message, Debug)]
#[rtype(result = "Result<(),TxIngressError>")]
pub struct CommitmentTxIngressMessage(pub CommitmentTransaction);

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

#[derive(MessageResponse, Debug)]
pub struct MempoolTxs {
    pub commitment_tx: Vec<CommitmentTransaction>,
    pub storage_tx: Vec<IrysTransactionHeader>,
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

    let ro_tx = db.tx()?;
    let mut dup_cursor = ro_tx.cursor_dup_read::<CachedChunksIndex>()?;

    // start from first duplicate entry for this root_hash
    let dup_walker = dup_cursor.walk_dup(Some(data_root), None)?;

    // we need to validate that the index is valid
    // we do this by constructing a set over the chunk hashes, checking if we've seen this hash before
    // if we have, we *must* error
    let mut set = HashSet::<H256>::new();
    let expected_chunk_count = data_size_to_chunk_count(size, chunk_size).unwrap();

    let mut chunk_count: u32 = 0;
    let mut data_size: u64 = 0;

    let iter = dup_walker.into_iter().map(|entry| {
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

        // TODO: add code to read from ChunkProvider once it can read through CachedChunks & we have a nice system for unpacking chunks on-demand
        let chunk = ro_tx
            .get::<CachedChunks>(index_entry.meta.chunk_path_hash)?
            .ok_or(eyre!(
                "unable to get chunk {chunk_path_hash} for data root {data_root} from DB"
            ))?;

        let chunk_bin = chunk
            .chunk
            .ok_or(eyre!(
                "Missing required chunk ({chunk_path_hash}) body for data root {data_root} from DB"
            ))?
            .0;
        data_size += chunk_bin.len() as u64;
        chunk_count += 1;

        Ok(chunk_bin)
    });

    // generate the ingress proof hash
    let proof = irys_types::ingress::generate_ingress_proof(signer, data_root, iter)?;
    info!(
        "generated ingress proof {} for data root {}",
        &proof.proof, &data_root
    );
    assert_eq!(data_size, size);
    assert_eq!(chunk_count as u32, expected_chunk_count);

    ro_tx.commit()?;

    db.update(|rw_tx| rw_tx.put::<IngressProofs>(data_root, proof))??;

    Ok(())
}

impl Inner {
    #[tracing::instrument(skip_all, err)]
    async fn handle_message(&mut self, msg: MempoolServiceMessage) -> eyre::Result<()> {
        let mempool_state = &self.mempool_state.clone();
        let mut mempool_state_guard = mempool_state.write().expect("expected valid mempool state");

        match msg {
            MempoolServiceMessage::GetTransaction(tx, response) => {
                if let Some(tx_header) = mempool_state_guard.valid_tx.get(&tx) {
                    if let Err(e) = response.send(Some(tx_header.clone())) {
                        tracing::error!("response.send(tx_header) error: {:?}", e);
                    };
                    return Ok(());
                }

                if let Ok(read_tx) = self.read_tx() {
                    let tx_header = tx_header_by_txid(&read_tx, &tx).unwrap_or(None);
                    if let Err(e) = response.send(tx_header.clone()) {
                        tracing::error!("response.send(tx_header) error: {:?}", e);
                    };
                }

                Ok(())
            }
            MempoolServiceMessage::BlockConfirmedMessage(block, all_txs) => {
                for txid in block.data_ledgers[DataLedger::Submit].tx_ids.iter() {
                    // Remove the submit tx from the pending valid_tx pool
                    mempool_state_guard.valid_tx.remove(txid);
                    mempool_state_guard.recent_valid_tx.remove(txid);
                }

                // Is there a commitment ledger in this block?
                let commitment_ledger = block
                    .system_ledgers
                    .iter()
                    .find(|b| b.ledger_id == SystemLedger::Commitment);

                if let Some(commitment_ledger) = commitment_ledger {
                    for txid in commitment_ledger.tx_ids.iter() {
                        // Remove the commitment tx from the pending valid_tx pool
                        self.remove_commitment_tx(txid);
                    }
                }

                let published_txids = &block.data_ledgers[DataLedger::Publish].tx_ids.0;

                // Loop though the promoted transactions and remove their ingress proofs
                // from the mempool. In the future on a multi node network we may keep
                // ingress proofs around longer to account for re-orgs, but for now
                // we just remove them.
                if !published_txids.is_empty() {
                    let mut_tx = mempool_state_guard
                        .irys_db
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
                                error!(
                                    "Error fetching transaction header for txid {}: {}",
                                    txid, e
                                );
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
            }
            MempoolServiceMessage::CommitmentTxIngressMessage(commitment_tx, response) => {
                //let commitment_tx = commitment_tx_msg.0.clone();
                debug!(
                    "received commitment tx {:?}",
                    &commitment_tx.id.0.to_base58()
                );

                // Early out if we already know about this transaction (invalid)
                if mempool_state_guard.invalid_tx.contains(&commitment_tx.id) {
                    if let Err(e) = response.send(Err(TxIngressError::Skipped)) {
                        tracing::error!("response.send(tx_header) error: {:?}", e);
                    };
                    return Ok(());
                }

                // Check if the transaction already exists in valid transactions
                let tx_exists = mempool_state_guard
                    .valid_commitment_tx
                    .get(&commitment_tx.signer)
                    .map_or(false, |txs| txs.iter().any(|c| c.id == commitment_tx.id));

                if tx_exists {
                    if let Err(e) = response.send(Err(TxIngressError::Skipped)) {
                        tracing::error!("response.send(tx_header) error: {:?}", e);
                    };
                    return Ok(());
                }

                // Validate the tx anchor
                if let Err(e) = self.validate_anchor(&commitment_tx.id, &commitment_tx.anchor) {
                    tracing::error!(
                        "Anchor {:?} for tx {:?} failure with error: {:?}",
                        &commitment_tx.anchor,
                        commitment_tx.id,
                        e
                    );
                    return Ok(());
                }

                // Check pending commitments and cached commitments and active commitments
                let commitment_status = self.get_commitment_status(&commitment_tx).await;
                if commitment_status == CommitmentStatus::Accepted {
                    // Validate tx signature
                    if let Err(e) = self.validate_signature(&commitment_tx) {
                        tracing::error!(
                            "Signature validation for commitment_tx {:?} failed with error: {:?}",
                            &commitment_tx,
                            e
                        );
                    }

                    // Add the commitment tx to the valid tx list to be included in the next block
                    mempool_state_guard
                        .valid_commitment_tx
                        .entry(commitment_tx.signer)
                        .or_default()
                        .push(commitment_tx.clone());

                    mempool_state_guard.recent_valid_tx.insert(commitment_tx.id);

                    // Process any pending pledges for this newly staked address
                    // ------------------------------------------------------
                    // When a stake transaction is accepted, we can now process any pledge
                    // transactions from the same address that arrived earlier but were
                    // waiting for the stake. This effectively resolves the dependency
                    // order for address-based validation.
                    if let Some(pledges_lru) = mempool_state_guard
                        .pending_pledges
                        .pop(&commitment_tx.signer)
                    {
                        // Extract all pending pledges as a vector of owned transactions
                        let pledges: Vec<_> = pledges_lru
                            .into_iter()
                            .map(|(_, pledge_tx)| pledge_tx)
                            .collect();

                        // PERFORMANCE NOTE: Processing all pending pledges synchronously
                        // If an address has accumulated many pending pledges, this could
                        // potentially block the actor for a significant time.
                        for pledge_tx in pledges {
                            // Re-process each pledge now that its signer is staked
                            // No need to clone as we own the transaction objects
                            let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
                            self.handle_message(MempoolServiceMessage::CommitmentTxIngressMessage(
                                pledge_tx, oneshot_tx,
                            ));

                            let status = oneshot_rx
                                .await
                                .expect("to process pending pledge for newly staked address");
                        }
                    }

                    // Gossip transaction
                    // TODO is gossip sender not in the senders struct?
                    let gossip_sender = mempool_state_guard.gossip_tx.clone();
                    let gossip_data = GossipData::CommitmentTransaction(commitment_tx.clone());

                    //todo fix this to be async!
                    let _ = tokio::task::spawn(async move {
                        if let Err(error) = gossip_sender.send(gossip_data).await {
                            tracing::error!("Failed to send gossip data: {:?}", error);
                        }
                    });

                    Ok(())
                } else {
                    if commitment_status == CommitmentStatus::Unstaked {
                        // For unstaked pledges, we cache them in a 2-level LRU structure:
                        // Level 1: Keyed by signer address (allows tracking multiple addresses)
                        // Level 2: Keyed by transaction ID (allows tracking multiple pledge tx per address)

                        if let Some(pledges_cache) = mempool_state_guard
                            .pending_pledges
                            .get_mut(&commitment_tx.signer)
                        {
                            // Address already exists in cache - add this pledge transaction to its lru cache
                            pledges_cache.put(commitment_tx.id, commitment_tx.clone());
                        } else {
                            // First pledge from this address - create a new nested lru cache
                            let max_pending_pledge_items = mempool_state_guard
                                .config
                                .consensus
                                .mempool
                                .max_pending_pledge_items;
                            let mut new_address_cache =
                                LruCache::new(NonZeroUsize::new(max_pending_pledge_items).unwrap());

                            // Add the pledge transaction to the new lru cache for the address
                            new_address_cache.put(commitment_tx.id, commitment_tx.clone());

                            // Add the address cache to the primary lru cache
                            mempool_state_guard
                                .pending_pledges
                                .put(commitment_tx.signer, new_address_cache);
                        }
                        Ok(())
                    } else {
                        Err(TxIngressError::Skipped)
                    }
                }
            }
            MempoolServiceMessage::ChunkIngressMessage(chunk) => {
                // TODO: maintain a shared read transaction so we have read isolation
                let max_chunks_per_item = mempool_state_guard
                    .config
                    .consensus
                    .mempool
                    .max_chunks_per_item;

                info!(data_root = ?chunk.data_root, number = ?chunk.tx_offset, "Processing chunk");

                // Check to see if we have a cached data_root for this chunk
                let read_tx = match self.read_tx() {
                    Err(e) => {
                        // todo: check this fits with error strategy for handler
                        return Ok(());
                    }
                    Ok(v) => v,
                };

                let binding = mempool_state_guard.storage_modules_guard.read();
                let candidate_sms = binding
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
                        debug!(data_root=?chunk.data_root, number=?chunk.tx_offset,"Checking SMs for data_size");
                        candidate_sms.iter().find_map(|(sm, write_offsets)| {
                            write_offsets.iter().find_map(|wo| {
                                sm.query_submodule_db_by_offset(*wo, |tx| {
                                    get_data_size_by_data_root(tx, chunk.data_root)
                                })
                                .ok()
                                .flatten()
                            })
                        })
                    });

                let data_size = match data_size {
                    Some(ds) => ds,
                    None => {
                        // We don't have a data_root for this chunk but possibly the transaction containing this
                        // chunks data_root will arrive soon. Park it in the pending chunks LRU cache until it does.
                        if let Some(chunks_map) =
                            mempool_state_guard.pending_chunks.get_mut(&chunk.data_root)
                        {
                            chunks_map.put(chunk.tx_offset, chunk.clone());
                        } else {
                            // If there's no entry for this data_root yet, create one
                            let mut new_lru_cache =
                                LruCache::new(NonZeroUsize::new(max_chunks_per_item).unwrap());
                            new_lru_cache.put(chunk.tx_offset, chunk.clone());
                            mempool_state_guard
                                .pending_chunks
                                .put(chunk.data_root, new_lru_cache);
                        }
                        return Ok(());
                    }
                };

                // Validate that the data_size for this chunk matches the data_size
                // recorded in the transaction header.
                if data_size != chunk.data_size {
                    error!(
                        "Error: {:?}. Invalid data_size for data_root: expected: {} got:{}",
                        ChunkIngressError::InvalidDataSize,
                        data_size,
                        chunk.data_size
                    );
                    return Ok(());
                }

                // Next validate the data_path/proof for the chunk, linking
                // data_root->chunk_hash
                let root_hash = chunk.data_root.0;
                let target_offset = u128::from(
                    chunk.end_byte_offset(mempool_state_guard.config.consensus.chunk_size),
                );
                let path_buff = &chunk.data_path;

                info!(
                    "chunk_offset:{} data_size:{} offset:{}",
                    chunk.tx_offset, chunk.data_size, target_offset
                );

                let path_result = validate_path(root_hash, path_buff, target_offset)
                    .map_err(|_| ChunkIngressError::InvalidProof);

                if let Err(e) = path_result {
                    error!("error validating path: {:?}", e);
                    return Ok(());
                }

                // Use data_size to identify and validate that only the last chunk
                // can be less than chunk_size
                let chunk_len = chunk.bytes.len() as u64;

                // TODO: Mark the data_root as invalid if the chunk is an incorrect size
                // Someone may have created a data_root that seemed valid, but if the
                // data_path is valid but the chunk size doesn't mach the protocols
                // consensus size, then the data_root is actually invalid and no future
                // chunks from that data_root should be ingressed.
                let chunk_size = mempool_state_guard.config.consensus.chunk_size;

                // Is this chunk index any of the chunks before the last in the tx?
                let num_chunks_in_tx = data_size.div_ceil(chunk_size);
                if u64::from(*chunk.tx_offset) < num_chunks_in_tx - 1 {
                    // Ensure prefix chunks are all exactly chunk_size
                    if chunk_len != chunk_size {
                        error!(
                            "{:?}: incomplete not last chunk, tx offset: {} chunk len: {}",
                            ChunkIngressError::InvalidChunkSize,
                            chunk.tx_offset,
                            chunk_len
                        );
                        return Ok(());
                    }
                } else {
                    // Ensure the last chunk is no larger than chunk_size
                    if chunk_len > chunk_size {
                        error!(
                            "{:?}: chunk bigger than max. chunk size, tx offset: {} chunk len: {}",
                            ChunkIngressError::InvalidChunkSize,
                            chunk.tx_offset,
                            chunk_len
                        );
                        return Ok(());
                    }
                }

                if path_result.leaf_hash
                    != hash_sha256(&chunk.bytes.0)
                        .map_err(|_| ChunkIngressError::InvalidDataHash)?
                {
                    return Err(ChunkIngressError::InvalidDataHash);
                }
                // Check that the leaf hash on the data_path matches the chunk_hash

                // TODO: fix all these unwraps!
                // Finally write the chunk to CachedChunks, this will succeed even if the chunk is one that's already inserted

                mempool_state_guard
                    .irys_db
                    .update_eyre(|tx| irys_database::cache_chunk(tx, &chunk))
                    .map_err(|_| ChunkIngressError::DatabaseError)?;

                for sm in mempool_state_guard.storage_modules_guard.read().iter() {
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
                let read_tx = mempool_state_guard
                    .irys_db
                    .tx()
                    .map_err(|_| ChunkIngressError::DatabaseError)?;

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

                    let canon_chain = mempool_state_guard
                        .block_tree_read_guard
                        .read()
                        .get_canonical_chain();

                    let (_, latest_height, _, _) = canon_chain
                        .0
                        .last()
                        .ok_or(ChunkIngressError::ServiceUninitialized)?;

                    let db = mempool_state_guard.irys_db.clone();
                    let signer = mempool_state_guard.config.irys_signer();
                    let latest_height = *latest_height;
                    mempool_state_guard
                        .task_exec
                        .clone()
                        .spawn_blocking(async move {
                            generate_ingress_proof(
                                db.clone(),
                                root_hash,
                                data_size,
                                chunk_size,
                                signer,
                            )
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

                let gossip_sender = mempool_state_guard.gossip_tx.clone();
                let gossip_data = GossipData::Chunk(chunk);

                let _ = tokio::task::spawn(async move {
                    if let Err(error) = gossip_sender.send(gossip_data).await {
                        tracing::error!("Failed to send gossip data: {:?}", error);
                    }
                });

                Ok(())
            }
            MempoolServiceMessage::GetBestMempoolTxs(response) => {
                let reth_db = mempool_state_guard.reth_db.clone();
                let mut fees_spent_per_address = HashMap::new();
                let mut commitment_tx = Vec::new();
                let mut unfunded_address = HashSet::new();

                // Helper function that verifies transaction funding and tracks cumulative fees
                // Returns true if the transaction can be funded based on current account balance
                // and previously included transactions in this block
                let mut check_funding = |tx: &dyn IrysTransactionCommon| -> bool {
                    let signer = tx.signer();

                    // Skip transactions from addresses with previously unfunded transactions
                    // This ensures we don't include any transactions (including pledges) from
                    // addresses that couldn't afford their stake commitments
                    if unfunded_address.contains(&signer) {
                        return false;
                    }

                    let fee = tx.total_fee();
                    let current_spent = *fees_spent_per_address.get(&signer).unwrap_or(&0_u64);

                    // Calculate total required balance including previously selected transactions
                    let tx_ref = &reth_db.tx().unwrap();
                    let has_funds = irys_database::get_account_balance(tx_ref, signer).unwrap()
                        >= U256::from(current_spent + fee);

                    // Track fees for this address regardless of whether this specific transaction is included
                    fees_spent_per_address
                        .entry(signer)
                        .and_modify(|val| *val += fee)
                        .or_insert(fee);

                    // If transaction cannot be funded, mark the entire address as unfunded
                    // Since stakes are processed before pledges, this prevents inclusion of
                    // pledge commitments when their associated stake commitment is unfunded
                    if !has_funds {
                        unfunded_address.insert(signer);
                        return false;
                    }

                    has_funds
                };

                // Process commitments in priority order (stakes then pledges)
                // This order ensures stake transactions are processed before pledges
                for commitment_type in &[CommitmentType::Stake, CommitmentType::Pledge] {
                    // Gather all commitments of current type from all addresses
                    let mut sorted_commitments: Vec<_> = mempool_state_guard
                        .valid_commitment_tx
                        .values()
                        .flat_map(|txs| {
                            txs.iter()
                                .filter(|tx| tx.commitment_type == *commitment_type)
                                .map(|tx| tx.clone())
                        })
                        .collect();

                    // Sort commitments by fee (highest first) to maximize network revenue
                    sorted_commitments.sort_by(|a, b| b.total_fee().cmp(&a.total_fee()));

                    // Select fundable commitments in fee-priority order
                    for tx in sorted_commitments {
                        if check_funding(&tx) {
                            commitment_tx.push(tx);
                        }
                    }
                }

                // Prepare storage transactions for inclusion after commitments
                let mut all_storage_txs: Vec<_> =
                    mempool_state_guard.valid_tx.values().cloned().collect();

                // Sort storage transactions by fee (highest first) to maximize revenue
                all_storage_txs.sort_by(|a, b| b.total_fee().cmp(&a.total_fee()));

                // Apply block size constraint and funding checks to storage transactions
                let mut storage_tx = Vec::new();
                let max_txs = mempool_state_guard
                    .config
                    .node_config
                    .consensus_config()
                    .mempool
                    .max_data_txs_per_block
                    .try_into()
                    .expect("max_data_txs_per_block to fit into usize");

                // Select storage transactions in fee-priority order, respecting funding limits
                // and maximum transaction count per block
                for tx in all_storage_txs {
                    if check_funding(&tx) {
                        storage_tx.push(tx);
                        if storage_tx.len() >= max_txs {
                            break;
                        }
                    }
                }

                let txns = MempoolTxs {
                    commitment_tx,
                    storage_tx,
                };

                // Return selected transactions grouped by type
                if let Err(e) = response.send(txns) {
                    tracing::error!("response.send(txns) error: {:?}", e);
                };

                Ok(())
            }
            MempoolServiceMessage::TxExistenceQuery(txid, response) => {
                let response_value = if mempool_state_guard.valid_tx.contains_key(&txid) {
                    Ok(true)
                } else if mempool_state_guard.recent_valid_tx.contains(&txid) {
                    Ok(true)
                } else if mempool_state_guard.invalid_tx.contains(&txid) {
                    // Still has it, just invalid
                    Ok(true)
                } else {
                    let read_tx = mempool_state_guard.irys_db.as_ref().tx();
                    let result = if read_tx.is_err() {
                        Err(TxIngressError::DatabaseError)
                    } else {
                        let tx_header = tx_header_by_txid(
                            &read_tx.expect("expected valid header from tx id"),
                            &txid,
                        );
                        if tx_header.is_err() {
                            Err(TxIngressError::DatabaseError)
                        } else {
                            Ok(tx_header.expect("exepected ").is_some())
                        }
                    };

                    result
                };

                if let Err(e) = response.send(response_value) {
                    tracing::error!("response.send(bool) error: {:?}", e);
                };

                Ok(())
            }
            MempoolServiceMessage::TxIngressMessage(tx) => {
                debug!(
                    "received tx {:?} (data_root {:?})",
                    &tx.id.0.to_base58(),
                    &tx.data_root.0.to_base58()
                );

                // Early out if we already know about this transaction
                if mempool_state_guard.invalid_tx.contains(&tx.id)
                    || mempool_state_guard.recent_valid_tx.contains(&tx.id)
                {
                    return Err(TxIngressError::Skipped);
                }

                // Validate anchor
                let hdr = self.validate_anchor(&tx.id, &tx.anchor);
                if let Err(e) = self.validate_anchor(&tx.id, &tx.anchor) {
                    error!(
                        "Validation failed: {:?} - mapped to: {:?}",
                        e,
                        TxIngressError::DatabaseError
                    );
                    return Ok(());
                }

                let read_tx = &mempool_state_guard
                    .irys_db
                    .tx()
                    .map_err(|_| TxIngressError::DatabaseError);

                if let Err(e) = read_tx {
                    error!("{:?}", TxIngressError::DatabaseError);
                    return Ok(());
                }

                let read_reth_tx = &mempool_state_guard
                    .reth_db
                    .tx()
                    .map_err(|_| TxIngressError::DatabaseError)?;

                // Update any associated ingress proofs
                if let Ok(Some(old_expiry)) = read_tx.get::<DataRootLRU>(tx.data_root) {
                    let anchor_expiry_depth = mempool_state_guard
                        .config
                        .node_config
                        .consensus_config()
                        .mempool
                        .anchor_expiry_depth as u64;
                    let new_expiry = hdr.height + anchor_expiry_depth;
                    debug!(
                        "Updating ingress proof for data root {} expiry from {} -> {}",
                        &tx.data_root, &old_expiry.last_height, &new_expiry
                    );
                    mempool_state_guard
                        .irys_db
                        .update(|write_tx| write_tx.put::<DataRootLRU>(tx.data_root, old_expiry))
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

                // Check account balance
                if irys_database::get_account_balance(read_reth_tx, tx.signer)
                    .map_err(|_| TxIngressError::DatabaseError)?
                    < U256::from(tx.total_fee())
                {
                    error!(
                        "unfunded balance from irys_database::get_account_balance({:?})",
                        tx.signer
                    );
                    return Err(TxIngressError::Unfunded);
                }

                // Validate the transaction signature
                self.validate_signature(&tx)?;
                mempool_state_guard.valid_tx.insert(tx.id, tx.clone());
                mempool_state_guard.recent_valid_tx.insert(tx.id);

                // Cache the data_root in the database
                match mempool_state_guard.irys_db.update_eyre(|db_tx| {
                    irys_database::cache_data_root(db_tx, &tx)?;
                    // TODO: tx headers should not immediately be added to the database
                    // this is a work around until the mempool can persist its state
                    // during shutdown. Currently this has the potential to create
                    // orphaned tx headers in the database with expired anchors and
                    // not linked to any blocks.
                    irys_database::insert_tx_header(db_tx, &tx)?;
                    Ok(())
                }) {
                    Ok(()) => {
                        info!(
                            "Successfully cached data_root {:?} for tx {:?}",
                            tx.data_root,
                            tx.id.0.to_base58()
                        );
                    }
                    Err(db_error) => {
                        error!(
                            "Failed to cache data_root {:?} for tx {:?}: {:?}",
                            tx.data_root,
                            tx.id.0.to_base58(),
                            db_error
                        );
                    }
                };

                // Process any chunks that arrived before their parent transaction
                // These were temporarily stored in the pending_chunks cache
                if let Some(chunks_map) = mempool_state_guard.pending_chunks.pop(&tx.data_root) {
                    // Extract owned chunks from the map to process them
                    let chunks: Vec<_> = chunks_map.into_iter().map(|(_, chunk)| chunk).collect();

                    // PERFORMANCE CONSIDERATION:
                    // This is executing in a synchronous actor context. If this transaction has
                    // many pending chunks (hundreds or thousands), processing them
                    // all here could block the actor for a significant time, delaying other messages.
                    // This should be addressed when the mempool_service is converted to a tokio service
                    // and the handlers become async
                    for chunk in chunks {
                        // Process each chunk with full ownership (no cloning needed)
                        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
                        self.handle_message(MempoolServiceMessage::ChunkIngressMessage(
                            chunk, oneshot_tx,
                        ));

                        let status = oneshot_rx
                            .await
                            .expect("pending chunks should be processed by the mempool");
                    }
                }

                // Gossip transaction
                let gossip_sender = mempool_state_guard.gossip_tx.clone();
                let gossip_data = GossipData::Transaction(tx.clone());

                let _ = tokio::task::spawn(async move {
                    if let Err(error) = gossip_sender.send(gossip_data).await {
                        tracing::error!("Failed to send gossip data: {:?}", error);
                    }
                });

                Ok(())
            }
        }
    }

    /// Opens a read-only database transaction from the Irys mempool state.
    ///
    /// Returns a `Tx<RO>` handle if successful, or a `ChunkIngressError::DatabaseError`
    /// if the transaction could not be created. Logs an error if the transaction fails.
    fn read_tx(
        &self,
    ) -> Result<irys_database::reth_db::mdbx::tx::Tx<reth_db::mdbx::RO>, ChunkIngressError> {
        let mempool_state = &self.mempool_state.clone();
        let mempool_state_read_guard = mempool_state.read().expect("expected valid mempool state");

        mempool_state_read_guard
            .irys_db
            .tx()
            .map_err(|_| ChunkIngressError::DatabaseError)
            .inspect_err(|e| error!("database error reading tx: {:?}", e))
    }

    /// Removes a commitment transaction with the specified transaction ID from the valid_commitment_tx map
    /// Returns true if the transaction was found and removed, false otherwise
    fn remove_commitment_tx(&mut self, txid: &H256) -> bool {
        let mut found = false;

        let mempool_state = &self.mempool_state.clone();
        let mut mempool_state_guard = mempool_state.write().expect("expected valid mempool state");

        mempool_state_guard.recent_valid_tx.remove(&txid);

        // Create a vector of addresses to update to avoid borrowing issues
        let addresses_to_check: Vec<Address> = mempool_state_guard
            .valid_commitment_tx
            .keys()
            .cloned()
            .collect();

        for address in addresses_to_check {
            if let Some(transactions) = mempool_state_guard.valid_commitment_tx.get_mut(&address) {
                // Find the index of the transaction to remove
                if let Some(index) = transactions.iter().position(|tx| tx.id == *txid) {
                    // Remove the transaction
                    transactions.remove(index);
                    found = true;

                    // If the vector is now empty, remove the entry
                    if transactions.is_empty() {
                        mempool_state_guard.valid_commitment_tx.remove(&address);
                    }

                    // Exit early once we've found and removed the transaction
                    break;
                }
            }
        }

        found
    }

    async fn get_commitment_status(
        &self,
        commitment_tx: &CommitmentTransaction,
    ) -> CommitmentStatus {
        let mempool_state = &self.mempool_state.clone();
        let mempool_state_guard = mempool_state.read().expect("expected valid mempool state");
        // Check if already staked in the blockchain
        let is_staked = mempool_state_guard
            .commitment_state_guard
            .is_staked(commitment_tx.signer);

        // Most commitments are valid by default
        // Only pledges require special validation when not already staked
        let is_pledge = commitment_tx.commitment_type == CommitmentType::Pledge;
        if !is_pledge || is_staked {
            return CommitmentStatus::Accepted;
        }

        // For unstaked pledges, validate against cache and pending transactions
        let commitment_cache = mempool_state_guard.service_senders.commitment_cache.clone();
        let commitment_tx_clone = commitment_tx.clone();

        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        let _ = commitment_cache.send(CommitmentCacheMessage::GetCommitmentStatus {
            commitment_tx: commitment_tx_clone,
            response: oneshot_tx,
        });
        let cache_status = oneshot_rx
            .await
            .expect("to receive CommitmentStatus from GetCommitmentStatus message");

        // Reject unsupported commitment types
        if matches!(cache_status, CommitmentStatus::Unsupported) {
            warn!(
                "Commitment is unsupported: {}",
                commitment_tx.id.0.to_base58()
            );
            return CommitmentStatus::Unsupported;
        }

        // For unstaked addresses, check for pending stake transactions
        if matches!(cache_status, CommitmentStatus::Unstaked) {
            // Get pending transactions for this address
            if let Some(pending) = mempool_state_guard
                .valid_commitment_tx
                .get(&commitment_tx.signer)
            {
                // Check if there's at least one pending stake transaction
                if pending
                    .iter()
                    .any(|c| c.commitment_type == CommitmentType::Stake)
                {
                    return CommitmentStatus::Accepted;
                }
            }

            // No pending stakes found
            warn!(
                "Pledge Commitment is unstaked: {}",
                commitment_tx.id.0.to_base58()
            );
            return CommitmentStatus::Unstaked;
        }

        // All other cases are valid
        CommitmentStatus::Accepted
    }

    // Helper to validate anchor
    fn validate_anchor(
        &mut self,
        tx_id: &IrysTransactionId,
        anchor: &H256,
    ) -> Result<IrysBlockHeader, TxIngressError> {
        let mempool_state = &self.mempool_state.clone();
        let mempool_state_read_guard = mempool_state
            .read()
            .expect("expected valid mempool state read guard");

        let read_tx = self.read_tx().map_err(|_| TxIngressError::DatabaseError)?;

        let latest_height = self.get_latest_block_height()?;
        let anchor_expiry_depth = mempool_state_read_guard
            .config
            .node_config
            .consensus_config()
            .mempool
            .anchor_expiry_depth as u64;

        // Allow transactions to use the txid of a transaction in the mempool
        if mempool_state_read_guard.recent_valid_tx.contains(anchor) {
            let (canonical_blocks, _) = mempool_state_read_guard
                .block_tree_read_guard
                .read()
                .get_canonical_chain();
            let (latest_block_hash, _, _, _) = canonical_blocks.last().unwrap();
            // Just provide the most recent block as an anchor
            match irys_database::block_header_by_hash(&read_tx, latest_block_hash, false) {
                Ok(Some(hdr)) if hdr.height + anchor_expiry_depth >= latest_height => {
                    debug!("valid txid anchor {} for tx {}", anchor, tx_id);
                    return Ok(hdr);
                }
                _ => {}
            };
        }

        drop(mempool_state_read_guard); // Release read lock before acquiring write lock

        match irys_database::block_header_by_hash(&read_tx, anchor, false) {
            Ok(Some(hdr)) if hdr.height + anchor_expiry_depth >= latest_height => {
                debug!("valid block hash anchor {} for tx {}", anchor, tx_id);
                Ok(hdr)
            }
            _ => {
                let mut mempool_state_write_guard = mempool_state
                    .write()
                    .expect("expected valid mempool write guard");
                mempool_state_write_guard.invalid_tx.push(tx_id.clone());
                warn!("Invalid anchor value {} for tx {}", anchor, tx_id);
                Err(TxIngressError::InvalidAnchor)
            }
        }
    }

    // Helper to get the canonical chain and latest height
    fn get_latest_block_height(&self) -> Result<u64, TxIngressError> {
        let mempool_state = &self.mempool_state.clone();
        let mempool_state_read_guard = mempool_state.read().expect("expected valid mempool state");
        let canon_chain = mempool_state_read_guard
            .block_tree_read_guard
            .read()
            .get_canonical_chain();
        drop(mempool_state_read_guard);
        let (_, latest_height, _, _) = canon_chain.0.last().ok_or(TxIngressError::Other(
            "unable to get canonical chain from block tree".to_owned(),
        ))?;

        Ok(*latest_height)
    }

    // Helper to verify signature
    fn validate_signature<T: IrysTransactionCommon>(
        &mut self,
        tx: &T,
    ) -> Result<(), TxIngressError> {
        if tx.is_signature_valid() {
            info!("Signature is valid");
            Ok(())
        } else {
            let mempool_state = &self.mempool_state.clone();
            mempool_state
                .write()
                .expect("expected valid mempool state")
                .invalid_tx
                .push(tx.id());
            debug!("Signature is NOT valid");
            Err(TxIngressError::InvalidSignature)
        }
    }
}

/// Create a new instance of the mempool state passing in a reference
/// counted reference to a `DatabaseEnv`, a copy of reth's task executor and the miner's signer
pub fn create_state(
    irys_db: DatabaseProvider,
    reth_db: RethDbWrapper,
    task_exec: TaskExecutor,
    block_tree_guard: BlockTreeReadGuard,
    commitment_state_guard: CommitmentStateReadGuard,
    storage_modules_guard: StorageModulesReadGuard,
    config: &Config,
    service_senders: ServiceSenders,
    gossip_tx: tokio::sync::mpsc::Sender<GossipData>,
) -> MempoolState {
    let mempool_config = &config.consensus.mempool;
    let max_pending_chunk_items = mempool_config.max_pending_chunk_items;
    let max_pending_pledge_items = mempool_config.max_pending_pledge_items;
    MempoolState {
        irys_db,
        reth_db,
        valid_tx: BTreeMap::new(),
        valid_commitment_tx: BTreeMap::new(),
        invalid_tx: Vec::new(),
        task_exec,
        config: config.clone(),
        storage_modules_guard,
        block_tree_read_guard: block_tree_guard,
        commitment_state_guard,
        service_senders,
        gossip_tx,
        recent_valid_tx: HashSet::new(),
        pending_chunks: LruCache::new(NonZeroUsize::new(max_pending_chunk_items).unwrap()),
        pending_pledges: LruCache::new(NonZeroUsize::new(max_pending_pledge_items).unwrap()),
    }
}
