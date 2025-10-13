use alloy_core::primitives::FixedBytes;
use alloy_eips::HashOrNumber;
use alloy_rpc_types_eth::TransactionTrait as _;
use irys_reth_node_bridge::irys_reth::shadow_tx::{
    shadow_tx_topics, ShadowTransaction, TransactionPacket,
};
use irys_testing_utils::initialize_tracing;
use irys_types::{
    partition::PartitionAssignment, Address, CommitmentTransaction, ConsensusConfig, NodeConfig,
    PledgeDataProvider as _, U256,
};
use reth::providers::{ReceiptProvider as _, TransactionsProvider as _};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tracing::warn;

use crate::utils::IrysNodeTest;

// Test setup: two-node network with genesis producing blocks and the peer starting with three
// pledged partitions assigned to local storage modules and reflected in treasury accounting.
//
// Action: peer submits a single unpledge commitment, we mine the inclusion block, then advance to
// the epoch boundary that emits refunds.
//
// Expectation: inclusion block charges only the commitment fee without touching treasury or SM state,
// epoch block emits the refund, increases the peer balance by the pledged value, decreases treasury,
// and clears the local storage module assignment for the partition.
#[test_log::test(actix_web::test)]
async fn heavy_unpledge_epoch_refund_flow() -> eyre::Result<()> {
    initialize_tracing();
    let num_blocks_in_epoch = 2_u64;
    let seconds_to_wait = 20_usize;
    let (genesis_node, peer_node, peer_addr, capacity_pa, consensus) =
        setup_env(num_blocks_in_epoch, seconds_to_wait).await?;

    // Helper: current set of assigned SM partition hashes on a node
    let assigned_sm_hashes =
        |node: &IrysNodeTest<irys_chain::IrysNodeCtx>| -> Vec<irys_types::H256> {
            let sms = node.node_ctx.storage_modules_guard.read();
            sms.iter().filter_map(|sm| sm.partition_hash()).collect()
        };

    // --- Build and submit Unpledge commitment for that partition
    let anchor = peer_node.get_anchor().await?;

    // Use the peer's pledge provider view; refunds use LIFO and value is set by builder
    let mut unpledge_tx = CommitmentTransaction::new_unpledge(
        &consensus,
        anchor,
        peer_node.node_ctx.mempool_pledge_provider.as_ref(),
        peer_addr,
        capacity_pa.partition_hash,
    )
    .await;
    let signer = peer_node.cfg.signer();
    signer
        .sign_commitment(&mut unpledge_tx)
        .expect("sign unpledge tx");
    let expected_refund_amount: U256 = unpledge_tx.value; // refund amount at epoch

    // ---------- Action: submit and include unpledge ----------
    genesis_node.post_commitment_tx(&unpledge_tx).await?;
    genesis_node
        .wait_for_mempool(unpledge_tx.id, seconds_to_wait)
        .await?;
    // Mine exactly one block to include the unpledge (non-epoch block).
    // Refund will occur later at the epoch boundary.
    let head_height = genesis_node.get_canonical_chain_height().await;
    let head_block = genesis_node.get_block_by_height(head_height).await?;
    let balance_before_inclusion = genesis_node.get_balance(peer_addr, head_block.evm_block_hash);

    let inclusion_block_peer = genesis_node.mine_block().await?;
    genesis_node
        .wait_until_height(inclusion_block_peer.height, seconds_to_wait)
        .await
        .expect("genesis should sync peer-mined inclusion block");
    let inclusion_block = genesis_node
        .get_block_by_height(inclusion_block_peer.height)
        .await?;

    // ---------- Assert (inclusion): storage modules not released yet ----------
    let inclusion_hashes = assigned_sm_hashes(&peer_node);
    assert!(
        inclusion_hashes.contains(&capacity_pa.partition_hash),
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
    assert!(
        matched,
        "Did not find decoded UnpledgeRefund for signer in epoch block"
    );

    // ---------- Assert (epoch): storage module released for target (if it was assigned) ----------
    let epoch_hashes = assigned_sm_hashes(&peer_node);
    tracing::error!(hash = ?capacity_pa.partition_hash, "hash unassigned");
    assert!(
        !epoch_hashes.contains(&capacity_pa.partition_hash),
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

// Test setup: single genesis node bootstrapped with exactly three pledged partitions assigned to
// its local storage modules, treasury funded with the pledge collateral.
//
// Action: submit two sequential unpledge commitments for distinct partitions, mine the inclusion
// block to capture fee effects, then advance to the epoch boundary to trigger refunds.
//
// Expectation: inclusion keeps storage module assignments untouched, epoch block emits two refunds
// matching the commitments, treasury drops by the total refund amount, and only the remaining
// pledged partition stays assigned.
#[test_log::test(actix_web::test)]
async fn heavy_genesis_unpledge_two_partitions_refund_flow() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2_u64;
    let seconds_to_wait = 20_usize;
    let (genesis_node, consensus) = setup_genesis_env(num_blocks_in_epoch, seconds_to_wait).await?;

    let miner_signer = genesis_node.cfg.signer();
    let miner_addr = miner_signer.address();

    let assigned_partitions: Vec<PartitionAssignment> = {
        let sms = genesis_node.node_ctx.storage_modules_guard.read();
        sms.iter()
            .filter_map(|sm| sm.partition_assignment())
            .collect()
    };
    assert_eq!(
        assigned_partitions.len(),
        3,
        "Genesis node should start with exactly three pledged partitions"
    );
    assert!(
        assigned_partitions
            .iter()
            .all(|pa| pa.miner_address == miner_addr),
        "All initial assignments must belong to the genesis miner"
    );

    let partitions_to_unpledge: Vec<PartitionAssignment> =
        assigned_partitions.iter().take(2).copied().collect();
    assert_eq!(
        partitions_to_unpledge.len(),
        2,
        "Test requires at least two partitions to unpledge"
    );

    let initial_partition_hashes: Vec<_> = assigned_partitions
        .iter()
        .map(|pa| pa.partition_hash)
        .collect();

    let head_height = genesis_node.get_canonical_chain_height().await;
    let head_block = genesis_node.get_block_by_height(head_height).await?;
    let treasury_before = head_block.treasury;
    let reth_ctx = genesis_node.node_ctx.reth_node_adapter.clone();

    let mut total_refund = U256::from(0_u64);
    let mut unpledge_txs: Vec<(CommitmentTransaction, PartitionAssignment)> = Vec::new();

    let initial_pledge_count = genesis_node
        .node_ctx
        .mempool_pledge_provider
        .as_ref()
        .pledge_count(miner_addr)
        .await;
    for (idx, target) in partitions_to_unpledge.iter().enumerate() {
        let anchor = genesis_node.get_anchor().await?;
        let mut unsigned = CommitmentTransaction::new_unpledge(
            &consensus,
            anchor,
            &(initial_pledge_count - (idx as u64)),
            miner_addr,
            target.partition_hash,
        )
        .await;
        let signer = genesis_node.cfg.signer();
        signer
            .sign_commitment(&mut unsigned)
            .expect("sign genesis unpledge tx");

        total_refund += unsigned.value;
        genesis_node.post_commitment_tx(&unsigned).await?;
        genesis_node
            .wait_for_mempool_commitment_txs(vec![unsigned.id], seconds_to_wait)
            .await?;
        unpledge_txs.push((unsigned, *target));
    }

    let inclusion_block = genesis_node.mine_block().await?;

    let inclusion_assignments: Vec<_> = {
        let sms = genesis_node.node_ctx.storage_modules_guard.read();
        sms.iter()
            .filter_map(|sm| sm.partition_assignment().map(|pa| pa.partition_hash))
            .collect()
    };
    assert_eq!(
        inclusion_assignments.len(),
        initial_partition_hashes.len(),
        "Unpledge inclusion must not alter storage module assignments yet"
    );
    for target in &partitions_to_unpledge {
        assert!(
            inclusion_assignments.contains(&target.partition_hash),
            "Partition {} should remain assigned until epoch release",
            target.partition_hash
        );
    }

    let inclusion_commitments = inclusion_block.get_commitment_ledger_tx_ids();
    for (tx, _) in &unpledge_txs {
        assert!(
            inclusion_commitments.contains(&tx.id),
            "Inclusion block missing unpledge commitment {}",
            tx.id
        );
    }

    let inclusion_receipts = reth_ctx
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(inclusion_block.evm_block_hash))?
        .expect("receipts should exist for inclusion block");
    let mut inclusion_unpledge_logs = 0_usize;
    for receipt in &inclusion_receipts {
        assert!(
            !receipt
                .logs
                .iter()
                .any(|log| log.topics()[0] == *shadow_tx_topics::UNPLEDGE_REFUND),
            "Inclusion block must not contain UNPLEDGE_REFUND logs"
        );
        inclusion_unpledge_logs += receipt
            .logs
            .iter()
            .filter(|log| {
                log.topics()[0] == *shadow_tx_topics::UNPLEDGE && log.address == miner_addr
            })
            .count();
    }
    assert_eq!(
        inclusion_unpledge_logs,
        partitions_to_unpledge.len(),
        "Expected one UNPLEDGE log per commitment in inclusion block"
    );

    let inclusion_txs = reth_ctx
        .inner
        .provider
        .transactions_by_block(HashOrNumber::Hash(inclusion_block.evm_block_hash))?
        .expect("transactions should exist for inclusion block");
    let expected_irys_refs: HashSet<FixedBytes<32>> =
        unpledge_txs.iter().map(|(tx, _)| tx.id.into()).collect();
    let mut matched_irys_refs: HashSet<FixedBytes<32>> = HashSet::new();
    for tx in inclusion_txs {
        if let Ok(shadow_tx) = ShadowTransaction::decode(&mut tx.input().as_ref()) {
            if let Some(TransactionPacket::Unpledge(debit)) = shadow_tx.as_v1() {
                if debit.target == miner_addr && expected_irys_refs.contains(&debit.irys_ref) {
                    matched_irys_refs.insert(debit.irys_ref);
                }
            }
        }
    }
    assert_eq!(
        matched_irys_refs, expected_irys_refs,
        "Every unpledge commitment should produce a matching shadow debit"
    );

    assert_eq!(
        inclusion_block.treasury, treasury_before,
        "Treasury must remain unchanged during inclusion block"
    );

    let (_mined, epoch_height) = genesis_node.mine_until_next_epoch().await?;
    let epoch_block = genesis_node.get_block_by_height(epoch_height).await?;
    let epoch_receipts = reth_ctx
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(epoch_block.evm_block_hash))?
        .expect("receipts should exist for epoch block");
    let mut refund_logs = 0_usize;
    for receipt in &epoch_receipts {
        let has_unpledge = receipt
            .logs
            .iter()
            .any(|log| log.topics()[0] == *shadow_tx_topics::UNPLEDGE);
        assert!(
            !has_unpledge,
            "Epoch block must not contain UNPLEDGE inclusion logs"
        );

        refund_logs += receipt
            .logs
            .iter()
            .filter(|log| {
                log.topics()[0] == *shadow_tx_topics::UNPLEDGE_REFUND && log.address == miner_addr
            })
            .count();
    }
    assert_eq!(
        refund_logs,
        partitions_to_unpledge.len(),
        "Epoch block must emit one UNPLEDGE_REFUND per unpledged partition"
    );

    let epoch_txs = reth_ctx
        .inner
        .provider
        .transactions_by_block(HashOrNumber::Hash(epoch_block.evm_block_hash))?
        .expect("epoch block should have transactions");
    let expected_refunds: HashMap<FixedBytes<32>, U256> = unpledge_txs
        .iter()
        .map(|(tx, _)| (tx.id.into(), tx.value))
        .collect();
    let mut matched_refunds: HashMap<FixedBytes<32>, U256> = HashMap::new();
    for tx in epoch_txs {
        if let Ok(shadow_tx) = ShadowTransaction::decode(&mut tx.input().as_ref()) {
            if let Some(TransactionPacket::UnpledgeRefund(inc)) = shadow_tx.as_v1() {
                if inc.target == miner_addr {
                    if let Some(expected_amount) = expected_refunds.get(&inc.irys_ref) {
                        assert_eq!(
                            U256::from(inc.amount),
                            *expected_amount,
                            "Refund amount mismatch for commitment {:?}",
                            inc.irys_ref
                        );
                        matched_refunds.insert(inc.irys_ref, U256::from(inc.amount));
                    }
                }
            }
        }
    }
    assert_eq!(
        matched_refunds, expected_refunds,
        "Epoch block must refund every unpledge commitment"
    );

    let epoch_prev = genesis_node
        .get_block_by_height(epoch_block.height - 1)
        .await?;
    assert_eq!(
        epoch_block.treasury,
        epoch_prev.treasury - total_refund,
        "Epoch treasury should decrease by total refund amount"
    );

    let post_epoch_assignments: HashSet<_> = {
        let sms = genesis_node.node_ctx.storage_modules_guard.read();
        sms.iter()
            .filter_map(|sm| sm.partition_assignment().map(|pa| pa.partition_hash))
            .collect()
    };
    for (_, assignment) in &unpledge_txs {
        assert!(
            !post_epoch_assignments.contains(&assignment.partition_hash),
            "Partition {} should be de-assigned after epoch refund",
            assignment.partition_hash
        );
    }
    let expected_remaining: HashSet<_> = assigned_partitions
        .iter()
        .skip(partitions_to_unpledge.len())
        .map(|pa| pa.partition_hash)
        .collect();
    assert_eq!(
        post_epoch_assignments, expected_remaining,
        "Only the non-unpledged partitions should remain assigned locally"
    );

    genesis_node.stop().await;
    Ok(())
}

