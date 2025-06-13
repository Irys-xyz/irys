use crate::block_pool::{BlockPool, BlockPoolError};
use crate::peer_list::PeerListServiceWithClient;
use crate::tests::util::{FakeGossipServer, MockRethServiceActor};
use crate::{BlockStatusProvider, PeerList, SyncState};
use actix::{Actor, Addr};
use async_trait::async_trait;
use base58::ToBase58;
use irys_actors::block_discovery::BlockDiscoveryFacade;
use irys_api_client::ApiClient;
use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{
    AcceptedResponse, Address, BlockHash, BlockIndexItem, BlockIndexQuery, CombinedBlockHeader,
    Config, DatabaseProvider, IrysBlockHeader, IrysTransactionHeader, IrysTransactionResponse,
    NodeConfig, PeerAddress, PeerListItem, PeerResponse, PeerScore, VersionRequest, H256,
};
use std::net::SocketAddr;
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
        _transaction: IrysTransactionHeader,
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
}

fn create_test_config() -> Config {
    let temp_dir = setup_tracing_and_temp_dir(None, false);
    let mut node_config = NodeConfig::testnet();
    node_config.base_directory = temp_dir.path().to_path_buf();
    node_config.trusted_peers = vec![];
    Config::new(node_config)
}

#[derive(Clone)]
struct BlockDiscoveryStub {
    received_blocks: Arc<RwLock<Vec<IrysBlockHeader>>>,
    block_status_provider: BlockStatusProvider,
}

impl BlockDiscoveryStub {
    fn get_blocks(&self) -> Vec<IrysBlockHeader> {
        self.received_blocks.read().unwrap().clone()
    }
}

#[async_trait]
impl BlockDiscoveryFacade for BlockDiscoveryStub {
    async fn handle_block(&self, block: IrysBlockHeader) -> eyre::Result<()> {
        self.block_status_provider
            .add_block_to_index_and_tree_for_testing(&block);
        self.received_blocks
            .write()
            .expect("to unlock blocks")
            .push(block);
        Ok(())
    }
}

struct MockedServices {
    block_status_provider_mock: BlockStatusProvider,
    block_discovery_stub: BlockDiscoveryStub,
    peer_list_service_addr: Addr<PeerListServiceWithClient<MockApiClient, MockRethServiceActor>>,
    db: DatabaseProvider,
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
            received_blocks: Arc::new(RwLock::new(vec![])),
            block_status_provider: block_status_provider_mock.clone(),
        };
        let reth_service = MockRethServiceActor {};
        let reth_addr = reth_service.start();
        let peer_list_service = PeerListServiceWithClient::new_with_custom_api_client(
            db.clone(),
            config,
            mock_client.clone(),
            reth_addr,
        );
        let peer_addr = peer_list_service.start();

        Self {
            block_status_provider_mock,
            block_discovery_stub,
            peer_list_service_addr: peer_addr,
            db,
        }
    }
}

#[actix_rt::test]
async fn should_process_block() {
    let config = create_test_config();

    let MockedServices {
        block_status_provider_mock,
        block_discovery_stub,
        peer_list_service_addr: peer_addr,
        db,
    } = MockedServices::new(&config).await;

    let sync_state = SyncState::new(false);
    let service = BlockPool::new(
        db.clone(),
        peer_addr,
        block_discovery_stub.clone(),
        sync_state,
        block_status_provider_mock.clone(),
    );

    let mock_chain = BlockStatusProvider::produce_mock_chain(2, None);
    let parent_block_header = mock_chain[0].clone();
    let test_header = mock_chain[1].clone();

    // Inserting parent block header to the db, so the current block should go to the
    //  block producer
    block_status_provider_mock.add_block_to_index_and_tree_for_testing(&parent_block_header);

    debug!(
        "Previous block hash: {:?}",
        test_header.previous_block_hash.0.to_base58()
    );

    service
        .process_block(test_header.clone())
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
        peer_list_service_addr: peer_addr,
        db,
    } = MockedServices::new(&config).await;

    // Set the mock client to return block2 when requested
    // Adding a peer so we can send a request to the mock client
    peer_addr
        .add_peer(
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
        )
        .await
        .expect("can't send message to peer list");

    let sync_state = SyncState::new(false);

    let block_pool = BlockPool::new(
        db.clone(),
        peer_addr,
        block_discovery_stub.clone(),
        sync_state,
        block_status_provider_mock.clone(),
    );

    // Set the fake server to mimic get_data -> gossip_service sends message to block pool
    let block_for_server = block2.clone();
    let pool_for_server = block_pool.clone();
    gossip_server.set_on_block_data_request(move |block_hash| {
        let block = block_for_server.clone();
        let pool = pool_for_server.clone();
        debug!("Receive get block: {:?}", block_hash.0.to_base58());
        tokio::spawn(async move {
            debug!("Send block to block pool");
            pool.process_block(block.clone())
                .await
                .expect("to process block");
        });
        true
    });

    // Insert block1 into the database
    block_status_provider_mock.add_block_to_index_and_tree_for_testing(&block1);

    // Process block3
    block_pool
        .process_block(block3.clone())
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

    assert_eq!(discovered_block2, block2);
    assert_eq!(discovered_block3, block3);
}

