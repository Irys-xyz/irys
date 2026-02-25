use crate::utils::*;
use alloy_rpc_types_eth::TransactionTrait as _;
use assert_matches::assert_matches;
use eyre::eyre;

use irys_chain::IrysNodeCtx;
use irys_domain::{CommitmentSnapshotStatus, EpochSnapshot};
use irys_reth_node_bridge::irys_reth::shadow_tx::{ShadowTransaction, TransactionPacket};
use irys_testing_utils::initialize_tracing;
use irys_types::{
    irys::IrysSigner, CommitmentTransaction, CommitmentTransactionV2, CommitmentTypeV2,
    IrysAddress, NodeConfig, H256, U256,
};
use std::sync::Arc;
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

#[tokio::test]
async fn heavy_test_commitments_3epochs_test() -> eyre::Result<()> {
    // ===== TEST ENVIRONMENT SETUP =====
    // Configure logging to reduce noise while keeping relevant commitment outputs
    std::env::set_var("RUST_LOG", "debug,irys_database=off,irys_p2p::gossip_service=off,irys_actors::storage_module_service=debug,trie=off,irys_reth::evm=off,engine::root=off,irys_p2p::peer_list=off,storage::db::mdbx=off,reth_basic_payload_builder=off,irys_gossip_service=off,providers::db=off,reth_payload_builder::service=off,irys_actors::mining_bus=off,reth_ethereum_payload_builder=off,provider::static_file=off,engine::persistence=off,provider::storage_writer=off,reth_engine_tree::persistence=off,irys_actors::cache_service=off,irys_vdf=off,irys_actors::block_tree_service=off,irys_actors::vdf_service=off,rys_gossip_service::service=off,eth_ethereum_payload_builder=off,reth_node_events::node=off,reth::cli=off,reth_engine_tree::tree=off,irys_actors::ema_service=off,irys_efficient_sampling=off,hyper_util::client::legacy::connect::http=off,hyper_util::client::legacy::pool=off,irys_database::migration::v0_to_v1=off,irys_storage::storage_module=off,actix_server::worker=off,irys::packing::update=off,engine::tree=off,irys_actors::mining=error,payload_builder=off,irys_actors::reth_service=off,irys_actors::packing=off,irys_actors::reth_service=off,irys::packing::progress=off,irys_chain::vdf=off,irys_vdf::vdf_state=off");
    initialize_tracing();

    // ===== TEST PURPOSE: Multiple Epochs with Commitments =====
    // This test verifies that:
    // 1. Stake and pledge commitments are correctly processed across multiple epoch transitions
    // 2. Partition assignments are properly created and maintained for all pledges
    // 3. The commitment state correctly tracks stake and pledge relationships
    // 4. Capacity pledges can migrate to ledger slots and maintain their storage_module.id mappings

    // Configure a test network with accelerated epochs (2 blocks per epoch)
    let num_blocks_in_epoch = 2;
    let mut config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    // set block_migration depth to one less than epoch depth
    let block_migration_depth = num_blocks_in_epoch - 1;
    // set block migration depth so epoch blocks go to index correctly
    config.consensus.get_mut().block_migration_depth = block_migration_depth.try_into()?;
    config.consensus.get_mut().num_chunks_in_partition = 20;
    config.consensus.get_mut().num_chunks_in_recall_range = 5;

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
    let sm_infos_before;

    let node = {
        // Start a test node with custom configuration
        let node = IrysNodeTest::new_genesis(config.clone())
            .start_and_wait_for_packing("GENESIS", 10)
            .await;

        // Get access to commitment and partition services for verification
        let block_tree_guard = &node.node_ctx.block_tree_guard;
        let epoch_snapshot = block_tree_guard.read().canonical_epoch_snapshot();

        // ===== PHASE 1: Verify Genesis Block Initialization =====
        // Check that the genesis block producer has the expected initial pledges
        let genesis_block = node.get_block_by_height(0).await?;
        let commitment_state = &epoch_snapshot.commitment_state;
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
            genesis_block.commitment_tx_ids(),
            stakes.unwrap().id,
            pledges.unwrap().iter().map(|x| x.id).collect::<Vec<_>>(),
        );

        // ===== PHASE 2: First Epoch - Create Commitments =====
        // Create stake commitment for first test signer
        let stake_tx1 = post_stake_commitment(&node, &signer1).await;

        // Create two pledge commitments for first test signer
        let pledge1 = &node.post_pledge_commitment_with_signer(&signer1).await;

        let pledge2 = &node.post_pledge_commitment_with_signer(&signer1).await;

        // Create stake commitment for second test signer
        let stake_tx2 = post_stake_commitment(&node, &signer2).await;

        // Mine enough blocks to reach the first epoch boundary
        info!("MINE FIRST EPOCH BLOCK:");
        node.mine_blocks(num_blocks_in_epoch).await?;

        debug!(
            "Post Commitments:\nstake1: {:?}\nstake2: {:?}\npledge1: {:?}\npledge2: {:?}\n",
            stake_tx1.id(),
            stake_tx2.id(),
            pledge1.id(),
            pledge2.id()
        );

        // Block height: 1 should have two stake and two pledge commitments
        let expected_ids = [stake_tx1.id(), stake_tx2.id(), pledge1.id(), pledge2.id()];
        let block_1 = node.get_block_by_height(1).await.unwrap();
        let commitments_1 = block_1.commitment_tx_ids();
        debug!(
            "Block commitments - height: {:?}\n{:#?}",
            block_1.height, commitments_1,
        );
        assert_eq!(commitments_1.len(), 4);
        assert!(expected_ids.iter().all(|id| commitments_1.contains(id)));

        // Block height: 2 is an epoch block and should have the same commitments and no more
        let block_2 = node.get_block_by_height(2).await.unwrap();
        let commitments_2 = block_2.commitment_tx_ids();
        debug!(
            "Block commitments - height: {:?}\n{:#?}",
            block_2.height, commitments_2
        );
        assert_eq!(commitments_2.len(), expected_ids.len());
        assert!(expected_ids.iter().all(|id| commitments_2.contains(id)));

        // ===== PHASE 3: Verify First Epoch Assignments =====
        let epoch_snapshot = block_tree_guard.read().canonical_epoch_snapshot();
        debug!("Epoch height: {}", epoch_snapshot.epoch_block.height);

        // Verify that all pledges have been assigned partitions
        assert_ok!(validate_pledge_assignments(
            epoch_snapshot.clone(),
            "genesis",
            &genesis_signer,
        ));
        assert_ok!(validate_pledge_assignments(
            epoch_snapshot.clone(),
            "signer1",
            &signer1.address(),
        ));

        // Verify commitment state contains expected pledges and stakes
        let commitment_state = &epoch_snapshot.commitment_state;

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

        // ===== PHASE 4: Second Epoch - Add More Commitments =====
        // Create pledge for second test signer
        let pledge3 = post_pledge_commitment(&node, &signer2, node.get_anchor().await?).await;
        info!("signer2: {} post pledge: {}", signer2_address, pledge3.id());

        // Add some data so that the submit ledger grows and a capacity
        // partition is assigned to submit ledger slot 1

        // Create data chunks for this partition to store
        // Make sure the data chunks fill > 50% of a partition to trigger ledger growth
        let num_chunks = config.consensus.get_mut().num_chunks_in_partition / 2 + 2;
        let chunk_size = config.consensus.get_mut().chunk_size;

        let mut data = Vec::with_capacity((chunk_size * num_chunks) as usize);
        for chunk_index in 0..num_chunks {
            let chunk_data = vec![chunk_index as u8; chunk_size as usize];
            data.extend(chunk_data);
        }

        // DATA TX: Create and post data transaction using proper pricing from API
        let data_tx = node
            .post_data_tx(node.get_anchor().await?, data, &signer1)
            .await;

        let res = node.wait_for_mempool(data_tx.header.id, 5).await;
        assert_matches!(res, Ok(()));

        // Mine enough blocks to reach the second epoch boundary
        info!("MINE SECOND EPOCH BLOCK:");
        node.mine_blocks(num_blocks_in_epoch + 2).await?;

        let block_3 = node.get_block_by_height(3).await.unwrap();
        let commitments_3 = block_3.commitment_tx_ids();
        debug!("Block - height: {:?}\n{:#?}", block_3.height, commitments_3);

        // Block height: 3 should have 1 pledge commitment
        assert_eq!(commitments_3.len(), 1);
        assert_eq!(commitments_3, vec![pledge3.id()]);

        // ===== PHASE 5: Verify Second Epoch Assignments =====
        let epoch_snapshot = block_tree_guard.read().canonical_epoch_snapshot();
        debug!("Epoch height: {}", epoch_snapshot.epoch_block.height);

        // Verify all signers have proper partition assignments for all pledges
        genesis_parts_before = assert_ok!(validate_pledge_assignments(
            epoch_snapshot.clone(),
            "genesis",
            &genesis_signer,
        ));
        signer1_parts_before = assert_ok!(validate_pledge_assignments(
            epoch_snapshot.clone(),
            "signer1",
            &signer1_address,
        ));
        signer2_parts_before = assert_ok!(validate_pledge_assignments(
            epoch_snapshot.clone(),
            "signer2",
            &signer2_address,
        ));

        sm_infos_before = node
            .node_ctx
            .block_tree_guard
            .read()
            .canonical_epoch_snapshot()
            .map_storage_modules_to_partition_assignments();

        node.wait_until_height_confirmed(6, 10).await?;

        node
    };

    // Restart the node
    info!("Restarting node");
    let stopped_node = node.stop().await;
    let restarted_node = stopped_node.start().await;

    // wait a few seconds for node to wake up
    restarted_node.wait_for_packing(10).await;

    // Get access to commitment and partition services for verification
    let block_tree_guard = &restarted_node.node_ctx.block_tree_guard;
    let epoch_snapshot = block_tree_guard.read().canonical_epoch_snapshot();

    // Make sure genesis has 3 pledge commitments
    assert_eq!(
        epoch_snapshot
            .commitment_state
            .pledge_commitments
            .get(&genesis_signer)
            .expect("commitments for genesis miner")
            .len(),
        3
    );

    // Make sure signer1 has 2 pledge commitments
    assert_eq!(
        epoch_snapshot
            .commitment_state
            .pledge_commitments
            .get(&signer1.address())
            .expect("commitments for signer1 miner")
            .len(),
        2
    );

    // Make sure signer2 has 1 pledge commitment
    assert_eq!(
        epoch_snapshot
            .commitment_state
            .pledge_commitments
            .get(&signer2.address())
            .expect("commitments for signer2 miner")
            .len(),
        1
    );

    // Verify the partition assignments persist (and the map to the same pledges)
    let genesis_parts_after = assert_ok!(validate_pledge_assignments(
        epoch_snapshot.clone(),
        "genesis",
        &genesis_signer,
    ));
    let signer1_parts_after = assert_ok!(validate_pledge_assignments(
        epoch_snapshot.clone(),
        "signer1",
        &signer1.address(),
    ));
    let signer2_parts_after = assert_ok!(validate_pledge_assignments(
        epoch_snapshot.clone(),
        "signer2",
        &signer2.address(),
    ));

    assert_eq!(genesis_parts_after, genesis_parts_before);
    assert_eq!(signer1_parts_after, signer1_parts_before);
    assert_eq!(signer2_parts_after, signer2_parts_before);

    let sm_infos_after = restarted_node
        .node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot()
        .map_storage_modules_to_partition_assignments();

    assert_eq!(sm_infos_before, sm_infos_after);

    // ===== TEST CLEANUP =====
    restarted_node.stop().await;
    Ok(())
}

