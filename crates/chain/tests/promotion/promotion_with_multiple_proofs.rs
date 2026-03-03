use crate::utils::IrysNodeTest;
use assert_matches::assert_matches;
use irys_chain::IrysNodeCtx;
use irys_testing_utils::initialize_tracing;
use irys_types::{irys::IrysSigner, CommitmentTransaction, NodeConfig};

#[tokio::test]
async fn heavy4_promotion_with_multiple_proofs_test() -> eyre::Result<()> {
    // SAFETY: test code; env var set before other threads spawn.
    unsafe {
        std::env::set_var(
        "RUST_LOG",
        "debug,storage::db=off,irys_domain::models::block_tree=off,actix_web=off,engine=off,trie=off,pruner=off,irys_actors::reth_service=off,provider=off,hyper=off,reqwest=off,irys_vdf=off,irys_actors::cache_service=off,irys_p2p=off,irys_actors::mining=off,irys_efficient_sampling=off,reth::cli=off,payload_builder=off",
    );
    }
    initialize_tracing();

    let seconds_to_wait = 30;

    // Set up consensus to require 3 ingress proofs to promote
    let mut config = NodeConfig::testing()
        .with_consensus(|consensus| {
            consensus.chunk_size = 32;
            // Set the total number of proofs required to promote above the number of nodes (3)
            // to validate the clamping to 3 proofs to promote.
            consensus.hardforks.frontier.number_of_ingress_proofs_total = 5;
            consensus
                .hardforks
                .frontier
                .number_of_ingress_proofs_from_assignees = 2;
            consensus.num_partitions_per_slot = 3;
            consensus.epoch.num_blocks_in_epoch = 3;
            consensus.block_migration_depth = 1;
        })
        .with_genesis_peer_discovery_timeout(1000);

    config
        .consensus
        .get_mut()
        .hardforks
        .frontier
        .number_of_ingress_proofs_total = 3;
    config.consensus.get_mut().chunk_size = 32;

    // Create a signer (keypair) for the peers and fund them
    let peer1_signer = config.new_random_signer();
    let peer2_signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&peer1_signer, &peer2_signer]);

    // Start the genesis node and wait for packing
    let genesis_node = IrysNodeTest::new_genesis(config.clone())
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

    genesis_node.mine_block().await?;

    let peer1_stake_tx = stake_and_pledge_signer(&peer1_node, &peer1_signer, 3).await?;
    let peer2_stake_tx = stake_and_pledge_signer(&peer2_node, &peer2_signer, 3).await?;

    genesis_node
        .wait_for_mempool_commitment_txs(
            vec![peer1_stake_tx.id(), peer2_stake_tx.id()],
            seconds_to_wait,
        )
        .await?;

    // Mine blocks to include the stake commitments in a confirmed block, then wait for peers to catch up.
    let height_before_commitments = genesis_node.get_canonical_chain_height().await;
    genesis_node.mine_block().await?;
    genesis_node
        .wait_until_height_confirmed(height_before_commitments + 1, seconds_to_wait)
        .await?;
    // Ensure peers see the same height so their snapshots include the new stakes
    peer1_node
        .wait_until_height(height_before_commitments + 1, seconds_to_wait)
        .await?;
    peer2_node
        .wait_until_height(height_before_commitments + 1, seconds_to_wait)
        .await?;

    genesis_node.mine_until_next_epoch().await?; // First epoch: Gets peers assigned capacity partitions
    genesis_node.mine_until_next_epoch().await?; // Second epoch: Gets peer partitions assigned to data ledger slots

    // Post a data transaction to the genesis node and wait for it to reach the other peers
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

    // Now post the 3 chunks of the data transaction to the genesis node
    genesis_node.post_chunk_32b(&data_tx, 0, &chunks).await;
    genesis_node.post_chunk_32b(&data_tx, 1, &chunks).await;
    genesis_node.post_chunk_32b(&data_tx, 2, &chunks).await;

    // Wait for ingress proof on the genesis node
    let res = genesis_node
        .wait_for_ingress_proofs(vec![data_tx.header.id], seconds_to_wait)
        .await;
    assert_matches!(res, Ok(()));

    // Wait for ingress proofs on peer1 node
    let res = peer1_node
        .wait_for_ingress_proofs_no_mining(vec![data_tx.header.id], seconds_to_wait * 2)
        .await;
    assert_matches!(res, Ok(()));

    // Wait specifically for 3 ingress proofs on the genesis node
    let res = genesis_node
        .wait_for_multiple_ingress_proofs_no_mining(vec![data_tx.header.id], 3, seconds_to_wait)
        .await;
    assert_matches!(res, Ok(()));

    let res = peer2_node
        .wait_for_multiple_ingress_proofs_no_mining(vec![data_tx.header.id], 3, seconds_to_wait * 2)
        .await;
    assert_matches!(res, Ok(()));

    let height = genesis_node.get_canonical_chain_height().await;
    genesis_node.mine_block().await?;
    genesis_node
        .wait_until_height_confirmed(height + 1, seconds_to_wait)
        .await?;

    // Check is promoted state of tx in the mempool
    let is_promoted = genesis_node.get_is_promoted(&data_tx.header.id).await?;
    assert!(is_promoted);

    genesis_node
        .wait_until_height_confirmed(
            height + config.consensus_config().block_migration_depth as u64,
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

async fn post_multiple_pledges(
    node: &IrysNodeTest<IrysNodeCtx>,
    signer: &IrysSigner,
    count: usize,
) {
    for _ in 0..count {
        node.post_pledge_commitment_with_signer(signer).await;
    }
}

async fn stake_and_pledge_signer(
    node: &IrysNodeTest<IrysNodeCtx>,
    signer: &IrysSigner,
    pledge_count: usize,
) -> eyre::Result<CommitmentTransaction> {
    let commitment_tx = node.post_stake_commitment_with_signer(signer).await?;
    post_multiple_pledges(node, signer, pledge_count).await;
    Ok(commitment_tx)
}
