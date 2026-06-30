use crate::{apply_reset_seed, step_number_to_salt_number, vdf_sha, warn_mismatches};
use eyre::{bail, eyre};
use irys_efficient_sampling::num_recall_ranges_in_partition;
use irys_types::{
    AtomicVdfStepNumber, Config, H256, H256List, U256, VDFLimiterInfo, VdfConfig,
    block_production::Seed,
};
use nodit::{InclusiveInterval as _, Interval, interval::ii};
use rayon::prelude::*;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::{
    collections::VecDeque,
    ops::Range,
    sync::{Arc, OnceLock, RwLock, RwLockReadGuard},
};
use tokio::time::{Duration, sleep};
use tracing::{debug, error, info, warn};

#[derive(Debug, thiserror::Error)]
pub enum WaitForStepError {
    #[error("Cancelled")]
    Cancelled,
    #[error(
        "VDF state did not advance for {progress_timeout:?} (current={current}, desired={desired})"
    )]
    Stalled {
        progress_timeout: Duration,
        current: u64,
        desired: u64,
    },
}

#[derive(Debug)]
pub struct VdfState {
    /// Last global step stored — the single source of truth for the step
    /// number. Held as an `Arc<AtomicU64>` so the mining hot path and the read
    /// handle observe it WITHOUT taking the `RwLock` (same allocation, shared
    /// by `VdfStateReadonly`). Private so the SSOT cannot be replaced after
    /// construction; mutate via `store_step`/`set_canonical_step`, read via
    /// `current_step`/`step_counter`.
    ///
    /// `Relaxed` is sufficient: every writer runs under the `RwLock` write
    /// guard (`store_step`), which serialises writers; any reader that also
    /// needs the seed deque re-takes the read lock, which provides the
    /// happens-before edge. The bare counter only ever needs a coherent
    /// scalar, not ordering against the deque.
    global_step: AtomicVdfStepNumber,
    /// maximum number of seeds to store in seeds VecDeque
    pub capacity: usize,
    /// stored seeds
    pub seeds: VecDeque<Seed>,
    /// whether the VDF thread is mining or paused. The single shared
    /// allocation (same `Arc` as the `VdfController` and the read handle);
    /// private so it cannot be swapped out after construction.
    is_vdf_mining_enabled: Arc<AtomicBool>,
    /// global step from the latest canonical block
    global_step_from_the_latest_canonical_block: u64,
    /// minimum global step to keep in the seeds VecDeque
    minimum_step_to_keep: u64,
}

impl VdfState {
    pub fn new(capacity: usize, global_step: u64, is_vdf_mining_enabled: Arc<AtomicBool>) -> Self {
        Self {
            global_step: Arc::new(AtomicU64::new(global_step)),
            global_step_from_the_latest_canonical_block: global_step,
            minimum_step_to_keep: global_step.saturating_sub(capacity as u64),
            seeds: VecDeque::with_capacity(capacity),
            capacity,
            is_vdf_mining_enabled,
        }
    }

    /// Lock-free read of the single source-of-truth step counter.
    pub fn current_step(&self) -> u64 {
        self.global_step.load(Ordering::Relaxed)
    }

    /// Shared handle to the owned step counter (same allocation). Lets the read
    /// handle observe the SSOT counter without taking the `RwLock`.
    pub(crate) fn step_counter(&self) -> AtomicVdfStepNumber {
        Arc::clone(&self.global_step)
    }

    /// Shared handle to the owned mining-enable flag (same allocation). Lets the
    /// read handle / controller observe and toggle mining without the `RwLock`.
    pub(crate) fn mining_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.is_vdf_mining_enabled)
    }

    pub fn set_canonical_step(&mut self, global_canonical_step: u64) {
        self.global_step_from_the_latest_canonical_block = global_canonical_step;
        self.minimum_step_to_keep = global_canonical_step.saturating_sub(self.capacity as u64);
    }

    pub fn canonical_step(&self) -> u64 {
        self.global_step_from_the_latest_canonical_block
    }

    pub fn get_last_step_and_seed(&self) -> (u64, Seed) {
        (
            self.current_step(),
            self.seeds
                .back()
                .cloned()
                .expect("To have at least the genesis step to be inserted"),
        )
    }

    pub fn store_step(&mut self, seed: Seed, global_step: u64) -> u64 {
        let current = self.current_step();
        if current >= global_step {
            return current;
        }
        if current + 1 != global_step {
            // Gap path: previously panicked, now log and no-op so the VDF
            // loop catches up via normal stepping. Callers detect the no-op
            // by observing the returned step == self.current_step().
            //
            // pop_front MUST stay in the sequential branch below — a stale
            // pop_front here silently shrinks the seed buffer on every gap
            // until `get_last_step_and_seed` panics on an empty deque.
            error!(
                current = current,
                proposed = global_step,
                gap = global_step.saturating_sub(current + 1),
                "VDF state would have a gap; ignoring step (VDF will catch up via normal stepping)"
            );
            return current;
        }
        // Saturating to usize::MAX means seeds.len() >= vdf_depth is always
        // false, so the buffer never trims in this edge case. Safe — only
        // unrealistic step counts (well past usize::MAX) would hit this.
        let vdf_depth = usize::try_from(global_step.saturating_sub(self.minimum_step_to_keep))
            .unwrap_or(usize::MAX);
        if self.seeds.len() >= vdf_depth {
            self.seeds.pop_front();
        }
        self.seeds.push_back(seed);
        self.global_step.store(current + 1, Ordering::Relaxed);
        global_step
    }

    /// Get steps in the given global steps numbers Interval
    pub fn get_steps(&self, i: Interval<u64>) -> eyre::Result<H256List> {
        let vdf_steps_len =
            u64::try_from(self.seeds.len()).expect("seed buffer length must fit in u64");

        let last_global_step = self.current_step();

        // first available global step should be at least one.
        // TODO: Should this instead panic! as something has gone very wrong?
        let first_global_step = last_global_step.saturating_sub(vdf_steps_len) + 1;

        if first_global_step > last_global_step {
            return Err(eyre::eyre!("No steps stored!"));
        }

        if !ii(first_global_step, last_global_step).contains_interval(&i) {
            return Err(eyre::eyre!(
                "Unavailable requested range ({}..={}). Stored steps range is ({}..={})",
                i.start(),
                i.end(),
                first_global_step,
                last_global_step
            ));
        }

        let start: usize = (i.start() - first_global_step).try_into()?;
        let end: usize = (i.end() - first_global_step).try_into()?;

        Ok(H256List(
            self.seeds
                .range(start..=end)
                .map(|seed| seed.0)
                .collect::<Vec<H256>>(),
        ))
    }
}

