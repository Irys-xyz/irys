use crate::util::MockRethServiceActor;
use actix::{Actor, Handler, Message};
use base58::ToBase58;
use irys_actors::block_discovery::BlockDiscoveredMessage;
use irys_actors::peer_list_service::{AddPeer, PeerListServiceWithClient};
use irys_api_client::ApiClient;
use irys_database::reth_db::Database;
use irys_database::{block_header_by_hash, insert_block_header};
use irys_gossip_service::block_pool_service::{BlockPoolService, ProcessBlock};
use irys_gossip_service::GossipClient;
use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
use irys_testing_utils::utils::setup_tracing_and_temp_dir;
use irys_types::{
    AcceptedResponse, Address, BlockHash, CombinedBlockHeader, Config, DatabaseProvider,
    IrysBlockHeader, IrysTransactionHeader, NodeConfig, PeerListItem, PeerResponse, PeerScore,
    VersionRequest, H256,
};
use std::net::SocketAddr;
use std::sync::Arc;
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
    ) -> eyre::Result<Option<IrysTransactionHeader>> {
        Ok(None)
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
    ) -> eyre::Result<Vec<Option<IrysTransactionHeader>>> {
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
}

struct BlockDiscoveryMockActor {
    messages: Vec<BlockDiscoveredMessage>,
    db: DatabaseProvider,
}

impl Actor for BlockDiscoveryMockActor {
    type Context = actix::Context<Self>;
}

impl Handler<BlockDiscoveredMessage> for BlockDiscoveryMockActor {
    type Result = eyre::Result<()>;

    fn handle(&mut self, msg: BlockDiscoveredMessage, _ctx: &mut Self::Context) -> Self::Result {
        self.messages.push(msg.clone());
        self.db.update_eyre(|tx| insert_block_header(tx, &msg.0))?;
        Ok(())
    }
}

#[derive(Message, Debug, Clone)]
#[rtype(result = "Vec<BlockDiscoveredMessage>")]
struct GetBlockDiscoveryMessages;

impl Handler<GetBlockDiscoveryMessages> for BlockDiscoveryMockActor {
    type Result = Vec<BlockDiscoveredMessage>;

    fn handle(
        &mut self,
        _msg: GetBlockDiscoveryMessages,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        self.messages.clone()
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
    let mock_block_discovery_actor = BlockDiscoveryMockActor {
        messages: vec![],
        db: db.clone(),
    };
    let block_discovery_addr = mock_block_discovery_actor.start();
    let reth_service = MockRethServiceActor {};
    let reth_addr = reth_service.start();
    let peer_list_service = PeerListServiceWithClient::new_with_custom_api_client(
        db.clone(),
        &config,
        mock_client.clone(),
        reth_addr,
    );
    let peer_addr = peer_list_service.start();

    let service = BlockPoolService::new_with_client(
        db.clone(),
        mock_client,
        peer_addr.into(),
        block_discovery_addr.clone(),
        GossipClient::new(Duration::from_secs(5), Address::default()),
    );
    let addr = service.start();

    let mut parent_block_header = IrysBlockHeader::default();
    parent_block_header.block_hash = BlockHash::random();
    let parent_block_hash = parent_block_header.block_hash;

    let mut test_header = IrysBlockHeader::default();
    test_header.block_hash = BlockHash::random();
    test_header.previous_block_hash = parent_block_header.block_hash;

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

    let block_messages = block_discovery_addr
        .send(GetBlockDiscoveryMessages)
        .await
        .expect("to get block messages");
    let block_header_in_discovery = block_messages
        .get(0)
        .expect("to get block message")
        .0
        .as_ref()
        .clone();
    assert_eq!(block_messages.len(), 1);
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

    // Create three blocks in a chain: block1 -> block2 -> block3
    // block1: in database
    // block2: in API client
    // block3: test block to be processed

    // Create block1 (will be in database)
    let mut block1 = IrysBlockHeader::default();
    block1.block_hash = BlockHash::random();
    let block1_hash = block1.block_hash;

    // Create block2 (will be in API client)
    let mut block2 = IrysBlockHeader::default();
    block2.block_hash = BlockHash::random();
    block2.previous_block_hash = block1.block_hash;

    // Create a combined block header for block2 to be returned by the API client
    let combined_block2 = CombinedBlockHeader {
        irys: block2.clone(),
        execution: Default::default(),
    };

    // Create block3 (test block)
    let mut block3 = IrysBlockHeader::default();
    block3.block_hash = BlockHash::random();
    block3.previous_block_hash = block2.block_hash;

    debug!("Block 1: {:?}", block1.block_hash.0.to_base58());
    debug!("Block 2: {:?}", block2.block_hash.0.to_base58());
    debug!("Block 3: {:?}", block3.block_hash.0.to_base58());
    debug!(
        "Block 1 previous_block_hash: {:?}",
        block1.previous_block_hash.0.to_base58()
    );
    debug!(
        "Block 2 previous_block_hash: {:?}",
        block2.previous_block_hash.0.to_base58()
    );
    debug!(
        "Block 3 previous_block_hash: {:?}",
        block3.previous_block_hash.0.to_base58()
    );

    // Setup MockApiClient to return block2 when queried
    let mock_client = MockApiClient {
        block_response: Some(combined_block2),
    };

    let mock_block_discovery_actor = BlockDiscoveryMockActor {
        messages: vec![],
        db: db.clone(),
    };
    let block_discovery_addr = mock_block_discovery_actor.start();
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
            mining_addr: Default::default(),
            peer: PeerListItem {
                reputation_score: PeerScore::new(100),
                response_time: 0,
                address: Default::default(),
                last_seen: 0,
                is_online: true,
            },
        })
        .await
        .expect("can't send message to peer list");

    let service = BlockPoolService::new_with_client(
        db.clone(),
        mock_client,
        peer_addr.into(),
        block_discovery_addr.clone(),
        GossipClient::new(Duration::from_secs(5), Address::default()),
    );
    let addr = service.start();

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

    // Verify both block2 and block3 are sent to the block discovery service
    let block_messages = block_discovery_addr
        .send(GetBlockDiscoveryMessages)
        .await
        .expect("to get block messages");

    // We should have received 2 blocks: block2 and block3
    assert_eq!(block_messages.len(), 2);

    // The blocks should be received in order of processing: first block2, then block3
    let discovered_block2 = block_messages
        .get(0)
        .expect("to get block2 message")
        .0
        .as_ref()
        .clone();
    let discovered_block3 = block_messages
        .get(1)
        .expect("to get block3 message")
        .0
        .as_ref()
        .clone();

    assert_eq!(discovered_block2, block2);
    assert_eq!(discovered_block3, block3);
}
