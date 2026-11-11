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
//! - **Block 29**: Records EMA (E29) using oracle\[18\] + ema\[19\]
//! - **Block 30-39**: Uses E19 for fees
//! - **Block 40-49**: Uses E29 for fees
//!
//! ## Rules from the Delta hardfork
//!
//! The rules for a block are resolved at that block's own timestamp
//! (`ConsensusConfig::ema_rules_at`). From Delta:
//!
//! - **Oracle price bound**: a block's oracle price may differ from its parent's by
//!   `Delta::token_price_safe_range` (±10%), not `token_price_safe_range` (±100%).
//! - **Interval length**: an interval is `Delta::ema_price_adjustment_interval`
//!   blocks (20), not `ema.price_adjustment_interval` (10). Interval boundaries stay
//!   keyed on the block height, so the first Delta interval ends at the next
//!   height that is a multiple of the new length.
//! - **Sampling**: after the two bootstrap intervals an EMA update takes the median
//!   of the oracle prices of every block in the interval, the recalculation block
//!   included. Before Delta it takes the oracle price of one earlier block (block
//!   18 for the update at block 29), so that block's producer decides the update.
//!   With the median, the producers of fewer than half of the interval's blocks
//!   cannot move the update outside the prices the other producers reported.

use eyre::{Result, ensure};
use irys_types::{
    ConsensusConfig, IrysBlockHeader, IrysTokenPrice,
    hardfork_config::{EmaRules, EmaSampling},
    storage_pricing::{Amount, phantoms::Percentage},
};
use std::sync::Arc;

