use crate::{
    cache::GossipCache,
    client::GossipClient,
    server::GossipServer,
    types::{GossipData, GossipError, GossipResult},
    PeerListProvider,
};
use actix_web::dev::ServerHandle;
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

pub struct GossipServiceHandle {
    task_handle: tokio::task::JoinHandle<()>,
    service_shutdown_tx: oneshot::Sender<()>,
}

pub struct ServiceHandleWithShutdownSignal<T> {
    pub handle: tokio::task::JoinHandle<T>,
    pub shutdown_signal: oneshot::Sender<()>,
}

impl<T> ServiceHandleWithShutdownSignal<T> {
    pub fn spawn<F, Fut>(f: F) -> Self
    where
        F: FnOnce(oneshot::Receiver<()>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (shutdown_signal, shutdown_rx) = oneshot::channel();
        let handle = tokio::spawn(f(shutdown_rx));
        Self {
            handle,
            shutdown_signal,
        }
    }

    pub async fn stop(self) -> Result<T, tokio::task::JoinError> {
        let _ = self.shutdown_signal.send(());
        self.handle.await
    }
}

impl GossipServiceHandle {
    pub async fn stop(self) -> GossipResult<()> {
        self.service_shutdown_tx
            .send(())
            .map_err(|e| GossipError::Internal(format!("{e:?}")))?;
        self.task_handle
            .await
            .map_err(|e| GossipError::Internal(e.to_string()))?;
        Ok(())
    }
}

pub struct GossipService {
    server: Option<GossipServer>,
    server_address: String,
    server_port: u16,
    server_handle: Option<ServerHandle>,
    client: GossipClient,
    cache: Arc<GossipCache>,
    peer_list: PeerListProvider,
    message_rx: mpsc::Receiver<(SocketAddr, GossipData)>,
    shutdown_rx: oneshot::Receiver<()>,
    shutdown_tx: oneshot::Sender<()>,
}

impl GossipService {
    pub fn new(
        server_address: String,
        server_port: u16,
        client_timeout: Duration,
        db: DatabaseProvider,
    ) -> (Self, mpsc::Sender<(SocketAddr, GossipData)>) {
        let cache = Arc::new(GossipCache::new());
        let (message_tx, message_rx) = mpsc::channel(1000);

        let peer_list = PeerListProvider::new(db);

        let server = GossipServer::new(cache.clone(), message_tx.clone(), peer_list.clone());
        let client = GossipClient::new(client_timeout);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        (
            Self {
                server: Some(server),
                server_address,
                server_port,
                server_handle: None,
                client,
                cache,
                peer_list,
                message_rx,
                shutdown_rx,
                shutdown_tx,
            },
            message_tx,
        )
    }

    pub async fn run(mut self) -> GossipResult<GossipServiceHandle> {
        let server = self
            .server
            .take()
            .ok_or(GossipError::Internal("Server already running".to_string()))?;
        let server = server.run(&self.server_address, self.server_port)?;
        let server_handle = server.handle();

        let service = Arc::new(self);

        let service_handle_for_cleanup = service.clone();
        let cleanup_handle = ServiceHandleWithShutdownSignal::spawn(
            move |shutdown_rx| async move {
                let mut interval = time::interval(CACHE_CLEANUP_INTERVAL);

                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            if let Err(e) = service_handle_for_cleanup.cache.cleanup(CACHE_ENTRY_TTL) {
                                tracing::error!("Failed to clean up cache: {}", e);
                            }
                        }
                        _ = shutdown_rx => {
                            break;
                        }
                    }
                }
            },
        );

        // let message_handle = tokio::spawn(self.process_messages());

        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let service_task_handle = tokio::spawn(async move {
            let res: GossipResult<()> = tokio::select! {
                result = server => {
                    result.map_err(|e| GossipError::Internal(e.to_string()))
                }
                // result = cleanup_handle => {
                //     result.map_err(|e| GossipError::Internal(e.to_string()))
                // }
                // result = message_handle => {
                //     result.map_err(|e| GossipError::Internal(e.to_string()))
                // }
                _ = shutdown_rx => {
                    tracing::info!("Stopping gossip service");
                    Ok(())
                }
            };

            server_handle.stop(true).await;
            let _ = cleanup_handle.stop().await;
            // let _ = message_handle.stop().await;
        });

        Ok(GossipServiceHandle {
            task_handle: service_task_handle,
            service_shutdown_tx: shutdown_tx,
        })
    }

    async fn run_cache_cleanup(self) -> GossipResult<()> {
        let mut interval = time::interval(CACHE_CLEANUP_INTERVAL);

        loop {
            interval.tick().await;
            if let Err(e) = self.cache.cleanup(CACHE_ENTRY_TTL) {
                tracing::error!("Failed to clean up cache: {}", e);
            }
        }
    }

    async fn process_messages(mut self) -> GossipResult<()> {
        while let Some((source_ip, data)) = self.message_rx.recv().await {
            if let Err(e) = self.broadcast_data(source_ip, &data).await {
                tracing::error!("Failed to broadcast data: {}", e);
            }
        }

        Ok(())
    }

    async fn broadcast_data(&self, source_ip: SocketAddr, data: &GossipData) -> GossipResult<()> {
        let mut rng = rand::thread_rng();

        // Get all active peers except the source
        let peers: Vec<CompactPeerListItem> = self
            .peer_list
            .all_known_peers()
            .map_err(|e| GossipError::Internal(e.to_string()))?
            .into_iter()
            .filter(|peer| {
                peer.is_online
                    && peer.reputation_score.is_active()
                    && peer.address != source_ip
                    && !self
                        .cache
                        .has_seen(&peer.address, data, CACHE_ENTRY_TTL)
                        .unwrap_or(true)
            })
            .collect();

        // Select random subset of peers
        let selected_peers: Vec<&CompactPeerListItem> = peers
            .iter()
            .choose_multiple(&mut rng, MAX_PEERS_PER_BROADCAST);

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
