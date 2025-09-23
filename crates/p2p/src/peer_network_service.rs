use crate::types::{GossipResponse, RejectionReason};
use crate::{gossip_client::GossipClientError, GossipClient, GossipError};
use actix::prelude::*;
use futures::StreamExt as _;
use irys_actors::reth_service::RethServiceMessage;
use irys_api_client::{ApiClient, IrysApiClient};
use irys_database::insert_peer_list_item;
use irys_database::reth_db::{Database as _, DatabaseError};
use irys_domain::{PeerList, ScoreDecreaseReason, ScoreIncreaseReason};
use irys_types::{
    build_user_agent, Address, Config, DatabaseProvider, GossipDataRequest, PeerAddress,
    PeerFilterMode, PeerListItem, PeerNetworkError, PeerNetworkSender, PeerNetworkServiceMessage,
    PeerResponse, RejectedResponse, RethPeerInfo, VersionRequest,
};
use moka::sync::Cache;
use rand::prelude::SliceRandom as _;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const INACTIVE_PEERS_HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(10);
const SUCCESSFUL_ANNOUNCEMENT_CACHE_TTL: Duration = Duration::from_secs(30);

/*
Global singletons for handshake flow control and safety

Why globals (process-wide):
- Multiple actor instances/tasks may trigger handshakes concurrently. To avoid
  per-actor throttling gaps, we enforce limits and backoff state process-wide.

Why OnceLock + Mutex:
- OnceLock provides thread-safe, lazy initialization without paying the cost
  of static constructors on startup, and it prevents races on first-use.
- Interior state is wrapped in a Mutex for low-contention, short critical
  sections. The hot path (permit acquire) is handled by a Semaphore.

Configuration and lifetime:
- Some of these singletons use values derived from NodeConfig and are
  initialized the first time the service starts in this process. Subsequent
  instances reuse the same values (first-wins).
*/

/// Global semaphore limiting the total number of concurrent handshake tasks across
/// the entire process. This prevents resource exhaustion (sockets, memory, CPU).
/// Initialized once with the maximum from the first service that calls it — see
/// `handshake_semaphore_with_max`. Subsequent calls reuse the same semaphore.
static HANDSHAKE_SEMAPHORE: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
    std::sync::OnceLock::new();

/// Returns the global handshake semaphore, initializing it with `max` if this is
/// the first call in the process. Note: configuration is first-wins; later calls
/// will not resize the semaphore.
fn handshake_semaphore_with_max(max: usize) -> std::sync::Arc<tokio::sync::Semaphore> {
    HANDSHAKE_SEMAPHORE
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(max)))
        .clone()
}

/// Global map of consecutive handshake failure counts per peer (by API SocketAddr).
/// Used to compute exponential backoff intervals and to decide when to place a
/// peer onto the temporary blocklist. Entries are cleared on successful handshakes
/// or when a peer is moved to the blocklist.
static HANDSHAKE_FAILURES: std::sync::OnceLock<std::sync::Mutex<HashMap<SocketAddr, u32>>> =
    std::sync::OnceLock::new();

/// Accessor for the global handshake failures map.
fn handshake_failures() -> &'static std::sync::Mutex<HashMap<SocketAddr, u32>> {
    HANDSHAKE_FAILURES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Global blocklist containing peers that should be skipped until a specific
/// Instant in the future (i.e., a TTL-based block). The TTL duration is configured
/// via NodeConfig.p2p_handshake.blacklist_ttl_secs. Peers are added after too
/// many consecutive failures, and removed either on success or when the TTL elapses
/// (checked at use sites).
static BLOCKLIST_UNTIL: std::sync::OnceLock<
    std::sync::Mutex<HashMap<SocketAddr, std::time::Instant>>,
> = std::sync::OnceLock::new();

/// Accessor for the global blocklist with expiry timestamps.
fn blocklist_until() -> &'static std::sync::Mutex<HashMap<SocketAddr, std::time::Instant>> {
    BLOCKLIST_UNTIL.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Cached, per-process cap on how many peers we will process from a single
/// Accepted handshake response. This is set once from NodeConfig when the
/// service starts and then reused in the hot path to avoid repeated config
/// lookups or cloning.
static PEERS_LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// Returns the configured peer processing cap (first-wins). If not initialized
/// yet via service startup, falls back to a config value
fn peers_limit() -> usize {
    *PEERS_LIMIT
        .get_or_init(|| irys_types::config::P2PHandshakeConfig::default().max_peers_per_response)
}

