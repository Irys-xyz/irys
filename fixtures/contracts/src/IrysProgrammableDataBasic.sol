// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "./IIrysPD.sol";
import "./IrysPDLib.sol";

/// @title ProgrammableDataBasic — minimal example of reading PD chunk data
/// @dev Uses IrysPDLib convenience wrappers. Access-list specifiers are set by
///      the transaction sender, not by this contract.
contract ProgrammableDataBasic {
    using IrysPDLib for *;

    bytes public storedData;

    /// @notice Read specifier-0 data into contract storage.
    function readPdChunkIntoStorage() public {
        storedData = IrysPDLib.readData();
    }

    /// @notice Return stored data.
    function getStorage() public view returns (bytes memory) {
        return storedData;
    }
}
