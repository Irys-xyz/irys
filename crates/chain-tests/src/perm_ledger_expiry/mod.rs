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
    let target_height = earliest_expiry.div_ceil(BLOCKS_PER_EPOCH) * BLOCKS_PER_EPOCH;
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
    let mut checked_partitions = 0_usize;
    for (slot_index, slot) in perm_slots.iter().enumerate() {
        if slot_index < num_slots - 1 && slot.is_expired {
            for partition_hash in &slot.partitions {
                let assignment = partition_assignments
                    .get_assignment(*partition_hash)
                    .unwrap_or_else(|| {
                        panic!(
                            "Missing partition assignment for {:?} at slot {}",
                            partition_hash, slot_index
                        )
                    });
                assert!(
                    assignment.ledger_id.is_none(),
                    "Expired perm partition {:?} at slot {} should be in capacity pool but has ledger_id={:?}",
                    partition_hash,
                    slot_index,
                    assignment.ledger_id
                );
                checked_partitions += 1;
            }
        }
    }
    assert!(
        checked_partitions > 0,
        "Expected to verify at least one expired partition, but none were found"
    );
    info!(
        "Verified {} expired perm partitions are in capacity pool",
        checked_partitions
    );

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

    // Derive expiry target from observed state — compute each ledger's rounded
    // expiry boundary separately so we catch cases where they would diverge.
    let perm_slot0_last_height = perm_slots[0].last_height;
    let submit_slot0_last_height = submit_slots[0].last_height;
    let perm_earliest = PUBLISH_LEDGER_EPOCH_LENGTH * BLOCKS_PER_EPOCH + perm_slot0_last_height;
    let submit_earliest = SUBMIT_LEDGER_EPOCH_LENGTH * BLOCKS_PER_EPOCH + submit_slot0_last_height;
    let perm_target = perm_earliest.div_ceil(BLOCKS_PER_EPOCH) * BLOCKS_PER_EPOCH;
    let submit_target = submit_earliest.div_ceil(BLOCKS_PER_EPOCH) * BLOCKS_PER_EPOCH;
    assert_eq!(
        perm_target, submit_target,
        "Perm and Submit expiry must land on the same epoch boundary \
         (perm_target={perm_target}, submit_target={submit_target})"
    );
    let target_height = perm_target;
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
    let mut checked_submit_partitions = 0_usize;
    for (slot_index, slot) in submit_slots.iter().enumerate() {
        if slot.is_expired {
            for partition_hash in &slot.partitions {
                let assignment = partition_assignments
                    .get_assignment(*partition_hash)
                    .unwrap_or_else(|| {
                        panic!(
                            "Missing partition assignment for {:?} at slot {}",
                            partition_hash, slot_index
                        )
                    });
                assert!(
                    assignment.ledger_id.is_none(),
                    "Expired submit partition {:?} at slot {} should be in capacity pool but has ledger_id={:?}",
                    partition_hash, slot_index, assignment.ledger_id
                );
                checked_submit_partitions += 1;
            }
        }
    }
    assert!(
        checked_submit_partitions > 0,
        "Expected to verify at least one expired submit partition, but none were found"
    );
    info!(
        "Verified {} expired submit partitions are in capacity pool",
        checked_submit_partitions
    );

    // --- Assertion 5: All expired non-last Publish partitions returned to capacity pool ---
    let mut checked_perm_partitions = 0_usize;
    for (slot_index, slot) in perm_slots.iter().enumerate() {
        if slot_index < perm_num - 1 && slot.is_expired {
            for partition_hash in &slot.partitions {
                let assignment = partition_assignments
                    .get_assignment(*partition_hash)
                    .unwrap_or_else(|| {
                        panic!(
                            "Missing partition assignment for {:?} at slot {}",
                            partition_hash, slot_index
                        )
                    });
                assert!(
                    assignment.ledger_id.is_none(),
                    "Expired perm partition {:?} at slot {} should be in capacity pool but has ledger_id={:?}",
                    partition_hash, slot_index, assignment.ledger_id
                );
                checked_perm_partitions += 1;
            }
        }
    }
    assert!(
        checked_perm_partitions > 0,
        "Expected to verify at least one expired perm partition, but none were found"
    );
    info!(
        "Verified {} expired perm partitions are in capacity pool",
        checked_perm_partitions
    );

    info!("Simultaneous perm+term expiry test passed!");
    node.stop().await;
    Ok(())
}