async fn send_message_and_print_error<T, A, R>(message: T, address: Addr<A>)
where
    T: Message<Result = R> + Send + 'static,
    R: Send,
    A: Actor<Context = Context<A>> + Handler<T>,
{
    match address.send(message).await {
        Ok(_) => {}
        Err(mailbox_error) => {
            error!(
                "Failed to send message to peer service: {:?}",
                mailbox_error
            );
        }
    }
}

#[derive(Debug)]
pub struct PeerNetworkService<A>
where
    A: ApiClient,
{
    /// Reference to the node database
    db: DatabaseProvider,

    peer_list: PeerList,

    currently_running_announcements: HashSet<SocketAddr>,
    successful_announcements: Cache<SocketAddr, AnnounceFinished>,
    failed_announcements: HashMap<SocketAddr, AnnounceFinished>,

    // This is related to networking - requesting data from the network and joining the network
    gossip_client: GossipClient,
    irys_api_client: A,

    chain_id: u64,
    peer_address: PeerAddress,

    reth_service: UnboundedSender<RethServiceMessage>,

    config: Config,

    peer_list_service_receiver: Option<UnboundedReceiver<PeerNetworkServiceMessage>>,
}

impl PeerNetworkService<IrysApiClient> {
    /// Create a new instance of the peer_list_service actor passing in a reference-counted
    /// reference to a `DatabaseEnv`
    pub fn new(
        db: DatabaseProvider,
        config: &Config,
        reth_service: UnboundedSender<RethServiceMessage>,
        service_receiver: UnboundedReceiver<PeerNetworkServiceMessage>,
        service_sender: PeerNetworkSender,
    ) -> Self {
        info!("service started: peer_list");
        Self::new_with_custom_api_client(
            db,
            config,
            IrysApiClient::new(),
            reth_service,
            service_receiver,
            service_sender,
        )
    }
}

impl<A> PeerNetworkService<A>
where
    A: ApiClient,
{
    /// Create a new instance of the peer_list_service actor passing in a reference-counted
    /// reference to a `DatabaseEnv`
    pub(crate) fn new_with_custom_api_client(
        db: DatabaseProvider,
        config: &Config,
        irys_api_client: A,
        reth_service: UnboundedSender<RethServiceMessage>,
        service_receiver: UnboundedReceiver<PeerNetworkServiceMessage>,
        service_sender: PeerNetworkSender,
    ) -> Self {
        let peer_list_data =
            PeerList::new(config, &db, service_sender).expect("Failed to load peer list data");
        PEERS_LIMIT.get_or_init(|| config.node_config.p2p_handshake.max_peers_per_response);

        Self {
            db,
            peer_list: peer_list_data,
            currently_running_announcements: HashSet::new(),
            successful_announcements: Cache::builder()
                .time_to_live(SUCCESSFUL_ANNOUNCEMENT_CACHE_TTL)
                .build(),
            failed_announcements: HashMap::new(),
            gossip_client: GossipClient::new(
                Duration::from_secs(5),
                config.node_config.miner_address(),
            ),
            irys_api_client,
            chain_id: config.consensus.chain_id,
            peer_address: PeerAddress {
                gossip: format!(
                    "{}:{}",
                    config.node_config.gossip.public_ip, config.node_config.gossip.public_port
                )
                .parse()
                .expect("valid SocketAddr expected"),
                api: format!(
                    "{}:{}",
                    config.node_config.http.public_ip, config.node_config.http.public_port
                )
                .parse()
                .expect("valid SocketAddr expected"),
                execution: RethPeerInfo {
                    peering_tcp_addr: format!(
                        "{}:{}",
                        &config.node_config.reth.network.public_ip,
                        &config.node_config.reth.network.public_port
                    )
                    .parse()
                    .expect("valid SocketAddr expected"),
                    peer_id: config.node_config.reth.network.peer_id,
                },
            },
            reth_service,
            config: config.clone(),
            peer_list_service_receiver: Some(service_receiver),
        }
    }
}

