use alloy_consensus::Transaction as _;
use alloy_core::primitives::FixedBytes;
use alloy_eips::HashOrNumber;
use irys_reth_node_bridge::irys_reth::shadow_tx::{
    shadow_tx_topics, ShadowTransaction, TransactionPacket,
};
use irys_testing_utils::initialize_tracing;
use irys_types::{
    partition::PartitionAssignment, CommitmentTransaction, PledgeDataProvider as _, U256,
};
use reth::providers::{ReceiptProvider as _, TransactionsProvider as _};
use reth::rpc::types::BlockNumberOrTag;
use tokio::time::{sleep, Duration};

use crate::block_production::unpledge_refund::{
    assert_single_log_for, send_unpledge_all, setup_env, setup_env_with_block_migration_depth,
};
use crate::utils::IrysNodeTest;

/// Two-node scenario exercising the full unstake flow:
/// 1. unpledge all partitions from a signer and process epoch refunds.
/// 2. Submit an unstake commitment, mine the inclusion block, and ensure it is fee-only.
/// 3. Advance to the next epoch to assert the unstake refund, treasury delta, and stake removal.
#[test_log::test(tokio::test)]
async fn heavy_unstake_epoch_refund_flow() -> eyre::Result<()> {
    initialize_tracing();

    let num_blocks_in_epoch = 2_u64;
    let seconds_to_wait = 20_usize;
    let (genesis_node, peer_node, peer_addr, initial_assignment, consensus) =
        setup_env(num_blocks_in_epoch, seconds_to_wait).await?;
    let _initial_assignment: PartitionAssignment = initial_assignment;

    let reth_ctx = genesis_node.node_ctx.reth_node_adapter.clone();

    // Collect all capacity assignments so we can drain every active pledge before unstaking.
    let assigned_partitions: Vec<PartitionAssignment> = {
        let sms = peer_node.node_ctx.storage_modules_guard.read();
        sms.iter()
            .filter_map(|sm| sm.partition_assignment())
            .collect()
    };
    assert!(
        !assigned_partitions.is_empty(),
        "Test requires the peer to start with at least one pledged partition"
    );

    // Unpledge all partitions and process epoch refunds
    let mut unpledge_txs = Vec::with_capacity(assigned_partitions.len());
    for assignment in &assigned_partitions {
        let anchor = peer_node.get_anchor().await?;
        let mut unpledge = CommitmentTransaction::new_unpledge(
            &consensus,
            anchor,
            peer_node.node_ctx.mempool_pledge_provider.as_ref(),
            peer_addr,
            assignment.partition_hash,
        )
        .await;
        let signer = peer_node.cfg.signer();
        signer
            .sign_commitment(&mut unpledge)
            .expect("sign unpledge commitment");

        genesis_node.post_commitment_tx(&unpledge).await?;
        genesis_node
            .wait_for_mempool(unpledge.id(), seconds_to_wait)
            .await?;
        unpledge_txs.push(unpledge);
    }

    // Mine inclusion block for unpledges (keeps assignments intact until epoch).
    let unpledge_inclusion = genesis_node.mine_block().await?;
    let unpledge_block = genesis_node
        .get_block_by_height(unpledge_inclusion.height)
        .await?;

    let inclusion_commitments = unpledge_block.commitment_tx_ids();
    for tx in &unpledge_txs {
        assert!(
            inclusion_commitments.contains(&tx.id()),
            "Unpledge commitment {} missing from inclusion block",
            tx.id()
        );
    }

    // Advance to epoch boundary so pledges fully clear before unstaking.
    let (_mined, epoch_height_after_unpledge) = genesis_node.mine_until_next_epoch().await?;
    peer_node
        .wait_for_block_at_height(epoch_height_after_unpledge, seconds_to_wait)
        .await
        .expect("peer to sync post-unpledge epoch");
    let post_unpledge_epoch_block = genesis_node
        .get_block_by_height(epoch_height_after_unpledge)
        .await?;

    // Epoch block must emit refunds for each unpledge.
    let epoch_receipts = get_block_receipts(
        &genesis_node,
        &reth_ctx,
        post_unpledge_epoch_block.height,
        post_unpledge_epoch_block.evm_block_hash,
        seconds_to_wait,
    )
    .await?;
    let mut refund_logs = 0_usize;
    for receipt in &epoch_receipts {
        refund_logs += receipt
            .logs
            .iter()
            .filter(|log| log.topics()[0] == *shadow_tx_topics::UNPLEDGE_REFUND)
            .count();
    }
    assert_eq!(
        refund_logs,
        unpledge_txs.len(),
        "Expected one UNPLEDGE_REFUND log per drained pledge"
    );

    // Ensure storage modules are empty and pledge count is zero after the epoch.
    let post_unpledge_assignments: Vec<_> = {
        let sms = peer_node.node_ctx.storage_modules_guard.read();
        sms.iter()
            .filter_map(|sm| sm.partition_assignment())
            .collect()
    };
    assert!(
        post_unpledge_assignments.is_empty(),
        "All capacity assignments must be released before attempting unstake"
    );

    let pledge_count_after_unpledge = peer_node
        .node_ctx
        .mempool_pledge_provider
        .as_ref()
        .pledge_count(peer_addr)
        .await;
    assert_eq!(
        pledge_count_after_unpledge, 0,
        "Pledge count must reach zero before unstake"
    );

    // Submit unstake commitment and validate inclusion semantics
    let anchor = peer_node.get_anchor().await?;
    let mut unstake_tx = CommitmentTransaction::new_unstake(&consensus, anchor);
    let expected_refund_amount: U256 = unstake_tx.value();
    let signer = peer_node.cfg.signer();
    signer
        .sign_commitment(&mut unstake_tx)
        .expect("sign unstake commitment");
    let expected_irys_ref: FixedBytes<32> = unstake_tx.id().into();

    let head_height = genesis_node.get_canonical_chain_height().await;
    let head_block = genesis_node.get_block_by_height(head_height).await?;
    let balance_before_unstake = genesis_node
        .get_balance(peer_addr, head_block.evm_block_hash)
        .await;
    let treasury_before_unstake = head_block.treasury;

    genesis_node.post_commitment_tx(&unstake_tx).await?;
    genesis_node
        .wait_for_mempool(unstake_tx.id(), seconds_to_wait)
        .await?;

    let unstake_inclusion = genesis_node.mine_block().await?;
    peer_node
        .wait_for_block_at_height(unstake_inclusion.height, seconds_to_wait)
        .await
        .expect("peer should sync unstake inclusion block");
    let unstake_block = genesis_node
        .get_block_by_height(unstake_inclusion.height)
        .await?;

    assert_commitment_in_ledger(
        &unstake_block,
        unstake_tx.id(),
        "Unstake commitment must appear in the inclusion ledger",
    );

    let receipts_inclusion = get_block_receipts(
        &genesis_node,
        &reth_ctx,
        unstake_block.height,
        unstake_block.evm_block_hash,
        seconds_to_wait,
    )
    .await?;
    let debit_receipt_idx = assert_single_log_for(
        &receipts_inclusion,
        &shadow_tx_topics::UNSTAKE_DEBIT,
        peer_addr,
        "inclusion UNSTAKE_DEBIT",
    );
    assert_no_shadow_tx_log(
        &receipts_inclusion,
        &shadow_tx_topics::UNSTAKE,
        peer_addr,
        "Inclusion block must not emit UNSTAKE refund logs",
    );

    let debit_receipt = &receipts_inclusion[debit_receipt_idx];
    assert!(
        debit_receipt.success,
        "Unstake debit shadow tx must succeed"
    );
    assert_eq!(
        debit_receipt.cumulative_gas_used, 0,
        "Unstake debit shadow tx should not consume gas"
    );

    let txs_inclusion = reth_ctx
        .inner
        .provider
        .transactions_by_block(HashOrNumber::Hash(unstake_block.evm_block_hash))?
        .expect("transactions should exist for unstake inclusion block");
    assert_unstake_debit_packet(&txs_inclusion, peer_addr, expected_irys_ref);

    assert_balance(
        &genesis_node,
        peer_addr,
        unstake_block.evm_block_hash,
        balance_before_unstake - U256::from(unstake_tx.fee()),
        "Unstake inclusion should reduce balance by priority fee only",
    )
    .await;
    assert_treasury(
        &unstake_block,
        treasury_before_unstake,
        "Treasury must remain unchanged during unstake inclusion",
    );

    // Advance to epoch and validate refunds + treasury delta
    let (_produced, epoch_height_after_unstake) = genesis_node.mine_until_next_epoch().await?;
    peer_node
        .wait_for_block_at_height(epoch_height_after_unstake, seconds_to_wait)
        .await
        .expect("peer to sync unstake refund epoch");
    let epoch_block = genesis_node
        .get_block_by_height(epoch_height_after_unstake)
        .await?;

    let receipts_epoch = get_block_receipts(
        &genesis_node,
        &reth_ctx,
        epoch_block.height,
        epoch_block.evm_block_hash,
        seconds_to_wait,
    )
    .await?;
    let refund_receipt_idx = assert_single_log_for(
        &receipts_epoch,
        &shadow_tx_topics::UNSTAKE,
        peer_addr,
        "epoch UNSTAKE refund",
    );
    assert_no_shadow_tx_log(
        &receipts_epoch,
        &shadow_tx_topics::UNSTAKE_DEBIT,
        peer_addr,
        "Epoch block must not contain UNSTAKE_DEBIT logs",
    );

    let refund_receipt = &receipts_epoch[refund_receipt_idx];
    assert!(
        refund_receipt.success,
        "Unstake refund shadow tx must succeed"
    );
    assert_eq!(
        refund_receipt.cumulative_gas_used, 0,
        "Unstake refund shadow tx should not consume gas"
    );

    let epoch_txs = reth_ctx
        .inner
        .provider
        .transactions_by_block(HashOrNumber::Hash(epoch_block.evm_block_hash))?
        .expect("epoch block should include refund transactions");
    assert_unstake_refund_packet(
        &epoch_txs,
        peer_addr,
        expected_refund_amount,
        expected_irys_ref,
    );

    let epoch_prev = genesis_node
        .get_block_by_height(epoch_block.height - 1)
        .await?;
    assert_treasury(
        &epoch_block,
        epoch_prev.treasury - expected_refund_amount,
        "Treasury must decrease exactly by the stake refund amount",
    );

    let expected_final_balance =
        balance_before_unstake - U256::from(unstake_tx.fee()) + expected_refund_amount;
    assert_balance(
        &genesis_node,
        peer_addr,
        epoch_block.evm_block_hash,
        expected_final_balance,
        "Net effect should be -fee at inclusion + full refund at epoch",
    )
    .await;

    assert_no_stake_in_epoch(
        &genesis_node,
        peer_addr,
        "Epoch snapshot must remove the stake after refunding",
    );

    genesis_node.stop().await;
    peer_node.stop().await;
    Ok(())
}

