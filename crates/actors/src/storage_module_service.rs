use actix::{Actor, Arbiter};
/// # StorageModuleService
///
/// Manages storage modules and their lifecycle within the node.
///
/// This service:
/// - Monitors and applies partition assignments from the network
/// - Initializes storage modules when assigned partitions
/// - Maintains the global registry of active storage modules
/// - Coordinates with the epoch service for runtime updates
/// - Handles dynamic addition/removal of storage modules
///
/// Acts as the central authority for storage module membership, with other
/// components accessing this information through read guards to ensure
/// consistency throughout the system.
use futures::future::Either;
use irys_config::StorageSubmodulesConfig;
use irys_storage::{PackingParams, StorageModule, StorageModuleInfo};
use irys_types::{ArbiterHandle, Config, PartitionChunkRange};
use irys_vdf::vdf_state::VdfStepsReadGuard;
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
use std::{
    path::PathBuf,
    pin::pin,
    sync::{atomic::AtomicU64, Arc, RwLock},
};
use tokio::{
    sync::{mpsc::UnboundedReceiver /*, oneshot*/},
    task::JoinHandle,
};
use tracing::{debug, warn};

use crate::{
    block_tree_service::BlockTreeReadGuard, mining::PartitionMiningActor, packing::PackingRequest,
    ActorAddresses,
};

// Messages that the StorageModuleService service supports
#[derive(Debug)]
pub enum StorageModuleServiceMessage {
    PartitionAssignmentsUpdated {
        storage_module_infos: Arc<Vec<StorageModuleInfo>>,
    },
}

#[derive(Debug)]
pub struct StorageModuleService {
    shutdown: GracefulShutdown,
    msg_rx: UnboundedReceiver<StorageModuleServiceMessage>,
    inner: StorageModuleServiceInner,
}

#[derive(Debug)]
pub struct StorageModuleServiceInner {
    storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
    actor_addresses: ActorAddresses,
    arbiters: Arc<RwLock<Vec<ArbiterHandle>>>,
    block_tree_guard: BlockTreeReadGuard,
    vdf_steps_guard: VdfStepsReadGuard,
    submodules_config: StorageSubmodulesConfig,
    config: Config,
}

impl StorageModuleServiceInner {
    /// Create a new StorageModuleServiceInner instance
    pub fn new(
        storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
        actor_addresses: ActorAddresses,
        arbiters: Arc<RwLock<Vec<ArbiterHandle>>>,
        block_tree_guard: BlockTreeReadGuard,
        vdf_steps_guard: VdfStepsReadGuard,
        config: Config,
    ) -> Self {
        let submodules_config =
            match StorageSubmodulesConfig::load(config.node_config.base_directory.clone()) {
                Ok(sm_config) => sm_config,
                Err(err) => panic!("{}", err),
            };

        Self {
            storage_modules,
            actor_addresses,
            arbiters,
            block_tree_guard,
            vdf_steps_guard,
            submodules_config,
            config,
        }
    }

    async fn handle_message(&mut self, msg: StorageModuleServiceMessage) -> eyre::Result<()> {
        match msg {
            StorageModuleServiceMessage::PartitionAssignmentsUpdated {
                storage_module_infos,
            } => self.handle_partition_assignments_update(storage_module_infos),
        }
    }

    fn handle_partition_assignments_update(
        &mut self,
        storage_module_infos: Arc<Vec<StorageModuleInfo>>,
    ) -> eyre::Result<()> {
        // Read the current storage modules once, outside the loop
        let mut current_modules = self.storage_modules.write().unwrap();
        let mut new_modules: Vec<Arc<StorageModule>> = Vec::new();

        for (i, info) in storage_module_infos.iter().enumerate() {
            // Skip if we don't have a module at this index
            if i >= current_modules.len() {
                // This must be a new partition assignment, create a new StorageModule for it in the global list
                let arc_module = Arc::new(StorageModule::new(&info, &self.config)?);
                current_modules.push(arc_module.clone());
                new_modules.push(arc_module);
                continue;
            }

            // Get the existing module
            let existing = &current_modules[i];

            // Get the path for this module
            let path = &self.submodules_config.submodule_paths[i];

            // Validate the path
            // ARCHITECTURE NOTE: Configuration vs. Implementation Mismatch
            //
            // There's a fundamental disconnect between the configuration system and the storage module design:
            //
            // 1. Original Design Intent:
            //    The StorageModule system was designed to support multiple submodules per StorageModule,
            //    allowing several smaller storage units to be combined into a single 16TB logical partition.
            //
            // 2. Current Configuration Limitation:
            //    The configuration system lacks the capability to express this many-to-one relationship.
            //
            // 3. Testnet Simplification:
            //    For Testnet, we adopt a simplified 1:1 mapping where each StorageModule contains
            //    exactly one submodule representing a full 16TB partition.
            //
            // This limitation should be addressed in future versions to fully realize the original
            // flexible storage architecture. see [`system_ledger::get_genesis_commitments()`] and
            // [`EpochServiceActor::map_storage_modules_to_partition_assignments`] for reference
            if *path != info.submodules[0].1 {
                return Err(eyre::eyre!("Submodule paths don't match"));
            }

            // Validate the module against on-disk packing parameters
            self.validate_packing_params(existing, path, i)?;
        }

        // For each new module, start packing and mining
        for new_sm in new_modules {
            // Message packing actor
            if let Ok(interval) = new_sm.reset() {
                self.actor_addresses.packing.do_send(PackingRequest {
                    storage_module: new_sm.clone(),
                    chunk_range: PartitionChunkRange(interval),
                });
            }

            // Add a mining actor to the global list
            let (global_step_number, _) = self.vdf_steps_guard.read().get_last_step_and_seed();
            let atomic_global_step_number = Arc::new(AtomicU64::new(global_step_number));

            // Create a block tree read guard that lives long enough
            let block_tree_guard = self.block_tree_guard.read();
            let block_hash = block_tree_guard.get_canonical_chain().0.last().unwrap().0;
            let latest_block = block_tree_guard.get_block_and_status(&block_hash);

            // Now we can safely use latest_block
            let initial_difficulty = match latest_block {
                Some(latest_block) => latest_block.0.diff,
                None => panic!("could not determine latest difficulty to start packing"),
            };

            let partition_mining_actor = PartitionMiningActor::new(
                &self.config,
                self.actor_addresses.block_producer.clone().recipient(),
                self.actor_addresses.packing.clone().recipient(),
                new_sm.clone(),
                true, // start mining automatically
                self.vdf_steps_guard.clone(),
                atomic_global_step_number.clone(),
                initial_difficulty,
            );
            let part_arbiter = Arbiter::new();
            let partition_mining_actor =
                PartitionMiningActor::start_in_arbiter(&part_arbiter.handle(), |_| {
                    partition_mining_actor
                });

            let mut arbiter_list = self.arbiters.write().unwrap();

            // Add the part arbiter to the global list of arbiters
            arbiter_list.push(ArbiterHandle::new(
                part_arbiter,
                "partition_arbiter".to_string(),
            ));

            // Add the partition mining actor to the global list
            self.actor_addresses.partitions.push(partition_mining_actor);
        }

        Ok(())
    }

