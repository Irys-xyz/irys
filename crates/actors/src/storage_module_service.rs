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
use irys_config::StorageSubmodulesConfig;
use irys_storage::{PackingParams, StorageModule, StorageModuleInfo};
use irys_types::{Config, PartitionChunkRange, TokioServiceHandle};
use reth::tasks::shutdown::Shutdown;
use std::{
    path::Path,
    sync::{Arc, RwLock},
};
use tokio::sync::{mpsc::UnboundedReceiver /*, oneshot*/};
use tracing::{debug, warn, Span};

use crate::{packing::PackingRequest, ActorAddresses};

// Messages that the StorageModuleService service supports
#[derive(Debug)]
pub enum StorageModuleServiceMessage {
    PartitionAssignmentsUpdated {
        storage_module_infos: Arc<Vec<StorageModuleInfo>>,
    },
}

#[derive(Debug)]
pub struct StorageModuleService {
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<StorageModuleServiceMessage>,
    inner: StorageModuleServiceInner,
}

#[derive(Debug)]
pub struct StorageModuleServiceInner {
    storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
    actor_addresses: ActorAddresses,
    submodules_config: StorageSubmodulesConfig,
    _config: Config,
}

impl StorageModuleServiceInner {
    /// Create a new StorageModuleServiceInner instance
    pub fn new(
        storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
        actor_addresses: ActorAddresses,
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
            submodules_config,
            _config: config,
        }
    }

    fn handle_message(&mut self, msg: StorageModuleServiceMessage) -> eyre::Result<()> {
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
        let span = Span::current();
        let _span = span.enter();

        // Read the current storage modules once, outside the loop
        let current_modules = self.storage_modules.read().unwrap();
        let mut updated_modules: Vec<Arc<StorageModule>> = Vec::new();

        for (i, info) in storage_module_infos.iter().enumerate() {
            // Get the existing StorageModule
            let existing = &current_modules[i];

            // Did this storage module get assigned a new partition_hash ?
            if existing.partition_assignment().is_none() && info.partition_assignment.is_some() {
                existing.assign_partition(info.partition_assignment.unwrap());
                // Record this storage module as updated
                updated_modules.push(existing.clone());

                // Skip any further validations for now
                continue;
            }

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
            if info.partition_assignment.is_some() {
                match self.validate_packing_params(existing, path, i) {
                    Ok(()) => {}
                    Err(err) => panic!("{}", err),
                }
            }
        }

        // For each updated module, start packing and mining
        for updated_sm in updated_modules {
            // Message packing actor
            if let Ok(interval) = updated_sm.reset() {
                self.actor_addresses.packing.do_send(PackingRequest {
                    storage_module: updated_sm.clone(),
                    chunk_range: PartitionChunkRange(interval),
                });
            }
        }

        Ok(())
    }

    /// Validates that a storage module's partition assignment matches the on-disk parameters.
    /// Reports an error if there's a mismatch.
    fn validate_packing_params(
        &self,
        module: &StorageModule,
        module_path: &Path,
        index: usize,
    ) -> eyre::Result<()> {
        // Skip modules without partition assignments
        if module.partition_assignment().is_none() {
            warn!(
                "Storage module at index {} has no partition assignment",
                index
            );
            return Ok(());
        }

        // Get the assignment
        let assignment = module.partition_assignment().unwrap();

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
    /// Spawn a new StorageModule service
    pub fn spawn_service(
        rx: UnboundedReceiver<StorageModuleServiceMessage>,
        storage_modules: Arc<RwLock<Vec<Arc<StorageModule>>>>,
        actor_addresses: &ActorAddresses,
        config: &Config,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        tracing::info!("Spawning storage module service");

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let actor_addresses = actor_addresses.clone();
        let config = config.clone();

        let handle = runtime_handle.spawn(async move {
            let pending_storage_module_service = Self {
                shutdown: shutdown_rx,
                msg_rx: rx,
                inner: StorageModuleServiceInner::new(storage_modules, actor_addresses, config),
            };
            pending_storage_module_service
                .start()
                .await
                .expect("StorageModule Service encountered an irrecoverable error")
        });

        TokioServiceHandle {
            name: "storage_module_service".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting StorageModule Service");

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

        tracing::info!("shutting down StorageModule Service gracefully");
        Ok(())
    }
}
