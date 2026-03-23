# PD Precompile ABI Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the custom 1-byte PD precompile dispatch with standard ABI selectors and unify the two-type access list encoding into a single `PdDataRead` specifier.

**Architecture:** Two phases executed sequentially. Phase 1 replaces `ChunkRangeSpecifier` + `ByteRangeSpecifier` with a unified `PdDataRead` type in `irys-types`, then updates all consumers (pd_tx, mempool, payload, evm, block_validation, pd_service, precompile). Phase 2 replaces the 1-byte function ID dispatch with 4-byte ABI selectors via `alloy::sol!`, adds ABI-encoded returns and custom error reverts, and ships new Solidity contracts (`IIrysPD.sol`, `IrysPDLib.sol`).

**Tech Stack:** Rust (alloy-sol-types for ABI codegen), Solidity ^0.8.20, revm PrecompileOutput::new_reverted for custom errors.

**Spec:** `docs/superpowers/specs/2026-03-17-pd-precompile-abi-redesign.md`

---

## File Map

### Phase 1: Unified Access List Encoding

| Action | File | Responsibility |
|--------|------|----------------|
| Rewrite | `crates/types/src/range_specifier.rs` | Replace `ChunkRangeSpecifier`, `ByteRangeSpecifier`, `PdAccessListArg`, `PdAccessListArgSerde`, `U34`, `U18` with `PdDataRead`. Keep fee encoding (`encode_pd_fee`, `decode_pd_fee`, `PdFeeDecodeError`, `MAX_PD_FEE`). |
| Modify | `crates/types/src/chunk_provider.rs:94-110` | Change `PdChunkMessage` variants from `Vec<ChunkRangeSpecifier>` to `Vec<PdDataRead>`. |
| Rewrite | `crates/irys-reth/src/pd_tx.rs` | Update `PdTransactionMeta` (single `Vec<PdDataRead>` replaces separate chunk/byte vecs), `parse_pd_transaction`, `build_pd_access_list`, `build_pd_access_list_with_fees`, `sum_pd_chunks_in_access_list`, `extract_pd_chunk_specs`. |
| Modify | `crates/irys-reth/src/mempool.rs:506-524` | Update `pd_transaction_monitor` to send `PdDataRead` specs via `PdChunkMessage`. |
| Modify | `crates/irys-reth/src/payload.rs:483-500` | Update `pd_chunks_for_transaction` to use `PdTransactionMeta.total_chunks` (already does — just ensure `PdTransactionMeta` compiles). |
| Modify | `crates/irys-reth/src/evm.rs:652-785` | Update PD fee deduction to use new `PdTransactionMeta` fields. |
| Modify | `crates/actors/src/block_validation.rs:1448-1480` | Update PD chunk validation to use `PdDataRead`. |
| Modify | `crates/actors/src/pd_service.rs` | Update `specs_to_keys` and chunk provisioning to accept `Vec<PdDataRead>` instead of `Vec<ChunkRangeSpecifier>`. |
| Rewrite | `crates/irys-reth/src/precompiles/pd/utils.rs` | Replace `parse_access_list` → `parse_pd_specifiers`. Returns `Vec<PdDataRead>`. |
| Modify | `crates/irys-reth/src/precompiles/pd/read_bytes.rs` | Rewrite to work with `PdDataRead` instead of cross-referencing `ByteRangeSpecifier` → `ChunkRangeSpecifier`. |
| Modify | `crates/irys-reth/src/precompiles/pd/precompile.rs` | Update to use `parse_pd_specifiers` and `Vec<PdDataRead>`. |

### Phase 2: Precompile ABI Redesign

| Action | File | Responsibility |
|--------|------|----------------|
| Rewrite | `crates/irys-reth/src/precompiles/pd/functions.rs` | Replace `PdFunctionId` enum with `sol!` macro block generating selectors + types. |
| Rewrite | `crates/irys-reth/src/precompiles/pd/precompile.rs` | 4-byte selector dispatch, ABI-encoded returns. |
| Rewrite | `crates/irys-reth/src/precompiles/pd/read_bytes.rs` | Simplify: `readData` uses specifier's byte_off/len, `readBytes` uses calldata offset/length. |
| Modify | `crates/irys-reth/src/precompiles/pd/error.rs` | Simplify variants, add `try_encode_as_revert()`. |
| Modify | `crates/irys-reth/src/precompiles/pd/constants.rs` | Remove raw encoding offset constants. Keep `PD_BASE_GAS_COST`. |
| Modify | `crates/irys-reth/Cargo.toml` | Add `alloy-sol-types.workspace = true`. |
| Create | `fixtures/contracts/src/IIrysPD.sol` | Interface + address constants. |
| Create | `fixtures/contracts/src/IrysPDLib.sol` | Convenience library with try-variants. |
| Rewrite | `fixtures/contracts/src/IrysProgrammableDataBasic.sol` | Use interface/library instead of inheritance. |
| Delete | `fixtures/contracts/src/Precompiles.sol` | Replaced by `IIrysPD.sol`. |
| Delete | `fixtures/contracts/src/ProgrammableData.sol` | Replaced by `IIrysPD.sol` + `IrysPDLib.sol`. |

### Tests (both phases)

| Action | File |
|--------|------|
| Rewrite | `crates/types/src/range_specifier.rs` (module tests) |
| Rewrite | `crates/irys-reth/src/pd_tx.rs` (module tests) |
| Rewrite | `crates/irys-reth/src/precompiles/pd/precompile.rs` (module tests) |
| Rewrite | `crates/irys-reth/src/precompiles/pd/read_bytes.rs` (module tests) |
| Rewrite | `crates/irys-reth/tests/pd_context_isolation.rs` |
| Update | `crates/irys-reth/src/lib.rs` (`chunk_spec_with_params` test helper — replace with `PdDataRead` helper) |
| Update | `crates/chain-tests/src/programmable_data/basic.rs` |
| Update | `crates/chain-tests/src/programmable_data/pd_chunk_limit.rs` |
| Update | `crates/chain-tests/src/programmable_data/pd_chunk_p2p_pull.rs` (inline `ChunkRangeSpecifier` construction) |
| Update | `crates/chain-tests/src/programmable_data/pd_content_verification.rs` (inline `ChunkRangeSpecifier` + `ByteRangeSpecifier` construction) |
| Update | `crates/chain-tests/src/programmable_data/preloaded_chunk_table.rs` (heavy usage of old types at lines 313, 318, 415, 420, 731, 736, 826, 833) |
| Update | `crates/chain-tests/src/programmable_data/hardfork_guard.rs` |
| Update | `crates/chain-tests/src/programmable_data/min_transaction_cost.rs` |
| Update | `crates/chain-tests/src/programmable_data/pd_mock_e2e.rs` (comment referencing old type name) |
| Update | `crates/chain-tests/src/external/programmable_data_basic.rs` |
| Update | `crates/chain-tests/src/utils.rs` (PD helpers) |

---

## Phase 1: Unified Access List Encoding (`PdDataRead`)

### Task 1: Implement `PdDataRead` in `irys-types`

**Files:**
- Rewrite: `crates/types/src/range_specifier.rs`

