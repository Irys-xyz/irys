use crate::block_pool::BlockPoolError;
use crate::peer_list::PeerListFacadeError;
use irys_actors::mempool_service::TxIngressError;
use irys_types::{BlockHash, H256};
use reth::revm::primitives::B256;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use thiserror::Error;

#[derive(Debug, Error, Clone)]
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
            PeerListFacadeError::ServiceError(err) => {
                Self::Internal(InternalGossipError::Unknown(format!("{:?}", err)))
            }
        }
    }
}

impl From<InternalGossipError> for GossipError {
    fn from(value: InternalGossipError) -> Self {
        Self::Internal(value)
    }
}

impl From<TxIngressError> for GossipError {
    fn from(value: TxIngressError) -> Self {
        match value {
            // ==== Not really errors
            TxIngressError::Skipped => {
                // Not an invalid transaction - just skipped
                Self::TransactionIsAlreadyHandled
            }
            // ==== External errors
            TxIngressError::InvalidSignature => {
                // Invalid signature, decrease source reputation
                Self::InvalidData(InvalidDataError::TransactionSignature)
            }
            TxIngressError::Unfunded => {
                // Unfunded transaction, decrease source reputation
                Self::InvalidData(InvalidDataError::TransactionUnfunded)
            }
            TxIngressError::InvalidAnchor => {
                // Invalid anchor, decrease source reputation
                Self::InvalidData(InvalidDataError::TransactionAnchor)
            }
            // ==== Internal errors - shouldn't be communicated to outside
            TxIngressError::DatabaseError => Self::Internal(InternalGossipError::Database),
            TxIngressError::ServiceUninitialized => {
                Self::Internal(InternalGossipError::ServiceUninitialized)
            }
            TxIngressError::Other(error) => Self::Internal(InternalGossipError::Unknown(error)),
        }
    }
}

impl GossipError {
    pub fn unknown<T: ToString + ?Sized>(error: &T) -> Self {
        Self::Internal(InternalGossipError::Unknown(error.to_string()))
    }
}

#[derive(Debug, Error, Clone)]
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
    #[error("Invalid block signature")]
    InvalidBlockSignature,
    #[error("Execution payload hash mismatch")]
    ExecutionPayloadHashMismatch,
}

#[derive(Debug, Error, Clone)]
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
    #[error("Failed to perform repair task for reth payloads: {0}")]
    PayloadRepair(BlockPoolError),
}

pub type GossipResult<T> = Result<T, GossipError>;

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum GossipDataRequest {
    ExecutionPayload(B256),
    Block(BlockHash),
    Transaction(H256),
}

impl Debug for GossipDataRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Block(hash) => write!(f, "block {:?}", hash),
            Self::Transaction(hash) => {
                write!(f, "transaction {:?}", hash)
            }
            Self::ExecutionPayload(block_hash) => {
                write!(f, "execution payload for block {:?}", block_hash)
            }
        }
    }
}
