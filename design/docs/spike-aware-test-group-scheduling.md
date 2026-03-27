# Spike-Aware Test Group Scheduling

## Status
Accepted

## Context

The nextest-monitor tool identifies tests as "spike-justified" — tests whose peak CPU usage spikes to 2-10x their average, justifying elevated thread allocations (2-4 threads) to prevent spike interference. However, these tests are I/O-bound on average (sustained CPU 0.15x-0.75x), wasting significant thread-seconds per CI run in reserved-but-idle capacity.

The core tension: these tests genuinely spike (e.g., Reth startup bursts, multi-node coordination), so simply downgrading their allocation would risk spike interference in parallel test runs. The challenge is to reclaim the wasted thread-time without reintroducing the contention that the elevated allocations prevent.

Two approaches were considered:
1. **Per-test spike padding** — keep each test's allocation high enough to absorb its own spike. This is the status quo and wastes significant thread-time.
2. **Group-based concurrency limiting** — assign spiky tests to a nextest test group with a concurrency cap, then downgrade individual allocations. The group cap prevents multiple spiky tests from spiking simultaneously, making per-test padding unnecessary.

## Decision

Use nextest's test-group feature to create a `spiky` group that limits how many spike-prone tests run concurrently. This replaces per-test thread padding with group-level concurrency control, enabling safe allocation downgrades.

### Single group, not tiered

A single `spiky` group contains all spike-justified tests. The alternative — tiering by peak range (e.g., `spiky-mild` for 2-4x peaks at higher concurrency, `spiky-high` for 4-10x at lower) — was rejected for initial simplicity. Future tiered grouping is a configuration change, not an architectural one.

### Phased concurrency reduction

The group is configured with `max-threads = 3` as a transitional starting point, with the target of reducing toward 1 as tests become more contention-resilient. The group sizing formula for the eventual target is:

`max(1, floor(available_threads / max_peak_cpu_in_group))`

This is deliberately conservative — it assumes all concurrently running spiky tests could spike simultaneously and ensures they cannot collectively exceed machine capacity.

### Nextest config structure

The spiky group and its overrides are hand-maintained in `.config/nextest.toml` alongside the existing `serial` group in the `[test-groups]` section:

```toml
[test-groups]
serial = { max-threads = 1 }
spiky = { max-threads = 3 }
```

Spiky tests are identified by the `spiky_` prefix and matched via a regex-based override that assigns them to the group with `threads-required = 1`:

```toml
[[profile.default.overrides]]
filter = 'test(/.*spiky_.*/)'
test-group = 'spiky'
threads-required = 1
priority = 100
```

The existing `heavy_`, `heavy3_`, and `heavy4_` overrides use set-difference filters to exclude spiky tests, so that spiky tests manage their own thread allocation independently:

```toml
filter = 'test(/.*heavy_.*/) - test(/.*spiky_.*/)'
```

### Set-difference filter parsing in `extract_test_pattern`

The `extract_test_pattern` function was extended to handle nextest's set-difference (`-`) operator in compound filter expressions. It returns a `ParsedFilter` struct with separate `include` and `exclude` fields:

```rust
struct ParsedFilter {
    include: String,          // e.g. ".*heavy_.*"
    exclude: Option<String>,  // e.g. ".*spiky_.*"
}
```

Each `ClassificationRule` carries an optional `exclude: Option<Regex>` field. During classification, a rule matches only when the test name matches the include pattern and does *not* match the exclude pattern. This allows the `heavy_` rules to correctly skip `spiky_heavy_` tests.

### Spike-justified tests remain conservative

The `analyze` command's spike-justified heuristic marks tests as `safe_to_apply = false` when they are I/O-bound on average but their peak CPU reaches the current allocation threshold. This means spike-justified tests still require manual operator review before downgrading, even when assigned to the spiky group. The reasoning is conservative: the group concurrency limit is a transitional value (`max-threads = 3`), so automated downgrades should not assume full serialization protection until the target of `max-threads = 1` is reached.

### Semantic class preservation

The `CapacityClass` type includes an `is_semantic` flag that distinguishes semantic groupings (e.g., "spiky", "serial") from resource-level classes (e.g., "heavy", "heavy3"). Semantic classes are preserved during reclassification even when their threads/timeout values match defaults — a test renamed from `spiky_heavy4_foo` to `spiky_foo` retains the `spiky_` prefix because it conveys group membership, not just a resource allocation.

## Consequences

- Reclaims wasted thread-seconds per CI run by enabling safe downgrades of spike-justified tests to `threads-required = 1` while the group limits concurrency
- Wall-clock cost is bounded by the group's `max-threads` setting, currently 3 (transitioning toward 1)
- The `analyze` → `apply` workflow is incremental — operators can preview changes with `--dry-run`
- Future tiered grouping (splitting mild vs. extreme spikers) is a configuration change, not an architectural one
- The `extract_test_pattern` function handles set-difference filters, so `heavy_` rules correctly exclude `spiky_` tests without additional lookup mechanisms
- All configuration is hand-maintained in `.config/nextest.toml` — no code generation step required

## Source
Plan: `docs/superpowers/plans/2026-03-24-spike-aware-test-groups.md` — Spike-Aware Test Group Scheduling
