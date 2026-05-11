use crate::VdfStep;
use irys_types::Traced;
use irys_types::{H256, VDFLimiterInfo};
use tokio::sync::mpsc::Sender;
use tokio::time::{Duration, timeout};

/// Replay a contiguous validated prefix of VDF steps into local state.
#[tracing::instrument(level = "trace", skip_all, err)]
pub async fn fast_forward_validated_steps(
    start_step_number: u64,
    steps: &[H256],
    vdf_fast_forward_sender: &Sender<Traced<VdfStep>>,
    send_timeout: Duration,
) -> eyre::Result<()> {
    let end_step_number = start_step_number + steps.len().saturating_sub(1) as u64;
    tracing::trace!(
        "VDF FF: validated batch step range: {}-{}",
        start_step_number,
        end_step_number
    );
    for (i, hash) in steps.iter().enumerate() {
        match timeout(
            send_timeout,
            vdf_fast_forward_sender.send(Traced::new(VdfStep {
                step: *hash,
                global_step_number: start_step_number + i as u64,
            })),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(err.into()),
            Err(_) => {
                // Send timeout means the bounded vdf_fast_forward channel stayed full
                // for `send_timeout` (the configured progress_timeout, default 15s).
                // `run_vdf` fully drains this channel between every ~1s SHA step
                // (see crates/vdf/src/vdf.rs:99), so a healthy consumer cannot fall
                // this far behind — a trip here means run_vdf is dead (poisoned-lock
                // exit) or deadlocked. The panic + global panic hook aborts the
                // process so the supervisor can restart cleanly; returning Err here
                // would only mask the underlying VDF-thread failure.
                panic!(
                    "VDF fast-forward channel remained full for {:?} while sending step {}",
                    send_timeout,
                    start_step_number + i as u64
                );
            }
        }
    }
    Ok(())
}

/// Replay vdf steps on local node, provided by an existing block's VDFLimiterInfo
/// Assumes the provided steps have been *FULLY VALIDATED*
#[tracing::instrument(level = "trace", skip_all, err)]
pub async fn fast_forward_vdf_steps_from_block(
    vdf_limiter_info: &VDFLimiterInfo,
    vdf_fast_forward_sender: &Sender<Traced<VdfStep>>,
    send_timeout: Duration,
) -> eyre::Result<()> {
    let block_end_step = vdf_limiter_info.global_step_number;
    let block_start_step = vdf_limiter_info.first_step_number();
    tracing::trace!(
        "VDF FF: block start-end step: {}-{}",
        block_start_step,
        block_end_step
    );
    fast_forward_validated_steps(
        block_start_step,
        &vdf_limiter_info.steps.0,
        vdf_fast_forward_sender,
        send_timeout,
    )
    .await
}