// TODO: this is a temporary solution to allow the peer list service to receive messages
impl<A> Handler<PeerNetworkServiceMessage> for PeerNetworkService<A>
where
    A: ApiClient,
{
    type Result = ();

    fn handle(&mut self, msg: PeerNetworkServiceMessage, ctx: &mut Self::Context) {
        let address = ctx.address();
        match msg {
            PeerNetworkServiceMessage::AnnounceYourselfToPeer(peer_list_item) => {
                ctx.spawn(
                    async move {
                        let _res = address
                            .send(AnnounceYourselfToPeerAndConnectReth {
                                peer: peer_list_item,
                            })
                            .await;
                    }
                    .into_actor(self),
                );
            }
            PeerNetworkServiceMessage::Handshake(handshake) => {
                ctx.spawn(
                    async move {
                        let _res = address
                            .send(NewPotentialPeer {
                                api_address: handshake.api_address,
                                force_announce: handshake.force,
                            })
                            .await;
                    }
                    .into_actor(self),
                );
            }
            PeerNetworkServiceMessage::RequestDataFromNetwork {
                data_request,
                use_trusted_peers_only,
                response,
                retries,
            } => {
                debug!("Requesting {:?} from network", &data_request);
                ctx.spawn(
                    async move {
                        match address
                            .send(RequestDataFromTheNetwork {
                                data_request,
                                use_trusted_peers_only,
                                retries,
                            })
                            .await
                        {
                            Ok(res) => {
                                response
                                    .send(res.map_err(|err| {
                                        PeerNetworkError::OtherInternalError(format!("{:?}", err))
                                    }))
                                    .unwrap_or_else(|e| {
                                        error!(
                                            "Failed to send response for block request: {:?}",
                                            e
                                        );
                                    });
                            }
                            Err(e) => {
                                error!("Failed to request block from network: {:?}", e);
                                response
                                    .send(Err(PeerNetworkError::OtherInternalError(format!(
                                        "{:?}",
                                        e
                                    ))))
                                    .unwrap_or_else(|e| {
                                        error!(
                                            "Failed to send response for block request: {:?}",
                                            e
                                        );
                                    });
                            }
                        }
                    }
                    .into_actor(self),
                );
            }
        }
    }
}

impl<A> Actor for PeerNetworkService<A>
where
    A: ApiClient,
{
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        let peer_service_address = ctx.address();
        let mut peer_list_service_receiver = self
            .peer_list_service_receiver
            .take()
            .expect("PeerListServiceWithClient should have a receiver");

        let peer_service_address_2 = peer_service_address.clone();
        // TODO: temporary solution to allow the peer list service to receive messages
        ctx.spawn(
            async move {
                while let Some(msg) = peer_list_service_receiver.recv().await {
                    peer_service_address_2.send(msg).await.unwrap_or_else(|e| {
                        error!("Failed to send message to peer list service: {:?}", e);
                    });
                }
            }
            .into_actor(self),
        );

        ctx.run_interval(FLUSH_INTERVAL, |act, _ctx| match act.flush() {
            Ok(()) => {}
            Err(e) => {
                error!("Failed to flush the peer list to the database: {:?}", e);
            }
        });

        ctx.run_interval(INACTIVE_PEERS_HEALTH_CHECK_INTERVAL, |act, ctx| {
            // Collect inactive peers with the required fields
            let inactive_peers: Vec<(Address, PeerListItem)> = act.peer_list.inactive_peers();

            for (mining_addr, peer) in inactive_peers {
                // Clone the peer address to use in the async block
                let peer_address = peer.address;
                let client = act.gossip_client.clone();
                let peer_list = act.peer_list.clone();
                // Create the future that does the health check
                let fut = async move { client.check_health(peer_address, &peer_list).await }
                    .into_actor(act)
                    .map(move |result, act, _ctx| match result {
                        Ok(true) => {
                            debug!("Peer {:?} is online", mining_addr);
                            act.increase_peer_score(&mining_addr, ScoreIncreaseReason::Online);
                        }
                        Ok(false) => {
                            debug!("Peer {:?} is offline", mining_addr);
                            act.decrease_peer_score(&mining_addr, ScoreDecreaseReason::Offline);
                        }
                        Err(GossipClientError::HealthCheck(u, e)) => {
                            debug!(
                                "Peer {:?}{} healthcheck failed with status {}",
                                mining_addr, u, e
                            );
                            act.decrease_peer_score(&mining_addr, ScoreDecreaseReason::Offline);
                        }
                        Err(e) => {
                            error!("Failed to check health of peer {:?}: {:?}", mining_addr, e);
                        }
                    });
                ctx.spawn(fut);
            }
        });

        // Initiate the trusted peers handshake
        let trusted_peers_handshake_task = Self::trusted_peers_handshake_task(
            peer_service_address.clone(),
            self.peer_list.trusted_peer_addresses(),
        )
        .into_actor(self);
        ctx.spawn(trusted_peers_handshake_task);

        // Announce yourself to the network
        let peers_cache = self
            .peer_list
            .all_peers()
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        let announce_fut = Self::announce_yourself_to_all_peers(peers_cache, peer_service_address)
            .into_actor(self);
        ctx.spawn(announce_fut);
    }
}

