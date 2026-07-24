pub mod chunk_fetcher;
pub mod chunk_orchestrator;
pub mod peer_bandwidth_manager;
pub mod peer_stats;

use crate::{chunk_fetcher::ChunkFetcherFactory, metrics, services::ServiceSenders};
use chunk_orchestrator::{ChunkBlockReason, ChunkOrchestrator, ChunkRequestState};
use irys_database::db::IrysDatabaseExt as _;
use irys_database::ingress_proofs_by_data_root;
use irys_domain::{BlockTreeReadGuard, ChunkType, PeerList, StorageModule, WriteDataChunkError};
use irys_packing::unpack;
use irys_types::{
    ChunkFormat, Config, DataRoot, IrysAddress, PartitionChunkOffset, SendTraced as _,
    TokioServiceHandle, Traced, UnpackedChunk, app_state::DatabaseProvider,
};
use peer_bandwidth_manager::PeerBandwidthManager;
use reth::tasks::shutdown::Shutdown;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
use tracing::{Instrument as _, debug, error, warn};

/// Cap online ingress-proof signers appended to `current_peers` per SM refresh.
/// Prefer assignees; these are residual-hole recovery hints only.
const MAX_INGRESS_PROOF_SIGNER_PEERS: usize = 2;
/// Max residual entropy offsets sampled when collecting data_roots for proof lookup.
const MAX_RESIDUAL_OFFSETS_FOR_PROOF_SCAN: usize = 16;

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

/// Resolve a storage module by its stable `id` field.
///
/// The `storage_modules` vec is **not** ordered by id — modules are inserted in
/// ledger-assignment processing order (Publish → Submit → OneYear → ThirtyDay →
/// Capacity). Using `vec.get(id)` therefore returns the wrong module whenever
/// `id != vec_index` (common for term-ledger SMs whose directory indices are
/// higher than their insertion rank). Always look up by `StorageModule::id`.
fn storage_module_by_id(
    storage_modules: &[Arc<StorageModule>],
    id: StorageModuleId,
) -> Option<Arc<StorageModule>> {
    storage_modules.iter().find(|sm| sm.id == id).cloned()
}

pub struct DataSyncServiceInner {
    pub block_tree: BlockTreeReadGuard,
    pub storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
    pub active_peer_bandwidth_managers: Arc<RwLock<HashMap<IrysAddress, PeerBandwidthManager>>>,
    pub chunk_orchestrators: HashMap<StorageModuleId, ChunkOrchestrator>,
    pub peer_list: PeerList,
    /// Main node DB — used to look up accepted ingress proofs for residual-hole
    /// peer expansion. Proof gossip is not chunk replication; signers are fetch hints.
    pub db: DatabaseProvider,
    pub chunk_fetcher_factory: ChunkFetcherFactory,
    pub service_senders: ServiceSenders,
    pub config: Config,
    pub runtime_handle: tokio::runtime::Handle,
}

