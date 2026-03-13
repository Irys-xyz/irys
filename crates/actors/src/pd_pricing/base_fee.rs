use irys_domain::EmaSnapshot;
use irys_types::{
    Config, IrysBlockHeader, U256, UnixTimestamp,
    hardfork_config::Sprite,
    storage_pricing::{
        Amount, PRECISION_SCALE, mul_div,
        phantoms::{CostPerChunk, Irys, Percentage, Usd},
    },
};

const MB_SIZE: u64 = 1024 * 1024;
pub const PD_BASE_FEE_INDEX: usize = 1;

/// Compute the PD base fee per chunk for a new block.
///
/// Returns:
/// - `None` for pre-Sprite blocks (no PD fees)
/// - `Some(amount)` for Sprite blocks (floor for first block, computed for subsequent)
pub fn compute_pd_base_fee_for_block(
    config: &Config,
    parent_block: &IrysBlockHeader,
    parent_ema_snapshot: &EmaSnapshot,
    current_ema_price: &irys_types::IrysTokenPrice,
    parent_evm_block: &alloy_consensus::Block<
        alloy_consensus::EthereumTxEnvelope<alloy_consensus::TxEip4844>,
    >,
    block_timestamp: UnixTimestamp,
) -> eyre::Result<Option<Amount<(CostPerChunk, Irys)>>> {
    let Some(sprite) = config.consensus.hardforks.sprite_at(block_timestamp) else {
        // Pre-Sprite: no PD fees
        return Ok(None);
    };

    // Sprite is active - check if parent is pre-Sprite
    let parent_is_pre_sprite = parent_block.timestamp_secs() < sprite.activation_timestamp;
    if parent_is_pre_sprite {
        // First Sprite block: parent doesn't have PdBaseFeeUpdate, use floor
        return Ok(Some(compute_floor_base_fee_per_chunk(
            sprite,
            current_ema_price,
            config.consensus.chunk_size,
        )?));
    }

    // Normal case: compute from parent's utilization
    Ok(Some(compute_base_fee_per_chunk(
        config,
        parent_block,
        parent_ema_snapshot,
        current_ema_price,
        parent_evm_block,
        block_timestamp,
    )?))
}

/// Compute the floor PD base fee per chunk.
/// Used for the first block after Sprite activation when parent is pre-Sprite.
fn compute_floor_base_fee_per_chunk(
    sprite: &Sprite,
    current_ema_price: &irys_types::IrysTokenPrice,
    chunk_size: u64,
) -> eyre::Result<Amount<(CostPerChunk, Irys)>> {
    let floor_per_chunk_usd = Amount::new(mul_div(
        sprite.base_fee_floor.amount,
        U256::from(chunk_size),
        U256::from(MB_SIZE),
    )?);
    convert_per_chunk_usd_to_irys(floor_per_chunk_usd, current_ema_price)
}

/// Compute the PD base fee per chunk for a new block based on parent block utilization.
///
/// # Preconditions (caller must ensure):
/// - Sprite hardfork is active for `block_timestamp`
/// - Parent block is post-Sprite (has `PdBaseFeeUpdate` transaction)
///
/// For the first block after Sprite activation (where parent is pre-Sprite),
/// use `compute_floor_base_fee_per_chunk` instead.
pub fn compute_base_fee_per_chunk(
    config: &Config,
    parent_block: &IrysBlockHeader,
    parent_ema_snapshot: &EmaSnapshot,
    current_ema_price: &irys_types::IrysTokenPrice,
    parent_evm_block: &alloy_consensus::Block<
        alloy_consensus::EthereumTxEnvelope<alloy_consensus::TxEip4844>,
    >,
    block_timestamp: UnixTimestamp,
) -> eyre::Result<Amount<(CostPerChunk, Irys)>> {
    // Caller must ensure: Sprite active AND parent is post-Sprite
    let sprite = config
        .consensus
        .hardforks
        .sprite_at(block_timestamp)
        .expect("Sprite hardfork must be active - caller should check before calling");

    // Extract from parent (which must be post-Sprite and have PdBaseFeeUpdate)
    let current_pd_base_fee_irys = extract_pd_base_fee_from_block(
        parent_block,
        parent_evm_block,
        sprite,
        config.consensus.chunk_size,
    )?;
    let total_pd_chunks = count_pd_chunks_in_block(parent_evm_block);
    let pd_base_fee = calculate_pd_base_fee_for_new_block(
        parent_ema_snapshot,
        current_ema_price,
        total_pd_chunks as u32,
        current_pd_base_fee_irys,
        sprite,
        config.consensus.chunk_size,
    )?;
    Ok(pd_base_fee)
}