#[derive(Debug, Clone)]
pub enum PeerListServiceError {
    DatabaseNotConnected,
    Database(DatabaseError),
    PostVersionError(String),
    PeerHandshakeRejected(RejectedResponse),
    NoPeersAvailable,
    InternalSendError(MailboxError),
    FailedToRequestData(String),
}

impl From<MailboxError> for PeerListServiceError {
    fn from(value: MailboxError) -> Self {
        Self::InternalSendError(value)
    }
}

impl From<DatabaseError> for PeerListServiceError {
    fn from(err: DatabaseError) -> Self {
        Self::Database(err)
    }
}

impl From<eyre::Report> for PeerListServiceError {
    fn from(err: eyre::Report) -> Self {
        Self::Database(DatabaseError::Other(err.to_string()))
    }
}

impl<A> PeerNetworkService<A>
where
    A: ApiClient,
{
    fn flush(&self) -> Result<(), PeerListServiceError> {
        self.db
            .update(|tx| {
                // Only persist peers that are staked or have reached the persistence threshold
                for (addr, peer) in self.peer_list.persistable_peers().iter() {
                    insert_peer_list_item(tx, addr, peer).map_err(PeerListServiceError::from)?;
                }
                Ok(())
            })
            .map_err(PeerListServiceError::Database)?
    }

    async fn trusted_peers_handshake_task(
        peer_service_address: Addr<Self>,
        trusted_peers_api_addresses: HashSet<SocketAddr>,
    ) {
        let peer_service_address = peer_service_address.clone();

        for peer_api_address in trusted_peers_api_addresses {
            match peer_service_address
                .send(NewPotentialPeer::force_announce(peer_api_address))
                .await
            {
                Ok(()) => {}
                Err(mailbox_error) => {
                    error!(
                        "Failed to send NewPotentialPeer message to peer service: {:?}",
                        mailbox_error
                    );
                }
            };
        }
    }

    fn increase_peer_score(&mut self, mining_addr: &Address, score: ScoreIncreaseReason) {
        self.peer_list.increase_peer_score(mining_addr, score);
    }

    fn decrease_peer_score(&mut self, mining_addr: &Address, reason: ScoreDecreaseReason) {
        self.peer_list.decrease_peer_score(mining_addr, reason);
    }

    fn create_version_request(&self) -> VersionRequest {
        let signer = self.config.irys_signer();
        let mut version_request = VersionRequest {
            address: self.peer_address,
            chain_id: self.chain_id,
            user_agent: Some(build_user_agent("Irys-Node", env!("CARGO_PKG_VERSION"))),
            ..VersionRequest::default()
        };
        signer
            .sign_p2p_handshake(&mut version_request)
            .expect("Failed to sign version request");
        version_request
    }

    async fn announce_yourself_to_address(
        api_client: A,
        api_address: SocketAddr,
        version_request: VersionRequest,
        peer_service_address: Addr<Self>,
        is_trusted_peer: bool,
        peer_filter_mode: PeerFilterMode,
        peer_list: PeerList,
    ) -> Result<(), PeerListServiceError> {
        let peer_response_result = api_client
            .post_version(api_address, version_request)
            .await
            .map_err(|e| {
                warn!(
                    "Failed to announce yourself to address {}: {}",
                    api_address, e
                );
                PeerListServiceError::PostVersionError(e.to_string())
            });

        let peer_response = match peer_response_result {
            Ok(peer_response) => {
                send_message_and_print_error(
                    AnnounceFinished::success(api_address),
                    peer_service_address.clone(),
                )
                .await;
                Ok(peer_response)
            }
            Err(error) => {
                debug!(
                    "Retrying to announce yourself to address {}: {:?}",
                    api_address, error
                );
                // This is likely due to the networking error, we need to retry later
                send_message_and_print_error(
                    AnnounceFinished::retry(api_address),
                    peer_service_address.clone(),
                )
                .await;
                Err(error)
            }
        }?;

        match peer_response {
            PeerResponse::Accepted(accepted_peers) => {
                // Collect peer addresses for potential whitelist addition
                let peer_addresses: Vec<SocketAddr> =
                    accepted_peers.peers.iter().map(|p| p.api).collect();

                // Add peers to whitelist if this was a handshake with a trusted peer in TrustedAndHandshake mode
                if is_trusted_peer && peer_filter_mode == PeerFilterMode::TrustedAndHandshake {
                    debug!(
                        "Adding {} peers from trusted peer handshake to whitelist: {:?}",
                        peer_addresses.len(),
                        peer_addresses
                    );
                    peer_list.add_peers_to_whitelist(peer_addresses.clone());
                }

                // Limit and randomize peers from response to avoid resource exhaustion
                let mut peers = accepted_peers.peers;
                peers.shuffle(&mut rand::thread_rng());
                let limit = peers_limit();
                if peers.len() > limit {
                    peers.truncate(limit);
                }
                for peer in peers {
                    send_message_and_print_error(
                        NewPotentialPeer::new(peer.api),
                        peer_service_address.clone(),
                    )
                    .await;
                }
                Ok(())
            }
            PeerResponse::Rejected(rejected_response) => Err(
                PeerListServiceError::PeerHandshakeRejected(rejected_response),
            ),
        }
    }

    async fn announce_yourself_to_address_task(
        api_client: A,
        api_address: SocketAddr,
        version_request: VersionRequest,
        peer_list_service_address: Addr<Self>,
        is_trusted_peer: bool,
        peer_filter_mode: PeerFilterMode,
        peer_list: PeerList,
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
            is_trusted_peer,
            peer_filter_mode,
            peer_list,
        )
        .await
        {
            Ok(()) => {
                debug!("Successfully announced yourself to address {}", api_address);
            }
            Err(e) => {
                warn!(
                    "Failed to announce yourself to address {}: {:?}",
                    api_address, e
                );
            }
        }
    }

    async fn add_reth_peer_task(
        reth_service: UnboundedSender<RethServiceMessage>,
        reth_peer_info: RethPeerInfo,
    ) {
        let (tx, rx) = oneshot::channel();
        if let Err(err) = reth_service.send(RethServiceMessage::ConnectToPeer {
            peer: reth_peer_info,
            response: tx,
        }) {
            error!("Failed to send connect request to reth service: {}", err);
            return;
        }

        match rx.await {
            Ok(Ok(())) => {
                debug!("Successfully connected to reth peer");
            }
            Ok(Err(err)) => {
                error!("Failed to connect to reth peer: {}", err);
            }
            Err(_) => {
                error!("Reth service dropped connect response");
            }
        }
    }

    /// Note: this method uses HashMap<Address, PeerListItem> because that's what is stored and
    /// returned by the PeerListGuard. It doesn't require any maps or other transformations, but
    /// requires the loop in this method to ignore mining addresses.
    async fn announce_yourself_to_all_peers(
        known_peers: HashMap<Address, PeerListItem>,
        peer_service_address: Addr<Self>,
    ) {
        for (_mining_address, peer) in known_peers {
            match peer_service_address
                .send(AnnounceYourselfToPeerAndConnectReth { peer })
                .await
            {
                Ok(()) => {}
                Err(mailbox_error) => {
                    error!(
                        "Failed to send AnnounceYourselfToPeer message to peer service: {:?}",
                        mailbox_error
                    );
                }
            }
        }
    }
}

