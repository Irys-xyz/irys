//! PD (Programmable Data) pricing service.
//!
//! Provides functionality for querying PD base fees, estimating future fees,
//! and analyzing priority fees for PD transactions.

use eyre::OptionExt as _;
use irys_domain::BlockTreeReadGuard;
use irys_reth_node_bridge::node::RethNodeProvider;
use irys_types::{
    storage_pricing::{
        mul_div,
        phantoms::{CostPerChunk, Irys, Percentage, Usd},
        Amount, PRECISION_SCALE,
    },
    Config, IrysTokenPrice, UnixTimestampMs, H256,
};
use reth::providers::BlockReader as _;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use self::base_fee::{
    calculate_pd_base_fee_for_new_block, count_pd_chunks_in_block, extract_pd_base_fee_from_block,
    extract_priority_fees_from_block,
};

// PdPricing Service

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
    /// Create a new PdPricing service.
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

    /// Get the current PD base fee per chunk.
    pub fn get_current_base_fee(&self) -> eyre::Result<PdBaseFeeResponse> {
        // Get the latest canonical block from block_tree
        let (block_height, block_hash, evm_block_hash, ema_snapshot, irys_block) = {
            let tree = self.block_tree.read();
            let tip_hash = tree.tip;

            let irys_block = tree
                .get_block(&tip_hash)
                .ok_or_eyre(format!("No block found at tip {}", tip_hash))?
                .clone();

            let ema_snapshot = tree
                .get_ema_snapshot(&tip_hash)
                .ok_or_eyre(format!("No EMA snapshot found for tip block {}", tip_hash))?
                .clone();

            let block_height = irys_block.height;
            let block_hash = irys_block.block_hash;
            let evm_block_hash = irys_block.evm_block_hash;

            (block_height, block_hash, evm_block_hash, ema_snapshot, irys_block)
        }; // Lock is released here

        // Get EVM block from reth provider
        let evm_block = self
            .reth_provider
            .provider
            .block_by_hash(evm_block_hash)?
            .ok_or_eyre("EVM block not found")?;

        // Extract PD base fee
        let pd_config = &self.config.consensus.programmable_data;
        let chunk_size = self.config.consensus.chunk_size;

        let base_fee_irys = extract_pd_base_fee_from_block(
            &irys_block,
            &evm_block,
            pd_config,
            chunk_size,
        )?;

        // Convert to USD
        let ema_price = ema_snapshot.ema_for_public_pricing();
        let base_fee_usd = convert_irys_to_usd(base_fee_irys, &ema_price)?;

        Ok(PdBaseFeeResponse {
            base_fee_per_chunk: base_fee_irys,
            base_fee_usd,
            block_height,
            block_hash,
            ema_price,
        })
    }

    /// Estimate future PD base fees using moving average utilization.
    pub fn estimate_base_fee(&self, blocks_ahead: u64) -> eyre::Result<PdBaseFeeEstimate> {
        let max_chunks = self.config.consensus.programmable_data.max_pd_chunks_per_block;

        // Get recent blocks from canonical chain (acquire lock once, clone needed data)
        let blocks_to_analyze: Vec<_> = {
            let tree = self.block_tree.read();
            let (canonical, _) = tree.get_canonical_chain();

            canonical
                .iter()
                .rev()
                .take(ANALYSIS_BLOCKS)
                .filter_map(|entry| {
                    tree.get_block(&entry.block_hash).map(|b| b.clone())
                })
                .collect()
        }; // Lock released here

        // Calculate moving average utilization from recent blocks (deterministic fixed-point)
        let mut total_utilization = Amount::<Percentage>::new(irys_types::U256::from(0));
        let mut analyzed_count = 0;

        for irys_block in &blocks_to_analyze {
            // Get EVM block - propagate error if not found (data consistency issue)
            let evm_block = self
                .reth_provider
                .provider
                .block_by_hash(irys_block.evm_block_hash)?
                .ok_or_eyre(format!(
                    "EVM block {} not found for Irys block {} (height {})",
                    irys_block.evm_block_hash,
                    irys_block.block_hash,
                    irys_block.height
                ))?;

            // Count PD chunks and calculate utilization
            let pd_chunks_used = count_pd_chunks_in_block(&evm_block);
            let utilization = if max_chunks > 0 {
                // Calculate: (pd_chunks_used * PRECISION_SCALE) / max_chunks
                let percent_amount = mul_div(
                    irys_types::U256::from(pd_chunks_used),
                    PRECISION_SCALE,
                    irys_types::U256::from(max_chunks),
                )?;
                Amount::<Percentage>::new(percent_amount)
            } else {
                Amount::<Percentage>::new(irys_types::U256::from(0))
            };

            total_utilization = Amount::new(
                total_utilization.amount.checked_add(utilization.amount)
                    .ok_or_eyre("Overflow adding utilization")?
            );
            analyzed_count += 1;
        }

        if analyzed_count == 0 {
            eyre::bail!(
                "Unable to analyze recent blocks for utilization (checked {} blocks from canonical chain)",
                blocks_to_analyze.len()
            );
        }

        let avg_utilization_percent = {
            let avg_amount = mul_div(
                total_utilization.amount,
                irys_types::U256::from(1),
                irys_types::U256::from(analyzed_count),
            )?;
            Amount::<Percentage>::new(avg_amount)
        };

        // Get current block and base fee
        let (current_height, evm_block_hash, ema_snapshot, irys_block) = {
            let tree = self.block_tree.read();
            let tip_hash = tree.tip;

            let irys_block = tree
                .get_block(&tip_hash)
                .ok_or_eyre(format!("No block found at tip {}", tip_hash))?
                .clone();

            let ema_snapshot = tree
                .get_ema_snapshot(&tip_hash)
                .ok_or_eyre(format!("No EMA snapshot found for tip block {}", tip_hash))?
                .clone();

            (
                irys_block.height,
                irys_block.evm_block_hash,
                ema_snapshot,
                irys_block,
            )
        }; // Lock released here

        // Get EVM block (outside lock scope)
        let evm_block = self
            .reth_provider
            .provider
            .block_by_hash(evm_block_hash)?
            .ok_or_eyre(format!("EVM block {} not found", evm_block_hash))?;

        // Extract PD base fee
        let pd_config = &self.config.consensus.programmable_data;
        let chunk_size = self.config.consensus.chunk_size;

        let current_base_fee = extract_pd_base_fee_from_block(
            &irys_block,
            &evm_block,
            pd_config,
            chunk_size,
        )?;

        let ema_price = ema_snapshot.ema_for_public_pricing();

        // Simulate future fees (simplified: assume constant EMA price)
        let mut estimates = Vec::new();
        let mut current_fee = current_base_fee;
        let pd_config = &self.config.consensus.programmable_data;
        let chunk_size = self.config.consensus.chunk_size;

        for i in 1..=blocks_ahead {
            // Simulate the adjustment using moving average utilization
            // Convert percentage back to chunks: (utilization * max_chunks) / PRECISION_SCALE
            let chunks_used = {
                let chunks_u256 = mul_div(
                    avg_utilization_percent.amount,
                    irys_types::U256::from(max_chunks),
                    PRECISION_SCALE,
                )?;
                u32::try_from(chunks_u256)
                    .map_err(|_| eyre::eyre!("Chunks used exceeds u32::MAX"))?
            };

            // Calculate new fee using the same function used in block production
            let new_fee_irys = calculate_pd_base_fee_for_new_block(
                &ema_snapshot,
                &ema_price,  // Assume constant EMA for simplicity
                chunks_used,
                current_fee,
                pd_config,
                chunk_size,
            )?;

            let new_fee_usd = convert_irys_to_usd(new_fee_irys, &ema_price)?;

            estimates.push(PdFeeAtHeight {
                block_height: current_height + i,
                base_fee_per_chunk: new_fee_irys,
                base_fee_usd: new_fee_usd,
            });

            current_fee = new_fee_irys;
        }

        Ok(PdBaseFeeEstimate {
            estimates,
            methodology: "moving_average_utilization".to_string(),
            avg_utilization_percent,
            analysis_period_blocks: analyzed_count as u64,
        })
    }

    /// Estimate priority fees based on historical data.
    ///
    /// Analyzes recent PD transactions to calculate low (25th percentile),
    /// medium (50th percentile), and high (75th percentile) priority fees.
    ///
    /// # Arguments
    /// * `target_blocks` - Number of recent blocks to analyze for priority fee percentiles (1-100)
    ///   - API layer enforces this range
    ///   - If canonical chain has fewer blocks, all available blocks are used
    ///   - Result includes actual block count in `analysis_period_blocks`
    ///
    /// # Returns
    /// * `Ok(PdPriorityFeeEstimate)` with percentile-based fee recommendations
    /// * `Err(_)` if no PD transactions found in analysis period
    ///
    /// # Example
    /// ```ignore
    /// let estimate = pd_pricing.estimate_priority_fee(20)?;
    /// // Returns percentiles from last 20 blocks (or fewer if chain is shorter)
    /// ```
    pub fn estimate_priority_fee(&self, target_blocks: u64) -> eyre::Result<PdPriorityFeeEstimate> {
        let max_chunks = self.config.consensus.programmable_data.max_pd_chunks_per_block;

        // Get canonical chain from block tree (acquire lock once)
        let blocks_to_analyze: Vec<_> = {
            let tree = self.block_tree.read();
            let (canonical, _) = tree.get_canonical_chain();

            canonical
                .iter()
                .rev()
                // Safe: API validates target_blocks ∈ [1, 100]
                .take(target_blocks as usize)
                .filter_map(|entry| {
                    tree.get_block(&entry.block_hash).map(|b| b.clone())
                })
                .collect()
        }; // Lock released here

        // Collect priority fees and utilization from recent blocks (deterministic fixed-point)
        let mut all_priority_fees = Vec::new();
        let mut total_utilization = Amount::<Percentage>::new(irys_types::U256::from(0));
        let mut analyzed_count = 0;

        for irys_block in &blocks_to_analyze {
            // Get EVM block - propagate error if not found (data consistency issue)
            let evm_block = self
                .reth_provider
                .provider
                .block_by_hash(irys_block.evm_block_hash)?
                .ok_or_eyre(format!(
                    "EVM block {} not found for Irys block {} (height {})",
                    irys_block.evm_block_hash,
                    irys_block.block_hash,
                    irys_block.height
                ))?;

            // Calculate utilization for this block
            let pd_chunks_used = count_pd_chunks_in_block(&evm_block);
            let utilization = if max_chunks > 0 {
                // Calculate: (pd_chunks_used * PRECISION_SCALE) / max_chunks
                let percent_amount = mul_div(
                    irys_types::U256::from(pd_chunks_used),
                    PRECISION_SCALE,
                    irys_types::U256::from(max_chunks),
                )?;
                Amount::<Percentage>::new(percent_amount)
            } else {
                Amount::<Percentage>::new(irys_types::U256::from(0))
            };

            total_utilization = Amount::new(
                total_utilization.amount.checked_add(utilization.amount)
                    .ok_or_eyre("Overflow adding utilization")?
            );
            analyzed_count += 1;

            // Extract priority fees from PD transactions
            let block_fees = extract_priority_fees_from_block(&evm_block);
            all_priority_fees.extend(block_fees);
        }

        // Check if we have enough data
        if all_priority_fees.is_empty() {
            eyre::bail!(
                "No PD transactions found in {} analyzed blocks (canonical chain has {} blocks)",
                analyzed_count,
                blocks_to_analyze.len()
            );
        }

        // Calculate average utilization
        let avg_utilization_percent = if analyzed_count > 0 {
            let avg_amount = mul_div(
                total_utilization.amount,
                irys_types::U256::from(1),
                irys_types::U256::from(analyzed_count),
            )?;
            Amount::<Percentage>::new(avg_amount)
        } else {
            Amount::<Percentage>::new(irys_types::U256::from(0))
        };

        // Sort fees for percentile calculation
        let mut sorted_fees = all_priority_fees.clone();
        sorted_fees.sort_by(|a, b| a.amount.cmp(&b.amount));

        // Safe percentile calculation with bounds checking
        let get_percentile = |pct: usize| -> Amount<(CostPerChunk, Irys)> {
            let index = (sorted_fees.len() * pct / 100).min(sorted_fees.len().saturating_sub(1));
            sorted_fees[index]
        };

        let percentile_25 = get_percentile(25);
        let percentile_50 = get_percentile(50);
        let percentile_75 = get_percentile(75);

        // Get current EMA price for USD conversion
        let ema_price = {
            let tree = self.block_tree.read();
            let tip_hash = tree.tip;
            let ema_snapshot = tree
                .get_ema_snapshot(&tip_hash)
                .ok_or_eyre("No EMA snapshot found for tip block")?
                .clone();
            ema_snapshot.ema_for_public_pricing()
        };

        // Convert to USD
        let percentile_25_usd = convert_irys_to_usd(percentile_25, &ema_price)?;
        let percentile_50_usd = convert_irys_to_usd(percentile_50, &ema_price)?;
        let percentile_75_usd = convert_irys_to_usd(percentile_75, &ema_price)?;

        // Calculate confidence based on both sample size and window size
        let sample_confidence = Confidence::from_sample_size(all_priority_fees.len());
        let window_confidence = Confidence::from_window_size(target_blocks);
        let confidence = sample_confidence.min(window_confidence);

        Ok(PdPriorityFeeEstimate {
            low_priority: percentile_25,
            low_priority_usd: percentile_25_usd,
            medium_priority: percentile_50,
            medium_priority_usd: percentile_50_usd,
            high_priority: percentile_75,
            high_priority_usd: percentile_75_usd,
            confidence,
            avg_utilization_percent,
            analysis_period_blocks: analyzed_count as u64,
            sample_size: all_priority_fees.len(),
        })
    }

    /// Get historical PD base fees.
    pub fn get_base_fee_history(&self, blocks: u64) -> eyre::Result<PdBaseFeeHistory> {
        // Get canonical chain and block data from block tree (acquire lock once)
        let blocks_with_ema: Vec<_> = {
            let tree = self.block_tree.read();
            let (canonical, _) = tree.get_canonical_chain();

            let total_available = canonical.len();
            if total_available == 0 {
                tracing::warn!("No canonical blocks available for base fee history");
                return Ok(PdBaseFeeHistory {
                    history: Vec::new(),
                    blocks_returned: 0,
                });
            }

            let blocks_to_fetch = std::cmp::min(blocks as usize, total_available);

            canonical
                .iter()
                .rev()
                .take(blocks_to_fetch)
                .filter_map(|entry| {
                    let block = tree.get_block(&entry.block_hash)?.clone();
                    let ema = tree.get_ema_snapshot(&entry.block_hash)?.clone();
                    Some((block, ema))
                })
                .collect()
        }; // Lock released here

        // Walk through blocks and build history
        let mut history = Vec::new();

        for (irys_block, ema_snapshot) in blocks_with_ema {
            // Get EVM block - propagate error if not found (data consistency issue)
            let evm_block = self
                .reth_provider
                .provider
                .block_by_hash(irys_block.evm_block_hash)?
                .ok_or_eyre(format!(
                    "EVM block {} not found for Irys block {} (height {})",
                    irys_block.evm_block_hash,
                    irys_block.block_hash,
                    irys_block.height
                ))?;

            // Extract PD base fee
            let pd_config = &self.config.consensus.programmable_data;
            let chunk_size = self.config.consensus.chunk_size;

            let base_fee_irys = extract_pd_base_fee_from_block(
                &irys_block,
                &evm_block,
                pd_config,
                chunk_size,
            )?;

            // Count PD chunks used
            let pd_chunks_used = count_pd_chunks_in_block(&evm_block);

            // Calculate utilization (deterministic fixed-point arithmetic)
            let max_chunks = self.config.consensus.programmable_data.max_pd_chunks_per_block;
            let utilization_percent = if max_chunks > 0 {
                // Calculate: (pd_chunks_used * PRECISION_SCALE) / max_chunks
                let percent_amount = mul_div(
                    irys_types::U256::from(pd_chunks_used),
                    PRECISION_SCALE,
                    irys_types::U256::from(max_chunks),
                )?;
                Amount::<Percentage>::new(percent_amount)
            } else {
                Amount::<Percentage>::new(irys_types::U256::from(0))
            };

            // Convert to USD
            let ema_price = ema_snapshot.ema_for_public_pricing();
            let base_fee_usd = convert_irys_to_usd(base_fee_irys, &ema_price)?;

            history.push(PdBaseFeeHistoryItem {
                block_height: irys_block.height,
                block_hash: irys_block.block_hash,
                base_fee_per_chunk: base_fee_irys,
                base_fee_usd,
                pd_chunks_used,
                utilization_percent,
                timestamp: irys_block.timestamp,
            });
        }

        let blocks_returned = history.len() as u64;
        Ok(PdBaseFeeHistory {
            history,
            blocks_returned,
        })
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
    /// // Returns per-block base fees, utilization, and priority fees
    /// ```
    pub fn get_fee_history(
        &self,
        block_count: u64,
        reward_percentiles: &[u8],
    ) -> eyre::Result<PdFeeHistoryResponse> {
        use self::base_fee::{count_pd_chunks_in_block, extract_pd_base_fee_from_block, calculate_pd_base_fee_for_new_block, extract_priority_fees_from_block};

        // Validate inputs
        validate_percentiles(reward_percentiles)?;

        // Get canonical blocks (acquire lock once)
        let blocks_with_ema: Vec<_> = {
            let tree = self.block_tree.read();
            let (canonical, _) = tree.get_canonical_chain();

            canonical
                .iter()
                .rev()
                .take(block_count as usize)
                .filter_map(|entry| {
                    let block = tree.get_block(&entry.block_hash)?.clone();
                    let ema = tree.get_ema_snapshot(&entry.block_hash)?.clone();
                    Some((block, ema))
                })
                .collect()
        };

        if blocks_with_ema.is_empty() {
            eyre::bail!("No blocks available for analysis");
        }

        // Process each block
        let mut base_fees = Vec::new();
        let mut utilizations = Vec::new();
        let mut priority_fees_per_block = Vec::new();
        let mut total_utilization = Amount::<Percentage>::new(irys_types::U256::from(0));
        let mut total_pd_transactions = 0;

        for (irys_block, ema_snapshot) in &blocks_with_ema {
            // Get EVM block
            let evm_block = self
                .reth_provider
                .provider
                .block_by_hash(irys_block.evm_block_hash)?
                .ok_or_eyre(format!(
                    "EVM block {} not found for Irys block {}",
                    irys_block.evm_block_hash,
                    irys_block.block_hash
                ))?;

            // Extract base fee
            let pd_config = &self.config.consensus.programmable_data;
            let chunk_size = self.config.consensus.chunk_size;
            let base_fee_irys = extract_pd_base_fee_from_block(
                irys_block,
                &evm_block,
                pd_config,
                chunk_size,
            )?;

            let ema_price = ema_snapshot.ema_for_public_pricing();
            let base_fee_usd = convert_irys_to_usd(base_fee_irys, &ema_price)?;

            base_fees.push(BlockBaseFee {
                block_height: irys_block.height,
                block_hash: irys_block.block_hash,
                timestamp: irys_block.timestamp,
                base_fee_irys,
                base_fee_usd,
            });

            // Calculate utilization (deterministic fixed-point arithmetic)
            let pd_chunks_used = count_pd_chunks_in_block(&evm_block);
            let max_pd_chunks = self.config.consensus.programmable_data.max_pd_chunks_per_block;
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

            // Accumulate total utilization for averaging
            total_utilization = Amount::new(
                total_utilization.amount.checked_add(utilization_percent.amount)
                    .ok_or_eyre("Overflow adding utilization")?
            );

            utilizations.push(BlockUtilization {
                pd_chunks_used,
                max_pd_chunks,
                utilization_percent,
            });

            // Extract priority fees and calculate percentiles
            let block_priority_fees = extract_priority_fees_from_block(&evm_block);
            total_pd_transactions += block_priority_fees.len();

            let mut percentile_map = std::collections::HashMap::new();
            if !block_priority_fees.is_empty() {
                let mut sorted_fees = block_priority_fees.clone();
                sorted_fees.sort_by(|a, b| a.amount.cmp(&b.amount));

                for &percentile in reward_percentiles {
                    let index = ((percentile as usize * sorted_fees.len()) / 100)
                        .min(sorted_fees.len().saturating_sub(1));
                    let fee_irys = sorted_fees[index];
                    let fee_usd = convert_irys_to_usd(fee_irys, &ema_price)?;

                    percentile_map.insert(
                        percentile,
                        PriorityFeeAtPercentile { fee_irys, fee_usd },
                    );
                }
            }

            priority_fees_per_block.push(BlockPriorityFees {
                percentiles: percentile_map,
                sample_size: block_priority_fees.len(),
            });
        }

        // Predict next block's base fee
        let (current_irys_block, current_ema) = blocks_with_ema.first()
            .ok_or_eyre("No blocks available")?;
        let current_evm_block = self
            .reth_provider
            .provider
            .block_by_hash(current_irys_block.evm_block_hash)?
            .ok_or_eyre("Current EVM block not found")?;

        let pd_config = &self.config.consensus.programmable_data;
        let chunk_size = self.config.consensus.chunk_size;
        let current_base_fee = extract_pd_base_fee_from_block(
            current_irys_block,
            &current_evm_block,
            pd_config,
            chunk_size,
        )?;
        let current_chunks_used = count_pd_chunks_in_block(&current_evm_block);
        let current_ema_price = current_ema.ema_for_public_pricing();

        let next_base_fee_irys = calculate_pd_base_fee_for_new_block(
            current_ema,
            &current_ema_price,
            current_chunks_used as u32,
            current_base_fee,
            pd_config,
            chunk_size,
        )?;
        let next_base_fee_usd = convert_irys_to_usd(next_base_fee_irys, &current_ema_price)?;

        // Calculate confidence
        let sample_confidence = Confidence::from_sample_size(total_pd_transactions);
        let window_confidence = Confidence::from_window_size(block_count);
        let confidence = sample_confidence.min(window_confidence);

        // Calculate average utilization (deterministic fixed-point arithmetic)
        let avg_utilization_percent = if !blocks_with_ema.is_empty() {
            let avg_amount = mul_div(
                total_utilization.amount,
                irys_types::U256::from(1),
                irys_types::U256::from(blocks_with_ema.len()),
            )?;
            Amount::<Percentage>::new(avg_amount)
        } else {
            Amount::<Percentage>::new(irys_types::U256::from(0))
        };

        // Build response
        let oldest_block = blocks_with_ema.last()
            .ok_or_eyre("No oldest block")?;

        Ok(PdFeeHistoryResponse {
            oldest_block: oldest_block.0.height,
            oldest_block_hash: oldest_block.0.block_hash,
            base_fee_per_chunk: base_fees,
            next_base_fee_per_chunk: next_base_fee_irys,
            next_base_fee_usd,
            pd_utilization: utilizations,
            priority_fees: priority_fees_per_block,
            analysis: FeeHistoryAnalysis {
                avg_utilization_percent,
                total_pd_transactions,
                priority_fee_confidence: confidence,
                ema_price: current_ema_price,
                blocks_returned: blocks_with_ema.len() as u64,
                requested_percentiles: reward_percentiles.to_vec(),
            },
        })
    }
}

