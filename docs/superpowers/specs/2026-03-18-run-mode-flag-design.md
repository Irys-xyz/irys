# Replace `cfg!(debug_assertions)` with Explicit `RunMode` and Granular Config Parameters

## Problem

The codebase uses `cfg!(debug_assertions)` as a runtime proxy for "are we in test mode" across 6 call sites. This conflates the compilation profile with operational intent:

- **Debug builds can't run production-like:** Running a debug build (for debugging with symbols) forces `UtterlyNoSync` DB mode, disables VDF core pinning, and reduces Reth cache — even if you want production behavior.
- **Release tests can't get test optimizations:** Running `cargo nextest run --release` gets `Durable` sync mode, CPU core pinning, and other production settings that slow tests and cause resource contention.
- **No granular control:** All test-vs-prod behaviors are a single boolean with no way to mix and match.

## Design

### 1. `RunMode` Enum

A new enum in `irys-types`, on `NodeConfig`:

```rust
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

Added to `NodeConfig`:

```rust
pub struct NodeConfig {
    #[serde(default)]
    pub run_mode: RunMode,
    // ... existing fields
}
```

- Defaults to `Production` via `#[serde(default)]` (existing config files keep working)
- Can be set via config file (`run_mode: "Test"`) for release-build test scenarios
- `testing()` / `testing_with_signer()` / `testing_with_epochs()` set `RunMode::Test`
- `.with_run_mode(RunMode)` builder method provided

