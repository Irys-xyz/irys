# Plan: Integration Tests for Large PD Read Gas Elimination

**Date**: 2026-03-24
**Branch**: rob/memory-expansion (rebased on feat/pd)
**Depends on**: `2026-03-23-pd-memory-expansion-gas-elimination.md` (Tasks 1-4)

## Goal

Prove that the custom `irys_returndatacopy` handler correctly eliminates
memory expansion gas for PD precompile return data at scale (5 MB+),
and that it does **not** alter gas accounting for non-PD return data.

## Background

### Why this matters

EVM memory expansion gas is `cost = 3 * words + words² / 512` where
`words = ceil(byte_size / 32)`. For large RETURNDATACOPY sizes:

Note: tests use `20 × 256,000 = 5,120,000` bytes (not 5 MiB = 5,242,880).
The ABI encoding adds 64 bytes overhead (offset + length prefix), plus
padding to a 32-byte boundary. Exact return buffer sizes must be computed
from `(num_chunks × chunk_size) + ABI_overhead`.

| Payload bytes | Words   | Memory expansion gas | vs 30 M gas limit |
|---------------|---------|---------------------|--------------------|
| 1,024,000     | 32,000  | 2,096,000           | 7.0 %              |
| 5,120,064*    | 160,002 | 50,481,256          | **168 %** (OOG)    |
| 10,240,064*   | 320,002 | 200,962,506         | **670 %** (OOG)    |

\* includes 64-byte ABI encoding overhead

Without the gas elimination, any PD read returning ≥ ~3 MB of data would
always OOG in a contract context, regardless of the gas limit. The feature
**must be tested at scale** to prove it works.

### What's already tested

- `pd_return_marker.rs`: 7 unit tests for marker identity logic (no EVM)
- `returndatacopy.rs`: **zero tests** for the handler itself
- `precompile.rs`: 8 unit tests via direct `TxKind::Call(0x500)` — these
  exercise the precompile but **never trigger RETURNDATACOPY** because
  direct precompile calls return data inline without opcode involvement
- Chain-level tests: use Solidity contracts that emit RETURNDATACOPY via
  `staticcall`, but never assert on gas and use tiny data (≤ 3 chunks)

### ABI surface (post feat/pd refactor)

```solidity
function readData(uint8 index) external view returns (bytes memory data);
function readBytes(uint8 index, uint32 offset, uint32 length) external view returns (bytes memory data);
```

Access list specifier: `PdDataRead { partition_index: u64, start: u32, len: u32, byte_off: u32 }`
encoded as a 32-byte key with type byte `0x01`.

Return encoding: ABI `bytes memory` = 32 bytes offset + 32 bytes length + data (padded to 32).
For 20 chunks × 256,000 bytes = 5,120,000 payload, return buffer = 5,120,064 bytes
(32 offset + 32 length + 5,120,000 data, already 32-byte aligned).

## Test Strategy

Three test tiers, each exercising a different layer:

1. **Unit test** (`returndatacopy.rs`): handler logic with mock interpreter —
   no real EVM, fast, precise gas assertions.
2. **EVM integration test** (`tests/pd_gas_elimination.rs`): full EVM with
   raw bytecode contract calling PD precompile — proves end-to-end gas
   elimination through real RETURNDATACOPY execution.
3. **Comparative gas test** (same file): same contract calling a non-PD
   precompile — proves non-PD RETURNDATACOPY still charges memory gas.

## Configuration

All EVM-level tests use a custom `IrysEvmFactory` with production-like
chunk config (not `ConsensusConfig::testing()` which has `chunk_size=32`):

```rust
let chunk_config = ChunkConfig {
    num_chunks_in_partition: 200,
    chunk_size: 256_000,       // 256 KB — production chunk size
    entropy_packing_iterations: 0,
    chain_id: 1,
};
```

With this config: 5 MB = 20 chunks, 10 MB = 40 chunks.

---

## Task 1: Unit Tests for `irys_returndatacopy` Handler (PRIMARY)

**File**: `crates/irys-reth/src/instructions/returndatacopy.rs`
**Estimated complexity**: Medium
**Priority**: This is the **primary proof** of the handler's correctness.
It is the cleanest place to test PD vs non-PD branching, exact gas
accounting, source offsets, and high-water bookkeeping — do not skip.

### Objective

