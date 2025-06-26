use eyre::{ensure, Result};
use irys_types::{
    block_height_to_use_for_price, is_ema_recalculation_block,
    previous_ema_recalculation_block_height,
    storage_pricing::{phantoms::Percentage, Amount},
    ConsensusConfig, IrysBlockHeader, IrysTokenPrice,
};
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct EmaSnapshot {
    /// EMA price to use for pricing (from block 2 intervals ago)
    pub ema_for_pricing: IrysTokenPrice,

    /// The previous block's oracle price (for validation)
    pub oracle_price_parent_block: IrysTokenPrice,

    /// Oracle price of previous EMA recalculation blocks predecessor. Used of EMA calculations.
    /// example EMA calculation on block 29:
    /// 1. take the registered Oracle Irys price in block 18 (this value)
    ///    and the stored EMA Irys price in block 19.
    pub oracle_price_for_ema_predecessor: IrysTokenPrice,

    /// Latest EMA value calculated at this block
    /// example EMA calculation on block 29:
    /// 1. take the registered Oracle Irys price in block 18
    ///    and the stored EMA Irys price in block 19 (this value).
    pub ema_price_for_latest_ema_calculation_block: IrysTokenPrice,
}

#[derive(Debug)]
pub struct EmaBlock {
    pub range_adjusted_oracle_price: IrysTokenPrice,
    pub ema: IrysTokenPrice,
}

impl EmaSnapshot {
    /// Create EMA cache for genesis block
    pub fn genesis(consensus_config: &ConsensusConfig) -> Arc<Self> {
        Arc::new(Self {
            ema_for_pricing: consensus_config.genesis_price,
            oracle_price_parent_block: consensus_config.genesis_price,
            oracle_price_for_ema_predecessor: consensus_config.genesis_price,
            ema_price_for_latest_ema_calculation_block: consensus_config.genesis_price,
        })
    }

    /// Calculate EMA for a new block based on parent's cache
    pub fn calculate_ema_for_new_block(
        &self,
        parent_block: &IrysBlockHeader,
        oracle_price: IrysTokenPrice,
        safe_range: Amount<Percentage>,
        blocks_in_interval: u64,
    ) -> EmaBlock {
        let parent_snapshot = self;

        // Special handling for first 2 adjustment intervals.
        // the first 2 adjustment intervals have special handling where we calculate the
        // EMA for each block using the value from the preceding oracle price.
        //
        // But the generic case:
        // example EMA calculation on block 29:
        // 1. take the registered Oracle Irys price in block 18
        //    and the stored EMA Irys price in block 19.
        // 2. using these values compute EMA for block 29. In this case
        //    the *n* (number of block prices) would be 10 (E29.height - E19.height).
        // 3. this is the price that will be used in the interval 39->49,
        //    which will be reported to other systems querying for EMA prices.
        let oracle_price_to_use = if parent_block.height < (blocks_in_interval * 2) {
            oracle_price
        } else {
            // Use oracle price from the predecessor of the latest EMA block
            parent_snapshot.oracle_price_for_ema_predecessor
        };
        let oracle_price_to_use = bound_in_min_max_range(
            oracle_price_to_use,
            safe_range,
            parent_block.oracle_irys_price,
        );

        let ema = oracle_price_to_use
            .calculate_ema(
                blocks_in_interval,
                parent_snapshot.ema_price_for_latest_ema_calculation_block,
            )
            .unwrap_or_else(|err| {
                tracing::warn!(?err, "price overflow, using previous EMA price");
                parent_snapshot.ema_price_for_latest_ema_calculation_block
            });
        EmaBlock {
            range_adjusted_oracle_price: oracle_price_to_use,
            ema,
        }
    }

    /// Validate oracle price is within safe range
    pub fn validate_oracle_price(
        oracle_price: IrysTokenPrice,
        previous_oracle_price: IrysTokenPrice,
        safe_range: Amount<Percentage>,
    ) -> bool {
        let capped = bound_in_min_max_range(oracle_price, safe_range, previous_oracle_price);
        oracle_price == capped
    }
}

/// Create EMA cache for a block
pub fn create_ema_snapshot_for_block(
    new_block: &IrysBlockHeader,
    prev_ema_snapshot: &EmaSnapshot,
    parent_block: &IrysBlockHeader,
    consensus_config: &ConsensusConfig,
) -> eyre::Result<Arc<EmaSnapshot>> {
    let blocks_in_interval = consensus_config.ema.price_adjustment_interval;
    let safe_range = consensus_config.token_price_safe_range;
    let parent_snapshot = prev_ema_snapshot;
    
    // Validate that this is for the next block
    ensure!(
        parent_block.height + 1 == new_block.height,
        "new block must be parent's successor"
    );

    // Calculate new EMA if this is a recalculation block
    let ema_result = if is_ema_recalculation_block(new_block.height, blocks_in_interval) {
        // This is an EMA recalculation block - calculate new EMA
        parent_snapshot.calculate_ema_for_new_block(
            parent_block,
            new_block.oracle_irys_price,
            safe_range,
            blocks_in_interval,
        )
    } else {
        // Not a recalculation block - use existing EMA
        EmaBlock {
            range_adjusted_oracle_price: new_block.oracle_irys_price,
            ema: parent_snapshot.ema_price_for_latest_ema_calculation_block,
        }
    };

    // Create the new snapshot
    Ok(Arc::new(EmaSnapshot {
        ema_for_pricing: parent_snapshot.ema_price_for_latest_ema_calculation_block,
        oracle_price_parent_block: parent_block.oracle_irys_price,
        oracle_price_for_ema_predecessor: parent_snapshot.oracle_price_parent_block,
        ema_price_for_latest_ema_calculation_block: ema_result.ema,
    }))
}

