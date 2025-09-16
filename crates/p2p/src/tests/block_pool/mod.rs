use crate::block_pool::{BlockPool, BlockPoolError};
use crate::chain_sync::{ChainSyncService, ChainSyncServiceInner};
use crate::peer_network_service::PeerNetworkService;
use crate::tests::util::{
    data_handler_stub, ApiClientStub, BlockDiscoveryStub, FakeGossipServer, MempoolStub,
    MockRethServiceActor,
};
use crate::types::GossipResponse;
use crate::{BlockStatusProvider, GetPeerListGuard};
use actix::Actor as _;
use irys_actors::services::ServiceSenders;
use irys_api_client::ApiClient;
use irys_domain::chain_sync_state::ChainSyncState;
use irys_domain::{ExecutionPayloadCache, PeerList, RethBlockProvider};
use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{
    AcceptedResponse, Address, BlockHash, BlockIndexItem, BlockIndexQuery, CombinedBlockHeader,
    Config, DataTransactionHeader, DatabaseProvider, GossipData, GossipDataRequest,
    IrysTransactionResponse, NodeConfig, NodeInfo, PeerAddress, PeerListItem, PeerNetworkSender,
    PeerResponse, PeerScore, VersionRequest, H256,
};
use irys_vdf::state::{VdfState, VdfStateReadonly};
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::channel;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::{debug, error};

#[derive(Clone, Default, Debug)]
struct MockApiClient {
    pub block_response: Option<CombinedBlockHeader>,
}

#[async_trait::async_trait]
impl ApiClient for MockApiClient {
    async fn get_transaction(
        &self,
        _peer: SocketAddr,
        _tx_id: H256,
    ) -> eyre::Result<IrysTransactionResponse> {
        Err(eyre::eyre!("not implemented"))
    }

    async fn post_transaction(
        &self,
        _peer: SocketAddr,
        _transaction: DataTransactionHeader,
    ) -> eyre::Result<()> {
        Ok(())
    }

    async fn get_transactions(
        &self,
        _peer: SocketAddr,
        _tx_ids: &[H256],
    ) -> eyre::Result<Vec<IrysTransactionResponse>> {
        Ok(vec![])
    }

    async fn post_version(
        &self,
        _peer: SocketAddr,
        _version: VersionRequest,
    ) -> eyre::Result<PeerResponse> {
        Ok(PeerResponse::Accepted(AcceptedResponse::default()))
    }

    async fn get_block_by_hash(
        &self,
        _peer: SocketAddr,
        _block_hash: BlockHash,
    ) -> Result<Option<CombinedBlockHeader>, eyre::Error> {
        Ok(self.block_response.clone())
    }

    async fn get_block_index(
        &self,
        _peer: SocketAddr,
        _block_index_query: BlockIndexQuery,
    ) -> eyre::Result<Vec<BlockIndexItem>> {
        Ok(vec![])
    }

    async fn node_info(&self, _peer: SocketAddr) -> eyre::Result<NodeInfo> {
        Ok(NodeInfo::default())
    }
}

