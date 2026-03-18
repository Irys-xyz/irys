# Design Spec: Move PD Fee Parameters from Calldata Prefix to Access List

**Date**: 2026-03-18
**Branch**: feat/pd
**Status**: Draft
**Breaking**: Yes — removes `irys-pd-meta` calldata prefix
**Depends on**: `2026-03-17-pd-precompile-abi-redesign.md` (unified `PdDataRead` specifier)

## 1. Problem

PD transactions prepend a 78-byte magic header to EVM transaction calldata:

```
[irys-pd-meta:12][version:u16][max_priority_fee_per_chunk:U256][max_base_fee_per_chunk:U256][actual_calldata...]
```

This breaks standard EVM tooling and permanently loses fee data after block inclusion.

### 1.1 What Breaks

| Tool / Layer | Impact |
|---|---|
| **ABI decoders** (etherscan, tenderly, dethcode) | Expect 4-byte function selector at calldata[0]. See `0x697279732d70642d6d657461...` ("irys-pd-meta" in hex). Fail to decode. |
| **Wallet UIs** (MetaMask, Rainbow) | Display raw hex instead of decoded function call. Users cannot verify what they're signing. |
| **SDK encoding** (ethers.js, viem, alloy) | `encodeFunctionData` produces standard ABI calldata. Prepending 78 bytes requires manual byte manipulation outside the type system. |
| **Transaction simulators** (Tenderly, Foundry `cast`) | Need custom handling to understand the prefix. |
| **Block explorers** | Cannot display PD fee parameters — they're stripped before block storage. |

### 1.2 Fee Data Is Opaque To Standard Tooling

The header is stripped during EVM execution (`evm.rs:748-750`) so the EVM sees clean calldata. The stripping only affects the temporary `TxEnv` — the block stores the original `TransactionSigned` with the PD header intact. **Fee data is not permanently lost.**

However, recovering fee parameters requires custom parsing of the magic prefix. Standard block explorers, indexers, and RPC tooling cannot decode the PD header — they see opaque bytes at the start of calldata. This means:
- Fee auditing requires custom tooling that understands the `irys-pd-meta` format
- Standard EVM indexers (etherscan, Dune, etc.) cannot extract fee parameters
- Transaction calldata appears malformed to any tool expecting a 4-byte function selector

### 1.3 Current Implementation

**Header construction** (`pd_tx.rs:115-128`):
```rust
pub fn prepend_pd_header_v1_to_calldata(header: &PdHeaderV1, rest: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(
        IRYS_PD_HEADER_MAGIC.len() + PD_HEADER_VERSION_SIZE + PD_HEADER_V1_SIZE + rest.len(),
    );
    out.extend_from_slice(IRYS_PD_HEADER_MAGIC);  // b"irys-pd-meta" (12 bytes)
    out.extend_from_slice(&PD_HEADER_VERSION_V1.to_be_bytes());  // u16 (2 bytes)
    header.serialize(&mut buf)?;  // 2 × U256 borsh (64 bytes)
    out.extend_from_slice(&buf);
    out.extend_from_slice(rest);  // actual contract calldata
    out.into()
}
```

**Header detection** — used in 5+ locations across the codebase:
- `mempool.rs:266,283,418` — mempool validation, fee extraction
- `payload.rs:492,536` — payload builder chunk budgeting
- `evm.rs:654` — EVM execution, fee deduction, calldata stripping

**PdHeaderV1** (`pd_tx.rs:78-84`):
```rust
pub struct PdHeaderV1 {
    pub max_priority_fee_per_chunk: U256,
    pub max_base_fee_per_chunk: U256,
}
```

## 2. Proposed Solution

Move the two fee parameters into the EIP-2930 access list as typed storage keys under `PD_PRECOMPILE_ADDRESS`, using the same type-byte convention as `PdDataRead` specifiers. Remove the magic calldata prefix entirely.

### 2.1 Access List Key Layout

Two new access list key types are added alongside the `PdDataRead` specifier (`0x01`):