// Test setup: Genesis producing blocks and the peer starting with exactly three locally assigned
// pledged partitions persisted in storage modules and reflected in the treasury snapshot.
//
// Action: have the peer submit unpledge commitments for every assigned partition, mine a single
// inclusion block, then advance to the epoch boundary so all refunds are processed together.
//
// Expectation: inclusion reduces the peer balance only by aggregate priority fees and leaves
// assignments untouched, epoch refunds appear in strictly increasing value order (LIFO semantics),
// treasury drops by the total refund amount, the peer balance returns to the initial value minus fees,
// and no storage modules remain assigned to the peer.
#[test_log::test(actix_web::test)]
async fn heavy_unpledge_all_partitions_refund_flow() -> eyre::Result<()> {
    let num_blocks_in_epoch = 2_u64;
    let seconds_to_wait = 20_usize;
    let (genesis_node, peer_node, _, _capacity_pa, consensus) =
        setup_env(num_blocks_in_epoch, seconds_to_wait).await?;
    let genesis_signer = genesis_node.node_ctx.config.irys_signer();

    let assigned_sm_hashes =
        |node: &IrysNodeTest<irys_chain::IrysNodeCtx>| -> Vec<irys_types::H256> {
            let sms = node.node_ctx.storage_modules_guard.read();
            sms.iter().filter_map(|sm| sm.partition_hash()).collect()
        };

    let assigned_partitions = {
        let sms = genesis_node.node_ctx.storage_modules_guard.read();
        sms.iter().cloned().collect::<Vec<_>>()
    };
    assert_eq!(
        assigned_partitions.len(),
        3,
        "Peer must begin with exactly three locally assigned capacity partitions"
    );

    let pre_hashes = assigned_sm_hashes(&genesis_node);
    assert_eq!(
        pre_hashes.len(),
        assigned_partitions.len(),
        "Storage module count should align with discovered assignments"
    );

    let head_height = genesis_node.get_canonical_chain_height().await;
    let head_block = genesis_node.get_block_by_height(head_height).await?;
    let balance_before_inclusion =
        genesis_node.get_balance(genesis_signer.address(), head_block.evm_block_hash);

    let reth_ctx = genesis_node.node_ctx.reth_node_adapter.clone();
    let (total_fee, total_refund, unpledge_txs) = send_unpledge_all(
        seconds_to_wait,
        &genesis_node,
        &peer_node,
        consensus,
        &genesis_signer,
        assigned_partitions,
    )
    .await?;

    let inclusion_block = peer_node.mine_block().await?;

    let inclusion_hashes = assigned_sm_hashes(&genesis_node);
    assert_eq!(
        inclusion_hashes, pre_hashes,
        "Inclusion block must not change storage module assignments"
    );

    let inclusion_commitments = inclusion_block.get_commitment_ledger_tx_ids();
    for (tx, _) in &unpledge_txs {
        assert!(
            inclusion_commitments.contains(&tx.id),
            "Unpledge commitment {tx_id:?} missing from inclusion block",
            tx_id = tx.id
        );
    }

    let inclusion_receipts = reth_ctx
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(inclusion_block.evm_block_hash))?
        .expect("receipts present for inclusion block");
    let mut inclusion_unpledge_logs = 0_usize;
    for receipt in &inclusion_receipts {
        assert!(
            !receipt
                .logs
                .iter()
                .any(|log| log.topics()[0] == *shadow_tx_topics::UNPLEDGE_REFUND),
            "Inclusion block must not contain refund logs"
        );
        inclusion_unpledge_logs += receipt
            .logs
            .iter()
            .filter(|log| {
                log.topics()[0] == *shadow_tx_topics::UNPLEDGE
                    && log.address == genesis_signer.address()
            })
            .count();
    }
    assert_eq!(
        inclusion_unpledge_logs,
        unpledge_txs.len(),
        "Expected one UNPLEDGE log per commitment"
    );

    let inclusion_txs = reth_ctx
        .inner
        .provider
        .transactions_by_block(HashOrNumber::Hash(inclusion_block.evm_block_hash))?
        .expect("transactions available for inclusion block");
    let expected_irys_refs: HashSet<FixedBytes<32>> =
        unpledge_txs.iter().map(|(tx, _)| tx.id.into()).collect();
    let mut matched_irys_refs: HashSet<FixedBytes<32>> = HashSet::new();
    for tx in inclusion_txs {
        if let Ok(shadow_tx) = ShadowTransaction::decode(&mut tx.input().as_ref()) {
            if let Some(TransactionPacket::Unpledge(debit)) = shadow_tx.as_v1() {
                if debit.target == genesis_signer.address()
                    && expected_irys_refs.contains(&debit.irys_ref)
                {
                    matched_irys_refs.insert(debit.irys_ref);
                }
            }
        }
    }
    assert_eq!(
        matched_irys_refs, expected_irys_refs,
        "Every unpledge commitment should have a decoded shadow debit"
    );

    let balance_after_inclusion =
        genesis_node.get_balance(genesis_signer.address(), inclusion_block.evm_block_hash);
    assert_eq!(
        balance_after_inclusion,
        balance_before_inclusion - total_fee,
        "Inclusion must charge only the aggregate priority fees"
    );

    let prev_header = genesis_node
        .get_block_by_height(inclusion_block.height - 1)
        .await?;
    assert_eq!(
        inclusion_block.treasury, prev_header.treasury,
        "Treasury must remain unchanged during inclusion"
    );

    let (_mined, epoch_height) = peer_node.mine_until_next_epoch().await?;
    genesis_node
        .wait_until_height(epoch_height, seconds_to_wait)
        .await
        .expect("genesis should sync peer-mined epoch block");
    let epoch_block = genesis_node.get_block_by_height(epoch_height).await?;

    let epoch_receipts = reth_ctx
        .inner
        .provider
        .receipts_by_block(HashOrNumber::Hash(epoch_block.evm_block_hash))?
        .expect("epoch receipts available");
    let mut refund_logs = 0_usize;
    for receipt in &epoch_receipts {
        assert!(
            !receipt
                .logs
                .iter()
                .any(|log| log.topics()[0] == *shadow_tx_topics::UNPLEDGE),
            "Epoch block must not contain inclusion UNPLEDGE logs"
        );
        refund_logs += receipt
            .logs
            .iter()
            .filter(|log| {
                log.topics()[0] == *shadow_tx_topics::UNPLEDGE_REFUND
                    && log.address == genesis_signer.address()
            })
            .count();
    }
    assert_eq!(
        refund_logs,
        unpledge_txs.len(),
        "Epoch block must emit one refund per unpledge"
    );

    let epoch_txs = reth_ctx
        .inner
        .provider
        .transactions_by_block(HashOrNumber::Hash(epoch_block.evm_block_hash))?
        .expect("epoch transactions available");
    let expected_refunds: HashSet<FixedBytes<32>> =
        unpledge_txs.iter().map(|(tx, _)| tx.id.into()).collect();
    let mut seen_refs: HashSet<FixedBytes<32>> = HashSet::new();
    let mut refund_amounts: Vec<U256> = Vec::new();
    for tx in epoch_txs {
        if let Ok(shadow_tx) = ShadowTransaction::decode(&mut tx.input().as_ref()) {
            if let Some(TransactionPacket::UnpledgeRefund(inc)) = shadow_tx.as_v1() {
                if inc.target == genesis_signer.address()
                    && expected_refunds.contains(&inc.irys_ref)
                {
                    seen_refs.insert(inc.irys_ref);
                    refund_amounts.push(U256::from(inc.amount));
                }
            }
        }
    }
    assert_eq!(
        seen_refs, expected_refunds,
        "All refund shadow txs must correspond to submitted commitments"
    );
    assert_eq!(
        refund_amounts.len(),
        unpledge_txs.len(),
        "Refunds collected should match number of unpledge commitments"
    );
    for window in refund_amounts.windows(2) {
        assert!(
            window[0] < window[1],
            "Refund amounts must be strictly increasing (LIFO unpledge semantics)"
        );
    }

    let epoch_prev = genesis_node
        .get_block_by_height(epoch_block.height - 1)
        .await?;
    assert_eq!(
        epoch_block.treasury,
        epoch_prev.treasury - total_refund,
        "Treasury must drop by the total refund amount"
    );

    let balance_after_epoch =
        genesis_node.get_balance(genesis_signer.address(), epoch_block.evm_block_hash);
    let expected_final_balance = balance_before_inclusion - total_fee + total_refund;
    assert_eq!(
        balance_after_epoch, expected_final_balance,
        "Final balance should equal initial balance minus aggregate inclusion fees plus refunded pledges"
    );

    let post_epoch_hashes = assigned_sm_hashes(&genesis_node);
    assert!(
        post_epoch_hashes.is_empty(),
        "Genesis storage modules should be fully de-assigned after epoch refunds"
    );

    genesis_node.stop().await;
    peer_node.stop().await;
    Ok(())
}

