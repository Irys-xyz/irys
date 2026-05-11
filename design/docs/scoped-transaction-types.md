# Scoped Transaction Types

## Status
Accepted

## Context

Splitting consensus and cache tables into two MDBX environments creates a new failure mode: a code path can compile and run while reading or writing the *wrong* environment. The error surfaces at MDBX runtime as a cryptic "no matching key/data pair found" (the sub-DB is absent), often deep inside a closure where the original env-vs-table mismatch is non-obvious. Several real instances of this were caught after the split:

- A test opening `db.tx()` (consensus env via `Deref`) and calling `walk_all::<IngressProofs, _>(&tx)` — the table now lives in the cache env.
- `block_validation::data_txs_are_valid` opening `db.tx()` for `cached_chunk_by_chunk_offset` (a cache-table read).
- Several chain-tests reading `IngressProofs` or walking `CachedChunks` through the consensus deref.

Catching these at compile time was preferred over runtime detection because (a) the runtime error gives no clue which env was expected, and (b) the test surface is large enough that runtime-only enforcement leaves real coverage gaps. The alternative of a single `DatabaseProvider` with separate `consensus_db()` / `cache_db()` accessors returning bare `Arc<DatabaseEnv>` was rejected — it still lets the wrong env be paired with any table at any call site without compiler help.

## Decision

Introduce two zero-sized scope marker types and a sealed trait in `crates/database/src/scoped_tx.rs`:

```rust
pub enum Consensus {}
pub enum Cache {}

pub trait DbScope: 'static + Send + Sync {}
impl DbScope for Consensus {}
impl DbScope for Cache {}
```

Tag each table type with exactly one marker trait:

```rust
pub trait ConsensusTable: Table {}
pub trait CacheTable: Table {}

impl ConsensusTable for IrysBlockHeaders {}
impl CacheTable for CachedChunks {}
// ... etc
```

Wrap the underlying reth `DatabaseEnv::TX` / `TXMut` in scope-parameterized newtypes:

```rust
pub struct ScopedTx<S: DbScope>    { inner: ...::TX,    _scope: PhantomData<fn() -> S> }
pub struct ScopedTxMut<S: DbScope> { inner: ...::TXMut, _scope: PhantomData<fn() -> S> }
```

Re-implement the small set of MDBX operations actually used (`get`, `put`, `delete`, `entries`, `clear`, `cursor_read`, `cursor_write`, `cursor_dup_read`, `cursor_dup_write`) as inherent methods on each `(ScopedTx, scope)` pair, bounded so that only tables matching the scope's marker trait are reachable:

```rust
impl ScopedTxMut<Cache> {
    pub fn put<T: Table + CacheTable>(&self, k: T::Key, v: T::Value) -> Result<(), DatabaseError> { ... }
    // ...
}
```

`tx.put::<CachedChunks>(k, v)` on a `ScopedTxMut<Consensus>` is a **compile error** because `CachedChunks` does not implement `ConsensusTable`.

`DatabaseProvider` exposes the scoped types through a `DatabaseProviderCacheExt` extension trait (`view_cache`, `update_cache`, plus `_eyre` variants) whose closure parameters are `&ScopedTx<Cache>` / `&ScopedTxMut<Cache>`. Existing consensus-targeting methods (`view`, `update`, …) keep working through the existing `IrysDatabaseExt`; they route to the consensus env via `Deref`.

The wrapper provides a `pub fn inner()` escape hatch returning the untyped reth tx for code that legitimately needs cross-scope access — most importantly the V3→V4 migration, which touches both envs' tables in the same code path, and the few free functions that remain generic over `DbTx` (`walk_all`, `get_cache_size`) and are called with `tx.inner()` from scoped call sites.

Table marker impls are emitted by a small `impl_consensus_tables!` / `impl_cache_tables!` macro in `tables.rs` rather than `#[derive]` machinery — the macros take an explicit list of table identifiers so adding a new table forces the author to decide which env it belongs to. `static_assertions::assert_impl_all!` / `assert_not_impl_any!` in a test module verifies the markers are mutually exclusive per scope.

Each `tables! { ... }` invocation in `tables.rs` is wrapped in a private inner module (`consensus_tables_inner`, `cache_tables_inner`) because the reth macro generates a private `mod table_names` per invocation; two top-level invocations in the same scope would collide on that module name. The struct and enum types from the inner modules are re-exported at the parent scope, preserving the existing `IrysTables::ALL` / `CachedChunks::NAME` surface.

See also: [Cache DB Split](cache-db-split.md)

## Consequences

- Cross-scope table access is a compile error, not a runtime error. The class of bugs found during the migration cannot recur silently.
- A small amount of code duplication exists between the `ScopedTx<Consensus>` and `ScopedTx<Cache>` impls (same method shape, different table marker bound). Specialization or `Borrow`-based generics could collapse them; the duplication is deemed acceptable for a fixed set of two scopes.
- `ScopedTxMut<S>` does not deref to `ScopedTx<S>`, so a few helpers that conceptually take a read-only tx (e.g., `cached_data_root_by_data_root` accepting `&ScopedTx<Cache>`) cannot be called directly from inside an `update_cache` closure with `&ScopedTxMut<Cache>`. Affected call sites inline the underlying `tx.get::<T>(...)` rather than synthesize a separate read tx. A `Borrow`-based unification is a possible follow-up.
- The `.inner()` escape hatch is documented as the only sanctioned cross-scope path. Migration code uses it; runtime code should not.
- New tables must be tagged with `ConsensusTable` or `CacheTable` in the `impl_*_tables!` invocation. Forgetting either marker yields a compile error at the first scoped call site (no implicit "untagged → no scope" default).
- `pub fn new` on `ScopedTx` / `ScopedTxMut` is exposed (rather than `pub(crate)`) so that downstream services like `cache_service::prune_data_root_cache` can manually construct a scoped tx when the closure-based `view_cache` / `update_cache` shape does not fit (e.g., async code holding the tx across `.await` points, or code that opens both a cache tx and a consensus tx in the same function).

## Source
Branch `feat/cache-db`; design spec `docs/superpowers/specs/2026-05-11-cache-db-split-design.md`.
