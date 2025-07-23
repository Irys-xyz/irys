//! # EMA (Exponential Moving Average) Snapshot System
//!
//! ## Problem Being Solved
//!
//! **Core Challenge**: Storage fees are quoted in USD but paid in $IRYS tokens, creating a pricing stability problem:
//! - Token prices fluctuate constantly
//! - Users need predictable storage costs when submitting transactions
//! - Transactions submitted in one interval might be confirmed in the next
//! - If token price suddenly increases, pending transactions become underfunded and fail
//!
//! ## Solution: 2-Interval Lookahead Pricing
//!
//! The EMA system provides stable, predictable pricing by:
//! - **Using EMA from 2 intervals ago**: Miners and users know the exact token price that will be used
//! - **Publishing future prices in block headers**: Each interval's EMA is recorded for future use
//! - **Preventing transaction failures**: Even if submitted in interval N, transaction remains valid in interval N+1
//!
//! ## Behaviour
//!
//! ### Why 2 Intervals Ahead?
//! - **Interval 1**: Transaction is submitted (uses price from 2 intervals ago)
//! - **Interval 2**: Transaction might still be pending (same price still applies)
//! - **Price Stability**: Both miners and users can quote/pay using a known, fixed token price
//!
//! ### Special Handling for First 2 Intervals
//!
//! **Problem**: System needs historical data to calculate "2 intervals ago" pricing
//! **Solution**: Different behavior during bootstrap:
//!
//! - **Intervals 1-2 (blocks 0-19)**:
//!   - EMA recalculated EVERY block (not just interval boundaries)
//!   - Uses genesis price for all fee calculations
//!   - Builds up historical EMA data for future use
//!
//! - **Interval 3+ (blocks 20+)**:
//!   - Standard operation begins
//!   - EMA recalculated only at interval boundaries (blocks 29, 39, 49...)
//!   - Fee calculations use EMA from 2 intervals ago
//!
//! ## Example Flow (10-block intervals)
//!
//! - **Block 0-9**: Fees use genesis price, EMA calculated each block
//! - **Block 9**: Records EMA (E9) in header for future use
//! - **Block 10-19**: Still uses genesis price, EMA calculated each block
//! - **Block 19**: Records EMA (E19) in header
//! - **Block 20-29**: NOW uses E9 for fees (recorded 2 intervals ago)
//! - **Block 29**: Records EMA (E29) using oracle[18] + ema[19]
//! - **Block 30-39**: Uses E19 for fees
//! - **Block 40-49**: Uses E29 for fees

use eyre::{ensure, Result};
use irys_types::{
    storage_pricing::{phantoms::Percentage, Amount},
    ConsensusConfig, IrysBlockHeader, IrysTokenPrice,
};
use std::sync::Arc;

/// Snapshot of EMA-related pricing data for a specific block.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EmaSnapshot {
    /// EMA price to use for public pricing operations (from block 2 intervals ago).
    /// This is the "stable" price that external systems and users see.
    pub ema_price_2_intervals_ago: IrysTokenPrice,

    /// Oracle price of previous EMA recalculation block's predecessor.
    /// This is used for EMA calculations in the standard (3rd+) intervals.
    ///
    /// Example: At block 29 (EMA recalculation block), this contains oracle price from block 18
    pub oracle_price_for_current_ema_predecessor: IrysTokenPrice,

    /// Latest EMA value calculated at the most recent EMA recalculation block.
    /// This is the "current" EMA that will become the pricing EMA in 2 intervals.
    ///
    /// Example: At block 35, this contains the EMA calculated at block 29
    pub ema_price_current_interval: IrysTokenPrice,

    /// EMA from the previous interval (1 interval ago).
    /// This is needed to properly shift values when crossing interval boundaries.
    /// When we cross into a new interval, this becomes ema_price_2_intervals_ago.
    ///
    /// Note: this value is crucial only during the first 2 pricing intervals.
    /// After the first 2 pricing intervals, it contains the same value as `ema_price_current_interval`
    pub ema_price_1_interval_ago: IrysTokenPrice,
}

/// Result of EMA calculation for a new block.
#[derive(Debug)]
pub struct ExponentialMarketAvgCalculation {
    /// The oracle price that was used for EMA calculations.
    /// After the first 2 pricing intervals, this value will
    /// point to the oracle price of the preceding EMA recalculation block.
    ///
    /// eg. On block 25, this will blocks 18 oracle price (19 being the preceding EMA recalculation block)
    pub oracle_price_for_calculation: IrysTokenPrice,

