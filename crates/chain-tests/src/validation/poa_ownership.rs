//! PoA partition ownership.
//!
//! A block may prove only a partition the parent epoch snapshot assigns to
//! its miner. The producer drops a solution that fails the same check.

use std::{collections::BTreeSet, sync::Arc};

use crate::utils::{
    BlockValidationOutcome, IrysNodeTest, read_block_from_state, solution_context,
    solution_context_for_miner_partition, solution_context_for_partition,
};
use irys_actors::{
    BlockProdStrategy, BlockProducerInner, BlockProductionError, ProductionStrategy, async_trait,
    block_discovery::{BlockDiscoveryError, BlockDiscoveryFacade as _, BlockDiscoveryFacadeImpl},
    block_producer::current_timestamp,
    block_reward::split_block_reward,
    block_validation::{PreValidationError, prevalidate_block},
};
use irys_chain::IrysNodeCtx;
use irys_domain::{EmaSnapshot, EpochSnapshot};
use irys_types::{
    AdjustmentStats, Base64, CommitmentTransaction, DataLedger, H256, IrysAddress, IrysBlockHeader,
    NodeConfig, SealedBlock, UnixTimestamp, block_production::SolutionContext,
    hardfork_config::Echo, partition::PartitionAssignment,
};
use reth::payload::EthBuiltPayload;

fn start_config(num_blocks_in_epoch: usize) -> NodeConfig {
    let mut config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    config.consensus.get_mut().chunk_size = 32;
    config
}

async fn start_node(config: NodeConfig) -> IrysNodeTest<IrysNodeCtx> {
    IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("GENESIS", 20)
        .await
}

fn producer(node: &IrysNodeTest<IrysNodeCtx>) -> ProductionStrategy {
    ProductionStrategy {
        inner: node.node_ctx.block_producer_inner.clone(),
    }
}

fn discovery(node: &IrysNodeTest<IrysNodeCtx>) -> BlockDiscoveryFacadeImpl {
    BlockDiscoveryFacadeImpl::new(node.node_ctx.service_senders.block_discovery.clone())
}

fn reseal(block: &SealedBlock, header: IrysBlockHeader) -> eyre::Result<Arc<SealedBlock>> {
    let mut body = block.to_block_body();
    body.block_hash = header.block_hash;
    Ok(Arc::new(SealedBlock::new(header, body)?))
}

async fn produce_block(node: &IrysNodeTest<IrysNodeCtx>) -> eyre::Result<Arc<SealedBlock>> {
    let solution = solution_context(&node.node_ctx).await?;
    let prod = producer(node);
    match prod.fully_produce_new_block_without_gossip(&solution).await {
        Ok(Some((block, _stats, _payload))) => Ok(block),
        Ok(None) => eyre::bail!("owned solution must produce a block"),
        Err(err) => eyre::bail!("block production failed: {err:?}"),
    }
}

fn miner_partitions(node: &IrysNodeTest<IrysNodeCtx>, block_hash: &H256) -> BTreeSet<H256> {
    let miner = node.node_ctx.config.node_config.miner_address();
    let tree = node.node_ctx.block_tree_guard.read();
    let snapshot = tree
        .get_epoch_snapshot(block_hash)
        .unwrap_or_else(|| panic!("epoch snapshot missing for {block_hash}"));
    snapshot
        .get_partition_assignments(miner)
        .into_iter()
        .map(|assignment| assignment.partition_hash)
        .collect()
}

fn assert_not_owned(
    result: &Result<(), BlockDiscoveryError>,
    partition_hash: H256,
    miner: IrysAddress,
    owner: Option<IrysAddress>,
) {
    assert!(
        matches!(
            result,
            Err(BlockDiscoveryError::BlockValidationError(
                PreValidationError::PoAPartitionNotOwned {
                    partition_hash: got_hash,
                    miner: got_miner,
                    owner: got_owner,
                }
            )) if *got_hash == partition_hash && *got_miner == miner && *got_owner == owner
        ),
        "{result:?}"
    );
}

