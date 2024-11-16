use alloy_primitives::{Address, Parity, U256};
use alloy_signer::Signature;
use reth_codecs::Compact;
use reth_db::DatabaseError;
use reth_db_api::table::{Compress, Decompress};
use reth_primitives::recover_signer_unchecked;
use serde::{Deserialize, Serialize};

use crate::irys::IrysSigner;

use crate::{generate_data_root, generate_interleaved_leaves, Node, H256, IRYS_CHAIN_ID};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Compact)]
pub struct IngressProof {
    pub tx_id: H256,
    pub owner: Address,
    pub signature: Signature,
    pub block_relative_tx_offset: u128,
    pub data_root: H256,
    pub data_size: u128,
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
            tx_id: Default::default(),
            owner: Default::default(),
            signature: Signature::new(U256::ZERO, U256::ZERO, Parity::Parity(false)),
            block_relative_tx_offset: Default::default(),
            data_root: Default::default(),
            data_size: Default::default(),
            proof: Default::default(),
        }
    }
}

pub fn generate_ingress_proof_tree(data: Vec<u8>, address: Address) -> eyre::Result<Node> {
    let chunks = generate_interleaved_leaves(data.clone(), address.as_slice())?;
    let root = generate_data_root(chunks.clone())?;
    Ok(root)
}

pub fn generate_ingress_proof(signer: IrysSigner, data: Vec<u8>) -> eyre::Result<IngressProof> {
    let root = generate_ingress_proof_tree(data, signer.address())?;
    let proof: [u8; 32] = root.id.clone();
    let mut signature: Signature = signer.signer.sign_prehash_recoverable(&proof)?.into();
    signature = signature.with_chain_id(IRYS_CHAIN_ID);
    Ok(IngressProof {
        tx_id: H256::random(),
        owner: signer.address(),
        signature,
        block_relative_tx_offset: 0,
        data_root: H256::random(),
        data_size: 0,
        proof: H256(root.id.clone()),
    })
}

pub fn verify_ingress_proof(proof: IngressProof, data: Vec<u8>) -> eyre::Result<bool> {
    let prehash = proof.proof.0;
    let sig = proof.signature.as_bytes();

    let recovered_address = recover_signer_unchecked(&sig, &prehash)?;
    if recovered_address != proof.owner {
        return Ok(false);
    }
    // re-compute the proof
    let recomputed = generate_ingress_proof_tree(data, recovered_address)?;
    // make sure they match
    Ok(recomputed.id == prehash)
}

mod tests {
    use rand::Rng;

    use crate::{
        generate_leaves, hash_sha256, ingress::verify_ingress_proof, irys::IrysSigner,
        MAX_CHUNK_SIZE,
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
        let proof = generate_ingress_proof(signer.clone(), data_bytes.clone())?;

        assert!(verify_ingress_proof(proof.clone(), data_bytes.clone())?);
        let mut reversed = data_bytes.clone();
        reversed.reverse();
        assert!(!verify_ingress_proof(proof, reversed)?);

        Ok(())
    }
}
