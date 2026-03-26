// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "./IIrysPD.sol";

/// @title IrysPDLib — convenience helpers for calling the Irys PD precompile
/// @dev Wraps low-level staticcall with ABI decoding and try-variants.
library IrysPDLib {
    // ── Index-0 convenience wrappers ───────────────────────────────────

    /// @notice Read the full byte range of specifier 0 (the most common case).
    function readData() internal view returns (bytes memory data) {
        return IRYS_PD.readData(0);
    }

    /// @notice Read an arbitrary byte slice from specifier 0.
    function readBytes(
        uint32 offset,
        uint32 length
    ) internal view returns (bytes memory data) {
        return IRYS_PD.readBytes(0, offset, length);
    }

    // ── Try variants (return success bool instead of reverting) ────────

    /// @notice Try to read the full byte range of the specifier at `index`.
    /// @return success True if the call succeeded.
    /// @return data    The bytes on success, empty on failure.
    function tryReadData(
        uint8 index
    ) internal view returns (bool success, bytes memory data) {
        (success, data) = _tryCall(
            abi.encodeCall(IIrysPD.readData, (index))
        );
        // ABI-encoded `bytes` requires ≥64 bytes (32 offset + 32 length).
        // Guard against short returndata to avoid reverting in abi.decode.
        if (!success || data.length < 64) {
            return (false, bytes(""));
        }
        data = abi.decode(data, (bytes));
    }

    /// @notice Try to read an arbitrary byte slice from the specifier at `index`.
    /// @return success True if the call succeeded.
    /// @return data    The bytes on success, empty on failure.
    function tryReadBytes(
        uint8 index,
        uint32 offset,
        uint32 length
    ) internal view returns (bool success, bytes memory data) {
        (success, data) = _tryCall(
            abi.encodeCall(IIrysPD.readBytes, (index, offset, length))
        );
        if (!success || data.length < 64) {
            return (false, bytes(""));
        }
        data = abi.decode(data, (bytes));
    }

    // ── Internal ───────────────────────────────────────────────────────

    function _tryCall(
        bytes memory callData
    ) private view returns (bool success, bytes memory result) {
        (success, result) = IRYS_PD_ADDRESS.staticcall(callData);
    }
}
