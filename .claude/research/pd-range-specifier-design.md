# Research: PD Access List Range Specifier Design

**Date**: 2026-03-17
**Git Commit**: f0c9a860
**Branch**: feat/pd

## Research Question

How do ChunkRangeSpecifier and ByteRangeSpecifier encode data access requests into 32-byte EVM access list storage keys? How do they flow through the system from transaction construction to precompile execution? Is the design logically expressive and ergonomic?

## Summary

PD uses two complementary 32-byte specifiers packed into EIP-2930 access list storage keys. **ChunkRangeSpecifier** declares which chunks the node must prefetch (used before EVM execution). **ByteRangeSpecifier** declares which byte slice the precompile should return (used during EVM execution). ByteRangeSpecifier references a ChunkRangeSpecifier by positional index, creating a cross-reference between the two.

---

## 1. Binary Encoding Layout

### 1.1 ChunkRangeSpecifier (type_id = 0x00)

Declares a contiguous range of chunks from a partition. Used by the node for prefetching and by the payload builder for chunk budgeting.

```
Byte 0:       0x00 (type discriminant)
Bytes 1-25:   partition_index (U200, 25 bytes, LE)
Bytes 26-29:  offset (u32, 4 bytes, LE) — chunk offset within partition
Bytes 30-31:  chunk_count (u16, 2 bytes, LE) — consecutive chunk count
              ────────────────────────────────────────────
              Total: 32 bytes, 0 unused
```

All 32 bytes are used. Endianness is little-endian throughout.

**Field capacities:**

| Field | Type | Max value | Meaning |
|---|---|---|---|
| partition_index | U200 | 2^200 - 1 | Partition identifier |
| offset | u32 | 4,294,967,295 | Chunk offset within partition |
| chunk_count | u16 | 65,535 | Consecutive chunks to prefetch |

**Defined at**: `crates/types/src/range_specifier.rs:103-134`

### 1.2 ByteRangeSpecifier (type_id = 0x01)

Declares a byte slice to extract from prefetched chunks. References a ChunkRangeSpecifier by position.

```
Byte 0:       0x01 (type discriminant)
Byte 1:       index (u8) — positional reference to ChunkRangeSpecifier
Bytes 2-3:    chunk_offset (u16, 2 bytes, LE) — relative chunk offset
Bytes 4-6:    byte_offset (U18, 3 bytes, LE) — byte offset within chunk
Bytes 7-11:   length (U34, 5 bytes, LE) — bytes to read
Bytes 12-31:  UNUSED (20 bytes of zeros)
              ────────────────────────────────────────────
              Total: 32 bytes, 20 unused (62.5%)
```

**Field capacities:**

| Field | Type | Max value | Meaning |
|---|---|---|---|
| index | u8 | 255 | References ChunkRangeSpecifier by position |
| chunk_offset | u16 | 65,535 | Offset from start of chunk range (in chunks) |
| byte_offset | U18 | 262,143 | Byte offset within a chunk (= chunk_size - 1) |
| length | U34 | 17,179,869,183 | Bytes to read (~17 GB) |

**Defined at**: `crates/types/src/range_specifier.rs:207-268`

### 1.3 Custom Integer Types

```rust
pub type U34 = Uint<34, 1>;  // 34-bit unsigned, stored in 5 bytes
pub type U18 = Uint<18, 1>;  // 18-bit unsigned, stored in 3 bytes
```

From the `ruint` crate. `Uint<BITS, LIMBS>` provides exact-width unsigned integers.

**Defined at**: `crates/types/src/range_specifier.rs:204-205`

### 1.4 Encoding Trait

Both specifiers implement `PdAccessListArgSerde`:

```rust
pub trait PdAccessListArgSerde {
    fn encode(&self) -> [u8; 32] {
        let mut bytes = [0_u8; 32];
        bytes[0] = Self::get_type() as u8;     // type discriminant
        bytes[1..].copy_from_slice(&self.encode_inner()); // 31 inner bytes
        bytes
    }
    fn decode(bytes: &[u8; 32]) -> eyre::Result<Self> { ... }
    fn get_type() -> PdAccessListArgsTypeId;
    fn encode_inner(&self) -> [u8; 31];
    fn decode_inner(bytes: &[u8; 31]) -> eyre::Result<Self>;
}
```

