# Cache DB Split

## Status
Accepted

## Context

The Irys consensus MDBX environment (`<base>/irys_consensus_data/`) held both consensus-critical tables (block headers, tx headers, ledger metadata, peer list) and reconstructable cache tables (cached data roots, cached chunks, cached chunk index, ingress proofs). MDBX serializes writers per environment — a single rw-tx slot per env — so a long-running cache-pruning batch in `cache_service` (multi-thousand-row walks of `CachedChunksIndex` deleting from `CachedChunks` / `IngressProofs`) would block unrelated consensus writes (block migration, mempool tx insertion, block production metadata persistence).

Telemetry on prior branches (`tx_mut_acquire` histogram, libmdbx rw-tx lock-stall counter, `update_scoped` attribution) confirmed that contention on the per-env writer lock was the source of observed stalls.

The alternative of optimizing cache-pruning batch sizes was rejected because it papers over the structural issue: as long as cache state lives in the same env as consensus state, any cache rewrite competes with consensus writes for one lock.

## Decision

Open two MDBX environments at startup:

```
<base_directory>/
├── irys_consensus_data/    # existing — consensus-critical tables
└── irys_cache_data/        # new — reconstructable cache tables
```

Move exactly four tables into the cache env: `CachedDataRoots`, `CachedChunksIndex`, `CachedChunks`, `IngressProofs`. Each env carries its own `DBSchemaVersion` stamp (the cache env's `CacheMetadata` table is structurally identical to consensus `Metadata` but with a distinct on-disk name to avoid the `tables!` macro generating duplicate struct types).

Two tables that one might expect to move were deliberately kept in the consensus env:

- **`IrysPoAChunks`** — stores per-block PoA chunk blobs. Written atomically with `IrysBlockHeaders` in `block_migration_service::persist_block`. Moving it would either break that atomicity (header committed without its chunk under crash) or require multi-env coordination machinery we do not want. The contention savings did not justify the recovery complexity for v1.

- **`PeerListItems`** — peer reputation persistence. Not cache; not contention-heavy. Out of scope.

`IngressProofs` is moved despite the name suggesting permanence because it is a **pre-promotion working buffer**, not historic state. Historic / consensus-critical proofs live inside the published block header itself (the Publish ledger entry on `IrysBlockHeader` carries `proofs: Option<IngressProofsList>`). The `IngressProofs` table is actively pruned by `cache_service::prune_ingress_proofs` once proofs are promoted, expired, or unanchored. Worst case on cache loss: nodes lose pending publish candidates and the network re-gossips/regenerates them before the next promotion attempt.

`DatabaseProvider` (in `irys_types::app_state`) now holds both envs as named fields (`consensus`, `cache`). Its `Deref` target remains the consensus env so that existing call sites (`db.tx()`, `db.update(...)`, `db.view(...)`) continue compiling and operating against the consensus env without modification. Cache access goes through new explicit accessors (`db.cache()`) and the `DatabaseProviderCacheExt` extension trait (`view_cache`, `update_cache`, `_eyre` variants).

The cache env honors `DatabaseConfig::cache_sync_mode`, defaulting to `SafeNoSync` — never `UtterlyNoSync`. `SafeNoSync` skips fsync but preserves DB integrity on crash; `UtterlyNoSync` risks structural corruption, not just lost writes, and was explicitly rejected for production.

See also: [Scoped Transaction Types](scoped-transaction-types.md), [Cross-Env Migration (V3→V4)](cross-env-migration.md), [Database Schema Versioning and Migration](database-schema-versioning-and-migration.md), [Database Arguments Preset Trait](database-arguments-preset-trait.md)

## Consequences

- Cache pruning no longer blocks consensus writes — the two envs have independent writer locks. Telemetry (`db = consensus | cache` label) measures the contention drop directly.
- Atomicity that previously spanned cache + consensus tables in a single MDBX rw-tx is broken; this is acceptable because no such cross-table atomic invariant existed for the four moved tables, and the few mixed transactions found during the migration were split into two consecutive txns (e.g., `cache_service::prune_data_root_cache`, `cache_service::prune_ingress_proofs`, ingress-proof validation tests) without losing correctness.
- Operators get a second on-disk directory (`irys_cache_data/`); backup tooling must include it. A node that loses its cache env can rebuild from gossip — V4-consensus + no-cache-stamp is treated as recoverable, not an error.
- `DatabaseProvider`'s `Deref` to consensus is a deliberate backwards-compat shim; misuse (writing cache tables through the deref'd consensus env) is caught at MDBX-runtime by "table not found", and the scoped-transaction types make the correct path the obvious path.
- The cache env adopts wider growth/shrink geometry (`DatabaseArguments::irys_cache`: 50 MB growth / 100 MB shrink vs 10 MB / 20 MB for consensus) because cache writes happen in larger batches.
- A subset of test setups and pre-existing `DatabaseProvider` consumers that wrapped non-consensus envs (e.g., `StorageSubmodule`, the CLI `RollbackBlocks` command) now construct `DatabaseProvider::new(env.clone(), env)` to satisfy the two-env constructor while only using the Deref→consensus path. This is pre-existing misuse of `DatabaseProvider` as a generic single-env wrapper, flagged for follow-up cleanup.

## Source
Branch `feat/cache-db`; design spec `docs/superpowers/specs/2026-05-11-cache-db-split-design.md`.
