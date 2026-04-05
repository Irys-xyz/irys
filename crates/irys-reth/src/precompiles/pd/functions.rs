//! PD precompile ABI definitions via alloy sol! macro.
//!
//! Generates 4-byte selectors, parameter decoders, return encoders,
//! and error struct encoders at compile time.

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
