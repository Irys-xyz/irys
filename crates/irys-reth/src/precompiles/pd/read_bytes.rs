//! PD precompile chunk read implementation.
//!
//! Provides a single core function ([`read_chunk_data`]) that fetches chunk data
//! for a [`PdDataRead`] specifier and extracts the requested byte range.

use alloy_primitives::Bytes;
use irys_types::range_specifier::PdDataRead;
use tracing::{debug, warn};

use super::context::PdContext;
use super::error::PdPrecompileError;

/// Read chunk data for a [`PdDataRead`] specifier and extract the requested byte range.
///
/// # Arguments
///
/// - `spec` — the decoded data read specifier from the access list
/// - `offset` — byte offset into the concatenated chunk data (`byte_off` for readData,
///   calldata offset for readBytes)
/// - `length` — number of bytes to read (`len` for readData, calldata length for readBytes)
/// - `context` — PD execution context providing chunk config and chunk retrieval
///
/// # Gas
///
/// Returns `gas_used = 0` in the [`PrecompileOutput`](revm::precompile::PrecompileOutput).
/// PD chunk I/O is metered via per-chunk fees in IRYS tokens (deducted in
/// `IrysEvm::transact_raw`), not via EVM gas. The caller adds `PD_BASE_GAS_COST` on top.
pub fn read_chunk_data(
    spec: &PdDataRead,
    offset: u32,
    length: u32,
    context: &PdContext,
) -> Result<Bytes, PdPrecompileError> {
    let config = context.config();
    let chunk_size = config.chunk_size;

    // The declared access window is [byte_off, byte_off + len).
    let window_end = spec.byte_off as u64 + spec.len as u64;

    // Validate length > 0
    if length == 0 {
        return Err(PdPrecompileError::ByteRangeOutOfBounds {
            start: offset as usize,
            end: offset as usize,
            available: window_end as usize,
        });
    }

    // Validate range is within the declared window [byte_off, byte_off + len)
    let req_end = (offset as u64).checked_add(length as u64).ok_or_else(|| {
        warn!(offset, length, "Byte range end overflow");
        PdPrecompileError::ByteRangeOutOfBounds {
            start: offset as usize,
            end: usize::MAX,
            available: window_end as usize,
        }
    })?;

    if (offset as u64) < spec.byte_off as u64 || req_end > window_end {
        warn!(
            offset,
            length,
            byte_off = spec.byte_off,
            len = spec.len,
            "Requested byte range exceeds declared PdDataRead window"
        );
        return Err(PdPrecompileError::ByteRangeOutOfBounds {
            start: offset as usize,
            end: req_end as usize,
            available: window_end as usize,
        });
    }

    // Determine which chunks overlap with [offset, offset + length)
    let first_chunk = offset as u64 / chunk_size;
    let last_chunk = (req_end - 1) / chunk_size;

    // Calculate ledger offsets from partition-relative coordinates
    let base_offset = config
        .num_chunks_in_partition
        .checked_mul(spec.partition_index)
        .ok_or_else(|| {
            warn!(
                partition_index = spec.partition_index,
                num_chunks_in_partition = config.num_chunks_in_partition,
                "Partition offset calculation overflow"
            );
            PdPrecompileError::OffsetOutOfRange {
                partition_index: spec.partition_index,
                start: spec.start,
                num_chunks_in_partition: config.num_chunks_in_partition,
            }
        })?;

    let ledger_start = base_offset.checked_add(spec.start as u64).ok_or_else(|| {
        warn!(
            base_offset,
            start = spec.start,
            "Ledger start offset calculation overflow"
        );
        PdPrecompileError::OffsetOutOfRange {
            partition_index: spec.partition_index,
            start: spec.start,
            num_chunks_in_partition: config.num_chunks_in_partition,
        }
    })?;

    debug!(
        partition_index = spec.partition_index,
        start = spec.start,
        first_chunk,
        last_chunk,
        ledger_start,
        "Fetching chunks from cache"
    );

    // Fetch only the overlapping chunks and extract the requested slice
    let mut result = Vec::with_capacity(length as usize);
    for i in first_chunk..=last_chunk {
        let ledger_offset = ledger_start.checked_add(i).ok_or_else(|| {
            warn!(ledger_start, chunk_index = i, "Ledger offset overflow");
            PdPrecompileError::OffsetOutOfRange {
                partition_index: spec.partition_index,
                start: spec.start,
                num_chunks_in_partition: config.num_chunks_in_partition,
            }
        })?;

        let unpacked_chunk = context
            .get_chunk(0, ledger_offset)
            .map_err(|e| {
                warn!(ledger_offset, error = %e, "Failed to fetch chunk from cache");
                PdPrecompileError::ChunkFetchFailed {
                    offset: ledger_offset,
                    reason: e.to_string(),
                }
            })?
            .ok_or_else(|| {
                warn!(ledger_offset, "Chunk not found in cache");
                PdPrecompileError::ChunkNotFound {
                    offset: ledger_offset,
                }
            })?;

        // Compute the byte slice within this chunk that overlaps the request.
        // The last chunk of a data transaction may be shorter than chunk_size,
        // so we must clamp to the actual chunk length to avoid an OOB panic.
        let actual_len = unpacked_chunk.len();
        let chunk_byte_start = i * chunk_size;
        let slice_start = if offset as u64 > chunk_byte_start {
            (offset as u64 - chunk_byte_start) as usize
        } else {
            0
        };
        let chunk_byte_end = chunk_byte_start + chunk_size;
        let slice_end = if req_end < chunk_byte_end {
            (req_end - chunk_byte_start) as usize
        } else {
            actual_len
        };

        if slice_start >= actual_len || slice_end > actual_len {
            warn!(
                ledger_offset,
                slice_start,
                slice_end,
                actual_len,
                "Chunk shorter than expected; requested range exceeds chunk data"
            );
            return Err(PdPrecompileError::ChunkFetchFailed {
                offset: ledger_offset,
                reason: format!(
                    "chunk at offset {ledger_offset} has {actual_len} bytes, \
                     need [{slice_start}..{slice_end})"
                ),
            });
        }

        result.extend_from_slice(&unpacked_chunk[slice_start..slice_end]);
    }

    debug!(
        bytes_extracted = result.len(),
        "Byte range extraction successful"
    );

    Ok(Bytes::copy_from_slice(&result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::precompiles::pd::context::PdContext;
    use dashmap::DashMap;
    use irys_types::chunk_provider::ChunkConfig;
    use std::sync::Arc;

    /// Creates a test PD context with N pre-populated zero-filled chunks at offsets 0..N.
    fn create_test_context_with_chunks(num_chunks: u64) -> PdContext {
        let chunk_config = ChunkConfig {
            num_chunks_in_partition: 100,
            chunk_size: 256_000,
            entropy_packing_iterations: 0,
            chain_id: 1,
        };
        let chunk_data_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
        let chunk = Arc::new(bytes::Bytes::from(vec![0_u8; 256_000]));
        for offset in 0..num_chunks {
            chunk_data_index.insert((0_u32, offset), chunk.clone());
        }
        PdContext::new(chunk_config, chunk_data_index)
    }

    #[test]
    fn test_valid_read_single_chunk() {
        let context = create_test_context_with_chunks(1);
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 100,
            byte_off: 0,
        };

        let result = read_chunk_data(&spec, 0, 100, &context);
        assert!(result.is_ok(), "Valid single-chunk read should succeed");
        assert_eq!(result.unwrap().len(), 100);
    }

    #[test]
    fn test_read_with_byte_offset() {
        let context = create_test_context_with_chunks(1);
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 50,
            byte_off: 100,
        };

        let result = read_chunk_data(&spec, 100, 50, &context);
        assert!(result.is_ok(), "Read with byte offset should succeed");
        assert_eq!(result.unwrap().len(), 50);
    }

    #[test]
    fn test_read_partial_with_custom_offset_and_length() {
        let context = create_test_context_with_chunks(2);
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 500,
            byte_off: 0,
        };
        // chunks_needed = ceil(500 / 256_000) = 1, so total data = 256_000

        // Read a sub-range at offset 100, length 200
        let result = read_chunk_data(&spec, 100, 200, &context);
        assert!(result.is_ok(), "Partial read should succeed");
        assert_eq!(result.unwrap().len(), 200);
    }

    #[test]
    fn test_zero_length_returns_error() {
        let context = create_test_context_with_chunks(1);
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 100,
            byte_off: 0,
        };

        let result = read_chunk_data(&spec, 0, 0, &context);
        assert!(result.is_err(), "Zero length should fail");
        assert!(matches!(
            result.unwrap_err(),
            PdPrecompileError::ByteRangeOutOfBounds { .. }
        ));
    }

    #[test]
    fn test_range_exceeds_declared_window() {
        let context = create_test_context_with_chunks(1);
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 100,
            byte_off: 0,
        };
        // Declared window is [0, 100). Request [50, 200) exceeds the window.
        let result = read_chunk_data(&spec, 50, 150, &context);
        assert!(result.is_err(), "Out-of-window range should fail");
        assert!(matches!(
            result.unwrap_err(),
            PdPrecompileError::ByteRangeOutOfBounds { .. }
        ));
    }

    #[test]
    fn test_offset_before_byte_off_rejected() {
        let context = create_test_context_with_chunks(1);
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 50,
            byte_off: 100,
        };
        // Declared window is [100, 150). Request [0, 50) is before byte_off.
        let result = read_chunk_data(&spec, 0, 50, &context);
        assert!(result.is_err(), "Offset before byte_off should fail");
        assert!(matches!(
            result.unwrap_err(),
            PdPrecompileError::ByteRangeOutOfBounds { .. }
        ));
    }

    #[test]
    fn test_chunk_not_found() {
        let context = create_test_context_with_chunks(0); // No chunks in index
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 100,
            byte_off: 0,
        };

        let result = read_chunk_data(&spec, 0, 100, &context);
        assert!(result.is_err(), "Missing chunk should fail");
        assert!(matches!(
            result.unwrap_err(),
            PdPrecompileError::ChunkNotFound { .. }
        ));
    }

    #[test]
    fn test_multi_chunk_read() {
        let context = create_test_context_with_chunks(3);
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 512_000, // > 256_000, requires 2 chunks
            byte_off: 0,
        };
        // chunks_needed = ceil(512_000 / 256_000) = 2

        let result = read_chunk_data(&spec, 0, 512_000, &context);
        assert!(result.is_ok(), "Multi-chunk read should succeed");
        assert_eq!(result.unwrap().len(), 512_000);
    }

    #[test]
    fn test_short_last_chunk_does_not_panic() {
        // The last chunk of a data transaction can be shorter than chunk_size.
        // Ensure read_chunk_data returns an error instead of panicking.
        let chunk_config = ChunkConfig {
            num_chunks_in_partition: 100,
            chunk_size: 256_000,
            entropy_packing_iterations: 0,
            chain_id: 1,
        };
        let chunk_data_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());

        // Insert a full first chunk and a short second chunk (100 bytes)
        let full_chunk = Arc::new(bytes::Bytes::from(vec![0xAA_u8; 256_000]));
        let short_chunk = Arc::new(bytes::Bytes::from(vec![0xBB_u8; 100]));
        chunk_data_index.insert((0_u32, 0), full_chunk);
        chunk_data_index.insert((0_u32, 1), short_chunk);

        let context = PdContext::new(chunk_config, chunk_data_index);

        // Spec spans 2 chunks (byte_off=0, len=300_000 > 256_000)
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 300_000,
            byte_off: 0,
        };

        // Request bytes that extend into the short second chunk
        // Byte 256_000..300_000 lives in chunk 1, but chunk 1 is only 100 bytes.
        let result = read_chunk_data(&spec, 0, 300_000, &context);
        assert!(
            result.is_err(),
            "Should error (not panic) when chunk is shorter than expected"
        );
        assert!(matches!(
            result.unwrap_err(),
            PdPrecompileError::ChunkFetchFailed { .. }
        ));
    }
}
