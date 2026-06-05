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

`VdfStateReadonly::wait_for_step` (`crates/vdf/src/state.rs`) takes `cancel: Arc<AtomicU8>` and `progress_timeout: Duration` and returns `eyre::Result<()>`. On error it wraps a `WaitForStepError`, which callers (`PreemptibleVdfTask::execute` in `crates/actors/src/validation_service/active_validations.rs`) recover via `eyre::Report::downcast_ref::<WaitForStepError>()` to dispatch on two variants kept deliberately distinct:

- `WaitForStepError::Cancelled` — the cancel atomic was set (preemption by a higher-priority block, or shutdown). `PreemptibleVdfTask::execute` maps this to `VdfValidationResult::Cancelled` and the service requeues the same task with refreshed priority.
- `WaitForStepError::Stalled` — `global_step` did not advance for `progress_timeout`. The classifier **panics** the validation task. This is a local-infrastructure failure (dead VDF writer thread, poisoned lock exit, paused sync), not block-invalidity evidence — the non-cooperative watchdog already panics on the same condition inside computational stages, and the cooperative `Wait*` stages must follow the same policy. The panic propagates through the spawned task as `JoinError::is_panic()`, the validation service's select loop `resume_unwind`s it onto the service task, and the global panic hook then raises SIGINT and the 45s shutdown watchdog forces process abort.

We keep the return type as `eyre::Result<()>` rather than a concrete `Result<(), WaitForStepError>` so that callers using `?` against an `eyre::Result` context can propagate without an explicit `From`-conversion at every call site; the variant distinction lives in the carried error type and is recovered by downcast at the one place that needs it.

