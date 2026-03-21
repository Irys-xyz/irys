# Config-Driven Runtime Behavior Over Build Profile Detection

## Status

Accepted

## Context

The codebase used `cfg!(debug_assertions)` as a runtime proxy for "are we in test mode" across 6+ call sites. This conflated the Rust compilation profile with operational intent:

- Debug builds couldn't run production-like settings (forced `UtterlyNoSync` DB mode, disabled VDF core pinning, reduced Reth cache), even when debugging production issues.
- Release test builds got production settings (`Durable` sync, CPU pinning) that slowed tests and caused resource contention.
- No granular control — all test-vs-production behaviors were a single boolean with no way to mix and match.

## Decision

Replace all `cfg!(debug_assertions)` runtime checks with explicit, config-driven settings on `NodeConfig`. Each behavior that was gated by the build profile gets its own config field:

- **`RunMode`** (`Production` | `Test`) — top-level informational flag. Defaults to `Production`. Does NOT automatically propagate to other settings — it's a label, not a driver. The `testing()` constructors set it alongside all related knobs. This was a deliberate choice: `RunMode` remains available for ad-hoc `is_test()` checks (startup warnings, future behaviors) without creating a hidden coupling where changing `RunMode` silently alters durability or performance.

- **`DbSyncMode`** (`Durable` | `SafeNoSync` | `UtterlyNoSync`) — explicit database sync mode. Wraps `reth_db::mdbx::SyncMode` so it can live in `irys-types` without depending on `reth_db`. Conversion is centralized via `impl From<DbSyncMode> for reth_db::mdbx::SyncMode` in `crates/types/src/config/node.rs`.

- **`DatabaseConfig`** — groups `sync_mode` and `cache_sync_mode` under `NodeConfig.database`.

- **`CorePinning`** (`Disabled` | `Auto` | `Core(usize)`) — controls VDF thread CPU pinning. Replaces a dual-check that used both `cfg!(debug_assertions)` and a `.tmp` path heuristic. `Auto` gracefully degrades to unpinned execution if `core_affinity::get_core_ids()` returns `None`.

- **`RethConfig` extensions** — `db_sync_mode`, `cross_block_cache_size_megabytes`, `additional_validation_tasks` as explicit fields.

All new fields have `#[serde(default)]` so existing config files continue to work unchanged. The `NodeConfig::testing()` constructors set test-appropriate defaults for every field using struct literal syntax, so the compiler enforces that new fields are always initialized.

## Consequences

- Operators can run debug builds with production durability, or release builds with test-optimized settings
- Each behavior is independently configurable — no all-or-nothing toggle
- The 5-second sleep penalty on debug build startup is removed; startup warnings are split into a debug-build performance warning and a separate `RunMode::Test` durability warning
- ~35 call sites across the workspace required mechanical updates to pass explicit `DbSyncMode` arguments
- The `open_or_create_db` API changed from `Option<DatabaseArguments>` to required `DatabaseArguments`, with presets provided by the `IrysDatabaseArgs` extension trait

## Source

PR #1228 — feat: run mode
