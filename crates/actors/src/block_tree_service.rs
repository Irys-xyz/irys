use crate::{
    block_index_service::{BlockIndexReadGuard, BlockIndexService},
    chunk_migration_service::ChunkMigrationService,
    mempool_service::MempoolServiceMessage,
    reth_service::{BlockHashType, ForkChoiceUpdateMessage, RethServiceActor},
    services::ServiceSenders,
    validation_service::ValidationServiceMessage,
    BlockFinalizedMessage, CommitmentStateReadGuard,
};
use actix::prelude::*;
use base58::ToBase58 as _;
use eyre::{eyre, Context as _};
use futures::future::Either;
use irys_database::{
    block_header_by_hash, commitment_tx_by_txid, db::IrysDatabaseExt as _, CommitmentSnapshot,
    SystemLedger,
};
use irys_types::{
    Address, BlockHash, CommitmentTransaction, Config, ConsensusConfig, DataLedger,
    DatabaseProvider, H256List, IrysBlockHeader, IrysTransactionHeader, H256, U256,
};
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
use reth_db::Database as _;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    pin::pin,
    sync::{Arc, RwLock, RwLockReadGuard},
    time::SystemTime,
};
use tokio::{
    sync::{mpsc::UnboundedReceiver, oneshot},
    task::JoinHandle,
};
use tracing::{debug, error, info};

pub mod ema_snapshot;
use ema_snapshot::{create_ema_snapshot_from_chain_history, EmaSnapshot};

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

/// Wraps the internal `Arc<RwLock<_>>` to make the reference readonly
#[derive(Debug, Clone, MessageResponse)]
pub struct BlockTreeReadGuard {
    block_tree_cache: Arc<RwLock<BlockTreeCache>>,
}

impl BlockTreeReadGuard {
    /// Creates a new `ReadGuard` for the `block_tree` cache
    pub const fn new(block_tree_cache: Arc<RwLock<BlockTreeCache>>) -> Self {
        Self { block_tree_cache }
    }

    /// Accessor method to get a read guard for the `block_tree` cache
    pub fn read(&self) -> RwLockReadGuard<'_, BlockTreeCache> {
        self.block_tree_cache.read().unwrap()
    }

    #[cfg(any(test, feature = "test-utils"))]
    /// Accessor method to get a write guard for the `block_tree` cache
    pub fn write(&self) -> std::sync::RwLockWriteGuard<'_, BlockTreeCache> {
        self.block_tree_cache.write().unwrap()
    }
}

// Messages that the CommitmentCache service supports
#[derive(Debug)]
pub enum BlockTreeServiceMessage {
    GetBlockTreeReadGuard {
        response: oneshot::Sender<BlockTreeReadGuard>,
    },
    BlockPreValidated {
        block: Arc<IrysBlockHeader>,
        commitment_txs: Arc<Vec<CommitmentTransaction>>,
        response: oneshot::Sender<eyre::Result<()>>,
    },
    BlockValidationFinished {
        block_hash: H256,
        validation_result: ValidationResult,
    },
}

/// `BlockDiscoveryActor` listens for discovered blocks & validates them.
#[derive(Debug)]
pub struct BlockTreeService {
    shutdown: GracefulShutdown,
    msg_rx: UnboundedReceiver<BlockTreeServiceMessage>,
    inner: BlockTreeServiceInner,
}

#[derive(Debug)]
pub struct BlockTreeServiceInner {
    db: DatabaseProvider,
    /// Block tree internal state
    pub cache: Arc<RwLock<BlockTreeCache>>,
    /// The wallet address of the local miner
    pub miner_address: Address,
    /// Read view of the `block_index`
    pub block_index_guard: BlockIndexReadGuard,
    /// Read only view of the current epoch's commitments
    pub commitment_state_guard: CommitmentStateReadGuard,
    /// Global storage config
    pub consensus_config: ConsensusConfig,
    /// Channels for communicating with the services
    pub service_senders: ServiceSenders,
    /// Reth service actor sender
    pub reth_service_actor: Addr<RethServiceActor>,
    /// Current actix system
    pub system: System,
}

#[derive(Debug, Clone)]
pub struct ReorgEvent {
    pub old_fork: Arc<Vec<Arc<IrysBlockHeader>>>,
    pub new_fork: Arc<Vec<Arc<IrysBlockHeader>>>,
    pub fork_parent: Arc<IrysBlockHeader>,
    pub new_tip: BlockHash,
    pub timestamp: SystemTime,
}

#[derive(Debug, Clone)]
pub struct BlockMigratedEvent {
    pub block: Arc<IrysBlockHeader>,
}

#[derive(Debug, Clone)]
pub struct BlockStateUpdated {
    pub block_hash: BlockHash,
    pub height: u64,
    pub state: ChainState,
    pub discarded: bool,
}

impl BlockTreeService {
    /// Spawn a new BlockTree service
    pub fn spawn_service(
        exec: &TaskExecutor,
        rx: UnboundedReceiver<BlockTreeServiceMessage>,
        db: DatabaseProvider,
        block_index_guard: BlockIndexReadGuard,
        commitment_state_guard: CommitmentStateReadGuard,
        config: &Config,
        service_senders: &ServiceSenders,
        reth_service_actor: Addr<RethServiceActor>,
    ) -> JoinHandle<()> {
        // Dereference miner_address here, before the closure
        let miner_address = config.node_config.miner_address();
        let consensus_config = config.node_config.consensus_config();
        let service_senders = service_senders.clone();
        let system = System::current();
        let bi_guard = block_index_guard;
        let cs_guard = commitment_state_guard;

        exec.spawn_critical_with_graceful_shutdown_signal(
            "BlockTree Service",
            |shutdown| async move {
                let cache = BlockTreeCache::restore_from_db(
                    bi_guard.clone(),
                    cs_guard.clone(),
                    reth_service_actor.clone(),
                    db.clone(),
                    consensus_config.clone(),
                );

                let block_tree_service = Self {
                    shutdown,
                    msg_rx: rx,
                    inner: BlockTreeServiceInner {
                        db,
                        cache: Arc::new(RwLock::new(cache)),
                        miner_address,
                        block_index_guard: bi_guard,
                        commitment_state_guard: cs_guard,
                        consensus_config,
                        service_senders,
                        reth_service_actor,
                        system,
                    },
                };
                block_tree_service
                    .start()
                    .await
                    .expect("BlockTree encountered an irrecoverable error")
            },
        )
    }

    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting BlockTree service");

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

        tracing::debug!(amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.inner.handle_message(msg).await?;
        }

        // explicitly inform the TaskManager that we're shutting down
        drop(shutdown_guard);

        tracing::info!("shutting down BlockTree service");
        Ok(())
    }
}

impl BlockTreeServiceInner {
    /// Dispatches received messages to appropriate handler methods and sends responses
    #[tracing::instrument(skip_all, err)]
    async fn handle_message(&mut self, msg: BlockTreeServiceMessage) -> eyre::Result<()> {
        match msg {
            BlockTreeServiceMessage::GetBlockTreeReadGuard { response } => {
                let guard = BlockTreeReadGuard::new(self.cache.clone());
                let _ = response.send(guard);
            }
            BlockTreeServiceMessage::BlockPreValidated {
                block,
                commitment_txs,
                response,
            } => {
                let result = self.on_block_prevalidated(block, commitment_txs);
                let _ = response.send(result);
            }
            BlockTreeServiceMessage::BlockValidationFinished {
                block_hash,
                validation_result,
            } => {
                self.on_block_validation_finished(block_hash, validation_result)
                    .await?;
            }
        }
        Ok(())
    }

    async fn send_storage_finalized_message(&self, block_hash: BlockHash) -> eyre::Result<()> {
        // retrieve block header from the mempool or database
        let block_header = {
            let (tx_block, rx_block) = oneshot::channel();
            self.service_senders
                .mempool
                .send(MempoolServiceMessage::GetBlockHeader(
                    block_hash, false, tx_block,
                ))?;
            match rx_block.await? {
                Some(h) => h,
                None => self
                    .db
                    .view_eyre(|tx| block_header_by_hash(tx, &block_hash, false))?
                    .ok_or_else(|| eyre!("No block header found for hash {}", block_hash,))?,
            }
        };

        let submit_txs = self
            .get_data_ledger_tx_headers_from_mempool(&block_header, DataLedger::Submit)
            .await?;
        let publish_txs = self
            .get_data_ledger_tx_headers_from_mempool(&block_header, DataLedger::Publish)
            .await?;

        let mut all_txs = vec![];
        all_txs.extend(publish_txs);
        all_txs.extend(submit_txs);

        info!(
            "Migrating to block_index - hash: {} height: {}",
            &block_header.block_hash.0.to_base58(),
            &block_header.height
        );

        // HACK
        System::set_current(self.system.clone());

        let chunk_migration = ChunkMigrationService::from_registry();
        let block_index = BlockIndexService::from_registry();
        let block_finalized_message = BlockFinalizedMessage {
            block_header: Arc::new(block_header),
            all_txs: Arc::new(all_txs),
        };

        block_index.do_send(block_finalized_message.clone());
        chunk_migration.do_send(block_finalized_message);
        Ok(())
    }

    async fn notify_services_of_block_confirmation(
        &self,
        tip_hash: BlockHash,
        confirmed_block: &Arc<IrysBlockHeader>,
    ) -> eyre::Result<()> {
        debug!(
            "JESSEDEBUG confirming irys block evm_block_hash: {} ({})",
            &confirmed_block.evm_block_hash, &confirmed_block.height
        );
        let _ = self
            .reth_service_actor
            .send(ForkChoiceUpdateMessage {
                head_hash: BlockHashType::Irys(tip_hash),
                confirmed_hash: Some(BlockHashType::Evm(confirmed_block.evm_block_hash)),
                finalized_hash: None,
            })
            .await??;
        self.service_senders
            .mempool
            .send(MempoolServiceMessage::BlockConfirmed(
                confirmed_block.clone(),
            ))
            .expect("mempool service has unexpectedly become unreachable");
        Ok(())
    }

    /// Checks if a block that is `block_migration_depth` blocks behind `arc_block`
    /// should be finalized. If eligible, sends finalization message unless block
    /// is already in `block_index`. Panics if the `block_tree` and `block_index` are
    /// inconsistent.
    async fn try_notify_services_of_block_finalization(
        &self,
        arc_block: &Arc<IrysBlockHeader>,
    ) -> eyre::Result<()> {
        let finalized_hash;
        let cache_tip;
        {
            let binding = self.cache.clone();
            let cache = binding.write().unwrap();
            let migration_depth = self.consensus_config.block_migration_depth as usize;

            // Skip if block isn't deep enough for finalization
            if arc_block.height <= migration_depth as u64 {
                return Ok(());
            }

            let (longest_chain, _) = cache.get_canonical_chain();
            if longest_chain.len() <= migration_depth {
                return Ok(());
            }

            // Find block to finalize
            let Some(current_index) = longest_chain
                .iter()
                .position(|x| x.block_hash == arc_block.block_hash)
            else {
                info!(
                    "Validated block not in longest chain, block {} height: {}, skipping finalization",
                    arc_block.block_hash, arc_block.height
                );
                return Ok(());
            };

            if current_index < migration_depth {
                return Ok(()); // Block already finalized
            }

            let finalize_index = current_index - migration_depth;
            finalized_hash = longest_chain[finalize_index].block_hash;
            let finalized_height = longest_chain[finalize_index].height;

            // Verify block isn't already finalized
            let binding = self.block_index_guard.clone();
            let bi = binding.read();
            if bi.num_blocks() > finalized_height && bi.num_blocks() > finalized_height {
                let finalized = bi.get_item(finalized_height).unwrap();
                if finalized.block_hash == finalized_hash {
                    return Ok(());
                }
                panic!("Block tree and index out of sync");
            }

            match cache.get_block(&finalized_hash) {
                Some(block) => {
                    let mut block = block.clone();
                    block.poa.chunk = None;
                    let migrated_block = Arc::new(block);
                    // Broadcast BlockMigratedEvent event using the shared sender
                    let block_migrated_event = BlockMigratedEvent {
                        block: migrated_block,
                    };
                    if let Err(e) = self
                        .service_senders
                        .block_migrated_events
                        .send(block_migrated_event)
                    {
                        debug!("No reorg subscribers: {:?}", e);
                    }
                }
                None => error!("migrated block {} not found in block_tree", finalized_hash),
            }

            debug!(?finalized_hash, ?finalized_height, "migrating irys block");
            cache_tip = cache.tip
        }
        // TODO: this is the wrong place for this, it should be at the prune depth not the block_migration_depth
        let _res = self
            .reth_service_actor
            .send(ForkChoiceUpdateMessage {
                head_hash: BlockHashType::Irys(cache_tip),
                confirmed_hash: None,
                finalized_hash: Some(BlockHashType::Irys(finalized_hash)),
            })
            .await??;

        self.send_storage_finalized_message(finalized_hash).await?;
        Ok(())
    }

