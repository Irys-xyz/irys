# VDF Validation Progress-Check Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound the VDF validation pipeline against silent VDF-state stalls by replacing two unbounded polling loops with a progress-aware wait, plumbing the cancel signal through every wait, and surfacing previously-silent VDF-thread death paths.

**Architecture:** Replace the polling loops in `wait_for_step_with_cancel` (validation_service) and `VdfStateReadonly::wait_for_step` (vdf::state) with a single progress-aware implementation that tracks `(last_observed_step, last_progress_at)` and bails if `global_step` does not advance for `progress_timeout`. Site C (the post-fast-forward wait) gains a cancel signal and the same progress check. Per-stage logs in `ensure_vdf_is_valid` are promoted from trace to debug/info. `run_vdf` early-return paths gain explicit error logs.

**Tech Stack:** Rust 2024, Tokio, std::sync::RwLock, eyre, tracing, rstest, tokio::test, irys-testing-utils.

**Scope:**
- IN: progress check + cancel for both wait sites in `ensure_vdf_is_valid`; observability of `ensure_vdf_is_valid` stages and `run_vdf` early returns; config plumbing for the progress timeout; unit and integration tests.
- OUT: site B (sync `block_tree_guard.read()` in async); block_pool orphan storm (Fix 2); liveness watchdog (Fix 3); broader block_tree lock audit (other developer).

---

## Background

From `/tmp/report.md` and code investigation:

- `crates/actors/src/validation_service.rs:401-426` — local `wait_for_step_with_cancel` polls `vdf_state.read().global_step` at 20Hz with a cancel signal but no progress check.
- `crates/vdf/src/state.rs:218-232` — `VdfStateReadonly::wait_for_step` polls the same field with **no cancel signal at all** (so even shutdown can't unstick it).
- `crates/vdf/src/vdf.rs:121, 224` — `run_vdf` exits silently when `store_step` returns `None` (poison). The `error!` lives inside `store_step`, but the caller's `return` is unannotated.
- Probe 2 evidence (zero `BlockValidationFinished` events over 8.5h despite 354 schedulings) is most consistent with a hang at one of the two polling waits — both unblock only when `global_step >= desired`. If the VDF thread dies, neither poll exits.

The fix bounds *progress*, not duration. VDF waits can legitimately be long when `global_step_number - first_step_number` is large; what is never legitimate is `global_step` failing to advance for tens of seconds while a valid block expects it.

---

## File Structure

| Path | Responsibility | Action |
|---|---|---|
| `crates/types/src/config/node.rs` | `VdfNodeConfig` operational knobs | Modify — add `progress_timeout_secs: u64` field, default 30 |
| `crates/types/src/config/mod.rs` | Composite `VdfConfig` and `From<&NodeConfig>` | Modify — propagate the new field |
| `crates/vdf/src/state.rs` | `VdfStateReadonly` wait helpers | Modify — `wait_for_step` gains `cancel` + `progress_timeout`, returns `eyre::Result<()>`; add unit tests |
| `crates/actors/src/validation_service.rs` | `ensure_vdf_is_valid` and pipeline | Modify — delete local `wait_for_step_with_cancel`, route both sites through `vdf_state.wait_for_step`, promote stage logs |
| `crates/vdf/src/vdf.rs` | `run_vdf` writer loop | Modify — surface silent return paths with `error!` logs |
| `crates/chain-tests/src/multi_node/vdf_validation_progress.rs` | New integration test | Create — multi-node test exercising the progress-check path |
| `crates/chain-tests/src/multi_node/mod.rs` | Multi-node test module index | Modify — register the new test module |

---

## Task 1: Add `progress_timeout_secs` to VDF config

**Files:**
- Modify: `crates/types/src/config/node.rs:775-798`
- Modify: `crates/types/src/config/mod.rs:241-253, 289-311`

- [ ] **Step 1.1: Add `progress_timeout_secs` field to `VdfNodeConfig`**

In `crates/types/src/config/node.rs:775-798`, change the struct and default:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VdfNodeConfig {
    /// Maximum number of threads to use for parallel VDF verification
    pub parallel_verification_thread_limit: usize,

    /// Controls whether and how the VDF thread is pinned to a CPU core.
    #[serde(default)]
    pub core_pinning: CorePinning,

    /// When true, enforce a minimum step duration to prevent VDF from
    /// outrunning block production when sha_1s_difficulty is low.
    #[serde(default)]
    pub throttle: bool,

    /// Bail out of a VDF wait if the local `global_step` has not advanced for
    /// this many seconds. Detects a dead/stuck VDF thread without imposing a
    /// wall-clock cap on legitimate long waits.
    #[serde(default = "default_vdf_progress_timeout_secs")]
    pub progress_timeout_secs: u64,
}

fn default_vdf_progress_timeout_secs() -> u64 {
    30
}

impl Default for VdfNodeConfig {
    fn default() -> Self {
        Self {
            parallel_verification_thread_limit: 4,
            core_pinning: CorePinning::default(),
            throttle: false,
            progress_timeout_secs: default_vdf_progress_timeout_secs(),
        }
    }
}
```

- [ ] **Step 1.2: Add field to composite `VdfConfig` and propagate via `From<&NodeConfig>`**

In `crates/types/src/config/mod.rs:289-311`:

```rust
pub struct VdfConfig {
    pub reset_frequency: usize,
    pub parallel_verification_thread_limit: usize,
    pub num_checkpoints_in_vdf_step: usize,
    pub max_allowed_vdf_fork_steps: u64,
    pub sha_1s_difficulty: u64,
    pub throttle: bool,
    /// See `VdfNodeConfig::progress_timeout_secs`.
    pub progress_timeout_secs: u64,
}
```

In `crates/types/src/config/mod.rs:241-253`, update the `From` impl:

```rust
impl From<&NodeConfig> for VdfConfig {
    fn from(value: &NodeConfig) -> Self {
        let consensus = value.consensus_config().vdf;
        Self {
            parallel_verification_thread_limit: value.vdf.parallel_verification_thread_limit,
            reset_frequency: consensus.reset_frequency,
            num_checkpoints_in_vdf_step: consensus.num_checkpoints_in_vdf_step,
            max_allowed_vdf_fork_steps: consensus.max_allowed_vdf_fork_steps,
            sha_1s_difficulty: consensus.sha_1s_difficulty,
            throttle: value.vdf.throttle,
            progress_timeout_secs: value.vdf.progress_timeout_secs,
        }
    }
}
```

- [ ] **Step 1.3: Verify the workspace compiles**

Run: `cargo xtask check`
Expected: PASS — no callers construct `VdfConfig` or `VdfNodeConfig` literally outside the `Default`/`From` paths above (verify with `grep -rn "VdfConfig {\\|VdfNodeConfig {" --include="*.rs" crates/` and update any literal constructors found, e.g. test fixtures, to set the new field to `30`).

- [ ] **Step 1.4: Commit**

```bash
git add crates/types/src/config/node.rs crates/types/src/config/mod.rs
git commit -m "feat(vdf): add progress_timeout_secs to VdfConfig"
```

---

## Task 2: Add cancel + progress check to `VdfStateReadonly::wait_for_step` (TDD)

**Files:**
- Modify: `crates/vdf/src/state.rs:215-232` (the `wait_for_step` impl)
- Test: `crates/vdf/src/state.rs` (existing `tests` module, add 3 new tests)

- [ ] **Step 2.1: Write the failing tests**

Add these tests inside the existing `#[cfg(test)] mod tests { ... }` block in `crates/vdf/src/state.rs` (after the existing `vdf_state_readonly_read_recovers_from_poisoned_lock` test):

```rust
/// Progress check fires when `global_step` does not advance within the timeout.
#[tokio::test(start_paused = true)]
async fn wait_for_step_bails_when_no_progress() {
    let inner = Arc::new(RwLock::new(vdf_state_at(100)));
    let readonly = VdfStateReadonly::new(inner);
    let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));

    let result = readonly
        .wait_for_step(
            200,
            Arc::clone(&cancel),
            std::time::Duration::from_secs(30),
        )
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
        .wait_for_step(
            200,
            Arc::clone(&cancel),
            std::time::Duration::from_secs(30),
        )
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
        .wait_for_step(
            110,
            Arc::clone(&cancel),
            std::time::Duration::from_secs(30),
        )
        .await;

    advancer.await.unwrap();
    assert!(result.is_ok(), "wait should succeed when state advances");
}
```

- [ ] **Step 2.2: Run tests to verify they fail**

Run: `cargo nextest run -p irys-vdf wait_for_step --no-fail-fast`
Expected: FAIL with compile errors (the new signature doesn't exist yet).

- [ ] **Step 2.3: Update `wait_for_step` signature and body**

In `crates/vdf/src/state.rs:215-232`, replace the existing impl with:

```rust
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
            bail!("Cancelled");
        }

        let current_step = self.read().global_step;

        if current_step >= desired_step_number {
            debug!(vdf.desired_step = desired_step_number, "VDF step available");
            return Ok(());
        }

        if current_step > last_observed_step {
            last_observed_step = current_step;
            last_progress_at = Instant::now();
        } else if last_progress_at.elapsed() >= progress_timeout {
            bail!(
                "VDF state did not advance for {:?} (current={}, desired={})",
                progress_timeout,
                current_step,
                desired_step_number
            );
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
```

Add the imports needed (`use eyre::bail;` is already present indirectly via `eyre::eyre`; double-check the existing `use eyre::{bail, eyre};` import is in place — it currently imports only `eyre`, so update line 2 to `use eyre::{bail, eyre};`).

- [ ] **Step 2.4: Run tests to verify they pass**

Run: `cargo nextest run -p irys-vdf wait_for_step --no-fail-fast`
Expected: PASS for all three new tests.

- [ ] **Step 2.5: Update existing callers in tests of vdf.rs**

The existing test in `crates/vdf/src/vdf.rs:652` calls `vdf_steps_guard.wait_for_step(1)` without cancel/timeout. Update:

```rust
let cancel = Arc::new(AtomicU8::new(CancelEnum::Continue as u8));
tokio::time::timeout(
    Duration::from_millis(100),
    vdf_steps_guard.wait_for_step(1, cancel, Duration::from_secs(30)),
)
.await
.expect("fast-forward step should be applied promptly while syncing")
.expect("wait_for_step should not error");
```

(Add `use std::sync::atomic::AtomicU8;` and `use crate::state::CancelEnum;` to the test module if not already imported.)

Run: `cargo nextest run -p irys-vdf --no-fail-fast`
Expected: PASS.

- [ ] **Step 2.6: Commit**

```bash
git add crates/vdf/src/state.rs crates/vdf/src/vdf.rs
git commit -m "feat(vdf): add cancel + progress check to wait_for_step"
```

---

## Task 3: Consolidate site A — replace local helper with `VdfStateReadonly::wait_for_step`

**Files:**
- Modify: `crates/actors/src/validation_service.rs:401-426` (delete local helper)
- Modify: `crates/actors/src/validation_service.rs:441-445` (update call site)

- [ ] **Step 3.1: Delete the local `wait_for_step_with_cancel` helper**

Remove `crates/actors/src/validation_service.rs:401-426` entirely, including the `#[tracing::instrument(...)]` attribute on it. The new `VdfStateReadonly::wait_for_step` from Task 2 supersedes it.

- [ ] **Step 3.2: Update site A's call**

In `crates/actors/src/validation_service.rs:441-445`, replace:

```rust
        // First, wait for the previous VDF step to be available
        let first_step_number = vdf_info.first_step_number();
        let prev_output_step_number = first_step_number.saturating_sub(1);
        self.wait_for_step_with_cancel(prev_output_step_number, Arc::clone(&cancel))
            .await?;
```

with:

```rust
        // First, wait for the previous VDF step to be available
        let first_step_number = vdf_info.first_step_number();
        let prev_output_step_number = first_step_number.saturating_sub(1);
        let progress_timeout = Duration::from_secs(self.config.vdf.progress_timeout_secs);
        self.vdf_state
            .wait_for_step(
                prev_output_step_number,
                Arc::clone(&cancel),
                progress_timeout,
            )
            .await?;
```

- [ ] **Step 3.3: Verify the file compiles**

Run: `cargo check -p irys-actors`
Expected: PASS.

- [ ] **Step 3.4: Run validation_service tests**

Run: `cargo nextest run -p irys-actors validation_service`
Expected: PASS — existing tests should still pass; the local helper had no direct tests.

- [ ] **Step 3.5: Commit**

```bash
git add crates/actors/src/validation_service.rs
git commit -m "refactor(validation): route site A through VdfStateReadonly::wait_for_step"
```

---

## Task 4: Site C — pass cancel + progress timeout

**Files:**
- Modify: `crates/actors/src/validation_service.rs:498-501`

- [ ] **Step 4.1: Update site C**

In `crates/actors/src/validation_service.rs:498-501`, replace:

```rust
        // Fast forward VDF steps
        fast_forward_vdf_steps_from_block(&vdf_info, &vdf_ff)?;
        vdf_state.wait_for_step(vdf_info.global_step_number).await;
        Ok(())
```

with:

```rust
        // Fast forward VDF steps
        fast_forward_vdf_steps_from_block(&vdf_info, &vdf_ff)?;
        vdf_state
            .wait_for_step(
                vdf_info.global_step_number,
                Arc::clone(&cancel),
                progress_timeout,
            )
            .await?;
        Ok(())
```

(`progress_timeout` is the local `Duration` introduced in Task 3.2; it's still in scope here. `vdf_state` is the local clone created at line 477.)

- [ ] **Step 4.2: Verify the file compiles**

Run: `cargo check -p irys-actors`
Expected: PASS.

- [ ] **Step 4.3: Run validation_service tests**

Run: `cargo nextest run -p irys-actors validation_service`
Expected: PASS.

- [ ] **Step 4.4: Commit**

```bash
git add crates/actors/src/validation_service.rs
git commit -m "fix(validation): plumb cancel signal into site C's wait_for_step"
```

---

## Task 5: Promote per-stage logs in `ensure_vdf_is_valid`

**Files:**
- Modify: `crates/actors/src/validation_service.rs:430-502`

- [ ] **Step 5.1: Promote the function-level instrument**

Change the `#[tracing::instrument]` attribute at line 430 from `level = "trace"` to default level (debug) so spans appear in production logs. Replace:

```rust
    #[tracing::instrument(level = "trace", err, skip_all, fields(block.hash = ?block.block_hash, block.height = ?block.height))]
```

with:

```rust
    #[tracing::instrument(err, skip_all, fields(block.hash = ?block.block_hash, block.height = ?block.height))]
```

- [ ] **Step 5.2: Add explicit per-stage logs**

After the function-level instrument and inside `ensure_vdf_is_valid`, add `info!`/`debug!` markers around each blocking site. Replace lines 437-501 of the function body so it reads:

```rust
        let vdf_info = block.vdf_limiter_info.clone();
        let first_step_number = vdf_info.first_step_number();
        let prev_output_step_number = first_step_number.saturating_sub(1);
        let progress_timeout = Duration::from_secs(self.config.vdf.progress_timeout_secs);

        info!(
            vdf.first_step_number = first_step_number,
            vdf.global_step_number = vdf_info.global_step_number,
            vdf.prev_output_step_number = prev_output_step_number,
            vdf.local_step = self.vdf_state.read().global_step,
            "ensure_vdf_is_valid: entered"
        );

        // Stage A: wait for the previous VDF step to be available locally
        debug!(stage = "wait_prev_step", "ensure_vdf_is_valid: waiting for previous step");
        self.vdf_state
            .wait_for_step(
                prev_output_step_number,
                Arc::clone(&cancel),
                progress_timeout,
            )
            .await?;

        let stored_previous_step = self
            .vdf_state
            .get_step(prev_output_step_number)
            .expect("to get the step, since we've just waited for it");

        ensure!(
            stored_previous_step == vdf_info.prev_output,
            "vdf output is not equal to the saved step with the same index {:?}, got {:?}",
            stored_previous_step,
            vdf_info.prev_output,
        );

        // Stage B: validate seeds against parent (early guard before heavy VDF work)
        let vdf_reset_frequency = self.config.vdf.reset_frequency as u64;
        debug!(stage = "validate_seeds", "ensure_vdf_is_valid: validating seed data against parent");
        {
            let binding = self.block_tree_guard.read();
            let previous_block = binding
                .get_block(&block.previous_block_hash)
                .expect("previous block should exist");
            ensure!(
                matches!(
                    is_seed_data_valid(block, previous_block, vdf_reset_frequency),
                    crate::block_tree_service::ValidationResult::Valid
                ),
                "Seed data is invalid"
            );
        }

        // Stage C: VDF step validation (heavy SHA work in rayon pool)
        let vdf_ff = self.service_senders.vdf_fast_forward.clone();
        let vdf_state = self.vdf_state.clone();
        if !skip_vdf_validation {
            debug!(stage = "vdf_steps_are_valid", "ensure_vdf_is_valid: validating VDF steps");
            let vdf_info = vdf_info.clone();
            let this_inner = Arc::clone(&self);
            let cancel_clone = Arc::clone(&cancel);
            tokio::task::spawn_blocking(move || {
                vdf_steps_are_valid(
                    &this_inner.pool,
                    &vdf_info,
                    &this_inner.config.vdf,
                    &this_inner.vdf_state,
                    cancel_clone,
                )
            })
            .await??;
        } else {
            debug!(
                stage = "vdf_steps_are_valid",
                "ensure_vdf_is_valid: skipping vdf_steps_are_valid"
            );
        }

        // Stage D: fast-forward + wait for global_step to catch up
        debug!(stage = "fast_forward", "ensure_vdf_is_valid: fast-forwarding VDF steps");
        fast_forward_vdf_steps_from_block(&vdf_info, &vdf_ff)?;
        debug!(stage = "wait_global_step", "ensure_vdf_is_valid: waiting for fast-forward to complete");
        vdf_state
            .wait_for_step(
                vdf_info.global_step_number,
                Arc::clone(&cancel),
                progress_timeout,
            )
            .await?;

        info!(
            vdf.global_step_number = vdf_info.global_step_number,
            "ensure_vdf_is_valid: completed successfully"
        );
        Ok(())
```

(Note: this reuses the original `cancel` Arc; the previous code consumed it into the `spawn_blocking` closure. Cloning it before the move keeps `cancel` available for site C.)

- [ ] **Step 5.3: Verify compiles and tests pass**

Run: `cargo check -p irys-actors && cargo nextest run -p irys-actors validation_service`
Expected: PASS.

- [ ] **Step 5.4: Commit**

```bash
git add crates/actors/src/validation_service.rs
git commit -m "feat(validation): promote ensure_vdf_is_valid stage logs to debug/info"
```

---

## Task 6: Surface silent return paths in `run_vdf`

**Files:**
- Modify: `crates/vdf/src/vdf.rs:113-122, 218-226`

- [ ] **Step 6.1: Annotate the fast-forward `store_step` early return**

In `crates/vdf/src/vdf.rs:114-122`, replace:

```rust
                let Some(returned) = store_step(
                    proposed_ff_step.step,
                    &atomic_vdf_global_step,
                    &vdf_state,
                    proposed_ff_step.global_step_number,
                    canonical_global_step_number,
                ) else {
                    return;
                };
```

with:

```rust
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
                    return;
                };
```

- [ ] **Step 6.2: Annotate the local-stepping `store_step` early return**

In `crates/vdf/src/vdf.rs:218-226`, replace:

```rust
        let Some(returned) = store_step(
            hash,
            &atomic_vdf_global_step,
            &vdf_state,
            global_step_number + 1,
            canonical_global_step_number,
        ) else {
            return;
        };
```

with:

```rust
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
            return;
        };
```

- [ ] **Step 6.3: Verify compiles and tests pass**

Run: `cargo nextest run -p irys-vdf`
Expected: PASS — existing `run_vdf_returns_gracefully_on_poisoned_state_lock` still passes (it asserts no panic, not log silence).

- [ ] **Step 6.4: Commit**

```bash
git add crates/vdf/src/vdf.rs
git commit -m "feat(vdf): log explicit error when run_vdf exits via poisoned store_step"
```

---

## Task 7: Multi-node integration test — stalled-peer block validation fails fast

**Files:**
- Create: `crates/chain-tests/src/multi_node/vdf_validation_progress.rs`
- Modify: `crates/chain-tests/src/multi_node/mod.rs` — add `mod vdf_validation_progress;`

> **Implementer:** invoke the `writing-integration-tests` skill before this task. It documents the harness patterns and explains the `BlockValidationOutcome` shape, the `subscribe_block_state_updates` channel, and the `mine_blocks_without_gossip` / `gossip_block_to_peers` pattern this test relies on. Read `crates/chain-tests/src/multi_node/peer_mining.rs` for a worked two-node example.

The test exercises the following sequence:

1. Peer A is the genesis miner. Peer B joins as a trusted peer with `is_vdf_mining_enabled=false`. Both reach a common height via gossip.
2. Peer A calls `mine_blocks_without_gossip(5)` so it produces 5 blocks privately. Peer A's `global_step` advances; Peer B's does not (Peer B is not mining, no gossip arrives, no fast-forward fires).
3. Peer A calls `gossip_block_to_peers(&block_at_n_plus_5)` directly, delivering the *latest* private block to Peer B without delivering the chain in between.
4. Peer B's `block_pool` sees an orphan; we expect Peer B to attempt validation of the head block (the one we gossiped), whose `vdf_limiter_info.first_step_number` is far above Peer B's local `global_step`. Site A's wait should bail via the progress check well before any catch-up succeeds.
5. The test subscribes to Peer B's `block_state_events` and asserts a `BlockValidationOutcome::Invalid` with a `VdfValidationFailed` reason citing the progress check, within `progress_timeout_secs + ε`.

- [ ] **Step 7.1: Add the test module to the index**

Append to `crates/chain-tests/src/multi_node/mod.rs`:

```rust
mod vdf_validation_progress;
```

- [ ] **Step 7.2: Write the integration test using verified harness APIs**

Create `crates/chain-tests/src/multi_node/vdf_validation_progress.rs`:

```rust
//! Integration test: a peer whose local VDF state cannot reach a block's
//! required `global_step_number` must surface VdfValidationFailed within
//! `progress_timeout_secs` rather than hanging the validation pipeline.
//!
//! See docs/superpowers/plans/2026-05-08-vdf-validation-progress-check.md

use crate::utils::IrysNodeTest;
use irys_types::NodeConfig;
use std::time::{Duration, Instant};

const SHORT_PROGRESS_TIMEOUT_SECS: u64 = 3;

fn testing_cfg() -> NodeConfig {
    let mut cfg = NodeConfig::testing();
    cfg.vdf.progress_timeout_secs = SHORT_PROGRESS_TIMEOUT_SECS;
    cfg
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn stalled_peer_validation_fails_fast() -> eyre::Result<()> {
    let genesis_cfg = testing_cfg();
    let peer_a = IrysNodeTest::new_genesis(genesis_cfg.clone()).start().await;

    let mut peer_b_cfg = peer_a.testing_peer();
    peer_b_cfg.vdf.progress_timeout_secs = SHORT_PROGRESS_TIMEOUT_SECS;
    let peer_b = IrysNodeTest::new(peer_b_cfg).start().await;

    // Common baseline: peer A mines, peer B follows via gossip + fast-forward.
    peer_a.start_mining();
    peer_a.wait_until_height(2, 30).await?;
    peer_b.wait_until_height(2, 30).await?;

    // Freeze peer B's VDF: stop_mining() halts local stepping. Peer B will not
    // receive fast-forward steps because we use mine_blocks_without_gossip below.
    peer_b.stop_mining();

    // Peer A privately mines 5 blocks. Peer A's global_step jumps; peer B stays.
    peer_a.mine_blocks_without_gossip(5).await?;
    let private_tip_height = peer_a.get_canonical_chain_height().await;
    assert!(
        private_tip_height >= 7,
        "peer A should have advanced to at least height 7, got {private_tip_height}"
    );
    let private_tip = peer_a.get_block_by_height(private_tip_height).await?;

    // Subscribe to peer B's block-state events before delivering the block.
    let mut event_rx = peer_b
        .node_ctx
        .service_senders
        .subscribe_block_state_updates();

    // Deliver only the head block to peer B. The intervening blocks are absent,
    // so peer B has no source of fast-forward steps to bridge the step gap.
    peer_a.gossip_block_to_peers(&std::sync::Arc::new(private_tip.clone()))?;

    let target_hash = private_tip.block_hash;
    let deadline = Instant::now() + Duration::from_secs(SHORT_PROGRESS_TIMEOUT_SECS + 5);

    let mut saw_failure = false;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Ok(event)) => {
                if event.block_hash != target_hash {
                    continue;
                }
                if let irys_actors::block_tree_service::ValidationResult::Invalid(err) =
                    &event.validation_result
                {
                    let reason = err.to_string();
                    assert!(
                        reason.contains("did not advance")
                            || reason.contains("stalled")
                            || reason.contains("VdfValidationFailed"),
                        "validation failed for unexpected reason: {reason}"
                    );
                    saw_failure = true;
                    break;
                }
            }
            Ok(Err(_)) | Err(_) => break,
        }
    }
    assert!(
        saw_failure,
        "expected VdfValidationFailed for {target_hash} within {SHORT_PROGRESS_TIMEOUT_SECS}+5s"
    );

    peer_a.stop().await;
    peer_b.stop().await;
    Ok(())
}
```

> **Implementer note on the subscription API:** `service_senders.subscribe_block_state_updates()` is the working subscription pattern used by `mine_block_and_wait_for_validation` (utils.rs:1665) — verify the same call site exists at the time of implementation. `BlockStateUpdated` is defined at `crates/actors/src/block_tree_service.rs:91-97` with a `validation_result: ValidationResult` field, where `ValidationResult` is the enum at line 1100 with variants `Valid` and `Invalid(ValidationError)`.

- [ ] **Step 7.3: Run the integration test**

Run: `cargo nextest run -p chain-tests stalled_peer_validation_fails_fast --no-fail-fast`
Expected: PASS within ~10s. The test would have timed out / hung indefinitely without the progress check.

- [ ] **Step 7.4: Commit**

```bash
git add crates/chain-tests/src/multi_node/mod.rs crates/chain-tests/src/multi_node/vdf_validation_progress.rs
git commit -m "test(chain-tests): integration test for VDF progress-check"
```

---

## Task 8: Final verification

- [ ] **Step 8.1: Run local checks**

Run: `cargo xtask local-checks`
Expected: PASS — fmt, clippy, unused-deps, typos.

- [ ] **Step 8.2: Run full test suites for touched crates**

Run (in parallel as separate Bash invocations):
- `cargo nextest run -p irys-vdf`
- `cargo nextest run -p irys-actors`
- `cargo nextest run -p irys-types`
- `cargo nextest run -p chain-tests vdf_validation_progress`

Expected: all PASS.

- [ ] **Step 8.3: Manual review checklist**

- [ ] No `wait_for_step*` callsite is missing the cancel argument.
- [ ] `ensure_vdf_is_valid` reuses the same `progress_timeout: Duration` value at both wait sites (no drift).
- [ ] `cancel` is cloned before the `spawn_blocking` move so site C still has it.
- [ ] No new sync `RwLock::read()` was introduced inside an async function.
- [ ] Existing `cancel` semantics in `vdf_steps_are_valid` (rayon SHA) are unchanged.

- [ ] **Step 8.4: Push the branch**

Confirm with the user before pushing (per global instructions). Then:

```bash
git push -u origin fix/vdf-progress
```

---

## Deferred / out of scope

- **Site B** (`block_tree_guard.read()` inside async at validation_service.rs:462) — flagged but not addressed. Investigation showed it's a sync-lock-in-async antipattern but unlikely to be the root cause of the observed 8h hang. Tackle in a follow-up if profiling shows tokio-worker stalls during validation.
- **Defect 2** (block_pool orphan storm): owned by another developer.
- **Liveness watchdog (Fix 3)**: owned by another developer.
- **Full block-tree lock audit (Fix 5 expansion)**: separate audit ticket.