This is the foundation. Replace `ChunkRangeSpecifier`, `ByteRangeSpecifier`, `PdAccessListArg`, `PdAccessListArgSerde` trait, `U34`, `U18`, and `PdArgsEncWrapper` with a single `PdDataRead` struct. Keep fee encoding untouched.

- [ ] **Step 1: Read the current `range_specifier.rs` to understand all public exports**

  Identify every type, trait, and function exported from this module. Check `crates/types/src/lib.rs` for re-exports.

  Run: `cargo check -p irys-types 2>&1 | head -5` to confirm current state compiles.

- [ ] **Step 2: Write `PdDataRead` struct with `decode`, `encode`, `chunks_needed`**

  Replace the old specifier types. The new struct lives in the same file (`range_specifier.rs`).

  ```rust
  use irys_types::chunk_provider::ChunkConfig;

  /// Unified PD access list specifier. Encodes a data read from a partition's chunk range.
  /// Binary layout (32 bytes, all big-endian):
  ///   [0]     = 0x01 (type discriminant)
  ///   [1..9]  = partition_index (u64)
  ///   [9..13] = start (u32) — first chunk offset within partition
  ///   [13..17] = len (u32) — number of bytes to read
  ///   [17..20] = byte_off (u24) — byte offset within first chunk
  ///   [20..32] = reserved (must be zero)
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
  pub struct PdDataRead {
      pub partition_index: u64,
      pub start: u32,
      pub len: u32,
      pub byte_off: u32, // u24 stored as u32, top byte always 0
  }
  ```

  Implement:
  - `PdDataRead::decode(bytes: &[u8; 32], chunk_config: &ChunkConfig) -> eyre::Result<Self>` — canonical decoder with all validity rules from spec section 6.2.
  - `PdDataRead::encode(&self) -> [u8; 32]` — canonical encoder.
  - `PdDataRead::chunks_needed(&self, chunk_size: u64) -> u64` — `ceil((byte_off + len) / chunk_size)`.

  See spec section 6.7 for exact implementation.

- [ ] **Step 3: Update `PdAccessListArgsTypeId` enum**

  Remove `ChunkRead = 0` and `ByteRead = 1`. Add `DataRead = 1` (matching the `0x01` type byte). Keep `PdPriorityFee = 2` and `PdBaseFeeCap = 3`.

  Note: the type byte for `PdDataRead` is `0x01`, not `0x00`. This is intentional — `B256::ZERO` (all zeros) must fail validation because type byte 0 != 1.

  ```rust
  #[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
  #[repr(u8)]
  pub enum PdAccessListArgsTypeId {
      DataRead = 1,
      PdPriorityFee = 2,
      PdBaseFeeCap = 3,
  }
  ```

- [ ] **Step 4: Remove old types and traits**

  Delete: `ChunkRangeSpecifier`, `ByteRangeSpecifier`, `PdAccessListArg`, `PdAccessListArgSerde` trait, `PdArgsEncWrapper`, `U34`, `U18`. These are fully replaced by `PdDataRead` + direct fee encoding.

  Keep: `PdAccessListArgsTypeId` (updated), `encode_pd_fee`, `decode_pd_fee`, `PdFeeDecodeError`, `MAX_PD_FEE`.

- [ ] **Step 5: Write unit tests for `PdDataRead`**

  Test encode/decode roundtrip, validity rules, edge cases:

  ```rust
  #[cfg(test)]
  mod tests {
      use super::*;

      fn test_chunk_config() -> ChunkConfig {
          // ChunkConfig has 4 fields — use from_consensus or construct fully:
          ChunkConfig {
              chunk_size: 262_144, // 256 KB
              num_chunks_in_partition: 51_872_000,
              entropy_packing_iterations: 0,
              chain_id: 1,
          }
          // Alternatively: ChunkConfig::from_consensus(&ConsensusConfig::testing())
      }

      #[test]
      fn roundtrip_basic() {
          let spec = PdDataRead { partition_index: 42, start: 100, len: 1000, byte_off: 500 };
          let encoded = spec.encode();
          let decoded = PdDataRead::decode(&encoded, &test_chunk_config()).unwrap();
          assert_eq!(spec, decoded);
      }

      #[test]
      fn zero_type_byte_rejected() {
          let bytes = [0u8; 32]; // type byte 0x00
          assert!(PdDataRead::decode(&bytes, &test_chunk_config()).is_err());
      }

      #[test]
      fn zero_len_rejected() {
          let mut spec = PdDataRead { partition_index: 0, start: 0, len: 0, byte_off: 0 };
          let mut encoded = [0u8; 32];
          encoded[0] = 0x01; // valid type byte but len=0
          assert!(PdDataRead::decode(&encoded, &test_chunk_config()).is_err());
      }

      #[test]
      fn byte_off_exceeds_chunk_size_rejected() {
          let spec = PdDataRead { partition_index: 0, start: 0, len: 100, byte_off: 262_144 };
          let encoded = spec.encode();
          assert!(PdDataRead::decode(&encoded, &test_chunk_config()).is_err());
      }

      #[test]
      fn nonzero_reserved_bytes_rejected() {
          let spec = PdDataRead { partition_index: 0, start: 0, len: 100, byte_off: 0 };
          let mut encoded = spec.encode();
          encoded[25] = 0xFF; // taint reserved byte
          assert!(PdDataRead::decode(&encoded, &test_chunk_config()).is_err());
      }

      #[test]
      fn partition_boundary_overflow_rejected() {
          let config = test_chunk_config();
          // start + chunks_needed would exceed num_chunks_in_partition
          let spec = PdDataRead {
              partition_index: 0,
              start: config.num_chunks_in_partition as u32, // at boundary
              len: 100,
              byte_off: 0,
          };
          let encoded = spec.encode();
          assert!(PdDataRead::decode(&encoded, &config).is_err());
      }

      #[test]
      fn chunks_needed_calculation() {
          let chunk_size = 262_144u64;
          // Exactly one chunk
          let spec = PdDataRead { partition_index: 0, start: 0, len: 100, byte_off: 0 };
          assert_eq!(spec.chunks_needed(chunk_size), 1);

          // Spans two chunks due to byte_off
          let spec = PdDataRead { partition_index: 0, start: 0, len: 100, byte_off: 262_100 };
          assert_eq!(spec.chunks_needed(chunk_size), 2);
      }

      #[test]
      fn big_endian_encoding() {
          let spec = PdDataRead { partition_index: 0x0102030405060708, start: 0x090A0B0C, len: 0x0D0E0F10, byte_off: 0x111213 };
          let encoded = spec.encode();
          assert_eq!(encoded[0], 0x01); // type byte
          assert_eq!(&encoded[1..9], &[0x01,0x02,0x03,0x04,0x05,0x06,0x07,0x08]);
          assert_eq!(&encoded[9..13], &[0x09,0x0A,0x0B,0x0C]);
          assert_eq!(&encoded[13..17], &[0x0D,0x0E,0x0F,0x10]);
          assert_eq!(&encoded[17..20], &[0x11,0x12,0x13]);
          assert_eq!(&encoded[20..32], &[0u8; 12]); // reserved
      }
  }
  ```

- [ ] **Step 6: Verify `irys-types` compiles**

  Run: `cargo check -p irys-types 2>&1 | head -20`

  Expected: compilation errors in downstream crates (they still reference old types), but `irys-types` itself should compile and tests pass.