pub enum DataSyncServiceMessage {
    /// Refresh peer/orchestrator membership for current ledger-assigned SMs.
    ///
    /// When `unblock_missing_data_root_index` is true, also re-queues
    /// [`ChunkBlockReason::MissingDataRootIndex`] offsets (capped by free pending
    /// budget). Only set that after index heal finished with zero issues (every
    /// SM plan Complete or fully migrated; no plan soft-skips, no migrate
    /// failures) — otherwise mass-unblock on a still-broken index becomes an
    /// epoch refetch/re-block thrash.
    SyncPartitions {
        unblock_missing_data_root_index: bool,
    },
    ChunkCompleted {
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_address: IrysAddress,
        /// Packed from assignee ledger-offset fetch, or Unpacked from cache-backed
        /// data_root fetch (ingress-proof signers).
        chunk: ChunkFormat,
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
        db: DatabaseProvider,
        chunk_fetcher_factory: ChunkFetcherFactory,
        service_senders: ServiceSenders,
        config: Config,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        let mut data_sync = Self {
            block_tree,
            storage_modules,
            peer_list,
            db,
            active_peer_bandwidth_managers: Default::default(),
            chunk_fetcher_factory,
            chunk_orchestrators: Default::default(),
            service_senders,
            config,
            runtime_handle,
        };
        data_sync.synchronize_peers_and_orchestrators();
        data_sync
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub fn handle_message(&mut self, msg: DataSyncServiceMessage) -> eyre::Result<()> {
        match msg {
            DataSyncServiceMessage::SyncPartitions {
                unblock_missing_data_root_index,
            } => {
                self.synchronize_peers_and_orchestrators();
                if unblock_missing_data_root_index {
                    self.unblock_missing_data_root_indexes();
                }
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
        Ok(())
    }

    /// Re-queue offsets blocked on missing data_root indexes, capped per
    /// orchestrator at free pending-budget slots so one heal cannot flood dispatch.
    fn unblock_missing_data_root_indexes(&mut self) {
        let max_pending = self.config.node_config.data_sync.max_pending_chunk_requests as usize;
        let mut total = 0_usize;
        for (id, orchestrator) in self.chunk_orchestrators.iter_mut() {
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
        if total > 0 {
            metrics::record_data_sync_chunk_unblocked(total as u64);
        }
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
    ))]
    fn on_chunk_completed(
        &mut self,
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
        chunk: ChunkFormat,
    ) -> eyre::Result<()> {
        // Peer delivery success: credit bandwidth stats, leave Requested until write outcome.
        if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
            orchestrator.on_chunk_fetched(chunk_offset, peer_addr)?;
        }

        let consensus = &self.config.consensus;
        let unpacked_chunk = match chunk {
            ChunkFormat::Unpacked(u) => u,
            ChunkFormat::Packed(packed) => unpack(
                &packed,
                consensus.entropy_packing_iterations,
                consensus.chunk_size as usize,
                consensus.chain_id,
            ),
        };

        let sm = storage_module_by_id(&self.storage_modules.read().unwrap(), storage_module_id)
            .ok_or_else(|| eyre::eyre!("storage_module_id {storage_module_id} not found"))?;

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

    /// Ensure a PeerBandwidthManager exists for an online peer that is only a
    /// residual-hole fetch hint (ingress-proof signer), with no new partition
    /// assignment attached. Such peers are fetched via data_root + tx_offset.
    fn ensure_bandwidth_manager_for_peer(&mut self, miner_address: IrysAddress) -> bool {
        let Some(peer) = self.peer_list.peer_by_mining_address(&miner_address) else {
            return false;
        };
        if !peer.is_online {
            return false;
        }
        let mut active_peers = self.active_peer_bandwidth_managers.write().unwrap();
        active_peers
            .entry(miner_address)
            .or_insert_with(|| PeerBandwidthManager::new(&miner_address, &peer, &self.config));
        true
    }

    /// Sample residual Entropy offsets that already have tx migration
    /// (`data_root_and_tx_offset_at` succeeds) and collect unique data_roots.
    fn residual_data_roots_for_proof_lookup(storage_module: &StorageModule) -> Vec<DataRoot> {
        let entropy_intervals = storage_module.get_intervals(ChunkType::Entropy);
        let mut roots = Vec::new();
        let mut seen = HashSet::new();
        let mut sampled = 0_usize;

        for interval in entropy_intervals {
            for offset_u32 in *interval.start()..=*interval.end() {
                if sampled >= MAX_RESIDUAL_OFFSETS_FOR_PROOF_SCAN {
                    return roots;
                }
                // Prefer low offsets (residual holes under the frontier / packing tail).
                if offset_u32 >= chunk_orchestrator::LOW_OFFSET_PROBE_THRESHOLD {
                    // Still allow a few high-offset samples if we have no roots yet,
                    // but skip bulk packing-tail entropy once we already have candidates.
                    if !roots.is_empty() {
                        continue;
                    }
                }
                sampled += 1;
                let part_off = PartitionChunkOffset::from(offset_u32);
                if let Ok(Some((data_root, _))) =
                    storage_module.data_root_and_tx_offset_at(part_off)
                    && seen.insert(data_root)
                {
                    roots.push(data_root);
                }
            }
        }
        roots
    }

    /// Online peers that signed accepted local ingress proofs for residual data_roots.
    ///
    /// Ingress-proof gossip is **not** chunk replication — these addresses are
    /// hints for data_sync dual-fetch (data_root path). Cap at
    /// [`MAX_INGRESS_PROOF_SIGNER_PEERS`].
    fn collect_ingress_proof_signer_peers(
        &mut self,
        storage_module: &StorageModule,
    ) -> Vec<IrysAddress> {
        let data_roots = Self::residual_data_roots_for_proof_lookup(storage_module);
        if data_roots.is_empty() {
            return Vec::new();
        }

        let local_miner = self.config.node_config.miner_address();
        let mut signers = Vec::new();
        let mut seen = HashSet::new();

        for data_root in data_roots {
            let proofs = match self
                .db
                .view_eyre(|tx| ingress_proofs_by_data_root(tx, data_root))
            {
                Ok(p) => p,
                Err(e) => {
                    warn!(
                        data_root = %data_root,
                        error = %e,
                        "failed to load ingress proofs for residual-hole peer expansion"
                    );
                    continue;
                }
            };

            for (_root, compact) in proofs {
                let address = compact.0.address;
                if address == local_miner || !seen.insert(address) {
                    continue;
                }
                if !self.ensure_bandwidth_manager_for_peer(address) {
                    continue;
                }
                debug!(
                    data_root = %data_root,
                    peer.address = %address,
                    "data_sync adding ingress-proof signer as residual-hole fetch peer"
                );
                signers.push(address);
                if signers.len() >= MAX_INGRESS_PROOF_SIGNER_PEERS {
                    return signers;
                }
            }
        }
        signers
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
            storage_module_by_id(&storage_modules, *sm_id)
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
            let Some(storage_module) = storage_module_by_id(&storage_modules, sm_id) else {
                continue;
            };

            let best_peers = self.get_best_available_peers(&storage_module, 4);

            // data_sync_probe: a data-assigned SM with entropy still to sync but
            // zero selectable peers. `matching_assignments` distinguishes "no
            // peer advertises this ledger/slot" (=0, peer-side gap) from "peers
            // advertise it but selection failed" (>0, local lookup/selection bug).
            if best_peers.is_empty()
                && let Some(pa) = storage_module.partition_assignment()
                && let Some(ledger_id) = pa.ledger_id
                && let Some((max_offset, _)) = self
                    .chunk_orchestrators
                    .get(&sm_id)
                    .and_then(ChunkOrchestrator::get_max_chunk_offset)
                && storage_module
                    .get_intervals(ChunkType::Entropy)
                    .iter()
                    .any(|iv| *iv.start() <= *max_offset)
            {
                let managers = self.active_peer_bandwidth_managers.read().unwrap();
                let matching = managers
                    .values()
                    .filter(|m| {
                        m.partition_assignments.iter().any(|a| {
                            a.ledger_id == Some(ledger_id) && a.slot_index == pa.slot_index
                        })
                    })
                    .count();
                debug!(
                    "data_sync_probe empty_peers sm_id={} ledger={:?} slot={:?} managers={} matching_assignments={}",
                    sm_id,
                    pa.ledger_id,
                    pa.slot_index,
                    managers.len(),
                    matching
                );
            }

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
        &mut self,
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
        let slot_index = pa.slot_index;

        // Find all peers that are assigned to store data for the same ledger slot
        let assigned: Vec<IrysAddress> = {
            let active_peers = self.active_peer_bandwidth_managers.read().unwrap();
            let mut candidates: Vec<&PeerBandwidthManager> = active_peers
                .values()
                .filter(|peer_manager| peer_manager.is_assigned_to(ledger_id, slot_index))
                .collect();

            // Prioritize healthy peers with available bandwidth capacity
            candidates.sort_by(|a, b| {
                (b.health_score(), b.available_concurrency())
                    .partial_cmp(&(a.health_score(), a.available_concurrency()))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            candidates
                .into_iter()
                .take(desired_count)
                .map(|peer_manager| peer_manager.miner_address)
                .collect()
        };

        // Expand with online ingress-proof signers for residual holes that already
        // have a data_root indexed. Assignees stay first (preferred path);
        // orchestrator `find_best_peer` also prefers assignees until excluded.
        //
        // Without this, empty×empty assignee loops never consult the upload/proof
        // node that still holds the body in cache (proof gossip ≠ chunk replication).
        let mut peers = assigned;
        let proof_signers = self.collect_ingress_proof_signer_peers(storage_module);
        for addr in proof_signers {
            if !peers.contains(&addr) {
                peers.push(addr);
            }
        }
        peers
    }
}

impl DataSyncService {
    pub fn spawn_service(
        rx: UnboundedReceiver<Traced<DataSyncServiceMessage>>,
        block_tree: BlockTreeReadGuard,
        storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
        peer_list: PeerList,
        db: DatabaseProvider,
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
                        db,
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
mod storage_module_lookup_tests {
    use super::storage_module_by_id;
    use irys_domain::{StorageModule, StorageModuleInfo};
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{
        Config, ConsensusConfig, DataLedger, H256, IrysAddress, NodeConfig, PartitionChunkOffset,
        partition::PartitionAssignment, partition_chunk_offset_ie,
    };
    use std::sync::Arc;

    fn test_sm(id: usize, ledger: Option<u32>, base: &std::path::Path) -> Arc<StorageModule> {
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 4,
                num_chunks_in_recall_range: 2,
                num_partitions_per_slot: 1,
                entropy_packing_iterations: 1,
                chain_id: 1,
                ..ConsensusConfig::testing()
            }),
            base_directory: base.join(format!("sm-{id}")),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let pa = ledger.map(|ledger_id| PartitionAssignment {
            ledger_id: Some(ledger_id),
            slot_index: Some(0),
            miner_address: IrysAddress::from([1_u8; 20]),
            partition_hash: H256::random(),
        });
        let info = StorageModuleInfo {
            id,
            partition_assignment: pa,
            submodules: vec![(partition_chunk_offset_ie!(0, 4), "chunks".into())],
        };
        Arc::new(StorageModule::new(&info, &config).expect("storage module"))
    }

    /// Regression: module vec is ledger-order, not id-order. Lookup must use
    /// `StorageModule::id`, not the vec index — otherwise term-ledger SMs
    /// (high directory ids, mid insertion rank) get empty data-sync peers.
    #[test]
    fn storage_module_by_id_ignores_vec_index() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        // Mimic map_storage_modules_to_partition_assignments order:
        // Publish(id=0), Submit(id=1), OneYear(id=7), ThirtyDay(id=6).
        let modules = vec![
            test_sm(0, Some(DataLedger::Publish.into()), tmp.path()),
            test_sm(1, Some(DataLedger::Submit.into()), tmp.path()),
            test_sm(7, Some(DataLedger::OneYear.into()), tmp.path()),
            test_sm(6, Some(DataLedger::ThirtyDay.into()), tmp.path()),
        ];

        assert_eq!(storage_module_by_id(&modules, 0).unwrap().id, 0);
        assert_eq!(storage_module_by_id(&modules, 1).unwrap().id, 1);
        assert_eq!(storage_module_by_id(&modules, 7).unwrap().id, 7);
        assert_eq!(storage_module_by_id(&modules, 6).unwrap().id, 6);
        // Vec-index 2 is OneYear (id=7) — must NOT be returned for id=2.
        assert!(storage_module_by_id(&modules, 2).is_none());
        // Old bug: modules.get(7) would be None (len=4); by-id finds id=7 at index 2.
        assert_eq!(
            storage_module_by_id(&modules, 7)
                .unwrap()
                .partition_assignment()
                .unwrap()
                .ledger_id,
            Some(DataLedger::OneYear.into())
        );
    }
}

#[cfg(test)]
mod ingress_proof_peer_tests {
    use super::DataSyncServiceInner;
    use crate::chunk_fetcher::MockChunkFetcher;
    use crate::services::ServiceSenders;
    use irys_database::db::IrysDatabaseExt as _;
    use irys_database::{
        IrysDatabaseArgs as _, cache_data_root, open_or_create_db,
        store_external_ingress_proof_checked, tables::IrysTables,
    };
    use irys_domain::{BlockTree, PeerList, StorageModule, StorageModuleInfo};
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{
        Config, ConsensusConfig, DataLedger, DataTransactionLedger, H256, IrysAddress,
        IrysBlockHeader, IrysPeerId, LedgerChunkOffset, LedgerChunkRange, NodeConfig,
        PartitionChunkOffset, PeerAddress, PeerListItem, PeerScore, ProtocolVersion,
        app_state::DatabaseProvider, ingress::IngressProof, irys::IrysSigner,
        ledger_chunk_offset_ii, partition::PartitionAssignment, partition_chunk_offset_ie,
    };
    use nodit::interval::ii;
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{Arc, RwLock},
    };

    /// Residual Entropy hole with an online ingress-proof signer must expand
    /// `get_best_available_peers` beyond assignees-only — otherwise empty×empty
    /// assignee loops never try the proof generator that holds the body in cache.
    #[tokio::test]
    async fn get_best_available_peers_includes_online_proof_signer() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let chunk_size = 32_u64;
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: 20,
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

        // Assignee SM with residual hole: tx indexed, no body written.
        let assignee = IrysAddress::from([0xAA; 20]);
        let prover = IrysAddress::from([0xBB; 20]);
        let pa = PartitionAssignment {
            ledger_id: Some(DataLedger::Submit.into()),
            slot_index: Some(0),
            miner_address: assignee,
            partition_hash: H256::random(),
        };
        let sm = Arc::new(
            StorageModule::new(
                &StorageModuleInfo {
                    id: 0,
                    partition_assignment: Some(pa),
                    submodules: vec![(partition_chunk_offset_ie!(0, 20), "hdd0".into())],
                },
                &config,
            )
            .expect("sm"),
        );
        sm.pack_with_zeros();

        let data_size = chunk_size as usize; // 1 chunk
        let data_bytes = vec![7_u8; data_size];
        let irys = IrysSigner::random_signer(&config.consensus);
        let tx = irys.create_transaction(data_bytes, H256::zero()).unwrap();
        let tx = irys.sign_transaction(tx).unwrap();
        let data_root = tx.header.data_root;
        let (_tx_root, proofs) =
            DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&tx.header));
        sm.index_transaction_data(
            &tx.header,
            &proofs[0].proof,
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
        )
        .expect("index");

