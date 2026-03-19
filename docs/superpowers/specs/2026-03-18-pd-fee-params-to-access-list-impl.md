# Implementation Spec: Move PD Fee Parameters from Calldata Prefix to Access List

**Design Spec**: `2026-03-18-pd-fee-params-to-access-list.md`
**Date**: 2026-03-19
**Branch**: `feat/pd`

---

## Overview

This spec translates the design into ordered, file-level implementation steps. Each step is scoped to one logical unit of work and can be committed independently.

---

## Step 1: Extend the type-byte registry in `range_specifier.rs`

**File**: `crates/types/src/range_specifier.rs`

### 1a. Remap existing type IDs and add new ones

The design uses type bytes `0x01`, `0x02`, `0x03`. The current code uses `0x00` for `ChunkRead` and `0x01` for `ByteRead`. To align:

| Type byte | Current | After |
|-----------|---------|-------|
| `0x00` | `ChunkRead` | `ChunkRead` (unchanged) |
| `0x01` | `ByteRead` | `ByteRead` (unchanged) |
| `0x02` | — | `PdPriorityFee` |
| `0x03` | — | `PdBaseFeeCap` |

> **Note**: The design spec says `PdDataRead = 0x01`. However, `ChunkRead` is currently `0x00` and `ByteRead` is `0x01` — this is already deployed on the `feat/pd` branch. The implementation should keep `0x00` and `0x01` as-is and add `0x02` and `0x03` for the fee types. The design spec's `0x01` for "PdDataRead" was an abstract grouping, not a literal re-numbering.

### 1b. Add variants to `PdAccessListArgsTypeId`

```rust
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PdAccessListArgsTypeId {
    ChunkRead = 0,
    ByteRead = 1,
    PdPriorityFee = 2,
    PdBaseFeeCap = 3,
}
```

Update `TryFrom<u8>`:
```rust
impl TryFrom<u8> for PdAccessListArgsTypeId {
    type Error = PdAccessListArgsTypeIdDecodeError;
    fn try_from(id: u8) -> Result<Self, Self::Error> {
        match id {
            0 => Ok(Self::ChunkRead),
            1 => Ok(Self::ByteRead),
            2 => Ok(Self::PdPriorityFee),
            3 => Ok(Self::PdBaseFeeCap),
            _ => Err(PdAccessListArgsTypeIdDecodeError::UnknownPdAccessListArgsTypeId(id)),
        }
    }
}
```

### 1c. Add variants to `PdAccessListArg`

```rust
pub enum PdAccessListArg {
    ChunkRead(ChunkRangeSpecifier),
    ByteRead(ByteRangeSpecifier),
    PdPriorityFee(U256),
    PdBaseFeeCap(U256),
}
```

Update `type_id()`, `encode()`, `decode()` methods accordingly.

### 1d. Define `MAX_PD_FEE` and fee encode/decode

Add to `range_specifier.rs` (or a new sibling `pd_fee.rs` in `crates/types/src/` — prefer keeping it in `range_specifier.rs` since the trait `PdAccessListArgSerde` is already there):

```rust
/// Maximum PD fee value: 2^248 - 1. Fits in 31 bytes (the payload after the 1-byte type discriminant).
pub const MAX_PD_FEE: U256 = U256::from_be_bytes([
    0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
]);
```

Implement `PdAccessListArgSerde` for a fee wrapper (or implement encode/decode directly in the `PdAccessListArg` match arms). The encoding is:

```
encode_fee(type_byte, fee) -> [u8; 32]:
  buf[0] = type_byte
  buf[1..32] = fee.to_be_bytes()[1..32]  // top byte is always 0 for u248

decode_fee(bytes, expected_type) -> Result<U256>:
  assert bytes[0] == expected_type
  be_bytes[0] = 0x00  // enforce u248 bound
  be_bytes[1..32] = bytes[1..32]
  fee = U256::from_be_bytes(be_bytes)
  assert fee > 0
```

> **Important**: The fee encoding uses **big-endian** for the 31-byte U256, unlike `ChunkRangeSpecifier` which uses little-endian for its fields. This is by design — fees are conceptually big integers, not multi-field structs.

### 1e. Unit tests

- Encode/decode round-trip for `PdPriorityFee` and `PdBaseFeeCap`
- Reject fee = 0
- Reject fee > `MAX_PD_FEE`
- Reject wrong type byte
- Verify `MAX_PD_FEE` encodes/decodes correctly
- Verify a fee of `U256::from(1)` encodes correctly (31 zero bytes + 0x01)

