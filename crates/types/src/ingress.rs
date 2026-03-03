use crate::irys::IrysSigner;
use crate::{
    decode_rlp_version, encode_rlp_version, generate_data_root, generate_ingress_leaves, DataRoot,
    IrysAddress, IrysSignature, Node, Signable, VersionDiscriminant, Versioned, H256,
};
use alloy_primitives::{keccak256, ChainId};
use alloy_rlp::Encodable as _;
use arbitrary::Arbitrary;
use bytes::BufMut;
use eyre::OptionExt as _;
use irys_macros_integer_tagged::IntegerTagged;
use reth_codecs::Compact;
use reth_db::DatabaseError;
use reth_db_api::table::{Compress, Decompress};
use reth_primitives_traits::crypto::secp256k1::recover_signer;
use serde::{Deserialize, Serialize};
use std::ops::{Deref, DerefMut};

#[derive(Debug, Clone, PartialEq, IntegerTagged, Eq, Compact, Arbitrary)]
#[repr(u8)]
#[integer_tagged(tag = "version")]
pub enum IngressProof {
    #[integer_tagged(version = 1)]
    V1(IngressProofV1) = 1,
}

impl Default for IngressProof {
    fn default() -> Self {
        Self::V1(Default::default())
    }
}

impl Deref for IngressProof {
    type Target = IngressProofV1;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::V1(inner) => inner,
        }
    }
}

impl DerefMut for IngressProof {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::V1(inner) => inner,
        }
    }
}

impl alloy_rlp::Encodable for IngressProof {
    fn encode(&self, out: &mut dyn BufMut) {
        let mut buf = Vec::new();
        match self {
            Self::V1(inner) => {
                inner.encode(&mut buf);
            }
        }
        encode_rlp_version(buf, self.version(), out);
    }
}

impl alloy_rlp::Decodable for IngressProof {
    fn decode(buf: &mut &[u8]) -> Result<Self, alloy_rlp::Error> {
        let (version, inner_buf) = decode_rlp_version(buf)?;
        let inner_buf = &mut &inner_buf[..];

        match version {
            1 => {
                let inner = IngressProofV1::decode(inner_buf)?;
                Ok(Self::V1(inner))
            }
            _ => Err(alloy_rlp::Error::Custom("Unknown version")),
        }
    }
}

impl Signable for IngressProof {
    fn encode_for_signing(&self, out: &mut dyn BufMut) {
        self.encode(out);
    }
}

impl VersionDiscriminant for IngressProof {
    fn version(&self) -> u8 {
        match self {
            Self::V1(_) => 1,
        }
    }
}

impl IngressProof {
    pub fn recover_signer(&self) -> eyre::Result<IrysAddress> {
        let prehash = self.signature_hash();
        self.signature.recover_signer(prehash)
    }

    /// Validates that the proof matches the provided data_root and recovers the signer address
    /// This method ensures the proof is for the correct data_root before validating the signature
    pub fn pre_validate(&self, data_root: &H256) -> eyre::Result<IrysAddress> {
        // Validate that the data_root matches
        if self.data_root != *data_root {
            return Err(eyre::eyre!("Ingress proof data_root mismatch"));
        }
        // Recover and return the signer address
        self.recover_signer()
    }

    pub fn id(&self) -> H256 {
        let id: [u8; 32] = keccak256(self.signature.as_bytes()).into();
        H256::from(id)
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq, Compact, Arbitrary)]
pub struct CachedIngressProof {
    pub address: IrysAddress, // subkey
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
pub struct IngressProofV1 {
    #[rlp(skip)]
    #[rlp(default)]
    pub signature: IrysSignature,
    pub data_root: H256,
    pub proof: H256,
    pub chain_id: ChainId,
    pub anchor: H256,
}

impl Versioned for IngressProofV1 {
    const VERSION: u8 = 1;
}

impl Compress for IngressProofV1 {
    type Compressed = Vec<u8>;
    fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        let _ = Compact::to_compact(&self, buf);
    }
}
impl Decompress for IngressProofV1 {
    fn decompress(value: &[u8]) -> Result<Self, DatabaseError> {
        let (obj, _) = Compact::from_compact(value, value.len());
        Ok(obj)
    }
}

pub fn generate_ingress_proof_tree<C: AsRef<[u8]>>(
    chunks: impl Iterator<Item = eyre::Result<C>>,
    address: IrysAddress,
    and_regular: bool,
) -> eyre::Result<(Node, Option<Node>)> {
    let (ingress_leaves, regular_leaves) = generate_ingress_leaves(chunks, address, and_regular)?;
    let ingress_root = generate_data_root(ingress_leaves)?;

    Ok((
        ingress_root,
        regular_leaves.map(generate_data_root).transpose()?,
    ))
}

