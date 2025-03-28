use crate::types::{InternalGossipError, InvalidDataError};
use crate::{
    cache::GossipCache,
    client::GossipClient,
    server::GossipServer,
    types::{tx_ingress_error_to_gossip_error, GossipError, GossipResult},
    PeerListProvider,
};
use actix::{Actor, Addr, Context, Handler};
use irys_actors::mempool_service::{ChunkIngressError, TxExistenceQuery, TxIngressError};
use irys_actors::mempool_service::{ChunkIngressMessage, TxIngressMessage};
use irys_api_client::ApiClient;
use irys_database::tables::CompactPeerListItem;
use irys_types::{DatabaseProvider, GossipData, H256};
use rand::seq::IteratorRandom;
use std::net::SocketAddr;
use std::{sync::Arc, time::Duration};
use tokio::{sync::mpsc, time};

const ONE_HOUR: Duration = Duration::from_secs(3600);
const TWO_HOURS: Duration = Duration::from_secs(7200);
const MAX_PEERS_PER_BROADCAST: usize = 5;
const CACHE_CLEANUP_INTERVAL: Duration = ONE_HOUR;
const CACHE_ENTRY_TTL: Duration = TWO_HOURS;

type TaskExecutionResult = Result<Option<GossipResult<()>>, tokio::task::JoinError>;

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

#[derive(Debug)]
pub struct GossipService {
    server: Option<GossipServer>,
    server_address: String,
    server_port: u16,
    cache: Arc<GossipCache>,
    peer_list: PeerListProvider,
    untrusted_gossip_data_rx: Option<mpsc::Receiver<(SocketAddr, GossipData)>>,
    trusted_gossip_data_rx: Option<mpsc::Receiver<GossipData>>,
    client: GossipClient,
}

impl GossipService {
    /// Create a new gossip service. To run the service, use the [GossipService::run] method.
    /// Also returns a channel to send trusted gossip data to the service. Trusted data should
    /// be sent by the internal components of the system only after complete validation.
    pub fn new(
        server_address: impl Into<String>,
        server_port: u16,
        irys_db: DatabaseProvider,
    ) -> (Self, mpsc::Sender<GossipData>) {
        let cache = Arc::new(GossipCache::new());
        let (untrusted_data_tx, untrusted_data_rx) = mpsc::channel(1000);
        let (trusted_data_tx, trusted_data_rx) = mpsc::channel(1000);

        let peer_list = PeerListProvider::new(irys_db.clone());

        let client_timeout = Duration::from_secs(5);
        let server = GossipServer::new(cache.clone(), untrusted_data_tx, peer_list.clone());
        let client = GossipClient::new(client_timeout);

        (
            Self {
                server: Some(server),
                server_address: server_address.into(),
                server_port,
                client,
                cache,
                peer_list,
                untrusted_gossip_data_rx: Some(untrusted_data_rx),
                trusted_gossip_data_rx: Some(trusted_data_rx),
            },
            trusted_data_tx,
        )
    }