#[tokio::test]
async fn heavy_no_commitments_basic_test() -> eyre::Result<()> {
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
    let mut genesis_config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    genesis_config.consensus.get_mut().chunk_size = 32;

    // Start the genesis node and wait for packing
    let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    // Initialize the peer with our keypair/signer
    let peer_config = genesis_node.testing_peer();

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

#[test_log::test(tokio::test)]
async fn heavy_test_commitments_basic_test() -> eyre::Result<()> {
    // tracing
    // ===== TEST SETUP =====
    // Create test environment with a funded signer for transaction creation
    let mut config = NodeConfig::testing();
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);

    let node = IrysNodeTest::new_genesis(config.clone()).start().await;

    // Initialize packing and mining
    node.node_ctx
        .packing_waiter
        .wait_for_idle(Some(Duration::from_secs(10)))
        .await?;

    // ===== TEST CASE 1: Stake Commitment Creation and Processing =====
    // Create a new stake commitment transaction
    let stake_tx = post_stake_commitment(&node, &signer).await;

    info!("Generated stake_tx.id: {}", stake_tx.id());

    // Verify stake commitment starts in 'Unknown' state (already posted by helper)
    let status = node.get_commitment_snapshot_status(&stake_tx);
    assert_eq!(status, CommitmentSnapshotStatus::Unknown);

    // Mine a block to include the commitment
    node.mine_blocks(1).await?;

    // Verify stake commitment is now 'Accepted'
    let status = node.get_commitment_snapshot_status(&stake_tx);
    assert_eq!(status, CommitmentSnapshotStatus::Accepted);

    // ===== TEST CASE 2: Pledge Creation for Staked Address =====
    // Create a pledge commitment for the already staked address using the pricing API pattern
    let pledge_tx = post_pledge_commitment(&node, &signer, node.get_anchor().await?).await;
    info!("Generated pledge_tx.id: {}", pledge_tx.id());

    // Verify pledge starts in 'Unknown' state (already posted by helper)
    let status = node.get_commitment_snapshot_status(&pledge_tx);
    assert_eq!(status, CommitmentSnapshotStatus::Unknown);

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
    node.post_commitment_tx(&stake_tx).await?;
    node.mine_blocks(1).await?;

    // Verify stake is still 'Accepted' (idempotent operation)
    let status = node.get_commitment_snapshot_status(&stake_tx);
    assert_eq!(status, CommitmentSnapshotStatus::Accepted);

    // ===== TEST CASE 4: Pledge Without Stake (Should Fail) =====
    // Create a new signer without any stake commitment (unfunded to test failure case)
    let signer2 = IrysSigner::random_signer(&config.consensus_config());

    // Create a pledge for the unstaked address manually
    let consensus = &node.node_ctx.config.consensus;
    let mut pledge_tx = CommitmentTransaction::new_pledge(
        consensus,
        node.get_anchor().await?,
        node.node_ctx.mempool_pledge_provider.as_ref(),
        signer2.address(),
    )
    .await;
    signer2.sign_commitment(&mut pledge_tx).unwrap();
    info!("Generated pledge_tx.id: {}", pledge_tx.id());

    // Verify pledge starts in 'Unstaked' state
    let status = node.get_commitment_snapshot_status(&pledge_tx);
    assert_eq!(status, CommitmentSnapshotStatus::Unstaked);

    // Submit pledge via API (expect this to fail for unfunded signer)
    let post_result = node.post_commitment_tx(&pledge_tx).await;
    assert!(
        post_result.is_err(),
        "Expected unfunded pledge to fail, but it succeeded"
    );

    // Since posting failed (as expected), we can't mine it, but the status should remain Unstaked
    let status = node.get_commitment_snapshot_status(&pledge_tx);
    assert_eq!(status, CommitmentSnapshotStatus::Unstaked);

    // ===== TEST CLEANUP =====
    node.stop().await;
    Ok(())
}

