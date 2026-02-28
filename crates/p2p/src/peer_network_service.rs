use crate::types::{GossipResponse, RejectionReason};
use crate::{gossip_client::GossipClientError, GossipClient, GossipError};
use eyre::{Report, Result as EyreResult};
use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt as _};
use irys_database::insert_peer_list_item;
use irys_database::reth_db::{Database as _, DatabaseError};
use irys_domain::{PeerEvent, PeerList, ScoreDecreaseReason, ScoreIncreaseReason};
use irys_types::v2::GossipDataRequestV2;
use irys_types::{
    build_user_agent, AnnouncementFinishedMessage, Config, DatabaseProvider, HandshakeMessage,
    HandshakeRequest, HandshakeRequestV2, IrysPeerId, NetworkConfigWithDefaults as _, PeerAddress,
    PeerFilterMode, PeerListItem, PeerNetworkError, PeerNetworkSender, PeerNetworkServiceMessage,
    PeerResponse, ProtocolVersion, RejectedResponse, RethPeerInfo, TokioServiceHandle,
};
use moka::sync::Cache;
use rand::prelude::SliceRandom as _;
use reth::tasks::shutdown::{signal, Shutdown};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tokio::sync::{broadcast, mpsc::UnboundedReceiver};
use tokio::time::{interval, sleep, MissedTickBehavior};
use tracing::{debug, error, info, instrument, warn, Instrument as _};

const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const INACTIVE_PEERS_HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(10);
const SUCCESSFUL_ANNOUNCEMENT_CACHE_TTL: Duration = Duration::from_secs(30);

fn send_message_and_log_error(sender: &PeerNetworkSender, message: PeerNetworkServiceMessage) {
    if let Err(error) = sender.send(message) {
        error!(
            "Failed to send message to peer network service: {:?}",
            error
        );
    }
}
type RethPeerSender = Arc<dyn Fn(RethPeerInfo) -> BoxFuture<'static, ()> + Send + Sync>;
struct PeerNetworkService {
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<PeerNetworkServiceMessage>,
    inner: Arc<PeerNetworkServiceInner>,
}

struct PeerNetworkServiceInner {
    peer_list: PeerList,
    state: Mutex<PeerNetworkServiceState>,
    sender: PeerNetworkSender,
}

struct PeerNetworkServiceState {
    db: DatabaseProvider,
    currently_running_announcements: HashSet<SocketAddr>,
    successful_announcements: Cache<SocketAddr, AnnouncementFinishedMessage>,
    failed_announcements: HashMap<SocketAddr, AnnouncementFinishedMessage>,
    gossip_client: GossipClient,
    chain_id: u64,
    peer_address: PeerAddress,
    reth_peer_sender: RethPeerSender,
    config: Config,

    // Per-node handshake/backoff state
    handshake_failures: HashMap<SocketAddr, u32>,
    blocklist_until: HashMap<SocketAddr, std::time::Instant>,
    peers_limit: usize,
    handshake_semaphore: Arc<tokio::sync::Semaphore>,
}

struct HandshakeTask {
    api_address: SocketAddr,
    gossip_address: SocketAddr,
    inner: Arc<PeerNetworkServiceInner>,
    is_trusted_peer: bool,
    peer_filter_mode: PeerFilterMode,
    peer_list: PeerList,
    gossip_client: GossipClient,
    sender: PeerNetworkSender,
    semaphore: Arc<tokio::sync::Semaphore>,
    peers_limit: usize,
}
#[derive(Debug, Clone)]
pub enum PeerListServiceError {
    DatabaseNotConnected,
    Database(DatabaseError),
    PostVersionError(String),
    PeerHandshakeRejected(RejectedResponse),
    NoPeersAvailable,
    InternalSendError(String),
    FailedToRequestData(String),
}
impl From<DatabaseError> for PeerListServiceError {
    fn from(err: DatabaseError) -> Self {
        Self::Database(err)
    }
}

impl From<Report> for PeerListServiceError {
    fn from(err: Report) -> Self {
        Self::Database(DatabaseError::Other(err.to_string()))
    }
}
fn build_peer_address(config: &Config) -> PeerAddress {
    PeerAddress {
        gossip: format!(
            "{}:{}",
            config
                .node_config
                .gossip
                .public_ip(&config.node_config.network_defaults),
            config.node_config.gossip.public_port
        )
        .parse()
        .expect("valid SocketAddr expected"),
        api: format!(
            "{}:{}",
            config
                .node_config
                .http
                .public_ip(&config.node_config.network_defaults),
            config.node_config.http.public_port
        )
        .parse()
        .expect("valid SocketAddr expected"),
        execution: RethPeerInfo {
            peering_tcp_addr: format!(
                "{}:{}",
                config
                    .node_config
                    .reth
                    .network
                    .public_ip(&config.node_config.network_defaults),
                config.node_config.reth.network.public_port
            )
            .parse()
            .expect("valid SocketAddr expected"),
            peer_id: config.node_config.reth.network.peer_id,
        },
    }
}
impl PeerNetworkServiceState {
    fn create_handshake_request_v1(&self) -> HandshakeRequest {
        let mut handshake_request = HandshakeRequest {
            address: self.peer_address,
            chain_id: self.chain_id,
            protocol_version: ProtocolVersion::V1,
            user_agent: Some(build_user_agent("Irys-Node", env!("CARGO_PKG_VERSION"))),
            ..HandshakeRequest::default()
        };
        self.config
            .irys_signer()
            .sign_p2p_handshake_v1(&mut handshake_request)
            .expect("Failed to sign handshake request");
        handshake_request
    }

    fn create_handshake_request_v2(&self, protocol_version: ProtocolVersion) -> HandshakeRequestV2 {
        let peer_id = self.config.peer_id();
        let mut handshake_request = HandshakeRequestV2 {
            address: self.peer_address,
            chain_id: self.chain_id,
            peer_id,
            protocol_version,
            user_agent: Some(build_user_agent("Irys-Node", env!("CARGO_PKG_VERSION"))),
            consensus_config_hash: self.config.consensus.keccak256_hash(),
            ..HandshakeRequestV2::default()
        };
        self.config
            .irys_signer()
            .sign_p2p_handshake_v2(&mut handshake_request)
            .expect("Failed to sign handshake request");
        handshake_request
    }
}

