# RunMode Flag Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace all `cfg!(debug_assertions)` runtime checks with an explicit `RunMode` enum and granular per-behavior configuration parameters, decoupling test/prod behavior from the Rust compilation profile.

**Architecture:** New types (`RunMode`, `DbSyncMode`, `CorePinning`, `DatabaseConfig`) are added to `irys-types`. Each `cfg!(debug_assertions)` call site is replaced by reading a specific config field. The `NodeConfig::testing()` constructors set test defaults. All new fields have serde defaults so existing config files continue to work.

**Tech Stack:** Rust, serde, reth_db (for `SyncMode` conversion), core_affinity (for CPU pinning)

**Spec:** `docs/superpowers/specs/2026-03-18-run-mode-flag-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `crates/types/src/config/node.rs` | Modify | Add `RunMode`, `DbSyncMode`, `CorePinning` enums, `DatabaseConfig` struct, new fields on `NodeConfig`/`RethConfig`/`VdfNodeConfig`, update constructors |
| `crates/database/src/database.rs` | Modify | Add `sync_mode: DbSyncMode` param, `From<DbSyncMode>` impl, remove `cfg!(debug_assertions)` |
| `crates/storage/src/irys_consensus_data_db.rs` | Modify | Thread `DbSyncMode` param through to `open_or_create_db` |
| `crates/database/src/submodule/db.rs` | Modify | Add `DbSyncMode` param to `create_or_open_submodule_db` |
| `crates/chain/src/chain.rs` | Modify | Pass config values through `init_irys_db`, replace VDF dual-check |
| `crates/reth-node-bridge/src/node.rs` | Modify | Read from `node_config.reth` fields instead of `cfg!(debug_assertions)` |
| `crates/chain/src/main.rs` | Modify | Reword debug warning, add run_mode warning |
| ~35 test files across `crates/database/`, `crates/domain/`, `crates/actors/`, `crates/p2p/` | Modify | Add `DbSyncMode::UtterlyNoSync` argument to DB open calls |
| `crates/utils/debug-utils/src/db.rs` | Modify | Add `DbSyncMode::Durable` argument |

---

## Task 1: Add new types to `irys-types`

**Files:**
- Modify: `crates/types/src/config/node.rs`

This task adds all the new enums and structs. No call sites change yet — the compiler will tell us what to fix in subsequent tasks since the constructors use struct literal syntax.

- [ ] **Step 1: Add `RunMode` enum**

Add before the `NodeConfig` struct definition (before line 25):

```rust
/// Controls whether the node runs with production or test optimizations.
/// Test mode uses faster but less durable settings (e.g. no fsync, no CPU pinning).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RunMode {
    #[default]
    Production,
    Test,
}

impl RunMode {
    pub fn is_test(&self) -> bool {
        matches!(self, RunMode::Test)
    }
}
```

- [ ] **Step 2: Add `DbSyncMode` enum**

Add next to `RunMode`:

```rust
/// Database sync mode controlling durability vs write performance.
/// Wraps `reth_db::mdbx::SyncMode` so it can live in `irys-types` without
/// depending on `reth_db`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DbSyncMode {
    /// fsync after every transaction — safest, slowest.
    Durable,
    /// No fsync, but DB integrity preserved on crash (uncommitted txns lost).
    SafeNoSync,
    /// No fsync, no crash safety — fastest, for tests only.
    UtterlyNoSync,
}
```

- [ ] **Step 3: Add `DatabaseConfig` struct**

```rust
/// Database durability and sync settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Sync mode for the main Irys consensus database.
    #[serde(default = "default_db_sync_mode")]
    pub sync_mode: DbSyncMode,

    /// Sync mode for the cache database.
    #[serde(default = "default_cache_db_sync_mode")]
    pub cache_sync_mode: DbSyncMode,
}

fn default_db_sync_mode() -> DbSyncMode {
    DbSyncMode::Durable
}

fn default_cache_db_sync_mode() -> DbSyncMode {
    DbSyncMode::SafeNoSync
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            sync_mode: default_db_sync_mode(),
            cache_sync_mode: default_cache_db_sync_mode(),
        }
    }
}
```

- [ ] **Step 4: Add `CorePinning` enum**

```rust
/// Controls whether and how the VDF thread is pinned to a CPU core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CorePinning {
    /// Do not pin the VDF thread to any core.
    Disabled,
    /// Pin to the first available core (current production behavior).
    Auto,
    /// Pin to a specific core by ID.
    Core(usize),
}

