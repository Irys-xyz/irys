// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "./IIrysPD.sol";

/// @title IrysPDLib — convenience helpers for calling the Irys PD precompile
/// @dev Wraps low-level staticcall with ABI decoding and try-variants.
library IrysPDLib {
    error MalformedPdReturnData();

    // ── Index-0 convenience wrappers ───────────────────────────────────

    /// @notice Read the full byte range of specifier 0 (the most common case).
    function readData() internal view returns (bytes memory data) {
        return _callAndExtractBytes(abi.encodeCall(IIrysPD.readData, (0)));
    }

    /// @notice Read an arbitrary byte slice from specifier 0.
    function readBytes(
        uint32 offset,
        uint32 length
    ) internal view returns (bytes memory data) {
        return _callAndExtractBytes(
            abi.encodeCall(IIrysPD.readBytes, (0, offset, length))
        );
    }

    // ── Try variants (return success bool instead of reverting) ────────

    /// @notice Try to read the full byte range of the specifier at `index`.
    /// @return success True if the call succeeded.
    /// @return data    The bytes on success, empty on failure.
    function tryReadData(
        uint8 index
    ) internal view returns (bool success, bytes memory data) {
        return _tryCallAndDecode(abi.encodeCall(IIrysPD.readData, (index)));
    }

    /// @notice Try to read an arbitrary byte slice from the specifier at `index`.
    /// @return success True if the call succeeded.
    /// @return data    The bytes on success, empty on failure.
    function tryReadBytes(
        uint8 index,
        uint32 offset,
        uint32 length
    ) internal view returns (bool success, bytes memory data) {
        return _tryCallAndDecode(
            abi.encodeCall(IIrysPD.readBytes, (index, offset, length))
        );
    }

    // ── Internal ───────────────────────────────────────────────────────

    /// @dev Staticcall the PD precompile, validate the ABI envelope, and decode.
    /// Returns (false, "") on call failure or malformed returndata.
    function _tryCallAndDecode(
        bytes memory payload
    ) private view returns (bool success, bytes memory data) {
        (success, data) = _tryCall(payload);
        if (!success || !_isValidAbiBytes(data)) {
            return (false, bytes(""));
        }
        data = abi.decode(data, (bytes));
    }

    /// @dev Validate that `data` is a well-formed ABI-encoded `bytes memory`.
    /// Checks: (1) at least 64 bytes, (2) dynamic offset == 0x20,
    /// (3) declared length fits within the remaining returndata.
    /// Returns false instead of reverting on malformed payloads.
    function _isValidAbiBytes(bytes memory data) private pure returns (bool) {
        if (data.length < 64) return false;
        uint256 dynOffset;
        uint256 bytesLen;
        assembly {
            dynOffset := mload(add(data, 32))
            bytesLen := mload(add(data, 64))
        }
        if (dynOffset != 0x20) return false;
        // 64 bytes for offset + length words, then the declared payload.
        // Use subtraction form to avoid uint256 overflow when bytesLen is large.
        if (bytesLen > data.length - 64) return false;
        return true;
    }

    function _tryCall(
        bytes memory callData
    ) private view returns (bool success, bytes memory result) {
        (success, result) = IRYS_PD_ADDRESS.staticcall(callData);
    }

    /// @dev Call the PD precompile and reinterpret its ABI `bytes` return value in place.
    /// This avoids the compiler-generated second large `MCOPY` that `abi.decode(data, (bytes))`
    /// would perform when the returndata itself is already a large `bytes` payload.
    function _callAndExtractBytes(
        bytes memory payload
    ) private view returns (bytes memory data) {
        (bool success, bytes memory result) = IRYS_PD_ADDRESS.staticcall(payload);

        if (!success) {
            assembly {
                revert(add(result, 32), mload(result))
            }
        }

        if (!_isValidAbiBytes(result)) {
            revert MalformedPdReturnData();
        }

        assembly {
            // `result` is a bytes array containing the raw returndata:
            // [returndata_len][abi_offset=0x20][bytes_len][bytes_data...]
            // Re-point to the embedded `[bytes_len][bytes_data...]` region so
            // callers see a standard `bytes memory` without copying the payload again.
            data := add(result, 0x40)
        }
    }
}