fn create_test_config() -> Config {
    let temp_dir = setup_tracing_and_temp_dir(None, false);
    let mut node_config = NodeConfig::testing();
    node_config.base_directory = temp_dir.path().to_path_buf();
    node_config.trusted_peers = vec![];
    Config::new(node_config)
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
    async fn new(config: &Config) -> Self {
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&config.node_config.base_directory)
                .expect("can't open temp dir"),
        ));

        let mock_client = MockApiClient {
            block_response: None,
        };

        let block_status_provider_mock = BlockStatusProvider::mock(&config.node_config).await;

        let block_discovery_stub = BlockDiscoveryStub {
            blocks: Arc::new(RwLock::new(vec![])),
            block_status_provider: block_status_provider_mock.clone(),
            internal_message_bus: None,
        };
        let reth_service = MockRethServiceActor {};
        let reth_addr = reth_service.start();
        let (sender, receiver) = PeerNetworkSender::new_with_receiver();
        let peer_list_service = PeerNetworkService::new_with_custom_api_client(
            db.clone(),
            config,
            mock_client.clone(),
            reth_addr,
            receiver,
            sender,
        );
        let peer_service_addr = peer_list_service.start();
        let peer_list_data_guard = peer_service_addr
            .send(GetPeerListGuard)
            .await
            .expect("to get peer list")
            .expect("to get peer list");
        let execution_payload_provider =
            ExecutionPayloadCache::new(peer_list_data_guard.clone(), RethBlockProvider::new_mock());

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mempool_stub = MempoolStub::new(tx);

        let vdf_state_readonly =
            VdfStateReadonly::new(Arc::new(RwLock::new(VdfState::new(0, 0, None))));

        let (service_senders, service_receivers) = ServiceSenders::new();

        let mut vdf_receiver = service_receivers.vdf_fast_forward;
        let vdf_state = vdf_state_readonly;
        tokio::spawn(async move {
            loop {
                match vdf_receiver.recv().await {
                    Some(step) => {
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

        let mut block_tree_receiver = service_receivers.block_tree;

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

#[actix_rt::test]
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
    } = MockedServices::new(&config).await;

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
        config,
        service_senders,
    );

    let mock_chain = BlockStatusProvider::produce_mock_chain(2, None);
    let parent_block_header = mock_chain[0].clone();
    let test_header = mock_chain[1].clone();

    // Inserting parent block header to the db, so the current block should go to the
    //  block producer
    block_status_provider_mock.add_block_to_index_and_tree_for_testing(&parent_block_header);

    debug!("Previous block hash: {:?}", test_header.previous_block_hash);

    let test_header = Arc::new(test_header.clone());

    service
        .process_block::<ApiClientStub>(Arc::clone(&test_header), false)
        .await
        .expect("can't process block");

    let block_header_in_discovery = block_discovery_stub
        .get_blocks()
        .first()
        .expect("to have a block")
        .clone();
    assert_eq!(block_header_in_discovery, test_header);
}

#[actix_rt::test]
async fn should_process_block_with_intermediate_block_in_api() {
    let config = create_test_config();

    let gossip_server = FakeGossipServer::new();
    let (server_handle, fake_peer_gossip_addr) =
        gossip_server.run(SocketAddr::from(([127, 0, 0, 1], 0)));

    tokio::spawn(server_handle);

    // Wait for the server to start
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Create three blocks in a chain: block1 -> block2 -> block3
    // block1: in database
    // block2: in API client
    // block3: test block to be processed
    let test_chain = BlockStatusProvider::produce_mock_chain(3, None);

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

    let MockedServices {
        block_status_provider_mock,
        block_discovery_stub,
        peer_list_data_guard,
        db,
        execution_payload_provider,
        mempool_stub,
        service_senders,
        is_vdf_mining_enabled,
    } = MockedServices::new(&config).await;

    // Create a direct channel for the sync service
    let (sync_sender, sync_receiver) = tokio::sync::mpsc::unbounded_channel();

    let peer_list_guard = peer_list_data_guard.clone();
    // Set the mock client to return block2 when requested
    // Adding a peer so we can send a request to the mock client
    peer_list_guard.add_or_update_peer(
        Address::new([0, 1, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0]),
        PeerListItem {
            reputation_score: PeerScore::new(100),
            response_time: 0,
            address: PeerAddress {
                gossip: fake_peer_gossip_addr,
                ..PeerAddress::default()
            },
            last_seen: 0,
            is_online: true,
        },
        true,
    );

    let api_client_stub = MockApiClient {
        block_response: Some(CombinedBlockHeader {
            irys: block2.clone(),
            execution: Default::default(),
        }),
    };

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
    ));

    let data_handler = data_handler_stub(
        &config,
        &peer_list_guard,
        db.clone(),
        api_client_stub.clone(),
        sync_state.clone(),
    )
    .await;

    let sync_service_inner = ChainSyncServiceInner::new_with_client(
        sync_state.clone(),
        api_client_stub.clone(),
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
        GossipDataRequest::ExecutionPayload(_) => GossipResponse::Accepted(None),
        GossipDataRequest::Block(block_hash) => {
            let block = block_for_server.clone();
            let block_for_response = block.clone();
            let pool = pool_for_server.clone();
            debug!("Receive get block: {:?}", block_hash);
            tokio::spawn(async move {
                debug!("Send block to block pool");
                pool.process_block::<ApiClientStub>(Arc::new(block.clone()), false)
                    .await
                    .expect("to process block");
            });
            GossipResponse::Accepted(Some(GossipData::Block(Arc::new(block_for_response))))
        }
        GossipDataRequest::Chunk(_) => GossipResponse::Accepted(None),
    });

    let block2 = Arc::new(block2.clone());
    let block3 = Arc::new(block3.clone());

    // Insert block1 into the database
    block_status_provider_mock.add_block_to_index_and_tree_for_testing(&block1);

    // Process block3
    block_pool
        .process_block::<ApiClientStub>(Arc::clone(&block3), false)
        .await
        .expect("can't process block");

    // Wait for the block to be processed
    tokio::time::sleep(Duration::from_secs(1)).await;

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

