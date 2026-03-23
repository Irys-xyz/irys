# PD Memory Expansion Gas Elimination — Corrected Implementation Plan

> This plan supersedes conflicting details in `docs/superpowers/specs/2026-03-20-pd-memory-expansion-gas-elimination-design.md`.
>
> In particular:
> 1. Do **not** introduce a custom `PrecompileProvider` wrapper as `IrysEvmFactory::Precompiles`.
> 2. Do **not** change `type Precompiles = PrecompilesMap`.
> 3. Do **not** use a raw thread-local `(ptr, len)` fingerprint.
> 4. Do **not** rely on `revm-interpreter` macros being available through the existing `revm` dependency.
>
> This document is the implementation authority for the feature. If the design doc disagrees with this plan, follow this plan.

**Goal:** Eliminate EVM memory expansion gas for `RETURNDATACOPY` when the current frame's `return_data` buffer is the exact `Bytes` allocation returned by the PD precompile at `0x500`.

**Architecture:** Three components cooperate:
1. The existing PD precompile closure records a **thread-local owned `Bytes` clone** of the successful PD output.
2. A custom `RETURNDATACOPY` instruction handler checks whether the interpreter's current `return_data` buffer is the **same live allocation** and, if so, skips only the memory expansion gas.
3. `transact_raw` installs a **scope guard** that clears the thread-local on entry and again on drop.

**Tech Stack:** Rust, revm v34.0.0, revm-interpreter 32.0.0, revm-handler 15.0.0, alloy-evm 0.27.2, forked reth v1.11.1

**Primary constraints that were verified against upstream code before writing this plan:**

- `IrysEvmFactory` must continue to expose `Precompiles = PrecompilesMap`.
  Reason: reth's `ConfigureEvm`, `EthEvmConfig`, and RPC helpers require that exact associated type.
- `Instruction::new` takes a bare function pointer.
  Reason: the custom opcode handler cannot capture runtime state.
- The PD precompile closure already receives `PrecompileInput<'_>` from alloy-evm.
  Reason: there is no need for a `PrecompileProvider` wrapper just to observe successful PD output.
- A raw `(ptr, len)` thread-local is not sufficient.
  Reason: once the original `Bytes` is freed, another allocation can reuse that address and cause a false positive.
- The custom handler must preserve upstream `RETURNDATACOPY` ordering exactly:
  bounds check first, copy gas next, zero-length early return next, then memory work.
- The PD precompile must be marked **stateful / non-pure**.
  Reason: it depends on `PdContext` and transaction-scoped access-list state, and pure precompiles can be wrapped by engine-level precompile caching.

## Codebase Anchors

Read these local code anchors before editing:

- `crates/irys-reth/src/evm.rs`
  - `impl EvmFactory for IrysEvmFactory`
  - `fn create_evm`
  - `fn create_evm_with_inspector`
  - `impl Evm for IrysEvm<...>`
  - `fn transact_raw`
- `crates/irys-reth/src/precompiles/pd/precompile.rs`
  - `fn pd_precompile`
  - `fn register_irys_precompiles_if_active`
- `crates/irys-reth/src/precompiles/pd/context.rs`

Read these upstream anchors before coding the custom opcode:

- `revm-handler-15.0.0/src/instructions.rs`
  - `EthInstructions`
  - `insert_instruction`
- `revm-interpreter-32.0.0/src/instructions/system.rs`
  - `returndatacopy`
  - `copy_cost_and_memory_resize`
- `revm-interpreter-32.0.0/src/interpreter/shared_memory.rs`
  - `num_words`
  - `resize_memory`
  - `resize_memory_cold`
- `revm-interpreter-32.0.0/src/gas.rs`
  - `Gas`
  - `MemoryGas`
  - `set_words_num`
- `alloy-evm-0.27.2/src/precompiles.rs`
  - `DynPrecompile`
  - `DynPrecompile::new_stateful`
  - `PrecompileInput`
  - `Precompile::is_pure`

---

## Final Design

### 1. Keep `PrecompilesMap` as the EVM associated type

`IrysEvmFactory` currently uses:

```rust
type Precompiles = PrecompilesMap;
```

