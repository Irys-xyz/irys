# Cross-Environment Migration (V3 → V4)

## Status
Accepted

## Context

The cache-DB split moves four tables (`CachedDataRoots`, `CachedChunksIndex`, `CachedChunks`, `IngressProofs`) out of the consensus MDBX environment into a sibling cache environment. Existing nodes already have rows in those tables inside their consensus DB; on first startup of the new binary those rows must end up in the cache DB and be removed from the consensus DB.

The existing migration framework (`ensure_db_version_compatible`, sequential `vN_to_vN+1` modules) was built around a single `&DatabaseEnv`. MDBX transactions are env-scoped — there is no native way to commit a write to env A and a write to env B atomically. Naïve single-tx migration was therefore impossible, and any cross-env migration must define crash semantics explicitly.

A "start fresh" alternative (drop cache rows in the consensus env, accept loss of pending publish candidates) was considered and rejected — existing operators have in-flight ingress proofs and cached chunks for unpromoted transactions; losing them silently on upgrade is poor UX.

## Decision

Extend the migration framework to take both envs:

```rust
pub fn ensure_db_version_compatible(consensus: &DatabaseEnv, cache: &DatabaseEnv) -> eyre::Result<()>;
```

Bump `DatabaseVersion::CURRENT` to `V4` and add a `v3_to_v4` migration module that copies each cache table from consensus → cache, then clears the consensus copies. Each table is processed in batches of 10 000 rows in separate per-side transactions:

1. **Copy batch** — read up to `BATCH_SIZE` rows from `consensus` starting after the last copied key; write them to `cache`, skipping rows whose key already exists in cache (presence check before `put`).
2. **Clear consensus** — once a table is fully copied, `tx.clear::<T>` in a single consensus write transaction.
3. **Stamp** — after all four tables, write `DatabaseVersion::V4` to both `Metadata` (consensus) and `CacheMetadata` (cache).

For DupSort tables (`CachedChunksIndex`, `IngressProofs`), idempotency relies on MDBX `UPSERT` semantics on DupSort sub-tables: `tx.put::<T>(k, v)` is a no-op when the exact `(key, sub_value)` pair already exists. This is verified against the reth-irys fork's MDBX `tx.rs` and removes the need for an explicit per-row "does this (key, sub_value) pair already exist?" cursor lookup.

The version-stamp matrix on every startup is:

| consensus stamp | cache stamp | action |
|---|---|---|
| none (fresh) | none | stamp both V4 |
| V3 | none / V3 | run `v3_to_v4`; stamp both V4 |
| V4 | V4 | no-op |
| V4 | none | accept fresh cache (consensus copies already cleared on a prior run, or operator deliberately deleted the cache directory); stamp cache V4 |
| > V4 | * | error — newer binary required |
| * | > V4 | error — newer binary required |

The "V4 / none" case is explicitly **recoverable, not an error**. Cache state is reconstructable from gossip; an operator who deletes `irys_cache_data/` to reclaim disk should not see startup failures.

`set_cache_database_schema_version` / `cache_database_schema_version` helpers in `database.rs` are written generic over `DbTx`/`DbTxMut` rather than against `ScopedTx<Cache>`, because the migration runs before the scoped-tx invariants are fully established (the cache `Metadata` table needs to be stamped *during* migration, not after). Inside `v3_to_v4` the copy helpers operate on raw reth txns via `.update_eyre` / `.view_eyre` directly on the `DatabaseEnv` handles.

See also: [Cache DB Split](cache-db-split.md), [Database Schema Versioning and Migration](database-schema-versioning-and-migration.md), [Scoped Transaction Types](scoped-transaction-types.md)

## Consequences

- Crash mid-copy of any table: rerun re-copies rows still missing in cache (presence-skip on non-dupsort; MDBX UPSERT no-op on dupsort); existing rows are unaffected. The clear-consensus step is a no-op on a re-run because the migration also re-copies any rows still present in consensus.
- Crash between copy and clear of table N: rerun re-copies (no-op) and then clears. Crash between table N's clear and table N+1's copy: rerun resumes at table N+1; table N's consensus copy is already empty so its re-copy step is also a no-op.
- Crash between the cache-side V4 stamp and the consensus-side V4 stamp: next startup sees consensus = V3, runs `v3_to_v4` again (idempotent), and stamps both V4. The order of the two stamps is therefore not load-bearing.
- During the migration, cache rows transiently exist in both envs. Operators with multi-GB `CachedChunks` should expect transient disk doubling for that one table. The post-migration consensus DB does **not** auto-shrink — MDBX retains free pages; a follow-up `mdbx_copy` compacts it but is out of scope.
- `DatabaseVersion::V4` is the first migration that takes two `DatabaseEnv` arguments. Future migrations may also span both envs (or only one); the `ensure_db_version_compatible` signature is now (consensus, cache) regardless.
- The exhaustiveness of the `match version { ... }` in `ensure_db_version_compatible` still forces the compiler to flag missing arms when `DatabaseVersion` gains a `V5`, preserving the property that adding a new schema version forces an explicit migration decision.
- `copy_dup_table` loads each DupSort table's row set in batches without explicit pagination of the dupsort key space. This is acceptable because (a) the cache tables are bounded by the operator's configured cache size (typically single-digit GBs), (b) the migration runs before any services start so peak memory pressure is bounded to this one task, and (c) MDBX UPSERT keeps reruns cheap. Future work could page the dupsort scan if production-scale evidence motivates it.

## Source
Branch `feat/cache-db`.
