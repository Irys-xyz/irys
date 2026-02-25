use crate::data_sync_service::peer_stats::PeerStats;
use irys_domain::ChunkTimeRecord;
use irys_types::{Config, IrysAddress, PeerAddress, PeerListItem, partition::PartitionAssignment};

#[derive(Debug)]
pub struct PeerBandwidthManager {
    pub partition_assignments: Vec<PartitionAssignment>,
    pub peer_address: PeerAddress,
    pub miner_address: IrysAddress,
    pub config: Config,
    peer_stats: PeerStats, // private - external access only through wrapper methods
}

impl PeerBandwidthManager {
    pub fn new(
        miner_address: &IrysAddress,
        peer_list_item: &PeerListItem,
        config: &Config,
    ) -> Self {
        let chunk_size = config.consensus.chunk_size;
        let timeout = config.node_config.data_sync.chunk_request_timeout;
        let target_bandwidth_mbps = 100; // Default target, adjust as needed

        Self {
            partition_assignments: Vec::new(),
            peer_address: peer_list_item.address,
            miner_address: *miner_address,
            config: config.clone(),
            peer_stats: PeerStats::new(target_bandwidth_mbps, chunk_size, timeout),
        }
    }

    pub fn active_requests(&self) -> usize {
        self.peer_stats.active_requests
    }

    /// Record when a new request is started
    pub fn on_chunk_request_started(&mut self) {
        self.peer_stats.record_request_started();
    }

    /// Record a successful chunk completion
    pub fn on_chunk_request_completed(&mut self, completion_record: ChunkTimeRecord) {
        self.peer_stats.record_request_completed(completion_record);
    }

    /// Record a failed chunk request
    pub fn on_chunk_request_failure(&mut self) {
        self.peer_stats.record_request_failed();
    }

    /// Get max concurrency (upper bound)
    pub fn max_concurrency(&self) -> usize {
        self.peer_stats.max_concurrency as usize
    }

    /// Set max concurrency (upper bound)
    pub fn set_max_concurrency(&mut self, concurrency: usize) {
        self.peer_stats.max_concurrency = concurrency as u32;
    }

    /// Get current max concurrency (dynamic limit)
    pub fn current_max_concurrency(&self) -> usize {
        self.peer_stats.current_max_concurrency as usize
    }

    /// Get available concurrency capacity (relative to dynamic limit)
    pub fn available_concurrency(&self) -> u32 {
        self.peer_stats.available_concurrency()
    }

    /// Get short-term bandwidth throughput in bytes per second (10s window)
    pub fn current_bandwidth_bps(&self) -> u64 {
        self.peer_stats.short_term_bandwidth_bps()
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.peer_stats.consecutive_failures
    }

    pub fn total_failures(&self) -> u64 {
        self.peer_stats.total_failures
    }

    /// Get a health score for this peer (0.0 to 1.0)
    /// Higher scores indicate better performing, more reliable peers
    pub fn health_score(&self) -> f64 {
        self.peer_stats.health_score()
    }

    pub fn short_term_bandwidth_bps(&self) -> u64 {
        self.peer_stats.short_term_bandwidth_bps()
    }

    pub fn medium_term_bandwidth_bps(&self) -> u64 {
        self.peer_stats.medium_term_bandwidth_bps()
    }

    pub fn is_throughput_stable(&self) -> bool {
        self.peer_stats.is_throughput_stable()
    }

    pub fn is_throughput_improving(&self) -> bool {
        self.peer_stats.is_throughput_improving()
    }
}