impl PeerNetworkServiceInner {
    fn new(
        db: DatabaseProvider,
        config: Config,
        reth_peer_sender: RethPeerSender,
        peer_list: PeerList,
        sender: PeerNetworkSender,
    ) -> Self {
        let peer_address = build_peer_address(&config);
        let peers_limit = config.node_config.p2p_handshake.max_peers_per_response;
        let handshake_semaphore = Arc::new(tokio::sync::Semaphore::new(
            config.node_config.p2p_handshake.max_concurrent_handshakes,
        ));

        let state = PeerNetworkServiceState {
            db,
            currently_running_announcements: HashSet::new(),
            successful_announcements: Cache::builder()
                .time_to_live(SUCCESSFUL_ANNOUNCEMENT_CACHE_TTL)
                .build(),
            failed_announcements: HashMap::new(),
            gossip_client: GossipClient::new(
                Duration::from_secs(5),
                config.node_config.miner_address(),
                config.peer_id(),
            ),
            chain_id: config.consensus.chain_id,
            peer_address,
            reth_peer_sender,
            config,
            handshake_failures: HashMap::new(),
            blocklist_until: HashMap::new(),
            peers_limit,
            handshake_semaphore,
        };

        Self {
            peer_list,
            state: Mutex::new(state),
            sender,
        }
    }

    async fn flush(&self) -> Result<(), PeerListServiceError> {
        let db = {
            let state = self.state.lock().await;
            state.db.clone()
        };

        let persistable_peers = self.peer_list.persistable_peers_with_mining_addr();
        let _ = db
            .update(|tx| {
                for (peer_id, peer) in persistable_peers.iter() {
                    insert_peer_list_item(tx, peer_id, peer).map_err(PeerListServiceError::from)?;
                }
                Ok::<(), PeerListServiceError>(())
            })
            .map_err(PeerListServiceError::Database)?;

        Ok(())
    }

    fn increase_peer_score(&self, peer_id: &IrysPeerId, reason: ScoreIncreaseReason) {
        self.peer_list
            .increase_peer_score_by_peer_id(peer_id, reason);
    }

    fn decrease_peer_score(&self, peer_id: &IrysPeerId, reason: ScoreDecreaseReason) {
        self.peer_list
            .decrease_peer_score_by_peer_id(peer_id, reason);
    }

    async fn create_handshake_request_v1(&self) -> HandshakeRequest {
        let state = self.state.lock().await;
        state.create_handshake_request_v1()
    }

    async fn create_handshake_request_v2(
        &self,
        protocol_version: ProtocolVersion,
    ) -> HandshakeRequestV2 {
        let state = self.state.lock().await;
        state.create_handshake_request_v2(protocol_version)
    }

    fn sender(&self) -> PeerNetworkSender {
        self.sender.clone()
    }

    fn peer_list(&self) -> PeerList {
        self.peer_list.clone()
    }
}
impl PeerNetworkService {
    fn new(
        shutdown: Shutdown,
        msg_rx: UnboundedReceiver<PeerNetworkServiceMessage>,
        inner: Arc<PeerNetworkServiceInner>,
    ) -> Self {
        Self {
            shutdown,
            msg_rx,
            inner,
        }
    }

