use crate::utils::IrysNodeTest;
use alloy_genesis::GenesisAccount;
use alloy_rpc_types_eth::TransactionTrait as _;
use irys_reth_node_bridge::irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};
use irys_types::{irys::IrysSigner, DataLedger, NodeConfig, U256};
use tracing::info;

/// Tests that publish ledger slots expire when publish_ledger_epoch_length is configured.
/// Verifies:
/// - Perm data is stored and accessible before expiry
/// - After epoch_length epochs, perm slots are marked expired
/// - No fee distribution shadow transactions are generated for perm expiry
/// - Expired perm partitions are returned to capacity pool
/// - User balance is unchanged by perm expiry (no fees or refunds)
#[test_log::test(tokio::test)]
async fn heavy_perm_ledger_expiry_basic() -> eyre::Result<()> {
    const CHUNK_SIZE: u64 = 32;
    const DATA_SIZE: usize = 64; // 2 chunks — enough to trigger slot allocation at epoch boundary
    const BLOCKS_PER_EPOCH: u64 = 3;
    const PUBLISH_LEDGER_EPOCH_LENGTH: u64 = 2;
    const INITIAL_BALANCE: u128 = 10_000_000_000_000_000_000;

    let mut config = NodeConfig::testing();
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().chunk_size = CHUNK_SIZE;
    config.consensus.get_mut().num_chunks_in_partition = 4;
    config.consensus.get_mut().num_chunks_in_recall_range = 1;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;
    config.consensus.get_mut().epoch.publish_ledger_epoch_length =
        Some(PUBLISH_LEDGER_EPOCH_LENGTH);

    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(INITIAL_BALANCE).into(),
            ..Default::default()
        },
    )]);

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("perm_expiry_test", 30)
        .await;

    let anchor = node.get_block_by_height(0).await?.block_hash;

    // Post a transaction to the Submit ledger
    let tx = node
        .post_data_tx(anchor, vec![1_u8; DATA_SIZE], &signer)
        .await;
    node.wait_for_mempool(tx.header.id, 10).await?;

    // Upload chunks to trigger promotion to Publish
    node.upload_chunks(&tx).await?;
    node.wait_for_ingress_proofs_no_mining(vec![tx.header.id], 20)
        .await?;

    // Mine 1 block to include tx (triggers promotion to Publish)
    node.mine_block().await?;

    // Capture balance before expiry mining
    let pre_expiry_height = node.get_canonical_chain_height().await;
    let pre_expiry_block = node.get_block_by_height(pre_expiry_height).await?;
    let pre_expiry_balance = node
        .get_balance(signer.address(), pre_expiry_block.evm_block_hash)
        .await;
    info!("User balance before expiry mining: {}", pre_expiry_balance);

    // Mine to first epoch boundary to trigger additional slot allocation
    let (_, epoch_height) = node.mine_until_next_epoch().await?;
    info!("Reached first epoch boundary at height {}", epoch_height);

    // Verify we have 2+ perm slots (last-slot protection prevents single-slot expiry)
    let snapshot = node.get_canonical_epoch_snapshot();
    let perm_slots = snapshot.ledgers.get_slots(DataLedger::Publish);
    assert!(
        perm_slots.len() >= 2,
        "Expected 2+ perm slots after epoch boundary, got {}",
        perm_slots.len()
    );

    // Derive expiry target from observed state
    let slot0_last_height = perm_slots[0].last_height;
    let min_blocks = PUBLISH_LEDGER_EPOCH_LENGTH * BLOCKS_PER_EPOCH;
    let earliest_expiry = min_blocks + slot0_last_height;
    // Round up to epoch boundary
    let target_height =
        ((earliest_expiry + BLOCKS_PER_EPOCH - 1) / BLOCKS_PER_EPOCH) * BLOCKS_PER_EPOCH;
    info!(
        "Slot 0 last_height={}, min_blocks={}, target expiry height={}",
        slot0_last_height, min_blocks, target_height
    );

    // Mine to expiry target
    let current_height = node.get_canonical_chain_height().await;
    for _ in current_height..target_height {
        node.mine_block().await?;
    }

    // Verify we reached the target height
    let final_height = node.get_canonical_chain_height().await;
    info!(
        "Reached height {}, target was {}",
        final_height, target_height
    );
    assert!(
        final_height >= target_height,
        "Should have reached target height"
    );

    // --- Assertion 1: Publish ledger slots are marked expired ---
    let epoch_snapshot = node.get_canonical_epoch_snapshot();
    let perm_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Publish);
    let num_slots = perm_slots.len();

    // At least one non-last slot should be expired (last slot is protected)
    let expired_count = perm_slots
        .iter()
        .enumerate()
        .filter(|(i, slot)| *i < num_slots - 1 && slot.is_expired)
        .count();
    assert!(
        expired_count > 0,
        "Expected at least one non-last perm slot to be expired after height {}",
        final_height
    );
    info!("{} of {} perm slots are expired", expired_count, num_slots);

    // --- Assertion 2: No TermFeeReward shadow txs for Publish expiry ---
    let epoch_block = node.get_block_by_height(target_height).await?;
    let evm_block = node
        .wait_for_evm_block(epoch_block.evm_block_hash, 30)
        .await?;
    for tx in &evm_block.body.transactions {
        let mut input = tx.input().as_ref();
        if let Ok(shadow) = ShadowTransaction::decode(&mut input) {
            if let Some(packet) = shadow.as_v1() {
                // TermFeeReward should never appear for Publish ledger expiry
                assert!(
                    !matches!(packet, TransactionPacket::TermFeeReward(_)),
                    "Unexpected TermFeeReward shadow tx in perm expiry epoch block"
                );
            }
        }
    }
    info!(
        "Verified no TermFeeReward shadow txs in epoch block at height {}",
        target_height
    );

    // --- Assertion 3: Expired partitions returned to capacity pool ---
    let partition_assignments = &epoch_snapshot.partition_assignments;
    for (slot_index, slot) in perm_slots.iter().enumerate() {
        if slot_index < num_slots - 1 && slot.is_expired {
            for partition_hash in &slot.partitions {
                if let Some(assignment) = partition_assignments.get_assignment(*partition_hash) {
                    // Expired perm partitions should have no ledger_id (returned to capacity)
                    assert!(
                        assignment.ledger_id.is_none(),
                        "Expired perm partition {:?} at slot {} should be in capacity pool but has ledger_id={:?}",
                        partition_hash,
                        slot_index,
                        assignment.ledger_id
                    );
                }
            }
        }
    }
    info!("Verified expired perm partitions are in capacity pool");

    // --- Assertion 4: User balance unchanged after perm expiry ---
    let post_expiry_block = node.get_block_by_height(final_height).await?;
    let post_expiry_balance = node
        .get_balance(signer.address(), post_expiry_block.evm_block_hash)
        .await;
    assert_eq!(
        pre_expiry_balance, post_expiry_balance,
        "User balance should not change due to perm expiry (no fees or refunds)"
    );

    info!("Publish ledger expiry test passed!");
    node.stop().await;
    Ok(())
}

