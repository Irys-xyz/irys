# nextest-cpu-monitor

A tool to instrument and analyze CPU usage of cargo-nextest test runs, helping you correctly categorize tests by their resource consumption.

NOTE: WRITTEN ENTIRELY BY CLAUDE (with some guidance)

## Overview

This tool consists of two binaries:

1. **`nextest-cpu-wrapper`** - A wrapper that runs test binaries while monitoring their CPU usage
2. **`nextest-cpu-report`** - Analyzes collected data and suggests test reclassifications based on your `nextest.toml` configuration

## Installation

```bash
cargo install --path .
```

Or build locally:

```bash
cargo build --release
```

The binaries will be in `target/release/`.

## Quick Start

### 1. Configure wrapper scripts in `.config/nextest.toml`

```toml
# Enable experimental wrapper scripts feature
experimental = ["wrapper-scripts"]

# Define the CPU monitoring wrapper script
[scripts.wrapper.cpu-monitor]
command = 'nextest-cpu-wrapper'

# Create a profiling profile that uses the wrapper
[profile.cpu-profile]
test-threads = 1  # Run serially for accurate measurements

# Apply the wrapper to all tests in the cpu-profile
[[profile.cpu-profile.scripts]]
filter = 'all()'
run-wrapper = 'cpu-monitor'
```

### 2. Run your tests

```bash
# Clear any previous stats
nextest-cpu-report clear

# Run tests (multiple times for better averaging)
cargo nextest run --profile cpu-profile
cargo nextest run --profile cpu-profile
cargo nextest run --profile cpu-profile
```

### 3. Analyze results

```bash
# Show the configuration parsed from nextest.toml
nextest-cpu-report config

# Show summary sorted by peak CPU usage
nextest-cpu-report summary --sort peak

# Analyze and suggest reclassifications
nextest-cpu-report analyze

# Show all tests including correctly classified ones
nextest-cpu-report analyze --all

# Get JSON output for scripting
nextest-cpu-report analyze --format json
```

## Configuration

The report tool reads your classification rules directly from `.config/nextest.toml`. It parses your `[[profile.default.overrides]]` sections to understand:

- Which patterns trigger which rules (e.g., `test(/.*slow_.*/)`)
- What `threads-required` each rule sets
- What `slow-timeout` each rule sets

### Example nextest.toml

```toml
# Enable wrapper scripts (experimental feature)
experimental = ["wrapper-scripts"]

# Define the CPU monitoring wrapper
[scripts.wrapper.cpu-monitor]
command = 'nextest-cpu-wrapper'

[profile.default]
slow-timeout = { period = "30s", terminate-after = 2 }
threads-required = 1

# slow tests - only affects timeout
[[profile.default.overrides]]
filter = 'test(/.*slow_.*/)'
slow-timeout = { period = "90s", terminate-after = 2 }
priority = 100

# heavy tests - 2 threads
[[profile.default.overrides]]
filter = 'test(/.*heavy_.*/)'
threads-required = 2
priority = 90

# heavy3 tests - 3 threads  
[[profile.default.overrides]]
filter = 'test(/.*heavy3_.*/)'
threads-required = 3
priority = 80

# heavy4 tests - 4 threads  
[[profile.default.overrides]]
filter = 'test(/.*heavy4_.*/)'
threads-required = 4
priority = 70

# Profiling profile - runs tests serially with CPU monitoring
[profile.cpu-profile]
test-threads = 1

[[profile.cpu-profile.scripts]]
filter = 'all()'
run-wrapper = 'cpu-monitor'
```

Running `nextest-cpu-report config` will show:

```
╔══════════════════════════════════════════════════════════════════════════════╗
║                         NEXTEST CONFIGURATION                               ║
╚══════════════════════════════════════════════════════════════════════════════╝

Defaults:
  Threads required: 1
  Timeout:          60s

Classification Rules (by priority):
Name                 Threads      Timeout Priority  Pattern
--------------------------------------------------------------------------------
slow                       -         180s      100  .*slow_.*
heavy                      2            -       90  .*heavy_.*
heavy3                     3            -       80  .*heavy3_.*
heavy4                     4            -       70  .*heavy4_.*
```

## Reclassification Logic

The tool suggests reclassification when actual usage doesn't match the allocated resources:

### CPU Issues
- **Exceeds allocation**: If `max(peak, avg) > threads_required` → suggest upgrade
- **Over-allocated**: If test uses significantly less than allocated → suggest downgrade

