// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

contract SimpleStorage {
    event Log(uint256);

    uint256 public storedData;

    function set(uint256 value) public {
        emit Log(value);
        storedData = value;
    }

    function get() public view returns (uint256) {
        return storedData;
    }
}