async fn post_stake_commitment(
    node: &IrysNodeTest<IrysNodeCtx>,
    signer: &IrysSigner,
) -> CommitmentTransaction {
    // Get stake price from API
    let price_info = node
        .get_stake_price()
        .await
        .expect("Failed to get stake price from API");

    let consensus = &node.node_ctx.config.consensus;
    let anchor = node
        .get_anchor()
        .await
        .expect("anchor should be available for stake commitment");
    let mut stake_tx = CommitmentTransaction::V2(irys_types::CommitmentV2WithMetadata {
        tx: CommitmentTransactionV2 {
            commitment_type: CommitmentTypeV2::Stake,
            anchor,
            fee: price_info.fee.try_into().expect("fee should fit in u64"),
            value: price_info.value,
            ..CommitmentTransactionV2::new(consensus)
        },
        metadata: Default::default(),
    });

    info!("Created stake_tx with value: {:?}", stake_tx.value());
    signer.sign_commitment(&mut stake_tx).unwrap();
    info!("Generated stake_tx.id: {}", stake_tx.id());

    // Submit stake commitment via API
    node.post_commitment_tx(&stake_tx)
        .await
        .expect("posted commitment tx");
    stake_tx
}

async fn post_pledge_commitment(
    node: &IrysNodeTest<IrysNodeCtx>,
    signer: &IrysSigner,
    anchor: H256,
) -> CommitmentTransaction {
    // Get pledge price from API
    let price_info = node
        .get_pledge_price(signer.address())
        .await
        .expect("Failed to get pledge price from API");

    let consensus = &node.node_ctx.config.consensus;
    let mut pledge_tx = CommitmentTransaction::V2(irys_types::CommitmentV2WithMetadata {
        tx: CommitmentTransactionV2 {
            commitment_type: CommitmentTypeV2::Pledge {
                pledge_count_before_executing: 0, // First pledge
            },
            anchor,
            fee: price_info.fee.try_into().expect("fee should fit in u64"),
            value: price_info.value,
            ..CommitmentTransactionV2::new(consensus)
        },
        metadata: Default::default(),
    });

    signer.sign_commitment(&mut pledge_tx).unwrap();
    info!("Generated pledge_tx.id: {}", pledge_tx.id());

    // Submit pledge commitment via API
    node.post_commitment_tx(&pledge_tx)
        .await
        .expect("posted commitment tx");

    pledge_tx
}

