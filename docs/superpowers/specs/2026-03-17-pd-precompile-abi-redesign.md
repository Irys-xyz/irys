# Design Spec: PD Precompile ABI Redesign

**Date**: 2026-03-17
**Branch**: feat/pd
**Status**: Draft
**Breaking**: Yes — changes precompile calldata/return encoding

## 1. What This Enables

This redesign brings the PD precompile in line with the industry standard set by Arbitrum, Moonbeam, Avalanche, and Optimism. Concretely, it enables:

**For Solidity developers building on Irys:**

- **Natural call syntax** — `IRYS_PD.readByteRange(0)` instead of `address(0x500).staticcall(bytes.concat(bytes1(0), bytes1(0)))`. Contracts read like business logic, not byte manipulation.
- **Typed custom errors** — When a read fails, callers get structured errors like `ByteRangeNotFound(index, available)` instead of an opaque `success = false`. This enables precise error handling in Solidity (`try/catch` with specific error types) and clear diagnostics during development.
- **Composability without inheritance** — Contracts use `IRYS_PD.readByteRange(0)` or `IrysPDLib.readBytes()` directly. No forced `is ProgrammableData` inheritance, no ABI pollution, no diamond-problem risks when combining with other base contracts.
- **Both strict and graceful error patterns** — Direct interface calls revert with custom errors (the common case). Library `try*` variants return `(bool, bytes)` for contracts that need to handle failure without reverting.

**For off-chain tooling and integrators:**

- **Standard ABI works everywhere** — Etherscan can display and verify the precompile's interface. Block explorers decode PD calls and errors automatically. No custom decoding plugins needed.
- **SDK error decoding** — ethers.js, viem, and alloy can parse PD revert data into typed error objects using the interface ABI, enabling clear error messages in dapp UIs and debugging tools.
- **`abi.encodeCall` support** — Transaction builders and testing frameworks can use Solidity's type-safe encoding (`abi.encodeCall(IIrysPD.readByteRange, (0))`) instead of manual byte packing.

**For protocol evolution:**

- **Extensibility via interface versioning** — New read operations are added as new functions on the interface (e.g., `readMultipleByteRanges`). Existing function selectors and their behavior never change. This follows the Arbitrum pattern of additive-only interface evolution.
- **Standard toolchain integration** — Foundry/Hardhat test helpers, fuzzing tools, and static analyzers understand standard ABI calls out of the box. No special handling needed for PD reads.

**What does NOT change:** The PD transaction envelope (on-chain `irys-pd-meta` header, EIP-2930 access list encoding, fee model) is entirely unaffected. This redesign only changes the internal EVM call from contract to precompile.

## 2. Problem Statement

The Programmable Data (PD) precompile at address `0x500` uses a custom 1-byte function ID dispatch format. Downstream Solidity developers must use manual `bytes.concat()` + `staticcall` through an inherited base contract. This is non-standard, limits tooling support, and provides poor error feedback.

The code is not live. We can make breaking changes to ship a best-in-class API before launch.

## 3. Background: How PD Works Today

### 3.1 Architecture Overview

PD enables smart contracts to read large data stored in Irys partitions. The data flow has two completely separate layers:

```
Layer 1: PD Transaction Envelope (on-chain, in EVM tx calldata)
┌─────────────────────────────────────────────────────┐
│ [irys-pd-meta magic:12][version:u16][PdHeaderV1:64] │  ← Parsed by Irys node
│ + EIP-2930 access list with packed B256 keys        │    before EVM execution
└─────────────────────────────────────────────────────┘

Layer 2: Precompile Call (inside EVM, contract → 0x500)
┌─────────────────────────────────────────────────────┐
│ contract.staticcall(0x500, encoded_params)           │  ← THIS is what changes
│ → returns chunk bytes                                │
└─────────────────────────────────────────────────────┘
```