/// Calculate the new PD base fee for a new block based on utilization.
///
/// This function computes the PD (Programmable Data) base fee per chunk by:
/// 1. Converting from per-chunk Irys to per-chunk USD using the parent block's EMA price
/// 2. Adjusting the USD fee based on block utilization (target: 50%, range: +-12.5%)
///    - The floor is converted from per-MB to per-chunk for comparison
/// 3. Converting the adjusted per-chunk USD fee back to per-chunk Irys using the current block's EMA price
pub fn calculate_pd_base_fee_for_new_block(
    parent_block_ema_snapshot: &EmaSnapshot,
    current_block_ema_price: &irys_types::IrysTokenPrice,
    parent_chunks_used_in_block: u32,
    parent_pd_base_fee_irys: Amount<(CostPerChunk, Irys)>,
    sprite_config: &Sprite,
    chunk_size: u64,
) -> eyre::Result<Amount<(CostPerChunk, Irys)>> {
    let parent_ema_price = parent_block_ema_snapshot.ema_for_public_pricing();

    // Step 1: Convert per-chunk Irys -> per-chunk USD (using parent EMA)
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
        sprite_config.base_fee_floor.amount,
        U256::from(chunk_size),
        U256::from(MB_SIZE),
    )?);

    // Step 3: Adjust per-chunk USD fee based on utilization
    let new_fee_per_chunk_usd = pd_fee_adjustments::calculate_new_base_fee(
        current_fee_per_chunk_usd,
        parent_chunks_used_in_block,
        sprite_config.max_pd_chunks_per_block,
        floor_per_chunk_usd,
    )?;

    // Step 4: Convert per-chunk USD -> per-chunk Irys (using current EMA)
    let new_fee_per_chunk_irys = convert_per_chunk_usd_to_irys(
        Amount::new(new_fee_per_chunk_usd.amount),
        current_block_ema_price,
    )?;

    Ok(new_fee_per_chunk_irys)
}