        // Main DB: CDR + ingress proof from prover (non-assignee).
        let db = {
            let env = open_or_create_db(
                tmp.path().join("irys_db"),
                IrysTables::ALL,
                reth_db::mdbx::DatabaseArguments::irys_testing().unwrap(),
            )
            .unwrap();
            DatabaseProvider(Arc::new(env))
        };
        db.update_eyre(|wtx| {
            cache_data_root(wtx, &tx.header, None)?;
            let proof = IngressProof::V1(irys_types::ingress::IngressProofV1 {
                signature: Default::default(),
                data_root,
                proof: H256::random(),
                chain_id: config.consensus.chain_id,
                anchor: H256::zero(),
            });
            store_external_ingress_proof_checked(wtx, &proof, prover)?;
            Ok(())
        })
        .unwrap();

        // Peer list: assignee + online prover (no Submit assignment for prover).
        let (service_senders, _rx) = ServiceSenders::new();
        let peer_api = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9999);
        let peers = vec![
            PeerListItem {
                peer_id: IrysPeerId::from(assignee),
                mining_address: assignee,
                address: PeerAddress {
                    gossip: peer_api,
                    api: peer_api,
                    ..Default::default()
                },
                reputation_score: PeerScore::new(PeerScore::INITIAL),
                response_time: 0,
                last_seen: 0,
                is_online: true,
                protocol_version: ProtocolVersion::default(),
            },
            PeerListItem {
                peer_id: IrysPeerId::from(prover),
                mining_address: prover,
                address: PeerAddress {
                    gossip: peer_api,
                    api: peer_api,
                    ..Default::default()
                },
                reputation_score: PeerScore::new(PeerScore::INITIAL),
                response_time: 0,
                last_seen: 0,
                is_online: true,
                protocol_version: ProtocolVersion::default(),
            },
        ];
        let peer_list = PeerList::from_peers(
            peers,
            service_senders.peer_network.clone(),
            &config,
            tokio::sync::broadcast::channel(8).0,
        )
        .expect("peer list");

        let mut genesis = IrysBlockHeader::new_mock_header();
        {
            use irys_testing_utils::IrysBlockHeaderTestExt as _;
            genesis.test_sign();
        }
        let block_tree = BlockTree::new(&genesis, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));

        let storage_modules = Arc::new(RwLock::new(vec![sm.clone()]));
        let factory: crate::chunk_fetcher::ChunkFetcherFactory =
            Box::new(|ledger_id| Arc::new(MockChunkFetcher::new(ledger_id as usize)));

        let mut inner = DataSyncServiceInner::new(
            block_tree_guard,
            storage_modules,
            peer_list,
            db,
            factory,
            service_senders,
            config,
            tokio::runtime::Handle::current(),
        );

        // Manually ensure assignee bandwidth manager with matching PA (epoch snapshot
        // is empty in this unit fixture; production path fills this from epoch).
        {
            let peer = inner
                .peer_list
                .peer_by_mining_address(&assignee)
                .expect("assignee peer");
            let mut managers = inner.active_peer_bandwidth_managers.write().unwrap();
            let entry = managers.entry(assignee).or_insert_with(|| {
                crate::data_sync_service::peer_bandwidth_manager::PeerBandwidthManager::new(
                    &assignee,
                    &peer,
                    &inner.config,
                )
            });
            if !entry.partition_assignments.contains(&pa) {
                entry.partition_assignments.push(pa);
            }
        }

        // DataSyncServiceInner::new already creates orchestrators for data-assigned SMs.
        assert!(
            inner.chunk_orchestrators.contains_key(&sm.id),
            "orchestrator must exist for residual-hole peer selection"
        );

        let peers = inner.get_best_available_peers(&sm, 4);
        assert!(
            peers.contains(&prover),
            "online ingress-proof signer must be in peer set for residual hole; got {peers:?}"
        );
        // Assignees preferred first when present.
        if peers.contains(&assignee) {
            assert_eq!(
                peers[0], assignee,
                "assigned peers should be ordered before proof signers"
            );
        }
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
