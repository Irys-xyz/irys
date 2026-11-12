use crate::irys::IrysSigner;
use crate::{
    DataCommitment, H256, IngressMerkleLeaf, IrysAddress, IrysSignature, Node, Signable,
    VersionDiscriminant, Versioned, decode_rlp_version, encode_rlp_version, generate_data_root,
    generate_ingress_leaves, generate_ingress_root_from_leaves,
};
use alloy_primitives::{ChainId, keccak256};
use alloy_rlp::Encodable as _;
use arbitrary::Arbitrary;
use bytes::BufMut;
use irys_data_tree::{DataTreeBuilder, Geometry};
use irys_macros_integer_tagged::IntegerTagged;
use reth_codecs::Compact;
#[cfg(feature = "db")]
use reth_db::DatabaseError;
#[cfg(feature = "db")]
use reth_db_api::table::{Compress, Decompress};
use serde::{Deserialize, Serialize};
use std::ops::{Deref, DerefMut};

#[derive(Debug, Clone, PartialEq, IntegerTagged, Eq, Arbitrary, Compact)]
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

    /// Validates that the proof signs the provided data_commitment and recovers the signer address
    /// This method ensures the proof is for the correct data_commitment before validating the signature
    pub fn pre_validate(&self, data_commitment: &DataCommitment) -> eyre::Result<IrysAddress> {
        if self.data_commitment != *data_commitment {
            return Err(eyre::eyre!("Ingress proof data_commitment mismatch"));
        }
        // Recover and return the signer address
        self.recover_signer()
    }

    pub fn id(&self) -> H256 {
        let id: [u8; 32] = keccak256(self.signature.as_bytes()).into();
        H256::from(id)
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq, Arbitrary, Compact)]
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
    Arbitrary,
    alloy_rlp::RlpEncodable,
    alloy_rlp::RlpDecodable,
    Compact,
)]
pub struct IngressProofV1 {
    #[rlp(skip)]
    #[rlp(default)]
    pub signature: IrysSignature,
    /// The commitment of the data this proof attests to. The JSON key stays
    /// `data_root` for wire compatibility.
    #[serde(rename = "data_root")]
    pub data_commitment: DataCommitment,
    pub proof: H256,
    pub chain_id: ChainId,
    pub anchor: H256,
}

impl Versioned for IngressProofV1 {
    const VERSION: u8 = 1;
}

#[cfg(feature = "db")]
impl Compress for IngressProofV1 {
    type Compressed = Vec<u8>;
    fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        let _ = Compact::to_compact(&self, buf);
    }
}
#[cfg(feature = "db")]
impl Decompress for IngressProofV1 {
    fn decompress(value: &[u8]) -> Result<Self, DatabaseError> {
        let (obj, _) = Compact::from_compact(value, value.len());
        Ok(obj)
    }
}

/// The address-bound ingress tree over `chunks`. It stays on the v1 tree
/// encoder, so its root does not depend on the data tree.
pub fn generate_ingress_proof_tree<C: AsRef<[u8]>>(
    chunks: impl Iterator<Item = eyre::Result<C>>,
    address: IrysAddress,
) -> eyre::Result<Node> {
    generate_data_root(generate_ingress_leaves(chunks, address)?)
}

pub fn generate_ingress_proof<C: AsRef<[u8]>>(
    signer: &IrysSigner,
    data_commitment: DataCommitment,
    chunks: impl Iterator<Item = eyre::Result<C>>,
    chain_id: u64,
    anchor: H256,
) -> eyre::Result<IngressProof> {
    let root = generate_ingress_proof_tree(chunks, signer.address())?;
    generate_ingress_proof_from_root(signer, data_commitment, H256(root.id), chain_id, anchor)
}

/// Signs the existing ingress-proof payload for a precomputed ingress root.
/// This is deliberately the same payload and signer path used by full-body
/// generation; only the storage lifecycle that produced `ingress_root` differs.
pub fn generate_ingress_proof_from_root(
    signer: &IrysSigner,
    data_commitment: DataCommitment,
    ingress_root: H256,
    chain_id: u64,
    anchor: H256,
) -> eyre::Result<IngressProof> {
    let mut proof = IngressProof::V1(IngressProofV1 {
        signature: Default::default(),
        data_commitment,
        proof: ingress_root,
        chain_id,
        anchor,
    });

    signer.sign_ingress_proof(&mut proof)?;
    Ok(proof)
}

/// Builds and signs an ingress proof from ordered compact leaves.
pub fn generate_ingress_proof_from_leaves(
    signer: &IrysSigner,
    data_commitment: DataCommitment,
    leaves: impl IntoIterator<Item = IngressMerkleLeaf>,
    chain_id: u64,
    anchor: H256,
) -> eyre::Result<IngressProof> {
    let root = generate_ingress_root_from_leaves(leaves)?;
    generate_ingress_proof_from_root(signer, data_commitment, H256(root.id), chain_id, anchor)
}