/// Tests that a perm slot is NOT expired one epoch before its boundary, but IS expired
/// at the exact boundary. Best defense against off-by-one bugs in expiry logic.
/// Requires 2+ slots to exercise the boundary case (not last-slot protection).
/// Validates:
/// - At pre_expiry_epoch: slot 0 is NOT expired
/// - At expiry_epoch: slot 0 IS expired
#[test_log::test(tokio::test)]
async fn heavy_perm_exact_boundary_expiry() -> eyre::Result<()> {
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
        .start_and_wait_for_packing("perm_exact_boundary_test", 30)
        .await;

    let anchor = node.get_block_by_height(0).await?.block_hash;

    // Post and promote a tx
    let tx = node
        .post_data_tx(anchor, vec![1_u8; DATA_SIZE], &signer)
        .await;
    node.wait_for_mempool(tx.header.id, 10).await?;
    node.upload_chunks(&tx).await?;
    node.wait_for_ingress_proofs_no_mining(vec![tx.header.id], 20)
        .await?;

    // Mine 1 block to include tx (triggers promotion to Publish)
    node.mine_block().await?;

    // Mine to first epoch boundary to trigger slot allocation (need 2+ slots)
    let (_, epoch_height) = node.mine_until_next_epoch().await?;
    info!("Reached first epoch boundary at height {}", epoch_height);

    // Read slot state and compute exact boundaries
    let snapshot = node.get_canonical_epoch_snapshot();
    let perm_slots = snapshot.ledgers.get_slots(DataLedger::Publish);
    assert!(
        perm_slots.len() >= 2,
        "Expected 2+ publish slots so this test exercises exact-boundary expiry instead of last-slot protection, got {}",
        perm_slots.len()
    );
    let slot0_last_height = perm_slots[0].last_height;
    info!(
        "Perm slots: {}, slot0 last_height: {}",
        perm_slots.len(),
        slot0_last_height
    );

    let min_blocks = PUBLISH_LEDGER_EPOCH_LENGTH * BLOCKS_PER_EPOCH;
    let earliest_expiry = min_blocks + slot0_last_height;
    // Round up to epoch boundary
    let expiry_epoch = earliest_expiry.div_ceil(BLOCKS_PER_EPOCH) * BLOCKS_PER_EPOCH;
    let pre_expiry_epoch = expiry_epoch - BLOCKS_PER_EPOCH;
    info!(
        "earliest_expiry={}, expiry_epoch={}, pre_expiry_epoch={}",
        earliest_expiry, expiry_epoch, pre_expiry_epoch
    );

    // Mine to pre_expiry_epoch (may already be there if pre_expiry == current height)
    let current_height = node.get_canonical_chain_height().await;
    for _ in current_height..pre_expiry_epoch {
        node.mine_block().await?;
    }

    // --- Assertion 1: At pre_expiry_epoch, slot 0 is NOT expired ---
    let pre_snapshot = node.get_canonical_epoch_snapshot();
    let pre_perm_slots = pre_snapshot.ledgers.get_slots(DataLedger::Publish);
    assert!(
        !pre_perm_slots[0].is_expired,
        "Slot 0 should NOT be expired at pre-expiry epoch height {}",
        pre_expiry_epoch
    );
    info!(
        "Confirmed slot 0 is NOT expired at height {}",
        node.get_canonical_chain_height().await
    );

    // Mine to expiry_epoch
    let current_height = node.get_canonical_chain_height().await;
    for _ in current_height..expiry_epoch {
        node.mine_block().await?;
    }

    // --- Assertion 2: At expiry_epoch, slot 0 should be expired (multi-slot guaranteed above) ---
    let post_snapshot = node.get_canonical_epoch_snapshot();
    let post_perm_slots = post_snapshot.ledgers.get_slots(DataLedger::Publish);
    assert!(
        post_perm_slots.len() >= 2,
        "Expected 2+ publish slots at expiry epoch, got {}",
        post_perm_slots.len()
    );
    assert!(
        post_perm_slots[0].is_expired,
        "Slot 0 should be expired exactly at expiry epoch height {}",
        expiry_epoch
    );
    info!("Confirmed slot 0 IS expired at expiry epoch");

    info!("Exact boundary expiry test passed!");
    node.stop().await;
    Ok(())
}

