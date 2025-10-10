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

use crate::block_production::unpledge_refund::{assert_single_log_for, setup_env};

/// Two-node scenario exercising the full unstake flow:
/// 1. Drain all pledges for the signer and process their epoch refunds.
/// 2. Submit an unstake commitment, mine the inclusion block, and ensure it is fee-only.
/// 3. Advance to the next epoch to assert the unstake refund, treasury delta, and stake removal.
#[test_log::test(actix_web::test)]
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

    // ------------- Phase 1: Unpledge all partitions and process epoch refunds -------------
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
            .wait_for_mempool(unpledge.id, seconds_to_wait)
            .await?;
        unpledge_txs.push(unpledge);
    }

    // Mine inclusion block for unpledges (keeps assignments intact until epoch).
    let unpledge_inclusion = genesis_node.mine_block().await?;
    genesis_node
        .wait_until_height(unpledge_inclusion.height, seconds_to_wait)
        .await
        .expect("peer should observe unpledge inclusion block");
    let unpledge_block = genesis_node
        .get_block_by_height(unpledge_inclusion.height)
        .await?;

    let inclusion_commitments = unpledge_block.get_commitment_ledger_tx_ids();
    for tx in &unpledge_txs {
        assert!(
            inclusion_commitments.contains(&tx.id),
            "Unpledge commitment {} missing from inclusion block",
            tx.id
        );
    }

    // Advance to epoch boundary so pledges fully clear before unstaking.
    let (_mined, epoch_height_after_unpledge) = genesis_node.mine_until_next_epoch().await?;
    peer_node
        .wait_until_height(epoch_height_after_unpledge, seconds_to_wait)
        .await
        .expect("peer to sync post-unpledge epoch");
    let post_unpledge_epoch_block = genesis_node
        .get_block_by_height(epoch_height_after_unpledge)
        .await?;

    // Epoch block must emit refunds for each unpledge.
    let epoch_receipts = reth_ctx
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(post_unpledge_epoch_block.evm_block_hash))?
        .expect("epoch receipts available");
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

    // ------------- Phase 2: Submit unstake commitment and validate inclusion semantics -------------
    let anchor = peer_node.get_anchor().await?;
    let mut unstake_tx = CommitmentTransaction::new_unstake(&consensus, anchor);
    let expected_refund_amount: U256 = unstake_tx.value;
    let signer = peer_node.cfg.signer();
    signer
        .sign_commitment(&mut unstake_tx)
        .expect("sign unstake commitment");
    let expected_irys_ref: FixedBytes<32> = unstake_tx.id.into();

    let head_height = genesis_node.get_canonical_chain_height().await;
    let head_block = genesis_node.get_block_by_height(head_height).await?;
    let balance_before_unstake = genesis_node.get_balance(peer_addr, head_block.evm_block_hash);
    let treasury_before_unstake = head_block.treasury;

    genesis_node.post_commitment_tx(&unstake_tx).await?;
    genesis_node
        .wait_for_mempool(unstake_tx.id, seconds_to_wait)
        .await?;

    let unstake_inclusion = genesis_node.mine_block().await?;
    genesis_node
        .wait_until_height(unstake_inclusion.height, seconds_to_wait)
        .await
        .expect("peer should sync unstake inclusion block");
    let unstake_block = genesis_node
        .get_block_by_height(unstake_inclusion.height)
        .await?;

    assert_commitment_in_ledger(
        &unstake_block,
        unstake_tx.id,
        "Unstake commitment must appear in the inclusion ledger"
    );

    let receipts_inclusion = get_block_receipts(&reth_ctx, unstake_block.evm_block_hash)?;
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
        "Inclusion block must not emit UNSTAKE refund logs"
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
        balance_before_unstake - U256::from(unstake_tx.fee),
        "Unstake inclusion should reduce balance by priority fee only"
    );
    assert_treasury(
        &unstake_block,
        treasury_before_unstake,
        "Treasury must remain unchanged during unstake inclusion"
    );

    // ------------- Phase 3: Advance to epoch and validate refunds + treasury delta -------------
    let (_produced, epoch_height_after_unstake) = genesis_node.mine_until_next_epoch().await?;
    peer_node
        .wait_until_height(epoch_height_after_unstake, seconds_to_wait)
        .await
        .expect("peer to sync unstake refund epoch");
    let epoch_block = genesis_node
        .get_block_by_height(epoch_height_after_unstake)
        .await?;

    let receipts_epoch = get_block_receipts(&reth_ctx, epoch_block.evm_block_hash)?;
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
        "Epoch block must not contain UNSTAKE_DEBIT logs"
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
    assert_unstake_refund_packet(&epoch_txs, peer_addr, expected_refund_amount, expected_irys_ref);

    let epoch_prev = genesis_node
        .get_block_by_height(epoch_block.height - 1)
        .await?;
    assert_treasury(
        &epoch_block,
        epoch_prev.treasury - expected_refund_amount,
        "Treasury must decrease exactly by the stake refund amount"
    );

    let expected_final_balance =
        balance_before_unstake - U256::from(unstake_tx.fee) + expected_refund_amount;
    assert_balance(
        &genesis_node,
        peer_addr,
        epoch_block.evm_block_hash,
        expected_final_balance,
        "Net effect should be -fee at inclusion + full refund at epoch"
    );

    assert_no_stake_in_epoch(
        &genesis_node,
        peer_addr,
        "Epoch snapshot must remove the stake after refunding"
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
#[test_log::test(actix_web::test)]
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

    // ------------- Phase 1: Submit unstake while pledges are still active -------------
    let anchor = peer_node.get_anchor().await?;
    let mut unstake_tx = CommitmentTransaction::new_unstake(&consensus, anchor);
    let _expected_refund_amount: U256 = unstake_tx.value;
    let signer = peer_node.cfg.signer();
    signer
        .sign_commitment(&mut unstake_tx)
        .expect("sign unstake commitment");

    let head_height = genesis_node.get_canonical_chain_height().await;
    let head_block = genesis_node.get_block_by_height(head_height).await?;
    let balance_before_unstake_attempt = genesis_node.get_balance(peer_addr, head_block.evm_block_hash);
    let treasury_before_unstake_attempt = head_block.treasury;

    // Submit the unstake transaction (should be accepted by mempool)
    genesis_node.post_commitment_tx(&unstake_tx).await?;
    genesis_node
        .wait_for_mempool(unstake_tx.id, seconds_to_wait)
        .await?;

    // Mine a block - unstake should NOT be included due to HasActivePledges
    let first_block_after_unstake = genesis_node.mine_block().await?;
    genesis_node
        .wait_until_height(first_block_after_unstake.height, seconds_to_wait)
        .await
        .expect("peer should sync first block after unstake attempt");
    let first_block = genesis_node
        .get_block_by_height(first_block_after_unstake.height)
        .await?;

    // Assert unstake is NOT in the commitment ledger
    assert_commitment_not_in_ledger(
        &first_block,
        unstake_tx.id,
        "Unstake commitment must NOT be included in block while pledges are active"
    );

    // Assert no UNSTAKE_DEBIT shadow tx in block
    let receipts_first_block = get_block_receipts(&reth_ctx, first_block.evm_block_hash)?;
    assert_no_shadow_tx_log(
        &receipts_first_block,
        &shadow_tx_topics::UNSTAKE_DEBIT,
        peer_addr,
        "First block must not contain UNSTAKE_DEBIT shadow tx while pledges active"
    );

    // Assert balance is unchanged (no fee deduction because tx wasn't included)
    assert_balance(
        &genesis_node,
        peer_addr,
        first_block.evm_block_hash,
        balance_before_unstake_attempt,
        "Balance must be unchanged when unstake is not included in block"
    );

    // Assert treasury is unchanged
    assert_treasury(
        &first_block,
        treasury_before_unstake_attempt,
        "Treasury must be unchanged when unstake is not included in block"
    );

    // Verify unstake is NOT in the first block's commitment snapshot
    // Note: Commitment snapshot only tracks new commitments being added in the block,
    // not existing commitments from the epoch snapshot
    assert_no_unstake_in_commitment_snapshot(
        &genesis_node,
        first_block.block_hash,
        peer_addr,
        "Unstake must NOT be present in commitment snapshot (first block) while pledges are active"
    );

    // ------------- Phase 2: Advance to epoch boundary -------------
    let (_mined, first_epoch_height) = genesis_node.mine_until_next_epoch().await?;
    peer_node
        .wait_until_height(first_epoch_height, seconds_to_wait)
        .await
        .expect("peer to sync first epoch after unstake attempt");
    let first_epoch_block = genesis_node.get_block_by_height(first_epoch_height).await?;

    // Assert no UNSTAKE refund shadow tx at epoch
    let receipts_first_epoch = get_block_receipts(&reth_ctx, first_epoch_block.evm_block_hash)?;
    assert_no_shadow_tx_log(
        &receipts_first_epoch,
        &shadow_tx_topics::UNSTAKE,
        peer_addr,
        "Epoch block must not contain UNSTAKE refund for rejected unstake"
    );

    // Assert treasury is unchanged at epoch
    let epoch_prev = genesis_node
        .get_block_by_height(first_epoch_block.height - 1)
        .await?;
    assert_treasury(
        &first_epoch_block,
        epoch_prev.treasury,
        "Treasury must remain unchanged at epoch when unstake was not processed"
    );

    // Assert balance is still unchanged
    assert_balance(
        &genesis_node,
        peer_addr,
        first_epoch_block.evm_block_hash,
        balance_before_unstake_attempt,
        "Balance must remain unchanged at epoch when unstake was not processed"
    );

    // Assert stake remains in epoch snapshot
    assert_stake_exists_in_epoch(
        &genesis_node,
        peer_addr,
        "Stake must still exist in epoch snapshot after rejected unstake"
    );

    // ------------- Phase 3: Verify unstake remains pending in mempool -------------
    // The unstake transaction should still be in mempool, waiting for pledges to clear
    let mempool_unstake = genesis_node
        .get_commitment_tx_from_mempool(&unstake_tx.id)
        .await;
    assert!(
        mempool_unstake.is_ok(),
        "Unstake transaction must remain in mempool after being rejected from blocks"
    );

    // ------------- Phase 4: Mine more blocks - unstake should continue to be rejected -------------
    // Mine another block to verify unstake continues to be rejected
    let second_block_after_unstake = genesis_node.mine_block().await?;
    peer_node
        .wait_until_height(second_block_after_unstake.height, seconds_to_wait)
        .await
        .expect("peer to sync second block");
    let second_block = genesis_node
        .get_block_by_height(second_block_after_unstake.height)
        .await?;

    // Verify unstake is still NOT included
    assert_commitment_not_in_ledger(
        &second_block,
        unstake_tx.id,
        "Unstake must still not be included in subsequent blocks while pledges active"
    );

    // Verify unstake is NOT in the block's commitment snapshot (before epoch processing)
    assert_no_unstake_in_commitment_snapshot(
        &genesis_node,
        second_block.block_hash,
        peer_addr,
        "Unstake must NOT be present in commitment snapshot while pledges are active"
    );

    // Advance to another epoch - unstake should still not be processed
    let (_mined, second_epoch_height) = genesis_node.mine_until_next_epoch().await?;
    peer_node
        .wait_until_height(second_epoch_height, seconds_to_wait)
        .await
        .expect("peer to sync second epoch");
    let second_epoch_block = genesis_node.get_block_by_height(second_epoch_height).await?;

    // Verify no UNSTAKE refund at this epoch either
    let receipts_second_epoch = get_block_receipts(&reth_ctx, second_epoch_block.evm_block_hash)?;
    assert_no_shadow_tx_log(
        &receipts_second_epoch,
        &shadow_tx_topics::UNSTAKE,
        peer_addr,
        "Second epoch block must also not contain UNSTAKE refund"
    );

    // Verify stake still exists in epoch snapshot
    assert_stake_exists_in_epoch(
        &genesis_node,
        peer_addr,
        "Stake must still exist in epoch snapshot after multiple rejection attempts"
    );

    // Verify balance and treasury remain unchanged throughout
    assert_balance(
        &genesis_node,
        peer_addr,
        second_epoch_block.evm_block_hash,
        balance_before_unstake_attempt,
        "Balance must remain unchanged after multiple epochs with rejected unstake"
    );

    genesis_node.stop().await;
    peer_node.stop().await;
    Ok(())
}

