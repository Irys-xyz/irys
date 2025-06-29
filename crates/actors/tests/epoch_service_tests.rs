use actix::Actor as _;
use base58::ToBase58 as _;
use irys_actors::epoch_service::EpochReplayData;

use actix::{actors::mocker::Mocker, Addr, Arbiter, Recipient, SystemRegistry};
use irys_actors::EpochServiceMessage;
use reth::payload::EthBuiltPayload;
use std::collections::VecDeque;
use std::sync::{Arc, RwLock};
use std::{any::Any, sync::atomic::AtomicU64, time::Duration};
use tokio::time::sleep;
use tracing::{debug, error, info};

use irys_actors::services::ServiceSenders;
use irys_actors::{
    block_index_service::{BlockIndexService, GetBlockIndexGuardMessage},
    epoch_service::EpochServiceInner,
};
use irys_actors::{
    mining::PartitionMiningActor,
    packing::{PackingActor, PackingRequest},
    BlockFinalizedMessage, BlockProducerMockActor, MockedBlockProducerAddr, SolutionFoundMessage,
};
use irys_config::StorageSubmodulesConfig;
use irys_database::{add_genesis_commitments, add_test_commitments, BlockIndex};
use irys_storage::{ie, StorageModule, StorageModuleVec};
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::NodeConfig;
use irys_types::PartitionChunkRange;
use irys_types::{partition::PartitionAssignment, DataLedger, IrysBlockHeader, H256};
use irys_types::{
    partition_chunk_offset_ie, ConsensusConfig, ConsensusOptions, EpochConfig, PartitionChunkOffset,
};
use irys_types::{Config, U256};
use irys_vdf::state::{VdfState, VdfStateReadonly};

