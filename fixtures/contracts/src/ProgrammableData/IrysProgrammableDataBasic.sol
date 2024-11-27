// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

contract ProgrammableDataBasic {
    event CallResult(bool success, bytes data);

    function get_pd_chunks(
        uint256 ledger_index,
        uint256 offset,
        uint256 count /* view  */
    ) public returns (bytes memory) {
        bytes memory data = abi.encodePacked(ledger_index, offset, count);
        // when encodePacked gets removed/deprecated, use `bytes.concat(abi.encode(ledger_index), abi.encode(offset), abi.encode(count))`

        // call the precompile
        (bool success, bytes memory result) = address(
            0x0000000000000000000000000000000000000539
        ).staticcall(data);

        // can be made `view` if the emit is removed
        emit CallResult(success, result);

        require(success, "get_pd_chunks: loading chunks failed");

        return result;
    }
}
