use crate::utils::IrysNodeTest;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_types::{hardfork_config::Cascade, BoundedFee, DataLedger, NodeConfig, UnixTimestamp};
use std::sync::Arc;

/// Verify that when a reorg occurs, OneYear and ThirtyDay term ledger transactions
/// confirmed in the old fork are re-submitted to the mempool and become includable
/// in subsequent blocks on the new canonical chain.
#[test_log::test(tokio::test)]
async fn slow_heavy_cascade_reorg_reinjects_term_ledger_txs() -> eyre::Result<()> {
    let seconds_to_wait = 20_usize;
    // Use epoch size 10 (matching fork_recovery tests) to avoid epoch boundary
    // issues when mining isolated forks.
    let num_blocks_in_epoch = 10_u64;
    let activation_height = num_blocks_in_epoch;

    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch as usize);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().hardforks.cascade = Some(Cascade {
        activation_timestamp: UnixTimestamp::from_secs(0),
        one_year_epoch_length: 365,
        thirty_day_epoch_length: 30,
        annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
    });

    let peer_signer = genesis_config.new_random_signer();
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer, &signer]);

    // Pre-configure 5 storage submodules so there are enough partitions for
    // all 4 data ledgers (Publish, Submit, OneYear, ThirtyDay).
    let genesis_test = IrysNodeTest::new_genesis(genesis_config.clone());
    StorageSubmodulesConfig::load_for_test(genesis_test.cfg.base_directory.clone(), 5)?;
    let genesis_node = genesis_test
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let peer_config = genesis_node.testing_peer_with_signer(&peer_signer);
    let peer_node = genesis_node
        .testing_peer_with_assignments_and_name(peer_config, "PEER")
        .await?;

    // Mine past the Cascade activation height so term ledgers are available
    while genesis_node.get_canonical_chain_height().await < activation_height {
        let mined = genesis_node.mine_block().await?;
        peer_node
            .wait_until_height(mined.height, seconds_to_wait)
            .await?;
    }

    // Isolate both nodes so they produce independent forks
    genesis_node.gossip_disable();
    peer_node.gossip_disable();

    let one_year_data = vec![11_u8; 96]; // 3 chunks x 32 bytes
    let thirty_day_data = vec![22_u8; 96];

    let one_year_price = genesis_node
        .get_data_price(DataLedger::OneYear, one_year_data.len() as u64)
        .await?;
    let thirty_day_price = genesis_node
        .get_data_price(DataLedger::ThirtyDay, thirty_day_data.len() as u64)
        .await?;

    let one_year_tx = signer.create_transaction_with_fees(
        one_year_data,
        genesis_node.get_anchor().await?,
        DataLedger::OneYear,
        BoundedFee::new(one_year_price.term_fee),
        Some(BoundedFee::default()),
    )?;
    let one_year_tx = signer.sign_transaction(one_year_tx)?;

    let thirty_day_tx = signer.create_transaction_with_fees(
        thirty_day_data,
        genesis_node.get_anchor().await?,
        DataLedger::ThirtyDay,
        BoundedFee::new(thirty_day_price.term_fee),
        Some(BoundedFee::default()),
    )?;
    let thirty_day_tx = signer.sign_transaction(thirty_day_tx)?;

    // Ingest into genesis node's mempool only (gossip is off)
    genesis_node
        .ingest_data_tx(one_year_tx.header.clone())
        .await?;
    genesis_node
        .ingest_data_tx(thirty_day_tx.header.clone())
        .await?;

    genesis_node
        .wait_for_mempool(one_year_tx.header.id, seconds_to_wait)
        .await?;
    genesis_node
        .wait_for_mempool(thirty_day_tx.header.id, seconds_to_wait)
        .await?;

    let base_height = genesis_node.get_canonical_chain_height().await;

    // Both nodes mine one block in isolation — genesis includes the term txs
    tokio::try_join!(
        genesis_node.mine_blocks_without_gossip(1),
        peer_node.mine_blocks_without_gossip(1)
    )?;

    genesis_node
        .wait_until_height(base_height + 1, seconds_to_wait)
        .await?;
    peer_node
        .wait_until_height(base_height + 1, seconds_to_wait)
        .await?;

    // Verify the genesis fork block contains our term txs
    let genesis_fork_block = genesis_node.get_block_by_height(base_height + 1).await?;
    assert!(
        genesis_fork_block.data_ledgers[DataLedger::OneYear]
            .tx_ids
            .0
            .contains(&one_year_tx.header.id),
        "expected OneYear tx in genesis fork block"
    );
    assert!(
        genesis_fork_block.data_ledgers[DataLedger::ThirtyDay]
            .tx_ids
            .0
            .contains(&thirty_day_tx.header.id),
        "expected ThirtyDay tx in genesis fork block"
    );

    // Peer mines 2 more blocks to build a longer chain that will trigger a reorg
    peer_node.mine_blocks_without_gossip(2).await?;
    peer_node
        .wait_until_height(base_height + 3, seconds_to_wait)
        .await?;

    let peer_block_1 = Arc::new(peer_node.get_block_by_height(base_height + 1).await?);
    let peer_block_2 = Arc::new(peer_node.get_block_by_height(base_height + 2).await?);
    let peer_block_3 = Arc::new(peer_node.get_block_by_height(base_height + 3).await?);

    // Prepare to detect the reorg, then re-enable gossip and send blocks
    let reorg_future = genesis_node.wait_for_reorg(seconds_to_wait);

    genesis_node.gossip_enable();
    peer_node.gossip_enable();

    peer_node.gossip_block_to_peers(&peer_block_1)?;
    genesis_node
        .wait_for_block(&peer_block_1.block_hash, seconds_to_wait)
        .await?;
    peer_node.gossip_block_to_peers(&peer_block_2)?;
    genesis_node
        .wait_for_block(&peer_block_2.block_hash, seconds_to_wait)
        .await?;
    peer_node.gossip_block_to_peers(&peer_block_3)?;
    genesis_node
        .wait_for_block(&peer_block_3.block_hash, seconds_to_wait)
        .await?;

    let _reorg = reorg_future.await?;
    genesis_node
        .wait_until_height(base_height + 3, seconds_to_wait)
        .await?;

    // Verify orphaned term txs are back in the mempool
    let canonical_tip = genesis_node
        .get_canonical_chain()
        .last()
        .expect("canonical chain should have a tip")
        .block_hash();
    let mempool_txs = genesis_node.get_best_mempool_tx(canonical_tip).await?;

    assert!(
        mempool_txs
            .one_year_tx
            .iter()
            .any(|tx| tx.id == one_year_tx.header.id),
        "expected OneYear orphan tx to be reinjected into mempool"
    );
    assert!(
        mempool_txs
            .thirty_day_tx
            .iter()
            .any(|tx| tx.id == thirty_day_tx.header.id),
        "expected ThirtyDay orphan tx to be reinjected into mempool"
    );

    // Mine another block and verify the reinjected txs get included
    let inclusion_block = genesis_node.mine_block().await?;
    let inclusion_header = genesis_node
        .get_block_by_height(inclusion_block.height)
        .await?;
    assert!(
        inclusion_header.data_ledgers[DataLedger::OneYear]
            .tx_ids
            .0
            .contains(&one_year_tx.header.id),
        "expected OneYear tx to be includable after reorg"
    );
    assert!(
        inclusion_header.data_ledgers[DataLedger::ThirtyDay]
            .tx_ids
            .0
            .contains(&thirty_day_tx.header.id),
        "expected ThirtyDay tx to be includable after reorg"
    );

    tokio::join!(genesis_node.stop(), peer_node.stop());

    Ok(())
}