/// Snapshot of EMA-related pricing data for a specific block.
///
/// Ema snapshot contains core expression of "how much USD is a single irys token worth".
/// Most of our data pricing is expressed in USD. So as the irys token goes up
/// in value compared to the dollar, we need less irys to cover the same usd fee.
/// The opposite is also true: if irys goes down in value then we need more of
/// it to cover the base fees.
#[derive(Debug, Clone, Default, PartialEq, Hash)]
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

    /// Oracle prices of the blocks of the current interval, oldest first: from the
    /// latest block that crossed an interval boundary (genesis before the first
    /// one) up to and including this snapshot's block.
    ///
    /// From Delta the EMA update takes the median of these prices. Tracked before
    /// Delta too, so the first Delta update has the prices of its whole interval.
    pub interval_oracle_prices: Vec<IrysTokenPrice>,
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
            interval_oracle_prices: vec![genesis_header.oracle_irys_price],
        })
    }

    /// Create the next EMA snapshot for a new block based on the current snapshot.
    pub fn next_snapshot(
        &self,
        new_block: &IrysBlockHeader,
        parent_block: &IrysBlockHeader,
        consensus_config: &ConsensusConfig,
    ) -> eyre::Result<Arc<Self>> {
        let rules = consensus_config.ema_rules_at(new_block.timestamp_secs());
        let blocks_in_interval = rules.price_adjustment_interval;

        // Check if we're at an EMA boundary where we shift intervals
        // This happens at blocks 10, 20, 30, etc. (assuming the price adj interval is 10 blocks)
        let crossing_interval_boundary =
            crosses_ema_interval_boundary(new_block.height, blocks_in_interval);

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

        // A block that crosses the boundary opens a new interval.
        let interval_oracle_prices = self.interval_oracle_prices_with(
            new_block.height,
            blocks_in_interval,
            new_block.oracle_irys_price,
        );

        // Update oracle_price_for_ema_predecessor and ema_price_last_interval when we hit an EMA recalculation block
        // During the first 2 intervals, every block is an EMA recalculation block
        if new_block.is_ema_recalculation_block(blocks_in_interval) {
            Ok(Arc::new(Self {
                ema_price_2_intervals_ago,
                ema_price_1_interval_ago,
                oracle_price_for_current_ema_predecessor: parent_block.oracle_irys_price,
                ema_price_current_interval: new_block.ema_irys_price,
                interval_oracle_prices,
            }))
        } else {
            Ok(Arc::new(Self {
                ema_price_2_intervals_ago,
                ema_price_1_interval_ago,
                ema_price_current_interval: self.ema_price_current_interval,
                oracle_price_for_current_ema_predecessor: self
                    .oracle_price_for_current_ema_predecessor,
                interval_oracle_prices,
            }))
        }
    }

    /// The oracle prices of the interval of the block at `new_block_height`, built
    /// on this (parent) snapshot, ending with that block's own `oracle_price`.
    fn interval_oracle_prices_with(
        &self,
        new_block_height: u64,
        blocks_in_interval: u64,
        oracle_price: IrysTokenPrice,
    ) -> Vec<IrysTokenPrice> {
        if crosses_ema_interval_boundary(new_block_height, blocks_in_interval) {
            return vec![oracle_price];
        }
        let mut prices = Vec::with_capacity(self.interval_oracle_prices.len() + 1);
        prices.extend_from_slice(&self.interval_oracle_prices);
        prices.push(oracle_price);
        prices
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
    /// - Before Delta ([`EmaSampling::PreviousIntervalBlock`]): uses the oracle price
    ///   from the predecessor of the previous EMA recalculation block.
    ///   Example: Block 29 uses oracle price from block 18 (not block 28)
    /// - From Delta ([`EmaSampling::IntervalMedian`]): uses the median of the oracle
    ///   prices of every block in the interval, the new block included.
    ///   Example (20-block intervals): Block 59 uses the median of blocks 40..=59
    ///
    /// ## Price Safety
    /// The oracle price is always bounded within a safe range to prevent manipulation.
    /// If the price change is too large, it's capped to the maximum allowed change.
    ///
    /// `rules` are the rules of the new block, resolved at its own timestamp
    /// (`ConsensusConfig::ema_rules_at`).
    ///
    /// # Returns
    ///
    /// An `EmaBlock` containing the calculated EMA and the range-adjusted oracle price used
    pub fn calculate_ema_for_new_block(
        &self,
        parent_block: &IrysBlockHeader,
        oracle_price: IrysTokenPrice,
        rules: &EmaRules,
    ) -> ExponentialMarketAvgCalculation {
        let parent_snapshot = self;
        let safe_range = rules.token_price_safe_range;
        let blocks_in_interval = rules.price_adjustment_interval;

        let oracle_price_for_block_inclusion =
            bound_in_min_max_range(oracle_price, safe_range, parent_block.oracle_irys_price);

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
        //
        // From Delta step 1 takes the median oracle price of the interval instead.
        let oracle_price_for_calculation =
            if parent_block.height < blocks_in_interval.saturating_mul(2) {
                oracle_price_for_block_inclusion
            } else {
                match rules.sampling {
                    // Use oracle price from the predecessor of the latest EMA block
                    EmaSampling::PreviousIntervalBlock => bound_in_min_max_range(
                        parent_snapshot.oracle_price_for_current_ema_predecessor,
                        safe_range,
                        parent_block.oracle_irys_price,
                    ),
                    // Every price in the interval already passed its own safe-range
                    // check, so the median needs no further bound.
                    EmaSampling::IntervalMedian => {
                        median_price(parent_snapshot.interval_oracle_prices_with(
                            parent_block.height.saturating_add(1),
                            blocks_in_interval,
                            oracle_price_for_block_inclusion,
                        ))
                        .unwrap_or(oracle_price_for_block_inclusion)
                    }
                }
            };

        let ema = oracle_price_for_calculation
            .calculate_ema(
                blocks_in_interval,
                parent_snapshot.ema_price_current_interval,
            )
            .unwrap_or_else(|err| {
                tracing::warn!(custom.error = ?err, "price overflow, using previous EMA price");
                parent_snapshot.ema_price_current_interval
            });
        ExponentialMarketAvgCalculation {
            oracle_price_for_calculation,
            ema,
            oracle_price_for_block_inclusion,
        }
    }

    /// Validate that an oracle price is positive and within the safe range.
    ///
    /// Zero (or non-positive) prices are always rejected: fee math divides by the
    /// token price, and a zero parent makes the relative safe-range band collapse
    /// to `{0}` so the chain cannot recover via normal oracle updates.
    ///
    /// The safe range then limits sudden spikes or drops vs the previous oracle
    /// price (typically ±n%, configurable).
    pub fn oracle_price_is_valid(
        oracle_price: IrysTokenPrice,
        previous_oracle_price: IrysTokenPrice,
        safe_range: Amount<Percentage>,
    ) -> bool {
        if oracle_price.amount.is_zero() {
            return false;
        }
        let capped = bound_in_min_max_range(oracle_price, safe_range, previous_oracle_price);
        oracle_price == capped
    }

    /// Get the EMA price that should be used for public pricing.
    pub fn ema_for_public_pricing(&self) -> IrysTokenPrice {
        self.ema_price_2_intervals_ago
    }

    /// Get the EMA price for a public fee quote made on top of the block that
    /// holds this snapshot (the tip at `tip_height`).
    ///
    /// Intent: a quote must stay valid if its tx is validated after the next
    /// interval roll. After the roll, public pricing uses the current
    /// `ema_price_1_interval_ago`. Near the roll the quote uses the lower of the
    /// two EMAs: a lower USD price gives a higher IRYS fee, which satisfies the
    /// requirement both before and after the roll.
    pub fn ema_for_public_quote(
        &self,
        tip_height: u64,
        price_adjustment_interval: u64,
    ) -> IrysTokenPrice {
        if tip_in_last_quarter_of_ema_interval(tip_height, price_adjustment_interval) {
            self.ema_price_1_interval_ago
                .min(self.ema_price_2_intervals_ago)
        } else {
            self.ema_for_public_pricing()
        }
    }

    // NON CANONICAL HASH
    // SHOULD BE USED FOR DEBUGGING ONLY
    pub fn get_hash(&self) -> String {
        use std::hash::{DefaultHasher, Hash as _, Hasher as _};
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        let res = hasher.finish();
        use irys_types::ToBase58 as _;
        res.to_le_bytes().to_base58()
    }
}

