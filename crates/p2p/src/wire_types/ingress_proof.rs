use irys_types::ingress::DataSourceType;
use irys_types::kzg::KzgCommitmentBytes;
use irys_types::{H256, IrysSignature};
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IngressProofV2Inner {
    pub signature: IrysSignature,
    pub data_root: H256,
    pub kzg_commitment: KzgCommitmentBytes,
    pub composite_commitment: H256,
    pub chain_id: u64,
    pub anchor: H256,
    pub source_type: DataSourceType,
    #[serde(default)]
    pub possession_y: [u8; irys_types::kzg::SCALAR_SIZE],
    #[serde(default)]
    pub possession_proof: KzgCommitmentBytes,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IngressProof {
    V1(IngressProofV1Inner),
    V2(IngressProofV2Inner),
}

impl_json_version_tagged_serde!(IngressProof {
    1 => V1(IngressProofV1Inner),
    2 => V2(IngressProofV2Inner),
});

super::impl_mirror_from!(irys_types::ingress::IngressProofV1 => IngressProofV1Inner {
    signature, data_root, proof, chain_id, anchor,
});

super::impl_mirror_from!(irys_types::ingress::IngressProofV2 => IngressProofV2Inner {
    signature,
    data_root,
    kzg_commitment,
    composite_commitment,
    chain_id,
    anchor,
    source_type,
    possession_y,
    possession_proof,
});

super::impl_mirror_enum_from!(
    irys_types::IngressProof, IngressProof mixed {
        convert: V1, V2;
    }
);
