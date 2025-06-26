pub mod chunks;
pub mod commitment_txs;
pub mod data_txs;
pub mod facade;
pub mod inner;
pub mod lifecycle;

pub use chunks::*;
pub use facade::*;
pub use inner::*;

use crate::block_tree_service::{BlockMigratedEvent, BlockTreeReadGuard, ReorgEvent};
use crate::services::ServiceSenders;
use crate::CommitmentStateReadGuard;
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_storage::StorageModulesReadGuard;
use irys_types::{app_state::DatabaseProvider, Config};
use reth::tasks::{shutdown::GracefulShutdown, TaskExecutor};
use std::{pin::pin, sync::Arc};
use tokio::sync::broadcast;
use tokio::{
    sync::{mpsc::UnboundedReceiver, RwLock},
    task::JoinHandle,
};
use tracing::info;

/// The Mempool oversees pending transactions and validation of incoming tx.
#[derive(Debug)]
pub struct MempoolService {
    shutdown: GracefulShutdown,
    msg_rx: UnboundedReceiver<MempoolServiceMessage>, // mempool message receiver
    reorg_rx: broadcast::Receiver<ReorgEvent>,        // reorg broadcast receiver
    block_migrated_rx: broadcast::Receiver<BlockMigratedEvent>, // block broadcast migrated receiver
    inner: Inner,
}

impl Default for MempoolService {
    fn default() -> Self {
        unimplemented!("don't rely on the default implementation of the `MempoolService`");
    }
}

impl MempoolService {
    /// Spawn a new Mempool service
    pub fn spawn_service(
        exec: &TaskExecutor,
        irys_db: DatabaseProvider,
        reth_node_adapter: IrysRethNodeAdapter,
        storage_modules_guard: StorageModulesReadGuard,
        block_tree_read_guard: &BlockTreeReadGuard,
        commitment_state_guard: &CommitmentStateReadGuard,
        rx: UnboundedReceiver<MempoolServiceMessage>,
        config: &Config,
        service_senders: &ServiceSenders,
    ) -> eyre::Result<JoinHandle<()>> {
        info!("mempool service spawned");
        let block_tree_read_guard = block_tree_read_guard.clone();
        let config = config.clone();
        let mempool_config = &config.consensus.mempool;
        let mempool_state = create_state(mempool_config);
        let exec = exec.clone();
        let commitment_state_guard = commitment_state_guard.clone();
        let storage_modules_guard = storage_modules_guard;
        let service_senders = service_senders.clone();
        let reorg_rx = service_senders.subscribe_reorgs();
        let block_migrated_rx = service_senders.subscribe_block_migrated();

        Ok(exec.clone().spawn_critical_with_graceful_shutdown_signal(
            "Mempool Service",
            |shutdown| async move {
                let mempool_service = Self {
                    shutdown,
                    msg_rx: rx,
                    reorg_rx,
                    block_migrated_rx,
                    inner: Inner {
                        block_tree_read_guard,
                        commitment_state_guard,
                        config,
                        exec,
                        irys_db,
                        mempool_state: Arc::new(RwLock::new(mempool_state)),
                        reth_node_adapter,
                        service_senders,
                        storage_modules_guard,
                    },
                };
                mempool_service
                    .start()
                    .await
                    .expect("Mempool service encountered an irrecoverable error")
            },
        ))
    }

    async fn start(mut self) -> eyre::Result<()> {
        tracing::info!("starting Mempool service");

        self.inner.restore_mempool_from_disk().await;

        let mut shutdown_future = pin!(self.shutdown);
        let shutdown_guard = loop {
            tokio::select! {
                // Handle regular mempool messages
                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(msg) => {
                            self.inner.handle_message(msg).await?;
                        }
                        None => {
                            tracing::warn!("receiver channel closed");
                            break None;
                        }
                    }
                }

                // Handle reorg events
                reorg_result = self.reorg_rx.recv() => {
                    if let Some(event) = handle_broadcast_recv(reorg_result, "Reorg") {
                        self.inner.handle_reorg(event).await?;
                    }
                }

                // Handle block migrated events
                 migrated_result = self.block_migrated_rx.recv() => {
                    if let Some(event) = handle_broadcast_recv(migrated_result, "BlockMigrated") {
                        self.inner.handle_block_migrated(event).await?;
                    }
                }


                // Handle shutdown signal
                shutdown = &mut shutdown_future => {
                    tracing::warn!("shutdown signal received");
                    break Some(shutdown);
                }
            }
        };

        tracing::debug!(amount_of_messages = ?self.msg_rx.len(), "processing last in-bound messages before shutdown");
        while let Ok(msg) = self.msg_rx.try_recv() {
            self.inner.handle_message(msg).await?;
        }

        // explicitly inform the TaskManager that we're shutting down
        drop(shutdown_guard);

        self.inner.persist_mempool_to_disk().await?;

        tracing::info!("shutting down Mempool service");
        Ok(())
    }
}

pub fn handle_broadcast_recv<T>(
    result: Result<T, broadcast::error::RecvError>,
    channel_name: &str,
) -> Option<T> {
    match result {
        Ok(event) => Some(event),
        Err(broadcast::error::RecvError::Closed) => {
            tracing::debug!("{} channel closed", channel_name);
            None
        }
        Err(broadcast::error::RecvError::Lagged(n)) => {
            tracing::warn!("{} lagged by {} events", channel_name, n);
            if n > 5 {
                tracing::error!("{} significantly lagged", channel_name);
            }
            None
        }
    }
}