pub fn generate_ingress_proof<C: AsRef<[u8]>>(
    signer: &IrysSigner,
    data_root: DataRoot,
    chunks: impl Iterator<Item = eyre::Result<C>>,
    chain_id: u64,
    anchor: H256,
) -> eyre::Result<IngressProof> {
    let (root, _) = generate_ingress_proof_tree(chunks, signer.address(), false)?;
    let proof = H256(root.id);

    let mut proof = IngressProof::V1(IngressProofV1 {
        signature: Default::default(),
        data_root,
        proof,
        chain_id,
        anchor,
    });

    signer.sign_ingress_proof(&mut proof)?;
    Ok(proof)
}

pub fn verify_ingress_proof<C: AsRef<[u8]>>(
    proof: &IngressProof,
    chunks: impl IntoIterator<Item = C>,
    chain_id: ChainId,
) -> eyre::Result<bool> {
    if chain_id != proof.chain_id {
        return Ok(false); // Chain ID mismatch
    }

    let sig = proof.signature.as_bytes();
    let prehash = proof.signature_hash();

    let recovered_address = recover_signer(&sig[..].try_into()?, prehash.into())?;

    // re-compute the ingress proof & regular trees & roots
    let (proof_root, regular_root) =
        generate_ingress_proof_tree(chunks.into_iter().map(Ok), recovered_address.into(), true)?;

    let data_root = H256(
        regular_root
            .ok_or_eyre("expected regular_root to be Some")?
            .id,
    );

    // re-compute the prehash (combining data_root, proof, and chain_id)

    let new_prehash = IngressProof::V1(IngressProofV1 {
        signature: Default::default(),
        data_root,
        proof: H256(proof_root.id),
        chain_id,
        anchor: proof.anchor,
    })
    .signature_hash();

    // make sure they match
    Ok(new_prehash == prehash)
}

#[cfg(test)]
mod tests {
    use alloy_rlp::{Decodable as _, Encodable as _};
    use rand::Rng as _;

    use crate::{
        generate_data_root, generate_leaves, hash_sha256,
        ingress::{verify_ingress_proof, IngressProofV1},
        irys::IrysSigner,
        ConsensusConfig, IngressProof, H256,
    };

    use super::generate_ingress_proof;

    #[test]
    fn ingress_proof_rlp_roundtrip_test() {
        use bytes::BytesMut;
        let original = IngressProof::V1(IngressProofV1 {
            signature: crate::IrysSignature::default(),
            proof: H256::from([12_u8; 32]),
            chain_id: 1_u64,
            data_root: H256::from([13_u8; 32]),
            anchor: H256::from([14_u8; 32]),
        });

        let mut buf = BytesMut::new();
        original.encode(&mut buf);
        let mut slice = buf.as_ref();
        let decoded = IngressProof::decode(&mut slice).unwrap();
        assert_eq!(original, decoded);
        assert!(slice.is_empty());
    }

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
        let anchor = H256::random(); // example anchor for testing
        let proof = generate_ingress_proof(
            &signer,
            data_root,
            chunks.iter().map(|c| Ok(c.as_slice())),
            chain_id,
            anchor,
        )?;

        // Verify the ingress proof
        assert!(verify_ingress_proof(
            &proof,
            chunks.iter().as_slice(),
            chain_id
        )?);
        let mut reversed = chunks;
        reversed.reverse();
        assert!(!verify_ingress_proof(
            &proof,
            reversed.iter().as_slice(),
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

        let anchor = H256::random(); // example anchor for testing
                                     // Generate proof for testnet (chain_id = 1)
        let testnet_chain_id = 1;

        let testnet_proof = generate_ingress_proof(
            &signer,
            data_root,
            chunks.iter().map(|c| Ok(c.as_slice())),
            testnet_chain_id,
            anchor,
        )?;

        // Generate proof for mainnet (chain_id = 2)
        let mainnet_chain_id = 2;
        let mainnet_proof = generate_ingress_proof(
            &signer,
            data_root,
            chunks.iter().map(|c| Ok(c.as_slice())),
            mainnet_chain_id,
            anchor,
        )?;

        // Verify that testnet proof is valid for testnet
        assert!(verify_ingress_proof(
            &testnet_proof,
            chunks.iter().as_slice(),
            testnet_chain_id
        )?);

        // Verify that mainnet proof is valid for mainnet
        assert!(verify_ingress_proof(
            &mainnet_proof,
            chunks.iter().as_slice(),
            mainnet_chain_id
        )?);

        // Create a modified proof where we try to use testnet proof with mainnet chain_id
        let mut replay_attack_proof = testnet_proof;
        replay_attack_proof.chain_id = mainnet_chain_id;

        // This should fail verification because the signature was created with testnet chain_id
        // but we're trying to verify it with mainnet chain_id
        assert!(!verify_ingress_proof(
            &replay_attack_proof,
            chunks.iter().as_slice(),
            mainnet_chain_id
        )?);

        // This should fail verification because there's going to be a mismatch in chain_id
        // even if the proof is valid for testnet
        assert!(!verify_ingress_proof(
            &replay_attack_proof,
            chunks.iter().as_slice(),
            testnet_chain_id
        )?);

        Ok(())
    }
}
