use dashmap::DashMap;
use irys_domain::StorageModule;
use irys_types::PartitionChunkRange;
use irys_types::{PackedChunk, UnpackedChunk};
use std::sync::{Arc, atomic::AtomicUsize};
use std::time::Duration;
use std::time::Instant;
use tokio::sync::{Semaphore, mpsc, oneshot};

use super::config::UnpackingConfig;
use super::errors::UnpackingError;
use super::{PackingError, PackingResult, config::PackingConfig};

// ------------------------------------------------------------------------
// PACKING
// ------------------------------------------------------------------------

/// A validated packing request for a specific storage module and chunk range
#[derive(Debug, Clone)]
pub struct PackingRequest {
    storage_module: Arc<StorageModule>,
    chunk_range: PartitionChunkRange,
}

impl PackingRequest {
    pub fn new(
        storage_module: Arc<StorageModule>,
        chunk_range: PartitionChunkRange,
    ) -> PackingResult<Self> {
        // Validate partition assignment exists
        storage_module
            .partition_assignment()
            .ok_or(PackingError::InvalidAssignment {
                sm_id: storage_module.id,
            })?;

        // Validate chunk range is within partition bounds
        let max_chunks = storage_module.config.consensus.num_chunks_in_partition;
        if *chunk_range.0.end() >= max_chunks as u32 {
            return Err(PackingError::InvalidRange {
                requested: chunk_range,
                max: max_chunks,
            });
        }

        Ok(Self {
            storage_module,
            chunk_range,
        })
    }

    pub fn storage_module(&self) -> &Arc<StorageModule> {
        &self.storage_module
    }

    pub fn chunk_range(&self) -> &PartitionChunkRange {
        &self.chunk_range
    }
}

/// Channel for sending/receiving packing requests per storage module
pub(super) type PackingSMChannel = (
    mpsc::Sender<PackingRequest>,
    Arc<tokio::sync::Mutex<mpsc::Receiver<PackingRequest>>>,
);

/// Collection of per-storage-module packing queues
pub type PackingQueues = Arc<DashMap<usize, PackingSMChannel>>;

/// Internal state exposed for monitoring and testing
#[derive(Debug, Clone)]
pub struct PackingInternals {
    pub pending_jobs: PackingQueues,
    pub semaphore: Arc<Semaphore>,
    pub active_workers: Arc<AtomicUsize>,
    pub config: PackingConfig,
}

/// Control messages for the packing service
#[derive(Debug)]
pub(crate) enum PackingServiceMessage {
    /// Request to wait until all packing is complete
    Drain { respond_to: oneshot::Sender<()> },
}

/// Handle for waiting for packing to become idle
#[derive(Debug, Clone)]
pub struct PackingIdleWaiter {
    packing_service_sender: mpsc::Sender<PackingServiceMessage>,
}

impl PackingIdleWaiter {
    pub(crate) fn new(sender: mpsc::Sender<PackingServiceMessage>) -> Self {
        Self {
            packing_service_sender: sender,
        }
    }

    #[tracing::instrument(level = "trace", skip_all, fields(packing.timeout_secs = timeout.map(|d| d.as_secs())))]
    pub async fn wait_for_idle(&self, timeout: Option<Duration>) -> eyre::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.packing_service_sender
            .send(PackingServiceMessage::Drain { respond_to: tx })
            .await
            .map_err(|e| eyre::eyre!("control channel closed: {}", e))?;

        tokio::time::timeout(timeout.unwrap_or(Duration::from_secs(10)), async move {
            rx.await
                .map_err(|e| eyre::eyre!("waiter dropped: {}", e))
                .map(|_| ())
        })
        .await
        .map_err(|_| eyre::eyre!("timed out waiting for packing to become idle"))??;

        Ok(())
    }
}

/// Handle for interacting with the packing service
#[derive(Debug, Clone)]
pub struct PackingHandle {
    sender: mpsc::Sender<PackingRequest>,
    internals: PackingInternals,
    packing_service_sender: mpsc::Sender<PackingServiceMessage>,
}

