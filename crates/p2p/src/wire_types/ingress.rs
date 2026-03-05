use irys_types::{IrysSignature, H256};
use serde::{Deserialize, Serialize};

/// Sovereign wire type for IngressProof (flattened versioned enum).
/// Matches the IntegerTagged JSON output: { "version": N, ...inner_fields... }
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IngressProof {
    pub version: u8,
    pub anchor: H256,
    pub chain_id: u64,
    pub data_root: H256,
    pub proof: H256,
    pub signature: IrysSignature,
}

impl From<&irys_types::IngressProof> for IngressProof {
    fn from(p: &irys_types::IngressProof) -> Self {
        match p {
            irys_types::IngressProof::V1(inner) => Self {
                version: 1,
                signature: inner.signature,
                data_root: inner.data_root,
                proof: inner.proof,
                chain_id: inner.chain_id,
                anchor: inner.anchor,
            },
        }
    }
}

impl TryFrom<IngressProof> for irys_types::IngressProof {
    type Error = eyre::Report;
    fn try_from(p: IngressProof) -> eyre::Result<Self> {
        match p.version {
            1 => Ok(Self::V1(irys_types::ingress::IngressProofV1 {
                signature: p.signature,
                data_root: p.data_root,
                proof: p.proof,
                chain_id: p.chain_id,
                anchor: p.anchor,
            })),
            v => Err(eyre::eyre!("Unknown IngressProof version: {}", v)),
        }
    }
}
