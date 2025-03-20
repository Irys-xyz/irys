//! chunk migration tests
use std::{
    str::FromStr,
    sync::{Arc, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use irys_actors::{
    block_index_service::BlockIndexService, mempool_service::MempoolService,
    services::ServiceSenders,
};

use actix::prelude::*;
use dev::SystemRegistry;
use irys_actors::{
    block_producer::BlockFinalizedMessage,
    block_tree_service::{BlockTreeService, GetBlockTreeGuardMessage},
    chunk_migration_service::ChunkMigrationService,
    mempool_service::GetBestMempoolTxs,
};
use irys_api_server::{run_server, ApiState};
use irys_config::IrysNodeConfig;
use irys_database::{
    open_or_create_db,
    tables::{IngressProofs, IrysTables},
    BlockIndex, Initialized, Ledger,
};
use irys_reth_node_bridge::node::{RethNode, RethNodeAddOns, RethNodeProvider};
use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
use irys_storage::*;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{
    app_state::DatabaseProvider, partition::*, partition_chunk_offset_ii,
    vdf_config::VDFStepsConfig, Address, Base64, Config, H256List, IrysBlockHeader,
    PartitionChunkOffset, PoaData, Signature, StorageConfig, TransactionLedger, VDFLimiterInfo,
    H256, U256,
};
use reth::builder::FullNode;
use reth::{revm::primitives::B256, tasks::TaskManager};
use reth_db::transaction::DbTx;
use reth_db::Database as _;
use tokio::sync::oneshot::{self};
use tokio::{task, time::sleep};
use tracing::info;

#[ignore]
#[actix::test]
async fn external_api() -> eyre::Result<()> {
    let temp_dir = setup_tracing_and_temp_dir(Some("external_api"), false);

    let mut node_config = IrysNodeConfig::default();
    node_config.base_directory = temp_dir.path().to_path_buf();
    let arc_config = Arc::new(node_config);

    let irys_db_env =
        open_or_create_irys_consensus_data_db(&arc_config.irys_consensus_data_dir()).unwrap();

    let testnet_config = Config::testnet();
    let storage_config = StorageConfig::new(&testnet_config);

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
                (partition_chunk_offset_ii!(0, 5), "sm1".into()), // 0 to 5 inclusive
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
                (partition_chunk_offset_ii!(0, 5), "sm2".into()), // 0 to 5 inclusive
            ],
        },
    ];

    let tmp_dir = setup_tracing_and_temp_dir(Some("chunk_migration_test"), false);
    let base_path = tmp_dir.path().to_path_buf();
    info!("temp_dir:{:?}\nbase_path:{:?}", tmp_dir, base_path);

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

    let (reth_handle_sender, reth_handle_receiver) =
        oneshot::channel::<FullNode<RethNode, RethNodeAddOns>>();

    let reth_node = RethNodeProvider(Arc::new(reth_handle_receiver.await.unwrap()));
    let reth_db = reth_node.provider.database.db.clone();
    let irys_db = DatabaseProvider(Arc::new(irys_db_env));
    let vdf_config = VDFStepsConfig::new(&testnet_config);

    let block_tree_service = BlockTreeService::from_registry();
    let block_tree_guard = block_tree_service
        .send(GetBlockTreeGuardMessage)
        .await
        .unwrap();

    // Create an instance of the mempool actor
    let mempool_service = MempoolService::new(
        irys_db.clone(),
        reth_db.clone(),
        reth_node.task_executor.clone(),
        node_config.mining_signer.clone(),
        storage_config.clone(),
        storage_modules.clone(),
        block_tree_guard.clone(),
        &testnet_config,
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

    let chunk_provider = ChunkProvider::new(storage_config.clone(), storage_modules.clone());

    let app_state = ApiState {
        reth_provider: None,
        block_index: None,
        block_tree: None,
        db: arc_db.clone(),
        mempool: mempool_addr.clone(),
        chunk_provider: Arc::new(chunk_provider),
        config: testnet_config,
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
    let irys_block = IrysBlockHeader::new_mock_header();

    // Send the block confirmed message
    let block = Arc::new(irys_block);
    let txs: Arc<Vec<irys_types::IrysTransactionHeader>> = Arc::new(tx_headers);
    let block_finalized_message = BlockFinalizedMessage {
        block_header: block.clone(),
        all_txs: Arc::clone(&txs),
    };

    block_index_addr.do_send(block_finalized_message.clone());

    let (service_senders, receivers) = ServiceSenders::new();
    // Send the block finalized message
    let chunk_migration_service = ChunkMigrationService::new(
        block_index.clone(),
        storage_config.clone(),
        storage_modules.clone(),
        arc_db.clone(),
        service_senders.clone(),
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
