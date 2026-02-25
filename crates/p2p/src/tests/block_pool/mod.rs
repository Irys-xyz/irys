use crate::block_pool::{BlockPool, BlockPoolError, CriticalBlockPoolError};
use crate::chain_sync::{ChainSyncService, ChainSyncServiceInner};
use crate::peer_network_service::spawn_peer_network_service_with_client;
use crate::tests::util::{
    data_handler_stub, data_handler_with_stubbed_pool, wait_for_block, BlockDiscoveryStub,
    FakeGossipServer, MempoolStub,
};
use crate::types::GossipResponse;
use crate::BlockStatusProvider;
use futures::{future, FutureExt as _};
use irys_actors::mempool_guard::MempoolReadGuard;
use irys_actors::mempool_service::{create_state, AtomicMempoolState};
use irys_actors::services::ServiceSenders;
use irys_domain::chain_sync_state::ChainSyncState;
use irys_domain::{ExecutionPayloadCache, PeerList, RethBlockProvider};
use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::v2::{GossipDataRequestV2, GossipDataV2};
use irys_types::{
    BlockBody, Config, DatabaseProvider, IrysAddress, IrysPeerId, MempoolConfig, NodeConfig,
    PeerAddress, PeerListItem, PeerNetworkSender, PeerScore, ProtocolVersion, RethPeerInfo,
    SealedBlock,
};
use irys_vdf::state::{VdfState, VdfStateReadonly};
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::{debug, error, info};

fn create_test_config() -> Config {
    let temp_dir = setup_tracing_and_temp_dir(None, false);
    let mut node_config = NodeConfig::testing();
    node_config.base_directory = temp_dir.path().to_path_buf();
    node_config.trusted_peers = vec![];
    Config::new_with_random_peer_id(node_config)
}

fn create_test_block_body(block_hash: irys_types::BlockHash) -> BlockBody {
    BlockBody {
        block_hash,
        ..Default::default()
    }
}

fn create_test_sealed_block(
    header: irys_types::IrysBlockHeader,
    body: BlockBody,
) -> Arc<SealedBlock> {
    Arc::new(SealedBlock::new(header, body).expect("Failed to create SealedBlock"))
}

struct MockedServices {
    block_status_provider_mock: BlockStatusProvider,
    block_discovery_stub: BlockDiscoveryStub,
    peer_list_data_guard: PeerList,
    db: DatabaseProvider,
    execution_payload_provider: ExecutionPayloadCache,
    mempool_stub: MempoolStub,
    service_senders: ServiceSenders,
    is_vdf_mining_enabled: Arc<AtomicBool>,
}

