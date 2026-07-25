//! Path-hash index heal for ledger-assigned storage modules.
//!
//! Detects holes, re-migrates covering blocks via `UpdateStorageModuleIndexes`,
//! and reports whether another pass is still needed. Data-sync liveness is
//! **not** driven from here — orchestrators re-queue `Blocked` offsets by
//! probing local index readiness on their own tick.

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
    time::Duration,
};
use tracing::{debug, error, warn};

/// Max blocks re-indexed in one heal pass. Remaining holes retry when
/// [`HealOutcome::needs_retry`] is true (periodic / assignment / restart).
pub(super) const INDEX_HEAL_MAX_BLOCKS_PER_PASS: usize = 128;

/// Per-block wait for `UpdateStorageModuleIndexes`.
pub(super) const INDEX_HEAL_MIGRATE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Steady-state heal poll when the last pass left unrepaired work.
pub(super) const INDEX_HEAL_RETRY_INTERVAL: Duration = Duration::from_secs(60);

/// Result of one heal pass across all ledger-assigned SMs.
#[derive(Debug, Clone, Copy)]
pub(super) struct HealOutcome {
    /// True when any SM still has path-hash gaps, soft-skips, or deferred migrate
    /// work — the service should schedule another pass without waiting for epoch.
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

/// Plan / migrate / re-check path-hash indexes for every ledger-assigned SM.
///
/// Always refreshes data_sync membership via `SyncPartitions`. Does **not**
/// push unblock signals — data_sync re-queues `Blocked` offsets by probing
/// whether the local index is ready at each offset.
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
    let mut blocks_to_migrate: BTreeMap<u64, BlockHash> = BTreeMap::new();
    // SMs that needed repair this pass — re-check gaps after migrate.
    let mut recheck_after_migrate: Vec<(Arc<StorageModule>, PartitionChunkOffset)> = Vec::new();

