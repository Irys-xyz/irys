pub mod assignee_intervals;
pub mod chunk_fetcher;
pub mod chunk_orchestrator;
pub mod peer_bandwidth_manager;
pub mod peer_stats;

use crate::{
    chunk_fetcher::ChunkFetcherFactory,
    chunk_ingress_service::{
        AdvisoryChunkIngressError, ChunkIngressError, CriticalChunkIngressError,
        facade::ChunkIngressFacadeImpl,
    },
    metrics,
    services::ServiceSenders,
};
use chunk_fetcher::ChunkFetchFailureKind;
use chunk_orchestrator::{ChunkBlockReason, ChunkOrchestrator, ChunkRequestState};
use futures::FutureExt as _;
use irys_database::db::IrysDatabaseExt as _;
use irys_database::ingress_proofs_by_data_root;
use irys_domain::{BatchEnqueueItem, BlockTreeReadGuard, ChunkType, PeerList, StorageModule};
use irys_packing::unpack;
use irys_types::{
    ChunkFormat, Config, DataLedger, DataRoot, IrysAddress, PartitionChunkOffset, SendTraced as _,
    TokioServiceHandle, Traced, TxChunkOffset, UnpackedChunk, app_state::DatabaseProvider,
};
use peer_bandwidth_manager::PeerBandwidthManager;
use reth::tasks::shutdown::Shutdown;
use std::{
    collections::{HashMap, HashSet},
    panic::AssertUnwindSafe,
    sync::{
        Arc, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
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

/// Bodies data sync may have in the ingress service at once, across every
/// peer and storage module. The ingress chunk lane is
/// `max_concurrent_chunk_ingress_tasks` less the control-plane share (26 on
/// the presets), and gossip and HTTP uploads share it. Data sync stays under
/// that lane instead of drawing `Overloaded` replies.
const DATA_SYNC_INGRESS_PERMITS: usize = 20;

/// Delivered bodies held between arrival and their ingress result. At this
/// many, `tick` starts no fetch, so a slow unpack or ingress backs up to the
/// network instead of into memory.
const MAX_DELIVERED_BODIES: usize = 2 * DATA_SYNC_INGRESS_PERMITS;

/// A delivered body that has not finished unpack, validation, and ingress in
/// this long goes back to the queue. Nothing else would release it.
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(60);

/// Wait before the first resend of a body that ingress answered `Overloaded`.
/// Each resend doubles it up to [`INGRESS_OVERLOADED_BACKOFF_MAX`].
const INGRESS_OVERLOADED_BACKOFF: Duration = Duration::from_millis(50);
const INGRESS_OVERLOADED_BACKOFF_MAX: Duration = Duration::from_secs(1);

/// Why a delivered body was not accepted.
#[derive(Debug)]
pub enum DeliveryError {
    /// The peer's bytes are wrong for this chunk. The peer is penalized.
    InvalidBody(String),
    /// Ingress rejected the body. [`ingress_error_is_invalid_peer_data`]
    /// decides whether the peer is at fault.
    Ingress(ChunkIngressError),
    /// The node failed, not the peer: an unpack panic or a timeout. The
    /// offset is requeued without blaming the peer.
    Local(String),
    /// Direct route: the body names a different data root, tx offset, or data
    /// size than the placement captured when it arrived. The peer is at fault
    /// only if that placement is still current; a rollback in between means
    /// the peer may have served the new chunk correctly.
    PlacementMismatch {
        expected: (DataRoot, TxChunkOffset, u64),
        detail: String,
    },
}

impl DeliveryError {
    fn is_peer_fault(&self) -> bool {
        match self {
            Self::InvalidBody(_) | Self::PlacementMismatch { .. } => true,
            Self::Ingress(error) => ingress_error_is_invalid_peer_data(error),
            Self::Local(_) => false,
        }
    }
}

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidBody(error) => write!(f, "invalid peer chunk: {error}"),
            Self::Ingress(error) => write!(f, "chunk ingress rejected the body: {error}"),
            Self::Local(error) => write!(f, "{error}"),
            Self::PlacementMismatch { detail, .. } => write!(f, "invalid peer chunk: {detail}"),
        }
    }
}

/// How a delivered body reaches the storage-module write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryRoute {
    /// Through chunk ingress, which commits the body and its compact leaf
    /// before the storage module may make the offset durable. Submit data
    /// needs that leaf: this node may sign an ingress proof to promote it.
    Ingress,
    /// Straight to the storage-module write, skipping the ingress lane, the
    /// chunk cache, gossip, and the ingress-proof attempt. No ingress proof
    /// can be needed: term-ledger data never takes one, and Publish data is
    /// already promoted. The body must match this offset's local placement.
    Direct {
        data_root: DataRoot,
        tx_offset: TxChunkOffset,
        data_size: u64,
    },
}

/// Check a body for the direct route: it must be the chunk indexed at the
/// requested offset, and its bytes must match its data path. With the
/// placement fields equal, the data-path check runs against the local data
/// root, tx offset, and data size.
fn check_against_placement(
    unpacked: UnpackedChunk,
    data_root: DataRoot,
    tx_offset: TxChunkOffset,
    data_size: u64,
    chunk_size: u64,
) -> Result<UnpackedChunk, DeliveryError> {
    if unpacked.data_root != data_root
        || unpacked.tx_offset != tx_offset
        || unpacked.data_size != data_size
    {
        return Err(DeliveryError::PlacementMismatch {
            expected: (data_root, tx_offset, data_size),
            detail: format!(
                "fetched data_root {} tx_offset {} data_size {} does not match the local data_root {data_root} tx_offset {tx_offset} data_size {data_size}",
                unpacked.data_root, unpacked.tx_offset, unpacked.data_size,
            ),
        });
    }
    validate_peer_unpacked_chunk(&unpacked, chunk_size)
        .map_err(|error| DeliveryError::InvalidBody(error.to_string()))?;
    Ok(unpacked)
}

/// Everything the task that carries one delivered body needs.
#[derive(Clone)]
struct DeliveryPipeline {
    service_senders: ServiceSenders,
    /// Packed bodies unpacked at once on the blocking pool. One per core.
    unpack_permits: Arc<tokio::sync::Semaphore>,
    /// See [`DATA_SYNC_INGRESS_PERMITS`].
    ingress_permits: Arc<tokio::sync::Semaphore>,
    /// Delivered bodies whose task has not finished.
    held_bodies: Arc<AtomicUsize>,
    entropy_packing_iterations: u32,
    chunk_size: u64,
    chain_id: u64,
    /// [`DELIVERY_TIMEOUT`] outside tests.
    timeout: Duration,
}

/// Counts one held body until dropped, including when its task is dropped.
struct HeldBody(Arc<AtomicUsize>);

impl HeldBody {
    fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(count))
    }
}

