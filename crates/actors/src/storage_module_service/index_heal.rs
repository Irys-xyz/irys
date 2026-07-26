//! Path-hash index heal for ledger-assigned storage modules.
//!
//! **Repair driver:** path-hash gap scan finds which placement blocks to
//! re-migrate via `UpdateStorageModuleIndexes`.
//!
//! **Completion** uses the same readiness predicate as data_sync re-arm —
//! [`StorageModule::is_data_root_index_ready_at`] — not path-hash density alone.
//! Dense path-hashes with missing `DataRootInfos` are still incomplete.
//!
//! When path-hash is already dense, residual detection samples first/mid/last
//! only (best-effort). Offsets that stay unready stay `Blocked` until re-arm
//! probes them; heal will not re-migrate residual that never hits those samples.
//!
//! Data-sync liveness is **not** driven from here: orchestrators re-queue
//! `Blocked` offsets by probing that same readiness predicate on their tick.

use crate::{
    DataSyncServiceMessage, chunk_migration_service::ChunkMigrationServiceMessage,
    services::ServiceSenders,
};
use eyre::eyre;
use futures::FutureExt as _;
use irys_domain::{BlockBoundsError, BlockIndexReadGuard, BlockTreeReadGuard, StorageModule};
use irys_types::{
    BlockHash, Config, DataLedger, LedgerChunkOffset, PartitionChunkOffset, SendTraced as _,
};
use reth::tasks::shutdown::Shutdown;
use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tracing::{debug, error, warn};

/// Max blocks re-indexed in one heal pass. Remaining holes retry when
/// [`HealOutcome::needs_retry`] is true (periodic / assignment / restart).
pub(super) const INDEX_HEAL_MAX_BLOCKS_PER_PASS: usize = 128;

/// Per-block wait for `UpdateStorageModuleIndexes`.
pub(super) const INDEX_HEAL_MIGRATE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Monotonic budget for one migrate pass. Stops 128×30s sequential waits from
/// monopolizing the storage-module run loop; remaining blocks retry next pass.
pub(super) const INDEX_HEAL_MIGRATE_PASS_BUDGET: Duration = Duration::from_secs(60);

/// Steady-state heal poll when the last pass left unrepaired work.
pub(super) const INDEX_HEAL_RETRY_INTERVAL: Duration = Duration::from_secs(60);

/// Result of one heal pass across all ledger-assigned SMs.
#[derive(Debug, Clone, Copy)]
pub(super) struct HealOutcome {
    /// True when any SM still has path-hash gaps, readiness failures, soft-skips,
    /// or deferred migrate work — schedule another pass without waiting for epoch.
    pub needs_retry: bool,
}

/// Dependencies for index heal (shared with the parent service).
pub(super) struct IndexHealCtx<'a> {
    pub storage_modules: &'a Arc<RwLock<Vec<Arc<StorageModule>>>>,
    pub block_index: &'a BlockIndexReadGuard,
    pub block_tree: &'a BlockTreeReadGuard,
    pub service_senders: &'a ServiceSenders,
    pub config: &'a Config,
}