```
PdPriorityFee (type = 0x02):
┌──────┬────────────────────────────────────────────────────────┐
│ 0x02 │ max_priority_fee_per_chunk (U256, 31 bytes, BE)       │
│  1B  │                    31B                                 │
└──────┴────────────────────────────────────────────────────────┘

PdBaseFeeCap (type = 0x03):
┌──────┬────────────────────────────────────────────────────────┐
│ 0x03 │ max_base_fee_per_chunk (U256, 31 bytes, BE)           │
│  1B  │                    31B                                 │
└──────┴────────────────────────────────────────────────────────┘
```

Each fee parameter fits in one 32-byte storage key: 1 byte type discriminant + 31 bytes for the value.

**Fee values are protocol-typed as `u248` (not `U256`).** The 31-byte encoding supports values up to `2^248 - 1`. Values >= `2^248` are rejected at validation time — not silently truncated. This is far beyond any realistic token amount (IRYS has 18 decimals; even 10^30 tokens fits in ~100 bits).

```rust
pub const MAX_PD_FEE: U256 = U256::from_be_bytes([
    0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
    0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
]); // 2^248 - 1
```

### 2.2 Complete Access List Structure

A PD transaction's access list now looks like:

```
AccessListItem {
    address: 0x500 (PD_PRECOMPILE_ADDRESS),
    storage_keys: [
        [0x01][partition(8)][start(4)][len(4)][byte_off(3)][reserved(12)],  ← PdDataRead
        [0x01][partition(8)][start(4)][len(4)][byte_off(3)][reserved(12)],  ← PdDataRead
        [0x02][max_priority_fee_per_chunk (31 bytes BE)],                    ← PdPriorityFee
        [0x03][max_base_fee_per_chunk (31 bytes BE)],                        ← PdBaseFeeCap
    ]
}
```

**Type byte summary:**

| Type | Name | Purpose |
|---|---|---|
| `0x01` | `PdDataRead` | Chunk range + byte range specifier |
| `0x02` | `PdPriorityFee` | User's offered priority fee per chunk |
| `0x03` | `PdBaseFeeCap` | User's maximum acceptable base fee per chunk |

### 2.3 Detection & Parsing: Single Canonical Parser

Currently, PD transactions are detected by the magic prefix `b"irys-pd-meta"` at the start of calldata, with 5+ separate call sites each doing their own parsing.

With this change, **one canonical parser** handles detection, validation, and extraction. It returns a tri-state result and is called from every system component (mempool, payload builder, block validation, EVM execution):

```rust
pub enum PdParseResult {
    /// No PD keys found under PD_PRECOMPILE_ADDRESS. Not a PD transaction.
    NotPd,
    /// Valid PD transaction with extracted metadata.
    ValidPd(PdTransactionMeta),
    /// Keys found under PD_PRECOMPILE_ADDRESS but validation failed.
    InvalidPd(PdValidationError),
}

pub struct PdTransactionMeta {
    pub data_reads: Vec<PdDataRead>,
    pub priority_fee_per_chunk: U256,
    pub base_fee_cap_per_chunk: U256,
    pub total_chunks: u64,
}
```

**Classification rules:**
- If no `AccessListItem` targets `PD_PRECOMPILE_ADDRESS` → `NotPd`
- If any key under `PD_PRECOMPILE_ADDRESS` exists but validation fails → `InvalidPd`
- If all validation passes → `ValidPd(meta)`

`InvalidPd` transactions are **rejected** at every stage — mempool admission, payload building, and block validation. They are never silently downgraded to `NotPd`.

### 2.4 Validation Rules (consensus-critical)

All rules are enforced by the single canonical parser. Violation of any rule makes the transaction `InvalidPd`.

