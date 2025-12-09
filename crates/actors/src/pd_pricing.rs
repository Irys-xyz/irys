use self::base_fee::{
    calculate_pd_base_fee_for_new_block, count_pd_chunks_in_block, extract_pd_base_fee_from_block,
    extract_priority_fees_from_block,
};
use eyre::OptionExt as _;
use irys_domain::BlockTreeReadGuard;
use irys_reth_node_bridge::node::RethNodeProvider;
use irys_types::{
    storage_pricing::{
        mul_div,
        phantoms::{CostPerChunk, Irys, Percentage, Usd},
        Amount, PRECISION_SCALE,
    },
    Config, IrysTokenPrice,
};
use reth::providers::BlockReader as _;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Service for calculating and estimating PD pricing.
///
/// This service encapsulates all PD pricing logic, making it reusable across
/// different parts of the application (API, CLI, tests, etc.).
#[derive(Debug, Clone)]
pub struct PdPricing {
    block_tree: BlockTreeReadGuard,
    reth_provider: RethNodeProvider,
    config: Arc<Config>,
}

impl PdPricing {
    pub fn new(
        block_tree: BlockTreeReadGuard,
        reth_provider: RethNodeProvider,
        config: Arc<Config>,
    ) -> Self {
        Self {
            block_tree,
            reth_provider,
            config,
        }
    }

    /// Get comprehensive fee history (Ethereum eth_feeHistory style).
    ///
    /// Returns per-block data for base fees, utilization, and priority fees.
    ///
    /// # Arguments
    /// * `block_count` - Number of recent blocks to analyze (1-1000)
    /// * `reward_percentiles` - Priority fee percentiles to calculate (0-100)
    ///
    /// # Returns
    /// * `PdFeeHistoryResponse` with per-block data
    ///
    /// # Example
    /// ```ignore
    /// let history = pd_pricing.get_fee_history(20, &[25, 50, 75])?;
    /// ```
    pub fn get_fee_history(
        &self,
        block_count: u64,
        reward_percentiles: &[u8],
    ) -> eyre::Result<PdFeeHistoryResponse> {
        // Validate inputs
        validate_percentiles(reward_percentiles)?;

        // Get recent blocks from tip (oldest-first, eth_feeHistory pattern)
        let blocks_with_ema: Vec<_> = {
            let tree = self.block_tree.read();
            tree.get_recent_blocks_from_tip(block_count as usize)
                .into_iter()
                .filter_map(|block| {
                    let ema = tree.get_ema_snapshot(&block.block_hash)?;
                    Some((block.clone(), ema))
                })
                .collect()
        };

        if blocks_with_ema.is_empty() {
            eyre::bail!("No blocks available for analysis");
        }

        // Process each block - build flat arrays
        let mut base_fee_irys_vec = Vec::new();
        let mut base_fee_usd_vec = Vec::new();
        let mut gas_used_ratio_vec = Vec::new();
        let mut reward_vec = Vec::new();

        for (irys_block, ema_snapshot) in &blocks_with_ema {
            // Get EVM block
            let evm_block = self.get_evm_block_for_irys_block(irys_block)?;

            // Extract base fee
            let pd_config = &self.config.consensus.programmable_data;
            let chunk_size = self.config.consensus.chunk_size;
            let base_fee_irys =
                extract_pd_base_fee_from_block(irys_block, &evm_block, pd_config, chunk_size)?;

            let ema_price = ema_snapshot.ema_for_public_pricing();
            let base_fee_usd = convert_irys_to_usd(base_fee_irys, &ema_price)?;

            // Push to flat arrays
            base_fee_irys_vec.push(base_fee_irys);
            base_fee_usd_vec.push(base_fee_usd);

            // Calculate utilization (deterministic fixed-point arithmetic)
            let utilization_percent = self.calculate_pd_utilization_percent(&evm_block)?;
            gas_used_ratio_vec.push(utilization_percent);

            // Extract priority fees and calculate percentiles
            let block_priority_fees = Self::calculate_priority_fee_percentiles(
                &evm_block,
                reward_percentiles,
                &ema_price,
            )?;
            reward_vec.push(block_priority_fees);
        }

        // Predict next block's base fee (Ethereum N+1 pattern)
        // Use newest block (last in array after oldest-first ordering) for prediction
        let (current_irys_block, current_ema) =
            blocks_with_ema.last().ok_or_eyre("No blocks available")?;

        let (next_base_fee_irys, next_base_fee_usd) =
            self.predict_next_block_fees(current_irys_block, current_ema)?;

        // Append next block prediction to base fee arrays (Ethereum N+1 pattern)
        base_fee_irys_vec.push(next_base_fee_irys);
        base_fee_usd_vec.push(next_base_fee_usd);

        // Build simplified response
        // oldest_block is first in array after oldest-first ordering
        let oldest_block = blocks_with_ema.first().ok_or_eyre("No oldest block")?;

        Ok(PdFeeHistoryResponse {
            oldest_block: oldest_block.0.height,
            base_fee_per_chunk_irys: base_fee_irys_vec,
            base_fee_per_chunk_usd: base_fee_usd_vec,
            gas_used_ratio: gas_used_ratio_vec,
            reward: reward_vec,
        })
    }

