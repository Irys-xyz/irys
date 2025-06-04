use crate::{api::post_commitment_tx_request, utils::*};
use actix_web::{middleware::Logger, App};
use alloy_core::primitives::U256;
use alloy_genesis::GenesisAccount;
use assert_matches::assert_matches;
use base58::ToBase58;
use irys_actors::{
    packing::wait_for_packing, CommitmentCacheMessage, CommitmentCacheStatus,
    CommitmentStateReadGuard, GetCommitmentStateGuardMessage, GetPartitionAssignmentsGuardMessage,
    PartitionAssignmentsReadGuard,
};
use irys_api_server::routes;
use irys_chain::IrysNodeCtx;
use irys_primitives::CommitmentType;
use irys_testing_utils::initialize_tracing;
use irys_types::{irys::IrysSigner, Address, CommitmentTransaction, NodeConfig, H256};
use tokio::time::Duration;
use tracing::{debug, debug_span, info};

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

    peer_node
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
    // ===== TEST SETUP =====
    // Create test environment with a funded signer for transaction creation
    let mut config = NodeConfig::testnet();
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.consensus.extend_genesis_accounts(vec![(
        signer.address(),
        GenesisAccount {
            balance: U256::from(690000000000000000_u128),
            ..Default::default()
        },
    )]);
    let node = IrysNodeTest::new_genesis(config.clone()).start().await;

    let uri = format!(
        "http://127.0.0.1:{}",
        node.node_ctx.config.node_config.http.bind_port
    );

    // Initialize packing and mining
    wait_for_packing(
        node.node_ctx.actor_addresses.packing.clone(),
        Some(Duration::from_secs(10)),
    )
    .await?;

    let api_state = node.node_ctx.get_api_state();
    let _db = api_state.db.clone();

    // Start the API server
    let _app = actix_web::test::init_service(
        App::new()
            .wrap(Logger::default())
            .app_data(actix_web::web::Data::new(api_state))
            .service(routes()),
    )
    .await;

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
    let status = get_commitment_status(&stake_tx, &node.node_ctx).await;
    assert_eq!(status, CommitmentCacheStatus::Unknown);

    // Submit stake commitment via API
    post_commitment_tx_request(&uri, &stake_tx).await;

    // Mine a block to include the commitment
    node.mine_blocks(1).await?;

    // Verify stake commitment is now 'Accepted'
    let status = get_commitment_status(&stake_tx, &node.node_ctx).await;
    assert_eq!(status, CommitmentCacheStatus::Accepted);

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
    let status = get_commitment_status(&pledge_tx, &node.node_ctx).await;
    assert_eq!(status, CommitmentCacheStatus::Unknown);

    // Submit pledge via API
    post_commitment_tx_request(&uri, &pledge_tx).await;

    // Verify pledge is still 'Unknown' before mining
    let status = get_commitment_status(&pledge_tx, &node.node_ctx).await;
    assert_eq!(status, CommitmentCacheStatus::Unknown);

    // Mine a block to include the pledge
    debug!("MINE BLOCK - Height should become 2");
    node.mine_blocks(1).await?;

    // Verify pledge is now 'Accepted' after mining
    let status = get_commitment_status(&pledge_tx, &node.node_ctx).await;
    assert_eq!(status, CommitmentCacheStatus::Accepted);

    // ===== TEST CASE 3: Re-submitting Existing Commitment =====
    // Verify stake commitment is still accepted
    let status = get_commitment_status(&stake_tx, &node.node_ctx).await;
    assert_eq!(status, CommitmentCacheStatus::Accepted);

    // Re-submit the same stake commitment
    post_commitment_tx_request(&uri, &stake_tx).await;
    node.mine_blocks(1).await?;

    // Verify stake is still 'Accepted' (idempotent operation)
    let status = get_commitment_status(&stake_tx, &node.node_ctx).await;
    assert_eq!(status, CommitmentCacheStatus::Accepted);

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
    let status = get_commitment_status(&pledge_tx, &node.node_ctx).await;
    assert_eq!(status, CommitmentCacheStatus::Unstaked);

    // Submit pledge via API
    post_commitment_tx_request(&uri, &pledge_tx).await;
    node.mine_blocks(1).await?;

    // Verify pledge remains 'Unstaked' (invalid without stake)
    let status = get_commitment_status(&pledge_tx, &node.node_ctx).await;
    assert_eq!(status, CommitmentCacheStatus::Unstaked);

    // ===== TEST CLEANUP =====
    node.node_ctx.stop().await;
    Ok(())
}

