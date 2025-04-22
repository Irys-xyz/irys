use actix::prelude::*;
use irys_api_client::{ApiClient, IrysApiClient};
use irys_database::reth_db::{Database, DatabaseError};
use irys_database::tables::PeerListItems;
use irys_database::{insert_peer_list_item, walk_all};
use irys_types::{
    Address, Config, DatabaseProvider, PeerAddress, PeerListItem, PeerResponse, RejectedResponse,
    VersionRequest,
};
use reqwest::Client;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tracing::{debug, error, warn};

use crate::reth_service::RethServiceActor;

const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const INACTIVE_PEERS_HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(10);
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(1);

pub type PeerListService = PeerListServiceWithClient<IrysApiClient>;

#[derive(Debug, Default)]
pub struct PeerListServiceWithClient<T: ApiClient + 'static + Unpin + Default> {
    /// Reference to the node database
    #[allow(dead_code)]
    db: Option<DatabaseProvider>,

    gossip_addr_to_mining_addr_map: HashMap<IpAddr, Address>,
    api_addr_to_mining_addr_map: HashMap<SocketAddr, Address>,
    peer_list_cache: HashMap<Address, PeerListItem>,
    known_peers_cache: HashSet<PeerAddress>,

    currently_running_announcements: HashSet<SocketAddr>,

    gossip_client: Client,
    irys_api_client: T,

    chain_id: u64,
    miner_address: Address,
    peer_address: PeerAddress,
}

// impl<T> Default for PeerListServiceWithClient<T> {
//     fn default() -> Self {
//         PeerListService::
//     }
// }

impl PeerListServiceWithClient<IrysApiClient> {
    /// Create a new instance of the peer_list_service actor passing in a reference-counted
    /// reference to a `DatabaseEnv`
    pub fn new(db: DatabaseProvider, config: &Config) -> Self {
        println!("service started: peer_list");
        Self::new_with_custom_api_client(db, config, IrysApiClient::new())
    }
}

impl<T: ApiClient + 'static + Unpin + Default> PeerListServiceWithClient<T> {
    /// Create a new instance of the peer_list_service actor passing in a reference-counted
    /// reference to a `DatabaseEnv`
    pub fn new_with_custom_api_client(
        db: DatabaseProvider,
        config: &Config,
        irys_api_client: T,
    ) -> Self {
        Self {
            db: Some(db),
            gossip_addr_to_mining_addr_map: HashMap::new(),
            api_addr_to_mining_addr_map: HashMap::new(),
            peer_list_cache: HashMap::new(),
            known_peers_cache: HashSet::new(),
            currently_running_announcements: HashSet::new(),
            gossip_client: Client::new(),
            irys_api_client,
            chain_id: config.chain_id,
            miner_address: config.miner_address(),
            peer_address: PeerAddress {
                gossip: format!(
                    "{}:{}",
                    config.gossip_service_bind_ip, config.gossip_service_port
                )
                .parse()
                .expect("valid SocketAddr expected"),
                api: format!("{}:{}", config.api_bind_ip, config.api_port)
                    .parse()
                    .expect("valid SocketAddr expected"),
                execution: config.reth_peer_info,
                mining_address: Address::ZERO,
            },
        }
    }
}

impl<T: ApiClient + 'static + Unpin + Default> Actor for PeerListServiceWithClient<T> {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        let peer_service_address = ctx.address();

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
                let client = act.gossip_client.clone();
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

        // Announce yourself to the network
        let version_request = self.create_version_request();
        let api_client = self.irys_api_client.clone();
        let peers_cache = self.known_peers_cache.clone();
        let announce_fut = Self::announce_yourself_to_all_peers(
            api_client,
            version_request,
            peers_cache,
            peer_service_address,
        )
        .into_actor(self);
        ctx.spawn(announce_fut);
    }
}

/// Allows this actor to live in the the service registry
impl<T: ApiClient + 'static + Unpin + Default> Supervised for PeerListServiceWithClient<T> {}

impl<T: ApiClient + 'static + Unpin + Default> SystemService for PeerListServiceWithClient<T> {
    fn service_started(&mut self, _ctx: &mut Context<Self>) {
        println!("service started: peer_list");
    }
}

#[derive(Debug, Clone)]
pub enum PeerListServiceError {
    DatabaseNotConnected,
    Database(DatabaseError),
    HealthCheckFailed(String),
    PostVersionError(String),
    PeerHandshakeRejected(RejectedResponse),
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

impl<T: ApiClient + 'static + Unpin + Default> PeerListServiceWithClient<T> {
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