Add a `#[cfg(test)] mod tests` block to `returndatacopy.rs` that validates
the handler's branching logic without constructing a real EVM. These tests
give fast, precise control over the interpreter state.

### Subtask 1a: Test helper — mock interpreter builder

Create a helper that builds a minimal revm `Interpreter` (or the relevant
subset) with:
- A pre-set return data buffer (`Bytes`)
- A gas limit
- A stack with the three RETURNDATACOPY operands pushed
- An empty memory

Investigate what public APIs revm 32.0.0 exposes for constructing an
`Interpreter` in tests. The `Interpreter::new(...)` constructor or
`Interpreter::default()` + field mutations may work. If the interpreter
struct has private fields, consider whether a `#[cfg(test)]` helper on the
crate side or using `EthInterpreter::new(...)` is feasible.

If a standalone `Interpreter` truly cannot be constructed, use the full EVM
approach from Task 2 as a workaround, but document the limitation. In this
case, the handler-level assertions must still be verified via gas_used
arithmetic on the EVM result.

### Subtask 1b: Test — PD return data skips memory expansion gas

Use exact byte counts derived from the actual return buffer size.

1. Set the PD return marker via `set_pd_return_marker(return_data.clone())`
2. Push stack: `(dest_offset=0, offset=0, size=5_120_064)` (20 chunks ABI-encoded)
3. Call `irys_returndatacopy` with the interpreter
4. Assert:
   - No OOG (interpreter status is OK / Continue)
   - Memory is resized to ≥ 5,120,064 bytes
   - `gas.memory().words_num` updated to `ceil(5_120_064 / 32)` = 160,002
   - Gas spent is **only** the copy gas: `3 * 160,002` = 480,006
   - Memory expansion gas (~50,481,256) was **not** charged

### Subtask 1c: Test — non-PD return data charges full gas

1. Do NOT set the PD return marker (or set it to a different allocation)
2. Same stack operands as 1b
3. Call `irys_returndatacopy`
4. Assert:
   - Gas spent includes copy gas **plus** memory expansion gas
   - Or: OOG if gas limit < total (demonstrating the cost is real)

### Subtask 1d: Test — zero-length RETURNDATACOPY short-circuits

1. Set PD return marker
2. Push stack: `(dest_offset=0, offset=0, size=0)`
3. Call handler
4. Assert: no gas charged beyond static gas, marker not consulted

### Subtask 1e: Test — PD marker cleared between transactions

1. Set PD return marker, create `PdReturnMarkerScope`
2. Drop the scope
3. Call `irys_returndatacopy` with size > 0
4. Assert: memory expansion gas IS charged (marker was cleared)

### Subtask 1f: Test — non-zero source offset in RETURNDATACOPY

1. Set PD return marker with a 5 MB buffer
2. Push stack: `(dest_offset=0, offset=1_000_000, size=100_000)` (partial read)
3. Call handler
4. Assert:
   - Succeeds without OOG
   - Memory expansion is still skipped (marker matches regardless of offset)
   - Copied bytes match `return_data[1_000_000..1_100_000]`

### Subtask 1g: Test — multiple RETURNDATACOPY ops against same PD buffer

1. Set PD return marker
2. Call handler with `(dest_offset=0, offset=0, size=1_000_000)` — 1 MB
3. Call handler again with `(dest_offset=1_000_000, offset=1_000_000, size=1_000_000)` — next 1 MB
4. Assert:
   - Both calls skip memory expansion gas
   - Memory high-water mark is correct after both calls
   - Second call's memory expansion (from 1 MB to 2 MB) is also skipped

### Subtask 1h: Test — high-water mark bookkeeping is exact

1. Set PD return marker, call handler with 5 MB
2. Check `gas.memory().words_num` and `gas.memory().expansion_cost`
3. Assert `expansion_cost` equals what it would be for 5 MB of normal memory
   (even though that cost was not charged). This proves `set_words_num` was
   called correctly for subsequent operations.

---

## Task 2: EVM Integration Test — Large PD Read Through Contract

**File**: `crates/irys-reth/tests/pd_gas_elimination.rs` (new)
**Estimated complexity**: High

### Objective

Prove end-to-end that a contract calling the PD precompile and using
RETURNDATACOPY to copy the result into memory does NOT pay memory
expansion gas. This is the critical integration test.

### Subtask 2a: Raw EVM bytecode for a PD proxy contract

Construct minimal bytecode (no Solidity, no Foundry dependency) that:

```
// 1. Copy calldata (readData selector + args) to memory
CALLDATASIZE      // [cds]
PUSH1 0x00        // [0, cds]
PUSH1 0x00        // [0, 0, cds]
CALLDATACOPY      // memory[0..cds] = calldata

// 2. STATICCALL the PD precompile at 0x500
PUSH1 0x00        // retSize = 0 (we'll use RETURNDATACOPY)
PUSH1 0x00        // retOffset = 0
CALLDATASIZE      // argsSize
PUSH1 0x00        // argsOffset
PUSH2 0x0500      // address
GAS               // forward all gas
STATICCALL        // success flag on stack [0 or 1]

// 2b. Check success — REVERT if inner call failed
//     STATICCALL returns 1 on success, 0 on failure.
//     Without this, a failed precompile call would still produce
//     return data (revert bytes) that the proxy would happily copy
//     and RETURN, making the outer tx appear successful.
ISZERO            // [!success]
PUSH1 <revert>    // [revert_target, !success]
JUMPI             // jump to REVERT if inner call failed

// 3. Copy all return data to memory
RETURNDATASIZE    // [rds]
PUSH1 0x00        // [0, rds]
PUSH1 0x00        // [0, 0, rds]
RETURNDATACOPY    // memory[0..rds] = returndata  ← THIS is the opcode under test

// 4. Return all data
RETURNDATASIZE    // [rds]
PUSH1 0x00        // [0, rds]
RETURN            // return memory[0..rds]

// 5. Revert path (inner call failed)
JUMPDEST          // <revert> label
RETURNDATASIZE    // [rds]
PUSH1 0x00        // [0, rds]
PUSH1 0x00        // [0, 0, rds]
RETURNDATACOPY    // copy revert data
RETURNDATASIZE    // [rds]
PUSH1 0x00        // [0, rds]
REVERT            // bubble up inner revert
```

Encode this as a `Bytes` literal. The contract's **creation bytecode**
should use the `CODECOPY` + `RETURN` pattern to deploy the runtime
bytecode above.

### Subtask 2b: Test helper — deploy contract and provision chunks

```rust
fn setup_large_pd_test(
    num_chunks: u64,
    chunk_size: u64,
) -> (IrysEvmFactory, CacheDB<EmptyDB>, Address, AccessList) {
    // 1. Create ChunkConfig with production-like settings
    // 2. Populate chunk_data_index with num_chunks zero-filled chunks
    // 3. Build IrysEvmFactory with the chunk config + Sprite-enabled hardfork
    // 4. Create CacheDB seeded with:
    //    - IRYS_USD_PRICE_ACCOUNT (balance = 1e18)
    //    - TEST_CALLER (balance = U256::MAX / 2)
    //    - The proxy contract at a known address (with runtime bytecode)
    // 5. Build PdDataRead specifier for the full range
    // 6. Build AccessList with specifier + test fees
    // 7. Return factory, db, contract_address, access_list
}
```

Pre-deploy the contract by directly inserting its runtime bytecode into the
`CacheDB` via `insert_account_info` with the bytecode set. This avoids
needing a CREATE transaction and keeps the test focused.

### Subtask 2c: Test — 5 MB PD read succeeds within gas limit

```rust
#[test]
fn pd_5mb_read_through_contract_succeeds() {
    let (factory, db, contract_addr, access_list) =
        setup_large_pd_test(20, 256_000); // 20 chunks × 256,000 = 5,120,000 bytes

    let mut evm = factory.create_evm(db, evm_env());

    let tx = TxEnv {
        caller: TEST_CALLER,
        kind: TxKind::Call(contract_addr),
        data: encode_read_data(0).into(), // readData(0) ABI calldata
        access_list,
        gas_limit: 30_000_000,
        ..default_tx()
    };

    let result = evm.transact_raw(tx).unwrap();
    assert!(result.result.is_success(), "5 MB PD read should succeed");

    // Without gas elimination, memory expansion alone would cost ~50.5M gas (OOG).
    // With elimination, the dominant cost is copy gas: 3 * 160,002 = 480,006.
    // Plus: PD_BASE_GAS_COST (5000) + STATICCALL overhead + proxy opcodes + 21000 intrinsic.
    // Total should be well under 1M.
    let gas_used = result.result.gas_used();

    // Derive expected gas from exact return length:
    // return_bytes = 5,120,000 + 64 (ABI overhead) = 5,120,064
    // copy_words = ceil(5,120,064 / 32) = 160,002
    // copy_gas = 3 * 160,002 = 480,006
    let expected_copy_gas = 480_006_u64;
    assert!(
        gas_used < expected_copy_gas + 600_000, // copy gas + generous overhead
        "Gas used ({gas_used}) should be copy gas ({expected_copy_gas}) + overhead, not 50M+"
    );

    // Verify actual data was returned (ABI-encoded bytes)
    let output = result.result.into_output().unwrap();
    // The contract returns the precompile's ABI-encoded bytes memory
    assert!(output.len() > 5_100_000, "Should return ~5.12 MB of ABI-encoded data");
}
```

