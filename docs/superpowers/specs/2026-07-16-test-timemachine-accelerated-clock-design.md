# Test "Time Machine": Accelerated Virtual Wall-Clock for Tests

**Status:** Design (approved for spec review)
**Date:** 2026-07-16
**Branch:** `feat/test-timemachine`

## Problem

Integration tests are capped at roughly **one block per real-world second**. The
cause is the block-production timestamp path, not the VDF:

- EVM/Reth block headers carry **seconds-granularity** timestamps, and Reth's
  consensus (`validate_against_parent_timestamp`) requires
  `child_secs > parent_secs` strictly. The Irys validator additionally forces the
  EVM second to equal the Irys-header second
  (`block_validation.rs:4699`), so the millisecond-precision Irys header cannot
  smuggle two blocks into one second.
- Consequently the producer proactively **sleeps** in `current_timestamp()`
  (`block_producer.rs:1970`) until the real wall clock crosses into a new second
  past the parent's. That sleep is the binding ~1 block/sec cap.

The secondary coupling is the VDF: a block's solution must have
`vdf_step > parent.vdf_step`, and steps only exist once broadcast. VDF cadence is
**cost-based (SHA iterations), not wall-clock**; in test config a deliberate
`throttle: true` + 25 ms floor (`vdf.rs:333`) caps it at ~40 steps/sec — 40× looser
than the timestamp wall.

## Goal

Let tests produce many blocks per real second by feeding block production a fake,
accelerated wall clock, while keeping production behavior identical and keeping
tests honest about real-time assumptions.

### Key insight that keeps the change small

We accelerate **virtual seconds** (many virtual seconds per real second), rather
than packing multiple blocks into one second. Every block therefore still has a
strictly-greater second than its parent. As a result **nothing in the Reth fork,
the seconds-equality check, or Reth's `validate_against_parent_timestamp` needs to
change.** The entire change stays inside Irys code.

## Decisions (locked)

1. **Scope:** consensus path + one settable global virtual clock. The producer
   derives timestamps deterministically; the validator drift check, cache-TTL
   eviction, hardfork-by-timestamp gating, and other `now()`-based reads follow one
   injectable clock. VDF, p2p, rate-limiters, metrics/telemetry stay on real time.
2. **Speed target (first cut):** ~40 blocks/sec — keep the VDF throttle. Removing
   the timestamp sleep alone yields the ~40× speedup. Dropping the throttle is a
   documented follow-up.
3. **Clock model:** deterministic auto-advance. Each block's timestamp =
   `parent + block_time` (no wall-clock read, no sleep). Keeps difficulty
   adjustment stable (actual == target) and is fully deterministic.
4. **Default-on, randomly, per run:** the testing harness randomly selects Real or
   Accelerated mode per test process (nextest = one process per test → per-test,
   per-run). This forces every test to pass under **both** regimes, so no test can
   silently bake in a real-time assumption. Randomization is **seeded, logged, and
   overridable** for reproducibility. This is the end-state default, enabled only
   after the staged rollout below (land Real-default → stabilize → flip).
5. **Distribution:** tunable via config; default **50/50**; per-test override.
   Env-var tuning of the probability is deferred.
6. **Randomized axis:** mode only (Real vs Accelerated with `tick = block_time`).
   All nodes in a multi-node test share one virtual timeline.

## Architecture

### 1. `Clock` abstraction (`crates/types/src/time.rs`)

A process-global, swappable clock. Modeled as an enum (no `dyn`, no `async_trait`):

```rust
#[derive(Clone, Debug)]
pub enum Clock {
    Real,
    Test(Arc<TestClock>),
}

impl Clock {
    pub fn now_ms(&self) -> UnixTimestampMs { /* Real: SystemTime; Test: atomic load */ }
}

/// Virtual clock: an atomic millisecond counter plus a configured tick.
/// ms-since-epoch fits in u64 (overflows ~year 584e6), so a single AtomicU64
/// suffices; `UnixTimestampMs` (u128) is produced by widening on read.
#[derive(Debug)]
pub struct TestClock { now_ms: AtomicU64, tick_ms: u64 }
impl TestClock {
    pub fn now_ms(&self) -> UnixTimestampMs;
    pub fn advance(&self, d: Duration);
    pub fn set_at_least(&self, ts: UnixTimestampMs); // monotonic set
    pub fn tick_ms(&self) -> u64;
}
```