**This design only changes Layer 2.** Layer 1 (PD transaction envelope, access list encoding, PD header format, `pd_tx.rs`) is entirely unaffected.

### 3.2 Current Solidity API

**`Precompiles.sol`** — Constants:
```solidity
address constant PD_READ_PRECOMPILE_ADDRESS = address(0x500);
uint8 constant READ_FULL_BYTE_RANGE = 0;
uint8 constant READ_PARTIAL_BYTE_RANGE = 1;
```

**`ProgrammableData.sol`** — Base contract with 4 public functions:
```solidity
contract ProgrammableData {
    function readBytes() public view returns (bool success, bytes memory data) {
        return readByteRange(0);
    }

    function readBytes(uint32 relative_offset, uint32 length)
        public view returns (bool success, bytes memory result)
    {
        return readByteRange(0, relative_offset, length);
    }

    function readByteRange(uint8 byte_range_index)
        public view returns (bool success, bytes memory data)
    {
        return address(PD_READ_PRECOMPILE_ADDRESS).staticcall(
            bytes.concat(bytes1(READ_FULL_BYTE_RANGE), bytes1(byte_range_index))
        );
    }

    function readByteRange(uint8 byte_range_index, uint32 start_offset, uint32 length)
        public view returns (bool success, bytes memory data)
    {
        return address(PD_READ_PRECOMPILE_ADDRESS).staticcall(
            bytes.concat(
                bytes1(READ_PARTIAL_BYTE_RANGE),
                bytes1(byte_range_index),
                bytes4(start_offset),
                bytes4(length)
            )
        );
    }
}
```

**`IrysProgrammableDataBasic.sol`** — Example consumer:
```solidity
contract ProgrammableDataBasic is ProgrammableData {
    bytes public storedData;

    function readPdChunkIntoStorage() public {
        (bool success, bytes memory data) = readBytes();
        require(success, "reading bytes failed");
        storedData = data;
    }
}
```

### 3.3 Current Rust Precompile

**Calldata format** — Custom packed binary:
```
ReadFullByteRange:    [0x00:1][index:1]           = 2 bytes
ReadPartialByteRange: [0x01:1][index:1][offset:4][length:4] = 10 bytes
```

**Dispatch** (`precompile.rs`):
```rust
let decoded_id = PdFunctionId::try_from(data[0])?;
match decoded_id {
    PdFunctionId::ReadFullByteRange => read_bytes_range_by_index(...),
    PdFunctionId::ReadPartialByteRange => read_partial_byte_range(...),
}
```

**Return data**: Raw bytes — the exact chunk data with no encoding wrapper.

**Errors**: All 15 variants of `PdPrecompileError` are converted to `PrecompileError::Other(String)`, losing structure:
```rust
impl From<PdPrecompileError> for PrecompileError {
    fn from(e: PdPrecompileError) -> Self {
        Self::Other(Cow::Owned(format!("PD precompile: {}", e)))
    }
}
```

### 3.4 Current Rust Files

| File | Purpose |
|------|---------|
| `crates/irys-reth/src/precompiles/pd/precompile.rs` | Entry point, dispatch, registration |
| `crates/irys-reth/src/precompiles/pd/functions.rs` | `PdFunctionId` enum + TryFrom |
| `crates/irys-reth/src/precompiles/pd/read_bytes.rs` | Read implementations + arg decoding |
| `crates/irys-reth/src/precompiles/pd/constants.rs` | Gas costs, raw encoding offsets/sizes |
| `crates/irys-reth/src/precompiles/pd/error.rs` | `PdPrecompileError` (15 variants) |
| `crates/irys-reth/src/precompiles/pd/context.rs` | `PdContext` (access list + chunk provider) |
| `crates/irys-reth/src/precompiles/pd/utils.rs` | Access list parsing |
| `crates/irys-reth/src/precompiles/pd/mod.rs` | Module root |

## 4. Issues With the Current Design

### 4.1 Non-standard dispatch prevents interface pattern