    pub async fn run<TMemPoolService>(
        mut self,
        mempool: Addr<TMemPoolService>,
        api_client: impl ApiClient + 'static,
    ) -> GossipResult<ServiceHandleWithShutdownSignal<GossipResult<()>>>
    where
        TMemPoolService: Handler<TxIngressMessage>
            + Handler<ChunkIngressMessage>
            + Handler<TxExistenceQuery>
            + Actor<Context = Context<TMemPoolService>>,
    {
        tracing::debug!("Staring gossip service");

        let server = self.server.take().ok_or(GossipError::Internal(
            InternalGossipError::ServerAlreadyRunning,
        ))?;
        let server = server.run(&self.server_address, self.server_port)?;
        let server_handle = server.handle();

        let mut untrusted_gossip_data_rx =
            self.untrusted_gossip_data_rx
                .take()
                .ok_or(GossipError::Internal(
                    InternalGossipError::BroadcastReceiverShutdown,
                ))?;

        let mut trusted_gossip_data_rx =
            self.trusted_gossip_data_rx
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

        let service_handle_for_gossip_data_receiver = service.clone();
        let mut gossip_data_receiver_handle = ServiceHandleWithShutdownSignal::spawn(
            Some("gossip handler for untrusted data from other peers"),
            move |mut shutdown_rx| {
                let service = service_handle_for_gossip_data_receiver;
                async move {
                    loop {
                        tokio::select! {
                            maybe_message = untrusted_gossip_data_rx.recv() => {
                                match maybe_message {
                                    Some((source_address, data)) => {
                                       match handle_gossip_data_from_other_peer(source_address, data, service.clone(), mempool.clone(), &api_client).await {
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
                                                    GossipError::InvalidData(_data_error) => {
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

        let mut broadcast_task_handle = ServiceHandleWithShutdownSignal::spawn(
            Some("gossip broadcast"),
            move |mut shutdown_rx| async move {
                let service = service.clone();
                loop {
                    tokio::select! {
                        maybe_data = trusted_gossip_data_rx.recv() => {
                            match maybe_data {
                                Some(data) => {
                                    match service.broadcast_data(GossipSource::Internal, &data).await {
                                        Ok(()) => {}
                                        Err(e) => {
                                            tracing::warn!("Failed to broadcast data: {}", e);
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
                    res = gossip_data_receiver_handle.wait_for_exit() => {
                        tracing::debug!("Handler of gossip data from other peers exited because: {:?}", res);
                    }
                    cleanup_res = cleanup_handle.wait_for_exit() => {
                        tracing::debug!("Gossip cleanup exited because: {:?}", cleanup_res);
                    }
                    server_res = server => {
                        tracing::debug!("Gossip server exited because: {:?}", server_res);
                    }
                    broadcast_res = broadcast_task_handle.wait_for_exit() => {
                        tracing::debug!("Gossip broadcast exited because: {:?}", broadcast_res);
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

                handle_result(gossip_data_receiver_handle.stop().await);
                handle_result(cleanup_handle.stop().await);
                handle_result(broadcast_task_handle.stop().await);

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
    service: Arc<GossipService>,
    mempool: Addr<T>,
    api_client: &impl ApiClient,
) -> GossipResult<()>
where
    T: Handler<TxIngressMessage>
        + Handler<ChunkIngressMessage>
        + Handler<TxExistenceQuery>
        + Actor<Context = Context<T>>,
{
    match data {
        GossipData::Transaction(tx) => {
            tracing::debug!(
                "Gossip transaction received from the internal message bus: {:?}",
                tx
            );
            match mempool.send(TxIngressMessage(tx.clone())).await {
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
            match mempool.send(ChunkIngressMessage(chunk.clone())).await {
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
        GossipData::Block(block) => {
            tracing::debug!(
                "Gossip block received from peer {}: {:?}",
                source_address,
                block.irys.block_hash
            );

            // Get all transaction IDs from the block
            let data_tx_ids = block
                .irys
                .data_ledgers
                .iter()
                .flat_map(|ledger| ledger.tx_ids.0.clone())
                .collect::<Vec<H256>>();

            let mut missing_tx_ids = Vec::new();

            for tx_id in block
                .irys
                .data_ledgers
                .iter()
                .flat_map(|ledger| ledger.tx_ids.0.clone())
            {
                if !is_known_tx(&mempool, tx_id).await? {
                    missing_tx_ids.push(tx_id);
                }
            }

            for system_tx_id in block
                .irys
                .system_ledgers
                .iter()
                .flat_map(|ledger| ledger.tx_ids.0.clone())
            {
                if !is_known_tx(&mempool, system_tx_id).await? {
                    missing_tx_ids.push(system_tx_id);
                }
            }

            // Fetch missing transactions from the source peer
            let missing_txs = api_client
                .get_transactions(source_address, &missing_tx_ids)
                .await
                .map_err(|e| {
                    tracing::error!(
                        "Failed to fetch transactions from peer {}: {}",
                        source_address,
                        e
                    );
                    GossipError::unknown(e)
                })?;

            // Process each transaction
            for (tx_id, tx) in data_tx_ids.iter().zip(missing_txs.iter()) {
                if let Some(tx) = tx {
                    // Send transaction to mempool
                    match mempool.send(TxIngressMessage(tx.clone())).await {
                        Ok(message_result) => {
                            match message_result {
                                Ok(()) => {
                                    // Success. Record in cache
                                    service.cache.record_seen(
                                        source_address,
                                        &GossipData::Transaction(tx.clone()),
                                    )?;
                                }
                                Err(e) => {
                                    match tx_ingress_error_to_gossip_error(e) {
                                        Some(GossipError::InvalidData(e)) => {
                                            // Invalid transaction, decrease source reputation
                                            return Err(GossipError::InvalidData(e));
                                        }
                                        Some(GossipError::Internal(e)) => {
                                            // Internal error - log it
                                            tracing::error!("Internal error: {:?}", e);
                                            return Err(GossipError::Internal(e));
                                        }
                                        Some(e) => {
                                            // Other error - log it
                                            tracing::error!("Unexpected error when handling gossip transaction: {:?}", e);
                                            return Err(e);
                                        }
                                        None => {
                                            // Not an invalid transaction - just skipped
                                            service.cache.record_seen(
                                                source_address,
                                                &GossipData::Transaction(tx.clone()),
                                            )?;
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to send transaction to mempool: {}", e);
                            return Err(GossipError::Internal(InternalGossipError::Unknown(
                                e.to_string(),
                            )));
                        }
                    }
                } else {
                    tracing::warn!(
                        "Missing transaction {} in block from peer {}",
                        tx_id,
                        source_address
                    );
                    return Err(GossipError::InvalidData(InvalidDataError::InvalidBlock));
                }
            }

            // Record block in cache
            service
                .cache
                .record_seen(source_address, &GossipData::Block(block))?;
            Ok(())
        }
    }
}

async fn is_known_tx<T>(mempool: &Addr<T>, tx_id: H256) -> Result<bool, GossipError>
where
    T: Handler<TxExistenceQuery> + Actor<Context = Context<T>>,
{
    mempool
        .send(TxExistenceQuery(tx_id))
        .await
        .map_err(GossipError::unknown)?
        .map_err(|e| {
            tx_ingress_error_to_gossip_error(e).unwrap_or(GossipError::unknown(
                "Did not receive an error from mempool where an error was expected",
            ))
        })
}