/// Test scenario where an unstake transaction is submitted while the account still has active pledges.
/// Expected behavior:
/// 1. Unstake transaction is accepted by the mempool (not rejected at ingress)
/// 2. Unstake transaction is NOT included in any block while pledges are active
/// 3. No balance changes or treasury changes occur from the unstake attempt
/// 4. After all pledges are cleared, the same unstake transaction can be included and processed normally
#[test_log::test(tokio::test)]
async fn heavy_unstake_rejected_with_active_pledge() -> eyre::Result<()> {
    initialize_tracing();

    let num_blocks_in_epoch = 2_u64;
    let seconds_to_wait = 20_usize;
    let (genesis_node, peer_node, peer_addr, _initial_assignment, consensus) =
        setup_env(num_blocks_in_epoch, seconds_to_wait).await?;

    let reth_ctx = genesis_node.node_ctx.reth_node_adapter.clone();

    // Collect all pledged partitions - peer starts with 3 active pledges
    let assigned_partitions: Vec<PartitionAssignment> = {
        let sms = peer_node.node_ctx.storage_modules_guard.read();
        sms.iter()
            .filter_map(|sm| sm.partition_assignment())
            .collect()
    };
    assert!(
        !assigned_partitions.is_empty(),
        "Test requires the peer to start with at least one pledged partition"
    );

    // Submit unstake while pledges are still active
    let anchor = peer_node.get_anchor().await?;
    let mut unstake_tx = CommitmentTransaction::new_unstake(&consensus, anchor);
    let _expected_refund_amount: U256 = unstake_tx.value();
    let signer = peer_node.cfg.signer();
    signer
        .sign_commitment(&mut unstake_tx)
        .expect("sign unstake commitment");

    let head_height = genesis_node.get_canonical_chain_height().await;
    let head_block = genesis_node.get_block_by_height(head_height).await?;
    let balance_before_unstake_attempt = genesis_node
        .get_balance(peer_addr, head_block.evm_block_hash)
        .await;
    let treasury_before_unstake_attempt = head_block.treasury;

    // Submit the unstake transaction (should be accepted by mempool)
    genesis_node.post_commitment_tx(&unstake_tx).await?;
    genesis_node
        .wait_for_mempool(unstake_tx.id(), seconds_to_wait)
        .await?;

    // Mine a block - unstake should NOT be included due to HasActivePledges
    let first_block_after_unstake = genesis_node.mine_block().await?;
    peer_node
        .wait_for_block_at_height(first_block_after_unstake.height, seconds_to_wait)
        .await
        .expect("peer should sync first block after unstake attempt");
    let first_block = genesis_node
        .get_block_by_height(first_block_after_unstake.height)
        .await?;

    // Assert unstake is NOT in the commitment ledger
    assert_commitment_not_in_ledger(
        &first_block,
        unstake_tx.id(),
        "Unstake commitment must NOT be included in block while pledges are active",
    );

    // Assert no UNSTAKE_DEBIT shadow tx in block
    let receipts_first_block = get_block_receipts(
        &genesis_node,
        &reth_ctx,
        first_block.height,
        first_block.evm_block_hash,
        seconds_to_wait,
    )
    .await?;
    assert_no_shadow_tx_log(
        &receipts_first_block,
        &shadow_tx_topics::UNSTAKE_DEBIT,
        peer_addr,
        "First block must not contain UNSTAKE_DEBIT shadow tx while pledges active",
    );

    // Assert balance is unchanged (no fee deduction because tx wasn't included)
    assert_balance(
        &genesis_node,
        peer_addr,
        first_block.evm_block_hash,
        balance_before_unstake_attempt,
        "Balance must be unchanged when unstake is not included in block",
    )
    .await;

    // Assert treasury is unchanged
    assert_treasury(
        &first_block,
        treasury_before_unstake_attempt,
        "Treasury must be unchanged when unstake is not included in block",
    );

    // Verify unstake is NOT in the first block's commitment snapshot
    // Note: Commitment snapshot only tracks new commitments being added in the block,
    // not existing commitments from the epoch snapshot
    assert_no_unstake_in_commitment_snapshot(
        &genesis_node,
        first_block.block_hash,
        peer_addr,
        "Unstake must NOT be present in commitment snapshot (first block) while pledges are active",
    );

    // Advance to epoch boundary
    let (_mined, first_epoch_height) = genesis_node.mine_until_next_epoch().await?;
    peer_node
        .wait_for_block_at_height(first_epoch_height, seconds_to_wait)
        .await
        .expect("peer to sync first epoch after unstake attempt");
    let first_epoch_block = genesis_node.get_block_by_height(first_epoch_height).await?;

    // Assert no UNSTAKE refund shadow tx at epoch
    let receipts_first_epoch = get_block_receipts(
        &genesis_node,
        &reth_ctx,
        first_epoch_block.height,
        first_epoch_block.evm_block_hash,
        seconds_to_wait,
    )
    .await?;
    assert_no_shadow_tx_log(
        &receipts_first_epoch,
        &shadow_tx_topics::UNSTAKE,
        peer_addr,
        "Epoch block must not contain UNSTAKE refund for rejected unstake",
    );

    // Assert treasury is unchanged at epoch
    let epoch_prev = genesis_node
        .get_block_by_height(first_epoch_block.height - 1)
        .await?;
    assert_treasury(
        &first_epoch_block,
        epoch_prev.treasury,
        "Treasury must remain unchanged at epoch when unstake was not processed",
    );

    // Assert balance is still unchanged
    assert_balance(
        &genesis_node,
        peer_addr,
        first_epoch_block.evm_block_hash,
        balance_before_unstake_attempt,
        "Balance must remain unchanged at epoch when unstake was not processed",
    )
    .await;

    // Assert stake remains in epoch snapshot
    assert_stake_exists_in_epoch(
        &genesis_node,
        peer_addr,
        "Stake must still exist in epoch snapshot after rejected unstake",
    );

    // Verify unstake remains pending in mempool
    // The unstake transaction should still be in mempool, waiting for pledges to clear
    let mempool_unstake = genesis_node
        .get_commitment_tx_from_mempool(&unstake_tx.id())
        .await;
    assert!(
        mempool_unstake.is_ok(),
        "Unstake transaction must remain in mempool after being rejected from blocks"
    );

    // Mine more blocks - unstake should continue to be rejected
    // Mine another block to verify unstake continues to be rejected
    let second_block_after_unstake = genesis_node.mine_block().await?;
    peer_node
        .wait_for_block_at_height(second_block_after_unstake.height, seconds_to_wait)
        .await
        .expect("peer to sync second block");
    let second_block = genesis_node
        .get_block_by_height(second_block_after_unstake.height)
        .await?;

    // Verify unstake is still NOT included
    assert_commitment_not_in_ledger(
        &second_block,
        unstake_tx.id(),
        "Unstake must still not be included in subsequent blocks while pledges active",
    );

    // Verify unstake is NOT in the block's commitment snapshot (before epoch processing)
    assert_no_unstake_in_commitment_snapshot(
        &genesis_node,
        second_block.block_hash,
        peer_addr,
        "Unstake must NOT be present in commitment snapshot while pledges are active",
    );

    // Advance to another epoch - unstake should still not be processed
    let (_mined, second_epoch_height) = genesis_node.mine_until_next_epoch().await?;
    peer_node
        .wait_for_block_at_height(second_epoch_height, seconds_to_wait)
        .await
        .expect("peer to sync second epoch");
    let second_epoch_block = genesis_node
        .get_block_by_height(second_epoch_height)
        .await?;

    // Verify no UNSTAKE refund at this epoch either
    let receipts_second_epoch = get_block_receipts(
        &genesis_node,
        &reth_ctx,
        second_epoch_block.height,
        second_epoch_block.evm_block_hash,
        seconds_to_wait,
    )
    .await?;
    assert_no_shadow_tx_log(
        &receipts_second_epoch,
        &shadow_tx_topics::UNSTAKE,
        peer_addr,
        "Second epoch block must also not contain UNSTAKE refund",
    );

    // Verify stake still exists in epoch snapshot
    assert_stake_exists_in_epoch(
        &genesis_node,
        peer_addr,
        "Stake must still exist in epoch snapshot after multiple rejection attempts",
    );

    // Verify balance and treasury remain unchanged throughout
    assert_balance(
        &genesis_node,
        peer_addr,
        second_epoch_block.evm_block_hash,
        balance_before_unstake_attempt,
        "Balance must remain unchanged after multiple epochs with rejected unstake",
    )
    .await;

    genesis_node.stop().await;
    peer_node.stop().await;
    Ok(())
}

