use crate::VdfStep;
use irys_types::H256;
use irys_types::Traced;
use irys_types::VDFLimiterInfo;
use irys_types::block_production::Seed;
use irys_types::block_provider::CanonicalVdfSnapshot;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::watch;
use tokio::time::{Duration, timeout};

/// Bound for the inbound VDF fast-forward channel. Owned by `irys-vdf` so the
/// VDF crate, not the actor hub, defines the contract. The loop drains before
/// local steps and between recomputed steps so rewrite time cannot block sends.
pub const VDF_FAST_FORWARD_CHANNEL_CAPACITY: usize = 4_096;

/// Shared re-anchor generation + buffer-suspect flag (FF senders, `run_vdf`,
/// block-tree gate, recall-range validation). Generation bumps on applied heal;
/// suspect is set while a heal is pending/running. `recompute_in_progress` is set
/// while `run_vdf` recomputes its tail after a reset seed changed. Also records
/// whether `run_vdf` stopped because the node is shutting down.
#[derive(Debug, Clone)]
pub struct ReanchorSignals {
    generation: Arc<AtomicU64>,
    buffer_suspect: Arc<AtomicBool>,
    stopped_for_shutdown: Arc<AtomicBool>,
    recompute_in_progress: Arc<AtomicBool>,
}

impl ReanchorSignals {
    pub(crate) fn new() -> Self {
        Self {
            generation: Arc::new(AtomicU64::new(0)),
            buffer_suspect: Arc::new(AtomicBool::new(false)),
            stopped_for_shutdown: Arc::new(AtomicBool::new(false)),
            recompute_in_progress: Arc::new(AtomicBool::new(false)),
        }
    }

    /// `run_vdf` only, just before it exits on the shutdown token and drops the
    /// fast-forward receiver. Release.
    pub(crate) fn record_shutdown_stop(&self) {
        self.stopped_for_shutdown.store(true, Ordering::Release);
    }

    /// True once `run_vdf` has exited on the shutdown token. A poisoned-lock exit
    /// never sets it, so a closed channel without it still means a dead VDF thread.
    pub fn is_stopped_for_shutdown(&self) -> bool {
        self.stopped_for_shutdown.load(Ordering::Acquire)
    }

    /// `run_vdf` starts recomputing its tail; the step counter stops until
    /// [`Self::finish_recompute`].
    pub(crate) fn begin_recompute(&self) {
        self.recompute_in_progress.store(true, Ordering::Release);
    }

