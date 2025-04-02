// This rule is added here because otherwise clippy starts to throw warnings about using %
//  at random macro uses in this file for whatever reason. The second one is because
//  I have absolutely no idea how to name this module to satisfy this lint
#![allow(
    clippy::integer_division_remainder_used,
    clippy::module_name_repetitions,
    reason = "I don't know how to name it"
)]
use crate::server_data_handler::GossipServerDataHandler;
use crate::types::InternalGossipError;
use crate::{
    cache::GossipCache,
    client::GossipClient,
    server::GossipServer,
    types::{GossipError, GossipResult},
    PeerListProvider,
};
use actix::{Actor, Addr, Context, Handler};
use actix_web::dev::{Server, ServerHandle};
use core::net::SocketAddr;
use core::time::Duration;
use irys_actors::block_discovery::BlockDiscoveredMessage;
use irys_actors::mempool_service::TxExistenceQuery;
use irys_actors::mempool_service::{ChunkIngressMessage, TxIngressMessage};
use irys_api_client::ApiClient;
use irys_database::tables::CompactPeerListItem;
use irys_types::{DatabaseProvider, GossipData};
use rand::seq::IteratorRandom as _;
use std::sync::Arc;
use tokio::{sync::mpsc, time};

const ONE_HOUR: Duration = Duration::from_secs(3600);
const TWO_HOURS: Duration = Duration::from_secs(7200);
const MAX_PEERS_PER_BROADCAST: usize = 5;
const CACHE_CLEANUP_INTERVAL: Duration = ONE_HOUR;
const CACHE_ENTRY_TTL: Duration = TWO_HOURS;

type TaskExecutionResult = Result<GossipResult<()>, tokio::task::JoinError>;

#[derive(Debug)]
pub struct ServiceHandleWithShutdownSignal<T> {
    pub handle: tokio::task::JoinHandle<T>,
    pub shutdown_tx: mpsc::Sender<()>,
    pub name: Option<String>,
}

impl<T> ServiceHandleWithShutdownSignal<T> {
    pub fn spawn<F, Fut, S>(name: Option<S>, task: F) -> Self
    where
        F: FnOnce(mpsc::Receiver<()>) -> Fut + Send + 'static,
        Fut: core::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
        S: Into<String>,
    {
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let handle = tokio::spawn(task(shutdown_rx));
        Self {
            handle,
            shutdown_tx,
            name: name.map(Into::into),
        }
    }

    /// Stops the task, joins it and returns the result
    ///
    /// # Errors
    ///
    /// If the task panics, an error is returned.
    pub async fn stop(mut self) -> Result<T, tokio::task::JoinError> {
        let res = self.shutdown_tx.send(()).await;

        if let Err(error) = res {
            tracing::error!(
                "Failed to send shutdown signal to the task \"{}\": {}",
                self.name.as_deref().unwrap_or(""),
                error
            );
        }

        let result = self.wait_for_exit().await?;

        tracing::debug!("Task \"{}\" stopped", self.name.as_deref().unwrap_or(""));

        Ok(result)
    }

    /// Waits for the task to exit or immediately returns if the task has already exited. To get
    ///  the execution result, call [`ServiceHandleWithShutdownSignal::stop`].
    ///
    /// # Errors
    ///
    /// If the task panics, an error is returned.
    pub async fn wait_for_exit(&mut self) -> Result<T, tokio::task::JoinError> {
        let handle = &mut self.handle;
        handle.await
    }
}

#[derive(Debug)]
pub struct GossipService {
    server_address: String,
    server_port: u16,
    cache: Arc<GossipCache>,
    peer_list: PeerListProvider,
    mempool_data_receiver: Option<mpsc::Receiver<GossipData>>,
    client: GossipClient,
}

