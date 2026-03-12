# Sovereign Gossip Wire Types Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Create sovereign copies of all gossip wire-serialized types in the p2p crate, decoupling wire format from `irys-types` internal changes.

**Architecture:** New `wire_types` module in `crates/p2p/src/` containing copies of every struct/enum serialized over gossip HTTP. Each sovereign type has explicit serde attributes (no reliance on IntegerTagged macro) and From/TryFrom conversions to canonical `irys-types` types. Server/client code is updated to serialize/deserialize using wire types at the HTTP boundary.

**Tech Stack:** Rust, serde/serde_json, alloy_primitives (B256 via reth re-export), irys-types primitives (H256, U256, IrysAddress, etc.)

---

## Shared Primitives (NOT copied — imported from irys-types)

These primitives have stable serde and are used as field types in sovereign structs:
- `H256`, `U256`, `IrysAddress`, `IrysSignature`, `IrysPeerId`, `Base64`, `H256List`, `IngressProofsList`
- `TxChunkOffset`, `UnixTimestampMs`, `BoundedFee`, `BlockHash`, `ChunkPathHash`, `IrysTransactionId`, `DataRoot`, `PartitionHash`
- `B256` (from `reth::revm::primitives`)
- `Amount<T>` / `IrysTokenPrice` (from `irys_types::storage_pricing`)
- `ProtocolVersion` (enum with V1=1, V2=2 — simple repr, no custom serde)
- `semver::Version`

## Serde Helpers Needed

The sovereign types need access to these serde helper modules from irys-types:
- `string_u64` — serializes `u64` as `"123"`
- `optional_string_u64` — serializes `Option<u64>` as `"123"` or absent
- `string_usize` — serializes `usize` as `"123"`
- `u64_stringify` — serializes `u64` as `"123"` (used on DataTransactionLedger)
- `option_u64_stringify` — serializes `Option<u64>` as `"123"` (used on VDFLimiterInfo)

These are currently private in `irys-types`. They need to be re-exported or duplicated. Check if they're already public. If not, the simplest path is to make them `pub` in irys-types.

---

### Task 1: Expose serde helper modules from irys-types [DONE]

**Files:**
- Modify: `crates/types/src/serialization.rs` (or wherever `string_u64` etc. are defined)
- Modify: `crates/types/src/lib.rs` — re-export the modules

Serde helper modules were made public so the p2p crate can use them with `#[serde(with = "irys_types::string_u64")]`.

---

### Task 2: Create wire_types module scaffold with leaf types [DONE]

**Files:**
- Created: `crates/p2p/src/wire_types/mod.rs`
- Created: `crates/p2p/src/wire_types/chunk.rs` — `UnpackedChunk`
- Created: `crates/p2p/src/wire_types/ingress_proof.rs` — `IngressProofV1Inner`, `IngressProof`
- Modified: `crates/p2p/src/lib.rs` — added `pub(crate) mod wire_types;`

**Implementation note:** The module uses three helper macros defined in `mod.rs`:
- `impl_mirror_from!` — generates field-by-field From impls for plain structs
- `impl_mirror_enum_from!` — generates field-by-field From impls for enums
- `impl_versioned_tx_from!` — generates From/TryFrom for IntegerTagged versioned types with metadata wrappers
- `impl_json_version_tagged_serde!` — generates hand-written Serialize/Deserialize for flattened `{"version": N, ...fields}` JSON format

---

### Task 3: Add commitment wire types [DONE]

**Files:**
- Created: `crates/p2p/src/wire_types/commitment_transaction.rs`

Types: `CommitmentTypeV1`, `CommitmentTypeV2`, `CommitmentTransactionV1Inner`, `CommitmentTransactionV2Inner`, `CommitmentTransaction`

Uses `impl_json_version_tagged_serde!` for the versioned enum and `impl_mirror_enum_from!` for the commitment type enums.

---

### Task 4: Add transaction wire types [DONE]

**Files:**
- Created: `crates/p2p/src/wire_types/data_transaction.rs`

Types: `DataTransactionHeaderV1Inner`, `DataTransactionHeader`, `IrysTransactionResponse`

`IrysTransactionResponse` was added beyond the original plan — it's a `#[serde(tag = "type")]` enum with `DataTransaction` and `CommitmentTransaction` variants, used in API responses.

