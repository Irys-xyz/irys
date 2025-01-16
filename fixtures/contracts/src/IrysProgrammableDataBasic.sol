// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

contract ProgrammableDataBasic {
    bytes public storedData;

    function read_pd_chunk_into_storage() public {
        // call the precompile
        uint8 function_id = 0;
        (bool success, bytes memory chunk) = address(0x539).staticcall(
            abi.encodePacked(function_id)
        );

        require(success, "reading bytes failed");

        // write bytes to storage
        storedData = chunk;
    }

    function get_storage() public view returns (bytes memory) {
        return storedData;
    }
}
