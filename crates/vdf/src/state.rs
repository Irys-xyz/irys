use crate::{apply_reset_seed, step_number_to_salt_number, vdf_sha, warn_mismatches};
use eyre::{bail, eyre};
use irys_database::block_header_by_hash;
use irys_domain::BlockIndex;
use irys_efficient_sampling::num_recall_ranges_in_partition;
use irys_types::{
    AtomicVdfStepNumber, Config, DatabaseProvider, H256, H256List, IrysBlockHeader, U256,
    VDFLimiterInfo, VdfConfig, block_production::Seed,
};
use nodit::{InclusiveInterval as _, Interval, interval::ii};
use rayon::prelude::*;
use reth_db::Database as _;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
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

#[derive(Debug, Clone, Default)]
pub struct VdfState {
    /// last global step stored
    pub global_step: u64,
    /// maximum number of seeds to store in seeds VecDeque
    pub capacity: usize,
    /// stored seeds
    pub seeds: VecDeque<Seed>,
    /// whether the VDF thread is mining or paused
    pub is_vdf_mining_enabled: Option<Arc<AtomicBool>>,
    /// global step from the latest canonical block
    global_step_from_the_latest_canonical_block: u64,
    /// minimum global step to keep in the seeds VecDeque
    minimum_step_to_keep: u64,
}

impl VdfState {
    /// Construct from a pre-built seed buffer (oldest step at the front, newest at the back),
    /// anchored at `global_step`. The canonical step and `minimum_step_to_keep` are derived from
    /// `global_step` and `capacity`; [`Self::new`] is this with an empty buffer.
    pub fn from_seeds(
        global_step: u64,
        capacity: usize,
        seeds: VecDeque<Seed>,
        is_vdf_mining_enabled: Option<Arc<AtomicBool>>,
    ) -> Self {
        Self {
            global_step,
            global_step_from_the_latest_canonical_block: global_step,
            minimum_step_to_keep: global_step.saturating_sub(capacity as u64),
            seeds,
            capacity,
            is_vdf_mining_enabled,
        }
    }

