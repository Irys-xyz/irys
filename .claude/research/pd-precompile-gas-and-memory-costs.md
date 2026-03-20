# Research: PD Precompile Gas & Memory Expansion Costs

**Date**: 2026-03-17
**Git Commit**: f0c9a860
**Branch**: feat/pd

## Research Question

When the PD precompile returns large data (up to 256KB chunks) to a Solidity contract, does the user pay extra EVM gas for memory expansion? Can we disable this since users already pay per-chunk IRYS token fees? What are the practical options?

## Summary

The PD precompile currently charges a flat 5,000 EVM gas per call, regardless of return data size. The real cost control is per-chunk IRYS token fees deducted at transaction inclusion. However, the EVM independently charges **memory expansion gas** when the calling Solidity contract copies the precompile's return data into its own memory. This is a quadratic cost baked into revm's opcode-level interpreter — the precompile has no control over it. The cost is manageable (155K gas for 256KB, or 0.5% of a 30M block) but constitutes double-billing for the same data access. Disabling it is technically possible but introduces significant complexity and correctness risks.

---

## 1. How EVM Memory Expansion Gas Works

### 1.1 The Formula

EVM memory expansion gas is defined in the Yellow Paper and implemented in revm at `revm-context-interface/src/cfg/gas_params.rs`:

```
C_mem(a) = G_memory × a + floor(a² / 512)
```

Where:
- `a` = number of 32-byte words: `ceil(bytes / 32)`
- `G_memory` = 3 (linear coefficient)
- `512` = quadratic divisor

The linear term dominates for small allocations. The quadratic term dominates above ~85KB and grows rapidly, creating a natural ceiling on how much memory a single transaction can use.

### 1.2 Cost Table for PD Return Data Sizes

| Return size | Words (a) | Linear (3×a) | Quadratic (a²/512) | Total gas | % of 30M block |
|---|---|---|---|---|---|
| 256 B | 8 | 24 | 0 | 24 | 0.00% |
| 1 KB | 32 | 96 | 2 | 98 | 0.00% |
| 32 KB | 1,024 | 3,072 | 2,048 | 5,120 | 0.02% |
| 256 KB (1 chunk) | 8,192 | 24,576 | 131,072 | 155,648 | 0.52% |
| 512 KB (2 chunks) | 16,384 | 49,152 | 524,288 | 573,440 | 1.91% |
| 1 MB (4 chunks) | 32,768 | 98,304 | 2,097,152 | 2,195,456 | 7.32% |
| 2 MB (8 chunks) | 65,536 | 196,608 | 8,388,608 | 8,585,216 | 28.6% |
| 4 MB (16 chunks) | 131,072 | 393,216 | 33,554,432 | 33,947,648 | 113% (exceeds) |

**Key observations:**
- Single chunk reads (256KB) cost only ~155K gas — affordable
- The quadratic term makes costs explode above 2MB
- The practical ceiling is ~3-3.7 MB before memory gas alone exhausts a 30M block
- Multi-chunk reads within a single precompile call become expensive quickly

### 1.3 Additional Costs Beyond Memory Expansion

A complete PD precompile call also incurs:

| Cost component | Gas | Notes |
|---|---|---|
| Transaction intrinsic | 21,000 | Standard EVM tx cost |
| Cold account access (0x500) | 2,600 | First access to precompile address |
| STATICCALL base | 0 | Included in cold access |
| PD precompile execution | 5,000 | `PD_BASE_GAS_COST` (flat) |
| RETURNDATACOPY copy cost | 3 per word | `3 × ceil(size/32)` |
| Memory expansion | See table | Quadratic, dominant for large returns |

For a 256KB read, total gas is approximately:
- 21,000 (intrinsic) + 2,600 (cold) + 5,000 (precompile) + 24,576 (copy) + 155,648 (memory) = **~209K gas**

---

## 2. Where Memory Gas Is Charged (And Why the Precompile Can't Control It)

### 2.1 The Execution Flow

When a Solidity contract calls `IRYS_PD.readByteRange(0)`:

```
Step 1: Solidity compiler emits STATICCALL(gas, 0x500, inOffset, inSize, 0, 0)
        └─ retSize=0 because `bytes memory` is dynamic-length
        └─ No memory expansion charged at this point

Step 2: revm dispatches to PD precompile
        └─ Precompile executes, returns PrecompileOutput { bytes: [256KB data] }
        └─ Precompile charges 5,000 gas from the subcall's gas allotment

Step 3: revm stores return data in ReturnDataBuffer
        └─ This is a heap buffer in revm's interpreter, NOT EVM memory
        └─ No gas charged

Step 4: Control returns to calling contract
        └─ Solidity emits RETURNDATASIZE → pushes 262,208 onto stack

Step 5: Solidity emits RETURNDATACOPY(destOffset, 0, 262208)
        └─ revm's interpreter charges:
           a) Copy cost: 3 × ceil(262208/32) = 24,576 gas
           b) Memory expansion: C_mem(ceil((destOffset + 262208)/32)) = 155,648 gas
        └─ This is charged in the CALLER's execution frame
        └─ The precompile is not involved — it finished at Step 2
```

### 2.2 The Fundamental Problem

Memory expansion gas is charged by **revm's opcode interpreter** at step 5, inside the `RETURNDATACOPY` opcode handler (`revm-interpreter/src/instructions/system.rs`). The precompile has already returned and has no way to influence what happens next. There is no callback, hook, or flag that a precompile can set to say "don't charge memory gas for my return data."

The interpreter doesn't even know that the return data came from a precompile vs. a regular contract call. By step 5, the origin of the data is lost — it's just bytes in the return data buffer.

### 2.3 revm's Architecture Constraints

Irys's current EVM stack:

| Layer | Source | Customized? |
|---|---|---|
| revm (interpreter, opcodes, gas) | crates.io `v34.0.0` | **No** — standard, unforked |
| reth-evm (EVM integration) | `github.com/irys-xyz/reth-irys` | **Yes** — forked |
| irys-reth (IrysEvm wrapper) | In-repo | **Yes** — wraps `transact_raw()` |

The memory expansion logic lives in revm's interpreter (the unforked layer). To modify it, Irys would need to either fork revm or find a workaround at the reth-evm or irys-reth layer.

---

## 3. Current PD Gas Model

### 3.1 Dual Cost Structure

PD transactions have two independent cost mechanisms:

**EVM gas (standard Ethereum gas accounting):**
- 5,000 flat per precompile invocation (`PD_BASE_GAS_COST`)
- 0 additional gas for I/O operations (`read_bytes_range` returns `gas_used: 0`)
- Memory expansion gas for return data (charged by EVM, not precompile)
- Standard tx intrinsic, calldata, etc.

**IRYS token fees (Irys-specific, deducted in `IrysEvm::transact_raw()`):**
- `base_fee_per_chunk × chunk_count` — dynamic, EIP-1559-style adjustment
- `priority_fee_per_chunk × chunk_count` — user-set tip
- `min_pd_transaction_cost` — floor price per PD transaction
- Base fee goes to treasury, priority fee goes to block producer

### 3.2 The Design Intent

From comments in `crates/irys-reth/src/precompiles/pd/constants.rs`:
> The real anti-DoS mechanism for PD chunk I/O is the per-chunk PD fee deducted at EVM execution time, not EVM gas metering.

The IRYS token fee is the primary cost control. EVM gas is secondary — it exists because the EVM requires it, not because it's the intended pricing mechanism for data access.

### 3.3 Block-Level Constraints

| Constraint | Value | Source |
|---|---|---|
| Block gas limit | 30,000,000 | Standard config |
| Max PD chunks per block | 7,500 | `ConsensusConfig.max_pd_chunks_per_block` |
| Max PD data per block | ~1.9 GB | 7,500 × 256KB |
| Chunk size | 256 KB | `ConsensusConfig.chunk_size` |

The chunk budget (7,500 per block) is enforced by the payload builder at block assembly time, independent of gas accounting.

### 3.4 Relevant Code Locations