    /// Validates that a storage module's partition assignment matches the on-disk parameters.
    /// Reports an error if there's a mismatch.
    fn validate_packing_params(
        &self,
        module: &StorageModule,
        module_path: &PathBuf,
        index: usize,
    ) -> eyre::Result<()> {
        // Skip modules without partition assignments
        if module.partition_assignment.is_none() {
            warn!(
                "Storage module at index {} has no partition assignment",
                index
            );
            return Ok(());
        }

        // Get the assignment
        let assignment = module.partition_assignment.as_ref().unwrap();

        // Load parameters from disk
        let params_path = module_path.join("packing_params.toml");
        let params = match PackingParams::from_toml(&params_path) {
            Ok(p) => p,
            Err(e) => {
                warn!(
                    "Failed to load packing params for module at index {}: {}",
                    index, e
                );
                return Ok(()); // Skip validation
            }
        };

        // Check all parameters
        let hash_match = assignment.partition_hash == params.partition_hash.unwrap();
        let slot_match = assignment.slot_index == params.slot;
        let ledger_match = assignment.ledger_id == params.ledger;

        // Report overall status
        if hash_match && slot_match && ledger_match {
            debug!(
                "Storage module at index {} matches on-disk parameters",
                index
            );
            return Ok(());
        }

        // Collect detailed mismatch information for error message
        let mut mismatches = Vec::new();

        if !hash_match {
            mismatches.push(format!(
                "partition hash: module={:?}, disk={:?}",
                assignment.partition_hash, params.partition_hash
            ));
        }

        if !slot_match {
            mismatches.push(format!(
                "slot index: module={:?}, disk={:?}",
                assignment.slot_index, params.slot
            ));
        }

        if !ledger_match {
            mismatches.push(format!(
                "ledger ID: module={:?}, disk={:?}",
                assignment.ledger_id, params.ledger
            ));
        }

        // Return a detailed error with all mismatches
        Err(eyre::eyre!(
            "Storage module at index {} has mismatched parameters: {}",
            index,
            mismatches.join(", ")
        ))
    }
}

/// mpsc style service wrapper for the Storage Module Service
impl StorageModuleService {
    /// Spawn a new CommitmentCache service
    pub fn spawn_service(
        exec: &TaskExecutor,
        rx: UnboundedReceiver<StorageModuleServiceMessage>,
        storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
        actor_addresses: &ActorAddresses,
        arbiters: Arc<RwLock<Vec<ArbiterHandle>>>,
        block_tree_guard: BlockTreeReadGuard,
        vdf_steps_guard: VdfStepsReadGuard,
        config: &Config,
    ) -> JoinHandle<()> {
        let actor_addresses = actor_addresses.clone();
        let config = config.clone();
        exec.spawn_critical_with_graceful_shutdown_signal(
            "StorageModule Service",
            |shutdown| async move {
                let pending_storage_module_service = Self {
                    shutdown,
                    msg_rx: rx,
                    inner: StorageModuleServiceInner::new(
                        storage_modules,
                        actor_addresses,
                        arbiters,
                        block_tree_guard,
                        vdf_steps_guard,
                        config,
                    ),
                };
                pending_storage_module_service
                    .start()
                    .await
                    .expect("StorageModule Service encountered an irrecoverable error")
            },
        )
    }

    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting StorageModule Service");

        let mut shutdown_future = pin!(self.shutdown);
        let shutdown_guard = loop {
            let mut msg_rx = pin!(self.msg_rx.recv());
            match futures::future::select(&mut msg_rx, &mut shutdown_future).await {
                Either::Left((Some(msg), _)) => {
                    self.inner.handle_message(msg).await?;
                }
                Either::Left((None, _)) => {
                    tracing::warn!("receiver channel closed");
                    break None;
                }
                Either::Right((shutdown, _)) => {
                    tracing::warn!("shutdown signal received");
                    break Some(shutdown);
                }
            }
        };

        tracing::debug!(amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.inner.handle_message(msg).await?;
        }

        // explicitly inform the TaskManager that we're shutting down
        drop(shutdown_guard);

        tracing::info!("shutting down StorageModule Service");
        Ok(())
    }
}