---

## Step 2: Build the canonical PD parser in `pd_tx.rs`

**File**: `crates/irys-reth/src/pd_tx.rs`

### 2a. Add `PdParseResult` and `PdTransactionMeta`

```rust
/// Result of parsing a transaction for PD metadata.
pub enum PdParseResult {
    /// No PD keys found under PD_PRECOMPILE_ADDRESS. Not a PD transaction.
    NotPd,
    /// Valid PD transaction with extracted metadata.
    ValidPd(PdTransactionMeta),
    /// Keys found under PD_PRECOMPILE_ADDRESS but validation failed.
    InvalidPd(PdValidationError),
}

pub struct PdTransactionMeta {
    pub data_reads: Vec<ChunkRangeSpecifier>,
    pub byte_reads: Vec<ByteRangeSpecifier>,
    pub priority_fee_per_chunk: U256,
    pub base_fee_cap_per_chunk: U256,
    pub total_chunks: u64,
}
```

> Design says `data_reads: Vec<PdDataRead>`. In the codebase, this is `Vec<ChunkRangeSpecifier>`. Add `byte_reads` too since `ByteRangeSpecifier` entries exist in the access list and downstream code (`extract_pd_chunk_specs`, precompile) needs them.

### 2b. Define `PdValidationError`

```rust
#[derive(Debug, thiserror::Error)]
pub enum PdValidationError {
    #[error("multiple AccessListItems targeting PD_PRECOMPILE_ADDRESS")]
    MultiplePdItems,
    #[error("duplicate PdPriorityFee key")]
    DuplicatePriorityFee,
    #[error("duplicate PdBaseFeeCap key")]
    DuplicateBaseFeeCap,
    #[error("missing PdPriorityFee key")]
    MissingPriorityFee,
    #[error("missing PdBaseFeeCap key")]
    MissingBaseFeeCap,
    #[error("no PdDataRead (ChunkRead) keys")]
    NoDataReads,
    #[error("fee value is zero")]
    ZeroFee,
    #[error("fee value exceeds MAX_PD_FEE (u248)")]
    FeeOverflow,
    #[error("unknown type byte {0:#x} in PD access list key")]
    UnknownTypeByte(u8),
    #[error("duplicate PdDataRead key")]
    DuplicateDataRead,
    #[error("invalid PdDataRead encoding: {0}")]
    InvalidDataRead(String),
    #[error("invalid ByteRead encoding: {0}")]
    InvalidByteRead(String),
}
```

### 2c. Implement `parse_pd_transaction`

