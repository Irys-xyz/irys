// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "./Precompiles.sol";

// public struct BytesRangeSpecifier {
//     uint8 index;
//     uint16 chunk_offset;
//     // clamped to uint18
//     uint24 byte_offset;
//     // clamped to uint34
//     uint40 length;
// }

contract ProgrammableData {
    /**
     * @notice Reads all bytes from the first BytesRange in the access list
     * @dev Convenience method that calls readByteRange(0) to read the entire first range
     * @return success Boolean indicating whether the read operation was successful
     * @return data The complete bytes from the first BytesRange. If success is false, this will be empty
     */
    function readBytes() public view returns (bool success, bytes memory data) {
        return readByteRange(0);
    }

    /**
     * @notice Reads a specified number of bytes from the first BytesRange in the access list
     * @dev Convenience method that calls readByteRange(0, offset, length)
     * @param relative_offset The starting position within the BytesRange to begin reading, in bytes
     * @param length The number of bytes to read from the BytesRange
     * @return success Boolean indicating whether the read operation was successful
     * @return result The bytes that were read from the BytesRange. If success is false, this will be empty
     */
    function readBytes(
        uint32 relative_offset,
        uint32 length
    ) public view returns (bool success, bytes memory result) {
        return readByteRange(0, relative_offset, length);
    }

    /**
     * @notice Reads all bytes from a specified (by index) BytesRange in the access list
     * @dev Note that "by index" means the index in the list of ByteRanges, from the access list with all other types filtered out.
     * @param byte_range_index The index of the BytesRange in the access list to read from (0-255)
     * @return success Boolean indicating whether the read operation was successful
     * @return data The complete bytes from the specified BytesRange. If success is false, this will be empty
     * @custom:precompile Calls PD_READ_PRECOMPILE_ADDRESS with READ_FULL_BYTE_RANGE operation
     */
    function readByteRange(
        uint8 byte_range_index
    ) public view returns (bool success, bytes memory data) {
        return
            // encodePacked is deprecated
            address(PD_READ_PRECOMPILE_ADDRESS).staticcall(
                bytes.concat(
                    bytes1(READ_FULL_BYTE_RANGE),
                    bytes1(byte_range_index)
                )
            );
    }

    /**
     * @notice Reads a specified number of bytes from a specified (by index) BytesRange in the access list
     * @dev Note that "by index" means the index in the list of ByteRanges, from the access list with all other types filtered out.
     * @param byte_range_index The index of the BytesRange in the access list to read from (0-255)
     * @param start_offset The starting position within the BytesRange to begin reading, in bytes
     * @param length The number of bytes to read from the BytesRange
     * @return success Boolean indicating whether the read operation was successful
     * @return data The bytes that were read from the BytesRange. If success is false, this will be empty
     * @custom:precompile Calls PD_READ_PRECOMPILE_ADDRESS with READ_PARTIAL_BYTE_RANGE operation
     */
    function readByteRange(
        uint8 byte_range_index,
        uint32 start_offset,
        uint32 length
    ) public view returns (bool success, bytes memory data) {
        return
            address(PD_READ_PRECOMPILE_ADDRESS).staticcall(
                bytes.concat(
                    bytes1(READ_PARTIAL_BYTE_RANGE),
                    bytes1(byte_range_index),
                    bytes4(start_offset),
                    bytes4(length)
                )
            );
    }

    // function readBytesFromChunkRange(
    //     uint8 chunk_range_index,
    //     uint24 byte_offset,
    //     uint40 length
    // ) public view returns (bool success, bytes memory data) {
    //     return
    //         readBytesFromChunkRange(
    //             chunk_range_index,
    //             uint16(0),
    //             byte_offset,
    //             length
    //         );
    // }

    // function readBytesFromChunkRange(
    //     uint8 chunk_range_index,
    //     uint16 chunk_offset,
    //     uint24 byte_offset,
    //     uint40 length
    // ) public view returns (bool success, bytes memory data) {
    //     BytesRangeSpecifier memory range_specifier = BytesRangeSpecifier(
    //         chunk_range_index,
    //         chunk_offset,
    //         byte_offset,
    //         length
    //     );
    //     return readByteRange(range_specifier);
    // }

    // function readByteRange(
    //     BytesRangeSpecifier memory read_range
    // ) public view returns (bool success, bytes memory data) {
    //     return
    //         address(PD_READ_PRECOMPILE_ADDRESS).staticcall(
    //             abi.encode(bytes1(READ_CUSTOM_BYTE_RANGE), read_range)
    //         );
    // }
}
