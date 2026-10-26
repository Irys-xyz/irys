//! A signer has at most one stake. A Stake from a signer that is already
//! staked, or that has a pending stake (included earlier in this epoch), is
//! rejected at mempool ingress, skipped by block-producer selection, and
//! rejected by block prevalidation and full validation.

use super::send_block_and_read_state;
use crate::utils::{
    AddTxError, InjectCommitmentsStrategy, IrysNodeTest, assert_validation_error, solution_context,
};
use irys_actors::{
    ProductionStrategy,
    block_discovery::{BlockDiscoveryError, BlockDiscoveryFacade as _},
    block_producer::BlockProdStrategy as _,
    block_validation::ValidationError,
    mempool_service::TxIngressError,
};
use irys_chain::IrysNodeCtx;
use irys_domain::CommitmentSnapshotStatus;
use irys_types::{
    CommitmentTransaction, CommitmentTypeV2, CommitmentValidationError, H256, NodeConfig,
    irys::IrysSigner,
};
use std::sync::Arc;

const NUM_BLOCKS_IN_EPOCH: u64 = 8;
const SECONDS_TO_WAIT: usize = 20;

async fn start_node_with_funded_signer() -> (IrysNodeTest<IrysNodeCtx>, IrysSigner) {
    let mut config = NodeConfig::testing_with_epochs(NUM_BLOCKS_IN_EPOCH as usize);
    config.consensus.get_mut().chunk_size = 32;
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);
    let node = IrysNodeTest::new_genesis(config)
        .start_and_wait_for_packing("GENESIS", SECONDS_TO_WAIT)
        .await;
    (node, signer)
}

/// Signs a Stake for `signer`. `fee_increment` makes each stake's id distinct.
fn signed_stake(
    node: &IrysNodeTest<IrysNodeCtx>,
    signer: &IrysSigner,
    anchor: H256,
    fee_increment: u64,
) -> eyre::Result<CommitmentTransaction> {
    let mut stake = CommitmentTransaction::new_stake(&node.node_ctx.config.consensus, anchor);
    stake.set_fee(stake.fee() + fee_increment);
    signer.sign_commitment(&mut stake)?;
    Ok(stake)
}

/// Produces a block that carries exactly `stakes` in its commitment ledger, then
/// checks that block prevalidation and full validation both reject it with
/// `AlreadyStaked`.
async fn assert_block_with_stakes_rejected(
    node: &IrysNodeTest<IrysNodeCtx>,
    mut stakes: Vec<CommitmentTransaction>,
    rejected_stake_id: H256,
) -> eyre::Result<()> {
    stakes.sort();
    let strategy = InjectCommitmentsStrategy {
        txs: stakes,
        prod: ProductionStrategy {
            inner: node.node_ctx.block_producer_inner.clone(),
        },
    };

    node.gossip_disable();
    let (block, _stats, _payload) = strategy
        .fully_produce_new_block_without_gossip(&solution_context(&node.node_ctx).await?)
        .await?
        .expect("block produced");
    node.gossip_enable();

    let prevalidation = strategy
        .inner()
        .block_discovery
        .handle_block(Arc::clone(&block), false)
        .await;
    assert!(
        matches!(
            &prevalidation,
            Err(BlockDiscoveryError::InvalidCommitmentTransaction(msg))
                if msg.contains(&rejected_stake_id.to_string())
        ),
        "prevalidation must reject the second stake, got {prevalidation:?}"
    );

    let outcome = send_block_and_read_state(&node.node_ctx, block, false).await?;
    assert_validation_error(
        outcome,
        |e| {
            matches!(
                e,
                ValidationError::CommitmentSnapshotRejected {
                    tx_id,
                    status: CommitmentSnapshotStatus::AlreadyStaked,
                } if *tx_id == rejected_stake_id
            )
        },
        "full validation must reject the second stake with AlreadyStaked",
    );
    Ok(())
}