| Rule | Rationale |
|---|---|
| **Exactly one `AccessListItem` for `PD_PRECOMPILE_ADDRESS`** | Multiple items targeting 0x500 add ambiguity. Reject if more than one. |
| Exactly one `PdPriorityFee` key (`0x02`) | Multiple fee declarations are ambiguous. Duplicates rejected. |
| Exactly one `PdBaseFeeCap` key (`0x03`) | Same. |
| At least one `PdDataRead` key (`0x01`) | Fee-only PD metadata with no reads is invalid. |
| Both fee keys required if any `PdDataRead` keys present | A PD read without fee declaration is invalid. |
| Fee values > 0 | Zero fees rejected (existing behavior). |
| Fee values <= `MAX_PD_FEE` (2^248 - 1) | Explicit u248 bound. Values >= 2^248 rejected, not silently truncated. |
| No unknown type bytes | Any storage key under `PD_PRECOMPILE_ADDRESS` with type byte not in {0x01, 0x02, 0x03} makes the transaction `InvalidPd`. No skipping. |
| No duplicate `PdDataRead` keys | Duplicate specifiers inflate chunk counting. Reject exact duplicates. |
| Order-independent | Fee keys and data-read keys can appear in any position. Parsed by type byte, not position. |
| All `PdDataRead` keys individually valid | Each key must pass `PdDataRead::decode()` with its validity rules (see companion spec). |

### 2.5 Encoding / Decoding

```rust
/// Encode a U256 fee into a 32-byte access list key with type prefix.
/// Panics if fee > MAX_PD_FEE (2^248 - 1). Use `try_encode_fee` for fallible version.
fn encode_fee(type_byte: u8, fee: U256) -> [u8; 32] {
    assert!(fee <= MAX_PD_FEE, "fee exceeds u248 max ({fee})");
    let mut buf = [0u8; 32];
    buf[0] = type_byte;
    let be_bytes = fee.to_be_bytes::<32>();
    buf[1..32].copy_from_slice(&be_bytes[1..32]);
    buf
}

/// Decode a U256 fee from a 32-byte access list key.
/// Rejects values >= 2^248 (the top byte of the U256 is always 0 in valid encodings).
fn decode_fee(bytes: &[u8; 32], expected_type: u8) -> eyre::Result<U256> {
    eyre::ensure!(bytes[0] == expected_type, "unexpected type byte");
    let mut be_bytes = [0u8; 32];
    // Top byte stays 0 — this enforces the u248 bound by construction.
    be_bytes[1..32].copy_from_slice(&bytes[1..32]);
    let fee = U256::from_be_bytes(be_bytes);
    eyre::ensure!(fee > U256::ZERO, "fee must be > 0");
    Ok(fee)
}
```

The u248 bound is enforced at both encode (assert) and decode (top byte forced to 0). There is no silent truncation path — any U256 > `MAX_PD_FEE` is rejected before encoding.

### 2.6 Transaction Construction (Before vs After)

**Before** — calldata prefix:
```rust
let header = PdHeaderV1 {
    max_priority_fee_per_chunk: U256::from(1_000_000),
    max_base_fee_per_chunk: U256::from(5_000_000),
};
let calldata = prepend_pd_header_v1_to_calldata(&header, &contract_abi_calldata);
// calldata is now [irys-pd-meta...78 bytes...][actual ABI calldata]

let access_list = build_pd_access_list(chunk_specs);
```

**After** — clean calldata, fees in access list:
```rust
let mut storage_keys: Vec<B256> = chunk_specs
    .into_iter()
    .map(|spec| B256::from(spec.encode()))
    .collect();

// Fee params as access list keys
storage_keys.push(B256::from(encode_fee(0x02, U256::from(1_000_000))));
storage_keys.push(B256::from(encode_fee(0x03, U256::from(5_000_000))));

let access_list = AccessList::from(vec![AccessListItem {
    address: PD_PRECOMPILE_ADDRESS,
    storage_keys,
}]);

// calldata is just the contract ABI calldata — no prefix
let calldata = contract_abi_calldata;
```

### 2.7 What This Fixes

| Issue | Before | After |
|---|---|---|
| ABI decoding | Broken — prefix confuses decoders | Works — calldata is standard ABI |
| Wallet UIs | Show hex garbage | Decode function call normally |
| Fee visibility on-chain | Preserved but opaque — requires custom parser to extract from calldata prefix | Transparent — typed access list keys, decodable by any tool that knows the type bytes |
| Fee auditing | Requires custom tooling for the `irys-pd-meta` format | Standard access list decoding |
| Transaction replay | Works but calldata contains non-ABI prefix | Standard — calldata is clean ABI |
| SDK integration | Manual byte manipulation | Standard `AccessList` construction |
| Detection complexity | Magic byte matching on calldata | Check for PD keys in access list |
| Calldata stripping | `tx.data` mutated during execution | No mutation needed |