This must remain unchanged.

Do not introduce:

- `IrysPdPrecompileProvider`
- `type Precompiles = IrysPdPrecompileProvider`
- `precompiles_mut().inner_mut()` plumbing

Those approaches break the generic assumptions made by:

- `reth_evm::ConfigureEvm`
- `reth_evm_ethereum::EthEvmConfig`
- RPC helpers that require `impl Evm<Precompiles = PrecompilesMap>`

### 2. Record PD provenance inside the existing PD precompile closure

The PD precompile closure already has the right integration point:

- it runs exactly when the PD precompile succeeds or fails
- it already has access to the produced `Bytes`
- it can capture `PdContext`
- it can mutate thread-local state without changing any revm generic parameters

Implementation rule:

- Clear the PD marker at the **start** of every PD precompile invocation.
- On **successful, non-reverted, non-empty** PD output, store an **owned `Bytes` clone** in thread-local.
- On errors, reverted outputs, or empty outputs, leave the marker cleared.

### 3. Store an owned `Bytes` clone in thread-local, not a raw pointer tuple

The thread-local must keep the backing allocation alive. The safe shape is:

```rust
thread_local! {
    static LAST_PD_RETURN: RefCell<Option<Bytes>> = RefCell::new(None);
}
```

Why this is required:

- `Bytes::clone()` shares the underlying allocation.
- If thread-local stores the clone, the allocation stays alive until cleared.
- Comparing `return_data.buffer().as_ptr()` and `.len()` against the stored clone is now safe.
- This avoids allocator-address reuse false positives.

### 4. Scope the exemption to the current frame's live `return_data` buffer only

The exemption applies if and only if:

- `context.interpreter.return_data.buffer()` is the same live allocation as the stored PD `Bytes`, and
- the lengths match.

This is intentionally conservative and direct.

Important consequence:

- A contract that calls PD directly can benefit.
- A DELEGATECALL/library frame that calls PD and then executes `RETURNDATACOPY` in that same frame can benefit.
- An outer caller after a normal `CALL`/`DELEGATECALL` return does **not** inherit the exemption automatically, because contract `RETURN`/`REVERT` copies memory into fresh `Bytes`.

This is the correct behavior for the initial implementation.

### 5. Skip only memory expansion gas, never copy gas

`RETURNDATACOPY` must still retain:

- static gas `3` via `Instruction::new(..., 3)`
- dynamic copy gas via `gas_params.copy_cost(len)`

It must skip only the memory expansion charge that upstream `resize_memory` would normally record.

### 6. Update memory bookkeeping even when memory gas is skipped

If memory expansion gas is skipped, the handler must still update:

- `gas.memory().words_num`
- `gas.memory().expansion_cost`
- the physical memory buffer length

Use `MemoryGas::set_words_num(new_num_words, new_cost)` and then `memory.resize(new_num_words * 32)`.

Do **not** call `gas.record_cost(delta)` in the PD-exempt path.

### 7. Mirror upstream `memory_limit` behavior

In the PD-exempt path, do not call internal `resize_memory_cold`, but do preserve the public `memory_limit` behavior:

```rust
#[cfg(feature = "memory_limit")]
if interpreter.memory.limit_reached(offset, len) {
    interpreter.halt_memory_limit_oog();
    return false;
}
```

This keeps the custom path aligned with upstream `resize_memory`.

### 8. Make the PD precompile non-pure

The PD precompile depends on:

- `PdContext::read_access_list()`
- shared chunk state
- now also the thread-local provenance marker

Therefore it is not pure and must not be cached by any engine path that wraps pure precompiles.

Create it as a stateful dynamic precompile.

Do not continue using the plain closure `.into()` conversion, because that marks it pure by default.

## Determinism and Scoping Invariants

These rules are consensus-critical. The implementation must preserve them exactly.

### 1. The exemption is identity-based, not content-based

The handler is allowed to skip memory expansion gas only when:

- the current frame's `return_data.buffer()` points to the same live allocation as the stored PD marker, and
- the lengths match.

Do not compare bytes by value. Two different allocations with equal contents must not get the exemption.

