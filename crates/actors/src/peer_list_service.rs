use actix::prelude::*;
use irys_database::reth_db::{Database, DatabaseError};
use irys_database::tables::PeerListItems;
use irys_database::{insert_peer_list_item, walk_all};
use irys_types::{Address, DatabaseProvider, PeerAddress, PeerListItem};
use reqwest::Client;
use reth_db::Database;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tracing::{debug, error, warn};

const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const INACTIVE_PEERS_HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(10);
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Default)]
pub struct PeerListService {
    /// Reference to the node database
    #[allow(dead_code)]
    db: Option<DatabaseProvider>,

    gossip_addr_to_mining_addr_map: HashMap<IpAddr, Address>,
    peer_list_cache: HashMap<Address, PeerListItem>,
    known_peers_cache: HashSet<PeerAddress>,

    client: Client,
}

impl Actor for PeerListService {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        ctx.run_interval(FLUSH_INTERVAL, |act, _ctx| match act.flush() {
            Ok(()) => {}
            Err(e) => {
                error!("Failed to flush peer list to database: {:?}", e);
            }
        });

        ctx.run_interval(INACTIVE_PEERS_HEALTH_CHECK_INTERVAL, |act, ctx| {
            // Collect inactive peers with the required fields
            let inactive_peers: Vec<(Address, PeerListItem, SocketAddr)> = act
                .peer_list_cache
                .iter()
                .filter(|(_mining_addr, peer)| !peer.reputation_score.is_active())
                .map(|(mining_addr, peer)| {
                    // Clone or copy the fields we need for the async operation
                    let peer_item = peer.clone();
                    let mining_addr = *mining_addr;
                    let peer_addr = peer_item.address.clone();
                    (mining_addr, peer_item, peer_addr.gossip)
                })
                .collect();

            for (mining_addr, peer, gossip_addr) in inactive_peers {
                // Clone the peer address to use in the async block
                let peer_address = peer.address.clone();
                let client = act.client.clone();
                // Create the future that does the health check
                let fut = async move { check_health(peer_address, client).await }
                    .into_actor(act)
                    .map(move |result, act, _ctx| match result {
                        Ok(true) => {
                            debug!("Peer {:?} is online", mining_addr);
                            act.increase_peer_score(&gossip_addr, ScoreIncreaseReason::Online);
                        }
                        Ok(false) => {
                            debug!("Peer {:?} is offline", mining_addr);
                            act.decrease_peer_score(&gossip_addr, ScoreDecreaseReason::Offline);
                        }
                        Err(e) => {
                            error!("Failed to check health of peer {:?}: {:?}", mining_addr, e);
                        }
                    });
                ctx.spawn(fut);
            }
        });
    }
}

/// Allows this actor to live in the the service registry
impl Supervised for PeerListService {}

impl SystemService for PeerListService {
    fn service_started(&mut self, _ctx: &mut Context<Self>) {
        println!("service started: peer_list");
    }
}

#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct AddPeerMessage(pub PeerListItem);

impl Handler<AddPeerMessage> for PeerListService {
    type Result = ();
    fn handle(&mut self, msg: AddPeerMessage, _ctx: &mut Self::Context) -> Self::Result {
        //TODO: What is the purpose of this mining address when adding a peer?
        let address = Address::random();
        let db = self.db.as_mut().expect("expected valid db");
        if let Err(e) = db
            .update(|tx| insert_peer_list_item(tx, &address, &msg.0))
            .expect("")
        {
            error!("Writing peer to db failed: {e}");
        }

        ()
    }
}

