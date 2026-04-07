# Research: PD Precompile Solidity API Ergonomics Review

**Date**: 2026-03-17
**Git Commit**: f0c9a860
**Branch**: feat/pd

## Research Question
Is the PD precompile Solidity API ergonomic? Does it provide good access to downstream users? What would best-in-class look like given we're pre-launch and can break things?

## Summary

The current API is functional but uses a **low-level manual encoding pattern** that forces downstream Solidity developers to inherit a base contract and think in terms of raw `staticcall` + `bytes.concat`. The ecosystem consensus (Arbitrum, Moonbeam, Avalanche, Optimism) is that precompiles should expose a **standard ABI-encoded interface** so callers can use normal Solidity function call syntax. The current design also has several specific ergonomic issues worth addressing.

---

## Current API Surface

### Solidity Side (`fixtures/contracts/src/`)

**`Precompiles.sol`** - Constants:
```solidity
address constant PD_READ_PRECOMPILE_ADDRESS = address(0x500);
uint8 constant READ_FULL_BYTE_RANGE = 0;
uint8 constant READ_PARTIAL_BYTE_RANGE = 1;
```

**`ProgrammableData.sol`** - Base contract (4 functions, 2 active):
| Function | Params | Returns | What it does |
|----------|--------|---------|-------------|
| `readBytes()` | none | `(bool, bytes)` | Reads all of byte range index 0 |
| `readBytes(offset, length)` | `uint32, uint32` | `(bool, bytes)` | Reads slice of byte range index 0 |
| `readByteRange(index)` | `uint8` | `(bool, bytes)` | Reads all of byte range at index |
| `readByteRange(index, offset, length)` | `uint8, uint32, uint32` | `(bool, bytes)` | Reads slice of byte range at index |

Under the hood, all functions do:
```solidity
address(PD_READ_PRECOMPILE_ADDRESS).staticcall(
    bytes.concat(bytes1(FUNCTION_ID), bytes1(index), ...)
)
```

**`IrysProgrammableDataBasic.sol`** - Example consumer:
```solidity
contract ProgrammableDataBasic is ProgrammableData {
    function readPdChunkIntoStorage() public {
        (bool success, bytes memory data) = readBytes();
        require(success, "reading bytes failed");
        storedData = data;
    }
}
```

### Rust Side (`crates/irys-reth/src/precompiles/pd/`)

- Precompile at `0x500` dispatches on first byte: `0x00` = ReadFullByteRange, `0x01` = ReadPartialByteRange
- Returns raw bytes (not ABI-encoded) - the actual chunk data
- Access list carries `ChunkRangeSpecifier` + `ByteRangeSpecifier` as encoded B256 storage keys
- Gas: 5000 base + per-operation costs

### Transaction Construction (off-chain)

Users must:
1. Query `/v1/price/pd/fee-history` for fee info
2. Build `ChunkRangeSpecifier` + `ByteRangeSpecifier` → encode as B256 → pack into EIP-2930 access list
3. Build `PdHeaderV1` (priority + max base fee per chunk) → Borsh-serialize → prepend magic header to calldata
4. Submit as EIP-1559 transaction

---

## Issues Identified

### Issue 1: Contract inheritance instead of interface/library

**Current**: `ProgrammableData` is a `contract` that users must inherit (`is ProgrammableData`).

**Problem**:
- Contracts using PD inherit an entire base contract just to get 4 helper functions
- The functions are `public` but called internally - should be `internal` to avoid polluting the ABI
- Cannot compose with other inheritance hierarchies without diamond problems
- No standalone interface exists for direct precompile calls

**Industry pattern**: Arbitrum uses `interface`, OpenZeppelin uses `library`. Both are better for composition.

### Issue 2: `(bool success, bytes memory)` return pattern leaks low-level details

**Current**: Every function returns `(bool success, bytes memory data)` and leaves error handling to the caller:
```solidity
(bool success, bytes memory data) = readBytes();
require(success, "reading bytes failed"); // string revert, no error info
```