    async fn start(mut self) -> EyreResult<()> {
        info!("starting peer network service");

        let sender = self.inner.sender();
        let peer_list = self.inner.peer_list();

        let trusted_peers = peer_list.trusted_peer_api_to_gossip_addresses();
        Self::trusted_peers_handshake_task(sender.clone(), trusted_peers);

        let initial_peers: HashMap<IrysPeerId, PeerListItem> = peer_list
            .all_peers()
            .iter()
            .map(|(peer_id, peer)| (*peer_id, peer.clone()))
            .collect();
        Self::spawn_announce_yourself_to_all_peers_task(initial_peers, sender.clone());

        let mut flush_interval = interval(FLUSH_INTERVAL);
        flush_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut health_interval = interval(INACTIVE_PEERS_HEALTH_CHECK_INTERVAL);
        health_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = &mut self.shutdown => {
                    info!("Shutdown signal received for peer network service");
                    break;
                }
                maybe_msg = self.msg_rx.recv() => {
                    match maybe_msg {
                        Some(msg) => self.handle_message(msg).await,
                        None => {
                            warn!("Peer network service channel closed");
                            break;
                        }
                    }
                }
                _ = flush_interval.tick() => {
                    if let Err(err) = self.inner.flush().await {
                        error!("Failed to flush the peer list to the database: {:?}", err);
                    }
                }
                _ = health_interval.tick() => {
                    self.run_inactive_peers_health_check().await;
                }
            }
        }

        Ok(())
    }

    async fn handle_message(&self, msg: PeerNetworkServiceMessage) {
        match msg {
            PeerNetworkServiceMessage::AnnounceYourselfToPeer(peer) => {
                self.handle_announce_peer(peer).await;
            }
            PeerNetworkServiceMessage::Handshake(handshake) => {
                self.handle_handshake_request(handshake).await;
            }
            PeerNetworkServiceMessage::AnnouncementFinished(result) => {
                self.handle_announcement_finished(result).await;
            }
            PeerNetworkServiceMessage::RequestDataFromNetwork {
                data_request,
                use_trusted_peers_only,
                response,
                retries,
            } => {
                self.handle_request_data_from_network(
                    data_request,
                    use_trusted_peers_only,
                    response,
                    retries,
                )
                .await;
            }
        }
    }

    async fn run_inactive_peers_health_check(&self) {
        let inactive_peers = self.inner.peer_list().inactive_peers();
        if inactive_peers.is_empty() {
            return;
        }

        let gossip_client = {
            let state = self.inner.state.lock().await;
            state.gossip_client.clone()
        };
        let sender_inner = self.inner.clone();

        for (peer_id, peer) in inactive_peers {
            let client = gossip_client.clone();
            let peer_list = self.inner.peer_list();
            let inner_clone = sender_inner.clone();
            tokio::spawn(
                async move {
                    match client
                        .check_health(&peer_id, peer.address, peer.protocol_version, &peer_list)
                        .await
                    {
                        Ok(true) => {
                            debug!("Peer {:?} is online", peer_id);
                            inner_clone.increase_peer_score(&peer_id, ScoreIncreaseReason::Online);
                        }
                        Ok(false) => {
                            debug!("Peer {:?} is offline", peer_id);
                            inner_clone.decrease_peer_score(
                                &peer_id,
                                ScoreDecreaseReason::Offline(
                                    "Health check returned false".to_string(),
                                ),
                            );
                        }
                        Err(GossipClientError::HealthCheck(url, status)) => {
                            let message = format!(
                                "Peer {:?} ({}) health check failed with status {}",
                                peer_id, url, status
                            );
                            debug!("{message}");
                            inner_clone.decrease_peer_score(
                                &peer_id,
                                ScoreDecreaseReason::NetworkError(message),
                            );
                        }
                        Err(err) => {
                            error!("Failed to check health of peer {:?}: {:?}", peer_id, err);
                        }
                    }
                }
                .instrument(tracing::info_span!("peer_health_check", %peer_id)),
            );
        }
    }

    async fn handle_announce_peer(&self, peer: PeerListItem) {
        debug!("AnnounceYourselfToPeer message received: {:?}", peer);
        let peer_api_addr = peer.address.api;
        let peer_gossip_addr = peer.address.gossip;
        let reth_peer_info = peer.address.execution;

        let is_trusted_peer = self.inner.peer_list().is_trusted_peer(&peer_api_addr);
        let (
            gossip_client,
            peer_filter_mode,
            peer_list,
            sender,
            reth_peer_sender,
            peers_limit,
            inner,
        ) = {
            let state = self.inner.state.lock().await;
            (
                state.gossip_client.clone(),
                state.config.node_config.peer_filter_mode,
                self.inner.peer_list(),
                self.inner.sender(),
                state.reth_peer_sender.clone(),
                state.peers_limit,
                self.inner.clone(),
            )
        };

        tokio::spawn(Self::announce_yourself_to_address_task(
            gossip_client,
            peer_api_addr,
            peer_gossip_addr,
            inner,
            sender,
            is_trusted_peer,
            peer_filter_mode,
            peer_list,
            peers_limit,
        ));

        tokio::spawn(async move {
            (reth_peer_sender)(reth_peer_info).await;
        });
    }

    #[instrument(skip_all, fields(peer.api_address = ?handshake.api_address))]
    async fn handle_handshake_request(&self, handshake: HandshakeMessage) {
        let task = {
            let mut state = self.inner.state.lock().await;
            let api_address = handshake.api_address;
            let force_announce = handshake.force;

            if api_address == state.peer_address.api {
                debug!("Ignoring self address");
                return;
            }

            if !self.inner.peer_list().is_peer_allowed(&api_address) {
                debug!(
                    "Peer {:?} is not in whitelist, ignoring based on filter mode: {:?}",
                    api_address, state.config.node_config.peer_filter_mode
                );
                return;
            }

            if state.successful_announcements.contains_key(&api_address) && !force_announce {
                debug!("Already announced to peer {:?}", api_address);
                return;
            }

            let already_in_cache = self.inner.peer_list().contains_api_address(&api_address);
            let already_announcing = state.currently_running_announcements.contains(&api_address);

            debug!(
                peer.api_address = ?api_address,
                "Handshake request {:?}: already_in_cache={:?}, already_announcing={:?}, force={}",
                api_address, already_in_cache, already_announcing, force_announce
            );
            let needs_announce = force_announce || !already_announcing;

            debug!(
                "Handshake request {:?}: needs_announce={:?}",
                api_address, needs_announce
            );
            if !needs_announce {
                return;
            }

            if let Some(until) = state.blocklist_until.get(&api_address).copied() {
                if std::time::Instant::now() < until {
                    debug!(
                        "Peer {:?} is blacklisted until {:?}, skipping announce",
                        api_address, until
                    );
                    return;
                }
            }

            debug!("Need to announce yourself to peer {:?}", api_address);
            state.currently_running_announcements.insert(api_address);

            let gossip_address = handshake.gossip_address;

            HandshakeTask {
                api_address,
                gossip_address,
                inner: self.inner.clone(),
                is_trusted_peer: self.inner.peer_list().is_trusted_peer(&api_address),
                peer_filter_mode: state.config.node_config.peer_filter_mode,
                peer_list: self.inner.peer_list(),
                gossip_client: state.gossip_client.clone(),
                sender: self.inner.sender(),
                semaphore: state.handshake_semaphore.clone(),
                peers_limit: state.peers_limit,
            }
        };

        self.spawn_handshake_task(task);
    }

    fn spawn_handshake_task(&self, task: HandshakeTask) {
        let semaphore = task.semaphore.clone();
        tokio::spawn(async move {
            let _permit = semaphore.acquire().await.expect("semaphore closed");
            Self::announce_yourself_to_address_task(
                task.gossip_client,
                task.api_address,
                task.gossip_address,
                task.inner,
                task.sender,
                task.is_trusted_peer,
                task.peer_filter_mode,
                task.peer_list,
                task.peers_limit,
            )
            .await;
        });
    }
    async fn handle_announcement_finished(&self, msg: AnnouncementFinishedMessage) {
        let (retry_backoff, api_address) = {
            let mut state = self.inner.state.lock().await;

            if !msg.success && msg.retry {
                state
                    .currently_running_announcements
                    .remove(&msg.peer_api_address);

                let attempts = {
                    let entry = state
                        .handshake_failures
                        .entry(msg.peer_api_address)
                        .or_insert(0);
                    *entry += 1;
                    *entry
                };

                if attempts >= state.config.node_config.p2p_handshake.max_retries {
                    let until = std::time::Instant::now()
                        + std::time::Duration::from_secs(
                            state.config.node_config.p2p_handshake.blocklist_ttl_secs,
                        );
                    state.blocklist_until.insert(msg.peer_api_address, until);
                    state.handshake_failures.remove(&msg.peer_api_address);
                    debug!(
                        "Peer {:?} blacklisted until {:?} after {} failures",
                        msg.peer_api_address, until, attempts
                    );
                    (None, msg.peer_api_address)
                } else {
                    let backoff_secs = (1_u64 << (attempts - 1))
                        .saturating_mul(state.config.node_config.p2p_handshake.backoff_base_secs)
                        .min(state.config.node_config.p2p_handshake.backoff_cap_secs);
                    let backoff = std::time::Duration::from_secs(backoff_secs);
                    debug!(
                        "Waiting for {:?} to try to announce yourself again (attempt {})",
                        backoff, attempts
                    );
                    (Some(backoff), msg.peer_api_address)
                }
            } else if !msg.success && !msg.retry {
                state.failed_announcements.insert(msg.peer_api_address, msg);
                state
                    .currently_running_announcements
                    .remove(&msg.peer_api_address);
                (None, msg.peer_api_address)
            } else {
                state
                    .successful_announcements
                    .insert(msg.peer_api_address, msg);
                state
                    .currently_running_announcements
                    .remove(&msg.peer_api_address);
                state.handshake_failures.remove(&msg.peer_api_address);
                state.blocklist_until.remove(&msg.peer_api_address);
                (None, msg.peer_api_address)
            }
        };

        if let Some(delay) = retry_backoff {
            let sender = self.inner.sender();
            tokio::spawn(async move {
                sleep(delay).await;
                send_message_and_log_error(
                    &sender,
                    PeerNetworkServiceMessage::Handshake(HandshakeMessage {
                        api_address,
                        gossip_address: msg.peer_gossip_address,
                        force: false,
                    }),
                );
            });
        }
    }
    async fn handle_request_data_from_network(
        &self,
        data_request: GossipDataRequestV2,
        use_trusted_peers_only: bool,
        response: tokio::sync::oneshot::Sender<Result<(), PeerNetworkError>>,
        retries: u8,
    ) {
        let (gossip_client, peer_list, sender, top_active_window, sample_size) = {
            let state = self.inner.state.lock().await;
            (
                state.gossip_client.clone(),
                self.inner.peer_list(),
                self.inner.sender(),
                state.config.node_config.p2p_pull.top_active_window,
                state.config.node_config.p2p_pull.sample_size,
            )
        };

        tokio::spawn(async move {
            let result = Self::request_data_from_network_task(
                gossip_client,
                peer_list,
                sender,
                data_request,
                use_trusted_peers_only,
                retries,
                top_active_window,
                sample_size,
            )
            .await;

            let send_result = match result {
                Ok(()) => response.send(Ok(())),
                Err(err) => response.send(Err(PeerNetworkError::OtherInternalError(format!(
                    "{:?}",
                    err
                )))),
            };

            if let Err(send_err) = send_result {
                error!(
                    "Failed to send response for network data request: {:?}",
                    send_err
                );
            }
        });
    }
    async fn request_data_from_network_task(
        gossip_client: GossipClient,
        peer_list: PeerList,
        sender: PeerNetworkSender,
        data_request: GossipDataRequestV2,
        use_trusted_peers_only: bool,
        retries: u8,
        top_active_window: usize,
        sample_size: usize,
    ) -> Result<(), PeerListServiceError> {
        let mut peers = if use_trusted_peers_only {
            peer_list.online_trusted_peers()
        } else {
            peer_list.top_active_peers(Some(top_active_window), None)
        };

        peers.shuffle(&mut rand::thread_rng());
        peers.truncate(sample_size);

        if peers.is_empty() {
            return Err(PeerListServiceError::NoPeersAvailable);
        }

        let mut last_error = None;
        let mut retryable_peers = peers.clone();

        for attempt in 1..=retries {
            if retryable_peers.is_empty() {
                break;
            }

            let current_round = retryable_peers.clone();
            let mut futs = FuturesUnordered::new();

            for peer in current_round {
                let gc = gossip_client.clone();
                let dr = data_request.clone();
                let pl = peer_list.clone();
                futs.push(async move {
                    let peer_id = peer.0;
                    let res = gc
                        .make_get_data_request_and_update_the_score(&peer, dr, &pl)
                        .await;
                    (peer_id, peer, res)
                });
            }

            let mut next_retryable = Vec::new();

            while let Some((peer_id, peer, result)) = futs.next().await {
                match result {
                    Ok(GossipResponse::Accepted(has)) => {
                        if has {
                            info!(
                                "Successfully requested {:?} from peer {}",
                                data_request, peer_id
                            );
                            return Ok(());
                        } else {
                            debug!("Peer {} doesn't have {:?}", peer_id, data_request);
                            next_retryable.push(peer);
                        }
                    }
                    Ok(GossipResponse::Rejected(reason)) => {
                        warn!(
                            "Peer {} rejected data request {:?}: {:?}",
                            peer_id, data_request, reason
                        );
                        match reason {
                            RejectionReason::HandshakeRequired(reason) => {
                                warn!(
                                    "Peer {} requires a handshake before requesting data: {:?}",
                                    peer_id, reason
                                );
                                last_error = Some(GossipError::PeerNetwork(
                                    PeerNetworkError::FailedToRequestData(
                                        "Peer requires a handshake".to_string(),
                                    ),
                                ));
                                send_message_and_log_error(
                                    &sender,
                                    PeerNetworkServiceMessage::Handshake(HandshakeMessage {
                                        api_address: peer.1.address.api,
                                        gossip_address: peer.1.address.gossip,
                                        force: true,
                                    }),
                                );
                            }
                            RejectionReason::GossipDisabled => {
                                last_error = Some(GossipError::PeerNetwork(
                                    PeerNetworkError::FailedToRequestData(format!(
                                        "Peer {:?} has gossip disabled",
                                        peer_id
                                    )),
                                ));
                            }
                            RejectionReason::InvalidData => {
                                last_error = Some(GossipError::PeerNetwork(
                                    PeerNetworkError::FailedToRequestData(format!(
                                        "Peer {:?} reported invalid data for request {:?}",
                                        peer_id, data_request
                                    )),
                                ));
                            }
                            RejectionReason::RateLimited => {
                                last_error = Some(GossipError::PeerNetwork(
                                    PeerNetworkError::FailedToRequestData(format!(
                                        "Peer {:?} rate limited the request {:?}",
                                        peer_id, data_request
                                    )),
                                ));
                            }
                            RejectionReason::UnableToVerifyOrigin => {
                                last_error = Some(GossipError::PeerNetwork(
                                    PeerNetworkError::FailedToRequestData(format!(
                                        "Peer {:?} unable to verify our origin of request {:?}",
                                        peer_id, data_request
                                    )),
                                ));
                            }
                            RejectionReason::InvalidCredentials
                            | RejectionReason::ProtocolMismatch => {
                                last_error = Some(GossipError::PeerNetwork(
                                    PeerNetworkError::FailedToRequestData(format!(
                                        "Peer {:?} rejected data request {:?} with {:?}",
                                        peer_id, data_request, reason
                                    )),
                                ));
                            }
                            RejectionReason::UnsupportedProtocolVersion(unsupported_version) => {
                                last_error = Some(GossipError::PeerNetwork(
                                    PeerNetworkError::FailedToRequestData(format!(
                                        "Peer {:?} has unsupported protocol version {}",
                                        peer_id, unsupported_version
                                    )),
                                ));
                            }
                            RejectionReason::UnsupportedFeature => {
                                last_error = Some(GossipError::PeerNetwork(
                                    PeerNetworkError::FailedToRequestData(format!(
                                        "Peer {:?} does not support requested feature for {:?}",
                                        peer_id, data_request
                                    )),
                                ));
                            }
                        }
                    }
                    Err(err) => {
                        last_error = Some(err);
                        warn!(
                            "Failed to fetch {:?} from peer {:?} (attempt {}/{}): {:?}",
                            data_request,
                            peer_id,
                            attempt,
                            retries,
                            last_error.as_ref().unwrap()
                        );
                        next_retryable.push(peer);
                    }
                }
            }

            retryable_peers = next_retryable;

            if attempt != retries {
                sleep(Duration::from_millis(50)).await;
            }
        }

        Err(PeerListServiceError::FailedToRequestData(format!(
            "Failed to fetch {:?} after trying {} peers: {:?}",
            data_request, sample_size, last_error
        )))
    }
    fn trusted_peers_handshake_task(
        sender: PeerNetworkSender,
        trusted_peers_api_to_gossip_addresses: HashMap<SocketAddr, SocketAddr>,
    ) {
        for (api_address, gossip_address) in trusted_peers_api_to_gossip_addresses {
            send_message_and_log_error(
                &sender,
                PeerNetworkServiceMessage::Handshake(HandshakeMessage {
                    api_address,
                    gossip_address,
                    force: true,
                }),
            );
        }
    }

    fn spawn_announce_yourself_to_all_peers_task(
        known_peers: HashMap<IrysPeerId, PeerListItem>,
        sender: PeerNetworkSender,
    ) {
        for (_peer_id, peer) in known_peers {
            send_message_and_log_error(
                &sender,
                PeerNetworkServiceMessage::AnnounceYourselfToPeer(peer),
            );
        }
    }
    #[instrument(level = "info", skip_all, fields(%api_address, %gossip_address))]
    async fn announce_yourself_to_address_task(
        gossip_client: GossipClient,
        api_address: SocketAddr,
        gossip_address: SocketAddr,
        inner: Arc<PeerNetworkServiceInner>,
        sender: PeerNetworkSender,
        is_trusted_peer: bool,
        peer_filter_mode: PeerFilterMode,
        peer_list: PeerList,
        peers_limit: usize,
    ) {
        match Self::announce_yourself_to_address(
            gossip_client,
            api_address,
            gossip_address,
            inner,
            sender.clone(),
            is_trusted_peer,
            peer_filter_mode,
            peer_list,
            peers_limit,
        )
        .await
        {
            Ok(()) => {
                debug!("Successfully announced yourself to address {}", api_address);
            }
            Err(err) => {
                warn!(
                    "Failed to announce yourself to address {}: {:?}",
                    api_address, err
                );
            }
        }
    }

    async fn announce_yourself_to_address(
        gossip_client: GossipClient,
        api_address: SocketAddr,
        gossip_address: SocketAddr,
        inner: Arc<PeerNetworkServiceInner>,
        sender: PeerNetworkSender,
        is_trusted_peer: bool,
        peer_filter_mode: PeerFilterMode,
        peer_list: PeerList,
        peers_limit: usize,
    ) -> Result<(), PeerListServiceError> {
        let peer_protocol_versions = gossip_client
            .get_protocol_versions(PeerAddress {
                gossip: gossip_address,
                ..Default::default()
            })
            .await
            .map_err(|e| {
                warn!(
                    "Failed to get protocol versions from gossip address {}: {}",
                    gossip_address, e
                );
                PeerListServiceError::PostVersionError(e.to_string())
            })?;

        let our_supported_versions = irys_types::ProtocolVersion::supported_versions_u32();

        // Find the intersection of supported versions
        let mut common_versions: Vec<u32> = peer_protocol_versions
            .iter()
            .filter(|v| our_supported_versions.contains(v))
            .copied()
            .collect();

        if common_versions.is_empty() {
            warn!(
                "Peer at {} has no compatible protocol versions. Peer supports: {:?}, We support: {:?}",
                gossip_address, peer_protocol_versions, our_supported_versions
            );
            return Err(PeerListServiceError::PostVersionError(format!(
                "Peer {} has no compatible protocol versions",
                gossip_address
            )));
        }

        // Use the highest common version
        common_versions.sort_unstable();
        let negotiated_protocol_version = *common_versions.last().unwrap();

        debug!(
            "Negotiated protocol version {} with peer {} (peer supports: {:?}, we support: {:?})",
            negotiated_protocol_version,
            gossip_address,
            peer_protocol_versions,
            our_supported_versions
        );

        let protocol_version: irys_types::ProtocolVersion = negotiated_protocol_version.into();

        // Create the appropriate handshake based on a negotiated version
        let peer_response_result = match protocol_version {
            ProtocolVersion::V1 => {
                let handshake_request = inner.create_handshake_request_v1().await;
                gossip_client
                    .post_handshake_v1(gossip_address, handshake_request)
                    .await
            }
            ProtocolVersion::V2 | ProtocolVersion::VersionPD => {
                let handshake_request = inner.create_handshake_request_v2(protocol_version).await;
                gossip_client
                    .post_handshake_v2(gossip_address, handshake_request, protocol_version)
                    .await
            }
        }
        .map_err(|e| {
            warn!(
                "Failed to announce yourself to gossip address {}: {}",
                gossip_address, e
            );
            PeerListServiceError::PostVersionError(e.to_string())
        });

        let peer_response = match peer_response_result {
            Ok(peer_response) => {
                send_message_and_log_error(
                    &sender,
                    PeerNetworkServiceMessage::AnnouncementFinished(AnnouncementFinishedMessage {
                        peer_api_address: api_address,
                        peer_gossip_address: gossip_address,
                        success: true,
                        retry: false,
                    }),
                );
                Ok(peer_response)
            }
            Err(error) => {
                debug!(
                    "Retrying to announce yourself to address {}: {:?}",
                    api_address, error
                );
                send_message_and_log_error(
                    &sender,
                    PeerNetworkServiceMessage::AnnouncementFinished(AnnouncementFinishedMessage {
                        peer_api_address: api_address,
                        peer_gossip_address: gossip_address,
                        success: false,
                        retry: true,
                    }),
                );
                Err(error)
            }
        }?;

        match peer_response {
            PeerResponse::Accepted(mut accepted_peers) => {
                // Only log mismatch if the version is not V1 - V1 peers have zero hash
                if protocol_version != ProtocolVersion::V1 {
                    let our_hash = inner.state.lock().await.config.consensus.keccak256_hash();
                    if accepted_peers.consensus_config_hash != our_hash {
                        error!(
                            "Consensus config mismatch with peer at {}! ours={} theirs={}",
                            gossip_address, our_hash, accepted_peers.consensus_config_hash,
                        );
                    }
                }

                if is_trusted_peer && peer_filter_mode == PeerFilterMode::TrustedAndHandshake {
                    let peer_addresses: Vec<SocketAddr> =
                        accepted_peers.peers.iter().map(|p| p.api).collect();
                    debug!(
                        "Adding {} peers from trusted peer handshake to whitelist: {:?}",
                        peer_addresses.len(),
                        peer_addresses
                    );
                    peer_list.add_peers_to_whitelist(peer_addresses);
                }

                accepted_peers.peers.shuffle(&mut rand::thread_rng());
                let limit = peers_limit;
                if accepted_peers.peers.len() > limit {
                    accepted_peers.peers.truncate(limit);
                }
                for peer in accepted_peers.peers {
                    send_message_and_log_error(
                        &sender,
                        PeerNetworkServiceMessage::Handshake(HandshakeMessage {
                            api_address: peer.api,
                            gossip_address: peer.gossip,
                            force: false,
                        }),
                    );
                }
                Ok(())
            }
            PeerResponse::Rejected(rejected_response) => Err(
                PeerListServiceError::PeerHandshakeRejected(rejected_response),
            ),
        }
    }
}

