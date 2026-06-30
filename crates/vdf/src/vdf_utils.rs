use crate::VdfStep;
use irys_types::H256;
use irys_types::Traced;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::time::{Duration, timeout};

/// Bound for the inbound VDF fast-forward channel. Owned by `irys-vdf` so the
/// VDF crate, not the actor hub, defines the contract. `run_vdf` fully drains
/// this channel between each ~1s SHA step, so a healthy consumer never fills it.
pub const VDF_FAST_FORWARD_CHANNEL_CAPACITY: usize = 4_096;

/// Sending half of the VDF fast-forward channel.
///
/// Nominal newtype over `Sender<Traced<VdfStep>>` that exposes only
/// [`Self::send_validated_batch`]. Steps pushed through it must already be
/// validated, or explicitly trusted by the sync path (`skip_vdf_validation`); the
/// type makes the raw `Sender` (which carries no such invariant) unreachable
/// outside this crate.
#[derive(Debug, Clone)]
pub struct VdfFastForwardSender(Sender<Traced<VdfStep>>);

impl VdfFastForwardSender {
    /// Replay a contiguous batch of VDF steps into local state. The steps must
    /// already be validated, or explicitly trusted by the sync path. Delegates to
    /// [`fast_forward_validated_steps`] (same tracing span and fail-stop
    /// semantics).
    pub async fn send_validated_batch(
        &self,
        start_step_number: u64,
        steps: &[H256],
        send_timeout: Duration,
    ) -> eyre::Result<()> {
        fast_forward_validated_steps(start_step_number, steps, &self.0, send_timeout).await
    }
}

/// Create the bounded VDF fast-forward channel. The hub holds the
/// [`VdfFastForwardSender`]; `run_vdf` consumes the `Receiver`.
pub fn fast_forward_channel() -> (VdfFastForwardSender, Receiver<Traced<VdfStep>>) {
    let (tx, rx) = tokio::sync::mpsc::channel(VDF_FAST_FORWARD_CHANNEL_CAPACITY);
    (VdfFastForwardSender(tx), rx)
}

/// Replay a contiguous validated prefix of VDF steps into local state.
#[tracing::instrument(level = "trace", skip_all, err)]
pub(crate) async fn fast_forward_validated_steps(
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

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::H256;
    use std::time::Duration;

    #[tokio::test]
    async fn factory_delivers_validated_steps() {
        let (sender, mut rx) = fast_forward_channel();
        let steps = [H256::from_low_u64_be(1), H256::from_low_u64_be(2)];
        sender
            .send_validated_batch(10, &steps, Duration::from_secs(1))
            .await
            .expect("validated batch should send");

        let (first, _span) = rx.recv().await.expect("first step").into_parts();
        assert_eq!(first.global_step_number, 10);
        assert_eq!(first.step, steps[0]);
        let (second, _span) = rx.recv().await.expect("second step").into_parts();
        assert_eq!(second.global_step_number, 11);
        assert_eq!(second.step, steps[1]);
    }

    // Fail-stop: a dead receiver (run_vdf exited) must panic, never return an Err
    // that could let validation mislabel a block. Verified through the factory +
    // newtype.
    #[tokio::test]
    #[should_panic(expected = "channel closed")]
    async fn send_panics_when_receiver_dropped() {
        let (sender, rx) = fast_forward_channel();
        drop(rx);
        sender
            .send_validated_batch(1, &[H256::zero()], Duration::from_millis(50))
            .await
            .expect("must panic before returning");
    }

    // Fail-stop on send-timeout. Filling past capacity also proves the channel is
    // bounded at VDF_FAST_FORWARD_CHANNEL_CAPACITY (an unbounded channel would
    // never time out). `_rx` is held (not dropped) so this is the timeout arm, not
    // the closed arm.
    #[tokio::test]
    #[should_panic(expected = "remained full")]
    async fn send_panics_on_timeout_when_channel_full() {
        let (sender, _rx) = fast_forward_channel();
        let steps = vec![H256::zero(); VDF_FAST_FORWARD_CHANNEL_CAPACITY + 1];
        sender
            .send_validated_batch(1, &steps, Duration::from_millis(50))
            .await
            .expect("must panic before returning");
    }
}
