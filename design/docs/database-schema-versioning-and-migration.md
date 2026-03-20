# Database Schema Versioning and Startup Migration

## Status
Accepted

## Context

The database had no schema version tracking. When the on-disk format changed (e.g., removing a field from a Compact-encoded record), the node would silently read corrupt data or panic with an inscrutable decode error. Two specific risks motivated this:

1. **Silent data corruption on rollback** — if a newer binary migrated the database forward and an operator rolled back to an older binary, the older binary would misinterpret the new record format with no indication of the problem.
2. **No migration path for live networks** — mainnet-0.1.x databases existed with `promoted_height` inlined in `DataTransactionHeaderV1`. Moving that field to a separate `IrysDataTxMetadata` table required a migration that could run unattended at node startup, not a manual operator step.

The alternative of detecting format changes at the record level (e.g., checking Compact discriminants) was rejected because it would scatter version awareness across every table reader and couldn't protect against the rollback scenario.

## Decision

Introduce a `DatabaseVersion` enum (`V0`, `V1`, `V2`, …) in `irys-types` and a `Metadata` table key (`DBSchemaVersion`) that stores the current version as a little-endian `u32`. On startup, `ensure_db_version_compatible()` runs before any services are initialized.

A database without a version stamp is treated as V0. Since the V0→V1 transition is purely "add the stamp" (no data format change), the function unconditionally stamps V1 for any unstamped database — whether it contains legacy data or is brand-new. The V1→V2 migration is then applied in all cases; on an empty database this is a harmless no-op. This avoids fragile heuristics for distinguishing "fresh" from "legacy" databases.

The function then handles three cases based on the stamped version:

- **V0 or V1**: runs sequential migrations (V1→V2, future V2→V3, etc.) within MDBX write transactions and stamps each new version after its migration succeeds.
- **CURRENT version**: no-op.
- **Newer version than the binary**: returns an `eyre::bail!` error with a clear message telling the operator to use the newer binary or restore from backup. Rollback is explicitly unsupported.

Each migration is a module (`v1_to_v2`) in `crates/database/src/migration.rs` that receives a mutable MDBX transaction. Migrations process records in batches (10 000 records per batch) to keep memory bounded on large databases. The crash semantics differ between the initial stamp and subsequent migrations:

- **Initial unstamped path (V0→V1):** `DatabaseVersion::V1` is committed in its own write transaction *before* the migration loop begins. If the node crashes after this commit but before `v1_to_v2` completes, the database is left stamped V1 with no data changes — on next startup the migration loop picks up at V1 and re-runs `v1_to_v2` from scratch.
- **Subsequent migrations (V1→V2 and beyond):** The version stamp is written inside the same MDBX write transaction as the migration itself (e.g., `v1_to_v2::migrate` writes `DatabaseVersion::V2` at the end of the transaction). A crash mid-migration rolls back the entire transaction — both the data changes and the version bump — leaving the database at the previous version so the migration re-runs on next startup.

The V1→V2 migration specifically:
1. Reads every `IrysDataTxHeaders` record using the old Compact layout (with `promoted_height` inline), writes the new layout (without it), and moves `promoted_height` values into `IrysDataTxMetadata`.
2. Iterates all `IrysBlockHeaders` to back-fill `included_height` and `promoted_height` in `IrysDataTxMetadata` and `IrysCommitmentTxMetadata` from ledger membership, using min-height semantics when a transaction appears in multiple blocks (fork resolution).

The old Compact layout is preserved as `old_structures::DataTransactionHeaderV1WithPromotedHeight` in the migration module so that future developers can see exactly what the pre-migration format looked like. This structure is only compiled into the migration code path, not the main data path.

See also: [Centralized Version Enums](centralized-version-enums.md)

## Consequences

- Operators get a clear, actionable error instead of silent corruption when running version-mismatched binaries
- Migrations are deterministic and idempotent — re-running after a crash re-applies from the last stamped version
- Each new schema change requires adding a `DatabaseVersion` variant, a migration module, and a match arm in `ensure_db_version_compatible`. The match explicitly enumerates every `DatabaseVersion` variant by name (no wildcard/catch-all, no `CURRENT` alias as a pattern), so the compiler will emit an exhaustiveness error when a new variant is added — forcing the developer to handle it
- Old Compact layouts accumulate in `migration.rs` as historical artifacts; this is intentional to preserve decode capability for databases at any prior version
- The migration runs synchronously at startup, blocking service initialization — acceptable because migrations are infrequent and correctness outweighs startup latency

## Source
PR #1223 — feat: check database schema version on startup