#[derive(Message, Debug)]
#[rtype(result = "Option<PeerList>")]
pub struct GetPeerListGuard;

impl<T> Handler<GetPeerListGuard> for PeerNetworkService<T>
where
    T: ApiClient,
{
    type Result = Option<PeerList>;

    fn handle(&mut self, _msg: GetPeerListGuard, _ctx: &mut Self::Context) -> Self::Result {
        Some(self.peer_list.clone())
    }
}

/// Add peer to the peer list
#[derive(Message, Debug)]
#[rtype(result = "()")]
struct AnnounceYourselfToPeerAndConnectReth {
    pub peer: PeerListItem,
}

impl<A> Handler<AnnounceYourselfToPeerAndConnectReth> for PeerNetworkService<A>
where
    A: ApiClient,
{
    type Result = ();

    fn handle(
        &mut self,
        msg: AnnounceYourselfToPeerAndConnectReth,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        debug!("AnnounceYourselfToPeer message received: {:?}", msg.peer);
        let peer_api_addr = msg.peer.address.api;
        let reth_peer_info = msg.peer.address.execution;
        let peer_service_addr = ctx.address();

        let version_request = self.create_version_request();
        let is_trusted_peer = self.peer_list.is_trusted_peer(&peer_api_addr);
        let handshake_task = Self::announce_yourself_to_address_task(
            self.irys_api_client.clone(),
            peer_api_addr,
            version_request,
            peer_service_addr,
            is_trusted_peer,
            self.config.node_config.peer_filter_mode,
            self.peer_list.clone(),
        );
        ctx.spawn(handshake_task.into_actor(self));
        let reth_task =
            Self::add_reth_peer_task(self.reth_service.clone(), reth_peer_info).into_actor(self);
        ctx.spawn(reth_task);
    }
}

