# Cache DB Split Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split the 4 cache tables (`CachedDataRoots`, `CachedChunksIndex`, `CachedChunks`, `IngressProofs`) out of the consensus MDBX environment into a separate cache MDBX environment, eliminating per-env writer-lock contention between cache pruning and consensus writes.

**Architecture:** Two `DatabaseEnv` instances live behind a single `DatabaseProvider`. Compile-time scoped transactions (`ScopedTx<Consensus>` vs `ScopedTx<Cache>`) enforce which tables can be touched in which env. A one-shot `V3 → V4` migration copies existing cache rows from the consensus DB to the cache DB.

**Tech Stack:** Rust 1.93, MDBX via `reth-db` (irys-xyz fork), `tables!` macro from `reth-db-api`, `tokio` for runtime, `nextest` for test runs.

**Spec:** `docs/superpowers/specs/2026-05-11-cache-db-split-design.md`

**Working directory:** `/workspaces/irys-rs/.worktrees/cache-db` (branch `feat/cache-db`).

---

## Background: read these files first

Skim before starting — they're the load-bearing context:

- `docs/superpowers/specs/2026-05-11-cache-db-split-design.md` — full design.
- `crates/database/src/tables.rs` — current `tables! { IrysTables; ... }` invocation; the macro is in `/workspaces/reth-irys/crates/storage/db-api/src/tables/mod.rs` (note: that path is outside the worktree but the source is also at the cargo git checkout fed by `Cargo.toml`'s `reth-db` git rev).
- `crates/database/src/database.rs` — `IrysDatabaseArgs`, `open_or_create_db`, `open_or_create_cache_db`, cache free functions (`cache_chunk`, `cache_data_root`, `store_ingress_proof_checked`, etc.).
- `crates/database/src/db.rs` — `RethDbWrapper` and `IrysDatabaseExt`.
- `crates/database/src/migration.rs` — existing `ensure_db_version_compatible` and `v2_to_v3`, the model for `v3_to_v4`.
- `crates/types/src/versions.rs` — `DatabaseVersion` enum.
- `crates/types/src/app_state.rs` — `DatabaseProvider` definition (currently `pub struct DatabaseProvider(pub Arc<DatabaseEnv>)` with a `Deref` to `Arc<DatabaseEnv>`).
- `crates/types/src/config/node.rs` — `DatabaseConfig`, `cache_sync_mode`, `irys_consensus_data_dir`.
- `crates/storage/src/irys_consensus_data_db.rs` — `open_or_create_irys_consensus_data_db`.
- `crates/chain/src/chain.rs:2440` — `init_irys_db`.
- `crates/actors/src/cache_service.rs` — heaviest cache-write call site; pruning paths.
- `crates/actors/src/chunk_ingress_service/{chunks.rs,ingress_proofs.rs}` — cache writers/readers.
- `crates/actors/src/mempool_service/data_txs.rs:459` — `cache_data_root_with_expiry`.
- `crates/actors/src/tx_selector/mod.rs:760-810` — walks `IngressProofs` for publish candidates.

## Conventions

- Never push to remote without explicit user confirmation.
- Run `cargo xtask check` between tasks (~30s) to keep yourself honest about compile breakage. Run `cargo nextest run -p irys-database` after each crate-local change. Run `cargo xtask test` (full suite) at the verification step.
- Never add "Co-Authored-By" lines to commit messages.
- Preserve existing comments explaining *why* (project rule from `CLAUDE.md`).
- Don't refactor adjacent code beyond what the task requires.
- Frequent commits — one task = one commit (or two if a TDD pair).

---

## File structure

### Files to create

| Path | Responsibility |
|------|----------------|
| `crates/database/src/scoped_tx.rs` | `DbScope` trait, `Consensus`/`Cache` marker enums, `ScopedTx<S>` / `ScopedTxMut<S>` wrappers, `ConsensusTable` / `CacheTable` marker traits, inherent typed methods. |

### Files to modify

| Path | Changes |
|------|---------|
| `crates/types/src/versions.rs` | Add `V4`; bump `CURRENT`; extend `from_u32`. |
| `crates/types/src/config/node.rs` | Add `irys_cache_data_dir()` helper. |
| `crates/types/src/app_state.rs` | `DatabaseProvider` becomes `{ consensus, cache }`; keep `Deref` to consensus env for backwards compat; add accessors. |
| `crates/database/src/tables.rs` | Split single `tables!` invocation into `ConsensusTables` and `CacheTables`; add `CacheMetadata` table; emit marker-trait impls. |
| `crates/database/src/lib.rs` | Export `scoped_tx` module and new types. |
| `crates/database/src/db.rs` | `RethDbWrapper` is unchanged; add helper trait method that returns the typed env. |
| `crates/database/src/database.rs` | Tighten cache free-fn signatures from `<T: DbTx + DbTxMut>` to `&ScopedTx<Cache>` / `&ScopedTxMut<Cache>`. `DatabaseProvider` gains `view_cache` / `update_cache` (inherent methods, added here or in `db.rs`). |
| `crates/database/src/db_cache.rs` | (Currently only holds types; if any free fns are added later they'll go here scoped to Cache.) |
| `crates/database/src/db_index.rs` | Cache helpers (none here today) → no change unless we relocate. Tighten consensus fns to `Consensus` if low-risk. |
| `crates/database/src/migration.rs` | `ensure_db_version_compatible(&consensus, &cache)` two-env signature; add `v3_to_v4` step with idempotent table copy + clear. |
| `crates/storage/src/irys_consensus_data_db.rs` | Add `open_or_create_irys_cache_data_db(path, sync_mode)`. |
| `crates/chain/src/chain.rs` | `init_irys_db` opens both envs, runs two-env migration, constructs `DatabaseProvider`. |
| `crates/actors/src/cache_service.rs` | Switch ~25 `db.update`/`db.view`/`db.update_eyre`/`db.view_eyre` cache-table sites to `db.update_cache`/`db.view_cache`. |
| `crates/actors/src/mempool_service/data_txs.rs` | One site (line 459). |
| `crates/actors/src/chunk_ingress_service/chunks.rs` | Five sites. |
| `crates/actors/src/chunk_ingress_service/ingress_proofs.rs` | Three sites + helper functions. |
| `crates/actors/src/tx_selector/mod.rs` | One site (line 777). |
| `crates/api-server/src/routes/block.rs` and `crates/api-server/src/lib.rs` | Any reads of `IngressProofs` / `CachedChunks`. |
| `crates/chain-tests/src/{integration,external,multi_node,validation,promotion}/*.rs` | Test helpers reading cache tables — switch to `view_cache`. |
| `crates/p2p/src/{peer_network_service.rs,chain_sync.rs,server.rs,tests/util.rs,tests/block_pool/mod.rs}` | Test setups currently calling `open_or_create_irys_consensus_data_db` — switch to a `DatabaseProvider::for_testing()` helper. |
| `crates/utils/debug-utils/src/db.rs` | Reads `IngressProofs` — switch to `view_cache`. |

---

## Task 0: Workspace sanity

### Files
- Read-only: confirm working tree.

- [ ] **Step 1: Confirm we're on the right branch in a clean tree**

Run: `git status && git rev-parse --abbrev-ref HEAD`

Expected:
```
On branch feat/cache-db
nothing to commit, working tree clean
feat/cache-db
```

- [ ] **Step 2: Confirm the spec is present**

Run: `ls docs/superpowers/specs/2026-05-11-cache-db-split-design.md`

Expected: file exists.

- [ ] **Step 3: Establish a green baseline**

Run: `cargo xtask check` (allow up to 5 min on first build)

Expected: exit 0, no errors. If errors exist on master — STOP and report; we need a green baseline before making changes.

---

## Task 1: Bump DatabaseVersion to V4

### Files
- Modify: `crates/types/src/versions.rs`

- [ ] **Step 1: Add the `V4` variant and bump `CURRENT`**

Edit `crates/types/src/versions.rs`:

Replace the `pub enum DatabaseVersion { ... V3 = 3, }` block (lines 19–64) — append the new variant inside the enum:

```rust
    /// Cache DB split migration.
    ///
    /// Splits the four cache tables (`CachedDataRoots`, `CachedChunksIndex`,
    /// `CachedChunks`, `IngressProofs`) out of the single MDBX environment at
    /// `<base>/irys_consensus_data/` into a sibling environment at
    /// `<base>/irys_cache_data/`. The cache environment carries its own
    /// `CacheMetadata` table with an independent `DBSchemaVersion` stamp.
    ///
    /// The migration is idempotent and resumable: each cache table is copied
    /// row-by-row into the cache env (skipping rows already present), then the
    /// consensus copy is cleared. A crash mid-migration replays cleanly on
    /// next startup.
    V4 = 4,
```

Then update `CURRENT` and `from_u32`:

```rust
impl DatabaseVersion {
    /// The current schema version that this binary expects.
    pub const CURRENT: Self = Self::V4;

    /// Returns `Some(version)` if the raw value corresponds to a known variant,
    /// or `None` if the value was written by a newer binary.
    pub fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::V0),
            1 => Some(Self::V1),
            2 => Some(Self::V2),
            3 => Some(Self::V3),
            4 => Some(Self::V4),
            _ => None,
        }
    }
}
```

- [ ] **Step 2: Compile-check**

Run: `cargo check -p irys-types`

Expected: succeeds. (Subsequent compile errors elsewhere — e.g., the `match DatabaseVersion::CURRENT => ...` in `migration.rs` not handling V4 — are expected; we'll fix in later tasks. **Do not** fix them yet.)

- [ ] **Step 3: Commit**

```bash
git add crates/types/src/versions.rs
git commit -m "feat(types): add DatabaseVersion::V4 for cache DB split"
```

---

## Task 2: Add `irys_cache_data_dir()` config helper

### Files
- Modify: `crates/types/src/config/node.rs`

- [ ] **Step 1: Add the helper next to `irys_consensus_data_dir`**

Find the existing `irys_consensus_data_dir` definition (around line 1280):

```rust
pub fn irys_consensus_data_dir(&self) -> PathBuf {
    self.base_directory.join("irys_consensus_data")
}
```

Add immediately after it:

```rust
/// Get the irys cache DB directory path
pub fn irys_cache_data_dir(&self) -> PathBuf {
    self.base_directory.join("irys_cache_data")
}
```

- [ ] **Step 2: Compile-check**

Run: `cargo check -p irys-types`

Expected: succeeds.

- [ ] **Step 3: Commit**

```bash
git add crates/types/src/config/node.rs
git commit -m "feat(types): add NodeConfig::irys_cache_data_dir helper"
```

---

## Task 3: Add `scoped_tx` module — scope marker types and traits

### Files
- Create: `crates/database/src/scoped_tx.rs`
- Modify: `crates/database/src/lib.rs`

- [ ] **Step 1: Create `scoped_tx.rs` with marker traits and scope types**

Write `crates/database/src/scoped_tx.rs`:

```rust
//! Type-safe scoped database transactions.
//!
//! The consensus and cache databases are separate MDBX environments. This
//! module defines marker types and wrapper transactions that prevent code
//! from accidentally touching a table from the wrong environment.
//!
//! Each table in `tables.rs` implements exactly one of [`ConsensusTable`] or
//! [`CacheTable`] (with the exception of metadata stamps, where each env has
//! its own table). A `ScopedTx<Consensus>` only exposes operations on
//! `ConsensusTable` impls; `ScopedTx<Cache>` only on `CacheTable` impls. A
//! cross-scope call is a compile error.

use std::marker::PhantomData;

use reth_db::table::{DupSort, Table};
use reth_db::{
    DatabaseEnv,
    cursor::{
        DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW,
    },
    transaction::{DbTx, DbTxMut},
    Database, DatabaseError,
};

/// Marker trait: this table lives in the consensus MDBX env.
pub trait ConsensusTable: Table {}

/// Marker trait: this table lives in the cache MDBX env.
pub trait CacheTable: Table {}

/// Zero-sized scope tag for the consensus env.
pub enum Consensus {}

/// Zero-sized scope tag for the cache env.
pub enum Cache {}

/// Sealed trait: a database scope.
pub trait DbScope: 'static + Send + Sync {}
impl DbScope for Consensus {}
impl DbScope for Cache {}

/// Read-only transaction scoped to a single DB environment.
pub struct ScopedTx<S: DbScope> {
    inner: <DatabaseEnv as Database>::TX,
    _scope: PhantomData<fn() -> S>,
}

/// Read-write transaction scoped to a single DB environment.
pub struct ScopedTxMut<S: DbScope> {
    inner: <DatabaseEnv as Database>::TXMut,
    _scope: PhantomData<fn() -> S>,
}

impl<S: DbScope> ScopedTx<S> {
    pub(crate) fn new(inner: <DatabaseEnv as Database>::TX) -> Self {
        Self { inner, _scope: PhantomData }
    }

    /// Escape hatch: the underlying untyped reth transaction.
    ///
    /// Use only when crossing scope boundaries is unavoidable (e.g. the
    /// V3→V4 migration must touch both consensus and cache tables in the
    /// same code path). Prefer the scoped methods otherwise.
    pub fn inner(&self) -> &<DatabaseEnv as Database>::TX {
        &self.inner
    }

    /// Commit a read transaction (releases reader slot).
    pub fn commit(self) -> Result<bool, DatabaseError> {
        self.inner.commit()
    }
}

impl<S: DbScope> ScopedTxMut<S> {
    pub(crate) fn new(inner: <DatabaseEnv as Database>::TXMut) -> Self {
        Self { inner, _scope: PhantomData }
    }

    pub fn inner(&self) -> &<DatabaseEnv as Database>::TXMut {
        &self.inner
    }

    pub fn commit(self) -> Result<bool, DatabaseError> {
        self.inner.commit()
    }
}

// ----- Consensus-scoped operations -----
impl ScopedTx<Consensus> {
    pub fn get<T: Table + ConsensusTable>(
        &self,
        key: T::Key,
    ) -> Result<Option<T::Value>, DatabaseError> {
        self.inner.get::<T>(key)
    }

    pub fn entries<T: Table + ConsensusTable>(&self) -> Result<usize, DatabaseError> {
        self.inner.entries::<T>()
    }

    pub fn cursor_read<T: Table + ConsensusTable>(
        &self,
    ) -> Result<<<DatabaseEnv as Database>::TX as DbTx>::Cursor<T>, DatabaseError> {
        self.inner.cursor_read::<T>()
    }

    pub fn cursor_dup_read<T: DupSort + ConsensusTable>(
        &self,
    ) -> Result<<<DatabaseEnv as Database>::TX as DbTx>::DupCursor<T>, DatabaseError> {
        self.inner.cursor_dup_read::<T>()
    }
}

impl ScopedTxMut<Consensus> {
    pub fn get<T: Table + ConsensusTable>(
        &self,
        key: T::Key,
    ) -> Result<Option<T::Value>, DatabaseError> {
        self.inner.get::<T>(key)
    }

    pub fn put<T: Table + ConsensusTable>(
        &self,
        key: T::Key,
        value: T::Value,
    ) -> Result<(), DatabaseError> {
        self.inner.put::<T>(key, value)
    }

    pub fn delete<T: Table + ConsensusTable>(
        &self,
        key: T::Key,
        value: Option<T::Value>,
    ) -> Result<bool, DatabaseError> {
        self.inner.delete::<T>(key, value)
    }

    pub fn entries<T: Table + ConsensusTable>(&self) -> Result<usize, DatabaseError> {
        self.inner.entries::<T>()
    }

    pub fn clear<T: Table + ConsensusTable>(&self) -> Result<(), DatabaseError> {
        self.inner.clear::<T>()
    }

    pub fn cursor_read<T: Table + ConsensusTable>(
        &self,
    ) -> Result<<<DatabaseEnv as Database>::TXMut as DbTx>::Cursor<T>, DatabaseError> {
        self.inner.cursor_read::<T>()
    }

    pub fn cursor_write<T: Table + ConsensusTable>(
        &self,
    ) -> Result<<<DatabaseEnv as Database>::TXMut as DbTxMut>::CursorMut<T>, DatabaseError> {
        self.inner.cursor_write::<T>()
    }

    pub fn cursor_dup_read<T: DupSort + ConsensusTable>(
        &self,
    ) -> Result<<<DatabaseEnv as Database>::TXMut as DbTx>::DupCursor<T>, DatabaseError> {
        self.inner.cursor_dup_read::<T>()
    }

    pub fn cursor_dup_write<T: DupSort + ConsensusTable>(
        &self,
    ) -> Result<<<DatabaseEnv as Database>::TXMut as DbTxMut>::DupCursorMut<T>, DatabaseError> {
        self.inner.cursor_dup_write::<T>()
    }
}

// ----- Cache-scoped operations (same shape, CacheTable bound) -----
impl ScopedTx<Cache> {
    pub fn get<T: Table + CacheTable>(
        &self,
        key: T::Key,
    ) -> Result<Option<T::Value>, DatabaseError> {
        self.inner.get::<T>(key)
    }

    pub fn entries<T: Table + CacheTable>(&self) -> Result<usize, DatabaseError> {
        self.inner.entries::<T>()
    }

    pub fn cursor_read<T: Table + CacheTable>(
        &self,
    ) -> Result<<<DatabaseEnv as Database>::TX as DbTx>::Cursor<T>, DatabaseError> {
        self.inner.cursor_read::<T>()
    }

    pub fn cursor_dup_read<T: DupSort + CacheTable>(
        &self,
    ) -> Result<<<DatabaseEnv as Database>::TX as DbTx>::DupCursor<T>, DatabaseError> {
        self.inner.cursor_dup_read::<T>()
    }
}

impl ScopedTxMut<Cache> {
    pub fn get<T: Table + CacheTable>(
        &self,
        key: T::Key,
    ) -> Result<Option<T::Value>, DatabaseError> {
        self.inner.get::<T>(key)
    }

    pub fn put<T: Table + CacheTable>(
        &self,
        key: T::Key,
        value: T::Value,
    ) -> Result<(), DatabaseError> {
        self.inner.put::<T>(key, value)
    }

    pub fn delete<T: Table + CacheTable>(
        &self,
        key: T::Key,
        value: Option<T::Value>,
    ) -> Result<bool, DatabaseError> {
        self.inner.delete::<T>(key, value)
    }

    pub fn entries<T: Table + CacheTable>(&self) -> Result<usize, DatabaseError> {
        self.inner.entries::<T>()
    }

    pub fn clear<T: Table + CacheTable>(&self) -> Result<(), DatabaseError> {
        self.inner.clear::<T>()
    }

    pub fn cursor_read<T: Table + CacheTable>(
        &self,
    ) -> Result<<<DatabaseEnv as Database>::TXMut as DbTx>::Cursor<T>, DatabaseError> {
        self.inner.cursor_read::<T>()
    }

    pub fn cursor_write<T: Table + CacheTable>(
        &self,
    ) -> Result<<<DatabaseEnv as Database>::TXMut as DbTxMut>::CursorMut<T>, DatabaseError> {
        self.inner.cursor_write::<T>()
    }

    pub fn cursor_dup_read<T: DupSort + CacheTable>(
        &self,
    ) -> Result<<<DatabaseEnv as Database>::TXMut as DbTx>::DupCursor<T>, DatabaseError> {
        self.inner.cursor_dup_read::<T>()
    }

    pub fn cursor_dup_write<T: DupSort + CacheTable>(
        &self,
    ) -> Result<<<DatabaseEnv as Database>::TXMut as DbTxMut>::DupCursorMut<T>, DatabaseError> {
        self.inner.cursor_dup_write::<T>()
    }
}
```

- [ ] **Step 2: Export `scoped_tx` from `lib.rs`**

Edit `crates/database/src/lib.rs` — find the existing `pub mod` declarations and add:

```rust
pub mod scoped_tx;
```

After the existing `pub use` lines, add:

```rust
pub use scoped_tx::{Cache, CacheTable, Consensus, ConsensusTable, DbScope, ScopedTx, ScopedTxMut};
```

- [ ] **Step 3: Compile-check the new module**

Run: `cargo check -p irys-database`

Expected: succeeds. If `DbCursorRO`/`DbCursorRW`/`DbDupCursorRO`/`DbDupCursorRW` imports are flagged as unused, delete them (they are exported in case downstream call sites need to import — but if the compiler complains, drop them).

If a particular method signature doesn't compile because the underlying `Cursor` associated type isn't in scope, the issue is path-syntax for the cursor type; switch to `impl ...` returns:

```rust
pub fn cursor_read<T: Table + ConsensusTable>(
    &self,
) -> Result<impl DbCursorRO<T>, DatabaseError> {
    self.inner.cursor_read::<T>()
}
```

Repeat the `impl Trait` substitution for the other cursor methods if needed.

- [ ] **Step 4: Commit**

```bash
git add crates/database/src/scoped_tx.rs crates/database/src/lib.rs
git commit -m "feat(database): add scoped transaction wrappers"
```

---

## Task 4: Split `IrysTables` into `ConsensusTables` and `CacheTables`

### Files
- Modify: `crates/database/src/tables.rs`

This task is delicate. The `tables!` macro generates a *struct type* per `table` and a *single enum* per invocation; the struct's on-disk name is its identifier. We can't have two `tables!` invocations both declare `table Metadata { ... }` — they'd collide on struct definition.

Plan:
1. Rename the cache-env metadata table to `CacheMetadata` (a new struct with on-disk name `CacheMetadata`).
2. Declare two `tables!` blocks: `ConsensusTables` (everything except the four cache tables, keeps `Metadata` as-is) and `CacheTables` (the four cache tables + `CacheMetadata`).
3. Emit marker-trait impls for every table.

- [ ] **Step 1: Replace the single `tables!` invocation with two**

Replace lines 138–235 in `crates/database/src/tables.rs` with:

```rust
tables! {
ConsensusTables;

/// Stores block headers keyed by their hash (canonical chain).
table IrysBlockHeaders {
    type Key = H256;
    type Value = CompactIrysBlockHeader;
}

// Block index table: BlockHeight -> DataLedger -> LedgerIndexItem
// Stores ledger metadata for each block, keyed by height with ledger type as subkey.
// This indexing scheme is optimized for pruning ledgers by height which is
// a function of expiring term ledgers.
table IrysBlockIndexItems {
    type Key = BlockHeight;
    type Value = CompactLedgerIndexItem;
    type SubKey = DataLedger;
}

// Indexes migrated block hashes by height
table MigratedBlockHashes {
    type Key = BlockHeight;
    type Value = H256;
}

/// Stores PoA chunks
table IrysPoAChunks {
    type Key = H256;
    type Value = CompactBase64;
}

/// Stores confirmed transaction headers
table IrysDataTxHeaders {
    type Key = H256;
    type Value = CompactTxHeader;
}

/// Stores commitment transactions
table IrysCommitments {
    type Key = H256;
    type Value = CompactCommitment;
}

/// Stores metadata for commitment transactions
/// Tracks inclusion height
table IrysCommitmentTxMetadata {
    type Key = H256;
    type Value = CompactCommitmentTxMetadata;
}

/// Stores metadata for data transactions
/// Tracks inclusion height and promotion height
table IrysDataTxMetadata {
    type Key = H256;
    type Value = CompactDataTxMetadata;
}

/// Tracks the peer list of known peers as well as their reputation score.
/// While the node maintains connections to a subset of these peers - the
/// ones with high reputation - the PeerListItems contain all the peers
/// that the node is aware of and is periodically updated via peer discovery
table PeerListItems {
    type Key = IrysPeerId;
    type Value = CompactPeerListItem;
}

/// Table to store various metadata, such as the current db schema version
/// (consensus environment).
table Metadata {
    type Key = MetadataKey;
    type Value = Vec<u8>;
}
}

tables! {
CacheTables;

/// Indexes the DataRoots currently in the cache
table CachedDataRoots {
    type Key = DataRoot;
    type Value = CachedDataRoot;
}

/// Index mapping a DataRoot to a set of ordered-by-index index entries, which contain the ChunkPathHash ('chunk id')
table CachedChunksIndex {
    type Key = DataRoot;
    type Value = CachedChunkIndexEntry;
    type SubKey = u32;
}

/// Maps a ChunkPathHash to the cached chunk metadata and optionally its data
table CachedChunks {
    type Key = ChunkPathHash;
    type Value = CachedChunk;
}

/// Indexes ingress proofs by DataRoot and Address
table IngressProofs {
    type Key = DataRoot;
    type Value = CompactCachedIngressProof;
    type SubKey = IrysAddress;
}

/// Schema-version metadata for the cache environment
/// (independent of the consensus `Metadata` table).
table CacheMetadata {
    type Key = MetadataKey;
    type Value = Vec<u8>;
}
}

// Backwards-compat alias so existing callers that name `IrysTables` keep
// compiling. Delete once all callers migrate to `ConsensusTables`.
pub use ConsensusTables as IrysTables;

// Marker-trait impls — wire each table to its environment.
use crate::scoped_tx::{CacheTable, ConsensusTable};

macro_rules! impl_consensus_tables {
    ($($name:ident),* $(,)?) => {
        $(impl ConsensusTable for $name {})*
    };
}

macro_rules! impl_cache_tables {
    ($($name:ident),* $(,)?) => {
        $(impl CacheTable for $name {})*
    };
}

impl_consensus_tables!(
    IrysBlockHeaders,
    IrysBlockIndexItems,
    MigratedBlockHashes,
    IrysPoAChunks,
    IrysDataTxHeaders,
    IrysCommitments,
    IrysCommitmentTxMetadata,
    IrysDataTxMetadata,
    PeerListItems,
    Metadata,
);

impl_cache_tables!(
    CachedDataRoots,
    CachedChunksIndex,
    CachedChunks,
    IngressProofs,
    CacheMetadata,
);
```

- [ ] **Step 2: Compile-check**

Run: `cargo check -p irys-database`

Expected: succeeds. The `pub use ConsensusTables as IrysTables;` line keeps `IrysTables::ALL` callers working (they now see only the non-cache tables — but downstream code that iterates `IrysTables::ALL` doesn't care about cache tables and won't break at this step). If you see "ambiguous import" errors, the existing `pub use database::*` in `lib.rs` may be re-exporting `IrysTables`; resolve by adjusting the alias's visibility.

- [ ] **Step 3: Verify table names on disk are preserved**

Run a quick sanity script (one-off in scratch test or `cargo expand`-style check) to confirm `IrysBlockHeaders::NAME == "IrysBlockHeaders"`, `CachedChunks::NAME == "CachedChunks"`, `Metadata::NAME == "Metadata"`, `CacheMetadata::NAME == "CacheMetadata"`. The `tables!` macro derives `NAME` from the struct ident — this should be automatic, but verify with:

```bash
cargo test -p irys-database --lib --no-run 2>&1 | head -50
```

If you want a hard assertion, add a small test in `tables.rs`:

```rust
#[cfg(test)]
mod tables_names {
    use super::*;
    use reth_db::table::Table;

    #[test]
    fn table_names_match_struct_idents() {
        assert_eq!(IrysBlockHeaders::NAME, "IrysBlockHeaders");
        assert_eq!(Metadata::NAME, "Metadata");
        assert_eq!(CachedChunks::NAME, "CachedChunks");
        assert_eq!(CacheMetadata::NAME, "CacheMetadata");
    }
}
```

Run: `cargo nextest run -p irys-database tables_names`

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/database/src/tables.rs
git commit -m "feat(database): split IrysTables into ConsensusTables and CacheTables"
```

---

## Task 5: Add marker-trait sanity tests

### Files
- Modify: `crates/database/src/tables.rs`

- [ ] **Step 1: Add positive and negative marker-trait tests**

Append to `crates/database/src/tables.rs`:

```rust
#[cfg(test)]
mod marker_trait_sanity {
    use super::*;
    use crate::scoped_tx::{CacheTable, ConsensusTable};
    use static_assertions::{assert_impl_all, assert_not_impl_any};

    // Consensus tables
    assert_impl_all!(IrysBlockHeaders: ConsensusTable);
    assert_not_impl_any!(IrysBlockHeaders: CacheTable);
    assert_impl_all!(Metadata: ConsensusTable);
    assert_not_impl_any!(Metadata: CacheTable);

    // Cache tables
    assert_impl_all!(CachedChunks: CacheTable);
    assert_not_impl_any!(CachedChunks: ConsensusTable);
    assert_impl_all!(IngressProofs: CacheTable);
    assert_not_impl_any!(IngressProofs: ConsensusTable);
    assert_impl_all!(CacheMetadata: CacheTable);
    assert_not_impl_any!(CacheMetadata: ConsensusTable);
}
```

- [ ] **Step 2: Ensure `static_assertions` is in dev-deps**

Check `crates/database/Cargo.toml` for `static_assertions`:

```bash
grep -n "static_assertions" crates/database/Cargo.toml
```

If absent, add to `[dev-dependencies]`:

```toml
static_assertions.workspace = true
```

If it's not in the workspace either, add to workspace `[workspace.dependencies]`:

```toml
static_assertions = "1.1"
```

- [ ] **Step 3: Run tests**

Run: `cargo nextest run -p irys-database marker_trait_sanity`

Expected: PASS (the assertions are compile-time, so a successful compile = a successful test).

- [ ] **Step 4: Commit**

```bash
git add crates/database/src/tables.rs crates/database/Cargo.toml Cargo.toml
git commit -m "test(database): marker-trait sanity assertions for ConsensusTable/CacheTable"
```

(Adjust the `git add` list to whatever files actually changed.)

---

## Task 6: Migration — add `v3_to_v4` step + two-env `ensure_db_version_compatible`

### Files
- Modify: `crates/database/src/migration.rs`
- Test: same file (existing `#[cfg(test)] mod tests`)

- [ ] **Step 1: Add a separate `cache_database_schema_version` helper alongside the existing `database_schema_version`**

In `crates/database/src/database.rs` (where `database_schema_version` / `set_database_schema_version` live; see lines 813–836 of the existing file), add the cache-side equivalents:

```rust
/// Set the schema version in the cache database's `CacheMetadata` table.
pub fn set_cache_database_schema_version<T: DbTxMut>(
    tx: &T,
    version: DatabaseVersion,
) -> Result<(), DatabaseError> {
    tx.put::<crate::tables::CacheMetadata>(
        MetadataKey::DBSchemaVersion,
        (version as u32).to_le_bytes().to_vec(),
    )
}

pub fn cache_database_schema_version<T: DbTx>(
    tx: &mut T,
) -> Result<Option<u32>, DatabaseError> {
    if let Some(bytes) = tx.get::<crate::tables::CacheMetadata>(MetadataKey::DBSchemaVersion)? {
        let arr: [u8; 4] = bytes.as_slice().try_into().map_err(|_| {
            DatabaseError::Other(
                "Cache DB schema version metadata does not have exactly 4 bytes".to_string(),
            )
        })?;
        Ok(Some(u32::from_le_bytes(arr)))
    } else {
        Ok(None)
    }
}
```

Why these are deliberately untyped (`<T: DbTxMut>` not `&ScopedTxMut<Cache>`): the migration crosses both envs and uses the inner reth transactions directly to avoid circular dependencies between `migration.rs` and the scope wrappers during V3→V4 (when the cache env is being populated for the first time).

- [ ] **Step 2: Change `ensure_db_version_compatible` signature to take both envs**

Open `crates/database/src/migration.rs`. Find the existing `pub fn ensure_db_version_compatible(db: &DatabaseEnv)` (around line 606).

Replace its signature and body with the two-env version. Keep all existing `v1_to_v2` and `v2_to_v3` modules intact — those still run against the consensus env only.

New body:

```rust
pub fn ensure_db_version_compatible(
    consensus: &DatabaseEnv,
    cache: &DatabaseEnv,
) -> eyre::Result<()> {
    use reth_db::Database as _;

    // -- consensus side: drive forward through V1/V2/V3 as before --
    let raw_consensus = consensus.view(crate::database_schema_version)??;

    let raw = match raw_consensus {
        Some(v) => v,
        None => {
            debug!("No consensus DB version stamp found — treating as V0, stamping V1.");
            consensus.update_eyre(|tx| {
                crate::set_database_schema_version(tx, DatabaseVersion::V1)?;
                Ok(())
            })?;
            DatabaseVersion::V1 as u32
        }
    };

    let Some(mut version) = DatabaseVersion::from_u32(raw) else {
        eyre::bail!(
            "Consensus database schema version {} is newer than this binary supports \
             (version {}). Use the newer binary or restore from backup.",
            raw,
            DatabaseVersion::CURRENT
        );
    };

    loop {
        match version {
            DatabaseVersion::V0 => {
                debug!("Explicit V0 stamp found — upgrading to V1.");
                consensus.update_eyre(|tx| {
                    crate::set_database_schema_version(tx, DatabaseVersion::V1)?;
                    Ok(())
                })?;
                version = DatabaseVersion::V1;
            }
            DatabaseVersion::V1 => {
                debug!("Applying migration from V1 to V2");
                consensus.update_eyre(|tx| {
                    v1_to_v2::migrate(tx)?;
                    Ok(())
                })?;
                version = DatabaseVersion::V2;
            }
            DatabaseVersion::V2 => {
                debug!("Applying migration from V2 to V3");
                consensus.update_eyre(|tx| {
                    v2_to_v3::migrate(tx)?;
                    Ok(())
                })?;
                version = DatabaseVersion::V3;
            }
            DatabaseVersion::V3 => {
                debug!("Applying migration from V3 to V4: cache DB split");
                v3_to_v4::migrate(consensus, cache)?;
                version = DatabaseVersion::V4;
            }
            DatabaseVersion::CURRENT => {
                debug!(
                    "Consensus database schema is up-to-date (V{})",
                    DatabaseVersion::CURRENT
                );
                break;
            }
        }
    }

    // -- cache side: stamp V4 if missing; reject newer versions --
    let raw_cache = cache.view(crate::cache_database_schema_version)??;
    match raw_cache {
        None => {
            debug!("Stamping fresh cache DB as V{}", DatabaseVersion::CURRENT);
            cache.update_eyre(|tx| {
                crate::set_cache_database_schema_version(tx, DatabaseVersion::CURRENT)?;
                Ok(())
            })?;
        }
        Some(v) if v == DatabaseVersion::CURRENT as u32 => {}
        Some(v) if v < DatabaseVersion::CURRENT as u32 => {
            // The only possible value < CURRENT is V3 written by a partial
            // earlier migration. v3_to_v4 above is idempotent and has just
            // re-run, so stamp CURRENT now.
            debug!("Cache DB at V{}, advancing to V{}", v, DatabaseVersion::CURRENT);
            cache.update_eyre(|tx| {
                crate::set_cache_database_schema_version(tx, DatabaseVersion::CURRENT)?;
                Ok(())
            })?;
        }
        Some(v) => {
            eyre::bail!(
                "Cache database schema version {} is newer than this binary supports \
                 (version {}). Use the newer binary or restore from backup.",
                v,
                DatabaseVersion::CURRENT
            );
        }
    }

    Ok(())
}
```

- [ ] **Step 3: Add the `v3_to_v4` module**

Append to `crates/database/src/migration.rs` (placed near the existing `v1_to_v2` / `v2_to_v3` modules):

```rust
/// V3 → V4: cache DB split.
///
/// Moves the four cache tables out of the consensus env into the cache env.
/// Idempotent and resumable: copies rows that don't yet exist in the cache,
/// then clears the consensus side.
mod v3_to_v4 {
    use super::*;
    use crate::tables::{
        CachedChunks, CachedChunksIndex, CachedDataRoots, IngressProofs,
    };
    use reth_db::cursor::DbCursorRO as _;
    use reth_db::transaction::DbTx as _;
    use reth_db::transaction::DbTxMut as _;
    use reth_db::Database;

    const BATCH_SIZE: usize = 10_000;

    pub(super) fn migrate(
        consensus: &DatabaseEnv,
        cache: &DatabaseEnv,
    ) -> eyre::Result<()> {
        copy_table::<CachedDataRoots>(consensus, cache)?;
        clear_consensus::<CachedDataRoots>(consensus)?;

        copy_dup_table::<CachedChunksIndex>(consensus, cache)?;
        clear_consensus::<CachedChunksIndex>(consensus)?;

        copy_table::<CachedChunks>(consensus, cache)?;
        clear_consensus::<CachedChunks>(consensus)?;

        copy_dup_table::<IngressProofs>(consensus, cache)?;
        clear_consensus::<IngressProofs>(consensus)?;

        // Stamp the cache side V4. The consensus side is stamped by the
        // outer ensure_db_version_compatible loop after this returns.
        cache.update_eyre(|tx| {
            crate::set_cache_database_schema_version(tx, DatabaseVersion::V4)?;
            Ok(())
        })?;
        consensus.update_eyre(|tx| {
            crate::set_database_schema_version(tx, DatabaseVersion::V4)?;
            Ok(())
        })?;
        debug!("V3 → V4 migration complete");
        Ok(())
    }

    /// Copy a non-dupsort table row-by-row from consensus into cache.
    fn copy_table<T>(consensus: &DatabaseEnv, cache: &DatabaseEnv) -> eyre::Result<usize>
    where
        T: reth_db::table::Table + 'static,
        T::Key: Clone,
    {
        let mut total = 0_usize;
        let mut last_key: Option<T::Key> = None;
        loop {
            // Read up to BATCH_SIZE rows starting after last_key.
            let batch: Vec<(T::Key, T::Value)> = consensus.view_eyre(|tx| {
                let mut cursor = tx.cursor_read::<T>()?;
                let mut walker = match last_key.clone() {
                    Some(k) => cursor.walk(Some(k))?,
                    None => cursor.walk(None)?,
                };
                // If resuming, the first row is the resume point itself — skip.
                if last_key.is_some() {
                    let _ = walker.next();
                }
                let mut out = Vec::with_capacity(BATCH_SIZE);
                for _ in 0..BATCH_SIZE {
                    match walker.next() {
                        Some(Ok(row)) => out.push(row),
                        Some(Err(e)) => return Err(e.into()),
                        None => break,
                    }
                }
                Ok(out)
            })?;
            if batch.is_empty() {
                break;
            }
            last_key = batch.last().map(|(k, _)| k.clone());
            let batch_len = batch.len();
            cache.update_eyre(|tx| {
                for (k, v) in batch {
                    // Skip if already present (idempotent on resume).
                    if tx.get::<T>(k.clone())?.is_some() {
                        continue;
                    }
                    tx.put::<T>(k, v)?;
                }
                Ok(())
            })?;
            total += batch_len;
        }
        debug!(table = T::NAME, total, "copied table");
        Ok(total)
    }

    /// Dupsort variant — copy all (key, sub_value) pairs.
    fn copy_dup_table<T>(consensus: &DatabaseEnv, cache: &DatabaseEnv) -> eyre::Result<usize>
    where
        T: reth_db::table::DupSort + 'static,
        T::Key: Clone,
        T::Value: Clone + PartialEq,
    {
        let mut total = 0_usize;
        // For dupsort we read the whole table in chunks; cursor.walk visits all
        // sub-values of each key in order, so the simple paged approach above
        // works.
        let mut last_seen: Option<(T::Key, T::Value)> = None;
        loop {
            let batch: Vec<(T::Key, T::Value)> = consensus.view_eyre(|tx| {
                let mut cursor = tx.cursor_dup_read::<T>()?;
                let mut walker = match last_seen.clone() {
                    Some((k, _)) => cursor.walk(Some(k))?,
                    None => cursor.walk(None)?,
                };
                // Resume: skip rows up to and including last_seen.
                if let Some((lk, lv)) = last_seen.clone() {
                    while let Some(Ok((k, v))) = walker.next() {
                        if k == lk && v == lv {
                            break;
                        }
                    }
                }
                let mut out = Vec::with_capacity(BATCH_SIZE);
                for _ in 0..BATCH_SIZE {
                    match walker.next() {
                        Some(Ok(row)) => out.push(row),
                        Some(Err(e)) => return Err(e.into()),
                        None => break,
                    }
                }
                Ok(out)
            })?;
            if batch.is_empty() {
                break;
            }
            last_seen = batch.last().cloned();
            let batch_len = batch.len();
            cache.update_eyre(|tx| {
                for (k, v) in batch {
                    // Idempotency for dupsort: walk existing sub-values and
                    // skip if (k, v) already present.
                    let mut already = false;
                    {
                        let mut c = tx.cursor_dup_read::<T>()?;
                        if let Some(_) = c.seek_by_key_subkey_exact(k.clone(), &v)? {
                            already = true;
                        }
                    }
                    if !already {
                        tx.put::<T>(k, v)?;
                    }
                }
                Ok(())
            })?;
            total += batch_len;
        }
        debug!(table = T::NAME, total, "copied dupsort table");
        Ok(total)
    }

    fn clear_consensus<T: reth_db::table::Table + 'static>(
        consensus: &DatabaseEnv,
    ) -> eyre::Result<()> {
        consensus.update_eyre(|tx| {
            tx.clear::<T>()?;
            Ok(())
        })?;
        Ok(())
    }
}
```

NOTE on `seek_by_key_subkey_exact`: if reth's `DbDupCursorRO` does not expose that exact method, fall back to the pattern in `cache_service.rs:412-418`:

```rust
let exists = c.seek_by_key_subkey(k.clone(), v.clone().subkey_from_value())?
    .filter(|existing| existing == &v)
    .is_some();
```

Adjust to whichever method shape compiles — the goal is the same idempotency check.

- [ ] **Step 4: Update existing migration tests for the new signature**

In `crates/database/src/migration.rs`, the test module at the bottom currently opens a single DB and calls `ensure_db_version_compatible(&db)`. Update each call site to:

```rust
let cache = open_or_create_db(
    cache_path.path(),
    crate::tables::CacheTables::ALL,
    DatabaseArguments::irys_testing().unwrap(),
)
.unwrap();
ensure_db_version_compatible(&db, &cache).unwrap();
```

Where `cache_path` is a separate `TempDirBuilder::new().build()` per test.

- [ ] **Step 5: Add new migration tests for V3 → V4**

Append in the existing `#[cfg(test)] mod tests` block:

```rust
mod v3_to_v4_tests {
    use super::*;
    use crate::tables::{
        CacheTables, CachedChunks, CachedChunksIndex, CachedDataRoots, ConsensusTables,
        IngressProofs, Metadata,
    };
    use crate::IrysDatabaseArgs as _;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{DatabaseVersion, MetadataKey};
    use reth_db::mdbx::DatabaseArguments;
    use reth_db::transaction::{DbTx as _, DbTxMut as _};
    use reth_db::Database as _;

    fn open_pair() -> (DatabaseEnv, DatabaseEnv, tempfile::TempDir, tempfile::TempDir) {
        let c_dir = TempDirBuilder::new().build();
        let k_dir = TempDirBuilder::new().build();
        let consensus = open_or_create_db(
            c_dir.path(),
            ConsensusTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();
        let cache = open_or_create_db(
            k_dir.path(),
            CacheTables::ALL,
            DatabaseArguments::irys_testing().unwrap(),
        )
        .unwrap();
        (consensus, cache, c_dir, k_dir)
    }

    #[test]
    fn fresh_install_stamps_both_v4() {
        let (consensus, cache, _c, _k) = open_pair();
        ensure_db_version_compatible(&consensus, &cache).unwrap();
        let cv = consensus
            .view(|tx| crate::database_schema_version(&mut { tx }))
            .unwrap()
            .unwrap()
            .unwrap();
        let kv = cache
            .view(|tx| crate::cache_database_schema_version(&mut { tx }))
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(cv, DatabaseVersion::V4 as u32);
        assert_eq!(kv, DatabaseVersion::V4 as u32);
    }

    #[test]
    fn v3_consensus_with_cache_rows_migrates_to_v4() {
        let (consensus, cache, _c, _k) = open_pair();

        // Seed consensus as V3 with cache rows.
        consensus
            .update(|tx| {
                crate::set_database_schema_version(tx, DatabaseVersion::V3)?;
                tx.put::<CachedDataRoots>(
                    irys_types::H256::from([1_u8; 32]),
                    crate::db_cache::CachedDataRoot::default(),
                )?;
                tx.put::<CachedChunks>(
                    irys_types::H256::from([2_u8; 32]),
                    crate::db_cache::CachedChunk::default(),
                )?;
                Ok::<_, reth_db::DatabaseError>(())
            })
            .unwrap()
            .unwrap();

        ensure_db_version_compatible(&consensus, &cache).unwrap();

        // Cache now has the rows.
        let key1 = irys_types::H256::from([1_u8; 32]);
        let got = cache
            .view(|tx| tx.get::<CachedDataRoots>(key1))
            .unwrap()
            .unwrap();
        assert!(got.is_some());

        // Consensus cache copies are cleared.
        let consensus_count = consensus
            .view(|tx| tx.entries::<CachedDataRoots>())
            .unwrap()
            .unwrap();
        assert_eq!(consensus_count, 0);

        // Both stamped V4.
        let cv = consensus
            .view(|tx| crate::database_schema_version(&mut { tx }))
            .unwrap()
            .unwrap()
            .unwrap();
        let kv = cache
            .view(|tx| crate::cache_database_schema_version(&mut { tx }))
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(cv, DatabaseVersion::V4 as u32);
        assert_eq!(kv, DatabaseVersion::V4 as u32);
    }

    #[test]
    fn migration_is_idempotent() {
        let (consensus, cache, _c, _k) = open_pair();

        // Seed and migrate.
        consensus
            .update(|tx| {
                crate::set_database_schema_version(tx, DatabaseVersion::V3)?;
                tx.put::<CachedDataRoots>(
                    irys_types::H256::from([1_u8; 32]),
                    crate::db_cache::CachedDataRoot::default(),
                )?;
                Ok::<_, reth_db::DatabaseError>(())
            })
            .unwrap()
            .unwrap();
        ensure_db_version_compatible(&consensus, &cache).unwrap();
        // Run again — must be a no-op.
        ensure_db_version_compatible(&consensus, &cache).unwrap();

        let cache_count = cache
            .view(|tx| tx.entries::<CachedDataRoots>())
            .unwrap()
            .unwrap();
        assert_eq!(cache_count, 1);
    }

    #[test]
    fn newer_cache_version_errors() {
        let (consensus, cache, _c, _k) = open_pair();

        // Stamp the cache with a version newer than CURRENT.
        cache
            .update(|tx| {
                tx.put::<crate::tables::CacheMetadata>(
                    MetadataKey::DBSchemaVersion,
                    (DatabaseVersion::V4 as u32 + 1).to_le_bytes().to_vec(),
                )?;
                Ok::<_, reth_db::DatabaseError>(())
            })
            .unwrap()
            .unwrap();

        let err = ensure_db_version_compatible(&consensus, &cache).unwrap_err();
        assert!(err.to_string().contains("Cache database schema version"));
    }
}
```

- [ ] **Step 6: Run the database tests**

Run: `cargo nextest run -p irys-database`

Expected: all migration tests pass, including the new V3→V4 tests.

If a method like `seek_by_key_subkey_exact` doesn't exist, the build will fail in step 3 above — return there and substitute the alternative pattern noted.

- [ ] **Step 7: Commit**

```bash
git add crates/database/src/migration.rs crates/database/src/database.rs
git commit -m "feat(database): V3->V4 migration splits cache tables to sibling env"
```

---

## Task 7: Add `open_or_create_irys_cache_data_db`

### Files
- Modify: `crates/storage/src/irys_consensus_data_db.rs`

- [ ] **Step 1: Add the cache-side opener**

Edit `crates/storage/src/irys_consensus_data_db.rs`:

```rust
use irys_database::tables::{CacheTables, IrysTables};
use irys_database::{IrysDatabaseArgs as _, open_or_create_cache_db, open_or_create_db};
use irys_types::DbSyncMode;
use reth_db::DatabaseEnv;
use reth_db::mdbx::DatabaseArguments;
use std::path::Path;

pub fn open_or_create_irys_consensus_data_db(
    path: impl AsRef<Path>,
    sync_mode: DbSyncMode,
) -> eyre::Result<DatabaseEnv> {
    open_or_create_db(
        path,
        IrysTables::ALL,
        DatabaseArguments::irys_default(sync_mode)?,
    )
}

/// Open or create the cache-side MDBX environment.
///
/// Uses the wider growth/shrink geometry from `DatabaseArguments::irys_cache`
/// because the cache tables churn in large batches during pruning.
pub fn open_or_create_irys_cache_data_db(
    path: impl AsRef<Path>,
    sync_mode: DbSyncMode,
) -> eyre::Result<DatabaseEnv> {
    open_or_create_cache_db(path, CacheTables::ALL, sync_mode)
}
```

- [ ] **Step 2: Compile-check**

Run: `cargo check -p irys-storage`

Expected: succeeds.

- [ ] **Step 3: Commit**

```bash
git add crates/storage/src/irys_consensus_data_db.rs
git commit -m "feat(storage): add open_or_create_irys_cache_data_db"
```

---

## Task 8: `DatabaseProvider` holds both envs

### Files
- Modify: `crates/types/src/app_state.rs`
- Modify: `crates/database/src/db.rs` (the `IrysDatabaseExt` trait)
- Modify: `crates/database/src/database.rs` (the `DatabaseProvider`-related helpers, if any)

- [ ] **Step 1: Update `DatabaseProvider` to hold both envs**

Replace `crates/types/src/app_state.rs`:

```rust
use reth_db::DatabaseEnv;
use std::{ops::Deref, sync::Arc};

pub struct AppState {}

/// Holds both MDBX environments.
///
/// `Deref` targets the consensus env so existing code that does
/// `db.tx()` / `db.update(...)` / `db.view(...)` continues to operate on
/// the consensus database without modification. Cache access is via the
/// dedicated accessors (`cache()` plus the `view_cache` / `update_cache`
/// extension methods provided by `irys_database`).
#[derive(Debug, Clone)]
pub struct DatabaseProvider {
    consensus: Arc<DatabaseEnv>,
    cache: Arc<DatabaseEnv>,
}

impl DatabaseProvider {
    pub fn new(consensus: Arc<DatabaseEnv>, cache: Arc<DatabaseEnv>) -> Self {
        Self { consensus, cache }
    }

    /// Accessor for the consensus environment.
    pub fn consensus(&self) -> &Arc<DatabaseEnv> {
        &self.consensus
    }

    /// Accessor for the cache environment.
    pub fn cache(&self) -> &Arc<DatabaseEnv> {
        &self.cache
    }
}

impl Deref for DatabaseProvider {
    type Target = Arc<DatabaseEnv>;

    /// Deref targets the consensus env so existing call sites keep working.
    fn deref(&self) -> &Self::Target {
        &self.consensus
    }
}

#[derive(Debug, Clone)]
pub struct RethDatabaseProvider(pub Arc<DatabaseEnv>);

impl Deref for RethDatabaseProvider {
    type Target = Arc<DatabaseEnv>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
```

- [ ] **Step 2: Add `view_cache` / `update_cache` extension to `IrysDatabaseExt`**

In `crates/database/src/db.rs`, after the existing `IrysDatabaseExt` trait and impls, add a new extension trait on `DatabaseProvider`:

```rust
pub trait DatabaseProviderCacheExt {
    fn update_cache_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&crate::scoped_tx::ScopedTxMut<crate::scoped_tx::Cache>) -> eyre::Result<T>;

    fn view_cache_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&crate::scoped_tx::ScopedTx<crate::scoped_tx::Cache>) -> eyre::Result<T>;

    fn update_cache<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&crate::scoped_tx::ScopedTxMut<crate::scoped_tx::Cache>) -> T;

    fn view_cache<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&crate::scoped_tx::ScopedTx<crate::scoped_tx::Cache>) -> T;
}

impl DatabaseProviderCacheExt for irys_types::DatabaseProvider {
    fn update_cache_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&crate::scoped_tx::ScopedTxMut<crate::scoped_tx::Cache>) -> eyre::Result<T>,
    {
        let inner = self.cache().tx_mut()?;
        let tx = crate::scoped_tx::ScopedTxMut::<crate::scoped_tx::Cache>::new(inner);
        let res = f(&tx)?;
        tx.commit()?;
        Ok(res)
    }

    fn view_cache_eyre<T, F>(&self, f: F) -> eyre::Result<T>
    where
        F: FnOnce(&crate::scoped_tx::ScopedTx<crate::scoped_tx::Cache>) -> eyre::Result<T>,
    {
        let inner = self.cache().tx()?;
        let tx = crate::scoped_tx::ScopedTx::<crate::scoped_tx::Cache>::new(inner);
        let res = f(&tx)?;
        tx.commit()?;
        Ok(res)
    }

    fn update_cache<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&crate::scoped_tx::ScopedTxMut<crate::scoped_tx::Cache>) -> T,
    {
        let inner = self.cache().tx_mut()?;
        let tx = crate::scoped_tx::ScopedTxMut::<crate::scoped_tx::Cache>::new(inner);
        let res = f(&tx);
        tx.commit()?;
        Ok(res)
    }

    fn view_cache<T, F>(&self, f: F) -> Result<T, DatabaseError>
    where
        F: FnOnce(&crate::scoped_tx::ScopedTx<crate::scoped_tx::Cache>) -> T,
    {
        let inner = self.cache().tx()?;
        let tx = crate::scoped_tx::ScopedTx::<crate::scoped_tx::Cache>::new(inner);
        let res = f(&tx);
        tx.commit()?;
        Ok(res)
    }
}
```

Note: this trait targets `irys_types::DatabaseProvider` directly. Add `pub use db::DatabaseProviderCacheExt;` to `crates/database/src/lib.rs` after the existing re-exports.

- [ ] **Step 3: Compile-check**

Run: `cargo check -p irys-types -p irys-database`

Expected: succeeds.

A wave of compile errors will appear downstream because:
- Callers using `DatabaseProvider(Arc::new(...))` tuple-construction now break.
- `db.tx()`, `db.update(...)`, `db.view(...)` still work (Deref-based) — these should not have broken.

Do NOT fix downstream errors yet; the next tasks update construction sites.

- [ ] **Step 4: Commit**

```bash
git add crates/types/src/app_state.rs crates/database/src/db.rs crates/database/src/lib.rs
git commit -m "feat(database): DatabaseProvider holds consensus + cache envs"
```

---

## Task 9: Update construction sites — `chain.rs::init_irys_db`

### Files
- Modify: `crates/chain/src/chain.rs`

- [ ] **Step 1: Update `init_irys_db` to open both envs**

In `crates/chain/src/chain.rs`, find `fn init_irys_db` (around line 2440). Replace its body with:

```rust
#[tracing::instrument(level = "trace", skip_all)]
fn init_irys_db(node_config: &NodeConfig) -> Result<DatabaseProvider, eyre::Error> {
    let consensus = irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db(
        node_config.irys_consensus_data_dir(),
        node_config.database.sync_mode,
    )?;
    let cache = irys_storage::irys_consensus_data_db::open_or_create_irys_cache_data_db(
        node_config.irys_cache_data_dir(),
        node_config.database.cache_sync_mode,
    )?;
    irys_database::migration::ensure_db_version_compatible(&consensus, &cache)?;
    let db = DatabaseProvider::new(Arc::new(consensus), Arc::new(cache));
    debug!("Irys DB initialized (consensus + cache)");
    Ok(db)
}
```

- [ ] **Step 2: Compile-check**

Run: `cargo check -p irys-chain`

Expected: succeeds.

- [ ] **Step 3: Commit**

```bash
git add crates/chain/src/chain.rs
git commit -m "feat(chain): open consensus + cache DBs in init_irys_db"
```

---

## Task 10: Test-setup helper — `DatabaseProvider::for_testing`

### Files
- Modify: `crates/types/src/app_state.rs` OR `crates/database/src/db.rs` (whichever can take a dependency on `irys_testing_utils`)

The helper opens both envs against temp dirs, runs the migration, returns a `DatabaseProvider`. It's used by the dozen test sites that today call `open_or_create_irys_consensus_data_db` directly.

- [ ] **Step 1: Add the helper in `crates/database/src/db.rs`** (lives next to the cache ext-trait)

```rust
impl irys_types::DatabaseProvider {
    /// Test-only constructor: opens both consensus and cache envs at the
    /// given paths in `irys_testing` (UtterlyNoSync) mode and runs the
    /// migration. The caller owns the temp directories — drop them
    /// after the test to clean up.
    pub fn for_testing(
        consensus_path: &std::path::Path,
        cache_path: &std::path::Path,
    ) -> eyre::Result<Self> {
        use crate::IrysDatabaseArgs as _;
        use crate::tables::{CacheTables, ConsensusTables};
        use reth_db::mdbx::DatabaseArguments;
        let consensus = crate::open_or_create_db(
            consensus_path,
            ConsensusTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let cache = crate::open_or_create_db(
            cache_path,
            CacheTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        crate::migration::ensure_db_version_compatible(&consensus, &cache)?;
        Ok(Self::new(std::sync::Arc::new(consensus), std::sync::Arc::new(cache)))
    }
}
```

NOTE: this `impl` block is in `irys_database` but adds methods to `irys_types::DatabaseProvider`. The orphan rule allows this because `irys_database` adds `for_testing` only via an inherent-style method on a type from another crate — Rust forbids that. We need an *extension trait* instead. Adjust:

```rust
pub trait DatabaseProviderTestExt {
    fn for_testing(
        consensus_path: &std::path::Path,
        cache_path: &std::path::Path,
    ) -> eyre::Result<Self>
    where
        Self: Sized;
}

impl DatabaseProviderTestExt for irys_types::DatabaseProvider {
    fn for_testing(
        consensus_path: &std::path::Path,
        cache_path: &std::path::Path,
    ) -> eyre::Result<Self> {
        use crate::IrysDatabaseArgs as _;
        use crate::tables::{CacheTables, ConsensusTables};
        use reth_db::mdbx::DatabaseArguments;
        let consensus = crate::open_or_create_db(
            consensus_path,
            ConsensusTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let cache = crate::open_or_create_db(
            cache_path,
            CacheTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        crate::migration::ensure_db_version_compatible(&consensus, &cache)?;
        Ok(Self::new(
            std::sync::Arc::new(consensus),
            std::sync::Arc::new(cache),
        ))
    }
}
```

Add `pub use db::DatabaseProviderTestExt;` to `crates/database/src/lib.rs`.

- [ ] **Step 2: Compile-check**

Run: `cargo check -p irys-database`

Expected: succeeds.

- [ ] **Step 3: Commit**

```bash
git add crates/database/src/db.rs crates/database/src/lib.rs
git commit -m "feat(database): DatabaseProvider::for_testing helper"
```

---

## Task 11: Update test setups to use `for_testing`

### Files
- Modify: `crates/p2p/src/peer_network_service.rs:1153,1200`
- Modify: `crates/p2p/src/chain_sync.rs:1878,1895,2072,2146,2218,2349`
- Modify: `crates/p2p/src/server.rs:1882,1897`
- Modify: `crates/p2p/src/tests/util.rs:21,272`
- Modify: `crates/p2p/src/tests/block_pool/mod.rs:16,68`
- Modify: `crates/actors/src/anchor_validation_tests.rs`
- Modify: `crates/actors/tests/block_validation_tests.rs`
- Modify: `crates/actors/tests/epoch_snapshot_tests.rs`
- Modify: `crates/chain-tests/src/utils.rs` (if it opens DBs directly)
- Modify: `crates/cli/src/db_utils.rs:227` — leaves consensus-only open (CLI sub-commands may legitimately want consensus-only access; do not split for the CLI here).

For each: rather than mechanically rewriting every file in one task, do the rewrite in a single sweep but commit per-crate.

- [ ] **Step 1: p2p sweep**

For each match in `crates/p2p/...` listed above, find the `open_or_create_irys_consensus_data_db(temp_dir.path(), DbSyncMode::UtterlyNoSync)` pattern. Replace with:

```rust
let cache_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
let db = irys_types::DatabaseProvider::for_testing(temp_dir.path(), cache_dir.path())
    .expect("db setup");
```

When the existing code wraps the env in `DatabaseProvider(Arc::new(env))`, replace the wrapping too — `for_testing` already returns a `DatabaseProvider`.

Run after sweep: `cargo check -p irys-p2p` — expected: succeeds.

Commit: `git commit -am "test(p2p): use DatabaseProvider::for_testing"`

- [ ] **Step 2: actors test sweep**

Same pattern for `crates/actors/src/anchor_validation_tests.rs` and tests under `crates/actors/tests/`. The `crates/database/src/database.rs:872` and `:960` test sites stay (they exercise the raw `open_or_create_db` API and don't need a cache env).

Run: `cargo check -p irys-actors --tests` — expected: succeeds.

Commit: `git commit -am "test(actors): use DatabaseProvider::for_testing"`

- [ ] **Step 3: chain-tests sweep**

`crates/chain-tests/src/utils.rs` and other chain-tests files. Same pattern.

Run: `cargo check -p irys-chain-tests` — expected: succeeds.

Commit: `git commit -am "test(chain-tests): use DatabaseProvider::for_testing"`

---

## Task 12: Migrate `database.rs` cache free functions to scoped signatures

### Files
- Modify: `crates/database/src/database.rs`

These free functions today take `<TX: DbTx>` / `<T: DbTx + DbTxMut>`. They need to be tightened to `&ScopedTx<Cache>` / `&ScopedTxMut<Cache>`. The list:

- `confirm_data_size_for_data_root` (line 264)
- `cache_data_root` (line 284)
- `cached_data_root_by_data_root` (line 337)
- `cache_chunk` (line 349)
- `cache_chunk_verified` (line 372)
- `cached_chunk_meta_by_offset` (line 407)
- `cached_chunk_by_chunk_offset` (line 420)
- `cached_chunk_by_chunk_path_hash` (line 444)
- `delete_cached_chunks_by_data_root` (line 453)
- `delete_cached_chunks_by_data_root_older_than` (line 473)
- `ingress_proofs_by_data_root` (line 533)
- `ingress_proof_by_data_root_address` (line 545)
- `delete_ingress_proof` (line 562)
- `store_ingress_proof_checked` (line 574)
- `store_external_ingress_proof_checked` (line 609)

- [ ] **Step 1: Tighten each cache fn signature**

For each function, change the `tx: &T` (where `T: DbTx + DbTxMut`) to `tx: &ScopedTxMut<Cache>` for fns that mutate, and `tx: &ScopedTx<Cache>` for read-only fns.

Example transformation — `cache_chunk`:

Before:
```rust
pub fn cache_chunk<T: DbTx + DbTxMut>(tx: &T, chunk: &UnpackedChunk) -> eyre::Result<IsDuplicate> {
    if tx.get::<CachedDataRoots>(chunk.data_root)?.is_none() {
        return Err(eyre::eyre!(
            "Data root {} not found in CachedDataRoots",
            chunk.data_root
        ));
    }
    cache_chunk_verified(tx, chunk)
}
```

After:
```rust
pub fn cache_chunk(
    tx: &crate::scoped_tx::ScopedTxMut<crate::scoped_tx::Cache>,
    chunk: &UnpackedChunk,
) -> eyre::Result<IsDuplicate> {
    if tx.get::<CachedDataRoots>(chunk.data_root)?.is_none() {
        return Err(eyre::eyre!(
            "Data root {} not found in CachedDataRoots",
            chunk.data_root
        ));
    }
    cache_chunk_verified(tx, chunk)
}
```

For functions that *read* only (e.g., `cached_data_root_by_data_root`), use `&ScopedTx<Cache>`. For functions that do both reads and writes (most of the cache fns), `&ScopedTxMut<Cache>` (which exposes both `get` and `put`).

`store_ingress_proof` is special — it calls `db.update(...)` to open its own tx. Rewrite:

```rust
pub fn store_ingress_proof(
    db: &DatabaseProvider,
    ingress_proof: &IngressProof,
    signer: &IrysSigner,
) -> eyre::Result<()> {
    use crate::db::DatabaseProviderCacheExt as _;
    db.update_cache_eyre(|rw_tx| store_ingress_proof_checked(rw_tx, ingress_proof, signer))
}
```

`store_ingress_proof_checked` and `store_external_ingress_proof_checked` open their own internal cursors; their tx parameter changes to `&ScopedTxMut<Cache>`.

Tip: in each function body, where the old generic `tx` was used for `cursor_dup_read`/`cursor_dup_write`/`get`/`put`, the new typed `ScopedTxMut<Cache>` exposes the same methods (with `CacheTable` bounds). If the method isn't on the wrapper but exists on the inner reth tx, drop to `tx.inner()` for that one call.

- [ ] **Step 2: Compile-check**

Run: `cargo check -p irys-database`

Expected: succeeds. If a free function called from a `database.rs` test (lines 837–993) breaks, update the test call sites to use `db.update_cache(|tx| ...)` analogously.

- [ ] **Step 3: Commit**

```bash
git add crates/database/src/database.rs
git commit -m "refactor(database): tighten cache free fns to ScopedTx<Cache>"
```

---

## Task 13: Migrate cache_service.rs (~25 call sites)

### Files
- Modify: `crates/actors/src/cache_service.rs`

- [ ] **Step 1: Add the cache ext trait import**

At the top of `crates/actors/src/cache_service.rs`, add:

```rust
use irys_database::db::DatabaseProviderCacheExt as _;
```

- [ ] **Step 2: Replace cache-table `db.update`/`db.view` with `db.update_cache`/`db.view_cache`**

For each line listed below (line numbers from the spec; verify they still match after earlier tasks), replace the pattern:

- `self.db.view_eyre(|tx| { ... })` → `self.db.view_cache_eyre(|tx| { ... })`
- `self.db.update_eyre(|tx| { ... })` → `self.db.update_cache_eyre(|tx| { ... })`
- `db.view(|rtx| ...)` → `db.view_cache(|rtx| ...)`
- `db.update(|wtx| ...)` → `db.update_cache(|wtx| ...)`
- etc.

Lines to touch (cache-table operations only; verify each is in fact cache-only):
215, 374, 422, 449, 452, 473, 490, 521, 533, 1004, 1016, 1091, 1097, 1177, 1188, 1194, 1272, 1283, 1361, 1375, 1429, 1494, 1614, 1625, 1685, 1691, 1704.

Inside each closure, references like `tx.put::<CachedDataRoots>(...)` continue to work — the typed wrapper exposes the same `put`/`get`/`cursor_*` methods, just constrained to `CacheTable`.

If a closure body uses `tx.entries::<IngressProofs>()`, that maps to `tx.entries::<IngressProofs>()` on `ScopedTx<Cache>` (no change in call syntax).

- [ ] **Step 3: Compile-check**

Run: `cargo check -p irys-actors`

Expected: succeeds.

- [ ] **Step 4: Run cache_service tests**

Run: `cargo nextest run -p irys-actors cache_service`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/actors/src/cache_service.rs
git commit -m "refactor(actors): cache_service uses update_cache/view_cache"
```

---

## Task 14: Migrate chunk_ingress_service

### Files
- Modify: `crates/actors/src/chunk_ingress_service/chunks.rs`
- Modify: `crates/actors/src/chunk_ingress_service/ingress_proofs.rs`

- [ ] **Step 1: Add the ext trait import to both files**

```rust
use irys_database::db::DatabaseProviderCacheExt as _;
```

- [ ] **Step 2: Switch cache-table call sites**

In `chunks.rs`:
- `:115` (`view_eyre` of `cached_chunk_meta_by_offset` reads)
- `:355` (`update_eyre` for `confirm_data_size_for_data_root`)
- `:517` (`view_eyre` for `cached_data_root_by_data_root`)
- `:546` (`view_eyre` reading `CachedChunksIndex`)
- `:671` (`update_eyre` for `confirm_data_size_for_data_root`)
- `:813` (`view_eyre` of `cached_chunk_by_chunk_offset`)
- `:879` (`db.update(|rw_tx| store_ingress_proof_checked(rw_tx, ...))` — switch to `db.update_cache(|rw_tx| store_ingress_proof_checked(rw_tx, ...))`)
- `:845` (`tx.get::<CachedChunks>(...)`) — inside a `view_eyre` that's in step 6's scope

Same `db.view_eyre(...)` → `db.view_cache_eyre(...)` and `db.update_eyre(...)` → `db.update_cache_eyre(...)` substitution.

In `ingress_proofs.rs`:
- `:192` (`db.update(...)` for `delete_ingress_proof`) → `db.update_cache(...)`.
- `:502` (`store_ingress_proof(db, ...)`) — this already takes a `db: &DatabaseProvider`; it now internally uses `update_cache_eyre`. No change at the call site beyond the imports.
- `:562` (cursor read of `CachedChunksIndex`) — wrap in `view_cache`.

- [ ] **Step 3: Compile-check**

Run: `cargo check -p irys-actors`

Expected: succeeds.

- [ ] **Step 4: Run chunk_ingress tests**

Run: `cargo nextest run -p irys-actors chunk_ingress`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/actors/src/chunk_ingress_service/
git commit -m "refactor(actors): chunk_ingress_service uses cache-scoped transactions"
```

---

## Task 15: Migrate mempool_service

### Files
- Modify: `crates/actors/src/mempool_service/data_txs.rs:459`

- [ ] **Step 1: Switch `cache_data_root_with_expiry`**

In `crates/actors/src/mempool_service/data_txs.rs`:

```rust
use irys_database::db::DatabaseProviderCacheExt as _;
// ...
fn cache_data_root_with_expiry(&self, tx: &DataTransactionHeader, expiry_height: u64) {
    match self.irys_db.update_cache_eyre(|db_tx| {
        let mut cdr = irys_database::cache_data_root(db_tx, tx, None)?
            .ok_or_else(|| eyre!("failed to cache data_root"))?;
        cdr.expiry_height = Some(expiry_height);
        db_tx.put::<CachedDataRoots>(tx.data_root, cdr)?;
        Ok(())
    }) {
        Ok(()) => { ... }
        Err(db_error) => { ... }
    };
}
```

- [ ] **Step 2: Compile-check**

Run: `cargo check -p irys-actors`

Expected: succeeds.

- [ ] **Step 3: Run mempool tests**

Run: `cargo nextest run -p irys-actors mempool`

Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/actors/src/mempool_service/data_txs.rs
git commit -m "refactor(actors): mempool cache_data_root_with_expiry uses update_cache_eyre"
```

---

## Task 16: Migrate tx_selector

### Files
- Modify: `crates/actors/src/tx_selector/mod.rs`

- [ ] **Step 1: Switch the publish-candidate scan**

Around line 772, the publish-candidate walk reads `IngressProofs` and `CachedDataRoots`. Change:

```rust
let (publish_txids, cached_data_roots) = ctx
    .db
    .view_eyre(|tx| { ... })
```

to:

```rust
use irys_database::db::DatabaseProviderCacheExt as _;
// ...
let (publish_txids, cached_data_roots) = ctx
    .db
    .view_cache_eyre(|tx| { ... })
```

Around line 907 (`ingress_proofs_by_data_root` call), wrap in `view_cache_eyre`.

- [ ] **Step 2: Compile-check + test**

Run:
```bash
cargo check -p irys-actors
cargo nextest run -p irys-actors tx_selector
```

Expected: both succeed.

- [ ] **Step 3: Commit**

```bash
git add crates/actors/src/tx_selector/mod.rs
git commit -m "refactor(actors): tx_selector uses view_cache_eyre for cache reads"
```

---

## Task 17: Migrate api-server, debug-utils, and chain-tests cache reads

### Files
- Modify: `crates/api-server/src/lib.rs` and `crates/api-server/src/routes/block.rs` (cache reads in route handlers)
- Modify: `crates/utils/debug-utils/src/db.rs`
- Modify: relevant chain-tests files that read `IngressProofs` or `CachedChunks`:
  - `crates/chain-tests/src/external/api.rs:110`
  - `crates/chain-tests/src/external/programmable_data_basic.rs:154`
  - `crates/chain-tests/src/integration/cache_service.rs:141, 153, 198`
  - `crates/chain-tests/src/multi_node/mempool_tests.rs:1326`
  - `crates/chain-tests/src/promotion/data_promotion_double.rs:239`
  - `crates/chain-tests/src/promotion/stale_txid_in_cached_data_root.rs:88`
  - `crates/chain-tests/src/validation/mempool_ingress_proof_dedup.rs:179`
  - `crates/chain-tests/src/utils.rs:1286, 1427`

- [ ] **Step 1: Sweep — switch reads of cache tables to `view_cache(_eyre)`; writes to `update_cache(_eyre)`**

For each call site, follow the same mechanical substitution as previous tasks. Skip sites that read consensus tables — those keep using the existing methods.

If a test holds a `ro_tx` cursor across both consensus and cache tables (search for patterns where a single `ro_tx` does `.get::<Foo>(...)` for both), split into two scoped txns.

- [ ] **Step 2: Compile-check**

Run:
```bash
cargo check -p irys-api-server
cargo check -p irys-debug-utils
cargo check -p irys-chain-tests
```

Expected: all succeed.

- [ ] **Step 3: Commit**

```bash
git add crates/api-server/ crates/utils/debug-utils/ crates/chain-tests/
git commit -m "refactor: api-server / debug-utils / chain-tests use cache-scoped tx"
```

---

## Task 18: Tighten remaining (now misplaced) generic bounds

### Files
- Modify: `crates/database/src/database.rs` consensus free fns (optional)
- Modify: `crates/database/src/db_index.rs` (optional)

Optional cleanup — only do this if `cargo check` produces zero unused-trait-bound warnings; otherwise defer.

- [ ] **Step 1: Tighten consensus free fns to `&ScopedTx<Consensus>` / `&ScopedTxMut<Consensus>`**

Same pattern as Task 12 but for consensus-side functions (`insert_block_header`, `block_header_by_hash`, `insert_tx_header`, etc.). This is purely additive type safety; behavior is identical.

If any function is *legitimately* scope-agnostic (`walk_all`, `get_cache_size`), leave it generic.

- [ ] **Step 2: Compile-check + test**

Run:
```bash
cargo xtask check
cargo nextest run -p irys-database -p irys-actors
```

Expected: succeed. If they don't, revert this task's changes — the gains are marginal.

- [ ] **Step 3: Commit (if changes were kept)**

```bash
git add crates/database/src/
git commit -m "refactor(database): tighten consensus free fns to ScopedTx<Consensus>"
```

If reverted: skip the commit.

---

## Task 19: Telemetry — add `db` label

### Files
- Modify: `crates/database/src/reth_ext.rs`
- Modify: wherever metrics were attached in the prior telemetry branches (search for `tx_mut_acquire`, `libmdbx_rw_tx_stalls`, `update_scoped`)

This branch starts clean (no telemetry commits yet on `feat/cache-db`), but the design requires the existing OTel/metrics paths to carry a `db = "consensus" | "cache"` label.

- [ ] **Step 1: Audit current metrics emission**

Run:
```bash
grep -rn "tx_mut_acquire\|libmdbx_rw_tx_stalls\|update_scoped\|metrics::counter\|metrics::histogram" --include="*.rs" crates/database/ | head -20
```

If the grep returns matches, locate the metric attachment site and extend the label set with `db`. If the grep returns nothing, this telemetry was on *other* branches not yet merged into `feat/cache-db` — note this in the commit and skip the implementation. Add a comment in `reth_ext.rs` flagging that the `db` label will be added when the telemetry branches land.

- [ ] **Step 2: Compile-check**

Run: `cargo xtask check`

Expected: succeeds.

- [ ] **Step 3: Commit (or skip if telemetry isn't present yet)**

If changes made:

```bash
git add crates/database/src/
git commit -m "feat(telemetry): tag db metrics with consensus/cache label"
```

---

## Task 20: Full verification

### Files
- Read-only.

- [ ] **Step 1: Run local-checks (fmt + clippy + unused-deps + typos)**

Run: `cargo xtask local-checks`

Expected: clean. If clippy complains about unused imports (likely from the ext-trait `as _` imports we added), fix inline.

- [ ] **Step 2: Run the full test suite via nextest**

Run: `cargo xtask test`

Expected: green. If anything fails, group failures:
- Cache-related test failures → bug in scope-wrapper translation; re-read the relevant Task 12/13 changes.
- Migration test failures → bug in `v3_to_v4`; the idempotency and resume paths need to be re-checked.
- Other failures → likely an unrelated regression; isolate and report before continuing.

- [ ] **Step 3: Run flaky-detection on the affected services 5x**

Run: `cargo xtask flaky -i 5`

Expected: no new flakiness in `cache_service`, `chunk_ingress`, `mempool`, `tx_selector` tests.

- [ ] **Step 4: Smoke-test the actual binary**

Run a local devnet bootstrap (this validates that `init_irys_db` correctly opens both envs and that the migration runs against a real on-disk database):

```bash
# In a scratch tmp dir
mkdir -p .tmp/cache-db-smoke
IRYS_CUSTOM_TMP_DIR=$(pwd)/.tmp/cache-db-smoke \
    cargo run --release -p irys-chain -- start-node --help 2>&1 | head -20
```

If the binary requires more setup, fall back to: `cargo run -p irys-chain -- --help` and confirm it doesn't panic during config parse.

To explicitly exercise the migration: copy an existing V3 database into the test base directory under `irys_consensus_data/`, run the node briefly (Ctrl-C after init), and verify `irys_cache_data/` was created and contains the cache tables. (If no V3 database is available, skip this step.)

- [ ] **Step 5: Final summary commit (no-op or housekeeping)**

If no changes need committing, skip. Otherwise:

```bash
git status
# resolve any leftovers
git commit -am "chore(cache-db): final cleanup"
```

- [ ] **Step 6: Confirm with the user**

The user must decide whether to push (recall: never push without explicit confirmation per `/home/vscode/.claude/CLAUDE.md`). State the test results and ask.

---

## Self-review (run after writing the plan)

(Performed by the plan author; engineers executing can skim and skip.)

* **Spec coverage**: every section of the spec maps to a task — V4 (Task 1), `irys_cache_data_dir` (Task 2), `scoped_tx` (Task 3), table split (Task 4), marker-trait tests (Task 5), V3→V4 migration (Task 6), cache-db open helper (Task 7), DatabaseProvider two-env (Task 8), `init_irys_db` (Task 9), test setups (Tasks 10–11), free-fn tightening (Task 12), service call sites (Tasks 13–17), telemetry (Task 19), verification (Task 20).
* **Placeholders**: scanned; no "TBD" or "implement later" remain. The optional Task 18 is genuinely optional, not a placeholder.
* **Type consistency**: `ScopedTx`, `ScopedTxMut`, `DbScope`, `Consensus`, `Cache`, `ConsensusTable`, `CacheTable`, `view_cache`, `update_cache`, `view_cache_eyre`, `update_cache_eyre`, `DatabaseProviderCacheExt`, `DatabaseProviderTestExt`, `for_testing` — names used consistently across tasks.
* **Ambiguity**: the `seek_by_key_subkey_exact` fallback is the one known-unknown; the plan explicitly tells the implementer how to substitute. Otherwise no hidden choices left to the implementer.