### Subtask 2d: Test — 10 MB PD read also succeeds

Same as 2c but with 40 chunks (10,240,000 bytes). Without elimination, memory
expansion would cost ~200M gas. With elimination, copy gas is ~960K.

```rust
#[test]
fn pd_10mb_read_through_contract_succeeds() {
    let (factory, db, contract_addr, access_list) =
        setup_large_pd_test(40, 256_000); // 40 × 256,000 = 10,240,000 bytes
    // copy_words = ceil(10,240,064 / 32) = 320,002
    // copy_gas = 3 * 320,002 = 960,006
    // assert gas_used < 960_006 + 600_000
}
```

### Subtask 2e: Test — gas scales with copy cost, not memory expansion

Run two PD reads of different sizes through the contract and verify that
the gas delta is proportional to copy gas (3 gas/word), not memory expansion
(which grows quadratically).

```rust
#[test]
fn pd_gas_scales_linearly_with_size() {
    // Read 1 MB (4 chunks × 256 KB)
    let gas_1mb = run_pd_read_through_contract(4, 256_000);
    // Read 5 MB (20 chunks × 256 KB)
    let gas_5mb = run_pd_read_through_contract(20, 256_000);

    // Copy gas is 3 per word. 5 MB has 5× the words of 1 MB.
    // So gas delta should be roughly 4× the 1 MB copy cost.
    // Memory expansion (quadratic) would make the ratio much larger.
    let ratio = gas_5mb as f64 / gas_1mb as f64;
    assert!(
        ratio < 7.0,
        "Gas ratio ({ratio:.1}) should be roughly linear (< 7×), not quadratic"
    );
}
```

---

## Task 3: Comparative Test — Non-PD RETURNDATACOPY Still Charges Gas

**File**: same as Task 2 (`crates/irys-reth/tests/pd_gas_elimination.rs`)
**Estimated complexity**: Medium

### Objective

Prove the gas elimination is **targeted** — only PD precompile return data
gets the exemption. A contract calling a non-PD source and using
RETURNDATACOPY on the result must still pay full memory expansion gas.

### Why not IDENTITY precompile

The naive approach (same proxy calling IDENTITY at 0x04 with large input)
is **fundamentally broken**: the proxy must first `CALLDATACOPY` the large
input into memory, which already expands memory before RETURNDATACOPY runs.
When RETURNDATACOPY writes back to offset 0, memory is already expanded →
zero expansion gas → the test proves nothing. Additionally, 5 MB calldata
costs 80M gas in intrinsic cost alone (16 gas/byte × 5M).

### Subtask 3a: Two-contract "returner" pattern

Deploy TWO contracts:

**Contract B ("returner")**: has a large byte array stored in its bytecode
or uses CODECOPY to generate a large return buffer at a fresh memory offset.
Returns N bytes via RETURN.

```
// Contract B: returns N zero-bytes (N encoded as first 4 bytes of calldata)
// 1. Read N from calldata
PUSH1 0x00
CALLDATALOAD      // [N_as_u256]
// 2. Allocate N bytes of memory (zeros)
DUP1              // [N, N]
PUSH1 0x00        // [0, N, N]
RETURN            // return memory[0..N] (memory auto-zeros on expansion)
```

**Contract A ("caller")**: same as the PD proxy but targeting Contract B
instead of 0x500. This means RETURNDATACOPY operates on non-PD return data
at a fresh memory offset.

