use crate::utils::IrysNodeTest;
use assert_matches::assert_matches;
use irys_testing_utils::initialize_tracing;
use irys_types::NodeConfig;

#[actix_web::test]
async fn slow_heavy_promotion_with_multiple_proofs_test() -> eyre::Result<()> {
    std::env::set_var(
        "RUST_LOG",
        "debug,storage::db=off,irys_domain::models::block_tree=off,actix_web=off,engine=off,trie=off,pruner=off,irys_actors::reth_service=off,provider=off,hyper=off,reqwest=off,irys_vdf=off,irys_actors::cache_service=off,irys_p2p=off,irys_actors::mining=off,irys_efficient_sampling=off,reth::cli=off,payload_builder=off",
    );
    initialize_tracing();

    let seconds_to_wait = 30;
    let mut genesis_config = NodeConfig::testing();

    // Set up consensus to require 3 ingress proofs to promote
    genesis_config
        .consensus
        .get_mut()
        .number_of_ingress_proofs_total = 3;
    genesis_config.consensus.get_mut().chunk_size = 32;

    // Create a signer (keypair) for the peer and fund it
    let peer1_signer = genesis_config.new_random_signer();
    let peer2_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer1_signer, &peer2_signer]);

    // Start the genesis node and wait for packing
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Initialize peer configs with their keypair/signer
    let peer1_config = genesis_node.testing_peer_with_signer(&peer1_signer);
    let peer2_config = genesis_node.testing_peer_with_signer(&peer2_signer);

    // Start the peers: No packing on the peers, they don't have partition assignments yet
    let peer1_node = IrysNodeTest::new(peer1_config.clone())
        .start_with_name("PEER1")
        .await;

    let peer2_node = IrysNodeTest::new(peer2_config.clone())
        .start_with_name("PEER2")
        .await;

    genesis_node.start_mining().await;

    genesis_node.wait_until_height(1, seconds_to_wait).await?;

    let peer1_stake_tx = peer1_node.post_stake_commitment(None).await?;
    let peer2_stake_tx = peer2_node.post_stake_commitment(None).await?;

    genesis_node
        .wait_for_mempool_commitment_txs(
            vec![peer1_stake_tx.id, peer2_stake_tx.id],
            seconds_to_wait,
        )
        .await?;

    // Post a transaction and it's chunks to all 3
    let chunks = vec![[10; 32], [20; 32], [30; 32]];
    let mut data: Vec<u8> = Vec::new();
    for chunk in &chunks {
        data.extend_from_slice(chunk);
    }

    let data_tx = peer1_node
        .create_signed_data_tx(&peer1_signer, data)
        .await?;

    genesis_node.post_data_tx_raw(&data_tx.header).await;

    peer1_node
        .wait_for_mempool(data_tx.header.id, seconds_to_wait)
        .await?;

    peer2_node
        .wait_for_mempool(data_tx.header.id, seconds_to_wait)
        .await?;

    genesis_node.post_chunk_32b(&data_tx, 0, &chunks).await;
    genesis_node.post_chunk_32b(&data_tx, 1, &chunks).await;
    genesis_node.post_chunk_32b(&data_tx, 2, &chunks).await;

    let res = genesis_node
        .wait_for_ingress_proofs(vec![data_tx.header.id], seconds_to_wait)
        .await;
    assert_matches!(res, Ok(()));

    let res = peer1_node
        .wait_for_ingress_proofs_no_mining(vec![data_tx.header.id], seconds_to_wait * 2)
        .await;
    assert_matches!(res, Ok(()));

    let res = genesis_node
        .wait_for_multiple_ingress_proofs_no_mining(vec![data_tx.header.id], 3, seconds_to_wait)
        .await;
    assert_matches!(res, Ok(()));

    let res = peer2_node
        .wait_for_multiple_ingress_proofs_no_mining(vec![data_tx.header.id], 3, seconds_to_wait * 2)
        .await;
    assert_matches!(res, Ok(()));

    let height = genesis_node.get_canonical_chain_height().await;
    genesis_node.start_mining().await;
    genesis_node
        .wait_until_height_confirmed(height + 1, seconds_to_wait)
        .await?;

    // Check is promoted state of tx in the mempool
    let is_promoted = genesis_node.get_is_promoted(&data_tx.header.id).await?;
    assert!(is_promoted);

    genesis_node
        .wait_until_height_confirmed(
            height + genesis_config.consensus_config().block_migration_depth as u64,
            seconds_to_wait,
        )
        .await?;

    // Check is promoted state of tx after it migrates to DB
    let is_promoted = genesis_node.get_is_promoted(&data_tx.header.id).await?;
    assert!(is_promoted);

    // Wind down test
    genesis_node.stop().await;
    peer1_node.stop().await;
    peer2_node.stop().await;
    Ok(())
}