**Problems**:
- Caller must check `success` every time - boilerplate
- If `success` is `false`, there's no way to know WHY (the error bytes from the precompile are discarded)
- String reverts (`"reading bytes failed"`) are gas-expensive and unparseable
- No `try/catch` pattern available since it's a `staticcall`, not an external call to a typed interface

**Industry pattern**: Provide both strict functions (revert with custom errors) AND `try*` variants:
```solidity
// Strict - reverts with custom error
function readByteRange(uint8 index) → bytes memory

// Try variant - returns success flag
function tryReadByteRange(uint8 index) → (bool, bytes memory)
```

### Issue 3: Raw byte encoding instead of ABI encoding

**Current**: The precompile uses custom 1-byte function ID dispatch:
```
[function_id: u8][index: u8][...params as raw bytes]
```

**Problems**:
- Can't use `abi.encodeCall()` - must manually `bytes.concat()`
- Can't use Solidity `interface` pattern - the precompile doesn't understand 4-byte selectors
- No standard tooling support (etherscan verification, ABI decoders, etc.)
- Return data is raw bytes, not ABI-encoded

**Industry pattern**: Arbitrum, Moonbeam, Avalanche all use standard 4-byte ABI selector dispatch. This enables:
```solidity
// Instead of this:
address(0x500).staticcall(bytes.concat(bytes1(0), bytes1(index)))

// Users write this:
IIrysPD(address(0x500)).readByteRange(index)
```

**Tradeoff**: ABI encoding adds ~30 bytes overhead per call. For a precompile that returns potentially large byte arrays, this overhead is negligible. The gas cost difference is minimal (a few hundred gas for ABI decoding vs the 5000+ base cost).

### Issue 4: No custom errors on the Solidity side

**Current**: The Rust side has rich error types (`PdPrecompileError` with 13 variants), but the Solidity side has no `error` declarations. Callers just see `success = false` with no context.

**Industry pattern**: Define errors on the interface:
```solidity
error InsufficientInput(uint256 expected, uint256 actual);
error MissingAccessList();
error ByteRangeNotFound(uint8 index, uint256 available);
error ByteRangeOutOfBounds(uint256 start, uint256 end, uint256 available);
```

These would be ABI-encoded in the revert data, parseable by ethers.js/viem/alloy.

### Issue 5: ByteRangeSpecifier.index cross-references ChunkRangeSpecifier by position

**Current**: `ByteRangeSpecifier.index` refers to the positional index of a `ChunkRangeSpecifier` in the access list. This creates an implicit coupling where:
- Order of access list entries matters
- Users must mentally track which chunk range is at which position
- Adding/removing a chunk range shifts all byte range references

This is a protocol-level design issue, not just Solidity API, but worth noting as it affects developer experience.

### Issue 6: Commented-out `readBytesFromChunkRange` functions

`ProgrammableData.sol` has 3 commented-out functions (lines 88-124) including a `BytesRangeSpecifier` struct. This suggests the API is still evolving but the dead code shouldn't ship.

### Issue 7: Functions are `public` instead of `internal`

All `ProgrammableData` functions are marked `public`, meaning:
- They appear in the contract's ABI (polluting it for inheriting contracts)
- They can be called externally, which is pointless since they just proxy to the precompile
- They cost more gas when called internally (external dispatch overhead)

Should be `internal` since they're helper functions meant to be called from within the inheriting contract.

---

## Recommended API Design

### Option A: ABI-Compatible Interface (Recommended)

Requires changing the Rust precompile to use 4-byte ABI selector dispatch instead of raw 1-byte function IDs.

**`IIrysPD.sol`** - The canonical interface:
```solidity
interface IIrysPD {
    /// @notice Read all bytes from a byte range in the access list
    /// @param index The byte range index (0-255)
    /// @return data The raw bytes
    function readByteRange(uint8 index) external view returns (bytes memory data);

    /// @notice Read a slice from a byte range
    function readPartialByteRange(uint8 index, uint32 offset, uint32 length)
        external view returns (bytes memory data);

    error InsufficientInput(uint256 expected, uint256 actual);
    error MissingAccessList();
    error ByteRangeNotFound(uint8 index, uint256 available);
    error ByteRangeOutOfBounds(uint256 start, uint256 end, uint256 available);
}

IIrysPD constant IRYS_PD = IIrysPD(address(0x500));
```

