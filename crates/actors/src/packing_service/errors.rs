use thiserror::Error;

/// Errors that can occur during unpacking
#[derive(Debug, Error)]
pub enum UnpackingError {
    #[error("Channel closed")]
    ChannelClosed,

    #[error("Unpacking failed: {0}")]
    UnpackingFailed(String),

    #[error("Unpacking batch timed out after {0:?}")]
    Timeout(std::time::Duration),

    #[error("Thread pool creation failed: {0}")]
    ThreadPoolCreation(String),
}

#[derive(Error, Debug)]
pub enum PackingError {
    #[error("Invalid partition assignment for SM {sm_id}")]
    InvalidAssignment { sm_id: usize },

    #[error("Invalid chunk range: {requested:?} exceeds max {max}")]
    InvalidRange {
        requested: irys_types::PartitionChunkRange,
        max: u64,
    },

    #[error("Failed to build HTTP client: {0}")]
    ClientBuildFailed(String),

    #[error("Semaphore acquisition failed (channel closed)")]
    SemaphoreAcquisitionFailed,

    #[error("Invalid remote response size: expected {expected}, got {actual}")]
    InvalidResponseSize { expected: usize, actual: usize },

    #[error("Buffer size conversion failed: {0}")]
    BufferSizeConversion(String),

    #[error("Chunk size conversion failed: {0}")]
    ChunkSizeConversion(String),

    #[error("Failed to get current runtime handle")]
    RuntimeHandleUnavailable,

    #[error("Failed to convert value: {0}")]
    ConversionFailed(String),
}