/// Re-signing with an unstaked key keeps the genesis partition and must fail
/// ownership. The canonical tip stays put, and a later honest block extends it.
#[test_log::test(tokio::test)]
async fn block_mined_by_unstaked_key_over_owned_partition_gets_rejected() -> eyre::Result<()> {
    let config = start_config(4);
    let unstaked = config.new_random_signer();
    let genesis_miner = config.miner_address();
    let node = start_node(config).await;
    node.mine_block().await?;

    let block = produce_block(&node).await?;
    let owned_partition = block.header().poa.partition_hash;
    let mut header = (**block.header()).clone();
    unstaked.sign_block_header(&mut header)?;
    let tampered = reseal(&block, header)?;

    let tip = node.get_max_difficulty_block();
    let facade = discovery(&node);
    let result = facade.handle_block(tampered, false).await;
    assert_not_owned(
        &result,
        owned_partition,
        unstaked.address(),
        Some(genesis_miner),
    );
    assert_eq!(
        node.get_max_difficulty_block().block_hash,
        tip.block_hash,
        "rejected block must not move the canonical tip"
    );

    let mined = node.mine_block().await?;
    assert_eq!(mined.previous_block_hash, tip.block_hash);
    assert_eq!(mined.height, tip.height + 1);

    node.stop().await;
    Ok(())
}

/// A zero partition hash is unassigned. Re-signing with the genesis key still
/// fails ownership, and the canonical tip stays put.
#[test_log::test(tokio::test)]
async fn block_over_unassigned_partition_gets_rejected() -> eyre::Result<()> {
    let config = start_config(4);
    let genesis_miner = config.miner_address();
    let signer = config.signer();
    let node = start_node(config).await;
    node.mine_block().await?;

    let block = produce_block(&node).await?;
    let mut header = (**block.header()).clone();
    header.poa.partition_hash = H256::zero();
    signer.sign_block_header(&mut header)?;
    let tampered = reseal(&block, header)?;

    let tip_hash = node.get_max_difficulty_block().block_hash;
    let facade = discovery(&node);
    let result = facade.handle_block(tampered, false).await;
    assert_not_owned(&result, H256::zero(), genesis_miner, None);
    assert_eq!(node.get_max_difficulty_block().block_hash, tip_hash);

    node.stop().await;
    Ok(())
}

/// Trusted sync skips `poa_is_valid`, so prevalidation alone must reject a data
/// PoA naming a ledger other than the one its partition is assigned to.
#[test_log::test(tokio::test)]
async fn data_poa_ledger_mismatch_rejected_when_poa_validation_skipped() -> eyre::Result<()> {
    let config = start_config(4);
    let genesis_miner = config.miner_address();
    let signer = config.signer();
    let node = start_node(config).await;
    node.mine_block().await?;

    let block = produce_block(&node).await?;
    let (data_partition, assigned_ledger) = {
        let tree = node.node_ctx.block_tree_guard.read();
        let snapshot = tree
            .get_epoch_snapshot(&block.header().previous_block_hash)
            .expect("parent epoch snapshot");
        let assignment = snapshot
            .partition_assignments
            .data_partitions
            .values()
            .find(|assignment| assignment.miner_address == genesis_miner)
            .expect("genesis owns a data partition");
        (
            assignment.partition_hash,
            assignment.ledger_id.expect("data partition has a ledger"),
        )
    };
    let other_ledger = if assigned_ledger == DataLedger::Publish as u32 {
        DataLedger::Submit as u32
    } else {
        DataLedger::Publish as u32
    };

    let mut header = (**block.header()).clone();
    header.poa.partition_hash = data_partition;
    header.poa.ledger_id = Some(other_ledger);
    header.poa.tx_path = Some(Base64(vec![]));
    header.poa.data_path = Some(Base64(vec![]));
    signer.sign_block_header(&mut header)?;
    let tampered = reseal(&block, header)?;

    let tip_hash = node.get_max_difficulty_block().block_hash;
    let skip_vdf = true;
    let result = discovery(&node).handle_block(tampered, skip_vdf).await;
    assert!(
        matches!(
            &result,
            Err(BlockDiscoveryError::BlockValidationError(
                PreValidationError::PoALedgerMismatch {
                    partition_hash,
                    poa_ledger_id,
                    assigned_ledger_id,
                }
            )) if *partition_hash == data_partition
                && *poa_ledger_id == other_ledger
                && *assigned_ledger_id == Some(assigned_ledger)
        ),
        "{result:?}"
    );
    assert_eq!(node.get_max_difficulty_block().block_hash, tip_hash);

    node.stop().await;
    Ok(())
}

