use crate::block_pool_service::{BlockPoolService, ProcessBlock};
use crate::peer_list::{AddPeer, PeerListServiceWithClient};
use crate::tests::util::{FakeGossipServer, MockRethServiceActor};
use crate::SyncState;
use actix::Actor;
use async_trait::async_trait;
use base58::ToBase58;
use irys_actors::block_discovery::BlockDiscoveryFacade;
use irys_api_client::ApiClient;
use irys_database::db::IrysDatabaseExt as _;
use irys_database::{block_header_by_hash, insert_block_header};
use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{
    AcceptedResponse, Address, BlockHash, BlockIndexItem, BlockIndexQuery, CombinedBlockHeader,
    Config, DatabaseProvider, IrysBlockHeader, IrysTransactionHeader, IrysTransactionResponse,
    NodeConfig, PeerAddress, PeerListItem, PeerResponse, PeerScore, VersionRequest, H256,
};
use irys_vdf::state::test_helpers::mocked_vdf_service;
use irys_vdf::state::VdfStateReadonly;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tracing::debug;

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

#[derive(Clone)]
struct BlockDiscoveryStub {
    received_blocks: Arc<RwLock<Vec<IrysBlockHeader>>>,
    db: DatabaseProvider,
}

impl BlockDiscoveryStub {
    fn get_blocks(&self) -> Vec<IrysBlockHeader> {
        self.received_blocks.read().unwrap().clone()
    }
}

#[async_trait]
impl BlockDiscoveryFacade for BlockDiscoveryStub {
    async fn handle_block(&self, block: IrysBlockHeader) -> eyre::Result<()> {
        self.db.update_eyre(|tx| insert_block_header(tx, &block))?;
        self.received_blocks
            .write()
            .expect("to unlock blocks")
            .push(block);
        Ok(())
    }
}

