mod chunk_orchestrator;
mod peer_bandwidth_manager;
use irys_domain::{BlockTreeReadGuard, ChunkType, PeerList, StorageModule};
use irys_types::{Address, Config, TokioServiceHandle};
use reth::tasks::shutdown::Shutdown;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
use tokio::sync::mpsc::UnboundedReceiver;

use chunk_orchestrator::ChunkOrchestrator;
use peer_bandwidth_manager::PeerBandwidthManager;

pub struct DataSyncService {
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<DataSyncServiceMessage>,
    pub inner: DataSyncServiceInner,
}

type StorageModuleId = usize;

pub struct DataSyncServiceInner {
    pub block_tree: BlockTreeReadGuard,
    pub storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
    // TODO: Listen to epoch events to know when to refresh all_peers
    // TODO: update all_peers when peer list is updated
    pub all_peers: HashMap<Address, PeerBandwidthManager>,
    pub chunk_orchestrators: HashMap<StorageModuleId, ChunkOrchestrator>,
    pub peer_list: PeerList,
    pub config: Config,
}

pub enum DataSyncServiceMessage {
    SyncPartitions,
}

impl DataSyncServiceInner {
    pub fn new(
        block_tree: BlockTreeReadGuard,
        storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
        peer_list: PeerList,
        config: Config,
    ) -> Self {
        // let mut data_sync = Self {
        //     block_tree,
        //     storage_modules,
        //     peer_list,
        //     all_peers: Default::default(),
        //     chunk_orchestrators: Default::default(),
        //     config,
        // };
        // data_sync.sync_peer_partition_assignments();
        // data_sync.bootstrap_peer_downloads();
        // data_sync

        Self {
            block_tree,
            storage_modules,
            peer_list,
            all_peers: Default::default(),
            chunk_orchestrators: Default::default(),
            config,
        }
    }

    pub fn handle_message(&self, _msg: DataSyncServiceMessage) -> eyre::Result<()> {
        Ok(())
    }

    /// Creates or updates peer bandwidth managers for all peers with partition assignments that store
    /// data relevant to the local miner's storage modules.
    ///
    /// For each qualifying storage module, this function identifies all peers assigned to the same
    /// ledger slot and ensures they have a PeerBandwidthManager in the `all_peers` list with their
    /// respective partition assignments. This maintains an up-to-date mapping of peers that are
    /// assigned to store data the local miner needs to access.
    pub fn sync_peer_partition_assignments(&mut self) {
        let storage_modules = self.storage_modules.read().unwrap();

        // Process each storage module to find those with partition assignments
        for storage_module in storage_modules.iter() {
            // Skip storage modules without partition assignments
            let Some(pa) = *storage_module.partition_assignment.read().unwrap() else {
                continue;
            };

            // Skip partition assignments without a ledger ID (Capacity partitions)
            let Some(ledger_id) = pa.ledger_id else {
                continue;
            };

            // Check if the storage module has packed entropy ready to store data
            let entropy_intervals = storage_module.get_intervals(ChunkType::Entropy);
            if entropy_intervals.is_empty() {
                continue;
            }

            // Sync the peers from the peer_list that store data for this ledger slot
            let epoch_snapshot = self.block_tree.read().canonical_epoch_snapshot();

            // Find all assigned partition hashes for the same ledger slot
            let slot_assignments: Vec<_> = epoch_snapshot
                .partition_assignments
                .data_partitions
                .iter()
                .filter(|(_, a)| a.ledger_id == Some(ledger_id))
                .collect();

            // Make sure we have an updated PeerBandwidthManager for any peers that are assigned to stor the same
            // slot data as us.
            for (_partition_hash, pa) in slot_assignments {
                // Find the peer by their mining address
                let Some(peer) = self.peer_list.peer_by_mining_address(&pa.miner_address) else {
                    return;
                };

                // Get or create a bandwidth manager for this peer
                let entry = self
                    .all_peers
                    .entry(pa.miner_address)
                    .or_insert(PeerBandwidthManager::new(&peer, &self.config));

                // Add this partition assignment to the peers bandwidth manager if it's not already tracked
                if !entry.partition_assignments.contains(pa) {
                    entry.partition_assignments.push(*pa);
                }
            }
        }
    }

    /// Assume ~50MB/s per peer initially
    /// Start with 2 peers to get 100MB/s baseline
    /// Add 3rd peer if first two can't hit 150MB/s combined
    /// Add 4th peer only if needed for full 200MB/s
    /// The key is ramping up quickly but safely - we'll want to discover actual peer capacity within 30-60 seconds without overwhelming anyone.
    pub fn bootstrap_peer_downloads(&mut self) {
        // Loop though Storage modules
        for sm in self.storage_modules.read().unwrap().iter() {
            // Identify any without orchestrators
            let sm_id = sm.id;

            match self.chunk_orchestrators.get(&sm_id) {
                Some(_orchestrator) => {
                    // TODO: check to see if the peer / concurrency should be increased
                }
                None => {
                    // Initialize any missing orchestrators for bootstrapping
                    let mut chunk_orchestrator = ChunkOrchestrator::new(sm.clone());

                    // Conservative start: Begin with 2 peers at low concurrency to avoid overwhelming
                    // unknown peers or saturating your partition limit.
                    let best_peers = self.get_best_available_peers(sm, 2);
                    chunk_orchestrator.current_peers = best_peers.clone();

                    // Initial_concurrency_per_peer = 5 requests, this gives 60MB/s initially while we learn
                    chunk_orchestrator.concurrency_per_peer = 5;

                    self.chunk_orchestrators.insert(sm_id, chunk_orchestrator);
                }
            }
        }
    }

