# Design Spec: PD Precompile ABI Redesign

**Date**: 2026-03-17
**Branch**: feat/pd
**Status**: Draft
**Breaking**: Yes — changes precompile calldata/return encoding

## 1. What This Enables

This redesign brings the PD precompile in line with the industry standard set by Arbitrum, Moonbeam, Avalanche, and Optimism. Concretely, it enables:

**For Solidity developers building on Irys:**

- **Natural call syntax** — `IRYS_PD.readData(0)` or `IRYS_PD.readBytes(0, offset, length)` instead of `address(0x500).staticcall(bytes.concat(bytes1(0), bytes1(0)))`. Contracts read like business logic, not byte manipulation.
- **Typed custom errors** — When a read fails, callers get structured errors like `SpecifierNotFound(index, available)` instead of an opaque `success = false`. This enables precise error handling in Solidity (`try/catch` with specific error types) and clear diagnostics during development.
- **Composability without inheritance** — Contracts use `IRYS_PD.readData(0)` or `IrysPDLib.readData()` directly. No forced `is ProgrammableData` inheritance, no ABI pollution, no diamond-problem risks when combining with other base contracts.
- **Both strict and graceful error patterns** — Direct interface calls revert with custom errors (the common case). Library `try*` variants return `(bool, bytes)` for contracts that need to handle failure without reverting.

**For off-chain tooling and integrators:**

- **Standard ABI works everywhere** — Etherscan can display and verify the precompile's interface. Block explorers decode PD calls and errors automatically. No custom decoding plugins needed.
- **SDK error decoding** — ethers.js, viem, and alloy can parse PD revert data into typed error objects using the interface ABI, enabling clear error messages in dapp UIs and debugging tools.
- **`abi.encodeCall` support** — Transaction builders and testing frameworks can use Solidity's type-safe encoding (`abi.encodeCall(IIrysPD.readData, (0))`) instead of manual byte packing.

**For protocol evolution:**

- **Extensibility via interface versioning** — New read operations are added as new functions on the interface (e.g., `readMultipleByteRanges`). Existing function selectors and their behavior never change. This follows the Arbitrum pattern of additive-only interface evolution.
- **Standard toolchain integration** — Foundry/Hardhat test helpers, fuzzing tools, and static analyzers understand standard ABI calls out of the box. No special handling needed for PD reads.

**What does NOT change:** The PD transaction envelope (on-chain `irys-pd-meta` header, fee model) is unaffected. The fee deduction, payload builder chunk budgeting, and block validation logic retain their existing structure.

## 2. Problem Statement

The Programmable Data (PD) precompile at address `0x500` uses a custom 1-byte function ID dispatch format. Downstream Solidity developers must use manual `bytes.concat()` + `staticcall` through an inherited base contract. This is non-standard, limits tooling support, and provides poor error feedback.

The code is not live. We can make breaking changes to ship a best-in-class API before launch.

## 3. Background: How PD Works Today

### 3.1 Architecture Overview

PD enables smart contracts to read large data stored in Irys partitions. The data flow has three layers:

```
Layer 1: PD Transaction Envelope (on-chain, in EVM tx calldata)
┌─────────────────────────────────────────────────────┐
│ [irys-pd-meta magic:12][version:u16][PdHeaderV1:64] │  ← Parsed by Irys node
│ + EIP-2930 access list with packed B256 keys        │    before EVM execution
└─────────────────────────────────────────────────────┘

Layer 2: Access List Specifiers (chunk provisioning)
┌─────────────────────────────────────────────────────┐
│ B256 storage keys under precompile address (0x500)  │  ← CHANGES: unified specifier
│ Node reads these to prefetch chunks                  │
└─────────────────────────────────────────────────────┘

Layer 3: Precompile Call (inside EVM, contract → 0x500)
┌─────────────────────────────────────────────────────┐
│ contract.staticcall(0x500, encoded_params)           │  ← CHANGES: ABI selectors
│ Byte-level reads specified in calldata               │
└─────────────────────────────────────────────────────┘
```

**This design changes Layers 2 and 3.** Layer 1 (PD transaction envelope, PD header format, fee model) is unaffected.

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
ReadFullByteRange:    [0x01:1][index:1]           = 2 bytes
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

