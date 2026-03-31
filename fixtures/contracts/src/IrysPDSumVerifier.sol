// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "./IIrysPD.sol";
import "./IrysPDLib.sol";

/// @title PdSumVerifier — reads the full PD data blob and verifies byte-level content
/// @dev Access-list specifier 0 must declare a window covering at least 4 MB + 1 byte.
///      The contract reads the entire blob in one readData() call and uses array
///      indexing (data[n]) to sample bytes at 1 MB intervals.
contract PdSumVerifier {
    using IrysPDLib for *;

    uint32 constant MB = 1048576;

    /// @notice Reads the full PD data via readData(), then checks that the bytes
    ///         at offsets 0, 1 MB, 2 MB, 3 MB, 4 MB sum to exactly 28.
    ///         Uses distinct primes (2,3,5,7,11) so no permutation or substitution collides.
    ///         Reverts with "PD byte sum != 28" if the sum is wrong.
    function verifySumAt1MbOffsets() public view {
        bytes memory data = IrysPDLib.readData();
        uint8 sum = uint8(data[0])
                  + uint8(data[MB])
                  + uint8(data[2 * MB])
                  + uint8(data[3 * MB])
                  + uint8(data[4 * MB]);
        require(sum == 28, "PD byte sum != 28");
    }
}