### 2. The live allocation must be kept alive until transaction cleanup

The thread-local marker must own a `Bytes` clone so that:

- the PD backing allocation cannot be freed immediately after the precompile returns
- allocator address reuse cannot create a false positive during the same transaction

This is the reason the marker is `Option<Bytes>` instead of `(ptr, len)`.

### 3. The exemption is scoped to the current frame's current `return_data`

The feature is intentionally narrow:

- if frame `F` calls PD and then frame `F` executes `RETURNDATACOPY`, the exemption may apply
- if frame `F` calls another contract and that contract returns freshly allocated bytes, the exemption must stop applying
- an outer caller after a normal contract `RETURN`/`REVERT` must not inherit PD provenance automatically

### 4. Thread-local state is transaction-scoped only

The marker exists only to bridge:

- the PD precompile closure, and
- the custom `RETURNDATACOPY` handler

within one synchronous EVM transaction execution on one thread.

The scope guard in `transact_raw` is mandatory because it enforces:

- clear on entry
- clear on every early return path
- clear on every normal exit path

Do not move this marker into global process state, `PdContext`, or any shared cross-thread structure.

### 5. Static opcode gas remains unchanged

`RETURNDATACOPY` still has static gas `3`, because the replacement instruction is installed with:

```rust
Instruction::new(irys_returndatacopy::<...>, 3)
```

The handler must not manually re-charge that static gas. The handler is only responsible for:

- dynamic copy gas
- optional memory expansion gas
- the copy itself

---

## File Changes

### New Files

| File | Responsibility |
|------|---------------|
| `crates/irys-reth/src/instructions/mod.rs` | Module root for custom opcode handlers |
| `crates/irys-reth/src/instructions/pd_return_marker.rs` | Thread-local owned-`Bytes` marker plus scope guard |
| `crates/irys-reth/src/instructions/returndatacopy.rs` | PD-aware `RETURNDATACOPY` replacement |

### Modified Files

| File | Change |
|------|--------|
| `crates/irys-reth/src/lib.rs` | Add `mod instructions;` |
| `crates/irys-reth/src/evm.rs` | Keep `PrecompilesMap`; install custom instruction when Sprite is active; add clear-on-entry/drop guard in `transact_raw` |
| `crates/irys-reth/src/precompiles/pd/precompile.rs` | Mark PD precompile stateful; clear/set thread-local marker inside the precompile closure |

### Files Explicitly Not Needed

Do **not** create:

- `crates/irys-reth/src/precompiles/pd/provider.rs`
- any custom `PrecompileProvider` wrapper type
- any `PdContext` field for the fingerprint

---

## Task 1: Thread-Local PD Return Marker

**Files:**

- Create: `crates/irys-reth/src/instructions/mod.rs`
- Create: `crates/irys-reth/src/instructions/pd_return_marker.rs`
- Modify: `crates/irys-reth/src/lib.rs`

### Design requirements

The thread-local must:

- store `Option<alloy_primitives::Bytes>`
- keep the allocation alive
- be clearable explicitly
- support a RAII scope guard for transaction boundaries
- treat empty bytes as "no marker"

### Required public surface

Implement exactly this module-level API:

```rust
pub(crate) struct PdReturnMarkerScope;

impl PdReturnMarkerScope {
    pub(crate) fn new() -> Self;
}

impl Drop for PdReturnMarkerScope {
    fn drop(&mut self);
}

pub(crate) fn clear_pd_return_marker();
pub(crate) fn set_pd_return_marker(bytes: &Bytes);
pub(crate) fn is_current_pd_return_marker(bytes: &Bytes) -> bool;
```

### Required semantics

#### `PdReturnMarkerScope::new`

- clears the marker immediately
- returns a zero-sized scope guard

#### `Drop for PdReturnMarkerScope`

- clears the marker again

#### `set_pd_return_marker`

- if `bytes.is_empty()`, clear instead of storing
- otherwise store `Some(bytes.clone())`

#### `is_current_pd_return_marker`

- return `false` for empty input
- return `true` only if:
  - thread-local holds `Some(stored)`, and
  - `stored.len() == bytes.len()`, and
  - `stored.as_ptr() == bytes.as_ptr()`

