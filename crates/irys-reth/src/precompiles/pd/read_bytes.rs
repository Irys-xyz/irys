//! PD precompile byte range read implementations.
//!
//! Provides three read operations for accessing byte ranges from the Programmable Data system:
//!
//! 1. **Full byte range read** ([`read_bytes_range_by_index`]) - Reads complete byte range from access list
//! 2. **Partial byte range read** ([`read_partial_byte_range`]) - Applies offset/length to existing range
//! 3. **Core byte range read** ([`read_bytes_range`]) - Fetches chunks and returns requested bytes

use alloy_primitives::{Bytes, aliases::U200};
use irys_types::range_specifier::{ByteRangeSpecifier, ChunkRangeSpecifier, U34};
use irys_types::storage::ii;
use irys_types::{LedgerChunkOffset, LedgerChunkRange, ledger_chunk_offset_ii};
use revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};
use tracing::{debug, warn};

use super::constants::{
    INDEX_OFFSET, LENGTH_FIELD_OFFSET, LENGTH_SIZE, OFFSET_FIELD_OFFSET, OFFSET_SIZE,
    READ_BYTES_RANGE_BY_INDEX_CALLDATA_LEN, READ_PARTIAL_BYTE_RANGE_CALLDATA_LEN,
};
use super::context::PdContext;
use super::error::PdPrecompileError;
use super::utils::ParsedAccessLists;

fn calculate_chunk_offsets(
    partition_index: &U200,
    offset: u32,
    chunk_count: u16,
    num_chunks_in_partition: u64,
) -> Result<LedgerChunkRange, PdPrecompileError> {
    let partition_index_u64: u64 = (*partition_index).try_into().map_err(|_| {
        warn!(partition_index = %partition_index, "Partition index exceeds u64::MAX");
        PdPrecompileError::PartitionIndexTooLarge {
            index: partition_index.to_string(),
        }
    })?;

    let base_offset = num_chunks_in_partition
        .checked_mul(partition_index_u64)
        .ok_or_else(|| {
            warn!(
                num_chunks_in_partition,
                partition_index = partition_index_u64,
                "Partition offset calculation overflow"
            );
            PdPrecompileError::OffsetOutOfRange {
                chunk_offset: 0,
                chunk_size: num_chunks_in_partition,
                byte_offset: partition_index_u64,
            }
        })?;

    let start = base_offset.checked_add(offset as u64).ok_or_else(|| {
        warn!(
            base_offset,
            chunk_offset = offset,
            "Chunk start offset calculation overflow"
        );
        PdPrecompileError::OffsetOutOfRange {
            chunk_offset: offset as u16,
            chunk_size: 0,
            byte_offset: base_offset,
        }
    })?;

    let end = start.checked_add(chunk_count as u64).ok_or_else(|| {
        warn!(start, chunk_count, "Chunk end offset calculation overflow");
        PdPrecompileError::OffsetOutOfRange {
            chunk_offset: chunk_count,
            chunk_size: 0,
            byte_offset: start,
        }
    })?;

    Ok(LedgerChunkRange::from(ledger_chunk_offset_ii!(start, end)))
}

fn calculate_byte_offset(
    chunk_offset: u16,
    byte_offset: u64,
    chunk_size: u64,
) -> Result<usize, PdPrecompileError> {
    let chunk_bytes = u64::from(chunk_offset)
        .checked_mul(chunk_size)
        .ok_or_else(|| {
            warn!(chunk_offset, chunk_size, "Chunk byte offset overflow");
            PdPrecompileError::OffsetOutOfRange {
                chunk_offset,
                chunk_size,
                byte_offset: 0,
            }
        })?;

    let absolute_offset = chunk_bytes.checked_add(byte_offset).ok_or_else(|| {
        warn!(chunk_bytes, byte_offset, "Absolute byte offset overflow");
        PdPrecompileError::OffsetOutOfRange {
            chunk_offset,
            chunk_size,
            byte_offset,
        }
    })?;

    absolute_offset.try_into().map_err(|_| {
        warn!(absolute_offset, "Offset exceeds usize::MAX");
        PdPrecompileError::OffsetOutOfRange {
            chunk_offset,
            chunk_size,
            byte_offset,
        }
    })
}

#[derive(Debug)]
struct ReadBytesRangeByIndexArgs {
    index: u8,
}

