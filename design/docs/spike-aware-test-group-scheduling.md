# Spike-Aware Test Group Scheduling

## Status
Accepted

## Context

The nextest-monitor tool identifies 71 tests as "spike-justified" — tests whose peak CPU usage spikes to 2-10x their average, justifying elevated thread allocations (2-4 threads) to prevent spike interference. However, these tests are I/O-bound on average (sustained CPU 0.15x-0.75x), wasting ~1800+ thread-seconds per CI run in reserved-but-idle capacity.

The core tension: these tests genuinely spike (e.g., Reth startup bursts, multi-node coordination), so simply downgrading their allocation would risk spike interference in parallel test runs. The challenge is to reclaim the wasted thread-time without reintroducing the contention that the elevated allocations prevent.

Two approaches were considered:
1. **Per-test spike padding** — keep each test's allocation high enough to absorb its own spike. This is the status quo and wastes significant thread-time.
2. **Group-based concurrency limiting** — assign spiky tests to a nextest test group with a concurrency cap, then downgrade individual allocations. The group cap prevents multiple spiky tests from spiking simultaneously, making per-test padding unnecessary.

## Decision

Use nextest's test-group feature to create a `spiky` group that limits how many spike-prone tests run concurrently. This replaces per-test thread padding with group-level concurrency control, enabling safe allocation downgrades. The implementation lives entirely in the `nextest-report` binary's new `groups` subcommand.

### Single group, not tiered

A single `spiky` group contains all 71 spike-justified tests. The alternative — tiering by peak range (e.g., `spiky-mild` for 2-4x peaks at higher concurrency, `spiky-high` for 4-10x at lower) — was rejected for initial simplicity. The `--threads` and `--spike-threshold` flags make future experimentation easy without code changes. With current data (max peak 9.17x, 12 threads), the single group gets `max-threads=1`, serializing all spiky tests. The thread-time savings from downgrading (1800s+ per run) far outweighs the wall-clock cost (~532s serial vs ~200-300s current parallel).

### Group sizing formula

`max(1, floor(available_threads / max_peak_cpu_in_group))`

This is deliberately conservative — it assumes all concurrently running spiky tests could spike simultaneously and ensures they cannot collectively exceed machine capacity. With 12 available threads and a 9.17x max peak, this yields `floor(12 / 9.17) = 1`. Operators can override via the `--threads` flag if the single-test-at-a-time cost is unacceptable for their hardware.

### TOML merging via dotted-key syntax

The existing `nextest.toml` already has a `[test-groups]` section (containing `serial`). Appending another `[test-groups]` header would produce invalid TOML (duplicate table). Instead, the generator emits dotted-key syntax:

```toml
test-groups.spiky = { max-threads = 1 }
```

This adds a key to the existing `[test-groups]` table without a new section header. The generated overrides use exact-match filters (`test(=name)`) and are appended at the end of the file. Since nextest merges non-conflicting settings from all matching overrides, the test-group assignment coexists with existing `threads-required` and `slow-timeout` settings from regex-based overrides.

### Group membership detection bypasses regex parsing

The existing `extract_test_pattern` function parses `test(/regex/)` filters into regexes for classification. It cannot handle the `test(=exact_name)` syntax or compound `|` filter expressions used by test-group overrides. Rather than extending `extract_test_pattern` (which would require fragile compound-filter parsing), group membership uses a separate `HashMap<String, String>` lookup table (test_name -> group_name). This table is built at config load time by scanning overrides that have a `test-group` field and extracting exact names from their `test(=name)` filters. The `classify()` method checks this lookup after regex pattern matching, keeping the two detection mechanisms cleanly separated.

### Idempotency and safety

The `groups` command checks for an existing `spiky` group in the config and refuses to run if one exists, with instructions to remove and regenerate. Before modifying `nextest.toml`, a `.toml.bak` backup is created. This makes the operation safe to re-run and easy to undo.

### Heuristic update for group-protected tests

The `analyze` command's spike-justified heuristic is extended: when a test is assigned to a concurrency-limited test group, it is marked `safe_to_apply = true` regardless of peak CPU. The reasoning is that the group's concurrency limit already prevents spike interference, making per-test thread padding redundant. Tests not in a group retain the existing conservative behavior (`safe_to_apply = false` when peak CPU reaches the allocation threshold).

## Consequences

- Reclaims ~1800+ wasted thread-seconds per CI run by enabling safe downgrades of 71 spike-justified tests from 2-4T to 1T
- Wall-clock cost is bounded: at `max-threads=1`, spiky tests serialize to ~532s (vs ~200-300s parallel), but total CI throughput improves because freed threads run other tests
- The `groups` → `analyze` → `apply` workflow is incremental — operators can preview changes at each step with `--dry-run`
- Future tiered grouping (splitting mild vs. extreme spikers) is a configuration change, not an architectural one
- The separate lookup-table approach for group detection means `extract_test_pattern` remains untouched — no risk of regressions in regex-based classification
- Generated TOML uses exact-match filters, so override ordering and interaction with regex-based overrides is deterministic and non-interfering

## Source
Plan: `docs/superpowers/plans/2026-03-24-spike-aware-test-groups.md` — Spike-Aware Test Group Scheduling
