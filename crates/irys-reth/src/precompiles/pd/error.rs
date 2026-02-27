//! Error types for the PD precompile.

use revm::precompile::PrecompileError;
use std::borrow::Cow;
use thiserror::Error;

/// Errors that can occur during PD precompile execution.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum PdPrecompileError {
    #[error("insufficient input data (expected at least {expected} bytes, got {actual})")]
    InsufficientInput { expected: usize, actual: usize },

    #[error("transaction missing required access list")]
    MissingAccessList,

    #[error("invalid calldata length for {function} (expected {expected}, got {actual})")]
    InvalidCalldataLength {
        function: &'static str,
        expected: usize,
        actual: usize,
    },

    #[error("byte range index {index} not found in access list (available: {available})")]
    ByteRangeNotFound { index: u8, available: usize },

    #[error("chunk range index {index} not found in access list (available: {available})")]
    ChunkRangeNotFound { index: u8, available: usize },

    #[error("invalid access list format: {reason}")]
    InvalidAccessList { reason: String },

    #[error("gas calculation overflow (base: {base}, operation: {operation})")]
    GasOverflow { base: u64, operation: u64 },

    #[error("failed to apply offset {offset} to byte range: {reason}")]
    OffsetTranslationFailed { offset: u32, reason: String },

    #[error("invalid length value {length}: {reason}")]
    InvalidLength { length: u32, reason: String },

    #[error("partition index {index} exceeds u64::MAX")]
    PartitionIndexTooLarge { index: String },

    #[error("chunk not found at ledger offset {offset}")]
    ChunkNotFound { offset: u64 },

    #[error("failed to fetch chunk at offset {offset}: {reason}")]
    ChunkFetchFailed { offset: u64, reason: String },

    #[error("requested byte range [{start}..{end}) exceeds available data (length: {available})")]
    ByteRangeOutOfBounds {
        start: usize,
        end: usize,
        available: usize,
    },

    #[error(
        "calculated offset out of range (usize overflow): chunk_offset={chunk_offset}, chunk_size={chunk_size}, byte_offset={byte_offset}"
    )]
    OffsetOutOfRange {
        chunk_offset: u16,
        chunk_size: u64,
        byte_offset: u64,
    },

    #[error("length {length} exceeds usize::MAX")]
    LengthOutOfRange { length: u64 },
}

impl From<PdPrecompileError> for PrecompileError {
    fn from(e: PdPrecompileError) -> Self {
        Self::Other(Cow::Owned(format!("PD precompile: {}", e)))
    }
}
