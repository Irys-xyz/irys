pub mod chunk_fetcher;
pub mod chunk_orchestrator;
pub mod peer_bandwidth_manager;
pub mod peer_stats;

use crate::{chunk_fetcher::ChunkFetcherFactory, services::ServiceSenders};
use chunk_orchestrator::ChunkOrchestrator;
use irys_domain::{BlockTreeReadGuard, ChunkType, PeerList, StorageModule};
use irys_packing::unpack;
use irys_types::{Address, Config, PackedChunk, PartitionChunkOffset, TokioServiceHandle};
use peer_bandwidth_manager::PeerBandwidthManager;
use reth::tasks::shutdown::Shutdown;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::oneshot;
use tracing::{debug, warn};

pub struct DataSyncService {
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<DataSyncServiceMessage>,
    pub inner: DataSyncServiceInner,
}

type StorageModuleId = usize;

pub struct DataSyncServiceInner {
    pub block_tree: BlockTreeReadGuard,
    pub storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
    pub active_sync_peers: Arc<RwLock<HashMap<Address, PeerBandwidthManager>>>,
    pub chunk_orchestrators: HashMap<StorageModuleId, ChunkOrchestrator>,
    pub peer_list: PeerList,
    pub chunk_fetcher_factory: ChunkFetcherFactory,
    pub service_senders: ServiceSenders,
    pub config: Config,
}

pub enum DataSyncServiceMessage {
    SyncPartitions,
    ChunkCompleted {
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_addr: Address,
        chunk: PackedChunk,
    },
    ChunkFailed {
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_addr: Address,
    },
    ChunkTimedOut {
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_addr: Address,
    },
    PeerDisconnected {
        peer_addr: Address,
    },
    GetActivePeersList(oneshot::Sender<Arc<RwLock<HashMap<Address, PeerBandwidthManager>>>>),
}

impl DataSyncServiceInner {
    pub fn new(
        block_tree: BlockTreeReadGuard,
        storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
        peer_list: PeerList,
        chunk_fetcher_factory: ChunkFetcherFactory,
        service_senders: ServiceSenders,
        config: Config,
    ) -> Self {
        let mut data_sync = Self {
            block_tree,
            storage_modules,
            peer_list,
            active_sync_peers: Default::default(),
            chunk_fetcher_factory,
            chunk_orchestrators: Default::default(),
            service_senders,
            config,
        };
        data_sync.initialize_peers_and_orchestrators();
        data_sync
    }

    pub fn handle_message(&mut self, msg: DataSyncServiceMessage) -> eyre::Result<()> {
        match msg {
            DataSyncServiceMessage::SyncPartitions => {
                self.sync_peer_partition_assignments();
                self.update_orchestrator_peers();
            }
            DataSyncServiceMessage::ChunkCompleted {
                storage_module_id,
                chunk_offset,
                peer_addr,
                chunk,
            } => self.on_chunk_completed(storage_module_id, chunk_offset, peer_addr, chunk)?,
            DataSyncServiceMessage::ChunkFailed {
                storage_module_id,
                chunk_offset,
                peer_addr,
            } => self.on_chunk_failed(storage_module_id, chunk_offset, peer_addr)?,
            DataSyncServiceMessage::ChunkTimedOut {
                storage_module_id,
                chunk_offset,
                peer_addr,
            } => self.on_chunk_timeout(storage_module_id, chunk_offset, peer_addr)?,
            DataSyncServiceMessage::PeerDisconnected { peer_addr } => {
                self.handle_peer_disconnection(peer_addr)
            }
            DataSyncServiceMessage::GetActivePeersList(tx) => self.handle_get_active_peers_list(tx),
        }
        Ok(())
    }

    pub fn tick(&mut self) -> eyre::Result<()> {
        for orchestrator in self.chunk_orchestrators.values_mut() {
            orchestrator.tick()?;
        }
        self.optimize_peer_concurrency();
        Ok(())
    }