- [ ] **Step 7: Run `PdDataRead` unit tests**

  Run: `cargo nextest run -p irys-types range_specifier`

  Expected: All new tests pass. Old tests are gone (replaced).

---

### Task 2: Update `PdChunkMessage` in `chunk_provider.rs`

**Files:**
- Modify: `crates/types/src/chunk_provider.rs:94-110`

- [ ] **Step 1: Change `ChunkRangeSpecifier` → `PdDataRead` in `PdChunkMessage`**

  Update the import and both variants that carry chunk specs:

  ```rust
  use crate::range_specifier::PdDataRead;

  pub enum PdChunkMessage {
      NewTransaction {
          tx_hash: B256,
          chunk_specs: Vec<PdDataRead>,
      },
      TransactionRemoved { tx_hash: B256 },
      ProvisionBlockChunks {
          block_hash: B256,
          chunk_specs: Vec<PdDataRead>,
          response: oneshot::Sender<Result<(), Vec<(u32, u64)>>>,
      },
      ReleaseBlockChunks { block_hash: B256 },
  }
  ```

- [ ] **Step 2: Verify `irys-types` still compiles**

  Run: `cargo check -p irys-types 2>&1 | head -10`

  Expected: `irys-types` compiles. Downstream crates will have errors.

---

### Task 3: Update `pd_tx.rs` — Access List Parsing & Building

**Files:**
- Rewrite: `crates/irys-reth/src/pd_tx.rs`

This is the canonical PD transaction parser. It needs to decode `PdDataRead` keys (type byte `0x01`) instead of separate `ChunkRead`/`ByteRead` keys.

- [ ] **Step 1: Update `PdTransactionMeta`**

  Replace separate `data_reads`/`byte_reads` with a single vector:

  ```rust
  pub struct PdTransactionMeta {
      pub data_reads: Vec<PdDataRead>,
      pub priority_fee_per_chunk: U256,
      pub base_fee_cap_per_chunk: U256,
      pub total_chunks: u64,
  }
  ```

  `total_chunks` is now computed as: `data_reads.iter().map(|r| r.chunks_needed(chunk_size)).sum()`.

- [ ] **Step 2: Update `parse_pd_transaction` to decode `PdDataRead` keys**

  The function signature changes to accept a `ChunkConfig` parameter (needed for `PdDataRead::decode`):

  ```rust
  pub fn parse_pd_transaction(access_list: &AccessList, chunk_config: &ChunkConfig) -> PdParseResult
  ```

  Key changes:
  - Match type byte `0x01` → decode as `PdDataRead` (replaces both `ChunkRead` and `ByteRead` arms).
  - Remove `ByteRead` handling entirely.
  - Compute `total_chunks` by summing `chunks_needed(chunk_config.chunk_size)` across all `PdDataRead` entries.
  - Update `PdValidationError` variants: remove `DuplicateByteReadKey`, rename `DuplicateChunkReadKey` → `DuplicateDataReadKey`, remove `UnknownTypeByte` for byte 0/1 distinction.

- [ ] **Step 3: Update `build_pd_access_list` and `build_pd_access_list_with_fees`**

  ```rust
  pub fn build_pd_access_list(specs: &[PdDataRead]) -> AccessList {
      let keys: Vec<B256> = specs.iter().map(|s| B256::from(s.encode())).collect();
      AccessList(vec![AccessListItem {
          address: PD_PRECOMPILE_ADDRESS,
          storage_keys: keys,
      }])
  }

  pub fn build_pd_access_list_with_fees(
      data_reads: &[PdDataRead],
      priority_fee: U256,
      base_fee_cap: U256,
  ) -> eyre::Result<AccessList> {
      let mut keys: Vec<B256> = data_reads.iter().map(|s| B256::from(s.encode())).collect();
      keys.push(B256::from(encode_pd_fee(PdAccessListArgsTypeId::PdPriorityFee as u8, priority_fee)?));
      keys.push(B256::from(encode_pd_fee(PdAccessListArgsTypeId::PdBaseFeeCap as u8, base_fee_cap)?));
      Ok(AccessList(vec![AccessListItem {
          address: PD_PRECOMPILE_ADDRESS,
          storage_keys: keys,
      }]))
  }
  ```

  Note: `build_pd_access_list_with_fees` no longer takes a separate `byte_specs` parameter.

- [ ] **Step 4: Update `sum_pd_chunks_in_access_list` and `extract_pd_chunk_specs`**

  Both functions need to decode `PdDataRead` from keys with type byte `0x01`:

  ```rust
  pub fn sum_pd_chunks_in_access_list(access_list: &AccessList, chunk_config: &ChunkConfig) -> u64 {
      // ... filter to PD_PRECOMPILE_ADDRESS, decode each key,
      // sum chunks_needed() for DataRead keys, skip fee keys
  }

  pub fn extract_pd_chunk_specs(access_list: &AccessList, chunk_config: &ChunkConfig) -> Vec<PdDataRead> {
      // ... filter to PD_PRECOMPILE_ADDRESS, decode each key,
      // collect DataRead keys, skip fee keys
  }
  ```

- [ ] **Step 5: Update `PdValidationError` enum**

  Remove variants for the old two-type system, simplify:

  ```rust
  pub enum PdValidationError {
      MultiplePdItems,
      NoDataReads,
      MissingPriorityFee,
      MissingBaseFeeCap,
      DuplicateDataReadKey,
      DuplicatePriorityFee,
      DuplicateBaseFeeCap,
      InvalidDataReadEncoding { reason: String },
      InvalidFeeEncoding { key_type: &'static str, reason: String },
      UnknownTypeByte(u8),
  }
  ```

- [ ] **Step 6: Rewrite `pd_tx.rs` unit tests**

  All existing tests construct `ChunkRangeSpecifier` + `ByteRangeSpecifier`. Rewrite them to use `PdDataRead`. Test the same scenarios: valid parse, missing fees, duplicate keys, invalid encoding, etc.

  Important: `parse_pd_transaction` now takes `&ChunkConfig`, so all test calls need a test config. Use `ChunkConfig::from_consensus(&ConsensusConfig::testing())` or construct fully with all 4 fields (`chunk_size`, `num_chunks_in_partition`, `entropy_packing_iterations`, `chain_id`).

  Also update or replace the `chunk_spec_with_params` helper in `crates/irys-reth/src/lib.rs:3427` — it creates old `ChunkRangeSpecifier` values and is used by `pd_tx.rs` tests. Replace with a `data_read_with_params` helper returning `PdDataRead`.

- [ ] **Step 7: Verify `irys-reth` compiles (expect downstream errors)**

  Run: `cargo check -p irys-reth 2>&1 | head -40`

  Expected: `pd_tx.rs` compiles. Other files in `irys-reth` that call `parse_pd_transaction` without the new `ChunkConfig` param will fail — that's expected, we fix them in the next tasks.

---

### Task 4: Update Error Types (must precede precompile changes)

**Files:**
- Modify: `crates/irys-reth/src/precompiles/pd/error.rs`

Error types must be updated before the precompile internals, since Task 5 introduces usage of `SpecifierNotFound` which doesn't exist yet.

