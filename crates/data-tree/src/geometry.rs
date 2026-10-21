//! Chunk and segment layout (spec §2).
//!
//! Lengths use checked arithmetic. `segment_len` returns 0 when the segment
//! starts at or past `data_size`, and it never subtracts past zero.

use crate::error::{GeometryError, SizeError};

/// Upper bound on `segments_per_chunk` (spec §2). A power of two, and at most this.
pub const MAX_SEGMENTS_PER_CHUNK: u32 = 256;

/// `log2(MAX_SEGMENTS_PER_CHUNK)`. A chunk subtree is never taller (spec §2).
pub const MAX_SUBTREE_HEIGHT: u32 = MAX_SEGMENTS_PER_CHUNK.trailing_zeros();

/// Mainnet and testnet chunk size, 256 KiB (spec §2).
const MAINNET_CHUNK_SIZE: u64 = 262_144;

/// Mainnet and testnet segments per chunk (spec §2).
const MAINNET_SEGMENTS_PER_CHUNK: u32 = 64;

/// `testing()` chunk size (spec §2). Not the hash width: a test chunk happens to
/// be 32 bytes as well.
const TESTING_CHUNK_SIZE: u64 = 32;

/// `testing()` segments per chunk (spec §2).
const TESTING_SEGMENTS_PER_CHUNK: u32 = 4;

/// Maximum chunks in one upload. `2^24 + 2^22`, 5 TiB at the mainnet chunk size (spec §2).
const MAX_DATA_TX_CHUNKS: u32 = 20_971_520;

/// Validated `(chunk_size, segments_per_chunk, max_data_tx_chunks)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    chunk_size: u64,
    segments_per_chunk: u32,
    max_data_tx_chunks: u32,
}

impl Geometry {
    /// Mainnet and testnet: 256 KiB chunks, 64 segments, 5 TiB maximum.
    pub const MAINNET: Self = Self::from_parts(
        MAINNET_CHUNK_SIZE,
        MAINNET_SEGMENTS_PER_CHUNK,
        MAX_DATA_TX_CHUNKS,
    );

    /// `testing()`: 32-byte chunks, 4 segments, the same maximum chunk count.
    pub const TESTING: Self = Self::from_parts(
        TESTING_CHUNK_SIZE,
        TESTING_SEGMENTS_PER_CHUNK,
        MAX_DATA_TX_CHUNKS,
    );

    /// Accept a geometry or say which rule failed (spec §2).
    ///
    /// `segments_per_chunk` may be 1. The lower bound of 2 is a config-layer
    /// choice (spec §14 Q9); this crate does not make it.
    pub const fn new(
        chunk_size: u64,
        segments_per_chunk: u32,
        max_data_tx_chunks: u32,
    ) -> Result<Self, GeometryError> {
        if segments_per_chunk > MAX_SEGMENTS_PER_CHUNK || !segments_per_chunk.is_power_of_two() {
            return Err(GeometryError::SegmentsPerChunk { segments_per_chunk });
        }
        if chunk_size == 0 {
            return Err(GeometryError::ChunkSizeZero);
        }
        if !chunk_size.is_multiple_of(segments_per_chunk as u64) {
            return Err(GeometryError::ChunkSizeNotMultiple {
                chunk_size,
                segments_per_chunk,
            });
        }
        if max_data_tx_chunks == 0 {
            return Err(GeometryError::MaxChunksZero);
        }
        if chunk_size.checked_mul(max_data_tx_chunks as u64).is_none() {
            return Err(GeometryError::MaxDataSizeOverflow);
        }
        Ok(Self::from_parts(
            chunk_size,
            segments_per_chunk,
            max_data_tx_chunks,
        ))
    }

    const fn from_parts(chunk_size: u64, segments_per_chunk: u32, max_data_tx_chunks: u32) -> Self {
        Self {
            chunk_size,
            segments_per_chunk,
            max_data_tx_chunks,
        }
    }

    /// Byte length of one full chunk.
    pub const fn chunk_size(&self) -> u64 {
        self.chunk_size
    }

    /// Leaves in one chunk subtree. A power of two in `1..=MAX_SEGMENTS_PER_CHUNK`.
    pub const fn segments_per_chunk(&self) -> u32 {
        self.segments_per_chunk
    }

    /// Maximum chunk count of one upload.
    pub const fn max_data_tx_chunks(&self) -> u32 {
        self.max_data_tx_chunks
    }

    /// `chunk_size / segments_per_chunk`.
    pub const fn segment_size(&self) -> u64 {
        self.chunk_size / (self.segments_per_chunk as u64)
    }

    /// `h = log2(segments_per_chunk)`, the height of a chunk subtree (spec §2).
    pub const fn height(&self) -> u32 {
        // `segments_per_chunk` is a power of two, so `trailing_zeros` is `log2`.
        self.segments_per_chunk.trailing_zeros()
    }

