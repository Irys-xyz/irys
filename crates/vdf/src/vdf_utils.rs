use crate::VdfStep;
use irys_types::H256;
use irys_types::Traced;
use irys_types::block_production::Seed;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;
use tokio::time::{Duration, timeout};

/// Bound for the inbound VDF fast-forward channel. Owned by `irys-vdf` so the
/// VDF crate, not the actor hub, defines the contract. `run_vdf` fully drains
/// this channel between each ~1s SHA step, so a healthy consumer never fills it.
pub const VDF_FAST_FORWARD_CHANNEL_CAPACITY: usize = 4_096;

/// Shared re-anchor generation + buffer-suspect flag (FF senders, `run_vdf`,
/// block-tree gate, recall-range validation). Generation bumps on applied heal;
/// suspect is set while a heal is pending/running.
#[derive(Debug, Clone)]
pub struct ReanchorSignals {
    generation: Arc<AtomicU64>,
    buffer_suspect: Arc<AtomicBool>,
}

impl ReanchorSignals {
    pub(crate) fn new() -> Self {
        Self {
            generation: Arc::new(AtomicU64::new(0)),
            buffer_suspect: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Current re-anchor generation (`0` until first applied heal). Acquire.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// After `reanchor_seeds`: bump generation and clear suspect (`run_vdf` only).
    pub(crate) fn record_heal_applied(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.buffer_suspect.store(false, Ordering::Release);
        crate::metrics::record_buffer_suspect(false);
        crate::metrics::record_reanchor_healed();
    }

    /// Mark buffer untrusted (gate before queue; `run_vdf` on consume). Release.
    pub fn mark_buffer_suspect(&self) {
        self.buffer_suspect.store(true, Ordering::Release);
        crate::metrics::record_buffer_suspect(true);
    }

    /// True while a re-anchor is pending/running — do not trust the seed buffer.
    pub fn is_buffer_suspect(&self) -> bool {
        self.buffer_suspect.load(Ordering::Acquire)
    }
}

#[cfg(any(test, feature = "test-utils"))]
impl ReanchorSignals {
    pub fn test_record_heal_applied(&self) {
        self.record_heal_applied();
    }
}

/// Generation changed mid-validation; batch must revalidate (not invalid).
#[derive(Debug, thiserror::Error)]
#[error(
    "VDF re-anchor generation changed during validation (captured {captured}, current {current}); batch dropped for revalidation"
)]
pub struct VdfReanchorGenerationChanged {
    pub captured: u64,
    pub current: u64,
}

/// Fast-forward send half: only validated (or sync-trusted) steps.
#[derive(Debug, Clone)]
pub struct VdfFastForwardSender {
    sender: Sender<Traced<VdfStep>>,
    signals: ReanchorSignals,
}

impl VdfFastForwardSender {
    /// Capture before validation; pass into [`Self::send_validated_batch`].
    pub fn current_generation(&self) -> u64 {
        self.signals.generation()
    }

    /// Replay a pre-validated step batch, stamped with `captured_generation`.
    /// Fails with [`VdfReanchorGenerationChanged`] if a heal applied since capture.
    pub async fn send_validated_batch(
        &self,
        start_step_number: u64,
        steps: &[H256],
        send_timeout: Duration,
        captured_generation: u64,
    ) -> eyre::Result<()> {
        let current = self.signals.generation();
        if current != captured_generation {
            return Err(VdfReanchorGenerationChanged {
                captured: captured_generation,
                current,
            }
            .into());
        }
        fast_forward_validated_steps(
            start_step_number,
            steps,
            &self.sender,
            captured_generation,
            send_timeout,
        )
        .await
    }
}

/// Bounded VDF fast-forward channel + shared [`ReanchorSignals`].
pub fn fast_forward_channel() -> (
    VdfFastForwardSender,
    Receiver<Traced<VdfStep>>,
    ReanchorSignals,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(VDF_FAST_FORWARD_CHANNEL_CAPACITY);
    let signals = ReanchorSignals::new();
    (
        VdfFastForwardSender {
            sender: tx,
            signals: signals.clone(),
        },
        rx,
        signals,
    )
}