#[actix_rt::test]
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
    } = MockedServices::new(&config).await;

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
        config,
        service_senders,
    );

    let mock_chain = BlockStatusProvider::produce_mock_chain(15, None);

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
        BlockStatusProvider::produce_mock_chain(1, old_blocks.get(1))[0].clone();

    debug!(
        "Sending bogus block: {:?}",
        header_building_on_very_old_block.block_hash
    );

    let res = block_pool
        .process_block::<ApiClientStub>(Arc::new(header_building_on_very_old_block.clone()), false)
        .await;

    assert!(res.is_err());
    assert!(matches!(res, Err(BlockPoolError::ForkedBlock(_))));
}

#[actix_rt::test]
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
    } = MockedServices::new(&config).await;

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
    peer_list_guard.add_or_update_peer(
        Address::new([0, 1, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0]),
        PeerListItem {
            reputation_score: PeerScore::new(100),
            response_time: 0,
            address: PeerAddress {
                gossip: fake_peer_gossip_addr,
                ..PeerAddress::default()
            },
            last_seen: 0,
            is_online: true,
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
    ));

    let api_client_stub = MockApiClient {
        block_response: None,
    };

    let data_handler = data_handler_stub(
        &config,
        &peer_list_guard,
        db.clone(),
        api_client_stub.clone(),
        sync_state.clone(),
    )
    .await;

    let sync_service_inner = ChainSyncServiceInner::new_with_client(
        sync_state.clone(),
        api_client_stub.clone(),
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

    let mock_chain = BlockStatusProvider::produce_mock_chain(15, None);

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
    let bogus_block =
        BlockStatusProvider::produce_mock_chain(1, old_blocks.get(bogus_block_parent_index))[0]
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
                let res = pool
                    .process_block::<ApiClientStub>(Arc::new(block.clone()), false)
                    .await;
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
    let res = block_pool
        .process_block::<ApiClientStub>(Arc::new(bogus_block), false)
        .await;

    sync_service_handle.shutdown_signal.fire();

    assert!(matches!(res, Err(BlockPoolError::ForkedBlock(_))));
}

#[actix_rt::test]
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
    } = MockedServices::new(&config).await;

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
        config,
        service_senders,
    );

    let mock_chain = BlockStatusProvider::produce_mock_chain(2, None);
    let parent_block_header = mock_chain[0].clone();
    let test_header = mock_chain[1].clone();

    // Inserting the parent block header to the db, so the current block should go to the
    //  block producer
    block_status_provider_mock.add_block_to_index_and_tree_for_testing(&parent_block_header);
    block_status_provider_mock.add_block_to_index_and_tree_for_testing(&test_header);

    debug!("Previous block hash: {:?}", test_header.previous_block_hash);

    let err = service
        .process_block::<ApiClientStub>(Arc::new(test_header.clone()), true)
        .await
        .expect_err("to have an error");

    let blocks_in_discovery = block_discovery_stub.get_blocks();
    // No blocks should be in discovery service, since we've fast tracked the block
    assert_eq!(blocks_in_discovery.len(), 0);

    assert_eq!(
        err,
        BlockPoolError::TryingToReprocessFinalizedBlock(test_header.block_hash)
    );
}
