use crate::{
    data_sync_service::peer_bandwidth_manager::PeerBandwidthManager, services::ServiceSenders,
    DataSyncServiceMessage,
};
use irys_domain::{ChunkTimeRecord, ChunkType, CircularBuffer, StorageModule};
use irys_types::{Address, LedgerChunkOffset, PackedChunk, PartitionChunkOffset};
use std::{
    collections::{hash_map, HashMap},
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tokio::{sync::mpsc::UnboundedSender, time::timeout};

const BYTES_IN_200_MB: u64 = 200 * 1024 * 1024;
const TARGET_QUEUE_DEPTH: usize = 1000;
const BANDWIDTH_CHECK_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq)]
pub enum ChunkRequestState {
    Pending,
    Requested(Address, Instant),
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
    chunk_requests: HashMap<PartitionChunkOffset, ChunkRequest>,
    recent_chunk_times: CircularBuffer<ChunkTimeRecord>,
    pub(crate) current_peers: Vec<Address>,
    all_peers: Arc<RwLock<HashMap<Address, PeerBandwidthManager>>>,
    service_senders: ServiceSenders,
    slot_index: usize,
    timeout_duration: Duration,
    last_bandwidth_check: Instant,
}

impl ChunkOrchestrator {
    pub fn new(
        storage_module: Arc<StorageModule>,
        all_peers: Arc<RwLock<HashMap<Address, PeerBandwidthManager>>>,
        service_senders: &ServiceSenders,
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
            all_peers,
            service_senders: service_senders.clone(),
            slot_index,
            timeout_duration: Duration::from_secs(15),
            last_bandwidth_check: Instant::now(),
        }
    }

    pub fn tick(&mut self) -> eyre::Result<()> {
        self.recycle_timed_out_requests();
        self.populate_request_queue();
        self.adjust_peer_concurrency()?;
        self.dispatch_chunk_requests();
        Ok(())
    }

    fn recycle_timed_out_requests(&mut self) {
        let now = Instant::now();

        for request in self.chunk_requests.values_mut() {
            if let ChunkRequestState::Requested(peer_addr, start_time) = request.request_state {
                if now.duration_since(start_time) > self.timeout_duration {
                    if let Ok(mut peers) = self.all_peers.write() {
                        if let Some(peer_manager) = peers.get_mut(&peer_addr) {
                            peer_manager.on_chunk_request_failure();
                        }
                    }
                    request.request_state = ChunkRequestState::Pending;
                }
            }
        }
    }

    fn populate_request_queue(&mut self) {
        let pending_count = self
            .chunk_requests
            .values()
            .filter(|r| r.request_state == ChunkRequestState::Pending)
            .count();

        let mut requests_to_add = TARGET_QUEUE_DEPTH.saturating_sub(pending_count);
        if requests_to_add == 0 {
            return;
        }

        let entropy_intervals = self.storage_module.get_intervals(ChunkType::Entropy);

        for interval in entropy_intervals {
            for interval_step in *interval.start()..=*interval.end() {
                let chunk_offset = PartitionChunkOffset::from(interval_step);

                if let hash_map::Entry::Vacant(e) = self.chunk_requests.entry(chunk_offset) {
                    e.insert(ChunkRequest {
                        ledger_id: self.storage_module.id,
                        slot_index: self.slot_index,
                        chunk_offset,
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

    fn adjust_peer_concurrency(&mut self) -> eyre::Result<()> {
        let storage_throughput = self.storage_module.write_throughput();
        let storage_capacity_remaining = BYTES_IN_200_MB.saturating_sub(storage_throughput);

        if storage_capacity_remaining < (BYTES_IN_200_MB / 10) {
            self.reduce_all_peer_concurrency();
            return Ok(());
        }

        let now = Instant::now();
        if now.duration_since(self.last_bandwidth_check) >= BANDWIDTH_CHECK_INTERVAL {
            self.optimize_peer_concurrency();
            self.last_bandwidth_check = now;
        }

        Ok(())
    }

    /// Reduces chunk request concurrency for peers associated with this Orchestrators storage_module
    fn reduce_all_peer_concurrency(&mut self) {
        if let Ok(mut peers) = self.all_peers.write() {
            for peer_addr in &self.current_peers {
                if let Some(peer_manager) = peers.get_mut(peer_addr) {
                    let max_concurrency = peer_manager.max_concurrency();
                    if max_concurrency > 1 {
                        peer_manager.set_max_concurrency((max_concurrency * 8) / 10);
                    }
                }
            }
        }
    }

    fn get_peer_scores(
        &self,
        peers: &HashMap<Address, PeerBandwidthManager>,
    ) -> Vec<(Address, f64, u32)> {
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
        if let Ok(mut peers) = self.all_peers.write() {
            let peer_scores = self.get_peer_scores(&peers);

            for (peer_addr, health_score, available_concurrency) in peer_scores {
                if health_score > 0.7 && available_concurrency > 0 {
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
        let pending_offsets: Vec<_> = self
            .chunk_requests
            .iter()
            .filter(|(_, request)| request.request_state == ChunkRequestState::Pending)
            .map(|(offset, _)| *offset)
            .collect();

        if pending_offsets.is_empty() {
            return;
        }

        let client = reqwest::Client::new();

        for chunk_offset in pending_offsets {
            if let Some(peer_addr) = self.find_best_peer() {
                self.dispatch_chunk_request(client.clone(), chunk_offset, peer_addr);
            } else {
                break;
            }
        }
    }

    fn find_best_peer(&self) -> Option<Address> {
        let peers = self.all_peers.read().ok()?;
        let peer_scores = self.get_peer_scores(&peers);

        peer_scores
            .into_iter()
            .find(|(_, _, available_concurrency)| *available_concurrency > 0)
            .map(|(addr, _, _)| addr)
    }

    fn dispatch_chunk_request(
        &mut self,
        client: reqwest::Client,
        chunk_offset: PartitionChunkOffset,
        peer_addr: Address,
    ) {
        let request = match self.chunk_requests.get_mut(&chunk_offset) {
            Some(req) => req,
            None => return,
        };

        let (ledger_id, api_addr) = {
            let mut peers = match self.all_peers.write() {
                Ok(p) => p,
                Err(_) => return,
            };

            let peer_manager = match peers.get_mut(&peer_addr) {
                Some(pm) => pm,
                None => return,
            };

            peer_manager.on_chunk_request_started();
            (request.ledger_id, peer_manager.peer_address.api)
        };

        let start_instant = Instant::now();
        let start_ledger_offset = u64::from(
            self.storage_module
                .get_storage_module_ledger_range()
                .unwrap()
                .start(),
        );
        let ledger_chunk_offset =
            LedgerChunkOffset::from(start_ledger_offset + u64::from(chunk_offset));

        request.request_state = ChunkRequestState::Requested(peer_addr, start_instant);

        Self::spawn_chunk_request(
            client,
            self.service_senders.data_sync.clone(),
            chunk_offset,
            ledger_chunk_offset,
            self.storage_module.id,
            ledger_id,
            peer_addr,
            api_addr,
        );
    }

    fn spawn_chunk_request(
        client: reqwest::Client,
        tx: UnboundedSender<DataSyncServiceMessage>,
        chunk_offset: PartitionChunkOffset,
        ledger_chunk_offset: LedgerChunkOffset,
        storage_module_id: usize,
        ledger_id: usize,
        peer_addr: Address,
        api_addr: SocketAddr,
    ) {
        tokio::spawn(async move {
            let url = format!(
                "http://{}/v1/chunk/ledger/{}/{}",
                api_addr, ledger_id, ledger_chunk_offset
            );

            let result = timeout(Duration::from_secs(15), async {
                let response = client
                    .get(&url)
                    .send()
                    .await
                    .map_err(|e| eyre::eyre!("Request failed: {}", e))?;
                let chunk: PackedChunk = response
                    .json()
                    .await
                    .map_err(|e| eyre::eyre!("JSON parsing failed: {}", e))?;
                eyre::Ok(chunk)
            })
            .await;

            let message = match result {
                Ok(Ok(chunk)) => DataSyncServiceMessage::ChunkCompleted {
                    storage_module_id,
                    chunk_offset,
                    peer_addr,
                    chunk,
                },
                Ok(Err(_)) => DataSyncServiceMessage::ChunkFailed {
                    storage_module_id,
                    chunk_offset,
                    peer_addr,
                },
                Err(_) => DataSyncServiceMessage::ChunkTimedOut {
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

        if let Ok(mut peers) = self.all_peers.write() {
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
                "Peer mismatch for chunk failure: {:?}",
                chunk_offset
            ));
        }

        request.request_state = ChunkRequestState::Pending;

        if let Ok(mut peers) = self.all_peers.write() {
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
                    request.request_state = ChunkRequestState::Pending;
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
                        ChunkRequestState::Pending => (p + 1, a, c),
                        ChunkRequestState::Requested(_, _) => (p, a + 1, c),
                        ChunkRequestState::Completed => (p, a, c + 1),
                    }
                });

        let total_throughput_bps = if let Ok(peers) = self.all_peers.read() {
            self.current_peers
                .iter()
                .filter_map(|addr| peers.get(addr))
                .map(|pm| pm.current_bandwidth_bps())
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