    /// Handles pre-validated blocks received from the validation service.
    fn on_block_prevalidated(
        &mut self,
        block: Arc<IrysBlockHeader>,
        commitment_txs: Arc<Vec<CommitmentTransaction>>,
    ) -> eyre::Result<()> {
        let block_hash = &block.block_hash;

        let mut cache = self.cache.write().expect("cache lock poisoned");

        // Early return if block already exists
        if let Some(existing) = cache.get_block(block_hash) {
            debug!(
                "on_block_prevalidated: {} at height: {} already in block_tree",
                existing.block_hash, existing.height
            );
            return Ok(());
        }

        let parent_block = cache
            .blocks
            .get(&block.previous_block_hash)
            .expect("previous block to be in block tree");

        // Get previous block's commitment snapshot
        let prev_commitment_snapshot = parent_block.commitment_snapshot.clone();

        // Create commitment snapshot for this block
        let commitment_snapshot = create_commitment_snapshot_for_block(
            &block,
            &commitment_txs,
            &prev_commitment_snapshot,
            &self.consensus_config,
            &self.commitment_state_guard,
        );

        // Create ema snapshot for this block
        let ema_snapshot = parent_block.ema_snapshot.next_snapshot(
            &block,
            &parent_block.block,
            &self.consensus_config,
        )?;

        // Add block to the tree (all blocks start as NotOnchain and go through validation)
        let add_result = cache.add_block(&block, commitment_snapshot, ema_snapshot);

        if let Ok(new_state) = add_result {
            // Schedule validation and mark as scheduled
            self.service_senders
                .validation_service
                .send(ValidationServiceMessage::ValidateBlock {
                    block: block.clone(),
                })
                .context("validation service unreachable!")?;

            if cache
                .mark_block_as_validation_scheduled(block_hash)
                .is_err()
            {
                error!("Unable to mark block as ValidationScheduled");
            }

            let _ = self
                .service_senders
                .block_state_events
                .send(BlockStateUpdated {
                    block_hash: block.block_hash,
                    height: block.height,
                    state: new_state,
                    discarded: false,
                });

            debug!(
                "scheduling block for validation: {} height: {}",
                block_hash, block.height
            );
        }

        Ok(())
    }

    /// Handles the completion of full block validation.
    ///
    /// When a block passes validation:
    /// 1. Updates the block's state in the cache to `ValidBlock`
    /// 2. Moves the tip of the chain to this block if it is now the head of the longest chain
    /// 3. If the tip moves, checks whether it's a simple extension or a reorganization:
    ///    - **Extension**: The new tip's parent is the current tip
    ///    - **Reorg**: The new tip's parent is not the current tip (fork switch)
    /// 4. For reorgs, broadcasts a `ReorgEvent` containing:
    ///    - Blocks from the old fork (now orphaned)
    ///    - Blocks from the new fork (now canonical)
    ///    - The common ancestor where the fork occurred
    /// 5. Notifies services of the new confirmed block
    /// 6. Checks if any blocks are deep enough to be finalized to storage
    ///
    /// Invalid blocks are logged but currently remain in the cache.
    async fn on_block_validation_finished(
        &mut self,
        block_hash: H256,
        validation_result: ValidationResult,
    ) -> eyre::Result<()> {
        debug!(
            "\u{001b}[32mOn validation complete : result {} {:?}\u{001b}[0m",
            block_hash, validation_result
        );
        let arc_block;
        let height;
        let state;
        let discarded;
        match validation_result {
            ValidationResult::Invalid => {
                error!(block_hash = %block_hash.0.to_base58(),"invalid block");
                let mut cache = self.cache.write().unwrap();

                let Some(block_entry) = cache.get_block(&block_hash) else {
                    // block not in the tree
                    return Ok(());
                };
                // Get block state info before removal for the event
                height = block_entry.height;
                state = cache
                    .get_block_and_status(&block_hash)
                    .map(|(_, state)| *state)
                    .unwrap_or(ChainState::NotOnchain(BlockState::Unknown));

                // Remove the block
                let _ = cache
                    .remove_block(&block_hash)
                    .inspect_err(|err| tracing::error!(?err));

                discarded = true;
            }
            ValidationResult::Valid => {
                let mut block_confirmed_event = false;
                {
                    let binding = self.cache.clone();
                    let mut cache = binding.write().unwrap();

                    // Get the current tip before any changes
                    // Note: We cant rely on canonical chain here, because the canonical chain was already updated when this
                    //       block arrived and was added after pre-validation. The tip only moves after full validation.
                    let old_tip = cache.tip;
                    let old_tip_block = cache.get_block(&old_tip).unwrap().clone();

                    // Get block info before marking as valid
                    let block_entry = cache.get_block(&block_hash).unwrap();
                    height = block_entry.height;
                    discarded = false;
                    // Get the updated state after marking as valid
                    state = cache
                        .get_block_and_status(&block_hash)
                        .map(|(_, state)| *state)
                        .unwrap_or(ChainState::NotOnchain(BlockState::Unknown));

                    // Mark block as validated in cache, this will update the canonical chain
                    if let Err(err) = cache.mark_block_as_valid(&block_hash) {
                        error!("{}", err);
                        return Ok(());
                    }

                    // let (longest_chain, not_on_chain_count) = cache.get_canonical_chain();
                    let Some((_block_entry, fork_blocks, _)) =
                        cache.get_earliest_not_onchain_in_longest_chain()
                    else {
                        if block_hash == old_tip {
                            debug!(
                                "\u{001b}[32mSame Tip Marked current tip {} cdiff: {} height: {}\u{001b}[0m",
                                block_hash, old_tip_block.cumulative_diff, old_tip_block.height
                            );
                        } else {
                            debug!(
                                "\u{001b}[32mNo new tip found {}, current tip {} cdiff: {} height: {}\u{001b}[0m",
                                block_hash,
                                old_tip_block.block_hash,
                                old_tip_block.cumulative_diff,
                                old_tip_block.height
                            );
                        }
                        return Ok(());
                    };

                    // if the old tip isn't in the fork_blocks, it's a reorg
                    let is_reorg = !fork_blocks.iter().any(|bh| bh.block_hash == old_tip);

                    // Get block info before mutable operations
                    let block_entry = cache.blocks.get(&block_hash).unwrap();
                    arc_block = Arc::new(block_entry.block.clone());

                    // Now do mutable operations
                    if cache.mark_tip(&block_hash).is_ok() {
                        if is_reorg {
                            let mut orphaned_blocks = cache.get_fork_blocks(&old_tip_block);
                            orphaned_blocks.push(&old_tip_block);

                            let fork_hash = orphaned_blocks.first().unwrap().block_hash;
                            let fork_block = cache.get_block(&fork_hash).unwrap();
                            let fork_height = fork_block.height;

                            // Populate `old_canonical` by converting each orphaned block into a `ChainCacheEntry`.
                            let mut old_canonical = Vec::with_capacity(orphaned_blocks.len());
                            for block in &orphaned_blocks {
                                let entry = make_block_tree_entry(block);
                                old_canonical.push(entry);
                            }
                            let new_canonical = cache.get_canonical_chain();

                            let (old_fork, new_fork) = prune_chains_at_ancestor(
                                old_canonical,
                                new_canonical.0,
                                fork_hash,
                                fork_height,
                            );

                            // Build Arc'd IrysBlockHeader lists for the ReorgEvent to minimize overhead of cloning
                            let old_fork_blocks: Vec<Arc<IrysBlockHeader>> = old_fork
                                .iter()
                                .map(|e| {
                                    let mut block = cache.get_block(&e.block_hash).unwrap().clone();
                                    block.poa.chunk = None; // Strip out the chunk
                                    Arc::new(block)
                                })
                                .collect();

                            let new_fork_blocks: Vec<Arc<IrysBlockHeader>> = new_fork
                                .iter()
                                .map(|e| {
                                    let mut block = cache.get_block(&e.block_hash).unwrap().clone();
                                    block.poa.chunk = None; // Strip out the chunk
                                    Arc::new(block)
                                })
                                .collect();

                            debug!(
                                "\u{001b}[32mReorg at block height {} with {}\u{001b}[0m",
                                arc_block.height, arc_block.block_hash
                            );
                            let event = ReorgEvent {
                                old_fork: Arc::new(old_fork_blocks),
                                new_fork: Arc::new(new_fork_blocks),
                                fork_parent: Arc::new(fork_block.clone()),
                                new_tip: block_hash,
                                timestamp: SystemTime::now(),
                            };

                            // Broadcast reorg event using the shared sender
                            if let Err(e) = self.service_senders.reorg_events.send(event) {
                                debug!("No reorg subscribers: {:?}", e);
                            }
                        } else {
                            debug!(
                                "\u{001b}[32mExtending longest chain to height {} with {} parent: {} height: {}\u{001b}[0m",
                                arc_block.height,
                                arc_block.block_hash,
                                old_tip_block.block_hash,
                                old_tip_block.height
                            );
                        }
                        block_confirmed_event = true;
                    }
                }

                if block_confirmed_event {
                    self.notify_services_of_block_confirmation(block_hash, &arc_block)
                        .await?;
                }

                // Handle block finalization (move chunks to disk and add to block_index)
                self.try_notify_services_of_block_finalization(&arc_block)
                    .await?;
            }
        }

        let event = BlockStateUpdated {
            block_hash,
            height,
            state,
            discarded,
        };
        let _ = self.service_senders.block_state_events.send(event);
        Ok(())
    }

    /// Fetches full transaction headers from mempool using the txids from a ledger in a block
    async fn get_data_ledger_tx_headers_from_mempool(
        &self,
        block_header: &IrysBlockHeader,
        ledger: DataLedger,
    ) -> eyre::Result<Vec<IrysTransactionHeader>> {
        // Explicitly cast enum to index
        let ledger_index = ledger as usize;

        let data_tx_ids = block_header
            .data_ledgers
            .get(ledger_index)
            .ok_or_else(|| eyre::eyre!("Ledger index {} out of bounds", ledger_index))?
            .tx_ids
            .0
            .clone();
        let mempool = self.service_senders.mempool.clone();

        let (tx, rx) = oneshot::channel();
        mempool
            .send(MempoolServiceMessage::GetDataTxs(data_tx_ids.clone(), tx))
            .map_err(|_| eyre::eyre!("Failed to send request to mempool"))?;

        let received = rx
            .await
            .map_err(|e| eyre::eyre!("Mempool response error: {}", e))?
            .into_iter()
            .flatten()
            .collect::<Vec<IrysTransactionHeader>>();

        if received.len() != data_tx_ids.len() {
            return Err(eyre::eyre!(
                "Mismatch in {:?} tx count: expected {}, got {}",
                ledger,
                data_tx_ids.len(),
                received.len()
            ));
        }

        Ok(received)
    }
}

fn make_block_tree_entry(block: &IrysBlockHeader) -> BlockTreeEntry {
    // DataLedgers
    let mut data_ledgers = BTreeMap::new();

    // TODO: potentially loop through DataLedger::ALL and add them to the entry
    //       to better support more data ledgers in the future.

    let publish_ledger = block
        .data_ledgers
        .iter()
        .find(|tx_ledger| tx_ledger.ledger_id == DataLedger::Publish as u32);

    if let Some(publish_ledger) = publish_ledger {
        data_ledgers.insert(DataLedger::Publish, publish_ledger.tx_ids.clone());
    }

    let submit_ledger = block
        .data_ledgers
        .iter()
        .find(|tx_ledger| tx_ledger.ledger_id == DataLedger::Submit as u32);

    if let Some(submit_ledger) = submit_ledger {
        data_ledgers.insert(DataLedger::Submit, submit_ledger.tx_ids.clone());
    }

    // System Ledgers
    let mut system_ledgers = BTreeMap::new();
    let commitment_ledger = block
        .system_ledgers
        .iter()
        .find(|tx_ledger| tx_ledger.ledger_id == SystemLedger::Commitment as u32);

    if let Some(commitment_ledger) = commitment_ledger {
        system_ledgers.insert(SystemLedger::Commitment, commitment_ledger.tx_ids.clone());
    }

    BlockTreeEntry {
        block_hash: block.block_hash,
        height: block.height,
        data_ledgers,
        system_ledgers,
    }
}

