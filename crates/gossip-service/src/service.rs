use crate::{
    cache::GossipCache,
    client::GossipClient,
    server::GossipServer,
    types::{GossipData, GossipError, GossipResult},
    PeerListProvider,
};
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

#[derive(Debug)]
pub struct GossipServiceHandle {
    task_handle: tokio::task::JoinHandle<()>,
    service_shutdown_tx: oneshot::Sender<()>,
}

#[derive(Debug)]
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

#[derive(Debug)]
pub struct GossipService {
    server: Option<GossipServer>,
    server_address: String,
    server_port: u16,
    client: GossipClient,
    cache: Arc<GossipCache>,
    peer_list: PeerListProvider,
    broadcast_message_rx: Option<mpsc::Receiver<(SocketAddr, GossipData)>>,
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

        (
            Self {
                server: Some(server),
                server_address,
                server_port,
                client,
                cache,
                peer_list,
                broadcast_message_rx: Some(message_rx),
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

        let mut broadcast_message_rx = self
            .broadcast_message_rx
            .take()
            .ok_or(GossipError::Internal("Broadcast message receiver already taken".to_string()))?;

        let service = Arc::new(self);

        let service_handle_for_cleanup = service.clone();
        let cleanup_handle = ServiceHandleWithShutdownSignal::spawn(
            move |mut shutdown_rx| async move {
                let mut interval = time::interval(CACHE_CLEANUP_INTERVAL);

                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            if let Err(e) = service_handle_for_cleanup.cache.cleanup(CACHE_ENTRY_TTL) {
                                tracing::error!("Failed to clean up cache: {}", e);
                            }
                        }
                        _ = &mut shutdown_rx => {
                            break;
                        }
                    }
                }
            },
        );

        let broadcast_message_handle = ServiceHandleWithShutdownSignal::spawn(move |shutdown_rx| {
            async move {
                while let Some((source_ip, data)) = broadcast_message_rx.recv().await {
                    if let Err(e) = service.broadcast_data(source_ip, &data).await {
                        tracing::error!("Failed to broadcast data: {}", e);
                    }
                }
            }
        });

        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let service_task_handle = tokio::spawn(async move {
            let _ = tokio::select! {
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
            let _ = broadcast_message_handle.stop().await;
        });

        Ok(GossipServiceHandle {
            task_handle: service_task_handle,
            service_shutdown_tx: shutdown_tx,
        })
    }

    async fn broadcast_data(&self, source_ip: SocketAddr, data: &GossipData) -> GossipResult<()> {
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