1. **Clean layer separation** — Access list specifiers handle chunk provisioning (Irys layer). Byte-level reads are specified in precompile calldata (Solidity layer). Each layer does one thing.
2. **Unified access list specifier** — One specifier type replaces the current ChunkRangeSpecifier + ByteRangeSpecifier pair. No cross-references, no ordering dependencies.
3. **Standard ABI dispatch** — 4-byte selectors so the precompile can be called through a Solidity `interface`.
4. **Interface + Library layering** — interface is the protocol, library is ergonomics. No inheritance required.
5. **Strict by default, try-variants opt-in** — direct calls revert with custom errors; library provides `try*` functions.
6. **Right-sized fields** — partition_index matches runtime capability (u64, not U200). Length matches practical limits (u32, not U34).

### 6.2 Access List Specifier: `PdDataRead`

A single specifier replaces the current ChunkRangeSpecifier + ByteRangeSpecifier pair. Since the code is not live, there is no backward compatibility concern — the old types are simply replaced.

**Binary layout:**

```
┌──────┬───────────────────────┬──────────────┬────────────┬──────────────┬─────────────┐
│ 0x01 │ partition_index (u64) │  start (u32) │  len (u32) │ byte_off(u24)│ reserved(0) │
│  1B  │       8B (BE)         │   4B (BE)    │  4B (BE)   │   3B (BE)    │    12B      │
└──────┴───────────────────────┴──────────────┴────────────┴──────────────┴─────────────┘
  byte:  0         1-8                9-12          13-16         17-19         20-31
```

Fields ordered by requirement: required fields first (`type`, `partition_index`, `start`, `len`), optional field last (`byte_off` defaults to 0), reserved bytes at end (must be zero, validated).

**Field definitions:**

| Field | Type | Bytes | Max value | Purpose |
|---|---|---|---|---|
| type | u8 | 1 | — | `0x01` — PD data read specifier. Nonzero so that `B256::ZERO` always fails validation. Other values reserved for future specifier types. |
| partition_index | u64 | 8 (BE) | 2^64 - 1 | Partition in the publish ledger. Matches runtime u64 capability. |
| start | u32 | 4 (BE) | 4,294,967,295 | First chunk offset within partition. Covers 51.8M chunks/partition with 82× headroom. |
| len | u32 | 4 (BE) | 4,294,967,295 | Number of bytes to read. Required — node derives chunk count from this. Max ~4 GB, 2× block max. |
| byte_off | u24 | 3 (BE) | 16,777,215 | Byte offset within the first chunk. 0 = start of chunk. Forward-compatible with chunks up to 16 MB. |
| reserved | — | 12 | 0 | Must be zero. Validated on decode — nonzero values are rejected. |

**How the node derives chunk count for prefetching:**

```
chunks_needed = ceil((byte_off + len) / chunk_size)
prefetch range: [start, start + chunks_needed)
```

If `len = 0`, the specifier is invalid and rejected.

**Addressing capacity:**

Any byte at position P in a ~12.37 TB partition (51,872,000 chunks × 256 KB):
- `start = P / chunk_size` → u32, max 4.29B, partition needs 51.8M. Sufficient.
- `byte_off = P % chunk_size` → u24, max 16.7M, chunk is 262,144. Sufficient.
- `len` → u32, max ~4 GB, block max ~1.83 GB. Sufficient.

**Validity rules (enforced on decode, consensus-critical):**

All rules are checked by a single canonical `PdDataRead::decode()` function used everywhere — mempool admission, payload building, block validation, chunk provisioning, and precompile execution. Malformed keys are rejected (not skipped).

| Rule | Rationale |
|---|---|
| `type == 0x01` | Type discriminant. `B256::ZERO` (all-zero key) always fails because type byte is 0, not 1. |
| `len > 0` | Zero-length reads are meaningless. Node cannot derive chunk count. |
| `byte_off < chunk_size` | Ensures canonical encoding — each logical position has exactly one representation. Without this, `(start=5, byte_off=262144)` and `(start=6, byte_off=0)` encode the same location. |
| `start + chunks_needed <= num_chunks_in_partition` | Prevents partition boundary overflow. |
| `reserved == [0u8; 12]` | Reserved bytes must be zero. Nonzero values are rejected to preserve canonicality and forward compatibility. |
| All arithmetic is checked | `byte_off + len`, `start + chunks_needed`, and ledger offset calculations use checked ops. Overflow = reject. |

**Encoding (big-endian throughout):**

All fields use big-endian (network byte order), matching EVM convention and eliminating the endianness mismatch between the current little-endian access list keys and big-endian precompile calldata.

