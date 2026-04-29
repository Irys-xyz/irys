---
name: monotonic-time
description: Use when writing or reviewing code that involves time — sleeps, timeouts, retry/backoff, deduplication windows, rate limits, cache TTLs, "wait until X", timing assertions, or any arithmetic over `SystemTime::now()` / `UnixTimestamp::now()` / `Instant::now()` / `tokio::time::sleep`. Decides whether to use monotonic (`Instant`) or wall-clock (`SystemTime`) and how to mix them safely.
---

# Use monotonic time for internal timing

## Default: `Instant` / `Duration`

For any internal duration measurement, deduplication window, rate limit, retry/backoff, timeout, sleep, or cache TTL, default to `std::time::Instant` (or `tokio::time::Instant`) and `Duration`.

`SystemTime::now()` / `UnixTimestamp::now()` is wall-clock. It can:

- Step backward (NTP correction, manual change, VM host resync).
- Jump forward arbitrarily.
- Be inconsistent with `tokio::time::sleep`, which is monotonic.

WSL2 in particular regularly skews `CLOCK_REALTIME` against `CLOCK_MONOTONIC` by hundreds of milliseconds, occasionally seconds (the Hyper-V host clock periodically rewrites the VM's realtime). Mixing tokio sleeps with realtime arithmetic produces hard-to-reproduce flakiness — and in production code, real correctness bugs (a backward jump can violate "child timestamp > parent timestamp" invariants, defeat dedup windows, or expire caches early).

## When wall-clock IS correct

Use `SystemTime` / `UnixTimestamp` *only* when the value crosses a trust boundary that requires it:

- **Block timestamps** — consensus uses real-time for hardfork activation and EVM block headers.
- **Persisted records** — values written to disk/DB that other processes or future invocations will read.
- **Log lines** — human-readable wall-clock context.
- **Anything compared against another node's clock** — peer protocols, signed timestamps.

## If you must use wall-clock + sleep together

Never assume `tokio::time::sleep(N)` advances `SystemTime::now()` by `N`. They use different clocks. After the sleep, re-check the wall clock and loop or retry if the post-condition isn't met.

Two patterns in the tree to copy:

- **`current_timestamp` in `crates/actors/src/block_producer.rs`** — block production needs a wall-clock timestamp strictly greater than the parent's. The function loops until `now().to_secs()` has actually crossed the boundary, instead of trusting one tokio sleep.

- **`wait_for_wallclock` in `crates/chain-tests/src/api/hardfork_tests.rs`** — waits past activation, then *holds on monotonic time* and re-reads realtime. If realtime dips back below activation during the hold (e.g. a backwards lurch), the wait restarts. Adaptive on flaky hosts, fast on stable ones.

## Refactor signal

If you see `SystemTime::now()` / `UnixTimestamp::now()` used for any of:

- A duration measurement (`later - earlier` math).
- A timeout / sleep target.
- A "has window expired" check.
- A "last seen / last request" timestamp for in-process bookkeeping.

…it should almost certainly be `Instant` instead. See `crates/p2p/src/rate_limiting.rs` for a clean example: the rate limiter uses `Instant`/`Duration` because there's no externally-observable reason to expose realtime — switching from `SystemTime` epoch math eliminated a class of WSL2 flakes there.

## Background reading

If a contributor is hitting WSL2 clock issues directly (test flakes, `block timestamp ... is in the past compared to parent timestamp` panics, etc.), see the "WSL2 clock instability" section in the repo `README.md` for the Windows-side `.wslconfig` fix.
