use crate::types::GossipDataRequest;
use crate::GossipClient;
use actix::prelude::*;
use irys_actors::reth_service::RethServiceActor;
use irys_api_client::{ApiClient, IrysApiClient};
use irys_database::insert_peer_list_item;
use irys_database::reth_db::{Database as _, DatabaseError};
use irys_domain::{
    PeerListDataError, PeerListDataMessage, PeerListGuard, ScoreDecreaseReason, ScoreIncreaseReason,
};
use irys_types::{
    build_user_agent, Address, Config, DatabaseProvider, PeerAddress, PeerListItem, PeerResponse,
    RejectedResponse, RethPeerInfo, VersionRequest,
};
use rand::prelude::SliceRandom as _;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tracing::{debug, error, info, warn};

const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const INACTIVE_PEERS_HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(10);
const PEER_HANDSHAKE_RETRY_INTERVAL: Duration = Duration::from_secs(5);

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

#[derive(Debug, Default)]
pub struct PeerListServiceWithClient<A, R>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    /// Reference to the node database
    db: Option<DatabaseProvider>,

    pub peer_list_data_guard: PeerListGuard,

    currently_running_announcements: HashSet<SocketAddr>,
    successful_announcements: HashMap<SocketAddr, AnnounceFinished>,
    failed_announcements: HashMap<SocketAddr, AnnounceFinished>,

    // This is related to networking - requesting data from the network and joining the network
    gossip_client: GossipClient,
    irys_api_client: A,

    chain_id: u64,
    peer_address: PeerAddress,

    reth_service_addr: Option<Addr<R>>,

    config: Option<Config>,

    peer_list_service_receiver: Option<UnboundedReceiver<PeerListDataMessage>>,
}

pub type PeerListService = PeerListServiceWithClient<IrysApiClient, RethServiceActor>;

impl PeerListServiceWithClient<IrysApiClient, RethServiceActor> {
    /// Create a new instance of the peer_list_service actor passing in a reference-counted
    /// reference to a `DatabaseEnv`
    pub fn new(
        db: DatabaseProvider,
        config: &Config,
        reth_service_addr: Addr<RethServiceActor>,
    ) -> Self {
        info!("service started: peer_list");
        Self::new_with_custom_api_client(db, config, IrysApiClient::new(), reth_service_addr)
    }
}

impl<A, R> PeerListServiceWithClient<A, R>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    /// Create a new instance of the peer_list_service actor passing in a reference-counted
    /// reference to a `DatabaseEnv`
    pub(crate) fn new_with_custom_api_client(
        db: DatabaseProvider,
        config: &Config,
        irys_api_client: A,
        reth_actor: Addr<R>,
    ) -> Self {
        let (service_sender, service_receiver) = tokio::sync::mpsc::unbounded_channel();
        let peer_list_data =
            PeerListGuard::new(config, &db, service_sender).expect("Failed to load peer list data");

        Self {
            db: Some(db),
            peer_list_data_guard: peer_list_data,
            currently_running_announcements: HashSet::new(),
            successful_announcements: HashMap::new(),
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
                execution: config.node_config.reth_peer_info,
            },
            reth_service_addr: Some(reth_actor),
            config: Some(config.clone()),
            peer_list_service_receiver: Some(service_receiver),
        }
    }
}