async fn get_commitment_status(
    commitment_tx: &CommitmentTransaction,
    node_context: &IrysNodeCtx,
) -> CommitmentCacheStatus {
    let commitment_cache = &node_context.service_senders.commitment_cache;
    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();

    let _ = commitment_cache.send(CommitmentCacheMessage::GetCommitmentStatus {
        commitment_tx: commitment_tx.clone(),
        response: oneshot_tx,
    });

    oneshot_rx
        .await
        .expect("to receive CommitmentStatus from GetCommitmentStatus message")
}

#[actix_web::test]
async fn heavy_test_commitments_3epochs_test() -> eyre::Result<()> {
    // ===== TEST ENVIRONMENT SETUP =====
    // Configure logging to reduce noise while keeping relevant commitment outputs
    std::env::set_var("RUST_LOG", "debug,reth_basic_payload_builder=off,irys_gossip_service=off,providers::db=off,reth_payload_builder::service=off,irys_actors::broadcast_mining_service=off,reth_ethereum_payload_builder=off,provider::static_file=off,engine::persistence=off,provider::storage_writer=off,reth_engine_tree::persistence=off,irys_actors::cache_service=off,irys_actors::block_validation=off,irys_vdf=off,irys_actors::block_tree_service=off,irys_actors::vdf_service=off,rys_gossip_service::service=off,eth_ethereum_payload_builder=off,reth_node_events::node=off,reth::cli=off,reth_engine_tree::tree=off,irys_actors::ema_service=off,irys_efficient_sampling=off,hyper_util::client::legacy::connect::http=off,hyper_util::client::legacy::pool=off,irys_database::migration::v0_to_v1=off,irys_storage::storage_module=off,actix_server::worker=off,irys::packing::update=off,engine::tree=off,irys_actors::mining=error,payload_builder=off,irys_actors::block_producer=info,irys_actors::reth_service=off,irys_actors::packing=off,irys_actors::reth_service=off,irys::packing::progress=off,irys_chain::vdf=off,irys_vdf::vdf_state=off");

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

        // Initialize blockchain components
        node.start_mining().await;

        let uri = format!(
            "http://127.0.0.1:{}",
            node.node_ctx.config.node_config.http.bind_port
        );

        // Initialize blockchain components
        wait_for_packing(
            node.node_ctx.actor_addresses.packing.clone(),
            Some(Duration::from_secs(10)),
        )
        .await?;

        // Initialize API for submitting commitment transactions
        let api_state = node.node_ctx.get_api_state();

        let _app = actix_web::test::init_service(
            App::new()
                .wrap(Logger::default())
                .app_data(actix_web::web::Data::new(api_state))
                .service(routes()),
        )
        .await;

        // Get access to commitment and partition services for verification
        let epoch_service = node.node_ctx.actor_addresses.epoch_service.clone();
        let commitment_state_guard = epoch_service
            .send(GetCommitmentStateGuardMessage)
            .await
            .unwrap();
        let pa_guard = epoch_service
            .send(GetPartitionAssignmentsGuardMessage)
            .await
            .unwrap();

        // ===== PHASE 1: Verify Genesis Block Initialization =====
        // Check that the genesis block producer has the expected initial pledges

        let commitment_state = commitment_state_guard.read();
        let pledges = commitment_state.pledge_commitments.get(&genesis_signer);
        if let Some(pledges) = pledges {
            assert_eq!(
                pledges.len(),
                3,
                "Genesis miner should have exactly 3 pledges"
            );
        } else {
            panic!("Expected genesis miner to have pledges!");
        }
        drop(commitment_state); // Release lock to allow node operations

        // ===== PHASE 2: First Epoch - Create Commitments =====
        // Create stake commitment for first test signer
        post_stake_commitment(&uri, &signer1).await;

        // Create two pledge commitments for first test signer
        let anchor = post_pledge_commitment(&uri, &signer1, H256::default())
            .await
            .id;
        post_pledge_commitment(&uri, &signer1, anchor).await;

        // Create stake commitment for second test signer
        post_stake_commitment(&uri, &signer2).await;

        // Mine enough blocks to reach the first epoch boundary
        info!("MINE FIRST EPOCH BLOCK:");
        node.mine_blocks(num_blocks_in_epoch).await?;

        // ===== PHASE 3: Verify First Epoch Assignments =====
        // Verify that all pledges have been assigned partitions
        validate_pledge_assignments(&commitment_state_guard, &pa_guard, &genesis_signer);
        validate_pledge_assignments(&commitment_state_guard, &pa_guard, &signer1.address());

        // Verify commitment state contains expected pledges and stakes
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

        // ===== PHASE 4: Second Epoch - Add More Commitments =====
        // Create pledge for second test signer
        post_pledge_commitment(&uri, &signer2, H256::default()).await;

        // Mine enough blocks to reach the second epoch boundary
        info!("MINE SECOND EPOCH BLOCK:");
        node.mine_blocks(num_blocks_in_epoch + 2).await?;

        // ===== PHASE 5: Verify Second Epoch Assignments =====
        // Verify all signers have proper partition assignments for all pledges
        genesis_parts_before =
            validate_pledge_assignments(&commitment_state_guard, &pa_guard, &genesis_signer);
        signer1_parts_before =
            validate_pledge_assignments(&commitment_state_guard, &pa_guard, &signer1.address());
        signer2_parts_before =
            validate_pledge_assignments(&commitment_state_guard, &pa_guard, &signer2.address());

        node
    };

    // Restart the node
    info!("Restarting node");
    let restarted_node = node.stop().await.start().await;

    // Get access to commitment and partition services for verification
    let epoch_service = restarted_node
        .node_ctx
        .actor_addresses
        .epoch_service
        .clone();
    let commitment_state_guard = epoch_service
        .send(GetCommitmentStateGuardMessage)
        .await
        .unwrap();
    let pa_guard = epoch_service
        .send(GetPartitionAssignmentsGuardMessage)
        .await
        .unwrap();

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
    let genesis_parts_after =
        validate_pledge_assignments(&commitment_state_guard, &pa_guard, &genesis_signer);
    let signer1_parts_after =
        validate_pledge_assignments(&commitment_state_guard, &pa_guard, &signer1.address());
    let signer2_parts_after =
        validate_pledge_assignments(&commitment_state_guard, &pa_guard, &signer2.address());

    assert_eq!(genesis_parts_after, genesis_parts_before);
    assert_eq!(signer1_parts_after, signer1_parts_before);
    assert_eq!(signer2_parts_after, signer2_parts_before);

    // ===== TEST CLEANUP =====
    restarted_node.node_ctx.stop().await;
    Ok(())
}

