use irys_domain::{ChunkTimeRecord, CircularBuffer};
use irys_types::{partition::PartitionAssignment, Config, PeerAddress, PeerListItem};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum PeerCapacity {
    Low,    // 0-40 MB/s of throughput
    Medium, // 40-80 MB/s of throughput
    High,   // >80 MB/s of throughput
}

#[derive(Debug)]
pub struct PeerBandwidthManager {
    pub partition_assignments: Vec<PartitionAssignment>,
    pub peer_address: PeerAddress,
    pub peer_capacity: PeerCapacity,
    pub interval_start_time: Instant,
    pub recent_chunk_times: CircularBuffer<ChunkTimeRecord>,
    pub throughput: u64,
    pub last_throughput: u64,
    pub active_requests: usize,
    pub config: Config,
}

impl PeerBandwidthManager {
    pub fn new(peer_list_item: &PeerListItem, config: &Config) -> Self {
        Self {
            partition_assignments: Vec::new(),
            peer_address: peer_list_item.address,
            peer_capacity: PeerCapacity::Medium,
            throughput: 0,
            interval_start_time: Instant::now(),
            last_throughput: 0,
            recent_chunk_times: Default::default(),
            active_requests: 0,
            config: config.clone(),
        }
    }

    pub fn requests_available(&self, max_bandwidth_bps: u32) -> i32 {
        self.optimal_concurrency(max_bandwidth_bps) as i32 - self.active_requests as i32
    }

    /// Returns the average completion time for chunks in the buffer.
    /// Returns Duration::ZERO if no chunks are recorded.
    pub fn average_chunk_completion_time(&self) -> Duration {
        let recent_chunk_times = &self.recent_chunk_times;
        if recent_chunk_times.is_empty() {
            return Duration::ZERO;
        }

        // Calculate average duration from all records
        let total_duration_nanos: u128 = recent_chunk_times
            .iter()
            .map(|record| record.total_duration.as_nanos())
            .sum();

        let avg_duration_nanos = total_duration_nanos / recent_chunk_times.len() as u128;

        Duration::from_nanos(avg_duration_nanos as u64)
    }

    pub fn optimal_concurrency(&self, max_bandwidth_bps: u32) -> u32 {
        let average_chunk_completion_time = self.average_chunk_completion_time();

        // Calculate current throughput per chunk (bytes/s)
        let throughput_per_chunk =
            self.config.consensus.chunk_size as f32 / average_chunk_completion_time.as_secs_f32();

        // Calculate how many concurrent chunks we can run without exceeding bandwidth
        let optimal_concurrency = max_bandwidth_bps as f32 / throughput_per_chunk;

        let optimal_concurrency = optimal_concurrency.floor() as u32;

        // Set the floor concurrency to at least 1
        optimal_concurrency.max(1)
    }
}
