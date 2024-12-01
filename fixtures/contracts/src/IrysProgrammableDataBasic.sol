// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

struct ProgrammableDataArgs {
    uint32 range_index;
    uint32 offset;
    uint16 count;
}

contract ProgrammableDataBasic {
    bytes public storedData;

    function read_pd_chunk_into_storage(
        ProgrammableDataArgs calldata arg,
        uint8 read_bytes
    ) public {
        uint32 range_index = arg.range_index;
        uint32 offset = arg.offset;
        uint16 count = arg.count;

        bytes memory data = abi.encodePacked(range_index, offset, count);

        // call the precompile
        (bool success, bytes memory chunk) = address(0x539).staticcall(data);

        require(success, "loading chunks failed");

        bytes memory out = new bytes(read_bytes);
        for (uint i = 0; i < read_bytes; i++) {
            out[i] = chunk[i];
        }
        // write bytes to storage
        storedData = out;
    }

    function get_storage() public view returns (bytes memory) {
        return storedData;
    }
}
