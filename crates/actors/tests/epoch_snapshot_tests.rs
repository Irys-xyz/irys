use irys_actors::{
    block_producer::BlockProducerCommand,
    mining_bus::BroadcastPartitionsExpiration,
    partition_mining_service::{PartitionMiningService, PartitionMiningServiceInner},
    services::ServiceSenders,
};
use irys_config::StorageSubmodulesConfig;
use irys_database::{
    add_genesis_commitments, add_test_commitments, add_test_commitments_for_signer,
    db::IrysDatabaseExt as _,
};
use irys_domain::{BlockIndex, EpochBlockData, EpochSnapshot, StorageModule, StorageModuleVec};
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::PartitionChunkRange;
use irys_types::irys::IrysSigner;
use irys_types::{
    BlockTransactions, DataLedger, H256, IrysBlockHeader, SealedBlock,
    partition::PartitionAssignment,
};
use irys_types::{Config, U256};
use irys_types::{
    ConsensusConfig, ConsensusOptions, EpochConfig, PartitionChunkOffset, partition_chunk_offset_ie,
};
use irys_types::{H256List, NodeConfig};
use irys_vdf::state::{VdfState, VdfStateReadonly};
use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use std::{sync::atomic::AtomicU64, time::Duration};
use tracing::{debug, error};

#[tokio::test]
async fn genesis_test() {
    // setup temp dir
    let mut config = NodeConfig::testing();
    let tmp_dir = setup_tracing_and_temp_dir(None, false);
    let base_path = tmp_dir.path().to_path_buf();
    config.base_directory = base_path;
    let config: Config = Config::new_with_random_peer_id(config);

    // genesis block
    let mut genesis_block = IrysBlockHeader::new_mock_header();
    genesis_block.height = 0;
    let (commitments, initial_treasury) =
        add_genesis_commitments(&mut genesis_block, &config).await;
    genesis_block.treasury = initial_treasury;

    let storage_submodules_config =
        StorageSubmodulesConfig::load(config.node_config.base_directory.clone()).unwrap();

    let epoch_snapshot = EpochSnapshot::new(
        &storage_submodules_config,
        genesis_block.clone(),
        commitments,
        &config,
    );
    let miner_address = config.node_config.miner_address();

    // Process genesis message directly instead of through actor system
    // This allows us to inspect the actor's state after processing
    {
        // Verify the correct number of ledgers have been added
        let expected_ledger_count = DataLedger::ALL.len();
        assert_eq!(epoch_snapshot.ledgers.len(), expected_ledger_count);

        // Verify each ledger has one slot and the correct number of partitions
        let pub_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Publish);
        let sub_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Submit);

        assert_eq!(pub_slots.len(), 1);
        assert_eq!(sub_slots.len(), 1);

        assert_eq!(
            pub_slots[0].partitions.len() as u64,
            config.consensus.num_partitions_per_slot
        );
        assert_eq!(
            sub_slots[0].partitions.len() as u64,
            config.consensus.num_partitions_per_slot
        );

        // Verify data partition assignments match _PUBLISH_ ledger slots
        for (slot_idx, slot) in pub_slots.iter().enumerate() {
            let pa = &epoch_snapshot.partition_assignments;
            for &partition_hash in &slot.partitions {
                let assignment = pa
                    .data_partitions
                    .get(&partition_hash)
                    .expect("partition should be assigned");

                assert_eq!(
                    assignment,
                    &PartitionAssignment {
                        partition_hash,
                        ledger_id: Some(DataLedger::Publish.into()),
                        slot_index: Some(slot_idx),
                        miner_address,
                    }
                );
            }
            assert_eq!(
                slot.partitions.len(),
                config.consensus.num_partitions_per_slot as usize
            );
        }

        // Verify data partition assignments match _SUBMIT_ledger slots
        for (slot_idx, slot) in sub_slots.iter().enumerate() {
            let pa = &epoch_snapshot.partition_assignments;
            for &partition_hash in &slot.partitions {
                let assignment = pa
                    .data_partitions
                    .get(&partition_hash)
                    .expect("partition should be assigned");

                assert_eq!(
                    assignment,
                    &PartitionAssignment {
                        partition_hash,
                        ledger_id: Some(DataLedger::Submit.into()),
                        slot_index: Some(slot_idx),
                        miner_address,
                    }
                );
            }
            assert_eq!(
                slot.partitions.len(),
                config.consensus.num_partitions_per_slot as usize
            );
        }
    }

    // Verify the correct number of genesis partitions have been activated
    {
        let pa = &epoch_snapshot.partition_assignments;
        let data_partition_count = pa.data_partitions.len() as u64;
        let expected_partitions = data_partition_count
            + EpochSnapshot::get_num_capacity_partitions(data_partition_count, &config.consensus);
        assert_eq!(
            epoch_snapshot.all_active_partitions.len(),
            expected_partitions as usize
        );

        // Validate that all the capacity partitions are assigned to the
        // bootstrap miner but not assigned to any ledger
        for pair in &pa.capacity_partitions {
            let partition_hash = pair.0;
            let ass = pair.1;
            assert_eq!(
                ass,
                &PartitionAssignment {
                    partition_hash: *partition_hash,
                    ledger_id: None,
                    slot_index: None,
                    miner_address
                }
            )
        }
    }

    // Debug output for verification
    // println!("Data Partitions: {:#?}", epoch_service.capacity_partitions);
    println!("Ledger State: {:#?}", epoch_snapshot.ledgers);

    // let infos = epoch_service.get_genesis_storage_module_infos();
    // println!("{:#?}", infos);
}