// Constants

/// Number of recent blocks to analyze for utilization patterns and priority fee estimation.
const ANALYSIS_BLOCKS: usize = 20;

// Confidence Level

/// Confidence level for priority fee estimates based on sample size and analysis window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    /// Returns the minimum (most conservative) of two confidence levels.
    pub fn min(self, other: Self) -> Self {
        if self < other {
            self
        } else {
            other
        }
    }

    /// Determines confidence based on sample size (number of transactions).
    pub fn from_sample_size(sample_size: usize) -> Self {
        if sample_size >= 20 {
            Confidence::High
        } else if sample_size >= 10 {
            Confidence::Medium
        } else {
            Confidence::Low
        }
    }

    /// Determines confidence based on analysis window size (number of blocks).
    pub fn from_window_size(window_size: u64) -> Self {
        if window_size < 5 {
            Confidence::Low
        } else if window_size <= 50 {
            Confidence::High
        } else {
            Confidence::Medium
        }
    }
}

// Response Types

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PdBaseFeeResponse {
    pub base_fee_per_chunk: Amount<(CostPerChunk, Irys)>,
    pub base_fee_usd: Amount<(CostPerChunk, Usd)>,
    pub block_height: u64,
    pub block_hash: H256,
    pub ema_price: IrysTokenPrice,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PdBaseFeeEstimate {
    pub estimates: Vec<PdFeeAtHeight>,
    pub methodology: String,
    pub avg_utilization_percent: Amount<Percentage>,
    pub analysis_period_blocks: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PdFeeAtHeight {
    pub block_height: u64,
    pub base_fee_per_chunk: Amount<(CostPerChunk, Irys)>,
    pub base_fee_usd: Amount<(CostPerChunk, Usd)>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PdPriorityFeeEstimate {
    pub low_priority: Amount<(CostPerChunk, Irys)>,
    pub low_priority_usd: Amount<(CostPerChunk, Usd)>,
    pub medium_priority: Amount<(CostPerChunk, Irys)>,
    pub medium_priority_usd: Amount<(CostPerChunk, Usd)>,
    pub high_priority: Amount<(CostPerChunk, Irys)>,
    pub high_priority_usd: Amount<(CostPerChunk, Usd)>,
    pub confidence: Confidence,
    pub avg_utilization_percent: Amount<Percentage>,
    pub analysis_period_blocks: u64,
    pub sample_size: usize,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PdBaseFeeHistoryItem {
    pub block_height: u64,
    pub block_hash: H256,
    pub base_fee_per_chunk: Amount<(CostPerChunk, Irys)>,
    pub base_fee_usd: Amount<(CostPerChunk, Usd)>,
    pub pd_chunks_used: u64,
    pub utilization_percent: Amount<Percentage>,
    pub timestamp: UnixTimestampMs,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PdBaseFeeHistory {
    pub history: Vec<PdBaseFeeHistoryItem>,
    pub blocks_returned: u64,
}

// Unified Fee History Response (Ethereum eth_feeHistory style)

/// Comprehensive fee history response combining base fees, utilization, and priority fees.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PdFeeHistoryResponse {
    /// Oldest block in the range (first block analyzed)
    pub oldest_block: u64,

    /// Oldest block hash
    pub oldest_block_hash: H256,

    /// Per-block base fees (length = blockCount)
    pub base_fee_per_chunk: Vec<BlockBaseFee>,

    /// Predicted base fee for the next block (N+1)
    pub next_base_fee_per_chunk: Amount<(CostPerChunk, Irys)>,
    pub next_base_fee_usd: Amount<(CostPerChunk, Usd)>,

    /// Per-block PD utilization (length = blockCount)
    pub pd_utilization: Vec<BlockUtilization>,

    /// Per-block priority fee percentiles (length = blockCount)
    pub priority_fees: Vec<BlockPriorityFees>,

    /// Metadata about the analysis
    pub analysis: FeeHistoryAnalysis,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockBaseFee {
    pub block_height: u64,
    pub block_hash: H256,
    pub timestamp: UnixTimestampMs,
    pub base_fee_irys: Amount<(CostPerChunk, Irys)>,
    pub base_fee_usd: Amount<(CostPerChunk, Usd)>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockUtilization {
    pub pd_chunks_used: u64,
    pub max_pd_chunks: u64,
    pub utilization_percent: Amount<Percentage>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockPriorityFees {
    /// Requested percentiles with their values
    /// Key: percentile (e.g., 25, 50, 90)
    /// Value: fee amount in Irys and USD
    pub percentiles: std::collections::HashMap<u8, PriorityFeeAtPercentile>,

    /// Number of PD transactions in this block
    pub sample_size: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriorityFeeAtPercentile {
    pub fee_irys: Amount<(CostPerChunk, Irys)>,
    pub fee_usd: Amount<(CostPerChunk, Usd)>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeHistoryAnalysis {
    /// Average utilization across analyzed blocks
    pub avg_utilization_percent: Amount<Percentage>,

    /// Total PD transactions found across all blocks
    pub total_pd_transactions: usize,

    /// Confidence in priority fee estimates
    pub priority_fee_confidence: Confidence,

    /// Current EMA price used for conversions
    pub ema_price: IrysTokenPrice,

    /// Actual number of blocks returned (may be less than requested)
    pub blocks_returned: u64,

    /// Requested percentiles
    pub requested_percentiles: Vec<u8>,
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
            let floor_per_chunk_irys =
                convert_per_chunk_usd_to_irys(floor_per_chunk_usd, &irys_block_header.ema_irys_price)?;
            return Ok(floor_per_chunk_irys);
        }

        // Extract current PD base fee from block's 2nd shadow transaction (PdBaseFeeUpdate)
        // The ordering is: [0] BlockReward, [1] PdBaseFeeUpdate, [2+] other shadow txs
        let second_tx = evm_block
            .body
            .transactions
            .get(PD_BASE_FEE_INDEX)
            .ok_or_eyre("Block must have at least 2 transactions (BlockReward + PdBaseFeeUpdate)")?;

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
                let priority_fee_irys = Amount::<(CostPerChunk, Irys)>::new(header.max_priority_fee_per_chunk.into());
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
