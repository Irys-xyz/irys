use crate::block_pool::{AdvisoryBlockPoolError, BlockPoolError, CriticalBlockPoolError};
use irys_actors::{
    AdvisoryChunkIngressError, ChunkIngressError, CriticalChunkIngressError,
    chunk_ingress_service::IngressProofError, mempool_service::TxIngressError,
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
            // Overloaded is a retryable backpressure signal — use the
            // dedicated `RateLimited` variant so metrics/dashboards classify
            // it correctly instead of polluting `Internal` error counts.
            IngressProofError::Overloaded => Self::RateLimited,
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

impl From<ChunkIngressError> for GossipError {
    fn from(value: ChunkIngressError) -> Self {
        match value {
            ChunkIngressError::Critical(err) => match err {
                CriticalChunkIngressError::InvalidProof => {
                    Self::InvalidData(InvalidDataError::ChunkInvalidProof)
                }
                CriticalChunkIngressError::InvalidDataHash => {
                    Self::InvalidData(InvalidDataError::ChunkInvalidDataHash)
                }
                CriticalChunkIngressError::InvalidChunkSize => {
                    Self::InvalidData(InvalidDataError::ChunkInvalidChunkSize)
                }
                CriticalChunkIngressError::InvalidDataSize => {
                    Self::InvalidData(InvalidDataError::ChunkInvalidDataSize)
                }
                CriticalChunkIngressError::InvalidOffset(msg) => {
                    Self::InvalidData(InvalidDataError::ChunkInvalidOffset(msg))
                }
                CriticalChunkIngressError::DatabaseError => Self::Internal(
                    InternalGossipError::Database("Chunk ingress database error".into()),
                ),
                CriticalChunkIngressError::ServiceUninitialized => {
                    Self::Internal(InternalGossipError::ServiceUninitialized)
                }
                CriticalChunkIngressError::Other(other) => {
                    Self::Internal(InternalGossipError::Unknown(other))
                }
            },
            ChunkIngressError::Advisory(err) => match err {
                // Saturation is a retryable backpressure signal — map it to
                // the dedicated `RateLimited` variant (mirrors
                // `IngressProofError::Overloaded`). Without this the gossip
                // POST handlers collapse it into `InvalidData`, hiding the
                // retry signal and penalising the peer for our own
                // backpressure.
                AdvisoryChunkIngressError::Overloaded => Self::RateLimited,
                other => Self::Advisory(AdvisoryGossipError::ChunkIngress(other)),
            },
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
            TxIngressError::ZeroDataSize(tx_id) => {
                // Structurally invalid tx (no data), decrease source reputation
                Self::InvalidData(InvalidDataError::TransactionZeroDataSize(tx_id))
            }
            TxIngressError::PrefixSizeExceedsDataSize(tx_id) => {
                // Structurally invalid tx (prefix_size > data_size), decrease source reputation
                Self::InvalidData(InvalidDataError::TransactionPrefixSizeExceedsDataSize(
                    tx_id,
                ))
            }
            TxIngressError::ChainIdMismatch { tx_id, .. } => {
                // Tx signed for another chain, decrease source reputation
                Self::InvalidData(InvalidDataError::TransactionChainIdMismatch(tx_id))
            }
            TxIngressError::DataSizeExceedsMax { tx_id, .. } => {
                // Structurally invalid tx (exceeds max data size), decrease source reputation
                Self::InvalidData(InvalidDataError::TransactionDataSizeExceedsMax(tx_id))
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
    #[error("Transaction {0} has zero data_size")]
    TransactionZeroDataSize(irys_types::H256),
    #[error("Transaction {0} has prefix_size greater than data_size")]
    TransactionPrefixSizeExceedsDataSize(irys_types::H256),
    #[error("Transaction {0} has a foreign chain_id")]
    TransactionChainIdMismatch(irys_types::H256),
    #[error("Transaction {0} exceeds the maximum data size")]
    TransactionDataSizeExceedsMax(irys_types::H256),
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

/// Wire type — serialized directly in gossip HTTP responses.
/// Adding a variant? Add a fixture entry in `gossip_fixture_tests.rs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipResponse<T> {
    Accepted(T),
    Rejected(RejectionReason),
}

/// JSON body for a v2 gossip rejection. Same `{"Rejected": ...}` envelope
/// as [`GossipResponse::<T>::Rejected`], but [`RejectionReason::HandshakeRequired`]
/// keeps its `Option<HandshakeRequirementReason>` payload on the wire as a
/// newtype-variant object (e.g. `{"HandshakeRequired": "MinerAddressIsUnknown"}`)
/// instead of being flattened to the bare unit string by the default v1-compat
/// [`RejectionReason`] [`Serialize`] impl.
///
/// Used by v2 server handlers responding to peers that came in on a
/// `/gossip/v2/*` route — v2 peers run code whose [`RejectionReason`]
/// [`Deserialize`] accepts both wire shapes, so they recover the diagnostic.
/// v1 handlers and shared (no-version-prefix) routes keep using
/// [`GossipResponse::<T>::Rejected`] directly so older peers — which only
/// know the unit form — can still parse the response.
pub(crate) fn rejected_v2_json(reason: RejectionReason) -> serde_json::Value {
    let inner = match reason {
        // The only variant whose v2 wire form differs from v1: keep the
        // `Option<HandshakeRequirementReason>` payload visible on the wire.
        RejectionReason::HandshakeRequired(payload) => {
            serde_json::json!({ "HandshakeRequired": payload })
        }
        // All other variants serialize identically in v1 and v2.
        other => serde_json::to_value(other)
            .expect("RejectionReason serialize is infallible for in-memory values"),
    };
    serde_json::json!({ "Rejected": inner })
}

/// Wire type — serialized as part of [`RejectionReason::HandshakeRequired`].
/// Adding a variant? Add a fixture entry in `gossip_fixture_tests.rs`.
#[derive(Debug, Clone, Serialize, Deserialize, Copy)]
pub enum HandshakeRequirementReason {
    RequestOriginIsNotInThePeerList,
    RequestOriginDoesNotMatchExpected,
    MinerAddressIsUnknown,
}

/// Wire type — serialized as part of [`GossipResponse::Rejected`].
/// Adding a variant? Add a fixture entry in `gossip_fixture_tests.rs`.
///
/// Two wire forms exist depending on which envelope is used:
///
/// * **v1-compatible** (default `Serialize` impl, used by [`GossipResponse`]
///   and emitted from `/gossip/*` v1 routes plus shared no-version-prefix
///   routes): every variant serializes as a bare unit string
///   (e.g. `"HandshakeRequired"`) — except [`Self::UnsupportedProtocolVersion`],
///   which keeps its newtype shape because the version number is load-bearing.
///   The `Option<HandshakeRequirementReason>` payload on
///   [`Self::HandshakeRequired`] is dropped on serialize so mainnet v1 nodes
///   (which only know the unit form) can still parse our responses.
///
/// * **v2** ([`rejected_v2_json`], used from `/gossip/v2/*` handlers via
///   [`crate::server::GossipServer::check_peer_v2`]): same outer envelope,
///   but [`Self::HandshakeRequired`] keeps its
///   `Option<HandshakeRequirementReason>` payload as a newtype-variant object
///   (e.g. `{"HandshakeRequired": "MinerAddressIsUnknown"}`). v2 peers can
///   parse this richer form via the [`Deserialize`] impl below.
///
/// On the receive side the [`Deserialize`] impl accepts *both* shapes — the
/// unit string from v1 producers, and the newtype-variant object from v2
/// producers (or future versions that serialize differently).
#[derive(Debug, Clone, Copy)]
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
    /// The requesting peer is on a different chain (`chain_id` mismatch). Unlike
    /// the advisory consensus-config-hash mismatch, this is a hard handshake
    /// rejection — we never peer with a node on a different chain.
    ChainIdMismatch,
}

const REJECTION_REASON_VARIANTS: &[&str] = &[
    "HandshakeRequired",
    "GossipDisabled",
    "InvalidData",
    "RateLimited",
    "UnableToVerifyOrigin",
    "InvalidCredentials",
    "ProtocolMismatch",
    "UnsupportedProtocolVersion",
    "UnsupportedFeature",
    "ChainIdMismatch",
];

impl Serialize for RejectionReason {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            // v1-compat: drop the diagnostic `Option<reason>` and emit just
            // the unit string. v1 peers only know the unit form, so this
            // keeps NEW→OLD responses parseable. Local code paths that need
            // the reason can read it directly from the in-memory value
            // before it's serialized.
            Self::HandshakeRequired(_) => {
                serializer.serialize_unit_variant("RejectionReason", 0, "HandshakeRequired")
            }
            Self::GossipDisabled => {
                serializer.serialize_unit_variant("RejectionReason", 1, "GossipDisabled")
            }
            Self::InvalidData => {
                serializer.serialize_unit_variant("RejectionReason", 2, "InvalidData")
            }
            Self::RateLimited => {
                serializer.serialize_unit_variant("RejectionReason", 3, "RateLimited")
            }
            Self::UnableToVerifyOrigin => {
                serializer.serialize_unit_variant("RejectionReason", 4, "UnableToVerifyOrigin")
            }
            Self::InvalidCredentials => {
                serializer.serialize_unit_variant("RejectionReason", 5, "InvalidCredentials")
            }
            Self::ProtocolMismatch => {
                serializer.serialize_unit_variant("RejectionReason", 6, "ProtocolMismatch")
            }
            // The version number IS load-bearing — without it the peer can't
            // tell what version we expected — so we keep the newtype shape.
            // v1 peers will reject this as unknown, which is the correct
            // outcome (they didn't understand the negotiation anyway).
            Self::UnsupportedProtocolVersion(n) => serializer.serialize_newtype_variant(
                "RejectionReason",
                7,
                "UnsupportedProtocolVersion",
                n,
            ),
            Self::UnsupportedFeature => {
                serializer.serialize_unit_variant("RejectionReason", 8, "UnsupportedFeature")
            }
            Self::ChainIdMismatch => {
                serializer.serialize_unit_variant("RejectionReason", 9, "ChainIdMismatch")
            }
        }
    }
}

impl<'de> Deserialize<'de> for RejectionReason {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = RejectionReason;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(
                    "a RejectionReason variant: either a unit string \
                     (v1 wire form) or a single-key object (newtype form)",
                )
            }

            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<RejectionReason, E> {
                match s {
                    "HandshakeRequired" => Ok(RejectionReason::HandshakeRequired(None)),
                    "GossipDisabled" => Ok(RejectionReason::GossipDisabled),
                    "InvalidData" => Ok(RejectionReason::InvalidData),
                    "RateLimited" => Ok(RejectionReason::RateLimited),
                    "UnableToVerifyOrigin" => Ok(RejectionReason::UnableToVerifyOrigin),
                    "InvalidCredentials" => Ok(RejectionReason::InvalidCredentials),
                    "ProtocolMismatch" => Ok(RejectionReason::ProtocolMismatch),
                    "UnsupportedFeature" => Ok(RejectionReason::UnsupportedFeature),
                    "ChainIdMismatch" => Ok(RejectionReason::ChainIdMismatch),
                    other => Err(E::unknown_variant(other, REJECTION_REASON_VARIANTS)),
                }
            }

            fn visit_string<E: serde::de::Error>(self, s: String) -> Result<RejectionReason, E> {
                self.visit_str(&s)
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<RejectionReason, A::Error> {
                let key: String = map.next_key()?.ok_or_else(|| {
                    serde::de::Error::custom("RejectionReason object must have a single key")
                })?;
                let value = match key.as_str() {
                    "HandshakeRequired" => {
                        let reason: Option<HandshakeRequirementReason> = map.next_value()?;
                        RejectionReason::HandshakeRequired(reason)
                    }
                    "UnsupportedProtocolVersion" => {
                        let n: u32 = map.next_value()?;
                        RejectionReason::UnsupportedProtocolVersion(n)
                    }
                    // Older peers may have serialized these as objects when
                    // they were derived; tolerate that on input even though
                    // we never emit it.
                    "GossipDisabled" => {
                        map.next_value::<serde::de::IgnoredAny>()?;
                        RejectionReason::GossipDisabled
                    }
                    "InvalidData" => {
                        map.next_value::<serde::de::IgnoredAny>()?;
                        RejectionReason::InvalidData
                    }
                    "RateLimited" => {
                        map.next_value::<serde::de::IgnoredAny>()?;
                        RejectionReason::RateLimited
                    }
                    "UnableToVerifyOrigin" => {
                        map.next_value::<serde::de::IgnoredAny>()?;
                        RejectionReason::UnableToVerifyOrigin
                    }
                    "InvalidCredentials" => {
                        map.next_value::<serde::de::IgnoredAny>()?;
                        RejectionReason::InvalidCredentials
                    }
                    "ProtocolMismatch" => {
                        map.next_value::<serde::de::IgnoredAny>()?;
                        RejectionReason::ProtocolMismatch
                    }
                    "UnsupportedFeature" => {
                        map.next_value::<serde::de::IgnoredAny>()?;
                        RejectionReason::UnsupportedFeature
                    }
                    "ChainIdMismatch" => {
                        map.next_value::<serde::de::IgnoredAny>()?;
                        RejectionReason::ChainIdMismatch
                    }
                    other => {
                        return Err(serde::de::Error::unknown_variant(
                            other,
                            REJECTION_REASON_VARIANTS,
                        ));
                    }
                };
                if map.next_key::<String>()?.is_some() {
                    return Err(serde::de::Error::custom(
                        "RejectionReason object must have exactly one key",
                    ));
                }
                Ok(value)
            }
        }

        deserializer.deserialize_any(V)
    }
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
        }
    }
}

