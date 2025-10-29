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
    Config, IrysBlockHeader, ProgrammableDataConfig, U256,
};

pub fn compute_base_fee_per_chunk(
    config: &Config,
    parent_block: &IrysBlockHeader,
    parent_ema_snapshot: &EmaSnapshot,
    current_ema_price: &irys_types::IrysTokenPrice,
    parent_evm_block: &alloy_consensus::Block<
        alloy_consensus::EthereumTxEnvelope<alloy_consensus::TxEip4844>,
    >,
) -> eyre::Result<Amount<(CostPerChunk, Irys)>> {
    let current_pd_base_fee_irys = extract_pd_base_fee_from_block(
        parent_block,
        parent_evm_block,
        &config.consensus.programmable_data,
        config.consensus.chunk_size,
    )?;
    let total_pd_chunks = count_pd_chunks_in_block(parent_evm_block);
    let pd_base_fee = calculate_pd_base_fee_for_new_block(
        parent_ema_snapshot,
        current_ema_price,
        total_pd_chunks as u32,
        current_pd_base_fee_irys,
        &config.consensus.programmable_data,
        config.consensus.chunk_size,
    )?;
    Ok(pd_base_fee)
}

/// Calculate the new PD base fee for a new block based on utilization.
///
/// This function computes the PD (Programmable Data) base fee per chunk by:
/// 1. Converting from per-chunk Irys to per-chunk USD using the parent block's EMA price
/// 2. Adjusting the USD fee based on block utilization (target: 50%, range: ±12.5%)
///    - The floor is converted from per-MB to per-chunk for comparison
/// 3. Converting the adjusted per-chunk USD fee back to per-chunk Irys using the current block's EMA price
fn calculate_pd_base_fee_for_new_block(
    parent_block_ema_snapshot: &EmaSnapshot,
    current_block_ema_price: &irys_types::IrysTokenPrice,
    parent_chunks_used_in_block: u32,
    parent_pd_base_fee_irys: Amount<(CostPerChunk, Irys)>,
    pd_config: &ProgrammableDataConfig,
    chunk_size: u64,
) -> eyre::Result<Amount<(CostPerChunk, Irys)>> {
    const MB_SIZE: u64 = 1024 * 1024; // 1 MB in bytes
    let parent_ema_price = parent_block_ema_snapshot.ema_for_public_pricing();

    // Step 1: Convert per-chunk Irys → per-chunk USD (using parent EMA)
    // usd_per_chunk = irys_per_chunk * (price / PRECISION_SCALE)
    let current_fee_per_chunk_usd = Amount::new(mul_div(
        parent_pd_base_fee_irys.amount,
        parent_ema_price.amount,
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
        parent_chunks_used_in_block,
        pd_config.max_pd_chunks_per_block,
        floor_per_chunk_usd,
    )?;

    // Step 4: Convert per-chunk USD → per-chunk Irys (using current EMA)
    // irys_per_chunk = usd_per_chunk * (PRECISION_SCALE / price)
    let new_fee_per_chunk_irys = mul_div(
        new_fee_per_chunk_usd.amount,
        PRECISION_SCALE,
        current_block_ema_price.amount,
    )?;

    Ok(Amount::new(new_fee_per_chunk_irys))
}

/// Extract the current PD base fee from an EVM block's 2nd shadow transaction.
///
/// The PD base fee is stored in the PdBaseFeeUpdate transaction, which is always
/// the 2nd transaction in a block (after BlockReward at position 0).
fn extract_pd_base_fee_from_block(
    irys_block_header: &irys_types::IrysBlockHeader,
    evm_block: &reth_ethereum_primitives::Block,
    pd_config: &ProgrammableDataConfig,
    chunk_size: u64,
) -> eyre::Result<Amount<(CostPerChunk, Irys)>> {
    use eyre::{eyre, OptionExt as _};
    use irys_reth::shadow_tx::{detect_and_decode, TransactionPacket};

    // Special case: Genesis block (height 0) should use the minimum base fee
    // The genesis block doesn't have the standard transaction structure
    // TODO: instead of using the genesis block, we MUST hardcode the block height/timestamp in our hardfork config
    if irys_block_header.height == 0 {
        // Convert the floor from per-MB to per-chunk
        const MB_SIZE: u64 = 1024 * 1024;
        let floor_per_chunk_usd = Amount::new(mul_div(
            pd_config.base_fee_floor.amount,
            U256::from(chunk_size),
            U256::from(MB_SIZE),
        )?);

        // For genesis, we return the floor as the initial fee
        // This will be in USD, so ideally we'd convert to Irys, but since we don't have
        // an EMA price at genesis, we just return the floor value directly.
        // The caller will need to handle the USD->Irys conversion if needed.
        return Ok(floor_per_chunk_usd);
    }

    // Extract current PD base fee from block's 2nd shadow transaction (PdBaseFeeUpdate)
    // The ordering is: [0] BlockReward, [1] PdBaseFeeUpdate, [2+] other shadow txs
    let second_tx =
        evm_block.body.transactions.get(1).ok_or_eyre(
            "Block must have at least 2 transactions (BlockReward + PdBaseFeeUpdate)",
        )?;

    let shadow_tx = detect_and_decode(second_tx)
        .map_err(|e| eyre!("Failed to decode 2nd transaction as shadow tx: {}", e))?
        .ok_or_eyre("2nd transaction in block is not a shadow transaction")?;

    match &shadow_tx {
        irys_reth::shadow_tx::ShadowTransaction::V1 { packet, .. } => {
            match packet {
                TransactionPacket::PdBaseFeeUpdate(update) => {
                    // Convert alloy U256 to irys U256
                    Ok(Amount::new(update.per_chunk.into()))
                }
                _ => {
                    eyre::bail!(
                        "2nd transaction in block is not a PdBaseFeeUpdate (found: {:?})",
                        packet
                    );
                }
            }
        }
        _ => {
            eyre::bail!("Unsupported shadow transaction version in block's 2nd transaction");
        }
    }
}

/// Count the total number of PD chunks used in an EVM block.
///
/// This function iterates through all transactions in the block, detects PD transactions
/// by their header, and sums up the chunk counts from their access lists.
fn count_pd_chunks_in_block(evm_block: &reth_ethereum_primitives::Block) -> u64 {
    use alloy_consensus::Transaction as _;
    use irys_reth::pd_tx::{detect_and_decode_pd_header, sum_pd_chunks_in_access_list};

    let mut total_pd_chunks: u64 = 0;
    for tx in evm_block.body.transactions.iter() {
        // Try to detect PD header in transaction input
        let input = tx.input();
        if let Ok(Some(_header)) = detect_and_decode_pd_header(input) {
            // This is a PD transaction, sum chunks from access list if present
            if let Some(access_list) = tx.access_list() {
                let chunks = sum_pd_chunks_in_access_list(access_list);
                total_pd_chunks = total_pd_chunks.saturating_add(chunks);
            }
        }
    }

    total_pd_chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_domain::EmaSnapshot;
    use rust_decimal_macros::dec;

    

    /// Test correct price conversion when EMA prices change.
    ///
    /// When parent EMA price differs from current EMA price, the function should:
    /// 1. Convert parent fee from Irys to USD using parent EMA
    /// 2. Adjust USD fee based on utilization
    /// 3. Convert back to Irys using current EMA
    ///
    /// This test uses 50% utilization (no adjustment) to isolate the price conversion logic.
    ///
    /// At 50% utilization: new_fee_irys = parent_fee_irys × (parent_ema_price / current_ema_price)
    #[rstest::rstest]
    #[case(dec!(1.0), dec!(1.0), dec!(1.5))]  // No price change
    #[case(dec!(2.0), dec!(1.0), dec!(0.75))] // Current price doubles → need fewer tokens
    #[case(dec!(0.5), dec!(1.0), dec!(3.0))]  // Current price halves → need more tokens
    fn test_price_conversion_with_changing_ema(
        #[case] current_ema_price_decimal: rust_decimal::Decimal,
        #[case] parent_ema_price_decimal: rust_decimal::Decimal,
        #[case] expected_result_decimal: rust_decimal::Decimal,
    ) -> eyre::Result<()> {
        let chunk_size = 256 * 1024;
        let max_chunks = 100;
        let chunks_used = 50; // 50% utilization → no fee adjustment

        let parent_ema_price = Amount::token(parent_ema_price_decimal)?;
        let current_ema_price = Amount::token(current_ema_price_decimal)?;
        let price_unused_for_calc = Amount::token(dec!(99.0))?;

        let parent_ema_snapshot = EmaSnapshot {
            ema_price_2_intervals_ago: parent_ema_price,
            oracle_price_for_current_ema_predecessor: price_unused_for_calc,
            ema_price_current_interval: price_unused_for_calc,
            ema_price_1_interval_ago: price_unused_for_calc,
        };

        let parent_pd_base_fee_irys = Amount::token(dec!(1.5))?;

        let pd_config = ProgrammableDataConfig {
            cost_per_mb: Amount::token(dec!(0.001))?,
            base_fee_floor: Amount::token(dec!(0.001))?,
            max_pd_chunks_per_block: max_chunks,
        };

        let result = calculate_pd_base_fee_for_new_block(
            &parent_ema_snapshot,
            &current_ema_price,
            chunks_used,
            parent_pd_base_fee_irys,
            &pd_config,
            chunk_size,
        )?;

        let expected = Amount::token(expected_result_decimal)?;
        assert_eq!(result, expected);

        Ok(())
    }
}