/// Test scenario where both a pledge and an unstake transaction are submitted to the mempool
/// for a staked account that has NO active pledges (all pledges were previously cleared).
/// Expected behavior:
/// 1. Both transactions are accepted by the mempool
/// 2. When a block is mined, the PLEDGE is included (priority 1) but UNSTAKE is rejected (priority 3)
/// 3. The simulation validation sees the pledge being added, which creates an active pledge that blocks the unstake
/// 4. This demonstrates transaction priority ordering: Pledge (1) < Unstake (3)
#[test_log::test(tokio::test)]
async fn heavy_unstake_rejected_with_pending_pledge() -> eyre::Result<()> {
    initialize_tracing();

    let num_blocks_in_epoch = 2_u64;
    let seconds_to_wait = 20_usize;
    let block_migration_depth = 2_u32;
    let (genesis_node, peer_node, peer_addr, _initial_assignment, consensus) =
        setup_env_with_block_migration_depth(
            num_blocks_in_epoch,
            seconds_to_wait,
            block_migration_depth,
        )
        .await?;
    let peer_signer = peer_node.node_ctx.config.irys_signer();

    let reth_ctx = genesis_node.node_ctx.reth_node_adapter.clone();

    let (_fee, _refund, _unpledges) = send_unpledge_all(
        seconds_to_wait,
        &genesis_node,
        &peer_node,
        consensus.clone(),
        &peer_signer,
        {
            let sms = peer_node.node_ctx.storage_modules_guard.read();
            sms.iter().cloned().collect::<Vec<_>>()
        },
    )
    .await?;
    let (_mined, epoch_height_1) = genesis_node
        .mine_until_next_epoch()
        .await
        .expect("failed to mine until next epoch (first)");
    peer_node
        .wait_for_block_at_height(epoch_height_1, seconds_to_wait)
        .await
        .expect("peer did not sync to epoch_height_1");
    let (_mined, epoch_height_2) = genesis_node
        .mine_until_next_epoch()
        .await
        .expect("failed to mine until next epoch (second)");
    peer_node
        .wait_for_block_at_height(epoch_height_2, seconds_to_wait)
        .await
        .expect("peer did not sync to epoch_height_2");

    // Verify peer has no pledges
    let assigned_partitions: Vec<PartitionAssignment> = {
        let sms = peer_node.node_ctx.storage_modules_guard.read();
        sms.iter()
            .filter_map(|sm| sm.partition_assignment())
            .collect()
    };
    tracing::debug!(?assigned_partitions);
    assert!(
        assigned_partitions.is_empty(),
        "Test requires the peer to have no pledges"
    );

    // Verify peer is staked
    assert_stake_exists_in_epoch(&genesis_node, peer_addr, "Peer must be staked");

    // Verify peer has active pledges
    let pledge_count = peer_node
        .node_ctx
        .mempool_pledge_provider
        .as_ref()
        .pledge_count(peer_addr)
        .await;
    assert!(pledge_count == 0, "Peer must have no active pledges");

    // Submit both pledge and unstake transactions
    let head_height = genesis_node.get_canonical_chain_height().await;
    let head_block = genesis_node.get_block_by_height(head_height).await?;
    let balance_before = genesis_node
        .get_balance(peer_addr, head_block.evm_block_hash)
        .await;
    let treasury_before = head_block.treasury;

    // Create and submit pledge transaction
    let anchor = genesis_node.get_anchor().await?;
    let mut pledge_tx = CommitmentTransaction::new_pledge(
        &consensus,
        anchor,
        peer_node.node_ctx.mempool_pledge_provider.as_ref(),
        peer_addr,
    )
    .await;
    let pledge_value = pledge_tx.value();
    let pledge_fee = pledge_tx.fee();
    peer_signer
        .sign_commitment(&mut pledge_tx)
        .expect("sign pledge commitment");

    tracing::debug!(
        tx.id = ?pledge_tx.id(),
        tx.commitment_type = ?pledge_tx.commitment_type(),
        tx.anchor = ?anchor,
        "Submitting NEW pledge transaction"
    );
    genesis_node.post_commitment_tx(&pledge_tx).await?;

    let pledge_mempool_result = genesis_node
        .wait_for_mempool(pledge_tx.id(), seconds_to_wait)
        .await;
    tracing::debug!(
        tx.pledge_mempool_result = ?pledge_mempool_result,
        "Pledge wait_for_mempool result"
    );
    pledge_mempool_result?;

    // Create and submit unstake transaction
    let mut unstake_tx = CommitmentTransaction::new_unstake(&consensus, anchor);
    let _unstake_value = unstake_tx.value();
    peer_signer
        .sign_commitment(&mut unstake_tx)
        .expect("sign unstake commitment");

    tracing::debug!(
        tx.id = ?unstake_tx.id(),
        tx.commitment_type = ?unstake_tx.commitment_type(),
        tx.anchor = ?anchor,
        "Submitting NEW unstake transaction"
    );
    genesis_node.post_commitment_tx(&unstake_tx).await?;

    let unstake_mempool_result = genesis_node
        .wait_for_mempool(unstake_tx.id(), seconds_to_wait)
        .await;
    tracing::debug!(
        tx.unstake_mempool_result = ?unstake_mempool_result,
        "Unstake wait_for_mempool result"
    );
    unstake_mempool_result?;

    tracing::debug!("Both transactions confirmed in mempool, now mining block");

    // Mine block and verify pledge is included, unstake is NOT
    let block = genesis_node.mine_block().await?;
    let block_header = genesis_node.get_block_by_height(block.height).await?;

    // Assert PLEDGE IS in the commitment ledger
    assert_commitment_in_ledger(
        &block_header,
        pledge_tx.id(),
        "Pledge commitment MUST be included (higher priority than unstake)",
    );

    // Assert UNSTAKE is NOT in the commitment ledger
    assert_commitment_not_in_ledger(
        &block_header,
        unstake_tx.id(),
        "Unstake commitment must NOT be included when pledge is being added in same block",
    );

    // Verify commitment snapshot: unstake should NOT be present
    assert_no_unstake_in_commitment_snapshot(
        &genesis_node,
        block_header.block_hash,
        peer_addr,
        "Unstake must NOT be in commitment snapshot when pledge is being added in same block",
    );

    // Verify shadow transactions
    let receipts = get_block_receipts(
        &genesis_node,
        &reth_ctx,
        block_header.height,
        block_header.evm_block_hash,
        seconds_to_wait,
    )
    .await?;

    // Verify PLEDGE shadow transaction exists
    let pledge_receipt_idx = assert_single_log_for(
        &receipts,
        &shadow_tx_topics::PLEDGE,
        peer_addr,
        "Block must contain PLEDGE shadow tx for included pledge",
    );
    let pledge_receipt = &receipts[pledge_receipt_idx];
    assert!(pledge_receipt.success, "Pledge shadow tx must succeed");

    // No UNSTAKE_DEBIT or UNSTAKE shadow transactions should exist
    assert_no_shadow_tx_log(
        &receipts,
        &shadow_tx_topics::UNSTAKE_DEBIT,
        peer_addr,
        "Block must not contain UNSTAKE_DEBIT when unstake was rejected",
    );
    assert_no_shadow_tx_log(
        &receipts,
        &shadow_tx_topics::UNSTAKE,
        peer_addr,
        "Block must not contain UNSTAKE refund when unstake was rejected",
    );

    // Verify balance and treasury reflect ONLY pledge processing
    // Treasury should increase by pledge value (no unstake refund)
    assert_treasury(
        &block_header,
        treasury_before + pledge_value,
        "Treasury should increase by pledge value only (unstake was rejected)",
    );

    // Balance should decrease by pledge value + pledge fee (no unstake fee charged)
    let expected_balance = balance_before - pledge_value - U256::from(pledge_fee);

    tracing::warn!(
        "balance_before: {:?}, pledge_value={:?}, pledge_fee={:?}",
        balance_before,
        pledge_value,
        pledge_fee
    );

    assert_balance(
        &genesis_node,
        peer_addr,
        block_header.evm_block_hash,
        expected_balance,
        "Balance should decrease by pledge value + fee only (unstake was rejected)",
    )
    .await;

    // Verify peer state after pledge inclusion
    // Peer should still have stake
    assert_stake_exists_in_epoch(
        &genesis_node,
        peer_addr,
        "Peer must still be staked after pledge inclusion",
    );

    // The pledge won't be in epoch snapshot yet (still within the same epoch)
    // It will be in the block's commitment snapshot instead
    let commitment_snapshot = {
        let tree = genesis_node.node_ctx.block_tree_guard.read();
        tree.get_commitment_snapshot(&block_header.block_hash)
            .expect("Block must have commitment snapshot")
    };

    let peer_commitments = commitment_snapshot
        .commitments
        .get(&peer_addr)
        .expect("Peer should have commitments in block's commitment snapshot");

    // Verify the pledge is in the commitment snapshot (not unstake)
    assert!(
        peer_commitments.unstake.is_none(),
        "Unstake should NOT be in commitment snapshot"
    );
    assert_eq!(
        peer_commitments.pledges.len(),
        1,
        "Exactly 1 pledge should be in commitment snapshot"
    );

    // Verify unstake remains in mempool
    let mempool_unstake = genesis_node
        .get_commitment_tx_from_mempool(&unstake_tx.id())
        .await;
    assert!(
        mempool_unstake.is_ok(),
        "Unstake transaction must remain in mempool after being rejected (contextually invalid)"
    );

    // Verify unstake continues to be rejected in next block
    let second_block = genesis_node.mine_block().await?;
    let second_block_header = genesis_node
        .get_block_by_height(second_block.height)
        .await?;

    // Unstake should still NOT be included because pledge is now active
    assert_commitment_not_in_ledger(
        &second_block_header,
        unstake_tx.id(),
        "Unstake must continue to be rejected in subsequent blocks while pledge is active",
    );

    // Verify unstake is NOT in commitment snapshot of second block either
    assert_no_unstake_in_commitment_snapshot(
        &genesis_node,
        second_block_header.block_hash,
        peer_addr,
        "Unstake must NOT be in commitment snapshot of second block while pledge is active",
    );

    // Balance should remain at expected_balance (no additional changes from unstake)
    assert_balance(
        &genesis_node,
        peer_addr,
        second_block_header.evm_block_hash,
        expected_balance,
        "Balance should remain unchanged from first block (no unstake processing in second block)",
    )
    .await;

    genesis_node.stop().await;
    peer_node.stop().await;
    Ok(())
}