#[actix::test]
async fn genesis_test() {
    // setup temp dir
    let mut config = NodeConfig::testnet();
    let tmp_dir = setup_tracing_and_temp_dir(None, false);
    let base_path = tmp_dir.path().to_path_buf();
    config.base_directory = base_path;
    let config: Config = config.into();

    // genesis block
    let mut genesis_block = IrysBlockHeader::new_mock_header();
    genesis_block.height = 0;
    let commitments = add_genesis_commitments(&mut genesis_block, &config);

    // Create epoch service with random miner address
    let block_index: Arc<RwLock<BlockIndex>> = Arc::new(RwLock::new(
        BlockIndex::new(&config.node_config).await.unwrap(),
    ));

    let block_index_actor = BlockIndexService::new(block_index, &config.consensus).start();
    SystemRegistry::set(block_index_actor);

    let storage_submodules_config =
        StorageSubmodulesConfig::load(config.node_config.base_directory.clone()).unwrap();
    let service_senders = ServiceSenders::new().0;
    let mut epoch_service =
        EpochServiceInner::new(&service_senders, &storage_submodules_config, &config);
    let miner_address = config.node_config.miner_address();

    // Process genesis message directly instead of through actor system
    // This allows us to inspect the actor's state after processing
    let (sender, _rx) = tokio::sync::oneshot::channel();
    epoch_service
        .handle_message(EpochServiceMessage::NewEpoch {
            new_epoch_block: genesis_block.into(),
            previous_epoch_block: None,
            commitments: Arc::new(commitments),
            sender,
        })
        .unwrap();

    {
        // Verify the correct number of ledgers have been added
        let ledgers = epoch_service.ledgers.read().unwrap();
        let expected_ledger_count = DataLedger::ALL.len();
        assert_eq!(ledgers.len(), expected_ledger_count);

        // Verify each ledger has one slot and the correct number of partitions
        let pub_slots = ledgers.get_slots(DataLedger::Publish);
        let sub_slots = ledgers.get_slots(DataLedger::Submit);

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
            let pa = epoch_service.partition_assignments.read().unwrap();
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
            let pa = epoch_service.partition_assignments.read().unwrap();
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
        let pa = epoch_service.partition_assignments.read().unwrap();
        let data_partition_count = pa.data_partitions.len() as u64;
        let expected_partitions = data_partition_count
            + EpochServiceInner::get_num_capacity_partitions(
                data_partition_count,
                &config.consensus,
            );
        assert_eq!(
            epoch_service.all_active_partitions.len(),
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
    println!("Ledger State: {:#?}", epoch_service.ledgers);

    let (sender, rx) = tokio::sync::oneshot::channel();
    epoch_service
        .handle_message(EpochServiceMessage::GetLedgersGuard(sender))
        .unwrap();
    let ledgers = rx.await.unwrap();

    println!("{:?}", ledgers.read());

    // let infos = epoch_service.get_genesis_storage_module_infos();
    // println!("{:#?}", infos);
}

#[actix::test]
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
        ..ConsensusConfig::testnet()
    };
    let mut testnet_config = NodeConfig::testnet();
    testnet_config.base_directory = base_path;
    testnet_config.consensus = ConsensusOptions::Custom(consensus_config);
    let config = Config::new(testnet_config);
    genesis_block.height = 0;
    let num_blocks_in_epoch = config.consensus.epoch.num_blocks_in_epoch;
    let num_chunks_in_partition = config.consensus.num_chunks_in_partition;
    let commitments = add_genesis_commitments(&mut genesis_block, &config);

    let storage_submodules_config =
        StorageSubmodulesConfig::load(config.node_config.base_directory.clone()).unwrap();
    let service_senders = ServiceSenders::new().0;
    let mut epoch_service =
        EpochServiceInner::new(&service_senders, &storage_submodules_config, &config);

    let _ = epoch_service.initialize(genesis_block.clone(), commitments);

    let mut mock_header = IrysBlockHeader::new_mock_header();
    mock_header.data_ledgers[DataLedger::Submit].max_chunk_offset = 0;

    // Now create a new epoch block & give the Submit ledger enough size to add one slot
    let mut new_epoch_block = mock_header.clone();
    new_epoch_block.height = num_blocks_in_epoch;
    new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset = num_chunks_in_partition / 2;

    // Post the new epoch block to the service and let it perform_epoch_tasks()
    let (sender, _rx) = tokio::sync::oneshot::channel();
    epoch_service
        .handle_message(EpochServiceMessage::NewEpoch {
            new_epoch_block: new_epoch_block.clone().into(),
            previous_epoch_block: Some(genesis_block.clone()),
            commitments: Arc::new(Vec::new()),
            sender,
        })
        .unwrap();

    let ledgers = epoch_service.ledgers.read().unwrap();
    debug!("{:#?}", ledgers);
    drop(ledgers);

    // Verify each ledger has one slot and the correct number of partitions
    {
        let ledgers = epoch_service.ledgers.read().unwrap();
        let pub_slots = ledgers.get_slots(DataLedger::Publish);
        let sub_slots = ledgers.get_slots(DataLedger::Submit);
        assert_eq!(pub_slots.len(), 1);
        assert_eq!(sub_slots.len(), 3); // TODO: check 1 expired, 2 new slots added
    }

    let previous_epoch_block = Some(new_epoch_block.clone());

    // Simulate a subsequent epoch block that adds multiple ledger slots
    let mut new_epoch_block = IrysBlockHeader::new_mock_header();
    new_epoch_block.height = num_blocks_in_epoch * 2;

    // Increase the Submit ledger by 3 slots  and the Publish ledger by 2 slots
    new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset =
        (num_chunks_in_partition as f64 * 2.5) as u64;
    new_epoch_block.data_ledgers[DataLedger::Publish as usize].max_chunk_offset =
        (num_chunks_in_partition as f64 * 0.75) as u64;

    let (sender, _rx) = tokio::sync::oneshot::channel();
    epoch_service
        .handle_message(EpochServiceMessage::NewEpoch {
            new_epoch_block: new_epoch_block.clone().into(),
            previous_epoch_block,
            commitments: Arc::new(Vec::new()),
            sender,
        })
        .unwrap();

    let ledgers = epoch_service.ledgers.read().unwrap();
    debug!("{:#?}", ledgers);
    drop(ledgers);

    // Validate the correct number of ledgers slots were added to each ledger
    {
        let ledgers = epoch_service.ledgers.read().unwrap();
        let pub_slots = ledgers.get_slots(DataLedger::Publish);
        let sub_slots = ledgers.get_slots(DataLedger::Submit);
        assert_eq!(pub_slots.len(), 3);
        assert_eq!(sub_slots.len(), 7);
        println!("Ledger State: {:#?}", ledgers);
    }
}

