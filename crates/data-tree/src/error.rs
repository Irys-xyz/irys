//! Failures for geometry, building, proofs, and verification.
//!
//! Verifiers return these instead of panicking. Length checks run before any
//! hashing (spec §9).

use thiserror::Error;

/// `Geometry::new` rejected a parameter set (spec §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum GeometryError {
    /// `segments_per_chunk` must be a power of two in `1..=MAX_SEGMENTS_PER_CHUNK`.
    #[error(
        "segments_per_chunk {segments_per_chunk} must be a power of two in 1..={max_segments_per_chunk}",
        max_segments_per_chunk = crate::geometry::MAX_SEGMENTS_PER_CHUNK
    )]
    SegmentsPerChunk {
        /// Rejected value.
        segments_per_chunk: u32,
    },
    /// `chunk_size` is 0.
    #[error("chunk_size must be greater than 0")]
    ChunkSizeZero,
    /// `chunk_size` is not divisible by `segments_per_chunk`.
    #[error("chunk_size {chunk_size} is not divisible by segments_per_chunk {segments_per_chunk}")]
    ChunkSizeNotMultiple {
        /// Rejected chunk size.
        chunk_size: u64,
        /// Rejected segments per chunk.
        segments_per_chunk: u32,
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
    #[error("data_size {data_size} is outside 1..={max_data_size}")]
    DataSizeOutOfRange {
        /// Rejected byte length (`n`).
        data_size: u64,
        /// Maximum byte length for this geometry.
        max_data_size: u64,
    },
    /// Chunk index `c` is not strictly below the chunk count.
    #[error("chunk index {chunk_index} is outside 0..{chunk_count}")]
    ChunkIndexOutOfRange {
        /// Rejected chunk index (`c`).
        chunk_index: u32,
        /// Chunk count for the data size.
        chunk_count: u32,
    },
    /// Segment index `j` is not strictly below `segments_per_chunk`.
    #[error("segment index {segment_index} is outside 0..{segments_per_chunk}")]
    SegmentIndexOutOfRange {
        /// Rejected segment index (`j`).
        segment_index: u32,
        /// Segments per chunk.
        segments_per_chunk: u32,
    },
}

/// `chunk_root` rejected the byte length (spec §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ChunkLengthError {
    /// A chunk must contain at least one byte.
    #[error("chunk is empty")]
    Empty,
    /// The slice is longer than `chunk_size`.
    #[error("chunk length {chunk_length} exceeds chunk_size {chunk_size}")]
    TooLong {
        /// Rejected length.
        chunk_length: u64,
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
    #[error("chunk count exceeds max_data_tx_chunks {max_data_tx_chunks}")]
    TooManyChunks {
        /// Configured maximum.
        max_data_tx_chunks: u32,
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
    /// The bytes hash to a chunk root whose chunk leaf at `c` is not the one
    /// stored at `c`.
    #[error("chunk bytes do not match the stored chunk leaf")]
    ChunkRootMismatch,
}

/// A chunk, segment, or size proof failed (spec §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum VerifyError {
    /// The data size or an index is outside the geometry.
    #[error(transparent)]
    Size(#[from] SizeError),
    /// `bytes` is not `chunk_len(data_size, c)`.
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
    /// The proof is not `32 × (depth(c, C) + 1)` bytes for a chunk, or
    /// `32 × (h + depth(c, C) + 1)` bytes for a segment (spec §5).
    ///
    /// The extra hash is the size witness. A length that is not a multiple of 32
    /// fails here, before any hashing (spec §5, §9).
    #[error("proof length {actual} does not match {expected}")]
    InvalidProofLength {
        /// Byte length required by `(data_size, c[, j])`.
        expected: usize,
        /// Byte length of the supplied proof.
        actual: usize,
    },
    /// The proof's last hash is not `size_witness(G, data_size)` (spec §5, §11 Q10).
    ///
    /// The verifier computes `size_witness(G, n)` from `G` and `n`.
    /// A supplied witness is not climbed until it matches that value.
    #[error("size witness does not match")]
    SizeWitnessMismatch,
    /// The proof does not open the anchored commitment (spec §5).
    ///
    /// A bad climb, a climbed root that does not match the commitment, and an
    /// in-chunk sibling that must be `empty(ℓ)` but is not, all fail here.
    #[error("commitment does not match")]
    CommitmentMismatch,
}