The precompile uses 1-byte function IDs instead of 4-byte ABI selectors. This means Solidity cannot call it through a typed `interface` — the compiler always emits 4-byte selectors for external calls. Users are forced to use raw `staticcall` with manual `bytes.concat()` encoding.

**Industry comparison**: Arbitrum (`ArbSys`), Moonbeam, Avalanche, and Optimism precompiles all use standard ABI selector dispatch, enabling:
```solidity
// Arbitrum: ArbSys(address(100)).arbBlockNumber()
// Moonbeam: ParachainStaking(STAKING_ADDR).isDelegator(addr)
// Irys (current): address(0x500).staticcall(bytes.concat(bytes1(0), bytes1(0)))
```

### 4.2 Contract inheritance instead of interface/library

`ProgrammableData` is a `contract` that users must inherit. This:
- Pollutes the ABI with 4 `public` functions that should be `internal`
- Forces inheritance where composition would be cleaner
- Creates diamond-problem risks with other base contracts

**Industry comparison**: Arbitrum uses `interface`, OpenZeppelin/Solady use `library`. Both are composable without inheritance.

### 4.3 `(bool, bytes)` return with no error context

Every function returns `(bool success, bytes memory data)`. When `success` is false, the caller has no way to know why — the precompile's error information is discarded by the `staticcall` wrapper. Callers write `require(success, "reading bytes failed")` — a string revert that is gas-expensive and unparseable.

**Industry comparison**: Arbitrum and Moonbeam define custom errors on their interfaces. OpenZeppelin provides both strict (reverting) and `try*` (bool-returning) variants.

### 4.4 Rich Rust errors lost in translation

The Rust side has 15 well-structured error variants (`PdPrecompileError`) with specific fields. All of them are flattened to `PrecompileError::Other(String)`, which Solidity receives as an opaque revert. The error structure exists but is invisible to callers.

### 4.5 Dead code in the Solidity contract

`ProgrammableData.sol` contains 37 lines of commented-out functions (`readBytesFromChunkRange`, `BytesRangeSpecifier` struct) that suggest an incomplete API evolution.

## 5. Industry References

### 5.1 Arbitrum ArbSys — Interface at Fixed Address

```solidity
interface ArbSys {
    function arbBlockNumber() external view returns (uint256);
    function arbBlockHash(uint256 arbBlockNum) external view returns (bytes32);
    error InvalidBlockNumber(uint256 requested, uint256 current);
}
// Usage: ArbSys(address(100)).arbBlockNumber()
```

