use crate::block_validation::recall_recall_range_is_valid;
use crate::broadcast_mining_service::BroadcastMiningSeed;
use crate::vdf_service::{vdf_steps_are_valid, VdfServiceMessage, VdfStepsReadGuard};
use crate::{
    block_index_service::BlockIndexReadGuard,
    block_tree_service::{BlockTreeService, ValidationResult, ValidationResultMessage},
    block_validation::poa_is_valid,
    epoch_service::PartitionAssignmentsReadGuard,
};
use actix::{
    Actor, AsyncContext, Context, Handler, Message, Supervised, SystemService, WrapFuture,
};
use base58::ToBase58;
use irys_types::block_production::Seed;
use irys_types::{Config, H256List, IrysBlockHeader};
use nodit::interval::ii;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{Sender, UnboundedSender};
use tokio::time::sleep;
use tracing::{debug, error};

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
    /// Sender to send validated FF steps to VDF thread to be handled
    pub vdf_fast_forward_sender: Sender<BroadcastMiningSeed>,
    /// Sender for the VDF service
    pub vdf_service_sender: UnboundedSender<VdfServiceMessage>,
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
        vdf_fast_forward_sender: Sender<BroadcastMiningSeed>,
        vdf_service_sender: UnboundedSender<VdfServiceMessage>,
    ) -> Self {
        Self {
            block_index_guard,
            partition_assignments_guard,
            vdf_steps_guard,
            config: config.clone(),
            vdf_fast_forward_sender,
            vdf_service_sender,
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

/// Polls VDF service for `VdfState` until `global_step` >= `desired_step`, with a 30s timeout.
pub async fn wait_for_vdf_step(
    vdf_service_sender: UnboundedSender<VdfServiceMessage>,
    desired_step: u64,
) -> eyre::Result<()> {
    let retries_per_second = 20;
    loop {
        tracing::trace!("looping waiting for step {}", desired_step);
        let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
        if let Err(e) = vdf_service_sender.send(VdfServiceMessage::GetVdfStateMessage {
            response: oneshot_tx,
        }) {
            tracing::error!(
                "error sending VdfServiceMessage::GetVdfStateMessage: {:?}",
                e
            );
        };
        // TODO: move before the loop
        let vdf_steps_guard = oneshot_rx
            .await
            .expect("to receive VdfStepsReadGuard from GetVdfStateMessage message");
        if vdf_steps_guard.read().global_step >= desired_step {
            return Ok(());
        }
        sleep(Duration::from_millis(1000 / retries_per_second)).await;
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
        let vdf_steps_guard_for_recall_validation = self.vdf_steps_guard.clone();

        let prev_output = vdf_info.prev_output;
        let vdf_steps_in_block = vdf_info.steps.0.len() as u64;
        // This is the start where block's VDF starts
        let first_step_of_the_block = vdf_info
            .global_step_number
            .saturating_sub(vdf_steps_in_block);

        let info = vdf_info.clone();
        // Spawn VDF validation first
        let vdf_config = self.config.consensus.vdf.clone();
        let vdf_future = tokio::task::spawn_blocking(move || {
            vdf_steps_are_valid(&vdf_info, &vdf_config, vdf_steps_guard.clone())
        });

        //  1. Take the first step from the block, wait for VDF to advance to that step.
        //  2. Check that the first step is equal to the last VDF step.
        //  3. Run the validation, gather steps
        //  4. Send the gathered steps over to VDF thread so it can save them

        debug!("Block {}: {:?}", block_hash.0.to_base58(), info);

        let vdf_service_sender = self.vdf_service_sender.clone();
        let vdf_fast_forward_sender = self.vdf_fast_forward_sender.clone();
        // Wait for results before processing next message
        let config = self.config.clone();
        ctx.wait(
            async move {
                let block_tree_service = BlockTreeService::from_registry();

                let prev_output_step = first_step_of_the_block;
                debug!(
                    "Waiting for the VDF to catch up to step {}",
                    first_step_of_the_block
                );
                if let Err(vdf_error) =
                    wait_for_vdf_step(vdf_service_sender.clone(), prev_output_step)
                        .await
                {
                    error!(
                        "VDF failed to reach a required step for block validation: {:?}",
                        vdf_error
                    );
                    block_tree_service.do_send(ValidationResultMessage {
                        block_hash,
                        validation_result: ValidationResult::Invalid,
                    });
                    return;
                }

                // TODO: there should be no special logic for genesis
                // if first_step_of_the_block > 0 {
                    let stored_steps =
                        match vdf_steps_guard_for_recall_validation.read().get_steps(ii(
                            prev_output_step,
                            prev_output_step,
                        )) {
                            Ok(steps) => steps,
                            Err(error) => {
                                error!("{:?}", error);
                                block_tree_service.do_send(ValidationResultMessage {
                                    block_hash,
                                    validation_result: ValidationResult::Invalid,
                                });
                                return;
                            }
                        };

                    if let Some(stored_previous_output) = stored_steps.get(0) {
                        if *stored_previous_output != prev_output {
                            error!(
                                "Block validation {} failed: Expected stored step {} to be {}, but got {}",
                                block_hash.0.to_base58(), prev_output_step, stored_previous_output.0.to_base58(), prev_output.0.to_base58()
                            );
                            block_tree_service.do_send(ValidationResultMessage {
                                block_hash,
                                validation_result: ValidationResult::Invalid,
                            });
                            return;
                        }
                    } else {
                        error!("Expected to have a required seed");
                        block_tree_service.do_send(ValidationResultMessage {
                            block_hash,
                            validation_result: ValidationResult::Invalid,
                        });
                        return;
                    }
                // }

                let validation_result = match vdf_future.await.unwrap() {
                    Ok(vdf_steps) => {
                        // VDF passed, which means that the steps are valid, now we can store them

                        for step in vdf_steps {
                            debug!("");
                            vdf_fast_forward_sender
                                .send(step)
                                .await
                                .expect("to send vdf_fast_forward_sender");
                        }

                        debug!(
                            "Waiting for the VDF to catch up to step {}",
                            first_step_of_the_block + vdf_steps_in_block
                        );
                        if let Err(vdf_error) = wait_for_vdf_step(
                            vdf_service_sender,
                            first_step_of_the_block + vdf_steps_in_block,
                        )
                        .await
                        {
                            error!(
                                "VDF failed to reach required step for block validation: {:?}",
                                vdf_error
                            );
                            let block_tree_service = BlockTreeService::from_registry();
                            block_tree_service.do_send(ValidationResultMessage {
                                block_hash,
                                validation_result: ValidationResult::Invalid,
                            });
                            return;
                        }

                        // Recall range check
                        match recall_recall_range_is_valid(
                            &block,
                            &config.consensus,
                            &vdf_steps_guard_for_recall_validation,
                        )
                        .await
                        {
                            Ok(()) => {
                                debug!(
                                    block_hash = ?block.block_hash.0.to_base58(),
                                    ?block.height,
                                    "recall_recall_range_is_valid",
                                );
                            }
                            Err(error) => {
                                error!("Recall range is invalid: {}", error);
                                let block_tree_service = BlockTreeService::from_registry();
                                block_tree_service.do_send(ValidationResultMessage {
                                    block_hash,
                                    validation_result: ValidationResult::Invalid,
                                });
                                return;
                            }
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

                block_tree_service.do_send(ValidationResultMessage {
                    block_hash,
                    validation_result,
                });
            }
            .into_actor(self),
        );
    }
}