```rust
/// Canonical PD transaction parser. Single entry point for all components.
///
/// Examines the transaction's access list for keys under `PD_PRECOMPILE_ADDRESS`.
/// Returns `NotPd` if no such keys exist, `InvalidPd` if validation fails,
/// or `ValidPd(meta)` with extracted fee parameters and chunk specifiers.
pub fn parse_pd_transaction(access_list: &AccessList) -> PdParseResult {
    // 1. Find AccessListItems targeting PD_PRECOMPILE_ADDRESS
    let pd_items: Vec<&AccessListItem> = access_list
        .0
        .iter()
        .filter(|item| item.address == PD_PRECOMPILE_ADDRESS)
        .collect();

    // No PD items → not a PD transaction
    if pd_items.is_empty() {
        return PdParseResult::NotPd;
    }

    // Multiple items targeting 0x500 → invalid
    if pd_items.len() > 1 {
        return PdParseResult::InvalidPd(PdValidationError::MultiplePdItems);
    }

    let pd_item = pd_items[0];

    // If the PD item has no storage keys, it's not a PD transaction
    // (could be a regular access list warm-up for 0x500)
    if pd_item.storage_keys.is_empty() {
        return PdParseResult::NotPd;
    }

    // 2. Decode all storage keys by type byte
    let mut chunk_reads = Vec::new();
    let mut byte_reads = Vec::new();
    let mut priority_fee: Option<U256> = None;
    let mut base_fee_cap: Option<U256> = None;
    let mut seen_chunk_reads = HashSet::new();

    for key in &pd_item.storage_keys {
        let type_byte = key.0[0];
        match PdAccessListArgsTypeId::try_from(type_byte) {
            Ok(PdAccessListArgsTypeId::ChunkRead) => {
                match ChunkRangeSpecifier::decode(&key.0) {
                    Ok(spec) => {
                        if !seen_chunk_reads.insert(key.0) {
                            return PdParseResult::InvalidPd(PdValidationError::DuplicateDataRead);
                        }
                        chunk_reads.push(spec);
                    }
                    Err(e) => return PdParseResult::InvalidPd(
                        PdValidationError::InvalidDataRead(e.to_string())
                    ),
                }
            }
            Ok(PdAccessListArgsTypeId::ByteRead) => {
                match ByteRangeSpecifier::decode(&key.0) {
                    Ok(spec) => byte_reads.push(spec),
                    Err(e) => return PdParseResult::InvalidPd(
                        PdValidationError::InvalidByteRead(e.to_string())
                    ),
                }
            }
            Ok(PdAccessListArgsTypeId::PdPriorityFee) => {
                if priority_fee.is_some() {
                    return PdParseResult::InvalidPd(PdValidationError::DuplicatePriorityFee);
                }
                match decode_fee(&key.0, 0x02) {
                    Ok(fee) => priority_fee = Some(fee),
                    Err(_) => return PdParseResult::InvalidPd(PdValidationError::ZeroFee),
                }
            }
            Ok(PdAccessListArgsTypeId::PdBaseFeeCap) => {
                if base_fee_cap.is_some() {
                    return PdParseResult::InvalidPd(PdValidationError::DuplicateBaseFeeCap);
                }
                match decode_fee(&key.0, 0x03) {
                    Ok(fee) => base_fee_cap = Some(fee),
                    Err(_) => return PdParseResult::InvalidPd(PdValidationError::ZeroFee),
                }
            }
            Err(_) => {
                return PdParseResult::InvalidPd(PdValidationError::UnknownTypeByte(type_byte));
            }
        }
    }

    // 3. Enforce: at least one ChunkRead required
    if chunk_reads.is_empty() {
        return PdParseResult::InvalidPd(PdValidationError::NoDataReads);
    }

    // 4. Enforce: both fee keys required
    let priority_fee = match priority_fee {
        Some(f) => f,
        None => return PdParseResult::InvalidPd(PdValidationError::MissingPriorityFee),
    };
    let base_fee_cap = match base_fee_cap {
        Some(f) => f,
        None => return PdParseResult::InvalidPd(PdValidationError::MissingBaseFeeCap),
    };

    // 5. Compute total chunks
    let total_chunks: u64 = chunk_reads.iter().map(|s| s.chunk_count as u64).sum();

    PdParseResult::ValidPd(PdTransactionMeta {
        data_reads: chunk_reads,
        byte_reads,
        priority_fee_per_chunk: priority_fee,
        base_fee_cap_per_chunk: base_fee_cap,
        total_chunks,
    })
}
```

### 2d. Add `build_pd_access_list_with_fees` helper

Replace `build_pd_access_list` with an extended version that also encodes fee keys:

```rust
/// Build a PD access list with chunk/byte specifiers AND fee parameters.
///
/// This is the canonical constructor for PD transaction access lists.
pub fn build_pd_access_list_with_fees(
    chunk_specs: impl IntoIterator<Item = ChunkRangeSpecifier>,
    byte_specs: impl IntoIterator<Item = ByteRangeSpecifier>,
    priority_fee_per_chunk: U256,
    base_fee_cap_per_chunk: U256,
) -> AccessList {
    let mut storage_keys: Vec<B256> = chunk_specs
        .into_iter()
        .map(|spec| B256::from(spec.encode()))
        .collect();
    storage_keys.extend(
        byte_specs.into_iter().map(|spec| B256::from(spec.encode()))
    );
    storage_keys.push(B256::from(encode_fee(0x02, priority_fee_per_chunk)));
    storage_keys.push(B256::from(encode_fee(0x03, base_fee_cap_per_chunk)));

    AccessList::from(vec![AccessListItem {
        address: PD_PRECOMPILE_ADDRESS,
        storage_keys,
    }])
}
```

Keep `build_pd_access_list` as a convenience for tests that don't need fee parameters (e.g., precompile-only tests). Alternatively, deprecate it and migrate all callers — this is a judgment call during implementation.

### 2e. Update `sum_pd_chunks_in_access_list`

This function currently iterates all keys and tries to decode them as `PdAccessListArg`. With the new fee type bytes, it must skip fee keys cleanly. Two approaches:

**Option A** (minimal change): Update `PdAccessListArg::decode` to handle the new type bytes, and have `sum_pd_chunks_in_access_list` match on the new variants returning `Some(0)`.

**Option B** (recommended): Since `parse_pd_transaction` now returns `total_chunks`, most callers can switch to using the parser. Keep `sum_pd_chunks_in_access_list` for backward-compat but update it to handle fee keys gracefully (return 0 for fee keys).