/// Get receipts for a block
async fn get_block_receipts(
    node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    reth_ctx: &irys_reth_node_bridge::IrysRethNodeAdapter,
    block_height: u64,
    block_hash: FixedBytes<32>,
    seconds_to_wait: usize,
) -> eyre::Result<Vec<reth_ethereum_primitives::Receipt>> {
    node.wait_for_reth_marker(
        BlockNumberOrTag::Number(block_height),
        block_hash,
        seconds_to_wait as u64,
    )
    .await?;

    let poll_interval = Duration::from_millis(200);
    let max_retries = ((seconds_to_wait * 1000) / poll_interval.as_millis() as usize).max(1);

    for retry in 0..=max_retries {
        if let Some(receipts) = reth_ctx
            .inner
            .provider
            .receipts_by_block(HashOrNumber::Hash(block_hash))?
        {
            return Ok(receipts);
        }

        if retry == max_retries {
            break;
        }
        sleep(poll_interval).await;
    }

    Err(eyre::eyre!(
        "Receipts not found for block {:?} at height {} after {} retries ({}s)",
        block_hash,
        block_height,
        max_retries,
        seconds_to_wait
    ))
}

/// Assert that a specific shadow tx log is NOT present in receipts
fn assert_no_shadow_tx_log(
    receipts: &[reth_ethereum_primitives::Receipt],
    topic: &[u8; 32],
    address: irys_types::IrysAddress,
    context: &str,
) {
    let found = receipts.iter().any(|r| {
        r.logs
            .iter()
            .any(|log| log.topics()[0] == *topic && log.address == address.to_alloy_address())
    });
    assert!(!found, "{}: shadow tx log must NOT be present", context);
}