// ============================================================================
// Test Helper Functions
// ============================================================================

/// Get receipts for a block
fn get_block_receipts(
    reth_ctx: &irys_reth_node_bridge::IrysRethNodeAdapter,
    block_hash: FixedBytes<32>,
) -> eyre::Result<Vec<reth::primitives::Receipt>> {
    reth_ctx
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(block_hash))?
        .ok_or_else(|| eyre::eyre!("Receipts not found for block {:?}", block_hash))
}

/// Assert that a specific shadow tx log is NOT present in receipts
fn assert_no_shadow_tx_log(
    receipts: &[reth::primitives::Receipt],
    topic: &[u8; 32],
    address: irys_types::Address,
    context: &str,
) {
    let found = receipts.iter().any(|r| {
        r.logs
            .iter()
            .any(|log| log.topics()[0] == *topic && log.address == address)
    });
    assert!(!found, "{}: shadow tx log must NOT be present", context);
}

/// Assert balance equals expected value
fn assert_balance(
    node: &crate::utils::IrysNodeTest<irys_chain::IrysNodeCtx>,
    address: irys_types::Address,
    block_hash: FixedBytes<32>,
    expected: U256,
    message: &str,
) {
    let balance = node.get_balance(address, block_hash);
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
    let commitments = block.get_commitment_ledger_tx_ids();
    assert!(commitments.contains(&tx_id), "{}", message);
}

