//! PD (Programmable Data) base fee calculation logic.
//!
//! This module provides functionality to calculate the PD base fee for new blocks
//! based on utilization and market conditions. The fee is calculated per chunk and
//! denominated in Irys tokens.

use crate::reth_service::pd_fee_adjustments;
use irys_domain::EmaSnapshot;
use irys_types::{
    storage_pricing::{
        mul_div,
        phantoms::{CostPerChunk, Irys},
        Amount, PRECISION_SCALE,
    },
    ProgrammableDataConfig, U256,
};

/// Calculate the new PD base fee for a new block based on utilization.
///
/// This function computes the PD (Programmable Data) base fee per chunk by:
/// 1. Converting from per-chunk Irys to per-chunk USD using the EMA price
/// 2. Adjusting the USD fee based on block utilization (target: 50%, range: ±12.5%)
///    - The floor is converted from per-MB to per-chunk for comparison
/// 3. Converting the adjusted per-chunk USD fee back to per-chunk Irys
///
/// # Notes
///
/// - Uses `prev_block_ema_snapshot.ema_for_public_pricing()` for stable price conversion
/// - This EMA is from 2 intervals ago, providing predictable pricing for users
/// - The fee adjustment algorithm targets 50% utilization with ±12.5% adjustments
///
/// # Errors
///
/// Returns an error if any arithmetic operations fail (overflow/underflow/division by zero)
pub fn calculate_pd_base_fee_for_new_block(
    prev_block_ema_snapshot: &EmaSnapshot,
    chunks_used_in_block: u32,
    current_pd_base_fee_irys: Amount<(CostPerChunk, Irys)>,
    pd_config: &ProgrammableDataConfig,
    chunk_size: u64,
) -> eyre::Result<Amount<(CostPerChunk, Irys)>> {
    const MB_SIZE: u64 = 1024 * 1024; // 1 MB in bytes
    let ema_price = prev_block_ema_snapshot.ema_for_public_pricing();

    // Step 1: Convert per-chunk Irys → per-chunk USD
    // usd_per_chunk = irys_per_chunk * (price / PRECISION_SCALE)
    let current_fee_per_chunk_usd = Amount::new(mul_div(
        current_pd_base_fee_irys.amount,
        ema_price.amount,
        PRECISION_SCALE,
    )?);

    // Step 2: Convert floor from per-MB to per-chunk for the adjustment algorithm
    // The config floor is in USD per-MB, but we're working with per-chunk values
    // floor_per_chunk = floor_per_mb * (chunk_size / MB_SIZE)
    let floor_per_chunk_usd = Amount::new(mul_div(
        pd_config.base_fee_floor.amount,
        U256::from(chunk_size),
        U256::from(MB_SIZE),
    )?);

    // Step 3: Adjust per-chunk USD fee based on utilization
    let new_fee_per_chunk_usd = pd_fee_adjustments::calculate_new_base_fee(
        current_fee_per_chunk_usd,
        chunks_used_in_block,
        pd_config.max_pd_chunks_per_block,
        floor_per_chunk_usd,
    )?;

    // Step 4: Convert per-chunk USD → per-chunk Irys
    // irys_per_chunk = usd_per_chunk * (PRECISION_SCALE / price)
    let new_fee_per_chunk_irys = mul_div(
        new_fee_per_chunk_usd.amount,
        PRECISION_SCALE,
        ema_price.amount,
    )?;

    Ok(Amount::new(new_fee_per_chunk_irys))
}