/// Assert balance equals expected value
async fn assert_balance(
    node: &crate::utils::IrysNodeTest<irys_chain::IrysNodeCtx>,
    address: irys_types::IrysAddress,
    block_hash: FixedBytes<32>,
    expected: U256,
    message: &str,
) {
    let balance = node.get_balance(address, block_hash).await;
    assert_eq!(balance, expected, "{}", message);
}

/// Assert treasury equals expected value
fn assert_treasury(block: &irys_types::IrysBlockHeader, expected: U256, message: &str) {
    assert_eq!(block.treasury, expected, "{}", message);
}

/// Assert commitment is in the block's commitment ledger
fn assert_commitment_in_ledger(
    block: &irys_types::IrysBlockHeader,
    tx_id: irys_types::H256,
    message: &str,
) {
    let commitments = block.commitment_tx_ids();
    assert!(commitments.contains(&tx_id), "{}", message);
}

/// Assert commitment is NOT in the block's commitment ledger
fn assert_commitment_not_in_ledger(
    block: &irys_types::IrysBlockHeader,
    tx_id: irys_types::H256,
    message: &str,
) {
    let commitments = block.commitment_tx_ids();
    assert!(!commitments.contains(&tx_id), "{}", message);
}

/// Assert stake exists for address in canonical epoch snapshot
fn assert_stake_exists_in_epoch(
    node: &crate::utils::IrysNodeTest<irys_chain::IrysNodeCtx>,
    address: irys_types::IrysAddress,
    message: &str,
) {
    let epoch_snapshot = node
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot();
    assert!(
        epoch_snapshot
            .commitment_state
            .stake_commitments
            .contains_key(&address),
        "{}",
        message
    );
}

