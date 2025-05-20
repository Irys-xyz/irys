use crate::utils::{get_chunk, post_chunk, post_storage_tx, IrysNodeTest};
use assert_matches::assert_matches;
use irys_testing_utils::initialize_tracing;
use irys_types::{DataLedger, LedgerChunkOffset, NodeConfig};

#[actix::test]
async fn heavy_pending_chunks_test() -> eyre::Result<()> {
    // Turn on tracing even before the nodes start
    // std::env::set_var("RUST_LOG", "debug");
    initialize_tracing();

    // Configure a test network
    let mut genesis_config = NodeConfig::testnet();
    genesis_config.consensus.get_mut().chunk_size = 32;

    // Create a signer (keypair) for transactions and fund it
    let signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&signer]);

    // Start the genesis node
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start()
        .await;
    let app = genesis_node.start_public_api().await;

    let chunks = vec![[10; 32], [20; 32], [30; 32]];
    let mut data: Vec<u8> = Vec::new();
    for chunk in chunks.iter() {
        data.extend_from_slice(chunk);
    }

    let tx = signer.create_transaction(data, None)?;
    let tx = signer.sign_transaction(tx)?;

    // First post the chunks
    post_chunk(&app, &tx, 0, &chunks).await;
    post_chunk(&app, &tx, 1, &chunks).await;
    post_chunk(&app, &tx, 2, &chunks).await;

    // Then post the tx
    post_storage_tx(&app, &tx).await;

    // Mine some blocks to trigger chunk migration
    genesis_node.mine_blocks(2).await?;

    // Finally verify the chunks didn't get dropped
    let c1 = get_chunk(&app, DataLedger::Submit, LedgerChunkOffset::from(0)).await;
    let c2 = get_chunk(&app, DataLedger::Submit, LedgerChunkOffset::from(1)).await;
    let c3 = get_chunk(&app, DataLedger::Submit, LedgerChunkOffset::from(2)).await;
    assert_matches!(c1, Some(_));
    assert_matches!(c2, Some(_));
    assert_matches!(c3, Some(_));

    genesis_node.stop().await;

    Ok(())
}
