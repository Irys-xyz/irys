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
use irys_domain::{BatchEnqueueItem, BlockTreeReadGuard, ChunkType, PeerList, StorageModule};
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

/// Local write outcome after a successful peer fetch or a local-cache hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DataSyncWriteOutcome {
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

#[cfg(test)]
async fn attempt_data_sync_write(
    sm: &StorageModule,
    unpacked: &UnpackedChunk,
    expected_offset: PartitionChunkOffset,
) -> DataSyncWriteOutcome {
    match sm.write_data_chunk_queued(unpacked).await {
        Err(irys_domain::WriteDataChunkError::DataRootNotFound) => {
            DataSyncWriteOutcome::MissingDataRootIndex
        }
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

/// The peer body must match its own data path. Ingress can return `Ok`
/// without this check: a recent-chunk hit keys only the path hash, and a
/// wrong length returns `Ok` before the leaf hash is compared.
fn validate_peer_unpacked_chunk(chunk: &UnpackedChunk, chunk_size: u64) -> eyre::Result<()> {
    let (min_byte, max_byte) =
        irys_types::expected_chunk_byte_range(chunk.tx_offset, chunk.data_size, chunk_size)
            .map_err(|error| eyre::eyre!("peer chunk data path range: {error}"))?;
    let target = u128::from(max_byte - 1);
    let path = irys_types::validate_path(chunk.data_root.0, &chunk.data_path, target)
        .map_err(|error| eyre::eyre!("peer chunk data path: {error}"))?;
    let chunk_len = u64::try_from(chunk.bytes.len())
        .map_err(|_| eyre::eyre!("peer chunk length exceeds u64"))?;
    let expected_len = max_byte - min_byte;
    eyre::ensure!(
        chunk_len == expected_len,
        "peer chunk length {chunk_len} does not match the data path range {expected_len}"
    );
    irys_types::validate_path_byte_range(&path, (min_byte, max_byte))
        .map_err(|error| eyre::eyre!("peer chunk data path range: {error}"))?;
    let hash = irys_types::hash_sha256(&chunk.bytes.0);
    eyre::ensure!(
        path.leaf_hash == hash,
        "peer chunk hash does not match the data path leaf"
    );
    Ok(())
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

/// Chunk size 32, one partition per slot, packing iterations 1, chain id 1.
/// Callers that need a different storage sync overlay mutate the result.
#[cfg(test)]
fn sync_test_node_config(base: &std::path::Path, num_chunks: u64) -> irys_types::NodeConfig {
    irys_types::NodeConfig {
        consensus: irys_types::ConsensusOptions::Custom(irys_types::ConsensusConfig {
            chunk_size: 32,
            num_chunks_in_partition: num_chunks,
            num_chunks_in_recall_range: 2,
            num_partitions_per_slot: 1,
            entropy_packing_iterations: 1,
            chain_id: 1,
            ..irys_types::ConsensusConfig::testing()
        }),
        base_directory: base.to_path_buf(),
        ..irys_types::NodeConfig::testing()
    }
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
    burst: usize,
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

    let burst = burst.max(1);
    loop {
        let mut dispatched = false;
        for id in term_ids.iter().copied() {
            for _ in 0..burst {
                if !dispatch(id) {
                    break;
                }
                dispatched = true;
            }
        }
        for id in publish_ids.iter().copied() {
            for _ in 0..burst {
                if !dispatch(id) {
                    break;
                }
                dispatched = true;
            }
        }
        if !dispatched {
            break;
        }
    }
    (next_term, next_publish)
}

pub(crate) fn classify_synced_chunk(
    sm: &StorageModule,
    unpacked: &UnpackedChunk,
    expected_offset: PartitionChunkOffset,
    item: &BatchEnqueueItem,
) -> DataSyncWriteOutcome {
    if matches!(item, BatchEnqueueItem::DataRootNotFound) {
        return DataSyncWriteOutcome::MissingDataRootIndex;
    }
    if sm.is_data_chunk_durable_at(expected_offset) {
        return DataSyncWriteOutcome::AlreadyDurable;
    }
    let queued_here = match item {
        BatchEnqueueItem::Queued(offsets) => offsets.contains(&expected_offset),
        BatchEnqueueItem::NotQueued | BatchEnqueueItem::DataRootNotFound => false,
    };
    if queued_here
        || sm.is_data_write_pending_at(expected_offset)
        || matches!(sm.get_chunk_type(&expected_offset), Some(ChunkType::Data))
    {
        return DataSyncWriteOutcome::AwaitingDurability;
    }
    if irys_domain::trace_index_read(irys_domain::DATA_SYNC, || {
        matches!(
            sm.collect_data_root_infos(unpacked.data_root),
            Ok(infos) if infos.0.is_empty()
        )
    }) {
        return DataSyncWriteOutcome::MissingDataRootIndex;
    }
    DataSyncWriteOutcome::NoWriteableOffset
}

#[cfg(test)]
mod scheduler_fairness_tests {
    use super::dispatch_term_then_publish;

    #[test]
    fn term_ledgers_take_permits_before_publish_when_capacity_is_scarce() {
        // 1 Submit + 3 Publish, only 3 shared permits: Submit must not be the
        // SM left out, or the term write head waits behind a permanent backlog.
        let mut term = [1_u8];
        let mut publish = [0_u8, 2_u8, 3_u8];
        let mut permits = 3_usize;
        let mut order = Vec::new();
        let _ = dispatch_term_then_publish(&mut term, &mut publish, 0, 0, 1, |id| {
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
            dispatch_term_then_publish(&mut term, &mut publish, 0, 0, 1, |id| {
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
            dispatch_term_then_publish(&mut term, &mut publish, next_term, next_publish, 1, |id| {
                if permits == 0 {
                    return false;
                }
                permits -= 1;
                order.push(id);
                true
            });
        assert_eq!(order, vec![4, 1, 0]);
    }

    #[test]
    fn one_storage_module_fills_a_span_before_the_next() {
        let mut term = [1_u8, 4_u8];
        let mut publish = [0_u8];
        let mut permits = 5_usize;
        let mut order = Vec::new();
        let _ = dispatch_term_then_publish(&mut term, &mut publish, 0, 0, 3, |id| {
            if permits == 0 {
                return false;
            }
            permits -= 1;
            order.push(id);
            true
        });
        assert_eq!(order, vec![1, 1, 1, 4, 4]);
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
    GetActivePeersList(oneshot::Sender<Arc<RwLock<HashMap<IrysAddress, PeerBandwidthManager>>>>),
    /// Entropy offsets a retired body-migration job left behind. The local-cache
    /// miss memo is cleared. Holes join the tail of the ready queue, and only
    /// while a hot slot is free. A blocked offset stays blocked.
    BodyMigrationHoles {
        storage_module_id: usize,
        chunk_offsets: Vec<PartitionChunkOffset>,
    },
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
            DataSyncServiceMessage::GetActivePeersList(tx) => self.handle_get_active_peers_list(tx),
            DataSyncServiceMessage::BodyMigrationHoles {
                storage_module_id,
                chunk_offsets,
            } => {
                if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
                    orchestrator.queue_body_migration_holes(&chunk_offsets);
                } else {
                    debug!(
                        storage_module.id = storage_module_id,
                        holes = chunk_offsets.len(),
                        "retired body holes have no data sync orchestrator"
                    );
                }
            }
        }
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub fn tick(&mut self) -> eyre::Result<()> {
        let mut orchestrator_ids: Vec<_> = self.chunk_orchestrators.keys().copied().collect();
        orchestrator_ids.sort_unstable();
        // A module that was fully Data at the last peer sync has an orchestrator
        // and no bandwidth managers. Discovery of a new Entropy interval is the
        // first time that hole can be fetched, so attach the slot's assignees
        // before this tick dispatches.
        let mut sync_new_entropy = false;
        for id in &orchestrator_ids {
            if let Some(orchestrator) = self.chunk_orchestrators.get_mut(id) {
                orchestrator.prepare_tick();
                sync_new_entropy |= orchestrator.note_new_entropy_without_peers();
            }
        }
        if sync_new_entropy {
            self.sync_peer_partition_assignments();
            self.update_orchestrator_peers();
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
            let chunk = self.config.consensus.chunk_size.max(1);
            let sweep = self
                .config
                .node_config
                .storage
                .entropy_sweep_max_bytes
                .max(chunk);
            let burst = usize::try_from(sweep / chunk).unwrap_or(1).max(1);
            let (next_term, next_publish) = dispatch_term_then_publish(
                &mut term_ids,
                &mut publish_ids,
                self.term_dispatch_cursor,
                self.publish_dispatch_cursor,
                burst,
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
            let unblocked = irys_domain::trace_index_read(irys_domain::DATA_SYNC, || {
                orchestrator.unblock_missing_data_root_index_where(free_slots, max_probes, |off| {
                    sm.is_data_root_index_ready_at(off)
                })
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
            if utilization_ratio >= 0.8
                && let Some(peer_manager) = peers.get_mut(&peer_addr)
            {
                // Peers below 0.7 already continued. 0.9 and above get the larger step.
                // The step stops at the ceiling from peer construction. That ceiling
                // is the bandwidth scale or the 10 MiB sweep span, whichever is larger.
                let increase = if health_score >= 0.9 { 5 } else { 3 };
                let next = current_max
                    .saturating_add(increase)
                    .min(peer_manager.concurrency_ceiling());
                if next == current_max {
                    continue;
                }
                debug!(
                    "Increasing max concurrency from {} to {} for peer {} (utilization: {:.1}%, health: {:.2})",
                    current_max,
                    next,
                    peer_addr,
                    utilization_ratio * 100.0,
                    health_score
                );
                peer_manager.set_max_concurrency(next);
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

        // Ingress can accept a body it did not hash. Check this chunk against
        // its own data path before the bytes are credited or written.
        if let Err(error) = validate_peer_unpacked_chunk(&unpacked_chunk, consensus.chunk_size) {
            if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
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
            }
            return Err(eyre::eyre!(
                "data sync rejected peer chunk before durable write: {error}"
            ));
        }

        // The body matches its own data path. It still has to be the chunk
        // indexed at the offset we asked for. A self-consistent chunk for
        // another data_root would otherwise be written at that root's
        // placement, and the requested offset would be blocked or re-queued
        // with the peer still eligible. A missing local index is not a
        // mismatch: the enqueue path still classifies that write.
        let mismatched_placement =
            storage_module_by_id(&self.storage_modules.read().unwrap(), storage_module_id)
                .and_then(|sm| {
                    irys_domain::trace_index_read(irys_domain::DATA_SYNC, || {
                        sm.data_root_and_tx_offset_at(chunk_offset)
                    })
                    .ok()
                    .flatten()
                    .filter(|(root, tx_offset)| {
                        *root != unpacked_chunk.data_root || *tx_offset != unpacked_chunk.tx_offset
                    })
                });
        if let Some((local_root, local_tx_offset)) = mismatched_placement {
            if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
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
            }
            return Err(eyre::eyre!(
                "data sync rejected peer chunk at offset {chunk_offset}: fetched data_root {} tx_offset {} does not match the local data_root {local_root} tx_offset {local_tx_offset}",
                unpacked_chunk.data_root,
                unpacked_chunk.tx_offset,
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

        if sm.is_data_chunk_durable_at(chunk_offset) {
            return self.apply_sync_outcome(
                storage_module_id,
                chunk_offset,
                &unpacked_chunk,
                peer_addr,
                ledger_id,
                slot_index,
                partition_hash,
                DataSyncWriteOutcome::AlreadyDurable,
            );
        }
        if sm.is_data_write_pending_at(chunk_offset)
            || matches!(sm.get_chunk_type(&chunk_offset), Some(ChunkType::Data))
        {
            return self.apply_sync_outcome(
                storage_module_id,
                chunk_offset,
                &unpacked_chunk,
                peer_addr,
                ledger_id,
                slot_index,
                partition_hash,
                DataSyncWriteOutcome::AwaitingDurability,
            );
        }
        let enqueued = match sm.enqueue_ingress_batch(std::slice::from_ref(&unpacked_chunk)) {
            Ok(items) => items,
            Err(error) => {
                return self.apply_sync_outcome(
                    storage_module_id,
                    chunk_offset,
                    &unpacked_chunk,
                    peer_addr,
                    ledger_id,
                    slot_index,
                    partition_hash,
                    DataSyncWriteOutcome::Other(error.to_string()),
                );
            }
        };
        let Some(item) = enqueued.into_iter().next() else {
            return self.apply_sync_outcome(
                storage_module_id,
                chunk_offset,
                &unpacked_chunk,
                peer_addr,
                ledger_id,
                slot_index,
                partition_hash,
                DataSyncWriteOutcome::Other("data sync enqueue returned no item".to_string()),
            );
        };
        let outcome = classify_synced_chunk(&sm, &unpacked_chunk, chunk_offset, &item);
        self.apply_sync_outcome(
            storage_module_id,
            chunk_offset,
            &unpacked_chunk,
            peer_addr,
            ledger_id,
            slot_index,
            partition_hash,
            outcome,
        )
    }

    fn apply_sync_outcome(
        &mut self,
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        unpacked_chunk: &UnpackedChunk,
        peer_addr: IrysAddress,
        ledger_id: Option<u32>,
        slot_index: Option<usize>,
        partition_hash: Option<irys_types::H256>,
        write_outcome: DataSyncWriteOutcome,
    ) -> eyre::Result<()> {
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
                    orchestrator.note_body_buffered(chunk_offset)?;
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
                self.requeue_requested(storage_module_id, chunk_offset)?;
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
                    "data_sync write failed; re-queueing"
                );
                self.requeue_requested(storage_module_id, chunk_offset)?;
            }
        }
        Ok(())
    }

    fn requeue_requested(
        &mut self,
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
    ) -> eyre::Result<()> {
        let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) else {
            return Ok(());
        };
        let requested = matches!(
            orchestrator
                .chunk_requests
                .get(&chunk_offset)
                .map(|request| &request.request_state),
            Some(ChunkRequestState::Requested(..))
        );
        if requested {
            orchestrator.requeue_after_local_write_failure(chunk_offset)?;
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

            if failure_kind != ChunkFetchFailureKind::NotFound
                && let Some(pa) = orchestrator.storage_module.partition_assignment()
            {
                debug!(
                    "chunk failed: ledger:{:?}, slot_index:{:?} chunk_offset:{} peer:{} kind:{:?}",
                    pa.ledger_id, pa.slot_index, chunk_offset, peer_addr, failure_kind
                );
            }
        }
        Ok(())
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

            // Check to see if the storage module has any entropy (packed) chunks that need data.
            // The live interval map is enough: rebuilding every span is the cost that
            // stalls a fragmented module, and pending-only entropy appears after the next sync.
            if !storage_module_has_entropy(&storage_module) {
                debug!("StorageModule has no entropy chunks\n{:?}", pa);
                continue;
            }

            // If it does, ensure there's a bandwidth manager for any peer storing the data for this storage module
            self.ensure_bandwidth_managers_for_peers(ledger_id, slot_index);
        }

        // Managers outlive a single slot. Replace each list with the epoch's
        // current data partitions for that miner, so a slot the peer left
        // does not stay preferred.
        self.replace_peer_assignments_from_epoch();
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

            // Create the manager when this peer is new. Its partition list is
            // replaced from the epoch snapshot after every local slot is visited.
            let mut active_peers = self.active_peer_bandwidth_managers.write().unwrap();
            active_peers
                .entry(pa.miner_address)
                .or_insert(PeerBandwidthManager::new(
                    &pa.miner_address,
                    &peer,
                    &self.config,
                ));
        }
    }

    /// Set every live manager's partition list from the canonical epoch.
    ///
    /// The epoch read is dropped before the peer map is locked. The same
    /// order is used when a manager is created.
    fn replace_peer_assignments_from_epoch(&mut self) {
        let by_miner = {
            let epoch = self.block_tree.read().canonical_epoch_snapshot();
            let mut by_miner: HashMap<IrysAddress, Vec<_>> = HashMap::new();
            for assignment in epoch
                .partition_assignments
                .data_partitions
                .values()
                .copied()
            {
                by_miner
                    .entry(assignment.miner_address)
                    .or_default()
                    .push(assignment);
            }
            by_miner
        };
        let mut active = self.active_peer_bandwidth_managers.write().unwrap();
        for manager in active.values_mut() {
            manager.partition_assignments = by_miner
                .get(&manager.miner_address)
                .cloned()
                .unwrap_or_default();
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
        irys_domain::trace_index_read(irys_domain::DATA_SYNC, || {
            Self::residual_data_roots_in_caller(storage_module)
        })
    }

    fn residual_data_roots_in_caller(storage_module: &StorageModule) -> Vec<DataRoot> {
        // Sample a bounded prefix of each entropy span. Walking a packing tail
        // offset by offset is one iteration per chunk in the partition.
        let (low, high) = sample_residual_entropy_offsets(storage_module);
        let mut roots = Vec::new();
        let mut seen = HashSet::new();
        for offset in low {
            push_residual_root(storage_module, offset, &mut roots, &mut seen);
        }
        // High samples exist only to fill the cap when the low holes produced
        // no indexed root. A root from the low holes ends the scan.
        if !roots.is_empty() {
            return roots;
        }
        for offset in high {
            push_residual_root(storage_module, offset, &mut roots, &mut seen);
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

        // Drop an orchestrator whose ledger is gone or has changed. The loop
        // below builds a new one with a fetcher for the live ledger. A
        // same-ledger slot change stays: the slot and the ledger range are
        // read from the module on each request.
        //
        // `Requested` rows still hold a peer slot. The dropped orchestrator
        // will not see their completion, so release those slots here. A row
        // that already left `Requested` released its own slot. Do not penalize
        // the peer: the ledger changed, the peer did not fail.
        self.chunk_orchestrators.retain(|sm_id, orchestrator| {
            let keep = storage_module_by_id(&storage_modules, *sm_id)
                .and_then(|sm| sm.partition_assignment())
                .and_then(|pa| pa.ledger_id)
                == Some(orchestrator.ledger_id());
            if !keep {
                orchestrator.release_in_flight_peer_slots();
            }
            keep
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

            // The fetcher is bound to this ledger. A second read inside the
            // constructor must still see the same ledger, or the build aborts.
            let ledger_id = pa.ledger_id.unwrap();
            let chunk_fetcher = (self.chunk_fetcher_factory)(ledger_id);

            let Some(orchestrator) = ChunkOrchestrator::try_new_for_ledger(
                sm.clone(),
                self.active_peer_bandwidth_managers.clone(),
                self.block_tree.clone(),
                &self.service_senders,
                chunk_fetcher,
                self.config.node_config.clone(),
                self.runtime_handle.clone(),
                self.db.clone(),
                ledger_id,
            ) else {
                continue;
            };

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
                && storage_module_has_entropy_at_or_below(&storage_module, max_offset)
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

        // An epoch update can clear the assignment before SyncPartitions drops
        // this orchestrator.
        let Some(pa) = storage_module.partition_assignment() else {
            return Vec::new();
        };

        // Check to see that the partition hash hasn't been re-assigned to capacity and no longer
        // has any data to sync
        if pa.ledger_id.is_none() {
            // Capacity stores entropy, not ledger bodies. Membership sync drops
            // this orchestrator; until then there is nothing to fetch.
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

fn storage_module_has_entropy(storage_module: &StorageModule) -> bool {
    storage_module
        .label_runs()
        .iter()
        .any(|(_, chunk_type)| *chunk_type == ChunkType::Entropy)
}

fn storage_module_has_entropy_at_or_below(
    storage_module: &StorageModule,
    max_offset: PartitionChunkOffset,
) -> bool {
    storage_module
        .label_runs()
        .iter()
        .any(|(interval, chunk_type)| {
            *chunk_type == ChunkType::Entropy && *interval.start() <= *max_offset
        })
}

fn push_residual_root(
    storage_module: &StorageModule,
    offset: u32,
    roots: &mut Vec<DataRoot>,
    seen: &mut HashSet<DataRoot>,
) {
    let part_off = PartitionChunkOffset::from(offset);
    if let Ok(Some((data_root, _))) = storage_module.data_root_and_tx_offset_at(part_off)
        && seen.insert(data_root)
    {
        roots.push(data_root);
    }
}

/// At most [`MAX_RESIDUAL_OFFSETS_FOR_PROOF_SCAN`] entropy offsets. Low offsets
/// come first. A span wider than the remaining budget contributes only its
/// prefix, never one step per chunk.
fn sample_residual_entropy_offsets(storage_module: &StorageModule) -> (Vec<u32>, Vec<u32>) {
    let intervals = storage_module.label_runs();
    let mut low = Vec::new();
    let mut high = Vec::new();
    let mut sampled = 0_usize;
    let threshold = chunk_orchestrator::LOW_OFFSET_PROBE_THRESHOLD;
    for (interval, chunk_type) in intervals.iter() {
        if sampled >= MAX_RESIDUAL_OFFSETS_FOR_PROOF_SCAN {
            break;
        }
        if *chunk_type != ChunkType::Entropy {
            continue;
        }
        let start = *interval.start();
        let end = *interval.end();
        if start < threshold {
            let low_end = end.min(threshold - 1);
            sampled = take_offset_prefix(start, low_end, sampled, &mut low);
        }
        if sampled >= MAX_RESIDUAL_OFFSETS_FOR_PROOF_SCAN || end < threshold {
            continue;
        }
        let high_start = start.max(threshold);
        if high_start <= end {
            sampled = take_offset_prefix(high_start, end, sampled, &mut high);
        }
    }
    (low, high)
}

fn take_offset_prefix(start: u32, end: u32, mut sampled: usize, out: &mut Vec<u32>) -> usize {
    if start > end {
        return sampled;
    }
    let mut offset = start;
    while sampled < MAX_RESIDUAL_OFFSETS_FOR_PROOF_SCAN {
        out.push(offset);
        sampled += 1;
        if offset == end {
            break;
        }
        offset += 1;
    }
    sampled
}

/// A failed peer-event receive. `Closed` stays ready on every `select`.
/// Continuing the loop busy-spins, and a new subscribe cannot reopen a
/// channel whose senders are already gone.
enum PeerEventChannelFault {
    /// Missed events. The caller resyncs membership.
    Lagged,
    /// Every sender is gone. The caller stops the service.
    Closed,
}

fn peer_event_channel_fault(
    error: tokio::sync::broadcast::error::RecvError,
) -> PeerEventChannelFault {
    match error {
        tokio::sync::broadcast::error::RecvError::Lagged(_) => PeerEventChannelFault::Lagged,
        tokio::sync::broadcast::error::RecvError::Closed => PeerEventChannelFault::Closed,
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

        // Subscribe to peer lifecycle events for event-driven synchronization
        let mut peer_events_rx = self.inner.peer_list.subscribe_to_peer_events();
        self.run_until_stopped(&mut peer_events_rx).await
    }

    /// Runs until shutdown, a closed message channel, a tick error, or a
    /// closed peer-event channel. The receiver is a parameter so a test can
    /// close it. `PeerList` keeps the production sender alive.
    async fn run_until_stopped(
        &mut self,
        peer_events_rx: &mut tokio::sync::broadcast::Receiver<irys_domain::PeerEvent>,
    ) -> eyre::Result<()> {
        let mut interval = tokio::time::interval(Duration::from_millis(250));
        interval.tick().await; // Skip first immediate tick

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
                        Err(error) => match peer_event_channel_fault(error) {
                            PeerEventChannelFault::Lagged => {
                                // Missed events; do a conservative resync
                                self.inner.synchronize_peers_and_orchestrators();
                            }
                            PeerEventChannelFault::Closed => {
                                warn!(
                                    "peer events channel closed in DataSyncService; stopping"
                                );
                                break;
                            }
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
mod peer_event_channel_tests {
    use super::{
        DataSyncService, DataSyncServiceInner, PeerEventChannelFault, peer_event_channel_fault,
    };
    use crate::chunk_fetcher::MockChunkFetcher;
    use crate::services::ServiceSenders;
    use irys_database::{IrysDatabaseArgs as _, open_or_create_db, tables::IrysTables};
    use irys_domain::{BlockTree, PeerList};
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{Config, NodeConfig, app_state::DatabaseProvider};
    use std::sync::{Arc, RwLock};
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn closed_peer_event_channel_stops_instead_of_spinning() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = tmp.path().to_path_buf();
        let config = Config::new_with_random_peer_id(node_config);
        let env = open_or_create_db(
            tmp.path().join("db"),
            IrysTables::ALL,
            reth_db::mdbx::DatabaseArguments::irys_testing().expect("mdbx testing args"),
        )
        .expect("db");
        let (service_senders, _receivers) = ServiceSenders::new();
        let genesis = irys_testing_utils::new_mock_signed_header();
        let block_tree = BlockTree::new(&genesis, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let inner = DataSyncServiceInner::new(
            block_tree_guard,
            Arc::new(RwLock::new(Vec::new())),
            PeerList::test_mock().expect("peer list"),
            DatabaseProvider(Arc::new(env)),
            Box::new(|ledger_id| Arc::new(MockChunkFetcher::new(ledger_id as usize))),
            service_senders,
            config,
            tokio::runtime::Handle::current(),
        );
        let (msg_tx, msg_rx) = tokio::sync::mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();
        let (peer_tx, mut peer_rx) = tokio::sync::broadcast::channel(1);
        drop(peer_tx);
        let mut service = DataSyncService {
            shutdown: shutdown_rx,
            msg_rx,
            inner,
        };

        // The message sender and the shutdown signal stay alive. A dropped
        // message sender or a fired shutdown would leave the loop on another arm.
        // The service runs on its own thread: a closed broadcast receiver stays
        // ready, so `continue` never returns to this thread's timer.
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("runtime");
            let result = runtime.block_on(service.run_until_stopped(&mut peer_rx));
            let _ = done_tx.send(result);
        });
        let report = done_rx.recv_timeout(Duration::from_millis(500));
        let elapsed = started.elapsed();
        drop(msg_tx);
        drop(shutdown_tx);
        match report {
            Ok(Ok(())) => {}
            Ok(Err(err)) => panic!("closed peer-event channel stopped with an error: {err:#}"),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // The service thread is still inside the ready Closed receive.
                // Leave the process so the test binary does not wait on that thread.
                eprintln!("closed peer-event channel did not stop the service");
                std::process::exit(101);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("service thread ended without a result");
            }
        }
        assert!(elapsed < Duration::from_millis(500), "elapsed {elapsed:?}");
    }

    #[test]
    fn lagged_peer_events_do_not_stop_the_loop() {
        let error = tokio::sync::broadcast::error::RecvError::Lagged(3);
        assert!(matches!(
            peer_event_channel_fault(error),
            PeerEventChannelFault::Lagged
        ));
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
        Config, DataLedger, H256, IrysAddress, PartitionChunkOffset,
        partition::PartitionAssignment, partition_chunk_offset_ie,
    };
    use std::sync::Arc;

    fn test_sm(id: usize, ledger: Option<u32>, base: &std::path::Path) -> Arc<StorageModule> {
        let node_config = super::sync_test_node_config(&base.join(format!("sm-{id}")), 4);
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
        Config, DataLedger, DataTransactionLedger, H256, IrysAddress, IrysBlockHeader, IrysPeerId,
        LedgerChunkOffset, LedgerChunkRange, PartitionChunkOffset, PeerAddress, PeerListItem,
        PeerScore, ProtocolVersion, app_state::DatabaseProvider, ingress::IngressProof,
        irys::IrysSigner, ledger_chunk_offset_ii, partition::PartitionAssignment,
        partition_chunk_offset_ie,
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
        let node_config = super::sync_test_node_config(tmp.path(), 20);
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
            0,
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
    use irys_database::submodule::{SubmoduleStore as _, tables::DataRootInfo};
    use irys_domain::{StorageModule, StorageModuleInfo, WriteDataChunkError};
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{
        Config, DataLedger, H256, IrysAddress, PartitionChunkOffset, RelativeChunkOffset,
        StorageSyncConfig, TxChunkOffset, UnpackedChunk, partition::PartitionAssignment,
        partition_chunk_offset_ie,
    };
    use std::sync::Arc;

    /// Unindexed data_root must classify as MissingDataRootIndex (not Other/requeue thrash).
    #[tokio::test]
    async fn unindexed_data_root_classifies_as_missing_index() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let num_chunks = 4_u64;
        let chunk_size = 32_u64;
        let node_config = super::sync_test_node_config(tmp.path(), num_chunks);
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

        let outcome = attempt_data_sync_write(&sm, &chunk, PartitionChunkOffset::from(0_u32)).await;
        assert_eq!(outcome, DataSyncWriteOutcome::MissingDataRootIndex);
    }

    #[tokio::test]
    async fn successful_data_sync_write_stays_buffered_below_sync_threshold() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let num_chunks = 10_u64;
        let chunk_size = 32_u64;
        let mut node_config = super::sync_test_node_config(tmp.path(), num_chunks);
        node_config.storage = StorageSyncConfig {
            num_writes_before_sync: num_chunks,
            max_pending_write_bytes: None,
            entropy_sweep_interval_millis: 0,
            min_durable_interval_ms: 0,
            ..StorageSyncConfig::default()
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
            .update(|tx| {
                tx.add_data_root_info(
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
            attempt_data_sync_write(&sm, &chunk, offset).await,
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

#[cfg(test)]
mod body_migration_holes_routing_tests {
    use super::chunk_orchestrator::ChunkRequestState;
    use super::{DataSyncServiceInner, DataSyncServiceMessage};
    use crate::chunk_fetcher::MockChunkFetcher;
    use crate::services::ServiceSenders;
    use irys_database::{IrysDatabaseArgs as _, open_or_create_db, tables::IrysTables};
    use irys_domain::{BlockTree, PeerList, StorageModule, StorageModuleInfo};
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{
        Config, DataLedger, H256, IrysAddress, IrysBlockHeader, PartitionChunkOffset,
        app_state::DatabaseProvider, partition::PartitionAssignment, partition_chunk_offset_ie,
    };
    use std::sync::{Arc, RwLock};

    /// The service message reaches the orchestrator for that storage-module id.
    /// An unknown id is ignored.
    #[tokio::test]
    async fn body_migration_holes_message_queues_on_that_storage_module() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let config = Config::new_with_random_peer_id(super::sync_test_node_config(tmp.path(), 4));
        let sm = Arc::new(
            StorageModule::new(
                &StorageModuleInfo {
                    id: 3,
                    partition_assignment: Some(PartitionAssignment {
                        ledger_id: Some(DataLedger::Publish.into()),
                        slot_index: Some(0),
                        miner_address: IrysAddress::from([1_u8; 20]),
                        partition_hash: H256::random(),
                    }),
                    submodules: vec![(partition_chunk_offset_ie!(0, 4), "chunks".into())],
                },
                &config,
            )
            .expect("storage module"),
        );
        sm.pack_with_zeros();

        let env = open_or_create_db(
            tmp.path().join("db"),
            IrysTables::ALL,
            reth_db::mdbx::DatabaseArguments::irys_testing().expect("mdbx testing args"),
        )
        .expect("db");
        let db = DatabaseProvider(Arc::new(env));
        let (service_senders, _receivers) = ServiceSenders::new();
        let mut genesis = IrysBlockHeader::new_mock_header();
        {
            use irys_testing_utils::IrysBlockHeaderTestExt as _;
            genesis.test_sign();
        }
        let block_tree = BlockTree::new(&genesis, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let peer_list = PeerList::from_peers(
            Vec::new(),
            service_senders.peer_network.clone(),
            &config,
            tokio::sync::broadcast::channel(8).0,
        )
        .expect("peer list");
        let factory: crate::chunk_fetcher::ChunkFetcherFactory =
            Box::new(|ledger_id| Arc::new(MockChunkFetcher::new(ledger_id as usize)));
        let mut inner = DataSyncServiceInner::new(
            block_tree_guard,
            Arc::new(RwLock::new(vec![sm.clone()])),
            peer_list,
            db,
            factory,
            service_senders,
            config,
            tokio::runtime::Handle::current(),
        );

        let orchestrator = inner
            .chunk_orchestrators
            .get(&sm.id)
            .expect("orchestrator for the assigned module");
        assert!(orchestrator.chunk_requests.is_empty());

        inner
            .handle_message(DataSyncServiceMessage::BodyMigrationHoles {
                storage_module_id: 99,
                chunk_offsets: vec![PartitionChunkOffset::from(0)],
            })
            .await
            .expect("unknown storage module is ignored");
        assert!(!inner.chunk_orchestrators.contains_key(&99));
        assert!(inner.chunk_orchestrators[&sm.id].chunk_requests.is_empty());

        inner
            .handle_message(DataSyncServiceMessage::BodyMigrationHoles {
                storage_module_id: sm.id,
                chunk_offsets: vec![PartitionChunkOffset::from(0), PartitionChunkOffset::from(1)],
            })
            .await
            .expect("holes are routed");
        let orchestrator = &inner.chunk_orchestrators[&sm.id];
        assert!(matches!(
            orchestrator.chunk_requests[&PartitionChunkOffset::from(0)].request_state,
            ChunkRequestState::Pending
        ));
        assert!(matches!(
            orchestrator.chunk_requests[&PartitionChunkOffset::from(1)].request_state,
            ChunkRequestState::Pending
        ));
    }
}

#[cfg(test)]
mod peer_chunk_validation_tests {
    use super::DataSyncServiceInner;
    use super::chunk_orchestrator::{ChunkRequest, ChunkRequestState};
    use crate::chunk_fetcher::MockChunkFetcher;
    use crate::chunk_ingress_service::ChunkIngressMessage;
    use crate::services::ServiceSenders;
    use irys_database::submodule::{SubmoduleStore as _, tables::DataRootInfo};
    use irys_database::{IrysDatabaseArgs as _, open_or_create_db, tables::IrysTables};
    use irys_domain::{BlockTree, ChunkType, PeerList, StorageModule, StorageModuleInfo};
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{
        Base64, ChunkFormat, Config, DataLedger, H256, IrysAddress, IrysBlockHeader,
        LedgerChunkOffset, LedgerChunkRange, PartitionChunkOffset, RelativeChunkOffset,
        TxChunkOffset, UnpackedChunk, app_state::DatabaseProvider, irys::IrysSigner,
        ledger_chunk_offset_ii, partition::PartitionAssignment, partition_chunk_offset_ie,
    };
    use nodit::interval::ii;
    use std::{
        sync::{Arc, RwLock},
        time::Instant,
    };

    fn mark_requested(
        inner: &mut DataSyncServiceInner,
        storage_module_id: usize,
        offset: PartitionChunkOffset,
        peer: IrysAddress,
    ) {
        let orchestrator = inner
            .chunk_orchestrators
            .get_mut(&storage_module_id)
            .expect("orchestrator");
        orchestrator.chunk_requests.insert(
            offset,
            ChunkRequest {
                excluded: Default::default(),
                request_state: ChunkRequestState::Requested(peer, Instant::now()),
                attempt_count: 1,
                retry_round: 0,
                ready_since: Instant::now(),
                local_cache_miss: false,
            },
        );
    }

    /// Ingress answers `Ok` without looking at the bytes. Data sync must still
    /// reject a body whose length or hash does not match its own data path,
    /// and it must store a body that does match.
    #[tokio::test]
    async fn peer_bytes_must_match_their_data_path_before_enqueue() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let chunk_size = 32_u64;
        let config = Config::new_with_random_peer_id(super::sync_test_node_config(tmp.path(), 4));
        let sm = Arc::new(
            StorageModule::new(
                &StorageModuleInfo {
                    id: 0,
                    partition_assignment: Some(PartitionAssignment {
                        ledger_id: Some(DataLedger::Publish.into()),
                        slot_index: Some(0),
                        miner_address: IrysAddress::from([1_u8; 20]),
                        partition_hash: H256::random(),
                    }),
                    submodules: vec![(partition_chunk_offset_ie!(0, 4), "chunks".into())],
                },
                &config,
            )
            .expect("storage module"),
        );
        sm.pack_with_zeros();

        let signer = IrysSigner::random_signer(&config.consensus);
        let tx = signer
            .create_transaction(vec![7_u8; chunk_size as usize], H256::zero())
            .expect("tx");
        let good = tx.data_chunks().expect("chunk").remove(0);
        let offset = PartitionChunkOffset::from(0_u32);
        let (_, submodule) = sm.get_submodule_for_offset(offset).expect("submodule");
        submodule
            .db
            .update(|db_tx| {
                db_tx.add_data_root_info(
                    good.data_root,
                    &DataRootInfo {
                        start_offset: RelativeChunkOffset::from(0_i32),
                        data_size: good.data_size,
                    },
                )
            })
            .expect("index data root");

        let env = open_or_create_db(
            tmp.path().join("db"),
            IrysTables::ALL,
            reth_db::mdbx::DatabaseArguments::irys_testing().expect("mdbx testing args"),
        )
        .expect("db");
        let db = DatabaseProvider(Arc::new(env));
        let (service_senders, receivers) = ServiceSenders::new();
        let mut ingress_rx = receivers.chunk_ingress;
        tokio::spawn(async move {
            while let Some(traced) = ingress_rx.recv().await {
                let (msg, _span) = traced.into_parts();
                if let ChunkIngressMessage::IngestChunk(_, Some(reply), _) = msg {
                    let _ = reply.send(Ok(()));
                }
            }
        });
        let mut genesis = IrysBlockHeader::new_mock_header();
        {
            use irys_testing_utils::IrysBlockHeaderTestExt as _;
            genesis.test_sign();
        }
        let block_tree = BlockTree::new(&genesis, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let peer_list = PeerList::from_peers(
            Vec::new(),
            service_senders.peer_network.clone(),
            &config,
            tokio::sync::broadcast::channel(8).0,
        )
        .expect("peer list");
        let factory: crate::chunk_fetcher::ChunkFetcherFactory =
            Box::new(|ledger_id| Arc::new(MockChunkFetcher::new(ledger_id as usize)));
        let mut inner = DataSyncServiceInner::new(
            block_tree_guard,
            Arc::new(RwLock::new(vec![sm.clone()])),
            peer_list,
            db,
            factory,
            service_senders,
            config,
            tokio::runtime::Handle::current(),
        );

        let peer = IrysAddress::from([9_u8; 20]);
        let short = UnpackedChunk {
            bytes: Base64(vec![7_u8; 4]),
            ..good.clone()
        };
        mark_requested(&mut inner, sm.id, offset, peer);
        let err = inner
            .on_chunk_completed(sm.id, offset, peer, ChunkFormat::Unpacked(short))
            .await
            .expect_err("a short body must not be stored");
        assert!(
            err.to_string().contains("data path") || err.to_string().contains("length"),
            "{err}"
        );
        assert!(!sm.is_data_write_pending_at(offset));
        assert_eq!(sm.get_chunk_type(&offset), Some(ChunkType::Entropy));
        assert!(
            inner.chunk_orchestrators[&sm.id].chunk_requests[&offset]
                .excluded
                .contains(&peer)
        );

        let mismatched = UnpackedChunk {
            bytes: Base64(vec![0_u8; chunk_size as usize]),
            ..good.clone()
        };
        mark_requested(&mut inner, sm.id, offset, peer);
        let err = inner
            .on_chunk_completed(sm.id, offset, peer, ChunkFormat::Unpacked(mismatched))
            .await
            .expect_err("bytes that miss the leaf hash must not be stored");
        assert!(
            err.to_string().contains("leaf") || err.to_string().contains("hash"),
            "{err}"
        );
        assert!(!sm.is_data_write_pending_at(offset));
        assert!(
            inner.chunk_orchestrators[&sm.id].chunk_requests[&offset]
                .excluded
                .contains(&peer)
        );

        mark_requested(&mut inner, sm.id, offset, peer);
        inner
            .on_chunk_completed(sm.id, offset, peer, ChunkFormat::Unpacked(good))
            .await
            .expect("a matching chunk is stored");
        sm.pump_entropy_reads();
        sm.force_sync_pending_chunks().expect("durable point");
        assert!(sm.is_data_chunk_durable_at(offset));
        assert_eq!(sm.get_chunk_type(&offset), Some(ChunkType::Data));
    }

    /// A body that matches its own data path, but not the data_root indexed at
    /// the requested offset, must not be written at the foreign placement and
    /// must not clear the peer exclusion by blocking the hole.
    #[tokio::test]
    async fn foreign_self_consistent_chunk_does_not_fill_the_requested_offset() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let chunk_size = 32_u64;
        let config = Config::new_with_random_peer_id(super::sync_test_node_config(tmp.path(), 4));
        let sm = Arc::new(
            StorageModule::new(
                &StorageModuleInfo {
                    id: 0,
                    partition_assignment: Some(PartitionAssignment {
                        ledger_id: Some(DataLedger::Publish.into()),
                        slot_index: Some(0),
                        miner_address: IrysAddress::from([1_u8; 20]),
                        partition_hash: H256::random(),
                    }),
                    submodules: vec![(partition_chunk_offset_ie!(0, 4), "chunks".into())],
                },
                &config,
            )
            .expect("storage module"),
        );
        sm.pack_with_zeros();

        let signer = IrysSigner::random_signer(&config.consensus);
        let local_tx = signer
            .create_transaction(vec![1_u8; chunk_size as usize], H256::zero())
            .expect("local tx");
        let local_tx = signer.sign_transaction(local_tx).expect("sign local");
        let foreign_tx = signer
            .create_transaction(vec![2_u8; chunk_size as usize], H256::zero())
            .expect("foreign tx");
        let foreign_tx = signer.sign_transaction(foreign_tx).expect("sign foreign");
        let (_root, local_proofs) = irys_types::DataTransactionLedger::merklize_tx_root(
            std::slice::from_ref(&local_tx.header),
        );
        let (_root, foreign_proofs) = irys_types::DataTransactionLedger::merklize_tx_root(
            std::slice::from_ref(&foreign_tx.header),
        );
        sm.index_transaction_data(
            &local_tx.header,
            &local_proofs[0].proof,
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            0,
        )
        .expect("index local tx at offset 0");
        sm.index_transaction_data(
            &foreign_tx.header,
            &foreign_proofs[0].proof,
            LedgerChunkRange(ledger_chunk_offset_ii!(1, 1)),
            0,
        )
        .expect("index foreign tx at offset 1");

        let requested = PartitionChunkOffset::from(0_u32);
        let foreign_offset = PartitionChunkOffset::from(1_u32);
        let (local_root, local_tx_offset) = sm
            .data_root_and_tx_offset_at(requested)
            .expect("local index read")
            .expect("offset 0 is indexed");
        let foreign = foreign_tx.data_chunks().expect("foreign chunk").remove(0);
        assert_ne!(foreign.data_root, local_root);
        assert_eq!(local_tx_offset, TxChunkOffset::from(0_u32));
        assert_eq!(sm.get_chunk_type(&requested), Some(ChunkType::Entropy));
        assert_eq!(sm.get_chunk_type(&foreign_offset), Some(ChunkType::Entropy));

        let env = open_or_create_db(
            tmp.path().join("db"),
            IrysTables::ALL,
            reth_db::mdbx::DatabaseArguments::irys_testing().expect("mdbx testing args"),
        )
        .expect("db");
        let db = DatabaseProvider(Arc::new(env));
        let (service_senders, receivers) = ServiceSenders::new();
        let mut ingress_rx = receivers.chunk_ingress;
        tokio::spawn(async move {
            while let Some(traced) = ingress_rx.recv().await {
                let (msg, _span) = traced.into_parts();
                if let ChunkIngressMessage::IngestChunk(_, Some(reply), _) = msg {
                    let _ = reply.send(Ok(()));
                }
            }
        });
        let mut genesis = IrysBlockHeader::new_mock_header();
        {
            use irys_testing_utils::IrysBlockHeaderTestExt as _;
            genesis.test_sign();
        }
        let block_tree = BlockTree::new(&genesis, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let peer_list = PeerList::from_peers(
            Vec::new(),
            service_senders.peer_network.clone(),
            &config,
            tokio::sync::broadcast::channel(8).0,
        )
        .expect("peer list");
        let factory: crate::chunk_fetcher::ChunkFetcherFactory =
            Box::new(|ledger_id| Arc::new(MockChunkFetcher::new(ledger_id as usize)));
        let mut inner = DataSyncServiceInner::new(
            block_tree_guard,
            Arc::new(RwLock::new(vec![sm.clone()])),
            peer_list,
            db,
            factory,
            service_senders,
            config,
            tokio::runtime::Handle::current(),
        );

        let peer = IrysAddress::from([9_u8; 20]);
        mark_requested(&mut inner, sm.id, requested, peer);
        let err = inner
            .on_chunk_completed(sm.id, requested, peer, ChunkFormat::Unpacked(foreign))
            .await
            .expect_err("a chunk for another data_root must not be stored");
        assert!(
            err.to_string().contains("data_root") || err.to_string().contains("tx_offset"),
            "{err}"
        );
        assert_eq!(sm.get_chunk_type(&requested), Some(ChunkType::Entropy));
        assert_eq!(sm.get_chunk_type(&foreign_offset), Some(ChunkType::Entropy));
        assert!(!sm.is_data_write_pending_at(requested));
        assert!(!sm.is_data_write_pending_at(foreign_offset));
        let request = &inner.chunk_orchestrators[&sm.id].chunk_requests[&requested];
        assert!(
            request.excluded.contains(&peer),
            "the peer that returned the foreign chunk must be excluded"
        );
        assert!(
            !matches!(request.request_state, ChunkRequestState::Blocked(_)),
            "a foreign chunk is not a local index miss: {:?}",
            request.request_state
        );
    }
}

#[cfg(test)]
mod entropy_peer_sync_tests {
    use super::DataSyncServiceInner;
    use crate::chunk_fetcher::MockChunkFetcher;
    use crate::services::ServiceSenders;
    use irys_database::{IrysDatabaseArgs as _, open_or_create_db, tables::IrysTables};
    use irys_domain::{BlockTree, ChunkType, PeerList, StorageModule, StorageModuleInfo};
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{
        Config, DataLedger, H256, IrysAddress, IrysBlockHeader, IrysPeerId, PartitionChunkOffset,
        PeerAddress, PeerListItem, PeerScore, ProtocolVersion, app_state::DatabaseProvider,
        partition::PartitionAssignment, partition_chunk_offset_ie,
    };
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{Arc, RwLock},
    };

    /// A module that was entirely non-entropy at the last peer sync has no
    /// bandwidth managers. The first entropy interval must attach the slot's
    /// assignees, or the hole waits for the next epoch assignment update.
    #[tokio::test]
    async fn new_entropy_interval_syncs_peers_when_the_module_has_none() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let config = Config::new_with_random_peer_id(super::sync_test_node_config(tmp.path(), 4));
        let assignee = IrysAddress::from([0xA1; 20]);
        let pa = PartitionAssignment {
            ledger_id: Some(DataLedger::Submit.into()),
            slot_index: Some(0),
            miner_address: assignee,
            partition_hash: H256::from([0x31; 32]),
        };
        let sm = Arc::new(
            StorageModule::new(
                &StorageModuleInfo {
                    id: 2,
                    partition_assignment: Some(pa),
                    // Absolute, so clear_assignment rewrites the same packing_params.toml
                    // that open read (a relative path resolves against the cwd).
                    submodules: vec![(partition_chunk_offset_ie!(0, 4), tmp.path().join("chunks"))],
                },
                &config,
            )
            .expect("storage module"),
        );
        assert!(
            sm.get_intervals(ChunkType::Entropy).is_empty(),
            "the module starts with no entropy, so peer sync must skip it"
        );

        let env = open_or_create_db(
            tmp.path().join("db"),
            IrysTables::ALL,
            reth_db::mdbx::DatabaseArguments::irys_testing().expect("mdbx testing args"),
        )
        .expect("db");
        let db = DatabaseProvider(Arc::new(env));
        let (service_senders, _receivers) = ServiceSenders::new();
        let api = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 19091);
        let peers = vec![PeerListItem {
            peer_id: IrysPeerId::from(assignee),
            mining_address: assignee,
            address: PeerAddress {
                gossip: api,
                api,
                ..Default::default()
            },
            reputation_score: PeerScore::new(PeerScore::INITIAL),
            is_online: true,
            protocol_version: ProtocolVersion::default(),
            ..Default::default()
        }];
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
        {
            let tip = block_tree_guard.read().tip;
            let mut tree = block_tree_guard.write();
            let entry = tree.blocks.get_mut(&tip).expect("genesis entry");
            let mut snapshot = irys_domain::EpochSnapshot::default();
            snapshot
                .partition_assignments
                .data_partitions
                .insert(pa.partition_hash, pa);
            entry.epoch_snapshot = Arc::new(snapshot);
        }

        let mut inner = DataSyncServiceInner::new(
            block_tree_guard,
            Arc::new(RwLock::new(vec![sm.clone()])),
            peer_list,
            db,
            Box::new(|ledger_id| Arc::new(MockChunkFetcher::new(ledger_id as usize))),
            service_senders,
            config,
            tokio::runtime::Handle::current(),
        );

        assert!(
            inner
                .active_peer_bandwidth_managers
                .read()
                .unwrap()
                .is_empty(),
            "no entropy at startup means no peer managers"
        );
        assert!(
            inner.chunk_orchestrators[&sm.id].current_peers.is_empty(),
            "the orchestrator has nobody to fetch from"
        );

        sm.pack_with_zeros();
        assert!(
            !sm.get_intervals(ChunkType::Entropy).is_empty(),
            "packing opens an entropy interval"
        );
        inner.tick().expect("tick");

        let managers = inner.active_peer_bandwidth_managers.read().unwrap();
        let manager = managers
            .get(&assignee)
            .expect("the slot assignee must gain a bandwidth manager");
        assert!(
            manager.partition_assignments.contains(&pa),
            "the manager must record this partition assignment"
        );
        drop(managers);
        assert!(
            inner.chunk_orchestrators[&sm.id]
                .current_peers
                .contains(&assignee),
            "discovery must publish the new manager to the orchestrator"
        );

        // An epoch update can clear the assignment before SyncPartitions drops
        // the orchestrator. Peer lookups and the frontier must not panic then.
        sm.clear_assignment(1).expect("clear assignment");
        assert!(inner.get_best_available_peers(&sm, 4).is_empty());
        assert!(
            inner.chunk_orchestrators[&sm.id]
                .get_max_chunk_offset()
                .is_none()
        );
    }
}

#[cfg(test)]
mod residual_scan_tests {
    use super::DataSyncServiceInner;
    use super::chunk_orchestrator::LOW_OFFSET_PROBE_THRESHOLD;
    use irys_domain::{ChunkType, StorageModule, StorageModuleInfo};
    use irys_testing_utils::TempDirBuilder;
    use irys_types::irys::IrysSigner;
    use irys_types::{
        Config, ConsensusConfig, DataTransactionLedger, H256, LedgerChunkOffset, LedgerChunkRange,
        NodeConfig, PartitionChunkOffset, ledger_chunk_offset_ii, partition::PartitionAssignment,
        partition_chunk_offset_ie,
    };
    use nodit::interval::ii;
    use std::time::Instant;

    /// A few indexed low holes plus one packing-tail interval must not walk
    /// the tail offset by offset. The tail is what a full partition looks like.
    #[test]
    fn residual_root_scan_skips_a_high_entropy_span() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let mut node_config = NodeConfig::testing();
        node_config.base_directory = tmp.path().to_path_buf();
        node_config.consensus = irys_types::ConsensusOptions::Custom(ConsensusConfig {
            chunk_size: 32,
            num_chunks_in_partition: 4,
            num_chunks_in_recall_range: 2,
            entropy_packing_iterations: 1,
            ..ConsensusConfig::testing()
        });
        let config = Config::new_with_random_peer_id(node_config);
        let sm = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(PartitionAssignment::default()),
                submodules: vec![(partition_chunk_offset_ie!(0, 4), "chunks".into())],
            },
            &config,
        )
        .expect("storage module");
        sm.pack_with_zeros();

        let data = vec![7_u8; 32];
        let signer = IrysSigner::random_signer(&config.consensus);
        let tx = signer
            .create_transaction(data, H256::zero())
            .expect("transaction");
        let tx = signer.sign_transaction(tx).expect("signed transaction");
        let data_root = tx.header.data_root;
        let (_tx_root, proofs) =
            DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&tx.header));
        sm.index_transaction_data(
            &tx.header,
            &proofs[0].proof,
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            0,
        )
        .expect("index");

        let tail_end = LOW_OFFSET_PROBE_THRESHOLD.saturating_add(100_000_000);
        {
            let mut intervals = sm.intervals().write().unwrap();
            intervals
                .insert_strict(
                    ii(
                        PartitionChunkOffset::from(LOW_OFFSET_PROBE_THRESHOLD),
                        PartitionChunkOffset::from(tail_end),
                    ),
                    ChunkType::Entropy,
                )
                .expect("high entropy span");
        }

        let started = Instant::now();
        let roots = DataSyncServiceInner::residual_data_roots_in_caller(&sm);
        let elapsed = started.elapsed();
        eprintln!("scale_residual roots={} elapsed={elapsed:?}", roots.len());
        assert!(
            roots.contains(&data_root),
            "the low indexed hole must still be reported"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(200),
            "high entropy span of 100_000_000 offsets took {elapsed:?}"
        );
    }
}

#[cfg(test)]
mod assignment_follow_tests {
    use super::{DataSyncServiceInner, DataSyncServiceMessage};
    use crate::chunk_fetcher::{ChunkFetcher, ChunkFetcherFactory, MockChunkFetcher};
    use crate::data_sync_service::chunk_orchestrator::{ChunkRequest, ChunkRequestState};
    use crate::services::ServiceSenders;
    use irys_database::{IrysDatabaseArgs as _, open_or_create_db, tables::IrysTables};
    use irys_domain::{BlockTree, PeerList, StorageModule, StorageModuleInfo};
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{
        Config, DataLedger, H256, IrysAddress, IrysBlockHeader, IrysPeerId, PartitionChunkOffset,
        PeerAddress, PeerListItem, PeerScore, ProtocolVersion, app_state::DatabaseProvider,
        partition::PartitionAssignment, partition_chunk_offset_ie,
    };
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::Instant;

    fn test_config(base: &std::path::Path) -> Config {
        Config::new_with_random_peer_id(super::sync_test_node_config(base, 4))
    }

    fn signed_genesis() -> IrysBlockHeader {
        use irys_testing_utils::IrysBlockHeaderTestExt as _;
        let mut genesis = IrysBlockHeader::new_mock_header();
        genesis.test_sign();
        genesis
    }

    fn open_db(base: &std::path::Path) -> DatabaseProvider {
        let env = open_or_create_db(
            base.join("db"),
            IrysTables::ALL,
            reth_db::mdbx::DatabaseArguments::irys_testing().expect("mdbx testing args"),
        )
        .expect("db");
        DatabaseProvider(Arc::new(env))
    }

    fn module(
        config: &Config,
        path: std::path::PathBuf,
        ledger_id: u32,
        slot_index: usize,
    ) -> Arc<StorageModule> {
        let pa = PartitionAssignment {
            ledger_id: Some(ledger_id),
            slot_index: Some(slot_index),
            miner_address: IrysAddress::from([0x11; 20]),
            partition_hash: H256::repeat_byte(0x44),
        };
        Arc::new(
            StorageModule::new(
                &StorageModuleInfo {
                    id: 3,
                    partition_assignment: Some(pa),
                    submodules: vec![(partition_chunk_offset_ie!(0, 4), path)],
                },
                config,
            )
            .expect("storage module"),
        )
    }

    fn set_live_assignment(sm: &StorageModule, ledger_id: Option<u32>, slot_index: Option<usize>) {
        let mut guard = sm.partition_assignment.write().unwrap();
        let mut assignment = guard.expect("assignment");
        assignment.ledger_id = ledger_id;
        assignment.slot_index = slot_index;
        *guard = Some(assignment);
    }

    struct Built {
        inner: DataSyncServiceInner,
        sm: Arc<StorageModule>,
        /// `(ledger, fetcher allocation)` in factory-call order.
        built: Arc<Mutex<Vec<(u32, usize)>>>,
    }

    fn build(base: &std::path::Path, ledger_id: u32, slot_index: usize) -> Built {
        let config = test_config(base);
        let sm = module(&config, base.join("chunks"), ledger_id, slot_index);
        let built = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&built);
        let factory: ChunkFetcherFactory = Box::new(move |ledger| {
            let fetcher: Arc<dyn ChunkFetcher> = Arc::new(MockChunkFetcher::new(ledger as usize));
            let addr = Arc::as_ptr(&fetcher) as *const () as usize;
            seen.lock().unwrap().push((ledger, addr));
            fetcher
        });
        let (service_senders, _receivers) = ServiceSenders::new();
        let genesis = signed_genesis();
        let block_tree = BlockTree::new(&genesis, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let peer_list = PeerList::test_mock().expect("peer list");
        let inner = DataSyncServiceInner::new(
            block_tree_guard,
            Arc::new(RwLock::new(vec![Arc::clone(&sm)])),
            peer_list,
            open_db(base),
            factory,
            service_senders,
            config,
            tokio::runtime::Handle::current(),
        );
        Built { inner, sm, built }
    }

    fn fetcher_addr(inner: &DataSyncServiceInner, sm_id: usize) -> usize {
        inner.chunk_orchestrators[&sm_id].test_fetcher_addr()
    }

    /// Same-ledger slot moves keep the orchestrator. A ledger change drops it
    /// and builds one whose fetcher uses the new ledger. Capacity drops it.
    #[tokio::test]
    async fn ledger_change_rebuilds_the_orchestrator_and_same_ledger_keeps_it() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let submit = u32::from(DataLedger::Submit);
        let publish = u32::from(DataLedger::Publish);
        let mut built = build(tmp.path(), submit, 0);
        let sm_id = built.sm.id;
        assert_eq!(built.inner.chunk_orchestrators[&sm_id].ledger_id(), submit);
        let original = fetcher_addr(&built.inner, sm_id);
        assert_eq!(
            built.built.lock().unwrap().as_slice(),
            &[(submit, original)]
        );

        let offset = PartitionChunkOffset::from(2);
        built
            .inner
            .chunk_orchestrators
            .get_mut(&sm_id)
            .unwrap()
            .chunk_requests
            .insert(
                offset,
                ChunkRequest {
                    excluded: Default::default(),
                    request_state: ChunkRequestState::Pending,
                    attempt_count: 4,
                    retry_round: 1,
                    ready_since: Instant::now(),
                    local_cache_miss: false,
                },
            );

        set_live_assignment(&built.sm, Some(submit), Some(1));
        built
            .inner
            .handle_message(DataSyncServiceMessage::SyncPartitions)
            .await
            .expect("same-ledger sync");
        assert_eq!(
            built.inner.chunk_orchestrators[&sm_id].ledger_id(),
            submit,
            "a same-ledger slot change keeps the orchestrator"
        );
        assert!(
            built.inner.chunk_orchestrators[&sm_id]
                .chunk_requests
                .contains_key(&offset),
            "in-flight requests survive a same-ledger slot change"
        );
        assert_eq!(
            fetcher_addr(&built.inner, sm_id),
            original,
            "a same-ledger slot change does not build a new fetcher"
        );

        set_live_assignment(&built.sm, Some(publish), Some(1));
        built
            .inner
            .handle_message(DataSyncServiceMessage::SyncPartitions)
            .await
            .expect("ledger change sync");
        assert_eq!(
            built.inner.chunk_orchestrators[&sm_id].ledger_id(),
            publish,
            "a ledger change rebuilds the orchestrator"
        );
        assert!(
            !built.inner.chunk_orchestrators[&sm_id]
                .chunk_requests
                .contains_key(&offset),
            "the previous ledger's requests are not reused"
        );
        let made = built.built.lock().unwrap().clone();
        assert_eq!(made.len(), 2, "the new ledger builds one fetcher: {made:?}");
        assert_eq!(made[1].0, publish);
        assert_eq!(fetcher_addr(&built.inner, sm_id), made[1].1);

        set_live_assignment(&built.sm, None, None);
        built
            .inner
            .handle_message(DataSyncServiceMessage::SyncPartitions)
            .await
            .expect("capacity sync");
        assert!(
            !built.inner.chunk_orchestrators.contains_key(&sm_id),
            "a ledger-to-capacity assignment drops the orchestrator"
        );
    }

    fn peer_item(address: IrysAddress, port: u16) -> PeerListItem {
        let api =
            std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port);
        PeerListItem {
            peer_id: IrysPeerId::from(address),
            mining_address: address,
            address: PeerAddress {
                gossip: api,
                api,
                ..Default::default()
            },
            reputation_score: PeerScore::new(PeerScore::INITIAL),
            is_online: true,
            protocol_version: ProtocolVersion::default(),
            ..Default::default()
        }
    }

    fn install_epoch(guard: &irys_domain::BlockTreeReadGuard, assignments: &[PartitionAssignment]) {
        let tip = guard.read().tip;
        let mut tree = guard.write();
        let entry = tree.blocks.get_mut(&tip).expect("genesis entry");
        let mut snapshot = irys_domain::EpochSnapshot::default();
        for assignment in assignments {
            snapshot
                .partition_assignments
                .data_partitions
                .insert(assignment.partition_hash, *assignment);
        }
        entry.epoch_snapshot = Arc::new(snapshot);
    }

    /// A peer that stays connected across epochs keeps only the slots the
    /// current epoch snapshot still has for that miner.
    #[tokio::test]
    async fn peer_assignments_follow_the_current_epoch() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let config = test_config(tmp.path());
        let assignee = IrysAddress::from([0xA5; 20]);
        let ledger = u32::from(DataLedger::Submit);
        let slot0 = PartitionAssignment {
            ledger_id: Some(ledger),
            slot_index: Some(0),
            miner_address: assignee,
            partition_hash: H256::repeat_byte(0x21),
        };
        let sm = Arc::new(
            StorageModule::new(
                &StorageModuleInfo {
                    id: 4,
                    partition_assignment: Some(slot0),
                    submodules: vec![(partition_chunk_offset_ie!(0, 4), tmp.path().join("chunks"))],
                },
                &config,
            )
            .expect("storage module"),
        );
        sm.pack_with_zeros();

        let (service_senders, _receivers) = ServiceSenders::new();
        let peer_list = PeerList::from_peers(
            vec![peer_item(assignee, 19191)],
            service_senders.peer_network.clone(),
            &config,
            tokio::sync::broadcast::channel(8).0,
        )
        .expect("peer list");
        let genesis = signed_genesis();
        let block_tree = BlockTree::new(&genesis, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        install_epoch(&block_tree_guard, &[slot0]);

        let mut inner = DataSyncServiceInner::new(
            block_tree_guard.clone(),
            Arc::new(RwLock::new(vec![sm])),
            peer_list,
            open_db(tmp.path()),
            Box::new(|ledger_id| Arc::new(MockChunkFetcher::new(ledger_id as usize))),
            service_senders,
            config,
            tokio::runtime::Handle::current(),
        );

        {
            let managers = inner.active_peer_bandwidth_managers.read().unwrap();
            let manager = managers
                .get(&assignee)
                .expect("the current assignee has a bandwidth manager");
            assert!(manager.is_assigned_to(ledger, Some(0)));
            assert!(!manager.is_assigned_to(ledger, Some(1)));
        }

        let slot1 = PartitionAssignment {
            ledger_id: Some(ledger),
            slot_index: Some(1),
            miner_address: assignee,
            partition_hash: H256::repeat_byte(0x22),
        };
        install_epoch(&block_tree_guard, &[slot1]);
        inner
            .handle_message(DataSyncServiceMessage::SyncPartitions)
            .await
            .expect("epoch sync");

        let managers = inner.active_peer_bandwidth_managers.read().unwrap();
        let manager = managers
            .get(&assignee)
            .expect("a connected peer keeps its bandwidth manager");
        assert!(
            !manager.is_assigned_to(ledger, Some(0)),
            "the slot the peer left must not stay preferred: {:?}",
            manager.partition_assignments
        );
        assert!(
            manager.is_assigned_to(ledger, Some(1)),
            "the slot the epoch still assigns is recorded: {:?}",
            manager.partition_assignments
        );
        assert_eq!(manager.partition_assignments.len(), 1);
    }

    fn peer_slot_counts(inner: &DataSyncServiceInner, peer: IrysAddress) -> (usize, u32, u32, u64) {
        let managers = inner.active_peer_bandwidth_managers.read().unwrap();
        let manager = managers
            .get(&peer)
            .expect("the connected peer keeps its bandwidth manager");
        (
            manager.active_requests(),
            manager.available_concurrency(),
            manager.consecutive_failures(),
            manager.total_failures(),
        )
    }

    fn insert_request(
        inner: &mut DataSyncServiceInner,
        sm_id: usize,
        offset: u32,
        state: ChunkRequestState,
    ) {
        inner
            .chunk_orchestrators
            .get_mut(&sm_id)
            .expect("orchestrator")
            .chunk_requests
            .insert(
                PartitionChunkOffset::from(offset),
                ChunkRequest {
                    excluded: Default::default(),
                    request_state: state,
                    attempt_count: 1,
                    retry_round: 0,
                    ready_since: Instant::now(),
                    local_cache_miss: false,
                },
            );
    }

    /// A ledger change drops the orchestrator. In-flight `Requested` rows still
    /// hold peer slots. Releasing those slots must not penalize the peer, must
    /// leave a slot another module still holds, and must not release a row
    /// whose completion already left `Requested`. A same-ledger slot change
    /// keeps the orchestrator and the slots. A later completion against the
    /// replacement orchestrator must not release the slot again.
    #[tokio::test]
    async fn ledger_change_releases_in_flight_peer_slots() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let config = test_config(tmp.path());
        let assignee = IrysAddress::from([0xA6; 20]);
        let unknown = IrysAddress::from([0xA7; 20]);
        let submit = u32::from(DataLedger::Submit);
        let publish = u32::from(DataLedger::Publish);
        let slot0 = PartitionAssignment {
            ledger_id: Some(submit),
            slot_index: Some(0),
            miner_address: assignee,
            partition_hash: H256::repeat_byte(0x31),
        };
        let sm = Arc::new(
            StorageModule::new(
                &StorageModuleInfo {
                    id: 5,
                    partition_assignment: Some(slot0),
                    submodules: vec![(partition_chunk_offset_ie!(0, 4), tmp.path().join("chunks"))],
                },
                &config,
            )
            .expect("storage module"),
        );
        sm.pack_with_zeros();

        let (service_senders, _receivers) = ServiceSenders::new();
        let peer_list = PeerList::from_peers(
            vec![peer_item(assignee, 19192)],
            service_senders.peer_network.clone(),
            &config,
            tokio::sync::broadcast::channel(8).0,
        )
        .expect("peer list");
        let genesis = signed_genesis();
        let block_tree = BlockTree::new(&genesis, config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        install_epoch(&block_tree_guard, &[slot0]);

        let mut inner = DataSyncServiceInner::new(
            block_tree_guard.clone(),
            Arc::new(RwLock::new(vec![Arc::clone(&sm)])),
            peer_list,
            open_db(tmp.path()),
            Box::new(|ledger_id| Arc::new(MockChunkFetcher::new(ledger_id as usize))),
            service_senders,
            config,
            tokio::runtime::Handle::current(),
        );
        let sm_id = sm.id;
        assert!(inner.chunk_orchestrators.contains_key(&sm_id));

        // A live failure still releases its own slot and still penalizes the peer.
        insert_request(
            &mut inner,
            sm_id,
            0,
            ChunkRequestState::Requested(assignee, Instant::now()),
        );
        inner
            .active_peer_bandwidth_managers
            .write()
            .unwrap()
            .get_mut(&assignee)
            .unwrap()
            .on_chunk_request_started();
        inner
            .chunk_orchestrators
            .get_mut(&sm_id)
            .unwrap()
            .on_chunk_failed(
                PartitionChunkOffset::from(0),
                assignee,
                super::chunk_fetcher::ChunkFetchFailureKind::Timeout,
            )
            .expect("a live failure still completes");
        assert!(
            matches!(
                inner.chunk_orchestrators[&sm_id].chunk_requests[&PartitionChunkOffset::from(0)]
                    .request_state,
                ChunkRequestState::Delayed { .. }
            ),
            "the only peer failed, so the offset waits instead of staying Requested"
        );
        let (active, allowance, failures, total_failures) = peer_slot_counts(&inner, assignee);
        assert_eq!(active, 0, "a live failure releases its slot");
        assert_eq!(
            failures, 1,
            "a live transport failure still penalizes the peer"
        );
        assert_eq!(total_failures, 1);
        assert!(
            allowance > 1,
            "the penalized allowance must still leave a sibling slot, got {allowance}"
        );

        for offset in 1..=allowance {
            insert_request(
                &mut inner,
                sm_id,
                offset,
                ChunkRequestState::Requested(assignee, Instant::now()),
            );
            inner
                .active_peer_bandwidth_managers
                .write()
                .unwrap()
                .get_mut(&assignee)
                .unwrap()
                .on_chunk_request_started();
        }
        // Another module on this node holds one slot. This orchestrator must
        // not release it.
        inner
            .active_peer_bandwidth_managers
            .write()
            .unwrap()
            .get_mut(&assignee)
            .unwrap()
            .on_chunk_request_started();
        let saturated = usize::try_from(allowance).expect("allowance fits usize") + 1;
        assert_eq!(
            peer_slot_counts(&inner, assignee),
            (saturated, 0, 1, 1),
            "the in-flight set fills the allowance"
        );

        set_live_assignment(&sm, Some(submit), Some(1));
        inner
            .handle_message(DataSyncServiceMessage::SyncPartitions)
            .await
            .expect("same-ledger sync");
        assert!(
            inner.chunk_orchestrators[&sm_id]
                .chunk_requests
                .contains_key(&PartitionChunkOffset::from(1)),
            "a same-ledger slot change keeps the in-flight requests"
        );
        assert_eq!(
            peer_slot_counts(&inner, assignee),
            (saturated, 0, 1, 1),
            "a same-ledger slot change keeps the peer slots"
        );

        // These rows do not hold a slot. A delivery that already ran has left
        // `Requested`. The drop must not release them.
        insert_request(
            &mut inner,
            sm_id,
            1_000,
            ChunkRequestState::AwaitingDurability(Instant::now()),
        );
        insert_request(
            &mut inner,
            sm_id,
            1_005,
            ChunkRequestState::Requested(unknown, Instant::now()),
        );
        insert_request(&mut inner, sm_id, 1_001, ChunkRequestState::Completed);
        insert_request(
            &mut inner,
            sm_id,
            1_002,
            ChunkRequestState::Delayed {
                retry_at: Instant::now(),
            },
        );
        insert_request(
            &mut inner,
            sm_id,
            1_003,
            ChunkRequestState::Blocked(crate::data_sync_service::chunk_orchestrator::ChunkBlockReason::MissingDataRootIndex),
        );
        insert_request(
            &mut inner,
            sm_id,
            1_004,
            ChunkRequestState::HeldByBodyWorker,
        );

        let publish_slot = PartitionAssignment {
            ledger_id: Some(publish),
            slot_index: Some(1),
            miner_address: assignee,
            partition_hash: H256::repeat_byte(0x32),
        };
        install_epoch(&block_tree_guard, &[publish_slot]);
        set_live_assignment(&sm, Some(publish), Some(1));
        inner
            .handle_message(DataSyncServiceMessage::SyncPartitions)
            .await
            .expect("ledger change");
        assert!(
            inner.chunk_orchestrators[&sm_id].chunk_requests.is_empty(),
            "the replacement orchestrator does not keep the old requests"
        );
        assert_eq!(
            peer_slot_counts(&inner, assignee),
            (1, allowance - 1, 1, 1),
            "the dropped orchestrator releases only the slots it still held"
        );

        let late = PartitionChunkOffset::from(2);
        let fetched = inner
            .chunk_orchestrators
            .get_mut(&sm_id)
            .unwrap()
            .on_chunk_fetched(late, assignee);
        assert!(
            fetched.is_err(),
            "a late delivery does not find the dropped request"
        );
        let failed = inner
            .chunk_orchestrators
            .get_mut(&sm_id)
            .unwrap()
            .on_chunk_failed(
                late,
                assignee,
                super::chunk_fetcher::ChunkFetchFailureKind::Timeout,
            );
        assert!(
            failed.is_err(),
            "a late failure does not find the dropped request"
        );
        assert_eq!(
            peer_slot_counts(&inner, assignee),
            (1, allowance - 1, 1, 1),
            "a late completion must not release the sibling slot or add a failure"
        );

        // Capacity removes the orchestrator. The slot this replacement still
        // holds is released. A completion with no orchestrator does not
        // release anything else.
        insert_request(
            &mut inner,
            sm_id,
            7,
            ChunkRequestState::Requested(assignee, Instant::now()),
        );
        inner
            .active_peer_bandwidth_managers
            .write()
            .unwrap()
            .get_mut(&assignee)
            .unwrap()
            .on_chunk_request_started();
        assert_eq!(peer_slot_counts(&inner, assignee).0, 2);
        set_live_assignment(&sm, None, None);
        inner
            .handle_message(DataSyncServiceMessage::SyncPartitions)
            .await
            .expect("capacity sync");
        assert!(
            !inner.chunk_orchestrators.contains_key(&sm_id),
            "capacity drops the orchestrator"
        );
        assert_eq!(
            peer_slot_counts(&inner, assignee),
            (1, allowance - 1, 1, 1),
            "capacity releases the in-flight slot and leaves the sibling slot"
        );
        inner
            .handle_message(DataSyncServiceMessage::ChunkFailed {
                storage_module_id: sm_id,
                chunk_offset: PartitionChunkOffset::from(7),
                peer_addr: assignee,
                failure_kind: super::chunk_fetcher::ChunkFetchFailureKind::Timeout,
            })
            .await
            .expect("a completion with no orchestrator is ignored");
        assert_eq!(
            peer_slot_counts(&inner, assignee),
            (1, allowance - 1, 1, 1),
            "a late failure with no orchestrator must not change the peer"
        );
    }
}