#[tokio::test]
async fn add_slots_test() {
    let tmp_dir = setup_tracing_and_temp_dir(Some("add_slots_test"), false);
    let base_path = tmp_dir.path().to_path_buf();
    let mut genesis_block = IrysBlockHeader::new_mock_header();
    let consensus_config = ConsensusConfig {
        chunk_size: 32,
        num_chunks_in_partition: 10,
        num_chunks_in_recall_range: 2,
        num_partitions_per_slot: 1,
        block_migration_depth: 1, // Testnet / single node config
        chain_id: 333,
        epoch: EpochConfig {
            capacity_scalar: 100,
            num_blocks_in_epoch: 100,
            num_capacity_partitions: Some(123),
            submit_ledger_epoch_length: 5,
        },
        ..ConsensusConfig::testing()
    };
    let mut testing_config = NodeConfig::testing();
    testing_config.base_directory = base_path;
    testing_config.consensus = ConsensusOptions::Custom(consensus_config);
    let config = Config::new_with_random_peer_id(testing_config);
    genesis_block.height = 0;
    let num_blocks_in_epoch = config.consensus.epoch.num_blocks_in_epoch;
    let num_chunks_in_partition = config.consensus.num_chunks_in_partition;
    let (commitments, initial_treasury) =
        add_genesis_commitments(&mut genesis_block, &config).await;
    genesis_block.treasury = initial_treasury;

    let storage_submodules_config =
        StorageSubmodulesConfig::load(config.node_config.base_directory.clone()).unwrap();

    let mut epoch_snapshot = EpochSnapshot::new(
        &storage_submodules_config,
        genesis_block.clone(),
        commitments,
        &config,
    );

    let mut mock_header = IrysBlockHeader::new_mock_header();
    mock_header.data_ledgers[DataLedger::Submit].total_chunks = 0;

    // Now create a new epoch block & give the Submit ledger enough size to add one slot
    let mut new_epoch_block = mock_header.clone();
    new_epoch_block.height = num_blocks_in_epoch;
    new_epoch_block.data_ledgers[DataLedger::Submit].total_chunks = num_chunks_in_partition / 2;

    // Post the new epoch block to the service and let it perform_epoch_tasks()
    let _ = epoch_snapshot.perform_epoch_tasks(
        &Some(genesis_block.clone()),
        &new_epoch_block,
        Vec::new(),
    );

    debug!("{:#?}", epoch_snapshot.ledgers);

    // Verify each ledger has one slot and the correct number of partitions
    {
        let pub_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Publish);
        let sub_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Submit);
        assert_eq!(pub_slots.len(), 1);
        assert_eq!(sub_slots.len(), 3); // TODO: check 1 expired, 2 new slots added
    }

    let previous_epoch_block = Some(new_epoch_block.clone());

    // Simulate a subsequent epoch block that adds multiple ledger slots
    let mut new_epoch_block = IrysBlockHeader::new_mock_header();
    new_epoch_block.height = num_blocks_in_epoch * 2;

    // Increase the Submit ledger by 3 slots  and the Publish ledger by 2 slots
    new_epoch_block.data_ledgers[DataLedger::Submit].total_chunks =
        (num_chunks_in_partition as f64 * 2.5) as u64;
    new_epoch_block.data_ledgers[DataLedger::Publish as usize].total_chunks =
        (num_chunks_in_partition as f64 * 0.75) as u64;

    let _ = epoch_snapshot.perform_epoch_tasks(&previous_epoch_block, &new_epoch_block, Vec::new());

    debug!("{:#?}", epoch_snapshot.ledgers);

    // Validate the correct number of ledgers slots were added to each ledger
    let pub_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Publish);
    let sub_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Submit);
    assert_eq!(pub_slots.len(), 3);
    assert_eq!(sub_slots.len(), 7);
    println!("Ledger State: {:#?}", epoch_snapshot.ledgers);
}

