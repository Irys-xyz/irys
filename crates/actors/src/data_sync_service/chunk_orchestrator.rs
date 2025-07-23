use irys_domain::{ChunkTimeRecord, ChunkType, CircularBuffer, StorageModule};
use irys_types::{Address, PartitionChunkOffset};
use std::{
    collections::{hash_map, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};

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
    pub last_adjustment_time: Instant,
    pub storage_module: Arc<StorageModule>,
    pub chunk_requests: HashMap<PartitionChunkOffset, ChunkRequest>,
    pub recent_chunk_times: CircularBuffer<ChunkTimeRecord>,
    pub current_peers: Vec<Address>,
    pub concurrency_per_peer: usize,
    slot_index: usize,
}

impl ChunkOrchestrator {
    pub fn new(storage_module: Arc<StorageModule>) -> Self {
        let slot_index = storage_module
            .partition_assignment()
            .unwrap()
            .slot_index
            .unwrap();

        Self {
            last_adjustment_time: Instant::now(),
            storage_module,
            chunk_requests: Default::default(),
            recent_chunk_times: CircularBuffer::new(8000),
            current_peers: Default::default(),
            concurrency_per_peer: 1,
            slot_index,
        }
    }

    pub fn tick() {
        // Check the storage modules available throughput
    }

    pub fn refresh_chunk_request_queue(
        &mut self,
        desired_queue_depth: usize,
        timeout_seconds: u64,
    ) {
        // Find and retry timed-out requests
        self.retry_failed_requests(timeout_seconds);

        // Add new pending requests to the queue to fill it to the desired depth
        self.populate_request_queue(desired_queue_depth);
    }

    fn retry_failed_requests(&mut self, timeout_seconds: u64) {
        let timeout_duration = Duration::from_secs(timeout_seconds);
        let now = Instant::now();

        self.chunk_requests
       .values_mut()  // Get mutable references to all ChunkRequest values
       .filter(|request| matches!(
           request.request_state,
           // Match requests that are in "Requested" state and have timed out
           ChunkRequestState::Requested(_, instant) if now.duration_since(instant) > timeout_duration
       ))
       .for_each(|request| {
           // Reset timed-out requests back to pending state for retry
           request.request_state = ChunkRequestState::Pending;
       });
    }

    fn populate_request_queue(&mut self, desired_queue_depth: usize) {
        let pending_requests: HashMap<_, _> = self
            .chunk_requests
            .iter()
            .filter(|(_, request)| request.request_state == ChunkRequestState::Pending)
            .collect();

        let current_queue_depth = pending_requests.len();

        // Ensure we have enough pending requests in the pipeline
        let mut requests_to_add = desired_queue_depth.saturating_sub(current_queue_depth);

        // Get the Entropy intervals from the storage module
        let entropy_intervals = self.storage_module.get_intervals(ChunkType::Entropy);

        for interval in entropy_intervals {
            for interval_step in *interval.start()..=*interval.end() {
                let chunk_offset = PartitionChunkOffset::from(interval_step);
                // check to see if the offset is already requested
                if let hash_map::Entry::Vacant(e) = self.chunk_requests.entry(chunk_offset) {
                    // Add the chunk offset as a pending request
                    let chunk_request = ChunkRequest {
                        ledger_id: self.storage_module.id,
                        slot_index: self.slot_index,
                        chunk_offset,
                        request_state: ChunkRequestState::Pending,
                    };
                    e.insert(chunk_request);

                    requests_to_add -= 1;

                    if requests_to_add == 0 {
                        return;
                    }
                }
            }
        }
    }
}
