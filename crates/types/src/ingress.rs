use alloy_primitives::Address;
use alloy_signer::Signature;
use eyre::OptionExt;
use openssl::sha;
use reth_codecs::Compact;
use reth_db::DatabaseError;
use reth_db_api::table::{Compress, Decompress};
use reth_primitives::transaction::recover_signer;
use serde::{Deserialize, Serialize};

use crate::irys::IrysSigner;

use crate::{
    generate_data_root, generate_ingress_leaves, ChunkBytes, DataRoot, IrysSignature, Node, H256,
};
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq, Compact)]
pub struct IngressProof {
    pub signature: IrysSignature,
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

pub fn generate_ingress_proof_tree(
    chunks: impl Iterator<Item = eyre::Result<ChunkBytes>>,
    address: Address,
    and_regular: bool,
) -> eyre::Result<(Node, Option<Node>)> {
    let (ingress_leaves, regular_leaves) = generate_ingress_leaves(chunks, address, and_regular)?;
    let ingress_root = generate_data_root(ingress_leaves)?;

    Ok((
        ingress_root,
        regular_leaves.map(generate_data_root).transpose()?,
    ))
}

pub fn generate_ingress_proof(
    signer: IrysSigner,
    data_root: DataRoot,
    chunks: impl Iterator<Item = eyre::Result<ChunkBytes>>,
) -> eyre::Result<IngressProof> {
    let (root, _) = generate_ingress_proof_tree(chunks, signer.address(), false)?;
    let proof: [u8; 32] = root.id;

    // Combine proof and data_root into a single digest to sign
    let mut hasher = sha::Sha256::new();
    hasher.update(&proof);
    hasher.update(&data_root.0);
    let prehash = hasher.finish();

    let signature: Signature = signer.signer.sign_prehash_recoverable(&prehash)?.into();

    Ok(IngressProof {
        signature: signature.into(),
        data_root,
        proof: H256(root.id),
    })
}

pub fn verify_ingress_proof(
    proof: IngressProof,
    chunks: impl Iterator<Item = eyre::Result<ChunkBytes>>,
) -> eyre::Result<bool> {
    let mut hasher = sha::Sha256::new();
    hasher.update(&proof.proof.0);
    hasher.update(&proof.data_root.0);
    let prehash = hasher.finish();

    let sig = proof.signature.as_bytes();

    let recovered_address = recover_signer(&sig[..].try_into()?, prehash.into())
        .ok_or_eyre("Unable to recover signer")?;

    // re-compute the ingress proof & regular trees & roots
    let (proof_root, regular_root) = generate_ingress_proof_tree(chunks, recovered_address, true)?;

    let data_root = H256(
        regular_root
            .ok_or_eyre("expected regular_root to be Some")?
            .id,
    );

    // re-compute the prehash (combining data_root and proof)
    let mut hasher = sha::Sha256::new();
    hasher.update(&proof_root.id);
    hasher.update(&data_root.0);
    let new_prehash = hasher.finish();

    // make sure they match
    Ok(new_prehash == prehash)
}

#[cfg(test)]
mod tests {
    use rand::Rng;

    use crate::{
        generate_data_root, generate_leaves, hash_sha256, ingress::verify_ingress_proof,
        irys::IrysSigner, Config, H256,
    };

    use super::generate_ingress_proof;

    #[test]
    fn interleave_test() -> eyre::Result<()> {
        let testnet_config = Config::testnet();
        let data_size = (testnet_config.chunk_size as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);
        let signer = IrysSigner::random_signer(&testnet_config);
        let leaves = generate_leaves(
            vec![data_bytes].into_iter().map(Ok),
            testnet_config.chunk_size as usize,
        )?;
        let interleave_value = signer.address();
        let interleave_hash = hash_sha256(&interleave_value.0 .0)?;

        // interleave the interleave hash with the leaves
        // TODO improve
        let mut interleaved = Vec::with_capacity(leaves.len() * 2);
        for leaf in leaves.iter() {
            interleaved.push(interleave_hash);
            interleaved.push(leaf.id)
        }

        let _interleaved_hash = hash_sha256(interleaved.concat().as_slice())?;
        Ok(())
    }

    #[test]
    fn basic() -> eyre::Result<()> {
        // Create some random data
        let testnet_config = Config::testnet();
        let data_size = (testnet_config.chunk_size as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        // Build a merkle tree and data_root from the chunks
        let leaves = generate_leaves(
            vec![data_bytes.clone()].into_iter().map(Ok),
            testnet_config.chunk_size as usize,
        )
        .unwrap();
        let root = generate_data_root(leaves)?;
        let data_root = H256(root.id);

        // Generate an ingress proof
        let signer = IrysSigner::random_signer(&testnet_config);
        let chunks: Vec<Vec<u8>> = data_bytes
            .chunks(testnet_config.chunk_size as usize)
            .map(|c| Vec::from(c))
            .collect();
        let proof = generate_ingress_proof(
            signer.clone(),
            data_root,
            chunks.clone().into_iter().map(Ok),
        )?;

        // Verify the ingress proof
        assert!(verify_ingress_proof(
            proof.clone(),
            chunks.clone().into_iter().map(Ok)
        )?);
        let mut reversed = chunks.clone();
        reversed.reverse();
        assert!(!verify_ingress_proof(proof, reversed.into_iter().map(Ok))?);

        Ok(())
    }
}