    /// Oracle price after applying safe range bounds.
    /// If the original oracle price was outside the safe range, this will be capped.
    pub oracle_price_for_block_inclusion: IrysTokenPrice,

    /// The newly calculated EMA value for the new block.
    pub ema: IrysTokenPrice,
}

impl EmaSnapshot {
    /// Create EMA snapshot for the genesis block.
    ///
    /// The genesis block is special: all price fields are initialized with the same value
    /// from the genesis configuration. This provides the starting point for all future
    /// EMA calculations.
    pub fn genesis(genesis_header: &IrysBlockHeader) -> Arc<Self> {
        Arc::new(Self {
            ema_price_2_intervals_ago: genesis_header.ema_irys_price,
            oracle_price_for_current_ema_predecessor: genesis_header.oracle_irys_price,
            ema_price_current_interval: genesis_header.ema_irys_price,
            ema_price_1_interval_ago: genesis_header.ema_irys_price,
        })
    }

    /// Create the next EMA snapshot for a new block based on the current snapshot.
    pub fn next_snapshot(
        &self,
        new_block: &IrysBlockHeader,
        parent_block: &IrysBlockHeader,
        consensus_config: &ConsensusConfig,
    ) -> eyre::Result<Arc<Self>> {
        let blocks_in_interval = consensus_config.ema.price_adjustment_interval;

        // Check if we're at an EMA boundary where we shift intervals
        // This happens at blocks 10, 20, 30, etc. (assuming the price adj interval is 10 blocks)
        let crossing_interval_boundary =
            new_block.height % blocks_in_interval == 0 && new_block.height > 0;

        // Update the interval tracking
        let (ema_price_2_intervals_ago, ema_price_1_interval_ago) = if crossing_interval_boundary {
            // Shift the intervals:
            // - What was 1 interval ago becomes 2 intervals ago
            // - What was the last interval becomes 1 interval ago
            (
                self.ema_price_1_interval_ago,
                self.ema_price_current_interval,
            )
        } else {
            // Keep the same interval tracking
            (
                self.ema_price_2_intervals_ago,
                self.ema_price_1_interval_ago,
            )
        };

        // Update oracle_price_for_ema_predecessor and ema_price_last_interval when we hit an EMA recalculation block
        // During the first 2 intervals, every block is an EMA recalculation block
        if new_block.is_ema_recalculation_block(blocks_in_interval) {
            Ok(Arc::new(Self {
                ema_price_2_intervals_ago,
                ema_price_1_interval_ago,
                oracle_price_for_current_ema_predecessor: parent_block.oracle_irys_price,
                ema_price_current_interval: new_block.ema_irys_price,
            }))
        } else {
            Ok(Arc::new(Self {
                ema_price_2_intervals_ago,
                ema_price_1_interval_ago,
                ema_price_current_interval: self.ema_price_current_interval,
                oracle_price_for_current_ema_predecessor: self
                    .oracle_price_for_current_ema_predecessor,
            }))
        }
    }

    /// Calculate EMA for a new block based on the current snapshot.
    ///
    /// This method implements the core EMA calculation logic, which differs based on
    /// which interval we're in:
    ///
    /// ## First 2 Intervals (blocks 0-19)
    /// - Uses the new oracle price directly for EMA calculation
    /// - Each block's EMA is based on its immediate predecessor
    ///
    /// ## Standard Intervals (blocks 20+)
    /// - Uses oracle price from the predecessor of the previous EMA recalculation block
    /// - Example: Block 29 uses oracle price from block 18 (not block 28)
    ///
    /// ## Price Safety
    /// The oracle price is always bounded within a safe range to prevent manipulation.
    /// If the price change is too large, it's capped to the maximum allowed change.
    ///
    /// # Returns
    ///
    /// An `EmaBlock` containing the calculated EMA and the range-adjusted oracle price used
    pub fn calculate_ema_for_new_block(
        &self,
        parent_block: &IrysBlockHeader,
        oracle_price: IrysTokenPrice,
        safe_range: Amount<Percentage>,
        blocks_in_interval: u64,
    ) -> ExponentialMarketAvgCalculation {
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
        let oracle_price_for_calculation = if parent_block.height < (blocks_in_interval * 2) {
            oracle_price
        } else {
            // Use oracle price from the predecessor of the latest EMA block
            parent_snapshot.oracle_price_for_current_ema_predecessor
        };
        let oracle_price_for_calculation = bound_in_min_max_range(
            oracle_price_for_calculation,
            safe_range,
            parent_block.oracle_irys_price,
        );

        let ema = oracle_price_for_calculation
            .calculate_ema(
                blocks_in_interval,
                parent_snapshot.ema_price_current_interval,
            )
            .unwrap_or_else(|err| {
                tracing::warn!(?err, "price overflow, using previous EMA price");
                parent_snapshot.ema_price_current_interval
            });
        ExponentialMarketAvgCalculation {
            oracle_price_for_calculation,
            ema,
            oracle_price_for_block_inclusion: bound_in_min_max_range(
                oracle_price,
                safe_range,
                parent_block.oracle_irys_price,
            ),
        }
    }

