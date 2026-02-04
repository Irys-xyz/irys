use crate::mempool_service::MempoolServiceMessage;
use crate::metrics::record_reth_fcu_head_height;
use eyre::eyre;
use irys_database::{database, db::IrysDatabaseExt as _};
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_types::chunk_provider::{PdChunkMessage, PdChunkReceiver, RethChunkProvider};
use irys_types::range_specifier::ChunkRangeSpecifier;
use irys_types::{
    BlockHash, DatabaseProvider, H256, RethPeerInfo, SendTraced as _, TokioServiceHandle, Traced,
};
use reth::{
    network::{NetworkInfo as _, Peers as _},
    revm::primitives::{bytes::Bytes, B256},
    rpc::{eth::EthApiServer as _, types::BlockNumberOrTag},
    tasks::shutdown::Shutdown,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot,
};
use tracing::{Instrument as _, debug, error, info, trace, warn};

// ============================================================================
// PD Chunk Manager - Global Cache with Reference Counting
// ============================================================================

/// Key for identifying a chunk globally
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkKey {
    pub ledger: u32,
    pub offset: u64,
}

/// Status of chunk provisioning for a PD transaction
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxState {
    /// Waiting to start provisioning
    Pending,
    /// Provisioning in progress
    Provisioning { fetched: usize, total: usize },
    /// All chunks ready for execution
    Ready,
    /// Chunks locked during block execution
    Locked,
}

/// Cached chunk with reference counting
struct CachedChunk {
    data: Arc<Bytes>,
    /// Transactions currently referencing this chunk
    referencing_txs: HashSet<B256>,
    #[allow(dead_code)]
    cached_at: Instant,
}

/// Transaction state - references chunks in the global cache, doesn't own them
#[derive(Debug)]
pub struct PdTxState {
    /// Transaction hash
    pub tx_hash: B256,
    /// Required chunks (just references, not data)
    pub required_chunks: HashSet<ChunkKey>,
    /// Current state
    pub state: TxState,
    /// When provisioning started
    pub started_at: Instant,
}

/// Unified manager for PD chunk provisioning across all pending transactions.
///
/// Uses a global chunk cache with reference counting:
/// - Chunks are stored once, shared by all transactions that need them
/// - Reference counting tracks which transactions need each chunk
/// - Chunks are evicted only when no transactions reference them
pub struct PdChunkManager {
    /// Global chunk cache keyed by (ledger, offset)
    chunks: HashMap<ChunkKey, CachedChunk>,
    /// Per-transaction state (references chunks, doesn't own them)
    txs: HashMap<B256, PdTxState>,
    /// Storage backend for fetching chunks
    storage_provider: Arc<dyn RethChunkProvider>,
}

impl PdChunkManager {
    /// Create a new chunk manager with the given storage provider.
    pub fn new(storage_provider: Arc<dyn RethChunkProvider>) -> Self {
        Self {
            chunks: HashMap::new(),
            txs: HashMap::new(),
            storage_provider,
        }
    }

    /// Run the message processing loop.
    pub async fn run(&mut self, mut rx: PdChunkReceiver) {
        info!("PdChunkManager started");
        while let Some(msg) = rx.recv().await {
            match msg {
                PdChunkMessage::NewTransaction {
                    tx_hash,
                    chunk_specs,
                } => {
                    self.start_provisioning(tx_hash, chunk_specs).await;
                }
                PdChunkMessage::TransactionRemoved { tx_hash } => {
                    self.remove(&tx_hash);
                }
                PdChunkMessage::IsReady { tx_hash, response } => {
                    let ready = self.is_ready(&tx_hash);
                    let _ = response.send(ready);
                }
                PdChunkMessage::Lock { tx_hash, response } => {
                    let locked = self.lock(&tx_hash);
                    let _ = response.send(locked);
                }
                PdChunkMessage::Unlock { tx_hash } => {
                    self.unlock(&tx_hash);
                }
                PdChunkMessage::GetChunk {
                    ledger,
                    offset,
                    response,
                } => {
                    let chunk = self.get_chunk(ledger, offset);
                    let _ = response.send(chunk);
                }
            }
        }
        info!("PdChunkManager stopped");
    }