async fn assert_skipped(node: &IrysNodeTest<IrysNodeCtx>, solution: &SolutionContext) {
    let prod = producer(node);
    match prod.fully_produce_new_block_without_gossip(solution).await {
        Ok(None) => {}
        Ok(Some(_)) => panic!("unowned partition must be skipped"),
        Err(err) => panic!("production error is not a skip: {err:?}"),
    }
}

/// A solution whose partition the parent snapshot does not assign to the miner
/// is discarded. The same solution, unchanged, still produces a block.
#[test_log::test(tokio::test)]
async fn producer_skips_solution_over_unowned_partition() -> eyre::Result<()> {
    let node = start_node(start_config(4)).await;
    node.mine_block().await?;
    let solution = solution_context(&node.node_ctx).await?;

    let mut unknown_partition = solution.clone();
    unknown_partition.partition_hash = H256::random();
    assert_skipped(&node, &unknown_partition).await;

    let mut unknown_miner = solution.clone();
    unknown_miner.mining_address = IrysAddress::random();
    assert_skipped(&node, &unknown_miner).await;

    let prod = producer(&node);
    match prod.fully_produce_new_block_without_gossip(&solution).await {
        Ok(Some(_)) => {}
        Ok(None) => eyre::bail!("owned solution must produce a block"),
        Err(err) => eyre::bail!("owned solution failed: {err:?}"),
    }

    node.stop().await;
    Ok(())
}

async fn mine_into_block(
    node: &IrysNodeTest<IrysNodeCtx>,
    commitment_id: H256,
) -> eyre::Result<IrysBlockHeader> {
    node.wait_for_mempool_commitment_txs(vec![commitment_id], 20)
        .await?;

    let mut inclusion = node.mine_block().await?;
    let mut mined = 1;
    while !inclusion.commitment_tx_ids().contains(&commitment_id) {
        eyre::ensure!(mined < 8, "commitment {commitment_id} was not included");
        inclusion = node.mine_block().await?;
        mined += 1;
    }
    Ok(inclusion)
}

async fn mine_pledge_into_block(node: &IrysNodeTest<IrysNodeCtx>) -> eyre::Result<IrysBlockHeader> {
    let pledge_tx = node.post_pledge_commitment(None).await?;
    mine_into_block(node, pledge_tx.id()).await
}

async fn mine_assigning_epoch(
    node: &IrysNodeTest<IrysNodeCtx>,
    inclusion_height: u64,
) -> eyre::Result<(IrysBlockHeader, IrysBlockHeader)> {
    let epoch_len = node.node_ctx.config.consensus.epoch.num_blocks_in_epoch;
    // `height % N == 0` yields a full epoch, so this distance is never zero.
    let blocks_after_inclusion = epoch_len - (inclusion_height % epoch_len);
    let gap_to_parent = blocks_after_inclusion - 1;
    if gap_to_parent > 0 {
        node.mine_blocks(gap_to_parent as usize).await?;
    }
    let parent = node.get_max_difficulty_block();
    let epoch_block = node.mine_block().await?;
    eyre::ensure!(
        epoch_block.height == parent.height + 1
            && epoch_block.height % epoch_len == 0
            && epoch_block.height == inclusion_height + blocks_after_inclusion,
        "assigning epoch {} must be the first epoch boundary after inclusion {inclusion_height}",
        epoch_block.height
    );
    Ok((parent, epoch_block))
}

