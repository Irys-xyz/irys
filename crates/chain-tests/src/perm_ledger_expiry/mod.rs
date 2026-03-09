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
#[test_log::test(tokio::test)]
async fn heavy_perm_ledger_expiry_basic() -> eyre::Result<()> {
    const CHUNK_SIZE: u64 = 32;
    const DATA_SIZE: usize = 32; // 1 chunk per tx
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
    node.wait_for_ingress_proofs(vec![tx.header.id], 20).await?;

    // Mine through epochs until expiry
    let target_height = (PUBLISH_LEDGER_EPOCH_LENGTH + 1) * BLOCKS_PER_EPOCH;
    info!("Mining to height {} to trigger perm expiry", target_height);

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
    // The block at the expiry boundary must exist since we asserted final_height >= target_height.
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

    info!("Publish ledger expiry test passed!");
    node.stop().await;
    Ok(())
}