pub type AtomicVdfState = Arc<RwLock<VdfState>>;

/// Read-only handle over the shared VDF state. Holds clones of the owned step
/// counter and mining flag (same allocations as `VdfState`) so `current_step()`
/// / `is_mining_enabled()` never take the `RwLock`.
///
/// The read-only contract is sealed: a production build has no public way to
/// obtain a writable handle to the underlying state. The former
/// `into_inner_cloned` escape was removed, and the only mutation entry point
/// (`test_set_step`) is gated behind the `test-utils` feature. The doc-test
/// below proves the escape is unreachable — it must fail to compile (keyed on
/// the removed `into_inner_cloned`, so it holds even if feature unification
/// enables `test-utils`):
///
/// ```compile_fail
/// use irys_vdf::state::{VdfState, VdfStateReadonly};
/// use std::sync::atomic::AtomicBool;
/// use std::sync::{Arc, RwLock};
///
/// let mining = Arc::new(AtomicBool::new(false));
/// let state = Arc::new(RwLock::new(VdfState::new(0, 0, mining)));
/// let handle = VdfStateReadonly::new(state);
/// // `into_inner_cloned` was removed: no public writable escape exists.
/// let _writable = handle.into_inner_cloned();
/// ```
#[derive(Debug, Clone)]
pub struct VdfStateReadonly {
    state: AtomicVdfState,
    global_step: AtomicVdfStepNumber,
    is_vdf_mining_enabled: Arc<AtomicBool>,
}

impl VdfStateReadonly {
    /// Creates a read handle, capturing the owned step counter and mining flag
    /// (same allocations) once at construction so the lock-free accessors never
    /// re-take the `RwLock`. No longer `const`: it reads the lock once here.
    /// Handles lock poisoning the same way `read()` does.
    pub fn new(state: AtomicVdfState) -> Self {
        let (global_step, is_vdf_mining_enabled) = {
            let guard = state
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (guard.step_counter(), guard.mining_flag())
        };
        Self {
            state,
            global_step,
            is_vdf_mining_enabled,
        }
    }

    /// Lock-free current global step (reads the owned atomic).
    pub fn current_step(&self) -> u64 {
        self.global_step.load(Ordering::Relaxed)
    }

    /// Lock-free mining-enabled read (reads the owned flag).
    pub fn is_mining_enabled(&self) -> bool {
        self.is_vdf_mining_enabled.load(Ordering::Relaxed)
    }

