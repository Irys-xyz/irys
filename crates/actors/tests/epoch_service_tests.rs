use actix::{Actor, Context, Handler};
use base58::ToBase58;
use irys_actors::epoch_service::{GetLedgersGuardMessage, GetPartitionAssignmentsGuardMessage};
use irys_config::StorageSubmodulesConfig;
use irys_types::{
    partition::PartitionAssignment, DatabaseProvider, IrysBlockHeader, StorageConfig, H256,
};
use irys_types::{partition_chunk_offset_ie, Address, PartitionChunkOffset};
use irys_types::{Config, U256};
use reth_db::Database;
use std::collections::VecDeque;
use std::sync::{Arc, RwLock};
use tokio::time::sleep;
use tracing::{debug, info};

use std::{any::Any, sync::atomic::AtomicU64, time::Duration};

use actix::{actors::mocker::Mocker, Addr, Arbiter, Recipient, SystemRegistry};
use alloy_rpc_types_engine::ExecutionPayloadEnvelopeV1Irys;
use irys_actors::{
    block_index_service::{BlockIndexService, GetBlockIndexGuardMessage},
    epoch_service::{EpochServiceActor, EpochServiceConfig, NewEpochMessage},
};
use irys_actors::{
    mining::PartitionMiningActor,
    packing::{PackingActor, PackingRequest},
    BlockFinalizedMessage, BlockProducerMockActor, MockedBlockProducerAddr, SolutionFoundMessage,
};
use irys_config::IrysNodeConfig;
use irys_database::{
    add_genesis_commitments, add_test_commitments, insert_commitment_tx, open_or_create_db,
    tables::IrysTables, BlockIndex, DataLedger, Initialized,
};
use irys_storage::{ie, StorageModule, StorageModuleVec};
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::PartitionChunkRange;
use irys_vdf::vdf_state::{VdfState, VdfStepsReadGuard};

#[cfg(test)]
#[actix::test]
async fn genesis_test() {
    // Initialize genesis block at height 0

    use irys_actors::epoch_service::GetLedgersGuardMessage;
    let mut genesis_block = IrysBlockHeader::new_mock_header();
    let testnet_config = Config::testnet();
    genesis_block.height = 0;
    let commitments = add_genesis_commitments(&mut genesis_block, &testnet_config);

    // Create epoch service with random miner address
    let config = EpochServiceConfig::new(&testnet_config);
    let arc_config = Arc::new(IrysNodeConfig::default());
    let block_index: Arc<RwLock<BlockIndex<Initialized>>> = Arc::new(RwLock::new(
        BlockIndex::default()
            .reset(&arc_config.clone())
            .unwrap()
            .init(arc_config.clone())
            .await
            .unwrap(),
    ));

    let storage_config = StorageConfig::default();
    let block_index_actor =
        BlockIndexService::new(block_index.clone(), storage_config.clone()).start();
    SystemRegistry::set(block_index_actor.clone());

    let block_index_guard = block_index_actor
        .send(GetBlockIndexGuardMessage)
        .await
        .unwrap();

    let mut epoch_service =
        EpochServiceActor::new(config.clone(), &testnet_config, block_index_guard);
    let miner_address = config.storage_config.miner_address;

    // Process genesis message directly instead of through actor system
    // This allows us to inspect the actor's state after processing
    let _ = epoch_service.handle(
        NewEpochMessage {
            epoch_block: genesis_block.into(),
            commitments,
        },
        &mut Context::new(),
    );

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
            config.storage_config.num_partitions_in_slot
        );
        assert_eq!(
            sub_slots[0].partitions.len() as u64,
            config.storage_config.num_partitions_in_slot
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
                config.storage_config.num_partitions_in_slot as usize
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
                config.storage_config.num_partitions_in_slot as usize
            );
        }
    }

    // Verify the correct number of genesis partitions have been activated
    {
        let pa = epoch_service.partition_assignments.read().unwrap();
        let data_partition_count = pa.data_partitions.len() as u64;
        let expected_partitions = data_partition_count
            + EpochServiceActor::get_num_capacity_partitions(data_partition_count, &config);
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

    let ledgers = epoch_service.handle(GetLedgersGuardMessage, &mut Context::new());

    println!("{:?}", ledgers.read());

    // let infos = epoch_service.get_genesis_storage_module_infos();
    // println!("{:#?}", infos);
}

