use crate::utils::{IrysNodeTest, TimeMode};
use irys_chain::IrysNodeCtx;
use irys_types::{H256, IrysBlockHeader, NodeConfig, NodeMode, SyncMode};
use std::time::{Duration, Instant};

const SECONDS_TO_WAIT: usize = 30;

#[test_log::test(tokio::test)]
async fn heavy_shallow_fork_with_diverged_reset_seeds_heals_vdf_and_syncs() -> eyre::Result<()> {
    let block_migration_depth: u32 = 6;
    let reset_frequency = 120_usize;

    let mut genesis_config = NodeConfig::testing_with_epochs(3);
    let consensus = genesis_config.consensus.get_mut();
    consensus.chunk_size = 32;
    consensus.block_tree_depth = 12;
    consensus.block_migration_depth = block_migration_depth;
    // Leave room for a block between rotations; step targets below make each fork
    // cross two boundaries without exhausting the shallow-fork block budget.
    consensus.vdf.reset_frequency = reset_frequency;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    let genesis = IrysNodeTest::new_genesis(genesis_config)
        .with_time_mode(TimeMode::Real)
        .start_and_wait_for_packing("GENESIS", SECONDS_TO_WAIT)
        .await;
    let peer_config = genesis.testing_peer_with_signer(&peer_signer);
    let peer = genesis
        .testing_peer_with_assignments_and_name(peer_config, "PEER")
        .await?;

    let fork_parent_height = genesis.get_canonical_chain_height().await;
    peer.wait_until_height(fork_parent_height, SECONDS_TO_WAIT)
        .await?;
    let fork_parent = genesis.get_block_by_height(fork_parent_height).await?;
    assert_eq!(
        fork_parent.block_hash,
        peer.get_block_by_height(fork_parent_height)
            .await?
            .block_hash,
        "both private forks must start from the same parent"
    );

    genesis.gossip_disable();
    peer.gossip_disable();

    let mut genesis_fork = Vec::new();
    let mut peer_fork = Vec::new();
    let max_balanced_fork_blocks = block_migration_depth as usize - 2;
    let reset_frequency_u64 = reset_frequency as u64;

    let diverged_boundary = loop {
        if let Some(diverged) = find_diverged_reset_boundary(
            fork_parent.vdf_limiter_info.global_step_number,
            &genesis_fork,
            &peer_fork,
            reset_frequency_u64,
        ) {
            break diverged;
        }

        eyre::ensure!(
            genesis_fork.len() < max_balanced_fork_blocks,
            "private forks did not produce a shared reset boundary with different seeds after {} blocks per side; genesis [{}], peer [{}]",
            genesis_fork.len(),
            fork_seed_summary(&genesis_fork),
            fork_seed_summary(&peer_fork),
        );

        // After the first private block, cross the next shared boundary on both
        // sides. Repeating this rotates each fork's own block hash into its seed.
        let target_step = genesis_fork
            .last()
            .zip(peer_fork.last())
            .map(|(left, right)| {
                let last_step = left
                    .vdf_limiter_info
                    .global_step_number
                    .max(right.vdf_limiter_info.global_step_number);
                (last_step / reset_frequency_u64 + 1) * reset_frequency_u64 + 1
            })
            .unwrap_or(0);
        let next_height = fork_parent_height + genesis_fork.len() as u64 + 1;
        let (genesis_block, peer_block) = tokio::try_join!(
            mine_private_block_toward_step(&genesis, next_height, target_step),
            mine_private_block_toward_step(&peer, next_height, target_step),
        )?;
        genesis_fork.push(genesis_block);
        peer_fork.push(peer_block);
    };

    tracing::info!(
        boundary = diverged_boundary.0,
        genesis_seed = %diverged_boundary.1,
        peer_seed = %diverged_boundary.2,
        blocks_per_side = genesis_fork.len(),
        "private forks carry different seeds at the same reset boundary"
    );
    // Genesis mined across the boundary on its own fork, so its buffer folded the seed that
    // fork carries. The reorg below makes that seed non-canonical; the heal must replace it.
    assert_eq!(
        genesis
            .node_ctx
            .vdf_steps_guard
            .read()
            .reset_seed_at(diverged_boundary.0),
        Some(diverged_boundary.1),
        "before the reorg, genesis must have folded its own fork's seed at the boundary"
    );

    let canonical_cumulative_diff = genesis_fork
        .last()
        .expect("the seed precondition requires a non-empty fork")
        .cumulative_diff;
    while peer_fork
        .last()
        .expect("the seed precondition requires a non-empty fork")
        .cumulative_diff
        <= canonical_cumulative_diff
    {
        eyre::ensure!(
            peer_fork.len() + 1 < block_migration_depth as usize,
            "peer fork could not become strictly heavier while remaining shallower than migration depth {} (genesis diff {}, peer diff {})",
            block_migration_depth,
            canonical_cumulative_diff,
            peer_fork
                .last()
                .expect("non-empty peer fork")
                .cumulative_diff,
        );
        let next_height = fork_parent_height + peer_fork.len() as u64 + 1;
        peer_fork.push(mine_private_block(&peer, next_height).await?);
    }

    eyre::ensure!(
        genesis_fork.len() < block_migration_depth as usize
            && peer_fork.len() < block_migration_depth as usize,
        "both fork suffixes must stay shallower than migration depth {} (genesis {}, peer {})",
        block_migration_depth,
        genesis_fork.len(),
        peer_fork.len(),
    );

    let peer_tip = peer_fork.last().expect("non-empty peer fork").clone();
    let reorg_future = genesis.wait_for_reorg(SECONDS_TO_WAIT);
    genesis.gossip_enable();
    peer.gossip_enable();
    for block in &peer_fork {
        let block = std::sync::Arc::new(block.clone());
        peer.gossip_block_to_peers(&block)?;
        genesis
            .wait_for_block(&block.block_hash, SECONDS_TO_WAIT)
            .await?;
    }

    let reorg = reorg_future.await?;
    assert_eq!(
        reorg.fork_parent.block_hash, fork_parent.block_hash,
        "the reorg must replace the private suffix from the intended fork point"
    );
    assert!(
        reorg.old_fork.len() < block_migration_depth as usize,
        "the genesis reorg depth {} must stay below migration depth {}",
        reorg.old_fork.len(),
        block_migration_depth
    );
    assert_eq!(
        reorg.new_tip, peer_tip.block_hash,
        "genesis must adopt the strictly heavier peer fork"
    );

    genesis
        .wait_until_height(peer_tip.height, SECONDS_TO_WAIT)
        .await?;
    let canonical_suffix = genesis
        .get_blocks(fork_parent_height + 1, peer_tip.height)
        .await?;
    let checked_blocks = wait_for_canonical_vdf_buffer(
        &genesis,
        &canonical_suffix,
        diverged_boundary.0,
        diverged_boundary.2,
        reset_frequency_u64,
        Duration::from_secs(SECONDS_TO_WAIT as u64),
    )
    .await?;
    assert!(
        checked_blocks > 0,
        "at least one canonical block above the fork point must be inside the VDF buffer"
    );

    let genesis_extension = genesis.mine_block().await?;
    peer.wait_until_height(genesis_extension.height, SECONDS_TO_WAIT)
        .await?;
    assert_eq!(
        peer.get_block_by_height(genesis_extension.height)
            .await?
            .block_hash,
        genesis_extension.block_hash,
        "peer must accept the block genesis mines after healing"
    );

    let peer_extension = peer.mine_block().await?;
    genesis
        .wait_until_height(peer_extension.height, SECONDS_TO_WAIT)
        .await?;
    assert_eq!(
        genesis
            .get_block_by_height(peer_extension.height)
            .await?
            .block_hash,
        peer_extension.block_hash,
        "genesis must accept the block peer mines after healing"
    );

    // Full sync serves migrated history. Confirm the divergent block and both
    // healed extensions before starting the fresh peer.
    genesis
        .mine_blocks(block_migration_depth as usize + 1)
        .await?;
    let tip_height = genesis.get_canonical_chain_height().await;
    let sync_height = tip_height - block_migration_depth as u64;
    assert!(sync_height > peer_extension.height);
    genesis
        .wait_until_block_index_height(sync_height, SECONDS_TO_WAIT)
        .await?;
    let mut sync_config = genesis.testing_peer();
    sync_config.node_mode = NodeMode::Miner;
    sync_config.consensus.get_mut().expected_genesis_hash = Some(genesis.node_ctx.genesis_hash);
    sync_config.sync_mode = SyncMode::Full;
    let fresh_peer = IrysNodeTest::new(sync_config)
        .with_time_mode(TimeMode::Real)
        .start_with_name("FRESH_PEER")
        .await;
    fresh_peer
        .wait_until_height(sync_height, SECONDS_TO_WAIT * 3)
        .await?;
    // Older blocks have left the tree after mining the confirmation suffix.
    // Compare indexed history as well as the fresh peer's unconfirmed tree tail.
    for height in 0..=sync_height {
        let expected = genesis.get_block_by_height_from_index(height, false)?;
        let actual = match fresh_peer.get_block_by_height(height).await {
            Ok(block) => block,
            Err(_) => fresh_peer.get_block_by_height_from_index(height, false)?,
        };
        assert_eq!(
            actual.block_hash, expected.block_hash,
            "fresh peer differs at height {height}"
        );
    }

    fresh_peer.stop().await;
    peer.stop().await;
    genesis.stop().await;
    Ok(())
}

