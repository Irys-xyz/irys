use crate::utils::IrysNodeTest;
use irys_types::{irys::IrysSigner, CommitmentTransaction, DataLedger, NodeConfig, H256};

#[test_log::test(actix_web::test)]
async fn heavy_test_rejection_of_duplicate_tx() -> eyre::Result<()> {
    // ===== TEST ENVIRONMENT SETUP =====
    // Default test node config
    let seconds_to_wait = 10;
    let num_blocks_in_epoch = 15;
    let mut config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    config.consensus.get_mut().chunk_size = 32;
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);

    // Start a test node with default configuration
    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

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
    assert_eq!(node.get_canonical_chain_height().await, 1);
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

    // Wait for ingress proofs using the shared helper, then mine the next block to publish
    node.wait_for_ingress_proofs(vec![txid], seconds_to_wait)
        .await?;

    // loop through the block chain and confirm the tx was in Submit, and then Publish ledgers.
    node.assert_tx_progresses_submit_then_publish(txid).await?;

    // ===== TEST CASE 2: post duplicate commitment tx =====
    let consensus = &node.node_ctx.config.consensus;
    let stake_tx = CommitmentTransaction::new_stake(consensus, H256::default());

    // Post the stake commitment and await until its in the mempool
    let stake_tx = signer.sign_commitment(stake_tx).unwrap();
    node.post_commitment_tx(&stake_tx).await?;
    node.wait_for_mempool_commitment_txs(vec![stake_tx.id], seconds_to_wait)
        .await?;

    // Mine a block and verify the stake commitment is included
    let block = node.mine_block().await?;
    let tx_ids = block.get_commitment_ledger_tx_ids();
    let txid_map = block.get_data_ledger_tx_ids();
    assert_eq!(tx_ids, vec![stake_tx.id]);
    assert_eq!(txid_map.get(&DataLedger::Submit).unwrap().len(), 0);
    assert_eq!(txid_map.get(&DataLedger::Publish).unwrap().len(), 0);

    // Post the stake commitment again
    node.post_commitment_tx(&stake_tx).await?;
    node.wait_for_mempool_commitment_txs(vec![stake_tx.id], seconds_to_wait)
        .await?;

    // Mine a block and make sure the commitment isn't included again
    let block = node.mine_block().await?;
    let tx_ids = block.get_commitment_ledger_tx_ids();
    let txid_map = block.get_data_ledger_tx_ids();
    assert_eq!(tx_ids, vec![]);
    assert_eq!(txid_map.get(&DataLedger::Submit).unwrap().len(), 0);
    assert_eq!(txid_map.get(&DataLedger::Publish).unwrap().len(), 0);

    // ===== TEST CASE 3: post duplicate pledge tx =====
    // Get the CommitmentSnapshot from the latest canonical block
    let pledge_tx = CommitmentTransaction::new_pledge(
        consensus,
        anchor,
        node.node_ctx.mempool_pledge_provider.as_ref(),
        signer.address(),
    )
    .await;
    let pledge_tx = signer.sign_commitment(pledge_tx).unwrap();

    // Post pledge commitment
    node.post_commitment_tx(&pledge_tx).await?;
    node.wait_for_mempool_commitment_txs(vec![pledge_tx.id], seconds_to_wait)
        .await?;

    // Mine a block and verify the pledge commitment is included
    let block = node.mine_block().await?;
    let tx_ids = block.get_commitment_ledger_tx_ids();
    let txid_map = block.get_data_ledger_tx_ids();
    assert_eq!(tx_ids, vec![pledge_tx.id]);
    assert_eq!(txid_map.get(&DataLedger::Submit).unwrap().len(), 0);
    assert_eq!(txid_map.get(&DataLedger::Publish).unwrap().len(), 0);

    // Post the pledge commitment again
    node.post_commitment_tx(&pledge_tx).await?;
    node.wait_for_mempool_commitment_txs(vec![pledge_tx.id], seconds_to_wait)
        .await?;

    // Mine a block and verify the pledge is not included again
    let block = node.mine_block().await?;
    let tx_ids = block.get_commitment_ledger_tx_ids();
    let txid_map = block.get_data_ledger_tx_ids();
    assert_eq!(tx_ids, vec![]);
    assert_eq!(txid_map.get(&DataLedger::Submit).unwrap().len(), 0);
    assert_eq!(txid_map.get(&DataLedger::Publish).unwrap().len(), 0);

    // ===== TEST CASE 4: ensure we have mined an epoch block and test duplicates again =====
    let block = {
        let mut epoch_block = node.get_last_epoch_block().await?;
        // mine until we generate an epoch block
        for _ in 0..10 {
            if epoch_block.height == 0 {
                node.mine_block().await?;
                epoch_block = node.get_last_epoch_block().await?;
            }
        }
        assert!(epoch_block.height > 0);
        epoch_block
    };
    let tx_ids = block.get_commitment_ledger_tx_ids();
    let txid_map = block.get_data_ledger_tx_ids();
    assert_eq!(txid_map.get(&DataLedger::Submit).unwrap().len(), 0);
    assert_eq!(txid_map.get(&DataLedger::Publish).unwrap().len(), 0);

    // Validate the stake and pledge tx are in the commitments roll up
    assert_eq!(tx_ids, vec![stake_tx.id, pledge_tx.id]);

    // Post all the transactions again
    node.post_data_tx_raw(&tx).await;
    node.post_commitment_tx(&stake_tx).await?;
    node.post_commitment_tx(&pledge_tx).await?;
    node.wait_for_mempool(tx.id, seconds_to_wait).await?;

    // Post the chunks for the data again
    for i in 0..chunks.len() {
        node.post_chunk_32b(&data_tx, i, &chunks).await;
    }

    // Mine a block and validate that none of them are included
    let block = node.mine_block().await?;
    let txid_map = block.get_data_ledger_tx_ids();
    assert_eq!(txid_map.get(&DataLedger::Submit).unwrap().len(), 0);
    let tx_ids = block.get_commitment_ledger_tx_ids();
    assert_eq!(tx_ids, vec![]);

    // Validate the data tx is not published again
    assert_eq!(
        txid_map.get(&DataLedger::Publish).unwrap().len(),
        0,
        "publish txs found: {:?}",
        txid_map
            .get(&DataLedger::Publish)
            .unwrap()
            .iter()
            .collect::<Vec<&H256>>()
    );

    Ok(())
}