### 2f. Remove old header code

Delete from `pd_tx.rs`:
- `IRYS_PD_HEADER_MAGIC`
- `PD_HEADER_VERSION_V1`
- `PD_HEADER_VERSION_SIZE`
- `U256_SIZE`
- `PD_HEADER_V1_SIZE`
- `PdHeaderV1` (struct + BorshSerialize + BorshDeserialize impls)
- `prepend_pd_header_v1_to_calldata()`
- `detect_and_decode_pd_header()`

### 2g. Unit tests for `parse_pd_transaction`

- `NotPd`: empty access list, access list with non-PD addresses only, PD address with empty keys
- `ValidPd`: 1 ChunkRead + both fees, multiple ChunkReads + ByteReads + both fees
- `InvalidPd` for each error variant:
  - Multiple `AccessListItem`s targeting `PD_PRECOMPILE_ADDRESS`
  - Duplicate `PdPriorityFee`
  - Duplicate `PdBaseFeeCap`
  - Missing `PdPriorityFee`
  - Missing `PdBaseFeeCap`
  - No `ChunkRead` keys
  - Zero fee value
  - Unknown type byte (e.g., `0x04`)
  - Duplicate `ChunkRead` key (exact byte match)
  - Invalid `ChunkRead` encoding
- Round-trip: `build_pd_access_list_with_fees` → `parse_pd_transaction` → `ValidPd` with matching values
- Order independence: fee keys before/after/between data reads all parse identically

---

## Step 3: Migrate EVM execution (`evm.rs`)

**File**: `crates/irys-reth/src/evm.rs`

### 3a. Replace PD detection in `transact_raw` (line ~654)

**Before** (lines 654-750):
```rust
if let Some((pd_header, consumed)) =
    crate::pd_tx::detect_and_decode_pd_header(&tx.data).expect("pd header parse error")
{
    let chunks_u64 = crate::pd_tx::sum_pd_chunks_in_access_list(&tx.access_list);
    // ... fee calculation using pd_header.max_base_fee_per_chunk, pd_header.max_priority_fee_per_chunk
    // ... strip calldata: tx.data = tx.data.slice(consumed..);
}
```

**After**:
```rust
if let PdParseResult::ValidPd(pd_meta) =
    crate::pd_tx::parse_pd_transaction(&tx.access_list)
{
    let chunks = U256::from(pd_meta.total_chunks);
    let actual_per_chunk = self.read_pd_base_fee_per_chunk();

    if actual_per_chunk > pd_meta.base_fee_cap_per_chunk {
        // ... reject (same as before)
    }

    let base_per_chunk = actual_per_chunk;
    let prio_per_chunk = pd_meta.priority_fee_per_chunk;
    let base_total = chunks.saturating_mul(base_per_chunk);
    let prio_total = chunks.saturating_mul(prio_per_chunk);

    // ... min cost validation (same as before)
    // ... balance check (same as before)

    // NO calldata stripping needed — calldata is already clean ABI
    // (remove the 3 lines: let stripped = tx.data.slice(consumed..); let mut tx = tx; tx.data = stripped;)

    // ... fee transfers (same as before)
}
```

**Also handle `InvalidPd`**:
```rust
if let PdParseResult::InvalidPd(err) =
    crate::pd_tx::parse_pd_transaction(&tx.access_list)
{
    tracing::debug!(?err, "Invalid PD transaction rejected at EVM layer");
    return Err(EVMError::Transaction(InvalidTransaction::Other(
        format!("invalid PD transaction: {err}").into(),
    )));
}
```

> **Key change**: No calldata mutation. The 3-line stripping block (`let stripped`, `let mut tx`, `tx.data = stripped`) is deleted entirely.

### 3b. Update test module (lines ~1760+)

The test module in `evm.rs` has a local `build_pd_access_list(num_chunks)` helper and multiple test functions that construct PD transactions with `prepend_pd_header_v1_to_calldata`.

**For each test** (`test_transact_raw_pd_*`):
1. Replace `PdHeaderV1 { ... }` + `prepend_pd_header_v1_to_calldata(&header, &user_calldata)` with:
   - `input: user_calldata` (or `Bytes::from(...)` for the raw contract calldata)
2. Replace `build_pd_access_list(N)` with `build_pd_access_list_with_fees(chunk_specs, [], priority_fee, base_fee_cap)`
3. Remove `detect_and_decode_pd_header` round-trip assertions in tests (they validated the old header format)

