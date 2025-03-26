use crate::types::{InternalGossipError, InvalidDataError};
use crate::{
    cache::GossipCache,
    client::GossipClient,
    server::GossipServer,
    types::{GossipData, GossipError, GossipResult},
    PeerListProvider,
};
use actix::{Actor, Addr, Context, Handler};
use irys_actors::mempool_service::{ChunkIngressError, TxIngressError};
use irys_actors::mempool_service::{ChunkIngressMessage, TxIngressMessage};
use irys_database::tables::CompactPeerListItem;
use irys_types::DatabaseProvider;
use rand::seq::IteratorRandom;
use std::net::SocketAddr;
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, oneshot},
    time,
};

const ONE_HOUR: Duration = Duration::from_secs(3600);
const TWO_HOURS: Duration = Duration::from_secs(7200);
const MAX_PEERS_PER_BROADCAST: usize = 5;
const CACHE_CLEANUP_INTERVAL: Duration = ONE_HOUR;
const CACHE_ENTRY_TTL: Duration = TWO_HOURS;

type TaskExecutionResult = Result<Option<GossipResult<()>>, tokio::task::JoinError>;

#[derive(Debug)]
pub struct GossipServiceHandle {
    task_handle: tokio::task::JoinHandle<()>,
    service_shutdown_tx: oneshot::Sender<()>,
}

#[derive(Debug)]
pub struct ServiceHandleWithShutdownSignal<T> {
    pub handle: Option<tokio::task::JoinHandle<T>>,
    pub result: Option<T>,
    pub shutdown_tx: mpsc::Sender<()>,
    pub name: Option<String>,
}

impl<T> ServiceHandleWithShutdownSignal<T> {
    pub fn spawn<F, Fut>(name: Option<impl Into<String>>, f: F) -> Self
    where
        F: FnOnce(mpsc::Receiver<()>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let handle = tokio::spawn(f(shutdown_rx));
        Self {
            handle: Some(handle),
            result: None,
            shutdown_tx,
            name: name.map(Into::into),
        }
    }

    /// Stops the task, joins it and returns the result
    pub async fn stop(mut self) -> Result<Option<T>, tokio::task::JoinError> {
        let res = self.shutdown_tx.send(()).await;

        if let Err(e) = res {
            tracing::error!(
                "Failed to send shutdown signal to the task \"{}\": {}",
                self.name.as_deref().unwrap_or(""),
                e
            );
        }

        self.wait_for_exit().await?;

        tracing::debug!("Task \"{}\" stopped", self.name.as_deref().unwrap_or(""));
        Ok(self.result)
    }

    /// Waits for the task to exit or immediately returns if the task has already exited. To get
    ///  the execution result, call [ServiceHandleWithShutdownSignal::stop].
    pub async fn wait_for_exit(&mut self) -> Result<(), tokio::task::JoinError> {
        let handle = self.handle.take();
        match handle {
            Some(h) => match h.await {
                Ok(result) => {
                    self.result = Some(result);
                    Ok(())
                }
                Err(e) => Err(e),
            },
            None => Ok(()),
        }
    }
}

