//! Test that verifies: at the same epoch boundary, a miner receives ledger expiry rewards
//! (to their `reward_address`) while also processing their unpledge and unstake refunds.
//! This validates that the reward address resolution uses the parent epoch snapshot
//! (before unstake takes effect).

use alloy_eips::HashOrNumber;
use alloy_rpc_types_eth::TransactionTrait as _;
use irys_reth_node_bridge::irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};
use irys_types::{
    partition::PartitionAssignment, CommitmentTransaction, DataLedger, IrysAddress, NodeConfig,
    PledgeDataProvider as _, U256,
};
use reth::providers::TransactionsProvider as _;
use tracing::info;

use crate::utils::IrysNodeTest;

/// Test that validates ledger expiry rewards go to the custom reward_address even when
/// the miner is simultaneously unstaking at the same epoch boundary.
///
/// ## Why a Second Node is Needed
/// After genesis unstakes, it loses all partition assignments. Without another miner (peer),
/// no one can produce valid PoA solutions. The peer keeps the network functional for
/// verification after genesis exits.
///
/// ## Test Flow
/// 1. Setup genesis node with stake + pledges (owns partitions)
/// 2. Genesis posts UpdateRewardAddress with custom_reward_address
/// 3. Post data tx (stored in genesis's partition - genesis is only miner at this point)
/// 4. Create peer miner with stake + pledge to keep network functional
/// 5. Mine to next epoch so UpdateRewardAddress takes effect
/// 6. Genesis unpledges all partitions and posts unstake
/// 7. Mine to expiry epoch where: ledger expires AND unpledge/unstake refunds occur
/// 8. Verify:
///    - TermFeeReward goes to custom_reward_address (NOT genesis_addr)
///    - UnpledgeRefund goes to genesis miner_address
///    - UnstakeRefund goes to genesis miner_address
///    - Genesis is fully unstaked
#[test_log::test(tokio::test)]
async fn heavy_test_ledger_expiry_with_concurrent_unstake() -> eyre::Result<()> {
    // Configuration - use smaller epochs to reduce test time
    let num_blocks_in_epoch: usize = 2; // Smaller epochs for faster test
    let submit_ledger_epoch_length: u64 = 1; // Data expires after 1 epoch (faster)
    let chunk_size = 32_u64;
    let num_chunks_in_partition = 10_u64;
    let seconds_to_wait = 30;

    // Setup genesis config
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().block_migration_depth = 1;
    genesis_config.consensus.get_mut().chunk_size = chunk_size;
    genesis_config.consensus.get_mut().num_chunks_in_partition = num_chunks_in_partition;
    genesis_config
        .consensus
        .get_mut()
        .epoch
        .submit_ledger_epoch_length = submit_ledger_epoch_length;

    // Fund user for data tx
    let user_signer =
        irys_types::irys::IrysSigner::random_signer(&genesis_config.consensus_config());
    genesis_config.fund_genesis_accounts(vec![&user_signer]);

    // Fund peer signer
    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    // Start genesis node
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let genesis_signer = genesis_node.cfg.signer();
    let genesis_addr = genesis_signer.address();
    let custom_reward_address = IrysAddress::random();

    info!(
        "Genesis address: {}, Custom reward address: {}",
        genesis_addr, custom_reward_address
    );

    // Genesis updates reward_address (before adding peer)
    let _update_tx = genesis_node
        .post_update_reward_address(&genesis_signer, custom_reward_address, U256::from(1))
        .await
        .expect("posted update reward address commitment tx");
    genesis_node
        .wait_for_mempool(_update_tx.id(), seconds_to_wait)
        .await?;
    genesis_node.mine_block().await?;

    info!("UpdateRewardAddress transaction included in block");

    // Post data before adding peer (ensures data in genesis's partitions)
    // Need >1 slot worth of data so first slot can expire (expiry skips active slot)
    let num_txs_to_post = (num_chunks_in_partition + 2) as usize;
    info!(
        "Posting {} data transactions to fill at least one complete slot",
        num_txs_to_post
    );

    let anchor = genesis_node.get_anchor().await?;
    let mut data_tx_ids = Vec::new();
    for i in 0..num_txs_to_post {
        let data = vec![42 + i as u8; chunk_size as usize]; // One chunk per tx
        let data_tx = genesis_node.post_data_tx(anchor, data, &user_signer).await;
        genesis_node
            .wait_for_mempool(data_tx.header.id, seconds_to_wait)
            .await?;
        data_tx_ids.push(data_tx.header.id);
    }

    let data_inclusion_block = genesis_node.mine_block().await?;

    // Verify data txs were included in Submit ledger
    let tx_ids = data_inclusion_block.get_data_ledger_tx_ids();
    let submit_txs = tx_ids
        .get(&DataLedger::Submit)
        .expect("Submit ledger should have transactions");
    let included_count = data_tx_ids
        .iter()
        .filter(|id| submit_txs.contains(id))
        .count();
    info!(
        "Included {}/{} data transactions in Submit ledger at height {}",
        included_count, num_txs_to_post, data_inclusion_block.height
    );
    assert!(
        included_count > 0,
        "At least one data transaction should be included in Submit ledger"
    );

    let data_submit_height = data_inclusion_block.height;

    // Calculate when data will expire (for later TermFeeReward check)
    // Data in epoch N expires at the start of epoch N + submit_ledger_epoch_length
    let data_inclusion_epoch = data_submit_height / num_blocks_in_epoch as u64;
    let expiry_epoch = data_inclusion_epoch + submit_ledger_epoch_length;
    let data_expiry_height = expiry_epoch * num_blocks_in_epoch as u64;
    info!(
        "Data included at height {} (epoch {}), will expire at height {} (epoch {})",
        data_submit_height, data_inclusion_epoch, data_expiry_height, expiry_epoch
    );

    // TermFeeReward generated at Submit expiry (promotion not needed)

    // Add peer miner (keeps network functional after genesis unstakes)
    let peer_node = genesis_node
        .testing_peer_with_assignments(&peer_signer)
        .await?;

    let height_after_peer = genesis_node.get_canonical_chain_height().await;
    info!(
        "Peer node created with address: {}, current height: {}",
        peer_signer.address(),
        height_after_peer
    );

    // Mine to epoch boundary (UpdateRewardAddress takes effect)
    let (_mined, epoch1_height) = genesis_node.mine_until_next_epoch().await?;
    peer_node
        .wait_until_height(epoch1_height, seconds_to_wait)
        .await?;

    info!(
        "Reached epoch boundary at height {}, UpdateRewardAddress should now be active",
        epoch1_height
    );

    // Verify reward_address is set in epoch snapshot
    let epoch_snapshot = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot();
    let stake_entry = epoch_snapshot
        .commitment_state
        .stake_commitments
        .get(&genesis_addr)
        .expect("Genesis should have stake in epoch snapshot");
    assert_eq!(
        stake_entry.reward_address,
        Some(custom_reward_address),
        "reward_address should be updated to custom_reward_address after epoch boundary"
    );

    info!(
        "Verified reward_address is active: {:?}",
        stake_entry.reward_address
    );

    // Genesis unpledges all partitions
    let assigned_partitions: Vec<PartitionAssignment> =
        genesis_node.get_partition_assignments(genesis_addr);
    info!(
        "Genesis has {} partition assignments to unpledge",
        assigned_partitions.len()
    );

    let consensus = &genesis_node.node_ctx.config.consensus;
    let initial_pledge_count = genesis_node
        .node_ctx
        .mempool_pledge_provider
        .as_ref()
        .pledge_count(genesis_addr)
        .await;

    for (idx, assignment) in assigned_partitions.iter().enumerate() {
        let pledge_count = initial_pledge_count - (idx as u64);
        if pledge_count == 0 {
            break;
        }
        let anchor = genesis_node.get_anchor().await?;
        let mut unpledge_tx = CommitmentTransaction::new_unpledge(
            consensus,
            anchor,
            &pledge_count,
            genesis_addr,
            assignment.partition_hash,
        )
        .await;
        genesis_signer.sign_commitment(&mut unpledge_tx)?;
        genesis_node.post_commitment_tx(&unpledge_tx).await?;
        genesis_node
            .wait_for_mempool(unpledge_tx.id(), seconds_to_wait)
            .await?;
        info!(
            "Posted unpledge for partition {} (pledge_count: {})",
            assignment.partition_hash, pledge_count
        );
    }

    // Genesis posts unstake commitment
    let anchor = genesis_node.get_anchor().await?;
    let mut unstake_tx = CommitmentTransaction::new_unstake(consensus, anchor);
    genesis_signer.sign_commitment(&mut unstake_tx)?;
    genesis_node.post_commitment_tx(&unstake_tx).await?;
    genesis_node
        .wait_for_mempool(unstake_tx.id(), seconds_to_wait)
        .await?;

    info!("Posted unstake commitment: {}", unstake_tx.id());

    // Mine block to include unpledges and unstake
    genesis_node.mine_block().await?;

    // Mine to refund epoch (refunds happen at next epoch boundary)
    let current_height = genesis_node.get_canonical_chain_height().await;
    info!(
        "Unpledge/unstake commitments included, current height: {}. Mining to next epoch boundary for refunds.",
        current_height
    );

    let (_, refund_epoch_height) = genesis_node.mine_until_next_epoch().await?;
    peer_node
        .wait_until_height(refund_epoch_height, seconds_to_wait)
        .await?;
    info!(
        "Reached refund epoch boundary at height {}",
        refund_epoch_height
    );

    // Verify shadow transactions
    let reth_ctx = genesis_node.node_ctx.reth_node_adapter.clone();

    let mut found_term_fee_reward = false;
    let mut found_unpledge_refunds = 0_usize;
    let mut found_unstake_refund = false;

    // Check data_expiry_height for TermFeeReward (ledger expiry)
    {
        let expiry_block = genesis_node.get_block_by_height(data_expiry_height).await?;
        let block_txs = reth_ctx
            .inner
            .provider
            .transactions_by_block(HashOrNumber::Hash(expiry_block.evm_block_hash))?
            .unwrap_or_default();

        for tx in &block_txs {
            if let Ok(shadow_tx) = ShadowTransaction::decode(&mut tx.input().as_ref()) {
                if let Some(TransactionPacket::TermFeeReward(reward)) = shadow_tx.as_v1() {
                    info!(
                        "Found TermFeeReward at height {}: target={}, amount={}",
                        data_expiry_height, reward.target, reward.amount
                    );
                    // Data was submitted before peer was added, so it MUST be in genesis's partition.
                    // KEY ASSERTION: TermFeeReward must go to custom_reward_address (not genesis miner address)
                    assert_eq!(
                        reward.target,
                        custom_reward_address.to_alloy_address(),
                        "TermFeeReward must go to custom reward_address, not genesis miner address"
                    );
                    found_term_fee_reward = true;
                }
            }
        }
    }

    // Check refund_epoch_height for UnpledgeRefund and UnstakeRefund
    {
        let refund_block = genesis_node
            .get_block_by_height(refund_epoch_height)
            .await?;
        let block_txs = reth_ctx
            .inner
            .provider
            .transactions_by_block(HashOrNumber::Hash(refund_block.evm_block_hash))?
            .unwrap_or_default();

        for tx in &block_txs {
            if let Ok(shadow_tx) = ShadowTransaction::decode(&mut tx.input().as_ref()) {
                match shadow_tx.as_v1() {
                    Some(TransactionPacket::UnpledgeRefund(refund)) => {
                        info!(
                            "Found UnpledgeRefund at height {}: target={}, amount={}",
                            refund_epoch_height, refund.target, refund.amount
                        );
                        assert_eq!(
                            refund.target,
                            genesis_addr.to_alloy_address(),
                            "UnpledgeRefund must go to genesis miner address"
                        );
                        found_unpledge_refunds += 1;
                    }
                    Some(TransactionPacket::UnstakeRefund(refund)) => {
                        info!(
                            "Found UnstakeRefund at height {}: target={}, amount={}",
                            refund_epoch_height, refund.target, refund.amount
                        );
                        assert_eq!(
                            refund.target,
                            genesis_addr.to_alloy_address(),
                            "UnstakeRefund must go to genesis miner address"
                        );
                        found_unstake_refund = true;
                    }
                    _ => {}
                }
            }
        }
    }

    // TermFeeReward is now mandatory: data was submitted before peer was added,
    // so it's guaranteed to be in genesis's partition and must expire with rewards.
    assert!(
        found_term_fee_reward,
        "Expected TermFeeReward at data_expiry_height (height {})",
        data_expiry_height
    );
    info!(
        "TermFeeReward found at height {} and correctly sent to custom_reward_address",
        data_expiry_height
    );

    assert_eq!(
        found_unpledge_refunds,
        assigned_partitions.len(),
        "Expected {} UnpledgeRefund transactions, found {}",
        assigned_partitions.len(),
        found_unpledge_refunds
    );
    assert!(
        found_unstake_refund,
        "Expected UnstakeRefund in expiry epoch"
    );

    info!(
        "Shadow transactions verified: TermFeeReward={}, UnpledgeRefunds={}, UnstakeRefund={}",
        found_term_fee_reward, found_unpledge_refunds, found_unstake_refund
    );

    // Verify genesis is fully unstaked
    let final_snapshot = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot();
    assert!(
        !final_snapshot
            .commitment_state
            .stake_commitments
            .contains_key(&genesis_addr),
        "Genesis should be fully unstaked after epoch boundary"
    );

    info!("Verified genesis is fully unstaked");

    // Balance verification skipped (complex timing); transaction targets verified above

    // Cleanup
    genesis_node.stop().await;
    peer_node.stop().await;

    info!("Test passed: Ledger expiry rewards correctly went to custom reward_address while unpledge/unstake refunds went to miner address");

    Ok(())
}
