pub mod chunk_fetcher;
pub mod chunk_orchestrator;
pub mod peer_bandwidth_manager;
pub mod peer_stats;

use crate::{chunk_fetcher::ChunkFetcherFactory, metrics, services::ServiceSenders};
use chunk_orchestrator::{ChunkBlockReason, ChunkOrchestrator, ChunkRequestState};
use irys_domain::{BlockTreeReadGuard, ChunkType, PeerList, StorageModule, WriteDataChunkError};
use irys_packing::unpack;
use irys_types::{
    Config, IrysAddress, PackedChunk, PartitionChunkOffset, SendTraced as _, TokioServiceHandle,
    Traced, UnpackedChunk,
};
use peer_bandwidth_manager::PeerBandwidthManager;
use reth::tasks::shutdown::Shutdown;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
use tracing::{Instrument as _, debug, error, warn};

/// Local write outcome after a successful peer fetch.
#[derive(Debug, PartialEq, Eq)]
enum DataSyncWriteOutcome {
    /// Offset is durably [`ChunkType::Data`] after pending flush + fsync.
    Stored,
    /// SM has no `DataRootInfos` entry for this data_root — needs index rebuild.
    MissingDataRootIndex,
    /// data_root is indexed but no Entropy target at the expected offsets.
    NoWriteableOffset,
    /// Other write error.
    Other(String),
}

fn attempt_data_sync_write(
    sm: &StorageModule,
    unpacked: &UnpackedChunk,
    expected_offset: PartitionChunkOffset,
) -> DataSyncWriteOutcome {
    match sm.write_data_chunk(unpacked) {
        Err(WriteDataChunkError::DataRootNotFound) => DataSyncWriteOutcome::MissingDataRootIndex,
        Err(e) => DataSyncWriteOutcome::Other(e.to_string()),
        Ok(()) => {
            // write_data_chunk only enqueues into pending_writes; get_chunk_type
            // can report Data before persistence. Do not mark Stored (or
            // Completed in the orchestrator) until force_sync flushes + fsyncs.
            // On flush failure we return Other → requeue; the request stays
            // Requested until this function returns an outcome that advances it.
            if matches!(sm.get_chunk_type(&expected_offset), Some(ChunkType::Data)) {
                if let Err(e) = sm.force_sync_pending_chunks() {
                    return DataSyncWriteOutcome::Other(e.to_string());
                }
                if matches!(sm.get_chunk_type(&expected_offset), Some(ChunkType::Data)) {
                    DataSyncWriteOutcome::Stored
                } else {
                    DataSyncWriteOutcome::Other(
                        "chunk not Data after force_sync_pending_chunks".into(),
                    )
                }
            } else if matches!(
                sm.collect_data_root_infos(unpacked.data_root),
                Ok(infos) if infos.0.is_empty()
            ) {
                // Defensive: empty infos should have been DataRootNotFound Err.
                DataSyncWriteOutcome::MissingDataRootIndex
            } else {
                DataSyncWriteOutcome::NoWriteableOffset
            }
        }
    }
}

fn try_send_chunk_to_ingress(service_senders: &ServiceSenders, unpacked: UnpackedChunk) {
    if let Err(e) = service_senders
        .chunk_ingress
        .send_traced(crate::chunk_ingress_service::ChunkIngressMessage::IngestChunk(unpacked, None))
    {
        warn!(
            error = %e,
            "Failed to send ChunkIngressMessage to chunk ingress channel after data_sync write failure"
        );
    }
}

pub struct DataSyncService {
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<Traced<DataSyncServiceMessage>>,
    pub inner: DataSyncServiceInner,
}

type StorageModuleId = usize;

pub struct DataSyncServiceInner {
    pub block_tree: BlockTreeReadGuard,
    pub storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
    pub active_peer_bandwidth_managers: Arc<RwLock<HashMap<IrysAddress, PeerBandwidthManager>>>,
    pub chunk_orchestrators: HashMap<StorageModuleId, ChunkOrchestrator>,
    pub peer_list: PeerList,
    pub chunk_fetcher_factory: ChunkFetcherFactory,
    pub service_senders: ServiceSenders,
    pub config: Config,
    pub runtime_handle: tokio::runtime::Handle,
    /// SM ids whose path-hash index heal was last confirmed clean by
    /// `SyncPartitions`. Re-armed on the slower re-arm cadence (see
    /// [`REARM_EVERY_N_TICKS`]) until the set is replaced by a later
    /// `SyncPartitions` (heal regressed or re-confirmed) or an SM in it
    /// re-blocks a write on `MissingDataRootIndex` (index regressed).
    healed_sm_ids: std::collections::HashSet<usize>,
    /// Dispatch-tick counter, used to run re-arm every [`REARM_EVERY_N_TICKS`]
    /// ticks instead of on every dispatch tick.
    rearm_tick: u64,
}

/// Re-arm the healed-SM Blocked backlog once every N dispatch ticks. Dispatch
/// runs on a 250ms interval (latency-sensitive), but draining a Blocked backlog
/// only needs steady progress, so re-arm runs ~1s (4 × 250ms) apart to avoid the
/// per-tick scan cost without slowing dispatch.
const REARM_EVERY_N_TICKS: u64 = 4;

pub enum DataSyncServiceMessage {
    /// Refresh peer/orchestrator membership for current ledger-assigned SMs.
    ///
    /// `unblock_sm_ids` lists the SM ids whose path-hash index was confirmed
    /// gap-free this heal pass; each such orchestrator re-queues its
    /// [`ChunkBlockReason::MissingDataRootIndex`] offsets (capped by free pending
    /// budget). An empty vec unblocks nobody. Per-SM confirmation (a
    /// post-migration re-verify) prevents one still-broken module from either
    /// suppressing unblock for healthy modules or being mass-unblocked into an
    /// epoch refetch/re-block thrash.
    SyncPartitions {
        unblock_sm_ids: Vec<usize>,
    },
    ChunkCompleted {
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_address: IrysAddress,
        chunk: PackedChunk,
    },
    ChunkFailed {
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
    },
    ChunkTimedOut {
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_address: IrysAddress,
    },
    PeerListUpdated,
    PeerDisconnected {
        peer_address: IrysAddress,
    },
    GetActivePeersList(oneshot::Sender<Arc<RwLock<HashMap<IrysAddress, PeerBandwidthManager>>>>),
}

