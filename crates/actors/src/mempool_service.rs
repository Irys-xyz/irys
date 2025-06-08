use crate::block_tree_service::{BlockMigratedEvent, BlockTreeReadGuard, ReorgEvent};
use crate::services::ServiceSenders;
use crate::{CommitmentCacheMessage, CommitmentCacheStatus, CommitmentStateReadGuard};
use base58::ToBase58 as _;
use core::fmt::Display;
use eyre::eyre;
use futures::future::BoxFuture;
use irys_database::insert_commitment_tx;
use irys_database::{
    db::{IrysDatabaseExt as _, IrysDupCursorExt as _},
    db_cache::{data_size_to_chunk_count, DataRootLRUEntry},
    submodule::get_data_size_by_data_root,
    tables::{CachedChunks, CachedChunksIndex, DataRootLRU, IngressProofs},
    {insert_tx_header, tx_header_by_txid},
};
use irys_primitives::CommitmentType;
use irys_reth_node_bridge::{ext::IrysRethRpcTestContextExt, IrysRethNodeAdapter};
use irys_storage::{get_atomic_file, RecoveredMempoolState, StorageModulesReadGuard};
use irys_types::{
    app_state::DatabaseProvider, chunk::UnpackedChunk, hash_sha256, irys::IrysSigner,
    validate_path, Address, CommitmentTransaction, Config, DataLedger, DataRoot, GossipData,
    IrysBlockHeader, IrysTransactionCommon, IrysTransactionHeader, IrysTransactionId,
    MempoolConfig, TxChunkOffset, H256, U256,
};
use lru::LruCache;
use reth::rpc::types::BlockId;
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
use reth_db::{
    cursor::DbDupCursorRO as _, transaction::DbTx as _, transaction::DbTxMut as _, Database as _,
    DatabaseError,
};
use std::fs;
use std::io::Write;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    num::NonZeroUsize,
    pin::pin,
    sync::Arc,
};
use tokio::sync::{broadcast, oneshot};
use tokio::{
    sync::{mpsc::UnboundedReceiver, mpsc::UnboundedSender, RwLock},
    task::JoinHandle,
};
use tracing::{debug, error, info, warn};

#[async_trait::async_trait]
pub trait MempoolFacade: Clone + Send + Sync + 'static {
    async fn handle_data_transaction(
        &self,
        tx_header: IrysTransactionHeader,
    ) -> Result<(), TxIngressError>;
    async fn handle_commitment_transaction(
        &self,
        tx_header: CommitmentTransaction,
    ) -> Result<(), TxIngressError>;
    async fn handle_chunk(&self, chunk: UnpackedChunk) -> Result<(), ChunkIngressError>;
    async fn is_known_tx(&self, tx_id: H256) -> Result<bool, TxIngressError>;
}

#[derive(Clone, Debug)]
pub struct MempoolServiceFacadeImpl {
    service: UnboundedSender<MempoolServiceMessage>,
}

impl From<UnboundedSender<MempoolServiceMessage>> for MempoolServiceFacadeImpl {
    fn from(value: UnboundedSender<MempoolServiceMessage>) -> Self {
        Self { service: value }
    }
}

#[async_trait::async_trait]
impl MempoolFacade for MempoolServiceFacadeImpl {
    async fn handle_data_transaction(
        &self,
        tx_header: IrysTransactionHeader,
    ) -> Result<(), TxIngressError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        self.service
            .send(MempoolServiceMessage::TxIngressMessage(
                tx_header, oneshot_tx,
            ))
            .map_err(|_| TxIngressError::Other("Error sending TxIngressMessage ".to_owned()))?;

        oneshot_rx.await.expect("to process TxIngressMessage")
    }

    async fn handle_commitment_transaction(
        &self,
        commitment_tx: CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        self.service
            .send(MempoolServiceMessage::CommitmentTxIngressMessage(
                commitment_tx,
                oneshot_tx,
            ))
            .map_err(|_| {
                TxIngressError::Other("Error sending CommitmentTxIngressMessage ".to_owned())
            })?;

        oneshot_rx
            .await
            .expect("to process CommitmentTxIngressMessage")
    }

    async fn handle_chunk(&self, chunk: UnpackedChunk) -> Result<(), ChunkIngressError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        self.service
            .send(MempoolServiceMessage::ChunkIngressMessage(
                chunk, oneshot_tx,
            ))
            .map_err(|_| {
                ChunkIngressError::Other("Error sending ChunkIngressMessage ".to_owned())
            })?;

        oneshot_rx.await.expect("to process ChunkIngressMessage")
    }

    async fn is_known_tx(&self, tx_id: H256) -> Result<bool, TxIngressError> {
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        self.service
            .send(MempoolServiceMessage::TxExistenceQuery(tx_id, oneshot_tx))
            .map_err(|_| TxIngressError::Other("Error sending TxExistenceQuery ".to_owned()))?;

        oneshot_rx.await.expect("to process TxExistenceQuery")
    }
}

#[derive(Debug)]
pub struct MempoolState {
    /// Temporary mempool stubs - will replace with proper data models - `DMac`
    valid_tx: BTreeMap<H256, IrysTransactionHeader>,
    valid_commitment_tx: BTreeMap<Address, Vec<CommitmentTransaction>>,
    /// The miner's signer instance, used to sign ingress proofs
    invalid_tx: Vec<H256>,
    /// Tracks recent valid txids from either storage or commitment
    recent_valid_tx: HashSet<H256>,
    /// LRU caches for out of order gossip data
    pending_chunks: LruCache<DataRoot, LruCache<TxChunkOffset, UnpackedChunk>>,
    pending_pledges: LruCache<Address, LruCache<IrysTransactionId, CommitmentTransaction>>,
}

pub type AtomicMempoolState = Arc<RwLock<MempoolState>>;

