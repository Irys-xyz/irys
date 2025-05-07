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
        TxIngressError::Other(error) => {
            Some(GossipError::Internal(InternalGossipError::Unknown(error)))
        }
    }
}

pub type GossipResult<T> = Result<T, GossipError>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum GossipDataRequest {
    Block(BlockHash),
    Transaction(H256),
}