    /// Fetch the EVM block corresponding to an Irys block.
    ///
    /// # Arguments
    /// * `irys_block` - The Irys block header containing the EVM block hash
    ///
    /// # Returns
    /// The corresponding EVM block
    ///
    /// # Errors
    /// Returns error if EVM block not found or fetch fails
    fn get_evm_block_for_irys_block(
        &self,
        irys_block: &irys_types::IrysBlockHeader,
    ) -> eyre::Result<reth_ethereum_primitives::Block> {
        self.reth_provider
            .provider
            .block_by_hash(irys_block.evm_block_hash)?
            .ok_or_eyre(format!(
                "EVM block {} not found for Irys block {}",
                irys_block.evm_block_hash, irys_block.block_hash
            ))
    }

    /// Calculate PD utilization as a percentage using fixed-point arithmetic.
    ///
    /// Computes: (chunks_used * PRECISION_SCALE) / max_chunks
    ///
    /// # Arguments
    /// * `evm_block` - The EVM block to analyze
    ///
    /// # Returns
    /// Utilization percentage (0-100% in fixed-point)
    ///
    /// # Examples
    /// - 50 chunks used / 100 max = 50% utilization
    /// - 0 chunks used = 0% utilization
    /// - max_chunks = 0 = 0% utilization (edge case)
    fn calculate_pd_utilization_percent(
        &self,
        evm_block: &reth_ethereum_primitives::Block,
    ) -> eyre::Result<Amount<Percentage>> {
        let pd_chunks_used = count_pd_chunks_in_block(evm_block);
        let max_pd_chunks = self
            .config
            .consensus
            .programmable_data
            .max_pd_chunks_per_block;

        let utilization_percent = if max_pd_chunks > 0 {
            // Calculate: (pd_chunks_used * PRECISION_SCALE) / max_pd_chunks
            // This gives us the percentage in fixed-point where PRECISION_SCALE = 100%
            let percent_amount = mul_div(
                irys_types::U256::from(pd_chunks_used),
                PRECISION_SCALE,
                irys_types::U256::from(max_pd_chunks),
            )?;
            Amount::<Percentage>::new(percent_amount)
        } else {
            Amount::<Percentage>::new(irys_types::U256::from(0))
        };

        Ok(utilization_percent)
    }

    /// Calculate priority fee percentiles for a block's PD transactions.
    ///
    /// Extracts all priority fees, sorts them, calculates requested percentiles,
    /// and converts each to USD.
    ///
    /// # Arguments
    /// * `evm_block` - The EVM block to analyze
    /// * `reward_percentiles` - Percentiles to calculate (e.g., [25, 50, 75])
    /// * `ema_price` - Current EMA price for USD conversion
    ///
    /// # Returns
    /// Block priority fees with percentile map and sample size
    ///
    /// # Examples
    /// - Empty block (no PD txs) = empty percentile map, pd_tx_count = 0
    /// - Block with 10 PD txs, percentiles [50] = median fee
    fn calculate_priority_fee_percentiles(
        evm_block: &reth_ethereum_primitives::Block,
        reward_percentiles: &[u8],
        ema_price: &IrysTokenPrice,
    ) -> eyre::Result<BlockPriorityFees> {
        let block_priority_fees = extract_priority_fees_from_block(evm_block);

        let mut percentile_map = std::collections::HashMap::new();
        if !block_priority_fees.is_empty() {
            let mut sorted_fees = block_priority_fees.clone();
            sorted_fees.sort_by(|a, b| a.amount.cmp(&b.amount));

            for &percentile in reward_percentiles {
                let index = ((percentile as usize * sorted_fees.len()) / 100)
                    .min(sorted_fees.len().saturating_sub(1));
                let fee_irys = sorted_fees[index];
                let fee_usd = convert_irys_to_usd(fee_irys, ema_price)?;

                percentile_map.insert(percentile, PriorityFeeAtPercentile { fee_irys, fee_usd });
            }
        }

        Ok(BlockPriorityFees {
            percentiles: percentile_map,
            pd_tx_count: block_priority_fees.len(),
        })
    }

