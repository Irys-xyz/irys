# Test Time Machine (Accelerated Virtual Clock) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let integration tests produce many blocks per real-world second by feeding block production a fake, accelerated wall clock, chosen randomly (Real vs Accelerated) per test so no test can silently depend on real time.

**Architecture:** A process-global, swappable `Clock` lives in `irys-types` (`Real` reads `SystemTime`; `Test` reads an atomic virtual-ms counter). `UnixTimestamp(Ms)::now()` funnel through it. The block producer, in Accelerated mode, sets each block's timestamp to `parent + block_time` (no wall-clock sleep) and advances the virtual clock; the validator's future-drift check reads the same clock. The test harness picks the mode (per-test pin > `IRYS_TEST_TIME` env > default) and installs the clock before the node boots. We accelerate *virtual seconds* (many per real second), so every block still has a strictly-greater second — nothing in the Reth fork changes.

**Tech Stack:** Rust 1.93 (edition 2024), tokio, nextest, `std::sync::{OnceLock, atomic::AtomicU64}`, existing `rand = 0.8`.

## Global Constraints

- Toolchain: Rust 1.93.0, edition 2024 (pinned in `rust-toolchain.toml`).
- Production behavior must be **unchanged**: the only reachable clock in a release binary is `Clock::Real` (nothing installs a `Test` clock outside test harness code), and `Real` is byte-for-byte the current `SystemTime::now()` behavior.
- We accelerate virtual seconds, **not** sub-second blocks: every block's second strictly exceeds its parent's. Do **not** modify the Reth fork, the EVM-vs-Irys seconds-equality check (`block_validation.rs:4699`), or Reth's `validate_against_parent_timestamp`.
- VDF is **not** touched in this plan (throttle stays on → ~40 blocks/sec ceiling).
- Test temp dirs go through `irys_testing_utils::TempDirBuilder` / `.tmp` (never `tempfile::tempdir()`), per project convention. Set `IRYS_CUSTOM_TMP_DIR` to the scratchpad if sandboxed.
- Fixed virtual-time anchor: `ANCHOR_MS = 1_767_225_600_000` (2026-01-01T00:00:00Z) — chosen near the real test era so hardfork-by-timestamp gating matches today's behavior at the low end.
- Commands: `cargo nextest run -p <crate> <test>` to run a single test; `cargo xtask local-checks` before finishing.
- Before any commit: match existing style; no `Co-Authored-By` lines; commit messages describe the *why*, no issue/PR/incident references.

---

## File Structure

- `crates/types/src/time.rs` — **add** `Clock`, `TestClock`, global `OnceLock`, `global_clock()`, `install_clock()`; **rewrite** `UnixTimestamp::now()` / `UnixTimestampMs::now()` to funnel through the clock. (Re-exported automatically via `lib.rs:75 pub use time::*;`.)
- `crates/actors/src/block_producer.rs` — **modify** `current_timestamp` (line ~1970) to take `&Clock` and branch; **modify** its call site (line ~606).
- `crates/actors/src/block_validation.rs` — **modify** `timestamp_is_valid` (line ~2125) to read the clock funnel instead of raw `SystemTime`.
- `crates/chain-tests/src/utils.rs` — **add** `TimeMode`, `IrysNodeTest` field + `with_time_mode`, mode resolution + clock install at the top of `start()`, `advance_time`.
- `crates/chain-tests/src/` — **add** an integration test proving accelerated speedup.

---

## Task 1: `Clock` primitive + `now()` funnel in `irys-types`

**Files:**
- Modify: `crates/types/src/time.rs`
- Test: `crates/types/src/time.rs` (in-file `#[cfg(test)]`)

**Interfaces:**
- Produces:
  - `pub enum Clock { Real, Test(std::sync::Arc<TestClock>) }`, `Clock: Clone + Debug`, method `pub fn now_ms(&self) -> UnixTimestampMs`.
  - `pub struct TestClock` with `pub fn new(anchor_ms: u64, tick_ms: u64) -> Self`, `pub fn now_ms(&self) -> UnixTimestampMs`, `pub fn tick_ms(&self) -> u64`, `pub fn advance_millis(&self, millis: u64)`, `pub fn set_at_least(&self, ts: UnixTimestampMs)`.
  - `pub fn global_clock() -> &'static Clock` (default `&Clock::Real`).
  - `pub fn install_clock(clock: Clock) -> bool` (gated `#[cfg(any(test, feature = "test-utils"))]`; idempotent — returns `false` if already installed).
  - All re-exported at `irys_types::` via existing `pub use time::*;`.

- [ ] **Step 1: Write the failing tests**

