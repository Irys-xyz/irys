pub mod chunk_fetcher;
pub mod chunk_orchestrator;
pub mod peer_bandwidth_manager;
pub mod peer_stats;

use crate::{
    chunk_fetcher::ChunkFetcherFactory,
    chunk_ingress_service::{
        ChunkIngressError, CriticalChunkIngressError, facade::ChunkIngressFacadeImpl,
    },
    metrics,
    services::ServiceSenders,
};
use chunk_fetcher::ChunkFetchFailureKind;
use chunk_orchestrator::{ChunkBlockReason, ChunkOrchestrator, ChunkRequestState};
use irys_database::db::IrysDatabaseExt as _;
use irys_database::ingress_proofs_by_data_root;
use irys_domain::{BlockTreeReadGuard, ChunkType, PeerList, StorageModule, WriteDataChunkError};
use irys_packing::unpack;
use irys_types::{
    ChunkFormat, Config, DataRoot, IrysAddress, PartitionChunkOffset, TokioServiceHandle, Traced,
    UnpackedChunk, app_state::DatabaseProvider,
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
    /// Another writer already made the requested offset durably `Data`.
    AlreadyDurable,
    /// Offset is buffered as [`ChunkType::Data`] and awaits a completed fsync.
    AwaitingDurability,
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
            // can report Data before persistence. Keep the request in an
            // explicit awaiting-durability state until the normal batched sync
            // becomes durably visible after a completed fsync.
            if matches!(sm.get_chunk_type(&expected_offset), Some(ChunkType::Data)) {
                if sm.is_data_chunk_durable_at(expected_offset) {
                    DataSyncWriteOutcome::AlreadyDurable
                } else {
                    DataSyncWriteOutcome::AwaitingDurability
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

fn ingress_error_is_invalid_peer_data(error: &ChunkIngressError) -> bool {
    matches!(
        error,
        ChunkIngressError::Critical(
            CriticalChunkIngressError::InvalidProof
                | CriticalChunkIngressError::InvalidDataHash
                | CriticalChunkIngressError::InvalidChunkSize
                | CriticalChunkIngressError::InvalidDataSize
                | CriticalChunkIngressError::InvalidOffset(_)
        )
    )
}

async fn forward_chunk_to_ingress(
    service_senders: &ServiceSenders,
    unpacked: UnpackedChunk,
) -> Result<(), ChunkIngressError> {
    ChunkIngressFacadeImpl::from(service_senders)
        .handle_chunk_ingress(unpacked)
        .await
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
    /// Dispatch-tick counter; re-arm Blocked offsets every [`REARM_EVERY_N_TICKS`].
    rearm_tick: u64,
    /// Re-arm *passes* still to skip after a zero-yield probe (counts down to 0).
    rearm_backoff_remaining: u64,
    /// Skip budget applied after the next zero-yield pass (grows, capped).
    rearm_backoff_next_skips: u64,
    /// Rotates which term-ledger storage module is visited first each tick.
    term_dispatch_cursor: usize,
    /// Rotates which Publish storage module is visited first each tick.
    publish_dispatch_cursor: usize,
}

/// Re-arm `Blocked(MissingDataRootIndex)` when the local index looks ready.
/// Dispatch runs on 250ms; re-arm is ~1s so probe cost stays off the hot path.
const REARM_EVERY_N_TICKS: u64 = 4;

/// Max readiness probes per orchestrator per re-arm = free_slots × this.
/// Stops a large still-unindexed Blocked map from becoming a full index walk
/// every second while still searching a few candidates past free-slot budget
/// for holes that are not at the lowest offsets.
const REARM_PROBE_MULTIPLIER: usize = 4;

/// After a zero-yield re-arm that actually probed Blocked offsets, skip this
/// many subsequent re-arm passes (then grow up to [`REARM_BACKOFF_MAX_SKIPS`]).
const REARM_BACKOFF_INITIAL_SKIPS: u64 = 1;

/// Cap zero-yield skip budget (~16s at 1s re-arm cadence).
const REARM_BACKOFF_MAX_SKIPS: u64 = 16;

/// Give every storage module one dispatch opportunity per round, rotating the
/// first module between ticks. The callback returns whether it consumed work.
/// Rounds stop once no module can use another shared peer permit.
#[cfg(test)]
fn dispatch_round_robin<T: Copy>(
    ids: &mut [T],
    cursor: usize,
    mut dispatch: impl FnMut(T) -> bool,
) -> usize {
    if ids.is_empty() {
        return 0;
    }
    let start = cursor % ids.len();
    ids.rotate_left(start);
    loop {
        let mut dispatched = false;
        for id in ids.iter().copied() {
            dispatched |= dispatch(id);
        }
        if !dispatched {
            break;
        }
    }
    (cursor + 1) % ids.len()
}

/// Term-ledger SMs (Submit / OneYear / ThirtyDay) take a turn before Publish
/// in each inner pass. Publish copies Submit data for promotion; if Submit
/// replicas stay on the write frontier, Publish has many sources. When shared
/// peer permits are fewer than the number of SMs, this keeps a busy permanent
/// backlog from skipping the term write head. Within each group the start
/// index still rotates so two Submit SMs share fairly.
fn dispatch_term_then_publish<T: Copy>(
    term_ids: &mut [T],
    publish_ids: &mut [T],
    term_cursor: usize,
    publish_cursor: usize,
    mut dispatch: impl FnMut(T) -> bool,
) -> (usize, usize) {
    let next_term = if term_ids.is_empty() {
        0
    } else {
        let start = term_cursor % term_ids.len();
        term_ids.rotate_left(start);
        (term_cursor + 1) % term_ids.len()
    };
    let next_publish = if publish_ids.is_empty() {
        0
    } else {
        let start = publish_cursor % publish_ids.len();
        publish_ids.rotate_left(start);
        (publish_cursor + 1) % publish_ids.len()
    };

    loop {
        let mut dispatched = false;
        for id in term_ids.iter().copied() {
            dispatched |= dispatch(id);
        }
        for id in publish_ids.iter().copied() {
            dispatched |= dispatch(id);
        }
        if !dispatched {
            break;
        }
    }
    (next_term, next_publish)
}

#[cfg(test)]
mod scheduler_fairness_tests {
    use super::{dispatch_round_robin, dispatch_term_then_publish};

    #[test]
    fn shared_capacity_is_round_robin_across_storage_modules() {
        let mut ids = [0_u8, 1_u8];
        let mut permits = 3_usize;
        let mut order = Vec::new();
        let next_cursor = dispatch_round_robin(&mut ids, 0, |id| {
            if permits == 0 {
                return false;
            }
            permits -= 1;
            order.push(id);
            true
        });
        assert_eq!(order, vec![0, 1, 0]);
        assert_eq!(next_cursor, 1);

        let mut ids = [0_u8, 1_u8];
        let mut permits = 3_usize;
        let mut order = Vec::new();
        let _ = dispatch_round_robin(&mut ids, next_cursor, |id| {
            if permits == 0 {
                return false;
            }
            permits -= 1;
            order.push(id);
            true
        });
        assert_eq!(order, vec![1, 0, 1]);
    }

    #[test]
    fn term_ledgers_take_permits_before_publish_when_capacity_is_scarce() {
        // 1 Submit + 3 Publish, only 3 shared permits: Submit must not be the
        // SM left out, or the term write head waits behind a permanent backlog.
        let mut term = [1_u8];
        let mut publish = [0_u8, 2_u8, 3_u8];
        let mut permits = 3_usize;
        let mut order = Vec::new();
        let _ = dispatch_term_then_publish(&mut term, &mut publish, 0, 0, |id| {
            if permits == 0 {
                return false;
            }
            permits -= 1;
            order.push(id);
            true
        });
        assert_eq!(order, vec![1, 0, 2]);
        assert!(!order.contains(&3), "a Publish SM is the one that waits");
    }

    #[test]
    fn two_term_ledgers_rotate_fairly_ahead_of_publish() {
        let mut term = [1_u8, 4_u8];
        let mut publish = [0_u8];
        let mut permits = 3_usize;
        let mut order = Vec::new();
        let (next_term, next_publish) =
            dispatch_term_then_publish(&mut term, &mut publish, 0, 0, |id| {
                if permits == 0 {
                    return false;
                }
                permits -= 1;
                order.push(id);
                true
            });
        assert_eq!(order, vec![1, 4, 0]);
        assert_eq!(next_term, 1);
        assert_eq!(next_publish, 0);

        let mut term = [1_u8, 4_u8];
        let mut publish = [0_u8];
        let mut permits = 3_usize;
        let mut order = Vec::new();
        let _ =
            dispatch_term_then_publish(&mut term, &mut publish, next_term, next_publish, |id| {
                if permits == 0 {
                    return false;
                }
                permits -= 1;
                order.push(id);
                true
            });
        assert_eq!(order, vec![4, 1, 0]);
    }
}

pub enum DataSyncServiceMessage {
    /// Refresh peer/orchestrator membership for current ledger-assigned SMs.
    ///
    /// Does **not** drive Blocked-offset re-queue. Orchestrators re-arm
    /// `MissingDataRootIndex` offsets by probing local index readiness on the
    /// re-arm tick (see [`DataSyncServiceInner::rearm_index_ready_blocked`]).
    SyncPartitions,
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
        failure_kind: ChunkFetchFailureKind,
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
            rearm_tick: 0,
            rearm_backoff_remaining: 0,
            rearm_backoff_next_skips: 0,
            term_dispatch_cursor: 0,
            publish_dispatch_cursor: 0,
        };
        data_sync.synchronize_peers_and_orchestrators();
        data_sync
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub async fn handle_message(&mut self, msg: DataSyncServiceMessage) -> eyre::Result<()> {
        match msg {
            DataSyncServiceMessage::SyncPartitions => {
                // New membership / post-heal: probe again promptly.
                self.reset_rearm_backoff();
                self.synchronize_peers_and_orchestrators();
            }
            DataSyncServiceMessage::ChunkCompleted {
                storage_module_id,
                chunk_offset,
                peer_address: peer_addr,
                chunk,
            } => {
                if let Err(e) = self
                    .on_chunk_completed(storage_module_id, chunk_offset, peer_addr, chunk)
                    .await
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
                failure_kind,
            } => {
                metrics::record_data_sync_chunk_failure();
                if let Err(e) =
                    self.on_chunk_failed(storage_module_id, chunk_offset, peer_addr, failure_kind)
                {
                    error!(
                        "Failed to handle chunk failure for storage_module {} chunk_offset {} from peer {}: {e:?}",
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
        let mut orchestrator_ids: Vec<_> = self.chunk_orchestrators.keys().copied().collect();
        orchestrator_ids.sort_unstable();
        for id in &orchestrator_ids {
            if let Some(orchestrator) = self.chunk_orchestrators.get_mut(id) {
                orchestrator.prepare_tick();
            }
        }

        if !orchestrator_ids.is_empty() {
            // Term-ledger SMs (especially Submit) visit first in each pass so
            // a large Publish backlog cannot consume every shared peer permit
            // before the term write head is fetched. Publish later copies
            // Submit data; that path is healthy when Submit replicas are on
            // the frontier. Within each group, rotate so two Submit SMs still
            // share fairly.
            let mut term_ids = Vec::new();
            let mut publish_ids = Vec::new();
            for id in orchestrator_ids.iter().copied() {
                if self
                    .chunk_orchestrators
                    .get(&id)
                    .is_some_and(ChunkOrchestrator::prioritizes_write_frontier)
                {
                    term_ids.push(id);
                } else {
                    publish_ids.push(id);
                }
            }
            let (next_term, next_publish) = dispatch_term_then_publish(
                &mut term_ids,
                &mut publish_ids,
                self.term_dispatch_cursor,
                self.publish_dispatch_cursor,
                |id| {
                    self.chunk_orchestrators
                        .get_mut(&id)
                        .is_some_and(ChunkOrchestrator::dispatch_next)
                },
            );
            self.term_dispatch_cursor = next_term;
            self.publish_dispatch_cursor = next_publish;

            for id in &orchestrator_ids {
                if let Some(orchestrator) = self.chunk_orchestrators.get(id) {
                    let state = orchestrator.get_metrics();
                    metrics::record_data_sync_scheduler_state(
                        orchestrator.ledger_id(),
                        *id,
                        &state,
                    );
                }
            }
        }
        self.optimize_peer_concurrency();
        self.rearm_tick = self.rearm_tick.wrapping_add(1);
        if self.rearm_tick.is_multiple_of(REARM_EVERY_N_TICKS) {
            if self.rearm_backoff_remaining > 0 {
                self.rearm_backoff_remaining -= 1;
            } else {
                self.rearm_index_ready_blocked();
            }
        }
        Ok(())
    }

    fn reset_rearm_backoff(&mut self) {
        self.rearm_backoff_remaining = 0;
        self.rearm_backoff_next_skips = 0;
    }

    /// After probing Blocked offsets with zero unblocks, skip more re-arm passes.
    fn grow_rearm_backoff(&mut self) {
        let next = if self.rearm_backoff_next_skips == 0 {
            REARM_BACKOFF_INITIAL_SKIPS
        } else {
            self.rearm_backoff_next_skips
                .saturating_mul(2)
                .min(REARM_BACKOFF_MAX_SKIPS)
        };
        self.rearm_backoff_next_skips = next;
        self.rearm_backoff_remaining = next;
        debug!(
            data_sync.rearm_backoff_skips = next,
            "zero-yield re-arm; backing off index readiness probes"
        );
    }

    /// Re-queue `Blocked(MissingDataRootIndex)` offsets whose local SM index is
    /// ready ([`StorageModule::is_data_root_index_ready_at`] — same completion
    /// predicate as index heal), capped by free pending budget.
    ///
    /// Anti-thrash is local: still-unindexed offsets stay Blocked. Probe work is
    /// also capped (`free_slots × REARM_PROBE_MULTIPLIER`) so a large unready
    /// backlog cannot force a full-map index walk every re-arm tick.
    ///
    /// Zero-yield passes that actually saw Blocked offsets grow a skip backoff
    /// (reset on unblock success or [`DataSyncServiceMessage::SyncPartitions`]).
    fn rearm_index_ready_blocked(&mut self) {
        let max_pending = self.config.node_config.data_sync.max_pending_chunk_requests as usize;
        let mut total = 0_usize;
        // True if we ran readiness probes against at least one Blocked map entry.
        let mut probed_blocked = false;
        // Collect ids first so we can probe each SM without holding orchestrator mut.
        let sm_ids: Vec<StorageModuleId> = self.chunk_orchestrators.keys().copied().collect();
        for id in sm_ids {
            let Some(sm) = storage_module_by_id(&self.storage_modules.read().unwrap(), id) else {
                continue;
            };
            let Some(orchestrator) = self.chunk_orchestrators.get_mut(&id) else {
                continue;
            };
            let pending = orchestrator
                .chunk_requests
                .values()
                .filter(|r| matches!(r.request_state, ChunkRequestState::Pending))
                .count();
            let free_slots = max_pending.saturating_sub(pending);
            if free_slots == 0 {
                continue;
            }
            let has_blocked = orchestrator.chunk_requests.values().any(|r| {
                matches!(
                    r.request_state,
                    ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex)
                )
            });
            if !has_blocked {
                continue;
            }
            let max_probes = free_slots.saturating_mul(REARM_PROBE_MULTIPLIER);
            probed_blocked = true;
            let unblocked =
                orchestrator.unblock_missing_data_root_index_where(free_slots, max_probes, |off| {
                    sm.is_data_root_index_ready_at(off)
                });
            if unblocked > 0 {
                debug!(
                    storage_module.id = id,
                    data_sync.unblocked = unblocked,
                    data_sync.unblock_cap = free_slots,
                    data_sync.probe_cap = max_probes,
                    "re-queued Blocked offsets with ready local data_root index"
                );
                total += unblocked;
            }
        }
        if total > 0 {
            metrics::record_data_sync_chunk_unblocked(total as u64);
            self.reset_rearm_backoff();
        } else if probed_blocked {
            self.grow_rearm_backoff();
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
    async fn on_chunk_completed(
        &mut self,
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
        chunk: ChunkFormat,
    ) -> eyre::Result<()> {
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

        // The ingress acknowledgement is the durable compact-leaf fence. Do
        // not let the storage module make this fetch permanently complete
        // until validation has committed the cached body and signer-specific
        // ingress hash. This also makes a closed ingress channel retryable.
        if let Err(error) =
            forward_chunk_to_ingress(&self.service_senders, unpacked_chunk.clone()).await
        {
            if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
                if ingress_error_is_invalid_peer_data(&error) {
                    metrics::record_data_sync_chunk_failure();
                    metrics::record_data_sync_fetch_failure(
                        orchestrator.ledger_id(),
                        peer_addr,
                        "invalid_chunk",
                    );
                    orchestrator.on_chunk_failed(
                        chunk_offset,
                        peer_addr,
                        ChunkFetchFailureKind::InvalidResponse,
                    )?;
                } else {
                    // The peer delivered valid bytes as far as the network is
                    // concerned; local ingress/database pressure must not
                    // penalize that peer, but the offset remains unresolved.
                    orchestrator.on_chunk_fetched(chunk_offset, peer_addr)?;
                    orchestrator.requeue_after_local_write_failure(chunk_offset)?;
                }
            }
            return Err(eyre::eyre!(
                "chunk ingress rejected data-sync body before durable write: {error}"
            ));
        }

        // Validation accepted the body. Credit the fetch separately from the
        // later storage-module durability transition.
        metrics::record_data_sync_chunk_fetched();
        if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
            orchestrator.on_chunk_fetched(chunk_offset, peer_addr)?;
        }

        let sm = storage_module_by_id(&self.storage_modules.read().unwrap(), storage_module_id)
            .ok_or_else(|| eyre::eyre!("storage_module_id {storage_module_id} not found"))?;

        let pa = sm.partition_assignment();
        let ledger_id = pa.and_then(|p| p.ledger_id);
        let slot_index = pa.and_then(|p| p.slot_index);
        let partition_hash = pa.map(|p| p.partition_hash);

        let write_outcome = attempt_data_sync_write(&sm, &unpacked_chunk, chunk_offset);

        match write_outcome {
            DataSyncWriteOutcome::AlreadyDurable => {
                metrics::record_data_sync_chunk_stored();
                if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
                    orchestrator.mark_chunk_already_durable(chunk_offset)?;
                }
                debug!(
                    storage_module.id = storage_module_id,
                    chunk.offset = %chunk_offset,
                    chunk.data_root = %unpacked_chunk.data_root,
                    ?ledger_id,
                    ?slot_index,
                    ?partition_hash,
                    peer.address = %peer_addr,
                    "data_sync chunk was already durably stored"
                );
            }
            DataSyncWriteOutcome::AwaitingDurability => {
                if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
                    orchestrator.mark_chunk_awaiting_durability(chunk_offset)?;
                }
                debug!(
                    storage_module.id = storage_module_id,
                    chunk.offset = %chunk_offset,
                    chunk.data_root = %unpacked_chunk.data_root,
                    ?ledger_id,
                    ?slot_index,
                    ?partition_hash,
                    peer.address = %peer_addr,
                    "data_sync chunk buffered awaiting durable batch"
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
                     will re-queue when local index resolves this offset"
                );
                if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
                    orchestrator
                        .mark_chunk_blocked(chunk_offset, ChunkBlockReason::MissingDataRootIndex)?;
                }
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
        failure_kind: ChunkFetchFailureKind,
    ) -> eyre::Result<()> {
        if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
            orchestrator.on_chunk_failed(chunk_offset, peer_addr, failure_kind)?;

            if failure_kind != ChunkFetchFailureKind::NotFound {
                let pa = orchestrator
                    .storage_module
                    .partition_assignment()
                    .expect("A partition assignment present");
                debug!(
                    "chunk failed: ledger:{:?}, slot_index:{:?} chunk_offset:{} peer:{} kind:{:?}",
                    pa.ledger_id, pa.slot_index, chunk_offset, peer_addr, failure_kind
                );
            }
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
                tracing::trace!(
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
                            let (msg, span) = traced.into_parts();
                            self.inner.handle_message(msg).instrument(span).await?;
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
            let (msg, span) = traced.into_parts();
            self.inner.handle_message(msg).instrument(span).await?;
        }

        tracing::info!("shutting down DataSync Service gracefully");
        Ok(())
    }
}

#[cfg(test)]
mod ingress_handoff_tests {
    use super::{forward_chunk_to_ingress, ingress_error_is_invalid_peer_data};
    use crate::chunk_ingress_service::{ChunkIngressError, CriticalChunkIngressError};
    use crate::services::ServiceSenders;
    use irys_types::UnpackedChunk;

    #[tokio::test]
    async fn closed_ingress_channel_fails_the_handoff() {
        let (senders, receivers) = ServiceSenders::new();
        drop(receivers.chunk_ingress);

        assert!(
            forward_chunk_to_ingress(&senders, UnpackedChunk::default())
                .await
                .is_err(),
            "data sync must not credit a body that ingress cannot durably acknowledge"
        );
    }

    #[test]
    fn only_validation_failures_penalize_the_source_peer() {
        assert!(ingress_error_is_invalid_peer_data(
            &ChunkIngressError::Critical(CriticalChunkIngressError::InvalidProof,)
        ));
        assert!(!ingress_error_is_invalid_peer_data(
            &ChunkIngressError::Critical(CriticalChunkIngressError::DatabaseError,)
        ));
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
        // Distinct gossip sockets — two processes cannot share a listen address.
        let (service_senders, _rx) = ServiceSenders::new();
        let assignee_api = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9998);
        let prover_api = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9999);
        let peers = vec![
            PeerListItem {
                peer_id: IrysPeerId::from(assignee),
                mining_address: assignee,
                address: PeerAddress {
                    gossip: assignee_api,
                    api: assignee_api,
                    ..Default::default()
                },
                reputation_score: PeerScore::new(PeerScore::INITIAL),
                response_time: 0,
                last_seen: 0,
                is_online: true,
                protocol_version: ProtocolVersion::default(),
                ..Default::default()
            },
            PeerListItem {
                peer_id: IrysPeerId::from(prover),
                mining_address: prover,
                address: PeerAddress {
                    gossip: prover_api,
                    api: prover_api,
                    ..Default::default()
                },
                reputation_score: PeerScore::new(PeerScore::INITIAL),
                response_time: 0,
                last_seen: 0,
                is_online: true,
                protocol_version: ProtocolVersion::default(),
                ..Default::default()
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
    use irys_database::{
        db::IrysDatabaseExt as _,
        submodule::{add_data_root_info, tables::DataRootInfo},
    };
    use irys_domain::{StorageModule, StorageModuleInfo, WriteDataChunkError};
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{
        Config, ConsensusConfig, DataLedger, H256, IrysAddress, NodeConfig, PartitionChunkOffset,
        RelativeChunkOffset, StorageSyncConfig, TxChunkOffset, UnpackedChunk,
        partition::PartitionAssignment, partition_chunk_offset_ie,
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

    #[test]
    fn successful_data_sync_write_stays_buffered_below_sync_threshold() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let num_chunks = 10_u64;
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
            storage: StorageSyncConfig {
                num_writes_before_sync: num_chunks,
            },
            base_directory: tmp.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                ledger_id: Some(DataLedger::Publish.into()),
                slot_index: Some(0),
                miner_address: IrysAddress::from([7_u8; 20]),
                partition_hash: H256::random(),
            }),
            submodules: vec![(
                partition_chunk_offset_ie!(0, num_chunks as u32),
                "chunks".into(),
            )],
        };
        let sm = Arc::new(StorageModule::new(&info, &config).expect("storage module"));
        sm.pack_with_zeros();

        let data_root = H256::random();
        let offset = PartitionChunkOffset::from(0_u32);
        let (_, submodule) = sm
            .get_submodule_for_offset(offset)
            .expect("submodule for offset");
        submodule
            .db
            .update_eyre(|tx| {
                add_data_root_info(
                    tx,
                    data_root,
                    &DataRootInfo {
                        start_offset: RelativeChunkOffset::from(0_i32),
                        data_size: chunk_size,
                    },
                )
            })
            .expect("index data root");

        let chunk = UnpackedChunk {
            data_root,
            data_size: chunk_size,
            data_path: vec![1, 2, 3, 4].into(),
            bytes: vec![0xcd; chunk_size as usize].into(),
            tx_offset: TxChunkOffset::from(0_u32),
        };

        assert_eq!(
            attempt_data_sync_write(&sm, &chunk, offset),
            DataSyncWriteOutcome::AwaitingDurability
        );
        assert!(
            sm.has_pending_writes(),
            "the per-chunk data-sync path must not force a below-threshold fsync"
        );
        assert!(
            !sm.is_data_chunk_durable_at(offset),
            "buffered Data must not be reported as durable"
        );
    }
}
