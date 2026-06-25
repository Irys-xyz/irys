use crate::metrics;
use crate::state::{
    AtomicVdfState, ReanchorLockPoisoned, create_state_for_canonical_tip, publish_reanchored_state,
};
use crate::{MiningBroadcaster, VdfStep, apply_reset_seed, step_number_to_salt_number, vdf_sha};
use irys_domain::chain_sync_state::ChainSyncState;
use irys_types::block_provider::{BlockProvider, CanonicalVdfSnapshot};
use irys_types::{
    AtomicVdfStepNumber, Config, DatabaseProvider, H256, H256List, IrysBlockHeader, Traced, U256,
    block_production::Seed,
};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{Receiver, UnboundedReceiver};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

const PAUSED_VDF_LOOP_SLEEP: Duration = Duration::from_millis(200);
const PAUSED_SYNC_FAST_FORWARD_SLEEP: Duration = Duration::from_millis(10);
/// Minimum gap between repeated reset-boundary-gate warnings. While parked the loop wakes
/// every `PAUSED_VDF_LOOP_SLEEP` (200ms), so the warning is rate-limited to avoid ~5/sec
/// log spam during an expected multi-block park; the first occurrence still logs immediately.
const BOUNDARY_GATE_WARN_INTERVAL: Duration = Duration::from_secs(30);

pub fn run_vdf_for_genesis_block(
    genesis_block: &mut IrysBlockHeader,
    config: &irys_types::VdfConfig,
) {
    let reset_seed = genesis_block.vdf_limiter_info.seed;
    let reset_frequency = config.reset_frequency as u64;

    let last_epoch_block_hash = genesis_block.last_epoch_hash;
    genesis_block.vdf_limiter_info.prev_output = last_epoch_block_hash;

    let mut hash: H256 = genesis_block.vdf_limiter_info.seed;
    let mut checkpoints: Vec<H256> = vec![H256::default(); config.num_checkpoints_in_vdf_step];

    for global_step_number in 0..=1 {
        let salt = U256::from(step_number_to_salt_number(config, global_step_number));

        vdf_sha(
            salt,
            &mut hash,
            config.num_checkpoints_in_vdf_step,
            config.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );

        if global_step_number == 0 {
            genesis_block.vdf_limiter_info.prev_output = hash;
        } else {
            genesis_block.vdf_limiter_info.global_step_number = 1;
            genesis_block.vdf_limiter_info.output = hash;
            genesis_block.vdf_limiter_info.last_step_checkpoints.0 = checkpoints.clone(); // clone: checkpoints reused across loop iterations
            genesis_block.vdf_limiter_info.steps.0 = vec![hash];
        }

        hash = process_reset(global_step_number, hash, reset_frequency, reset_seed);
    }
}

/// Why [`run_vdf`] returned, so the supervising VDF thread knows whether to exit
/// or re-anchor and restart the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VdfExit {
    /// Terminal: the shutdown token fired or the VDF state lock was poisoned.
    /// The supervisor must stop (and the node should shut down).
    Shutdown,
    /// A re-anchor was requested (a deep reorg / network-partition recovery
    /// rewound the canonical chain). The supervisor rebuilds the VDF state from
    /// the now-canonical block index and restarts the loop. See
    /// design/docs/vdf-partition-recovery-reanchor.md.
    Reanchor,
}

pub fn run_vdf<B: BlockProvider>(
    config: &irys_types::VdfConfig,
    global_step_number: u64,
    current_vdf_hash: H256,
    initial_reset_seed: H256,
    fast_forward_receiver: &mut Receiver<Traced<VdfStep>>,
    reanchor_receiver: &mut UnboundedReceiver<()>,
    is_mining_enabled: Arc<AtomicBool>,
    broadcast_mining_service: impl MiningBroadcaster,
    vdf_state: AtomicVdfState,
    atomic_vdf_global_step: AtomicVdfStepNumber,
    block_provider: B,
    chain_sync_state: ChainSyncState,
    shutdown_token: CancellationToken,
) -> VdfExit {
    let _span = tracing::info_span!("vdf_loop").entered();
    let mut next_reset_seed = initial_reset_seed;
    // Step number of the deepest block confirmed past the migration threshold
    // (`block_migration_depth`), as reported by the canonical snapshot. The reset-boundary
    // gate compares the upcoming boundary against THIS step, not the canonical tip's, so
    // the loop never applies a reset seed pinned by a still-forkable block — neither a
    // 0-confirmation fork-loser seed (issue #1447) nor a seed re-pinned by a reorg.
    // Because it tracks the confirmed chain (which only advances as blocks migrate), it
    // is also reorg- and startup-safe with no per-seed bookkeeping.
    let mut canonical_global_step_number = match vdf_state.read() {
        Ok(guard) => guard.canonical_step(),
        Err(_) => {
            // A prior panic in another caller poisoned the lock. Bail rather
            // than re-panic; the lifecycle's vdf_done channel surfaces this
            // as a controlled exit.
            error!("VDF state read lock poisoned at startup; exiting VDF thread");
            return VdfExit::Shutdown;
        }
    };
    // Until the first canonical snapshot read, treat the confirmed step as the canonical
    // step (they coincide at startup); the snapshot overwrites it on the first iteration.
    let mut confirmed_global_step_number = canonical_global_step_number;

    let mut hash: H256 = current_vdf_hash;
    let mut checkpoints: Vec<H256> = vec![H256::default(); config.num_checkpoints_in_vdf_step];
    let mut global_step_number = global_step_number;
    // FIXME: The reset seed is the same as the seed... which I suspect is incorrect!
    info!(
        "VDF thread started at global_step_number: {}",
        global_step_number
    );
    let vdf_reset_frequency = config.reset_frequency as u64;
    // Timestamp of the last reset-boundary-gate warning, for rate-limiting (see
    // `BOUNDARY_GATE_WARN_INTERVAL`). Reset when the loop pauses with mining disabled, so a
    // gate appearing after mining resumes warns immediately.
    let mut last_boundary_gate_warning: Option<Instant> = None;

    loop {
        if shutdown_token.is_cancelled() {
            tracing::info!("VDF loop: shutdown token cancelled, exiting");
            return VdfExit::Shutdown;
        }

        // A deep reorg (network-partition recovery) rewound the canonical chain, so the
        // running hash/global_step and the shared step buffer may carry reset entropy
        // pinned by the now-orphaned minority fork. Drain every pending request (coalesce
        // bursts) and hand control back to the supervisor, which rebuilds the VDF state
        // from the canonical index and restarts this loop at the new anchor. See
        // design/docs/vdf-partition-recovery-reanchor.md.
        let mut reanchor_requested = false;
        while reanchor_receiver.try_recv().is_ok() {
            reanchor_requested = true;
        }
        if reanchor_requested {
            info!(
                vdf.global_step_number = global_step_number,
                "VDF loop: re-anchor requested, returning to supervisor"
            );
            return VdfExit::Reanchor;
        }

        // check for VDF fast forward step
        while let Ok(traced_ff_step) = fast_forward_receiver.try_recv() {
            let (proposed_ff_step, _entered) = traced_ff_step.into_inner();
            // if the step number is ahead of local nodes vdf steps
            if global_step_number < proposed_ff_step.global_step_number {
                debug!(
                    "Fastforward Step {:?} with Seed {:?}",
                    proposed_ff_step.global_step_number, proposed_ff_step.step
                );

                // The confirmed-step reset-boundary gate (see `is_reset_boundary_blocked`)
                // intentionally does NOT apply on the fast-forward path, so the snapshot's
                // `confirmed_global_step_number` is discarded here. FF replays the steps of a
                // block already under validation into the shared step buffer verbatim; it never
                // runs the loop ahead, so it cannot reproduce the #1447 run-ahead bug (the local
                // loop applying a reset seed pinned by a still-forkable block).
                //
                // The step buffer is a single append-only sequence reflecting THIS node's lineage,
                // so a competing fork whose post-boundary steps differ mismatches it. That is NOT
                // treated as invalidity: `vdf_step_batch_is_valid` RECOMPUTES from the block's own
                // seed on a buffer mismatch (it does not conflate "differs from my lineage" with
                // "invalid"), so a heavier competing fork validates on its own merits. This is
                // load-bearing for partition recovery: a reorg in the band
                // `block_migration_depth < depth <= block_tree_depth` IS admitted and validated
                // against a possibly-poisoned buffer — admission keys on `block_tree_depth`
                // (`PartOfAPrunedFork` only refuses forks older than that), NOT
                // `block_migration_depth` — so an earlier comment here claiming such forks are
                // refused before validation was wrong. Recompute-on-mismatch is what lets the
                // recovering node validate and adopt the canonical chain (and then re-anchor).
                // See design/docs/vdf-reset-seed-confirmation-gate.md and
                // design/docs/vdf-partition-recovery-reanchor.md.
                if let Some(CanonicalVdfSnapshot {
                    vdf_info,
                    confirmed_global_step_number: _,
                    reset_seed_for_step,
                }) = block_provider
                    .canonical_vdf_snapshot(proposed_ff_step.global_step_number.saturating_sub(1))
                {
                    next_reset_seed = reset_seed_for_step;
                    canonical_global_step_number = vdf_info.global_step_number;
                }

                let prev_step = global_step_number;
                let Some(returned) = store_step(
                    proposed_ff_step.step,
                    &atomic_vdf_global_step,
                    &vdf_state,
                    proposed_ff_step.global_step_number,
                    canonical_global_step_number,
                ) else {
                    error!(
                        vdf.proposed_step = proposed_ff_step.global_step_number,
                        "VDF thread exiting: store_step failed during fast-forward (lock poisoned)"
                    );
                    return VdfExit::Shutdown;
                };
                if returned == prev_step {
                    // Gap rejected — leave hash and global_step_number
                    // untouched so the VDF state stays consistent. Normal
                    // stepping will close the gap.
                    warn!(
                        prev = prev_step,
                        proposed = proposed_ff_step.global_step_number,
                        "Fast-forward step had a gap; VDF will catch up via normal stepping"
                    );
                    continue;
                }
                global_step_number = returned;
                hash = proposed_ff_step.step;
                chain_sync_state.record_vdf_step(global_step_number);
                metrics::record_vdf_global_step(global_step_number);
                hash = process_reset(
                    global_step_number,
                    hash,
                    vdf_reset_frequency,
                    next_reset_seed,
                );
            } else {
                debug!(
                    "Fastforward Step {} is not ahead of {}",
                    proposed_ff_step.global_step_number, global_step_number
                );
            }
        }

        if let Some(CanonicalVdfSnapshot {
            vdf_info,
            confirmed_global_step_number: confirmed_step,
            reset_seed_for_step,
        }) = block_provider.canonical_vdf_snapshot(global_step_number)
        {
            canonical_global_step_number = vdf_info.global_step_number;
            confirmed_global_step_number = confirmed_step;
            next_reset_seed = reset_seed_for_step;
            debug!(
                "Canonical global step number: {}, next reset seed: {:?}, prev output: {:?}, confirmed step: {}, global_step: {:?}",
                canonical_global_step_number,
                next_reset_seed,
                vdf_info.prev_output,
                confirmed_global_step_number,
                global_step_number
            );
        }

        // Reset-boundary gate (see `is_reset_boundary_blocked`): do not cross the upcoming
        // boundary until the CONFIRMED chain (block_migration_depth deep) has reached the
        // rotation point, so the seed applied at the boundary was pinned by a block that can
        // no longer be reorged. Gating on the confirmed step is what prevents the run-ahead
        // seed poisoning of issue #1447.
        //
        // Fail closed: always gate on the confirmed step, never the (still-forkable) canonical
        // tip. The cost is run-ahead budget equal to the confirmation lag (canonical_step -
        // confirmed_step ≈ block_migration_depth blocks), which is negligible against a
        // production reset window — so honest mining keeps its head room while the full
        // confirmation guarantee holds unconditionally.
        let is_too_far_ahead = is_reset_boundary_blocked(
            global_step_number + 1,
            vdf_reset_frequency,
            confirmed_global_step_number,
        );

        // if mining disabled, wait 200ms and continue loop i.e. check again
        if !is_mining_enabled.load(std::sync::atomic::Ordering::Relaxed) || is_too_far_ahead {
            if is_too_far_ahead {
                // Rate-limit: an expected multi-block park would otherwise log every 200ms.
                let now = Instant::now();
                let throttled = last_boundary_gate_warning
                    .is_some_and(|last| now.duration_since(last) < BOUNDARY_GATE_WARN_INTERVAL);
                if throttled {
                    debug!(
                        "VDF still gated at reset boundary: next step {} (canonical {}, confirmed {})",
                        global_step_number + 1,
                        canonical_global_step_number,
                        confirmed_global_step_number
                    );
                } else {
                    warn!(
                        "VDF gated at reset boundary: next step {} (canonical {}, confirmed {}); waiting",
                        global_step_number + 1,
                        canonical_global_step_number,
                        confirmed_global_step_number
                    );
                    last_boundary_gate_warning = Some(now);
                }
            } else {
                last_boundary_gate_warning = None;
            }
            // During sync we pause local VDF mining, but trusted-peer catch-up
            // still depends on this loop consuming fast-forward steps promptly.
            // A 200ms sleep here limits that path to ~5 wakeups/sec.
            let pause_duration = if !is_too_far_ahead
                && !is_mining_enabled.load(std::sync::atomic::Ordering::Relaxed)
                && chain_sync_state.is_syncing()
            {
                PAUSED_SYNC_FAST_FORWARD_SLEEP
            } else {
                PAUSED_VDF_LOOP_SLEEP
            };
            debug!("VDF mining paused, waiting {:?}", pause_duration);
            std::thread::sleep(pause_duration);
            continue;
        }

        let now = Instant::now();

        let salt = U256::from(step_number_to_salt_number(config, global_step_number));

        vdf_sha(
            salt,
            &mut hash,
            config.num_checkpoints_in_vdf_step,
            config.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );

        let elapsed = now.elapsed();
        debug!("Vdf step duration: {:.2?}", elapsed);

        // Enforce a minimum step duration to prevent VDF from outrunning block
        // production when sha_1s_difficulty is low for tests.
        // With production difficulty (13M+), steps always exceed this floor.
        if config.throttle {
            const MIN_STEP_DURATION: Duration = Duration::from_millis(25);
            if elapsed < MIN_STEP_DURATION {
                std::thread::sleep(MIN_STEP_DURATION.checked_sub(elapsed).unwrap());
            }
        }

        let Some(returned) = store_step(
            hash,
            &atomic_vdf_global_step,
            &vdf_state,
            global_step_number + 1,
            canonical_global_step_number,
        ) else {
            error!(
                vdf.global_step_number = global_step_number,
                "VDF thread exiting: store_step failed during local stepping (lock poisoned)"
            );
            return VdfExit::Shutdown;
        };
        global_step_number = returned;
        chain_sync_state.record_vdf_step(global_step_number);
        metrics::record_vdf_global_step(global_step_number);
        info!("Seed created {} step number {}", hash, global_step_number);

        broadcast_mining_service.broadcast(
            Seed(hash),
            H256List(checkpoints.clone()), // clone: checkpoints reused across loop iterations
            global_step_number,
        );

        hash = process_reset(
            global_step_number,
            hash,
            vdf_reset_frequency,
            next_reset_seed,
        );
    }
}