/// Validates that the partition_hashes associated with pledges in the EpochSnapshot::commitment_state are reflected
/// in the EpochSnapshot::partition_assignments which maps partition_hashes to ledger or capacity.
fn validate_pledge_assignments(
    epoch_snapshot: Arc<EpochSnapshot>,
    address_name: &str,
    address: &IrysAddress,
) -> eyre::Result<Vec<H256>> {
    // Get all partition hashes from this address's pledges
    // Each pledge contains a partition_hash that identifies which partition the pledge is for
    let partition_hashes: Vec<Option<H256>> = epoch_snapshot
        .commitment_state
        .pledge_commitments
        .get(address)
        .map(|pledges| pledges.iter().map(|pledge| pledge.partition_hash).collect())
        .unwrap_or_default();

    // Retrieve the full commitment state entries for this address
    // These entries contain the complete pledge information needed for validation
    // Note: mostly used for debug logging here
    let binding = &epoch_snapshot.commitment_state;
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
        "\nGot partition_hashes from pledges for {} : address {}\nPartition Assignments in epoch snapshot:\n{:#?}\nPledge Status in CommitmentState:\n{:#?}",
        address_name, address, partition_hashes, direct
    );

    // For each partition hash entry in the commitment state for this address
    for partition_hash in partition_hashes.iter() {
        // Expect it to exist
        if let Some(partition_hash) = partition_hash {
            // Check that the partition assignments state is in sync with the commitment state
            // with regards to the partition_hash
            let pa = epoch_snapshot.get_data_partition_assignment(*partition_hash);
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

#[test_log::test(tokio::test)]
async fn heavy_test_update_reward_address() -> eyre::Result<()> {
    initialize_tracing();

    // Setup: 2-block epochs for fast transitions
    let num_blocks_in_epoch = 2;
    let mut config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);

    let signer = IrysSigner::random_signer(&config.consensus_config());
    let signer_address = signer.address();
    config.fund_genesis_accounts(vec![&signer]);

    // Create a different address for the new reward address
    let new_reward_signer = IrysSigner::random_signer(&config.consensus_config());
    let new_reward_address = new_reward_signer.address();

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("UPDATE_REWARD_ADDR_TEST", 10)
        .await;

    let block_tree_guard = &node.node_ctx.block_tree_guard;

    // Stake and mine to first epoch boundary
    let _stake_tx = post_stake_commitment(&node, &signer).await;
    node.mine_blocks(num_blocks_in_epoch).await?;

    // Initial reward_address equals signer address
    let epoch_snapshot = block_tree_guard.read().canonical_epoch_snapshot();
    let stake_entry = epoch_snapshot
        .commitment_state
        .stake_commitments
        .get(&signer_address)
        .expect("Signer should have stake in epoch snapshot");

    assert_eq!(
        stake_entry.reward_address, signer_address,
        "Initial reward_address should equal signer address"
    );

    // Submit UpdateRewardAddress and mine to include it
    let update_tx = node
        .post_update_reward_address(&signer, new_reward_address)
        .await?;

    // Status is Unknown until mined
    let status = node.get_commitment_snapshot_status(&update_tx);
    assert_eq!(status, CommitmentSnapshotStatus::Unknown);

    // Mine a block to include the commitment
    node.mine_blocks(1).await?;

    // Now status should be Accepted
    let status = node.get_commitment_snapshot_status(&update_tx);
    assert_eq!(status, CommitmentSnapshotStatus::Accepted);

    // Mine to next epoch boundary
    node.mine_blocks(num_blocks_in_epoch - 1).await?;

    // Verify reward_address is updated in epoch snapshot
    let epoch_snapshot = block_tree_guard.read().canonical_epoch_snapshot();
    let stake_entry = epoch_snapshot
        .commitment_state
        .stake_commitments
        .get(&signer_address)
        .expect("Signer should still have stake after update");

    assert_eq!(
        stake_entry.reward_address, new_reward_address,
        "reward_address should be updated after epoch boundary"
    );

    node.stop().await;
    Ok(())
}

#[test_log::test(tokio::test)]
async fn heavy_test_update_reward_address_without_stake_fails() -> eyre::Result<()> {
    initialize_tracing();

    let num_blocks_in_epoch = 2;
    let config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);

    // Create signer but DON'T stake
    let unstaked_signer = IrysSigner::random_signer(&config.consensus_config());
    let new_reward_address = IrysSigner::random_signer(&config.consensus_config()).address();

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("UPDATE_UNSTAKED_TEST", 10)
        .await;

    // Create UpdateRewardAddress tx for unstaked signer
    let consensus = &node.node_ctx.config.consensus;
    let anchor = node.get_anchor().await?;

    let mut update_tx = CommitmentTransaction::V2(irys_types::CommitmentV2WithMetadata {
        tx: CommitmentTransactionV2 {
            commitment_type: CommitmentTypeV2::UpdateRewardAddress { new_reward_address },
            anchor,
            fee: consensus.mempool.commitment_fee,
            value: U256::zero(),
            ..CommitmentTransactionV2::new(consensus)
        },
        metadata: Default::default(),
    });
    unstaked_signer.sign_commitment(&mut update_tx).unwrap();

    // Status should be Unstaked (no stake for signer)
    let status = node.get_commitment_snapshot_status(&update_tx);
    assert_eq!(status, CommitmentSnapshotStatus::Unstaked);

    // Posting should fail
    let result = node.post_commitment_tx(&update_tx).await;
    assert!(result.is_err());

    node.stop().await;
    Ok(())
}

