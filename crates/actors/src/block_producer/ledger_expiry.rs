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
//! ---------------------------- | ----------------------------- | ----------------------------
//! [Tx1: chunks 0-49]          |                               |
//! [Tx2: chunks 50-149] <------|---------> Spans A & B         |
//!                             | [Tx3: chunks 150-199]          |
//!                             | [Tx4: chunks 180-250] <--------|---------> Spans B & C
//!                             |                               | [Tx5: chunks 251-299]
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
use crate::mempool_service::MempoolServiceMessage;
use crate::shadow_tx_generator::RollingHash;
use eyre::OptionExt as _;
use irys_database::{block_header_by_hash, db::IrysDatabaseExt as _};
use irys_domain::{BlockIndex, EpochSnapshot};
use irys_types::{
    app_state::DatabaseProvider, fee_distribution::TermFeeCharges, ledger_chunk_offset_ii, Address,
    BlockIndexItem, Config, DataLedger, DataTransactionHeader, IrysBlockHeader, LedgerChunkOffset,
    LedgerChunkRange, H256, U256,
};
use nodit::{interval::ii, InclusiveInterval as _};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::{mpsc::UnboundedSender, oneshot};

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
/// HashMap mapping miner addresses to their total fees and a rolling hash of transaction IDs
#[tracing::instrument(skip_all, fields(block_height, ledger_type = ?ledger_type))]
pub async fn calculate_expired_ledger_fees(
    epoch_snapshot: &EpochSnapshot,
    block_height: u64,
    ledger_type: DataLedger,
    config: &Config,
    block_index: Arc<std::sync::RwLock<BlockIndex>>,
    mempool_sender: UnboundedSender<MempoolServiceMessage>,
    db: DatabaseProvider,
) -> eyre::Result<BTreeMap<Address, (U256, RollingHash)>> {
    // Step 1: Collect expired partitions
    let expired_slots = collect_expired_partitions(epoch_snapshot, block_height, ledger_type)?;

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
        return Ok(BTreeMap::new());
    }

    // Step 2: Find block ranges
    let block_range = find_block_range(expired_slots, config, &block_index, ledger_type)?;

    // Step 3: Process boundary blocks
    let same_block = block_range.min_block.item.block_hash == block_range.max_block.item.block_hash;
    tracing::info!(
        "Processing boundary blocks: min_block height={}, max_block height={}, same_block={}",
        block_range.min_block.height,
        block_range.max_block.height,
        same_block
    );

    let (earliest_txs, earliest_miners);
    let (latest_txs, latest_miners);

    if same_block {
        // When min and max are the same block, process it only once to avoid double-counting
        // Process as earliest block (will include all transactions in the partition range)
        let (txs, miners) = process_boundary_block(
            &block_range.min_block,
            block_range.min_block.item.block_hash,
            Arc::clone(&block_range.min_block_miners),
            true, // is_earliest
            ledger_type,
            config,
            &block_index,
            &mempool_sender,
            &db,
        )
        .await?;

        earliest_txs = txs;
        earliest_miners = miners;
        latest_txs = Vec::new();
        latest_miners = BTreeMap::new();
    } else {
        // Different blocks - process both boundaries
        let (e_txs, e_miners) = process_boundary_block(
            &block_range.min_block,
            block_range.min_block.item.block_hash,
            Arc::clone(&block_range.min_block_miners),
            true, // is_earliest
            ledger_type,
            config,
            &block_index,
            &mempool_sender,
            &db,
        )
        .await?;

        let (l_txs, l_miners) = process_boundary_block(
            &block_range.max_block,
            block_range.max_block.item.block_hash,
            Arc::clone(&block_range.max_block_miners),
            false, // is_earliest
            ledger_type,
            config,
            &block_index,
            &mempool_sender,
            &db,
        )
        .await?;

        earliest_txs = e_txs;
        earliest_miners = e_miners;
        latest_txs = l_txs;
        latest_miners = l_miners;
    }

    // Step 4: Process middle blocks
    let (middle_txs, middle_miners) =
        process_middle_blocks(block_range.middle_blocks, ledger_type, &mempool_sender, &db).await?;

    // Step 5: Combine all transactions
    let mut all_tx_ids = Vec::new();
    all_tx_ids.extend(earliest_txs.clone());
    all_tx_ids.extend(latest_txs.clone());
    all_tx_ids.extend(middle_txs.clone());

    tracing::info!(
        "Collected transactions: earliest={}, latest={}, middle={}, total={}",
        earliest_txs.len(),
        latest_txs.len(),
        middle_txs.len(),
        all_tx_ids.len()
    );

    let mut tx_to_miners = BTreeMap::new();
    tx_to_miners.extend(earliest_miners);
    tx_to_miners.extend(latest_miners);
    tx_to_miners.extend(middle_miners);

    // Step 6: Fetch transactions
    let mut transactions = get_data_tx_in_parallel(all_tx_ids, &mempool_sender, &db).await?;
    transactions.sort();

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

    let fees = aggregate_miner_fees(transactions, &tx_to_miners, config)?;

    let total_fees = fees
        .values()
        .fold(U256::from(0), |acc, (fee, _)| acc.saturating_add(*fee));

    tracing::info!(
        "Calculated fees for {} miners, total fees: {}",
        fees.len(),
        total_fees
    );

    Ok(fees)
}