The parent `PdAccessListArg` enum dispatches decode based on `bytes[0]`:

```rust
pub enum PdAccessListArg {
    ChunkRead(ChunkRangeSpecifier),
    ByteRead(ByteRangeSpecifier),
}
```

**Defined at**: `crates/types/src/range_specifier.rs:34-101`

---

## 2. System Flow: Transaction Construction to Chunk Delivery

### 2.1 Transaction Construction

Users build access lists with both specifier types:

```rust
// From crates/chain-tests/src/programmable_data/basic.rs:268-290
let access_list = vec![AccessListItem {
    address: precompile_address,
    storage_keys: vec![
        ChunkRangeSpecifier {
            partition_index: U200::from(0),
            offset: 0,
            chunk_count: 1,
        }.encode().into(),
        ByteRangeSpecifier {
            index: 0,              // references the ChunkRange above
            chunk_offset: 0,
            byte_offset: U18::from(0),
            length: U34::from(data_bytes.len()),
        }.encode().into(),
    ],
}];
```

The PD transaction header (`PdHeaderV1`) with fee parameters is prepended to calldata separately.

**Helper**: `build_pd_access_list()` in `crates/irys-reth/src/pd_tx.rs:14-25`

### 2.2 Mempool Detection & Chunk Prefetching

When a PD transaction enters the mempool:

1. **Monitor detects PD header** via `detect_and_decode_pd_header()` on calldata
2. **Extracts ChunkRangeSpecifiers** via `extract_pd_chunk_specs()` — only ChunkRead entries
3. **Sends to PdService** which converts specs to ledger offsets:
   ```
   ledger_offset = (partition_index × num_chunks_in_partition) + offset + i
   for i in 0..chunk_count
   ```
4. **PdService fetches chunks** from storage, populates `ChunkDataIndex` (lock-free `DashMap`)
5. **Marks transaction as ready** in `ready_pd_txs` DashSet

**Key files**: `crates/irys-reth/src/mempool.rs:444-538`, `crates/actors/src/pd_service.rs:96-266`

### 2.3 Payload Builder Chunk Budgeting

During block assembly:

1. `sum_pd_chunks_in_access_list()` counts total chunks from ChunkRead entries
2. `PdChunkBudget` tracks usage against `MAX_PD_CHUNKS_PER_BLOCK` (7,500)
3. PD transactions that exceed the budget are deferred to next block
4. Only transactions in `ready_pd_txs` (chunks prefetched) are eligible

**Key file**: `crates/irys-reth/src/payload.rs:390-583`

### 2.4 Block Validation

Validating nodes:

1. Extract all ChunkRangeSpecifiers from block transactions
2. Verify `total_pd_chunks <= MAX_PD_CHUNKS_PER_BLOCK`
3. Provision chunks locally via PdService
4. Reject block if chunks unavailable

**Key file**: `crates/actors/src/block_validation.rs:1450-1519`

### 2.5 Precompile Execution

When the contract calls the precompile:

1. **Parse access list**: `parse_access_list()` separates into `chunk_reads[]` and `byte_reads[]`
2. **Look up byte range**: `byte_reads[calldata.index]` gets the ByteRangeSpecifier
3. **Cross-reference chunk range**: `chunk_reads[byte_range.index]` gets the ChunkRangeSpecifier
4. **Calculate ledger range**: `calculate_chunk_offsets(partition_index, offset, chunk_count, num_chunks_in_partition)` → ledger range `[start, end)`
5. **Fetch chunks**: For each offset in range, look up in `ChunkDataIndex` via `PdContext`
6. **Concatenate**: All chunk bytes into a single buffer
7. **Calculate byte offset**: `(chunk_offset × chunk_size) + byte_offset` → absolute position
8. **Extract slice**: `buffer[absolute_offset..absolute_offset + length]`
9. **Return**: Raw bytes (or ABI-encoded bytes after the redesign)