    /// Validate that an oracle price is within the safe range.
    ///
    /// This prevents sudden price spikes or drops that could be used for manipulation.
    /// The safe range is typically ±n% from the previous oracle price, where n is configurable.
    pub fn oracle_price_is_valid(
        oracle_price: IrysTokenPrice,
        previous_oracle_price: IrysTokenPrice,
        safe_range: Amount<Percentage>,
    ) -> bool {
        let capped = bound_in_min_max_range(oracle_price, safe_range, previous_oracle_price);
        oracle_price == capped
    }

    /// Get the EMA price that should be used for public pricing.
    pub fn ema_for_public_pricing(&self) -> IrysTokenPrice {
        self.ema_price_2_intervals_ago
    }
}

/// Cap the provided price value to fit within the max/min acceptable range.
///
/// # Returns
///
/// The original price if within range, or the capped price at the range boundary
///
/// # Example
///
/// If base_price = $1.00 and safe_range = 10%:
/// - Allowed range: $0.90 to $1.10
/// - Input $1.15 → Returns $1.10 (capped at max)
/// - Input $0.85 → Returns $0.90 (capped at min)
/// - Input $1.05 → Returns $1.05 (within range)
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

/// Create EMA snapshot for a block using chain history.
///
/// The function identifies which blocks contain the relevant price data based on
/// the current block height and interval configuration.
///
/// # Example
///
/// For block 30 with 10-block intervals:
/// - Pricing uses block 19's EMA (2 intervals ago)
/// - Current EMA is from block 29 (last recalculation in this interval)
/// - 1 interval ago EMA from block 19 (end of previous interval)
pub fn create_ema_snapshot_from_chain_history(
    blocks: &[IrysBlockHeader],
    consensus_config: &ConsensusConfig,
) -> Result<Arc<EmaSnapshot>> {
    let latest_block = blocks.last().ok_or_else(|| {
        eyre::eyre!("No blocks provided to create_ema_snapshot_from_chain_history")
    })?;

    // Ensure the latest block has the highest height
    let max_height = blocks.iter().map(|b| b.height).max().unwrap_or(0);
    ensure!(
        latest_block.height == max_height,
        "Latest block (height {}) does not have the highest height in the provided blocks (max height: {})",
        latest_block.height,
        max_height
    );

    let blocks_in_interval = consensus_config.ema.price_adjustment_interval;

    let height_pricing_block = latest_block.block_height_to_use_for_price(blocks_in_interval);
    let height_latest_ema_block = if latest_block.is_ema_recalculation_block(blocks_in_interval) {
        latest_block.height
    } else {
        latest_block.previous_ema_recalculation_block_height(blocks_in_interval)
    };
    let height_latest_ema_interval_predecessor = height_latest_ema_block.saturating_sub(1);

    // utility to get the block with the desired height
    let get_block_with_height = |desired_height: u64| {
        let block = blocks
            .iter()
            .find(|b| b.height == desired_height)
            .ok_or_else(|| {
                eyre::eyre!(
                    "Block with height {} not found in chain history",
                    desired_height
                )
            })?;
        Result::<_, eyre::Report>::Ok(block)
    };

    // Calculate the height for 1 interval ago
    // This is the last EMA recalculation block from the previous interval
    let height_1_interval_ago = if latest_block.height < blocks_in_interval {
        // First interval (0-9) - no previous interval, use genesis
        0
    } else {
        // Get the last EMA block from the previous interval
        // For blocks 10-19: returns 9 (last EMA of interval 0)
        // For blocks 20-29: returns 19 (last EMA of interval 1)
        // For blocks 30-39: returns 29 (last EMA of interval 2)
        let current_interval = latest_block.height / blocks_in_interval;
        (current_interval - 1) * blocks_in_interval + (blocks_in_interval - 1)
    };

    // Calculate new EMA if this is a recalculation block
    Ok(Arc::new(EmaSnapshot {
        ema_price_2_intervals_ago: get_block_with_height(height_pricing_block)?.ema_irys_price,
        ema_price_1_interval_ago: get_block_with_height(height_1_interval_ago)?.ema_irys_price,
        oracle_price_for_current_ema_predecessor: get_block_with_height(
            height_latest_ema_interval_predecessor,
        )?
        .oracle_irys_price,
        ema_price_current_interval: get_block_with_height(height_latest_ema_block)?.ema_irys_price,
    }))
}