/// Messages that the Mempool Service handler supports
#[derive(Debug)]
pub enum MempoolServiceMessage {
    /// Block Confirmed, remove confirmed txns from mempool
    BlockConfirmedMessage(Arc<IrysBlockHeader>, Arc<Vec<IrysTransactionHeader>>),
    /// Get IrysTransactionHeader
    GetTransaction(H256, oneshot::Sender<Option<IrysTransactionHeader>>),
    /// Ingress Chunk, Add to CachedChunks, generate_ingress_proof, gossip chunk
    ChunkIngressMessage(
        UnpackedChunk,
        oneshot::Sender<Result<(), ChunkIngressError>>,
    ),
    /// Ingress CommitmentTransaction into the mempool
    ///
    /// This function performs a series of checks and validations:
    /// - Skips the transaction if it is already known to be invalid or previously processed
    /// - Validates the transaction's anchor and signature
    /// - Inserts the valid transaction into the mempool and database
    /// - Processes any pending pledge transactions that depended on this commitment
    /// - Gossips the transaction to peers if accepted
    /// - Caches the transaction for unstaked signers to be reprocessed later
    CommitmentTxIngressMessage(
        CommitmentTransaction,
        oneshot::Sender<Result<(), TxIngressError>>,
    ),
    /// Return filtered list of candidate txns
    /// Filtering based on funding status etc based on the provided EVM block ID
    /// If `None` is provided, the latest canonical block is used
    GetBestMempoolTxs(Option<BlockId>, oneshot::Sender<MempoolTxs>),
    /// Retrieves a list of CommitmentTransactions based on the provided tx ids
    GetCommitmentTxs {
        commitment_tx_ids: Vec<IrysTransactionId>,
        response: oneshot::Sender<HashMap<IrysTransactionId, CommitmentTransaction>>,
    },
    /// Confirm if tx exists in database
    TxExistenceQuery(H256, oneshot::Sender<Result<bool, TxIngressError>>),
    /// validate and process an incoming IrysTransactionHeader
    TxIngressMessage(
        IrysTransactionHeader,
        oneshot::Sender<Result<(), TxIngressError>>,
    ),
}

#[derive(Debug)]
struct Inner {
    block_tree_read_guard: BlockTreeReadGuard,
    commitment_state_guard: CommitmentStateReadGuard,
    config: Config,
    /// `task_exec` is used to spawn background jobs on reth's MT tokio runtime
    /// instead of the actor executor runtime, while also providing some `QoL`
    exec: TaskExecutor,
    irys_db: DatabaseProvider,
    reth_node_adapter: IrysRethNodeAdapter,
    mempool_state: AtomicMempoolState,
    /// Reference to all the services we can send messages to
    service_senders: ServiceSenders,
    storage_modules_guard: StorageModulesReadGuard,
}

/// The Mempool oversees pending transactions and validation of incoming tx.
#[derive(Debug)]
pub struct MempoolService {
    shutdown: GracefulShutdown,
    msg_rx: UnboundedReceiver<MempoolServiceMessage>, // mempool message receiver
    reorg_rx: broadcast::Receiver<ReorgEvent>,        // reorg broadcast receiver
    block_migrated_rx: broadcast::Receiver<BlockMigratedEvent>, // block broadcast migrated receiver
    inner: Inner,
}

impl Default for MempoolService {
    fn default() -> Self {
        unimplemented!("don't rely on the default implementation of the `MempoolService`");
    }
}

impl MempoolService {
    /// Spawn a new Mempool service
    pub async fn spawn_service(
        exec: &TaskExecutor,
        irys_db: DatabaseProvider,
        reth_node_adapter: IrysRethNodeAdapter,
        storage_modules_guard: StorageModulesReadGuard,
        block_tree_read_guard: &BlockTreeReadGuard,
        commitment_state_guard: &CommitmentStateReadGuard,
        rx: UnboundedReceiver<MempoolServiceMessage>,
        config: &Config,
        service_senders: &ServiceSenders,
    ) -> eyre::Result<JoinHandle<()>> {
        info!("mempool service spawned");
        let block_tree_read_guard = block_tree_read_guard.clone();
        let config = config.clone();
        let mempool_config = &config.consensus.mempool;
        let mempool_state = create_state(mempool_config);
        let exec = exec.clone();
        let commitment_state_guard = commitment_state_guard.clone();
        let storage_modules_guard = storage_modules_guard.clone();
        let service_senders = service_senders.clone();
        let reorg_rx = service_senders.subscribe_reorgs();
        let block_migrated_rx = service_senders.subscribe_block_migrated();

        Ok(exec.clone().spawn_critical_with_graceful_shutdown_signal(
            "Mempool Service",
            |shutdown| async move {
                let mempool_service = Self {
                    shutdown,
                    msg_rx: rx,
                    reorg_rx,
                    block_migrated_rx,
                    inner: Inner {
                        block_tree_read_guard,
                        commitment_state_guard,
                        config,
                        exec,
                        irys_db,
                        mempool_state: Arc::new(RwLock::new(mempool_state)),
                        reth_node_adapter,
                        service_senders,
                        storage_modules_guard,
                    },
                };
                mempool_service
                    .start()
                    .await
                    .expect("Mempool service encountered an irrecoverable error")
            },
        ))
    }

    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting Mempool service");

        self.inner.restore_mempool_from_disk().await;

        let mut shutdown_future = pin!(self.shutdown);
        let shutdown_guard = loop {
            tokio::select! {
                // Handle regular mempool messages
                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(msg) => {
                            self.inner.handle_message(msg).await?;
                        }
                        None => {
                            tracing::warn!("receiver channel closed");
                            break None;
                        }
                    }
                }

                // Handle reorg events
                reorg_result = self.reorg_rx.recv() => {
                    if let Some(event) = handle_broadcast_recv(reorg_result, "Reorg") {
                        self.inner.handle_reorg(event).await?;
                    }
                }