/// Test that verifies fee-based ordering and winner selection for UpdateRewardAddress.
///
/// With multiple updates in the same block:
/// - They are ordered by fee ascending (lowest fee first, highest fee last)
/// - The highest-fee update wins and takes effect at epoch boundary
#[test_log::test(tokio::test)]
async fn heavy_test_multiple_update_reward_address() -> eyre::Result<()> {
    initialize_tracing();

    // Use 4 blocks per epoch to have room for testing within an epoch
    let num_blocks_in_epoch = 4;
    let mut config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);

    let signer = IrysSigner::random_signer(&config.consensus_config());
    let signer_address = signer.address();
    config.fund_genesis_accounts(vec![&signer]);

    let reward_address_low_fee = IrysSigner::random_signer(&config.consensus_config()).address();
    let reward_address_mid_fee = IrysSigner::random_signer(&config.consensus_config()).address();
    let reward_address_high_fee = IrysSigner::random_signer(&config.consensus_config()).address();

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("MULTI_UPDATE_TEST", 10)
        .await;

    let block_tree_guard = &node.node_ctx.block_tree_guard;
    let base_fee = node.node_ctx.config.consensus.mempool.commitment_fee;

    let _stake_tx = post_stake_commitment(&node, &signer).await;
    node.mine_until_next_epoch().await?;

    // Submit updates with different fees (out of order by fee)
    // Fees: high=300, low=100, mid=200
    info!("Submitting 3 UpdateRewardAddress txs with fees 300, 100, 200 (out of order)");

    let update_tx_high_fee = node
        .post_update_reward_address_with_fee(&signer, reward_address_high_fee, base_fee + 300)
        .await?;
    node.wait_for_mempool(update_tx_high_fee.id(), 5).await?;

    let update_tx_low_fee = node
        .post_update_reward_address_with_fee(&signer, reward_address_low_fee, base_fee + 100)
        .await?;
    node.wait_for_mempool(update_tx_low_fee.id(), 5).await?;

    let update_tx_mid_fee = node
        .post_update_reward_address_with_fee(&signer, reward_address_mid_fee, base_fee + 200)
        .await?;
    node.wait_for_mempool(update_tx_mid_fee.id(), 5).await?;

    let block = node.mine_block().await?;
    let commitment_tx_ids = block.commitment_tx_ids();

    // Verify fee-ascending order: idx 0=low fee, idx 1=mid fee, idx 2=high fee
    assert_eq!(commitment_tx_ids[0], update_tx_low_fee.id());
    assert_eq!(commitment_tx_ids[1], update_tx_mid_fee.id());
    assert_eq!(commitment_tx_ids[2], update_tx_high_fee.id());

    // Only the highest-fee update is stored in the snapshot (last one wins due to fee ordering)
    // Replaced txs return Unknown - they were valid but not the winner
    assert_eq!(
        node.get_commitment_snapshot_status(&update_tx_low_fee),
        CommitmentSnapshotStatus::Unknown
    );
    assert_eq!(
        node.get_commitment_snapshot_status(&update_tx_mid_fee),
        CommitmentSnapshotStatus::Unknown
    );
    assert_eq!(
        node.get_commitment_snapshot_status(&update_tx_high_fee),
        CommitmentSnapshotStatus::Accepted
    );

    node.mine_until_next_epoch().await?;

    // Verify highest-fee update wins at epoch boundary
    let epoch_snapshot = block_tree_guard.read().canonical_epoch_snapshot();
    let stake_entry = epoch_snapshot
        .commitment_state
        .stake_commitments
        .get(&signer_address)
        .expect("stake");
    assert_eq!(
        stake_entry.reward_address, reward_address_high_fee,
        "Highest-fee update should win at epoch boundary"
    );

    // New epoch: verify we can update again
    let reward_address_new_epoch = IrysSigner::random_signer(&config.consensus_config()).address();
    let update_tx_new_epoch = node
        .post_update_reward_address(&signer, reward_address_new_epoch)
        .await?;
    node.mine_blocks(1).await?;
    assert_eq!(
        node.get_commitment_snapshot_status(&update_tx_new_epoch),
        CommitmentSnapshotStatus::Accepted
    );

    node.mine_until_next_epoch().await?;

    let epoch_snapshot = block_tree_guard.read().canonical_epoch_snapshot();
    let stake_entry = epoch_snapshot
        .commitment_state
        .stake_commitments
        .get(&signer_address)
        .expect("stake");
    assert_eq!(stake_entry.reward_address, reward_address_new_epoch);

    node.stop().await;
    Ok(())
}