impl DataSyncServiceInner {
    pub fn new(
        block_tree: BlockTreeReadGuard,
        storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
        peer_list: PeerList,
        chunk_fetcher_factory: ChunkFetcherFactory,
        service_senders: ServiceSenders,
        config: Config,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        let mut data_sync = Self {
            block_tree,
            storage_modules,
            peer_list,
            active_peer_bandwidth_managers: Default::default(),
            chunk_fetcher_factory,
            chunk_orchestrators: Default::default(),
            service_senders,
            config,
            runtime_handle,
            healed_sm_ids: Default::default(),
            rearm_tick: 0,
        };
        data_sync.synchronize_peers_and_orchestrators();
        data_sync
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub fn handle_message(&mut self, msg: DataSyncServiceMessage) -> eyre::Result<()> {
        match msg {
            DataSyncServiceMessage::SyncPartitions { unblock_sm_ids } => {
                self.synchronize_peers_and_orchestrators();
                if !unblock_sm_ids.is_empty() {
                    self.unblock_missing_data_root_indexes(&unblock_sm_ids);
                }
                // Replace (not union) the healed set: an SM that dropped out of
                // this heal pass's clean set must stop being re-armed by the
                // tick loop, and an empty vec clears all re-arming.
                self.healed_sm_ids = unblock_sm_ids.iter().copied().collect();
            }
            DataSyncServiceMessage::ChunkCompleted {
                storage_module_id,
                chunk_offset,
                peer_address: peer_addr,
                chunk,
            } => {
                // Fetch succeeded — record that separately from durable store.
                metrics::record_data_sync_chunk_fetched();
                if let Err(e) =
                    self.on_chunk_completed(storage_module_id, chunk_offset, peer_addr, chunk)
                {
                    error!(
                        storage_module.id = storage_module_id,
                        chunk.offset = %chunk_offset,
                        peer.address = %peer_addr,
                        "Failed to handle chunk completion: {e:?}"
                    );
                }
            }
            DataSyncServiceMessage::ChunkFailed {
                storage_module_id,
                chunk_offset,
                peer_addr,
            } => {
                metrics::record_data_sync_chunk_failure();
                if let Err(e) = self.on_chunk_failed(storage_module_id, chunk_offset, peer_addr) {
                    error!(
                        "Failed to handle chunk failure for storage_module {} chunk_offset {} from peer {}: {e:?}",
                        storage_module_id, chunk_offset, peer_addr
                    );
                }
            }
            DataSyncServiceMessage::ChunkTimedOut {
                storage_module_id,
                chunk_offset,
                peer_address: peer_addr,
            } => {
                metrics::record_data_sync_chunk_failure();
                if let Err(e) = self.on_chunk_timeout(storage_module_id, chunk_offset, peer_addr) {
                    error!(
                        "Failed to handle chunk timeout for storage_module {} chunk_offset {} from peer {}: {e:?}",
                        storage_module_id, chunk_offset, peer_addr
                    );
                }
            }
            DataSyncServiceMessage::PeerListUpdated => self.handle_peer_list_updated(),
            DataSyncServiceMessage::PeerDisconnected {
                peer_address: peer_addr,
            } => self.handle_peer_disconnection(peer_addr),
            DataSyncServiceMessage::GetActivePeersList(tx) => self.handle_get_active_peers_list(tx),
        }
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub fn tick(&mut self) -> eyre::Result<()> {
        for orchestrator in self.chunk_orchestrators.values_mut() {
            orchestrator.tick()?;
        }
        self.optimize_peer_concurrency();
        // Re-arm on a slower cadence than dispatch to keep dispatch latency-tight
        // while still draining Blocked backlogs (see REARM_EVERY_N_TICKS).
        self.rearm_tick = self.rearm_tick.wrapping_add(1);
        if self.rearm_tick.is_multiple_of(REARM_EVERY_N_TICKS) {
            self.rearm_healed_storage_modules();
        }
        Ok(())
    }

    /// Re-queue offsets blocked on missing data_root indexes for the
    /// confirmed-clean SMs in `sm_ids`, capped per orchestrator at free
    /// pending-budget slots so one heal cannot flood dispatch. Orchestrators
    /// whose SM id is not in `sm_ids` are left untouched (still Blocked).
    fn unblock_missing_data_root_indexes(&mut self, sm_ids: &[usize]) {
        let sm_ids: std::collections::HashSet<usize> = sm_ids.iter().copied().collect();
        let total = self.rearm_missing_data_root_indexes(|id| sm_ids.contains(id));
        if total > 0 {
            metrics::record_data_sync_chunk_unblocked(total as u64);
        }
    }

    /// Continuous re-arm: every tick, re-queue offsets blocked on missing
    /// data_root indexes for SMs whose heal is still considered clean
    /// (`healed_sm_ids`), capped per orchestrator at free pending-budget
    /// slots. This drains a large Blocked backlog across many ticks instead
    /// of relying on a single capped pulse from `SyncPartitions`, which is a
    /// no-op once dispatch is peer-starved (Pending already at cap).
    // Runs on the slower re-arm cadence (every REARM_EVERY_N_TICKS ticks), so it
    // no longer scans on every dispatch tick.
    // FOLLOWUP: still scans each healed orchestrator's requests even when nothing
    // is Blocked. For an O(1) skip, track a per-orchestrator
    // Blocked(MissingDataRootIndex) count (inc on mark_chunk_blocked, dec on
    // unblock and on the populate drop) and skip orchestrators whose count is 0.
    fn rearm_healed_storage_modules(&mut self) {
        if self.healed_sm_ids.is_empty() {
            return;
        }
        let healed = self.healed_sm_ids.clone();
        let total = self.rearm_missing_data_root_indexes(|id| healed.contains(id));
        if total > 0 {
            metrics::record_data_sync_chunk_unblocked(total as u64);
        }
    }

    /// Shared cap math for re-queuing `MissingDataRootIndex`-blocked offsets:
    /// for each orchestrator whose id satisfies `should_unblock`, unblock up
    /// to its free pending-budget slots. Returns the total offsets unblocked
    /// across all matching orchestrators (metric recording is the caller's
    /// responsibility, since callers differ in when they consider 0 notable).
    fn rearm_missing_data_root_indexes(
        &mut self,
        should_unblock: impl Fn(&usize) -> bool,
    ) -> usize {
        let max_pending = self.config.node_config.data_sync.max_pending_chunk_requests as usize;
        let mut total = 0_usize;
        for (id, orchestrator) in self.chunk_orchestrators.iter_mut() {
            if !should_unblock(id) {
                continue;
            }
            let pending = orchestrator
                .chunk_requests
                .values()
                .filter(|r| matches!(r.request_state, ChunkRequestState::Pending))
                .count();
            let free_slots = max_pending.saturating_sub(pending);
            let unblocked = orchestrator.unblock_missing_data_root_index(free_slots);
            if unblocked > 0 {
                debug!(
                    storage_module.id = id,
                    data_sync.unblocked = unblocked,
                    data_sync.unblock_cap = free_slots,
                    "re-queued offsets blocked on missing data_root index"
                );
                total += unblocked;
            }
        }
        total
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn optimize_peer_concurrency(&mut self) {
        // Get a write lock on the peer bandwidth managers list
        let Ok(mut peers) = self.active_peer_bandwidth_managers.write() else {
            return;
        };

        // Build a list of score tuples for the peer bandwidth managers (Address, health_score, active_requests, max_concurrency)
        let mut peer_scores: Vec<_> = peers
            .iter()
            .map(|(&addr, pm)| {
                (
                    addr,
                    pm.health_score(),
                    pm.active_requests(),
                    pm.max_concurrency(),
                )
            })
            .collect();

        peer_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        for (peer_addr, health_score, active_requests, current_max) in peer_scores {
            // Only optimize healthy peers
            if health_score < 0.7 {
                continue;
            }

            // Calculate utilization ratio of max concurrency and active requests for the peer
            let utilization_ratio = if current_max > 0 {
                active_requests as f32 / current_max as f32
            } else {
                0.0
            };

            // Only increase concurrency if peer is highly utilized
            if utilization_ratio >= 0.8 {
                if let Some(peer_manager) = peers.get_mut(&peer_addr) {
                    // Better performing peers get bigger increases
                    let increase = if health_score >= 0.9 {
                        5 // Excellent peer, trust it with more
                    } else if health_score >= 0.7 {
                        3 // Good peer, moderate increase
                    } else {
                        1 // Decent peer, conservative increase
                    };
                    debug!(
                        "Increasing max concurrency from {} to {} for peer {} (utilization: {:.1}%, health: {:.2})",
                        current_max,
                        current_max + increase,
                        peer_addr,
                        utilization_ratio * 100.0,
                        health_score
                    );
                    peer_manager.set_max_concurrency(current_max + increase);
                }
            } else {
                // debug!(
                //     "Not increasing concurrency for peer {} max_concurrency {} (concurrent utilization: {:.1}%, health: {:.2})",
                //     peer_addr,
                //     current_max,
                //     utilization_ratio * 100.0,
                //     health_score
                // );
            }
        }
    }

    #[tracing::instrument(level = "trace", skip_all, fields(
        chunk.storage_module_id = storage_module_id,
        chunk.offset = %chunk_offset,
        peer.address = %peer_addr,
        chunk.data_root = %chunk.data_root,
        chunk.partition_hash = %chunk.partition_hash,
    ))]
    fn on_chunk_completed(
        &mut self,
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
        chunk: PackedChunk,
    ) -> eyre::Result<()> {
        // Peer delivery success: credit bandwidth stats, leave Requested until write outcome.
        if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
            orchestrator.on_chunk_fetched(chunk_offset, peer_addr)?;
        }

