use alloy_primitives::{Address, ChainId};
use alloy_signer::Signature;
use arbitrary::Arbitrary;
use eyre::OptionExt as _;
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

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq, Compact, Arbitrary)]

pub struct CachedIngressProof {
    pub address: Address, // subkey
    pub proof: IngressProof,
}

#[derive(
    Debug,
    Default,
    Clone,
    Serialize,
    Deserialize,
    PartialEq,
    Eq,
    Compact,
    Arbitrary,
    alloy_rlp::RlpEncodable,
    alloy_rlp::RlpDecodable,
)]
pub struct IngressProof {
    pub signature: IrysSignature,
    pub data_root: H256,
    pub proof: H256,
    pub chain_id: ChainId,
}

impl IngressProof {
    /// Generates the prehash for signing/verification
    /// Combines proof, data_root, and chain_id into a single digest
    pub fn generate_prehash(proof: &H256, data_root: &H256, chain_id: ChainId) -> [u8; 32] {
        let mut hasher = sha::Sha256::new();
        hasher.update(&proof.0);
        hasher.update(&data_root.0);
        hasher.update(&chain_id.to_be_bytes());
        hasher.finish()
    }

    /// Recovers the signer address from the ingress proof signature
    pub fn recover_signer(&self) -> eyre::Result<Address> {
        // Recreate the prehash that was signed
        let prehash = Self::generate_prehash(&self.proof, &self.data_root, self.chain_id);

        // Recover the signer from the signature
        let sig = self.signature.as_bytes();
        let recovered_address = recover_signer(&sig[..].try_into()?, prehash.into())?;

        Ok(recovered_address)
    }

    /// Validates that the proof matches the provided data_root and recovers the signer address
    /// This method ensures the proof is for the correct data_root before validating the signature
    pub fn pre_validate(&self, data_root: &H256) -> eyre::Result<Address> {
        // Validate that the data_root matches
        if self.data_root != *data_root {
            return Err(eyre::eyre!("Ingress proof data_root mismatch"));
        }

        // Recover and return the signer address
        self.recover_signer()
    }
}

impl Compress for IngressProof {
    type Compressed = Vec<u8>;
    fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        let _ = Compact::to_compact(&self, buf);
    }
}
impl Decompress for IngressProof {
    fn decompress(value: &[u8]) -> Result<Self, DatabaseError> {
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
    signer: &IrysSigner,
    data_root: DataRoot,
    chunks: impl Iterator<Item = eyre::Result<ChunkBytes>>,
    chain_id: u64,
) -> eyre::Result<IngressProof> {
    let (root, _) = generate_ingress_proof_tree(chunks, signer.address(), false)?;
    let proof = H256(root.id);

    // Use the shared prehash generation function
    let prehash = IngressProof::generate_prehash(&proof, &data_root, chain_id);

    let signature: Signature = signer.signer.sign_prehash_recoverable(&prehash)?.into();

    Ok(IngressProof {
        signature: signature.into(),
        data_root,
        proof,
        chain_id,
    })
}

pub fn verify_ingress_proof(
    proof: IngressProof,
    chunks: impl Iterator<Item = eyre::Result<ChunkBytes>>,
    chain_id: ChainId,
) -> eyre::Result<bool> {
    if chain_id != proof.chain_id {
        return Ok(false); // Chain ID mismatch
    }

    let prehash = IngressProof::generate_prehash(&proof.proof, &proof.data_root, proof.chain_id);

    let sig = proof.signature.as_bytes();

    let recovered_address = recover_signer(&sig[..].try_into()?, prehash.into())?;

    // re-compute the ingress proof & regular trees & roots
    let (proof_root, regular_root) = generate_ingress_proof_tree(chunks, recovered_address, true)?;

    let data_root = H256(
        regular_root
            .ok_or_eyre("expected regular_root to be Some")?
            .id,
    );

    // re-compute the prehash (combining data_root, proof, and chain_id)
    let new_prehash =
        IngressProof::generate_prehash(&H256(proof_root.id), &data_root, proof.chain_id);

    // make sure they match
    Ok(new_prehash == prehash)
}

#[cfg(test)]
mod tests {
    use rand::Rng as _;

    use crate::{
        generate_data_root, generate_leaves, hash_sha256, ingress::verify_ingress_proof,
        irys::IrysSigner, ConsensusConfig, H256,
    };

    use super::generate_ingress_proof;