| File | What it does |
|---|---|
| `crates/irys-reth/src/precompiles/pd/constants.rs` | `PD_BASE_GAS_COST = 5000` |
| `crates/irys-reth/src/precompiles/pd/read_bytes.rs:345-350` | Returns `gas_used: 0` for read operations |
| `crates/irys-reth/src/precompiles/pd/precompile.rs:82-114` | Adds base gas to operation gas |
| `crates/irys-reth/src/evm.rs:652-777` | IRYS token fee deduction in `transact_raw()` |
| `crates/types/src/precompile.rs:38` | `PD_COST_PER_CHUNK = 5_000` (currently unused for EVM gas) |
| `crates/types/src/precompile.rs:40` | `ACCESS_KEY_NOP = 512` (defined but unused) |
| `crates/irys-reth/src/payload.rs:390-456` | `PdChunkBudget` enforcement |

---

## 4. Options for Reducing or Eliminating Memory Gas

### 4.1 Option A: Fork revm's Interpreter

**Approach**: Add a mechanism in revm's `RETURNDATACOPY` handler to skip memory expansion gas when the return data originated from a specific precompile address.

**Implementation sketch**:
- Add a flag to the interpreter's call context: `gas_free_return_data: bool`
- Set it when STATICCALL dispatches to a registered precompile address
- In `RETURNDATACOPY`, check the flag and skip `resize_memory` gas if set

**Pros**:
- Cleanest solution — gas accounting is accurate at every level
- Gas estimation tools work correctly
- No post-hoc adjustments

**Cons**:
- Requires forking revm (currently standard crates.io dependency)
- Must maintain the fork across revm version upgrades
- Introduces a non-standard EVM behavior that validators must agree on
- Potential consensus divergence if not all nodes run the same fork

**Effort**: High. Requires understanding revm internals, modifying the interpreter, and maintaining the fork.

### 4.2 Option B: Post-Execution Gas Adjustment in `IrysEvm::transact_raw()`

**Approach**: After the EVM executes, subtract the estimated memory expansion cost for PD return data from the transaction's `gas_used` in the result.

**Implementation sketch**:
```rust
// In IrysEvm::transact_raw(), after execution:
let result = self.inner.transact_raw(tx)?;

if is_pd_transaction {
    let pd_return_bytes = /* track via PdContext */;
    let memory_words = (pd_return_bytes + 31) / 32;
    let memory_gas = 3 * memory_words + memory_words * memory_words / 512;
    result.gas_used = result.gas_used.saturating_sub(memory_gas);
}
```

**Pros**:
- No revm fork needed — uses existing `IrysEvm` wrapper
- Leverages existing PD transaction detection (`detect_and_decode_pd_header`)
- Already in the same layer where IRYS token fees are deducted

**Cons**:
- **Gas estimation divergence**: `eth_estimateGas` would report higher gas than actually charged, because the adjustment happens post-execution. Users would overpay gas limits.
- **Block gas accounting**: The sum of `gas_used` across transactions in a block wouldn't match what the EVM actually consumed. Block validation must account for this.
- **Inaccurate refund**: The adjustment is an estimate — the actual memory expansion depends on the caller's existing memory layout, which the post-execution hook can't know precisely.
- **Composability issues**: If a contract calls the PD precompile multiple times, or mixes PD calls with other memory-heavy operations, the estimate could be wrong.

**Effort**: Medium. The wrapper exists; the complexity is in making gas estimation and block validation consistent.

### 4.3 Option C: Precompile `gas_refunded` Field

**Approach**: The precompile calculates expected memory expansion cost and reports it as `gas_refunded` in `PrecompileOutput`.

**Implementation sketch**:
```rust
Ok(PrecompileOutput {
    gas_used: PD_BASE_GAS_COST,
    gas_refunded: estimated_memory_expansion_gas as i64,
    bytes: result_bytes,
    reverted: false,
})
```

**Pros**:
- Simple to implement — single field change in precompile output
- Uses standard EVM refund mechanism

**Cons**:
- **EIP-3529 caps refunds at 20% of total gas used**. For a transaction using 209K gas where 155K is PD memory expansion, the max refund is 41.8K — only covering 27% of the memory cost.
- The precompile doesn't know the caller's memory layout, so the estimate may not match actual expansion cost.
- Refund accounting is already complex (EIP-3529 interactions with SSTORE refunds, etc.).