**Key file**: `crates/irys-reth/src/precompiles/pd/read_bytes.rs:232-351`

### 2.6 The `translate_offset` Method

For partial byte range reads (`ReadPartialByteRange`), the user-supplied offset from calldata is applied to the ByteRangeSpecifier:

```rust
impl ByteRangeSpecifier {
    pub fn translate_offset(&mut self, chunk_size: u64, offset: u64) -> eyre::Result<()> {
        let full_offset = offset + self.byte_offset;
        let additional_chunks = full_offset / chunk_size;
        self.chunk_offset += additional_chunks as u16;
        self.byte_offset = U18::try_from(full_offset % chunk_size)?;
        Ok(())
    }
}
```

This adjusts the specifier so that reading "100 bytes from offset 300" within a byte range correctly crosses chunk boundaries.

**Defined at**: `crates/types/src/range_specifier.rs:247-267`

---

## 3. Cross-Reference Mechanism

The `ByteRangeSpecifier.index` field references a `ChunkRangeSpecifier` by its position in the parsed access list's `chunk_reads` array. This creates an implicit ordering dependency:

```
Access list storage keys (in order):
  [0] ChunkRange { partition: 0, offset: 0, count: 4 }   ← chunk_reads[0]
  [1] ChunkRange { partition: 0, offset: 100, count: 2 }  ← chunk_reads[1]
  [2] ByteRange { index: 0, ... }                          ← references chunk_reads[0]
  [3] ByteRange { index: 1, ... }                          ← references chunk_reads[1]
```

The parse step (`parse_access_list`) filters by type, so `chunk_reads` only contains ChunkRead entries in their original order. The `index` field is a positional reference into this filtered list.

---

## 4. Design Observations

### 4.1 Space Utilization

| Specifier | Used bytes | Unused bytes | Utilization |
|---|---|---|---|
| ChunkRangeSpecifier | 32/32 | 0 | 100% |
| ByteRangeSpecifier | 12/32 | 20 | 37.5% |

### 4.2 Field Sizing vs Practical Limits

| Field | Type capacity | Practical limit | Ratio |
|---|---|---|---|
| partition_index (U200) | 2^200 partitions | Unknown future scale | Extremely large |
| offset (u32) | 4.3B chunks/partition | Partition size dependent | Reasonable |
| chunk_count (u16) | 65,535 | 7,500/block | 8.7× headroom |
| index (u8) | 256 byte ranges | Practical limit ~10s | 25× headroom |
| chunk_offset (u16) | 65,535 chunks | = chunk_count max | Matched |
| byte_offset (U18) | 262,143 bytes | 262,144 chunk_size | Exact fit (chunk_size - 1) |
| length (U34) | ~17 GB | ~1.9 GB/block | 9× headroom |

### 4.3 Endianness

All fields use little-endian encoding. EVM natively uses big-endian for its 256-bit words. This means Solidity contracts cannot easily decode these specifiers — they're opaque B256 values constructed off-chain. This is acceptable since contracts don't need to read or construct access list keys.

---

## Code References

- `crates/types/src/range_specifier.rs` — Type definitions, encoding/decoding, translate_offset
- `crates/irys-reth/src/pd_tx.rs` — Transaction construction helpers, chunk counting
- `crates/irys-reth/src/precompiles/pd/utils.rs` — Access list parsing into ParsedAccessLists
- `crates/irys-reth/src/precompiles/pd/read_bytes.rs` — Precompile read implementations
- `crates/irys-reth/src/precompiles/pd/context.rs` — PdContext with ChunkDataIndex
- `crates/irys-reth/src/mempool.rs:444-538` — Mempool PD detection and chunk provisioning trigger
- `crates/actors/src/pd_service.rs:96-266` — Chunk prefetching and caching
- `crates/irys-reth/src/payload.rs:390-583` — Payload builder PD chunk budget
- `crates/actors/src/block_validation.rs:1450-1519` — Block validation PD checks
- `crates/chain-tests/src/programmable_data/basic.rs:268-290` — Integration test showing access list construction

