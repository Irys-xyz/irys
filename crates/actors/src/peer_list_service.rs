use actix::prelude::*;
use irys_database::reth_db::{Database, DatabaseError};
use irys_database::tables::PeerListItems;
use irys_database::{insert_peer_list_item, walk_all};
use irys_types::{Address, DatabaseProvider, PeerAddress, PeerListItem};
use reqwest::Client;
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

/// Decrease the score of a peer
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
