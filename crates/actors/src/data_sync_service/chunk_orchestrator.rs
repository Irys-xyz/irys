use crate::{
    chunk_fetcher::{ChunkFetchError, ChunkFetcher},
    data_sync_service::peer_bandwidth_manager::PeerBandwidthManager,
    services::ServiceSenders,
    DataSyncServiceMessage,
};
use irys_domain::{ChunkTimeRecord, ChunkType, CircularBuffer, StorageModule};
use irys_types::{Address, LedgerChunkOffset, NodeConfig, PartitionChunkOffset};
use std::{
    collections::{hash_map, HashMap},
    sync::{Arc, RwLock},
    time::Instant,
};
use tracing::debug;

#[derive(Debug, PartialEq)]
pub enum ChunkRequestState {
    /// Chunk needs to be requested. The optional Address indicates a peer that should be
    /// excluded from selection (typically because a previous request to that peer failed).
    Pending(Option<Address>),

    /// Chunk has been requested from the specified peer at the given timestamp.
    /// Used for tracking timeouts and preventing duplicate requests.
    Requested(Address, Instant),

    /// Chunk has been successfully retrieved and stored.
    Completed,
}

#[derive(Debug)]
pub struct ChunkRequest {
    pub ledger_id: usize,
    pub slot_index: usize,
    pub chunk_offset: PartitionChunkOffset,
    pub request_state: ChunkRequestState,
}

#[derive(Debug)]
pub struct ChunkOrchestrator {
    storage_module: Arc<StorageModule>,
    pub chunk_requests: HashMap<PartitionChunkOffset, ChunkRequest>,
    recent_chunk_times: CircularBuffer<ChunkTimeRecord>, // To support better observability in the future
    pub current_peers: Vec<Address>,
    // Keep a reference to the active_sync_peers in DataSyncService where it is maintained
    active_sync_peers: Arc<RwLock<HashMap<Address, PeerBandwidthManager>>>,
    service_senders: ServiceSenders,
    slot_index: usize,
    last_bandwidth_check: Instant,
    chunk_fetcher: Arc<dyn ChunkFetcher>,
    config: NodeConfig,
}

impl ChunkOrchestrator {
    pub fn new(
        storage_module: Arc<StorageModule>,
        sync_peers: Arc<RwLock<HashMap<Address, PeerBandwidthManager>>>,
        service_senders: &ServiceSenders,
        chunk_fetcher: Arc<dyn ChunkFetcher>,
        config: NodeConfig,
    ) -> Self {
        let slot_index = storage_module
            .partition_assignment()
            .expect("storage_module should have a partition assignment")
            .slot_index
            .expect("storage_module should be assigned to a ledger slot");

        Self {
            storage_module,
            chunk_requests: Default::default(),
            recent_chunk_times: CircularBuffer::new(8000),
            current_peers: Default::default(),
            active_sync_peers: sync_peers,
            service_senders: service_senders.clone(),
            slot_index,
            last_bandwidth_check: Instant::now(),
            chunk_fetcher, // Store the chunk fetcher
            config,
        }
    }

    pub fn tick(&mut self) -> eyre::Result<()> {
        self.populate_request_queue();
        self.adjust_peer_concurrency()?;
        self.dispatch_chunk_requests();
        Ok(())
    }

    fn populate_request_queue(&mut self) {
        let pending_count: usize = self
            .chunk_requests
            .values()
            .filter(|r| matches!(r.request_state, ChunkRequestState::Pending(_)))
            .count();

        let max_requests = self.config.data_sync.max_pending_chunk_requests as usize;
        let mut requests_to_add = max_requests.saturating_sub(pending_count);

        if requests_to_add == 0 {
            return;
        }

        let entropy_intervals = self.storage_module.get_intervals(ChunkType::Entropy);
        for interval in entropy_intervals {
            for interval_step in *interval.start()..=*interval.end() {
                let chunk_offset = PartitionChunkOffset::from(interval_step);

                if let hash_map::Entry::Vacant(e) = self.chunk_requests.entry(chunk_offset) {
                    // Only executes when the entry in the hashmap is vacant
                    e.insert(ChunkRequest {
                        ledger_id: self.storage_module.id,
                        slot_index: self.slot_index,
                        chunk_offset,
                        request_state: ChunkRequestState::Pending(None), // First time chunk requests don't have past failed peer addresses
                    });

                    requests_to_add -= 1;
                    if requests_to_add == 0 {
                        return;
                    }
                }
            }
        }
    }