/// Handle potential new peer
#[derive(Message, Debug)]
#[rtype(result = "()")]
struct NewPotentialPeer {
    pub api_address: SocketAddr,
    pub force_announce: bool,
}

impl NewPotentialPeer {
    fn new(api_address: SocketAddr) -> Self {
        Self {
            api_address,
            force_announce: false,
        }
    }

    fn force_announce(api_address: SocketAddr) -> Self {
        Self {
            api_address,
            force_announce: true,
        }
    }
}

impl<A> Handler<NewPotentialPeer> for PeerNetworkService<A>
where
    A: ApiClient,
{
    type Result = ();

    fn handle(&mut self, msg: NewPotentialPeer, ctx: &mut Self::Context) -> Self::Result {
        let self_address = self.peer_address.api;
        debug!("NewPotentialPeer message received: {:?}", msg.api_address);
        if msg.api_address == self_address {
            debug!("Ignoring self address");
            return;
        }

        // Check peer whitelist based on filter mode
        if !self.peer_list.is_peer_allowed(&msg.api_address) {
            debug!(
                "Peer {:?} is not in whitelist, ignoring based on filter mode: {:?}",
                msg.api_address, self.config.node_config.peer_filter_mode
            );
            return;
        }

        if self.successful_announcements.contains_key(&msg.api_address) && !msg.force_announce {
            debug!("Already announced to peer {:?}", msg.api_address);
            return;
        }

        let already_in_cache = self.peer_list.contains_api_address(&msg.api_address);
        let already_announcing = self
            .currently_running_announcements
            .contains(&msg.api_address);

        debug!("Already announcing: {:?}", already_announcing);
        debug!("Already in cache: {:?}", already_in_cache);
        let announcing_or_in_cache = already_announcing || already_in_cache;

        let needs_announce = msg.force_announce || !announcing_or_in_cache;

        if needs_announce {
            // Skip if peer is currently blacklisted
            if let Some(until) = blocklist_until()
                .lock()
                .expect("blocklist_until mutex poisoned")
                .get(&msg.api_address)
                .copied()
            {
                if std::time::Instant::now() < until {
                    debug!(
                        "Peer {:?} is blacklisted until {:?}, skipping announce",
                        msg.api_address, until
                    );
                    return;
                }
            }

            debug!("Need to announce yourself to peer {:?}", msg.api_address);
            self.currently_running_announcements.insert(msg.api_address);
            let version_request = self.create_version_request();
            let peer_service_addr = ctx.address();
            let is_trusted_peer = self.peer_list.is_trusted_peer(&msg.api_address);
            let peer_filter_mode = self.config.node_config.peer_filter_mode;
            let peer_list = self.peer_list.clone();

            let api_client = self.irys_api_client.clone();
            let addr = msg.api_address;
            let semaphore = handshake_semaphore_with_max(
                self.config
                    .node_config
                    .p2p_handshake
                    .max_concurrent_handshakes,
            );
            let handshake_task = async move {
                // Limit concurrent handshakes globally
                let _permit = semaphore.acquire().await.expect("semaphore closed");
                Self::announce_yourself_to_address_task(
                    api_client,
                    addr,
                    version_request,
                    peer_service_addr,
                    is_trusted_peer,
                    peer_filter_mode,
                    peer_list,
                )
                .await;
            };
            ctx.spawn(handshake_task.into_actor(self));
        }
    }
}