                // Handle block migrated events
                 migrated_result = self.block_migrated_rx.recv() => {
                    if let Some(event) = handle_broadcast_recv(migrated_result, "BlockMigrated") {
                        self.inner.handle_block_migrated(event).await?;
                    }
                }


                // Handle shutdown signal
                shutdown = &mut shutdown_future => {
                    tracing::warn!("shutdown signal received");
                    break Some(shutdown);
                }
            }
        };

        tracing::debug!(amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.inner.handle_message(msg).await?;
        }

        // explicitly inform the TaskManager that we're shutting down
        drop(shutdown_guard);

        self.inner.persist_mempool_to_disk().await?;

        tracing::info!("shutting down Mempool service");
        Ok(())
    }
}

fn handle_broadcast_recv<T>(
    result: Result<T, broadcast::error::RecvError>,
    channel_name: &str,
) -> Option<T> {
    match result {
        Ok(event) => Some(event),
        Err(broadcast::error::RecvError::Closed) => {
            tracing::debug!("{} channel closed", channel_name);
            None
        }
        Err(broadcast::error::RecvError::Lagged(n)) => {
            tracing::warn!("{} lagged by {} events", channel_name, n);
            if n > 5 {
                tracing::error!("{} significantly lagged", channel_name);
            }
            None
        }
    }
}

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

/// Reasons why Chunk Ingress might fail
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
    /// Catch-all variant for other errors.
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

#[derive(Debug)]
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
    assert_eq!({ chunk_count }, expected_chunk_count);

    ro_tx.commit()?;

    db.update(|rw_tx| rw_tx.put::<IngressProofs>(data_root, proof))??;

    Ok(())
}

impl Inner {
    async fn handle_transaction_message(&self, tx: H256) -> Option<IrysTransactionHeader> {
        let mempool_state = &self.mempool_state.clone();
        let mempool_state_guard = mempool_state.read().await;
        // if tx exists
        if let Some(tx_header) = mempool_state_guard.valid_tx.get(&tx) {
            return Some(tx_header.clone());
        }
        drop(mempool_state_guard);

        if let Ok(read_tx) = self.read_tx().await {
            let tx_header = tx_header_by_txid(&read_tx, &tx).unwrap_or(None);
            return tx_header.clone();
        }

        None
    }

    async fn handle_commitment_tx_ingress_message(
        &mut self,
        commitment_tx: CommitmentTransaction,
    ) -> Result<(), TxIngressError> {
        debug!(
            "received commitment tx {:?}",
            &commitment_tx.id.0.to_base58()
        );

        let mempool_state = &self.mempool_state.clone();
        let mempool_state_guard = mempool_state.read().await;

        // Early out if we already know about this transaction (invalid)
        if mempool_state_guard.invalid_tx.contains(&commitment_tx.id) {
            return Err(TxIngressError::Skipped);
        }

        // Check if the transaction already exists in valid transactions
        let tx_exists = mempool_state_guard
            .valid_commitment_tx
            .get(&commitment_tx.signer)
            .is_some_and(|txs| txs.iter().any(|c| c.id == commitment_tx.id));

        drop(mempool_state_guard);

        if tx_exists {
            return Err(TxIngressError::Skipped);
        }

        // Validate the tx anchor
        if let Err(e) = self
            .validate_anchor(&commitment_tx.id, &commitment_tx.anchor)
            .await
        {
            tracing::warn!(
                "Anchor {:?} for tx {:?} failure with error: {:?}",
                &commitment_tx.anchor,
                commitment_tx.id,
                e
            );
            return Err(TxIngressError::InvalidAnchor);
        }

        // Check pending commitments and cached commitments and active commitments
        let commitment_status = self.get_commitment_status(&commitment_tx).await;
        if commitment_status == CommitmentCacheStatus::Accepted {
            // Validate tx signature
            if let Err(e) = self.validate_signature(&commitment_tx).await {
                tracing::error!(
                    "Signature validation for commitment_tx {:?} failed with error: {:?}",
                    &commitment_tx,
                    e
                );
                return Err(TxIngressError::InvalidSignature);
            }

            let mut mempool_state_guard = mempool_state.write().await;
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
            let pop = mempool_state_guard
                .pending_pledges
                .pop(&commitment_tx.signer);
            drop(mempool_state_guard);
            if let Some(pledges_lru) = pop {
                // Extract all pending pledges as a vector of owned transactions
                let pledges: Vec<_> = pledges_lru
                    .into_iter()
                    .map(|(_, pledge_tx)| pledge_tx)
                    .collect();

                for pledge_tx in pledges {
                    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
                    // todo switch _ to actually handle the result
                    let _ = self
                        .handle_message(MempoolServiceMessage::CommitmentTxIngressMessage(
                            pledge_tx, oneshot_tx,
                        ))
                        .await;

                    let _ = oneshot_rx
                        .await
                        .expect("to process pending pledge for newly staked address");
                }
            }

            // // HACK HACK: in order for block discovery to validate incoming blocks
            // // it needs to read commitment tx from the database. Ideally it should
            // // be reading them from the mempool_service in memory cache, but we are
            // // putting off that work until the actix mempool_service is rewritten as a
            // // tokio service.
            // match self.irys_db.update_eyre(|db_tx| {
            //     irys_database::insert_commitment_tx(db_tx, &commitment_tx)?;
            //     Ok(())
            // }) {
            //     Ok(()) => {
            //         info!(
            //             "Successfully stored commitment_tx in db {:?}",
            //             commitment_tx.id.0.to_base58()
            //         );
            //     }
            //     Err(db_error) => {
            //         error!(
            //             "Failed to store commitment_tx in db {:?}: {:?}",
            //             commitment_tx.id.0.to_base58(),
            //             db_error
            //         );
            //     }
            // }

            // Gossip transaction
            self.service_senders
                .gossip_broadcast
                .send(GossipData::CommitmentTransaction(commitment_tx.clone()))
                .expect("Failed to send gossip data");
        } else if commitment_status == CommitmentCacheStatus::Unstaked {
            // For unstaked pledges, we cache them in a 2-level LRU structure:
            // Level 1: Keyed by signer address (allows tracking multiple addresses)
            // Level 2: Keyed by transaction ID (allows tracking multiple pledge tx per address)

            let mut mempool_state_guard = mempool_state.write().await;
            if let Some(pledges_cache) = mempool_state_guard
                .pending_pledges
                .get_mut(&commitment_tx.signer)
            {
                // Address already exists in cache - add this pledge transaction to its lru cache
                pledges_cache.put(commitment_tx.id, commitment_tx.clone());
            } else {
                // First pledge from this address - create a new nested lru cache
                let max_pending_pledge_items =
                    self.config.consensus.mempool.max_pending_pledge_items;
                let mut new_address_cache =
                    LruCache::new(NonZeroUsize::new(max_pending_pledge_items).unwrap());

                // Add the pledge transaction to the new lru cache for the address
                new_address_cache.put(commitment_tx.id, commitment_tx.clone());

                // Add the address cache to the primary lru cache
                mempool_state_guard
                    .pending_pledges
                    .put(commitment_tx.signer, new_address_cache);
            }
            drop(mempool_state_guard)
        } else {
            return Err(TxIngressError::Skipped);
        }
        Ok(())
    }