pub async fn send_unpledge_all(
    seconds_to_wait: usize,
    genesis_node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    peer_node: &IrysNodeTest<irys_chain::IrysNodeCtx>,
    consensus: ConsensusConfig,
    signer: &irys_types::irys::IrysSigner,
    assigned_partitions: Vec<Arc<irys_domain::StorageModule>>,
) -> Result<
    (
        U256,
        U256,
        Vec<(CommitmentTransaction, Arc<irys_domain::StorageModule>)>,
    ),
    eyre::Error,
> {
    let mut total_fee = U256::from(0_u64);
    let mut total_refund = U256::from(0_u64);
    let mut unpledge_txs = Vec::new();
    let initial_pledge_count = genesis_node
        .node_ctx
        .mempool_pledge_provider
        .as_ref()
        .pledge_count(signer.address())
        .await;
    tracing::error!(?initial_pledge_count);
    for (idx, target) in assigned_partitions.iter().enumerate() {
        let pledge_count = initial_pledge_count - (idx as u64);
        if pledge_count == 0 {
            break;
        }
        if target.partition_assignment.read().unwrap().is_none() {
            continue;
        };
        let anchor = peer_node.get_anchor().await?;
        tracing::error!(?pledge_count);
        let mut unsigned = CommitmentTransaction::new_unpledge(
            &consensus,
            anchor,
            &pledge_count,
            signer.address(),
            target.partition_hash().unwrap(),
        )
        .await;
        tracing::error!(part_hash = ?target.partition_hash());
        signer
            .sign_commitment(&mut unsigned)
            .expect("sign multi-unpledge tx");
        total_fee += U256::from(unsigned.fee);
        total_refund += unsigned.value;

        peer_node.post_commitment_tx(&unsigned).await?;
        genesis_node
            .wait_for_mempool(unsigned.id, seconds_to_wait)
            .await?;
        unpledge_txs.push((unsigned, Arc::clone(target)));
    }
    Ok((total_fee, total_refund, unpledge_txs))
}

