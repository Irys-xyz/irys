use crate::utils::*;
use assert_matches::assert_matches;
use base58::ToBase58 as _;
use eyre::eyre;
use irys_actors::{
    packing::wait_for_packing, CommitmentStateReadGuard, EpochServiceMessage,
    PartitionAssignmentsReadGuard,
};
use irys_chain::IrysNodeCtx;
use irys_database::CommitmentSnapshotStatus;
use irys_primitives::CommitmentType;
use irys_testing_utils::initialize_tracing;
use irys_types::{irys::IrysSigner, Address, CommitmentTransaction, NodeConfig, H256};
use tokio::time::Duration;
use tracing::{debug, debug_span, info};
macro_rules! assert_ok {
    ($result:expr) => {
        match $result {
            Ok(val) => val,
            Err(e) => panic!("Assertion failed: {:?}", e),
        }
    };
}

#[actix_web::test]
async fn heavy_test_commitments_3epochs_test() -> eyre::Result<()> {
    // ===== TEST ENVIRONMENT SETUP =====
    // Configure logging to reduce noise while keeping relevant commitment outputs
    std::env::set_var("RUST_LOG", "debug,irys_database=off,irys_p2p::gossip_service=off,irys_actors::storage_module_service=off,trie=off,irys_reth::evm=off,engine::root=off,irys_p2p::peer_list=off,storage::db::mdbx=off,reth_basic_payload_builder=off,irys_gossip_service=off,providers::db=off,reth_payload_builder::service=off,irys_actors::broadcast_mining_service=off,reth_ethereum_payload_builder=off,provider::static_file=off,engine::persistence=off,provider::storage_writer=off,reth_engine_tree::persistence=off,irys_actors::cache_service=off,irys_vdf=off,irys_actors::block_tree_service=off,irys_actors::vdf_service=off,rys_gossip_service::service=off,eth_ethereum_payload_builder=off,reth_node_events::node=off,reth::cli=off,reth_engine_tree::tree=off,irys_actors::ema_service=off,irys_efficient_sampling=off,hyper_util::client::legacy::connect::http=off,hyper_util::client::legacy::pool=off,irys_database::migration::v0_to_v1=off,irys_storage::storage_module=off,actix_server::worker=off,irys::packing::update=off,engine::tree=off,irys_actors::mining=error,payload_builder=off,irys_actors::reth_service=off,irys_actors::packing=off,irys_actors::reth_service=off,irys::packing::progress=off,irys_chain::vdf=off,irys_vdf::vdf_state=off");
    initialize_tracing();

    // ===== TEST PURPOSE: Multiple Epochs with Commitments =====
    // This test verifies that:
    // 1. Stake and pledge commitments are correctly processed across multiple epoch transitions
    // 2. Partition assignments are properly created and maintained for all pledges
    // 3. The commitment state correctly tracks stake and pledge relationships

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let mut config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);

    // Create multiple signers to test different commitment scenarios
    let signer1 = IrysSigner::random_signer(&config.consensus_config());
    let signer2 = IrysSigner::random_signer(&config.consensus_config());
    let signer1_address = signer1.address();
    let signer2_address = signer2.address();

    config.fund_genesis_accounts(vec![&signer1, &signer2]);

    let genesis_signer = config.miner_address();
    let genesis_parts_before;
    let signer1_parts_before;
    let signer2_parts_before;

    let node = {
        // Start a test node with custom configuration
        let node = IrysNodeTest::new_genesis(config.clone())
            .start_and_wait_for_packing("GENESIS", 10)
            .await;

        // Get access to commitment and partition services for verification
        let epoch_service = node.node_ctx.service_senders.epoch_service.clone();

        let (sender, rx) = tokio::sync::oneshot::channel();
        epoch_service.send(EpochServiceMessage::GetCommitmentStateGuard(sender))?;
        let commitment_state_guard = rx.await?;

        let (sender, rx) = tokio::sync::oneshot::channel();
        epoch_service.send(EpochServiceMessage::GetPartitionAssignmentsGuard(sender))?;
        let pa_guard = rx.await?;

        // ===== PHASE 1: Verify Genesis Block Initialization =====
        // Check that the genesis block producer has the expected initial pledges

        {
            let genesis_block = node.get_block_by_height(0).await?;

            let commitment_state = commitment_state_guard.read();
            let pledges = commitment_state.pledge_commitments.get(&genesis_signer);
            let stakes = commitment_state.stake_commitments.get(&genesis_signer);

            if let Some(pledges) = pledges {
                assert_eq!(
                    pledges.len(),
                    3,
                    "Genesis miner should have exactly 3 pledges"
                );
            } else {
                panic!("Expected genesis miner to have pledges!");
            }

            debug!(
                "\nGenesis Block Commitments:\n{:#?}\nStake: {:#?}\nPledges:\n{:#?}",
                genesis_block.get_commitment_ledger_tx_ids(),
                stakes.unwrap().id,
                pledges.unwrap().iter().map(|x| x.id).collect::<Vec<_>>(),
            );
            drop(commitment_state); // Release lock to allow node operations
        }

        // ===== PHASE 2: First Epoch - Create Commitments =====
        // Create stake commitment for first test signer
        let stake_tx1 = post_stake_commitment(&node, &signer1).await;

        // Create two pledge commitments for first test signer
        let pledge1 = post_pledge_commitment(&node, &signer1, H256::default()).await;
        let pledge2 = post_pledge_commitment(&node, &signer1, pledge1.id).await;

        // Create stake commitment for second test signer
        let stake_tx2 = post_stake_commitment(&node, &signer2).await;

        // Mine enough blocks to reach the first epoch boundary
        info!("MINE FIRST EPOCH BLOCK:");
        node.mine_blocks(num_blocks_in_epoch).await?;

        debug!(
            "Post Commitments:\nstake1: {:?}\nstake2: {:?}\npledge1: {:?}\npledge2: {:?}\n",
            stake_tx1.id, stake_tx2.id, pledge1.id, pledge2.id
        );

        // Block height: 1 should have two stake and two pledge commitments
        let expected_ids = [stake_tx1.id, stake_tx2.id, pledge1.id, pledge2.id];
        let block_1 = node.get_block_by_height(1).await.unwrap();
        let commitments_1 = block_1.get_commitment_ledger_tx_ids();
        debug!("Block - height: {:?}\n{:#?}", block_1.height, commitments_1,);
        assert_eq!(commitments_1.len(), 4);
        assert!(expected_ids.iter().all(|id| commitments_1.contains(id)));

        // Block height: 2 is an epoch block and should have the same commitments and no more
        let block_2 = node.get_block_by_height(2).await.unwrap();
        let commitments_2 = block_2.get_commitment_ledger_tx_ids();
        debug!("Block - height: {:?}\n{:#?}", block_2.height, commitments_2);
        assert_eq!(commitments_2.len(), 4);
        assert!(expected_ids.iter().all(|id| commitments_2.contains(id)));

        // ===== PHASE 3: Verify First Epoch Assignments =====
        // Verify that all pledges have been assigned partitions
        assert_ok!(validate_pledge_assignments(
            &commitment_state_guard,
            &pa_guard,
            "genesis",
            &genesis_signer,
        ));
        assert_ok!(validate_pledge_assignments(
            &commitment_state_guard,
            &pa_guard,
            "signer1",
            &signer1.address(),
        ));

        // Verify commitment state contains expected pledges and stakes
        {
            let commitment_state = commitment_state_guard.read();

            // Check genesis miner pledges
            let pledges = commitment_state
                .pledge_commitments
                .get(&genesis_signer)
                .expect("Expected genesis miner pledges!");
            assert_eq!(
                pledges.len(),
                3,
                "Genesis miner should still have 3 pledges after first epoch"
            );

            // Check signer1 pledges and stake
            let pledges = commitment_state
                .pledge_commitments
                .get(&signer1.address())
                .expect("Expected signer1 miner pledges!");
            assert_eq!(
                pledges.len(),
                2,
                "Signer1 should have 2 pledges after first epoch"
            );

            let stake = commitment_state.stake_commitments.get(&signer1.address());
            assert_matches!(stake, Some(_), "Signer1 should have a stake commitment");
            drop(commitment_state);
        }

        // ===== PHASE 4: Second Epoch - Add More Commitments =====
        // Create pledge for second test signer
        let pledge3 = post_pledge_commitment(&node, &signer2, H256::default()).await;
        info!("signer2: {} post pledge: {}", signer2_address, pledge3.id);

        // Mine enough blocks to reach the second epoch boundary
        info!("MINE SECOND EPOCH BLOCK:");
        node.mine_blocks(num_blocks_in_epoch + 2).await?;

        let block_3 = node.get_block_by_height(3).await.unwrap();
        let commitments_3 = block_3.get_commitment_ledger_tx_ids();
        debug!("Block - height: {:?}\n{:#?}", block_3.height, commitments_3);

        // Block height: 3 should have 1 pledge commitment
        assert_eq!(commitments_3.len(), 1);
        assert_eq!(commitments_3, vec![pledge3.id]);

        // ===== PHASE 5: Verify Second Epoch Assignments =====
        // Verify all signers have proper partition assignments for all pledges
        genesis_parts_before = assert_ok!(validate_pledge_assignments(
            &commitment_state_guard,
            &pa_guard,
            "genesis",
            &genesis_signer,
        ));
        signer1_parts_before = assert_ok!(validate_pledge_assignments(
            &commitment_state_guard,
            &pa_guard,
            "signer1",
            &signer1_address,
        ));
        signer2_parts_before = assert_ok!(validate_pledge_assignments(
            &commitment_state_guard,
            &pa_guard,
            "signer2",
            &signer2_address,
        ));

        node
    };

    // Restart the node
    info!("Restarting node");
    let stopped_node = node.stop().await;
    let restarted_node = stopped_node.start().await;

    // wait a few seconds for node to wake up
    restarted_node.wait_for_packing(10).await;

    // Get access to commitment and partition services for verification
    let epoch_service = restarted_node
        .node_ctx
        .service_senders
        .epoch_service
        .clone();

    let (sender, rx) = tokio::sync::oneshot::channel();
    epoch_service.send(EpochServiceMessage::GetCommitmentStateGuard(sender))?;
    let commitment_state_guard = rx.await?;

    let (sender, rx) = tokio::sync::oneshot::channel();
    epoch_service.send(EpochServiceMessage::GetPartitionAssignmentsGuard(sender))?;
    let pa_guard = rx.await?;

    // Make sure genesis has 3 commitments (1 stake, 2 pledge)
    assert_eq!(
        commitment_state_guard
            .read()
            .pledge_commitments
            .get(&genesis_signer)
            .expect("commitments for genesis miner")
            .len(),
        3
    );

    // Make sure signer1 has 2 commitments (1 stake, 1 pledge)
    assert_eq!(
        commitment_state_guard
            .read()
            .pledge_commitments
            .get(&signer1.address())
            .expect("commitments for genesis miner")
            .len(),
        2
    );

    // Make sure signer2 has 1 commitments (1 stake, 0 pledge)
    assert_eq!(
        commitment_state_guard
            .read()
            .pledge_commitments
            .get(&signer2.address())
            .expect("commitments for genesis miner")
            .len(),
        1
    );

    // Verify the partition assignments persist (and the map to the same pledges)
    let genesis_parts_after = assert_ok!(validate_pledge_assignments(
        &commitment_state_guard,
        &pa_guard,
        "genesis",
        &genesis_signer,
    ));
    let signer1_parts_after = assert_ok!(validate_pledge_assignments(
        &commitment_state_guard,
        &pa_guard,
        "signer1",
        &signer1.address(),
    ));
    let signer2_parts_after = assert_ok!(validate_pledge_assignments(
        &commitment_state_guard,
        &pa_guard,
        "signer2",
        &signer2.address(),
    ));

    assert_eq!(genesis_parts_after, genesis_parts_before);
    assert_eq!(signer1_parts_after, signer1_parts_before);
    assert_eq!(signer2_parts_after, signer2_parts_before);

    // ===== TEST CLEANUP =====
    restarted_node.node_ctx.stop().await;
    Ok(())
}