fn new_partition_at_epoch(
    node: &IrysNodeTest<IrysNodeCtx>,
    parent_hash: &H256,
    epoch_hash: &H256,
) -> H256 {
    let before = miner_partitions(node, parent_hash);
    let after = miner_partitions(node, epoch_hash);
    let added: Vec<H256> = after.difference(&before).copied().collect();
    assert_eq!(
        added.len(),
        1,
        "one extra pledge must add one partition (before {}, after {})",
        before.len(),
        after.len()
    );
    let new_partition = added[0];
    let tree = node.node_ctx.block_tree_guard.read();
    let parent_snapshot = tree
        .get_epoch_snapshot(parent_hash)
        .unwrap_or_else(|| panic!("parent epoch snapshot missing for {parent_hash}"));
    assert!(
        parent_snapshot
            .partition_assignments
            .get_assignment(new_partition)
            .is_none(),
        "partition {new_partition} must be absent from the parent epoch snapshot"
    );
    new_partition
}

async fn produce_over_partition(
    node: &IrysNodeTest<IrysNodeCtx>,
    partition_hash: H256,
) -> eyre::Result<Arc<SealedBlock>> {
    let solution = solution_context_for_partition(&node.node_ctx, partition_hash).await?;
    let prod = producer(node);
    match prod.fully_produce_new_block_without_gossip(&solution).await {
        Ok(Some((block, _stats, _payload))) => Ok(block),
        Ok(None) => eyre::bail!("solution over {partition_hash} must produce a block"),
        Err(err) => eyre::bail!("block production failed: {err:?}"),
    }
}

/// The partition a pledge receives exists only on the assigning epoch snapshot.
/// A block whose parent is that epoch block may use it. The hash is absent from
/// the previous epoch snapshot, so a block with that parent could not.
#[test_log::test(tokio::test)]
async fn ownership_uses_parent_epoch_snapshot() -> eyre::Result<()> {
    let node = start_node(start_config(4)).await;
    let inclusion = mine_pledge_into_block(&node).await?;
    let (parent, epoch_block) = mine_assigning_epoch(&node, inclusion.height).await?;
    let new_partition = new_partition_at_epoch(&node, &parent.block_hash, &epoch_block.block_hash);

    let block = produce_over_partition(&node, new_partition).await?;
    assert_eq!(block.header().previous_block_hash, epoch_block.block_hash);
    assert_eq!(block.header().poa.partition_hash, new_partition);

    let event_rx = node
        .node_ctx
        .service_senders
        .subscribe_block_state_updates();
    let facade = discovery(&node);
    let result = facade.handle_block(Arc::clone(&block), false).await;
    assert!(matches!(&result, Ok(())), "{result:?}");

    let outcome = read_block_from_state(&node.node_ctx, &block.header().block_hash, event_rx).await;
    assert!(matches!(outcome, BlockValidationOutcome::StoredOnNode(_)));

    node.stop().await;
    Ok(())
}

/// Produces without the ownership skip, so a test can hand validators a block
/// over a partition the parent snapshot does not assign to the miner. The block
/// is a capacity (entropy) PoA, built as the default strategy builds it.
struct UnownedPartitionStrategy {
    prod: ProductionStrategy,
}

#[async_trait::async_trait]
impl BlockProdStrategy for UnownedPartitionStrategy {
    fn inner(&self) -> &BlockProducerInner {
        &self.prod.inner
    }

