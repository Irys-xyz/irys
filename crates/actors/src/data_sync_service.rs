pub mod chunk_fetcher;
pub mod chunk_orchestrator;
pub mod peer_bandwidth_manager;
pub mod peer_stats;

use crate::{chunk_fetcher::ChunkFetcherFactory, metrics, services::ServiceSenders};
use chunk_orchestrator::ChunkOrchestrator;
use irys_domain::{BlockTreeReadGuard, ChunkType, PeerList, StorageModule};
use irys_packing::unpack;
use irys_types::{
    Config, IrysAddress, PackedChunk, PartitionChunkOffset, SendTraced as _, TokioServiceHandle,
    Traced,
};
use peer_bandwidth_manager::PeerBandwidthManager;
use reth::tasks::shutdown::Shutdown;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::{mpsc::UnboundedReceiver, oneshot};
use tracing::{Instrument as _, debug, error, warn};

pub struct DataSyncService {
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<Traced<DataSyncServiceMessage>>,
    pub inner: DataSyncServiceInner,
}

type StorageModuleId = usize;

pub struct DataSyncServiceInner {
    pub block_tree: BlockTreeReadGuard,
    pub storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
    pub active_peer_bandwidth_managers: Arc<RwLock<HashMap<IrysAddress, PeerBandwidthManager>>>,
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
        peer_address: IrysAddress,
        chunk: PackedChunk,
    },
    ChunkFailed {
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
    },
    ChunkTimedOut {
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_address: IrysAddress,
    },
    PeerListUpdated,
    PeerDisconnected {
        peer_address: IrysAddress,
    },
    GetActivePeersList(oneshot::Sender<Arc<RwLock<HashMap<IrysAddress, PeerBandwidthManager>>>>),
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
            active_peer_bandwidth_managers: Default::default(),
            chunk_fetcher_factory,
            chunk_orchestrators: Default::default(),
            service_senders,
            config,
        };
        data_sync.synchronize_peers_and_orchestrators();
        data_sync
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub fn handle_message(&mut self, msg: DataSyncServiceMessage) -> eyre::Result<()> {
        match msg {
            DataSyncServiceMessage::SyncPartitions => {
                self.synchronize_peers_and_orchestrators();
            }
            DataSyncServiceMessage::ChunkCompleted {
                storage_module_id,
                chunk_offset,
                peer_address: peer_addr,
                chunk,
            } => {
                metrics::record_data_sync_chunk_completed();
                if let Err(e) =
                    self.on_chunk_completed(storage_module_id, chunk_offset, peer_addr, chunk)
                {
                    error!(
                        "Failed to handle chunk completion for storage_module {} chunk_offset {} from peer {}: {e:?}",
                        storage_module_id, chunk_offset, peer_addr
                    );
                }
            }
            DataSyncServiceMessage::ChunkFailed {
                storage_module_id,
                chunk_offset,
                peer_addr,
            } => {
                metrics::record_data_sync_chunk_failure();
                if let Err(e) = self.on_chunk_failed(storage_module_id, chunk_offset, peer_addr) {
                    error!(
                        "Failed to handle chunk failure for storage_module {} chunk_offset {} from peer {}: {e:?}",
                        storage_module_id, chunk_offset, peer_addr
                    );
                }
            }
            DataSyncServiceMessage::ChunkTimedOut {
                storage_module_id,
                chunk_offset,
                peer_address: peer_addr,
            } => {
                metrics::record_data_sync_chunk_failure();
                if let Err(e) = self.on_chunk_timeout(storage_module_id, chunk_offset, peer_addr) {
                    error!(
                        "Failed to handle chunk timeout for storage_module {} chunk_offset {} from peer {}: {e:?}",
                        storage_module_id, chunk_offset, peer_addr
                    );
                }
            }
            DataSyncServiceMessage::PeerListUpdated => self.handle_peer_list_updated(),
            DataSyncServiceMessage::PeerDisconnected {
                peer_address: peer_addr,
            } => self.handle_peer_disconnection(peer_addr),
            DataSyncServiceMessage::GetActivePeersList(tx) => self.handle_get_active_peers_list(tx),
        }
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    pub fn tick(&mut self) -> eyre::Result<()> {
        for orchestrator in self.chunk_orchestrators.values_mut() {
            orchestrator.tick()?;
        }
        self.optimize_peer_concurrency();
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn optimize_peer_concurrency(&mut self) {
        // Get a write lock on the peer bandwidth managers list
        let Ok(mut peers) = self.active_peer_bandwidth_managers.write() else {
            return;
        };

        // Build a list of score tuples for the peer bandwidth managers (Address, health_score, active_requests, max_concurrency)
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
                // debug!(
                //     "Not increasing concurrency for peer {} max_concurrency {} (concurrent utilization: {:.1}%, health: {:.2})",
                //     peer_addr,
                //     current_max,
                //     utilization_ratio * 100.0,
                //     health_score
                // );
            }
        }
    }

    #[tracing::instrument(level = "trace", skip_all, fields(
        chunk.storage_module_id = storage_module_id,
        chunk.offset = %chunk_offset,
        peer.address = %peer_addr
    ))]
    fn on_chunk_completed(
        &mut self,
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
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

        // Attempt to write the chunk directly to the sm, if it doesn't succeed
        // for a variety of reasons, indexes not initialized, no packing etc etc...
        let sm = self
            .storage_modules
            .read()
            .unwrap()
            .get(storage_module_id)
            .unwrap()
            .clone();
        if sm.write_data_chunk(&unpacked_chunk).is_err() {
            // ..then, send the unpacked chunk to the mempool and let the it do it's thing.
            if let Err(e) = self.service_senders.chunk_ingress.send_traced(
                crate::chunk_ingress_service::ChunkIngressMessage::IngestChunk(
                    unpacked_chunk,
                    None,
                ),
            ) {
                tracing::warn!(
                    "Failed to send ChunkIngressMessage to chunk ingress channel: {:?}",
                    e
                );
            }
        }

        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    fn on_chunk_failed(
        &mut self,
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
    ) -> eyre::Result<()> {
        if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
            orchestrator.on_chunk_failed(chunk_offset, peer_addr)?;

            let pa = orchestrator
                .storage_module
                .partition_assignment()
                .expect("A partition assignment present");
            debug!(
                "chunk failed: ledger:{:?}, slot_index:{:?} chunk_offset:{} peer:{}",
                pa.ledger_id, pa.slot_index, chunk_offset, peer_addr
            );
        }
        Ok(())
    }

    #[tracing::instrument(level = "trace", skip_all, err)]
    fn on_chunk_timeout(
        &mut self,
        storage_module_id: usize,
        chunk_offset: PartitionChunkOffset,
        peer_addr: IrysAddress,
    ) -> eyre::Result<()> {
        // TODO: Opportunity to do custom timeout tracking/handling here
        debug!("chunk timed out: {} peer:{}", chunk_offset, peer_addr);
        if let Some(orchestrator) = self.chunk_orchestrators.get_mut(&storage_module_id) {
            orchestrator.on_chunk_failed(chunk_offset, peer_addr)?;
        }
        Ok(())
    }

    fn handle_peer_list_updated(&mut self) {
        self.sync_peer_partition_assignments();
    }

    fn handle_peer_disconnection(&mut self, peer_addr: IrysAddress) {
        // Remove peer from all orchestrators
        for orchestrator in self.chunk_orchestrators.values_mut() {
            orchestrator.remove_peer(peer_addr);
        }

        // Remove from peer list
        self.active_peer_bandwidth_managers
            .write()
            .unwrap()
            .remove(&peer_addr);
    }

    #[tracing::instrument(level = "trace", skip_all)]
    fn handle_get_active_peers_list(
        &self,
        tx: oneshot::Sender<Arc<RwLock<HashMap<IrysAddress, PeerBandwidthManager>>>>,
    ) {
        if let Err(e) = tx.send(self.active_peer_bandwidth_managers.clone()) {
            tracing::error!("handle_get_active_peers_list() tx.send() error: {:?}", e);
        };
    }

    fn synchronize_peers_and_orchestrators(&mut self) {
        self.sync_peer_partition_assignments();
        self.create_chunk_orchestrators();
        self.update_orchestrator_peers();
        metrics::record_data_sync_active_peers(
            self.active_peer_bandwidth_managers.read().unwrap().len() as u64,
        );
    }

    /// Synchronizes peer bandwidth managers with current network peers and local
    /// storage module assignments.
    ///
    /// For each local storage module assigned to a data ledger slot:
    /// - Checks if the module has entropy chunks requiring data
    /// - Ensures PeerBandwidthManagers exist for all peers storing relevant partition data
    ///
    /// This maintains an up-to-date mapping between peers and bandwidth managers
    /// for efficient chunk downloading across the network.
    #[tracing::instrument(level = "trace", skip_all)]
    fn sync_peer_partition_assignments(&mut self) {
        let storage_modules = self.storage_modules.read().unwrap().clone();

        // Loop though all the storage modules managed by the local node
        for storage_module in storage_modules {
            // Skip any storage modules not assigned to a data ledger
            let Some(pa) = *storage_module.partition_assignment.read().unwrap() else {
                continue;
            };

            let Some(ledger_id) = pa.ledger_id else {
                continue;
            };

            let Some(slot_index) = pa.slot_index else {
                continue;
            };

            // Check to see if the storage module has any entropy (packed) chunks that need data
            let entropy_intervals = storage_module.get_intervals(ChunkType::Entropy);
            if entropy_intervals.is_empty() {
                debug!("StorageModule has no entropy chunks\n{:?}", pa);
                continue;
            }

            // If it does, ensure there's a bandwidth manager for any peer storing the data for this storage module
            self.ensure_bandwidth_managers_for_peers(ledger_id, slot_index);
        }
    }

    /// Updates the active_peers list and ensures there are PeerBandwidthManagers for
    /// any peers assigned to store the same slot data.
    #[tracing::instrument(level = "trace", skip_all)]
    fn ensure_bandwidth_managers_for_peers(&mut self, ledger_id: u32, slot_index: usize) {
        // Get the slot assignments for all partition hashes in this slot
        let epoch_snapshot = self.block_tree.read().canonical_epoch_snapshot();
        let slot_assignments: Vec<_> = epoch_snapshot
            .partition_assignments
            .data_partitions
            .values()
            .filter(|a| a.ledger_id == Some(ledger_id) && a.slot_index == Some(slot_index))
            .copied()
            .collect();

        // Loop though all of this slots assigned partition_hashes
        for pa in slot_assignments {
            // Use the mining address in the assignment to retrieve a peer from the global peer_list
            let Some(peer) = self.peer_list.peer_by_mining_address(&pa.miner_address) else {
                continue;
            };

            // Get existing entry for a peer bandwidth manager or add a new one for the peer
            let mut active_peers = self.active_peer_bandwidth_managers.write().unwrap();
            let entry = active_peers
                .entry(pa.miner_address)
                .or_insert(PeerBandwidthManager::new(
                    &pa.miner_address,
                    &peer,
                    &self.config,
                ));

            // Finally let the peer bandwidth manager for this peer store a reference to this partition assignment
            // so we can filter the active_peer_bandwidth_managers list for peers assigned to this ledger/slot in the future
            if !entry.partition_assignments.contains(&pa) {
                debug!(
                    "Adding partition assignment: {:#?} to Peer: {}",
                    pa, entry.miner_address
                );
                entry.partition_assignments.push(pa);
            }
            // active_peers dropped here
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

        // Drop orchestrators for modules that no longer hold a data ledger assignment
        self.chunk_orchestrators.retain(|sm_id, _| {
            storage_modules
                .get(*sm_id)
                .and_then(|sm| sm.partition_assignment())
                .and_then(|pa| pa.ledger_id)
                .is_some()
        });

        for sm in storage_modules {
            let sm_id = sm.id;

            // Skip if we already have a chunk orchestrator for this storage module
            if self.chunk_orchestrators.contains_key(&sm_id) {
                continue;
            }

            // Skip unused storage modules without partition assignments (not yet capacity or data)
            let Some(pa) = sm.partition_assignment() else {
                continue;
            };

            // Skip capacity partitions - they store entropy, not data chunks that need syncing
            if pa.ledger_id.is_none() {
                continue;
            }

            // Use the factory to create a chunk_fetcher (allows mock chunk fetchers for testing)
            let chunk_fetcher = (self.chunk_fetcher_factory)(pa.ledger_id.unwrap());

            // Create a chunk orchestrator for storage modules that needs to sync data
            let orchestrator = ChunkOrchestrator::new(
                sm.clone(),
                self.active_peer_bandwidth_managers.clone(),
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
        let mut peer_updates: Vec<(StorageModuleId, Vec<IrysAddress>)> = Vec::new();

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
                warn!(
                    "Storage module with id: {sm_id} does not have a chunk_orchestrator and it should."
                );
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
    ) -> Vec<IrysAddress> {
        // Only return peers for storage modules that have active chunk orchestrators
        // This ensures we don't waste time finding peers for modules that aren't syncing
        if !self.chunk_orchestrators.contains_key(&storage_module.id) {
            return Vec::new();
        }

        // Extract partition assignment
        let pa = storage_module.partition_assignment().unwrap();

        // Check to see that the partition hash hasn't been re-assigned to capacity and no longer
        // has any data to sync
        if pa.ledger_id.is_none() {
            // We don't remove the orchestrator for the partition because it's not hurting anything to keep it around...
            return Vec::new();
        }

        let ledger_id = pa.ledger_id.unwrap();

        // Find all peers that are assigned to store data for the same ledger slot
        let active_peers = self.active_peer_bandwidth_managers.read().unwrap();
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
        rx: UnboundedReceiver<Traced<DataSyncServiceMessage>>,
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

        let handle = runtime_handle.spawn(
            async move {
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
            }
            .instrument(tracing::Span::current()),
        );

        TokioServiceHandle {
            name: "data_sync_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    #[tracing::instrument(level = "trace", skip_all)]
    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting DataSync Service");

        let mut interval = tokio::time::interval(Duration::from_millis(250));
        interval.tick().await; // Skip first immediate tick

        // Subscribe to peer lifecycle events for event-driven synchronization
        let mut peer_events_rx = self.inner.peer_list.subscribe_to_peer_events();

        loop {
            tokio::select! {
                biased;

                _ = &mut self.shutdown => {
                    tracing::info!("Shutdown signal received for DataSync Service");
                    break;
                }

                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(traced) => {
                            let (msg, _entered) = traced.into_inner();
                            self.inner.handle_message(msg)?;
                        }
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

                evt = peer_events_rx.recv() => {
                    match evt {
                        Ok(irys_domain::PeerEvent::BecameActive { .. }) => {
                            // New active peer available; resync orchestrators/managers
                            self.inner.synchronize_peers_and_orchestrators();
                        }
                        Ok(irys_domain::PeerEvent::BecameInactive { mining_addr, .. }) => {
                            // Peer no longer active; resync
                            debug!("Peer became inactive: {}", mining_addr);
                            self.inner.synchronize_peers_and_orchestrators();
                        }
                        Ok(irys_domain::PeerEvent::PeerUpdated { .. }) => {
                            // Metadata changed; just refresh orchestrator peer sets
                            self.inner.update_orchestrator_peers();
                        }
                        Ok(irys_domain::PeerEvent::PeerRemoved { mining_addr, .. }) => {
                            // Treat same as disconnect
                            debug!("Peer removed: {}", mining_addr);
                            self.inner.handle_peer_disconnection(mining_addr);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // Missed events; do a conservative resync
                            self.inner.synchronize_peers_and_orchestrators();
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            warn!("peer events channel closed in DataSyncService; resubscribing");
                        }
                    }
                }
            }
        }

        // Process remaining messages before shutdown
        while let Ok(traced) = self.msg_rx.try_recv() {
            let (msg, _entered) = traced.into_inner();
            self.inner.handle_message(msg)?;
        }

        tracing::info!("shutting down DataSync Service gracefully");
        Ok(())
    }
}