### Timeout Issues  
- **Exceeds timeout**: If `avg_duration > timeout` → suggest adding slow_ or increasing timeout
- **Over-allocated**: If slow_ test finishes quickly → suggest removing slow_

## Commands

### `config`

Show the parsed configuration from nextest.toml.

```bash
nextest-cpu-report config
nextest-cpu-report --config path/to/nextest.toml config
```

### `summary`

Show a summary of all tests sorted by CPU usage.

```bash
nextest-cpu-report summary [OPTIONS]

Options:
  -s, --sort <SORT>  Sort by: p90, peak, avg, duration, nearpeak, atp90 [default: p90]
  -t, --top <N>      Show top N tests (0 for all) [default: 0]
```

Output columns:
- **Alloc**: Current allocation class (1T, 2T, 3T, 4T)
- **P90**: 90th percentile CPU usage (useful for "typical high" usage)
- **Peak**: Maximum CPU usage observed
- **Avg**: Mean CPU usage
- **Duration**: Total test duration
- **≥P90**: Percentage of runtime spent at or above P90 level
- **NrPk**: Percentage of runtime spent near peak (≥80% of peak)

### `analyze`

Analyze tests and suggest reclassifications based on your nextest.toml rules.

```bash
nextest-cpu-report analyze [OPTIONS]

Options:
  -f, --format <FORMAT>  Output format: text, json [default: text]
      --all              Show all tests, not just those needing reclassification
```

### `detail`

Show detailed stats for a specific test.

```bash
nextest-cpu-report detail <PATTERN>
```

### `export`

Export stats in various formats.

```bash
nextest-cpu-report export [OPTIONS]

Options:
  -f, --format <FORMAT>  Output format: csv, json [default: csv]
  -o, --output <FILE>    Output file (stdout if not specified)
```

### `clear`

Clear the stats file.

```bash
nextest-cpu-report clear
```

## Global Options

```bash
nextest-cpu-report [OPTIONS] <COMMAND>

Options:
  -i, --input <FILE>   Stats JSON file [default: target/nextest-cpu-stats.json]
  -c, --config <FILE>  nextest.toml path [default: .config/nextest.toml]
```

## Environment Variables (for wrapper)

| Variable | Default | Description |
|----------|---------|-------------|
| `NEXTEST_CPU_OUTPUT` | `<workspace>/target/nextest-cpu-stats.json` | Path to output JSON file |
| `NEXTEST_CPU_INTERVAL_MS` | `50` | Sampling interval in milliseconds |
| `NEXTEST_CPU_DETAILED` | `false` | Set to `1` to record all samples for charts |

## Example Output

```
╔══════════════════════════════════════════════════════════════════════════════╗
║                     TESTS NEEDING RECLASSIFICATION                          ║
╚══════════════════════════════════════════════════════════════════════════════╝

┌─ integration::database::test_concurrent_writes
│  Current: (default) (1T, 60s timeout)
│  CPU:     peak: 2.34x | P90: 2.10x | P50: 1.45x | avg: 1.52x
│  Time:    3 runs | 12.4s | at P90: 12% | near peak: 18%
│  Above 1T: 8.2s (66% of runtime)
│  ⚠️  CPU regularly exceeds 1T: P90=2.10x, peak=2.34x, above 1T for 8.2s (66%)
│  → Suggested: heavy (2T, 60s timeout)
└────────────────────────────────────────────────────────────────────────────────

┌─ e2e::slow_test_full_sync
│  Current: slow (1T, 180s timeout)
│  CPU:     peak: 0.85x | P90: 0.62x | P50: 0.35x | avg: 0.42x
│  Time:    3 runs | 45.2s | at P90: 10% | near peak: 8%
│  ⚠️  Timeout over-allocated: 45.2s duration but 180s timeout - could remove slow_
│  → Suggested: (default) (1T, 60s timeout)
└────────────────────────────────────────────────────────────────────────────────

Summary: 2 tests need reclassification
```

The **P90** metric is particularly useful: it tells you the CPU level that the test stays below 90% of the time. If P90 > allocation, the test is regularly exceeding its allocation, not just spiking.

The **≥P90** metric shows how much time is spent at or above the P90 level, helping you understand if the high CPU usage is sustained or brief.

## Platform Support

- **Linux**: Full support using `/proc` filesystem
- **macOS**: Support using `ps` command
- **Windows**: Not currently supported