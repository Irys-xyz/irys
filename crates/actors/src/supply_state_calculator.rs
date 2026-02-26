use eyre::Result;
use irys_database::{block_header_by_hash, db::IrysDatabaseExt as _};
use irys_domain::{BlockIndexReadGuard, SupplyState};
use irys_types::{DatabaseProvider, U256};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Waits for the first block migration to set `first_migration_height`.
///
/// We use this rather than block_index to determine backfill range because the migrate_block
/// method updated the supply state.  Waiting for the first migrated block after restart allows us
/// to be absolutely sure there isn't a mismatch or race condition between block migration and
/// block index. This establishes a clear boundary between backfill (historical) and live tracking.
async fn wait_for_first_migration(
    supply_state: &SupplyState,
    cancel: &CancellationToken,
) -> Option<u64> {
    tokio::select! {
        _ = cancel.cancelled() => {
            info!("Backfill cancelled while waiting for first migration");
            None
        }
        height = supply_state.wait_for_first_migration() => {
            info!(
                "First migration detected at height {}, starting backfill",
                height
            );
            Some(height)
        }
    }
}

pub async fn backfill_supply_state(
    supply_state: Arc<SupplyState>,
    block_index: BlockIndexReadGuard,
    db: DatabaseProvider,
    cancel: CancellationToken,
) -> Result<()> {
    info!("Starting supply state backfill");

    match do_backfill(&supply_state, &block_index, &db, &cancel).await {
        Ok(true) => {
            let state = supply_state.get();
            info!(
                height = state.height,
                cumulative_emitted = %state.cumulative_emitted,
                "Supply state backfill complete"
            );
            Ok(())
        }
        Ok(false) => {
            info!("Supply state backfill cancelled");
            Ok(())
        }
        Err(e) => {
            warn!("Supply state backfill failed: {}", e);
            Err(e)
        }
    }
}

async fn do_backfill(
    supply_state: &SupplyState,
    block_index: &BlockIndexReadGuard,
    db: &DatabaseProvider,
    cancel: &CancellationToken,
) -> Result<bool> {
    let Some(first_migration_height) = wait_for_first_migration(supply_state, cancel).await else {
        return Ok(false);
    };

    // Validate persisted state is not ahead of current migration boundary
    if let Some(persisted_height) = supply_state.persisted_backfill_height()
        && persisted_height >= first_migration_height
    {
        return Err(eyre::eyre!(
            "Persisted backfill height {} is >= first migration height {}. \
                 Refusing to continue with potentially stale supply state. \
                 This may indicate a chain reset, corrupted state file, or mismatched data directory.",
            persisted_height,
            first_migration_height
        ));
    }

    // Start from persisted backfill height + 1, or 0 if no previous backfill
    let backfill_start = supply_state
        .persisted_backfill_height()
        .map(|h| h.saturating_add(1))
        .unwrap_or(0);

    // End at first_migration_height - 1 (blocks before first migration)
    let backfill_end = first_migration_height.saturating_sub(1);

    // No backfill needed if start > end (already caught up or first migration at genesis)
    if backfill_start > backfill_end || first_migration_height == 0 {
        info!(
            persisted_height = ?supply_state.persisted_backfill_height(),
            first_migration_height,
            "No additional backfill needed"
        );
        supply_state.add_historical_sum_and_mark_ready(U256::zero())?;
        return Ok(true);
    }

    info!(
        "Backfilling supply state from height {} to {}",
        backfill_start, backfill_end
    );

    let mut historical_sum = U256::zero();
    let mut last_log_height = backfill_start;
    const LOG_INTERVAL: u64 = 10000;
    const BATCH_SIZE: u64 = 1000;

    let mut height = backfill_start;
    while height <= backfill_end {
        if cancel.is_cancelled() {
            info!("Backfill cancelled at height {}/{}", height, backfill_end);
            return Ok(false);
        }

        let batch_end = height
            .saturating_add(BATCH_SIZE.saturating_sub(1))
            .min(backfill_end);
        let batch_rewards = get_block_rewards_batch(block_index, db, height, batch_end)?;

        let expected_count = (batch_end - height + 1) as usize;
        assert_eq!(
            batch_rewards.len(),
            expected_count,
            "Batch returned {} rewards but expected {} for heights {}-{}",
            batch_rewards.len(),
            expected_count,
            height,
            batch_end
        );

        for reward in batch_rewards {
            historical_sum = historical_sum.saturating_add(reward);
        }

        if batch_end >= last_log_height.saturating_add(LOG_INTERVAL) {
            debug!(
                "Backfill progress: height {}/{}, sum: {}",
                batch_end, backfill_end, historical_sum
            );
            last_log_height = batch_end;
        }

        height = batch_end.saturating_add(1);
        tokio::task::yield_now().await;
    }

    supply_state.add_historical_sum_and_mark_ready(historical_sum)?;

    Ok(true)
}

fn get_block_rewards_batch(
    block_index: &BlockIndexReadGuard,
    db: &DatabaseProvider,
    start_height: u64,
    end_height: u64,
) -> Result<Vec<U256>> {
    let block_hashes: Vec<_> = {
        let index = block_index.read();
        (start_height..=end_height)
            .map(|height| {
                index
                    .get_item(height)
                    .map(|item| item.block_hash)
                    .ok_or_else(|| {
                        eyre::eyre!(
                            "Block at height {} not found in index - cannot proceed with missing blocks",
                            height
                        )
                    })
            })
            .collect::<Result<Vec<_>>>()?
    };

    db.view_eyre(|tx| {
        block_hashes
            .iter()
            .map(|hash| {
                block_header_by_hash(tx, hash, false)?
                    .ok_or_else(|| eyre::eyre!("Block header not found for hash {}", hash))
                    .map(|h| h.reward_amount)
            })
            .collect()
    })
}