- [ ] **Step 1: Simplify error variants for the unified specifier model**

  Remove variants that referenced the old two-type system:
  - Remove `ByteRangeNotFound` (replaced by `SpecifierNotFound`)
  - Remove `ChunkRangeNotFound` (replaced by `SpecifierNotFound`)
  - Remove `OffsetTranslationFailed` (no more `translate_offset`)
  - Remove `InvalidLength` (length is now a simple u32)
  - Remove `LengthOutOfRange` (u32 always fits in usize on 64-bit)

  Add:
  ```rust
  #[error("specifier index {index} not found (available: {available})")]
  SpecifierNotFound { index: u8, available: usize },
  ```

  Keep: `InsufficientInput`, `MissingAccessList`, `InvalidCalldataLength`, `InvalidAccessList`, `GasOverflow`, `PartitionIndexTooLarge`, `ChunkNotFound`, `ChunkFetchFailed`, `ByteRangeOutOfBounds`, `OffsetOutOfRange`.

- [ ] **Step 2: Verify compilation**

  Run: `cargo check -p irys-reth 2>&1 | head -10`

---

### Task 5: Update Precompile Internals (`utils.rs`, `read_bytes.rs`, `precompile.rs`)

**Files:**
- Rewrite: `crates/irys-reth/src/precompiles/pd/utils.rs`
- Rewrite: `crates/irys-reth/src/precompiles/pd/read_bytes.rs`
- Modify: `crates/irys-reth/src/precompiles/pd/precompile.rs`

These files form the precompile's internal pipeline. In Phase 1 we update the data model only (still using 1-byte function IDs — Phase 2 changes dispatch).

- [ ] **Step 1: Rewrite `utils.rs` — `parse_pd_specifiers`**

  Replace `parse_access_list` → `parse_pd_specifiers`. Returns `Vec<PdDataRead>` instead of `ParsedAccessLists { chunk_reads, byte_reads }`:

  ```rust
  use irys_types::range_specifier::PdDataRead;
  use irys_types::chunk_provider::ChunkConfig;

  /// Parse all PdDataRead specifiers from the access list.
  /// Rejects (not skips) malformed keys — any invalid key is an error.
  pub fn parse_pd_specifiers(
      access_list: &[AccessListItem],
      chunk_config: &ChunkConfig,
  ) -> eyre::Result<Vec<PdDataRead>> {
      let mut specifiers = Vec::new();
      for ali in access_list {
          if ali.address != PD_PRECOMPILE_ADDRESS {
              continue;
          }
          for key in &ali.storage_keys {
              let type_byte = key[0];
              match type_byte {
                  0x01 => {
                      let spec = PdDataRead::decode(key.as_ref().try_into()?, chunk_config)?;
                      specifiers.push(spec);
                  }
                  // Fee keys (0x02, 0x03) — skip, handled by parse_pd_transaction
                  0x02 | 0x03 => {}
                  _ => eyre::bail!("unknown access list key type byte: {:#04x}", type_byte),
              }
          }
      }
      Ok(specifiers)
  }
  ```

  Delete `ParsedAccessLists` struct.

- [ ] **Step 2: Rewrite `read_bytes.rs` to work with `PdDataRead`**

  The core change: `PdDataRead` is self-contained — no cross-reference from byte range to chunk range. Each specifier has its own `partition_index`, `start`, `len`, `byte_off`.

  Remove: `ReadBytesRangeByIndexArgs`, `ReadPartialByteRangeArgs`, `calculate_byte_offset`, the old `read_bytes_range` that took a `ByteRangeSpecifier`.

  New core function:

  ```rust
  /// Read chunk data for a PdDataRead specifier and extract the requested byte range.
  ///
  /// - `offset`: byte offset within concatenated chunk data (0 = first byte of first chunk)
  /// - `length`: number of bytes to read
  pub fn read_chunk_data(
      spec: &PdDataRead,
      offset: u32,   // byte_off for readData, calldata offset for readBytes
      length: u32,    // len for readData, calldata length for readBytes
      context: &PdContext,
  ) -> Result<Bytes, PdPrecompileError> {
      let config = context.config();
      let chunk_size = config.chunk_size;
      let chunks_needed = spec.chunks_needed(chunk_size);

      // Calculate ledger offsets
      let base_offset = (spec.partition_index as u64)
          .checked_mul(config.num_chunks_in_partition)
          .ok_or(PdPrecompileError::PartitionIndexTooLarge { index: spec.partition_index.to_string() })?;
      let ledger_start = base_offset
          .checked_add(spec.start as u64)
          .ok_or(PdPrecompileError::OffsetOutOfRange { .. })?;

      // Fetch all chunks
      let mut bytes = Vec::with_capacity((chunks_needed * chunk_size) as usize);
      for i in 0..chunks_needed {
          let ledger_offset = ledger_start + i;
          let chunk = context.get_chunk(0, ledger_offset)
              .map_err(|e| PdPrecompileError::ChunkFetchFailed { offset: ledger_offset, reason: e.to_string() })?
              .ok_or(PdPrecompileError::ChunkNotFound { offset: ledger_offset })?;
          bytes.extend(&*chunk);
      }

      // Validate length > 0 (spec requirement for readBytes)
      if length == 0 {
          return Err(PdPrecompileError::ByteRangeOutOfBounds { start: offset as usize, end: offset as usize, available: bytes.len() });
      }

      // Extract byte range
      let start = offset as usize;
      let end = start.checked_add(length as usize)
          .ok_or(PdPrecompileError::ByteRangeOutOfBounds { start, end: usize::MAX, available: bytes.len() })?;
      if end > bytes.len() {
          return Err(PdPrecompileError::ByteRangeOutOfBounds { start, end, available: bytes.len() });
      }

      Ok(Bytes::copy_from_slice(&bytes[start..end]))
  }
  ```

- [ ] **Step 3: Update `precompile.rs` dispatch to use `parse_pd_specifiers`**

  Replace:
  ```rust
  let parsed = parse_access_list(&access_list)?;
  ```
  With:
  ```rust
  let specifiers = parse_pd_specifiers(&access_list, &pd_context.chunk_config())?;
  ```

  Update the two dispatch arms to look up `specifiers[index]` and call `read_chunk_data`:

  For `ReadFullByteRange` (function ID 0):
  ```rust
  let index = data[1] as usize;
  let spec = specifiers.get(index).ok_or(PdPrecompileError::SpecifierNotFound { .. })?;
  let result = read_chunk_data(spec, spec.byte_off, spec.len, &pd_context)?;
  ```

  For `ReadPartialByteRange` (function ID 1):
  ```rust
  let index = data[1] as usize;
  let offset = u32::from_be_bytes(data[2..6].try_into()?);
  let length = u32::from_be_bytes(data[6..10].try_into()?);
  let spec = specifiers.get(index).ok_or(PdPrecompileError::SpecifierNotFound { .. })?;
  let result = read_chunk_data(spec, offset, length, &pd_context)?;
  ```

  Note: we still use 1-byte function IDs in Phase 1. Phase 2 changes this to 4-byte ABI selectors.

- [ ] **Step 4: Verify precompile crate compiles**

  Run: `cargo check -p irys-reth 2>&1 | head -40`

  Fix any remaining type mismatches in the precompile module.

---

### Task 6: Update EVM Layer and Mempool

