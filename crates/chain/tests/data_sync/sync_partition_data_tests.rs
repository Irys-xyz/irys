// use assert_matches::assert_matches;
use irys_chain::IrysNodeCtx;
use irys_domain::EpochSnapshot;
use irys_testing_utils::initialize_tracing;
use irys_types::{irys::IrysSigner, Address, NodeConfig};
// use tracing::debug;

use crate::utils::IrysNodeTest;

// 1. Configure the network to use 3 partition replicas per slot and one ingress proof to promote
// 2. Start a genesis node, verify it is assigned one partition per slot
// 3. Post a data tx header that fills 90% of the first submit slot
// 4. Stake and pledge two more nodes so their assignments happen at the next epoch
// 5. Mine a epoch block so that the submit ledger grows
// 6. Verify all the genesis node partition hashes are assigned
// 7. Start two more nodes
// 8. Verify that all slots now have 3 replicas
// 9. Upload the chunks of the data_tx to the genesis node and wait for promotion
// 10. Start the two peers and let them sync with the network, but not mine
// 11. Validate that they are syncing data chunks to their assigned partitions
#[actix_web::test]
async fn slow_heavy_sync_partition_data_between_peers_test() -> eyre::Result<()> {
    std::env::set_var(
        "RUST_LOG",
        "debug,engine=off,irys_vdf::vdf=off,storage::db::mdbx=off",
    );
    initialize_tracing();

    let seconds_to_wait = 20;
    let chunk_size: usize = 32;

    // 1. Configure network
    let mut config = NodeConfig::testing()
        .with_consensus(|consensus| {
            consensus.chunk_size = chunk_size as u64;
            consensus.num_partitions_per_slot = 3;
            consensus.num_chunks_in_partition = 60;
            consensus.epoch.num_blocks_in_epoch = 4;
            consensus.number_of_ingress_proofs_total = 1;
            consensus.block_migration_depth = 1;
        })
        .with_genesis_peer_discovery_timeout(1000);

    // 2. Setup signers and genesis node
    let signer1 = IrysSigner::random_signer(&config.consensus_config());
    let signer2 = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer1, &signer2]);

    let genesis_node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    let genesis_signer = genesis_node.node_ctx.config.irys_signer();

    genesis_node.stop_mining().await;

    // Validate initial assignments
    let epoch_snapshot = genesis_node.get_canonical_epoch_snapshot();
    validate_partition_assignments(&epoch_snapshot, genesis_signer.address(), 1, 1, 1);

    // 3. Post data tx and setup signers with pledges
    let num_chunks = 50;
    let mut chunks = Vec::with_capacity(num_chunks);
    for i in 0..num_chunks {
        chunks.push([i as u8; 32]);
    }
    let data: Vec<u8> = chunks.concat();

    let data_tx = genesis_node
        .create_publish_data_tx(&genesis_signer, data)
        .await?;
    genesis_node.post_data_tx_raw(&data_tx.header).await;

    stake_and_pledge_signer(&genesis_node, &signer1, 3).await?;
    stake_and_pledge_signer(&genesis_node, &signer2, 3).await?;

    // 4. Mine epoch and validate capacity assignments
    genesis_node.mine_blocks(4).await?;
    let epoch_snapshot = genesis_node.get_canonical_epoch_snapshot();

    validate_partition_assignments(&epoch_snapshot, genesis_signer.address(), 1, 2, 0);
    validate_partition_assignments(&epoch_snapshot, signer1.address(), 0, 0, 3);
    validate_partition_assignments(&epoch_snapshot, signer2.address(), 0, 0, 3);

    // Mine another epoch for final ledger assignments
    genesis_node.mine_blocks(4).await?;
    let epoch_snapshot = genesis_node.get_canonical_epoch_snapshot();

    // Validate all signer partitions are assigned to data ledger slots
    validate_partition_assignments(&epoch_snapshot, signer1.address(), 1, 2, 0);
    validate_partition_assignments(&epoch_snapshot, signer2.address(), 1, 2, 0);

    //5. Upload the chunks of the data_tx to the genesis node and wait for promotion
    for chunk_index in 0..num_chunks {
        genesis_node
            .post_chunk_32b(&data_tx, chunk_index, &chunks)
            .await;
    }

    genesis_node
        .wait_for_ingress_proofs(vec![data_tx.header.id], seconds_to_wait)
        .await?;

    //6. Start both the peer nodes and wait for them to sync the data
    let peer1_config = genesis_node.testing_peer_with_signer(&signer1);
    let peer2_config = genesis_node.testing_peer_with_signer(&signer2);

    // Start the peers: wait for them to pack so they can start syncing data
    let peer1_node = IrysNodeTest::new(peer1_config.clone())
        .start_and_wait_for_packing("PEER1", seconds_to_wait)
        .await;

    let peer2_node = IrysNodeTest::new(peer2_config.clone())
        .start_and_wait_for_packing("PEER2", seconds_to_wait)
        .await;

    let _block = genesis_node.mine_block().await?;

    peer1_node.stop().await;
    peer2_node.stop().await;
    genesis_node.stop().await;
    Ok(())
}

fn validate_partition_assignments(
    epoch_snapshot: &EpochSnapshot,
    signer_address: Address,
    expected_publish: usize,
    expected_submit: usize,
    expected_capacity: usize,
) {
    let partition_assignments = epoch_snapshot.get_partition_assignments(signer_address);

    let publish_count = partition_assignments
        .iter()
        .filter(|pa| pa.ledger_id == Some(0))
        .count();
    let submit_count = partition_assignments
        .iter()
        .filter(|pa| pa.ledger_id == Some(1))
        .count();
    let capacity_count = partition_assignments
        .iter()
        .filter(|pa| pa.ledger_id == None)
        .count();

    assert_eq!(
        publish_count, expected_publish,
        "Publish assignment mismatch for {}",
        signer_address
    );
    assert_eq!(
        submit_count, expected_submit,
        "Submit assignment mismatch for {}",
        signer_address
    );
    assert_eq!(
        capacity_count, expected_capacity,
        "Capacity assignment mismatch for {}",
        signer_address
    );
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
) -> eyre::Result<()> {
    node.post_stake_commitment_with_signer(signer).await?;
    post_multiple_pledges(node, signer, pledge_count).await;
    Ok(())
}