Do not compare contents. The comparison is identity-based, not equality-based.

### Recommended implementation

```rust
use std::cell::RefCell;

use alloy_primitives::Bytes;

thread_local! {
    static LAST_PD_RETURN: RefCell<Option<Bytes>> = RefCell::new(None);
}

pub(crate) struct PdReturnMarkerScope;

impl PdReturnMarkerScope {
    pub(crate) fn new() -> Self {
        clear_pd_return_marker();
        Self
    }
}

impl Drop for PdReturnMarkerScope {
    fn drop(&mut self) {
        clear_pd_return_marker();
    }
}

pub(crate) fn clear_pd_return_marker() {
    LAST_PD_RETURN.with(|slot| {
        *slot.borrow_mut() = None;
    });
}

pub(crate) fn set_pd_return_marker(bytes: &Bytes) {
    if bytes.is_empty() {
        clear_pd_return_marker();
        return;
    }

    LAST_PD_RETURN.with(|slot| {
        *slot.borrow_mut() = Some(bytes.clone());
    });
}

pub(crate) fn is_current_pd_return_marker(bytes: &Bytes) -> bool {
    if bytes.is_empty() {
        return false;
    }

    LAST_PD_RETURN.with(|slot| {
        slot.borrow().as_ref().is_some_and(|stored| {
            stored.len() == bytes.len() && stored.as_ptr() == bytes.as_ptr()
        })
    })
}
```

### Tests to add in `pd_return_marker.rs`

- default state is clear
- setting marker makes the same `Bytes` match
- cloned `Bytes` sharing the same allocation also matches
- different allocation with same contents does not match
- clear removes the match
- empty bytes are never considered PD marker data
- scope guard clears on creation
- scope guard clears on drop
- stale allocator reuse cannot be observed through the public API because the stored clone keeps the allocation alive

### `instructions/mod.rs`

Create:

```rust
pub(crate) mod pd_return_marker;
pub(crate) mod returndatacopy;
```

### `lib.rs`

Add:

```rust
mod instructions;
```

---

## Task 2: Custom RETURNDATACOPY Handler

**Files:**

- Create: `crates/irys-reth/src/instructions/returndatacopy.rs`

### Critical implementation rules

1. The handler must be a plain function pointer compatible with `Instruction::new`.
2. Do **not** assume revm macros are available through the existing `revm` dependency.
3. Implement the logic with ordinary Rust and public `revm` APIs.
4. Preserve upstream `RETURNDATACOPY` ordering exactly.
5. Use `Interpreter::resize_memory(...)` for the normal path.
6. Use a dedicated helper for the no-charge PD-exempt path.
7. Keep the instruction's static gas at `3`; do not charge it inside the handler body.

### Required imports

Use these exact import families:

```rust
use revm::context_interface::{cfg::GasParams, Host};
use revm::interpreter::{
    num_words, InstructionContext, InstructionResult, Interpreter, InterpreterTypes,
};
use revm::interpreter::interpreter_types::{MemoryTr, ReturnData, RuntimeFlag, StackTr};
use revm::primitives::{hardfork::SpecId, U256};
```

If import resolution is cleaner in the final code, `Host` may also be imported from `revm::interpreter`, because revm re-exports it there. Use one path consistently.

### Helper functions to implement locally

Implement small local helpers instead of using `check!`, `popn!`, `gas!`, `as_usize_or_fail!`, or `resize_memory!`.

#### `ensure_byzantium`

```rust
fn ensure_byzantium<W: InterpreterTypes>(interpreter: &mut Interpreter<W>) -> bool {
    if !interpreter
        .runtime_flag
        .spec_id()
        .is_enabled_in(SpecId::BYZANTIUM)
    {
        interpreter.halt_not_activated();
        return false;
    }
    true
}
```

#### `pop3_or_underflow`

```rust
fn pop3_or_underflow<W: InterpreterTypes>(
    interpreter: &mut Interpreter<W>,
) -> Option<[U256; 3]> {
    let popped = interpreter.stack.popn::<3>();
    if popped.is_none() {
        interpreter.halt_underflow();
    }
    popped
}
```

