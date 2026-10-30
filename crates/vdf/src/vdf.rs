use crate::metrics;
use crate::state::{AtomicVdfState, VdfState};
use crate::vdf_utils::{
    ReanchorReceiver, ReanchorRequest, ReanchorSignals, canonical_reset_seed, tip_child_reset_seed,
};
use crate::{MiningBroadcaster, VdfStep, apply_reset_seed, step_number_to_salt_number, vdf_sha};
use irys_domain::chain_sync_state::ChainSyncState;
use irys_types::block_provider::{BlockProvider, CanonicalVdfSnapshot};
use irys_types::{H256, H256List, IrysBlockHeader, Traced, U256, block_production::Seed};
use nodit::interval::ii;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

const PAUSED_VDF_LOOP_SLEEP: Duration = Duration::from_millis(200);
const PAUSED_SYNC_FAST_FORWARD_SLEEP: Duration = Duration::from_millis(10);
/// Minimum gap between repeated "VDF paused" lines. The parked loop wakes every
/// 10-200ms, so an unthrottled line is 5-100 per second — and a non-mining node
/// whose free-run the startup benchmark disabled stays parked for its whole
/// lifetime. Not reset when the loop resumes: a node flipping in and out of the
/// pause branch must not restore the spam. A genuinely new pause episode still
/// logs at once, because by then the interval has elapsed.
const PAUSED_LOG_INTERVAL: Duration = Duration::from_secs(30);
/// Wall-clock floor for one locally computed step. The hash count stays the
/// consensus difficulty. A core that finishes early waits out the rest of this
/// interval, so local steps are at most one per second. Fast-forward steps do
/// not use this floor.
const MIN_LOCAL_STEP_DURATION: Duration = Duration::from_secs(1);
/// Floor used when `throttle` is set. Those configs run a tiny hash count and
/// still need more than one step per second.
const THROTTLED_STEP_DURATION: Duration = Duration::from_millis(25);

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
    // Fallback reset seed for a boundary the latest snapshot cannot decide (below the
    // confirmed step, or before the first snapshot read).
    let mut next_reset_seed = initial_reset_seed;
    // Latest canonical snapshot. It decides the reset seed of each boundary the loop folds
    // (`canonical_reset_seed`), and the loop checks the seeds it already folded above the
    // confirmed step against it. Free-run never waits for a block: a boundary above the tip
    // folds the seed a child of the tip would carry, and a later block or reorg that
    // disagrees costs a recompute of the steps after that boundary, never a halt.
    // See `design/docs/vdf-reset-seed-fork-handling.md`.
    let mut canonical: Option<CanonicalVdfSnapshot> = None;
    // Extract the canonical step and the shared mining-enable flag under a single read
    // guard. The flag is the SSOT `Arc<AtomicBool>` owned by `VdfState`; cloning the `Arc`
    // once here lets the loop gate on it lock-free thereafter.
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

    let mut hash: H256 = current_vdf_hash;
    let mut checkpoints: Vec<H256> = vec![H256::default(); config.num_checkpoints_in_vdf_step];
    let mut global_step_number = global_step_number;
    // FIXME: The reset seed is the same as the seed... which I suspect is incorrect!
    info!(
        "VDF thread started at global_step_number: {}",
        global_step_number
    );
    let vdf_reset_frequency = config.reset_frequency as u64;
    let mut last_paused_log: Option<Instant> = None;
    // Highest fast-forward step refused for not continuing the buffered lineage. The
    // loop computes up to it on its own lineage even while mining is paused: a node that
    // only advances by fast-forward would otherwise stop, and the validation waiting for
    // that step would read the frozen counter as a dead VDF thread.
    let mut catch_up_to_step: u64 = 0;

    loop {
        if shutdown_token.is_cancelled() {
            tracing::info!("VDF loop: shutdown token cancelled, exiting");
            // Before the fast-forward receiver drops, so validation still
            // draining can tell this exit from a dead VDF thread.
            reanchor_signals.record_shutdown_stop();
            break;
        }

        // Latest-value re-anchor: rewrite buffer values, leave counter forward.
        if reanchor_receiver.has_changed().unwrap_or(false)
            && let Some(request) = reanchor_receiver.borrow_and_update().clone()
        {
            // Re-mark suspect: a prior heal may have cleared the flag mid-queue.
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
                    // Bump generation + clear suspect; stale-stamped FF steps drop.
                    reanchor_signals.record_heal_applied();
                    hash = new_hash;
                    next_reset_seed = new_next_seed;
                    info!(
                        vdf.global_step_number = global_step_number,
                        "VDF buffer re-anchored to canonical after deep reorg"
                    );
                    // Notify miners: step numbers unchanged but seeds were healed.
                    broadcast_mining_service.broadcast_reanchored();
                }
                // Buffer untouched; the suspect flag stays set, so the
                // block-tree gate re-sends a fresh request from the next
                // canonical tip (no broadcast — miners keep their rotation).
                // Free-run remains paused while suspect.
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
            // Drop steps validated against a pre-heal buffer.
            if proposed_ff_step.generation < reanchor_signals.generation() {
                debug!(
                    vdf.ff_step = proposed_ff_step.global_step_number,
                    vdf.step_generation = proposed_ff_step.generation,
                    "Dropping stale fast-forward step from before a re-anchor"
                );
                continue;
            }
            // A step past the next one is a gap: the steps before it were refused below,
            // or have not arrived yet. Either way its validation waits for the counter to
            // get there, so compute up to it locally rather than wait for a fast-forward
            // step that may never come.
            if proposed_ff_step.global_step_number > global_step_number + 1 {
                debug!(
                    vdf.ff_step = proposed_ff_step.global_step_number,
                    vdf.global_step_number = global_step_number,
                    "Fast-forward step is past the next step; computing up to it locally"
                );
                catch_up_to_step = catch_up_to_step.max(proposed_ff_step.global_step_number);
                continue;
            }
            // Store a step only if it continues the buffered lineage. A block on another
            // lineage (another fork, or one this buffer left by folding a stale reset
            // seed) must not join it: the validation fast paths rely on every buffered
            // step being the VDF of the one before it. If the buffer is the stale side,
            // the reset-seed check below rewrites it in this same iteration.
            if proposed_ff_step.global_step_number == global_step_number + 1 {
                let continues_buffer = match vdf_state.read() {
                    Ok(guard) => !matches!(
                        guard.get_steps(ii(global_step_number, global_step_number)),
                        Ok(buffered) if buffered.0[0] != proposed_ff_step.predecessor
                    ),
                    Err(_) => {
                        error!("VDF state read lock poisoned; exiting VDF thread");
                        return;
                    }
                };
                if !continues_buffer {
                    warn!(
                        vdf.ff_step = proposed_ff_step.global_step_number,
                        "Refusing fast-forward step: its predecessor is not the buffered step (another VDF lineage)"
                    );
                    metrics::record_fast_forward_lineage_refused();
                    catch_up_to_step = catch_up_to_step.max(proposed_ff_step.global_step_number);
                    continue;
                }
            }
            // if the step number is ahead of local nodes vdf steps
            if global_step_number < proposed_ff_step.global_step_number {
                debug!(
                    "Fastforward Step {:?} with Seed {:?}",
                    proposed_ff_step.global_step_number, proposed_ff_step.step
                );

                // FF replays validated steps only. Per-step reset seed, not tip next_seed.
                if let Some(snapshot) = block_provider
                    .canonical_vdf_snapshot(proposed_ff_step.global_step_number.saturating_sub(1))
                {
                    next_reset_seed = snapshot.reset_seed_for_step;
                    canonical_global_step_number = snapshot.vdf_info.global_step_number;
                    canonical = Some(snapshot);
                }

                let ff_step_number = proposed_ff_step.global_step_number;
                let fold_seed = reset_seed_to_fold(
                    canonical.as_ref(),
                    next_reset_seed,
                    ff_step_number,
                    vdf_reset_frequency,
                );
                // The block folded its own seed at the boundary before this step (it
                // computed the step), so that boundary records the block's seed even if
                // the block is not canonical: the loop's reset-seed check then finds it.
                // The loop folds the canonical seed at this step if it is a boundary.
                let reset_seeds = [
                    (ff_step_number - 1, proposed_ff_step.reset_seed),
                    (ff_step_number, fold_seed),
                ];
                let prev_step = global_step_number;
                let Some(returned) = store_step(
                    proposed_ff_step.step,
                    &vdf_state,
                    ff_step_number,
                    canonical_global_step_number,
                    &reset_seeds,
                    vdf_reset_frequency,
                ) else {
                    error!(
                        vdf.proposed_step = ff_step_number,
                        "VDF thread exiting: store_step failed during fast-forward (lock poisoned)"
                    );
                    return;
                };
                if returned == prev_step {
                    // Not stored (gaps are filtered above, so this is not expected) —
                    // leave hash and global_step_number untouched so the VDF state
                    // stays consistent.
                    warn!(
                        prev = prev_step,
                        proposed = ff_step_number,
                        "Fast-forward step was not stored"
                    );
                    continue;
                }
                global_step_number = returned;
                hash = proposed_ff_step.step;
                chain_sync_state.record_vdf_step(global_step_number);
                metrics::record_vdf_global_step(global_step_number);
                hash = process_reset(global_step_number, hash, vdf_reset_frequency, fold_seed);
            } else {
                debug!(
                    "Fastforward Step {} is not ahead of {}",
                    proposed_ff_step.global_step_number, global_step_number
                );
            }
        }

        if let Some(snapshot) = block_provider.canonical_vdf_snapshot(global_step_number) {
            next_reset_seed = snapshot.reset_seed_for_step;
            canonical_global_step_number = snapshot.vdf_info.global_step_number;
            debug!(
                "Canonical global step number: {}, next reset seed: {:?}, prev output: {:?}, confirmed step: {}, global_step: {:?}",
                canonical_global_step_number,
                next_reset_seed,
                snapshot.vdf_info.prev_output,
                snapshot.confirmed_global_step_number,
                global_step_number
            );
            canonical = Some(snapshot);
        }

        // Pause free-run while suspect; FF of validated steps still runs above.
        let buffer_suspect = reanchor_signals.is_buffer_suspect();

        // Check the reset seeds folded above the confirmed step against the canonical
        // chain. A reorg, or a block that lands after free-run guessed past the tip, can
        // change them; the steps after the first changed boundary are then rewritten in
        // place. Skipped while a block-tree re-anchor is pending: it rewrites the buffer.
        if !buffer_suspect && let Some(snapshot) = canonical.as_ref() {
            let changed = match vdf_state.read() {
                Ok(guard) => first_non_canonical_reset_seed(
                    &guard,
                    snapshot,
                    global_step_number,
                    vdf_reset_frequency,
                )
                .is_some(),
                Err(_) => {
                    error!("VDF state read lock poisoned; exiting VDF thread");
                    return;
                }
            };
            if changed {
                // Re-read the chain with the canonical blocks' steps, so the rewrite can
                // copy what they hold and recompute only free-run steps past the tip. The
                // boundary is decided again on this view: the chain may have moved.
                let view = block_provider
                    .canonical_vdf_snapshot_with_steps(global_step_number)
                    .unwrap_or_else(|| snapshot.clone());
                let boundary = match vdf_state.read() {
                    Ok(guard) => first_non_canonical_reset_seed(
                        &guard,
                        &view,
                        global_step_number,
                        vdf_reset_frequency,
                    ),
                    Err(_) => {
                        error!("VDF state read lock poisoned; exiting VDF thread");
                        return;
                    }
                };
                next_reset_seed = view.reset_seed_for_step;
                canonical_global_step_number = view.vdf_info.global_step_number;
                canonical = Some(view);
                if let (Some(boundary), Some(view)) = (boundary, canonical.as_ref()) {
                    // The counter stops for the whole rewrite; validation's waits must
                    // not read that as a dead VDF thread.
                    reanchor_signals.begin_recompute();
                    match rewrite_after_reset_seed_change(
                        boundary,
                        view,
                        next_reset_seed,
                        config,
                        &vdf_state,
                        global_step_number,
                        &mut checkpoints,
                        &shutdown_token,
                    ) {
                        ReseedOutcome::Rewritten {
                            new_hash,
                            rewrote_steps,
                        } => {
                            hash = new_hash;
                            // Same contract as a block-tree re-anchor: buffered values
                            // changed under unchanged step numbers.
                            reanchor_signals.finish_recompute(rewrote_steps);
                            if rewrote_steps {
                                broadcast_mining_service.broadcast_reanchored();
                            }
                            continue;
                        }
                        ReseedOutcome::LineageMismatch => {
                            // The buffer left the canonical lineage below the boundary,
                            // where this check cannot see. Hand it to the block-tree
                            // re-anchor, which rewrites the buffer from the canonical
                            // blocks; the suspect flag makes it send a request.
                            reanchor_signals.finish_recompute(false);
                            reanchor_signals.mark_buffer_suspect();
                            continue;
                        }
                        // Fall through: on shutdown the next iteration exits; otherwise
                        // re-detecting at once would spin with the counter frozen.
                        ReseedOutcome::Aborted => reanchor_signals.finish_recompute(false),
                        ReseedOutcome::PoisonedLock => {
                            reanchor_signals.finish_recompute(false);
                            return;
                        }
                    }
                }
            }
        }

        let catching_up = global_step_number < catch_up_to_step;
        if (!is_mining_enabled.load(std::sync::atomic::Ordering::Relaxed) && !catching_up)
            || buffer_suspect
        {
            if buffer_suspect {
                debug!(
                    vdf.global_step_number = global_step_number,
                    "VDF free-run paused: seed buffer suspect pending re-anchor heal"
                );
            }
            // During sync we pause local VDF mining, but trusted-peer catch-up
            // still depends on this loop consuming fast-forward steps promptly.
            // A 200ms sleep here limits that path to ~5 wakeups/sec.
            let pause_duration = if !is_mining_enabled.load(std::sync::atomic::Ordering::Relaxed)
                && chain_sync_state.is_syncing()
            {
                PAUSED_SYNC_FAST_FORWARD_SLEEP
            } else {
                PAUSED_VDF_LOOP_SLEEP
            };
            let pause_now = Instant::now();
            if last_paused_log
                .is_none_or(|last| pause_now.duration_since(last) >= PAUSED_LOG_INTERVAL)
            {
                debug!(
                    vdf.global_step_number = global_step_number,
                    vdf.canonical_global_step_number = canonical_global_step_number,
                    "VDF free-run paused; draining fast-forward steps every {pause_duration:?}"
                );
                last_paused_log = Some(pause_now);
            }
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

        // Pad a fast core out to the floor. The hash count is unchanged.
        let min_step = if config.throttle {
            THROTTLED_STEP_DURATION
        } else {
            MIN_LOCAL_STEP_DURATION
        };
        if let Some(rest) = min_step.checked_sub(elapsed) {
            std::thread::sleep(rest);
        }

        let new_step_number = global_step_number + 1;
        let fold_seed = reset_seed_to_fold(
            canonical.as_ref(),
            next_reset_seed,
            new_step_number,
            vdf_reset_frequency,
        );
        let Some(returned) = store_step(
            hash,
            &vdf_state,
            new_step_number,
            canonical_global_step_number,
            &[(new_step_number, fold_seed)],
            vdf_reset_frequency,
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

        hash = process_reset(global_step_number, hash, vdf_reset_frequency, fold_seed);
    }
    debug!(vdf.global_step_number = ?global_step_number, "VDF thread stopped");
}

