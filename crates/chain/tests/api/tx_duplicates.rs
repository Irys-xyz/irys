use crate::utils::IrysNodeTest;
use irys_primitives::CommitmentType;
use irys_testing_utils::initialize_tracing;
use irys_types::{irys::IrysSigner, CommitmentTransaction, DataLedger, NodeConfig, H256};

#[actix_web::test]
async fn test_rejection_of_duplicate_tx() -> eyre::Result<()> {
    // ===== TEST ENVIRONMENT SETUP =====
    std::env::set_var("RUST_LOG", "debug");
    initialize_tracing();

    // Default test node config
    let seconds_to_wait = 10;
    let num_blocks_in_epoch = 8;
    let mut config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    config.consensus.get_mut().chunk_size = 32;
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);

    // Start a test node with default configuration
    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Initialize blockchain components
    node.start_mining().await;

    // ===== TEST CASE 1: post duplicate data tx =====
    let chunks = vec![[10; 32], [20; 32], [30; 32]];
    let mut data: Vec<u8> = Vec::new();
    for chunk in chunks.iter() {
        data.extend_from_slice(chunk);
    }

    // Then post the tx and wait for it to arrive
    let anchor = H256::zero(); // Genesis block
    let data_tx = node.post_data_tx(anchor, data, &signer).await;
    let tx = data_tx.header.clone();
    let txid = tx.id;
    node.wait_for_mempool(txid, seconds_to_wait).await?;

    // Mine a block and verify that it is included in a block
    node.mine_block().await?;
    let block1 = node.get_block_by_height(1).await?;
    let txid_map = block1.get_data_ledger_tx_ids();
    assert!(txid_map.get(&DataLedger::Submit).unwrap().contains(&txid));

    // Post the chunks for the data tx
    for i in 0..chunks.len() {
        node.post_chunk_32b(&data_tx, i, &chunks).await;
    }

    // Re-post the transaction as duplicate of an already included txid
    node.post_data_tx_raw(&tx).await;

    // Wait for mempool should return immediately because the transaction is already known
    node.wait_for_mempool(txid, seconds_to_wait).await?;

    // Mine a block and verify the duplicate tx is not included again
    node.mine_block().await?;
    let block2 = node.get_block_by_height(2).await?;
    let txid_map = block2.get_data_ledger_tx_ids();
    assert_eq!(
        txid_map.get(&DataLedger::Submit).unwrap().contains(&txid),
        false
    );

    // Verify the tx is published
    assert!(txid_map.get(&DataLedger::Publish).unwrap().contains(&txid));
    assert_eq!(txid_map.get(&DataLedger::Publish).unwrap().len(), 1);

    // ===== TEST CASE 2: post duplicate commitment tx =====
    let stake_tx = CommitmentTransaction {
        commitment_type: CommitmentType::Stake,
        fee: 1,
        ..Default::default()
    };

    // Post the stake commitment and await it in the mempool
    let stake_tx = signer.sign_commitment(stake_tx).unwrap();
    node.post_commitment_tx(&stake_tx).await;
    node.wait_for_mempool_commitment_txs(vec![stake_tx.id], seconds_to_wait)
        .await?;

    // Mine a block and verify the stake commitment is included
    node.mine_block().await?;
    let block3 = node.get_block_by_height(3).await?;
    let tx_ids = block3.get_commitment_ledger_tx_ids();
    assert_eq!(tx_ids, vec![stake_tx.id]);

    // Post the stake commitment again
    node.post_commitment_tx(&stake_tx).await;
    node.wait_for_mempool_commitment_txs(vec![stake_tx.id], seconds_to_wait)
        .await?;

    // Mine a block and make sure the commitment isn't included again
    node.mine_block().await?;
    let block4 = node.get_block_by_height(4).await?;
    let tx_ids = block4.get_commitment_ledger_tx_ids();
    assert_eq!(tx_ids, vec![]);

    // ===== TEST CASE 3: post duplicate pledge tx =====
    let pledge_tx = CommitmentTransaction {
        commitment_type: CommitmentType::Pledge,
        anchor,
        fee: 1,
        ..Default::default()
    };
    let pledge_tx = signer.sign_commitment(pledge_tx).unwrap();

    // Post pledge commitment
    node.post_commitment_tx(&pledge_tx).await;
    node.wait_for_mempool_commitment_txs(vec![pledge_tx.id], seconds_to_wait)
        .await?;

    // Mine a block and verify the pledge commitment is included
    node.mine_block().await?;
    let block5 = node.get_block_by_height(5).await?;
    let tx_ids = block5.get_commitment_ledger_tx_ids();
    assert_eq!(tx_ids, vec![pledge_tx.id]);

    // Post the pledge commitment again
    node.post_commitment_tx(&pledge_tx).await;
    node.wait_for_mempool_commitment_txs(vec![pledge_tx.id], seconds_to_wait)
        .await?;

    // Mine a block and verify the pledges is not included again
    node.mine_block().await?;
    let block6 = node.get_block_by_height(6).await?;
    let tx_ids = block6.get_commitment_ledger_tx_ids();
    assert_eq!(tx_ids, vec![]);

    // ===== TEST CASE 4: mine an epoch block and test duplicates again =====
    node.mine_blocks(2).await?;
    let block8 = node.get_block_by_height(8).await?;
    let tx_ids = block8.get_commitment_ledger_tx_ids();

    // Validate the stake and pledge tx are in the commitments roll up
    assert_eq!(tx_ids, vec![stake_tx.id, pledge_tx.id]);

    // Post all the transactions again
    node.post_data_tx_raw(&tx).await;
    node.post_commitment_tx(&stake_tx).await;
    node.post_commitment_tx(&pledge_tx).await;
    node.wait_for_mempool_commitment_txs(vec![stake_tx.id, pledge_tx.id], seconds_to_wait)
        .await?;

    // Post the chunks for the data again
    for i in 0..chunks.len() {
        node.post_chunk_32b(&data_tx, i, &chunks).await;
    }

    // Mine a block and validate that none of them are included
    node.mine_block().await?;
    let block9 = node.get_block_by_height(9).await?;
    let txid_map = block9.get_data_ledger_tx_ids();
    assert_eq!(txid_map.get(&DataLedger::Submit).unwrap().len(), 0);
    let tx_ids = block9.get_commitment_ledger_tx_ids();
    assert_eq!(tx_ids, vec![]);

    // Validate the data tx is not published again
    assert_eq!(txid_map.get(&DataLedger::Publish).unwrap().len(), 0);

    Ok(())
}