    /// Convert chunk specs to chunk keys
    fn specs_to_keys(&self, chunk_specs: &[ChunkRangeSpecifier]) -> HashSet<ChunkKey> {
        let config = self.storage_provider.config();
        let mut keys = HashSet::new();

        for spec in chunk_specs {
            let partition_index: u64 = spec.partition_index.try_into().unwrap_or(0);
            let base_offset = config
                .num_chunks_in_partition
                .saturating_mul(partition_index);

            for i in 0..spec.chunk_count {
                let ledger_offset = base_offset
                    .saturating_add(spec.offset as u64)
                    .saturating_add(i as u64);

                keys.insert(ChunkKey {
                    ledger: 0,
                    offset: ledger_offset,
                });
            }
        }
        keys
    }

    /// Start provisioning chunks for a PD transaction.
    async fn start_provisioning(&mut self, tx_hash: B256, chunk_specs: Vec<ChunkRangeSpecifier>) {
        let required_chunks = self.specs_to_keys(&chunk_specs);
        let total_chunks = required_chunks.len();

        debug!(
            tx_hash = %tx_hash,
            total_chunks = total_chunks,
            "Starting PD chunk provisioning"
        );

        // Register transaction in Pending state
        let state = PdTxState {
            tx_hash,
            required_chunks: required_chunks.clone(),
            state: TxState::Provisioning {
                fetched: 0,
                total: total_chunks,
            },
            started_at: Instant::now(),
        };
        self.txs.insert(tx_hash, state);

        // Fetch missing chunks and add references
        let mut fetched = 0;
        for key in &required_chunks {
            if let Some(cached) = self.chunks.get_mut(key) {
                // Already cached - just add reference
                cached.referencing_txs.insert(tx_hash);
                fetched += 1;
                trace!(
                    tx_hash = %tx_hash,
                    ledger = key.ledger,
                    offset = key.offset,
                    "Chunk already cached, added reference"
                );
            } else {
                // Need to fetch from storage
                match self
                    .storage_provider
                    .get_unpacked_chunk_by_ledger_offset(key.ledger, key.offset)
                {
                    Ok(Some(chunk)) => {
                        let mut referencing_txs = HashSet::new();
                        referencing_txs.insert(tx_hash);

                        self.chunks.insert(
                            *key,
                            CachedChunk {
                                data: Arc::new(chunk),
                                referencing_txs,
                                cached_at: Instant::now(),
                            },
                        );
                        fetched += 1;
                        trace!(
                            tx_hash = %tx_hash,
                            ledger = key.ledger,
                            offset = key.offset,
                            fetched = fetched,
                            total = total_chunks,
                            "Fetched and cached chunk"
                        );
                    }
                    Ok(None) => {
                        warn!(
                            tx_hash = %tx_hash,
                            ledger = key.ledger,
                            offset = key.offset,
                            "Chunk not found in storage"
                        );
                    }
                    Err(e) => {
                        warn!(
                            tx_hash = %tx_hash,
                            ledger = key.ledger,
                            offset = key.offset,
                            error = %e,
                            "Failed to fetch chunk from storage"
                        );
                    }
                }
            }
        }

        // Transition to Ready
        if let Some(tx_state) = self.txs.get_mut(&tx_hash) {
            tx_state.state = TxState::Ready;
        }

        debug!(
            tx_hash = %tx_hash,
            fetched = fetched,
            total = total_chunks,
            cached_chunks = self.chunks.len(),
            "PD chunks ready"
        );
    }

    /// Check if chunks for a transaction are ready.
    fn is_ready(&self, tx_hash: &B256) -> bool {
        match self.txs.get(tx_hash) {
            Some(state) => {
                matches!(state.state, TxState::Ready | TxState::Locked)
            }
            None => {
                // Transaction not registered - stub behavior: assume ready
                // This allows non-PD transactions and transactions not yet monitored to pass
                trace!(tx_hash = %tx_hash, "PD transaction not registered, assuming ready (stub)");
                true
            }
        }
    }

