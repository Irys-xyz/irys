use irys_actors::mempool_service::TxIngressError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GossipError {
    #[error("Network error: {0}")]
    Network(String),
    #[error("Invalid peer: {0}")]
    InvalidPeer(String),
    #[error("Cache error: {0}")]
    Cache(String),
    #[error("Internal error: {0}")]
    Internal(InternalGossipError),
    #[error("Invalid data: {0}")]
    InvalidData(InvalidDataError),
}

impl GossipError {
    pub fn unknown(e: impl ToString) -> Self {
        GossipError::Internal(InternalGossipError::Unknown(e.to_string()))
    }
}

#[derive(Debug, Error)]
pub enum InvalidDataError {
    #[error("Invalid transaction signature")]
    TransactionSignature,
    #[error("Invalid transaction anchor")]
    TransactionAnchor,
    #[error("Transaction unfunded")]
    TransactionUnfunded,
    #[error("Invalid chunk proof")]
    ChunkInvalidProof,
    #[error("Invalid chunk data hash")]
    ChinkInvalidDataHash,
    #[error("Invalid chunk size")]
    ChunkInvalidChunkSize,
    #[error("Invalid block: {0}")]
    InvalidBlock(String),
}

#[derive(Debug, Error)]
pub enum InternalGossipError {
    #[error("Unknown internal error: {0}")]
    Unknown(String),
    #[error("Database error")]
    Database,
    #[error("Service uninitialized")]
    ServiceUninitialized,
    #[error("Cache cleanup error")]
    CacheCleanup(String),
    #[error("Server already running")]
    ServerAlreadyRunning,
    #[error("Broadcast receiver has been already shutdown")]
    BroadcastReceiverShutdown,
}

pub(crate) fn tx_ingress_error_to_gossip_error(error: TxIngressError) -> Option<GossipError> {
    match error {
        TxIngressError::Skipped => None,
        TxIngressError::InvalidSignature => Some(GossipError::InvalidData(
            InvalidDataError::TransactionSignature,
        )),
        TxIngressError::Unfunded => Some(GossipError::InvalidData(
            InvalidDataError::TransactionUnfunded,
        )),
        TxIngressError::InvalidAnchor => Some(GossipError::InvalidData(
            InvalidDataError::TransactionAnchor,
        )),
        TxIngressError::DatabaseError => Some(GossipError::Internal(InternalGossipError::Database)),
        TxIngressError::ServiceUninitialized => Some(GossipError::Internal(
            InternalGossipError::ServiceUninitialized,
        )),
        TxIngressError::Other(e) => Some(GossipError::Internal(InternalGossipError::Unknown(e))),
    }
}

pub type GossipResult<T> = Result<T, GossipError>;