**Tests to update**:
- `test_transact_raw_pd_fee_deduction` (~line 1938)
- `test_transact_raw_pd_simulate_payload_builder` (~line 2100)
- `test_transact_raw_pd_revert_preserves_fees` (~line 2230)
- `test_transact_raw_pd_max_base_fee_cap_enforcement` (~line 2410)
- `test_transact_raw_pd_excessive_base_fee_rejected` (~line 2570)

---

## Step 4: Migrate mempool validation (`mempool.rs`)

**File**: `crates/irys-reth/src/mempool.rs`

### 4a. Replace pre-Sprite PD rejection (line ~265)

**Before**:
```rust
if !self.is_sprite_active()
    && let Ok(Some(_)) = crate::pd_tx::detect_and_decode_pd_header(input)
```

**After**:
```rust
if !self.is_sprite_active() {
    match crate::pd_tx::parse_pd_transaction(
        tx.access_list().unwrap_or(&AccessList::default())
    ) {
        PdParseResult::ValidPd(_) | PdParseResult::InvalidPd(_) => {
            // Reject any PD-shaped tx before Sprite
            return Err(TransactionValidationOutcome::Invalid(...));
        }
        PdParseResult::NotPd => {}
    }
}
```

> **Note on API**: `tx.access_list()` returns `Option<&AccessList>`. The parser takes `&AccessList`. When `None`, pass a default empty access list (which will return `NotPd`).

### 4b. Replace Sprite fee validation (line ~282)

**Before**:
```rust
if self.is_sprite_active()
    && let Ok(Some((pd_header, _))) = crate::pd_tx::detect_and_decode_pd_header(input)
{
    let chunks = tx.access_list().map(sum_pd_chunks_in_access_list).unwrap_or(0);
    let total_per_chunk = pd_header.max_base_fee_per_chunk
        .saturating_add(pd_header.max_priority_fee_per_chunk);
    // ...
}
```

**After**:
```rust
if self.is_sprite_active() {
    match crate::pd_tx::parse_pd_transaction(
        tx.access_list().unwrap_or(&AccessList::default())
    ) {
        PdParseResult::InvalidPd(err) => {
            tracing::trace!(?err, "Rejecting invalid PD transaction at mempool");
            return Err(TransactionValidationOutcome::Invalid(...));
        }
        PdParseResult::ValidPd(meta) => {
            let total_per_chunk = meta.base_fee_cap_per_chunk
                .saturating_add(meta.priority_fee_per_chunk);
            let total_fees = total_per_chunk
                .saturating_mul(U256::from(meta.total_chunks));
            if total_fees.is_zero() {
                return Err(TransactionValidationOutcome::Invalid(...));
            }
        }
        PdParseResult::NotPd => {}
    }
}
```

### 4c. Replace priority scoring (line ~418)

**Before**:
```rust
match crate::pd_tx::detect_and_decode_pd_header(transaction.input()) {
    Ok(Some((hdr, _))) => {
        let pd_tip_per_chunk = hdr.max_priority_fee_per_chunk;
        let chunks_u64 = transaction.access_list()
            .map(sum_pd_chunks_in_access_list).unwrap_or(0_u64);
        U256::from(chunks_u64).saturating_mul(pd_tip_per_chunk)
    }
    _ => U256::ZERO,
}
```

**After**:
```rust
match crate::pd_tx::parse_pd_transaction(
    transaction.access_list().unwrap_or(&AccessList::default())
) {
    PdParseResult::ValidPd(meta) => {
        U256::from(meta.total_chunks).saturating_mul(meta.priority_fee_per_chunk)
    }
    _ => U256::ZERO,
}
```

### 4d. Replace PD transaction monitor detection (line ~492)

**Before**:
```rust
if let Ok(Some(_)) =
    crate::pd_tx::detect_and_decode_pd_header(tx.transaction.input())
    && known_pd_txs.insert(tx_hash)
    && let Some(access_list) = tx.transaction.access_list()
{
    let chunk_specs = crate::pd_tx::extract_pd_chunk_specs(access_list);
```

**After**:
```rust
if let Some(access_list) = tx.transaction.access_list()
    && let PdParseResult::ValidPd(meta) = crate::pd_tx::parse_pd_transaction(access_list)
    && known_pd_txs.insert(tx_hash)
{
    let chunk_specs = meta.data_reads; // Already extracted by the parser
```

---

## Step 5: Migrate payload builder (`payload.rs`)

**File**: `crates/irys-reth/src/payload.rs`

### 5a. Replace `pd_chunks_for_transaction` (line ~483)

