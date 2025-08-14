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
use nodit::{interval::ii, InclusiveInterval};
use std::collections::{BTreeMap, HashMap};
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

    if expired_slots.is_empty() {
        return Ok(BTreeMap::new());
    }

    // Step 2: Find block ranges
    let block_range = find_block_range(expired_slots, config, &block_index, ledger_type)?;

    // Step 3: Process boundary blocks
    let (earliest_txs, earliest_miners) = process_boundary_block(
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

    let (latest_txs, latest_miners) = process_boundary_block(
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

    // Step 4: Process middle blocks
    let (middle_txs, middle_miners) =
        process_middle_blocks(block_range.middle_blocks, ledger_type, &mempool_sender, &db).await?;

    // Step 5: Combine all transactions
    let mut all_tx_ids = Vec::new();
    all_tx_ids.extend(earliest_txs);
    all_tx_ids.extend(latest_txs);
    all_tx_ids.extend(middle_txs);

    let mut tx_to_miners = HashMap::new();
    tx_to_miners.extend(earliest_miners);
    tx_to_miners.extend(latest_miners);
    tx_to_miners.extend(middle_miners);

    // Step 6: Fetch transactions
    let mut transactions = get_data_tx_in_parallel(all_tx_ids, &mempool_sender, &db).await?;
    transactions.sort();

    // Step 7: Calculate fees
    aggregate_miner_fees(transactions, &tx_to_miners, config)
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
fn collect_expired_partitions(
    epoch_snapshot: &EpochSnapshot,
    block_height: u64,
    target_ledger_type: DataLedger,
) -> eyre::Result<HashMap<SlotIndex, Vec<Address>>> {
    let mut ledgers = epoch_snapshot.ledgers.clone();
    let partition_assignments = &epoch_snapshot.partition_assignments;
    let expired_partition_hashes = ledgers.get_expired_partition_hashes(block_height);
    let mut expired_ledger_slot_indexes = HashMap::new();

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

            expired_ledger_slot_indexes
                .entry(slot_index)
                .and_modify(|miners: &mut Vec<Address>| {
                    miners.push(partition.miner_address);
                })
                .or_insert(vec![partition.miner_address]);
        }
    }

    Ok(expired_ledger_slot_indexes)
}