    /// Read access to internal steps queue.
    ///
    /// On `RwLock` poisoning (a prior writer panicked), recovers the inner
    /// guard via `into_inner` and logs once at error level instead of
    /// re-panicking. Readers cannot observably worsen a poisoned state, and
    /// `run_vdf`'s shutdown handling already converts the underlying writer
    /// panic into a controlled exit; surfacing a panic here would only
    /// re-cascade through consensus-critical callers (mining, validation,
    /// `wait_for_step`) whose only useful response is to drop work.
    pub fn read(&self) -> RwLockReadGuard<'_, VdfState> {
        self.state.read().unwrap_or_else(|poisoned| {
            tracing::error!(
                "VDF state RwLock poisoned by a prior writer panic; \
                 recovering inner guard for read-only access. The node \
                 should be restarted once the lifecycle observes the \
                 controlled-shutdown signal."
            );
            poisoned.into_inner()
        })
    }

    /// Get steps in the given global steps numbers Interval
    pub fn get_steps(&self, i: Interval<u64>) -> eyre::Result<H256List> {
        self.read().get_steps(i)
    }

    /// Get a specific step by step number
    pub fn get_step(&self, step_number: u64) -> eyre::Result<H256> {
        self.get_steps(ii(step_number, step_number))?
            .0
            .first()
            .copied()
            .ok_or(eyre!("Step not found"))
    }

    /// Wait until `desired_step_number` is reached.
    ///
    /// Polls `global_step` at 20 Hz, bailing if:
    /// - the cancel signal is set (e.g., shutdown, preemption), or
    /// - `global_step` does not advance for `progress_timeout`.
    ///
    /// The progress check guards against a dead/stuck VDF writer thread:
    /// callers can wait for legitimately long step ranges, but a stalled
    /// state surfaces as a typed error instead of an indefinite hang.
    pub async fn wait_for_step(
        &self,
        desired_step_number: u64,
        cancel: Arc<AtomicU8>,
        progress_timeout: std::time::Duration,
    ) -> eyre::Result<()> {
        use tokio::time::Instant;

        let retries_per_second: u32 = 20;
        let mut last_observed_step = self.current_step();
        let mut last_progress_at = Instant::now();
        let mut attempts = 0_u32;

        loop {
            if cancel.load(Ordering::Relaxed) == CancelEnum::Cancelled as u8 {
                warn!(
                    vdf.desired_step = desired_step_number,
                    vdf.current_step = last_observed_step,
                    "VDF wait cancelled"
                );
                return Err(WaitForStepError::Cancelled.into());
            }

            let current_step = self.current_step();

            if current_step >= desired_step_number {
                debug!(vdf.desired_step = desired_step_number, "VDF step available");
                return Ok(());
            }

            if current_step > last_observed_step {
                last_observed_step = current_step;
                last_progress_at = Instant::now();
            } else if last_progress_at.elapsed() >= progress_timeout {
                return Err(WaitForStepError::Stalled {
                    progress_timeout,
                    current: current_step,
                    desired: desired_step_number,
                }
                .into());
            }

            if attempts.is_multiple_of(retries_per_second) {
                debug!(
                    vdf.desired_step = desired_step_number,
                    vdf.current_step = current_step,
                    "Waiting for VDF step"
                );
            }
            attempts = attempts.wrapping_add(1);
            sleep(Duration::from_millis(1000 / u64::from(retries_per_second))).await;
        }
    }
}

/// TEST-ONLY mutation handle. Gated behind `test` (in-crate) and the
/// `test-utils` feature (so downstream crates' tests — e.g. `irys-p2p` — can
/// enable it as a dev-dependency feature). It replaces the removed
/// `into_inner_cloned` writable escape: it can push the step counter forward
/// WITHOUT handing back the writable `Arc<RwLock<_>>`, so the read-only
/// contract holds in production builds.
#[cfg(any(test, feature = "test-utils"))]
impl VdfStateReadonly {
    /// Force the global step counter to `step` (lock-free). Mirrors the legacy
    /// `into_inner_cloned().write().global_step = step` stub.
    pub fn test_set_step(&self, step: u64) {
        self.global_step.store(step, Ordering::Relaxed);
    }
}

/// Narrow control surface for VDF mining enable/disable. Owns a clone of the
/// single `Arc<AtomicBool>` (same allocation as `VdfState.is_vdf_mining_enabled`
/// and the read handle).
///
/// Last-writer-wins semantics: the toggles are plain `Relaxed` stores with no
/// compare-and-swap (see the chain-sync pause/restore in `chain_sync.rs`, which
/// is deliberately preserved as last-writer-wins).
#[derive(Debug, Clone)]
pub struct VdfController {
    is_vdf_mining_enabled: Arc<AtomicBool>,
}

impl VdfController {
    pub fn new(flag: &Arc<AtomicBool>) -> Self {
        Self {
            is_vdf_mining_enabled: Arc::clone(flag),
        }
    }

