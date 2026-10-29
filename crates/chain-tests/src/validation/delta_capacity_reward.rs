//! After Delta, the block reward follows the PoA kind: a data PoA earns the
//! full reward-curve amount and an entropy (capacity) PoA earns the configured
//! fraction. The rest goes to the treasury and counts as emitted. The kind is
//! not tied to the partition kind: a data-assigned partition mines an entropy
//! PoA at an unwritten offset.

use crate::utils::{
    BlockValidationOutcome, IrysNodeTest, TimeMode, read_block_from_state, solution_context,
    wait_supply_height,
};
use irys_actors::block_discovery::{BlockDiscoveryFacade as _, BlockDiscoveryFacadeImpl};
use irys_actors::block_reward::BlockRewardSplit;
use irys_actors::block_validation::{PreValidationError, ValidationError, prevalidate_block};
use irys_actors::{
    BlockProdStrategy, BlockProducerInner, ProductionStrategy, async_trait,
    reth_ethereum_primitives,
};
use irys_chain::IrysNodeCtx;
use irys_domain::EmaSnapshot;
use irys_types::hardfork_config::Delta;
use irys_types::storage_pricing::{Amount, PRECISION_SCALE, mul_div, phantoms::Irys};
use irys_types::{
    AdjustmentStats, H256, IrysBlockHeader, NodeConfig, SealedBlock, U256, UnixTimestamp,
    UnixTimestampMs, block_production::SolutionContext,
};
use reth::{core::primitives::SealedBlock as RethSealedBlock, payload::EthBuiltPayload};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn delta_from_genesis_config() -> NodeConfig {
    NodeConfig::testing().with_consensus(|consensus| {
        consensus.chunk_size = 32;
        consensus.hardforks.delta = Some(Delta {
            activation_timestamp: UnixTimestamp::from_secs(1),
            initial_slots_per_new_ledger: Delta::default_initial_slots_per_new_ledger(),
            capacity_chunk_reward_percent: Delta::default_capacity_chunk_reward_percent(),
        });
    })
}

fn full_reward(node: &IrysNodeTest<IrysNodeCtx>, parent_height: u64) -> eyre::Result<U256> {
    let block_time = node
        .node_ctx
        .config
        .consensus
        .difficulty_adjustment
        .block_time;
    let from = u128::from(parent_height * block_time);
    let to = from + u128::from(block_time);
    Ok(node.node_ctx.reward_curve.reward_between(from, to)?.amount)
}

fn capacity_reward(node: &IrysNodeTest<IrysNodeCtx>, full: U256) -> eyre::Result<U256> {
    let delta = node
        .node_ctx
        .config
        .consensus
        .hardforks
        .delta
        .ok_or_else(|| eyre::eyre!("delta hardfork must be configured"))?;
    mul_div(
        full,
        delta.capacity_chunk_reward_percent.amount,
        PRECISION_SCALE,
    )
}

/// Every post-Delta block pays by PoA kind, including entropy PoAs mined by a
/// partition that is assigned to a data ledger.
#[test_log::test(tokio::test)]
async fn heavy_delta_reward_follows_poa_kind() -> eyre::Result<()> {
    let mut config = delta_from_genesis_config();
    let signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&signer]);
    let node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("GENESIS", 30)
        .await;

    // Write half of each data partition so it holds both written and
    // unwritten offsets: the miner then finds both data and entropy PoAs there.
    let chunk_size = node.node_ctx.config.consensus.chunk_size;
    let half_partition = node.node_ctx.config.consensus.num_chunks_in_partition / 2;
    let data = vec![7_u8; usize::try_from(chunk_size * half_partition)?];
    let tx = node.post_publish_data_tx(&signer, data).await?;
    node.wait_for_mempool(tx.header.id, 20).await?;
    node.upload_chunks(&tx).await?;

    let data_partitions: Vec<H256> = {
        let modules = node.node_ctx.storage_modules_guard.read();
        modules
            .iter()
            .filter_map(|module| module.partition_assignment())
            .filter(|assignment| assignment.ledger_id.is_some())
            .map(|assignment| assignment.partition_hash)
            .collect()
    };
    eyre::ensure!(!data_partitions.is_empty(), "no data partition assigned");

    // Which PoA kind a block gets is random, so mine until each kind is seen.
    let (mut data_poas, mut capacity_poas, mut data_partition_entropy_poas) = (0_u32, 0_u32, 0_u32);
    let mut mined = 0;
    while data_poas == 0 || data_partition_entropy_poas == 0 {
        eyre::ensure!(
            mined < 100,
            "after {mined} blocks: data_poas={data_poas}, data_partition_entropy_poas={data_partition_entropy_poas}"
        );
        mined += 1;
        let block = node.mine_block().await?;
        let full = full_reward(&node, block.height - 1)?;
        let expected = if block.poa.is_data_poa() {
            data_poas += 1;
            full
        } else {
            capacity_poas += 1;
            if data_partitions.contains(&block.poa.partition_hash) {
                data_partition_entropy_poas += 1;
            }
            capacity_reward(&node, full)?
        };
        eyre::ensure!(
            block.reward_amount == expected,
            "height {}: data_poa={} reward {} != expected {}",
            block.height,
            block.poa.is_data_poa(),
            block.reward_amount,
            expected
        );
    }
    tracing::info!(
        data_poas,
        capacity_poas,
        data_partition_entropy_poas,
        "delta reward by PoA kind"
    );

    node.stop().await;
    Ok(())
}