#[cfg(test)]
mod test {
    use super::*;
    use irys_types::is_ema_recalculation_block;
    use irys_types::{ConsensusConfig, EmaConfig};
    use rstest::rstest;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    /// Helper function to calculate deterministic price (matching test_utils::deterministic_price)
    fn deterministic_price(height: u64) -> IrysTokenPrice {
        use irys_types::storage_pricing::TOKEN_SCALE;
        let amount = TOKEN_SCALE + IrysTokenPrice::token(Decimal::from(height)).unwrap().amount;
        IrysTokenPrice::new(amount)
    }

    /// Helper function to calculate oracle price for a given block height
    fn oracle_price_for_height(height: u64) -> IrysTokenPrice {
        Amount::new(
            deterministic_price(height)
                .amount
                .saturating_add(421000.into()),
        )
    }

    /// Utility function to build blocks with their corresponding snapshots
    fn build_blocks_with_snapshots(
        max_height: u64,
        config: &ConsensusConfig,
    ) -> Vec<(IrysBlockHeader, Arc<EmaSnapshot>)> {
        let mut results: Vec<(IrysBlockHeader, Arc<EmaSnapshot>)> = Vec::new();

        for height in 0..=max_height {
            let mut block = IrysBlockHeader::new_mock_header();
            block.height = height;

            // Set oracle price for all blocks
            block.oracle_irys_price = if height == 0 {
                config.genesis_price
            } else {
                oracle_price_for_height(height)
            };

            // Calculate snapshot and EMA price based on block height
            let snapshot = if height == 0 {
                // Genesis block: use genesis price for EMA and create genesis snapshot
                block.ema_irys_price = config.genesis_price;
                EmaSnapshot::genesis(&block)
            } else {
                // Non-genesis blocks: calculate EMA and create next snapshot
                let (parent_block, parent_snapshot) = &results[height as usize - 1];

                // Calculate and set EMA price before creating snapshot
                block.ema_irys_price = parent_snapshot
                    .calculate_ema_for_new_block(
                        parent_block,
                        block.oracle_irys_price,
                        config.token_price_safe_range,
                        config.ema.price_adjustment_interval,
                    )
                    .ema;

                // Create next snapshot with the block that now has the correct EMA price
                parent_snapshot
                    .next_snapshot(&block, parent_block, config)
                    .unwrap()
            };

            // Store block and snapshot
            results.push((block, snapshot));
        }

        results
    }

