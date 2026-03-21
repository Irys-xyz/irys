# Centralized Version Enums

## Status

Accepted

## Context

Version constants were scattered across multiple crates: `DatabaseVersion` lived as a raw `u32` constant in `irys-database`, `ProtocolVersion` was in `irys-types/src/version.rs` alongside P2P handshake types, and `TransactionVersion` was implicit in Compact discriminants. This made it difficult to answer "what changed at version X?" without grep-ing across the workspace, and there was no single place to document the semantic meaning of each version boundary.

The alternative — keeping each version near its consumer — was rejected because version boundaries often span multiple crates (a database version change may require a protocol version bump), and co-locating the enums makes cross-version relationships visible.

## Decision

Create `crates/types/src/versions.rs` as the single file where every version enum lives. Currently this includes:

- **`DatabaseVersion`** (`#[repr(u32)]`) — V0 (original, no stamp), V1 (introduced versioning key), V2 (metadata back-fill). Each variant has a doc comment describing what changed. Provides `from_u32()` for safe conversion from stored values, `CURRENT` constant, `Display`, and `PartialOrd`/`Ord` for comparison.

- **`ProtocolVersion`** (`#[repr(u32)]`) — V1 (original handshake), V2 (peer identity + config hash). Includes `TryFrom<u32>`, RLP `Encodable`/`Decodable`, serde, and `supported_versions()` for negotiation.

- **`TransactionVersion`** (`#[repr(u8)]`) — V1 (original commitment format), V2 (added `UpdateRewardAddress` + RLP signing). Provides `from_u8()`, `CURRENT`, and `Display`.

The file is re-exported from `irys_types::versions` and individual enums are re-exported at the crate root (e.g., `irys_types::DatabaseVersion`). The old `ProtocolVersion` definition in `version.rs` now re-exports from `versions.rs` to avoid breaking existing imports.

Each enum variant's doc comment serves as a changelog entry: it lists the concrete changes at that version boundary so that `versions.rs` alone is sufficient to understand the full version history.

See also: [Database Schema Versioning and Migration](database-schema-versioning-and-migration.md)

## Consequences

- A single `git blame` on `versions.rs` shows when each version was introduced and why
- Adding a new version to any axis requires touching exactly one file, making it easy to review and hard to miss
- Doc comments on variants enforce that version semantics are documented at the point of definition, not scattered in commit messages
- The `#[repr(u32)]`/`#[repr(u8)]` guarantees stable discriminant values for on-disk and on-wire formats
- `ProtocolVersion` is re-exported from the old location (`version.rs`) for backward compatibility, avoiding a workspace-wide import change

## Source
PR #1223 — feat: check database schema version on startup