impl GossipServiceHandle {
    pub async fn stop(self) -> GossipResult<()> {
        self.service_shutdown_tx
            .send(())
            .map_err(|e| GossipError::Internal(InternalGossipError::Unknown(format!("{e:?}"))))?;
        self.task_handle
            .await
            .map_err(|e| GossipError::Internal(InternalGossipError::Unknown(e.to_string())))?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct GossipService<TMempoolService>
where
    TMempoolService: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Actor<Context = Context<TMempoolService>>,
{
    server: Option<GossipServer>,
    server_address: String,
    server_port: u16,
    client: GossipClient,
    cache: Arc<GossipCache>,
    peer_list: PeerListProvider,
    broadcast_message_rx: Option<mpsc::Receiver<(SocketAddr, GossipData)>>,
    pub mempool: Addr<TMempoolService>,
}

impl<TMempoolService> GossipService<TMempoolService>
where
    TMempoolService: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Actor<Context = Context<TMempoolService>>,
{
    pub fn new(
        server_address: impl Into<String>,
        server_port: u16,
        client_timeout: Duration,
        db: DatabaseProvider,
        mempool: Addr<TMempoolService>,
    ) -> (Self, mpsc::Sender<(SocketAddr, GossipData)>) {
        let cache = Arc::new(GossipCache::new());
        let (message_tx, message_rx) = mpsc::channel(1000);

        let peer_list = PeerListProvider::new(db);

        let server = GossipServer::new(cache.clone(), message_tx.clone(), peer_list.clone());
        let client = GossipClient::new(client_timeout);

        (
            Self {
                server: Some(server),
                server_address: server_address.into(),
                server_port,
                client,
                cache,
                peer_list,
                broadcast_message_rx: Some(message_rx),
                mempool,
            },
            message_tx,
        )
    }

    pub async fn run(mut self) -> GossipResult<ServiceHandleWithShutdownSignal<GossipResult<()>>> {
        tracing::debug!("Staring gossip service");

        let server = self.server.take().ok_or(GossipError::Internal(
            InternalGossipError::ServerAlreadyRunning,
        ))?;
        let server = server.run(&self.server_address, self.server_port)?;
        let server_handle = server.handle();

        let mut broadcast_message_rx =
            self.broadcast_message_rx
                .take()
                .ok_or(GossipError::Internal(
                    InternalGossipError::BroadcastReceiverShutdown,
                ))?;

        let service = Arc::new(self);

        let service_handle_for_cleanup = service.clone();
        let mut cleanup_handle = ServiceHandleWithShutdownSignal::spawn(
            Some("gossip cache cleanup"),
            move |mut shutdown_rx| async move {
                let mut interval = time::interval(CACHE_CLEANUP_INTERVAL);

                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            if let Err(e) = service_handle_for_cleanup.cache.cleanup(CACHE_ENTRY_TTL) {
                                tracing::error!("Failed to clean up cache: {}", e);
                                return Err(GossipError::Internal(InternalGossipError::CacheCleanup(e.to_string())));
                            }
                        }
                        _ = shutdown_rx.recv() => {
                            return Ok(());
                        }
                    }
                }
            },
        );

        let mut broadcast_message_handle = ServiceHandleWithShutdownSignal::spawn(
            Some("gossip broadcast"),
            move |mut shutdown_rx| {
                async move {
                    loop {
                        tokio::select! {
                            maybe_message = broadcast_message_rx.recv() => {
                                match maybe_message {
                                    Some((source_address, data)) => {
                                       match handle_gossip_data_from_other_peer(source_address, data, service.clone()).await {
                                             Ok(()) => {}
                                             Err(e) => {
                                                match e {
                                                    GossipError::Network(e) => {
                                                        tracing::warn!("Gossip error: {}", e);
                                                    }
                                                    GossipError::InvalidPeer(e) => {
                                                        tracing::warn!("Gossip error: {}", e);
                                                    }
                                                    GossipError::Cache(e) => {
                                                        tracing::warn!("Gossip cache error: {}", e);
                                                    }
                                                    GossipError::Internal(internal) => {
                                                        tracing::error!("Internal gossip error: {:?}", internal);
                                                    }
                                                    GossipError::InvalidData(data_error) => {
                                                        // Not a service error - most likely the
                                                        //  peer sent us bogus data
                                                    }
                                                }
                                             }
                                        };
                                    },
                                    None => return Ok(()), // channel closed
                                }
                            },
                            _ = shutdown_rx.recv() => {
                                return Ok(());
                            }
                        }
                    }
                }
            },
        );