/// True when the block at `height` crosses an EMA interval boundary: it is the
/// first block of an interval, and the snapshot shifts its interval EMAs on it.
fn crosses_ema_interval_boundary(height: u64, blocks_in_interval: u64) -> bool {
    height > 0 && height.is_multiple_of(blocks_in_interval)
}

/// Median of `prices`, `None` when empty. For an even count it is the midpoint of
/// the two middle prices, rounded down.
fn median_price(mut prices: Vec<IrysTokenPrice>) -> Option<IrysTokenPrice> {
    prices.sort_unstable();
    let upper = *prices.get(prices.len() / 2)?;
    if prices.len() % 2 == 1 {
        return Some(upper);
    }
    let lower = prices[prices.len() / 2 - 1];
    // `lower <= upper` after the sort, so neither step can overflow or underflow.
    Some(IrysTokenPrice::new(
        lower.amount + (upper.amount - lower.amount) / 2,
    ))
}

/// True when the tip at `tip_height` is in the last quarter of its EMA interval.
///
/// The EMA snapshot rolls at the block whose height is a multiple of the
/// interval, so the window is keyed on the tip, not on the next block: the tip
/// just below a roll (`tip_height % interval == interval - 1`) is the most
/// exposed, and the window always contains it.
pub fn tip_in_last_quarter_of_ema_interval(
    tip_height: u64,
    price_adjustment_interval: u64,
) -> bool {
    let last_quarter_start = price_adjustment_interval - price_adjustment_interval.div_ceil(4);
    tip_height % price_adjustment_interval >= last_quarter_start
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
#[tracing::instrument(level = "trace", skip_all, fields(
    price.desired = ?desired_price,
    price.base = ?base_price,
    price.safe_range = ?safe_range
))]
pub fn bound_in_min_max_range(
    desired_price: IrysTokenPrice,
    safe_range: Amount<Percentage>,
    base_price: IrysTokenPrice,
) -> IrysTokenPrice {
    let max_acceptable = base_price.add_multiplier(safe_range).unwrap_or(base_price);
    let min_acceptable = base_price.sub_multiplier(safe_range).unwrap_or(base_price);

    if desired_price > max_acceptable {
        tracing::warn!(
            price.max_acceptable = ?max_acceptable,
            price.desired_price = ?desired_price,
            "oracle price too high, capping"
        );
        return max_acceptable;
    }

    if desired_price < min_acceptable {
        tracing::warn!(
            price.min_acceptable = ?min_acceptable,
            price.desired_price = ?desired_price,
            "oracle price too low, capping"
        );
        return min_acceptable;
    }

    desired_price
}

