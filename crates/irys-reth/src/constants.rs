//! Shared constants for PD pricing and utilities.
use alloy_primitives::U256;

/// 1 MB in bytes.
pub const BYTES_PER_MB: u64 = 1024 * 1024;

/// Fixed-point scale 1e12 used for percentage math in PD base-rate step.
pub const FIXED_POINT_SCALE_1E12: u128 = 1_000_000_000_000;

/// Target utilization (50%) as a simple denominator (1/2).
pub const PD_TARGET_UTIL_DEN: u64 = 2;

/// Maximum adjustment per block (12.5%) as a simple denominator (1/8).
pub const PD_MAX_ADJ_DEN: u64 = 8;

/// Maximum PD chunks per block (temporary default; move to consensus).
pub const PD_MAX_CHUNKS_PER_BLOCK: u64 = 7_500;

/// 1e18 as an alloy U256 (token scaling factor).
/// Little-endian limb representation for 1e18: 0x0de0b6b3a7640000.
pub const ONE_TOKEN_SCALE_ALLOY: U256 = U256::from_limbs([0x0de0b6b3a7640000, 0, 0, 0]);

/// $0.01 in USD scaled by 1e18 as alloy U256.
/// 1e16 decimal equals 0x002386f26fc10000.
pub const USD_CENT_SCALED_ALLOY: U256 = U256::from_limbs([0x002386f26fc10000, 0, 0, 0]);