/// The reset seed to fold at `boundary` when the loop's live step is `boundary`: the
/// canonical one from `canonical`, else `fallback` (the snapshot's per-step seed).
fn reset_seed_to_fold(
    canonical: Option<&CanonicalVdfSnapshot>,
    fallback: H256,
    boundary: u64,
    reset_frequency: u64,
) -> H256 {
    canonical
        .and_then(|snapshot| canonical_reset_seed(snapshot, boundary, boundary, reset_frequency))
        .unwrap_or(fallback)
}

/// The lowest reset boundary from the confirmed step up to `live_step` whose recorded seed
/// is missing or differs from the canonical seed in `snapshot`. Only boundaries the buffer
/// still holds are checked, since a recompute starts from the buffered step at the boundary.
fn first_non_canonical_reset_seed(
    state: &VdfState,
    snapshot: &CanonicalVdfSnapshot,
    live_step: u64,
    reset_frequency: u64,
) -> Option<u64> {
    let lowest = snapshot
        .confirmed_global_step_number
        .max(state.first_buffered_step())
        .max(1);
    let first_boundary = lowest
        .div_ceil(reset_frequency)
        .saturating_mul(reset_frequency);
    (first_boundary..=live_step)
        .step_by(usize::try_from(reset_frequency).unwrap_or(usize::MAX))
        .find(|&boundary| {
            canonical_reset_seed(snapshot, boundary, live_step, reset_frequency)
                .is_some_and(|expected| state.reset_seed_at(boundary) != Some(expected))
        })
}

