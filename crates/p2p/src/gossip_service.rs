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
use crate::gossip_data_handler::GossipDataHandler;
use crate::types::InternalGossipError;
use crate::{
    cache::GossipCache,
    gossip_client::GossipClient,
    server::GossipServer,
    types::{GossipError, GossipResult},
    SyncChainServiceMessage,
};
use actix_web::dev::{Server, ServerHandle};
use core::time::Duration;
use irys_actors::mempool_guard::MempoolReadGuard;
use irys_actors::services::ServiceSenders;
use irys_actors::{block_discovery::BlockDiscoveryFacade, mempool_service::MempoolFacade};
use irys_domain::chain_sync_state::ChainSyncState;
use irys_domain::execution_payload_cache::ExecutionPayloadCache;
use irys_domain::{BlockIndexReadGuard, BlockTreeReadGuard, PeerList};
use irys_types::version_pd::GossipBroadcastMessageVersionPD;
use irys_types::Traced;
use irys_types::{
    Config, DatabaseProvider, IrysAddress, IrysPeerId, P2PGossipConfig, ProtocolVersion,
};
use reth_tasks::TaskExecutor;
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc::{
    channel, error::SendError, Receiver, Sender, UnboundedReceiver, UnboundedSender,
};
use tracing::{debug, info, instrument, warn, Instrument as _};

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
        let handle = task_executor.spawn_task(task(shutdown_rx));
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
    #[tracing::instrument(level = "trace", skip_all, err)]
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
    #[tracing::instrument(level = "trace", skip_all, err)]
    pub async fn wait_for_exit(&mut self) -> Result<(), tokio::task::JoinError> {
        info!("Waiting for task \"{}\" to exit", self.name);
        let handle = &mut self.handle;
        handle.await
    }
}

