+++
id = "wall-clock-for-internal-timing"
severity = "dos"
enforcement = "scan"
scope = "node code that measures time: timeouts, sleeps, retry and backoff, rate limits, deduplication windows, cache expiry, last-seen bookkeeping"
flag = "`SystemTime::now()` or `UnixTimestamp(Ms)::now()` used for a duration, a timeout, an expiry window or an in-process last-seen value; or a `tokio::time::sleep` that the code trusts to move the wall clock by its duration"
paths = ["crates/**"]
patterns = ['\b(SystemTime|UnixTimestamp(Ms)?)::now\(\)']
+++

## Pattern

Code reads the wall clock (`SystemTime::now()`, `UnixTimestamp::now()`, `UnixTimestampMs::now()`)
to measure time inside the process: `later - earlier` arithmetic, a timeout target, a "window has
expired" check, or a last-request timestamp that only this process reads.

A second form uses a wall-clock value correctly, but computes one `tokio::time::sleep` from it and
then assumes that the wall clock has moved by the sleep duration. `tokio` sleeps use the monotonic
clock.

## Why it is a defect

The wall clock is not monotonic. NTP correction, a manual change or a VM host resync can move it
back or forward by a large step. WSL2 moves it by hundreds of milliseconds, sometimes seconds.

- A step back makes `now - earlier` negative. With unsigned arithmetic, this panics or wraps.
- A step forward expires windows and caches early. A step back keeps them open too long. A rate
  limiter or a deduplication window then lets a peer through, or blocks it for too long.
- After one monotonic sleep, the wall clock can still be behind the target. Code that needs a value
  strictly greater than a parent's (for example a block timestamp) then gets an equal or smaller
  value.

## Correct form

For internal timing, use `std::time::Instant` or `tokio::time::Instant` with `Duration`
(`elapsed()`, `saturating_duration_since`). `crates/p2p/src/rate_limiting.rs` is an example.

Use the wall clock only where the value crosses a boundary that needs it: block timestamps, records
that are written to disk, log lines, and values compared with another node's clock. When such code
must wait for the wall clock, re-read the clock after each sleep and loop until the condition
holds: `current_timestamp` in `crates/actors/src/block_producer.rs`. The `monotonic-time` skill
gives the full guidance.

## Detection

`cargo xtask defects scan`: the pattern matches each added `SystemTime::now()`,
`UnixTimestamp::now()` and `UnixTimestampMs::now()` in `crates/`. Most hits are correct uses (block
timestamps, persisted records, logs): for each hit, follow the value and flag it only if it feeds a
duration, a timeout, an expiry check or in-process bookkeeping. The scan misses a wall-clock value
that comes from a helper or another type (for example `chrono`), and a sleep that is trusted to move
the wall clock when the clock read is on an unchanged line. Manual check: for each new sleep, timeout
or window, find the clock it reads.