#[actix::test]
async fn capacity_projection_tests() {
    let max_data_parts = 1000;
    let config = ConsensusConfig::testnet();
    for i in (0..max_data_parts).step_by(10) {
        let data_partition_count = i;
        let capacity_count =
            EpochServiceInner::get_num_capacity_partitions(data_partition_count, &config);
        let total = data_partition_count + capacity_count;
        println!(
            "data:{}, capacity:{}, total:{}",
            data_partition_count, capacity_count, total
        );
    }
}

#[actix::test]
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
        ..ConsensusConfig::testnet()
    };
    let mut config = NodeConfig::testnet();
    config.base_directory = base_path.clone();
    config.consensus = ConsensusOptions::Custom(consensus_config);
    let config = Config::new(config);

    let mut genesis_block = IrysBlockHeader::new_mock_header();
    genesis_block.height = 0;
    let commitments = add_test_commitments(&mut genesis_block, 5, &config);

    // Create a storage config for testing
    let num_blocks_in_epoch = config.consensus.epoch.num_blocks_in_epoch;
    let num_chunks_in_partition = config.consensus.num_chunks_in_partition;

    // Create epoch service
    let storage_submodules_config = StorageSubmodulesConfig::load(base_path.clone()).unwrap();
    let service_senders = ServiceSenders::new().0;
    let mut epoch_service =
        EpochServiceInner::new(&service_senders, &storage_submodules_config, &config);
    let storage_module_infos = epoch_service
        .initialize(genesis_block.clone(), commitments)
        .unwrap();

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

    let rwlock: RwLock<Option<PackingRequest>> = RwLock::new(None);
    let arc_rwlock = Arc::new(rwlock);
    let closure_arc = arc_rwlock.clone();

    let mocked_block_producer = BlockProducerMockActor::mock(Box::new(move |_msg, _ctx| {
        let inner_result: eyre::Result<Option<(Arc<IrysBlockHeader>, EthBuiltPayload)>> = Ok(None);
        Box::new(Some(inner_result)) as Box<dyn Any>
    }));

    let block_producer_actor_addr: Addr<BlockProducerMockActor> = mocked_block_producer.start();
    let recipient: Recipient<SolutionFoundMessage> = block_producer_actor_addr.recipient();
    let mocked_addr = MockedBlockProducerAddr(recipient);

    let packing = Mocker::<PackingActor>::mock(Box::new(move |msg, _ctx| {
        let packing_req = *msg.downcast::<PackingRequest>().unwrap();
        debug!("Packing request arrived ...");

        {
            let mut lck = closure_arc.write().unwrap();
            lck.replace(packing_req);
        }

        debug!("Packing request result pushed ...");
        Box::new(Some(())) as Box<dyn Any>
    }));

    let vdf_steps_guard = VdfStateReadonly::new(Arc::new(RwLock::new(VdfState {
        capacity: 10,
        global_step: 0,
        seeds: VecDeque::new(),
        mining_state_sender: None,
    })));

    let packing_addr = packing.start();
    let mut part_actors = Vec::new();

    let atomic_global_step_number = Arc::new(AtomicU64::new(0));

    for sm in &storage_modules {
        let partition_mining_actor = PartitionMiningActor::new(
            &config,
            mocked_addr.0.clone(),
            packing_addr.clone().recipient(),
            sm.clone(),
            true, // do not start mining automatically
            vdf_steps_guard.clone(),
            atomic_global_step_number.clone(),
            U256::zero(),
            None,
        );

        let part_arbiter = Arbiter::new();
        let partition_address =
            PartitionMiningActor::start_in_arbiter(&part_arbiter.handle(), |_| {
                partition_mining_actor
            });
        debug!("starting miner partition hash {:?}", sm.partition_hash());
        part_actors.push(partition_address);
    }

    let assign_submit_partition_hash = {
        let (sender, rx) = tokio::sync::oneshot::channel();
        epoch_service
            .handle_message(EpochServiceMessage::GetPartitionAssignmentsGuard(sender))
            .unwrap();
        let partitions_guard = rx.await.unwrap();

        let partition_hash = partitions_guard
            .read()
            .data_partitions
            .iter()
            .find(|(_hash, assignment)| assignment.ledger_id == Some(DataLedger::Submit.get_id()))
            .map(|(hash, _)| *hash)
            .expect("There should be a partition assigned to submit ledger");

        partition_hash
    };

    let (publish_partition_hash, submit_partition_hash) = {
        let (sender, rx) = tokio::sync::oneshot::channel();
        epoch_service
            .handle_message(EpochServiceMessage::GetLedgersGuard(sender))
            .unwrap();
        let ledgers = rx.await.unwrap();

        let pub_slots = ledgers.read().get_slots(DataLedger::Publish).clone();
        let sub_slots = ledgers.read().get_slots(DataLedger::Submit).clone();
        assert_eq!(pub_slots.len(), 1);
        assert_eq!(sub_slots.len(), 1);

        (pub_slots[0].partitions[0], sub_slots[0].partitions[0])
    };

    assert_eq!(assign_submit_partition_hash, submit_partition_hash);

    let capacity_partitions = {
        let (sender, rx) = tokio::sync::oneshot::channel();
        epoch_service
            .handle_message(EpochServiceMessage::GetPartitionAssignmentsGuard(sender))
            .unwrap();
        let partitions_guard = rx.await.unwrap();

        let capacity_partitions: Vec<H256> = partitions_guard
            .read()
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

    let (sender, rx) = tokio::sync::oneshot::channel();
    epoch_service
        .handle_message(EpochServiceMessage::GetLedgersGuard(sender))
        .unwrap();
    let ledgers_guard = rx.await.unwrap();

    // Simulate enough epoch blocks to compete a Submit ledger storage term, expiring a slot
    let mut new_epoch_block = IrysBlockHeader::new_mock_header();
    let mut previous_epoch_block = Some(genesis_block.clone());
    for i in 0..config.consensus.epoch.submit_ledger_epoch_length + 4 {
        new_epoch_block.height = num_blocks_in_epoch + num_blocks_in_epoch * i;

        if i == 3 {
            new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset =
                num_chunks_in_partition / 3;
        }

        if i == 5 {
            new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset =
                num_chunks_in_partition / 2;
        }

        debug!(
            "Epoch Block: Submit.max_chunk_offset = {}",
            new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset
        );

        let (sender, rx) = tokio::sync::oneshot::channel();
        epoch_service
            .handle_message(EpochServiceMessage::NewEpoch {
                new_epoch_block: new_epoch_block.clone().into(),
                previous_epoch_block,
                commitments: Arc::new(Vec::new()),
                sender,
            })
            .unwrap();
        let result = rx.await.unwrap();

        if let Err(err) = result {
            error!("Error processing NewEpochMessage: {:?}", err);
            panic!("Test failed: {:?}", err);
        }

        previous_epoch_block = Some(new_epoch_block.clone());
        let ledgers = ledgers_guard.read();
        debug!("{:#?}", ledgers);
        drop(ledgers);
    }

    // busypoll the solution context rwlock
    let mut max_pools = 10;
    let pack_req = 'outer: loop {
        if max_pools == 0 {
            panic!("Max. retries reached");
        } else {
            max_pools -= 1;
        }
        match arc_rwlock.try_read() {
            Ok(lck) => {
                if lck.is_none() {
                    debug!("Packing request not ready waiting!");
                } else {
                    debug!("Packing request received ready!");
                    break 'outer lck.as_ref().unwrap().clone();
                }
            }
            Err(err) => {
                debug!("Packing request read error {:?}", err);
            }
        }
        sleep(Duration::from_millis(50)).await;
    };

    // check a new slots is inserted with a partition assigned to it, and slot 0 expired and its partition was removed
    let (publish_partition, submit_partition, submit_partition2) = {
        let pub_slots = ledgers_guard.read().get_slots(DataLedger::Publish).clone();
        let sub_slots = ledgers_guard.read().get_slots(DataLedger::Submit).clone();
        assert_eq!(
            pub_slots.len(),
            1,
            "Publish should still have only one slot"
        );
        debug!("Ledger State: {:#?}", ledgers_guard);

        assert_eq!(sub_slots.len(), 3, "Submit slots should have two new not expired slots with a new fresh partition from available previous capacity ones!");
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
        let (sender, rx) = tokio::sync::oneshot::channel();
        epoch_service
            .handle_message(EpochServiceMessage::GetPartitionAssignmentsGuard(sender))
            .unwrap();
        let partitions_guard = rx.await.unwrap();

        assert_eq!(
            partitions_guard.read().data_partitions.len(),
            3,
            "Should have four partitions assignments"
        );

        if let Some(publish_assignment) = partitions_guard
            .read()
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

        if let Some(submit_assignment) = partitions_guard
            .read()
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

        if let Some(submit_assignment) = partitions_guard
            .read()
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
        pack_req.storage_module.partition_hash(),
        Some(submit_partition_hash),
        "Partition hashes should be equal"
    );
    assert_eq!(
        pack_req.chunk_range,
        PartitionChunkRange(partition_chunk_offset_ie!(0, chunk_count as u32)),
        "The whole partition should be repacked"
    );
}