---

## 5. Codex Second Opinion (gpt-5.4)

### 5.1 Overall Assessment

> The design is deterministic for a given signed transaction, but it is more fragile than it needs to be. The main issues are:
> - semantics depend on filtered access-list ordering, not just field values
> - the public encoding advertises much larger ranges than the runtime actually supports
> - ByteRangeSpecifier leaves 20 bytes semantically undefined today
> - the one-byte indices quietly cap you at 256 byte ranges and 256 referenced chunk ranges

### 5.2 Per-Question Analysis

**1. Cross-reference by positional index — fragile?**

Codex confirms it's deterministic (transaction bytes preserve storage-key order, parser preserves encounter order). But it's **fragile because any tooling that reorders keys before signing changes meaning silently**. The index refers to the filtered `ChunkRange` subsequence, not raw access-list position — this is implicit, not documented as protocol spec.

Recommendation: **Remove the cross-reference entirely with a unified specifier** (preferred) or add an explicit `range_id` field in both specifiers (acceptable).

**2. 20 unused bytes — concern?**

Not a problem because they're "wasted" — a problem because they're **undefined**. Decode ignores them, so multiple distinct 32-byte keys encode the same semantic range. This weakens canonicality and makes future upgrades dangerous.

Recommendation: **Require all reserved bytes to be zero and reject nonzero values.**

**3. U200 partition_index — overkill?**

Codex found a concrete mismatch: the runtime converts `partition_index` to `u64` in provisioning and execution (via `try_into()` in `read_bytes.rs`). The actual usable range is bounded by `u64 / num_chunks_in_partition`. With `num_chunks_in_partition = 51,872,000`, that's **~355 billion, or about 38 bits**.

Recommendation: **Use `u64` to match system reality.** Even `u48` would be plenty. Do not expose `U200` unless the storage-addressing stack is actually moving to support it.

**4. chunk_count u16 vs block limit?**

13 bits would suffice for the 7,500 block limit. But since `u16` is byte-aligned and shrinking doesn't save a byte, **keep `u16` in a byte-oriented encoding**. In a repacked v2, could use 13 bits or derive chunk span from `length`.

**5. byte_offset U18 tight coupling?**

Acceptable **only if chunk_size is a versioned protocol constant**. If chunk size could evolve, this breaks silently.

Recommendation: **Define this format as v1, valid only for `chunk_size = 256 KiB`.** New chunk sizes require a new version.

**6. length U34 oversized?**

A full 7,500-chunk block is ~1.97 GB, which fits in 31 bits. The Solidity ABI already uses `uint32`.

Recommendation: **Use `u32`. U34 buys nothing material.**

**7. Unified specifier proposal**

Codex proposes a single `ReadSpecifierV1`:
```
Byte 0:       type/version
Bytes 1-8:    partition_index (u64)
Bytes 9-12:   start_chunk (u32)
Bytes 13-15:  byte_offset (u24)
Bytes 16-19:  length (u32)
Bytes 20-31:  reserved, must be zero
```

Node prefetch rule: `chunks_needed = ceil((byte_offset + length) / chunk_size)`, prefetch `[start_chunk, start_chunk + chunks_needed)`.

Why preferred:
- No cross-reference fragility
- No separate "what to prefetch" vs "what to return" sources of truth
- Fewer semantic invariants
- Simpler off-chain tooling

The two-key design is only better if reusable named chunk windows are first-class objects.

**8. Endianness**

Little-endian is not a consensus bug but a **tooling and ergonomics tax**. Especially awkward because the precompile calldata path is big-endian while access-list specifiers are little-endian.

Recommendation: For any new format, **prefer big-endian/network order**.

### 5.3 Codex Bottom Line

> The biggest design mistake is not "unused bytes"; it is **letting ordering carry meaning when the bytes have room to carry identity directly**.

If starting fresh: ship a v2 unified specifier, deprecate the two-specifier model.