/// Prunes two canonical chains at the specified common ancestor, returning only the divergent portions
/// Returns (old_chain_from_fork, new_chain_from_fork)
pub fn prune_chains_at_ancestor(
    old_chain: Vec<BlockTreeEntry>,
    new_chain: Vec<BlockTreeEntry>,
    ancestor_hash: BlockHash,
    ancestor_height: u64,
) -> (Vec<BlockTreeEntry>, Vec<BlockTreeEntry>) {
    // Find the ancestor index in the old chain
    let old_ancestor_idx = old_chain
        .iter()
        .position(|e| e.block_hash == ancestor_hash && e.height == ancestor_height)
        .expect("Common ancestor should exist in old chain");

    // Find the ancestor index in the new chain
    let new_ancestor_idx = new_chain
        .iter()
        .position(|e| e.block_hash == ancestor_hash && e.height == ancestor_height)
        .expect("Common ancestor should exist in new chain");

    // Return the portions after the common ancestor (excluding the ancestor itself)
    let old_divergent = old_chain[old_ancestor_idx + 1..].to_vec();
    let new_divergent = new_chain[new_ancestor_idx + 1..].to_vec();

    (old_divergent, new_divergent)
}

#[derive(Debug, Clone, Copy)]
pub enum ValidationResult {
    Valid,
    Invalid,
}

#[derive(Debug, Clone)]
pub struct BlockTreeEntry {
    pub block_hash: BlockHash,
    pub height: u64,
    pub data_ledgers: BTreeMap<DataLedger, H256List>,
    pub system_ledgers: BTreeMap<SystemLedger, H256List>,
}

#[derive(Debug)]
pub struct BlockTreeCache {
    // Main block storage
    blocks: HashMap<BlockHash, BlockEntry>,

    // Track solutions -> block hashes
    solutions: HashMap<H256, HashSet<BlockHash>>,

    // Current tip
    pub tip: BlockHash,

    // Track max cumulative difficulty
    max_cumulative_difficulty: (U256, BlockHash), // (difficulty, block_hash)

    // Height -> Hash mapping
    height_index: BTreeMap<u64, HashSet<BlockHash>>,

    // Cache of longest chain: (block/tx pairs, count of non-onchain blocks)
    longest_chain_cache: (Vec<BlockTreeEntry>, usize),

    // Consensus configuration containing cache depth
    consensus_config: ConsensusConfig,
}

#[derive(Debug)]
pub struct BlockEntry {
    block: IrysBlockHeader,
    chain_state: ChainState,
    timestamp: SystemTime,
    children: HashSet<H256>,
    commitment_snapshot: Arc<CommitmentSnapshot>,
    ema_snapshot: Arc<EmaSnapshot>,
}

/// Represents the `ChainState` of a block, is it Onchain? or a valid fork?
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ChainState {
    /// Block is confirmed (by another block) and part of the main chain
    Onchain,
    /// Block is Validated but may not be on the main chain
    /// Locally produced blocks can have `BlockState::ValidationScheduled`
    /// while maintaining `ChainState::Validated`
    Validated(BlockState),
    /// Block exists but is not conformed by any other block
    NotOnchain(BlockState),
}

/// Represents the validation state of a block, independent of its `ChainState`
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum BlockState {
    /// Initial state, validation not yet started
    Unknown,
    /// Validation has been requested but not completed
    ValidationScheduled,
    /// Block has passed all validation checks
    ValidBlock,
}

/// The result of marking a new tip for the canonical chain
#[derive(Debug, Clone)]
pub enum TipChangeResult {
    NoChange,
    Extension,
    Reorg {
        orphaned_blocks: Vec<BlockHash>,
        fork_height: u64,
    },
}

impl BlockTreeCache {
    /// Create a new cache initialized with a starting block. The block is marked as
    /// on-chain and set as the tip. Only used in testing that doesn't intersect
    /// the commitment snapshot so it stubs one out
    #[cfg(any(feature = "test-utils", test))]
    pub fn new(genesis_block: &IrysBlockHeader, consensus_config: ConsensusConfig) -> Self {
        let block_hash = genesis_block.block_hash;
        let solution_hash = genesis_block.solution_hash;
        let height = genesis_block.height;
        let cumulative_diff = genesis_block.cumulative_diff;

        let mut blocks = HashMap::new();
        let mut solutions = HashMap::new();
        let mut height_index = BTreeMap::new();

        // Create a dummy commitment snapshot
        let commitment_snapshot = Arc::new(CommitmentSnapshot::default());

        // Create EMA cache for genesis block
        let ema_snapshot = EmaSnapshot::genesis(genesis_block);

        // Create initial block entry for genesis block, marking it as confirmed
        // and part of the canonical chain
        let block_entry = BlockEntry {
            block: genesis_block.clone(),
            chain_state: ChainState::Onchain,
            timestamp: SystemTime::now(),
            children: HashSet::new(),
            commitment_snapshot,
            ema_snapshot,
        };

        // Initialize all indices
        blocks.insert(block_hash, block_entry);
        solutions.insert(solution_hash, HashSet::from([block_hash]));
        height_index.insert(height, HashSet::from([block_hash]));

        // Initialize longest chain cache to contain the genesis block
        let entry = make_block_tree_entry(genesis_block);
        let longest_chain_cache = (vec![(entry)], 0);

        Self {
            blocks,
            solutions,
            tip: block_hash,
            max_cumulative_difficulty: (cumulative_diff, block_hash),
            height_index,
            longest_chain_cache,
            consensus_config,
        }
    }

    /// Restores the block tree cache from the database and `block_index` during startup.
    ///
    /// Rebuilds the block tree by iterating the `block_index` and loading the most recent blocks from
    /// the database (up to `block_cache_depth` blocks). For each block, it loads associated commitment
    /// transactions and reconstructs the commitment snapshot for that block. The function also notifies
    /// the Reth service of the current chain tip.
    ///
    /// ## Arguments
    /// * `block_index_guard` - Read guard for accessing the block index
    /// * `commitment_state_guard` - Read guard for checking staking status of a signer during commitment processing
    /// * `reth_service_actor` - Actor handle for sending fork choice updates to Reth
    /// * `db` - Database provider for querying block and transaction data
    /// * `consensus_config` - Consensus configuration including epoch settings
    ///
    /// ## Returns
    /// Fully initialized block tree cache ready for use
    ///
    /// ## Panics
    /// Panics if the block index is empty or if database queries fail unexpectedly
    pub fn restore_from_db(
        block_index_guard: BlockIndexReadGuard,
        commitment_state_guard: CommitmentStateReadGuard,
        reth_service_actor: Addr<RethServiceActor>,
        db: DatabaseProvider,
        consensus_config: ConsensusConfig,
    ) -> Self {
        // Extract block range and start block info
        let (start, end, start_block_hash) = {
            let block_index = block_index_guard.read();
            assert!(block_index.num_blocks() > 0, "Block list must not be empty");

            let start = block_index
                .num_blocks()
                .saturating_sub(consensus_config.block_cache_depth - 1);
            let end = block_index.num_blocks();
            let start_block_hash = block_index.get_item(start).unwrap().block_hash;
            (start, end, start_block_hash)
        };

        let tx = db.tx().unwrap();
        let start_block = block_header_by_hash(&tx, &start_block_hash, false)
            .unwrap()
            .unwrap();

        debug!(
            "block tree start block - hash: {} height: {}",
            start_block_hash, start_block.height
        );

        // Initialize cache with start block
        let entry = make_block_tree_entry(&start_block);
        let mut block_tree_cache = Self {
            blocks: HashMap::new(),
            solutions: HashMap::new(),
            tip: start_block_hash,
            max_cumulative_difficulty: (start_block.cumulative_diff, start_block_hash),
            height_index: BTreeMap::new(),
            longest_chain_cache: (vec![entry], 0),
            consensus_config: consensus_config.clone(),
        };

        // Initialize commitment snapshot and add start block
        let mut commitment_snapshot = build_current_commitment_snapshot_from_index(
            block_index_guard.clone(),
            commitment_state_guard.clone(),
            db.clone(),
            &consensus_config,
        );

        let arc_commitment_snapshot = Arc::new(commitment_snapshot.clone());

        // Get the latest block from index for EMA snapshot
        let latest_block_hash = {
            let block_index = block_index_guard.read();
            block_index.get_item(end - 1).unwrap().block_hash
        };
        let latest_block = block_header_by_hash(&tx, &latest_block_hash, false)
            .unwrap()
            .unwrap();

        // Create EMA cache for start block
        let ema_snapshot =
            build_current_ema_snapshot_from_index(&latest_block, db.clone(), &consensus_config);

        let block_entry = BlockEntry {
            block: start_block.clone(),
            chain_state: ChainState::Onchain,
            timestamp: SystemTime::now(),
            children: HashSet::new(),
            commitment_snapshot: arc_commitment_snapshot.clone(),
            ema_snapshot: ema_snapshot.clone(),
        };

        block_tree_cache
            .blocks
            .insert(start_block_hash, block_entry);
        block_tree_cache
            .solutions
            .insert(start_block.solution_hash, HashSet::from([start_block_hash]));
        block_tree_cache
            .height_index
            .insert(start_block.height, HashSet::from([start_block_hash]));

        let mut prev_commitment_snapshot = arc_commitment_snapshot;
        let mut prev_ema_snapshot = ema_snapshot;
        let mut prev_block = start_block;

        // Process remaining blocks
        for block_height in (start + 1)..end {
            let block_hash = {
                let block_index = block_index_guard.read();
                block_index.get_item(block_height).unwrap().block_hash
            };

            let block = block_header_by_hash(&tx, &block_hash, false)
                .unwrap()
                .unwrap();

            // Load commitment transactions (from DB during startup)
            let commitment_txs =
                load_commitment_transactions(&block, &db).expect("to load transactions from db");

            // Create commitment snapshot for this block
            let arc_commitment_snapshot = create_commitment_snapshot_for_block(
                &block,
                &commitment_txs,
                &prev_commitment_snapshot,
                &consensus_config,
                &commitment_state_guard,
            );

            let arc_ema_snapshot = prev_ema_snapshot
                .next_snapshot(&block, &prev_block, &consensus_config)
                .expect("failed to create EMA snapshot");

            // Update commitment snapshot with new commitments if it's not an epoch block
            if block.height % consensus_config.epoch.num_blocks_in_epoch != 0 {
                for commitment_tx in &commitment_txs {
                    let is_staked_in_current_epoch =
                        commitment_state_guard.is_staked(commitment_tx.signer);
                    commitment_snapshot.add_commitment(commitment_tx, is_staked_in_current_epoch);
                }
            }

            prev_commitment_snapshot = arc_commitment_snapshot.clone();
            prev_ema_snapshot = arc_ema_snapshot.clone();
            prev_block = block.clone();

            // Use add_common directly for restored blocks since they're already validated
            block_tree_cache
                .add_common(
                    &block,
                    arc_commitment_snapshot,
                    arc_ema_snapshot,
                    ChainState::Validated(BlockState::ValidBlock),
                )
                .unwrap();
        }

        // Set tip and notify reth service
        let tip_hash = {
            let block_index = block_index_guard.read();
            block_index.get_latest_item().unwrap().block_hash
        };

        block_tree_cache.mark_tip(&tip_hash).unwrap();
        reth_service_actor
            .try_send(ForkChoiceUpdateMessage {
                head_hash: BlockHashType::Irys(tip_hash),
                confirmed_hash: None,
                finalized_hash: None,
            })
            .expect("could not send message to `RethServiceActor`");

        block_tree_cache
    }