    /// Lock chunks for execution. Only succeeds if state is Ready.
    fn lock(&mut self, tx_hash: &B256) -> bool {
        if let Some(state) = self.txs.get_mut(tx_hash) {
            if state.state == TxState::Ready {
                state.state = TxState::Locked;
                trace!(tx_hash = %tx_hash, "PD chunks locked for execution");
                return true;
            }
            // Not ready - can't lock
            return false;
        }
        // Transaction not registered - stub behavior: allow lock
        trace!(tx_hash = %tx_hash, "PD transaction not registered, allowing lock (stub)");
        true
    }

    /// Unlock chunks after execution completes.
    fn unlock(&mut self, tx_hash: &B256) {
        if let Some(state) = self.txs.get_mut(tx_hash) {
            if state.state == TxState::Locked {
                state.state = TxState::Ready;
                trace!(tx_hash = %tx_hash, "PD chunks unlocked");
            }
        }
    }

    /// Remove transaction state and decrement chunk references.
    /// Chunks with no remaining references are evicted from the cache.
    fn remove(&mut self, tx_hash: &B256) {
        if let Some(tx_state) = self.txs.remove(tx_hash) {
            // Decrement references for all chunks this transaction used
            let mut evicted = 0;
            for key in &tx_state.required_chunks {
                let should_remove = if let Some(cached) = self.chunks.get_mut(key) {
                    cached.referencing_txs.remove(tx_hash);
                    cached.referencing_txs.is_empty()
                } else {
                    false
                };

                if should_remove {
                    self.chunks.remove(key);
                    evicted += 1;
                }
            }

            trace!(
                tx_hash = %tx_hash,
                evicted_chunks = evicted,
                remaining_cached = self.chunks.len(),
                "PD transaction removed, references decremented"
            );
        }
    }

    /// Get a chunk from the global cache.
    /// No tx_hash needed - chunks are immutable data identified by (ledger, offset).
    fn get_chunk(&self, ledger: u32, offset: u64) -> Option<Arc<Bytes>> {
        let key = ChunkKey { ledger, offset };
        self.chunks.get(&key).map(|c| Arc::clone(&c.data))
    }
}

#[derive(Debug)]
pub struct RethService {
    shutdown: Shutdown,
    cmd_rx: UnboundedReceiver<Traced<RethServiceMessage>>,
    handle: IrysRethNodeAdapter,
    db: DatabaseProvider,
    mempool: UnboundedSender<Traced<MempoolServiceMessage>>,
}

#[derive(Debug, Clone, Copy)]
pub struct ForkChoiceUpdateMessage {
    pub head_hash: BlockHash,
    pub confirmed_hash: BlockHash,
    pub finalized_hash: BlockHash,
}

#[derive(Debug)]
pub enum RethServiceMessage {
    ForkChoice {
        update: ForkChoiceUpdateMessage,
        response: oneshot::Sender<()>,
    },
    ConnectToPeer {
        peer: RethPeerInfo,
        response: oneshot::Sender<eyre::Result<()>>,
    },
    GetPeeringInfo {
        response: oneshot::Sender<eyre::Result<RethPeerInfo>>,
    },
}

// Represents the fork-choice hashes we feed to Reth. Each field is an ancestor of `head`:
// - `head_hash`: current canonical tip (latest block we want Reth to follow).
// - `confirmed_hash`: migration/safe block, roughly `migration_depth` behind head.
// - `finalized_hash`: prune/finalized block, `block_tree_depth` behind the confirmed block.
#[derive(Debug, Clone, Copy, Default)]
pub struct ForkChoiceUpdate {
    pub head_hash: B256,
    pub confirmed_hash: B256,
    pub finalized_hash: B256,
}

