pub mod chunk_orchestrator;
pub mod peer_bandwidth_manager;
pub mod peer_stats;

use irys_domain::{BlockTreeReadGuard, ChunkType, PeerList, StorageModule};
use irys_packing::unpack;
use irys_types::{Address, Config, PackedChunk, PartitionChunkOffset, TokioServiceHandle};
use reth::tasks::shutdown::Shutdown;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::warn;

use chunk_orchestrator::ChunkOrchestrator;
use peer_bandwidth_manager::PeerBandwidthManager;

use crate::services::ServiceSenders;

pub struct DataSyncService {
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<DataSyncServiceMessage>,
    pub inner: DataSyncServiceInner,
}

type StorageModuleId = usize;

pub struct DataSyncServiceInner {
    pub block_tree: BlockTreeReadGuard,
    pub storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
    pub all_peers: Arc<RwLock<HashMap<Address, PeerBandwidthManager>>>,
    pub chunk_orchestrators: HashMap<StorageModuleId, ChunkOrchestrator>,
    pub peer_list: PeerList,
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
}

impl DataSyncServiceInner {
    pub fn new(
        block_tree: BlockTreeReadGuard,
        storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
        peer_list: PeerList,
        service_senders: ServiceSenders,
        config: Config,
    ) -> Self {
        let mut data_sync = Self {
            block_tree,
            storage_modules,
            peer_list,
            all_peers: Default::default(),
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
            } => self.on_chunk_failed(storage_module_id, chunk_offset, peer_addr)?,
            DataSyncServiceMessage::PeerDisconnected { peer_addr } => {
                self.handle_peer_disconnection(peer_addr);
            }
        }
        Ok(())
    }

    pub fn tick(&mut self) -> eyre::Result<()> {
        for orchestrator in self.chunk_orchestrators.values_mut() {
            orchestrator.tick()?;
        }
        Ok(())
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
        self.all_peers.write().unwrap().remove(&peer_addr);
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

            // Only sync peers for storage modules that need data
            let entropy_intervals = storage_module.get_intervals(ChunkType::Entropy);
            if entropy_intervals.is_empty() {
                continue;
            }

            self.sync_peers_for_ledger(ledger_id);
        }
    }

    fn sync_peers_for_ledger(&mut self, ledger_id: u32) {
        let epoch_snapshot = self.block_tree.read().canonical_epoch_snapshot();

        let slot_assignments: Vec<_> = epoch_snapshot
            .partition_assignments
            .data_partitions
            .values()
            .filter(|a| a.ledger_id == Some(ledger_id))
            .copied()
            .collect();

        for pa in slot_assignments {
            let Some(peer) = self.peer_list.peer_by_mining_address(&pa.miner_address) else {
                continue;
            };

            // Get existing peer bandwidth manager or add a new one for the peer
            let mut all_peers = self.all_peers.write().unwrap();
            let entry = all_peers
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

            // Create orchestrator for storage modules that needs to sync data
            let orchestrator =
                ChunkOrchestrator::new(sm.clone(), self.all_peers.clone(), &self.service_senders);

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

        // Apply the peer_updates
        for (sm_id, best_peers) in peer_updates {
            // Skip ff we don't have an orchestrator for this storage_module
            let Some(orchestrator) = self.chunk_orchestrators.get_mut(&sm_id) else {
                warn!("Storage module with id: {sm_id} does not have a chunk_orchestrator and it should.");
                continue;
            };

            // Add new peers
            // Note: orchestrator.add_peer() filters dupes
            for peer_addr in &best_peers {
                orchestrator.add_peer(*peer_addr);
            }

            // Remove peers that are no longer in the best_peers list
            let current_peers: Vec<Address> = orchestrator.current_peers.clone();
            for peer_addr in current_peers {
                if !best_peers.contains(&peer_addr) {
                    orchestrator.remove_peer(peer_addr);
                }
            }
        }
    }

    fn get_best_available_peers(
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
        let all_peers = self.all_peers.read().unwrap();
        let mut candidates: Vec<&PeerBandwidthManager> = all_peers
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