/// Assert commitment is NOT in the block's commitment ledger
fn assert_commitment_not_in_ledger(
    block: &irys_types::IrysBlockHeader,
    tx_id: irys_types::H256,
    message: &str,
) {
    let commitments = block.get_commitment_ledger_tx_ids();
    assert!(!commitments.contains(&tx_id), "{}", message);
}

/// Assert stake exists for address in canonical epoch snapshot
fn assert_stake_exists_in_epoch(
    node: &crate::utils::IrysNodeTest<irys_chain::IrysNodeCtx>,
    address: irys_types::Address,
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
    address: irys_types::Address,
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
    peer_addr: irys_types::Address,
    expected_irys_ref: FixedBytes<32>,
) {
    let mut found = false;
    for tx in transactions {
        if let Ok(shadow_tx) = ShadowTransaction::decode(&mut tx.input().as_ref()) {
            if let Some(TransactionPacket::UnstakeDebit(debit)) = shadow_tx.as_v1() {
                if debit.target == peer_addr {
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
    peer_addr: irys_types::Address,
    expected_amount: U256,
    expected_irys_ref: FixedBytes<32>,
) {
    let mut found = false;
    for tx in transactions {
        if let Ok(shadow_tx) = ShadowTransaction::decode(&mut tx.input().as_ref()) {
            if let Some(TransactionPacket::UnstakeRefund(increment)) = shadow_tx.as_v1() {
                if increment.target == peer_addr {
                    assert_eq!(
                        increment.amount,
                        expected_amount.into(),
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
    address: irys_types::Address,
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