/// Heal the seed buffer to canonical without moving the step counter.
#[derive(Debug, Clone)]
pub struct ReanchorRequest {
    /// Canonical seeds for `[c_step - len + 1, c_step]`, oldest first.
    pub canonical_seeds: VecDeque<Seed>,
    /// Canonical tip step; free-running tail is recomputed to `live_step`.
    pub c_step: u64,
    /// Tip `seed` (fold value when `c_step` is on a reset boundary).
    pub seed: H256,
    /// `next_seed` for the first boundary strictly after `c_step`.
    pub next_seed: H256,
}

/// First reset boundary where forks splitting at `lca_step` can fold different seeds.
/// `reset_frequency` must be non-zero.
#[must_use]
pub fn first_divergent_boundary(lca_step: u64, reset_frequency: u64) -> u64 {
    (lca_step / reset_frequency)
        .saturating_add(2)
        .saturating_mul(reset_frequency)
}

/// True if progress to `crossing_step` past `lca_step` may have poisoned the buffer.
#[must_use]
pub fn reorg_crossed_divergent_boundary(
    lca_step: u64,
    crossing_step: u64,
    reset_frequency: u64,
) -> bool {
    crossing_step >= first_divergent_boundary(lca_step, reset_frequency)
}

/// Latest-value re-anchor channel (coalesces to newest request).
pub type ReanchorReceiver = watch::Receiver<Option<ReanchorRequest>>;

#[derive(Debug, Clone)]
pub struct VdfReanchorSender(watch::Sender<Option<ReanchorRequest>>);

impl VdfReanchorSender {
    /// Non-blocking publish; replaces any unconsumed request. `false` if VDF is down.
    #[must_use]
    pub fn try_send(&self, request: ReanchorRequest) -> bool {
        self.0.send(Some(request)).is_ok()
    }
}

pub fn reanchor_channel() -> (VdfReanchorSender, ReanchorReceiver) {
    let (tx, rx) = watch::channel(None);
    (VdfReanchorSender(tx), rx)
}

