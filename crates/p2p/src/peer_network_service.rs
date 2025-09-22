use crate::types::{GossipResponse, RejectionReason};
use crate::{gossip_client::GossipClientError, GossipClient, GossipError};
use eyre::{Report, Result as EyreResult};
use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
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

static HANDSHAKE_SEMAPHORE: std::sync::OnceLock<std::sync::Arc<tokio::sync::Semaphore>> =
    std::sync::OnceLock::new();

fn handshake_semaphore_with_max(max: usize) -> std::sync::Arc<tokio::sync::Semaphore> {
    HANDSHAKE_SEMAPHORE
        .get_or_init(|| std::sync::Arc::new(tokio::sync::Semaphore::new(max)))
        .clone()
}

static HANDSHAKE_FAILURES: std::sync::OnceLock<std::sync::Mutex<HashMap<SocketAddr, u32>>> =
    std::sync::OnceLock::new();

fn handshake_failures() -> &'static std::sync::Mutex<HashMap<SocketAddr, u32>> {
    HANDSHAKE_FAILURES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

static BLOCKLIST_UNTIL: std::sync::OnceLock<
    std::sync::Mutex<HashMap<SocketAddr, std::time::Instant>>,
> = std::sync::OnceLock::new();

fn blocklist_until() -> &'static std::sync::Mutex<HashMap<SocketAddr, std::time::Instant>> {
    BLOCKLIST_UNTIL.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

static PEERS_LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

fn peers_limit() -> usize {
    *PEERS_LIMIT
        .get_or_init(|| irys_types::config::P2PHandshakeConfig::default().max_peers_per_response)
}
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
    max_concurrent_handshakes: usize,
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
        PEERS_LIMIT.get_or_init(|| config.node_config.p2p_handshake.max_peers_per_response);

        let peer_address = build_peer_address(&config);
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
        tokio::spawn(Self::trusted_peers_handshake_task(
            sender.clone(),
            trusted_peers,
        ));

        let initial_peers: HashMap<Address, PeerListItem> = peer_list
            .all_peers()
            .iter()
            .map(|(addr, peer)| (*addr, peer.clone()))
            .collect();
        tokio::spawn(Self::announce_yourself_to_all_peers(
            initial_peers,
            sender.clone(),
        ));

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
        let (api_client, peer_filter_mode, peer_list, sender, reth_peer_sender) = {
            let state = self.inner.state.lock().await;
            (
                state.irys_api_client.clone(),
                state.config.node_config.peer_filter_mode,
                self.inner.peer_list(),
                self.inner.sender(),
                state.reth_peer_sender.clone(),
            )
        };

        tokio::spawn(Self::announce_yourself_to_address_task(
            api_client,
            peer_api_addr,
            version_request,
            sender.clone(),
            is_trusted_peer,
            peer_filter_mode,
            peer_list,
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

            debug!("Already announcing: {:?}", already_announcing);
            debug!("Already in cache: {:?}", already_in_cache);
            let needs_announce = force_announce || !(already_announcing || already_in_cache);

            if !needs_announce {
                return;
            }

            if let Some(until) = blocklist_until()
                .lock()
                .expect("blocklist_until mutex poisoned")
                .get(&api_address)
                .copied()
            {
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
                max_concurrent_handshakes: state
                    .config
                    .node_config
                    .p2p_handshake
                    .max_concurrent_handshakes,
            };

            Some(task)
        };

        if let Some(task) = task {
            self.spawn_handshake_task(task).await;
        }
    }

    async fn spawn_handshake_task(&self, task: HandshakeTask<A>) {
        let semaphore = handshake_semaphore_with_max(task.max_concurrent_handshakes);
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
                    let mut guard = handshake_failures()
                        .lock()
                        .expect("handshake_failures mutex poisoned");
                    let entry = guard.entry(msg.peer_api_address).or_insert(0);
                    *entry += 1;
                    *entry
                };

                if attempts >= state.config.node_config.p2p_handshake.max_retries {
                    let until = std::time::Instant::now()
                        + std::time::Duration::from_secs(
                            state.config.node_config.p2p_handshake.blocklist_ttl_secs,
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
                handshake_failures()
                    .lock()
                    .expect("handshake_failures mutex poisoned")
                    .remove(&msg.peer_api_address);
                blocklist_until()
                    .lock()
                    .expect("blocklist_until mutex poisoned")
                    .remove(&msg.peer_api_address);
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
    async fn trusted_peers_handshake_task(
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

    async fn announce_yourself_to_all_peers(
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
    ) {
        match Self::announce_yourself_to_address(
            api_client,
            api_address,
            version_request,
            sender.clone(),
            is_trusted_peer,
            peer_filter_mode,
            peer_list,
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
                    peer_list.add_peers_to_whitelist(peer_addresses.clone());
                }

                accepted_peers.peers.shuffle(&mut rand::thread_rng());
                let limit = peers_limit();
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
        db.clone(),
        config.clone(),
        irys_api_client,
        reth_peer_sender,
        peer_list.clone(),
        service_sender,
    ));

    let (shutdown_tx, shutdown_rx) = signal();
    let service = PeerNetworkService::new(shutdown_rx, service_receiver, inner.clone());

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