/// Plan / migrate / re-check indexes for every ledger-assigned SM.
///
/// Always refreshes data_sync membership via `SyncPartitions`. Does **not**
/// push unblock signals — data_sync re-queues `Blocked` offsets via
/// [`StorageModule::is_data_root_index_ready_at`] on its own tick.
#[tracing::instrument(level = "debug", skip_all, err)]
pub(super) async fn heal_ledger_data_indexes(
    ctx: &IndexHealCtx<'_>,
    cancel: &Shutdown,
) -> eyre::Result<HealOutcome> {
    let ledger_modules: Vec<Arc<StorageModule>> = {
        let guard = ctx.storage_modules.read().unwrap();
        guard
            .iter()
            .filter(|sm| {
                sm.partition_assignment()
                    .and_then(|a| a.ledger_id)
                    .is_some()
            })
            .cloned()
            .collect()
    };

    debug!(
        storage_module.ledger_assigned_count = ledger_modules.len(),
        "healing data indexes for ledger-assigned storage modules"
    );

    let mut needs_retry = false;
    // Unique placement blocks (height → hash) across all SMs.
    let mut blocks_to_migrate: BTreeMap<u64, BlockHash> = BTreeMap::new();
    // SMs that needed repair — re-verify readiness after migrate.
    let mut recheck_after_migrate: Vec<RecheckTarget> = Vec::new();

    for sm in &ledger_modules {
        match plan_index_repair(ctx, sm) {
            IndexRepairPlan::Complete => {}
            IndexRepairPlan::SoftSkipped => {
                crate::metrics::record_index_heal_unrepaired("soft_skip");
                needs_retry = true;
            }
            IndexRepairPlan::NeedsRepair {
                placement_blocks,
                sample_offsets,
                partial,
                recheck_max,
            } => {
                for (height, hash) in placement_blocks {
                    blocks_to_migrate.insert(height, hash);
                }
                if partial {
                    crate::metrics::record_index_heal_unrepaired("partial_plan");
                    needs_retry = true;
                }
                recheck_after_migrate.push(RecheckTarget {
                    sm: sm.clone(),
                    recheck_max,
                    sample_offsets,
                });
            }
        }
    }

    if !blocks_to_migrate.is_empty() {
        debug!(
            index_heal.blocks = blocks_to_migrate.len(),
            "migrating blocks to repair storage-module data indexes"
        );
        let deferred_or_failed =
            migrate_storage_module_indexes(ctx, blocks_to_migrate, cancel).await?;
        if deferred_or_failed {
            needs_retry = true;
        }
    }

    // Completeness = same readiness predicate data_sync re-arm uses.
    for target in &recheck_after_migrate {
        if !index_ready_after_heal(target) {
            needs_retry = true;
        }
    }

    // Membership only — data_sync re-arms Blocked offsets via local index probe.
    if let Err(e) = ctx
        .service_senders
        .data_sync
        .send_traced(DataSyncServiceMessage::SyncPartitions)
    {
        error!(
            "Failed to send SyncPartitions message to data_sync service: {}",
            e
        );
    }

    Ok(HealOutcome { needs_retry })
}

/// SM that needed repair this pass — readiness re-checked after migrate.
struct RecheckTarget {
    sm: Arc<StorageModule>,
    /// Exclusive partition bound for the post-migrate path-hash density walk.
    recheck_max: PartitionChunkOffset,
    /// Offsets that drove placement-block selection (path-hash holes or
    /// readiness-sample misses). Completion requires these to pass
    /// [`StorageModule::is_data_root_index_ready_at`].
    sample_offsets: Vec<PartitionChunkOffset>,
}

enum IndexRepairPlan {
    /// Path-hash dense in the scan window **and** readiness samples pass.
    Complete,
    /// Placement blocks to re-migrate. `sample_offsets` are rechecked for
    /// data_sync readiness after migrate. `recheck_max` bounds the path-hash
    /// density walk.
    NeedsRepair {
        placement_blocks: BTreeMap<u64, BlockHash>,
        sample_offsets: Vec<PartitionChunkOffset>,
        partial: bool,
        recheck_max: PartitionChunkOffset,
    },
    SoftSkipped,
}

/// Post-migrate recheck: path-hash density in the plan window, then data_sync
/// readiness on sample offsets. Records unrepaired metrics; `false` → `needs_retry`.
fn index_ready_after_heal(target: &RecheckTarget) -> bool {
    match target
        .sm
        .missing_path_hash_ranges(PartitionChunkOffset::from(0), target.recheck_max)
    {
        Ok(gaps) if !gaps.is_empty() => {
            warn!(
                storage_module.id = target.sm.id,
                index_heal.first_gap = %gaps[0].0,
                index_heal.gap_count = gaps.len(),
                index_heal.max_partition_offset = %target.recheck_max,
                "path-hash index still gapped after heal; will retry"
            );
            crate::metrics::record_index_heal_unrepaired("gap_after_heal");
            return false;
        }
        Err(e) => {
            warn!(
                storage_module.id = target.sm.id,
                error = %e,
                "path-hash recheck failed after heal; will retry"
            );
            crate::metrics::record_index_heal_unrepaired("soft_skip");
            return false;
        }
        Ok(_) => {}
    }

    let fallback;
    let samples: &[PartitionChunkOffset] = if target.sample_offsets.is_empty() {
        fallback = readiness_sample_offsets(target.recheck_max);
        &fallback
    } else {
        &target.sample_offsets
    };
    if let Some(off) = samples
        .iter()
        .copied()
        .find(|off| !target.sm.is_data_root_index_ready_at(*off))
    {
        warn!(
            storage_module.id = target.sm.id,
            index_heal.first_unready = %off,
            index_heal.samples = samples.len(),
            "index not ready for data_sync after heal (path-hash dense); will retry"
        );
        crate::metrics::record_index_heal_unrepaired("not_ready");
        return false;
    }
    true
}