/// Handle potential new peer
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
struct AnnounceFinished {
    pub peer_api_address: SocketAddr,
    pub success: bool,
    pub retry: bool,
}

impl AnnounceFinished {
    fn retry(api_address: SocketAddr) -> Self {
        Self {
            peer_api_address: api_address,
            success: false,
            retry: true,
        }
    }

    fn success(api_address: SocketAddr) -> Self {
        Self {
            peer_api_address: api_address,
            success: true,
            retry: false,
        }
    }
}

impl<A> Handler<AnnounceFinished> for PeerNetworkService<A>
where
    A: ApiClient,
{
    type Result = ();

    fn handle(&mut self, msg: AnnounceFinished, ctx: &mut Self::Context) -> Self::Result {
        if !msg.success && msg.retry {
            self.currently_running_announcements
                .remove(&msg.peer_api_address);

            // Update failure count and compute backoff
            let attempts = {
                let mut guard = handshake_failures()
                    .lock()
                    .expect("handshake_failures mutex poisoned");
                let entry = guard.entry(msg.peer_api_address).or_insert(0);
                *entry += 1;
                *entry
            };

            if attempts >= self.config.node_config.p2p_handshake.max_retries {
                let until = std::time::Instant::now()
                    + std::time::Duration::from_secs(
                        self.config.node_config.p2p_handshake.blocklist_ttl_secs,
                    );
                blocklist_until()
                    .lock()
                    .expect("blocklist_until mutex poisoned")
                    .insert(msg.peer_api_address, until);
                handshake_failures()
                    .lock()
                    .expect("handshake_failures mutex poisoned")
                    .remove(&msg.peer_api_address);
                debug!(
                    "Peer {:?} blacklisted until {:?} after {} failures",
                    msg.peer_api_address, until, attempts
                );
                return;
            }

            let backoff_secs = (1_u64 << (attempts - 1))
                .saturating_mul(self.config.node_config.p2p_handshake.backoff_base_secs);
            let backoff_secs =
                backoff_secs.min(self.config.node_config.p2p_handshake.backoff_cap_secs);
            let backoff = std::time::Duration::from_secs(backoff_secs);

            let message = NewPotentialPeer::new(msg.peer_api_address);
            debug!(
                "Waiting for {:?} to try to announce yourself again (attempt {})",
                backoff, attempts
            );
            ctx.run_later(backoff, move |service, ctx| {
                debug!("Trying to run an announcement again");
                let address = ctx.address();
                ctx.spawn(send_message_and_print_error(message, address).into_actor(service));
            });
        } else if !msg.success && !msg.retry {
            self.failed_announcements
                .insert(msg.peer_api_address, msg.clone());
            self.currently_running_announcements
                .remove(&msg.peer_api_address);
        } else {
            self.successful_announcements
                .insert(msg.peer_api_address, msg.clone());
            self.currently_running_announcements
                .remove(&msg.peer_api_address);
            // Reset failure/blacklist state on success
            handshake_failures()
                .lock()
                .expect("handshake_failures mutex poisoned")
                .remove(&msg.peer_api_address);
            blocklist_until()
                .lock()
                .expect("blocklist_until mutex poisoned")
                .remove(&msg.peer_api_address);
        }
    }
}

/// Flush the peer list to the database
#[derive(Message, Debug)]
#[rtype(result = "Result<(), PeerListServiceError>")]
struct RequestDataFromTheNetwork {
    data_request: GossipDataRequest,
    use_trusted_peers_only: bool,
    retries: u8,
}