/// Cap the provided price value to fit within the max / min acceptable range.
/// The range is defined by the `token_price_safe_range` percentile value.
///
/// Use the previous blocks oracle price as the base value.
#[tracing::instrument]
pub fn bound_in_min_max_range(
    desired_price: IrysTokenPrice,
    safe_range: Amount<Percentage>,
    base_price: IrysTokenPrice,
) -> IrysTokenPrice {
    let max_acceptable = base_price.add_multiplier(safe_range).unwrap_or(base_price);
    let min_acceptable = base_price.sub_multiplier(safe_range).unwrap_or(base_price);

    if desired_price > max_acceptable {
        tracing::warn!(
            ?max_acceptable,
            ?desired_price,
            "oracle price too high, capping"
        );
        return max_acceptable;
    }

    if desired_price < min_acceptable {
        tracing::warn!(
            ?min_acceptable,
            ?desired_price,
            "oracle price too low, capping"
        );
        return min_acceptable;
    }

    desired_price
}

/// Create EMA snapshot for a block using chain history
/// This is used for historical validation where we need to reconstruct the EMA state
pub fn create_ema_snapshot_from_chain_history(
    block: &IrysBlockHeader,
    chain_blocks: &[IrysBlockHeader],
    consensus_config: &ConsensusConfig,
) -> Result<Arc<EmaSnapshot>> {
    let blocks_in_interval = consensus_config.ema.price_adjustment_interval;

    // Find latest EMA block at or before this height
    let latest_ema_height = if is_ema_recalculation_block(block.height, blocks_in_interval) {
        block.height
    } else {
        previous_ema_recalculation_block_height(block.height, blocks_in_interval)
    };

    let latest_ema_predecessor_height = latest_ema_height.saturating_sub(1);

    // Find the blocks we need
    let latest_ema_block = chain_blocks
        .iter()
        .find(|b| b.height == latest_ema_height)
        .ok_or_else(|| eyre::eyre!("Latest EMA block not found in chain history"))?;

    let pricing_height = block_height_to_use_for_price(block.height, blocks_in_interval);
    let pricing_block = chain_blocks
        .iter()
        .find(|b| b.height == pricing_height)
        .ok_or_else(|| eyre::eyre!("Pricing block not found in chain history"))?;

    let prev_block = chain_blocks
        .iter()
        .find(|b| b.height == block.height.saturating_sub(1))
        .ok_or_else(|| eyre::eyre!("Previous block not found in chain history"))?;

    Ok(Arc::new(EmaSnapshot {
        ema_for_pricing: pricing_block.ema_irys_price,
        oracle_price_parent_block: prev_block.oracle_irys_price,
        oracle_price_for_ema_predecessor: chain_blocks
            .iter()
            .find(|b| b.height == latest_ema_predecessor_height)
            .ok_or_else(|| eyre::eyre!("Latest EMA predecessor block not found in chain history"))?
            .oracle_irys_price,
        ema_price_for_latest_ema_calculation_block: latest_ema_block.ema_irys_price,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::storage_pricing::Amount;
    use irys_types::{ConsensusConfig, EmaConfig};
    use rust_decimal_macros::dec;

    fn test_consensus_config() -> ConsensusConfig {
        ConsensusConfig {
            ema: EmaConfig {
                price_adjustment_interval: 10,
            },
            genesis_price: Amount::token(dec!(1.0)).unwrap(),
            token_price_safe_range: Amount::percentage(dec!(0.1)).unwrap(),
            ..ConsensusConfig::testnet()
        }
    }

    #[test]
    fn test_genesis_ema_snapshot() {
        let config = test_consensus_config();
        let cache = EmaSnapshot::genesis(&config);

        assert_eq!(cache.ema_for_pricing, config.genesis_price);
        assert_eq!(cache.oracle_price_parent_block, config.genesis_price);
        assert_eq!(cache.oracle_price_for_ema_predecessor, config.genesis_price);
        assert_eq!(cache.ema_price_for_latest_ema_calculation_block, config.genesis_price);
    }

    #[test]
    fn test_bound_in_min_max_range() {
        let base_price = Amount::token(dec!(1.0)).unwrap();
        let safe_range = Amount::percentage(dec!(0.1)).unwrap();

        // Price within range
        let desired = Amount::token(dec!(1.05)).unwrap();
        let result = bound_in_min_max_range(desired, safe_range, base_price);
        assert_eq!(result, desired);

        // Price too high
        let desired = Amount::token(dec!(1.15)).unwrap();
        let result = bound_in_min_max_range(desired, safe_range, base_price);
        assert_eq!(result, Amount::token(dec!(1.1)).unwrap());

        // Price too low
        let desired = Amount::token(dec!(0.85)).unwrap();
        let result = bound_in_min_max_range(desired, safe_range, base_price);
        assert_eq!(result, Amount::token(dec!(0.9)).unwrap());
    }

    #[test]
    fn test_validate_oracle_price() {
        let base_price = Amount::token(dec!(1.0)).unwrap();
        let safe_range = Amount::percentage(dec!(0.1)).unwrap();

        // Valid price
        let oracle_price = Amount::token(dec!(1.05)).unwrap();
        assert!(EmaSnapshot::validate_oracle_price(
            oracle_price,
            base_price,
            safe_range
        ));

        // Invalid price (too high)
        let oracle_price = Amount::token(dec!(1.15)).unwrap();
        assert!(!EmaSnapshot::validate_oracle_price(
            oracle_price,
            base_price,
            safe_range
        ));
    }
}
