// This rule is added here because otherwise clippy starts to throw warnings about using %
//  at random macro uses in this file for whatever reason. The second one is because
//  I have absolutely no idea how to name this module to satisfy this lint
#![allow(
    clippy::integer_division_remainder_used,
    clippy::module_name_repetitions,
    reason = "I don't know how to name it"
)]
use crate::block_pool_service::BlockPoolService;
use crate::cache::GossipCacheKey;
use crate::peer_list::PeerListFacade;
use crate::server_data_handler::GossipServerDataHandler;
use crate::types::InternalGossipError;
use crate::{
    cache::GossipCache,
    gossip_client::GossipClient,
    server::GossipServer,
    types::{GossipError, GossipResult},
};
use actix::{Actor, Context, Handler};
use actix_web::dev::{Server, ServerHandle};
use core::time::Duration;
use irys_actors::block_discovery::BlockDiscoveryFacade;
use irys_actors::broadcast_mining_service::BroadcastMiningSeed;
use irys_actors::mempool_service::MempoolFacade;
use irys_api_client::ApiClient;
use irys_types::{
    block_production::Seed, Address, BlockIndexItem, BlockIndexQuery, DatabaseProvider, GossipData,
    H256List, PeerListItem, RethPeerInfo, VDFLimiterInfo,
};
use rand::prelude::SliceRandom as _;
use reth_tasks::{TaskExecutor, TaskManager};
use std::collections::VecDeque;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::{
    sync::mpsc::{channel, error::SendError, Receiver, Sender},
    time,
};
use tracing::{debug, error, info, warn};

const ONE_HOUR: Duration = Duration::from_secs(3600);
const TWO_HOURS: Duration = Duration::from_secs(7200);
const MAX_PEERS_PER_BROADCAST: usize = 5;
const BROADCAST_INTERVAL: Duration = Duration::from_secs(1);
const CACHE_CLEANUP_INTERVAL: Duration = ONE_HOUR;
const CACHE_ENTRY_TTL: Duration = TWO_HOURS;

type TaskExecutionResult = Result<(), tokio::task::JoinError>;

#[derive(Debug)]
pub struct ServiceHandleWithShutdownSignal {
    pub handle: tokio::task::JoinHandle<()>,
    pub shutdown_tx: Sender<()>,
    pub name: String,
}

impl ServiceHandleWithShutdownSignal {
    pub fn spawn<F, S, Fut>(name: S, task: F, task_executor: &TaskExecutor) -> Self
    where
        F: FnOnce(Receiver<()>) -> Fut + Send + 'static,
        S: Into<String>,
        Fut: core::future::Future<Output = ()> + Send + 'static,
    {
        let (shutdown_tx, shutdown_rx) = channel(1);
        let handle = task_executor.spawn(task(shutdown_rx));
        Self {
            handle,
            shutdown_tx,
            name: name.into(),
        }
    }

    /// Stops the task, joins it and returns the result
    ///
    /// # Errors
    ///
    /// If the task panics, an error is returned.
    pub async fn stop(mut self) -> Result<(), tokio::task::JoinError> {
        info!("Called stop on task \"{}\"", self.name);
        match self.shutdown_tx.send(()).await {
            Ok(()) => {
                debug!("Shutdown signal sent to task \"{}\"", self.name);
            }
            Err(SendError(())) => {
                warn!("Shutdown signal was already sent to task \"{}\"", self.name);
            }
        }

        self.wait_for_exit().await?;

        debug!("Task \"{}\" stopped", self.name);

        Ok(())
    }

    /// Waits for the task to exit or immediately returns if the task has already exited. To get
    ///  the execution result, call [`ServiceHandleWithShutdownSignal::stop`].
    ///
    /// # Errors
    ///
    /// If the task panics, an error is returned.
    pub async fn wait_for_exit(&mut self) -> Result<(), tokio::task::JoinError> {
        info!("Waiting for task \"{}\" to exit", self.name);
        let handle = &mut self.handle;
        handle.await
    }
}