#[actix_rt::test]
async fn should_warn_about_mismatches_for_very_old_block() {
    let config = create_test_config();

    let MockedServices {
        block_status_provider_mock,
        block_discovery_stub,
        peer_list_service_addr: peer_addr,
        db,
    } = MockedServices::new(&config).await;

    let sync_state = SyncState::new(false);
    let block_pool = BlockPool::new(
        db.clone(),
        peer_addr,
        block_discovery_stub.clone(),
        sync_state,
        block_status_provider_mock.clone(),
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
        .process_block(header_building_on_very_old_block.clone())
        .await;

    assert!(res.is_err());
    assert!(matches!(
        res,
        Err(BlockPoolError::TryingToReprocessFinalizedBlock(_))
    ));
}

#[actix_rt::test]
async fn should_refuse_fresh_block_trying_to_build_old_chain() {
    let config = create_test_config();

    let MockedServices {
        block_status_provider_mock,
        block_discovery_stub,
        peer_list_service_addr: peer_addr,
        db,
    } = MockedServices::new(&config).await;

    let gossip_server = FakeGossipServer::new();
    let (server_handle, fake_peer_gossip_addr) =
        gossip_server.run(SocketAddr::from(([127, 0, 0, 1], 0)));

    tokio::spawn(server_handle);

    // Wait for the server to start
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Adding a peer so we can send a request to the mock client
    peer_addr
        .add_peer(
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
        )
        .await
        .expect("can't send message to peer list");

    let sync_state = SyncState::new(false);
    let block_pool = BlockPool::new(
        db.clone(),
        peer_addr,
        block_discovery_stub.clone(),
        sync_state,
        block_status_provider_mock.clone(),
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

    // Fresh block that t
    let mut bogus_block =
        BlockStatusProvider::produce_mock_chain(1, old_blocks.get(bogus_block_parent_index))[0]
            .clone();
    bogus_block.height = 15;

    let oldest_block = block_status_provider_mock.oldest_tree_height();
    assert_eq!(oldest_block, 5);

    // Set the fake server to mimic get_data -> gossip_service sends message to block pool
    let block_pool_for_server = block_pool.clone();
    let blocks = mock_chain.clone();
    let (errors_sender, error_receiver) = channel::<BlockPoolError>();
    gossip_server.set_on_block_data_request(move |block_hash| {
        let block = blocks
            .iter()
            .find(|block| block.block_hash == block_hash)
            .cloned();
        let pool = block_pool_for_server.clone();
        debug!("Receive get block: {:?}", block_hash.0.to_base58());
        let errors_sender = errors_sender.clone();
        if let Some(block) = block {
            tokio::spawn(async move {
                debug!("Send block to block pool");
                let res = pool.process_block(block.clone()).await;
                if let Err(err) = res {
                    error!("Error processing block: {:?}", err);
                    errors_sender.send(err).unwrap();
                } else {
                    debug!("Block processed successfully");
                }
            });
            true
        } else {
            debug!("Block not found");
            false
        }
    });

    debug!("Sending bogus block: {:?}", bogus_block.block_hash);
    let res = block_pool.process_block(bogus_block).await;

    assert!(res.is_ok());
    let processing_error = error_receiver.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        processing_error,
        BlockPoolError::TryingToReprocessFinalizedBlock(_)
    ));
}
