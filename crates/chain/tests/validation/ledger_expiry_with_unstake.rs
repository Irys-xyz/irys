//! Tests for reward address behavior during ledger expiry and unpledge refunds.

use alloy_eips::HashOrNumber;
use alloy_rpc_types_eth::TransactionTrait as _;
use irys_reth_node_bridge::irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};
use irys_types::{
    CommitmentTransaction, DataLedger, IrysAddress, NodeConfig, PledgeDataProvider as _, U256,
};
use reth::providers::TransactionsProvider as _;

use crate::utils::IrysNodeTest;

#[test_log::test(tokio::test)]
async fn heavy_test_ledger_expiry_uses_custom_reward_address() -> eyre::Result<()> {
    let num_blocks_in_epoch: usize = 5;
    let submit_ledger_epoch_length: u64 = 2;
    let chunk_size = 32_u64;
    let num_chunks_in_partition = 10_u64;

    let mut config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().chunk_size = chunk_size;
    config.consensus.get_mut().num_chunks_in_partition = num_chunks_in_partition;
    config.consensus.get_mut().epoch.submit_ledger_epoch_length = submit_ledger_epoch_length;

    let user_signer = irys_types::irys::IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&user_signer]);

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", 30)
        .await;

    let signer = node.cfg.signer();
    let custom_reward_address = IrysAddress::random();

    // Epoch 0: Set custom reward address and post data
    let update_tx = node
        .post_update_reward_address(&signer, custom_reward_address, U256::from(1))
        .await?;
    node.wait_for_mempool(update_tx.id(), 30).await?;
    node.mine_block().await?;

    let anchor = node.get_anchor().await?;
    let num_txs = (num_chunks_in_partition + 2) as usize;
    for i in 0..num_txs {
        let data = vec![42 + i as u8; chunk_size as usize];
        let tx = node.post_data_tx(anchor, data, &user_signer).await;
        node.wait_for_mempool(tx.header.id, 30).await?;
    }
    let data_block = node.mine_block().await?;

    let tx_ids = data_block.get_data_ledger_tx_ids();
    assert!(tx_ids
        .get(&DataLedger::Submit)
        .map(|t| !t.is_empty())
        .unwrap_or(false));

    // Epoch 1: Reward address takes effect
    node.mine_until_next_epoch().await?;

    // Epoch 2: Data expires (data_epoch=0 + submit_ledger_epoch_length=2)
    let (_, expiry_height) = node.mine_until_next_epoch().await?;

    // Verify TermFeeReward goes to custom reward address
    let expiry_block = node.get_block_by_height(expiry_height).await?;
    let txs = node
        .node_ctx
        .reth_node_adapter
        .inner
        .provider
        .transactions_by_block(HashOrNumber::Hash(expiry_block.evm_block_hash))?
        .unwrap_or_default();

    let mut found_reward = false;
    for tx in &txs {
        if let Ok(shadow) = ShadowTransaction::decode(&mut tx.input().as_ref()) {
            if let Some(TransactionPacket::TermFeeReward(reward)) = shadow.as_v1() {
                assert_eq!(
                    reward.target,
                    custom_reward_address.to_alloy_address(),
                    "TermFeeReward must go to custom reward_address"
                );
                found_reward = true;
            }
        }
    }
    assert!(
        found_reward,
        "Expected TermFeeReward at expiry height {}",
        expiry_height
    );

    node.stop().await;
    Ok(())
}

/// Test that UnpledgeRefund goes to miner_address, not custom reward_address.
#[test_log::test(tokio::test)]
async fn heavy_test_unpledge_refund_uses_miner_address() -> eyre::Result<()> {
    let num_blocks_in_epoch: usize = 5;

    let mut config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    config.consensus.get_mut().block_migration_depth = 1;

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", 30)
        .await;

    let signer = node.cfg.signer();
    let miner_address = signer.address();
    let custom_reward_address = IrysAddress::random();

    // Epoch 0: Set custom reward address
    let update_tx = node
        .post_update_reward_address(&signer, custom_reward_address, U256::from(1))
        .await?;
    node.wait_for_mempool(update_tx.id(), 30).await?;
    node.mine_block().await?;

    // Epoch 1: Reward address takes effect, post unpledge
    node.mine_until_next_epoch().await?;

    let assignments = node.get_partition_assignments(miner_address);
    assert!(
        !assignments.is_empty(),
        "Miner should have partition assignments"
    );
    let partition_to_unpledge = assignments[0].partition_hash;

    let pledge_count = node
        .node_ctx
        .mempool_pledge_provider
        .as_ref()
        .pledge_count(miner_address)
        .await;
    let anchor = node.get_anchor().await?;
    let mut unpledge_tx = CommitmentTransaction::new_unpledge(
        &node.node_ctx.config.consensus,
        anchor,
        &pledge_count,
        miner_address,
        partition_to_unpledge,
    )
    .await;
    signer.sign_commitment(&mut unpledge_tx)?;
    node.post_commitment_tx(&unpledge_tx).await?;
    node.wait_for_mempool(unpledge_tx.id(), 30).await?;
    node.mine_block().await?;

    // Epoch 2: Unpledge refund issued
    let (_, refund_height) = node.mine_until_next_epoch().await?;

    // Verify UnpledgeRefund goes to miner address
    let refund_block = node.get_block_by_height(refund_height).await?;
    let txs = node
        .node_ctx
        .reth_node_adapter
        .inner
        .provider
        .transactions_by_block(HashOrNumber::Hash(refund_block.evm_block_hash))?
        .unwrap_or_default();

    let mut found_refund = false;
    for tx in &txs {
        if let Ok(shadow) = ShadowTransaction::decode(&mut tx.input().as_ref()) {
            if let Some(TransactionPacket::UnpledgeRefund(refund)) = shadow.as_v1() {
                assert_eq!(
                    refund.target,
                    miner_address.to_alloy_address(),
                    "UnpledgeRefund must go to miner_address, not custom reward_address"
                );
                found_refund = true;
            }
        }
    }
    assert!(
        found_refund,
        "Expected UnpledgeRefund at refund height {}",
        refund_height
    );

    node.stop().await;
    Ok(())
}
