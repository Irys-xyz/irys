use assert_matches::assert_matches;
use irys_testing_utils::initialize_tracing;
use irys_types::{DataLedger, NodeConfig};
use tracing::debug;

use crate::utils::IrysNodeTest;

// 1. Start a node
// 2. Post a data_tx that will cause the submit ledger to add a slot
// 3. Mine enough blocks to cross the epoch boundary
// 4. Verify the nodes capacity partition is now assigned to the submit ledger.
// 5. Mine enough epochs to expire the first slot out of the submit ledger
// 6. Verify the partition_hash is assigned to capacity and that the storage module matches
#[tokio::test]
async fn heavy_sm_reassignment_with_restart_test() -> eyre::Result<()> {
    // SAFETY: test code; env var set before other threads spawn.
    unsafe {
        std::env::set_var(
        "RUST_LOG",
        "debug,storage::db=off,irys_domain::models::block_tree=off,actix_web=off,engine=off,trie=off,pruner=off,irys_actors::reth_service=off,provider=off,hyper=off,reqwest=off,irys_vdf=off,irys_actors::cache_service=off,irys_p2p=off,irys_actors::mining=off,irys_efficient_sampling=off,reth::cli=off,payload_builder=off",
    );
    }
    initialize_tracing();

    let seconds_to_wait = 10;
    let mut config = NodeConfig::testing();

    let chunk_size: usize = 32;
    config.consensus.get_mut().chunk_size = chunk_size as u64;
    config.consensus.get_mut().num_chunks_in_partition = 10;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = 4;
    config.genesis_peer_discovery_timeout_millis = 1000; // Faster restart
    config.consensus.get_mut().block_migration_depth = 1; // <- so we lose less blocks in the restart
    config.consensus.get_mut().epoch.submit_ledger_epoch_length = 2;

    // 1. Start a node
    let genesis_node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    let genesis_signer = genesis_node.node_ctx.config.irys_signer();
    genesis_node.stop_mining();

    // Retrieve the nodes capacity partition_hash
    let epoch_snapshot = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot();
    let partition_assignments = epoch_snapshot.get_partition_assignments(genesis_signer.address());
    let capacity_pa = partition_assignments
        .iter()
        .find(|pa| pa.ledger_id.is_none())
        .unwrap();
    let submit_pa = partition_assignments
        .iter()
        .find(|pa| pa.ledger_id == Some(DataLedger::Submit.into()))
        .unwrap();
    debug!("Capacity Partition: {:#?}", capacity_pa);
    debug!("Submit Partition: {:#?}", submit_pa);

    // Mine a block
    let _block1 = genesis_node.mine_block().await?;

    // 2. Post a data_tx that will cause the submit ledger to add a slot (9 chunks worth of bytes)
    let data = vec![255_u8; 9 * chunk_size];
    let data_tx = genesis_node
        .post_publish_data_tx(&genesis_signer, data)
        .await?;
    genesis_node.post_data_tx_raw(&data_tx.header).await;

    //3. Mine enough blocks to cross the epoch boundary
    assert_matches!(genesis_node.mine_blocks(4).await, Ok(()));
    genesis_node.wait_until_height(5, seconds_to_wait).await?;

    // 4. Verify the nodes capacity partition is now assigned to the submit ledger in slot_index:1
    let data_pa = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot()
        .get_data_partition_assignment(capacity_pa.partition_hash)
        .expect("to retrieve the partition assignment");
    assert!(data_pa.ledger_id.is_some() && data_pa.slot_index == Some(1));

    // 5. Mine enough epochs to expire the first slot out of the submit ledger
    assert_matches!(genesis_node.mine_blocks(3).await, Ok(()));

    // 6. Verify the submit partition_hash is assigned to capacity and then reassigned to submit slot 2
    let reassigned_pa = genesis_node
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot()
        .get_data_partition_assignment(submit_pa.partition_hash)
        .expect("to retrieve the partition assignment");
    assert!(
        reassigned_pa.ledger_id == Some(DataLedger::Submit.into())
            && reassigned_pa.slot_index == Some(2)
    );

    // Mine on block past the epoch so the node restarts with the new assignments
    let _block9 = genesis_node.mine_block().await?;

    let stopped_node = genesis_node.stop().await;
    let restarted_node = stopped_node.start().await;

    // Wait for the restarted node to mine a block
    let _block9 = restarted_node.mine_block().await?;

    // Verify all it's partition assignments are correct
    let new_reassigned_pa = restarted_node
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot()
        .get_data_partition_assignment(reassigned_pa.partition_hash)
        .expect("to retrieve the partition assignment");

    assert_eq!(new_reassigned_pa, reassigned_pa);

    // Wind down test with graceful shutdown
    restarted_node.stop().await;

    Ok(())
}
