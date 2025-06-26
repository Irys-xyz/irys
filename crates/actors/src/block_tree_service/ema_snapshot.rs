use eyre::{ensure, OptionExt, Result};
use irys_database::{block_header_by_hash, db::IrysDatabaseExt};
use irys_types::{
    block_height_to_use_for_price, is_ema_recalculation_block,
    previous_ema_recalculation_block_height,
    storage_pricing::{phantoms::Percentage, Amount},
    ConsensusConfig, DatabaseProvider, IrysBlockHeader, IrysTokenPrice,
};
use reth::rpc::server_types::eth::gas_oracle;
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct EmaSnapshot {
    /// EMA price to use for pricing (from block 2 intervals ago)
    pub ema_price_2_intervals_ago: IrysTokenPrice,

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
    pub ema_price_last_interval: IrysTokenPrice,
}

// pub struct EmaInfo {
//     pub ema_value: IrysTokenPrice,
//     pub parent_oracle_price: IrysTokenPrice,
// }

#[derive(Debug)]
pub struct EmaBlock {
    pub range_adjusted_oracle_price: IrysTokenPrice,
    pub ema: IrysTokenPrice,
}

impl EmaSnapshot {
    /// Create EMA cache for genesis block
    pub fn genesis(consensus_config: &ConsensusConfig) -> Arc<Self> {
        Arc::new(Self {
            ema_price_2_intervals_ago: consensus_config.genesis_price,
            oracle_price_parent_block: consensus_config.genesis_price,
            oracle_price_for_ema_predecessor: consensus_config.genesis_price,
            ema_price_last_interval: consensus_config.genesis_price,
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
            .calculate_ema(blocks_in_interval, parent_snapshot.ema_price_last_interval)
            .unwrap_or_else(|err| {
                tracing::warn!(?err, "price overflow, using previous EMA price");
                parent_snapshot.ema_price_last_interval
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
    parent_block: &IrysBlockHeader,
    parent_ema_snapshot: &EmaSnapshot,
    consensus_config: &ConsensusConfig,
) -> eyre::Result<Arc<EmaSnapshot>> {
    let blocks_in_interval = consensus_config.ema.price_adjustment_interval;

    if is_ema_recalculation_block(new_block.height, blocks_in_interval) {
        return Ok(Arc::new(EmaSnapshot {
            ema_price_2_intervals_ago: parent_ema_snapshot.ema_price_last_interval,
            oracle_price_parent_block: parent_block.oracle_irys_price,
            oracle_price_for_ema_predecessor: parent_block.oracle_irys_price,
            ema_price_last_interval: new_block.ema_irys_price,
        }));
    } else {
        return Ok(Arc::new(EmaSnapshot {
            ema_price_2_intervals_ago: parent_ema_snapshot.ema_price_2_intervals_ago,
            oracle_price_parent_block: parent_block.oracle_irys_price,
            oracle_price_for_ema_predecessor: parent_ema_snapshot.oracle_price_for_ema_predecessor,
            ema_price_last_interval: parent_ema_snapshot.ema_price_last_interval,
        }));
    };
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
    latest_block: &IrysBlockHeader,
    previous_blocks: &[IrysBlockHeader],
    consensus_config: &ConsensusConfig,
) -> Result<Arc<EmaSnapshot>> {
    let blocks_in_interval = consensus_config.ema.price_adjustment_interval;
    let latest_block_height = latest_block.height;
    let previous_blocks = [previous_blocks, &[latest_block.clone()]].concat();

    let height_pricing_block =
        block_height_to_use_for_price(latest_block_height, blocks_in_interval);
    let height_latest_ema_block =
        if is_ema_recalculation_block(latest_block_height, blocks_in_interval) {
            latest_block.height
        } else {
            // Derive indexes
            previous_ema_recalculation_block_height(latest_block_height, blocks_in_interval)
        };
    let height_latest_ema_interval_predecessor = height_latest_ema_block.saturating_sub(1);
    let height_parent_block = latest_block.height.saturating_sub(1);

    // utility to get the block with the desired height
    let get_block_with_height = |desired_height: u64| {
        let block = previous_blocks
            .iter()
            .find(|b| b.height == desired_height)
            .ok_or_else(|| eyre::eyre!("Pricing block not found in chain history"))?;
        Result::<_, eyre::Report>::Ok(block)
    };

    // Calculate new EMA if this is a recalculation block
    Ok(Arc::new(EmaSnapshot {
        ema_price_2_intervals_ago: get_block_with_height(height_pricing_block)?.ema_irys_price,
        oracle_price_parent_block: get_block_with_height(height_parent_block)?.oracle_irys_price,
        oracle_price_for_ema_predecessor: get_block_with_height(
            height_latest_ema_interval_predecessor,
        )?
        .oracle_irys_price,
        ema_price_last_interval: get_block_with_height(height_latest_ema_block)?.ema_irys_price,
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