#[tokio::test]
async fn unique_addresses_per_slot_test() {
    // SAFETY: test code; env var set before other threads spawn.
    unsafe { std::env::set_var("RUST_LOG", "debug") };

    let tmp_dir = setup_tracing_and_temp_dir(Some("unique_addresses_per_slot_test"), false);
    let base_path = tmp_dir.path().to_path_buf();
    let mut genesis_block = IrysBlockHeader::new_mock_header();
    let consensus_config = ConsensusConfig {
        chunk_size: 32,
        num_chunks_in_partition: 10,
        num_chunks_in_recall_range: 2,
        num_partitions_per_slot: 10, // <- relevant to this test
        block_migration_depth: 1,    // Testnet / single node config
        chain_id: 333,
        epoch: EpochConfig {
            capacity_scalar: 100,
            num_blocks_in_epoch: 100,
            num_capacity_partitions: Some(123),
            submit_ledger_epoch_length: 5,
        },
        ..ConsensusConfig::testing()
    };
    let mut testing_config = NodeConfig::testing();
    testing_config.base_directory = base_path;
    testing_config.consensus = ConsensusOptions::Custom(consensus_config);
    let config = Config::new_with_random_peer_id(testing_config);
    let genesis_signer = config.irys_signer();
    genesis_block.height = 0;
    let (mut commitments, _) = add_genesis_commitments(&mut genesis_block, &config).await;

    // Create some other signers to simulate other pledged and staked addresses
    let signer1 = IrysSigner::random_signer(&config.consensus);
    let signer2 = IrysSigner::random_signer(&config.consensus);

    // Give them both 10 pledged partitions
    let (mut comm1, _) =
        add_test_commitments_for_signer(&mut genesis_block, &signer1, 10, &config).await;
    let (mut comm2, _) =
        add_test_commitments_for_signer(&mut genesis_block, &signer2, 10, &config).await;

    commitments.append(&mut comm1);
    commitments.append(&mut comm2);

    let storage_submodules_config =
        StorageSubmodulesConfig::load(config.node_config.base_directory.clone()).unwrap();

    let epoch_snapshot = EpochSnapshot::new(
        &storage_submodules_config,
        genesis_block.clone(),
        commitments,
        &config,
    );

    debug!("{:#?}", epoch_snapshot.ledgers);

    let publish_slot = &epoch_snapshot.ledgers.get_slots(DataLedger::Publish)[0];
    let submit_slot = &epoch_snapshot.ledgers.get_slots(DataLedger::Submit)[0];

    // Publish and submit slots should have exactly 3 partitions (not 10) because that's
    // how many uniquely staked addresses there are.
    assert_eq!(publish_slot.partitions.len(), 3);
    assert_eq!(submit_slot.partitions.len(), 3);

    // Collect mining addresses from publish_slot partitions
    let publish_mining_addresses: Vec<_> = publish_slot
        .partitions
        .iter()
        .map(|partition_hash| {
            epoch_snapshot
                .get_data_partition_assignment(*partition_hash)
                .unwrap()
                .miner_address
        })
        .collect();

    // Convert to HashSet for easier assertion (assuming you have 3 expected addresses)
    let publish_addresses_set: HashSet<_> = publish_mining_addresses.iter().collect();

    // Assert that all 3 expected addresses are in the collected addresses
    assert!(publish_addresses_set.contains(&genesis_signer.address()));
    assert!(publish_addresses_set.contains(&signer1.address()));
    assert!(publish_addresses_set.contains(&signer2.address()));

    // Collect mining addresses from submit_slot partitions
    let submit_mining_addresses: Vec<_> = submit_slot
        .partitions
        .iter()
        .map(|partition_hash| {
            epoch_snapshot
                .get_data_partition_assignment(*partition_hash)
                .unwrap()
                .miner_address
        })
        .collect();

    // Convert to HashSet for easier assertion (assuming you have 3 expected addresses)
    let submit_addresses_set: HashSet<_> = submit_mining_addresses.iter().collect();

    // Assert that all 3 expected addresses are in the collected addresses
    assert!(submit_addresses_set.contains(&genesis_signer.address()));
    assert!(submit_addresses_set.contains(&signer1.address()));
    assert!(submit_addresses_set.contains(&signer2.address()));
}

#[tokio::test]
async fn capacity_projection_tests() {
    let max_data_parts = 1000;
    let config = ConsensusConfig::testing();
    for i in (0..max_data_parts).step_by(10) {
        let data_partition_count = i;
        let capacity_count =
            EpochSnapshot::get_num_capacity_partitions(data_partition_count, &config);
        let total = data_partition_count + capacity_count;
        println!(
            "data:{}, capacity:{}, total:{}",
            data_partition_count, capacity_count, total
        );
    }
}