```
// Contract A: CALL Contract B → RETURNDATACOPY at a FRESH memory offset
// 1. Store target return size in memory for Contract B's calldata
PUSH4 <size>      // desired return size
PUSH1 0x00
MSTORE            // memory[0..32] = size

// 2. CALL Contract B
PUSH1 0x00        // retSize = 0
PUSH1 0x00        // retOffset = 0
PUSH1 0x20        // argsSize = 32 (the uint256 size)
PUSH1 0x00        // argsOffset = 0
PUSH1 0x00        // value = 0
PUSH20 <addr_B>   // Contract B address
GAS
CALL              // success on stack

// 3. RETURNDATACOPY at a FRESH offset (not offset 0, which was already expanded)
RETURNDATASIZE
PUSH1 0x00        // srcOffset = 0
PUSH4 <fresh_off> // destOffset = some offset beyond current memory
RETURNDATACOPY    // THIS triggers memory expansion for non-PD data

// 4. Return
RETURNDATASIZE
PUSH4 <fresh_off>
RETURN
```

The critical detail: `destOffset` must be beyond any previously-touched
memory so that RETURNDATACOPY causes a real expansion. Using `destOffset =
0x10000` (64 KB) while the prior MSTORE only touched 32 bytes guarantees
the expansion is caused by RETURNDATACOPY, not CALLDATACOPY.

### Subtask 3b: Test — non-PD moderate RETURNDATACOPY charges memory gas

Use a moderate size (512 KB) that fits within the gas limit even with
memory expansion, and verify the gas includes the expansion cost.

```rust
#[test]
fn non_pd_512kb_returndatacopy_includes_memory_expansion_gas() {
    // 512 KB = 16,384 words
    // Memory expansion: 3 * 16384 + 16384² / 512 = 49,152 + 524,288 = 573,440 gas
    // Copy gas: 3 * 16384 = 49,152 gas
    // Total RETURNDATACOPY cost ≈ 622,592 gas + other overhead
    let result = evm.transact_raw(tx).unwrap();
    assert!(result.result.is_success());

    let gas_used = result.result.gas_used();
    // Must include memory expansion
    assert!(
        gas_used > 600_000,
        "Non-PD gas ({gas_used}) should include ~573K memory expansion"
    );
}
```

### Subtask 3c: Test — same-size PD read costs dramatically less gas

Run the same 512 KB through the PD proxy (Task 2 contract) and compare.

```rust
#[test]
fn pd_vs_non_pd_512kb_gas_comparison() {
    let gas_pd = run_pd_read(2, 256_000);       // 2 chunks = 512 KB through PD
    let gas_non_pd = run_non_pd_read(512_000);   // 512 KB through returner contract

    // PD path: no memory expansion → ~50K gas (copy only)
    // Non-PD path: memory expansion → ~622K gas
    assert!(
        gas_non_pd > gas_pd * 5,
        "Non-PD ({gas_non_pd}) should be much higher than PD ({gas_pd})"
    );
}
```

---

## Task 4: Test — Memory High-Water Mark Preserved After PD Copy

**File**: same as Task 2
**Estimated complexity**: Medium

### Objective

After a PD RETURNDATACOPY skips memory expansion gas, the `MemoryGas`
bookkeeping (`words_num` and `expansion_cost`) must still be updated so
that subsequent memory operations charge gas relative to the new high-water
mark. This is the most subtle correctness property.

### Subtask 4a: Bytecode for a two-phase contract

```
// Phase 1: STATICCALL PD precompile → RETURNDATACOPY (5 MB)
// Phase 2: MSTORE at offset 0 (within already-expanded memory)
// Phase 3: MSTORE at offset 5,120,064 + 32 (1 word beyond return data)
// Phase 4: RETURN
```

The key assertion: Phase 2 should cost 0 memory expansion gas (already
within high-water mark). Phase 3 should charge only the **incremental**
expansion — not the full 5 MB again.

**Gas math for Phase 3:** At ~5 MB memory (160,002 words), the incremental
cost of expanding by 1 word is:
`cost(160,003) - cost(160,002) = [3 × 160,003 + 160,003² / 512] - [3 × 160,002 + 160,002² / 512]`
= `3 + (160,003² - 160,002²) / 512` = `3 + (2 × 160,002 + 1) / 512` ≈ `3 + 625` = **~628 gas**.
Plus MSTORE base cost (3 gas) = ~631 total. Two words ≈ 1,260 gas.

### Subtask 4b: Test — subsequent MSTORE after PD copy doesn't double-charge

Run the same contract with and without the Phase 3 MSTORE and verify the
gas delta matches incremental expansion, not the full 50M+ of re-expanding
from zero.