#[actix_web::test]
async fn heavy_no_commitments_basic_test() -> eyre::Result<()> {
    // std::env::set_var("RUST_LOG", "debug");
    std::env::set_var(
        "RUST_LOG",
        "irys_actors::epoch_service::epoch_service=debug",
    );
    initialize_tracing();

    let span = debug_span!("TEST");
    let _enter = span.enter();
    debug!("span test");

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let seconds_to_wait = 20;
    let mut genesis_config = NodeConfig::testnet_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    // Start the genesis node and wait for packing
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Initialize the peer with our keypair/signer
    let peer_config = genesis_node.testnet_peer();

    // Start the peer: No packing on the peer, it doesn't have partition assignments yet
    let peer_node = IrysNodeTest::new(peer_config.clone())
        .start_with_name("PEER")
        .await;

    genesis_node.mine_blocks(num_blocks_in_epoch).await?;

    let _block_hash = peer_node
        .wait_until_height(num_blocks_in_epoch as u64, seconds_to_wait)
        .await?;
    let block = peer_node
        .get_block_by_height(num_blocks_in_epoch as u64)
        .await?;
    debug!(
        "epoch block:\n{}",
        serde_json::to_string_pretty(&block).unwrap()
    );

    assert_eq!(block.height, num_blocks_in_epoch as u64);

    genesis_node.stop().await;
    peer_node.stop().await;

    Ok(())
}