    #[rstest]
    // 1st interval - EMA calculated every block
    #[case(0, 0, 0, 0, 0)]
    #[case(1, 0, 0, 1, 0)]
    #[case(5, 0, 0, 5, 4)]
    #[case(9, 0, 0, 9, 8)]
    // 2nd interval - still special handling
    #[case(10, 0, 9, 10, 9)]
    #[case(15, 0, 9, 15, 14)]
    #[case(18, 0, 9, 18, 17)]
    #[case(19, 0, 9, 19, 18)]
    // 3rd interval - standard operation begins
    #[case(20, 9, 19, 19, 18)]
    #[case(25, 9, 19, 19, 18)]
    #[case(30, 19, 29, 29, 28)]
    #[case(35, 19, 29, 29, 28)]
    #[case(40, 29, 39, 39, 38)]
    #[case(45, 29, 39, 39, 38)]
    #[case(50, 39, 49, 49, 48)]
    #[case(85, 69, 79, 79, 78)]
    #[case(90, 79, 89, 89, 88)]
    #[case(99, 79, 89, 99, 98)]
    #[case(100, 89, 99, 99, 98)]
    #[case(105, 89, 99, 99, 98)]
    #[case(110, 99, 109, 109, 108)]
    #[case(200, 189, 199, 199, 198)]
    fn test_valid_price_snapshots(
        #[case] height_latest_block: u64,
        #[case] height_for_pricing: u64,
        #[case] height_1_interval_ago: u64,
        #[case] height_current_ema: u64,
        #[case] height_current_ema_predecessor: u64,
    ) {
        // setup
        let interval = 10;
        let config = ConsensusConfig {
            ema: irys_types::EmaConfig {
                price_adjustment_interval: interval,
            },
            ..ConsensusConfig::testing()
        };

        // Build blocks and snapshots iteratively
        let blocks_and_snapshots = build_blocks_with_snapshots(height_latest_block, &config);

        // Extract blocks for chain history approach
        let blocks: Vec<IrysBlockHeader> = blocks_and_snapshots
            .iter()
            .map(|(block, _)| block.clone())
            .collect();

        // Get the iterative snapshot for the target height
        let (_, iterative_snapshot) = blocks_and_snapshots
            .last()
            .expect("Should have at least one block");

        // action - create snapshot from chain history
        let ema_snapshot = create_ema_snapshot_from_chain_history(&blocks, &config).unwrap();

        // assert - verify iterative and chain history approaches match
        assert_eq!(
            ema_snapshot.as_ref(),
            iterative_snapshot.as_ref(),
            "Iterative snapshot should match chain history snapshot at height {}",
            height_latest_block
        );

        // assert - verify expected values
        let get_block = |height: u64| blocks.iter().find(|x| x.height == height).unwrap();

        let expected_price_snapshot = EmaSnapshot {
            ema_price_2_intervals_ago: get_block(height_for_pricing).ema_irys_price,
            oracle_price_for_current_ema_predecessor: get_block(height_current_ema_predecessor)
                .oracle_irys_price,
            ema_price_current_interval: get_block(height_current_ema).ema_irys_price,
            ema_price_1_interval_ago: get_block(height_1_interval_ago).ema_irys_price,
        };
        assert_eq!(&expected_price_snapshot, ema_snapshot.as_ref());
    }

    #[rstest]
    #[case(1, 0)] // Block 1: Uses genesis (block 0) for pricing
    #[case(2, 0)] // Block 2: Still uses genesis
    #[case(9, 0)] // Block 9: Still uses genesis (end of 1st interval)
    #[case(19, 0)] // Block 19: Still uses genesis (end of 2nd interval)
    #[case(20, 9)] // Block 20: NOW uses block 9's EMA (2 intervals ago)
    #[case(29, 9)] // Block 29: Still uses block 9's EMA
    #[case(30, 19)] // Block 30: NOW uses block 19's EMA (2 intervals ago)
    fn get_ema_for_pricing(#[case] max_block_height: u64, #[case] price_block_height: u64) {
        // setup
        let config = ConsensusConfig {
            ema: EmaConfig {
                price_adjustment_interval: 10,
            },
            ..ConsensusConfig::testing()
        };

        // Build blocks and snapshots using utility function
        let blocks_and_snapshots = build_blocks_with_snapshots(max_block_height, &config);

        // Get the last snapshot
        let (_, current_snapshot) = blocks_and_snapshots
            .last()
            .expect("Should have at least one block");

        // Find the expected block for pricing
        let (expected_block, _) = blocks_and_snapshots
            .iter()
            .find(|(b, _)| b.height == price_block_height)
            .expect("Price block should exist in blocks array");

        assert_eq!(
            current_snapshot.ema_for_public_pricing(),
            expected_block.ema_irys_price,
            "Snapshot ema_for_public_pricing() should equal EMA price from block {}",
            price_block_height
        );
    }