#[tokio::test]
/*
Summary:
Verify that when a Submit ledger slot expires at an epoch boundary,
miners reset their storage modules and a repacking request is enqueued
for the full partition, and that ledger slot/partition assignments are
updated consistently.

High-level steps:
1) Configure a minimal consensus and build an EpochSnapshot with initial
   Publish/Submit slots and capacity partitions. Map local
   StorageModuleInfos to StorageModule instances for the miner.
2) Spawn a Tokio PartitionMiningService per storage module.
   Subscribe to the MiningBus via ServiceSenders; publish events
   using ServiceSenders send_* helpers. No Actix registry is involved.
3) Wire a packing channel in ServiceSenders and forward the first
   PackingRequest into a oneshot receiver for assertions.
4) Drive epoch transitions by repeatedly calling perform_epoch_tasks
   with synthetic epoch blocks (tweaking Submit ledger chunk totals as
   needed). In each iteration, broadcast any expired partitions reported
   by the snapshot to the miners.
5) Assert:
   - A PackingRequest arrives targeting the Submit partition and the
     full partition range.
   - Submit slot 0 is marked expired and emptied; new Submit slots are
     added and assigned; Publish still has one slot.
   - Partition assignments for Publish and Submit slots are internally
     consistent with the ledger state.
*/
async fn partition_expiration_and_repacking_test() {
    let tmp_dir = setup_tracing_and_temp_dir(Some("partition_expiration_test"), false);
    let base_path = tmp_dir.path().to_path_buf();
    let chunk_size = 32;
    let chunk_count = 10;
    let consensus_config = ConsensusConfig {
        chunk_size,
        num_chunks_in_partition: chunk_count,
        num_chunks_in_recall_range: 2,
        num_partitions_per_slot: 1,
        block_migration_depth: 1,
        epoch: EpochConfig {
            capacity_scalar: 100,
            submit_ledger_epoch_length: 2,
            num_blocks_in_epoch: 5,
            num_capacity_partitions: Some(123),
        },
        ..ConsensusConfig::testing()
    };
    let mut config = NodeConfig::testing();
    config.base_directory = base_path.clone();
    config.consensus = ConsensusOptions::Custom(consensus_config);
    let config = Config::new_with_random_peer_id(config);

    let mut genesis_block = IrysBlockHeader::new_mock_header();
    genesis_block.height = 0;
    let (commitments, initial_treasury) =
        add_test_commitments(&mut genesis_block, 5, &config).await;
    genesis_block.treasury = initial_treasury;

    // Create a storage config for testing
    let num_blocks_in_epoch = config.consensus.epoch.num_blocks_in_epoch;
    let num_chunks_in_partition = config.consensus.num_chunks_in_partition;

    // Create epoch service
    let storage_submodules_config = StorageSubmodulesConfig::load(base_path.clone()).unwrap();

    let mut epoch_snapshot = EpochSnapshot::new(
        &storage_submodules_config,
        genesis_block.clone(),
        commitments,
        &config,
    );
    let storage_module_infos = epoch_snapshot.map_storage_modules_to_partition_assignments();
    let mut storage_modules: StorageModuleVec = Vec::new();
    // Create a list of storage modules wrapping the storage files
    for info in storage_module_infos {
        let arc_module = Arc::new(
            StorageModule::new(&info, &config)
                // TODO: remove this unwrap
                .unwrap(),
        );
        storage_modules.push(arc_module.clone());
    }

    // Wire a Tokio packing handle and capture first packing request
    let (tx_packing, mut rx_packing) =
        tokio::sync::mpsc::channel::<irys_actors::packing_service::PackingRequest>(1);
    let (pack_req_tx, pack_req_rx) =
        tokio::sync::oneshot::channel::<irys_actors::packing_service::PackingRequest>();

    tokio::spawn(async move {
        if let Some(packing_req) = rx_packing.recv().await {
            pack_req_tx.send(packing_req).expect("pack_req_rx dropped");
        }
    });

    // Wire a Tokio unpacking handle (for test compatibility)
    let (tx_unpacking, mut rx_unpacking) =
        tokio::sync::mpsc::channel::<irys_actors::packing_service::UnpackingRequest>(1);
    tokio::spawn(async move {
        while let Some(_unpacking_req) = rx_unpacking.recv().await {
            // No-op for test
        }
    });

    // Create ServiceSenders for testing (channel-first)
    let (service_senders, mut receivers) =
        ServiceSenders::new_with_packing_sender(tx_packing.clone(), tx_unpacking);

    // Spawn a task to handle block producer commands
    tokio::spawn(async move {
        while let Some(traced_cmd) = receivers.block_producer.recv().await {
            let (cmd, _parent_span) = traced_cmd.into_parts();
            if let BlockProducerCommand::SolutionFound { response, .. } = cmd {
                // Return Ok(None) for the test
                response
                    .send(Ok(None))
                    .expect("block producer response receiver dropped");
            }
        }
    });

    let vdf_steps_guard = VdfStateReadonly::new(Arc::new(RwLock::new(VdfState::new(10, 0, None))));

    let mut mining_service_handles = Vec::new();
    let mut mining_service_controllers = Vec::new();

    let atomic_global_step_number = Arc::new(AtomicU64::new(0));

    let should_mine = true;
    for sm in &storage_modules {
        let inner = PartitionMiningServiceInner::new(
            &config,
            service_senders.clone(),
            sm.clone(),
            should_mine,
            vdf_steps_guard.clone(),
            atomic_global_step_number.clone(),
            U256::zero(),
        );

        let (controller, handle) =
            PartitionMiningService::spawn_service(inner, tokio::runtime::Handle::current());
        debug!("starting miner partition hash {:?}", sm.partition_hash());
        mining_service_controllers.push(controller);
        mining_service_handles.push(handle);
    }

    let assign_submit_partition_hash = {
        epoch_snapshot
            .partition_assignments
            .data_partitions
            .iter()
            .find(|(_hash, assignment)| assignment.ledger_id == Some(DataLedger::Submit.get_id()))
            .map(|(hash, _)| *hash)
            .expect("There should be a partition assigned to submit ledger")
    };

    let (publish_partition_hash, submit_partition_hash) = {
        let pub_slots = epoch_snapshot
            .ledgers
            .get_slots(DataLedger::Publish)
            .clone();
        let sub_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Submit).clone();
        assert_eq!(pub_slots.len(), 1);
        assert_eq!(sub_slots.len(), 1);

        (pub_slots[0].partitions[0], sub_slots[0].partitions[0])
    };

    assert_eq!(assign_submit_partition_hash, submit_partition_hash);

    let capacity_partitions = {
        let capacity_partitions: Vec<H256> = epoch_snapshot
            .partition_assignments
            .capacity_partitions
            .keys()
            .copied()
            .collect();

        assert!(
            !capacity_partitions.contains(&publish_partition_hash),
            "Publish partition should not be in capacity partitions"
        );

        assert!(
            !capacity_partitions.contains(&submit_partition_hash),
            "Submit partition should not be in capacity partitions"
        );

        capacity_partitions
    };

    // Simulate enough epoch blocks to compete a Submit ledger storage term, expiring a slot
    let mut new_epoch_block = IrysBlockHeader::new_mock_header();
    let mut previous_epoch_block = Some(genesis_block.clone());
    for i in 0..config.consensus.epoch.submit_ledger_epoch_length + 4 {
        new_epoch_block.height = num_blocks_in_epoch + num_blocks_in_epoch * i;

        if i == 3 {
            new_epoch_block.data_ledgers[DataLedger::Submit].total_chunks =
                num_chunks_in_partition / 3;
        }

        if i == 5 {
            new_epoch_block.data_ledgers[DataLedger::Submit].total_chunks =
                num_chunks_in_partition / 2;
        }

        debug!(
            "Epoch Block: Submit.max_chunk_offset = {}",
            new_epoch_block.data_ledgers[DataLedger::Submit].total_chunks
        );

        let result =
            epoch_snapshot.perform_epoch_tasks(&previous_epoch_block, &new_epoch_block, Vec::new());

        if let Err(err) = result {
            error!("Error processing NewEpochMessage: {:?}", err);
            panic!("Test failed: {:?}", err);
        }

        // Simulate the partition expiry broadcast the service would normally do
        let expired_partition_hashes: Vec<_> = epoch_snapshot
            .expired_partition_infos
            .as_ref()
            .map_or(Vec::new(), |infos| {
                infos.iter().map(|info| info.partition_hash).collect()
            });

        service_senders.send_partitions_expiration(BroadcastPartitionsExpiration(H256List(
            expired_partition_hashes,
        )));

        previous_epoch_block = Some(new_epoch_block.clone());

        debug!("{:#?}", epoch_snapshot.ledgers);
    }

    // wait for packing request with a short timeout instead of busy-polling
    let pack_req = tokio::time::timeout(Duration::from_secs(3), pack_req_rx)
        .await
        .expect("timed out waiting for packing request")
        .expect("packing request sender dropped");

    // check a new slots is inserted with a partition assigned to it, and slot 0 expired and its partition was removed
    let (publish_partition, submit_partition, submit_partition2) = {
        let pub_slots = epoch_snapshot
            .ledgers
            .get_slots(DataLedger::Publish)
            .clone();
        let sub_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Submit).clone();
        assert_eq!(
            pub_slots.len(),
            1,
            "Publish should still have only one slot"
        );
        debug!("Ledger State: {:#?}", epoch_snapshot.ledgers);

        assert_eq!(
            sub_slots.len(),
            3,
            "Submit slots should have two new not expired slots with a new fresh partition from available previous capacity ones!"
        );
        assert!(
            sub_slots[0].is_expired && sub_slots[0].partitions.is_empty(),
            "Slot 0 should have expired and have no assigned partition!"
        );

        assert!(
            !sub_slots[1].is_expired
                && sub_slots[1].partitions.len() == 1
                && (capacity_partitions.contains(&sub_slots[1].partitions[0])
                    || submit_partition_hash == sub_slots[1].partitions[0]),
            "Slot 1 should not be expired and have a capacity or the just expired partition"
        );
        assert!(
            !sub_slots[2].is_expired
                && sub_slots[2].partitions.len() == 1
                && (capacity_partitions.contains(&sub_slots[2].partitions[0])
                    || submit_partition_hash == sub_slots[2].partitions[0]),
            "Slot 2 should not be expired and have a capacity or the just expired partition"
        );

        println!("{}", serde_json::to_string_pretty(&sub_slots).unwrap());

        let publish_partition = *pub_slots[0]
            .partitions
            .first()
            .expect("publish ledger slot 0 should have a partition assigned");
        let submit_partition = *sub_slots[1]
            .partitions
            .first()
            .expect("submit ledger slot 1 should have a partition assigned");
        let submit_partition2 = *sub_slots[2]
            .partitions
            .first()
            .expect("submit ledger slot 2 should have a partition assigned");

        (publish_partition, submit_partition, submit_partition2)
    };

    // check repacking request expired partition for its whole interval range, and partitions assignments are consistent
    {
        assert_eq!(
            epoch_snapshot.partition_assignments.data_partitions.len(),
            3,
            "Should have four partitions assignments"
        );

        if let Some(publish_assignment) = epoch_snapshot
            .partition_assignments
            .data_partitions
            .get(&publish_partition)
        {
            assert_eq!(
                publish_assignment.ledger_id,
                Some(DataLedger::Publish.get_id()),
                "Should be assigned to publish ledger"
            );
            assert_eq!(
                publish_assignment.slot_index,
                Some(0),
                "Should be assigned to slot 0"
            );
        } else {
            panic!("Should have an assignment");
        };

        if let Some(submit_assignment) = epoch_snapshot
            .partition_assignments
            .data_partitions
            .get(&submit_partition)
        {
            assert_eq!(
                submit_assignment.ledger_id,
                Some(DataLedger::Submit.get_id()),
                "Should be assigned to submit ledger"
            );
            assert_eq!(
                submit_assignment.slot_index,
                Some(1),
                "Should be assigned to slot 1"
            );
        } else {
            panic!("Should have an assignment");
        };

        if let Some(submit_assignment) = epoch_snapshot
            .partition_assignments
            .data_partitions
            .get(&submit_partition2)
        {
            assert_eq!(
                submit_assignment.ledger_id,
                Some(DataLedger::Submit.get_id()),
                "Should be assigned to submit ledger"
            );
            assert_eq!(
                submit_assignment.slot_index,
                Some(2),
                "Should be assigned to slot 2"
            );
        } else {
            panic!("Should have an assignment");
        };
    }

    assert_eq!(
        pack_req.storage_module().partition_hash(),
        Some(submit_partition_hash),
        "Partition hashes should be equal"
    );
    assert_eq!(
        *pack_req.chunk_range(),
        PartitionChunkRange(partition_chunk_offset_ie!(0, chunk_count as u32)),
        "The whole partition should be repacked"
    );
}

