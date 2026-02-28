# nextest-monitor

A unified test wrapper for cargo-nextest that tracks pass/fail results and optionally monitors CPU and memory usage per test. Includes a reporting tool that analyzes collected data and suggests test reclassifications.

## Overview

This crate provides two binaries and a shared library:

- **`nextest-wrapper`** — Wraps every test execution. Always records pass/fail, duration, and exit code. When enabled, also samples CPU and RSS memory at 50 ms intervals.
- **`nextest-report`** — Reads the collected stats and produces summaries, reclassification suggestions, per-test detail views, and CSV/JSON exports.
- **`nextest_monitor` (lib)** — Shared types (`TestStats`, `AggregatedStats`), the `CpuMonitor`, and the `MemoryMonitor`, usable from other crates (e.g. `xtask`).

## How it works

1. `cargo xtask test` (or manual nextest setup) tells nextest to run every test binary through `nextest-wrapper`.
2. The wrapper spawns the real test, optionally polls CPU/memory while it runs, then appends a `TestStats` JSON entry to `target/nextest-monitor/stats.jsonl`.
3. After the run, `xtask` reads that file to update its failure-tracking database, and you can run `nextest-report` to inspect resource usage.

## Quick start

### Via xtask (recommended)

```bash
# Plain test run — failure tracking only, no resource monitoring
cargo xtask test

# With CPU + memory monitoring
cargo xtask test --monitor

# Rerun only previously-failed tests
cargo xtask test --rerun-failures

# Run a single package with monitoring
cargo xtask test --monitor -- -p irys-actors
```

### Analyzing results after a `--monitor` run

```bash
# Top 10 tests by peak CPU
cargo run --bin nextest-report -- summary --sort peak --top 10

# Top 10 tests by peak RSS
cargo run --bin nextest-report -- summary --sort peak_rss --top 10

# Tests that may need reclassification (heavy_ / slow_ prefix changes)
cargo run --bin nextest-report -- analyze

# Deep dive into a specific test (includes ASCII charts when --monitor + NEXTEST_MONITOR_DETAILED=1)
cargo run --bin nextest-report -- detail test_block_producer

# Export everything to CSV
cargo run --bin nextest-report -- export -o stats.csv
```

### Example: full workflow

```bash
# 1. Run tests three times with monitoring + detailed samples
for i in 1 2 3; do
  cargo xtask test --monitor -- -p irys-actors
done

# 2. Summary — sorted by P90 CPU (default)
cargo run --bin nextest-report -- summary --top 15
```

Example summary output (with `--monitor`):

```
Test Name                                     Alloc    P90   Peak    Avg Duration   ≥P90   NrPk PeakRSS  AvgRSS Runs
------------------------------------------------------------------------------------------------------------------------------
actors::block_producer::test_produce_block       1T  2.31x  3.05x  1.72x     4.2s    12%    8%   245M    180M    3
actors::validation::test_validate_block          2T  1.85x  2.44x  1.20x     2.1s    10%    6%   312M    220M    3
actors::mempool::test_add_transaction            1T  0.45x  0.62x  0.30x     0.3s    11%    5%    52M     48M    3
```

Example summary output (without `--monitor` — no PeakRSS/AvgRSS columns):

```
Test Name                                     Alloc    P90   Peak    Avg Duration   ≥P90   NrPk Runs
--------------------------------------------------------------------------------------------------------------
actors::block_producer::test_produce_block       1T  0.00x  0.00x  0.00x     4.2s     0%    0%    3
```

```bash
# 3. Check for misclassified tests
cargo run --bin nextest-report -- analyze
```

Example analyze output:

```
╔══════════════════════════════════════════════════════════════════════════════╗
║                     TESTS NEEDING RECLASSIFICATION                          ║
╚══════════════════════════════════════════════════════════════════════════════╝

┌─ actors::block_producer::test_produce_block
│  Current: (default) (1T, 60s timeout)
│  CPU:     peak: 3.05x | P90: 2.31x | P50: 1.72x | avg: 1.72x
│  Time:    3 runs | 4.2s | at P90: 12% | near peak: 8%
│  Above 1T: 3.1s (74% of runtime)
│  Memory:  peak: 245M | avg: 180M
│  ⚠️  CPU regularly exceeds 1T for 74% of runtime (>20% threshold): avg=1.72x, peak=3.05x, above 1T for 3.1s
│  → Suggested: heavy (2T, 60s timeout)
└────────────────────────────────────────────────────────────────────────────────

Summary: 1 tests need reclassification
```

The suggested fix: rename `test_produce_block` → `heavy_test_produce_block` so nextest allocates 2 threads.

### Manual setup (without xtask)

If you want to use the wrapper directly without `cargo xtask`:

```bash
# Build the wrapper
cargo build -p nextest-monitor --bin nextest-wrapper

# Set env vars
export NEXTEST_MONITOR_OUTPUT="$PWD/target/nextest-monitor/stats.jsonl"
export NEXTEST_MONITOR_CPU=1
export NEXTEST_MONITOR_MEMORY=1

# Run tests (nextest.toml already has the wrapper configured for cpu-profile)
cargo nextest run --profile cpu-profile
```

The `.config/nextest.toml` already defines the wrapper script:

```toml
experimental = ["wrapper-scripts"]

[scripts.wrapper.nextest-monitor]
command = 'nextest-wrapper'

[profile.cpu-profile]
test-threads = 12

[[profile.cpu-profile.scripts]]
filter = 'all()'
run-wrapper = 'nextest-monitor'
```

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `NEXTEST_MONITOR_OUTPUT` | `<workspace>/target/nextest-monitor/stats.jsonl` | Output JSON path |
| `NEXTEST_MONITOR_CPU` | `0` | `1` to enable CPU sampling |
| `NEXTEST_MONITOR_MEMORY` | `0` | `1` to enable RSS memory sampling |
| `NEXTEST_MONITOR_INTERVAL_MS` | `50` | Sampling interval in milliseconds |
| `NEXTEST_MONITOR_DETAILED` | `0` | `1` to record every individual sample (needed for charts in `detail`) |

When neither CPU nor memory monitoring is enabled, the wrapper still records pass/fail, duration, and exit code for every test (used by `cargo xtask test` for failure tracking).

## Report commands

| Command | Description |
|---------|-------------|
| `summary` | Table of all tests sorted by a metric (`--sort p90\|peak\|avg\|duration\|peak_rss\|avg_rss`) |
| `analyze` | Identify tests that should be reclassified (`--all` to include correctly-classified) |
| `detail <pattern>` | Deep dive on one test — stats + ASCII charts (if detailed samples exist) |
| `export` | Dump to CSV or JSON (`--format csv\|json`, `-o FILE`) |
| `config` | Show parsed classification rules from `nextest.toml` |
| `clear` | Delete the stats file |

Global options: `--input <FILE>` (stats JSON), `--config <FILE>` (nextest.toml path).

## Reclassification logic

The tool suggests reclassification based on **sustained CPU exceedance** (default threshold: 20% of runtime):

- If a test exceeds its thread allocation for >20% of runtime → suggest upgrading (e.g. add `heavy_` prefix)
- If a test uses far less than allocated → suggest downgrading
- If duration exceeds timeout → suggest adding `slow_` prefix
- Memory is **observability only** — it does not affect reclassification

## Platform support

| Platform | CPU | Memory |
|----------|-----|--------|
| Linux | `/proc/[pid]/stat` (ticks → threads) | `/proc/[pid]/statm` (resident pages × page size) |
| macOS | `ps -o %cpu=` | `ps -o rss=` (KB → bytes) |
| Other | stub (returns 0) | stub (returns 0) |
