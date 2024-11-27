// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

contract ProgrammableDataBasic {
    function get_pd_chunks(
        uint256 ledger_index,
        uint256 offset,
        uint256 count
    ) public view returns (bytes memory) {
        bytes memory data = abi.encodePacked(ledger_index, offset, count);
        // when encodePacked gets removed/deprecated, use `bytes.concat(abi.encode(ledger_index), abi.encode(offset), abi.encode(count))`

        // call the precompile
        (bool success, bytes memory result) = address(0x1).staticcall(data);
        require(success, "get_pd_chunks: loading chunks failed");

        return result;
    }
}
