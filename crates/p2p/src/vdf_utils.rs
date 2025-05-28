use irys_actors::broadcast_mining_service::BroadcastMiningSeed;
use irys_actors::vdf_service::VdfServiceMessage;
use irys_types::{block_production::Seed, H256List, VDFLimiterInfo};
use std::time::Duration;
use tokio::sync::mpsc::{Sender, UnboundedSender};
use tokio::time::sleep;
use tracing::error;

/// Polls VDF service for `VdfState` until `global_step` >= `desired_step`, with a 30s timeout.
pub async fn wait_for_vdf_step(
    vdf_service_sender: UnboundedSender<VdfServiceMessage>,
    desired_step: u64,
) -> eyre::Result<()> {
    let seconds_to_wait = 30;
    let retries_per_second = 20;
    let total_retries = seconds_to_wait * retries_per_second;
    for _ in 0..total_retries {
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
        let vdf_steps_guard = oneshot_rx
            .await
            .expect("to receive VdfStepsReadGuard from GetVdfStateMessage message");
        if vdf_steps_guard.read().global_step >= desired_step {
            return Ok(());
        }
        sleep(Duration::from_millis(1000 / retries_per_second)).await;
    }
    Err(eyre::eyre!(
        "timed out after {seconds_to_wait}s waiting for VDF step {desired_step}"
    ))
}

/// Replay vdf steps on local node, provided by an existing block's VDFLimiterInfo
pub async fn fast_forward_vdf_steps_from_block(
    vdf_limiter_info: VDFLimiterInfo,
    vdf_sender: UnboundedSender<BroadcastMiningSeed>,
) {
    let block_end_step = vdf_limiter_info.global_step_number;
    let len = vdf_limiter_info.steps.len();
    let block_start_step = block_end_step - len as u64;
    tracing::trace!(
        "VDF FF: block start-end step: {}-{}",
        block_start_step,
        block_end_step
    );
    for (i, hash) in vdf_limiter_info.steps.iter().enumerate() {
        //fast forward VDF step and seed before adding the new block...or we wont be at a new enough vdf step to "discover" block
        let mining_seed = BroadcastMiningSeed {
            seed: Seed { 0: *hash },
            global_step: block_start_step + i as u64,
            checkpoints: H256List::new(),
        };

        if let Err(e) = vdf_sender.send(mining_seed) {
            error!("VDF FF: VDF Send Error: {:?}", e);
        }
    }
}
