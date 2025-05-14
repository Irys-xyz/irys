use std::sync::Arc;

use actix::{Actor, Context, Handler, Message, Supervised, WrapFuture};
use actix::{AsyncContext, SystemService};
use irys_types::{Config, IrysBlockHeader, VdfStepsReadGuard};
use irys_vdf::vdf_steps_are_valid;
use tracing::error;

use crate::{
    block_index_service::BlockIndexReadGuard,
    block_tree_service::{BlockTreeService, ValidationResult, ValidationResultMessage},
    block_validation::poa_is_valid,
    epoch_service::PartitionAssignmentsReadGuard,
};

#[derive(Debug)]
pub struct ValidationService {
    /// Read only view of the block index
    pub block_index_guard: BlockIndexReadGuard,
    /// `PartitionAssignmentsReadGuard` for looking up ledger info
    pub partition_assignments_guard: PartitionAssignmentsReadGuard,
    /// VDF steps read guard
    pub vdf_steps_guard: VdfStepsReadGuard,
    /// Reference to global config for node
    pub config: Config,
}

impl Default for ValidationService {
    fn default() -> Self {
        unimplemented!("don't rely on the default implementation for ValidationService");
    }
}

impl ValidationService {
    /// Creates a new `VdfService` setting up how many steps are stored in memory
    pub fn new(
        block_index_guard: BlockIndexReadGuard,
        partition_assignments_guard: PartitionAssignmentsReadGuard,
        vdf_steps_guard: VdfStepsReadGuard,
        config: &Config,
    ) -> Self {
        Self {
            block_index_guard,
            partition_assignments_guard,
            vdf_steps_guard,
            config: config.clone(),
        }
    }
}

/// `ValidationService` is an Actor
impl Actor for ValidationService {
    type Context = Context<Self>;
}

/// Allows this actor to live in the the local service registry
impl Supervised for ValidationService {}

impl SystemService for ValidationService {
    fn service_started(&mut self, _ctx: &mut Context<Self>) {
        println!("service started: block_index");
    }
}

#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct RequestValidationMessage(pub Arc<IrysBlockHeader>);

impl Handler<RequestValidationMessage> for ValidationService {
    type Result = ();

    fn handle(&mut self, msg: RequestValidationMessage, ctx: &mut Self::Context) -> Self::Result {
        let block = msg.0;
        let block_index_guard = self.block_index_guard.clone();
        let partitions_guard = self.partition_assignments_guard.clone();
        let miner_address = block.miner_address;
        let block_hash = block.block_hash;
        let vdf_info = block.vdf_limiter_info.clone();
        let poa = block.poa.clone();
        let vdf_steps_guard = self.vdf_steps_guard.clone();

        // Spawn VDF validation first
        let vdf_config = self.config.consensus.vdf.clone();
        let vdf_future = tokio::task::spawn_blocking(move || {
            vdf_steps_are_valid(&vdf_info, &vdf_config, vdf_steps_guard)
        });

        // Wait for results before processing next message
        let config = self.config.clone();
        ctx.wait(
            async move {
                let validation_result = match vdf_future.await.unwrap() {
                    Ok(_) => {
                        // VDF passed, now spawn and run PoA validation
                        let poa_future = tokio::task::spawn_blocking(move || {
                            poa_is_valid(
                                &poa,
                                &block_index_guard,
                                &partitions_guard,
                                &config.consensus,
                                &miner_address,
                            )
                        });

                        match poa_future.await.unwrap() {
                            Ok(_) => ValidationResult::Valid,
                            Err(e) => {
                                error!("PoA validation failed: {}", e);
                                ValidationResult::Invalid
                            }
                        }
                    }
                    Err(e) => {
                        error!("VDF validation failed: {}", e);
                        ValidationResult::Invalid
                    }
                };

                let block_tree_service = BlockTreeService::from_registry();
                block_tree_service.do_send(ValidationResultMessage {
                    block_hash,
                    validation_result,
                });
            }
            .into_actor(self),
        );
    }
}
