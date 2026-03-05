use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipResponse<T> {
    Accepted(T),
    Rejected(RejectionReason),
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

// -- Conversions --

impl From<crate::types::HandshakeRequirementReason> for HandshakeRequirementReason {
    fn from(r: crate::types::HandshakeRequirementReason) -> Self {
        match r {
            crate::types::HandshakeRequirementReason::RequestOriginIsNotInThePeerList => {
                Self::RequestOriginIsNotInThePeerList
            }
            crate::types::HandshakeRequirementReason::RequestOriginDoesNotMatchExpected => {
                Self::RequestOriginDoesNotMatchExpected
            }
            crate::types::HandshakeRequirementReason::MinerAddressIsUnknown => {
                Self::MinerAddressIsUnknown
            }
        }
    }
}

impl From<HandshakeRequirementReason> for crate::types::HandshakeRequirementReason {
    fn from(r: HandshakeRequirementReason) -> Self {
        match r {
            HandshakeRequirementReason::RequestOriginIsNotInThePeerList => {
                Self::RequestOriginIsNotInThePeerList
            }
            HandshakeRequirementReason::RequestOriginDoesNotMatchExpected => {
                Self::RequestOriginDoesNotMatchExpected
            }
            HandshakeRequirementReason::MinerAddressIsUnknown => Self::MinerAddressIsUnknown,
        }
    }
}

impl From<crate::types::RejectionReason> for RejectionReason {
    fn from(r: crate::types::RejectionReason) -> Self {
        match r {
            crate::types::RejectionReason::HandshakeRequired(reason) => {
                Self::HandshakeRequired(reason.map(Into::into))
            }
            crate::types::RejectionReason::GossipDisabled => Self::GossipDisabled,
            crate::types::RejectionReason::InvalidData => Self::InvalidData,
            crate::types::RejectionReason::RateLimited => Self::RateLimited,
            crate::types::RejectionReason::UnableToVerifyOrigin => Self::UnableToVerifyOrigin,
            crate::types::RejectionReason::InvalidCredentials => Self::InvalidCredentials,
            crate::types::RejectionReason::ProtocolMismatch => Self::ProtocolMismatch,
            crate::types::RejectionReason::UnsupportedProtocolVersion(v) => {
                Self::UnsupportedProtocolVersion(v)
            }
            crate::types::RejectionReason::UnsupportedFeature => Self::UnsupportedFeature,
        }
    }
}

impl From<RejectionReason> for crate::types::RejectionReason {
    fn from(r: RejectionReason) -> Self {
        match r {
            RejectionReason::HandshakeRequired(reason) => {
                Self::HandshakeRequired(reason.map(Into::into))
            }
            RejectionReason::GossipDisabled => Self::GossipDisabled,
            RejectionReason::InvalidData => Self::InvalidData,
            RejectionReason::RateLimited => Self::RateLimited,
            RejectionReason::UnableToVerifyOrigin => Self::UnableToVerifyOrigin,
            RejectionReason::InvalidCredentials => Self::InvalidCredentials,
            RejectionReason::ProtocolMismatch => Self::ProtocolMismatch,
            RejectionReason::UnsupportedProtocolVersion(v) => Self::UnsupportedProtocolVersion(v),
            RejectionReason::UnsupportedFeature => Self::UnsupportedFeature,
        }
    }
}

impl<T, U: From<T>> From<crate::types::GossipResponse<T>> for GossipResponse<U> {
    fn from(r: crate::types::GossipResponse<T>) -> Self {
        match r {
            crate::types::GossipResponse::Accepted(t) => Self::Accepted(t.into()),
            crate::types::GossipResponse::Rejected(reason) => Self::Rejected(reason.into()),
        }
    }
}

impl<T, U: From<T>> From<GossipResponse<T>> for crate::types::GossipResponse<U> {
    fn from(r: GossipResponse<T>) -> Self {
        match r {
            GossipResponse::Accepted(t) => Self::Accepted(t.into()),
            GossipResponse::Rejected(reason) => Self::Rejected(reason.into()),
        }
    }
}