    /// Predict the base fees for the next block based on current block state.
    ///
    /// This implements Ethereum's N+1 fee prediction pattern by:
    /// 1. Fetching the current block's EVM data
    /// 2. Extracting current base fee
    /// 3. Counting chunks used in current block
    /// 4. Calculating next block's base fee based on utilization
    /// 5. Converting to USD
    ///
    /// # Arguments
    /// * `current_irys_block` - Current (most recent) Irys block header
    /// * `current_ema` - Current EMA snapshot for price conversions
    ///
    /// # Returns
    /// Tuple of (next_base_fee_irys, next_base_fee_usd)
    ///
    /// # Errors
    /// Returns error if EVM block fetch fails, fee extraction fails, or calculation fails
    fn predict_next_block_fees(
        &self,
        current_irys_block: &irys_types::IrysBlockHeader,
        current_ema: &irys_domain::EmaSnapshot,
    ) -> eyre::Result<(Amount<(CostPerChunk, Irys)>, Amount<(CostPerChunk, Usd)>)> {
        // Fetch current EVM block using helper #1
        let current_evm_block = self.get_evm_block_for_irys_block(current_irys_block)?;

        // Extract config values
        let pd_config = &self.config.consensus.programmable_data;
        let chunk_size = self.config.consensus.chunk_size;

        // Extract current base fee
        let current_base_fee = extract_pd_base_fee_from_block(
            current_irys_block,
            &current_evm_block,
            pd_config,
            chunk_size,
        )?;

        // Count chunks used in current block
        let current_chunks_used = count_pd_chunks_in_block(&current_evm_block);

        // Get current EMA price
        let current_ema_price = current_ema.ema_for_public_pricing();

        // Calculate next block's base fee
        let next_base_fee_irys = calculate_pd_base_fee_for_new_block(
            current_ema,
            &current_ema_price,
            current_chunks_used as u32,
            current_base_fee,
            pd_config,
            chunk_size,
        )?;

        // Convert to USD
        let next_base_fee_usd = convert_irys_to_usd(next_base_fee_irys, &current_ema_price)?;

        Ok((next_base_fee_irys, next_base_fee_usd))
    }
}