    pub fn add_common(
        &mut self,
        block: &IrysBlockHeader,
        commitment_snapshot: Arc<CommitmentSnapshot>,
        ema_snapshot: Arc<EmaSnapshot>,
        chain_state: ChainState,
    ) -> eyre::Result<()> {
        let hash = block.block_hash;
        let prev_hash = block.previous_block_hash;

        // Get parent
        let prev_entry = self
            .blocks
            .get_mut(&prev_hash)
            .ok_or_else(|| eyre::eyre!("Previous block not found"))?;

        // Update indices
        prev_entry.children.insert(hash);
        self.solutions
            .entry(block.solution_hash)
            .or_default()
            .insert(hash);
        self.height_index
            .entry(block.height)
            .or_default()
            .insert(hash);

        debug!(
            "adding block: max_cumulative_difficulty: {} block.cumulative_diff: {} {}",
            self.max_cumulative_difficulty.0, block.cumulative_diff, block.block_hash
        );
        if block.cumulative_diff > self.max_cumulative_difficulty.0 {
            debug!(
                "setting max_cumulative_difficulty ({}, {}) for height: {}",
                block.cumulative_diff, hash, block.height
            );
            self.max_cumulative_difficulty = (block.cumulative_diff, hash);
        }

        self.blocks.insert(
            hash,
            BlockEntry {
                block: block.clone(),
                chain_state,
                timestamp: SystemTime::now(),
                children: HashSet::new(),
                commitment_snapshot,
                ema_snapshot,
            },
        );

        self.update_longest_chain_cache();
        Ok(())
    }

    /// Adds a block to the block tree.
    ///
    /// All blocks undergo strict validation before acceptance:
    /// 1. **Full validation sequence** - Full validation rules must be run and pass
    /// 2. **Confirmation required** - Block must be confirmed by another block building on it
    /// 3. **Block Index Migration** - Only then is the block added to the block_index
    pub fn add_block(
        &mut self,
        block: &IrysBlockHeader,
        commitment_snapshot: Arc<CommitmentSnapshot>,
        ema_snapshot: Arc<EmaSnapshot>,
    ) -> eyre::Result<ChainState> {
        let hash = block.block_hash;

        debug!(
            "add_block() - {} height: {}",
            block.block_hash, block.height
        );

        // Check if block is already part of the canonical chain
        if matches!(
            self.blocks.get(&hash).map(|b| &b.chain_state),
            Some(ChainState::Onchain)
        ) {
            debug!(?hash, "already part of the main chain state");
            return Ok(ChainState::Onchain);
        }

        let state = ChainState::NotOnchain(BlockState::Unknown);
        self.add_common(block, commitment_snapshot, ema_snapshot, state)?;
        Ok(state)
    }

    /// Helper function to delete a single block without recursion
    fn delete_block(&mut self, block_hash: &BlockHash) -> eyre::Result<()> {
        let block_entry = self
            .blocks
            .get(block_hash)
            .ok_or_else(|| eyre::eyre!("Block not found"))?;

        let solution_hash = block_entry.block.solution_hash;
        let height = block_entry.block.height;
        let prev_hash = block_entry.block.previous_block_hash;

        // Update parent's children set
        if let Some(prev_entry) = self.blocks.get_mut(&prev_hash) {
            prev_entry.children.remove(block_hash);
        }

        // Update height index
        if let Some(height_set) = self.height_index.get_mut(&height) {
            height_set.remove(block_hash);
            if height_set.is_empty() {
                self.height_index.remove(&height);
            }
        }

        // Update solutions map
        if let Some(solutions) = self.solutions.get_mut(&solution_hash) {
            solutions.remove(block_hash);
            if solutions.is_empty() {
                self.solutions.remove(&solution_hash);
            }
        }

        // Remove the block
        self.blocks.remove(block_hash);

        // Update max_cumulative_difficulty if necessary
        if self.max_cumulative_difficulty.1 == *block_hash {
            self.max_cumulative_difficulty = self.find_max_difficulty();
        }

        self.update_longest_chain_cache();
        Ok(())
    }

    #[cfg(feature = "test-utils")]
    pub fn test_delete(&mut self, block_hash: &BlockHash) -> eyre::Result<()> {
        self.delete_block(block_hash)
    }

    /// Removes a block and all its descendants recursively
    pub fn remove_block(&mut self, block_hash: &BlockHash) -> eyre::Result<()> {
        // Get children before deleting the block
        let children = self
            .blocks
            .get(block_hash)
            .map(|entry| entry.children.iter().copied().collect::<Vec<_>>())
            .ok_or_else(|| eyre::eyre!("Block not found"))?;

        // Recursively remove all children first
        for child in children {
            self.remove_block(&child)?;
        }

        // Delete this block
        self.delete_block(block_hash)
    }

    // Helper to find new max difficulty when current max is removed
    fn find_max_difficulty(&self) -> (U256, BlockHash) {
        self.blocks
            .iter()
            .map(|(hash, entry)| (entry.block.cumulative_diff, *hash))
            .max_by_key(|(diff, _)| *diff)
            .unwrap_or((U256::zero(), BlockHash::default()))
    }

    /// Returns: cache of longest chain: (block/tx pairs, count of non-onchain blocks)
    /// 0th element -- genesis block
    /// last element -- the latest block
    #[must_use]
    pub fn get_canonical_chain(&self) -> (Vec<BlockTreeEntry>, usize) {
        self.longest_chain_cache.clone()
    }

    fn update_longest_chain_cache(&mut self) {
        let pairs = {
            self.longest_chain_cache.0.clear();
            &mut self.longest_chain_cache.0
        };
        let mut not_onchain_count = 0;

        let mut current = self.max_cumulative_difficulty.1;
        let mut blocks_to_collect = self.consensus_config.block_cache_depth;
        debug!(
            "updating canonical chain cache latest_cache_tip: {}",
            current
        );

        while let Some(entry) = self.blocks.get(&current) {
            match &entry.chain_state {
                // For blocks awaiting initial validation, restart chain from parent
                ChainState::NotOnchain(BlockState::Unknown | BlockState::ValidationScheduled) => {
                    // Reset everything and continue from parent block
                    pairs.clear();
                    not_onchain_count = 0;
                    current = entry.block.previous_block_hash;
                    blocks_to_collect = self.consensus_config.block_cache_depth;
                    continue;
                }

                ChainState::Onchain => {
                    // Include OnChain blocks in pairs
                    let chain_cache_entry = make_block_tree_entry(&entry.block);
                    pairs.push(chain_cache_entry);

                    if blocks_to_collect == 0 {
                        break;
                    }
                    blocks_to_collect -= 1;
                }

                // For Validated or other NotOnchain states
                ChainState::Validated(_) | ChainState::NotOnchain(_) => {
                    let chain_cache_entry = make_block_tree_entry(&entry.block);
                    pairs.push(chain_cache_entry);
                    not_onchain_count += 1;

                    if blocks_to_collect == 0 {
                        break;
                    }
                    blocks_to_collect -= 1;
                }
            }

            if entry.block.height == 0 {
                break;
            } else {
                current = entry.block.previous_block_hash;
            }
        }

        pairs.reverse();
        self.longest_chain_cache.1 = not_onchain_count;
    }

    /// Helper to mark off-chain blocks in a set
    fn mark_off_chain(&mut self, children: HashSet<H256>, current: &BlockHash) {
        for child in children {
            if child == *current {
                continue;
            }
            if let Some(entry) = self.blocks.get_mut(&child) {
                if matches!(entry.chain_state, ChainState::Onchain) {
                    entry.chain_state = ChainState::Validated(BlockState::ValidBlock);
                    // Recursively mark children of this block
                    let children = entry.children.clone();
                    self.mark_off_chain(children, current);
                }
            }
        }
    }

    /// Helper to recursively mark blocks on-chain
    fn mark_on_chain(&mut self, block: &IrysBlockHeader) -> eyre::Result<()> {
        let prev_hash = block.previous_block_hash;

        match self.blocks.get(&prev_hash) {
            None => Ok(()), // Reached the end
            Some(prev_entry) => {
                let prev_block = prev_entry.block.clone();
                let prev_children = prev_entry.children.clone();

                match prev_entry.chain_state {
                    ChainState::Onchain => {
                        // Mark other branches as not onchain (but preserve their validation state)
                        self.mark_off_chain(prev_children, &block.block_hash);
                        Ok(())
                    }
                    ChainState::NotOnchain(BlockState::ValidBlock) | ChainState::Validated(_) => {
                        // Update previous block to on_chain
                        if let Some(entry) = self.blocks.get_mut(&prev_hash) {
                            entry.chain_state = ChainState::Onchain;
                        }
                        // Recursively mark previous blocks
                        self.mark_on_chain(&prev_block)
                    }
                    ChainState::NotOnchain(_) => Err(eyre::eyre!("invalid_tip")),
                }
            }
        }
    }

    /// Marks a block as the new tip
    pub fn mark_tip(&mut self, block_hash: &BlockHash) -> eyre::Result<bool> {
        debug!("mark_tip({})", block_hash);
        // Get the current block
        let block_entry = self
            .blocks
            .get(block_hash)
            .ok_or_else(|| eyre::eyre!("Block not found in cache"))?;

        let block = block_entry.block.clone();
        let old_tip = self.tip;

        // Recursively mark previous blocks
        self.mark_on_chain(&block)?;

        // Mark the tip block as on_chain
        if let Some(entry) = self.blocks.get_mut(block_hash) {
            entry.chain_state = ChainState::Onchain;
        }

        self.tip = *block_hash;
        self.update_longest_chain_cache();

        debug!(
            "\u{001b}[32mmark tip: hash:{} height: {}\u{001b}[0m",
            block_hash, block.height
        );

        Ok(old_tip != *block_hash)
    }

    pub fn mark_block_as_validation_scheduled(
        &mut self,
        block_hash: &BlockHash,
    ) -> eyre::Result<()> {
        if let Some(entry) = self.blocks.get_mut(block_hash) {
            if entry.chain_state == ChainState::NotOnchain(BlockState::Unknown) {
                entry.chain_state = ChainState::NotOnchain(BlockState::ValidationScheduled);
                self.update_longest_chain_cache();
            } else if entry.chain_state == ChainState::Validated(BlockState::Unknown) {
                entry.chain_state = ChainState::Validated(BlockState::ValidationScheduled);
                self.update_longest_chain_cache();
            }
        }
        Ok(())
    }

    pub fn mark_block_as_valid(&mut self, block_hash: &BlockHash) -> eyre::Result<()> {
        if let Some(entry) = self.blocks.get_mut(block_hash) {
            match entry.chain_state {
                ChainState::NotOnchain(BlockState::ValidationScheduled) => {
                    entry.chain_state = ChainState::NotOnchain(BlockState::ValidBlock);
                    self.update_longest_chain_cache();
                    Ok(())
                }
                // When we add blocks to the block tree that we produced locally, they are added
                // as ChainState::Validated but we can still schedule them for full validation
                // with their BlockState.
                ChainState::Validated(BlockState::ValidationScheduled) => {
                    entry.chain_state = ChainState::Validated(BlockState::ValidBlock);
                    self.update_longest_chain_cache();
                    Ok(())
                }
                _ => Err(eyre::eyre!(
                    "unable to mark block as valid: chain_state {:?} {}",
                    entry.chain_state,
                    entry.block.block_hash,
                )),
            }
        } else {
            Err(eyre::eyre!(
                "unable to mark block as valid: block not found"
            ))
        }
    }

    /// Gets block by hash
    #[must_use]
    pub fn get_block(&self, block_hash: &BlockHash) -> Option<&IrysBlockHeader> {
        self.blocks.get(block_hash).map(|entry| &entry.block)
    }

    pub fn canonical_commitment_snapshot(&self) -> Arc<CommitmentSnapshot> {
        let head_entry = self
            .longest_chain_cache
            .0
            .last()
            .expect("at least one block in the longest chain");

        self.blocks
            .get(&head_entry.block_hash)
            .expect("commitment snapshot for block")
            .commitment_snapshot
            .clone()
    }

    pub fn get_commitment_snapshot(
        &self,
        block_hash: &BlockHash,
    ) -> eyre::Result<Arc<CommitmentSnapshot>> {
        match self.blocks.get(block_hash) {
            Some(entry) => Ok(entry.commitment_snapshot.clone()),
            None => Err(eyre::eyre!("Block not found: {}", block_hash)),
        }
    }

