# Cache DB Split — Design Spec

**Status:** Proposed
**Date:** 2026-05-11
**Branch:** `feat/cache-db`

## Problem

The Irys consensus database (`<base>/irys_consensus_data/`) is a single
MDBX environment holding both consensus-critical tables (block headers,
tx headers, block index, ledger metadata, peer list) and reconstructable
cache tables (cached data roots, cached chunks, cached chunk index,
ingress proofs).

MDBX serializes writers per environment. Cache-pruning workloads in
`cache_service` (large multi-thousand-row batches walking
`CachedChunksIndex` and deleting from `CachedChunks` / `IngressProofs`)
hold the consensus DB's single rw-tx slot for extended periods, blocking
unrelated consensus writes (block migration, mempool tx insertion, block
production metadata persistence).

Recent telemetry branches (`feat(database): add tx_mut acquire histogram
for irys consensus DB`, `feat(database, telemetry): count libmdbx rw-tx
lock stall warnings`, `feat(database): add update_scoped to
scope-attribute consensus DB stalls`) confirm contention is the source
of observed stalls.

## Goal

Move the four cache tables into a **second MDBX environment** so cache
writes no longer contend with consensus writes for the per-env writer
lock. Tighten the database type system so the compiler enforces which
tables live in which environment.

## Non-goals

* Moving `IrysPoAChunks` — keep atomicity with `IrysBlockHeaders`.
* Moving `PeerListItems` — out of scope; could be revisited.
* Compacting the consensus DB after migration (B-tree free pages
  remain; disk does not auto-shrink). Defer.
* Granting cache-only services a `CacheDb`-only view; defer to a
  follow-up PR.

## Decisions captured during brainstorming

1. **Scope.** The new cache DB holds exactly four tables:
   `CachedDataRoots`, `CachedChunksIndex`, `CachedChunks`,
   `IngressProofs`. Justification for moving `IngressProofs`:
   * `cache_service::prune_ingress_proofs` (line 517) actively prunes
     expired / promoted / unanchored proofs.
   * `cache_service::prune_chunks_without_active_ingress_proofs`
     deletes from `IngressProofs` during chunk cache eviction
     (line 449).
   * `tx_selector` reads `IngressProofs` only to find publish
     candidates (pre-promotion).
   * Historic / consensus-critical proofs live inside the published
     block header itself: `crates/types/src/block.rs:687` —
     `pub proofs: Option<IngressProofsList>` on the Publish ledger
     entry. The `IngressProofs` table is a working buffer, not
     historic state.
2. **`IrysPoAChunks` stays in the consensus DB** to preserve the
   atomic `IrysBlockHeaders` + `IrysPoAChunks` write in
   `block_migration_service::persist_block`.
3. **Migration.** Bump `DatabaseVersion::CURRENT` to `V4`. A one-shot
   `V3 → V4` migration copies existing rows from the consensus DB into
   the cache DB, then clears the consensus copies. The migration runs
   in `ensure_db_version_compatible` before any service starts.
4. **API shape.** Type-safe scoped transactions: split `IrysTables`
   into `ConsensusTables` and `CacheTables`; introduce
   `ScopedTx<Scope>` / `ScopedTxMut<Scope>` wrappers parameterized by
   a `Consensus` or `Cache` marker; the compiler refuses
   cross-scope table access.
5. **Sync mode.** Cache DB honors `database.cache_sync_mode` (already
   present in `DatabaseConfig`, defaulting to `SafeNoSync`). Never
   `UtterlyNoSync` — that risks structural corruption, not just lost
   writes.

## Architecture

### Disk layout

```
<base_directory>/
├── irys_consensus_data/       # existing — non-cache tables
└── irys_cache_data/           # new — the 4 cache tables
```

`NodeConfig::irys_cache_data_dir()` is a new helper mirroring
`irys_consensus_data_dir()`.

### Table split

In `crates/database/src/tables.rs`:

```rust
tables! {
    ConsensusTables;
    // IrysBlockHeaders, IrysBlockIndexItems, MigratedBlockHashes,
    // IrysPoAChunks, IrysDataTxHeaders, IrysCommitments,
    // IrysCommitmentTxMetadata, IrysDataTxMetadata,
    // PeerListItems, Metadata
}

tables! {
    CacheTables;
    // CachedDataRoots, CachedChunksIndex, CachedChunks,
    // IngressProofs, Metadata
}
```

`Metadata` exists in both envs — each carries its own
`DBSchemaVersion` stamp. `IrysTables` is retained as a type alias for
`ConsensusTables` during the transition (deleted once all callers
migrate).

### Type-safe scoped transactions

```rust
// Marker traits, emitted alongside each table by a small macro.
pub trait ConsensusTable: Table {}
pub trait CacheTable:     Table {}

// Scope marker enums (zero-sized).
pub enum Consensus {}
pub enum Cache {}

pub trait DbScope: 'static {}
impl DbScope for Consensus {}
impl DbScope for Cache {}

// Scoped transaction wrappers.
pub struct ScopedTx<S: DbScope>(
    <DatabaseEnv as Database>::TX, PhantomData<S>,
);
pub struct ScopedTxMut<S: DbScope>(
    <DatabaseEnv as Database>::TXMut, PhantomData<S>,
);
```

Inherent methods on `ScopedTx<Consensus>` accept only
`T: ConsensusTable`; same for `Cache`. `tx.put::<CachedChunks>(...)`
on a consensus-scoped tx is a **compile error**.

Methods reimplemented (delegating to the inner reth tx):
`get`, `put`, `delete`, `cursor_read`, `cursor_write`,
`cursor_dup_read`, `cursor_dup_write`, `entries`, `clear`.

`commit` lives on the inner tx as today. A `pub fn inner(&self)`
escape hatch exposes the untyped reth tx for code that genuinely
needs cross-scope access (the V3 → V4 migration uses this).

### `DatabaseProvider`

```rust
pub struct DatabaseProvider {
    consensus: Arc<DatabaseEnv>,
    cache:     Arc<DatabaseEnv>,
}

impl DatabaseProvider {
    pub fn view_consensus<F, T>(&self, f: F)   -> ...  // &ScopedTx<Consensus>
    pub fn update_consensus<F, T>(&self, f: F) -> ...  // &ScopedTxMut<Consensus>
    pub fn view_cache<F, T>(&self, f: F)       -> ...  // &ScopedTx<Cache>
    pub fn update_cache<F, T>(&self, f: F)     -> ...  // &ScopedTxMut<Cache>
}
```

Backwards-compat shims: existing `view` / `update` / `view_eyre` /
`update_eyre` route to `view_consensus` / `update_consensus`. Services
that touch only non-cache tables compile unchanged; only cache call
sites switch to the new methods.

No service grows a second `db` field. Construction-side change is
confined to `chain.rs::init_irys_db` and the dozen-ish test setup
sites that today call `open_or_create_irys_consensus_data_db`.

### Construction (`crates/chain/src/chain.rs::init_irys_db`)

```rust
fn init_irys_db(node_config: &NodeConfig) -> eyre::Result<DatabaseProvider> {
    let consensus = open_or_create_db(
        node_config.irys_consensus_data_dir(),
        ConsensusTables::ALL,
        DatabaseArguments::irys_default(node_config.database.sync_mode)?,
    )?;
    let cache = open_or_create_cache_db(
        node_config.irys_cache_data_dir(),
        CacheTables::ALL,
        node_config.database.cache_sync_mode,
    )?;
    irys_database::migration::ensure_db_version_compatible(
        &consensus, &cache,
    )?;
    Ok(DatabaseProvider::new(Arc::new(consensus), Arc::new(cache)))
}
```

`open_or_create_cache_db` already exists; it uses the wider growth /
shrink geometry from `DatabaseArguments::irys_cache`.

