// This rule is added here because otherwise clippy starts to throw warnings about using %
//  at random macro uses in this file for whatever reason. The second one is because
//  I have absolutely no idea how to name this module to satisfy this lint
#![allow(
    clippy::integer_division_remainder_used,
    clippy::module_name_repetitions,
    reason = "I don't know how to name it"
)]
use crate::block_pool::BlockPool;
use crate::block_status_provider::BlockStatusProvider;
use crate::execution_payload_provider::ExecutionPayloadProvider;
use crate::peer_list::PeerList;
use crate::server_data_handler::GossipServerDataHandler;
use crate::types::InternalGossipError;
use crate::{
    cache::GossipCache,
    gossip_client::GossipClient,
    server::GossipServer,
    types::{GossipError, GossipResult},
    SyncState,
};
use actix_web::dev::{Server, ServerHandle};
use core::time::Duration;
use irys_actors::services::ServiceSenders;
use irys_actors::{block_discovery::BlockDiscoveryFacade, mempool_service::MempoolFacade};
use irys_api_client::ApiClient;
use irys_types::{Address, Config, DatabaseProvider, GossipBroadcastMessage, PeerListItem};
use irys_vdf::state::VdfStateReadonly;
use rand::prelude::SliceRandom as _;
use reth_tasks::{TaskExecutor, TaskManager};
use std::net::TcpListener;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::{
    sync::mpsc::{channel, error::SendError, Receiver, Sender},
    time,
};
use tracing::{debug, error, info, warn, Span};

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
pub struct P2PService {
    cache: Arc<GossipCache>,
    broadcast_data_receiver: Option<UnboundedReceiver<GossipBroadcastMessage>>,
    client: GossipClient,
    pub sync_state: SyncState,
}

impl P2PService {
    /// Returns whether the gossip service is currently syncing
    pub fn is_syncing(&self) -> bool {
        self.sync_state.is_syncing()
    }

    /// Waits until the gossip service has completed syncing
    pub async fn wait_for_sync(&self) {
        self.sync_state.wait_for_sync().await
    }

    /// Create a new gossip service. To run the service, use the [`P2PService::run`] method.
    /// Also returns a channel to send trusted gossip data to the service. Trusted data should
    /// be sent by the internal components of the system only after complete validation.
    pub fn new(
        mining_address: Address,
        broadcast_data_receiver: UnboundedReceiver<GossipBroadcastMessage>,
    ) -> Self {
        let cache = Arc::new(GossipCache::new());

        let client_timeout = Duration::from_secs(5);
        let client = GossipClient::new(client_timeout, mining_address);

        Self {
            client,
            cache,
            broadcast_data_receiver: Some(broadcast_data_receiver),
            sync_state: SyncState::new(true, false),
        }
    }

    /// Spawns all gossip tasks and returns a handle to the service. The service will run until
    /// the stop method is called or the task is dropped.
    ///
    /// # Errors
    ///
    /// If the service fails to start, an error is returned. This can happen if the server fails to
    /// bind to the address or if any of the tasks fails to spawn.
    pub fn run<M, B, A, P>(
        mut self,
        mempool: M,
        block_discovery: B,
        api_client: A,
        task_executor: &TaskExecutor,
        peer_list: P,
        db: DatabaseProvider,
        listener: TcpListener,
        block_status_provider: BlockStatusProvider,
        execution_payload_provider: ExecutionPayloadProvider<P>,
        vdf_state: VdfStateReadonly,
        config: Config,
        service_senders: ServiceSenders,
    ) -> GossipResult<(ServiceHandleWithShutdownSignal, Arc<BlockPool<P, B, M>>)>
    where
        M: MempoolFacade,
        B: BlockDiscoveryFacade,
        A: ApiClient,
        P: PeerList,
    {
        debug!("Staring gossip service");

        let block_pool = BlockPool::new(
            db,
            peer_list.clone(),
            block_discovery,
            mempool.clone(),
            self.sync_state.clone(),
            block_status_provider,
            execution_payload_provider.clone(),
            vdf_state,
            config,
            service_senders,
        );

        let arc_pool = Arc::new(block_pool);

        let server_data_handler = GossipServerDataHandler {
            mempool,
            block_pool: Arc::clone(&arc_pool),
            api_client,
            cache: Arc::clone(&self.cache),
            gossip_client: self.client.clone(),
            peer_list: peer_list.clone(),
            sync_state: self.sync_state.clone(),
            span: Span::current(),
            execution_payload_provider,
        };
        let server = GossipServer::new(server_data_handler, peer_list.clone());

        let server = server.run(listener)?;
        let server_handle = server.handle();

        let mempool_data_receiver =
            self.broadcast_data_receiver
                .take()
                .ok_or(GossipError::Internal(
                    InternalGossipError::BroadcastReceiverShutdown,
                ))?;

        let cache = Arc::clone(&self.cache);

        let cache_pruning_task_handle = spawn_cache_pruning_task(cache, task_executor);

        let broadcast_task_handle =
            spawn_broadcast_task(mempool_data_receiver, self, task_executor, peer_list);

        let gossip_service_handle = spawn_watcher_task(
            server,
            server_handle,
            cache_pruning_task_handle,
            broadcast_task_handle,
            task_executor,
        );

        Ok((gossip_service_handle, arc_pool))
    }

    async fn broadcast_data<P>(
        &self,
        broadcast_message: &GossipBroadcastMessage,
        peer_list: &P,
    ) -> GossipResult<()>
    where
        P: PeerList,
    {
        // Check if gossip broadcast is enabled
        if !self.sync_state.is_gossip_broadcast_enabled() {
            debug!("Gossip broadcast is disabled, skipping broadcast");
            return Ok(());
        }

        debug!(
            "Broadcasting data to peers: {}",
            broadcast_message.data_type_and_id()
        );

        // Get all active peers except the source
        let mut peers: Vec<(Address, PeerListItem)> = peer_list
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
            let peers_that_seen_data = self.cache.peers_that_have_seen(&broadcast_message.key)?;
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
                            &broadcast_message.data,
                            peer_list,
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
                        .record_seen(*peer_miner_address, broadcast_message.key)
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

fn spawn_broadcast_task<P>(
    mut mempool_data_receiver: UnboundedReceiver<GossipBroadcastMessage>,
    service: P2PService,
    task_executor: &TaskExecutor,
    peer_list: P,
) -> ServiceHandleWithShutdownSignal
where
    P: PeerList,
{
    ServiceHandleWithShutdownSignal::spawn(
        "gossip broadcast",
        move |mut shutdown_rx| async move {
            let peer_list = peer_list.clone();
            loop {
                tokio::select! {
                    maybe_data = mempool_data_receiver.recv() => {
                        match maybe_data {
                            Some(data) => {
                                if let Err(error) = service.broadcast_data(&data, &peer_list).await {
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
        },
        task_executor,
    )
}
