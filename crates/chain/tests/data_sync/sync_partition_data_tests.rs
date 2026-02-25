use std::time::Duration;

// use assert_matches::assert_matches;
use irys_chain::IrysNodeCtx;
use irys_domain::{ChunkType, EpochSnapshot};
use irys_testing_utils::initialize_tracing;
use irys_types::{irys::IrysSigner, DataLedger, IrysAddress, NodeConfig};
use tracing::{debug, info};
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
#[tokio::test]
async fn slow_heavy3_sync_partition_data_between_peers_test() -> eyre::Result<()> {
    std::env::set_var("RUST_LOG", "info");
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
            consensus.hardforks.frontier.number_of_ingress_proofs_total = 1;
            consensus.block_migration_depth = 1;
            consensus.epoch.submit_ledger_epoch_length = 1000;
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
        .post_publish_data_tx(&genesis_signer, data)
        .await?;

    stake_and_pledge_signer(&genesis_node, &signer1, 3).await?;
    stake_and_pledge_signer(&genesis_node, &signer2, 3).await?;
    tracing::info!("staged and pledged");

    // 4. Mine epoch and validate capacity assignments
    genesis_node.mine_until_next_epoch().await?;
    let epoch_snapshot = genesis_node.get_canonical_epoch_snapshot();

    validate_partition_assignments(&epoch_snapshot, genesis_signer.address(), 1, 2, 0);
    validate_partition_assignments(&epoch_snapshot, signer1.address(), 0, 0, 3);
    validate_partition_assignments(&epoch_snapshot, signer2.address(), 0, 0, 3);

    // Mine another epoch for final ledger assignments
    tracing::info!("next epoch");
    genesis_node.mine_until_next_epoch().await?;
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

    tracing::info!("waiting for ingress proofs");
    genesis_node
        .wait_for_ingress_proofs(vec![data_tx.header.id], seconds_to_wait)
        .await?;
    tracing::info!("got ingress proofs");

    //6. Start both the peer nodes and wait for them to sync the data
    let peer1_config = genesis_node.testing_peer_with_signer(&signer1);
    let peer2_config = genesis_node.testing_peer_with_signer(&signer2);

    // Start the peers: wait for them to pack so they can start syncing data
    tracing::info!("start peer 1");
    let peer1_node = IrysNodeTest::new(peer1_config.clone())
        .start_and_wait_for_packing("PEER1", seconds_to_wait)
        .await;
    tracing::info!("start peer 2");

    let peer2_node = IrysNodeTest::new(peer2_config.clone())
        .start_and_wait_for_packing("PEER2", seconds_to_wait)
        .await;

    // Mine a block and wait for all nodes to sync to the same height
    tracing::info!("mining block");
    let latest_block = genesis_node.mine_block().await?;

    // Wait for all nodes to reach the same block height before checking data
    tracing::info!("waiting for peers to sync");
    genesis_node
        .wait_until_height(latest_block.height, seconds_to_wait)
        .await?;
    peer1_node
        .wait_until_height(latest_block.height, seconds_to_wait)
        .await?;
    peer2_node
        .wait_until_height(latest_block.height, seconds_to_wait)
        .await?;

    let expected_tip_hash = latest_block.block_hash();
    let genesis_tip_hash = genesis_node
        .get_block_by_height(latest_block.height)
        .await?
        .block_hash();
    let peer1_tip_hash = peer1_node
        .get_block_by_height(latest_block.height)
        .await?
        .block_hash();
    let peer2_tip_hash = peer2_node
        .get_block_by_height(latest_block.height)
        .await?
        .block_hash();

    assert_eq!(
        genesis_tip_hash, expected_tip_hash,
        "Genesis canonical hash at height {} diverged from mined block hash",
        latest_block.height
    );
    assert_eq!(
        peer1_tip_hash, expected_tip_hash,
        "Peer1 canonical hash at height {} diverged from mined block hash",
        latest_block.height
    );
    assert_eq!(
        peer2_tip_hash, expected_tip_hash,
        "Peer2 canonical hash at height {} diverged from mined block hash",
        latest_block.height
    );

    // Check data sync completion with simple polling
    let (mut genesis_synced, mut peer1_synced, mut peer2_synced) = (false, false, false);
    let mut last_genesis_diag = String::new();
    let mut last_peer1_diag = String::new();
    let mut last_peer2_diag = String::new();
    let mut last_genesis_state = String::new();
    let mut last_peer1_state = String::new();
    let mut last_peer2_state = String::new();

    // Slot(0) can be either mixed (data + packed) or fully packed.
    // Slot(1) is expected to be fully packed in this test scenario.
    let slot0_expectation = SlotSyncExpectation {
        expected_total: 60,
        max_data: 50,
        min_packed: 10,
    };
    let slot1_expectation = SlotSyncExpectation {
        expected_total: 60,
        max_data: 0,
        min_packed: 60,
    };

    tracing::info!("waiting for data to sync");
    for attempt in 0..80 {
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Check genesis node
        let counts1 = check_storage_module_chunks(&genesis_node, "GENESIS", DataLedger::Publish, 0);
        let counts2 = check_storage_module_chunks(&genesis_node, "GENESIS", DataLedger::Submit, 0);
        let counts3 = check_storage_module_chunks(&genesis_node, "GENESIS", DataLedger::Submit, 1);

        let (g_publish_ok, g_publish_diag) = slot_matches_expectation(&counts1, &slot0_expectation);
        let (g_submit0_ok, g_submit0_diag) = slot_matches_expectation(&counts2, &slot0_expectation);
        let (g_submit1_ok, g_submit1_diag) = slot_matches_expectation(&counts3, &slot1_expectation);
        genesis_synced = g_publish_ok && g_submit0_ok && g_submit1_ok;
        last_genesis_diag = format!(
            "Publish(0): {} | Submit(0): {} | Submit(1): {}",
            g_publish_diag, g_submit0_diag, g_submit1_diag
        );

        // Check peer1
        let peer1_counts1 =
            check_storage_module_chunks(&peer1_node, "PEER1", DataLedger::Publish, 0);
        let peer1_counts2 =
            check_storage_module_chunks(&peer1_node, "PEER1", DataLedger::Submit, 0);
        let peer1_counts3 =
            check_storage_module_chunks(&peer1_node, "PEER1", DataLedger::Submit, 1);

        let (p1_publish_ok, p1_publish_diag) =
            slot_matches_expectation(&peer1_counts1, &slot0_expectation);
        let (p1_submit0_ok, p1_submit0_diag) =
            slot_matches_expectation(&peer1_counts2, &slot0_expectation);
        let (p1_submit1_ok, p1_submit1_diag) =
            slot_matches_expectation(&peer1_counts3, &slot1_expectation);
        peer1_synced = p1_publish_ok && p1_submit0_ok && p1_submit1_ok;
        last_peer1_diag = format!(
            "Publish(0): {} | Submit(0): {} | Submit(1): {}",
            p1_publish_diag, p1_submit0_diag, p1_submit1_diag
        );

        // Check peer2
        let peer2_counts1 =
            check_storage_module_chunks(&peer2_node, "PEER2", DataLedger::Publish, 0);
        let peer2_counts2 =
            check_storage_module_chunks(&peer2_node, "PEER2", DataLedger::Submit, 0);
        let peer2_counts3 =
            check_storage_module_chunks(&peer2_node, "PEER2", DataLedger::Submit, 1);

        let (p2_publish_ok, p2_publish_diag) =
            slot_matches_expectation(&peer2_counts1, &slot0_expectation);
        let (p2_submit0_ok, p2_submit0_diag) =
            slot_matches_expectation(&peer2_counts2, &slot0_expectation);
        let (p2_submit1_ok, p2_submit1_diag) =
            slot_matches_expectation(&peer2_counts3, &slot1_expectation);
        peer2_synced = p2_publish_ok && p2_submit0_ok && p2_submit1_ok;
        last_peer2_diag = format!(
            "Publish(0): {} | Submit(0): {} | Submit(1): {}",
            p2_publish_diag, p2_submit0_diag, p2_submit1_diag
        );

        if attempt % 5 == 0 || attempt == 79 || (genesis_synced && peer1_synced && peer2_synced) {
            last_genesis_state = genesis_node.sync_state_snapshot().await;
            last_peer1_state = peer1_node.sync_state_snapshot().await;
            last_peer2_state = peer2_node.sync_state_snapshot().await;
        }

        // Log sync status
        info!(
            "Sync status at attempt {}: Genesis={}, Peer1={}, Peer2={}",
            attempt, genesis_synced, peer1_synced, peer2_synced
        );
        info!(
            "Genesis chunks - Publish(0): data={}, packed={} | Submit(0): data={}, packed={} | Submit(1): data={}, packed={}",
            counts1.data, counts1.packed, counts2.data, counts2.packed, counts3.data, counts3.packed
        );
        info!(
            "Peer1 chunks - Publish(0): data={}, packed={} | Submit(0): data={}, packed={} | Submit(1): data={}, packed={}",
            peer1_counts1.data, peer1_counts1.packed, peer1_counts2.data, peer1_counts2.packed, peer1_counts3.data, peer1_counts3.packed
        );
        info!(
            "Peer2 chunks - Publish(0): data={}, packed={} | Submit(0): data={}, packed={} | Submit(1): data={}, packed={}",
            peer2_counts1.data, peer2_counts1.packed, peer2_counts2.data, peer2_counts2.packed, peer2_counts3.data, peer2_counts3.packed
        );
        info!("Genesis diagnostic: {}", last_genesis_diag);
        info!("Peer1 diagnostic: {}", last_peer1_diag);
        info!("Peer2 diagnostic: {}", last_peer2_diag);
        if attempt % 5 == 0 || attempt == 79 || (genesis_synced && peer1_synced && peer2_synced) {
            info!("Genesis state: {}", last_genesis_state);
            info!("Peer1 state: {}", last_peer1_state);
            info!("Peer2 state: {}", last_peer2_state);
        }

        if genesis_synced && peer1_synced && peer2_synced {
            debug!("All nodes synced successfully at attempt {}", attempt);
            break;
        }
    }

    assert!(
        genesis_synced,
        "Genesis node failed to sync data. chunks={} state={}",
        last_genesis_diag, last_genesis_state
    );
    assert!(
        peer1_synced,
        "Peer1 node failed to sync data. chunks={} state={}",
        last_peer1_diag, last_peer1_state
    );
    assert!(
        peer2_synced,
        "Peer2 node failed to sync data. chunks={} state={}",
        last_peer2_diag, last_peer2_state
    );

    // Make sure peers can mine - mine one block at a time with sync verification
    for i in 0..5 {
        let current_height = peer1_node.get_canonical_chain_height().await;
        peer1_node.mine_block().await?;

        // Wait for all nodes to sync
        let target_height = current_height + 1;
        peer1_node
            .wait_until_height(target_height, seconds_to_wait)
            .await?;
        peer2_node
            .wait_until_height(target_height, seconds_to_wait)
            .await?;
        genesis_node
            .wait_until_height(target_height, seconds_to_wait)
            .await?;
        debug!("Peer1 mined block {} at height {}", i + 1, target_height);
    }

    for i in 0..5 {
        let current_height = peer2_node.get_canonical_chain_height().await;
        peer2_node.mine_block().await?;

        // Wait for all nodes to sync
        let target_height = current_height + 1;
        peer2_node
            .wait_until_height(target_height, seconds_to_wait)
            .await?;
        peer1_node
            .wait_until_height(target_height, seconds_to_wait)
            .await?;
        genesis_node
            .wait_until_height(target_height, seconds_to_wait)
            .await?;
        debug!("Peer2 mined block {} at height {}", i + 1, target_height);
    }

    peer1_node.stop().await;
    peer2_node.stop().await;
    genesis_node.stop().await;
    Ok(())
}