**Effort**: Low, but **doesn't solve the problem** due to the 20% refund cap.

### 4.4 Option D: Accept the Cost

**Approach**: Document the memory expansion cost and accept it as a minor additional gas expense.

**Cost assessment for typical PD usage patterns:**

| Use case | Chunks | Return data | Memory gas | Total tx gas | % of block |
|---|---|---|---|---|---|
| Read a small config blob | 1 partial | 1 KB | ~98 | ~29K | 0.1% |
| Read a single chunk | 1 | 256 KB | ~155K | ~209K | 0.7% |
| Read an image/document | 4 | 1 MB | ~2.2M | ~2.4M | 8% |
| Read a large dataset | 8 | 2 MB | ~8.6M | ~8.8M | 29% |

**Pros**:
- Zero implementation complexity
- Gas accounting is accurate and standard
- No consensus risks
- Gas estimation tools work correctly
- Memory expansion gas provides a natural safety valve against memory abuse

**Cons**:
- Users pay both IRYS token fees and EVM gas for the same data
- Large multi-chunk reads (>1MB) become gas-expensive
- The cost is invisible in PD fee estimation APIs (`/v1/price/pd/fee-history`)

**Effort**: None. Document only.

### 4.5 Option E: Paginated Returns via Multiple Smaller Calls

**Approach**: Instead of returning all requested data in a single call, the precompile returns data in smaller pages. The contract makes multiple calls.

**Implementation sketch**:
```solidity
// Instead of:
bytes memory allData = IRYS_PD.readByteRange(0); // 256KB at once

// User writes:
for (uint i = 0; i < numPages; i++) {
    bytes memory page = IRYS_PD.readPartialByteRange(0, i * PAGE_SIZE, PAGE_SIZE);
    processPage(page);
}
```

**Pros**:
- Each call returns a smaller amount, keeping memory expansion gas low per-call
- If the contract processes and discards each page, peak memory stays bounded
- No EVM modifications needed

**Cons**:
- Only works if the contract can process data in chunks (streaming pattern)
- If the contract needs all data in memory simultaneously, this doesn't help — total memory expansion is the same
- Adds Solidity-side complexity
- Multiple precompile calls = multiple base gas costs (5,000 each)
- Solidity optimizer may not reclaim memory between iterations

**Effort**: None on the Rust/precompile side. Adds complexity to Solidity consumer contracts.

---

## 5. Recommendation

**Short term: Option D (Accept the cost).** The memory expansion gas for typical PD reads (single chunk, 256KB) is ~155K gas — 0.5% of a 30M block. This is a rounding error compared to the IRYS token fees that represent the real cost of data access. Users won't notice it.

**Document the gas costs** in the PD developer documentation and fee estimation APIs so users can account for it. Consider adding memory expansion estimates to the `/v1/price/pd/fee-history` response.

**If large multi-chunk reads (>1MB) become a common pattern**, revisit with Option B (post-execution adjustment in `IrysEvm::transact_raw()`). This is the most practical path since the wrapper already exists and handles PD-specific logic. The gas estimation divergence can be mitigated by also patching `eth_estimateGas` to apply the same adjustment.

**Option A (fork revm) should only be considered if** the gas cost becomes a genuine blocker for PD adoption and the post-execution adjustment in Option B proves too inaccurate for block validation.

---

## 6. Open Questions

1. **Should `PD_BASE_GAS_COST` account for expected memory expansion?** Currently flat 5,000. Could be increased to better reflect true cost, though this doesn't help the user — it increases gas, not decreases it.

2. **Should the per-chunk EVM gas (`PD_COST_PER_CHUNK = 5,000`, currently unused) be activated?** This would make EVM gas proportional to data size, which is more honest accounting but further increases the double-billing problem.

3. **Should the PD fee estimation API include estimated EVM gas costs?** Currently `/v1/price/pd/fee-history` only returns IRYS token fees. Adding gas estimates would give users a complete cost picture.

4. **What's the expected distribution of PD read sizes?** If most reads are sub-32KB (config blobs, metadata), memory gas is negligible (<5K). If multi-MB reads are common, this needs more attention.
