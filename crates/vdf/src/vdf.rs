use crate::metrics;
use crate::state::AtomicVdfState;
use crate::vdf_utils::{ReanchorReceiver, ReanchorRequest, ReanchorSignals};
use crate::{MiningBroadcaster, VdfStep, apply_reset_seed, step_number_to_salt_number, vdf_sha};
use irys_domain::chain_sync_state::ChainSyncState;
use irys_types::block_provider::{BlockProvider, CanonicalVdfSnapshot};
use irys_types::{H256, H256List, IrysBlockHeader, Traced, U256, block_production::Seed};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Receiver;
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

pub fn run_vdf<B: BlockProvider>(
    config: &irys_types::VdfConfig,
    global_step_number: u64,
    current_vdf_hash: H256,
    initial_reset_seed: H256,
    mut fast_forward_receiver: Receiver<Traced<VdfStep>>,
    mut reanchor_receiver: ReanchorReceiver,
    reanchor_signals: ReanchorSignals,
    broadcast_mining_service: impl MiningBroadcaster,
    vdf_state: AtomicVdfState,
    block_provider: B,
    chain_sync_state: ChainSyncState,
    shutdown_token: CancellationToken,
) {
    let _span = tracing::info_span!("vdf_loop").entered();
    let mut next_reset_seed = initial_reset_seed;
    // Step number of the deepest block confirmed past the migration threshold
    // (`block_migration_depth`), as reported by the canonical snapshot. The reset-boundary
    // gate compares the upcoming boundary against THIS step, not the canonical tip's, so
    // the loop never applies a reset seed pinned by a still-forkable block — neither a
    // 0-confirmation fork-loser seed (issue #1447) nor a seed re-pinned by a reorg.
    // Because it tracks the confirmed chain (which only advances as blocks migrate), it
    // is also reorg- and startup-safe with no per-seed bookkeeping.
    // Extract both the confirmed canonical step and the shared mining-enable
    // flag under a single read guard. The flag is the SSOT `Arc<AtomicBool>`
    // owned by `VdfState`; cloning the `Arc` once here lets the loop gate on it
    // lock-free thereafter.
    let (mut canonical_global_step_number, is_mining_enabled) = match vdf_state.read() {
        Ok(guard) => (guard.canonical_step(), guard.mining_flag()),
        Err(_) => {
            // A prior panic in another caller poisoned the lock. Bail rather
            // than re-panic; the lifecycle's vdf_done channel surfaces this
            // as a controlled exit.
            error!("VDF state read lock poisoned at startup; exiting VDF thread");
            return;
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
            break;
        }

        // VDF re-anchor: after a deep reorg the block-tree gate ships canonical
        // seeds to heal a buffer poisoned across a reset boundary. We rewrite the
        // buffer's VALUES in place and leave the counter forward (see
        // `VdfState::reanchor_seeds`), then continue stepping from the new tip.
        // The watch channel holds only the newest request, so a burst of deep
        // reorgs coalesces to the freshest canonical window by construction.
        if reanchor_receiver.has_changed().unwrap_or(false)
            && let Some(request) = reanchor_receiver.borrow_and_update().clone()
        {
            // Re-assert suspect before healing. A request queued while a prior
            // heal was mid-recompute is only consumed AFTER that heal's
            // `record_heal_applied` cleared the flag; without this re-mark the
            // second heal would freeze the counter with the stall suppression
            // in `wait_for_step_inner` disarmed, escalating a live validation
            // wait into the never-mislabel panic.
            reanchor_signals.mark_buffer_suspect();
            match apply_reanchor(
                request,
                config,
                &vdf_state,
                global_step_number,
                &mut checkpoints,
                &shutdown_token,
            ) {
                ReanchorOutcome::Healed {
                    new_hash,
                    new_next_seed,
                } => {
                    // Bump the fast-forward generation and clear the suspect
                    // flag. Any step stamped with an older generation was
                    // validated against the pre-heal buffer and is dropped by
                    // the check in the fast-forward loop below — a precise
                    // barrier, not an indiscriminate drain: post-heal steps
                    // (current generation) are still applied, so an in-flight
                    // validation waiting on its own steps is not stranded.
                    reanchor_signals.record_heal_applied();
                    hash = new_hash;
                    next_reset_seed = new_next_seed;
                    info!(
                        vdf.global_step_number = global_step_number,
                        "VDF buffer re-anchored to canonical after deep reorg"
                    );
                    // Step numbers are unchanged, so nothing else prompts miners to
                    // discard rotation derived from the poisoned seeds — tell them to
                    // rebuild from the healed buffer.
                    broadcast_mining_service.broadcast_reanchored();
                }
                // Buffer untouched; the suspect flag stays set, so the
                // block-tree gate re-sends a fresh request from the next
                // canonical tip (no broadcast — miners keep their rotation).
                // Free-run remains paused while suspect so live_step cannot
                // race further past the second-boundary skip condition.
                ReanchorOutcome::Skipped => {
                    metrics::record_reanchor_skipped();
                    warn!(
                        vdf.global_step_number = global_step_number,
                        "VDF re-anchor skipped; buffer remains suspect until a fresher tip heals"
                    );
                }
                ReanchorOutcome::PoisonedLock => return,
            }
            continue;
        }

        // check for VDF fast forward step
        while let Ok(traced_ff_step) = fast_forward_receiver.try_recv() {
            let (proposed_ff_step, _entered) = traced_ff_step.into_inner();
            // Drop a step stamped with an older re-anchor generation: it was
            // validated against a buffer that has since been re-anchored in place,
            // so replaying it could re-poison the healed buffer. The sender re-emits
            // it under the current generation when the block requeues.
            if proposed_ff_step.generation < reanchor_signals.generation() {
                debug!(
                    vdf.ff_step = proposed_ff_step.global_step_number,
                    vdf.step_generation = proposed_ff_step.generation,
                    "Dropping stale fast-forward step from before a re-anchor"
                );
                continue;
            }
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
                // A residual, *theoretical* concern remains: the step buffer is a single
                // append-only sequence and validation rejects a block whose steps disagree with
                // it, so a competing fork whose post-boundary steps differ could — if validated
                // first — make the canonical block be rejected. Reaching that requires a reorg
                // ~one reset window deep (where the boundary's reset seed is pinned), far deeper
                // than `block_migration_depth`. Such a fork is already refused at p2p block-pool
                // admission (`PartOfAPrunedFork`, plus the block tree's no-reorg-past-migration
                // rule) before its blocks are ever validated/fast-forwarded, so the FF path needs
                // no gate. Full mechanism, the deep-reorg bound, the admission guards, and the
                // invariant relied upon: design/docs/vdf-reset-seed-confirmation-gate.md.
                // Seed for the boundary we are about to cross after storing this step:
                // look up the block ending at or before (proposed_step - 1), not the tip.
                // Tip-only next_seed is wrong when local is behind the tip across a reset
                // window (partition recovery / catch-up).
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
                    &vdf_state,
                    proposed_ff_step.global_step_number,
                    canonical_global_step_number,
                ) else {
                    error!(
                        vdf.proposed_step = proposed_ff_step.global_step_number,
                        "VDF thread exiting: store_step failed during fast-forward (lock poisoned)"
                    );
                    return;
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
            // Per-step seed: the reset seed pinned for the next boundary above
            // `global_step_number`, not always the tip's next_seed.
            next_reset_seed = reset_seed_for_step;
            canonical_global_step_number = vdf_info.global_step_number;
            confirmed_global_step_number = confirmed_step;
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

        // Free-running steps while the buffer is suspect would (a) mine on
        // minority-fork seeds and (b) push `live_step` further past the
        // second-boundary skip in `apply_reanchor`, making heals harder to land.
        // Pause local production until a heal clears the flag; FF of validated
        // peer steps still runs above and is generation-stamped.
        let buffer_suspect = reanchor_signals.is_buffer_suspect();

        // if mining disabled, buffer suspect, or gated at a reset boundary —
        // wait and continue (re-check re-anchor / FF first next iteration).
        if !is_mining_enabled.load(std::sync::atomic::Ordering::Relaxed)
            || buffer_suspect
            || is_too_far_ahead
        {
            if buffer_suspect {
                debug!(
                    vdf.global_step_number = global_step_number,
                    "VDF free-run paused: seed buffer suspect pending re-anchor heal"
                );
            } else if is_too_far_ahead {
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
            &vdf_state,
            global_step_number + 1,
            canonical_global_step_number,
        ) else {
            error!(
                vdf.global_step_number = global_step_number,
                "VDF thread exiting: store_step failed during local stepping (lock poisoned)"
            );
            return;
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
    debug!(vdf.global_step_number = ?global_step_number, "VDF thread stopped");
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

/// Outcome of an [`apply_reanchor`] attempt.
enum ReanchorOutcome {
    /// The buffer was rewritten to canonical. Carries the loop's new running
    /// `hash` (the seed feeding `live_step + 1`) and `next_reset_seed`.
    Healed { new_hash: H256, new_next_seed: H256 },
    /// The request was not applied and the buffer is untouched (unusable window,
    /// canonical tip ahead of the live step, or the tail would cross a second,
    /// unpinned reset boundary). Validation's fork-local recompute keeps the
    /// node on canonical meanwhile, and the block-tree gate re-sends a fresh
    /// request from every new canonical tip while the buffer stays suspect.
    Skipped,
    /// The state write lock was poisoned; the caller must exit the VDF thread.
    PoisonedLock,
}

/// Apply a [`ReanchorRequest`]: overwrite the seed buffer with the canonical
/// window `[floor, c_step]`, recompute the free-running tail `(c_step, live_step]`
/// from the canonical tip seed (mirroring the main loop's step derivation), and
/// install it via `VdfState::reanchor_seeds` — leaving the step counter forward
/// at `live_step`.
///
/// Fold schedule for the recompute (each fold seed must be pinned by a canonical
/// block at or before the tip):
/// - at `c_step` itself, when it sits on a reset boundary: the tip's `seed`
///   (the first post-tip block starts at `c_step + 1`, contains no boundary
///   step, and so carries the tip's `seed` forward per `calculate_seeds`);
/// - at the FIRST boundary strictly after `c_step`: the tip's `next_seed`
///   (that boundary's rotation block sits at `B - reset_frequency <= c_step`);
/// - at the SECOND boundary after `c_step`: no canonical block has pinned the
///   fold seed yet (its rotation block lies strictly past the tip), so the
///   request is [`ReanchorOutcome::Skipped`] rather than folded speculatively.
///   The gate re-sends with a fresher `c_step` on the next canonical advance.
///
/// The tail recompute is sequential (~1 s/step at production difficulty) and
/// runs on the VDF thread, so it honours `shutdown_token`: a cancellation
/// mid-recompute aborts to [`ReanchorOutcome::Skipped`] BEFORE the buffer
/// write, keeping shutdown prompt instead of stalling it into the watchdog.
#[must_use]
fn apply_reanchor(
    request: ReanchorRequest,
    config: &irys_types::VdfConfig,
    vdf_state: &AtomicVdfState,
    live_step: u64,
    checkpoints: &mut [H256],
    shutdown_token: &CancellationToken,
) -> ReanchorOutcome {
    let ReanchorRequest {
        mut canonical_seeds,
        c_step,
        seed,
        next_seed,
    } = request;
    let reset_frequency = config.reset_frequency as u64;

    // Guard against an unusable request rather than corrupt the buffer: an empty
    // window, or a canonical tip somehow ahead of the free-running counter.
    let Some(tip_seed) = canonical_seeds.back().map(|s| s.0) else {
        warn!(
            c_step,
            live_step, "VDF re-anchor request had no seeds; skipping"
        );
        return ReanchorOutcome::Skipped;
    };
    if c_step > live_step {
        warn!(
            c_step,
            live_step, "VDF re-anchor tip is ahead of the live step; skipping"
        );
        return ReanchorOutcome::Skipped;
    }
    // The tail may fold at most ONE boundary past the tip (see the fn doc). The
    // second boundary after `c_step` is `(c_step / rf + 2) * rf` whether or not
    // `c_step` sits on a boundary itself; reaching it means folding a seed no
    // canonical block has pinned.
    if reset_frequency != 0 {
        let second_boundary_after_tip = (c_step / reset_frequency)
            .saturating_add(2)
            .saturating_mul(reset_frequency);
        if live_step >= second_boundary_after_tip {
            warn!(
                c_step,
                live_step,
                second_boundary_after_tip,
                "VDF re-anchor tail would cross a second, unpinned reset boundary; skipping until a fresher canonical tip arrives"
            );
            return ReanchorOutcome::Skipped;
        }
    }

    // Recompute the free-running tail `(c_step, live_step]` exactly as the main
    // loop would: salt(step-1) -> vdf_sha -> store the pre-reset value -> fold at
    // boundaries per the schedule in the fn doc. `running` tracks the seed fed
    // into the NEXT step (post-reset), matching `hash` in the loop. When `c_step`
    // sits on a boundary this first fold uses the tip's `seed`; off-boundary it
    // is a no-op and the argument is ignored.
    let mut running = process_reset(c_step, tip_seed, reset_frequency, seed);
    for step in (c_step + 1)..=live_step {
        // Abort BEFORE the buffer write: plain `Skipped` semantics — the
        // suspect flag stays set and the gate re-sends from the next canonical
        // tip if the node comes back before shutdown completes.
        if shutdown_token.is_cancelled() {
            warn!(
                c_step,
                live_step, step, "VDF re-anchor aborted by shutdown; buffer left untouched"
            );
            return ReanchorOutcome::Skipped;
        }
        let salt = U256::from(step_number_to_salt_number(config, step - 1));
        let mut step_seed = running;
        vdf_sha(
            salt,
            &mut step_seed,
            config.num_checkpoints_in_vdf_step,
            config.num_iterations_per_checkpoint(),
            checkpoints,
        );
        canonical_seeds.push_back(Seed(step_seed));
        running = process_reset(step, step_seed, reset_frequency, next_seed);
    }

    match vdf_state.write() {
        Ok(mut guard) => guard.reanchor_seeds(canonical_seeds),
        Err(_) => {
            error!("VDF state write lock poisoned during re-anchor; exiting VDF thread");
            return ReanchorOutcome::PoisonedLock;
        }
    }

    // `running` is the post-reset seed at `live_step` — the hash the loop needs to
    // compute `live_step + 1`. Adopt the canonical reset seed for future steps.
    ReanchorOutcome::Healed {
        new_hash: running,
        new_next_seed: next_seed,
    }
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
///
/// This is the SHALLOW-reorg half of fork-related VDF correctness — prevention inside the
/// loop. The DEEP-reorg half is an out-of-band cure:
/// `BlockTreeServiceInner::maybe_reanchor_vdf_after_reorg` re-anchors the buffer once a reorg
/// has already crossed a divergent boundary (see `reorg_crossed_divergent_boundary`). Two
/// homes because they need different information; see
/// `design/docs/vdf-reset-seed-confirmation-gate.md`.
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
    // VdfState owns the single step counter; store_step publishes it under
    // this write guard.
    let global_step_number = vdf_guard.store_step(Seed(hash), new_global_step_number);
    Some(global_step_number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_helpers::mocked_vdf_service;
    use crate::state::{CancelEnum, VdfController, VdfStateReadonly, vdf_steps_are_valid};
    use crate::vdf_sha_verification;
    use irys_types::*;
    use nodit::interval::ii;
    use std::sync::atomic::AtomicU8;
    use std::{sync::Arc, time::Duration};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use tracing::{debug, level_filters::LevelFilter};
    use tracing_subscriber::{fmt::SubscriberBuilder, util::SubscriberInitExt as _};

    /// Enable mining on a mocked VDF state. `run_vdf` reads the mining flag from
    /// the state (the SSOT `Arc<AtomicBool>`), so tests toggle it there rather
    /// than passing a separate flag argument.
    fn enable_mining(vdf_state: &AtomicVdfState) {
        VdfController::new(&vdf_state.read().unwrap().mining_flag()).start();
    }

    struct MockMining;

    impl MiningBroadcaster for MockMining {
        fn broadcast(&self, _seed: Seed, _checkpoints: H256List, _global_step: u64) {}
        fn broadcast_reanchored(&self) {}
    }

    struct MockBlockProvider(pub IrysBlockHeader);

    impl MockBlockProvider {
        fn new() -> Self {
            Self(IrysBlockHeader::new_mock_header())
        }
    }

    impl BlockProvider for MockBlockProvider {
        fn canonical_vdf_snapshot(&self, _step_number: u64) -> Option<CanonicalVdfSnapshot> {
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
        let (_, ff_step_receiver) = mpsc::channel::<Traced<VdfStep>>(16);

        let vdf_state = mocked_vdf_service(&config);
        let vdf_steps_guard = VdfStateReadonly::new(vdf_state.clone());
        enable_mining(&vdf_state);

        let mut mock_header = IrysBlockHeader::new_mock_header();
        // Set global step number to 2 to simulate a scenario where canonical chain progresses
        mock_header.vdf_limiter_info.global_step_number = 2;

        let chain_sync_state = ChainSyncState::new(false, false);
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
                    ff_step_receiver,
                    crate::reanchor_channel().1,
                    crate::vdf_utils::ReanchorSignals::new(),
                    broadcast_mining_service,
                    vdf_state.clone(),
                    MockBlockProvider(mock_header),
                    chain_sync_state,
                    shutdown_token,
                )
            }
        });

        // wait for some vdf steps
        tokio::time::sleep(Duration::from_millis(500)).await;

        let step_num = vdf_steps_guard.read().current_step();

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
        let (_, ff_step_receiver) = mpsc::channel::<Traced<VdfStep>>(16);

        let vdf_state = mocked_vdf_service(&config);
        let vdf_steps_guard = VdfStateReadonly::new(vdf_state.clone());
        enable_mining(&vdf_state);

        let chain_sync_state = ChainSyncState::new(false, false);
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
                    ff_step_receiver,
                    crate::reanchor_channel().1,
                    crate::vdf_utils::ReanchorSignals::new(),
                    broadcast_mining_service,
                    vdf_state.clone(),
                    MockBlockProvider::new(),
                    chain_sync_state,
                    shutdown_token,
                )
            }
        });

        // wait for some vdf steps
        tokio::time::sleep(Duration::from_millis(500)).await;

        let step_num = vdf_steps_guard.read().current_step();

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
        let (ff_step_sender, ff_step_receiver) = mpsc::channel::<Traced<VdfStep>>(16);
        // Mining stays disabled (mocked_vdf_service defaults the flag to false):
        // this test exercises the fast-forward path while mining is paused.
        let vdf_state = mocked_vdf_service(&config);
        let vdf_steps_guard = VdfStateReadonly::new(vdf_state.clone());
        let chain_sync_state = ChainSyncState::new(true, false);
        let shutdown_token = CancellationToken::new();

        let vdf_thread_handler = std::thread::spawn({
            let config = config.clone();
            let vdf_state = vdf_state.clone();
            let chain_sync_state = chain_sync_state.clone();
            let shutdown_token = shutdown_token.clone();
            move || {
                run_vdf(
                    &config.vdf,
                    0,
                    current_seed,
                    reset_seed,
                    ff_step_receiver,
                    crate::reanchor_channel().1,
                    crate::vdf_utils::ReanchorSignals::new(),
                    MockMining,
                    vdf_state,
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
                generation: 0,
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

        let (_ff_tx, ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);
        // Lock is already poisoned; run_vdf bails at the entry-time read before
        // it ever reads the mining flag, so no need to enable mining here.
        let chain_sync_state = ChainSyncState::new(false, false);
        let shutdown_token = CancellationToken::new();

        let join_result = std::thread::spawn(move || {
            run_vdf(
                &config.vdf,
                0,
                H256::zero(),
                H256::zero(),
                ff_rx,
                crate::reanchor_channel().1,
                crate::vdf_utils::ReanchorSignals::new(),
                MockMining,
                vdf_state,
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

    /// The re-anchor generation barrier: a fast-forward step stamped with an
    /// OLDER generation than the buffer's current one is dropped, so it cannot
    /// replay onto a since-healed buffer. Proven positively: a stale step and a
    /// fresh step for the SAME slot (step 1) are queued in order; the buffer must
    /// end up holding the FRESH seed. If the stale step were applied instead of
    /// dropped, the fresh step would be a no-op (the counter already advanced) and
    /// the buffer would hold the stale seed.
    #[tokio::test]
    async fn drops_fast_forward_step_from_an_older_reanchor_generation() {
        let config = Config::new_with_random_peer_id(NodeConfig::testing());
        let vdf_state = mocked_vdf_service(&config);
        let vdf_steps_guard = VdfStateReadonly::new(vdf_state.clone());

        // Current generation 1: one re-anchor has already been applied.
        let reanchor_signals = crate::vdf_utils::ReanchorSignals::new();
        reanchor_signals.record_heal_applied();
        let (ff_tx, ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);

        let stale_seed = H256::repeat_byte(0xAA);
        let fresh_seed = H256::repeat_byte(0xBB);
        // Queue the stale step (generation 0) BEFORE the fresh step (generation 1).
        ff_tx
            .send(Traced::new(VdfStep {
                step: stale_seed,
                global_step_number: 1,
                generation: 0,
            }))
            .await
            .unwrap();
        ff_tx
            .send(Traced::new(VdfStep {
                step: fresh_seed,
                global_step_number: 1,
                generation: 1,
            }))
            .await
            .unwrap();

        // Mining paused + syncing, so the loop only advances via fast-forward.
        let chain_sync_state = ChainSyncState::new(true, false);
        let shutdown_token = CancellationToken::new();
        let handle = std::thread::spawn({
            let config = config.clone();
            let vdf_state = vdf_state.clone();
            let reanchor_signals = reanchor_signals.clone();
            let chain_sync_state = chain_sync_state.clone();
            let shutdown_token = shutdown_token.clone();
            move || {
                run_vdf(
                    &config.vdf,
                    0,
                    H256::zero(),
                    H256::zero(),
                    ff_rx,
                    crate::reanchor_channel().1,
                    reanchor_signals,
                    MockMining,
                    vdf_state,
                    MockBlockProvider::new(),
                    chain_sync_state,
                    shutdown_token,
                )
            }
        });

        let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));
        vdf_steps_guard
            .wait_for_step(1, cancel, Duration::from_secs(10))
            .await
            .expect("the fresh (current-generation) step must be applied");

        let steps = vdf_steps_guard
            .read()
            .get_steps(ii(1, 1))
            .expect("step 1 present");
        assert_eq!(
            steps.0,
            vec![fresh_seed],
            "the stale-generation step must be dropped, so the fresh seed wins"
        );

        shutdown_token.cancel();
        handle.join().unwrap();
    }

    /// A re-anchor request consumed AFTER a prior heal cleared the suspect flag
    /// must itself run suspect-marked. `run_vdf` clears the flag on an applied
    /// heal, so a request queued while that heal was still recomputing arrives
    /// with the flag already clear; the consume-time re-mark is what keeps the
    /// following heal (or a deterministic skip) from running unmarked and
    /// disarming the stall suppression in `wait_for_step_inner` — the path that
    /// escalates a live validation wait into the never-mislabel panic. A skip
    /// never sets the flag, so the second request (its tip ahead of the live
    /// step) can only leave the buffer suspect via that re-mark.
    #[tokio::test]
    async fn consuming_a_reanchor_request_re_marks_the_buffer_suspect() {
        // Bounded poll over a flag the VDF thread flips from another OS thread;
        // bails rather than hangs so a regression surfaces as a test failure.
        async fn poll_until(mut cond: impl FnMut() -> bool, msg: &str) {
            for _ in 0..200 {
                if cond() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("timed out after 5s waiting for: {msg}");
        }

        let mut node_config = NodeConfig::testing();
        node_config.consensus.get_mut().vdf.reset_frequency = 5;
        node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
        let config = Config::new_with_random_peer_id(node_config);

        // Poisoned buffer parked at live_step 8, values a distinct sentinel.
        let live_step = 8_u64;
        let mut state = crate::state::VdfState::new(
            1_000,
            live_step,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        state.seeds = vec![Seed(H256::repeat_byte(0xEE)); live_step as usize].into();
        let vdf_state: crate::state::AtomicVdfState = Arc::new(std::sync::RwLock::new(state));

        let signals = crate::vdf_utils::ReanchorSignals::new();
        let (reanchor_tx, reanchor_rx) = crate::reanchor_channel();
        let (_ff_tx, ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);

        // Mining stays DISABLED: the parked loop still polls the re-anchor
        // channel each ~200ms, which is all this test drives.
        let chain_sync_state = ChainSyncState::new(false, false);
        let shutdown_token = CancellationToken::new();
        let handle = std::thread::spawn({
            let config = config.clone();
            let vdf_state = vdf_state.clone();
            let reanchor_signals = signals.clone();
            let chain_sync_state = chain_sync_state.clone();
            let shutdown_token = shutdown_token.clone();
            move || {
                run_vdf(
                    &config.vdf,
                    live_step,
                    H256::zero(),
                    H256::zero(),
                    ff_rx,
                    reanchor_rx,
                    reanchor_signals,
                    MockMining,
                    vdf_state,
                    MockBlockProvider::new(),
                    chain_sync_state,
                    shutdown_token,
                )
            }
        });

        // (1) Mirror the gate: mark suspect, then queue a request whose tip
        // sits exactly at the live step — an empty tail, so it heals at once.
        signals.mark_buffer_suspect();
        let heal_request = crate::vdf_utils::ReanchorRequest {
            canonical_seeds: (1..=live_step)
                .map(|i| Seed(H256::from_low_u64_be(i)))
                .collect(),
            c_step: live_step,
            seed: H256::repeat_byte(0x33),
            next_seed: H256::repeat_byte(0x22),
        };
        assert!(
            reanchor_tx.try_send(heal_request),
            "VDF receiver must be live"
        );
        poll_until(|| signals.generation() == 1, "the healing request to apply").await;
        assert!(
            !signals.is_buffer_suspect(),
            "an applied heal must clear the suspect flag"
        );

        // (2) Queue a request the thread must SKIP: its tip is ahead of the
        // live step (`c_step > live_step`), a deterministic skip that never
        // sets the flag. Consumed only after (1) cleared suspect, so the flag
        // can return solely via the consume-time re-mark under test.
        let skip_c_step = live_step + 2;
        let skip_request = crate::vdf_utils::ReanchorRequest {
            canonical_seeds: (1..=skip_c_step)
                .map(|i| Seed(H256::from_low_u64_be(i)))
                .collect(),
            c_step: skip_c_step,
            seed: H256::repeat_byte(0x33),
            next_seed: H256::repeat_byte(0x22),
        };
        assert!(
            reanchor_tx.try_send(skip_request),
            "VDF receiver must be live"
        );
        poll_until(
            || signals.is_buffer_suspect(),
            "consuming a re-anchor request to re-mark the buffer suspect",
        )
        .await;
        assert_eq!(
            signals.generation(),
            1,
            "a skipped re-anchor must not apply a heal"
        );

        shutdown_token.cancel();
        handle.join().unwrap();
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

        let (ff_tx, ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);
        ff_tx
            .send(Traced::new(VdfStep {
                step: bad_ff_seed,
                global_step_number: gap_target_step,
                generation: 0,
            }))
            .await
            .unwrap();

        let vdf_state = mocked_vdf_service(&config);
        let vdf_steps_guard = VdfStateReadonly::new(vdf_state.clone());
        enable_mining(&vdf_state);

        // Canonical far ahead so the "too far ahead" guard never pauses.
        let mut mock_header = IrysBlockHeader::new_mock_header();
        mock_header.vdf_limiter_info.global_step_number = 10_000;

        let chain_sync_state = ChainSyncState::new(false, false);
        let shutdown_token = CancellationToken::new();
        let vdf_thread = std::thread::spawn({
            let config = config.clone();
            let shutdown_token = shutdown_token.clone();
            let vdf_state = vdf_state.clone();
            move || {
                run_vdf(
                    &config.vdf,
                    0,
                    initial_seed,
                    reset_seed,
                    ff_rx,
                    crate::reanchor_channel().1,
                    crate::vdf_utils::ReanchorSignals::new(),
                    MockMining,
                    vdf_state,
                    MockBlockProvider(mock_header),
                    chain_sync_state,
                    shutdown_token,
                )
            }
        });

        tokio::time::sleep(Duration::from_millis(300)).await;
        shutdown_token.cancel();
        vdf_thread.join().unwrap();

        let step_num = vdf_steps_guard.read().current_step();
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
    struct ControllableBlockProvider(std::sync::Arc<std::sync::Mutex<(VDFLimiterInfo, u64)>>);
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
    }
    impl BlockProvider for ControllableBlockProvider {
        fn canonical_vdf_snapshot(&self, _step_number: u64) -> Option<CanonicalVdfSnapshot> {
            let g = self.0.lock().unwrap();
            Some(CanonicalVdfSnapshot {
                vdf_info: g.0.clone(),
                confirmed_global_step_number: g.1,
                // Single-block mock: tip next_seed is the only seed available.
                reset_seed_for_step: g.0.next_seed,
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

        let (_tx, ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);
        let vdf_state = mocked_vdf_service(&config);
        let vdf_steps_guard = VdfStateReadonly::new(vdf_state.clone());
        enable_mining(&vdf_state);
        let chain_sync_state = ChainSyncState::new(false, false);
        let shutdown_token = CancellationToken::new();

        let handle = std::thread::spawn({
            let config = config.clone();
            let provider = provider.clone();
            let shutdown_token = shutdown_token.clone();
            let vdf_state = vdf_state.clone();
            move || {
                run_vdf(
                    &config.vdf,
                    0,
                    initial_seed,
                    initial_seed,
                    ff_rx,
                    crate::reanchor_channel().1,
                    crate::vdf_utils::ReanchorSignals::new(),
                    MockMining,
                    vdf_state,
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
        assert_eq!(vdf_steps_guard.read().current_step(), 7);

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

        let (_tx, ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);
        let vdf_state = mocked_vdf_service(config);
        let vdf_steps_guard = VdfStateReadonly::new(vdf_state.clone());
        enable_mining(&vdf_state);
        let chain_sync_state = ChainSyncState::new(false, false);
        let shutdown_token = CancellationToken::new();

        let handle = std::thread::spawn({
            let config = config.clone();
            let provider = provider.clone();
            let shutdown_token = shutdown_token.clone();
            let vdf_state = vdf_state.clone();
            move || {
                run_vdf(
                    &config.vdf,
                    0,
                    pre_boundary_seed,
                    pre_boundary_seed,
                    ff_rx,
                    crate::reanchor_channel().1,
                    crate::vdf_utils::ReanchorSignals::new(),
                    MockMining,
                    vdf_state,
                    provider,
                    chain_sync_state,
                    shutdown_token,
                )
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
            vdf_steps_guard.read().current_step(),
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
        assert_eq!(clean_guard.read().current_step(), 15);

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

        // --- The wedge: the canonical (winner) block is REJECTED by the poisoned buffer but
        //     ACCEPTED by the clean one. `vdf_steps_are_valid` is exactly what the validation
        //     service runs, so this is the real rejection a poisoned node would hit. ---
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
            "poisoned buffer must REJECT the canonical winner block — this is the #1447 wedge"
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

    mod apply_reanchor_tests {
        use super::super::{ReanchorOutcome, apply_reanchor, process_reset};
        use crate::state::{AtomicVdfState, VdfState};
        use crate::vdf_utils::ReanchorRequest;
        use crate::{apply_reset_seed, step_number_to_salt_number, vdf_sha};
        use irys_types::{Config, H256, NodeConfig, U256, VdfConfig, block_production::Seed};
        use nodit::interval::ii;
        use std::collections::VecDeque;
        use std::sync::Arc;
        use std::sync::RwLock;
        use std::sync::atomic::AtomicBool;
        use tokio_util::sync::CancellationToken;

        /// Canonical seed values for steps `[1, live]`, derived exactly as
        /// `run_vdf`: `salt(step-1)` -> `vdf_sha` -> keep the pre-reset value ->
        /// `process_reset(step, .., next_seed)` seeds the next step. Index `i` is
        /// step `i + 1`.
        fn canonical_step_values(
            config: &VdfConfig,
            initial_seed: H256,
            next_seed: H256,
            live: u64,
        ) -> Vec<H256> {
            let reset_frequency = config.reset_frequency as u64;
            let mut checkpoints = vec![H256::default(); config.num_checkpoints_in_vdf_step];
            let mut running = initial_seed;
            let mut values = Vec::with_capacity(live as usize);
            for step in 1..=live {
                let salt = U256::from(step_number_to_salt_number(config, step - 1));
                let mut step_seed = running;
                vdf_sha(
                    salt,
                    &mut step_seed,
                    config.num_checkpoints_in_vdf_step,
                    config.num_iterations_per_checkpoint(),
                    &mut checkpoints,
                );
                values.push(step_seed);
                running = process_reset(step, step_seed, reset_frequency, next_seed);
            }
            values
        }

        /// `apply_reanchor` must overwrite the poisoned buffer with canonical
        /// seeds over `[1, live]` — the request's `[1, c_step]` window plus the
        /// locally recomputed free-running tail `(c_step, live]` — while leaving
        /// the step counter forward at `live`. Cases span a tail that crosses a
        /// reset boundary (the seed-fold path that would silently re-poison on the
        /// wrong seed), a tail landing on a boundary, and no tail at all.
        #[rstest::rstest]
        #[case::tail_crosses_boundary(8, 12)]
        #[case::tail_lands_on_boundary(8, 10)]
        #[case::no_tail(8, 8)]
        fn heals_buffer_and_keeps_counter(#[case] c_step: u64, #[case] live_step: u64) {
            let mut node_config = NodeConfig::testing();
            node_config.consensus.get_mut().vdf.reset_frequency = 5;
            node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
            let config = Config::new_with_random_peer_id(node_config);
            let vdf = &config.vdf;

            let initial_seed = H256::repeat_byte(0x11);
            let next_seed = H256::repeat_byte(0x22);
            let canonical = canonical_step_values(vdf, initial_seed, next_seed, live_step);

            // Poisoned buffer: counter parked at live_step, values a distinct
            // sentinel so a failure to overwrite would be caught.
            let capacity = 1_000_usize;
            let mut state = VdfState::new(capacity, live_step, Arc::new(AtomicBool::new(false)));
            state.seeds = vec![Seed(H256::repeat_byte(0xEE)); live_step as usize].into();
            let vdf_state: AtomicVdfState = Arc::new(RwLock::new(state));

            // The request carries only the canonical window [1, c_step]; the VDF
            // thread recomputes the tail itself.
            let canonical_seeds: VecDeque<Seed> = canonical[..c_step as usize]
                .iter()
                .map(|h| Seed(*h))
                .collect();
            let request = ReanchorRequest {
                canonical_seeds,
                c_step,
                // c_step = 8 is off-boundary (rf = 5), so the tip `seed` must be
                // ignored: a distinct sentinel proves no fold consumes it.
                seed: H256::repeat_byte(0x33),
                next_seed,
            };

            let mut checkpoints = vec![H256::default(); vdf.num_checkpoints_in_vdf_step];
            let ReanchorOutcome::Healed {
                new_hash,
                new_next_seed,
            } = apply_reanchor(
                request,
                vdf,
                &vdf_state,
                live_step,
                &mut checkpoints,
                &CancellationToken::new(),
            )
            else {
                panic!("a boundary-free re-anchor must heal, not skip");
            };

            let guard = vdf_state.read().unwrap();
            assert_eq!(
                guard.current_step(),
                live_step,
                "the step counter must stay forward at live_step"
            );
            let stored = guard
                .get_steps(ii(1, live_step))
                .expect("buffer must cover [1, live]");
            assert_eq!(
                stored.0, canonical,
                "the healed buffer must equal canonical over [1, live]"
            );

            // The returned locals must seed step live+1 exactly as the loop would.
            assert_eq!(
                new_next_seed, next_seed,
                "must adopt the canonical reset seed"
            );
            let expected_running = process_reset(
                live_step,
                canonical[(live_step - 1) as usize],
                vdf.reset_frequency as u64,
                next_seed,
            );
            assert_eq!(
                new_hash, expected_running,
                "the running hash must be the post-reset seed feeding step live+1"
            );
        }

        /// When the canonical tip `c_step` sits exactly on a reset boundary, the
        /// fold at `c_step` (feeding `c_step + 1`) must use the request's `seed`
        /// — the tip's own `seed`, which the first post-tip block carries per
        /// `calculate_seeds` — NOT the `next_seed`. Distinct sentinel values for
        /// the two seeds pin the schedule: folding the wrong one would produce a
        /// different tail and fail the buffer equality below.
        #[test]
        fn heals_when_tip_sits_on_reset_boundary_folding_tip_seed() {
            let mut node_config = NodeConfig::testing();
            node_config.consensus.get_mut().vdf.reset_frequency = 5;
            node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
            let config = Config::new_with_random_peer_id(node_config);
            let vdf = &config.vdf;

            // c_step = 10 is a multiple of reset_frequency 5; live = 12 keeps the
            // tail short of the second post-tip boundary (20), so it must heal.
            let c_step = 10_u64;
            let live_step = 12_u64;
            let seed = H256::repeat_byte(0x44);
            let next_seed = H256::repeat_byte(0x22);

            let capacity = 1_000_usize;
            let mut state = VdfState::new(capacity, live_step, Arc::new(AtomicBool::new(false)));
            state.seeds = vec![Seed(H256::repeat_byte(0xEE)); live_step as usize].into();
            let vdf_state: AtomicVdfState = Arc::new(RwLock::new(state));

            let window: Vec<H256> = (1..=c_step).map(H256::from_low_u64_be).collect();
            let tip_seed = *window.last().expect("non-empty window");
            let request = ReanchorRequest {
                canonical_seeds: window.iter().copied().map(Seed).collect(),
                c_step,
                seed,
                next_seed,
            };

            let mut checkpoints = vec![H256::default(); vdf.num_checkpoints_in_vdf_step];
            let ReanchorOutcome::Healed {
                new_hash,
                new_next_seed,
            } = apply_reanchor(
                request,
                vdf,
                &vdf_state,
                live_step,
                &mut checkpoints,
                &CancellationToken::new(),
            )
            else {
                panic!("a boundary tip with a one-boundary tail must heal, not skip");
            };

            // Expected tail, folding the tip `seed` at the boundary c_step and
            // nothing thereafter (11 and 12 are off-boundary).
            let mut expected = window;
            let mut running = apply_reset_seed(tip_seed, seed);
            for step in (c_step + 1)..=live_step {
                let salt = U256::from(step_number_to_salt_number(vdf, step - 1));
                let mut step_seed = running;
                vdf_sha(
                    salt,
                    &mut step_seed,
                    vdf.num_checkpoints_in_vdf_step,
                    vdf.num_iterations_per_checkpoint(),
                    &mut checkpoints,
                );
                expected.push(step_seed);
                running = process_reset(step, step_seed, vdf.reset_frequency as u64, next_seed);
            }

            let guard = vdf_state.read().unwrap();
            assert_eq!(guard.current_step(), live_step, "counter stays forward");
            let stored = guard
                .get_steps(ii(1, live_step))
                .expect("buffer must cover [1, live]");
            assert_eq!(
                stored.0, expected,
                "the healed tail must fold the tip `seed` at the boundary c_step"
            );
            assert_eq!(new_hash, running, "running hash must seed step live+1");
            assert_eq!(new_next_seed, next_seed);
        }

        /// A tail that reaches the SECOND reset boundary past the canonical tip
        /// must skip: that boundary's rotation block lies strictly past the tip,
        /// so no canonical block has pinned its fold seed yet. Covers both an
        /// off-boundary and an on-boundary tip. The buffer stays untouched (the
        /// gate re-sends with a fresher tip on the next canonical advance).
        #[rstest::rstest]
        #[case::off_boundary_tip(8, 15)]
        #[case::on_boundary_tip(10, 20)]
        fn skips_when_tail_would_cross_second_boundary(
            #[case] c_step: u64,
            #[case] live_step: u64,
        ) {
            let mut node_config = NodeConfig::testing();
            node_config.consensus.get_mut().vdf.reset_frequency = 5;
            node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
            let config = Config::new_with_random_peer_id(node_config);
            let vdf = &config.vdf;

            let capacity = 1_000_usize;
            let poison = Seed(H256::repeat_byte(0xEE));
            let mut state = VdfState::new(capacity, live_step, Arc::new(AtomicBool::new(false)));
            state.seeds = vec![poison.clone(); live_step as usize].into();
            let vdf_state: AtomicVdfState = Arc::new(RwLock::new(state));

            let request = ReanchorRequest {
                canonical_seeds: (1..=c_step)
                    .map(|i| Seed(H256::from_low_u64_be(i)))
                    .collect(),
                c_step,
                seed: H256::repeat_byte(0x44),
                next_seed: H256::repeat_byte(0x22),
            };

            let mut checkpoints = vec![H256::default(); vdf.num_checkpoints_in_vdf_step];
            let outcome = apply_reanchor(
                request,
                vdf,
                &vdf_state,
                live_step,
                &mut checkpoints,
                &CancellationToken::new(),
            );
            assert!(
                matches!(outcome, ReanchorOutcome::Skipped),
                "a tail crossing a second, unpinned boundary must skip the re-anchor"
            );

            let guard = vdf_state.read().unwrap();
            assert!(
                guard.seeds.iter().all(|s| *s == poison),
                "the buffer must be untouched on a skip"
            );
        }

        /// The sequential tail recompute (~1 s/step in production) honours the
        /// shutdown token: a cancellation aborts to `Skipped` BEFORE the buffer
        /// write, so shutdown stays prompt and the state is untouched — the
        /// suspect flag remains set for the gate's retry if the node survives.
        #[test]
        fn shutdown_mid_recompute_skips_and_leaves_buffer_untouched() {
            let mut node_config = NodeConfig::testing();
            node_config.consensus.get_mut().vdf.reset_frequency = 5;
            node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
            let config = Config::new_with_random_peer_id(node_config);
            let vdf = &config.vdf;

            let (c_step, live_step) = (8_u64, 9_u64); // one tail step to recompute
            let capacity = 1_000_usize;
            let poison = Seed(H256::repeat_byte(0xEE));
            let mut state = VdfState::new(capacity, live_step, Arc::new(AtomicBool::new(false)));
            state.seeds = vec![poison.clone(); live_step as usize].into();
            let vdf_state: AtomicVdfState = Arc::new(RwLock::new(state));

            let request = ReanchorRequest {
                canonical_seeds: (1..=c_step)
                    .map(|i| Seed(H256::from_low_u64_be(i)))
                    .collect(),
                c_step,
                seed: H256::repeat_byte(0x44),
                next_seed: H256::repeat_byte(0x22),
            };

            let cancelled = CancellationToken::new();
            cancelled.cancel();
            let mut checkpoints = vec![H256::default(); vdf.num_checkpoints_in_vdf_step];
            let outcome = apply_reanchor(
                request,
                vdf,
                &vdf_state,
                live_step,
                &mut checkpoints,
                &cancelled,
            );
            assert!(
                matches!(outcome, ReanchorOutcome::Skipped),
                "a shutdown mid-recompute must skip the re-anchor"
            );

            let guard = vdf_state.read().unwrap();
            assert_eq!(
                guard.current_step(),
                live_step,
                "the step counter must be untouched on a shutdown abort"
            );
            assert!(
                guard.seeds.iter().all(|s| *s == poison),
                "the buffer must be untouched on a shutdown abort"
            );
        }
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