    /// Get EMA cache for a specific block
    pub fn get_ema_snapshot(&self, block_hash: &BlockHash) -> Option<Arc<EmaSnapshot>> {
        self.blocks
            .get(block_hash)
            .map(|entry| entry.ema_snapshot.clone())
    }

    /// Returns the current possible set of candidate hashes for a given height.
    pub fn get_hashes_for_height(&self, height: u64) -> Option<&HashSet<BlockHash>> {
        self.height_index.get(&height)
    }

    /// Gets block and its current validation status
    #[must_use]
    pub fn get_block_and_status(
        &self,
        block_hash: &BlockHash,
    ) -> Option<(&IrysBlockHeader, &ChainState)> {
        self.blocks
            .get(block_hash)
            .map(|entry| (&entry.block, &entry.chain_state))
    }

    /// Get the block with maximum cumulative difficulty
    pub fn get_max_cumulative_difficulty_block(&self) -> (U256, BlockHash) {
        self.max_cumulative_difficulty
    }

    /// Check if a block can be built upon
    pub fn can_be_built_upon(&self, block_hash: &BlockHash) -> bool {
        self.blocks.get(block_hash).is_some_and(|entry| {
            matches!(
                entry.chain_state,
                ChainState::Onchain | ChainState::Validated(BlockState::ValidBlock)
            )
        })
    }

    /// Walk backwards from a block to find the validation status of the chain
    /// Returns: (blocks_awaiting_validation, last_validated_block_hash)
    pub fn get_validation_chain_status(
        &self,
        from_block_hash: &BlockHash,
    ) -> (usize, Option<BlockHash>) {
        let mut current_hash = *from_block_hash;
        let mut blocks_awaiting = 0;
        let mut last_validated = None;

        while let Some(entry) = self.blocks.get(&current_hash) {
            match entry.chain_state {
                ChainState::Onchain | ChainState::Validated(BlockState::ValidBlock) => {
                    // Found a validated block
                    if last_validated.is_none() {
                        last_validated = Some(current_hash);
                    }
                }
                ChainState::NotOnchain(BlockState::ValidBlock) => {
                    // This block passed validation but isn't on chain yet
                    if last_validated.is_none() {
                        blocks_awaiting += 1;
                    }
                }
                ChainState::NotOnchain(BlockState::Unknown | BlockState::ValidationScheduled)
                | ChainState::Validated(BlockState::Unknown | BlockState::ValidationScheduled) => {
                    // This block is awaiting validation
                    blocks_awaiting += 1;
                }
            }

            if entry.block.height == 0 {
                break; // Reached genesis
            }

            current_hash = entry.block.previous_block_hash;
        }

        (blocks_awaiting, last_validated)
    }

    /// Collect previous blocks up to the last on-chain block
    fn get_fork_blocks(&self, block: &IrysBlockHeader) -> Vec<&IrysBlockHeader> {
        let mut prev_hash = block.previous_block_hash;
        let mut fork_blocks = Vec::new();

        while let Some(prev_entry) = self.blocks.get(&prev_hash) {
            match prev_entry.chain_state {
                ChainState::Onchain => {
                    fork_blocks.push(&prev_entry.block);
                    break;
                }
                ChainState::Validated(_) | ChainState::NotOnchain(_) => {
                    fork_blocks.push(&prev_entry.block);
                    prev_hash = prev_entry.block.previous_block_hash;
                }
            }
        }

        fork_blocks
    }

    /// Finds the earliest not validated block, walking back the chain
    /// until finding a validated block, reaching block height 0, or exceeding cache depth
    #[must_use]
    fn get_earliest_not_onchain<'a>(
        &'a self,
        block: &'a BlockEntry,
    ) -> Option<(&'a BlockEntry, Vec<&'a IrysBlockHeader>, SystemTime)> {
        let mut current_entry = block;
        let mut prev_block = &current_entry.block;
        let mut depth_count = 0;

        while prev_block.height > 0 && depth_count < self.consensus_config.block_cache_depth {
            let prev_hash = prev_block.previous_block_hash;
            let prev_entry = self.blocks.get(&prev_hash)?;
            debug!(
                "\u{001b}[32mget_earliest_not_onchain: prev_entry.chain_state: {:?} {} height: {}\u{001b}[0m",
                prev_entry.chain_state, prev_hash, prev_entry.block.height
            );
            match prev_entry.chain_state {
                ChainState::Validated(BlockState::ValidBlock) | ChainState::Onchain => {
                    return Some((
                        current_entry,
                        self.get_fork_blocks(prev_block),
                        current_entry.timestamp,
                    ));
                }
                ChainState::NotOnchain(_) | ChainState::Validated(_) => {
                    current_entry = prev_entry;
                    prev_block = &current_entry.block;
                    depth_count += 1;
                }
            }
        }

        // If we've reached height 0 or exceeded cache depth, return None
        None
    }

    /// Get the earliest unvalidated block from the longest chain
    /// Relies on the `longest_chain_cache`
    #[must_use]
    pub fn get_earliest_not_onchain_in_longest_chain(
        &self,
    ) -> Option<(&BlockEntry, Vec<&IrysBlockHeader>, SystemTime)> {
        // Get the block with max cumulative difficulty
        let (_max_cdiff, max_diff_hash) = self.max_cumulative_difficulty;

        // Get the tip's cumulative difficulty
        let tip_entry = self.blocks.get(&self.tip)?;
        let tip_cdiff = tip_entry.block.cumulative_diff;

        // Check if tip's difficulty exceeds max difficulty
        if tip_cdiff >= self.max_cumulative_difficulty.0 {
            return None;
        }

        // Get the block with max difficulty
        let entry = self.blocks.get(&max_diff_hash)?;

        debug!(
            "get_earliest_not_onchain_in_longest_chain() with max_diff_hash: {} height: {} state: {:?}",
            max_diff_hash, entry.block.height, entry.chain_state
        );

        // Check if it's part of a fork and get the start of the fork
        if let ChainState::NotOnchain(_) | ChainState::Validated(_) = &entry.chain_state {
            self.get_earliest_not_onchain(entry)
        } else {
            None
        }
    }

    /// Gets block with matching solution hash, excluding specified block.
    /// Returns a block meeting these requirements:
    /// - Has matching `solution_hash`
    /// - Is not the excluded block
    /// - Either has same `cumulative_diff` as input or meets double-signing criteria
    #[must_use]
    pub fn get_by_solution_hash(
        &self,
        solution_hash: &H256,
        excluding: &BlockHash,
        cumulative_difficulty: U256,
        previous_cumulative_difficulty: U256,
    ) -> Option<&IrysBlockHeader> {
        // Get set of blocks with this solution hash
        let block_hashes = self.solutions.get(solution_hash)?;

        let mut best_block = None;

        // Examine each block hash
        for &hash in block_hashes {
            // Skip the excluded block
            if hash == *excluding {
                continue;
            }

            if let Some(entry) = self.blocks.get(&hash) {
                let block = &entry.block;

                // Case 1: Exact cumulative_diff match - return immediately
                if block.cumulative_diff == cumulative_difficulty {
                    return Some(block);
                }

                // Case 2: Double signing case - return immediately
                if block.cumulative_diff > previous_cumulative_difficulty
                    && cumulative_difficulty > block.previous_cumulative_diff
                {
                    return Some(block);
                }

                // Store as best block seen so far if we haven't found one yet
                if best_block.is_none() {
                    best_block = Some(block);
                }
            }
        }

        // Return best block found (if any)
        best_block
    }

    /// Prunes blocks below specified depth from tip. When pruning an on-chain block,
    /// removes all its non-on-chain children regardless of their height.
    pub fn prune(&mut self, depth: u64) -> eyre::Result<()> {
        if self.blocks.is_empty() {
            return Ok(());
        }

        let tip_height = self
            .blocks
            .get(&self.tip)
            .ok_or_else(|| eyre::eyre!("Tip block not found"))?
            .block
            .height;
        let min_keep_height = tip_height.saturating_sub(depth);

        let min_height = match self.height_index.keys().min() {
            Some(&h) => h,
            None => return Ok(()),
        };

        let mut current_height = min_height;
        while current_height < min_keep_height {
            let Some(hashes) = self.height_index.get(&current_height) else {
                current_height += 1;
                continue;
            };

            // Clone hashes to avoid borrow issues during removal
            let hashes: Vec<_> = hashes.iter().copied().collect();

            for hash in hashes {
                if let Some(entry) = self.blocks.get(&hash) {
                    if matches!(entry.chain_state, ChainState::Onchain) {
                        // First remove all non-on-chain children
                        let children = entry.children.clone();
                        for child in children {
                            if let Some(child_entry) = self.blocks.get(&child) {
                                if !matches!(child_entry.chain_state, ChainState::Onchain) {
                                    self.remove_block(&child)?;
                                }
                            }
                        }

                        // Now remove just this block
                        self.delete_block(&hash)?;
                    }
                }
            }

            current_height += 1;
        }

        Ok(())
    }

    /// Returns true if solution hash exists in cache
    #[must_use]
    pub fn is_known_solution_hash(&self, solution_hash: &H256) -> bool {
        self.solutions.contains_key(solution_hash)
    }
}

fn build_current_ema_snapshot_from_index(
    latest_block_from_index: &IrysBlockHeader,
    db: DatabaseProvider,
    config: &ConsensusConfig,
) -> Arc<EmaSnapshot> {
    let tx = db.tx().unwrap();

    // Start from the provided latest block
    let mut current_block = latest_block_from_index.clone();

    // Collect blocks by walking up the chain
    let mut chain_blocks = vec![current_block.clone()];
    let blocks_to_collect = config.ema.price_adjustment_interval * 3;

    // Walk backwards through the chain, collecting blocks
    while chain_blocks.len() < blocks_to_collect as usize && current_block.height > 0 {
        current_block = block_header_by_hash(&tx, &current_block.previous_block_hash, false)
            .unwrap()
            .expect("previous block must be in database");
        chain_blocks.push(current_block.clone());
    }

    // Reverse to have oldest blocks first
    chain_blocks.reverse();

    // Pass the collected blocks to create_ema_snapshot_from_chain_history
    create_ema_snapshot_from_chain_history(&chain_blocks, config).unwrap_or_else(|err| {
        panic!("Failed to create EMA snapshot from chain history: {}", err);
    })
}

pub async fn get_optimistic_chain(tree: BlockTreeReadGuard) -> eyre::Result<Vec<(H256, u64)>> {
    let canonical_chain = tokio::task::spawn_blocking(move || {
        let cache = tree.read();

        let mut blocks_to_collect = cache.consensus_config.block_cache_depth;
        let mut chain_cache = Vec::with_capacity(
            blocks_to_collect
                .try_into()
                .expect("u64 must fit into usize"),
        );
        let mut current = cache.max_cumulative_difficulty.1;
        debug!("get_optimistic_chain with latest_cache_tip: {}", current);

        while let Some(entry) = cache.blocks.get(&current) {
            chain_cache.push((current, entry.block.height));

            if blocks_to_collect == 0 {
                break;
            }
            blocks_to_collect -= 1;

            if entry.block.height == 0 {
                break;
            } else {
                current = entry.block.previous_block_hash;
            }
        }

        chain_cache.reverse();
        chain_cache
    })
    .await?;
    Ok(canonical_chain)
}

/// Returns the canonical chain where the first item in the Vec is the oldest block
/// Uses spawn_blocking to prevent the read operation from blocking the async executor
/// and locking other async tasks while traversing the block tree.
/// Notably useful in single-threaded tokio based unittests.
pub async fn get_canonical_chain(
    tree: BlockTreeReadGuard,
) -> eyre::Result<(Vec<BlockTreeEntry>, usize)> {
    let canonical_chain =
        tokio::task::spawn_blocking(move || tree.read().get_canonical_chain()).await?;
    Ok(canonical_chain)
}

