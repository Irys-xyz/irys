use crate::block_pool::{AdvisoryBlockPoolError, BlockPoolError, CriticalBlockPoolError};
use irys_actors::{
    chunk_ingress_service::IngressProofError, mempool_service::TxIngressError,
    AdvisoryChunkIngressError, ChunkIngressError,
};
use irys_types::{CommitmentValidationError, PeerNetworkError};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use thiserror::Error;

#[derive(Debug, Error, Clone)]
pub enum GossipError {
    #[error("Network error: {0}")]
    Network(String),
    #[error("Circuit breaker open for peer {0}")]
    CircuitBreakerOpen(irys_types::IrysPeerId),
    #[error("Invalid peer: {0}")]
    InvalidPeer(String),
    #[error("Cache error: {0}")]
    Cache(String),
    #[error("Internal error: {0}")]
    Internal(InternalGossipError),
    #[error("Invalid data: {0}")]
    InvalidData(InvalidDataError),
    #[error("Block pool error: {0:?}")]
    BlockPool(CriticalBlockPoolError),
    #[error("Transaction has already been handled")]
    TransactionIsAlreadyHandled,
    #[error("Commitment validation error: {0}")]
    CommitmentValidation(#[from] CommitmentValidationError),
    #[error(transparent)]
    PeerNetwork(PeerNetworkError),
    #[error("Rate limited: too many requests")]
    RateLimited,
    #[error("Advisory error: {0}")]
    Advisory(AdvisoryGossipError),
}

impl From<InternalGossipError> for GossipError {
    fn from(value: InternalGossipError) -> Self {
        Self::Internal(value)
    }
}

impl From<IngressProofError> for GossipError {
    fn from(value: IngressProofError) -> Self {
        match value {
            IngressProofError::InvalidSignature => {
                Self::InvalidData(InvalidDataError::IngressProofSignature)
            }
            IngressProofError::DatabaseError(err) => {
                Self::Internal(InternalGossipError::Database(err))
            }
            IngressProofError::Other(error) => Self::Internal(InternalGossipError::Unknown(error)),
            IngressProofError::UnstakedAddress => {
                Self::Internal(InternalGossipError::Unknown("Unstaked Address".into()))
            }
            IngressProofError::InvalidAnchor(anchor) => {
                Self::InvalidData(InvalidDataError::IngressProofAnchor(anchor))
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
                Self::TransactionIsAlreadyHandled
            }
            // ==== External errors
            TxIngressError::InvalidSignature(address) => {
                // Invalid signature, decrease source reputation
                Self::InvalidData(InvalidDataError::TransactionSignature(address))
            }
            TxIngressError::Unfunded(tx_id) => {
                // Unfunded transaction, decrease source reputation
                Self::InvalidData(InvalidDataError::TransactionUnfunded(tx_id))
            }
            TxIngressError::InvalidAnchor(anchor) => {
                // Invalid anchor, decrease source reputation
                Self::InvalidData(InvalidDataError::TransactionAnchor(anchor))
            }
            TxIngressError::InvalidLedger(ledger_id) => {
                // Invalid ledger type, decrease source reputation
                Self::InvalidData(InvalidDataError::TransactionInvalidLedger(ledger_id))
            }
            // ==== Internal errors - shouldn't be communicated to outside
            TxIngressError::DatabaseError(err) => {
                Self::Internal(InternalGossipError::Database(err))
            }
            TxIngressError::ServiceUninitialized => {
                Self::Internal(InternalGossipError::ServiceUninitialized)
            }
            TxIngressError::Other(error) => Self::Internal(InternalGossipError::Unknown(error)),
            // todo: `CommitmentValidationError` should  probably be made into an external error
            TxIngressError::CommitmentValidationError(commitment_validation_error) => {
                Self::CommitmentValidation(commitment_validation_error)
            }
            TxIngressError::BalanceFetchError { address, reason } => {
                Self::Internal(InternalGossipError::Unknown(format!(
                    "Failed to fetch balance for {}: {}",
                    address, reason
                )))
            }
            TxIngressError::MempoolFull(reason) => {
                // Mempool at capacity - treat as internal/temporary issue
                Self::Internal(InternalGossipError::MempoolFull(reason))
            }
            TxIngressError::FundMisalignment(reason) => {
                Self::Internal(InternalGossipError::FundMisalignment(reason))
            }
            TxIngressError::InvalidVersion { version, minimum } => {
                Self::InvalidData(InvalidDataError::TransactionInvalidVersion { version, minimum })
            }
            TxIngressError::UpdateRewardAddressNotAllowed => {
                // UpdateRewardAddress not allowed before Borealis hardfork
                Self::InvalidData(InvalidDataError::TransactionCommitmentTypeNotAllowed)
            }
        }
    }
}

impl From<PeerNetworkError> for GossipError {
    fn from(value: PeerNetworkError) -> Self {
        Self::PeerNetwork(value)
    }
}

impl From<BlockPoolError> for GossipError {
    fn from(value: BlockPoolError) -> Self {
        match value {
            BlockPoolError::Critical(err) => Self::BlockPool(err),
            BlockPoolError::Advisory(err) => {
                Self::Advisory(AdvisoryGossipError::BlockPoolError(err))
            }
        }
    }
}

impl GossipError {
    pub fn unknown<T: ToString + ?Sized>(error: &T) -> Self {
        Self::Internal(InternalGossipError::Unknown(error.to_string()))
    }

