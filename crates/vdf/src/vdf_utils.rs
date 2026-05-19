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
            Ok(Err(_)) => {
                // Channel closed: the receiver was dropped because `run_vdf` has
                // exited (e.g., graceful exit on a poisoned VDF state lock; see
                // crates/vdf/src/vdf.rs `run_vdf`). This is the same local-
                // infrastructure failure class as the send-timeout arm below —
                // the consumer is dead either way. Panic for the same reason: a
                // dead VDF thread cannot validate any block, so surfacing this
                // as an Err that the caller turns into Invalid would silently
                // fork us off the network on a programmer-error condition. The
                // global panic hook raises SIGINT and the 45 s shutdown watchdog
                // forces process abort, letting the supervisor restart clean.
                // See design/docs/vdf-validation-stall-detection.md.
                panic!(
                    "VDF fast-forward channel closed (receiver dropped) while sending step {}; \
                     run_vdf has exited",
                    start_step_number + i as u64
                );
            }
            Err(_) => {
                // Send timeout means the bounded vdf_fast_forward channel stayed full
                // for `send_timeout` (the configured progress_timeout, default 15s).
                // `run_vdf` fully drains this channel between every ~1s SHA step
                // (see crates/vdf/src/vdf.rs:99), so a healthy consumer cannot fall
                // this far behind — a trip here means run_vdf is dead (poisoned-lock
                // exit) or deadlocked.
                //
                // Governing rule: never mislabel a block as Valid or Invalid. With
                // run_vdf dead we can't validate any block, so surfacing this as an
                // Err that the caller turns into Invalid would silently fork us off
                // the network on a programmer-error condition. Panic instead — the
                // global panic hook (setup_panic_hook) raises SIGINT and the 45s
                // shutdown watchdog forces process abort, letting the supervisor
                // restart the node clean. See design/docs/vdf-validation-stall-detection.md.
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
