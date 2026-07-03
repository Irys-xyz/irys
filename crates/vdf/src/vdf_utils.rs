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

/// Shared re-anchor signals: the heal generation counter and the buffer-suspect
/// flag, one allocation pair shared by the fast-forward sender, `run_vdf`, the
/// block-tree re-anchor gate, and recall-range validation.
///
/// - **generation**: bumped by `run_vdf` on every APPLIED heal. Fast-forward
///   steps are stamped with the generation captured when their validation
///   began; `run_vdf` drops steps stamped older than current, so steps
///   validated against a pre-heal buffer can never replay onto the healed one.
/// - **suspect**: set by the block-tree gate when it queues a [`ReanchorRequest`]
///   (the buffer may hold minority-fork seeds); cleared by `run_vdf` when a heal
///   applies. While set, recall-range validation must not trust the buffer in
///   EITHER direction, and the gate re-sends a fresh request from every new
///   canonical tip (the retry source for a skipped heal).
///
/// Mutation is sealed: only `run_vdf` (in-crate) can bump the generation or
/// clear the suspect flag. Marking suspect is public — the only external writer
/// is the gate, and a spurious mark merely costs a slower validation path.
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

    /// Current re-anchor generation. `0` until the first applied heal.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Applied-heal transition: invalidate steps stamped under older
    /// generations and mark the buffer trustworthy again. `run_vdf` only.
    pub(crate) fn record_heal_applied(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.buffer_suspect.store(false, Ordering::Relaxed);
    }

    /// Mark the seed buffer suspect (a re-anchor request is pending). Called by
    /// the block-tree gate BEFORE it queues the request, so validation observes
    /// the flag no later than the heal.
    pub fn mark_buffer_suspect(&self) {
        self.buffer_suspect.store(true, Ordering::Relaxed);
    }

    /// Whether a queued re-anchor has not yet been applied. While true, the
    /// seed buffer must not be treated as authoritative.
    pub fn is_buffer_suspect(&self) -> bool {
        self.buffer_suspect.load(Ordering::Relaxed)
    }
}

/// Sentinel error from [`VdfFastForwardSender::send_validated_batch`]: the
/// re-anchor generation changed between validation start and send, so the batch
/// was validated against a since-healed buffer and must not be replayed.
/// Downcast by the validation service and routed to the requeue lane
/// (`VdfValidationResult::Cancelled`) — the block revalidates against the
/// healed buffer; this is never block-invalidity evidence.
#[derive(Debug, thiserror::Error)]
#[error(
    "VDF re-anchor generation changed during validation (captured {captured}, current {current}); batch dropped for revalidation"
)]
pub struct VdfReanchorGenerationChanged {
    pub captured: u64,
    pub current: u64,
}

/// Sending half of the VDF fast-forward channel.
///
/// Nominal newtype over `Sender<Traced<VdfStep>>` that exposes only
/// [`Self::send_validated_batch`]. Steps pushed through it must already be
/// validated, or explicitly trusted by the sync path (`skip_vdf_validation`); the
/// type makes the raw `Sender` (which carries no such invariant) unreachable
/// outside this crate.
#[derive(Debug, Clone)]
pub struct VdfFastForwardSender {
    sender: Sender<Traced<VdfStep>>,
    signals: ReanchorSignals,
}

impl VdfFastForwardSender {
    /// Re-anchor generation to capture BEFORE validating the steps a batch will
    /// carry; pass it back to [`Self::send_validated_batch`].
    pub fn current_generation(&self) -> u64 {
        self.signals.generation()
    }

    /// Replay a contiguous batch of VDF steps into local state. The steps must
    /// already be validated, or explicitly trusted by the sync path. Delegates to
    /// [`fast_forward_validated_steps`] (same tracing span and fail-stop
    /// semantics), stamping each step with `captured_generation` — the value the
    /// caller read via [`Self::current_generation`] BEFORE validation began.
    ///
    /// Fails with [`VdfReanchorGenerationChanged`] if a heal applied since the
    /// capture: the steps were validated against the pre-heal buffer and must be
    /// revalidated, not replayed. Even if a heal lands between this check and
    /// the send, the steps carry the stale stamp and `run_vdf` drops them.
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

/// Create the bounded VDF fast-forward channel. Returns the sender, the receiver
/// (consumed by `run_vdf`), and the shared [`ReanchorSignals`]: senders stamp
/// the generation onto each step and `run_vdf` bumps it on every applied
/// re-anchor, so steps in flight across a heal are dropped rather than replayed.
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

/// A request to heal the VDF seed buffer to canonical after a deep reorg,
/// WITHOUT moving the step counter. Built by the block-tree gate (which holds the
/// canonical chain) and applied by `run_vdf` under its write guard.
#[derive(Debug, Clone)]
pub struct ReanchorRequest {
    /// Canonical seed values for `[c_step - canonical_seeds.len() + 1, c_step]`,
    /// oldest first. These overwrite the poisoned buffer over that range.
    pub canonical_seeds: VecDeque<Seed>,
    /// The highest step for which a canonical seed is supplied (the canonical
    /// tip). `run_vdf` recomputes the free-running tail `(c_step, live_step]`
    /// locally from `canonical_seeds`' last value.
    pub c_step: u64,
    /// The canonical tip's `seed` — the reset seed folded at `c_step` itself
    /// when `c_step` sits exactly on a reset boundary (the first post-tip block
    /// starts at `c_step + 1`, contains no boundary step of its own, and so
    /// carries the tip's `seed` forward as its fold value per
    /// `calculate_seeds`). Ignored when `c_step` is off-boundary.
    pub seed: H256,
    /// The canonical `next_seed` to fold at the FIRST reset boundary strictly
    /// after `c_step` (its rotation block is at or before the canonical tip).
    /// Later boundaries are unpinned — `apply_reanchor` refuses to fold them.
    pub next_seed: H256,
}

/// Receiving half of the VDF re-anchor channel, consumed by `run_vdf`.
///
/// A `watch` receiver: the channel holds only the LATEST request, so a burst of
/// deep reorgs coalesces to the newest window by construction — no bounded
/// queue that could drop the freshest request on overflow.
pub type ReanchorReceiver = watch::Receiver<Option<ReanchorRequest>>;

/// Sending half of the VDF re-anchor channel. Owned by `irys-vdf`; the block-tree
/// gate holds the sender, `run_vdf` consumes the [`ReanchorReceiver`].
#[derive(Debug, Clone)]
pub struct VdfReanchorSender(watch::Sender<Option<ReanchorRequest>>);

impl VdfReanchorSender {
    /// Non-blocking publish from the synchronous block-tree reorg handler.
    /// Latest-value semantics: a newer request REPLACES any unconsumed older
    /// one (each later request carries a fresher canonical window). Returns
    /// `false` only when the receiver is gone (VDF thread not running).
    #[must_use]
    pub fn try_send(&self, request: ReanchorRequest) -> bool {
        self.0.send(Some(request)).is_ok()
    }
}

/// Create the VDF re-anchor channel. The gate holds the [`VdfReanchorSender`];
/// `run_vdf` consumes the [`ReanchorReceiver`].
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
    use std::time::Duration;

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
