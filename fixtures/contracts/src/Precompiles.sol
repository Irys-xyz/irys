// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

address constant PD_READ_PRECOMPILE_ADDRESS = address(0x539);

// internal PD function IDs
// ID for the function that reads an entire byte range, by index.
uint8 constant READ_FULL_BYTE_RANGE = 0;
// read part of a byte range, by index with an offset (range offset + provided offset) and an overriden length
uint8 constant READ_PARTIAL_BYTE_RANGE = 1;
// // read using a custom byte range
// uint8 constant READ_CUSTOM_BYTE_RANGE = 2;