    for sm in &ledger_modules {
        match plan_index_repair(ctx, sm) {
            IndexRepairPlan::Complete => {}
            IndexRepairPlan::SoftSkipped => {
                crate::metrics::record_index_heal_unrepaired("soft_skip");
                needs_retry = true;
            }
            IndexRepairPlan::NeedsRepair {
                height_spans,
                partial,
                recheck_max,
            } => {
                let bi = ctx.block_index.read();
                for (start_height, end_height) in height_spans {
                    for height in start_height..=end_height {
                        let Some(item) = bi.get_item(height) else {
                            // Skip missing height; continue other heights in the span
                            // so a single reorg hole does not drop the rest of the plan.
                            warn!(
                                storage_module.id = sm.id,
                                block.height = height,
                                plan.start_height = start_height,
                                plan.end_height = end_height,
                                "block missing from index during heal; continuing remaining heights"
                            );
                            needs_retry = true;
                            continue;
                        };
                        blocks_to_migrate.insert(height, item.block_hash);
                    }
                }
                if partial {
                    crate::metrics::record_index_heal_unrepaired("partial_plan");
                    needs_retry = true;
                }
                recheck_after_migrate.push((sm.clone(), recheck_max));
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

    // Only re-scan modules that actually needed repair — Completes already proved
    // dense at plan time; a second density walk every pass is wasted work.
    for (sm, recheck_max) in &recheck_after_migrate {
        match sm.missing_path_hash_ranges(PartitionChunkOffset::from(0), *recheck_max) {
            Ok(gaps) if gaps.is_empty() => {}
            Ok(gaps) => {
                warn!(
                    storage_module.id = sm.id,
                    index_heal.first_gap = %gaps[0].0,
                    index_heal.gap_count = gaps.len(),
                    index_heal.max_partition_offset = %recheck_max,
                    "path-hash index still gapped after heal; will retry"
                );
                crate::metrics::record_index_heal_unrepaired("gap_after_heal");
                needs_retry = true;
            }
            Err(e) => {
                warn!(
                    storage_module.id = sm.id,
                    error = %e,
                    "path-hash recheck failed after heal; will retry"
                );
                crate::metrics::record_index_heal_unrepaired("soft_skip");
                needs_retry = true;
            }
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

enum IndexRepairPlan {
    /// No repairable path-hash gap in the scan window.
    Complete,
    /// Inclusive block-height spans covering holes; `recheck_max` is the plan-time
    /// exclusive partition bound used after migrate to decide `needs_retry`.
    NeedsRepair {
        height_spans: Vec<(u64, u64)>,
        partial: bool,
        recheck_max: PartitionChunkOffset,
    },
    SoftSkipped,
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

    let gaps =
        match sm.missing_path_hash_ranges(PartitionChunkOffset::from(0), max_partition_offset) {
            Ok(gaps) if gaps.is_empty() => return IndexRepairPlan::Complete,
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
        "path-hash index gaps detected; scheduling per-hole index repair"
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

    let mut height_spans = Vec::new();
    let mut any_soft_skip = false;
    let mut recheck_max = PartitionChunkOffset::from(0);
    for (gap_start, gap_end) in gaps {
        if gap_start >= gap_end {
            continue;
        }
        let gap_last = PartitionChunkOffset(*gap_end - 1);

        let start_ledger = ledger_range.start() + LedgerChunkOffset::from(*gap_start);
        if *start_ledger >= max_chunk_offset {
            continue;
        }

        let end_ledger = ledger_range.start() + LedgerChunkOffset::from(*gap_last);
        let clamped_end = if *end_ledger >= max_chunk_offset {
            LedgerChunkOffset::from(max_chunk_offset - 1)
        } else {
            end_ledger
        };
        if clamped_end < start_ledger {
            continue;
        }

        let Some(start_block) = block_height_for_ledger_offset(
            block_index_guard,
            data_ledger,
            start_ledger,
            sm.id,
            "gap_start",
        ) else {
            any_soft_skip = true;
            continue;
        };
        let Some(end_block) = block_height_for_ledger_offset(
            block_index_guard,
            data_ledger,
            clamped_end,
            sm.id,
            "gap_end",
        ) else {
            any_soft_skip = true;
            continue;
        };

        let hole_recheck = PartitionChunkOffset::from(
            u32::try_from(*clamped_end - *ledger_range.start() + 1)
                .unwrap_or(*max_partition_offset),
        );
        if hole_recheck > recheck_max {
            recheck_max = hole_recheck;
        }

        height_spans.push((start_block, end_block));
    }

    if height_spans.is_empty() {
        return if any_soft_skip {
            IndexRepairPlan::SoftSkipped
        } else {
            // All holes past frontier — nothing to repair in the on-ledger range.
            IndexRepairPlan::Complete
        };
    }

    IndexRepairPlan::NeedsRepair {
        height_spans,
        partial: any_soft_skip,
        recheck_max,
    }
}

fn block_height_for_ledger_offset(
    block_index: &irys_domain::BlockIndex,
    data_ledger: DataLedger,
    offset: LedgerChunkOffset,
    storage_module_id: usize,
    bound_label: &'static str,
) -> Option<u64> {
    match block_index.get_block_bounds(data_ledger, offset) {
        Ok(bounds) => Some(bounds.height),
        Err(
            BlockBoundsError::IndexEmpty
            | BlockBoundsError::LedgerInactive { .. }
            | BlockBoundsError::OffsetBeyondFrontier { .. },
        ) => None,
        Err(BlockBoundsError::Internal(e)) => {
            warn!(
                storage_module.id = storage_module_id,
                error = %e,
                bound = bound_label,
                "block bounds internal error; soft-skipping index repair"
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

    let migration_service = &ctx.service_senders.chunk_migration;
    for (block_height, block_hash) in blocks_this_pass {
        if shutdown_requested(cancel) {
            debug!(
                block.height = block_height,
                "shutdown requested during index-heal migration; returning early"
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
mod exclusive_partition_end_tests {
    use super::exclusive_partition_end;

    #[test]
    fn full_sm_includes_last_chunk_when_frontier_past_sm() {
        assert_eq!(exclusive_partition_end(0, 99, Some(1_000)), 100);
        assert_eq!(exclusive_partition_end(0, 99, Some(100)), 100);
    }

    #[test]
    fn partial_frontier_inside_sm_is_exclusive_relative() {
        assert_eq!(exclusive_partition_end(0, 99, Some(50)), 50);
    }

    #[test]
    fn frontier_before_sm_is_empty() {
        assert_eq!(exclusive_partition_end(100, 199, Some(50)), 0);
        assert_eq!(exclusive_partition_end(100, 199, Some(100)), 0);
    }

    #[test]
    fn frontier_none_is_empty() {
        assert_eq!(exclusive_partition_end(0, 99, None), 0);
    }

    #[test]
    fn non_zero_sm_start() {
        assert_eq!(exclusive_partition_end(100, 199, Some(150)), 50);
        assert_eq!(exclusive_partition_end(100, 199, Some(500)), 100);
    }
}
