use alloy_core::primitives::FixedBytes;
use alloy_eips::HashOrNumber;
use alloy_rpc_types_eth::TransactionTrait as _;
use irys_reth_node_bridge::irys_reth::shadow_tx::{
    shadow_tx_topics, ShadowTransaction, TransactionPacket,
};
use irys_testing_utils::initialize_tracing;
use irys_types::{
    partition::PartitionAssignment, Address, CommitmentTransaction, ConsensusConfig, NodeConfig,
    U256,
};
use reth::providers::{ReceiptProvider as _, TransactionsProvider as _};
use tracing::warn;

use crate::utils::IrysNodeTest;

/// End-to-end: a pledged user unpledges a specific capacity partition.
/// Inclusion block: fee-only UNPLEDGE; Epoch block: UNPLEDGE_REFUND with value.
#[test_log::test(actix_web::test)]
async fn heavy_unpledge_epoch_refund_flow() -> eyre::Result<()> {
    // ---------- Setup ----------
    initialize_tracing();
    let num_blocks_in_epoch = 2_u64;
    let seconds_to_wait = 20_usize;
    let (genesis_node, peer_node, peer_addr, capacity_pa, consensus) =
        setup_env(num_blocks_in_epoch, seconds_to_wait).await?;

    // Helper: current set of assigned SM partition hashes on a node
    let assigned_sm_hashes = |node: &IrysNodeTest<irys_chain::IrysNodeCtx>| -> Vec<irys_types::H256> {
        let sms = node.node_ctx.storage_modules_guard.read();
        sms.iter()
            .filter_map(|sm| sm.partition_assignment().map(|pa| pa.partition_hash))
            .collect()
    };

    // ---------- Pre-state: storage modules should be assigned before unpledge ----------
    let pre_hashes = assigned_sm_hashes(&peer_node);
    assert!(
        !pre_hashes.is_empty(),
        "Peer should have at least one assigned storage module before unpledge"
    );
    // Test setup must guarantee the target capacity partition is locally loaded
    assert!(
        pre_hashes
            .iter()
            .any(|h| *h == capacity_pa.partition_hash),
        "Test setup invariant violated: target capacity partition must be assigned to a local SM"
    );

    // --- Build and submit Unpledge commitment for that partition
    let anchor = peer_node.get_anchor().await?;

    // Use the peer's pledge provider view; refunds use LIFO and value is set by builder
    let unpledge_tx = CommitmentTransaction::new_unpledge(
        &consensus,
        anchor,
        peer_node.node_ctx.mempool_pledge_provider.as_ref(),
        peer_addr,
        capacity_pa.partition_hash,
    )
    .await;
    let signer = peer_node.cfg.signer();
    let unpledge_tx = signer
        .sign_commitment(unpledge_tx)
        .expect("sign unpledge tx");
    let expected_refund_amount: U256 = unpledge_tx.value; // refund amount at epoch

    // ---------- Action: submit and include unpledge ----------
    peer_node.post_commitment_tx(&unpledge_tx).await?;
    genesis_node
        .wait_for_mempool(unpledge_tx.id, seconds_to_wait)
        .await?;
    // Mine exactly one block to include the unpledge (non-epoch block).
    // Refund will occur later at the epoch boundary.
    let head_height = genesis_node.get_canonical_chain_height().await;
    let head_block = genesis_node.get_block_by_height(head_height).await?;
    let balance_before_inclusion = genesis_node.get_balance(peer_addr, head_block.evm_block_hash);

    let inclusion_block = genesis_node.mine_block().await?;

    // ---------- Assert (inclusion): storage modules not released yet ----------
    let inclusion_hashes = assigned_sm_hashes(&peer_node);
    assert_eq!(
        inclusion_hashes.len(),
        pre_hashes.len(),
        "Unpledge inclusion must not change number of assigned storage modules"
    );
    assert!(
        inclusion_hashes
            .iter()
            .any(|h| *h == capacity_pa.partition_hash),
        "Target partition should still be assigned after inclusion (release only at epoch)"
    );

    // ---------- Assert (inclusion): UNPLEDGE only, fee-only debit ----------
    // First, verify the Irys commitment ledger actually contains the unpledge tx id
    let inclusion_commitments = inclusion_block.get_commitment_ledger_tx_ids();
    warn!(
        ?inclusion_commitments,
        expected_unpledge = ?unpledge_tx.id,
        "Commitment txs present in inclusion block"
    );
    assert!(
        inclusion_commitments.contains(&unpledge_tx.id),
        "Inclusion block should contain the unpledge commitment id"
    );

    let reth_ctx = genesis_node.node_ctx.reth_node_adapter.clone();
    let receipts_inclusion = reth_ctx
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(inclusion_block.evm_block_hash))?
        .expect("receipts should be present for inclusion block");

    let unpledge_receipt_idx = assert_single_log_for(
        &receipts_inclusion,
        &shadow_tx_topics::UNPLEDGE,
        peer_addr,
        "inclusion UNPLEDGE",
    );

    // No refund should appear in the inclusion block
    let has_refund_in_inclusion = receipts_inclusion.iter().any(|r| {
        r.logs
            .iter()
            .any(|log| log.topics()[0] == *shadow_tx_topics::UNPLEDGE_REFUND)
    });
    assert!(
        !has_refund_in_inclusion,
        "Inclusion block must not have UNPLEDGE_REFUND logs"
    );

    // Receipt-level asserts for the unpledge
    let unpledge_receipt = &receipts_inclusion[unpledge_receipt_idx];
    assert!(
        unpledge_receipt.success,
        "Unpledge shadow tx should succeed"
    );
    assert_eq!(
        unpledge_receipt.cumulative_gas_used, 0,
        "Shadow tx should not consume gas"
    );

    // Decode inclusion transactions to find the Unpledge shadow tx and assert fields
    let txs_inclusion = reth_ctx
        .inner
        .provider
        .transactions_by_block(HashOrNumber::Hash(inclusion_block.evm_block_hash))?
        .expect("inclusion block should have transactions");

    let mut found_unpledge_debit = false;
    for tx in txs_inclusion {
        if let Ok(shadow_tx) = ShadowTransaction::decode(&mut tx.input().as_ref()) {
            if let Some(TransactionPacket::Unpledge(debit)) = shadow_tx.as_v1() {
                assert_eq!(debit.target, peer_addr, "Unpledge target mismatch");
                let expected_irys_ref: FixedBytes<32> = unpledge_tx.id.into();
                assert_eq!(
                    debit.irys_ref, expected_irys_ref,
                    "Unpledge irys_ref should match commitment tx id"
                );
                found_unpledge_debit = true;
                break;
            }
        }
    }
    assert!(
        found_unpledge_debit,
        "Did not find Unpledge debit in inclusion block"
    );

    // Fee-only debit semantics: balance decreases exactly by the commitment priority fee
    let balance_after_inclusion =
        genesis_node.get_balance(peer_addr, inclusion_block.evm_block_hash);
    assert_eq!(
        balance_after_inclusion,
        balance_before_inclusion - U256::from(unpledge_tx.fee),
        "Unpledge inclusion should reduce balance by fee only"
    );

    // Treasury should be unchanged at inclusion (unpledge has no treasury movement)
    let prev_header = genesis_node
        .get_block_by_height(inclusion_block.height - 1)
        .await?;
    assert_eq!(
        inclusion_block.treasury, prev_header.treasury,
        "Treasury must be unchanged in unpledge inclusion block"
    );

    // ---------- Action: mine to the next epoch ----------
    let (_mined, final_height) = genesis_node.mine_until_next_epoch().await?;
    // Ensure the peer has fully synced the epoch block so SM updates are applied
    peer_node
        .wait_until_height(final_height, seconds_to_wait)
        .await
        .expect("peer to sync to epoch height");
    let last_block = genesis_node.get_block_by_height(final_height).await?;

    // ---------- Assert (epoch): UNPLEDGE_REFUND with value, balances and treasury ----------
    let receipts_epoch = reth_ctx
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(last_block.evm_block_hash))?
        .expect("receipts should be present for epoch block");
    let refund_receipt_idx = assert_single_log_for(
        &receipts_epoch,
        &shadow_tx_topics::UNPLEDGE_REFUND,
        peer_addr,
        "epoch UNPLEDGE_REFUND",
    );

    // Epoch block should not contain Unpledge inclusion logs
    let has_unpledge_in_epoch = receipts_epoch.iter().any(|r| {
        r.logs
            .iter()
            .any(|log| log.topics()[0] == *shadow_tx_topics::UNPLEDGE)
    });
    assert!(
        !has_unpledge_in_epoch,
        "Epoch block must not contain UNPLEDGE logs (only refunds)"
    );

    // Receipt-level asserts for the refund
    let refund_receipt = &receipts_epoch[refund_receipt_idx];
    assert!(refund_receipt.success, "Refund shadow tx should succeed");
    assert_eq!(
        refund_receipt.cumulative_gas_used, 0,
        "Refund shadow tx should not consume gas"
    );

    // Decode the refund tx and verify amount == unpledge_tx.value, target == signer
    let txs_epoch = reth_ctx
        .inner
        .provider
        .transactions_by_block(HashOrNumber::Hash(last_block.evm_block_hash))?
        .expect("epoch block should have transactions");

    let mut matched = false;
    for tx in txs_epoch {
        if let Ok(shadow_tx) = ShadowTransaction::decode(&mut tx.input().as_ref()) {
            if let Some(TransactionPacket::UnpledgeRefund(inc)) = shadow_tx.as_v1() {
                if inc.target == peer_addr {
                    assert_eq!(
                        inc.amount,
                        expected_refund_amount.into(),
                        "Refund amount should equal last pledge value (builder's value)"
                    );
                    matched = true;
                    break;
                }
            }
        }
    }
    assert!(matched, "Did not find decoded UnpledgeRefund for signer in epoch block");

    // ---------- Assert (epoch): storage module released for target (if it was assigned) ----------
    let epoch_hashes = assigned_sm_hashes(&peer_node);
    assert!(
        !epoch_hashes
            .iter()
            .any(|h| *h == capacity_pa.partition_hash),
        "Target partition must be unassigned from storage modules at epoch boundary"
    );

    // Treasury should decrease by refund amount at epoch block
    let epoch_prev = genesis_node
        .get_block_by_height(last_block.height - 1)
        .await?;
    assert_eq!(
        last_block.treasury,
        epoch_prev.treasury - expected_refund_amount,
        "Epoch block treasury must decrease exactly by the refund value"
    );

    // User balance should increase by exactly the refund amount at epoch (zero priority fee)
    let balance_after_epoch = genesis_node.get_balance(peer_addr, last_block.evm_block_hash);

    assert_eq!(
        balance_after_epoch,
        balance_after_inclusion + expected_refund_amount,
        "Epoch refund should increase balance by refund amount with zero priority fee"
    );

    // Net effect from before inclusion: -fee at inclusion + refund at epoch
    let expected_final =
        balance_before_inclusion - U256::from(unpledge_tx.fee) + expected_refund_amount;
    assert_eq!(
        balance_after_epoch, expected_final,
        "Final balance should equal initial - inclusion fee + refund"
    );

    // ---------- Cleanup ----------
    genesis_node.stop().await;
    peer_node.stop().await;
    Ok(())
}