    async fn produce_block_with_parent(
        &self,
        solution: &SolutionContext,
        prev_block_header: IrysBlockHeader,
        prev_block_ema_snapshot: Arc<EmaSnapshot>,
    ) -> Result<
        Option<(Arc<SealedBlock>, Option<AdjustmentStats>, EthBuiltPayload)>,
        BlockProductionError,
    > {
        let prev_evm_block = self.get_evm_block(&prev_block_header).await?;
        let timestamp = current_timestamp(&prev_block_header, irys_types::global_clock()).await;
        let mempool_bundle = self.get_mempool_txs(&prev_block_header, timestamp).await?;
        let reward_address = self
            .inner()
            .block_tree_guard
            .read()
            .get_epoch_snapshot(&prev_block_header.block_hash)
            .expect("parent epoch snapshot")
            .resolve_reward_address(solution.mining_address);

        let Some((poa, _)) = self
            .create_poa_data(solution, None)
            .await
            .map_err(BlockProductionError::recoverable("poa_data"))?
        else {
            return Ok(None);
        };
        let block_reward = split_block_reward(
            self.block_reward(&prev_block_header)
                .map_err(BlockProductionError::irrecoverable)?,
            &self.inner().config.consensus,
            timestamp.to_secs(),
            !poa.is_data_poa(),
        )
        .map_err(BlockProductionError::irrecoverable)?;
        let (eth_built_payload, final_treasury) = self
            .create_evm_block(
                &prev_block_header,
                &prev_evm_block,
                &mempool_bundle,
                block_reward,
                timestamp,
                solution.solution_hash,
                reward_address,
            )
            .await?;
        let block_result = self
            .produce_block_without_broadcasting(
                solution,
                &prev_block_header,
                mempool_bundle,
                timestamp,
                block_reward.miner,
                eth_built_payload.block(),
                &prev_block_ema_snapshot,
                final_treasury,
                reward_address,
            )
            .await?;
        Ok(block_result.map(|(block, stats)| (block, stats, eth_built_payload)))
    }
}

fn unowned_strategy(node: &IrysNodeTest<IrysNodeCtx>) -> UnownedPartitionStrategy {
    UnownedPartitionStrategy {
        prod: producer(node),
    }
}

fn parent_snapshots(
    node: &IrysNodeTest<IrysNodeCtx>,
    parent_hash: &H256,
) -> (IrysBlockHeader, Arc<EpochSnapshot>, Arc<EmaSnapshot>) {
    let tree = node.node_ctx.block_tree_guard.read();
    (
        tree.get_block(parent_hash).cloned().expect("parent block"),
        tree.get_epoch_snapshot(parent_hash)
            .expect("parent epoch snapshot"),
        tree.get_ema_snapshot(parent_hash)
            .expect("parent ema snapshot"),
    )
}

async fn prevalidate(
    node: &IrysNodeTest<IrysNodeCtx>,
    block: &SealedBlock,
    parent: &IrysBlockHeader,
    parent_epoch_snapshot: Arc<EpochSnapshot>,
    parent_ema_snapshot: &EmaSnapshot,
) -> Result<(), PreValidationError> {
    let pool = Arc::new(irys_vdf::build_verification_pool(&node.node_ctx.config.vdf));
    prevalidate_block(
        block,
        parent,
        parent_epoch_snapshot,
        node.node_ctx.config.clone(),
        pool,
        node.node_ctx.reward_curve.clone(),
        parent_ema_snapshot,
    )
    .await
}

