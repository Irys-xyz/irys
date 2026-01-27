//! Constants for the Programmable Data precompile.

/// TODO: Benchmark to find optimal cost.
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
