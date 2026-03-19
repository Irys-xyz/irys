# Database Arguments Preset Trait (`IrysDatabaseArgs`)

## Status
Accepted

## Context
With `cfg!(debug_assertions)` removed from `open_or_create_db`, callers needed an ergonomic way to construct `DatabaseArguments` with the correct sync mode, growth/shrink geometry, and read transaction duration. An early design had `open_or_create_db` accept `Option<DatabaseArguments>` plus a separate `sync_mode: DbSyncMode` parameter, but this created a dual-source-of-truth problem: callers passing `Some(DatabaseArguments)` could inadvertently ignore the `sync_mode` parameter, leaving submodule DBs on MDBX defaults. This bug was caught during code review.

## Decision
Introduce an `IrysDatabaseArgs` extension trait on `DatabaseArguments` with three preset constructors:

- **`irys_default(sync_mode)`** — standard preset for most DB opens (consensus DB, submodule DBs). Unbounded read transaction duration, 10 MB growth step, 20 MB shrink threshold, caller-specified sync mode.
- **`irys_testing()`** — convenience shortcut for `irys_default(UtterlyNoSync)`.
- **`irys_cache(sync_mode)`** — larger geometry (50 MB growth / 100 MB shrink) for cache DBs with bigger write batches. Durability depends on the caller-provided sync mode, not the preset.

`open_or_create_db` now takes a required `DatabaseArguments` (not `Option`). The sync mode is embedded in the `DatabaseArguments` via the presets — there is exactly one source of truth. Specialized callers like `create_or_open_submodule_db` chain additional overrides (e.g., `.with_geometry_max_size(2 TB)`) onto the preset args.

See also: [Config-Driven Runtime Behavior](config-driven-runtime-behavior.md)

## Consequences
- Single source of truth for sync mode eliminates the class of bugs where callers pass args that silently override the sync mode parameter
- Callers must be explicit about sync mode — no hidden defaults
- The compiler catches any missed call site since the function signature changed from `Option` to required
- `open_or_create_cache_db` currently has zero external callers; it was updated for consistency and future use
- The `db_sync_mode_to_mdbx()` helper exists as a private function in each crate that needs it (database, reth-node-bridge) due to Rust's orphan rule — acceptable duplication for a 4-line match statement

## Source
PR #1228 — feat: run mode