pub(crate) async fn setup_genesis_env(
    num_blocks_in_epoch: u64,
    seconds_to_wait: usize,
) -> eyre::Result<(IrysNodeTest<irys_chain::IrysNodeCtx>, ConsensusConfig)> {
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch as usize);
    genesis_config.consensus.get_mut().chunk_size = 32;

    let genesis_node = IrysNodeTest::new_genesis(genesis_config)
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let consensus = genesis_node.node_ctx.config.node_config.consensus_config();

    Ok((genesis_node, consensus))
}

pub(crate) async fn setup_env_with_block_migration_depth(
    num_blocks_in_epoch: u64,
    seconds_to_wait: usize,
    block_migration_depth: u32,
) -> eyre::Result<(
    IrysNodeTest<irys_chain::IrysNodeCtx>,
    IrysNodeTest<irys_chain::IrysNodeCtx>,
    Address,
    PartitionAssignment,
    ConsensusConfig,
)> {
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch as usize);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().block_migration_depth = block_migration_depth;

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

pub(crate) async fn setup_env(
    num_blocks_in_epoch: u64,
    seconds_to_wait: usize,
) -> eyre::Result<(
    IrysNodeTest<irys_chain::IrysNodeCtx>,
    IrysNodeTest<irys_chain::IrysNodeCtx>,
    Address,
    PartitionAssignment,
    ConsensusConfig,
)> {
    setup_env_with_block_migration_depth(num_blocks_in_epoch, seconds_to_wait, 4).await
}

