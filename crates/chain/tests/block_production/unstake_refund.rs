use alloy_core::primitives::FixedBytes;
use alloy_eips::HashOrNumber;
use alloy_rpc_types_eth::TransactionTrait as _;
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
    let unstake_commitments = unstake_block.get_commitment_ledger_tx_ids();
    assert!(
        unstake_commitments.contains(&unstake_tx.id),
        "Unstake commitment must appear in the inclusion ledger"
    );

    let receipts_inclusion = reth_ctx
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(unstake_block.evm_block_hash))?
        .expect("receipts should exist for unstake inclusion block");
    let debit_receipt_idx = assert_single_log_for(
        &receipts_inclusion,
        &shadow_tx_topics::UNSTAKE_DEBIT,
        peer_addr,
        "inclusion UNSTAKE_DEBIT",
    );
    let has_refund_in_inclusion = receipts_inclusion.iter().any(|r| {
        r.logs
            .iter()
            .any(|log| log.topics()[0] == *shadow_tx_topics::UNSTAKE)
    });
    assert!(
        !has_refund_in_inclusion,
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
    let mut found_unstake_debit = false;
    for tx in txs_inclusion {
        if let Ok(shadow_tx) = ShadowTransaction::decode(&mut tx.input().as_ref()) {
            if let Some(TransactionPacket::UnstakeDebit(debit)) = shadow_tx.as_v1() {
                if debit.target == peer_addr {
                    assert_eq!(
                        debit.irys_ref, expected_irys_ref,
                        "Unstake debit irys_ref must match commitment id"
                    );
                    found_unstake_debit = true;
                    break;
                }
            }
        }
    }
    assert!(
        found_unstake_debit,
        "Unstake inclusion block must carry an UNSTAKE_DEBIT packet"
    );

    let balance_after_inclusion = genesis_node.get_balance(peer_addr, unstake_block.evm_block_hash);
    assert_eq!(
        balance_after_inclusion,
        balance_before_unstake - U256::from(unstake_tx.fee),
        "Unstake inclusion should reduce balance by priority fee only"
    );
    assert_eq!(
        unstake_block.treasury, treasury_before_unstake,
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

    let receipts_epoch = reth_ctx
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(epoch_block.evm_block_hash))?
        .expect("epoch receipts available for unstake refund");
    let refund_receipt_idx = assert_single_log_for(
        &receipts_epoch,
        &shadow_tx_topics::UNSTAKE,
        peer_addr,
        "epoch UNSTAKE refund",
    );
    let has_debit_in_epoch = receipts_epoch.iter().any(|r| {
        r.logs
            .iter()
            .any(|log| log.topics()[0] == *shadow_tx_topics::UNSTAKE_DEBIT)
    });
    assert!(
        !has_debit_in_epoch,
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
    let mut verified_refund = false;
    for tx in epoch_txs {
        if let Ok(shadow_tx) = ShadowTransaction::decode(&mut tx.input().as_ref()) {
            if let Some(TransactionPacket::UnstakeRefund(increment)) = shadow_tx.as_v1() {
                if increment.target == peer_addr {
                    assert_eq!(
                        increment.amount,
                        expected_refund_amount.into(),
                        "Unstake refund amount must equal configured stake value"
                    );
                    assert_eq!(
                        increment.irys_ref, expected_irys_ref,
                        "Unstake refund irys_ref must match commitment id"
                    );
                    verified_refund = true;
                }
            }
        }
    }
    assert!(
        verified_refund,
        "Epoch block must carry an UNSTAKE refund transaction for the signer"
    );

    let epoch_prev = genesis_node
        .get_block_by_height(epoch_block.height - 1)
        .await?;
    assert_eq!(
        epoch_block.treasury,
        epoch_prev.treasury - expected_refund_amount,
        "Treasury must decrease exactly by the stake refund amount"
    );

    let balance_after_epoch = genesis_node.get_balance(peer_addr, epoch_block.evm_block_hash);
    let expected_final_balance =
        balance_before_unstake - U256::from(unstake_tx.fee) + expected_refund_amount;
    assert_eq!(
        balance_after_epoch, expected_final_balance,
        "Net effect should be -fee at inclusion + full refund at epoch"
    );

    let epoch_snapshot = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot();
    assert!(
        !epoch_snapshot
            .commitment_state
            .stake_commitments
            .contains_key(&peer_addr),
        "Epoch snapshot must remove the stake after refunding"
    );

    genesis_node.stop().await;
    peer_node.stop().await;
    Ok(())
}