```rust
#[test]
fn pd_copy_preserves_memory_high_water_mark() {
    let gas_with_extra_mstore = run_contract_with_mstore_after_pd_copy(true);
    let gas_without_extra_mstore = run_contract_with_mstore_after_pd_copy(false);

    let delta = gas_with_extra_mstore.saturating_sub(gas_without_extra_mstore);
    // Extra MSTORE 1 word past the PD copy should cost:
    // - MSTORE base gas (3) + incremental expansion for 1-2 words (~628-1260 gas)
    // Total delta should be under 2000, NOT millions.
    assert!(
        delta < 2_000,
        "Extra MSTORE delta ({delta}) should be ~630-1260 — high-water mark was preserved"
    );
    // Also assert it's not zero — the expansion DID happen
    assert!(
        delta > 500,
        "Extra MSTORE delta ({delta}) should be > 500 — incremental expansion was charged"
    );
}
```

---

## Task 5: Test — Transaction Boundary Isolation

**File**: same as Task 2
**Estimated complexity**: Low

### Objective

Verify that the PD return marker is properly cleared between transactions,
so a PD read in transaction N does not cause a non-PD RETURNDATACOPY in
transaction N+1 to skip memory expansion gas.

### Subtask 5a: Test — marker does not leak across transactions

```rust
#[test]
fn pd_marker_does_not_leak_across_transactions() {
    // Transaction 1: PD read through contract (5 MB) — should succeed, low gas
    let result1 = evm.transact_raw(pd_tx).unwrap();
    assert!(result1.result.is_success());
    let gas_pd = result1.result.gas_used();

    // Transaction 2: Non-PD identity call through same contract pattern (512 KB)
    let result2 = evm.transact_raw(non_pd_tx).unwrap();
    assert!(result2.result.is_success());
    let gas_non_pd = result2.result.gas_used();

    // Transaction 2 must include memory expansion gas — marker was cleared
    assert!(
        gas_non_pd > 500_000,
        "Non-PD tx gas ({gas_non_pd}) should include memory expansion — marker was cleared"
    );
}
```

---

## Task 6: Frame-Scope and Edge Case Tests

**File**: same as Task 2 (`crates/irys-reth/tests/pd_gas_elimination.rs`)
**Estimated complexity**: High

### Objective

The PD return marker is a **thread-local, flat slot** — it has no concept
of call frames. The semantic risks are:

1. **Nested calls**: `A → B → PD(0x500)`. When B's frame returns, A sees
   B's return data (not PD's). Does A's RETURNDATACOPY correctly charge
   memory expansion? (It should — B's return data is a different allocation.)

2. **Failed PD call**: If the PD precompile reverts, it produces revert data
   but should NOT set the marker. A subsequent RETURNDATACOPY on the revert
   data must charge memory expansion.

3. **Multiple PD calls in one transaction**: If a contract calls PD twice
   and uses RETURNDATACOPY after each, both should get the exemption.

### Subtask 6a: Test — nested call does NOT inherit PD exemption

Deploy three contracts:
- **Contract C**: calls PD precompile directly, returns the data
- **Contract D**: calls Contract C, then does RETURNDATACOPY on C's return data

D's RETURNDATACOPY sees C's return allocation (not PD's original), so the
marker should NOT match. Memory expansion gas must be charged.

```rust
#[test]
fn nested_call_does_not_inherit_pd_exemption() {
    // Contract C calls PD(0x500), returns data (512 KB)
    // Contract D calls C, then RETURNDATACOPY at fresh offset
    // D's RETURNDATACOPY should charge memory expansion because
    // D's return data buffer is C's RETURN output, not the PD allocation
    let result = evm.transact_raw(tx_calling_D).unwrap();
    let gas_used = result.result.gas_used();

    // If exemption leaked to D, gas would be low (~50K).
    // With proper isolation, gas includes memory expansion (~573K for 512 KB).
    assert!(
        gas_used > 500_000,
        "Nested call gas ({gas_used}) should include memory expansion — exemption is frame-local"
    );
}
```

### Subtask 6b: Test — failed PD call does not set marker

```rust
#[test]
fn failed_pd_call_does_not_exempt_returndatacopy() {
    // Trigger a PD precompile failure (e.g., invalid access list, missing chunks)
    // The precompile reverts → return data is the revert bytes
    // RETURNDATACOPY on the revert data must charge memory expansion
    // (The marker should not be set for a failed call)
}
```

### Subtask 6c: Test — two PD calls in one transaction both get exemption

```rust
#[test]
fn two_pd_calls_both_exempt() {
    // Contract calls PD(0x500) with readData(0) → RETURNDATACOPY (exempt)
    // Then calls PD(0x500) with readData(1) → RETURNDATACOPY (also exempt)
    // Both should skip memory expansion gas
    // This tests that set_pd_return_marker correctly updates for each call
}
```

---

## Implementation Order

```
Task 1 (primary)  ──→  Task 2  ──→  Task 3
                         │              │
                         ├──→ Task 4    │
                         ├──→ Task 5    │
                         └──→ Task 6 ───┘
```

- **Task 1** (unit tests): Start here. **Primary proof** of handler
  correctness. Fast iteration, exact gas assertions, covers source offsets,
  double-copy, and high-water bookkeeping.
- **Task 2** (integration — PD path): Proves the feature works end-to-end
  at scale through real RETURNDATACOPY execution.
- **Task 3** (comparative): Uses two-contract "returner" pattern. Proves
  non-PD RETURNDATACOPY still pays memory expansion.
- **Task 4** (high-water mark): Proves incremental expansion is correct.
- **Task 5** (transaction isolation): Proves marker doesn't leak across txs.
- **Task 6** (frame-scope): Proves nested calls, failed PD calls, and
  multi-PD-call semantics. This covers the real semantic risks.

## Prerequisites

Before starting implementation:

1. **Rebase `rob/memory-expansion` onto `origin/feat/pd`**. The feat/pd
   branch refactored the precompile ABI (readData/readBytes selectors,
   PdDataRead specifier, ABI-encoded returns). Our RETURNDATACOPY handler
   and marker code are orthogonal to this refactor but the test code must
   use the new ABI.

2. **Verify all 4 existing commits still compile and pass** after rebase.

## Open Questions

1. **Interpreter construction**: Can we build a standalone `Interpreter`
   (or `EthInterpreter`) in unit tests with a controlled stack, memory, and
   return data buffer? Investigate revm 32.0.0 public APIs. If infeasible,
   use the full EVM approach but still ensure handler-level assertions are
   covered via gas arithmetic. Task 1 must not be skipped entirely.

2. **Bytecode assembly**: Hand-roll the ~30 bytes of runtime bytecode for
   Tasks 2-6. The proxy now includes success checking (ISZERO + JUMPI +
   REVERT path), making it ~35 bytes. For the two-contract Task 3 pattern,
   the returner contract is ~10 bytes. No external bytecode crate needed.
   Consider a tiny Yul fixture if the hand-rolled bytecode becomes unwieldy
   for Task 6's nested-call scenarios.

3. **Test chunk content**: Tests use zero-filled chunks. Content correctness
   is already covered by chain-level tests (`pd_content_verification`), so
   gas tests can safely use zeros.

4. **Frame semantics of the marker**: Task 6a is the most important
   open question. When Contract C calls PD and returns, does C's RETURN
   create a new `Bytes` allocation for D's return data buffer? If revm
   copies return data between frames (new allocation), the marker naturally
   won't match and Task 6a passes. If revm reuses the same `Bytes` handle
   across frames, the marker WOULD match incorrectly, which is a real bug.
   Investigate revm's `RETURN` → parent frame `return_data` flow before
   implementing.

## Review History

- **2026-03-24 v1**: Initial plan.
- **2026-03-24 v2**: Updated after Codex (gpt-5.4) review. Key fixes:
  - Redesigned Task 3: replaced broken IDENTITY approach with two-contract
    "returner" pattern that avoids pre-expanding memory via CALLDATACOPY.
  - Added ISZERO + REVERT to proxy bytecode (Task 2a) to check inner call
    success — prevents silently returning revert data as success.
  - Fixed gas math: use exact byte counts (20 × 256,000 = 5,120,000, not
    5,242,880 MiB) and derive assertions from actual return buffer sizes.
  - Fixed Task 4 delta threshold from `< 100` to `< 2,000` (incremental
    expansion at 160K words costs ~628 gas/word, not ~6).
  - Promoted Task 1 from optional to **primary** proof of correctness.
  - Added Task 6: frame-scope tests (nested calls, failed PD calls,
    multi-PD calls) — the real semantic risks of the thread-local marker.
  - Added subtasks 1f (non-zero source offset), 1g (multiple
    RETURNDATACOPY ops), 1h (exact high-water bookkeeping).