impl PackingHandle {
    pub(crate) fn new(
        sender: mpsc::Sender<PackingRequest>,
        internals: PackingInternals,
        packing_service_sender: mpsc::Sender<PackingServiceMessage>,
    ) -> Self {
        Self {
            sender,
            internals,
            packing_service_sender,
        }
    }

    /// Enqueue a packing request. Drops the request if the channel is full.
    pub fn send(&self, req: PackingRequest) -> Result<(), mpsc::error::SendError<PackingRequest>> {
        use mpsc::error::TrySendError;

        match self.sender.try_send(req) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                tracing::warn!(target: "irys::packing", "Dropping packing request due to saturated channel");
                Ok(())
            }
            Err(TrySendError::Closed(req)) => Err(mpsc::error::SendError(req)),
        }
    }

    /// Access a clone of the internals snapshot
    pub fn internals(&self) -> PackingInternals {
        self.internals.clone()
    }

    /// Access a clone of the underlying sender
    pub fn sender(&self) -> mpsc::Sender<PackingRequest> {
        self.sender.clone()
    }

    /// Create a waiter for idle detection
    pub fn waiter(&self) -> PackingIdleWaiter {
        PackingIdleWaiter::new(self.packing_service_sender.clone())
    }
}

// ------------------------------------------------------------------------
// UNPACKING
// ------------------------------------------------------------------------

pub type UnpackingQueue = DashMap<Instant, UnpackingRequest>;
/// Shared reference to packed chunks for concurrent unpacking operations.
pub type PackedChunks = Arc<Vec<PackedChunk>>;

/// Priority level for unpacking operations
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnpackingPriority {
    High,       // PoA, PD (execution-blocking)
    Background, // DataSync, EntropyPacking
}

/// A validated unpacking request
#[derive(Debug)]
pub struct UnpackingRequest {
    pub packed_chunks: PackedChunks,
    pub priority: UnpackingPriority,
    pub response_tx: oneshot::Sender<Result<UnpackedChunk, UnpackingError>>,
}

impl UnpackingRequest {
    /// Create a new unpacking request and return it with its response receiver
    pub fn new(
        packed_chunks: PackedChunks,
        priority: UnpackingPriority,
    ) -> (
        Self,
        oneshot::Receiver<Result<UnpackedChunk, UnpackingError>>,
    ) {
        let (response_tx, response_rx) = oneshot::channel();
        let request = Self {
            packed_chunks,
            priority,
            response_tx,
        };
        (request, response_rx)
    }

    /// Helper to create an unpacking request from a single chunk
    pub fn from_chunk(
        packed_chunk: PackedChunk,
        priority: UnpackingPriority,
    ) -> (
        Self,
        oneshot::Receiver<Result<UnpackedChunk, UnpackingError>>,
    ) {
        Self::new(Arc::new(vec![packed_chunk]), priority)
    }
}

/// Internal state exposed for monitoring and testing
#[derive(Debug, Clone)]
pub struct UnpackingInternals {
    pub config: UnpackingConfig,
}
/// Handle for interacting with the unpacking service
#[derive(Debug, Clone)]
pub struct UnpackingHandle {
    sender: mpsc::Sender<UnpackingRequest>,
    internals: UnpackingInternals,
}

impl UnpackingHandle {
    pub(crate) fn new(
        sender: mpsc::Sender<UnpackingRequest>,
        internals: UnpackingInternals,
    ) -> Self {
        Self { sender, internals }
    }

    /// Enqueue an unpacking request. Applies backpressure if the channel is full.
    pub async fn send(
        &self,
        req: UnpackingRequest,
    ) -> Result<(), mpsc::error::SendError<UnpackingRequest>> {
        self.sender.send(req).await
    }

    /// Access a clone of the internals snapshot
    pub fn internals(&self) -> UnpackingInternals {
        self.internals.clone()
    }

    /// Access a clone of the underlying sender
    pub fn sender(&self) -> mpsc::Sender<UnpackingRequest> {
        self.sender.clone()
    }
}
