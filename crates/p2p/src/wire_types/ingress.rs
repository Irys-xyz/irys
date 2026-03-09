use irys_types::{IrysSignature, H256};
use serde::{Deserialize, Serialize};

use super::impl_json_version_tagged_serde;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IngressProofV1Inner {
    pub signature: IrysSignature,
    pub data_root: H256,
    pub proof: H256,
    pub chain_id: u64,
    pub anchor: H256,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IngressProof {
    V1(IngressProofV1Inner),
}

impl_json_version_tagged_serde!(IngressProof { 1 => V1(IngressProofV1Inner) });

super::impl_mirror_from!(irys_types::ingress::IngressProofV1 => IngressProofV1Inner {
    signature, data_root, proof, chain_id, anchor,
});

impl From<&irys_types::IngressProof> for IngressProof {
    fn from(p: &irys_types::IngressProof) -> Self {
        match p {
            irys_types::IngressProof::V1(inner) => Self::V1(inner.into()),
        }
    }
}

impl From<IngressProof> for irys_types::IngressProof {
    fn from(p: IngressProof) -> Self {
        match p {
            IngressProof::V1(inner) => Self::V1(inner.into()),
        }
    }
}