        let consensus = &self.config.consensus;
        let unpacked_chunk = unpack(
            &chunk,
            consensus.entropy_packing_iterations,
            consensus.chunk_size as usize,
            consensus.chain_id,
        );

        let sm = self
            .storage_modules
            .read()
            .unwrap()
            .get(storage_module_id)
            .ok_or_else(|| eyre::eyre!("storage_module_id {storage_module_id} not found"))?
            .clone();

        let pa = sm.partition_assignment();
        let ledger_id = pa.and_then(|p| p.ledger_id);
        let slot_index = pa.and_then(|p| p.slot_index);
        let partition_hash = pa.map(|p| p.partition_hash);

        let write_outcome = attempt_data_sync_write(&sm, &unpacked_chunk, chunk_offset);

        match write_outcome {
            DataSyncWriteOutcome::Stored => {
                metrics::record_data_sync_chunk_stored();
                if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
                    orchestrator.mark_chunk_stored(chunk_offset)?;
                }
                debug!(
                    storage_module.id = storage_module_id,
                    chunk.offset = %chunk_offset,
                    chunk.data_root = %unpacked_chunk.data_root,
                    ?ledger_id,
                    ?slot_index,
                    ?partition_hash,
                    peer.address = %peer_addr,
                    "data_sync chunk stored"
                );
            }
            DataSyncWriteOutcome::MissingDataRootIndex => {
                let reason = ChunkBlockReason::MissingDataRootIndex.as_metric_label();
                metrics::record_data_sync_chunk_write_failed(reason);
                metrics::record_data_sync_chunk_blocked(reason);
                warn!(
                    storage_module.id = storage_module_id,
                    chunk.offset = %chunk_offset,
                    chunk.data_root = %unpacked_chunk.data_root,
                    chunk.tx_offset = %unpacked_chunk.tx_offset,
                    ?ledger_id,
                    ?slot_index,
                    ?partition_hash,
                    peer.address = %peer_addr,
                    reason,
                    "data_sync write blocked: data_root not indexed in storage module; \
                     stopping hot re-fetch until SyncPartitions after a successful index heal"
                );
                if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
                    orchestrator
                        .mark_chunk_blocked(chunk_offset, ChunkBlockReason::MissingDataRootIndex)?;
                }
                // Index regressed for this SM after a prior heal marked it clean —
                // stop the per-tick re-arm for it until the next heal re-confirms it.
                // FOLLOWUP: one re-block drops the whole SM, freezing drain of its
                // other still-healthy Blocked offsets until the next heal pass.
                // Consider an N-reblock threshold or one more requeue before drop,
                // trading a little anti-thrash strictness for faster backlog drain.
                self.healed_sm_ids.remove(&storage_module_id);
                // Best-effort: mempool/ingress may still place the chunk if another
                // path holds indexes; do not treat handoff as durable success.
                try_send_chunk_to_ingress(&self.service_senders, unpacked_chunk);
            }
            DataSyncWriteOutcome::NoWriteableOffset => {
                metrics::record_data_sync_chunk_write_failed("no_writeable_offset");
                warn!(
                    storage_module.id = storage_module_id,
                    chunk.offset = %chunk_offset,
                    chunk.data_root = %unpacked_chunk.data_root,
                    chunk.tx_offset = %unpacked_chunk.tx_offset,
                    ?ledger_id,
                    ?slot_index,
                    ?partition_hash,
                    peer.address = %peer_addr,
                    reason = "no_writeable_offset",
                    "data_sync write had no Entropy target at expected offsets; re-queueing"
                );
                if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
                    orchestrator.requeue_after_local_write_failure(chunk_offset)?;
                }
                try_send_chunk_to_ingress(&self.service_senders, unpacked_chunk);
            }
            DataSyncWriteOutcome::Other(err) => {
                metrics::record_data_sync_chunk_write_failed("other");
                warn!(
                    storage_module.id = storage_module_id,
                    chunk.offset = %chunk_offset,
                    chunk.data_root = %unpacked_chunk.data_root,
                    chunk.tx_offset = %unpacked_chunk.tx_offset,
                    ?ledger_id,
                    ?slot_index,
                    ?partition_hash,
                    peer.address = %peer_addr,
                    reason = "other",
                    error = %err,
                    "data_sync write failed; re-queueing and forwarding to chunk ingress"
                );
                if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
                    orchestrator.requeue_after_local_write_failure(chunk_offset)?;
                }
                try_send_chunk_to_ingress(&self.service_senders, unpacked_chunk);
            }
        }

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    fn on_chunk_failed(
        &mut self,
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
    ) -> eyre::Result<()> {
        if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
            orchestrator.on_chunk_failed(chunk_offset, peer_addr)?;

            let pa = orchestrator
                .storage_module
                .partition_assignment()
                .expect("A partition assignment present");
            debug!(
                "chunk failed: ledger:{:?}, slot_index:{:?} chunk_offset:{} peer:{}",
                pa.ledger_id, pa.slot_index, chunk_offset, peer_addr
            );
        }
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    fn on_chunk_timeout(
        &mut self,
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
    ) -> eyre::Result<()> {
        // TODO: Opportunity to do custom timeout tracking/handling here
        debug!("chunk timed out: {} peer:{}", chunk_offset, peer_addr);
        if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
            orchestrator.on_chunk_failed(chunk_offset, peer_addr)?;
        }
        Ok(())
    }

    fn handle_peer_list_updated(&mut self) {
        self.sync_peer_partition_assignments();
    }

    fn handle_peer_disconnection(&mut self, peer_addr: IrysAddress) {
        // Remove peer from all orchestrators
        for orchestrator in self.chunk_orchestrators.values_mut() {
            orchestrator.remove_peer(peer_addr);
        }

        // Remove from peer list
        self.active_peer_bandwidth_managers
            .write()
            .unwrap()
            .remove(&peer_addr);
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn handle_get_active_peers_list(
        &self,
        tx: oneshot::Sender<Arc<RwLock<HashMap<IrysAddress, PeerBandwidthManager>>>>,
    ) {
        if let Err(e) = tx.send(self.active_peer_bandwidth_managers.clone()) {
            tracing::error!("handle_get_active_peers_list() tx.send() error: {:?}", e);
        };
    }

    fn synchronize_peers_and_orchestrators(&mut self) {
        self.sync_peer_partition_assignments();
        self.create_chunk_orchestrators();
        self.update_orchestrator_peers();
        metrics::record_data_sync_active_peers(
            self.active_peer_bandwidth_managers.read().unwrap().len() as u64,
        );
    }

    /// Synchronizes peer bandwidth managers with current network peers and local
    /// storage module assignments.
    ///
    /// For each local storage module assigned to a data ledger slot:
    /// - Checks if the module has entropy chunks requiring data
    /// - Ensures PeerBandwidthManagers exist for all peers storing relevant partition data
    ///
    /// This maintains an up-to-date mapping between peers and bandwidth managers
    /// for efficient chunk downloading across the network.
    #[tracing::instrument(level = "trace", skip_all)]
    fn sync_peer_partition_assignments(&mut self) {
        let storage_modules = self.storage_modules.read().unwrap().clone();

        // Loop though all the storage modules managed by the local node
        for storage_module in storage_modules {
            // Skip any storage modules not assigned to a data ledger
            let Some(pa) = *storage_module.partition_assignment.read().unwrap() else {
                continue;
            };

            let Some(ledger_id) = pa.ledger_id else {
                continue;
            };

            let Some(slot_index) = pa.slot_index else {
                continue;
            };

            // Check to see if the storage module has any entropy (packed) chunks that need data
            let entropy_intervals = storage_module.get_intervals(ChunkType::Entropy);
            if entropy_intervals.is_empty() {
                debug!("StorageModule has no entropy chunks\n{:?}", pa);
                continue;
            }

            // If it does, ensure there's a bandwidth manager for any peer storing the data for this storage module
            self.ensure_bandwidth_managers_for_peers(ledger_id, slot_index);
        }
    }

    /// Updates the active_peers list and ensures there are PeerBandwidthManagers for
    /// any peers assigned to store the same slot data.
    #[tracing::instrument(level = "trace", skip_all)]
    fn ensure_bandwidth_managers_for_peers(&mut self, ledger_id: u32, slot_index: usize) {
        // Get the slot assignments for all partition hashes in this slot
        let epoch_snapshot = self.block_tree.read().canonical_epoch_snapshot();
        let slot_assignments: Vec<_> = epoch_snapshot
            .partition_assignments
            .data_partitions
            .values()
            .filter(|a| a.ledger_id == Some(ledger_id) && a.slot_index == Some(slot_index))
            .copied()
            .collect();

        // Loop though all of this slots assigned partition_hashes
        for pa in slot_assignments {
            // Use the mining address in the assignment to retrieve a peer from the global peer_list
            let Some(peer) = self.peer_list.peer_by_mining_address(&pa.miner_address) else {
                continue;
            };

            // Get existing entry for a peer bandwidth manager or add a new one for the peer
            let mut active_peers = self.active_peer_bandwidth_managers.write().unwrap();
            let entry = active_peers
                .entry(pa.miner_address)
                .or_insert(PeerBandwidthManager::new(
                    &pa.miner_address,
                    &peer,
                    &self.config,
                ));

            // Finally let the peer bandwidth manager for this peer store a reference to this partition assignment
            // so we can filter the active_peer_bandwidth_managers list for peers assigned to this ledger/slot in the future
            if !entry.partition_assignments.contains(&pa) {
                debug!(
                    "Adding partition assignment: {:#?} to Peer: {}",
                    pa, entry.miner_address
                );
                entry.partition_assignments.push(pa);
            }
            // active_peers dropped here
        }
    }

    fn create_chunk_orchestrators(&mut self) {
        // Clone the storage modules list to avoid holding the read lock during iteration
        // This is lightweight since we're cloning Arc references, not the actual modules
        let storage_modules: Vec<Arc<StorageModule>> = {
            self.storage_modules
                .read()
                .unwrap()
                .iter()
                .cloned()
                .collect()
        };

        // Drop orchestrators for modules that no longer hold a data ledger assignment
        self.chunk_orchestrators.retain(|sm_id, _| {
            storage_modules
                .get(*sm_id)
                .and_then(|sm| sm.partition_assignment())
                .and_then(|pa| pa.ledger_id)
                .is_some()
        });

        for sm in storage_modules {
            let sm_id = sm.id;

            // Skip if we already have a chunk orchestrator for this storage module
            if self.chunk_orchestrators.contains_key(&sm_id) {
                continue;
            }

            // Skip unused storage modules without partition assignments (not yet capacity or data)
            let Some(pa) = sm.partition_assignment() else {
                continue;
            };

            // Skip capacity partitions - they store entropy, not data chunks that need syncing
            if pa.ledger_id.is_none() {
                continue;
            }

            // Use the factory to create a chunk_fetcher (allows mock chunk fetchers for testing)
            let chunk_fetcher = (self.chunk_fetcher_factory)(pa.ledger_id.unwrap());

            // Create a chunk orchestrator for storage modules that needs to sync data
            let orchestrator = ChunkOrchestrator::new(
                sm.clone(),
                self.active_peer_bandwidth_managers.clone(),
                self.block_tree.clone(),
                &self.service_senders,
                chunk_fetcher,
                self.config.node_config.clone(),
                self.runtime_handle.clone(),
            );

            self.chunk_orchestrators.insert(sm_id, orchestrator);
        }
    }

    fn update_orchestrator_peers(&mut self) {
        let storage_modules = self.storage_modules.read().unwrap().clone();

        // Collect storage_module IDs first to avoid borrowing conflicts
        let sm_ids: Vec<StorageModuleId> = self.chunk_orchestrators.keys().copied().collect();

        // Get a list of the best peers (by mining address) for each storage module
        let mut peer_updates: Vec<(StorageModuleId, Vec<IrysAddress>)> = Vec::new();

        for sm_id in sm_ids {
            let Some(storage_module) = storage_modules.get(sm_id) else {
                continue;
            };

            let best_peers = self.get_best_available_peers(storage_module, 4);
            peer_updates.push((sm_id, best_peers));
        }

        // Add the peers to the orchestrators
        for (sm_id, best_peers) in peer_updates {
            // Skip ff we don't have an orchestrator for this storage_module
            let Some(orchestrator) = self.chunk_orchestrators.get_mut(&sm_id) else {
                warn!(
                    "Storage module with id: {sm_id} does not have a chunk_orchestrator and it should."
                );
                continue;
            };

            // Skip the add_peer() orchestrator fn and update current_peers directly
            orchestrator.current_peers = best_peers.clone();
        }
    }

    pub fn get_best_available_peers(
        &self,
        storage_module: &StorageModule,
        desired_count: usize,
    ) -> Vec<IrysAddress> {
        // Only return peers for storage modules that have active chunk orchestrators
        // This ensures we don't waste time finding peers for modules that aren't syncing
        if !self.chunk_orchestrators.contains_key(&storage_module.id) {
            return Vec::new();
        }

        // Extract partition assignment
        let pa = storage_module.partition_assignment().unwrap();

        // Check to see that the partition hash hasn't been re-assigned to capacity and no longer
        // has any data to sync
        if pa.ledger_id.is_none() {
            // We don't remove the orchestrator for the partition because it's not hurting anything to keep it around...
            return Vec::new();
        }

        let ledger_id = pa.ledger_id.unwrap();

        // Find all peers that are assigned to store data for the same ledger slot
        let active_peers = self.active_peer_bandwidth_managers.read().unwrap();
        let mut candidates: Vec<&PeerBandwidthManager> = active_peers
            .values()
            .filter(|peer_manager| {
                peer_manager.partition_assignments.iter().any(|assignment| {
                    assignment.ledger_id == Some(ledger_id)
                        && assignment.slot_index == pa.slot_index
                })
            })
            .collect();

        // Prioritize healthy peers with available bandwidth capacity
        // Primary sort: health score (reliability, recent performance)
        // Secondary sort: available concurrency (current capacity to handle more requests)
        candidates.sort_by(|a, b| {
            (b.health_score(), b.available_concurrency())
                .partial_cmp(&(a.health_score(), a.available_concurrency()))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Return the top performers up to the desired count
        candidates
            .into_iter()
            .take(desired_count)
            .map(|peer_manager| peer_manager.miner_address)
            .collect()
    }
}

impl DataSyncService {
    pub fn spawn_service(
        rx: UnboundedReceiver<Traced<DataSyncServiceMessage>>,
        block_tree: BlockTreeReadGuard,
        storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
        peer_list: PeerList,
        chunk_fetcher_factory: ChunkFetcherFactory,
        service_senders: &ServiceSenders,
        config: &Config,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        let config = config.clone();
        let service_senders = service_senders.clone();
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
        let runtime_handle_clone = runtime_handle.clone();

        let handle = runtime_handle.spawn(
            async move {
                let data_sync_service = Self {
                    shutdown: shutdown_rx,
                    msg_rx: rx,
                    inner: DataSyncServiceInner::new(
                        block_tree,
                        storage_modules,
                        peer_list,
                        chunk_fetcher_factory,
                        service_senders,
                        config,
                        runtime_handle_clone,
                    ),
                };
                data_sync_service
                    .start()
                    .await
                    .expect("DataSync Service encountered an irrecoverable error")
            }
            .instrument(tracing::Span::current()),
        );

        TokioServiceHandle {
            name: "data_sync_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    #[tracing::instrument(name = "data_sync_service_start", level = "trace", skip_all)]
    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting DataSync Service");

        let mut interval = tokio::time::interval(Duration::from_millis(250));
        interval.tick().await; // Skip first immediate tick

        // Subscribe to peer lifecycle events for event-driven synchronization
        let mut peer_events_rx = self.inner.peer_list.subscribe_to_peer_events();

        loop {
            tokio::select! {
                biased;

                _ = &mut self.shutdown => {
                    tracing::info!("Shutdown signal received for DataSync Service");
                    break;
                }

                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(traced) => {
                            let (msg, _entered) = traced.into_inner();
                            self.inner.handle_message(msg)?;
                        }
                        None => {
                            tracing::warn!("Message channel closed unexpectedly");
                            break;
                        }
                    }
                }

                _ = interval.tick() => {
                    if let Err(e) = self.inner.tick() {
                        tracing::error!("Error during tick: {}", e);
                        break;
                    }
                }

                evt = peer_events_rx.recv() => {
                    match evt {
                        Ok(irys_domain::PeerEvent::BecameActive { .. }) => {
                            // New active peer available; resync orchestrators/managers
                            self.inner.synchronize_peers_and_orchestrators();
                        }
                        Ok(irys_domain::PeerEvent::BecameInactive { mining_addr, .. }) => {
                            // Peer no longer active; resync
                            debug!("Peer became inactive: {}", mining_addr);
                            self.inner.synchronize_peers_and_orchestrators();
                        }
                        Ok(irys_domain::PeerEvent::PeerUpdated { .. }) => {
                            // Metadata changed; just refresh orchestrator peer sets
                            self.inner.update_orchestrator_peers();
                        }
                        Ok(irys_domain::PeerEvent::PeerRemoved { mining_addr, .. }) => {
                            // Treat same as disconnect
                            debug!("Peer removed: {}", mining_addr);
                            self.inner.handle_peer_disconnection(mining_addr);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // Missed events; do a conservative resync
                            self.inner.synchronize_peers_and_orchestrators();
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            warn!("peer events channel closed in DataSyncService; resubscribing");
                        }
                    }
                }
            }
        }

        // Process remaining messages before shutdown
        while let Ok(traced) = self.msg_rx.try_recv() {
            let (msg, _entered) = traced.into_inner();
            self.inner.handle_message(msg)?;
        }

        tracing::info!("shutting down DataSync Service gracefully");
        Ok(())
    }
}

#[cfg(test)]
mod write_outcome_tests {
    use super::{DataSyncWriteOutcome, attempt_data_sync_write};
    use irys_domain::{StorageModule, StorageModuleInfo, WriteDataChunkError};
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{
        Config, ConsensusConfig, DataLedger, H256, IrysAddress, NodeConfig, PartitionChunkOffset,
        TxChunkOffset, UnpackedChunk, partition::PartitionAssignment, partition_chunk_offset_ie,
    };
    use std::sync::Arc;

    /// Unindexed data_root must classify as MissingDataRootIndex (not Other/requeue thrash).
    #[test]
    fn unindexed_data_root_classifies_as_missing_index() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let num_chunks = 4_u64;
        let chunk_size = 32_u64;
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: num_chunks,
                num_chunks_in_recall_range: 2,
                num_partitions_per_slot: 1,
                entropy_packing_iterations: 1,
                chain_id: 1,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);

        let pa = PartitionAssignment {
            ledger_id: Some(DataLedger::Publish.into()),
            slot_index: Some(0),
            miner_address: IrysAddress::from([7_u8; 20]),
            partition_hash: H256::random(),
        };
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(pa),
            submodules: vec![(
                partition_chunk_offset_ie!(0, num_chunks as u32),
                "chunks".into(),
            )],
        };
        let sm = Arc::new(StorageModule::new(&info, &config).expect("storage module"));
        // Packed entropy, but deliberately no index_transaction_data — peer-4 hole case.
        sm.pack_with_zeros();

        let chunk = UnpackedChunk {
            data_root: H256::random(),
            data_size: chunk_size,
            data_path: vec![1, 2, 3, 4].into(),
            bytes: vec![0xcd; chunk_size as usize].into(),
            tx_offset: TxChunkOffset::from(0_u32),
        };

        let err = sm
            .write_data_chunk(&chunk)
            .expect_err("unindexed data_root must fail write");
        assert!(
            matches!(err, WriteDataChunkError::DataRootNotFound),
            "expected DataRootNotFound, got: {err:?}"
        );

        let outcome = attempt_data_sync_write(&sm, &chunk, PartitionChunkOffset::from(0_u32));
        assert_eq!(outcome, DataSyncWriteOutcome::MissingDataRootIndex);
    }
}

