use crate::state::VdfStateReadonly;
use crate::VdfStep;
use irys_types::VDFLimiterInfo;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::sleep;
use tracing::error;

/// Polls VDF service for `VdfState` until `global_step` >= `desired_step`, with a 30s timeout.
pub async fn wait_for_vdf_step(
    vdf_steps_guard: &VdfStateReadonly,
    desired_step: u64,
) -> eyre::Result<()> {
    let seconds_to_wait = 30;
    let retries_per_second = 20;
    let total_retries = seconds_to_wait * retries_per_second;
    for _ in 0..total_retries {
        tracing::trace!("looping waiting for step {}", desired_step);
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
    vdf_fast_forward_sender: UnboundedSender<VdfStep>,
) {
    let block_end_step = vdf_limiter_info.global_step_number;
    let len = vdf_limiter_info.steps.len();
    let block_start_step = block_end_step - len as u64 + 1;
    tracing::trace!(
        "VDF FF: block start-end step: {}-{}",
        block_start_step,
        block_end_step
    );
    for (i, hash) in vdf_limiter_info.steps.iter().enumerate() {
        if let Err(e) = vdf_fast_forward_sender.send(VdfStep {
            step: *hash,
            global_step_number: block_start_step + i as u64,
        }) {
            error!("VDF FF: VDF Send Error: {:?}", e);
        }
    }
}
