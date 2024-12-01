// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

struct PdArgs {
    uint32 range_index;
    uint32 offset;
    uint16 count;
}

contract ProgrammableDataBasic {
    event CallResult(bool success, bytes data);
    event Log(uint256);

    event BytesLog(bytes data);

    bytes32 public storageData;

    function get_pd_chunks(PdArgs[] calldata args) public {
        // function get_pd_chunks(bytes calldata args) public {
        emit Log(uint256(1));

        emit Log(uint256(args.length));

        bytes memory result;
        uint16 one = 1;

        for (uint i = 0; i < args.length; i++) {
            PdArgs memory arg = args[i];
            uint32 range_index = arg.range_index;
            uint32 offset = arg.offset;
            uint16 count = arg.count;
            emit Log(uint256(2));
            emit Log(uint256(range_index));
            emit Log(uint256(offset));
            emit Log(uint256(count));
            emit Log(uint256(3));

            for (uint32 j = offset; j < count; j++) {
                // read a single chunk from the current range
                // when encodePacked gets removed/deprecated, use `bytes.concat(abi.encode(range_index), abi.encode(offset), abi.encode(count))`
                bytes memory data = abi.encodePacked(range_index, j, one);
                // call the precompile

                (bool success, bytes memory chunk) = address(
                    0x0000000000000000000000000000000000000539
                ).staticcall(data);

                // can be made `view` if the emit is removed
                emit CallResult(success, result);

                require(success, "get_pd_chunks: loading chunks failed");
                result = bytes.concat(result, chunk[0]);
            }

            // read a single chunk from the current range
            // when encodePacked gets removed/deprecated, use `bytes.concat(abi.encode(range_index), abi.encode(offset), abi.encode(count))`

            // bytes memory data = abi.encodePacked(range_index, offset, one);

            // // call the precompile
            // emit Log(uint256(4));
            // emit CallResult(true, args);

            // // (bool success, bytes memory chunk) = address(0x539).staticcall(data);
            // (bool success, bytes memory chunk) = address(0x45).staticcall(args);
            // // (bool success, bytes memory chunk) = address(0x46).staticcall(args);

            // // (bool success, bytes memory chunk) = address(0x4).staticcall(args);

            // emit Log(uint256(chunk.length));

            // emit Log(uint256(5));
            // emit BytesLog(chunk);
            // // emit CallResult(success, chunk);

            // emit Log(uint256(6));

            // require(success, "get_pd_chunks: loading chunks failed");

            // // can be made `view` if the emit is removed
            // result = bytes.concat(result, chunk[0]);
            // emit CallResult(success, result);
        }

        storageData = bytes32(uint256(69));
        bytes32 padded = bytes32(result);
        // // store to a specific storage slot (0), padding to 32 bytes
        bytes32 slot = bytes32(uint256(1337));
        assembly {
            mstore(add(padded, 32), mload(add(result, 32)))
            sstore(slot, padded)
        }
    }

    function get_storage() public view returns (bytes32) {
        return storageData;
    }
}
