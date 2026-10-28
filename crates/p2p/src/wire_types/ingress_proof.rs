use irys_types::{DataCommitment, H256, IrysSignature};
use serde::{Deserialize, Serialize};

use super::impl_json_version_tagged_serde;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IngressProofV1Inner {
    pub signature: IrysSignature,
    /// The JSON key stays `data_root` for wire compatibility.
    #[serde(rename = "data_root")]
    pub data_commitment: DataCommitment,
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
    signature, data_commitment, proof, chain_id, anchor,
});

super::impl_mirror_enum_from!(
    irys_types::IngressProof, IngressProof mixed {
        convert: V1;
    }
);
