use eyre::Result;
use irys_types::{
    block_height_to_use_for_price, is_ema_recalculation_block,
    previous_ema_recalculation_block_height,
    storage_pricing::{phantoms::Percentage, Amount},
    ConsensusConfig, IrysBlockHeader, IrysTokenPrice,
};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct EmaSnapshot {
    /// Latest EMA value calculated at this block
    pub latest_ema: IrysTokenPrice,

    /// EMA price to use for pricing (from block 2 intervals ago)
    pub ema_for_pricing: IrysTokenPrice,

    /// Height of the latest EMA recalculation block in this chain path
    pub latest_ema_height: u64,

    /// Height of the predecessor to the latest EMA block
    pub latest_ema_predecessor_height: u64,

    /// The previous block's oracle price (for validation)
    pub previous_oracle_price: IrysTokenPrice,
}

impl EmaSnapshot {
    /// Create EMA cache for genesis block
    pub fn genesis(consensus_config: &ConsensusConfig) -> Arc<Self> {
        Arc::new(Self {
            latest_ema: consensus_config.genesis_price,
            ema_for_pricing: consensus_config.genesis_price,
            latest_ema_height: 0,
            latest_ema_predecessor_height: 0,
            previous_oracle_price: consensus_config.genesis_price,
        })
    }

    /// Calculate EMA for a new block based on parent's cache
    pub fn calculate_ema_for_new_block(
        parent_snapshot: &EmaSnapshot,
        parent_block: &IrysBlockHeader,
        oracle_price: IrysTokenPrice,
        blocks_in_interval: u64,
    ) -> IrysTokenPrice {
        // Special handling for first 2 adjustment intervals
        let oracle_price_to_use = if parent_block.height < (blocks_in_interval * 2) {
            oracle_price
        } else {
            // Use oracle price from the predecessor of the latest EMA block
            parent_snapshot.previous_oracle_price
        };

        oracle_price_to_use
            .calculate_ema(blocks_in_interval, parent_snapshot.latest_ema)
            .unwrap_or_else(|err| {
                tracing::warn!(?err, "price overflow, using previous EMA price");
                parent_snapshot.latest_ema
            })
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

    /// Validate EMA price calculation
    pub fn validate_ema_price(
        &self,
        ema_price: IrysTokenPrice,
        oracle_price: IrysTokenPrice,
        blocks_in_interval: u64,
    ) -> bool {
        // Recalculate EMA to verify
        let calculated_ema = oracle_price
            .calculate_ema(blocks_in_interval, self.latest_ema)
            .unwrap_or(self.latest_ema);

        ema_price == calculated_ema
    }
}

/// Create EMA cache for a block
pub fn create_ema_snapshot_for_block(
    block: &IrysBlockHeader,
    parent_snapshot: &EmaSnapshot,
    parent_block: &IrysBlockHeader,
    consensus_config: &ConsensusConfig,
) -> Arc<EmaSnapshot> {
    let blocks_in_interval = consensus_config.ema.price_adjustment_interval;
    let token_price_safe_range = consensus_config.token_price_safe_range;

    // Cap oracle price to safe range
    let capped_oracle_price = bound_in_min_max_range(
        block.oracle_irys_price,
        token_price_safe_range,
        parent_snapshot.previous_oracle_price,
    );

    // Calculate new EMA if this is a recalculation block
    let (new_ema, latest_ema_height, latest_ema_predecessor_height) =
        if is_ema_recalculation_block(block.height, blocks_in_interval) {
            // Calculate new EMA
            let new_ema = EmaSnapshot::calculate_ema_for_new_block(
                parent_snapshot,
                parent_block,
                capped_oracle_price,
                blocks_in_interval,
            );

            // Update heights
            (new_ema, block.height, block.height.saturating_sub(1))
        } else {
            // Keep existing EMA and heights
            (
                parent_snapshot.latest_ema,
                parent_snapshot.latest_ema_height,
                parent_snapshot.latest_ema_predecessor_height,
            )
        };

    // Determine EMA for pricing (from 2 intervals ago)
    let ema_for_pricing = if block.height < blocks_in_interval * 2 {
        // In first 2 intervals, use genesis price
        consensus_config.genesis_price
    } else {
        // Use EMA from 2 intervals ago
        let pricing_height = block_height_to_use_for_price(block.height, blocks_in_interval);

        // If the pricing height matches our latest EMA height, use that
        if pricing_height == latest_ema_height {
            new_ema
        } else {
            // Otherwise keep parent's pricing EMA
            parent_snapshot.ema_for_pricing
        }
    };

    Arc::new(EmaSnapshot {
        latest_ema: new_ema,
        ema_for_pricing,
        latest_ema_height,
        latest_ema_predecessor_height,
        previous_oracle_price: block.oracle_irys_price,
    })
}

/// Cap the provided price value to fit within the max / min acceptable range.
/// The range is defined by the `token_price_safe_range` percentile value.
///
/// Use the previous blocks oracle price as the base value.
#[tracing::instrument]
fn bound_in_min_max_range(
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
        latest_ema: latest_ema_block.ema_irys_price,
        ema_for_pricing: pricing_block.ema_irys_price,
        latest_ema_height,
        latest_ema_predecessor_height,
        previous_oracle_price: prev_block.oracle_irys_price,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::storage_pricing::Amount;
    use irys_types::{ConsensusConfig, ConsensusOptions, EmaConfig, NodeConfig};
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

        assert_eq!(cache.latest_ema, config.genesis_price);
        assert_eq!(cache.ema_for_pricing, config.genesis_price);
        assert_eq!(cache.latest_ema_height, 0);
        assert_eq!(cache.latest_ema_predecessor_height, 0);
        assert_eq!(cache.previous_oracle_price, config.genesis_price);
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