/// With no data or commitment txs, each block moves the treasury only by its
/// reward share. The miner gets the miner part, and the supply state counts
/// the full reward-curve amount as emitted.
#[test_log::test(tokio::test)]
async fn heavy_delta_capacity_reward_share_goes_to_treasury() -> eyre::Result<()> {
    let node = IrysNodeTest::new_genesis(delta_from_genesis_config())
        .start()
        .await;

    let mut parent = node.get_block_by_height(0).await?;
    for _ in 0..12 {
        let block = node.mine_block().await?;
        eyre::ensure!(
            block.get_data_ledger_tx_ids()?.values().all(Vec::is_empty)
                && block.system_ledgers.iter().all(|l| l.tx_ids.is_empty()),
            "height {}: expected a block with no txs",
            block.height
        );
        eyre::ensure!(
            !block.poa.is_data_poa(),
            "height {}: expected a capacity PoA on an empty chain",
            block.height
        );
        let full = full_reward(&node, parent.height)?;
        let miner = capacity_reward(&node, full)?;
        let treasury_share = full - miner;
        eyre::ensure!(treasury_share > U256::zero(), "treasury share is zero");

        eyre::ensure!(
            block.reward_amount == miner,
            "height {}: reward {} != miner part {}",
            block.height,
            block.reward_amount,
            miner
        );
        eyre::ensure!(
            block.treasury == parent.treasury + treasury_share,
            "height {}: treasury {} != parent {} + share {}",
            block.height,
            block.treasury,
            parent.treasury,
            treasury_share
        );
        let paid = node
            .get_balance(block.reward_address, block.evm_block_hash)
            .await
            - node
                .get_balance(block.reward_address, parent.evm_block_hash)
                .await;
        eyre::ensure!(
            paid == miner,
            "height {}: miner paid {} != miner part {}",
            block.height,
            paid,
            miner
        );
        parent = block;
    }

    // The index, and with it the supply state, trails the tip by the
    // migration depth.
    let migrated_height =
        parent.height - u64::from(node.node_ctx.config.consensus.block_migration_depth);
    let supply = wait_supply_height(&node, migrated_height).await?;
    let mut expected_emitted = U256::zero();
    for height in 1..=supply.height {
        expected_emitted += full_reward(&node, height - 1)?;
    }
    eyre::ensure!(
        supply.cumulative_emitted == expected_emitted,
        "supply at height {}: emitted {} != sum of full rewards {}",
        supply.height,
        supply.cumulative_emitted,
        expected_emitted
    );

    node.stop().await;
    Ok(())
}

/// Pays the miner part but leaves the treasury share out of the treasury.
struct NoTreasuryShareStrategy {
    prod: ProductionStrategy,
}

#[async_trait::async_trait]
impl BlockProdStrategy for NoTreasuryShareStrategy {
    fn inner(&self) -> &BlockProducerInner {
        &self.prod.inner
    }