#### `u256_to_usize_or_fail`

Use the exact semantics of revm's `as_usize_or_fail!` macro:

```rust
fn u256_to_usize_or_fail<W: InterpreterTypes>(
    interpreter: &mut Interpreter<W>,
    value: U256,
) -> Option<usize> {
    let limbs = value.as_limbs();
    if (limbs[0] > usize::MAX as u64) || (limbs[1] != 0) || (limbs[2] != 0) || (limbs[3] != 0) {
        interpreter.halt(InstructionResult::InvalidOperandOOG);
        return None;
    }
    Some(limbs[0] as usize)
}
```

#### `u256_to_usize_saturated`

Use the exact semantics of revm's `as_usize_saturated!` macro:

```rust
fn u256_to_usize_saturated(value: U256) -> usize {
    let limbs = value.as_limbs();
    let as_u64 = if (limbs[1] == 0) & (limbs[2] == 0) & (limbs[3] == 0) {
        limbs[0]
    } else {
        u64::MAX
    };
    usize::try_from(as_u64).unwrap_or(usize::MAX)
}
```

#### `charge_cost_or_oog`

```rust
fn charge_cost_or_oog<W: InterpreterTypes>(
    interpreter: &mut Interpreter<W>,
    cost: u64,
) -> bool {
    if !interpreter.gas.record_cost(cost) {
        interpreter.halt_oog();
        return false;
    }
    true
}
```

#### `resize_memory_without_gas`

This helper is the core of the feature.

Requirements:

- preserve `memory_limit` behavior under the feature gate
- compute `new_num_words` exactly as upstream
- update `MemoryGas` with `set_words_num`
- resize the memory buffer
- do **not** call `gas.record_cost`

Recommended implementation:

```rust
fn resize_memory_without_gas<W: InterpreterTypes>(
    interpreter: &mut Interpreter<W>,
    gas_params: &GasParams,
    offset: usize,
    len: usize,
) -> bool {
    #[cfg(feature = "memory_limit")]
    if interpreter.memory.limit_reached(offset, len) {
        interpreter.halt_memory_limit_oog();
        return false;
    }

    let new_num_words = num_words(offset.saturating_add(len));
    if new_num_words > interpreter.gas.memory().words_num {
        let new_cost = gas_params.memory_cost(new_num_words);
        let _delta = interpreter
            .gas
            .memory_mut()
            .set_words_num(new_num_words, new_cost)
            .expect("new_num_words > current words_num implies Some(delta)");

        let _ = interpreter.memory.resize(new_num_words * 32);
    }

    true
}
```

This helper must intentionally mirror upstream `resize_memory` semantics except for one change:

- upstream charges `gas.record_cost(delta)`
- this helper must update memory bookkeeping and memory length without calling `gas.record_cost`

### Handler body

Implement:

```rust
pub fn irys_returndatacopy<W: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, W>,
)
```

Use this exact control flow:

1. `ensure_byzantium`
2. pop `[memory_offset, offset, len]`
3. convert `len` with `u256_to_usize_or_fail`
4. convert `data_offset` with `u256_to_usize_saturated`
5. bounds-check `data_offset.saturating_add(len) > return_data.buffer().len()`
6. charge `gas_params.copy_cost(len)`
7. if `len == 0`, return immediately
8. convert `memory_offset` with `u256_to_usize_or_fail`
9. if `is_current_pd_return_marker(return_data.buffer())`:
   - call `resize_memory_without_gas`
10. else:
   - call `context.interpreter.resize_memory(gas_params, memory_offset, len)`
11. copy bytes with `memory.set_data(...)`

Important: step 9 happens only after copy gas and after the zero-length fast path. That matches upstream behavior and avoids broadening the exemption surface.

### Exact code skeleton