#[actix_web::test]
async fn heavy_test_commitments_basic_test() -> eyre::Result<()> {
    // tracing
    initialize_tracing();
    // ===== TEST SETUP =====
    // Create test environment with a funded signer for transaction creation
    let mut config = NodeConfig::testnet();
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);

    let node = IrysNodeTest::new_genesis(config.clone()).start().await;

    // Initialize packing and mining
    wait_for_packing(
        node.node_ctx.actor_addresses.packing.clone(),
        Some(Duration::from_secs(10)),
    )
    .await?;

    // ===== TEST CASE 1: Stake Commitment Creation and Processing =====
    // Create a new stake commitment transaction
    let stake_tx = CommitmentTransaction {
        commitment_type: CommitmentType::Stake,
        fee: 1,
        ..Default::default()
    };
    let stake_tx = signer.sign_commitment(stake_tx).unwrap();
    info!("Generated stake_tx.id: {}", stake_tx.id);

    // Verify stake commitment starts in 'Unknown' state
    let status = node.get_commitment_snapshot_status(&stake_tx);
    assert_eq!(status, CommitmentSnapshotStatus::Unknown);

    // Submit stake commitment via API
    node.post_commitment_tx(&stake_tx).await;

    // Mine a block to include the commitment
    node.mine_blocks(1).await?;

    // Verify stake commitment is now 'Accepted'
    let status = node.get_commitment_snapshot_status(&stake_tx);
    assert_eq!(status, CommitmentSnapshotStatus::Accepted);

    // ===== TEST CASE 2: Pledge Creation for Staked Address =====
    // Create a pledge commitment for the already staked address
    let pledge_tx = CommitmentTransaction {
        commitment_type: CommitmentType::Pledge,
        fee: 1,
        ..Default::default()
    };
    let pledge_tx = signer.sign_commitment(pledge_tx).unwrap();
    info!("Generated pledge_tx.id: {}", pledge_tx.id);

    // Verify pledge starts in 'Unknown' state
    let status = node.get_commitment_snapshot_status(&pledge_tx);
    assert_eq!(status, CommitmentSnapshotStatus::Unknown);

    // Submit pledge via API
    node.post_commitment_tx(&pledge_tx).await;

    // Verify pledge is still 'Unknown' before mining
    let status = node.get_commitment_snapshot_status(&pledge_tx);
    assert_eq!(status, CommitmentSnapshotStatus::Unknown);

    // Mine a block to include the pledge
    debug!("MINE BLOCK - Height should become 2");
    node.mine_blocks(1).await?;

    // Verify pledge is now 'Accepted' after mining
    let status = node.get_commitment_snapshot_status(&pledge_tx);
    assert_eq!(status, CommitmentSnapshotStatus::Accepted);

    // ===== TEST CASE 3: Re-submitting Existing Commitment =====
    // Verify stake commitment is still accepted
    let status = node.get_commitment_snapshot_status(&stake_tx);
    assert_eq!(status, CommitmentSnapshotStatus::Accepted);

    // Re-submit the same stake commitment
    node.post_commitment_tx(&stake_tx).await;
    node.mine_blocks(1).await?;

    // Verify stake is still 'Accepted' (idempotent operation)
    let status = node.get_commitment_snapshot_status(&stake_tx);
    assert_eq!(status, CommitmentSnapshotStatus::Accepted);

    // ===== TEST CASE 4: Pledge Without Stake (Should Fail) =====
    // Create a new signer without any stake commitment
    let signer2 = IrysSigner::random_signer(&config.consensus_config());

    // Create a pledge for the unstaked address
    let pledge_tx = CommitmentTransaction {
        commitment_type: CommitmentType::Pledge,
        fee: 1,
        ..Default::default()
    };
    let pledge_tx = signer2.sign_commitment(pledge_tx).unwrap();
    info!("Generated pledge_tx.id: {}", pledge_tx.id);

    // Verify pledge starts in 'Unstaked' state
    let status = node.get_commitment_snapshot_status(&pledge_tx);
    assert_eq!(status, CommitmentSnapshotStatus::Unstaked);

    // Submit pledge via API
    node.post_commitment_tx(&pledge_tx).await;
    node.mine_blocks(1).await?;

    // Verify pledge remains 'Unstaked' (invalid without stake)
    let status = node.get_commitment_snapshot_status(&pledge_tx);
    assert_eq!(status, CommitmentSnapshotStatus::Unstaked);

    // ===== TEST CLEANUP =====
    node.node_ctx.stop().await;
    Ok(())
}

