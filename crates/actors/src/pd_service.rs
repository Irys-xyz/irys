use irys_types::{
    range_specifier::{ChunkRangeId, ChunkRangeSpecifier},
    TokioServiceHandle, UnpackedChunk,
};
use reth::tasks::shutdown::Shutdown;
use std::sync::Arc;
use tokio::sync::{
    mpsc::{self, UnboundedReceiver},
    oneshot,
};
use tracing::{debug, error, info, warn, Instrument as _};

pub mod chunk_range_request_lru;
pub mod unpacked_chunks_lru;

/// Describes the mpsc messages that can be sent to this service
pub enum PDServiceMessage {
    ProvisionChunks(ChunkRangeSpecifier),
    LockChunks(ChunkRangeId),
    UnlockChunks(ChunkRangeId),
}

/// Broadcast event to notify any listeners of chunks becoming available
#[derive(Debug, Clone)]
pub struct ChunkRangeProvisionedEvent {
    pub chunk: Arc<[UnpackedChunk]>,
}

/// PD Service Struct
pub struct PDService {
    shutdown: Shutdown,
    msg_rx: UnboundedReceiver<PDServiceMessage>,
    inner: PDServiceInner,
}

/// Inner implementation of the Service, allows it to be owned by the tokio task
struct PDServiceInner {}

impl PDServiceInner {
    fn new() -> Self {
        PDServiceInner {}
    }

    fn handle_provision_chunks(&self, chunk_range_specifier: &ChunkRangeSpecifier) {
        let chunk_range_id = chunk_range_specifier.get_id();
        info!(
            "Provisioning chunks for id:{}\n ChunkRange: {:?}",
            chunk_range_id, chunk_range_specifier
        )

        // Chunk lifecycle:
        // - States: Requested → Provisioned
        // - Each chunk tracks which chunk_range_ids reference it
        // - Eviction: A chunk can be evicted when it has no references OR
        //   all its referencing chunk_ranges are "unlocked"
        //
        // Chunk Range Request lifecycle:
        // - States: Requesting → Provisioned (Locked/Unlocked) → Expired
        // - Expiry: Each request gets a TTL (in blocks). New requests for the
        //   same range reset the TTL (expiry height).
        // - Expiration paths:
        //   • Requesting → Expired (never provisioned)
        //   • Provisioned (Unlocked) → Expired (normal expiry)
        //   • Provisioned (Locked) → cannot expire (must unlock first)
        // - Cleanup: When expired, the chunk_range_id is removed from all
        //   referenced chunks
    }

    fn handle_lock_chunks(&self, chunk_range_id: &ChunkRangeId) {
        info!("lock chunks for id:{}", chunk_range_id);
    }

    fn handle_unlock_chunks(&self, chunk_range_id: &ChunkRangeId) {
        info!("unlock chunks for id:{}", chunk_range_id);
    }
}

impl PDService {
    #[tracing::instrument(skip_all)]
    pub fn spawn_service(
        rx: mpsc::UnboundedReceiver<PDServiceMessage>,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        info!("Spawning PD service");

        let (shutdown_tx, shutdown_rx) = reth::tasks::shutdown::signal();

        let handle = runtime_handle.spawn(
            async move {
                let service = Self {
                    shutdown: shutdown_rx,
                    msg_rx: rx,
                    inner: PDServiceInner::new(),
                };
                service
                    .start()
                    .await
                    .expect("PD service encountered an irrecoverable error")
            }
            .instrument(tracing::Span::current()),
        );

        TokioServiceHandle {
            name: "pd_service_handle".to_string(),
            handle,
            shutdown_signal: shutdown_tx,
        }
    }

    #[tracing::instrument(skip_all)]
    async fn start(mut self) -> eyre::Result<()> {
        info!("Starting PD service");

        loop {
            tokio::select! {
                biased; // enable bias so polling happens in definition order

                // Check for shutdown signal
                _ = &mut self.shutdown => {
                    info!("Shutdown signal received for PD service");
                    break;
                },
                // Handle messages
                cmd = self.msg_rx.recv() => {
                    match cmd {
                        Some(cmd) => {
                            match cmd {
                                PDServiceMessage::ProvisionChunks(chunk_range_specifier) => {
                                    self.inner.handle_provision_chunks(&chunk_range_specifier)
                                },
                                PDServiceMessage::LockChunks(chunk_range_id) => {
                                    self.inner.handle_lock_chunks(&chunk_range_id)
                                },
                                PDServiceMessage::UnlockChunks(chunk_range_id) => {
                                    self.inner.handle_unlock_chunks(&chunk_range_id)
                                },
                            }
                        }
                        None => {
                            warn!("Command channel closed unexpectedly");
                            break;
                        }
                    }
                },
            }
        }

        info!("Shutting down PD service gracefully");
        Ok(())
    }
}
