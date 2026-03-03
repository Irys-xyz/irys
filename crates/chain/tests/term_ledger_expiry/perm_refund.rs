use crate::utils::IrysNodeTest;
use alloy_genesis::GenesisAccount;
use irys_reth_node_bridge::irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};
use irys_types::{irys::IrysSigner, DataLedger, NodeConfig, U256};
use reth::rpc::types::TransactionTrait as _;
use tracing::{debug, info};

/// Tests that permanent fees are refunded for unpromoted transactions when they expire.
/// - User1's transaction remains unpromoted and receives full perm_fee refund at expiry
/// - User2's transaction gets promoted (chunks uploaded) and does NOT receive perm_fee refund
#[test_log::test(tokio::test)]
async fn heavy_perm_fee_refund_for_unpromoted_tx() -> eyre::Result<()> {
    // ==================== SETUP ====================
    // Configure node with short epochs for faster testing
    const CHUNK_SIZE: u64 = 32;
    const DATA_SIZE: usize = 512;
    const INITIAL_BALANCE: u128 = 10_000_000_000_000_000_000; // 10 IRYS
    const SUBMIT_LEDGER_EPOCH_LENGTH: u64 = 2; // Txs expire after 2 epochs
    const BLOCKS_PER_EPOCH: u64 = 3;

    let mut config = NodeConfig::testing();
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().chunk_size = CHUNK_SIZE;
    config.consensus.get_mut().num_chunks_in_partition = 4;
    config.consensus.get_mut().epoch.submit_ledger_epoch_length = SUBMIT_LEDGER_EPOCH_LENGTH;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;

    // Create and fund two users
    let user1_signer = IrysSigner::random_signer(&config.consensus_config());
    let user2_signer = IrysSigner::random_signer(&config.consensus_config());
    let user1_address = user1_signer.address();
    let user2_address = user2_signer.address();
    let initial_balance = U256::from(INITIAL_BALANCE);

    config.consensus.extend_genesis_accounts(vec![
        (
            user1_address,
            GenesisAccount {
                balance: initial_balance.into(),
                ..Default::default()
            },
        ),
        (
            user2_address,
            GenesisAccount {
                balance: initial_balance.into(),
                ..Default::default()
            },
        ),
    ]);

    // Start node and get anchor
    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("perm_refund_test", 30)
        .await;
    let anchor = node.get_block_by_height(0).await?.block_hash;

    // ==================== ACTION ====================
    // Post two transactions with permanent fees
    info!("Posting transactions from both users");

    // User1: Transaction that will NOT be promoted (no chunks uploaded)
    let tx1 = node
        .post_data_tx(anchor, vec![1_u8; DATA_SIZE], &user1_signer)
        .await;
    let tx1_perm_fee = tx1
        .header
        .perm_fee
        .expect("Transaction should have perm_fee");
    node.wait_for_mempool(tx1.header.id, 10).await?;
    debug!("tx1: id={}, perm_fee={}", tx1.header.id, tx1_perm_fee);

    // User2: Transaction that WILL be promoted (chunks uploaded)
    let tx2 = node
        .post_data_tx(anchor, vec![2_u8; DATA_SIZE], &user2_signer)
        .await;
    let tx2_perm_fee = tx2
        .header
        .perm_fee
        .expect("Transaction should have perm_fee");
    node.wait_for_mempool(tx2.header.id, 10).await?;
    debug!("tx2: id={}, perm_fee={}", tx2.header.id, tx2_perm_fee);

    // Mine block to include both transactions in Submit ledger
    info!("Mining block to include transactions");
    let block1 = node.mine_block().await?;
    let tx_ids = block1.get_data_ledger_tx_ids();
    let submit_txs = tx_ids
        .get(&DataLedger::Submit)
        .expect("Submit ledger should have transactions");
    assert!(
        submit_txs.contains(&tx1.header.id) && submit_txs.contains(&tx2.header.id),
        "Both transactions should be in Submit ledger"
    );

    // Upload chunks for tx2 to trigger promotion
    info!("Promoting tx2 by uploading chunks");
    node.upload_chunks(&tx2).await?;
    node.wait_for_ingress_proofs(vec![tx2.header.id], 20)
        .await?;

    // Complete first epoch
    node.mine_until_next_epoch().await?;

    // Mine to expiry height (transactions expire at start of epoch SUBMIT_LEDGER_EPOCH_LENGTH)
    let target_expiry_height = SUBMIT_LEDGER_EPOCH_LENGTH * BLOCKS_PER_EPOCH;
    info!("Mining to expiry height {}", target_expiry_height);

    let current_height = node.get_canonical_chain_height().await;
    let epochs_to_mine = (target_expiry_height - current_height).div_ceil(BLOCKS_PER_EPOCH);

    for _ in 0..epochs_to_mine {
        let (_, height) = node.mine_until_next_epoch().await?;
        if height >= target_expiry_height {
            break;
        }
    }

    // ==================== ASSERT ====================
    // Extract PermFeeRefund transactions from the expiry block
    info!("Checking for perm fee refunds at expiry");
    let final_block = node.get_block_by_height(target_expiry_height).await?;
    let final_evm_block = node
        .wait_for_evm_block(final_block.evm_block_hash, 20)
        .await?;
    let refunds: Vec<_> = final_evm_block
        .body
        .transactions
        .into_iter()
        .filter(|tx| tx.input().len() >= 4)
        .filter_map(|tx| {
            let shadow_tx = ShadowTransaction::decode(&mut tx.input().as_ref()).ok()?;
            let packet = shadow_tx.as_v1()?;
            match packet {
                TransactionPacket::PermFeeRefund(refund) => Some(refund.clone()),
                _ => None,
            }
        })
        .collect();

    // Calculate refund amounts for each user
    let user1_refund_amount = refunds
        .iter()
        .filter(|r| r.target == user1_address.to_alloy_address())
        .map(|r| U256::from_le_bytes(r.amount.to_le_bytes()))
        .fold(U256::from(0), irys_types::U256::saturating_add);

    let user2_refund_amount = refunds
        .iter()
        .filter(|r| r.target == user2_address.to_alloy_address())
        .map(|r| U256::from_le_bytes(r.amount.to_le_bytes()))
        .fold(U256::from(0), irys_types::U256::saturating_add);

    // Verify refunds: unpromoted tx gets refund, promoted tx does not
    assert_eq!(
        user1_refund_amount, tx1_perm_fee,
        "Unpromoted tx should receive perm_fee refund"
    );
    assert_eq!(
        user2_refund_amount,
        U256::from(0),
        "Promoted tx should NOT receive perm_fee refund"
    );

    // Verify final balances
    let user1_final = U256::from_be_bytes(
        node.get_balance(user1_address, final_block.evm_block_hash)
            .await
            .to_be_bytes(),
    );
    let user2_final = U256::from_be_bytes(
        node.get_balance(user2_address, final_block.evm_block_hash)
            .await
            .to_be_bytes(),
    );

    // User1: pays total_cost but gets perm_fee refunded
    let user1_expected = initial_balance
        .saturating_sub(tx1.header.total_cost().into())
        .saturating_add(tx1_perm_fee.into());

    // User2: pays full total_cost (no refund for promoted tx)
    let user2_expected = initial_balance.saturating_sub(tx2.header.total_cost().into());

    assert_eq!(
        user1_final, user1_expected,
        "User1 balance should reflect perm_fee refund"
    );
    assert_eq!(
        user2_final, user2_expected,
        "User2 balance should not include refund"
    );
    info!("Test passed: Unpromoted tx received refund, promoted tx did not");

    Ok(())
}