**Before**:
```rust
fn pd_chunks_for_transaction(&self, transaction: &ValidPoolTransaction<...>) -> u64 {
    if !self.is_sprite_active { return 0; }
    match detect_and_decode_pd_header(transaction.transaction.input()) {
        Ok(Some(_)) => transaction.transaction.access_list()
            .map(sum_pd_chunks_in_access_list).unwrap_or_default(),
        _ => 0,
    }
}
```

**After**:
```rust
fn pd_chunks_for_transaction(&self, transaction: &ValidPoolTransaction<...>) -> u64 {
    if !self.is_sprite_active { return 0; }
    match transaction.transaction.access_list()
        .map(parse_pd_transaction)
    {
        Some(PdParseResult::ValidPd(meta)) => meta.total_chunks,
        _ => 0,
    }
}
```

### 5b. Replace `pop_ready_deferred` closure (line ~532)

Same pattern as 5a — replace `detect_and_decode_pd_header` + `sum_pd_chunks_in_access_list` with `parse_pd_transaction`.

### 5c. Update test helper `pd_pooled_transaction` (line ~793)

Replace `PdHeaderV1` construction + `prepend_pd_header_v1_to_calldata` with `build_pd_access_list_with_fees` and clean calldata.

---

## Step 6: Migrate block validation (`block_validation.rs`)

**File**: `crates/actors/src/block_validation.rs`

### 6a. Replace PD chunk budget check (line ~1448)

**Before**:
```rust
for tx in evm_block.body.transactions.iter() {
    let input = tx.input();
    if let Ok(Some(_header)) = detect_and_decode_pd_header(input)
        && let Some(access_list) = tx.access_list()
    {
        let chunks = sum_pd_chunks_in_access_list(access_list);
        total_pd_chunks = total_pd_chunks.saturating_add(chunks);
        pd_chunk_specs.extend(extract_pd_chunk_specs(access_list));
    }
}
```

**After**:
```rust
for tx in evm_block.body.transactions.iter() {
    if let Some(access_list) = tx.access_list() {
        match parse_pd_transaction(access_list) {
            PdParseResult::ValidPd(meta) => {
                total_pd_chunks = total_pd_chunks.saturating_add(meta.total_chunks);
                pd_chunk_specs.extend(meta.data_reads);
            }
            PdParseResult::InvalidPd(err) => {
                tracing::debug!(%err, "Rejecting block: contains invalid PD transaction");
                eyre::bail!("Block contains invalid PD transaction: {err}");
            }
            PdParseResult::NotPd => {}
        }
    }
}
```

> **Consensus-critical**: `InvalidPd` transactions in a block MUST cause block rejection. This is a stricter check than before (previously, a malformed PD header would just be `None` and the tx would be treated as non-PD).

---

## Step 7: Migrate PD pricing (`pd_pricing/base_fee.rs`)

**File**: `crates/actors/src/pd_pricing/base_fee.rs`

### 7a. Replace `count_pd_chunks_in_block` (line ~272)

**Before**:
```rust
for tx in evm_block.body.transactions.iter() {
    let input = tx.input();
    if let Ok(Some(_header)) = detect_and_decode_pd_header(input) {
        if let Some(access_list) = tx.access_list() {
            let chunks = sum_pd_chunks_in_access_list(access_list);
            total_pd_chunks = total_pd_chunks.saturating_add(chunks);
        }
    }
}
```

**After**:
```rust
for tx in evm_block.body.transactions.iter() {
    if let Some(access_list) = tx.access_list() {
        if let PdParseResult::ValidPd(meta) = parse_pd_transaction(access_list) {
            total_pd_chunks = total_pd_chunks.saturating_add(meta.total_chunks);
        }
    }
}
```

### 7b. Replace `extract_priority_fees_from_block` (line ~297)

**Before**:
```rust
if let Ok(Some((header, _))) = detect_and_decode_pd_header(input) {
    let priority_fee_irys = Amount::new(header.max_priority_fee_per_chunk.into());
    priority_fees.push(priority_fee_irys);
}
```

**After**:
```rust
if let Some(access_list) = tx.access_list() {
    if let PdParseResult::ValidPd(meta) = parse_pd_transaction(access_list) {
        let priority_fee_irys = Amount::new(meta.priority_fee_per_chunk.into());
        priority_fees.push(priority_fee_irys);
    }
}
```

---

## Step 8: Migrate integration test utilities (`chain-tests/src/utils.rs`)

**File**: `crates/chain-tests/src/utils.rs`

### 8a. Update `send_pd_tx_with_fees` (line ~2510)

