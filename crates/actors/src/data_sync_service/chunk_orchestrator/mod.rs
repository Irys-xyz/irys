use crate::{
    DataSyncServiceMessage,
    chunk_fetcher::{ChunkFetchError, ChunkFetcher},
    data_sync_service::peer_bandwidth_manager::PeerBandwidthManager,
    services::ServiceSenders,
};
use irys_domain::{BlockTreeReadGuard, ChunkTimeRecord, ChunkType, CircularBuffer, StorageModule};
use irys_types::{
    IrysAddress, LedgerChunkOffset, NodeConfig, PartitionChunkOffset, SendTraced as _,
    hardfork_config::DataLedgerLookup,
};
use std::{
    collections::{HashMap, HashSet, hash_map},
    sync::{Arc, RwLock},
    time::Instant,
};
use tracing::{Instrument as _, debug, warn};

/// Why a chunk offset is blocked from the hot re-fetch loop.
///
/// These are local progress blockers (not peer-delivery failures). Peers may
/// have delivered the bytes; we still cannot place them until the underlying
/// issue is repaired (e.g. SM data_root index rebuild).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkBlockReason {
    /// `write_data_chunk` failed because `DataRootInfosByDataRoot` has no entry
    /// for the chunk's data_root. Needs index rebuild / backfill.
    MissingDataRootIndex,
}