    /// `max_data_tx_chunks * chunk_size`. The product fits in `u64`.
    pub const fn max_data_size(&self) -> u64 {
        self.chunk_size * (self.max_data_tx_chunks as u64)
    }

    /// `C = ceil(n / chunk_size)` when `1 <= n <= max_data_size`.
    ///
    /// `n` is `data_size`.
    pub const fn chunk_count(&self, data_size: u64) -> Result<u32, SizeError> {
        let max_data_size = self.max_data_size();
        if data_size == 0 || data_size > max_data_size {
            return Err(SizeError::DataSizeOutOfRange {
                data_size,
                max_data_size,
            });
        }
        let count = data_size.div_ceil(self.chunk_size);
        // `TryFrom` is not a const trait yet. `count` fits `u32` whenever
        // `data_size` fits this geometry: `C <= max_data_tx_chunks`.
        if count > u32::MAX as u64 {
            return Err(SizeError::DataSizeOutOfRange {
                data_size,
                max_data_size,
            });
        }
        Ok(count as u32)
    }

    /// Number of data bytes in chunk `c` (spec §2 `chunk_len`): `chunk_size` for
    /// every chunk but the last, `1..=chunk_size` for the last. Not a count of
    /// segments. Requires `c < chunk_count(n)`.
    ///
    /// `n` is `data_size`. `c` is `chunk_index`.
    pub fn chunk_len(&self, data_size: u64, chunk_index: u32) -> Result<u64, SizeError> {
        let chunk_count = self.chunk_count(data_size)?;
        if chunk_index >= chunk_count {
            return Err(SizeError::ChunkIndexOutOfRange {
                chunk_index,
                chunk_count,
            });
        }
        // `c < C <= max_data_tx_chunks`. `Geometry::new` proved
        // `max_data_tx_chunks * chunk_size` fits `u64`, so this product does too.
        let start = (chunk_index as u64)
            .checked_mul(self.chunk_size)
            .expect("chunk_index < chunk_count, and max_data_tx_chunks * chunk_size fits u64");
        // `c < C` and `C = ceil(n / chunk_size)` imply `start < n`.
        let remain = data_size
            .checked_sub(start)
            .expect("chunk_index < chunk_count implies the chunk start is below data_size");
        Ok(if remain < self.chunk_size {
            remain
        } else {
            self.chunk_size
        })
    }

    /// Number of data bytes in segment `j` of chunk `c` (spec §2 `segment_len`):
    /// `segment_size` for a full segment, fewer for the segment holding the last
    /// data byte, 0 for a segment past the data. This is the exact length a
    /// segment hashes at; no padding is hashed (spec §3).
    ///
    /// Requires `c < chunk_count(n)` and `j < segments_per_chunk`. Returns 0 when
    /// the segment starts at or past `n` (spec §2). The subtraction runs only
    /// when the start is strictly below `n`.
    ///
    /// `n` is `data_size`. `c` is `chunk_index`. `j` is `segment_index`.
    pub fn segment_len(
        &self,
        data_size: u64,
        chunk_index: u32,
        segment_index: u32,
    ) -> Result<u64, SizeError> {
        let chunk_count = self.chunk_count(data_size)?;
        if chunk_index >= chunk_count {
            return Err(SizeError::ChunkIndexOutOfRange {
                chunk_index,
                chunk_count,
            });
        }
        if segment_index >= self.segments_per_chunk {
            return Err(SizeError::SegmentIndexOutOfRange {
                segment_index,
                segments_per_chunk: self.segments_per_chunk,
            });
        }
        let segment_size = self.segment_size();
        // `c < C <= max_data_tx_chunks` and `j < segments_per_chunk`. Both
        // products, and their sum, are at most `max_data_tx_chunks * chunk_size`,
        // which `Geometry::new` proved fits `u64`.
        let chunk_start = (chunk_index as u64)
            .checked_mul(self.chunk_size)
            .expect("chunk_index < chunk_count, and max_data_tx_chunks * chunk_size fits u64");
        let segment_offset = (segment_index as u64)
            .checked_mul(segment_size)
            .expect("segment_index < segments_per_chunk, so the offset is below chunk_size");
        let start = chunk_start
            .checked_add(segment_offset)
            .expect("chunk start plus segment offset is at most max_data_tx_chunks * chunk_size");
        if start >= data_size {
            return Ok(0);
        }
        // `start < data_size`, so this stays at or above zero.
        let remain = data_size
            .checked_sub(start)
            .expect("segment start is below data_size");
        Ok(if remain < segment_size {
            remain
        } else {
            segment_size
        })
    }
}