Add to the existing `#[cfg(test)] mod tests` in `crates/types/src/time.rs` (near line 333):

```rust
    #[test]
    fn test_testclock_advance_and_set_at_least() {
        let c = TestClock::new(1_000, 1_000);
        assert_eq!(c.now_ms().as_millis(), 1_000);
        assert_eq!(c.tick_ms(), 1_000);

        c.advance_millis(500);
        assert_eq!(c.now_ms().as_millis(), 1_500);

        // set_at_least is monotonic: a lower target does nothing.
        c.set_at_least(UnixTimestampMs::from_millis(1_200));
        assert_eq!(c.now_ms().as_millis(), 1_500);

        // a higher target moves it forward.
        c.set_at_least(UnixTimestampMs::from_millis(2_000));
        assert_eq!(c.now_ms().as_millis(), 2_000);
    }

    #[test]
    fn test_clock_enum_now_ms() {
        // Real is close to SystemTime.
        let real = Clock::Real.now_ms().as_millis();
        let sys = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        assert!(sys.abs_diff(real) < 5_000);

        // Test reads its virtual value.
        let t = Clock::Test(std::sync::Arc::new(TestClock::new(42_000, 1_000)));
        assert_eq!(t.now_ms().as_millis(), 42_000);
    }

    #[test]
    fn test_global_clock_defaults_to_real_then_installs() {
        // NOTE: touches the process-global clock. nextest runs each test in its
        // own process, so this is isolated. Keep this the ONLY test in this
        // crate that installs a global clock.
        // Before install, now() tracks the OS clock.
        let before = UnixTimestampMs::now().unwrap().as_millis();
        let sys = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        assert!(sys.abs_diff(before) < 5_000);

        // After install, now() is virtual.
        assert!(install_clock(Clock::Test(std::sync::Arc::new(TestClock::new(
            7_000_000, 1_000,
        )))));
        assert_eq!(UnixTimestampMs::now().unwrap().as_millis(), 7_000_000);
        assert_eq!(UnixTimestamp::now().unwrap().as_secs(), 7_000);

        // A second install is a no-op.
        assert!(!install_clock(Clock::Real));
        assert_eq!(UnixTimestampMs::now().unwrap().as_millis(), 7_000_000);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo nextest run -p irys-types test_testclock_advance_and_set_at_least test_clock_enum_now_ms test_global_clock_defaults_to_real_then_installs`
Expected: FAIL — `cannot find type/function 'TestClock' / 'Clock' / 'install_clock'`.

- [ ] **Step 3: Add the `Clock`/`TestClock` types and global**

At the top of `crates/types/src/time.rs`, add imports (the file already has `use std::time::SystemTime;` at line 10):

```rust
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
```

Add near the end of the file, before the `#[cfg(test)]` modules:

```rust
/// Virtual clock for tests: an atomic millisecond-since-epoch counter plus a
/// fixed per-block tick. ms-since-epoch fits in u64 (overflows ~year 584e6).
#[derive(Debug)]
pub struct TestClock {
    now_ms: AtomicU64,
    tick_ms: u64,
}

impl TestClock {
    /// `anchor_ms`: initial virtual time. `tick_ms`: per-block advance (= block_time).
    pub fn new(anchor_ms: u64, tick_ms: u64) -> Self {
        Self {
            now_ms: AtomicU64::new(anchor_ms),
            tick_ms,
        }
    }

    pub fn now_ms(&self) -> UnixTimestampMs {
        UnixTimestampMs::from_millis(self.now_ms.load(Ordering::Relaxed) as u128)
    }

    pub fn tick_ms(&self) -> u64 {
        self.tick_ms
    }

    /// Advance virtual time by `millis`.
    pub fn advance_millis(&self, millis: u64) {
        self.now_ms.fetch_add(millis, Ordering::Relaxed);
    }

    /// Monotonically raise virtual now to at least `ts` (never moves backward).
    pub fn set_at_least(&self, ts: UnixTimestampMs) {
        self.now_ms
            .fetch_max(ts.as_millis() as u64, Ordering::Relaxed);
    }
}

/// Process-wide time source. `Real` reads the OS clock (production default);
/// `Test` reads a virtual clock. Cloning `Test` shares the same atomic counter.
#[derive(Clone, Debug)]
pub enum Clock {
    Real,
    Test(Arc<TestClock>),
}

impl Clock {
    pub fn now_ms(&self) -> UnixTimestampMs {
        match self {
            Self::Real => UnixTimestampMs::from_millis(
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .expect("system clock before UNIX epoch")
                    .as_millis(),
            ),
            Self::Test(t) => t.now_ms(),
        }
    }
}

static GLOBAL_CLOCK: OnceLock<Clock> = OnceLock::new();
static REAL_CLOCK: Clock = Clock::Real;

/// The installed process clock, or `Real` if none was installed. Always Real in
/// production because nothing outside test harness code calls `install_clock`.
pub fn global_clock() -> &'static Clock {
    GLOBAL_CLOCK.get().unwrap_or(&REAL_CLOCK)
}

/// Install the process clock. Test-only. Idempotent: returns `false` (and keeps
/// the existing clock) if one is already installed.
#[cfg(any(test, feature = "test-utils"))]
pub fn install_clock(clock: Clock) -> bool {
    GLOBAL_CLOCK.set(clock).is_ok()
}
```

