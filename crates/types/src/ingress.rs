use alloy_primitives::{Address, Parity, U256};
use alloy_signer::Signature;
use reth_codecs::Compact;
use reth_db::DatabaseError;
use reth_db_api::table::{Compress, Decompress};
use reth_primitives::recover_signer_unchecked;
use serde::{Deserialize, Serialize};

use crate::irys::IrysSigner;

use crate::{
    generate_data_root, generate_interleaved_leaves, DataChunks, DataRoot, Node, H256,
    IRYS_CHAIN_ID,
};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Compact)]
pub struct IngressProof {
    pub signature: Signature,
    pub data_root: H256,
    pub proof: H256,
}

impl Compress for IngressProof {
    type Compressed = Vec<u8>;
    fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(self, buf: &mut B) {
        let _ = Compact::to_compact(&self, buf);
    }
}
impl Decompress for IngressProof {
    fn decompress(value: &[u8]) -> Result<IngressProof, DatabaseError> {
        let (obj, _) = Compact::from_compact(value, value.len());
        Ok(obj)
    }
}

impl Default for IngressProof {
    fn default() -> Self {
        Self {
            signature: Signature::new(U256::ZERO, U256::ZERO, Parity::Parity(false)),
            data_root: Default::default(),
            proof: Default::default(),
        }
    }
}

pub fn generate_ingress_proof_tree(chunks: DataChunks, address: Address) -> eyre::Result<Node> {
    let chunks = generate_interleaved_leaves(chunks.clone(), address)?;
    let root = generate_data_root(chunks.clone())?;
    Ok(root)
}

pub fn generate_ingress_proof(
    signer: IrysSigner,
    data_root: DataRoot,
    chunks: DataChunks,
) -> eyre::Result<IngressProof> {
    let root = generate_ingress_proof_tree(chunks, signer.address())?;
    let proof: [u8; 32] = root.id.clone();
    let mut signature: Signature = signer.signer.sign_prehash_recoverable(&proof)?.into();
    signature = signature.with_chain_id(IRYS_CHAIN_ID);
    Ok(IngressProof {
        signature,
        data_root,
        proof: H256(root.id.clone()),
    })
}

pub fn verify_ingress_proof(proof: IngressProof, chunks: DataChunks) -> eyre::Result<bool> {
    let prehash = proof.proof.0;
    let sig = proof.signature.as_bytes();

    let recovered_address = recover_signer_unchecked(&sig, &prehash)?;

    // re-compute the proof
    let recomputed = generate_ingress_proof_tree(chunks, recovered_address)?;
    // make sure they match
    Ok(recomputed.id == prehash)
}

mod tests {
    use rand::Rng;

    use crate::{
        generate_leaves, hash_sha256, ingress::verify_ingress_proof, irys::IrysSigner, DataChunks,
        H256, MAX_CHUNK_SIZE,
    };

    use super::generate_ingress_proof;

    #[test]
    fn interleave_test() -> eyre::Result<()> {
        let data_size = (MAX_CHUNK_SIZE as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);
        let signer = IrysSigner::random_signer();
        let leaves = generate_leaves(data_bytes.clone())?;
        let interleave_value = signer.address();
        let interleave_hash = hash_sha256(&interleave_value.0 .0)?;

        // interleave the interleave hash with the leaves
        // TODO improve
        let mut interleaved = Vec::with_capacity(leaves.len() * 2);
        for leaf in leaves.iter() {
            interleaved.push(interleave_hash);
            interleaved.push(leaf.id)
        }

        let interleaved_hash = hash_sha256(interleaved.concat().as_slice())?;
        Ok(())
    }

    #[test]
    fn basic() -> eyre::Result<()> {
        let data_size = (MAX_CHUNK_SIZE as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);
        let signer = IrysSigner::random_signer();
        let data_root = H256::random();
        let chunks: DataChunks = data_bytes
            .chunks(MAX_CHUNK_SIZE)
            .map(|c| c.to_vec())
            .collect();
        let proof = generate_ingress_proof(signer.clone(), data_root, chunks.clone())?;

        assert!(verify_ingress_proof(proof.clone(), chunks.clone())?);
        let mut reversed = chunks.clone();
        reversed.reverse();
        assert!(!verify_ingress_proof(proof, reversed)?);

        Ok(())
    }
}