impl<A> Handler<RequestDataFromTheNetwork> for PeerNetworkService<A>
where
    A: ApiClient,
{
    type Result = ResponseActFuture<Self, Result<(), PeerListServiceError>>;

    fn handle(&mut self, msg: RequestDataFromTheNetwork, ctx: &mut Self::Context) -> Self::Result {
        let data_request = msg.data_request;
        let use_trusted_peers_only = msg.use_trusted_peers_only;
        let retries = msg.retries;
        let gossip_client = self.gossip_client.clone();
        let self_addr = ctx.address();
        // Capture config values to avoid borrowing self across async move
        let top_active_window = self.config.node_config.p2p_pull.top_active_window;
        let sample_size = self.config.node_config.p2p_pull.sample_size;

        Box::pin(
            async move {
                let peer_list = self_addr
                    .send(GetPeerListGuard)
                    .await
                    .map_err(PeerListServiceError::InternalSendError)?
                    .ok_or(PeerListServiceError::DatabaseNotConnected)?;

                let mut peers = if use_trusted_peers_only {
                    peer_list.online_trusted_peers()
                } else {
                    // Get the top 10 most active peers
                    peer_list.top_active_peers(Some(top_active_window), None)
                };

                // Shuffle peers to randomize the selection
                peers.shuffle(&mut rand::thread_rng());
                // Take random sample
                peers.truncate(sample_size);

                if peers.is_empty() {
                    return Err(PeerListServiceError::NoPeersAvailable);
                }

                // Try up to sample_size iterations over the peer list to get the block
                let mut last_error = None;

                // Cycle through peers per attempt; keep only peers that failed transiently
                let mut retryable_peers = peers.clone();

                for attempt in 1..=retries {
                    if retryable_peers.is_empty() {
                        break;
                    }

                    // Fan-out concurrently to all retryable peers in this round and accept first success

                    let current_round = retryable_peers.clone();
                    let mut futs = futures::stream::FuturesUnordered::new();

                    for peer in current_round {
                        let gc = gossip_client.clone();
                        let dr = data_request.clone();
                        let pl = peer_list.clone();
                        futs.push(async move {
                            let addr = peer.0;
                            let res = gc
                                .make_get_data_request_and_update_the_score(&peer, dr, &pl)
                                .await;
                            (addr, peer, res)
                        });
                    }

                    let mut next_retryable = Vec::new();

                    while let Some((address, peer, result)) = futs.next().await {
                        match result {
                            Ok(GossipResponse::Accepted(has)) => {
                                if has {
                                    info!(
                                        "Successfully requested {:?} from peer {}",
                                        data_request, address
                                    );
                                    // Drop remaining futures to cancel outstanding requests
                                    return Ok(());
                                } else {
                                    // Peer does not have the data yet; keep for future rounds
                                    debug!("Peer {} doesn't have {:?}", address, data_request);
                                    next_retryable.push(peer);
                                }
                            }
                            Ok(GossipResponse::Rejected(reason)) => {
                                warn!(
                                    "Peer {} rejected data request {:?}: {:?}",
                                    address, data_request, reason
                                );
                                match reason {
                                    RejectionReason::HandshakeRequired => {
                                        last_error = Some(GossipError::PeerNetwork(
                                            PeerNetworkError::FailedToRequestData(
                                                "Peer requires a handshake".to_string(),
                                            ),
                                        ));
                                        // Peer needs a handshake, send a NewPotentialPeer message
                                        self_addr
                                            .send(NewPotentialPeer::force_announce(
                                                peer.1.address.api,
                                            ))
                                            .await
                                            .map_err(PeerListServiceError::InternalSendError)?;
                                    }
                                    RejectionReason::GossipDisabled => {
                                        last_error = Some(GossipError::PeerNetwork(
                                            PeerNetworkError::FailedToRequestData(format!(
                                                "Peer {:?} has gossip disabled",
                                                address
                                            )),
                                        ));
                                    }
                                }
                                // Do not retry same peer on rejection
                            }
                            Err(err) => {
                                last_error = Some(err);
                                warn!(
                                    "Failed to fetch {:?} from peer {:?} (attempt {}/{}): {:?}",
                                    data_request,
                                    address,
                                    attempt,
                                    retries,
                                    last_error.as_ref().unwrap()
                                );
                                // Transient failure: keep peer for next round
                                next_retryable.push(peer);
                            }
                        }
                    }

                    retryable_peers = next_retryable;

                    // minimal delay between attempts, skip after final iteration
                    if attempt != retries {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }

                Err(PeerListServiceError::FailedToRequestData(format!(
                    "Failed to fetch {:?} after trying {} peers: {:?}",
                    data_request, sample_size, last_error
                )))
            }
            .into_actor(self),
        )
    }
}