/// Outcome of [`rewrite_after_reset_seed_change`].
enum ReseedOutcome {
    /// The tail after the boundary now follows the canonical chain. Carries the loop's new
    /// running `hash`; `rewrote_steps` is false when only the fold at the live step changed.
    Rewritten { new_hash: H256, rewrote_steps: bool },
    /// Shutdown interrupted the recompute, or the boundary step was not buffered;
    /// the buffer is untouched.
    Aborted,
    /// The canonical blocks hold the step at the boundary and it is not the buffered
    /// one: the buffer diverged below the boundary. The buffer is untouched.
    LineageMismatch,
    /// The state lock was poisoned; the caller must exit the VDF thread.
    PoisonedLock,
}

/// Rewrite the buffered steps after `boundary` to follow the canonical chain in `view`,
/// in place. Steps the canonical blocks hold are copied from them; only steps past the
/// canonical tip (free-run steps no block holds yet) are recomputed, from the last copied
/// step or the buffered step at `boundary`.
#[expect(
    clippy::too_many_arguments,
    reason = "loop state passed through; grouping it would only rename the fields"
)]
fn rewrite_after_reset_seed_change(
    boundary: u64,
    view: &CanonicalVdfSnapshot,
    fallback_seed: H256,
    config: &irys_types::VdfConfig,
    vdf_state: &AtomicVdfState,
    live_step: u64,
    checkpoints: &mut [H256],
    shutdown_token: &CancellationToken,
) -> ReseedOutcome {
    let reset_frequency = config.reset_frequency as u64;
    let (buffered_at_boundary, recorded) = match vdf_state.read() {
        Ok(guard) => match guard.get_steps(ii(boundary, boundary)) {
            Ok(steps) => (steps.0[0], guard.reset_seed_at(boundary)),
            // `first_non_canonical_reset_seed` only returns buffered boundaries, and only
            // this thread moves the buffer, so this is not expected.
            Err(err) => {
                warn!(
                    vdf.boundary = boundary,
                    "VDF reset-seed rewrite skipped: boundary step not buffered: {err}"
                );
                return ReseedOutcome::Aborted;
            }
        },
        Err(_) => {
            error!("VDF state read lock poisoned; exiting VDF thread");
            return ReseedOutcome::PoisonedLock;
        }
    };
    let seed_for = |step: u64| {
        canonical_reset_seed(view, step, live_step, reset_frequency).unwrap_or(fallback_seed)
    };

    // A canonical step at `boundary` that differs from the buffered one means the buffer
    // diverged below the boundary; recomputing from it would only extend the wrong lineage.
    if let Some(canonical_at_boundary) =
        canonical_block_steps(view, boundary, boundary).and_then(|steps| steps.first().copied())
        && canonical_at_boundary != buffered_at_boundary
    {
        error!(
            vdf.boundary = boundary,
            "VDF buffer is off the canonical lineage below a changed reset boundary; requesting a re-anchor"
        );
        return ReseedOutcome::LineageMismatch;
    }

    // Copy `(boundary, min(tip, live)]` from the canonical blocks when they hold all of
    // it (their step at `boundary` is the buffered one, checked above).
    let copy_end = live_step.min(view.vdf_info.global_step_number);
    let copied = (boundary < copy_end)
        .then(|| canonical_block_steps(view, boundary, copy_end))
        .flatten();
    let (mut steps, mut reset_seeds, from_step, from_hash) = match copied {
        Some(block_steps) => {
            let reset_seeds: Vec<(u64, H256)> = (boundary..copy_end)
                .filter(|step| step.is_multiple_of(reset_frequency))
                .map(|step| (step, seed_for(step)))
                .collect();
            let from_hash = *block_steps.last().expect("covers boundary..=copy_end");
            (block_steps[1..].to_vec(), reset_seeds, copy_end, from_hash)
        }
        None => (Vec::new(), Vec::new(), boundary, buffered_at_boundary),
    };
    warn!(
        vdf.boundary = boundary,
        vdf.global_step_number = live_step,
        vdf.recorded_reset_seed = ?recorded,
        vdf.canonical_reset_seed = ?seed_for(boundary),
        vdf.copied_steps = steps.len(),
        vdf.recomputed_steps = live_step - from_step,
        "VDF folded a reset seed the canonical chain no longer has; rewriting the steps after the boundary"
    );
    let Some(tail) = recompute_tail(
        config,
        from_hash,
        from_step,
        live_step,
        seed_for,
        checkpoints,
        shutdown_token,
    ) else {
        return ReseedOutcome::Aborted;
    };
    steps.extend(tail.steps);
    reset_seeds.extend(tail.reset_seeds);
    let rewrote_steps = !steps.is_empty();
    match vdf_state.write() {
        Ok(mut guard) => guard.rewrite_tail(boundary + 1, steps, reset_seeds),
        Err(_) => {
            error!("VDF state write lock poisoned; exiting VDF thread");
            return ReseedOutcome::PoisonedLock;
        }
    }
    metrics::record_reset_seed_recompute();
    ReseedOutcome::Rewritten {
        new_hash: tail.running_hash,
        rewrote_steps,
    }
}