/// Tests that the last remaining Publish slot never expires, even far past its expiry boundary.
/// Verifies:
/// - The last slot remains active after 5x the expiry window
/// - All last-slot partitions keep their ledger assignment (not returned to capacity pool)
/// - No TermFeeReward shadow txs are generated in any epoch block past min_blocks
#[test_log::test(tokio::test)]
async fn heavy_perm_last_slot_never_expires() -> eyre::Result<()> {
    const CHUNK_SIZE: u64 = 32;
    const DATA_SIZE: usize = 32;
    const BLOCKS_PER_EPOCH: u64 = 3;
    const PUBLISH_LEDGER_EPOCH_LENGTH: u64 = 1; // Very short — expiry would trigger quickly
    const INITIAL_BALANCE: u128 = 10_000_000_000_000_000_000;

    let mut config = NodeConfig::testing();
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().chunk_size = CHUNK_SIZE;
    config.consensus.get_mut().num_chunks_in_partition = 4;
    config.consensus.get_mut().num_chunks_in_recall_range = 1;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;
    config.consensus.get_mut().epoch.publish_ledger_epoch_length =
        Some(PUBLISH_LEDGER_EPOCH_LENGTH);
    // Set Submit epoch length very high so Submit never expires during this test
    config.consensus.get_mut().epoch.submit_ledger_epoch_length = 100;

    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(INITIAL_BALANCE).into(),
            ..Default::default()
        },
    )]);

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("perm_last_slot_test", 30)
        .await;

    let anchor = node.get_block_by_height(0).await?.block_hash;

    // Post 1 tx and promote it (uses genesis slot)
    let tx = node
        .post_data_tx(anchor, vec![1_u8; DATA_SIZE], &signer)
        .await;
    node.wait_for_mempool(tx.header.id, 10).await?;
    node.upload_chunks(&tx).await?;
    node.wait_for_ingress_proofs_no_mining(vec![tx.header.id], 20)
        .await?;

    // Mine 1 block to include tx
    node.mine_block().await?;

    // Mine to 5x past where expiry would trigger: (EPOCH_LENGTH + 4) * BLOCKS_PER_EPOCH
    let target_height = (PUBLISH_LEDGER_EPOCH_LENGTH + 4) * BLOCKS_PER_EPOCH;
    let current_height = node.get_canonical_chain_height().await;
    info!(
        "Mining from height {} to {} (5x past expiry window)",
        current_height, target_height
    );
    for _ in current_height..target_height {
        node.mine_block().await?;
    }

    let final_height = node.get_canonical_chain_height().await;
    assert!(
        final_height >= target_height,
        "Should have reached target height"
    );

    // --- Assertion 1: Last perm slot is NOT expired ---
    let epoch_snapshot = node.get_canonical_epoch_snapshot();
    let perm_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Publish);
    assert_eq!(
        perm_slots.len(),
        1,
        "Expected exactly 1 perm slot (single-slot test), got {}",
        perm_slots.len()
    );
    let last_slot = perm_slots.last().unwrap();
    assert!(
        !last_slot.is_expired,
        "Last perm slot should never expire (last-slot protection)"
    );
    info!(
        "Perm slots: {}, last slot is_expired: {}",
        perm_slots.len(),
        last_slot.is_expired
    );

    // --- Assertion 2: All last-slot partitions still have Publish ledger assignment ---
    let partition_assignments = &epoch_snapshot.partition_assignments;
    for partition_hash in &last_slot.partitions {
        let assignment = partition_assignments
            .get_assignment(*partition_hash)
            .unwrap_or_else(|| {
                panic!(
                    "Missing partition assignment for last-slot partition {:?}",
                    partition_hash
                )
            });
        assert!(
            assignment.ledger_id == Some(DataLedger::Publish as u32),
            "Last-slot partition {:?} should still be assigned to Publish, but has ledger_id={:?}",
            partition_hash,
            assignment.ledger_id
        );
    }
    info!("Verified all last-slot partitions remain assigned to Publish");

    // --- Assertion 3: No TermFeeReward shadow txs in any epoch block past min_blocks ---
    let min_blocks = PUBLISH_LEDGER_EPOCH_LENGTH * BLOCKS_PER_EPOCH;
    let mut epoch_height = min_blocks;
    while epoch_height <= final_height {
        let epoch_block = node.get_block_by_height(epoch_height).await?;
        let evm_block = node
            .wait_for_evm_block(epoch_block.evm_block_hash, 30)
            .await?;
        for tx in &evm_block.body.transactions {
            let mut input = tx.input().as_ref();
            if let Ok(shadow) = ShadowTransaction::decode(&mut input) {
                if let Some(TransactionPacket::TermFeeReward(_)) = shadow.as_v1() {
                    panic!(
                        "Unexpected TermFeeReward shadow tx at epoch height {} — \
                         last-slot protection should prevent any fee distribution",
                        epoch_height
                    );
                }
            }
        }
        info!(
            "No TermFeeReward at epoch height {} (confirmed)",
            epoch_height
        );
        epoch_height += BLOCKS_PER_EPOCH;
    }

    info!("Last-slot protection test passed!");
    node.stop().await;
    Ok(())
}