**Files:**
- Modify: `crates/irys-reth/src/evm.rs:652-785`
- Modify: `crates/irys-reth/src/mempool.rs:263-524`
- Modify: `crates/irys-reth/src/payload.rs:483-500`

All three files call `parse_pd_transaction` which now requires `&ChunkConfig`.

- [ ] **Step 1: Thread `ChunkConfig` into `parse_pd_transaction` call sites in `evm.rs`**

  The `IrysEvm` struct already holds `PdContext` which has `chunk_config()`. Use it:

  ```rust
  // In transact_raw, where parse_pd_transaction is called:
  let chunk_config = self.pd_context.chunk_config();
  match parse_pd_transaction(&tx.access_list, &chunk_config) { ... }
  ```

  Update the `ValidPd(pd_meta)` branch: `pd_meta.total_chunks` is still available and used the same way. The only change is `pd_meta` no longer has `byte_reads` — all reads are in `data_reads`.

- [ ] **Step 2: Thread `ChunkConfig` into `mempool.rs` call sites**

  `IrysShadowTxValidator::prefilter_tx` and `pd_transaction_monitor` both call `parse_pd_transaction`. They need access to `ChunkConfig`.

  The `IrysPoolBuilder` already constructs the pool — find where `ChunkConfig` is available (from `ConsensusConfig` or passed in) and thread it through to validators and the monitor task. The `PdAwareCoinbaseTipOrdering::priority()` method also calls `parse_pd_transaction` (line 433).

  Options:
  - Add `chunk_config: ChunkConfig` field to `IrysShadowTxValidator` and `PdAwareCoinbaseTipOrdering`.
  - Pass it when constructing these in `IrysPoolBuilder::build_pool`.

- [ ] **Step 3: Thread `ChunkConfig` into `payload.rs` call sites**

  `pd_chunks_for_transaction` calls `parse_pd_transaction`. Add `chunk_config: ChunkConfig` to `CombinedTransactionIterator` and pass it through from `IrysPayloadBuilder`.

- [ ] **Step 4: Update mempool `PdChunkMessage` sends**

  In `pd_transaction_monitor` (mempool.rs line 506-524), the message changes from:
  ```rust
  PdChunkMessage::NewTransaction { tx_hash, chunk_specs: meta.data_reads }
  ```
  This still works — `meta.data_reads` is now `Vec<PdDataRead>` instead of `Vec<ChunkRangeSpecifier>`, matching the updated `PdChunkMessage`.

- [ ] **Step 5: Verify `irys-reth` compiles**

  Run: `cargo check -p irys-reth 2>&1 | head -20`

  Expected: clean compilation for `irys-reth`.

---

### Task 7: Update Block Validation and PD Service

**Files:**
- Modify: `crates/actors/src/block_validation.rs:1448-1480`
- Modify: `crates/actors/src/pd_service.rs`
- Modify: `crates/actors/src/pd_pricing/base_fee.rs`

- [ ] **Step 1: Update `block_validation.rs`**

  At line ~1448, `parse_pd_transaction` is called for each EVM tx. Thread `ChunkConfig` through (it should be available in the validation context from `ConsensusConfig`).

  Update the `ValidPd(meta)` branch:
  - `pd_chunk_specs.extend(meta.data_reads)` — type changes from `Vec<ChunkRangeSpecifier>` to `Vec<PdDataRead>`.
  - `total_pd_chunks += meta.total_chunks` — unchanged.
  - `PdChunkMessage::ProvisionBlockChunks { chunk_specs: pd_chunk_specs, ... }` — type now matches.

- [ ] **Step 2: Update `pd_service.rs` — `specs_to_keys`**

  Change `specs_to_keys(&self, chunk_specs: &[ChunkRangeSpecifier])` → `specs_to_keys(&self, chunk_specs: &[PdDataRead])`.

  The conversion logic changes:
  - Old: `partition_index: U200` → `try_into::<u64>()`
  - New: `partition_index: u64` — direct use, no conversion needed.
  - Old: iterates `offset..(offset + chunk_count)`
  - New: iterates `start..(start + chunks_needed(chunk_size))`

  Update `handle_provision_chunks` and `handle_provision_block_chunks` parameter types accordingly.

- [ ] **Step 3: Update `pd_pricing/base_fee.rs`**

  `count_pd_chunks_in_block` and `extract_priority_fees_from_block` call `parse_pd_transaction`. Thread `ChunkConfig` through. The `ChunkConfig` should be available from the pricing context.

- [ ] **Step 4: Verify `irys-actors` compiles**

  Run: `cargo check -p irys-actors 2>&1 | head -20`

  Expected: clean compilation.

---

### Task 8: Update Precompile Unit Tests

**Files:**
- Rewrite: `crates/irys-reth/src/precompiles/pd/precompile.rs` (module tests)
- Rewrite: `crates/irys-reth/src/precompiles/pd/utils.rs` (module tests)

- [ ] **Step 1: Rewrite `precompile.rs` tests to use `PdDataRead`**

  All tests currently construct `ChunkRangeSpecifier` + `ByteRangeSpecifier` pairs. Replace with single `PdDataRead` entries.

  Example — rewrite `pd_precompile_read_full_byte_range`:

  ```rust
  #[test]
  fn pd_precompile_read_full_byte_range() {
      use irys_types::range_specifier::PdDataRead;

      let spec = PdDataRead {
          partition_index: 0,
          start: 0,
          len: 100,
          byte_off: 0,
      };

      let access_list = with_test_fees(AccessList(vec![AccessListItem {
          address: PD_PRECOMPILE_ADDRESS,
          storage_keys: vec![B256::from(spec.encode())],
      }]));

      let result = execute_precompile(vec![0, 0], access_list, 1);
      assert!(result.result.is_success());
  }
  ```

  Repeat for all 8 test cases. Key pattern: one `PdDataRead` key per specifier instead of chunk+byte pair.

- [ ] **Step 2: Rewrite `utils.rs` tests for `parse_pd_specifiers`**

  Update to test the new function signature and behavior (rejects invalid keys instead of skipping).

- [ ] **Step 3: Run precompile unit tests**

  Run: `cargo nextest run -p irys-reth pd_precompile`

  Expected: All tests pass.

---

### Task 9: Update Integration Test Helpers and Context Isolation Test

**Files:**
- Modify: `crates/chain-tests/src/utils.rs` (PD helpers at lines ~2569-2741)
- Rewrite: `crates/irys-reth/tests/pd_context_isolation.rs`

- [ ] **Step 1: Update `create_and_inject_pd_transaction_with_custom_fees` in `utils.rs`**

  Replace `ChunkRangeSpecifier` construction with `PdDataRead`:

  ```rust
  // Old:
  let chunk_spec = ChunkRangeSpecifier { partition_index: U200::MAX, offset: 0, chunk_count: num_chunks as u16 };
  let byte_spec = ByteRangeSpecifier { index: 0, chunk_offset: 0, byte_offset: U18::ZERO, length: U34::from(len) };
  build_pd_access_list_with_fees(&[chunk_spec], &[byte_spec], priority_fee, base_fee_cap)

  // New:
  let data_read = PdDataRead { partition_index: u64::MAX, start: 0, len: total_bytes, byte_off: 0 };
  build_pd_access_list_with_fees(&[data_read], priority_fee, base_fee_cap)
  ```

  Note: `partition_index: u64::MAX` replaces the sentinel `U200::MAX`. The pd_service `specs_to_keys` overflow check still works with `u64::MAX * num_chunks_in_partition` overflowing.