---

### Task 5: Add block wire types [DONE]

**Files:**
- Created: `crates/p2p/src/wire_types/block.rs`

Types: `PoaData`, `VDFLimiterInfo`, `DataTransactionLedger`, `SystemTransactionLedger`, `IrysBlockHeaderV1Inner`, `IrysBlockHeader`, `BlockBody`

---

### Task 6: Add gossip envelope wire types [DONE]

**Files:**
- Created: `crates/p2p/src/wire_types/gossip_data.rs`

Types: `GossipDataV1`, `GossipDataV2`, `GossipDataRequestV1`, `GossipDataRequestV2`, `GossipRequestV1<T>`, `GossipRequestV2<T>`

---

### Task 7: Add handshake, response, and node info wire types [DONE]

**Files:**
- Created: `crates/p2p/src/wire_types/handshake.rs` — `RethPeerInfo`, `PeerAddress`, `HandshakeRequestV1/V2`, `HandshakeResponseV1/V2`
- Created: `crates/p2p/src/wire_types/node_info.rs` — `NodeInfoV1`, `NodeInfoV2`
- `GossipResponse`, `RejectionReason`, `HandshakeRequirementReason` are re-exported from `crate::types` (not copied — already owned by the p2p crate)

**Implementation note:** `PeerAddress` was moved out of `crate::types` into `wire_types::handshake` and re-exported for broader use. `NodeInfoV2` differs from `NodeInfoV1` in that `peer_count` uses `string_u64` serialization (stringified number) vs bare integer.

---

### Task 8: Add block index wire types [DONE — NOT IN ORIGINAL PLAN]

**Files:**
- Created: `crates/p2p/src/wire_types/block_index.rs`

Types: `BlockIndexItemV1`, `BlockIndexItemV2`, `LedgerIndexItem`, `BlockIndexItemV2ConversionError`

This was not in the original plan but was needed for the `/gossip/block_index` endpoint. `BlockIndexItemV2` includes validation that rejects oversized ledger lists (`num_ledgers` must match actual ledger data). Conversion from canonical `BlockIndexItem` handles V1 (flattened single-ledger) and V2 (multi-ledger) formats.

---

### Task 9: Write fixture parity tests [DONE]

**Files:**
- Created: `crates/p2p/src/wire_types/tests.rs` — 24 roundtrip tests
- Created: `crates/p2p/src/wire_types/test_helpers.rs` — fixture builder functions

Tests cover:
- All leaf types (chunk, ingress, commitment V1/V2, data transaction, block header, block body)
- Edge cases (none optional fields, empty ledgers, unknown versions)
- Handshake V1/V2 request/response roundtrips
- GossipData V1/V2 envelope roundtrips
- NodeInfo V1/V2 roundtrips
- BlockIndexItem V1/V2 roundtrips with validation
- IrysTransactionResponse roundtrips

---

### Task 10: Update gossip_fixture_tests.rs to use wire types [DONE]

**Files:**
- Modified: `crates/p2p/src/gossip_fixture_tests.rs` (903 lines)

The fixture tests now use wire types exclusively. Key features:
- `fixture_tests!` macro generates tests from external JSON fixture file (`gossip_fixtures.json`)
- `assert_fixture_coverage!` macro enforces that all enum variants have fixture coverage
- `all_wire_types_have_fixture_coverage()` test uses `syn` to parse the wire_types module and verify every public type has fixture test coverage (excludes inner types like `V1Inner` that are tested via their wrapper enums)
- 70+ fixture entries covering all gossip protocol message types
- Exhaustive variant coverage for all enums: `GossipDataV1/V2`, `GossipDataRequestV1/V2`, `CommitmentTypeV1/V2`, `GossipResponse`, `RejectionReason`, `IrysTransactionResponse`

---

### Task 11: Wire up server.rs deserialization boundary [DONE]

**Files:**
- Modified: `crates/p2p/src/server.rs`

All HTTP handlers now deserialize into wire types and convert to canonical types before internal processing:
- 14 V1/V2 data handlers (chunk, block_header, block_body, execution_payload, transaction, commitment_tx, ingress_proof)
- 4 data request/pull handlers
- 2 handshake handlers (V1/V2)
- 2 node info handlers (V1/V2)
- 2 block index handlers (V1/V2)