/// Replay a contiguous validated prefix of VDF steps into local state.
#[tracing::instrument(level = "trace", skip_all, err)]
pub(crate) async fn fast_forward_validated_steps(
    start_step_number: u64,
    steps: &[H256],
    vdf_fast_forward_sender: &Sender<Traced<VdfStep>>,
    generation: u64,
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
                generation,
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
    use rstest::rstest;
    use std::time::Duration;

    /// Off-by-one pins for [`first_divergent_boundary`].
    #[rstest]
    #[case::lca_on_boundary(0, 5, 10)]
    #[case::lca_just_after_boundary(6, 5, 15)]
    #[case::lca_one_short_of_boundary(9, 5, 15)]
    #[case::lca_exactly_on_second_boundary(10, 5, 20)]
    #[case::reset_frequency_one(7, 1, 9)]
    fn first_divergent_boundary_is_first_unshared_rotation(
        #[case] lca_step: u64,
        #[case] reset_frequency: u64,
        #[case] expected: u64,
    ) {
        let boundary = first_divergent_boundary(lca_step, reset_frequency);
        assert_eq!(boundary, expected);

        // Its rotation block (boundary - reset_frequency) is strictly after the
        // fork point: the forks fold different seeds here.
        assert!(
            boundary - reset_frequency > lca_step,
            "the boundary's rotation block must be strictly after the LCA"
        );
        // The PREVIOUS boundary's rotation block (boundary - 2*reset_frequency) is
        // at or before the fork point, so this really is the FIRST divergent one.
        assert!(
            boundary - 2 * reset_frequency <= lca_step,
            "the previous boundary's rotation block must not be after the LCA"
        );
    }

    #[tokio::test]
    async fn factory_delivers_validated_steps() {
        let (sender, mut rx, signals) = fast_forward_channel();
        // Advance past generation 0 so the stamp assertion below cannot pass
        // vacuously on a default value.
        signals.record_heal_applied();
        signals.record_heal_applied();
        let captured = sender.current_generation();
        assert_eq!(captured, 2);

        let steps = [H256::from_low_u64_be(1), H256::from_low_u64_be(2)];
        sender
            .send_validated_batch(10, &steps, Duration::from_secs(1), captured)
            .await
            .expect("validated batch should send");

        let (first, _span) = rx.recv().await.expect("first step").into_parts();
        assert_eq!(first.global_step_number, 10);
        assert_eq!(first.step, steps[0]);
        assert_eq!(
            first.generation, captured,
            "steps carry the captured generation"
        );
        let (second, _span) = rx.recv().await.expect("second step").into_parts();
        assert_eq!(second.global_step_number, 11);
        assert_eq!(second.step, steps[1]);
        assert_eq!(
            second.generation, captured,
            "steps carry the captured generation"
        );
    }

    /// A heal applied between generation capture and send must abort the batch
    /// with the typed sentinel (routing to the requeue lane) and deliver
    /// nothing — steps validated against the pre-heal buffer never replay.
    #[tokio::test]
    async fn send_rejects_batch_when_generation_changed_since_capture() {
        let (sender, mut rx, signals) = fast_forward_channel();
        let captured = sender.current_generation();

        // A re-anchor lands mid-validation.
        signals.record_heal_applied();

        let err = sender
            .send_validated_batch(1, &[H256::zero()], Duration::from_secs(1), captured)
            .await
            .expect_err("stale captured generation must be rejected");
        let changed = err
            .downcast_ref::<VdfReanchorGenerationChanged>()
            .expect("typed sentinel must survive eyre boxing");
        assert_eq!(changed.captured, captured);
        assert_eq!(changed.current, captured + 1);
        assert!(
            rx.try_recv().is_err(),
            "no step may be delivered from a rejected batch"
        );
    }

    /// The gate marks the buffer suspect; only an applied heal clears it.
    #[test]
    fn suspect_flag_set_by_mark_cleared_by_heal() {
        let signals = ReanchorSignals::new();
        assert!(!signals.is_buffer_suspect());
        signals.mark_buffer_suspect();
        assert!(signals.is_buffer_suspect());
        signals.record_heal_applied();
        assert!(!signals.is_buffer_suspect());
        assert_eq!(signals.generation(), 1);
    }

    /// Latest-value re-anchor channel: a newer request replaces an unconsumed
    /// older one, so a burst of deep reorgs can never evict the freshest window.
    #[test]
    fn reanchor_channel_keeps_only_the_newest_request() {
        let (tx, mut rx) = reanchor_channel();
        let request_at = |c_step: u64| ReanchorRequest {
            canonical_seeds: VecDeque::from(vec![Seed(H256::from_low_u64_be(c_step))]),
            c_step,
            seed: H256::zero(),
            next_seed: H256::zero(),
        };
        assert!(tx.try_send(request_at(10)));
        assert!(tx.try_send(request_at(20)));

        let latest = rx
            .borrow_and_update()
            .clone()
            .expect("a request must be present");
        assert_eq!(latest.c_step, 20, "the newest request wins");
        assert!(
            !rx.has_changed().expect("sender alive"),
            "borrow_and_update consumed the pending marker"
        );

        drop(rx);
        assert!(
            !tx.try_send(request_at(30)),
            "send must report failure once the receiver is gone"
        );
    }

    // Fail-stop: a dead receiver (run_vdf exited) must panic, never return an Err
    // that could let validation mislabel a block. Verified through the factory +
    // newtype.
    #[tokio::test]
    #[should_panic(expected = "channel closed")]
    async fn send_panics_when_receiver_dropped() {
        let (sender, rx, _signals) = fast_forward_channel();
        drop(rx);
        let captured = sender.current_generation();
        sender
            .send_validated_batch(1, &[H256::zero()], Duration::from_millis(50), captured)
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
        let (sender, _rx, _signals) = fast_forward_channel();
        let steps = vec![H256::zero(); VDF_FAST_FORWARD_CHANNEL_CAPACITY + 1];
        let captured = sender.current_generation();
        sender
            .send_validated_batch(1, &steps, Duration::from_millis(50), captured)
            .await
            .expect("must panic before returning");
    }
}