/// A block that carries two Stakes from one signer is invalid.
#[test_log::test(tokio::test)]
async fn heavy_block_with_two_stakes_from_one_signer_rejected() -> eyre::Result<()> {
    let (node, signer) = start_node_with_funded_signer().await;

    let anchor = node.get_anchor().await?;
    let first_stake = signed_stake(&node, &signer, anchor, 0)?;
    let second_stake = signed_stake(&node, &signer, anchor, 1)?;

    // Validation adds commitments in sorted order; the later one is rejected.
    let rejected_stake_id = std::cmp::max(first_stake.clone(), second_stake.clone()).id();
    assert_block_with_stakes_rejected(&node, vec![first_stake, second_stake], rejected_stake_id)
        .await?;

    node.stop().await;
    Ok(())
}

/// After a signer's stake is included (pending until the epoch boundary), a
/// second Stake from that signer is rejected at mempool ingress, and a block
/// that carries it is invalid.
#[test_log::test(tokio::test)]
async fn heavy_stake_from_signer_with_pending_stake_rejected() -> eyre::Result<()> {
    let (node, signer) = start_node_with_funded_signer().await;

    let first_stake = node.post_stake_commitment_with_signer(&signer).await?;
    node.wait_for_mempool_commitment_txs(vec![first_stake.id()], SECONDS_TO_WAIT)
        .await?;
    let block = node.mine_block().await?;
    assert!(
        block.height < NUM_BLOCKS_IN_EPOCH,
        "the stake must stay pending in this epoch"
    );
    assert!(block.commitment_tx_ids().contains(&first_stake.id()));
    assert_eq!(
        node.get_commitment_snapshot_status(&first_stake),
        CommitmentSnapshotStatus::Accepted
    );

    let second_stake = signed_stake(&node, &signer, node.get_anchor().await?, 1)?;
    assert_eq!(
        node.get_commitment_snapshot_status(&second_stake),
        CommitmentSnapshotStatus::AlreadyStaked
    );

    let ingress = node.ingest_commitment_tx(second_stake.clone()).await;
    assert!(
        matches!(
            ingress,
            Err(AddTxError::TxIngress(
                TxIngressError::CommitmentValidationError(CommitmentValidationError::AlreadyStaked)
            ))
        ),
        "mempool ingress must reject the second stake, got {ingress:?}"
    );

    assert_block_with_stakes_rejected(&node, vec![second_stake.clone()], second_stake.id()).await?;

    node.stop().await;
    Ok(())
}

/// With two Stakes from one signer in the mempool, the block producer selects
/// only one of them.
#[test_log::test(tokio::test)]
async fn heavy_producer_selects_one_of_two_stakes_from_one_signer() -> eyre::Result<()> {
    let (node, signer) = start_node_with_funded_signer().await;

    // Neither stake is in a block yet, so mempool ingress admits both.
    let anchor = node.get_anchor().await?;
    let stakes = [
        signed_stake(&node, &signer, anchor, 0)?,
        signed_stake(&node, &signer, anchor, 1)?,
    ];
    for stake in &stakes {
        node.post_commitment_tx(stake).await?;
    }
    node.wait_for_mempool_commitment_txs(
        stakes.iter().map(CommitmentTransaction::id).collect(),
        SECONDS_TO_WAIT,
    )
    .await?;

    let mut included_stake_ids = Vec::new();
    for _ in 0..2 {
        let block = node.mine_block().await?;
        assert!(block.height < NUM_BLOCKS_IN_EPOCH);
        included_stake_ids.extend(
            stakes
                .iter()
                .map(CommitmentTransaction::id)
                .filter(|id| block.commitment_tx_ids().contains(id)),
        );
    }
    assert_eq!(
        included_stake_ids.len(),
        1,
        "exactly one of the two stakes must be included, got {included_stake_ids:?}"
    );

    let snapshot = node
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_commitment_snapshot();
    let pending_stakes = snapshot
        .get_epoch_commitments()
        .into_iter()
        .filter(|tx| tx.signer() == signer.address())
        .filter(|tx| matches!(tx.commitment_type(), CommitmentTypeV2::Stake))
        .count();
    assert_eq!(pending_stakes, 1);

    node.stop().await;
    Ok(())
}
