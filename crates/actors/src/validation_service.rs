use std::sync::Arc;

use actix::{Actor, ArbiterService, Context, Handler, Message, Supervised};
use irys_types::{IrysBlockHeader, StorageConfig};

use crate::{
    block_index_service::BlockIndexReadGuard,
    block_tree_service::{BlockTreeService, ValidationResult, ValidationResultMessage},
    block_validation::poa_is_valid,
    epoch_service::PartitionAssignmentsReadGuard,
};

#[derive(Debug, Default)]
pub struct ValidationService {
    /// Read only view of the block index
    pub block_index_guard: Option<BlockIndexReadGuard>,
    /// `PartitionAssignmentsReadGuard` for looking up ledger info
    pub partition_assignments_guard: Option<PartitionAssignmentsReadGuard>,
    /// Reference to global storage config for node
    pub storage_config: StorageConfig,
}

impl ValidationService {
    /// Creates a new `VdfService` setting up how many steps are stored in memory
    pub fn new(
        block_index_guard: BlockIndexReadGuard,
        partition_assignments_guard: PartitionAssignmentsReadGuard,
        storage_config: StorageConfig,
    ) -> Self {
        Self {
            block_index_guard: Some(block_index_guard),
            partition_assignments_guard: Some(partition_assignments_guard),
            storage_config: storage_config,
        }
    }
}

/// ValidationService is an Actor
impl Actor for ValidationService {
    type Context = Context<Self>;
}

/// Allows this actor to live in the the local service registry
impl Supervised for ValidationService {}

impl ArbiterService for ValidationService {
    fn service_started(&mut self, _ctx: &mut Context<Self>) {
        println!("service started: block_index");
    }
}

#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct RequestValidationMessage(pub Arc<IrysBlockHeader>);

impl Handler<RequestValidationMessage> for ValidationService {
    type Result = ();

    fn handle(&mut self, msg: RequestValidationMessage, _ctx: &mut Self::Context) -> Self::Result {
        if self.partition_assignments_guard.is_none() || self.block_index_guard.is_none() {
            panic!("vdf_service is not initialized");
        }

        let block = msg.0;
        let block_index_guard = self.block_index_guard.clone().unwrap();
        let partitions_guard = self.partition_assignments_guard.clone().unwrap();
        let storage_config = &self.storage_config;
        let miner_address = &block.miner_address;

        // Validates the PoA chunk and vdf steps
        let validation_result = match poa_is_valid(
            &block.poa,
            &block_index_guard,
            &partitions_guard,
            storage_config,
            miner_address,
        ) {
            Ok(_) => ValidationResult::Valid,
            Err(_) => ValidationResult::Invalid,
        };

        let block_tree_service = BlockTreeService::from_registry();
        block_tree_service.do_send(ValidationResultMessage {
            block_hash: block.block_hash,
            validation_result,
        });
    }
}