/// The steps `[first, last]` from the unconfirmed canonical blocks in `view`, or `None`
/// when they do not hold every one of them (or carry no steps).
fn canonical_block_steps(view: &CanonicalVdfSnapshot, first: u64, last: u64) -> Option<Vec<H256>> {
    let mut steps = Vec::with_capacity(usize::try_from(last - first + 1).ok()?);
    for block in &view.unconfirmed_blocks {
        if block.last_step < first || block.first_step > last {
            continue;
        }
        let block_steps = block.steps.as_ref()?;
        let from = first.max(block.first_step);
        let to = last.min(block.last_step);
        if from != first + steps.len() as u64 {
            return None;
        }
        let offset = usize::try_from(from - block.first_step).ok()?;
        let count = usize::try_from(to - from + 1).ok()?;
        steps.extend_from_slice(block_steps.0.get(offset..offset + count)?);
    }
    (steps.len() as u64 == last - first + 1).then_some(steps)
}

/// Steps `(from_step, to_step]` recomputed from step `from_step`.
struct RecomputedTail {
    steps: Vec<H256>,
    /// Reset seed folded at each boundary in `[from_step, to_step]`.
    reset_seeds: Vec<(u64, H256)>,
    /// Step `to_step` with its own reset fold applied: the hash that seeds `to_step + 1`.
    running_hash: H256,
}

/// Recompute the steps after `from_step` up to `to_step` exactly as the main loop does
/// (`process_reset` → salt → `vdf_sha`), folding `seed_for(b)` at each boundary `b` in
/// `[from_step, to_step]`. `None` when shutdown interrupts it.
fn recompute_tail(
    config: &irys_types::VdfConfig,
    from_hash: H256,
    from_step: u64,
    to_step: u64,
    seed_for: impl Fn(u64) -> H256,
    checkpoints: &mut [H256],
    shutdown_token: &CancellationToken,
) -> Option<RecomputedTail> {
    let reset_frequency = config.reset_frequency as u64;
    let mut reset_seeds = Vec::new();
    let mut fold = |step: u64, hash: H256| {
        if step > 0 && step.is_multiple_of(reset_frequency) {
            let seed = seed_for(step);
            reset_seeds.push((step, seed));
            process_reset(step, hash, reset_frequency, seed)
        } else {
            hash
        }
    };
    let mut running = fold(from_step, from_hash);
    let mut steps = Vec::with_capacity(usize::try_from(to_step - from_step).unwrap_or(0));
    for step in (from_step + 1)..=to_step {
        if shutdown_token.is_cancelled() {
            warn!(
                from_step,
                to_step, step, "VDF recompute aborted by shutdown; buffer left untouched"
            );
            return None;
        }
        let salt = U256::from(step_number_to_salt_number(config, step - 1));
        vdf_sha(
            salt,
            &mut running,
            config.num_checkpoints_in_vdf_step,
            config.num_iterations_per_checkpoint(),
            checkpoints,
        );
        steps.push(running);
        running = fold(step, running);
    }
    Some(RecomputedTail {
        steps,
        reset_seeds,
        running_hash: running,
    })
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
    /// canonical tip ahead of the live step, or shutdown mid-recompute).
    /// Validation's fork-local recompute keeps the node on canonical meanwhile,
    /// and the block-tree gate re-sends a fresh request from every new canonical
    /// tip while the buffer stays suspect.
    Skipped,
    /// The state write lock was poisoned; the caller must exit the VDF thread.
    PoisonedLock,
}

/// Apply a re-anchor: write canonical `[floor, c_step]`, recompute free-run
/// tail to `live_step`. Boundaries above the tip fold the seed a child of the
/// tip would carry ([`tip_child_reset_seed`]), the same rule free-run uses; if
/// a later block disagrees, the loop's reset-seed check recomputes again.
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
        mut reset_seed_by_boundary,
    } = request;
    let reset_frequency = config.reset_frequency as u64;

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

    let Some(tail) = recompute_tail(
        config,
        tip_seed,
        c_step,
        live_step,
        |boundary| {
            tip_child_reset_seed(
                c_step,
                seed,
                next_seed,
                boundary,
                live_step,
                reset_frequency,
            )
        },
        checkpoints,
        shutdown_token,
    ) else {
        return ReanchorOutcome::Skipped;
    };
    canonical_seeds.extend(tail.steps.into_iter().map(Seed));
    reset_seed_by_boundary.extend(tail.reset_seeds);

    match vdf_state.write() {
        Ok(mut guard) => guard.reanchor_seeds(canonical_seeds, reset_seed_by_boundary),
        Err(_) => {
            error!("VDF state write lock poisoned during re-anchor; exiting VDF thread");
            return ReanchorOutcome::PoisonedLock;
        }
    }

    ReanchorOutcome::Healed {
        new_hash: tail.running_hash,
        new_next_seed: next_seed,
    }
}