    async fn handle_block_confirmed_message(
        &mut self,
        block: Arc<IrysBlockHeader>,
        all_txs: Arc<Vec<IrysTransactionHeader>>,
    ) -> Result<(), TxIngressError> {
        let mempool_state = &self.mempool_state.clone();
        let mut mempool_state_write_guard = mempool_state.write().await;
        for txid in block.data_ledgers[DataLedger::Submit].tx_ids.iter() {
            // Remove the submit tx from the pending valid_tx pool
            mempool_state_write_guard.valid_tx.remove(txid);
            mempool_state_write_guard.recent_valid_tx.remove(txid);
        }
        drop(mempool_state_write_guard);

        let published_txids = &block.data_ledgers[DataLedger::Publish].tx_ids.0;

        // Loop though the promoted transactions and remove their ingress proofs
        // from the mempool. In the future on a multi node network we may keep
        // ingress proofs around longer to account for re-orgs, but for now
        // we just remove them.
        // FIXME: Note above about re-orgs!
        if !published_txids.is_empty() {
            let mut_tx = self
                .irys_db
                .tx_mut()
                .map_err(|e| {
                    error!("Failed to create mdbx transaction: {}", e);
                })
                .expect("expected to read/write to database");

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

            mut_tx.commit().expect("expect to commit to database");
        }

        info!(
            "Removing confirmed tx - Block height: {} num tx: {}",
            block.height,
            all_txs.len()
        );

        Ok(())
    }

    async fn handle_reorg(&self, event: ReorgEvent) -> eyre::Result<()> {
        tracing::debug!(
            "Processing reorg: {} orphaned blocks from height {}",
            event.old_fork.len(),
            event.fork_parent.height
        );

        // TODO: Implement mempool-specific reorg handling
        // 1. Check to see that orphaned submit ledger tx are available in the mempool if not included in the new fork (canonical chain)
        // 2. Re-post any reorged submit ledger transactions though handle_tx_ingress_message so account balances and anchors are checked
        // 3. Filter out any invalidated transactions
        // 4. If a transaction was promoted in the orphaned fork but not the new canonical chain, restore ingress proof state to mempool
        // 5. If a transaction was promoted in both forks, make sure the transaction has the ingress proofs from the canonical fork
        // 6. Similar work with commitment transactions (stake and pledge)
        //    - This may require adding some features to the commitment_cache so that stake/pledge tx can be rolled back and new ones applied

        tracing::info!("Reorg handled, new tip: {}", event.new_tip.0.to_base58());

        Ok(())
    }

    /// When a block is migrated from the block_tree to the block_index at the migration depth
    /// it moves from "the cache" (largely the mempool) to "the index" (long term storage, usually
    /// in a database or disk)
    async fn handle_block_migrated(&mut self, event: BlockMigratedEvent) -> eyre::Result<()> {
        tracing::debug!(
            "Processing block migrated broadcast: {} height: {}",
            event.block.block_hash,
            event.block.height
        );

        let migrated_block = event.block;
        let commitment_tx_ids = migrated_block.get_commitment_ledger_tx_ids();
        let commitments = self.handle_get_commitment_txs(commitment_tx_ids).await;

        let tx = self
            .irys_db
            .tx_mut()
            .expect("to get a mutable tx reference from the db");

        for commitment_tx in commitments.values() {
            // Insert the commitment transaction in to the db, perform migration
            insert_commitment_tx(&tx, commitment_tx)?;
            // Remove the commitment tx from the mempool cache, completing the migration
            self.remove_commitment_tx(&commitment_tx.id).await;
        }
        tx.inner.commit()?;

        // TODO: Also migrate publish and submit ledger tx

        Ok(())
    }

    async fn handle_chunk_ingress_message(
        &self,
        chunk: UnpackedChunk,
    ) -> Result<(), ChunkIngressError> {
        let mempool_state = &self.mempool_state;
        // TODO: maintain a shared read transaction so we have read isolation
        let max_chunks_per_item = self.config.consensus.mempool.max_chunks_per_item;

        info!(data_root = ?chunk.data_root, number = ?chunk.tx_offset, "Processing chunk");

        // Check to see if we have a cached data_root for this chunk
        let read_tx = self
            .read_tx()
            .await
            .map_err(|_| ChunkIngressError::DatabaseError)?;

        let binding = self.storage_modules_guard.read().clone();
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
                let mut mempool_state_write_guard = mempool_state.write().await;
                // We don't have a data_root for this chunk but possibly the transaction containing this
                // chunks data_root will arrive soon. Park it in the pending chunks LRU cache until it does.
                if let Some(chunks_map) = mempool_state_write_guard
                    .pending_chunks
                    .get_mut(&chunk.data_root)
                {
                    chunks_map.put(chunk.tx_offset, chunk.clone());
                } else {
                    // If there's no entry for this data_root yet, create one
                    let mut new_lru_cache = LruCache::new(
                        NonZeroUsize::new(max_chunks_per_item)
                            .expect("expected valid NonZeroUsize::new"),
                    );
                    new_lru_cache.put(chunk.tx_offset, chunk.clone());
                    mempool_state_write_guard
                        .pending_chunks
                        .put(chunk.data_root, new_lru_cache);
                }
                drop(mempool_state_write_guard);
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
            return Err(ChunkIngressError::InvalidDataSize);
        }