/// Reconstructs the commitment snapshot for the current epoch by loading all commitment
/// transactions from blocks since the last epoch boundary.
///
/// Iterates through all blocks from the first block after the most recent epoch block
/// up to the latest block, collecting and applying all commitment transactions to build
/// the current epoch's commitment state. This is typically used during startup or when
/// the commitment snapshot needs to be rebuilt from persistent storage.
///
/// # Returns
/// Initialized commitment snapshot containing all commitments from the current epoch
pub fn build_current_commitment_snapshot_from_index(
    block_index_guard: BlockIndexReadGuard,
    commitment_state_guard: CommitmentStateReadGuard,
    db: DatabaseProvider,
    consensus_config: &ConsensusConfig,
) -> CommitmentSnapshot {
    let num_blocks_in_epoch = consensus_config.epoch.num_blocks_in_epoch;
    let block_index = block_index_guard.read();
    let latest_item = block_index.get_latest_item();

    let mut snapshot = CommitmentSnapshot::default();

    if let Some(latest_item) = latest_item {
        let tx = db.tx().unwrap();

        let latest = block_header_by_hash(&tx, &latest_item.block_hash, false)
            .unwrap()
            .expect("block_index block to be in database");
        let last_epoch_block_height = latest.height - (latest.height % num_blocks_in_epoch);

        let start = last_epoch_block_height + 1;

        // Loop though all the blocks starting with the first block following the last epoch block
        for height in start..=latest.height {
            // Query each block to see if they have commitment txids
            let block_item = block_index.get_item(height).unwrap();
            let block = block_header_by_hash(&tx, &block_item.block_hash, false)
                .unwrap()
                .expect("block_index block to be in database");

            let commitment_tx_ids = block.get_commitment_ledger_tx_ids();
            if !commitment_tx_ids.is_empty() {
                // If so, retrieve the full commitment transactions
                for txid in commitment_tx_ids {
                    let commitment_tx = commitment_tx_by_txid(&tx, &txid)
                        .unwrap()
                        .expect("commitment transactions to be in database");

                    let is_staked_in_current_epoch =
                        commitment_state_guard.is_staked(commitment_tx.signer);

                    // Apply them to the commitment snapshot
                    let _status =
                        snapshot.add_commitment(&commitment_tx, is_staked_in_current_epoch);
                }
            }
        }
    }

    // Return the initialized commitment snapshot
    snapshot
}

/// Creates a new commitment snapshot for the given block based on commitment transactions
/// and the previous commitment snapshot.
///
/// ## Behavior
/// - Returns a fresh empty snapshot if this is an epoch block (height divisible by num_blocks_in_epoch)
/// - Returns a clone of the previous snapshot if no commitment transactions are present in the new block
/// - Otherwise, creates a new commitment snapshot by adding all commitment transactions to a copy of the
/// previous snapshot
///
/// ## Arguments
/// * `block` - The block header to create a commitment snapshot for
/// * `commitment_txs` - Slice of commitment transactions to process for this block (should match txids in the block)
/// * `prev_commitment_snapshot` - The commitment snapshot from the previous block
/// * `consensus_config` - Configuration containing epoch settings
/// * `commitment_state_guard` - Read guard for checking staking status of transaction signers
///
/// # Returns
/// Arc-wrapped commitment snapshot for the new block
fn create_commitment_snapshot_for_block(
    block: &IrysBlockHeader,
    commitment_txs: &[CommitmentTransaction],
    prev_commitment_snapshot: &Arc<CommitmentSnapshot>,
    consensus_config: &ConsensusConfig,
    commitment_state_guard: &CommitmentStateReadGuard,
) -> Arc<CommitmentSnapshot> {
    let is_epoch_block = block.height % consensus_config.epoch.num_blocks_in_epoch == 0;

    if is_epoch_block {
        return Arc::new(CommitmentSnapshot::default());
    }

    if commitment_txs.is_empty() {
        return prev_commitment_snapshot.clone();
    }

    let mut new_commitment_snapshot = (**prev_commitment_snapshot).clone();
    for commitment_tx in commitment_txs {
        let is_staked_in_current_epoch = commitment_state_guard.is_staked(commitment_tx.signer);
        new_commitment_snapshot.add_commitment(commitment_tx, is_staked_in_current_epoch);
    }
    Arc::new(new_commitment_snapshot)
}