`RunMode` remains available for ad-hoc `is_test()` checks (startup warning, future behaviors that don't warrant their own config knob).

### 2. `DbSyncMode` Wrapper Enum

`reth_db::mdbx::SyncMode` needs to be accessible from `irys-types` for the config fields, but we don't want to pull `reth_db` into `irys-types`' public API. Define a wrapper:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DbSyncMode {
    Durable,
    SafeNoSync,
    UtterlyNoSync,
}
```

A local helper function `db_sync_mode_to_mdbx(mode: DbSyncMode) -> SyncMode` converts between the two types. This helper is defined in each crate that needs it (`database`, `reth-node-bridge`) because Rust's orphan rule prevents a `From` impl when neither type is local to the crate.

**Note:** `NodeConfig` already has a field `sync_mode: SyncMode` where `SyncMode` is the chain sync strategy (`Full` / `Trusted`). The `DbSyncMode` name avoids any confusion between chain sync and database sync.

### 3. Granular Configuration Parameters

Each behavior currently gated by `cfg!(debug_assertions)` becomes its own config field. The `testing()` constructors set test-appropriate defaults, but any field can be independently overridden.

#### 3a. Database Configuration (`DatabaseConfig` on `NodeConfig`)

A new sub-config struct groups database-related settings:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Sync mode for the main Irys consensus database.
    /// Test default: UtterlyNoSync. Production default: Durable.
    #[serde(default = "default_db_sync_mode")]
    pub sync_mode: DbSyncMode,

    /// Sync mode for the cache database.
    /// Test default: UtterlyNoSync. Production default: SafeNoSync.
    #[serde(default = "default_cache_db_sync_mode")]
    pub cache_sync_mode: DbSyncMode,
}
```

Added to `NodeConfig`:

```rust
pub struct NodeConfig {
    #[serde(default)]
    pub database: DatabaseConfig,
    // ...
}
```

- `Default` impl: `sync_mode: DbSyncMode::Durable`, `cache_sync_mode: DbSyncMode::SafeNoSync`
- `testing_with_signer()` sets both to `DbSyncMode::UtterlyNoSync`
- Accessed as `node_config.database.sync_mode` / `node_config.database.cache_sync_mode`

`open_or_create_db` and `open_or_create_cache_db` receive a `DbSyncMode` parameter instead of making the decision internally. The caller passes the appropriate config value.

#### 3b. Reth Configuration (`RethConfig`)

```rust
pub struct RethConfig {
    pub network: RethNetworkConfig,

    /// Sync mode for the Reth embedded database.
    /// Test default: UtterlyNoSync. Production default: Durable.
    #[serde(default = "default_reth_db_sync_mode")]
    pub db_sync_mode: DbSyncMode,

    /// Override for Reth engine cross-block cache size.
    /// Test default: Some(10). Production default: None (use Reth default).
    #[serde(default)]
    pub cross_block_cache_size_megabytes: Option<usize>,

    /// Number of additional transaction validation tasks.
    /// Test default: 0. Production default: 2.
    #[serde(default = "default_additional_validation_tasks")]
    pub additional_validation_tasks: usize,
}
```

- `default_reth_db_sync_mode()` returns `DbSyncMode::Durable`
- `default_additional_validation_tasks()` returns `2`
- `testing_with_signer()` sets: `db_sync_mode: DbSyncMode::UtterlyNoSync`, `cross_block_cache_size_megabytes: Some(10)`, `additional_validation_tasks: 0`

The Reth `run_node` function reads these fields directly from `node_config.reth` instead of checking `cfg!(debug_assertions)`.

#### 3c. VDF Core Pinning (`VdfNodeConfig`)

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

pub struct VdfNodeConfig {
    pub parallel_verification_thread_limit: usize,

    /// Controls VDF thread core pinning.
    /// Test default: Disabled. Production default: Auto.
    #[serde(default)]
    pub core_pinning: CorePinning,
}
```

- Default: `CorePinning::Auto` (pin to first available core, matches current production behavior)
- `testing_with_signer()` sets `CorePinning::Disabled`
- `CorePinning::Core(id)` allows operators to specify an exact core, useful for tuning on known hardware

The VDF thread spawn in `chain.rs` becomes:

```rust
match vdf_config.core_pinning {
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

Replaces the dual-check (`cfg!(debug_assertions)` + `.tmp` path heuristic) and the associated panic in `chain.rs`. The FIXME comment requesting exactly this change is removed.

### 4. Startup Warnings (`crates/chain/src/main.rs`)

Two independent warnings:

1. **Debug build warning** (stays at current location, before config load):

   ```text
   WARNING: Running a debug build. Performance will be degraded.
   RECOMPILE WITH --release for production use.
   ```

   Reworded to focus on compilation profile (optimizations, debug symbols) since durability is no longer tied to it. No sleep — just a warning.

2. **Test mode warning** (after `load_config()`, before `IrysNode::bind_listeners()`):

   ```text
   WARNING: run_mode is set to Test. Durability and performance settings
   are optimized for testing, not production. If this is unintentional,
   check your node configuration.
   ```

Neither warning sleeps. The original 5-second sleep was a stopgap for the debug-assertions-as-test-mode conflation; with explicit `RunMode`, misconfiguration is the operator's responsibility.

### 5. `testing_with_signer()` Constructor Updates

The `testing_with_signer()` constructor uses struct literal syntax (not `..Default::default()`), so the compiler will enforce that all new fields are set. The constructor must be updated to include:

```rust
Self {
    run_mode: RunMode::Test,
    database: DatabaseConfig {
        sync_mode: DbSyncMode::UtterlyNoSync,
        cache_sync_mode: DbSyncMode::UtterlyNoSync,
    },
    // ...existing fields...
    reth: RethConfig {
        network: RethNetworkConfig { ... },
        db_sync_mode: DbSyncMode::UtterlyNoSync,
        cross_block_cache_size_megabytes: Some(10),
        additional_validation_tasks: 0,
    },
    vdf: VdfNodeConfig {
        parallel_verification_thread_limit: 4,
        core_pinning: CorePinning::Disabled,
    },
    // ...
}
```

The `testnet()` constructor similarly needs the new fields, but with production defaults (or testnet-specific values as appropriate).

### 6. `open_or_create_db` Signature Change and Blast Radius

The `open_or_create_db` function accepts caller-built `DatabaseArguments` directly, with no separate `sync_mode` parameter. Callers use `DatabaseArguments::irys_default(sync_mode)?` (or `irys_cache`, `irys_testing`) to build sync-mode-aware args before passing them in:

```rust
pub fn open_or_create_db<P: AsRef<Path>, T: TableSet + TableInfo>(
    path: P,
    tables: &[T],
    args: DatabaseArguments,
) -> eyre::Result<DatabaseEnv> {
    let db = init_db_for::<P, T>(path, args)?.with_metrics_and_tables(tables);
    Ok(db)
}

// Most callers build args via the IrysDatabaseArgs trait:
open_or_create_db(path, tables, DatabaseArguments::irys_default(sync_mode)?);
```

Each caller is responsible for building `DatabaseArguments` with the appropriate `DbSyncMode`. The `DatabaseArguments::irys_default` constructor embeds the sync mode alongside standard settings (unbounded read transaction duration, 10 MB growth step, 20 MB shrink threshold). Specialized callers like `open_or_create_cache_db` and `create_or_open_submodule_db` use their own constructors (`irys_cache`, `irys_default` with custom geometry) to tailor `DatabaseArguments` while still honoring `DbSyncMode`.

**Direct callers of `open_or_create_db` using `DatabaseArguments::irys_default` or `irys_testing`** (~22 call sites):

| Location | Count | Notes |
|----------|-------|-------|
| `crates/database/src/database.rs` (tests) | ~4 | Pass `DbSyncMode::UtterlyNoSync` |
| `crates/database/src/db_index.rs` (tests) | ~4 | Pass `DbSyncMode::UtterlyNoSync` |
| `crates/database/src/submodule/tables.rs` (tests) | 1 | Pass `DbSyncMode::UtterlyNoSync` |
| `crates/database/src/submodule/db.rs` (tests) | 1 | Pass `DbSyncMode::UtterlyNoSync` |
| `crates/database/src/migration.rs` (tests) | ~4 | Pass `DbSyncMode::UtterlyNoSync` |
| `crates/database/examples/dupsort.rs` | 1 | Pass `DbSyncMode::UtterlyNoSync` |
| `crates/domain/tests/` | 2 | Pass `DbSyncMode::UtterlyNoSync` |
| `crates/domain/src/models/block_index.rs` (tests) | 1 | Pass `DbSyncMode::UtterlyNoSync` |
| `crates/actors/tests/block_validation_tests.rs` | 1 | Pass `DbSyncMode::UtterlyNoSync` |
| `crates/actors/src/anchor_validation_tests.rs` | 1 | Pass `DbSyncMode::UtterlyNoSync` |
| `crates/actors/src/cache_service.rs` (tests) | ~8 | Pass `DbSyncMode::UtterlyNoSync` |
| `crates/utils/debug-utils/src/db.rs` | 1 | Pass `DbSyncMode::Durable` (operates on real DBs) |

**Direct callers of `open_or_create_db`** — each caller builds its own `DatabaseArguments` embedding the desired `DbSyncMode`, which `open_or_create_db` passes through to MDBX:
- `open_or_create_cache_db` — builds `DatabaseArguments::irys_cache(sync_mode)` with the caller-provided `DbSyncMode` baked into the `DatabaseArguments`. The `sync_mode` is honored for cache DBs through these args.
- `create_or_open_submodule_db` in `crates/database/src/submodule/db.rs` — builds `DatabaseArguments::irys_default(sync_mode)` with custom geometry (max size 2 TB). The `sync_mode` parameter is now honored for submodule DBs through these sync-mode-aware `DatabaseArguments`, rather than falling back to MDBX library defaults.

**Callers of `open_or_create_irys_consensus_data_db`** (the wrapper, which also gains a `DbSyncMode` param):

| Location | Notes |
|----------|-------|
| `crates/chain/src/chain.rs` (`init_irys_db`) | Production path — pass `node_config.database.sync_mode` |
| `crates/actors/tests/epoch_snapshot_tests.rs` | Pass `DbSyncMode::UtterlyNoSync` |
| `crates/actors/src/block_validation.rs` (tests) | Pass `DbSyncMode::UtterlyNoSync` |
| `crates/p2p/src/server.rs` (tests) | Pass `DbSyncMode::UtterlyNoSync` |
| `crates/p2p/src/chain_sync.rs` (tests, ~4 sites) | Pass `DbSyncMode::UtterlyNoSync` |
| `crates/p2p/src/peer_network_service.rs` (tests) | Pass `DbSyncMode::UtterlyNoSync` |
| `crates/p2p/src/tests/util.rs` | Pass `DbSyncMode::UtterlyNoSync` |
| `crates/p2p/src/tests/block_pool/mod.rs` | Pass `DbSyncMode::UtterlyNoSync` |

All test callers mechanically pass `DbSyncMode::UtterlyNoSync`. The compiler will catch any missed call sites since the function signatures change.

## Files Changed

| File | Change | Scope |
|------|--------|-------|
| `crates/types/src/config/node.rs` | Add `RunMode` enum, `DbSyncMode` enum, `DatabaseConfig` struct, `run_mode` + `database` fields on `NodeConfig`. Update `testing_with_signer()` and `testnet()` constructors. Add `.with_run_mode()`. | ~50 lines |
| `crates/types/src/config/node.rs` (`RethConfig`) | Add `db_sync_mode`, `cross_block_cache_size_megabytes`, `additional_validation_tasks` fields with defaults. | ~15 lines |
| `crates/types/src/config/node.rs` (`VdfNodeConfig`) | Add `CorePinning` enum and `core_pinning` field. | ~20 lines |
| `crates/database/src/database.rs` | Add `sync_mode: DbSyncMode` param to `open_or_create_db` and `open_or_create_cache_db`. Add `db_sync_mode_to_mdbx` helper. Remove `cfg!(debug_assertions)`. Sync mode applied only when `args` is `None`. | ~15 lines |
| `crates/database/src/submodule/db.rs` | Update `create_or_open_submodule_db` to accept and pass `DbSyncMode`. | ~3 lines |
| `crates/storage/src/irys_consensus_data_db.rs` | Add `DbSyncMode` param, pass through to `open_or_create_db`. | ~3 lines |
| `crates/chain/src/chain.rs` | Pass `node_config.database.sync_mode` through `init_irys_db`. Replace VDF dual-check with `config.vdf.core_pinning`. Remove `.tmp` heuristic and panic. | ~20 lines |
| `crates/reth-node-bridge/src/node.rs` | Read `node_config.reth.db_sync_mode`, `.cross_block_cache_size_megabytes`, `.additional_validation_tasks` instead of `cfg!(debug_assertions)`. | ~10 lines |
| `crates/chain/src/main.rs` | Keep debug build warning (reword), add `run_mode` warning after config load. | ~10 lines |
| `crates/database/` test files (~14 sites) | Add `DbSyncMode::UtterlyNoSync` argument to `open_or_create_db` calls. | ~1 line each |
| `crates/domain/` tests + `block_index.rs` (~3 sites) | Add `DbSyncMode::UtterlyNoSync` argument. | ~1 line each |
| `crates/actors/` tests (~10 sites) | Add `DbSyncMode::UtterlyNoSync` to `open_or_create_db` and `open_or_create_irys_consensus_data_db` calls. | ~1 line each |
| `crates/p2p/` tests (~8 sites) | Add `DbSyncMode::UtterlyNoSync` to `open_or_create_irys_consensus_data_db` calls. | ~1 line each |
| `crates/utils/debug-utils/src/db.rs` | Add `DbSyncMode::Durable` argument (operates on real databases). | ~1 line |

## What Is NOT Changed

- `#[cfg(any(test, feature = "test-utils"))]` attributes on test builder methods — these gate code availability at compile time, a different concern.
- `open_or_create_cache_db` has zero production callers currently. It is updated for consistency but may be a candidate for removal in a separate cleanup.
- No new crate dependencies. `RunMode` and `DbSyncMode` live in `irys-types`.
- Existing config files continue to work — all new fields have `#[serde(default)]` and `NodeConfig` uses `#[serde(deny_unknown_fields)]`, so only missing fields (not extra fields) are an issue, and defaults cover that.