impl GossipService {
    /// Create a new gossip service. To run the service, use the [`GossipService::run`] method.
    /// Also returns a channel to send trusted gossip data to the service. Trusted data should
    /// be sent by the internal components of the system only after complete validation.
    pub fn new<T: Into<String>>(
        server_address: T,
        server_port: u16,
        irys_db: DatabaseProvider,
    ) -> (Self, mpsc::Sender<GossipData>) {
        let cache = Arc::new(GossipCache::new());
        let (trusted_data_tx, trusted_data_rx) = mpsc::channel(1000);

        let peer_list = PeerListProvider::new(irys_db);

        let client_timeout = Duration::from_secs(5);
        let client = GossipClient::new(client_timeout);

        (
            Self {
                server_address: server_address.into(),
                server_port,
                client,
                cache,
                peer_list,
                mempool_data_receiver: Some(trusted_data_rx),
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
    pub fn run<M, B, A>(
        mut self,
        mempool: Addr<M>,
        block_discovery: Addr<B>,
        api_client: A,
    ) -> GossipResult<ServiceHandleWithShutdownSignal<GossipResult<()>>>
    where
        M: Handler<TxIngressMessage>
            + Handler<ChunkIngressMessage>
            + Handler<TxExistenceQuery>
            + Actor<Context = Context<M>>,
        B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
        A: ApiClient + Clone + 'static,
    {
        // Ok(ServiceHandleWithShutdownSignal::spawn(Some("stub"), |a| {
        //    async {
        //        println!("Bibka");
        //        Ok(())
        //    }
        // }))
        tracing::debug!("Staring gossip service");

        let server_data_handler = GossipServerDataHandler {
            mempool,
            block_discovery,
            api_client,
            cache: Arc::clone(&self.cache),
        };
        let server = GossipServer::new(server_data_handler, self.peer_list.clone());

        let server = server.run(&self.server_address, self.server_port)?;
        let server_handle = server.handle();

        let mempool_data_receiver =
            self.mempool_data_receiver
                .take()
                .ok_or(GossipError::Internal(
                    InternalGossipError::BroadcastReceiverShutdown,
                ))?;

        let service = Arc::new(self);

        let cleanup_handle = spawn_cache_cleanup_task(Arc::clone(&service.cache));

        let broadcast_task_handle =
            spawn_broadcast_task(mempool_data_receiver, Arc::clone(&service));

        let gossip_service_handle =
            spawn_main_task(server, server_handle, cleanup_handle, broadcast_task_handle);

        Ok(gossip_service_handle)
    }

    async fn broadcast_data(
        &self,
        original_source: GossipSource,
        data: &GossipData,
    ) -> GossipResult<()> {
        // Get all active peers except the source
        let peers: Vec<CompactPeerListItem> = self
            .peer_list
            .all_known_peers()
            .map_err(|error| {
                GossipError::Internal(InternalGossipError::Unknown(error.to_string()))
            })?
            .into_iter()
            .filter(|peer| {
                let is_not_source = match original_source {
                    GossipSource::Internal => false,
                    GossipSource::External(ip) => peer.address.gossip != ip,
                };
                peer.is_online
                    && peer.reputation_score.is_active()
                    && !is_not_source
                    && !self
                        .cache
                        .has_seen(&peer.address.gossip, data, CACHE_ENTRY_TTL)
                        .unwrap_or(true)
            })
            .collect();

        // Select random subset of peers
        let selected_peers: Vec<&CompactPeerListItem> = peers
            .iter()
            .choose_multiple(&mut rand::thread_rng(), MAX_PEERS_PER_BROADCAST);

        // Send data to selected peers
        for peer in selected_peers {
            if let Err(error) = self.client.send_data(peer, data).await {
                tracing::warn!(
                    "Failed to send data to peer {}: {}",
                    peer.address.gossip,
                    error
                );
                continue;
            }

            if let Err(error) = self.cache.record_seen(peer.address.gossip, data) {
                tracing::error!(
                    "Failed to record data in cache for peer {}: {}",
                    peer.address.gossip,
                    error
                );
            }
        }

        Ok(())
    }
}

fn spawn_cache_cleanup_task(
    cache: Arc<GossipCache>,
) -> ServiceHandleWithShutdownSignal<GossipResult<()>> {
    ServiceHandleWithShutdownSignal::spawn(
        Some("gossip cache cleanup"),
        move |mut shutdown_rx| async move {
            let mut interval = time::interval(CACHE_CLEANUP_INTERVAL);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Err(error) = cache.cleanup(CACHE_ENTRY_TTL) {
                            tracing::error!("Failed to clean up cache: {}", error);
                            return Err(GossipError::Internal(InternalGossipError::CacheCleanup(error.to_string())));
                        }
                    }
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                }
            }

            tracing::debug!("Cleanup task complete");

            Ok(())
        },
    )
}