/// Assert stake does NOT exist for address in canonical epoch snapshot
fn assert_no_stake_in_epoch(
    node: &crate::utils::IrysNodeTest<irys_chain::IrysNodeCtx>,
    address: irys_types::IrysAddress,
    message: &str,
) {
    let epoch_snapshot = node
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot();
    assert!(
        !epoch_snapshot
            .commitment_state
            .stake_commitments
            .contains_key(&address),
        "{}",
        message
    );
}

/// Find and validate UnstakeDebit shadow transaction packet
fn assert_unstake_debit_packet<T: alloy_rpc_types_eth::TransactionTrait>(
    transactions: &[T],
    peer_addr: irys_types::IrysAddress,
    expected_irys_ref: FixedBytes<32>,
) {
    let mut found = false;
    for tx in transactions {
        if let Ok(shadow_tx) = ShadowTransaction::decode(&mut tx.input().as_ref()) {
            if let Some(TransactionPacket::UnstakeDebit(debit)) = shadow_tx.as_v1() {
                if debit.target == peer_addr.to_alloy_address() {
                    assert_eq!(
                        debit.irys_ref, expected_irys_ref,
                        "Unstake debit irys_ref must match commitment id"
                    );
                    found = true;
                    break;
                }
            }
        }
    }
    assert!(
        found,
        "Block must contain UNSTAKE_DEBIT packet for address {:?}",
        peer_addr
    );
}