#[must_use]
pub fn process_reset(
    global_step_number: u64,
    hash: H256,
    reset_frequency: u64,
    reset_seed: H256,
) -> H256 {
    if global_step_number.is_multiple_of(reset_frequency) {
        info!(
            "Reset seed {:?} applied to step {}",
            reset_seed, global_step_number
        );
        apply_reset_seed(hash, reset_seed)
    } else {
        hash
    }
}

/// Fold the reset-boundary entropy into a VDF ANCHOR hash before it seeds [`run_vdf`].
///
/// The step buffer stores RAW step outputs — `process_reset` applies the reset fold to the carried
/// hash only AFTER a step is stored (the loop tail), and fast-forward stores values verbatim. So a
/// hash read back from the buffer via `get_last_step_and_seed` at (re-)anchor time is unfolded. When
/// the anchor `step` is itself a reset boundary, the first `vdf_sha` for `step + 1` must run on the
/// hash WITH boundary `step`'s reset folded in, or that first step diverges from the canonical
/// lineage. This is rare (only when the anchor lands exactly on a boundary) and non-safety
/// (validation recomputes from each block's own seed), but it mis-steps local mining until it heals.
///
/// `seed` is boundary `step`'s reset seed: the anchoring block's OWN `vdf_limiter_info.seed`. When a
/// block's step range contains a reset boundary, `IrysBlockHeader::set_seeds` pins that boundary's
/// entropy in `seed` (while `next_seed` targets the NEXT boundary above the block — what the loop
/// applies going forward). A no-op when `step` is not a boundary.
#[must_use]
pub fn reset_applied_anchor_hash(reset_frequency: u64, step: u64, hash: H256, seed: H256) -> H256 {
    process_reset(step, hash, reset_frequency, seed)
}

/// The `(step, hash, reset_seed)` triple [`run_vdf`] restarts from, replacing the three loose
/// `anchor_*` locals the supervisor previously threaded by hand.
#[derive(Debug, Clone, Copy)]
pub struct VdfAnchor {
    pub step: u64,
    pub hash: H256,
    pub reset_seed: H256,
}

impl VdfAnchor {
    /// The startup anchor from the latest block. Folds the anchor step's own reset boundary into the
    /// raw buffer hash when the step lands on one (finding #5), so `run_vdf`'s first step continues
    /// the canonical lineage; otherwise a no-op. `seed` is the anchoring block's own
    /// `vdf_limiter_info.seed` (the in-range boundary's entropy); `reset_seed` is the next boundary
    /// above the block, which the loop applies going forward.
    #[must_use]
    pub fn at_startup(
        reset_frequency: u64,
        step: u64,
        hash: H256,
        seed: H256,
        reset_seed: H256,
    ) -> Self {
        Self {
            step,
            hash: reset_applied_anchor_hash(reset_frequency, step, hash, seed),
            reset_seed,
        }
    }
}

/// The outcome of a re-anchor request. It lets the supervisor choose break, restart, and
/// notify-mining without touching any VDF-internal mechanics.
pub enum ReanchorOutcome {
    /// The buffer was rebuilt and published; restart `run_vdf` from the anchor and notify mining.
    Reanchored(VdfAnchor),
    /// The rebuild failed (transient DB error or a missing ancestor) and the buffer was left intact;
    /// restart from the live tip and do NOT notify mining, since the rotation still matches the
    /// unchanged buffer.
    Fallback(VdfAnchor),
    /// The VDF state lock was poisoned; break the supervisor (terminal shutdown).
    Shutdown,
}

