# VDF Validation Stall Detection

## Status

Accepted

## Context

The validation pipeline parked indefinitely on testnet when `ensure_vdf_is_valid` waited for VDF state that never advanced. Two of the `wait_for_step` pollers had no progress check at all, and one had no cancel signal — when the VDF writer thread died silently (a poisoned `RwLock` exit from `store_step`), validation tasks took the single `vdf_scheduler.current` slot, parked on a 20Hz poll loop, and never emitted `BlockValidationFinished`. The symptom on a single peer was 354 redundant validation cycles in 4 hours with zero completions, contributing to cluster fragmentation.

The fix needs two complementary guarantees:

1. A **cooperative** path: VDF waits must time out and surface a typed error to the validation pipeline, so the block tree learns about the failure and re-prioritises.
2. A **non-cooperative** path: if the validation task itself goes off the rails (deadlock, runaway, lock holding a thread), an outside observer must force it down rather than letting it hold the single VDF lane.

The two paths use related but not identical mechanisms.

## Decision

### A single liveness budget: `vdf.progress_timeout_secs`

Introduce one config field (`crates/types/src/config/node.rs`) that gates three sites in the validation pipeline. Default 15s.

The budget is calibrated against the worst-case legitimate batch: VDF difficulty is ~1s per step per thread, `validation_batch_size` is runtime-clamped to `2 × parallel_verification_thread_limit`, and a rayon pool parallelises the batch over those threads — so each worker does ~2 steps ≈ ~2s. 15s leaves ~7× headroom; tripping it indicates a real stall, not slow hardware.

`Config::validate()` rejects a zero value, which would otherwise make every wait instantly bail.

### Cooperative: `wait_for_step` with cancel + typed errors

`VdfStateReadonly::wait_for_step` (`crates/vdf/src/state.rs`) takes `cancel: Arc<AtomicU8>` and `progress_timeout: Duration` and returns `Result<(), WaitForStepError>` with two variants kept deliberately distinct:

- `WaitForStepError::Cancelled` — the cancel atomic was set (preemption by a higher-priority block, or shutdown). `PreemptibleVdfTask::execute` maps this to `VdfValidationResult::Cancelled` and the service requeues the same task with refreshed priority.
- `WaitForStepError::Stalled` — `global_step` did not advance for `progress_timeout`. The classifier maps this to `VdfValidationResult::Invalid(e)`, surfacing to the block tree as `BlockValidationFinished(Invalid(VdfValidationFailed))`.

Collapsing the two into a single `Err` would re-introduce the original failure mode: requeuing a stalled block is wrong (local VDF state is the bottleneck, retrying won't help) and dropping it on cooperative cancel is wrong (the block is still potentially valid, just deprioritised). They must be different code paths.

### Non-cooperative: stage watchdog with process abort

`ensure_vdf_is_valid` records its progress at each stage boundary via `record_vdf_task_progress`, which writes both a `stage_signal: AtomicU8` and a `progress_instant: Mutex<Instant>`. The `ValidationService` select loop ticks every 5s and asks `VdfScheduler::abort_stalled_current(progress_timeout)`:

- It watches only the **computational** stages (`ValidateSeeds`, `ValidateBatch`, `FastForwardBatch`). The `WaitPrevStep` and `WaitFinalCatchUp` stages are excluded — those are already bounded by `wait_for_step`'s cooperative progress check, so doubling up would be redundant.
- If a watched stage's `progress_instant` is older than `progress_timeout`, the watchdog sets the cooperative cancel signal, calls `JoinHandle::abort()` on the task, then `panic!`s the validation service.

Critically, the panic is *not* handled in the loop — a process-wide panic hook converts it to a process abort, and the supervisor restarts the node clean. Marking the block Invalid (the obvious alternative) would have masked an underlying protocol-level bug as a data-level failure; the next block would re-trigger it.

`record_vdf_task_progress` is documented as "call only at real stage transitions" — adding periodic heartbeats inside a stage would silently defeat the watchdog.

### Bounded `vdf_fast_forward` channel with the same panic policy

The `vdf_fast_forward` channel was previously unbounded; under the deadlock scenario above it would have grown without bound while the validation pipeline parked. It is now bounded to 4096 messages, and `fast_forward_validated_steps` sends with `progress_timeout` as a per-step send timeout.

On timeout it panics under the same reasoning as the watchdog: `run_vdf` drains this channel fully between every ~1s SHA step, so 15s of sustained backpressure means the consumer is dead, not slow. Process-abort is the correct response.

## Consequences

- A stalled VDF state surfaces as `Invalid(VdfValidationFailed)` instead of silently parking validation. The block tree learns and acts.
- A deadlocked validation task takes the whole node down via process abort, not just the validation service. This is intentional: an L1 node that can't validate is worse than one that's offline — supervised restart is preferable to silent consensus divergence.
- Operators must configure node supervision (systemd, k8s, etc.) to restart on abort. This was already required for other panic-as-crash paths in the codebase.
- The cooperative progress check and the non-cooperative watchdog deliberately do not overlap (the watchdog skips the `Wait*` stages). A regression in either mechanism is not caught by the other — both must stay green.
- `progress_timeout_secs` couples three timeouts (wait_for_step's progress check, fast_forward send timeout, watchdog stage budget) under one knob. Operators tune one number and the system stays internally consistent; the cost is that the three sites cannot be independently relaxed.

## Source

Branch `fix/vdf-progress`.
