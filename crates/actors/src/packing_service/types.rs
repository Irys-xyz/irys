use std::sync::{Arc, atomic::AtomicUsize};
use std::time::Duration;

use dashmap::DashMap;
use irys_domain::StorageModule;
use irys_types::{PartitionChunkOffset, PartitionChunkRange, ii};
use tokio::sync::{Semaphore, mpsc, oneshot};

use super::{PackingError, PackingResult, config::PackingConfig};

/// A validated packing request for a specific storage module and chunk range
#[derive(Debug, Clone)]
pub struct PackingRequest {
    storage_module: Arc<StorageModule>,
    chunk_range: PartitionChunkRange,
}

impl PackingRequest {
    /// Create a new validated packing request
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

        // Clamp the inclusive end to the logical partition size rather than
        // reject: the logical size can shrink between a caller's own clamp
        // (e.g. `StorageModule::reset`) and this check when the tip moves onto E.
        let max_chunks = storage_module.partition_chunks();
        let start = chunk_range.0.start();
        let end = chunk_range.0.end().min(PartitionChunkOffset::from(
            max_chunks.saturating_sub(1) as u32
        ));
        if start > end {
            return Err(PackingError::InvalidRange {
                requested: chunk_range,
                max: max_chunks,
            });
        }

        Ok(Self {
            storage_module,
            chunk_range: PartitionChunkRange(ii(start, end)),
        })
    }

    /// Access storage module
    pub fn storage_module(&self) -> &Arc<StorageModule> {
        &self.storage_module
    }

    /// Access chunk range
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

/// Semaphore for limiting concurrent packing operations
pub type PackingSemaphore = Arc<Semaphore>;

/// Channel for sending packing requests
pub type PackingSender = mpsc::Sender<PackingRequest>;

/// Channel for receiving packing requests
pub type PackingReceiver = mpsc::Receiver<PackingRequest>;

/// Internal state exposed for monitoring and testing
#[derive(Debug, Clone)]
pub struct Internals {
    pub pending_jobs: PackingQueues,
    pub semaphore: PackingSemaphore,
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
    sender: PackingSender,
    internals: Internals,
    packing_service_sender: mpsc::Sender<PackingServiceMessage>,
}

impl PackingHandle {
    pub(crate) fn new(
        sender: PackingSender,
        internals: Internals,
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
    pub fn internals(&self) -> Internals {
        self.internals.clone()
    }

    /// Access a clone of the underlying sender
    pub fn sender(&self) -> PackingSender {
        self.sender.clone()
    }

    /// Create a waiter for idle detection
    pub fn waiter(&self) -> PackingIdleWaiter {
        PackingIdleWaiter::new(self.packing_service_sender.clone())
    }
}