/// Create EMA snapshot for a block using chain history.
///
/// `blocks` are consecutive ancestors, oldest first, ending with the block the
/// snapshot is for. The result equals the snapshot that [`EmaSnapshot::next_snapshot`]
/// builds block by block from genesis.
///
/// Each block decides with its own rules (`ConsensusConfig::ema_rules_at`) whether
/// it crosses an interval boundary and whether it recalculates the EMA, so the
/// walk stays correct across the Delta change of the interval length.
/// [`ConsensusConfig::ema_snapshot_history_depth`] blocks are always enough.
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
    let oldest_block = blocks.first().ok_or_else(|| {
        eyre::eyre!("No blocks provided to create_ema_snapshot_from_chain_history")
    })?;
    ensure!(
        blocks
            .windows(2)
            .all(|pair| pair[0].height.checked_add(1) == Some(pair[1].height)),
        "Blocks provided to create_ema_snapshot_from_chain_history must have consecutive ascending heights"
    );
    let reaches_genesis = oldest_block.height == 0;
    let tip_idx = blocks.len() - 1;

    let interval_of = |block: &IrysBlockHeader| {
        consensus_config
            .ema_rules_at(block.timestamp_secs())
            .price_adjustment_interval
    };
    // The latest block at or below `from_idx` that matches, as an index.
    let latest_at_or_below = |from_idx: usize, matches: &dyn Fn(&IrysBlockHeader) -> bool| {
        (0..=from_idx).rev().find(|&idx| matches(&blocks[idx]))
    };
    let crosses_boundary =
        |block: &IrysBlockHeader| crosses_ema_interval_boundary(block.height, interval_of(block));
    let recalculates_ema =
        |block: &IrysBlockHeader| block.is_ema_recalculation_block(interval_of(block));
    let not_enough_history = |what: &str| {
        eyre::eyre!(
            "Chain history starting at height {} is too short to find {what}",
            oldest_block.height
        )
    };

    // The EMA that the snapshot of the block at `from_idx` holds as its current
    // one: the EMA of the latest recalculation block at or below it. Genesis is a
    // recalculation block, so a history that reaches genesis always has one.
    let current_ema_idx = |from_idx: usize| {
        latest_at_or_below(from_idx, &recalculates_ema)
            .ok_or_else(|| not_enough_history("the latest EMA recalculation block"))
    };
    // The interval EMA that the block at `boundary_idx` shifted in when it crossed
    // the boundary: the current EMA of its parent.
    let ema_shifted_in_at = |boundary_idx: usize| -> Result<IrysTokenPrice> {
        let parent_idx = boundary_idx
            .checked_sub(1)
            .ok_or_else(|| not_enough_history("the parent of an interval boundary block"))?;
        Ok(blocks[current_ema_idx(parent_idx)?].ema_irys_price)
    };
    // The latest boundary crossing at or below `from_idx`. `None` when there was
    // none since genesis, which only a history that reaches genesis can tell.
    let latest_boundary_idx = |from_idx: usize| -> Result<Option<usize>> {
        match latest_at_or_below(from_idx, &crosses_boundary) {
            Some(idx) => Ok(Some(idx)),
            None if reaches_genesis => Ok(None),
            None => Err(not_enough_history("an interval boundary block")),
        }
    };

    let latest_ema_idx = current_ema_idx(tip_idx)?;
    let latest_ema_block = &blocks[latest_ema_idx];
    // Genesis has no parent: its snapshot holds its own oracle price.
    let oracle_price_for_current_ema_predecessor = if latest_ema_block.height == 0 {
        latest_ema_block.oracle_irys_price
    } else {
        let parent_idx = latest_ema_idx.checked_sub(1).ok_or_else(|| {
            not_enough_history("the parent of the latest EMA recalculation block")
        })?;
        blocks[parent_idx].oracle_irys_price
    };

    // Before any boundary crossing the snapshot still holds the genesis EMA.
    let genesis_ema = oldest_block.ema_irys_price;
    let latest_boundary = latest_boundary_idx(tip_idx)?;
    let (ema_price_1_interval_ago, ema_price_2_intervals_ago) = match latest_boundary {
        None => (genesis_ema, genesis_ema),
        Some(boundary_idx) => {
            let ema_price_1_interval_ago = ema_shifted_in_at(boundary_idx)?;
            let previous_boundary = match boundary_idx.checked_sub(1) {
                Some(parent_idx) => latest_boundary_idx(parent_idx)?,
                None => return Err(not_enough_history("the previous interval boundary block")),
            };
            let ema_price_2_intervals_ago = match previous_boundary {
                None => genesis_ema,
                Some(previous_boundary_idx) => ema_shifted_in_at(previous_boundary_idx)?,
            };
            (ema_price_1_interval_ago, ema_price_2_intervals_ago)
        }
    };

    let interval_oracle_prices = blocks[latest_boundary.unwrap_or(0)..]
        .iter()
        .map(|block| block.oracle_irys_price)
        .collect();

    Ok(Arc::new(EmaSnapshot {
        ema_price_2_intervals_ago,
        ema_price_1_interval_ago,
        oracle_price_for_current_ema_predecessor,
        ema_price_current_interval: latest_ema_block.ema_irys_price,
        interval_oracle_prices,
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
        build_blocks_with_oracle_prices(max_height, config, oracle_price_for_height)
    }

    /// Like [`build_blocks_with_snapshots`], with the oracle price of every
    /// non-genesis block given by `oracle_price`. Block `n` has timestamp `n` seconds.
    fn build_blocks_with_oracle_prices(
        max_height: u64,
        config: &ConsensusConfig,
        oracle_price: impl Fn(u64) -> IrysTokenPrice,
    ) -> Vec<(IrysBlockHeader, Arc<EmaSnapshot>)> {
        let mut results: Vec<(IrysBlockHeader, Arc<EmaSnapshot>)> = Vec::new();

        for height in 0..=max_height {
            let mut block = IrysBlockHeader::new_mock_header();
            block.height = height;
            // One block per second, so a hardfork timestamp of `n` seconds
            // activates at height `n`.
            block.timestamp = irys_types::UnixTimestampMs::from_millis(u128::from(height) * 1000);

            // Set oracle price for all blocks
            block.oracle_irys_price = if height == 0 {
                config.genesis.genesis_price
            } else {
                oracle_price(height)
            };

            // Calculate snapshot and EMA price based on block height
            let snapshot = if height == 0 {
                // Genesis block: use genesis price for EMA and create genesis snapshot
                block.ema_irys_price = config.genesis.genesis_price;
                EmaSnapshot::genesis(&block)
            } else {
                // Non-genesis blocks: calculate EMA and create next snapshot
                let (parent_block, parent_snapshot) = &results[height as usize - 1];

                // Calculate and set EMA price before creating snapshot. Like the
                // block producer, cap the oracle price to the safe range first.
                let ema_calculation = parent_snapshot.calculate_ema_for_new_block(
                    parent_block,
                    block.oracle_irys_price,
                    &config.ema_rules_at(block.timestamp_secs()),
                );
                block.oracle_irys_price = ema_calculation.oracle_price_for_block_inclusion;
                block.ema_irys_price = ema_calculation.ema;

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
            // Every block since the latest interval boundary (genesis before the first).
            interval_oracle_prices: blocks
                .iter()
                .filter(|block| {
                    block.height >= height_latest_block - height_latest_block % interval
                })
                .map(|block| block.oracle_irys_price)
                .collect(),
        };
        assert_eq!(&expected_price_snapshot, ema_snapshot.as_ref());
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
        genesis_block.oracle_irys_price = config.genesis.genesis_price;
        genesis_block.ema_irys_price = config.genesis.genesis_price;

        // Create genesis snapshot
        let genesis_snapshot = EmaSnapshot::genesis(&genesis_block);

        // New oracle price for block 1
        let new_oracle_price = Amount::token(dec!(1.01)).unwrap();

        // Calculate EMA for block 1
        let ema_block = genesis_snapshot.calculate_ema_for_new_block(
            &genesis_block,
            new_oracle_price,
            &config.ema_rules_at(irys_types::UnixTimestamp::from_secs(0)),
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
            &config.ema_rules_at(irys_types::UnixTimestamp::from_secs(0)),
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
                &config.ema_rules_at(irys_types::UnixTimestamp::from_secs(0)),
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
            &config.ema_rules_at(irys_types::UnixTimestamp::from_secs(0)),
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

    /// Testing config with Delta active from the block at `activation_height`
    /// (blocks are one second apart), a 10-block interval before it and a
    /// 20-block interval from it.
    fn config_with_delta_at(activation_height: u64) -> ConsensusConfig {
        use irys_types::hardfork_config::Delta;
        let mut config = ConsensusConfig {
            ema: EmaConfig {
                price_adjustment_interval: 10,
            },
            ..ConsensusConfig::testing()
        };
        config.hardforks.delta = Some(Delta {
            activation_timestamp: irys_types::UnixTimestamp::from_secs(activation_height),
            initial_slots_per_new_ledger: Delta::default_initial_slots_per_new_ledger(),
            capacity_chunk_reward_percent: Delta::default_capacity_chunk_reward_percent(),
            ingress_proof_reward_pool_percent: Delta::default_ingress_proof_reward_pool_percent(),
            token_price_safe_range: Delta::default_token_price_safe_range(),
            ema_price_adjustment_interval: 20,
        });
        config
    }

    #[rstest]
    #[case::odd(&[3, 1, 2], 2)]
    #[case::even_takes_midpoint(&[4, 1, 2, 10], 3)]
    #[case::even_midpoint_rounds_down(&[1, 2], 1)]
    #[case::single(&[7], 7)]
    #[case::outlier_is_ignored(&[100, 100, 100, 100, 1_000_000], 100)]
    fn median_price_cases(#[case] amounts: &[u64], #[case] expected: u64) {
        let prices = amounts
            .iter()
            .map(|&amount| IrysTokenPrice::new(amount.into()))
            .collect();
        assert_eq!(
            median_price(prices),
            Some(IrysTokenPrice::new(expected.into()))
        );
    }

    #[test]
    fn median_price_of_nothing_is_none() {
        assert_eq!(median_price(Vec::new()), None);
    }

    /// From Delta the update at the end of an interval takes the median of the
    /// interval's 20 oracle prices, the recalculation block's own included.
    #[test]
    fn delta_ema_update_uses_interval_median() {
        let config = config_with_delta_at(0);
        // Interval 40..=59 ends with the recalculation block 59.
        let blocks_and_snapshots = build_blocks_with_snapshots(59, &config);
        let (recalculation_block, snapshot) = &blocks_and_snapshots[59];
        let (_, parent_snapshot) = &blocks_and_snapshots[58];

        let interval_prices: Vec<IrysTokenPrice> = (40..=59)
            .map(|height| blocks_and_snapshots[height].0.oracle_irys_price)
            .collect();
        assert_eq!(snapshot.interval_oracle_prices, interval_prices);

        // Prices rise with the height, so the median is the midpoint of blocks 49 and 50.
        let low = blocks_and_snapshots[49].0.oracle_irys_price;
        let high = blocks_and_snapshots[50].0.oracle_irys_price;
        let median = IrysTokenPrice::new(low.amount + (high.amount - low.amount) / 2);
        let expected_ema = median
            .calculate_ema(20, parent_snapshot.ema_price_current_interval)
            .unwrap();
        assert_eq!(recalculation_block.ema_irys_price, expected_ema);
        assert_eq!(snapshot.ema_price_current_interval, expected_ema);
    }

    /// One producer reporting the lowest allowed price cannot move the Delta EMA
    /// update, at any position in the interval. Before Delta the producer of the
    /// sampled block decides the update on its own.
    #[rstest]
    #[case::first_block_of_interval(40)]
    #[case::middle_of_interval(47)]
    #[case::block_before_recalculation(58)]
    #[case::recalculation_block(59)]
    fn delta_single_producer_cannot_move_ema_update(#[case] manipulated_height: u64) {
        let config = config_with_delta_at(0);
        let honest_price = IrysTokenPrice::token(dec!(1.0)).unwrap();
        // The largest drop the 10% bound allows from the honest price.
        let manipulated_price = IrysTokenPrice::token(dec!(0.9)).unwrap();

        let honest = build_blocks_with_oracle_prices(59, &config, |_| honest_price);
        let manipulated = build_blocks_with_oracle_prices(59, &config, |height| {
            if height == manipulated_height {
                manipulated_price
            } else {
                honest_price
            }
        });

        assert_eq!(
            manipulated[manipulated_height as usize].0.oracle_irys_price,
            manipulated_price
        );
        assert_eq!(
            manipulated[59].1.ema_price_current_interval, honest[59].1.ema_price_current_interval,
            "one manipulated oracle price must not change the interval's EMA update"
        );
    }

    /// The pre-Delta rule this fork replaces: the update at block 29 follows the
    /// oracle price of block 18 alone.
    #[test]
    fn pre_delta_single_producer_moves_ema_update() {
        let config = ConsensusConfig {
            ema: EmaConfig {
                price_adjustment_interval: 10,
            },
            ..ConsensusConfig::testing()
        };
        let honest_price = IrysTokenPrice::token(dec!(1.0)).unwrap();
        let manipulated_price = IrysTokenPrice::token(dec!(0.5)).unwrap();

        let honest = build_blocks_with_oracle_prices(29, &config, |_| honest_price);
        let manipulated = build_blocks_with_oracle_prices(29, &config, |height| {
            if height == 18 {
                manipulated_price
            } else {
                honest_price
            }
        });

        // Block 18 is inside the bootstrap intervals, so compare the EMA inputs
        // of the update at block 29, not its result.
        let ema_input = |chain: &[(IrysBlockHeader, Arc<EmaSnapshot>)]| {
            chain[28]
                .1
                .calculate_ema_for_new_block(
                    &chain[28].0,
                    honest_price,
                    &config.ema_rules_at(chain[28].0.timestamp_secs()),
                )
                .oracle_price_for_calculation
        };
        assert_eq!(ema_input(&honest), honest_price);
        assert_eq!(ema_input(&manipulated), manipulated_price);
    }

    /// From Delta an oracle price may differ from its parent's by 10%, where the
    /// base config allows 100%.
    #[test]
    fn delta_narrows_oracle_price_bound() {
        let config = config_with_delta_at(50);
        assert_eq!(
            config.token_price_safe_range,
            Amount::percentage(dec!(1)).unwrap(),
            "the testing config allows a 100% move before Delta"
        );
        let blocks_and_snapshots = build_blocks_with_oracle_prices(49, &config, |_| {
            IrysTokenPrice::token(dec!(1.0)).unwrap()
        });
        let (parent_block, parent_snapshot) = blocks_and_snapshots.last().unwrap();
        let doubled = IrysTokenPrice::token(dec!(2.0)).unwrap();

        let before = config.ema_rules_at(irys_types::UnixTimestamp::from_secs(49));
        let from = config.ema_rules_at(irys_types::UnixTimestamp::from_secs(50));

        assert!(EmaSnapshot::oracle_price_is_valid(
            doubled,
            parent_block.oracle_irys_price,
            before.token_price_safe_range
        ));
        assert!(!EmaSnapshot::oracle_price_is_valid(
            doubled,
            parent_block.oracle_irys_price,
            from.token_price_safe_range
        ));
        // The producer caps its own price to the Delta bound.
        assert_eq!(
            parent_snapshot
                .calculate_ema_for_new_block(parent_block, doubled, &from)
                .oracle_price_for_block_inclusion,
            IrysTokenPrice::token(dec!(1.1)).unwrap()
        );
    }

    /// Delta switches the interval from 10 to 20 blocks at whichever height its
    /// timestamp falls on. The snapshot built block by block shifts its interval
    /// EMAs only where the rules of each block say so.
    #[rstest]
    // Active from block 25: 20 was the last 10-block boundary, 30 is not one.
    #[case(25, &[10, 20, 40, 60, 80])]
    // Active from block 30, which no longer crosses a boundary.
    #[case(30, &[10, 20, 40, 60, 80])]
    // Active from block 31: 30 still crossed under the 10-block rule.
    #[case(31, &[10, 20, 30, 40, 60, 80])]
    #[case(40, &[10, 20, 30, 40, 60, 80])]
    #[case(55, &[10, 20, 30, 40, 50, 60, 80])]
    fn delta_interval_boundaries_across_activation(
        #[case] activation_height: u64,
        #[case] expected_boundaries: &[u64],
    ) {
        let config = config_with_delta_at(activation_height);
        let blocks_and_snapshots = build_blocks_with_snapshots(85, &config);

        // A block crosses a boundary exactly when its snapshot restarts the
        // interval's oracle prices with its own.
        let boundaries: Vec<u64> = blocks_and_snapshots
            .iter()
            .skip(1)
            .filter(|(_, snapshot)| snapshot.interval_oracle_prices.len() == 1)
            .map(|(block, _)| block.height)
            .collect();
        assert_eq!(boundaries, expected_boundaries);

        for window in blocks_and_snapshots.windows(2) {
            let (_, parent_snapshot) = &window[0];
            let (block, snapshot) = &window[1];
            if boundaries.contains(&block.height) {
                assert_eq!(
                    snapshot.ema_price_2_intervals_ago, parent_snapshot.ema_price_1_interval_ago,
                    "block {} shifts the interval EMAs",
                    block.height
                );
                assert_eq!(
                    snapshot.ema_price_1_interval_ago,
                    parent_snapshot.ema_price_current_interval
                );
            } else {
                assert_eq!(
                    snapshot.ema_price_2_intervals_ago, parent_snapshot.ema_price_2_intervals_ago,
                    "block {} keeps the interval EMAs",
                    block.height
                );
            }
        }
    }

    /// A node that restarts rebuilds the snapshot from the latest
    /// `ema_snapshot_history_depth` blocks. That must equal the snapshot built
    /// block by block from genesis, at every height around Delta activation.
    #[rstest]
    #[case::delta_from_genesis(0)]
    #[case::inside_first_bootstrap_interval(5)]
    #[case::inside_second_bootstrap_interval(15)]
    #[case::mid_interval(25)]
    #[case::on_ten_block_boundary(30)]
    #[case::after_ten_block_boundary(31)]
    #[case::before_twenty_block_boundary(39)]
    #[case::on_twenty_block_boundary(40)]
    #[case::late(87)]
    #[case::never(u64::MAX)]
    fn delta_history_rebuild_matches_iterative_snapshot(#[case] activation_height: u64) {
        let config = config_with_delta_at(activation_height);
        let depth = config.ema_snapshot_history_depth() as usize;
        let blocks_and_snapshots = build_blocks_with_snapshots(200, &config);
        let blocks: Vec<IrysBlockHeader> = blocks_and_snapshots
            .iter()
            .map(|(block, _)| block.clone())
            .collect();

        for (height, (_, iterative_snapshot)) in blocks_and_snapshots.iter().enumerate() {
            let oldest = (height + 1).saturating_sub(depth);
            let rebuilt =
                create_ema_snapshot_from_chain_history(&blocks[oldest..=height], &config).unwrap();
            assert_eq!(
                rebuilt.as_ref(),
                iterative_snapshot.as_ref(),
                "rebuilt snapshot differs at height {height} (Delta from height {activation_height})"
            );
        }
    }

    #[test]
    fn history_rebuild_rejects_too_short_or_gapped_history() {
        let config = config_with_delta_at(u64::MAX);
        let blocks: Vec<IrysBlockHeader> = build_blocks_with_snapshots(100, &config)
            .into_iter()
            .map(|(block, _)| block)
            .collect();

        // Five blocks do not reach back two interval boundaries.
        assert!(create_ema_snapshot_from_chain_history(&blocks[96..=100], &config).is_err());

        let mut gapped = blocks[40..=100].to_vec();
        gapped.remove(10);
        assert!(create_ema_snapshot_from_chain_history(&gapped, &config).is_err());
    }

    #[rstest]
    #[case(10, &[7, 8, 9])]
    #[case(4, &[3])]
    #[case(2, &[1])]
    #[case(1, &[0])]
    fn last_quarter_window_ends_on_tip_below_roll(
        #[case] interval: u64,
        #[case] expected_positions: &[u64],
    ) {
        let in_window: Vec<u64> = (0..interval)
            .filter(|&position| {
                // Offset by two intervals so the position is not the height itself.
                tip_in_last_quarter_of_ema_interval(2 * interval + position, interval)
            })
            .collect();
        assert_eq!(in_window, expected_positions);
    }

    #[test]
    fn public_quote_uses_lower_ema_on_tip_below_roll() {
        let interval = 10;
        let lower = IrysTokenPrice::token(dec!(0.9)).unwrap();
        let higher = IrysTokenPrice::token(dec!(1.1)).unwrap();
        let falling = EmaSnapshot {
            ema_price_2_intervals_ago: higher,
            ema_price_1_interval_ago: lower,
            ..Default::default()
        };
        let rising = EmaSnapshot {
            ema_price_2_intervals_ago: lower,
            ema_price_1_interval_ago: higher,
            ..Default::default()
        };

        // The next block (height 30) rolls the snapshot.
        assert_eq!(falling.ema_for_public_quote(29, interval), lower);
        assert_eq!(rising.ema_for_public_quote(29, interval), lower);
        // The roll is 5 blocks ahead: the standard public price applies.
        assert_eq!(falling.ema_for_public_quote(25, interval), higher);
        assert_eq!(rising.ema_for_public_quote(25, interval), lower);
    }

    mod bound_in_min_max_range_tests {
        use super::*;
        use proptest::prelude::*;
        use rust_decimal_macros::dec;

        proptest! {
            #[test]
            fn result_always_within_safe_range(
                base in 1_u64..1_000_000,
                desired in 0_u64..2_000_000,
                pct in 0_u32..=100,
            ) {
                let base_price = IrysTokenPrice::token(Decimal::from(base)).unwrap();
                let desired_price = IrysTokenPrice::token(Decimal::from(desired)).unwrap();
                let safe_range = Amount::percentage(
                    Decimal::from(pct) / dec!(100)
                ).unwrap();

                let result = bound_in_min_max_range(desired_price, safe_range, base_price);

                let max = base_price.add_multiplier(safe_range).unwrap_or(base_price);
                let min = base_price.sub_multiplier(safe_range).unwrap_or(base_price);
                prop_assert!(result <= max, "result {result:?} > max {max:?}");
                prop_assert!(result >= min, "result {result:?} < min {min:?}");
                if desired_price >= min && desired_price <= max {
                    prop_assert_eq!(result, desired_price, "in-range value should pass through unchanged");
                }
            }
        }
    }

    mod oracle_price_is_valid_tests {
        use super::*;
        use rust_decimal_macros::dec;

        #[test]
        fn zero_oracle_price_is_always_invalid() {
            let parent = IrysTokenPrice::token(dec!(1.0)).unwrap();
            // Full ±100% range would otherwise treat 0 as the min bound of parent.
            let full_range = Amount::percentage(dec!(1.0)).unwrap();
            let zero = IrysTokenPrice::token(dec!(0.0)).unwrap();

            assert!(
                !EmaSnapshot::oracle_price_is_valid(zero, parent, full_range),
                "zero oracle price must be rejected even when the relative safe range includes 0"
            );
        }

        #[test]
        fn positive_price_within_range_is_valid() {
            let parent = IrysTokenPrice::token(dec!(1.0)).unwrap();
            let range = Amount::percentage(dec!(0.1)).unwrap();
            let price = IrysTokenPrice::token(dec!(1.05)).unwrap();

            assert!(EmaSnapshot::oracle_price_is_valid(price, parent, range));
        }

        #[test]
        fn positive_price_outside_range_is_invalid() {
            let parent = IrysTokenPrice::token(dec!(1.0)).unwrap();
            let range = Amount::percentage(dec!(0.1)).unwrap();
            let price = IrysTokenPrice::token(dec!(1.20)).unwrap();

            assert!(!EmaSnapshot::oracle_price_is_valid(price, parent, range));
        }
    }
}