If keeping the split: require explicit spec text for index semantics, switch to `range_id` cross-reference, clamp `partition_index` to actual bounds, enforce zero-reserved bytes, use `u32` length, version-lock to chunk size.

---

## 6. Claude's Take

I largely agree with Codex's analysis. Key points where I'd add nuance:

1. **Unified specifier is compelling** but has one tradeoff Codex doesn't fully address: with a single specifier, if a contract wants to read 3 different byte ranges from the same chunk window, it must repeat the chunk identification (partition_index + start_chunk) in each specifier. With the current two-key design, you declare the chunk window once and reference it 3 times. For typical usage (1 byte range per chunk range), the unified design wins. For multi-view-over-same-data patterns, it adds redundancy.

2. **The `u24` byte_offset in Codex's proposal** (max 16,777,215) is much larger than needed for a 256KB chunk (262,143 max). This is fine for forward compatibility with larger chunk sizes, but wastes 6 bits compared to U18. Minor point.

3. **The partition_index finding is the most important insight**: the U200 field advertises 2^200 partitions but the runtime rejects anything above u64 at `read_bytes.rs:30-35`. This is a correctness issue — the encoding should match the actual system capability.

---

## 7. Concrete Comparison: Current v1 vs Proposed v2

### 7.1 Encoding Layout Side-by-Side

**Current: Two specifiers (2 × 32 bytes minimum per read)**

```
ChunkRangeSpecifier (32 bytes) — WHAT TO PREFETCH
┌──────┬─────────────────────────────────────┬──────────────┬─────────────┐
│ 0x00 │ partition_index (U200, 25B, LE)     │ offset (u32) │ count (u16) │
│  1B  │              25 bytes               │    4 bytes   │   2 bytes   │
└──────┴─────────────────────────────────────┴──────────────┴─────────────┘
  Used: 32/32 (100%)

ByteRangeSpecifier (32 bytes) — WHAT BYTES TO RETURN
┌──────┬───────┬──────────────┬──────────────┬────────────┬──────────────────┐
│ 0x01 │ index │ chunk_offset │ byte_offset  │   length   │     UNUSED       │
│  1B  │  1B   │   2B (LE)    │  3B U18 (LE) │ 5B U34(LE) │    20 bytes      │
└──────┴───────┴──────────────┴──────────────┴────────────┴──────────────────┘
  Used: 12/32 (37.5%)    Wasted: 20 bytes
```

**Proposed: Single unified specifier (1 × 32 bytes per read)**

```
ReadSpecifierV2 (32 bytes) — WHAT TO PREFETCH AND RETURN
┌──────┬──────────────────────┬──────────────┬──────────────┬────────────┬──────────────────┐
│ 0x02 │ partition_index (u64)│ start_chunk  │ byte_offset  │   length   │ reserved (zero)  │
│  1B  │     8 bytes (BE)     │  4B u32 (BE) │  3B u24 (BE) │ 4B u32(BE) │    12 bytes      │
└──────┴──────────────────────┴──────────────┴──────────────┴────────────┴──────────────────┘
  Used: 20/32 (62.5%)    Reserved: 12 bytes (must be zero, validated)
```

### 7.2 Field-by-Field Comparison

| Concern | Current v1 | Proposed v2 | Change |
|---|---|---|---|
| **Keys per read** | 2 (ChunkRange + ByteRange) | 1 (unified) | Halved |
| **partition_index** | U200 (25 bytes) — runtime rejects > u64 | u64 (8 bytes) — matches runtime | 17 bytes reclaimed, honest |
| **chunk offset** | u32 in ChunkRange | u32 `start_chunk` | Same capacity |
| **chunk count** | u16 explicit field | Derived: `ceil((byte_offset + length) / chunk_size)` | Eliminated, computed |
| **byte_offset** | U18 (3 bytes, max 262,143) | u24 (3 bytes, max 16,777,215) | Same size, larger range, forward-compatible |
| **length** | U34 (5 bytes, max ~17 GB) | u32 (4 bytes, max ~4 GB) | 1 byte saved, still 2× block max |
| **Cross-reference** | ByteRange.index → ChunkRange by position | None — self-contained | Eliminated |
| **Unused bytes** | 20 bytes (undefined, not validated) | 12 bytes (reserved, must be zero, rejected if nonzero) | Defined, canonical |
| **Endianness** | Little-endian | Big-endian (network order) | Matches EVM convention |
| **Custom int types** | U18, U34 (ruint) | u24, u32 (standard) | Simpler, no ruint dependency for specifiers |
| **Type discriminant** | 0x00 / 0x01 | 0x02 | New version, no ambiguity with v1 |

