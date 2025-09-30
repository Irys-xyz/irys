use crate::types::{GossipResponse, RejectionReason};
use crate::{gossip_client::GossipClientError, GossipClient, GossipError};
use eyre::{Report, Result as EyreResult};
use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt as _};
use irys_api_client::{ApiClient, IrysApiClient};
use irys_database::insert_peer_list_item;
use irys_database::reth_db::{Database as _, DatabaseError};
use irys_domain::{PeerList, ScoreDecreaseReason, ScoreIncreaseReason};
use irys_types::{
    build_user_agent, Address, AnnouncementFinishedMessage, Config, DatabaseProvider,
    GossipDataRequest, HandshakeMessage, PeerAddress, PeerFilterMode, PeerListItem,
    PeerNetworkError, PeerNetworkSender, PeerNetworkServiceMessage, PeerResponse, RejectedResponse,
    RethPeerInfo, TokioServiceHandle, VersionRequest,
};
use moka::sync::Cache;
use rand::prelude::SliceRandom as _;
use reth::tasks::shutdown::{signal, Shutdown};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::Mutex;
use tokio::time::{interval, sleep, MissedTickBehavior};
use tracing::{debug, error, info, warn};
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
struct PeerNetworkService<A>
where
    A: ApiClient,
{
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<PeerNetworkServiceMessage>,
    inner: Arc<PeerNetworkServiceInner<A>>,
}

struct PeerNetworkServiceInner<A>
where
    A: ApiClient,
{
    peer_list: PeerList,
    state: Mutex<PeerNetworkServiceState<A>>,
    sender: PeerNetworkSender,
}