#[tracing::instrument(level = "trace", skip_all, err)]
async fn evm_block_hash_from_block_hash(
    mempool_service: &UnboundedSender<Traced<MempoolServiceMessage>>,
    db: &DatabaseProvider,
    irys_hash: H256,
) -> eyre::Result<B256> {
    debug!(block.hash = %irys_hash, "Resolving EVM block hash for Irys block");

    let irys_header = {
        let (tx, rx) = oneshot::channel();
        mempool_service
            .send_traced(MempoolServiceMessage::GetBlockHeader(irys_hash, true, tx))
            .expect("expected send to mempool to succeed");
        let mempool_response = rx.await?;
        match mempool_response {
            Some(h) => {
                debug!(block.hash = %irys_hash, "Found block in mempool");
                h
            }
            None => {
                debug!(block.hash = %irys_hash, "Block not in mempool, checking database");
                db
                    .view_eyre(|tx| database::block_header_by_hash(tx, &irys_hash, false))?
                    .ok_or_else(|| {
                        error!(block.hash = %irys_hash, "Irys block not found in mempool or database");
                        eyre!("Missing irys block {} in DB!", irys_hash)
                    })?
            }
        }
    };
    debug!(
        block.hash = %irys_hash,
        block.evm_block_hash = %irys_header.evm_block_hash,
        block.height = irys_header.height,
        "Resolved Irys block to EVM block"
    );
    Ok(irys_header.evm_block_hash)
}