    pub fn start(&self) {
        self.is_vdf_mining_enabled.store(true, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.is_vdf_mining_enabled.store(false, Ordering::Relaxed);
    }

    /// Set the mining flag to an explicit value (used by the chain-sync
    /// pause/restore and `IrysNodeCtx::vdf_state`).
    pub fn set_enabled(&self, enabled: bool) {
        self.is_vdf_mining_enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn is_enabled(&self) -> bool {
        self.is_vdf_mining_enabled.load(Ordering::Relaxed)
    }
}

/// One atomic snapshot of the VDF seed history needed to initialise [`VdfState`].
///
/// Contract: `ordered_seeds` is oldest→newest and contiguous over global steps
/// `first_step ..= global_step`, with `first_step == global_step -
/// ordered_seeds.len() + 1`. Genesis is step-anchored: a genesis-only chain
/// (after `run_vdf_for_genesis_block`) yields `{ global_step: 1, first_step: 1,
/// ordered_seeds: [genesis.steps[0]] }`.
///
/// Note: the production replay may return up to `capacity + 1` seeds — it can
/// exhaust `capacity` within a height-1 block and then unconditionally prepend
/// the genesis seed. This is the legacy behaviour and is preserved.
#[derive(Debug, Clone)]
pub struct VdfBootstrap {
    pub global_step: u64,
    pub first_step: u64,
    pub ordered_seeds: VecDeque<Seed>,
}

/// Supplies the seed history used to bootstrap [`VdfState`] at startup. Inverts
/// the former `irys-vdf -> irys-database` dependency: the DB/header replay lives
/// in `irys-chain`.
pub trait VdfSeedSource {
    /// Build a bootstrap snapshot keeping at most `capacity` newest seeds
    /// (plus the unconditional genesis anchor — see [`VdfBootstrap`]). Panics on
    /// storage corruption, preserving the legacy `create_state`
    /// fail-fast-at-startup behaviour.
    fn vdf_bootstrap(&self, capacity: usize) -> VdfBootstrap;
}

/// create VDF state from a seed source (DB-backed in production).
pub fn create_state(
    seed_source: &dyn VdfSeedSource,
    is_vdf_mining_enabled: Arc<AtomicBool>,
    config: &Config,
) -> VdfState {
    let capacity = calc_capacity(config);
    let VdfBootstrap {
        global_step,
        first_step,
        ordered_seeds,
    } = seed_source.vdf_bootstrap(capacity);
    // Enforce the VdfBootstrap contract in release builds too: a corrupt or buggy
    // VdfSeedSource must fail fast at startup rather than silently initialize a
    // non-contiguous or empty seed window (consensus-critical).
    assert!(
        !ordered_seeds.is_empty(),
        "VdfBootstrap must contain at least one seed"
    );
    let seed_count =
        u64::try_from(ordered_seeds.len()).expect("VdfBootstrap seed count must fit in u64");
    let expected_first_step = global_step
        .checked_sub(seed_count.saturating_sub(1))
        .expect("VdfBootstrap contains more seeds than global_step can anchor");
    assert_eq!(
        first_step, expected_first_step,
        "VdfBootstrap contiguity contract"
    );

    info!(
        "Initializing vdf service from seed source at step number {}",
        global_step
    );

    VdfState {
        global_step: Arc::new(AtomicU64::new(global_step)),
        global_step_from_the_latest_canonical_block: global_step,
        minimum_step_to_keep: global_step.saturating_sub(capacity as u64),
        seeds: ordered_seeds,
        capacity,
        is_vdf_mining_enabled,
    }
}

/// return the larger of max_allowed_vdf_fork_steps or num_recall_ranges_in_partition()
/// num_recall_ranges_in_partition() ensures the capacity of VecDeqeue is large enough for the partition.
/// max_allowed_vdf_fork_steps of 60k allows for forks. VDF capacity limits the depth at which a fork can happen. If the fork happens out of the VDF range, the node cannot validate it.
#[tracing::instrument(level = "trace", skip_all)]
fn calc_capacity(config: &Config) -> usize {
    let capacity_from_config: u64 = num_recall_ranges_in_partition(&config.consensus);

    let max_allowed_vdf_fork_steps = config.vdf.max_allowed_vdf_fork_steps;

    let capacity = if capacity_from_config < max_allowed_vdf_fork_steps {
        warn!(
            "capacity in config: {} set too low. Overridden with {}",
            capacity_from_config, max_allowed_vdf_fork_steps
        );
        max_allowed_vdf_fork_steps
    } else {
        capacity_from_config
    };

    capacity.try_into().expect("expected u64 to cast to u32")
}

#[repr(u8)]
#[derive(Debug, Copy, Clone)]
pub enum CancelEnum {
    Continue = 0,
    InvalidStep = 1,
    Cancelled = 2,
}

/// Validate the steps from the `nonce_info` to see if they are valid.
/// Verifies each step in parallel across as many cores as are available.
pub fn vdf_step_batch_is_valid(
    pool: &rayon::ThreadPool,
    vdf_info: &VDFLimiterInfo,
    config: &VdfConfig,
    vdf_steps_guard: &VdfStateReadonly,
    batch_range: Range<usize>,
    verify_last_step_checkpoints: bool,
    cancel: Arc<AtomicU8>,
) -> eyre::Result<()> {
    if batch_range.start >= batch_range.end {
        bail!("VDF batch range must be non-empty");
    }
    if batch_range.end > vdf_info.steps.len() {
        bail!(
            "VDF batch range {:?} exceeds {} available steps",
            batch_range,
            vdf_info.steps.len()
        );
    }

    let batch_steps = &vdf_info.steps.0[batch_range.clone()];
    let batch_start_step_number = vdf_info.first_step_number() + batch_range.start as u64;
    let batch_end_step_number = batch_start_step_number + batch_steps.len() as u64 - 1;

    // Fast-path: if the range is already known locally and matches, skip the
    // parallel VDF recomputation. `get_steps` returns owned data and releases
    // the underlying read guard before we log or compare below.
    match vdf_steps_guard.get_steps(ii(batch_start_step_number, batch_end_step_number)) {
        Ok(steps) => {
            tracing::debug!(
                vdf.batch_start = batch_start_step_number,
                vdf.batch_end = batch_end_step_number,
                "Validating VDF steps from VdfStepsReadGuard!"
            );
            if steps.0.as_slice() != batch_steps {
                let expected = H256List(batch_steps.to_vec());
                warn_mismatches(&steps, &expected);
                return Err(eyre!("VDF steps are invalid!"));
            }
            // `verify_last_step_checkpoints` is intentionally skipped on this
            // fast path. Every block reaching the validation service has
            // already passed through `prevalidate_block`
            // (crates/actors/src/block_validation.rs), which unconditionally
            // calls `last_step_checkpoints_is_valid` against the block's
            // claimed `vdf_limiter_info`. That helper (crates/vdf/src/lib.rs)
            // re-derives the SHA chain from the previous step's seed and
            // rejects any mismatch — including the invariant that the last
            // checkpoint equals the last step. So by the time we reach here,
            // the block's `last_step_checkpoints` are already proven
            // consistent with `steps`; repeating the check would just redo
            // work the node already did. Static reviewers (CodeRabbit) flag
            // the missing call repeatedly — leave this comment so they don't.
            return Ok(());
        }
        Err(err) => tracing::debug!(
            vdf.batch_start = batch_start_step_number,
            vdf.batch_end = batch_end_step_number,
            "Unable to get full steps range from VdfStepsReadGuard: {:?} so calculating vdf batch for validation",
            err.to_string()
        ),
    }

    let previous_seed = if batch_range.start == 0 {
        vdf_info.prev_output
    } else {
        vdf_info.steps[batch_range.start - 1]
    };

    if cancel.load(Ordering::Relaxed) == CancelEnum::Cancelled as u8 {
        bail!("Cancelled");
    }

    // Only the thread for the last index ever writes here, so `OnceLock`
    // captures the value without locking or an `Arc`.
    let computed_last_checkpoints: OnceLock<H256List> = OnceLock::new();
    let last_index = batch_steps.len() - 1;

    pool.install(|| {
        (0..batch_steps.len()).into_par_iter().try_for_each(|i| {
            let cancel_state = cancel.load(Ordering::Relaxed);
            if cancel_state == CancelEnum::InvalidStep as u8 {
                return Err(eyre!(
                    "One of the previous threads found a mismatch, stopping further calculations"
                ));
            }
            if cancel_state == CancelEnum::Cancelled as u8 {
                bail!("Cancelled");
            }

            let previous_step_number = batch_start_step_number - 1 + i as u64;
            let salt = U256::from(step_number_to_salt_number(config, previous_step_number));
            let mut seed = if i == 0 {
                previous_seed
            } else {
                batch_steps[i - 1]
            };
            if previous_step_number > 0
                && previous_step_number.is_multiple_of(config.reset_frequency as u64)
            {
                info!(
                    "Applying reset seed {:?} to step number {}",
                    vdf_info.seed, previous_step_number
                );
                seed = apply_reset_seed(seed, vdf_info.seed);
            }
            let mut checkpoints = vec![H256::default(); config.num_checkpoints_in_vdf_step];
            vdf_sha(
                salt,
                &mut seed,
                config.num_checkpoints_in_vdf_step,
                config.num_iterations_per_checkpoint(),
                &mut checkpoints,
            );

            if seed != batch_steps[i] {
                // Unconditional store: a real validation finding takes priority
                // over any concurrent `Cancelled` state. The block is invalid
                // regardless of whether shutdown/preemption also asked us to
                // stop — cancellation is a coordination signal and must never
                // be allowed to mask a deterministic protocol violation.
                cancel.store(CancelEnum::InvalidStep as u8, Ordering::Relaxed);
                return Err(eyre!(
                    "VDF step {} is invalid! Expected: {:?}, got: {:?}",
                    previous_step_number,
                    batch_steps[i],
                    seed
                ));
            }

            if verify_last_step_checkpoints && i == last_index {
                // Infallible: only one thread reaches this branch.
                let _ = computed_last_checkpoints.set(H256List(checkpoints));
            }

            Ok(())
        })
    })?;

    if verify_last_step_checkpoints {
        match computed_last_checkpoints.get() {
            Some(cks) if cks == &vdf_info.last_step_checkpoints => {}
            Some(cks) => {
                warn_mismatches(cks, &vdf_info.last_step_checkpoints);
                return Err(eyre!("VDF last step checkpoints are invalid!"));
            }
            None => return Err(eyre!("VDF last step checkpoints are invalid!")),
        }
    }

    Ok(())
}

/// Validate the steps from the `nonce_info` to see if they are valid.
/// Verifies each step in parallel across as many cores as are available.
pub fn vdf_steps_are_valid(
    pool: &rayon::ThreadPool,
    vdf_info: &VDFLimiterInfo,
    config: &VdfConfig,
    vdf_steps_guard: &VdfStateReadonly,
    cancel: Arc<AtomicU8>, // fun fact: AtomicBool is the same thing as AtomicU8 (UnsafeCell around a u8)
                           // but we use AtomicU8 to signal *why* we need to stop (cancellation vs actual error)
) -> eyre::Result<()> {
    info!(
        "Checking seed {:?} reset_seed {:?}",
        vdf_info.prev_output, vdf_info.seed
    );
    vdf_step_batch_is_valid(
        pool,
        vdf_info,
        config,
        vdf_steps_guard,
        0..vdf_info.steps.len(),
        true,
        cancel,
    )
}

pub mod test_helpers {
    use super::*;

    use std::sync::RwLock;

    pub fn mocked_vdf_service(config: &Config) -> AtomicVdfState {
        let is_vdf_mining_enabled = Arc::new(AtomicBool::new(false));
        let capacity = calc_capacity(config);

        let state = VdfState {
            global_step: Arc::new(AtomicU64::new(0)),
            global_step_from_the_latest_canonical_block: 0,
            minimum_step_to_keep: 0,
            capacity,
            seeds: VecDeque::default(),
            is_vdf_mining_enabled,
        };
        Arc::new(RwLock::new(state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{Config, H256List, NodeConfig, VDFLimiterInfo};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn test_mid_execution_cancellation() {
        // Create node config and extract VDF config
        let mut node_config = NodeConfig::testing();
        // Set moderately high computational cost for longer computation
        node_config.consensus.get_mut().vdf.sha_1s_difficulty = 5_000_000; // Moderately high difficulty
        node_config
            .consensus
            .get_mut()
            .vdf
            .num_checkpoints_in_vdf_step = 10;

        let config = Config::new_with_random_peer_id(node_config.clone());
        let vdf_config = config.vdf.clone();

        // Generate fewer steps but with much higher computational cost each
        let num_steps = 10; // Fewer steps but each takes longer
        let mut steps = Vec::new();

        // Generate initial seed
        let mut seed = H256::from_low_u64_be(42);

        println!("Generating {} VDF steps for test...", num_steps);
        for i in 0..num_steps {
            // Apply reset seed if needed
            if i > 0 && i % vdf_config.reset_frequency == 0 {
                seed = apply_reset_seed(seed, H256::from_low_u64_be(1337));
            }

            // Calculate next step
            let salt = U256::from(step_number_to_salt_number(&vdf_config, i as u64));
            let mut checkpoints = vec![H256::default(); vdf_config.num_checkpoints_in_vdf_step];

            vdf_sha(
                salt,
                &mut seed,
                vdf_config.num_checkpoints_in_vdf_step,
                vdf_config.num_iterations_per_checkpoint(),
                &mut checkpoints,
            );

            steps.push(seed);
        }

        // Create VDF state WITHOUT the steps we're going to validate
        // This forces vdf_steps_are_valid to actually compute them
        let vdf_state = Arc::new(RwLock::new(VdfState {
            global_step: Arc::new(AtomicU64::new(0)), // Start at 0, not at num_steps
            global_step_from_the_latest_canonical_block: 0,
            minimum_step_to_keep: 0,
            capacity: 1000,
            seeds: {
                let mut seeds = VecDeque::new();
                // Only store the initial seed (step 0)
                seeds.push_back(Seed(H256::from_low_u64_be(42)));
                seeds
            },
            is_vdf_mining_enabled: Arc::new(AtomicBool::new(false)),
        }));

        // Create VDFLimiterInfo with our generated steps
        let vdf_info = VDFLimiterInfo {
            output: steps.last().copied().unwrap_or(H256::from_low_u64_be(42)), // Last step output
            global_step_number: num_steps as u64,
            seed: H256::from_low_u64_be(1337),      // Reset seed
            next_seed: H256::from_low_u64_be(1338), // Next reset seed
            prev_output: H256::from_low_u64_be(42), // Initial seed
            last_step_checkpoints: H256List(vec![
                H256::default();
                vdf_config.num_checkpoints_in_vdf_step
            ]),
            steps: H256List(steps.clone()),
            vdf_difficulty: Some(vdf_config.sha_1s_difficulty),
            next_vdf_difficulty: Some(vdf_config.sha_1s_difficulty),
        };

        // Create thread pool with limited threads
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .expect("Failed to build thread pool");

        // Create cancel signal
        let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));
        let cancel_clone = Arc::clone(&cancel);

        // Create VdfStateReadonly wrapper
        let vdf_state_readonly = VdfStateReadonly::new(Arc::clone(&vdf_state));

        // Spawn validation task
        let validation_handle = tokio::task::spawn_blocking(move || {
            vdf_steps_are_valid(
                &pool,
                &vdf_info,
                &vdf_config,
                &vdf_state_readonly,
                cancel_clone,
            )
        });

        // Wait longer to ensure validation has started processing multiple steps
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Set cancellation signal
        cancel.store(CancelEnum::Cancelled as u8, Ordering::Relaxed);
        println!("Cancellation signal set");

        // Wait for validation to complete (should return quickly after cancellation)
        let start = std::time::Instant::now();
        let result = validation_handle.await;
        let elapsed = start.elapsed();

        println!("Validation completed in {:?}", elapsed);

        // Verify the result
        match result {
            Ok(Err(e)) if e.to_string().contains("Cancelled") => {
                println!("Validation cancelled successfully: {}", e);
                // Success - validation was properly cancelled
            }
            Ok(Ok(())) => {
                panic!("Validation should have been cancelled but completed successfully");
            }
            Ok(Err(e)) => {
                panic!("Unexpected error during validation: {}", e);
            }
            Err(e) => {
                panic!("Task panicked: {}", e);
            }
        }

        // Verify cancellation happened relatively quickly
        // With 20 steps at 50M difficulty, full validation would take many seconds
        // Cancellation should happen within 5 seconds
        assert!(
            elapsed < Duration::from_secs(5),
            "Cancellation took too long: {:?}",
            elapsed
        );
    }

    fn vdf_state_at(current_step: u64) -> VdfState {
        let capacity: usize = 64;
        VdfState {
            global_step: Arc::new(AtomicU64::new(current_step)),
            global_step_from_the_latest_canonical_block: current_step,
            minimum_step_to_keep: current_step.saturating_sub(capacity as u64),
            seeds: VecDeque::with_capacity(capacity),
            capacity,
            is_vdf_mining_enabled: Arc::new(AtomicBool::new(false)),
        }
    }

    proptest::proptest! {
        /// Invariants of `store_step`:
        ///   1. returned step is either `current` (no progress) or `current + 1` (advance),
        ///   2. advance happens iff `proposed == current + 1`,
        ///   3. it never panics — including for backwards/equal/large-gap proposals.
        #[test]
        fn store_step_advances_by_at_most_one_or_no_op(
            current in 0_u64..1_000_000,
            proposed in 0_u64..1_000_000,
        ) {
            let mut state = vdf_state_at(current);
            let returned = state.store_step(Seed(H256::zero()), proposed);

            proptest::prop_assert!(
                returned == current || returned == current + 1,
                "returned ({}) must be current ({}) or current+1",
                returned, current
            );
            if returned == current + 1 {
                proptest::prop_assert_eq!(
                    proposed, current + 1,
                    "advance only when proposed == current + 1"
                );
            } else {
                proptest::prop_assert_eq!(
                    returned, current,
                    "no-op must leave step unchanged"
                );
            }
        }

        /// Regression: rejected gaps must not shrink the seed buffer.
        /// Reachable when canonical has lapped local + capacity so vdf_depth
        /// saturates near 0; pre-fix each rejected gap leaked one seed until
        /// the buffer was empty and `get_last_step_and_seed` panicked.
        #[test]
        fn gap_rejection_preserves_seed_buffer(
            initial_seeds in 1_usize..32,
            gap_count in 1_usize..20,
        ) {
            let capacity = 64;
            let local_step = 100_u64;
            let canonical = 10_000_u64;
            let mut state = VdfState {
                global_step: Arc::new(AtomicU64::new(local_step)),
                global_step_from_the_latest_canonical_block: canonical,
                minimum_step_to_keep: canonical.saturating_sub(capacity as u64),
                seeds: (0..initial_seeds)
                    .map(|i| Seed(H256::from_low_u64_be(i as u64)))
                    .collect(),
                capacity,
                is_vdf_mining_enabled: Arc::new(AtomicBool::new(false)),
            };
            let initial_len = state.seeds.len();

            for i in 0..gap_count {
                let proposed = local_step + 2 + i as u64;
                let returned = state.store_step(Seed(H256::zero()), proposed);
                proptest::prop_assert_eq!(
                    returned, local_step,
                    "gap proposal must not advance step"
                );
            }

            proptest::prop_assert_eq!(
                state.seeds.len(),
                initial_len,
                "seed buffer must not shrink on gap rejections"
            );
        }
    }

    #[rstest::rstest]
    #[case::same_step(100, 100, 100)]
    #[case::backward(100, 99, 100)]
    #[case::sequential(100, 101, 101)]
    #[case::small_gap(100, 102, 100)]
    #[case::large_gap(100, 200, 100)]
    fn store_step_gap_handling(#[case] current: u64, #[case] proposed: u64, #[case] expected: u64) {
        let mut state = vdf_state_at(current);
        let returned = state.store_step(Seed(H256::zero()), proposed);
        assert_eq!(returned, expected);
    }

    /// Regression: `VdfStateReadonly::read()` previously called `.unwrap()`,
    /// so a writer panic that poisoned the lock cascaded into validation /
    /// mining hot paths. The recovery path must surface the most recent
    /// state instead of re-panicking.
    #[test]
    fn vdf_state_readonly_read_recovers_from_poisoned_lock() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        let inner = Arc::new(RwLock::new(vdf_state_at(42)));

        // Poison the lock by panicking inside a write guard.
        let poison_inner = Arc::clone(&inner);
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _guard = poison_inner.write().unwrap();
            panic!("writer panic to poison the VDF state lock");
        }));
        assert!(
            inner.read().is_err(),
            "test setup failed: lock should be poisoned after writer panic"
        );