impl MockedServices {
    fn new(config: &Config) -> Self {
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&config.node_config.base_directory)
                .expect("can't open temp dir"),
        ));

        let block_status_provider_mock = BlockStatusProvider::mock(&config.node_config, db.clone());

        let block_discovery_stub = BlockDiscoveryStub {
            blocks: Arc::new(RwLock::new(vec![])),
            block_status_provider: block_status_provider_mock.clone(),
            internal_message_bus: None,
        };
        let (service_senders, service_receivers) =
            irys_actors::test_helpers::build_test_service_senders();
        let _reth_service_tx = service_senders.reth_service.clone();
        let mut vdf_receiver = service_receivers.vdf_fast_forward;
        let mut block_tree_receiver = service_receivers.block_tree;

        let (sender, receiver) = PeerNetworkSender::new_with_receiver();
        let runtime_handle = tokio::runtime::Handle::current();
        let reth_peer_sender = Arc::new(|peer_info: RethPeerInfo| {
            let _ = peer_info;
            future::ready(()).boxed()
        });

        let (_peer_network_handle, peer_list_data_guard) = spawn_peer_network_service_with_client(
            db.clone(),
            config,
            reth_peer_sender,
            receiver,
            sender,
            tokio::sync::broadcast::channel::<irys_domain::PeerEvent>(100).0,
            runtime_handle,
        );
        let execution_payload_provider =
            ExecutionPayloadCache::new(peer_list_data_guard.clone(), RethBlockProvider::new_mock());

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mempool_config = MempoolConfig::testing();
        let state = create_state(&mempool_config, &[]);
        let mempool_state = AtomicMempoolState::new(state);
        let mempool_stub = MempoolStub::new(tx, mempool_state);

        let vdf_state_readonly =
            VdfStateReadonly::new(Arc::new(RwLock::new(VdfState::new(0, 0, None))));

        let vdf_state = vdf_state_readonly;
        tokio::spawn(async move {
            loop {
                match vdf_receiver.recv().await {
                    Some(traced_step) => {
                        let (step, _parent_span) = traced_step.into_parts();
                        debug!("Received VDF step: {:?}", step);
                        let state = vdf_state.into_inner_cloned();
                        let mut lock = state.write().unwrap();
                        lock.global_step = step.global_step_number;
                    }
                    None => {
                        debug!("VDF receiver channel closed");
                        break;
                    }
                }
            }
        });

        tokio::spawn(async move {
            while let Some(message) = block_tree_receiver.recv().await {
                debug!("Received BlockTreeServiceMessage: {:?}", message);
            }
            debug!("BlockTreeServiceMessage channel closed");
        });

        Self {
            block_status_provider_mock,
            block_discovery_stub,
            peer_list_data_guard,
            db,
            execution_payload_provider,
            mempool_stub,
            service_senders,
            is_vdf_mining_enabled: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[tokio::test]
async fn should_process_block() {
    let config = create_test_config();

    let MockedServices {
        block_status_provider_mock,
        block_discovery_stub,
        peer_list_data_guard: _,
        db,
        execution_payload_provider,
        mempool_stub,
        service_senders,
        is_vdf_mining_enabled: _,
    } = MockedServices::new(&config);

    // Create a direct channel for the sync service
    let (sync_sender, _sync_receiver) = tokio::sync::mpsc::unbounded_channel();

    let sync_state = ChainSyncState::new(false, false);
    let service = BlockPool::new(
        db.clone(),
        block_discovery_stub.clone(),
        mempool_stub.clone(),
        sync_sender,
        sync_state,
        block_status_provider_mock.clone(),
        execution_payload_provider.clone(),
        config.clone(),
        service_senders,
        MempoolReadGuard::stub(),
    );

    let genesis = block_status_provider_mock.genesis_header();
    let mock_chain = BlockStatusProvider::produce_mock_chain(2, Some(&genesis), &config.consensus);
    let parent_block_header = mock_chain[0].clone();
    let test_header = mock_chain[1].clone();

    // Inserting parent block header to the db, so the current block should go to the
    //  block producer
    block_status_provider_mock.add_block_to_index_and_tree_for_testing(&parent_block_header);

    debug!("Previous block hash: {:?}", test_header.previous_block_hash);

    let sealed_block = create_test_sealed_block(
        test_header.clone(),
        create_test_block_body(test_header.block_hash),
    );

    service
        .process_block(sealed_block, false)
        .await
        .expect("can't process block");

    let block_header_in_discovery = block_discovery_stub
        .get_blocks()
        .first()
        .expect("to have a block")
        .clone();
    assert_eq!(*block_header_in_discovery, test_header);
}

#[tokio::test]
async fn should_process_block_with_intermediate_block_in_api() {
    let config = create_test_config();

    let gossip_server = FakeGossipServer::new();
    let (server_handle, fake_peer_gossip_addr) =
        gossip_server.run(SocketAddr::from(([127, 0, 0, 1], 0)));

    tokio::spawn(server_handle);

    // Wait for the server to start
    tokio::time::sleep(Duration::from_secs(5)).await;

    let MockedServices {
        block_status_provider_mock,
        block_discovery_stub,
        peer_list_data_guard,
        db,
        execution_payload_provider,
        mempool_stub,
        service_senders,
        is_vdf_mining_enabled,
    } = MockedServices::new(&config);

    // Create three blocks in a chain: block1 -> block2 -> block3
    // block1: in database
    // block2: in API client
    // block3: test block to be processed
    let genesis = block_status_provider_mock.genesis_header();
    let test_chain = BlockStatusProvider::produce_mock_chain(3, Some(&genesis), &config.consensus);

    // Create block1 (will be in the database)
    let block1 = test_chain[0].clone();
    // Create block2 (will be in the API client)
    let block2 = test_chain[1].clone();
    // Create block3 (test block)
    let block3 = test_chain[2].clone();

    debug!("Block 1: {:?}", block1.block_hash);
    debug!("Block 2: {:?}", block2.block_hash);
    debug!("Block 3: {:?}", block3.block_hash);
    debug!(
        "Block 1 previous_block_hash: {:?}",
        block1.previous_block_hash
    );
    debug!(
        "Block 2 previous_block_hash: {:?}",
        block2.previous_block_hash
    );
    debug!(
        "Block 3 previous_block_hash: {:?}",
        block3.previous_block_hash
    );

    // Create a direct channel for the sync service
    let (sync_sender, sync_receiver) = tokio::sync::mpsc::unbounded_channel();

    let peer_list_guard = peer_list_data_guard.clone();
    // Set the mock client to return block2 when requested
    // Adding a peer so we can send a request to the mock client
    let fake_mining_addr2 =
        IrysAddress::new([0, 1, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 1]);
    peer_list_guard.add_or_update_peer(
        PeerListItem {
            peer_id: IrysPeerId::from(fake_mining_addr2),
            mining_address: fake_mining_addr2,
            reputation_score: PeerScore::new(100),
            response_time: 0,
            address: PeerAddress {
                gossip: fake_peer_gossip_addr,
                ..PeerAddress::default()
            },
            last_seen: 0,
            is_online: true,
            protocol_version: ProtocolVersion::default(),
        },
        true,
    );

    let sync_state = ChainSyncState::new(false, false);

    let block_pool = Arc::new(BlockPool::new(
        db.clone(),
        block_discovery_stub.clone(),
        mempool_stub.clone(),
        sync_sender,
        sync_state.clone(),
        block_status_provider_mock.clone(),
        execution_payload_provider.clone(),
        config.clone(),
        service_senders,
        MempoolReadGuard::stub(),
    ));

    let data_handler = data_handler_stub(&config, &peer_list_guard, db.clone(), sync_state.clone());

    let sync_service_inner = ChainSyncServiceInner::new(
        sync_state.clone(),
        peer_list_guard.clone(),
        config.clone(),
        block_status_provider_mock.block_index(),
        block_pool.clone(),
        data_handler,
        None,
        is_vdf_mining_enabled,
    );

    let sync_service_handle = ChainSyncService::spawn_service(
        sync_service_inner,
        sync_receiver,
        tokio::runtime::Handle::current(),
    );

    // Set the fake server to mimic get_data -> gossip_service sends a message to the block pool
    let block_for_server = block2.clone();
    let pool_for_server = block_pool.clone();
    gossip_server.set_on_pull_data_request(move |data_request| match data_request {
        GossipDataRequestV2::ExecutionPayload(_) => GossipResponse::Accepted(None),
        GossipDataRequestV2::BlockHeader(block_hash) => {
            let block = block_for_server.clone();
            let block_for_response = block.clone();
            let pool = pool_for_server.clone();
            debug!("Receive get block: {:?}", block_hash);
            tokio::spawn(async move {
                debug!("Send block to block pool");
                let sealed_block = create_test_sealed_block(
                    block.clone(),
                    create_test_block_body(block.block_hash),
                );
                pool.process_block(sealed_block, false)
                    .await
                    .expect("to process block");
            });
            GossipResponse::Accepted(Some(GossipDataV2::BlockHeader(Arc::new(
                block_for_response,
            ))))
        }
        GossipDataRequestV2::Chunk(_) => GossipResponse::Accepted(None),
        GossipDataRequestV2::BlockBody(_) => GossipResponse::Accepted(None),
        GossipDataRequestV2::Transaction(_) => GossipResponse::Accepted(None),
    });

    let block2 = Arc::new(block2.clone());
    let block3 = Arc::new(block3.clone());

    // Insert block1 into the database
    block_status_provider_mock.add_block_to_index_and_tree_for_testing(&block1);

    // Process block3
    let block3_sealed =
        create_test_sealed_block((*block3).clone(), create_test_block_body(block3.block_hash));

    block_pool
        .process_block(block3_sealed, false)
        .await
        .expect("can't process block");

    // Wait for the block to be processed
    wait_for_block(&sync_state, block3.height as usize, Duration::from_secs(5)).await;

    // The blocks should be received in order of processing: first block2, then block3
    let discovered_block2 = block_discovery_stub.get_blocks().first().unwrap().clone();
    let discovered_block3 = block_discovery_stub
        .get_blocks()
        .get(1)
        .expect("to get block3 message")
        .clone();

    sync_service_handle.shutdown_signal.fire();

    assert_eq!(discovered_block2, block2);
    assert_eq!(discovered_block3, block3);
}

#[tokio::test]
async fn heavy_should_reprocess_block_again_if_processing_its_parent_failed_when_new_block_arrives()
{
    let config = create_test_config();

    let gossip_server = FakeGossipServer::new();
    let (server_handle, fake_peer_gossip_addr) =
        gossip_server.run(SocketAddr::from(([127, 0, 0, 1], 0)));

    tokio::spawn(server_handle);

    // Wait for the server to start
    tokio::time::sleep(Duration::from_secs(5)).await;

    let MockedServices {
        block_status_provider_mock,
        block_discovery_stub,
        peer_list_data_guard,
        db,
        execution_payload_provider,
        mempool_stub,
        service_senders,
        is_vdf_mining_enabled,
    } = MockedServices::new(&config);

    // Create four blocks in a chain: block1 -> block2 -> block3 -> block4
    // block1: in database
    // block2: in API client, but fails. It doesn't make it all the way to the block pool
    // block3: test block to be processed - getting stuck when processing block2
    // block4: new block that should trigger reprocessing of block2 and then block3
    let genesis = block_status_provider_mock.genesis_header();
    let test_chain = BlockStatusProvider::produce_mock_chain(4, Some(&genesis), &config.consensus);

    // Create block1 (will be in the database)
    let block1 = test_chain[0].clone();
    // Create block2 (will be in the API client)
    let block2 = test_chain[1].clone();
    // Create block3 (test block)
    let block3 = test_chain[2].clone();
    // Create block4 (new block that should trigger reprocessing of block2 and then block3)
    let block4 = test_chain[3].clone();

    debug!("Block 1: {:?}", block1.block_hash);
    debug!("Block 2: {:?}", block2.block_hash);
    debug!("Block 3: {:?}", block3.block_hash);
    debug!("Block 4: {:?}", block4.block_hash);
    debug!(
        "Block 1 previous_block_hash: {:?}",
        block1.previous_block_hash
    );
    debug!(
        "Block 2 previous_block_hash: {:?}",
        block2.previous_block_hash
    );
    debug!(
        "Block 3 previous_block_hash: {:?}",
        block3.previous_block_hash
    );
    debug!(
        "Block 4 previous_block_hash: {:?}",
        block4.previous_block_hash
    );

    // Create a direct channel for the sync service
    let (sync_sender, sync_receiver) = tokio::sync::mpsc::unbounded_channel();

    let peer_list_guard = peer_list_data_guard.clone();
    // Set the mock client to return block2 when requested
    // Adding a peer so we can send a request to the mock client
    let fake_mining_addr2 =
        IrysAddress::new([0, 1, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 1]);
    peer_list_guard.add_or_update_peer(
        PeerListItem {
            peer_id: IrysPeerId::from(fake_mining_addr2),
            mining_address: fake_mining_addr2,
            reputation_score: PeerScore::new(100),
            response_time: 0,
            address: PeerAddress {
                gossip: fake_peer_gossip_addr,
                ..PeerAddress::default()
            },
            last_seen: 0,
            is_online: true,
            protocol_version: ProtocolVersion::default(),
        },
        true,
    );

    let sync_state = ChainSyncState::new(false, false);

    let block_pool = Arc::new(BlockPool::new(
        db.clone(),
        block_discovery_stub.clone(),
        mempool_stub.clone(),
        sync_sender,
        sync_state.clone(),
        block_status_provider_mock.clone(),
        execution_payload_provider.clone(),
        config.clone(),
        service_senders,
        MempoolReadGuard::stub(),
    ));

    let data_handler = data_handler_with_stubbed_pool(
        &peer_list_guard,
        sync_state.clone(),
        block_pool.clone(),
        &config,
        db.clone(),
    );

    let sync_service_inner = ChainSyncServiceInner::new(
        sync_state.clone(),
        peer_list_guard.clone(),
        config.clone(),
        block_status_provider_mock.block_index(),
        block_pool.clone(),
        data_handler,
        None,
        is_vdf_mining_enabled,
    );

    let sync_service_handle = ChainSyncService::spawn_service(
        sync_service_inner,
        sync_receiver,
        tokio::runtime::Handle::current(),
    );

    // Set the fake server to mimic get_data -> gossip_service sends a message to the block pool
    let block_for_server = Arc::new(RwLock::new(None));
    let block_for_server_clone = block_for_server.clone();
    gossip_server.set_on_pull_data_request(move |data_request| match data_request {
        GossipDataRequestV2::ExecutionPayload(_) => GossipResponse::Accepted(None),
        GossipDataRequestV2::BlockHeader(block_hash) => {
            debug!("Received a request to pull the block: {:?}", block_hash);
            let block_for_server = block_for_server_clone
                .read()
                .unwrap()
                .clone()
                .map(|b| GossipDataV2::BlockHeader(Arc::new(b)));
            GossipResponse::Accepted(block_for_server)
        }
        GossipDataRequestV2::Chunk(_) => GossipResponse::Accepted(None),
        GossipDataRequestV2::BlockBody(hash) => GossipResponse::Accepted(Some(
            GossipDataV2::BlockBody(Arc::new(create_test_block_body(hash))),
        )),
        GossipDataRequestV2::Transaction(_) => GossipResponse::Accepted(None),
    });

    let block2 = Arc::new(block2.clone());
    let block3 = Arc::new(block3.clone());
    let block4 = Arc::new(block4.clone());

    info!("Block1 hash: {:?}", block1.block_hash);
    // Insert block1 into the database
    block_status_provider_mock.add_block_to_index_and_tree_for_testing(&block1);

    // Process block3
    let block3_sealed =
        create_test_sealed_block((*block3).clone(), create_test_block_body(block3.block_hash));

    block_pool
        .process_block(block3_sealed, false)
        .await
        .expect("can't process block");

    // Wait a little for processing - there's no specific event we're waiting for here
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Couldn't fetch block2, so block3 is not processed
    assert!(block_discovery_stub.get_blocks().is_empty());
    assert!(!block_pool.contains_block(&block2.block_hash).await);
    assert!(block_pool.contains_block(&block3.block_hash).await);
    // Assert that both blocks are not marked as processing
    assert!(!block_pool.is_block_processing(&block2.block_hash).await);
    assert!(!block_pool.is_block_processing(&block3.block_hash).await);

    info!("Adding block2 to the server and processing block4 to trigger reprocessing");
    // Add a previously missing block to the server
    *block_for_server.write().unwrap() = Some(block2.as_ref().clone());
    // Process block4 to trigger reprocessing of block2 and then block3
    let block4_sealed =
        create_test_sealed_block((*block4).clone(), create_test_block_body(block4.block_hash));

    block_pool
        .process_block(block4_sealed, false)
        .await
        .expect("can't process block");

    // Wait for the block to be processed
    wait_for_block(&sync_state, block4.height as usize, Duration::from_secs(5)).await;

    let discovered_block2 = block_discovery_stub.get_blocks().first().unwrap().clone();
    let discovered_block3 = block_discovery_stub
        .get_blocks()
        .get(1)
        .expect("to get block3 message")
        .clone();
    let discovered_block4 = block_discovery_stub
        .get_blocks()
        .get(2)
        .expect("to get block4 message")
        .clone();

    assert_eq!(discovered_block2, block2);
    assert_eq!(discovered_block3, block3);
    assert_eq!(discovered_block4, block4);

    sync_service_handle.shutdown_signal.fire();
}

#[tokio::test]
async fn should_warn_about_mismatches_for_very_old_block() {
    let config = create_test_config();

    let MockedServices {
        block_status_provider_mock,
        block_discovery_stub,
        peer_list_data_guard: _,
        db,
        execution_payload_provider,
        mempool_stub,
        service_senders,
        is_vdf_mining_enabled: _,
    } = MockedServices::new(&config);

    // Create a direct channel for the sync service
    let (sync_sender, _sync_receiver) = tokio::sync::mpsc::unbounded_channel();

    let sync_state = ChainSyncState::new(false, false);

    let block_pool = BlockPool::new(
        db.clone(),
        block_discovery_stub.clone(),
        mempool_stub.clone(),
        sync_sender,
        sync_state,
        block_status_provider_mock.clone(),
        execution_payload_provider,
        config.clone(),
        service_senders,
        MempoolReadGuard::stub(),
    );

    let genesis = block_status_provider_mock.genesis_header();
    let mock_chain = BlockStatusProvider::produce_mock_chain(15, Some(&genesis), &config.consensus);

    // Test case: 5 older blocks are in the index, but pruned from the tree;
    // 5 newer blocks are in the tree and in the index
    // 5 newest blocks are in the tree, but not in the index
    let old_blocks = mock_chain[0..5].to_vec();
    let middle_blocks = mock_chain[5..10].to_vec();
    let new_blocks = mock_chain[10..15].to_vec();

    for block in old_blocks.iter() {
        block_status_provider_mock.add_block_to_index_and_tree_for_testing(block);
    }

    for block in middle_blocks.iter() {
        block_status_provider_mock.add_block_to_index_and_tree_for_testing(block);
    }

    for block in new_blocks.iter() {
        block_status_provider_mock.add_block_mock_to_the_tree(block);
    }

    block_status_provider_mock.set_tip_for_testing(&new_blocks.last().as_ref().unwrap().block_hash);
    // Prune everything older than the 10th block
    block_status_provider_mock.delete_mocked_blocks_older_than(10);

    let header_building_on_very_old_block =
        BlockStatusProvider::produce_mock_chain(1, old_blocks.get(1), &config.consensus)[0].clone();

    debug!(
        "Sending bogus block: {:?}",
        header_building_on_very_old_block.block_hash
    );

    let sealed_block = create_test_sealed_block(
        header_building_on_very_old_block.clone(),
        create_test_block_body(header_building_on_very_old_block.block_hash),
    );

    let res = block_pool.process_block(sealed_block, false).await;

    assert!(res.is_err());
    assert!(matches!(
        res,
        Err(BlockPoolError::Critical(
            CriticalBlockPoolError::ForkedBlock(_)
        ))
    ));
}

#[tokio::test]
async fn should_refuse_fresh_block_trying_to_build_old_chain() {
    let config = create_test_config();

    let MockedServices {
        block_status_provider_mock,
        block_discovery_stub,
        peer_list_data_guard,
        db,
        execution_payload_provider,
        mempool_stub,
        service_senders,
        is_vdf_mining_enabled,
    } = MockedServices::new(&config);

    // Create a direct channel for the sync service
    let (sync_sender, sync_receiver) = tokio::sync::mpsc::unbounded_channel();

    let gossip_server = FakeGossipServer::new();
    let (server_handle, fake_peer_gossip_addr) =
        gossip_server.run(SocketAddr::from(([127, 0, 0, 1], 0)));

    tokio::spawn(server_handle);

    // Wait for the server to start
    tokio::time::sleep(Duration::from_secs(5)).await;

    let peer_list_guard = peer_list_data_guard.clone();

    // Adding a peer so we can send a request to the mock client
    let fake_mining_addr3 =
        IrysAddress::new([0, 1, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0]);
    peer_list_guard.add_or_update_peer(
        PeerListItem {
            peer_id: IrysPeerId::from(fake_mining_addr3),
            mining_address: fake_mining_addr3,
            reputation_score: PeerScore::new(100),
            response_time: 0,
            address: PeerAddress {
                gossip: fake_peer_gossip_addr,
                ..PeerAddress::default()
            },
            last_seen: 0,
            is_online: true,
            protocol_version: ProtocolVersion::default(),
        },
        true,
    );

    let sync_state = ChainSyncState::new(false, false);

    let block_pool = Arc::new(BlockPool::new(
        db.clone(),
        block_discovery_stub.clone(),
        mempool_stub,
        sync_sender,
        sync_state.clone(),
        block_status_provider_mock.clone(),
        execution_payload_provider.clone(),
        config.clone(),
        service_senders,
        MempoolReadGuard::stub(),
    ));

    let data_handler = data_handler_stub(&config, &peer_list_guard, db.clone(), sync_state.clone());

    let sync_service_inner = ChainSyncServiceInner::new(
        sync_state.clone(),
        peer_list_guard.clone(),
        config.clone(),
        block_status_provider_mock.block_index(),
        block_pool.clone(),
        data_handler,
        None,
        is_vdf_mining_enabled,
    );

    let sync_service_handle = ChainSyncService::spawn_service(
        sync_service_inner,
        sync_receiver,
        tokio::runtime::Handle::current(),
    );

    let genesis = block_status_provider_mock.genesis_header();
    let mock_chain = BlockStatusProvider::produce_mock_chain(15, Some(&genesis), &config.consensus);

    // Test case: 5 older blocks are in the index, but pruned from the tree;
    // 5 newer blocks are in the tree and in the index
    // 5 newest blocks are in the tree, but not in the index
    let old_blocks = mock_chain[0..5].to_vec();
    let middle_blocks = mock_chain[5..10].to_vec();
    let new_blocks = mock_chain[10..15].to_vec();

    for block in old_blocks.iter() {
        block_status_provider_mock.add_block_to_index_and_tree_for_testing(block);
    }

    for block in middle_blocks.iter() {
        block_status_provider_mock.add_block_to_index_and_tree_for_testing(block);
    }

    for block in new_blocks.iter() {
        block_status_provider_mock.add_block_mock_to_the_tree(block);
    }

    block_status_provider_mock.set_tip_for_testing(&new_blocks.last().as_ref().unwrap().block_hash);
    // Prune everything older than the 10th block
    block_status_provider_mock.delete_mocked_blocks_older_than(5);

    let bogus_block_parent_index = 1;

    // Fresh block that is trying to build on the old chain
    let bogus_block = BlockStatusProvider::produce_mock_chain(
        1,
        old_blocks.get(bogus_block_parent_index),
        &config.consensus,
    )[0]
    .clone();

    let oldest_block = block_status_provider_mock.oldest_tree_height();
    assert_eq!(oldest_block, 5);

    // Set the fake server to mimic get_data -> gossip_service sends a message to block pool
    let block_pool_for_server = block_pool.clone();
    let blocks = mock_chain.clone();
    let (errors_sender, _error_receiver) = channel::<BlockPoolError>();
    gossip_server.set_on_block_data_request(move |block_hash| {
        let block = blocks
            .iter()
            .find(|block| block.block_hash == block_hash)
            .cloned();
        let pool = block_pool_for_server.clone();
        debug!("Receive get block: {:?}", block_hash);
        let errors_sender = errors_sender.clone();
        if let Some(block) = block {
            tokio::spawn(async move {
                debug!("Send block to block pool");
                let sealed_block = create_test_sealed_block(
                    block.clone(),
                    create_test_block_body(block.block_hash),
                );
                let res = pool.process_block(sealed_block, false).await;
                if let Err(err) = res {
                    error!("Error processing block: {:?}", err);
                    errors_sender.send(err).unwrap();
                } else {
                    debug!("Block processed successfully");
                }
            });
            GossipResponse::Accepted(true)
        } else {
            debug!("Block not found");
            GossipResponse::Accepted(false)
        }
    });

    tokio::time::sleep(Duration::from_secs(1)).await;
    let is_parent_in_the_tree =
        block_status_provider_mock.is_block_in_the_tree(&bogus_block.previous_block_hash);
    let is_parent_in_index = block_status_provider_mock.is_height_in_the_index(1);
    assert!(!is_parent_in_the_tree);
    assert!(is_parent_in_index);

    debug!("Sending bogus block: {:?}", bogus_block.block_hash);
    let sealed_block = create_test_sealed_block(
        bogus_block.clone(),
        create_test_block_body(bogus_block.block_hash),
    );
    let res = block_pool.process_block(sealed_block, false).await;

    sync_service_handle.shutdown_signal.fire();

    assert!(matches!(
        res,
        Err(BlockPoolError::Critical(
            CriticalBlockPoolError::ForkedBlock(_)
        ))
    ));
}

#[tokio::test]
async fn should_not_fast_track_block_already_in_index() {
    let config = create_test_config();

    let MockedServices {
        block_status_provider_mock,
        block_discovery_stub,
        peer_list_data_guard: _,
        db,
        execution_payload_provider,
        mempool_stub,
        service_senders,
        is_vdf_mining_enabled: _,
    } = MockedServices::new(&config);

    // Create a direct channel for the sync service
    let (sync_sender, _sync_receiver) = tokio::sync::mpsc::unbounded_channel();

    let sync_state = ChainSyncState::new(false, true);

    let service = BlockPool::new(
        db.clone(),
        block_discovery_stub.clone(),
        mempool_stub.clone(),
        sync_sender,
        sync_state,
        block_status_provider_mock.clone(),
        execution_payload_provider.clone(),
        config.clone(),
        service_senders,
        MempoolReadGuard::stub(),
    );

    let genesis = block_status_provider_mock.genesis_header();
    let mock_chain = BlockStatusProvider::produce_mock_chain(2, Some(&genesis), &config.consensus);
    let parent_block_header = mock_chain[0].clone();
    let test_header = mock_chain[1].clone();

    // Inserting the parent block header to the db, so the current block should go to the
    //  block producer
    block_status_provider_mock.add_block_to_index_and_tree_for_testing(&parent_block_header);
    block_status_provider_mock.add_block_to_index_and_tree_for_testing(&test_header);

    debug!("Previous block hash: {:?}", test_header.previous_block_hash);

    let sealed_block = create_test_sealed_block(
        test_header.clone(),
        create_test_block_body(test_header.block_hash),
    );

    let err = service
        .process_block(sealed_block, true)
        .await
        .expect_err("to have an error");

    let blocks_in_discovery = block_discovery_stub.get_blocks();
    // No blocks should be in discovery service, since we've fast tracked the block
    assert_eq!(blocks_in_discovery.len(), 0);

    assert_eq!(
        err,
        BlockPoolError::Critical(CriticalBlockPoolError::TryingToReprocessFinalizedBlock(
            test_header.block_hash
        ))
    );
}
