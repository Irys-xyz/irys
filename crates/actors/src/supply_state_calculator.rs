//! Async supply state recalculation on node startup.
//!
//! This module recalculates the cumulative emitted supply
//! by iterating through all blocks in the block index. This runs asynchronously
//! on node startup to ensure the supply state is always accurate.

use eyre::Result;
use irys_database::{block_header_by_hash, db::IrysDatabaseExt as _};
use irys_domain::{BlockIndexReadGuard, SupplyState};
use irys_types::{DatabaseProvider, U256};
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Recalculates the cumulative emitted supply by iterating through all blocks
/// in the block index.
///
/// This function:
/// 1. Marks the supply state as not ready
/// 2. Iterates through all blocks from genesis to latest
/// 3. Sums up all reward_amount values
/// 4. Updates the supply state with the final total
/// 5. Marks the supply state as ready
///
/// This should be spawned as a background task during node startup.
pub async fn recalculate_supply_state(
    supply_state: Arc<SupplyState>,
    block_index: BlockIndexReadGuard,
    db: DatabaseProvider,
) -> Result<()> {
    info!("Starting supply state recalculation");
    supply_state.mark_not_ready();

    match do_recalculation(&supply_state, &block_index, &db).await {
        Ok(()) => {
            let state = supply_state.get();
            info!(
                height = state.height,
                cumulative_emitted = %state.cumulative_emitted,
                "Supply state recalculation complete"
            );
            Ok(())
        }
        Err(e) => {
            warn!("Supply state recalculation failed: {}", e);
            Err(e)
        }
    }
}

async fn do_recalculation(
    supply_state: &SupplyState,
    block_index: &BlockIndexReadGuard,
    db: &DatabaseProvider,
) -> Result<()> {
    let latest_height = {
        let index = block_index.read();
        if index.num_blocks() == 0 {
            info!("Block index is empty, nothing to recalculate");
            supply_state.set_and_mark_ready(0, U256::zero())?;
            return Ok(());
        }
        index.latest_height()
    };

    info!(
        "Recalculating supply state from genesis to height {}",
        latest_height
    );

    let mut cumulative_emitted = U256::zero();
    let mut last_log_height = 0_u64;
    const LOG_INTERVAL: u64 = 10000;
    const YIELD_INTERVAL: u64 = 1000;

    for height in 0..=latest_height {
        let block_hash = {
            let index = block_index.read();
            match index.get_item(height) {
                Some(item) => item.block_hash,
                None => {
                    warn!("Block at height {} not found in index", height);
                    continue;
                }
            }
        };

        let block_header = db
            .view_eyre(|tx| block_header_by_hash(tx, &block_hash, false))?
            .ok_or_else(|| eyre::eyre!("Block header not found for hash {}", block_hash))?;

        cumulative_emitted = cumulative_emitted.saturating_add(block_header.reward_amount);

        if height - last_log_height >= LOG_INTERVAL {
            debug!(
                "Supply recalculation progress: height {}/{}, cumulative: {}",
                height, latest_height, cumulative_emitted
            );
            last_log_height = height;
        }

        if height % YIELD_INTERVAL == 0 {
            tokio::task::yield_now().await;
        }
    }

    supply_state.set_and_mark_ready(latest_height, cumulative_emitted)?;

    Ok(())
}