impl RethService {
    /// Spawn the Reth service.
    pub fn spawn_service(
        handle: IrysRethNodeAdapter,
        database_provider: DatabaseProvider,
        mempool: UnboundedSender<Traced<MempoolServiceMessage>>,
        cmd_rx: UnboundedReceiver<Traced<RethServiceMessage>>,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        let (shutdown_signal, shutdown) = reth::tasks::shutdown::signal();

        let service = Self {
            shutdown,
            cmd_rx,
            handle,
            db: database_provider,
            mempool,
        };

        let join_handle = runtime_handle.spawn(
            async move {
                if let Err(err) = service.run().await {
                    error!(
                        custom.error = %err,
                        "Reth service terminated with error"
                    );
                }
            }
            .in_current_span(),
        );

        TokioServiceHandle {
            name: "reth_service".to_string(),
            handle: join_handle,
            shutdown_signal,
        }
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    async fn run(mut self) -> eyre::Result<()> {
        info!("Starting Reth service");

        loop {
            tokio::select! {
                biased;

                _ = &mut self.shutdown => {
                    info!("Shutdown signal received for Reth service");
                    break;
                }

                command = self.cmd_rx.recv() => {
                    match command {
                        Some(traced) => {
                            let (command, parent_span) = traced.into_parts();
                            let span = tracing::trace_span!(parent: &parent_span, "reth_handle_command");
                            self.handle_command(command).instrument(span).await?;
                        }
                        None => {
                            info!("Reth service command channel closed");
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_command(&mut self, command: RethServiceMessage) -> eyre::Result<()> {
        match command {
            RethServiceMessage::ForkChoice { update, response } => {
                self.handle_forkchoice(update).await?;
                let _ = response.send(());
            }
            RethServiceMessage::ConnectToPeer { peer, response } => {
                let result = self.connect_to_peer(peer);
                let _ = response.send(result);
            }
            RethServiceMessage::GetPeeringInfo { response } => {
                let result = self.get_peering_info();
                let _ = response.send(result);
            }
        }
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    async fn handle_forkchoice(&mut self, update: ForkChoiceUpdateMessage) -> eyre::Result<()> {
        debug!(?update, "Received fork choice update command");

        let resolved = self.resolve_new_fcu(update).await?;
        self.process_fcu(resolved).await?;
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, err, ret)]
    async fn resolve_new_fcu(
        &self,
        new_fcu: ForkChoiceUpdateMessage,
    ) -> eyre::Result<ForkChoiceUpdate> {
        debug!("Resolving new fork choice update");

        let ForkChoiceUpdateMessage {
            head_hash,
            confirmed_hash,
            finalized_hash,
        } = new_fcu;

        let evm_head_hash =
            evm_block_hash_from_block_hash(&self.mempool, &self.db, head_hash).await?;

        let evm_confirmed_hash =
            evm_block_hash_from_block_hash(&self.mempool, &self.db, confirmed_hash).await?;

        let evm_finalized_hash =
            evm_block_hash_from_block_hash(&self.mempool, &self.db, finalized_hash).await?;

        Ok(ForkChoiceUpdate {
            head_hash: evm_head_hash,
            confirmed_hash: evm_confirmed_hash,
            finalized_hash: evm_finalized_hash,
        })
    }

    async fn process_fcu(&self, fcu: ForkChoiceUpdate) -> eyre::Result<ForkChoiceUpdate> {
        let ForkChoiceUpdate {
            head_hash,
            confirmed_hash,
            finalized_hash,
        } = fcu;

        tracing::debug!(
            fcu.head = %head_hash,
            fcu.confirmed = %confirmed_hash,
            fcu.finalized = %finalized_hash,
            "Updating Reth fork choice"
        );
        let handle = self.handle.clone();
        let eth_api = handle.inner.eth_api();

        let get_blocks = async || {
            let latest_before = eth_api.block_by_number(BlockNumberOrTag::Latest, false);
            let safe_before = eth_api.block_by_number(BlockNumberOrTag::Safe, false);
            let finalized_before = eth_api.block_by_number(BlockNumberOrTag::Finalized, false);
            futures::try_join!(latest_before, safe_before, finalized_before)
        };
        let (latest_before, safe_before, finalized_before) = get_blocks().await?;

        tracing::debug!(
            eth_api.latest_block = ?latest_before.as_ref().map(|b| (b.header.number, b.header.hash)),
            eth_api.safe_block = ?safe_before.as_ref().map(|b| (b.header.number, b.header.hash)),
            eth_api.finalized_block = ?finalized_before.as_ref().map(|b| (b.header.number, b.header.hash)),
            "Reth state before fork choice update"
        );

        handle
            .update_forkchoice_full(head_hash, Some(confirmed_hash), Some(finalized_hash))
            .await
            .map_err(|e| {
                error!(
                    custom.error = %e,
                    fcu.message = ?fcu,
                    "Failed to update Reth fork choice"
                );
                eyre!("Error updating reth with forkchoice {:?} - {}", &fcu, &e)
            })?;

        debug!("Fork choice update sent to Reth, fetching current state");

        let (latest_after, safe_after, finalized_after) = get_blocks().await?;
        tracing::debug!(
            eth_api.latest_block = ?latest_after.as_ref().map(|b| (b.header.number, b.header.hash)),
            eth_api.safe_block = ?safe_after.as_ref().map(|b| (b.header.number, b.header.hash)),
            eth_api.finalized_block = ?finalized_after.as_ref().map(|b| (b.header.number, b.header.hash)),
            "Reth state after fork choice update"
        );

        let latest_after = latest_after.unwrap();
        eyre::ensure!(
            head_hash == latest_after.header.hash,
            "head hashes don't match post FCU"
        );
        eyre::ensure!(
            confirmed_hash == safe_after.unwrap().header.hash,
            "safe/confirmed hashes don't match post FCU"
        );
        eyre::ensure!(
            finalized_hash == finalized_after.unwrap().header.hash,
            "finalized hashes don't match post FCU"
        );

        record_reth_fcu_head_height(latest_after.header.number);

        Ok(fcu)
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    fn connect_to_peer(&self, peer: RethPeerInfo) -> eyre::Result<()> {
        info!(
            reth_peer.id = %peer.peer_id,
            reth_peer.address = %peer.peering_tcp_addr,
            "Connecting to peer"
        );
        self.handle
            .inner
            .network
            .add_peer(peer.peer_id, peer.peering_tcp_addr);
        debug!(reth_peer.id = %peer.peer_id, "Peer connection initiated");
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    fn get_peering_info(&self) -> eyre::Result<RethPeerInfo> {
        let handle = self.handle.clone();
        let peer_id = *handle.inner.network.peer_id();
        let local_addr = handle.inner.network.local_addr();

        debug!(
            reth_peer.id = %peer_id,
            reth_peer.local_address = %local_addr,
            "Returning peering info"
        );

        Ok(RethPeerInfo {
            peer_id,
            peering_tcp_addr: local_addr,
        })
    }
}
