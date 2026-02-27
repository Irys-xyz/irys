//! # Ledger Expiry Fee Distribution
//!
//! This module calculates and distributes fees to miners when data ledgers expire at epoch boundaries.
//! The primary challenge is handling transactions that span partition boundaries, requiring careful
//! filtering to ensure miners are compensated only for data they actually store.
//!
//! ## The Partition Boundary Problem
//!
//! Transactions don't align perfectly with partition boundaries. When a transaction's data size
//! doesn't divide evenly into the partition's chunk capacity, it can span multiple partitions:
//!
//! ```text
//! Partition A (chunks 0-99)   | Partition B (chunks 100-199) | Partition C (chunks 200-299)
//! ----------------------------| -----------------------------| ----------------------------
//! [Tx1: chunks 0-49]          |                              |
//! [Tx2: chunks 50-149] <------|---------> Spans A & B        |
//!                             | [Tx3: chunks 150-199]        |
//!                             | [Tx4: chunks 180-250] <------|---------> Spans B & C
//!                             |                              | [Tx5: chunks 251-299]
//! ```
//!
//! When Partition B expires, we must:
//! - Exclude Tx2 (starts in partition A - not fully contained)
//! - Include Tx3 (fully within partition B)
//! - Include Tx4 (starts in partition B - fully owned by B)
//! - Ignore Tx1 and Tx5 (not in partition B range)
//!
//! ## Detection Strategy
//!
//! 1. **Identify Boundary Blocks**: Find the earliest and latest blocks containing chunks
//!    from the expired partition. These blocks may contain transactions that extend beyond
//!    the partition boundaries.
//!
//! 2. **Track Middle Blocks**: All blocks between the boundaries contain only transactions
//!    fully within the partition range - these can be included wholesale.
//!
//! ## Filtering Logic
//!
//! ### Earliest Block
//! - Skip transactions that start before the partition boundary
//! - Include the first transaction fully contained within the partition
//! - Include all subsequent transactions in the block
//!
//! ### Latest Block
//! - Include all transactions that start within the partition
//! - Stop processing when a transaction begins after the partition end
//!
//! ### Middle Blocks
//! - Include all transactions (guaranteed to be within partition range)
//!
//! ## Algorithm Steps
//!
//! 1. **Collect Expired Partitions**: Identify which partitions have expired and their miners
//! 2. **Find Block Range**: Determine earliest, latest, and middle blocks containing partition data
//! 3. **Process Boundary Blocks**: Filter transactions at partition boundaries
//! 4. **Process Middle Blocks**: Include all transactions from middle blocks
//! 5. **Fetch Transaction Data**: Retrieve full transaction details
//! 6. **Calculate Fees**: Distribute fees proportionally among miners who stored the data