    pub fn new(
        capacity: usize,
        global_step: u64,
        is_vdf_mining_enabled: Option<Arc<AtomicBool>>,
    ) -> Self {
        Self::from_seeds(
            global_step,
            capacity,
            VecDeque::with_capacity(capacity),
            is_vdf_mining_enabled,
        )
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
            self.global_step,
            self.seeds
                .back()
                .cloned()
                .expect("To have at least the genesis step to be inserted"),
        )
    }

    pub fn store_step(&mut self, seed: Seed, global_step: u64) -> u64 {
        if self.global_step >= global_step {
            return self.global_step;
        }
        if self.global_step + 1 != global_step {
            // Gap path: previously panicked, now log and no-op so the VDF
            // loop catches up via normal stepping. Callers detect the no-op
            // by observing the returned step == self.global_step.
            //
            // pop_front MUST stay in the sequential branch below — a stale
            // pop_front here silently shrinks the seed buffer on every gap
            // until `get_last_step_and_seed` panics on an empty deque.
            error!(
                current = self.global_step,
                proposed = global_step,
                gap = global_step.saturating_sub(self.global_step + 1),
                "VDF state would have a gap; ignoring step (VDF will catch up via normal stepping)"
            );
            return self.global_step;
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
        self.global_step += 1;
        global_step
    }

    /// Called when local vdf thread generates a new step, or vdf step synced from another peer, and we want to increment vdf step state
    pub fn increment_step(&mut self, seed: Seed) -> u64 {
        let new_step = self.global_step + 1;
        self.store_step(seed, new_step);
        new_step
    }

    /// Get steps in the given global steps numbers Interval
    pub fn get_steps(&self, i: Interval<u64>) -> eyre::Result<H256List> {
        let vdf_steps_len = self.seeds.len() as u64;

        let last_global_step = self.global_step;

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

    pub fn start_mining(&self) -> eyre::Result<()> {
        self.is_vdf_mining_enabled
            .as_ref()
            .ok_or(eyre!("Mining state sender isn't set!"))?
            .store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn stop_mining(&self) -> eyre::Result<()> {
        self.is_vdf_mining_enabled
            .as_ref()
            .ok_or(eyre!("Mining state sender isn't set!"))?
            .store(false, Ordering::Relaxed);
        Ok(())
    }
}

/// Marker error returned by [`publish_reanchored_state`] when the VDF state `RwLock` is poisoned.
/// A zero-size type so the re-anchor orchestration can distinguish a terminal poisoned lock (the
/// supervisor must shut the node down) from a recoverable buffer-rebuild failure.
#[derive(Debug)]
pub struct ReanchorLockPoisoned;

/// Publish a re-anchored buffer. The (rewound) tip step is stored into `atomic_step` BEFORE the
/// buffer is swapped, both under the buffer's single write lock, so `atomic_step <=
/// buffer.global_step` holds at every instant — a reader can never observe the atomic pointing past
/// the buffer's newest step. Mirrors [`VdfState::store_step`]'s lock discipline. The fold of the
/// anchor hash is intentionally left to the caller: it is lock-free, pure-functional post-processing
/// and must not extend the hold time. Returns the published tip `(step, seed)`; `Err` only on a
/// poisoned lock.
pub fn publish_reanchored_state(
    vdf_state: &AtomicVdfState,
    atomic_step: &AtomicVdfStepNumber,
    new_state: VdfState,
) -> Result<(u64, Seed), ReanchorLockPoisoned> {
    // Read the tip BEFORE the move so the lock is held for the two stores alone, and return it so
    // the caller can derive the restart anchor.
    let tip = new_state.get_last_step_and_seed();
    let mut guard = vdf_state.write().map_err(|_| ReanchorLockPoisoned)?;
    atomic_step.store(tip.0, std::sync::atomic::Ordering::Relaxed);
    *guard = new_state;
    Ok(tip)
}

pub type AtomicVdfState = Arc<RwLock<VdfState>>;

/// Wraps the internal Arc<`RwLock`<>> to make the reference readonly
#[derive(Debug, Clone)]
pub struct VdfStateReadonly(AtomicVdfState);

impl VdfStateReadonly {
    /// Creates a new `ReadGuard` for Ledgers
    pub const fn new(state: Arc<RwLock<VdfState>>) -> Self {
        Self(state)
    }

    pub fn into_inner_cloned(&self) -> AtomicVdfState {
        self.0.clone()
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
        self.0.read().unwrap_or_else(|poisoned| {
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

        let retries_per_second = 20;
        let mut last_observed_step = self.read().global_step;
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

            let current_step = self.read().global_step;

            if current_step >= desired_step_number {
                debug!(vdf.desired_step = desired_step_number, "VDF step available");
                return Ok(());
            }

            if current_step != last_observed_step {
                // Any movement resets the stall baseline/timer — including a
                // BACKWARD jump from a partition-recovery VDF re-anchor, which
                // rebuilds the buffer at a lower `global_step`. The stall detector
                // exists to catch a DEAD writer, which leaves `global_step`
                // constant; only the re-anchor swap ever decreases it (`store_step`
                // is strictly forward-only), so resetting on a decrease responds to
                // a genuine liveness event and cannot mask the frozen-writer case.
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
            sleep(Duration::from_millis(1000 / retries_per_second as u64)).await;
        }
    }
}

/// create VDF state using the latest block in db
pub fn create_state(
    block_index: &BlockIndex,
    db: &DatabaseProvider,
    is_vdf_mining_enabled: Arc<AtomicBool>,
    config: &Config,
) -> VdfState {
    let capacity = calc_capacity(config);

    let block_hash = block_index
        .get_latest_item()
        .map(|item| item.block_hash)
        .expect("To have at least genesis block");

    let tx = db.tx().unwrap();
    let tip = block_header_by_hash(&tx, &block_hash, false)
        .unwrap()
        .unwrap();
    let global_step_number = tip.vdf_limiter_info.global_step_number;

    // Shared walk-back with the re-anchor / fork-local paths. Startup reads ancestors from the
    // DB; a missing header or DB error is fatal here (matching the historical `.unwrap()`s) via
    // the `expect` below.
    let seeds = build_vdf_seed_buffer(&tip, capacity, |hash| {
        block_header_by_hash(&tx, hash, false)?
            .ok_or_else(|| eyre!("missing header {hash} during VDF state init"))
    })
    .expect("startup VDF buffer build from DB");

    info!(
        "Initializing vdf service from block's info in step number {}",
        global_step_number
    );

    VdfState::from_seeds(
        global_step_number,
        capacity,
        seeds,
        Some(is_vdf_mining_enabled),
    )
}

/// Walk back from `tip` accumulating up to `capacity` VDF steps into a seed buffer (oldest step
/// at the front, the tip's last step at the back), resolving each ancestor header via
/// `fetch_parent`. Genesis (height 0) contributes only its first step. This is the shared
/// walk-back used by the canonical-tip re-anchor; it mirrors [`create_state`]'s inline loop.
fn build_vdf_seed_buffer(
    tip: &IrysBlockHeader,
    capacity: usize,
    mut fetch_parent: impl FnMut(&H256) -> eyre::Result<IrysBlockHeader>,
) -> eyre::Result<VecDeque<Seed>> {
    let mut seeds: VecDeque<Seed> = VecDeque::with_capacity(capacity);
    let mut steps_remaining = capacity;
    let mut block = tip.clone();
    while steps_remaining > 0 && block.height > 0 {
        // get all the steps out of the block
        for step in block.vdf_limiter_info.steps.0.iter().rev() {
            seeds.push_front(Seed(*step));
            steps_remaining -= 1;
            if steps_remaining == 0 {
                break;
            }
        }
        // Buffer full — stop before fetching the parent. The inner break only exits the
        // for-loop; falling through to fetch the parent would, when that parent is genesis,
        // trip the post-loop genesis branch and push one EXTRA seed (capacity+1). That
        // inflates seeds.len(), dragging get_steps' first_global_step one too low and
        // mis-mapping the buffer's oldest step.
        if steps_remaining == 0 {
            break;
        }
        // get the previous block
        block = fetch_parent(&block.previous_block_hash)?;
    }
    if block.height == 0 {
        // Genesis contributes only its first step. Guard the index: this runs on
        // the live VDF supervisor thread during re-anchor, so a malformed genesis
        // header must surface as a no-op rather than panicking the node.
        if let Some(first) = block.vdf_limiter_info.steps.0.first() {
            seeds.push_front(Seed(*first));
        }
    }
    Ok(seeds)
}

/// Rebuild [`VdfState`] anchored at the canonical chain TIP, for partition-recovery re-anchor.
///
/// Unlike [`create_state`] (which anchors at the block index's latest item — the LCA after a
/// partition-recovery truncation), this anchors at the new canonical tip so the rebuilt buffer
/// carries the recovered range's canonical steps, each with its OWN reset-boundary seed.
/// Anchoring at the LCA instead leaves the loop to re-cross the recovered range's reset
/// boundaries by local stepping with the canonical tip's single `next_seed`, which is wrong for
/// every intermediate boundary — diverging the buffer and re-wedging block validation. See
/// design/docs/vdf-partition-recovery-reanchor.md.
///
/// `canonical_headers` is the block tree's canonical chain (oldest cached .. tip, as returned by
/// `BlockTree::get_canonical_chain`). The recovered range is always within the block tree (a
/// reorg is only representable up to `block_tree_depth` deep), so the tip and the whole recovered
/// range are present here. Block headers are persisted to the DB only at migration, so the
/// un-migrated tail MUST come from these headers; steps deeper than the cache (needed to fill
/// `capacity`) fall back to the DB, which holds the migrated ancestors.
pub fn create_state_for_canonical_tip(
    canonical_headers: &[Arc<IrysBlockHeader>],
    db: &DatabaseProvider,
    is_vdf_mining_enabled: Arc<AtomicBool>,
    config: &Config,
) -> eyre::Result<(VdfState, H256)> {
    let capacity = calc_capacity(config);

    let cached: std::collections::HashMap<H256, Arc<IrysBlockHeader>> = canonical_headers
        .iter()
        .map(|h| (h.block_hash, Arc::clone(h)))
        .collect();
    let tip: &IrysBlockHeader = canonical_headers
        .last()
        .ok_or_else(|| eyre!("canonical chain empty during VDF re-anchor"))?;
    let global_step_number = tip.vdf_limiter_info.global_step_number;
    // Captured from the anchor (tip) block; the walk-back below only reads ancestors.
    let next_seed = tip.vdf_limiter_info.next_seed;

    // This runs on the live VDF supervisor thread (the re-anchor arm of
    // init_vdf_thread). A transient DB error or an ancestor missing from BOTH the
    // canonical cache and the DB must surface as an error — the caller keeps the
    // current buffer and retries on the next re-anchor signal — rather than
    // panicking and shutting the node down via the thread's CancelOnDrop guard.
    let tx = db
        .tx()
        .map_err(|e| eyre!("VDF re-anchor: opening db tx failed: {e}"))?;
    // Resolve an ancestor header by hash: the block tree's canonical cache first (the recovered,
    // un-migrated range), then the DB (migrated ancestors below the cache).
    let seeds = build_vdf_seed_buffer(tip, capacity, |hash| {
        if let Some(header) = cached.get(hash) {
            return Ok((**header).clone());
        }
        block_header_by_hash(&tx, hash, false)
            .map_err(|e| eyre!("VDF re-anchor: header lookup failed for {hash}: {e}"))?
            .ok_or_else(|| eyre!("VDF re-anchor: missing header for {hash}"))
    })?;

    info!(
        "Re-anchoring vdf service to canonical tip at step number {}",
        global_step_number
    );

    Ok((
        VdfState::from_seeds(
            global_step_number,
            capacity,
            seeds,
            Some(is_vdf_mining_enabled),
        ),
        next_seed,
    ))
}

/// Build a TRANSIENT, read-only VDF step view anchored on `tip`'s OWN lineage — its
/// `vdf_limiter_info.steps` plus ancestors resolved via `fetch_parent` (oldest at the front, the
/// tip's last step at the back), filling up to `capacity` steps. Mirrors the walk-back of
/// [`create_state_for_canonical_tip`] / [`build_vdf_seed_buffer`].
///
/// This is the single place that resolves FORK-LOCAL VDF steps for block validation. A node
/// validating a block on a competing fork (deep partition recovery) must not trust its live VDF
/// buffer, which may hold a different (poisoned) lineage past a reset boundary — that buffer would
/// make validation reject the canonical fork it must adopt (the partition-recovery wedge). Callers
/// build a view over just the steps a validation stage needs (e.g. a recall-range window back to
/// the reset boundary, a handful of blocks) from the block's own ancestors in the block tree, then
/// hand it to the existing consumers (which take `&VdfStateReadonly`) unchanged. The view never
/// touches the live buffer, so mining and the #1447 confirmation gate are unaffected.
pub fn build_fork_local_view(
    tip: &IrysBlockHeader,
    capacity: usize,
    fetch_parent: impl FnMut(&H256) -> eyre::Result<IrysBlockHeader>,
) -> eyre::Result<VdfStateReadonly> {
    let seeds = build_vdf_seed_buffer(tip, capacity, fetch_parent)?;
    let global_step = tip.vdf_limiter_info.global_step_number;
    Ok(VdfStateReadonly::new(Arc::new(RwLock::new(
        VdfState::from_seeds(global_step, capacity, seeds, None),
    ))))
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
        // Fast-path MATCH: the range is known locally AND identical, so these steps are
        // already proven consistent with this node's lineage — accept without recomputing.
        Ok(steps) if steps.0.as_slice() == batch_steps => {
            tracing::debug!(
                vdf.batch_start = batch_start_step_number,
                vdf.batch_end = batch_end_step_number,
                "Validating VDF steps from VdfStepsReadGuard!"
            );
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
        // MISMATCH against the local buffer is NOT proof of invalidity: a competing fork
        // legitimately diverges at/after a VDF reset boundary (its rotation block pins a
        // different `next_seed`), and the local buffer reflects THIS node's lineage — it is
        // not authoritative over another fork's steps. Fall through to the authoritative
        // parallel recompute below, which derives every step from the block's OWN
        // prev_output/steps/seed and still rejects a genuinely invalid chain. This closes the
        // partition-recovery wedge: a node whose buffer free-ran past a reset boundary on a
        // minority seed would otherwise reject the canonical chain's boundary-crossing blocks
        // here, so the deep reorg — and the VDF re-anchor it triggers — could never fire. The
        // #1447 run-ahead seed-poisoning protection is the mining-loop confirmed-step gate
        // (`is_reset_boundary_blocked`), NOT this validation fast path, so recomputing here
        // does not weaken it.
        Ok(steps) => {
            let expected = H256List(batch_steps.to_vec());
            warn_mismatches(&steps, &expected);
            tracing::debug!(
                vdf.batch_start = batch_start_step_number,
                vdf.batch_end = batch_end_step_number,
                "VDF batch mismatches local buffer; recomputing from the block's own seed (competing fork or poisoned buffer)"
            );
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
            global_step: 0,
            global_step_from_the_latest_canonical_block: 0,
            minimum_step_to_keep: 0,
            capacity,
            seeds: VecDeque::default(),
            is_vdf_mining_enabled: Some(is_vdf_mining_enabled),
        };
        Arc::new(RwLock::new(state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{Config, H256List, NodeConfig, VDFLimiterInfo};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
    use std::time::Duration;

    /// Regression for the partition-recovery re-anchor fix (review Finding #1):
    /// [`build_vdf_seed_buffer`] — the walk-back behind [`create_state_for_canonical_tip`] — must
    /// reproduce the canonical chain's recorded steps VERBATIM, anchored at the tip. That is what
    /// lets the re-anchored buffer match (and validate) the canonical chain across the recovered
    /// range. The broken design re-anchored at the LCA and let the loop re-derive the post-LCA
    /// steps with the wrong reset seed; rebuilding from the canonical headers instead copies each
    /// block's own steps, which already encode the correct boundary crossings.
    #[test]
    fn build_vdf_seed_buffer_reproduces_canonical_steps_anchored_at_tip() {
        fn mock_header(height: u64, hash: u8, prev: u8, steps: &[u8]) -> IrysBlockHeader {
            let mut h = IrysBlockHeader::new_mock_header();
            h.height = height;
            h.block_hash = H256::repeat_byte(hash);
            h.previous_block_hash = H256::repeat_byte(prev);
            h.vdf_limiter_info.global_step_number = height; // unused by the walk-back
            h.vdf_limiter_info.steps =
                H256List(steps.iter().map(|b| H256::repeat_byte(*b)).collect());
            h
        }

        // Canonical chain: genesis(0x00) <- b1(0x01) <- tip(0x02), each carrying its own steps.
        let genesis = mock_header(0, 0x00, 0xFF, &[0x10]);
        let b1 = mock_header(1, 0x01, 0x00, &[0x11, 0x12]);
        let tip = mock_header(2, 0x02, 0x01, &[0x13, 0x14]);
        let by_hash: std::collections::HashMap<H256, IrysBlockHeader> = [genesis, b1, tip.clone()]
            .into_iter()
            .map(|h| (h.block_hash, h))
            .collect();

        let seeds = build_vdf_seed_buffer(&tip, 1000, |hash| {
            Ok(by_hash
                .get(hash)
                .cloned()
                .expect("canonical ancestor present"))
        })
        .expect("walk-back over present canonical headers must succeed");

        // Oldest at the front; genesis contributes only its first step; b1 and tip contribute all
        // of theirs; the back is the canonical TIP's last step (anchored at the tip, not the LCA).
        let got: Vec<H256> = seeds.iter().map(|s| s.0).collect();
        let expected: Vec<H256> = [0x10, 0x11, 0x12, 0x13, 0x14]
            .iter()
            .map(|b| H256::repeat_byte(*b))
            .collect();
        assert_eq!(
            got, expected,
            "re-anchor rebuild must reproduce the canonical chain's steps verbatim"
        );
        assert_eq!(
            seeds.back().map(|s| s.0),
            Some(H256::repeat_byte(0x14)),
            "buffer must be anchored at the canonical TIP's last step (the re-anchor-to-tip fix)"
        );
    }

    /// Regression: when `capacity` is exhausted exactly while consuming the height-1 block's
    /// steps, the walk-back must stop at exactly `capacity` seeds and NOT fall through to fetch
    /// genesis and prepend its first step (which produced a capacity+1 buffer). The extra front
    /// seed inflates `seeds.len()`, dragging `get_steps`' `first_global_step` one too low and
    /// mis-mapping the buffer's oldest step onto genesis entropy.
    #[test]
    fn build_vdf_seed_buffer_stops_at_capacity_without_appending_genesis() {
        fn mock_header(height: u64, hash: u8, prev: u8, steps: &[u8]) -> IrysBlockHeader {
            let mut h = IrysBlockHeader::new_mock_header();
            h.height = height;
            h.block_hash = H256::repeat_byte(hash);
            h.previous_block_hash = H256::repeat_byte(prev);
            h.vdf_limiter_info.steps =
                H256List(steps.iter().map(|b| H256::repeat_byte(*b)).collect());
            h
        }

        // Same chain as the verbatim-reproduction test: genesis(0x10) <- b1(0x11,0x12) <- tip(0x13,0x14).
        let genesis = mock_header(0, 0x00, 0xFF, &[0x10]);
        let b1 = mock_header(1, 0x01, 0x00, &[0x11, 0x12]);
        let tip = mock_header(2, 0x02, 0x01, &[0x13, 0x14]);
        let by_hash: std::collections::HashMap<H256, IrysBlockHeader> = [genesis, b1, tip.clone()]
            .into_iter()
            .map(|h| (h.block_hash, h))
            .collect();

        // capacity=3 is reached while consuming b1 (whose parent IS genesis), exercising the
        // off-by-one path. Want the 3 most-recent steps; genesis's 0x10 must NOT appear.
        let seeds = build_vdf_seed_buffer(&tip, 3, |hash| {
            Ok(by_hash.get(hash).cloned().expect("ancestor present"))
        })
        .expect("walk-back must succeed");

        let got: Vec<H256> = seeds.iter().map(|s| s.0).collect();
        let expected: Vec<H256> = [0x12, 0x13, 0x14]
            .iter()
            .map(|b| H256::repeat_byte(*b))
            .collect();
        assert_eq!(
            got, expected,
            "buffer must hold exactly the `capacity` most-recent steps, not capacity+1 with genesis prepended"
        );
        assert_eq!(
            seeds.len(),
            3,
            "buffer length must equal capacity, not capacity+1"
        );
        assert!(
            !seeds.iter().any(|s| s.0 == H256::repeat_byte(0x10)),
            "genesis's first step must not be appended once capacity is already filled"
        );
    }

    /// Finding #3: the re-anchor walk-back runs on the live VDF supervisor thread, so an
    /// ancestor missing from both the block-tree cache and the DB (or a transient DB error)
    /// must PROPAGATE as an error — letting the supervisor keep the old buffer and retry —
    /// rather than panicking and shutting the node down.
    #[test]
    fn build_vdf_seed_buffer_propagates_fetch_error() {
        let mut tip = IrysBlockHeader::new_mock_header();
        tip.height = 2;
        tip.block_hash = H256::repeat_byte(0x02);
        tip.previous_block_hash = H256::repeat_byte(0x01);
        tip.vdf_limiter_info.steps = H256List(vec![H256::repeat_byte(0x13)]);

        // capacity is large, so the walk-back proceeds past the tip into the parent fetch,
        // which fails — the error must surface instead of unwinding through a panic.
        let result = build_vdf_seed_buffer(&tip, 1000, |_hash| {
            Err(eyre!("ancestor missing from cache and DB"))
        });

        assert!(
            result.is_err(),
            "a failed ancestor fetch must propagate as an error, not panic the VDF thread"
        );
    }

    /// Partition-recovery wedge fix: `vdf_step_batch_is_valid` must RECOMPUTE (from the block's own
    /// seed) when the local buffer MISMATCHES, not reject. A competing fork legitimately has
    /// different steps at the same step-numbers past a reset boundary, so a node whose buffer is
    /// poisoned with a minority lineage must still ACCEPT an honest canonical block (otherwise the
    /// deep reorg — and the VDF re-anchor it triggers — can never fire: the wedge). A genuinely
    /// FORGED block must still be rejected by the recompute.
    #[test]
    fn vdf_step_batch_recomputes_on_buffer_mismatch() {
        let mut node_config = NodeConfig::testing();
        node_config.consensus.get_mut().vdf.sha_1s_difficulty = 1;
        node_config
            .consensus
            .get_mut()
            .vdf
            .num_checkpoints_in_vdf_step = 2;
        node_config.consensus.get_mut().vdf.reset_frequency = 4;
        let config = Config::new_with_random_peer_id(node_config);
        let vdf_config = config.vdf.clone();

        let num_steps = 10_usize;
        let prev_output = H256::from_low_u64_be(42);

        // Build a VALID VDF chain exactly as `vdf_step_batch_is_valid`'s recompute derives it:
        // step i (batch index i, previous_step_number = i) is vdf_sha(salt(i), seed), where seed is
        // the previous step (or prev_output for i==0), with `reset_seed` folded in at multiples of
        // reset_frequency. Returns the steps and the LAST step's checkpoints.
        let build_chain = |reset_seed: H256| -> (Vec<H256>, Vec<H256>) {
            let mut steps: Vec<H256> = Vec::with_capacity(num_steps);
            let mut last_checkpoints = Vec::new();
            for i in 0..num_steps {
                let mut seed = if i == 0 { prev_output } else { steps[i - 1] };
                if i > 0 && (i as u64).is_multiple_of(vdf_config.reset_frequency as u64) {
                    seed = apply_reset_seed(seed, reset_seed);
                }
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
                last_checkpoints = checkpoints;
            }
            (steps, last_checkpoints)
        };

        let reset_seed = H256::from_low_u64_be(1337);
        let (canonical_steps, canonical_checkpoints) = build_chain(reset_seed);
        // A losing fork's lineage: same step-numbers, a DIFFERENT reset seed → diverges after the
        // first reset boundary. This is what poisons a recovering node's buffer.
        let (poisoned_steps, _) = build_chain(H256::from_low_u64_be(0xBAD));
        assert_ne!(
            canonical_steps, poisoned_steps,
            "the two reset seeds must diverge the chains"
        );

        let canonical_block = VDFLimiterInfo {
            output: *canonical_steps.last().unwrap(),
            global_step_number: num_steps as u64,
            seed: reset_seed,
            next_seed: H256::zero(),
            prev_output,
            last_step_checkpoints: H256List(canonical_checkpoints),
            steps: H256List(canonical_steps.clone()),
            vdf_difficulty: Some(vdf_config.sha_1s_difficulty),
            next_vdf_difficulty: Some(vdf_config.sha_1s_difficulty),
        };

        // Buffer poisoned with the losing fork's steps over the block's exact step range.
        let poisoned_guard = VdfStateReadonly::new(Arc::new(RwLock::new(VdfState {
            global_step: num_steps as u64,
            global_step_from_the_latest_canonical_block: num_steps as u64,
            minimum_step_to_keep: 0,
            capacity: 1000,
            seeds: poisoned_steps.iter().map(|s| Seed(*s)).collect(),
            is_vdf_mining_enabled: None,
        })));
        // Sanity: the buffer genuinely mismatches the block over its range → exercises the
        // recompute fall-through, not the fast-path match.
        let buffered = poisoned_guard.get_steps(ii(1, num_steps as u64)).unwrap();
        assert_ne!(
            buffered.0, canonical_block.steps.0,
            "poisoned buffer must mismatch the canonical block's steps"
        );

        let pool = crate::build_verification_pool(&vdf_config);

        // THE FIX: the poisoned buffer ACCEPTS the honest canonical block via recompute.
        let accepted = vdf_steps_are_valid(
            &pool,
            &canonical_block,
            &vdf_config,
            &poisoned_guard,
            Arc::new(AtomicU8::new(CancelEnum::Continue as u8)),
        );
        assert!(
            accepted.is_ok(),
            "poisoned buffer must ACCEPT the honest canonical block via recompute (wedge fix): {accepted:?}"
        );

        // A genuinely FORGED chain (one tampered step) is still rejected by the recompute.
        let mut forged_steps = canonical_steps;
        forged_steps[num_steps / 2] = H256::repeat_byte(0xFF);
        let forged_block = VDFLimiterInfo {
            steps: H256List(forged_steps),
            ..canonical_block
        };
        let forged = vdf_steps_are_valid(
            &pool,
            &forged_block,
            &vdf_config,
            &poisoned_guard,
            Arc::new(AtomicU8::new(CancelEnum::Continue as u8)),
        );
        assert!(
            forged.is_err(),
            "a forged VDF chain must still be REJECTED by the recompute"
        );
    }

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
            global_step: 0, // Start at 0, not at num_steps
            global_step_from_the_latest_canonical_block: 0,
            minimum_step_to_keep: 0,
            capacity: 1000,
            seeds: {
                let mut seeds = VecDeque::new();
                // Only store the initial seed (step 0)
                seeds.push_back(Seed(H256::from_low_u64_be(42)));
                seeds
            },
            is_vdf_mining_enabled: None,
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
            global_step: current_step,
            global_step_from_the_latest_canonical_block: current_step,
            minimum_step_to_keep: current_step.saturating_sub(capacity as u64),
            seeds: VecDeque::with_capacity(capacity),
            capacity,
            is_vdf_mining_enabled: None,
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
                global_step: local_step,
                global_step_from_the_latest_canonical_block: canonical,
                minimum_step_to_keep: canonical.saturating_sub(capacity as u64),
                seeds: (0..initial_seeds)
                    .map(|i| Seed(H256::from_low_u64_be(i as u64)))
                    .collect(),
                capacity,
                is_vdf_mining_enabled: None,
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

    /// The publish invariant: `publish_reanchored_state` stores the rewound tip into the atomic and
    /// swaps the buffer under one write lock, leaving `atomic == buffer.global_step == tip.step`. A
    /// re-anchor is always a rewind, so the published step is below the live buffer's free-run tip.
    #[test]
    fn publish_reanchored_state_rewinds_atomic_and_buffer() {
        // Live buffer far ahead (the loop free-ran above the canonical chain).
        let live = Arc::new(RwLock::new(vdf_state_at(9_999)));
        let atomic = Arc::new(AtomicU64::new(9_999));

        // Rebuilt (canonical-tip) buffer at a lower step, carrying its own tip seed.
        let tip_seed = Seed(H256::repeat_byte(0x42));
        let mut rebuilt = vdf_state_at(100);
        rebuilt.seeds.push_back(tip_seed.clone());

        let (step, seed) =
            publish_reanchored_state(&live, &atomic, rebuilt).expect("healthy lock must publish");

        assert_eq!(step, 100, "returned tip step is the rebuilt step");
        assert_eq!(
            seed, tip_seed,
            "returned tip seed is the rebuilt buffer's back"
        );
        assert_eq!(
            atomic.load(Ordering::Relaxed),
            100,
            "atomic must be rewound to the published step"
        );
        let guard = live.read().expect("lock not poisoned");
        assert_eq!(guard.global_step, 100, "buffer must hold the rebuilt step");
        assert_eq!(
            guard.seeds.back().cloned(),
            Some(tip_seed),
            "buffer must hold the rebuilt contents"
        );
    }

    /// A poisoned VDF write lock surfaces as `Err(ReanchorLockPoisoned)` rather than a panic, and
    /// leaves the atomic untouched (the store happens only after the lock is acquired).
    #[test]
    fn publish_reanchored_state_reports_poisoned_lock() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        let live = Arc::new(RwLock::new(vdf_state_at(9_999)));
        let atomic = Arc::new(AtomicU64::new(9_999));

        let poison = Arc::clone(&live);
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _guard = poison.write().unwrap();
            panic!("writer panic to poison the VDF state lock");
        }));
        assert!(live.is_poisoned(), "test setup: lock must be poisoned");

        let mut rebuilt = vdf_state_at(100);
        rebuilt.seeds.push_back(Seed(H256::repeat_byte(0x42)));
        let result = publish_reanchored_state(&live, &atomic, rebuilt);

        assert!(
            matches!(result, Err(ReanchorLockPoisoned)),
            "poisoned lock must surface as ReanchorLockPoisoned, not a panic"
        );
        assert_eq!(
            atomic.load(Ordering::Relaxed),
            9_999,
            "atomic must be untouched when the lock is poisoned"
        );
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
            guard.global_step, 42,
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
                    inner.write().unwrap().global_step = step;
                }
            })
        };

        let result = readonly
            .wait_for_step(110, Arc::clone(&cancel), std::time::Duration::from_secs(30))
            .await;

        advancer.await.unwrap();
        assert!(result.is_ok(), "wait should succeed when state advances");
    }

    /// Regression for the partition-recovery re-anchor (review Finding #2, Mode A):
    /// a BACKWARD jump in `global_step` (the shared buffer rebuilt at a lower step)
    /// must reset the stall baseline/timer, not be mistaken for a dead writer.
    ///
    /// Timeline (progress_timeout = 10s): advance 100→150 at t=4s, REWIND 150→120 at
    /// t=8s, then climb to the desired 200 at t=16s. Pre-fix, the climb after the
    /// rewind never exceeds the pre-rewind `last_observed_step` (150), so
    /// `last_progress_at` (frozen at t=4s) elapses its 10s window at t=14s and the
    /// wait spuriously returns `Stalled` (which panics the validation service).
    /// Post-fix, the rewind at t=8s resets the timer, so the wait reaches 200 first.
    #[tokio::test(start_paused = true)]
    async fn wait_for_step_tolerates_backward_reanchor_jump() {
        let inner = Arc::new(RwLock::new(vdf_state_at(100)));
        let readonly = VdfStateReadonly::new(Arc::clone(&inner));
        let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));
        let progress_timeout = std::time::Duration::from_secs(10);

        let advancer = {
            let inner = Arc::clone(&inner);
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                inner.write().unwrap().global_step = 150; // forward
                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                inner.write().unwrap().global_step = 120; // re-anchor rewind (the trigger)
                tokio::time::sleep(std::time::Duration::from_secs(8)).await;
                inner.write().unwrap().global_step = 200; // climb to desired before timeout
            })
        };

        let result = readonly
            .wait_for_step(200, Arc::clone(&cancel), progress_timeout)
            .await;

        advancer.await.unwrap();
        assert!(
            result.is_ok(),
            "a backward re-anchor jump must reset the stall timer, not surface as Stalled: {result:?}"
        );
    }
}