/// Find and validate UnstakeRefund shadow transaction packet
fn assert_unstake_refund_packet<T: alloy_rpc_types_eth::TransactionTrait>(
    transactions: &[T],
    peer_addr: irys_types::IrysAddress,
    expected_amount: U256,
    expected_irys_ref: FixedBytes<32>,
) {
    let mut found = false;
    for tx in transactions {
        if let Ok(shadow_tx) = ShadowTransaction::decode(&mut tx.input().as_ref()) {
            if let Some(TransactionPacket::UnstakeRefund(increment)) = shadow_tx.as_v1() {
                if increment.target == peer_addr.to_alloy_address() {
                    let expected_amount_alloy: alloy_primitives::U256 = expected_amount.into();
                    assert_eq!(
                        increment.amount, expected_amount_alloy,
                        "Unstake refund amount must equal configured stake value"
                    );
                    assert_eq!(
                        increment.irys_ref, expected_irys_ref,
                        "Unstake refund irys_ref must match commitment id"
                    );
                    found = true;
                    break;
                }
            }
        }
    }
    assert!(
        found,
        "Block must contain UNSTAKE refund packet for address {:?}",
        peer_addr
    );
}

/// Assert that unstake is NOT present in block's commitment snapshot
fn assert_no_unstake_in_commitment_snapshot(
    node: &crate::utils::IrysNodeTest<irys_chain::IrysNodeCtx>,
    block_hash: irys_types::BlockHash,
    address: irys_types::IrysAddress,
    message: &str,
) {
    let commitment_snapshot = {
        let tree = node.node_ctx.block_tree_guard.read();
        tree.get_commitment_snapshot(&block_hash)
            .expect("Block must have commitment snapshot")
    };

    if let Some(commitments) = commitment_snapshot.commitments.get(&address) {
        assert!(commitments.unstake.is_none(), "{}", message);
    }
    // If no commitments exist for address, that's also valid
}

