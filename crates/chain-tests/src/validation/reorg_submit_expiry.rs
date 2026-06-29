//! NC-0042 M4: a reorg whose canonical tip moves ACROSS the Submit-ledger expiry
//! epoch must settle expiry branch-correctly on the WINNING branch.
//!
//! The `slot_lifetime > block_tree_depth` config invariant keeps every expired
//! tx's Submit inclusion BELOW the reorg floor, so a reorg cannot rewrite an
//! expired inclusion. What this test pins is the integration behaviour the unit
//! tests (C1 guard, by-hash walk) cannot: a node whose canonical tip sat on the
//! LOSING fork validates and adopts the WINNING fork's expiry-epoch block —
//! recomputing the perm-fee-refund settlement against the winning branch's own
//! ancestry — without stalling, and the resulting canonical expiry block carries
//! the refunds for the expired slot-0 txs.
//!
//! Layout (num_blocks_in_epoch = 2, submit_ledger_epoch_length = 2 ⇒ slot 0
//! expires at height 4):
//!   h1: overflow Submit data + peer stake/pledge commitments (common ancestor)
//!   h2: epoch — 2nd Submit slot allocated (slot 0 becomes non-last); peers packed
//!   h3: FORK — peer1 and peer2 each mine a competing block (no gossip)
//!   h4: extend the fork genesis did NOT choose → heavier → reorg moves the
//!       canonical tip across the expiry epoch onto the winning branch.

use crate::utils::IrysNodeTest;
use irys_actors::block_producer::ledger_expiry::calculate_expired_ledger_fees;
use irys_reth_node_bridge::irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};
use irys_types::{DataLedger, H256, NodeConfig};
use reth::rpc::types::TransactionTrait as _;
use std::collections::BTreeSet;
use std::sync::Arc;
use tracing::debug;