    async fn create_evm_block(
        &self,
        prev_block_header: &IrysBlockHeader,
        prev_evm_block: &reth_ethereum_primitives::Block,
        mempool: &irys_actors::block_producer::MempoolTxsBundle,
        block_reward: BlockRewardSplit,
        timestamp_ms: UnixTimestampMs,
        solution_hash: H256,
        reward_address: irys_types::IrysAddress,
    ) -> Result<(EthBuiltPayload, U256), irys_actors::block_producer::BlockProductionError> {
        self.prod
            .create_evm_block(
                prev_block_header,
                prev_evm_block,
                mempool,
                BlockRewardSplit {
                    treasury: Amount::new(U256::zero()),
                    ..block_reward
                },
                timestamp_ms,
                solution_hash,
                reward_address,
            )
            .await
    }
}

/// A capacity-PoA block whose treasury leaves out the reward share fails
/// shadow-tx validation with a treasury mismatch.
#[test_log::test(tokio::test)]
async fn heavy_delta_capacity_poa_missing_treasury_share_rejected() -> eyre::Result<()> {
    let node = IrysNodeTest::new_genesis(delta_from_genesis_config())
        .start()
        .await;
    node.gossip_disable();

    let strategy = NoTreasuryShareStrategy {
        prod: ProductionStrategy {
            inner: node.node_ctx.block_producer_inner.clone(),
        },
    };
    let (block, _, _) = strategy
        .fully_produce_new_block_without_gossip(&solution_context(&node.node_ctx).await?)
        .await?
        .ok_or_else(|| eyre::eyre!("no block produced"))?;
    let header = block.header();
    eyre::ensure!(
        !header.poa.is_data_poa(),
        "expected a capacity PoA on an empty chain"
    );
    let parent = node.get_block_by_height(header.height - 1).await?;
    let full = full_reward(&node, parent.height)?;
    let miner = capacity_reward(&node, full)?;
    eyre::ensure!(full > miner, "treasury share is zero");
    eyre::ensure!(header.reward_amount == miner);
    eyre::ensure!(header.treasury == parent.treasury);

    let event_rx = node
        .node_ctx
        .service_senders
        .subscribe_block_state_updates();
    BlockDiscoveryFacadeImpl::new(node.node_ctx.service_senders.block_discovery.clone())
        .handle_block(Arc::clone(&block), false)
        .await?;
    let outcome = read_block_from_state(&node.node_ctx, &header.block_hash, event_rx).await;
    assert!(
        matches!(
            &outcome,
            BlockValidationOutcome::Discarded(ValidationError::ShadowTransactionInvalid(msg))
                if msg.contains("Treasury mismatch")
        ),
        "expected a treasury mismatch, got {outcome:?}"
    );

    node.stop().await;
    Ok(())
}

/// Pays the full reward on both the Irys header and the EVM coinbase.
struct FullRewardStrategy {
    prod: ProductionStrategy,
}

#[async_trait::async_trait]
impl BlockProdStrategy for FullRewardStrategy {
    fn inner(&self) -> &BlockProducerInner {
        &self.prod.inner
    }

    async fn create_evm_block(
        &self,
        prev_block_header: &IrysBlockHeader,
        prev_evm_block: &reth_ethereum_primitives::Block,
        mempool: &irys_actors::block_producer::MempoolTxsBundle,
        _block_reward: BlockRewardSplit,
        timestamp_ms: UnixTimestampMs,
        solution_hash: H256,
        reward_address: irys_types::IrysAddress,
    ) -> Result<(EthBuiltPayload, U256), irys_actors::block_producer::BlockProductionError> {
        let full = BlockRewardSplit {
            miner: self
                .prod
                .block_reward(prev_block_header)
                .map_err(irys_actors::block_producer::BlockProductionError::irrecoverable)?,
            treasury: Amount::new(U256::zero()),
        };
        self.prod
            .create_evm_block(
                prev_block_header,
                prev_evm_block,
                mempool,
                full,
                timestamp_ms,
                solution_hash,
                reward_address,
            )
            .await
    }

    async fn produce_block_without_broadcasting(
        &self,
        solution: &SolutionContext,
        prev_block_header: &IrysBlockHeader,
        mempool_bundle: irys_actors::block_producer::MempoolTxsBundle,
        current_timestamp: UnixTimestampMs,
        _adjusted_reward: Amount<Irys>,
        eth_built_payload: &RethSealedBlock<reth_ethereum_primitives::Block>,
        prev_block_ema_snapshot: &EmaSnapshot,
        treasury: U256,
        reward_address: irys_types::IrysAddress,
    ) -> Result<
        Option<(Arc<SealedBlock>, Option<AdjustmentStats>)>,
        irys_actors::block_producer::BlockProductionError,
    > {
        let full = self
            .prod
            .block_reward(prev_block_header)
            .map_err(irys_actors::block_producer::BlockProductionError::irrecoverable)?;
        self.prod
            .produce_block_without_broadcasting(
                solution,
                prev_block_header,
                mempool_bundle,
                current_timestamp,
                full,
                eth_built_payload,
                prev_block_ema_snapshot,
                treasury,
                reward_address,
            )
            .await
    }
}