    /// `run_vdf` finished (or abandoned) a tail recompute. When it rewrote buffered
    /// steps, bump the generation as a heal does, but leave the suspect flag alone:
    /// it belongs to a pending block-tree re-anchor.
    pub(crate) fn finish_recompute(&self, rewrote_steps: bool) {
        if rewrote_steps {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
        self.recompute_in_progress.store(false, Ordering::Release);
    }

    /// True while `run_vdf` recomputes its tail: the step counter is frozen on
    /// purpose, so a waiter must not read that as a dead VDF thread.
    pub fn is_recompute_in_progress(&self) -> bool {
        self.recompute_in_progress.load(Ordering::Acquire)
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

/// `run_vdf` stopped for shutdown before this batch was fast-forwarded; the batch
/// must requeue (not invalid, and not a node fault).
#[derive(Debug, thiserror::Error)]
#[error("VDF thread stopped for shutdown before step {step} was fast-forwarded")]
pub struct VdfFastForwardStopped {
    pub step: u64,
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
    /// `predecessor` is the block's own step before `start_step_number` (its
    /// `prev_output` for the block's first step). `reset_seed` is the validated
    /// block's `vdf_limiter_info.seed`: the seed it folds at every reset boundary
    /// in its range.
    /// Fails with [`VdfReanchorGenerationChanged`] if a heal applied since capture.
    pub async fn send_validated_batch(
        &self,
        start_step_number: u64,
        predecessor: H256,
        steps: &[H256],
        reset_seed: H256,
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
            predecessor,
            steps,
            reset_seed,
            &self.sender,
            &self.signals,
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
    /// Canonical reset seed of every boundary `b` with `b + 1` in the window, from the
    /// covering block headers ([`reset_seeds_folded_by_block`]).
    pub reset_seed_by_boundary: BTreeMap<u64, H256>,
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

/// The reset boundaries that validation of a block with `info` folds, each paired with the seed
/// it folds there (`info.seed`): every multiple `b > 0` of `reset_frequency` with `b + 1` in the
/// block's step range. Mirrors `vdf_step_batch_is_valid`. `reset_frequency` must be non-zero.
pub fn reset_seeds_folded_by_block(
    info: &VDFLimiterInfo,
    reset_frequency: u64,
) -> impl Iterator<Item = (u64, H256)> {
    let lowest = info.first_step_number().saturating_sub(1).max(1);
    let highest = info.global_step_number.saturating_sub(1);
    let first_boundary = lowest
        .div_ceil(reset_frequency)
        .saturating_mul(reset_frequency);
    let seed = info.seed;
    (first_boundary..=highest)
        .step_by(usize::try_from(reset_frequency).unwrap_or(usize::MAX))
        .map(move |boundary| (boundary, seed))
}

/// The seed a child of the canonical tip folds at `boundary` (`boundary >= tip_last_step`) when
/// the child's step range runs from `tip_last_step + 1` to `max(live_step, boundary + 1)`.
///
/// Mirrors `VDFLimiterInfo::calculate_seeds`: a child whose range holds a reset step carries the
/// tip's `next_seed`, otherwise the tip's `seed`, and validation folds that one seed at every
/// boundary in its range. For `boundary > tip_last_step` the range holds `boundary` itself, so
/// the answer is always `tip_next_seed`. For `boundary == tip_last_step` the answer depends on
/// how far the child reaches: `tip_seed` until `live_step` gets to the next boundary, then
/// `tip_next_seed`. `reset_frequency` must be non-zero.
#[must_use]
pub fn tip_child_reset_seed(
    tip_last_step: u64,
    tip_seed: H256,
    tip_next_seed: H256,
    boundary: u64,
    live_step: u64,
    reset_frequency: u64,
) -> H256 {
    let child_last_step = live_step.max(boundary.saturating_add(1));
    let first_reset_after_tip = (tip_last_step / reset_frequency)
        .saturating_add(1)
        .saturating_mul(reset_frequency);
    if first_reset_after_tip <= child_last_step {
        tip_next_seed
    } else {
        tip_seed
    }
}

/// The reset seed the canonical chain in `snapshot` folds at `boundary`, for a loop whose live
/// step is `live_step`. A canonical block that covers `boundary + 1` decides it with its own
/// `seed`; above the tip, the child-of-tip rule ([`tip_child_reset_seed`]) decides it. `None`
/// when `boundary + 1` is in a confirmed block (not in the snapshot). `reset_frequency` must be
/// non-zero.
#[must_use]
pub fn canonical_reset_seed(
    snapshot: &CanonicalVdfSnapshot,
    boundary: u64,
    live_step: u64,
    reset_frequency: u64,
) -> Option<H256> {
    let tip = &snapshot.vdf_info;
    if boundary >= tip.global_step_number {
        return Some(tip_child_reset_seed(
            tip.global_step_number,
            tip.seed,
            tip.next_seed,
            boundary,
            live_step,
            reset_frequency,
        ));
    }
    let next_step = boundary + 1;
    snapshot
        .unconfirmed_blocks
        .iter()
        .find(|block| block.first_step <= next_step && next_step <= block.last_step)
        .map(|block| block.seed)
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
    predecessor: H256,
    steps: &[H256],
    reset_seed: H256,
    vdf_fast_forward_sender: &Sender<Traced<VdfStep>>,
    signals: &ReanchorSignals,
    generation: u64,
    send_timeout: Duration,
) -> eyre::Result<()> {
    let end_step_number = start_step_number + steps.len().saturating_sub(1) as u64;
    tracing::trace!(
        "VDF FF: validated batch step range: {}-{}",
        start_step_number,
        end_step_number
    );
    let predecessors = std::iter::once(predecessor).chain(steps.iter().copied());
    for (i, (hash, predecessor)) in steps.iter().zip(predecessors).enumerate() {
        match timeout(
            send_timeout,
            vdf_fast_forward_sender.send(Traced::new(VdfStep {
                step: *hash,
                predecessor,
                global_step_number: start_step_number + i as u64,
                reset_seed,
                generation,
            })),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                // `run_vdf` exits on the shutdown token before the lifecycle stops
                // block intake, so validation can still be draining when the
                // receiver goes. That exit is requested, not a fault: requeue the
                // batch instead of panicking an orderly shutdown.
                if signals.is_stopped_for_shutdown() {
                    return Err(VdfFastForwardStopped {
                        step: start_step_number + i as u64,
                    }
                    .into());
                }
                // Otherwise the channel closed because `run_vdf` died
                // (e.g., graceful exit on a poisoned VDF state lock; see
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
                // The loop drains before local steps and between recomputed steps,
                // dropping already-buffered steps and deferring future steps during
                // rewrites. A full channel therefore waits for at most one SHA step
                // on a healthy consumer, including during a long tail rewrite.
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

    fn limiter(first_step: u64, last_step: u64, seed: H256) -> VDFLimiterInfo {
        VDFLimiterInfo {
            global_step_number: last_step,
            steps: irys_types::H256List(vec![H256::zero(); (last_step - first_step + 1) as usize]),
            seed,
            ..VDFLimiterInfo::default()
        }
    }

    /// Validation folds `seed` at every boundary `b > 0` with `b + 1` in the block's range.
    #[rstest]
    #[case::no_boundary(7, 9, vec![])]
    #[case::boundary_inside(4, 7, vec![5])]
    // The step before the first one is a boundary: computing step 6 folds boundary 5.
    #[case::boundary_just_before_first_step(6, 7, vec![5])]
    #[case::boundary_on_last_step_not_folded(8, 10, vec![])]
    #[case::two_boundaries(4, 16, vec![5, 10, 15])]
    #[case::step_zero_never_folded(1, 3, vec![])]
    fn reset_seeds_folded_by_block_matches_validation(
        #[case] first_step: u64,
        #[case] last_step: u64,
        #[case] expected: Vec<u64>,
    ) {
        let seed = H256::repeat_byte(0x5E);
        let folded: Vec<(u64, H256)> =
            reset_seeds_folded_by_block(&limiter(first_step, last_step, seed), 5).collect();
        let expected: Vec<(u64, H256)> = expected.into_iter().map(|b| (b, seed)).collect();
        assert_eq!(folded, expected);
    }

    /// A child of the tip carries `next_seed` when its range (tip + 1 ..= live) holds a
    /// reset step, else the tip's `seed`.
    #[rstest]
    // Boundary above the tip: the child's range holds it.
    #[case::boundary_above_tip(7, 10, 10, true)]
    #[case::second_boundary_above_tip(7, 15, 15, true)]
    // Tip ends on boundary 10: the child folds 10 with the tip's seed until it reaches 15.
    #[case::tip_on_boundary_short_child(10, 10, 14, false)]
    #[case::tip_on_boundary_child_reaches_next(10, 10, 15, true)]
    fn tip_child_reset_seed_follows_calculate_seeds(
        #[case] tip_last_step: u64,
        #[case] boundary: u64,
        #[case] live_step: u64,
        #[case] expect_next_seed: bool,
    ) {
        let seed = H256::repeat_byte(0x01);
        let next_seed = H256::repeat_byte(0x02);
        let got = tip_child_reset_seed(tip_last_step, seed, next_seed, boundary, live_step, 5);
        assert_eq!(got, if expect_next_seed { next_seed } else { seed });
    }

    /// Below the tip the covering unconfirmed block decides; below those, nothing does.
    #[test]
    fn canonical_reset_seed_reads_covering_block_then_tip_child() {
        let block_a = irys_types::block_provider::CanonicalBlockVdf {
            first_step: 3,
            last_step: 6,
            seed: H256::repeat_byte(0x0A),
            steps: None,
        };
        let block_b = irys_types::block_provider::CanonicalBlockVdf {
            first_step: 7,
            last_step: 12,
            seed: H256::repeat_byte(0x0B),
            steps: None,
        };
        let snapshot = CanonicalVdfSnapshot {
            vdf_info: VDFLimiterInfo {
                global_step_number: 12,
                seed: block_b.seed,
                next_seed: H256::repeat_byte(0x0C),
                ..VDFLimiterInfo::default()
            },
            confirmed_global_step_number: 2,
            confirmed_output: H256::zero(),
            reset_seed_for_step: H256::zero(),
            unconfirmed_blocks: vec![block_a.clone(), block_b.clone()],
        };
        assert_eq!(
            canonical_reset_seed(&snapshot, 5, 20, 5),
            Some(block_a.seed)
        );
        assert_eq!(
            canonical_reset_seed(&snapshot, 10, 20, 5),
            Some(block_b.seed)
        );
        assert_eq!(
            canonical_reset_seed(&snapshot, 15, 20, 5),
            Some(H256::repeat_byte(0x0C))
        );
        assert_eq!(canonical_reset_seed(&snapshot, 0, 20, 5), None);
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
            .send_validated_batch(
                10,
                H256::zero(),
                &steps,
                H256::zero(),
                Duration::from_secs(1),
                captured,
            )
            .await
            .expect("validated batch should send");

        let (first, _span) = rx.recv().await.expect("first step").into_parts();
        assert_eq!(first.global_step_number, 10);
        assert_eq!(first.step, steps[0]);
        assert_eq!(first.predecessor, H256::zero(), "the batch predecessor");
        assert_eq!(
            first.generation, captured,
            "steps carry the captured generation"
        );
        let (second, _span) = rx.recv().await.expect("second step").into_parts();
        assert_eq!(second.global_step_number, 11);
        assert_eq!(second.step, steps[1]);
        assert_eq!(
            second.predecessor, steps[0],
            "the previous step in the batch"
        );
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
            .send_validated_batch(
                1,
                H256::zero(),
                &[H256::zero()],
                H256::zero(),
                Duration::from_secs(1),
                captured,
            )
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
            reset_seed_by_boundary: Default::default(),
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

    // A receiver dropped by a shutdown stop is expected while validation drains:
    // it must surface as the typed requeue error, not a panic.
    #[tokio::test]
    async fn send_reports_shutdown_stop_when_receiver_dropped() {
        let (sender, rx, signals) = fast_forward_channel();
        signals.record_shutdown_stop();
        drop(rx);
        let captured = sender.current_generation();
        let error = sender
            .send_validated_batch(
                7,
                H256::zero(),
                &[H256::zero()],
                H256::zero(),
                Duration::from_millis(50),
                captured,
            )
            .await
            .expect_err("a shutdown stop must fail the send");
        let stopped = error
            .downcast_ref::<VdfFastForwardStopped>()
            .expect("shutdown stop must be the typed requeue error");
        assert_eq!(stopped.step, 7);
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
            .send_validated_batch(
                1,
                H256::zero(),
                &[H256::zero()],
                H256::zero(),
                Duration::from_millis(50),
                captured,
            )
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
            .send_validated_batch(
                1,
                H256::zero(),
                &steps,
                H256::zero(),
                Duration::from_millis(50),
                captured,
            )
            .await
            .expect("must panic before returning");
    }
}