impl Drop for HeldBody {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Unpack a peer body on the blocking pool. At mainnet packing iterations
/// one 256 KiB chunk is tens of milliseconds of hashing, which must not hold
/// the data-sync loop.
async fn unpack_off_loop(
    chunk: ChunkFormat,
    pipeline: &DeliveryPipeline,
) -> Result<UnpackedChunk, DeliveryError> {
    let chunk_size = usize::try_from(pipeline.chunk_size)
        .map_err(|_| DeliveryError::Local("chunk size exceeds usize".to_string()))?;
    // Entropy is `chunk_size` bytes. A longer body is the peer's data, rejected
    // before the XOR indexes that buffer.
    let body_len = match &chunk {
        ChunkFormat::Unpacked(unpacked) => unpacked.bytes.0.len(),
        ChunkFormat::Packed(packed) => packed.bytes.0.len(),
    };
    if body_len > chunk_size {
        return Err(DeliveryError::InvalidBody(format!(
            "peer chunk is {body_len} bytes, more than the {chunk_size}-byte chunk size"
        )));
    }
    let packed = match chunk {
        ChunkFormat::Unpacked(unpacked) => return Ok(unpacked),
        ChunkFormat::Packed(packed) => packed,
    };
    let unpack_failed =
        |error: String| DeliveryError::Local(format!("could not unpack the peer chunk: {error}"));
    // The semaphore is never closed. The permit is held until the blocking
    // unpack returns.
    let _permit = Arc::clone(&pipeline.unpack_permits)
        .acquire_owned()
        .await
        .map_err(|error| unpack_failed(error.to_string()))?;
    let (iterations, chain_id) = (pipeline.entropy_packing_iterations, pipeline.chain_id);
    let unpacked =
        tokio::task::spawn_blocking(move || unpack(&packed, iterations, chunk_size, chain_id))
            .await
            .map_err(|error| unpack_failed(error.to_string()))?;
    unpacked.map_err(|error| DeliveryError::InvalidBody(error.to_string()))
}

/// Unpack, then check the body. On the direct route it is checked against
/// the local placement and returned for the storage-module write.
///
/// On the ingress route it is checked against its own data path, then
/// handed to ingress. Ingress can accept a body it did not hash, so the path
/// check runs first and an invalid body never reaches the ingress cache.
/// The ingress acknowledgement is the durable compact-leaf fence: the
/// storage module may not make this fetch permanently complete until
/// ingress has committed the cached body and signer-specific ingress hash.
/// `Overloaded` means the shared lane is full, not that the body is bad, so
/// the same body is sent again after a short wait.
async fn process_delivered_body(
    chunk: ChunkFormat,
    route: DeliveryRoute,
    pipeline: &DeliveryPipeline,
) -> Result<UnpackedChunk, DeliveryError> {
    let unpacked = unpack_off_loop(chunk, pipeline).await?;
    if let DeliveryRoute::Direct {
        data_root,
        tx_offset,
        data_size,
    } = route
    {
        return check_against_placement(
            unpacked,
            data_root,
            tx_offset,
            data_size,
            pipeline.chunk_size,
        );
    }
    validate_peer_unpacked_chunk(&unpacked, pipeline.chunk_size)
        .map_err(|error| DeliveryError::InvalidBody(error.to_string()))?;
    let _permit = Arc::clone(&pipeline.ingress_permits)
        .acquire_owned()
        .await
        .map_err(|error| DeliveryError::Local(error.to_string()))?;
    let mut backoff = INGRESS_OVERLOADED_BACKOFF;
    loop {
        match forward_chunk_to_ingress(&pipeline.service_senders, unpacked.clone()).await {
            Ok(()) => return Ok(unpacked),
            Err(ChunkIngressError::Advisory(AdvisoryChunkIngressError::Overloaded)) => {
                tokio::time::sleep(backoff).await;
                backoff = backoff
                    .saturating_mul(2)
                    .min(INGRESS_OVERLOADED_BACKOFF_MAX);
            }
            Err(error) => return Err(DeliveryError::Ingress(error)),
        }
    }
}

/// Run a delivery to its result. A delivery that takes longer than `timeout`
/// or panics still yields a local error, so the offset waiting on it goes back
/// to pending. Without a result the offset would stay requested for good:
/// it holds no peer slot, so no slot sweep or peer removal touches it.
async fn settle_delivery(
    delivery: impl Future<Output = Result<UnpackedChunk, DeliveryError>>,
    timeout: Duration,
) -> Result<UnpackedChunk, DeliveryError> {
    match AssertUnwindSafe(tokio::time::timeout(timeout, delivery))
        .catch_unwind()
        .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(_elapsed)) => Err(DeliveryError::Local(format!(
            "unpack, validation, and ingress took longer than {timeout:?}"
        ))),
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic payload");
            Err(DeliveryError::Local(format!(
                "delivery task panicked: {message}"
            )))
        }
    }
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
    /// The previous successful refill started a term-ledger module. The next
    /// refill tries Publish first.
    last_refill_was_term: bool,
    /// Unpack, validation, and ingress for delivered bodies.
    delivery: DeliveryPipeline,
    /// See [`MAX_DELIVERED_BODIES`].
    max_delivered_bodies: usize,
    /// Dispatch into a slot as soon as its request finishes. Tests that count
    /// slots across a completion turn it off.
    #[cfg(test)]
    refill_on_release: bool,
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

/// Term-ledger SMs (Submit / OneYear / ThirtyDay) take one turn before Publish
/// in each pass, and the pass repeats while any module dispatched. One start
/// per module per pass: a run of starts on every term module can fill the
/// shared peer slots before Publish is visited. Publish copies Submit data for
/// promotion; if Submit replicas stay on the write frontier, Publish has many
/// sources. When shared peer permits are fewer than the number of SMs,
/// term-first keeps a busy permanent backlog from skipping the term write head.
/// Within each group the start index still rotates so two Submit SMs share fairly.
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
            if dispatch(id) {
                dispatched = true;
            }
        }
        for id in publish_ids.iter().copied() {
            if dispatch(id) {
                dispatched = true;
            }
        }
        if !dispatched {
            break;
        }
    }
    (next_term, next_publish)
}