/// A capacity-PoA block that claims the full reward fails prevalidation.
#[test_log::test(tokio::test)]
async fn heavy_delta_capacity_poa_full_reward_rejected() -> eyre::Result<()> {
    let node = IrysNodeTest::new_genesis(delta_from_genesis_config())
        .start()
        .await;
    node.gossip_disable();

    let strategy = FullRewardStrategy {
        prod: ProductionStrategy {
            inner: node.node_ctx.block_producer_inner.clone(),
        },
    };
    let (block, _, _) = strategy
        .fully_produce_new_block_without_gossip(&solution_context(&node.node_ctx).await?)
        .await?
        .ok_or_else(|| eyre::eyre!("no block produced"))?;
    eyre::ensure!(
        !block.header().poa.is_data_poa(),
        "expected a capacity PoA on an empty chain"
    );
    let full = full_reward(&node, block.header().height - 1)?;
    eyre::ensure!(block.header().reward_amount == full && full > U256::zero());

    let parent = node.get_block_by_height(block.header().height - 1).await?;
    let (parent_epoch_snapshot, parent_ema_snapshot) = {
        let read = node.node_ctx.block_tree_guard.read();
        (
            read.get_epoch_snapshot(&parent.block_hash)
                .ok_or_else(|| eyre::eyre!("parent epoch snapshot"))?,
            read.get_ema_snapshot(&parent.block_hash)
                .ok_or_else(|| eyre::eyre!("parent ema snapshot"))?,
        )
    };
    let pool = Arc::new(irys_vdf::build_verification_pool(&node.node_ctx.config.vdf));
    let result = prevalidate_block(
        &block,
        &parent,
        parent_epoch_snapshot,
        node.node_ctx.config.clone(),
        pool,
        node.node_ctx.reward_curve.clone(),
        &parent_ema_snapshot,
    )
    .await;

    let expected = capacity_reward(&node, full)?;
    assert!(
        matches!(
            &result,
            Err(PreValidationError::RewardMismatch { got, expected: want })
                if *got == full && *want == expected
        ),
        "expected RewardMismatch, got {result:?}"
    );

    node.stop().await;
    Ok(())
}

/// Full reward-curve amount of every block in `1..=end_height`.
fn sum_full_rewards(node: &IrysNodeTest<IrysNodeCtx>, end_height: u64) -> eyre::Result<U256> {
    let mut sum = U256::zero();
    for h in 1..=end_height {
        sum += full_reward(node, h - 1)?;
    }
    Ok(sum)
}

/// Miner reward declared on the headers in `1..=end_height`. On a Delta chain
/// this is below the full emission for capacity PoA blocks.
async fn header_reward_sum(
    node: &IrysNodeTest<IrysNodeCtx>,
    end_height: u64,
) -> eyre::Result<U256> {
    let mut sum = U256::zero();
    for h in 1..=end_height {
        sum += node.get_block_by_height(h).await?.reward_amount;
    }
    Ok(sum)
}