```rust
use crate::instructions::pd_return_marker::is_current_pd_return_marker;

pub fn irys_returndatacopy<W: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, W>,
) {
    if !ensure_byzantium(context.interpreter) {
        return;
    }

    let Some([memory_offset, offset, len_word]) = pop3_or_underflow(context.interpreter) else {
        return;
    };

    let Some(len) = u256_to_usize_or_fail(context.interpreter, len_word) else {
        return;
    };
    let data_offset = u256_to_usize_saturated(offset);

    let data_end = data_offset.saturating_add(len);
    if data_end > context.interpreter.return_data.buffer().len() {
        context.interpreter.halt(InstructionResult::OutOfOffset);
        return;
    }

    let gas_params = context.host.gas_params();
    if !charge_cost_or_oog(context.interpreter, gas_params.copy_cost(len)) {
        return;
    }

    if len == 0 {
        return;
    }

    let Some(memory_offset) = u256_to_usize_or_fail(context.interpreter, memory_offset) else {
        return;
    };

    let is_pd = is_current_pd_return_marker(context.interpreter.return_data.buffer());

    if is_pd {
        if !resize_memory_without_gas(context.interpreter, gas_params, memory_offset, len) {
            return;
        }
    } else if !context
        .interpreter
        .resize_memory(gas_params, memory_offset, len)
    {
        return;
    }

    context.interpreter.memory.set_data(
        memory_offset,
        data_offset,
        len,
        context.interpreter.return_data.buffer(),
    );
}
```

### Important edge-case notes

- Keep bounds-check before copy gas, matching upstream.
- Keep copy gas before zero-length early return, matching upstream.
- Do not convert `memory_offset` before the `len == 0` early return.
- Do not exempt memory for non-PD return data.
- Do not clear the PD marker inside `RETURNDATACOPY`.
- A zero-length `RETURNDATACOPY` must return before checking the marker. This is correct because no memory resize occurs.
- Empty `return_data` must never match the marker, even if PD returned `Bytes::new()`.

### Tests to add for the handler

Add unit and integration coverage for:

- direct PD success with one `RETURNDATACOPY`
- multiple `RETURNDATACOPY` instructions on the same PD return data
- PD success followed by `MSTORE` in the same frame; `MSTORE` must charge against the updated highwater mark
- non-PD `RETURNDATACOPY` remains unchanged
- zero-length `RETURNDATACOPY` still charges copy gas semantics and does not resize memory
- out-of-bounds copy still halts with `OutOfOffset`
- invalid oversized operands still halt with `InvalidOperandOOG`

---

## Task 3: Set/Clear the Marker in the PD Precompile

**Files:**

- Modify: `crates/irys-reth/src/precompiles/pd/precompile.rs`

### Required changes

#### A. Import the marker helpers

Add:

```rust
use crate::instructions::pd_return_marker::{
    clear_pd_return_marker,
    set_pd_return_marker,
};
```

#### B. Make the PD precompile stateful

The PD precompile must not remain pure.

Change `pd_precompile(pd_context)` so that it returns a stateful dynamic precompile, for example:

```rust
DynPrecompile::new_stateful(
    alloy_evm::precompiles::PrecompileId::Custom("irys_pd".into()),
    move |input: PrecompileInput<'_>| -> PrecompileResult {
        // body
    },
)
```

Using a stable custom ID string such as `"irys_pd"` is preferred so tests and debugging output are readable.

Do not continue to rely on the plain closure `.into()` conversion, because that marks the precompile pure by default.

#### C. Clear the marker at the start of every PD invocation

At the top of the closure body, before validation:

```rust
clear_pd_return_marker();
```

This ensures:

- PD errors do not leave stale marker state
- repeated PD calls only ever expose the latest successful output

#### D. Set the marker only for successful, non-empty output

After the output bytes and total gas have been computed, but before returning the final `PrecompileOutput`:

```rust
let output_bytes = res.bytes;

if !output_bytes.is_empty() {
    set_pd_return_marker(&output_bytes);
}

Ok(PrecompileOutput {
    gas_used: total_gas,
    gas_refunded: 0,
    bytes: output_bytes,
    reverted: false,
})
```

Do not set the marker for:

- empty output
- any error path
- reverted output

The marker should be set from the exact `Bytes` that will be moved into `PrecompileOutput.bytes`. Do not clone into a temporary and then return a different `Bytes` object.

### Important note about `PrecompileInput`

The design spec previously stated that the precompile closure lacked access to EVM internals.
That is not true for alloy-evm 0.27.2: `PrecompileInput<'_>` already includes `internals`.