### 7.3 Capacity Comparison

| Dimension | Current v1 | Proposed v2 | Sufficient? |
|---|---|---|---|
| Addressable partitions | 2^200 (~1.6 × 10^60) | 2^64 (~1.8 × 10^19) | Yes — runtime already limited to u64 |
| Chunks per partition | 2^32 (~4.3 billion) | 2^32 (~4.3 billion) | Same |
| Chunk count per specifier | 2^16 (65,535) explicit | Derived from byte_offset + length | Same effective range |
| Byte offset within chunk | 2^18 - 1 (262,143) | 2^24 - 1 (16,777,215) | v2 handles chunks up to 16 MB |
| Read length | 2^34 - 1 (~17 GB) | 2^32 - 1 (~4 GB) | Yes — block max is ~1.9 GB |
| Byte ranges per tx | 256 (u8 index) | Unlimited (1 key per read) | Better |
| Chunk ranges per tx | 256 (u8 index) | N/A — no separate chunk ranges | Simpler |

### 7.4 Prefetch Derivation in v2

The node no longer needs a separate ChunkRangeSpecifier. It derives the chunk window from the unified specifier:

```
Given: partition_index, start_chunk, byte_offset, length, chunk_size

chunks_needed = ceil((byte_offset + length) / chunk_size)
// edge case: if byte_offset + length == 0, chunks_needed = 0

prefetch range: [start_chunk, start_chunk + chunks_needed)
ledger offsets: for each chunk in range:
    ledger_offset = (partition_index × num_chunks_in_partition) + start_chunk + i
```

The precompile applies the same logic, fetches the chunks, and extracts `bytes[byte_offset .. byte_offset + length]` from the concatenated buffer.

### 7.5 Transaction Construction Comparison

**Current v1** — must construct and coordinate two specifier types:
```rust
let access_list = vec![AccessListItem {
    address: PD_PRECOMPILE_ADDRESS,
    storage_keys: vec![
        // Chunk range — declares prefetch window
        ChunkRangeSpecifier {
            partition_index: U200::from(0),
            offset: 0,
            chunk_count: 1,
        }.encode().into(),
        // Byte range — references chunk range by position (index: 0)
        ByteRangeSpecifier {
            index: 0,           // ← fragile positional reference
            chunk_offset: 0,
            byte_offset: U18::from(0),
            length: U34::from(100),
        }.encode().into(),
    ],
}];
```

**Proposed v2** — single self-contained specifier:
```rust
let access_list = vec![AccessListItem {
    address: PD_PRECOMPILE_ADDRESS,
    storage_keys: vec![
        ReadSpecifierV2 {
            partition_index: 0_u64,
            start_chunk: 0,
            byte_offset: 0_u32,  // u24 stored as u32, top byte must be 0
            length: 100,
        }.encode().into(),
    ],
}];
```

### 7.6 Multi-Read Comparison

**Scenario**: Read 3 different byte ranges from the same 4-chunk window.

**Current v1** — 4 access list keys (1 chunk range + 3 byte ranges):
```
Key 0: ChunkRange { partition: 5, offset: 100, count: 4 }
Key 1: ByteRange  { index: 0, chunk_offset: 0, byte_offset: 0,      length: 1000 }
Key 2: ByteRange  { index: 0, chunk_offset: 1, byte_offset: 50000,  length: 500  }
Key 3: ByteRange  { index: 0, chunk_offset: 3, byte_offset: 200000, length: 2000 }
```
Total: 4 × 32 = 128 bytes in access list