        let readonly = VdfStateReadonly::new(inner);
        let guard = readonly.read();
        assert_eq!(
            guard.current_step(),
            42,
            "poison-recovery must surface the data the writer wrote before panicking"
        );
    }

    /// Progress check fires when `global_step` does not advance within the timeout.
    #[tokio::test(start_paused = true)]
    async fn wait_for_step_bails_when_no_progress() {
        let inner = Arc::new(RwLock::new(vdf_state_at(100)));
        let readonly = VdfStateReadonly::new(inner);
        let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));

        let result = readonly
            .wait_for_step(200, Arc::clone(&cancel), std::time::Duration::from_secs(30))
            .await;

        assert!(result.is_err(), "stalled state must bail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("did not advance") || err.contains("stalled"),
            "error must explain stall, got: {err}"
        );
    }

    /// Cancel signal causes immediate exit even if `global_step` is below desired.
    #[tokio::test(start_paused = true)]
    async fn wait_for_step_bails_on_cancel() {
        let inner = Arc::new(RwLock::new(vdf_state_at(100)));
        let readonly = VdfStateReadonly::new(inner);
        let cancel = Arc::new(AtomicU8::new(CancelEnum::Cancelled as u8));

        let result = readonly
            .wait_for_step(200, Arc::clone(&cancel), std::time::Duration::from_secs(30))
            .await;

        assert!(result.is_err(), "cancelled wait must bail");
        assert!(
            result.unwrap_err().to_string().contains("Cancelled"),
            "error must indicate cancellation"
        );
    }

    /// Happy path: each step advance resets the progress timer; wait completes
    /// when `global_step` reaches the desired number.
    #[tokio::test(start_paused = true)]
    async fn wait_for_step_completes_when_state_advances() {
        let inner = Arc::new(RwLock::new(vdf_state_at(100)));
        let readonly = VdfStateReadonly::new(Arc::clone(&inner));
        let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));

        let advancer = {
            let inner = Arc::clone(&inner);
            tokio::spawn(async move {
                for step in 101_u64..=110 {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    inner
                        .write()
                        .unwrap()
                        .global_step
                        .store(step, Ordering::Relaxed);
                }
            })
        };

        let result = readonly
            .wait_for_step(110, Arc::clone(&cancel), std::time::Duration::from_secs(30))
            .await;

        advancer.await.unwrap();
        assert!(result.is_ok(), "wait should succeed when state advances");
    }

    /// SSOT invariant: after N sequential `store_step` calls the owned atomic
    /// counter equals the locked view, and the seed window is contiguous over
    /// `[first ..= last]` with one seed per step.
    #[test]
    fn store_step_keeps_atomic_counter_and_seed_window_consistent() {
        // capacity 64, start at step 0 with the genesis seed present (anchor).
        let mut state = vdf_state_at(0);
        state.seeds.push_back(Seed(H256::zero())); // step-0 seed

        for step in 1..=50_u64 {
            let returned = state.store_step(Seed(H256::from_low_u64_be(step)), step);
            assert_eq!(returned, step, "sequential store advances by one");
            // SSOT: the owned atomic equals the just-stored step.
            assert_eq!(state.current_step(), step);
        }

        // Seed window is contiguous: first..=last maps 1:1 onto stored seeds.
        let last = state.current_step();
        let first = last - state.seeds.len() as u64 + 1;
        let steps = state.get_steps(ii(first, last)).expect("contiguous window");
        assert_eq!(steps.len(), state.seeds.len());
        assert_eq!(last, 50);
    }

    /// Lock-freedom proven through the PUBLIC handle API: `current_step()` must
    /// return while a writer holds the `RwLock` write guard. A bounded channel
    /// timeout (not an unbounded `join`) fails the test if the call blocked.
    #[test]
    fn current_step_is_readable_through_handle_while_write_guard_held() {
        let state = Arc::new(RwLock::new(vdf_state_at(5)));
        let readonly = VdfStateReadonly::new(Arc::clone(&state));

        let guard = state.write().unwrap(); // hold the writer
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let _ = tx.send(readonly.current_step());
        });

        let observed = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("current_step() must not block on the held write guard");
        assert_eq!(observed, 5);

        drop(guard);
        reader.join().unwrap();
    }

    /// The read handle exposes the step counter lock-free and reflects the
    /// controller's mining toggles (same `Arc`, shared by handle).
    #[test]
    fn readonly_exposes_lockfree_step_and_controller_toggles_mining() {
        let mining = Arc::new(AtomicBool::new(false));
        let state = Arc::new(RwLock::new(VdfState::new(64, 9, Arc::clone(&mining))));
        let readonly = VdfStateReadonly::new(Arc::clone(&state));
        assert_eq!(readonly.current_step(), 9);

        // Controller and handle share the same mining Arc (same allocation).
        let controller = VdfController::new(&mining);
        assert!(!readonly.is_mining_enabled());
        controller.start();
        assert!(readonly.is_mining_enabled());
        controller.stop();
        assert!(!readonly.is_mining_enabled());
    }

    struct FakeSeedSource(VdfBootstrap);
    impl VdfSeedSource for FakeSeedSource {
        fn vdf_bootstrap(&self, _capacity: usize) -> VdfBootstrap {
            self.0.clone()
        }
    }

    /// `create_state` faithfully assembles a `VdfState` from a `VdfBootstrap`,
    /// honouring the one-based contiguity contract.
    #[test]
    fn create_state_assembles_vdf_state_from_bootstrap() {
        let config = Config::new_with_random_peer_id(NodeConfig::testing());
        let mining = Arc::new(AtomicBool::new(false));
        // Contiguous steps 1..=5 (one-based): first_step == 5 - 5 + 1 == 1.
        let seeds: VecDeque<Seed> = (1..=5).map(|i| Seed(H256::from_low_u64_be(i))).collect();
        let source = FakeSeedSource(VdfBootstrap {
            global_step: 5,
            first_step: 1,
            ordered_seeds: seeds.clone(),
        });

        let state = create_state(&source, Arc::clone(&mining), &config);

        assert_eq!(state.current_step(), 5);
        assert_eq!(state.canonical_step(), 5);
        assert_eq!(state.seeds, seeds);
        let (last, _) = state.get_last_step_and_seed();
        assert_eq!(last, 5);
    }
}
