use eyre::Result;
use irys_types::{
    block_height_to_use_for_price, is_ema_recalculation_block,
    previous_ema_recalculation_block_height,
    storage_pricing::{phantoms::Percentage, Amount},
    ConsensusConfig, IrysBlockHeader, IrysTokenPrice,
};
use std::sync::Arc;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EmaSnapshot {
    /// EMA price to use for pricing (from block 2 intervals ago)
    pub ema_price_2_intervals_ago: IrysTokenPrice,

    /// The previous block's oracle price (for validation)
    pub oracle_price_parent_block: IrysTokenPrice,

    /// Oracle price of previous EMA recalculation blocks predecessor. Used of EMA calculations.
    /// example EMA calculation on block 29:
    /// 1. take the registered Oracle Irys price in block 18 (this value)
    ///    and the stored EMA Irys price in block 19.
    pub oracle_price_for_current_ema_predecessor: IrysTokenPrice,

    /// Latest EMA value calculated at this block
    /// example EMA calculation on block 29:
    /// 1. take the registered Oracle Irys price in block 18
    ///    and the stored EMA Irys price in block 19 (this value).
    pub ema_price_current_interval: IrysTokenPrice,

    /// EMA from the previous interval (1 interval ago)
    /// This is needed to properly update ema_price_2_intervals_ago when crossing pricing boundaries
    pub ema_price_1_interval_ago: IrysTokenPrice,
}

#[derive(Debug)]
pub struct EmaBlock {
    pub range_adjusted_oracle_price: IrysTokenPrice,
    pub ema: IrysTokenPrice,
}

impl EmaSnapshot {
    /// Create EMA cache for genesis block
    pub fn genesis(genesis_header: &IrysBlockHeader) -> Arc<Self> {
        Arc::new(Self {
            ema_price_2_intervals_ago: genesis_header.ema_irys_price,
            oracle_price_parent_block: genesis_header.oracle_irys_price,
            oracle_price_for_current_ema_predecessor: genesis_header.oracle_irys_price,
            ema_price_current_interval: genesis_header.ema_irys_price,
            ema_price_1_interval_ago: genesis_header.ema_irys_price,
        })
    }

    /// Create EMA cache for a block
    pub fn next_snapshot(
        &self,
        new_block: &IrysBlockHeader,
        parent_block: &IrysBlockHeader,
        consensus_config: &ConsensusConfig,
    ) -> eyre::Result<Arc<EmaSnapshot>> {
        let blocks_in_interval = consensus_config.ema.price_adjustment_interval;

        // Check if we're at an EMA boundary where we shift intervals
        // This happens at blocks 10, 20, 30, etc.
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
        if is_ema_recalculation_block(new_block.height, blocks_in_interval) {
            Ok(Arc::new(EmaSnapshot {
                ema_price_2_intervals_ago,
                ema_price_1_interval_ago,
                oracle_price_parent_block: parent_block.oracle_irys_price,
                oracle_price_for_current_ema_predecessor: parent_block.oracle_irys_price,
                ema_price_current_interval: new_block.ema_irys_price,
            }))
        } else {
            Ok(Arc::new(EmaSnapshot {
                ema_price_2_intervals_ago,
                ema_price_1_interval_ago,
                ema_price_current_interval: self.ema_price_current_interval,
                oracle_price_parent_block: parent_block.oracle_irys_price,
                oracle_price_for_current_ema_predecessor: self
                    .oracle_price_for_current_ema_predecessor,
            }))
        }
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
            parent_snapshot.oracle_price_for_current_ema_predecessor
        };
        let oracle_price_to_use = bound_in_min_max_range(
            oracle_price_to_use,
            safe_range,
            parent_block.oracle_irys_price,
        );