#[test_log::test(tokio::test)]
async fn heavy_reorg_across_submit_expiry_epoch_settles_on_winning_branch() -> eyre::Result<()> {
    let num_blocks_in_epoch: usize = 2;
    let submit_ledger_epoch_length: u64 = 2;
    let blocks_per_cycle = num_blocks_in_epoch as u64 * submit_ledger_epoch_length; // 4 (expiry height)
    let chunk_size: u64 = 32;
    let num_chunks_in_partition: u64 = 10;
    let seconds_to_wait = 40_usize;

    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    {
        let c = genesis_config.consensus.get_mut();
        c.chunk_size = chunk_size;
        c.num_chunks_in_partition = num_chunks_in_partition;
        c.block_migration_depth = 1;
        c.epoch.submit_ledger_epoch_length = submit_ledger_epoch_length;
        // One ingress proof makes a tx promotable; keeps the promotion side simple.
        c.hardforks.frontier.number_of_ingress_proofs_total = 1;
        // Cascade OFF: allocation-anchored Submit expiry at exactly blocks_per_cycle.
        c.hardforks.cascade = None;
    }

    let peer1_signer = genesis_config.new_random_signer();
    let peer2_signer = genesis_config.new_random_signer();
    let user_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer1_signer, &peer2_signer, &user_signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    let peer1_node = IrysNodeTest::new(genesis_node.testing_peer_with_signer(&peer1_signer))
        .start_with_name("PEER1")
        .await;
    let peer2_node = IrysNodeTest::new(genesis_node.testing_peer_with_signer(&peer2_signer))
        .start_with_name("PEER2")
        .await;

    // Stake + pledge both peers so they get partition assignments at the epoch.
    let p1_stake = peer1_node.post_stake_commitment(None).await?;
    let p1_pledge = peer1_node.post_pledge_commitment(None).await?;
    let p2_stake = peer2_node.post_stake_commitment(None).await?;
    let p2_pledge = peer2_node.post_pledge_commitment(None).await?;
    for id in [p1_stake.id(), p1_pledge.id(), p2_stake.id(), p2_pledge.id()] {
        genesis_node.wait_for_mempool(id, seconds_to_wait).await?;
    }

    // Overflow data txs into the genesis Submit slot (forces a 2nd slot so slot 0
    // is non-last and can expire). These land in the common ancestor (h1).
    let num_txs = (num_chunks_in_partition + 2) as usize;
    let anchor = genesis_node.get_anchor().await?;
    let mut txs = Vec::with_capacity(num_txs);
    for i in 0..num_txs {
        let data = vec![100 + i as u8; chunk_size as usize];
        let tx = genesis_node.post_data_tx(anchor, data, &user_signer).await;
        genesis_node
            .wait_for_mempool(tx.header.id, seconds_to_wait)
            .await?;
        txs.push(tx);
    }
    let _posted_ids: BTreeSet<H256> = txs.iter().map(|t| t.header.id).collect();

    // h1 (data + commitments), h2 (epoch: 2nd Submit slot + peer partitions).
    genesis_node.mine_block().await?;
    genesis_node.mine_block().await?;
    genesis_node
        .wait_for_block_at_height(2, seconds_to_wait)
        .await?;
    genesis_node
        .wait_until_block_index_height(1, seconds_to_wait)
        .await?;
    peer1_node
        .wait_until_block_index_height(1, seconds_to_wait)
        .await?;
    peer2_node
        .wait_until_block_index_height(1, seconds_to_wait)
        .await?;
    peer1_node.wait_for_packing(seconds_to_wait).await;
    peer2_node.wait_for_packing(seconds_to_wait).await;

    // FORK at h2: each peer mines its own h3 without gossip.
    let (r1, r2) = tokio::join!(
        peer1_node.mine_blocks_without_gossip(1),
        peer2_node.mine_blocks_without_gossip(1),
    );
    r1?;
    r2?;
    peer1_node
        .wait_for_block_at_height(3, seconds_to_wait)
        .await?;
    peer2_node
        .wait_for_block_at_height(3, seconds_to_wait)
        .await?;

    let peer1_b3 = Arc::new(peer1_node.get_block_by_height(3).await?);
    let peer2_b3 = Arc::new(peer2_node.get_block_by_height(3).await?);
    peer1_node.gossip_block_to_peers(&peer1_b3)?;
    peer2_node.gossip_block_to_peers(&peer2_b3)?;
    peer1_node
        .wait_for_block(&peer2_b3.block_hash, seconds_to_wait)
        .await?;
    peer2_node
        .wait_for_block(&peer1_b3.block_hash, seconds_to_wait)
        .await?;
    genesis_node
        .wait_for_block_at_height(3, seconds_to_wait)
        .await?;
    let genesis_b3 = genesis_node.get_block_by_height(3).await?;
    let genesis_chose_peer1 = genesis_b3.block_hash == peer1_b3.block_hash;

    // Extend the fork genesis did NOT choose to h4 (the expiry epoch). The extended
    // branch is heavier (height 4 vs 3) → genesis reorgs onto it, carrying the
    // canonical tip across the expiry boundary.
    let reorg_future = genesis_node.wait_for_reorg(seconds_to_wait);
    if genesis_chose_peer1 {
        peer2_node.mine_block().await?;
    } else {
        peer1_node.mine_block().await?;
    }
    let reorg_event = reorg_future.await?;
    debug!(
        "reorg across expiry: fork_parent h={}, old_fork={} new_fork={}",
        reorg_event.fork_parent.height,
        reorg_event.old_fork.len(),
        reorg_event.new_fork.len()
    );

    // Genesis must now hold the winning branch's expiry block at h4.
    genesis_node
        .wait_for_block_at_height(blocks_per_cycle, seconds_to_wait)
        .await?;
    let expiry_block = genesis_node.get_block_by_height(blocks_per_cycle).await?;
    assert_eq!(expiry_block.height, blocks_per_cycle);

    let winner_b4 = if genesis_chose_peer1 {
        peer2_node.get_block_by_height(blocks_per_cycle).await?
    } else {
        peer1_node.get_block_by_height(blocks_per_cycle).await?
    };
    assert_eq!(
        expiry_block.block_hash, winner_b4.block_hash,
        "canonical expiry block must be the WINNING branch's h{blocks_per_cycle} after the reorg"
    );

    // Branch-correct settlement: the winning branch's expiry block must perm-fee
    // -refund the expired slot-0 Submit txs. Decode the canonical EVM body and
    // confirm the refunds are present and only for the posted Submit txs.
    let user_addr = user_signer.address().to_alloy_address();
    let evm_block = genesis_node
        .wait_for_evm_block(expiry_block.evm_block_hash, seconds_to_wait)
        .await?;
    let refunded: BTreeSet<H256> = evm_block
        .body
        .transactions
        .into_iter()
        .filter(|tx| tx.input().len() >= 4)
        .filter_map(|tx| {
            let shadow_tx = ShadowTransaction::decode(&mut tx.input().as_ref()).ok()?;
            match shadow_tx.as_v1()? {
                TransactionPacket::PermFeeRefund(refund) if refund.target == user_addr => {
                    Some(H256(refund.irys_ref.into()))
                }
                _ => None,
            }
        })
        .collect();
    assert!(
        !refunded.is_empty(),
        "the winning branch's expiry block must perm-fee-refund the expired slot-0 Submit txs \
         (branch-correct settlement must survive the reorg across the expiry epoch)"
    );

    // Exact equality against Pipeline B: recompute what the refund pipeline would
    // produce for this block and assert the on-chain set matches it exactly — a
    // partial or missing refund fails the test.
    let expiry_parent_block = genesis_node
        .get_block_by_height(blocks_per_cycle - 1)
        .await?;
    let expiry_parent_snapshot = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .get_epoch_snapshot(&expiry_block.previous_block_hash)
        .expect("epoch snapshot for the expiry block's parent");
    // Cascade is OFF (hardforks.cascade = None above), so cascade_active_for_block is false.
    let cascade_active_for_block = genesis_node
        .node_ctx
        .config
        .consensus
        .hardforks
        .is_cascade_active_at(expiry_block.timestamp_secs());
    let pipeline_b = calculate_expired_ledger_fees(
        &expiry_parent_snapshot,
        &expiry_parent_block,
        blocks_per_cycle,
        DataLedger::Submit,
        &genesis_node.node_ctx.config,
        genesis_node
            .node_ctx
            .block_producer_inner
            .block_index
            .clone(),
        &genesis_node.node_ctx.block_tree_guard,
        &genesis_node.node_ctx.mempool_guard,
        &genesis_node.node_ctx.db,
        true,
        expiry_block.ledger_total_chunks(DataLedger::Submit),
        cascade_active_for_block,
    )
    .await?;
    let expected_refunds: BTreeSet<H256> = pipeline_b
        .user_perm_fee_refunds
        .iter()
        .map(|(id, _, _)| *id)
        .collect();
    assert!(
        !expected_refunds.is_empty(),
        "fixture must produce refunds (non-vacuous)"
    );
    assert_eq!(
        refunded, expected_refunds,
        "winning branch's on-chain refunds must EXACTLY equal Pipeline B's refund set \
         (branch-correct settlement across the reorg)"
    );

    peer2_node.stop().await;
    peer1_node.stop().await;
    genesis_node.stop().await;
    Ok(())
}
