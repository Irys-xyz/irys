# Design: Eliminate EVM Memory Expansion Gas for PD Precompile Return Data

**Date**: 2026-03-20
**Issue**: [#1227](https://github.com/Irys-xyz/irys/issues/1227)
**Git Commit**: 95483eeb
**Branch**: rob/memory-expansion
**Priority**: Low (post-launch)
**Status**: Design

> Implementation note:
> The implementation authority for this feature is `docs/superpowers/plans/2026-03-23-pd-memory-expansion-gas-elimination.md`.
> In particular, the wrapper-based `PrecompileProvider` approach and raw pointer fingerprint described below are superseded by the implementation plan.

---

## 1. Problem Statement

When a Solidity contract calls the Programmable Data (PD) precompile at `0x500` and copies the return data via `RETURNDATACOPY`, revm charges **quadratic memory expansion gas** to the caller's execution frame. The precompile cannot suppress this charge — it occurs after the precompile returns, in the caller's context.

Users already pay per-chunk IRYS token fees for data access (`(base_fee + priority_fee) * chunk_count`), making the EVM memory expansion gas a **double charge** for the same data.

### Cost Impact

| Return Size | Chunks | Memory Gas | Total Tx Gas | % of 30M Block |
|-------------|--------|------------|--------------|-----------------|
| 256 KB      | 1      | 155,648    | ~209,000     | 0.7%            |
| 1 MB        | 4      | 2,195,456  | ~2,400,000   | 8%              |
| 2 MB        | 8      | 8,585,216  | ~8,800,000   | 29%             |
| 4 MB        | 16     | 33,554,432 | ~33,976,248  | 113% (exceeds)  |

Memory expansion gas is **74% of total gas** for a single 256KB chunk read. The quadratic term (`words^2 / 512`) makes multi-MB reads prohibitively expensive.

### Gas Breakdown for a 256KB PD Read

| Component               | Gas     | Notes                        |
|-------------------------|---------|------------------------------|
| Transaction intrinsic   | 21,000  | Standard EVM cost            |
| Cold account access     | 2,600   | First access to 0x500        |
| PD precompile base cost | 5,000   | `PD_BASE_GAS_COST` (flat)   |
| RETURNDATACOPY copy     | 24,576  | 3 gas/word                   |
| **Memory expansion**    | **155,648** | **Quadratic, dominant**  |
| **Total**               | **~209K** |                            |

---

## 2. Current Architecture

### EVM Stack

| Layer | Source | Forked? |
|-------|--------|---------|
| `revm` v34.0.0 | crates.io | No |
| `revm-handler` v15.0.0 | crates.io (via revm) | No |
| `revm-interpreter` v32.0.0 | crates.io (via revm) | No |
| `reth-*` crates | `github.com/irys-xyz/reth-irys` rev `324bfb8` | Yes |
| `irys-reth` (`IrysEvm`) | In-repo | Yes |

### Where Memory Gas Is Charged

```
Interpreter::step()
  └─ RETURNDATACOPY opcode (0x3E)
       └─ returndatacopy()                  [revm-interpreter/instructions/system.rs:203]
            └─ copy_cost_and_memory_resize() [revm-interpreter/instructions/system.rs:250]
                 ├─ GasParams::copy_cost(len)    ← 3 gas/word
                 └─ resize_memory!()             [macro → shared_memory.rs]
                      └─ resize_memory_cold()
                           ├─ memory_cost(words) = 3*w + w²/512
                           ├─ delta = new_cost - old_cost
                           └─ gas.record_cost(delta)  ← CHARGED HERE
```

**Note**: `copy_cost_and_memory_resize` is monolithic — it charges both copy gas and memory expansion in one call via macros. The custom handler must **reimplement the entire RETURNDATACOPY body**, not compose from existing helpers.

The precompile has no control over this. Memory expansion gas is charged by the **interpreter's opcode handler** after the precompile has returned. By step 5 of the call sequence, the origin of the return data (precompile vs regular contract) is lost — it's just bytes in the return data buffer.

### Current PD Gas Model

**EVM gas** (reported by precompile):
- `PD_BASE_GAS_COST = 5,000` — flat, per invocation
- Read operations return `gas_used: 0`

**IRYS token fees** (deducted in `transact_raw` before EVM execution):
- `base_fee_per_chunk * chunk_count` → treasury
- `priority_fee_per_chunk * chunk_count` → block beneficiary
- Minimum cost floor in USD terms

### Key Code Locations

| Component | File | Lines |
|-----------|------|-------|
| `IrysEvmFactory::create_evm` | `crates/irys-reth/src/evm.rs` | 381-418 |
| `IrysEvmFactory::create_evm_with_inspector` | `crates/irys-reth/src/evm.rs` | 420-462 |
| `IrysEvm` struct | `crates/irys-reth/src/evm.rs` | 498-514 |
| `IrysEvm::transact_raw` | `crates/irys-reth/src/evm.rs` | 617-795 |
| PD precompile closure | `crates/irys-reth/src/precompiles/pd/precompile.rs` | 21-121 |
| `PdContext` | `crates/irys-reth/src/precompiles/pd/context.rs` | 10-108 |
| `PD_BASE_GAS_COST` | `crates/irys-reth/src/precompiles/pd/constants.rs` | 14 |
| PD fee deduction | `crates/irys-reth/src/evm.rs` | 652-786 |
| `PdChunkBudget` | `crates/irys-reth/src/payload.rs` | 413-479 |
| Precompile registration | `crates/irys-reth/src/precompiles/pd/precompile.rs` | 124-136 |

---

## 3. Chosen Approach: Custom RETURNDATACOPY via `insert_instruction`

### Why This Approach

revm v34 provides `EthInstructions::insert_instruction(opcode, handler)` which replaces any opcode handler in the instruction table. All fields on revm's `Evm` struct are `pub`. This means we can replace the RETURNDATACOPY handler with a custom one that skips memory expansion gas for PD-originated return data — **without forking revm**.

### Rejected Alternatives

| Approach | Why Rejected |
|----------|--------------|
| **Fork revm** | Diamond dependency problem (reth fork depends on crates.io revm). Massive maintenance burden for something `insert_instruction` solves natively. |
| **Post-execution gas adjustment** | Cannot rescue txs that already OOG'd during execution. Breaks `eth_estimateGas`. Wrong ETH accounting (reimbursement/beneficiary payment happen before `ExecutionResult` is built). |
| **EIP-3529 gas refunds** | Capped at 20% of gas used. For a 209K gas tx with 155K memory expansion, max refund is 41.8K — only 27%. Not viable. |
| **Inspector::step_end gas erasure** | Only runs on the inspect path. Running inspection universally adds per-opcode overhead for all validators. Not a consensus-path solution. |
| **Custom `Cfg`/`GasParams`** | `Host::gas_params()` is global to the tx — changing memory pricing affects ALL memory expansion, not just PD-origin RETURNDATACOPY. |
| **Paginated reads (contract-level)** | Adds contract complexity, multiple base gas costs, Solidity may not reclaim memory between iterations. Workaround, not a fix. |

---

## 4. Detailed Design

### 4.1 Overview

Three components work together:

1. **Custom `PrecompileProvider` wrapper** — intercepts PD precompile returns and sets a provenance marker
2. **Provenance marker in transient storage** — journal-backed, revert-aware, keyed per caller frame
3. **Custom RETURNDATACOPY opcode handler** — checks the marker and skips memory expansion gas when return data originated from the PD precompile

### 4.2 Component 1: PD-Aware PrecompileProvider Wrapper

Wrap `PrecompilesMap` (or `EthPrecompiles`) with a custom `PrecompileProvider` implementation that:

1. Delegates all calls to the underlying precompile provider
2. After the PD precompile at `0x500` returns successfully, **sets a transient storage marker** on the current caller's address indicating "the return data in this frame came from PD"
3. The marker is set using the journal's transient storage mechanism, which is automatically revert-aware

```
┌─────────────────────────────────┐
│   IrysPdPrecompileProvider      │
│                                 │
│  ┌───────────────────────────┐  │
│  │  inner: PrecompilesMap    │  │
│  └───────────────────────────┘  │
│                                 │
│  fn run(ctx, inputs):           │
│    result = inner.run(ctx, inputs)  │
│    if inputs.address == 0x500   │
│       && result.is_ok():        │
│      set_pd_marker(ctx, caller) │
│    return result                │
│                                 │
└─────────────────────────────────┘
```

**Why a PrecompileProvider wrapper instead of modifying the precompile closure:**
- The precompile closure receives `PrecompileInput` — it doesn't have access to set transient storage on the journal
- `PrecompilesMap::run` has access to the full `context` including journaled state
- This is the integration point where both caller identity and journal writes are available

### 4.3 Component 2: Return Data Provenance Marker

#### Critical Constraint: revm's Async Frame Model

In revm v34, CALL-family opcodes do **NOT** execute subcalls inline. They set `InterpreterAction::NewFrame(FrameInput::Call(...))` and return immediately. The actual subcall execution and `return_data` update happens later in `EthFrame::return_result` (`revm-handler/src/frame.rs:462`: `interpreter.return_data.set_buffer(outcome.result.output)`).

This means:
- A custom CALL opcode handler **cannot observe "after return"** — the subcall hasn't happened yet when the handler finishes
- The marker cannot be conditionally set "after the call returns to check if it targeted PD" from within a replaced opcode handler
- Only the `PrecompileProvider` wrapper (Component 1) runs at the right point in the lifecycle to observe a successful PD call

#### Marker Requirements

| Property | Requirement | Why |
|----------|-------------|-----|
| Transaction-scoped | Cleared between txs | Prevents cross-tx leakage on reused EVM instances |
| Per-interpreter | Tied to the interpreter's `return_data` | Each interpreter frame has its own `return_data` buffer |
| Survives multiple copies | Not cleared on RETURNDATACOPY | Same return data can be partially copied multiple times |
| Invalidated when return_data changes | Must track return_data identity | Prevents stale markers from waiving gas on non-PD data |

#### Recommended Implementation: Return Data Identity Comparison

Instead of a boolean flag that must be carefully set/cleared, **compare the return data buffer identity** to determine PD provenance:

1. When the PD precompile succeeds, the `PrecompileProvider` wrapper records a fingerprint of the return data: `(len, hash)` or simply `(data_ptr, len)` — stored in a shared `Arc` field on `PdContext`
2. The custom RETURNDATACOPY handler checks whether `context.interpreter.return_data.buffer()` matches the recorded PD fingerprint
3. If it matches, skip memory expansion gas
4. The fingerprint is naturally invalidated when any CALL/CREATE replaces `return_data.buffer()` with different bytes — no explicit clearing needed

This eliminates the entire class of stale-flag bugs because the check is against the *actual current return data*, not a side-channel boolean.

**Fingerprint storage**: Add to `PdContext`:
```rust
pub struct PdContext {
    // ... existing fields ...
    /// Fingerprint of the last successful PD precompile return data.
    /// (pointer_as_usize, length) — both change when return_data is replaced.
    last_pd_return: Arc<AtomicU64>,  // packed (len << 32 | hash_lo_32)
}
```

The custom RETURNDATACOPY reads `context.interpreter.return_data.buffer()`, computes the same fingerprint, and compares. Since the `Bytes` type uses reference-counted storage, the pointer/length pair changes when the buffer is replaced by a non-PD CALL.

**Per-tx clearing**: In `transact_raw`, before each tx execution, reset `last_pd_return` to 0. This prevents cross-tx leakage.

#### Why NOT a LocalContext Flag

Customizing `LocalContext` requires changing the `LOCAL` type parameter on `Context<BLOCK, TX, CFG, DB, JOURNAL, CHAIN, LOCAL>`. Since `EthEvmContext<DB>` is a type alias with `LOCAL = LocalContext`, swapping in a custom wrapper type would change the `EthEvmContext` type, propagating through all trait bounds in `IrysEvm` (~10+ sites in `evm.rs`). This is far more invasive than it appears and is not recommended.

#### Why NOT Transient Storage

Transient storage (EIP-1153) is revert-aware via the journal, which is appealing. However:
- The custom RETURNDATACOPY handler accesses state via `context.host` — `Host::tload` does an actual storage read through the journal, adding overhead to every RETURNDATACOPY (not just PD ones)
- Transient storage is keyed by address + slot, not by interpreter frame. Mapping "current frame's return_data provenance" to a storage key requires encoding the call depth, which adds complexity
- The return-data identity comparison approach is cheaper (in-memory pointer comparison) and doesn't touch the journal at all

### 4.4 Component 3: Custom RETURNDATACOPY Handler

Replace opcode `0x3E` via `insert_instruction`. The custom handler **reimplements the entire `returndatacopy` function body** — it cannot delegate to the existing `copy_cost_and_memory_resize` because that function charges copy gas and memory expansion atomically via macros.

#### Handler Logic

```
fn irys_returndatacopy(context: InstructionContext<'_, H, W>) {
    1. check!(BYZANTIUM)
    2. popn!([memory_offset, offset, len])
    3. Bounds-check: len <= return_data.buffer().len() - offset
    4. Charge copy cost: gas::copy_cost(len)  — always, even for PD
    5. If len == 0: return early
    6. Compute fingerprint of current return_data.buffer()
    7. Read PD fingerprint from shared PdContext (via context.host access path)
    8. If fingerprints match (PD data):
       a. Resize memory buffer (memory.resize)
       b. Advance memory highwater mark WITHOUT charging gas
       c. Skip gas.record_cost(delta)
    9. If fingerprints don't match (normal data):
       a. resize_memory!() — charges memory expansion gas as normal
   10. Copy bytes: memory.set_data(...)
}
```

#### Critical: Memory Bookkeeping

If you skip the gas charge but don't advance `gas.memory().words_num` and `expansion_cost`, later memory operations in the same frame will **undercharge** because the highwater mark is wrong.

The implementation must use `MemoryGas::set_words_num` (at `revm-interpreter/src/gas.rs:197-201`):

```rust
// Advance the memory highwater mark WITHOUT charging gas
let new_num_words = num_words(memory_offset.saturating_add(len));
if new_num_words > interpreter.gas.memory().words_num {
    let new_cost = gas_params.memory_cost(new_num_words);
    // set_words_num returns the delta — we intentionally discard it
    let _delta = unsafe {
        interpreter.gas.memory_mut().set_words_num(new_num_words, new_cost)
    };
    // Do NOT call gas.record_cost(delta)
    interpreter.memory.resize(new_num_words * 32);
}
```

This advances the highwater mark so subsequent memory ops (MSTORE, MLOAD, CALLDATACOPY, etc.) compute their deltas correctly.

### 4.5 Type System Constraint: Function Pointers, Not Closures

**This is the single biggest implementation constraint.**

`Instruction::new` takes `fn(InstructionContext<'_, H, W>)` — a **plain function pointer**, not a capturing closure. You cannot close over `pd_context` or any runtime state from the factory.

```rust
// revm-interpreter/src/instructions.rs
pub struct Instruction<W: InterpreterTypes, H: ?Sized> {
    fn_: fn(InstructionContext<'_, H, W>),  // function pointer, NOT Fn closure
    static_gas: u64,
}
```

This means the custom RETURNDATACOPY handler must access the PD marker **exclusively through `context.host`** — the `EthEvmContext<DB>` that implements `Host`. The marker must live somewhere reachable from the Host trait implementation (e.g., transient storage, or a custom field accessible via `context.host`).

### 4.6 Integration Point: IrysEvmFactory

Both `create_evm` and `create_evm_with_inspector` need modification:

```rust
fn create_evm<DB: Database>(&self, db: DB, input: EvmEnv) -> Self::Evm<DB, NoOpInspector> {
    // ... existing setup ...
    let mut evm = IrysEvm::new(/* ... */);

    if is_sprite_active {
        // 1. Register PD precompile (existing)
        let pd_context = evm.pd_context().clone();
        register_irys_precompiles_if_active(evm.precompiles_mut(), spec_id, pd_context);

        // 2. NEW: Replace RETURNDATACOPY with PD-aware version
        evm.inner.instruction.insert_instruction(
            0x3E, // RETURNDATACOPY
            Instruction::new(irys_returndatacopy::<EthInterpreter, EthEvmContext<DB>>, 3),
        );

        // 3. NEW: Replace CALL opcodes to clear PD marker on new calls
        // (CALL=0xF1, STATICCALL=0xFA, DELEGATECALL=0xF4, CALLCODE=0xF2, CREATE=0xF0, CREATE2=0xF5)
        // These need wrappers that clear the PD marker after return_data is replaced
    }

    evm
}
```

### 4.7 Why CALL-Family Opcodes Do NOT Need Replacement

With the return-data identity comparison approach (Section 4.3), **no CALL-family opcode wrappers are needed**. Here's why:

In revm v34, CALL-family opcodes use an **async frame model**:
1. The CALL opcode handler sets `InterpreterAction::NewFrame(...)` and returns immediately
2. The actual subcall execution happens in the frame handler
3. `return_data` is replaced in `EthFrame::return_result` (frame.rs:462)

This means CALL opcode handlers **cannot observe** "after return" — the subcall hasn't happened yet when the handler finishes. Any "clear marker before CALL, set after" pattern is architecturally impossible.

**However, this doesn't matter** with the identity comparison approach:
- When any CALL replaces `return_data` with non-PD bytes, the buffer pointer/length changes
- The custom RETURNDATACOPY compares the current `return_data` buffer against the recorded PD fingerprint
- If they don't match (because a CALL replaced the buffer), no gas exemption is granted
- No explicit clearing is needed — the identity check handles it naturally

| Opcode | Value | Replaces return_data? | Needs wrapper? | Why |
|--------|-------|----------------------|----------------|-----|
| RETURNDATACOPY | 0x3E | No (copies from it) | **Yes** (gas exemption) | Core change |
| CALL | 0xF1 | Yes (via frame handler) | **No** | Identity check handles it |
| STATICCALL | 0xFA | Yes (via frame handler) | **No** | Identity check handles it |
| DELEGATECALL | 0xF4 | Yes (via frame handler) | **No** | Identity check handles it |
| CALLCODE | 0xF2 | Yes (via frame handler) | **No** | Identity check handles it |
| CREATE | 0xF0 | Yes (via frame handler) | **No** | Identity check handles it |
| CREATE2 | 0xF5 | Yes (via frame handler) | **No** | Identity check handles it |

This dramatically simplifies the implementation — only **one opcode** (RETURNDATACOPY) needs replacement.

---

## 5. Security Analysis

### 5.1 Stale Flag Exploit (ELIMINATED BY DESIGN)

**Attack**: Contract calls PD (marker set) → makes a normal CALL that overwrites `return_data` → executes RETURNDATACOPY on non-PD data → gets free memory expansion.

**Why it doesn't work**: The fingerprint approach (Section 4.3) compares the *current* `return_data.buffer()` against the recorded PD fingerprint. When the normal CALL replaces `return_data` with different bytes, the fingerprint won't match. No explicit clearing is needed — the identity check prevents this attack structurally.

### 5.2 Repeated RETURNDATACOPY (CORRECT BY DESIGN)

**Scenario**: Contract calls PD, then issues multiple RETURNDATACOPY instructions on the same return data (partial copies to different memory offsets).

**Behavior**: The marker is NOT cleared by RETURNDATACOPY. It persists until the next CALL/CREATE replaces `return_data`. All copies from the same PD return data are gas-exempt. This is correct — the data's provenance hasn't changed.

### 5.3 Nested/Bubbled Returns (SCOPED TO DIRECT CALLER)

**Scenario**: Contract A calls Contract B, which calls PD, then B returns PD data to A. A's `return_data` now contains PD-originated bytes, but A didn't call PD directly.

**Behavior**: The marker is set in B's frame when B called PD. When B returns to A, A's `return_data` is set to B's return value. The CALL wrapper in A's frame cleared A's marker before the call to B. After B returns, A's marker is NOT set — only B's was.

**Decision**: For the initial implementation, **the gas exemption applies only to the direct caller of the PD precompile**. If A calls B which calls PD, B gets the exemption but A does not. This is the safe, conservative choice. Bubbling the exemption through arbitrary return chains adds significant complexity and consensus risk.

### 5.4 DoS: Unbounded Memory Allocation (MITIGATED BY PD CHUNK BUDGET)

**Risk**: With memory expansion gas removed, a 30M-gas tx could theoretically allocate ~320MB of memory via repeated PD reads.

**Mitigations**:
1. **`PdChunkBudget`**: Block-level cap of 7,500 chunks (~1.9GB total PD data per block). Individual txs can't exceed this.
2. **IRYS token fees**: Economic cost scales with chunk count
3. **Copy cost retained**: 3 gas/word still applies, limiting raw copy throughput
4. **Block gas limit**: Still applies to the transaction as a whole
5. **Single-tx practical limit**: Even with free memory expansion, tx intrinsic + cold access + copy costs limit a single tx to reading ~100 chunks before hitting the 30M gas limit (from copy cost alone)

**Additional safeguard to consider**: Cap the gas exemption at a configurable maximum (e.g., exempt up to N words of memory expansion per tx). This provides a hard safety valve.

### 5.5 Consensus Divergence (LOW RISK)

The custom opcode handler is deterministic — it reads a deterministic marker from execution-scoped state and makes a binary gas/no-gas decision. No floating point, no external I/O, no nondeterminism.

**Risk**: If the marker is stored incorrectly (e.g., not cleared between txs on reused EVM instances), different nodes could see different marker states. This is why the marker must be:
- Transaction-scoped (cleared in `transact_raw` before each tx)
- Execution-scoped (not persisted across blocks)
- Set/cleared by deterministic opcode handlers

### 5.6 Memory Bookkeeping Corruption (CRITICAL TO AVOID)

**Risk**: If the custom RETURNDATACOPY skips `gas.record_cost(delta)` but does NOT update `gas.memory().words_num` and `expansion_cost`, the memory highwater mark becomes stale. All subsequent memory operations (MSTORE, MLOAD, CALLDATACOPY, etc.) in the same frame will compute their expansion delta against the wrong baseline, **undercharging** for memory.

**Mitigation**: The implementation MUST:
1. Call memory resize to grow the physical buffer
2. Update `MemoryGas` fields (`words_num`, last computed cost) to reflect the new size
3. Only skip the `gas.record_cost(delta)` call

This may require reimplementing parts of `resize_memory_cold` or using `MemoryGas::set_words_num` to advance the accounting without charging.

---

## 6. Gas Accounting After This Change

### For a 256KB PD Read (Direct Caller)

| Component               | Before  | After  | Notes |
|-------------------------|---------|--------|-------|
| Transaction intrinsic   | 21,000  | 21,000 | Unchanged |
| Cold account access     | 2,600   | 2,600  | Unchanged |
| PD precompile base cost | 5,000   | 5,000  | Could reduce to 0 separately |
| RETURNDATACOPY copy     | 24,576  | 24,576 | Retained (cheap) |
| Memory expansion        | 155,648 | **0**  | **Eliminated** |
| **Total**               | **~209K** | **~53K** | **75% reduction** |

### Block Capacity Impact

| Metric | Before | After |
|--------|--------|-------|
| Max PD reads per block (256KB each) | ~143 (gas-limited) | ~565 (gas-limited) |
| Max PD reads per block (budget-limited) | 7,500 | 7,500 |
| Practical bottleneck | Gas limit | Chunk budget |

After this change, the PD chunk budget (7,500 chunks/block) becomes the binding constraint rather than EVM gas.

---

## 7. Implementation Scope

### Files to Modify

| File | Change |
|------|--------|
| `crates/irys-reth/src/evm.rs` | Add instruction replacement in both `create_evm` variants. Clear PD marker in `transact_raw` before each tx. |
| `crates/irys-reth/src/precompiles/pd/context.rs` | Add PD return marker field (if using PdContext approach) |
| `crates/irys-reth/src/precompiles/pd/precompile.rs` | Set PD marker on successful precompile return (if modifying precompile directly) |

### New Files

| File | Purpose |
|------|---------|
| `crates/irys-reth/src/instructions/mod.rs` | Custom opcode handlers module |
| `crates/irys-reth/src/instructions/returndatacopy.rs` | PD-aware RETURNDATACOPY implementation |
| `crates/irys-reth/src/precompiles/pd/provider.rs` | Custom PrecompileProvider wrapper that records PD return fingerprint |

### Hardfork Gating

All changes gated behind `is_sprite_active`. When Sprite is not active, standard opcode handlers are used. This ensures:
- Backward compatibility with pre-Sprite blocks
- Clean activation at a known block timestamp
- Testability of both paths

---

## 8. Test Matrix

### Unit Tests

| Test Case | What It Verifies |
|-----------|-----------------|
| Direct PD read, single RETURNDATACOPY | Gas exemption applies, memory allocated correctly |
| Direct PD read, multiple RETURNDATACOPY | All copies exempt, marker not cleared between copies |
| PD read then non-PD CALL | Marker cleared, subsequent RETURNDATACOPY charges normally |
| Non-PD CALL then RETURNDATACOPY | No exemption (marker never set) |
| PD read with 0-length copy | No gas charged, no memory resize |
| PD read filling entire memory | Memory bookkeeping correct, subsequent MSTORE charges correctly |
| Two consecutive PD txs on same EVM | Marker cleared between txs, no leakage |
| Reverted PD subcall | Marker rolled back (if using transient storage) or cleared (if using flag) |

### Exploit Tests

| Test Case | What It Verifies |
|-----------|-----------------|
| PD call → normal CALL → RETURNDATACOPY on non-PD data | Fingerprint mismatch, normal gas charged |
| Recursive self-call after PD | Fingerprint scoped correctly per return_data buffer |
| PD call in inner frame, RETURNDATACOPY in outer frame | No exemption for outer frame (different return_data buffer) |
| Maximum memory allocation via PD reads | Within PdChunkBudget limits, no OOM |
| Malicious contract attempting to abuse fingerprint | All exploit vectors produce correct gas accounting |
| DELEGATECALL to library that calls PD | Exemption works through proxy pattern |
| PD call → revert → RETURNDATACOPY on revert data | Fingerprint mismatch (revert data ≠ PD data), normal gas charged |
| Two consecutive PD txs on same EVM instance | Fingerprint reset between txs, no cross-tx leakage |

### Integration Tests

| Test Case | What It Verifies |
|-----------|-----------------|
| `eth_estimateGas` for PD tx | Returns correct (lower) gas estimate |
| `eth_call` with PD read | Executes with reduced gas, returns correct data |
| Block production with mixed PD/non-PD txs | Gas accounting consistent across block |
| Block validation re-execution | Identical gas results on re-execution |
| Multi-node consensus | All nodes agree on gas used for PD blocks |

---

## 9. Open Questions

1. **Should copy cost (3 gas/word) also be exempted?** Removing it would drop a 256KB read from ~53K to ~29K gas. The copy cost is cheap enough that it might not be worth the added complexity.

2. **Should `PD_BASE_GAS_COST` be reduced to 0?** The real anti-DoS is IRYS token fees. Reducing base cost to 0 would make PD reads even cheaper (~48K → ~24K gas). Could be a separate change.

3. **Should the exemption bubble through intermediate returns?** The conservative choice (direct caller only) is safe but limits composability. A contract library that wraps PD calls would not get the exemption for its callers. With the fingerprint approach, bubbling would work naturally IF the inner contract returns the exact PD bytes unchanged — but if it ABI-re-encodes or transforms the data, the fingerprint won't match.

4. **Should there be a per-tx memory expansion gas exemption cap?** A safety valve (e.g., exempt up to 1M words / ~32MB) would prevent worst-case memory allocation scenarios even if the chunk budget is large.

5. **How to handle `MCOPY` (EIP-5656)?** If the caller MCOPYs PD data to a new memory region, should that expansion also be exempt? Probably not — the exemption should be narrowly scoped to RETURNDATACOPY.

6. **DELEGATECALL through proxy pattern**: If Contract A DELEGATECALLs a library that STATICCALLs PD, the precompile executes in A's context. The fingerprint is recorded from the PrecompileProvider which runs in the context of the DELEGATECALL frame. Does the fingerprint correctly match when A's interpreter later does RETURNDATACOPY? Needs verification — the DELEGATECALL returns the subcall's output to A's `return_data`, which may or may not be the same `Bytes` object.

7. **EOF/EIP-7069 future-proofing**: If EOF is activated, `EXTCALL`, `EXTDELEGATECALL`, and `EXTSTATICCALL` would also overwrite `return_data`. The fingerprint approach handles this automatically (mismatched fingerprint = no exemption), but the PrecompileProvider wrapper would need to recognize EOF-style calls to PD if the calling convention changes.

### Resolved Questions

- **Marker storage mechanism**: Resolved — use return-data fingerprint comparison on `PdContext` (Section 4.3). LocalContext customization is too invasive (changes `EthEvmContext` type alias). Transient storage adds journal overhead to every RETURNDATACOPY. The fingerprint approach is cheapest and most correct.

- **CALL-family wrappers**: Resolved — not needed. The async frame model makes them architecturally impossible, and the fingerprint approach makes them unnecessary (Section 4.7).

---

## 10. Appendix: revm Extension Points Reference

| Extension Point | Scope | Can Modify Memory Gas? | Suitable? |
|----------------|-------|----------------------|-----------|
| `insert_instruction` | Per-opcode | Yes (replace entire handler) | **Yes** |
| Custom `PrecompileProvider` | Per-precompile-call | Can set markers | **Yes** (for tracking) |
| `Inspector::step_end` | Per-opcode (inspect path) | Can erase gas post-hoc | No (inspect-only, perf overhead) |
| Custom `Cfg`/`GasParams` | Global per-tx | Can change all memory pricing | No (too coarse) |
| Custom `Handler` | Per-execution-phase | Pre/post execution hooks | No (no per-opcode hooks) |
| `EthFrame` customization | Per-frame | Return data handling | Possible for bubbling, complex |

---

## 11. Appendix: Data Flow Diagrams

### Current Flow (Problem)

```
Contract.sol                    revm Interpreter              PD Precompile
     │                               │                            │
     │── STATICCALL(0x500, data) ──→ │                            │
     │                               │── dispatch to precompile ─→│
     │                               │                            │── fetch chunks from DashMap
     │                               │                            │── return 256KB data
     │                               │←── PrecompileOutput ───────│
     │                               │    (gas_used=5000)         │
     │                               │                            │
     │                               │── store in return_data ────│
     │                               │   (heap buffer, no gas)    │
     │                               │                            │
     │←── STATICCALL returns ────────│                            │
     │                               │                            │
     │── RETURNDATACOPY ───────────→ │                            │
     │   (copy 256KB to memory)      │                            │
     │                               │── copy_cost: 24,576 gas    │
     │                               │── MEMORY EXPANSION: 155,648│ ← THE PROBLEM
     │                               │── copy bytes to memory     │
     │←── done ──────────────────────│                            │
```

### Proposed Flow (Solution)

```
Contract.sol                    Custom Interpreter            PD Precompile Provider
     │                               │                            │
     │── STATICCALL(0x500, data) ──→ │                            │
     │                               │── dispatch via provider ──→│
     │                               │                            │── run PD precompile
     │                               │                            │── SUCCESS → record fingerprint
     │                               │                            │   of return data in PdContext
     │                               │←── PrecompileOutput ───────│
     │                               │    (gas_used=5000)         │
     │                               │                            │
     │                               │── return_data.set_buffer() │
     │                               │   (frame handler sets it)  │
     │                               │                            │
     │←── STATICCALL returns ────────│                            │
     │                               │                            │
     │── RETURNDATACOPY ───────────→ │ (custom handler)           │
     │   (copy 256KB to memory)      │                            │
     │                               │── fingerprint(return_data) │
     │                               │── compare to PdContext     │
     │                               │── MATCH → PD data          │
     │                               │── copy_cost: 24,576 gas    │
     │                               │── MEMORY EXPANSION: SKIP   │ ← FIXED
     │                               │── advance highwater mark   │
     │                               │── resize memory buffer     │
     │                               │── copy bytes to memory     │
     │←── done ──────────────────────│                            │
```