- `interface` (not contract) — no inheritance required
- Custom errors defined on the interface
- Standard ABI dispatch — callers use normal Solidity syntax
- Source: [github.com/OffchainLabs/nitro-contracts](https://github.com/OffchainLabs/nitro-contracts)

### 5.2 Moonbeam — Interface with Rich Function Sets

```solidity
interface ParachainStaking {
    /// @custom:selector 8e5080e7
    function isDelegator(address delegator) external view returns (bool);
    function delegate(address candidate, uint256 amount, ...) external;
}
```

- 42+ functions on a single precompile interface
- `@custom:selector` NatSpec tags document 4-byte selectors
- Events defined on the interface for indexing
- Source: [github.com/moonbeam-foundation/moonbeam](https://github.com/moonbeam-foundation/moonbeam)

### 5.3 EIP-4844 Point Evaluation — Raw Precompile

```solidity
(bool ok, bytes memory data) = address(0x0A).staticcall(
    abi.encodePacked(versionedHash, z, y, commitment, proof)
);
```

- Uses `abi.encodePacked` — custom binary encoding, not ABI selectors
- No interface wrapper — callers must manually encode/decode
- This is the pattern Irys PD currently follows. It's appropriate for Ethereum's built-in precompiles (stable, rarely called directly) but not for a chain's primary developer-facing feature.

### 5.4 OpenZeppelin ECDSA — Library Pattern

```solidity
library ECDSA {
    error ECDSAInvalidSignature();
    function recover(bytes32 hash, bytes memory signature) internal pure returns (address) { ... }
    function tryRecover(bytes32 hash, bytes memory sig) internal pure returns (address, RecoverError, bytes32) { ... }
}
```

- `internal` functions — inlined, no external call overhead
- Both strict (`recover` — reverts) and try-variants (`tryRecover` — returns error)
- Custom errors for structured failure reporting
- Source: [github.com/OpenZeppelin/openzeppelin-contracts](https://github.com/OpenZeppelin/openzeppelin-contracts)

### 5.5 Optimism L1Block — Predeploy with ISemver

```solidity
contract L1Block is ISemver {
    uint64 public number;
    uint256 public basefee;
    function version() public pure returns (string memory) { return "1.8.0"; }
}
```

- `ISemver` interface for versioning via `version()` function
- New hardforks add new setter functions rather than modifying existing ones
- Source: [github.com/ethereum-optimism/optimism](https://github.com/ethereum-optimism/optimism)

## 6. Proposed Design

### 6.1 Design Principles

1. **Standard ABI dispatch** — 4-byte selectors so the precompile can be called through a Solidity `interface`
2. **Interface + Library layering** — interface is the protocol, library is ergonomics. No inheritance required.
3. **Strict by default, try-variants opt-in** — direct calls revert with custom errors; library provides `try*` functions
4. **Only Layer 2 changes** — PD transaction envelope, access list encoding, and PD header format are untouched

### 6.2 Solidity: `IIrysPD.sol` — The Interface

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title IIrysPD — Irys Programmable Data Precompile Interface
/// @notice Read byte ranges from Irys partition storage via EIP-2930 access lists
/// @dev Precompile at address 0x500. Data ranges are specified in the transaction's
///      access list as encoded ChunkRangeSpecifier + ByteRangeSpecifier storage keys.
interface IIrysPD {
    /// @notice Read all bytes from a byte range specified in the access list
    /// @param index The byte range index in the access list (0-255)
    /// @return data The raw bytes from the specified range
    function readByteRange(uint8 index) external view returns (bytes memory data);

    /// @notice Read a subset of bytes from a byte range
    /// @param index The byte range index in the access list (0-255)
    /// @param offset Starting byte offset within the range
    /// @param length Number of bytes to read
    /// @return data The requested byte slice
    function readPartialByteRange(uint8 index, uint32 offset, uint32 length)
        external view returns (bytes memory data);

    /// @notice Transaction has no access list entries for the PD precompile
    error MissingAccessList();

    /// @notice Requested byte range index exceeds available byte ranges
    /// @param index The requested index
    /// @param available Number of byte ranges in the access list
    error ByteRangeNotFound(uint8 index, uint256 available);

    /// @notice Byte range references a chunk range index that doesn't exist
    /// @param index The referenced chunk range index
    /// @param available Number of chunk ranges in the access list
    error ChunkRangeNotFound(uint8 index, uint256 available);

    /// @notice Requested byte slice exceeds the available data
    /// @param start Start offset of the requested range
    /// @param end End offset of the requested range (exclusive)
    /// @param available Total bytes available
    error ByteRangeOutOfBounds(uint256 start, uint256 end, uint256 available);

    /// @notice Chunk not found at the given ledger offset
    /// @param offset The ledger chunk offset
    error ChunkNotFound(uint64 offset);

    /// @notice Failed to fetch chunk from storage
    /// @param offset The ledger chunk offset
    error ChunkFetchFailed(uint64 offset);
}

/// @dev Precompile address for Irys Programmable Data
address constant IRYS_PD_ADDRESS = address(0x500);

/// @dev Typed constant for direct interface calls: IRYS_PD.readByteRange(0)
IIrysPD constant IRYS_PD = IIrysPD(IRYS_PD_ADDRESS);
```

**Replaces**: `Precompiles.sol` and `ProgrammableData.sol`.

### 6.3 Solidity: `IrysPDLib.sol` — The Convenience Library

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {IIrysPD, IRYS_PD, IRYS_PD_ADDRESS} from "./IIrysPD.sol";

/// @title IrysPDLib — Convenience library for Irys Programmable Data reads
/// @notice Provides shorthand functions and try-variants for PD precompile access
library IrysPDLib {
    /// @notice Read all bytes from the first byte range (index 0)
    /// @dev Reverts with IIrysPD custom errors on failure
    function readBytes() internal view returns (bytes memory) {
        return IRYS_PD.readByteRange(0);
    }

    /// @notice Read a slice from the first byte range (index 0)
    /// @dev Reverts with IIrysPD custom errors on failure
    function readBytes(uint32 offset, uint32 length) internal view returns (bytes memory) {
        return IRYS_PD.readPartialByteRange(0, offset, length);
    }

    /// @notice Try to read a full byte range, returning success flag instead of reverting
    /// @param index The byte range index in the access list
    /// @return success Whether the read succeeded
    /// @return data The raw bytes (empty if success is false)
    function tryReadByteRange(uint8 index)
        internal view returns (bool success, bytes memory data)
    {
        (success, data) = IRYS_PD_ADDRESS.staticcall(
            abi.encodeCall(IIrysPD.readByteRange, (index))
        );
    }

    /// @notice Try to read a partial byte range, returning success flag instead of reverting
    /// @param index The byte range index in the access list
    /// @param offset Starting byte offset within the range
    /// @param length Number of bytes to read
    /// @return success Whether the read succeeded
    /// @return data The raw bytes (empty if success is false)
    function tryReadPartialByteRange(uint8 index, uint32 offset, uint32 length)
        internal view returns (bool success, bytes memory data)
    {
        (success, data) = IRYS_PD_ADDRESS.staticcall(
            abi.encodeCall(IIrysPD.readPartialByteRange, (index, offset, length))
        );
    }
}
```

### 6.4 Solidity: Updated Example Consumer

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {IRYS_PD} from "./IIrysPD.sol";
import {IrysPDLib} from "./IrysPDLib.sol";

contract ProgrammableDataBasic {
    bytes public storedData;

    /// @notice Read PD data using direct interface call (reverts on error)
    function readPdChunkIntoStorage() public {
        storedData = IRYS_PD.readByteRange(0);
    }

    /// @notice Read PD data using library convenience
    function readPdChunkIntoStorageAlt() public {
        storedData = IrysPDLib.readBytes();
    }

    /// @notice Read PD data with graceful error handling
    function tryReadPdChunk() public returns (bool) {
        (bool success, bytes memory data) = IrysPDLib.tryReadByteRange(0);
        if (success) {
            storedData = data;
        }
        return success;
    }

    function getStorage() public view returns (bytes memory) {
        return storedData;
    }
}
```

### 6.5 Rust: Precompile Dispatch Redesign

**Use `alloy::sol!` to generate selectors, decoders, and error encoders:**

```rust
use alloy_sol_types::sol;

sol! {
    function readByteRange(uint8 index) external view returns (bytes memory data);
    function readPartialByteRange(uint8 index, uint32 offset, uint32 length)
        external view returns (bytes memory data);

    error MissingAccessList();
    error ByteRangeNotFound(uint8 index, uint256 available);
    error ChunkRangeNotFound(uint8 index, uint256 available);
    error ByteRangeOutOfBounds(uint256 start, uint256 end, uint256 available);
    error ChunkNotFound(uint64 offset);
    error ChunkFetchFailed(uint64 offset);
}
```

This generates at compile time:
- `readByteRangeCall::SELECTOR` — the 4-byte selector
- `readByteRangeCall::abi_decode(data)` — parameter decoder
- `readByteRangeCall::abi_encode_returns(&(result,))` — return encoder
- `MissingAccessList {}` — error struct with `abi_encode()` for revert data

**New dispatch logic** (replaces `PdFunctionId` enum):

```rust
fn pd_precompile(pd_context: PdContext) -> DynPrecompile {
    (move |input: PrecompileInput<'_>| -> PrecompileResult {
        let data = input.data;
        if data.len() < 4 {
            return Err(PrecompileError::Other("calldata too short".into()));
        }

        // ... gas check, access list parsing (unchanged) ...

        let selector: [u8; 4] = data[..4].try_into().unwrap();
        match selector {
            readByteRangeCall::SELECTOR => {
                let decoded = readByteRangeCall::abi_decode(&data[4..])
                    .map_err(|_| PrecompileError::Other("ABI decode failed".into()))?;
                let result = read_bytes_range_by_index(decoded.index, ...)?;
                let encoded = readByteRangeCall::abi_encode_returns(&(result.into(),));
                Ok(PrecompileOutput { bytes: encoded.into(), .. })
            }
            readPartialByteRangeCall::SELECTOR => {
                let decoded = readPartialByteRangeCall::abi_decode(&data[4..])
                    .map_err(|_| PrecompileError::Other("ABI decode failed".into()))?;
                let result = read_partial_byte_range(decoded.index, decoded.offset, decoded.length, ...)?;
                let encoded = readPartialByteRangeCall::abi_encode_returns(&(result.into(),));
                Ok(PrecompileOutput { bytes: encoded.into(), .. })
            }
            _ => Err(PrecompileError::Other("unknown selector".into()))
        }
    }).into()
}
```

**Error encoding** — User-facing errors are returned as `PrecompileOutput::new_reverted()` with ABI-encoded revert data. Internal errors remain as `PrecompileError::Other`.

revm's `PrecompileOutput` struct has a `reverted: bool` field and a `new_reverted(gas_used, bytes)` constructor (defined in `revm-precompile/src/interface.rs`). When `reverted` is `true`, the `bytes` field carries ABI-encoded revert data — including the 4-byte error selector. This is the correct mechanism for returning custom errors that Solidity can decode.

The dispatch logic must therefore distinguish user-facing errors (returned as `Ok(PrecompileOutput::new_reverted(...))`) from internal errors (returned as `Err(PrecompileError::...)`):

```rust
/// Convert a user-facing PdPrecompileError into a reverted PrecompileOutput
/// with ABI-encoded custom error data. Returns None for internal errors.
fn try_encode_as_revert(e: &PdPrecompileError, gas_used: u64) -> Option<PrecompileOutput> {
    let revert_data: Vec<u8> = match e {
        PdPrecompileError::MissingAccessList =>
            MissingAccessList {}.abi_encode(),
        PdPrecompileError::ByteRangeNotFound { index, available } =>
            ByteRangeNotFound { index: *index, available: U256::from(*available) }.abi_encode(),
        PdPrecompileError::ChunkRangeNotFound { index, available } =>
            ChunkRangeNotFound { index: *index, available: U256::from(*available) }.abi_encode(),
        PdPrecompileError::ByteRangeOutOfBounds { start, end, available } =>
            ByteRangeOutOfBounds {
                start: U256::from(*start), end: U256::from(*end), available: U256::from(*available)
            }.abi_encode(),
        PdPrecompileError::ChunkNotFound { offset } =>
            ChunkNotFound { offset: *offset }.abi_encode(),
        PdPrecompileError::ChunkFetchFailed { offset, .. } =>
            ChunkFetchFailed { offset: *offset }.abi_encode(),
        // Internal errors: not user-facing, return None
        _ => return None,
    };
    Some(PrecompileOutput::new_reverted(gas_used, revert_data.into()))
}
```

In the dispatch, errors are handled as:
```rust
Err(e) => {
    // Try to encode as a user-facing ABI revert
    if let Some(reverted_output) = try_encode_as_revert(&e, gas_used) {
        Ok(reverted_output)
    } else {
        // Internal error — propagate as PrecompileError
        Err(PrecompileError::Other(Cow::Owned(format!("PD: {}", e))))
    }
}
```

This means user-facing errors produce a **successful** `PrecompileResult` (`Ok(...)`) with `reverted: true`, which the EVM treats as a REVERT with data. Solidity callers see the custom error selector and can decode it. Internal errors produce an `Err(PrecompileError)` which the EVM treats as an exceptional halt.

### 6.6 Rust: File-Level Changes

| File | Change |
|------|--------|
| `precompiles/pd/precompile.rs` | Replace 1-byte dispatch with 4-byte selector matching. ABI-encode return data. |
| `precompiles/pd/functions.rs` | Replace `PdFunctionId` enum with `sol!` macro block generating selectors + types. |
| `precompiles/pd/read_bytes.rs` | Remove manual `ReadBytesRangeByIndexArgs` / `ReadPartialByteRangeArgs` decoders (ABI decoding handles this). Core read logic (`read_bytes_range`) unchanged. |
| `precompiles/pd/constants.rs` | Remove raw encoding offset constants (`FUNCTION_ID_SIZE`, `INDEX_SIZE`, `INDEX_OFFSET`, etc.). Keep `PD_BASE_GAS_COST`. |
| `precompiles/pd/error.rs` | Add `try_encode_as_revert()` function mapping user-facing variants to `PrecompileOutput::new_reverted()`. Keep `PdPrecompileError` enum. |
| `precompiles/pd/context.rs` | No changes. |
| `precompiles/pd/utils.rs` | No changes. |
| `crates/irys-reth/Cargo.toml` | Add `alloy-sol-types.workspace = true` dependency (provides the `sol!` macro). Already available in workspace `Cargo.toml` but not listed for `irys-reth`. |

### 6.7 What Does NOT Change

| Component | Why unchanged |
|-----------|--------------|
| `crates/irys-reth/src/pd_tx.rs` | PD transaction envelope (header, access list building) is Layer 1 — orthogonal to precompile encoding |
| `crates/types/src/range_specifier.rs` | Access list key encoding (ChunkRangeSpecifier, ByteRangeSpecifier) is Layer 1 |
| `crates/types/src/precompile.rs` | Address constant `PD_PRECOMPILE_ADDRESS` stays the same |
| `crates/irys-reth/src/mempool.rs` | PD header detection, fee validation — all Layer 1 |
| `crates/irys-reth/src/evm.rs` | PD fee deduction, min cost validation — all Layer 1 |
| `crates/irys-reth/src/payload.rs` | PD chunk budgeting — all Layer 1 |
| Gas model | Base cost + per-operation cost structure unchanged |

## 7. Return Data Overhead Analysis

ABI encoding wraps raw bytes in `abi.encode(bytes)` format: 32-byte offset + 32-byte length + data padded to 32-byte boundary.

| Data size | Raw (current) | ABI-encoded | Overhead |
|-----------|---------------|-------------|----------|
| 100 bytes | 100 B | 192 B | +92% |
| 1 KB | 1,024 B | 1,088 B | +6% |
| 32 KB | 32,768 B | 32,832 B | +0.2% |
| 256 KB (1 chunk) | 262,144 B | 262,208 B | +0.02% |

The fixed 64-byte header (offset + length) plus up-to-31-byte padding is negligible for typical PD reads (kilobytes to megabytes). For a 100-byte read, the overhead is 92 bytes (64 header + 28 padding) — noticeable in relative terms but still under 200 bytes total. For reads above 1 KB the overhead drops below 6%.

## 8. Error Classification

### 8.1 User-Facing Errors (ABI-encoded custom errors)

These are errors a Solidity developer can act on. They are returned as ABI-encoded revert data matching the error selectors in `IIrysPD`:

| Solidity Error | Rust Source Variant | Trigger |
|----------------|-------------------|---------|
| `MissingAccessList()` | `MissingAccessList` | Transaction has no PD access list entries |
| `ByteRangeNotFound(index, available)` | `ByteRangeNotFound` | Requested byte range index exceeds count |
| `ChunkRangeNotFound(index, available)` | `ChunkRangeNotFound` | Byte range references nonexistent chunk range |
| `ByteRangeOutOfBounds(start, end, available)` | `ByteRangeOutOfBounds` | Requested slice exceeds fetched data |
| `ChunkNotFound(offset)` | `ChunkNotFound` | Chunk missing from storage at ledger offset |
| `ChunkFetchFailed(offset)` | `ChunkFetchFailed` | Storage I/O error fetching chunk |

### 8.2 Internal Errors (EVM-level failures)

These are not user-actionable and stay as opaque reverts or standard EVM errors:

| Rust Variant | Handling | Reason |
|-------------|----------|--------|
| `GasOverflow`, `OutOfGas` | `PrecompileError::OutOfGas` | Standard EVM gas error |
| `InvalidAccessList` | `PrecompileError::Other` | Malformed access list encoding — node concern |
| `OffsetOutOfRange`, `LengthOutOfRange`, `PartitionIndexTooLarge` | `PrecompileError::Other` | Arithmetic overflow — internal invariant violation |
| `InvalidCalldataLength` | Removed | Replaced by ABI decoding (alloy handles this) |
| `InsufficientInput` | Removed | Replaced by selector length check (< 4 bytes) |
| `OffsetTranslationFailed`, `InvalidLength` | `PrecompileError::Other` | Internal computation errors |

## 9. Migration

### 9.1 Solidity File Changes

| Current File | Action |
|-------------|--------|
| `fixtures/contracts/src/Precompiles.sol` | **Delete** — constants moved into `IIrysPD.sol` |
| `fixtures/contracts/src/ProgrammableData.sol` | **Delete** — replaced by `IIrysPD.sol` + `IrysPDLib.sol` |
| `fixtures/contracts/src/IrysProgrammableDataBasic.sol` | **Rewrite** — use interface/library instead of inheritance |
| `fixtures/contracts/src/IIrysPD.sol` | **New** — interface + address constants |
| `fixtures/contracts/src/IrysPDLib.sol` | **New** — convenience library |

### 9.2 Test Updates

All tests that construct precompile calldata must switch from packed encoding to ABI encoding:

```rust
// Before:
let input = Bytes::from(vec![0, 0]); // function_id=0, index=0

// After:
let input = readByteRangeCall { index: 0 }.abi_encode();
```

Tests that assert on return data must account for ABI encoding:

```rust
// Before:
let raw_bytes = result.into_output().unwrap();

// After:
let abi_encoded = result.into_output().unwrap();
let (decoded_bytes,) = readByteRangeCall::abi_decode_returns(&abi_encoded).unwrap();
```

Affected test files:
- `crates/irys-reth/src/precompiles/pd/precompile.rs` (module tests)
- `crates/irys-reth/src/precompiles/pd/read_bytes.rs` (module tests)
- `crates/irys-reth/tests/pd_context_isolation.rs`
- `crates/chain-tests/src/programmable_data/basic.rs`
- `crates/chain-tests/src/external/programmable_data_basic.rs`
- Solidity contract artifact recompilation (foundry/forge)

## 10. Cons and Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| ABI return encoding overhead for small reads | Low | 64-byte fixed overhead + up to 31-byte padding, negligible for typical chunk-sized reads (see Section 7) |
| Solidity contract recompilation required | Low | Rebuild with forge, update artifact JSON files. One-time cost. |
| All existing PD integration tests break | Medium | Expected — every test touching the precompile needs calldata/return encoding updates. Scope is bounded to the files listed in Section 9.2. |
| `alloy-sol-types` must be added to `irys-reth` | Low | Already in workspace `Cargo.toml`, just needs adding to crate-level dependencies |