    fn update_peer_address(&mut self, mining_addr: Address, new_address: PeerAddress) {
        if let Some(peer) = self.peer_list_cache.get_mut(&mining_addr) {
            let old_address = peer.address;
            peer.address = new_address;
            self.gossip_addr_to_mining_addr_map
                .remove(&old_address.gossip.ip());
            self.gossip_addr_to_mining_addr_map
                .insert(new_address.gossip.ip(), mining_addr);
            self.known_peers_cache.remove(&old_address);
            self.known_peers_cache.insert(old_address);
            self.api_addr_to_mining_addr_map.remove(&old_address.api);
            self.api_addr_to_mining_addr_map.insert(new_address.api, mining_addr);
        }
    }

    /// Add a peer to the peer list. Returns true if the peer was added, false if it already exists.
    fn add_peer(&mut self, mining_addr: Address, peer: PeerListItem) -> bool {
        let gossip_addr = peer.address.gossip;
        let peer_address = peer.address.clone();

        if !self.peer_list_cache.contains_key(&mining_addr) {
            self.peer_list_cache.insert(mining_addr, peer);
            self.gossip_addr_to_mining_addr_map
                .insert(gossip_addr.ip(), mining_addr);
            self.api_addr_to_mining_addr_map.insert(peer_address.api, mining_addr);
            self.known_peers_cache.insert(peer_address);
            true
        } else {
            debug!(
                "Peer {:?} already exists in the peer list, checking if the address needs updating",
                mining_addr
            );
            if let Some(existing_peer) = self.peer_list_cache.get_mut(&mining_addr) {
                if existing_peer.address != peer_address {
                    debug!("Peer address mismatch, updating to new address");
                    self.update_peer_address(mining_addr, peer_address);
                    true
                } else {
                    debug!("Peer does not need updating");
                    false
                }
            } else {
                warn!(
                    "Peer {:?} is not found in the peer list cache, which shouldn't happen",
                    mining_addr
                );
                false
            }
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

    fn create_version_request(&self) -> VersionRequest {
        VersionRequest {
            address: self.peer_address,
            chain_id: self.chain_id,
            user_agent: Some(format!("Irys-Node-{}", env!("CARGO_PKG_VERSION"))),
            ..VersionRequest::default()
        }
    }

    async fn announce_yourself_to_address(
        api_client: T,
        api_address: SocketAddr,
        version_request: VersionRequest,
        peer_service_address: Addr<PeerListServiceWithClient<T>>,
    ) -> Result<(), PeerListServiceError> {
        let peer_response = api_client
            .post_version(api_address, version_request)
            .await
            .map_err(|e| {
                error!(
                    "Failed to announce yourself to address {}: {:?}",
                    api_address, e
                );
                PeerListServiceError::PostVersionError(e.to_string())
            })?;

        match peer_service_address
            .send(AnnounceFinished {
                peer_api_address: api_address,
            })
            .await
        {
            Ok(()) => {}
            Err(mailbox_error) => {
                error!(
                    "Failed to send RemovePotentialPeer message to peer service: {:?}",
                    mailbox_error
                );
            }
        };

        match peer_response {
            PeerResponse::Accepted(accepted_peers) => {
                for peer in accepted_peers.peers {
                    match peer_service_address
                        .send(NewPotentialPeer {
                            api_address: peer.api,
                            force_announce: false,
                        })
                        .await
                    {
                        Ok(()) => {}
                        Err(mailbox_error) => {
                            error!(
                                "Failed to send NewPotentialPeer message to peer service: {:?}",
                                mailbox_error
                            );
                        }
                    }
                }

                Ok(())
            }
            PeerResponse::Rejected(rejected_response) => Err(
                PeerListServiceError::PeerHandshakeRejected(rejected_response),
            ),
        }
    }

    async fn announce_yourself_to_address_task(
        api_client: T,
        api_address: SocketAddr,
        version_request: VersionRequest,
        peer_list_service_address: Addr<PeerListServiceWithClient<T>>,
    ) {
        debug!(
            "Announcing yourself to address {} with version request: {:?}",
            api_address, version_request
        );
        match Self::announce_yourself_to_address(
            api_client,
            api_address,
            version_request,
            peer_list_service_address,
        )
        .await
        {
            Ok(()) => {}
            Err(e) => {
                error!(
                    "Failed to announce yourself to address {}: {:?}",
                    api_address, e
                );
            }
        }
    }

    async fn announce_yourself_to_all_peers(
        api_client: T,
        version_request: VersionRequest,
        known_peers_cache: HashSet<PeerAddress>,
        peer_service_address: Addr<PeerListServiceWithClient<T>>,
    ) {
        let reth_service = RethServiceActor::from_registry();
        for peer in known_peers_cache.iter() {
            match Self::announce_yourself_to_address(
                api_client.clone(),
                peer.api,
                version_request.clone(),
                peer_service_address.clone(),
            )
            .await
            {
                Ok(_peer_response) => {
                    // TODO: announce yourself to those peers as well
                    let _ = reth_service
                        .send(peer.execution)
                        .await
                        .inspect_err(|e| error!("Failed to connect to reth peer {}", &e));
                }
                Err(e) => {
                    error!(
                        "Failed to announce yourself to address {}: {:?}",
                        peer.api, e
                    );
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

impl<T: ApiClient + 'static + Unpin + Default> Handler<PeerListEntryRequest>
    for PeerListServiceWithClient<T>
{
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

impl<T: ApiClient + 'static + Unpin + Default> Handler<DecreasePeerScore>
    for PeerListServiceWithClient<T>
{
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

impl<T: ApiClient + 'static + Unpin + Default> Handler<IncreasePeerScore>
    for PeerListServiceWithClient<T>
{
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

impl<T: ApiClient + 'static + Unpin + Default> Handler<ActivePeersRequest>
    for PeerListServiceWithClient<T>
{
    type Result = Vec<PeerListItem>;

    fn handle(&mut self, msg: ActivePeersRequest, _ctx: &mut Self::Context) -> Self::Result {
        let mut peers: Vec<PeerListItem> = self.peer_list_cache.values().cloned().collect();
        tracing::trace!("ActivePeersRequest: {} peers before retain()", peers.len());
        tracing::trace!("ActivePeersRequest: peers {:?})", peers);
        peers.retain(|peer| {
            !msg.exclude_peers.contains(&peer.address.gossip)
                && peer.reputation_score.is_active()
                && peer.is_online
        });
        tracing::trace!("ActivePeersRequest: {} peers after retain()", peers.len());
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

impl<T: ApiClient + 'static + Unpin + Default> Handler<FlushRequest>
    for PeerListServiceWithClient<T>
{
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

impl<T: ApiClient + 'static + Unpin + Default> Handler<AddPeer> for PeerListServiceWithClient<T> {
    type Result = ();

    fn handle(&mut self, msg: AddPeer, ctx: &mut Self::Context) -> Self::Result {
        debug!("AddPeer message received: {:?}", msg.peer);
        let peer_api_addr = msg.peer.address.api;
        let is_updated = self.add_peer(msg.mining_addr, msg.peer);
        let peer_service_addr = ctx.address();

        if is_updated {
            let version_request = self.create_version_request();
            let handshake_task = Self::announce_yourself_to_address_task(
                self.irys_api_client.clone(),
                peer_api_addr,
                version_request,
                peer_service_addr,
            );
            ctx.spawn(handshake_task.into_actor(self));
        }
    }
}

/// Request a list of peers
#[derive(Message, Debug)]
#[rtype(result = "Vec<PeerAddress>")]
pub struct KnownPeersRequest;

impl<T: ApiClient + 'static + Unpin + Default> Handler<KnownPeersRequest>
    for PeerListServiceWithClient<T>
{
    type Result = Vec<PeerAddress>;

    fn handle(&mut self, _msg: KnownPeersRequest, _ctx: &mut Self::Context) -> Self::Result {
        self.known_peers_cache.iter().cloned().collect()
    }
}

/// Handle potential new peer
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct NewPotentialPeer {
    pub api_address: SocketAddr,
    pub force_announce: bool,
}

impl<T: ApiClient + 'static + Unpin + Default> Handler<NewPotentialPeer>
    for PeerListServiceWithClient<T>
{
    type Result = ();

    fn handle(&mut self, msg: NewPotentialPeer, ctx: &mut Self::Context) -> Self::Result {
        let already_in_cache = self.api_addr_to_mining_addr_map.contains_key(&msg.api_address);
        let already_announcing = self
            .currently_running_announcements
            .contains(&msg.api_address);

        let announcing_or_in_cache = already_announcing || already_in_cache;

        let needs_announce = msg.force_announce || !announcing_or_in_cache;

        if needs_announce {
            debug!(
                "Need to announce yourself to peer {:?}",
                msg.api_address
            );
            self.currently_running_announcements
                .insert(msg.api_address);
            let version_request = self.create_version_request();
            let peer_service_addr = ctx.address();
            let handshake_task = Self::announce_yourself_to_address_task(
                self.irys_api_client.clone(),
                msg.api_address,
                version_request,
                peer_service_addr,
            );
            ctx.spawn(handshake_task.into_actor(self));
        }
    }
}

/// Handle potential new peer
#[derive(Message, Debug)]
#[rtype(result = "()")]
pub struct AnnounceFinished {
    pub peer_api_address: SocketAddr,
}

impl<T: ApiClient + 'static + Unpin + Default> Handler<AnnounceFinished>
    for PeerListServiceWithClient<T>
{
    type Result = ();

    fn handle(&mut self, msg: AnnounceFinished, _ctx: &mut Self::Context) -> Self::Result {
        self.currently_running_announcements
            .remove(&msg.peer_api_address);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_api_client::test_utils::CountingMockClient;
    use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::peer_list::PeerScore;
    use irys_types::{RethPeerInfo, VersionRequest};
    use std::collections::HashSet;
    use std::net::IpAddr;
    use std::str::FromStr;
    use std::sync::Arc;
    use tokio::sync::Mutex;

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
            execution: RethPeerInfo::default(),
            mining_address: mining_addr,
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
        let config = Config::testnet();
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));
        let mut service = PeerListServiceWithClient::new(db, &config);
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
        let config = Config::testnet();
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));
        let mut service = PeerListServiceWithClient::new(db, &config);
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
        let config = Config::testnet();
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));
        let mut service = PeerListServiceWithClient::new(db, &config);
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
        let config = Config::testnet();
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));
        let mut service = PeerListServiceWithClient::new(db, &config);
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
        let mut empty_service = PeerListServiceWithClient::new(new_test_db, &config);

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
        let config = Config::testnet();
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));

        // Start the actor system with our service
        let service = PeerListServiceWithClient::new(db.clone(), &config);
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
        let config = Config::testnet();
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));

        // Create first service instance and add some peers
        let mut service = PeerListServiceWithClient::new(db.clone(), &config);
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
        let mut new_service = PeerListServiceWithClient::new(db, &config);
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

    #[actix_rt::test]
    async fn test_announce_yourself_to_all_peers() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config = Config::testnet();
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));

        // Create first service instance and add some peers
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mock_client = CountingMockClient {
            post_version_calls: calls.clone(),
        };
        let peer_list_service =
            PeerListServiceWithClient::new_with_custom_api_client(db, &config, mock_client.clone());
        let addr = peer_list_service.start();

        let (_mining1, peer1) = create_test_peer(
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            9001,
            true,
            None,
        );
        let (_mining2, peer2) = create_test_peer(
            "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            9002,
            true,
            None,
        );
        let known_peers: HashSet<_> = vec![peer1.address.clone(), peer2.address.clone()]
            .into_iter()
            .collect();
        let version_request = VersionRequest::default();

        PeerListServiceWithClient::announce_yourself_to_all_peers(
            mock_client,
            version_request,
            known_peers,
            addr,
        )
        .await;

        let calls = calls.lock().await;
        assert_eq!(calls.len(), 2);
        assert!(calls.contains(&peer1.address.api));
        assert!(calls.contains(&peer2.address.api));
    }

    #[actix_rt::test]
    async fn test_update_address_in_add_peer() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config = Config::testnet();
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));
        let mut service = PeerListServiceWithClient::new_with_custom_api_client(
            db,
            &config,
            CountingMockClient::default(),
        );
        let ctx = &mut Context::new();

        // Add initial peer
        let mining_addr = Address::from_str("0x1234567890123456789012345678901234567890")
            .expect("Invalid mining address");

        let initial_ip = IpAddr::from_str("127.0.0.1").expect("Invalid IP");
        let initial_gossip_addr = SocketAddr::new(initial_ip, 8080);
        let initial_api_addr = SocketAddr::new(initial_ip, 8081);

        let initial_peer_addr = PeerAddress {
            gossip: initial_gossip_addr,
            api: initial_api_addr,
            execution: RethPeerInfo::default(),
            mining_address: Address::ZERO,
        };

        let initial_peer = PeerListItem {
            address: initial_peer_addr.clone(),
            reputation_score: PeerScore::new(50),
            response_time: 100,
            last_seen: 123,
            is_online: true,
        };

        // Add the initial peer
        service.handle(
            AddPeer {
                mining_addr,
                peer: initial_peer,
            },
            ctx,
        );

        // Verify the peer was added
        let initial_result = service.handle(
            PeerListEntryRequest::GossipSocketAddress(initial_gossip_addr),
            ctx,
        );
        assert!(initial_result.is_some());
        assert_eq!(initial_result.unwrap().address, initial_peer_addr);

        // Create a new peer with the same mining address but different network addresses
        let new_ip = IpAddr::from_str("192.168.1.1").expect("Invalid IP");
        let new_gossip_addr = SocketAddr::new(new_ip, 9090);
        let new_api_addr = SocketAddr::new(new_ip, 9091);

        let new_peer_addr = PeerAddress {
            gossip: new_gossip_addr,
            api: new_api_addr,
            execution: RethPeerInfo::default(),
            mining_address: Address::with_last_byte(1),
        };

        let updated_peer = PeerListItem {
            address: new_peer_addr.clone(),
            reputation_score: PeerScore::new(50),
            response_time: 100,
            last_seen: 123,
            is_online: true,
        };

        // Update the peer with new address
        service.handle(
            AddPeer {
                mining_addr,
                peer: updated_peer,
            },
            ctx,
        );

        // Verify the peer address was updated
        let updated_result = service.handle(
            PeerListEntryRequest::GossipSocketAddress(new_gossip_addr),
            ctx,
        );
        assert!(
            updated_result.is_some(),
            "Should find peer with new gossip address"
        );
        assert_eq!(
            updated_result.unwrap().address,
            new_peer_addr,
            "Peer address should be updated"
        );

        // The old address should no longer be associated with this peer
        let old_result = service.handle(
            PeerListEntryRequest::GossipSocketAddress(initial_gossip_addr),
            ctx,
        );
        assert!(
            old_result.is_none(),
            "Should not find peer with old gossip address"
        );
    }

    #[actix_rt::test]
    async fn should_perform_handshake_when_adding_a_peer() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config = Config::testnet();
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));

        // Create a mock client to track API calls
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mock_client = CountingMockClient {
            post_version_calls: calls.clone(),
        };

        // Create the service with our mock client instead of the real one
        let service =
            PeerListServiceWithClient::new_with_custom_api_client(db, &config, mock_client);
        let service_addr = service.start();

        // Create a test peer
        let (mining_addr, peer) = create_test_peer(
            "0x1234567890123456789012345678901234567890",
            8080,
            true,
            None,
        );

        // Add the peer which should trigger announce_yourself_to_address_task
        service_addr
            .send(AddPeer {
                mining_addr,
                peer: peer.clone(),
            })
            .await
            .expect("send peer");

        tokio::time::sleep(Duration::from_millis(10)).await;

        // Verify the API call was made to the peer's API address
        let calls = calls.lock().await;
        assert_eq!(calls.len(), 1, "Should have made one API call");
        assert!(
            calls.contains(&peer.address.api),
            "Should have called the peer's API address"
        );
    }

    #[actix_rt::test]
    async fn should_prevent_infinite_handshake_loop() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config = Config::testnet();
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));

        // Create a mock client to track API calls
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mock_client = CountingMockClient {
            post_version_calls: calls.clone(),
        };

        // Create the service with our mock client instead of the real one
        let service =
            PeerListServiceWithClient::new_with_custom_api_client(db, &config, mock_client);
        let service_addr = service.start();

        // Create a test peer
        let (mining_addr, peer) = create_test_peer(
            "0x1234567890123456789012345678901234567890",
            8080,
            true,
            None,
        );

        // Add the peer which should trigger announce_yourself_to_address_task
        service_addr
            .send(AddPeer {
                mining_addr,
                peer: peer.clone(),
            })
            .await
            .expect("send peer");

        // Send a NewPotentialPeer message for the same peer while an announcement is already running
        service_addr
            .send(NewPotentialPeer {
                api_address: peer.address.api,
                force_announce: false,
            })
            .await
            .expect("send NewPotentialPeer");

        // Even though we sent two messages that could trigger a handshake,
        // the currently_running_announcements tracking should prevent duplicate calls
        tokio::time::sleep(Duration::from_millis(10)).await;

        {
            let calls_guard = calls.lock().await;
            assert_eq!(
                calls_guard.len(),
                1,
                "Should have made only one API call despite multiple triggers"
            );
            assert!(
                calls_guard.contains(&peer.address.api),
                "Should have called the peer's API address"
            );
        }

        // Now let's simulate the announcement finishing
        service_addr
            .send(AnnounceFinished {
                peer_api_address: peer.address.api,
            })
            .await
            .expect("send AnnounceFinished");

        // Now we can force a new announcement
        service_addr
            .send(NewPotentialPeer {
                api_address: peer.address.api,
                force_announce: true,
            })
            .await
            .expect("send NewPotentialPeer with force");

        tokio::time::sleep(Duration::from_millis(10)).await;

        // Now we should have two calls
        let calls = calls.lock().await;
        assert_eq!(
            calls.len(),
            2,
            "Should make another API call after announcement finished"
        );
    }
}