- [ ] **Step 4: Funnel `now()` through the clock**

Replace the body of `UnixTimestamp::now()` (lines 66-72) with:

```rust
    #[inline]
    #[allow(clippy::unnecessary_wraps)] // Result kept for source-compat with all call sites
    pub fn now() -> Result<Self, std::time::SystemTimeError> {
        Ok(global_clock().now_ms().to_secs())
    }
```

Replace the body of `UnixTimestampMs::now()` (lines 179-185) with:

```rust
    #[inline]
    #[allow(clippy::unnecessary_wraps)] // Result kept for source-compat with all call sites
    pub fn now() -> Result<Self, std::time::SystemTimeError> {
        Ok(global_clock().now_ms())
    }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo nextest run -p irys-types test_testclock_advance_and_set_at_least test_clock_enum_now_ms test_global_clock_defaults_to_real_then_installs`
Expected: PASS (3 tests).

Also run the existing time tests to confirm no regression:
Run: `cargo nextest run -p irys-types time::`
Expected: PASS (including pre-existing `test_now`).

- [ ] **Step 6: Commit**

```bash
git add crates/types/src/time.rs
git commit -m "feat(types): add swappable Clock with virtual TestClock for tests

now() funnels through a process-global clock, default Real (unchanged in
production); a Test clock lets tests read a virtual wall-clock."
```

---

## Task 2: Producer derives timestamps deterministically in Accelerated mode

**Files:**
- Modify: `crates/actors/src/block_producer.rs` (`current_timestamp` ~1969-1993; call site ~606)
- Test: `crates/actors/src/block_producer.rs` (in-file `#[cfg(test)]`, or the existing test module if present)

**Interfaces:**
- Consumes: `irys_types::{Clock, TestClock}`, `IrysBlockHeader::timestamp: UnixTimestampMs`, `UnixTimestampMs::{to_secs, saturating_add_millis, as_millis}`.
- Produces: `pub async fn current_timestamp(prev_block_header: &IrysBlockHeader, clock: &Clock) -> UnixTimestampMs`.

- [ ] **Step 1: Write the failing test**