async fn post_stake_commitment(
    node: &IrysNodeTest<IrysNodeCtx>,
    signer: &IrysSigner,
) -> CommitmentTransaction {
    let stake_tx = CommitmentTransaction {
        commitment_type: CommitmentType::Stake,
        fee: 1,
        ..Default::default()
    };
    let stake_tx = signer.sign_commitment(stake_tx).unwrap();
    info!("Generated stake_tx.id: {}", stake_tx.id.0.to_base58());

    // Submit stake commitment via API
    node.post_commitment_tx(&stake_tx).await;
    stake_tx
}

async fn post_pledge_commitment(
    node: &IrysNodeTest<IrysNodeCtx>,
    signer: &IrysSigner,
    anchor: H256,
) -> CommitmentTransaction {
    let pledge_tx = CommitmentTransaction {
        commitment_type: CommitmentType::Pledge,
        anchor,
        fee: 1,
        ..Default::default()
    };
    let pledge_tx = signer.sign_commitment(pledge_tx).unwrap();
    info!("Generated pledge_tx.id: {}", pledge_tx.id.0.to_base58());

    // Submit pledge commitment via API
    node.post_commitment_tx(&pledge_tx).await;

    pledge_tx
}

fn validate_pledge_assignments(
    commitment_state_guard: &CommitmentStateReadGuard,
    pa_guard: &PartitionAssignmentsReadGuard,
    address_name: &str,
    address: &Address,
) -> eyre::Result<Vec<H256>> {
    // Get all partition hashes from this address's pledges
    // Each pledge contains a partition_hash that identifies which partition the pledge is for
    let partition_hashes: Vec<Option<H256>> = commitment_state_guard
        .read()
        .pledge_commitments
        .get(address)
        .map(|pledges| pledges.iter().map(|pledge| pledge.partition_hash).collect())
        .unwrap_or_default();

    // Retrieve the full commitment state entries for this address
    // These entries contain the complete pledge information needed for validation
    // Note: mostly used for debug logging here
    let binding = commitment_state_guard.read();
    let result = binding.pledge_commitments.get(address);
    let direct = match result {
        Some(entries) => entries,
        None => {
            return Err(eyre!(
                "Expected to find commitment entries for {}",
                address_name
            ))
        }
    };

    debug!(
        "\nGot partition_hashes from pledges for {} : address {}\nPartition Assignments in epoch_service:\n{:#?}\nPledge Status in CommitmentState:\n{:#?}",
        address_name, address, partition_hashes, direct
    );

    // For each partition hash entry in the commitment state for this address
    for partition_hash in partition_hashes.iter() {
        // Expect it to exist
        if let Some(partition_hash) = partition_hash {
            // Check that the partition assignments state is in sync with the commitment state
            // with regards to the partition_hash
            let pa = pa_guard.read().get_assignment(*partition_hash);
            match pa {
                Some(pa) => {
                    // Verify the partition assignments in the partition assignment state
                    assert_eq!(&pa.miner_address, address);
                }
                None => return Err(eyre::eyre!("expected partition assignment for hash")),
            }
        } else {
            return Err(eyre::eyre!(
                "expected partition hash for {}'s pledge, was None should be Some(partition_hash)",
                address_name
            ));
        }
    }

    // Return a vec of partition hashes
    Ok(direct
        .iter()
        .filter_map(|entry| entry.partition_hash)
        .collect())
}
