use crate::block_validation::recall_recall_range_is_valid;
use crate::{
    block_index_service::BlockIndexReadGuard,
    block_tree_service::{BlockTreeServiceMessage, ValidationResult},
    block_validation::poa_is_valid,
    epoch_service::PartitionAssignmentsReadGuard,
    services::ServiceSenders,
};
use actix::{
    Actor, AsyncContext, Context, Handler, Message, Supervised, SystemService, WrapFuture,
};
use irys_types::{Config, IrysBlockHeader};
use irys_vdf::state::{vdf_steps_are_valid, VdfStateReadonly};
use irys_vdf::vdf_utils::fast_forward_vdf_steps_from_block;
use std::sync::Arc;
use tracing::error;

#[derive(Debug)]
pub struct ValidationService {
    /// Read only view of the block index
    pub block_index_guard: BlockIndexReadGuard,
    /// `PartitionAssignmentsReadGuard` for looking up ledger info
    pub partition_assignments_guard: PartitionAssignmentsReadGuard,
    /// VDF steps read guard
    pub vdf_state_readonly: VdfStateReadonly,
    /// Reference to global config for node
    pub config: Config,
    /// Service channels
    pub service_senders: ServiceSenders,
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
        vdf_state_readonly: VdfStateReadonly,
        config: &Config,
        service_senders: &ServiceSenders,
    ) -> Self {
        Self {
            block_index_guard,
            partition_assignments_guard,
            vdf_state_readonly,
            config: config.clone(),
            service_senders: service_senders.clone(),
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
        let block_height = block.height;
        let vdf_info = block.vdf_limiter_info.clone();
        let vdf_to_fast_forward = vdf_info.clone();
        let poa = block.poa.clone();
        let vdf_state_for_validation = self.vdf_state_readonly.clone();
        let vdf_state = self.vdf_state_readonly.clone();

        // Spawn VDF validation first
        let vdf_config = self.config.consensus.vdf.clone();
        let vdf_future = tokio::task::spawn_blocking(move || {
            vdf_steps_are_valid(&vdf_info, &vdf_config, vdf_state_for_validation)
        });

        // Wait for results before processing the next message
        let config = self.config.clone();
        let block_tree_sender = self.service_senders.block_tree.clone();
        let vdf_fast_forward_sender = self.service_senders.vdf_fast_forward.clone();
        ctx.wait(
            async move {
                let first_step_number = vdf_to_fast_forward.first_step_number();
                let prev_output_step_number = first_step_number.saturating_sub(1);

                vdf_state.wait_for_step(prev_output_step_number).await;
                let stored_previous_step = vdf_state.get_step(prev_output_step_number).expect("to get the step, since we've just waited for it");

                if stored_previous_step != vdf_to_fast_forward.prev_output {
                    error!("Previous output from the block {}/{} is not equal to the saved step with the same index. Expected {}, got {}", block_hash, block_height, stored_previous_step, vdf_to_fast_forward.prev_output);
                    block_tree_sender
                        .send(BlockTreeServiceMessage::BlockValidationFinished {
                            block_hash,
                            validation_result: ValidationResult::Invalid,
                        })
                        .unwrap();
                    return;
                }

                let validation_result = match vdf_future.await.unwrap() {
                    Ok(_) => {
                        fast_forward_vdf_steps_from_block(&vdf_to_fast_forward, vdf_fast_forward_sender).await;
                        vdf_state.wait_for_step(vdf_to_fast_forward.global_step_number).await;

                        // Recall range check
                        if let Err(report) = recall_recall_range_is_valid(&block, &config.consensus, &vdf_state).await {
                            error!("Recall range check for block {}/{} failed: {}", block_hash, block_height, report);
                            block_tree_sender
                                .send(BlockTreeServiceMessage::BlockValidationFinished {
                                    block_hash,
                                    validation_result: ValidationResult::Invalid,
                                })
                                .unwrap();
                            return;
                        }

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

                block_tree_sender
                    .send(BlockTreeServiceMessage::BlockValidationFinished {
                        block_hash,
                        validation_result,
                    })
                    .unwrap();
            }
            .into_actor(self),
        );
    }
}
