use crate::metrics;
use crate::state::AtomicVdfState;
use crate::{
    MiningBroadcaster, ReanchorRequest, VdfStep, apply_reset_seed, step_number_to_salt_number,
    vdf_sha,
};
use irys_domain::chain_sync_state::ChainSyncState;
use irys_types::block_provider::{BlockProvider, CanonicalVdfSnapshot};
use irys_types::{H256, H256List, IrysBlockHeader, Traced, U256, block_production::Seed};
use std::collections::VecDeque;
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

pub fn run_vdf<B: BlockProvider>(
    config: &irys_types::VdfConfig,
    global_step_number: u64,
    current_vdf_hash: H256,
    initial_reset_seed: H256,
    mut fast_forward_receiver: Receiver<Traced<VdfStep>>,
    mut reanchor_receiver: UnboundedReceiver<ReanchorRequest>,
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

        // In-process VDF re-anchor (rare — one per deep partition-recovery reorg). The
        // block-tree gate sends the canonical seed window here; we rewrite the poisoned
        // buffer onto canonical steps in place, keeping the forward-only step counter so
        // no VdfStateReadonly holder is orphaned and no restart is needed. Build the
        // corrected buffer OUTSIDE the write lock (the tail recompute runs vdf_sha, up to
        // ~1s/step at production difficulty), then hold the write lock only for the swap.
        while let Ok(request) = reanchor_receiver.try_recv() {
            let capacity = match vdf_state.read() {
                Ok(guard) => guard.capacity,
                Err(_) => {
                    error!("VDF state read lock poisoned during re-anchor; exiting VDF thread");
                    return;
                }
            };
            let (corrected, resume_hash) = match build_reanchored_buffer(
                config,
                &request,
                global_step_number,
                capacity,
            ) {
                Ok(built) => built,
                Err(e) => {
                    error!(
                        canonical_step = request.canonical_step,
                        live_step = global_step_number,
                        "VDF re-anchor request could not be applied; leaving buffer unchanged: {e:?}"
                    );
                    continue;
                }
            };
            {
                let mut guard = match vdf_state.write() {
                    Ok(guard) => guard,
                    Err(_) => {
                        error!(
                            "VDF state write lock poisoned during re-anchor; exiting VDF thread"
                        );
                        return;
                    }
                };
                if let Err(e) = guard.reanchor_seeds(corrected) {
                    error!("VDF re-anchor failed to swap the buffer; leaving it unchanged: {e:?}");
                    continue;
                }
            }
            // Resume stepping from L on the canonical seed. `global_step_number` (the
            // counter) is unchanged; only the seed contents and the loop's running
            // `hash`/`next_reset_seed` move onto the canonical chain.
            hash = resume_hash;
            next_reset_seed = request.next_reset_seed;
            canonical_global_step_number = canonical_global_step_number.max(request.canonical_step);
            // Notify subscribers only AFTER the swap: partition-mining rebuilds its
            // recall-range rotation from the buffer on this event, so broadcasting any
            // earlier would let it reconstruct from the still-poisoned seeds (and no
            // second event would ever correct it).
            broadcast_mining_service.broadcast_reanchored();
            info!(
                canonical_step = request.canonical_step,
                live_step = global_step_number,
                "VDF re-anchored onto canonical steps in-process (no restart)"
            );
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
                if let Some(CanonicalVdfSnapshot {
                    vdf_info,
                    confirmed_global_step_number: _,
                }) = block_provider.latest_canonical_vdf_info()
                {
                    next_reset_seed = vdf_info.next_seed;
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
        }) = block_provider.latest_canonical_vdf_info()
        {
            next_reset_seed = vdf_info.next_seed;
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

/// Build the corrected VDF seed buffer for an in-process re-anchor, plus the running
/// hash to resume stepping from.
///
/// Inputs: the canonical seed window `[cw_first, C]` (`request.canonical_window`, ending
/// at `request.canonical_step`) and the live local tip `live_step` (`L`). Behaviour:
/// - `L > C`: extend the window with a locally recomputed tail `(C, L]`, SHA-chaining
///   forward from step `C`'s output and folding the scheduled reset seed at each
///   boundary — exactly as validation re-derives those steps (#1457: the wrong seed
///   here silently re-poisons, so unknowable boundaries are rejected, not guessed);
/// - `C >= L`: truncate the window to end at `L` (drop canonical steps the forward-only
///   counter has not reached);
/// - then trim from the front so the buffer never exceeds `capacity + 1`
///   (the [`crate::state::VdfState::reanchor_seeds`] contract).
///
/// The reset-seed schedule mirrors `calculate_seeds`/the validation recompute, which
/// folds the *containing block's* `seed` at a boundary: a boundary exactly at `C` sits
/// in the canonical tip's own step window, so it folds `request.canonical_seed` (the
/// tip's `seed` — the child block inherits it); the first boundary strictly after `C`
/// folds `request.next_reset_seed` (the tip's `next_seed`); any boundary after that is
/// pinned by a canonical block that does not exist yet, so such a tail is rejected. A
/// boundary at `L` strictly inside a truncated window is likewise rejected — its fold
/// seed belongs to whichever canonical block contains it, which the request does not
/// carry. A rejected re-anchor leaves the buffer as-is; the Tier-1 validation heal
/// keeps the node following the chain regardless.
///
/// Returns `(corrected, resume_hash)`. `corrected` maps 1:1 onto steps
/// `[L - corrected.len() + 1, L]` (see `VdfState::get_steps`); `resume_hash` is the
/// output of step `L` with the boundary reset folded — the seed the loop feeds into step
/// `L + 1`, so stepping continues as if the loop had reached `L` on the canonical seed.
fn build_reanchored_buffer(
    config: &irys_types::VdfConfig,
    request: &ReanchorRequest,
    live_step: u64,
    capacity: usize,
) -> eyre::Result<(VecDeque<Seed>, H256)> {
    let canonical_step = request.canonical_step;
    let reset_frequency = config.reset_frequency as u64;
    eyre::ensure!(reset_frequency > 0, "reanchor: reset_frequency is zero");

    let mut corrected = request.canonical_window.clone();
    eyre::ensure!(!corrected.is_empty(), "reanchor: canonical window is empty");

    // Scheduled reset seed for a fold at `boundary` (see the doc comment above).
    let seed_at = |boundary: u64| -> H256 {
        if boundary == canonical_step {
            request.canonical_seed
        } else {
            request.next_reset_seed
        }
    };

    match live_step.cmp(&canonical_step) {
        std::cmp::Ordering::Greater => {
            // Boundaries strictly inside (C, L] beyond the first are pinned by
            // canonical blocks that are not mined yet — their seeds are unknowable.
            let boundaries_past_tip =
                live_step / reset_frequency - canonical_step / reset_frequency;
            eyre::ensure!(
                boundaries_past_tip <= 1,
                "reanchor: tail ({canonical_step}, {live_step}] spans {boundaries_past_tip} reset boundaries; only the first has a canonically pinned seed"
            );
            // Recompute the local free-run tail (C, L] from step C's canonical output.
            let mut running = corrected.back().expect("non-empty checked above").0;
            let mut checkpoints = vec![H256::default(); config.num_checkpoints_in_vdf_step];
            for step in canonical_step..live_step {
                // Fold the reset seed at `step` before deriving `step + 1`, mirroring
                // `process_reset` at the bottom of the run_vdf loop.
                let mut out = process_reset(step, running, reset_frequency, seed_at(step));
                let salt = U256::from(step_number_to_salt_number(config, step));
                vdf_sha(
                    salt,
                    &mut out,
                    config.num_checkpoints_in_vdf_step,
                    config.num_iterations_per_checkpoint(),
                    &mut checkpoints,
                );
                corrected.push_back(Seed(out));
                running = out;
            }
        }
        std::cmp::Ordering::Less => {
            // Canonical ran ahead of the local counter: drop steps above L so the buffer
            // ends exactly at L (the forward-only counter never moves).
            for _ in 0..(canonical_step - live_step) {
                corrected.pop_back();
            }
            // The resume fold at L would need the seed of the canonical block whose
            // window contains L — not carried by the request. Reject rather than guess.
            eyre::ensure!(
                !live_step.is_multiple_of(reset_frequency),
                "reanchor: live step {live_step} is a reset boundary inside the canonical window; its fold seed is not carried by the request"
            );
        }
        std::cmp::Ordering::Equal => {}
    }

    eyre::ensure!(
        !corrected.is_empty(),
        "reanchor: corrected buffer empty after aligning to live step {live_step}"
    );

    // Honour the reanchor_seeds contract: at most capacity + 1 seeds. Trim the oldest
    // (nearest the LCA, least likely to be needed by future recall-range validation).
    while corrected.len() > capacity + 1 {
        corrected.pop_front();
    }

    let output_l = corrected.back().expect("non-empty checked above").0;
    // Seed the loop feeds into step L+1: output of L with the boundary reset folded.
    let resume_hash = process_reset(live_step, output_l, reset_frequency, seed_at(live_step));

    Ok((corrected, resume_hash))
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
    Some(global_step_number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_helpers::mocked_vdf_service;
    use crate::state::{
        CancelEnum, VdfController, VdfStateReadonly, vdf_step_batch_is_valid, vdf_steps_are_valid,
    };
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
        fn latest_canonical_vdf_info(&self) -> Option<CanonicalVdfSnapshot> {
            Some(CanonicalVdfSnapshot {
                vdf_info: self.0.vdf_limiter_info.clone(),
                // The mock holds a single block, so the tip is also the confirmed tip.
                confirmed_global_step_number: self.0.vdf_limiter_info.global_step_number,
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
                    mpsc::unbounded_channel::<ReanchorRequest>().1,
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

        let step_num = vdf_steps_guard.current_step();

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
                    mpsc::unbounded_channel::<ReanchorRequest>().1,
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

        let step_num = vdf_steps_guard.current_step();

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
                    mpsc::unbounded_channel::<ReanchorRequest>().1,
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
                mpsc::unbounded_channel::<ReanchorRequest>().1,
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
                    mpsc::unbounded_channel::<ReanchorRequest>().1,
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

        let step_num = vdf_steps_guard.current_step();
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

    /// Broadcaster probe for the re-anchor ordering contract: at the moment
    /// `broadcast_reanchored` fires, the seed buffer must ALREADY hold the canonical
    /// window (subscribers rebuild derived state from the buffer on this event, so a
    /// pre-swap broadcast would hand them the poisoned seeds).
    #[derive(Clone)]
    struct ReanchorOrderProbe {
        vdf_state: AtomicVdfState,
        /// `(first, last, expected steps)` — set by the test before it sends the request.
        expected: Arc<std::sync::Mutex<Option<(u64, u64, Vec<H256>)>>>,
        /// 0 = not fired, 1 = buffer already canonical, 2 = buffer not yet canonical.
        outcome: Arc<AtomicU8>,
    }

    impl MiningBroadcaster for ReanchorOrderProbe {
        fn broadcast(&self, _seed: Seed, _checkpoints: H256List, _global_step: u64) {}

        fn broadcast_reanchored(&self) {
            let Some((first, last, expected)) = self.expected.lock().unwrap().clone() else {
                return;
            };
            let already_canonical = self
                .vdf_state
                .read()
                .unwrap()
                .get_steps(ii(first, last))
                .is_ok_and(|got| got.0 == expected);
            self.outcome.store(
                if already_canonical { 1 } else { 2 },
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    }

    /// End-to-end wiring for the in-process re-anchor (P-A + P-C): a live `run_vdf`
    /// thread drains a `ReanchorRequest` off the channel, rewrites its seed buffer onto
    /// the supplied canonical window, leaves the forward-only step counter untouched —
    /// no restart — and broadcasts `Reanchored` only AFTER the swap (asserted via
    /// `ReanchorOrderProbe`). Deterministic: the loop is frozen at a stable step by
    /// disabling mining (the re-anchor drain runs at the loop top, ahead of the
    /// mining-pause gate, so a paused loop still applies it), then polled for
    /// convergence.
    #[tokio::test]
    async fn run_vdf_applies_reanchor_request_in_process() {
        // reset_frequency high so [1, L] crosses no boundary: the test isolates the
        // channel → drain → buffer-swap path (the tail recompute is unit-tested in
        // `reanchor_buffer`), with C == L so no tail is recomputed here.
        let mut node_config = NodeConfig::testing();
        node_config.consensus.get_mut().vdf.reset_frequency = 1_000;
        node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
        let config = Config::new_with_random_peer_id(node_config);

        init_tracing();

        let initial_seed = H256::repeat_byte(0x01);
        let reset_seed = H256::repeat_byte(0x02);

        let (_ff_tx, ff_rx) = mpsc::channel::<Traced<VdfStep>>(16);
        let (reanchor_tx, reanchor_rx) = mpsc::unbounded_channel::<ReanchorRequest>();
        let vdf_state = mocked_vdf_service(&config);
        let guard = VdfStateReadonly::new(vdf_state.clone());
        enable_mining(&vdf_state);

        let probe = ReanchorOrderProbe {
            vdf_state: vdf_state.clone(),
            expected: Arc::new(std::sync::Mutex::new(None)),
            outcome: Arc::new(AtomicU8::new(0)),
        };

        // Canonical tip far ahead so the loop never parks at the boundary gate.
        let mut mock_header = IrysBlockHeader::new_mock_header();
        mock_header.vdf_limiter_info.global_step_number = 100_000;

        let chain_sync_state = ChainSyncState::new(false, false);
        let shutdown = CancellationToken::new();
        let handle = std::thread::spawn({
            let config = config.clone();
            let shutdown = shutdown.clone();
            let vdf_state = vdf_state.clone();
            let probe = probe.clone();
            move || {
                run_vdf(
                    &config.vdf,
                    0,
                    initial_seed,
                    reset_seed,
                    ff_rx,
                    reanchor_rx,
                    probe,
                    vdf_state,
                    MockBlockProvider(mock_header),
                    chain_sync_state,
                    shutdown,
                )
            }
        });

        let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));
        guard
            .wait_for_step(10, Arc::clone(&cancel), Duration::from_secs(5))
            .await
            .expect("VDF should reach step 10");

        // Freeze the loop at a stable step (mining disabled → no new steps, but the
        // re-anchor channel is still drained each 200ms park cycle).
        VdfController::new(&vdf_state.read().unwrap().mining_flag()).stop();
        tokio::time::sleep(Duration::from_millis(250)).await;
        let live = guard.current_step();

        // A distinct "canonical" window over [live-4, live]; C == live so it is applied
        // verbatim (no tail recompute).
        let canonical: VecDeque<Seed> = (0..5).map(|i| Seed(H256::repeat_byte(0xC0 + i))).collect();
        let canonical_hashes: Vec<H256> = canonical.iter().map(|s| s.0).collect();
        *probe.expected.lock().unwrap() = Some((live - 4, live, canonical_hashes.clone()));
        reanchor_tx
            .send(ReanchorRequest {
                canonical_window: canonical,
                canonical_step: live,
                canonical_seed: reset_seed,
                next_reset_seed: reset_seed,
            })
            .expect("re-anchor channel open");

        // The paused loop applies it within a couple of park cycles.
        let mut converged = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Ok(steps) = guard.get_steps(ii(live - 4, live))
                && steps.0 == canonical_hashes
            {
                converged = true;
                break;
            }
        }
        assert!(
            converged,
            "re-anchor request must replace the buffer window [{}, {}] with the canonical steps",
            live - 4,
            live
        );
        assert_eq!(
            guard.current_step(),
            live,
            "the forward-only step counter must not move on re-anchor"
        );
        // The broadcast follows the swap by nanoseconds, but the convergence poll
        // above watches the buffer, not the probe — give the broadcast a bounded
        // grace window before asserting on it.
        let mut outcome = 0;
        for _ in 0..100 {
            outcome = probe.outcome.load(std::sync::atomic::Ordering::Relaxed);
            if outcome != 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            outcome, 1,
            "Reanchored must be broadcast only AFTER the buffer swap (probe saw: 0 = never fired, 2 = fired before the swap)"
        );

        shutdown.cancel();
        handle.join().unwrap();
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
        fn latest_canonical_vdf_info(&self) -> Option<CanonicalVdfSnapshot> {
            let g = self.0.lock().unwrap();
            Some(CanonicalVdfSnapshot {
                vdf_info: g.0.clone(),
                confirmed_global_step_number: g.1,
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
                    mpsc::unbounded_channel::<ReanchorRequest>().1,
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
        assert_eq!(vdf_steps_guard.current_step(), 7);

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
                    mpsc::unbounded_channel::<ReanchorRequest>().1,
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
            vdf_steps_guard.current_step(),
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
        assert_eq!(clean_guard.current_step(), 15);

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

        // --- The heal (Seam 1): the block-rooted recompute fall-through means a
        //     poisoned buffer no longer WEDGES. A canonical block whose steps chain
        //     from its own prev_output/seed is now ACCEPTED even though the local
        //     buffer holds divergent (loser) steps for the same range, while a
        //     forged block is still rejected. `vdf_step_batch_is_valid` is called
        //     with checkpoint verification off: the step chain is the property under
        //     test (last_step_checkpoints are proven separately in prevalidation),
        //     so this synthetic block needs no precomputed checkpoints. ---
        let winner_step_16 = clean_guard.get_steps(ii(16, 16)).expect("winner step 16").0[0];
        let healed_block = VDFLimiterInfo {
            global_step_number: 20,
            prev_output: winner_step_16,
            seed: winner_seed,
            steps: clean_guard
                .get_steps(ii(17, 20))
                .expect("winner steps 17..=20"),
            ..VDFLimiterInfo::default()
        };
        let healed = vdf_step_batch_is_valid(
            &pool,
            &healed_block,
            &config.vdf,
            &poisoned_guard,
            0..healed_block.steps.len(),
            false,
            Arc::new(AtomicU8::new(CancelEnum::Continue as u8)),
        );
        assert!(
            healed.is_ok(),
            "Seam 1: poisoned buffer must ACCEPT a recompute-consistent canonical block (the heal): {healed:?}"
        );

        let mut forged_steps = clean_guard
            .get_steps(ii(17, 20))
            .expect("winner steps 17..=20");
        let last = forged_steps.0.len() - 1;
        forged_steps.0[last] = H256::repeat_byte(0xee);
        let forged_block = VDFLimiterInfo {
            global_step_number: 20,
            prev_output: winner_step_16,
            seed: winner_seed,
            steps: forged_steps,
            ..VDFLimiterInfo::default()
        };
        let forged = vdf_step_batch_is_valid(
            &pool,
            &forged_block,
            &config.vdf,
            &poisoned_guard,
            0..forged_block.steps.len(),
            false,
            Arc::new(AtomicU8::new(CancelEnum::Continue as u8)),
        );
        assert!(
            forged.is_err(),
            "Seam 1: recompute must still REJECT a forged block even with a poisoned buffer"
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

    /// Unit tests for the in-process re-anchor buffer builder (P-C). They pin the
    /// consensus-critical property that the recomputed tail `(C, L]` equals what
    /// `run_vdf` would produce stepping forward from the canonical step `C` on the
    /// canonical reset seed — #1457 finding #2: the wrong seed here silently re-poisons.
    mod reanchor_buffer {
        use super::super::{build_reanchored_buffer, process_reset};
        use crate::{ReanchorRequest, apply_reset_seed, step_number_to_salt_number, vdf_sha};
        use irys_types::{Config, H256, NodeConfig, U256, VdfConfig, block_production::Seed};
        use std::collections::VecDeque;

        /// Ground-truth step outputs for steps `1..=count`, stepping exactly as
        /// `run_vdf`: `salt(step - 1)` → `vdf_sha` → output of `step`, then
        /// `process_reset(step)` folds the seed `schedule(step)` returns for that
        /// boundary. `out[i]` is the output of step `i + 1`. The per-boundary schedule
        /// lets tests model the canonical rotation (a different seed at each boundary),
        /// which a fixed seed cannot distinguish from the implementation's own folds.
        fn gen_steps_with_schedule(
            config: &VdfConfig,
            seed0: H256,
            schedule: impl Fn(u64) -> H256,
            count: u64,
        ) -> Vec<H256> {
            let mut out = Vec::with_capacity(count as usize);
            let mut hash = seed0;
            let mut checkpoints = vec![H256::default(); config.num_checkpoints_in_vdf_step];
            for step in 1..=count {
                let salt = U256::from(step_number_to_salt_number(config, step - 1));
                vdf_sha(
                    salt,
                    &mut hash,
                    config.num_checkpoints_in_vdf_step,
                    config.num_iterations_per_checkpoint(),
                    &mut checkpoints,
                );
                out.push(hash);
                hash = process_reset(step, hash, config.reset_frequency as u64, schedule(step));
            }
            out
        }

        /// Fixed-seed convenience over [`gen_steps_with_schedule`].
        fn gen_steps(config: &VdfConfig, seed0: H256, reset_seed: H256, count: u64) -> Vec<H256> {
            gen_steps_with_schedule(config, seed0, |_| reset_seed, count)
        }

        fn test_config() -> Config {
            let mut node_config = NodeConfig::testing();
            node_config.consensus.get_mut().vdf.reset_frequency = 8;
            node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
            Config::new_with_random_peer_id(node_config)
        }

        fn window(ground: &[H256], first_step: u64, last_step: u64) -> VecDeque<Seed> {
            ground[(first_step - 1) as usize..last_step as usize]
                .iter()
                .map(|h| Seed(*h))
                .collect()
        }

        fn as_hashes(buf: &VecDeque<Seed>) -> Vec<H256> {
            buf.iter().map(|s| s.0).collect()
        }

        /// L > C: the tail `(C, L]` is recomputed and, together with the canonical
        /// window, equals the canonical ground truth over `[window_first, L]` — crossing
        /// a reset boundary (reset_frequency 8, so boundaries at 8/16/24) on the
        /// canonical seed.
        #[test]
        fn recomputes_tail_matching_forward_stepping() {
            let config = test_config();
            let vdf = &config.vdf;
            let reset_seed = H256::repeat_byte(0x22);
            let (live_step, canonical_step, window_first) = (30_u64, 20_u64, 5_u64);

            let ground = gen_steps(vdf, H256::repeat_byte(0x11), reset_seed, live_step);
            let request = ReanchorRequest {
                canonical_window: window(&ground, window_first, canonical_step),
                canonical_step,
                // Distinct from the boundary seed: C (20) is not on a boundary, so a
                // build that wrongly folded canonical_seed anywhere would diverge.
                canonical_seed: H256::repeat_byte(0xAA),
                next_reset_seed: reset_seed,
            };

            let (corrected, resume_hash) =
                build_reanchored_buffer(vdf, &request, live_step, 1024).unwrap();

            assert_eq!(
                as_hashes(&corrected),
                ground[(window_first - 1) as usize..live_step as usize].to_vec(),
                "corrected buffer must equal canonical ground truth over [window_first, L]"
            );
            let output_l = ground[(live_step - 1) as usize];
            assert_eq!(
                resume_hash,
                process_reset(live_step, output_l, vdf.reset_frequency as u64, reset_seed),
                "resume hash must be the output of L with the boundary reset folded"
            );
        }

        /// C >= L: canonical ran ahead of the local counter; the window is truncated to
        /// end exactly at L (forward-only counter honoured, no tail recompute).
        #[test]
        fn truncates_when_canonical_ahead_of_live() {
            let config = test_config();
            let vdf = &config.vdf;
            let reset_seed = H256::repeat_byte(0x44);
            let (canonical_step, live_step, window_first) = (25_u64, 18_u64, 4_u64);

            let ground = gen_steps(vdf, H256::repeat_byte(0x33), reset_seed, canonical_step);
            let request = ReanchorRequest {
                canonical_window: window(&ground, window_first, canonical_step),
                canonical_step,
                canonical_seed: H256::repeat_byte(0xBB),
                next_reset_seed: reset_seed,
            };

            let (corrected, resume_hash) =
                build_reanchored_buffer(vdf, &request, live_step, 1024).unwrap();

            assert_eq!(
                as_hashes(&corrected),
                ground[(window_first - 1) as usize..live_step as usize].to_vec(),
                "corrected buffer must be truncated to end at L"
            );
            let output_l = ground[(live_step - 1) as usize];
            assert_eq!(
                resume_hash,
                process_reset(live_step, output_l, vdf.reset_frequency as u64, reset_seed)
            );
        }

        /// The corrected buffer never exceeds `capacity + 1`: the oldest steps (nearest
        /// the LCA) are trimmed and it still ends at L.
        #[test]
        fn clamps_to_capacity_plus_one() {
            let config = test_config();
            let vdf = &config.vdf;
            let reset_seed = H256::repeat_byte(0x66);
            let (live_step, canonical_step, capacity) = (40_u64, 40_u64, 10_usize);

            let ground = gen_steps(vdf, H256::repeat_byte(0x55), reset_seed, canonical_step);
            let request = ReanchorRequest {
                canonical_window: window(&ground, 1, canonical_step),
                canonical_step,
                // C = L = 40 sits on a boundary; only the (ignored) resume fold uses
                // this, so the clamp assertion is seed-independent.
                canonical_seed: H256::repeat_byte(0x77),
                next_reset_seed: reset_seed,
            };

            let (corrected, _resume) =
                build_reanchored_buffer(vdf, &request, live_step, capacity).unwrap();

            assert_eq!(corrected.len(), capacity + 1, "must clamp to capacity + 1");
            assert_eq!(
                as_hashes(&corrected),
                ground[(live_step as usize - capacity - 1)..live_step as usize].to_vec(),
                "clamp keeps the newest capacity + 1 steps, ending at L"
            );
        }

        /// An empty canonical window is rejected — there is nothing to anchor onto.
        #[test]
        fn rejects_empty_window() {
            let config = test_config();
            let request = ReanchorRequest {
                canonical_window: VecDeque::new(),
                canonical_step: 10,
                canonical_seed: H256::zero(),
                next_reset_seed: H256::zero(),
            };
            assert!(build_reanchored_buffer(&config.vdf, &request, 12, 64).is_err());
        }

        /// Rotating-schedule regression (X-2): the canonical tip's last step sits
        /// exactly ON a reset boundary (C = 24, rf = 8), and the tail then crosses the
        /// NEXT boundary (32) before reaching L = 33. Per `calculate_seeds`, the fold
        /// at 24 belongs to the tip's own window (tip's `seed`), while the fold at 32
        /// belongs to the first post-tip rotation (tip's `next_seed`). The ground truth
        /// rotates seeds per boundary, so an implementation folding a single seed at
        /// every boundary cannot match it.
        #[test]
        fn folds_tip_seed_on_boundary_then_next_seed_at_first_post_tip_boundary() {
            let config = test_config();
            let vdf = &config.vdf;
            let pre_tip_seed = H256::repeat_byte(0x10); // boundaries 8, 16 (inside the window)
            let tip_seed = H256::repeat_byte(0x20); // boundary 24 == C (tip's own window)
            let next_seed = H256::repeat_byte(0x30); // boundary 32 (first post-tip rotation)
            let (canonical_step, live_step, window_first) = (24_u64, 33_u64, 20_u64);

            let schedule = |boundary: u64| match boundary {
                24 => tip_seed,
                32 => next_seed,
                _ => pre_tip_seed,
            };
            let ground = gen_steps_with_schedule(vdf, H256::repeat_byte(0x99), schedule, live_step);
            let request = ReanchorRequest {
                canonical_window: window(&ground, window_first, canonical_step),
                canonical_step,
                canonical_seed: tip_seed,
                next_reset_seed: next_seed,
            };

            let (corrected, resume_hash) =
                build_reanchored_buffer(vdf, &request, live_step, 1024).unwrap();

            assert_eq!(
                as_hashes(&corrected),
                ground[(window_first - 1) as usize..live_step as usize].to_vec(),
                "tail must fold the tip's seed at C and the tip's next_seed at the first post-tip boundary"
            );
            let output_l = ground[(live_step - 1) as usize];
            assert_eq!(
                resume_hash, output_l,
                "L = 33 is not a boundary, so the resume hash is step L's output unfolded"
            );
        }

        /// L == C on a boundary: the resume fold (deriving L + 1) belongs to the tip's
        /// own window, so it must use the tip's `seed`, not its `next_seed`.
        #[test]
        fn resume_folds_tip_seed_when_live_equals_canonical_on_boundary() {
            let config = test_config();
            let vdf = &config.vdf;
            let tip_seed = H256::repeat_byte(0x21);
            let next_seed = H256::repeat_byte(0x31);
            let (canonical_step, live_step) = (24_u64, 24_u64);

            let ground = gen_steps(vdf, H256::repeat_byte(0x88), H256::repeat_byte(0x11), 24);
            let request = ReanchorRequest {
                canonical_window: window(&ground, 20, canonical_step),
                canonical_step,
                canonical_seed: tip_seed,
                next_reset_seed: next_seed,
            };

            let (_, resume_hash) = build_reanchored_buffer(vdf, &request, live_step, 1024).unwrap();

            let output_l = ground[(live_step - 1) as usize];
            assert_eq!(
                resume_hash,
                apply_reset_seed(output_l, tip_seed),
                "the fold at a boundary equal to C must use the tip's seed"
            );
        }

        /// A tail spanning TWO post-tip boundaries is rejected: the second boundary's
        /// seed is pinned by a canonical rotation block that does not exist yet, so
        /// recomputing across it would only guess (and silently re-poison — #1457).
        #[test]
        fn rejects_tail_spanning_two_post_tip_boundaries() {
            let config = test_config();
            let vdf = &config.vdf;
            let reset_seed = H256::repeat_byte(0x42);
            let (canonical_step, live_step) = (20_u64, 33_u64); // boundaries 24 and 32 in (C, L]

            let ground = gen_steps(vdf, H256::repeat_byte(0x41), reset_seed, canonical_step);
            let request = ReanchorRequest {
                canonical_window: window(&ground, 5, canonical_step),
                canonical_step,
                canonical_seed: reset_seed,
                next_reset_seed: reset_seed,
            };

            let err = build_reanchored_buffer(vdf, &request, live_step, 1024).unwrap_err();
            assert!(
                err.to_string().contains("spans 2 reset boundaries"),
                "{err}"
            );
        }

        /// Truncation ending exactly on a boundary is rejected: the resume fold at L
        /// needs the seed of the canonical block containing L, which the request does
        /// not carry.
        #[test]
        fn rejects_truncation_ending_on_boundary() {
            let config = test_config();
            let vdf = &config.vdf;
            let reset_seed = H256::repeat_byte(0x52);
            let (canonical_step, live_step) = (25_u64, 16_u64); // L is a boundary (rf = 8)

            let ground = gen_steps(vdf, H256::repeat_byte(0x51), reset_seed, canonical_step);
            let request = ReanchorRequest {
                canonical_window: window(&ground, 4, canonical_step),
                canonical_step,
                canonical_seed: reset_seed,
                next_reset_seed: reset_seed,
            };

            let err = build_reanchored_buffer(vdf, &request, live_step, 1024).unwrap_err();
            assert!(
                err.to_string()
                    .contains("reset boundary inside the canonical window"),
                "{err}"
            );
        }
    }
}