#[cfg(test)]
mod unblock_filter_tests {
    use super::*;
    use crate::chunk_fetcher::{ChunkFetcher, MockChunkFetcher};
    use crate::chunk_orchestrator::ChunkRequest;
    use crate::test_helpers::build_test_service_senders;
    use irys_domain::{BlockTree, StorageModuleInfo};
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{
        ConsensusConfig, DataLedger, H256, NodeConfig, TxChunkOffset,
        partition::PartitionAssignment, partition_chunk_offset_ie,
    };

    fn test_config(base_directory: std::path::PathBuf) -> Config {
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 4,
                num_chunks_in_recall_range: 2,
                num_partitions_per_slot: 1,
                entropy_packing_iterations: 1,
                block_migration_depth: 1,
                chain_id: 1,
                ..ConsensusConfig::testing()
            }),
            base_directory,
            ..NodeConfig::testing()
        };
        Config::new_with_random_peer_id(node_config)
    }

    fn ledger_sm(config: &Config, id: usize, dir: &str) -> Arc<StorageModule> {
        let pa = PartitionAssignment {
            ledger_id: Some(DataLedger::Publish.into()),
            slot_index: Some(0),
            miner_address: IrysAddress::from([1_u8; 20]),
            partition_hash: H256::random(),
        };
        let info = StorageModuleInfo {
            id,
            partition_assignment: Some(pa),
            submodules: vec![(partition_chunk_offset_ie!(0, 4), dir.into())],
        };
        Arc::new(StorageModule::new(&info, config).expect("storage module"))
    }

    /// `unblock_missing_data_root_indexes(&[id])` must re-queue only the named
    /// orchestrator's `MissingDataRootIndex` offsets and leave the others Blocked.
    #[test_log::test(tokio::test)]
    async fn unblock_targets_only_named_sm_ids() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let config = test_config(tmp.path().to_path_buf());

        let sm0 = ledger_sm(&config, 0, "chunks0");
        let sm1 = ledger_sm(&config, 1, "chunks1");
        let storage_modules = Arc::new(RwLock::new(vec![sm0, sm1]));

        let genesis = irys_testing_utils::new_mock_signed_header();
        let block_tree = BlockTree::new(&genesis, config.consensus.clone());
        let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));

        let (service_senders, _receivers) = build_test_service_senders();
        let peer_list = PeerList::from_peers(
            vec![],
            service_senders.peer_network.clone(),
            &config,
            tokio::sync::broadcast::channel::<irys_domain::PeerEvent>(100).0,
        )
        .expect("peer list");

        let factory: ChunkFetcherFactory = Box::new(|ledger_id| {
            Arc::new(MockChunkFetcher::new(ledger_id as usize)) as Arc<dyn ChunkFetcher>
        });

        let mut inner = DataSyncServiceInner::new(
            block_tree_guard,
            storage_modules,
            peer_list,
            factory,
            service_senders,
            config,
            tokio::runtime::Handle::current(),
        );

        // Both orchestrators should exist (one per ledger-assigned SM).
        assert!(inner.chunk_orchestrators.contains_key(&0));
        assert!(inner.chunk_orchestrators.contains_key(&1));

        // Seed each with a MissingDataRootIndex-blocked offset.
        for id in [0_usize, 1_usize] {
            let offset = PartitionChunkOffset::from(0_u32);
            let orch = inner.chunk_orchestrators.get_mut(&id).unwrap();
            orch.chunk_requests.insert(
                offset,
                ChunkRequest {
                    ledger_id: 0,
                    slot_index: 0,
                    chunk_offset: offset,
                    excluded: None,
                    request_state: ChunkRequestState::Blocked(
                        ChunkBlockReason::MissingDataRootIndex,
                    ),
                },
            );
        }

        // Unblock only SM 0.
        inner.unblock_missing_data_root_indexes(&[0]);

        let offset = PartitionChunkOffset::from(0_u32);
        assert_eq!(
            inner.chunk_orchestrators[&0].chunk_requests[&offset].request_state,
            ChunkRequestState::Pending,
            "SM 0 offset should be re-queued to Pending"
        );
        assert_eq!(
            inner.chunk_orchestrators[&1].chunk_requests[&offset].request_state,
            ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex),
            "SM 1 offset must stay Blocked (not in unblock set)"
        );
    }

    fn build_test_inner(
        config: Config,
        storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
    ) -> DataSyncServiceInner {
        let genesis = irys_testing_utils::new_mock_signed_header();
        let block_tree = BlockTree::new(&genesis, config.consensus.clone());
        let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));

        let (service_senders, _receivers) = build_test_service_senders();
        let peer_list = PeerList::from_peers(
            vec![],
            service_senders.peer_network.clone(),
            &config,
            tokio::sync::broadcast::channel::<irys_domain::PeerEvent>(100).0,
        )
        .expect("peer list");

        let factory: ChunkFetcherFactory = Box::new(|ledger_id| {
            Arc::new(MockChunkFetcher::new(ledger_id as usize)) as Arc<dyn ChunkFetcher>
        });

        DataSyncServiceInner::new(
            block_tree_guard,
            storage_modules,
            peer_list,
            factory,
            service_senders,
            config,
            tokio::runtime::Handle::current(),
        )
    }

    /// The tick-driven re-arm must drain a Blocked backlog larger than a single
    /// pass's free-slot cap across multiple ticks (as Pending drains between
    /// them), instead of stalling after one capped pulse.
    #[test_log::test(tokio::test)]
    async fn rearm_healed_storage_modules_drains_backlog_across_ticks() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 8,
                num_chunks_in_recall_range: 2,
                num_partitions_per_slot: 1,
                entropy_packing_iterations: 1,
                block_migration_depth: 1,
                chain_id: 1,
                ..ConsensusConfig::testing()
            }),
            data_sync: irys_types::DataSyncServiceConfig {
                max_pending_chunk_requests: 2,
                ..Default::default()
            },
            base_directory: tmp.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let pa = PartitionAssignment {
            ledger_id: Some(DataLedger::Publish.into()),
            slot_index: Some(0),
            miner_address: IrysAddress::from([1_u8; 20]),
            partition_hash: H256::random(),
        };
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(pa),
            submodules: vec![(partition_chunk_offset_ie!(0, 8), "chunks_backlog".into())],
        };
        let sm0 = Arc::new(StorageModule::new(&info, &config).expect("storage module"));
        let storage_modules = Arc::new(RwLock::new(vec![sm0]));

        let mut inner = build_test_inner(config, storage_modules);
        assert!(inner.chunk_orchestrators.contains_key(&0));

        // Seed 5 Blocked offsets — more than the free-slot cap (2) per pass.
        let orch = inner.chunk_orchestrators.get_mut(&0).unwrap();
        for i in 0..5_u32 {
            let offset = PartitionChunkOffset::from(i);
            orch.chunk_requests.insert(
                offset,
                ChunkRequest {
                    ledger_id: 0,
                    slot_index: 0,
                    chunk_offset: offset,
                    excluded: None,
                    request_state: ChunkRequestState::Blocked(
                        ChunkBlockReason::MissingDataRootIndex,
                    ),
                },
            );
        }
        inner.healed_sm_ids.insert(0);

        fn blocked_count(inner: &DataSyncServiceInner) -> usize {
            inner.chunk_orchestrators[&0]
                .chunk_requests
                .values()
                .filter(|r| {
                    matches!(
                        r.request_state,
                        ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex)
                    )
                })
                .count()
        }

        assert_eq!(blocked_count(&inner), 5);

        let mut backlog_after_each_tick = Vec::new();
        for _ in 0..4 {
            inner.rearm_healed_storage_modules();

            // Simulate Pending draining (dispatched + completed/removed) before
            // the next tick, so free-slot budget is available again.
            let orch = inner.chunk_orchestrators.get_mut(&0).unwrap();
            let pending_offsets: Vec<_> = orch
                .chunk_requests
                .iter()
                .filter_map(|(&offset, r)| {
                    matches!(r.request_state, ChunkRequestState::Pending).then_some(offset)
                })
                .collect();
            for offset in pending_offsets {
                orch.chunk_requests.remove(&offset);
            }

            backlog_after_each_tick.push(blocked_count(&inner));
        }

        assert_eq!(
            backlog_after_each_tick,
            vec![3, 1, 0, 0],
            "backlog must drain monotonically to zero across ticks, not stall after one capped pulse"
        );
    }

    /// A prior heal marks SM 0 clean; a subsequent write on that SM re-blocks
    /// on `MissingDataRootIndex` (index regressed). The id must drop out of
    /// `healed_sm_ids` so the tick loop stops re-arming it.
    #[test_log::test(tokio::test)]
    async fn missing_data_root_index_reblock_clears_healed_sm_id() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let config = test_config(tmp.path().to_path_buf());
        let sm0 = ledger_sm(&config, 0, "chunks_reblock");
        let pa = sm0.partition_assignment().expect("partition assignment");
        sm0.pack_with_zeros();
        let storage_modules = Arc::new(RwLock::new(vec![sm0]));

        let mut inner = build_test_inner(config.clone(), storage_modules);

        // A heal pass confirms SM 0 clean.
        inner
            .handle_message(DataSyncServiceMessage::SyncPartitions {
                unblock_sm_ids: vec![0],
            })
            .expect("handle SyncPartitions");
        assert!(inner.healed_sm_ids.contains(&0));

        // Simulate an in-flight fetch for offset 0 from `peer`.
        let peer = IrysAddress::from([9_u8; 20]);
        let offset = PartitionChunkOffset::from(0_u32);
        inner
            .chunk_orchestrators
            .get_mut(&0)
            .unwrap()
            .chunk_requests
            .insert(
                offset,
                ChunkRequest {
                    ledger_id: 0,
                    slot_index: 0,
                    chunk_offset: offset,
                    excluded: None,
                    request_state: ChunkRequestState::Requested(peer, std::time::Instant::now()),
                },
            );

        // Packed chunk whose data_root was never indexed on SM 0 — the write
        // must fail with MissingDataRootIndex regardless of packing content.
        let chunk_size = config.consensus.chunk_size as usize;
        let mut entropy_chunk = Vec::with_capacity(chunk_size);
        irys_packing::capacity_single::compute_entropy_chunk(
            pa.miner_address,
            0_u64,
            pa.partition_hash.into(),
            config.consensus.entropy_packing_iterations,
            chunk_size,
            &mut entropy_chunk,
            config.consensus.chain_id,
        );
        let packed_bytes =
            irys_packing::packing_xor_vec_u8(entropy_chunk, &vec![0xcd_u8; chunk_size]);
        let packed_chunk = PackedChunk {
            data_root: H256::random(),
            data_size: chunk_size as u64,
            data_path: vec![1, 2, 3, 4].into(),
            bytes: packed_bytes.into(),
            packing_address: pa.miner_address,
            partition_offset: offset,
            tx_offset: TxChunkOffset::from(0_u32),
            partition_hash: pa.partition_hash,
        };

        inner
            .on_chunk_completed(0, offset, peer, packed_chunk)
            .expect("on_chunk_completed handles MissingDataRootIndex without erroring");

        assert!(
            !inner.healed_sm_ids.contains(&0),
            "index regressed for SM 0 must clear it from healed_sm_ids"
        );
    }
}