        let gossip_service_handle = ServiceHandleWithShutdownSignal::spawn(
            Some("gossip main"),
            move |mut shutdown_rx| async move {
                tracing::debug!("Starting gossip service watch thread");
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        tracing::debug!("Gossip service shutdown signal received");
                    }
                    res = broadcast_message_handle.wait_for_exit() => {
                        tracing::debug!("Gossip broadcast exited because: {:?}", res);
                    }
                    cleanup_res = cleanup_handle.wait_for_exit() => {
                        tracing::debug!("Gossip cleanup exited because: {:?}", cleanup_res);
                    }
                    server_res = server => {
                        tracing::debug!("Gossip server exited because: {:?}", server_res);
                    }
                }

                tracing::debug!("Shutting down gossip service tasks");
                let mut errors: Vec<GossipError> = vec![];

                server_handle.stop(true).await;
                tracing::debug!("Task \"gossip web server\" stopped");

                let mut handle_result = |res: TaskExecutionResult| match res {
                    Ok(maybe_response) => {
                        maybe_response.map(|r| r.map_err(|e| errors.push(e)));
                    }
                    Err(e) => errors.push(GossipError::Internal(InternalGossipError::Unknown(
                        e.to_string(),
                    ))),
                };

                handle_result(broadcast_message_handle.stop().await);
                handle_result(cleanup_handle.stop().await);

                if errors.is_empty() {
                    Ok(())
                } else {
                    Err(GossipError::Internal(InternalGossipError::Unknown(
                        format!("{:?}", errors),
                    )))
                }
            },
        );

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
            .map_err(|e| GossipError::Internal(InternalGossipError::Unknown(e.to_string())))?
            .into_iter()
            .filter(|peer| {
                let is_not_source = match original_source {
                    GossipSource::Internal => false,
                    GossipSource::External(ip) => peer.address != ip,
                };
                peer.is_online
                    && peer.reputation_score.is_active()
                    && !is_not_source
                    && !self
                        .cache
                        .has_seen(&peer.address, data, CACHE_ENTRY_TTL)
                        .unwrap_or(true)
            })
            .collect();

        // Select random subset of peers
        let selected_peers: Vec<&CompactPeerListItem> = peers
            .iter()
            .choose_multiple(&mut rand::thread_rng(), MAX_PEERS_PER_BROADCAST);

        // Send data to selected peers
        for peer in selected_peers {
            if let Err(e) = self.client.send_data(peer, data).await {
                tracing::warn!("Failed to send data to peer {}: {}", peer.address, e);
                continue;
            }

            if let Err(e) = self.cache.record_seen(peer.address, data) {
                tracing::error!(
                    "Failed to record data in cache for peer {}: {}",
                    peer.address,
                    e
                );
            }
        }

        Ok(())
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum GossipSource {
    Internal,
    External(SocketAddr),
}

impl GossipSource {
    pub fn from_ip(ip: SocketAddr) -> Self {
        GossipSource::External(ip)
    }

    pub fn is_internal(&self) -> bool {
        matches!(self, GossipSource::Internal)
    }
}

// /// This function handles the gossip data coming from the internal message bus.
// /// It assumes that all data has been validated by the mempool service and is ready to be gossiped.
// fn handle_gossip_data_from_internal_bus<T>(data: GossipData, service: Arc<GossipService<T>>) -> GossipResult<()> {
//     service.broadcast_data(data)?;
// }