/// Returns `None` if the VDF state write lock is poisoned, so the VDF loop can
/// exit gracefully rather than re-panic on `expect`/`unwrap`. On `Some(step)`,
/// `step` is the new global step (which may equal `new_global_step_number - 1`
/// if a gap was detected and ignored — see `VdfState::store_step`).
///
/// When the step is stored, each `(step, seed)` in `reset_seeds` whose step is a reset
/// boundary above zero is recorded as that boundary's reset seed.
#[must_use]
fn store_step(
    hash: H256,
    vdf_state: &AtomicVdfState,
    new_global_step_number: u64,
    canonical_global_step_number: u64,
    reset_seeds: &[(u64, H256)],
    reset_frequency: u64,
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
    if global_step_number == new_global_step_number {
        for &(boundary, seed) in reset_seeds {
            if boundary > 0 && boundary.is_multiple_of(reset_frequency) {
                vdf_guard.record_reset_seed(boundary, seed);
            }
        }
    }
    Some(global_step_number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_helpers::mocked_vdf_service;
    use crate::state::{CancelEnum, VdfController, VdfStateReadonly, vdf_steps_are_valid};
    use crate::vdf_sha_verification;
    use irys_types::block_provider::CanonicalBlockVdf;
    use irys_types::*;
    use nodit::interval::ii;
    use std::collections::BTreeMap;
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
                unconfirmed_blocks: Vec::new(),
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

        init_tracing();

        let broadcast_mining_service = MockMining;
        let (_, ff_step_receiver) = mpsc::channel::<Traced<VdfStep>>(16);

        let vdf_state = mocked_vdf_service(&config);
        let vdf_steps_guard = VdfStateReadonly::new(vdf_state.clone());
        enable_mining(&vdf_state);

        let mut mock_header = IrysBlockHeader::new_mock_header();
        // Set global step number to 2 to simulate a scenario where canonical chain progresses
        mock_header.vdf_limiter_info.global_step_number = 2;
        // The loop folds the canonical tip's seeds, so a block over its steps carries them
        // (the mock tip's `seed` and `next_seed` are equal).
        let reset_seed = mock_header.vdf_limiter_info.next_seed;

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
    async fn free_run_steps_validate_against_the_buffer() {
        let mut node_config = NodeConfig::testing();
        node_config.consensus.get_mut().vdf.reset_frequency = 2;
        node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
        let config = Config::new_with_random_peer_id(node_config);

        let seed = H256::random();
        // Free-run folds the canonical tip's `next_seed` above the tip.
        let reset_seed = MockBlockProvider::new().0.vdf_limiter_info.next_seed;

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

        // Free-run crosses reset boundaries without waiting for blocks.
        vdf_steps_guard
            .wait_for_step(
                9,
                Arc::new(AtomicU8::new(CancelEnum::Continue as u8)),
                Duration::from_secs(5),
            )
            .await
            .expect("free-run must cross several reset boundaries");
        let step_num = vdf_steps_guard.read().current_step();

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
                predecessor: H256::zero(),
                global_step_number: 1,
                reset_seed: H256::zero(),
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
        let signals = crate::vdf_utils::ReanchorSignals::new();
        let thread_signals = signals.clone(); // clone: shared flags for the VDF thread

        let join_result = std::thread::spawn(move || {
            run_vdf(
                &config.vdf,
                0,
                H256::zero(),
                H256::zero(),
                ff_rx,
                crate::reanchor_channel().1,
                thread_signals,
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
        assert!(
            !signals.is_stopped_for_shutdown(),
            "a poisoned-lock exit is a dead VDF thread, not a shutdown stop"
        );
    }

    /// The token exit is the only one that records a shutdown stop, and it does
    /// so while `run_vdf` still owns the fast-forward receiver.
    #[test]
    fn run_vdf_records_shutdown_stop_on_token_exit() {
        let config = Config::new_with_random_peer_id(NodeConfig::testing());
        let vdf_state = mocked_vdf_service(&config);
        let (ff_tx, ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);
        let shutdown_token = CancellationToken::new();
        shutdown_token.cancel();
        let signals = crate::vdf_utils::ReanchorSignals::new();
        let thread_signals = signals.clone(); // clone: shared flags for the VDF thread

        std::thread::spawn(move || {
            run_vdf(
                &config.vdf,
                0,
                H256::zero(),
                H256::zero(),
                ff_rx,
                crate::reanchor_channel().1,
                thread_signals,
                MockMining,
                vdf_state,
                MockBlockProvider::new(),
                ChainSyncState::new(false, false),
                shutdown_token,
            );
        })
        .join()
        .expect("run_vdf must exit cleanly on a cancelled token");

        assert!(signals.is_stopped_for_shutdown());
        assert!(
            ff_tx.is_closed(),
            "run_vdf must have dropped the fast-forward receiver"
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
                predecessor: H256::zero(),
                global_step_number: 1,
                reset_seed: H256::zero(),
                generation: 0,
            }))
            .await
            .unwrap();
        ff_tx
            .send(Traced::new(VdfStep {
                step: fresh_seed,
                predecessor: H256::zero(),
                global_step_number: 1,
                reset_seed: H256::zero(),
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
        // No reset boundary in the buffer, so the loop's own reset-seed check has nothing
        // to recompute and only the re-anchor requests below touch the generation.
        node_config.consensus.get_mut().vdf.reset_frequency = 1_000;
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
            reset_seed_by_boundary: Default::default(),
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
            reset_seed_by_boundary: Default::default(),
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
                predecessor: H256::zero(),
                global_step_number: gap_target_step,
                reset_seed: H256::zero(),
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

    /// Canonical chain view the tests drive by hand: the tip's limiter info, the confirmed
    /// step and the unconfirmed blocks' seeds, swapped atomically so the loop never sees a
    /// half-applied fork switch.
    #[derive(Clone)]
    struct ControllableBlockProvider(std::sync::Arc<std::sync::Mutex<CanonicalVdfSnapshot>>);

    /// One canonical block for [`ControllableBlockProvider`]: step range, seed, next seed.
    #[derive(Clone, Copy)]
    struct TestBlock {
        first_step: u64,
        last_step: u64,
        seed: H256,
        next_seed: H256,
    }

    impl ControllableBlockProvider {
        fn new(chain: &[TestBlock], confirmed_global_step_number: u64) -> Self {
            Self(std::sync::Arc::new(std::sync::Mutex::new(Self::snapshot(
                chain,
                confirmed_global_step_number,
                &BTreeMap::new(),
            ))))
        }

        /// Replace the canonical chain (a reorg or a new tip). `chain` holds the blocks above
        /// the confirmed step, oldest first; the last one is the tip.
        fn set_chain(&self, chain: &[TestBlock], confirmed_global_step_number: u64) {
            self.set_chain_with_steps(chain, confirmed_global_step_number, &BTreeMap::new());
        }

        /// [`Self::set_chain`], where a block whose every step is in `steps` also carries
        /// its steps in [`BlockProvider::canonical_vdf_snapshot_with_steps`].
        fn set_chain_with_steps(
            &self,
            chain: &[TestBlock],
            confirmed_global_step_number: u64,
            steps: &BTreeMap<u64, H256>,
        ) {
            *self.0.lock().unwrap() = Self::snapshot(chain, confirmed_global_step_number, steps);
        }

        fn snapshot(
            chain: &[TestBlock],
            confirmed_global_step_number: u64,
            steps: &BTreeMap<u64, H256>,
        ) -> CanonicalVdfSnapshot {
            let tip = chain.last().expect("chain needs a tip");
            let vdf_info = VDFLimiterInfo {
                global_step_number: tip.last_step,
                seed: tip.seed,
                next_seed: tip.next_seed,
                ..VDFLimiterInfo::default()
            };
            CanonicalVdfSnapshot {
                vdf_info,
                confirmed_global_step_number,
                reset_seed_for_step: tip.next_seed,
                unconfirmed_blocks: chain
                    .iter()
                    .map(|block| CanonicalBlockVdf {
                        first_step: block.first_step,
                        last_step: block.last_step,
                        seed: block.seed,
                        steps: (block.first_step..=block.last_step)
                            .map(|step| steps.get(&step).copied())
                            .collect::<Option<Vec<H256>>>()
                            .map(H256List),
                    })
                    .collect(),
            }
        }
    }

    impl BlockProvider for ControllableBlockProvider {
        fn canonical_vdf_snapshot(&self, _step_number: u64) -> Option<CanonicalVdfSnapshot> {
            let mut snapshot = self.0.lock().unwrap().clone();
            for block in &mut snapshot.unconfirmed_blocks {
                block.steps = None;
            }
            Some(snapshot)
        }

        fn canonical_vdf_snapshot_with_steps(
            &self,
            _step_number: u64,
        ) -> Option<CanonicalVdfSnapshot> {
            Some(self.0.lock().unwrap().clone())
        }
    }

    /// A running `run_vdf` loop driven by a [`ControllableBlockProvider`].
    struct LoopUnderTest {
        provider: ControllableBlockProvider,
        guard: VdfStateReadonly,
        signals: crate::vdf_utils::ReanchorSignals,
        shutdown: CancellationToken,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl LoopUnderTest {
        fn start(config: &Config, chain: &[TestBlock], confirmed_global_step_number: u64) -> Self {
            let provider = ControllableBlockProvider::new(chain, confirmed_global_step_number);
            let (_tx, ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);
            let vdf_state = mocked_vdf_service(config);
            let guard = VdfStateReadonly::new(vdf_state.clone());
            enable_mining(&vdf_state);
            let signals = crate::vdf_utils::ReanchorSignals::new();
            let shutdown = CancellationToken::new();
            let initial_seed = H256::repeat_byte(0xAA);
            let handle = std::thread::spawn({
                let config = config.clone();
                let provider = provider.clone();
                let shutdown = shutdown.clone();
                let signals = signals.clone();
                move || {
                    // Keep the FF sender alive for the loop's lifetime.
                    let _tx = _tx;
                    run_vdf(
                        &config.vdf,
                        0,
                        initial_seed,
                        initial_seed,
                        ff_rx,
                        crate::reanchor_channel().1,
                        signals,
                        MockMining,
                        vdf_state,
                        provider,
                        ChainSyncState::new(false, false),
                        shutdown,
                    )
                }
            });
            Self {
                provider,
                guard,
                signals,
                shutdown,
                handle: Some(handle),
            }
        }

        async fn wait_for_step(&self, step: u64) {
            let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));
            self.guard
                .wait_for_step(step, cancel, Duration::from_secs(5))
                .await
                .unwrap_or_else(|err| panic!("loop must reach step {step}: {err}"));
        }

        /// Wait until the recorded reset seed of `boundary` is `seed`.
        async fn wait_for_reset_seed(&self, boundary: u64, seed: H256) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.guard.read().reset_seed_at(boundary) != Some(seed) {
                assert!(
                    Instant::now() < deadline,
                    "reset seed of boundary {boundary} never became {seed:?}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        fn steps(&self, from: u64, to: u64) -> H256List {
            self.guard
                .get_steps(ii(from, to))
                .expect("range must be buffered")
        }

        fn stop(mut self) -> VdfStateReadonly {
            self.shutdown.cancel();
            self.handle.take().unwrap().join().unwrap();
            self.guard.clone()
        }
    }

    fn fork_test_config(reset_frequency: usize) -> Config {
        let mut node_config = NodeConfig::testing();
        node_config.consensus.get_mut().vdf.reset_frequency = reset_frequency;
        node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
        Config::new_with_random_peer_id(node_config)
    }

    /// The block validation would see for buffered steps `[first, last]` of `guard`, with
    /// `seed` as the block's reset seed.
    fn block_from_buffer(
        guard: &VdfStateReadonly,
        first: u64,
        last: u64,
        seed: H256,
    ) -> VDFLimiterInfo {
        let prev_output = guard.get_step(first - 1).expect("predecessor buffered");
        let steps = guard.get_steps(ii(first, last)).expect("steps buffered");
        VDFLimiterInfo {
            global_step_number: last,
            output: *steps.0.last().unwrap(),
            prev_output,
            steps,
            seed,
            ..VDFLimiterInfo::default()
        }
    }

    /// Slow chain: the canonical tip does not move for many reset windows. The loop keeps
    /// stepping across every boundary on the seed a child of the tip would carry — it never
    /// waits for a block.
    #[tokio::test]
    async fn free_run_keeps_stepping_while_tip_is_stalled() {
        let config = fork_test_config(4);
        let tip = TestBlock {
            first_step: 1,
            last_step: 1,
            seed: H256::repeat_byte(0x01),
            next_seed: H256::repeat_byte(0x02),
        };
        let vdf = LoopUnderTest::start(&config, &[tip], 1);

        // Five reset windows past the tip, with no block arriving.
        vdf.wait_for_step(21).await;
        let guard = vdf.stop();
        for boundary in [4, 8, 12, 16, 20] {
            assert_eq!(
                guard.read().reset_seed_at(boundary),
                Some(tip.next_seed),
                "boundary {boundary} above the tip folds the tip's next_seed"
            );
        }
    }

    /// Free-run guessed past the tip, then a late block lands that ends between the first
    /// and the second boundary after the old tip. The second boundary's seed is now that
    /// block's `next_seed`, not the old tip's: the loop recomputes the steps after it and
    /// ends up with the buffer a node that saw the block in time holds.
    #[tokio::test]
    async fn late_block_after_free_run_guess_recomputes_the_tail() {
        let config = fork_test_config(4);
        let old_tip = TestBlock {
            first_step: 1,
            last_step: 2,
            seed: H256::repeat_byte(0x01),
            next_seed: H256::repeat_byte(0x02),
        };
        // Contains boundary 4: seed = old tip's next_seed, next_seed = old tip's hash.
        let late_block = TestBlock {
            first_step: 3,
            last_step: 5,
            seed: old_tip.next_seed,
            next_seed: H256::repeat_byte(0x03),
        };

        let guessing = LoopUnderTest::start(&config, &[old_tip], 1);
        guessing.wait_for_step(10).await;
        assert_eq!(
            guessing.guard.read().reset_seed_at(8),
            Some(old_tip.next_seed)
        );
        guessing.provider.set_chain(&[old_tip, late_block], 1);
        guessing.wait_for_reset_seed(8, late_block.next_seed).await;
        guessing.wait_for_step(12).await;

        let reference = LoopUnderTest::start(&config, &[old_tip, late_block], 1);
        reference.wait_for_step(12).await;

        assert_eq!(guessing.steps(1, 12), reference.steps(1, 12));
        assert!(
            guessing.signals.generation() >= 1,
            "rewriting steps bumps the generation"
        );
        guessing.stop();
        reference.stop();
    }

    /// Fork-loser seed without a confirmation gate. The loop crosses boundary 16 on a fork-loser
    /// seed (the rotation block is at the tip, unconfirmed). The fork resolves within the
    /// migration depth to the winner. The loop sees its folded seed is no longer canonical,
    /// recomputes after boundary 16, and then holds exactly the buffer of a node that only
    /// ever saw the winner — so it accepts the winner's next block through the buffer fast
    /// path, and still rejects a block built from its old, loser-seed steps.
    #[tokio::test]
    async fn crosses_on_fork_loser_seed_then_recomputes_when_fork_resolves() {
        let config = fork_test_config(8);
        let confirmed_step = 6;
        // Two competing tips that contain boundary 8, above the confirmed step. Their
        // `next_seed` pins boundary 16.
        let loser_tip = TestBlock {
            first_step: 7,
            last_step: 9,
            seed: H256::repeat_byte(0x0A),
            next_seed: H256::repeat_byte(0x10),
        };
        let winner_tip = TestBlock {
            next_seed: H256::repeat_byte(0x20),
            ..loser_tip
        };

        let node = LoopUnderTest::start(&config, &[loser_tip], confirmed_step);
        node.wait_for_step(20).await;
        assert_eq!(
            node.guard.read().reset_seed_at(16),
            Some(loser_tip.next_seed)
        );
        let loser_block = block_from_buffer(&node.guard, 17, 20, winner_tip.next_seed);

        node.provider.set_chain(&[winner_tip], confirmed_step);
        node.wait_for_reset_seed(16, winner_tip.next_seed).await;
        node.wait_for_step(24).await;

        let clean = LoopUnderTest::start(&config, &[winner_tip], confirmed_step);
        clean.wait_for_step(24).await;
        assert_eq!(node.steps(1, 24), clean.steps(1, 24));

        let node_guard = node.stop();
        clean.stop();

        let winner_block = block_from_buffer(&node_guard, 17, 20, winner_tip.next_seed);
        assert!(
            node_guard
                .read()
                .holds_block_steps(
                    winner_block.prev_output,
                    17,
                    &winner_block.steps.0,
                    winner_block.seed,
                    8
                )
                .unwrap(),
            "the recomputed buffer holds the winner's lineage, so the fast path applies"
        );
        let pool = crate::build_verification_pool(&config.vdf);
        let cancel = || Arc::new(AtomicU8::new(CancelEnum::Continue as u8));
        vdf_steps_are_valid(&pool, &winner_block, &config.vdf, &node_guard, cancel())
            .expect("the winner's block must validate");
        assert!(
            vdf_steps_are_valid(&pool, &loser_block, &config.vdf, &node_guard, cancel()).is_err(),
            "steps folded with the loser seed are invalid under the winner's seed"
        );
    }

    /// A two-block fork within the migration depth replaces the block whose hash pins the
    /// seed of boundary 16, after the loop already crossed 16. Boundary 8 keeps its seed
    /// (both forks inherit it); boundary 16 changes. The loop recomputes after 16 only.
    #[tokio::test]
    async fn shallow_fork_at_rotation_block_after_loop_crossed_the_boundary() {
        let config = fork_test_config(8);
        let confirmed_step = 4;
        let inherited_seed = H256::repeat_byte(0x08);
        let old_parent = TestBlock {
            first_step: 5,
            last_step: 7,
            seed: H256::repeat_byte(0x04),
            next_seed: inherited_seed,
        };
        // Contains boundary 8: seed = parent's next_seed, next_seed = parent's hash.
        let old_rotation = TestBlock {
            first_step: 8,
            last_step: 9,
            seed: inherited_seed,
            next_seed: H256::repeat_byte(0x0B),
        };
        let new_parent = TestBlock {
            last_step: 6,
            ..old_parent
        };
        let new_rotation = TestBlock {
            first_step: 7,
            last_step: 10,
            seed: inherited_seed,
            next_seed: H256::repeat_byte(0x0C),
        };

        let node = LoopUnderTest::start(&config, &[old_parent, old_rotation], confirmed_step);
        node.wait_for_step(20).await;
        assert_eq!(
            node.guard.read().reset_seed_at(16),
            Some(old_rotation.next_seed)
        );
        let steps_before_16 = node.steps(1, 16);

        node.provider
            .set_chain(&[new_parent, new_rotation], confirmed_step);
        node.wait_for_reset_seed(16, new_rotation.next_seed).await;
        node.wait_for_step(22).await;
        assert_eq!(
            node.steps(1, 16),
            steps_before_16,
            "steps up to the changed boundary are kept"
        );
        assert_eq!(node.guard.read().reset_seed_at(8), Some(inherited_seed));

        let clean = LoopUnderTest::start(&config, &[new_parent, new_rotation], confirmed_step);
        clean.wait_for_step(22).await;
        assert_eq!(node.steps(1, 22), clean.steps(1, 22));
        node.stop();
        clean.stop();
    }

    /// A fork switch whose changed boundary (16) sits below the new canonical tip (18):
    /// the steps the canonical blocks hold (17, 18) are copied from them, and only the
    /// free-run steps past the tip are recomputed, from the last copied step. Sentinel
    /// values in the blocks prove where the rewritten steps came from.
    #[tokio::test]
    async fn fork_switch_copies_steps_canonical_blocks_hold() {
        let config = fork_test_config(8);
        let confirmed_step = 4;
        let inherited_seed = H256::repeat_byte(0x08);
        let old_parent = TestBlock {
            first_step: 5,
            last_step: 7,
            seed: H256::repeat_byte(0x04),
            next_seed: inherited_seed,
        };
        let old_rotation = TestBlock {
            first_step: 8,
            last_step: 9,
            seed: inherited_seed,
            next_seed: H256::repeat_byte(0x0B),
        };
        let node = LoopUnderTest::start(&config, &[old_parent, old_rotation], confirmed_step);
        node.wait_for_step(22).await;

        let new_parent = TestBlock {
            last_step: 6,
            ..old_parent
        };
        let new_rotation = TestBlock {
            first_step: 7,
            last_step: 10,
            seed: inherited_seed,
            next_seed: H256::repeat_byte(0x0C),
        };
        // Holds boundary 16: its seed is the new rotation block's next_seed.
        let new_tip = TestBlock {
            first_step: 11,
            last_step: 18,
            seed: new_rotation.next_seed,
            next_seed: H256::repeat_byte(0x0D),
        };
        // Up to 16 nothing changed, so the new blocks hold the node's own steps there.
        let mut block_steps: BTreeMap<u64, H256> = (5..=16)
            .map(|step| (step, node.guard.get_step(step).unwrap()))
            .collect();
        let sentinels = [H256::repeat_byte(0x51), H256::repeat_byte(0x52)];
        block_steps.insert(17, sentinels[0]);
        block_steps.insert(18, sentinels[1]);
        node.provider.set_chain_with_steps(
            &[new_parent, new_rotation, new_tip],
            confirmed_step,
            &block_steps,
        );
        node.wait_for_reset_seed(16, new_tip.seed).await;
        node.wait_for_step(24).await;

        assert_eq!(
            node.steps(17, 18).0,
            sentinels.to_vec(),
            "copied from the blocks"
        );
        let mut expected_19 = sentinels[1];
        let mut checkpoints = vec![H256::default(); config.vdf.num_checkpoints_in_vdf_step];
        vdf_sha(
            U256::from(step_number_to_salt_number(&config.vdf, 18)),
            &mut expected_19,
            config.vdf.num_checkpoints_in_vdf_step,
            config.vdf.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );
        assert_eq!(
            node.steps(19, 19).0,
            vec![expected_19],
            "recomputed from the last copied step"
        );
        assert_eq!(node.guard.read().reset_seed_at(24), Some(new_tip.next_seed));
        node.stop();
    }

    /// Fast-forward stores a validated step only when its predecessor is the buffered
    /// step; a step from another lineage is refused and the buffer stays one chain.
    /// With mining paused, the loop then computes the refused steps on its own lineage,
    /// so the counter still reaches them and validation waiting on them does not stall.
    #[tokio::test]
    async fn fast_forward_refuses_a_step_off_the_buffered_lineage() {
        let config = fork_test_config(1_000);
        let vdf_state = mocked_vdf_service(&config);
        for step in 1..=3 {
            vdf_state
                .write()
                .unwrap()
                .store_step(Seed(H256::from_low_u64_be(step)), step);
        }
        let guard = VdfStateReadonly::new(vdf_state.clone());
        let (ff_tx, ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);
        let other_lineage = VdfStep {
            step: H256::repeat_byte(0xBA),
            predecessor: H256::repeat_byte(0xBB),
            global_step_number: 4,
            reset_seed: H256::zero(),
            generation: 0,
        };
        let same_lineage = VdfStep {
            step: H256::repeat_byte(0x04),
            predecessor: H256::from_low_u64_be(3),
            ..other_lineage.clone()
        };
        let other_lineage_next = VdfStep {
            step: H256::repeat_byte(0xBC),
            predecessor: other_lineage.step,
            global_step_number: 5,
            ..other_lineage.clone()
        };
        ff_tx.send(Traced::new(other_lineage)).await.unwrap();
        ff_tx.send(Traced::new(same_lineage)).await.unwrap();
        ff_tx.send(Traced::new(other_lineage_next)).await.unwrap();

        // Mining paused + syncing, so the loop only advances via fast-forward.
        let shutdown = CancellationToken::new();
        let handle = std::thread::spawn({
            let config = config.clone();
            let shutdown = shutdown.clone();
            move || {
                run_vdf(
                    &config.vdf,
                    3,
                    H256::from_low_u64_be(3),
                    H256::zero(),
                    ff_rx,
                    crate::reanchor_channel().1,
                    crate::vdf_utils::ReanchorSignals::new(),
                    MockMining,
                    vdf_state,
                    MockBlockProvider::new(),
                    ChainSyncState::new(true, false),
                    shutdown,
                )
            }
        });

        guard
            .wait_for_step(
                4,
                Arc::new(AtomicU8::new(CancelEnum::Continue as u8)),
                Duration::from_secs(5),
            )
            .await
            .expect("the step on the buffered lineage must be stored");
        assert_eq!(
            guard.get_step(4).unwrap(),
            H256::repeat_byte(0x04),
            "the step from another lineage must not be stored"
        );

        // Step 5 continues the other lineage, not the buffer: refused, then computed
        // locally from the buffered step 4.
        guard
            .wait_for_step(
                5,
                Arc::new(AtomicU8::new(CancelEnum::Continue as u8)),
                Duration::from_secs(5),
            )
            .await
            .expect("a refused step is computed locally even with mining paused");
        let mut expected_5 = H256::repeat_byte(0x04);
        let mut checkpoints = vec![H256::default(); config.vdf.num_checkpoints_in_vdf_step];
        vdf_sha(
            U256::from(step_number_to_salt_number(&config.vdf, 4)),
            &mut expected_5,
            config.vdf.num_checkpoints_in_vdf_step,
            config.vdf.num_iterations_per_checkpoint(),
            &mut checkpoints,
        );
        assert_eq!(guard.get_step(5).unwrap(), expected_5);

        // A later batch of the refused block arrives in a later drain as a gap (step 8
        // while the buffer is at 5). The loop computes up to it too.
        ff_tx
            .send(Traced::new(VdfStep {
                step: H256::repeat_byte(0xBD),
                predecessor: H256::repeat_byte(0xBE),
                global_step_number: 8,
                reset_seed: H256::zero(),
                generation: 0,
            }))
            .await
            .unwrap();
        guard
            .wait_for_step(
                8,
                Arc::new(AtomicU8::new(CancelEnum::Continue as u8)),
                Duration::from_secs(5),
            )
            .await
            .expect("a gap step from a refused block is computed locally too");
        assert_ne!(guard.get_step(8).unwrap(), H256::repeat_byte(0xBD));
        shutdown.cancel();
        handle.join().unwrap();
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
                reset_seed_by_boundary: Default::default(),
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
                reset_seed_by_boundary: Default::default(),
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

        /// A tail that reaches the SECOND reset boundary past the canonical tip heals
        /// too: free-run never waits for a block, so the re-anchor must not either.
        /// A child of the tip that reaches `live_step` spans a reset step, so it
        /// carries the tip's `next_seed` and folds it at every boundary from `c_step`
        /// on — including `c_step` itself when the tip ends on a boundary. Covers an
        /// off-boundary and an on-boundary tip.
        #[rstest::rstest]
        #[case::off_boundary_tip(8, 15)]
        #[case::on_boundary_tip(10, 20)]
        fn heals_when_tail_crosses_several_boundaries(#[case] c_step: u64, #[case] live_step: u64) {
            let mut node_config = NodeConfig::testing();
            node_config.consensus.get_mut().vdf.reset_frequency = 5;
            node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
            let config = Config::new_with_random_peer_id(node_config);
            let vdf = &config.vdf;
            let reset_frequency = vdf.reset_frequency as u64;

            let capacity = 1_000_usize;
            let mut state = VdfState::new(capacity, live_step, Arc::new(AtomicBool::new(false)));
            state.seeds = vec![Seed(H256::repeat_byte(0xEE)); live_step as usize].into();
            let vdf_state: AtomicVdfState = Arc::new(RwLock::new(state));

            let window: Vec<H256> = (1..=c_step).map(H256::from_low_u64_be).collect();
            let next_seed = H256::repeat_byte(0x22);
            let request = ReanchorRequest {
                canonical_seeds: window.iter().copied().map(Seed).collect(),
                c_step,
                seed: H256::repeat_byte(0x44),
                next_seed,
                reset_seed_by_boundary: Default::default(),
            };

            let mut checkpoints = vec![H256::default(); vdf.num_checkpoints_in_vdf_step];
            let ReanchorOutcome::Healed { new_hash, .. } = apply_reanchor(
                request,
                vdf,
                &vdf_state,
                live_step,
                &mut checkpoints,
                &CancellationToken::new(),
            ) else {
                panic!("a tail across several boundaries must heal, not skip");
            };

            let mut expected = window;
            let mut running = process_reset(
                c_step,
                *expected.last().unwrap(),
                reset_frequency,
                next_seed,
            );
            for step in (c_step + 1)..=live_step {
                let salt = U256::from(step_number_to_salt_number(vdf, step - 1));
                vdf_sha(
                    salt,
                    &mut running,
                    vdf.num_checkpoints_in_vdf_step,
                    vdf.num_iterations_per_checkpoint(),
                    &mut checkpoints,
                );
                expected.push(running);
                running = process_reset(step, running, reset_frequency, next_seed);
            }

            let guard = vdf_state.read().unwrap();
            assert_eq!(guard.current_step(), live_step, "counter stays forward");
            assert_eq!(guard.get_steps(ii(1, live_step)).unwrap().0, expected);
            assert_eq!(new_hash, running, "running hash must seed step live+1");
            let first_boundary = c_step.div_ceil(reset_frequency) * reset_frequency;
            for boundary in (first_boundary..=live_step).step_by(reset_frequency as usize) {
                assert_eq!(
                    guard.reset_seed_at(boundary),
                    Some(next_seed),
                    "boundary {boundary} must record the seed it folded"
                );
            }
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
                reset_seed_by_boundary: Default::default(),
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