Response serialization also uses wire types — `GossipResponse<wire::GossipDataV1/V2>` for data responses, `wire::NodeInfoV1/V2` for info, `wire::HandshakeResponseV1/V2` for handshakes.

---

### Task 12: Wire up gossip_client.rs serialization boundary [DONE]

**Files:**
- Modified: `crates/p2p/src/gossip_client.rs`

All outgoing gossip messages convert canonical → wire before JSON serialization:
- `send_data_*()` methods convert each `GossipDataV2` variant to its wire type
- `create_request_v1/v2()` wraps data in `wire::GossipRequestV1/V2`
- `handshake_v1/v2()` sends `wire::HandshakeRequestV1/V2`, deserializes `GossipResponse<wire::HandshakeResponseV1/V2>`
- `get_block_index()` receives `Vec<wire::BlockIndexItemV1>`
- `request_data()` / `pull_data()` receive `GossipResponse<Option<wire::GossipDataV1/V2>>`

---

### Task 13: Wire up gossip_data_handler.rs and chain_sync.rs [DONE — NO CHANGES NEEDED]

As anticipated in the original plan, these files did not need changes. They work with canonical types internally — wire type conversion happens entirely at the HTTP boundary in `server.rs` and `gossip_client.rs`.

---

### Task 14: Additional work beyond original plan [DONE]

Several improvements were made beyond the original 13-task plan:

1. **Removed hazardous `PartialEq` impls** — `PartialEq` was removed from types where field-level comparison would be misleading (e.g., block headers where equality should be by hash). This prevents accidental semantic bugs.

2. **Removed `usize` from wire protocol** — `usize` is platform-dependent (32-bit vs 64-bit), so wire types use `u64` or `u32` instead.

3. **Removed gossip↔irys JSON conversion invariant** — The old code had an implicit invariant that canonical types and wire types must produce identical JSON. This was removed since the wire types now own the format independently.

4. **Added `BlockIndexItemV2` with validation** — Multi-ledger block index support with `num_ledgers` consistency checks.

5. **Added fixture coverage enforcement via `syn`** — Compile-time check that every public wire type has fixture test coverage.

6. **Added `IrysTransactionResponse` wire type** — For the `/tx/:id` API response format.

7. **Moved `PeerAddress` into wire_types** — It's a wire protocol type that was previously in `crate::types`.

---

### Task 15: Run full test suite and clean up [DONE]

All tests pass, clippy is clean, formatting is applied.

---

## Summary of Files Created/Modified

**Created (12 files):**
- `crates/p2p/src/wire_types/mod.rs` — module root with macros and re-exports (338 lines)
- `crates/p2p/src/wire_types/block.rs` — block types (161 lines)
- `crates/p2p/src/wire_types/block_index.rs` — block index types (91 lines)
- `crates/p2p/src/wire_types/chunk.rs` — chunk type (24 lines)
- `crates/p2p/src/wire_types/commitment_transaction.rs` — commitment types (124 lines)
- `crates/p2p/src/wire_types/data_transaction.rs` — data transaction types (75 lines)
- `crates/p2p/src/wire_types/gossip_data.rs` — gossip envelope types (133 lines)
- `crates/p2p/src/wire_types/handshake.rs` — handshake types (91 lines)
- `crates/p2p/src/wire_types/ingress_proof.rs` — ingress proof types (31 lines)
- `crates/p2p/src/wire_types/node_info.rs` — node info types (74 lines)
- `crates/p2p/src/wire_types/test_helpers.rs` — test fixture builders
- `crates/p2p/src/wire_types/tests.rs` — roundtrip tests (459 lines)

**Modified (4+ files):**
- `crates/types/src/` — exposed serde helper modules
- `crates/p2p/src/lib.rs` — added wire_types module
- `crates/p2p/src/server.rs` — wire type deserialization/serialization boundary
- `crates/p2p/src/gossip_client.rs` — wire type serialization/deserialization boundary
- `crates/p2p/src/gossip_fixture_tests.rs` — fixture tests using wire types with coverage enforcement

## Status: ALL TASKS COMPLETE