/// Rebuilds both trees from `chunks` and checks the signature over the
/// recomputed data commitment and ingress root. `chunks` are the upload's
/// chunks in order; the v2 data tree built from them under `geometry` fixes
/// the commitment. Chunks that do not form an upload under `geometry` are an
/// error.
pub fn verify_ingress_proof<C: AsRef<[u8]>>(
    proof: &IngressProof,
    chunks: impl IntoIterator<Item = C>,
    chain_id: ChainId,
    geometry: &Geometry,
) -> eyre::Result<bool> {
    if chain_id != proof.chain_id {
        return Ok(false); // Chain ID mismatch
    }

    let prehash = proof.signature_hash();

    let recovered_address = proof.signature.recover_signer(prehash)?;

    // One pass over the chunks builds the address-bound tree and the data tree.
    let mut data_tree = DataTreeBuilder::new(*geometry);
    let proof_root = generate_ingress_proof_tree(
        chunks.into_iter().map(|chunk| {
            data_tree.push_chunk(chunk.as_ref())?;
            Ok(chunk)
        }),
        recovered_address,
    )?;
    let data_commitment = DataCommitment::from(data_tree.finish()?.data_commitment());

    // re-compute the prehash (combining data_commitment, proof, and chain_id)

    let new_prehash = IngressProof::V1(IngressProofV1 {
        signature: Default::default(),
        data_commitment,
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
    use rand::Rng as _;

    use crate::{
        ConsensusConfig, DataCommitment, DataTreeRoot, H256, IrysAddress,
        data_tree::{Geometry, data_commitment},
        generate_ingress_data_hash, generate_ingress_leaf_from_data_hash,
        ingress::verify_ingress_proof,
        irys::IrysSigner,
    };
    use irys_data_tree::DataTree;

    use super::{generate_ingress_proof, generate_ingress_proof_from_leaves};

    /// The commitment of the v2 data tree over `data`.
    fn upload_commitment(geometry: &Geometry, data: &[u8]) -> DataCommitment {
        DataTree::from_bytes(*geometry, data)
            .expect("non-empty data")
            .data_commitment()
            .into()
    }

    // The address-bound ingress tree must not change when the data tree moves to v2.
    const ADDRESS_BOUND_ROOT_FOR_FIXED_CHUNKS: [u8; 32] =
        alloy_primitives::hex!("b5f96912a85cb07047fc41cf7afafd4ccd33f6fd2449aa3f1e120969fc38ebab");

    #[test]
    fn address_bound_root_is_unchanged() -> eyre::Result<()> {
        let address = IrysAddress::from([0x11_u8; 20]);
        let chunks = [vec![0x01_u8; 32], vec![0x02_u8; 32], vec![0x03_u8; 7]];
        let root = super::generate_ingress_proof_tree(
            chunks.iter().map(|chunk| Ok(chunk.as_slice())),
            address,
        )?;
        assert_eq!(root.id, ADDRESS_BOUND_ROOT_FOR_FIXED_CHUNKS);
        Ok(())
    }

    #[test]
    fn compact_leaves_match_full_body_proof_with_partial_final_chunk() -> eyre::Result<()> {
        let config = ConsensusConfig::testing();
        let signer = IrysSigner::random_signer(&config);
        let chunks = [vec![1_u8; 32], vec![2_u8; 32], vec![3_u8; 7]];
        let data = chunks.concat();
        let geometry = config.data_tree_geometry()?;
        let commitment = upload_commitment(&geometry, &data);
        let anchor = H256::random();

        let full = generate_ingress_proof(
            &signer,
            commitment,
            chunks.iter().map(|chunk| Ok(chunk.as_slice())),
            17,
            anchor,
        )?;
        let mut boundary = 0_u64;
        let compact = chunks
            .iter()
            .map(|chunk| {
                boundary += chunk.len() as u64;
                let persisted_hash = generate_ingress_data_hash(chunk, signer.address());
                generate_ingress_leaf_from_data_hash(persisted_hash, boundary)
            })
            .collect::<eyre::Result<Vec<_>>>()?;
        let from_compact =
            generate_ingress_proof_from_leaves(&signer, commitment, compact, 17, anchor)?;

        assert_eq!(from_compact, full);
        assert!(verify_ingress_proof(&from_compact, &chunks, 17, &geometry)?);
        Ok(())
    }

    #[test]
    fn ingress_proof_signs_the_commitment() -> eyre::Result<()> {
        let config = ConsensusConfig::testing();
        let geometry = config.data_tree_geometry()?;
        let signer = IrysSigner::random_signer(&config);
        let chunks = [vec![1_u8; 32], vec![2_u8; 32]];
        let data_tree_root = DataTreeRoot(H256::from([9_u8; 32]));
        let commitment = data_commitment(&geometry, data_tree_root, 64);
        let other_size = data_commitment(&geometry, data_tree_root, 63);

        let proof = generate_ingress_proof(
            &signer,
            commitment,
            chunks.iter().map(|chunk| Ok(chunk.as_slice())),
            1,
            H256::random(),
        )?;

        assert_eq!(proof.pre_validate(&commitment)?, signer.address());
        assert!(proof.pre_validate(&other_size).is_err());

        // The signature covers the commitment: a proof whose commitment is
        // changed after signing no longer recovers the signer.
        let super::IngressProof::V1(mut tampered) = proof;
        tampered.data_commitment = other_size;
        let tampered = super::IngressProof::V1(tampered);
        assert!(!matches!(
            tampered.pre_validate(&other_size),
            Ok(address) if address == signer.address()
        ));
        assert!(!matches!(
            tampered.recover_signer(),
            Ok(address) if address == signer.address()
        ));
        Ok(())
    }

    #[test]
    fn ingress_proof_verifies_against_the_v2_commitment() -> eyre::Result<()> {
        let config = ConsensusConfig::testing();
        let geometry = config.data_tree_geometry()?;
        let signer = IrysSigner::random_signer(&config);
        let data: Vec<u8> = (0..(geometry.chunk_size() * 2 + 9))
            .map(|byte| byte as u8)
            .collect();
        let tx = signer.create_transaction(data, H256::zero())?;
        let commitment = tx.header.data_commitment(&geometry);
        let mut chunks: Vec<Vec<u8>> = tx
            .data_chunks()?
            .into_iter()
            .map(|chunk| chunk.bytes.0)
            .collect();

        let proof = generate_ingress_proof(
            &signer,
            commitment,
            chunks.iter().map(|chunk| Ok(chunk.as_slice())),
            config.chain_id,
            H256::random(),
        )?;
        assert!(verify_ingress_proof(
            &proof,
            &chunks,
            config.chain_id,
            &geometry
        )?);

        // Same chunks, so the same address-bound root. A proof signed over
        // the commitment of another size for the same root does not verify.
        let other_size =
            data_commitment(&geometry, tx.header.data_tree_root, tx.header.data_size + 1);
        let other_size_proof = generate_ingress_proof(
            &signer,
            other_size,
            chunks.iter().map(|chunk| Ok(chunk.as_slice())),
            config.chain_id,
            H256::random(),
        )?;
        assert_eq!(other_size_proof.proof, proof.proof);
        assert!(!verify_ingress_proof(
            &other_size_proof,
            &chunks,
            config.chain_id,
            &geometry
        )?);

        chunks[1][0] ^= 1;
        assert!(!verify_ingress_proof(
            &proof,
            &chunks,
            config.chain_id,
            &geometry
        )?);
        Ok(())
    }

    #[test]
    fn basic() -> eyre::Result<()> {
        // Create some random data
        let testing_config = ConsensusConfig::testing();
        let data_size = (testing_config.chunk_size as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0_u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        // Build the data tree and its commitment from the data
        let geometry = testing_config.data_tree_geometry()?;
        let commitment = upload_commitment(&geometry, &data_bytes);

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
            commitment,
            chunks.iter().map(|c| Ok(c.as_slice())),
            chain_id,
            anchor,
        )?;

        // Verify the ingress proof
        assert!(verify_ingress_proof(
            &proof,
            chunks.iter().as_slice(),
            chain_id,
            &geometry
        )?);
        // Reversed, the short last chunk comes first, so the chunks are not
        // an upload under the geometry and the proof does not verify.
        let mut reversed = chunks;
        reversed.reverse();
        assert!(!matches!(
            verify_ingress_proof(&proof, reversed.iter().as_slice(), chain_id, &geometry),
            Ok(true)
        ));

        Ok(())
    }

    #[test]
    fn test_chain_id_prevents_replay_attack() -> eyre::Result<()> {
        // Create some random data
        let testing_config = ConsensusConfig::testing();
        let data_size = (testing_config.chunk_size as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0_u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        // Build the data tree and its commitment from the data
        let geometry = testing_config.data_tree_geometry()?;
        let commitment = upload_commitment(&geometry, &data_bytes);

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
            commitment,
            chunks.iter().map(|c| Ok(c.as_slice())),
            testnet_chain_id,
            anchor,
        )?;

        // Generate proof for mainnet (chain_id = 2)
        let mainnet_chain_id = 2;
        let mainnet_proof = generate_ingress_proof(
            &signer,
            commitment,
            chunks.iter().map(|c| Ok(c.as_slice())),
            mainnet_chain_id,
            anchor,
        )?;

        // Verify that testnet proof is valid for testnet
        assert!(verify_ingress_proof(
            &testnet_proof,
            chunks.iter().as_slice(),
            testnet_chain_id,
            &geometry
        )?);

        // Verify that mainnet proof is valid for mainnet
        assert!(verify_ingress_proof(
            &mainnet_proof,
            chunks.iter().as_slice(),
            mainnet_chain_id,
            &geometry
        )?);

        // Create a modified proof where we try to use testnet proof with mainnet chain_id
        let mut replay_attack_proof = testnet_proof;
        replay_attack_proof.chain_id = mainnet_chain_id;

        // This should fail verification because the signature was created with testnet chain_id
        // but we're trying to verify it with mainnet chain_id
        assert!(!verify_ingress_proof(
            &replay_attack_proof,
            chunks.iter().as_slice(),
            mainnet_chain_id,
            &geometry
        )?);

        // This should fail verification because there's going to be a mismatch in chain_id
        // even if the proof is valid for testnet
        assert!(!verify_ingress_proof(
            &replay_attack_proof,
            chunks.iter().as_slice(),
            testnet_chain_id,
            &geometry
        )?);

        Ok(())
    }
}

#[cfg(test)]
mod prop_ingress_tests {
    use super::*;
    use alloy_rlp::Decodable as _;
    use proptest::prelude::*;

    fn arb_ingress_proof_v1() -> impl Strategy<Value = IngressProofV1> {
        (
            proptest::array::uniform32(any::<u8>()),
            proptest::array::uniform32(any::<u8>()),
            any::<u64>(),
            proptest::array::uniform32(any::<u8>()),
        )
            .prop_map(
                |(data_commitment, proof, chain_id, anchor)| IngressProofV1 {
                    signature: IrysSignature::default(),
                    data_commitment: DataCommitment(H256(data_commitment)),
                    proof: H256(proof),
                    chain_id,
                    anchor: H256(anchor),
                },
            )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn prop_ingress_proof_rlp_roundtrip(
            inner in arb_ingress_proof_v1(),
        ) {
            let original = IngressProof::V1(inner);
            let mut buf = bytes::BytesMut::new();
            original.encode(&mut buf);
            let mut slice = buf.as_ref();
            let decoded = IngressProof::decode(&mut slice).unwrap();
            prop_assert_eq!(&original, &decoded);
            prop_assert!(slice.is_empty(), "trailing bytes after decode");
        }

        #[test]
        fn prop_ingress_proof_rlp_strips_signature(
            data_commitment in proptest::array::uniform32(any::<u8>()),
            proof_hash in proptest::array::uniform32(any::<u8>()),
            chain_id in any::<u64>(),
            anchor in proptest::array::uniform32(any::<u8>()),
        ) {
            let non_default_sig = IrysSignature::new(
                crate::Signature::test_signature()
            );
            let with_sig = IngressProof::V1(IngressProofV1 {
                signature: non_default_sig,
                data_commitment: DataCommitment(H256(data_commitment)),
                proof: H256(proof_hash),
                chain_id,
                anchor: H256(anchor),
            });
            let without_sig = IngressProof::V1(IngressProofV1 {
                signature: IrysSignature::default(),
                data_commitment: DataCommitment(H256(data_commitment)),
                proof: H256(proof_hash),
                chain_id,
                anchor: H256(anchor),
            });

            prop_assert_ne!(with_sig.signature, without_sig.signature,
                "precondition: signatures must differ");

            let mut buf1 = bytes::BytesMut::new();
            let mut buf2 = bytes::BytesMut::new();
            with_sig.encode(&mut buf1);
            without_sig.encode(&mut buf2);

            prop_assert_eq!(buf1.as_ref(), buf2.as_ref(),
                "encoding should be identical regardless of signature");

            let mut slice = buf1.as_ref();
            let decoded = IngressProof::decode(&mut slice).unwrap();
            prop_assert_eq!(decoded.signature, IrysSignature::default());
        }

        #[test]
        fn prop_ingress_proof_compact_roundtrip(
            inner in arb_ingress_proof_v1(),
        ) {
            let original = IngressProof::V1(inner);
            let mut buf = Vec::new();
            let len = original.to_compact(&mut buf);
            let (decoded, remaining) = IngressProof::from_compact(&buf, len);
            prop_assert_eq!(&original, &decoded);
            prop_assert!(remaining.is_empty(), "trailing bytes after compact decode");
        }

        #[test]
        fn prop_ingress_proof_rlp_length_consistency(
            inner in arb_ingress_proof_v1(),
        ) {
            let proof = IngressProof::V1(inner);
            let reported_len = proof.length();
            let mut buf = bytes::BytesMut::new();
            proof.encode(&mut buf);
            prop_assert_eq!(
                reported_len, buf.len(),
                "Encodable::length() disagrees with actual encoded size"
            );
        }

    }
}