/// Finds all blocks containing data in the expired chunk ranges
fn find_block_range(
    expired_slots: HashMap<SlotIndex, Vec<Address>>,
    config: &Config,
    block_index: &std::sync::RwLock<BlockIndex>,
    ledger_type: DataLedger,
) -> eyre::Result<BlockRange> {
    let mut blocks_with_expired_ledgers = HashMap::new();

    // Collect all chunk ranges and find the global min/max
    let mut all_ranges = Vec::new();
    for (slot_index, miners) in expired_slots {
        let chunk_range = slot_index.compute_chunk_range(config.consensus.num_chunks_in_partition);
        all_ranges.push((chunk_range, Arc::new(miners)));
    }

    // Find the overall min and max chunk offsets
    let min_chunk = all_ranges
        .iter()
        .map(|(r, _)| r.start())
        .min()
        .ok_or_eyre("No chunk ranges found")?;
    let max_chunk = all_ranges
        .iter()
        .map(|(r, _)| r.end())
        .max()
        .ok_or_eyre("No chunk ranges found")?;

    let block_index_read = block_index
        .read()
        .map_err(|_| eyre::eyre!("block index read guard poisoned"))?;

    // Get the blocks at the boundary offsets
    let (min_height, min_item) = block_index_read.get_block_index_item(ledger_type, *min_chunk)?;
    let (max_height, max_item) = block_index_read.get_block_index_item(ledger_type, *max_chunk)?;

    // Find which range the min/max blocks belong to
    let min_range = all_ranges
        .iter()
        .find(|(r, _)| r.contains_point(min_chunk))
        .map(|(r, _)| *r)
        .ok_or_eyre("Min chunk not in any range")?;
    let max_range = all_ranges
        .iter()
        .find(|(r, _)| r.contains_point(max_chunk))
        .map(|(r, _)| *r)
        .ok_or_eyre("Max chunk not in any range")?;

    let min_block = BoundaryBlock {
        height: min_height,
        item: min_item.clone(),
        chunk_range: min_range,
    };

    let max_block = BoundaryBlock {
        height: max_height,
        item: max_item.clone(),
        chunk_range: max_range,
    };

    // Collect all blocks in the ranges
    for (chunk_range, miners) in all_ranges {
        for chunk_offset in *chunk_range.start()..=*chunk_range.end() {
            let (_, block_index_item) =
                block_index_read.get_block_index_item(ledger_type, chunk_offset)?;
            blocks_with_expired_ledgers.insert(block_index_item.block_hash, Arc::clone(&miners));
        }
    }

    // Ensure min and max blocks are different to avoid duplicate processing
    eyre::ensure!(
        min_block.item.block_hash != max_block.item.block_hash,
        "Min and max blocks are the same - partition spans only one block"
    );

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
) -> eyre::Result<(Vec<H256>, HashMap<H256, Arc<Vec<Address>>>)> {
    // Get the block and its transactions
    let block = get_block_by_hash(block_hash, mempool_sender, db).await?;
    let data_txs = block.get_data_ledger_tx_ids();
    let ledger_tx_ids = data_txs.get(&ledger_type).ok_or_eyre(format!(
        "{:?} ledger is required for expired blocks",
        ledger_type
    ))?;

    // Fetch the actual transactions
    let mut ledger_data_txs =
        get_data_tx_in_parallel(ledger_tx_ids.iter().copied().collect(), mempool_sender, db)
            .await?;

    // Sort transactions to match their order in the block
    ledger_data_txs.sort_by_key(|tx| {
        ledger_tx_ids
            .iter()
            .position(|id| *id == tx.id)
            .unwrap_or(usize::MAX)
    });

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
) -> (Vec<H256>, HashMap<H256, Arc<Vec<Address>>>) {
    let mut current_offset = prev_max_offset;
    let mut filtered_txs = Vec::new();
    let mut tx_to_miners = HashMap::new();

    for tx in transactions {
        let chunks = tx.data_size.div_ceil(chunk_size);
        let tx_start = current_offset;
        let tx_end = current_offset + chunks;

        if is_earliest {
            // For earliest block: skip transactions that start before the partition
            // We only include transactions fully contained within the partition
            if tx_start < partition_range.start() {
                current_offset = tx_end;
                continue;
            }
        } else {
            // For latest block: stop when we reach a transaction that starts after the partition end
            if tx_start > partition_range.end() {
                break;
            }
        }

        // Include this transaction
        filtered_txs.push(tx.id);
        tx_to_miners.insert(tx.id, Arc::clone(&miners));
        current_offset = tx_end;
    }

    (filtered_txs, tx_to_miners)
}

/// Processes all middle blocks (non-boundary blocks)
async fn process_middle_blocks(
    middle_blocks: HashMap<H256, Arc<Vec<Address>>>,
    ledger_type: DataLedger,
    mempool_sender: &UnboundedSender<MempoolServiceMessage>,
    db: &DatabaseProvider,
) -> eyre::Result<(Vec<H256>, HashMap<H256, Arc<Vec<Address>>>)> {
    let mut all_tx_ids = Vec::new();
    let mut tx_to_miners = HashMap::new();

    for (block_hash, miners) in middle_blocks {
        let block = get_block_by_hash(block_hash, mempool_sender, db).await?;
        let data_txs = block.get_data_ledger_tx_ids();
        let ledger_tx_ids = data_txs
            .get(&ledger_type)
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
    transactions: Vec<DataTransactionHeader>,
    tx_to_miners: &HashMap<H256, Arc<Vec<Address>>>,
    config: &Config,
) -> eyre::Result<BTreeMap<Address, (U256, RollingHash)>> {
    let mut aggregated_miner_fees = BTreeMap::<Address, (U256, RollingHash)>::new();

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SlotIndex(u64);

impl SlotIndex {
    fn new(value: u64) -> Self {
        Self(value)
    }

    fn compute_chunk_range(&self, chunks_per_partition: u64) -> LedgerChunkRange {
        let start = LedgerChunkOffset::from(self.0 * chunks_per_partition);
        let end = LedgerChunkOffset::from((self.0 + 1) * chunks_per_partition - 1);
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
    middle_blocks: HashMap<H256, Arc<Vec<Address>>>,
}