#[actix::test]
async fn epoch_blocks_reinitialization_test() {
    let tmp_dir = setup_tracing_and_temp_dir(Some("epoch_block_reinitialization_test"), false);
    let base_path = tmp_dir.path().to_path_buf();
    let chunk_size = 32;
    let consensus_config = ConsensusConfig {
        chunk_size,
        ..ConsensusConfig::testnet()
    };
    let mut config = NodeConfig::testnet();
    config.base_directory = base_path.clone();
    config.consensus = ConsensusOptions::Custom(consensus_config);
    let config = Config::new(config);
    let num_chunks_in_partition = config.consensus.num_chunks_in_partition;
    let num_blocks_in_epoch = config.consensus.epoch.num_blocks_in_epoch;

    let block_index: Arc<RwLock<BlockIndex>> = Arc::new(RwLock::new(
        BlockIndex::new(&config.node_config).await.unwrap(),
    ));

    let block_index_actor = BlockIndexService::new(block_index.clone(), &config.consensus).start();
    SystemRegistry::set(block_index_actor.clone());

    let storage_submodules_config =
        StorageSubmodulesConfig::load(config.node_config.base_directory.clone()).unwrap();
    let service_senders = ServiceSenders::new().0;
    let mut epoch_service =
        EpochServiceInner::new(&service_senders, &storage_submodules_config, &config);

    // Initialize genesis block at height 0
    let mut genesis_block = IrysBlockHeader::new_mock_header();
    genesis_block.height = 0;
    let pledge_count = config.consensus.epoch.num_capacity_partitions.unwrap_or(31) as u8;
    let commitments = add_test_commitments(&mut genesis_block, pledge_count, &config);

    // Get the genesis storage modules and their assigned partitions

    let storage_module_infos = epoch_service
        .initialize(genesis_block.clone(), commitments.clone())
        .unwrap();
    debug!("{:#?}", storage_module_infos);

    genesis_block.block_hash = H256::from_slice(&[0; 32]);

    let msg = BlockFinalizedMessage {
        block_header: Arc::new(genesis_block.clone()),
        all_txs: Arc::new(vec![]),
    };
    match block_index_actor.send(msg).await {
        Ok(_) => info!("Genesis block indexed"),
        Err(_) => panic!("Failed to index genesis block"),
    }

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
    new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset = 0;

    let mut epoch_replay_data: Vec<EpochReplayData> = Vec::new();
    let epochs_in_term = config.consensus.epoch.submit_ledger_epoch_length;
    let mut previous_epoch_block = Some(genesis_block.clone());

    for i in 0..=epochs_in_term {
        // Simulate blocks up to one before the next epoch boundary
        let next_epoch_height = num_blocks_in_epoch * (i + 1);

        // For the second to last epoch block in the term, have it resize the submit ledger
        if i == epochs_in_term - 1 {
            new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset =
                num_chunks_in_partition / 2;
        }

        // Send the epoch message
        new_epoch_block.height = next_epoch_height;

        let (sender, rx) = tokio::sync::oneshot::channel();
        epoch_service
            .handle_message(EpochServiceMessage::NewEpoch {
                new_epoch_block: new_epoch_block.clone().into(),
                previous_epoch_block,
                commitments: Arc::new(Vec::new()),
                sender,
            })
            .unwrap();
        let result = rx.await.unwrap();

        if let Err(err) = result {
            error!("Error processing NewEpochMessage: {:?}", err);
            panic!("Test failed: {:?}", err);
        }

        epoch_replay_data.push(EpochReplayData {
            epoch_block: new_epoch_block.clone(),
            commitments: Vec::new(),
        });
        previous_epoch_block = Some(new_epoch_block.clone());
    }

    // Verify each ledger has one slot and the correct number of partitions
    {
        let ledgers = epoch_service.ledgers.read().unwrap();
        debug!("{:#?}", ledgers);
        let pub_slots = ledgers.get_slots(DataLedger::Publish);
        let sub_slots = ledgers.get_slots(DataLedger::Submit);
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

    let (sender, rx) = tokio::sync::oneshot::channel();
    epoch_service
        .handle_message(EpochServiceMessage::GetPartitionAssignmentsGuard(sender))
        .unwrap();
    let partitions_guard = rx.await.unwrap();

    partitions_guard.read().print_assignments();

    let block_index_guard = block_index_actor
        .send(GetBlockIndexGuardMessage)
        .await
        .unwrap();

    debug!(
        "num blocks in block_index: {}",
        block_index_guard.read().num_blocks()
    );

    let service_senders = ServiceSenders::new().0;
    // Get the genesis storage modules and their assigned partitions
    let mut epoch_service =
        EpochServiceInner::new(&service_senders, &storage_submodules_config, &config);
    let storage_module_infos = epoch_service
        .initialize(genesis_block, commitments)
        .unwrap();

    epoch_service
        .replay_epoch_data(epoch_replay_data)
        .expect("to replay the epoch data");

    debug!("{:#?}", storage_module_infos);

    let new_sm_infos =
        epoch_service.map_storage_modules_to_partition_assignments(&storage_submodules_config);

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

#[actix::test]
async fn partitions_assignment_determinism_test() {
    //std::env::set_var("RUST_LOG", "debug");
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
        ..ConsensusConfig::testnet()
    };
    let mut config = NodeConfig::testnet();
    config.storage.num_writes_before_sync = 1;
    config.base_directory = base_path.clone();
    config.consensus = ConsensusOptions::Custom(consensus_config);
    let config = Config::new(config);
    let num_chunks_in_partition = config.consensus.num_chunks_in_partition;
    let num_blocks_in_epoch = config.consensus.epoch.num_blocks_in_epoch;

    // Initialize genesis block at height 0
    let mut genesis_block = IrysBlockHeader::new_mock_header();
    genesis_block.last_epoch_hash = H256::zero(); // for partitions hash determinism
    genesis_block.height = 0;
    let pledge_count = 20;
    let commitments = add_test_commitments(&mut genesis_block, pledge_count, &config);

    let storage_submodules_config = StorageSubmodulesConfig::load_for_test(base_path, 40).unwrap();
    let service_senders = ServiceSenders::new().0;
    let mut epoch_service =
        EpochServiceInner::new(&service_senders, &storage_submodules_config, &config);

    let _ = epoch_service.initialize(genesis_block.clone(), commitments);

    let (sender, rx) = tokio::sync::oneshot::channel();
    epoch_service
        .handle_message(EpochServiceMessage::GetPartitionAssignmentsGuard(sender))
        .unwrap();
    let partitions_guard = rx.await.unwrap();
    partitions_guard.read().print_assignments();

    // Now create a new epoch block & give the Submit ledger enough size to add a slot
    let total_epoch_messages = 6;
    let mut epoch_num = 1;
    let mut new_epoch_block = IrysBlockHeader::new_mock_header();
    new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset = num_chunks_in_partition;
    new_epoch_block.data_ledgers[DataLedger::Publish].max_chunk_offset = num_chunks_in_partition;

    let mut previous_epoch_block = Some(genesis_block.clone());
    while epoch_num <= total_epoch_messages {
        new_epoch_block.height = epoch_num * num_blocks_in_epoch;
        //(testnet_config.submit_ledger_epoch_length * epoch_num) * num_blocks_in_epoch; // next epoch block, next multiple of num_blocks_in epoch,
        epoch_num += 1;
        debug!("epoch block {}", new_epoch_block.height);
        let (sender, _rx) = tokio::sync::oneshot::channel();
        epoch_service
            .handle_message(EpochServiceMessage::NewEpoch {
                new_epoch_block: new_epoch_block.clone().into(),
                previous_epoch_block,
                commitments: Arc::new(Vec::new()),
                sender,
            })
            .unwrap();
        previous_epoch_block = Some(new_epoch_block.clone());
    }

    partitions_guard.read().print_assignments();
    debug!(
        "\nAll Partitions({})\n{}",
        &epoch_service.all_active_partitions.len(),
        serde_json::to_string_pretty(&epoch_service.all_active_partitions).unwrap()
    );

    // Check determinism in assigned partitions
    let publish_slot_0 = H256::from_base58("2F5eg8FE2VmXGcgpyUKTzBrLzSmVXMKqawUJeDgKC1vW");
    debug!("expected publish[0] -> {}", publish_slot_0.0.to_base58());

    if let Some(publish_assignment) = epoch_service
        .partition_assignments
        .read()
        .unwrap()
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

    let publish_slot_1 = H256::from_base58("2HVmW86qVyKTw1DYJMX6NoNvVxATLNZHSAyMceEWPtLC");
    debug!("expected publish[1] -> {}", publish_slot_1.0.to_base58());

    if let Some(publish_assignment) = epoch_service
        .partition_assignments
        .read()
        .unwrap()
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

    if let Some(capacity_assignment) = epoch_service
        .partition_assignments
        .read()
        .unwrap()
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

    let submit_slot_2 = H256::from_base58("8sRHV12yycwpUSzean97JemQrzAXSSQWMmC4Jx3xUXzQ");

    if let Some(submit_assignment) = epoch_service
        .partition_assignments
        .read()
        .unwrap()
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