/// Mid-chain activation: capacity PoA blocks mined before the activation
/// timestamp earn the full reward; from the first block whose timestamp meets
/// the activation they earn the configured fraction, and the treasury gain
/// starts on exactly that block.
#[test_log::test(tokio::test)]
async fn heavy_delta_midchain_activation_boundary() -> eyre::Result<()> {
    // The accelerated test clock is anchored to real time, so the block
    // timestamps cross the boundary after a few real seconds. At the 1s test
    // block time, 100 seconds gives many pre-activation blocks. Real time
    // would need 100+ real seconds, so the time mode is pinned.
    let activation = UnixTimestamp::now()?.as_secs() + 100;
    let config = NodeConfig::testing().with_consensus(|consensus| {
        consensus.chunk_size = 32;
        consensus.hardforks.delta = Some(Delta {
            activation_timestamp: UnixTimestamp::from_secs(activation),
            initial_slots_per_new_ledger: Delta::default_initial_slots_per_new_ledger(),
            capacity_chunk_reward_percent: Delta::default_capacity_chunk_reward_percent(),
        });
    });
    let node = IrysNodeTest::new_genesis(config)
        .with_time_mode(TimeMode::Accelerated)
        .start()
        .await;

    let wall_start = Instant::now();
    let (mut pre, mut post) = (0_u32, 0_u32);
    let mut boundary_checked = false;
    loop {
        eyre::ensure!(
            wall_start.elapsed() < Duration::from_secs(180),
            "did not cross the activation timestamp"
        );
        let block = node.mine_block().await?;
        let full = full_reward(&node, block.height - 1)?;
        if block.timestamp_secs() < UnixTimestamp::from_secs(activation) {
            pre += 1;
            eyre::ensure!(
                block.reward_amount == full,
                "height {}: pre-activation reward {} != full {}",
                block.height,
                block.reward_amount,
                full
            );
        } else {
            eyre::ensure!(
                pre >= 5,
                "only {pre} blocks before the activation timestamp, need 5"
            );
            post += 1;
            let expected = capacity_reward(&node, full)?;
            eyre::ensure!(
                block.reward_amount == expected,
                "height {}: post-activation reward {} != expected {}",
                block.height,
                block.reward_amount,
                expected
            );
            if !boundary_checked {
                let parent = node.get_block_by_height(block.height - 1).await?;
                eyre::ensure!(
                    parent.timestamp_secs() < UnixTimestamp::from_secs(activation),
                    "boundary block {} has a post-activation parent",
                    block.height
                );
                eyre::ensure!(
                    block.treasury == parent.treasury + (full - expected),
                    "height {}: treasury {} != parent {} + share {}",
                    block.height,
                    block.treasury,
                    parent.treasury,
                    full - expected
                );
                boundary_checked = true;
            }
        }
        if post >= 3 {
            break;
        }
    }
    tracing::info!(pre, post, "midchain Delta activation boundary crossed");

    node.stop().await;
    Ok(())
}

/// Restarting a Delta-active node rebuilds the supply state from the reward
/// curve, not from the header rewards: every migrated block counts its full
/// curve amount as emitted, even though its capacity PoA miner earned only a
/// fraction.
#[test_log::test(tokio::test)]
async fn heavy_delta_backfill_rebuilds_supply_from_curve() -> eyre::Result<()> {
    let block_migration_depth: u64 = 1;
    let config = delta_from_genesis_config().with_consensus(|consensus| {
        consensus.block_migration_depth = block_migration_depth as u32;
    });
    let node = IrysNodeTest::new_genesis(config).start().await;

    let mined = 10_u64;
    for _ in 0..mined {
        node.mine_block().await?;
    }
    let index_height = mined - 1;
    node.wait_until_block_index_height(index_height, 30).await?;

    let supply = wait_supply_height(&node, index_height).await?;
    let expected = sum_full_rewards(&node, supply.height)?;
    eyre::ensure!(
        supply.cumulative_emitted == expected,
        "supply at height {}: emitted {} != sum of full rewards {}",
        supply.height,
        supply.cumulative_emitted,
        expected
    );
    let reward_sum = header_reward_sum(&node, supply.height).await?;
    eyre::ensure!(
        reward_sum < expected,
        "header rewards {reward_sum} should be below the full emission {expected} on a Delta chain"
    );
    tracing::info!(?supply, reward_sum = %reward_sum, "live supply state counts full emission");

    // Restart on the same data directory: the backfill recomputes the
    // historical sum from the curve over the already-migrated blocks. The
    // tree tip resets to the last migrated height on restart, and the index
    // always trails the tip by block_migration_depth, so 3 mined blocks
    // advance the index by 3 - depth.
    let node = node.stop().await.start().await;
    for _ in 0..3 {
        node.mine_block().await?;
    }
    let final_index_height = index_height + 3 - block_migration_depth;
    node.wait_until_block_index_height(final_index_height, 30)
        .await?;

    let supply = wait_supply_height(&node, final_index_height).await?;
    let expected = sum_full_rewards(&node, supply.height)?;
    eyre::ensure!(
        supply.cumulative_emitted == expected,
        "supply at height {} after restart: emitted {} != sum of full rewards {}",
        supply.height,
        supply.cumulative_emitted,
        expected
    );
    tracing::info!(?supply, "restarted supply state rebuilt from the curve");

    node.stop().await;
    Ok(())
}