    #[test]
    fn calculate_ema_for_new_block_first_block() {
        use rust_decimal_macros::dec;

        // Setup
        let config = ConsensusConfig {
            ema: EmaConfig {
                price_adjustment_interval: 10,
            },
            ..ConsensusConfig::testing()
        };

        // Create genesis block
        let mut genesis_block = IrysBlockHeader::new_mock_header();
        genesis_block.height = 0;
        genesis_block.oracle_irys_price = config.genesis_price;
        genesis_block.ema_irys_price = config.genesis_price;

        // Create genesis snapshot
        let genesis_snapshot = EmaSnapshot::genesis(&genesis_block);

        // New oracle price for block 1
        let new_oracle_price = Amount::token(dec!(1.01)).unwrap();

        // Calculate EMA for block 1
        let ema_block = genesis_snapshot.calculate_ema_for_new_block(
            &genesis_block,
            new_oracle_price,
            config.token_price_safe_range,
            config.ema.price_adjustment_interval,
        );

        // Assert the computed EMA matches expected value
        assert_eq!(
            ema_block.ema,
            Amount::token(dec!(1.0018181818181818181818)).unwrap(),
            "known first magic value when oracle price is 1.01"
        );
    }

    #[rstest]
    #[case(1)]
    #[case(5)]
    #[case(10)]
    #[case(15)]
    #[case(19)]
    fn calculate_ema_for_new_block_first_and_second_adjustment_period(#[case] max_height: u64) {
        // Setup
        let config = ConsensusConfig {
            ema: EmaConfig {
                price_adjustment_interval: 10,
            },
            ..ConsensusConfig::testing()
        };

        // Build blocks and snapshots using utility function
        let blocks_and_snapshots = build_blocks_with_snapshots(max_height, &config);

        // Get the last block and snapshot
        let (parent_block, current_snapshot) = blocks_and_snapshots
            .last()
            .expect("Should have at least one block");

        // Test calculating EMA for the next block
        let new_oracle_price = oracle_price_for_height(max_height + 1);

        let ema_block = current_snapshot.calculate_ema_for_new_block(
            parent_block,
            new_oracle_price,
            config.token_price_safe_range,
            config.ema.price_adjustment_interval,
        );

        // Verify the EMA was calculated using the current interval's EMA
        let expected_ema = new_oracle_price
            .calculate_ema(
                config.ema.price_adjustment_interval,
                current_snapshot.ema_price_current_interval,
            )
            .unwrap();

        assert_eq!(
            ema_block.ema, expected_ema,
            "EMA should be calculated using current interval's EMA for blocks in first two intervals"
        );
    }

    #[rstest]
    #[case(5)]
    #[case(8)]
    #[case(9)]
    #[case(20)]
    #[case(30)]
    #[case(15)]
    #[case(19)]
    #[case(20)]
    #[case(29)]
    #[case(28)]
    #[case(27)]
    fn oracle_price_gets_capped(#[case] max_height: u64) {
        // Setup
        let config = ConsensusConfig {
            ema: EmaConfig {
                price_adjustment_interval: 10,
            },
            token_price_safe_range: Amount::percentage(dec!(0.1)).unwrap(),
            ..ConsensusConfig::testing()
        };

        // Build blocks and snapshots using utility function
        let blocks_and_snapshots = build_blocks_with_snapshots(max_height, &config);

        // Get the last block and snapshot
        let (parent_block, current_snapshot) = blocks_and_snapshots
            .last()
            .expect("Should have at least one block");
        let price_oracle_latest = parent_block.oracle_irys_price;

        // Create prices outside the safe range (10.1% above and below)
        let mul_outside_of_range = Amount::percentage(dec!(0.101)).unwrap();
        let oracle_prices = [
            price_oracle_latest
                .add_multiplier(mul_outside_of_range)
                .unwrap(),
            price_oracle_latest
                .sub_multiplier(mul_outside_of_range)
                .unwrap(),
        ];

        // Test both prices (too high and too low)
        for oracle_price in oracle_prices {
            let ema_block = current_snapshot.calculate_ema_for_new_block(
                parent_block,
                oracle_price,
                config.token_price_safe_range,
                config.ema.price_adjustment_interval,
            );

            // Verify price was capped
            assert_ne!(
                ema_block.oracle_price_for_calculation, oracle_price,
                "Oracle price outside safe range should be capped"
            );

            // Verify the capped price is valid (within safe range)
            assert!(
                EmaSnapshot::oracle_price_is_valid(
                    ema_block.oracle_price_for_calculation,
                    parent_block.oracle_irys_price,
                    config.token_price_safe_range,
                ),
                "Capped oracle price should be within safe range"
            );

            // Verify the original price was invalid
            assert!(
                !EmaSnapshot::oracle_price_is_valid(
                    oracle_price,
                    parent_block.oracle_irys_price,
                    config.token_price_safe_range,
                ),
                "Original oracle price should be outside safe range"
            );
        }
    }

