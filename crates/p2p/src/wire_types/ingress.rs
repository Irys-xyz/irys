use irys_types::{IrysSignature, H256};
use serde::{Deserialize, Serialize};

use super::impl_version_tagged_serde;

/// Inner fields for IngressProof V1.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IngressProofV1Inner {
    pub anchor: H256,
    pub chain_id: u64,
    pub data_root: H256,
    pub proof: H256,
    pub signature: IrysSignature,
}

/// Sovereign wire type for IngressProof (IntegerTagged-compatible).
#[derive(Debug, Clone, PartialEq)]
pub enum IngressProof {
    V1(IngressProofV1Inner),
}

impl_version_tagged_serde!(IngressProof { 1 => V1(IngressProofV1Inner) });

impl From<&irys_types::IngressProof> for IngressProof {
    fn from(p: &irys_types::IngressProof) -> Self {
        match p {
            irys_types::IngressProof::V1(inner) => Self::V1(IngressProofV1Inner {
                signature: inner.signature,
                data_root: inner.data_root,
                proof: inner.proof,
                chain_id: inner.chain_id,
                anchor: inner.anchor,
            }),
        }
    }
}

impl TryFrom<IngressProof> for irys_types::IngressProof {
    type Error = eyre::Report;
    fn try_from(p: IngressProof) -> eyre::Result<Self> {
        match p {
            IngressProof::V1(inner) => Ok(Self::V1(irys_types::ingress::IngressProofV1 {
                signature: inner.signature,
                data_root: inner.data_root,
                proof: inner.proof,
                chain_id: inner.chain_id,
                anchor: inner.anchor,
            })),
        }
    }
}