/// Only ownership rejects this block. The signer is an unstaked key with entropy
/// packed for its own address, a valid recall range, and the reward address
/// the snapshot resolves for it. Prevalidation accepts the same block once the
/// partition is assigned to that key, and rejects it with the real snapshot.
#[test_log::test(tokio::test)]
async fn ownership_is_the_only_rule_rejecting_unstaked_miner_over_invented_partition()
-> eyre::Result<()> {
    let config = start_config(4);
    let unstaked = config.new_random_signer();
    let node = start_node(config).await;
    node.mine_block().await?;

    let partition_hash = H256::random();
    let solution =
        solution_context_for_miner_partition(&node.node_ctx, unstaked.address(), partition_hash)
            .await?;
    let (block, _stats, _payload) = unowned_strategy(&node)
        .fully_produce_new_block_without_gossip(&solution)
        .await
        .map_err(|err| eyre::eyre!("block production failed: {err:?}"))?
        .ok_or_else(|| eyre::eyre!("evil producer must build the block"))?;
    let mut header = (**block.header()).clone();
    assert_eq!(header.poa.partition_hash, partition_hash);
    unstaked.sign_block_header(&mut header)?;
    assert_eq!(header.miner_address, unstaked.address());
    let block = reseal(&block, header)?;

    let (parent, parent_snapshot, parent_ema) =
        parent_snapshots(&node, &block.header().previous_block_hash);
    let result = prevalidate(&node, &block, &parent, parent_snapshot.clone(), &parent_ema).await;
    assert!(
        matches!(
            &result,
            Err(PreValidationError::PoAPartitionNotOwned { owner: None, .. })
        ),
        "{result:?}"
    );

    let mut assigned = (*parent_snapshot).clone();
    assigned.partition_assignments.capacity_partitions.insert(
        partition_hash,
        PartitionAssignment {
            partition_hash,
            miner_address: unstaked.address(),
            ledger_id: None,
            slot_index: None,
        },
    );
    let result = prevalidate(&node, &block, &parent, Arc::new(assigned), &parent_ema).await;
    assert!(
        matches!(&result, Ok(())),
        "block must pass every other prevalidation rule: {result:?}"
    );

    let tip = node.get_max_difficulty_block();
    let result = discovery(&node).handle_block(block, false).await;
    assert_not_owned(&result, partition_hash, unstaked.address(), None);
    assert_eq!(node.get_max_difficulty_block().block_hash, tip.block_hash);

    node.stop().await;
    Ok(())
}

/// Before Echo activates, prevalidation does not check ownership: the block that
/// only ownership rejects passes with the real parent snapshot.
#[test_log::test(tokio::test)]
async fn before_echo_unstaked_miner_over_invented_partition_passes_prevalidation()
-> eyre::Result<()> {
    let mut config = start_config(4);
    config.consensus.get_mut().hardforks.echo = Some(Echo {
        activation_timestamp: UnixTimestamp::from_secs(u64::MAX),
    });
    let unstaked = config.new_random_signer();
    let node = start_node(config).await;
    node.mine_block().await?;

    let partition_hash = H256::random();
    let solution =
        solution_context_for_miner_partition(&node.node_ctx, unstaked.address(), partition_hash)
            .await?;
    let (block, _stats, _payload) = unowned_strategy(&node)
        .fully_produce_new_block_without_gossip(&solution)
        .await
        .map_err(|err| eyre::eyre!("block production failed: {err:?}"))?
        .ok_or_else(|| eyre::eyre!("evil producer must build the block"))?;
    let mut header = (**block.header()).clone();
    unstaked.sign_block_header(&mut header)?;
    let block = reseal(&block, header)?;

    let (parent, parent_snapshot, parent_ema) =
        parent_snapshots(&node, &block.header().previous_block_hash);
    let result = prevalidate(&node, &block, &parent, parent_snapshot, &parent_ema).await;
    assert!(matches!(&result, Ok(())), "{result:?}");

    node.stop().await;
    Ok(())
}