- [ ] **Step 2: Update `inject_pd_contract_call` in `utils.rs`**

  Change signature from `(chunk_specs: Vec<ChunkRangeSpecifier>, byte_specs: Vec<ByteRangeSpecifier>)` to `(data_reads: Vec<PdDataRead>)`.

- [ ] **Step 3: Rewrite `pd_context_isolation.rs`**

  Replace `create_access_list_with_chunks` to use `PdDataRead`:

  ```rust
  fn create_access_list_with_chunks(num_chunks: u64) -> AccessList {
      let spec = PdDataRead {
          partition_index: 0,
          start: 0,
          len: (num_chunks * 256_000) as u32, // request all bytes
          byte_off: 0,
      };
      AccessList(vec![AccessListItem {
          address: PD_PRECOMPILE_ADDRESS,
          storage_keys: vec![
              B256::from(spec.encode()),
              B256::from(encode_pd_fee(PdAccessListArgsTypeId::PdPriorityFee as u8, U256::from(1_u64)).unwrap()),
              B256::from(encode_pd_fee(PdAccessListArgsTypeId::PdBaseFeeCap as u8, U256::from(1_u64)).unwrap()),
          ],
      }])
  }
  ```

- [ ] **Step 4: Verify all unit tests pass**

  Run: `cargo nextest run -p irys-reth -p irys-types 2>&1 | tail -20`

---

### Task 10: Update Chain Integration Tests

**Files:**
- Update: `crates/chain-tests/src/programmable_data/basic.rs`
- Update: `crates/chain-tests/src/programmable_data/pd_chunk_limit.rs`
- Update: `crates/chain-tests/src/programmable_data/pd_chunk_p2p_pull.rs` (constructs `ChunkRangeSpecifier` inline at line ~237)
- Update: `crates/chain-tests/src/programmable_data/pd_content_verification.rs` (imports+constructs old types at lines 9-10, 229, 234)
- Update: `crates/chain-tests/src/programmable_data/preloaded_chunk_table.rs` (heavy old-type usage at lines 313, 318, 415, 420, 731, 736, 826, 833)
- Update: `crates/chain-tests/src/programmable_data/hardfork_guard.rs`
- Update: `crates/chain-tests/src/programmable_data/min_transaction_cost.rs`
- Update: `crates/chain-tests/src/programmable_data/pd_mock_e2e.rs` (comment referencing old type name)
- Update: `crates/chain-tests/src/external/programmable_data_basic.rs`

- [ ] **Step 1: Update `basic.rs` — the main e2e PD test**

  Replace the two-key access list (ChunkRangeSpecifier + ByteRangeSpecifier) with a single `PdDataRead` key. The test at line ~38 constructs access list keys manually — update to use `PdDataRead::encode()`.

  ```rust
  // Old:
  let chunk_key = B256::from(PdAccessListArg::ChunkRead(ChunkRangeSpecifier { ... }).encode());
  let byte_key = B256::from(PdAccessListArg::ByteRead(ByteRangeSpecifier { ... }).encode());
  storage_keys: vec![chunk_key, byte_key],

  // New:
  let spec = PdDataRead { partition_index: 0, start: 0, len: data_bytes.len() as u32, byte_off: 0 };
  storage_keys: vec![B256::from(spec.encode())],
  ```

  Note: calldata sent to the precompile is still `vec![0, 0]` (function ID 0, index 0) in Phase 1.

- [ ] **Step 2: Update `pd_chunk_p2p_pull.rs`**

  This file constructs `ChunkRangeSpecifier` directly at line ~237 (not via utils helpers). Replace inline:

  ```rust
  // Old:
  let specs = vec![ChunkRangeSpecifier { partition_index: U200::from(partition_index), offset, chunk_count }];
  // New:
  let specs = vec![PdDataRead { partition_index, start: offset, len: chunk_count as u32 * chunk_size as u32, byte_off: 0 }];
  ```

  Note: `len` must be the total bytes to read, not chunk_count. The node derives chunk_count from `ceil((byte_off + len) / chunk_size)`.

- [ ] **Step 3: Update `pd_content_verification.rs` and `preloaded_chunk_table.rs`**

  Both files import old types and construct them inline. `preloaded_chunk_table.rs` has 8 construction sites — each `ChunkRangeSpecifier` + `ByteRangeSpecifier` pair becomes a single `PdDataRead`. Note that `inject_pd_contract_call` signature changed in Task 9 Step 2.

- [ ] **Step 4: Update remaining chain tests**

  - `hardfork_guard.rs`, `min_transaction_cost.rs` — mostly use helpers from `utils.rs` (already updated)
  - `pd_mock_e2e.rs` — update comment referencing old type name
  - `external/programmable_data_basic.rs` — update if it constructs access lists directly

- [ ] **Step 5: Full workspace compilation check**

  Run: `cargo check --workspace --tests 2>&1 | tail -20`

  Expected: Clean compilation.

- [ ] **Step 6: Run PD-related tests**

  Run: `cargo nextest run -p irys-reth -p irys-types pd 2>&1 | tail -20`
  Run: `cargo nextest run -p irys-reth -p irys-types range_specifier 2>&1 | tail -20`

  Expected: All pass.

---

### Task 11: Phase 1 Cleanup and Verification

- [ ] **Step 1: Search for any remaining references to old types**

  Run grep for: `ChunkRangeSpecifier`, `ByteRangeSpecifier`, `PdAccessListArg`, `PdAccessListArgSerde`, `U34`, `U18`, `PdArgsEncWrapper`, `ParsedAccessLists`, `parse_access_list` (the old function name).

  Any hits in non-test code are bugs to fix. Hits in comments should be updated.

- [ ] **Step 2: Run `cargo fmt --all`**

- [ ] **Step 3: Run `cargo clippy --workspace --tests --all-targets`**

  Fix any warnings.

- [ ] **Step 4: Run full test suite**

  Run: `cargo xtask test 2>&1 | tail -30`

  Expected: All tests pass (excluding known flaky tests).

---

## Phase 2: Precompile ABI Redesign

### Task 12: Add `alloy-sol-types` Dependency

**Files:**
- Modify: `crates/irys-reth/Cargo.toml`

- [ ] **Step 1: Add dependency**

  Add `alloy-sol-types.workspace = true` to the `[dependencies]` section.

- [ ] **Step 2: Verify compilation**

  Run: `cargo check -p irys-reth 2>&1 | head -5`

---

### Task 13: Replace Function IDs with `sol!` ABI Definitions

**Files:**
- Rewrite: `crates/irys-reth/src/precompiles/pd/functions.rs`