#[derive(Debug)]
pub struct P2PService {
    cache: Arc<GossipCache>,
    broadcast_data_receiver: Option<UnboundedReceiver<Traced<GossipBroadcastMessageVersionPD>>>,
    client: GossipClient,
    pub sync_state: ChainSyncState,
    gossip_cfg: P2PGossipConfig,
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
        mining_address: IrysAddress,
        peer_id: IrysPeerId,
        broadcast_data_receiver: UnboundedReceiver<Traced<GossipBroadcastMessageVersionPD>>,
    ) -> Self {
        let cache = Arc::new(GossipCache::new());

        let client_timeout = Duration::from_secs(5);
        let client = GossipClient::new(client_timeout, mining_address, peer_id);

        Self {
            client,
            cache,
            broadcast_data_receiver: Some(broadcast_data_receiver),
            sync_state: ChainSyncState::new(true, false),
            gossip_cfg: P2PGossipConfig::default(),
        }
    }

    /// Spawns all gossip tasks and returns a handle to the service. The service will run until
    /// the stop method is called or the task is dropped.
    ///
    /// # Errors
    ///
    /// If the service fails to start, an error is returned. This can happen if the server fails to
    /// bind to the address or if any of the tasks fails to spawn.
    pub fn run<M, B>(
        mut self,
        mempool: M,
        block_discovery: B,
        task_executor: &TaskExecutor,
        peer_list: PeerList,
        db: DatabaseProvider,
        listener: TcpListener,
        block_status_provider: BlockStatusProvider,
        execution_payload_provider: ExecutionPayloadCache,
        config: Config,
        service_senders: ServiceSenders,
        chain_sync_tx: UnboundedSender<SyncChainServiceMessage>,
        mempool_guard: MempoolReadGuard,
        block_index: BlockIndexReadGuard,
        block_tree: BlockTreeReadGuard,
        started_at: Instant,
    ) -> GossipResult<(
        Server,
        ServerHandle,
        ServiceHandleWithShutdownSignal,
        Arc<BlockPool<B, M>>,
        Arc<GossipDataHandler<M, B>>,
    )>
    where
        M: MempoolFacade,
        B: BlockDiscoveryFacade,
    {
        debug!("Starting the gossip service");

        let chunk_ingress =
            irys_actors::chunk_ingress_service::facade::ChunkIngressFacadeImpl::from(
                &service_senders,
            );

        let block_pool = BlockPool::new(
            db,
            block_discovery,
            mempool.clone(),
            chain_sync_tx,
            self.sync_state.clone(),
            block_status_provider,
            execution_payload_provider.clone(),
            config.clone(),
            service_senders,
            mempool_guard,
        );

        let arc_pool = Arc::new(block_pool);

        let consensus_config_hash = config.consensus.keccak256_hash();
        let gossip_data_handler = Arc::new(GossipDataHandler {
            mempool,
            chunk_ingress,
            block_pool: Arc::clone(&arc_pool),
            cache: Arc::clone(&self.cache),
            gossip_client: self.client.clone(),
            peer_list: peer_list.clone(),
            sync_state: self.sync_state.clone(),
            execution_payload_cache: execution_payload_provider,
            data_request_tracker: crate::rate_limiting::DataRequestTracker::new(),
            block_index,
            block_tree,
            config: config.clone(),
            started_at,
            consensus_config_hash,
        });
        let server = GossipServer::new(
            Arc::clone(&gossip_data_handler),
            peer_list.clone(),
            config.node_config.p2p_gossip.max_concurrent_gossip_chunks,
        );

        let server = server.run(listener)?;
        let server_handle = server.handle();

        let broadcast_data_receiver =
            self.broadcast_data_receiver
                .take()
                .ok_or(GossipError::Internal(
                    InternalGossipError::BroadcastReceiverShutdown,
                ))?;

        // Load gossip config from NodeConfig
        self.gossip_cfg = config.node_config.p2p_gossip;

        // Wrap the service in an Arc so we can spawn a detached broadcast per message
        let service_arc = Arc::new(self);

        let broadcast_task_handle = spawn_broadcast_task(
            broadcast_data_receiver,
            Arc::clone(&service_arc),
            task_executor,
            peer_list,
        );

        debug!("Started gossip service");

        Ok((
            server,
            server_handle,
            broadcast_task_handle,
            arc_pool,
            gossip_data_handler,
        ))
    }

    #[instrument(name = "broadcast_data", skip_all)]
    async fn broadcast_data(
        &self,
        broadcast_message: GossipBroadcastMessageVersionPD,
        peer_list: &PeerList,
    ) -> GossipResult<()> {
        // Check if gossip broadcast is enabled
        if !self.sync_state.is_gossip_broadcast_enabled() {
            debug!("Gossip broadcast is disabled, skipping broadcast");
            return Ok(());
        }

        let message_type_and_id = broadcast_message.data_type_and_id();
        let GossipBroadcastMessageVersionPD { key, data } = broadcast_message;
        let broadcast_data = Arc::new(data);

        // Pre-serialize once for all V2 peers (O(1) clone per peer instead of N serializations)
        let preserialized = broadcast_data
            .to_v2()
            .as_ref()
            .and_then(|v2_data| self.client.pre_serialize_for_broadcast(v2_data));

        debug!("Broadcasting data to peers: {}", message_type_and_id);

        // Get all peers sorted by score, so we broadcast to the best ones first, but try to
        // reach to all peers eventually.
        // TODO: we need to make an algorithm that doesn't try to reach all peers every time,
        //  but rather a random subset of them. We should take n top peers, and add some
        //  randomly selected peers from the rest of the list.
        let mut peers = peer_list.all_peers_sorted_by_score();

        if peers.is_empty() {
            debug!(
                "Node {:?}: No peers to broadcast to",
                self.client.mining_address
            );
        }

        while !peers.is_empty() {
            // Remove peers that have seen the data since the last iteration
            let peers_that_seen_data = self.cache.peers_that_have_seen(&key)?;
            peers.retain(|(peer_id, _peer)| !peers_that_seen_data.contains(peer_id));

            if peers.is_empty() {
                debug!(
                    "Node {:?}: No peers left to broadcast to",
                    self.client.mining_address
                );
                break;
            }

            let n = std::cmp::min(self.gossip_cfg.broadcast_batch_size, peers.len());
            let selected_peers = peers.drain(0..n);

            debug!(
                "Node {:?}: Peers selected for the current broadcast step: {:?}",
                self.client.mining_address, selected_peers
            );
            // Send data to selected peers
            for (peer_miner_address, peer_entry) in selected_peers {
                if peer_entry.protocol_version == ProtocolVersion::V2 {
                    if let Some((route, ref body)) = preserialized {
                        self.client.send_preserialized_detached(
                            (&peer_miner_address, &peer_entry),
                            route,
                            body.clone(),
                            peer_list,
                            Arc::clone(&self.cache),
                            key,
                        );
                        continue;
                    }
                }
                // V1 peers or preserialization failure: fall back to existing path
                self.client.send_data_and_update_the_score_detached(
                    (&peer_miner_address, &peer_entry),
                    Arc::clone(&broadcast_data),
                    peer_list,
                    Arc::clone(&self.cache),
                    key,
                );
            }
        }

        tokio::time::sleep(Duration::from_millis(
            self.gossip_cfg.broadcast_batch_throttle_interval,
        ))
        .await;

        debug!("Node {:?}: Broadcast finished", self.client.mining_address);
        Ok(())
    }
}