/// Loads commitment transactions from the database for the given block's commitment ledger transaction IDs.
fn load_commitment_transactions(
    block: &IrysBlockHeader,
    db: &DatabaseProvider,
) -> eyre::Result<Vec<CommitmentTransaction>> {
    let commitment_tx_ids = block.get_commitment_ledger_tx_ids();
    if commitment_tx_ids.is_empty() {
        return Ok(Vec::new());
    }

    // Startup: query database directly
    let mut txs = Vec::new();
    let db_tx = db.tx().expect("to create a read only tx for the db");
    for tx_id in &commitment_tx_ids {
        if let Some(header) =
            commitment_tx_by_txid(&db_tx, tx_id).expect("to retrieve tx header from db")
        {
            txs.push(header);
        }
    }
    Ok(txs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use eyre::ensure;
    use rstest::rstest;

    fn dummy_ema_snapshot() -> Arc<EmaSnapshot> {
        let config = irys_types::ConsensusConfig::testnet();
        let genesis_header = IrysBlockHeader {
            oracle_irys_price: config.genesis_price,
            ema_irys_price: config.genesis_price,
            ..Default::default()
        };
        EmaSnapshot::genesis(&genesis_header)
    }

    #[actix::test]
    async fn test_block_cache() {
        let b1 = random_block(U256::from(0));

        // For the purposes of these tests, the block cache will not track transaction headers
        let comm_cache = Arc::new(CommitmentSnapshot::default());

        // Initialize block tree cache from `b1`
        let mut cache = BlockTreeCache::new(&b1, ConsensusConfig::testnet());

        // Verify cache returns `None` for unknown hashes
        assert_eq!(cache.get_block(&H256::random()), None);
        assert_eq!(
            cache.get_by_solution_hash(&H256::random(), &H256::random(), U256::one(), U256::one()),
            None
        );

        // Verify cache returns the expected block
        assert_eq!(cache.get_block(&b1.block_hash), Some(&b1));
        assert_eq!(
            cache.get_by_solution_hash(
                &b1.solution_hash,
                &H256::random(),
                U256::one(),
                U256::one()
            ),
            Some(&b1)
        );

        // Verify getting by `solution_hash` excludes the expected block
        assert_matches!(
            cache.get_by_solution_hash(&b1.solution_hash, &b1.block_hash, U256::one(), U256::one()),
            None
        );

        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Adding `b1` again shouldn't change the state because it is confirmed
        // onchain
        let mut b1_test = b1.clone();
        b1_test.data_ledgers[DataLedger::Submit]
            .tx_ids
            .push(H256::random());
        assert_matches!(
            cache.add_block(&b1_test, comm_cache.clone(), dummy_ema_snapshot()),
            Ok(_)
        );
        assert_eq!(
            cache.get_block(&b1.block_hash).unwrap().data_ledgers[DataLedger::Submit]
                .tx_ids
                .len(),
            0
        );
        assert_eq!(
            cache
                .get_by_solution_hash(&b1.solution_hash, &H256::random(), U256::one(), U256::one())
                .unwrap()
                .data_ledgers[DataLedger::Submit]
                .tx_ids
                .len(),
            0
        );
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Same as above, `get_deepest_unvalidated_in_longest_chain` should not
        // modify state
        assert_matches!(cache.get_earliest_not_onchain_in_longest_chain(), None);
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Add b2 block as not_validated
        let mut b2 = extend_chain(random_block(U256::from(1)), &b1);
        assert_matches!(
            cache.add_block(&b2, comm_cache.clone(), dummy_ema_snapshot()),
            Ok(_)
        );
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b2.block_hash
        );

        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Add a TXID to b2, and re-add it to the cache, but still don't mark as validated
        let txid = H256::random();
        b2.data_ledgers[DataLedger::Submit].tx_ids.push(txid);
        assert_matches!(
            cache.add_block(&b2, comm_cache.clone(), dummy_ema_snapshot()),
            Ok(_)
        );
        assert_eq!(
            cache.get_block(&b2.block_hash).unwrap().data_ledgers[DataLedger::Submit].tx_ids[0],
            txid
        );
        assert_eq!(
            cache
                .get_by_solution_hash(&b2.solution_hash, &H256::random(), U256::one(), U256::one())
                .unwrap()
                .data_ledgers[DataLedger::Submit]
                .tx_ids[0],
            txid
        );
        assert_eq!(
            cache
                .get_by_solution_hash(&b2.solution_hash, &b1.block_hash, U256::one(), U256::one())
                .unwrap()
                .data_ledgers[DataLedger::Submit]
                .tx_ids[0],
            txid
        );

        // Remove b2_1
        assert_matches!(cache.remove_block(&b2.block_hash), Ok(_));
        assert_eq!(cache.get_block(&b2.block_hash), None);
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Remove b2_1 again
        assert_matches!(cache.remove_block(&b2.block_hash), Err(_));

        // Re-add b2_1 and add a competing b2 block called b1_2, it will be built
        // on b1 but share the same solution_hash
        assert_matches!(
            cache.add_block(&b2, comm_cache.clone(), dummy_ema_snapshot()),
            Ok(_)
        );
        let mut b1_2 = extend_chain(random_block(U256::from(2)), &b1);
        b1_2.solution_hash = b1.solution_hash;
        assert_matches!(
            cache.add_block(&b1_2, comm_cache.clone(), dummy_ema_snapshot()),
            Ok(_)
        );

        println!(
            "b1:   {} cdiff: {} solution_hash: {}",
            b1.block_hash, b1.cumulative_diff, b1.solution_hash
        );
        println!(
            "b2:   {} cdiff: {} solution_hash: {}",
            b2.block_hash, b2.cumulative_diff, b2.solution_hash
        );
        println!(
            "b1_2: {} cdiff: {} solution_hash: {}",
            b1_2.block_hash, b1_2.cumulative_diff, b1_2.solution_hash
        );

        // Verify if we exclude b1_2 we wont get it back
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b1.solution_hash,
                    &b1_2.block_hash,
                    U256::one(),
                    U256::one()
                )
                .unwrap()
                .block_hash,
            b1.block_hash
        );

        // Verify that we do get b1_2 back when not excluding it
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b1.solution_hash,
                    &BlockHash::random(),
                    b1_2.cumulative_diff,
                    U256::one()
                )
                .unwrap()
                .block_hash,
            b1_2.block_hash
        );

        // Get result with empty excluded hash
        let result = cache
            .get_by_solution_hash(
                &b1.solution_hash,
                &BlockHash::default(), // Empty/zeroed hash
                U256::one(),
                U256::one(),
            )
            .unwrap();

        // Assert result is either b1 or b1_2
        assert!(
            result.block_hash == b1.block_hash || result.block_hash == b1_2.block_hash,
            "Expected either b1 or b1_2 to be returned"
        );
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Even though b2 is marked as a tip, it is still lower difficulty than b1_2 so will
        // not be included in the longest chain
        assert_matches!(cache.mark_tip(&b2.block_hash), Ok(_));
        assert_eq!(Some(&b1_2), cache.get_block(&b1_2.block_hash));
        assert_matches!(check_longest_chain(&[&b1], 0, &cache), Ok(_));

        // Remove b1_2, causing b2 to now be the tip of the heaviest chain
        assert_matches!(cache.remove_block(&b1_2.block_hash), Ok(_));
        assert_eq!(cache.get_block(&b1_2.block_hash), None);
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b1.solution_hash,
                    &BlockHash::random(),
                    U256::zero(),
                    U256::zero()
                )
                .unwrap()
                .block_hash,
            b1.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2], 0, &cache), Ok(_));

        // Prune to a depth of 1 behind the tip
        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(Some(&b1), cache.get_block(&b1.block_hash));
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b1.solution_hash,
                    &BlockHash::random(),
                    U256::zero(),
                    U256::zero()
                )
                .unwrap()
                .block_hash,
            b1.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2], 0, &cache), Ok(_));

        // Prune at the tip, removing all ancestors (verify b1 is pruned)
        assert_matches!(cache.prune(0), Ok(_));
        assert_eq!(None, cache.get_block(&b1.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b1.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2], 0, &cache), Ok(_));

        // Again, this time to make sure b1_2 is really gone
        assert_matches!(cache.prune(0), Ok(_));
        assert_eq!(None, cache.get_block(&b1_2.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b1_2.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2], 0, &cache), Ok(_));

        // <Reset the cache>
        // b1_2->b1 fork is the heaviest, but only b1 is validated. b2_2->b2->b1 is longer but
        // has a lower cdiff.
        let mut cache = BlockTreeCache::new(&b1, ConsensusConfig::testnet());
        assert_matches!(
            cache.add_block(&b1_2, comm_cache.clone(), dummy_ema_snapshot()),
            Ok(_)
        );
        assert_matches!(
            cache.add_block(&b2, comm_cache.clone(), dummy_ema_snapshot()),
            Ok(_)
        );
        assert_matches!(cache.mark_tip(&b2.block_hash), Ok(_));
        let b2_2 = extend_chain(random_block(U256::one()), &b2);
        println!(
            "b2_2: {} cdiff: {} solution_hash: {}",
            b2_2.block_hash, b2_2.cumulative_diff, b2_2.solution_hash
        );
        assert_matches!(
            cache.add_block(&b2_2, comm_cache.clone(), dummy_ema_snapshot()),
            Ok(_)
        );
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b1_2.block_hash
        );

        // b2_3->b2_2->b2->b1 is longer and heavier but only b2->b1 are validated.
        let b2_3 = extend_chain(random_block(U256::from(3)), &b2_2);
        println!(
            "b2_3: {} cdiff: {} solution_hash: {}",
            b2_3.block_hash, b2_3.cumulative_diff, b2_3.solution_hash
        );
        assert_matches!(
            cache.add_block(&b2_3, comm_cache.clone(), dummy_ema_snapshot()),
            Ok(_)
        );
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b2_2.block_hash
        );
        assert_matches!(cache.mark_tip(&b2_3.block_hash), Err(_));
        assert_matches!(check_longest_chain(&[&b1, &b2], 0, &cache), Ok(_));

        // Now b2_2->b2->b1 are validated.
        assert_matches!(
            cache.add_common(
                &b2_2,
                comm_cache.clone(),
                dummy_ema_snapshot(),
                ChainState::Validated(BlockState::ValidBlock)
            ),
            Ok(())
        );
        assert_eq!(
            cache.get_block_and_status(&b2_2.block_hash).unwrap(),
            (&b2_2, &ChainState::Validated(BlockState::ValidBlock))
        );
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b2_3.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2, &b2_2], 1, &cache), Ok(_));

        // Now the b3->b2->b1 fork is heaviest
        let b3 = extend_chain(random_block(U256::from(4)), &b2);
        println!(
            "b3:   {} cdiff: {} solution_hash: {}",
            b3.block_hash, b3.cumulative_diff, b3.solution_hash
        );
        assert_matches!(
            cache.add_block(&b3, comm_cache.clone(), dummy_ema_snapshot()),
            Ok(_)
        );
        assert_matches!(
            cache.add_common(
                &b3,
                comm_cache.clone(),
                dummy_ema_snapshot(),
                ChainState::Validated(BlockState::ValidBlock)
            ),
            Ok(())
        );
        assert_matches!(cache.mark_tip(&b3.block_hash), Ok(_));
        assert_matches!(cache.get_earliest_not_onchain_in_longest_chain(), None);
        assert_matches!(check_longest_chain(&[&b1, &b2, &b3], 0, &cache), Ok(_));

        // b3->b2->b1 fork is still heaviest
        assert_matches!(cache.mark_tip(&b2_2.block_hash), Ok(_));
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b3.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2, &b3], 1, &cache), Ok(_));

        // add not validated b4, b3->b2->b1 fork is still heaviest
        let b4 = extend_chain(random_block(U256::from(5)), &b3);
        println!(
            "b4:   {} cdiff: {} solution_hash: {}",
            b4.block_hash, b4.cumulative_diff, b4.solution_hash
        );
        assert_matches!(
            cache.add_block(&b4, comm_cache.clone(), dummy_ema_snapshot()),
            Ok(_)
        );
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b4.block_hash
        );
        assert_matches!(check_longest_chain(&[&b1, &b2, &b3], 1, &cache), Ok(_));

        // Prune to a depth of 1 past the tip and verify b1 and the b1_2 branch are pruned
        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(None, cache.get_block(&b1.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b1.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2, &b3], 1, &cache), Ok(_));

        // Mark a new tip for the longest chain, validating b2_3, and pruning again
        assert_matches!(cache.mark_tip(&b2_3.block_hash), Ok(_));
        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(None, cache.get_block(&b2.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b2.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // Also make sure b3 (the old tip) got pruned
        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(None, cache.get_block(&b3.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b3.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // and that the not yet validated b4 was also pruned
        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(None, cache.get_block(&b4.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b4.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // Now make sure b2_2 and b2_3 are still in the cache/state
        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(Some(&b2_2), cache.get_block(&b2_2.block_hash));
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b2_2.solution_hash,
                    &BlockHash::random(),
                    U256::zero(),
                    U256::zero()
                )
                .unwrap()
                .block_hash,
            b2_2.block_hash
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        assert_matches!(cache.prune(1), Ok(_));
        assert_eq!(Some(&b2_3), cache.get_block(&b2_3.block_hash));
        assert_eq!(
            cache
                .get_by_solution_hash(
                    &b2_3.solution_hash,
                    &BlockHash::random(),
                    U256::zero(),
                    U256::zero()
                )
                .unwrap()
                .block_hash,
            b2_3.block_hash
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // Verify previously pruned b3 can be safely removed again, with longest chain cache staying stable
        assert_matches!(cache.remove_block(&b3.block_hash), Err(e) if e.to_string() == "Block not found");
        assert_eq!(None, cache.get_block(&b3.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b3.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // Same safety check for b4 - removing already pruned block shouldn't affect chain state
        assert_matches!(cache.remove_block(&b4.block_hash), Err(e) if e.to_string() == "Block not found");
        assert_eq!(None, cache.get_block(&b4.block_hash));
        assert_eq!(
            None,
            cache.get_by_solution_hash(
                &b4.solution_hash,
                &BlockHash::random(),
                U256::zero(),
                U256::zero()
            )
        );
        assert_matches!(check_longest_chain(&[&b2_2, &b2_3], 0, &cache), Ok(_));

        // <Reset the cache>
        let b11 = random_block(U256::zero());
        let mut cache = BlockTreeCache::new(&b11, ConsensusConfig::testnet());
        let b12 = extend_chain(random_block(U256::one()), &b11);
        assert_matches!(
            cache.add_block(&b12, comm_cache.clone(), dummy_ema_snapshot()),
            Ok(_)
        );
        let b13 = extend_chain(random_block(U256::one()), &b11);

        println!("---");
        println!(
            "b11: {} cdiff: {} solution_hash: {}",
            b11.block_hash, b11.cumulative_diff, b11.solution_hash
        );
        println!(
            "b12: {} cdiff: {} solution_hash: {}",
            b12.block_hash, b12.cumulative_diff, b12.solution_hash
        );
        println!(
            "b13: {} cdiff: {} solution_hash: {}",
            b13.block_hash, b13.cumulative_diff, b13.solution_hash
        );
        println!("tip: {} before mark_tip()", cache.tip);

        assert_matches!(
            cache.add_common(
                &b13,
                comm_cache.clone(),
                dummy_ema_snapshot(),
                ChainState::Validated(BlockState::ValidBlock)
            ),
            Ok(())
        );
        let reorg = cache.mark_tip(&b13.block_hash).unwrap();

        // The tip does change here, even though it's not part of the longest
        // chain, this seems like a bug
        println!("tip: {} after mark_tip()", cache.tip);
        assert!(reorg);

        assert_matches!(cache.get_earliest_not_onchain_in_longest_chain(), None);
        // Although b13 becomes the tip, it's not included in the longest_chain_cache.
        // This is because the cache follows blocks from max_cumulative_difficulty, which
        // was set to b12 when it was first added. When multiple blocks have the same
        // cumulative difficulty, max_cumulative_difficulty preserves the first one seen.
        // Since b13's difficulty equals b12's (rather than exceeds it), b12 remains the
        // reference point for longest chain calculations.

        // Block tree state:
        //
        //                     [B13] cdiff=1, Validated
        //                    /  ⚡ marked as tip but not in longest chain
        //                   /     because B12 has same cdiff & was first
        //                  /
        // [B11] cdiff=0 --+-- [B12] cdiff=1, NotValidated (first added)
        // (genesis - tip)       ⚠ not counted as onchain due to not being validated
        //
        // ▶ Longest chain contains: [B11]
        // ▶ Not on chain count: 0
        // ▶ First added wins longest_chain with equal cdiff

        // DMac's Note:
        // Issue: tip and longest chain can become misaligned when marking a tip that has
        //   equal difficulty to an earlier block. The tip could point to b13 while the
        //   longest chain contains b12, as b12 was seen first with same difficulty.
        // Fix: mark_tip() should reject attempts to change tip to a block that has equal
        //   (rather than greater) difficulty compared to the current max_difficulty block.
        //   This would ensure tip always follows the longest chain. TBH the current behavior
        //   is likely aligned with the local miners economic interest and why things
        //   like "uncle" blocks exist on other chains.
        assert_matches!(check_longest_chain(&[&b11], 0, &cache), Ok(_));

        // Extend the b13->b11 chain
        let b14 = extend_chain(random_block(U256::from(2)), &b13);
        assert_matches!(
            cache.add_block(&b14, comm_cache.clone(), dummy_ema_snapshot()),
            Ok(_)
        );
        assert_eq!(
            cache
                .get_earliest_not_onchain_in_longest_chain()
                .unwrap()
                .0
                .block
                .block_hash,
            b14.block_hash
        );
        // by adding b14 we've now made the b13->b11 chain heavier and because
        // b13 is already validated it is included in the longest chain
        // b14 isn't validated so it doesn't count towards the not_onchain_count
        assert_matches!(check_longest_chain(&[&b11, &b13], 0, &cache), Ok(_));

        // Try to mutate the state of the cache with some random validations
        assert_matches!(
            cache.mark_block_as_validation_scheduled(&BlockHash::random()),
            Ok(())
        );
        assert_matches!(cache.mark_block_as_valid(&BlockHash::random()), Err(_));
        // Attempt to mark the already onchain b13 to prior vdf states
        assert_matches!(
            cache.mark_block_as_validation_scheduled(&b13.block_hash),
            Ok(())
        );
        assert_matches!(cache.mark_block_as_valid(&b13.block_hash), Err(_));
        // Verify its state wasn't changed
        assert_eq!(
            cache.get_block_and_status(&b13.block_hash).unwrap(),
            (&b13, &ChainState::Onchain)
        );
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (&b14, &ChainState::NotOnchain(BlockState::Unknown))
        );
        // Verify none of this affected the longest chain cache
        assert_matches!(check_longest_chain(&[&b11, &b13], 0, &cache), Ok(_));

        // Move b14 though the vdf validation states
        assert_matches!(
            cache.mark_block_as_validation_scheduled(&b14.block_hash),
            Ok(())
        );
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (
                &b14,
                &ChainState::NotOnchain(BlockState::ValidationScheduled)
            )
        );
        assert_matches!(
            check_earliest_not_onchain(
                &b14.block_hash,
                &ChainState::NotOnchain(BlockState::ValidationScheduled),
                &cache
            ),
            Ok(())
        );
        // Verify none of this affected the longest chain cache
        assert_matches!(check_longest_chain(&[&b11, &b13], 0, &cache), Ok(_));

        // now mark b14 as vdf validated
        assert_matches!(cache.mark_block_as_valid(&b14.block_hash), Ok(_));
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (&b14, &ChainState::NotOnchain(BlockState::ValidBlock))
        );
        assert_matches!(
            check_earliest_not_onchain(
                &b14.block_hash,
                &ChainState::NotOnchain(BlockState::ValidBlock),
                &cache
            ),
            Ok(())
        );
        // Now that b14 is vdf validated it can be considered a NotOnchain
        // part of the longest chain
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // add a b15 block
        let b15 = extend_chain(random_block(U256::from(3)), &b14);
        assert_matches!(
            cache.add_block(&b15, comm_cache.clone(), dummy_ema_snapshot()),
            Ok(_)
        );
        assert_matches!(
            check_earliest_not_onchain(
                &b14.block_hash,
                &ChainState::NotOnchain(BlockState::ValidBlock),
                &cache
            ),
            Ok(())
        );
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // Validate b14
        assert_matches!(
            cache.add_common(
                &b14,
                comm_cache.clone(),
                dummy_ema_snapshot(),
                ChainState::Validated(BlockState::ValidBlock)
            ),
            Ok(())
        );
        assert_matches!(
            check_earliest_not_onchain(
                &b15.block_hash,
                &ChainState::NotOnchain(BlockState::Unknown),
                &cache
            ),
            Ok(())
        );
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (&b14, &ChainState::Validated(BlockState::ValidBlock))
        );
        // b14 is validated, but wont be onchain until is tip_height or lower
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // add a b16 block
        let b16 = extend_chain(random_block(U256::from(4)), &b15);
        assert_matches!(
            cache.add_block(&b16, comm_cache.clone(), dummy_ema_snapshot()),
            Ok(_)
        );
        assert_matches!(
            cache.mark_block_as_validation_scheduled(&b16.block_hash),
            Ok(())
        );
        assert_matches!(
            check_earliest_not_onchain(
                &b15.block_hash,
                &ChainState::NotOnchain(BlockState::Unknown),
                &cache
            ),
            Ok(())
        );
        // Verify the longest chain state isn't changed by b16 pending Vdf validation
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // Mark b16 as vdf validated eve though b15 is not
        assert_matches!(cache.mark_block_as_valid(&b16.block_hash), Ok(_));
        assert_matches!(
            check_earliest_not_onchain(
                &b15.block_hash,
                &ChainState::NotOnchain(BlockState::Unknown),
                &cache
            ),
            Ok(())
        );
        assert_eq!(
            cache.get_block_and_status(&b16.block_hash).unwrap(),
            (&b16, &ChainState::NotOnchain(BlockState::ValidBlock))
        );
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 1, &cache), Ok(_));

        // Make b14 the tip (making it OnChain)
        assert_matches!(cache.mark_tip(&b14.block_hash), Ok(_));
        assert_eq!(
            cache.get_block_and_status(&b14.block_hash).unwrap(),
            (&b14, &ChainState::Onchain)
        );
        assert_matches!(
            check_earliest_not_onchain(
                &b15.block_hash,
                &ChainState::NotOnchain(BlockState::Unknown),
                &cache
            ),
            Ok(())
        );
        assert_matches!(check_longest_chain(&[&b11, &b13, &b14], 0, &cache), Ok(_));

        // <Reset the cache>
        let b11 = random_block(U256::zero());
        let mut cache = BlockTreeCache::new(&b11, ConsensusConfig::testnet());
        let b12 = extend_chain(random_block(U256::one()), &b11);
        assert_matches!(
            cache.add_block(&b12, comm_cache.clone(), dummy_ema_snapshot()),
            Ok(_)
        );
        let _b13 = extend_chain(random_block(U256::one()), &b11);
        println!("---");
        assert_matches!(cache.mark_tip(&b11.block_hash), Ok(_));

        // Verify the longest chain state isn't changed by b16 pending Vdf validation
        assert_matches!(check_longest_chain(&[&b11], 0, &cache), Ok(_));

        // Now add the subsequent block, but as awaitingValidation
        assert_matches!(
            cache.add_common(
                &b12,
                comm_cache.clone(),
                dummy_ema_snapshot(),
                ChainState::Validated(BlockState::ValidationScheduled)
            ),
            Ok(())
        );
        assert_matches!(check_longest_chain(&[&b11, &b12], 1, &cache), Ok(_));

        // When a locally produced block is added as validated "onchain" but it
        // hasn't yet been validated by the validation_service
        assert_matches!(
            check_earliest_not_onchain(
                &b12.block_hash,
                &ChainState::Validated(BlockState::ValidationScheduled),
                &cache
            ),
            Ok(())
        );

        // <Reset the cache>
        let b11 = random_block(U256::zero());
        let mut cache = BlockTreeCache::new(&b11, ConsensusConfig::testnet());
        assert_matches!(cache.mark_tip(&b11.block_hash), Ok(_));

        let b12 = extend_chain(random_block(U256::one()), &b11);
        assert_matches!(
            cache.add_common(
                &b12,
                comm_cache.clone(),
                dummy_ema_snapshot(),
                ChainState::Validated(BlockState::ValidBlock)
            ),
            Ok(())
        );
        assert_matches!(cache.mark_tip(&b12.block_hash), Ok(_));

        assert_matches!(check_longest_chain(&[&b11, &b12], 0, &cache), Ok(_));

        // Create a fork at b12
        let b13a = extend_chain(random_block(U256::from(2)), &b12);
        let b13b = extend_chain(random_block(U256::from(2)), &b12);

        assert_matches!(
            cache.add_common(
                &b13a,
                comm_cache.clone(),
                dummy_ema_snapshot(),
                ChainState::Validated(BlockState::ValidBlock)
            ),
            Ok(())
        );
        assert_matches!(
            cache.add_common(
                &b13b,
                comm_cache.clone(),
                dummy_ema_snapshot(),
                ChainState::Validated(BlockState::ValidBlock)
            ),
            Ok(())
        );

        assert_matches!(check_longest_chain(&[&b11, &b12, &b13a], 1, &cache), Ok(_));

        assert_matches!(cache.mark_tip(&b13a.block_hash), Ok(_));
        assert_matches!(check_longest_chain(&[&b11, &b12, &b13a], 0, &cache), Ok(_));

        // extend the fork to make it canonical
        let b14b = extend_chain(random_block(U256::from(3)), &b13b);
        assert_matches!(
            cache.add_common(
                &b14b,
                comm_cache,
                dummy_ema_snapshot(),
                ChainState::Validated(BlockState::ValidBlock)
            ),
            Ok(())
        );

        assert_matches!(
            check_longest_chain(&[&b11, &b12, &b13b, &b14b], 2, &cache),
            Ok(())
        );

        // Mark the new tip
        assert_matches!(cache.mark_tip(&b14b.block_hash), Ok(_));

        assert_matches!(
            check_longest_chain(&[&b11, &b12, &b13b, &b14b], 0, &cache),
            Ok(())
        );
    }

    fn random_block(cumulative_diff: U256) -> IrysBlockHeader {
        let mut block = IrysBlockHeader::new_mock_header();
        block.block_hash = BlockHash::random();
        block.solution_hash = H256::random(); // Ensure unique solution hash
        block.height = 0; // Default to genesis
        block.cumulative_diff = cumulative_diff;
        block
    }

    const fn extend_chain(
        mut new_block: IrysBlockHeader,
        previous_block: &IrysBlockHeader,
    ) -> IrysBlockHeader {
        new_block.previous_block_hash = previous_block.block_hash;
        new_block.height = previous_block.height + 1;
        new_block.previous_cumulative_diff = previous_block.cumulative_diff;
        // Don't modify solution_hash - keep the random one from block creation
        new_block
    }

    fn check_earliest_not_onchain(
        block_hash: &BlockHash,
        chain_state: &ChainState,
        cache: &BlockTreeCache,
    ) -> eyre::Result<()> {
        let _x = 1;
        if let Some((block_entry, _, _)) = cache.get_earliest_not_onchain_in_longest_chain() {
            let c_s = &block_entry.chain_state;

            ensure!(
                block_entry.block.block_hash == *block_hash,
                "Wrong unvalidated block found: {} expected:{}",
                block_entry.block.block_hash,
                block_hash
            );

            ensure!(
                chain_state == c_s,
                "Wrong validation_state found: {:?}",
                c_s
            );
        } else if let ChainState::NotOnchain(_) = chain_state {
            return Err(eyre::eyre!("No unvalidated blocks found in longest chain"));
        }

        Ok(())
    }

    fn check_longest_chain(
        expected_blocks: &[&IrysBlockHeader],
        expected_not_onchain: usize,
        cache: &BlockTreeCache,
    ) -> eyre::Result<()> {
        let (canonical_blocks, not_onchain_count) = cache.get_canonical_chain();
        let actual_blocks: Vec<_> = canonical_blocks.iter().map(|e| e.block_hash).collect();

        ensure!(
            actual_blocks
                == expected_blocks
                    .iter()
                    .map(|b| b.block_hash)
                    .collect::<Vec<_>>(),
            "Canonical chain does not match expected blocks"
        );
        ensure!(
            not_onchain_count == expected_not_onchain,
            format!(
                "Number of not-onchain blocks ({}) does not match expected ({})",
                not_onchain_count, expected_not_onchain
            )
        );
        Ok(())
    }

    // Helper function to build a chain with specific validation states
    fn build_chain_with_states(
        states: Vec<(ChainState, U256)>,
    ) -> (BlockTreeCache, Vec<IrysBlockHeader>) {
        let comm_cache = Arc::new(CommitmentSnapshot::default());
        let mut blocks = Vec::new();

        // Create genesis block
        let genesis = random_block(U256::from(0));
        let mut cache = BlockTreeCache::new(&genesis, ConsensusConfig::testnet());
        blocks.push(genesis.clone());

        // Add blocks with specified states
        let mut prev_block = genesis;
        for (state, diff) in states {
            let mut block = extend_chain(random_block(diff), &prev_block);
            block.cumulative_diff = prev_block.cumulative_diff + diff;

            // Add block with specified state
            cache
                .add_common(&block, comm_cache.clone(), dummy_ema_snapshot(), state)
                .unwrap();

            blocks.push(block.clone());
            prev_block = block;
        }

        (cache, blocks)
    }

    #[rstest]
    #[case::from_first_block(0, 0, 0)]
    #[case::from_second_block(1, 0, 1)]
    #[case::from_third_block(2, 0, 2)]
    #[case::from_fourth_block(3, 1, 2)]
    #[case::from_fifth_block(4, 2, 2)]
    fn test_get_validation_chain_status_chain_scenario(
        #[case] from_block_index: usize,
        #[case] expected_blocks_awaiting: usize,
        #[case] expected_last_validated_index: usize,
    ) {
        // Create chain: [idx 0 genesis(onchain), idx 1 validated, idx 2 validated, idx 3 scheduled, idx 4 scheduled]
        let states = vec![
            // genesis gets prepended here
            (ChainState::Onchain, U256::from(1)),
            (ChainState::Validated(BlockState::ValidBlock), U256::from(1)),
            (
                ChainState::NotOnchain(BlockState::ValidationScheduled),
                U256::from(1),
            ),
            (
                ChainState::NotOnchain(BlockState::ValidationScheduled),
                U256::from(1),
            ),
        ];

        let (cache, blocks) = build_chain_with_states(states);

        // Test from different positions in the chain
        let test_block_hash = blocks[from_block_index].block_hash;
        let (blocks_awaiting, last_validated) = cache.get_validation_chain_status(&test_block_hash);

        assert_eq!(blocks_awaiting, expected_blocks_awaiting);
        assert_eq!(
            last_validated,
            Some(blocks[expected_last_validated_index].block_hash)
        );
    }

    #[test]
    fn test_get_validation_chain_status_nonexistent_block() {
        let genesis = random_block(U256::from(0));
        let cache = BlockTreeCache::new(&genesis, ConsensusConfig::testnet());

        let nonexistent_hash = H256::random();
        let (blocks_awaiting, last_validated) =
            cache.get_validation_chain_status(&nonexistent_hash);

        // Should return 0 blocks awaiting and no last validated for non-existent block
        assert_eq!(blocks_awaiting, 0);
        assert_eq!(last_validated, None);
    }

    #[test]
    fn test_get_validation_chain_status_all_awaiting() {
        // Create chain where all blocks after genesis are awaiting validation
        let states = vec![
            (ChainState::NotOnchain(BlockState::Unknown), U256::from(1)),
            (
                ChainState::NotOnchain(BlockState::ValidationScheduled),
                U256::from(1),
            ),
            (ChainState::NotOnchain(BlockState::Unknown), U256::from(1)),
        ];

        let (cache, blocks) = build_chain_with_states(states);

        let test_block_hash = blocks[3].block_hash;
        let (blocks_awaiting, last_validated) = cache.get_validation_chain_status(&test_block_hash);

        // All 3 blocks should be awaiting
        assert_eq!(blocks_awaiting, 3);
        // Last validated should be genesis
        assert_eq!(last_validated, Some(blocks[0].block_hash));
    }
}