**Before**:
```rust
let access_list = build_pd_access_list(storage_keys);
let header = PdHeaderV1 {
    max_priority_fee_per_chunk: priority_fee.into(),
    max_base_fee_per_chunk: base_fee.into(),
};
let calldata = prepend_pd_header_v1_to_calldata(&header, &[]);
let mut tx = TxEip1559 {
    access_list,
    input: calldata,
    ...
};
```

**After**:
```rust
let access_list = build_pd_access_list_with_fees(
    storage_keys,
    std::iter::empty(),
    priority_fee.into(),
    base_fee.into(),
);
let mut tx = TxEip1559 {
    access_list,
    input: Bytes::new(),  // Clean — no PD header
    ...
};
```

### 8b. Update `inject_pd_contract_call` (line ~2606)

**Before**:
```rust
let header = PdHeaderV1 { ... };
let calldata = prepend_pd_header_v1_to_calldata(&header, &abi_calldata);
// ... manual access list construction ...
```

**After**:
```rust
let access_list = build_pd_access_list_with_fees(
    chunk_specs,
    byte_specs,
    U256::from(priority_fee_per_chunk),
    U256::from(1_000_000_000_000_000_u64),  // max base fee cap
);
let mut tx = TxEip1559 {
    access_list,
    input: abi_calldata,  // Clean ABI calldata — no prefix
    ...
};
```

### 8c. Update imports

Remove: `prepend_pd_header_v1_to_calldata`, `PdHeaderV1`, `detect_and_decode_pd_header`
Add: `build_pd_access_list_with_fees`, `parse_pd_transaction`, `PdParseResult`

---

## Step 9: Migrate PD chunk limit integration test

**File**: `crates/chain-tests/src/programmable_data/pd_chunk_limit.rs`

### 9a. Replace PD transaction construction (line ~82)

**Before**:
```rust
let header = PdHeaderV1 {
    max_priority_fee_per_chunk: U256::from(1_000_000_000_000_000_u64),
    max_base_fee_per_chunk: U256::from(1_000_000_000_000_000_u64),
};
let calldata = prepend_pd_header_v1_to_calldata(&header, &[]);
```

**After**: Use `build_pd_access_list_with_fees` and empty calldata:
```rust
let access_list = build_pd_access_list_with_fees(
    storage_keys,
    std::iter::empty(),
    U256::from(1_000_000_000_000_000_u64),
    U256::from(1_000_000_000_000_000_u64),
);
// ...
let mut tx = TxEip1559 {
    access_list,
    input: Bytes::new(),
    ...
};
```

### 9b. Remove round-trip assertion (line ~101)

```rust
let _decoded = detect_and_decode_pd_header(&tx.input)
    .expect("pd header parse error")
    .unwrap();
```

Replace with:
```rust
// Verify PD metadata parses correctly from the access list
assert!(matches!(
    parse_pd_transaction(&tx.access_list),
    PdParseResult::ValidPd(_)
));
```

---

## Step 10: Update `precompiles/pd/utils.rs`

**File**: `crates/irys-reth/src/precompiles/pd/utils.rs`

The `parse_access_list` function in the precompile only cares about `ChunkRead` and `ByteRead` — it doesn't need fee keys. However, with the new type bytes present, it must handle them gracefully.

### 10a. Update to skip fee keys

Currently the function calls `PdAccessListArg::decode()` which will now succeed for fee keys too. Update the match:

```rust
match PdAccessListArg::decode(&key.0) {
    Ok(PdAccessListArg::ChunkRead(spec)) => chunk_reads.push(spec),
    Ok(PdAccessListArg::ByteRead(spec)) => byte_reads.push(spec),
    Ok(PdAccessListArg::PdPriorityFee(_) | PdAccessListArg::PdBaseFeeCap(_)) => {
        // Fee keys are handled by the canonical parser, skip here
    }
    Err(e) => {
        tracing::warn!("Skipping invalid PD access list key: {e}");
    }
}
```

---

## Step 11: Verify no remaining references

After all migrations, run:

```bash
cargo check --workspace --tests --all-targets
grep -rn "detect_and_decode_pd_header\|prepend_pd_header_v1_to_calldata\|IRYS_PD_HEADER_MAGIC\|PdHeaderV1\|PD_HEADER_VERSION_V1\|irys-pd-meta" crates/
```

The grep should return zero results. If any remain, update them to use the new parser.

---

## Step 12: Final checks

```bash
cargo fmt --all
cargo clippy --workspace --tests --all-targets
cargo xtask test
```

