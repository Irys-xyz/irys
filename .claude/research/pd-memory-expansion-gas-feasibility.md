# Research: Feasibility of Eliminating Memory Expansion Gas for PD Precompile

**Date**: 2026-03-20
**Git Commit**: 95483eeb
**Branch**: rob/memory-expansion

## Research Question

How feasible is it to fork revm (or update the reth fork) to eliminate EVM memory expansion gas for PD precompile return data? Ideally the PD precompile would have zero gas cost impact because users already paid for the PD transaction upfront in full via IRYS token fees.

## Summary

**You don't need to fork revm.** The current revm v34.0.0 exposes `EthInstructions::insert_instruction()` which lets you replace any opcode handler — including RETURNDATACOPY — with a custom implementation. All fields on revm's `Evm` struct are `pub`, so `IrysEvm` can access `self.inner.instruction.insert_instruction(0x3E, custom_handler)` directly in the factory.

This is **Option A-lite**: the cleanest solution from the issue, but without the maintenance burden of a revm fork.

## Current Architecture

### Gas Flow for a 256KB PD Read

| Step | Component | Gas Charged |
|------|-----------|-------------|
| Tx intrinsic | revm | 21,000 |
| Cold account access (0x500) | revm | 2,600 |
| PD precompile base cost | `PD_BASE_GAS_COST` | 5,000 |
| RETURNDATACOPY copy cost | revm (3 gas/word) | 24,576 |
| **Memory expansion** | **revm (3w + w²/512)** | **155,648** |
| **Total** | | **~209K** |

Memory expansion is 74% of the total cost. Users already pay per-chunk IRYS token fees, making this a double charge.

### Where Memory Gas Is Charged

```
Interpreter::step()
  └─ RETURNDATACOPY opcode (0x3E)
       └─ returndatacopy() [revm-interpreter/src/instructions/system.rs]
            └─ copy_cost_and_memory_resize()
                 ├─ gas!(copy_cost) = 3 * ceil(len/32)
                 └─ resize_memory!()
                      └─ resize_memory_cold()
                           └─ memory_cost(words) = 3*w + w²/512
                           └─ gas.record_cost(delta)  ← THIS is the problematic charge
```

The precompile has NO control over this — it happens in the caller's frame after the precompile returns. By the time RETURNDATACOPY runs, the interpreter doesn't know the data came from a precompile.

### Current Dependency Chain

| Layer | Source | Forked? |
|-------|--------|---------|
| `revm` v34.0.0 | crates.io | **No** |
| `revm-handler` v14.1.0 | crates.io (via revm) | **No** |
| `reth-*` crates | `github.com/irys-xyz/reth-irys` rev `324bfb8` | **Yes** |
| `irys-reth` (IrysEvm) | In-repo | **Yes** |

### Key Code Locations

- `crates/irys-reth/src/evm.rs:396-418` — `IrysEvmFactory::create_evm` builds the EVM
- `crates/irys-reth/src/evm.rs:498-514` — `IrysEvm` struct wraps `RevmEvm`
- `crates/irys-reth/src/evm.rs:617-795` — `transact_raw()` PD fee deduction
- `crates/irys-reth/src/precompiles/pd/precompile.rs:21-121` — PD precompile
- `crates/irys-reth/src/precompiles/pd/constants.rs:14` — `PD_BASE_GAS_COST = 5000`

## Feasibility Analysis: Three Approaches

### Approach 1: Custom RETURNDATACOPY via `insert_instruction` (RECOMMENDED)

**No revm fork needed. Medium effort. Cleanest solution.**

revm v34's `EthInstructions` has a public `insert_instruction(opcode, handler)` method. The `Evm` struct's fields (`instruction`, `ctx`, `precompiles`) are all `pub`. This means `IrysEvmFactory` can replace the RETURNDATACOPY opcode handler after construction.

**Implementation sketch:**

```rust
// In IrysEvmFactory::create_evm, after building the EVM:
if is_sprite_active {
    let pd_context_for_instruction = pd_context.clone();
    evm.inner.instruction.insert_instruction(
        0x3E, // RETURNDATACOPY
        Instruction::new(
            move |ctx: InstructionContext<'_, _, EthInterpreter>| {
                // 1. Pop stack: memory_offset, offset, len
                // 2. Bounds-check return data buffer
                // 3. Charge copy cost (3 gas/word) — always
                // 4. Check if return data came from PD precompile
                //    (via a flag on PdContext set during CALL/STATICCALL dispatch)
                // 5. If PD return data: skip memory expansion gas, just resize memory
                //    If normal: charge memory expansion gas as usual
                // 6. Copy bytes into memory
            },
            3, // static gas (VERYLOW)
        ),
    );
}
```

**The challenge:** How to know the return data came from the PD precompile? Two sub-approaches:

**1a. Flag in PdContext (simplest):** Add a `last_call_was_pd: Arc<AtomicBool>` to `PdContext`. The PD precompile sets it to `true` before returning. The custom RETURNDATACOPY checks and clears it. Since `PdContext` is already shared via `Arc` between the precompile closure and the EVM, this works naturally. Risk: if there's a CALL between the precompile return and the RETURNDATACOPY, the flag could be stale. But Solidity's ABI decoder issues RETURNDATACOPY immediately after STATICCALL returns, so in practice this is safe.

**1b. Replace STATICCALL/CALL too:** Also replace the CALL family opcodes to detect when the target is `PD_PRECOMPILE_ADDRESS` and set a flag on return. More robust but more code to maintain.

