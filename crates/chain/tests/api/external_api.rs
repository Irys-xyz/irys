//! chunk migration tests
use std::{
    str::FromStr,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use {
    irys_actors::block_index_service::BlockIndexService,
    irys_actors::mempool_service::MempoolService,
};

use actix::prelude::*;
use dev::SystemRegistry;
use irys_actors::{
    block_producer::BlockFinalizedMessage, chunk_migration_service::ChunkMigrationService,
    mempool_service::GetBestMempoolTxs,
};
use irys_api_server::{run_server, ApiState};
use irys_config::IrysNodeConfig;
use irys_database::{
    open_or_create_db,
    tables::{IngressProofs, IrysTables},
    BlockIndex, Initialized, Ledger,
};
use irys_storage::*;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{
    app_state::DatabaseProvider, partition::*, Address, Base64, H256List, IrysBlockHeader, PoaData,
    Signature, StorageConfig, TransactionLedger, VDFLimiterInfo, H256, U256,
};
use reth::{revm::primitives::B256, tasks::TaskManager};
use reth_db::transaction::DbTx;
use reth_db::Database as _;
use tokio::{task, time::sleep};
use tracing::info;

#[ignore]
#[actix::test]
async fn external_api() -> eyre::Result<()> {
    let temp_dir = setup_tracing_and_temp_dir(Some("external_api"), false);

    let mut node_config = IrysNodeConfig::default();
    node_config.base_directory = temp_dir.path().to_path_buf();
    let arc_config = Arc::new(node_config);

    // Create a storage config for testing
    let storage_config = StorageConfig {
        chunk_size: 32,
        num_chunks_in_partition: 6,
        num_chunks_in_recall_range: 2,
        num_partitions_in_slot: 1,
        miner_address: Address::random(),
        min_writes_before_sync: 1,
        entropy_packing_iterations: 1,
        chunk_migration_depth: 1, // Testnet / single node config
    };
    let _chunk_size = storage_config.chunk_size;

    // Create StorageModules for testing
    // TODO: once @DanMacDonald fixes promotion, switch back configs & ledger_num in JS test
    let storage_module_infos = vec![
        // StorageModuleInfo {
        //     id: 0,
        //     partition_assignment: Some(PartitionAssignment {
        //         partition_hash: H256::random(),
        //         miner_address: storage_config.miner_address,
        //         ledger_num: Some(0),
        //         slot_index: Some(0), // Publish Ledger Slot 0
        //     }),
        //     submodules: vec![
        //         (ii(0, 5), "sm1".to_string()), // 0 to 5 inclusive
        //     ],
        // },
        // StorageModuleInfo {
        //     id: 1,
        //     partition_assignment: Some(PartitionAssignment {
        //         partition_hash: H256::random(),
        //         miner_address: storage_config.miner_address,
        //         ledger_num: Some(1),
        //         slot_index: Some(0), // Submit Ledger Slot 0
        //     }),
        //     submodules: vec![
        //         (ii(0, 5), "sm2".to_string()), // 0 to 5 inclusive
        //     ],
        // },
        StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: H256::random(),
                miner_address: storage_config.miner_address,
                ledger_id: Some(1),
                slot_index: Some(0), // Submit Ledger Slot 0
            }),
            submodules: vec![
                (ii(0, 5), "sm1".into()), // 0 to 5 inclusive
            ],
        },
        StorageModuleInfo {
            id: 1,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: H256::random(),
                miner_address: storage_config.miner_address,
                ledger_id: Some(1),
                slot_index: Some(1), // Submit Ledger Slot 1
            }),
            submodules: vec![
                (ii(0, 5), "sm2".into()), // 0 to 5 inclusive
            ],
        },
    ];

    let tmp_dir = setup_tracing_and_temp_dir(Some("chunk_migration_test"), false);
    let base_path = tmp_dir.path().to_path_buf();
    info!("temp_dir:{:?}\nbase_path:{:?}", tmp_dir, base_path);
    let _ = initialize_storage_files(&base_path, &storage_module_infos);

    // Create a Vec initialized storage modules
    let mut storage_modules: Vec<Arc<StorageModule>> = Vec::new();
    for info in storage_module_infos {
        let arc_module = Arc::new(StorageModule::new(
            &base_path,
            &info,
            storage_config.clone(),
        )?);
        storage_modules.push(arc_module.clone());
        arc_module.pack_with_zeros();
    }

    let task_manager = TaskManager::current();
    let db = open_or_create_db(tmp_dir, IrysTables::ALL, None).unwrap();
    let arc_db = DatabaseProvider(Arc::new(db));

    // Create an instance of the mempool actor
    let mempool_service = MempoolService::new(
        arc_db.clone(),
        task_manager.executor(),
        arc_config.mining_signer.clone(),
        storage_config.clone(),
        storage_modules.clone(),
    );
    SystemRegistry::set(mempool_service.start());
    let mempool_addr = MempoolService::from_registry();

    // Create a block_index
    let block_index: Arc<RwLock<BlockIndex<Initialized>>> = Arc::new(RwLock::new(
        BlockIndex::default()
            .reset(&arc_config.clone())?
            .init(arc_config.clone())
            .await
            .unwrap(),
    ));

    let chunk_provider = ChunkProvider::new(
        storage_config.clone(),
        storage_modules.clone(),
        arc_db.clone(),
    );

    let app_state = ApiState {
        db: arc_db.clone(),
        mempool: mempool_addr.clone(),
        chunk_provider: Arc::new(chunk_provider),
    };

    // spawn server in a separate thread
    task::spawn(run_server(app_state));

    let address = "http://127.0.0.1:8080";
    // TODO: remove this delay and use proper probing to check if the server is active
    sleep(Duration::from_millis(500)).await;

    // server should be running
    // check with request to `/v1/info`
    let client = awc::Client::default();

    let response = client
        .get(format!("{}/v1/info", address))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    info!("HTTP server started");

    info!("waiting for tx header...");

    let recv_tx = loop {
        let txs = mempool_addr.send(GetBestMempoolTxs).await;
        match txs {
            Ok(transactions) if !transactions.is_empty() => {
                break transactions[0].clone();
            }
            _ => {
                sleep(Duration::from_millis(100)).await;
            }
        }
    };
    info!(
        "got tx {:?}- waiting for chunks & ingress proof generation...",
        &recv_tx.id
    );
    // now we wait for an ingress proof to be generated for this tx (automatic once all chunks have been uploaded)

    let ingress_proof = loop {
        // don't reuse the tx! it has read isolation (won't see anything committed after it's creation)
        let ro_tx = &arc_db.tx().unwrap();
        match ro_tx.get::<IngressProofs>(recv_tx.data_root).unwrap() {
            Some(ip) => break ip,
            None => sleep(Duration::from_millis(100)).await,
        }
    };

    info!(
        "got ingress proof for data root {}",
        &ingress_proof.data_root
    );
    assert_eq!(&ingress_proof.data_root, &recv_tx.data_root);

    info!("mining block");

    let tx_headers = vec![recv_tx];

    let data_tx_ids = tx_headers.iter().map(|h| h.id).collect::<Vec<H256>>();

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

    // Create a block_index actor
    let block_index_actor = BlockIndexService::new(block_index.clone(), storage_config.clone());
    SystemRegistry::set(block_index_actor.start());
    let block_index_addr = BlockIndexService::from_registry();

    let height: u64;
    {
        height = block_index.read().unwrap().num_blocks().max(1) - 1;
    }

    // Create a block from the tx
    let irys_block = IrysBlockHeader {
        diff: U256::from(1000),
        cumulative_diff: U256::from(5000),
        last_diff_timestamp: 1622543200,
        solution_hash: H256::zero(),
        previous_solution_hash: H256::zero(),
        last_epoch_hash: H256::random(),
        chunk_hash: H256::zero(),
        height,
        block_hash: H256::zero(),
        previous_block_hash: H256::zero(),
        previous_cumulative_diff: U256::from(4000),
        poa: PoaData {
            tx_path: None,
            data_path: None,
            chunk: Base64::from_str("").unwrap(),
            ledger_id: None,
            partition_chunk_offset: 0,
            partition_hash: PartitionHash::zero(),
            recall_chunk_index: 0,
        },
        reward_address: Address::ZERO,
        miner_address: Address::ZERO,
        signature: Signature::test_signature().into(),
        timestamp: now.as_millis(),
        ledgers: vec![
            // Permanent Publish Ledger
            TransactionLedger {
                ledger_id: Ledger::Publish.into(),
                tx_root: H256::zero(),
                tx_ids: H256List(Vec::new()),
                max_chunk_offset: 0,
                expires: None,
                proofs: None,
            },
            // Term Submit Ledger
            TransactionLedger {
                ledger_id: Ledger::Submit.into(),
                tx_root: TransactionLedger::merklize_tx_root(&tx_headers).0,
                tx_ids: H256List(data_tx_ids.clone()),
                max_chunk_offset: 0,
                expires: Some(1622543200),
                proofs: None,
            },
        ],
        evm_block_hash: B256::ZERO,
        vdf_limiter_info: VDFLimiterInfo::default(),
    };

    // Send the block confirmed message
    let block = Arc::new(irys_block);
    let txs: Arc<Vec<irys_types::IrysTransactionHeader>> = Arc::new(tx_headers);
    let block_finalized_message = BlockFinalizedMessage {
        block_header: block.clone(),
        all_txs: Arc::clone(&txs),
    };

    block_index_addr.do_send(block_finalized_message.clone());

    // Send the block finalized message
    let chunk_migration_service = ChunkMigrationService::new(
        block_index.clone(),
        storage_config.clone(),
        storage_modules.clone(),
        arc_db.clone(),
    );
    SystemRegistry::set(chunk_migration_service.start());
    let block_finalized_message = BlockFinalizedMessage {
        block_header: block.clone(),
        all_txs: txs.clone(),
    };
    let chunk_migration_addr = ChunkMigrationService::from_registry();
    let _res = chunk_migration_addr.send(block_finalized_message).await?;

    // Check to see if the chunks are in the StorageModules
    for sm in storage_modules.iter() {
        let _ = sm.sync_pending_chunks();
    }
    info!("mined block!");
    // sleep so the client has a chance to read the chunks
    sleep(Duration::from_millis(10_000)).await;

    Ok(())
}
