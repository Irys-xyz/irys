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

use crate::block_ancestry::{block_header_from_tree_then_db, walk_ancestors_tree_then_db};
use crate::block_discovery::get_data_tx_in_parallel;
use crate::mempool_guard::MempoolReadGuard;
use crate::shadow_tx_generator::RollingHash;
use eyre::{OptionExt as _, ensure, eyre};
use irys_database::{
    canonical_promoted_height, canonical_submit_height, db::IrysDatabaseExt as _,
    reth_db::transaction::DbTx as _, tables::MigratedBlockHashes,
};
use irys_domain::{BlockIndex, BlockTreeReadGuard, EpochSnapshot};
use irys_types::{
    BlockHeight, BlockIndexItem, Config, DataLedger, DataTransactionHeader, H256, IrysAddress,
    IrysBlockHeader, IrysTransactionId, LedgerChunkOffset, LedgerChunkRange, U256,
    app_state::DatabaseProvider, fee_distribution::TermFeeCharges, ledger_chunk_offset_ii,
};
use nodit::{InclusiveInterval as _, interval::ii};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
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
        parent_block_header.ledger_total_chunks(ledger_type),
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

    // NC-0042 P0: resolve which of these txs were promoted **on this block's own
    // branch** before computing refunds. The refund-suppression decision must
    // never read `DataTransactionHeader::promoted_height` from the fetched header
    // — `get_data_tx_in_parallel` prefers the node-local mempool copy, where
    // `promoted_height` is set at block confirmation within the reorg window
    // (`mempool_service::apply_block_confirmed_updates`) and is therefore
    // branch-variant: two forks contesting the slot-expiry boundary would
    // disagree about whether a tx was promoted → divergent refund sets →
    // consensus fork. `resolve_promoted_on_branch` answers purely from the
    // block's parent ancestry (by-hash), mirroring `resolve_submit_inclusion`.
    // Only the Submit ledger refunds (`expect_txs_to_be_promoted`); for term
    // ledgers the walk is skipped entirely.
    let promoted_on_branch = if expect_txs_to_be_promoted {
        resolve_promoted_on_branch(
            &all_tx_ids,
            parent_block_header.block_hash,
            block_height,
            config,
            block_tree_guard,
            db,
        )?
    } else {
        BTreeSet::new()
    };

    // No sort here: `aggregate_balance_deltas` sorts by `compare_tx` (it owns the
    // "refunds sorted by tx_id" determinism invariant), so a sort at this site is
    // redundant.
    let transactions = get_data_tx_in_parallel(all_tx_ids, mempool_guard, db).await?;

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
        &promoted_on_branch,
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

