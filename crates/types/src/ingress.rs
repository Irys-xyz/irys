use alloy_primitives::Address;
use alloy_signer::Signature;
use k256::ecdsa::VerifyingKey;
use rand::Rng;
use reth_primitives::recover_signer_unchecked;
use serde::{Deserialize, Serialize};

use crate::{generate_leaves, hash_sha256, irys::IrysSigner, MAX_CHUNK_SIZE};

use crate::{generate_data_root, generate_interleaved_leaves, Node, H256, IRYS_CHAIN_ID};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngressProof {
    tx_id: H256,     // IrysTxId
    owner: [u8; 20], // address
    signature: Signature,
    block_relative_tx_offset: u128,
    data_root: H256,
    data_size: u128,
    proof: H256,
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
        owner: signer.address().0 .0,
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