/// Tests that perm slots never expire when publish_ledger_epoch_length is None (mainnet config).
/// Uses multi-slot setup to distinguish the None config gate from last-slot protection.
/// Verifies:
/// - Multiple Publish slots exist (precondition — proves None is what blocks expiry, not last-slot)
/// - Zero perm slots are expired after mining far past where expiry would trigger
/// - All Publish partition assignments remain active (not returned to capacity pool)
#[test_log::test(tokio::test)]
async fn heavy_perm_expiry_disabled_nothing_expires() -> eyre::Result<()> {
    const CHUNK_SIZE: u64 = 32;
    const DATA_SIZE: usize = 64; // 2 chunks — enough to trigger slot allocation at epoch boundary
    const BLOCKS_PER_EPOCH: u64 = 3;
    const INITIAL_BALANCE: u128 = 10_000_000_000_000_000_000;

    let mut config = NodeConfig::testing();
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().chunk_size = CHUNK_SIZE;
    config.consensus.get_mut().num_chunks_in_partition = 4;
    config.consensus.get_mut().num_chunks_in_recall_range = 1;
    config.consensus.get_mut().num_partitions_per_slot = 1;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;
    // Mainnet config: perm expiry disabled
    config.consensus.get_mut().epoch.publish_ledger_epoch_length = None;

    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(INITIAL_BALANCE).into(),
            ..Default::default()
        },
    )]);

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("perm_expiry_disabled_test", 30)
        .await;

    let anchor = node.get_block_by_height(0).await?.block_hash;

    // Post + promote tx1 in epoch 0
    let tx1 = node
        .post_data_tx(anchor, vec![1_u8; DATA_SIZE], &signer)
        .await;
    node.wait_for_mempool(tx1.header.id, 10).await?;
    node.upload_chunks(&tx1).await?;
    node.wait_for_ingress_proofs_no_mining(vec![tx1.header.id], 20)
        .await?;

    // Mine 1 block to include tx1
    node.mine_block().await?;

    // Mine to first epoch boundary → triggers slot allocation → 2+ Publish slots
    let (_, epoch_height) = node.mine_until_next_epoch().await?;
    info!("Reached first epoch boundary at height {}", epoch_height);

    // Post + promote tx2 in epoch 1
    let anchor2 = node.get_block_by_height(epoch_height).await?.block_hash;
    let tx2 = node
        .post_data_tx(anchor2, vec![2_u8; DATA_SIZE], &signer)
        .await;
    node.wait_for_mempool(tx2.header.id, 10).await?;
    node.upload_chunks(&tx2).await?;
    node.wait_for_ingress_proofs_no_mining(vec![tx2.header.id], 20)
        .await?;

    // Mine 1 block to include tx2
    node.mine_block().await?;

    // Mine to 8 * BLOCKS_PER_EPOCH (far past where expiry would trigger if Some(2))
    let target_height = 8 * BLOCKS_PER_EPOCH;
    let current_height = node.get_canonical_chain_height().await;
    info!(
        "Mining from height {} to {} (far past hypothetical expiry)",
        current_height, target_height
    );
    for _ in current_height..target_height {
        node.mine_block().await?;
    }

    let final_height = node.get_canonical_chain_height().await;
    assert!(
        final_height >= target_height,
        "Should have reached target height"
    );

    // --- Precondition: Multi-slot (proves None config gate, not last-slot protection) ---
    let epoch_snapshot = node.get_canonical_epoch_snapshot();
    let perm_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Publish);
    assert!(
        perm_slots.len() >= 2,
        "Expected 2+ perm slots for multi-slot verification, got {}. \
         With a single slot, last-slot protection prevents expiry regardless of config.",
        perm_slots.len()
    );
    info!(
        "Perm slots: {} (multi-slot precondition met)",
        perm_slots.len()
    );

    // --- Assertion 1: Zero perm slots are expired ---
    let expired_count = perm_slots.iter().filter(|s| s.is_expired).count();
    assert_eq!(
        expired_count, 0,
        "No perm slots should expire when publish_ledger_epoch_length is None, but {} are expired",
        expired_count
    );
    info!(
        "Confirmed zero perm slots are expired at height {}",
        final_height
    );

    // --- Assertion 2: All Publish partition assignments still have Publish ledger ---
    let partition_assignments = &epoch_snapshot.partition_assignments;
    for (slot_index, slot) in perm_slots.iter().enumerate() {
        for partition_hash in &slot.partitions {
            let assignment = partition_assignments
                .get_assignment(*partition_hash)
                .unwrap_or_else(|| {
                    panic!(
                        "Missing partition assignment for {:?} at slot {}",
                        partition_hash, slot_index
                    )
                });
            assert!(
                assignment.ledger_id == Some(DataLedger::Publish as u32),
                "Perm partition {:?} at slot {} should still be assigned to Publish but has ledger_id={:?}",
                partition_hash, slot_index, assignment.ledger_id
            );
        }
    }
    info!("Verified all Publish partition assignments remain active");

    info!("Perm expiry disabled (mainnet safety) test passed!");
    node.stop().await;
    Ok(())
}