**Access list indexing rule:**

The `index` parameter in precompile calls (`readData(index)`, `readBytes(index, ...)`) refers to the Nth `PdDataRead` storage key within the `AccessListItem` whose `address == PD_PRECOMPILE_ADDRESS`. If multiple `AccessListItem` entries target `0x500`, their storage keys are concatenated in encounter order. Non-PD keys (those failing `PdDataRead::decode()`) are rejected — the transaction is invalid if any key under the PD precompile address fails validation.

### 6.3 Solidity: `IIrysPD.sol` — The Interface

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title IIrysPD — Irys Programmable Data Precompile Interface
/// @notice Read data from Irys partition storage via EIP-2930 access lists.
/// @dev Precompile at address 0x500. Chunk ranges are declared in the transaction's
///      access list as PdDataRead specifiers. Byte-level reads are specified via
///      function parameters at execution time.
interface IIrysPD {
    /// @notice Read bytes as specified by a PdDataRead access list entry
    /// @dev Uses the specifier's byte_off and len fields to determine the byte range.
    ///      Returns bytes [byte_off, byte_off + len) from the specifier's chunk range.
    /// @param index Index of the PdDataRead entry in the PD-bound access list storage keys (0-255)
    /// @return data The raw bytes from the specified range
    function readData(uint8 index) external view returns (bytes memory data);

    /// @notice Read a byte slice from a PdDataRead entry's chunk range
    /// @dev Ignores the specifier's byte_off and len — uses the provided offset and length
    ///      instead. The offset is relative to byte 0 of the first chunk in the specifier's
    ///      chunk range. Bounded by the total prefetched chunk data (chunks_needed × chunk_size).
    ///      Use this when byte ranges need to be determined dynamically at execution time.
    /// @param index Index of the PdDataRead entry in the PD-bound access list storage keys (0-255)
    /// @param offset Starting byte offset within the concatenated chunk data (0 = first byte of first chunk)
    /// @param length Number of bytes to read. Must be > 0.
    /// @return data The requested byte slice
    function readBytes(uint8 index, uint32 offset, uint32 length)
        external view returns (bytes memory data);

    /// @notice Transaction has no PdDataRead entries in its access list
    error MissingAccessList();

    /// @notice Requested specifier index exceeds available PdDataRead entries
    /// @param index The requested index
    /// @param available Number of PdDataRead entries found in the access list
    error SpecifierNotFound(uint8 index, uint256 available);