---

## Callsite Migration Checklist

| File | Line(s) | Function | Action |
|------|---------|----------|--------|
| `irys-reth/src/pd_tx.rs` | 63-159 | `IRYS_PD_HEADER_MAGIC`, `PdHeaderV1`, `prepend_pd_header_v1_to_calldata`, `detect_and_decode_pd_header` | **Delete** |
| `irys-reth/src/pd_tx.rs` | 16-27, 36-59 | `build_pd_access_list`, `sum_pd_chunks_in_access_list` | **Update** to handle fee keys |
| `irys-reth/src/pd_tx.rs` | — | — | **Add** `PdParseResult`, `PdTransactionMeta`, `PdValidationError`, `parse_pd_transaction`, `build_pd_access_list_with_fees` |
| `irys-reth/src/evm.rs` | 654-750 | `transact_raw` PD block | **Rewrite**: use `parse_pd_transaction`, remove calldata stripping |
| `irys-reth/src/evm.rs` | 1810, 1938+, 2100+, 2230+, 2410+, 2570+ | Test helpers & tests | **Rewrite**: use `build_pd_access_list_with_fees`, clean calldata |
| `irys-reth/src/mempool.rs` | 265-266 | Pre-Sprite PD rejection | **Rewrite**: use `parse_pd_transaction` |
| `irys-reth/src/mempool.rs` | 282-283 | Sprite fee validation | **Rewrite**: use `parse_pd_transaction` |
| `irys-reth/src/mempool.rs` | 418 | Priority scoring | **Rewrite**: use `parse_pd_transaction` |
| `irys-reth/src/mempool.rs` | 492-493 | PD tx monitor | **Rewrite**: use `parse_pd_transaction` |
| `irys-reth/src/payload.rs` | 492 | `pd_chunks_for_transaction` | **Rewrite**: use `parse_pd_transaction` |
| `irys-reth/src/payload.rs` | 532 | `pop_ready_deferred` closure | **Rewrite**: use `parse_pd_transaction` |
| `irys-reth/src/payload.rs` | 793-798 | Test helper `pd_pooled_transaction` | **Rewrite**: use `build_pd_access_list_with_fees` |
| `actors/src/block_validation.rs` | 1450 | PD chunk budget | **Rewrite**: use `parse_pd_transaction`, reject `InvalidPd` |
| `actors/src/pd_pricing/base_fee.rs` | 280 | `count_pd_chunks_in_block` | **Rewrite**: use `parse_pd_transaction` |
| `actors/src/pd_pricing/base_fee.rs` | 307 | `extract_priority_fees_from_block` | **Rewrite**: use `parse_pd_transaction` |
| `chain-tests/src/utils.rs` | 2513-2517 | `send_pd_tx_with_fees` | **Rewrite**: use `build_pd_access_list_with_fees`, empty calldata |
| `chain-tests/src/utils.rs` | 2620-2624 | `inject_pd_contract_call` | **Rewrite**: use `build_pd_access_list_with_fees`, clean ABI calldata |
| `chain-tests/src/programmable_data/pd_chunk_limit.rs` | 82-103 | PD tx construction | **Rewrite**: use `build_pd_access_list_with_fees` |
| `irys-reth/src/precompiles/pd/utils.rs` | 26-31 | `parse_access_list` match arms | **Update**: skip fee key variants |
| `types/src/range_specifier.rs` | 12-66 | `PdAccessListArgsTypeId`, `PdAccessListArg` | **Extend**: add `PdPriorityFee(2)` and `PdBaseFeeCap(3)` variants |
| `types/src/range_specifier.rs` | — | — | **Add**: `MAX_PD_FEE`, fee encode/decode functions |

---

## Risk Notes

1. **Consensus-critical change**: Block validation now rejects `InvalidPd` transactions. Previously, a corrupted PD header in calldata would cause the tx to be treated as non-PD (silently). Now, any malformed PD access list key under `0x500` is a hard reject. This is intentional per the design spec but means blocks produced by old nodes with malformed PD txs would be rejected by new validators.

2. **`extract_pd_chunk_specs`**: Currently used in block validation (line 1455) to collect chunk specs for PD service provisioning. After the migration, `parse_pd_transaction` returns `data_reads` directly. The standalone `extract_pd_chunk_specs` function can be removed or kept as a convenience.

3. **Backward compatibility**: This is a **breaking change** (as stated in the design spec). Old-format PD transactions (with calldata prefix) will be rejected by the new parser. There is no migration period — all nodes must upgrade simultaneously (hardfork semantics).