/// Test that verifies mining rewards (specifically TermFeeReward from ledger expiry)
/// go to the configured reward_address instead of the signer address when a custom
/// reward_address is set.
///
/// This test explicitly validates that TermFeeReward shadow transactions are emitted
/// and routed to the custom reward_address.
///
/// ## How This Test Works
/// 1. Start genesis node (which has stake + pledges + partitions already)
/// 2. Fund a separate user for posting data transactions
/// 3. Genesis updates its reward_address to a custom address
/// 4. Post data (which goes to genesis's partitions since genesis is the only miner)
/// 5. Mine to expiry epoch where Submit ledger expires
/// 6. Verify TermFeeReward goes to the custom reward_address (not genesis miner address)
#[test_log::test(tokio::test)]
async fn heavy_test_rewards_go_to_reward_address() -> eyre::Result<()> {
    initialize_tracing();

    // Configure with fast ledger expiry
    let num_blocks_in_epoch = 2;
    let submit_ledger_epoch_length: u64 = 1;
    let chunk_size = 32_u64;
    let num_chunks_in_partition = 10_u64;

    let mut config = NodeConfig::testing_with_epochs(num_blocks_in_epoch);
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().chunk_size = chunk_size;
    config.consensus.get_mut().num_chunks_in_partition = num_chunks_in_partition;
    config.consensus.get_mut().epoch.submit_ledger_epoch_length = submit_ledger_epoch_length;

    // Fund a separate user for posting data transactions
    let user_signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&user_signer]);

    let node = IrysNodeTest::new_genesis(config.clone())
        .start_and_wait_for_packing("REWARD_ADDR_TEST", 10)
        .await;

    // Genesis node owns all partitions
    let genesis_signer = node.cfg.signer();
    let genesis_address = genesis_signer.address();

    // Create a separate reward recipient address
    let reward_recipient = IrysSigner::random_signer(&config.consensus_config()).address();

    let block_tree_guard = &node.node_ctx.block_tree_guard;

    info!(
        "Genesis miner address: {}, Custom reward address: {}",
        genesis_address, reward_recipient
    );

    // Update reward_address to different address
    let _update_tx = node
        .post_update_reward_address(&genesis_signer, reward_recipient)
        .await?;
    node.wait_for_mempool(_update_tx.id(), 10).await?;
    node.mine_block().await?;

    info!("UpdateRewardAddress transaction included in block");

    // Post enough data to fill >1 slot (expiry logic skips active slot)
    let num_txs_to_post = (num_chunks_in_partition + 2) as usize;
    info!(
        "Posting {} data transactions to fill at least one complete slot",
        num_txs_to_post
    );

    let anchor = node.get_anchor().await?;
    let mut data_tx_ids = Vec::new();
    for i in 0..num_txs_to_post {
        let data = vec![42 + i as u8; chunk_size as usize];
        let data_tx = node.post_data_tx(anchor, data, &user_signer).await;
        node.wait_for_mempool(data_tx.header.id, 5).await?;
        data_tx_ids.push(data_tx.header.id);
    }
    node.mine_block().await?;

    info!("Data transactions included in block");

    // Get initial balances
    let head_block = node
        .get_block_by_height(node.get_canonical_chain_height().await)
        .await?;
    let reward_balance_before = node
        .get_balance(reward_recipient, head_block.evm_block_hash)
        .await;
    let genesis_balance_before = node
        .get_balance(genesis_address, head_block.evm_block_hash)
        .await;

    info!(
        "Initial balances - reward_recipient: {}, genesis: {}",
        reward_balance_before, genesis_balance_before
    );

    // Mine to epoch boundary (data expires, UpdateRewardAddress takes effect)
    let (_mined, expiry_height) = node.mine_until_next_epoch().await?;
    info!(
        "Reached epoch boundary at height {}, UpdateRewardAddress active and ledger expires",
        expiry_height
    );

    // Verify that reward_address is now updated in epoch snapshot
    let epoch_snapshot = block_tree_guard.read().canonical_epoch_snapshot();
    let stake_entry = epoch_snapshot
        .commitment_state
        .stake_commitments
        .get(&genesis_address)
        .expect("Genesis should have stake after epoch boundary");

    assert_eq!(
        stake_entry.reward_address, reward_recipient,
        "reward_address should be updated to reward_recipient after epoch boundary"
    );

    info!(
        "Verified reward_address is updated: genesis={}, reward_recipient={}",
        genesis_address, reward_recipient
    );

    // Verify TermFeeReward shadow transaction at expiry epoch block
    let expiry_block = node.get_block_by_height(expiry_height).await?;
    let expiry_evm_block = node
        .wait_for_evm_block(expiry_block.evm_block_hash, 20)
        .await?;
    let block_txs = expiry_evm_block.body.transactions;
    let block_tx_count = block_txs.len();
    info!(
        "Expiry block {} has {} EVM txs",
        expiry_height, block_tx_count
    );

    let mut found_term_fee_reward = false;
    for tx in block_txs {
        let mut input = tx.input().as_ref();
        if let Ok(shadow_tx) = ShadowTransaction::decode(&mut input) {
            if let Some(TransactionPacket::TermFeeReward(reward)) = shadow_tx.as_v1() {
                info!(
                    "Found TermFeeReward at height {}: target={}, amount={}",
                    expiry_height, reward.target, reward.amount
                );
                // KEY ASSERTION: TermFeeReward must go to reward_recipient
                assert_eq!(
                    reward.target,
                    reward_recipient.to_alloy_address(),
                    "TermFeeReward must go to custom reward_address, not genesis miner address"
                );
                found_term_fee_reward = true;
            }
        }
    }

    assert!(
        found_term_fee_reward,
        "Expected TermFeeReward at expiry epoch block height {} (evm_txs={})",
        expiry_height, block_tx_count
    );

    info!("TermFeeReward found and correctly sent to custom reward_address");

    // Verify rewards routing via balance changes
    let head_block = node
        .get_block_by_height(node.get_canonical_chain_height().await)
        .await?;
    let reward_balance_after = node
        .get_balance(reward_recipient, head_block.evm_block_hash)
        .await;
    let genesis_balance_after = node
        .get_balance(genesis_address, head_block.evm_block_hash)
        .await;

    info!(
        "Final balances - reward_recipient: {} (was {}), genesis: {} (was {})",
        reward_balance_after, reward_balance_before, genesis_balance_after, genesis_balance_before
    );

    // Verify the reward address configuration persists through block production
    let final_epoch_snapshot = block_tree_guard.read().canonical_epoch_snapshot();
    let final_stake_entry = final_epoch_snapshot
        .commitment_state
        .stake_commitments
        .get(&genesis_address)
        .expect("Genesis should still have stake at end of test");

    assert_eq!(
        final_stake_entry.reward_address, reward_recipient,
        "reward_address configuration should persist through mining operations"
    );

    info!("Test passed: TermFeeReward correctly routed to custom reward_address");

    node.stop().await;
    Ok(())
}
