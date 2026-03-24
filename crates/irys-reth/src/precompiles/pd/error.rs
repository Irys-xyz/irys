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

    #[error("invalid access list format: {reason}")]
    InvalidAccessList { reason: String },

    #[error("gas calculation overflow (base: {base}, operation: {operation})")]
    GasOverflow { base: u64, operation: u64 },

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
        "calculated offset out of range (overflow): partition_index={partition_index}, start={start}, num_chunks_in_partition={num_chunks_in_partition}"
    )]
    OffsetOutOfRange {
        partition_index: u64,
        start: u32,
        num_chunks_in_partition: u64,
    },

    #[error("specifier index {index} not found (available: {available})")]
    SpecifierNotFound { index: u8, available: usize },
}

impl From<PdPrecompileError> for PrecompileError {
    fn from(e: PdPrecompileError) -> Self {
        Self::Other(Cow::Owned(format!("PD precompile: {}", e)))
    }
}

/// Convert a user-facing [`PdPrecompileError`] into a reverted [`PrecompileOutput`]
/// with ABI-encoded custom error data.
///
/// Returns `None` for internal errors (e.g. `GasOverflow`, `OffsetOutOfRange`,
/// `InvalidAccessList`) that should remain as `PrecompileError::Other`.
pub fn try_encode_as_revert(
    e: &PdPrecompileError,
    gas_used: u64,
) -> Option<revm::precompile::PrecompileOutput> {
    use alloy_primitives::U256;
    use alloy_sol_types::SolError as _;
    use revm::precompile::PrecompileOutput;

    use super::functions::{
        ByteRangeOutOfBounds, ChunkFetchFailed, ChunkNotFound, MissingAccessList, SpecifierNotFound,
    };

    let revert_data: Vec<u8> = match e {
        PdPrecompileError::MissingAccessList => MissingAccessList {}.abi_encode(),
        PdPrecompileError::SpecifierNotFound { index, available } => SpecifierNotFound {
            index: *index,
            available: U256::from(*available),
        }
        .abi_encode(),
        PdPrecompileError::ByteRangeOutOfBounds {
            start,
            end,
            available,
        } => ByteRangeOutOfBounds {
            start: U256::from(*start),
            end: U256::from(*end),
            available: U256::from(*available),
        }
        .abi_encode(),
        PdPrecompileError::ChunkNotFound { offset } => {
            ChunkNotFound { offset: *offset }.abi_encode()
        }
        PdPrecompileError::ChunkFetchFailed { offset, .. } => {
            ChunkFetchFailed { offset: *offset }.abi_encode()
        }
        // Internal errors stay as PrecompileError::Other
        _ => return None,
    };
    Some(PrecompileOutput::new_reverted(gas_used, revert_data.into()))
}