impl std::fmt::Display for GossipRoutes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rstest::rstest;

    #[rstest]
    #[case(GossipRoutes::Transaction, "/transaction")]
    #[case(GossipRoutes::CommitmentTx, "/commitment_tx")]
    #[case(GossipRoutes::Chunk, "/chunk")]
    #[case(GossipRoutes::Block, "/block")]
    #[case(GossipRoutes::BlockBody, "/block_body")]
    #[case(GossipRoutes::IngressProof, "/ingress_proof")]
    #[case(GossipRoutes::ExecutionPayload, "/execution_payload")]
    #[case(GossipRoutes::GetData, "/get_data")]
    #[case(GossipRoutes::PullData, "/pull_data")]
    #[case(GossipRoutes::Handshake, "/handshake")]
    #[case(GossipRoutes::Health, "/health")]
    #[case(GossipRoutes::StakeAndPledgeWhitelist, "/stake_and_pledge_whitelist")]
    #[case(GossipRoutes::Info, "/info")]
    #[case(GossipRoutes::PeerList, "/peer-list")]
    #[case(GossipRoutes::BlockIndex, "/block-index")]
    #[case(GossipRoutes::Version, "/version")]
    #[case(GossipRoutes::ProtocolVersion, "/protocol_version")]
    fn route_as_str_and_display(#[case] route: GossipRoutes, #[case] expected: &str) {
        assert_eq!(route.as_str(), expected);
        assert_eq!(format!("{route}"), expected);
    }

    fn arb_rejection_reason() -> impl Strategy<Value = RejectionReason> {
        prop_oneof![
            Just(RejectionReason::HandshakeRequired(None)),
            Just(RejectionReason::HandshakeRequired(Some(
                HandshakeRequirementReason::RequestOriginIsNotInThePeerList
            ))),
            Just(RejectionReason::HandshakeRequired(Some(
                HandshakeRequirementReason::RequestOriginDoesNotMatchExpected
            ))),
            Just(RejectionReason::HandshakeRequired(Some(
                HandshakeRequirementReason::MinerAddressIsUnknown
            ))),
            Just(RejectionReason::GossipDisabled),
            Just(RejectionReason::InvalidData),
            Just(RejectionReason::RateLimited),
            Just(RejectionReason::UnableToVerifyOrigin),
            Just(RejectionReason::InvalidCredentials),
            Just(RejectionReason::ProtocolMismatch),
            any::<u32>().prop_map(RejectionReason::UnsupportedProtocolVersion),
            Just(RejectionReason::UnsupportedFeature),
            Just(RejectionReason::ChainIdMismatch),
        ]
    }

    proptest! {
        #[test]
        fn gossip_response_accepted_json_roundtrip(value: u64) {
            let resp = GossipResponse::Accepted(value);
            let json = serde_json::to_string(&resp).unwrap();
            let decoded: GossipResponse<u64> = serde_json::from_str(&json).unwrap();
            let json2 = serde_json::to_string(&decoded).unwrap();
            prop_assert_eq!(json, json2);
        }

        /// Asserts each [`RejectionReason`] serializes to exactly the wire form
        /// the v1-compat shim documents — bare unit string for every variant
        /// except `UnsupportedProtocolVersion`, which keeps its newtype shape
        /// because the version number is load-bearing. A pure JSON-stable
        /// round-trip (`json == json2`) silently passes if a payload-bearing
        /// variant accidentally re-enables on serialize, so this asserts the
        /// shape directly.
        #[test]
        fn rejection_reason_serializes_to_v1_wire_form(reason in arb_rejection_reason()) {
            let json = serde_json::to_value(GossipResponse::<u64>::Rejected(reason)).unwrap();
            let inner = json.get("Rejected").expect("Rejected envelope");
            let expected = match reason {
                RejectionReason::HandshakeRequired(_) => serde_json::json!("HandshakeRequired"),
                RejectionReason::GossipDisabled => serde_json::json!("GossipDisabled"),
                RejectionReason::InvalidData => serde_json::json!("InvalidData"),
                RejectionReason::RateLimited => serde_json::json!("RateLimited"),
                RejectionReason::UnableToVerifyOrigin => serde_json::json!("UnableToVerifyOrigin"),
                RejectionReason::InvalidCredentials => serde_json::json!("InvalidCredentials"),
                RejectionReason::ProtocolMismatch => serde_json::json!("ProtocolMismatch"),
                RejectionReason::UnsupportedFeature => serde_json::json!("UnsupportedFeature"),
                RejectionReason::ChainIdMismatch => serde_json::json!("ChainIdMismatch"),
                RejectionReason::UnsupportedProtocolVersion(n) => {
                    serde_json::json!({ "UnsupportedProtocolVersion": n })
                }
            };
            prop_assert_eq!(inner, &expected);
        }
    }

    /// Object-form deserialize: `{"HandshakeRequired": "<reason>"}` parses to
    /// the matching `Some(reason)`. Covers the input direction the v1-compat
    /// shim must keep working — the serialize side drops the payload, but old
    /// peers that emit it (or future versions that re-emit it) must still parse.
    #[test]
    fn deserialize_handshake_required_object_form() {
        let cases = [
            (
                "RequestOriginIsNotInThePeerList",
                HandshakeRequirementReason::RequestOriginIsNotInThePeerList,
            ),
            (
                "RequestOriginDoesNotMatchExpected",
                HandshakeRequirementReason::RequestOriginDoesNotMatchExpected,
            ),
            (
                "MinerAddressIsUnknown",
                HandshakeRequirementReason::MinerAddressIsUnknown,
            ),
        ];
        for (wire_name, expected) in cases {
            let json = format!(r#"{{"HandshakeRequired":"{wire_name}"}}"#);
            let parsed: RejectionReason = serde_json::from_str(&json).unwrap();
            match parsed {
                RejectionReason::HandshakeRequired(Some(reason)) => {
                    assert!(
                        std::mem::discriminant(&reason) == std::mem::discriminant(&expected),
                        "expected {expected:?}, got {reason:?}",
                    );
                }
                other => panic!("expected HandshakeRequired(Some(_)), got {other:?}"),
            }
        }

        // Bare unit string (v1 wire form) parses to `None`.
        let parsed: RejectionReason = serde_json::from_str(r#""HandshakeRequired""#).unwrap();
        assert!(matches!(parsed, RejectionReason::HandshakeRequired(None)));

        // Object with explicit null reason — also a legitimate v2 wire form.
        let parsed: RejectionReason =
            serde_json::from_str(r#"{"HandshakeRequired":null}"#).unwrap();
        assert!(matches!(parsed, RejectionReason::HandshakeRequired(None)));
    }

    /// [`rejected_v2_json`] keeps the `HandshakeRequired` newtype payload on
    /// the wire — the whole point of the helper. Other rejection variants
    /// share their wire form with [`GossipResponse::Rejected`] (the unit
    /// string for unit variants, newtype for `UnsupportedProtocolVersion`).
    #[test]
    fn rejected_v2_json_preserves_handshake_required_payload() {
        let cases = [
            (
                HandshakeRequirementReason::RequestOriginIsNotInThePeerList,
                "RequestOriginIsNotInThePeerList",
            ),
            (
                HandshakeRequirementReason::RequestOriginDoesNotMatchExpected,
                "RequestOriginDoesNotMatchExpected",
            ),
            (
                HandshakeRequirementReason::MinerAddressIsUnknown,
                "MinerAddressIsUnknown",
            ),
        ];
        for (reason, wire_name) in cases {
            let json = rejected_v2_json(RejectionReason::HandshakeRequired(Some(reason)));
            assert_eq!(
                json,
                serde_json::json!({ "Rejected": { "HandshakeRequired": wire_name } }),
                "v2 envelope must preserve {reason:?} on the wire",
            );
        }

        // `HandshakeRequired(None)` — the in-memory form when the producer
        // doesn't have a specific reason — still emits the object form on v2.
        assert_eq!(
            rejected_v2_json(RejectionReason::HandshakeRequired(None)),
            serde_json::json!({ "Rejected": { "HandshakeRequired": serde_json::Value::Null } }),
        );
    }

    /// Non-`HandshakeRequired` variants serialize identically through
    /// [`rejected_v2_json`] and [`GossipResponse::Rejected`] — the v2 envelope
    /// only changes the wire shape for the variant that carries the diagnostic.
    #[test]
    fn rejected_v2_json_matches_v1_for_non_handshake_variants() {
        for reason in [
            RejectionReason::GossipDisabled,
            RejectionReason::InvalidData,
            RejectionReason::RateLimited,
            RejectionReason::UnableToVerifyOrigin,
            RejectionReason::InvalidCredentials,
            RejectionReason::ProtocolMismatch,
            RejectionReason::UnsupportedFeature,
            RejectionReason::UnsupportedProtocolVersion(7),
        ] {
            let v1 = serde_json::to_value(GossipResponse::<()>::Rejected(reason)).unwrap();
            let v2 = rejected_v2_json(reason);
            assert_eq!(v1, v2, "v2 envelope diverged from v1 for {reason:?}");
        }
    }

    /// End-to-end: a v2-form rejection is parsed by the receiver as
    /// [`GossipResponse`] (which is what real clients use), and the
    /// `Some(reason)` payload survives the round-trip.
    #[test]
    fn rejected_v2_json_round_trips_through_v1_deserializer() {
        let original = RejectionReason::HandshakeRequired(Some(
            HandshakeRequirementReason::MinerAddressIsUnknown,
        ));
        let json = rejected_v2_json(original).to_string();
        let decoded: GossipResponse<()> = serde_json::from_str(&json).unwrap();
        match decoded {
            GossipResponse::Rejected(RejectionReason::HandshakeRequired(Some(reason))) => {
                assert_eq!(
                    std::mem::discriminant(&reason),
                    std::mem::discriminant(&HandshakeRequirementReason::MinerAddressIsUnknown),
                );
            }
            other => {
                panic!("expected HandshakeRequired(Some(MinerAddressIsUnknown)), got {other:?}")
            }
        }
    }

    /// Backpressure signals from chunk-ingress and ingress-proof MUST map to
    /// `GossipError::RateLimited` (which the gossip POST handlers translate
    /// to `RejectionReason::RateLimited`). Pre-fix the chunk path collapsed
    /// to `Advisory(ChunkIngress(Overloaded))`, which the handlers further
    /// collapsed to `RejectionReason::InvalidData` — losing the retry signal
    /// AND penalising the peer for our own backpressure.
    #[test]
    fn ingress_proof_overloaded_maps_to_rate_limited() {
        let mapped: GossipError = IngressProofError::Overloaded.into();
        assert!(
            matches!(mapped, GossipError::RateLimited),
            "expected GossipError::RateLimited, got {:?}",
            mapped
        );
    }

    #[test]
    fn chunk_ingress_overloaded_maps_to_rate_limited() {
        let mapped: GossipError =
            ChunkIngressError::Advisory(AdvisoryChunkIngressError::Overloaded).into();
        assert!(
            matches!(mapped, GossipError::RateLimited),
            "expected GossipError::RateLimited, got {:?}",
            mapped
        );
    }

    /// Non-overload Advisory variants stay in the Advisory bucket.
    #[test]
    fn chunk_ingress_other_advisory_stays_advisory() {
        let mapped: GossipError =
            ChunkIngressError::Advisory(AdvisoryChunkIngressError::Other("test".into())).into();
        assert!(
            matches!(
                mapped,
                GossipError::Advisory(AdvisoryGossipError::ChunkIngress(
                    AdvisoryChunkIngressError::Other(_)
                ))
            ),
            "expected Advisory(ChunkIngress(Other)), got {:?}",
            mapped
        );
    }
}