**`IrysPDLib.sol`** - Convenience library:
```solidity
library IrysPDLib {
    function readBytes() internal view returns (bytes memory) {
        return IRYS_PD.readByteRange(0);
    }

    function readBytes(uint32 offset, uint32 length) internal view returns (bytes memory) {
        return IRYS_PD.readPartialByteRange(0, offset, length);
    }

    function tryReadByteRange(uint8 index)
        internal view returns (bool success, bytes memory data)
    {
        (success, data) = address(IRYS_PD).staticcall(
            abi.encodeCall(IIrysPD.readByteRange, (index))
        );
    }
}
```

**User code**:
```solidity
import {IRYS_PD} from "./IIrysPD.sol";
import {IrysPDLib} from "./IrysPDLib.sol";

contract MyApp {
    using IrysPDLib for *;

    function processData() external view returns (bytes memory) {
        // Option 1: Direct interface call (reverts on error with custom error)
        return IRYS_PD.readByteRange(0);

        // Option 2: Library convenience (reverts on error)
        return IrysPDLib.readBytes();

        // Option 3: Try variant (returns bool)
        (bool ok, bytes memory data) = IrysPDLib.tryReadByteRange(0);
    }
}
```

**Rust changes needed**: The precompile must detect 4-byte ABI selectors and ABI-decode the parameters. This is well-supported by alloy's `sol!` macro - you can generate the function selectors and encode/decode at compile time.

### Option B: Keep Raw Encoding, Improve Solidity Wrapper (Minimal change)

Keep the current 1-byte function ID encoding but fix the Solidity wrapper:

1. Change `ProgrammableData` from `contract` to `library` with `internal` functions
2. Add custom error declarations
3. Add strict variants that decode the revert data and re-throw as custom errors
4. Add `try*` variants that return `(bool, bytes)`
5. Remove commented-out code
6. Add NatSpec documentation

---

## Comparison Matrix

| Aspect | Current | Option A (ABI Interface) | Option B (Improved Wrapper) |
|--------|---------|-------------------------|---------------------------|
| Call syntax | `readBytes()` via inheritance | `IRYS_PD.readByteRange(0)` | `IrysPDLib.readBytes()` |
| Error handling | `(bool, bytes)` only | Custom errors + try variants | Custom errors + try variants |
| Composition | Inheritance required | Interface or library | Library |
| Tooling | Manual encoding | Standard ABI tools work | Manual encoding still |
| Gas overhead | Minimal | +~200 gas ABI encode/decode | Minimal |
| Rust changes | None | Moderate (ABI dispatch) | None |
| Industry alignment | Non-standard | Matches Arbitrum/Moonbeam | Halfway |
| Extensibility | Add function IDs | Add interface methods | Add function IDs |

---

## Architecture Note: Access List Construction

Regardless of Solidity API changes, the off-chain construction of PD transactions (access list + PD header) remains complex. This is inherent to the protocol design and is primarily an SDK concern (irys-js). The Solidity API only affects what happens inside EVM execution.

## Code References

- `fixtures/contracts/src/ProgrammableData.sol` - Current Solidity wrapper
- `fixtures/contracts/src/Precompiles.sol` - Constants
- `fixtures/contracts/src/IrysProgrammableDataBasic.sol` - Example consumer
- `crates/irys-reth/src/precompiles/pd/precompile.rs` - Rust precompile entry point
- `crates/irys-reth/src/precompiles/pd/functions.rs` - Function ID dispatch
- `crates/irys-reth/src/precompiles/pd/read_bytes.rs` - Read implementations
- `crates/irys-reth/src/precompiles/pd/error.rs` - Rich Rust error types (13 variants)
- `crates/irys-reth/src/precompiles/pd/constants.rs` - Gas costs
- `crates/irys-reth/src/pd_tx.rs` - Transaction construction helpers
- `crates/chain-tests/src/programmable_data/basic.rs:265-290` - Real usage showing access list boilerplate