/// Re-anchor the VDF to the canonical chain tip after a deep partition recovery.
///
/// Rebuilds the step buffer from `canonical_headers` (anchored at the NEW CANONICAL TIP, not the
/// truncated index's LCA, so the buffer matches the canonical lineage across the whole recovered
/// range — each block carries its own reset-boundary seed), publishes it via
/// [`publish_reanchored_state`], drains stale fast-forward steps (which may carry the orphaned
/// fork's minority-seed steps that `store_step` would otherwise accept and re-poison), and returns
/// the folded anchor to restart [`run_vdf`] from.
///
/// On rebuild failure the live buffer is kept and a raw, UNFOLDED live-tip anchor is returned: the
/// loop free-ran above the canonical chain, no confirmed block pins the live step's boundary seed,
/// and folding a guess would re-poison (the #4 hazard); `store_step`'s forward-only rule and
/// fork-local validation recompute realign instead. Failure must be recoverable rather than fatal —
/// a panic here would drop the VDF thread's `CancelOnDrop` guard and shut the whole node down
/// mid-recovery on a transient DB error. There is no retry timer: the rebuild is re-attempted only
/// when the next deep reorg re-emits the re-anchor signal. See
/// design/docs/vdf-partition-recovery-reanchor.md.
#[expect(
    clippy::too_many_arguments,
    reason = "owns every shared VDF handle the re-anchor mutates"
)]
pub fn reanchor_to_canonical_tip(
    vdf_state: &AtomicVdfState,
    atomic_step: &AtomicVdfStepNumber,
    fast_forward_receiver: &mut Receiver<Traced<VdfStep>>,
    canonical_headers: &[Arc<IrysBlockHeader>],
    db: &DatabaseProvider,
    is_vdf_mining_enabled: Arc<AtomicBool>,
    config: &Config,
    prev_reset_seed: H256,
) -> ReanchorOutcome {
    let (new_state, next_seed) = match create_state_for_canonical_tip(
        canonical_headers,
        db,
        is_vdf_mining_enabled,
        config,
    ) {
        Ok(rebuilt) => rebuilt,
        Err(e) => {
            // Rebuild failed (transient DB error, or a canonical ancestor missing from both the
            // block-tree cache and the DB). Keep the live buffer and resume from its OWN current
            // tip — the live loop free-ran above the stale startup/previous anchor. Block
            // validation does NOT trust the live buffer (the VDF-sensitive checks resolve steps
            // from a fork-local view of the block's own lineage), so a not-yet-rebuilt buffer
            // cannot reject canonical blocks; it degrades only THIS node's local mining until a
            // later re-anchor rebuilds it.
            error!(
                error = ?e,
                "VDF re-anchor rebuild failed; resuming from the live buffer's current step (rebuild re-attempted on the next deep-reorg re-anchor)"
            );
            let Ok(guard) = vdf_state.read() else {
                error!(
                    "VDF state read lock poisoned during re-anchor fallback; exiting VDF thread"
                );
                return ReanchorOutcome::Shutdown;
            };
            let (live_step, live_seed) = guard.get_last_step_and_seed();
            // Intentionally NOT reset_applied_anchor_hash'd: the buffer was NOT rebuilt, so we
            // resume from the live tip, which free-ran ABOVE the canonical chain — no confirmed
            // block pins live_step's boundary seed to fold (guessing one would re-poison, the #4
            // hazard). Resume raw; store_step's forward-only rule + fork-local validation realign.
            return ReanchorOutcome::Fallback(VdfAnchor {
                step: live_step,
                hash: live_seed.0,
                reset_seed: prev_reset_seed,
            });
        }
    };

    let (rebuilt_step, rebuilt_seed) =
        match publish_reanchored_state(vdf_state, atomic_step, new_state) {
            Ok(tip) => tip,
            Err(ReanchorLockPoisoned) => {
                error!("VDF state write lock poisoned during re-anchor; exiting VDF thread");
                return ReanchorOutcome::Shutdown;
            }
        };

    // Discard any fast-forward steps queued before the re-anchor. They may carry steps pinned by the
    // orphaned fork's (minority) reset seed; applying them after the buffer was rebuilt to the
    // canonical TIP would re-poison it via store_step's exact-sequential accept. The canonical steps
    // are already in the rebuilt buffer, so dropping the queue loses no correct work.
    let mut drained = 0_u64;
    while fast_forward_receiver.try_recv().is_ok() {
        drained += 1;
    }
    if drained > 0 {
        debug!(
            vdf.drained_fast_forward_steps = drained,
            "Discarded stale fast-forward steps during VDF re-anchor"
        );
    }

    // Fold the rebuilt tip's own reset boundary into the raw buffer hash if the tip step lands on
    // one (finding #5). The boundary seed is the tip block's own `seed` (set_seeds pins the in-range
    // boundary's entropy there; `next_seed` targets the next boundary above the block).
    let tip_anchor_seed = canonical_headers
        .last()
        .map_or(next_seed, |tip| tip.vdf_limiter_info.seed);
    ReanchorOutcome::Reanchored(VdfAnchor {
        step: rebuilt_step,
        hash: reset_applied_anchor_hash(
            config.vdf.reset_frequency as u64,
            rebuilt_step,
            rebuilt_seed.0,
            tip_anchor_seed,
        ),
        reset_seed: next_seed,
    })
}

/// Returns `true` when the VDF loop must NOT yet cross the upcoming reset boundary.
///
/// The reset seed applied at boundary `B` is pinned by a rotation block at step
/// `B - reset_frequency`. The loop may cross `B` only once the CONFIRMED chain — the
/// canonical chain truncated to `block_migration_depth` deep, reported as
/// `confirmed_global_step_number` — has itself reached that rotation point. Crossing then
/// implies the rotation block is at least `block_migration_depth` blocks deep and so safe
/// from reorg, which is exactly what prevents the run-ahead seed poisoning of issue #1447.
///
/// Gating on the confirmed step (rather than the canonical tip, the original behaviour)
/// folds the old "readiness" check and the confirmation requirement into one comparison,
/// and is inherently reorg- and startup-safe: `confirmed_global_step_number` only advances
/// as blocks migrate, so it cannot be fooled by a fork-loser block briefly at the tip, by
/// a seed re-pinned across a reorg, or by a freshly started node's tip.
#[must_use]
pub fn is_reset_boundary_blocked(
    next_global_step: u64,
    reset_frequency: u64,
    confirmed_global_step_number: u64,
) -> bool {
    next_global_step.is_multiple_of(reset_frequency)
        && next_global_step > confirmed_global_step_number.saturating_add(reset_frequency)
}

