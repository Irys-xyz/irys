//! Constants for the Programmable Data precompile.

/// Flat EVM gas cost charged for any PD precompile invocation.
///
/// This is intentionally a small, fixed cost. The real anti-DoS mechanism for PD chunk I/O
/// is the **per-chunk PD fee** deducted at EVM execution time in `IrysEvm::transact_raw()`
/// (see `crates/irys-reth/src/evm.rs`), not EVM gas metering. PD transactions pay
/// `(base_fee + priority_fee) × chunk_count` in IRYS tokens before execution begins,
/// and total chunks per block are capped by `MAX_PD_CHUNKS_PER_BLOCK`.
///
/// TODO: Benchmark to find optimal cost. Consider reporting non-zero `gas_used`
/// proportional to chunks for more accurate block gas accounting, even though
/// PD fees are the primary payment mechanism.
pub const PD_BASE_GAS_COST: u64 = 5000;

pub const FUNCTION_ID_SIZE: usize = 1;
pub const INDEX_SIZE: usize = 1;
pub const INDEX_OFFSET: usize = 1;

pub const READ_BYTES_RANGE_BY_INDEX_CALLDATA_LEN: usize = FUNCTION_ID_SIZE + INDEX_SIZE;

pub const OFFSET_SIZE: usize = 4;
pub const OFFSET_FIELD_OFFSET: usize = FUNCTION_ID_SIZE + INDEX_SIZE;
pub const LENGTH_SIZE: usize = 4;
pub const LENGTH_FIELD_OFFSET: usize = FUNCTION_ID_SIZE + INDEX_SIZE + OFFSET_SIZE;

pub const READ_PARTIAL_BYTE_RANGE_CALLDATA_LEN: usize =
    FUNCTION_ID_SIZE + INDEX_SIZE + OFFSET_SIZE + LENGTH_SIZE;