#[actix::test]
async fn add_slots_test() {
    // Initialize genesis block at height 0
    let mut genesis_block = IrysBlockHeader::new_mock_header();
    let testnet_config = Config::testnet();
    genesis_block.height = 0;

    let commitments = add_genesis_commitments(&mut genesis_block, &testnet_config);

    // Create a storage config for testing
    let storage_config = StorageConfig {
        chunk_size: 32,
        num_chunks_in_partition: 10,
        num_chunks_in_recall_range: 2,
        num_partitions_in_slot: 1,
        miner_address: Address::random(),
        min_writes_before_sync: 1,
        entropy_packing_iterations: testnet_config.entropy_packing_iterations,
        chunk_migration_depth: 1, // Testnet / single node config
        chain_id: 333,
    };
    let num_chunks_in_partition = storage_config.num_chunks_in_partition;
    let tmp_dir = setup_tracing_and_temp_dir(Some("add_slots_test"), false);
    let base_path = tmp_dir.path().to_path_buf();

    let config = EpochServiceConfig {
        capacity_scalar: 100,
        num_blocks_in_epoch: 100,
        num_capacity_partitions: Some(123),
        storage_config: storage_config.clone(),
    };
    let num_blocks_in_epoch = config.num_blocks_in_epoch;

    let arc_config = Arc::new(IrysNodeConfig {
        base_directory: base_path.clone(),
        ..IrysNodeConfig::default()
    });

    let block_index: Arc<RwLock<BlockIndex<Initialized>>> = Arc::new(RwLock::new(
        BlockIndex::default()
            .reset(&arc_config.clone())
            .unwrap()
            .init(arc_config.clone())
            .await
            .unwrap(),
    ));

    let block_index_actor = BlockIndexService::new(block_index.clone(), storage_config).start();
    SystemRegistry::set(block_index_actor.clone());

    let block_index_guard = block_index_actor
        .send(GetBlockIndexGuardMessage)
        .await
        .unwrap();

    let mut epoch_service = EpochServiceActor::new(config, &testnet_config, block_index_guard);

    // Process genesis message directly instead of through actor system
    // This allows us to inspect the actor's state after processing
    let mut ctx = Context::new();
    let _ = epoch_service.handle(
        NewEpochMessage {
            epoch_block: genesis_block.clone().into(),
            commitments,
        },
        &mut ctx,
    );

    let msg = BlockFinalizedMessage {
        block_header: Arc::new(genesis_block.clone()),
        all_txs: Arc::new(vec![]),
    };
    match block_index_actor.send(msg).await {
        Ok(_) => info!("Genesis block indexed"),
        Err(_) => panic!("Failed to index genesis block"),
    }

    // Now create a new epoch block & give the Submit ledger enough size to add a slot
    let mut new_epoch_block = IrysBlockHeader::new_mock_header();
    new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset = 0;

    // index epoch previous blocks
    let mut height = 1;
    while height < num_blocks_in_epoch {
        new_epoch_block.height = height;
        let msg = BlockFinalizedMessage {
            block_header: Arc::new(new_epoch_block.clone()),
            all_txs: Arc::new(vec![]),
        };
        match block_index_actor.send(msg).await {
            Ok(_) => debug!("block indexed"),
            Err(err) => panic!("Failed to index block {:?}", err),
        }

        height += 1;
    }

    new_epoch_block.height = num_blocks_in_epoch;
    new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset = num_chunks_in_partition / 2;

    let _ = epoch_service.handle(
        NewEpochMessage {
            epoch_block: new_epoch_block.clone().into(),
            commitments: Vec::new(),
        },
        &mut ctx,
    );

    // Verify each ledger has one slot and the correct number of partitions
    {
        let ledgers = epoch_service.ledgers.read().unwrap();
        let pub_slots = ledgers.get_slots(DataLedger::Publish);
        let sub_slots = ledgers.get_slots(DataLedger::Submit);
        assert_eq!(pub_slots.len(), 1);
        assert_eq!(sub_slots.len(), 3); // TODO: check 1 expired, 2 new slots added
    }

    // index epoch block up to 2 * num_blocks_in_epoch
    while height < 2 * num_blocks_in_epoch {
        new_epoch_block.height = height;
        let msg = BlockFinalizedMessage {
            block_header: Arc::new(new_epoch_block.clone()),
            all_txs: Arc::new(vec![]),
        };
        match block_index_actor.send(msg).await {
            Ok(_) => debug!("block indexed"),
            Err(_) => panic!("Failed to index block"),
        }

        height += 1;
    }

    // Simulate a subsequent epoch block that adds multiple ledger slots
    let mut new_epoch_block = IrysBlockHeader::new_mock_header();
    new_epoch_block.height = num_blocks_in_epoch * 2;
    new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset =
        (num_chunks_in_partition as f64 * 2.5) as u64;
    new_epoch_block.data_ledgers[DataLedger::Publish as usize].max_chunk_offset =
        (num_chunks_in_partition as f64 * 0.75) as u64;

    let _ = epoch_service.handle(
        NewEpochMessage {
            epoch_block: new_epoch_block.clone().into(),
            commitments: Vec::new(),
        },
        &mut ctx,
    );

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
    let config = EpochServiceConfig::default();
    for i in (0..max_data_parts).step_by(10) {
        let data_partition_count = i;
        let capacity_count =
            EpochServiceActor::get_num_capacity_partitions(data_partition_count, &config);
        let total = data_partition_count + capacity_count;
        println!(
            "data:{}, capacity:{}, total:{}",
            data_partition_count, capacity_count, total
        );
    }
}