        let ema = oracle_price_to_use
            .calculate_ema(
                blocks_in_interval,
                parent_snapshot.ema_price_current_interval,
            )
            .unwrap_or_else(|err| {
                tracing::warn!(?err, "price overflow, using previous EMA price");
                parent_snapshot.ema_price_current_interval
            });
        EmaBlock {
            range_adjusted_oracle_price: oracle_price_to_use,
            ema,
        }
    }

    /// Validate oracle price is within safe range
    pub fn oracle_price_is_valid(
        oracle_price: IrysTokenPrice,
        previous_oracle_price: IrysTokenPrice,
        safe_range: Amount<Percentage>,
    ) -> bool {
        let capped = bound_in_min_max_range(oracle_price, safe_range, previous_oracle_price);
        oracle_price == capped
    }

    /// Get the EMA price that should be used for public pricing
    /// This is the EMA from 2 intervals ago
    pub fn ema_for_public_pricing(&self) -> IrysTokenPrice {
        self.ema_price_2_intervals_ago
    }
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
/// The blocks parameter should include all blocks up to and including the latest block
pub fn create_ema_snapshot_from_chain_history(
    blocks: &[IrysBlockHeader],
    consensus_config: &ConsensusConfig,
) -> Result<Arc<EmaSnapshot>> {
    let latest_block = blocks.last().ok_or_else(|| {
        eyre::eyre!("No blocks provided to create_ema_snapshot_from_chain_history")
    })?;

    let blocks_in_interval = consensus_config.ema.price_adjustment_interval;
    let latest_block_height = latest_block.height;

    let height_pricing_block =
        block_height_to_use_for_price(latest_block_height, blocks_in_interval);
    let height_latest_ema_block =
        if is_ema_recalculation_block(latest_block_height, blocks_in_interval) {
            latest_block_height
        } else {
            // Derive indexes
            previous_ema_recalculation_block_height(latest_block_height, blocks_in_interval)
        };
    let height_latest_ema_interval_predecessor = height_latest_ema_block.saturating_sub(1);
    let height_parent_block = latest_block_height.saturating_sub(1);

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
    let height_1_interval_ago = if latest_block_height < blocks_in_interval {
        // First interval (0-9) - no previous interval, use genesis
        0
    } else {
        // Get the last EMA block from the previous interval
        // For blocks 10-19: returns 9 (last EMA of interval 0)
        // For blocks 20-29: returns 19 (last EMA of interval 1)
        // For blocks 30-39: returns 29 (last EMA of interval 2)
        let current_interval = latest_block_height / blocks_in_interval;
        (current_interval - 1) * blocks_in_interval + (blocks_in_interval - 1)
    };

    // Calculate new EMA if this is a recalculation block
    Ok(Arc::new(EmaSnapshot {
        ema_price_2_intervals_ago: get_block_with_height(height_pricing_block)?.ema_irys_price,
        ema_price_1_interval_ago: get_block_with_height(height_1_interval_ago)?.ema_irys_price,
        oracle_price_parent_block: get_block_with_height(height_parent_block)?.oracle_irys_price,
        oracle_price_for_current_ema_predecessor: get_block_with_height(
            height_latest_ema_interval_predecessor,
        )?
        .oracle_irys_price,
        ema_price_current_interval: get_block_with_height(height_latest_ema_block)?.ema_irys_price,
    }))
}

#[cfg(test)]
mod snapshot_from_history {
    use super::*;
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

    /// Helper function to calculate EMA price for a given block height
    fn ema_price_for_height(height: u64) -> IrysTokenPrice {
        deterministic_price(height)
    }

    /// Utility function to build blocks with their corresponding snapshots
    fn build_blocks_with_snapshots(
        max_height: u64,
        config: &ConsensusConfig,
    ) -> Vec<(IrysBlockHeader, Arc<EmaSnapshot>)> {
        let mut current_snapshot: Arc<EmaSnapshot> = Arc::new(EmaSnapshot::default());
        let mut results: Vec<(IrysBlockHeader, Arc<EmaSnapshot>)> = Vec::new();

        for height in 0..=max_height {
            let mut block = IrysBlockHeader::new_mock_header();
            block.height = height;
            // Special handling for genesis block to use config.genesis_price
            if height == 0 {
                block.oracle_irys_price = config.genesis_price;
                block.ema_irys_price = config.genesis_price;
            } else {
                block.oracle_irys_price = oracle_price_for_height(height);
                block.ema_irys_price = ema_price_for_height(height);
            }

            // Create snapshot for this block
            if height == 0 {
                // Genesis block
                current_snapshot = EmaSnapshot::genesis(&block);
            } else {
                // Non-genesis blocks
                let parent_block = &results.last().unwrap().0;
                current_snapshot = current_snapshot
                    .next_snapshot(&block, parent_block, config)
                    .unwrap();
            }

            results.push((block, current_snapshot.clone()));
        }

        results
    }

