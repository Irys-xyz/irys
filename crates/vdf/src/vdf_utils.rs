use crate::VdfStep;
use irys_types::VDFLimiterInfo;
use tokio::sync::mpsc::UnboundedSender;
use tracing::error;

/// Replay vdf steps on local node, provided by an existing block's VDFLimiterInfo
pub fn fast_forward_vdf_steps_from_block(
    vdf_limiter_info: &VDFLimiterInfo,
    vdf_fast_forward_sender: &UnboundedSender<VdfStep>,
) {
    let block_end_step = vdf_limiter_info.global_step_number;
    let block_start_step = vdf_limiter_info.first_step_number();
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