/// One freed peer slot. The tick stays term-first. A refill alternates, so a
/// peer shared by term and Publish modules does not give every freed slot
/// back to term. `last_refill_was_term` tries Publish first, then term.
/// Otherwise term is first. An idle group falls through. The first module
/// that starts wins. `Some(true)` means that start was a term module.
fn dispatch_refill<T: Copy>(
    term_ids: &[T],
    publish_ids: &[T],
    last_refill_was_term: bool,
    mut dispatch: impl FnMut(T) -> bool,
) -> Option<bool> {
    let groups: [(&[T], bool); 2] = if last_refill_was_term {
        [(publish_ids, false), (term_ids, true)]
    } else {
        [(term_ids, true), (publish_ids, false)]
    };
    for (ids, is_term) in groups {
        for id in ids.iter().copied() {
            if dispatch(id) {
                return Some(is_term);
            }
        }
    }
    None
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
    use super::{dispatch_refill, dispatch_term_then_publish};

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

    #[test]
    fn each_pass_gives_every_module_one_dispatch() {
        // Two term modules and one Publish module, five shared permits.
        // Each pass gives every module one start, term before Publish.
        // The first term module must not take a run of those permits.
        let mut term = [1_u8, 4_u8];
        let mut publish = [0_u8];
        let mut permits = 5_usize;
        let mut order = Vec::new();
        let _ = dispatch_term_then_publish(&mut term, &mut publish, 0, 0, |id| {
            if permits == 0 {
                return false;
            }
            permits -= 1;
            order.push(id);
            true
        });
        assert_eq!(order, vec![1, 4, 0, 1, 4]);
    }

    #[test]
    fn publish_gets_a_dispatch_when_three_term_modules_exhaust_the_budget() {
        // Three term modules with endless work and one Publish module. The
        // shared budget is a fresh peer's baseline slot count. Publish must
        // be dispatched before that budget runs out.
        let mut term = [1_u8, 2_u8, 3_u8];
        let mut publish = [0_u8];
        let mut budget = 10_usize;
        let mut order = Vec::new();
        let _ = dispatch_term_then_publish(&mut term, &mut publish, 0, 0, |id| {
            if budget == 0 {
                return false;
            }
            budget -= 1;
            order.push(id);
            true
        });
        assert_eq!(order, vec![1, 2, 3, 0, 1, 2, 3, 0, 1, 2]);
    }

    #[test]
    fn release_refill_dispatches_publish_while_term_work_is_endless() {
        // One shared slot. Each call is one refill: the release path starts
        // one fetch and returns. Term modules always accept. Publish must
        // still be started across those refills.
        let term = [1_u8, 2_u8];
        let publish = [0_u8];
        let mut last_was_term = false;
        let mut order = Vec::new();
        for _ in 0..4 {
            let mut budget = 1_usize;
            let started = dispatch_refill(&term, &publish, last_was_term, |id| {
                if budget == 0 {
                    return false;
                }
                budget -= 1;
                order.push(id);
                true
            });
            last_was_term = started.expect("a module accepted the slot");
        }
        assert_eq!(order, vec![1, 0, 1, 0]);
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
    /// The task carrying a `ChunkCompleted` body finished unpack, validation,
    /// and ingress. The peer slot was released when the body arrived. This
    /// checks placement and makes the storage-module write.
    ChunkIngressed {
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_address: IrysAddress,
        /// Start of the attempt this body answered. A result for another
        /// attempt is ignored.
        requested_at: Instant,
        result: Result<UnpackedChunk, DeliveryError>,
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
    /// An assignee's Data intervals for one window, fetched so the scan can
    /// skip local Entropy no assignee holds. `head` is the write head when the
    /// fetch was planned.
    AssigneeIntervalsFetched {
        storage_module_id: usize,
        fetch: assignee_intervals::WindowFetch,
        head: u32,
        result: Result<chunk_fetcher::DataIntervalsPage, chunk_fetcher::ChunkFetchError>,
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
        let delivery = DeliveryPipeline {
            service_senders: service_senders.clone(),
            unpack_permits: Arc::new(tokio::sync::Semaphore::new(
                std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
            )),
            ingress_permits: Arc::new(tokio::sync::Semaphore::new(DATA_SYNC_INGRESS_PERMITS)),
            held_bodies: Arc::new(AtomicUsize::new(0)),
            entropy_packing_iterations: config.consensus.entropy_packing_iterations,
            chunk_size: config.consensus.chunk_size,
            chain_id: config.consensus.chain_id,
            timeout: DELIVERY_TIMEOUT,
        };
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
            last_refill_was_term: false,
            delivery,
            max_delivered_bodies: MAX_DELIVERED_BODIES,
            #[cfg(test)]
            refill_on_release: true,
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
            } => match self.on_chunk_completed(storage_module_id, chunk_offset, peer_addr, chunk) {
                Ok(true) => self.dispatch_after_release(),
                Ok(false) => {}
                Err(e) => error!(
                    storage_module.id = storage_module_id,
                    chunk.offset = %chunk_offset,
                    peer.address = %peer_addr,
                    "Failed to handle chunk completion: {e:?}"
                ),
            },
            DataSyncServiceMessage::ChunkIngressed {
                storage_module_id,
                chunk_offset,
                peer_address: peer_addr,
                requested_at,
                result,
            } => {
                if let Err(e) = self.on_chunk_ingressed(
                    storage_module_id,
                    chunk_offset,
                    peer_addr,
                    requested_at,
                    result,
                ) {
                    error!(
                        storage_module.id = storage_module_id,
                        chunk.offset = %chunk_offset,
                        peer.address = %peer_addr,
                        "Failed to handle chunk ingress result: {e:?}"
                    );
                }
                // A finished body freed a held-body place. A slot released
                // while that limit was reached is filled now, not at the tick.
                self.dispatch_after_release();
            }
            DataSyncServiceMessage::ChunkFailed {
                storage_module_id,
                chunk_offset,
                peer_addr,
                failure_kind,
            } => {
                metrics::record_data_sync_chunk_failure();
                // A failure that matched a live request released its slot.
                match self.on_chunk_failed(storage_module_id, chunk_offset, peer_addr, failure_kind)
                {
                    Ok(()) => self.dispatch_after_release(),
                    Err(e) => error!(
                        "Failed to handle chunk failure for storage_module {} chunk_offset {} from peer {}: {e:?}",
                        storage_module_id, chunk_offset, peer_addr
                    ),
                }
            }
            DataSyncServiceMessage::GetActivePeersList(tx) => self.handle_get_active_peers_list(tx),
            DataSyncServiceMessage::AssigneeIntervalsFetched {
                storage_module_id,
                fetch,
                head,
                result,
            } => {
                if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
                    orchestrator.on_assignee_intervals(fetch, head, result);
                }
            }
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

    /// A finished request released one peer slot. Use it now: waiting for
    /// the next tick idles that slot for up to 250 ms, even after a 1 ms 404.
    /// One dispatch, under the same held-body limit. Each group starts at its
    /// tick cursor. Publish is tried first when the previous refill started a
    /// term module, so a shared peer does not stay on term work. The tick
    /// still refills each queue, stays term-first, and advances the cursors.
    fn dispatch_after_release(&mut self) {
        #[cfg(test)]
        if !self.refill_on_release {
            return;
        }
        if self.delivery.held_bodies.load(Ordering::SeqCst) >= self.max_delivered_bodies {
            return;
        }
        let mut ids: Vec<_> = self.chunk_orchestrators.keys().copied().collect();
        ids.sort_unstable();
        let (mut term, mut publish): (Vec<_>, Vec<_>) = ids.into_iter().partition(|id| {
            self.chunk_orchestrators
                .get(id)
                .is_some_and(ChunkOrchestrator::prioritizes_write_frontier)
        });
        if !term.is_empty() {
            let start = self.term_dispatch_cursor % term.len();
            term.rotate_left(start);
        }
        if !publish.is_empty() {
            let start = self.publish_dispatch_cursor % publish.len();
            publish.rotate_left(start);
        }
        let started_on_term = dispatch_refill(&term, &publish, self.last_refill_was_term, |id| {
            self.chunk_orchestrators
                .get_mut(&id)
                .is_some_and(ChunkOrchestrator::dispatch_next)
        });
        if let Some(was_term) = started_on_term {
            self.last_refill_was_term = was_term;
        }
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
            // Bodies only join the held set in `handle_message`, so one
            // check covers the whole pass.
            let held = self.delivery.held_bodies.load(Ordering::SeqCst);
            if held >= self.max_delivered_bodies {
                debug!(
                    held_bodies = held,
                    "data sync holds its delivered-body limit; no new fetch this tick"
                );
            } else {
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
            }

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

    /// A peer delivered a body. Release its slot now and carry the body
    /// through unpack, validation, and ingress in a spawned task, so the
    /// service loop keeps ticking and `dispatch_next` can refill the slot. The
    /// offset stays `Requested` until [`DataSyncServiceMessage::ChunkIngressed`].
    ///
    /// A body no request is waiting for (its orchestrator is gone, or the
    /// offset was requeued) still goes to ingress, which writes it to any
    /// module with a writeable offset. No result comes back for it.
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
    ) -> eyre::Result<bool> {
        let requested_at = match self.chunk_orchestrators.get_mut(&storage_module_id) {
            // Releases the peer slot. Validation and ingress can still reject
            // the body; that path counts the failure without a second release.
            Some(orchestrator) => match orchestrator.on_chunk_fetched(chunk_offset, peer_addr) {
                Ok(record) => Some(record.start_time),
                Err(error) => {
                    debug!(
                        storage_module.id = storage_module_id,
                        chunk.offset = %chunk_offset,
                        "no request is waiting for this body; handing it to ingress only: {error}"
                    );
                    None
                }
            },
            // The orchestrator was removed and released its slots.
            None => None,
        };

        // A body no request waits for still goes to ingress, which writes it
        // wherever it belongs. Nothing here would write a direct-route body.
        let route = if requested_at.is_some() {
            self.delivery_route(storage_module_id, chunk_offset)
        } else {
            DeliveryRoute::Ingress
        };
        metrics::record_data_sync_delivery_route(match route {
            DeliveryRoute::Ingress => "ingress",
            DeliveryRoute::Direct { .. } => "direct",
        });
        let pipeline = self.delivery.clone();
        let held = HeldBody::new(&pipeline.held_bodies);
        self.runtime_handle.spawn(
            async move {
                let result = settle_delivery(
                    process_delivered_body(chunk, route, &pipeline),
                    pipeline.timeout,
                )
                .await;
                // Release the held-body place before the result is sent, so
                // the refill that handles it can count this body as gone.
                drop(held);
                let Some(requested_at) = requested_at else {
                    if let Err(error) = result {
                        debug!("untracked data-sync body was not accepted: {error}");
                    }
                    return;
                };
                let _ = pipeline.service_senders.data_sync.send_traced(
                    DataSyncServiceMessage::ChunkIngressed {
                        storage_module_id,
                        chunk_offset,
                        peer_address: peer_addr,
                        requested_at,
                        result,
                    },
                );
            }
            .instrument(tracing::Span::current()),
        );
        // A tracked body released its peer slot in `on_chunk_fetched`.
        Ok(requested_at.is_some())
    }

    /// Direct for Publish and the term ledgers when this offset has a local
    /// placement. Submit, an unknown ledger, and a missing or unreadable
    /// placement use ingress, which keeps the missing-index handling.
    fn delivery_route(
        &self,
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
    ) -> DeliveryRoute {
        let Some(orchestrator) = self.chunk_orchestrators.get(&storage_module_id) else {
            return DeliveryRoute::Ingress;
        };
        match DataLedger::try_from(orchestrator.ledger_id()) {
            Ok(DataLedger::Publish | DataLedger::OneYear | DataLedger::ThirtyDay) => {}
            Ok(DataLedger::Submit) | Err(_) => return DeliveryRoute::Ingress,
        }
        match self.local_placement(storage_module_id, chunk_offset) {
            Some((data_root, tx_offset, data_size)) => DeliveryRoute::Direct {
                data_root,
                tx_offset,
                data_size,
            },
            None => DeliveryRoute::Ingress,
        }
    }

    /// Data root, tx offset, and data size indexed at `chunk_offset`. `None`
    /// when nothing is placed there or the index cannot be read.
    fn local_placement(
        &self,
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
    ) -> Option<(DataRoot, TxChunkOffset, u64)> {
        let sm = storage_module_by_id(&self.storage_modules.read().unwrap(), storage_module_id)?;
        let placement = irys_domain::trace_index_read(irys_domain::DATA_SYNC, || {
            sm.data_root_and_tx_offset_span(chunk_offset, chunk_offset)
        });
        placement.ok()?.into_iter().next().flatten()
    }

    /// Penalize the peer for a body it delivered and retry the offset from
    /// another peer. Its slot was released on arrival and is not touched.
    fn reject_delivered_body(
        &mut self,
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
    ) -> eyre::Result<()> {
        let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) else {
            return Ok(());
        };
        metrics::record_data_sync_chunk_failure();
        metrics::record_data_sync_fetch_failure(
            orchestrator.ledger_id(),
            peer_addr,
            "invalid_chunk",
        );
        orchestrator.on_ingressed_chunk_rejected(chunk_offset, peer_addr)
    }

    /// Apply the result for a body [`Self::on_chunk_completed`] handed off.
    /// The peer slot was released there and is not touched again.
    #[tracing::instrument(level = "trace", skip_all, fields(
        chunk.storage_module_id = storage_module_id,
        chunk.offset = %chunk_offset,
        peer.address = %peer_addr,
    ))]
    fn on_chunk_ingressed(
        &mut self,
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
        requested_at: Instant,
        result: Result<UnpackedChunk, DeliveryError>,
    ) -> eyre::Result<()> {
        // A field mismatch is the peer's fault only if the placement it was
        // checked against is still the one indexed here.
        let placement_moved = match &result {
            Err(DeliveryError::PlacementMismatch { expected, .. }) => {
                self.local_placement(storage_module_id, chunk_offset) != Some(*expected)
            }
            _ => false,
        };
        let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) else {
            debug!(
                storage_module.id = storage_module_id,
                chunk.offset = %chunk_offset,
                "chunk ingress finished for a storage module with no orchestrator"
            );
            return Ok(());
        };
        if !orchestrator.body_awaiting_ingress(chunk_offset, peer_addr, requested_at) {
            // A replaced orchestrator or a later dispatch owns this offset now.
            debug!(
                storage_module.id = storage_module_id,
                chunk.offset = %chunk_offset,
                peer.address = %peer_addr,
                "chunk ingress result no longer matches the request"
            );
            return Ok(());
        }

        let unpacked_chunk = match result {
            Ok(unpacked) => unpacked,
            Err(error) => {
                if placement_moved {
                    orchestrator.requeue_after_local_write_failure(chunk_offset)?;
                    return Err(eyre::eyre!(
                        "the placement at offset {chunk_offset} changed while its body was in delivery; retrying without blaming the peer: {error}"
                    ));
                }
                if error.is_peer_fault() {
                    self.reject_delivered_body(storage_module_id, chunk_offset, peer_addr)?;
                } else {
                    // The peer delivered valid bytes as far as the network is
                    // concerned; local ingress/database pressure must not
                    // penalize that peer, but the offset remains unresolved.
                    orchestrator.requeue_after_local_write_failure(chunk_offset)?;
                }
                return Err(eyre::eyre!(
                    "data sync did not accept the peer body before durable write: {error}"
                ));
            }
        };

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
            self.reject_delivered_body(storage_module_id, chunk_offset, peer_addr)?;
            return Err(eyre::eyre!(
                "data sync rejected peer chunk at offset {chunk_offset}: fetched data_root {} tx_offset {} does not match the local data_root {local_root} tx_offset {local_tx_offset}",
                unpacked_chunk.data_root,
                unpacked_chunk.tx_offset,
            ));
        }

        // The body is accepted. Now the peer earns the success credit: its
        // failure streak clears and its bytes count.
        metrics::record_data_sync_chunk_fetched();
        if let Some(orchestrator) = self.chunk_orchestrators.get(&storage_module_id) {
            orchestrator.credit_accepted_delivery(peer_addr);
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
    use super::{
        DeliveryError, DeliveryPipeline, forward_chunk_to_ingress,
        ingress_error_is_invalid_peer_data, settle_delivery, unpack_off_loop,
    };
    use crate::chunk_ingress_service::{ChunkIngressError, CriticalChunkIngressError};
    use crate::services::ServiceSenders;
    use irys_types::{
        Base64, ChunkFormat, H256, IrysAddress, PackedChunk, PartitionChunkOffset, TxChunkOffset,
        UnpackedChunk,
    };
    use std::{sync::Arc, time::Duration};

    fn pipeline(unpack_permits: usize, chunk_size: u64) -> DeliveryPipeline {
        let (service_senders, _receivers) = ServiceSenders::new();
        DeliveryPipeline {
            service_senders,
            unpack_permits: Arc::new(tokio::sync::Semaphore::new(unpack_permits)),
            ingress_permits: Arc::new(tokio::sync::Semaphore::new(1)),
            held_bodies: Arc::default(),
            entropy_packing_iterations: 1_000,
            chunk_size,
            chain_id: 7,
            timeout: Duration::from_secs(5),
        }
    }

    fn packed_chunk(data: &[u8], chunk_size: usize) -> PackedChunk {
        let packing_address = IrysAddress::from([3_u8; 20]);
        let partition_hash = H256::repeat_byte(0x21);
        let partition_offset = PartitionChunkOffset::from(5_u32);
        let mut entropy = Vec::with_capacity(chunk_size);
        irys_packing::capacity_single::compute_entropy_chunk(
            packing_address,
            u64::from(*partition_offset),
            partition_hash.0,
            1_000,
            chunk_size,
            &mut entropy,
            7,
        );
        PackedChunk {
            data_root: H256::repeat_byte(0x31),
            data_size: chunk_size as u64,
            data_path: Base64(vec![1, 2, 3]),
            bytes: Base64(
                irys_packing::packing_xor_vec_u8(entropy, data)
                    .expect("test data fits the entropy buffer"),
            ),
            tx_offset: TxChunkOffset::from(0_u32),
            packing_address,
            partition_offset,
            partition_hash,
        }
    }

    /// A delivery that panics still yields a local error, which sends the
    /// offset back to pending instead of leaving it requested for good.
    #[tokio::test]
    async fn a_panicking_delivery_settles_to_a_local_error() {
        let delivery = async {
            if true {
                panic!("injected delivery panic");
            }
            Err(DeliveryError::Local("unreachable".to_string()))
        };
        let result = settle_delivery(delivery, Duration::from_secs(5)).await;
        assert!(
            matches!(
                &result,
                Err(DeliveryError::Local(message))
                    if message.contains("delivery task panicked: injected delivery panic")
            ),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_slow_delivery_settles_to_a_local_error() {
        let result = settle_delivery(
            std::future::pending::<Result<UnpackedChunk, DeliveryError>>(),
            Duration::from_millis(10),
        )
        .await;
        assert!(
            matches!(&result, Err(DeliveryError::Local(message)) if message.contains("longer than")),
            "{result:?}"
        );
    }

    /// A packed body is unpacked on the blocking pool, and only while a
    /// permit is free.
    #[tokio::test]
    async fn packed_body_is_unpacked_off_the_loop_under_the_permit_bound() {
        let chunk_size = 64_usize;
        let data: Vec<u8> = (0..chunk_size)
            .map(|i| u8::try_from(i).expect("byte"))
            .collect();
        let packed = packed_chunk(&data, chunk_size);
        let blocked = pipeline(0, chunk_size as u64);
        let permits = Arc::clone(&blocked.unpack_permits);
        let waiting = tokio::spawn({
            let packed = packed.clone();
            async move { unpack_off_loop(ChunkFormat::Packed(packed), &blocked).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished(), "no permit, so no unpack");
        permits.add_permits(1);
        let unpacked = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("a free permit lets the unpack run")
            .expect("task")
            .expect("unpack");
        assert_eq!(unpacked.bytes.0, data);
        assert_eq!(unpacked.data_root, packed.data_root);
        assert_eq!(unpacked.data_path, packed.data_path);
        assert_eq!(
            permits.available_permits(),
            1,
            "the permit is returned after the unpack"
        );

        let already = UnpackedChunk {
            bytes: Base64(data.clone()),
            ..UnpackedChunk::default()
        };
        let passed = unpack_off_loop(
            ChunkFormat::Unpacked(already.clone()),
            &pipeline(0, chunk_size as u64),
        )
        .await
        .expect("an unpacked body needs no permit");
        assert_eq!(passed, already);
    }

    /// A packed body longer than a chunk would index past the entropy. It is
    /// the peer's fault, not a local unpack failure.
    #[tokio::test]
    async fn oversized_packed_body_is_invalid_peer_data() {
        let chunk_size = 64_usize;
        let mut packed = packed_chunk(&[0_u8; 64], chunk_size);
        packed.bytes.0.push(0);
        let error = unpack_off_loop(ChunkFormat::Packed(packed), &pipeline(1, 64))
            .await
            .expect_err("an oversized body is rejected");
        assert!(matches!(error, DeliveryError::InvalidBody(_)), "{error}");
        assert!(error.is_peer_fault());
        assert!(!DeliveryError::Local("timeout".to_string()).is_peer_fault());
    }

    /// An unpacked body longer than a chunk is the peer's fault. It is
    /// rejected before any unpack work, including when no unpack permit is free.
    #[tokio::test]
    async fn oversized_unpacked_body_is_invalid_peer_data() {
        let chunk_size = 64_usize;
        let unpacked = UnpackedChunk {
            bytes: Base64(vec![0_u8; chunk_size + 1]),
            ..UnpackedChunk::default()
        };
        let error = unpack_off_loop(
            ChunkFormat::Unpacked(unpacked),
            &pipeline(0, chunk_size as u64),
        )
        .await
        .expect_err("an oversized unpacked body is rejected");
        assert!(matches!(error, DeliveryError::InvalidBody(_)), "{error}");
        assert!(error.is_peer_fault());
    }

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
    use super::chunk_orchestrator::{ChunkRequest, ChunkRequestState};
    use super::{DataSyncServiceInner, DataSyncServiceMessage};
    use crate::chunk_fetcher::MockChunkFetcher;
    use crate::chunk_ingress_service::ChunkIngressMessage;
    use crate::services::ServiceSenders;
    use irys_database::submodule::{SubmoduleStore as _, tables::DataRootInfo};
    use irys_database::{IrysDatabaseArgs as _, open_or_create_db, tables::IrysTables};
    use irys_domain::{BlockTree, ChunkType, PeerList, StorageModule, StorageModuleInfo};
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{
        Base64, ChunkFormat, Config, DataLedger, H256, IrysAddress, IrysBlockHeader,
        LedgerChunkOffset, LedgerChunkRange, PartitionChunkOffset, RelativeChunkOffset, Traced,
        TxChunkOffset, UnpackedChunk, app_state::DatabaseProvider, irys::IrysSigner,
        ledger_chunk_offset_ii, partition::PartitionAssignment, partition_chunk_offset_ie,
    };
    use nodit::interval::ii;
    use std::{
        sync::{Arc, RwLock},
        time::Instant,
    };
    use tokio::sync::mpsc::UnboundedReceiver;

    /// Deliver `chunk` as a peer body, then apply the result the spawned
    /// ingress sends back, the way the service loop does.
    async fn deliver(
        inner: &mut DataSyncServiceInner,
        data_sync_rx: &mut UnboundedReceiver<Traced<DataSyncServiceMessage>>,
        storage_module_id: usize,
        offset: PartitionChunkOffset,
        peer: IrysAddress,
        chunk: ChunkFormat,
    ) -> eyre::Result<()> {
        inner.on_chunk_completed(storage_module_id, offset, peer, chunk)?;
        let (msg, _span) = data_sync_rx
            .recv()
            .await
            .expect("ingress result")
            .into_parts();
        let DataSyncServiceMessage::ChunkIngressed {
            storage_module_id,
            chunk_offset,
            peer_address,
            requested_at,
            result,
        } = msg
        else {
            panic!("expected ChunkIngressed");
        };
        inner.on_chunk_ingressed(
            storage_module_id,
            chunk_offset,
            peer_address,
            requested_at,
            result,
        )
    }

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
                body_in_ingress: false,
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
        let mut data_sync_rx = receivers.data_sync;
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
        let err = deliver(
            &mut inner,
            &mut data_sync_rx,
            sm.id,
            offset,
            peer,
            ChunkFormat::Unpacked(short),
        )
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
        let err = deliver(
            &mut inner,
            &mut data_sync_rx,
            sm.id,
            offset,
            peer,
            ChunkFormat::Unpacked(mismatched),
        )
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
        deliver(
            &mut inner,
            &mut data_sync_rx,
            sm.id,
            offset,
            peer,
            ChunkFormat::Unpacked(good),
        )
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
        let mut data_sync_rx = receivers.data_sync;
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
        let err = deliver(
            &mut inner,
            &mut data_sync_rx,
            sm.id,
            requested,
            peer,
            ChunkFormat::Unpacked(foreign),
        )
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

    struct RouteHarness {
        inner: DataSyncServiceInner,
        sm: Arc<StorageModule>,
        data_sync_rx: UnboundedReceiver<Traced<DataSyncServiceMessage>>,
        /// `IngestChunk` messages that reached ingress.
        ingested: Arc<std::sync::atomic::AtomicUsize>,
        chunk: UnpackedChunk,
    }

    /// One storage module on `ledger` with a one-chunk tx at offset 0. With
    /// `index` false the offset has a data-root row but no placement.
    /// The second value is the temp dir; keep it alive for the test.
    fn route_harness(ledger: DataLedger, index: bool) -> (RouteHarness, impl Sized) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let tmp = TempDirBuilder::new().with_tracing().build();
        let config = Config::new_with_random_peer_id(super::sync_test_node_config(tmp.path(), 4));
        let sm = Arc::new(
            StorageModule::new(
                &StorageModuleInfo {
                    id: 0,
                    partition_assignment: Some(PartitionAssignment {
                        ledger_id: Some(ledger.into()),
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
            .create_transaction(vec![3_u8; 32], H256::zero())
            .expect("tx");
        let tx = signer.sign_transaction(tx).expect("sign");
        let chunk = tx.data_chunks().expect("chunk").remove(0);
        if index {
            let (_root, proofs) = irys_types::DataTransactionLedger::merklize_tx_root(
                std::slice::from_ref(&tx.header),
            );
            sm.index_transaction_data(
                &tx.header,
                &proofs[0].proof,
                LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
                0,
            )
            .expect("index tx");
        } else {
            let (_, submodule) = sm
                .get_submodule_for_offset(PartitionChunkOffset::from(0_u32))
                .expect("submodule");
            submodule
                .db
                .update(|db_tx| {
                    db_tx.add_data_root_info(
                        chunk.data_root,
                        &DataRootInfo {
                            start_offset: RelativeChunkOffset::from(0_i32),
                            data_size: chunk.data_size,
                        },
                    )
                })
                .expect("index data root");
        }

        let env = open_or_create_db(
            tmp.path().join("db"),
            IrysTables::ALL,
            reth_db::mdbx::DatabaseArguments::irys_testing().expect("mdbx testing args"),
        )
        .expect("db");
        let (service_senders, receivers) = ServiceSenders::new();
        let mut ingress_rx = receivers.chunk_ingress;
        let ingested = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ingested);
        tokio::spawn(async move {
            while let Some(traced) = ingress_rx.recv().await {
                let (msg, _span) = traced.into_parts();
                if let ChunkIngressMessage::IngestChunk(_, Some(reply), _) = msg {
                    counter.fetch_add(1, Ordering::SeqCst);
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
        let peer_list = PeerList::from_peers(
            Vec::new(),
            service_senders.peer_network.clone(),
            &config,
            tokio::sync::broadcast::channel(8).0,
        )
        .expect("peer list");
        let factory: crate::chunk_fetcher::ChunkFetcherFactory =
            Box::new(|ledger_id| Arc::new(MockChunkFetcher::new(ledger_id as usize)));
        let inner = DataSyncServiceInner::new(
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree))),
            Arc::new(RwLock::new(vec![sm.clone()])),
            peer_list,
            DatabaseProvider(Arc::new(env)),
            factory,
            service_senders,
            config,
            tokio::runtime::Handle::current(),
        );
        let harness = RouteHarness {
            inner,
            sm,
            data_sync_rx: receivers.data_sync,
            ingested,
            chunk,
        };
        (harness, tmp)
    }

    #[tokio::test]
    async fn the_route_follows_the_ledger_and_the_local_placement() {
        let offset = PartitionChunkOffset::from(0_u32);
        for ledger in [
            DataLedger::Publish,
            DataLedger::OneYear,
            DataLedger::ThirtyDay,
        ] {
            let (h, _tmp) = route_harness(ledger, true);
            assert_eq!(
                h.inner.delivery_route(h.sm.id, offset),
                super::DeliveryRoute::Direct {
                    data_root: h.chunk.data_root,
                    tx_offset: h.chunk.tx_offset,
                    data_size: h.chunk.data_size,
                },
                "{ledger:?} with a placement writes directly"
            );
        }
        let (submit, _tmp_submit) = route_harness(DataLedger::Submit, true);
        assert_eq!(
            submit.inner.delivery_route(submit.sm.id, offset),
            super::DeliveryRoute::Ingress,
            "Submit keeps the compact-leaf fence"
        );
        let (unplaced, _tmp_unplaced) = route_harness(DataLedger::ThirtyDay, false);
        assert_eq!(
            unplaced.inner.delivery_route(unplaced.sm.id, offset),
            super::DeliveryRoute::Ingress,
            "no placement: ingress keeps the missing-index handling"
        );
    }

    /// A ThirtyDay body is written to the storage module without touching
    /// ingress. A Submit body still goes through ingress.
    #[tokio::test]
    async fn a_term_ledger_body_is_written_without_ingress() {
        use std::sync::atomic::Ordering;

        let offset = PartitionChunkOffset::from(0_u32);
        let peer = IrysAddress::from([9_u8; 20]);
        for (ledger, expected_ingress) in [(DataLedger::ThirtyDay, 0), (DataLedger::Submit, 1)] {
            let (mut h, _tmp) = route_harness(ledger, true);
            mark_requested(&mut h.inner, h.sm.id, offset, peer);
            let chunk = ChunkFormat::Unpacked(h.chunk.clone());
            deliver(
                &mut h.inner,
                &mut h.data_sync_rx,
                h.sm.id,
                offset,
                peer,
                chunk,
            )
            .await
            .unwrap_or_else(|error| panic!("{ledger:?} body is stored: {error}"));
            assert!(
                h.sm.is_data_write_pending_at(offset),
                "{ledger:?} body reached the storage module"
            );
            assert_eq!(
                h.ingested.load(Ordering::SeqCst),
                expected_ingress,
                "{ledger:?} ingress messages"
            );
        }
    }

    /// The direct route checks the body against this offset's placement. A
    /// body that disagrees on data size is the peer's fault.
    /// A rollback can change the placement while a body is in delivery. A
    /// body that no longer matches the captured placement is then retried
    /// without blaming the peer, which may have served the new chunk.
    #[tokio::test]
    async fn a_placement_change_during_delivery_does_not_blame_the_peer() {
        let offset = PartitionChunkOffset::from(0_u32);
        let peer = IrysAddress::from([9_u8; 20]);
        let (mut h, _tmp) = route_harness(DataLedger::ThirtyDay, true);
        mark_requested(&mut h.inner, h.sm.id, offset, peer);
        let other = UnpackedChunk {
            data_size: h.chunk.data_size + 1,
            ..h.chunk.clone()
        };
        h.inner
            .on_chunk_completed(h.sm.id, offset, peer, ChunkFormat::Unpacked(other))
            .expect("completion");
        h.sm.rollback_unassigned_range(offset, offset, &[h.chunk.data_root])
            .expect("rollback");

        let (msg, _span) = h.data_sync_rx.recv().await.expect("result").into_parts();
        let DataSyncServiceMessage::ChunkIngressed {
            storage_module_id,
            chunk_offset,
            peer_address,
            requested_at,
            result,
        } = msg
        else {
            panic!("expected ChunkIngressed");
        };
        let error = h
            .inner
            .on_chunk_ingressed(
                storage_module_id,
                chunk_offset,
                peer_address,
                requested_at,
                result,
            )
            .expect_err("the body is not stored");
        assert!(error.to_string().contains("placement"), "{error}");
        let request = &h.inner.chunk_orchestrators[&h.sm.id].chunk_requests[&offset];
        assert!(
            !request.excluded.contains(&peer),
            "the peer is not blamed for a placement that moved"
        );
        assert!(matches!(request.request_state, ChunkRequestState::Pending));
    }

    #[tokio::test]
    async fn a_direct_body_that_misses_the_placement_is_rejected() {
        use std::sync::atomic::Ordering;

        let offset = PartitionChunkOffset::from(0_u32);
        let peer = IrysAddress::from([9_u8; 20]);
        let (mut h, _tmp) = route_harness(DataLedger::ThirtyDay, true);
        let wrong_size = UnpackedChunk {
            data_size: h.chunk.data_size + 1,
            ..h.chunk.clone()
        };
        mark_requested(&mut h.inner, h.sm.id, offset, peer);
        let error = deliver(
            &mut h.inner,
            &mut h.data_sync_rx,
            h.sm.id,
            offset,
            peer,
            ChunkFormat::Unpacked(wrong_size),
        )
        .await
        .expect_err("a body for another data size must not be stored");
        assert!(error.to_string().contains("data_size"), "{error}");
        assert!(!h.sm.is_data_write_pending_at(offset));
        assert!(
            h.inner.chunk_orchestrators[&h.sm.id].chunk_requests[&offset]
                .excluded
                .contains(&peer),
            "the peer is penalized"
        );

        let bad_bytes = UnpackedChunk {
            bytes: Base64(vec![0_u8; 32]),
            ..h.chunk.clone()
        };
        mark_requested(&mut h.inner, h.sm.id, offset, peer);
        let error = deliver(
            &mut h.inner,
            &mut h.data_sync_rx,
            h.sm.id,
            offset,
            peer,
            ChunkFormat::Unpacked(bad_bytes),
        )
        .await
        .expect_err("bytes that miss the leaf hash must not be stored");
        assert!(
            error.to_string().contains("leaf") || error.to_string().contains("hash"),
            "{error}"
        );
        assert!(!h.sm.is_data_write_pending_at(offset));
        assert_eq!(h.ingested.load(Ordering::SeqCst), 0);
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
        PeerAddress, PeerListItem, PeerScore, ProtocolVersion, UnpackedChunk,
        app_state::DatabaseProvider, partition::PartitionAssignment, partition_chunk_offset_ie,
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
                    body_in_ingress: false,
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
                    body_in_ingress: false,
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

    fn request_state(
        inner: &DataSyncServiceInner,
        sm_id: usize,
        offset: PartitionChunkOffset,
    ) -> (&ChunkRequestState, bool) {
        let request = &inner.chunk_orchestrators[&sm_id].chunk_requests[&offset];
        (&request.request_state, request.body_in_ingress)
    }

    type IngressReply =
        tokio::sync::oneshot::Sender<Result<(), crate::chunk_ingress_service::ChunkIngressError>>;

    /// One Submit module of 64 Entropy chunks, one assignee peer, and an
    /// ingress service that holds every reply until the test answers it.
    struct Pipelined {
        _tmp: irys_testing_utils::tempfile::TempDir,
        inner: DataSyncServiceInner,
        sm_id: usize,
        assignee: IrysAddress,
        held_rx: tokio::sync::mpsc::UnboundedReceiver<(UnpackedChunk, IngressReply)>,
        data_sync_rx:
            tokio::sync::mpsc::UnboundedReceiver<irys_types::Traced<DataSyncServiceMessage>>,
        /// Valid chunks of one transaction, indexed by tx offset.
        chunks: Vec<UnpackedChunk>,
    }

    const WAIT: std::time::Duration = std::time::Duration::from_secs(5);

    fn pipelined() -> Pipelined {
        use crate::chunk_ingress_service::ChunkIngressMessage;

        let tmp = TempDirBuilder::new().with_tracing().build();
        let num_chunks = 64;
        let config =
            Config::new_with_random_peer_id(super::sync_test_node_config(tmp.path(), num_chunks));
        let assignee = IrysAddress::from([0xB1; 20]);
        let slot0 = PartitionAssignment {
            ledger_id: Some(u32::from(DataLedger::Submit)),
            slot_index: Some(0),
            miner_address: assignee,
            partition_hash: H256::repeat_byte(0x51),
        };
        let sm = Arc::new(
            StorageModule::new(
                &StorageModuleInfo {
                    id: 6,
                    partition_assignment: Some(slot0),
                    submodules: vec![(
                        partition_chunk_offset_ie!(0, 64),
                        tmp.path().join("chunks"),
                    )],
                },
                &config,
            )
            .expect("storage module"),
        );
        sm.pack_with_zeros();

        let signer = irys_types::irys::IrysSigner::random_signer(&config.consensus);
        let data: Vec<u8> = (0..8 * config.consensus.chunk_size)
            .map(|i| u8::try_from(i % 251).expect("byte"))
            .collect();
        let chunks = signer
            .create_transaction(data, H256::zero())
            .expect("tx")
            .data_chunks()
            .expect("chunks");

        let (service_senders, receivers) = ServiceSenders::new();
        let data_sync_rx = receivers.data_sync;
        let mut ingress_rx = receivers.chunk_ingress;
        let (held_tx, held_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(traced) = ingress_rx.recv().await {
                let (msg, _span) = traced.into_parts();
                if let ChunkIngressMessage::IngestChunk(chunk, Some(reply), _) = msg {
                    let _ = held_tx.send((chunk, reply));
                }
            }
        });
        let peer_list = PeerList::from_peers(
            vec![peer_item(assignee, 19193)],
            service_senders.peer_network.clone(),
            &config,
            tokio::sync::broadcast::channel(8).0,
        )
        .expect("peer list");
        let block_tree = BlockTree::new(&signed_genesis(), config.consensus.clone());
        let block_tree_guard =
            irys_domain::BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        install_epoch(&block_tree_guard, &[slot0]);
        let mut inner = DataSyncServiceInner::new(
            block_tree_guard,
            Arc::new(RwLock::new(vec![Arc::clone(&sm)])),
            peer_list,
            open_db(tmp.path()),
            Box::new(|ledger_id| Arc::new(MockChunkFetcher::new(ledger_id as usize))),
            service_senders,
            config,
            tokio::runtime::Handle::current(),
        );
        let sm_id = sm.id;
        let orchestrator = inner
            .chunk_orchestrators
            .get_mut(&sm_id)
            .expect("orchestrator");
        for offset in 0..u32::try_from(num_chunks).expect("chunks") {
            orchestrator.queue_pending_for_test(PartitionChunkOffset::from(offset));
        }
        Pipelined {
            _tmp: tmp,
            inner,
            sm_id,
            assignee,
            held_rx,
            data_sync_rx,
            chunks,
        }
    }

    impl Pipelined {
        fn slots(&self) -> (usize, u32, u32, u64) {
            peer_slot_counts(&self.inner, self.assignee)
        }

        fn requested(&self) -> Vec<PartitionChunkOffset> {
            let mut offsets: Vec<_> = self.inner.chunk_orchestrators[&self.sm_id]
                .chunk_requests
                .iter()
                .filter(|(_, request)| {
                    matches!(request.request_state, ChunkRequestState::Requested(..))
                })
                .map(|(offset, _)| *offset)
                .collect();
            offsets.sort();
            offsets
        }

        fn state(&self, offset: PartitionChunkOffset) -> (&ChunkRequestState, bool) {
            request_state(&self.inner, self.sm_id, offset)
        }

        /// `ChunkCompleted` must return without waiting for ingress.
        async fn complete(&mut self, offset: PartitionChunkOffset, body: UnpackedChunk) {
            tokio::time::timeout(
                WAIT,
                self.inner
                    .handle_message(DataSyncServiceMessage::ChunkCompleted {
                        storage_module_id: self.sm_id,
                        chunk_offset: offset,
                        peer_address: self.assignee,
                        chunk: irys_types::ChunkFormat::Unpacked(body),
                    }),
            )
            .await
            .expect("ChunkCompleted must not wait for ingress")
            .expect("completion");
        }

        async fn next_ingress(&mut self) -> (UnpackedChunk, IngressReply) {
            tokio::time::timeout(WAIT, self.held_rx.recv())
                .await
                .expect("ingress received a body")
                .expect("ingress task")
        }

        async fn no_more_ingress(&mut self) {
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(150), self.held_rx.recv())
                    .await
                    .is_err(),
                "no further body may reach ingress"
            );
        }

        /// Apply `n` `ChunkIngressed` results. A rejected body returns an
        /// error, which the service loop only logs.
        async fn apply_results(&mut self, n: usize) {
            let mut applied = 0;
            while applied < n {
                let (msg, _span) = tokio::time::timeout(WAIT, self.data_sync_rx.recv())
                    .await
                    .expect("result message")
                    .expect("data sync channel")
                    .into_parts();
                if matches!(msg, DataSyncServiceMessage::ChunkIngressed { .. }) {
                    let _ = self.inner.handle_message(msg).await;
                    applied += 1;
                }
            }
        }

        async fn wait_held_below(&self, limit: usize) {
            let held = &self.inner.delivery.held_bodies;
            tokio::time::timeout(WAIT, async {
                while held.load(std::sync::atomic::Ordering::SeqCst) >= limit {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("held bodies drain");
        }
    }

    /// `ChunkCompleted` returns while ingress still holds the body. The peer
    /// slot is free at once and the next tick refills it, while the delivered
    /// offsets stay `Requested` and are not fetched again. A body that fails
    /// its data path never reaches ingress. The results that come back later
    /// requeue, fail, or store the offsets without releasing a slot again.
    #[tokio::test]
    async fn chunk_completed_does_not_wait_for_ingress() {
        use crate::chunk_ingress_service::{ChunkIngressError, CriticalChunkIngressError};

        let mut t = pipelined();
        // This test counts the slots each body releases.
        t.inner.refill_on_release = false;
        assert_eq!(t.slots(), (0, 20, 0, 0), "a fresh peer may fill 20 slots");
        t.inner.tick().expect("first tick");
        assert_eq!(t.slots().0, 20);
        let requested = t.requested();
        assert_eq!(requested.len(), 20, "dispatch stops at the allowance");
        let [local, refused, invalid, stored] =
            [requested[0], requested[1], requested[2], requested[3]];

        let bad = UnpackedChunk {
            bytes: irys_types::Base64(vec![0_u8; t.chunks[3].bytes.0.len()]),
            ..t.chunks[3].clone()
        };
        let bodies = [
            (local, t.chunks[0].clone()),
            (refused, t.chunks[1].clone()),
            (invalid, bad),
            (stored, t.chunks[2].clone()),
        ];
        for (offset, body) in bodies {
            t.complete(offset, body).await;
            let (state, in_ingress) = t.state(offset);
            assert!(
                matches!(state, ChunkRequestState::Requested(peer, _) if *peer == t.assignee),
                "{state:?}"
            );
            assert!(in_ingress);
        }
        assert_eq!(t.slots().0, 16, "each delivered body released its slot");

        let mut held = Vec::new();
        for _ in 0..3 {
            held.push(t.next_ingress().await);
        }
        t.no_more_ingress().await;

        t.inner.tick().expect("tick while ingress is busy");
        assert_eq!(t.slots().0, 20, "the next tick refills the released slots");
        for offset in [local, refused, invalid, stored] {
            let (state, in_ingress) = t.state(offset);
            assert!(
                matches!(state, ChunkRequestState::Requested(..)) && in_ingress,
                "offset {offset} was dispatched again: {state:?}"
            );
        }
        assert_eq!(t.requested().len(), 24);

        for (chunk, reply) in held {
            let result = match u32::from(chunk.tx_offset) {
                0 => Err(ChunkIngressError::Critical(
                    CriticalChunkIngressError::DatabaseError,
                )),
                1 => Err(ChunkIngressError::Critical(
                    CriticalChunkIngressError::InvalidProof,
                )),
                _ => Ok(()),
            };
            reply.send(result).expect("ingress reply");
        }
        t.apply_results(4).await;

        let request = &t.inner.chunk_orchestrators[&t.sm_id].chunk_requests[&local];
        assert!(matches!(request.request_state, ChunkRequestState::Pending));
        assert!(
            !request.excluded.contains(&t.assignee),
            "a local ingress failure does not blame the peer"
        );
        for offset in [refused, invalid] {
            let request = &t.inner.chunk_orchestrators[&t.sm_id].chunk_requests[&offset];
            assert!(
                matches!(request.request_state, ChunkRequestState::Delayed { .. }),
                "the only peer failed {offset}: {:?}",
                request.request_state
            );
            assert!(request.excluded.contains(&t.assignee));
        }
        let (state, _) = t.state(stored);
        assert!(
            !matches!(
                state,
                ChunkRequestState::Requested(..)
                    | ChunkRequestState::Pending
                    | ChunkRequestState::Delayed { .. }
            ),
            "the accepted body reached the storage module: {state:?}"
        );
        let (active, _, _, total) = t.slots();
        assert_eq!(active, 20, "an ingress result must not release a slot");
        assert_eq!(total, 2, "the refused and the invalid body each count once");
    }

    /// `Overloaded` means the shared lane is full. The same body is sent to
    /// ingress again; the offset is not requeued and fetched again.
    #[tokio::test]
    async fn overloaded_ingress_keeps_the_body_and_sends_it_again() {
        use crate::chunk_ingress_service::{AdvisoryChunkIngressError, ChunkIngressError};

        let mut t = pipelined();
        t.inner.tick().expect("tick");
        let offset = t.requested()[0];
        let body = t.chunks[0].clone();
        t.complete(offset, body.clone()).await;

        let (first, reply) = t.next_ingress().await;
        assert_eq!(first, body);
        reply
            .send(Err(ChunkIngressError::from(
                AdvisoryChunkIngressError::Overloaded,
            )))
            .expect("reply");
        let (again, reply) = t.next_ingress().await;
        assert_eq!(again, body, "the same body is sent again");
        assert!(
            t.state(offset).1,
            "the offset is still waiting on that body"
        );
        reply.send(Ok(())).expect("reply");
        t.apply_results(1).await;

        let (state, _) = t.state(offset);
        assert!(
            !matches!(
                state,
                ChunkRequestState::Requested(..) | ChunkRequestState::Pending
            ),
            "{state:?}"
        );
        assert_eq!(t.slots().3, 0, "an overloaded lane is not the peer's fault");
    }

    /// Data sync keeps at most its ingress permits in ingress. At the
    /// held-body limit the tick starts no fetch, so a slow ingress backs up
    /// to the network. Freeing one body lets both move again.
    #[tokio::test]
    async fn ingress_permits_and_the_held_body_limit_bound_the_pipeline() {
        let mut t = pipelined();
        // This test counts the slots that stay free across the held-body limit.
        t.inner.refill_on_release = false;
        t.inner.delivery.ingress_permits = Arc::new(tokio::sync::Semaphore::new(2));
        t.inner.max_delivered_bodies = 3;
        t.inner.tick().expect("tick");
        let requested = t.requested();
        for (i, offset) in requested.iter().take(3).enumerate() {
            let body = t.chunks[i].clone();
            t.complete(*offset, body).await;
        }
        assert_eq!(t.slots().0, 17);

        let (_, first_reply) = t.next_ingress().await;
        let _second = t.next_ingress().await;
        t.no_more_ingress().await;

        t.inner.tick().expect("tick at the limit");
        assert_eq!(t.slots().0, 17, "no fetch starts at the held-body limit");

        first_reply.send(Ok(())).expect("reply");
        let _third = t.next_ingress().await;
        t.apply_results(1).await;
        t.wait_held_below(3).await;
        t.inner.tick().expect("tick under the limit");
        assert_eq!(t.slots().0, 20, "fetching resumes under the limit");
    }

    /// A body whose ingress never answers goes back to the queue after the
    /// delivery timeout, without blaming the peer.
    #[tokio::test]
    async fn a_body_stuck_in_ingress_times_out_to_pending() {
        let mut t = pipelined();
        t.inner.delivery.timeout = std::time::Duration::from_millis(100);
        t.inner.tick().expect("tick");
        let offset = t.requested()[0];
        let body = t.chunks[0].clone();
        t.complete(offset, body).await;
        let _never_answered = t.next_ingress().await;
        t.apply_results(1).await;
        let request = &t.inner.chunk_orchestrators[&t.sm_id].chunk_requests[&offset];
        assert!(
            matches!(request.request_state, ChunkRequestState::Pending),
            "{:?}",
            request.request_state
        );
        assert!(!request.excluded.contains(&t.assignee));
        assert_eq!(t.slots().3, 0);
    }

    /// A slot freed while the held-body limit holds dispatch back is filled
    /// when a body finishes, not at the next tick.
    #[tokio::test]
    async fn a_finished_body_refills_a_slot_the_held_limit_held_back() {
        let mut t = pipelined();
        t.inner.max_delivered_bodies = 1;
        t.inner.tick().expect("tick");
        let first = t.requested();
        assert_eq!(first.len(), 20);

        let body = t.chunks[0].clone();
        t.complete(first[0], body).await;
        assert_eq!(
            t.slots().0,
            19,
            "the held-body limit keeps the freed slot idle"
        );

        let (_, reply) = t.next_ingress().await;
        reply.send(Ok(())).expect("reply");
        t.apply_results(1).await;
        assert_eq!(
            t.slots().0,
            20,
            "the finished body's result refilled the slot"
        );
    }

    /// A finished request's slot is used at once, not at the next tick.
    #[tokio::test]
    async fn a_released_slot_is_refilled_without_waiting_for_the_tick() {
        let mut t = pipelined();
        t.inner.tick().expect("tick");
        let first = t.requested();
        assert_eq!(first.len(), 20, "the tick fills the allowance");

        // A 404 frees its slot. The next offset goes out at once.
        t.inner
            .handle_message(DataSyncServiceMessage::ChunkFailed {
                storage_module_id: t.sm_id,
                chunk_offset: first[0],
                peer_addr: t.assignee,
                failure_kind: super::chunk_fetcher::ChunkFetchFailureKind::NotFound,
            })
            .await
            .expect("failure");
        let after_404 = t.requested();
        assert_eq!(
            after_404.len(),
            20,
            "the freed slot was used before any tick"
        );
        assert!(!after_404.contains(&first[0]));
        assert_eq!(t.slots().0, 20);

        // A delivered body frees its slot on arrival, before ingress answers.
        let body = t.chunks[1].clone();
        t.complete(first[1], body).await;
        let after_body = t.requested();
        assert_eq!(
            after_body.len(),
            21,
            "the delivered offset waits on ingress, and its slot already went out again"
        );
        assert!(t.state(first[1]).1, "the delivered body is in ingress");
        assert_eq!(t.slots().0, 20, "the peer is back at its allowance");
    }

    /// A body no request is waiting for still reaches ingress, which writes it
    /// to any module with a writeable offset. No result comes back for it, and
    /// it frees no slot, so nothing new is dispatched.
    #[tokio::test]
    async fn an_untracked_body_still_reaches_ingress() {
        let mut t = pipelined();
        let offset = PartitionChunkOffset::from(0_u32);
        assert!(matches!(t.state(offset).0, ChunkRequestState::Pending));
        let body = t.chunks[0].clone();
        t.complete(offset, body.clone()).await;
        let (received, reply) = t.next_ingress().await;
        assert_eq!(received, body);
        reply.send(Ok(())).expect("reply");
        t.wait_held_below(1).await;
        assert!(
            t.data_sync_rx.try_recv().is_err(),
            "an untracked body sends no result"
        );
        assert!(matches!(t.state(offset).0, ChunkRequestState::Pending));
    }
}