use crate::block_discovery::get_data_tx_in_parallel;
use crate::mempool_guard::MempoolReadGuard;
use crate::shadow_tx_generator::RollingHash;
use eyre::{OptionExt as _, eyre};
use irys_database::{block_header_by_hash, db::IrysDatabaseExt as _};
use irys_domain::{BlockIndex, BlockTreeReadGuard, EpochSnapshot};
use irys_types::{
    BlockHeight, BlockIndexItem, Config, DataLedger, DataTransactionHeader, H256, IrysAddress,
    IrysBlockHeader, IrysTransactionId, LedgerChunkOffset, LedgerChunkRange, U256,
    app_state::DatabaseProvider, fee_distribution::TermFeeCharges, ledger_chunk_offset_ii,
};
use nodit::{InclusiveInterval as _, interval::ii};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Calculates the aggregated fees owed to miners when data ledgers expire.
///
/// This function processes expired partitions at epoch boundaries, determines which miners
/// stored the data, and calculates the appropriate fee distributions based on the term fees
/// paid by users when submitting transactions.
///
/// # Parameters
/// - `ledger_type`: The type of ledger to process (e.g., Submit, or future expiring ledgers)
///
/// # Returns
/// LedgerExpiryBalanceDelta containing reward address to fee mappings and user refunds
#[tracing::instrument(skip_all, fields(block.height = block_height, ledger.type = ?ledger_type))]
pub async fn calculate_expired_ledger_fees(
    parent_epoch_snapshot: &EpochSnapshot,
    block_height: u64,
    ledger_type: DataLedger,
    config: &Config,
    block_index: BlockIndex,
    block_tree_guard: &BlockTreeReadGuard,
    mempool_guard: &MempoolReadGuard,
    db: &DatabaseProvider,
    expect_txs_to_be_promoted: bool,
) -> eyre::Result<LedgerExpiryBalanceDelta> {
    // Step 1: Collect expired partitions
    let expired_slots =
        collect_expired_partitions(parent_epoch_snapshot, block_height, ledger_type)?;

    tracing::info!(
        "Ledger expiry check at block {}: found {} expired slots for {:?} ledger",
        block_height,
        expired_slots.len(),
        ledger_type
    );

    if expired_slots.is_empty() {
        tracing::debug!(
            "No expired partitions for {:?} ledger at block {}",
            ledger_type,
            block_height
        );
        return Ok(LedgerExpiryBalanceDelta::default());
    }

    // Step 2: Find block ranges
    let block_range = match find_block_range(expired_slots, config, &block_index, ledger_type)? {
        Some(br) => br,
        None => {
            // Check to see if there were no chunks uploaded to this ledger slot!
            // If there wasn't, there aren't any fees to distribute
            return Ok(LedgerExpiryBalanceDelta::default());
        }
    };

    // Step 3: Process boundary blocks
    let same_block = block_range.min_block.item.block_hash == block_range.max_block.item.block_hash;
    tracing::info!(
        "Processing boundary blocks: min_block height={}, max_block height={}, same_block={}",
        block_range.min_block.height,
        block_range.max_block.height,
        same_block
    );

    let earliest_miners;
    let latest_miners;

    if same_block {
        // When min and max are the same block, process it only once to avoid double-counting
        // Process as earliest block (will include all transactions in the partition range)
        let miners = process_boundary_block(
            &block_range.min_block,
            block_range.min_block.item.block_hash,
            Arc::clone(&block_range.min_block_miners),
            true, // is_earliest
            ledger_type,
            config,
            &block_index,
            block_tree_guard,
            mempool_guard,
            db,
        )
        .await?;

        earliest_miners = miners;
        latest_miners = BTreeMap::new();
    } else {
        // Different blocks - process both boundaries
        let e_miners = process_boundary_block(
            &block_range.min_block,
            block_range.min_block.item.block_hash,
            Arc::clone(&block_range.min_block_miners),
            true, // is_earliest
            ledger_type,
            config,
            &block_index,
            block_tree_guard,
            mempool_guard,
            db,
        )
        .await?;

        let l_miners = process_boundary_block(
            &block_range.max_block,
            block_range.max_block.item.block_hash,
            Arc::clone(&block_range.max_block_miners),
            false, // is_earliest
            ledger_type,
            config,
            &block_index,
            block_tree_guard,
            mempool_guard,
            db,
        )
        .await?;

        earliest_miners = e_miners;
        latest_miners = l_miners;
    }

    // Step 4: Process middle blocks
    let middle_miners =
        process_middle_blocks(block_range.middle_blocks, ledger_type, block_tree_guard, db).await?;

    // Step 5: Combine all transactions
    let mut all_tx_ids = Vec::new();
    all_tx_ids.extend(earliest_miners.keys());
    all_tx_ids.extend(latest_miners.keys());
    all_tx_ids.extend(middle_miners.keys());

    tracing::info!(
        "Collected transactions: earliest={}, latest={}, middle={}, total={}",
        earliest_miners.len(),
        latest_miners.len(),
        middle_miners.len(),
        all_tx_ids.len()
    );

    let mut tx_to_miners = BTreeMap::new();
    tx_to_miners.extend(earliest_miners);
    tx_to_miners.extend(latest_miners);
    tx_to_miners.extend(middle_miners);

    // Step 6: Fetch transactions
    let mut transactions = get_data_tx_in_parallel(all_tx_ids, mempool_guard, db).await?;
    transactions.sort_by(irys_types::DataTransactionHeader::compare_tx);

    // Step 7: Calculate fees
    tracing::debug!(
        "Processing {} transactions for fee distribution to {} unique miners",
        transactions.len(),
        tx_to_miners
            .values()
            .flat_map(|v| v.iter())
            .collect::<std::collections::HashSet<_>>()
            .len()
    );

    let fees = aggregate_balance_deltas(
        transactions,
        &tx_to_miners,
        parent_epoch_snapshot,
        config,
        expect_txs_to_be_promoted,
    )?;

    let total_fees = fees
        .reward_balance_increment
        .values()
        .fold(U256::from(0), |acc, (fee, _)| acc.saturating_add(*fee));

    tracing::info!(
        "Calculated fees for {} reward addresses, total fees: {}",
        fees.reward_balance_increment.len(),
        total_fees
    );

    Ok(fees)
}

