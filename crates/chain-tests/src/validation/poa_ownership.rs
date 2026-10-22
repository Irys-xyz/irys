//! PoA partition ownership.
//!
//! A block may prove only a partition the parent epoch snapshot assigns to
//! its miner. The producer drops a solution that fails the same check.

use std::{collections::BTreeSet, sync::Arc};

use crate::utils::{
    BlockValidationOutcome, IrysNodeTest, read_block_from_state, solution_context,
    solution_context_for_partition,
};
use irys_actors::{
    BlockProdStrategy as _, ProductionStrategy,
    block_discovery::{BlockDiscoveryError, BlockDiscoveryFacade as _, BlockDiscoveryFacadeImpl},
    block_validation::PreValidationError,
};
use irys_chain::IrysNodeCtx;
use irys_types::{
    Base64, DataLedger, H256, IrysAddress, IrysBlockHeader, NodeConfig, SealedBlock,
    block_production::SolutionContext,
};

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

async fn mine_pledge_into_block(node: &IrysNodeTest<IrysNodeCtx>) -> eyre::Result<IrysBlockHeader> {
    let pledge_tx = node.post_pledge_commitment(None).await?;
    let pledge_id = pledge_tx.id();
    node.wait_for_mempool_commitment_txs(vec![pledge_id], 20)
        .await?;

    let mut inclusion = node.mine_block().await?;
    let mut mined = 1;
    while !inclusion.commitment_tx_ids().contains(&pledge_id) {
        eyre::ensure!(mined < 8, "pledge {pledge_id} was not included");
        inclusion = node.mine_block().await?;
        mined += 1;
    }
    Ok(inclusion)
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