async fn handle_gossip_data_from_other_peer<T>(
    source_address: SocketAddr,
    data: GossipData,
    service: Arc<GossipService<T>>,
) -> GossipResult<()>
where
    T: Handler<TxIngressMessage> + Handler<ChunkIngressMessage> + Actor<Context = Context<T>>,
{
    match data {
        GossipData::Transaction(tx) => {
            tracing::debug!(
                "Gossip transaction received from the internal message bus: {:?}",
                tx
            );
            match service.mempool.send(TxIngressMessage(tx.clone())).await {
                Ok(message_result) => {
                    match message_result {
                        Ok(()) => {
                            // Success. Mempool will send the tx data to the internal mempool,
                            //  but we still need to update the cache with the source address.
                            service
                                .cache
                                .record_seen(source_address, &GossipData::Transaction(tx))
                        }
                        Err(e) => {
                            match e {
                                // ==== Not really errors
                                TxIngressError::Skipped => {
                                    // Not an invalid transaction - just skipped
                                    service
                                        .cache
                                        .record_seen(source_address, &GossipData::Transaction(tx))
                                }
                                // ==== External errors
                                TxIngressError::InvalidSignature => {
                                    // Invalid signature, decrease source reputation
                                    Err(GossipError::InvalidData(
                                        InvalidDataError::TransactionSignature,
                                    ))
                                }
                                TxIngressError::Unfunded => {
                                    // Unfunded transaction, decrease source reputation
                                    Err(GossipError::InvalidData(
                                        InvalidDataError::TransactionUnfunded,
                                    ))
                                }
                                TxIngressError::InvalidAnchor => {
                                    // Invalid anchor, decrease source reputation
                                    Err(GossipError::InvalidData(
                                        InvalidDataError::TransactionAnchor,
                                    ))
                                }
                                // ==== Internal errors - shouldn't be communicated to outside
                                TxIngressError::DatabaseError => {
                                    Err(GossipError::Internal(InternalGossipError::Database))
                                }
                                TxIngressError::ServiceUninitialized => Err(GossipError::Internal(
                                    InternalGossipError::ServiceUninitialized,
                                )),
                                TxIngressError::Other(e) => {
                                    Err(GossipError::Internal(InternalGossipError::Unknown(e)))
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to send transaction to mempool: {}", e);
                    Err(GossipError::Internal(InternalGossipError::Unknown(
                        e.to_string(),
                    )))
                }
            }
        }
        GossipData::Chunk(chunk) => {
            match service
                .mempool
                .send(ChunkIngressMessage(chunk.clone()))
                .await
            {
                Ok(message_result) => {
                    match message_result {
                        Ok(()) => {
                            // Success. Mempool will send the tx data to the internal mempool,
                            //  but we still need to update the cache with the source address.
                            service
                                .cache
                                .record_seen(source_address, &GossipData::Chunk(chunk))
                        }
                        Err(e) => {
                            match e {
                                ChunkIngressError::UnknownTransaction => {
                                    // TODO:
                                    //  I suppose we have to ask the peer for transaction,
                                    //  but what if it doesn't have one?
                                    Ok(())
                                }
                                // ===== External invalid data errors
                                ChunkIngressError::InvalidProof => Err(GossipError::InvalidData(
                                    InvalidDataError::ChunkInvalidProof,
                                )),
                                ChunkIngressError::InvalidDataHash => {
                                    Err(GossipError::InvalidData(
                                        InvalidDataError::ChinkInvalidDataHash,
                                    ))
                                }
                                ChunkIngressError::InvalidChunkSize => {
                                    Err(GossipError::InvalidData(
                                        InvalidDataError::ChunkInvalidChunkSize,
                                    ))
                                }
                                // ===== Internal errors
                                ChunkIngressError::DatabaseError => {
                                    Err(GossipError::Internal(InternalGossipError::Database))
                                }
                                ChunkIngressError::ServiceUninitialized => {
                                    Err(GossipError::Internal(
                                        InternalGossipError::ServiceUninitialized,
                                    ))
                                }
                                ChunkIngressError::Other(other) => {
                                    Err(GossipError::Internal(InternalGossipError::Unknown(other)))
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to send transaction to mempool: {}", e);
                    Err(GossipError::Internal(InternalGossipError::Unknown(
                        e.to_string(),
                    )))
                }
            }
        }
        GossipData::Block(_block) => {
            // TODO: implement
            Ok(())
        }
    }
}