impl PeerListService {
    /// Create a new instance of the peer_list_service actor passing in a reference
    /// counted reference to a `DatabaseEnv`
    pub fn new(db: DatabaseProvider) -> Self {
        println!("service started: peer_list");
        Self {
            db: Some(db),
            gossip_addr_to_mining_addr_map: HashMap::new(),
            peer_list_cache: HashMap::new(),
            known_peers_cache: HashSet::new(),
            client: Client::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum PeerListServiceError {
    DatabaseNotConnected,
    Database(DatabaseError),
    HealthCheckFailed(String),
}

impl From<DatabaseError> for PeerListServiceError {
    fn from(err: DatabaseError) -> Self {
        Self::Database(err)
    }
}

#[derive(Clone, Debug, Copy)]
pub enum ScoreDecreaseReason {
    BogusData,
    Offline,
}

#[derive(Clone, Debug, Copy)]
pub enum ScoreIncreaseReason {
    Online,
    ValidData,
}

impl PeerListService {
    /// Initialize the peer list service
    ///
    /// # Errors
    ///
    /// This function will return an error if the load from the database fails
    pub fn initialize(&mut self) -> Result<(), PeerListServiceError> {
        if let Some(db) = self.db.as_ref() {
            let read_tx = db.tx().map_err(PeerListServiceError::from)?;

            let peer_list_items =
                walk_all::<PeerListItems, _>(&read_tx).map_err(PeerListServiceError::from)?;

            for (mining_addr, entry) in peer_list_items {
                let address = entry.address;
                self.gossip_addr_to_mining_addr_map
                    .insert(entry.address.gossip.ip(), mining_addr);
                self.peer_list_cache.insert(mining_addr, entry.0);
                self.known_peers_cache.insert(address);
            }
        } else {
            return Err(PeerListServiceError::DatabaseNotConnected);
        }

        Ok(())
    }

    fn flush(&self) -> Result<(), PeerListServiceError> {
        if let Some(db) = &self.db {
            db.update(|tx| {
                for (addr, peer) in self.peer_list_cache.iter() {
                    insert_peer_list_item(tx, addr, peer).map_err(PeerListServiceError::from)?;
                }
                Ok(())
            })
            .map_err(|e| PeerListServiceError::Database(e))?
        } else {
            Err(PeerListServiceError::DatabaseNotConnected)
        }
    }

    fn peer_by_address(&self, address: SocketAddr) -> Option<PeerListItem> {
        let mining_address = self
            .gossip_addr_to_mining_addr_map
            .get(&address.ip())
            .cloned()?;
        self.peer_list_cache.get(&mining_address).cloned()
    }

    fn add_peer(&mut self, mining_addr: Address, peer: PeerListItem) {
        let gossip_addr = peer.address.gossip;
        let peer_address = peer.address.clone();

        if !self.peer_list_cache.contains_key(&mining_addr) {
            self.peer_list_cache.insert(mining_addr, peer);
            self.gossip_addr_to_mining_addr_map
                .insert(gossip_addr.ip(), mining_addr);
            self.known_peers_cache.insert(peer_address);
        } else {
            warn!("Peer {:?} already exists in the peer list, adding it again will override previous data", mining_addr);
        }
    }

    fn increase_peer_score(&mut self, address: &SocketAddr, score: ScoreIncreaseReason) {
        if let Some(mining_addr) = self.gossip_addr_to_mining_addr_map.get(&address.ip()) {
            if let Some(peer_item) = self.peer_list_cache.get_mut(mining_addr) {
                match score {
                    ScoreIncreaseReason::Online => {
                        peer_item.reputation_score.increase();
                    }
                    ScoreIncreaseReason::ValidData => {
                        peer_item.reputation_score.increase();
                    }
                }
            }
        }
    }

    fn decrease_peer_score(&mut self, peer: &SocketAddr, reason: ScoreDecreaseReason) {
        if let Some(mining_addr) = self.gossip_addr_to_mining_addr_map.get(&peer.ip()) {
            if let Some(peer_item) = self.peer_list_cache.get_mut(mining_addr) {
                match reason {
                    ScoreDecreaseReason::BogusData => {
                        peer_item.reputation_score.decrease_bogus_data();
                    }
                    ScoreDecreaseReason::Offline => {
                        peer_item.reputation_score.decrease_offline();
                    }
                }

                // Don't propagate inactive peers
                if !peer_item.reputation_score.is_active() {
                    self.known_peers_cache.remove(&peer_item.address);
                }
            }
        }
    }
}

async fn check_health(peer: PeerAddress, client: Client) -> Result<bool, PeerListServiceError> {
    let url = format!("http://{}/gossip/health", peer.gossip);

    let response = client
        .get(&url)
        .timeout(HEALTH_CHECK_TIMEOUT)
        .send()
        .await
        .map_err(|error| PeerListServiceError::HealthCheckFailed(error.to_string()))?;

    if !response.status().is_success() {
        return Err(PeerListServiceError::HealthCheckFailed(format!(
            "Health check failed with status: {}",
            response.status()
        )));
    }

    response
        .json()
        .await
        .map_err(|error| PeerListServiceError::HealthCheckFailed(error.to_string()))
}

impl From<eyre::Report> for PeerListServiceError {
    fn from(err: eyre::Report) -> Self {
        PeerListServiceError::Database(DatabaseError::Other(err.to_string()))
    }
}

/// Request info about a specific peer
#[derive(Message, Debug)]
#[rtype(result = "Option<PeerListItem>")]
pub enum PeerListEntryRequest {
    GossipSocketAddress(SocketAddr),
}

impl Handler<PeerListEntryRequest> for PeerListService {
    type Result = Option<PeerListItem>;

    fn handle(&mut self, msg: PeerListEntryRequest, _ctx: &mut Self::Context) -> Self::Result {
        match msg {
            PeerListEntryRequest::GossipSocketAddress(gossip_addr) => {
                self.peer_by_address(gossip_addr)
            }
        }
    }
}

/// Decrease the score of a peer
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct DecreasePeerScore {
    pub peer: SocketAddr,
    pub reason: ScoreDecreaseReason,
}

impl Handler<DecreasePeerScore> for PeerListService {
    type Result = ();

    fn handle(&mut self, msg: DecreasePeerScore, _ctx: &mut Self::Context) -> Self::Result {
        self.decrease_peer_score(&msg.peer, msg.reason);
    }
}

/// Increase the score of a peer
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct IncreasePeerScore {
    pub peer: SocketAddr,
    pub reason: ScoreIncreaseReason,
}

impl Handler<IncreasePeerScore> for PeerListService {
    type Result = ();

    fn handle(&mut self, msg: IncreasePeerScore, _ctx: &mut Self::Context) -> Self::Result {
        if let Some(mining_addr) = self.gossip_addr_to_mining_addr_map.get(&msg.peer.ip()) {
            if let Some(peer_item) = self.peer_list_cache.get_mut(mining_addr) {
                match msg.reason {
                    ScoreIncreaseReason::Online => {
                        peer_item.reputation_score.increase();
                    }
                    ScoreIncreaseReason::ValidData => {
                        peer_item.reputation_score.increase();
                    }
                }
            }
        }
    }
}

/// Get the list of active peers
#[derive(Message, Debug)]
#[rtype(result = "Vec<PeerListItem>")]
pub struct ActivePeersRequest {
    pub truncate: Option<usize>,
    pub exclude_peers: HashSet<SocketAddr>,
}

impl Handler<ActivePeersRequest> for PeerListService {
    type Result = Vec<PeerListItem>;

    fn handle(&mut self, msg: ActivePeersRequest, _ctx: &mut Self::Context) -> Self::Result {
        let mut peers: Vec<PeerListItem> = self.peer_list_cache.values().cloned().collect();
        peers.retain(|peer| {
            !msg.exclude_peers.contains(&peer.address.gossip)
                && peer.reputation_score.is_active()
                && peer.is_online
        });
        peers.sort_by_key(|peer| peer.reputation_score.get());
        peers.reverse();

        if let Some(truncate) = msg.truncate {
            peers.truncate(truncate);
        }

        peers
    }
}

/// Flush the peer list to the database
#[derive(Message, Debug)]
#[rtype(result = "Result<(), PeerListServiceError>")]
pub struct FlushRequest;

impl Handler<FlushRequest> for PeerListService {
    type Result = Result<(), PeerListServiceError>;

    fn handle(&mut self, _msg: FlushRequest, _ctx: &mut Self::Context) -> Self::Result {
        self.flush()
    }
}

/// Add peer to the peer list
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct AddPeer {
    pub mining_addr: Address,
    pub peer: PeerListItem,
}

impl Handler<AddPeer> for PeerListService {
    type Result = ();

    fn handle(&mut self, msg: AddPeer, _ctx: &mut Self::Context) -> Self::Result {
        self.add_peer(msg.mining_addr, msg.peer)
    }
}

/// Add peer to the peer list
#[derive(Message, Debug)]
#[rtype(result = "Vec<PeerAddress>")]
pub struct KnownPeersRequest;

impl Handler<KnownPeersRequest> for PeerListService {
    type Result = Vec<PeerAddress>;

    fn handle(&mut self, _msg: KnownPeersRequest, _ctx: &mut Self::Context) -> Self::Result {
        self.known_peers_cache.iter().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::peer_list::PeerScore;
    use std::net::IpAddr;
    use std::str::FromStr;
    use std::sync::Arc;

    fn create_test_peer(
        mining_addr: &str,
        gossip_port: u16,
        is_online: bool,
        custom_ip: Option<IpAddr>,
    ) -> (Address, PeerListItem) {
        let mining_addr = Address::from_str(mining_addr).expect("Invalid mining address");
        let ip =
            custom_ip.unwrap_or_else(|| IpAddr::from_str("127.0.0.1").expect("Invalid ip address"));
        let gossip_addr = SocketAddr::new(ip, gossip_port);
        let api_addr = SocketAddr::new(ip, gossip_port + 1); // API port is gossip_port + 1

        let peer_addr = PeerAddress {
            gossip: gossip_addr,
            api: api_addr,
        };

        let peer = PeerListItem {
            address: peer_addr,
            reputation_score: PeerScore::new(50),
            response_time: 100, // Default response time in ms
            last_seen: 123,
            is_online,
        };
        (mining_addr, peer)
    }

    #[actix_rt::test]
    async fn test_add_peer() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));
        let mut service = PeerListService::new(db);
        let ctx = &mut Context::new();

        // Test adding a new peer
        let (mining_addr, peer) = create_test_peer(
            "0x1234567890123456789012345678901234567890",
            8080,
            true,
            None,
        );

        // Add peer using message handler
        service.handle(
            AddPeer {
                mining_addr,
                peer: peer.clone(),
            },
            ctx,
        );

        // Verify peer was added correctly using PeerListEntryRequest
        let result = service.handle(
            PeerListEntryRequest::GossipSocketAddress(peer.address.gossip),
            ctx,
        );
        assert!(result.is_some());
        assert_eq!(result.expect("get peer"), peer);

        // Verify known peers using KnownPeersRequest
        let known_peers = service.handle(KnownPeersRequest, ctx);
        assert!(known_peers.contains(&peer.address));
    }

    #[actix_rt::test]
    async fn test_peer_score_management() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));
        let mut service = PeerListService::new(db);
        let ctx = &mut Context::new();

        // Add a test peer
        let (mining_addr, peer) = create_test_peer(
            "0x1234567890123456789012345678901234567890",
            8080,
            true,
            None,
        );
        service.handle(
            AddPeer {
                mining_addr,
                peer: peer.clone(),
            },
            ctx,
        );

        // Test increasing score using message handler
        service.handle(
            IncreasePeerScore {
                peer: peer.address.gossip,
                reason: ScoreIncreaseReason::Online,
            },
            ctx,
        );

        // Verify score increased
        let updated_peer = service
            .handle(
                PeerListEntryRequest::GossipSocketAddress(peer.address.gossip),
                ctx,
            )
            .expect("updated peer score list");
        assert_eq!(updated_peer.reputation_score.get(), 51);

        // Test decreasing score using message handler
        service.handle(
            DecreasePeerScore {
                peer: peer.address.gossip,
                reason: ScoreDecreaseReason::Offline,
            },
            ctx,
        );

        // Verify score decreased
        let updated_peer = service
            .handle(
                PeerListEntryRequest::GossipSocketAddress(peer.address.gossip),
                ctx,
            )
            .expect("failed to get updated peer");
        assert_eq!(updated_peer.reputation_score.get(), 48);
    }

    #[actix_rt::test]
    async fn test_active_peers_request() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));
        let mut service = PeerListService::new(db);
        let ctx = &mut Context::new();

        // Add multiple peers with different states
        let (mining_addr1, mut peer1) = create_test_peer(
            "0x1111111111111111111111111111111111111111",
            8081,
            true,
            None,
        );
        let (mining_addr2, mut peer2) = create_test_peer(
            "0x2222222222222222222222222222222222222222",
            8082,
            true,
            None,
        );
        let (mining_addr3, peer3) = create_test_peer(
            "0x3333333333333333333333333333333333333333",
            8083,
            false,
            None,
        );

        // Make peer1 have higher reputation
        peer1.reputation_score.increase();
        peer1.reputation_score.increase();
        peer2.reputation_score.increase();

        // Add peers using message handler
        service.handle(
            AddPeer {
                mining_addr: mining_addr1,
                peer: peer1.clone(),
            },
            ctx,
        );
        service.handle(
            AddPeer {
                mining_addr: mining_addr2,
                peer: peer2.clone(),
            },
            ctx,
        );
        service.handle(
            AddPeer {
                mining_addr: mining_addr3,
                peer: peer3,
            },
            ctx,
        );

        // Test active peers request using message handler
        let exclude_peers = HashSet::new();
        let active_peers = service.handle(
            ActivePeersRequest {
                truncate: Some(2),
                exclude_peers,
            },
            ctx,
        );

        assert_eq!(active_peers.len(), 2);
        assert_eq!(active_peers[0].address, peer1.address); // Higher score should be first
        assert_eq!(active_peers[1].address, peer2.address);
    }

    #[actix_rt::test]
    async fn test_edge_cases() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));
        let mut service = PeerListService::new(db);
        let ctx = &mut Context::new();

        // Test adding duplicate peer
        let (mining_addr, peer) = create_test_peer(
            "0x1234567890123456789012345678901234567890",
            8080,
            true,
            None,
        );

        // Add same peer twice using message handler
        service.handle(
            AddPeer {
                mining_addr,
                peer: peer.clone(),
            },
            ctx,
        );
        service.handle(
            AddPeer {
                mining_addr,
                peer: peer.clone(),
            },
            ctx,
        );

        // Verify only one entry exists using KnownPeersRequest
        let known_peers = service.handle(KnownPeersRequest, ctx);
        assert_eq!(known_peers.len(), 1);

        // Test peer lookup with non-existent address
        let non_existent_addr =
            SocketAddr::new(IpAddr::from_str("192.168.1.1").expect("invalid IP"), 9999);
        let result = service.handle(
            PeerListEntryRequest::GossipSocketAddress(non_existent_addr),
            ctx,
        );
        assert!(result.is_none());

        // Test score manipulation for non-existent peer using message handlers
        service.handle(
            IncreasePeerScore {
                peer: non_existent_addr,
                reason: ScoreIncreaseReason::Online,
            },
            ctx,
        );
        service.handle(
            DecreasePeerScore {
                peer: non_existent_addr,
                reason: ScoreDecreaseReason::Offline,
            },
            ctx,
        );

        // Test active peers with empty list
        let new_temp_dir = setup_tracing_and_temp_dir(None, false);
        let new_test_db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&new_temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));
        let mut empty_service = PeerListService::new(new_test_db);

        let exclude_peers = HashSet::new();
        let active_peers = empty_service.handle(
            ActivePeersRequest {
                truncate: None,
                exclude_peers,
            },
            ctx,
        );
        assert!(active_peers.is_empty());
    }

    #[actix_rt::test]
    async fn test_periodic_flush() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));

        // Start the actor system with our service
        let service = PeerListService::new(db.clone());
        let addr = service.start();

        // Add a test peer
        let (mining_addr, peer) = create_test_peer(
            "0x1234567890123456789012345678901234567890",
            8080,
            true,
            None,
        );
        addr.send(AddPeer {
            mining_addr,
            peer: peer.clone(),
        })
        .await
        .expect("add peer failed");

        // Wait for more than the flush interval to ensure a flush has occurred
        tokio::time::sleep(FLUSH_INTERVAL + Duration::from_millis(100)).await;

        // Verify the data was persisted by reading directly from the database
        let read_tx = db
            .tx()
            .map_err(PeerListServiceError::from)
            .expect("failed to create read tx");

        let items = walk_all::<PeerListItems, _>(&read_tx)
            .map_err(PeerListServiceError::from)
            .expect("failed to walk all items");

        assert_eq!(items.len(), 1);

        let (stored_addr, stored_peer) = items.into_iter().next().expect("no peers");
        assert_eq!(stored_addr, mining_addr);
        assert_eq!(stored_peer.0.address, peer.address);
        assert_eq!(
            stored_peer.0.reputation_score.get(),
            peer.reputation_score.get()
        );
        assert_eq!(stored_peer.0.is_online, peer.is_online);
    }

    #[actix_rt::test]
    async fn test_load_from_database() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));

        // Create first service instance and add some peers
        let mut service = PeerListService::new(db.clone());
        let ctx = &mut Context::new();

        // Add multiple test peers
        let (mining_addr1, peer1) = create_test_peer(
            "0x1111111111111111111111111111111111111111",
            8081,
            true,
            Some(IpAddr::from_str("127.0.0.2").expect("Invalid IP")),
        );
        let (mining_addr2, peer2) = create_test_peer(
            "0x2222222222222222222222222222222222222222",
            8082,
            false,
            Some(IpAddr::from_str("127.0.0.3").expect("Invalid IP")),
        );

        service.handle(
            AddPeer {
                mining_addr: mining_addr1,
                peer: peer1.clone(),
            },
            ctx,
        );
        service.handle(
            AddPeer {
                mining_addr: mining_addr2,
                peer: peer2.clone(),
            },
            ctx,
        );

        // Manually flush data to database
        service
            .handle(FlushRequest, ctx)
            .expect("Failed to flush data");

        // Create new service instance that should load from database
        let mut new_service = PeerListService::new(db);
        new_service
            .initialize()
            .expect("Failed to initialize service");

        // Verify peers were loaded correctly
        let loaded_peer1 = new_service.handle(
            PeerListEntryRequest::GossipSocketAddress(peer1.address.gossip),
            ctx,
        );
        let loaded_peer2 = new_service.handle(
            PeerListEntryRequest::GossipSocketAddress(peer2.address.gossip),
            ctx,
        );

        assert!(
            loaded_peer1.is_some(),
            "Peer 1 should be loaded from database"
        );
        assert!(
            loaded_peer2.is_some(),
            "Peer 2 should be loaded from database"
        );
        assert_eq!(
            loaded_peer1.expect("Should have peer 1"),
            peer1,
            "Loaded peer 1 should match original"
        );
        assert_eq!(
            loaded_peer2.expect("Peer 2 should be loaded"),
            peer2,
            "Loaded peer 2 should match original"
        );

        // Verify internal maps are populated correctly
        let known_peers = new_service.handle(KnownPeersRequest, ctx);
        assert_eq!(known_peers.len(), 2, "Should have loaded 2 known peers");
        assert!(
            known_peers.contains(&peer1.address),
            "Known peers should contain peer 1"
        );
        assert!(
            known_peers.contains(&peer2.address),
            "Known peers should contain peer 2"
        );
    }
}