async fn post_stake_commitment(uri: &str, signer: &IrysSigner) {
    let stake_tx = CommitmentTransaction {
        commitment_type: CommitmentType::Stake,
        fee: 1,
        ..Default::default()
    };
    let stake_tx = signer.sign_commitment(stake_tx).unwrap();
    info!("Generated stake_tx.id: {}", stake_tx.id.0.to_base58());

    // Submit stake commitment via API
    post_commitment_tx_request(uri, &stake_tx).await;
}

async fn post_pledge_commitment(
    uri: &str,
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
    post_commitment_tx_request(uri, &pledge_tx).await;

    pledge_tx
}

fn validate_pledge_assignments(
    commitment_state_guard: &CommitmentStateReadGuard,
    pa_guard: &PartitionAssignmentsReadGuard,
    address: &Address,
) -> Vec<H256> {
    // Extract partition hashes from pledges
    let partition_hashes: Vec<Option<H256>> = commitment_state_guard
        .read()
        .pledge_commitments
        .get(address)
        .map(|pledges| pledges.iter().map(|pledge| pledge.partition_hash).collect())
        .unwrap_or_default();

    let direct = commitment_state_guard
        .read()
        .pledge_commitments
        .get(address)
        .unwrap()
        .clone();

    debug!(
        "Got partition_hashes from pledges {:#?} {:#?}",
        partition_hashes, direct
    );

    // Look up their partition assignments
    for partition_hash in partition_hashes.iter() {
        if let Some(partition_hash) = partition_hash {
            let pa = pa_guard.read().get_assignment(*partition_hash);
            match pa {
                Some(pa) => {
                    // Verify the partition assignments in the partition assignment state
                    assert_eq!(&pa.miner_address, address);
                }
                None => panic!("expected partition assignment for hash"),
            }
        } else {
            panic!("expected partition hash for pledge")
        }
    }

    // Return a vec of partition hashes
    direct
        .iter()
        .filter_map(|entry| entry.partition_hash)
        .collect()
}