    /// @notice Requested byte slice exceeds the available chunk data
    /// @param start Start offset of the requested range
    /// @param end End offset of the requested range (exclusive)
    /// @param available Total bytes available (chunks_needed × chunk_size)
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

/// @dev Typed constant for direct interface calls: IRYS_PD.readData(0)
IIrysPD constant IRYS_PD = IIrysPD(IRYS_PD_ADDRESS);
```

**Replaces**: `Precompiles.sol` and `ProgrammableData.sol`.

### 6.4 Solidity: `IrysPDLib.sol` — The Convenience Library

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {IIrysPD, IRYS_PD, IRYS_PD_ADDRESS} from "./IIrysPD.sol";

/// @title IrysPDLib — Convenience library for Irys Programmable Data reads
/// @notice Provides shorthand functions and try-variants for PD precompile access
library IrysPDLib {
    /// @notice Read all data from the first access list entry (index 0)
    /// @dev Reverts with IIrysPD custom errors on failure
    function readData() internal view returns (bytes memory) {
        return IRYS_PD.readData(0);
    }

    /// @notice Read a byte slice from the first access list entry (index 0)
    /// @dev Reverts with IIrysPD custom errors on failure
    function readBytes(uint32 offset, uint32 length) internal view returns (bytes memory) {
        return IRYS_PD.readBytes(0, offset, length);
    }

    /// @notice Try to read data, returning success flag instead of reverting
    /// @param index Index of the PdDataRead entry in the access list
    /// @return success Whether the read succeeded
    /// @return data The raw bytes on success; empty on failure
    function tryReadData(uint8 index)
        internal view returns (bool success, bytes memory data)
    {
        bytes memory returndata;
        (success, returndata) = IRYS_PD_ADDRESS.staticcall(
            abi.encodeCall(IIrysPD.readData, (index))
        );
        if (success) {
            data = abi.decode(returndata, (bytes));
        }
    }

    /// @notice Try to read a byte slice, returning success flag instead of reverting
    /// @param index Index of the PdDataRead entry in the access list
    /// @param offset Starting byte offset within the concatenated chunk data
    /// @param length Number of bytes to read
    /// @return success Whether the read succeeded
    /// @return data The raw bytes on success; empty on failure
    function tryReadBytes(uint8 index, uint32 offset, uint32 length)
        internal view returns (bool success, bytes memory data)
    {
        bytes memory returndata;
        (success, returndata) = IRYS_PD_ADDRESS.staticcall(
            abi.encodeCall(IIrysPD.readBytes, (index, offset, length))
        );
        if (success) {
            data = abi.decode(returndata, (bytes));
        }
    }
}
```

### 6.5 Solidity: Updated Example Consumer

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {IRYS_PD} from "./IIrysPD.sol";
import {IrysPDLib} from "./IrysPDLib.sol";

contract ProgrammableDataBasic {
    bytes public storedData;

    /// @notice Read PD data as specified in the access list (reverts on error)
    function readPdChunkIntoStorage() public {
        storedData = IRYS_PD.readData(0);
    }

    /// @notice Read a specific byte slice at execution time
    function readSlice(uint32 offset, uint32 length) public {
        storedData = IRYS_PD.readBytes(0, offset, length);
    }

    /// @notice Read PD data using library convenience
    function readPdChunkIntoStorageAlt() public {
        storedData = IrysPDLib.readData();
    }

    /// @notice Read PD data with graceful error handling
    function tryReadPdChunk() public returns (bool) {
        (bool success, bytes memory data) = IrysPDLib.tryReadData(0);
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

### 6.6 Rust: Precompile Dispatch Redesign

**Use `alloy::sol!` to generate selectors, decoders, and error encoders:**

```rust
use alloy_sol_types::sol;

sol! {
    function readData(uint8 index) external view returns (bytes memory data);
    function readBytes(uint8 index, uint32 offset, uint32 length)
        external view returns (bytes memory data);

    error MissingAccessList();
    error SpecifierNotFound(uint8 index, uint256 available);
    error ByteRangeOutOfBounds(uint256 start, uint256 end, uint256 available);
    error ChunkNotFound(uint64 offset);
    error ChunkFetchFailed(uint64 offset);
}
```

This generates at compile time:
- `readDataCall::SELECTOR` / `readBytesCall::SELECTOR` — 4-byte selectors
- `readDataCall::abi_decode(data)` — parameter decoder
- `readDataCall::abi_encode_returns(&(result,))` — return encoder
- `MissingAccessList {}` — error struct with `abi_encode()` for revert data

**New dispatch logic** (replaces `PdFunctionId` enum):

```rust
fn pd_precompile(pd_context: PdContext) -> DynPrecompile {
    (move |input: PrecompileInput<'_>| -> PrecompileResult {
        let data = input.data;
        if data.len() < 4 {
            return Err(PrecompileError::Other("calldata too short".into()));
        }

        // ... gas check ...

        let access_list = pd_context.read_access_list();
        let specifiers = parse_pd_specifiers(&access_list)?;

        let selector: [u8; 4] = data[..4].try_into().unwrap();
        match selector {
            readDataCall::SELECTOR => {
                let decoded = readDataCall::abi_decode(&data[4..])?;
                let spec = specifiers.get(decoded.index as usize)?;
                // Use byte_off and len from the access list specifier
                let result = read_chunk_data(spec, spec.byte_off, spec.len, &pd_context)?;
                let encoded = readDataCall::abi_encode_returns(&(result.into(),));
                Ok(PrecompileOutput::new(gas_used, encoded.into()))
            }
            readBytesCall::SELECTOR => {
                let decoded = readBytesCall::abi_decode(&data[4..])?;
                let spec = specifiers.get(decoded.index as usize)?;
                // Override byte range from calldata — chunk range still from specifier
                let result = read_chunk_data(spec, decoded.offset, decoded.length, &pd_context)?;
                let encoded = readBytesCall::abi_encode_returns(&(result.into(),));
                Ok(PrecompileOutput::new(gas_used, encoded.into()))
            }
            _ => Err(PrecompileError::Other("unknown selector".into()))
        }
    }).into()
}
```

**`readData(index)`** reads the byte range exactly as declared in the access list entry — uses the specifier's `byte_off` and `len` to return `bytes[byte_off .. byte_off + len]` from the chunk data.

**`readBytes(index, offset, length)`** ignores the specifier's `byte_off` and `len` entirely. It uses the access list entry only to identify which chunks were prefetched. The `offset` and `length` from calldata are applied against the full concatenated chunk data (`chunks_needed × chunk_size` bytes). This enables dynamic byte-level reads at execution time — the contract can decide what slice to read based on runtime logic. Bounded by the total prefetched chunk data; requests outside it revert with `ByteRangeOutOfBounds`.

**Error encoding** — User-facing errors are returned as `PrecompileOutput::new_reverted()` with ABI-encoded revert data. Internal errors remain as `PrecompileError::Other`.

revm's `PrecompileOutput` struct has a `reverted: bool` field and a `new_reverted(gas_used, bytes)` constructor (defined in `revm-precompile/src/interface.rs`). When `reverted` is `true`, the `bytes` field carries ABI-encoded revert data — including the 4-byte error selector. This is the correct mechanism for returning custom errors that Solidity can decode.

```rust
/// Convert a user-facing PdPrecompileError into a reverted PrecompileOutput
/// with ABI-encoded custom error data. Returns None for internal errors.
fn try_encode_as_revert(e: &PdPrecompileError, gas_used: u64) -> Option<PrecompileOutput> {
    let revert_data: Vec<u8> = match e {
        PdPrecompileError::MissingAccessList =>
            MissingAccessList {}.abi_encode(),
        PdPrecompileError::SpecifierNotFound { index, available } =>
            SpecifierNotFound { index: *index, available: U256::from(*available) }.abi_encode(),
        PdPrecompileError::ByteRangeOutOfBounds { start, end, available } =>
            ByteRangeOutOfBounds {
                start: U256::from(*start), end: U256::from(*end), available: U256::from(*available)
            }.abi_encode(),
        PdPrecompileError::ChunkNotFound { offset } =>
            ChunkNotFound { offset: *offset }.abi_encode(),
        PdPrecompileError::ChunkFetchFailed { offset, .. } =>
            ChunkFetchFailed { offset: *offset }.abi_encode(),
        _ => return None,
    };
    Some(PrecompileOutput::new_reverted(gas_used, revert_data.into()))
}
```

### 6.7 Rust: Access List Parsing Changes

`parse_pd_specifiers()` replaces `parse_access_list()`. It is the **single canonical parser** used by all system components — mempool admission, payload building, block validation, chunk provisioning, and precompile execution. This eliminates the current inconsistency where some paths skip invalid keys while others reject.

It produces a single `Vec<PdDataRead>` instead of separate `chunk_reads[]` and `byte_reads[]`:

```rust
pub struct PdDataRead {
    pub partition_index: u64,
    pub start: u32,
    pub len: u32,
    pub byte_off: u32,  // u24 stored as u32, top byte always 0
}

impl PdDataRead {
    /// Derive the number of chunks the node must prefetch.
    pub fn chunks_needed(&self, chunk_size: u64) -> u64 {
        let total_bytes = self.byte_off as u64 + self.len as u64;
        total_bytes.div_ceil(chunk_size)
    }

    /// Decode and validate from a 32-byte access list storage key.
    /// This is the single canonical decoder — all system components use this function.
    pub fn decode(bytes: &[u8; 32], chunk_config: &ChunkConfig) -> eyre::Result<Self> {
        // Type byte
        eyre::ensure!(bytes[0] == 0x01, "expected PdDataRead type byte 0x01, got {:#04x}", bytes[0]);

        // Reserved bytes
        eyre::ensure!(bytes[20..32] == [0u8; 12], "reserved bytes must be zero");

        let partition_index = u64::from_be_bytes(bytes[1..9].try_into()?);
        let start = u32::from_be_bytes(bytes[9..13].try_into()?);
        let len = u32::from_be_bytes(bytes[13..17].try_into()?);
        let byte_off = u32::from_be_bytes([0, bytes[17], bytes[18], bytes[19]]);

        // Validity rules (consensus-critical)
        eyre::ensure!(len > 0, "len must be > 0");
        eyre::ensure!(
            (byte_off as u64) < chunk_config.chunk_size,
            "byte_off ({}) must be < chunk_size ({})", byte_off, chunk_config.chunk_size
        );

        let spec = Self { partition_index, start, len, byte_off };

        let chunks_needed = spec.chunks_needed(chunk_config.chunk_size);
        eyre::ensure!(
            (start as u64).checked_add(chunks_needed)
                .map_or(false, |end| end <= chunk_config.num_chunks_in_partition),
            "start + chunks_needed exceeds partition boundary"
        );

        Ok(spec)
    }

    /// Encode to a 32-byte access list storage key.
    pub fn encode(&self) -> [u8; 32] {
        let mut buf = [0u8; 32];
        buf[0] = 0x01;
        buf[1..9].copy_from_slice(&self.partition_index.to_be_bytes());
        buf[9..13].copy_from_slice(&self.start.to_be_bytes());
        buf[13..17].copy_from_slice(&self.len.to_be_bytes());
        let bo = self.byte_off.to_be_bytes();
        buf[17..20].copy_from_slice(&bo[1..4]); // u24: skip top byte
        // bytes 20..32 remain zero (reserved)
        buf
    }
}
```

### 6.8 Rust: File-Level Changes

| File | Change |
|------|--------|
| `crates/types/src/range_specifier.rs` | Replace `ChunkRangeSpecifier`, `ByteRangeSpecifier`, `PdAccessListArg` with `PdDataRead`. |
| `precompiles/pd/precompile.rs` | Replace 1-byte dispatch with 4-byte ABI selector matching. ABI-encode return data. |
| `precompiles/pd/functions.rs` | Replace `PdFunctionId` enum with `sol!` macro block generating selectors + types. |
| `precompiles/pd/read_bytes.rs` | Rewrite to work with `PdDataRead`. Remove `ReadBytesRangeByIndexArgs` / `ReadPartialByteRangeArgs`. Remove cross-reference logic. Core chunk-fetch logic reused. |
| `precompiles/pd/constants.rs` | Remove raw encoding offset constants. Keep `PD_BASE_GAS_COST`. |
| `precompiles/pd/error.rs` | Update error variants: remove `ByteRangeNotFound`/`ChunkRangeNotFound`, add `SpecifierNotFound`. Add `try_encode_as_revert()`. |
| `precompiles/pd/utils.rs` | Replace `parse_access_list()` → `parse_pd_specifiers()`. Single vector output. |
| `precompiles/pd/context.rs` | No changes. |
| `crates/irys-reth/src/pd_tx.rs` | Update `build_pd_access_list()` and `sum_pd_chunks_in_access_list()` to use `PdDataRead`. |
| `crates/irys-reth/src/mempool.rs` | Update `extract_pd_chunk_specs()` to decode `PdDataRead` and derive chunk counts. |
| `crates/irys-reth/src/payload.rs` | Update chunk budget to use `PdDataRead::chunks_needed()`. |
| `crates/irys-reth/Cargo.toml` | Add `alloy-sol-types.workspace = true`. |

### 6.9 What Does NOT Change

| Component | Why unchanged |
|-----------|--------------|
| `crates/irys-reth/src/pd_tx.rs` (PD header) | `PdHeaderV1`, `prepend_pd_header_v1_to_calldata`, `detect_and_decode_pd_header` — fee envelope is orthogonal |
| `crates/types/src/precompile.rs` | Address constant `PD_PRECOMPILE_ADDRESS` stays the same |
| `crates/irys-reth/src/evm.rs` | PD fee deduction, min cost validation — reads chunk count from access list (updated to use `PdDataRead::chunks_needed()` but logic unchanged) |
| `crates/actors/src/pd_service.rs` | Chunk provisioning — consumes chunk specs, updated to accept `PdDataRead` but prefetch logic unchanged |
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

Returned as ABI-encoded revert data via `PrecompileOutput::new_reverted()`:

| Solidity Error | Trigger |
|----------------|---------|
| `MissingAccessList()` | Transaction has no PD access list entries |
| `SpecifierNotFound(index, available)` | Requested specifier index exceeds count |
| `ByteRangeOutOfBounds(start, end, available)` | Requested slice exceeds available chunk data |
| `ChunkNotFound(offset)` | Chunk missing from storage at ledger offset |
| `ChunkFetchFailed(offset)` | Storage I/O error fetching chunk |

### 8.2 Internal Errors (EVM-level failures)

Not user-actionable — stay as opaque reverts or standard EVM errors:

| Condition | Handling |
|-----------|----------|
| Gas exceeded | `PrecompileError::OutOfGas` |
| Malformed access list encoding | `PrecompileError::Other` |
| Arithmetic overflow (offset, partition index) | `PrecompileError::Other` |
| ABI decode failure (malformed calldata) | `PrecompileError::Other` |

## 9. Affected Files

### 9.1 Solidity

| File | Action |
|-------------|--------|
| `fixtures/contracts/src/Precompiles.sol` | **Delete** — constants moved into `IIrysPD.sol` |
| `fixtures/contracts/src/ProgrammableData.sol` | **Delete** — replaced by `IIrysPD.sol` + `IrysPDLib.sol` |
| `fixtures/contracts/src/IrysProgrammableDataBasic.sol` | **Rewrite** — use interface/library instead of inheritance |
| `fixtures/contracts/src/IIrysPD.sol` | **New** — interface + address constants |
| `fixtures/contracts/src/IrysPDLib.sol` | **New** — convenience library |

### 9.2 Rust — Access List & Specifiers

| File | Change |
|------|--------|
| `crates/types/src/range_specifier.rs` | Replace `ChunkRangeSpecifier`, `ByteRangeSpecifier`, `PdAccessListArg` with `PdDataRead`. Remove `U18`, `U34`, `PdAccessListArgSerde` trait. |
| `crates/irys-reth/src/pd_tx.rs` | Update `build_pd_access_list()` and `sum_pd_chunks_in_access_list()` to use `PdDataRead`. |
| `crates/irys-reth/src/mempool.rs` | Update chunk extraction to decode `PdDataRead` and derive chunk counts. |
| `crates/irys-reth/src/payload.rs` | Update chunk budget to use `PdDataRead::chunks_needed()`. |
| `crates/actors/src/block_validation.rs` | Update block PD chunk validation. |
| `crates/irys-reth/src/evm.rs` | Update PD fee calculation chunk counting. |

### 9.3 Rust — Precompile

| File | Change |
|------|--------|
| `precompiles/pd/precompile.rs` | Replace 1-byte dispatch with 4-byte ABI selector matching. ABI-encode return data. |
| `precompiles/pd/functions.rs` | Replace `PdFunctionId` enum with `sol!` macro block. |
| `precompiles/pd/read_bytes.rs` | Rewrite to work with `PdDataRead`. Remove cross-reference logic. Core chunk-fetch reused. |
| `precompiles/pd/constants.rs` | Remove raw encoding offset constants. Keep `PD_BASE_GAS_COST`. |
| `precompiles/pd/error.rs` | Simplify error variants. Add `try_encode_as_revert()`. |
| `precompiles/pd/utils.rs` | Replace `parse_access_list()` with `parse_pd_specifiers()`. Single canonical parser shared by all system components. Rejects (not skips) malformed keys. |
| `precompiles/pd/context.rs` | No changes. |
| `crates/irys-reth/Cargo.toml` | Add `alloy-sol-types.workspace = true`. |

### 9.4 Tests

All tests that construct access lists or precompile calldata need updating:
- `crates/irys-reth/src/precompiles/pd/precompile.rs` (module tests)
- `crates/irys-reth/src/precompiles/pd/read_bytes.rs` (module tests)
- `crates/irys-reth/tests/pd_context_isolation.rs`
- `crates/chain-tests/src/programmable_data/basic.rs`
- `crates/chain-tests/src/programmable_data/pd_chunk_limit.rs`
- `crates/chain-tests/src/programmable_data/hardfork_guard.rs`
- `crates/chain-tests/src/programmable_data/min_transaction_cost.rs`
- `crates/chain-tests/src/external/programmable_data_basic.rs`
- `crates/chain-tests/src/utils.rs`
- Solidity contract artifact recompilation (foundry/forge)

## 10. Risks

| Risk | Severity | Mitigation |
|------|----------|------------|
| ABI return encoding overhead for small reads | Low | 64-byte fixed overhead, negligible for typical chunk-sized reads (see Section 7) |
| Broad scope — access list + precompile ABI change together | Medium | One clean break instead of two separate migrations. All changes are pre-launch. |
| `alloy-sol-types` must be added to `irys-reth` | Low | Already in workspace `Cargo.toml`, just needs crate-level dependency. |
| Chunk derivation formula couples to `chunk_size` | Low | `chunk_size = 262,144` is a protocol constant. If it ever changes, specifier format gets a new type byte. |
| `readBytes(index, offset, length)` can request bytes outside the specifier's provisioned range | Low | Precompile validates bounds. Requests outside available data revert with `ByteRangeOutOfBounds`. |
