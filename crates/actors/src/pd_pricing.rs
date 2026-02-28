use base_fee::{
    count_pd_chunks_in_block, extract_pd_base_fee_from_block, extract_priority_fees_from_block,
};
use eyre::OptionExt as _;
use irys_domain::BlockTreeReadGuard;
use irys_reth_node_bridge::node::RethNodeProvider;
use irys_types::{
    Config, IrysTokenPrice,
    storage_pricing::{
        Amount, PRECISION_SCALE, mul_div,
        phantoms::{CostPerChunk, Irys, Percentage, Usd},
    },
};
use reth::providers::BlockReader as _;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
pub mod base_fee;

/// Service for calculating and estimating PD pricing.
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
        // Check if Sprite hardfork is active at the current tip
        let current_timestamp = {
            let tree = self.block_tree.read();
            tree.get_block(&tree.tip)
                .ok_or_eyre("No tip block found in block tree")?
                .timestamp_secs()
        };
        eyre::ensure!(
            self.config
                .consensus
                .hardforks
                .is_sprite_active(current_timestamp),
            "PD pricing unavailable: Sprite hardfork not active"
        );

        // Validate inputs
        validate_percentiles(reward_percentiles)?;

        // Get recent blocks from tip (oldest-first, eth_feeHistory pattern)
        // Filter out pre-Sprite blocks from the beginning of the range
        let blocks_with_ema: Vec<_> = {
            let tree = self.block_tree.read();
            let hardforks = &self.config.consensus.hardforks;
            tree.get_recent_blocks_from_tip(block_count as usize)
                .into_iter()
                .filter_map(|block| {
                    let ema = tree.get_ema_snapshot(&block.block_hash)?;
                    Some((block.clone(), ema))
                })
                // Skip pre-Sprite blocks (oldest-first order, so skip_while removes leading non-Sprite)
                .skip_while(|(block, _)| hardforks.sprite_at(block.timestamp_secs()).is_none())
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

            let block_timestamp = irys_block.timestamp_secs();
            let chunk_size = self.config.consensus.chunk_size;
            // Safe to unwrap: pre-Sprite blocks were filtered out above
            let sprite = self
                .config
                .consensus
                .hardforks
                .sprite_at(block_timestamp)
                .expect("pre-Sprite blocks filtered");
            let base_fee_irys =
                extract_pd_base_fee_from_block(irys_block, &evm_block, sprite, chunk_size)?;

            let ema_price = ema_snapshot.ema_for_public_pricing();
            let base_fee_usd = convert_irys_to_usd(base_fee_irys, &ema_price)?;

            // Push to flat arrays
            base_fee_irys_vec.push(base_fee_irys);
            base_fee_usd_vec.push(base_fee_usd);

            // Calculate utilization (deterministic fixed-point arithmetic)
            let utilization_percent =
                self.calculate_pd_utilization_percent(&evm_block, block_timestamp)?;
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

        // Get min_pd_transaction_cost from the current (newest) block's sprite config
        let current_timestamp = current_irys_block.timestamp_secs();
        let current_sprite = self
            .config
            .consensus
            .hardforks
            .sprite_at(current_timestamp)
            .expect("pre-Sprite blocks filtered");
        let min_cost_usd = current_sprite.min_pd_transaction_cost;
        let current_ema_price = current_ema.ema_for_public_pricing();
        let min_cost_irys = convert_usd_to_irys_total(min_cost_usd, &current_ema_price)?;

        // oldest_block is first in array after oldest-first ordering
        let oldest_block = blocks_with_ema.first().ok_or_eyre("No oldest block")?;

        Ok(PdFeeHistoryResponse {
            oldest_block: oldest_block.0.height,
            base_fee_per_chunk_irys: base_fee_irys_vec,
            base_fee_per_chunk_usd: base_fee_usd_vec,
            gas_used_ratio: gas_used_ratio_vec,
            reward: reward_vec,
            min_pd_transaction_cost_irys: min_cost_irys,
            min_pd_transaction_cost_usd: min_cost_usd,
        })
    }

    /// Fetch the EVM block corresponding to an Irys block.
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
    /// # Returns
    /// Utilization percentage (0-100% in fixed-point)
    ///
    /// # Examples
    /// - 50 chunks used / 100 max = 50% utilization
    /// - 0 chunks used = 0% utilization
    fn calculate_pd_utilization_percent(
        &self,
        evm_block: &reth_ethereum_primitives::Block,
        block_timestamp: irys_types::UnixTimestamp,
    ) -> eyre::Result<Amount<Percentage>> {
        let pd_chunks_used = count_pd_chunks_in_block(evm_block);
        let max_pd_chunks = self
            .config
            .consensus
            .hardforks
            .max_pd_chunks_per_block_at(block_timestamp)
            .unwrap_or(0);

        base_fee::calculate_utilization_percent(pd_chunks_used, max_pd_chunks)
    }

    /// Calculate priority fee percentiles for a block's PD transactions.
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
        let current_evm_block = self.get_evm_block_for_irys_block(current_irys_block)?;
        let current_ema_price = current_ema.ema_for_public_pricing();

        // Estimate next block timestamp by adding block time to current block
        let block_time = self.config.consensus.difficulty_adjustment.block_time;
        let next_block_timestamp = irys_types::UnixTimestamp::from_secs(
            current_irys_block.timestamp_secs().as_secs() + block_time,
        );

        let next_base_fee_irys = base_fee::compute_base_fee_per_chunk(
            &self.config,
            current_irys_block,
            current_ema,
            &current_ema_price,
            &current_evm_block,
            next_block_timestamp,
        )?;

        let next_base_fee_usd = convert_irys_to_usd(next_base_fee_irys, &current_ema_price)?;

        Ok((next_base_fee_irys, next_base_fee_usd))
    }
}

/// Fee history response
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

    /// Minimum total transaction cost in IRYS (at current EMA price).
    /// Transaction composers should ensure: `total_fee >= max(min_pd_transaction_cost, computed_cost)`
    pub min_pd_transaction_cost_irys: Amount<Irys>,

    /// Minimum total transaction cost in USD (from hardfork config).
    /// Transaction composers should ensure: `total_fee >= max(min_pd_transaction_cost, computed_cost)`
    pub min_pd_transaction_cost_usd: Amount<Usd>,
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

/// Convert USD amount to Irys amount using EMA price (for total transaction cost)
fn convert_usd_to_irys_total(
    usd_fee: Amount<Usd>,
    ema_price: &IrysTokenPrice,
) -> eyre::Result<Amount<Irys>> {
    let irys_amount = mul_div(usd_fee.amount, PRECISION_SCALE, ema_price.amount)?;
    Ok(Amount::<Irys>::new(irys_amount))
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