async fn setup_env(
    num_blocks_in_epoch: u64,
    seconds_to_wait: usize,
) -> eyre::Result<(
    IrysNodeTest<irys_chain::IrysNodeCtx>,
    IrysNodeTest<irys_chain::IrysNodeCtx>,
    Address,
    PartitionAssignment,
    ConsensusConfig,
)> {
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch as usize);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let peer_config = genesis_node.testing_peer_with_signer(&peer_signer);
    let peer_node = genesis_node
        .testing_peer_with_assignments_and_name(peer_config, "PEER")
        .await?;

    let peer_addr = peer_signer.address();

    // Prefer a capacity assignment that is actually loaded on a local Storage Module.
    // This guarantees the subsequent unpledge triggers a real SM unassignment at epoch.
    let capacity_pa = {
        let sms = peer_node.node_ctx.storage_modules_guard.read();
        // Require a locally loaded capacity assignment (ledger_id == None) for determinism
        sms.iter()
            .filter_map(|sm| sm.partition_assignment())
            .find(|pa| pa.ledger_id.is_none())
            .expect("Test requires at least one local capacity SM assignment")
    };

    let consensus = genesis_node.node_ctx.config.node_config.consensus_config();

    Ok((genesis_node, peer_node, peer_addr, capacity_pa, consensus))
}

fn assert_single_log_for(
    receipts: &[reth::primitives::Receipt],
    topic: &[u8; 32],
    addr: Address,
    context: &str,
) -> usize {
    // Find receipts that contain exactly one matching log for the given topic and address.
    let mut idx: Option<usize> = None;
    let hits: Vec<_> = receipts
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let has = r
                .logs
                .iter()
                .any(|log| log.topics()[0] == *topic && log.address == addr);
            if has {
                idx = Some(i);
                Some(())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(hits.len(), 1, "{}: expected exactly one log", context);
    idx.expect("receipt index")
}
