use crate::utils::*;
use assert_matches::assert_matches;
use eyre::eyre;
use irys_domain::PackingParams;
use irys_domain::{EpochSnapshot, PACKING_PARAMS_FILE_NAME};
use irys_testing_utils::initialize_tracing;
use irys_types::DataLedger;
use irys_types::{irys::IrysSigner, IrysAddress, NodeConfig, H256};
use std::fs;
use std::sync::Arc;
use tracing::{debug, info};

macro_rules! assert_ok {
    ($result:expr) => {
        match $result {
            Ok(val) => val,
            Err(e) => panic!("Assertion failed: {:?}", e),
        }
    };
}

/// Tests multi-node epoch replay functionality by simulating a peer going offline
/// and coming back online to replay missed epoch changes.
///
/// Test Steps:
/// 1. Set up genesis node with two partitions per slot
/// 2. Set up peer node, create stake and pledge commitments
/// 3. Mine first epoch so peer gets gets its partition assigned to capacity
/// 4. Shut down the peer node
/// 5. Mine second epoch on the genesis node to assign peer partition to a data ledger slot
/// 6. Delete peer's DB and block index to force sync from genesis
/// 7. Restart peer and verify it replays the missed slot assignment correctly
///
/// Verifies that:
/// - Commitments persist across restarts
/// - Partition assignments are correctly replayed
/// - Peer catches up to the correct data ledger assignment state
#[tokio::test]
async fn heavy_test_multi_node_epoch_replay() -> eyre::Result<()> {
    // Configure minimal logging
    std::env::set_var("RUST_LOG", "debug,irys_database=off,irys_actors::data_sync_service=off,irys_p2p::gossip_service=off,irys_actors::storage_module_service=debug,trie=off,irys_reth::evm=off,engine::root=off,irys_p2p::peer_list=off,storage::db::mdbx=off,reth_basic_payload_builder=off,irys_gossip_service=off,providers::db=off,reth_payload_builder::service=off,irys_actors::mining_bus=off,reth_ethereum_payload_builder=off,provider::static_file=off,engine::persistence=off,provider::storage_writer=off,reth_engine_tree::persistence=off,irys_actors::cache_service=off,irys_vdf=off,irys_actors::block_tree_service=off,irys_actors::vdf_service=off,rys_gossip_service::service=off,eth_ethereum_payload_builder=off,reth_node_events::node=off,reth::cli=off,reth_engine_tree::tree=off,irys_actors::ema_service=off,irys_efficient_sampling=off,hyper_util::client::legacy::connect::http=off,hyper_util::client::legacy::pool=off,irys_database::migration::v0_to_v1=off,irys_storage::storage_module=off,actix_server::worker=off,irys::packing::update=off,engine::tree=off,irys_actors::mining=error,payload_builder=off,irys_actors::reth_service=off,irys_actors::packing=off,irys_actors::reth_service=off,irys::packing::progress=off,irys_chain::vdf=off,irys_vdf::vdf_state=off");
    initialize_tracing();

    // Test configuration
    let seconds_to_wait = 10;
    let signer1 = IrysSigner::random_signer(&NodeConfig::testing().consensus_config());
    let signer1_address = signer1.address();

    let mut config = NodeConfig::testing()
        .with_consensus(|consensus| {
            consensus.chunk_size = 32;
            consensus.num_partitions_per_slot = 2;
            consensus.epoch.num_blocks_in_epoch = 2;
            consensus.block_migration_depth = 1;
            consensus.num_chunks_in_partition = 20;
            consensus.num_chunks_in_recall_range = 5;
        })
        .with_genesis_peer_discovery_timeout(1000);

    config.fund_genesis_accounts(vec![&signer1]);
    let genesis_signer = config.miner_address();

    // 1. Set up genesis node with two partitions per slot
    let genesis_node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // 2. Set up peer node and stake/pledge
    let peer_sm_infos_before;
    let peer_node = {
        let peer1_config = genesis_node.testing_peer_with_signer(&signer1);
        let peer1_node = IrysNodeTest::new(peer1_config.clone())
            .start_with_name("PEER1")
            .await;

        // Create commitments for signer1
        let stake_tx1 = genesis_node
            .post_stake_commitment_with_signer(&signer1)
            .await?;
        let pledge1 = genesis_node
            .post_pledge_commitment_with_signer(&signer1)
            .await;

        // 3. Mine an epoch so the peer is assigned capacity
        info!("Mining first epoch to assign peer capacity...");
        let (_, epoch_height) = genesis_node.mine_until_next_epoch().await?;

        // Wait for peer to sync the epoch blocks via gossip
        peer1_node
            .wait_for_block_at_height(epoch_height, seconds_to_wait)
            .await?;

        // Verify commitments in blocks
        let block_1 = genesis_node.get_block_by_height(1).await.unwrap();
        let block_2 = genesis_node.get_block_by_height(2).await.unwrap();

        let expected_commitment_ids = [stake_tx1.id(), pledge1.id()];
        assert_eq!(block_1.commitment_tx_ids().len(), 2);
        assert_eq!(block_2.commitment_tx_ids().len(), 2);
        assert!(expected_commitment_ids
            .iter()
            .all(|id| block_1.commitment_tx_ids().contains(id)));

        // Verify pledge assignments
        let epoch_snapshot = genesis_node
            .node_ctx
            .block_tree_guard
            .read()
            .canonical_epoch_snapshot();
        let commitment_state = &epoch_snapshot.commitment_state;

        assert_eq!(
            commitment_state
                .pledge_commitments
                .get(&genesis_signer)
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            commitment_state
                .pledge_commitments
                .get(&signer1_address)
                .unwrap()
                .len(),
            1
        );
        assert_matches!(
            commitment_state.stake_commitments.get(&signer1_address),
            Some(_)
        );

        // Store partition assignments before peer shutdown
        peer_sm_infos_before = peer1_node
            .node_ctx
            .block_tree_guard
            .read()
            .canonical_epoch_snapshot()
            .map_storage_modules_to_partition_assignments();

        assert!(peer_sm_infos_before[0].partition_assignment.is_some());
        assert_eq!(
            peer_sm_infos_before[0]
                .partition_assignment
                .unwrap()
                .partition_hash,
            H256::from_base58("xJjza43xjkd7G6vhb4R14dHL1CB4EM5SytHSyUUJSdw")
        );
        assert!(peer_sm_infos_before[0]
            .partition_assignment
            .unwrap()
            .ledger_id
            .is_none());
        assert!(peer_sm_infos_before[0]
            .partition_assignment
            .unwrap()
            .slot_index
            .is_none());

        peer1_node
    };

    // 4. Shut down the peer
    let stopped_peer = peer_node.stop().await;

    let submodule_path = &peer_sm_infos_before[0].submodules[0].1;
    let params_path = stopped_peer
        .cfg
        .storage_module_dir()
        .join(submodule_path.file_name().expect("submodule dir name"))
        .join(PACKING_PARAMS_FILE_NAME);
    let mut params = PackingParams::from_toml(&params_path).expect("packing params to load");
    params.last_updated_height = None;
    params.write_to_disk(&params_path);

    // 5. Mine another epoch to get peer partition assigned to data ledger slot
    info!("Mining second epoch to assign peer to data ledger slot...");
    let (_, final_height) = genesis_node.mine_until_next_epoch().await?;

    // 6. Delete DB and block index to force sync from genesis
    fs::remove_dir_all(stopped_peer.cfg.irys_consensus_data_dir())?;
    fs::remove_dir_all(stopped_peer.cfg.block_index_dir())?;

    // 7. Restart peer to sync and replay unseen slot assignment
    info!("Restarting peer to replay epoch changes...");
    let restarted_node = stopped_peer.start().await;
    restarted_node.wait_for_packing(10).await;
    // Ensure the restarted peer has fully synced to the current chain height
    let target_height = genesis_node.get_canonical_chain_height().await;
    restarted_node
        .wait_until_height(target_height, seconds_to_wait)
        .await?;

    // Ensure the peer has synced to the epoch that performs the publish-slot assignment.
    restarted_node
        .wait_until_height(final_height, 30)
        .await
        .expect("peer should sync to second epoch before checking assignments");

    // Verify commitments persisted after restart
    let epoch_snapshot = restarted_node
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot();
    let commitment_state = &epoch_snapshot.commitment_state;

    assert_eq!(
        commitment_state
            .pledge_commitments
            .get(&genesis_signer)
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        commitment_state
            .pledge_commitments
            .get(&signer1_address)
            .unwrap()
            .len(),
        1
    );

    // Verify partition assignments match pre-shutdown state
    let genesis_parts_before = assert_ok!(validate_pledge_assignments(
        epoch_snapshot.clone(),
        "genesis",
        &genesis_signer
    ));
    let signer1_parts_before = assert_ok!(validate_pledge_assignments(
        epoch_snapshot.clone(),
        "signer1",
        &signer1_address
    ));
    let genesis_parts_after = assert_ok!(validate_pledge_assignments(
        epoch_snapshot.clone(),
        "genesis",
        &genesis_signer
    ));
    let signer1_parts_after = assert_ok!(validate_pledge_assignments(
        epoch_snapshot.clone(),
        "signer1",
        &signer1_address
    ));

    assert_eq!(genesis_parts_after, genesis_parts_before);
    assert_eq!(signer1_parts_after, signer1_parts_before);

    // Verify peer has correct data ledger assignment from offline period
    let sm_infos_after = restarted_node
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot()
        .map_storage_modules_to_partition_assignments();

    let peer_assignment_before = peer_sm_infos_before[0].partition_assignment.unwrap();
    let peer_assignment_after = sm_infos_after
        .iter()
        .filter_map(|info| info.partition_assignment)
        .find(|pa| pa.partition_hash == peer_assignment_before.partition_hash)
        .expect("expected partition assignment for peer after replay");

    assert_eq!(
        peer_assignment_after.partition_hash,
        peer_assignment_before.partition_hash
    );
    assert_eq!(
        peer_assignment_after.ledger_id,
        Some(DataLedger::Publish as u32)
    );
    assert_eq!(peer_assignment_after.slot_index, Some(0));

    // Cleanup
    restarted_node.stop().await;
    genesis_node.stop().await;
    Ok(())
}

/// Validates pledge assignments match between commitment state and partition assignments
fn validate_pledge_assignments(
    epoch_snapshot: Arc<EpochSnapshot>,
    address_name: &str,
    address: &IrysAddress,
) -> eyre::Result<Vec<H256>> {
    let pledges = epoch_snapshot
        .commitment_state
        .pledge_commitments
        .get(address)
        .ok_or_else(|| eyre!("Expected to find commitment entries for {}", address_name))?;

    let partition_hashes: Vec<H256> = pledges
        .iter()
        .filter_map(|pledge| pledge.partition_hash)
        .collect();

    debug!(
        "Validating {} partition assignments for {}: {:?}",
        partition_hashes.len(),
        address_name,
        partition_hashes
    );

    // Verify each partition hash has corresponding assignment
    for &partition_hash in &partition_hashes {
        let assignment = epoch_snapshot
            .get_data_partition_assignment(partition_hash)
            .ok_or_else(|| eyre!("Expected partition assignment for hash"))?;

        if assignment.miner_address != *address {
            return Err(eyre!("Partition assignment miner mismatch"));
        }
    }

    Ok(partition_hashes)
}
