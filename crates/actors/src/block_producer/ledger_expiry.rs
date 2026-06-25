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
use irys_database::{
    block_header_by_hash, canonical_submit_height, db::IrysDatabaseExt as _,
    reth_db::transaction::DbTx as _, tables::MigratedBlockHashes,
};
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
    parent_block_header: &IrysBlockHeader,
    block_height: u64,
    ledger_type: DataLedger,
    config: &Config,
    block_index: BlockIndex,
    block_tree_guard: &BlockTreeReadGuard,
    mempool_guard: &MempoolReadGuard,
    db: &DatabaseProvider,
    expect_txs_to_be_promoted: bool,
    // `ledger_type`'s cumulative `total_chunks` at the block being produced/validated.
    // Used to exclude slots written this epoch from the expiring set (they are
    // rescued by the `last_height` touch and must not be settled here).
    new_total_chunks: u64,
    // Cascade status of the epoch block being produced/validated (its own
    // timestamp — NOT the parent snapshot's). Gates the write-window exclusion
    // so it mirrors the Cascade-gated `touch_active_ledger_slots`. When false,
    // settlement matches pre-Cascade master exactly (no exclusion).
    cascade_active: bool,
) -> eyre::Result<LedgerExpiryBalanceDelta> {
    // Fee distribution is only implemented for Submit ledger. Publish expiry
    // simply resets partitions without fee redistribution.
    assert_ne!(
        ledger_type,
        DataLedger::Publish,
        "fee distribution not supported for Publish ledger"
    );

    // Step 1: Collect expired partitions
    let expired_slots = collect_expired_partitions(
        parent_epoch_snapshot,
        block_height,
        ledger_type,
        new_total_chunks,
        cascade_active,
    )?;

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
    let block_range = match find_block_range(
        expired_slots,
        config,
        &block_index,
        ledger_type,
        parent_ledger_total_chunks(parent_block_header, ledger_type),
        parent_block_header.block_hash,
        block_tree_guard,
        db,
    )? {
        Some(br) => br,
        None => {
            // Check to see if there were no chunks uploaded to this ledger slot!
            // If there wasn't, there aren't any fees to distribute
            return Ok(LedgerExpiryBalanceDelta::default());
        }
    };

    // Steps 3-5: Process boundary + middle blocks → tx → miners
    let tx_to_miners = collect_tx_to_miners_from_range(
        block_range,
        ledger_type,
        config,
        block_tree_guard,
        mempool_guard,
        db,
    )
    .await?;

    // Step 6: Fetch transactions
    let all_tx_ids: Vec<_> = tx_to_miners.keys().copied().collect();
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

/// Boundary + middle block processing (algorithm steps 3-5): maps every tx in the
/// expired chunk ranges to the miners who stored it.
///
/// Shared by [`calculate_expired_ledger_fees`] (fee/refund distribution) and
/// [`expired_submit_tx_ids`] (NC-0042 non-promotability set) so the two cannot
/// disagree about which txs an expired slot contains — the structural flaw the
/// cycle-math approximation introduced (see NC-0042 §4b).
async fn collect_tx_to_miners_from_range(
    block_range: BlockRange,
    ledger_type: DataLedger,
    config: &Config,
    block_tree_guard: &BlockTreeReadGuard,
    mempool_guard: &MempoolReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<BTreeMap<IrysTransactionId, Arc<Vec<IrysAddress>>>> {
    let same_block = block_range.min_block.block_hash == block_range.max_block.block_hash;
    tracing::info!(
        "Processing boundary blocks: min_block height={}, max_block height={}, same_block={}",
        block_range.min_block.height,
        block_range.max_block.height,
        same_block
    );

    // Per-tx attribution is keyed on the slot containing each tx's START offset
    // (NC-0042 R4), so every block — boundary or middle — is processed against the
    // same un-merged `slot_miners` map rather than a per-block miner union.
    let slot_miners = &block_range.slot_miners;
    let earliest_miners;
    let latest_miners;

    if same_block {
        // When min and max are the same block, process it once as BOTH the earliest
        // and latest boundary so both ends of the expired range are trimmed. (Trimming
        // only the start — the old behavior — over-included the next slot's txs, which
        // diverged from the exact per-candidate promotion filter; see NC-0042 §4b.)
        let miners = process_boundary_block(
            &block_range.min_block,
            true, // is_earliest
            true, // is_latest — sole boundary block (min == max), so trim BOTH ends
            ledger_type,
            config,
            slot_miners,
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
            true,  // is_earliest — min block trims only the start
            false, // is_latest
            ledger_type,
            config,
            slot_miners,
            block_tree_guard,
            mempool_guard,
            db,
        )
        .await?;

        let l_miners = process_boundary_block(
            &block_range.max_block,
            false, // is_earliest
            true,  // is_latest — max block trims only the end
            ledger_type,
            config,
            slot_miners,
            block_tree_guard,
            mempool_guard,
            db,
        )
        .await?;

        earliest_miners = e_miners;
        latest_miners = l_miners;
    }

    // Middle (fully-interior) blocks: no trim (they lie entirely within the
    // expired range), but still attributed per-slot by tx start offset — a middle
    // block can straddle a slot boundary too. The boundary blocks share the same
    // global range, so reuse it for the (unused) trim arguments.
    let middle_miners = process_middle_blocks(
        &block_range.middle_blocks,
        block_range.min_block.chunk_range,
        ledger_type,
        config,
        slot_miners,
        block_tree_guard,
        mempool_guard,
        db,
    )
    .await?;

    tracing::info!(
        "Collected transactions: earliest={}, latest={}, middle={}",
        earliest_miners.len(),
        latest_miners.len(),
        middle_miners.len(),
    );

    let mut tx_to_miners = BTreeMap::new();
    tx_to_miners.extend(earliest_miners);
    tx_to_miners.extend(latest_miners);
    tx_to_miners.extend(middle_miners);

    Ok(tx_to_miners)
}

/// NC-0042 §4b/§4c: the set of Submit-ledger transaction ids whose storage has
/// **expired as of `block_height`** — i.e. the txs that are (or will be at the
/// next epoch) perm-fee-refunded and therefore must never be promoted.
///
/// It is derived from the *same* expired-partition → block → tx walk that
/// `calculate_expired_ledger_fees` uses for refunds, so the "is this tx
/// refunded?" and "may this tx be promoted?" decisions cannot diverge.
///
/// Unlike the refund pipeline (which keys on *newly* expiring slots, acting once
/// per slot), this keys on [`EpochSnapshot::get_all_expired_term_slot_indexes`]
/// (inclusive of slots that expired at an earlier epoch), so a tx remains
/// non-promotable for every block at-or-after its slot's expiry — covering both
/// the same-block (epoch) collision and the cross-block double-pay.
///
/// Miners are irrelevant here (no fee attribution), so a sentinel miner is used
/// purely to satisfy the boundary-filter's non-empty-miners requirement; the
/// returned value is the tx-id set only.
///
/// # Retained as a test-only oracle
///
/// Production no longer materializes this whole-history set on every block — it
/// was O(expired-partition history) per produced/validated block. The producer
/// filter and validator check now use the per-candidate `is_submit_storage_expired`
/// (O(candidate txs) per block). This walk is kept as the **differential-test
/// oracle**: a tx is in this set iff `is_submit_storage_expired` returns `true`
/// for it, and that equivalence is asserted against real chain state.
#[cfg(any(test, feature = "test-utils"))]
#[tracing::instrument(skip_all, fields(block.height = block_height))]
pub async fn expired_submit_tx_ids(
    parent_epoch_snapshot: &EpochSnapshot,
    parent_block_header: &IrysBlockHeader,
    block_height: u64,
    config: &Config,
    block_index: BlockIndex,
    block_tree_guard: &BlockTreeReadGuard,
    mempool_guard: &MempoolReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<std::collections::BTreeSet<IrysTransactionId>> {
    let slot_indexes =
        parent_epoch_snapshot.get_all_expired_term_slot_indexes(DataLedger::Submit, block_height);
    if slot_indexes.is_empty() {
        return Ok(std::collections::BTreeSet::new());
    }

    // Sentinel miner: `find_block_range` / `filter_transactions_by_chunk_range`
    // require a non-empty miner list to include a slot's txs, but we only need the
    // tx ids here. The address value is never read (we take `.keys()`).
    let expired_slots: BTreeMap<SlotIndex, Vec<IrysAddress>> = slot_indexes
        .into_iter()
        .map(|i| (SlotIndex::new(i as u64), vec![IrysAddress::ZERO]))
        .collect();

    let block_range = match find_block_range(
        expired_slots,
        config,
        &block_index,
        DataLedger::Submit,
        parent_ledger_total_chunks(parent_block_header, DataLedger::Submit),
        parent_block_header.block_hash,
        block_tree_guard,
        db,
    )? {
        Some(br) => br,
        // No chunks were ever uploaded into the expired slot ranges.
        None => return Ok(std::collections::BTreeSet::new()),
    };

    let tx_to_miners = collect_tx_to_miners_from_range(
        block_range,
        DataLedger::Submit,
        config,
        block_tree_guard,
        mempool_guard,
        db,
    )
    .await?;

    Ok(tx_to_miners.into_keys().collect())
}

/// Test-only convenience that combines [`expired_submit_range`] (computed once)
/// with [`submit_tx_expired`] (per candidate) into a single per-`txid` verdict.
///
/// Production does NOT call this: both the producer (`tx_selector`) and the
/// validator (`block_validation`) hoist `expired_submit_range` out of their
/// candidate loops and call `submit_tx_expired` directly. The branch-determinism
/// rationale lives on those two functions. Kept here as the differential-test
/// counterpart to the [`expired_submit_tx_ids`] walk oracle.
#[cfg(any(test, feature = "test-utils"))]
#[tracing::instrument(level = "debug", skip_all, fields(tx.id = %txid, block.height = block_height))]
pub async fn is_submit_storage_expired(
    txid: IrysTransactionId,
    block_height: u64,
    parent_epoch_snapshot: &EpochSnapshot,
    parent_block_header: &IrysBlockHeader,
    config: &Config,
    block_index: &BlockIndex,
    block_tree_guard: &BlockTreeReadGuard,
    mempool_guard: &MempoolReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<bool> {
    let Some(range) = expired_submit_range(
        block_height,
        parent_epoch_snapshot,
        parent_block_header,
        config,
    )?
    else {
        return Ok(false);
    };
    submit_tx_expired(
        txid,
        &range,
        config,
        block_index,
        block_tree_guard,
        mempool_guard,
        db,
    )
    .await
}

/// Block-level inputs for the §4b/§4c Submit-expiry check, computed once per
/// block and reused across every publish candidate via [`submit_tx_expired`].
///
/// Every field is a pure function of the block's **own parent** — never this
/// node's canonical tip or migration-lagged block index — so the verdict is
/// identical across nodes (NC-0042 F2 branch-determinism).
#[derive(Debug, Clone, Copy)]
pub struct ExpiredSubmitRange {
    /// The block being produced/validated (caps the canonical Submit lookup).
    block_height: u64,
    /// Exclusive end of the expired chunk range `[0, range_end)`, bounded by the
    /// parent header's recorded Submit total.
    range_end: u64,
    /// The block's own parent hash — root of the branch-correct ancestry walk
    /// used to resolve a candidate's Submit inclusion. Never the node's tip.
    parent_block_hash: H256,
}

/// Computes the [`ExpiredSubmitRange`] for `block_height`, or `None` if no Submit
/// storage has expired as of this block (nothing to filter).
///
/// `range_end` is bounded by the **parent header's** recorded Submit total only —
/// a pure function of the block's own parent, identical on every node regardless
/// of that node's migration-lagged block-index tip (NC-0042 F2). The branch-
/// correct mapping from a candidate's inclusion to a chunk offset happens later,
/// per candidate, in [`submit_tx_expired`].
pub fn expired_submit_range(
    block_height: u64,
    parent_epoch_snapshot: &EpochSnapshot,
    parent_block_header: &IrysBlockHeader,
    config: &Config,
) -> eyre::Result<Option<ExpiredSubmitRange>> {
    // The expired Submit slots (also handles "not enough blocks elapsed → empty").
    let slot_indexes =
        parent_epoch_snapshot.get_all_expired_term_slot_indexes(DataLedger::Submit, block_height);
    let Some(&max_expired_slot) = slot_indexes.iter().max() else {
        return Ok(None);
    };

    // NC-0042 contiguity guard. The `[0, range_end)` collapse below is correct only
    // if the expired slots are a prefix `{0..=max}`. In normal operation they are:
    // chunk offsets are cumulative and slots fill sequentially, so `last_height` is
    // non-decreasing in slot index, and empty pre-allocated slots sit at the top
    // where the never-expire-the-last-slot rule trims them. A hole (an empty,
    // non-last slot aged out by its allocation height beneath a still-live lower
    // slot) would make the prefix over-approximate — deterministically rejecting
    // valid promotions AND diverging from the slot-exact refund walk
    // (`collect_expired_partitions`, which only settles slots that actually hold
    // partitions). The set is a pure function of canonical state, so failing loud
    // here fails identically on every node (no fork) instead of silently diverging
    // — same philosophy as the `filter_transactions_by_chunk_range` guard below.
    // `slot_indexes` is ascending and distinct (`get_all_expired_slot_indexes`
    // iterates slots in order), so "value at position i equals i" ⇔ prefix-from-0.
    if let Some((i, &slot)) = slot_indexes
        .iter()
        .enumerate()
        .find(|&(i, &slot)| slot != i)
    {
        eyre::bail!(
            "expired Submit slot set {slot_indexes:?} is not a contiguous prefix from 0 \
             (slot {slot} at position {i}) — the [0, range_end) promotion-filter \
             assumption is violated; refusing to over-approximate (NC-0042)"
        );
    }

    // End of the expired chunk range = end of the highest expired slot, clamped
    // to data actually written *as of the parent*. Expired slots form a prefix
    // from slot 0, so the range is `[0, range_end)`. Bounding by the parent
    // header (not this node's block-index tip) is what makes the verdict
    // branch-deterministic.
    let p = config.consensus.num_chunks_in_partition;
    let parent_total = parent_ledger_total_chunks(parent_block_header, DataLedger::Submit);
    let range_end = (max_expired_slot as u64 + 1)
        .saturating_mul(p)
        .min(parent_total);
    if range_end == 0 {
        return Ok(None);
    }

    Ok(Some(ExpiredSubmitRange {
        block_height,
        range_end,
        parent_block_hash: parent_block_header.block_hash,
    }))
}

/// Per-candidate verdict against a precomputed [`ExpiredSubmitRange`]: is
/// `txid`'s Submit storage expired as of `range.block_height`?
///
/// Branch-correct: `txid`'s Submit inclusion is resolved from the block's **own
/// parent ancestry** (migrated inclusions via `canonical_submit_height`, which is
/// finalized and branch-invariant; un-migrated inclusions via a by-hash parent
/// walk — never the node's canonical tip or migration-lagged index). The verdict
/// is then a pure offset comparison: expired iff the candidate's Submit start
/// offset is `< range_end`. A candidate with no resolvable Submit inclusion on
/// this branch yields `false`.
#[tracing::instrument(level = "debug", skip_all, fields(tx.id = %txid))]
pub async fn submit_tx_expired(
    txid: IrysTransactionId,
    range: &ExpiredSubmitRange,
    config: &Config,
    block_index: &BlockIndex,
    block_tree_guard: &BlockTreeReadGuard,
    mempool_guard: &MempoolReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<bool> {
    let Some(inc) =
        resolve_submit_inclusion(txid, range, config, block_index, block_tree_guard, db)?
    else {
        // No Submit inclusion resolvable on this branch → cannot have expired.
        return Ok(false);
    };

    // Offset verdict against the expired range `[0, range_end)`. The block span
    // decides outright unless `range_end` falls strictly inside the inclusion
    // block, in which case the candidate's exact start offset settles it.
    match submit_span_verdict(inc.base, inc.total, range.range_end) {
        Some(verdict) => Ok(verdict),
        None => {
            let block = get_block_by_hash(inc.block_hash, block_tree_guard, db).await?;
            let ordered_ids = block
                .get_data_ledger_tx_ids_ordered(DataLedger::Submit)
                .ok_or_eyre("Submit ledger required for the expiry inclusion block")?;
            let headers = get_data_tx_in_parallel(ordered_ids.to_vec(), mempool_guard, db).await?;
            let ordered: Vec<(IrysTransactionId, u64)> =
                headers.iter().map(|h| (h.id, h.data_size)).collect();
            let start_offset =
                submit_tx_start_offset(txid, &ordered, inc.base, config.consensus.chunk_size);
            Ok(start_offset.is_some_and(|offset| offset < range.range_end))
        }
    }
}

/// Pure block-span decision: given an inclusion block's `[base, total)` Submit
/// chunk span and the expired range end, is the candidate expired?
/// `Some(true)`  → the whole block lies before `range_end`.
/// `Some(false)` → the block starts at/after `range_end`.
/// `None`        → `range_end` falls strictly inside the block; the caller must
///                  consult the candidate's exact start offset.
fn submit_span_verdict(base: u64, total: u64, range_end: u64) -> Option<bool> {
    if total <= range_end {
        Some(true)
    } else if base >= range_end {
        Some(false)
    } else {
        None
    }
}

/// A candidate's branch-correct Submit inclusion: the block that introduced it
/// into the Submit ledger, with the cumulative chunk offsets bracketing it.
struct SubmitInclusion {
    /// Hash of the inclusion block.
    block_hash: H256,
    /// Cumulative Submit chunks *before* the inclusion block (its start offset).
    base: u64,
    /// Cumulative Submit chunks *through* the inclusion block.
    total: u64,
}

/// Resolves `txid`'s Submit inclusion along the **block's own parent ancestry**
/// (`range.parent_block_hash`) — never the node's canonical tip.
///
/// Fast path: `canonical_submit_height` resolves migrated inclusions in O(1).
/// Migrated blocks are below the reorg floor, hence finalized and shared by every
/// branch, so the height is authoritative and branch-invariant.
///
/// Slow path (only when the inclusion is un-migrated, i.e. the fast path returns
/// `None`): walk the parent chain by hash up to `tx_anchor_expiry_depth` blocks
/// (config-asserted `>= block_migration_depth`, so the walk covers the entire
/// un-migrated window) looking for `txid` in each block's Submit `tx_ids`. The
/// inclusion block's predecessor total comes from the tree, or — if the
/// predecessor is below the tree window — from the canonical index, gated by
/// `MigratedBlockHashes` so a side-fork ancestor can never be mistaken for the
/// canonical block at that height (the C1 guard). No `get_canonical_chain` or
/// height→hash tree lookup is ever used.
fn resolve_submit_inclusion(
    txid: IrysTransactionId,
    range: &ExpiredSubmitRange,
    config: &Config,
    block_index: &BlockIndex,
    block_tree_guard: &BlockTreeReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<Option<SubmitInclusion>> {
    // Fast path: migrated inclusion (finalized, branch-invariant). Capped at the
    // parent — Submit inclusion is strictly before the block being evaluated.
    let max_height = range.block_height.saturating_sub(1);
    if let Some(h) = db.view_eyre(|tx| canonical_submit_height(tx, &txid, max_height))? {
        let item = block_index
            .get_item(h)
            .ok_or_eyre("canonical_submit_height returned a height absent from the block index")?;
        let total = submit_total_of_index_item(&item);
        let base = if h == 0 {
            0
        } else {
            block_index
                .get_item(h - 1)
                .map(|i| submit_total_of_index_item(&i))
                .unwrap_or(0)
        };
        return Ok(Some(SubmitInclusion {
            block_hash: item.block_hash,
            base,
            total,
        }));
    }

    // Slow path: un-migrated inclusion — walk the parent ancestry by hash. The
    // walk logic lives in the pure `walk_submit_inclusion` so the branch-
    // correctness rules (by-hash traversal, C1 side-fork guard) are unit-testable
    // with in-memory maps; here we back it with the live tree, the block index,
    // and `MigratedBlockHashes`.
    let max_walk = config.consensus.mempool.tx_anchor_expiry_depth as u64;
    let tree = block_tree_guard.read();
    let resolved = walk_submit_inclusion(
        range.parent_block_hash,
        max_walk,
        |hash| {
            tree.get_block(hash).map(|header| WalkBlock {
                height: header.height,
                prev_hash: header.previous_block_hash,
                submit_total: parent_ledger_total_chunks(header, DataLedger::Submit),
                includes_txid: block_includes_submit_tx(header, &txid),
            })
        },
        |height| {
            block_index
                .get_item(height)
                .map(|item| submit_total_of_index_item(&item))
        },
        // C1 gate input: is `hash` the canonical block at `height` per
        // `MigratedBlockHashes`? (shared with `assert_canonical_via_mbh`.)
        |hash, height| is_canonical_at(db, hash, height),
    )?;
    Ok(resolved.map(|(block_hash, base, total)| SubmitInclusion {
        block_hash,
        base,
        total,
    }))
}

/// Dependency-free view of a block needed by [`walk_submit_inclusion`].
#[derive(Clone, Copy)]
struct WalkBlock {
    height: u64,
    prev_hash: H256,
    /// Cumulative Submit chunks through this block.
    submit_total: u64,
    /// Whether this block's Submit ledger includes the candidate txid.
    includes_txid: bool,
}

/// Pure branch-correct ancestry walk — the testable core of
/// [`resolve_submit_inclusion`]'s slow path. Walks the parent chain **by hash**
/// from `parent_hash` (never the node's canonical tip) for at most `max_walk`
/// steps, looking for the block that includes the candidate. When found, the
/// predecessor's cumulative Submit total (the inclusion's `base` offset) comes
/// from the tree if present, otherwise from the canonical index — but only after
/// `is_canonical_at` confirms the predecessor hash is the canonical block at that
/// height (the **C1 side-fork guard**, so a side-fork ancestor's chunks are never
/// misattributed; `block_tree_depth > block_migration_depth` guarantees such a
/// predecessor is indexed). Returns `(block_hash, base, total)`.
fn walk_submit_inclusion(
    parent_hash: H256,
    max_walk: u64,
    header_by_hash: impl Fn(&H256) -> Option<WalkBlock>,
    index_submit_total_at: impl Fn(u64) -> Option<u64>,
    is_canonical_at: impl Fn(H256, u64) -> eyre::Result<bool>,
) -> eyre::Result<Option<(H256, u64, u64)>> {
    let mut cursor = parent_hash;
    let mut steps = 0_u64;
    while steps <= max_walk {
        let Some(block) = header_by_hash(&cursor) else {
            break; // fell out of the retained tree window
        };
        if block.includes_txid {
            let total = block.submit_total;
            // Predecessor total, branch-correct.
            let base = if block.height == 0 {
                0
            } else if let Some(prev) = header_by_hash(&block.prev_hash) {
                prev.submit_total
            } else {
                // Predecessor below the tree window → canonical index, C1-gated.
                let prev_height = block.height - 1;
                if !is_canonical_at(block.prev_hash, prev_height)? {
                    eyre::bail!(
                        "Submit-expiry walk reached a non-canonical ancestor at height \
                         {prev_height} (supplied {}) — off the validated block's branch \
                         (C1 guard)",
                        block.prev_hash
                    );
                }
                index_submit_total_at(prev_height).ok_or_eyre(
                    "Submit-expiry walk: predecessor below block_tree window must be \
                     indexed (block_tree_depth > block_migration_depth)",
                )?
            };
            return Ok(Some((cursor, base, total)));
        }
        cursor = block.prev_hash;
        steps += 1;
    }
    Ok(None)
}

/// Whether `header`'s Submit ledger includes `txid` (per-block `tx_ids`).
fn block_includes_submit_tx(header: &IrysBlockHeader, txid: &IrysTransactionId) -> bool {
    header
        .data_ledgers
        .iter()
        .find(|l| l.ledger_id == DataLedger::Submit as u32)
        .is_some_and(|l| l.tx_ids.0.contains(txid))
}

/// Cumulative Submit chunk total from a migrated block-index item.
fn submit_total_of_index_item(item: &BlockIndexItem) -> u64 {
    ledger_total_of_index_item(item, DataLedger::Submit)
}

/// Cumulative chunk total for `ledger` from a migrated block-index item
/// (`0` if the ledger is absent — pre-Cascade / inactive).
fn ledger_total_of_index_item(item: &BlockIndexItem, ledger: DataLedger) -> u64 {
    item.ledgers
        .iter()
        .find(|l| l.ledger == ledger)
        .map(|l| l.total_chunks)
        .unwrap_or(0)
}

/// Is `hash` the canonical block at `height` per `MigratedBlockHashes`? The
/// single C1 read, shared by the pure walk (`walk_submit_inclusion`, via a
/// closure) and `assert_canonical_via_mbh`. (`block_tree_depth >
/// block_migration_depth` guarantees a predecessor below the tree window is
/// indexed, so `false` here means a genuinely non-canonical ancestor.)
fn is_canonical_at(db: &DatabaseProvider, hash: H256, height: u64) -> eyre::Result<bool> {
    let canonical = db.view_eyre(|tx| Ok(tx.get::<MigratedBlockHashes>(height)?))?;
    Ok(canonical == Some(hash))
}

/// C1 side-fork guard: confirm `hash` is the canonical block at `height` per
/// `MigratedBlockHashes` before trusting a height-keyed (canonical-only) index
/// lookup for it. Without this, a walk that descended onto a side-fork ancestor
/// below the block-tree window would attribute the canonical block's chunks to
/// the wrong block.
fn assert_canonical_via_mbh(db: &DatabaseProvider, hash: H256, height: u64) -> eyre::Result<()> {
    if !is_canonical_at(db, hash, height)? {
        eyre::bail!(
            "expiry walk reached a non-canonical ancestor at height {height} \
             (supplied {hash}) — off the validated block's branch (C1 guard)"
        );
    }
    Ok(())
}

/// Branch-correct resolution of the canonical block that introduced chunk
/// `offset` in `ledger`, as a pure function of the block's **own parent
/// ancestry** (`parent_hash`) — never the node's canonical tip or migration-
/// lagged index frontier (NC-0042 R1). Returns `(height, block_hash, base,
/// block_total)` where `base` is the cumulative `ledger` total *before* the block
/// (its start offset) and `block_total` is the cumulative total *through* it.
/// `base` is resolved from the same branch-correct ancestry as the block itself,
/// so a boundary block's start offset never falls back to the migrated-only index
/// (NC-0042 R1: that fallback errored on an un-migrated boundary predecessor).
///
/// Fast path: offsets below the migrated index tip resolve via the index binary
/// search (finalized ⇒ branch-invariant, O(log n)). Slow path: offsets in the
/// un-migrated tail are resolved by walking the parent chain BY HASH (`get_block`
/// + `previous_block_hash`); a predecessor below the block-tree window is read
/// from the index only after the `MigratedBlockHashes` C1 check confirms it is
/// canonical at that height. No `get_canonical_chain` or height→hash tree lookup
/// is ever used for the forky region. The fast/slow split is itself branch-safe:
/// both paths return the same block (a migrated offset's block is finalized and
/// shared by every branch; an un-migrated offset's block is on the parent's own
/// chain).
fn resolve_ledger_offset_to_block(
    offset: u64,
    parent_hash: H256,
    ledger: DataLedger,
    block_tree_guard: &BlockTreeReadGuard,
    block_index: &BlockIndex,
    db: &DatabaseProvider,
) -> eyre::Result<(BlockHeight, H256, u64, u64)> {
    // Fast path: finalized/migrated region — binary search is branch-invariant.
    let index_tip_total = block_index
        .get_latest_item()
        .map(|item| ledger_total_of_index_item(&item, ledger))
        .unwrap_or(0);
    if offset < index_tip_total {
        let (height, item) = block_index.get_block_index_item(ledger, offset)?;
        return Ok((
            height,
            item.block_hash,
            migrated_predecessor_total(block_index, ledger, height),
            ledger_total_of_index_item(&item, ledger),
        ));
    }

    // Slow path: un-migrated tail — walk the parent ancestry by hash.
    let tree = block_tree_guard.read();
    let mut cursor = parent_hash;
    while let Some(header) = tree.get_block(&cursor) {
        let total = parent_ledger_total_chunks(header, ledger);
        let prev_total = if header.height == 0 {
            0
        } else if let Some(prev) = tree.get_block(&header.previous_block_hash) {
            parent_ledger_total_chunks(prev, ledger)
        } else {
            // Predecessor below the tree window → canonical index, C1-gated.
            let prev_height = header.height - 1;
            assert_canonical_via_mbh(db, header.previous_block_hash, prev_height)?;
            block_index
                .get_item(prev_height)
                .map(|i| ledger_total_of_index_item(&i, ledger))
                .ok_or_eyre(
                    "expiry walk: predecessor below block_tree window must be indexed \
                     (block_tree_depth > block_migration_depth)",
                )?
        };
        if (prev_total..total).contains(&offset) {
            return Ok((header.height, cursor, prev_total, total));
        }
        cursor = header.previous_block_hash;
    }

    // Walked off the retained tree window without matching → the offset is below
    // it, hence finalized/migrated; resolve from the index (branch-invariant).
    let (height, item) = block_index.get_block_index_item(ledger, offset)?;
    Ok((
        height,
        item.block_hash,
        migrated_predecessor_total(block_index, ledger, height),
        ledger_total_of_index_item(&item, ledger),
    ))
}

/// Cumulative `ledger` total *before* a migrated block at `height` (its base
/// offset): the predecessor's indexed total, or `0` at genesis. Sound only for
/// **migrated** heights — then `height - 1` is itself migrated (below the reorg
/// floor, branch-invariant), so the index read is exact. Used by
/// [`resolve_ledger_offset_to_block`]'s index fast paths; the un-migrated tail
/// instead carries `prev_total` straight from the by-hash ancestry walk.
fn migrated_predecessor_total(
    block_index: &BlockIndex,
    ledger: DataLedger,
    height: BlockHeight,
) -> u64 {
    if height == 0 {
        0
    } else {
        block_index
            .get_item(height - 1)
            .map(|i| ledger_total_of_index_item(&i, ledger))
            .unwrap_or(0)
    }
}

/// Reconstructs `target`'s Submit-ledger **start chunk offset**: the cumulative
/// chunk offset at the end of the previous block (`base_chunks`) plus the chunks
/// of every Submit tx ordered before `target` in its inclusion block — exactly
/// as `filter_transactions_by_chunk_range` accumulates `current_offset`.
/// `block_submit_txs` is that block's Submit txs as `(tx_id, data_size)` in
/// on-chain order. `None` if `target` is not in the block.
fn submit_tx_start_offset(
    target: IrysTransactionId,
    block_submit_txs: &[(IrysTransactionId, u64)],
    base_chunks: u64,
    chunk_size: u64,
) -> Option<u64> {
    if chunk_size == 0 {
        return None;
    }
    let mut offset = base_chunks;
    for (id, data_size) in block_submit_txs {
        if *id == target {
            return Some(offset);
        }
        offset = offset.saturating_add(data_size.div_ceil(chunk_size));
    }
    None
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
    new_total_chunks: u64,
    cascade_active: bool,
) -> eyre::Result<BTreeMap<SlotIndex, Vec<IrysAddress>>> {
    let partition_assignments = &parent_epoch_snapshot.partition_assignments;
    // Window-excluded (Cascade only): settle exactly the slots that actually
    // recycle, so a slot written in its expiry epoch (rescued by the last_height
    // touch) is not paid here and then again when it later recycles. Pre-Cascade
    // the touch is gated off, so `cascade_active=false` disables the exclusion
    // and settlement matches the original master set.
    let expired_partition_info = &parent_epoch_snapshot.get_expiring_partition_info(
        block_height,
        target_ledger_type,
        new_total_chunks,
        cascade_active,
    );
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
        let ledger_id = expired_partition.ledger_id;
        let slot_index = SlotIndex::new(expired_partition.slot_index as u64);

        // Filter by ledger type FIRST — before any lookup that could fail.
        // This prevents a Publish partition state inconsistency from blocking
        // Submit fee distribution (or vice versa).
        if ledger_id != target_ledger_type {
            tracing::debug!(
                "Skipping partition with ledger_id={:?} (looking for {:?})",
                ledger_id,
                target_ledger_type
            );
            continue;
        }

        let partition = partition_assignments
            .get_assignment(expired_partition.partition_hash)
            .ok_or_eyre("could not get expired partition")?;

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
    }

    Ok(expired_ledger_slot_indexes)
}

/// The parent block header's recorded total chunk count for `ledger`. Headers
/// carry per-ledger totals, so this is a pure function of the parent block —
/// unlike the node's block-index tip, which moves as the chain grows. `0` when
/// the ledger is absent from the header (e.g. pre-Cascade blocks without
/// OneYear/ThirtyDay entries).
fn parent_ledger_total_chunks(parent_block_header: &IrysBlockHeader, ledger: DataLedger) -> u64 {
    parent_block_header
        .data_ledgers
        .iter()
        .find(|dl| dl.ledger_id == ledger as u32)
        .map(|dl| dl.total_chunks)
        .unwrap_or(0)
}

/// Finds all blocks containing data in the expired chunk ranges
fn find_block_range(
    expired_slots: BTreeMap<SlotIndex, Vec<IrysAddress>>,
    config: &Config,
    block_index: &BlockIndex,
    ledger_type: DataLedger,
    parent_total_chunks: u64,
    parent_block_hash: H256,
    block_tree_guard: &BlockTreeReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<Option<BlockRange>> {
    // Per-block branch-correct base offset (hash → base). Miner attribution is
    // per-slot (`slot_miners` below), NOT per-block: a block straddling two
    // expired slots with different miners must split the txs by their start
    // offset, not pay every tx to the union of both slots' miners (NC-0042 R4).
    let mut block_bases: BTreeMap<H256, u64> = BTreeMap::new();
    // Slot index → its miners (un-merged), for per-tx attribution by start offset.
    let mut slot_miners: BTreeMap<SlotIndex, Arc<Vec<IrysAddress>>> = BTreeMap::new();

    // Bound the expired chunk range by the *parent header's* recorded ledger size
    // only — a pure function of the block being built/validated, identical on every
    // node regardless of that node's migration-lagged index tip (NC-0042 R1). Each
    // offset is then mapped to its block branch-correctly via
    // `resolve_ledger_offset_to_block` (parent-ancestry walk by hash for the
    // un-migrated tail, finalized index below it, C1-gated), so the whole
    // enumeration is a pure function of the parent's canonical ancestry.
    //
    // Previously this clamped to `min(index_tip, parent)` and resolved via the
    // migrated-only index, leaving expired chunks in the un-migrated tail invisible
    // — an epoch-block consensus-fork vector AND a stranded-refund accounting gap
    // (the now-exact §4b filter drops a tail tx that this walk never refunded).
    let max_chunk_offset_across_all_partitions = LedgerChunkOffset::from(parent_total_chunks);

    // Track min and max blocks as we iterate. Keyed by hash (not block-index item)
    // so un-migrated tail blocks — which have no index entry yet — are representable.
    let mut min_height: Option<(BlockHeight, H256, u64)> = None;
    let mut max_height: Option<(BlockHeight, H256, u64)> = None;
    // The boundary trim must span the UNION of all expired slots, not one slot's
    // range. When several expired slots share a block (small partitions), trimming
    // to a single slot's range would drop the others — under-refunding / halving
    // fee distribution (NC-0042). Expired slots are a contiguous prefix from slot
    // 0, so the union is `[lowest start, highest end)`.
    let mut global_start: Option<u64> = None;
    let mut global_end: Option<u64> = None;

    for (slot_index, miners) in expired_slots {
        // Per-slot miners (un-merged) — the attribution source keyed by tx start
        // offset's slot, instead of a per-block union (NC-0042 R4).
        slot_miners.insert(slot_index, Arc::new(miners));
        let chunk_range = slot_index.compute_chunk_range(
            config.consensus.num_chunks_in_partition,
            max_chunk_offset_across_all_partitions,
        );
        global_start =
            Some(global_start.map_or(*chunk_range.start(), |s| s.min(*chunk_range.start())));
        global_end = Some(global_end.map_or(*chunk_range.end(), |e| e.max(*chunk_range.end())));

        let mut chunk_offset = *chunk_range.start();
        while chunk_offset < *chunk_range.end() {
            // Branch-correct: resolve the offset against the parent's own ancestry
            // (tree-then-index, C1-gated), not the migrated-only index.
            let (height, block_hash, block_base, block_total) = resolve_ledger_offset_to_block(
                chunk_offset,
                parent_block_hash,
                ledger_type,
                block_tree_guard,
                block_index,
                db,
            )?;

            // Update min_height if this is the first block or a lower height.
            // Carry the block's branch-correct base (start offset) so boundary
            // trimming reads it from the parent ancestry, never re-deriving it
            // from the migrated-only index (NC-0042 R1 — the un-migrated boundary
            // predecessor errored / could diverge there).
            if min_height.as_ref().is_none_or(|(h, _, _)| height < *h) {
                min_height = Some((height, block_hash, block_base));
            }

            // Update max_height if this is the first block or a higher height
            if max_height.as_ref().is_none_or(|(h, _, _)| height > *h) {
                max_height = Some((height, block_hash, block_base));
            }

            // Record the block's branch-correct base offset (the same for every
            // offset that resolves to it). Miners are NOT merged here — each tx is
            // attributed to its own start-offset slot in `slot_miners` (NC-0042 R4).
            block_bases.entry(block_hash).or_insert(block_base);

            // Advance to the first chunk of the NEXT block. `block_total` is the
            // exclusive end of this block's chunks (offsets `[base, block_total)`),
            // so it is exactly the next block's first offset. The old `+ 1` here
            // skipped that boundary chunk, so a block whose first chunk landed on a
            // slot boundary was never resolved — silently dropping its expired txs
            // from the refund/fee walk (NC-0042). `block_total > chunk_offset`
            // always (the offset lies within this block), so progress is guaranteed.
            chunk_offset = block_total.min(*chunk_range.end());
        }
    }

    // Double check to see if there were any chunks added to this partition requiring rewards
    // (This should cause the min_height and max_height to be the same resulting in no fee distribution)
    if min_height.is_none() && max_height.is_none() {
        return Ok(None);
    }

    // Extract min and max block data - these must exist if we have expired slots
    let (min_height, min_hash, min_base) =
        min_height.expect("min_height must be populated after iterating expired slots");
    let (max_height, max_hash, max_base) =
        max_height.expect("max_height must be populated after iterating expired slots");

    // Both boundary blocks trim against the full expired span [global_start,
    // global_end): the earliest skips txs starting before it, the latest breaks at
    // its end, and a sole block (min == max) does both. Using the union — not a
    // single slot's range — is what keeps a block holding several expired slots
    // from being trimmed down to one (the multi-slot under-count bug).
    let global_range = LedgerChunkRange(ledger_chunk_offset_ii!(
        global_start.expect("global_start populated when expired slots exist"),
        global_end.expect("global_end populated when expired slots exist")
    ));
    let min_block = BoundaryBlock {
        height: min_height,
        block_hash: min_hash,
        base: min_base,
        chunk_range: global_range,
    };

    let max_block = BoundaryBlock {
        height: max_height,
        block_hash: max_hash,
        base: max_base,
        chunk_range: global_range,
    };

    // The boundaries are processed explicitly; drop them so `block_bases` holds
    // only the fully-interior ("middle") blocks.
    block_bases.remove(&min_block.block_hash);
    block_bases.remove(&max_block.block_hash);

    Ok(Some(BlockRange {
        min_block,
        max_block,
        middle_blocks: block_bases,
        slot_miners,
    }))
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
    is_earliest: bool,
    is_latest: bool,
    ledger_type: DataLedger,
    config: &Config,
    slot_miners: &BTreeMap<SlotIndex, Arc<Vec<IrysAddress>>>,
    block_tree_guard: &BlockTreeReadGuard,
    mempool_guard: &MempoolReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<BTreeMap<IrysTransactionId, Arc<Vec<IrysAddress>>>> {
    // Get the block and its transactions
    let block = get_block_by_hash(boundary.block_hash, block_tree_guard, db).await?;
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

    // The boundary block's branch-correct start offset, carried from
    // `find_block_range`'s parent-ancestry resolution. NOT re-derived from the
    // migrated-only block index: that read errored when the boundary block's
    // predecessor was in the un-migrated tail, and could return the canonical
    // (wrong-branch) total on a side fork — re-introducing the R1 index-lag
    // divergence in the refund/fee walk (NC-0042).
    let prev_max_offset = LedgerChunkOffset::from(boundary.base);

    // Filter transactions based on chunk positions
    let filtered_txs = filter_transactions_by_chunk_range(
        ledger_data_txs,
        prev_max_offset,
        boundary.chunk_range,
        is_earliest,
        is_latest,
        config.consensus.chunk_size,
        config.consensus.num_chunks_in_partition,
        slot_miners,
    )?;

    Ok(filtered_txs)
}

/// Filters transactions based on their chunk positions relative to partition boundaries.
///
/// This is the core logic for handling transaction overlaps at partition boundaries.
/// Transactions are processed sequentially, tracking their cumulative chunk positions.
///
/// # Boundary Handling
///
/// The two trims are independent so the **sole** boundary block (the whole
/// expired range packed into one block, `min == max`) can apply both at once:
/// - **Start trim** (`is_earliest`): skips transactions that start before the
///   partition boundary.
/// - **End trim** (`is_latest`): stops at the first transaction starting at or
///   after the partition end.
///
/// A multi-block range sets exactly one (`min` block → start trim, `max` block →
/// end trim). A sole block sets **both** — omitting the end trim there would
/// over-include the next slot's txs (NC-0042 §4b: keeps Pipeline B's refund set
/// exact, matching the per-candidate promotion filter `submit_tx_expired`).
///
/// # Returns
///
/// Mapping of tx ID to the miners who stored it. Fails loud (`Err`) if an
/// in-range tx maps to a non-expired slot — an unreachable state that would mean
/// the refund walk diverged from the per-candidate promotion filter (NC-0042).
#[tracing::instrument(level = "trace", skip_all, fields(
    chunk.prev_max_offset = %prev_max_offset,
    chunk.partition_start = %partition_range.start(),
    chunk.partition_end = %partition_range.end(),
    tx.count = transactions.len(),
    boundary.is_earliest = is_earliest,
    boundary.is_latest = is_latest
))]
fn filter_transactions_by_chunk_range(
    transactions: Vec<DataTransactionHeader>,
    prev_max_offset: LedgerChunkOffset,
    partition_range: LedgerChunkRange,
    is_earliest: bool,
    is_latest: bool,
    chunk_size: u64,
    num_chunks_in_partition: u64,
    slot_miners: &BTreeMap<SlotIndex, Arc<Vec<IrysAddress>>>,
) -> eyre::Result<BTreeMap<IrysTransactionId, Arc<Vec<IrysAddress>>>> {
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

        // Start trim (earliest boundary): skip transactions that start before
        // the partition; we only include transactions starting within it.
        if is_earliest && tx_start < partition_range.start() {
            tracing::debug!("  Skipping (starts before partition)");
            current_offset = tx_end;
            continue;
        }
        // End trim (latest boundary): stop at the first transaction that starts
        // at or after the partition end (one starting exactly at the end belongs
        // to the next partition). Independent of the start trim so a sole block
        // (min == max) applies both.
        if is_latest && tx_start >= partition_range.end() {
            tracing::debug!("  Breaking (starts at or after partition end)");
            break;
        }

        // Per-slot attribution: the slot containing this tx's START offset owns it
        // (the module's ownership rule). NOT a per-block miner union — that
        // cross-paid adjacent expired slots owned by different miners when they
        // shared a block (NC-0042 R4). Expired slots tile [global_start,
        // global_end) contiguously, so an in-range tx always maps to a present
        // slot. The `None` arm is therefore unreachable; if it ever fires the
        // refund walk has diverged from the per-candidate promotion filter
        // (`submit_tx_expired`) — fail loud rather than silently under-refund,
        // which in consensus code would strand a refund and break Pipeline A ≡ B.
        if num_chunks_in_partition != 0 {
            let slot = SlotIndex::new(*tx_start / num_chunks_in_partition);
            match slot_miners.get(&slot) {
                Some(miners) => {
                    tracing::debug!("  Including transaction (slot {})", slot.0);
                    tx_to_miners.insert(tx.id, Arc::clone(miners));
                }
                None => eyre::bail!(
                    "tx {} start offset {} maps to non-expired slot {} \
                     (expired slots must tile the range contiguously) — refund \
                     walk diverged from the promotion filter (NC-0042)",
                    tx.id,
                    *tx_start,
                    slot.0,
                ),
            }
        }
        current_offset = tx_end;
    }

    tracing::info!("Filtered to {} transactions", tx_to_miners.len());
    Ok(tx_to_miners)
}

/// Processes all middle (fully-interior) blocks. Unlike the boundaries these need
/// no trim — every tx lies within the expired range — but they DO need per-slot
/// attribution: a middle block can straddle a slot boundary, so each tx is paid to
/// the miners of the slot containing its start offset, computed from the block's
/// branch-correct base offset (NC-0042 R4 — previously every middle-block tx was
/// paid to the per-block miner union).
async fn process_middle_blocks(
    middle_blocks: &BTreeMap<H256, u64>,
    global_range: LedgerChunkRange,
    ledger_type: DataLedger,
    config: &Config,
    slot_miners: &BTreeMap<SlotIndex, Arc<Vec<IrysAddress>>>,
    block_tree_guard: &BlockTreeReadGuard,
    mempool_guard: &MempoolReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<BTreeMap<IrysTransactionId, Arc<Vec<IrysAddress>>>> {
    let mut tx_to_miners = BTreeMap::new();

    for (block_hash, base) in middle_blocks {
        let block = get_block_by_hash(*block_hash, block_tree_guard, db).await?;
        let ledger_tx_ids = block
            .get_data_ledger_tx_ids_ordered(ledger_type)
            .ok_or_eyre(format!("{:?} ledger is required", ledger_type))?;
        let ledger_data_txs =
            get_data_tx_in_parallel(ledger_tx_ids.to_vec(), mempool_guard, db).await?;

        // No trim (`is_earliest = is_latest = false`): a middle block lies entirely
        // within the expired range. `global_range` is passed only for logging
        // parity. Each tx is attributed by the slot containing its start offset.
        let filtered = filter_transactions_by_chunk_range(
            ledger_data_txs,
            LedgerChunkOffset::from(*base),
            global_range,
            false,
            false,
            config.consensus.chunk_size,
            config.consensus.num_chunks_in_partition,
            slot_miners,
        )?;
        tx_to_miners.extend(filtered);
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

impl LedgerExpiryBalanceDelta {
    /// Merge another delta into this one, combining reward increments and refund lists.
    pub fn merge(&mut self, other: Self) {
        for (addr, (fee, hash)) in other.reward_balance_increment {
            self.reward_balance_increment
                .entry(addr)
                .and_modify(|(current_fee, current_hash)| {
                    *current_fee = current_fee.saturating_add(fee);
                    current_hash.xor_assign(hash.0);
                })
                .or_insert((fee, hash));
        }
        self.user_perm_fee_refunds
            .extend(other.user_perm_fee_refunds);
    }
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
            // Deduplicate miners - each address should only get one share.
            // BTreeSet ensures deterministic iteration order across all nodes,
            // which is critical because the fee remainder is assigned to the
            // first miner in the list.
            let unique_miners: Vec<IrysAddress> = miners_that_stored_this_tx
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
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
    block_hash: H256,
    /// The block's branch-correct Submit start offset (cumulative chunks *before*
    /// it), resolved from the parent ancestry in [`find_block_range`]. Used as the
    /// boundary trim's starting `current_offset` instead of an index lookup, so a
    /// boundary block in the un-migrated tail is handled correctly (NC-0042 R1).
    base: u64,
    chunk_range: LedgerChunkRange,
}

/// Tracks the range of blocks containing expired partition data
struct BlockRange {
    min_block: BoundaryBlock,
    max_block: BoundaryBlock,
    /// Fully-interior blocks (strictly between the boundaries): hash → branch-
    /// correct base offset. Attributed per-slot by tx start offset just like the
    /// boundary blocks, so a middle block straddling a slot boundary splits
    /// correctly across miners.
    middle_blocks: BTreeMap<H256, u64>,
    /// Slot index → the miners that stored it (un-merged). A tx is attributed to
    /// the miners of the slot containing its START offset — the module's ownership
    /// rule. Keeping this per-slot (not a per-block union) is what stops adjacent
    /// expired slots owned by different miners from cross-paying when they share a
    /// block (NC-0042 R4).
    slot_miners: BTreeMap<SlotIndex, Arc<Vec<IrysAddress>>>,
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

    #[test]
    fn test_ledger_expiry_balance_delta_merge_combines_rewards() {
        use crate::shadow_tx_generator::RollingHash;

        let addr1 = IrysAddress::random();
        let addr2 = IrysAddress::random();

        let mut delta1 = LedgerExpiryBalanceDelta::default();
        delta1
            .reward_balance_increment
            .insert(addr1, (U256::from(100), RollingHash(U256::from(1))));

        let mut delta2 = LedgerExpiryBalanceDelta::default();
        delta2
            .reward_balance_increment
            .insert(addr1, (U256::from(50), RollingHash(U256::from(2))));
        delta2
            .reward_balance_increment
            .insert(addr2, (U256::from(200), RollingHash(U256::from(3))));

        delta1.merge(delta2);

        // addr1: fees are summed
        let (fee, _) = delta1.reward_balance_increment.get(&addr1).unwrap();
        assert_eq!(*fee, U256::from(150));

        // addr2: added from delta2
        let (fee, _) = delta1.reward_balance_increment.get(&addr2).unwrap();
        assert_eq!(*fee, U256::from(200));
    }

    #[test]
    fn test_ledger_expiry_balance_delta_merge_extends_refunds() {
        let mut delta1 = LedgerExpiryBalanceDelta::default();
        delta1
            .user_perm_fee_refunds
            .push((H256::random(), U256::from(1), IrysAddress::random()));

        let mut delta2 = LedgerExpiryBalanceDelta::default();
        delta2
            .user_perm_fee_refunds
            .push((H256::random(), U256::from(2), IrysAddress::random()));

        delta1.merge(delta2);
        assert_eq!(delta1.user_perm_fee_refunds.len(), 2);
    }

    #[test]
    fn test_ledger_expiry_balance_delta_merge_empty() {
        let mut delta1 = LedgerExpiryBalanceDelta::default();
        let delta2 = LedgerExpiryBalanceDelta::default();
        delta1.merge(delta2);
        assert!(delta1.reward_balance_increment.is_empty());
        assert!(delta1.user_perm_fee_refunds.is_empty());
    }

    // --- NC-0042 §4b/§4c per-candidate pure cores. The full DB-backed
    //     equivalence vs. the `expired_submit_tx_ids` walk oracle is asserted in
    //     chain-tests; these pin the offset math and boundary rules directly. ---

    // `submit_tx_start_offset`: chunk_size = 1 ⇒ data_size == chunk count unless
    // noted.

    #[test]
    fn start_offset_sums_preceding_txs_from_base() {
        let a = H256::random();
        let b = H256::random();
        // base 8: A occupies chunks [8,10), so B starts at chunk 10.
        let block = [(a, 2), (b, 5)];
        assert_eq!(submit_tx_start_offset(a, &block, 8, 1), Some(8));
        assert_eq!(submit_tx_start_offset(b, &block, 8, 1), Some(10));
    }

    #[test]
    fn start_offset_rounds_each_tx_up_to_whole_chunks() {
        // chunk_size 10: A data_size 15 → 2 chunks (div_ceil), so B starts at 2.
        let a = H256::random();
        let b = H256::random();
        assert_eq!(
            submit_tx_start_offset(b, &[(a, 15), (b, 5)], 0, 10),
            Some(2)
        );
    }

    #[test]
    fn start_offset_none_when_target_absent_or_chunk_size_zero() {
        let absent = H256::random();
        assert_eq!(
            submit_tx_start_offset(absent, &[(H256::random(), 3)], 0, 1),
            None
        );
        let a = H256::random();
        assert_eq!(submit_tx_start_offset(a, &[(a, 3)], 0, 0), None);
    }

    // `submit_span_verdict`: the pure offset decision over an inclusion block's
    // `[base, total)` Submit chunk span against the expired `range_end`. The
    // verdict is an exact offset comparison — no block-span over-inclusion (this
    // is the F4 fix vs. the old `submit_tx_in_expired_range` block-membership).

    #[test]
    fn span_verdict_block_wholly_before_range_end_is_expired() {
        // Inclusion block occupies [4, 9); range_end 10 ⇒ whole block expired.
        assert_eq!(submit_span_verdict(4, 9, 10), Some(true));
        // Exactly touching: total == range_end ⇒ still wholly before.
        assert_eq!(submit_span_verdict(4, 10, 10), Some(true));
    }

    #[test]
    fn span_verdict_block_at_or_after_range_end_is_not_expired() {
        // Inclusion block starts at the boundary ⇒ not expired.
        assert_eq!(submit_span_verdict(10, 14, 10), Some(false));
        // Entirely past the boundary.
        assert_eq!(submit_span_verdict(20, 25, 10), Some(false));
    }

    #[test]
    fn span_verdict_straddle_requires_exact_offset() {
        // range_end 10 falls strictly inside [8, 12) ⇒ caller must consult the
        // candidate's exact start offset (no over-inclusion of the whole block).
        assert_eq!(submit_span_verdict(8, 12, 10), None);
    }

    // `walk_submit_inclusion`: the pure, branch-correct ancestry walk that backs
    // `resolve_submit_inclusion`'s slow path (un-migrated Submit inclusions). The
    // NC-0042 F2 (ii) fix and the C1 side-fork guard live here; these exercise
    // them with in-memory maps (no BlockTree/BlockIndex/db fixtures).

    /// Build a `header_by_hash` map from `(hash, height, prev, submit_total,
    /// includes_txid)` rows.
    fn walk_tree(
        rows: &[(H256, u64, H256, u64, bool)],
    ) -> std::collections::HashMap<H256, WalkBlock> {
        rows.iter()
            .map(|&(hash, height, prev_hash, submit_total, includes_txid)| {
                (
                    hash,
                    WalkBlock {
                        height,
                        prev_hash,
                        submit_total,
                        includes_txid,
                    },
                )
            })
            .collect()
    }

    fn unreachable_index(_height: u64) -> Option<u64> {
        panic!("index lookup must not be called when the predecessor is in-tree / unneeded")
    }
    fn unreachable_canonical(_hash: H256, _height: u64) -> eyre::Result<bool> {
        panic!("C1 canonical check must not be called when the predecessor is in-tree / unneeded")
    }

    #[test]
    fn walk_finds_tx_with_in_tree_predecessor() {
        // h2(parent, not incl) → h1(incl, prev h0) → h0(genesis). base from the
        // in-tree predecessor h0's total; index/canonical never consulted.
        let h0 = H256::random();
        let h1 = H256::random();
        let h2 = H256::random();
        let tree = walk_tree(&[
            (h2, 2, h1, 20, false),
            (h1, 1, h0, 15, true),
            (h0, 0, H256::zero(), 8, false),
        ]);
        let got = walk_submit_inclusion(
            h2,
            100,
            |h| tree.get(h).copied(),
            unreachable_index,
            unreachable_canonical,
        )
        .unwrap();
        assert_eq!(got, Some((h1, 8, 15)));
    }

    #[test]
    fn walk_tree_bottom_predecessor_uses_canonical_index() {
        // Inclusion block is the bottom of the retained tree; its predecessor is
        // below the window → base via the canonical index, gated by the C1 check.
        let h_bottom = H256::random();
        let h_below = H256::random();
        let tree = walk_tree(&[(h_bottom, 5, h_below, 30, true)]);
        let got = walk_submit_inclusion(
            h_bottom,
            100,
            |h| tree.get(h).copied(),
            |height| (height == 4).then_some(25),
            |hash, height| Ok(hash == h_below && height == 4), // C1 passes
        )
        .unwrap();
        assert_eq!(got, Some((h_bottom, 25, 30)));
    }

    #[test]
    fn walk_c1_guard_rejects_non_canonical_predecessor() {
        // KEY branch-correctness test: predecessor below the window is NOT the
        // canonical block at its height (a side-fork ancestor) → C1 guard fires,
        // the walk errors rather than misattributing the canonical block's chunks.
        let h_bottom = H256::random();
        let h_below = H256::random();
        let tree = walk_tree(&[(h_bottom, 5, h_below, 30, true)]);
        let err = walk_submit_inclusion(
            h_bottom,
            100,
            |h| tree.get(h).copied(),
            |_| Some(25),
            |_, _| Ok(false), // C1: supplied hash is not canonical at that height
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("non-canonical ancestor"),
            "expected the C1 guard message, got: {err}"
        );
    }

    #[test]
    fn walk_returns_none_when_tx_absent() {
        // No block in the window includes the tx; the walk runs off the bottom
        // (predecessor of genesis absent from the map) → None.
        let h0 = H256::random();
        let h1 = H256::random();
        let h2 = H256::random();
        let tree = walk_tree(&[
            (h2, 2, h1, 20, false),
            (h1, 1, h0, 15, false),
            (h0, 0, H256::zero(), 8, false),
        ]);
        let got = walk_submit_inclusion(
            h2,
            100,
            |h| tree.get(h).copied(),
            unreachable_index,
            unreachable_canonical,
        )
        .unwrap();
        assert_eq!(got, None);
    }

    #[test]
    fn walk_genesis_inclusion_has_zero_base() {
        // Inclusion at height 0 ⇒ base is 0 with no predecessor lookup at all.
        let h0 = H256::random();
        let tree = walk_tree(&[(h0, 0, H256::zero(), 5, true)]);
        let got = walk_submit_inclusion(
            h0,
            100,
            |h| tree.get(h).copied(),
            unreachable_index,
            unreachable_canonical,
        )
        .unwrap();
        assert_eq!(got, Some((h0, 0, 5)));
    }

    #[test]
    fn walk_stops_at_max_walk_depth() {
        // The including block sits at depth 2, but max_walk=1 bounds the walk to
        // depths {0,1}, so it is never reached → None (the anchor-expiry bound).
        let h0 = H256::random();
        let h1 = H256::random();
        let h2 = H256::random();
        let h3 = H256::random();
        let tree = walk_tree(&[
            (h3, 3, h2, 30, false),
            (h2, 2, h1, 20, false),
            (h1, 1, h0, 15, true), // depth 2 from h3 — beyond max_walk
            (h0, 0, H256::zero(), 8, false),
        ]);
        let got = walk_submit_inclusion(
            h3,
            1,
            |h| tree.get(h).copied(),
            unreachable_index,
            unreachable_canonical,
        )
        .unwrap();
        assert_eq!(got, None);
    }

    /// NC-0042 determinism regression: `find_block_range` must bound the
    /// expired chunk range by the *parent block's* recorded ledger size, not
    /// this node's block-index tip. With a partially-filled expired slot, a
    /// tip-based bound sweeps in chunks that landed *after* the validated
    /// block's parent, so the verdict would change with when a node evaluates
    /// it (live vs. catching up) — a consensus divergence.
    #[test]
    fn find_block_range_bounds_by_parent_not_index_tip() -> eyre::Result<()> {
        use irys_database::{IrysDatabaseArgs as _, open_or_create_db, tables::IrysTables};
        use irys_domain::{BlockTree, BlockTreeReadGuard};
        use irys_testing_utils::IrysBlockHeaderTestExt as _;
        use reth_db::mdbx::DatabaseArguments;
        use std::sync::RwLock;

        let mut node_config = irys_types::NodeConfig::testing();
        node_config.consensus.get_mut().num_chunks_in_partition = 10;
        let config = Config::new_with_random_peer_id(node_config);

        // Index of 5 blocks with Submit totals 0,4,7,12,15. Slot 0 covers
        // chunks [0,10). At height 2 (the "parent") slot 0 is only 7/10 full;
        // heights 3-4 grew the index past the slot boundary into slot 1.
        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));
        let block_index = BlockIndex::new_for_testing(db.clone());
        for (height, submit_total) in [0_u64, 4, 7, 12, 15].into_iter().enumerate() {
            block_index.push_item(
                &BlockIndexItem {
                    block_hash: H256::random(),
                    num_ledgers: 2,
                    ledgers: vec![
                        irys_types::LedgerIndexItem {
                            total_chunks: 0,
                            tx_root: H256::random(),
                            ledger: DataLedger::Publish,
                        },
                        irys_types::LedgerIndexItem {
                            total_chunks: submit_total,
                            tx_root: H256::random(),
                            ledger: DataLedger::Submit,
                        },
                    ],
                },
                height as u64,
            )?;
        }
        let expired_slot_0 = || {
            let mut m = BTreeMap::new();
            m.insert(SlotIndex::new(0), vec![IrysAddress::ZERO]);
            m
        };

        // All three cases below resolve offsets that lie BELOW the migrated index
        // tip (total 15), so they exercise `find_block_range`'s fast path: the block
        // tree is never walked and `parent_hash` is unused. A genesis-only tree and
        // an arbitrary parent hash satisfy the signature. The slow/un-migrated-tail
        // path is covered by `find_block_range_resolves_unmigrated_tail`.
        let mut genesis = IrysBlockHeader::new_mock_header();
        genesis.height = 0;
        genesis.cumulative_diff = 0.into();
        genesis.test_sign();
        let guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(BlockTree::new(
            &genesis,
            irys_types::ConsensusConfig::testing(),
        ))));
        let parent_hash = genesis.block_hash;

        // Parent at height 2 (total 7): the range must stop at the block
        // holding chunk 6 (height 2), even though the index tip (total 15)
        // covers slot 0's full [0,10) range.
        let range = find_block_range(
            expired_slot_0(),
            &config,
            &block_index,
            DataLedger::Submit,
            7,
            parent_hash,
            &guard,
            &db,
        )?
        .expect("expired slot 0 holds data");
        assert_eq!(range.min_block.height, 1, "chunk 0 lands at height 1");
        assert_eq!(
            range.max_block.height, 2,
            "parent-bounded range must exclude chunks written after the parent"
        );

        // Parent == tip: identical to the old tip-based bound (slot 0 truncated
        // at the partition end 10 → chunk 9 lands at height 3).
        let range = find_block_range(
            expired_slot_0(),
            &config,
            &block_index,
            DataLedger::Submit,
            15,
            parent_hash,
            &guard,
            &db,
        )?
        .expect("expired slot 0 holds data");
        assert_eq!(range.max_block.height, 3);

        // Parent total ahead of the migrated index, but slot 0's data ([0,10)) is
        // fully migrated, so resolution stays on the fast path — same blocks as
        // parent == tip, no out-of-range error.
        let range = find_block_range(
            expired_slot_0(),
            &config,
            &block_index,
            DataLedger::Submit,
            20,
            parent_hash,
            &guard,
            &db,
        )?
        .expect("expired slot 0 holds data");
        assert_eq!(range.max_block.height, 3);

        Ok(())
    }

    /// NC-0042 R1: `find_block_range` must resolve expired chunks in the
    /// **un-migrated tail** from the parent's block tree, not truncate at the
    /// migrated block-index tip. Slot 1 (`[10,20)`) extends past the index tip
    /// (Submit total 14) into tree-only blocks (heights 3,4); the range's max
    /// block must be the tree block at height 4. Pre-R1 the range was capped at
    /// the index tip → max block height 2, silently dropping the tail's
    /// refunds/miner payouts (an epoch-block accounting + determinism gap).
    ///
    /// Also covers the R1 boundary-base fix: the resolved boundary blocks must
    /// carry their branch-correct start offset (`base`), and the downstream
    /// `collect_tx_to_miners_from_range` walk must succeed on an un-migrated
    /// boundary block. Pre-fix the boundary base was re-derived from
    /// `block_index.get_item(height - 1)`, which returned `None` for the
    /// un-migrated height 3 and errored "previous block must exist".
    #[tokio::test]
    async fn find_block_range_resolves_unmigrated_tail() -> eyre::Result<()> {
        use irys_database::{IrysDatabaseArgs as _, open_or_create_db, tables::IrysTables};
        use irys_domain::{
            BlockTree, BlockTreeReadGuard, CommitmentSnapshot, EpochSnapshot, dummy_ema_snapshot,
        };
        use irys_testing_utils::IrysBlockHeaderTestExt as _;
        use irys_types::{BlockTransactions, SealedBlock};
        use reth_db::mdbx::DatabaseArguments;
        use std::sync::RwLock;

        let mut node_config = irys_types::NodeConfig::testing();
        node_config.consensus.get_mut().num_chunks_in_partition = 10;
        let config = Config::new_with_random_peer_id(node_config);

        // A Submit header with a cumulative chunk total at `height`.
        fn with_submit(height: u64, total_chunks: u64) -> IrysBlockHeader {
            let mut h = IrysBlockHeader::new_mock_header();
            h.height = height;
            h.data_ledgers = vec![irys_types::DataTransactionLedger {
                ledger_id: DataLedger::Submit as u32,
                tx_root: H256::zero(),
                tx_ids: irys_types::H256List(vec![]),
                total_chunks,
                expires: None,
                proofs: None,
                required_proof_count: None,
            }];
            h
        }

        // Tree holds heights 0..=4 with cumulative Submit totals 0,8,14,18,22, so
        // block h introduced chunks [total(h-1), total(h)). The migrated index
        // holds only 0..=2 (tip total 14); heights 3,4 are the un-migrated tail,
        // present ONLY in the tree.
        let mut headers: Vec<IrysBlockHeader> = [0_u64, 8, 14, 18, 22]
            .into_iter()
            .enumerate()
            .map(|(h, t)| with_submit(h as u64, t))
            .collect();
        let guard = {
            let mut iter = headers.iter_mut();
            let genesis = iter.next().unwrap();
            genesis.cumulative_diff = 0.into();
            genesis.test_sign();
            let mut prev = genesis.block_hash;
            let mut tree = BlockTree::new(genesis, irys_types::ConsensusConfig::testing());
            tree.mark_tip(&prev).unwrap();
            for h in iter {
                h.previous_block_hash = prev;
                h.cumulative_diff = h.height.into();
                h.test_sign();
                prev = h.block_hash;
                let sealed = Arc::new(SealedBlock::new_unchecked(
                    Arc::new(h.clone()),
                    BlockTransactions::default(),
                ));
                tree.add_block(
                    &sealed,
                    Arc::new(CommitmentSnapshot::default()),
                    Arc::new(EpochSnapshot::default()),
                    dummy_ema_snapshot(),
                )?;
            }
            BlockTreeReadGuard::new(Arc::new(RwLock::new(tree)))
        };
        let parent_hash = headers[4].block_hash; // height 4 = tip = the parent
        let tree_tail_hash = headers[4].block_hash;

        // Migrated index: heights 0..=2 only (Submit totals 0,8,14).
        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));
        let block_index = BlockIndex::new_for_testing(db.clone());
        for (height, submit_total) in [0_u64, 8, 14].into_iter().enumerate() {
            block_index.push_item(
                &BlockIndexItem {
                    // Migrated index agrees with the tree on block hashes (as in
                    // reality), so the boundary walk can fetch the fast-path block.
                    block_hash: headers[height].block_hash,
                    num_ledgers: 2,
                    ledgers: vec![
                        irys_types::LedgerIndexItem {
                            total_chunks: 0,
                            tx_root: H256::random(),
                            ledger: DataLedger::Publish,
                        },
                        irys_types::LedgerIndexItem {
                            total_chunks: submit_total,
                            tx_root: H256::random(),
                            ledger: DataLedger::Submit,
                        },
                    ],
                },
                height as u64,
            )?;
        }

        // Expire slot 1 = chunks [10,20); parent's Submit total is 22.
        let mut expired = BTreeMap::new();
        expired.insert(SlotIndex::new(1), vec![IrysAddress::ZERO]);
        let range = find_block_range(
            expired,
            &config,
            &block_index,
            DataLedger::Submit,
            22, // parent_total — slot 1's tail (chunks 14-19) is past the index tip 14
            parent_hash,
            &guard,
            &db,
        )?
        .expect("expired slot 1 holds data");

        // chunk 10 (fast path, index) → height 2; chunk 19 (slow path, tree) →
        // the un-migrated tree block at height 4 — proving the tail was resolved.
        assert_eq!(
            range.min_block.height, 2,
            "slot 1 starts at chunk 10, which lives in height 2"
        );
        assert_eq!(
            range.max_block.height, 4,
            "slot 1's tail must resolve to the un-migrated tree block (pre-R1 this was capped at 2)"
        );
        assert_eq!(
            range.max_block.block_hash, tree_tail_hash,
            "the max boundary block must be the tree block, not an indexed one"
        );

        // R1 boundary-base fix: each boundary block carries its branch-correct
        // start offset, resolved from the parent ancestry. min_block (height 2)
        // base = Submit total through height 1 = 8 (present in the index);
        // max_block (height 4) base = total through height 3 = 18, which is PAST
        // the migrated index tip (14) and is obtainable ONLY from the tree walk.
        assert_eq!(
            range.min_block.base, 8,
            "min_block base = Submit total through height 1"
        );
        assert_eq!(
            range.max_block.base, 18,
            "max_block base = Submit total through the un-migrated height 3 — \
             tree-only; pre-R1 the index lookup of height 3 errored"
        );

        // End-to-end: the boundary/middle walk must succeed on the un-migrated
        // max boundary block. Pre-fix this errored with "previous block must
        // exist" because the boundary base was read from the migrated-only index.
        // (Fixture blocks carry no Submit tx_ids, so the result is empty — the
        // point is that resolution completes without error.)
        let tx_to_miners = collect_tx_to_miners_from_range(
            range,
            DataLedger::Submit,
            &config,
            &guard,
            &MempoolReadGuard::stub(),
            &db,
        )
        .await?;
        assert!(
            tx_to_miners.is_empty(),
            "fixture blocks carry no Submit tx_ids, so no txs are attributed"
        );

        Ok(())
    }

    /// NC-0042 R4: when two expired slots owned by DIFFERENT miners share one
    /// block (a block straddling the slot boundary), each tx must be attributed
    /// to the miners of the slot containing its **start offset** — the module's
    /// ownership rule — NOT the union of both slots' miners. Pre-fix
    /// `find_block_range` merged miners per block, so every tx in the straddle
    /// block was paid to both miner sets (mis-distributed term fees + a wrong
    /// reward rolling-hash). Deterministic across nodes (not a fork), but a real
    /// fee-accounting bug invisible to the single-miner heavy fixtures.
    #[tokio::test]
    async fn collect_attributes_each_tx_to_its_start_offset_slot_miners() -> eyre::Result<()> {
        use irys_database::{
            IrysDatabaseArgs as _, insert_tx_header, open_or_create_db, tables::IrysTables,
        };
        use irys_domain::{
            BlockTree, BlockTreeReadGuard, CommitmentSnapshot, EpochSnapshot, dummy_ema_snapshot,
        };
        use irys_testing_utils::IrysBlockHeaderTestExt as _;
        use irys_types::{BlockTransactions, DataTransactionHeaderV1, H256List, SealedBlock};
        use reth_db::mdbx::DatabaseArguments;
        use std::sync::RwLock;

        // chunk_size = 1 ⇒ data_size N occupies N chunks. Partition = 4 chunks, so
        // slot 0 = [0,4), slot 1 = [4,8). A single block (height 1) holds all 8
        // chunks, straddling the slot-0/slot-1 boundary.
        let mut node_config = irys_types::NodeConfig::testing();
        {
            let c = node_config.consensus.get_mut();
            c.chunk_size = 1;
            c.num_chunks_in_partition = 4;
        }
        let config = Config::new_with_random_peer_id(node_config);

        // Four 2-chunk txs: tx0 [0,2) + tx1 [2,4) start in slot 0; tx2 [4,6) +
        // tx3 [6,8) start in slot 1.
        let mk_tx = |id_byte: u8| -> DataTransactionHeader {
            let mut id = [0_u8; 32];
            id[0] = id_byte;
            DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
                tx: DataTransactionHeaderV1 {
                    id: H256::from(id),
                    data_size: 2,
                    ..Default::default()
                },
                metadata: irys_types::DataTransactionMetadata::new(),
            })
        };
        let txs = [mk_tx(1), mk_tx(2), mk_tx(3), mk_tx(4)];
        let tx_ids: Vec<H256> = txs.iter().map(|t| t.id).collect();

        fn submit_header(height: u64, total_chunks: u64, tx_ids: Vec<H256>) -> IrysBlockHeader {
            let mut h = IrysBlockHeader::new_mock_header();
            h.height = height;
            h.data_ledgers = vec![irys_types::DataTransactionLedger {
                ledger_id: DataLedger::Submit as u32,
                tx_root: H256::zero(),
                tx_ids: H256List(tx_ids),
                total_chunks,
                expires: None,
                proofs: None,
                required_proof_count: None,
            }];
            h
        }
        // Tree: genesis (Submit total 0) → block 1 (Submit total 8, the 4 txs).
        let mut genesis = submit_header(0, 0, vec![]);
        genesis.cumulative_diff = 0.into();
        genesis.test_sign();
        let mut block1 = submit_header(1, 8, tx_ids.clone());
        block1.previous_block_hash = genesis.block_hash;
        block1.cumulative_diff = 1.into();
        block1.test_sign();
        let block1_hash = block1.block_hash;
        let guard = {
            let mut tree = BlockTree::new(&genesis, irys_types::ConsensusConfig::testing());
            tree.mark_tip(&genesis.block_hash).unwrap();
            let sealed = Arc::new(SealedBlock::new_unchecked(
                Arc::new(block1.clone()),
                BlockTransactions::default(),
            ));
            tree.add_block(
                &sealed,
                Arc::new(CommitmentSnapshot::default()),
                Arc::new(EpochSnapshot::default()),
                dummy_ema_snapshot(),
            )?;
            BlockTreeReadGuard::new(Arc::new(RwLock::new(tree)))
        };

        // DB: seeded tx headers + a migrated index agreeing with the tree.
        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));
        db.update_eyre(|wtx| {
            for t in &txs {
                insert_tx_header(wtx, t)?;
            }
            Ok(())
        })?;
        let block_index = BlockIndex::new_for_testing(db.clone());
        for (height, (hash, submit_total)) in [(genesis.block_hash, 0_u64), (block1_hash, 8)]
            .into_iter()
            .enumerate()
        {
            block_index.push_item(
                &BlockIndexItem {
                    block_hash: hash,
                    num_ledgers: 2,
                    ledgers: vec![
                        irys_types::LedgerIndexItem {
                            total_chunks: 0,
                            tx_root: H256::random(),
                            ledger: DataLedger::Publish,
                        },
                        irys_types::LedgerIndexItem {
                            total_chunks: submit_total,
                            tx_root: H256::random(),
                            ledger: DataLedger::Submit,
                        },
                    ],
                },
                height as u64,
            )?;
        }

        // Slot 0 stored by miner A, slot 1 by miner B; both expired this epoch.
        let miner_a = IrysAddress::repeat_byte(0xAA);
        let miner_b = IrysAddress::repeat_byte(0xBB);
        let mut expired = BTreeMap::new();
        expired.insert(SlotIndex::new(0), vec![miner_a]);
        expired.insert(SlotIndex::new(1), vec![miner_b]);

        let range = find_block_range(
            expired,
            &config,
            &block_index,
            DataLedger::Submit,
            8, // parent_total
            block1_hash,
            &guard,
            &db,
        )?
        .expect("expired slots hold data");

        let tx_to_miners = collect_tx_to_miners_from_range(
            range,
            DataLedger::Submit,
            &config,
            &guard,
            &MempoolReadGuard::stub(),
            &db,
        )
        .await?;

        // Each tx is attributed to ONLY the miners of its start-offset slot.
        // Pre-fix every tx in the straddle block was attributed to [A, B].
        let miners_of = |id: &H256| tx_to_miners.get(id).map(|m| m.to_vec());
        assert_eq!(
            miners_of(&tx_ids[0]),
            Some(vec![miner_a]),
            "tx0 (offset 0, slot 0) → miner A only"
        );
        assert_eq!(
            miners_of(&tx_ids[1]),
            Some(vec![miner_a]),
            "tx1 (offset 2, slot 0) → miner A only"
        );
        assert_eq!(
            miners_of(&tx_ids[2]),
            Some(vec![miner_b]),
            "tx2 (offset 4, slot 1) → miner B only"
        );
        assert_eq!(
            miners_of(&tx_ids[3]),
            Some(vec![miner_b]),
            "tx3 (offset 6, slot 1) → miner B only"
        );

        Ok(())
    }

    /// NC-0042 F2 (ii) — real-fixture coverage of `resolve_submit_inclusion`'s
    /// SLOW path against an actual `BlockTree`: a candidate whose Submit inclusion
    /// is in the un-migrated tree (absent from the migrated index /
    /// `MigratedBlockHashes`, so `canonical_submit_height` returns `None`) must
    /// still be resolved — by the branch-correct by-hash parent walk — with the
    /// correct `(base, total)` offsets. This is the path the chain-test
    /// (`block_migration_depth = 1`) never reaches; fast-path resolution of a
    /// migrated inclusion is covered end-to-end by
    /// `chain-tests/.../promote_after_submit_expiry.rs`. (The below-tree-window
    /// predecessor + C1 side-fork rejection are covered by the `walk_*` unit
    /// tests, which need no heavy tree fixture.)
    #[test]
    fn resolve_submit_inclusion_slow_path_walks_untracked_tree() -> eyre::Result<()> {
        use irys_database::{IrysDatabaseArgs as _, open_or_create_db, tables::IrysTables};
        use irys_domain::{
            BlockTree, BlockTreeReadGuard, CommitmentSnapshot, EpochSnapshot, dummy_ema_snapshot,
        };
        use irys_testing_utils::IrysBlockHeaderTestExt as _;
        use irys_types::{BlockTransactions, SealedBlock};
        use reth_db::mdbx::DatabaseArguments;
        use std::sync::RwLock;

        // A header with one Submit ledger entry: cumulative chunk total + tx_ids.
        fn with_submit(height: u64, total_chunks: u64, tx_ids: Vec<H256>) -> IrysBlockHeader {
            let mut h = IrysBlockHeader::new_mock_header();
            h.height = height;
            h.data_ledgers = vec![irys_types::DataTransactionLedger {
                ledger_id: DataLedger::Submit as u32,
                tx_root: H256::zero(),
                tx_ids: irys_types::H256List(tx_ids),
                total_chunks,
                expires: None,
                proofs: None,
                required_proof_count: None,
            }];
            h
        }

        let target = H256::random();
        // Cumulative Submit totals h0=5, h1=12, h2=20. The target lands in h1, so
        // its inclusion spans Submit chunks [5, 12): base 5, total 12.
        let mut headers = [
            with_submit(0, 5, vec![]),
            with_submit(1, 12, vec![target]),
            with_submit(2, 20, vec![]),
        ];

        // Build a real BlockTree from the headers. The shared `genesis_tree`
        // helper can't be used here: it seals every block with the *checked*
        // `SealedBlock::new`, which rejects h1 (it declares `target` in its Submit
        // ledger but has an empty body). `SealedBlock::new_unchecked` (test-only)
        // skips that body check; genesis (empty ledgers) still seals via
        // `BlockTree::new`.
        let guard = {
            let mut iter = headers.iter_mut();
            let genesis = iter.next().unwrap();
            genesis.cumulative_diff = 0.into();
            genesis.test_sign();
            let mut prev = genesis.block_hash;
            let mut tree = BlockTree::new(genesis, irys_types::ConsensusConfig::testing());
            tree.mark_tip(&prev).unwrap();
            for h in iter {
                h.previous_block_hash = prev;
                h.cumulative_diff = h.height.into();
                h.test_sign();
                prev = h.block_hash;
                let sealed = Arc::new(SealedBlock::new_unchecked(
                    Arc::new(h.clone()),
                    BlockTransactions::default(),
                ));
                tree.add_block(
                    &sealed,
                    Arc::new(CommitmentSnapshot::default()),
                    Arc::new(EpochSnapshot::default()),
                    dummy_ema_snapshot(),
                )?;
            }
            BlockTreeReadGuard::new(Arc::new(RwLock::new(tree)))
        };
        let inclusion_hash = headers[1].block_hash; // h1
        let parent_hash = headers[2].block_hash; // h2 = tip = parent of block @ height 3

        // Empty db ⇒ canonical_submit_height returns None ⇒ the SLOW path runs.
        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));
        let block_index = BlockIndex::new_for_testing(db.clone());
        let config = Config::new_with_random_peer_id(irys_types::NodeConfig::testing());

        let range = ExpiredSubmitRange {
            block_height: 3,
            range_end: 12,
            parent_block_hash: parent_hash,
        };

        let inc = resolve_submit_inclusion(target, &range, &config, &block_index, &guard, &db)?
            .expect("slow path must resolve the un-migrated Submit inclusion");
        assert_eq!(
            inc.block_hash, inclusion_hash,
            "resolved the wrong inclusion block"
        );
        assert_eq!(
            inc.base, 5,
            "base = predecessor (h0) cumulative Submit total"
        );
        assert_eq!(
            inc.total, 12,
            "total = inclusion block (h1) cumulative Submit total"
        );

        // End-to-end verdict via the pure span decision (no body load needed):
        // range_end 12 ⇒ block [5,12) wholly before ⇒ expired; range_end 5 ⇒ not.
        assert_eq!(submit_span_verdict(inc.base, inc.total, 12), Some(true));
        assert_eq!(submit_span_verdict(inc.base, inc.total, 5), Some(false));

        Ok(())
    }

    /// NC-0042 contiguity guard: `expired_submit_range` collapses the expired
    /// Submit slots to a single `[0, range_end)` prefix, which is only correct if
    /// the slots are `{0..=max}`. A hole (an empty, non-last slot aged out by its
    /// allocation height beneath a still-live lower slot) would over-approximate
    /// — deterministically rejecting valid promotions AND diverging from the
    /// slot-exact refund walk. The verdict is a pure function of canonical state,
    /// so it must fail loud (identically on every node) rather than silently.
    #[test]
    fn expired_submit_range_bails_on_non_prefix_expired_set() {
        fn submit_header_total(total_chunks: u64) -> IrysBlockHeader {
            let mut h = IrysBlockHeader::new_mock_header();
            h.data_ledgers = vec![irys_types::DataTransactionLedger {
                ledger_id: DataLedger::Submit as u32,
                tx_root: H256::zero(),
                tx_ids: irys_types::H256List(vec![]),
                total_chunks,
                expires: None,
                proofs: None,
                required_proof_count: None,
            }];
            h
        }

        // Build the hole topology in real ledger state: 4 Submit slots allocated
        // at height 1, then only slot 1 touched (chunks [12,18) with a 10-chunk
        // partition) → last_height [1, 100, 1, 1]. At an expiry height in (1, 100)
        // slots 0 and 2 expire while slot 1 stays live; slot 3 is the never-
        // expiring last slot. So `get_all_expired_term_slot_indexes` returns the
        // non-prefix set {0, 2}.
        let mut epoch = EpochSnapshot::default();
        epoch.ledgers[DataLedger::Submit].allocate_slots(4, 1);
        epoch
            .ledgers
            .touch_filled_slots(DataLedger::Submit, 12, 18, 10, 100);

        let ep = &epoch.config.consensus.epoch;
        let min_blocks = ep.submit_ledger_epoch_length * ep.num_blocks_in_epoch;
        let block_height = min_blocks + 50; // expiry_height = 50 ∈ (1, 100)

        // Sanity: the fixture really is a hole before we assert the guard fires.
        assert_eq!(
            epoch.get_all_expired_term_slot_indexes(DataLedger::Submit, block_height),
            vec![0, 2],
            "fixture must produce the non-prefix expired set {{0, 2}}"
        );

        let parent = submit_header_total(25); // data through slot 2 (3 × 10 chunks)
        let err = expired_submit_range(block_height, &epoch, &parent, &epoch.config)
            .expect_err("a non-prefix expired set must fail loud, not over-approximate");
        assert!(
            err.to_string().contains("not a contiguous prefix"),
            "expected the NC-0042 contiguity guard message, got: {err}"
        );
    }

    /// NC-0042 fail-loud (commit c300d7ec2): a tx whose start offset maps to a
    /// slot absent from `slot_miners` (a non-expired slot) must `Err`, not be
    /// silently dropped — a silent under-refund would strand a refund and break
    /// Pipeline A ≡ B if the contiguity invariant ever broke.
    #[test]
    fn filter_transactions_by_chunk_range_bails_on_non_expired_slot() {
        let tx = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: H256::random(),
                data_size: 5,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });
        // tx starts at offset 0 → slot 0, but `slot_miners` is empty: no expired
        // slot covers it. Middle-block flags (no trim) so it is not skipped first.
        let slot_miners: BTreeMap<SlotIndex, Arc<Vec<IrysAddress>>> = BTreeMap::new();
        let err = filter_transactions_by_chunk_range(
            vec![tx],
            LedgerChunkOffset::from(0_u64),
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 100)),
            false, // is_earliest — no start trim
            false, // is_latest — no end trim
            1,     // chunk_size
            10,    // num_chunks_in_partition
            &slot_miners,
        )
        .expect_err("a tx mapping to a non-expired slot must fail loud");
        assert!(
            err.to_string().contains("non-expired slot"),
            "expected the NC-0042 fail-loud message, got: {err}"
        );
    }
}