/// Tests that publish and submit ledger slots can expire in the same epoch block.
/// Verifies:
/// - Both perm and term slots expire when both epoch lengths are reached simultaneously
/// - TermFeeReward shadow txs are generated (Submit fee distribution runs)
/// - Publish expiry does not block Submit fee distribution
/// - All expired partitions from both ledgers are returned to capacity pool
#[test_log::test(tokio::test)]
async fn heavy_perm_and_term_expiry_same_epoch() -> eyre::Result<()> {
    const CHUNK_SIZE: u64 = 32;
    const DATA_SIZE: usize = 64; // 2 chunks — enough to trigger slot allocation at epoch boundary
    const BLOCKS_PER_EPOCH: u64 = 3;
    const PUBLISH_LEDGER_EPOCH_LENGTH: u64 = 2;
    const SUBMIT_LEDGER_EPOCH_LENGTH: u64 = 2;
    const INITIAL_BALANCE: u128 = 10_000_000_000_000_000_000;

    let mut config = NodeConfig::testing();
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().chunk_size = CHUNK_SIZE;
    config.consensus.get_mut().num_chunks_in_partition = 4;
    config.consensus.get_mut().num_chunks_in_recall_range = 1;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;
    config.consensus.get_mut().epoch.publish_ledger_epoch_length =
        Some(PUBLISH_LEDGER_EPOCH_LENGTH);
    config.consensus.get_mut().epoch.submit_ledger_epoch_length = SUBMIT_LEDGER_EPOCH_LENGTH;

    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(INITIAL_BALANCE).into(),
            ..Default::default()
        },
    )]);

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("perm_term_expiry_test", 30)
        .await;

    let anchor = node.get_block_by_height(0).await?.block_hash;

    // Post tx1 (no chunks → stays on Submit)
    let tx1 = node
        .post_data_tx(anchor, vec![1_u8; DATA_SIZE], &signer)
        .await;
    node.wait_for_mempool(tx1.header.id, 10).await?;

    // Post tx2 (chunks uploaded → promoted to Publish)
    let tx2 = node
        .post_data_tx(anchor, vec![2_u8; DATA_SIZE], &signer)
        .await;
    node.wait_for_mempool(tx2.header.id, 10).await?;
    node.upload_chunks(&tx2).await?;
    node.wait_for_ingress_proofs_no_mining(vec![tx2.header.id], 20)
        .await?;

    // Mine 1 block to include both txs (tx2 gets promoted to Publish)
    node.mine_block().await?;

    // Verify promotion state before expiry
    assert!(
        !node.get_is_promoted(&tx1.header.id).await?,
        "tx1 should NOT be promoted (no chunks uploaded)"
    );
    assert!(
        node.get_is_promoted(&tx2.header.id).await?,
        "tx2 should be promoted (chunks uploaded)"
    );

    // Mine to first epoch boundary to trigger slot allocation (need 2+ slots per ledger)
    let (_, epoch_height) = node.mine_until_next_epoch().await?;
    info!("Reached first epoch boundary at height {}", epoch_height);

    // Verify multi-slot precondition for both ledgers
    let snapshot = node.get_canonical_epoch_snapshot();
    let perm_slots = snapshot.ledgers.get_slots(DataLedger::Publish);
    let submit_slots = snapshot.ledgers.get_slots(DataLedger::Submit);
    assert!(
        perm_slots.len() >= 2,
        "Expected 2+ perm slots after epoch boundary, got {}",
        perm_slots.len()
    );
    assert!(
        submit_slots.len() >= 2,
        "Expected 2+ submit slots after epoch boundary, got {}",
        submit_slots.len()
    );

    // Derive expiry target from observed state (both epoch lengths are equal)
    let perm_slot0_last_height = perm_slots[0].last_height;
    let submit_slot0_last_height = submit_slots[0].last_height;
    let perm_min_blocks = PUBLISH_LEDGER_EPOCH_LENGTH * BLOCKS_PER_EPOCH;
    let submit_min_blocks = SUBMIT_LEDGER_EPOCH_LENGTH * BLOCKS_PER_EPOCH;
    let perm_earliest = perm_min_blocks + perm_slot0_last_height;
    let submit_earliest = submit_min_blocks + submit_slot0_last_height;
    let earliest = perm_earliest.max(submit_earliest);
    // Round up to epoch boundary
    let target_height = ((earliest + BLOCKS_PER_EPOCH - 1) / BLOCKS_PER_EPOCH) * BLOCKS_PER_EPOCH;
    info!(
        "Perm slot0 last_height={}, Submit slot0 last_height={}, target expiry height={}",
        perm_slot0_last_height, submit_slot0_last_height, target_height
    );

    // Mine to expiry target
    let current_height = node.get_canonical_chain_height().await;
    for _ in current_height..target_height {
        node.mine_block().await?;
    }

    let final_height = node.get_canonical_chain_height().await;
    info!(
        "Reached height {}, target was {}",
        final_height, target_height
    );
    assert!(
        final_height >= target_height,
        "Should have reached target height"
    );

    // --- Assertion 1: Submit slots have at least one expired ---
    let epoch_snapshot = node.get_canonical_epoch_snapshot();
    let submit_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Submit);
    let submit_expired = submit_slots.iter().filter(|s| s.is_expired).count();
    assert!(
        submit_expired > 0,
        "Expected at least one expired submit slot after height {}",
        final_height
    );
    info!(
        "{} of {} submit slots are expired",
        submit_expired,
        submit_slots.len()
    );

    // --- Assertion 2: Perm slots have at least one expired non-last slot ---
    let perm_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Publish);
    let perm_num = perm_slots.len();
    let perm_expired = perm_slots
        .iter()
        .enumerate()
        .filter(|(i, s)| *i < perm_num - 1 && s.is_expired)
        .count();
    assert!(
        perm_expired > 0,
        "Expected at least one expired non-last perm slot after height {}",
        final_height
    );
    info!("{} of {} perm slots are expired", perm_expired, perm_num);

    // --- Assertion 3: TermFeeReward shadow tx found (proves Submit fee distribution ran) ---
    let epoch_block = node.get_block_by_height(target_height).await?;
    let evm_block = node
        .wait_for_evm_block(epoch_block.evm_block_hash, 30)
        .await?;
    let mut found_term_fee_reward = false;
    for tx in &evm_block.body.transactions {
        let mut input = tx.input().as_ref();
        if let Ok(shadow) = ShadowTransaction::decode(&mut input) {
            if let Some(TransactionPacket::TermFeeReward(_)) = shadow.as_v1() {
                found_term_fee_reward = true;
                info!(
                    "Found TermFeeReward shadow tx in epoch block at height {}",
                    target_height
                );
            }
        }
    }
    assert!(
        found_term_fee_reward,
        "TermFeeReward shadow tx must be present — Submit fee distribution should run \
         even when Publish expires simultaneously"
    );

    // --- Assertion 4: All expired Submit partitions returned to capacity pool ---
    let partition_assignments = &epoch_snapshot.partition_assignments;
    for (slot_index, slot) in submit_slots.iter().enumerate() {
        if slot.is_expired {
            for partition_hash in &slot.partitions {
                if let Some(assignment) = partition_assignments.get_assignment(*partition_hash) {
                    assert!(
                        assignment.ledger_id.is_none(),
                        "Expired submit partition {:?} at slot {} should be in capacity pool but has ledger_id={:?}",
                        partition_hash, slot_index, assignment.ledger_id
                    );
                }
            }
        }
    }
    info!("Verified expired submit partitions are in capacity pool");

    // --- Assertion 5: All expired non-last Publish partitions returned to capacity pool ---
    for (slot_index, slot) in perm_slots.iter().enumerate() {
        if slot_index < perm_num - 1 && slot.is_expired {
            for partition_hash in &slot.partitions {
                if let Some(assignment) = partition_assignments.get_assignment(*partition_hash) {
                    assert!(
                        assignment.ledger_id.is_none(),
                        "Expired perm partition {:?} at slot {} should be in capacity pool but has ledger_id={:?}",
                        partition_hash, slot_index, assignment.ledger_id
                    );
                }
            }
        }
    }
    info!("Verified expired perm partitions are in capacity pool");

    info!("Simultaneous perm+term expiry test passed!");
    node.stop().await;
    Ok(())
}