        let mempool_state_guard = mempool_state.read().await;

        // Next validate the data_path/proof for the chunk, linking
        // data_root->chunk_hash
        let root_hash = chunk.data_root.0;
        let target_offset = u128::from(chunk.end_byte_offset(self.config.consensus.chunk_size));
        let path_buff = &chunk.data_path;

        info!(
            "chunk_offset:{} data_size:{} offset:{}",
            chunk.tx_offset, chunk.data_size, target_offset
        );

        let path_result = match validate_path(root_hash, path_buff, target_offset)
            .map_err(|_| ChunkIngressError::InvalidProof)
        {
            Err(e) => {
                error!("error validating path: {:?}", e);
                return Err(e);
            }
            Ok(v) => v,
        };

        // Use data_size to identify and validate that only the last chunk
        // can be less than chunk_size
        let chunk_len = chunk.bytes.len() as u64;

        // TODO: Mark the data_root as invalid if the chunk is an incorrect size
        // Someone may have created a data_root that seemed valid, but if the
        // data_path is valid but the chunk size doesn't mach the protocols
        // consensus size, then the data_root is actually invalid and no future
        // chunks from that data_root should be ingressed.
        let chunk_size = self.config.consensus.chunk_size;

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

        // Check that the leaf hash on the data_path matches the chunk_hash
        match hash_sha256(&chunk.bytes.0).map_err(|_| ChunkIngressError::InvalidDataHash) {
            Err(e) => {
                error!("{:?}: hashed chunk_bytes hash_sha256() errored!", e);
                return Err(e);
            }
            Ok(hash_256) => {
                if path_result.leaf_hash != hash_256 {
                    warn!(
                        "{:?}: leaf_hash does not match hashed chunk_bytes",
                        ChunkIngressError::InvalidDataHash,
                    );
                    return Err(ChunkIngressError::InvalidDataHash);
                }
            }
        }

        // Finally write the chunk to CachedChunks, this will succeed even if the chunk is one that's already inserted
        if let Err(e) = self
            .irys_db
            .update_eyre(|tx| irys_database::cache_chunk(tx, &chunk))
            .map_err(|_| ChunkIngressError::DatabaseError)
        {
            error!("Database error: {:?}", e);
            return Err(e);
        }

        for sm in self.storage_modules_guard.read().iter() {
            if !sm
                .get_writeable_offsets(&chunk)
                .unwrap_or_default()
                .is_empty()
            {
                info!(target: "irys::mempool::chunk_ingress", "Writing chunk with offset {} for data_root {} to sm {}", &chunk.tx_offset, &chunk.data_root, &sm.id );
                let result = sm
                    .write_data_chunk(&chunk)
                    .map_err(|_| ChunkIngressError::Other("Internal error".to_owned()));
                if let Err(e) = result {
                    error!("Internal error: {:?}", e);
                    return Err(e);
                }
            }
        }

        // ==== INGRESS PROOFS ====
        let root_hash: H256 = root_hash.into();

        // check if we have generated an ingress proof for this tx already
        // if we have, update it's expiry height

        //  TODO: hook into whatever manages ingress proofs
        match read_tx
            .get::<IngressProofs>(root_hash)
            .map_err(|_| ChunkIngressError::DatabaseError)
        {
            Err(e) => {
                error!("Database error: {:?}", e);
                return Err(e);
            }
            Ok(v) => {
                if v.is_some() {
                    info!(
                        "We've already generated an ingress proof for data root {}",
                        &root_hash
                    );

                    return Ok(());
                };
            }
        }

        // check if we have all the chunks for this tx
        let read_tx = self
            .read_tx()
            .await
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

            let canon_chain = self.block_tree_read_guard.read().get_canonical_chain();

            let (_, latest_height, _, _) = canon_chain
                .0
                .last()
                .ok_or(ChunkIngressError::ServiceUninitialized)
                .unwrap();