Global installation and read:

```rust
static CLOCK: ArcSwap<Clock> = /* default Clock::Real */;
pub fn clock() -> Arc<Clock>;                 // always compiled; one cheap atomic load
#[cfg(feature = "test-time")]
pub fn install_clock(c: Clock);               // only test builds can install a fake
```

- `RealClock` behavior is the default and always compiled. A production binary,
  which never enables `test-time` and never calls `install_clock`, is byte-for-byte
  identical in behavior (one extra relaxed atomic load on the `now()` path).
- `UnixTimestamp::now()` and `UnixTimestampMs::now()` are rewritten to read
  `clock()` instead of calling `SystemTime::now()` directly. This funnels the
  pervasive callers (cache TTL, hardfork tx-version gating, price oracle,
  block-age metric) onto the virtual clock automatically.

### 2. Producer: deterministic, non-sleeping advance

`current_timestamp()` (`block_producer.rs:1970`) keeps its contract — "return a
timestamp whose second strictly exceeds the parent's" — but the *advance* is
delegated to the clock:

```rust
pub async fn current_timestamp(prev: &IrysBlockHeader) -> UnixTimestampMs {
    let parent = prev.timestamp;                         // UnixTimestampMs
    match &*clock() {
        Clock::Test(t) => {                              // accelerated: instant, deterministic
            let target = parent + t.tick_ms();           // parent + block_time
            t.set_at_least(target);                      // advance global virtual now
            target
        }
        Clock::Real => {                                 // unchanged production loop
            let prev_secs = parent.to_secs();
            loop {
                let now = clock().now_ms();              // == SystemTime for Real
                if now.to_secs() > prev_secs { return now; }
                /* compute wait, tokio::time::sleep, re-check (WSL2 drift-safe) */
            }
        }
    }
}
```

`tick_ms` is initialized to `block_time` by the harness. Because `tick_ms >= 1000`,
each block advances at least one virtual second, satisfying Reth. As blocks are
produced, `set_at_least` moves the global virtual clock forward, so everything that
reads `now()` sees a "now" consistent with the latest block.

### 3. Validator: read the clock, not `SystemTime`

`timestamp_is_valid()` (`block_validation.rs:2125`) reads `irys_types::clock().now_ms()`
instead of raw `SystemTime::now()` for its 15 s future-drift bound. In Real mode
this is identical to today. In Accelerated mode the virtual "now" tracks the latest
block, so a freshly produced block (`= parent + tick <= now`) passes.

### 4. Funnel the remaining consensus-relevant raw `SystemTime::now()`

- `block_validation.rs:2125` (drift check) → `clock()` — **critical**.
- `genesis_builder.rs:218` (genesis timestamp) → `clock()`, so genesis anchors to
  the virtual start and block 1 = genesis + tick is coherent and deterministic.
- Left on real time (out of scope, first cut): VDF throttle (`vdf.rs`), p2p
  freshness / rate-limiters, metrics/telemetry (`block_tree_service::now_ms`,
  `telemetry.rs`), TUI/CLI. The `irys-reth:219` `SystemTime::now()` is blob-cache
  sizing only, not the block timestamp — left as-is.

### 5. VDF: unchanged

Keep `throttle: true` + 25 ms floor → ~40 steps/sec ceiling → **~40 blocks/sec**.
No VDF code changes in this cut.

### 6. Harness integration (`crates/chain-tests/src/utils.rs`, `irys-testing-utils`)

```rust
enum TimeMode {
    Real,
    Accelerated { tick_ms: u64 },
    Random { accel_pct: u8, seed: Option<u64> }, // default: accel_pct = 50
}
```

- Default for the testing harness is `Random { accel_pct: 50, seed: None }`.
- On node start the harness resolves the mode by precedence —
  **per-test `.with_time_mode()` > `IRYS_TEST_TIME` env > harness default (`Random`)** —
  drawing a concrete Real/Accelerated from the seed when `Random`, **logs** the
  decision, and installs the global clock:

  ```
  ⏱ test time mode: ACCELERATED tick=1000ms  [seed=0x9f3c… — pin with .with_time_mode(Accelerated)]
  ```