#[tokio::test]
async fn epoch_blocks_reinitialization_test() {
    let tmp_dir = setup_tracing_and_temp_dir(Some("epoch_block_reinitialization_test"), false);
    let base_path = tmp_dir.path().to_path_buf();
    let chunk_size = 32;
    let consensus_config = ConsensusConfig {
        chunk_size,
        ..ConsensusConfig::testing()
    };
    let mut config = NodeConfig::testing();
    config.base_directory = base_path.clone();
    config.consensus = ConsensusOptions::Custom(consensus_config);
    let config = Config::new_with_random_peer_id(config);
    let num_chunks_in_partition = config.consensus.num_chunks_in_partition;
    let num_blocks_in_epoch = config.consensus.epoch.num_blocks_in_epoch;

    let db_env =
        irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db(&base_path)
            .expect("to create DB");
    let db = irys_types::DatabaseProvider(std::sync::Arc::new(db_env));
    let block_index = BlockIndex::new_for_testing(db);

    // Initialize genesis block at height 0
    let mut genesis_block = IrysBlockHeader::new_mock_header();
    genesis_block.height = 0;
    let pledge_count = config.consensus.epoch.num_capacity_partitions.unwrap_or(31) as u8;
    let (commitments, initial_treasury) =
        add_test_commitments(&mut genesis_block, pledge_count, &config).await;
    genesis_block.treasury = initial_treasury;

    let storage_submodules_config =
        StorageSubmodulesConfig::load(config.node_config.base_directory.clone()).unwrap();

    let mut epoch_snapshot = EpochSnapshot::new(
        &storage_submodules_config,
        genesis_block.clone(),
        commitments.clone(),
        &config,
    );

    // Get the genesis storage modules and their assigned partitions
    let storage_module_infos = epoch_snapshot.map_storage_modules_to_partition_assignments();
    debug!("{:#?}", storage_module_infos);

    genesis_block.block_hash = H256::from_slice(&[0; 32]);

    let genesis_sealed = SealedBlock::new_unchecked(
        Arc::new(genesis_block.clone()),
        BlockTransactions::default(),
    );
    block_index
        .db()
        .update_eyre(|tx| BlockIndex::push_block(tx, &genesis_sealed, config.consensus.chunk_size))
        .expect("Failed to index genesis block");

    {
        let mut storage_modules: StorageModuleVec = Vec::new();

        // Create a list of storage modules wrapping the storage files
        for info in storage_module_infos {
            let arc_module = Arc::new(StorageModule::new(&info, &config).unwrap());
            storage_modules.push(arc_module.clone());
        }
    }

    //         +---+
    //         |sm0|
    //         +-+-+  |    |
    // Publish 0----+----+----+---
    //           |    |    |
    //           0    1    2
    //         +---+
    //         |sm1|
    //         +-+-+  |    |
    // Submit  1----+----+----+---
    //           |    |    |
    //           0    1    2
    // Capacity +---+
    //          |sm2|
    //          +-+-+

    // Now create a new epoch block & give the Submit ledger enough size to add a slot
    let mut new_epoch_block = IrysBlockHeader::new_mock_header();
    new_epoch_block.data_ledgers[DataLedger::Submit].total_chunks = 0;

    let mut epoch_block_data: Vec<EpochBlockData> = Vec::new();
    let epochs_in_term = config.consensus.epoch.submit_ledger_epoch_length;
    let mut previous_epoch_block = Some(genesis_block.clone());

    for i in 0..=epochs_in_term {
        // Simulate blocks up to one before the next epoch boundary
        let next_epoch_height = num_blocks_in_epoch * (i + 1);

        // For the second to last epoch block in the term, have it resize the submit ledger
        if i == epochs_in_term - 1 {
            new_epoch_block.data_ledgers[DataLedger::Submit].total_chunks =
                num_chunks_in_partition / 2;
        }

        // Send the epoch message
        new_epoch_block.height = next_epoch_height;

        let result =
            epoch_snapshot.perform_epoch_tasks(&previous_epoch_block, &new_epoch_block, Vec::new());

        if let Err(err) = result {
            error!("Error processing NewEpochMessage: {:?}", err);
            panic!("Test failed: {:?}", err);
        }

        epoch_block_data.push(EpochBlockData {
            epoch_block: new_epoch_block.clone(),
            commitments: Vec::new(),
        });
        previous_epoch_block = Some(new_epoch_block.clone());
    }

    // Verify each ledger has one slot and the correct number of partitions
    {
        debug!("{:#?}", epoch_snapshot.ledgers);
        let pub_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Publish);
        let sub_slots = epoch_snapshot.ledgers.get_slots(DataLedger::Submit);
        assert_eq!(pub_slots.len(), 1);
        assert_eq!(sub_slots.len(), 3); // TODO: check slot 1 expired, 2 new slots added
    }

    //            +---+
    //            |sm0|
    //            +-|-+  |    |
    // Publish 0----+----+----+---
    //              |    |    |
    //              0    1    2
    //                +---+ +---+
    //                |sm2| |sm1|
    //                +-|-+ +-|-+
    // Submit 1----+----+-----+---
    //             |    |     |
    //             0    1     2
    // Capacity

    // partitions_guard.read().print_assignments();

    let block_index_guard =
        irys_domain::block_index_guard::BlockIndexReadGuard::new(block_index.clone());

    debug!(
        "num blocks in block_index: {}",
        block_index_guard.read().num_blocks()
    );

    // Get the genesis storage modules and their assigned partitions
    let mut epoch_service = EpochSnapshot::new(
        &storage_submodules_config,
        genesis_block,
        commitments,
        &config,
    );
    let storage_module_infos = epoch_service.map_storage_modules_to_partition_assignments();
    epoch_service
        .replay_epoch_data(epoch_block_data)
        .expect("to replay the epoch data");

    debug!("{:#?}", storage_module_infos);

    let new_sm_infos = epoch_service.map_storage_modules_to_partition_assignments();

    debug!("{:#?}", new_sm_infos);

    // Check partition hashes have not changed in storage modules
    {
        let mut storage_modules: StorageModuleVec = Vec::new();

        // Create a list of storage modules wrapping the storage files
        for info in storage_module_infos {
            let arc_module = Arc::new(StorageModule::new(&info, &config).unwrap());
            storage_modules.push(arc_module.clone());
        }
    }
}