/// Simplified fee history response following Ethereum's eth_feeHistory structure.
///
/// Array lengths:
/// - base_fee_per_chunk_irys/usd: N+1 (includes next block prediction)
/// - gas_used_ratio: N (historical blocks only)
/// - reward: N (historical blocks only)
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PdFeeHistoryResponse {
    /// Oldest block number in the returned range
    pub oldest_block: u64,

    /// Base fee per chunk in Irys (length = N+1, includes next block prediction)
    pub base_fee_per_chunk_irys: Vec<Amount<(CostPerChunk, Irys)>>,

    /// Base fee per chunk in USD (length = N+1, includes next block prediction)
    pub base_fee_per_chunk_usd: Vec<Amount<(CostPerChunk, Usd)>>,

    /// PD utilization as percentage (length = N, historical blocks only)
    pub gas_used_ratio: Vec<Amount<Percentage>>,

    /// Priority fees per block with percentiles (length = N, historical blocks only)
    pub reward: Vec<BlockPriorityFees>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockPriorityFees {
    /// Requested percentiles with their values
    /// Key: percentile (e.g., 25, 50, 90)
    /// Value: fee amount in Irys and USD
    pub percentiles: std::collections::HashMap<u8, PriorityFeeAtPercentile>,

    /// Number of PD transactions in this block
    pub pd_tx_count: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriorityFeeAtPercentile {
    pub fee_irys: Amount<(CostPerChunk, Irys)>,
    pub fee_usd: Amount<(CostPerChunk, Usd)>,
}

// Helper Functions

/// Convert typed Irys amount to typed USD amount using EMA price
fn convert_irys_to_usd(
    irys_fee: Amount<(CostPerChunk, Irys)>,
    ema_price: &IrysTokenPrice,
) -> eyre::Result<Amount<(CostPerChunk, Usd)>> {
    let usd_amount = mul_div(irys_fee.amount, ema_price.amount, PRECISION_SCALE)?;
    Ok(Amount::<(CostPerChunk, Usd)>::new(usd_amount))
}

/// Validate percentiles array for fee history queries
fn validate_percentiles(percentiles: &[u8]) -> eyre::Result<()> {
    if percentiles.is_empty() {
        eyre::bail!("At least one percentile must be specified");
    }
    if percentiles.len() > 10 {
        eyre::bail!("Maximum 10 percentiles allowed");
    }
    for &p in percentiles {
        if p > 100 {
            eyre::bail!("Percentiles must be 0-100, got {}", p);
        }
    }
    Ok(())
}

// Base Fee Calculation Module

/// Base fee calculation utilities for Programmable Data.
///
/// This module provides low-level functions for:
/// - Calculating PD base fees based on utilization
/// - Extracting fee data from blocks
/// - Counting PD chunks in blocks
/// - Extracting priority fees from transactions
pub mod base_fee {
    use irys_domain::EmaSnapshot;
    use irys_types::{
        storage_pricing::{
            mul_div,
            phantoms::{CostPerChunk, Irys, Usd},
            Amount, PRECISION_SCALE,
        },
        Config, IrysBlockHeader, ProgrammableDataConfig, U256,
    };

    const MB_SIZE: u64 = 1024 * 1024;
    pub const PD_BASE_FEE_INDEX: usize = 1;

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
    /// 2. Adjusting the USD fee based on block utilization (target: 50%, range: +-12.5%)
    ///    - The floor is converted from per-MB to per-chunk for comparison
    /// 3. Converting the adjusted per-chunk USD fee back to per-chunk Irys using the current block's EMA price
    pub fn calculate_pd_base_fee_for_new_block(
        parent_block_ema_snapshot: &EmaSnapshot,
        current_block_ema_price: &irys_types::IrysTokenPrice,
        parent_chunks_used_in_block: u32,
        parent_pd_base_fee_irys: Amount<(CostPerChunk, Irys)>,
        pd_config: &ProgrammableDataConfig,
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
    pub fn extract_pd_base_fee_from_block(
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
        //       This is just a temp measurement until we hardfork-guard PD functionality
        if irys_block_header.height == 0 {
            // Convert the floor from per-MB to per-chunk
            let floor_per_chunk_usd = Amount::new(mul_div(
                pd_config.base_fee_floor.amount,
                U256::from(chunk_size),
                U256::from(MB_SIZE),
            )?);

            // Convert per-chunk USD -> per-chunk Irys using genesis block's EMA price
            let floor_per_chunk_irys = convert_per_chunk_usd_to_irys(
                floor_per_chunk_usd,
                &irys_block_header.ema_irys_price,
            )?;
            return Ok(floor_per_chunk_irys);
        }

        // Extract current PD base fee from block's 2nd shadow transaction (PdBaseFeeUpdate)
        // The ordering is: [0] BlockReward, [1] PdBaseFeeUpdate, [2+] other shadow txs
        let second_tx = evm_block
            .body
            .transactions
            .get(PD_BASE_FEE_INDEX)
            .ok_or_eyre(
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
            // utilization = (chunks_used * PRECISION_SCALE) / max_chunks
            let utilization = mul_div(
                U256::from(chunks_used_in_block),
                PRECISION_SCALE,
                U256::from(max_pd_chunks_per_block),
            )?;

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
            use irys_types::ProgrammableDataConfig;
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
                let pd_config = ProgrammableDataConfig {
                    cost_per_mb: Amount::token(dec!(0.01))?,
                    base_fee_floor: Amount::token(dec!(0.01))?,
                    max_pd_chunks_per_block: max_chunks_per_block,
                };

                // Action
                let new_fee = calculate_new_base_fee(
                    current_base_fee,
                    chunks_used,
                    pd_config.max_pd_chunks_per_block,
                    pd_config.base_fee_floor,
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
}
