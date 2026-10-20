use crate::data_sync_service::peer_stats::PeerStats;
use irys_domain::ChunkTimeRecord;
use irys_types::{Config, IrysAddress, PeerAddress, PeerListItem, partition::PartitionAssignment};

#[derive(Debug)]
pub struct PeerBandwidthManager {
    pub partition_assignments: Vec<PartitionAssignment>,
    pub peer_address: PeerAddress,
    pub miner_address: IrysAddress,
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
            peer_stats: PeerStats::new(target_bandwidth_mbps, chunk_size, timeout),
        }
    }

    /// True when this peer advertises a partition assignment for the given ledger/slot.
    ///
    /// Assignees can serve `GET /chunk/ledger/{ledger}/{offset}`. Peers that only
    /// appear as ingress-proof signers (empty matching assignments) must be
    /// fetched via data_root + tx_offset instead.
    pub fn is_assigned_to(&self, ledger_id: u32, slot_index: Option<usize>) -> bool {
        self.partition_assignments.iter().any(|assignment| {
            assignment.ledger_id == Some(ledger_id) && assignment.slot_index == slot_index
        })
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

    /// Release a request that returned 404 without treating a sparse-ledger
    /// absence as a transport or peer-health failure.
    pub fn on_chunk_not_found(&mut self) {
        self.peer_stats.record_request_not_found();
    }

    /// The request will never complete. Release its slot without a health penalty.
    pub fn release_abandoned_request(&mut self) {
        self.peer_stats.release_abandoned_request();
    }

    /// Get max concurrency (upper bound)
    pub fn max_concurrency(&self) -> usize {
        self.peer_stats.max_concurrency as usize
    }

    /// Construction-time ceiling. [`Self::set_max_concurrency`] will not go above it.
    pub fn concurrency_ceiling(&self) -> usize {
        self.peer_stats.concurrency_ceiling as usize
    }

    /// Set max concurrency (upper bound). Values above [`Self::concurrency_ceiling`]
    /// clamp to that ceiling, so a healthy peer cannot grow an unbounded in-flight allowance.
    pub fn set_max_concurrency(&mut self, concurrency: usize) {
        let capped = concurrency.min(self.concurrency_ceiling());
        self.peer_stats.max_concurrency = capped as u32;
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

#[cfg(test)]
mod tests {
    use super::PeerBandwidthManager;
    use irys_types::{
        Config, IrysAddress, IrysPeerId, NodeConfig, PeerAddress, PeerListItem, PeerScore,
        ProtocolVersion,
    };
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    /// The optimizer adds 3 or 5 every tick. That step has to stop at the
    /// ceiling `PeerStats::new` already computed, and a later step must be
    /// able to return to that ceiling after a lower value.
    #[test]
    fn set_max_concurrency_does_not_exceed_the_construction_ceiling() {
        let config = Config::new_with_random_peer_id(NodeConfig::testing());
        let miner = IrysAddress::from([3_u8; 20]);
        let api = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 9);
        let peer = PeerListItem {
            peer_id: IrysPeerId::from(miner),
            mining_address: miner,
            address: PeerAddress {
                gossip: api,
                api,
                ..Default::default()
            },
            reputation_score: PeerScore::new(PeerScore::INITIAL),
            is_online: true,
            protocol_version: ProtocolVersion::default(),
            ..Default::default()
        };
        let mut manager = PeerBandwidthManager::new(&miner, &peer, &config);
        let ceiling = manager.max_concurrency();
        assert!(
            ceiling > 1,
            "ceiling {ceiling} is too small to prove the cap"
        );

        manager.set_max_concurrency(ceiling + 5);
        assert_eq!(manager.max_concurrency(), ceiling);

        manager.set_max_concurrency(1);
        assert_eq!(manager.max_concurrency(), 1);

        manager.set_max_concurrency(ceiling + 5);
        assert_eq!(manager.max_concurrency(), ceiling);
    }
}
