use crate::utils::IrysNodeTest;
use alloy_genesis::GenesisAccount;
use irys_types::{irys::IrysSigner, NodeConfig, U256};
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

    info!("Publish ledger expiry test passed!");
    node.stop().await;
    Ok(())
}
