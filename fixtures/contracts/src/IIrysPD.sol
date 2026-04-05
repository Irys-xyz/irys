// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title IIrysPD — Irys Programmable Data precompile interface
/// @notice Call the precompile at IRYS_PD_ADDRESS to read on-chain chunk data.
/// @dev The precompile uses ABI-standard selectors (readData / readBytes).
///      Data specifiers are passed via the EIP-2930 access list, not calldata.
interface IIrysPD {
    /// @notice Read the full byte range described by the specifier at `index`.
    /// @param index Zero-based index into the PdDataRead access-list specifiers.
    /// @return data The requested bytes.
    function readData(uint8 index) external view returns (bytes memory data);

    /// @notice Read an arbitrary byte slice from the chunks of the specifier at `index`.
    /// @param index  Zero-based index into the PdDataRead access-list specifiers.
    /// @param offset Byte offset into the concatenated chunk data.
    /// @param length Number of bytes to read.
    /// @return data  The requested bytes.
    function readBytes(uint8 index, uint32 offset, uint32 length)
        external
        view
        returns (bytes memory data);

    // ── Custom errors ──────────────────────────────────────────────────
    error MissingAccessList();
    error SpecifierNotFound(uint8 index, uint256 available);
    error ByteRangeOutOfBounds(uint256 start, uint256 end, uint256 available);
    error ChunkNotFound(uint64 offset);
    error ChunkFetchFailed(uint64 offset);
}

/// @dev Precompile deployed at this fixed address.
address constant IRYS_PD_ADDRESS = address(0x500);

/// @dev Convenience binding so callers can write `IRYS_PD.readData(0)`.
IIrysPD constant IRYS_PD = IIrysPD(IRYS_PD_ADDRESS);