async fn mine_private_block_toward_step(
    node: &IrysNodeTest<IrysNodeCtx>,
    height: u64,
    target_step: u64,
) -> eyre::Result<IrysBlockHeader> {
    node.node_ctx.start_vdf();
    let deadline = Instant::now() + Duration::from_secs(SECONDS_TO_WAIT as u64);
    while node.node_ctx.vdf_steps_guard.current_step() < target_step {
        eyre::ensure!(
            Instant::now() < deadline,
            "VDF did not reach target {target_step}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    node.node_ctx.stop_vdf();
    mine_private_block(node, height).await
}

async fn mine_private_block(
    node: &IrysNodeTest<IrysNodeCtx>,
    height: u64,
) -> eyre::Result<IrysBlockHeader> {
    node.mine_blocks_without_gossip(1).await?;
    node.get_block_by_height(height).await
}

fn find_diverged_reset_boundary(
    fork_parent_step: u64,
    genesis_fork: &[IrysBlockHeader],
    peer_fork: &[IrysBlockHeader],
    reset_frequency: u64,
) -> Option<(u64, H256, H256)> {
    let last_shared_step = genesis_fork
        .last()?
        .vdf_limiter_info
        .global_step_number
        .min(peer_fork.last()?.vdf_limiter_info.global_step_number);
    let first_boundary = fork_parent_step
        .div_ceil(reset_frequency)
        .saturating_mul(reset_frequency);
    let last_boundary = last_shared_step.saturating_sub(1);

    (first_boundary..=last_boundary)
        .step_by(usize::try_from(reset_frequency).ok()?)
        .find_map(|boundary| {
            let next_step = boundary + 1;
            let genesis_block = block_holding_step(genesis_fork, next_step)?;
            let peer_block = block_holding_step(peer_fork, next_step)?;
            let genesis_seed = genesis_block.vdf_limiter_info.seed;
            let peer_seed = peer_block.vdf_limiter_info.seed;
            (genesis_seed != peer_seed).then_some((boundary, genesis_seed, peer_seed))
        })
}

fn block_holding_step(blocks: &[IrysBlockHeader], step: u64) -> Option<&IrysBlockHeader> {
    blocks.iter().find(|block| {
        block.vdf_limiter_info.first_step_number() <= step
            && step <= block.vdf_limiter_info.global_step_number
    })
}

fn fork_seed_summary(blocks: &[IrysBlockHeader]) -> String {
    blocks
        .iter()
        .map(|block| {
            format!(
                "h{}:{}..={} seed={}",
                block.height,
                block.vdf_limiter_info.first_step_number(),
                block.vdf_limiter_info.global_step_number,
                block.vdf_limiter_info.seed
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

async fn wait_for_canonical_vdf_buffer(
    node: &IrysNodeTest<IrysNodeCtx>,
    canonical_blocks: &[IrysBlockHeader],
    diverged_boundary: u64,
    canonical_seed: H256,
    reset_frequency: u64,
    timeout: Duration,
) -> eyre::Result<usize> {
    let deadline = Instant::now() + timeout;

    loop {
        let (all_hold, checked_blocks, status) = {
            let state = node.node_ctx.vdf_steps_guard.read();
            let first_buffered_step = state.first_buffered_step();
            let current_step = state.current_step();
            let mut checked_blocks = 0_usize;
            let mut mismatch = None;

            for block in canonical_blocks {
                let info = &block.vdf_limiter_info;
                let predecessor_step = info.first_step_number().saturating_sub(1);
                if predecessor_step < first_buffered_step || info.global_step_number > current_step
                {
                    continue;
                }

                checked_blocks += 1;
                match state.holds_block_steps(
                    info.prev_output,
                    info.first_step_number(),
                    &info.steps.0,
                    info.seed,
                    reset_frequency,
                ) {
                    Ok(true) => {}
                    Ok(false) => {
                        mismatch = Some(format!(
                            "canonical block {} at height {} does not match the buffered lineage",
                            block.block_hash, block.height
                        ));
                        break;
                    }
                    Err(err) => {
                        mismatch = Some(format!(
                            "canonical block {} at height {} could not be checked: {err:#}",
                            block.block_hash, block.height
                        ));
                        break;
                    }
                }
            }

            let boundary_seed = state.reset_seed_at(diverged_boundary);
            if boundary_seed != Some(canonical_seed) {
                mismatch = Some(format!(
                    "reset boundary {diverged_boundary} records {boundary_seed:?}, expected {canonical_seed}"
                ));
            }
            let all_hold = checked_blocks > 0 && mismatch.is_none();
            let status = mismatch.unwrap_or_else(|| {
                format!(
                    "checked {checked_blocks} blocks in buffer {first_buffered_step}..={current_step}"
                )
            });
            (all_hold, checked_blocks, status)
        };

        if all_hold {
            return Ok(checked_blocks);
        }
        if Instant::now() >= deadline {
            eyre::bail!(
                "VDF buffer did not converge to the canonical fork within {timeout:?}: {status}"
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