    /// Gets the best available peers for syncing data with the specified storage module.
    ///
    /// Returns up to `desired_count` peers that have partition assignments matching the
    /// storage module's ledger ID and slot index, prioritized by bandwidth availability
    /// and connection quality.
    ///
    /// # Arguments
    /// * `storage_module` - The storage module to find peers for
    /// * `desired_count` - Maximum number of peers to return
    ///
    /// # Returns
    /// A vector of peer addresses sorted by preference (best first)
    pub fn get_best_available_peers(
        &self,
        storage_module: &StorageModule,
        desired_count: usize,
    ) -> Vec<Address> {
        // Get the storage module's partition assignment
        let Some(pa) = *storage_module.partition_assignment.read().unwrap() else {
            return Vec::new();
        };

        let Some(ledger_id) = pa.ledger_id else {
            return Vec::new();
        };

        // Find peers that have partition assignments for the same ledger slot
        let mut candidate_peers: Vec<_> = self
            .all_peers
            .iter()
            .filter(|(_, peer_manager)| {
                peer_manager.partition_assignments.iter().any(|assignment| {
                    assignment.ledger_id == Some(ledger_id)
                        && assignment.slot_index == pa.slot_index
                })
            })
            .collect();

        // TEMP: Drive by config value
        let max_bandwidth_bps: u32 = 200 * 1024 * 1024; // 200 MB/s as bytes

        // Sort by bandwidth availability (best first)
        candidate_peers.sort_by(|(_, a), (_, b)| {
            b.requests_available(max_bandwidth_bps)
                .partial_cmp(&a.requests_available(max_bandwidth_bps))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Return up to the desired count
        candidate_peers
            .into_iter()
            .take(desired_count)
            .map(|(address, _)| *address)
            .collect()
    }

    // Rapid Discovery Phase:
    // Parallel ramp-up: Increase all peers simultaneously to quickly discover their capabilities.
    //    discovery_phase (first 30-60 seconds):
    //    every 10_seconds:
    //        for each_peer:
    //            if (peer_throughput_improving):
    //                peer_concurrency += 2
    //            else:
    //                peer_concurrency += 1
    //
    //        total_throughput = sum(peer_throughputs)
    //
    //        if (total_throughput > 180MB):
    //            switch_to_steady_state_management()
    //    }

    // Classify peers:
    //    after 20-30 seconds per peer:
    //    if (peer_throughput > 80MB):
    //        peer_class = "high_capacity"
    //    elif (peer_throughput > 40MB):
    //        peer_class = "medium_capacity"
    //    else:
    //        peer_class = "low_capacity"
    //        consider_replacing_peer()

    // Add Peers Gradually + Fallback management
    //    if (current_peers_maxed_out && total_throughput < 180MB):
    //        add_new_peer_at_low_concurrency()

    //    if (peer_performing_poorly):
    //        reduce_peer_concurrency()
    //        start_evaluating_replacement_peer()
}

/// mpsc style service wrapper for the Data Sync Service
impl DataSyncService {
    /// Spawn a new DataSync service
    pub fn spawn_service(
        rx: UnboundedReceiver<DataSyncServiceMessage>,
        block_tree: BlockTreeReadGuard,
        storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>, // As managed by storage module service
        peer_list: PeerList,
        config: &Config,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        let config = config.clone();
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let handle = runtime_handle.spawn(async move {
            let data_sync_service = Self {
                shutdown: shutdown_rx,
                msg_rx: rx,
                inner: DataSyncServiceInner::new(block_tree, storage_modules, peer_list, config),
            };
            data_sync_service
                .start()
                .await
                .expect("DataSync Service encountered an irrecoverable error")
        });

        TokioServiceHandle {
            name: "data_sync_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting DataSync Service");

        loop {
            tokio::select! {
                biased;

                // Check for shutdown signal
                _ = &mut self.shutdown => {
                    tracing::info!("Shutdown signal received for storage module service");
                    break;
                }
                // Handle messages
                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(msg) => {
                            self.inner.handle_message(msg)?;
                        }
                        None => {
                            tracing::warn!("Message channel closed unexpectedly");
                            break;
                        }
                    }
                }
            }
        }

        tracing::debug!(amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.inner.handle_message(msg)?
        }

        tracing::info!("shutting down DataSync Service gracefully");
        Ok(())
    }
}