// TODO: this is a temporary solution to allow the peer list service to receive messages
impl<A, R> Handler<PeerListDataMessage> for PeerListServiceWithClient<A, R>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    type Result = ();

    fn handle(&mut self, msg: PeerListDataMessage, ctx: &mut Self::Context) {
        let address = ctx.address();
        match msg {
            PeerListDataMessage::PeerUpdated(peer_list_item) => {
                ctx.spawn(
                    async move {
                        let _res = address
                            .send(PeerUpdated {
                                peer: peer_list_item,
                            })
                            .await;
                    }
                    .into_actor(self),
                );
            }
            PeerListDataMessage::RequestBlockFromNetwork {
                block_hash,
                use_trusted_peers_only,
                response,
            } => {
                ctx.spawn(
                    async move {
                        match address
                            .send(RequestDataFromTheNetwork {
                                data_request: GossipDataRequest::Block(block_hash),
                                use_trusted_peers_only,
                            })
                            .await
                        {
                            Ok(res) => {
                                response
                                    .send(res.map_err(|err| {
                                        PeerListDataError::OtherInternalError(format!("{:?}", err))
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
                                    .send(Err(PeerListDataError::OtherInternalError(format!(
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
            PeerListDataMessage::RequestPayloadFromNetwork {
                payload_hash,
                use_trusted_peers_only,
                response,
            } => {
                ctx.spawn(
                    async move {
                        match address
                            .send(RequestDataFromTheNetwork {
                                data_request: GossipDataRequest::ExecutionPayload(payload_hash),
                                use_trusted_peers_only,
                            })
                            .await
                        {
                            Ok(res) => {
                                response
                                    .send(res.map_err(|err| {
                                        PeerListDataError::OtherInternalError(format!("{:?}", err))
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
                                    .send(Err(PeerListDataError::OtherInternalError(format!(
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

impl<A, R> Actor for PeerListServiceWithClient<A, R>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
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
                error!("Failed to flush peer list to database: {:?}", e);
            }
        });

        ctx.run_interval(INACTIVE_PEERS_HEALTH_CHECK_INTERVAL, |act, ctx| {
            // Collect inactive peers with the required fields
            let inactive_peers: Vec<(Address, PeerListItem, SocketAddr)> =
                act.peer_list_data_guard.inactive_peers();

            for (mining_addr, peer, ..) in inactive_peers {
                // Clone the peer address to use in the async block
                let peer_address = peer.address;
                let client = act.gossip_client.clone();
                // Create the future that does the health check
                let fut = async move { client.check_health(peer_address).await }
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
            self.peer_list_data_guard.trusted_peer_addresses(),
        )
        .into_actor(self);
        ctx.spawn(trusted_peers_handshake_task);

        // Announce yourself to the network
        let version_request = self.create_version_request();
        let api_client = self.irys_api_client.clone();
        let peers_cache = self.peer_list_data_guard.all_known_peers();
        let announce_fut = Self::announce_yourself_to_all_peers(
            api_client,
            version_request,
            peers_cache,
            peer_service_address,
            self.reth_service_addr.clone(),
        )
        .into_actor(self);
        ctx.spawn(announce_fut);
    }
}

/// Allows this actor to live in the the service registry
impl<A, R> Supervised for PeerListServiceWithClient<A, R>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
}

impl<A, R> SystemService for PeerListServiceWithClient<A, R>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>> + Default,
{
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

impl<A, R> PeerListServiceWithClient<A, R>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    fn flush(&self) -> Result<(), PeerListServiceError> {
        if let Some(db) = &self.db {
            db.update(|tx| {
                for (addr, peer) in self
                    .peer_list_data_guard
                    .all_know_peers_with_mining_address()
                    .iter()
                {
                    insert_peer_list_item(tx, addr, peer).map_err(PeerListServiceError::from)?;
                }
                Ok(())
            })
            .map_err(PeerListServiceError::Database)?
        } else {
            Err(PeerListServiceError::DatabaseNotConnected)
        }
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
        self.peer_list_data_guard
            .increase_peer_score(mining_addr, score);
    }

    fn decrease_peer_score(&mut self, mining_addr: &Address, reason: ScoreDecreaseReason) {
        self.peer_list_data_guard
            .decrease_peer_score(mining_addr, reason);
    }

    fn create_version_request(&self) -> VersionRequest {
        let signer = self
            .config
            .as_ref()
            .expect("Config must exist")
            .irys_signer();
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
    ) -> Result<(), PeerListServiceError> {
        let peer_response_result = api_client
            .post_version(api_address, version_request)
            .await
            .map_err(|e| {
                warn!(
                    "Failed to announce yourself to address {}: {:?}",
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
                for peer in accepted_peers.peers {
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

    async fn announce_yourself_to_all_peers(
        api_client: A,
        version_request: VersionRequest,
        known_peers_cache: Vec<PeerAddress>,
        peer_service_address: Addr<Self>,
        reth_service_address: Option<Addr<R>>,
    ) {
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
                    // TODO @antouhou do we need this here?
                    if let Some(ref reth_service_address) = reth_service_address {
                        let _ = reth_service_address
                            .send(peer.execution)
                            .await
                            .inspect_err(|e| error!("Failed to connect to reth peer {}", &e));
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to announce yourself to address {}: {:?}",
                        peer.api, e
                    );
                }
            }
        }
    }
}

#[derive(Message, Debug)]
#[rtype(result = "Option<PeerListGuard>")]
pub struct GetPeerListGuard;

impl<T, R> Handler<GetPeerListGuard> for PeerListServiceWithClient<T, R>
where
    T: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    type Result = Option<PeerListGuard>;

    fn handle(&mut self, _msg: GetPeerListGuard, _ctx: &mut Self::Context) -> Self::Result {
        Some(self.peer_list_data_guard.clone())
    }
}

/// Flush the peer list to the database
#[derive(Message, Debug)]
#[rtype(result = "Result<(), PeerListServiceError>")]
struct FlushRequest;

impl<A, R> Handler<FlushRequest> for PeerListServiceWithClient<A, R>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    type Result = Result<(), PeerListServiceError>;

    fn handle(&mut self, _msg: FlushRequest, _ctx: &mut Self::Context) -> Self::Result {
        self.flush()
    }
}

/// Add peer to the peer list
#[derive(Message, Debug)]
#[rtype(result = "()")]
struct PeerUpdated {
    pub peer: PeerListItem,
}

impl<A, R> Handler<PeerUpdated> for PeerListServiceWithClient<A, R>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    type Result = ();

    fn handle(&mut self, msg: PeerUpdated, ctx: &mut Self::Context) -> Self::Result {
        debug!("PeerUpdated message received: {:?}", msg.peer);
        let peer_api_addr = msg.peer.address.api;
        let reth_peer_info = msg.peer.address.execution;
        let peer_service_addr = ctx.address();

        let version_request = self.create_version_request();
        let handshake_task = Self::announce_yourself_to_address_task(
            self.irys_api_client.clone(),
            peer_api_addr,
            version_request,
            peer_service_addr,
        );
        ctx.spawn(handshake_task.into_actor(self));
        if let Some(reth_service_addr) = &self.reth_service_addr {
            let future = reth_service_addr.send(reth_peer_info);
            let reth_task = async move {
                match future.await {
                    Ok(res) => match res {
                        Ok(()) => {
                            debug!("Successfully connected to reth peer: {:?}", reth_peer_info);
                        }
                        Err(reth_error) => {
                            error!("Failed to connect to reth peer: {}", reth_error.to_string());
                        }
                    },
                    Err(mailbox_error) => {
                        error!("Failed to connect to reth peer: {}", mailbox_error);
                    }
                }
            }
            .into_actor(self);
            ctx.spawn(reth_task);
        } else {
            warn!("Reth service address is not set in the peer list service");
        }
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

impl<A, R> Handler<NewPotentialPeer> for PeerListServiceWithClient<A, R>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    type Result = ();

    fn handle(&mut self, msg: NewPotentialPeer, ctx: &mut Self::Context) -> Self::Result {
        let self_address = self.peer_address.api;
        debug!("NewPotentialPeer message received: {:?}", msg.api_address);
        if msg.api_address == self_address {
            debug!("Ignoring self address");
            return;
        }

        if self.successful_announcements.contains_key(&msg.api_address) && !msg.force_announce {
            debug!("Already announced to peer {:?}", msg.api_address);
            return;
        }

        let already_in_cache = self
            .peer_list_data_guard
            .contains_api_address(&msg.api_address);
        let already_announcing = self
            .currently_running_announcements
            .contains(&msg.api_address);

        debug!("Already announcing: {:?}", already_announcing);
        debug!("Already in cache: {:?}", already_in_cache);
        let announcing_or_in_cache = already_announcing || already_in_cache;

        let needs_announce = msg.force_announce || !announcing_or_in_cache;

        if needs_announce {
            debug!("Need to announce yourself to peer {:?}", msg.api_address);
            self.currently_running_announcements.insert(msg.api_address);
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

impl<A, R> Handler<AnnounceFinished> for PeerListServiceWithClient<A, R>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    type Result = ();

    fn handle(&mut self, msg: AnnounceFinished, ctx: &mut Self::Context) -> Self::Result {
        if !msg.success && msg.retry {
            self.currently_running_announcements
                .remove(&msg.peer_api_address);
            let message = NewPotentialPeer::new(msg.peer_api_address);
            debug!(
                "Waiting for {:?} to try to announce yourself again",
                PEER_HANDSHAKE_RETRY_INTERVAL
            );
            ctx.run_later(PEER_HANDSHAKE_RETRY_INTERVAL, move |service, ctx| {
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
        }
    }
}

/// Flush the peer list to the database
#[derive(Message, Debug)]
#[rtype(result = "Result<(), PeerListServiceError>")]
struct RequestDataFromTheNetwork {
    data_request: GossipDataRequest,
    use_trusted_peers_only: bool,
}

impl<A, R> Handler<RequestDataFromTheNetwork> for PeerListServiceWithClient<A, R>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
{
    type Result = ResponseActFuture<Self, Result<(), PeerListServiceError>>;

    fn handle(&mut self, msg: RequestDataFromTheNetwork, ctx: &mut Self::Context) -> Self::Result {
        let data_request = msg.data_request;
        let use_trusted_peers_only = msg.use_trusted_peers_only;
        let gossip_client = self.gossip_client.clone();
        let self_addr = ctx.address();

        Box::pin(
            async move {
                let peer_list = self_addr
                    .send(GetPeerListGuard)
                    .await
                    .map_err(PeerListServiceError::InternalSendError)?
                    .ok_or(PeerListServiceError::DatabaseNotConnected)?;

                let mut peers = if use_trusted_peers_only {
                    peer_list.trusted_peers()
                } else {
                    // Get the top 10 most active peers
                    peer_list.top_active_peers(Some(10), None)
                };

                // Shuffle peers to randomize the selection
                peers.shuffle(&mut rand::thread_rng());
                // Take random 5
                peers.truncate(5);

                if peers.is_empty() {
                    return Err(PeerListServiceError::NoPeersAvailable);
                }

                // Try up to 5 peers to get the block
                let mut last_error = None;

                for peer in peers {
                    for attempt in 1..=5 {
                        let address = &peer.0;
                        debug!(
                            "Attempting to fetch {:?} from peer {} (attempt {}/5)",
                            data_request, address, attempt
                        );

                        match gossip_client
                            .make_get_data_request_and_update_the_score(
                                &peer,
                                data_request.clone(),
                                &peer_list,
                            )
                            .await
                        {
                            Ok(true) => {
                                info!(
                                    "Successfully requested {:?} from peer {}",
                                    data_request, address
                                );

                                return Ok(());
                            }
                            Ok(false) => {
                                // Peer doesn't have this block, try another peer
                                debug!("Peer {} doesn't have {:?}", address, data_request);
                                continue;
                            }
                            Err(err) => {
                                last_error = Some(err);
                                warn!(
                                    "Failed to fetch {:?} from peer {} (attempt {}/5): {}",
                                    data_request,
                                    address,
                                    attempt,
                                    last_error.as_ref().unwrap()
                                );

                                // Continue trying with the same peer if not the last attempt
                                if attempt < 5 {
                                    continue;
                                }
                            }
                        }
                    }
                }

                Err(PeerListServiceError::FailedToRequestData(format!(
                    "Failed to fetch {:?} after trying 5 peers: {:?}",
                    data_request, last_error
                )))
            }
            .into_actor(self),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_api_client::test_utils::CountingMockClient;
    use irys_database::tables::PeerListItems;
    use irys_database::walk_all;
    use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::peer_list::PeerScore;
    use irys_types::{NodeConfig, RethPeerInfo, VersionRequest};
    use std::collections::HashSet;
    use std::net::IpAddr;
    use std::str::FromStr as _;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// MockRethServiceActor for testing
    #[derive(Default)]
    struct MockRethServiceActor {
        received_peers: Vec<RethPeerInfo>,
        response: Option<eyre::Result<()>>,
    }

    impl MockRethServiceActor {
        fn new() -> Self {
            Self {
                received_peers: Vec::new(),
                response: None,
            }
        }

        /// Create a new mock that will return success when called
        fn with_success() -> Self {
            Self {
                received_peers: Vec::new(),
                response: Some(Ok(())),
            }
        }

        /// Get a clone of the received peers vector
        fn get_received_peers(&self) -> Vec<RethPeerInfo> {
            self.received_peers.clone()
        }
    }

    impl Actor for MockRethServiceActor {
        type Context = Context<Self>;
    }

    impl Handler<RethPeerInfo> for MockRethServiceActor {
        type Result = eyre::Result<()>;

        fn handle(&mut self, msg: RethPeerInfo, _ctx: &mut Self::Context) -> Self::Result {
            let peer_info = msg;

            self.received_peers.push(peer_info);

            self.response.take().unwrap_or(Ok(()))
        }
    }

    #[derive(Message, Debug)]
    #[rtype(result = "Vec<RethPeerInfo>")]
    struct MockGetPeersRequest;

    impl Handler<MockGetPeersRequest> for MockRethServiceActor {
        type Result = Vec<RethPeerInfo>;

        fn handle(&mut self, _msg: MockGetPeersRequest, _ctx: &mut Self::Context) -> Self::Result {
            self.get_received_peers()
        }
    }

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
        let config = NodeConfig::testnet().into();
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));

        // Create a mock reth service actor
        let mock_reth_actor = MockRethServiceActor::new();
        let reth_actor = mock_reth_actor.start();

        // Use our custom mock client
        let mock_api_client = CountingMockClient::default();

        // Create service with our mocks
        let service = PeerListServiceWithClient::new_with_custom_api_client(
            db,
            &config,
            mock_api_client,
            reth_actor,
        );

        // Test adding a new peer
        let (mining_addr, peer) = create_test_peer(
            "0x1234567890123456789012345678901234567890",
            8080,
            true,
            None,
        );

        // Add peer using guard
        service
            .peer_list_data_guard
            .add_or_update_peer(mining_addr, peer.clone());

        // Verify peer was added correctly
        let result = service
            .peer_list_data_guard
            .peer_by_gossip_address(peer.address.gossip);

        assert!(result.is_some());
        assert_eq!(result.expect("get peer"), peer);

        // Verify known peers using KnownPeersRequest
        let known_peers = service.peer_list_data_guard.all_known_peers();
        assert!(known_peers.contains(&peer.address));
    }

    #[actix_rt::test]
    async fn test_peer_score_management() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config = NodeConfig::testnet().into();
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));
        let mock_client = CountingMockClient::default();
        let mock_actor = MockRethServiceActor::new();
        let mock_addr = mock_actor.start();
        let service = PeerListServiceWithClient::new_with_custom_api_client(
            db,
            &config,
            mock_client,
            mock_addr,
        );

        // Add a test peer
        let (mining_addr, peer) = create_test_peer(
            "0x1234567890123456789012345678901234567890",
            8080,
            true,
            None,
        );

        service
            .peer_list_data_guard
            .add_or_update_peer(mining_addr, peer.clone());
        // Test increasing score
        service
            .peer_list_data_guard
            .increase_peer_score(&mining_addr, ScoreIncreaseReason::Online);

        // Verify score increased
        let updated_peer = service
            .peer_list_data_guard
            .peer_by_gossip_address(peer.address.gossip)
            .expect("failed to get updated peer");

        assert_eq!(updated_peer.reputation_score.get(), 51);

        // Test decreasing score using message handler
        service
            .peer_list_data_guard
            .decrease_peer_score(&mining_addr, ScoreDecreaseReason::Offline);

        // Verify score decreased
        let updated_peer = service
            .peer_list_data_guard
            .peer_by_gossip_address(peer.address.gossip)
            .expect("failed to get updated peer");
        assert_eq!(updated_peer.reputation_score.get(), 48);
    }

    #[actix_rt::test]
    async fn test_active_peers_request() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config = NodeConfig::testnet().into();
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));
        let mock_client = CountingMockClient::default();
        let mock_actor = MockRethServiceActor::new();
        let mock_addr = mock_actor.start();
        let service = PeerListServiceWithClient::new_with_custom_api_client(
            db,
            &config,
            mock_client,
            mock_addr,
        );

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

        // Add peers
        service
            .peer_list_data_guard
            .add_or_update_peer(mining_addr1, peer1.clone());
        service
            .peer_list_data_guard
            .add_or_update_peer(mining_addr2, peer2.clone());
        service
            .peer_list_data_guard
            .add_or_update_peer(mining_addr3, peer3);

        // Test active peers request using message handler
        let exclude_peers = HashSet::new();
        let active_peers = service
            .peer_list_data_guard
            .top_active_peers(Some(2), Some(exclude_peers));

        assert_eq!(active_peers.len(), 2);
        assert_eq!(active_peers[0].1, peer1); // Higher score should be first
        assert_eq!(active_peers[1].1, peer2);
    }

    #[actix_rt::test]
    async fn test_edge_cases() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config = NodeConfig::testnet().into();
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));
        let mock_client = CountingMockClient::default();
        let mock_actor = MockRethServiceActor::new();
        let mock_addr = mock_actor.start();
        let service = PeerListServiceWithClient::new_with_custom_api_client(
            db,
            &config,
            mock_client,
            mock_addr,
        );

        // Test adding duplicate peer
        let (mining_addr, peer) = create_test_peer(
            "0x1234567890123456789012345678901234567890",
            8080,
            true,
            None,
        );

        // Add same peer twice
        service
            .peer_list_data_guard
            .add_or_update_peer(mining_addr, peer.clone());
        service
            .peer_list_data_guard
            .add_or_update_peer(mining_addr, peer);

        // Verify only one entry exists using KnownPeersRequest
        let known_peers = service.peer_list_data_guard.all_known_peers();
        assert_eq!(known_peers.len(), 1);

        // Test peer lookup with non-existent address
        let non_existent_addr = Address::from_str("0xDEAD111111111111111111111111111111111111")
            .expect("expected valid mining address");
        let non_existent_gossip_addr =
            SocketAddr::new(IpAddr::from_str("192.168.1.1").expect("invalid IP"), 9999);
        let result = service
            .peer_list_data_guard
            .peer_by_gossip_address(non_existent_gossip_addr);
        assert!(result.is_none());

        // Test score manipulation for non-existent peer using message handlers
        service
            .peer_list_data_guard
            .increase_peer_score(&non_existent_addr, ScoreIncreaseReason::Online);
        service
            .peer_list_data_guard
            .decrease_peer_score(&non_existent_addr, ScoreDecreaseReason::Offline);

        // Test active peers with empty list
        let new_temp_dir = setup_tracing_and_temp_dir(None, false);
        let new_test_db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&new_temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));
        let mock_client = CountingMockClient::default();
        let mock_actor = MockRethServiceActor::new();
        let mock_addr = mock_actor.start();
        let empty_service = PeerListServiceWithClient::new_with_custom_api_client(
            new_test_db,
            &config,
            mock_client,
            mock_addr,
        );

        let exclude_peers = HashSet::new();
        let active_peers = empty_service
            .peer_list_data_guard
            .top_active_peers(None, Some(exclude_peers));
        assert!(active_peers.is_empty());
    }

    #[actix_rt::test]
    async fn test_periodic_flush() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config = NodeConfig::testnet().into();
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));
        let mock_client = CountingMockClient::default();
        let mock_actor = MockRethServiceActor::new();
        let mock_addr = mock_actor.start();

        // Start the actor system with our service
        let service = PeerListServiceWithClient::new_with_custom_api_client(
            db.clone(),
            &config,
            mock_client,
            mock_addr,
        );
        let peer_list_data_guard = service.peer_list_data_guard.clone();
        service.start();

        // Give some time for the service to start
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Add a test peer
        let (mining_addr, peer) = create_test_peer(
            "0x1234567890123456789012345678901234567890",
            8080,
            true,
            None,
        );
        peer_list_data_guard.add_or_update_peer(mining_addr, peer.clone());

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
        let config = NodeConfig::testnet().into();
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));

        let mock_client = CountingMockClient::default();
        let mock_actor = MockRethServiceActor::new();
        let mock_addr = mock_actor.start();
        // Create first service instance and add some peers
        let mut service = PeerListServiceWithClient::new_with_custom_api_client(
            db.clone(),
            &config,
            mock_client,
            mock_addr,
        );
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

        service
            .peer_list_data_guard
            .add_or_update_peer(mining_addr1, peer1.clone());
        service
            .peer_list_data_guard
            .add_or_update_peer(mining_addr2, peer2.clone());

        // Manually flush data to database
        service
            .handle(FlushRequest, ctx)
            .expect("Failed to flush data");

        let mock_client = CountingMockClient::default();
        let mock_actor = MockRethServiceActor::new();
        let mock_addr = mock_actor.start();

        // Create new service instance that should load from database
        let new_service = PeerListServiceWithClient::new_with_custom_api_client(
            db,
            &config,
            mock_client,
            mock_addr,
        );

        // Verify peers were loaded correctly
        let loaded_peer1 = new_service
            .peer_list_data_guard
            .peer_by_gossip_address(peer1.address.gossip);
        let loaded_peer2 = new_service
            .peer_list_data_guard
            .peer_by_gossip_address(peer2.address.gossip);

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
        let known_peers = service.peer_list_data_guard.all_known_peers();
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
        let mut node_config = NodeConfig::testnet();
        node_config.trusted_peers = vec![];
        let config = Config::new(node_config);

        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));

        // Create first service instance and add some peers
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mock_client = CountingMockClient {
            post_version_calls: calls.clone(),
        };
        let mock_actor = MockRethServiceActor::new();
        let mock_addr = mock_actor.start();
        let peer_list_service = PeerListServiceWithClient::new_with_custom_api_client(
            db,
            &config,
            mock_client.clone(),
            mock_addr.clone(),
        );
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
        let known_peers = vec![peer1.address, peer2.address];
        let version_request = VersionRequest::default();

        PeerListServiceWithClient::announce_yourself_to_all_peers(
            mock_client,
            version_request,
            known_peers,
            addr,
            Some(mock_addr),
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
        let config = Config::new(NodeConfig::testnet());
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));
        let mock_actor = MockRethServiceActor::new();
        let mock_addr = mock_actor.start();
        let service = PeerListServiceWithClient::new_with_custom_api_client(
            db,
            &config,
            CountingMockClient::default(),
            mock_addr,
        );

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
        };

        let initial_peer = PeerListItem {
            address: initial_peer_addr,
            reputation_score: PeerScore::new(50),
            response_time: 100,
            last_seen: 123,
            is_online: true,
        };

        // Add the initial peer
        service
            .peer_list_data_guard
            .add_or_update_peer(mining_addr, initial_peer);

        // Verify the peer was added
        let initial_result = service
            .peer_list_data_guard
            .peer_by_gossip_address(initial_gossip_addr);
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
        };

        let updated_peer = PeerListItem {
            address: new_peer_addr,
            reputation_score: PeerScore::new(50),
            response_time: 100,
            last_seen: 123,
            is_online: true,
        };

        // Update the peer with new address
        service
            .peer_list_data_guard
            .add_or_update_peer(mining_addr, updated_peer);

        // Verify the peer address was updated
        let updated_result = service
            .peer_list_data_guard
            .peer_by_gossip_address(new_gossip_addr);
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
        let old_result = service
            .peer_list_data_guard
            .peer_by_gossip_address(initial_gossip_addr);
        assert!(
            old_result.is_none(),
            "Should not find peer with old gossip address"
        );
    }

    #[actix_rt::test]
    async fn test_reth_actor_receives_reth_peer_info() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config = NodeConfig::testnet().into();
        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));

        // Create a mock reth service actor to track calls
        let mock_reth_actor = MockRethServiceActor::with_success();
        let reth_actor = mock_reth_actor.start();

        // Create mock api client
        let mock_api_client = CountingMockClient::default();

        // Create service with our mocks
        let service = PeerListServiceWithClient::new_with_custom_api_client(
            db,
            &config,
            mock_api_client,
            reth_actor.clone(),
        );
        let peer_list_data_guard = service.peer_list_data_guard.clone();
        service.start();

        // Give some time for the service to start
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Create a test peer with a specific RethPeerInfo
        let mining_addr = Address::from_str("0x1234567890123456789012345678901234567890")
            .expect("Invalid mining address");

        let test_reth_peer_info = RethPeerInfo {
            peering_tcp_addr: "192.168.1.100:30303".parse().unwrap(),
            peer_id: "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890".parse().unwrap()
        };

        let peer = PeerListItem {
            address: PeerAddress {
                gossip: "127.0.0.1:8080".parse().unwrap(),
                api: "127.0.0.1:8081".parse().unwrap(),
                execution: test_reth_peer_info,
            },
            reputation_score: PeerScore::new(50),
            response_time: 100,
            last_seen: 123,
            is_online: true,
        };

        // Add the peer to the service
        peer_list_data_guard.add_or_update_peer(mining_addr, peer.clone());

        // Give some time for async processing
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Check if the reth actor received the correct RethPeerInfo
        let received_peers = reth_actor
            .send(MockGetPeersRequest)
            .await
            .expect("Failed to send message");
        assert_eq!(
            received_peers.len(),
            1,
            "RethServiceActor should have received exactly one peer"
        );
        assert_eq!(
            received_peers[0], test_reth_peer_info,
            "RethServiceActor received incorrect peer info"
        );
    }

    #[actix_rt::test]
    async fn should_perform_handshake_when_adding_a_peer() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let mut node_config = NodeConfig::testnet();
        node_config.trusted_peers = vec![];
        let config = Config::new(node_config);

        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));

        // Create a mock client to track API calls
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mock_client = CountingMockClient {
            post_version_calls: calls.clone(),
        };
        let mock_actor = MockRethServiceActor::new();
        let mock_addr = mock_actor.start();

        // Create the service with our mock client instead of the real one
        let service = PeerListServiceWithClient::new_with_custom_api_client(
            db,
            &config,
            mock_client,
            mock_addr,
        );
        let peer_list_data_guard = service.peer_list_data_guard.clone();
        service.start();

        // Create a test peer
        let (mining_addr, peer) = create_test_peer(
            "0x1234567890123456789012345678901234567890",
            8080,
            true,
            None,
        );

        {
            let calls = calls.lock().await;
            assert_eq!(calls.len(), 0, "Shouldn't have made API calls just yet");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Add the peer which should trigger announce_yourself_to_address_task
        peer_list_data_guard.add_or_update_peer(mining_addr, peer.clone());

        tokio::time::sleep(Duration::from_millis(10)).await;

        // Verify the API call was made to the peer's API address
        let calls = calls.lock().await;
        debug!("API calls made: {:?}", calls);
        assert_eq!(calls.len(), 1, "Should have made one API call");
        assert!(
            calls.contains(&peer.address.api),
            "Should have called the peer's API address"
        );
    }

    #[actix_rt::test]
    async fn should_prevent_infinite_handshake_loop() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let mut node_config = NodeConfig::testnet();
        node_config.trusted_peers = vec![];
        let config = Config::new(node_config);

        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));

        // Create a mock client to track API calls
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mock_client = CountingMockClient {
            post_version_calls: calls.clone(),
        };
        let mock_actor = MockRethServiceActor::new();
        let mock_addr = mock_actor.start();

        // Create the service with our mock client instead of the real one
        let service = PeerListServiceWithClient::new_with_custom_api_client(
            db,
            &config,
            mock_client,
            mock_addr,
        );
        let peer_list_data_guard = service.peer_list_data_guard.clone();
        let service_addr = service.start();

        // Give some time for the service to start
        tokio::time::sleep(Duration::from_millis(10)).await;

        // Create a test peer
        let (mining_addr, peer) = create_test_peer(
            "0x1234567890123456789012345678901234567890",
            8080,
            true,
            None,
        );

        // Add the peer which should trigger announce_yourself_to_address_task
        peer_list_data_guard.add_or_update_peer(mining_addr, peer.clone());

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
                success: true,
                retry: false,
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

    #[actix_rt::test]
    async fn test_wait_for_active_peer() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let mut node_config = NodeConfig::testnet();
        node_config.trusted_peers = vec![];
        let config = Config::new(node_config);

        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));

        // Create the service with an empty peer list (no trusted peers)
        let mock_client = CountingMockClient::default();
        let mock_actor = MockRethServiceActor::new();
        let mock_addr = mock_actor.start();
        let service = PeerListServiceWithClient::new_with_custom_api_client(
            db,
            &config,
            mock_client,
            mock_addr,
        );
        let peer_list_data_guard = service.peer_list_data_guard.clone();
        service.start();

        // Verify we don't have any peers
        let active_peers = peer_list_data_guard.top_active_peers(None, None);

        assert!(active_peers.is_empty(), "Should start with no active peers");

        // Create a test peer to add later
        let (mining_addr, peer) = create_test_peer(
            "0x1234567890123456789012345678901234567890",
            8080,
            true,
            None,
        );

        // Start two tasks in parallel
        // 1. Send WaitForActivePeer and await the result
        let wait_task = tokio::spawn({
            let peer_list_data_guard = peer_list_data_guard.clone();
            async move {
                // Send WaitForActivePeer message which should only resolve when a peer becomes available
                peer_list_data_guard.wait_for_active_peers().await;
                debug!("WaitForActivePeer message resolved");
            }
        });

        // 2. Wait a bit and then add a peer
        let add_task = tokio::spawn({
            let peer_list_data_guard = peer_list_data_guard.clone();
            let peer = peer.clone();
            async move {
                // Wait a bit to ensure the other task is waiting
                tokio::time::sleep(Duration::from_millis(100)).await;
                debug!("Adding peer");
                peer_list_data_guard.add_or_update_peer(mining_addr, peer);
            }
        });

        // Wait for both tasks to complete
        tokio::try_join!(wait_task, add_task).expect("tasks should complete successfully");

        // Verify we now have a peer
        let active_peers = peer_list_data_guard.top_active_peers(None, None);

        assert_eq!(active_peers.len(), 1, "Should have one active peer");
        assert_eq!(active_peers[0].1, peer, "Should be the peer we added");
    }

    #[actix_rt::test]
    async fn test_wait_for_active_peer_no_peers() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let mut node_config = NodeConfig::testnet();
        node_config.trusted_peers = vec![];
        let config = Config::new(node_config);

        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));

        // Create the service with an empty peer list (no trusted peers)
        let mock_client = CountingMockClient::default();
        let mock_actor = MockRethServiceActor::new();
        let mock_addr = mock_actor.start();
        let service = PeerListServiceWithClient::new_with_custom_api_client(
            db,
            &config,
            mock_client,
            mock_addr,
        );
        let peer_list_data_guard = service.peer_list_data_guard.clone();
        service.start();

        // Verify we don't have any peers
        let active_peers = peer_list_data_guard.top_active_peers(None, None);

        assert!(active_peers.is_empty(), "Should start with no active peers");

        // Use a short timeout to verify that WaitForActivePeer doesn't resolve
        // We expect this task to timeout since no peers will be added
        let wait_task = tokio::spawn({
            let peer_list_data_guard = peer_list_data_guard.clone();
            async move {
                let wait_result = tokio::time::timeout(
                    Duration::from_millis(500),
                    peer_list_data_guard.wait_for_active_peers(),
                )
                .await;

                // The timeout should expire before WaitForActivePeer resolves
                match wait_result {
                    Ok(_) => panic!("WaitForActivePeer should not have resolved without peers"),
                    Err(_timeout_error) => {
                        debug!("Expected timeout occurred. WaitForActivePeer did not resolve as expected.");
                    }
                }
            }
        });

        // Wait for the task to complete (it should timeout)
        wait_task.await.expect("wait task should complete");

        // Verify we still have no peers
        let active_peers = peer_list_data_guard.top_active_peers(None, None);

        assert!(active_peers.is_empty(), "Should still have no active peers");
    }

    #[actix_rt::test]
    async fn test_initial_handshake_with_trusted_peers() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);

        // Create trusted peers for configuration
        let trusted_peer1 = PeerAddress {
            gossip: "127.0.0.1:9001".parse().expect("valid SocketAddr expected"),
            api: "127.0.0.1:9002".parse().expect("valid SocketAddr expected"),
            execution: RethPeerInfo::default(),
        };

        let trusted_peer2 = PeerAddress {
            gossip: "127.0.0.1:9003".parse().expect("valid SocketAddr expected"),
            api: "127.0.0.1:9004".parse().expect("valid SocketAddr expected"),
            execution: RethPeerInfo::default(),
        };

        // Create config with trusted peers
        let mut node_config = NodeConfig::testnet();
        node_config.trusted_peers = vec![trusted_peer1, trusted_peer2];
        let config = Config::new(node_config);

        let db = DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                .expect("can't open temp dir"),
        ));

        // Create a mock client to track API calls
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mock_client = CountingMockClient {
            post_version_calls: calls.clone(),
        };
        let mock_actor = MockRethServiceActor::new();
        let mock_addr = mock_actor.start();

        // Create and start the service with our mock client
        let service = PeerListServiceWithClient::new_with_custom_api_client(
            db,
            &config,
            mock_client,
            mock_addr,
        );
        let _service_addr = service.start();

        // Give time for handshake to process
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Verify handshake calls were made to all trusted peers
        let api_calls = calls.lock().await;
        assert_eq!(
            api_calls.len(),
            2,
            "Should have made API calls to all trusted peers"
        );

        assert!(
            api_calls.contains(&trusted_peer1.api),
            "Should have called the first trusted peer's API address"
        );

        assert!(
            api_calls.contains(&trusted_peer2.api),
            "Should have called the second trusted peer's API address"
        );
    }
}