- **Per-test override:** `.with_time_mode(TimeMode::Real | Accelerated{..})` on the
  harness builder pins a regime (also settable via env, see reproducibility).
- **`node.advance_time(Duration)`** jumps the virtual clock forward for
  expiry/epoch scenarios.
  - **Accelerated mode:** advances the virtual clock instantly by `Duration`.
  - **Real mode:** **no-op.** Real wall-clock time advances on its own as the
    producer sleeps between blocks (`current_timestamp`'s per-block wait), so
    explicit advancement is unnecessary when time passage is driven by block
    production. Because `advance_time` is a no-op rather than an error in Real
    mode, time-warp tests still participate in randomization (run in both regimes)
    instead of being forced to pin Accelerated.
  - **Caveat:** a test that needs *pure* time passage in Real mode — time advancing
    without producing blocks (e.g. a cache-TTL eviction with no intervening blocks)
    — should call a real sleep, or pin Accelerated. Large jumps (hours/days) are
    only feasible in Accelerated mode; such tests pin Accelerated.

### Reproducibility contract

Random-by-default is only safe if a failure is reproducible:

- The resolved mode and seed are logged at node start.
- A failing test is reproduced two ways: in-code via `.with_time_mode(...)`, or —
  without editing the test — via the env override **`IRYS_TEST_TIME=real|accelerated`**,
  which force-pins the mode for every test in the process — overriding the random
  default, but **not** an explicit per-test `.with_time_mode()` pin (which exists
  for correctness, e.g. infeasible large jumps). This is the natural repro mechanism
  for CI flakes and is **in scope for this cut**.
- Distinct from the above: the *probability*-tuning env var (dialing `accel_pct`)
  is deferred; the probability is config-only for now (default 50/50).

## Determinism & difficulty interaction

Deterministic `parent + block_time` timestamps mean `calculate_difficulty` sees
`actual == target` block time on every block → no spurious retargeting. This is the
main reason deterministic auto-advance was chosen over a free-running
accelerated-real-time factor.

## Rollout sequencing (important)

Flipping random-by-default will surface every existing test with a hidden
real-time assumption — that is the intended outcome, but it must be staged:

1. Land the `Clock` infra + producer/validator/funnel changes with the harness
   default still **Real** (no behavior change; opt-in `.with_time_mode(Accelerated)`).
2. Run the full suite under **forced Accelerated** and under **forced Real**; fix
   fallout (tests asserting real durations, timestamps ≈ real `now()`, fixed
   real-time sleeps expecting N blocks).
3. Flip the harness default to `Random { accel_pct: 50 }`.

Ties into `cargo xtask flaky` and the nextest failure-tracking harness for the
stabilization pass.

## Non-goals / follow-ups

- Removing the VDF throttle for >40 blocks/sec.
- Modeling per-node clock skew (independent virtual clocks per node) — interacts
  with the 15 s drift check; deliberately out of scope.
- Abstracting p2p / rate-limiter / metrics time onto the virtual clock.
- Env-var tuning of the accel probability.

## Affected code (inventory)

| Area | Location | Change |
|---|---|---|
| Clock type + global + funnel | `crates/types/src/time.rs` | add `Clock`, `TestClock`, global, rewrite `now()` |
| Producer timestamp | `crates/actors/src/block_producer.rs:1970` | delegate advance to clock (no sleep in Accelerated) |
| Validator drift | `crates/actors/src/block_validation.rs:2125` | read `clock()` not `SystemTime` |
| Genesis anchor | `crates/chain/src/genesis_builder.rs:218` | read `clock()` |
| Harness | `crates/chain-tests/src/utils.rs`, `crates/utils/testing-utils` | `TimeMode`, random default, override, `advance_time` |
| Feature | `crates/types/Cargo.toml` (+ test crates) | `test-time` feature gating `install_clock` / `TestClock` install |

## Success criteria

- A representative multi-block test runs ≥10× faster in Accelerated mode with
  identical assertions passing.
- Production build behavior is unchanged (Real is the only reachable clock; verify
  no `test-time` feature leaks into release).
- The full suite passes under forced Real **and** forced Accelerated before
  random-by-default is enabled.
- A random-mode failure is reproducible from its logged seed/mode via a pin.