    #[rstest]
    #[case(0, 0, 0, 0, 0)]
    #[case(1, 0, 1, 0, 0)]
    #[case(10, 0, 10, 9, 9)]
    #[case(18, 0, 18, 17, 9)]
    #[case(19, 0, 19, 18, 9)]
    #[case(20, 9, 19, 18, 19)]
    #[case(85, 69, 79, 78, 79)]
    #[case(90, 79, 89, 88, 89)]
    #[case(99, 79, 99, 98, 89)]
    #[case(5, 0, 5, 4, 0)]
    #[case(15, 0, 15, 14, 9)]
    #[case(25, 9, 19, 18, 19)]
    #[case(30, 19, 29, 28, 29)]
    #[case(35, 19, 29, 28, 29)]
    #[case(40, 29, 39, 38, 39)]
    #[case(45, 29, 39, 38, 39)]
    #[case(50, 39, 49, 48, 49)]
    #[case(100, 89, 99, 98, 99)]
    #[case(105, 89, 99, 98, 99)]
    #[case(110, 99, 109, 108, 109)]
    #[case(200, 189, 199, 198, 199)]
    fn test_valid_price_cache(
        #[case] height_latest_block: u64,
        #[case] height_for_pricing: u64,
        #[case] height_current_ema: u64,
        #[case] height_current_ema_predecessor: u64,
        #[case] height_1_interval_ago: u64,
    ) {
        // setup
        let interval = 10;
        let config = ConsensusConfig {
            ema: irys_types::EmaConfig {
                price_adjustment_interval: interval,
            },
            ..ConsensusConfig::testnet()
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
            oracle_price_parent_block: get_block(height_latest_block.saturating_sub(1))
                .oracle_irys_price,
            oracle_price_for_current_ema_predecessor: get_block(height_current_ema_predecessor)
                .oracle_irys_price,
            ema_price_current_interval: get_block(height_current_ema).ema_irys_price,
            ema_price_1_interval_ago: get_block(height_1_interval_ago).ema_irys_price,
        };
        assert_eq!(&expected_price_snapshot, ema_snapshot.as_ref());
    }

    #[rstest]
    #[case(1, 0)]
    #[case(2, 0)]
    #[case(9, 0)]
    #[case(19, 0)]
    #[case(20, 9)] // use the 10th block price during 3rd EMA interval
    #[case(29, 9)]
    #[case(30, 19)]
    fn get_ema_for_pricing(#[case] max_block_height: u64, #[case] price_block_height: u64) {
        // setup
        let config = ConsensusConfig {
            ema: EmaConfig {
                price_adjustment_interval: 10,
            },
            ..ConsensusConfig::testnet()
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
            ..ConsensusConfig::testnet()
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
            ..ConsensusConfig::testnet()
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
            ema_block.ema,
            expected_ema,
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
            ..ConsensusConfig::testnet()
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
                ema_block.range_adjusted_oracle_price, oracle_price,
                "Oracle price outside safe range should be capped"
            );

            // Verify the capped price is valid (within safe range)
            assert!(
                EmaSnapshot::oracle_price_is_valid(
                    ema_block.range_adjusted_oracle_price,
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
    #[case(28, 19, 18)]
    #[case(38, 29, 28)]
    #[case(168, 159, 158)]
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
            ..ConsensusConfig::testnet()
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
        let expected_oracle_price = oracle_price_for_height(prev_ema_predecessor_height);
        let expected_prev_ema = ema_price_for_height(prev_ema_height);
        let expected_ema = expected_oracle_price
            .calculate_ema(config.ema.price_adjustment_interval, expected_prev_ema)
            .unwrap();

        assert_eq!(
            ema_block.ema,
            expected_ema,
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
            ..ConsensusConfig::testnet()
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
