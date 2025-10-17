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
}
