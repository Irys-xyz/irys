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
    let chunks_needed = spec.chunks_needed(chunk_size);

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
        chunks_needed,
        ledger_start,
        "Fetching chunks from cache"
    );

    // Fetch and concatenate all chunks
    let mut bytes = Vec::with_capacity((chunks_needed * chunk_size) as usize);
    for i in 0..chunks_needed {
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

        bytes.extend(&*unpacked_chunk);
    }

    debug!(
        total_bytes_fetched = bytes.len(),
        chunks_needed, "Successfully fetched all chunks from cache"
    );

    // Validate length > 0
    if length == 0 {
        return Err(PdPrecompileError::ByteRangeOutOfBounds {
            start: offset as usize,
            end: offset as usize,
            available: bytes.len(),
        });
    }

    // Extract byte range
    let start = offset as usize;
    let end = start.checked_add(length as usize).ok_or_else(|| {
        warn!(start, length, "Byte range end overflow");
        PdPrecompileError::ByteRangeOutOfBounds {
            start,
            end: usize::MAX,
            available: bytes.len(),
        }
    })?;

    if end > bytes.len() {
        warn!(
            start,
            end,
            available = bytes.len(),
            "Requested byte range exceeds available data"
        );
        return Err(PdPrecompileError::ByteRangeOutOfBounds {
            start,
            end,
            available: bytes.len(),
        });
    }

    let extracted = Bytes::copy_from_slice(&bytes[start..end]);

    debug!(
        bytes_extracted = extracted.len(),
        "Byte range extraction successful"
    );

    Ok(extracted)
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
    fn test_range_exceeds_available_data() {
        let context = create_test_context_with_chunks(1);
        let spec = PdDataRead {
            partition_index: 0,
            start: 0,
            len: 100,
            byte_off: 0,
        };
        // chunks_needed = 1, total data = 256_000 bytes
        // Request offset 255_900, length 200 → end = 256_100 > 256_000
        let result = read_chunk_data(&spec, 255_900, 200, &context);
        assert!(result.is_err(), "Out-of-bounds range should fail");
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
}