#[actix::test]
async fn partition_expiration_test() {
    std::env::set_var("RUST_LOG", "debug");
    // Initialize genesis block at height 0
    let chunk_size = 32;
    let chunk_count = 10;
    let testnet_config = Config {
        chunk_size,
        num_chunks_in_partition: chunk_count,
        num_chunks_in_recall_range: 2,
        num_partitions_per_slot: 1,
        num_writes_before_sync: 1,
        chunk_migration_depth: 1,
        capacity_scalar: 100,
        submit_ledger_epoch_length: 2,
        num_blocks_in_epoch: 5,
        ..Config::testnet()
    };
    let mining_address = testnet_config.miner_address();

    let mut genesis_block = IrysBlockHeader::new_mock_header();
    genesis_block.height = 0;
    let commitments = add_test_commitments(&mut genesis_block, 5, &testnet_config);

    // Create a storage config for testing
    let storage_config = StorageConfig::new(&testnet_config);
    let num_chunks_in_partition = storage_config.num_chunks_in_partition;
    let tmp_dir = setup_tracing_and_temp_dir(Some("partition_expiration_test"), false);
    let base_path = tmp_dir.path().to_path_buf();

    let num_blocks_in_epoch = testnet_config.num_blocks_in_epoch;

    // Create epoch service
    let config = EpochServiceConfig {
        capacity_scalar: 100,
        num_blocks_in_epoch: num_blocks_in_epoch,
        num_capacity_partitions: Some(123),
        storage_config: storage_config.clone(),
    };

    let arc_config = Arc::new(IrysNodeConfig::default());
    let block_index: Arc<RwLock<BlockIndex<Initialized>>> = Arc::new(RwLock::new(
        BlockIndex::default()
            .reset(&arc_config.clone())
            .unwrap()
            .init(arc_config.clone())
            .await
            .unwrap(),
    ));

    let block_index_actor =
        BlockIndexService::new(block_index.clone(), storage_config.clone()).start();
    SystemRegistry::set(block_index_actor.clone());

    let block_index_guard = block_index_actor
        .send(GetBlockIndexGuardMessage)
        .await
        .unwrap();

    let db_env = open_or_create_db(tmp_dir, IrysTables::ALL, None).unwrap();
    let db = DatabaseProvider(Arc::new(db_env));

    // Make sure the genesis_block is in the DB so the epoch service is initialized
    // I don't like this dependency, dependencies should be passed into initialize() -DMac
    {
        db.update_eyre(|tx| irys_database::insert_block_header(tx, &genesis_block))
            .expect("to write genesis_block to db");

        db.update_eyre(|tx| {
            commitments
                .iter()
                .map(|commitment| insert_commitment_tx(tx, commitment))
                .collect::<eyre::Result<()>>()
        })
        .expect("inserting commitment tx should succeed");
    }

    //  You also can't initialize the epoch_service without the genesis block being in the block_index
    // Need a better way to communicate these dependencies. -DMac
    let msg = BlockFinalizedMessage {
        block_header: Arc::new(genesis_block.clone()),
        all_txs: Arc::new(vec![]),
    };
    match block_index_actor.send(msg).await {
        Ok(_) => info!("Genesis block indexed"),
        Err(_) => panic!("Failed to index genesis block"),
    }

    let storage_module_config = StorageSubmodulesConfig::load(base_path.clone()).unwrap();
    let mut epoch_service = EpochServiceActor::new(config, &testnet_config, block_index_guard);
    let _ = epoch_service
        .initialize(&db, storage_module_config.clone())
        .await;

    // Get the genesis storage modules and their assigned partitions
    let storage_module_infos =
        epoch_service.map_storage_modules_to_partition_assignments(storage_module_config);
    let epoch_service_actor = epoch_service.start();

    // Process genesis message directly instead of through actor system
    // This allows us to inspect the actor's state after processing
    let _ = epoch_service_actor
        .send(NewEpochMessage {
            epoch_block: genesis_block.into(),
            commitments,
        })
        .await
        .unwrap();

    let mut storage_modules: StorageModuleVec = Vec::new();
    // Create a list of storage modules wrapping the storage files
    for info in storage_module_infos {
        let arc_module = Arc::new(
            StorageModule::new(&base_path, &info, storage_config.clone())
                // TODO: remove this unwrap
                .unwrap(),
        );
        storage_modules.push(arc_module.clone());
    }

    let rwlock: RwLock<Option<PackingRequest>> = RwLock::new(None);
    let arc_rwlock = Arc::new(rwlock);
    let closure_arc = arc_rwlock.clone();

    let mocked_block_producer = BlockProducerMockActor::mock(Box::new(move |_msg, _ctx| {
        let inner_result: eyre::Result<
            Option<(Arc<IrysBlockHeader>, ExecutionPayloadEnvelopeV1Irys)>,
        > = Ok(None);
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

    let vdf_steps_guard = VdfStepsReadGuard::new(Arc::new(RwLock::new(VdfState {
        max_seeds_num: 10,
        global_step: 0,
        seeds: VecDeque::new(),
    })));

    let packing_addr = packing.start();
    let mut part_actors = Vec::new();

    let atomic_global_step_number = Arc::new(AtomicU64::new(0));

    for sm in &storage_modules {
        let partition_mining_actor = PartitionMiningActor::new(
            mining_address,
            db.clone(),
            mocked_addr.0.clone(),
            packing_addr.clone().recipient(),
            sm.clone(),
            true, // do not start mining automatically
            vdf_steps_guard.clone(),
            atomic_global_step_number.clone(),
            U256::zero(),
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
        let partition_assignments_read = epoch_service_actor
            .send(GetPartitionAssignmentsGuardMessage)
            .await
            .unwrap();

        let partition_hash = partition_assignments_read
            .read()
            .data_partitions
            .iter()
            .find(|(_hash, assignment)| assignment.ledger_id == Some(DataLedger::Submit.get_id()))
            .map(|(hash, _)| hash.clone())
            .expect("There should be a partition assigned to submit ledger");

        partition_hash
    };

    let (publish_partition_hash, submit_partition_hash) = {
        let ledgers = epoch_service_actor
            .send(GetLedgersGuardMessage)
            .await
            .unwrap();

        let pub_slots = ledgers.read().get_slots(DataLedger::Publish).clone();
        let sub_slots = ledgers.read().get_slots(DataLedger::Submit).clone();
        assert_eq!(pub_slots.len(), 1);
        assert_eq!(sub_slots.len(), 1);

        (pub_slots[0].partitions[0], sub_slots[0].partitions[0])
    };

    assert_eq!(assign_submit_partition_hash, submit_partition_hash);

    let capacity_partitions = {
        let partition_assignments_read = epoch_service_actor
            .send(GetPartitionAssignmentsGuardMessage)
            .await
            .unwrap();

        let capacity_partitions: Vec<H256> = partition_assignments_read
            .read()
            .capacity_partitions
            .keys()
            .map(|partition| partition.clone())
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

    let mut genesis_block = IrysBlockHeader::new_mock_header();
    genesis_block.height = 0;

    let msg = BlockFinalizedMessage {
        block_header: Arc::new(genesis_block.clone()),
        all_txs: Arc::new(vec![]),
    };
    match block_index_actor.send(msg).await {
        Ok(_) => info!("Genesis block indexed"),
        Err(_) => panic!("Failed to index genesis block"),
    }

    // Now create a new epoch block & give the Submit ledger enough size to add a slot
    let mut new_epoch_block = IrysBlockHeader::new_mock_header();
    new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset = 0;

    // index epoch previous blocks
    let mut height = 1;
    while height < (testnet_config.submit_ledger_epoch_length + 1) * num_blocks_in_epoch {
        new_epoch_block.height = height;
        let msg = BlockFinalizedMessage {
            block_header: Arc::new(new_epoch_block.clone()),
            all_txs: Arc::new(vec![]),
        };
        match block_index_actor.send(msg).await {
            Ok(_) => debug!("block indexed {}", height),
            Err(_) => panic!("Failed to index block {}", height),
        }

        height += 1;
    }

    new_epoch_block.height = (testnet_config.submit_ledger_epoch_length + 1) * num_blocks_in_epoch; // next epoch block, next multiple of num_blocks_in epoch,
    new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset = num_chunks_in_partition / 2;

    let _ = epoch_service_actor
        .send(NewEpochMessage {
            epoch_block: new_epoch_block.clone().into(),
            commitments: Vec::new(),
        })
        .await
        .unwrap();

    new_epoch_block.height = (testnet_config.submit_ledger_epoch_length + 2) * num_blocks_in_epoch;
    let _ = epoch_service_actor
        .send(NewEpochMessage {
            epoch_block: new_epoch_block.into(),
            commitments: Vec::new(),
        })
        .await
        .unwrap();

    let ledgers_guard = epoch_service_actor
        .send(GetLedgersGuardMessage)
        .await
        .unwrap();
    let ledgers = ledgers_guard.read();
    debug!("{:#?}", ledgers);
    drop(ledgers);

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
        let ledgers = epoch_service_actor
            .send(GetLedgersGuardMessage)
            .await
            .unwrap();

        let pub_slots = ledgers.read().get_slots(DataLedger::Publish).clone();
        let sub_slots = ledgers.read().get_slots(DataLedger::Submit).clone();
        assert_eq!(
            pub_slots.len(),
            1,
            "Publish should still have only one slot"
        );
        debug!("Ledger State: {:#?}", ledgers);

        assert_eq!(sub_slots.len(), 4, "Submit slots should have two new not expired slots with a new fresh partition from available previous capacity ones!");
        assert!(
            sub_slots[0].is_expired && sub_slots[0].partitions.len() == 0,
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

        assert!(
            !sub_slots[3].is_expired
                && sub_slots[3].partitions.len() == 1
                && (capacity_partitions.contains(&sub_slots[3].partitions[0])
                    || submit_partition_hash == sub_slots[3].partitions[0]),
            "Slot 3 should not be expired have a capacity or the just expired partition"
        );

        let publish_partition = pub_slots[0]
            .partitions
            .get(0)
            .expect("publish ledger slot 0 should have a partition assigned")
            .clone();
        let submit_partition = sub_slots[1]
            .partitions
            .get(0)
            .expect("submit ledger slot 1 should have a partition assigned")
            .clone();
        let submit_partition2 = sub_slots[2]
            .partitions
            .get(0)
            .expect("submit ledger slot 2 should have a partition assigned")
            .clone();

        (publish_partition, submit_partition, submit_partition2)
    };

    // check repacking request expired partition for its whole interval range, and partitions assignments are consistent
    {
        let partition_assignments_read = epoch_service_actor
            .send(GetPartitionAssignmentsGuardMessage)
            .await
            .unwrap();

        assert_eq!(
            partition_assignments_read.read().data_partitions.len(),
            4,
            "Should have four partitions assignments"
        );

        if let Some(publish_assignment) = partition_assignments_read
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

        if let Some(submit_assignment) = partition_assignments_read
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

        if let Some(submit_assignment) = partition_assignments_read
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
    std::env::set_var("RUST_LOG", "debug");
    let testnet_config = Config {
        chunk_size: 32,
        ..Config::testnet()
    };

    // Create a storage config for testing
    let storage_config = StorageConfig {
        chunk_size: testnet_config.chunk_size,
        num_chunks_in_partition: testnet_config.num_chunks_in_partition,
        num_chunks_in_recall_range: testnet_config.num_chunks_in_recall_range,
        num_partitions_in_slot: testnet_config.num_partitions_per_slot,
        miner_address: Address::random(),
        min_writes_before_sync: testnet_config.num_writes_before_sync,
        entropy_packing_iterations: testnet_config.entropy_packing_iterations,
        chunk_migration_depth: testnet_config.chunk_migration_depth, // Testnet / single node config
        chain_id: testnet_config.chain_id,
    };
    let num_chunks_in_partition = storage_config.num_chunks_in_partition;
    let tmp_dir = setup_tracing_and_temp_dir(Some("epoch_block_reinitialization_test"), false);
    let base_path = tmp_dir.path().to_path_buf();
    let storage_module_config = StorageSubmodulesConfig::load(base_path.clone()).unwrap();

    let db = open_or_create_db(tmp_dir, IrysTables::ALL, None).unwrap();
    let database_provider = DatabaseProvider(Arc::new(db));

    let config = EpochServiceConfig {
        capacity_scalar: testnet_config.capacity_scalar,
        num_blocks_in_epoch: testnet_config.num_blocks_in_epoch,
        num_capacity_partitions: Some(200),
        storage_config: storage_config.clone(),
    };
    let num_blocks_in_epoch = config.num_blocks_in_epoch;

    let arc_config = Arc::new(IrysNodeConfig {
        base_directory: base_path.clone(),
        ..IrysNodeConfig::default()
    });

    let block_index: Arc<RwLock<BlockIndex<Initialized>>> = Arc::new(RwLock::new(
        BlockIndex::default()
            .reset(&arc_config.clone())
            .unwrap()
            .init(arc_config.clone())
            .await
            .unwrap(),
    ));

    let block_index_actor =
        BlockIndexService::new(block_index.clone(), storage_config.clone()).start();
    SystemRegistry::set(block_index_actor.clone());

    let block_index_guard = block_index_actor
        .send(GetBlockIndexGuardMessage)
        .await
        .unwrap();

    let mut epoch_service =
        EpochServiceActor::new(config.clone(), &testnet_config, block_index_guard.clone());

    // Process genesis message directly instead of through actor system
    // This allows us to inspect the actor's state after processing
    let mut ctx = Context::new();
    // Initialize genesis block at height 0
    let mut genesis_block = IrysBlockHeader::new_mock_header();
    genesis_block.height = 0;
    let pledge_count = config.num_capacity_partitions.unwrap_or(31) as u8;
    let commitments = add_test_commitments(&mut genesis_block, pledge_count, &testnet_config);
    database_provider
        .update_eyre(|tx| {
            commitments
                .iter()
                .map(|commitment| insert_commitment_tx(tx, commitment))
                .collect::<eyre::Result<()>>()
        })
        .expect("inserting commitment tx should succeed");

    database_provider
        .update_eyre(|tx| irys_database::insert_block_header(tx, &genesis_block))
        .unwrap();

    // Get the genesis storage modules and their assigned partitions
    let storage_module_infos = epoch_service
        .initialize(&database_provider, storage_module_config.clone())
        .await
        .unwrap();
    debug!("{:#?}", storage_module_infos);

    genesis_block.block_hash = H256::from_slice(&[0; 32]);
    let _ = epoch_service.handle(
        NewEpochMessage {
            epoch_block: genesis_block.clone().into(),
            commitments,
        },
        &mut ctx,
    );

    let pa_read_guard = epoch_service.handle(GetPartitionAssignmentsGuardMessage, &mut ctx);

    // database_provider
    //     .update_eyre(|tx| irys_database::insert_block_header(tx, &genesis_block))
    //     .unwrap();

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
            let arc_module = Arc::new(
                StorageModule::new(
                    &arc_config.storage_module_dir(),
                    &info,
                    storage_config.clone(),
                )
                .unwrap(),
            );
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

    // index and store in db blocks
    let mut height = 1;
    while height <= (testnet_config.submit_ledger_epoch_length + 2) * num_blocks_in_epoch {
        new_epoch_block.height = height;
        new_epoch_block.block_hash = H256::random();

        if height == (testnet_config.submit_ledger_epoch_length + 1) * num_blocks_in_epoch {
            new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset =
                num_chunks_in_partition / 2;
        }

        let msg = BlockFinalizedMessage {
            block_header: Arc::new(new_epoch_block.clone()),
            all_txs: Arc::new(vec![]),
        };
        match block_index_actor.send(msg).await {
            Ok(_) => (), // debug!("block indexed"),
            Err(err) => panic!("Failed to index block {:?}", err),
        }

        if height % num_blocks_in_epoch == 0 {
            debug!("epoch block {}", height);
            let _ = epoch_service.handle(
                NewEpochMessage {
                    epoch_block: new_epoch_block.clone().into(),
                    commitments: Vec::new(),
                },
                &mut ctx,
            );
        }

        database_provider
            .update_eyre(|tx| irys_database::insert_block_header(tx, &new_epoch_block))
            .unwrap();
        height += 1;
    }

    // Verify each ledger has one slot and the correct number of partitions
    {
        let ledgers = epoch_service.ledgers.read().unwrap();
        debug!("{:#?}", ledgers);
        let pub_slots = ledgers.get_slots(DataLedger::Publish);
        let sub_slots = ledgers.get_slots(DataLedger::Submit);
        assert_eq!(pub_slots.len(), 1);
        assert_eq!(sub_slots.len(), 4); // TODO: check slot 1 expired, 3 new slots added
    }

    //            +---+
    //            |sm0|
    //            +-|-+  |    |
    // Publish 0----+----+----+---
    //              |    |    |
    //              0    1    2
    //                  +---+ +---+ +---+
    //                  |sm2| |sm1| | ? |
    //                  +-|-+ +-|-+ +-|-+
    // Submit 1 +----+----+-----+-----+----+---
    //          |    |    |     |     |
    //          0    1    2     3     4
    // Capacity

    pa_read_guard.read().print_assignments();

    let block_index_guard = block_index_actor
        .send(GetBlockIndexGuardMessage)
        .await
        .unwrap();

    debug!(
        "num blocks in block_index: {}",
        block_index_guard.read().num_blocks()
    );

    // Get the genesis storage modules and their assigned partitions
    let mut epoch_service = EpochServiceActor::new(config, &testnet_config, block_index_guard);
    let storage_module_infos = epoch_service
        .initialize(&database_provider, storage_module_config.clone())
        .await
        .unwrap();
    debug!("{:#?}", storage_module_infos);

    // Check partition hashes have not changed in storage modules
    {
        let mut storage_modules: StorageModuleVec = Vec::new();

        // Create a list of storage modules wrapping the storage files
        for info in storage_module_infos {
            let arc_module = Arc::new(
                StorageModule::new(
                    &arc_config.storage_module_dir(),
                    &info,
                    storage_config.clone(),
                )
                // TODO: remove this unwrap
                .unwrap(),
            );
            storage_modules.push(arc_module.clone());
        }
    }
}

#[actix::test]
async fn partitions_assignment_determinism_test() {
    std::env::set_var("RUST_LOG", "debug");
    let testnet_config = Config {
        submit_ledger_epoch_length: 2,
        ..Config::testnet()
    };
    // Initialize genesis block at height 0
    let mut genesis_block = IrysBlockHeader::new_mock_header();
    genesis_block.last_epoch_hash = H256::zero(); // for partitions hash determinism
    genesis_block.height = 0;
    let pledge_count = 20;
    let commitments = add_test_commitments(&mut genesis_block, pledge_count, &testnet_config);

    // TODO: need a test method that pledges X partitions regardless of the storage config

    // Create a storage config for testing
    let storage_config = StorageConfig {
        chunk_size: 32,
        num_chunks_in_partition: 10,
        num_chunks_in_recall_range: 2,
        num_partitions_in_slot: 1,
        miner_address: testnet_config.miner_address(),
        min_writes_before_sync: 1,
        entropy_packing_iterations: testnet_config.entropy_packing_iterations,
        chunk_migration_depth: 1, // Testnet / single node config
        chain_id: 1,
    };
    let num_chunks_in_partition = storage_config.num_chunks_in_partition;

    // Create epoch service
    let config = EpochServiceConfig {
        capacity_scalar: 100,
        num_blocks_in_epoch: 100,
        num_capacity_partitions: None,
        storage_config: storage_config.clone(),
    };
    let num_blocks_in_epoch = config.num_blocks_in_epoch;

    let tmp_dir = setup_tracing_and_temp_dir(Some("epoch_block_reinitialization_test"), false);
    let base_path = tmp_dir.path().to_path_buf();
    let arc_config = Arc::new(IrysNodeConfig {
        base_directory: base_path.clone(),
        ..IrysNodeConfig::default()
    });

    let block_index: Arc<RwLock<BlockIndex<Initialized>>> = Arc::new(RwLock::new(
        BlockIndex::default()
            .reset(&arc_config.clone())
            .unwrap()
            .init(arc_config.clone())
            .await
            .unwrap(),
    ));

    let block_index_actor =
        BlockIndexService::new(block_index.clone(), storage_config.clone()).start();
    SystemRegistry::set(block_index_actor.clone());

    let block_index_guard = block_index_actor
        .send(GetBlockIndexGuardMessage)
        .await
        .unwrap();

    let mut epoch_service =
        EpochServiceActor::new(config.clone(), &testnet_config, block_index_guard.clone());
    let mut ctx = Context::new();
    let _ = epoch_service.handle(
        NewEpochMessage {
            epoch_block: genesis_block.clone().into(),
            commitments,
        },
        &mut ctx,
    );

    let pa_read_guard = epoch_service.handle(GetPartitionAssignmentsGuardMessage, &mut ctx);
    pa_read_guard.read().print_assignments();

    let msg = BlockFinalizedMessage {
        block_header: Arc::new(genesis_block.clone()),
        all_txs: Arc::new(vec![]),
    };
    match block_index_actor.send(msg).await {
        Ok(_) => info!("Genesis block indexed"),
        Err(_) => panic!("Failed to index genesis block"),
    }

    // Now create a new epoch block & give the Submit ledger enough size to add a slot
    let total_epoch_messages = 6;
    let mut epoch_num = 1;
    let mut new_epoch_block = IrysBlockHeader::new_mock_header();
    new_epoch_block.data_ledgers[DataLedger::Submit].max_chunk_offset = num_chunks_in_partition;
    new_epoch_block.data_ledgers[DataLedger::Publish].max_chunk_offset = num_chunks_in_partition;

    let mut height = 1;
    while epoch_num <= total_epoch_messages {
        new_epoch_block.height = height;
        //(testnet_config.submit_ledger_epoch_length * epoch_num) * num_blocks_in_epoch; // next epoch block, next multiple of num_blocks_in epoch,
        let msg = BlockFinalizedMessage {
            block_header: Arc::new(new_epoch_block.clone()),
            all_txs: Arc::new(vec![]),
        };
        match block_index_actor.send(msg).await {
            Ok(_) => (), // debug!("block indexed"),
            Err(err) => panic!("Failed to index block {:?}", err),
        }

        if height % num_blocks_in_epoch == 0 {
            epoch_num += 1;
            debug!("epoch block {}", height);
            let _ = epoch_service.handle(
                NewEpochMessage {
                    epoch_block: new_epoch_block.clone().into(),
                    commitments: Vec::new(),
                },
                &mut ctx,
            );
        }
        height += 1;
    }

    pa_read_guard.read().print_assignments();
    // debug!(
    //     "\nAll Partitions({})\n{}",
    //     &epoch_service.all_active_partitions.len(),
    //     serde_json::to_string_pretty(&epoch_service.all_active_partitions).unwrap()
    // );

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

    let submit_slot_7 = H256::from_base58("AtGjnuZ1EKmp8sP1FT3aMPbJWCY8tdEoksVFDB6PKXr1");

    if let Some(submit_assignment) = epoch_service
        .partition_assignments
        .read()
        .unwrap()
        .data_partitions
        .get(&submit_slot_7)
    {
        assert_eq!(
            submit_assignment.ledger_id,
            Some(DataLedger::Submit.get_id()),
            "Should be assigned to submit ledger"
        );
        assert_eq!(
            submit_assignment.slot_index,
            Some(7),
            "Should be assigned to slot 7"
        );
    } else {
        panic!("Should have an assignment");
    };
}