Add a test module at the bottom of `crates/actors/src/block_producer.rs` (adjust `use` to the crate's test conventions; `IrysBlockHeader::new_mock_header()` exists per `crates/types/src/block.rs:917`):

```rust
#[cfg(test)]
mod current_timestamp_tests {
    use super::current_timestamp;
    use irys_types::{Clock, IrysBlockHeader, TestClock, UnixTimestampMs};
    use std::sync::Arc;

    #[tokio::test]
    async fn accelerated_mode_returns_parent_plus_tick_without_sleeping() {
        let mut parent = IrysBlockHeader::new_mock_header();
        parent.timestamp = UnixTimestampMs::from_millis(1_767_225_600_000); // anchor
        let clock = Clock::Test(Arc::new(TestClock::new(1_767_225_600_000, 1_000)));

        let ts = current_timestamp(&parent, &clock).await;

        // Deterministic: parent + block_time (tick).
        assert_eq!(ts.as_millis(), 1_767_225_600_000 + 1_000);
        // The virtual clock advanced to (at least) the new block's timestamp.
        if let Clock::Test(t) = &clock {
            assert!(t.now_ms().as_millis() >= 1_767_225_600_000 + 1_000);
        }
    }

    #[tokio::test]
    async fn real_mode_returns_now_when_already_past_parent() {
        // Parent is far in the past, so Real mode returns immediately (no sleep).
        let mut parent = IrysBlockHeader::new_mock_header();
        parent.timestamp = UnixTimestampMs::from_millis(1_000); // 1970-ish
        let before = UnixTimestampMs::now().unwrap().as_millis();

        let ts = current_timestamp(&parent, &Clock::Real).await;

        assert!(ts.as_millis() >= before);
        assert!(ts.to_secs().as_secs() > parent.timestamp.to_secs().as_secs());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p irys-actors current_timestamp_tests`
Expected: FAIL — `current_timestamp` takes 1 argument, not 2 (arity mismatch).

- [ ] **Step 3: Rewrite `current_timestamp` to take `&Clock`**

Replace the function at `crates/actors/src/block_producer.rs:1969-1993` with:

```rust
#[tracing::instrument(level = "trace", skip_all)]
pub async fn current_timestamp(
    prev_block_header: &IrysBlockHeader,
    clock: &irys_types::Clock,
) -> UnixTimestampMs {
    let prev_secs = prev_block_header.timestamp.to_secs();

    // Accelerated (virtual) clock: derive the timestamp deterministically as
    // parent + block_time and jump virtual time forward — no sleep. Because the
    // tick is >= 1000ms, the block's second strictly exceeds the parent's, which
    // is all Reth requires.
    if let irys_types::Clock::Test(t) = clock {
        let target = prev_block_header
            .timestamp
            .saturating_add_millis(t.tick_ms() as u128);
        t.set_at_least(target);
        return target;
    }

    // Real clock: EVM block timestamps are seconds-precision and must be strictly
    // greater than the parent's. Loop until wall-clock time has actually advanced
    // past the parent's second. We re-check after each sleep because tokio's sleep
    // is monotonic-clock based, while the timestamp comes from the realtime clock —
    // the two can drift (notably under WSL2, which periodically resyncs
    // CLOCK_REALTIME against the Windows host and can lurch by hundreds of ms or
    // seconds), so a one-shot computed wait is unsafe.
    loop {
        // Clock::now_ms() is fallible (Real arm can Err on pre-epoch clock);
        // the original code unwrapped UnixTimestampMs::now() here too.
        let now_ms = clock.now_ms().expect("system clock before UNIX epoch");
        if now_ms.to_secs() > prev_secs {
            return now_ms;
        }
        let ms_into_sec = (now_ms.as_millis() % 1000) as u64;
        let wait_duration = Duration::from_millis(1001 - ms_into_sec);
        info!("Waiting {:.2?} to prevent timestamp overlap", &wait_duration);
        tokio::time::sleep(wait_duration).await;
    }
}
```

- [ ] **Step 4: Update the call site**

At `crates/actors/src/block_producer.rs:606`, replace:

```rust
        let current_timestamp = current_timestamp(&prev_block_header).await;
```

with:

```rust
        let current_timestamp =
            current_timestamp(&prev_block_header, irys_types::global_clock()).await;
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo nextest run -p irys-actors current_timestamp_tests`
Expected: PASS (2 tests). The accelerated test must complete effectively instantly (no 1s sleep).

- [ ] **Step 6: Commit**

```bash
git add crates/actors/src/block_producer.rs
git commit -m "feat(actors): derive block timestamp from an injectable clock

In Accelerated mode the producer sets timestamp = parent + block_time and
advances virtual time instead of sleeping to the next real second; Real mode
keeps the existing sleep-to-next-second behavior."
```

---

## Task 3: Validator drift check reads the clock

**Files:**
- Modify: `crates/actors/src/block_validation.rs` (`timestamp_is_valid` ~2115-2140)
- Test: `crates/actors/src/block_validation.rs` (in-file `#[cfg(test)]`)

**Interfaces:**
- Consumes: `irys_types::UnixTimestampMs::now()` (funnel from Task 1).
- Produces: unchanged signature `pub fn timestamp_is_valid(current: u128, parent: u128, allowed_drift: u128) -> Result<(), PreValidationError>`.

- [ ] **Step 1: Write the failing test**

Add to the test module in `crates/actors/src/block_validation.rs` (near the existing `timestamp_is_valid` tests around line 8270):

```rust
    #[test]
    fn timestamp_is_valid_uses_clock_now_for_drift() {
        // now() here comes from the global clock funnel. In an un-installed
        // process it is the real OS clock. A block within drift of "now" passes;
        // one far in the future fails.
        let now_ms = irys_types::UnixTimestampMs::now().unwrap().as_millis();
        let drift = 15_000_u128;

        // within drift, strictly after parent -> ok
        assert!(timestamp_is_valid(now_ms, now_ms.saturating_sub(1_000), drift).is_ok());

        // far in the future -> rejected
        assert!(matches!(
            timestamp_is_valid(now_ms + 10 * 60 * 1000, now_ms.saturating_sub(1_000), drift),
            Err(PreValidationError::TimestampTooFarInFuture { .. })
        ));

        // not strictly after parent -> rejected
        assert!(matches!(
            timestamp_is_valid(now_ms, now_ms, drift),
            Err(PreValidationError::TimestampOlderThanParent { .. })
        ));
    }
```

- [ ] **Step 2: Run test to verify it fails or passes-by-accident**

Run: `cargo nextest run -p irys-actors timestamp_is_valid_uses_clock_now_for_drift`
Expected: PASS already for the last two asserts, but this test locks in behavior. If the module path for `PreValidationError` differs, fix the import (`use super::*;`). Proceed regardless — the real change is Step 3, verified by the full suite.

- [ ] **Step 3: Replace raw `SystemTime` with the clock funnel**

In `crates/actors/src/block_validation.rs`, inside `timestamp_is_valid` (~line 2125), replace:

```rust
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| PreValidationError::SystemTimeError(e.to_string()))?
        .as_millis();
```

with:

```rust
    // Read "now" from the process clock so accelerated tests, whose block
    // timestamps race ahead of real time, are not rejected as "too far in the
    // future". In production this is the real OS clock (unchanged behavior).
    let now_ms = irys_types::UnixTimestampMs::now()
        .map_err(|e| PreValidationError::SystemTimeError(e.to_string()))?
        .as_millis();
```

If, after this edit, `SystemTime` and/or `UNIX_EPOCH` (imported at `block_validation.rs:60`) become unused elsewhere in the file, remove them from the `use std::{...time::{...}}` block. Check with the compiler (Step 4); only remove imports your change orphaned.

- [ ] **Step 4: Run tests + check for orphaned imports**

Run: `cargo nextest run -p irys-actors timestamp_is_valid`
Expected: PASS.
Run: `cargo clippy -p irys-actors --tests 2>&1 | rg "unused import" || echo "no unused imports"`
Expected: `no unused imports` (or fix any that Step 3 orphaned).

- [ ] **Step 5: Commit**

```bash
git add crates/actors/src/block_validation.rs
git commit -m "feat(actors): validate timestamp drift against the process clock

Future-drift check now reads now() from the injectable clock so accelerated
tests aren't rejected; production still reads the real OS clock."
```

---

## Task 4: Test harness — `TimeMode`, install, `advance_time`

**Files:**
- Modify: `crates/chain-tests/src/utils.rs` (`IrysNodeTest` struct ~324-336; `new_inner` ~381; `start` ~400; builders ~465)
- Test: `crates/chain-tests/src/utils.rs` (in-file `#[cfg(test)]`) — small unit test for mode resolution

**Interfaces:**
- Consumes: `irys_types::{Clock, TestClock, install_clock}`, `NodeConfig::consensus.get_mut() -> &mut ConsensusConfig` (`consensus.rs:224`), `ConsensusConfig.genesis.timestamp_millis: u128`, `ConsensusConfig.difficulty_adjustment.block_time: u64`.
- Produces: `pub enum TimeMode { Real, Accelerated }`; `IrysNodeTest::with_time_mode(self, TimeMode) -> Self`; `IrysNodeTest::advance_time(&self, std::time::Duration)`; free fn `resolve_time_mode(override_mode: Option<TimeMode>) -> (TimeMode, String)`.

- [ ] **Step 1: Write the failing test (mode resolution precedence)**

Add near the bottom of `crates/chain-tests/src/utils.rs`:

```rust
#[cfg(test)]
mod time_mode_tests {
    use super::{resolve_time_mode, TimeMode};

    #[test]
    fn override_wins_over_default() {
        let (mode, reason) = resolve_time_mode(Some(TimeMode::Accelerated));
        assert_eq!(mode, TimeMode::Accelerated);
        assert!(reason.contains("per-test"));
    }

    #[test]
    fn default_is_real_before_rollout_flip() {
        // With no override and no env, the harness default is Real (Task 6 flips
        // this to a random draw). This test asserts the pre-flip default.
        // (Assumes IRYS_TEST_TIME is not set in the test environment.)
        if std::env::var("IRYS_TEST_TIME").is_ok() {
            return; // skip when the env pin is active
        }
        let (mode, _reason) = resolve_time_mode(None);
        assert_eq!(mode, TimeMode::Real);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo nextest run -p irys-chain-tests time_mode_tests`
Expected: FAIL — `resolve_time_mode` / `TimeMode` not found.

(If the crate name is not `irys-chain-tests`, use the name from `crates/chain-tests/Cargo.toml` `[package] name`.)

- [ ] **Step 3: Add `TimeMode` + resolution**

Add near the top of `crates/chain-tests/src/utils.rs` (after imports):

```rust
/// Fixed virtual-time anchor for Accelerated mode: 2026-01-01T00:00:00Z, in ms.
/// Chosen near the real test era so hardfork-by-timestamp gating matches
/// today's behavior at the low end.
const ANCHOR_MS: u64 = 1_767_225_600_000;

/// Which wall clock a test node runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeMode {
    /// Real OS clock (current behavior; ~1 block/sec).
    Real,
    /// Accelerated virtual clock: block timestamps = parent + block_time, no sleep.
    Accelerated,
}

/// Resolve the effective time mode by precedence:
/// per-test `override_mode` > `IRYS_TEST_TIME` env > harness default.
/// Returns the mode and a human-readable reason for logging.
pub fn resolve_time_mode(override_mode: Option<TimeMode>) -> (TimeMode, String) {
    if let Some(m) = override_mode {
        return (m, "per-test pin (.with_time_mode)".to_string());
    }
    if let Ok(v) = std::env::var("IRYS_TEST_TIME") {
        match v.trim().to_ascii_lowercase().as_str() {
            "real" => return (TimeMode::Real, "IRYS_TEST_TIME=real".to_string()),
            "accelerated" | "accel" => {
                return (TimeMode::Accelerated, "IRYS_TEST_TIME=accelerated".to_string())
            }
            other => {
                warn!("ignoring unknown IRYS_TEST_TIME value {other:?}; using default");
            }
        }
    }
    // Harness default. Task 6 flips this to a seeded 50/50 random draw.
    (TimeMode::Real, "harness default (Real)".to_string())
}
```

Ensure `warn` is in scope (the file already uses `warn!` at line 413).

- [ ] **Step 4: Add the struct field + builder**

In the `IrysNodeTest<T>` struct (lines 324-336), add a field:

```rust
    /// Per-test time-mode override; `None` means use `resolve_time_mode`'s default.
    time_mode_override: Option<TimeMode>,
```

In `new_inner` (the struct literal around lines 383-395), add:

```rust
            time_mode_override: None,
```

In the `IrysNodeTest<IrysNodeCtx>` struct literal returned by `start` (lines 446-455), add:

```rust
            time_mode_override: self.time_mode_override,
```

Add the builder next to `with_name` (line 465):

```rust
    /// Pin this node's wall-clock mode, opting out of the random default.
    /// Use `Accelerated` for tests that jump time far forward via `advance_time`.
    pub fn with_time_mode(mut self, mode: TimeMode) -> Self {
        self.time_mode_override = Some(mode);
        self
    }
```

- [ ] **Step 5: Install the clock + anchor genesis at the top of `start`**

Change `start`'s signature (line 400) from `pub async fn start(self)` to `pub async fn start(mut self)`.

Immediately after `let _enter = span.enter();` (line 402), insert:

```rust
        // Resolve and install the process time source before the node boots.
        {
            let consensus = self.cfg.consensus.get_mut();
            let block_time_secs = consensus.difficulty_adjustment.block_time;
            let (mode, reason) = resolve_time_mode(self.time_mode_override);
            match mode {
                TimeMode::Real => {
                    info!("⏱ test time mode: REAL [{reason}]");
                }
                TimeMode::Accelerated => {
                    // Anchor genesis to the virtual start (0 == "use now()" sentinel).
                    if consensus.genesis.timestamp_millis == 0 {
                        consensus.genesis.timestamp_millis = ANCHOR_MS as u128;
                    }
                    let tick_ms = block_time_secs.saturating_mul(1_000);
                    // Guard the producer's invariant (accelerated block seconds
                    // strictly increase) at the construction site.
                    assert!(
                        tick_ms >= 1_000,
                        "accelerated block_time must be >= 1s to keep block seconds \
                         strictly increasing; got {block_time_secs}s"
                    );
                    let installed = irys_types::install_clock(irys_types::Clock::Test(
                        std::sync::Arc::new(irys_types::TestClock::new(ANCHOR_MS, tick_ms)),
                    ));
                    info!(
                        "⏱ test time mode: ACCELERATED tick={tick_ms}ms anchor={ANCHOR_MS} \
                         installed={installed} [{reason}] \
                         (reproduce with IRYS_TEST_TIME=accelerated or .with_time_mode(Accelerated))"
                    );
                }
            }
        }
```

Note: `self.cfg` is read into `cfg_for_start` on the next line (403), so this mutation lands before the node is built.

- [ ] **Step 6: Add `advance_time`**

Add to `impl IrysNodeTest<IrysNodeCtx>` (the started-node impl; find it with `rg "impl IrysNodeTest<IrysNodeCtx>" crates/chain-tests/src/utils.rs`):

```rust
    /// Jump virtual time forward. In Accelerated mode this advances the virtual
    /// clock instantly. In Real mode it is a no-op: real wall-clock time advances
    /// on its own as the producer sleeps between blocks. For pure time-passage
    /// (no intervening blocks) in Real mode, sleep instead; for hour/day-scale
    /// jumps, pin Accelerated with `.with_time_mode(TimeMode::Accelerated)`.
    pub fn advance_time(&self, by: std::time::Duration) {
        if let irys_types::Clock::Test(t) = irys_types::global_clock() {
            t.advance_millis(by.as_millis() as u64);
        }
    }
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo nextest run -p irys-chain-tests time_mode_tests`
Expected: PASS (2 tests).
Run: `cargo build -p irys-chain-tests --tests`
Expected: builds clean.

- [ ] **Step 8: Commit**

```bash
git add crates/chain-tests/src/utils.rs
git commit -m "feat(chain-tests): time-mode harness (Real/Accelerated) + advance_time

Resolve mode by per-test pin > IRYS_TEST_TIME env > default (Real for now);
in Accelerated mode anchor genesis and install a virtual clock before boot."
```

---

## Task 5: Integration test proving accelerated speedup

**Files:**
- Create: `crates/chain-tests/src/block_production/accelerated_time.rs` (or add to an existing block-production test module — check `rg "mod block_production" crates/chain-tests/src/` and follow the module-registration pattern)
- Modify: the parent `mod.rs` to register the new module.

**Interfaces:**
- Consumes: `IrysNodeTest`, `TimeMode`, and these verified harness accessors — `mine_blocks(&self, usize) -> eyre::Result<()>` (utils.rs:1639), `get_canonical_chain_height(&self) -> u64` (utils.rs:1537), `get_block_by_height(&self, u64) -> eyre::Result<IrysBlockHeader>` (utils.rs:2497), `stop(self) -> IrysNodeTest<()>` (utils.rs:2574). `IrysBlockHeader` derefs to `IrysBlockHeaderV1`, so `.timestamp` field access works.

- [ ] **Step 1: Write the test**

Create `crates/chain-tests/src/block_production/accelerated_time.rs`:

```rust
use crate::utils::{IrysNodeTest, TimeMode};
use irys_types::NodeConfig;

/// In Accelerated mode, producing several blocks must not take ~1s/block, and
/// block timestamps must be strictly increasing.
#[test_log::test(tokio::test)]
async fn accelerated_time_produces_blocks_faster_than_realtime() {
    let config = NodeConfig::testing();
    let node = IrysNodeTest::new_genesis(config)
        .with_time_mode(TimeMode::Accelerated)
        .start()
        .await;

    let n = 10_u64;
    let start = std::time::Instant::now();
    node.mine_blocks(n as usize)
        .await
        .expect("mining should succeed");
    let elapsed = start.elapsed();

    // Real mode would need >= ~n seconds (1 block/sec). Accelerated must be well
    // under that. Generous bound to avoid CI flake while still proving the point.
    assert!(
        elapsed.as_secs() < n,
        "expected accelerated mining < {n}s, took {elapsed:?}"
    );
    assert!(node.get_canonical_chain_height().await >= n);

    // Timestamps strictly increase across the produced chain (.timestamp via Deref).
    let mut prev = node
        .get_block_by_height(0)
        .await
        .expect("genesis")
        .timestamp;
    for h in 1..=n {
        let ts = node.get_block_by_height(h).await.expect("block").timestamp;
        assert!(
            ts.as_millis() > prev.as_millis(),
            "block {h} timestamp not strictly increasing"
        );
        prev = ts;
    }

    node.stop().await;
}
```

- [ ] **Step 2: Register the module**

In the block-production module file (e.g. `crates/chain-tests/src/block_production/mod.rs`), add:

```rust
mod accelerated_time;
```

- [ ] **Step 3: Run the test**

Run: `cargo nextest run -p irys-chain-tests accelerated_time_produces_blocks_faster_than_realtime`
Expected: PASS, completing in well under 10 seconds.

- [ ] **Step 4: Sanity-check Real mode still works for the same test path**

Run: `IRYS_TEST_TIME=real cargo nextest run -p irys-chain-tests accelerated_time_produces_blocks_faster_than_realtime`
Expected: the env pin is overridden by the test's explicit `.with_time_mode(Accelerated)` (per-test pin wins), so it still runs Accelerated and PASSES. This verifies precedence end-to-end.

- [ ] **Step 5: Commit**

```bash
git add crates/chain-tests/src/block_production/accelerated_time.rs crates/chain-tests/src/block_production/mod.rs
git commit -m "test(chain-tests): prove accelerated time mines blocks sub-realtime"
```

---

## Task 6: Flip harness default to random 50/50 + stabilize (rollout stage 3)

> This task changes the default for **every** test. It will surface tests with hidden real-time assumptions — that is the intended outcome. Do it only after Tasks 1–5 are merged and green.

**Files:**
- Modify: `crates/chain-tests/src/utils.rs` (`resolve_time_mode` default branch; add `Random` seed logging)
- Modify: `crates/chain-tests/src/utils.rs` test `default_is_real_before_rollout_flip` → update to assert the random default is one of {Real, Accelerated} and is logged.

**Interfaces:**
- Consumes: `rand` (already a dep of `irys-types`; add to `crates/chain-tests/Cargo.toml` if not present — check `rg "^rand" crates/chain-tests/Cargo.toml`).

- [ ] **Step 1: Update the resolution default to a seeded random draw**

Replace the default branch in `resolve_time_mode` (the final `(TimeMode::Real, ...)` line) with:

```rust
    // Harness default: seeded 50/50 draw, logged for reproducibility.
    let seed: u64 = rand::random();
    let mode = if seed % 2 == 0 {
        TimeMode::Real
    } else {
        TimeMode::Accelerated
    };
    (
        mode,
        format!(
            "random default 50/50 [seed={seed:#x}] (pin with IRYS_TEST_TIME=real|accelerated)"
        ),
    )
```

Add `use rand;` usage is implicit; ensure `rand` is a dependency of `chain-tests` (add `rand.workspace = true` to `[dependencies]` in `crates/chain-tests/Cargo.toml` if missing).

- [ ] **Step 2: Update the resolution unit test**

Replace `default_is_real_before_rollout_flip` with:

```rust
    #[test]
    fn default_is_random_and_logged() {
        if std::env::var("IRYS_TEST_TIME").is_ok() {
            return;
        }
        let (mode, reason) = resolve_time_mode(None);
        assert!(matches!(mode, TimeMode::Real | TimeMode::Accelerated));
        assert!(reason.contains("seed="));
    }
```

- [ ] **Step 3: Run the resolution test**

Run: `cargo nextest run -p irys-chain-tests time_mode_tests`
Expected: PASS.

- [ ] **Step 4: Stabilize the full suite under BOTH forced modes**

This is the substantive work. Run the whole integration suite pinned to each mode and fix or explicitly pin every failure:

```bash
IRYS_TEST_TIME=accelerated cargo nextest run -p irys-chain-tests
IRYS_TEST_TIME=real        cargo nextest run -p irys-chain-tests
```

For each failure:
- If the test made a real-time assumption that is genuinely wrong (e.g. asserting a real duration elapsed, or a timestamp ≈ real `now()`), fix the assertion.
- If the test legitimately requires one regime (e.g. it jumps hours via `advance_time`, or it specifically verifies real-clock drift rejection), add `.with_time_mode(TimeMode::Accelerated)` or `(TimeMode::Real)` to pin it, with a one-line comment on the *why*.
- Record any non-obvious fixes; large clusters of failures may warrant a follow-up plan.

Expected end state: both commands green.

- [ ] **Step 5: Full local checks**

Run: `cargo xtask local-checks`
Expected: fmt/check/clippy/unused-deps/typos all clean.

- [ ] **Step 6: Commit**

```bash
git add crates/chain-tests/
git commit -m "feat(chain-tests): default test time mode to seeded random 50/50

Every unpinned test now runs Real or Accelerated per run (logged seed,
IRYS_TEST_TIME pin to reproduce), so no test can hide a real-time assumption."
```

---

## Self-Review Notes (spec coverage)

- Clock abstraction + global + funnel → Task 1. ✓ (spec §Architecture 1)
- Producer deterministic non-sleeping advance → Task 2. ✓ (spec §2)
- Validator reads clock → Task 3. ✓ (spec §3)
- Genesis anchor → Task 4 Step 5 (via config sentinel, no `genesis_builder` edit). ✓ (spec §4, simplified)
- VDF unchanged → no task. ✓ (spec §5)
- Harness TimeMode/random/override/env/advance_time → Tasks 4 & 6. ✓ (spec §6, reproducibility contract)
- Staged rollout (Real-default → stabilize → flip) → Tasks 4 (default Real) → 5 → 6. ✓ (spec §Rollout)
- Production safety: only `install_clock` (gated + never called in prod) mutates; default Real → covered by Task 1 design + Global Constraints.

## Out of scope (spec non-goals)

- Removing the VDF throttle for >40 blocks/sec.
- Per-node clock skew.
- Abstracting p2p / rate-limiter / metrics time.
- Env-var tuning of the accel probability (probability is code/default-only for now).