#[derive(Debug)]
pub struct GossipService {
    cache: Arc<GossipCache>,
    mempool_data_receiver: Option<Receiver<GossipData>>,
    client: GossipClient,
    pub sync_state: SyncState,
}

#[derive(Clone, Debug)]
pub struct SyncState(Arc<AtomicBool>);

impl SyncState {
    pub fn new(is_syncing: bool) -> Self {
        let sync_state = Arc::new(AtomicBool::new(is_syncing));
        Self(sync_state)
    }

    pub fn store(&self, is_syncing: bool) {
        self.0.store(is_syncing, Ordering::Relaxed);
    }

    /// Returns whether the gossip service is currently syncing
    pub fn is_syncing(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    pub async fn wait_for_sync(&self) {
        // If already synced, return immediately
        if !self.is_syncing() {
            return;
        }

        // Create a future that polls the sync state
        let sync_state = Arc::clone(&self.0);
        tokio::spawn(async move {
            while sync_state.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("Sync checking task failed");
    }
}

impl GossipService {
    /// Returns whether the gossip service is currently syncing
    pub fn is_syncing(&self) -> bool {
        self.sync_state.is_syncing()
    }

    /// Waits until the gossip service has completed syncing
    pub async fn wait_for_sync(&self) {
        self.sync_state.wait_for_sync().await;
    }

    /// Create a new gossip service. To run the service, use the [`GossipService::run`] method.
    /// Also returns a channel to send trusted gossip data to the service. Trusted data should
    /// be sent by the internal components of the system only after complete validation.
    pub fn new(mining_address: Address) -> (Self, Sender<GossipData>) {
        let cache = Arc::new(GossipCache::new());
        let (trusted_data_tx, trusted_data_rx) = channel(1000);

        let client_timeout = Duration::from_secs(5);
        let client = GossipClient::new(client_timeout, mining_address);

        (
            Self {
                client,
                cache,
                mempool_data_receiver: Some(trusted_data_rx),
                sync_state: SyncState::new(true),
            },
            trusted_data_tx,
        )
    }

    /// Spawns all gossip tasks and returns a handle to the service. The service will run until
    /// the stop method is called or the task is dropped.
    ///
    /// # Errors
    ///
    /// If the service fails to start, an error is returned. This can happen if the server fails to
    /// bind to the address or if any of the tasks fails to spawn.
    pub fn run<A, R>(
        mut self,
        mempool: impl MempoolFacade,
        block_discovery: impl BlockDiscoveryFacade,
        api_client: A,
        task_executor: &TaskExecutor,
        peer_list: PeerListFacade<A, R>,
        db: DatabaseProvider,
        vdf_sender: Sender<BroadcastMiningSeed>,
        listener: TcpListener,
        needs_catching_up: bool,
        latest_known_height: usize,
    ) -> GossipResult<ServiceHandleWithShutdownSignal>
    where
        A: ApiClient,
        R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    {
        debug!("Staring gossip service");
        self.sync_state.store(needs_catching_up);

        // TODO: get the db
        let block_pool_service = BlockPoolService::new_with_client(
            db,
            peer_list.clone(),
            block_discovery.clone(),
            Some(vdf_sender),
        );
        let arbiter = actix::Arbiter::new();
        let block_pool_addr =
            BlockPoolService::start_in_arbiter(&arbiter.handle(), |_| block_pool_service);

        let server_data_handler = GossipServerDataHandler {
            mempool,
            block_pool: block_pool_addr,
            api_client: api_client.clone(),
            cache: Arc::clone(&self.cache),
            gossip_client: self.client.clone(),
            peer_list_service: peer_list.clone(),
        };
        let server = GossipServer::new(server_data_handler, peer_list.clone());

        let server = server.run(listener)?;
        let server_handle = server.handle();

        let mempool_data_receiver =
            self.mempool_data_receiver
                .take()
                .ok_or(GossipError::Internal(
                    InternalGossipError::BroadcastReceiverShutdown,
                ))?;

        let cache = Arc::clone(&self.cache);
        let sync_state = self.sync_state.clone();

        let cache_pruning_task_handle = spawn_cache_pruning_task(cache, task_executor);

        let broadcast_task_handle = spawn_broadcast_task(
            mempool_data_receiver,
            self,
            task_executor,
            peer_list.clone(),
        );

        if needs_catching_up {
            task_executor.spawn(async move {
                if let Err(error) = catch_up_task(
                    sync_state,
                    latest_known_height,
                    api_client.clone(),
                    peer_list,
                )
                .await
                {
                    error!("Failed to catch up: {}", error);
                }
            });
        }

        let gossip_service_handle = spawn_watcher_task(
            server,
            server_handle,
            cache_pruning_task_handle,
            broadcast_task_handle,
            task_executor,
            arbiter,
        );

        Ok(gossip_service_handle)
    }

    async fn broadcast_data<A, R>(
        &self,
        data: &GossipData,
        peer_list_service: &PeerListFacade<A, R>,
    ) -> GossipResult<()>
    where
        R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
        A: ApiClient,
    {
        if self.is_syncing() {
            // If we are syncing, we don't want to broadcast data
            return Ok(());
        }

        debug!("Broadcasting data to peers: {}", data.data_type_and_id());

        // Get all active peers except the source
        let mut peers: Vec<(Address, PeerListItem)> = peer_list_service
            .top_active_peers(None, None)
            .await
            .map_err(|err| GossipError::Internal(InternalGossipError::Unknown(err.to_string())))?;

        debug!(
            "Node {:?}: Peers selected for broadcast: {:?}",
            self.client.mining_address, peers
        );

        peers.shuffle(&mut rand::thread_rng());

        while !peers.is_empty() {
            // Remove peers that seen the data since the last iteration
            let peers_that_seen_data = self.cache.peers_that_have_seen(data)?;
            peers.retain(|(peer_miner_address, _peer)| {
                !peers_that_seen_data.contains(peer_miner_address)
            });

            if peers.is_empty() {
                debug!(
                    "Node {:?}: No peers left to broadcast to",
                    self.client.mining_address
                );
                break;
            }

            let n = std::cmp::min(MAX_PEERS_PER_BROADCAST, peers.len());
            let maybe_selected_peers = peers.get(0..n);

            if let Some(selected_peers) = maybe_selected_peers {
                debug!(
                    "Node {:?}: Peers selected for the current broadcast step: {:?}",
                    self.client.mining_address, selected_peers
                );
                // Send data to selected peers
                for (peer_miner_address, peer_entry) in selected_peers {
                    if let Err(error) = self
                        .client
                        .send_data_and_update_score(
                            (peer_miner_address, peer_entry),
                            data,
                            peer_list_service,
                        )
                        .await
                    {
                        warn!(
                            "Node {:?}: Failed to send data to peer {}: {}",
                            self.client.mining_address, peer_miner_address, error
                        );
                    }

                    // Record as seen anyway, so we don't rebroadcast to them
                    if let Err(error) = self
                        .cache
                        .record_seen(*peer_miner_address, GossipCacheKey::from(data))
                    {
                        error!(
                            "Failed to record data in cache for peer {}: {}",
                            peer_miner_address, error
                        );
                    }
                }
            } else {
                debug!(
                    "Node {:?}, No peers selected for the current broadcast step",
                    self.client.mining_address
                );
                break;
            }

            tokio::time::sleep(BROADCAST_INTERVAL).await;
        }

        debug!("Node {:?}: Broadcast finished", self.client.mining_address);
        Ok(())
    }
}

fn spawn_cache_pruning_task(
    cache: Arc<GossipCache>,
    task_executor: &TaskExecutor,
) -> ServiceHandleWithShutdownSignal {
    ServiceHandleWithShutdownSignal::spawn(
        "gossip cache pruning",
        move |mut shutdown_rx| async move {
            info!("Starting cache pruning task");
            let mut interval = time::interval(CACHE_CLEANUP_INTERVAL);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Err(error) = cache.prune_expired(CACHE_ENTRY_TTL) {
                            error!("Failed to clean up cache: {}", error);
                            break;
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                }
            }

            debug!("Cleanup task complete");
        },
        task_executor,
    )
}

fn spawn_broadcast_task<R, A>(
    mut mempool_data_receiver: Receiver<GossipData>,
    service: GossipService,
    task_executor: &TaskExecutor,
    peer_list_service: PeerListFacade<A, R>,
) -> ServiceHandleWithShutdownSignal
where
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    A: ApiClient,
{
    ServiceHandleWithShutdownSignal::spawn(
        "gossip broadcast",
        move |mut shutdown_rx| async move {
            let peer_list_service = peer_list_service.clone();
            loop {
                tokio::select! {
                    maybe_data = mempool_data_receiver.recv() => {
                        match maybe_data {
                            Some(data) => {
                                if let Err(error) = service.broadcast_data(&data, &peer_list_service).await {
                                    warn!("Failed to broadcast data: {}", error);
                                };
                            },
                            None => break, // channel closed
                        }
                    },
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                }
            }

            debug!("Broadcast task complete");
        },
        task_executor,
    )
}

fn spawn_watcher_task(
    server: Server,
    server_handle: ServerHandle,
    mut cache_pruning_task_handle: ServiceHandleWithShutdownSignal,
    mut broadcast_task_handle: ServiceHandleWithShutdownSignal,
    task_executor: &TaskExecutor,
    block_pool_arbiter: actix::Arbiter,
) -> ServiceHandleWithShutdownSignal {
    ServiceHandleWithShutdownSignal::spawn(
        "gossip main",
        move |mut task_shutdown_signal| async move {
            debug!("Starting gossip service watch thread");

            let tasks_shutdown_handle = TaskManager::current()
                .executor()
                .spawn_critical_with_shutdown_signal("server shutdown task", |_| async move {
                    tokio::select! {
                        _ = task_shutdown_signal.recv() => {
                            debug!("Gossip service shutdown signal received");
                        }
                        cleanup_res = cache_pruning_task_handle.wait_for_exit() => {
                            warn!("Gossip cleanup exited because: {:?}", cleanup_res);
                        }
                        broadcast_res = broadcast_task_handle.wait_for_exit() => {
                            warn!("Gossip broadcast exited because: {:?}", broadcast_res);
                        }
                    }

                    debug!("Sending stop signal to server handle...");
                    server_handle.stop(true).await;
                    debug!("Server handle stop signal sent, waiting for server to shut down...");

                    debug!("Shutting down gossip service tasks");
                    let mut errors: Vec<GossipError> = vec![];

                    debug!("Gossip listener stopped");

                    let mut handle_result = |res: TaskExecutionResult| match res {
                        Ok(()) => {}
                        Err(error) => errors.push(GossipError::Internal(
                            InternalGossipError::Unknown(error.to_string()),
                        )),
                    };

                    info!("Stopping gossip cleanup");
                    handle_result(cache_pruning_task_handle.stop().await);
                    info!("Stopping gossip broadcast");
                    handle_result(broadcast_task_handle.stop().await);

                    if errors.is_empty() {
                        info!("Gossip main task finished without errors");
                    } else {
                        warn!("Gossip main task finished with errors:");
                        for error in errors {
                            warn!("Error: {}", error);
                        }
                    };
                });

            match server.await {
                Ok(()) => {
                    info!("Gossip server stopped");
                }
                Err(error) => {
                    warn!("Gossip server shutdown error: {}", error);
                }
            };
            match tasks_shutdown_handle.await {
                Ok(()) => {}
                Err(error) => {
                    warn!("Gossip service shutdown error: {}", error);
                }
            };
            block_pool_arbiter.stop();
        },
        task_executor,
    )
}

async fn catch_up_task<
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
>(
    sync_state: SyncState,
    mut latest_known_height: usize,
    api_client: A,
    peer_list_service: PeerListFacade<A, R>,
) -> Result<(), GossipError> {
    peer_list_service.wait_for_active_peers().await?;

    let limit = 10;

    let mut block_queue = VecDeque::new();
    let block_index = get_block_index(
        &peer_list_service,
        &api_client,
        latest_known_height as usize,
        limit,
        5,
    )
    .await?;

    let mut blocks_left_to_process = block_index.len();
    block_queue.extend(block_index);

    while let Some(block) = block_queue.pop_front() {
        match peer_list_service
            .request_block_from_the_network(block.block_hash)
            .await
        {
            Ok(()) => {
                latest_known_height += 1;
                info!(
                    "Successfully requested block {} (height {}) from the network",
                    block.block_hash, latest_known_height
                );
            }
            Err(err) => {
                error!(
                    "Failed to request block {} (height {}) from the network: {}",
                    block.block_hash, latest_known_height, err
                );
            }
        }

        blocks_left_to_process -= 1;
        if blocks_left_to_process == 0 {
            block_queue.extend(
                get_block_index(
                    &peer_list_service,
                    &api_client,
                    latest_known_height as usize,
                    limit,
                    5,
                )
                .await?,
            );
            blocks_left_to_process = block_queue.len();
            if blocks_left_to_process == 0 {
                break;
            }
        }
    }

    sync_state.store(false);
    info!("Gossip service sync completed");
    Ok(())
}

async fn get_block_index<
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
>(
    peer_list_service: &PeerListFacade<A, R>,
    api_client: &A,
    start: usize,
    limit: usize,
    retries: usize,
) -> GossipResult<Vec<BlockIndexItem>> {
    let top_peers = peer_list_service.top_active_peers(Some(5), None).await?;

    if top_peers.is_empty() {
        return Err(GossipError::Network("No peers available".to_string()));
    }

    for _ in 0..retries {
        let (miner_address, top_peer) = top_peers
            .choose(&mut rand::thread_rng())
            .ok_or(GossipError::Network("No peers available".to_string()))?;
        match api_client
            .get_block_index(
                top_peer.address.api,
                BlockIndexQuery {
                    height: start,
                    limit,
                },
            )
            .await
            .map_err(|network_error| GossipError::Network(network_error.to_string()))
        {
            Ok(index) => {
                return Ok(index);
            }
            Err(error) => {
                error!(
                    "Failed to fetch block index from peer {:?}: {:?}",
                    miner_address, error
                );
                continue;
            }
        }
    }

    Err(GossipError::Network(
        "Failed to fetch block index from peer".to_string(),
    ))
}

/// Replay vdf steps on local node, provided by an existing block's VDFLimiterInfo
pub async fn fast_forward_vdf_steps_from_block(
    vdf_limiter_info: VDFLimiterInfo,
    vdf_sender: Sender<BroadcastMiningSeed>,
) {
    let block_end_step = vdf_limiter_info.global_step_number;
    let len = vdf_limiter_info.steps.len();
    let block_start_step = block_end_step - len as u64;
    tracing::trace!(
        "VDF FF: block start-end step: {}-{}",
        block_start_step,
        block_end_step
    );
    for (i, hash) in vdf_limiter_info.steps.iter().enumerate() {
        //fast forward VDF step and seed before adding the new block...or we wont be at a new enough vdf step to "discover" block
        let mining_seed = BroadcastMiningSeed {
            seed: Seed { 0: *hash },
            global_step: block_start_step + i as u64,
            checkpoints: H256List::new(),
        };

        if let Err(e) = vdf_sender.send(mining_seed).await {
            error!("VDF FF: VDF Send Error: {:?}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::util::{ApiClientStub, FakeGossipServer, MockRethServiceActor};
    use irys_types::BlockHash;

    mod catch_up_task {
        use super::*;
        use crate::peer_list::PeerListServiceWithClient;
        use irys_storage::irys_consensus_data_db::open_or_create_irys_consensus_data_db;
        use irys_testing_utils::utils::setup_tracing_and_temp_dir;
        use irys_types::{Config, NodeConfig, PeerAddress, PeerScore};
        use std::sync::Mutex;

        #[actix_web::test]
        async fn should_sync_and_change_status() -> eyre::Result<()> {
            let temp_dir = setup_tracing_and_temp_dir(None, false);
            let mut node_config = NodeConfig::testnet();
            node_config.trusted_peers = vec![];
            let config = Config::new(node_config);

            let db = DatabaseProvider(Arc::new(
                open_or_create_irys_consensus_data_db(&temp_dir.path().to_path_buf())
                    .expect("can't open temp dir"),
            ));

            let block_requests = Arc::new(Mutex::new(vec![]));
            let block_requests_clone = block_requests.clone();
            let fake_gossip_server = FakeGossipServer::new();
            fake_gossip_server.set_on_block_data_request(move |block_hash| {
                let mut block_requests = block_requests.lock().unwrap();
                let requests_len = block_requests.len();
                block_requests.push(block_hash);

                // Simulating one false response so the block gets requested again
                if requests_len == 0 {
                    false
                } else {
                    true
                }
            });
            let fake_gossip_address = fake_gossip_server.spawn();

            let sync_state = SyncState::new(true);

            let api_client_stub = ApiClientStub::new();
            let calls = Arc::new(Mutex::new(vec![]));
            let block_index_requests = calls.clone();
            api_client_stub.set_block_index_handler(move |query| {
                let mut calls_ref = calls.lock().unwrap();
                let calls_len = calls_ref.len();
                calls_ref.push(query);

                // Simulate process needing to make two calls
                if calls_len == 0 {
                    Ok(vec![BlockIndexItem {
                        block_hash: BlockHash::repeat_byte(1),
                        num_ledgers: 0,
                        ledgers: vec![],
                    }])
                } else if calls_len == 1 {
                    Ok(vec![BlockIndexItem {
                        block_hash: BlockHash::repeat_byte(2),
                        num_ledgers: 0,
                        ledgers: vec![],
                    }])
                } else {
                    Ok(vec![])
                }
            });

            let reth_mock = MockRethServiceActor {};
            let reth_mock_addr = reth_mock.start();
            let peer_list_service = PeerListServiceWithClient::new_with_custom_api_client(
                db,
                &config,
                api_client_stub.clone(),
                reth_mock_addr.clone(),
            );
            let peer_list = PeerListFacade::new(peer_list_service.start());
            peer_list
                .add_peer(
                    Address::repeat_byte(2),
                    PeerListItem {
                        reputation_score: PeerScore::new(100),
                        response_time: 0,
                        address: PeerAddress {
                            gossip: fake_gossip_address,
                            api: fake_gossip_address,
                            execution: Default::default(),
                        },
                        last_seen: 0,
                        is_online: true,
                    },
                )
                .await
                .expect("to add peer");

            // Check that the sync status is syncing
            assert!(sync_state.is_syncing());

            catch_up_task(sync_state.clone(), 10, api_client_stub.clone(), peer_list)
                .await
                .expect("to finish catching up");

            // There should be three calls total: two that got items and one that didn't
            let data_requests = block_index_requests.lock().unwrap();
            assert_eq!(data_requests.len(), 3);
            assert_eq!(data_requests[0].height, 10);
            assert_eq!(data_requests[1].height, 11);
            assert_eq!(data_requests[0].limit, 10);
            assert_eq!(data_requests[1].limit, 10);
            assert_eq!(data_requests[2].height, 12);
            assert_eq!(data_requests[2].limit, 10);

            // Check that the sync status has changed to synced
            assert!(!sync_state.is_syncing());

            let block_requests = block_requests_clone.lock().unwrap();
            assert_eq!(block_requests.len(), 3);
            assert_eq!(block_requests[0], BlockHash::repeat_byte(1));
            // As the first call didn't return anything, the peer tries to fetch it once again
            assert_eq!(block_requests[1], BlockHash::repeat_byte(1));
            assert_eq!(block_requests[2], BlockHash::repeat_byte(2));

            Ok(())
        }
    }
}