- [ ] **Step 1: Replace `PdFunctionId` with `sol!` macro block**

  ```rust
  //! PD precompile ABI definitions via alloy sol! macro.
  //!
  //! Generates 4-byte selectors, parameter decoders, return encoders,
  //! and error struct encoders at compile time.

  use alloy_sol_types::sol;

  sol! {
      function readData(uint8 index) external view returns (bytes memory data);
      function readBytes(uint8 index, uint32 offset, uint32 length)
          external view returns (bytes memory data);

      error MissingAccessList();
      error SpecifierNotFound(uint8 index, uint256 available);
      error ByteRangeOutOfBounds(uint256 start, uint256 end, uint256 available);
      error ChunkNotFound(uint64 offset);
      error ChunkFetchFailed(uint64 offset);
  }
  ```

  Delete `PdFunctionId` enum, `PdFunctionIdDecodeError`, and the `TryFrom<u8>` impl.

- [ ] **Step 2: Verify the sol! macro generates expected types**

  Run: `cargo check -p irys-reth 2>&1 | head -10`

  The macro generates: `readDataCall`, `readBytesCall`, `MissingAccessList`, `SpecifierNotFound`, `ByteRangeOutOfBounds`, `ChunkNotFound`, `ChunkFetchFailed`. Each has `SELECTOR`, `abi_decode`, `abi_encode`/`abi_encode_returns`.

---

### Task 14: Add `try_encode_as_revert` to Error Module

**Files:**
- Modify: `crates/irys-reth/src/precompiles/pd/error.rs`

- [ ] **Step 1: Add `try_encode_as_revert` function**

  ```rust
  use alloy_sol_types::SolError;
  use revm::precompile::PrecompileOutput;
  use super::functions::*;

  /// Convert a user-facing PdPrecompileError into a reverted PrecompileOutput
  /// with ABI-encoded custom error data. Returns None for internal errors.
  pub fn try_encode_as_revert(e: &PdPrecompileError, gas_used: u64) -> Option<PrecompileOutput> {
      let revert_data: Vec<u8> = match e {
          PdPrecompileError::MissingAccessList =>
              MissingAccessList {}.abi_encode(),
          PdPrecompileError::SpecifierNotFound { index, available } =>
              SpecifierNotFound { index: *index, available: U256::from(*available) }.abi_encode(),
          PdPrecompileError::ByteRangeOutOfBounds { start, end, available } =>
              ByteRangeOutOfBounds {
                  start: U256::from(*start), end: U256::from(*end), available: U256::from(*available)
              }.abi_encode(),
          PdPrecompileError::ChunkNotFound { offset } =>
              ChunkNotFound { offset: *offset }.abi_encode(),
          PdPrecompileError::ChunkFetchFailed { offset, .. } =>
              ChunkFetchFailed { offset: *offset }.abi_encode(),
          _ => return None, // Internal errors stay as PrecompileError::Other
      };
      Some(PrecompileOutput::new_reverted(gas_used, revert_data.into()))
  }
  ```

- [ ] **Step 2: Update `ChunkFetchFailed` error variant to include `offset: u64`**

  Ensure the error variant stores `offset` as `u64` (it already does per Task 7). The `reason: String` field is kept for logging but not included in the ABI-encoded revert data.

- [ ] **Step 3: Verify compilation**

  Run: `cargo check -p irys-reth 2>&1 | head -10`

---

### Task 15: Rewrite Precompile Dispatch with ABI Selectors

**Files:**
- Rewrite: `crates/irys-reth/src/precompiles/pd/precompile.rs` (the `pd_precompile` function)
- Modify: `crates/irys-reth/src/precompiles/pd/constants.rs`

- [ ] **Step 1: Update `constants.rs` — remove old encoding constants**

  Remove: `FUNCTION_ID_SIZE`, `INDEX_SIZE`, `INDEX_OFFSET`, `READ_BYTES_RANGE_BY_INDEX_CALLDATA_LEN`, `OFFSET_SIZE`, `OFFSET_FIELD_OFFSET`, `LENGTH_SIZE`, `LENGTH_FIELD_OFFSET`, `READ_PARTIAL_BYTE_RANGE_CALLDATA_LEN`.

  Keep: `PD_BASE_GAS_COST`.

- [ ] **Step 2: Rewrite `pd_precompile` function**

  ```rust
  use alloy_sol_types::SolCall;
  use super::functions::*;

  fn pd_precompile(pd_context: PdContext) -> DynPrecompile {
      (move |input: PrecompileInput<'_>| -> PrecompileResult {
          let data = input.data;
          let gas_limit = input.gas;

          // Minimum 4 bytes for ABI selector
          if data.len() < 4 {
              return Err(PdPrecompileError::InsufficientInput {
                  expected: 4,
                  actual: data.len(),
              }.into());
          }

          if PD_BASE_GAS_COST > gas_limit {
              return Err(PrecompileError::OutOfGas);
          }

          let access_list = pd_context.read_access_list();
          if access_list.is_empty() {
              return Err(PdPrecompileError::MissingAccessList.into());
          }

          let chunk_config = pd_context.chunk_config();
          let specifiers = parse_pd_specifiers(&access_list, &chunk_config)
              .map_err(|e| PdPrecompileError::InvalidAccessList { reason: e.to_string() })?;

          let selector: [u8; 4] = data[..4].try_into().unwrap();

          let result = match selector {
              readDataCall::SELECTOR => {
                  let decoded = readDataCall::abi_decode(&data[4..])
                      .map_err(|e| PrecompileError::Other(Cow::Owned(format!("ABI decode error: {e}"))))?;
                  let spec = specifiers.get(decoded.index as usize)
                      .ok_or(PdPrecompileError::SpecifierNotFound {
                          index: decoded.index,
                          available: specifiers.len(),
                      })?;
                  // readData: use specifier's byte_off and len
                  read_chunk_data(spec, spec.byte_off, spec.len, &pd_context)
              }
              readBytesCall::SELECTOR => {
                  let decoded = readBytesCall::abi_decode(&data[4..])
                      .map_err(|e| PrecompileError::Other(Cow::Owned(format!("ABI decode error: {e}"))))?;
                  let spec = specifiers.get(decoded.index as usize)
                      .ok_or(PdPrecompileError::SpecifierNotFound {
                          index: decoded.index,
                          available: specifiers.len(),
                      })?;
                  // readBytes: use calldata offset/length, ignore specifier's byte_off/len
                  read_chunk_data(spec, decoded.offset, decoded.length, &pd_context)
              }
              _ => return Err(PrecompileError::Other("unknown selector".into())),
          };

          match result {
              Ok(bytes) => {
                  // ABI-encode the return value: abi.encode(bytes memory)
                  let encoded = readDataCall::abi_encode_returns(&(bytes,));
                  Ok(PrecompileOutput::new(PD_BASE_GAS_COST, encoded.into()))
              }
              Err(e) => {
                  // Try to encode as ABI custom error revert
                  if let Some(reverted) = try_encode_as_revert(&e, PD_BASE_GAS_COST) {
                      Ok(reverted)
                  } else {
                      Err(e.into())
                  }
              }
          }
      }).into()
  }
  ```

  Key changes from Phase 1 dispatch:
  - Minimum input 4 bytes (was 2)
  - 4-byte selector matching (was 1-byte function ID)
  - ABI-decode parameters (was manual byte parsing)
  - ABI-encode return data: `readDataCall::abi_encode_returns(&(bytes,))` (was raw bytes)
  - User-facing errors return `PrecompileOutput::new_reverted()` (was `PrecompileError::Other`)

- [ ] **Step 3: Verify compilation**

  Run: `cargo check -p irys-reth 2>&1 | head -10`