/// Cheap readiness samples across `[0, max_excl)` (first / mid / last).
///
/// Dense path-hash residual outside these points is not re-planned here; data_sync
/// re-arm still holds those offsets `Blocked` until they become ready.
fn readiness_sample_offsets(max_excl: PartitionChunkOffset) -> Vec<PartitionChunkOffset> {
    if *max_excl == 0 {
        return Vec::new();
    }
    let last = *max_excl - 1;
    let mid = last / 2;
    let mut offs = vec![
        PartitionChunkOffset::from(0),
        PartitionChunkOffset::from(mid),
        PartitionChunkOffset::from(last),
    ];
    offs.dedup();
    offs
}

/// Absolute ledger span of one placement block (for pure collection / tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlacementSpan {
    height: u64,
    block_hash: BlockHash,
    /// Inclusive absolute ledger start of this block's chunks.
    start_chunk_offset: u64,
    /// Exclusive absolute ledger end of this block's chunks.
    end_chunk_offset: u64,
}

/// Result of walking holes → unique placement blocks.
#[derive(Debug)]
struct PlacementCollectResult {
    placement_blocks: BTreeMap<u64, BlockHash>,
    /// One partition offset per successful bounds resolution (first offset that
    /// selected that jump). Post-migrate readiness samples.
    sample_offsets: Vec<PartitionChunkOffset>,
    bounds_lookups: u64,
    any_soft_skip: bool,
    recheck_max: PartitionChunkOffset,
}

/// Pure placement-block collection over path-hash / readiness holes.
///
/// For each hole, walks missing partition offsets, resolves the unique block
/// that introduced each ledger offset via `lookup`, then jumps to that block's
/// exclusive `end_chunk_offset` so multi-chunk blocks are not re-queried.
///
/// `lookup` returns [`None`] when bounds are uncertain (soft-skip that offset).
fn collect_placement_blocks_for_gaps(
    gaps: &[(PartitionChunkOffset, PartitionChunkOffset)],
    sm_ledger_start: u64,
    // Exclusive on-ledger frontier (`total_chunks`).
    max_chunk_offset: u64,
    max_partition_offset: PartitionChunkOffset,
    mut lookup: impl FnMut(u64) -> Option<PlacementSpan>,
) -> PlacementCollectResult {
    let mut placement_blocks: BTreeMap<u64, BlockHash> = BTreeMap::new();
    let mut sample_offsets: Vec<PartitionChunkOffset> = Vec::new();
    let mut any_soft_skip = false;
    let mut recheck_max = PartitionChunkOffset::from(0);
    let mut bounds_lookups = 0_u64;

    for &(gap_start, gap_end) in gaps {
        if gap_start >= gap_end {
            continue;
        }

        // Exclusive end of this hole, clamped to the on-ledger frontier.
        let gap_last = PartitionChunkOffset(*gap_end - 1);
        let end_ledger = sm_ledger_start + u64::from(*gap_last);
        let clamped_end_abs = if end_ledger >= max_chunk_offset {
            max_chunk_offset.saturating_sub(1)
        } else {
            end_ledger
        };
        let start_ledger_abs = sm_ledger_start + u64::from(*gap_start);
        if start_ledger_abs >= max_chunk_offset || clamped_end_abs < start_ledger_abs {
            // Entire hole past frontier.
            continue;
        }

        // Partition-relative exclusive end for post-migrate recheck. Offsets are
        // `u32` in the partition domain; on overflow fall back to the scan bound
        // (same as a full-window recheck for this SM).
        let hole_recheck = PartitionChunkOffset::from(
            u32::try_from(clamped_end_abs - sm_ledger_start + 1).unwrap_or(*max_partition_offset),
        );
        if hole_recheck > recheck_max {
            recheck_max = hole_recheck;
        }

        // Walk missing partition offsets; for each, resolve the unique block that
        // introduced that ledger offset, then jump past that block's full ledger
        // span so we do not re-query every chunk in the same block.
        let mut part_off = *gap_start;
        let gap_end_excl = *gap_end;
        while part_off < gap_end_excl {
            let ledger_abs = sm_ledger_start + u64::from(part_off);
            if ledger_abs >= max_chunk_offset {
                break;
            }
            bounds_lookups += 1;
            let Some(span) = lookup(ledger_abs) else {
                any_soft_skip = true;
                // Skip one offset and continue — other offsets in the hole may resolve.
                part_off = part_off.saturating_add(1);
                continue;
            };

            // First time we see this height: record a readiness sample offset.
            if !placement_blocks.contains_key(&span.height) {
                sample_offsets.push(PartitionChunkOffset::from(part_off));
            }
            placement_blocks.insert(span.height, span.block_hash);

            // Block covers absolute [start_chunk_offset, end_chunk_offset). Jump to
            // the first partition offset at or after end_chunk_offset (always advance
            // at least one to avoid a stuck loop on degenerate bounds).
            let next_abs = span.end_chunk_offset.max(ledger_abs.saturating_add(1));
            let next_rel = next_abs.saturating_sub(sm_ledger_start);
            // Partition offsets are u32; overflow means "past any real SM" — jump
            // to u32::MAX so the loop exits via `part_off < gap_end_excl`.
            let next_part = u32::try_from(next_rel).unwrap_or(u32::MAX);
            part_off = next_part.max(part_off.saturating_add(1));
        }
    }

    PlacementCollectResult {
        placement_blocks,
        sample_offsets,
        bounds_lookups,
        any_soft_skip,
        recheck_max,
    }
}