#[tokio::test]
async fn partitions_assignment_determinism_test() {
    // SAFETY: test code; env var set before other threads spawn.
    unsafe { std::env::set_var("RUST_LOG", "debug") };
    let tmp_dir = setup_tracing_and_temp_dir(Some("partitions_assignment_determinism_test"), false);
    let base_path = tmp_dir.path().to_path_buf();
    let chunk_size = 32;
    let consensus_config = ConsensusConfig {
        chunk_size,
        num_chunks_in_partition: 10,
        num_chunks_in_recall_range: 2,
        num_partitions_per_slot: 1,
        block_migration_depth: 1, // Testnet / single node config
        chain_id: 1,
        epoch: EpochConfig {
            capacity_scalar: 100,
            num_blocks_in_epoch: 100,
            submit_ledger_epoch_length: 2,
            num_capacity_partitions: None,
        },
        ..ConsensusConfig::testing()
    };
    let mut config = NodeConfig::testing();
    config.storage.num_writes_before_sync = 1;
    config.base_directory = base_path.clone();
    config.consensus = ConsensusOptions::Custom(consensus_config);
    let config = Config::new_with_random_peer_id(config);
    let num_chunks_in_partition = config.consensus.num_chunks_in_partition;
    let num_blocks_in_epoch = config.consensus.epoch.num_blocks_in_epoch;

    // Initialize genesis block at height 0
    let mut genesis_block = IrysBlockHeader::new_mock_header();
    genesis_block.last_epoch_hash = H256::zero(); // for partitions hash determinism
    genesis_block.height = 0;
    let pledge_count = 20;
    let (commitments, initial_treasury) =
        add_test_commitments(&mut genesis_block, pledge_count, &config).await;
    genesis_block.treasury = initial_treasury;

    let storage_submodules_config = StorageSubmodulesConfig::load_for_test(base_path, 40).unwrap();

    let mut epoch_snapshot = EpochSnapshot::new(
        &storage_submodules_config,
        genesis_block.clone(),
        commitments,
        &config,
    );

    epoch_snapshot.partition_assignments.print_assignments();

    // Because we aren't actually building blocks with block_producer we need to
    // stub out some block hashes, for this tests we use the capacity partition hashes
    // as the source because it doubles down on determinism.
    let test_hashes: Vec<_> = epoch_snapshot
        .partition_assignments
        .capacity_partitions
        .keys()
        .copied()
        .collect();

    // Now create a new epoch block & give the Submit ledger enough size to add a slot
    let total_epoch_messages = 6;
    let mut epoch_num = 1;
    let mut new_epoch_block = IrysBlockHeader::new_mock_header();
    new_epoch_block.data_ledgers[DataLedger::Submit].total_chunks = num_chunks_in_partition;
    new_epoch_block.data_ledgers[DataLedger::Publish].total_chunks = num_chunks_in_partition;

    let mut previous_epoch_block = genesis_block.clone();

    while epoch_num <= total_epoch_messages {
        new_epoch_block.block_hash = test_hashes[epoch_num as usize]; // Pick a stable block hash from our test hashes list
        new_epoch_block.last_epoch_hash = previous_epoch_block.block_hash; // Make sure the new epoch block has the hash of the old one
        new_epoch_block.height = epoch_num * num_blocks_in_epoch; // Give the new block a valid epoch height

        epoch_num += 1;
        debug!("epoch block {}", new_epoch_block.height);

        let _ = epoch_snapshot.perform_epoch_tasks(
            &Some(previous_epoch_block),
            &new_epoch_block,
            Vec::new(),
        );
        previous_epoch_block = new_epoch_block.clone();
    }

    epoch_snapshot.partition_assignments.print_assignments();

    debug!(
        "\nAll Partitions({})\n{}",
        &epoch_snapshot.all_active_partitions.len(),
        serde_json::to_string_pretty(&epoch_snapshot.all_active_partitions).unwrap()
    );

    // Check determinism in assigned partitions
    let publish_slot_0 = H256::from_base58("2F5eg8FE2VmXGcgpyUKTzBrLzSmVXMKqawUJeDgKC1vW");
    debug!("expected publish[0] -> {}", publish_slot_0);

    if let Some(publish_assignment) = epoch_snapshot
        .partition_assignments
        .data_partitions
        .get(&publish_slot_0)
    {
        assert_eq!(
            publish_assignment.ledger_id,
            Some(DataLedger::Publish.get_id()),
            "Should be assigned to publish ledger"
        );
        assert_eq!(
            publish_assignment.slot_index,
            Some(0),
            "Should be assigned to slot 0"
        );
    } else {
        panic!("Should have an assignment");
    };

    let publish_slot_1 = H256::from_base58("3FQ7mPXwHhmNVD74DqNATHCex4wrtSQEBaa1FYeBEneN");

    epoch_snapshot.partition_assignments.print_assignments();

    debug!("expected publish[1] -> {}", publish_slot_1);

    if let Some(publish_assignment) = epoch_snapshot
        .partition_assignments
        .data_partitions
        .get(&publish_slot_1)
    {
        assert_eq!(
            publish_assignment.ledger_id,
            Some(DataLedger::Publish.get_id()),
            "Should be assigned to publish ledger"
        );
        assert_eq!(
            publish_assignment.slot_index,
            Some(1),
            "Should be assigned to slot 1"
        );
    } else {
        panic!("Should have an assignment");
    };

    let capacity_partition = H256::from_base58("5Wvv6erYhpk9aAzdrS9i6noQf57dBXHgLaMz46mNZeds");

    if let Some(capacity_assignment) = epoch_snapshot
        .partition_assignments
        .capacity_partitions
        .get(&capacity_partition)
    {
        assert_eq!(
            capacity_assignment.ledger_id, None,
            "Should not be assigned to data ledger"
        );
        assert_eq!(
            capacity_assignment.slot_index, None,
            "Should not be assigned a slot index"
        );
    } else {
        panic!("Should have an assignment");
    };

    let submit_slot_2 = H256::from_base58("4jNP3tGi9hxAGsR6WnkM1dbrWG68E7wzP1JZ58QvHnXk");

    if let Some(submit_assignment) = epoch_snapshot
        .partition_assignments
        .data_partitions
        .get(&submit_slot_2)
    {
        assert_eq!(
            submit_assignment.ledger_id,
            Some(DataLedger::Submit.get_id()),
            "Should be assigned to submit ledger"
        );
        assert_eq!(
            submit_assignment.slot_index,
            Some(2),
            "Should be assigned to slot 14"
        );
    } else {
        panic!("Should have an assignment");
    };
}