/// Fetches a block header from block tree or database
#[tracing::instrument(level = "trace", skip_all, fields(block.hash = %block_hash))]
async fn get_block_by_hash(
    block_hash: H256,
    block_tree_guard: &BlockTreeReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<IrysBlockHeader> {
    // Check block tree first
    if let Some(header) = block_tree_guard.read().get_block(&block_hash).cloned() {
        return Ok(header);
    }
    // Fallback to database
    db.view_eyre(|tx| block_header_by_hash(tx, &block_hash, false))?
        .ok_or_eyre("block not found in db")
}

/// Collects all expired partitions for the specified ledger type and their miners
#[tracing::instrument(skip_all, fields(block_height, target_ledger_type))]
fn collect_expired_partitions(
    parent_epoch_snapshot: &EpochSnapshot,
    block_height: u64,
    target_ledger_type: DataLedger,
) -> eyre::Result<BTreeMap<SlotIndex, Vec<IrysAddress>>> {
    let partition_assignments = &parent_epoch_snapshot.partition_assignments;
    let expired_partition_info = &parent_epoch_snapshot.get_expiring_partition_info(block_height);
    let mut expired_ledger_slot_indexes = BTreeMap::new();
    if expired_partition_info.is_empty() {
        return Ok(expired_ledger_slot_indexes);
    }

    tracing::debug!(
        "collect_expired_partitions: block_height={}, target_ledger={:?}, found {} expired partition hashes",
        block_height,
        target_ledger_type,
        expired_partition_info.len()
    );

    for expired_partition in expired_partition_info {
        let partition = partition_assignments
            .get_assignment(expired_partition.partition_hash)
            .ok_or_eyre("could not get expired partition")?;

        let ledger_id = expired_partition.ledger_id;
        let slot_index = SlotIndex::new(expired_partition.slot_index as u64);

        // Only process partitions for the target ledger type
        if ledger_id == target_ledger_type {
            // Verify this ledger type can expire
            if ledger_id == DataLedger::Publish {
                eyre::bail!("publish ledger cannot expire");
            }

            tracing::info!(
                "Found expired partition for {:?} ledger at slot_index={}, miner={:?}",
                ledger_id,
                slot_index.0,
                partition.miner_address
            );

            // Store miner_address (not reward_address) to preserve unique miner identities
            // for correct fee distribution. Reward address resolution is deferred to
            // aggregate_balance_deltas to ensure pooled miners (sharing a reward address)
            // are counted individually for fee splitting.
            expired_ledger_slot_indexes
                .entry(slot_index)
                .and_modify(|miners: &mut Vec<IrysAddress>| {
                    miners.push(partition.miner_address);
                })
                .or_insert(vec![partition.miner_address]);
        } else {
            tracing::debug!(
                "Skipping partition with ledger_id={:?} (looking for {:?})",
                ledger_id,
                target_ledger_type
            );
        }
    }

    Ok(expired_ledger_slot_indexes)
}

/// Finds all blocks containing data in the expired chunk ranges
fn find_block_range(
    expired_slots: BTreeMap<SlotIndex, Vec<IrysAddress>>,
    config: &Config,
    block_index: &BlockIndex,
    ledger_type: DataLedger,
) -> eyre::Result<Option<BlockRange>> {
    let mut blocks_with_expired_ledgers = BTreeMap::new();

    // Ensure that we don't start reading a partition that's only partially populated
    let last_item = block_index
        .get_latest_item()
        .expect("expected block index to contain at least one item");
    let max_chunk_offset_across_all_partitions =
        LedgerChunkOffset::from(last_item.ledgers[ledger_type].total_chunks);

    // Track min and max blocks as we iterate
    let mut min_height: Option<(BlockHeight, BlockIndexItem, LedgerChunkRange)> = None;
    let mut max_height: Option<(BlockHeight, BlockIndexItem, LedgerChunkRange)> = None;

    for (slot_index, miners) in expired_slots {
        let chunk_range = slot_index.compute_chunk_range(
            config.consensus.num_chunks_in_partition,
            max_chunk_offset_across_all_partitions,
        );

        let mut chunk_offset = *chunk_range.start();
        while chunk_offset < *chunk_range.end() {
            let (height, block_index_item) =
                block_index.get_block_index_item(ledger_type, chunk_offset)?;

            // Update min_height if this is the first block or a lower height
            if min_height.as_ref().is_none_or(|(h, _, _)| height < *h) {
                min_height = Some((height, block_index_item.clone(), chunk_range));
            }

            // Update max_height if this is the first block or a higher height
            if max_height.as_ref().is_none_or(|(h, _, _)| height > *h) {
                max_height = Some((height, block_index_item.clone(), chunk_range));
            }

            // If the block already exists, merge the miners
            blocks_with_expired_ledgers
                .entry(block_index_item.block_hash)
                .and_modify(|existing_miners: &mut Arc<Vec<IrysAddress>>| {
                    // Merge the new miners with existing ones
                    let mut combined = (**existing_miners).clone();
                    combined.extend(miners.clone());
                    *existing_miners = Arc::new(combined);
                })
                .or_insert_with(|| Arc::new(miners.clone()));

            // Skip to the next chunk after this block ends.
            // We do this by going to the very end of the current blocks max chunk offset
            chunk_offset =
                (block_index_item.ledgers[ledger_type].total_chunks + 1).min(*chunk_range.end());
        }
    }

    // Double check to see if there were any chunks added to this partition requiring rewards
    // (This should cause the min_height and max_height to be the same resulting in no fee distribution)
    if min_height.is_none() && max_height.is_none() {
        return Ok(None);
    }

    // Extract min and max block data - these must exist if we have expired slots
    let (min_height, min_item, min_range) =
        min_height.expect("min_height must be populated after iterating expired slots");
    let (max_height, max_item, max_range) =
        max_height.expect("max_height must be populated after iterating expired slots");

    let min_block = BoundaryBlock {
        height: min_height,
        item: min_item,
        chunk_range: min_range,
    };

    let max_block = BoundaryBlock {
        height: max_height,
        item: max_item,
        chunk_range: max_range,
    };

    // Get miners for boundary blocks before removing them
    let min_block_miners = blocks_with_expired_ledgers
        .remove(&min_block.item.block_hash)
        .unwrap_or_else(|| Arc::new(vec![]));

    let max_block_miners = blocks_with_expired_ledgers
        .remove(&max_block.item.block_hash)
        .unwrap_or_else(|| Arc::new(vec![]));

    Ok(Some(BlockRange {
        min_block,
        max_block,
        min_block_miners,
        max_block_miners,
        middle_blocks: blocks_with_expired_ledgers,
    }))
}

/// Helper to get the previous block's max chunk offset
fn get_previous_max_offset(
    block_index_guard: &BlockIndex,
    block_height: BlockHeight,
    ledger_type: DataLedger,
) -> eyre::Result<LedgerChunkOffset> {
    if block_height == 0 {
        Ok(LedgerChunkOffset::from(0))
    } else {
        let prev_height = block_height - 1;
        Ok(LedgerChunkOffset::from(
            block_index_guard
                .get_item(prev_height)
                .ok_or_eyre("previous block must exist")?
                .ledgers[ledger_type]
                .total_chunks,
        ))
    }
}

/// Processes transactions from a boundary block (first or last).
///
/// Boundary blocks require special handling because they may contain transactions
/// that extend beyond the partition boundaries. This function:
/// 1. Fetches the block's transactions
/// 2. Sorts them to match their on-chain order
/// 3. Applies filtering based on whether it's the earliest or latest block
async fn process_boundary_block(
    boundary: &BoundaryBlock,
    block_hash: H256,
    miners: Arc<Vec<IrysAddress>>,
    is_earliest: bool,
    ledger_type: DataLedger,
    config: &Config,
    block_index: &BlockIndex,
    block_tree_guard: &BlockTreeReadGuard,
    mempool_guard: &MempoolReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<BTreeMap<IrysTransactionId, Arc<Vec<IrysAddress>>>> {
    // Get the block and its transactions
    let block = get_block_by_hash(block_hash, block_tree_guard, db).await?;
    let ledger_tx_ids = block
        .get_data_ledger_tx_ids_ordered(ledger_type)
        .ok_or_eyre(format!(
            "{:?} ledger is required for expired blocks",
            ledger_type
        ))?;

    // Fetch the actual transactions
    // Note: get_data_tx_in_parallel preserves the order of input IDs
    let ledger_data_txs =
        get_data_tx_in_parallel(ledger_tx_ids.to_vec(), mempool_guard, db).await?;

    // Get the previous block's max offset
    let prev_max_offset = get_previous_max_offset(block_index, boundary.height, ledger_type)?;

    // Filter transactions based on chunk positions
    let filtered_txs = filter_transactions_by_chunk_range(
        ledger_data_txs,
        prev_max_offset,
        boundary.chunk_range,
        is_earliest,
        config.consensus.chunk_size,
        miners,
    );

    Ok(filtered_txs)
}

/// Filters transactions based on their chunk positions relative to partition boundaries.
///
/// This is the core logic for handling transaction overlaps at partition boundaries.
/// Transactions are processed sequentially, tracking their cumulative chunk positions.
///
/// # Boundary Handling
///
/// - **Earliest block**: Skips transactions that start before the partition boundary,
///   only including transactions fully contained within the partition
/// - **Latest block**: Includes all transactions that start within the partition,
///   even if they extend beyond the partition end
///
/// # Returns
///
/// mapping of tx ID to miners who stored it
#[tracing::instrument(level = "trace", skip_all, fields(
    chunk.prev_max_offset = %prev_max_offset,
    chunk.partition_start = %partition_range.start(),
    chunk.partition_end = %partition_range.end(),
    tx.count = transactions.len(),
    boundary.is_earliest = is_earliest
))]
fn filter_transactions_by_chunk_range(
    transactions: Vec<DataTransactionHeader>,
    prev_max_offset: LedgerChunkOffset,
    partition_range: LedgerChunkRange,
    is_earliest: bool,
    chunk_size: u64,
    miners: Arc<Vec<IrysAddress>>,
) -> BTreeMap<IrysTransactionId, Arc<Vec<IrysAddress>>> {
    let mut current_offset = prev_max_offset;
    let mut tx_to_miners = BTreeMap::new();

    tracing::info!(
        "Filtering {} transactions: is_earliest={}, prev_max_offset={}, partition_range=[{}, {}]",
        transactions.len(),
        is_earliest,
        *prev_max_offset,
        *partition_range.start(),
        *partition_range.end()
    );

    if !miners.is_empty() {
        for (idx, tx) in transactions.iter().enumerate() {
            let chunks = tx.data_size.div_ceil(chunk_size);
            let tx_start = current_offset;
            let tx_end = current_offset + chunks;

            tracing::debug!(
                "Tx {}: id={}, data_size={}, chunks={}, tx_start={}, tx_end={}",
                idx,
                tx.id,
                tx.data_size,
                chunks,
                *tx_start,
                *tx_end
            );

            if is_earliest {
                // For earliest block: skip transactions that start before the partition
                // We only include transactions fully contained within the partition
                if tx_start < partition_range.start() {
                    tracing::debug!("  Skipping (starts before partition)");
                    current_offset = tx_end;
                    continue;
                }
            } else {
                // For latest block: stop when we reach a transaction that starts at or after the partition end
                // We use >= because a transaction starting exactly at the end belongs to the next partition
                if tx_start >= partition_range.end() {
                    tracing::debug!("  Breaking (starts at or after partition end)");
                    break;
                }
            }

            // Include this transaction
            tracing::debug!("  Including transaction");
            tx_to_miners.insert(tx.id, Arc::clone(&miners));
            current_offset = tx_end;
        }
    }

    tracing::info!("Filtered to {} transactions", tx_to_miners.len());
    tx_to_miners
}