fn plan_index_repair(ctx: &IndexHealCtx<'_>, sm: &Arc<StorageModule>) -> IndexRepairPlan {
    let Some(ledger_id) = sm.partition_assignment().and_then(|a| a.ledger_id) else {
        warn!(
            storage_module.id = sm.id,
            "storage module not assigned to a data ledger slot during index plan; soft-skip"
        );
        return IndexRepairPlan::SoftSkipped;
    };

    let Ok(ledger_range) = sm.get_storage_module_ledger_offsets() else {
        warn!(
            storage_module.id = sm.id,
            "storage module not assigned to a ledger during index plan; soft-skip"
        );
        return IndexRepairPlan::SoftSkipped;
    };

    let Some(max_partition_offset) = get_max_partition_offset(ctx, sm) else {
        return IndexRepairPlan::SoftSkipped;
    };
    if *max_partition_offset == 0 {
        return IndexRepairPlan::Complete;
    }

    let gaps = match sm
        .missing_path_hash_ranges(PartitionChunkOffset::from(0), max_partition_offset)
    {
        Ok(gaps) if gaps.is_empty() => {
            // Path-hash dense. Completeness still requires data_sync readiness
            // (DataRootInfos residual). Unready samples become single-offset holes.
            let unready: Vec<PartitionChunkOffset> = readiness_sample_offsets(max_partition_offset)
                .into_iter()
                .filter(|off| !sm.is_data_root_index_ready_at(*off))
                .collect();
            if unready.is_empty() {
                return IndexRepairPlan::Complete;
            }
            debug!(
                storage_module.id = sm.id,
                index_heal.unready_samples = unready.len(),
                index_heal.max_partition_offset = %max_partition_offset,
                "path-hash dense but data_sync readiness samples fail; planning placement re-migrate"
            );
            unready
                .into_iter()
                .map(|off| (off, PartitionChunkOffset::from(*off + 1)))
                .collect()
        }
        Ok(gaps) => gaps,
        Err(e) => {
            warn!(
                storage_module.id = sm.id,
                error = %e,
                "path-hash gap scan failed; soft-skipping index repair for this module"
            );
            return IndexRepairPlan::SoftSkipped;
        }
    };

    debug!(
        storage_module.id = sm.id,
        index_heal.gap_count = gaps.len(),
        index_heal.first_gap = %gaps[0].0,
        index_heal.max_partition_offset = %max_partition_offset,
        "index holes detected; collecting placement blocks"
    );

    let block_index_guard = ctx.block_index.read();
    let Some(latest_item) = block_index_guard.get_latest_item() else {
        return IndexRepairPlan::SoftSkipped;
    };

    let Ok(data_ledger) = DataLedger::try_from(ledger_id) else {
        warn!(
            storage_module.id = sm.id,
            ledger_id, "invalid ledger id during index plan; soft-skip"
        );
        return IndexRepairPlan::SoftSkipped;
    };
    let max_chunk_offset = latest_item
        .ledgers
        .iter()
        .find(|l| l.ledger == data_ledger)
        .map(|l| l.total_chunks)
        .unwrap_or(0);

    if max_chunk_offset == 0 {
        return IndexRepairPlan::Complete;
    }

    let sm_ledger_start = *ledger_range.start();
    let sm_id = sm.id;
    let collected = collect_placement_blocks_for_gaps(
        &gaps,
        sm_ledger_start,
        max_chunk_offset,
        max_partition_offset,
        |ledger_abs| {
            let ledger_off = LedgerChunkOffset::from(ledger_abs);
            block_bounds_for_ledger_offset(block_index_guard, data_ledger, ledger_off, sm_id).map(
                |bounds| PlacementSpan {
                    height: bounds.height,
                    block_hash: bounds.block_hash,
                    start_chunk_offset: bounds.start_chunk_offset,
                    end_chunk_offset: bounds.end_chunk_offset,
                },
            )
        },
    );

    if collected.placement_blocks.is_empty() {
        return if collected.any_soft_skip {
            IndexRepairPlan::SoftSkipped
        } else {
            // All holes past frontier — nothing to repair in the on-ledger range.
            IndexRepairPlan::Complete
        };
    }

    debug!(
        storage_module.id = sm.id,
        index_heal.placement_blocks = collected.placement_blocks.len(),
        index_heal.bounds_lookups = collected.bounds_lookups,
        index_heal.samples = collected.sample_offsets.len(),
        index_heal.recheck_max = %collected.recheck_max,
        "collected unique placement blocks for index holes"
    );

    IndexRepairPlan::NeedsRepair {
        placement_blocks: collected.placement_blocks,
        sample_offsets: collected.sample_offsets,
        partial: collected.any_soft_skip,
        recheck_max: collected.recheck_max,
    }
}