fn spawn_broadcast_task(
    mut mempool_data_receiver: mpsc::Receiver<GossipData>,
    service: Arc<GossipService>,
) -> ServiceHandleWithShutdownSignal<GossipResult<()>> {
    ServiceHandleWithShutdownSignal::spawn(
        Some("gossip broadcast"),
        move |mut shutdown_rx| async move {
            let service = Arc::clone(&service);
            loop {
                tokio::select! {
                    maybe_data = mempool_data_receiver.recv() => {
                        match maybe_data {
                            Some(data) => {
                                match service.broadcast_data(GossipSource::Internal, &data).await {
                                    Ok(()) => {}
                                    Err(error) => {
                                        tracing::warn!("Failed to broadcast data: {}", error);
                                    }
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

            tracing::debug!("Broadcast task complete");

            Ok(())
        },
    )
}

fn spawn_main_task(
    server: Server,
    server_handle: ServerHandle,
    mut cleanup_handle: ServiceHandleWithShutdownSignal<GossipResult<()>>,
    mut broadcast_task_handle: ServiceHandleWithShutdownSignal<GossipResult<()>>,
) -> ServiceHandleWithShutdownSignal<GossipResult<()>> {
    ServiceHandleWithShutdownSignal::spawn(Some("gossip main"), move |mut shutdown_rx| async move {
        tracing::debug!("Starting gossip service watch thread");

        let tasks_shutdown_handle = tokio::spawn(async move {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    tracing::debug!("Gossip service shutdown signal received");
                }
                cleanup_res = cleanup_handle.wait_for_exit() => {
                    tracing::warn!("Gossip cleanup exited because: {:?}", cleanup_res);
                }
                broadcast_res = broadcast_task_handle.wait_for_exit() => {
                    tracing::warn!("Gossip broadcast exited because: {:?}", broadcast_res);
                }
            }

            tracing::debug!("Sending stop signal to server handle...");
            server_handle.stop(true).await;
            tracing::debug!("Server handle stop signal sent, waiting for server to shut down...");

            tracing::debug!("Shutting down gossip service tasks");
            let mut errors: Vec<GossipError> = vec![];

            tracing::debug!("Gossip listener stopped");

            let mut handle_result = |res: TaskExecutionResult| match res {
                Ok(result) => match result {
                    Ok(()) => {}
                    Err(error) => errors.push(error),
                },
                Err(error) => errors.push(GossipError::Internal(InternalGossipError::Unknown(
                    error.to_string(),
                ))),
            };

            tracing::info!("Stopping gossip cleanup");
            handle_result(cleanup_handle.stop().await);
            tracing::info!("Stopping gossip broadcast");
            handle_result(broadcast_task_handle.stop().await);

            if errors.is_empty() {
                tracing::info!("Gossip main task finished without errors");
                Ok(())
            } else {
                Err(GossipError::Internal(InternalGossipError::Unknown(
                    format!("{errors:?}"),
                )))
            }
        });

        let _ = server.await;
        tasks_shutdown_handle.await.unwrap()
    })
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum GossipSource {
    Internal,
    External(SocketAddr),
}

impl GossipSource {
    #[must_use]
    pub const fn from_ip(ip: SocketAddr) -> Self {
        Self::External(ip)
    }

    #[must_use]
    pub const fn is_internal(&self) -> bool {
        matches!(self, Self::Internal)
    }
}

// async fn handle_gossip_data_from_other_peer<T>(
//     source_address: SocketAddr,
//     data: GossipData,
//     service: &GossipService,
//     mempool: &Addr<T>,
//     api_client: &impl ApiClient,
// ) -> GossipResult<()>
// where
//     T: Handler<TxIngressMessage>
//         + Handler<ChunkIngressMessage>
//         + Handler<TxExistenceQuery>
//         + Actor<Context = Context<T>>,
// {
//     match data {
//         GossipData::Transaction(tx) => {
//             handle_transaction(&mempool, tx, &service.cache, source_address).await
//         }
//         GossipData::Chunk(chunk) => {
//             handle_chunk(&mempool, chunk, &service.cache, source_address).await
//         }
//         GossipData::Block(irys_block_header) => {
//             handle_block_header(&mempool, irys_block_header, &service.cache, source_address, api_client).await
//         }
//     }
// }