    pub fn is_advisory(&self) -> bool {
        matches!(self, Self::Advisory(_))
    }
}

#[derive(Debug, Error, Clone)]
pub enum InvalidDataError {
    #[error("Invalid transaction signature for address {0}")]
    TransactionSignature(irys_types::IrysAddress),
    #[error("Invalid transaction anchor: {0}")]
    TransactionAnchor(irys_types::H256),
    #[error("Invalid or unsupported ledger ID: {0}")]
    TransactionInvalidLedger(u32),
    #[error("Transaction {0} unfunded")]
    TransactionUnfunded(irys_types::H256),
    #[error("Invalid chunk proof")]
    ChunkInvalidProof,
    #[error("Invalid chunk data hash")]
    ChunkInvalidDataHash,
    #[error("Invalid chunk size")]
    ChunkInvalidChunkSize,
    #[error("Invalid chunk data size")]
    ChunkInvalidDataSize,
    #[error("Invalid chunk tx_offset: {0}")]
    ChunkInvalidOffset(String),
    #[error("Invalid block: {0}")]
    InvalidBlock(String),
    #[error("Invalid block signature")]
    InvalidBlockSignature,
    #[error("Execution payload hash mismatch")]
    ExecutionPayloadHashMismatch,
    #[error("Invalid execution payload structure")]
    ExecutionPayloadInvalidStructure,
    #[error("Invalid ingress proof signature")]
    IngressProofSignature,
    #[error("Invalid ingress proof anchor: {0}")]
    IngressProofAnchor(irys_types::BlockHash),
    #[error("Block body transactions do not match the header")]
    BlockBodyTransactionsMismatch,
    #[error("Invalid transaction version {version}, minimum required is {minimum}")]
    TransactionInvalidVersion { version: u8, minimum: u8 },
    #[error("Commitment type not allowed before hardfork activation")]
    TransactionCommitmentTypeNotAllowed,
    #[error("PD chunk timestamp expired")]
    PdChunkTimestampExpired,
    #[error("PD chunk timestamp in the future")]
    PdChunkTimestampFuture,
    #[error("PD chunk invalid range specifier")]
    PdChunkInvalidRangeSpecifier,
}

#[derive(Debug, Error, Clone)]
pub enum InternalGossipError {
    #[error("Unknown internal error: {0}")]
    Unknown(String),
    #[error("Database error: {0}")]
    Database(String),
    #[error("Mempool is full")]
    MempoolFull(String),
    #[error("Fund misalignment")]
    FundMisalignment(String),
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
    #[error("Failed to ingress a chunk: {0:?}")]
    ChunkIngress(ChunkIngressError),
}

#[derive(Debug, Error, Clone)]
pub enum AdvisoryGossipError {
    #[error("Failed to ingress chunk: {0:?}")]
    ChunkIngress(AdvisoryChunkIngressError),
    #[error("Block pool failed to ingest the block: {0:?}")]
    BlockPoolError(AdvisoryBlockPoolError),
}

pub type GossipResult<T> = Result<T, GossipError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipResponse<T> {
    Accepted(T),
    Rejected(RejectionReason),
}

impl GossipResponse<()> {
    pub fn rejected_gossip_disabled() -> Self {
        Self::Rejected(RejectionReason::GossipDisabled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Copy)]
pub enum HandshakeRequirementReason {
    RequestOriginIsNotInThePeerList,
    RequestOriginDoesNotMatchExpected,
    MinerAddressIsUnknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, Copy)]
pub enum RejectionReason {
    HandshakeRequired(Option<HandshakeRequirementReason>),
    GossipDisabled,
    InvalidData,
    RateLimited,
    UnableToVerifyOrigin,
    InvalidCredentials,
    ProtocolMismatch,
    UnsupportedProtocolVersion(u32),
    UnsupportedFeature,
}

#[derive(Debug, Clone, Copy)]
pub enum GossipRoutes {
    Transaction,
    CommitmentTx,
    Chunk,
    Block,
    BlockBody,
    IngressProof,
    ExecutionPayload,
    GetData,
    PullData,
    Handshake,
    Health,
    StakeAndPledgeWhitelist,
    Info,
    PeerList,
    BlockIndex,
    Version,
    ProtocolVersion,
    PdChunk,
}

impl GossipRoutes {
    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            Self::Transaction => "/transaction",
            Self::CommitmentTx => "/commitment_tx",
            Self::Chunk => "/chunk",
            Self::Block => "/block",
            Self::BlockBody => "/block_body",
            Self::IngressProof => "/ingress_proof",
            Self::ExecutionPayload => "/execution_payload",
            Self::GetData => "/get_data",
            Self::PullData => "/pull_data",
            Self::Handshake => "/handshake",
            Self::Health => "/health",
            Self::StakeAndPledgeWhitelist => "/stake_and_pledge_whitelist",
            Self::Info => "/info",
            Self::PeerList => "/peer-list",
            Self::BlockIndex => "/block-index",
            Self::Version => "/version",
            Self::ProtocolVersion => "/protocol_version",
            Self::PdChunk => "/pd_chunks",
        }
    }
}

impl std::fmt::Display for GossipRoutes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}