impl ReadBytesRangeByIndexArgs {
    pub(crate) fn decode(bytes: &Bytes) -> Result<Self, PrecompileError> {
        if bytes.len() != READ_BYTES_RANGE_BY_INDEX_CALLDATA_LEN {
            return Err(PdPrecompileError::InvalidCalldataLength {
                function: "ReadFullByteRange",
                expected: READ_BYTES_RANGE_BY_INDEX_CALLDATA_LEN,
                actual: bytes.len(),
            }
            .into());
        }
        Ok(Self {
            index: bytes[INDEX_OFFSET],
        })
    }
}

pub fn read_bytes_range_by_index(
    call_data: &Bytes,
    _gas_limit: u64,
    context: &PdContext,
    access_lists: ParsedAccessLists,
) -> PrecompileResult {
    let ReadBytesRangeByIndexArgs { index } = ReadBytesRangeByIndexArgs::decode(call_data)?;
    let bytes_range = *access_lists.byte_reads.get(index as usize).ok_or(
        PdPrecompileError::ByteRangeNotFound {
            index,
            available: access_lists.byte_reads.len(),
        },
    )?;
    read_bytes_range(bytes_range, context, access_lists)
}

#[derive(Debug)]
struct ReadPartialByteRangeArgs {
    index: u8,
    offset: u32,
    length: u32,
}

impl ReadPartialByteRangeArgs {
    pub(crate) fn decode(bytes: &Bytes) -> Result<Self, PrecompileError> {
        if bytes.len() != READ_PARTIAL_BYTE_RANGE_CALLDATA_LEN {
            return Err(PdPrecompileError::InvalidCalldataLength {
                function: "ReadPartialByteRange",
                expected: READ_PARTIAL_BYTE_RANGE_CALLDATA_LEN,
                actual: bytes.len(),
            }
            .into());
        }

        let index = bytes[INDEX_OFFSET];

        let offset_end = OFFSET_FIELD_OFFSET + OFFSET_SIZE;
        let offset =
            u32::from_be_bytes(bytes[OFFSET_FIELD_OFFSET..offset_end].try_into().map_err(
                |_| PdPrecompileError::InvalidCalldataLength {
                    function: "ReadPartialByteRange (offset field)",
                    expected: OFFSET_SIZE,
                    actual: bytes[OFFSET_FIELD_OFFSET..].len(),
                },
            )?);

        let length_end = LENGTH_FIELD_OFFSET + LENGTH_SIZE;
        let length =
            u32::from_be_bytes(bytes[LENGTH_FIELD_OFFSET..length_end].try_into().map_err(
                |_| PdPrecompileError::InvalidCalldataLength {
                    function: "ReadPartialByteRange (length field)",
                    expected: LENGTH_SIZE,
                    actual: bytes[LENGTH_FIELD_OFFSET..].len(),
                },
            )?);

        Ok(Self {
            index,
            offset,
            length,
        })
    }
}

pub fn read_partial_byte_range(
    call_data: &Bytes,
    _gas_limit: u64,
    context: &PdContext,
    access_lists: ParsedAccessLists,
) -> PrecompileResult {
    let ReadPartialByteRangeArgs {
        index,
        offset,
        length,
    } = ReadPartialByteRangeArgs::decode(call_data)?;

    let mut bytes_range = *access_lists.byte_reads.get(index as usize).ok_or(
        PdPrecompileError::ByteRangeNotFound {
            index,
            available: access_lists.byte_reads.len(),
        },
    )?;

    let chunk_size = context.chunk_provider().config().chunk_size;
    bytes_range
        .translate_offset(chunk_size, offset as u64)
        .map_err(|e| PdPrecompileError::OffsetTranslationFailed {
            offset,
            reason: e.to_string(),
        })?;

    bytes_range.length = U34::try_from(length).map_err(|e| PdPrecompileError::InvalidLength {
        length,
        reason: e.to_string(),
    })?;

    read_bytes_range(bytes_range, context, access_lists)
}