impl Default for CorePinning {
    fn default() -> Self {
        CorePinning::Auto
    }
}
```

- [ ] **Step 5: Add `run_mode` and `database` fields to `NodeConfig`**

Add to the `NodeConfig` struct (around line 25):

```rust
    /// Controls test vs production behavior for durability, caching, and resource allocation.
    #[serde(default)]
    pub run_mode: RunMode,

    /// Database durability settings.
    #[serde(default)]
    pub database: DatabaseConfig,
```

Add builder method on `impl NodeConfig` (near existing builder methods):

```rust
    pub fn with_run_mode(mut self, run_mode: RunMode) -> Self {
        self.run_mode = run_mode;
        self
    }
```

- [ ] **Step 6: Add fields to `RethConfig`**

Add to `RethConfig` struct (line 386). Add default functions nearby:

```rust
    /// Sync mode for the Reth embedded database.
    #[serde(default = "default_reth_db_sync_mode")]
    pub db_sync_mode: DbSyncMode,

    /// Override for Reth engine cross-block cache size in MB.
    /// None means use Reth's default.
    #[serde(default)]
    pub cross_block_cache_size_megabytes: Option<usize>,

    /// Number of additional transaction validation tasks.
    #[serde(default = "default_additional_validation_tasks")]
    pub additional_validation_tasks: usize,
```

```rust
fn default_reth_db_sync_mode() -> DbSyncMode {
    DbSyncMode::Durable
}

fn default_additional_validation_tasks() -> usize {
    2
}
```

- [ ] **Step 7: Add `core_pinning` field to `VdfNodeConfig`**

Add to the `VdfNodeConfig` struct (line 641) and update its `Default` impl (line 646):

```rust
pub struct VdfNodeConfig {
    /// Maximum number of threads to use for parallel VDF verification
    pub parallel_verification_thread_limit: usize,

    /// Controls VDF thread core pinning.
    #[serde(default)]
    pub core_pinning: CorePinning,
}

impl Default for VdfNodeConfig {
    fn default() -> Self {
        Self {
            // TODO: default to something like numcpus - 4
            parallel_verification_thread_limit: 4,
            core_pinning: CorePinning::default(),
        }
    }
}
```

- [ ] **Step 8: Update `testing_with_signer()` constructor**

In `testing_with_signer()` (line 818), add the new fields to the `Self { ... }` struct literal:

```rust
    run_mode: RunMode::Test,
    database: DatabaseConfig {
        sync_mode: DbSyncMode::UtterlyNoSync,
        cache_sync_mode: DbSyncMode::UtterlyNoSync,
    },
```

Update the `reth:` field to include new fields:

```rust
    reth: RethConfig {
        network: RethNetworkConfig { /* existing */ },
        db_sync_mode: DbSyncMode::UtterlyNoSync,
        cross_block_cache_size_megabytes: Some(10),
        additional_validation_tasks: 0,
    },
```

Update the `vdf:` field:

```rust
    vdf: VdfNodeConfig {
        parallel_verification_thread_limit: 4,
        core_pinning: CorePinning::Disabled,
    },
```

- [ ] **Step 9: Update `testnet()` constructor**

In `testnet()` (line 952), add the new fields with production defaults:

```rust
    run_mode: RunMode::Production,
    database: DatabaseConfig::default(),
```

Update `reth:` and `vdf:` similarly with production defaults (`DbSyncMode::Durable`, `None`, `2`, `CorePinning::Auto`).

- [ ] **Step 10: Add unit tests for new types**

Add a test module in `crates/types/src/config/node.rs` (or in an existing test module if one exists nearby). These verify serde round-trips and default behavior:

```rust
#[cfg(test)]
mod run_mode_tests {
    use super::*;

    #[test]
    fn run_mode_defaults_to_production() {
        assert_eq!(RunMode::default(), RunMode::Production);
        assert!(!RunMode::default().is_test());
        assert!(RunMode::Test.is_test());
    }

