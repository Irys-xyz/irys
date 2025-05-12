use crate::block_pool_service::BlockPoolError;
use irys_actors::mempool_service::TxIngressError;
use irys_actors::peer_list_service::PeerListFacadeError;
use irys_types::{BlockHash, H256};
use serde::{Deserialize, Serialize};
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
    #[error("Block pool error: {0:?}")]
    BlockPool(BlockPoolError),
    #[error("Transaction has already been handled")]
    TransactionIsAlreadyHandled,
}

impl From<PeerListFacadeError> for GossipError {
    fn from(error: PeerListFacadeError) -> Self {
        match error {
            PeerListFacadeError::InternalError(err) => {
                Self::Internal(InternalGossipError::Unknown(err))
            }
        }
    }
}

impl From<TxIngressError> for GossipError {
    fn from(value: TxIngressError) -> Self {
        match value {
            // ==== Not really errors
            TxIngressError::Skipped => {
                // Not an invalid transaction - just skipped
                GossipError::TransactionIsAlreadyHandled
            }
            // ==== External errors
            TxIngressError::InvalidSignature => {
                // Invalid signature, decrease source reputation
                GossipError::InvalidData(InvalidDataError::TransactionSignature)
            }
            TxIngressError::Unfunded => {
                // Unfunded transaction, decrease source reputation
                GossipError::InvalidData(InvalidDataError::TransactionUnfunded)
            }
            TxIngressError::InvalidAnchor => {
                // Invalid anchor, decrease source reputation
                GossipError::InvalidData(InvalidDataError::TransactionAnchor)
            }
            // ==== Internal errors - shouldn't be communicated to outside
            TxIngressError::DatabaseError => GossipError::Internal(InternalGossipError::Database),
            TxIngressError::ServiceUninitialized => {
                GossipError::Internal(InternalGossipError::ServiceUninitialized)
            }
            TxIngressError::Other(error) => {
                GossipError::Internal(InternalGossipError::Unknown(error))
            }
        }
    }
}

impl GossipError {
    pub fn unknown<T: ToString + ?Sized>(error: &T) -> Self {
        Self::Internal(InternalGossipError::Unknown(error.to_string()))
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
    #[error("Invalid chunk data size")]
    ChunkInvalidDataSize,
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
    #[error("Trying to shutdown a server that is already shutdown: {0}")]
    AlreadyShutdown(String),
}

pub type GossipResult<T> = Result<T, GossipError>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GossipDataRequest {
    Block(BlockHash),
    Transaction(H256),
}