/// Block that introduced `offset` on `data_ledger`, if resolvable.
fn block_bounds_for_ledger_offset(
    block_index: &irys_domain::BlockIndex,
    data_ledger: DataLedger,
    offset: LedgerChunkOffset,
    storage_module_id: usize,
) -> Option<irys_domain::BlockBounds> {
    match block_index.get_block_bounds(data_ledger, offset) {
        Ok(bounds) => Some(bounds),
        Err(
            BlockBoundsError::IndexEmpty
            | BlockBoundsError::LedgerInactive { .. }
            | BlockBoundsError::OffsetBeyondFrontier { .. },
        ) => None,
        Err(BlockBoundsError::Internal(e)) => {
            warn!(
                storage_module.id = storage_module_id,
                error = %e,
                "block bounds internal error; soft-skipping offset during index repair"
            );
            None
        }
    }
}

/// Returns `true` if any work was deferred or failed (caller should set `needs_retry`).
async fn migrate_storage_module_indexes(
    ctx: &IndexHealCtx<'_>,
    blocks_to_migrate: BTreeMap<u64, BlockHash>,
    cancel: &Shutdown,
) -> eyre::Result<bool> {
    let total_planned = blocks_to_migrate.len();
    let mut incomplete = false;

    let blocks_this_pass: BTreeMap<u64, BlockHash> =
        if total_planned > INDEX_HEAL_MAX_BLOCKS_PER_PASS {
            let deferred = total_planned - INDEX_HEAL_MAX_BLOCKS_PER_PASS;
            warn!(
                index_heal.planned = total_planned,
                index_heal.pass_limit = INDEX_HEAL_MAX_BLOCKS_PER_PASS,
                index_heal.deferred = deferred,
                "capping index heal migrate pass; remaining holes retry on next heal"
            );
            incomplete = true;
            blocks_to_migrate
                .into_iter()
                .take(INDEX_HEAL_MAX_BLOCKS_PER_PASS)
                .collect()
        } else {
            blocks_to_migrate
        };

    let pass_started = Instant::now();
    let migration_service = &ctx.service_senders.chunk_migration;
    for (block_height, block_hash) in blocks_this_pass {
        if shutdown_requested(cancel) {
            debug!(
                block.height = block_height,
                "shutdown requested during index-heal migration; returning early"
            );
            return Ok(true);
        }
        if pass_started.elapsed() >= INDEX_HEAL_MIGRATE_PASS_BUDGET {
            warn!(
                block.height = block_height,
                index_heal.pass_budget_secs = INDEX_HEAL_MIGRATE_PASS_BUDGET.as_secs(),
                "index-heal migrate pass budget exhausted; deferring remaining blocks"
            );
            return Ok(true);
        }
        let (tx, rx) = tokio::sync::oneshot::channel();

        if let Err(e) = migration_service.send_traced(
            ChunkMigrationServiceMessage::UpdateStorageModuleIndexes {
                block_hash,
                receiver: tx,
            },
        ) {
            error!(
                "Failed to send migration request for block {} (height {}): {}",
                block_hash, block_height, e
            );
            return Err(eyre!(
                "Unable to index storage module chunks due to mpsc send failure: {}",
                e
            ));
        }

        match tokio::time::timeout(INDEX_HEAL_MIGRATE_RESPONSE_TIMEOUT, rx).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => {
                warn!(
                    block.hash = %block_hash,
                    block.height = block_height,
                    error = %e,
                    "UpdateStorageModuleIndexes failed; soft-skipping block"
                );
                incomplete = true;
            }
            Ok(Err(e)) => {
                error!(
                    "Failed to receive migration response for block {} (height {}): {}",
                    block_hash, block_height, e
                );
                incomplete = true;
            }
            Err(_elapsed) => {
                warn!(
                    block.hash = %block_hash,
                    block.height = block_height,
                    timeout_secs = INDEX_HEAL_MIGRATE_RESPONSE_TIMEOUT.as_secs(),
                    "UpdateStorageModuleIndexes response timed out; soft-skipping block"
                );
                incomplete = true;
            }
        }
    }
    Ok(incomplete)
}