### Free-function signature tightening

Free functions in `db_cache.rs` (and cache-touching helpers in
`database.rs`) change their bound from generic `<T: DbTx + DbTxMut>`
to `&ScopedTx<Cache>` / `&ScopedTxMut<Cache>`. Examples:
`cache_chunk`, `cache_chunk_verified`, `cache_data_root`,
`confirm_data_size_for_data_root`, `cached_data_root_by_data_root`,
`cached_chunk_*`, `delete_cached_chunks_by_data_root*`,
`store_ingress_proof*`, `delete_ingress_proof`,
`ingress_proofs_by_data_root`, `ingress_proof_by_data_root_address`.

Functions in `db_index.rs` get tightened to `Consensus`.

Truly scope-agnostic helpers (`walk_all<T: Table, TX: DbTx>`,
`get_cache_size<T: Table, TX: DbTx>`) stay generic.

## Migration: V3 → V4

`DatabaseVersion::V4` is added to the enum in `crates/types/src` and
`CURRENT` advances to it.


```rust
pub fn ensure_db_version_compatible(
    consensus: &DatabaseEnv,
    cache:     &DatabaseEnv,
) -> eyre::Result<()> { ... }
```

Behavior:

| consensus stamp | cache stamp | action                                                            |
|-----------------|-------------|-------------------------------------------------------------------|
| none (fresh)    | none        | stamp both V4. done.                                              |
| V3              | none / V3   | run `v3_to_v4`; stamp both V4.                                    |
| V4              | V4          | no-op.                                                            |
| V4              | none        | accept fresh cache (consensus copies already cleared on prior run); stamp cache V4. Operators may have intentionally deleted the cache DB to recover disk — cache state is reconstructable, so this is recoverable, not an error. |
| > V4            | _           | error — newer binary required.                                    |
| _               | > V4        | error — newer binary required.                                    |

### The `v3_to_v4` step

```rust
fn v3_to_v4(consensus: &DatabaseEnv, cache: &DatabaseEnv) -> eyre::Result<()> {
    for table in [
        CachedDataRoots, CachedChunksIndex, CachedChunks, IngressProofs,
    ] {
        // 1. Copy: walk consensus.table -> write to cache.table.
        //    Skip rows already present in cache (idempotent).
        copy_table(consensus, cache, table)?;
        // 2. Drop: clear the consensus copy.
        consensus.update(|tx| tx.clear::<table>())?;
    }
    consensus.update(|tx| set_database_schema_version(tx, V4))?;
    cache    .update(|tx| set_database_schema_version(tx, V4))?;
    Ok(())
}
```

Batched at ~10k rows per rw-tx per side, matching the pattern used by
the existing `v2_to_v3` migration.

### Crash semantics

* Mid-copy: rerun copies remaining rows; existing rows in cache are
  skipped (key-presence check before `put`).
* Between copy and clear: rerun re-copies (no-op) then clears.
* Between table N and table N+1: rerun resumes at the next still-
  populated consensus table.
* Observers never see torn state — the migration runs before any
  service starts, and reads of any single table go to exactly one of
  the two envs at a time.

### Disk-usage note

During the migration, cache rows exist in both envs. Operators with
large `CachedChunks` tables should expect transient disk doubling
for those rows. The post-migration consensus DB does **not**
auto-shrink (MDBX retains free pages); a follow-up `mdbx_copy` would
compact it, but is out of scope.

## Service call-site changes

### Cache writers (switch to `db.update_cache(...)`)

* `crates/actors/src/cache_service.rs` — heaviest writer, ~15 sites
  in the pruning paths (lines 215, 374, 422, 449, 452, 473, 490, 521,
  533, 1004, 1016, 1091, 1097, 1177, 1188, 1194, 1272, 1283, 1361,
  1375, 1429, 1494, 1614, 1625, 1685, 1691, 1704).
