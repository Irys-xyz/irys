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
    let initial_height = {
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
        initial_height
    );

    let mut cumulative_emitted = U256::zero();
    let mut processed_height = 0_u64;
    let mut last_log_height = 0_u64;
    const LOG_INTERVAL: u64 = 10000;
    const YIELD_INTERVAL: u64 = 1000;

    for height in 0..=initial_height {
        cumulative_emitted = process_block_reward(block_index, db, height, cumulative_emitted)?;
        processed_height = height;

        if height >= last_log_height.saturating_add(LOG_INTERVAL) {
            debug!(
                "Supply recalculation progress: height {}/{}, cumulative: {}",
                height, initial_height, cumulative_emitted
            );
            last_log_height = height;
        }

        if height % YIELD_INTERVAL == 0 {
            tokio::task::yield_now().await;
        }
    }

    loop {
        let current_height = block_index.read().latest_height();

        if current_height <= processed_height {
            break;
        }

        info!(
            "Catching up blocks {} to {} that arrived during recalculation",
            processed_height + 1,
            current_height
        );

        for height in (processed_height + 1)..=current_height {
            cumulative_emitted = process_block_reward(block_index, db, height, cumulative_emitted)?;
            processed_height = height;

            if height % YIELD_INTERVAL == 0 {
                tokio::task::yield_now().await;
            }
        }
    }

    supply_state.set_and_mark_ready(processed_height, cumulative_emitted)?;

    // Final catch-up: process any blocks that arrived after the last check but before
    // set_and_mark_ready.
    let final_height = block_index.read().latest_height();
    if final_height > processed_height {
        info!(
            "Processing {} blocks that arrived during final state update",
            final_height - processed_height
        );
        for height in (processed_height + 1)..=final_height {
            let reward = get_block_reward(block_index, db, height)?;
            supply_state.add_block_reward(height, reward)?;
        }
    }

    Ok(())
}

fn process_block_reward(
    block_index: &BlockIndexReadGuard,
    db: &DatabaseProvider,
    height: u64,
    cumulative: U256,
) -> Result<U256> {
    let reward = get_block_reward(block_index, db, height)?;
    Ok(cumulative.saturating_add(reward))
}

fn get_block_reward(
    block_index: &BlockIndexReadGuard,
    db: &DatabaseProvider,
    height: u64,
) -> Result<U256> {
    let block_hash = {
        let index = block_index.read();
        match index.get_item(height) {
            Some(item) => item.block_hash,
            None => {
                return Err(eyre::eyre!(
                    "Block at height {} not found in index - cannot proceed with missing blocks",
                    height
                ));
            }
        }
    };

    let block_header = db
        .view_eyre(|tx| block_header_by_hash(tx, &block_hash, false))?
        .ok_or_else(|| eyre::eyre!("Block header not found for hash {}", block_hash))?;

    Ok(block_header.reward_amount)
}