    #[test]
    fn db_sync_mode_serde_round_trip() {
        for mode in [DbSyncMode::Durable, DbSyncMode::SafeNoSync, DbSyncMode::UtterlyNoSync] {
            let json = serde_json::to_string(&mode).unwrap();
            let deserialized: DbSyncMode = serde_json::from_str(&json).unwrap();
            assert_eq!(mode, deserialized);
        }
    }

    #[test]
    fn core_pinning_serde_round_trip() {
        for pinning in [CorePinning::Disabled, CorePinning::Auto, CorePinning::Core(42)] {
            let json = serde_json::to_string(&pinning).unwrap();
            let deserialized: CorePinning = serde_json::from_str(&json).unwrap();
            assert_eq!(pinning, deserialized);
        }
    }

    #[test]
    fn database_config_defaults() {
        let config = DatabaseConfig::default();
        assert_eq!(config.sync_mode, DbSyncMode::Durable);
        assert_eq!(config.cache_sync_mode, DbSyncMode::SafeNoSync);
    }
}
```

- [ ] **Step 11: Run compile check and tests**

Run: `cargo check -p irys-types`

Expected: PASS

Run: `cargo nextest run -p irys-types run_mode_tests`

Expected: PASS

- [ ] **Step 12: Commit**

```bash
git add crates/types/src/config/node.rs
git commit -m "feat: add RunMode, DbSyncMode, CorePinning, DatabaseConfig types

Add explicit configuration types to replace cfg!(debug_assertions)
runtime checks. New fields on NodeConfig, RethConfig, VdfNodeConfig
with serde defaults for backward compatibility."
```

---

## Task 2: Update database functions to accept `DbSyncMode`

**Files:**
- Modify: `crates/database/src/database.rs:39-86`
- Modify: `crates/database/src/lib.rs` (if needed for re-export)

- [ ] **Step 1: Add `db_sync_mode_to_mdbx` helper function**

At the top of `crates/database/src/database.rs`, add the import and a local conversion helper. A `From` impl is not possible here due to Rust's orphan rule — neither `DbSyncMode` (from `irys-types`) nor `SyncMode` (from `reth_db`) is local to this crate.

```rust
use irys_types::DbSyncMode;

