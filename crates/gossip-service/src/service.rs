use std::{net::IpAddr, sync::Arc, time::Duration};
use actix_web::dev::ServerHandle;
use rand::seq::IteratorRandom;
use tokio::{
    sync::{mpsc, oneshot},
    time,
};

use crate::{
    cache::GossipCache,
    client::GossipClient,
    server::{GossipServer, PeerList},
    types::{GossipData, GossipError, GossipResult, PeerInfo},
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

    pub async fn stop(self) -> T {
        let _ = self.shutdown_signal.send(());
        self.handle.await
    }
}

impl GossipServiceHandle {
    pub async fn stop(mut self) -> GossipResult<()> {
        self.service_shutdown_tx.send(()).map_err(|e| GossipError::Internal(e.to_string()))?;
        self.task_handle.await.map_err(|e| GossipError::Internal(e.to_string()))?;
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
    peer_list: Arc<dyn PeerList>,
    message_rx: mpsc::Receiver<(IpAddr, GossipData)>,
    shutdown_rx: oneshot::Receiver<()>,
    shutdown_tx: oneshot::Sender<()>,
}

impl GossipService {
    pub fn new(
        server_address: String,
        server_port: u16,
        client_timeout: Duration,
        peer_list: Arc<dyn PeerList>,
    ) -> (Self, mpsc::Sender<(IpAddr, GossipData)>) {
        let cache = Arc::new(GossipCache::new());
        let (message_tx, message_rx) = mpsc::channel(1000);

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
        let service = Arc::new(self);
        let server = self.server.take().ok_or(GossipError::Internal("Server already running".to_string()))?;
        let server = server.run(&service.server_address, service.server_port)?;
        let server_handle = server.handle();

        let cleanup_handle = ServiceHandleWithShutdownSignal::spawn(|shutdown_rx| {
            let mut interval = time::interval(CACHE_CLEANUP_INTERVAL);

            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Err(e) = self.cache.cleanup(CACHE_ENTRY_TTL) {
                            tracing::error!("Failed to clean up cache: {}", e);
                        }
                    }
                    _ = shutdown_rx => {
                        break;
                    }
                }
            }
        });

        let message_handle = tokio::spawn(self.process_messages());

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

    async fn broadcast_data(&self, source_ip: IpAddr, data: &GossipData) -> GossipResult<()> {
        let mut rng = rand::thread_rng();

        // Get all active peers except the source
        let peers: Vec<PeerInfo> = self
            .peer_list
            .get_all_peers()
            .await
            .into_iter()
            .filter(|peer| {
                peer.is_online && 
                peer.score.is_active() && 
                peer.ip != source_ip &&
                !self.cache.has_seen(&peer.ip, data, CACHE_ENTRY_TTL).unwrap_or(true)
            })
            .collect();

        // Select random subset of peers
        let selected_peers: Vec<&PeerInfo> = peers
            .iter()
            .choose_multiple(&mut rng, MAX_PEERS_PER_BROADCAST);

        // Send data to selected peers
        for peer in selected_peers {
            if let Err(e) = self.client.send_data(peer, data).await {
                tracing::warn!("Failed to send data to peer {}: {}", peer.ip, e);
                continue;
            }

            if let Err(e) = self.cache.record_seen(peer.ip, data) {
                tracing::error!("Failed to record data in cache for peer {}: {}", peer.ip, e);
            }
        }

        Ok(())
    }
} 