**Proposed v2** — 3 access list keys (each self-contained):
```
Key 0: ReadV2 { partition: 5, start: 100, byte_offset: 0,      length: 1000 }
Key 1: ReadV2 { partition: 5, start: 101, byte_offset: 50000,  length: 500  }
Key 2: ReadV2 { partition: 5, start: 103, byte_offset: 200000, length: 2000 }
```
Total: 3 × 32 = 96 bytes in access list

v2 uses **fewer bytes** even in the multi-read case, because the ChunkRange overhead (32 bytes) exceeds the repeated partition_index + start_chunk (12 bytes × 3 = 36 bytes, but saved 32 bytes by eliminating the ChunkRange key).

**When v1 wins**: Only if you have >3 byte ranges sharing the exact same chunk window. At 4+ byte ranges from one window, v1 saves 32 bytes per additional range. In practice, most PD transactions read 1-2 ranges.

### 7.7 System Impact Summary

| Component | v1 Impact | v2 Impact |
|---|---|---|
| **Transaction construction** | Must coordinate 2 specifier types, manage positional cross-references | Single specifier, self-contained |
| **Mempool monitor** | Extracts ChunkRangeSpecifiers only | Extracts ReadSpecifierV2, derives chunk window |
| **PdService prefetch** | Reads chunk_count from ChunkRange | Computes `ceil((byte_offset + length) / chunk_size)` |
| **Payload builder** | `sum_pd_chunks_in_access_list()` sums chunk_count fields | Sums derived chunk counts from each ReadV2 |
| **Block validation** | Same as payload builder | Same derivation |
| **Access list parsing** | Separates into `chunk_reads[]` and `byte_reads[]`, maintains cross-reference invariant | Single `read_specs[]` vector, no cross-reference |
| **Precompile execution** | Look up ByteRange → cross-ref ChunkRange → fetch chunks → extract bytes | Look up ReadV2 → derive chunk window → fetch chunks → extract bytes |
| **`translate_offset`** | Mutates ByteRangeSpecifier in place | Same logic, applied to ReadV2 fields |
| **Off-chain SDKs** | Must understand 2 types + ordering semantics | One type, no ordering dependency |
| **Solidity interface** | `readByteRange(uint8 index)` — index into byte_reads[] | `readByteRange(uint8 index)` — index into read_specs[] (simpler) |

### 7.8 Issues Resolved by v2

| Issue | v1 Status | v2 Resolution |
|---|---|---|
| Positional cross-reference fragility | Ordering-dependent, undocumented | Eliminated — no cross-reference |
| 20 undefined bytes in ByteRange | Not validated, breaks canonicality | 12 reserved bytes, must be zero, validated |
| U200 partition_index > runtime u64 | Encoding lies about capacity | u64 matches actual system capability |
| U34 length > practical max | 17 GB encodable, 1.9 GB usable | u32 (4 GB) — 2× block max, sufficient |
| Little-endian vs EVM big-endian | Inconsistent with EVM convention | Big-endian throughout |
| Custom ruint types (U18, U34) | Extra dependency for non-standard widths | Standard integer types (u24 as 3 BE bytes, u32) |
| Separate prefetch vs execute truth | Two sources of truth must agree | Single source, derivation is mechanical |

### 7.9 Risks of v2

| Risk | Severity | Mitigation |
|---|---|---|
| Multi-range reads use more keys than v1 when >3 reads share a window | Low | Rare pattern; savings elsewhere compensate |
| chunk_size coupling in derivation formula | Medium | Version-lock v2 to chunk_size = 256 KiB; new sizes get v3 |
| Derived chunk count may surprise users | Low | Document the derivation; SDK computes it for users |
| All existing code that parses ChunkRange/ByteRange must change | Medium | Same scope as the ABI redesign — do both together |
| `byte_offset` as u24 wastes 6 bits vs U18 for 256KB chunks | Negligible | Forward-compatible with chunks up to 16 MB |