/// Extract the current PD base fee from an EVM block's 2nd shadow transaction.
///
/// The PD base fee is stored in the PdBaseFeeUpdate transaction, which is always
/// the 2nd transaction in a block (after BlockReward at position 0).
///
/// # Panics
/// Callers must ensure this is only called for post-Sprite blocks that contain
/// the PdBaseFeeUpdate transaction. Use `compute_base_fee_per_chunk` which handles
/// pre-Sprite parent blocks correctly.
pub fn extract_pd_base_fee_from_block(
    irys_block_header: &irys_types::IrysBlockHeader,
    evm_block: &reth_ethereum_primitives::Block,
    sprite_config: &Sprite,
    chunk_size: u64,
) -> eyre::Result<Amount<(CostPerChunk, Irys)>> {
    use eyre::{OptionExt as _, eyre};
    use irys_reth::shadow_tx::{TransactionPacket, detect_and_decode};

    // Special case: Genesis block (height 0) should use the minimum base fee
    // The genesis block doesn't have the standard transaction structure
    if irys_block_header.height == 0 {
        // Convert the floor from per-MB to per-chunk
        let floor_per_chunk_usd = Amount::new(mul_div(
            sprite_config.base_fee_floor.amount,
            U256::from(chunk_size),
            U256::from(MB_SIZE),
        )?);

        // Convert per-chunk USD -> per-chunk Irys using genesis block's EMA price
        let floor_per_chunk_irys =
            convert_per_chunk_usd_to_irys(floor_per_chunk_usd, &irys_block_header.ema_irys_price)?;
        return Ok(floor_per_chunk_irys);
    }

    // Extract current PD base fee from block's shadow transactions.
    // Normal ordering: [0] BlockReward, [1] PdBaseFeeUpdate, [2+] other shadow txs
    // First Sprite block: [0] BlockReward, [1] TreasuryDeposit, [2] PdBaseFeeUpdate, [3+] other
    //
    // On the first Sprite block, TreasuryDeposit is at index 1, pushing PdBaseFeeUpdate to index 2.
    // We try index 1 first, and if it's TreasuryDeposit, we try index 2.
    let tx_at_index_1 = evm_block
        .body
        .transactions
        .get(PD_BASE_FEE_INDEX)
        .ok_or_eyre("Block must have at least 2 transactions (BlockReward + PdBaseFeeUpdate)")?;

    let shadow_tx_1 = detect_and_decode(tx_at_index_1)
        .map_err(|e| {
            eyre!(
                "Failed to decode transaction at index 1 as shadow tx: {}",
                e
            )
        })?
        .ok_or_eyre("Transaction at index 1 is not a shadow transaction")?;

    // Check if index 1 is PdBaseFeeUpdate (normal case) or TreasuryDeposit (first Sprite block)
    let pd_base_fee_update = match &shadow_tx_1 {
        irys_reth::shadow_tx::ShadowTransaction::V1 { packet, .. } => {
            match packet {
                TransactionPacket::PdBaseFeeUpdate(update) => {
                    // Normal case: PdBaseFeeUpdate is at index 1
                    Some(update.clone())
                }
                TransactionPacket::TreasuryDeposit(_) => {
                    // First Sprite block: TreasuryDeposit is at index 1, check index 2
                    let tx_at_index_2 = evm_block
                        .body
                        .transactions
                        .get(PD_BASE_FEE_INDEX + 1)
                        .ok_or_eyre("First Sprite block must have at least 3 transactions")?;

                    let shadow_tx_2 = detect_and_decode(tx_at_index_2)
                        .map_err(|e| {
                            eyre!(
                                "Failed to decode transaction at index 2 as shadow tx: {}",
                                e
                            )
                        })?
                        .ok_or_eyre("Transaction at index 2 is not a shadow transaction")?;

                    match &shadow_tx_2 {
                        irys_reth::shadow_tx::ShadowTransaction::V1 {
                            packet: TransactionPacket::PdBaseFeeUpdate(update),
                            ..
                        } => Some(update.clone()),
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    };

    let update = pd_base_fee_update.ok_or_eyre(
        "Could not find PdBaseFeeUpdate at expected index (1 or 2 if TreasuryDeposit present)",
    )?;

    // Convert alloy U256 to irys U256
    Ok(Amount::new(update.per_chunk.into()))
}

/// Count the total number of PD chunks used in an EVM block.
///
/// This function iterates through all transactions in the block, detects PD transactions
/// by their header, and sums up the chunk counts from their access lists.
pub fn count_pd_chunks_in_block(evm_block: &reth_ethereum_primitives::Block) -> u64 {
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

/// Extract priority fees from all PD transactions in an EVM block.
///
/// This function iterates through all transactions in the block, detects PD transactions
/// by their header, and collects their max_priority_fee_per_chunk values.
/// Returns a vector of priority fees as typed Amount values.
pub fn extract_priority_fees_from_block(
    evm_block: &reth_ethereum_primitives::Block,
) -> Vec<Amount<(CostPerChunk, Irys)>> {
    use alloy_consensus::Transaction as _;
    use irys_reth::pd_tx::detect_and_decode_pd_header;

    let mut priority_fees = Vec::new();
    for tx in evm_block.body.transactions.iter() {
        // Try to detect PD header in transaction input
        let input = tx.input();
        if let Ok(Some((header, _))) = detect_and_decode_pd_header(input) {
            // Convert alloy U256 to irys U256 and create typed Amount
            let priority_fee_irys =
                Amount::<(CostPerChunk, Irys)>::new(header.max_priority_fee_per_chunk.into());
            priority_fees.push(priority_fee_irys);
        }
    }

    priority_fees
}

/// Calculate PD utilization as a percentage using fixed-point arithmetic.
///
/// Returns utilization as `Amount<Percentage>` where PRECISION_SCALE = 100%.
pub fn calculate_utilization_percent(
    chunks_used: u64,
    max_chunks: u64,
) -> eyre::Result<Amount<Percentage>> {
    if max_chunks == 0 {
        return Ok(Amount::<Percentage>::new(U256::from(0)));
    }

    let percent_amount = mul_div(
        U256::from(chunks_used),
        PRECISION_SCALE,
        U256::from(max_chunks),
    )?;

    Ok(Amount::<Percentage>::new(percent_amount))
}

/// Convert a per-chunk USD amount to per-chunk Irys tokens using a given price.
///
/// Formula: irys_amount = usd_amount * (PRECISION_SCALE / price)
fn convert_per_chunk_usd_to_irys(
    usd_per_chunk: Amount<(CostPerChunk, Usd)>,
    irys_price: &irys_types::IrysTokenPrice,
) -> eyre::Result<Amount<(CostPerChunk, Irys)>> {
    let irys_amount = mul_div(usd_per_chunk.amount, PRECISION_SCALE, irys_price.amount)?;
    Ok(Amount::new(irys_amount))
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
    /// At 50% utilization: new_fee_irys = parent_fee_irys * (parent_ema_price / current_ema_price)
    #[rstest::rstest]
    #[case(dec!(1.0), dec!(1.0), dec!(1.5))] // No price change
    #[case(dec!(2.0), dec!(1.0), dec!(0.75))] // Current price doubles -> need fewer tokens
    #[case(dec!(0.5), dec!(1.0), dec!(3.0))] // Current price halves -> need more tokens
    fn test_price_conversion_with_changing_ema(
        #[case] current_ema_price_decimal: rust_decimal::Decimal,
        #[case] parent_ema_price_decimal: rust_decimal::Decimal,
        #[case] expected_result_decimal: rust_decimal::Decimal,
    ) -> eyre::Result<()> {
        let chunk_size = 256 * 1024;
        let max_chunks = 100;
        let chunks_used = 50; // 50% utilization -> no fee adjustment

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

        let sprite_config = Sprite {
            activation_timestamp: irys_types::UnixTimestamp::from_secs(0),
            cost_per_mb: Amount::token(dec!(0.001))?,
            base_fee_floor: Amount::token(dec!(0.001))?,
            max_pd_chunks_per_block: max_chunks,
            min_pd_transaction_cost: Amount::token(dec!(0.0))?,
        };

        let result = calculate_pd_base_fee_for_new_block(
            &parent_ema_snapshot,
            &current_ema_price,
            chunks_used,
            parent_pd_base_fee_irys,
            &sprite_config,
            chunk_size,
        )?;

        let expected = Amount::token(expected_result_decimal)?;
        assert_eq!(result, expected);

        Ok(())
    }
}

pub(crate) mod pd_fee_adjustments {
    use rust_decimal_macros::dec;

    use super::*;
    use irys_types::storage_pricing::{
        phantoms::{Percentage, Usd},
        safe_sub,
    };

    /// Calculate a new base fee for Programmable Data based on block utilization.
    ///
    /// The base fee adjusts linearly based on how much of the PD chunk budget was used:
    /// - At 50% utilization (target): no change
    /// - At 100% utilization: +12.5% adjustment
    /// - At 0% utilization: -12.5% adjustment
    pub(crate) fn calculate_new_base_fee(
        current_base_fee: Amount<Usd>,
        chunks_used_in_block: u32,
        max_pd_chunks_per_block: u64,
        base_fee_floor: Amount<Usd>,
    ) -> eyre::Result<Amount<Usd>> {
        // Protocol constants for base fee adjustment
        let max_adjustment = Amount::<Percentage>::percentage(dec!(0.125))?; // 12.5%
        let target_utilization = Amount::<Percentage>::percentage(dec!(0.5))?; // 50%

        // Calculate utilization as a ratio in PRECISION_SCALE
        let utilization = super::calculate_utilization_percent(
            chunks_used_in_block as u64,
            max_pd_chunks_per_block,
        )?
        .amount;

        // Calculate adjustment percentage based on utilization vs target
        let adjustment_pct = if utilization > target_utilization.amount {
            // Linear increase: 0% at 50%, +12.5% at 100%
            // delta = (utilization - target) / (100% - target)
            let numerator = safe_sub(utilization, target_utilization.amount)?;
            let denominator = safe_sub(PRECISION_SCALE, target_utilization.amount)?;
            let delta = mul_div(numerator, PRECISION_SCALE, denominator)?;

            // adjustment = delta * max_adjustment / PRECISION_SCALE
            Amount::new(mul_div(delta, max_adjustment.amount, PRECISION_SCALE)?)
        } else {
            // Linear decrease: 0% at 50%, -12.5% at 0%
            // delta = (target - utilization) / target
            let numerator = safe_sub(target_utilization.amount, utilization)?;
            let delta = mul_div(numerator, PRECISION_SCALE, target_utilization.amount)?;

            // adjustment = delta * max_adjustment / PRECISION_SCALE
            Amount::new(mul_div(delta, max_adjustment.amount, PRECISION_SCALE)?)
        };

        // Apply adjustment
        let new_fee = if utilization >= target_utilization.amount {
            current_base_fee.add_multiplier(adjustment_pct)?
        } else {
            current_base_fee.sub_multiplier(adjustment_pct)?
        };

        // Enforce floor
        let final_fee = if new_fee.amount < base_fee_floor.amount {
            base_fee_floor
        } else {
            new_fee
        };

        Ok(final_fee)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use eyre::Result;
        use rust_decimal::Decimal;
        use rust_decimal_macros::dec;

        /// Comprehensive parametrized test for PD base fee adjustment algorithm.
        #[rstest::rstest]
        // No change at 50% utilization (target)
        #[case(50, 100, dec!(1.00), dec!(1.00))]
        #[case(500, 1000, dec!(0.50), dec!(0.50))]
        #[case(3750, 7500, dec!(0.01), dec!(0.01))]
        // Fee converges to floor
        #[case(0, 100, dec!(0.011), dec!(0.01))] // -12.5% -> $0.009625 -> floor $0.01
        #[case(10, 100, dec!(0.0105), dec!(0.01))] // -10% -> $0.00945 -> floor $0.01
        #[case(0, 100, dec!(0.02), dec!(0.0175))] // -12.5% -> $0.0175 (above floor)
        // Fee increases linearly (>50% utilization)
        #[case(100, 100, dec!(1.00), dec!(1.125))] // 100% -> +12.5% (cap check)
        #[case(75, 100, dec!(1.00), dec!(1.0625))] // 75% -> +6.25%
        #[case(60, 100, dec!(1.00), dec!(1.025))] // 60% -> +2.5%
        #[case(90, 100, dec!(0.50), dec!(0.55))] // 90% -> +10%
        // Fee decreases linearly (<50% utilization)
        #[case(0, 100, dec!(1.00), dec!(0.875))] // 0% -> -12.5% (cap check)
        #[case(25, 100, dec!(1.00), dec!(0.9375))] // 25% -> -6.25%
        #[case(40, 100, dec!(1.00), dec!(0.975))] // 40% -> -2.5%
        #[case(10, 100, dec!(0.50), dec!(0.45))] // 10% -> -10%
        fn test_calculate_new_base_fee(
            #[case] chunks_used: u32,
            #[case] max_chunks_per_block: u64,
            #[case] current_fee_usd: Decimal,
            #[case] expected_fee_usd: Decimal,
        ) -> Result<()> {
            // Setup
            let current_base_fee = Amount::token(current_fee_usd)?;
            let sprite_config = Sprite {
                activation_timestamp: irys_types::UnixTimestamp::from_secs(0),
                cost_per_mb: Amount::token(dec!(0.01))?,
                base_fee_floor: Amount::token(dec!(0.01))?,
                max_pd_chunks_per_block: max_chunks_per_block,
                min_pd_transaction_cost: Amount::token(dec!(0.0))?,
            };

            // Action
            let new_fee = calculate_new_base_fee(
                current_base_fee,
                chunks_used,
                sprite_config.max_pd_chunks_per_block,
                sprite_config.base_fee_floor,
            )?;

            // Assert
            let actual = new_fee.token_to_decimal()?;
            let diff = (actual - expected_fee_usd).abs();

            assert!(
                diff < dec!(0.000001),
                "Fee mismatch for {}/{} chunks, current=${}: expected ${}, got ${} (diff: ${})",
                chunks_used,
                max_chunks_per_block,
                current_fee_usd,
                expected_fee_usd,
                actual,
                diff
            );

            Ok(())
        }
    }
}