Collapsing the two into a single `Err` would re-introduce the original failure mode: requeuing a stalled block is wrong (local VDF state is the bottleneck, retrying won't help) and dropping it on cooperative cancel is wrong (the block is still potentially valid, just deprioritised). Mapping `Stalled` to `VdfValidationResult::Invalid(_)` would be wrong for the same reason as below — surfacing local-infrastructure failure as block-Invalid would fork the node off the network on a programmer-error condition.

### Non-cooperative: stage watchdog with process abort

`ensure_vdf_is_valid` records its progress at each stage boundary via `record_vdf_task_progress`, which writes both a `stage_signal: AtomicU8` and a `progress_instant: Mutex<Instant>`. The `ValidationService` select loop ticks every 5s and asks `VdfScheduler::abort_stalled_current(progress_timeout)`:

- The only skipped stages are `WaitPrevStep` and `WaitFinalCatchUp`, and not because they have a redundant check — they need a *different kind* of check. The watchdog measures **stage wall-clock duration**; `wait_for_step` measures **`global_step` advancement**. For a wait stage these are not interchangeable: a `Wait*` stage can legitimately last minutes (e.g. a peer catching up several hundred VDF steps from gossip at ~1 step/s), and the wait is healthy as long as `global_step` keeps advancing — the stage clock has no bearing on whether progress is happening. Watching `Wait*` with a stage-duration timeout would false-fire on every legitimate long catch-up, recreating the original production-hang failure mode in reverse. `wait_for_step` panics on `WaitForStepError::Stalled` at the right granularity (no step advance for `progress_timeout`), and shares the same shutdown path. Every other stage (`Starting`, `ValidateSeeds`, `ValidateBatch`, `FastForwardBatch`, `Completed`) is watched. `Starting` is watched deliberately: a consensus-critical task the runtime cannot poll for `progress_timeout` seconds is itself a stall worth crashing on. `Completed` is watched because it's a terminal handoff state — the gap between the final `record_vdf_task_progress(Completed)` and the select loop collecting the `JoinHandle` should be microseconds; a 15 s dwell there means the validation service's own select loop is starved. That starvation only warrants a crash if the result was *never produced*: a task whose handle has already **finished** has its result ready to collect, and the fact that the watchdog is running again proves the loop has resumed, so crashing would discard a valid result and kill a recovered node. `abort_stalled_current` therefore exempts a finished handle (`JoinHandle::is_finished()`) — the next `poll_vdf` collects it normally. Only a task wedged *before* its handle finishes (deadlock, poisoned lock, runaway loop) keeps `is_finished()` false and trips the abort. This exemption was added after a CI flake where heavy parallel-test load starved the select loop past the 15 s budget between a VDF task completing and `poll_vdf` reaping it, panicking a node that was otherwise healthy.
- If a watched stage's `progress_instant` is older than `progress_timeout` **and** its `JoinHandle` is not yet finished, the watchdog sets the cooperative cancel signal, calls `JoinHandle::abort()` on the task, then `panic!`s the validation service.

To keep the watchdog's clock honest about *forward progress* rather than *queue time*, `PreemptibleVdfTask::execute` calls `record_vdf_task_progress(Starting)` as its very first action. `progress_instant` is initialized at queue time in `start_next()`, but that timestamp is overwritten the moment the future is first polled. The spawn-to-first-poll latency is preserved in the `vdf_starting` histogram for diagnostics; the watchdog's stall budget begins from first-poll, not from queue-time. If first-poll itself never happens within `progress_timeout` (runtime starvation, deadlocked executor, etc.), the watchdog still trips on the original queue-time `Instant` — which is the intended behavior, since a node that can't schedule its own validation task is broken.

The panic is *not* handled in the loop. It propagates to the global panic hook (`setup_panic_hook` in `crates/utils/testing-utils/src/utils.rs`, installed by `crates/chain/src/main.rs`), which logs the panic, raises `SIGINT` to start an orderly shutdown, and arms a `GRACEFUL_SHUTDOWN_TIMEOUT` (45s) watchdog that calls `std::process::abort()` if the shutdown stalls. Either way the supervisor restarts the node clean.

The governing rule, both here and at the matching select-loop arm that `std::panic::resume_unwind`s on `JoinError::is_panic()`, is: **never mislabel a block as Valid or Invalid.** Either misclassification near-guarantees a fork — Valid lets an actually-invalid block onto the local chain; Invalid drops an actually-valid block while every honest peer accepts it. Validation panics are by construction unreachable; if one ever fires, the only consensus-safe response is to crash and let the supervisor restart, so the node rejoins from a clean state instead of forking off on an internal programmer-error signal.

`record_vdf_task_progress` is documented as "call only at real stage transitions" — adding periodic heartbeats inside a stage would silently defeat the watchdog.

### Bounded `vdf_fast_forward` channel with the same panic policy

The `vdf_fast_forward` channel was previously unbounded; under the deadlock scenario above it would have grown without bound while the validation pipeline parked. It is now bounded to 4096 messages, and `fast_forward_validated_steps` sends with `progress_timeout` as a per-step send timeout.

Both consumer-dead conditions panic under the same reasoning as the watchdog:

- **Send timeout** — `run_vdf` drains this channel fully between every ~1s SHA step, so 15s of sustained backpressure means the consumer is dead, not slow.
- **`Sender::send` returning `Err`** (receiver dropped) — `run_vdf` has exited (e.g., the graceful exit on a poisoned VDF state lock added in this branch). Same root cause as the timeout, just observed faster.

Process-abort (via the same panic-hook → SIGINT → 45 s watchdog path as above) is the correct response in both cases. Treating either as an ordinary `Err` would let the validation task publish `Invalid(VdfValidationFailed)` for a local-infrastructure failure and fork the node off the network — violating the same "never mislabel" rule that governs the watchdog and the cooperative `Wait*` stages.

## Consequences

- A stalled VDF state surfaces as `Invalid(VdfValidationFailed)` instead of silently parking validation. The block tree learns and acts.
- A deadlocked validation task takes the whole node down via the panic-hook shutdown path described above, not just the validation service. This is intentional: an L1 node that can't validate is worse than one that's offline — supervised restart is preferable to silent consensus divergence. Worst-case wall-clock from panic to fresh process is bounded by `GRACEFUL_SHUTDOWN_TIMEOUT` (45 s).
- Operators must configure node supervision (systemd, k8s, etc.) to restart on abort. This was already required for other panic-as-crash paths in the codebase.
- The cooperative progress check and the non-cooperative watchdog deliberately do not overlap (the watchdog skips the `Wait*` stages). A regression in either mechanism is not caught by the other — both must stay green.
- `progress_timeout_secs` couples three timeouts (wait_for_step's progress check, fast_forward send timeout, watchdog stage budget) under one knob. Operators tune one number and the system stays internally consistent; the cost is that the three sites cannot be independently relaxed.

## Source

Branch `fix/vdf-progress`.