struct ChunkCountTotals {
    pub data: usize,
    pub packed: usize,
}

struct SlotSyncExpectation {
    expected_total: usize,
    max_data: usize,
    min_packed: usize,
}

fn slot_matches_expectation(
    counts: &ChunkCountTotals,
    expectation: &SlotSyncExpectation,
) -> (bool, String) {
    let total = counts.data + counts.packed;
    let is_match = total == expectation.expected_total
        && counts.data <= expectation.max_data
        && counts.packed >= expectation.min_packed;

    let state = if counts.data == 0 && counts.packed == expectation.expected_total {
        "fully_packed"
    } else if counts.data > 0 && counts.packed > 0 {
        "mixed_data_and_packed"
    } else if counts.data == expectation.expected_total {
        "data_only"
    } else {
        "other"
    };

    (
        is_match,
        format!(
            "state={} data={} packed={} total={} expected_total={} max_data={} min_packed={}",
            state,
            counts.data,
            counts.packed,
            total,
            expectation.expected_total,
            expectation.max_data,
            expectation.min_packed
        ),
    )
}

fn check_storage_module_chunks(
    node: &IrysNodeTest<IrysNodeCtx>,
    name: &str,
    ledger: DataLedger,
    slot_index: usize,
) -> ChunkCountTotals {
    let data_intervals = node.get_storage_module_intervals(ledger, slot_index, ChunkType::Data);
    let packed_intervals =
        node.get_storage_module_intervals(ledger, slot_index, ChunkType::Entropy);

    // Extract the offsets
    let mut data_chunks = Vec::new();
    for int in data_intervals {
        let start: u32 = int.start().into();
        let end: u32 = int.end().into();
        for offset in start..=end {
            data_chunks.push(offset);
        }
    }

    let mut packed_chunks = Vec::new();
    for int in packed_intervals {
        let start: u32 = int.start().into();
        let end: u32 = int.end().into();
        for offset in start..=end {
            packed_chunks.push(offset);
        }
    }

    debug!(
        "\n{}: {:?}:{}\n data offsets: {:?}\n pack offsets: {:?}\n",
        name, ledger, slot_index, data_chunks, packed_chunks
    );

    ChunkCountTotals {
        data: data_chunks.len(),
        packed: packed_chunks.len(),
    }
}

fn validate_partition_assignments(
    epoch_snapshot: &EpochSnapshot,
    signer_address: IrysAddress,
    expected_publish: usize,
    expected_submit: usize,
    expected_capacity: usize,
) {
    let partition_assignments = epoch_snapshot.get_partition_assignments(signer_address);

    let publish_count = partition_assignments
        .iter()
        .filter(|pa| pa.ledger_id == Some(DataLedger::Publish.into()))
        .count();
    let submit_count = partition_assignments
        .iter()
        .filter(|pa| pa.ledger_id == Some(DataLedger::Submit.into()))
        .count();
    let capacity_count = partition_assignments
        .iter()
        .filter(|pa| pa.ledger_id.is_none())
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