impl ChunkBlockReason {
    pub const fn as_metric_label(self) -> &'static str {
        match self {
            Self::MissingDataRootIndex => "missing_data_root_index",
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum ChunkRequestState {
    /// Chunk needs to be requested.
    Pending,

    /// Chunk has been requested from the specified peer at the given timestamp.
    /// Used for tracking timeouts and preventing duplicate requests.
    Requested(IrysAddress, Instant),

    /// Chunk was successfully fetched **and** durably written as [`ChunkType::Data`].
    ///
    /// Fetch alone is not enough — see [`Self::on_chunk_fetched`] then
    /// [`Self::mark_chunk_stored`].
    Completed,

    /// Locally blocked from hot re-fetch (index gap, etc.). Still retained while
    /// the offset is Entropy so we do not thrash peers. Cleared when the offset
    /// becomes Data, when this SM's id is in `SyncPartitions { unblock_sm_ids }`
    /// (immediately, and again on each tick while the SM stays healed) re-queues
    /// [`ChunkBlockReason::MissingDataRootIndex`], or on process restart.
    Blocked(ChunkBlockReason),
}

type ExcludedPeerAddresses = HashSet<IrysAddress>;
#[derive(Debug)]
pub struct ChunkRequest {
    pub ledger_id: usize,
    pub slot_index: usize,
    pub chunk_offset: PartitionChunkOffset,
    pub excluded: Option<ExcludedPeerAddresses>,
    pub request_state: ChunkRequestState,
}

/// Orchestrates efficient chunk downloading for a StorageModule's assigned data partition.
///
/// Key responsibilities:
/// - Rate-limits chunk requests based on local StorageModules' disk write throughput
/// - Queues and dispatches chunk requests across available peers
/// - Optimizes concurrency using peer health scores from PeerBandwidthManagers
/// - Tracks performance metrics for observability
#[derive(Debug)]
pub struct ChunkOrchestrator {
    pub chunk_requests: HashMap<PartitionChunkOffset, ChunkRequest>,
    pub current_peers: Vec<IrysAddress>,
    block_tree: BlockTreeReadGuard,
    pub storage_module: Arc<StorageModule>,
    recent_chunk_times: CircularBuffer<ChunkTimeRecord>, // Performance tracking for observability
    // Shared reference to peer bandwidth managers maintained by DataSyncService
    active_sync_peers: Arc<RwLock<HashMap<IrysAddress, PeerBandwidthManager>>>,
    service_senders: ServiceSenders,
    slot_index: usize,
    ledger_id: u32,
    chunk_fetcher: Arc<dyn ChunkFetcher>,
    config: NodeConfig,
    runtime_handle: tokio::runtime::Handle,
}
impl ChunkOrchestrator {
    pub fn new(
        storage_module: Arc<StorageModule>,
        sync_peers: Arc<RwLock<HashMap<IrysAddress, PeerBandwidthManager>>>,
        block_tree: BlockTreeReadGuard,
        service_senders: &ServiceSenders,
        chunk_fetcher: Arc<dyn ChunkFetcher>,
        config: NodeConfig,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        let slot_index = storage_module
            .partition_assignment()
            .expect("storage_module should have a partition assignment")
            .slot_index
            .expect("storage_module should be assigned to a ledger slot");

        let ledger_id = storage_module
            .partition_assignment()
            .unwrap()
            .ledger_id
            .unwrap();

        Self {
            storage_module,
            chunk_requests: Default::default(),
            recent_chunk_times: CircularBuffer::new(8000),
            current_peers: Default::default(),
            block_tree,
            active_sync_peers: sync_peers,
            service_senders: service_senders.clone(),
            ledger_id,
            slot_index,
            chunk_fetcher, // Store the chunk fetcher
            config,
            runtime_handle,
        }
    }

    pub fn tick(&mut self) -> eyre::Result<()> {
        // Only need to tick the Orchestrator if the partition is still assigned to a ledger.
        // Capacity partitions don't need to sync data.
        if self
            .storage_module
            .partition_assignment()
            .is_some_and(|pa| pa.ledger_id.is_some())
        {
            self.populate_request_queue();
            self.dispatch_chunk_requests();
        }

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn populate_request_queue(&mut self) {
        // Retain in-flight requests (for telemetry tracking) and pending entropy requests.
        // Remove completed requests and pending requests for chunks that changed type
        // (satisfied via gossip/upload or invalidated by storage fault/expiry).

        self.chunk_requests.retain(|offset, cr| {
            // Always retain in-flight requests
            if matches!(cr.request_state, ChunkRequestState::Requested(..)) {
                return true;
            }

            // Drop once the offset is no longer Entropy (stored as Data, or invalidated).
            if !matches!(
                self.storage_module.get_chunk_type(offset),
                Some(ChunkType::Entropy)
            ) {
                return false;
            }

            // Keep Pending / Blocked while still Entropy. Drop Completed.
            !matches!(cr.request_state, ChunkRequestState::Completed)
        });

        // Pending only — Blocked offsets intentionally do not consume the
        // pending budget or get re-dispatched.
        let pending_count: usize = self
            .chunk_requests
            .values()
            .filter(|r| matches!(r.request_state, ChunkRequestState::Pending))
            .count();

        let max_chunk_offset = self.get_max_chunk_offset();
        let pa = self.storage_module.partition_assignment().unwrap();

        let Some((max_chunk_offset, _)) = max_chunk_offset else {
            // Not requests needed
            debug!(
                "No chunk requests needed for ledger:{:?} slot_index:{:?}",
                pa.ledger_id, pa.slot_index
            );
            return;
        };

        let max_requests = self.config.data_sync.max_pending_chunk_requests as usize;
        let mut requests_to_add = max_requests.saturating_sub(pending_count);

        if requests_to_add == 0 {
            return;
        }

        let entropy_intervals = self.storage_module.get_intervals(ChunkType::Entropy);

        for interval in entropy_intervals {
            for interval_step in *interval.start()..=*interval.end() {
                let chunk_offset = PartitionChunkOffset::from(interval_step);

                // Don't try to sync above the maximum amount of data stored in the partition
                if chunk_offset > max_chunk_offset {
                    return;
                }

                if let hash_map::Entry::Vacant(entry) = self.chunk_requests.entry(chunk_offset) {
                    // Only executes when the entry in the hashmap is vacant
                    entry.insert(ChunkRequest {
                        ledger_id: self.storage_module.id,
                        slot_index: self.slot_index,
                        chunk_offset,
                        excluded: None, // First time chunk requests don't have past failed peer addresses
                        request_state: ChunkRequestState::Pending,
                    });

                    requests_to_add -= 1;
                    if requests_to_add == 0 {
                        return;
                    }
                }
            }
        }
    }

    #[tracing::instrument(skip_all)]
    fn dispatch_chunk_requests(&mut self) {
        if self.should_throttle_requests() {
            debug!("Throttling chunk requests due to storage throughput");
            return;
        }

        let pending_offsets: Vec<_> = self
            .chunk_requests
            .iter()
            .filter_map(|(&offset, req)| {
                matches!(req.request_state, ChunkRequestState::Pending).then_some(offset)
            })
            .collect();

        for chunk_offset in pending_offsets {
            // Reset exclusions if all peers are excluded
            if let Some(chunk_request) = self.chunk_requests.get_mut(&chunk_offset) {
                chunk_request.excluded = chunk_request
                    .excluded
                    .take()
                    .filter(|ex| ex.len() < self.current_peers.len());
            }

            // Find best peer and dispatch
            let Some(chunk_request) = self.chunk_requests.get(&chunk_offset) else {
                // Was the chunk requests at this offset removed? skip this offset (shouldn't happen)
                continue;
            };
            let Some(peer_address) = self.find_best_peer(chunk_request.excluded.as_ref()) else {
                // No available peers to request from? skip this offset (can happen)
                continue;
            };

            self.dispatch_chunk_request(chunk_offset, peer_address);
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn should_throttle_requests(&self) -> bool {
        let storage_throughput = self.storage_module.write_throughput_bps();
        let target_throughput = self.config.data_sync.max_storage_throughput_bps;
        let storage_capacity_remaining = target_throughput.saturating_sub(storage_throughput);

        let should_throttle = storage_capacity_remaining < (target_throughput / 10);

        debug!(
            "Throttle check: throughput={} target={} remaining={} throttle={}",
            storage_throughput, target_throughput, storage_capacity_remaining, should_throttle
        );

        // If we're within 10% of target_throughput, throttle this orchestrator
        should_throttle
    }

    #[tracing::instrument(skip_all)]
    pub fn get_max_chunk_offset(&self) -> Option<(PartitionChunkOffset, LedgerChunkOffset)> {
        // Find the maximum LedgerRelativeOffset of this storage module
        let ledger_range = self
            .storage_module
            .get_storage_module_ledger_offsets()
            .expect("storage module should be assigned to a ledger");

        // Fetch the most recently migrated block
        // We only want to download migrated chunks from other peers
        let max_chunk_offset: Option<u64> = {
            let tree = self.block_tree.read();
            let (canonical, _) = tree.get_canonical_chain();
            let block_migration_depth =
                self.config.consensus_config().block_migration_depth as usize;

            if canonical.len() >= block_migration_depth {
                let most_recent_migrated_block =
                    &canonical[canonical.len() - block_migration_depth];

                let block = most_recent_migrated_block.header();

                match tree
                    .consensus_config()
                    .hardforks
                    .classify_data_ledger(block, self.ledger_id)
                {
                    DataLedgerLookup::Present(dl) if dl.total_chunks == 0 => None,
                    DataLedgerLookup::Present(dl) => Some(dl.total_chunks.saturating_sub(1)),
                    // A migrated block that predates this ledger's activation
                    // (e.g. a pre-Cascade block for the OneYear/ThirtyDay term
                    // ledgers) legitimately has no entry for it — nothing to
                    // sync for this ledger yet.
                    DataLedgerLookup::ExpectedAbsent => {
                        debug!(
                            ledger_id = self.ledger_id,
                            block_height = block.height,
                            "migrated block predates this ledger's activation; nothing to sync yet"
                        );
                        None
                    }
                    // The block's shape is validated upstream, so this should be
                    // unreachable; surface it (defense-in-depth) but still degrade
                    // to None rather than aborting the task.
                    DataLedgerLookup::UnexpectedAbsent => {
                        warn!(
                            ledger_id = self.ledger_id,
                            block_height = block.height,
                            "data ledger missing from migrated block where consensus expects it; nothing to sync"
                        );
                        None
                    }
                }
            } else {
                None
            }
        };

        // If we couldn't find a valid max_chunk_offset return None
        let max_chunk_offset = max_chunk_offset?;

        // is the max chunk offset before the start of this storage module (can happen at head of chain)
        if ledger_range.start() > max_chunk_offset.into() {
            // Ledger range of the partition starts after the max_chunk_offset meaning don't attempt to sync anything
            return None;
        }

        if ledger_range.end() > max_chunk_offset.into() {
            let part_relative: u64 = max_chunk_offset.saturating_sub(ledger_range.start().into());
            Some((
                PartitionChunkOffset::from(part_relative as u32),
                LedgerChunkOffset::from(max_chunk_offset),
            ))
        } else {
            // Otherwise just return the maximum PartitionChunkOffset
            let max = ledger_range.end() - ledger_range.start();
            Some((
                PartitionChunkOffset::from(max),
                LedgerChunkOffset::from(max_chunk_offset),
            ))
        }
    }

    fn find_best_peer(&self, excluding: Option<&ExcludedPeerAddresses>) -> Option<IrysAddress> {
        let peers = self.active_sync_peers.read().ok()?;

        let mut candidates: Vec<&PeerBandwidthManager> = self
            .current_peers
            .iter()
            .filter_map(|&addr| peers.get(&addr))
            .filter(|peer_manager| {
                peer_manager.available_concurrency() > 0
                    && match &excluding {
                        Some(excluded) => !excluded.contains(&peer_manager.miner_address),
                        None => true,
                    }
            })
            .collect();

        if candidates.is_empty() {
            return None;
        }

        // Use the same sorting logic as the service
        // Primary sort: health score (reliability, recent performance)
        // Secondary sort: available concurrency (current capacity to handle more requests)
        candidates.sort_by(|a, b| {
            (b.health_score(), b.available_concurrency())
                .partial_cmp(&(a.health_score(), a.available_concurrency()))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Return the best peer
        candidates
            .first()
            .map(|peer_manager| peer_manager.miner_address)
    }

    #[tracing::instrument(skip_all)]
    fn dispatch_chunk_request(
        &mut self,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
    ) {
        let request = match self.chunk_requests.get_mut(&chunk_offset) {
            Some(req) => req,
            None => return,
        };

        let api_addr = {
            let mut peers = match self.active_sync_peers.write() {
                Ok(p) => p,
                Err(_) => return,
            };

            let peer_manager = match peers.get_mut(&peer_addr) {
                Some(pm) => pm,
                None => return,
            };

            peer_manager.on_chunk_request_started();
            peer_manager.peer_address.api
        };

        // Get a ledger chunk offset
        let start_ledger_offset = u64::from(
            self.storage_module
                .get_storage_module_ledger_offsets()
                .unwrap()
                .start(),
        );
        let ledger_chunk_offset =
            LedgerChunkOffset::from(start_ledger_offset + u64::from(chunk_offset));

        // Change the request_state from Pending -> Requested
        let start_instant = Instant::now();
        request.request_state = ChunkRequestState::Requested(peer_addr, start_instant);

        // Get the chunk fetcher
        let chunk_fetcher = self.chunk_fetcher.clone();
        let tx = self.service_senders.data_sync.clone();
        let storage_module_id = self.storage_module.id;
        let timeout = self.config.data_sync.chunk_request_timeout;

        self.runtime_handle.spawn(
            async move {
                debug!("Fetching chunk {chunk_offset} from {api_addr}");

                let result = chunk_fetcher
                    .fetch_chunk(ledger_chunk_offset, api_addr, timeout)
                    .await;

                let message = match result {
                    Ok(chunk) => DataSyncServiceMessage::ChunkCompleted {
                        storage_module_id,
                        chunk_offset,
                        peer_address: peer_addr,
                        chunk,
                    },
                    Err(ChunkFetchError::Timeout) => DataSyncServiceMessage::ChunkTimedOut {
                        storage_module_id,
                        chunk_offset,
                        peer_address: peer_addr,
                    },
                    Err(_) => DataSyncServiceMessage::ChunkFailed {
                        storage_module_id,
                        chunk_offset,
                        peer_addr,
                    },
                };

                // Service handles fetch credit + local write outcome (store / block / requeue).
                let _ = tx.send_traced(message);
            }
            .instrument(tracing::Span::current()),
        );
    }

    /// Peer delivered the chunk body. Credits peer bandwidth stats but leaves
    /// the request in [`ChunkRequestState::Requested`] until
    /// [`Self::mark_chunk_stored`] / [`Self::mark_chunk_blocked`] /
    /// [`Self::requeue_after_local_write_failure`] decides the local outcome.
    ///
    /// Peer delivery success must not be conflated with durable local storage.
    pub fn on_chunk_fetched(
        &mut self,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
    ) -> eyre::Result<ChunkTimeRecord> {
        let request = self.chunk_requests.get_mut(&chunk_offset).ok_or_else(|| {
            eyre::eyre!(
                "Chunk fetch completion for unknown offset: {:?}",
                chunk_offset
            )
        })?;

        let (expected_peer, start_instant) = match request.request_state {
            ChunkRequestState::Requested(addr, started) => (addr, started),
            ref invalid_state => {
                return Err(eyre::eyre!(
                    "Invalid state for chunk fetch at offset {}: expected Requested, got {:?}",
                    chunk_offset,
                    invalid_state
                ));
            }
        };

        if expected_peer != peer_addr {
            return Err(eyre::eyre!("Peer mismatch for chunk {:?}", chunk_offset));
        }

        let completion_time = Instant::now();
        let duration = completion_time.duration_since(start_instant);

        let completion_record = ChunkTimeRecord {
            chunk_offset,
            start_time: start_instant,
            completion_time,
            duration,
        };

        self.recent_chunk_times.push(completion_record.clone());

        // Credit the peer for successful delivery regardless of local write outcome.
        if let Ok(mut peers) = self.active_sync_peers.write()
            && let Some(peer_manager) = peers.get_mut(&peer_addr)
        {
            peer_manager.on_chunk_request_completed(completion_record.clone());
        }

        Ok(completion_record)
    }

    /// Durable local store succeeded — offset is (or will be observed as) Data.
    pub fn mark_chunk_stored(&mut self, chunk_offset: PartitionChunkOffset) -> eyre::Result<()> {
        let request = self.chunk_requests.get_mut(&chunk_offset).ok_or_else(|| {
            eyre::eyre!("mark_chunk_stored for unknown offset: {:?}", chunk_offset)
        })?;
        if !matches!(request.request_state, ChunkRequestState::Requested(..)) {
            return Err(eyre::eyre!(
                "mark_chunk_stored expected Requested state at {:?}, got {:?}",
                chunk_offset,
                request.request_state
            ));
        }
        request.request_state = ChunkRequestState::Completed;
        Ok(())
    }

    /// Locally blocked; stop hot re-fetching this offset.
    pub fn mark_chunk_blocked(
        &mut self,
        chunk_offset: PartitionChunkOffset,
        reason: ChunkBlockReason,
    ) -> eyre::Result<()> {
        let request = self.chunk_requests.get_mut(&chunk_offset).ok_or_else(|| {
            eyre::eyre!("mark_chunk_blocked for unknown offset: {:?}", chunk_offset)
        })?;
        if !matches!(request.request_state, ChunkRequestState::Requested(..)) {
            return Err(eyre::eyre!(
                "mark_chunk_blocked expected Requested state at {:?}, got {:?}",
                chunk_offset,
                request.request_state
            ));
        }
        request.request_state = ChunkRequestState::Blocked(reason);
        // Clear peer exclusions — the failure is local, not peer-specific.
        request.excluded = None;
        Ok(())
    }

    /// After a successful index heal, re-queue offsets blocked solely on
    /// [`ChunkBlockReason::MissingDataRootIndex`], up to `max` offsets, lowest
    /// offset first (deterministic — not HashMap iteration order).
    ///
    /// Cap prevents one call from flipping an unbounded Blocked backlog into
    /// Pending and storming the 250ms dispatch tick. Remaining Blocked offsets
    /// stay until a later call (immediate `SyncPartitions` unblock or the
    /// per-tick re-arm) drains them further.
    ///
    /// Returns the number of requests moved to [`ChunkRequestState::Pending`].
    pub fn unblock_missing_data_root_index(&mut self, max: usize) -> usize {
        if max == 0 {
            return 0;
        }
        let mut offsets: Vec<PartitionChunkOffset> = self
            .chunk_requests
            .iter()
            .filter_map(|(&offset, request)| {
                matches!(
                    request.request_state,
                    ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex)
                )
                .then_some(offset)
            })
            .collect();
        offsets.sort_unstable();
        offsets.truncate(max);
        for offset in &offsets {
            if let Some(request) = self.chunk_requests.get_mut(offset) {
                request.request_state = ChunkRequestState::Pending;
            }
        }
        offsets.len()
    }

    /// Local write failed for a non-blocking reason; re-queue without blaming the peer
    /// that just delivered the bytes.
    pub fn requeue_after_local_write_failure(
        &mut self,
        chunk_offset: PartitionChunkOffset,
    ) -> eyre::Result<()> {
        let request = self.chunk_requests.get_mut(&chunk_offset).ok_or_else(|| {
            eyre::eyre!(
                "requeue_after_local_write_failure for unknown offset: {:?}",
                chunk_offset
            )
        })?;
        if !matches!(request.request_state, ChunkRequestState::Requested(..)) {
            return Err(eyre::eyre!(
                "requeue_after_local_write_failure expected Requested at {:?}, got {:?}",
                chunk_offset,
                request.request_state
            ));
        }
        request.request_state = ChunkRequestState::Pending;
        // Do not add the delivering peer to `excluded` — they succeeded.
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, fields(
        chunk.offset = %chunk_offset,
        peer.address = %peer_addr
    ))]
    pub fn on_chunk_failed(
        &mut self,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
    ) -> eyre::Result<()> {
        let request = self
            .chunk_requests
            .get_mut(&chunk_offset)
            .ok_or_else(|| eyre::eyre!("Chunk failure for unknown offset: {:?}", chunk_offset))?;

        let expected_peer = match request.request_state {
            ChunkRequestState::Requested(addr, _) => addr,
            _ => {
                return Err(eyre::eyre!(
                    "Invalid request state for chunk failure: {:?}",
                    chunk_offset
                ));
            }
        };

        if expected_peer != peer_addr {
            return Err(eyre::eyre!(
                "Peer mismatch for chunk failure: {:?} expected peer: {} actual: {}",
                chunk_offset,
                expected_peer,
                peer_addr
            ));
        }

        debug!("resetting failed chunk to Pending {}", chunk_offset);
        // Record the peer that was expected to provide this chunk but failed
        request.request_state = ChunkRequestState::Pending;
        request
            .excluded
            .get_or_insert_with(HashSet::new)
            .insert(expected_peer);

        if let Ok(mut peers) = self.active_sync_peers.write()
            && let Some(peer_manager) = peers.get_mut(&peer_addr)
        {
            peer_manager.on_chunk_request_failure();
        }

        Ok(())
    }

    pub fn add_peer(&mut self, peer_addr: IrysAddress) {
        if !self.current_peers.contains(&peer_addr) {
            self.current_peers.push(peer_addr);
        }
    }

    pub fn remove_peer(&mut self, peer_addr: IrysAddress) {
        self.current_peers.retain(|&addr| addr != peer_addr);

        for request in self.chunk_requests.values_mut() {
            if let ChunkRequestState::Requested(addr, _) = request.request_state
                && addr == peer_addr
            {
                request.request_state = ChunkRequestState::Pending;
                request
                    .excluded
                    .get_or_insert_with(HashSet::new)
                    .insert(addr);
            }
        }
    }

    pub fn get_metrics(&self) -> OrchestrationMetrics {
        let (pending, active, completed, blocked) =
            self.chunk_requests
                .values()
                .fold((0, 0, 0, 0), |(p, a, c, b), request| {
                    match request.request_state {
                        ChunkRequestState::Pending => (p + 1, a, c, b),
                        ChunkRequestState::Requested(_, _) => (p, a + 1, c, b),
                        ChunkRequestState::Completed => (p, a, c + 1, b),
                        ChunkRequestState::Blocked(_) => (p, a, c, b + 1),
                    }
                });

        let total_throughput_bps = if let Ok(peers) = self.active_sync_peers.read() {
            self.current_peers
                .iter()
                .filter_map(|addr| peers.get(addr))
                .map(PeerBandwidthManager::current_bandwidth_bps)
                .sum()
        } else {
            0
        };

        OrchestrationMetrics {
            total_peers: self.current_peers.len(),
            pending_requests: pending,
            active_requests: active,
            completed_requests: completed,
            blocked_requests: blocked,
            total_throughput_bps,
        }
    }
}

#[derive(Debug)]
pub struct OrchestrationMetrics {
    pub total_peers: usize,
    pub pending_requests: usize,
    pub active_requests: usize,
    pub completed_requests: usize,
    pub blocked_requests: usize,
    pub total_throughput_bps: u64,
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod deterministic_unblock_order_tests {
    use super::*;
    use crate::{chunk_fetcher::MockChunkFetcher, test_helpers::build_test_service_senders};
    use irys_domain::{BlockTree, StorageModuleInfo};
    use irys_testing_utils::TempDirBuilder;
    use irys_types::{
        Config, ConsensusConfig, DataLedger, H256, partition::PartitionAssignment,
        partition_chunk_offset_ie,
    };

    /// With Blocked offsets {5, 1, 3} and `max = 2`, the lowest two offsets
    /// (1, 3) must be unblocked — not an arbitrary HashMap-order pair.
    #[test_log::test(tokio::test)]
    async fn unblock_missing_data_root_index_picks_lowest_offsets_first() {
        let tmp = TempDirBuilder::new().with_tracing().build();
        let num_chunks = 8_u64;
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: num_chunks,
                num_chunks_in_recall_range: 2,
                num_partitions_per_slot: 1,
                entropy_packing_iterations: 1,
                block_migration_depth: 1,
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
            miner_address: IrysAddress::from([1_u8; 20]),
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
        sm.pack_with_zeros();

        let genesis = irys_testing_utils::new_mock_signed_header();
        let block_tree = BlockTree::new(&genesis, config.consensus.clone());
        let block_tree_guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(block_tree)));
        let (service_senders, _receivers) = build_test_service_senders();
        let ledger_id: u32 = DataLedger::Publish.into();
        let mut orch = ChunkOrchestrator::new(
            sm,
            Arc::new(RwLock::new(HashMap::new())),
            block_tree_guard,
            &service_senders,
            Arc::new(MockChunkFetcher::new(ledger_id as usize)),
            config.node_config.clone(),
            tokio::runtime::Handle::current(),
        );

        for i in [5_u32, 1_u32, 3_u32] {
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

        let n = orch.unblock_missing_data_root_index(2);
        assert_eq!(n, 2);
        assert_eq!(
            orch.chunk_requests[&PartitionChunkOffset::from(1_u32)].request_state,
            ChunkRequestState::Pending,
            "lowest offset must be unblocked"
        );
        assert_eq!(
            orch.chunk_requests[&PartitionChunkOffset::from(3_u32)].request_state,
            ChunkRequestState::Pending,
            "second-lowest offset must be unblocked"
        );
        assert_eq!(
            orch.chunk_requests[&PartitionChunkOffset::from(5_u32)].request_state,
            ChunkRequestState::Blocked(ChunkBlockReason::MissingDataRootIndex),
            "highest offset must remain Blocked when capped below the full backlog"
        );
    }
}