pub fn db_sync_mode_to_mdbx(mode: DbSyncMode) -> SyncMode {
    match mode {
        DbSyncMode::Durable => SyncMode::Durable,
        DbSyncMode::SafeNoSync => SyncMode::SafeNoSync,
        DbSyncMode::UtterlyNoSync => SyncMode::UtterlyNoSync,
    }
}
```

Note: `SyncMode` here is `reth_db::mdbx::SyncMode`, already imported at line 32. The same helper pattern is used in `crates/reth-node-bridge/src/node.rs`. This function must be `pub` because `create_or_open_submodule_db` needs to call it when constructing custom `DatabaseArguments`.

- [ ] **Step 2: Update `open_or_create_db` signature**

Change the function at line 39 from:

```rust
pub fn open_or_create_db<P: AsRef<Path>, T: TableSet + TableInfo>(
    path: P,
    tables: &[T],
    args: Option<DatabaseArguments>,
) -> eyre::Result<DatabaseEnv> {
```

to:

```rust
pub fn open_or_create_db<P: AsRef<Path>, T: TableSet + TableInfo>(
    path: P,
    tables: &[T],
    args: Option<DatabaseArguments>,
    sync_mode: DbSyncMode,
) -> eyre::Result<DatabaseEnv> {
```

Replace the `cfg!(debug_assertions)` block (lines 50-54) with:

```rust
            .with_sync_mode(Some(db_sync_mode_to_mdbx(sync_mode)))
```

- [ ] **Step 3: Update `open_or_create_cache_db` signature**

Change the function at line 64 similarly — add `sync_mode: DbSyncMode` parameter and replace `cfg!(debug_assertions)` (lines 79-83) with:

```rust
            .with_sync_mode(Some(db_sync_mode_to_mdbx(sync_mode)))
```

Also update the call to `open_or_create_db` at line 85 to pass the new `sync_mode` argument.

**Note:** `open_or_create_cache_db` currently has zero external callers, so `DatabaseConfig::cache_sync_mode` has no consumer yet. We update the function for consistency and future use.

- [ ] **Step 4: Run compile check on this crate**

Run: `cargo check -p irys-database`

Expected: Errors in test code within this crate. The production code should be fine since callers are in other crates.

- [ ] **Step 5: Commit**

```bash
git add crates/database/src/database.rs
git commit -m "feat: add DbSyncMode param to open_or_create_db

Replace cfg!(debug_assertions) with explicit DbSyncMode parameter.
Sync mode is only applied when args is None; custom DatabaseArguments
take precedence."
```

---

## Task 3: Update database wrapper functions

**Files:**
- Modify: `crates/storage/src/irys_consensus_data_db.rs:6`
- Modify: `crates/database/src/submodule/db.rs:24`

- [ ] **Step 1: Update `open_or_create_irys_consensus_data_db`**

In `crates/storage/src/irys_consensus_data_db.rs`, change the signature at line 6 to accept `DbSyncMode` and pass it through:

```rust
use irys_types::DbSyncMode;

pub fn open_or_create_irys_consensus_data_db(
    path: &PathBuf,
    sync_mode: DbSyncMode,
) -> eyre::Result<DatabaseEnv> {
    open_or_create_db(path, IrysTables::ALL, None, sync_mode)
}
```

- [ ] **Step 2: Update `create_or_open_submodule_db`**

In `crates/database/src/submodule/db.rs`, the function passes `Some(args)` to `open_or_create_db`. Since `open_or_create_db` only applies `sync_mode` in its `unwrap_or` (i.e. when `args` is `None`), we must call `.with_sync_mode(...)` on the `DatabaseArguments` ourselves. First make `db_sync_mode_to_mdbx` public in `database.rs`, then import and use it here:

```rust
use irys_types::DbSyncMode;
use crate::database::db_sync_mode_to_mdbx;

pub fn create_or_open_submodule_db<P: AsRef<Path>>(
    path: P,
    sync_mode: DbSyncMode,
) -> eyre::Result<DatabaseEnv> {
    open_or_create_db(
        path,
        SubmoduleTables::ALL,
        Some(
            DatabaseArguments::new(ClientVersion::default())
                .with_max_read_transaction_duration(Some(MaxReadTransactionDuration::Unbounded))
                .with_growth_step((10 * MEGABYTE).into())
                .with_shrink_threshold((20 * MEGABYTE).try_into()?)
                .with_geometry_max_size(Some(2 * TERABYTE))
                .with_sync_mode(Some(db_sync_mode_to_mdbx(sync_mode))),
        ),
        sync_mode,
    )
}
```

- [ ] **Step 3: Commit**

```bash
git add crates/storage/src/irys_consensus_data_db.rs crates/database/src/submodule/db.rs
git commit -m "feat: thread DbSyncMode through DB wrapper functions"
```

---

## Task 4: Update production call sites in `chain.rs` and `storage_module.rs`

**Files:**
- Modify: `crates/chain/src/chain.rs:2413` (`init_irys_db`)
- Modify: `crates/chain/src/chain.rs:2013-2047` (VDF core pinning)
- Modify: `crates/domain/src/models/storage_module.rs:~267` (`StorageModule::open`)

- [ ] **Step 1: Update `init_irys_db`**

At line 2413, the function receives `&NodeConfig`. Pass `node_config.database.sync_mode` through:

```rust
fn init_irys_db(node_config: &NodeConfig) -> Result<DatabaseProvider, eyre::Error> {
    let irys_db_env = open_or_create_irys_consensus_data_db(
        &node_config.irys_consensus_data_dir(),
        node_config.database.sync_mode,
    )?;
    // ... rest unchanged
}
```

Add the import for `DbSyncMode` if not already present (it should come through `irys_types::*`).

- [ ] **Step 2: Replace VDF core pinning dual-check**

Replace the block at lines 2013-2047. Remove the `is_test_based_on_base_dir` and `is_test_based_on_cfg_flag` variables and the panic. Replace with:

```rust
match config.vdf.core_pinning {
    CorePinning::Disabled => {
        info!("VDF core pinning disabled");
    }
    CorePinning::Auto => {
        let core_ids = core_affinity::get_core_ids().expect("Failed to get core IDs");
        for core in core_ids {
            if core_affinity::set_for_current(core) {
                info!("VDF thread pinned to core {:?}", core);
                break;
            }
        }
    }
    CorePinning::Core(id) => {
        let core = core_affinity::CoreId { id };
        if core_affinity::set_for_current(core) {
            info!("VDF thread pinned to core {:?}", core);
        } else {
            warn!("Failed to pin VDF thread to core {}", id);
        }
    }
}
```

Add `use irys_types::CorePinning;` if needed.

- [ ] **Step 3: Update `StorageModule::open` in `crates/domain/src/models/storage_module.rs`**

This is a **production call site**. `StorageModule::open` at line ~267 calls `create_or_open_submodule_db(&submodule_db_path)`. The method receives a `&Config` which contains `node_config`. Update to:

```rust
create_or_open_submodule_db(&submodule_db_path, config.node_config.database.sync_mode)
```

Search for any other `create_or_open_submodule_db` callers across the workspace:

```bash
grep -rn "create_or_open_submodule_db" crates/ --include="*.rs"
```

Update each caller with the appropriate `DbSyncMode`.

- [ ] **Step 4: Run compile check**

Run: `cargo check -p irys-chain`

Expected: PASS (or errors only in test code fixed in later tasks)

- [ ] **Step 5: Commit**

```bash
git add crates/chain/src/chain.rs
git commit -m "feat: use config-driven DB sync mode and VDF core pinning

Replace cfg!(debug_assertions) and .tmp path heuristic in chain.rs
with explicit config fields: node_config.database.sync_mode for DB init,
config.vdf.core_pinning for VDF thread pinning."
```

---

## Task 5: Update Reth node configuration

**Files:**
- Modify: `crates/reth-node-bridge/src/node.rs:100-201`

- [ ] **Step 1: Replace `cfg!(debug_assertions)` for cache/validation (line 169)**

Replace:

```rust
if cfg!(debug_assertions) {
    reth_config.engine.cross_block_cache_size = 10;
} else {
    reth_config.txpool.additional_validation_tasks = 2;
}
```

With:

```rust
if let Some(cache_size) = node_config.reth.cross_block_cache_size_megabytes {
    reth_config.engine.cross_block_cache_size = cache_size;
}
reth_config.txpool.additional_validation_tasks = node_config.reth.additional_validation_tasks;
```

- [ ] **Step 2: Replace `cfg!(debug_assertions)` for DB sync mode (line 197)**

Replace:

```rust
.with_sync_mode(if cfg!(debug_assertions) {
    Some(SyncMode::UtterlyNoSync)
} else {
    Some(SyncMode::Durable)
})
```

With:

Add a local `db_sync_mode_to_mdbx` helper (same pattern as in `crates/database/src/database.rs`):

```rust
fn db_sync_mode_to_mdbx(mode: irys_types::DbSyncMode) -> SyncMode {
    match mode {
        irys_types::DbSyncMode::Durable => SyncMode::Durable,
        irys_types::DbSyncMode::SafeNoSync => SyncMode::SafeNoSync,
        irys_types::DbSyncMode::UtterlyNoSync => SyncMode::UtterlyNoSync,
    }
}
```

Then use: `.with_sync_mode(Some(db_sync_mode_to_mdbx(node_config.reth.db_sync_mode)))`

Note: a `From` impl is not possible due to Rust's orphan rule — neither type is local to this crate.

- [ ] **Step 3: Fix any `RethConfig` struct literal sites in this crate**

Search for `RethConfig {` in `crates/reth-node-bridge/` and add the new fields. These are likely only in tests if any.

- [ ] **Step 4: Run compile check**

Run: `cargo check -p irys-reth-node-bridge`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/reth-node-bridge/src/node.rs
git commit -m "feat: use config-driven Reth DB sync mode, cache size, validation tasks

Replace cfg!(debug_assertions) in Reth node setup with explicit
config fields from node_config.reth."
```

---

## Task 6: Update startup warnings in `main.rs`

**Files:**
- Modify: `crates/chain/src/main.rs:26-29`

- [ ] **Step 1: Reword debug build warning and remove sleep**

Replace lines 26-29:

```rust
if cfg!(debug_assertions) {
    eprintln!("WARNING: cfg!(debug_assertions) is TRUE. this setting toggles certain performance and durability settings to improve test performance, which is detrimental to production usecases. RECOMPILE WITH --release, or wait 5 seconds.");
    sleep(Duration::from_secs(5)).await;
}
```

With:

```rust
if cfg!(debug_assertions) {
    eprintln!("WARNING: Running a debug build. Performance will be degraded. RECOMPILE WITH --release for production use.");
}
```

- [ ] **Step 2: Add run_mode warning after config load**

After line 61 (`let config = load_config()?;`), add:

```rust
if config.run_mode.is_test() {
    eprintln!("WARNING: run_mode is set to Test. Durability and performance settings are optimized for testing, not production. If this is unintentional, check your node configuration.");
}
```

- [ ] **Step 3: Remove unused `sleep` import if no longer needed**

Check if `sleep` and `Duration` are still used elsewhere in main. If not, remove the imports.

- [ ] **Step 4: Run compile check**

Run: `cargo check -p irys-chain`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/chain/src/main.rs
git commit -m "feat: separate debug build and run_mode startup warnings

Debug build warning no longer sleeps. New run_mode warning after
config load catches Test mode misconfiguration."
```

---

## Task 7: Fix all `open_or_create_db` callers in `crates/database/` tests

**Files:**
- Modify: `crates/database/src/database.rs` (test section)
- Modify: `crates/database/src/db_index.rs` (tests)
- Modify: `crates/database/src/submodule/tables.rs` (tests)
- Modify: `crates/database/src/submodule/db.rs` (tests)
- Modify: `crates/database/src/migration.rs` (tests)
- Modify: `crates/database/examples/dupsort.rs`

This is mechanical: find every `open_or_create_db(path, tables, None)` and change to `open_or_create_db(path, tables, None, DbSyncMode::UtterlyNoSync)`. Similarly for `create_or_open_submodule_db`.

- [ ] **Step 1: Find all call sites**

Run in the worktree:

```bash
grep -rn "open_or_create_db\|open_or_create_cache_db\|create_or_open_submodule_db" crates/database/src/ crates/database/examples/ --include="*.rs"
```

- [ ] **Step 2: Update each call site**

Add `use irys_types::DbSyncMode;` to each file's test module imports, then add `DbSyncMode::UtterlyNoSync` as the last argument to each `open_or_create_db(..., None)` call.

For `create_or_open_submodule_db` calls in tests, add `DbSyncMode::UtterlyNoSync`.

- [ ] **Step 3: Run compile check and tests**

Run: `cargo check -p irys-database`

Expected: PASS

Run: `cargo nextest run -p irys-database`

Expected: PASS (existing tests still work with explicit UtterlyNoSync — same behavior as before in debug builds)

- [ ] **Step 4: Commit**

```bash
git add crates/database/
git commit -m "fix: update database crate test callers with explicit DbSyncMode"
```

---

## Task 8: Fix all `open_or_create_db` callers in `crates/domain/`

**Files:**
- Modify: `crates/domain/tests/storage_module_index_tests.rs`
- Modify: `crates/domain/tests/block_index_db_schema_tests.rs`
- Modify: `crates/domain/src/models/block_index.rs` (test section)

- [ ] **Step 1: Find and update call sites**

```bash
grep -rn "open_or_create_db" crates/domain/ --include="*.rs"
```

Add `DbSyncMode::UtterlyNoSync` to each call.

- [ ] **Step 2: Run compile check**

Run: `cargo check -p irys-domain`

Expected: PASS

- [ ] **Step 3: Commit**

```bash
git add crates/domain/
git commit -m "fix: update domain crate test callers with explicit DbSyncMode"
```

---

## Task 9: Fix all callers in `crates/actors/`

**Files:**
- Modify: `crates/actors/tests/block_validation_tests.rs`
- Modify: `crates/actors/src/anchor_validation_tests.rs`
- Modify: `crates/actors/src/cache_service.rs` (tests)
- Modify: `crates/actors/tests/epoch_snapshot_tests.rs`
- Modify: `crates/actors/src/block_validation.rs` (tests)

These call both `open_or_create_db` and `open_or_create_irys_consensus_data_db`.

- [ ] **Step 1: Find all call sites**

```bash
grep -rn "open_or_create_db\|open_or_create_irys_consensus_data_db\|create_or_open_submodule_db" crates/actors/ --include="*.rs"
```

- [ ] **Step 2: Update each call site**

Add `DbSyncMode::UtterlyNoSync` as appropriate. Also fix any `RethConfig { ... }` or `VdfNodeConfig { ... }` struct literals that are now missing the new fields.

- [ ] **Step 3: Run compile check**

Run: `cargo check -p irys-actors`

Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/actors/
git commit -m "fix: update actors crate test callers with explicit DbSyncMode and new config fields"
```

---

## Task 10: Fix all callers in `crates/p2p/`

**Files:**
- Modify: `crates/p2p/src/server.rs` (tests)
- Modify: `crates/p2p/src/chain_sync.rs` (tests, ~4 sites)
- Modify: `crates/p2p/src/peer_network_service.rs` (tests)
- Modify: `crates/p2p/src/tests/util.rs`
- Modify: `crates/p2p/src/tests/block_pool/mod.rs`

These call `open_or_create_irys_consensus_data_db`.

- [ ] **Step 1: Find all call sites**

```bash
grep -rn "open_or_create_irys_consensus_data_db\|open_or_create_db\|create_or_open_submodule_db" crates/p2p/ --include="*.rs"
```

- [ ] **Step 2: Update each call site**

Add `DbSyncMode::UtterlyNoSync`. Also fix any `RethConfig` or `VdfNodeConfig` struct literals.

- [ ] **Step 3: Run compile check**

Run: `cargo check -p irys-p2p`

Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/p2p/
git commit -m "fix: update p2p crate test callers with explicit DbSyncMode and new config fields"
```

---

## Task 11: Fix remaining callers and struct literals across the workspace

**Files:**
- Modify: `crates/utils/debug-utils/src/db.rs`
- Any other files that fail `cargo xtask check`

- [ ] **Step 1: Update debug-utils**

In `crates/utils/debug-utils/src/db.rs`, change:

```rust
open_or_create_db(path, IrysTables::ALL, None)
```

to:

```rust
open_or_create_db(path, IrysTables::ALL, None, DbSyncMode::Durable)
```

(This utility operates on real databases.)

- [ ] **Step 2: Run full workspace compile check**

Run: `cargo xtask check`

Expected: PASS. If there are remaining errors, they will be from `RethConfig { ... }` or `VdfNodeConfig { ... }` struct literals elsewhere that need the new fields added.

- [ ] **Step 3: Fix any remaining compile errors**

Search workspace-wide for struct literal construction of `RethConfig`, `VdfNodeConfig`, and `NodeConfig` that are missing new fields:

```bash
grep -rn "RethConfig {" crates/ --include="*.rs"
grep -rn "VdfNodeConfig {" crates/ --include="*.rs"
```

Add the new fields with appropriate defaults (test defaults in test code, production defaults elsewhere).

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "fix: update remaining callers and struct literals across workspace"
```

---

## Task 12: Run full checks and fix any issues

- [ ] **Step 1: Run fmt**

Run: `cargo fmt --all`

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --tests --all-targets`

Fix any warnings (unused imports, etc.).

- [ ] **Step 3: Run full test suite**

Run: `cargo xtask check`

Expected: PASS

Run: `cargo xtask test`

Expected: All existing tests pass. Behavior is unchanged — running with test configs (`NodeConfig::testing()` / `ConsensusConfig::testing()` / `run_mode: Test`) yields `UtterlyNoSync`; running with default config yields `Durable`.

- [ ] **Step 4: Commit any fixes**

```bash
git add -A
git commit -m "chore: fix fmt, clippy, and test issues"
```

---

## Task 13: Run local-checks

- [ ] **Step 1: Run full CI-equivalent checks**

Run: `cargo xtask local-checks`

This runs fmt, check, clippy, unused-deps, and typos — approximately 99% of CI.

Expected: PASS

- [ ] **Step 2: Fix any issues**

If `local-checks` reports issues (unused deps, typos in new code, etc.), fix and commit:

```bash
cargo xtask local-checks --fix
git add -A
git commit -m "chore: fix local-checks issues"
```