For this feature we still do **not** need to touch journal state from the PD precompile. The only required action is setting the thread-local marker from the successful output bytes.

### `register_irys_precompiles_if_active`

Keep the existing signature:

```rust
pub fn register_irys_precompiles_if_active(
    precompiles: &mut PrecompilesMap,
    spec: SpecId,
    pd_context: PdContext,
)
```

Do not change this to a wrapper type.

Tests can inspect the registered precompile with:

```rust
let precompile = precompiles
    .get(&PD_PRECOMPILE_ADDRESS)
    .expect("PD precompile should be registered");
assert!(!precompile.is_pure());
```

---

## Task 4: Integrate the Custom Instruction into `IrysEvmFactory`

**Files:**

- Modify: `crates/irys-reth/src/evm.rs`

### Required changes

#### A. Keep `type Precompiles = PrecompilesMap`

Do not change:

```rust
type Precompiles = PrecompilesMap;
```

#### B. Import the new instruction and marker scope

Add:

```rust
use crate::instructions::pd_return_marker::PdReturnMarkerScope;
use crate::instructions::returndatacopy::irys_returndatacopy;
use revm::bytecode::opcode::RETURNDATACOPY;
use revm::interpreter::Instruction;
```

#### C. Install the custom instruction only when Sprite is active

In both `create_evm` and `create_evm_with_inspector`:

1. keep the existing PD precompile registration
2. immediately after registration, replace opcode `RETURNDATACOPY`

Recommended code shape:

```rust
if is_sprite_active {
    let pd_context = evm.pd_context().clone();
    register_irys_precompiles_if_active(evm.precompiles_mut(), spec_id, pd_context);

    evm.inner.instruction.insert_instruction(
        RETURNDATACOPY,
        Instruction::new(
            irys_returndatacopy::<EthInterpreter, EthEvmContext<DB>>,
            3,
        ),
    );
}
```

Notes:

- `Instruction::new` is public.
- `insert_instruction` is the correct extension point.
- the instruction replacement should be gated by `is_sprite_active`
- the hardfork gate is inclusive at the exact activation timestamp
- in this repository, `EthInterpreter` is already imported from `revm::interpreter::interpreter::EthInterpreter` in `evm.rs`
- keep the existing `register_irys_precompiles_if_active(evm.precompiles_mut(), spec_id, pd_context)` call and add the opcode replacement immediately after it

#### D. Clear marker state for every transaction using a scope guard

At the top of `transact_raw`, before any early returns:

```rust
let _pd_return_marker_scope = PdReturnMarkerScope::new();
```

This must be placed before:

- EIP-4844 rejection
- shadow-tx handling
- PD access-list parsing
- `self.inner.transact(tx)`
- `self.inner.inspect_tx(tx)`

This guarantees:

- clear on entry
- clear on all normal returns
- clear on all early returns

This scope guard is the defense-in-depth fix for thread-local leakage. Keep it even though the PD precompile also clears on entry.

### Do not add extra clear calls elsewhere unless required by tests

With the scope guard in `transact_raw` and the clear-at-PD-entry behavior in the precompile, additional explicit end-of-function clears should not be necessary.

---

## Task 5: Testing

This change must land with thorough tests. Do not treat the handler as complete until the tests below exist.

### A. Marker unit tests

Add in `pd_return_marker.rs`:

- `same_bytes_matches`
- `cloned_bytes_matches`
- `same_contents_different_allocation_does_not_match`
- `empty_bytes_never_match`
- `scope_guard_clears_on_entry_and_drop`

### B. PD precompile tests

Extend existing tests in `crates/irys-reth/src/precompiles/pd/precompile.rs`:

- successful PD call sets marker
- PD call with invalid input clears marker
- PD call with missing access list clears marker
- empty PD output leaves marker clear if such a path can be constructed
- PD precompile is marked non-pure / stateful

If there is no direct helper to assert `is_pure() == false`, add a focused unit test that registers the precompile, fetches it from `PrecompilesMap`, and asserts the wrapped precompile is not pure.

### C. Custom RETURNDATACOPY integration tests