    fn adjust_peer_concurrency(&mut self) -> eyre::Result<()> {
        let storage_throughput = self.storage_module.write_throughput_bps();
        let target_throughput = self.config.data_sync.max_storage_throughput_bps;
        let storage_capacity_remaining = target_throughput.saturating_sub(storage_throughput);

        debug!(
            "adjust_peer_concurrency(): Storage: {}/{} ({}% remaining)",
            storage_throughput,
            target_throughput,
            (storage_capacity_remaining * 100) / target_throughput
        );

        let now = Instant::now();
        let bandwidth_interval = self.config.data_sync.bandwidth_adjustment_interval;
        if now.duration_since(self.last_bandwidth_check) >= bandwidth_interval {
            self.optimize_peer_concurrency();
            self.last_bandwidth_check = now;
        }

        Ok(())
    }

    fn get_peer_scores(
        &self,
        peers: &HashMap<Address, PeerBandwidthManager>,
    ) -> Vec<(Address, f64, u32)> {
        // Build a list of peer score tuples (Address, health_score, available_concurrency)
        // from all the known peers (not just current)
        let mut peer_scores: Vec<_> = self
            .current_peers
            .iter()
            .filter_map(|&addr| {
                peers
                    .get(&addr)
                    .map(|pm| (addr, pm.health_score(), pm.available_concurrency()))
            })
            .collect();

        peer_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        peer_scores
    }

    fn optimize_peer_concurrency(&mut self) {
        if let Ok(mut peers) = self.active_sync_peers.write() {
            let peer_scores = self.get_peer_scores(&peers);

            for (peer_addr, health_score, available_concurrency) in peer_scores {
                if health_score >= 0.7 {
                    if let Some(peer_manager) = peers.get_mut(&peer_addr) {
                        let current_max = peer_manager.max_concurrency();
                        let increase = std::cmp::min(available_concurrency as usize, 5);
                        peer_manager.set_max_concurrency(current_max + increase);
                    }
                }
            }
        }
    }

    fn dispatch_chunk_requests(&mut self) {
        if self.should_throttle_requests() {
            debug!("Throttling chunk requests due to storage throughput");
            return; // Don't dispatch any new requests if the storage_module is saturated
        }

        let pending_requests: Vec<_> = self
            .chunk_requests
            .iter()
            .filter(|(_, request)| matches!(request.request_state, ChunkRequestState::Pending(_)))
            .map(|(offset, request)| {
                let excluding = if let ChunkRequestState::Pending(addr) = &request.request_state {
                    *addr
                } else {
                    None
                };
                (*offset, excluding)
            })
            .collect();

        if pending_requests.is_empty() {
            return;
        }

        for (chunk_offset, excluding) in pending_requests {
            if let Some(peer_addr) = self.find_best_peer(excluding) {
                self.dispatch_chunk_request(chunk_offset, peer_addr);
            } else {
                break;
            }
        }
    }

    fn should_throttle_requests(&self) -> bool {
        let storage_throughput = self.storage_module.write_throughput_bps();
        let target_throughput = self.config.data_sync.max_storage_throughput_bps;
        let storage_capacity_remaining = target_throughput.saturating_sub(storage_throughput);

        // If we're within 10% of target_throughput, throttle this orchestrator
        storage_capacity_remaining < (target_throughput / 10)
    }