fn get_max_partition_offset(
    ctx: &IndexHealCtx<'_>,
    storage_module: &Arc<StorageModule>,
) -> Option<PartitionChunkOffset> {
    let Some(ledger_id) = storage_module
        .partition_assignment()
        .and_then(|a| a.ledger_id)
    else {
        warn!(
            storage_module.id = storage_module.id,
            "storage module not assigned to a data ledger slot; cannot determine max offset"
        );
        return None;
    };

    let current_height = ctx.block_tree.read().get_latest_canonical_entry().height();
    let migration_height =
        current_height.saturating_sub(ctx.config.consensus.block_migration_depth as u64);

    let max_ledger_offset = ctx.block_tree.get_total_chunks(migration_height, ledger_id);

    let Ok(range) = storage_module.get_storage_module_ledger_offsets() else {
        warn!(
            storage_module.id = storage_module.id,
            "storage module not assigned to a ledger; cannot determine max offset"
        );
        return None;
    };
    let start: u64 = *range.start();
    let end_incl: u64 = *range.end();
    let max_excl = max_ledger_offset.map(|m| *m);

    Some(PartitionChunkOffset::from(exclusive_partition_end(
        start, end_incl, max_excl,
    )))
}

fn shutdown_requested(cancel: &Shutdown) -> bool {
    cancel.clone().now_or_never().is_some()
}

/// Exclusive partition-relative end for index scan / backfill.
pub(super) fn exclusive_partition_end(
    start: u64,
    end_incl: u64,
    max_ledger_excl: Option<u64>,
) -> u64 {
    let sm_len_excl = end_incl.saturating_sub(start).saturating_add(1);
    match max_ledger_excl {
        Some(max) if max > start && max <= end_incl.saturating_add(1) => max.saturating_sub(start),
        Some(max) if max <= start => 0,
        Some(_) => sm_len_excl,
        None => 0,
    }
}

#[cfg(test)]
#[path = "index_heal_tests.rs"]
mod tests;