pub fn spawn_peer_network_service(
    db: DatabaseProvider,
    config: &Config,
    reth_peer_sender: RethPeerSender,
    service_receiver: UnboundedReceiver<PeerNetworkServiceMessage>,
    service_sender: PeerNetworkSender,
    peer_events: broadcast::Sender<PeerEvent>,
    runtime_handle: Handle,
) -> (TokioServiceHandle, PeerList) {
    spawn_peer_network_service_with_client(
        db,
        config,
        reth_peer_sender,
        service_receiver,
        service_sender,
        peer_events,
        runtime_handle,
    )
}

pub(crate) fn spawn_peer_network_service_with_client(
    db: DatabaseProvider,
    config: &Config,
    reth_peer_sender: RethPeerSender,
    service_receiver: UnboundedReceiver<PeerNetworkServiceMessage>,
    service_sender: PeerNetworkSender,
    peer_events: broadcast::Sender<PeerEvent>,
    runtime_handle: Handle,
) -> (TokioServiceHandle, PeerList) {
    let peer_list = PeerList::new(config, &db, service_sender.clone(), peer_events)
        .expect("Failed to load peer list data");

    let inner = Arc::new(PeerNetworkServiceInner::new(
        db,
        config.clone(),
        reth_peer_sender,
        peer_list.clone(),
        service_sender,
    ));

    let (shutdown_tx, shutdown_rx) = signal();
    let service = PeerNetworkService::new(shutdown_rx, service_receiver, inner);

    let handle = runtime_handle.spawn(async move {
        if let Err(err) = service.start().await {
            error!("Peer network service terminated: {:?}", err);
        }
    });

    let service_handle = TokioServiceHandle {
        name: "peer_network_service".to_string(),
        handle,
        shutdown_signal: shutdown_tx,
    };

    (service_handle, peer_list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt as _;
    use irys_database::{tables::PeerListItems, walk_all};
    use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::peer_list::PeerScore;
    use irys_types::{
        Config, IrysAddress, IrysPeerId, NodeConfig, PeerNetworkServiceMessage, RethPeerInfo,
    };
    use std::collections::{HashMap, HashSet};
    use std::net::{IpAddr, SocketAddr};
    use std::str::FromStr as _;
    use std::sync::Arc;
    use tokio::sync::Mutex as AsyncMutex;
    use tokio::test;
    use tokio::time::{sleep, timeout, Duration};

    fn create_test_peer(
        mining_addr: &str,
        gossip_port: u16,
        is_online: bool,
        custom_ip: Option<IpAddr>,
    ) -> (IrysAddress, PeerListItem) {
        let mining_addr = IrysAddress::from_str(mining_addr).expect("Invalid mining address");
        let ip = custom_ip.unwrap_or_else(|| IpAddr::from_str("127.0.0.1").expect("invalid ip"));
        let gossip_addr = SocketAddr::new(ip, gossip_port);
        let api_addr = SocketAddr::new(ip, gossip_port + 1);
        let peer_addr = PeerAddress {
            gossip: gossip_addr,
            api: api_addr,
            execution: RethPeerInfo::default(),
        };
        // Generate a different peer_id to ensure we don't rely on peer_id == mining_addr
        let peer_id = IrysPeerId::random();
        let peer = PeerListItem {
            peer_id,
            mining_address: mining_addr,
            address: peer_addr,
            reputation_score: PeerScore::new(50),
            response_time: 100,
            last_seen: 123,
            is_online,
            protocol_version: Default::default(),
        };
        (mining_addr, peer)
    }

    fn open_db(path: &std::path::Path) -> DatabaseProvider {
        DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&path.to_path_buf()).expect("open test database"),
        ))
    }

    struct TestHarness {
        config: Config,
        inner: Arc<PeerNetworkServiceInner>,
        service: PeerNetworkService,
    }

    impl TestHarness {
        fn new(temp_dir: &std::path::Path, config: Config) -> Self {
            let db = open_db(temp_dir);
            let (sender, receiver) = PeerNetworkSender::new_with_receiver();
            let reth_sender = { Arc::new(move |_info: RethPeerInfo| async move {}.boxed()) };
            let peer_list = PeerList::new(
                &config,
                &db,
                sender.clone(),
                broadcast::channel::<PeerEvent>(100).0,
            )
            .expect("peer list");
            let inner = Arc::new(PeerNetworkServiceInner::new(
                db,
                config.clone(),
                reth_sender,
                peer_list,
                sender,
            ));
            let (_shutdown_tx, shutdown_rx) = signal();
            let service = PeerNetworkService::new(shutdown_rx, receiver, inner.clone());
            Self {
                config,
                inner,
                service,
            }
        }

        fn peer_list(&self) -> PeerList {
            self.inner.peer_list()
        }
    }

    #[test]
    async fn test_add_peer() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config: Config = Config::new_with_random_peer_id(NodeConfig::testing());
        let db = open_db(temp_dir.path());
        let (sender, _receiver) = PeerNetworkSender::new_with_receiver();
        let peer_list = PeerList::new(
            &config,
            &db,
            sender,
            tokio::sync::broadcast::channel::<irys_domain::PeerEvent>(100).0,
        )
        .expect("peer list");
        let (_mining_addr, peer) = create_test_peer(
            "0x1234567890123456789012345678901234567890",
            8080,
            true,
            None,
        );
        peer_list.add_or_update_peer(peer.clone(), true);
        assert_eq!(
            peer_list
                .peer_by_gossip_address(peer.address.gossip)
                .expect("peer exists"),
            peer
        );
        assert!(peer_list.all_known_peers().contains(&peer.address));
    }

    #[test]
    async fn test_active_peers_request() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config: Config = Config::new_with_random_peer_id(NodeConfig::testing());
        let db = open_db(temp_dir.path());
        let (sender, _receiver) = PeerNetworkSender::new_with_receiver();
        let peer_list = PeerList::new(
            &config,
            &db,
            sender,
            tokio::sync::broadcast::channel::<irys_domain::PeerEvent>(100).0,
        )
        .expect("peer list");
        let (_mining_addr1, mut peer1) = create_test_peer(
            "0x1111111111111111111111111111111111111111",
            8081,
            true,
            None,
        );
        let (_mining_addr2, mut peer2) = create_test_peer(
            "0x2222222222222222222222222222222222222222",
            8082,
            true,
            None,
        );
        let (_mining_addr3, peer3) = create_test_peer(
            "0x3333333333333333333333333333333333333333",
            8083,
            false,
            None,
        );
        peer1.reputation_score.increase_online();
        peer1.reputation_score.increase_online();
        peer2.reputation_score.increase_online();
        peer_list.add_or_update_peer(peer1.clone(), true);
        peer_list.add_or_update_peer(peer2.clone(), true);
        peer_list.add_or_update_peer(peer3, true);
        let active = peer_list.top_active_peers(Some(2), Some(HashSet::new()));
        assert_eq!(active.len(), 2);
        assert_eq!(active[0].1, peer1);
        assert_eq!(active[1].1, peer2);
    }

    #[test]
    async fn test_initial_handshake_with_trusted_peers() {
        let (sender, mut receiver) = PeerNetworkSender::new_with_receiver();
        let peers: HashMap<SocketAddr, SocketAddr> = vec![
            (
                "127.0.0.1:25001".parse().unwrap(),
                "127.0.0.1:25001".parse().unwrap(),
            ),
            (
                "127.0.0.1:25002".parse().unwrap(),
                "127.0.0.1:25002".parse().unwrap(),
            ),
        ]
        .into_iter()
        .collect();
        PeerNetworkService::trusted_peers_handshake_task(sender, peers);
        let mut messages = Vec::new();
        while let Ok(msg) = receiver.try_recv() {
            messages.push(msg);
        }
        assert_eq!(messages.len(), 2);
        for msg in messages {
            match msg {
                PeerNetworkServiceMessage::Handshake(handshake) => assert!(handshake.force),
                other => panic!("unexpected message: {:?}", other),
            }
        }
    }

    #[test]
    async fn test_announce_yourself_to_all_peers() {
        let (sender, mut receiver) = PeerNetworkSender::new_with_receiver();
        let peer1 = create_test_peer(
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            9001,
            true,
            None,
        )
        .1;
        let peer2 = create_test_peer(
            "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            9002,
            true,
            None,
        )
        .1;
        let known_peers = HashMap::from([
            (
                IrysPeerId::from(IrysAddress::repeat_byte(0xAA)),
                peer1.clone(),
            ),
            (
                IrysPeerId::from(IrysAddress::repeat_byte(0xBB)),
                peer2.clone(),
            ),
        ]);
        PeerNetworkService::spawn_announce_yourself_to_all_peers_task(known_peers, sender);
        let mut api_addrs = Vec::new();
        while let Ok(message) = receiver.try_recv() {
            match message {
                PeerNetworkServiceMessage::AnnounceYourselfToPeer(peer) => {
                    api_addrs.push(peer.address.api);
                }
                other => panic!("unexpected message: {:?}", other),
            }
        }
        api_addrs.sort();
        let mut expected = vec![peer1.address.api, peer2.address.api];
        expected.sort();
        assert_eq!(api_addrs, expected);
    }

    #[test]
    async fn test_handshake_blacklist_after_max_retries() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config: Config = Config::new_with_random_peer_id(NodeConfig::testing());
        let harness = TestHarness::new(temp_dir.path(), config);
        let target: SocketAddr = "127.0.0.1:18080".parse().unwrap();
        let max_retries = harness.config.node_config.p2p_handshake.max_retries;
        for _ in 0..max_retries {
            harness
                .service
                .handle_announcement_finished(AnnouncementFinishedMessage {
                    peer_api_address: target,
                    peer_gossip_address: target,
                    success: false,
                    retry: true,
                })
                .await;
        }
        let blocklisted = harness
            .inner
            .state
            .lock()
            .await
            .blocklist_until
            .contains_key(&target);
        assert!(blocklisted);
    }

    // #[test]
    // async fn should_prevent_infinite_handshake_loop() {
    //     let temp_dir = setup_tracing_and_temp_dir(None, false);
    //     let mut node_config = NodeConfig::testing();
    //     node_config.trusted_peers = vec![];
    //     let config = Config::new_with_random_peer_id(node_config);
    //     let harness = TestHarness::new(temp_dir.path(), config);
    //     let peer = create_test_peer(
    //         "0x1234567890123456789012345678901234567890",
    //         8080,
    //         true,
    //         None,
    //     )
    //     .1;
    //     harness
    //         .peer_list()
    //         .add_or_update_peer(peer.clone(), true);
    //     harness
    //         .api_client
    //         .push_response(Ok(PeerResponse::Accepted(AcceptedResponse::default())))
    //         .await;
    //     harness
    //         .service
    //         .handle_handshake_request(HandshakeMessage {
    //             api_address: peer.address.api,
    //             force: false,
    //         })
    //         .await;
    //     sleep(Duration::from_millis(50)).await;
    //
    //     debug!("Handshake test: Checking API calls");
    //     let api_calls = harness.api_client.calls().await;
    //     assert_eq!(api_calls.len(), 1, "Expected one API call");
    //     harness
    //         .service
    //         .handle_announcement_finished(AnnouncementFinishedMessage {
    //             peer_api_address: peer.address.api,
    //             success: true,
    //             retry: false,
    //         })
    //         .await;
    //     harness
    //         .api_client
    //         .push_response(Ok(PeerResponse::Accepted(AcceptedResponse::default())))
    //         .await;
    //     harness
    //         .service
    //         .handle_handshake_request(HandshakeMessage {
    //             api_address: peer.address.api,
    //             force: true,
    //         })
    //         .await;
    //     sleep(Duration::from_millis(50)).await;
    //
    //     debug!("Handshake test: Checking API calls after a forced handshake");
    //     assert_eq!(harness.api_client.calls().await.len(), 2);
    // }
    //
    // #[test]
    // async fn test_reth_sender_receives_reth_peer_info() {
    //     let temp_dir = setup_tracing_and_temp_dir(None, false);
    //     let config: Config = Config::new_with_random_peer_id(NodeConfig::testing());
    //     let harness = TestHarness::new(temp_dir.path(), config);
    //     harness
    //         .api_client
    //         .push_response(Ok(PeerResponse::Accepted(AcceptedResponse::default())))
    //         .await;
    //     let (_, peer) = create_test_peer(
    //         "0x1234567890123456789012345678901234567890",
    //         8080,
    //         true,
    //         None,
    //     );
    //     harness
    //         .service
    //         .handle_message(PeerNetworkServiceMessage::AnnounceYourselfToPeer(peer))
    //         .await;
    //     sleep(Duration::from_millis(50)).await;
    //     let calls = harness.reth_calls.lock().await;
    //     assert!(!calls.is_empty());
    // }

    #[test]
    async fn test_periodic_flush() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config: Config = Config::new_with_random_peer_id(NodeConfig::testing());
        let db = open_db(temp_dir.path());
        let (sender, receiver) = PeerNetworkSender::new_with_receiver();
        let reth_calls = Arc::new(AsyncMutex::new(Vec::new()));
        let reth_sender = {
            let reth_calls = reth_calls.clone();
            Arc::new(move |info: RethPeerInfo| {
                let reth_calls = reth_calls.clone();
                async move {
                    reth_calls.lock().await.push(info);
                }
                .boxed()
            })
        };
        let runtime_handle = tokio::runtime::Handle::current();
        let (service_handle, peer_list) = spawn_peer_network_service(
            db.clone(),
            &config,
            reth_sender,
            receiver,
            sender,
            tokio::sync::broadcast::channel::<PeerEvent>(100).0,
            runtime_handle,
        );
        let (_addr, peer) = create_test_peer(
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            9100,
            true,
            None,
        );
        peer_list.add_or_update_peer(peer.clone(), true);
        sleep(FLUSH_INTERVAL + Duration::from_millis(100)).await;
        let TokioServiceHandle {
            shutdown_signal,
            handle,
            ..
        } = service_handle;
        shutdown_signal.fire();
        let _ = handle.await;
        let read_tx = db.tx().expect("tx");
        let items = walk_all::<PeerListItems, _>(&read_tx).expect("walk");
        assert_eq!(items.len(), 1);
        // Convert from the database format back to PeerListItem
        let peer_id = items[0].0;
        let inner: irys_types::PeerListItemInner = items[0].1.clone().into();
        let peer_item = PeerListItem::from_inner(inner, peer_id);
        assert_eq!(peer_item.address.api, peer.address.api);
    }

    #[test]
    async fn test_load_from_database() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config: Config = Config::new_with_random_peer_id(NodeConfig::testing());
        let db = open_db(temp_dir.path());
        let (sender, receiver) = PeerNetworkSender::new_with_receiver();
        let runtime_handle = tokio::runtime::Handle::current();
        let reth_sender = { Arc::new(move |_info: RethPeerInfo| async {}.boxed()) };
        let (service_handle, peer_list) = spawn_peer_network_service(
            db.clone(),
            &config,
            reth_sender.clone(),
            receiver,
            sender,
            tokio::sync::broadcast::channel::<PeerEvent>(100).0,
            runtime_handle.clone(),
        );
        let (_addr1, peer1) = create_test_peer(
            "0x1111111111111111111111111111111111111111",
            9200,
            true,
            None,
        );
        let (_addr2, peer2) = create_test_peer(
            "0x2222222222222222222222222222222222222222",
            9202,
            true,
            None,
        );
        peer_list.add_or_update_peer(peer1.clone(), true);
        peer_list.add_or_update_peer(peer2.clone(), true);
        sleep(FLUSH_INTERVAL + Duration::from_millis(100)).await;
        let TokioServiceHandle {
            shutdown_signal,
            handle,
            ..
        } = service_handle;
        shutdown_signal.fire();
        let _ = handle.await;
        let (sender2, receiver2) = PeerNetworkSender::new_with_receiver();
        let (new_handle, new_peer_list) = spawn_peer_network_service(
            db,
            &config,
            reth_sender,
            receiver2,
            sender2,
            tokio::sync::broadcast::channel::<PeerEvent>(100).0,
            runtime_handle,
        );
        assert!(new_peer_list
            .peer_by_gossip_address(peer1.address.gossip)
            .is_some());
        assert!(new_peer_list
            .peer_by_gossip_address(peer2.address.gossip)
            .is_some());
        let TokioServiceHandle {
            shutdown_signal,
            handle,
            ..
        } = new_handle;
        shutdown_signal.fire();
        let _ = handle.await;
    }

    #[test]
    async fn test_wait_for_active_peer() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config: Config = Config::new_with_random_peer_id(NodeConfig::testing());
        let db = open_db(temp_dir.path());
        let (sender, _receiver) = PeerNetworkSender::new_with_receiver();
        let peer_list = PeerList::new(
            &config,
            &db,
            sender,
            tokio::sync::broadcast::channel::<irys_domain::PeerEvent>(100).0,
        )
        .expect("peer list");
        let wait_list = peer_list.clone();
        let wait_handle = tokio::spawn(async move {
            wait_list.wait_for_active_peers().await;
        });
        sleep(Duration::from_millis(50)).await;
        let (_mining_addr, peer) = create_test_peer(
            "0x4444444444444444444444444444444444444444",
            9300,
            true,
            None,
        );
        peer_list.add_or_update_peer(peer.clone(), true);
        wait_handle.await.expect("wait task");
        let active = peer_list.top_active_peers(None, None);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].1, peer);
    }

    #[test]
    async fn test_wait_for_active_peer_no_peers() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config: Config = Config::new_with_random_peer_id(NodeConfig::testing());
        let db = open_db(temp_dir.path());
        let (sender, _receiver) = PeerNetworkSender::new_with_receiver();
        let peer_list = PeerList::new(
            &config,
            &db,
            sender,
            tokio::sync::broadcast::channel::<irys_domain::PeerEvent>(100).0,
        )
        .expect("peer list");
        let wait_list = peer_list.clone();
        let result = timeout(Duration::from_millis(200), async move {
            wait_list.wait_for_active_peers().await;
        })
        .await;
        assert!(result.is_err(), "wait should time out without peers");
    }

    #[test]
    async fn test_staked_unstaked_peer_flush_behavior() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let mut node_config = NodeConfig::testing();
        node_config.trusted_peers = vec![];
        let config = Config::new_with_random_peer_id(node_config);
        let harness = TestHarness::new(temp_dir.path(), config);
        let (_staked_mining_addr, staked_peer) = create_test_peer(
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            9400,
            true,
            None,
        );
        let (unstaked_mining_addr, unstaked_peer) = create_test_peer(
            "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            9402,
            true,
            None,
        );
        let staked_peer_id = staked_peer.peer_id;
        let unstaked_peer_id = unstaked_peer.peer_id;
        harness
            .peer_list()
            .add_or_update_peer(staked_peer.clone(), true);
        harness
            .peer_list()
            .add_or_update_peer(unstaked_peer.clone(), false);
        harness.inner.flush().await.expect("flush");
        let persistable = harness.peer_list().persistable_peers();
        assert!(persistable.contains_key(&staked_peer_id));
        assert!(!persistable.contains_key(&unstaked_peer_id));
        for _ in 0..31 {
            harness
                .peer_list()
                .increase_peer_score(&unstaked_mining_addr, ScoreIncreaseReason::Online);
        }
        harness.inner.flush().await.expect("second flush");
        let persistable_after = harness.peer_list().persistable_peers();
        assert!(persistable_after.contains_key(&unstaked_peer_id));
    }

    #[test]
    async fn should_be_able_to_handshake_if_removed_from_purgatory() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config: Config = Config::new_with_random_peer_id(NodeConfig::testing());
        let db = open_db(temp_dir.path());
        let (sender, _receiver) = PeerNetworkSender::new_with_receiver();
        let peer_list = PeerList::new(
            &config,
            &db,
            sender,
            tokio::sync::broadcast::channel::<irys_domain::PeerEvent>(100).0,
        )
        .expect("peer list");
        let (mining_addr, peer) = create_test_peer(
            "0x9999999999999999999999999999999999999999",
            9500,
            true,
            None,
        );
        let peer_id = peer.peer_id;
        peer_list.add_or_update_peer(peer.clone(), false);
        assert!(peer_list.temporary_peers().contains(&peer_id));
        peer_list.decrease_peer_score(&mining_addr, ScoreDecreaseReason::BogusData("test".into()));
        assert!(!peer_list.temporary_peers().contains(&peer_id));
        peer_list.add_or_update_peer(peer, false);
        assert!(peer_list.temporary_peers().contains(&peer_id));
    }
}