/// Test scenario where unpledge transactions for all partitions and an unstake transaction
/// are both submitted to the mempool concurrently while the account has active pledges.
/// Expected behavior:
/// 1. Both unpledge and unstake transactions are accepted by the mempool
/// 2. When mining to the next epoch, both are processed successfully
/// 3. Unpledges are processed (priority 2), clearing all active pledges
/// 4. Unstake is processed (priority 3), removing the stake
/// 5. Treasury decreases by total unpledge refunds + unstake refund
/// 6. Balance increases by total unpledge refunds + unstake refund - fees
/// 7. No storage modules remain assigned (all pledges cleared)
/// 8. User is no longer staked (removed from epoch snapshot)
#[test_log::test(tokio::test)]
async fn heavy3_unpledge_and_unstake_concurrent_success_flow() -> eyre::Result<()> {
    initialize_tracing();

    let num_blocks_in_epoch = 2_u64;
    let seconds_to_wait = 20_usize;
    let (genesis_node, peer_node, peer_addr, _initial_assignment, consensus) =
        setup_env(num_blocks_in_epoch, seconds_to_wait).await?;

    let reth_ctx = genesis_node.node_ctx.reth_node_adapter.clone();

    // Collect all capacity assignments so we can drain every active pledge
    let assigned_partitions: Vec<PartitionAssignment> = {
        let sms = peer_node.node_ctx.storage_modules_guard.read();
        sms.iter()
            .filter_map(|sm| sm.partition_assignment())
            .collect()
    };
    assert!(
        !assigned_partitions.is_empty(),
        "Test requires the peer to start with at least one pledged partition"
    );

    let initial_pledge_count = peer_node
        .node_ctx
        .mempool_pledge_provider
        .as_ref()
        .pledge_count(peer_addr)
        .await;
    assert!(
        initial_pledge_count > 0,
        "Peer must have active pledges at start"
    );

    // Verify peer is staked
    assert_stake_exists_in_epoch(&genesis_node, peer_addr, "Peer must be staked");

    let head_height = genesis_node.get_canonical_chain_height().await;
    let head_block = genesis_node.get_block_by_height(head_height).await?;
    let balance_before = genesis_node
        .get_balance(peer_addr, head_block.evm_block_hash)
        .await;

    // Submit all unpledge transactions
    let mut unpledge_txs = Vec::with_capacity(assigned_partitions.len());
    let mut total_unpledge_fee = U256::from(0_u64);
    let mut total_unpledge_refund = U256::from(0_u64);

    for (idx, assignment) in assigned_partitions.iter().enumerate() {
        let anchor = peer_node.get_anchor().await?;
        let pledge_count = initial_pledge_count - (idx as u64);
        let mut unpledge = CommitmentTransaction::new_unpledge(
            &consensus,
            anchor,
            &pledge_count,
            peer_addr,
            assignment.partition_hash,
        )
        .await;
        let signer = peer_node.cfg.signer();
        signer
            .sign_commitment(&mut unpledge)
            .expect("sign unpledge commitment");

        total_unpledge_fee += U256::from(unpledge.fee());
        total_unpledge_refund += unpledge.value();

        genesis_node.post_commitment_tx(&unpledge).await?;
        genesis_node
            .wait_for_mempool(unpledge.id(), seconds_to_wait)
            .await?;
        unpledge_txs.push(unpledge);
    }

    // Submit unstake transaction
    let anchor = peer_node.get_anchor().await?;
    let mut unstake_tx = CommitmentTransaction::new_unstake(&consensus, anchor);
    let unstake_refund_amount: U256 = unstake_tx.value();
    let signer = peer_node.cfg.signer();
    signer
        .sign_commitment(&mut unstake_tx)
        .expect("sign unstake commitment");
    let expected_irys_ref: FixedBytes<32> = unstake_tx.id().into();

    genesis_node.post_commitment_tx(&unstake_tx).await?;
    genesis_node
        .wait_for_mempool(unstake_tx.id(), seconds_to_wait)
        .await?;

    let total_fees = total_unpledge_fee + U256::from(unstake_tx.fee());
    let total_refunds = total_unpledge_refund + unstake_refund_amount;

    // Mine until next epoch - this should process both unpledges and unstake
    let (_mined, epoch_height) = genesis_node.mine_until_next_epoch().await?;
    peer_node
        .wait_for_block_at_height(epoch_height, seconds_to_wait)
        .await
        .expect("peer to sync to epoch");
    let epoch_block = genesis_node.get_block_by_height(epoch_height).await?;

    // Assert epoch block contains UNPLEDGE_REFUND logs
    let receipts_epoch = get_block_receipts(
        &genesis_node,
        &reth_ctx,
        epoch_block.height,
        epoch_block.evm_block_hash,
        seconds_to_wait,
    )
    .await?;
    let mut unpledge_refund_logs = 0_usize;
    let mut unstake_refund_logs = 0_usize;

    for receipt in &receipts_epoch {
        unpledge_refund_logs += receipt
            .logs
            .iter()
            .filter(|log| {
                log.topics()[0] == *shadow_tx_topics::UNPLEDGE_REFUND
                    && log.address == peer_addr.to_alloy_address()
            })
            .count();
        unstake_refund_logs += receipt
            .logs
            .iter()
            .filter(|log| {
                log.topics()[0] == *shadow_tx_topics::UNSTAKE
                    && log.address == peer_addr.to_alloy_address()
            })
            .count();
    }
    assert_eq!(
        unpledge_refund_logs,
        unpledge_txs.len(),
        "Expected one UNPLEDGE_REFUND log per unpledge transaction"
    );
    assert_eq!(
        unstake_refund_logs, 1,
        "Expected exactly one UNSTAKE refund log"
    );

    // Decode and verify unpledge refund packets
    let epoch_txs = reth_ctx
        .inner
        .provider
        .transactions_by_block(HashOrNumber::Hash(epoch_block.evm_block_hash))?
        .expect("epoch block should have transactions");

    let mut found_unpledge_refunds = 0_usize;
    let mut found_unstake_refund = false;

    for tx in &epoch_txs {
        if let Ok(shadow_tx) = ShadowTransaction::decode(&mut tx.input().as_ref()) {
            if let Some(TransactionPacket::UnpledgeRefund(refund)) = shadow_tx.as_v1() {
                if refund.target == peer_addr.to_alloy_address() {
                    found_unpledge_refunds += 1;
                }
            } else if let Some(TransactionPacket::UnstakeRefund(refund)) = shadow_tx.as_v1() {
                if refund.target == peer_addr.to_alloy_address() {
                    let refund_amount_alloy: alloy_primitives::U256 = unstake_refund_amount.into();
                    assert_eq!(
                        refund.amount, refund_amount_alloy,
                        "Unstake refund amount must match stake value"
                    );
                    assert_eq!(
                        refund.irys_ref, expected_irys_ref,
                        "Unstake refund irys_ref must match commitment id"
                    );
                    found_unstake_refund = true;
                }
            }
        }
    }
    assert_eq!(
        found_unpledge_refunds,
        unpledge_txs.len(),
        "Must find all unpledge refund packets"
    );
    assert!(
        found_unstake_refund,
        "Must find unstake refund packet in epoch block"
    );

    // Verify treasury decreased by total refunds
    let epoch_prev = genesis_node
        .get_block_by_height(epoch_block.height - 1)
        .await?;
    assert_treasury(
        &epoch_block,
        epoch_prev.treasury - total_refunds,
        "Treasury must decrease by total unpledge refunds + unstake refund",
    );

    // Verify balance increased by refunds minus fees
    let expected_final_balance = balance_before - total_fees + total_refunds;
    assert_balance(
        &genesis_node,
        peer_addr,
        epoch_block.evm_block_hash,
        expected_final_balance,
        "Balance should equal initial - fees + total refunds",
    )
    .await;

    // Verify all storage modules are unassigned
    let post_epoch_assignments: Vec<_> = {
        let sms = peer_node.node_ctx.storage_modules_guard.read();
        sms.iter()
            .filter_map(|sm| sm.partition_assignment())
            .collect()
    };
    assert!(
        post_epoch_assignments.is_empty(),
        "All storage modules must be unassigned after unpledge refunds"
    );

    // Verify pledge count is zero
    let pledge_count_after = peer_node
        .node_ctx
        .mempool_pledge_provider
        .as_ref()
        .pledge_count(peer_addr)
        .await;
    assert_eq!(
        pledge_count_after, 0,
        "Pledge count must be zero after unpledge refunds"
    );

    // Verify stake is removed from epoch snapshot
    assert_no_stake_in_epoch(
        &genesis_node,
        peer_addr,
        "Stake must be removed from epoch snapshot after unstake refund",
    );

    genesis_node.stop().await;
    peer_node.stop().await;
    Ok(())
}