---

### Task 16: Create Solidity Contracts

**Files:**
- Create: `fixtures/contracts/src/IIrysPD.sol`
- Create: `fixtures/contracts/src/IrysPDLib.sol`
- Rewrite: `fixtures/contracts/src/IrysProgrammableDataBasic.sol`
- Delete: `fixtures/contracts/src/Precompiles.sol`
- Delete: `fixtures/contracts/src/ProgrammableData.sol`

- [ ] **Step 1: Create `IIrysPD.sol`**

  Write the interface exactly as specified in spec section 6.3. Contains:
  - `interface IIrysPD` with `readData(uint8)` and `readBytes(uint8, uint32, uint32)`
  - 5 custom errors
  - `IRYS_PD_ADDRESS` and `IRYS_PD` constants

- [ ] **Step 2: Create `IrysPDLib.sol`**

  Write the library exactly as specified in spec section 6.4. Contains:
  - `readData()` and `readBytes(offset, length)` convenience wrappers (index 0)
  - `tryReadData(index)` and `tryReadBytes(index, offset, length)` try-variants

- [ ] **Step 3: Rewrite `IrysProgrammableDataBasic.sol`**

  Replace inheritance-based version with interface/library usage as specified in spec section 6.5.

- [ ] **Step 4: Delete `Precompiles.sol` and `ProgrammableData.sol`**

  These are fully replaced.

- [ ] **Step 5: Compile Solidity contracts**

  Run from `fixtures/contracts/`: `forge build` (or the project's Solidity build command).

  Expected: All contracts compile. Artifacts regenerated.

---

### Task 17: Rewrite All Precompile Tests for ABI Encoding

**Files:**
- Rewrite: `crates/irys-reth/src/precompiles/pd/precompile.rs` (module tests)
- Rewrite: `crates/irys-reth/tests/pd_context_isolation.rs`

- [ ] **Step 1: Rewrite precompile.rs test helpers**

  Tests now need to encode calldata using ABI selectors instead of raw bytes:

  ```rust
  use alloy_sol_types::SolCall;
  use super::functions::*;

  // Old: vec![0, 0]  (function ID 0, index 0)
  // New:
  let calldata = readDataCall { index: 0 }.abi_encode();

  // Old: vec![1, 0] + offset.to_be_bytes() + length.to_be_bytes()
  // New:
  let calldata = readBytesCall { index: 0, offset: 100, length: 200 }.abi_encode();
  ```

- [ ] **Step 2: Rewrite all 8 test cases**

  Key test updates:
  - `pd_precompile_read_full_byte_range`: use `readDataCall::abi_encode()`, verify output is ABI-encoded bytes (not raw)
  - `test_insufficient_input_data`: input of 1-3 bytes should fail (was 1 byte)
  - `test_read_partial_byte_range`: use `readBytesCall::abi_encode()`
  - `test_no_access_list`: calldata now starts with 4-byte selector
  - `test_invalid_function_id`: use an unknown 4-byte selector
  - `test_moderate_chunks`, `test_large_chunks_no_overflow`: update calldata encoding

  For tests that check return data, ABI-decode the output:
  ```rust
  let output = result.result.into_output().unwrap();
  let decoded = readDataCall::abi_decode_returns(&output).unwrap();
  let data: Bytes = decoded.data;
  ```

- [ ] **Step 3: Add test for custom error reverts**

  New test: verify that calling `readData` with an invalid index returns an ABI-encoded `SpecifierNotFound` error:

  ```rust
  #[test]
  fn test_specifier_not_found_custom_error() {
      let spec = PdDataRead { partition_index: 0, start: 0, len: 100, byte_off: 0 };
      let access_list = with_test_fees(AccessList(vec![AccessListItem {
          address: PD_PRECOMPILE_ADDRESS,
          storage_keys: vec![B256::from(spec.encode())],
      }]));

      // Request index 5, but only 1 specifier exists
      let calldata = readDataCall { index: 5 }.abi_encode();
      let result = execute_precompile(calldata, access_list, 1);

      // Should succeed at EVM level (reverted precompile output, not EVM error)
      assert!(result.result.is_success() || /* check for revert output */);

      // Decode the revert data
      let output = result.result.into_output().unwrap();
      // The output should contain ABI-encoded SpecifierNotFound error
  }
  ```

- [ ] **Step 4: Rewrite `pd_context_isolation.rs`**

  Update calldata from `vec![0, 0]` to `readDataCall { index: 0 }.abi_encode()`.

- [ ] **Step 5: Run all precompile tests**

  Run: `cargo nextest run -p irys-reth pd 2>&1 | tail -20`

---

### Task 18: Update Chain Integration Tests for ABI Calldata

**Files:**
- Update: `crates/chain-tests/src/programmable_data/basic.rs`
- Update: `crates/chain-tests/src/external/programmable_data_basic.rs`
- Update: `crates/chain-tests/src/utils.rs`
- Update: other chain-tests that send PD calldata

- [ ] **Step 1: Update `basic.rs`**

  The test calls `contract.readPdChunkIntoStorage()` which internally calls the precompile. Since the Solidity contract is rewritten (Task 16), the compiled artifact changes. The test uses `sol!` macro to codegen bindings from `ProgrammableDataBasic.json` — this artifact is regenerated after Solidity compilation.

  Update the test to use the new contract's interface. The function `readPdChunkIntoStorage()` still exists in the rewritten contract, so the test call likely stays the same. The access list construction changes (already done in Task 10).

- [ ] **Step 2: Verify Solidity artifact paths**

  The test imports from `../../fixtures/contracts/out/IrysProgrammableDataBasic.sol/ProgrammableDataBasic.json`. After rewriting the Solidity contract, ensure the artifact path and contract name still match.

- [ ] **Step 3: Run chain integration tests (PD-specific)**

  Run: `cargo nextest run -p irys-chain-tests programmable_data 2>&1 | tail -20`

  Note: Some of these are `#[ignore]` heavy tests. For initial verification, run the non-ignored ones first.

---

### Task 19: Phase 2 Cleanup and Final Verification

- [ ] **Step 1: Search for remaining references to old patterns**

  Grep for: `PdFunctionId`, `ReadFullByteRange`, `ReadPartialByteRange`, `FUNCTION_ID_SIZE`, `INDEX_SIZE`, `INDEX_OFFSET`, `READ_BYTES_RANGE_BY_INDEX_CALLDATA_LEN`, `READ_PARTIAL_BYTE_RANGE_CALLDATA_LEN`, `parse_access_list` (old name), `ParsedAccessLists`.

  Any hits in non-comment code are bugs.

  Also update module-level documentation in `crates/irys-reth/src/precompiles/pd/mod.rs` — it currently describes "two main operations for reading byte ranges" which is outdated. Update to describe `readData` and `readBytes` with ABI dispatch.

- [ ] **Step 2: Run `cargo fmt --all`**

- [ ] **Step 3: Run `cargo clippy --workspace --tests --all-targets`**

- [ ] **Step 4: Run full test suite**

  Run: `cargo xtask test 2>&1 | tail -30`

- [ ] **Step 5: Run local checks (approximates CI)**

  Run: `cargo xtask local-checks`

  Expected: All checks pass.