pub(crate) fn assert_single_log_for(
    receipts: &[reth::primitives::Receipt],
    topic: &[u8; 32],
    addr: Address,
    context: &str,
) -> usize {
    use irys_reth_node_bridge::irys_reth::shadow_tx::shadow_tx_topics::*;
    tracing::error!(UNSTAKE = ?(*UNSTAKE));
    tracing::error!(UNSTAKE_DEBIT = ?(*UNSTAKE_DEBIT));
    tracing::error!(BLOCK_REWARD = ?(*BLOCK_REWARD));
    tracing::error!(STAKE = ?(*STAKE));
    tracing::error!(STORAGE_FEES = ?(*STORAGE_FEES));
    tracing::error!(PLEDGE = ?(*PLEDGE));
    tracing::error!(UNPLEDGE = ?(*UNPLEDGE));
    tracing::error!(UNPLEDGE_REFUND = ?(*UNPLEDGE_REFUND));
    tracing::error!(TERM_FEE_REWARD = ?(*TERM_FEE_REWARD));
    tracing::error!(INGRESS_PROOF_REWARD = ?(*INGRESS_PROOF_REWARD));
    tracing::error!(PERM_FEE_REFUND = ?(*PERM_FEE_REFUND));

    // Find receipts that contain exactly one matching log for the given topic and address.
    let mut idx: Option<usize> = None;
    let hits: Vec<_> = receipts
        .iter()
        .enumerate()
        .filter_map(|(i, r)| {
            let has = r
                .logs
                .iter()
                .inspect(|log| {
                    tracing::warn!(topic = ?log.topics()[0], addrress =?log.address);
                })
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
