use eyre::Result;
use irys_database::{
    block_header_by_hash, db::IrysDatabaseExt as _, find_missing_block_reward_heights,
    insert_block_rewards_batch,
};
use irys_domain::{BlockIndexReadGuard, SupplyState};
use irys_types::{DatabaseProvider, U256};
use std::sync::Arc;
use tracing::{debug, info, warn};

const BATCH_SIZE: u64 = 1000;

pub async fn recalculate_supply_state(
    supply_state: Arc<SupplyState>,
    block_index: BlockIndexReadGuard,
    db: DatabaseProvider,
) -> Result<()> {
    info!("Starting supply state gap-fill");
    supply_state.mark_not_ready();

    match do_gap_fill(&supply_state, &block_index, &db).await {
        Ok(()) => {
            let state = supply_state.get();
            info!(
                height = ?state.height,
                cumulative_emitted = %state.cumulative_emitted,
                "Supply state initialization complete"
            );
            Ok(())
        }
        Err(e) => {
            warn!("Supply state gap-fill failed: {}", e);
            Err(e)
        }
    }
}

async fn do_gap_fill(
    supply_state: &SupplyState,
    block_index: &BlockIndexReadGuard,
    db: &DatabaseProvider,
) -> Result<()> {
    let latest_height = {
        let index = block_index.read();
        if index.num_blocks() == 0 {
            info!("Block index is empty, nothing to initialize");
            supply_state.recalculate_and_mark_ready(db)?;
            return Ok(());
        }
        index.latest_height()
    };

    info!(
        "Checking for gaps in BlockRewards table (heights 0..={})",
        latest_height
    );

    let mut processed_height = fill_gaps_up_to(block_index, db, 0, latest_height).await?;

    loop {
        let current_height = block_index.read().latest_height();
        if current_height <= processed_height {
            break;
        }

        info!(
            "Catching up blocks {} to {} that arrived during gap-fill",
            processed_height + 1,
            current_height
        );

        processed_height =
            fill_gaps_up_to(block_index, db, processed_height + 1, current_height).await?;
    }

    supply_state.recalculate_and_mark_ready(db)?;

    Ok(())
}

async fn fill_gaps_up_to(
    block_index: &BlockIndexReadGuard,
    db: &DatabaseProvider,
    from_height: u64,
    to_height: u64,
) -> Result<u64> {
    let mut total_gaps_filled = 0_u64;
    let mut current_from = from_height;

    while current_from <= to_height {
        let current_to = current_from.saturating_add(BATCH_SIZE - 1).min(to_height);

        // Find missing heights in this batch with a single cursor scan
        let missing_heights =
            db.view_eyre(|tx| find_missing_block_reward_heights(tx, current_from, current_to))?;

        if !missing_heights.is_empty() {
            // Collect rewards for all missing heights
            let mut rewards_to_insert = Vec::with_capacity(missing_heights.len());
            for &height in &missing_heights {
                let reward = get_block_reward_from_header(block_index, db, height)?;
                rewards_to_insert.push((height, reward));
            }

            // Batch insert all rewards in a single transaction
            db.update_eyre(|tx| insert_block_rewards_batch(tx, &rewards_to_insert))?;
            total_gaps_filled += missing_heights.len() as u64;
        }

        debug!(
            "Gap-fill progress: heights {}..={}/{}, gaps filled: {}",
            current_from, current_to, to_height, total_gaps_filled
        );

        current_from = current_to + 1;
        tokio::task::yield_now().await;
    }

    if total_gaps_filled > 0 {
        info!(
            "Filled {} gaps in BlockRewards table (heights {}..={})",
            total_gaps_filled, from_height, to_height
        );
    }

    Ok(to_height)
}

fn get_block_reward_from_header(
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