## 3. Gas Cost

| Component | Gas |
|---|---|
| Additional storage keys (2 × 1,900) | +3,800 |
| Saved: no calldata prefix (78 bytes, mix of zero/nonzero) | -540 to -1,236 |
| **Net cost** | **~2,564 to ~3,260 gas** |

Calldata gas depends on byte values: 16 gas per nonzero byte, 4 gas per zero byte. The old 78-byte header contains the magic string (all nonzero, 12 × 16 = 192 gas), version field (1 zero + 1 nonzero = 20 gas), and two U256 fee values (mostly zeros for small fees). Worst case (all nonzero): 78 × 16 = 1,248 gas saved. Typical case with small fees: ~540 gas saved.

Either way, negligible — ~0.01% of a 30M block.

## 4. Affected Code

### 4.1 Remove

| File | What to remove |
|---|---|
| `crates/irys-reth/src/pd_tx.rs` | `IRYS_PD_HEADER_MAGIC`, `PD_HEADER_VERSION_V1`, `PdHeaderV1` struct, `prepend_pd_header_v1_to_calldata()`, `detect_and_decode_pd_header()` |
| `crates/irys-reth/src/evm.rs` | Calldata stripping logic (`tx.data = stripped` at line 750), PD header detection in `transact_raw()` |

### 4.2 Add / Update

| File | Change |
|---|---|
| `crates/types/src/range_specifier.rs` | Add `PdPriorityFee` and `PdBaseFeeCap` encode/decode alongside `PdDataRead` |
| `crates/irys-reth/src/pd_tx.rs` | New `parse_pd_transaction()` → `PdParseResult` canonical parser (single entrypoint for all components). New `build_pd_access_list_with_fees()` helper. Remove `detect_and_decode_pd_header()`, `prepend_pd_header_v1_to_calldata()`, `PdHeaderV1`. |
| `crates/irys-reth/src/evm.rs` | Replace `detect_and_decode_pd_header()` with `parse_pd_transaction()`. Remove calldata mutation. Fee deduction logic unchanged except source of fee values. |
| `crates/irys-reth/src/mempool.rs` | Replace all `detect_and_decode_pd_header()` calls with `parse_pd_transaction()`. Reject `InvalidPd` at admission. |
| `crates/irys-reth/src/payload.rs` | Replace `detect_and_decode_pd_header()` with `parse_pd_transaction()`. |
| `crates/chain-tests/src/utils.rs` | Update all PD transaction construction helpers. |
| All chain-tests PD tests | Remove `prepend_pd_header_v1_to_calldata` usage. |

### 4.3 Unchanged

| Component | Why |
|---|---|
| `PdDataRead` specifier | Orthogonal — chunk specifiers are unaffected |
| Precompile dispatch | The precompile only reads chunk specifiers, not fee params |
| Fee deduction logic | Same math: `(base_fee + priority_fee) × chunks`. Only the source of fee values changes. |
| Chunk provisioning (PdService) | Only reads chunk specifiers from access list — already doesn't touch calldata |

## 5. Risks

| Risk | Severity | Mitigation |
|---|---|---|
| Fee values capped at u248 (2^248 - 1) | Negligible | Explicit protocol type. Rejected at encode and decode, never silently truncated. 2^248 is astronomically beyond any realistic fee. |
| Extra 2 access list keys per tx | Negligible | ~3,260 gas net cost, ~0.01% of block. |
| Malicious access list keys under 0x500 | Low | Strict canonical parser rejects unknown type bytes, duplicates, and missing fee keys. `InvalidPd` is rejected at every stage. |
| Multiple `AccessListItem`s targeting 0x500 | Low | Consensus rule: exactly one allowed. Rejected at validation. |
| Existing test infrastructure depends on calldata prefix | Medium | All usages found, bounded scope. Replace with `build_pd_access_list_with_fees()`. |
| Access list becomes the single source for PD metadata | Low | This is the desired outcome — single location, permanently stored, standard format. |