**Pros:**
- No revm fork — uses standard crates.io revm
- No reth fork changes needed
- Gas accounting is accurate at every level
- `eth_estimateGas` works correctly
- Contained entirely within `irys-reth` crate
- Hardfork-gated via existing `is_sprite_active`

**Cons:**
- Must reimplement the RETURNDATACOPY handler (though it's ~20 lines of code)
- Must track upstream revm changes to this handler across version bumps
- The flag mechanism needs to be carefully designed for correctness in re-entrant/nested call scenarios

**Effort estimate:** ~200-400 lines of code. 1-2 days implementation, 1-2 days testing.

### Approach 2: Post-execution Gas Adjustment in `transact_raw` (EASY BUT HACKY)

After `inner.transact(tx)` returns in `transact_raw`, subtract estimated memory expansion gas.

```rust
// In transact_raw, after inner.transact(tx):
if is_pd_tx {
    let pd_return_bytes = /* from PdContext tracking */;
    let words = (pd_return_bytes + 31) / 32;
    let memory_gas = 3 * words + words * words / 512;
    result.result.gas_used = result.result.gas_used.saturating_sub(memory_gas);
}
```

**Pros:**
- Very simple implementation (~20 lines)
- No revm or reth fork changes
- Already have the right hook point in `transact_raw`

**Cons:**
- `eth_estimateGas` diverges from actual charges (tools break)
- Estimate is inaccurate — depends on caller's existing memory layout (the quadratic formula uses the delta from the current memory highwater mark, not the absolute cost)
- Block gas accounting diverges from EVM execution
- Composability issues: multiple PD calls, mixed memory-heavy operations
- Consensus risk: validators must agree on the exact adjustment formula

**Effort:** ~50 lines, half a day. But the inaccuracy issues may cause more bugs downstream.

### Approach 3: Fork revm (OVERKILL)

Fork `revm` to a git repo, modify `returndatacopy()` in `system.rs` directly.

**Pros:**
- Most "correct" from a purist standpoint
- Can make arbitrarily deep changes

**Cons:**
- Must maintain a revm fork across version bumps (revm releases frequently)
- The reth fork already depends on crates.io revm — adding a revm fork creates a diamond dependency problem (reth's revm vs your revm)
- Would likely require a `[patch]` section or also updating the reth fork to use your revm fork
- Massively increases maintenance burden for a problem that `insert_instruction` solves natively

**Effort:** High. Weeks of integration work to wire up the forked dependency correctly, plus ongoing maintenance.

## Recommendation

**Approach 1 (custom RETURNDATACOPY via `insert_instruction`) is clearly the best path.**

It achieves zero memory expansion gas for PD return data without any fork. The key insight is that revm v34 was designed for exactly this kind of customization — `insert_instruction` exists specifically so chain implementations can override opcode behavior.

For the flag mechanism, sub-approach 1a (flag in PdContext) is sufficient for launch because:
- Solidity always issues RETURNDATACOPY immediately after the CALL that generated the return data
- The flag only needs to survive one opcode boundary
- The PD precompile is always called via STATICCALL (read-only), so re-entrancy isn't a concern

If you want belt-and-suspenders correctness, sub-approach 1b (also replace CALL/STATICCALL) can detect the precompile address at the call site and set the flag more precisely.

### What About Zero-Cost Entirely?

The issue asks about making PD "zero cost effect on the EVM." Memory expansion gas is the big one (74% of cost), but there are also:

| Cost | Gas | Can eliminate? |
|------|-----|---------------|
| Memory expansion | 155,648 | Yes, via Approach 1 |
| Copy cost (RETURNDATACOPY) | 24,576 | Yes, same custom handler can skip this |
| Tx intrinsic | 21,000 | No — fundamental EVM cost |
| Cold account access | 2,600 | Could skip via warm-account injection |
| PD_BASE_GAS_COST | 5,000 | Already controlled by us — can set to 0 |

You could eliminate memory expansion + copy cost + base cost, bringing a 256KB PD read down to ~23,600 gas (just intrinsic + cold access). That's essentially free.

## Open Questions

1. Should the custom RETURNDATACOPY also skip the copy cost (3 gas/word), or just memory expansion?
2. Should `PD_BASE_GAS_COST` be reduced to 0 since the real anti-DoS mechanism is IRYS token fees?
3. How should `eth_estimateGas` behave — should it reflect the reduced gas or the standard EVM gas?
4. Does the `insert_instruction` approach need a consensus-level specification (hardfork spec)?
5. For nested calls (contract A calls contract B which calls PD precompile, return data bubbles up), should all frames get the gas exemption or just the immediate caller?

## Code References

- `revm-context-13.0.0/src/evm.rs:11-25` — `pub struct Evm` with all pub fields including `instruction: I`
- `revm-handler-14.1.0/src/instructions.rs:56-60` — `EthInstructions::insert_instruction(opcode, handler)`
- `revm-interpreter/src/instructions/system.rs` — `returndatacopy()` and `copy_cost_and_memory_resize()`
- `revm-interpreter/src/interpreter/shared_memory.rs` — `resize_memory()` and `resize_memory_cold()`
- `revm-context-interface/src/cfg/gas_params.rs` — `GasParams::memory_cost()`
- `crates/irys-reth/src/evm.rs:396-418` — `IrysEvmFactory::create_evm` (insertion point)
- `crates/irys-reth/src/evm.rs:498-514` — `IrysEvm` wraps `RevmEvm` with `inner` field
- `crates/irys-reth/src/precompiles/pd/context.rs` — `PdContext` (shared state for flag mechanism)