pub fn read_bytes_range(
    bytes_range: ByteRangeSpecifier,
    context: &PdContext,
    access_lists: ParsedAccessLists,
) -> PrecompileResult {
    let ByteRangeSpecifier {
        index,
        chunk_offset,
        byte_offset,
        length,
    } = bytes_range;

    let chunk_read_range = access_lists.chunk_reads.get(index as usize).ok_or(
        PdPrecompileError::ChunkRangeNotFound {
            index,
            available: access_lists.chunk_reads.len(),
        },
    )?;

    let ChunkRangeSpecifier {
        partition_index,
        offset,
        chunk_count,
    } = chunk_read_range;

    let chunk_provider = context.chunk_provider();
    let config = chunk_provider.config();
    let chunk_size = config.chunk_size;

    let ledger_range = calculate_chunk_offsets(
        partition_index,
        *offset,
        *chunk_count,
        config.num_chunks_in_partition,
    )?;

    let ledger_start = ledger_range.start();
    let ledger_end = ledger_range.end();

    debug!(
        partition_index = %partition_index,
        chunk_offset = offset,
        chunk_count = chunk_count,
        ledger_start = *ledger_start,
        ledger_end = *ledger_end,
        "Fetching chunks from storage"
    );

    // Fetch and collect all chunks
    let mut bytes = Vec::with_capacity((*chunk_count as u64 * chunk_size) as usize);

    for ledger_offset in *ledger_start..*ledger_end {
        let unpacked_chunk = chunk_provider
            .get_unpacked_chunk_by_ledger_offset(0, ledger_offset)
            .map_err(|e| {
                warn!(ledger_offset, error = %e, "Failed to fetch chunk");
                PdPrecompileError::ChunkFetchFailed {
                    offset: ledger_offset,
                    reason: e.to_string(),
                }
            })?
            .ok_or_else(|| {
                warn!(ledger_offset, "Chunk not found");
                PdPrecompileError::ChunkNotFound {
                    offset: ledger_offset,
                }
            })?;

        bytes.extend(unpacked_chunk);
    }

    debug!(
        total_bytes_fetched = bytes.len(),
        chunks_fetched = chunk_count,
        "Successfully fetched all chunks"
    );

    let offset_usize = calculate_byte_offset(chunk_offset, byte_offset.to(), chunk_size)?;

    let length_usize: usize = length.to::<u64>().try_into().map_err(|_| {
        warn!(length = %length, "Length conversion out of range");
        PdPrecompileError::LengthOutOfRange {
            length: length.to::<u64>(),
        }
    })?;

    // Bounds check
    if offset_usize + length_usize > bytes.len() {
        warn!(
            offset = offset_usize,
            length = length_usize,
            end = offset_usize + length_usize,
            available = bytes.len(),
            "Requested byte range exceeds available data"
        );
        return Err(PdPrecompileError::ByteRangeOutOfBounds {
            start: offset_usize,
            end: offset_usize + length_usize,
            available: bytes.len(),
        }
        .into());
    }

    // Extract exact bytes requested
    let extracted: Bytes = bytes
        .drain(offset_usize..offset_usize + length_usize)
        .collect();

    debug!(
        bytes_extracted = extracted.len(),
        "Byte range extraction successful"
    );

    Ok(PrecompileOutput {
        gas_used: 0,
        gas_refunded: 0,
        bytes: extracted,
        reverted: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::precompiles::pd::context::PdContext;
    use crate::precompiles::pd::utils::ParsedAccessLists;
    use irys_types::chunk_provider::MockChunkProvider;
    use irys_types::range_specifier::{ByteRangeSpecifier, ChunkRangeSpecifier};
    use std::sync::Arc;

    /// Creates a test PD context with a mock chunk provider.
    fn create_test_context() -> PdContext {
        let mock_chunk_provider = Arc::new(MockChunkProvider::new());
        PdContext::new(mock_chunk_provider)
    }

    /// Creates parsed access lists from chunk and byte range specifiers.
    fn create_test_parsed_access_lists(
        chunk_reads: Vec<ChunkRangeSpecifier>,
        byte_reads: Vec<ByteRangeSpecifier>,
    ) -> ParsedAccessLists {
        ParsedAccessLists {
            chunk_reads,
            byte_reads,
        }
    }

    mod read_bytes_range_by_index_args {
        use super::*;
        use rstest::rstest;

        #[rstest]
        #[case(&[0], true)] // Too short (need 2 bytes)
        #[case(&[0, 1], false)] // Valid length
        #[case(&[0, 1, 2], true)] // Too long
        fn test_length_validation(#[case] calldata: &[u8], #[case] should_error: bool) {
            let bytes = Bytes::from(calldata.to_vec());
            let result = ReadBytesRangeByIndexArgs::decode(&bytes);

            if should_error {
                let err = result.expect_err("Should fail with invalid calldata length");
                assert!(
                    matches!(err, PrecompileError::Other(_)),
                    "Expected PrecompileError::Other, got: {:?}",
                    err
                );
                if let PrecompileError::Other(msg) = err {
                    assert!(
                        msg.contains("invalid calldata length"),
                        "Expected 'invalid calldata length' in error, got: {}",
                        msg
                    );
                }
            } else {
                assert!(result.is_ok(), "Valid calldata should decode successfully");
            }
        }
    }

    mod read_partial_byte_range_args {
        use super::*;
        use rstest::rstest;

        #[rstest]
        #[case(&[0, 1, 2, 3], true)] // Too short (need 10 bytes)
        #[case(&[0; 10], false)] // Valid length
        #[case(&[0; 11], true)] // Too long
        fn test_length_validation(#[case] calldata: &[u8], #[case] should_error: bool) {
            let bytes = Bytes::from(calldata.to_vec());
            let result = ReadPartialByteRangeArgs::decode(&bytes);

            if should_error {
                let err = result.expect_err("Should fail with invalid calldata length");
                assert!(
                    err.to_string().contains("invalid calldata length"),
                    "Expected 'invalid calldata length' in error, got: {}",
                    err
                );
            } else {
                assert!(result.is_ok(), "Valid calldata should decode successfully");
            }
        }
    }

    mod read_bytes_range_by_index {
        use super::*;

        #[test]
        fn test_index_out_of_bounds() {
            let context = create_test_context();
            let calldata = Bytes::from(vec![0, 5]); // Request index 5

            // Create access list with only 2 byte reads
            use irys_types::range_specifier::U18;
            let byte_reads = vec![
                ByteRangeSpecifier {
                    index: 0,
                    chunk_offset: 0,
                    byte_offset: U18::ZERO,
                    length: U34::try_from(100).unwrap(),
                },
                ByteRangeSpecifier {
                    index: 0,
                    chunk_offset: 0,
                    byte_offset: U18::ZERO,
                    length: U34::try_from(100).unwrap(),
                },
            ];
            let parsed = create_test_parsed_access_lists(vec![], byte_reads);

            let result = read_bytes_range_by_index(&calldata, 100000, &context, parsed);

            let err = result.expect_err("Should fail with index out of bounds");
            if let PrecompileError::Other(msg) = err {
                assert!(
                    msg.contains("not found in access list"),
                    "Expected 'not found in access list' in error, got: {}",
                    msg
                );
            }
        }

        #[test]
        fn test_invalid_calldata_length() {
            use irys_types::range_specifier::U18;
            let context = create_test_context();
            let calldata = Bytes::from(vec![0]); // Too short

            let byte_reads = vec![ByteRangeSpecifier {
                index: 0,
                chunk_offset: 0,
                byte_offset: U18::ZERO,
                length: U34::try_from(100).unwrap(),
            }];
            let parsed = create_test_parsed_access_lists(vec![], byte_reads);

            let result = read_bytes_range_by_index(&calldata, 100000, &context, parsed);

            let err = result.expect_err("Should fail with invalid calldata");
            if let PrecompileError::Other(msg) = err {
                assert!(
                    msg.contains("invalid calldata length"),
                    "Expected 'invalid calldata length' in error, got: {}",
                    msg
                );
            }
        }
    }

    mod read_partial_byte_range {
        use super::*;

        #[test]
        fn test_invalid_calldata() {
            use irys_types::range_specifier::U18;
            let context = create_test_context();
            let calldata = Bytes::from(vec![0; 5]); // Wrong length

            let byte_reads = vec![ByteRangeSpecifier {
                index: 0,
                chunk_offset: 0,
                byte_offset: U18::ZERO,
                length: U34::try_from(100).unwrap(),
            }];
            let parsed = create_test_parsed_access_lists(vec![], byte_reads);

            let result = read_partial_byte_range(&calldata, 100000, &context, parsed);

            let err = result.expect_err("Should fail with invalid calldata");
            if let PrecompileError::Other(msg) = err {
                assert!(
                    msg.contains("invalid calldata"),
                    "Expected 'invalid calldata' in error, got: {}",
                    msg
                );
            }
        }

        #[test]
        fn test_index_out_of_bounds() {
            use irys_types::range_specifier::U18;
            let context = create_test_context();

            let mut calldata = vec![0, 10]; // function_id=0, index=10
            calldata.extend_from_slice(&0_u32.to_be_bytes());
            calldata.extend_from_slice(&100_u32.to_be_bytes());
            let calldata = Bytes::from(calldata);

            // Only 2 byte reads, but requesting index 10
            let byte_reads = vec![
                ByteRangeSpecifier {
                    index: 0,
                    chunk_offset: 0,
                    byte_offset: U18::ZERO,
                    length: U34::try_from(100).unwrap(),
                },
                ByteRangeSpecifier {
                    index: 0,
                    chunk_offset: 0,
                    byte_offset: U18::ZERO,
                    length: U34::try_from(100).unwrap(),
                },
            ];
            let parsed = create_test_parsed_access_lists(vec![], byte_reads);

            let result = read_partial_byte_range(&calldata, 100000, &context, parsed);

            let err = result.expect_err("Should fail with index out of bounds");
            if let PrecompileError::Other(msg) = err {
                assert!(
                    msg.contains("not found in access list"),
                    "Expected 'not found in access list' in error, got: {}",
                    msg
                );
            }
        }
    }

    mod read_bytes_range {
        use super::*;

        #[test]
        fn test_chunk_range_index_out_of_bounds() {
            use alloy_primitives::aliases::U200;
            use irys_types::range_specifier::U18;
            let context = create_test_context();

            // ByteRangeSpecifier with index=5 but only provide 2 chunk reads
            let byte_range = ByteRangeSpecifier {
                index: 5,
                chunk_offset: 0,
                byte_offset: U18::ZERO,
                length: U34::try_from(100).unwrap(),
            };

            let chunk_reads = vec![
                ChunkRangeSpecifier {
                    partition_index: U200::ZERO,
                    offset: 0,
                    chunk_count: 1,
                },
                ChunkRangeSpecifier {
                    partition_index: U200::ZERO,
                    offset: 1,
                    chunk_count: 1,
                },
            ];

            let parsed = create_test_parsed_access_lists(chunk_reads, vec![]);

            let result = read_bytes_range(byte_range, &context, parsed);

            let err = result.expect_err("Should fail with chunk index out of bounds");
            if let PrecompileError::Other(msg) = err {
                assert!(
                    msg.contains("chunk range index") && msg.contains("not found"),
                    "Expected 'chunk range index ... not found' in error, got: {}",
                    msg
                );
            }
        }

        #[test]
        fn test_valid_read() {
            use alloy_primitives::aliases::U200;
            use irys_types::range_specifier::U18;
            let context = create_test_context();

            let byte_range = ByteRangeSpecifier {
                index: 0,
                chunk_offset: 0,
                byte_offset: U18::ZERO,
                length: U34::try_from(100).unwrap(),
            };

            let chunk_reads = vec![ChunkRangeSpecifier {
                partition_index: U200::ZERO,
                offset: 0,
                chunk_count: 1,
            }];

            let parsed = create_test_parsed_access_lists(chunk_reads, vec![]);

            let result = read_bytes_range(byte_range, &context, parsed);

            assert!(result.is_ok(), "Valid read should succeed");
            let output = result.unwrap();
            assert_eq!(output.bytes.len(), 100);
            assert_eq!(output.gas_used, 0);
        }
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn decode_never_panics_on_arbitrary_bytes(bytes: Vec<u8>) {
                let bytes = Bytes::from(bytes);

                // Should never panic - only return Ok or Err
                let _ = ReadBytesRangeByIndexArgs::decode(&bytes);
                let _ = ReadPartialByteRangeArgs::decode(&bytes);
            }

            #[test]
            fn valid_calldata_roundtrip_read_bytes_range_by_index(index: u8) {
                let calldata = vec![0, index];
                let bytes = Bytes::from(calldata);

                let decoded = ReadBytesRangeByIndexArgs::decode(&bytes);
                prop_assert!(decoded.is_ok());
                prop_assert_eq!(decoded.unwrap().index, index);
            }

            #[test]
            fn valid_calldata_roundtrip_read_partial_byte_range(
                index: u8,
                offset: u32,
                length: u32
            ) {
                let mut calldata = vec![0, index];
                calldata.extend_from_slice(&offset.to_be_bytes());
                calldata.extend_from_slice(&length.to_be_bytes());

                let bytes = Bytes::from(calldata);
                let decoded = ReadPartialByteRangeArgs::decode(&bytes);

                prop_assert!(decoded.is_ok());
                let args = decoded.unwrap();
                prop_assert_eq!(args.index, index);
                prop_assert_eq!(args.offset, offset);
                prop_assert_eq!(args.length, length);
            }

            #[test]
            fn translate_offset_never_panics(
                index: u8,
                chunk_offset: u16,
                byte_offset in 0_u64..262_144,
                length in 0_u32..100_000,
                offset_delta in 0_u64..10_000,
                chunk_size in 1_u64..1_000_000
            ) {
                use irys_types::range_specifier::U18;

                let Ok(length_u34) = U34::try_from(length) else { return Ok(()); };
                let Ok(byte_offset_u18) = U18::try_from(byte_offset) else { return Ok(()); };

                let mut range = ByteRangeSpecifier {
                    index,
                    chunk_offset,
                    byte_offset: byte_offset_u18,
                    length: length_u34,
                };

                // Property: translate_offset should never panic
                let _ = range.translate_offset(chunk_size, offset_delta);
            }
        }
    }
}