* `crates/actors/src/mempool_service/data_txs.rs:459` —
  `cache_data_root_with_expiry`.
* `crates/actors/src/chunk_ingress_service/chunks.rs:355` —
  `confirm_data_size_for_data_root`.
* `crates/actors/src/chunk_ingress_service/chunks.rs:879` —
  `store_ingress_proof_checked`.
* `crates/actors/src/chunk_ingress_service/ingress_proofs.rs:192,
  502` — `delete_ingress_proof`, `store_ingress_proof`.

### Cache readers (switch to `db.view_cache(...)`)

* `crates/actors/src/tx_selector/mod.rs:777` — walks `IngressProofs`.
* `crates/actors/src/chunk_ingress_service/chunks.rs:115, 517, 546,
  813`.
* `crates/actors/src/chunk_ingress_service/ingress_proofs.rs:562`.
* `crates/api-server/src/routes/block.rs` and chain-tests reading
  `IngressProofs` / `CachedChunks`.

### Cross-table transactions audited

* `mempool_service::cache_data_root_with_expiry` — cache-only.
* `database::store_ingress_proof_checked` — reads `CachedDataRoots`
  and writes `IngressProofs` (both cache).
* `database::cache_chunk` — reads `CachedDataRoots` and writes
  `CachedChunks` / `CachedChunksIndex` (both cache).
* `database::confirm_data_size_for_data_root` — `CachedDataRoots`
  only.

No transaction today mixes cache and non-cache tables that are also
expected to be atomic. The split does not break any existing
cross-table atomicity invariant.

### Other DB-open sites

The dozen-ish call sites in test harnesses, CLI, and p2p tests that
today call `open_or_create_irys_consensus_data_db` switch to a
`DatabaseProvider::for_testing(consensus_path, cache_path)` helper,
which opens both envs and runs the migration. Cleanup of
`open_or_create_irys_consensus_data_db` callers is mechanical.

## Telemetry

Existing metrics (`tx_mut_acquire` histogram, libmdbx rw-tx
lock-stall counter, `update_scoped` attribution) gain a `db` label
with values `"consensus"` and `"cache"`. The metrics recorder
attaches to both `DatabaseEnv` instances. The success criterion for
this PR is a measurable drop in consensus-DB rw-tx stall counts
under representative workload.

## Testing

* **Marker-trait sanity**: `assert_impl_all!(CachedChunks:
  CacheTable)`, `assert_not_impl_any!(CachedChunks: ConsensusTable)`,
  and symmetrical for each table. One sanity test per scope is
  sufficient — the marker macro is mechanical.
* **Compile-time gating**: a `compile_fail` doctest confirming that
  `tx.put::<CachedChunks>(...)` on a `ScopedTxMut<Consensus>` fails
  to compile.
* **Migration tests** in `crates/database/src/migration.rs`:
  * Fresh install (no consensus, no cache) → both stamped V4.
  * V3 → V4 with cache tables populated in the consensus DB → after
    migration: rows present in cache DB, cache tables empty in
    consensus DB, both stamped V4.
  * Idempotency: run migration twice → identical end state.
  * Crash-resumability: kill after partial table copy → rerun
    completes cleanly.
  * V4 already-stamped → no-op.
* **Integration**:
  `crates/chain-tests/src/integration/cache_service.rs` exercises the
  prune path; verify it passes against the split provider. Add one
  test asserting the on-disk paths exist independently.
* **Regression**: full `cargo xtask test`; focus on `cache_service`,
  `chunk_ingress_service`, `mempool_service`, `tx_selector`, and any
  test opening a temporary DB.

## Open items / follow-ups (out of scope)

* Compacting the consensus DB post-migration via `mdbx_copy`.
* Reorganizing `database.rs` into separate `consensus_db.rs` /
  `cache_db.rs` modules.
* Giving cache-only services a `CacheDb`-only view (forbid them from
  accidentally opening consensus txns).
* Revisit `PeerListItems` placement once the split is bedded in.
