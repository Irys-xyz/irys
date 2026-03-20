# Feasibility Report: 4GB PD Data Per Transaction

**Date**: 2026-03-20
**Related Issue**: [#1227](https://github.com/Irys-xyz/irys/issues/1227)
**Related Spec**: [pd-memory-expansion-gas-elimination-design.md](./2026-03-20-pd-memory-expansion-gas-elimination-design.md)
**Git Commit**: 95483eeb
**Research Method**: Claude (Opus 4.6) codebase analysis + OpenAI Codex (gpt-5.4, xhigh reasoning) second opinion

---

## 1. Executive Summary

Supporting 4GB of PD data per transaction is **technically feasible** but requires 64GB+ node RAM, a runtime byte meter to prevent repeated-copy abuse, and acceptance that 4GB single-read operations will add 0.4–1.0 seconds of latency to block production/validation.

The EVM gas cost can be made trivial (~29K gas for a 4GB read). The real constraints are physical: memory allocation, zero-fill time, memcpy throughput, and DoS protection. Contracts will realistically use paginated reads (256KB–2MB per call); the 4GB single-read path is an edge case that must be safe but need not be fast.

---

## 2. Gas Feasibility

### Why 4GB Is Impossible Under Standard EVM Gas

4GB = 4,294,967,296 bytes = 134,217,728 words (32 bytes each) = ~16,384 chunks (256KB each).

| Gas Component | Cost at 4GB | vs 30M Block Limit |
|---|---|---|
| Copy cost (3 gas/word) | 402,653,184 | **13.4x over limit** |
| Memory expansion (3w + w²/512) | ~35,200,000,000,000 | **1.17M x over limit** |
| **Combined** | **~35.2 trillion** | Completely infeasible |

Even with memory expansion fully exempt, copy cost alone at 402M gas is 13x the block gas limit. **Both copy cost AND memory expansion must be exempt for PD data.**

### With Full Exemption

| Component | Gas | Notes |
|---|---|---|
| Transaction intrinsic | 21,000 | Standard EVM cost |
| Cold account access (0x500) | 2,600 | First access |
| PD_BASE_GAS_COST | 5,000 | Flat precompile cost |
| RETURNDATASIZE | 2 | Stack push |
| RETURNDATACOPY (copy cost) | **0** | Exempted |
| Memory expansion | **0** | Exempted |
| **Total** | **~28,600** | Trivially fits in 30M |

The custom RETURNDATACOPY handler (from the gas elimination design) must exempt **both** copy cost and memory expansion gas when the return-data fingerprint matches PD precompile output.

### Precompile Type-Level Support

The precompile already supports 4GB at the type level:
- `ChunkRangeSpecifier.chunk_count`: `u16` (max 65,535 chunks = ~16GB)
- `ByteRangeSpecifier.length`: `U34` (max ~17GB)
- A single STATICCALL with the right access list can request 4GB

---

## 3. Physical Memory Analysis

### Memory Allocation Path

A 4GB PD read follows this allocation path:

```
1. PdService pre-fetches 16,384 chunks into DashMap
   └─ 16,384 × Arc<Bytes>(256KB) ≈ 4GB in DashMap

2. Precompile builds output buffer
   └─ Vec::with_capacity(4GB) → extends from 16K DashMap reads
   └─ drain(offset..offset+len).collect() → second 4GB allocation
   └─ First Vec dropped, Bytes::from(collected_vec) → owns 4GB

3. return_data.set_buffer(output)
   └─ Moves Bytes ownership, no copy (Arc increment)

4. RETURNDATACOPY: memory.resize(4GB)
   └─ Vec::resize(4GB, 0) → zero-fills 4GB
   └─ memory.set_data() → copies 4GB from return_data
```

### Peak Memory Per Transaction

| Buffer | Size | Lifetime |
|---|---|---|
| DashMap chunk cache | 4 GB | Until tx/block release |
| Precompile Vec (pre-drain) | 4 GB | Dropped after drain |
| Precompile Vec (post-drain) | 4 GB | Overlaps briefly with pre-drain |
| return_data (shared Arc) | 0 | Points to precompile output |
| EVM memory zero-fill | 4 GB | Entire tx execution |
| EVM memory (post-copy) | 4 GB | Contains actual data |
| **Peak** | **~12–16 GB** | During precompile execution |
| **Steady state** | **~8 GB** | During contract execution |

Add ~4–8 GB for normal node baseline (reth state, networking, OS).

### Node RAM Requirements

| RAM | Viability |
|---|---|
| 16 GB | **Not viable** — OOM on any 4GB PD tx |
| 32 GB | **Marginal** — works for single 4GB tx, no headroom for concurrent work |
| 64 GB | **Recommended** — handles 4GB tx with headroom |
| 128 GB | **Comfortable** — handles worst-case scenarios |

**Recommendation: 64GB minimum for validators.**

---

## 4. Performance Analysis

### 4.1 Precompile Execution Time

| Operation | Time Estimate | Notes |
|---|---|---|
| 16K DashMap lookups | ~1–5 ms | Sharded RwLock, low contention for single tx |
| 4GB Vec assembly (extend) | ~50–200 ms | Sequential memcpy from 16K chunks |
| drain().collect() | ~100–400 ms | Full 4GB pass + allocation |
| **Total precompile** | **~150–600 ms** | Dominated by memory throughput |

**Optimization**: Replace `drain(...).collect()` with zero-copy `Bytes::from(vec).slice(offset..offset+length)`. This eliminates the second 4GB pass and allocation, roughly halving precompile time.

### 4.2 RETURNDATACOPY Execution Time

| Operation | Time Estimate | Notes |
|---|---|---|
| Vec::resize(4GB, 0) — zero-fill | ~200–500 ms | 4GB of memory writes |
| memory.set_data() — copy | ~200–500 ms | 4GB memcpy |
| **Total RETURNDATACOPY** | **~400–1000 ms** | Dominated by memory bandwidth |

### 4.3 Total Block Latency Impact

A single 4GB PD transaction adds **0.5–1.5 seconds** to block production or validation. This is significant but acceptable given:
- VDF pacing provides ~1 step/sec timing
- The 4GB case is an edge case, not the common path
- Most PD reads will be 256KB–2MB (sub-millisecond)

### 4.4 Storage I/O for Block Validation

Validators must re-read chunks from storage during block validation. For a cold 4GB PD tx:
- `PdService::handle_provision_chunks` fetches 16,384 chunks from `ChunkStorageProvider`
- This is 16K random disk reads (or cache hits if warm)
- Cold NVMe: ~50–200 ms for 16K random reads
- Warm cache: ~1–5 ms

**Total cold validation**: 1–2 seconds for a 4GB PD tx. Acceptable but worth documenting as expected behavior.

---

## 5. Security & DoS Analysis

### 5.1 Repeated RETURNDATACOPY Abuse (CRITICAL)

**Attack**: A contract calls PD once (4GB, ~29K gas) then copies the return data into EVM memory N times:

```solidity
bytes memory a = IRYS_PD.readData(0);  // 4GB, ~29K gas
// Solidity generates RETURNDATACOPY for each `bytes memory` variable
// Each copy materializes another 4GB in EVM memory
```

Without mitigation, each RETURNDATACOPY exemption adds 4GB of memory with near-zero gas. With 30M gas available and ~29K per read, a single tx could theoretically issue ~1,000 PD calls, each materializing 4GB.

**Why current limits don't prevent this**:
- `PdChunkBudget` counts **declared chunks** in the access list, not actual memory materialized
- The same return data can be copied to different memory offsets unlimited times
- The mempool has no per-tx PD size cap

**Mitigation: Runtime Byte Meter**

Add a per-tx counter tracking total bytes exempted from RETURNDATACOPY gas:

```
pd_exempt_bytes_used: u64  (on PdContext, reset per-tx in transact_raw)

MAX_PD_EXEMPT_BYTES_PER_TX = 4GB  (hardfork config)
```

The custom RETURNDATACOPY handler:
1. Checks fingerprint match (PD data?)
2. Checks `pd_exempt_bytes_used + len <= MAX_PD_EXEMPT_BYTES_PER_TX`
3. If both pass: exempt gas, increment counter
4. If byte meter exceeded: charge gas normally (standard EVM behavior)

This caps total exempted memory at 4GB per tx regardless of how many times the contract copies.

### 5.2 Multiple PD Calls in One Transaction

A contract can make multiple STATICCALL to PD in one tx, each returning different data. Each call's RETURNDATACOPY would consume from the same 4GB byte meter. After 4GB total exempted bytes, subsequent copies are charged normally.

### 5.3 Infallible Allocation = Process Abort on OOM (HIGH)

Both the precompile path (`Vec::with_capacity`) and revm (`Vec::resize`) use **infallible allocation**. On OOM, the process aborts — it does not gracefully reject the tx.

**Mitigation**:
- Use `try_reserve` in the precompile's `Vec::with_capacity` path
- Return `PdPrecompileError` on allocation failure rather than aborting
- For revm's `Vec::resize`: this is in library code we don't control. The per-tx byte meter (Section 5.1) prevents the worst case by capping exempt memory at 4GB.

### 5.4 Cache Pinning Growth

The PD chunk cache (`ChunkCache`) grows beyond its initial capacity when all entries are pinned (referenced by active txs). With 16K chunks per tx and multiple pending PD txs in the mempool, cache memory can grow significantly.

Current behavior (from `cache.rs:170`): when all entries are pinned, capacity grows by 1 for each new insert.

**Mitigation**: Add a hard cap on total cache size (e.g., 32GB). Reject new PD chunk provisioning beyond this cap.

### 5.5 Block Gas Limit Interaction

A 4GB PD tx uses ~29K gas, leaving ~29.97M gas for other txs in the same block. The block could contain ~1,000 minimal txs alongside the 4GB PD tx. This is normal — the PD chunk budget is the real constraint, not gas.

### 5.6 revm `memory_limit` Feature

revm has an optional `memory_limit` feature (disabled by default) with a default cap of `(1<<32)-1` = 4,294,967,295 bytes — **one byte below 4 GiB**. If this feature is ever enabled, it would reject exactly 4GB of EVM memory.

**Mitigation**: Ensure `memory_limit` feature stays disabled, or if enabled, set the limit above 4GB.

---

## 6. Solidity Usability at 4GB

### What Solidity Can Do

Solidity can represent a 4GB `bytes memory` variable. ABI encoding uses `uint256` for the length prefix, which supports up to 2^256 bytes.

### What Solidity Cannot Do Efficiently

Even with free PD reads, **operating on 4GB in the EVM is prohibitively expensive**:

| Operation on 4GB | Gas Cost | % of 30M Block |
|---|---|---|
| keccak256(data) | ~805,000,000 | 2,683% (impossible) |
| Iterate every word | ~402,000,000 | 1,342% (impossible) |
| Read single word (MLOAD) | 3 | Trivial |
| Slice 1KB from memory | ~100 | Trivial |

A 4GB `bytes memory` is effectively a **read-only random-access buffer**. Contracts can:
- Read individual words or small slices cheaply
- Pass pointers/offsets to other precompiles
- Cannot hash, iterate, or transform the full buffer

### Realistic Usage Patterns

| Pattern | Data Size | Calls | Memory |
|---|---|---|---|
| Read a config blob | 1–32 KB | 1 | Trivial |
| Read a single chunk | 256 KB | 1 | Trivial |
| Read an image/document | 1–4 MB | 1 | Fine |
| Read a dataset (paginated) | 10–100 MB | 40–400 | Fine, paginated |
| Read entire archive (single) | 4 GB | 1 | Edge case, 1s+ latency |

**Conclusion**: Most contracts will use paginated reads (256KB–2MB per call). The 4GB single-read path is for specialized use cases (e.g., feeding data to a companion precompile for off-EVM processing).

---

## 7. Required Implementation Changes

### 7.1 Changes to the Gas Elimination Design

| Change | Where | Why |
|---|---|---|
| Exempt copy cost in addition to memory expansion | Custom RETURNDATACOPY handler | Copy cost alone exceeds 30M gas at 4GB |
| Add runtime per-tx byte meter | `PdContext` + custom RETURNDATACOPY | Prevent repeated-copy DoS |
| `MAX_PD_EXEMPT_BYTES_PER_TX = 4GB` | Hardfork config | Configurable per-tx exemption cap |

### 7.2 Precompile Optimizations

| Change | Where | Impact |
|---|---|---|
| Replace `drain(...).collect()` with `Bytes::from(vec).slice()` | `read_bytes.rs:344` | Eliminates second 4GB copy, saves ~100–400ms |
| Use `try_reserve` instead of infallible `Vec::with_capacity` | `read_bytes.rs:289` | Graceful failure on OOM instead of process abort |

### 7.3 Block-Level Enforcement

| Change | Where | Why |
|---|---|---|
| Per-tx PD chunk limit (16,384 chunks = 4GB) | `PdChunkBudget` / block validation | Enforce 4GB per-tx at protocol level |
| Per-block PD chunk budget increase | Hardfork config | Current 7,500 chunks (~1.9GB) is below 4GB per-tx |
| Mempool per-tx PD size check | `mempool.rs` prefilter | Reject oversized PD txs before inclusion |
| Hard cap on total chunk cache size | `ChunkCache` | Prevent unbounded cache growth |

### 7.4 Node Requirements

| Requirement | Value | Notes |
|---|---|---|
| Minimum RAM | **64 GB** | Handles 4GB PD tx with headroom |
| Recommended RAM | 128 GB | Comfortable for worst-case + concurrent work |
| Storage | NVMe SSD | 16K random reads for cold 4GB validation |
| revm `memory_limit` | Disabled or > 4GB | Default cap is 4GB - 1 byte |

---

## 8. Alternatives Considered

### 8.1 Memory-Mapped EVM Memory

Replace revm's `Vec<u8>`-backed `SharedMemory` with mmap'd storage.

**Verdict**: Not practical. Requires deep revm memory subsystem changes, far beyond `insert_instruction`. Would need a revm fork, which we're specifically avoiding.

### 8.2 Handle-Based API (Virtual Memory)

Instead of returning raw bytes, the PD precompile returns opaque handles. Companion precompiles (hash, search, slice) operate on handles without materializing data in EVM memory.

**Verdict**: Cleanest long-term solution for very large reads. But requires a new API surface and companion precompiles. Not needed for launch — paginated reads cover practical needs. Worth tracking as a future enhancement.

### 8.3 Streaming/Paged Read Within EVM

Contract reads in pages (e.g., 2MB per call) and processes each before reading the next.

**Verdict**: Already supported by the current precompile. This is the recommended pattern for contracts. The 4GB single-read path is an additional capability, not a replacement.

### 8.4 Reduced Per-TX Limit (e.g., 256MB)

Lower the per-tx cap to reduce worst-case memory.

**Verdict**: Possible fallback if 4GB proves problematic in practice. 256MB would require ~2GB peak memory (much more manageable) while still covering most use cases. Could start with 256MB and increase via hardfork config.

---

## 9. Recommendations

### For Launch

1. **Set `MAX_PD_EXEMPT_BYTES_PER_TX` conservatively** (e.g., 256MB or 1GB) via hardfork config
2. Implement the runtime byte meter in the custom RETURNDATACOPY handler
3. Fix `drain(...).collect()` in the precompile (zero-copy slice)
4. Add `try_reserve` for graceful OOM handling
5. Document 64GB RAM as node requirement

### Post-Launch

1. Increase `MAX_PD_EXEMPT_BYTES_PER_TX` toward 4GB as real-world usage is observed
2. Profile block production/validation latency with large PD txs
3. Consider handle-based API for truly large data processing
4. Monitor cache memory growth under load

### Key Metrics to Track

- P99 block production time with PD txs
- Peak memory per block execution
- Chunk cache hit rate and growth rate
- Distribution of PD read sizes in production

---

## 10. Conclusion

4GB per-tx PD data is feasible with these constraints:

| Aspect | Status | Requirement |
|---|---|---|
| EVM gas | Solved | Exempt copy cost + memory expansion |
| Type-level support | Already supported | chunk_count u16, length U34 |
| Physical memory | Feasible | 64GB+ node RAM |
| Latency | Acceptable | 0.5–1.5s for 4GB (edge case) |
| DoS protection | Needs work | Runtime byte meter + per-tx chunk limit |
| OOM safety | Needs work | Fallible allocation in precompile |
| Solidity usability | Limited | 4GB is read-only buffer; paginated reads for processing |

The 4GB limit should be configurable via hardfork config, starting conservatively and increasing as the system is proven under load.