fn spawn_broadcast_task(
    mut broadcast_data_receiver: UnboundedReceiver<Traced<GossipBroadcastMessageVersionPD>>,
    service: std::sync::Arc<P2PService>,
    task_executor: &TaskExecutor,
    peer_list: PeerList,
) -> ServiceHandleWithShutdownSignal {
    ServiceHandleWithShutdownSignal::spawn(
        "gossip broadcast",
        move |mut shutdown_rx| async move {
            let peer_list = peer_list.clone();
            loop {
                tokio::select! {
                    maybe_data = broadcast_data_receiver.recv() => {
                        match maybe_data {
                            Some(traced) => {
                                let (broadcast_message, parent_span) = traced.into_parts();
                                let span = tracing::info_span!(parent: &parent_span, "gossip_broadcast");
                                // For each incoming message, spawn a detached task so broadcasts don't block each other
                                let service = std::sync::Arc::clone(&service);
                                let peer_list = peer_list.clone(); // clone: shared across spawned tasks
                                tokio::spawn(async move {
                                    if let Err(error) = service.broadcast_data(broadcast_message, &peer_list).await {
                                        warn!("Failed to broadcast data: {}", error);
                                    }
                                }.instrument(span));
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

pub fn spawn_p2p_server_watcher_task(
    server: Server,
    server_handle: ServerHandle,
    mut broadcast_task_handle: ServiceHandleWithShutdownSignal,
    task_executor: &TaskExecutor,
) -> ServiceHandleWithShutdownSignal {
    let task_executor_clone = task_executor.clone();
    ServiceHandleWithShutdownSignal::spawn(
        "gossip main",
        move |mut task_shutdown_signal| async move {
            debug!("Starting gossip service watch thread");

            let tasks_shutdown_handle = task_executor_clone.spawn_critical_with_shutdown_signal(
                "server shutdown task",
                |_| async move {
                    let mut early_exit_result: Option<TaskExecutionResult> = None;
                    tokio::select! {
                        _ = task_shutdown_signal.recv() => {
                            debug!("Gossip service shutdown signal received");
                        }
                        broadcast_res = broadcast_task_handle.wait_for_exit() => {
                            warn!("Gossip broadcast exited because: {:?}", broadcast_res);
                            early_exit_result = Some(broadcast_res);
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

                    if let Some(res) = early_exit_result {
                        info!("Gossip broadcast already exited");
                        handle_result(res);
                    } else {
                        info!("Stopping gossip broadcast");
                        handle_result(broadcast_task_handle.stop().await);
                    }

                    if errors.is_empty() {
                        info!("Gossip main task finished without errors");
                    } else {
                        warn!("Gossip main task finished with errors:");
                        for error in errors {
                            warn!("Error: {}", error);
                        }
                    };
                },
            );

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