    fn find_best_peer(&self, excluding: Option<Address>) -> Option<Address> {
        let peers = self.active_sync_peers.read().ok()?;

        let mut candidates: Vec<&PeerBandwidthManager> = self
            .current_peers
            .iter()
            .filter_map(|&addr| peers.get(&addr))
            .filter(|peer_manager| {
                let available = peer_manager.available_concurrency();
                available > 0
            })
            .filter(|peer_manager| {
                // Exclude the specified address if provided
                excluding != Some(peer_manager.miner_address)
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
    fn dispatch_chunk_request(&mut self, chunk_offset: PartitionChunkOffset, peer_addr: Address) {
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
                .get_storage_module_ledger_range()
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

        tokio::spawn(async move {
            debug!("Fetching chunk {chunk_offset} from {api_addr}");

            let result = chunk_fetcher
                .fetch_chunk(ledger_chunk_offset, api_addr, timeout)
                .await;

            let message = match result {
                Ok(chunk) => DataSyncServiceMessage::ChunkCompleted {
                    storage_module_id,
                    chunk_offset,
                    peer_addr,
                    chunk,
                },
                Err(ChunkFetchError::Timeout) => DataSyncServiceMessage::ChunkTimedOut {
                    storage_module_id,
                    chunk_offset,
                    peer_addr,
                },
                Err(_) => DataSyncServiceMessage::ChunkFailed {
                    storage_module_id,
                    chunk_offset,
                    peer_addr,
                },
            };

            let _ = tx.send(message);
        });
    }

    pub fn on_chunk_completed(
        &mut self,
        chunk_offset: PartitionChunkOffset,
        peer_addr: Address,
    ) -> eyre::Result<ChunkTimeRecord> {
        let request = self.chunk_requests.get_mut(&chunk_offset).ok_or_else(|| {
            eyre::eyre!("Chunk completion for unknown offset: {:?}", chunk_offset)
        })?;

        let (expected_peer, start_instant) = match request.request_state {
            ChunkRequestState::Requested(addr, started) => (addr, started),
            _ => {
                return Err(eyre::eyre!(
                    "Invalid request state for chunk completion: {:?}",
                    chunk_offset
                ))
            }
        };

        if expected_peer != peer_addr {
            return Err(eyre::eyre!("Peer mismatch for chunk {:?}", chunk_offset));
        }

        let completion_time = Instant::now();
        let duration = completion_time.duration_since(start_instant);

        request.request_state = ChunkRequestState::Completed;

        let completion_record = ChunkTimeRecord {
            chunk_offset,
            start_time: start_instant,
            completion_time,
            duration,
        };

        self.recent_chunk_times.push(completion_record.clone());

        if let Ok(mut peers) = self.active_sync_peers.write() {
            if let Some(peer_manager) = peers.get_mut(&peer_addr) {
                peer_manager.on_chunk_request_completed(completion_record.clone());
            }
        }

        Ok(completion_record)
    }

    pub fn on_chunk_failed(
        &mut self,
        chunk_offset: PartitionChunkOffset,
        peer_addr: Address,
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
                ))
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
        request.request_state = ChunkRequestState::Pending(Some(expected_peer));

        if let Ok(mut peers) = self.active_sync_peers.write() {
            if let Some(peer_manager) = peers.get_mut(&peer_addr) {
                peer_manager.on_chunk_request_failure();
            }
        }

        Ok(())
    }

    pub fn add_peer(&mut self, peer_addr: Address) {
        if !self.current_peers.contains(&peer_addr) {
            self.current_peers.push(peer_addr);
        }
    }

    pub fn remove_peer(&mut self, peer_addr: Address) {
        self.current_peers.retain(|&addr| addr != peer_addr);

        for request in self.chunk_requests.values_mut() {
            if let ChunkRequestState::Requested(addr, _) = request.request_state {
                if addr == peer_addr {
                    request.request_state = ChunkRequestState::Pending(Some(addr));
                }
            }
        }
    }

    pub fn get_metrics(&self) -> OrchestrationMetrics {
        let (pending, active, completed) =
            self.chunk_requests
                .values()
                .fold((0, 0, 0), |(p, a, c), request| {
                    match request.request_state {
                        ChunkRequestState::Pending(_) => (p + 1, a, c),
                        ChunkRequestState::Requested(_, _) => (p, a + 1, c),
                        ChunkRequestState::Completed => (p, a, c + 1),
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
    pub total_throughput_bps: u64,
}