struct PeerNetworkServiceState<A>
where
    A: ApiClient,
{
    db: DatabaseProvider,
    currently_running_announcements: HashSet<SocketAddr>,
    successful_announcements: Cache<SocketAddr, AnnouncementFinishedMessage>,
    failed_announcements: HashMap<SocketAddr, AnnouncementFinishedMessage>,
    gossip_client: GossipClient,
    irys_api_client: A,
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

struct HandshakeTask<A>
where
    A: ApiClient,
{
    api_address: SocketAddr,
    version_request: VersionRequest,
    is_trusted_peer: bool,
    peer_filter_mode: PeerFilterMode,
    peer_list: PeerList,
    api_client: A,
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
    }
}
impl<A> PeerNetworkServiceInner<A>
where
    A: ApiClient,
{
    fn new(
        db: DatabaseProvider,
        config: Config,
        irys_api_client: A,
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
            ),
            irys_api_client,
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

        let persistable_peers = self.peer_list.persistable_peers();
        let _ = db
            .update(|tx| {
                for (addr, peer) in persistable_peers.iter() {
                    insert_peer_list_item(tx, addr, peer).map_err(PeerListServiceError::from)?;
                }
                Ok::<(), PeerListServiceError>(())
            })
            .map_err(PeerListServiceError::Database)?;

        Ok(())
    }

    fn increase_peer_score(&self, mining_addr: &Address, reason: ScoreIncreaseReason) {
        self.peer_list.increase_peer_score(mining_addr, reason);
    }

    fn decrease_peer_score(&self, mining_addr: &Address, reason: ScoreDecreaseReason) {
        self.peer_list.decrease_peer_score(mining_addr, reason);
    }

    async fn create_version_request(&self) -> VersionRequest {
        let state = self.state.lock().await;
        let mut version_request = VersionRequest {
            address: state.peer_address,
            chain_id: state.chain_id,
            user_agent: Some(build_user_agent("Irys-Node", env!("CARGO_PKG_VERSION"))),
            ..VersionRequest::default()
        };
        state
            .config
            .irys_signer()
            .sign_p2p_handshake(&mut version_request)
            .expect("Failed to sign version request");
        version_request
    }

    fn sender(&self) -> PeerNetworkSender {
        self.sender.clone()
    }

    fn peer_list(&self) -> PeerList {
        self.peer_list.clone()
    }
}
impl<A> PeerNetworkService<A>
where
    A: ApiClient,
{
    fn new(
        shutdown: Shutdown,
        msg_rx: UnboundedReceiver<PeerNetworkServiceMessage>,
        inner: Arc<PeerNetworkServiceInner<A>>,
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

        let trusted_peers = peer_list.trusted_peer_addresses();
        Self::trusted_peers_handshake_task(sender.clone(), trusted_peers);

        let initial_peers: HashMap<Address, PeerListItem> = peer_list
            .all_peers()
            .iter()
            .map(|(addr, peer)| (*addr, peer.clone()))
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

        for (mining_addr, peer) in inactive_peers {
            let client = gossip_client.clone();
            let peer_list = self.inner.peer_list();
            let inner_clone = sender_inner.clone();
            tokio::spawn(async move {
                match client.check_health(peer.address, &peer_list).await {
                    Ok(true) => {
                        debug!("Peer {:?} is online", mining_addr);
                        inner_clone.increase_peer_score(&mining_addr, ScoreIncreaseReason::Online);
                    }
                    Ok(false) => {
                        debug!("Peer {:?} is offline", mining_addr);
                        inner_clone.decrease_peer_score(&mining_addr, ScoreDecreaseReason::Offline);
                    }
                    Err(GossipClientError::HealthCheck(url, status)) => {
                        debug!(
                            "Peer {:?}{} healthcheck failed with status {}",
                            mining_addr, url, status
                        );
                        inner_clone.decrease_peer_score(&mining_addr, ScoreDecreaseReason::Offline);
                    }
                    Err(err) => {
                        error!(
                            "Failed to check health of peer {:?}: {:?}",
                            mining_addr, err
                        );
                    }
                }
            });
        }
    }
    async fn handle_announce_peer(&self, peer: PeerListItem) {
        debug!("AnnounceYourselfToPeer message received: {:?}", peer);
        let peer_api_addr = peer.address.api;
        let reth_peer_info = peer.address.execution;

        let version_request = self.inner.create_version_request().await;
        let is_trusted_peer = self.inner.peer_list().is_trusted_peer(&peer_api_addr);
        let (api_client, peer_filter_mode, peer_list, sender, reth_peer_sender, peers_limit) = {
            let state = self.inner.state.lock().await;
            (
                state.irys_api_client.clone(),
                state.config.node_config.peer_filter_mode,
                self.inner.peer_list(),
                self.inner.sender(),
                state.reth_peer_sender.clone(),
                state.peers_limit,
            )
        };

        tokio::spawn(Self::announce_yourself_to_address_task(
            api_client,
            peer_api_addr,
            version_request,
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

            let mut version_request = VersionRequest {
                address: state.peer_address,
                chain_id: state.chain_id,
                user_agent: Some(build_user_agent("Irys-Node", env!("CARGO_PKG_VERSION"))),
                ..VersionRequest::default()
            };
            state
                .config
                .irys_signer()
                .sign_p2p_handshake(&mut version_request)
                .expect("Failed to sign version request");

            let task = HandshakeTask {
                api_address,
                version_request,
                is_trusted_peer: self.inner.peer_list().is_trusted_peer(&api_address),
                peer_filter_mode: state.config.node_config.peer_filter_mode,
                peer_list: self.inner.peer_list(),
                api_client: state.irys_api_client.clone(),
                sender: self.inner.sender(),
                semaphore: state.handshake_semaphore.clone(),
                peers_limit: state.peers_limit,
            };

            Some(task)
        };

        if let Some(task) = task {
            self.spawn_handshake_task(task);
        }
    }

    fn spawn_handshake_task(&self, task: HandshakeTask<A>) {
        let semaphore = task.semaphore.clone();
        tokio::spawn(async move {
            let _permit = semaphore.acquire().await.expect("semaphore closed");
            Self::announce_yourself_to_address_task(
                task.api_client,
                task.api_address,
                task.version_request,
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
                        force: false,
                    }),
                );
            });
        }
    }
    async fn handle_request_data_from_network(
        &self,
        data_request: GossipDataRequest,
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
        data_request: GossipDataRequest,
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
                            return Ok(());
                        } else {
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
                                send_message_and_log_error(
                                    &sender,
                                    PeerNetworkServiceMessage::Handshake(HandshakeMessage {
                                        api_address: peer.1.address.api,
                                        force: true,
                                    }),
                                );
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
        trusted_peers_api_addresses: HashSet<SocketAddr>,
    ) {
        for api_address in trusted_peers_api_addresses {
            send_message_and_log_error(
                &sender,
                PeerNetworkServiceMessage::Handshake(HandshakeMessage {
                    api_address,
                    force: true,
                }),
            );
        }
    }

    fn spawn_announce_yourself_to_all_peers_task(
        known_peers: HashMap<Address, PeerListItem>,
        sender: PeerNetworkSender,
    ) {
        for (_mining_addr, peer) in known_peers {
            send_message_and_log_error(
                &sender,
                PeerNetworkServiceMessage::AnnounceYourselfToPeer(peer),
            );
        }
    }
    async fn announce_yourself_to_address_task(
        api_client: A,
        api_address: SocketAddr,
        version_request: VersionRequest,
        sender: PeerNetworkSender,
        is_trusted_peer: bool,
        peer_filter_mode: PeerFilterMode,
        peer_list: PeerList,
        peers_limit: usize,
    ) {
        match Self::announce_yourself_to_address(
            api_client,
            api_address,
            version_request,
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
        api_client: A,
        api_address: SocketAddr,
        version_request: VersionRequest,
        sender: PeerNetworkSender,
        is_trusted_peer: bool,
        peer_filter_mode: PeerFilterMode,
        peer_list: PeerList,
        peers_limit: usize,
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
                send_message_and_log_error(
                    &sender,
                    PeerNetworkServiceMessage::AnnouncementFinished(AnnouncementFinishedMessage {
                        peer_api_address: api_address,
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
                        success: false,
                        retry: true,
                    }),
                );
                Err(error)
            }
        }?;

        match peer_response {
            PeerResponse::Accepted(mut accepted_peers) => {
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
    runtime_handle: Handle,
) -> (TokioServiceHandle, PeerList) {
    spawn_peer_network_service_with_client(
        db,
        config,
        IrysApiClient::new(),
        reth_peer_sender,
        service_receiver,
        service_sender,
        runtime_handle,
    )
}

pub(crate) fn spawn_peer_network_service_with_client<A>(
    db: DatabaseProvider,
    config: &Config,
    irys_api_client: A,
    reth_peer_sender: RethPeerSender,
    service_receiver: UnboundedReceiver<PeerNetworkServiceMessage>,
    service_sender: PeerNetworkSender,
    runtime_handle: Handle,
) -> (TokioServiceHandle, PeerList)
where
    A: ApiClient,
{
    let peer_list =
        PeerList::new(config, &db, service_sender.clone()).expect("Failed to load peer list data");

    let inner = Arc::new(PeerNetworkServiceInner::new(
        db,
        config.clone(),
        irys_api_client,
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
    use actix_rt::test;
    use async_trait::async_trait;
    use eyre::eyre;
    use futures::FutureExt as _;
    use irys_api_client::test_utils::CountingMockClient;
    use irys_database::{tables::PeerListItems, walk_all};
    use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::peer_list::PeerScore;
    use irys_types::{
        AcceptedResponse, Address, BlockIndexItem, BlockIndexQuery, CombinedBlockHeader,
        CommitmentTransaction, Config, DataTransactionHeader, IrysTransactionResponse, NodeConfig,
        NodeInfo, PeerNetworkServiceMessage, RethPeerInfo, H256,
    };
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::net::{IpAddr, SocketAddr};
    use std::str::FromStr as _;
    use std::sync::Arc;
    use tokio::sync::Mutex as AsyncMutex;
    use tokio::time::{sleep, timeout, Duration};

    fn create_test_peer(
        mining_addr: &str,
        gossip_port: u16,
        is_online: bool,
        custom_ip: Option<IpAddr>,
    ) -> (Address, PeerListItem) {
        let mining_addr = Address::from_str(mining_addr).expect("Invalid mining address");
        let ip = custom_ip.unwrap_or_else(|| IpAddr::from_str("127.0.0.1").expect("invalid ip"));
        let gossip_addr = SocketAddr::new(ip, gossip_port);
        let api_addr = SocketAddr::new(ip, gossip_port + 1);
        let peer_addr = PeerAddress {
            gossip: gossip_addr,
            api: api_addr,
            execution: RethPeerInfo::default(),
        };
        let peer = PeerListItem {
            address: peer_addr,
            reputation_score: PeerScore::new(50),
            response_time: 100,
            last_seen: 123,
            is_online,
        };
        (mining_addr, peer)
    }

    fn open_db(path: &std::path::Path) -> DatabaseProvider {
        DatabaseProvider(Arc::new(
            open_or_create_irys_consensus_data_db(&path.to_path_buf()).expect("open test database"),
        ))
    }

    #[derive(Clone)]
    struct TestApiClient {
        inner: Arc<TestApiClientInner>,
    }

    #[derive(Default)]
    struct TestApiClientInner {
        responses: AsyncMutex<VecDeque<EyreResult<PeerResponse>>>,
        calls: AsyncMutex<Vec<SocketAddr>>,
    }

    impl Default for TestApiClient {
        fn default() -> Self {
            Self {
                inner: Arc::new(TestApiClientInner::default()),
            }
        }
    }

    impl TestApiClient {
        async fn push_response(&self, response: EyreResult<PeerResponse>) {
            self.inner.responses.lock().await.push_back(response);
        }

        async fn calls(&self) -> Vec<SocketAddr> {
            self.inner.calls.lock().await.clone()
        }
    }

    #[async_trait]
    impl ApiClient for TestApiClient {
        async fn get_transaction(
            &self,
            _peer: SocketAddr,
            _tx_id: H256,
        ) -> EyreResult<IrysTransactionResponse> {
            Err(eyre!("not implemented"))
        }

        async fn post_transaction(
            &self,
            _peer: SocketAddr,
            _transaction: DataTransactionHeader,
        ) -> EyreResult<()> {
            Err(eyre!("not implemented"))
        }

        async fn post_commitment_transaction(
            &self,
            _peer: SocketAddr,
            _transaction: CommitmentTransaction,
        ) -> EyreResult<()> {
            Err(eyre!("not implemented"))
        }

        async fn get_transactions(
            &self,
            _peer: SocketAddr,
            _tx_ids: &[H256],
        ) -> EyreResult<Vec<IrysTransactionResponse>> {
            Ok(Vec::new())
        }

        async fn post_version(
            &self,
            peer: SocketAddr,
            _version: VersionRequest,
        ) -> EyreResult<PeerResponse> {
            self.inner.calls.lock().await.push(peer);
            if let Some(response) = self.inner.responses.lock().await.pop_front() {
                response
            } else {
                Ok(PeerResponse::Accepted(AcceptedResponse::default()))
            }
        }

        async fn get_block_by_hash(
            &self,
            _peer: SocketAddr,
            _block_hash: H256,
        ) -> EyreResult<Option<CombinedBlockHeader>> {
            Err(eyre!("not implemented"))
        }

        async fn get_block_by_height(
            &self,
            _peer: SocketAddr,
            _block_height: u64,
        ) -> EyreResult<Option<CombinedBlockHeader>> {
            Err(eyre!("not implemented"))
        }

        async fn get_block_index(
            &self,
            _peer: SocketAddr,
            _block_index_query: BlockIndexQuery,
        ) -> EyreResult<Vec<BlockIndexItem>> {
            Err(eyre!("not implemented"))
        }

        async fn node_info(&self, _peer: SocketAddr) -> EyreResult<NodeInfo> {
            Err(eyre!("not implemented"))
        }
    }

    struct TestHarness {
        config: Config,
        api_client: TestApiClient,
        inner: Arc<PeerNetworkServiceInner<TestApiClient>>,
        service: PeerNetworkService<TestApiClient>,
        reth_calls: Arc<AsyncMutex<Vec<RethPeerInfo>>>,
    }

    impl TestHarness {
        fn new(temp_dir: &std::path::Path, config: Config) -> Self {
            let db = open_db(temp_dir);
            let (sender, receiver) = PeerNetworkSender::new_with_receiver();
            let api_client = TestApiClient::default();
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
            let peer_list = PeerList::new(&config, &db, sender.clone()).expect("peer list");
            let inner = Arc::new(PeerNetworkServiceInner::new(
                db,
                config.clone(),
                api_client.clone(),
                reth_sender,
                peer_list,
                sender,
            ));
            let (_shutdown_tx, shutdown_rx) = signal();
            let service = PeerNetworkService::new(shutdown_rx, receiver, inner.clone());
            Self {
                config,
                api_client,
                inner,
                service,
                reth_calls,
            }
        }

        fn peer_list(&self) -> PeerList {
            self.inner.peer_list()
        }
    }

    #[test]
    async fn test_add_peer() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config: Config = NodeConfig::testing().into();
        let db = open_db(temp_dir.path());
        let (sender, _receiver) = PeerNetworkSender::new_with_receiver();
        let peer_list = PeerList::new(&config, &db, sender).expect("peer list");
        let (mining_addr, peer) = create_test_peer(
            "0x1234567890123456789012345678901234567890",
            8080,
            true,
            None,
        );
        peer_list.add_or_update_peer(mining_addr, peer.clone(), true);
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
        let config: Config = NodeConfig::testing().into();
        let db = open_db(temp_dir.path());
        let (sender, _receiver) = PeerNetworkSender::new_with_receiver();
        let peer_list = PeerList::new(&config, &db, sender).expect("peer list");
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
        peer1.reputation_score.increase();
        peer1.reputation_score.increase();
        peer2.reputation_score.increase();
        peer_list.add_or_update_peer(mining_addr1, peer1.clone(), true);
        peer_list.add_or_update_peer(mining_addr2, peer2.clone(), true);
        peer_list.add_or_update_peer(mining_addr3, peer3, true);
        let active = peer_list.top_active_peers(Some(2), Some(HashSet::new()));
        assert_eq!(active.len(), 2);
        assert_eq!(active[0].1, peer1);
        assert_eq!(active[1].1, peer2);
    }

    #[test]
    async fn test_initial_handshake_with_trusted_peers() {
        let (sender, mut receiver) = PeerNetworkSender::new_with_receiver();
        let peers: HashSet<SocketAddr> = vec![
            "127.0.0.1:25001".parse().unwrap(),
            "127.0.0.1:25002".parse().unwrap(),
        ]
        .into_iter()
        .collect();
        PeerNetworkService::<IrysApiClient>::trusted_peers_handshake_task(sender, peers);
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
            (Address::repeat_byte(0xAA), peer1.clone()),
            (Address::repeat_byte(0xBB), peer2.clone()),
        ]);
        PeerNetworkService::<IrysApiClient>::spawn_announce_yourself_to_all_peers_task(
            known_peers,
            sender,
        );
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
        let config: Config = NodeConfig::testing().into();
        let harness = TestHarness::new(temp_dir.path(), config);
        let target: SocketAddr = "127.0.0.1:18080".parse().unwrap();
        let max_retries = harness.config.node_config.p2p_handshake.max_retries;
        for _ in 0..max_retries {
            harness
                .service
                .handle_announcement_finished(AnnouncementFinishedMessage {
                    peer_api_address: target,
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

    #[test]
    async fn should_prevent_infinite_handshake_loop() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let mut node_config = NodeConfig::testing();
        node_config.trusted_peers = vec![];
        let config = Config::new(node_config);
        let harness = TestHarness::new(temp_dir.path(), config);
        let peer = create_test_peer(
            "0x1234567890123456789012345678901234567890",
            8080,
            true,
            None,
        )
        .1;
        harness
            .peer_list()
            .add_or_update_peer(Address::repeat_byte(0xAA), peer.clone(), true);
        harness
            .api_client
            .push_response(Ok(PeerResponse::Accepted(AcceptedResponse::default())))
            .await;
        harness
            .service
            .handle_handshake_request(HandshakeMessage {
                api_address: peer.address.api,
                force: false,
            })
            .await;
        sleep(Duration::from_millis(50)).await;

        debug!("Handshake test: Checking API calls");
        let api_calls = harness.api_client.calls().await;
        assert_eq!(api_calls.len(), 1, "Expected one API call");
        harness
            .service
            .handle_announcement_finished(AnnouncementFinishedMessage {
                peer_api_address: peer.address.api,
                success: true,
                retry: false,
            })
            .await;
        harness
            .api_client
            .push_response(Ok(PeerResponse::Accepted(AcceptedResponse::default())))
            .await;
        harness
            .service
            .handle_handshake_request(HandshakeMessage {
                api_address: peer.address.api,
                force: true,
            })
            .await;
        sleep(Duration::from_millis(50)).await;

        debug!("Handshake test: Checking API calls after a forced handshake");
        assert_eq!(harness.api_client.calls().await.len(), 2);
    }

    #[test]
    async fn test_reth_sender_receives_reth_peer_info() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config: Config = NodeConfig::testing().into();
        let harness = TestHarness::new(temp_dir.path(), config);
        harness
            .api_client
            .push_response(Ok(PeerResponse::Accepted(AcceptedResponse::default())))
            .await;
        let (_, peer) = create_test_peer(
            "0x1234567890123456789012345678901234567890",
            8080,
            true,
            None,
        );
        harness
            .service
            .handle_message(PeerNetworkServiceMessage::AnnounceYourselfToPeer(peer))
            .await;
        sleep(Duration::from_millis(50)).await;
        let calls = harness.reth_calls.lock().await;
        assert!(!calls.is_empty());
    }

    #[test]
    async fn test_periodic_flush() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config: Config = NodeConfig::testing().into();
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
        let (service_handle, peer_list) = spawn_peer_network_service_with_client(
            db.clone(),
            &config,
            CountingMockClient::default(),
            reth_sender,
            receiver,
            sender,
            runtime_handle,
        );
        let (addr, peer) = create_test_peer(
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            9100,
            true,
            None,
        );
        peer_list.add_or_update_peer(addr, peer.clone(), true);
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
        assert_eq!(items[0].1 .0.address.api, peer.address.api);
    }

    #[test]
    async fn test_load_from_database() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config: Config = NodeConfig::testing().into();
        let db = open_db(temp_dir.path());
        let (sender, receiver) = PeerNetworkSender::new_with_receiver();
        let runtime_handle = tokio::runtime::Handle::current();
        let reth_sender = { Arc::new(move |_info: RethPeerInfo| async {}.boxed()) };
        let (service_handle, peer_list) = spawn_peer_network_service_with_client(
            db.clone(),
            &config,
            CountingMockClient::default(),
            reth_sender.clone(),
            receiver,
            sender,
            runtime_handle.clone(),
        );
        let (addr1, peer1) = create_test_peer(
            "0x1111111111111111111111111111111111111111",
            9200,
            true,
            None,
        );
        let (addr2, peer2) = create_test_peer(
            "0x2222222222222222222222222222222222222222",
            9202,
            true,
            None,
        );
        peer_list.add_or_update_peer(addr1, peer1.clone(), true);
        peer_list.add_or_update_peer(addr2, peer2.clone(), true);
        sleep(FLUSH_INTERVAL + Duration::from_millis(100)).await;
        let TokioServiceHandle {
            shutdown_signal,
            handle,
            ..
        } = service_handle;
        shutdown_signal.fire();
        let _ = handle.await;
        let (sender2, receiver2) = PeerNetworkSender::new_with_receiver();
        let (new_handle, new_peer_list) = spawn_peer_network_service_with_client(
            db,
            &config,
            CountingMockClient::default(),
            reth_sender,
            receiver2,
            sender2,
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
        let config: Config = NodeConfig::testing().into();
        let db = open_db(temp_dir.path());
        let (sender, _receiver) = PeerNetworkSender::new_with_receiver();
        let peer_list = PeerList::new(&config, &db, sender).expect("peer list");
        let wait_list = peer_list.clone();
        let wait_handle = tokio::spawn(async move {
            wait_list.wait_for_active_peers().await;
        });
        sleep(Duration::from_millis(50)).await;
        let (mining_addr, peer) = create_test_peer(
            "0x4444444444444444444444444444444444444444",
            9300,
            true,
            None,
        );
        peer_list.add_or_update_peer(mining_addr, peer.clone(), true);
        wait_handle.await.expect("wait task");
        let active = peer_list.top_active_peers(None, None);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].1, peer);
    }

    #[test]
    async fn test_wait_for_active_peer_no_peers() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config: Config = NodeConfig::testing().into();
        let db = open_db(temp_dir.path());
        let (sender, _receiver) = PeerNetworkSender::new_with_receiver();
        let peer_list = PeerList::new(&config, &db, sender).expect("peer list");
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
        let config = Config::new(node_config);
        let harness = TestHarness::new(temp_dir.path(), config);
        let (staked_addr, staked_peer) = create_test_peer(
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            9400,
            true,
            None,
        );
        let (unstaked_addr, unstaked_peer) = create_test_peer(
            "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            9402,
            true,
            None,
        );
        harness
            .peer_list()
            .add_or_update_peer(staked_addr, staked_peer.clone(), true);
        harness
            .peer_list()
            .add_or_update_peer(unstaked_addr, unstaked_peer.clone(), false);
        harness.inner.flush().await.expect("flush");
        let persistable = harness.peer_list().persistable_peers();
        assert!(persistable.contains_key(&staked_addr));
        assert!(!persistable.contains_key(&unstaked_addr));
        for _ in 0..31 {
            harness
                .peer_list()
                .increase_peer_score(&unstaked_addr, ScoreIncreaseReason::Online);
        }
        harness.inner.flush().await.expect("second flush");
        let persistable_after = harness.peer_list().persistable_peers();
        assert!(persistable_after.contains_key(&unstaked_addr));
    }

    #[test]
    async fn should_be_able_to_handshake_if_removed_from_purgatory() {
        let temp_dir = setup_tracing_and_temp_dir(None, false);
        let config: Config = NodeConfig::testing().into();
        let db = open_db(temp_dir.path());
        let (sender, _receiver) = PeerNetworkSender::new_with_receiver();
        let peer_list = PeerList::new(&config, &db, sender).expect("peer list");
        let (mining_addr, peer) = create_test_peer(
            "0x9999999999999999999999999999999999999999",
            9500,
            true,
            None,
        );
        peer_list.add_or_update_peer(mining_addr, peer.clone(), false);
        assert!(peer_list.temporary_peers().contains(&mining_addr));
        peer_list.decrease_peer_score(&mining_addr, ScoreDecreaseReason::BogusData);
        assert!(!peer_list.temporary_peers().contains(&mining_addr));
        peer_list.add_or_update_peer(mining_addr, peer, false);
        assert!(peer_list.temporary_peers().contains(&mining_addr));
    }
}