    fn optimize_peer_concurrency(&mut self) {
        let Ok(mut peers) = self.active_sync_peers.write() else {
            return;
        };

        // Build a list of peer score tuples (Address, health_score, active_requests, max_concurrency)
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
            if utilization_ratio >= 0.8 {
                if let Some(peer_manager) = peers.get_mut(&peer_addr) {
                    // Better performing peers get bigger increases
                    let increase = if health_score >= 0.9 {
                        5 // Excellent peer, trust it with more
                    } else if health_score >= 0.7 {
                        3 // Good peer, moderate increase
                    } else {
                        1 // Decent peer, conservative increase
                    };
                    debug!(
                    "Increasing max concurrency from {} to {} for peer {} (utilization: {:.1}%, health: {:.2})",
                    current_max,
                    current_max + increase,
                    peer_addr,
                    utilization_ratio * 100.0,
                    health_score
                );
                    peer_manager.set_max_concurrency(current_max + increase);
                }
            } else {
                debug!(
                "Not increasing concurrency for peer {} (concurrent utilization: {:.1}%, health: {:.2})",
                peer_addr,
                utilization_ratio * 100.0,
                health_score
            );
            }
        }
    }

    fn on_chunk_completed(
        &mut self,
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_addr: Address,
        chunk: PackedChunk,
    ) -> eyre::Result<()> {
        // Update the orchestrator with completion tracking
        if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
            orchestrator.on_chunk_completed(chunk_offset, peer_addr)?;
        }

        // Unpack and store the chunk data
        let consensus = &self.config.consensus;
        let unpacked_chunk = unpack(
            &chunk,
            consensus.entropy_packing_iterations,
            consensus.chunk_size as usize,
            consensus.chain_id,
        );

        self.storage_modules
            .read()
            .unwrap()
            .get(storage_module_id)
            .unwrap()
            .write_data_chunk(&unpacked_chunk)?;

        Ok(())
    }

    fn on_chunk_failed(
        &mut self,
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_addr: Address,
    ) -> eyre::Result<()> {
        debug!("chunk failed: {} peer:{}", chunk_offset, peer_addr);
        if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
            orchestrator.on_chunk_failed(chunk_offset, peer_addr)?;
        }
        Ok(())
    }

    fn on_chunk_timeout(
        &mut self,
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_addr: Address,
    ) -> eyre::Result<()> {
        // TODO: Opportunity to do custom timeout tracking/handling here
        debug!("chunk timed out: {} peer:{}", chunk_offset, peer_addr);
        if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
            orchestrator.on_chunk_failed(chunk_offset, peer_addr)?;
        }
        Ok(())
    }

    fn handle_peer_disconnection(&mut self, peer_addr: Address) {
        // Remove peer from all orchestrators
        for orchestrator in self.chunk_orchestrators.values_mut() {
            orchestrator.remove_peer(peer_addr);
        }

        // Remove from peer list
        self.active_sync_peers.write().unwrap().remove(&peer_addr);
    }

    fn handle_get_active_peers_list(
        &self,
        tx: oneshot::Sender<Arc<RwLock<HashMap<Address, PeerBandwidthManager>>>>,
    ) {
        if let Err(e) = tx.send(self.active_sync_peers.clone()) {
            tracing::error!("handle_get_active_peers_list() tx.send() error: {:?}", e);
        };
    }

    fn initialize_peers_and_orchestrators(&mut self) {
        self.sync_peer_partition_assignments();
        self.create_chunk_orchestrators();
        self.update_orchestrator_peers();
    }

    fn sync_peer_partition_assignments(&mut self) {
        let storage_modules = self.storage_modules.read().unwrap().clone();

        for storage_module in storage_modules {
            let Some(pa) = *storage_module.partition_assignment.read().unwrap() else {
                continue;
            };

            let Some(ledger_id) = pa.ledger_id else {
                continue;
            };

            let Some(slot_index) = pa.slot_index else {
                continue;
            };

            // Only sync peers for storage modules that need data
            let entropy_intervals = storage_module.get_intervals(ChunkType::Entropy);
            if entropy_intervals.is_empty() {
                debug!("StorageModule has no entropy chunks\n{:?}", pa);
                continue;
            }

            self.ensure_bandwidth_managers_for_peers(ledger_id, slot_index);
        }
    }

    /// Updates the active_peers list and ensures there are PeerBandwidthManagers for
    /// any peers assigned to store the same slot data.
    fn ensure_bandwidth_managers_for_peers(&mut self, ledger_id: u32, slot_index: usize) {
        let epoch_snapshot = self.block_tree.read().canonical_epoch_snapshot();

        let slot_assignments: Vec<_> = epoch_snapshot
            .partition_assignments
            .data_partitions
            .values()
            .filter(|a| a.ledger_id == Some(ledger_id) && a.slot_index == Some(slot_index))
            .copied()
            .collect();

        for pa in slot_assignments {
            let Some(peer) = self.peer_list.peer_by_mining_address(&pa.miner_address) else {
                continue;
            };

            // Get existing peer bandwidth manager or add a new one for the peer
            let mut active_peers = self.active_sync_peers.write().unwrap();
            let entry = active_peers
                .entry(pa.miner_address)
                .or_insert(PeerBandwidthManager::new(
                    &pa.miner_address,
                    &peer,
                    &self.config,
                ));

            // Finally add the partition assignment to the peer if it isn't present
            if !entry.partition_assignments.contains(&pa) {
                entry.partition_assignments.push(pa);
            }
        }
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

        for sm in storage_modules {
            let sm_id = sm.id;

            // Skip if we already have an orchestrator for this storage module
            if self.chunk_orchestrators.contains_key(&sm_id) {
                continue;
            }

            // Skip unused storage modules without partition assignments (not yet initialized)
            let Some(pa) = sm.partition_assignment() else {
                continue;
            };

            // Skip capacity partitions - they store entropy, not data chunks that need syncing
            if pa.ledger_id.is_none() {
                continue;
            }

            // Use the factory to create a chunk_fetcher (allows mock chunk fetchers for testing)
            let chunk_fetcher = (self.chunk_fetcher_factory)(pa.ledger_id.unwrap());

            // Create orchestrator for storage modules that needs to sync data
            let orchestrator = ChunkOrchestrator::new(
                sm.clone(),
                self.active_sync_peers.clone(),
                self.block_tree.clone(),
                &self.service_senders,
                chunk_fetcher,
                self.config.node_config.clone(),
            );

            self.chunk_orchestrators.insert(sm_id, orchestrator);
        }
    }

    fn update_orchestrator_peers(&mut self) {
        let storage_modules = self.storage_modules.read().unwrap().clone();

        // Collect storage_module IDs first to avoid borrowing conflicts
        let sm_ids: Vec<StorageModuleId> = self.chunk_orchestrators.keys().copied().collect();

        // Get a list of the best peers (by mining address) for each storage module
        let mut peer_updates: Vec<(StorageModuleId, Vec<Address>)> = Vec::new();

        for sm_id in sm_ids {
            let Some(storage_module) = storage_modules.get(sm_id) else {
                continue;
            };

            let best_peers = self.get_best_available_peers(storage_module, 4);
            peer_updates.push((sm_id, best_peers));
        }

        // Add the peers to the orchestrators
        for (sm_id, best_peers) in peer_updates {
            // Skip ff we don't have an orchestrator for this storage_module
            let Some(orchestrator) = self.chunk_orchestrators.get_mut(&sm_id) else {
                warn!("Storage module with id: {sm_id} does not have a chunk_orchestrator and it should.");
                continue;
            };

            // Skip the add_peer() orchestrator fn and update current_peers directly
            orchestrator.current_peers = best_peers.clone();
        }
    }

    pub fn get_best_available_peers(
        &self,
        storage_module: &StorageModule,
        desired_count: usize,
    ) -> Vec<Address> {
        // Only return peers for storage modules that have active chunk orchestrators
        // This ensures we don't waste time finding peers for modules that aren't syncing
        if !self.chunk_orchestrators.contains_key(&storage_module.id) {
            return Vec::new();
        }

        // Extract partition assignment - safe to unwrap since orchestrators are only
        // created for storage modules with valid data partition assignments
        let pa = storage_module.partition_assignment().unwrap();
        let ledger_id = pa.ledger_id.unwrap();

        // Find all peers that are assigned to store data for the same ledger slot
        let active_peers = self.active_sync_peers.read().unwrap();
        let mut candidates: Vec<&PeerBandwidthManager> = active_peers
            .values()
            .filter(|peer_manager| {
                peer_manager.partition_assignments.iter().any(|assignment| {
                    assignment.ledger_id == Some(ledger_id)
                        && assignment.slot_index == pa.slot_index
                })
            })
            .collect();

        // Prioritize healthy peers with available bandwidth capacity
        // Primary sort: health score (reliability, recent performance)
        // Secondary sort: available concurrency (current capacity to handle more requests)
        candidates.sort_by(|a, b| {
            (b.health_score(), b.available_concurrency())
                .partial_cmp(&(a.health_score(), a.available_concurrency()))
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Return the top performers up to the desired count
        candidates
            .into_iter()
            .take(desired_count)
            .map(|peer_manager| peer_manager.miner_address)
            .collect()
    }
}

impl DataSyncService {
    pub fn spawn_service(
        rx: UnboundedReceiver<DataSyncServiceMessage>,
        block_tree: BlockTreeReadGuard,
        storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
        peer_list: PeerList,
        chunk_fetcher_factory: ChunkFetcherFactory,
        service_senders: &ServiceSenders,
        config: &Config,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        let config = config.clone();
        let service_senders = service_senders.clone();
        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let handle = runtime_handle.spawn(async move {
            let data_sync_service = Self {
                shutdown: shutdown_rx,
                msg_rx: rx,
                inner: DataSyncServiceInner::new(
                    block_tree,
                    storage_modules,
                    peer_list,
                    chunk_fetcher_factory,
                    service_senders,
                    config,
                ),
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
                        Some(msg) => self.inner.handle_message(msg)?,
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
            }
        }

        // Process remaining messages before shutdown
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.inner.handle_message(msg)?;
        }

        tracing::info!("shutting down DataSync Service gracefully");
        Ok(())
    }
}