/// The epoch block assigns the partition, but a block at the epoch height has the
/// pre-epoch block as parent and is checked against the pre-epoch snapshot. The
/// producer skips the solution there too.
#[test_log::test(tokio::test)]
async fn epoch_block_over_partition_first_assigned_at_that_epoch_gets_rejected() -> eyre::Result<()>
{
    let node = start_node(start_config(4)).await;
    let inclusion = mine_pledge_into_block(&node).await?;
    let (parent, epoch_block) = mine_assigning_epoch(&node, inclusion.height).await?;
    let new_partition = new_partition_at_epoch(&node, &parent.block_hash, &epoch_block.block_hash);
    let miner = node.node_ctx.config.node_config.miner_address();

    let (parent, _parent_snapshot, parent_ema) = parent_snapshots(&node, &parent.block_hash);
    let solution = solution_context_for_partition(&node.node_ctx, new_partition).await?;

    let default_result = producer(&node)
        .produce_block_with_parent(&solution, parent.clone(), parent_ema.clone())
        .await;
    assert!(
        matches!(&default_result, Ok(None)),
        "producer must skip: {:?}",
        default_result.as_ref().map(|_| ())
    );

    let (block, _stats, _payload) = unowned_strategy(&node)
        .produce_block_with_parent(&solution, parent, parent_ema)
        .await
        .map_err(|err| eyre::eyre!("block production failed: {err:?}"))?
        .ok_or_else(|| eyre::eyre!("evil producer must build the block"))?;
    assert_eq!(block.header().height, epoch_block.height);
    assert_eq!(
        block.header().previous_block_hash,
        epoch_block.previous_block_hash
    );

    let tip = node.get_max_difficulty_block();
    let result = discovery(&node).handle_block(block, false).await;
    assert_not_owned(&result, new_partition, miner, None);
    assert_eq!(node.get_max_difficulty_block().block_hash, tip.block_hash);

    node.stop().await;
    Ok(())
}

/// An unpledge takes effect at the epoch block, so the partition is still owned
/// on that block's parent snapshot and gone on the epoch block's own snapshot.
/// A block whose parent is the epoch block cannot use it: the producer skips the
/// solution and validators reject a block built anyway.
#[test_log::test(tokio::test)]
async fn partition_unpledged_at_epoch_is_rejected_on_the_epoch_block_child() -> eyre::Result<()> {
    let node = start_node(start_config(4)).await;
    let miner = node.node_ctx.config.node_config.miner_address();

    let pledge_inclusion = mine_pledge_into_block(&node).await?;
    let (before_assign, assign_epoch) =
        mine_assigning_epoch(&node, pledge_inclusion.height).await?;
    let partition_hash =
        new_partition_at_epoch(&node, &before_assign.block_hash, &assign_epoch.block_hash);
    {
        let tree = node.node_ctx.block_tree_guard.read();
        let snapshot = tree
            .get_epoch_snapshot(&assign_epoch.block_hash)
            .expect("epoch snapshot");
        eyre::ensure!(
            snapshot
                .partition_assignments
                .capacity_partitions
                .contains_key(&partition_hash),
            "only a capacity partition can be unpledged"
        );
    }

    let mut unpledge = CommitmentTransaction::new_unpledge(
        &node.node_ctx.config.consensus,
        node.get_anchor().await?,
        node.node_ctx.mempool_pledge_provider.as_ref(),
        miner,
        partition_hash,
    )
    .await;
    node.cfg.signer().sign_commitment(&mut unpledge)?;
    node.post_commitment_tx(&unpledge).await?;
    let unpledge_inclusion = mine_into_block(&node, unpledge.id()).await?;
    let (before_unpledge, unpledge_epoch) =
        mine_assigning_epoch(&node, unpledge_inclusion.height).await?;

    assert!(
        miner_partitions(&node, &before_unpledge.block_hash).contains(&partition_hash),
        "partition must be owned on the epoch block's parent snapshot"
    );
    assert!(
        !miner_partitions(&node, &unpledge_epoch.block_hash).contains(&partition_hash),
        "partition must be gone on the epoch block's own snapshot"
    );

    let solution = solution_context_for_partition(&node.node_ctx, partition_hash).await?;
    assert_skipped(&node, &solution).await;

    let (block, _stats, _payload) = unowned_strategy(&node)
        .fully_produce_new_block_without_gossip(&solution)
        .await
        .map_err(|err| eyre::eyre!("block production failed: {err:?}"))?
        .ok_or_else(|| eyre::eyre!("evil producer must build the block"))?;
    assert_eq!(
        block.header().previous_block_hash,
        unpledge_epoch.block_hash
    );
    let tip = node.get_max_difficulty_block();
    let result = discovery(&node).handle_block(block, false).await;
    assert_not_owned(&result, partition_hash, miner, None);
    assert_eq!(node.get_max_difficulty_block().block_hash, tip.block_hash);

    node.stop().await;
    Ok(())
}
