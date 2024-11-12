use alloy_signer::Signature;
use k256::ecdsa::VerifyingKey;
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::{generate_leaves, hash_sha256, irys::IrysSigner, MAX_CHUNK_SIZE};

use crate::{Node, H256};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngressProof {
    tx_id: H256, // IrysTxId
    // owner: VerifyingKey, we can recover this from the signature
    signature: Signature,
    tx_offset: u128,
    data_root: H256,
    data_size: u128,
    proof: H256,
}

pub fn ingress_proof_from_leaves(
    signer: IrysSigner,
    leaves: Vec<Node>,
) -> eyre::Result<IngressProof> {
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

    Ok(IngressProof {
        tx_id: H256::random(),
        signature: signer
            .signer
            .sign_prehash_recoverable(&interleaved_hash)?
            .into(),
        tx_offset: 0,
        data_root: H256::random(),
        data_size: 0,
        proof: H256::from(interleaved_hash),
    })
}

mod tests {
    use rand::Rng;

    use crate::{generate_leaves, hash_sha256, irys::IrysSigner, MAX_CHUNK_SIZE};

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
}