/// Returns `None` if the VDF state write lock is poisoned, so the VDF loop can
/// exit gracefully rather than re-panic on `expect`/`unwrap`. On `Some(step)`,
/// `step` is the new global step (which may equal `new_global_step_number - 1`
/// if a gap was detected and ignored — see `VdfState::store_step`).
#[must_use]
fn store_step(
    hash: H256,
    atomic_vdf_global_step: &AtomicVdfStepNumber,
    vdf_state: &AtomicVdfState,
    new_global_step_number: u64,
    canonical_global_step_number: u64,
) -> Option<u64> {
    let mut vdf_guard = match vdf_state.write() {
        Ok(guard) => guard,
        Err(_) => {
            error!("VDF state write lock poisoned; exiting VDF thread");
            return None;
        }
    };

    vdf_guard.set_canonical_step(canonical_global_step_number);
    let global_step_number = vdf_guard.store_step(Seed(hash), new_global_step_number);
    atomic_vdf_global_step.store(global_step_number, std::sync::atomic::Ordering::Relaxed);
    Some(global_step_number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_helpers::mocked_vdf_service;
    use crate::state::{CancelEnum, VdfState, VdfStateReadonly, vdf_steps_are_valid};
    use crate::vdf_sha_verification;
    use irys_database::{IrysDatabaseArgs as _, open_or_create_db, tables::IrysTables};
    use irys_types::*;
    use nodit::interval::ii;
    use reth_db::mdbx::DatabaseArguments;
    use std::sync::atomic::AtomicU8;
    use std::{
        sync::{Arc, atomic::AtomicU64},
        time::Duration,
    };
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use tracing::{debug, level_filters::LevelFilter};
    use tracing_subscriber::{fmt::SubscriberBuilder, util::SubscriberInitExt as _};

    struct MockMining;

    impl MiningBroadcaster for MockMining {
        fn broadcast(&self, _seed: Seed, _checkpoints: H256List, _global_step: u64) {}
    }

    struct MockBlockProvider(pub IrysBlockHeader);

    impl MockBlockProvider {
        fn new() -> Self {
            Self(IrysBlockHeader::new_mock_header())
        }
    }

    impl BlockProvider for MockBlockProvider {
        fn canonical_vdf_snapshot(&self, step_number: u64) -> Option<CanonicalVdfSnapshot> {
            let _ = step_number;
            Some(CanonicalVdfSnapshot {
                vdf_info: self.0.vdf_limiter_info.clone(),
                // The mock holds a single block, so the tip is also the confirmed tip.
                confirmed_global_step_number: self.0.vdf_limiter_info.global_step_number,
                reset_seed_for_step: self.0.vdf_limiter_info.next_seed,
            })
        }
    }

    fn init_tracing() {
        let _ = SubscriberBuilder::default()
            .with_max_level(LevelFilter::DEBUG)
            .finish()
            .try_init();
    }

    #[tokio::test]
    async fn test_vdf_step() {
        let config = Config::new_with_random_peer_id(NodeConfig::testing());
        let mut checkpoints: Vec<H256> =
            vec![H256::default(); config.vdf.num_checkpoints_in_vdf_step];
        let mut hash: H256 = H256::random();
        let original_hash = hash;
        let salt: U256 = U256::from(10);

        init_tracing();

        debug!("VDF difficulty: {}", config.vdf.sha_1s_difficulty);
        let now = Instant::now();
        vdf_sha(
            salt,
            &mut hash,
            config.vdf.num_checkpoints_in_vdf_step,
            config.vdf.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );
        let elapsed = now.elapsed();
        debug!("vdf step: {:.2?}", elapsed);

        let now = Instant::now();
        let checkpoints2 = vdf_sha_verification(
            salt,
            original_hash,
            config.vdf.num_checkpoints_in_vdf_step,
            config.vdf.num_iterations_per_checkpoint(),
        );
        let elapsed = now.elapsed();
        debug!("vdf original code verification: {:.2?}", elapsed);

        assert_eq!(checkpoints, checkpoints2, "Should be equal");
    }

    #[tokio::test]
    async fn test_vdf_service() {
        let mut node_config = NodeConfig::testing();
        node_config.consensus.get_mut().vdf.reset_frequency = 2;
        node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
        let config = Config::new_with_random_peer_id(node_config);

        let seed = H256::random();
        let reset_seed = H256::random();

        init_tracing();

        let broadcast_mining_service = MockMining;
        let (_, mut ff_step_receiver) = mpsc::channel::<Traced<VdfStep>>(16);
        let (_reanchor_tx, mut reanchor_rx) = mpsc::unbounded_channel::<()>();

        let is_mining_enabled = Arc::new(AtomicBool::new(true));

        let vdf_state = mocked_vdf_service(&config);
        let vdf_steps_guard = VdfStateReadonly::new(vdf_state.clone());

        let atomic_global_step_number = Arc::new(AtomicU64::new(0));

        let mut mock_header = IrysBlockHeader::new_mock_header();
        // Set global step number to 2 to simulate a scenario where canonical chain progresses
        mock_header.vdf_limiter_info.global_step_number = 2;

        let chain_sync_state = ChainSyncState::new(false, false);
        let mining_state = Arc::clone(&is_mining_enabled);
        let shutdown_token = CancellationToken::new();
        let vdf_thread_handler = std::thread::spawn({
            let config = config.clone();
            let shutdown_token = shutdown_token.clone();
            move || {
                run_vdf(
                    &config.vdf,
                    0,
                    seed,
                    reset_seed,
                    &mut ff_step_receiver,
                    &mut reanchor_rx,
                    mining_state,
                    broadcast_mining_service,
                    vdf_state.clone(),
                    atomic_global_step_number,
                    MockBlockProvider(mock_header),
                    chain_sync_state,
                    shutdown_token,
                )
            }
        });

        // wait for some vdf steps
        tokio::time::sleep(Duration::from_millis(500)).await;

        let step_num = vdf_steps_guard.read().global_step;

        assert!(
            step_num > 4,
            "Should have more than 4 seeds, only have {}",
            step_num
        );

        // get last 4 steps
        let steps = vdf_steps_guard
            .read()
            .get_steps(ii(step_num - 3, step_num))
            .unwrap();

        // calculate last step checkpoints
        let salt = U256::from(step_number_to_salt_number(&config.vdf, step_num - 1_u64));
        let mut seed = steps[2];

        let mut checkpoints: Vec<H256> =
            vec![H256::default(); config.vdf.num_checkpoints_in_vdf_step];
        if step_num > 0 && (step_num - 1).is_multiple_of(config.vdf.reset_frequency as u64) {
            seed = apply_reset_seed(seed, reset_seed);
        }
        vdf_sha(
            salt,
            &mut seed,
            config.vdf.num_checkpoints_in_vdf_step,
            config.vdf.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );

        let vdf_info = VDFLimiterInfo {
            global_step_number: step_num,
            output: steps[3],
            prev_output: steps[0],
            steps: H256List(steps.0[1..=3].into()),
            last_step_checkpoints: H256List(checkpoints),
            seed: reset_seed,
            ..VDFLimiterInfo::default()
        };

        let pool = crate::build_verification_pool(&config.vdf);

        assert!(
            vdf_steps_are_valid(
                &pool,
                &vdf_info,
                &config.vdf,
                &vdf_steps_guard,
                Arc::new(AtomicU8::new(CancelEnum::Continue as u8))
            )
            .is_ok(),
            "Invalid VDF"
        );

        // Send shutdown signal
        shutdown_token.cancel();

        // Wait for vdf thread to finish
        vdf_thread_handler.join().unwrap();
    }

    #[tokio::test]
    async fn test_vdf_does_not_get_too_far_ahead() {
        let mut node_config = NodeConfig::testing();
        node_config.consensus.get_mut().vdf.reset_frequency = 2;
        node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
        let config = Config::new_with_random_peer_id(node_config);

        let seed = H256::random();
        let reset_seed = H256::random();

        init_tracing();

        let broadcast_mining_service = MockMining;
        let (_, mut ff_step_receiver) = mpsc::channel::<Traced<VdfStep>>(16);
        let (_reanchor_tx, mut reanchor_rx) = mpsc::unbounded_channel::<()>();

        let is_mining_enabled = Arc::new(AtomicBool::new(true));

        let vdf_state = mocked_vdf_service(&config);
        let vdf_steps_guard = VdfStateReadonly::new(vdf_state.clone());

        let atomic_global_step_number = Arc::new(AtomicU64::new(0));

        let chain_sync_state = ChainSyncState::new(false, false);
        let mining_state = Arc::clone(&is_mining_enabled);
        let shutdown_token = CancellationToken::new();
        let vdf_thread_handler = std::thread::spawn({
            let config = config.clone();
            let shutdown_token = shutdown_token.clone();
            move || {
                run_vdf(
                    &config.vdf,
                    0,
                    seed,
                    reset_seed,
                    &mut ff_step_receiver,
                    &mut reanchor_rx,
                    mining_state,
                    broadcast_mining_service,
                    vdf_state.clone(),
                    atomic_global_step_number,
                    MockBlockProvider::new(),
                    chain_sync_state,
                    shutdown_token,
                )
            }
        });

        // wait for some vdf steps
        tokio::time::sleep(Duration::from_millis(500)).await;

        let step_num = vdf_steps_guard.read().global_step;

        assert_eq!(step_num, 3);

        // get last 4 steps
        let steps = vdf_steps_guard
            .read()
            .get_steps(ii(step_num - 2, step_num))
            .unwrap();

        // calculate last step checkpoints
        let salt = U256::from(step_number_to_salt_number(&config.vdf, step_num - 1_u64));
        let mut seed = steps[2];

        let mut checkpoints: Vec<H256> =
            vec![H256::default(); config.vdf.num_checkpoints_in_vdf_step];
        if step_num > 0 && (step_num - 1).is_multiple_of(config.vdf.reset_frequency as u64) {
            seed = apply_reset_seed(seed, reset_seed);
        }
        vdf_sha(
            salt,
            &mut seed,
            config.vdf.num_checkpoints_in_vdf_step,
            config.vdf.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );

        let vdf_info = VDFLimiterInfo {
            global_step_number: step_num,
            output: steps[2],
            prev_output: steps[0],
            steps: H256List(steps.0[1..=2].into()),
            last_step_checkpoints: H256List(checkpoints),
            seed: reset_seed,
            ..VDFLimiterInfo::default()
        };

        let pool = crate::build_verification_pool(&config.vdf);

        assert!(
            vdf_steps_are_valid(
                &pool,
                &vdf_info,
                &config.vdf,
                &vdf_steps_guard,
                Arc::new(AtomicU8::new(CancelEnum::Continue as u8))
            )
            .is_ok(),
            "Invalid VDF"
        );

        // Send shutdown signal
        shutdown_token.cancel();

        // Wait for vdf thread to finish
        vdf_thread_handler.join().unwrap();
    }

    #[tokio::test]
    async fn fast_forward_remains_responsive_while_syncing_with_mining_paused() {
        let mut node_config = NodeConfig::testing();
        node_config.consensus.get_mut().vdf.reset_frequency = 2;
        node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
        let config = Config::new_with_random_peer_id(node_config);

        let current_seed = H256::random();
        let reset_seed = H256::random();
        let (ff_step_sender, mut ff_step_receiver) = mpsc::channel::<Traced<VdfStep>>(16);
        let (_reanchor_tx, mut reanchor_rx) = mpsc::unbounded_channel::<()>();
        let is_mining_enabled = Arc::new(AtomicBool::new(false));
        let vdf_state = mocked_vdf_service(&config);
        let vdf_steps_guard = VdfStateReadonly::new(vdf_state.clone());
        let atomic_global_step_number = Arc::new(AtomicU64::new(0));
        let chain_sync_state = ChainSyncState::new(true, false);
        let shutdown_token = CancellationToken::new();

        let vdf_thread_handler = std::thread::spawn({
            let config = config.clone();
            let mining_state = Arc::clone(&is_mining_enabled);
            let vdf_state = vdf_state.clone();
            let atomic_global_step_number = atomic_global_step_number.clone();
            let chain_sync_state = chain_sync_state.clone();
            let shutdown_token = shutdown_token.clone();
            move || {
                run_vdf(
                    &config.vdf,
                    0,
                    current_seed,
                    reset_seed,
                    &mut ff_step_receiver,
                    &mut reanchor_rx,
                    mining_state,
                    MockMining,
                    vdf_state,
                    atomic_global_step_number,
                    MockBlockProvider::new(),
                    chain_sync_state,
                    shutdown_token,
                )
            }
        });

        // Let the VDF loop enter its paused path before queuing a fast-forward step.
        tokio::time::sleep(Duration::from_millis(20)).await;

        ff_step_sender
            .send(Traced::new(VdfStep {
                step: H256::random(),
                global_step_number: 1,
            }))
            .await
            .unwrap();

        let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));
        tokio::time::timeout(
            Duration::from_millis(100),
            vdf_steps_guard.wait_for_step(1, cancel, Duration::from_secs(30)),
        )
        .await
        .expect("fast-forward step should be applied promptly while syncing")
        .expect("wait_for_step should not error");

        shutdown_token.cancel();
        vdf_thread_handler.join().unwrap();
    }

    /// A re-anchor signal must make the running loop return `VdfExit::Reanchor` (and stop)
    /// so the supervisor thread can rebuild the VDF state from the canonical block index
    /// after a deep reorg / network-partition recovery. The shutdown token is never set, so
    /// `Reanchor` is the only reason the loop can exit here. See
    /// design/docs/vdf-partition-recovery-reanchor.md.
    #[tokio::test]
    async fn reanchor_signal_returns_reanchor_exit() {
        let mut node_config = NodeConfig::testing();
        node_config.consensus.get_mut().vdf.reset_frequency = 2;
        node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
        let config = Config::new_with_random_peer_id(node_config);

        init_tracing();

        let seed = H256::random();
        let reset_seed = H256::random();

        let (_ff_tx, mut ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);
        let (reanchor_tx, mut reanchor_rx) = mpsc::unbounded_channel::<()>();
        let is_mining_enabled = Arc::new(AtomicBool::new(true));
        let vdf_state = mocked_vdf_service(&config);
        let vdf_steps_guard = VdfStateReadonly::new(vdf_state.clone());
        let atomic_step = Arc::new(AtomicU64::new(0));

        // Canonical far ahead so the reset-boundary gate never parks the loop.
        let mut mock_header = IrysBlockHeader::new_mock_header();
        mock_header.vdf_limiter_info.global_step_number = 10_000;

        let chain_sync_state = ChainSyncState::new(false, false);
        let shutdown_token = CancellationToken::new();
        let handle = std::thread::spawn({
            let config = config.clone();
            let shutdown_token = shutdown_token.clone();
            let mining_state = Arc::clone(&is_mining_enabled);
            move || {
                run_vdf(
                    &config.vdf,
                    0,
                    seed,
                    reset_seed,
                    &mut ff_rx,
                    &mut reanchor_rx,
                    mining_state,
                    MockMining,
                    vdf_state.clone(),
                    atomic_step,
                    MockBlockProvider(mock_header),
                    chain_sync_state,
                    shutdown_token,
                )
            }
        });

        // Let the loop produce a few steps, then request a re-anchor.
        let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));
        vdf_steps_guard
            .wait_for_step(3, Arc::clone(&cancel), Duration::from_secs(5))
            .await
            .expect("loop should produce steps before re-anchor");
        reanchor_tx
            .send(())
            .expect("re-anchor receiver should still be alive");

        let exit = handle.join().unwrap();
        assert_eq!(
            exit,
            VdfExit::Reanchor,
            "a re-anchor signal must make run_vdf return VdfExit::Reanchor"
        );
        assert!(
            !shutdown_token.is_cancelled(),
            "loop exited via re-anchor, not shutdown"
        );
    }

    fn temp_db() -> (irys_testing_utils::tempfile::TempDir, DatabaseProvider) {
        let tmp = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db = open_or_create_db(
            tmp.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();
        (tmp, DatabaseProvider(Arc::new(db)))
    }

    fn mock_canonical_block(
        height: u64,
        hash: u8,
        prev: u8,
        steps: &[u8],
        step_no: u64,
        seed: u8,
        next_seed: u8,
    ) -> Arc<IrysBlockHeader> {
        let mut h = IrysBlockHeader::new_mock_header();
        h.height = height;
        h.block_hash = H256::repeat_byte(hash);
        h.previous_block_hash = H256::repeat_byte(prev);
        h.vdf_limiter_info.global_step_number = step_no;
        h.vdf_limiter_info.steps = H256List(steps.iter().map(|b| H256::repeat_byte(*b)).collect());
        h.vdf_limiter_info.seed = H256::repeat_byte(seed);
        h.vdf_limiter_info.next_seed = H256::repeat_byte(next_seed);
        Arc::new(h)
    }

    /// `reanchor_to_canonical_tip` success path: rebuild from a self-contained canonical chain,
    /// publish the rewound buffer + atomic, and return the folded tip anchor. The tip step is a
    /// non-boundary (odd, `reset_frequency` 2), so the fold is a no-op and the anchor hash equals
    /// the tip's last step.
    #[test]
    fn reanchor_to_canonical_tip_rebuilds_and_publishes() {
        let mut node_config = NodeConfig::testing();
        node_config.consensus.get_mut().vdf.reset_frequency = 2;
        let config = Config::new_with_random_peer_id(node_config);

        // genesis(h0) <- b1(h1) <- tip(h2), fully self-contained so the rebuild never hits the DB.
        let genesis = mock_canonical_block(0, 0x00, 0xFF, &[0x10], 1, 0x90, 0x91);
        let b1 = mock_canonical_block(1, 0x01, 0x00, &[0x11, 0x12], 3, 0x92, 0x93);
        let tip = mock_canonical_block(2, 0x02, 0x01, &[0x13, 0xBB], 7, 0x94, 0x95);
        let canonical = vec![genesis, b1, tip];

        let (_tmp, db) = temp_db();
        let live = Arc::new(std::sync::RwLock::new(VdfState::new(64, 9_999, None)));
        let atomic = Arc::new(AtomicU64::new(9_999));
        let (ff_tx, mut ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);
        // Stale fast-forward steps queued before the re-anchor (e.g. the orphaned fork's
        // minority-seed steps). The re-anchor MUST discard them, or store_step would re-poison the
        // freshly rebuilt canonical buffer with the orphaned lineage. The drained queue is asserted
        // after the call, so deleting the drain loop fails this test.
        ff_tx
            .try_send(Traced::new(VdfStep {
                step: H256::repeat_byte(0xAA),
                global_step_number: 9_998,
            }))
            .expect("queue a stale fast-forward step");
        ff_tx
            .try_send(Traced::new(VdfStep {
                step: H256::repeat_byte(0xAB),
                global_step_number: 9_999,
            }))
            .expect("queue a stale fast-forward step");

        let outcome = reanchor_to_canonical_tip(
            &live,
            &atomic,
            &mut ff_rx,
            &canonical,
            &db,
            Arc::new(AtomicBool::new(true)),
            &config,
            H256::repeat_byte(0xEE), // prev_reset_seed: unused on the success path
        );

        let ReanchorOutcome::Reanchored(anchor) = outcome else {
            panic!("expected Reanchored on a successful rebuild");
        };
        assert_eq!(anchor.step, 7, "anchor step is the canonical tip's step");
        assert_eq!(
            anchor.hash,
            H256::repeat_byte(0xBB),
            "non-boundary tip step -> no-op fold -> anchor hash is the tip's last step"
        );
        assert_eq!(
            anchor.reset_seed,
            H256::repeat_byte(0x95),
            "reset seed is the tip's next_seed"
        );
        assert_eq!(
            atomic.load(std::sync::atomic::Ordering::Relaxed),
            7,
            "atomic must be published to the rebuilt step"
        );
        assert_eq!(
            live.read().unwrap().global_step,
            7,
            "buffer must be published to the rebuilt step"
        );
        assert!(
            ff_rx.try_recv().is_err(),
            "re-anchor must drain the stale fast-forward queue (else store_step could re-poison the rebuilt buffer)"
        );
    }

    /// Rebuild failure (empty canonical chain) -> keep the live buffer untouched and fall back to
    /// its own tip, carrying the previous reset seed. The atomic is not touched.
    #[test]
    fn reanchor_to_canonical_tip_falls_back_on_rebuild_failure() {
        let config = Config::new_with_random_peer_id(NodeConfig::testing());
        let (_tmp, db) = temp_db();

        let mut live_state = VdfState::new(64, 1_234, None);
        live_state.seeds.push_back(Seed(H256::repeat_byte(0x77)));
        let live = Arc::new(std::sync::RwLock::new(live_state));
        let atomic = Arc::new(AtomicU64::new(1_234));
        let (_ff_tx, mut ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);

        let outcome = reanchor_to_canonical_tip(
            &live,
            &atomic,
            &mut ff_rx,
            &[], // empty canonical chain -> create_state_for_canonical_tip errors
            &db,
            Arc::new(AtomicBool::new(true)),
            &config,
            H256::repeat_byte(0xEE),
        );

        let ReanchorOutcome::Fallback(anchor) = outcome else {
            panic!("expected Fallback on rebuild failure");
        };
        assert_eq!(
            anchor.step, 1_234,
            "fallback resumes from the live tip step"
        );
        assert_eq!(
            anchor.hash,
            H256::repeat_byte(0x77),
            "fallback resumes from the live tip seed, unfolded"
        );
        assert_eq!(
            anchor.reset_seed,
            H256::repeat_byte(0xEE),
            "fallback carries the previous reset seed"
        );
        assert_eq!(
            atomic.load(std::sync::atomic::Ordering::Relaxed),
            1_234,
            "atomic untouched on fallback"
        );
        assert_eq!(
            live.read().unwrap().global_step,
            1_234,
            "buffer untouched on fallback"
        );
    }

    /// Documented behavioural change: when the rebuild fails AND the fallback read lock is poisoned,
    /// `reanchor_to_canonical_tip` returns `Shutdown` directly (the old code kept the stale anchor
    /// and let `run_vdf` re-detect the poison on its next startup read — one dead round-trip).
    #[test]
    fn reanchor_to_canonical_tip_shuts_down_on_poisoned_lock() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        let config = Config::new_with_random_peer_id(NodeConfig::testing());
        let (_tmp, db) = temp_db();

        let live = Arc::new(std::sync::RwLock::new(VdfState::new(64, 1_234, None)));
        let atomic = Arc::new(AtomicU64::new(1_234));
        let (_ff_tx, mut ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);

        let poison = Arc::clone(&live);
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _guard = poison.write().unwrap();
            panic!("writer panic to poison the VDF state lock");
        }));
        assert!(live.is_poisoned(), "test setup: lock must be poisoned");

        let outcome = reanchor_to_canonical_tip(
            &live,
            &atomic,
            &mut ff_rx,
            &[], // empty -> rebuild fails -> fallback read hits the poisoned lock
            &db,
            Arc::new(AtomicBool::new(true)),
            &config,
            H256::repeat_byte(0xEE),
        );

        assert!(
            matches!(outcome, ReanchorOutcome::Shutdown),
            "poisoned fallback read must return Shutdown"
        );
    }

    /// Poisons the `vdf_state` RwLock, then drives `run_vdf`. Before F3 the
    /// thread re-panicked at `vdf_state.read().unwrap()` (line 68). After F3
    /// the entry-time read returns a graceful `return`, so `run_vdf` exits
    /// without panicking and the thread join is `Ok(())`.
    #[test]
    fn run_vdf_returns_gracefully_on_poisoned_state_lock() {
        let config = Config::new_with_random_peer_id(NodeConfig::testing());
        let vdf_state = mocked_vdf_service(&config);

        // Poison the lock by panicking inside a write guard from another thread.
        let poisoner_state = vdf_state.clone(); // clone: Arc handle for poisoner thread
        let _ = std::thread::spawn(move || {
            let _guard = poisoner_state.write().unwrap();
            panic!("deliberate poison");
        })
        .join();
        assert!(
            vdf_state.is_poisoned(),
            "lock must be poisoned to exercise the path"
        );

        let (_ff_tx, mut ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);
        let (_reanchor_tx, mut reanchor_rx) = mpsc::unbounded_channel::<()>();
        let is_mining_enabled = Arc::new(AtomicBool::new(true));
        let atomic_step = Arc::new(AtomicU64::new(0));
        let chain_sync_state = ChainSyncState::new(false, false);
        let shutdown_token = CancellationToken::new();

        let join_result = std::thread::spawn(move || {
            run_vdf(
                &config.vdf,
                0,
                H256::zero(),
                H256::zero(),
                &mut ff_rx,
                &mut reanchor_rx,
                is_mining_enabled,
                MockMining,
                vdf_state,
                atomic_step,
                MockBlockProvider::new(),
                chain_sync_state,
                shutdown_token,
            )
        })
        .join();

        assert!(
            join_result.is_ok(),
            "run_vdf must not panic on poisoned state lock; got: {:?}",
            join_result.err()
        );
    }

    /// Regression: a fast-forward step with a gap must not corrupt the
    /// running `hash`. Pre-fix, `hash = proposed_ff_step.step` ran before
    /// `store_step`, so a gap-rejected FF still left `hash` pointing at the
    /// future seed; the next sequential `vdf_sha` then derived a step from
    /// the wrong seed and broadcast it to peers as a fast-forward.
    /// Asserts that step 1 is the SHA-derivative of the original initial
    /// seed, not of the rejected FF's seed.
    #[tokio::test]
    async fn rejected_gap_ff_does_not_corrupt_subsequent_hash() {
        let mut node_config = NodeConfig::testing();
        // No reset within the test window so process_reset is a no-op and
        // step 1 derives directly from the initial seed.
        node_config.consensus.get_mut().vdf.reset_frequency = 1_000;
        node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
        let config = Config::new_with_random_peer_id(node_config);

        init_tracing();

        let initial_seed = H256::random();
        let reset_seed = H256::random();
        let bad_ff_seed = H256::repeat_byte(0xAA);
        let gap_target_step: u64 = 100;

        let (ff_tx, mut ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);
        let (_reanchor_tx, mut reanchor_rx) = mpsc::unbounded_channel::<()>();
        ff_tx
            .send(Traced::new(VdfStep {
                step: bad_ff_seed,
                global_step_number: gap_target_step,
            }))
            .await
            .unwrap();

        let is_mining_enabled = Arc::new(AtomicBool::new(true));
        let vdf_state = mocked_vdf_service(&config);
        let vdf_steps_guard = VdfStateReadonly::new(vdf_state.clone());
        let atomic_step = Arc::new(AtomicU64::new(0));

        // Canonical far ahead so the "too far ahead" guard never pauses.
        let mut mock_header = IrysBlockHeader::new_mock_header();
        mock_header.vdf_limiter_info.global_step_number = 10_000;

        let chain_sync_state = ChainSyncState::new(false, false);
        let shutdown_token = CancellationToken::new();
        let vdf_thread = std::thread::spawn({
            let config = config.clone();
            let shutdown_token = shutdown_token.clone();
            let mining_state = Arc::clone(&is_mining_enabled);
            move || {
                run_vdf(
                    &config.vdf,
                    0,
                    initial_seed,
                    reset_seed,
                    &mut ff_rx,
                    &mut reanchor_rx,
                    mining_state,
                    MockMining,
                    vdf_state.clone(),
                    atomic_step,
                    MockBlockProvider(mock_header),
                    chain_sync_state,
                    shutdown_token,
                )
            }
        });

        tokio::time::sleep(Duration::from_millis(300)).await;
        shutdown_token.cancel();
        vdf_thread.join().unwrap();

        let step_num = vdf_steps_guard.read().global_step;
        assert!(step_num > 0, "VDF should produce sequential steps");
        assert!(
            step_num < gap_target_step,
            "gap FF must not advance the local step to its proposed value; got {step_num}"
        );

        // Step 1 must equal vdf_sha(salt(0), initial_seed). Pre-fix, the
        // bad FF seed (post-reset) would have been used instead.
        let mut expected_hash = initial_seed;
        let mut checkpoints = vec![H256::default(); config.vdf.num_checkpoints_in_vdf_step];
        let salt = U256::from(step_number_to_salt_number(&config.vdf, 0));
        vdf_sha(
            salt,
            &mut expected_hash,
            config.vdf.num_checkpoints_in_vdf_step,
            config.vdf.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );

        let stored_step_1 = vdf_steps_guard.read().get_steps(ii(1, 1)).unwrap()[0];
        assert_eq!(
            stored_step_1, expected_hash,
            "step 1 must derive from initial_seed, not from the rejected FF's bad seed"
        );
    }

    mod boundary_gate {
        use super::super::is_reset_boundary_blocked;
        use proptest::prelude::*;

        #[rstest::rstest]
        // not a boundary -> never blocked
        #[case::not_boundary(7, 4, 100, false)]
        // boundary, confirmed chain hasn't reached the rotation point (B - rf) -> blocked
        #[case::confirmed_behind(8, 4, 0, true)]
        // boundary, confirmed chain only one step short -> blocked
        #[case::confirmed_one_short(8, 4, 3, true)]
        // boundary, confirmed chain exactly at the rotation point -> allowed
        #[case::confirmed_reached(8, 4, 4, false)]
        // boundary, confirmed chain well past the rotation point -> allowed
        #[case::confirmed_deep(8, 4, 100, false)]
        fn cases(
            #[case] next_global_step: u64,
            #[case] reset_frequency: u64,
            #[case] confirmed_global_step_number: u64,
            #[case] expected: bool,
        ) {
            assert_eq!(
                is_reset_boundary_blocked(
                    next_global_step,
                    reset_frequency,
                    confirmed_global_step_number,
                ),
                expected
            );
        }

        proptest! {
            #[test]
            fn matches_spec_and_never_panics(
                next_global_step in 0_u64..1_000_000,
                reset_frequency in 1_u64..5_000,
                confirmed_global_step_number in 0_u64..1_000_000,
            ) {
                let got = is_reset_boundary_blocked(
                    next_global_step, reset_frequency, confirmed_global_step_number,
                );
                let expected = next_global_step.is_multiple_of(reset_frequency)
                    && next_global_step > confirmed_global_step_number.saturating_add(reset_frequency);
                prop_assert_eq!(got, expected);
            }
        }

        /// The issue #1447 fix isolated to a single decision. At the exact fork window of
        /// #1447 — boundary 16, whose reset seed is pinned by the rotation block at step 8,
        /// which sits freshly at the canonical tip (0 confirmations) while the confirmed
        /// chain still trails at step 6 — the gate's outcome flips purely on which step it is
        /// given. The OLD rule fed the canonical tip and let the loop CROSS, consuming the
        /// still-forkable seed (the bug; see the wedge it causes in
        /// `issue_1447_unconfirmed_reset_seed_poisons_buffer_and_wedges_block_validation`).
        /// THIS branch feeds the confirmed step and BLOCKS the crossing until the rotation
        /// block is confirmed (the fix). Same predicate, different argument — that swap is
        /// the whole change.
        #[test]
        fn gating_on_confirmed_step_not_canonical_tip_is_the_1447_fix() {
            const RESET_FREQUENCY: u64 = 8;
            const BOUNDARY: u64 = 16; // rotation block at BOUNDARY - RESET_FREQUENCY = step 8
            const CANONICAL_TIP_STEP: u64 = 8; // rotation block freshly at the tip, 0 confirmations
            const CONFIRMED_STEP: u64 = 6; // confirmed chain still behind the rotation point

            // OLD rule (gate on the canonical tip): NOT blocked -> the loop crosses and applies
            // the unconfirmed fork-loser seed. This is the #1447 bug.
            assert!(
                !is_reset_boundary_blocked(BOUNDARY, RESET_FREQUENCY, CANONICAL_TIP_STEP),
                "old rule (gate on canonical tip) crosses the boundary on an unconfirmed seed — the #1447 bug"
            );

            // THIS branch (gate on the confirmed step): blocked -> the loop parks until the
            // rotation block is confirmed. This is the fix.
            assert!(
                is_reset_boundary_blocked(BOUNDARY, RESET_FREQUENCY, CONFIRMED_STEP),
                "confirmed-step gate parks until the seed's rotation block is confirmed — the #1447 fix"
            );
        }
    }

    #[derive(Clone)]
    struct ControllableBlockProvider(
        std::sync::Arc<
            std::sync::Mutex<(VDFLimiterInfo, u64, std::collections::HashMap<u64, H256>)>,
        >,
    );
    impl ControllableBlockProvider {
        /// The canonical tip step is held at 0 so `store_step` treats every produced step
        /// as a normal advance; the `u64` is the confirmed-chain step the gate reads.
        fn new(confirmed_global_step_number: u64) -> Self {
            let mut info = IrysBlockHeader::new_mock_header().vdf_limiter_info.clone();
            info.global_step_number = 0;
            info.next_seed = H256::repeat_byte(0xAB);
            Self(std::sync::Arc::new(std::sync::Mutex::new((
                info,
                confirmed_global_step_number,
                std::collections::HashMap::new(),
            ))))
        }
        fn set_confirmed(&self, confirmed_global_step_number: u64) {
            self.0.lock().unwrap().1 = confirmed_global_step_number;
        }
        /// Atomically update the whole snapshot the gate reads: the tip's `next_seed`, the
        /// canonical tip step (`global_step_number`), and the confirmed-chain step. One lock,
        /// so the loop never observes a half-applied transition (e.g. the gate opening before
        /// the intended seed is visible).
        fn set_snapshot(
            &self,
            next_seed: H256,
            canonical_tip_step: u64,
            confirmed_global_step_number: u64,
        ) {
            let mut g = self.0.lock().unwrap();
            g.0.next_seed = next_seed;
            g.0.global_step_number = canonical_tip_step;
            g.1 = confirmed_global_step_number;
        }
        fn set_seed_for_step(&self, step_number: u64, seed: H256) {
            self.0.lock().unwrap().2.insert(step_number, seed);
        }
    }
    impl BlockProvider for ControllableBlockProvider {
        fn canonical_vdf_snapshot(&self, step_number: u64) -> Option<CanonicalVdfSnapshot> {
            let g = self.0.lock().unwrap();
            let reset_seed_for_step = g.2.get(&step_number).copied().unwrap_or(g.0.next_seed);
            Some(CanonicalVdfSnapshot {
                vdf_info: g.0.clone(),
                confirmed_global_step_number: g.1,
                reset_seed_for_step,
            })
        }
    }

    /// Regression for issue #1447: the loop must not cross a reset boundary until the
    /// CONFIRMED chain (block_migration_depth deep) has reached the rotation point, so it
    /// never applies a seed pinned by a still-forkable block. With the confirmed step held
    /// behind the rotation point the loop parks at the boundary; once it advances to the
    /// rotation point the loop crosses.
    ///
    /// It also guards the inverse — no startup or post-reorg deadlock: a low confirmed step
    /// still lets the loop cross EARLIER boundaries it is already entitled to (reaching
    /// step 4 below), so a genesis or freshly started node is never wedged at its first
    /// boundary.
    ///
    /// Deterministic via `wait_for_step` (no fixed sleeps as the assertion mechanism).
    #[tokio::test]
    async fn parks_at_boundary_until_reset_seed_is_confirmed() {
        let mut node_config = NodeConfig::testing();
        node_config.consensus.get_mut().vdf.reset_frequency = 4;
        node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
        let config = Config::new_with_random_peer_id(node_config);

        let initial_seed = H256::repeat_byte(0xAA);

        // Confirmed chain starts at step 0: boundary 4 is allowed (4 > 0 + 4 is false) but
        // boundary 8 is gated (8 > 0 + 4), so the loop crosses 4 and parks at 8 (step 7).
        let provider = ControllableBlockProvider::new(0);

        let (_tx, mut ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);
        let (_reanchor_tx, mut reanchor_rx) = mpsc::unbounded_channel::<()>();
        let is_mining_enabled = Arc::new(AtomicBool::new(true));
        let vdf_state = mocked_vdf_service(&config);
        let vdf_steps_guard = VdfStateReadonly::new(vdf_state.clone());
        let atomic_step = Arc::new(AtomicU64::new(0));
        let chain_sync_state = ChainSyncState::new(false, false);
        let shutdown_token = CancellationToken::new();

        let handle = std::thread::spawn({
            let config = config.clone();
            let provider = provider.clone();
            let shutdown_token = shutdown_token.clone();
            let mining_state = Arc::clone(&is_mining_enabled);
            move || {
                run_vdf(
                    &config.vdf,
                    0,
                    initial_seed,
                    initial_seed,
                    &mut ff_rx,
                    &mut reanchor_rx,
                    mining_state,
                    MockMining,
                    vdf_state.clone(),
                    atomic_step,
                    provider,
                    chain_sync_state,
                    shutdown_token,
                )
            }
        });

        let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));

        // Crossing boundary 4 (reaching step 4) proves a low confirmed step does NOT wedge
        // earlier boundaries — the no-startup-deadlock invariant. The loop then parks at
        // boundary 8 (confirmed chain has not reached the rotation point), settling at 7.
        vdf_steps_guard
            .wait_for_step(4, Arc::clone(&cancel), Duration::from_secs(5))
            .await
            .expect("a low confirmed step must still allow the first boundary (no deadlock)");
        vdf_steps_guard
            .wait_for_step(7, Arc::clone(&cancel), Duration::from_secs(5))
            .await
            .expect("should reach step 7, one short of boundary 8");

        // Must stay parked at boundary 8 while the confirmed chain is behind the rotation
        // point: step 8 never arrives, so wait_for_step bails with a stall error.
        let parked = vdf_steps_guard
            .wait_for_step(8, Arc::clone(&cancel), Duration::from_millis(500))
            .await;
        assert!(
            parked.is_err(),
            "must park at the boundary while the seed's rotation block is unconfirmed"
        );
        assert_eq!(vdf_steps_guard.read().global_step, 7);

        // Advance the confirmed chain to the rotation point (step 4): now 8 > 4 + 4 is false.
        provider.set_confirmed(4);

        // Now it crosses boundary 8 (reaches step 8; it then parks at boundary 12 because
        // the confirmed chain stays at 4 — expected, not asserted).
        vdf_steps_guard
            .wait_for_step(8, Arc::clone(&cancel), Duration::from_secs(5))
            .await
            .expect("should cross the boundary once the rotation block is confirmed");

        shutdown_token.cancel();
        handle.join().unwrap();
    }

    /// Regression for the partition-recovery catch-up seed path: when the canonical tip has already
    /// passed a reset boundary, the loop must use the seed pinned for the CURRENT step rather than
    /// the tip's `next_seed`. Start the loop at step 1 with a provider whose tip seed is different
    /// from the per-step seed for step 1; crossing boundary 2 must produce step 3 from the
    /// step-1-pinned seed, not the tip seed.
    #[tokio::test]
    async fn uses_per_step_seed_from_single_snapshot_when_crossing_boundary() {
        let mut node_config = NodeConfig::testing();
        node_config.consensus.get_mut().vdf.reset_frequency = 2;
        node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
        let config = Config::new_with_random_peer_id(node_config);

        let initial_seed = H256::repeat_byte(0x33);
        let correct_boundary_seed = H256::repeat_byte(0x44);
        let tip_seed = H256::repeat_byte(0x55);

        let provider = ControllableBlockProvider::new(10_000);
        provider.set_snapshot(tip_seed, 10_000, 10_000);
        provider.set_seed_for_step(1, correct_boundary_seed);

        let mut step1 = initial_seed;
        let mut checkpoints = vec![H256::default(); config.vdf.num_checkpoints_in_vdf_step];
        vdf_sha(
            U256::from(step_number_to_salt_number(&config.vdf, 0)),
            &mut step1,
            config.vdf.num_checkpoints_in_vdf_step,
            config.vdf.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );

        let vdf_state = mocked_vdf_service(&config);
        {
            let mut guard = vdf_state.write().expect("test VDF state lock");
            guard.store_step(Seed(step1), 1);
            guard.set_canonical_step(1);
        }
        let vdf_steps_guard = VdfStateReadonly::new(vdf_state.clone());

        let (_tx, mut ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);
        let (_reanchor_tx, mut reanchor_rx) = mpsc::unbounded_channel::<()>();
        let is_mining_enabled = Arc::new(AtomicBool::new(true));
        let atomic_step = Arc::new(AtomicU64::new(1));
        let chain_sync_state = ChainSyncState::new(false, false);
        let shutdown_token = CancellationToken::new();

        let handle = std::thread::spawn({
            let config = config.clone();
            let provider = provider.clone();
            let shutdown_token = shutdown_token.clone();
            let mining_state = Arc::clone(&is_mining_enabled);
            move || {
                run_vdf(
                    &config.vdf,
                    1,
                    step1,
                    H256::zero(),
                    &mut ff_rx,
                    &mut reanchor_rx,
                    mining_state,
                    MockMining,
                    vdf_state.clone(),
                    atomic_step,
                    provider,
                    chain_sync_state,
                    shutdown_token,
                )
            }
        });

        let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));
        vdf_steps_guard
            .wait_for_step(3, Arc::clone(&cancel), Duration::from_secs(5))
            .await
            .expect("loop should reach step 3");
        shutdown_token.cancel();
        assert_eq!(handle.join().unwrap(), VdfExit::Shutdown);

        let stored_step3 = vdf_steps_guard
            .get_step(3)
            .expect("step 3 should be present");

        let mut expected_step2 = step1;
        let mut expected_checkpoints =
            vec![H256::default(); config.vdf.num_checkpoints_in_vdf_step];
        vdf_sha(
            U256::from(step_number_to_salt_number(&config.vdf, 1)),
            &mut expected_step2,
            config.vdf.num_checkpoints_in_vdf_step,
            config.vdf.num_iterations_per_checkpoint(),
            &mut expected_checkpoints,
        );
        let mut expected_step3 = apply_reset_seed(expected_step2, correct_boundary_seed);
        vdf_sha(
            U256::from(step_number_to_salt_number(&config.vdf, 2)),
            &mut expected_step3,
            config.vdf.num_checkpoints_in_vdf_step,
            config.vdf.num_iterations_per_checkpoint(),
            &mut expected_checkpoints,
        );

        let mut wrong_step3 = apply_reset_seed(expected_step2, tip_seed);
        vdf_sha(
            U256::from(step_number_to_salt_number(&config.vdf, 2)),
            &mut wrong_step3,
            config.vdf.num_checkpoints_in_vdf_step,
            config.vdf.num_iterations_per_checkpoint(),
            &mut expected_checkpoints,
        );

        assert_eq!(
            stored_step3, expected_step3,
            "step 3 must be derived from the seed pinned for step 1, not the tip seed"
        );
        assert_ne!(
            stored_step3, wrong_step3,
            "the test must distinguish the per-step seed from the tip seed"
        );
    }

    /// Regression for the FF-path exact-boundary edge: when a fast-forward step lands exactly on
    /// boundary `B`, the carried hash for subsequent local stepping must use the seed pinned at
    /// `B - 1`, not the seed for the block ending at `B` (which targets the next window).
    #[tokio::test]
    async fn fast_forward_exact_boundary_uses_pre_boundary_seed() {
        let mut node_config = NodeConfig::testing();
        node_config.consensus.get_mut().vdf.reset_frequency = 2;
        node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
        let config = Config::new_with_random_peer_id(node_config);

        let initial_seed = H256::repeat_byte(0x22);
        let correct_boundary_seed = H256::repeat_byte(0x44);
        let wrong_boundary_seed = H256::repeat_byte(0x66);
        let tip_seed = H256::repeat_byte(0x77);

        let provider = ControllableBlockProvider::new(10_000);
        provider.set_snapshot(tip_seed, 10_000, 10_000);
        provider.set_seed_for_step(1, correct_boundary_seed);
        provider.set_seed_for_step(2, wrong_boundary_seed);

        let mut step1 = initial_seed;
        let mut checkpoints = vec![H256::default(); config.vdf.num_checkpoints_in_vdf_step];
        vdf_sha(
            U256::from(step_number_to_salt_number(&config.vdf, 0)),
            &mut step1,
            config.vdf.num_checkpoints_in_vdf_step,
            config.vdf.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );
        let mut ff_step2 = step1;
        vdf_sha(
            U256::from(step_number_to_salt_number(&config.vdf, 1)),
            &mut ff_step2,
            config.vdf.num_checkpoints_in_vdf_step,
            config.vdf.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );

        let vdf_state = mocked_vdf_service(&config);
        {
            let mut guard = vdf_state.write().expect("test VDF state lock");
            guard.store_step(Seed(step1), 1);
            guard.set_canonical_step(1);
        }
        let vdf_steps_guard = VdfStateReadonly::new(vdf_state.clone());

        let (ff_tx, mut ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);
        ff_tx
            .try_send(Traced::new(VdfStep {
                global_step_number: 2,
                step: ff_step2,
            }))
            .expect("queue FF step before the loop starts");

        let (_reanchor_tx, mut reanchor_rx) = mpsc::unbounded_channel::<()>();
        let is_mining_enabled = Arc::new(AtomicBool::new(true));
        let atomic_step = Arc::new(AtomicU64::new(1));
        let chain_sync_state = ChainSyncState::new(false, false);
        let shutdown_token = CancellationToken::new();

        let handle = std::thread::spawn({
            let config = config.clone();
            let provider = provider.clone();
            let shutdown_token = shutdown_token.clone();
            let mining_state = Arc::clone(&is_mining_enabled);
            move || {
                run_vdf(
                    &config.vdf,
                    1,
                    step1,
                    H256::zero(),
                    &mut ff_rx,
                    &mut reanchor_rx,
                    mining_state,
                    MockMining,
                    vdf_state.clone(),
                    atomic_step,
                    provider,
                    chain_sync_state,
                    shutdown_token,
                )
            }
        });

        let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));
        vdf_steps_guard
            .wait_for_step(3, Arc::clone(&cancel), Duration::from_secs(5))
            .await
            .expect("loop should consume the FF boundary step and then locally reach step 3");
        shutdown_token.cancel();
        assert_eq!(handle.join().unwrap(), VdfExit::Shutdown);

        let stored_step3 = vdf_steps_guard
            .get_step(3)
            .expect("step 3 should be present");

        let mut expected_step3 = apply_reset_seed(ff_step2, correct_boundary_seed);
        vdf_sha(
            U256::from(step_number_to_salt_number(&config.vdf, 2)),
            &mut expected_step3,
            config.vdf.num_checkpoints_in_vdf_step,
            config.vdf.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );

        let mut wrong_step3 = apply_reset_seed(ff_step2, wrong_boundary_seed);
        vdf_sha(
            U256::from(step_number_to_salt_number(&config.vdf, 2)),
            &mut wrong_step3,
            config.vdf.num_checkpoints_in_vdf_step,
            config.vdf.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );

        assert_eq!(
            stored_step3, expected_step3,
            "an FF step landing on the boundary must carry forward with the seed pinned at step B-1"
        );
        assert_ne!(
            stored_step3, wrong_step3,
            "the test must distinguish the correct pre-boundary seed from the block-ending-at-B seed"
        );
    }

    /// Spin up a real `run_vdf` loop and drive it (deterministically, via `wait_for_step`)
    /// until it parks one step short of reset boundary 16, with the canonical tip already at
    /// the boundary's rotation point (step 8) but the CONFIRMED chain still behind it (step
    /// 6). That is the exact fork window of issue #1447: the seed's rotation block is at the
    /// tip yet not confirmed. The caller then decides what the loop observes next.
    ///
    /// `reset_frequency` is 8, so boundary 16's rotation point is step 8. The gate reads the
    /// confirmed step (6), not the canonical tip (8), so boundary 16 remains blocked.
    async fn parked_one_short_of_boundary_16(
        config: &Config,
    ) -> (
        ControllableBlockProvider,
        VdfStateReadonly,
        CancellationToken,
        std::thread::JoinHandle<()>,
    ) {
        // Seed applied at the earlier boundary 8; identical across runs, so the buffers can
        // only diverge at boundary 16.
        let pre_boundary_seed = H256::repeat_byte(0xAB);
        let provider = ControllableBlockProvider::new(6);
        // Canonical tip at the rotation point (8); confirmed chain behind it (6), so the gate
        // blocks boundary 16.
        provider.set_snapshot(pre_boundary_seed, 8, 6);

        let (_tx, mut ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);
        let (_reanchor_tx, mut reanchor_rx) = mpsc::unbounded_channel::<()>();
        let is_mining_enabled = Arc::new(AtomicBool::new(true));
        let vdf_state = mocked_vdf_service(config);
        let vdf_steps_guard = VdfStateReadonly::new(vdf_state.clone());
        let atomic_step = Arc::new(AtomicU64::new(0));
        let chain_sync_state = ChainSyncState::new(false, false);
        let shutdown_token = CancellationToken::new();

        let handle = std::thread::spawn({
            let config = config.clone();
            let provider = provider.clone();
            let shutdown_token = shutdown_token.clone();
            let mining_state = Arc::clone(&is_mining_enabled);
            move || {
                // Discard the VdfExit so the helper's JoinHandle stays JoinHandle<()>.
                let _ = run_vdf(
                    &config.vdf,
                    0,
                    pre_boundary_seed,
                    pre_boundary_seed,
                    &mut ff_rx,
                    &mut reanchor_rx,
                    mining_state,
                    MockMining,
                    vdf_state,
                    atomic_step,
                    provider,
                    chain_sync_state,
                    shutdown_token,
                );
            }
        });

        // Crosses boundary 8 (rotation point 0, always allowed) applying `pre_boundary_seed`,
        // then parks one short of boundary 16 because the confirmed chain (6) has not reached
        // the rotation point (8).
        let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));
        vdf_steps_guard
            .wait_for_step(15, Arc::clone(&cancel), Duration::from_secs(5))
            .await
            .expect("loop should reach step 15, parked one short of boundary 16");
        assert_eq!(
            vdf_steps_guard.read().global_step,
            15,
            "loop must be parked exactly at step 15"
        );

        (provider, vdf_steps_guard, shutdown_token, handle)
    }

    /// Issue #1447 — deterministic end-to-end reproduction across the real causal chain.
    ///
    /// A VDF loop that crosses a reset boundary while the seed's rotation block is still
    /// forkable consumes a fork-LOSER seed, poisoning its step buffer; it then REJECTS the
    /// canonical fork-WINNER block during validation and wedges. The confirmation gate
    /// prevents this: presented the same unconfirmed seed the loop parks, and crosses only
    /// once the rotation block is confirmed — so its buffer matches canonical and validation
    /// passes.
    ///
    /// This drives the real `run_vdf` loop and the real `vdf_steps_are_valid` (the function
    /// the validation service calls), deterministically via `wait_for_step`. It exercises the
    /// fix directly: the "loop stays parked while the seed is unconfirmed" assertion FAILS if
    /// the gate is reverted to read the canonical tip instead of the confirmed step.
    #[tokio::test]
    async fn issue_1447_unconfirmed_reset_seed_poisons_buffer_and_wedges_block_validation() {
        let mut node_config = NodeConfig::testing();
        node_config.consensus.get_mut().vdf.reset_frequency = 8;
        node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
        let config = Config::new_with_random_peer_id(node_config);

        // The two competing rotation blocks at boundary 16 pin distinct reset seeds.
        let loser_seed = H256::repeat_byte(0x10);
        let winner_seed = H256::repeat_byte(0x20);

        // --- Materialise the buffer a node holds after consuming the LOSER seed. ---
        // The fixed loop will not cross on an unconfirmed seed (that is the fix), so to obtain
        // the post-boundary steps such a node would compute we let it cross with the loser
        // seed confirmed. The resulting steps 17..=20 are exactly what ANY node that crossed
        // boundary 16 on the loser seed holds — including a pre-fix node that crossed at 0
        // confirmations.
        let (provider, poisoned_guard, shutdown, handle) =
            parked_one_short_of_boundary_16(&config).await;
        provider.set_snapshot(loser_seed, 8, 8);
        let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));
        poisoned_guard
            .wait_for_step(20, Arc::clone(&cancel), Duration::from_secs(5))
            .await
            .expect("loop crosses boundary 16 once the loser rotation block is confirmed");
        shutdown.cancel();
        handle.join().unwrap();

        // --- The fix: faced with the SAME unconfirmed loser seed, the loop refuses to cross.
        let (provider, clean_guard, shutdown, handle) =
            parked_one_short_of_boundary_16(&config).await;
        // The loser rotation block is at the canonical tip (step 8 = boundary 16's rotation
        // point) but NOT yet confirmed (confirmed chain at 6). The gate reads the confirmed
        // step, so the loop must stay parked. THIS is the fix — a build that gated on the
        // canonical tip would cross here and consume the loser seed.
        provider.set_snapshot(loser_seed, 8, 6);
        let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));
        let still_parked = clean_guard
            .wait_for_step(16, Arc::clone(&cancel), Duration::from_millis(500))
            .await;
        assert!(
            still_parked.is_err(),
            "confirmation gate must refuse the unconfirmed loser seed (regression guard for #1447)"
        );
        assert_eq!(clean_guard.read().global_step, 15);

        // The fork resolves: the winner's rotation block becomes canonical AND is confirmed,
        // so the loop crosses boundary 16 applying the winner seed.
        provider.set_snapshot(winner_seed, 8, 8);
        clean_guard
            .wait_for_step(20, Arc::clone(&cancel), Duration::from_secs(5))
            .await
            .expect("loop crosses boundary 16 once the winner rotation block is confirmed");
        shutdown.cancel();
        handle.join().unwrap();

        // --- Sanity: the two seeds really did fork the buffer after boundary 16. ---
        let poisoned_steps = poisoned_guard
            .get_steps(ii(17, 20))
            .expect("poisoned buffer must hold steps 17..=20");
        let clean_steps = clean_guard
            .get_steps(ii(17, 20))
            .expect("clean buffer must hold steps 17..=20");
        assert_ne!(
            poisoned_steps.0, clean_steps.0,
            "loser vs winner reset seed must diverge the VDF buffer after the boundary"
        );

        // --- NOTE (post recompute-on-mismatch fix): a buffer MISMATCH alone no longer rejects —
        //     `vdf_step_batch_is_valid` now recomputes from the block's own seed. This simplified
        //     `canonical_block` carries DEFAULT seed/prev_output, so it is self-inconsistent: the
        //     poisoned buffer rejects it via that recompute (its steps don't reproduce from its own
        //     seed), while the clean buffer accepts it via the fast-path match (it already holds
        //     these exact steps). The validation-level wedge proper — a poisoned buffer rejecting an
        //     HONEST canonical block — is FIXED and covered by
        //     `state::tests::vdf_step_batch_recomputes_on_buffer_mismatch`. ---
        let canonical_block = VDFLimiterInfo {
            global_step_number: 20,
            steps: clean_steps,
            ..VDFLimiterInfo::default()
        };
        let pool = crate::build_verification_pool(&config.vdf);

        let rejected = vdf_steps_are_valid(
            &pool,
            &canonical_block,
            &config.vdf,
            &poisoned_guard,
            Arc::new(AtomicU8::new(CancelEnum::Continue as u8)),
        );
        assert!(
            rejected.is_err(),
            "poisoned buffer rejects this self-inconsistent simplified block via recompute (the \
             honest-fork wedge itself is fixed — see vdf_step_batch_recomputes_on_buffer_mismatch)"
        );

        let accepted = vdf_steps_are_valid(
            &pool,
            &canonical_block,
            &config.vdf,
            &clean_guard,
            Arc::new(AtomicU8::new(CancelEnum::Continue as u8)),
        );
        assert!(
            accepted.is_ok(),
            "clean buffer (gate held until the seed was confirmed) must ACCEPT the canonical block: {accepted:?}"
        );
    }

    /// Regression for finding #5: a VDF anchor hash read back from the (raw) step buffer must have
    /// its OWN reset boundary folded in before it seeds the loop. The buffer stores raw step values
    /// (the reset fold is applied to the carried hash only AFTER a step is stored), so an anchor
    /// hash from `get_last_step_and_seed` is unfolded. When the anchor step is a reset boundary,
    /// `reset_applied_anchor_hash` folds boundary K's seed (the anchoring block's own `seed`); the
    /// first forward step (K+1) then matches the canonical lineage. Skipping the fold (the #5 bug)
    /// mis-steps the first range.
    #[test]
    fn anchor_hash_folds_its_own_reset_boundary_before_seeding_the_loop() {
        let mut node_config = NodeConfig::testing();
        node_config.consensus.get_mut().vdf.reset_frequency = 4;
        let config = Config::new_with_random_peer_id(node_config);
        let reset_frequency = config.vdf.reset_frequency as u64;

        let raw_anchor = H256::repeat_byte(0xA1); // buffer's raw step-8 value
        let boundary_seed = H256::repeat_byte(0xB2); // boundary 8's seed (the anchoring block's `seed`)

        // Anchor exactly on boundary 8: the raw hash must be folded with the boundary seed.
        let folded = reset_applied_anchor_hash(reset_frequency, 8, raw_anchor, boundary_seed);
        assert_eq!(
            folded,
            apply_reset_seed(raw_anchor, boundary_seed),
            "anchoring on a reset boundary must fold the boundary seed into the hash"
        );
        assert_ne!(
            folded, raw_anchor,
            "the fold must change the hash on a boundary"
        );

        // Anchor off a boundary (step 9): the hash is used as-is.
        assert_eq!(
            reset_applied_anchor_hash(reset_frequency, 9, raw_anchor, boundary_seed),
            raw_anchor,
            "anchoring off a boundary must leave the hash untouched"
        );

        // The fold is load-bearing: the next VDF step computed from the folded vs raw anchor differs,
        // so skipping it diverges step K+1 from the canonical lineage.
        let salt = U256::from(step_number_to_salt_number(&config.vdf, 8));
        let mut checkpoints = vec![H256::default(); config.vdf.num_checkpoints_in_vdf_step];
        let mut next_from_folded = folded;
        vdf_sha(
            salt,
            &mut next_from_folded,
            config.vdf.num_checkpoints_in_vdf_step,
            config.vdf.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );
        let mut next_from_raw = raw_anchor;
        vdf_sha(
            salt,
            &mut next_from_raw,
            config.vdf.num_checkpoints_in_vdf_step,
            config.vdf.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );
        assert_ne!(
            next_from_folded, next_from_raw,
            "folding boundary K's seed changes step K+1 — skipping it (finding #5) mis-steps the loop"
        );
    }

    mod process_reset_props {
        use super::super::process_reset;
        use crate::apply_reset_seed;
        use irys_types::H256;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn at_boundary_applies_seed(
                k in 0_u64..50,
                reset_frequency in 1_u64..100,
                hash_bytes in any::<[u8; 32]>(),
                seed_bytes in any::<[u8; 32]>(),
            ) {
                let global_step = k * reset_frequency;
                let hash = H256::from(hash_bytes);
                let reset_seed = H256::from(seed_bytes);
                let result = process_reset(global_step, hash, reset_frequency, reset_seed);
                prop_assert_eq!(result, apply_reset_seed(hash, reset_seed));
            }

            #[test]
            fn not_at_boundary_returns_hash(
                k in 0_u64..50,
                reset_frequency in 2_u64..100,
                hash_bytes in any::<[u8; 32]>(),
                seed_bytes in any::<[u8; 32]>(),
            ) {
                let global_step = k * reset_frequency + 1;
                let hash = H256::from(hash_bytes);
                let reset_seed = H256::from(seed_bytes);
                let result = process_reset(global_step, hash, reset_frequency, reset_seed);
                prop_assert_eq!(result, hash);
            }
        }
    }
}