/// Processes all middle blocks (non-boundary blocks)
async fn process_middle_blocks(
    middle_blocks: BTreeMap<H256, Arc<Vec<IrysAddress>>>,
    ledger_type: DataLedger,
    block_tree_guard: &BlockTreeReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<BTreeMap<IrysTransactionId, Arc<Vec<IrysAddress>>>> {
    let mut tx_to_miners = BTreeMap::new();

    for (block_hash, miners) in middle_blocks {
        let block = get_block_by_hash(block_hash, block_tree_guard, db).await?;
        let ledger_tx_ids = block
            .get_data_ledger_tx_ids_ordered(ledger_type)
            .ok_or_eyre(format!("{:?} ledger is required", ledger_type))?;

        for tx_id in ledger_tx_ids.iter() {
            tx_to_miners.insert(*tx_id, Arc::clone(&miners));
        }
    }

    Ok(tx_to_miners)
}

/// Represents balance changes resulting from ledger expiry at epoch boundaries.
///
/// This struct tracks two types of balance adjustments:
/// - Miner rewards for storing expired data (term fees distributed to storage providers)
/// - User refunds for permanent fees when transactions were not promoted to permanent storage
#[derive(Debug, Default, Clone)]
pub struct LedgerExpiryBalanceDelta {
    /// Rewards for miners who stored the expired data, keyed by reward/payout address.
    /// The tuple contains (total_reward, rolling_hash_of_tx_ids).
    pub reward_balance_increment: BTreeMap<IrysAddress, (U256, RollingHash)>,

    /// Refunds of permanent fees for users whose transactions were not promoted.
    /// Sorted by transaction ID. Each tuple contains (transaction_id, refund_amount, user_address).
    pub user_perm_fee_refunds: Vec<(IrysTransactionId, U256, IrysAddress)>,
}

/// Calculates and aggregates fees for each miner
///
/// # Parameters
/// - `transactions`: The transactions to process
/// - `tx_to_miners`: Mapping of transaction IDs to miner addresses that stored them
/// - `epoch_snapshot`: Used to resolve miner addresses to reward addresses at payout time
/// - `config`: Node configuration
/// - `expect_txs_to_be_promoted`: Whether transactions are expected to be promoted
fn aggregate_balance_deltas(
    mut transactions: Vec<DataTransactionHeader>,
    tx_to_miners: &BTreeMap<IrysTransactionId, Arc<Vec<IrysAddress>>>,
    epoch_snapshot: &EpochSnapshot,
    config: &Config,
    expect_txs_to_be_promoted: bool,
) -> eyre::Result<LedgerExpiryBalanceDelta> {
    let mut balance_delta = LedgerExpiryBalanceDelta::default();
    transactions.sort_by(irys_types::DataTransactionHeader::compare_tx); // This ensures refunds will be sorted by tx_id

    for data_tx in transactions.iter() {
        let miners_that_stored_this_tx = tx_to_miners
            .get(&data_tx.id)
            .ok_or_else(|| eyre!("Missing miner list for transaction {}", data_tx.id))?;

        // process miner balance increments for storing the term tx
        {
            // Deduplicate miners - each address should only get one share
            let unique_miners: Vec<IrysAddress> = miners_that_stored_this_tx
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();

            let fee_charges = TermFeeCharges::new(data_tx.term_fee, &config.consensus)?;
            let fee_distribution_per_miner = fee_charges.distribution_on_expiry(&unique_miners)?;

            for (miner, fee) in unique_miners.iter().zip(fee_distribution_per_miner) {
                // Resolve miner_address to reward_address at payout time.
                // This ensures pooled miners (multiple miner addresses sharing a reward address)
                // are counted individually for fee splitting, then aggregated at payout.
                let reward_addr = epoch_snapshot.resolve_reward_address(*miner);
                balance_delta
                    .reward_balance_increment
                    .entry(reward_addr)
                    .and_modify(|(current_fee, hash)| {
                        *current_fee = current_fee.saturating_add(fee);
                        hash.xor_assign(U256::from_le_bytes(data_tx.id.0));
                    })
                    .or_insert((fee, RollingHash(U256::from_le_bytes(data_tx.id.0))));
            }
        }

        if !expect_txs_to_be_promoted {
            continue;
        }

        // process refunds of perm fee if the tx was not promoted
        {
            if data_tx.promoted_height().is_none() {
                // Only process refund if perm_fee exists (should always be present if tx is expected to be promoted)
                let perm_fee = data_tx
                    .perm_fee
                    .ok_or_eyre("unpromoted tx should have the prem fee present")?;
                // Add refund to the vector (already sorted by tx_id due to transaction sorting)
                balance_delta.user_perm_fee_refunds.push((
                    data_tx.id,
                    perm_fee.get(),
                    data_tx.signer,
                ));
            } else {
                tracing::debug!(
                    tx.id = ?data_tx.id,
                    tx.promoted_height = ?data_tx.promoted_height(),
                    "Tx was promoted, no refund needed",
                );
            }
        }
    }

    Ok(balance_delta)
}

/// Represents a slot index in the partition system
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct SlotIndex(u64);

impl SlotIndex {
    fn new(value: u64) -> Self {
        Self(value)
    }

    fn compute_chunk_range(
        &self,
        chunks_per_partition: u64,
        max_offset: LedgerChunkOffset,
    ) -> LedgerChunkRange {
        let start = LedgerChunkOffset::from(self.0 * chunks_per_partition).min(max_offset);
        let end = start + chunks_per_partition;
        let end = end.min(max_offset);
        LedgerChunkRange(ledger_chunk_offset_ii!(start, end))
    }
}

/// Encapsulates information about a boundary block
#[derive(Debug, Clone)]
struct BoundaryBlock {
    height: BlockHeight,
    item: BlockIndexItem,
    chunk_range: LedgerChunkRange,
}

/// Tracks the range of blocks containing expired partition data
struct BlockRange {
    min_block: BoundaryBlock,
    max_block: BoundaryBlock,
    min_block_miners: Arc<Vec<IrysAddress>>,
    max_block_miners: Arc<Vec<IrysAddress>>,
    middle_blocks: BTreeMap<H256, Arc<Vec<IrysAddress>>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_domain::StakeEntry;
    use irys_types::{CommitmentStatus, DataTransactionHeaderV1};

    #[test]
    fn test_aggregate_miner_fees_handles_duplicates() {
        // Setup config
        let node_config = irys_types::NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);

        // Create test transactions
        let tx1 = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: H256::random(),
                term_fee: U256::from(1000).into(),
                data_size: 100,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });

        let tx2 = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: H256::random(),
                term_fee: U256::from(2000).into(),
                data_size: 200,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });

        // Create miners with duplicates
        let miner1 = IrysAddress::random();
        let miner2 = IrysAddress::random();

        // For tx1: miner1 appears twice (duplicate)
        let tx1_miners_with_dup = vec![miner1, miner2, miner1];

        // For tx2: only unique miners
        let tx2_miners = vec![miner1, miner2];

        // Create tx_to_miners mapping
        let mut tx_to_miners = BTreeMap::new();
        tx_to_miners.insert(tx1.id, Arc::new(tx1_miners_with_dup));
        tx_to_miners.insert(tx2.id, Arc::new(tx2_miners));

        // Create a default epoch snapshot (resolve_reward_address will return miner_address
        // unchanged when there's no stake entry)
        let epoch_snapshot = EpochSnapshot::default();

        // Call aggregate_miner_fees
        let result = aggregate_balance_deltas(
            vec![tx1, tx2],
            &tx_to_miners,
            &epoch_snapshot,
            &config,
            false,
        )
        .unwrap();

        // Calculate expected fees
        // For tx1: term_fee = 1000, treasury = 950 (95%)
        // With deduplication: 2 unique miners, so each gets 950/2 = 475
        let tx1_treasury = U256::from(950);
        let tx1_fee_per_miner = tx1_treasury / U256::from(2);

        // For tx2: term_fee = 2000, treasury = 1900 (95%)
        // 2 unique miners, so each gets 1900/2 = 950
        let tx2_treasury = U256::from(1900);
        let tx2_fee_per_miner = tx2_treasury / U256::from(2);

        // Verify each miner's total fees
        let miner1_total = tx1_fee_per_miner + tx2_fee_per_miner;
        let miner2_total = tx1_fee_per_miner + tx2_fee_per_miner;

        assert_eq!(
            result.reward_balance_increment.get(&miner1).unwrap().0,
            miner1_total,
            "Miner1 should receive correct deduplicated fee"
        );
        assert_eq!(
            result.reward_balance_increment.get(&miner2).unwrap().0,
            miner2_total,
            "Miner2 should receive correct fee"
        );

        // Verify total fees distributed equals treasury amounts
        let total_distributed: U256 = result
            .reward_balance_increment
            .values()
            .map(|(fee, _)| *fee)
            .fold(U256::from(0), |acc, fee| acc + fee);
        let expected_total = tx1_treasury + tx2_treasury;

        assert_eq!(
            total_distributed, expected_total,
            "Total distributed should equal sum of treasury amounts"
        );
    }

    #[test]
    fn test_aggregate_balance_deltas_with_pooled_miners() {
        let node_config = irys_types::NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);

        // term_fee = 3000 → treasury = 2850 (95%) → 950 per miner with 3 miners
        let tx1 = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: H256::random(),
                term_fee: U256::from(3000).into(),
                data_size: 100,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });

        let miner1 = IrysAddress::random();
        let miner2 = IrysAddress::random();
        let miner3 = IrysAddress::random();
        let pool_reward_address = IrysAddress::random();

        let mut epoch_snapshot = EpochSnapshot::default();

        // miner1 and miner2 share pool_reward_address, miner3 is independent
        for (miner, reward_addr) in [
            (miner1, pool_reward_address),
            (miner2, pool_reward_address),
            (miner3, miner3),
        ] {
            epoch_snapshot.commitment_state.stake_commitments.insert(
                miner,
                StakeEntry {
                    id: H256::random(),
                    commitment_status: CommitmentStatus::Active,
                    signer: miner,
                    amount: U256::from(10_000_u64),
                    reward_address: reward_addr,
                },
            );
        }

        let mut tx_to_miners = BTreeMap::new();
        tx_to_miners.insert(tx1.id, Arc::new(vec![miner1, miner2, miner3]));

        let result =
            aggregate_balance_deltas(vec![tx1], &tx_to_miners, &epoch_snapshot, &config, false)
                .unwrap();

        // 2850 split 3 ways = 950 each; pool gets 2x (1900), miner3 gets 1x (950)
        assert_eq!(result.reward_balance_increment.len(), 2);
        assert_eq!(
            result
                .reward_balance_increment
                .get(&pool_reward_address)
                .unwrap()
                .0,
            U256::from(1900)
        );
        assert_eq!(
            result.reward_balance_increment.get(&miner3).unwrap().0,
            U256::from(950)
        );
    }
}
