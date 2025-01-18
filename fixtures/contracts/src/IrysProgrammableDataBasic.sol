// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "./ProgrammableData.sol";

contract ProgrammableDataBasic is ProgrammableData {
    bytes public storedData;

    function readPdChunkIntoStorage() public {
        (bool success, bytes memory data) = readBytes();
        require(success, "reading bytes failed");
        // write bytes to storage
        storedData = data;
    }

    function getStorage() public view returns (bytes memory) {
        return storedData;
    }
}