/// Fetches a block header from mempool or database
async fn get_block_by_hash(
    block_hash: H256,
    mempool_sender: &UnboundedSender<MempoolServiceMessage>,
    db: &DatabaseProvider,
) -> eyre::Result<IrysBlockHeader> {
    let (tx, rx) = oneshot::channel();
    mempool_sender.send(MempoolServiceMessage::GetBlockHeader(block_hash, false, tx))?;

    match rx.await? {
        Some(header) => Ok(header),
        None => db
            .view_eyre(|tx| block_header_by_hash(tx, &block_hash, false))?
            .ok_or_eyre("block not found in db"),
    }
}

/// Collects all expired partitions for the specified ledger type and their miners
#[tracing::instrument(skip_all, fields(block_height, target_ledger_type))]
fn collect_expired_partitions(
    epoch_snapshot: &EpochSnapshot,
    block_height: u64,
    target_ledger_type: DataLedger,
) -> eyre::Result<BTreeMap<SlotIndex, Vec<Address>>> {
    let mut ledgers = epoch_snapshot.ledgers.clone();
    let partition_assignments = &epoch_snapshot.partition_assignments;
    let expired_partition_hashes = ledgers.get_expired_partition_hashes(block_height);
    let mut expired_ledger_slot_indexes = BTreeMap::new();
    if expired_partition_hashes.is_empty() {
        return Ok(expired_ledger_slot_indexes);
    }

    tracing::debug!(
        "collect_expired_partitions: block_height={}, target_ledger={:?}, found {} expired partition hashes",
        block_height,
        target_ledger_type,
        expired_partition_hashes.len()
    );

    for expired_partition_hash in expired_partition_hashes {
        let partition = partition_assignments
            .get_assignment(expired_partition_hash)
            .ok_or_eyre("could not get expired partition")?;

        let ledger_id = partition
            .ledger_id
            .map(DataLedger::try_from)
            .ok_or_eyre("ledger id must be present")??;

        let slot_index = SlotIndex::new(
            partition
                .slot_index
                .ok_or_eyre("slot index must be present")? as u64,
        );

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

            expired_ledger_slot_indexes
                .entry(slot_index)
                .and_modify(|miners: &mut Vec<Address>| {
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
    expired_slots: BTreeMap<SlotIndex, Vec<Address>>,
    config: &Config,
    block_index: &std::sync::RwLock<BlockIndex>,
    ledger_type: DataLedger,
) -> eyre::Result<BlockRange> {
    let mut blocks_with_expired_ledgers = BTreeMap::new();
    let block_index_read = block_index
        .read()
        .map_err(|_| eyre::eyre!("block index read guard poisoned"))?;

    // Ensure that we don't start reading a partition that's only partially populated
    let last_item = block_index_read
        .items
        .last()
        .expect("expected block index to contain at least one item");
    let max_chunk_offset_across_all_partitions =
        LedgerChunkOffset::from(last_item.ledgers[ledger_type].max_chunk_offset);

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
                block_index_read.get_block_index_item(ledger_type, chunk_offset)?;

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
                .and_modify(|existing_miners: &mut Arc<Vec<Address>>| {
                    // Merge the new miners with existing ones
                    let mut combined = (**existing_miners).clone();
                    combined.extend(miners.clone());
                    *existing_miners = Arc::new(combined);
                })
                .or_insert_with(|| Arc::new(miners.clone()));

            // Skip to the next chunk after this block ends.
            // We do this by going to the very end of the current blocks max chunk offset
            chunk_offset = (block_index_item.ledgers[ledger_type].max_chunk_offset + 1)
                .min(*chunk_range.end());
        }
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

    Ok(BlockRange {
        min_block,
        max_block,
        min_block_miners,
        max_block_miners,
        middle_blocks: blocks_with_expired_ledgers,
    })
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
                .max_chunk_offset,
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
    miners: Arc<Vec<Address>>,
    is_earliest: bool,
    ledger_type: DataLedger,
    config: &Config,
    block_index: &std::sync::RwLock<BlockIndex>,
    mempool_sender: &UnboundedSender<MempoolServiceMessage>,
    db: &DatabaseProvider,
) -> eyre::Result<(Vec<H256>, BTreeMap<H256, Arc<Vec<Address>>>)> {
    // Get the block and its transactions
    let block = get_block_by_hash(block_hash, mempool_sender, db).await?;
    let ledger_tx_ids = block
        .get_data_ledger_tx_ids_ordered(ledger_type)
        .ok_or_eyre(format!(
            "{:?} ledger is required for expired blocks",
            ledger_type
        ))?;

    // Fetch the actual transactions
    // Note: get_data_tx_in_parallel preserves the order of input IDs
    let ledger_data_txs =
        get_data_tx_in_parallel(ledger_tx_ids.to_vec(), mempool_sender, db).await?;

    // Get the previous block's max offset
    let block_index_read = block_index
        .read()
        .map_err(|_| eyre::eyre!("block index read guard poisoned"))?;
    let prev_max_offset = get_previous_max_offset(&block_index_read, boundary.height, ledger_type)?;
    drop(block_index_read);

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
/// Tuple of (transaction IDs to include, mapping of tx ID to miners who stored it)
fn filter_transactions_by_chunk_range(
    transactions: Vec<DataTransactionHeader>,
    prev_max_offset: LedgerChunkOffset,
    partition_range: LedgerChunkRange,
    is_earliest: bool,
    chunk_size: u64,
    miners: Arc<Vec<Address>>,
) -> (Vec<H256>, BTreeMap<H256, Arc<Vec<Address>>>) {
    let mut current_offset = prev_max_offset;
    let mut filtered_txs = Vec::new();
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
            filtered_txs.push(tx.id);
            tx_to_miners.insert(tx.id, Arc::clone(&miners));
            current_offset = tx_end;
        }
    }

    tracing::info!("Filtered to {} transactions", filtered_txs.len());
    (filtered_txs, tx_to_miners)
}

/// Processes all middle blocks (non-boundary blocks)
async fn process_middle_blocks(
    middle_blocks: BTreeMap<H256, Arc<Vec<Address>>>,
    ledger_type: DataLedger,
    mempool_sender: &UnboundedSender<MempoolServiceMessage>,
    db: &DatabaseProvider,
) -> eyre::Result<(Vec<H256>, BTreeMap<H256, Arc<Vec<Address>>>)> {
    let mut all_tx_ids = Vec::new();
    let mut tx_to_miners = BTreeMap::new();

    for (block_hash, miners) in middle_blocks {
        let block = get_block_by_hash(block_hash, mempool_sender, db).await?;
        let ledger_tx_ids = block
            .get_data_ledger_tx_ids_ordered(ledger_type)
            .ok_or_eyre(format!("{:?} ledger is required", ledger_type))?;

        for tx_id in ledger_tx_ids.iter() {
            tx_to_miners.insert(*tx_id, Arc::clone(&miners));
            all_tx_ids.push(*tx_id);
        }
    }

    Ok((all_tx_ids, tx_to_miners))
}

/// Calculates and aggregates fees for each miner
fn aggregate_miner_fees(
    mut transactions: Vec<DataTransactionHeader>,
    tx_to_miners: &BTreeMap<H256, Arc<Vec<Address>>>,
    config: &Config,
) -> eyre::Result<BTreeMap<Address, (U256, RollingHash)>> {
    let mut aggregated_miner_fees = BTreeMap::<Address, (U256, RollingHash)>::new();
    transactions.sort();

    for data_tx in transactions.iter() {
        let miners_that_stored_this_tx = tx_to_miners
            .get(&data_tx.id)
            .expect("guaranteed to have the miner list");

        let fee_charges = TermFeeCharges::new(data_tx.term_fee, &config.consensus)?;
        let fee_distribution_per_miner =
            fee_charges.distribution_on_expiry(miners_that_stored_this_tx)?;

        for (miner, fee) in miners_that_stored_this_tx
            .iter()
            .zip(fee_distribution_per_miner)
        {
            aggregated_miner_fees
                .entry(*miner)
                .and_modify(|(current_fee, hash)| {
                    *current_fee = current_fee.saturating_add(fee);
                    hash.xor_assign(U256::from_le_bytes(data_tx.id.0));
                })
                .or_insert((fee, RollingHash(U256::from_le_bytes(data_tx.id.0))));
        }
    }

    Ok(aggregated_miner_fees)
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

/// Type alias for block height/index position
type BlockHeight = u64;

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
    min_block_miners: Arc<Vec<Address>>,
    max_block_miners: Arc<Vec<Address>>,
    middle_blocks: BTreeMap<H256, Arc<Vec<Address>>>,
}
