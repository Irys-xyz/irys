use crate::utils::IrysNodeTest;
use irys_testing_utils::initialize_tracing;
use irys_types::{irys::IrysSigner, DataLedger, NodeConfig, H256};

#[actix_web::test]
/// demonstrate that duplicate txs are rejected from mempool
/// demonstrate that duplicate txs are blocked from the mempool when tx is in database after block migration
async fn heavy_double_spend_rejection_after_block_migration() -> eyre::Result<()> {
    // enable logs for troubleshooting
    std::env::set_var("RUST_LOG", "debug");
    initialize_tracing();

    // basic node config
    let seconds_to_wait = 10;
    let mut config = NodeConfig::testnet();
    config.consensus.get_mut().chunk_size = 32;
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);

    // start node
    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // create and submit a data transaction
    let tx_data = vec![1u8; 64];
    let anchor = H256::zero();
    let tx = node.post_data_tx(anchor, tx_data, &signer).await;
    let txid = tx.header.id;
    tracing::error!("txid: {:?}", txid);
    node.wait_for_mempool(txid, seconds_to_wait).await?;

    // mine block including tx
    node.mine_block().await?;
    let block1 = node.get_block_by_height(1).await?;
    assert!(block1
        .get_data_ledger_tx_ids()
        .get(&DataLedger::Submit)
        .unwrap()
        .contains(&txid));

    // mine enough blocks to cause block with txs to migrate to index
    node.mine_blocks(
        config
            .consensus
            .get_mut()
            .block_migration_depth
            .try_into()?,
    )
    .await?;

    // ensure block with tx is now in index
    node.wait_until_block_index_height(block1.height, seconds_to_wait)
        .await?;

    // resubmit same tx header
    node.post_data_tx_raw(&tx.header).await;

    // ensure mempool does not accept duplicate
    node.wait_for_mempool_shape(0, 0, 0, seconds_to_wait.try_into()?)
        .await?;

    // mine another block
    node.mine_block().await?;
    let block2 = node.get_block_by_height(2).await?;
    assert!(!block2
        .get_data_ledger_tx_ids()
        .get(&DataLedger::Submit)
        .unwrap()
        .contains(&txid));
    panic!("show me some debug");
    Ok(())
}