/// Producer/validator-shared orchestration of [`calculate_expired_ledger_fees`]
/// across the expiring data ledgers: always the Submit ledger (promotion expected
/// → perm-fee refunds for unpromoted txs), plus — only when Cascade is active for
/// the epoch — the OneYear and ThirtyDay term ledgers (no promotion). Returns the
/// merged delta.
///
/// This is the single home for the three-call pattern so producer↔validator parity
/// is **mechanical, not hand-maintained**: both sides pass the same
/// `cascade_active_for_block` (the block's OWN timestamp — gates the write-window
/// exclusion to match `touch_active_ledger_slots`, and must NOT be the
/// epoch-lagging `is_cascade_active_for_epoch`) and `cascade_active_for_epoch`
/// (gates whether the term ledgers settle at all). The ONLY legitimate difference —
/// each ledger's cumulative `total_chunks` at this block — is supplied by
/// `new_total_chunks`: the producer computes `parent + chunks_added`; the validator
/// reads the same value from the header after prevalidation has checked it equals
/// that formula.
pub async fn calculate_all_expired_ledger_fees(
    parent_epoch_snapshot: &EpochSnapshot,
    parent_block_header: &IrysBlockHeader,
    block_height: u64,
    config: &Config,
    block_index: &BlockIndex,
    block_tree_guard: &BlockTreeReadGuard,
    mempool_guard: &MempoolReadGuard,
    db: &DatabaseProvider,
    cascade_active_for_block: bool,
    cascade_active_for_epoch: bool,
    new_total_chunks: impl Fn(DataLedger) -> u64,
) -> eyre::Result<LedgerExpiryBalanceDelta> {
    let mut result = calculate_expired_ledger_fees(
        parent_epoch_snapshot,
        parent_block_header,
        block_height,
        DataLedger::Submit,
        config,
        block_index.clone(),
        block_tree_guard,
        mempool_guard,
        db,
        true, // Submit: expect promotion — refund the perm fee for unpromoted txs
        new_total_chunks(DataLedger::Submit),
        cascade_active_for_block,
    )
    .await?;

    // Term ledgers (no promotion) only settle once Cascade is active for the epoch.
    if cascade_active_for_epoch {
        for ledger in [DataLedger::OneYear, DataLedger::ThirtyDay] {
            let delta = calculate_expired_ledger_fees(
                parent_epoch_snapshot,
                parent_block_header,
                block_height,
                ledger,
                config,
                block_index.clone(),
                block_tree_guard,
                mempool_guard,
                db,
                false, // term ledgers: no promotion expected
                new_total_chunks(ledger),
                cascade_active_for_block,
            )
            .await?;
            result.merge(delta);
        }
    }

    Ok(result)
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

    // Process the earliest boundary. For the sole-block case (min == max), process
    // it as BOTH earliest and latest so both ends of the expired range are trimmed.
    // (Trimming only the start — the old behavior — over-included the next slot's
    // txs, which diverged from the exact per-candidate promotion filter; NC-0042 §4b.)
    let earliest_miners = process_boundary_block(
        &block_range.min_block,
        true,       // is_earliest
        same_block, // is_latest only when this is also the sole (max) block
        ledger_type,
        config,
        slot_miners,
        block_tree_guard,
        mempool_guard,
        db,
    )
    .await?;

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

    let mut tx_to_miners = BTreeMap::new();
    tx_to_miners.extend(earliest_miners);

    // Different blocks: process the latest boundary separately and extend.
    if !same_block {
        let latest_miners = process_boundary_block(
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
        tracing::info!(
            "Collected transactions: earliest={}, latest={}, middle={}",
            tx_to_miners.len(),
            latest_miners.len(),
            middle_miners.len(),
        );
        tx_to_miners.extend(latest_miners);
    } else {
        tracing::info!(
            "Collected transactions: earliest={}, middle={}",
            tx_to_miners.len(),
            middle_miners.len(),
        );
    }

    tx_to_miners.extend(middle_miners);

    Ok(tx_to_miners)
}

/// NC-0042 §4b/§4c: the set of Submit-ledger transaction ids whose storage has
/// **expired as of `block_height`** — i.e. txs whose Submit storage is already
/// expired at this block height, so they are perm-fee-refunded (now or at the
/// next epoch settlement) and therefore must never be promoted.
///
/// Near-expiry is not disqualifying by itself: a tx may still be promoted even
/// if its lowest slot is close to expiry, because its storage can span later
/// slots and remains promotable until it has actually expired.
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
    // Cascade status of the block being produced/validated (its OWN timestamp),
    // NOT the parent epoch snapshot's. Must mirror exactly what production passes
    // to `expired_submit_range`, so this differential-test oracle stays faithful
    // at the activation epoch boundary (NC-0042) — previously this derived it from
    // `parent_epoch_snapshot.epoch_block.timestamp_secs()`, which lags the block.
    cascade_active: bool,
    block_index: BlockIndex,
    block_tree_guard: &BlockTreeReadGuard,
    mempool_guard: &MempoolReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<std::collections::BTreeSet<IrysTransactionId>> {
    let slot_indexes = parent_epoch_snapshot.get_all_expired_term_slot_indexes(
        DataLedger::Submit,
        block_height,
        cascade_active,
        parent_block_header.ledger_total_chunks(DataLedger::Submit),
    );
    if slot_indexes.is_empty() {
        return Ok(std::collections::BTreeSet::new());
    }

    // Miners are irrelevant here — only the tx-id keys are read. The walk
    // includes a slot's txs whenever the slot is present in the map (an empty
    // miner list works too — see the minerless-slot settlement path); the
    // sentinel address is never read.
    let expired_slots: BTreeMap<SlotIndex, Vec<IrysAddress>> = slot_indexes
        .into_iter()
        .map(|i| (SlotIndex::new(i as u64), vec![IrysAddress::ZERO]))
        .collect();

    let block_range = match find_block_range(
        expired_slots,
        config,
        &block_index,
        DataLedger::Submit,
        parent_block_header.ledger_total_chunks(DataLedger::Submit),
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
    // The block-under-test's own Cascade status — see `expired_submit_tx_ids`.
    cascade_active: bool,
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
        cascade_active,
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
/// per candidate, in [`submit_tx_expired`]. `cascade_active` MUST be the
/// Cascade status of the block being produced/validated; pre-Cascade replay
/// keeps the old allocation-anchored unwritten-slot expiry behavior.
pub fn expired_submit_range(
    block_height: u64,
    parent_epoch_snapshot: &EpochSnapshot,
    parent_block_header: &IrysBlockHeader,
    config: &Config,
    cascade_active: bool,
) -> eyre::Result<Option<ExpiredSubmitRange>> {
    let parent_total = parent_block_header.ledger_total_chunks(DataLedger::Submit);
    let slot_indexes = parent_epoch_snapshot.get_all_expired_term_slot_indexes(
        DataLedger::Submit,
        block_height,
        cascade_active,
        parent_total,
    );
    let Some(&max_expired_slot) = slot_indexes.iter().max() else {
        return Ok(None);
    };

    // Expired Submit slots must be a prefix of the written data. Post-Cascade
    // only fully-written slots expire (the write frontier's slot stays live),
    // unwritten preallocated slots never expire, and Submit data is append-only,
    // so once a higher slot has written data, later writes cannot return to keep
    // a lower slot live while that higher written slot expires. If this guard
    // ever fires, the promotion filter would over-approximate real expired txs
    // and must fail loud rather than silently diverge.
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
    // Fail loud on a zero partition size rather than silently disabling the
    // §4b/§4c filter: with `p == 0`, `range_end` would collapse to 0 and this
    // function would return `Ok(None)` (no candidate filtered), while the refund
    // walk (`filter_transactions_by_chunk_range`) bails — an asymmetric silent
    // bypass. `Config::validate` also rejects `p == 0`; this guards any path that
    // bypassed validation (NC-0042).
    if p == 0 {
        eyre::bail!("num_chunks_in_partition must be non-zero for Submit-expiry range math");
    }
    // `max_expired_slot` is a slot index (usize); the checked conversion documents
    // the invariant and `saturating_add` keeps the lone `+1` from wrapping.
    let max_expired_slot = u64::try_from(max_expired_slot)
        .map_err(|_| eyre::eyre!("expired Submit slot index does not fit in u64"))?;
    let range_end = max_expired_slot
        .saturating_add(1)
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
/// parent ancestry** (migrated inclusions via `canonical_submit_height`, branch-
/// invariant for the already-expired inclusions this acts on — below the reorg
/// floor; see `resolve_submit_inclusion`; un-migrated inclusions via a by-hash
/// parent walk — never the node's canonical tip or migration-lagged index). The verdict
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
/// Fast path: `canonical_submit_height` resolves migrated inclusions in O(1)
/// from the canonical index / `MigratedBlockHashes`. Migrated rows are only
/// branch-invariant BELOW the reorg floor — a deep reorg can rewrite
/// them up to `block_tree_depth` deep. This is safe HERE because
/// `submit_tx_expired` only acts on *expired* Submit slots, and `Config::validate`
/// requires a slot's lifetime (`submit_ledger_epoch_length * num_blocks_in_epoch`)
/// to exceed `block_tree_depth`, so any expired tx's inclusion sits below the
/// reorg floor and the height is authoritative on every branch. (See the
/// fork-awareness note above `canonical_metadata_height` in `database.rs`.)
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
    // Fast path: migrated inclusion (branch-invariant below the reorg floor, where
    // the slot-lifetime invariant keeps expired txs — see the doc above). Capped at
    // the parent — Submit inclusion is strictly before the block being evaluated.
    let max_height = range.block_height.saturating_sub(1);
    if let Some(h) = db.view_eyre(|tx| canonical_submit_height(tx, &txid, max_height))? {
        let item = block_index
            .get_item(h)
            .ok_or_eyre("canonical_submit_height returned a height absent from the block index")?;
        let total = ledger_total_of_index_item(&item, DataLedger::Submit);
        let base = if h == 0 {
            0
        } else {
            block_index
                .get_item(h - 1)
                .map(|i| ledger_total_of_index_item(&i, DataLedger::Submit))
                .ok_or_eyre(
                    "migrated predecessor must be indexed \
                     (below the reorg floor → finalized)",
                )?
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
                submit_total: header.ledger_total_chunks(DataLedger::Submit),
                includes_txid: block_includes_ledger_tx(header, DataLedger::Submit, &txid),
            })
        },
        |height| {
            block_index
                .get_item(height)
                .map(|item| ledger_total_of_index_item(&item, DataLedger::Submit))
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
                    return Err(c1_non_canonical_ancestor_err(prev_height, block.prev_hash));
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

/// Whether `header` includes `txid` in `ledger`'s per-block `tx_ids`.
///
/// For `Submit` this is plain membership. For `Publish` it is the **branch-correct
/// promotion signal**: a tx's promotion is recorded by adding its id to the
/// promoting block's Publish ledger (`mempool_service` derives the mempool
/// `promoted_height` from exactly this set), so Publish membership on a branch is
/// the branch-correct "was this tx promoted here?" answer.
fn block_includes_ledger_tx(
    header: &IrysBlockHeader,
    ledger: DataLedger,
    txid: &IrysTransactionId,
) -> bool {
    header
        .get_data_ledger_tx_ids_ordered(ledger)
        .is_some_and(|ids| ids.contains(txid))
}

/// Branch-correct set of `candidate_txids` that were **promoted on the
/// block-being-settled's own branch** — the refund-suppression analogue of
/// [`resolve_submit_inclusion`] and the fix for the NC-0042 P0 consensus-fork
/// vector.
///
/// The fetched [`DataTransactionHeader::promoted_height`] is node-local mempool
/// state: it is set at block confirmation *within the reorg window*
/// (`mempool_service::apply_block_confirmed_updates`) and cleared only on reorg,
/// so it is **branch-variant**. Gating a perm-fee refund on it lets two forks
/// contesting a slot-expiry boundary compute different refund sets — a divergent
/// shadow-tx/treasury set → consensus fork. The answer here is instead a pure
/// function of the block's **own parent ancestry** (`parent_block_hash`), never
/// the node's canonical tip or live mempool.
///
/// Primary path — branch walk: from `parent_block_hash`, walk the parent chain
/// **by hash** (retained tree first, DB fallback for tree-evicted headers — as in
/// the validator's `get_previous_tx_inclusions`) over the full reorg window
/// ([`prior_inclusion_walk_depth`] = `max(tx_anchor_expiry_depth,
/// block_tree_depth)`), marking any candidate found in a block's Publish ledger
/// as promoted. Walking the entire reorg window guarantees every branch-variant
/// (recent) promotion is resolved here by-hash on this block's own branch.
///
/// Finalized fallback — [`canonical_promoted_height`]: for candidates NOT found
/// by the walk, consult the MBH-verified canonical promotion height, capped
/// strictly **below the walked window**. Below the reorg floor
/// `MigratedBlockHashes` is branch-invariant (see `canonical_metadata_height` in
/// `irys_database`), so an old finalized promotion (one made long before expiry,
/// outside the reorg window) resolves identically on every node/branch.
pub(crate) fn resolve_promoted_on_branch(
    candidate_txids: &[IrysTransactionId],
    parent_block_hash: H256,
    block_height: u64,
    config: &Config,
    block_tree_guard: &BlockTreeReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<BTreeSet<IrysTransactionId>> {
    // No candidates → nothing to resolve; skip the tree/DB ancestry walk entirely.
    if candidate_txids.is_empty() {
        return Ok(BTreeSet::new());
    }

    let walk_depth = crate::block_validation::prior_inclusion_walk_depth(config);
    let walk_min_height = block_height.saturating_sub(walk_depth);

    let mut remaining: BTreeSet<IrysTransactionId> = candidate_txids.iter().copied().collect();
    let mut promoted: BTreeSet<IrysTransactionId> = BTreeSet::new();

    // Primary path: by-hash parent walk over the reorg window. An in-window
    // ancestor missing from BOTH the tree and the DB is a local inconsistency
    // and fails loud (classified NodeFault upstream), never a silent
    // under-suppression that could diverge from a healthy node.
    walk_ancestors_tree_then_db(
        block_tree_guard,
        db,
        parent_block_hash,
        walk_min_height,
        |header| {
            remaining.retain(|txid| {
                if block_includes_ledger_tx(header, DataLedger::Publish, txid) {
                    promoted.insert(*txid);
                    false
                } else {
                    true
                }
            });
            Ok(if remaining.is_empty() {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            })
        },
    )
    .map_err(|err| {
        eyre!(
            "expiry promotion walk from parent {parent_block_hash} below height \
             {block_height} failed: {err}"
        )
    })?;

    // Finalized fallback: a promotion older than the walked window is below the
    // reorg floor, where MBH (and thus `canonical_promoted_height`) is
    // branch-invariant. Cap strictly below the window so a reorg-mutable height
    // can never be trusted here — the walk already covered that band by hash.
    // (The validator's sibling fallback in `data_txs_are_valid` caps the same
    // way — strictly below its own walk window, not just at the reorg floor.
    // Keep the two cap forms in sync when the walk depth or reorg ceiling
    // changes.)
    if !remaining.is_empty() {
        let max_height = walk_min_height.saturating_sub(1);
        db.view_eyre(|tx| {
            for txid in &remaining {
                if canonical_promoted_height(tx, txid, max_height)?.is_some() {
                    promoted.insert(*txid);
                }
            }
            Ok(())
        })?;
    }

    Ok(promoted)
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

/// Unified C1 side-fork error: the expiry walk reached a block that is not the
/// canonical block at `height` — i.e. it descended onto a side-fork ancestor off
/// the validated block's own branch. Shared by the pure walk
/// (`walk_submit_inclusion`) and [`assert_canonical_via_mbh`].
fn c1_non_canonical_ancestor_err(height: u64, hash: H256) -> eyre::Report {
    eyre::eyre!(
        "expiry walk reached a non-canonical ancestor at height {height} \
         (supplied {hash}) — off the validated block's branch (C1 guard)"
    )
}

/// C1 side-fork guard: confirm `hash` is the canonical block at `height` per
/// `MigratedBlockHashes` before trusting a height-keyed (canonical-only) index
/// lookup for it. Without this, a walk that descended onto a side-fork ancestor
/// below the block-tree window would attribute the canonical block's chunks to
/// the wrong block.
fn assert_canonical_via_mbh(db: &DatabaseProvider, hash: H256, height: u64) -> eyre::Result<()> {
    if !is_canonical_at(db, hash, height)? {
        return Err(c1_non_canonical_ancestor_err(height, hash));
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
/// is ever used for the forky region. The fast/slow split is branch-safe for the
/// offsets this is called with: callers only resolve *expired* partitions, which
/// the slot-lifetime > `block_tree_depth` config invariant keeps below the reorg
/// floor, so a migrated offset's block is finalized and shared by every branch
/// (migrated rows above the floor are reorg-mutable, but expired
/// offsets never reach there); an un-migrated offset's block is on the parent's
/// own chain.
fn resolve_ledger_offset_to_block(
    offset: u64,
    parent_hash: H256,
    ledger: DataLedger,
    block_tree_guard: &BlockTreeReadGuard,
    block_index: &BlockIndex,
    db: &DatabaseProvider,
) -> eyre::Result<(BlockHeight, H256, u64, u64)> {
    // Fast path: migrated region (index binary search). Branch-invariant for the
    // expired offsets this is called with — kept below the reorg floor by the
    // slot-lifetime > block_tree_depth config invariant; see the doc above.
    let index_tip_total = block_index
        .get_latest_item()
        .map(|item| ledger_total_of_index_item(&item, ledger))
        .unwrap_or(0);
    if offset < index_tip_total {
        return resolve_offset_via_index(block_index, ledger, offset);
    }

    // Slow path: un-migrated tail — walk the parent ancestry by hash.
    let tree = block_tree_guard.read();
    let mut cursor = parent_hash;
    while let Some(header) = tree.get_block(&cursor) {
        let total = header.ledger_total_chunks(ledger);
        let prev_total = if header.height == 0 {
            0
        } else if let Some(prev) = tree.get_block(&header.previous_block_hash) {
            prev.ledger_total_chunks(ledger)
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
    resolve_offset_via_index(block_index, ledger, offset)
}

/// Resolve `offset` to its introducing block via the **finalized block index**,
/// returning `(height, block_hash, base, total)`.
///
/// Uses [`BlockIndex::get_block_bounds`] (anchored at the latest indexed height),
/// NOT [`BlockIndex::get_block_index_item`]. The two binary-search the same index
/// but diverge on a probed block whose entry for `ledger` is absent:
/// `get_block_index_item` errors, while `get_block_bounds` treats it as
/// `total_chunks = 0` and keeps searching. That tolerance is required for ledgers
/// introduced after genesis — `OneYear`/`ThirtyDay` at a **mid-chain Cascade
/// activation**: a pre-activation block carries only `Publish`+`Submit`, so once
/// a term slot ages into the migrated index and expires, the strict variant would
/// error the instant the search probes a pre-activation height, bubbling out of
/// both the producer and validator epoch-block expiry walk → a deterministic
/// chain stall. Only called for offsets below the migrated index tip (finalized ⇒
/// branch-invariant), so anchoring at the latest indexed height is branch-safe.
/// `base` (the predecessor's total) comes straight from the bounds, which compute
/// it the same way the old `migrated_predecessor_total` did (0 at genesis or when
/// the predecessor predates the ledger's introduction).
fn resolve_offset_via_index(
    block_index: &BlockIndex,
    ledger: DataLedger,
    offset: u64,
) -> eyre::Result<(BlockHeight, H256, u64, u64)> {
    let bounds = block_index.get_block_bounds(ledger, LedgerChunkOffset::from(offset))?;
    Ok((
        bounds.height,
        bounds.block_hash,
        bounds.start_chunk_offset,
        bounds.end_chunk_offset,
    ))
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

/// Fetches a block header from the block tree or database, acquiring the read guard
/// for a single lookup. For walks that hold the guard across many lookups, call
/// [`block_header_from_tree_then_db`] directly with the held guard.
#[tracing::instrument(level = "trace", skip_all, fields(block.hash = %block_hash))]
async fn get_block_by_hash(
    block_hash: H256,
    block_tree_guard: &BlockTreeReadGuard,
    db: &DatabaseProvider,
) -> eyre::Result<IrysBlockHeader> {
    block_header_from_tree_then_db(&block_tree_guard.read(), db, &block_hash)?
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
    //
    // Slot-keyed, NOT partition-keyed: a slot whose replicas were all unpledged
    // before expiry (and never backfilled) recycles with an empty `partitions`
    // vec. It must still settle — its unpromoted txs are refunded — even though
    // there are no miners to pay term fees to. A partition-keyed walk would
    // never see the slot, stranding the refunds while the slot's txs stay
    // promotion-blocked forever (`get_all_expired_slot_indexes`).
    let expiring_slots = parent_epoch_snapshot.get_expiring_slot_partitions(
        block_height,
        target_ledger_type,
        new_total_chunks,
        cascade_active,
    );
    let mut expired_ledger_slot_indexes = BTreeMap::new();

    tracing::debug!(
        "collect_expired_partitions: block_height={}, target_ledger={:?}, found {} expiring slots",
        block_height,
        target_ledger_type,
        expiring_slots.len()
    );

    for (slot_index, partition_hashes) in expiring_slots {
        let slot_index = SlotIndex::new(slot_index as u64);

        // Store miner_address (not reward_address) to preserve unique miner identities
        // for correct fee distribution. Reward address resolution is deferred to
        // aggregate_balance_deltas to ensure pooled miners (sharing a reward address)
        // are counted individually for fee splitting.
        let mut miners = Vec::with_capacity(partition_hashes.len());
        for partition_hash in partition_hashes {
            let partition = partition_assignments
                .get_assignment(partition_hash)
                .ok_or_eyre("could not get expired partition")?;

            tracing::info!(
                "Found expired partition for {:?} ledger at slot_index={}, miner={:?}",
                target_ledger_type,
                slot_index.0,
                partition.miner_address
            );
            miners.push(partition.miner_address);
        }

        expired_ledger_slot_indexes.insert(slot_index, miners);
    }

    Ok(expired_ledger_slot_indexes)
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
            // from the refund/fee walk (NC-0042). A well-formed index always has
            // `block_total > chunk_offset` (the offset lies within this block);
            // fail loud on a corrupted index instead of spinning forever on a
            // non-advancing probe.
            ensure!(
                block_total > chunk_offset,
                "non-advancing block-index probe in {:?} ledger: block at height {} \
                 resolved for chunk offset {} has cumulative total {} (must exceed \
                 the probed offset) — corrupted ledger index",
                ledger_type,
                height,
                chunk_offset,
                block_total,
            );
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
    // `data_size.div_ceil(chunk_size)` below panics on a zero divisor. `chunk_size`
    // is a genesis-fixed consensus constant (never 0 in a real config), but guard
    // it loudly and deterministically rather than risk a producer/validator panic —
    // matching the `chunk_size == 0` guard in `submit_tx_start_offset`.
    if chunk_size == 0 {
        eyre::bail!("chunk_size must be non-zero for ledger-expiry chunk math");
    }
    // `*tx_start / num_chunks_in_partition` below would panic or yield all-slot-0
    // on a zero divisor. Guard loudly so a misconfigured partition size fails
    // deterministically rather than silently misattributing every tx to slot 0.
    if num_chunks_in_partition == 0 {
        eyre::bail!("num_chunks_in_partition must be non-zero for ledger-expiry chunk math");
    }

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
        // saturating: a malformed/huge data_size must not wrap the cumulative
        // offset in release builds (the comparisons below would then misclassify).
        let tx_end = LedgerChunkOffset::from((*current_offset).saturating_add(chunks));

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
/// - `promoted_on_branch`: tx ids that were promoted on the block-being-settled's
///   own branch (resolved branch-correctly by [`resolve_promoted_on_branch`]). A
///   perm-fee refund is suppressed iff the tx is in this set. Pre-computed (rather
///   than read from `DataTransactionHeader::promoted_height`) because the fetched
///   header's `promoted_height` is node-local mempool state and branch-variant.
fn aggregate_balance_deltas(
    mut transactions: Vec<DataTransactionHeader>,
    tx_to_miners: &BTreeMap<IrysTransactionId, Arc<Vec<IrysAddress>>>,
    epoch_snapshot: &EpochSnapshot,
    config: &Config,
    expect_txs_to_be_promoted: bool,
    promoted_on_branch: &BTreeSet<IrysTransactionId>,
) -> eyre::Result<LedgerExpiryBalanceDelta> {
    let mut balance_delta = LedgerExpiryBalanceDelta::default();
    transactions.sort_by(irys_types::DataTransactionHeader::compare_tx); // This ensures refunds will be sorted by tx_id

    for data_tx in transactions.iter() {
        let miners_that_stored_this_tx = tx_to_miners
            .get(&data_tx.id)
            .ok_or_else(|| eyre!("Missing miner list for transaction {}", data_tx.id))?;

        // process miner balance increments for storing the term tx
        // (skipped when the expired slot has NO miners — every replica unpledged
        // before expiry. There is no one to distribute term fees to, so they
        // stay in the treasury; the user perm-fee refunds below still settle.)
        if !miners_that_stored_this_tx.is_empty() {
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

        // process refunds of perm fee if the tx was not promoted **on this
        // branch**. Promotion is resolved branch-correctly upstream
        // (`resolve_promoted_on_branch`); we must NOT consult
        // `data_tx.promoted_height()` here — it is node-local mempool state and
        // would fork the refund set across competing branches (NC-0042 P0).
        {
            if !promoted_on_branch.contains(&data_tx.id) {
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
                    "Tx was promoted on this branch, no refund needed",
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
        let start =
            LedgerChunkOffset::from(self.0.saturating_mul(chunks_per_partition)).min(max_offset);
        // saturating_add for parity with the saturating_mul above; the following
        // .min(max_offset) clamps to real on-chain totals, so saturation can never
        // change a reachable result (overflow needs totals near u64::MAX).
        let end = LedgerChunkOffset::from((*start).saturating_add(chunks_per_partition));
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
    use irys_domain::{CommitmentState, PartitionAssignments, StakeEntry};
    use irys_types::{
        CommitmentStatus, DataTransactionHeaderV1, H256List, NodeConfig, UnixTimestamp,
        hardfork_config::Cascade,
    };

    /// Shared fixture: an otherwise-empty `EpochSnapshot` seeded from `config`.
    fn empty_epoch_snapshot(config: &Config) -> EpochSnapshot {
        EpochSnapshot {
            ledgers: irys_database::Ledgers::new(&config.consensus, false),
            partition_assignments: PartitionAssignments::new(),
            all_active_partitions: Vec::new(),
            unassigned_partitions: Vec::new(),
            storage_submodules_config: None,
            config: config.clone(),
            commitment_state: CommitmentState::default(),
            epoch_block: IrysBlockHeader::default(),
            previous_epoch_block: None,
            expired_partition_infos: None,
            epoch_height: 0,
        }
    }

    /// Shared fixture: a mock epoch-boundary header carrying only a Submit
    /// `total_chunks` update (no txs of its own).
    fn submit_epoch_header(height: u64, total_chunks: u64) -> IrysBlockHeader {
        let mut h = IrysBlockHeader::new_mock_header();
        h.height = height;
        h.timestamp = irys_types::UnixTimestampMs::from_millis((height as u128) * 1000);
        h.data_ledgers[DataLedger::Submit].total_chunks = total_chunks;
        h.data_ledgers[DataLedger::Submit].tx_ids = H256List::new();
        h
    }

    /// Shared fixture: a `Config` with Cascade active from genesis and small
    /// chunk/partition sizes so tests can drive slot boundaries with tiny data sizes.
    fn config_with_cascade() -> Config {
        let mut node_config = NodeConfig::testing();
        let consensus = node_config.consensus.get_mut();
        consensus.chunk_size = 1;
        consensus.num_chunks_in_partition = 10;
        consensus.hardforks.cascade = Some(Cascade {
            activation_timestamp: UnixTimestamp::from_secs(0),
            one_year_epoch_length: 365,
            thirty_day_epoch_length: 30,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
        Config::new_with_random_peer_id(node_config)
    }

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
            &BTreeSet::new(),
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

        let result = aggregate_balance_deltas(
            vec![tx1],
            &tx_to_miners,
            &epoch_snapshot,
            &config,
            false,
            &BTreeSet::new(),
        )
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

    /// NC-0042 R4 amount-attribution: when distinct miners own different expired
    /// slots (hence different txs), each miner's reward must be exactly the
    /// treasury slice of ITS slot's txs — never cross-credited to the other
    /// miner. The straddle MAPPING (tx start offset → slot → miner) is proven by
    /// `collect_attributes_each_tx_to_its_start_offset_slot_miners`; this composes
    /// that mapping with `aggregate_balance_deltas` to confirm the resulting
    /// per-miner reward AMOUNTS. (A full multi-node produced-block variant that
    /// decodes the on-chain reward shadow-txs is a reasonable follow-up — the
    /// refund harness is single-miner and cannot place two miners' slots in one
    /// boundary block without partition-assignment control it does not expose.)
    #[test]
    fn aggregate_attributes_each_slots_fee_to_its_own_miner() {
        let node_config = irys_types::NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);

        let miner_a = IrysAddress::repeat_byte(0xAA);
        let miner_b = IrysAddress::repeat_byte(0xBB);

        // tx0,tx1 stored by miner A (slot 0); tx2,tx3 by miner B (slot 1) — the
        // outcome of the per-slot start-offset attribution on a straddle block.
        let mk = |id_byte: u8, term_fee: u64| -> DataTransactionHeader {
            DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
                tx: DataTransactionHeaderV1 {
                    id: H256::from([id_byte; 32]),
                    term_fee: U256::from(term_fee).into(),
                    data_size: 100,
                    ..Default::default()
                },
                metadata: irys_types::DataTransactionMetadata::new(),
            })
        };
        let txs = [mk(1, 1000), mk(2, 2000), mk(3, 4000), mk(4, 8000)];

        let mut tx_to_miners = BTreeMap::new();
        tx_to_miners.insert(txs[0].id, Arc::new(vec![miner_a]));
        tx_to_miners.insert(txs[1].id, Arc::new(vec![miner_a]));
        tx_to_miners.insert(txs[2].id, Arc::new(vec![miner_b]));
        tx_to_miners.insert(txs[3].id, Arc::new(vec![miner_b]));

        let epoch_snapshot = EpochSnapshot::default();
        let result = aggregate_balance_deltas(
            txs.to_vec(),
            &tx_to_miners,
            &epoch_snapshot,
            &config,
            false,
            &BTreeSet::new(),
        )
        .unwrap();

        // Single miner per tx ⇒ that miner gets the whole treasury slice. Compute
        // it via TermFeeCharges so the assertion tracks the real fee model.
        let treasury = |term_fee: u64| -> U256 {
            TermFeeCharges::new(U256::from(term_fee).into(), &config.consensus)
                .unwrap()
                .distribution_on_expiry(&[IrysAddress::ZERO])
                .unwrap()[0]
        };
        assert_eq!(
            result.reward_balance_increment.get(&miner_a).unwrap().0,
            treasury(1000).saturating_add(treasury(2000)),
            "miner A receives exactly slot-0 txs' treasury slices"
        );
        assert_eq!(
            result.reward_balance_increment.get(&miner_b).unwrap().0,
            treasury(4000).saturating_add(treasury(8000)),
            "miner B receives exactly slot-1 txs' treasury slices"
        );
        assert_eq!(
            result.reward_balance_increment.len(),
            2,
            "exactly the two slot owners are credited — no cross-crediting"
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

    /// NC-0042 (mid-chain Cascade): `find_block_range` must resolve an expired
    /// OneYear/ThirtyDay chunk offset even though the block index contains
    /// PRE-activation items that carry only Publish+Submit (no term-ledger entry).
    ///
    /// Regression: `resolve_ledger_offset_to_block`'s index fast path used
    /// `get_block_index_item`, whose binary search errors ("Ledger OneYear not
    /// found in block at height N") the instant it probes a pre-activation
    /// 2-ledger block. On a network where Cascade activates mid-chain, once a
    /// OneYear/ThirtyDay slot ages into the migrated index and expires, that error
    /// bubbles out of BOTH the producer and the validator epoch-block expiry path
    /// → a deterministic chain stall. The tolerant `get_block_bounds` treats a
    /// probed block lacking the ledger as `total_chunks = 0`, so resolution
    /// converges on the introducing block instead of erroring.
    #[test]
    fn find_block_range_resolves_term_ledger_offset_across_preactivation_index() -> eyre::Result<()>
    {
        use irys_database::{IrysDatabaseArgs as _, open_or_create_db, tables::IrysTables};
        use irys_domain::{BlockTree, BlockTreeReadGuard};
        use irys_testing_utils::IrysBlockHeaderTestExt as _;
        use reth_db::mdbx::DatabaseArguments;
        use std::sync::RwLock;

        let mut node_config = irys_types::NodeConfig::testing();
        node_config.consensus.get_mut().num_chunks_in_partition = 10;
        let config = Config::new_with_random_peer_id(node_config);

        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));
        let block_index = BlockIndex::new_for_testing(db.clone());

        // Heights 0-2: PRE-Cascade — only Publish + Submit, no OneYear entry.
        for (height, submit_total) in [0_u64, 5, 8].into_iter().enumerate() {
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
        // Heights 3-5: POST-Cascade — OneYear introduced, cumulative totals 5,12,18.
        for (idx, one_year_total) in [5_u64, 12, 18].into_iter().enumerate() {
            block_index.push_item(
                &BlockIndexItem {
                    block_hash: H256::random(),
                    num_ledgers: 4,
                    ledgers: vec![
                        irys_types::LedgerIndexItem {
                            total_chunks: 0,
                            tx_root: H256::random(),
                            ledger: DataLedger::Publish,
                        },
                        irys_types::LedgerIndexItem {
                            total_chunks: 8,
                            tx_root: H256::random(),
                            ledger: DataLedger::Submit,
                        },
                        irys_types::LedgerIndexItem {
                            total_chunks: one_year_total,
                            tx_root: H256::random(),
                            ledger: DataLedger::OneYear,
                        },
                        irys_types::LedgerIndexItem {
                            total_chunks: 0,
                            tx_root: H256::random(),
                            ledger: DataLedger::ThirtyDay,
                        },
                    ],
                },
                3 + idx as u64,
            )?;
        }

        // Expired OneYear slot 0 ([0,10)). Offset 0 < the index-tip OneYear total
        // (18), so resolution takes the index fast path, whose binary search over
        // heights [0,5] probes pre-activation height 2 (which has no OneYear entry).
        let mut expired = BTreeMap::new();
        expired.insert(SlotIndex::new(0), vec![IrysAddress::ZERO]);

        // All resolved offsets lie below the migrated tip, so the tree/parent are
        // unused (same setup as `find_block_range_bounds_by_parent_not_index_tip`).
        let mut genesis = IrysBlockHeader::new_mock_header();
        genesis.height = 0;
        genesis.cumulative_diff = 0.into();
        genesis.test_sign();
        let guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(BlockTree::new(
            &genesis,
            irys_types::ConsensusConfig::testing(),
        ))));
        let parent_hash = genesis.block_hash;

        let range = find_block_range(
            expired,
            &config,
            &block_index,
            DataLedger::OneYear,
            18, // parent's OneYear total
            parent_hash,
            &guard,
            &db,
        )?
        .expect("expired OneYear slot 0 holds data");

        // OneYear slot 0 ([0,10)) spans height 3 (totals 0->5) and height 4 (5->12).
        assert_eq!(
            range.min_block.height, 3,
            "OneYear chunk 0 first appears at height 3"
        );
        assert_eq!(
            range.max_block.height, 4,
            "OneYear chunk 9 lands at height 4"
        );

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

    /// NC-0042 P0 (consensus-fork fix): the perm-fee refund suppression must read
    /// promotion from the block's OWN branch, never node-local mempool/DB state.
    /// This proves [`resolve_promoted_on_branch`] answers purely from
    /// `parent_block_hash` ancestry — the SAME txid resolves as promoted when
    /// walking the fork that promoted it and as un-promoted when walking the fork
    /// that did not, so two honest producers on competing branches compute
    /// matching (branch-determined) refund sets regardless of any local hint.
    #[test]
    fn resolve_promoted_on_branch_reads_only_the_blocks_own_branch() -> eyre::Result<()> {
        use irys_database::{IrysDatabaseArgs as _, open_or_create_db, tables::IrysTables};
        use irys_domain::{
            BlockTree, BlockTreeReadGuard, CommitmentSnapshot, EpochSnapshot, dummy_ema_snapshot,
        };
        use irys_testing_utils::IrysBlockHeaderTestExt as _;
        use irys_types::{BlockTransactions, SealedBlock};
        use reth_db::mdbx::DatabaseArguments;
        use std::sync::RwLock;

        // A header carrying `publish_tx_ids` in its Publish ledger (the promotion
        // signal `block_includes_publish_tx` reads) plus an empty Submit ledger.
        fn with_publish(height: u64, publish_tx_ids: Vec<H256>) -> IrysBlockHeader {
            let ledger = |id: DataLedger, tx_ids: Vec<H256>| irys_types::DataTransactionLedger {
                ledger_id: id as u32,
                tx_root: H256::zero(),
                tx_ids: irys_types::H256List(tx_ids),
                total_chunks: 0,
                expires: None,
                proofs: None,
                required_proof_count: None,
            };
            let mut h = IrysBlockHeader::new_mock_header();
            h.height = height;
            h.data_ledgers = vec![
                ledger(DataLedger::Publish, publish_tx_ids),
                ledger(DataLedger::Submit, vec![]),
            ];
            h
        }

        let target = H256::random();

        // genesis h0 → h1, then a fork at h2: `promoted` includes `target` in its
        // Publish ledger, `bare` does not. A settlement block at height 3 walks
        // back from whichever h2 is its parent.
        let mut genesis = with_publish(0, vec![]);
        let mut h1 = with_publish(1, vec![]);
        let mut h2_promoted = with_publish(2, vec![target]);
        let mut h2_bare = with_publish(2, vec![]);

        let guard = {
            genesis.cumulative_diff = 0.into();
            genesis.test_sign();
            let mut tree = BlockTree::new(&genesis, irys_types::ConsensusConfig::testing());
            tree.mark_tip(&genesis.block_hash).unwrap();

            h1.previous_block_hash = genesis.block_hash;
            h1.cumulative_diff = 1.into();
            h1.test_sign();

            h2_promoted.previous_block_hash = h1.block_hash;
            h2_promoted.cumulative_diff = 2.into();
            h2_promoted.test_sign();

            // Distinct cumulative_diff so the sibling is a genuine second child of
            // h1, not a duplicate. We never mark either h2 as tip — the walk is
            // driven by the explicit `parent_block_hash`, not fork choice.
            h2_bare.previous_block_hash = h1.block_hash;
            h2_bare.cumulative_diff = 3.into();
            h2_bare.test_sign();

            for h in [&h1, &h2_promoted, &h2_bare] {
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

        // Empty db ⇒ the finalized `canonical_promoted_height` fallback finds
        // nothing; the by-hash branch walk is the sole decider.
        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));
        let config = Config::new_with_random_peer_id(irys_types::NodeConfig::testing());

        // Walking the promoting fork → `target` IS promoted (refund suppressed).
        let on_promoted =
            resolve_promoted_on_branch(&[target], h2_promoted.block_hash, 3, &config, &guard, &db)?;
        assert!(
            on_promoted.contains(&target),
            "target IS promoted on the fork whose ancestry includes the Publish inclusion"
        );

        // Walking the bare fork → `target` is NOT promoted (refund emitted), even
        // though a sibling branch (and any node-local hint) promoted it.
        let on_bare =
            resolve_promoted_on_branch(&[target], h2_bare.block_hash, 3, &config, &guard, &db)?;
        assert!(
            on_bare.is_empty(),
            "target is NOT promoted on the fork whose ancestry never included it"
        );

        Ok(())
    }

    /// NC-0042 P0 — the exact trap, as a reintroduction guard. The original bug
    /// gated the refund on a **node-local** promotion signal. The most tempting
    /// wrong "fix" is to keep using the node-local `IrysDataTxMetadata`
    /// `promoted_height` hint via a by-height lookup — which is branch-invariant
    /// only BELOW the reorg floor. This pins that: with a stale `promoted_height`
    /// hint planted in the DB **inside the reorg window** (and `MigratedBlockHashes`
    /// agreeing, so a naive `canonical_promoted_height(parent_height)` WOULD report
    /// it promoted), `resolve_promoted_on_branch` must STILL report the tx as
    /// un-promoted because the block's own branch never carried it in a Publish
    /// ledger. Any regression to a fast-path-first by-height lookup, or a finalized
    /// fallback whose cap reaches into the reorg window, fails this test.
    #[test]
    fn resolve_promoted_on_branch_ignores_in_window_node_local_hint() -> eyre::Result<()> {
        use irys_database::{
            IrysDatabaseArgs as _, canonical_promoted_height, insert_block_header,
            insert_tx_header, open_or_create_db, set_data_tx_included_height,
            set_data_tx_promoted_height, tables::IrysTables, tables::MigratedBlockHashes,
        };
        use irys_domain::{BlockTree, BlockTreeReadGuard};
        use irys_testing_utils::IrysBlockHeaderTestExt as _;
        use irys_types::{
            BlockTransactions, DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata,
            DataTransactionLedger, DataTransactionMetadata, H256List, SealedBlock,
        };
        use reth_db::{mdbx::DatabaseArguments, transaction::DbTxMut as _};
        use std::sync::RwLock;

        // Submit-only headers ⇒ `target` is NEVER in a Publish ledger on this
        // branch, so the branch walk must conclude "not promoted".
        fn submit_only(height: u64) -> IrysBlockHeader {
            let mut h = IrysBlockHeader::new_mock_header();
            h.height = height;
            h.data_ledgers = vec![irys_types::DataTransactionLedger {
                ledger_id: DataLedger::Submit as u32,
                tx_root: H256::zero(),
                tx_ids: irys_types::H256List(vec![]),
                total_chunks: 0,
                expires: None,
                proofs: None,
                required_proof_count: None,
            }];
            h
        }

        let target = H256::random();

        // Linear chain genesis h0 → h1 → h2. A settlement block at height 3 walks
        // back from h2.
        let mut headers = [submit_only(0), submit_only(1), submit_only(2)];
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
                    Arc::new(irys_domain::CommitmentSnapshot::default()),
                    Arc::new(EpochSnapshot::default()),
                    irys_domain::dummy_ema_snapshot(),
                )?;
            }
            BlockTreeReadGuard::new(Arc::new(RwLock::new(tree)))
        };

        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));

        // Divergent sibling at height 1: `h1_alt` is a *second* child of h0 that
        // DOES carry `target` in its Publish ledger, and is the node-local
        // canonical block at height 1 (`MBH[1] = h1_alt`). h2's own branch goes
        // through the `submit_only` `headers[1]`, which never carried `target`.
        // This is the reorg-window divergence the resolver must survive: the
        // node-canonical view (MBH + content) says "promoted", but the block's
        // own by-hash ancestry says "not promoted". `h1_alt` is reachable only
        // via MBH (in the DB header table), never by the parent-pointer walk.
        let mut h1_alt = IrysBlockHeader::new_mock_header();
        h1_alt.height = 1;
        h1_alt.previous_block_hash = headers[0].block_hash;
        h1_alt.cumulative_diff = 100.into();
        h1_alt.data_ledgers = vec![
            DataTransactionLedger {
                ledger_id: DataLedger::Publish as u32,
                tx_root: H256::zero(),
                tx_ids: H256List(vec![target]),
                total_chunks: 0,
                expires: None,
                proofs: None,
                required_proof_count: None,
            },
            DataTransactionLedger {
                ledger_id: DataLedger::Submit as u32,
                tx_root: H256::zero(),
                tx_ids: H256List(vec![]),
                total_chunks: 0,
                expires: None,
                proofs: None,
                required_proof_count: None,
            },
        ];
        h1_alt.test_sign();

        let target_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: target,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        db.update_eyre(|wtx| {
            insert_tx_header(wtx, &target_header)?;
            // included_height is a precondition for promoted_height; the inclusion
            // sits below the reorg floor (slot-lifetime invariant) so its exact
            // value is immaterial here.
            set_data_tx_included_height(wtx, &target, 1)?;
            set_data_tx_promoted_height(wtx, &target, 1)?;
            // The node-local canonical block at height 1 is the divergent sibling,
            // which carries `target` in Publish — so the by-height content check
            // is satisfied and a naive lookup is fooled.
            insert_block_header(wtx, &h1_alt)?;
            wtx.put::<MigratedBlockHashes>(1, h1_alt.block_hash)?;
            Ok(())
        })?;

        let config = Config::new_with_random_peer_id(irys_types::NodeConfig::testing());

        // The trap is live: a naive by-height lookup at the parent height WOULD
        // report `target` promoted — MBH[1] = h1_alt, which genuinely carries
        // `target` in its Publish ledger, so even the content check passes for
        // the node-canonical (but off-branch) block.
        let naive = db.view_eyre(|tx| canonical_promoted_height(tx, &target, 2))?;
        assert_eq!(
            naive,
            Some(1),
            "precondition: the in-window node-canonical block fools a parent-height lookup"
        );

        // The branch-correct resolver ignores it: `target` is not in any Publish
        // ledger on h2's ancestry, and the finalized fallback is capped below the
        // walked reorg window.
        let promoted =
            resolve_promoted_on_branch(&[target], headers[2].block_hash, 3, &config, &guard, &db)?;
        assert!(
            promoted.is_empty(),
            "resolver must ignore the node-local in-window promoted_height hint"
        );

        Ok(())
    }

    /// NC-0042 P0 regression guard at the settlement layer: `aggregate_balance_deltas`
    /// bases refund suppression on the branch-resolved `promoted_on_branch` set,
    /// NOT on the fetched header's node-local `promoted_height`. A tx whose header
    /// still carries a (branch-variant) `promoted_height = Some` from local mempool
    /// state must STILL be refunded when it is absent from the branch set. Gating
    /// the refund on the header's node-local `promoted_height` instead of the
    /// branch-resolved set would wrongly suppress it — exactly what this guards.
    #[test]
    fn aggregate_refunds_use_branch_set_not_node_local_promoted_height() {
        let config = Config::new_with_random_peer_id(irys_types::NodeConfig::testing());

        let signer = IrysAddress::random();
        let mut tx = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: H256::random(),
                term_fee: U256::from(1000).into(),
                perm_fee: Some(irys_types::BoundedFee::from(777_u64)),
                signer,
                data_size: 100,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });
        // Stale, branch-variant node-local promotion state on the fetched header.
        tx.set_promoted_height(Some(5));

        let miner = IrysAddress::random();
        let mut tx_to_miners = BTreeMap::new();
        tx_to_miners.insert(tx.id, Arc::new(vec![miner]));

        let epoch_snapshot = EpochSnapshot::default();

        // Branch-resolved set is EMPTY ⇒ tx was NOT promoted on this branch ⇒ refund,
        // regardless of the header's `promoted_height = Some(5)`.
        let result = aggregate_balance_deltas(
            vec![tx.clone()],
            &tx_to_miners,
            &epoch_snapshot,
            &config,
            true, // expect_txs_to_be_promoted (Submit ledger)
            &BTreeSet::new(),
        )
        .unwrap();

        assert_eq!(
            result.user_perm_fee_refunds,
            vec![(tx.id, irys_types::BoundedFee::from(777_u64).get(), signer)],
            "refund must follow the branch set despite header promoted_height = Some"
        );
    }

    /// Canonical P0 repro turned regression: normal epoch-slot allocation
    /// preallocates future Submit slots, but those unwritten slots must never
    /// expire. Only slot 0 should expire here; slot 1 remains live because it
    /// was refreshed by a later write, and slots 2/3 remain live because they
    /// never held canonical data at all.
    #[tokio::test]
    async fn submit_expiry_skips_canonical_unwritten_slots() -> eyre::Result<()> {
        use irys_database::{
            IrysDatabaseArgs as _, insert_block_header, insert_tx_header, open_or_create_db,
            set_data_tx_included_height, tables::IrysTables,
        };
        use irys_domain::{BlockIndex, BlockTree, BlockTreeReadGuard};
        use irys_testing_utils::{IrysBlockHeaderTestExt as _, utils::TempDirBuilder};
        use irys_types::{
            ConsensusConfig, DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata,
            DataTransactionMetadata, H256List, LedgerIndexItem, app_state::DatabaseProvider,
        };
        use reth_db::{mdbx::DatabaseArguments, transaction::DbTxMut as _};
        use std::sync::{Arc, RwLock};

        fn submit_chain_header(
            height: u64,
            total_chunks: u64,
            tx_ids: Vec<H256>,
        ) -> IrysBlockHeader {
            let mut h = IrysBlockHeader::new_mock_header();
            h.height = height;
            h.data_ledgers[DataLedger::Submit].total_chunks = total_chunks;
            h.data_ledgers[DataLedger::Submit].tx_ids = H256List(tx_ids);
            h
        }

        let config = config_with_cascade();

        let num_blocks_in_epoch = config.consensus.epoch.num_blocks_in_epoch;
        let blocks_per_cycle =
            config.consensus.epoch.submit_ledger_epoch_length * num_blocks_in_epoch;
        let epoch1_height = num_blocks_in_epoch;
        let epoch2_height = 2 * num_blocks_in_epoch;
        let expiry_epoch_height = blocks_per_cycle + num_blocks_in_epoch;
        let probe_height = expiry_epoch_height + 1;

        // Canonical epoch evolution:
        //   h=0      : genesis allocates slot 0
        //   h=N      : total_chunks jumps to 18, so slots 0/1 receive data and
        //              several future slots are preallocated but untouched
        //   h=2N     : total_chunks advances only to 19, refreshing slot 1 alone
        //   h=3N..6N : no new data, so the touched/stale heights persist
        // At h=6N+1, slot 0 has expired, slot 1 is still live, and untouched
        // preallocated slots 2/3 must remain unexpired.
        let mut epoch = empty_epoch_snapshot(&config);
        let mut epoch0 = submit_epoch_header(0, 0);
        epoch0.test_sign();
        epoch.perform_epoch_tasks(&None, &epoch0, vec![])?;
        let mut epoch1 = submit_epoch_header(epoch1_height, 18);
        epoch1.test_sign();
        epoch.perform_epoch_tasks(&Some(epoch0.clone()), &epoch1, vec![])?;
        let mut epoch2 = submit_epoch_header(epoch2_height, 19);
        epoch2.test_sign();
        epoch.perform_epoch_tasks(&Some(epoch1.clone()), &epoch2, vec![])?;
        let mut prev_epoch = epoch2.clone();
        for height in
            (3 * num_blocks_in_epoch..=expiry_epoch_height).step_by(num_blocks_in_epoch as usize)
        {
            let mut next_epoch = submit_epoch_header(height, 19);
            next_epoch.test_sign();
            epoch.perform_epoch_tasks(&Some(prev_epoch.clone()), &next_epoch, vec![])?;
            prev_epoch = next_epoch;
        }

        assert_eq!(
            epoch.get_all_expired_term_slot_indexes(DataLedger::Submit, probe_height, true, 19),
            vec![0],
            "canonical epoch processing must skip unwritten preallocated Submit slots"
        );

        let tx_slot_0 = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: H256::random(),
                data_size: 10, // chunks [0,10) -> slot 0
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        let tx_slot_1 = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: H256::random(),
                data_size: 9, // chunks [10,19) -> slot 1
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });

        let mut inclusion_genesis = submit_chain_header(0, 0, vec![]);
        inclusion_genesis.cumulative_diff = 0.into();
        inclusion_genesis.test_sign();
        let mut inclusion_block = submit_chain_header(1, 19, vec![tx_slot_0.id, tx_slot_1.id]);
        inclusion_block.previous_block_hash = inclusion_genesis.block_hash;
        inclusion_block.cumulative_diff = 1.into();
        inclusion_block.test_sign();

        let temp_dir = TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));
        db.update_eyre(|wtx| {
            insert_tx_header(wtx, &tx_slot_0)?;
            insert_tx_header(wtx, &tx_slot_1)?;
            set_data_tx_included_height(wtx, &tx_slot_0.id, 1)?;
            set_data_tx_included_height(wtx, &tx_slot_1.id, 1)?;
            insert_block_header(wtx, &inclusion_genesis)?;
            insert_block_header(wtx, &inclusion_block)?;
            wtx.put::<irys_database::tables::MigratedBlockHashes>(0, inclusion_genesis.block_hash)?;
            wtx.put::<irys_database::tables::MigratedBlockHashes>(1, inclusion_block.block_hash)?;
            Ok(())
        })?;

        let block_index = BlockIndex::new_for_testing(db.clone());
        for (height, (hash, submit_total)) in [
            (inclusion_genesis.block_hash, 0_u64),
            (inclusion_block.block_hash, 19_u64),
        ]
        .into_iter()
        .enumerate()
        {
            block_index.push_item(
                &BlockIndexItem {
                    block_hash: hash,
                    num_ledgers: 2,
                    ledgers: vec![
                        LedgerIndexItem {
                            total_chunks: 0,
                            tx_root: H256::random(),
                            ledger: DataLedger::Publish,
                        },
                        LedgerIndexItem {
                            total_chunks: submit_total,
                            tx_root: H256::random(),
                            ledger: DataLedger::Submit,
                        },
                    ],
                },
                height as u64,
            )?;
        }

        let tree = BlockTree::new(&inclusion_genesis, ConsensusConfig::testing());
        let guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(tree)));

        let range = expired_submit_range(probe_height, &epoch, &prev_epoch, &config, true)?
            .expect("expired Submit slots must produce a range");
        assert!(
            submit_tx_expired(
                tx_slot_0.id,
                &range,
                &config,
                &block_index,
                &guard,
                &MempoolReadGuard::stub(),
                &db,
            )
            .await?,
            "slot-0 tx must be expired"
        );
        assert!(
            !submit_tx_expired(
                tx_slot_1.id,
                &range,
                &config,
                &block_index,
                &guard,
                &MempoolReadGuard::stub(),
                &db,
            )
            .await?,
            "slot-1 tx must stay live even though later empty slots also expired"
        );

        // Walk-oracle parity + the new `cascade_active` parameter: the (now
        // parameterized) `expired_submit_tx_ids` walk must return EXACTLY the
        // per-candidate–expired set — only the slot-0 tx — when passed the block's
        // own Cascade status. Previously the oracle derived Cascade from the parent
        // epoch snapshot's timestamp, which lags the block and was wrong at the
        // activation boundary; full mid-chain activation is covered end-to-end by
        // the `cascade_term_expiry` chain-tests.
        let oracle = expired_submit_tx_ids(
            &epoch,
            &prev_epoch,
            probe_height,
            &config,
            true, // the block-under-test's own Cascade status (activation at genesis here)
            block_index.clone(),
            &guard,
            &MempoolReadGuard::stub(),
            &db,
        )
        .await?;
        assert_eq!(
            oracle,
            std::collections::BTreeSet::from([tx_slot_0.id]),
            "walk oracle must equal the per-candidate verdicts (slot-0 expired, slot-1 live)"
        );

        Ok(())
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

    /// P2-5: `filter_transactions_by_chunk_range` must bail when `chunk_size == 0`
    /// rather than panicking. Mirrors the existing `chunk_size == 0` guard in
    /// `submit_tx_start_offset` and the `num_chunks_in_partition == 0` guard added
    /// alongside it.
    #[test]
    fn filter_transactions_by_chunk_range_bails_on_zero_chunk_size() {
        let tx = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: H256::random(),
                data_size: 100,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });
        let mut slot_miners: BTreeMap<SlotIndex, Arc<Vec<IrysAddress>>> = BTreeMap::new();
        slot_miners.insert(SlotIndex::new(0), Arc::new(vec![IrysAddress::ZERO]));
        let err = filter_transactions_by_chunk_range(
            vec![tx],
            LedgerChunkOffset::from(0_u64),
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 100)),
            false, // is_earliest
            false, // is_latest
            0,     // chunk_size = 0 → must bail
            10,    // num_chunks_in_partition
            &slot_miners,
        )
        .expect_err("chunk_size == 0 must fail loud");
        assert!(
            err.to_string().contains("chunk_size"),
            "expected the chunk_size guard message, got: {err}"
        );
    }

    /// P1-3: `expired_submit_range` must bail when the expired Submit slot set is a
    /// non-contiguous prefix (e.g. `{0, 2}` with slot 1 unwritten/excluded). The
    /// contiguity invariant underpins the `[0, range_end)` promotion filter; a hole
    /// would cause the filter to over-approximate real expired txs (NC-0042).
    #[test]
    fn expired_submit_range_bails_on_non_prefix_expired_set() {
        // Default config: num_blocks_in_epoch=100, submit_ledger_epoch_length=5,
        // num_chunks_in_partition=10. min_blocks = 5 * 100 = 500.
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);

        let mut epoch = empty_epoch_snapshot(&config);

        // Allocate 4 Submit slots at height 1 (last_height=1 for all).
        // Slot 3 is the never-expiring last slot.
        epoch.ledgers[DataLedger::Submit].allocate_slots(4, 1);

        // Touch slots 0 and 2 only (chunks [0,10) and [20,30)); slot 1 stays
        // unwritten. With cascade_active=true, unwritten slots are excluded from
        // expiry, so slot 1 is never returned as expired.
        let chunks_per_slot = config.consensus.num_chunks_in_partition;
        epoch.ledgers.touch_filled_slots(
            DataLedger::Submit,
            0,
            chunks_per_slot,
            chunks_per_slot,
            1,
            true,
        );
        epoch.ledgers.touch_filled_slots(
            DataLedger::Submit,
            2 * chunks_per_slot,
            3 * chunks_per_slot,
            chunks_per_slot,
            1,
            true,
        );

        // block_height = min_blocks + 50 → expiry_height = 50. Slots 0 and 2 have
        // last_height=1 ≤ 50, so they expire. Slot 1 is unwritten (excluded by
        // cascade). Slot 3 is the last slot (never expires). Result: {0, 2} — a hole.
        let ep = &config.consensus.epoch;
        let min_blocks = ep.submit_ledger_epoch_length * ep.num_blocks_in_epoch;
        let block_height = min_blocks + 50;

        // Sanity: fixture produces the non-prefix set before asserting the guard.
        assert_eq!(
            epoch.get_all_expired_term_slot_indexes(
                DataLedger::Submit,
                block_height,
                true,
                3 * chunks_per_slot
            ),
            vec![0, 2],
            "fixture must produce the non-prefix expired set {{0, 2}}"
        );

        // Parent header just needs a Submit total that covers at least slot 2's data.
        let mut parent = IrysBlockHeader::new_mock_header();
        parent.data_ledgers[DataLedger::Submit].total_chunks = 3 * chunks_per_slot;

        let err = expired_submit_range(block_height, &epoch, &parent, &config, true)
            .expect_err("a non-prefix expired set must fail loud, not over-approximate");
        assert!(
            err.to_string().contains("not a contiguous prefix"),
            "expected the NC-0042 contiguity guard message, got: {err}"
        );
    }

    /// The slot holding the write frontier must survive an idle Submit term.
    /// Chunk offsets are strictly cumulative, so if the partially-written
    /// frontier slot expired, every tx appended into its unwritten remainder
    /// afterwards would be charged yet permanently non-promotable (its start
    /// offset sits inside `[0, range_end)` forever) and never refunded (the
    /// refund pipeline settles each slot exactly once, at expiry). Post-Cascade
    /// only fully-written slots may expire, so the expired range can never
    /// reach the frontier. Unlike `submit_expiry_skips_canonical_unwritten_slots`
    /// (where the frontier slot is rescued by a later write refreshing its
    /// clock), here ingress stalls for a full term and the fully-written rule is
    /// the only thing keeping the frontier slot alive.
    #[test]
    fn submit_expiry_never_covers_the_partially_written_frontier_slot() -> eyre::Result<()> {
        use irys_testing_utils::IrysBlockHeaderTestExt as _;

        let config = config_with_cascade();

        let num_blocks_in_epoch = config.consensus.epoch.num_blocks_in_epoch;
        let blocks_per_cycle =
            config.consensus.epoch.submit_ledger_epoch_length * num_blocks_in_epoch;

        let mut epoch = empty_epoch_snapshot(&config);

        // h=0: genesis. h=N: data [0, 18) lands — slot 0 full, slot 1 holds the
        // write frontier (8/10), headroom slots preallocated above. Then ingress
        // stalls: idle epochs at total 18 until a full term has passed since the
        // last write.
        let mut epoch0 = submit_epoch_header(0, 0);
        epoch0.test_sign();
        epoch.perform_epoch_tasks(&None, &epoch0, vec![])?;
        let mut prev_epoch = epoch0;
        for height in (num_blocks_in_epoch..=blocks_per_cycle + num_blocks_in_epoch)
            .step_by(num_blocks_in_epoch as usize)
        {
            let mut next_epoch = submit_epoch_header(height, 18);
            next_epoch.test_sign();
            epoch.perform_epoch_tasks(&Some(prev_epoch.clone()), &next_epoch, vec![])?;
            prev_epoch = next_epoch;
        }

        // Both written slots are a full term stale, but only the FULL slot 0 may
        // expire; the frontier slot 1 must stay live.
        let probe_height = blocks_per_cycle + num_blocks_in_epoch + 1;
        assert_eq!(
            epoch.get_all_expired_term_slot_indexes(DataLedger::Submit, probe_height, true, 18),
            vec![0],
            "the write frontier's slot must not expire during an ingress stall"
        );
        let slots = epoch.ledgers.get_slots(DataLedger::Submit);
        assert!(slots[0].is_expired, "the fully-written slot 0 recycles");
        assert!(!slots[1].is_expired, "the frontier slot 1 must survive");

        // The non-promotability range must stop at slot 0's boundary: the next
        // append (offset 18) lies outside it and stays promotable/settleable.
        let mut parent = IrysBlockHeader::new_mock_header();
        parent.data_ledgers[DataLedger::Submit].total_chunks = 18;
        let range = expired_submit_range(probe_height, &epoch, &parent, &config, true)?
            .expect("slot 0 expired, so a range must exist");
        assert_eq!(
            range.range_end, 10,
            "expired range must end at the last full slot's boundary, not the frontier"
        );

        // Ingress resumes: [18, 25) fills slot 1. Because it never expired, the
        // touch refreshes its expiry clock instead of freezing it.
        let resume_height = blocks_per_cycle + 2 * num_blocks_in_epoch;
        let mut resume_epoch = submit_epoch_header(resume_height, 25);
        resume_epoch.test_sign();
        epoch.perform_epoch_tasks(&Some(prev_epoch), &resume_epoch, vec![])?;
        let slots = epoch.ledgers.get_slots(DataLedger::Submit);
        assert_eq!(
            slots[1].last_height, resume_height,
            "surviving the stall lets the refill refresh the frontier slot's clock"
        );
        assert_eq!(
            epoch.get_all_expired_term_slot_indexes(
                DataLedger::Submit,
                resume_height + 1,
                true,
                25
            ),
            vec![0],
            "the refilled slot stays out of the expired set for a fresh term"
        );

        Ok(())
    }

    /// An expired Submit slot with an EMPTY `partitions` vec — its sole replica
    /// unpledged before expiry and never backfilled — must still settle: its
    /// unpromoted txs are perm-fee refunded even though there are no miners to
    /// pay term fees to (those stay in the treasury). Settlement must be
    /// slot-keyed, not partition-keyed: the slot is marked `is_expired` and its
    /// txs are promotion-blocked forever (`get_all_expired_slot_indexes`), so a
    /// partition-keyed settlement walk that never sees the slot would leave its
    /// users charged, non-promotable, and unsettled.
    #[test_log::test(tokio::test)]
    async fn empty_partition_expired_slot_still_refunds_unpromoted_txs() -> eyre::Result<()> {
        use irys_database::{
            IrysDatabaseArgs as _, insert_block_header, insert_tx_header, open_or_create_db,
            tables::IrysTables,
        };
        use irys_domain::{BlockIndex, BlockTree, BlockTreeReadGuard};
        use irys_testing_utils::{IrysBlockHeaderTestExt as _, utils::TempDirBuilder};
        use irys_types::{
            ConsensusConfig, DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata,
            DataTransactionMetadata, H256List, LedgerIndexItem, app_state::DatabaseProvider,
        };
        use reth_db::{mdbx::DatabaseArguments, transaction::DbTxMut as _};
        use std::sync::{Arc, RwLock};

        fn submit_chain_header(
            height: u64,
            total_chunks: u64,
            tx_ids: Vec<H256>,
        ) -> IrysBlockHeader {
            let mut h = IrysBlockHeader::new_mock_header();
            h.height = height;
            h.data_ledgers[DataLedger::Submit].total_chunks = total_chunks;
            h.data_ledgers[DataLedger::Submit].tx_ids = H256List(tx_ids);
            h
        }

        let config = config_with_cascade();
        let num_blocks_in_epoch = config.consensus.epoch.num_blocks_in_epoch;
        let blocks_per_cycle =
            config.consensus.epoch.submit_ledger_epoch_length * num_blocks_in_epoch;
        // Slot 0 is written (and its expiry clock stamped) at the first epoch;
        // it expires a full slot lifetime later.
        let write_epoch_height = num_blocks_in_epoch;
        let expiry_height = write_epoch_height + blocks_per_cycle;
        let submit_total = config.consensus.num_chunks_in_partition; // slot 0 exactly full

        // --- Epoch snapshot: slot 0 fully written at the first epoch, then aged
        //     through the cycle WITHOUT ever holding a partition assignment (no
        //     pledges in this fixture — the state an unpledged sole replica
        //     leaves behind). Evolution stops at the last epoch BEFORE expiry:
        //     the parent snapshot of the expiry epoch block. ---
        let mut epoch = empty_epoch_snapshot(&config);
        let mut prev_epoch = submit_epoch_header(0, 0);
        prev_epoch.test_sign();
        epoch.perform_epoch_tasks(&None, &prev_epoch, vec![])?;
        for height in (num_blocks_in_epoch..expiry_height).step_by(num_blocks_in_epoch as usize) {
            let mut next = submit_epoch_header(height, submit_total);
            next.test_sign();
            epoch.perform_epoch_tasks(&Some(prev_epoch.clone()), &next, vec![])?;
            prev_epoch = next;
        }

        {
            let slots = epoch.ledgers.get_slots(DataLedger::Submit);
            assert!(
                slots.len() >= 2,
                "fixture: slot 0 must be non-last (got {} slots)",
                slots.len()
            );
            let slot0 = &slots[0];
            assert!(slot0.has_been_written && !slot0.is_expired);
            assert!(
                slot0.partitions.is_empty(),
                "fixture: slot 0 must have no partition assignment"
            );
        }
        // The slot IS in the non-promotability set at the expiry epoch (its txs
        // are blocked) ...
        assert_eq!(
            epoch.get_all_expired_term_slot_indexes(
                DataLedger::Submit,
                expiry_height,
                true,
                submit_total
            ),
            vec![0],
            "the minerless slot must be in the blocked set at its expiry epoch"
        );
        // ... while the partition-keyed view is blind to it, so settlement must
        // not be derived from partition infos.
        assert!(
            epoch
                .get_expiring_partition_info(expiry_height, DataLedger::Submit, submit_total, true)
                .is_empty(),
            "partition-keyed expiring set has no entry for a partition-less slot"
        );

        // --- Two unpromoted Submit txs exactly fill slot 0 (chunk_size = 1). ---
        let mk_tx = |data_size: u64, perm_fee: u64| {
            DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
                tx: DataTransactionHeaderV1 {
                    id: H256::random(),
                    signer: IrysAddress::random(),
                    data_size,
                    term_fee: U256::from(1000).into(),
                    perm_fee: Some(U256::from(perm_fee).into()),
                    ..Default::default()
                },
                metadata: DataTransactionMetadata::new(),
            })
        };
        let tx_a = mk_tx(6, 7001); // chunks [0,6)
        let tx_b = mk_tx(4, 7002); // chunks [6,10)

        let mut inclusion_genesis = submit_chain_header(0, 0, vec![]);
        inclusion_genesis.cumulative_diff = 0.into();
        inclusion_genesis.test_sign();
        let mut inclusion_block = submit_chain_header(1, submit_total, vec![tx_a.id, tx_b.id]);
        inclusion_block.previous_block_hash = inclusion_genesis.block_hash;
        inclusion_block.cumulative_diff = 1.into();
        inclusion_block.test_sign();

        let temp_dir = TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));
        db.update_eyre(|wtx| {
            insert_tx_header(wtx, &tx_a)?;
            insert_tx_header(wtx, &tx_b)?;
            insert_block_header(wtx, &inclusion_genesis)?;
            insert_block_header(wtx, &inclusion_block)?;
            wtx.put::<irys_database::tables::MigratedBlockHashes>(0, inclusion_genesis.block_hash)?;
            wtx.put::<irys_database::tables::MigratedBlockHashes>(1, inclusion_block.block_hash)?;
            Ok(())
        })?;

        let block_index = BlockIndex::new_for_testing(db.clone());
        for (height, (hash, total)) in [
            (inclusion_genesis.block_hash, 0_u64),
            (inclusion_block.block_hash, submit_total),
        ]
        .into_iter()
        .enumerate()
        {
            block_index.push_item(
                &BlockIndexItem {
                    block_hash: hash,
                    num_ledgers: 2,
                    ledgers: vec![
                        LedgerIndexItem {
                            total_chunks: 0,
                            tx_root: H256::random(),
                            ledger: DataLedger::Publish,
                        },
                        LedgerIndexItem {
                            total_chunks: total,
                            tx_root: H256::random(),
                            ledger: DataLedger::Submit,
                        },
                    ],
                },
                height as u64,
            )?;
        }

        // Parent-ancestry chain for the promotion-suppression walk
        // (`resolve_promoted_on_branch` descends by hash from the parent to the
        // walk floor; none of these blocks promotes anything).
        let walk_floor = expiry_height
            .saturating_sub(crate::block_validation::prior_inclusion_walk_depth(&config))
            .saturating_sub(2);
        let mut walk_prev: Option<H256> = None;
        let mut parent_header: Option<IrysBlockHeader> = None;
        for height in walk_floor..expiry_height {
            let mut hdr = submit_chain_header(height, submit_total, vec![]);
            if let Some(prev_hash) = walk_prev {
                hdr.previous_block_hash = prev_hash;
            }
            hdr.cumulative_diff = height.into();
            hdr.test_sign();
            db.update_eyre(|wtx| {
                insert_block_header(wtx, &hdr)?;
                Ok(())
            })?;
            walk_prev = Some(hdr.block_hash);
            parent_header = Some(hdr);
        }
        let parent_header = parent_header.expect("walk chain is non-empty");

        let tree = BlockTree::new(&inclusion_genesis, ConsensusConfig::testing());
        let guard = BlockTreeReadGuard::new(Arc::new(RwLock::new(tree)));

        let delta = calculate_expired_ledger_fees(
            &epoch,
            &parent_header,
            expiry_height,
            DataLedger::Submit,
            &config,
            block_index,
            &guard,
            &MempoolReadGuard::stub(),
            &db,
            true, // Submit: refund the perm fee for unpromoted txs
            submit_total,
            true,
        )
        .await?;

        let refunds: std::collections::BTreeMap<_, _> = delta
            .user_perm_fee_refunds
            .iter()
            .map(|(id, amount, addr)| (*id, (*amount, *addr)))
            .collect();
        assert_eq!(
            refunds.len(),
            2,
            "both unpromoted txs in the minerless expired slot must be refunded, got {:?}",
            delta.user_perm_fee_refunds
        );
        for tx in [&tx_a, &tx_b] {
            assert_eq!(
                refunds.get(&tx.id).copied(),
                Some((tx.perm_fee.expect("fixture sets perm_fee").get(), tx.signer)),
                "tx {} must be refunded exactly its perm_fee to its signer",
                tx.id
            );
        }
        assert!(
            delta.reward_balance_increment.is_empty(),
            "no miners stored the slot, so no term-fee rewards may be distributed"
        );

        Ok(())
    }

    /// An expired Submit slot with ZERO written chunks (pre-Cascade
    /// allocation-anchored recycle of an unwritten slot) yields `range_end == 0`:
    /// `expired_submit_range` must return `None` — nothing was ever written into
    /// the expired range, so there is nothing to block or walk — rather than
    /// erroring or fabricating a range.
    #[test]
    fn expired_submit_range_none_when_expired_slots_hold_no_chunks() -> eyre::Result<()> {
        // No Cascade config: the pre-Cascade gate is transparent, so an
        // unwritten, aged, non-last slot recycles by allocation age alone.
        let node_config = NodeConfig::testing();
        let config = Config::new_with_random_peer_id(node_config);
        let mut epoch = empty_epoch_snapshot(&config);
        epoch.ledgers[DataLedger::Submit].allocate_slots(2, 1);

        let ep = &config.consensus.epoch;
        let min_blocks = ep.submit_ledger_epoch_length * ep.num_blocks_in_epoch;
        let expiry_epoch = min_blocks + 50;

        // Recycle slot 0 (unwritten, partition-less): marks `is_expired`; with
        // no partitions there are no expiring-partition infos to report.
        let infos = epoch.ledgers.expire_partitions(
            expiry_epoch,
            false,
            |_| 0,
            config.consensus.num_chunks_in_partition,
        );
        assert!(
            infos.is_empty(),
            "a partition-less slot recycles without partition infos"
        );
        assert!(
            epoch.ledgers.get_slots(DataLedger::Submit)[0].is_expired,
            "slot 0 must have recycled"
        );
        // The inclusive (non-promotability) set still contains the slot...
        assert_eq!(
            epoch.get_all_expired_term_slot_indexes(DataLedger::Submit, expiry_epoch + 1, false, 0),
            vec![0],
            "the recycled slot stays in the inclusive set"
        );

        // ...but with zero chunks written as of the parent, there is no expired
        // chunk range at all.
        let mut parent = IrysBlockHeader::new_mock_header();
        parent.data_ledgers[DataLedger::Submit].total_chunks = 0;
        let range = expired_submit_range(expiry_epoch + 1, &epoch, &parent, &config, false)?;
        assert!(
            range.is_none(),
            "an expired-but-never-written slot yields no expired chunk range"
        );
        Ok(())
    }

    /// A candidate promoted BELOW the by-hash walk window must be resolved as
    /// promoted through the finalized `canonical_promoted_height` fallback —
    /// refund suppressed — not silently missed. The by-hash walk covers only
    /// the reorg window, and a Submit slot can expire long after its txs were
    /// promoted (the promotion then sits below the walk floor, where MBH is
    /// branch-invariant).
    #[test]
    fn resolve_promoted_below_walk_window_uses_finalized_fallback() -> eyre::Result<()> {
        use irys_database::{
            IrysDatabaseArgs as _, insert_block_header, insert_tx_header, open_or_create_db,
            tables::IrysTables,
        };
        use irys_domain::{
            BlockTree, BlockTreeReadGuard, CommitmentSnapshot, EpochSnapshot, dummy_ema_snapshot,
        };
        use irys_testing_utils::IrysBlockHeaderTestExt as _;
        use irys_types::{
            BlockTransactions, DataTransactionHeaderV1, DataTransactionHeaderV1WithMetadata,
            DataTransactionMetadata, SealedBlock,
        };
        use reth_db::{mdbx::DatabaseArguments, transaction::DbTxMut as _};
        use std::sync::RwLock;

        fn with_publish(height: u64, publish_tx_ids: Vec<H256>) -> IrysBlockHeader {
            let ledger = |id: DataLedger, tx_ids: Vec<H256>| irys_types::DataTransactionLedger {
                ledger_id: id as u32,
                tx_root: H256::zero(),
                tx_ids: irys_types::H256List(tx_ids),
                total_chunks: 0,
                expires: None,
                proofs: None,
                required_proof_count: None,
            };
            let mut h = IrysBlockHeader::new_mock_header();
            h.height = height;
            h.data_ledgers = vec![
                ledger(DataLedger::Publish, publish_tx_ids),
                ledger(DataLedger::Submit, vec![]),
            ];
            h
        }

        // Small walk window so the promotion height sits strictly below it:
        // walk_depth = max(tx_anchor_expiry_depth, block_tree_depth) = 5, and
        // the settlement block at height 20 walks only [15, 19].
        let mut node_config = NodeConfig::testing();
        {
            let c = node_config.consensus.get_mut();
            c.block_tree_depth = 5;
            c.mempool.tx_anchor_expiry_depth = 5;
        }
        let config = Config::new_with_random_peer_id(node_config);
        let block_height = 20_u64;
        let promote_height = 2_u64;

        let target = H256::random();

        // Chain 0..=19: the block at height 2 promotes `target`; every block
        // inside the walk window [15, 19] does not.
        let mut headers: Vec<IrysBlockHeader> = (0..block_height)
            .map(|h| {
                with_publish(
                    h,
                    if h == promote_height {
                        vec![target]
                    } else {
                        vec![]
                    },
                )
            })
            .collect();
        let guard = {
            let mut iter = headers.iter_mut();
            let genesis = iter.next().expect("chain has a genesis");
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

        // Finalized promotion record: tx metadata + the canonical (MBH-agreed)
        // promoting block whose Publish ledger carries the tx — everything
        // `canonical_promoted_height`'s content check requires.
        let tx_header = DataTransactionHeader::V1(DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                id: target,
                ..Default::default()
            },
            metadata: DataTransactionMetadata::new(),
        });
        let temp_dir = irys_testing_utils::utils::TempDirBuilder::new().build();
        let db_env = open_or_create_db(
            &temp_dir,
            IrysTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let db = DatabaseProvider(Arc::new(db_env));
        db.update_eyre(|wtx| {
            insert_tx_header(wtx, &tx_header)?;
            // promoted_height requires the Submit inclusion height to exist first.
            irys_database::db_index::set_data_tx_included_height(wtx, &target, 1)?;
            irys_database::db_index::set_data_tx_promoted_height(wtx, &target, promote_height)?;
            insert_block_header(wtx, &headers[promote_height as usize])?;
            wtx.put::<irys_database::tables::MigratedBlockHashes>(
                promote_height,
                headers[promote_height as usize].block_hash,
            )?;
            Ok(())
        })?;

        let parent_hash = headers[(block_height - 1) as usize].block_hash;
        let promoted =
            resolve_promoted_on_branch(&[target], parent_hash, block_height, &config, &guard, &db)?;
        assert!(
            promoted.contains(&target),
            "a promotion below the walk floor must be found via the finalized fallback \
             (refund suppressed)"
        );

        // Control: a tx with no promotion record anywhere stays unpromoted.
        let unknown = H256::random();
        let not_promoted = resolve_promoted_on_branch(
            &[unknown],
            parent_hash,
            block_height,
            &config,
            &guard,
            &db,
        )?;
        assert!(
            not_promoted.is_empty(),
            "a tx never promoted on the branch or below it must not be suppressed"
        );

        Ok(())
    }
}