    #[rstest]
    #[case(28, 19, 18)] // Block 29 will use: oracle[18] + ema[19]
    #[case(38, 29, 28)] // Block 39 will use: oracle[28] + ema[29]
    #[case(168, 159, 158)] // Block 169 will use: oracle[158] + ema[159]
    fn nth_adjustment_period(
        #[case] max_height: u64,
        #[case] prev_ema_height: u64,
        #[case] prev_ema_predecessor_height: u64,
    ) {
        // Setup
        let config = ConsensusConfig {
            ema: EmaConfig {
                price_adjustment_interval: 10,
            },
            ..ConsensusConfig::testing()
        };

        // Build blocks and snapshots using utility function
        let blocks_and_snapshots = build_blocks_with_snapshots(max_height, &config);

        // Get the last block and snapshot
        let (parent_block, current_snapshot) = blocks_and_snapshots
            .last()
            .expect("Should have at least one block");

        // Calculate EMA for the next block (which should be an EMA recalculation block)
        let new_block_height = max_height + 1;
        let new_oracle_price = oracle_price_for_height(new_block_height);

        // Verify this is an EMA recalculation block
        assert!(
            is_ema_recalculation_block(new_block_height, config.ema.price_adjustment_interval),
            "Block {} should be an EMA recalculation block",
            new_block_height
        );

        let ema_block = current_snapshot.calculate_ema_for_new_block(
            parent_block,
            new_oracle_price,
            config.token_price_safe_range,
            config.ema.price_adjustment_interval,
        );

        // Calculate expected EMA using the formula:
        // oracle_price[prev_ema_predecessor_height].calculate_ema(interval, ema_price[prev_ema_height])
        // We need to get the actual EMA from the built blocks, not the deterministic function
        let expected_oracle_price = oracle_price_for_height(prev_ema_predecessor_height);
        let expected_prev_ema = blocks_and_snapshots[prev_ema_height as usize]
            .0
            .ema_irys_price;
        let expected_ema = expected_oracle_price
            .calculate_ema(config.ema.price_adjustment_interval, expected_prev_ema)
            .unwrap();

        assert_eq!(
            ema_block.ema, expected_ema,
            "EMA at block {} should be calculated using oracle price from block {} and EMA from block {}",
            new_block_height, prev_ema_predecessor_height, prev_ema_height
        );
    }

    #[rstest]
    #[case(1)]
    #[case(5)]
    #[case(10)]
    #[case(13)]
    #[case(19)]
    #[case(29)]
    #[case(30)]
    #[case(31)]
    #[case(49)]
    #[case(50)]
    #[case(51)]
    fn test_iterative_vs_history_consistency(#[case] max_block_height: u64) {
        // Setup
        let config = ConsensusConfig {
            ema: irys_types::EmaConfig {
                price_adjustment_interval: 10,
            },
            ..ConsensusConfig::testing()
        };

        // Build blocks and snapshots using utility function
        let blocks_and_snapshots = build_blocks_with_snapshots(max_block_height, &config);

        // Extract blocks and snapshots into separate vectors for testing
        let blocks: Vec<IrysBlockHeader> = blocks_and_snapshots
            .iter()
            .map(|(block, _)| block.clone())
            .collect();

        let snapshots: Vec<EmaSnapshot> = blocks_and_snapshots
            .iter()
            .map(|(_, snapshot)| {
                Arc::try_unwrap(snapshot.clone()).unwrap_or_else(|arc| (*arc).clone())
            })
            .collect();

        // Now verify that create_ema_snapshot_from_chain_history produces the same results
        for test_height in 1..=max_block_height {
            let test_blocks = &blocks[0..=test_height as usize];

            // Create snapshot using chain history
            let history_snapshot =
                create_ema_snapshot_from_chain_history(test_blocks, &config).unwrap();

            // Get the iterative snapshot for this height
            let iterative_snapshot = &snapshots[test_height as usize];

            // Assert all fields match
            assert_eq!(
                history_snapshot.as_ref(),
                iterative_snapshot,
                "ema_price_2_intervals_ago mismatch at height {}",
                test_height
            );
        }
    }
}