#[actix_rt::test]
async fn should_process_block() {
    let temp_dir = setup_tracing_and_temp_dir(None, false);
    let mut node_config = NodeConfig::testnet();
    node_config.trusted_peers = vec![];
    let config = Config::new(node_config);

    let db = DatabaseProvider(Arc::new(
        open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
            .expect("can't open temp dir"),
    ));

    let mock_client = MockApiClient {
        block_response: None,
    };
    let block_discovery_stub = BlockDiscoveryStub {
        received_blocks: Arc::new(RwLock::new(vec![])),
        db: db.clone(),
    };
    let reth_service = MockRethServiceActor {};
    let reth_addr = reth_service.start();
    let peer_list_service = PeerListServiceWithClient::new_with_custom_api_client(
        db.clone(),
        &config,
        mock_client.clone(),
        reth_addr,
    );
    let peer_addr = peer_list_service.start();

    let (vdf_tx, _vdf_rx) = tokio::sync::mpsc::channel(1);
    let vdf_state = mocked_vdf_service(&config).await;
    let vdf_state_readonly = VdfStateReadonly::new(vdf_state.clone());
    let sync_state = SyncState::new(false);
    let service = BlockPoolService::new_with_client(
        db.clone(),
        peer_addr.into(),
        block_discovery_stub.clone(),
        Some(vdf_tx),
        sync_state,
        vdf_state_readonly,
    );
    let addr = service.start();

    let parent_block_header = IrysBlockHeader {
        block_hash: BlockHash::random(),
        height: 1,
        ..Default::default()
    };
    let parent_block_hash = parent_block_header.block_hash;

    let test_header = IrysBlockHeader {
        block_hash: BlockHash::random(),
        previous_block_hash: parent_block_hash,
        height: parent_block_header.height + 1,
        ..Default::default()
    };

    // Inserting parent block header to the db, so the current block should go to the
    //  block producer
    {
        db.update_eyre(|tx| insert_block_header(tx, &parent_block_header))
            .expect("to insert block");
        debug!(
            "Inserted parent block {:?} for block {:?}",
            parent_block_header.block_hash.0.to_base58(),
            test_header.block_hash.0.to_base58()
        );
    }

    let fetched_block = db
        .view_eyre(|tx| block_header_by_hash(tx, &parent_block_hash, false))
        .expect("to fetch a block")
        .expect("block should exist");

    debug!(
        "Block in the db: {}",
        fetched_block.block_hash.0.to_base58()
    );
    debug!(
        "Block previous_block_hash: {:?}",
        fetched_block.previous_block_hash.0.to_base58()
    );

    debug!(
        "Previous block hash: {:?}",
        test_header.previous_block_hash.0.to_base58()
    );

    addr.send(ProcessBlock {
        header: test_header.clone(),
    })
    .await
    .expect("can't send block")
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
    let temp_dir = setup_tracing_and_temp_dir(None, false);
    let mut node_config = NodeConfig::testnet();
    node_config.trusted_peers = vec![];
    let config = Config::new(node_config);

    let db = DatabaseProvider(Arc::new(
        open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
            .expect("can't open temp dir"),
    ));

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

    // Create block1 (will be in database)
    let block1 = IrysBlockHeader {
        block_hash: BlockHash::random(),
        ..Default::default()
    };

    let block1_hash = block1.block_hash;

    // Create block2 (will be in API client)
    let block2 = IrysBlockHeader {
        block_hash: BlockHash::random(),
        previous_block_hash: block1.block_hash,
        ..Default::default()
    };

    // Create block3 (test block)
    let block3 = IrysBlockHeader {
        block_hash: BlockHash::random(),
        previous_block_hash: block2.block_hash,
        ..Default::default()
    };

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

    // Setup MockApiClient to return block2 when queried
    let mock_client = MockApiClient::default();

    let block_discovery_stub = BlockDiscoveryStub {
        received_blocks: Arc::new(RwLock::new(vec![])),
        db: db.clone(),
    };
    let reth_service = MockRethServiceActor {};
    let reth_addr = reth_service.start();
    let peer_list_service = PeerListServiceWithClient::new_with_custom_api_client(
        db.clone(),
        &config,
        mock_client.clone(),
        reth_addr,
    );
    let peer_addr = peer_list_service.start();
    // Adding a peer so we can send a request to the mock client
    peer_addr
        .send(AddPeer {
            mining_addr: Address::new([0, 1, 3, 4, 5, 6, 7, 8, 9, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 0]),
            peer: PeerListItem {
                reputation_score: PeerScore::new(100),
                response_time: 0,
                address: PeerAddress {
                    gossip: fake_peer_gossip_addr,
                    ..PeerAddress::default()
                },
                last_seen: 0,
                is_online: true,
            },
        })
        .await
        .expect("can't send message to peer list");

    let (vdf_tx, _vdf_rx) = tokio::sync::mpsc::channel(1);
    let vdf_state = mocked_vdf_service(&config).await;
    let vdf_state_readonly = VdfStateReadonly::new(vdf_state.clone());
    let sync_state = SyncState::new(false);

    let service = BlockPoolService::new_with_client(
        db.clone(),
        peer_addr.into(),
        block_discovery_stub.clone(),
        Some(vdf_tx),
        sync_state,
        vdf_state_readonly,
    );
    let addr = service.start();

    // Set the fake server to mimic get_data -> gossip_service sends message to block pool
    let block_for_server = block2.clone();
    let addr_for_server = addr.clone();
    gossip_server.set_on_block_data_request(move |block_hash| {
        let block = block_for_server.clone();
        let addr = addr_for_server.clone();
        debug!("Receive get block: {:?}", block_hash.0.to_base58());
        tokio::spawn(async move {
            debug!("Send block to block pool");
            addr.send(ProcessBlock { header: block })
                .await
                .expect("to send message")
                .expect("to process block");
        });
        true
    });

    // Insert block1 into the database
    {
        db.update_eyre(|tx| insert_block_header(tx, &block1))
            .expect("to insert block1");
        debug!(
            "Inserted block1 {:?} into database",
            block1.block_hash.0.to_base58()
        );
    }

    // Verify block1 is in the database
    let fetched_block = db
        .view_eyre(|tx| block_header_by_hash(tx, &block1_hash, false))
        .expect("to fetch a block")
        .expect("block should exist");

    debug!(
        "Block in the db: {:?}",
        fetched_block.block_hash.0.to_base58()
    );
    debug!(
        "Block3 previous_block_hash: {:?}",
        block3.previous_block_hash.0.to_base58()
    );

    // Process block3
    addr.send(ProcessBlock {
        header: block3.clone(),
    })
    .await
    .expect("can't send block")
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
