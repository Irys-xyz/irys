//! Failures for geometry, building, proofs, and verification.
//!
//! Verifiers return these instead of panicking (spec §10).

use thiserror::Error;

/// `Geometry::new` rejected a parameter set (spec §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum GeometryError {
    /// `segments_per_chunk` is not a power of two in `1..=256`.
    #[error("segments_per_chunk {spc} is not a power of two in 1..=256")]
    SegmentsPerChunk {
        /// Rejected value.
        spc: u32,
    },
    /// `chunk_size` is 0.
    #[error("chunk_size must be greater than 0")]
    ChunkSizeZero,
    /// `chunk_size` is not divisible by `segments_per_chunk`.
    #[error("chunk_size {chunk_size} is not divisible by segments_per_chunk {spc}")]
    ChunkSizeNotMultiple {
        /// Rejected chunk size.
        chunk_size: u64,
        /// Rejected segments per chunk.
        spc: u32,
    },
    /// `max_data_tx_chunks` is 0.
    #[error("max_data_tx_chunks must be at least 1")]
    MaxChunksZero,
    /// `max_data_tx_chunks * chunk_size` does not fit in `u64`.
    #[error("max_data_tx_chunks * chunk_size does not fit in u64")]
    MaxDataSizeOverflow,
}

/// A length or index is outside the geometry (spec §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SizeError {
    /// `n` is 0 or above `max_data_tx_chunks * chunk_size`.
    #[error("data_size {n} is outside 1..={max}")]
    DataSizeOutOfRange {
        /// Rejected byte length.
        n: u64,
        /// Maximum byte length for this geometry.
        max: u64,
    },
    /// Chunk index `c` is not strictly below the chunk count.
    #[error("chunk index {c} is outside 0..{chunk_count}")]
    ChunkIndexOutOfRange {
        /// Rejected chunk index.
        c: u32,
        /// Chunk count for the data size.
        chunk_count: u32,
    },
    /// Segment index `j` is not strictly below `segments_per_chunk`.
    #[error("segment index {j} is outside 0..{spc}")]
    SegmentIndexOutOfRange {
        /// Rejected segment index.
        j: u32,
        /// Segments per chunk.
        spc: u32,
    },
}

/// `chunk_root` rejected the byte length (spec §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ChunkLengthError {
    /// A chunk must contain at least one byte.
    #[error("chunk is empty")]
    Empty,
    /// The slice is longer than `chunk_size`.
    #[error("chunk length {len} exceeds chunk_size {chunk_size}")]
    TooLong {
        /// Rejected length.
        len: u64,
        /// Geometry chunk size.
        chunk_size: u64,
    },
}

/// The streaming builder rejected a push or a finish (spec §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum BuildError {
    /// The chunk length is 0 or above `chunk_size`.
    #[error(transparent)]
    ChunkLength(#[from] ChunkLengthError),
    /// A short chunk is the last chunk. A later push is not a valid upload.
    #[error("push after a short chunk")]
    AfterShortChunk,
    /// The upload would exceed `max_data_tx_chunks`.
    #[error("chunk count exceeds max_data_tx_chunks {max}")]
    TooManyChunks {
        /// Configured maximum.
        max: u32,
    },
    /// `finish` with no chunks. `data_size` must be at least 1.
    #[error("data tree has no chunks")]
    Empty,
}

/// Proof generation rejected the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ProofError {
    /// The data size or an index is outside the tree.
    #[error(transparent)]
    Size(#[from] SizeError),
    /// The supplied chunk bytes do not have `chunk_len` for this index.
    #[error("chunk length {actual} does not match {expected}")]
    InvalidChunkLength {
        /// Length required by `(data_size, c)`.
        expected: u64,
        /// Length of the supplied slice.
        actual: u64,
    },
    /// The bytes hash to a different chunk root than the one stored at `c`.
    #[error("chunk bytes do not match the stored chunk root")]
    ChunkRootMismatch,
}

/// A chunk, segment, or size proof failed (spec §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum VerifyError {
    /// The data size or an index is outside the geometry.
    #[error(transparent)]
    Size(#[from] SizeError),
    /// `bytes` is not `chunk_len(data_size, c)`.
    ///
    /// L1 maps this to the peer fault `InvalidChunkSize`.
    #[error("chunk length {actual} does not match {expected}")]
    InvalidChunkLength {
        /// Length required by `(data_size, c)`.
        expected: u64,
        /// Length of the supplied slice.
        actual: u64,
    },
    /// `bytes` is not `segment_len(data_size, c, j)`.
    #[error("segment length {actual} does not match {expected}")]
    InvalidSegmentLength {
        /// Length required by `(data_size, c, j)`.
        expected: u64,
        /// Length of the supplied slice.
        actual: u64,
    },
    /// The proof is not `32 * k` bytes for the `k` the geometry requires.
    ///
    /// A length that is not a multiple of 32 fails here, before any hashing (spec §5, §10).
    #[error("proof length {actual} does not match {expected}")]
    InvalidProofLength {
        /// Byte length required by `(data_size, c[, j])`.
        expected: usize,
        /// Byte length of the supplied proof.
        actual: usize,
    },
    /// The climbed root and `size_witness(data_size)` are not the commitment.
    #[error("commitment does not match")]
    CommitmentMismatch,
}