            let db = self.irys_db.clone();
            let signer = self.config.irys_signer();
            let latest_height = *latest_height;
            self.exec.clone().spawn_blocking(async move {
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
        drop(mempool_state_guard);

        let gossip_sender = &self.service_senders.gossip_broadcast.clone();
        let gossip_data = GossipData::Chunk(chunk);

        if let Err(error) = gossip_sender.send(gossip_data) {
            tracing::error!("Failed to send gossip data: {:?}", error);
        }

        Ok(())
    }

    async fn handle_get_best_mempool_txs(
        &self,
        parent_evm_block_id: Option<BlockId>,
    ) -> MempoolTxs {
        let mempool_state = &self.mempool_state;
        let mut fees_spent_per_address = HashMap::new();
        let mut confirmed_commitments = HashSet::new();
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

            // get balance state for the block we're building off of
            let balance: U256 = self
                .reth_node_adapter
                .rpc
                .get_balance_irys(signer, parent_evm_block_id);

            let has_funds = balance >= U256::from(current_spent + fee);

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

        // Get a list of all recently confirmed commitment txids in the canonical chain
        let (canonical, _) = self.block_tree_read_guard.read().get_canonical_chain();
        for (block_hash, _, _, _) in canonical {
            // TODO: replace this with data from the canonical chain entry when block_tree refactors the tuple
            let commitment_tx_ids = self
                .block_tree_read_guard
                .read()
                .get_block(&block_hash)
                .unwrap()
                .get_commitment_ledger_tx_ids();

            // Remove any confirmed commitment tx
            for tx_id in commitment_tx_ids {
                confirmed_commitments.insert(tx_id);
            }
        }

        // Process commitments in priority order (stakes then pledges)
        // This order ensures stake transactions are processed before pledges

        let mempool_state_guard = mempool_state.read().await;

        for commitment_type in &[CommitmentType::Stake, CommitmentType::Pledge] {
            // Gather all commitments of current type from all addresses
            let mut sorted_commitments: Vec<_> = mempool_state_guard
                .valid_commitment_tx
                .values()
                .flat_map(|txs| {
                    txs.iter()
                        .filter(|tx| tx.commitment_type == *commitment_type)
                        .cloned()
                })
                .collect();

            // Sort commitments by fee (highest first) to maximize network revenue
            sorted_commitments.sort_by_key(|b| std::cmp::Reverse(b.total_fee()));

            // Select fundable commitments in fee-priority order
            for tx in sorted_commitments {
                if confirmed_commitments.contains(&tx.id) {
                    continue; // Skip already confirmed
                }
                if check_funding(&tx) {
                    commitment_tx.push(tx);
                }
            }
        }

        // Prepare storage transactions for inclusion after commitments
        let mut all_storage_txs: Vec<_> = mempool_state_guard.valid_tx.values().cloned().collect();

        drop(mempool_state_guard);

        // Sort storage transactions by fee (highest first) to maximize revenue
        all_storage_txs.sort_by_key(|b| std::cmp::Reverse(b.total_fee()));

        // Apply block size constraint and funding checks to storage transactions
        let mut storage_tx = Vec::new();
        let max_txs = self
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

        // Return selected transactions grouped by type
        MempoolTxs {
            commitment_tx,
            storage_tx,
        }
    }

    async fn handle_tx_ingress_message(
        &mut self,
        tx: IrysTransactionHeader,
    ) -> Result<(), TxIngressError> {
        debug!(
            "received tx {:?} (data_root {:?})",
            &tx.id.0.to_base58(),
            &tx.data_root.0.to_base58()
        );

        let mempool_state = &self.mempool_state.clone();
        let mempool_state_read_guard = mempool_state.read().await;

        // Early out if we already know about this transaction
        if mempool_state_read_guard.invalid_tx.contains(&tx.id)
            || mempool_state_read_guard.recent_valid_tx.contains(&tx.id)
        {
            error!("error: {:?}", TxIngressError::Skipped);
            return Err(TxIngressError::Skipped);
        }
        drop(mempool_state_read_guard);

        // Validate anchor
        let hdr = match self.validate_anchor(&tx.id, &tx.anchor).await {
            Err(e) => {
                error!(
                    "Validation failed: {:?} - mapped to: {:?}",
                    e,
                    TxIngressError::DatabaseError
                );
                return Ok(());
            }
            Ok(v) => v,
        };

        let read_tx = self
            .read_tx()
            .await
            .map_err(|_| TxIngressError::DatabaseError)?;

        // Update any associated ingress proofs
        if let Ok(Some(old_expiry)) = read_tx.get::<DataRootLRU>(tx.data_root) {
            let anchor_expiry_depth = self
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

            self.irys_db
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

        if self.reth_node_adapter.rpc.get_balance_irys(tx.signer, None) < U256::from(tx.total_fee())
        {
            error!(
                "{:?}: unfunded balance from irys_database::get_account_balance({:?})",
                TxIngressError::Unfunded,
                tx.signer
            );
            return Err(TxIngressError::Unfunded);
        }

        // Validate the transaction signature
        // check the result and error handle
        let _ = self.validate_signature(&tx).await;

        let mut mempool_state_write_guard = mempool_state.write().await;
        mempool_state_write_guard.valid_tx.insert(tx.id, tx.clone());
        mempool_state_write_guard.recent_valid_tx.insert(tx.id);
        drop(mempool_state_write_guard);

        // Cache the data_root in the database
        match self.irys_db.update_eyre(|db_tx| {
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
        let mut mempool_state_write_guard = mempool_state.write().await;
        let option_chunks_map = mempool_state_write_guard.pending_chunks.pop(&tx.data_root);
        drop(mempool_state_write_guard);
        if let Some(chunks_map) = option_chunks_map {
            // Extract owned chunks from the map to process them
            let chunks: Vec<_> = chunks_map.into_iter().map(|(_, chunk)| chunk).collect();
            for chunk in chunks {
                let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
                //todo check the value rather than _
                let _ = self
                    .handle_message(MempoolServiceMessage::ChunkIngressMessage(
                        chunk, oneshot_tx,
                    ))
                    .await;

                let msg_result = oneshot_rx
                    .await
                    .expect("pending chunks should be processed by the mempool");

                if let Err(err) = msg_result {
                    tracing::error!("oneshot failure: {:?}", err);
                    return Err(TxIngressError::Other("oneshot failure".to_owned()));
                }
            }
        }

        // Gossip transaction
        let gossip_data = GossipData::Transaction(tx.clone());
        if let Err(error) = self.service_senders.gossip_broadcast.send(gossip_data) {
            tracing::error!("Failed to send gossip data: {:?}", error);
        }

        Ok(())
    }

    async fn handle_tx_existence_query(&self, txid: H256) -> Result<bool, TxIngressError> {
        let mempool_state = &self.mempool_state;
        let mempool_state_guard = mempool_state.read().await;

        #[allow(clippy::if_same_then_else, reason = "readability")]
        if mempool_state_guard.valid_tx.contains_key(&txid) {
            Ok(true)
        } else if mempool_state_guard.recent_valid_tx.contains(&txid) {
            Ok(true)
        } else if mempool_state_guard.invalid_tx.contains(&txid) {
            // Still has it, just invalid
            Ok(true)
        } else {
            drop(mempool_state_guard);
            let read_tx = self.read_tx().await;

            if read_tx.is_err() {
                Err(TxIngressError::DatabaseError)
            } else {
                Ok(
                    tx_header_by_txid(&read_tx.expect("expected valid header from tx id"), &txid)
                        .map_err(|_| TxIngressError::DatabaseError)?
                        .is_some(),
                )
            }
        }
    }

    async fn handle_get_commitment_txs(
        &self,
        commitment_tx_ids: Vec<H256>,
    ) -> HashMap<IrysTransactionId, CommitmentTransaction> {
        let mut hash_map = HashMap::new();

        // first flat_map all the commitment transactions
        let mempool_state = &self.mempool_state;
        let mempool_state_guard = mempool_state.read().await;

        // Get any CommitmentTransactions from the valid commitments Map
        mempool_state_guard
            .valid_commitment_tx
            .values()
            .flat_map(|txs| txs.iter())
            .for_each(|tx| {
                hash_map.insert(tx.id, tx.clone());
            });

        // Get any CommitmentTransactions from the pending commitments LRU cache
        mempool_state_guard
            .pending_pledges
            .iter()
            .flat_map(|(_, inner)| inner.iter())
            .for_each(|(tx_id, tx)| {
                hash_map.insert(*tx_id, tx.clone());
            });

        debug!(
            "handle_get_commitment_tsx: {:?}",
            hash_map.iter().map(|x| x.0).collect::<Vec<_>>()
        );

        // Attempt to locate and retain only the requested tx_ids
        let mut filtered_map = HashMap::with_capacity(commitment_tx_ids.len());
        for txid in commitment_tx_ids {
            if let Some(tx) = hash_map.get(&txid) {
                filtered_map.insert(txid, tx.clone());
            }
        }

        // Return only the transactions matching the requested IDs
        filtered_map
    }

    #[tracing::instrument(skip_all, err)]
    /// handle inbound MempoolServiceMessage and send oneshot responses where required to do so
    fn handle_message<'a>(
        &'a mut self,
        msg: MempoolServiceMessage,
    ) -> BoxFuture<'a, eyre::Result<()>> {
        Box::pin(async move {
            match msg {
                MempoolServiceMessage::GetTransaction(tx, response) => {
                    let response_message = self.handle_transaction_message(tx).await;
                    if let Err(e) = response.send(response_message) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::BlockConfirmedMessage(block, all_txs) => {
                    let _unused_response_message =
                        self.handle_block_confirmed_message(block, all_txs).await;
                }
                MempoolServiceMessage::CommitmentTxIngressMessage(commitment_tx, response) => {
                    let response_message = self
                        .handle_commitment_tx_ingress_message(commitment_tx)
                        .await;
                    if let Err(e) = response.send(response_message) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::ChunkIngressMessage(chunk, response) => {
                    let response_value = self.handle_chunk_ingress_message(chunk).await;
                    if let Err(e) = response.send(response_value) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::GetBestMempoolTxs(block_id, response) => {
                    let response_value = self.handle_get_best_mempool_txs(block_id).await;
                    // Return selected transactions grouped by type
                    if let Err(e) = response.send(response_value) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::GetCommitmentTxs {
                    commitment_tx_ids,
                    response,
                } => {
                    let response_value = self.handle_get_commitment_txs(commitment_tx_ids).await;
                    if let Err(e) = response.send(response_value) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::TxExistenceQuery(txid, response) => {
                    let response_value = self.handle_tx_existence_query(txid).await;
                    if let Err(e) = response.send(response_value) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
                MempoolServiceMessage::TxIngressMessage(tx, response) => {
                    let response_value = self.handle_tx_ingress_message(tx).await;
                    if let Err(e) = response.send(response_value) {
                        tracing::error!("response.send() error: {:?}", e);
                    };
                }
            }
            Ok(())
        })
    }

    /// Opens a read-only database transaction from the Irys mempool state.
    ///
    /// Returns a `Tx<RO>` handle if successful, or a `ChunkIngressError::DatabaseError`
    /// if the transaction could not be created. Logs an error if the transaction fails.
    async fn read_tx(
        &self,
    ) -> Result<irys_database::reth_db::mdbx::tx::Tx<reth_db::mdbx::RO>, DatabaseError> {
        self.irys_db
            .tx()
            .inspect_err(|e| error!("database error reading tx: {:?}", e))
    }

    async fn persist_mempool_to_disk(&self) -> eyre::Result<()> {
        let base_path = self.config.node_config.mempool_dir();

        let commitment_tx_path = base_path.join("commitment_tx");
        fs::create_dir_all(commitment_tx_path.clone())
            .expect("to create the mempool/commitment_tx dir");
        let commitment_hash_map = self.get_all_commitment_tx().await;
        for tx in commitment_hash_map.values() {
            // Create a filepath for this transaction
            let tx_path = commitment_tx_path.join(format!("{}.json", tx.id.0.to_base58()));

            // Check to see if the file exists
            if tx_path.exists() {
                continue;
            }

            // If not, write it to  {mempool_dir}/commitment_tx/{txid}.json
            let json = serde_json::to_string(tx).unwrap();
            debug!("{}", json);
            debug!("{}", tx_path.to_str().unwrap());

            let mut file = get_atomic_file(tx_path).unwrap();
            file.write_all(json.as_bytes())?;
            file.commit()?;
        }

        // TODO: Do the same for all the pending storage tx
        let _storage_tx_path = base_path.join("storage_tx");

        Ok(())
    }

    /// should really only be called by persist_mempool_to_disk, all other scenarios need a more
    /// subtle filtering of commitment state, recently confirmed? pending? valid? etc.
    async fn get_all_commitment_tx(&self) -> HashMap<IrysTransactionId, CommitmentTransaction> {
        let mut hash_map = HashMap::new();

        // first flat_map all the commitment transactions
        let mempool_state = &self.mempool_state;
        let mempool_state_guard = mempool_state.read().await;

        // Get any CommitmentTransactions from the valid commitments
        mempool_state_guard
            .valid_commitment_tx
            .values()
            .flat_map(|txs| txs.iter())
            .for_each(|tx| {
                hash_map.insert(tx.id, tx.clone());
            });

        // Get any CommitmentTransactions from the pending commitments
        mempool_state_guard
            .pending_pledges
            .iter()
            .flat_map(|(_, inner)| inner.iter())
            .for_each(|(tx_id, tx)| {
                hash_map.insert(*tx_id, tx.clone());
            });

        hash_map
    }

    async fn restore_mempool_from_disk(&mut self) {
        let recovered =
            RecoveredMempoolState::load_from_disk(&self.config.node_config.mempool_dir()).await;

        for (_txid, commitment_tx) in recovered.commitment_txs {
            self.handle_commitment_tx_ingress_message(commitment_tx)
                .await
                .unwrap(); // We don't care about the outcome, just giving the mempool a crack at validating it
        }

        // TODO: Similar logic for storage_tx
    }

    /// Removes a commitment transaction with the specified transaction ID from the valid_commitment_tx map
    /// Returns true if the transaction was found and removed, false otherwise
    #[allow(dead_code)]
    async fn remove_commitment_tx(&mut self, txid: &H256) -> bool {
        let mut found = false;

        let mempool_state = &self.mempool_state;
        let mut mempool_state_guard = mempool_state.write().await;

        mempool_state_guard.recent_valid_tx.remove(txid);

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

        drop(mempool_state_guard);

        found
    }

    async fn get_commitment_status(
        &self,
        commitment_tx: &CommitmentTransaction,
    ) -> CommitmentCacheStatus {
        let mempool_state = &self.mempool_state;
        // Check if already staked in the blockchain
        let is_staked = self.commitment_state_guard.is_staked(commitment_tx.signer);

        // Most commitments are valid by default
        // Only pledges require special validation when not already staked
        let is_pledge = commitment_tx.commitment_type == CommitmentType::Pledge;
        if !is_pledge || is_staked {
            return CommitmentCacheStatus::Accepted;
        }

        // For unstaked pledges, validate against cache and pending transactions
        let commitment_cache = self.service_senders.commitment_cache.clone();
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
        if matches!(cache_status, CommitmentCacheStatus::Unsupported) {
            warn!(
                "Commitment is unsupported: {}",
                commitment_tx.id.0.to_base58()
            );
            return CommitmentCacheStatus::Unsupported;
        }

        // For unstaked addresses, check for pending stake transactions
        if matches!(cache_status, CommitmentCacheStatus::Unstaked) {
            let mempool_state_guard = mempool_state.read().await;
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
                    return CommitmentCacheStatus::Accepted;
                }
            }

            // No pending stakes found
            warn!(
                "Pledge Commitment is unstaked: {}",
                commitment_tx.id.0.to_base58()
            );
            return CommitmentCacheStatus::Unstaked;
        }

        // All other cases are valid
        CommitmentCacheStatus::Accepted
    }

    // Helper to validate anchor
    async fn validate_anchor(
        &mut self,
        tx_id: &IrysTransactionId,
        anchor: &H256,
    ) -> Result<IrysBlockHeader, TxIngressError> {
        let mempool_state = &self.mempool_state;

        let read_tx = self
            .read_tx()
            .await
            .map_err(|_| TxIngressError::DatabaseError)?;

        let latest_height = self.get_latest_block_height().await?;
        let anchor_expiry_depth = self
            .config
            .node_config
            .consensus_config()
            .mempool
            .anchor_expiry_depth as u64;

        let mempool_state_read_guard = mempool_state.read().await;
        // Allow transactions to use the txid of a transaction in the mempool
        if mempool_state_read_guard.recent_valid_tx.contains(anchor) {
            let (canonical_blocks, _) = self.block_tree_read_guard.read().get_canonical_chain();
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
                let mut mempool_state_write_guard = mempool_state.write().await;
                mempool_state_write_guard.invalid_tx.push(*tx_id);
                warn!("Invalid anchor value {} for tx {}", anchor, tx_id);
                Err(TxIngressError::InvalidAnchor)
            }
        }
    }

    // Helper to get the canonical chain and latest height
    async fn get_latest_block_height(&self) -> Result<u64, TxIngressError> {
        let canon_chain = self.block_tree_read_guard.read().get_canonical_chain();
        let (_, latest_height, _, _) = canon_chain.0.last().ok_or(TxIngressError::Other(
            "unable to get canonical chain from block tree".to_owned(),
        ))?;

        Ok(*latest_height)
    }

    // Helper to verify signature
    async fn validate_signature<T: IrysTransactionCommon>(
        &mut self,
        tx: &T,
    ) -> Result<(), TxIngressError> {
        if tx.is_signature_valid() {
            info!("Signature is valid");
            Ok(())
        } else {
            let mempool_state = &self.mempool_state;
            mempool_state.write().await.invalid_tx.push(tx.id());
            debug!("Signature is NOT valid");
            Err(TxIngressError::InvalidSignature)
        }
    }
}

/// Create a new instance of the mempool state passing in a reference
/// counted reference to a `DatabaseEnv`, a copy of reth's task executor and the miner's signer
pub fn create_state(config: &MempoolConfig) -> MempoolState {
    let max_pending_chunk_items = config.max_pending_chunk_items;
    let max_pending_pledge_items = config.max_pending_pledge_items;
    MempoolState {
        valid_tx: BTreeMap::new(),
        valid_commitment_tx: BTreeMap::new(),
        invalid_tx: Vec::new(),
        recent_valid_tx: HashSet::new(),
        pending_chunks: LruCache::new(NonZeroUsize::new(max_pending_chunk_items).unwrap()),
        pending_pledges: LruCache::new(NonZeroUsize::new(max_pending_pledge_items).unwrap()),
    }
}