Add new integration-style tests that execute bytecode directly or through a tiny test contract.

Required scenarios:

1. **Direct PD read**
   - call PD
   - `RETURNDATACOPY` into fresh memory
   - verify gas is reduced relative to upstream behavior

2. **Repeated copies in the same frame**
   - call PD once
   - issue multiple `RETURNDATACOPY` instructions
   - all copies from that same live `return_data` should skip memory expansion gas

3. **Subsequent memory op charges correctly**
   - PD read
   - large `RETURNDATACOPY`
   - then `MSTORE` or `CALLDATACOPY`
   - verify later memory op charges relative to the updated highwater mark

4. **Non-PD path unchanged**
   - regular contract return data copied with `RETURNDATACOPY`
   - memory expansion gas must still be charged

5. **Failed PD call does not exempt**
   - failing PD precompile call
   - `RETURNDATACOPY` on the resulting return data must not get the exemption

6. **Direct caller only**
   - contract A calls contract B
   - B calls PD and returns the bytes
   - A performs `RETURNDATACOPY`
   - A must not receive the exemption, because B's `RETURN` creates a fresh `Bytes`

7. **DELEGATECALL same-frame behavior**
   - a DELEGATECALL/library frame calls PD
   - that same delegatecall frame performs `RETURNDATACOPY`
   - exemption should apply there

8. **CREATE / CREATE2 constructor behavior**
   - constructor calls PD
   - constructor `RETURNDATACOPY` can be exempt in its own frame
   - parent frame after create should not inherit stale provenance

9. **Two consecutive transactions on the same EVM**
   - first tx uses PD successfully
   - second tx uses non-PD return data
   - second tx must not inherit the first tx marker

10. **Hardfork boundary**
    - timestamp one second before Sprite activation: no opcode replacement effect
    - timestamp exactly at Sprite activation: feature is active

11. **Return-data overwrite within one frame**
    - PD call succeeds
    - a later non-PD call overwrites `return_data`
    - subsequent `RETURNDATACOPY` must no longer get the exemption

### D. Optional but recommended regression tests

- a very large PD read that previously paid large memory gas now succeeds with materially lower gas
- `eth_call` / estimation path if there are existing harnesses
- a path where PD returns data and then another non-PD call overwrites `return_data`; exemption must stop applying

---

## Acceptance Criteria

The implementation is complete only when all of the following are true:

- `IrysEvmFactory` still compiles with `type Precompiles = PrecompilesMap`
- no custom `PrecompileProvider` wrapper exists
- the PD precompile is registered as a stateful/non-pure dynamic precompile
- the marker stores `Option<Bytes>`, not `(ptr, len)`
- `transact_raw` clears marker state with a scope guard
- `RETURNDATACOPY` is replaced only when Sprite is active
- copy gas remains charged
- static opcode gas remains `3`
- memory expansion gas is skipped only for the current frame's live PD `return_data`
- memory bookkeeping remains correct for subsequent memory ops
- non-PD behavior is unchanged
- hardfork activation is correct at the exact activation timestamp

---

## Implementation Notes for the Next Agent

- Read the upstream files again before coding:
  - `revm-handler-15.0.0/src/instructions.rs`
  - `revm-interpreter-32.0.0/src/instructions/system.rs`
  - `revm-interpreter-32.0.0/src/interpreter/shared_memory.rs`
  - `revm-interpreter-32.0.0/src/gas.rs`
  - `alloy-evm-0.27.2/src/precompiles.rs`

- Do not reintroduce the provider-wrapper idea while implementing.

- Do not use content equality for provenance.
  Reason: that would exempt ABI-re-encoded or copied data and broaden the consensus surface.

- Do not use pointer identity without holding an owned clone alive in thread-local.

- Do not broaden the scope to "all data that originated from PD somewhere in the call tree".
  Reason: upstream call/return paths allocate fresh `Bytes` in several places. The supported invariant is narrower and compile-time/runtime tractable.

- Do not mutate `PdContext` to carry the marker.
  Reason: the marker is transaction-local execution state, not shared PD context.

- Do not trust the old design spec where it conflicts with this plan.