    #[test]
    fn interleave_test() -> eyre::Result<()> {
        let testing_config = ConsensusConfig::testing();
        let data_size = (testing_config.chunk_size as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0_u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);
        let signer = IrysSigner::random_signer(&testing_config);
        let leaves = generate_leaves(
            vec![data_bytes].into_iter().map(Ok),
            testing_config.chunk_size as usize,
        )?;
        let interleave_value = signer.address();
        let interleave_hash = hash_sha256(&interleave_value.0 .0);

        // interleave the interleave hash with the leaves
        // TODO improve
        let mut interleaved = Vec::with_capacity(leaves.len() * 2);
        for leaf in leaves.iter() {
            interleaved.push(interleave_hash);
            interleaved.push(leaf.id)
        }

        let _interleaved_hash = hash_sha256(interleaved.concat().as_slice());
        Ok(())
    }

    #[test]
    fn basic() -> eyre::Result<()> {
        // Create some random data
        let testing_config = ConsensusConfig::testing();
        let data_size = (testing_config.chunk_size as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0_u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        // Build a merkle tree and data_root from the chunks
        let leaves = generate_leaves(
            vec![data_bytes.clone()].into_iter().map(Ok),
            testing_config.chunk_size as usize,
        )
        .unwrap();
        let root = generate_data_root(leaves)?;
        let data_root = H256(root.id);

        // Generate an ingress proof with chain_id
        let signer = IrysSigner::random_signer(&testing_config);
        let chunks: Vec<Vec<u8>> = data_bytes
            .chunks(testing_config.chunk_size as usize)
            .map(Vec::from)
            .collect();
        let chain_id = 1; // Example chain_id for testing
        let proof = generate_ingress_proof(
            &signer,
            data_root,
            chunks.clone().into_iter().map(Ok),
            chain_id,
        )?;

        // Verify the ingress proof
        assert!(verify_ingress_proof(
            proof.clone(),
            chunks.clone().into_iter().map(Ok),
            chain_id
        )?);
        let mut reversed = chunks;
        reversed.reverse();
        assert!(!verify_ingress_proof(
            proof,
            reversed.into_iter().map(Ok),
            chain_id
        )?);

        Ok(())
    }

    #[test]
    fn test_chain_id_prevents_replay_attack() -> eyre::Result<()> {
        // Create some random data
        let testing_config = ConsensusConfig::testing();
        let data_size = (testing_config.chunk_size as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0_u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        // Build a merkle tree and data_root from the chunks
        let leaves = generate_leaves(
            vec![data_bytes.clone()].into_iter().map(Ok),
            testing_config.chunk_size as usize,
        )
        .unwrap();
        let root = generate_data_root(leaves)?;
        let data_root = H256(root.id);

        let signer = IrysSigner::random_signer(&testing_config);
        let chunks: Vec<Vec<u8>> = data_bytes
            .chunks(testing_config.chunk_size as usize)
            .map(Vec::from)
            .collect();

        // Generate proof for testnet (chain_id = 1)
        let testnet_chain_id = 1;
        let testnet_proof = generate_ingress_proof(
            &signer,
            data_root,
            chunks.clone().into_iter().map(Ok),
            testnet_chain_id,
        )?;

        // Generate proof for mainnet (chain_id = 2)
        let mainnet_chain_id = 2;
        let mainnet_proof = generate_ingress_proof(
            &signer,
            data_root,
            chunks.clone().into_iter().map(Ok),
            mainnet_chain_id,
        )?;

        // Verify that testnet proof is valid for testnet
        assert!(verify_ingress_proof(
            testnet_proof.clone(),
            chunks.clone().into_iter().map(Ok),
            testnet_chain_id
        )?);

        // Verify that mainnet proof is valid for mainnet
        assert!(verify_ingress_proof(
            mainnet_proof,
            chunks.clone().into_iter().map(Ok),
            mainnet_chain_id
        )?);

        // Create a modified proof where we try to use testnet proof with mainnet chain_id
        let mut replay_attack_proof = testnet_proof;
        replay_attack_proof.chain_id = mainnet_chain_id;

        // This should fail verification because the signature was created with testnet chain_id
        // but we're trying to verify it with mainnet chain_id
        assert!(!verify_ingress_proof(
            replay_attack_proof.clone(),
            chunks.clone().into_iter().map(Ok),
            mainnet_chain_id
        )?);

        // This should fail verification because there's going to be a mismatch in chain_id
        // even if the proof is valid for testnet
        assert!(!verify_ingress_proof(
            replay_attack_proof,
            chunks.into_iter().map(Ok),
            testnet_chain_id
        )?);

        Ok(())
    }
}
