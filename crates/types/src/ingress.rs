use crate::irys::IrysSigner;
use crate::kzg::KzgCommitmentBytes;
use crate::{
    decode_rlp_version, encode_rlp_version, generate_data_root, generate_ingress_leaves, DataRoot,
    IrysAddress, IrysSignature, Node, Signable, VersionDiscriminant, Versioned, H256,
};
use alloy_primitives::ChainId;
use alloy_rlp::Encodable as _;
use arbitrary::Arbitrary;
use bytes::BufMut;
use eyre::OptionExt as _;
use irys_macros_integer_tagged::IntegerTagged;
use reth_codecs::Compact;
use reth_db::DatabaseError;
use reth_db_api::table::{Compress, Decompress};
use reth_primitives::transaction::recover_signer;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, IntegerTagged, Eq, Compact, Arbitrary)]
#[repr(u8)]
#[integer_tagged(tag = "version")]
pub enum IngressProof {
    #[integer_tagged(version = 1)]
    V1(IngressProofV1) = 1,
    #[integer_tagged(version = 2)]
    V2(IngressProofV2) = 2,
}

impl Default for IngressProof {
    fn default() -> Self {
        Self::V1(Default::default())
    }
}

impl alloy_rlp::Encodable for IngressProof {
    fn encode(&self, out: &mut dyn BufMut) {
        let mut buf = Vec::new();
        match self {
            Self::V1(inner) => inner.encode(&mut buf),
            Self::V2(inner) => inner.encode(&mut buf),
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
            2 => {
                let inner = IngressProofV2::decode(inner_buf)?;
                Ok(Self::V2(inner))
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
            Self::V2(_) => 2,
        }
    }
}

impl IngressProof {
    pub fn data_root(&self) -> H256 {
        match self {
            Self::V1(v1) => v1.data_root,
            Self::V2(v2) => v2.data_root,
        }
    }

    pub fn chain_id(&self) -> ChainId {
        match self {
            Self::V1(v1) => v1.chain_id,
            Self::V2(v2) => v2.chain_id,
        }
    }

    pub fn anchor(&self) -> H256 {
        match self {
            Self::V1(v1) => v1.anchor,
            Self::V2(v2) => v2.anchor,
        }
    }

    pub fn signature(&self) -> &IrysSignature {
        match self {
            Self::V1(v1) => &v1.signature,
            Self::V2(v2) => &v2.signature,
        }
    }

    pub fn signature_mut(&mut self) -> &mut IrysSignature {
        match self {
            Self::V1(v1) => &mut v1.signature,
            Self::V2(v2) => &mut v2.signature,
        }
    }

    pub fn set_anchor(&mut self, anchor: H256) {
        match self {
            Self::V1(v1) => v1.anchor = anchor,
            Self::V2(v2) => v2.anchor = anchor,
        }
    }

    /// Returns the V1 merkle proof hash, or V2 composite commitment.
    /// Used as a unique proof identifier (e.g. for gossip deduplication).
    pub fn proof_id(&self) -> H256 {
        match self {
            Self::V1(v1) => v1.proof,
            Self::V2(v2) => v2.composite_commitment,
        }
    }

    pub fn recover_signer(&self) -> eyre::Result<IrysAddress> {
        let prehash = self.signature_hash();
        self.signature().recover_signer(prehash)
    }

    /// Validates that the proof matches the provided data_root and recovers the signer address
    pub fn pre_validate(&self, data_root: &H256) -> eyre::Result<IrysAddress> {
        if self.data_root() != *data_root {
            return Err(eyre::eyre!("Ingress proof data_root mismatch"));
        }
        self.recover_signer()
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[repr(u8)]
pub enum DataSourceType {
    #[default]
    NativeData = 0,
    EvmBlob = 1,
}

impl DataSourceType {
    pub fn from_u8(val: u8) -> Self {
        match val {
            0 => Self::NativeData,
            1 => Self::EvmBlob,
            _ => Self::NativeData,
        }
    }
}

impl<'a> Arbitrary<'a> for DataSourceType {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(Self::from_u8(u.int_in_range(0..=1)?))
    }
}

impl Compact for DataSourceType {
    fn to_compact<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) -> usize {
        buf.put_u8(*self as u8);
        1
    }

    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        (Self::from_u8(buf[0]), &buf[1..])
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngressProofV2 {
    pub signature: IrysSignature,
    pub data_root: H256,
    pub kzg_commitment: KzgCommitmentBytes,
    pub composite_commitment: H256,
    pub chain_id: ChainId,
    pub anchor: H256,
    pub source_type: DataSourceType,
}

impl Compact for IngressProofV2 {
    fn to_compact<B: BufMut + AsMut<[u8]>>(&self, buf: &mut B) -> usize {
        let mut flags = 0_usize;
        // signature has no flag — always present, written first
        flags += self.signature.to_compact(buf);
        flags += self.data_root.to_compact(buf);
        flags += self.kzg_commitment.to_compact(buf);
        flags += self.composite_commitment.to_compact(buf);
        flags += self.chain_id.to_compact(buf);
        flags += self.anchor.to_compact(buf);
        flags += self.source_type.to_compact(buf);
        flags
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (signature, buf) = IrysSignature::from_compact(buf, len);
        let (data_root, buf) = H256::from_compact(buf, buf.len());
        let (kzg_commitment, buf) = KzgCommitmentBytes::from_compact(buf, buf.len());
        let (composite_commitment, buf) = H256::from_compact(buf, buf.len());
        let (chain_id, buf) = ChainId::from_compact(buf, buf.len());
        let (anchor, buf) = H256::from_compact(buf, buf.len());
        let (source_type, buf) = DataSourceType::from_compact(buf, buf.len());
        (
            Self {
                signature,
                data_root,
                kzg_commitment,
                composite_commitment,
                chain_id,
                anchor,
                source_type,
            },
            buf,
        )
    }
}

impl Arbitrary<'_> for IngressProofV2 {
    fn arbitrary(u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Self> {
        Ok(Self {
            signature: u.arbitrary()?,
            data_root: u.arbitrary()?,
            kzg_commitment: u.arbitrary()?,
            composite_commitment: u.arbitrary()?,
            chain_id: u.arbitrary()?,
            anchor: u.arbitrary()?,
            source_type: u.arbitrary()?,
        })
    }
}

impl Versioned for IngressProofV2 {
    const VERSION: u8 = 2;
}

impl alloy_rlp::Encodable for IngressProofV2 {
    fn encode(&self, out: &mut dyn BufMut) {
        let header = alloy_rlp::Header {
            list: true,
            payload_length: self.data_root.length()
                + self.kzg_commitment.length()
                + self.composite_commitment.length()
                + self.chain_id.length()
                + self.anchor.length()
                + (self.source_type as u8).length(),
        };
        header.encode(out);
        self.data_root.encode(out);
        self.kzg_commitment.encode(out);
        self.composite_commitment.encode(out);
        self.chain_id.encode(out);
        self.anchor.encode(out);
        (self.source_type as u8).encode(out);
    }
}

impl alloy_rlp::Decodable for IngressProofV2 {
    fn decode(buf: &mut &[u8]) -> Result<Self, alloy_rlp::Error> {
        let header = alloy_rlp::Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        let data_root = alloy_rlp::Decodable::decode(buf)?;
        let kzg_commitment = alloy_rlp::Decodable::decode(buf)?;
        let composite_commitment = alloy_rlp::Decodable::decode(buf)?;
        let chain_id = alloy_rlp::Decodable::decode(buf)?;
        let anchor = alloy_rlp::Decodable::decode(buf)?;
        let source_type_u8: u8 = alloy_rlp::Decodable::decode(buf)?;
        Ok(Self {
            signature: Default::default(),
            data_root,
            kzg_commitment,
            composite_commitment,
            chain_id,
            anchor,
            source_type: DataSourceType::from_u8(source_type_u8),
        })
    }
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

/// Generate a V2 ingress proof with KZG commitment for native Irys data.
///
/// Unlike V1 which only hashes chunks into a merkle tree, V2 also computes
/// a KZG commitment over the chunk data and binds it to the signer's address
/// via a composite commitment.
pub fn generate_ingress_proof_v2(
    signer: &IrysSigner,
    data_root: DataRoot,
    chunks: &[impl AsRef<[u8]>],
    chain_id: u64,
    anchor: H256,
    kzg_settings: &c_kzg::KzgSettings,
) -> eyre::Result<IngressProof> {
    use crate::kzg::{
        aggregate_all_commitments, compute_chunk_commitment, compute_composite_commitment,
        KzgCommitmentBytes,
    };

    // Step 1: Compute per-chunk KZG commitments and aggregate
    let chunk_commitments: Vec<c_kzg::KzgCommitment> = chunks
        .iter()
        .map(|chunk| compute_chunk_commitment(chunk.as_ref(), kzg_settings))
        .collect::<eyre::Result<Vec<_>>>()?;

    let aggregated = aggregate_all_commitments(&chunk_commitments)?;
    let kzg_bytes: [u8; 48] = aggregated
        .as_ref()
        .try_into()
        .map_err(|_| eyre::eyre!("KZG commitment is not 48 bytes"))?;

    // Step 2: Compute composite commitment binding KZG to signer
    let composite = compute_composite_commitment(&kzg_bytes, &signer.address());

    // Step 3: Build and sign the V2 proof
    let mut proof = IngressProof::V2(IngressProofV2 {
        signature: Default::default(),
        data_root,
        kzg_commitment: KzgCommitmentBytes::from(kzg_bytes),
        composite_commitment: composite,
        chain_id,
        anchor,
        source_type: DataSourceType::NativeData,
    });

    signer.sign_ingress_proof(&mut proof)?;
    Ok(proof)
}

pub fn verify_ingress_proof<C: AsRef<[u8]>>(
    proof: &IngressProof,
    chunks: impl IntoIterator<Item = C>,
    chain_id: ChainId,
) -> eyre::Result<bool> {
    if chain_id != proof.chain_id() {
        return Ok(false);
    }

    match proof {
        IngressProof::V1(_) => {
            let sig = proof.signature().as_bytes();
            let prehash = proof.signature_hash();
            let recovered_address = recover_signer(&sig[..].try_into()?, prehash.into())?;

            let (proof_root, regular_root) = generate_ingress_proof_tree(
                chunks.into_iter().map(Ok),
                recovered_address.into(),
                true,
            )?;

            let data_root = H256(
                regular_root
                    .ok_or_eyre("expected regular_root to be Some")?
                    .id,
            );

            let new_prehash = IngressProof::V1(IngressProofV1 {
                signature: Default::default(),
                data_root,
                proof: H256(proof_root.id),
                chain_id,
                anchor: proof.anchor(),
            })
            .signature_hash();

            Ok(new_prehash == prehash)
        }
        IngressProof::V2(v2) => {
            let sig = v2.signature.as_bytes();
            let prehash = proof.signature_hash();
            let recovered_address: IrysAddress =
                recover_signer(&sig[..].try_into()?, prehash.into())?.into();

            let settings = crate::kzg::default_kzg_settings();
            let chunks_vec: Vec<_> = chunks.into_iter().collect();
            let chunk_commitments: Vec<c_kzg::KzgCommitment> = chunks_vec
                .iter()
                .map(|c| crate::kzg::compute_chunk_commitment(c.as_ref(), settings))
                .collect::<eyre::Result<Vec<_>>>()?;

            let aggregated = crate::kzg::aggregate_all_commitments(&chunk_commitments)?;
            let kzg_bytes: [u8; 48] = aggregated
                .as_ref()
                .try_into()
                .map_err(|_| eyre::eyre!("KZG commitment is not 48 bytes"))?;

            if kzg_bytes != v2.kzg_commitment.0 {
                return Ok(false);
            }

            let expected_composite =
                crate::kzg::compute_composite_commitment(&kzg_bytes, &recovered_address);
            Ok(expected_composite == v2.composite_commitment)
        }
    }
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

    use super::{generate_ingress_proof, generate_ingress_proof_v2};

    /// Generate KZG-safe data: each 32-byte field element's first byte must be < 0x74.
    /// Uses a simple fill value that satisfies the BLS12-381 modulus constraint.
    fn kzg_safe_data(size: usize, fill: u8) -> Vec<u8> {
        assert!(fill < 0x74, "fill byte must be < 0x74 for KZG safety");
        vec![fill; size]
    }

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
        match &mut replay_attack_proof {
            IngressProof::V1(v1) => v1.chain_id = mainnet_chain_id,
            IngressProof::V2(v2) => v2.chain_id = mainnet_chain_id,
        }

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

    #[test]
    fn v2_generate_and_verify_roundtrip() -> eyre::Result<()> {
        let config = ConsensusConfig::testing();
        let chunk_size = config.chunk_size as usize;
        let data_bytes = kzg_safe_data((chunk_size as f64 * 2.5).round() as usize, 42);

        let leaves = generate_leaves(vec![data_bytes.clone()].into_iter().map(Ok), chunk_size)?;
        let root = generate_data_root(leaves)?;
        let data_root = H256(root.id);

        let signer = IrysSigner::random_signer(&config);
        let chunks: Vec<Vec<u8>> = data_bytes.chunks(chunk_size).map(Vec::from).collect();

        let chain_id = 1_u64;
        let anchor = H256::random();
        let kzg_settings = crate::kzg::default_kzg_settings();

        let proof =
            generate_ingress_proof_v2(&signer, data_root, &chunks, chain_id, anchor, kzg_settings)?;

        assert!(matches!(proof, IngressProof::V2(_)));
        assert!(verify_ingress_proof(&proof, chunks.iter(), chain_id)?);

        Ok(())
    }

    #[test]
    fn v2_wrong_chunks_fails_verification() -> eyre::Result<()> {
        let config = ConsensusConfig::testing();
        let chunk_size = config.chunk_size as usize;
        let data_bytes = kzg_safe_data((chunk_size as f64 * 2.5).round() as usize, 42);

        let leaves = generate_leaves(vec![data_bytes.clone()].into_iter().map(Ok), chunk_size)?;
        let root = generate_data_root(leaves)?;
        let data_root = H256(root.id);

        let signer = IrysSigner::random_signer(&config);
        let chunks: Vec<Vec<u8>> = data_bytes.chunks(chunk_size).map(Vec::from).collect();

        let chain_id = 1_u64;
        let anchor = H256::random();
        let kzg_settings = crate::kzg::default_kzg_settings();

        let proof =
            generate_ingress_proof_v2(&signer, data_root, &chunks, chain_id, anchor, kzg_settings)?;

        // Tampered chunk: use a different safe fill value
        let mut bad_chunks = chunks.clone();
        bad_chunks[0] = kzg_safe_data(bad_chunks[0].len(), 7);
        assert!(!verify_ingress_proof(&proof, bad_chunks.iter(), chain_id)?);

        // Reversed chunks should fail (only if >1 chunk)
        if chunks.len() > 1 {
            let mut reversed = chunks;
            reversed.reverse();
            assert!(!verify_ingress_proof(&proof, reversed.iter(), chain_id)?);
        }

        Ok(())
    }

    #[test]
    fn v2_wrong_chain_id_fails_verification() -> eyre::Result<()> {
        let config = ConsensusConfig::testing();
        let chunk_size = config.chunk_size as usize;
        let data_bytes = kzg_safe_data(chunk_size * 2, 42);

        let leaves = generate_leaves(vec![data_bytes.clone()].into_iter().map(Ok), chunk_size)?;
        let root = generate_data_root(leaves)?;
        let data_root = H256(root.id);

        let signer = IrysSigner::random_signer(&config);
        let chunks: Vec<Vec<u8>> = data_bytes.chunks(chunk_size).map(Vec::from).collect();

        let anchor = H256::random();
        let kzg_settings = crate::kzg::default_kzg_settings();

        let proof =
            generate_ingress_proof_v2(&signer, data_root, &chunks, 1, anchor, kzg_settings)?;

        assert!(!verify_ingress_proof(&proof, chunks.iter(), 2)?);

        Ok(())
    }

    #[test]
    fn v2_composite_commitment_binds_to_signer() -> eyre::Result<()> {
        let config = ConsensusConfig::testing();
        let chunk_size = config.chunk_size as usize;
        let data_bytes = kzg_safe_data(chunk_size * 2, 42);

        let leaves = generate_leaves(vec![data_bytes.clone()].into_iter().map(Ok), chunk_size)?;
        let root = generate_data_root(leaves)?;
        let data_root = H256(root.id);

        let signer_a = IrysSigner::random_signer(&config);
        let signer_b = IrysSigner::random_signer(&config);
        let chunks: Vec<Vec<u8>> = data_bytes.chunks(chunk_size).map(Vec::from).collect();

        let chain_id = 1_u64;
        let anchor = H256::random();
        let kzg_settings = crate::kzg::default_kzg_settings();

        let proof_a = generate_ingress_proof_v2(
            &signer_a,
            data_root,
            &chunks,
            chain_id,
            anchor,
            kzg_settings,
        )?;
        let proof_b = generate_ingress_proof_v2(
            &signer_b,
            data_root,
            &chunks,
            chain_id,
            anchor,
            kzg_settings,
        )?;

        // Same data → same KZG commitment, but different composite commitments
        let (kzg_a, composite_a) = match &proof_a {
            IngressProof::V2(v2) => (v2.kzg_commitment, v2.composite_commitment),
            _ => unreachable!(),
        };
        let (kzg_b, composite_b) = match &proof_b {
            IngressProof::V2(v2) => (v2.kzg_commitment, v2.composite_commitment),
            _ => unreachable!(),
        };

        assert_eq!(kzg_a, kzg_b);
        assert_ne!(composite_a, composite_b);

        assert!(verify_ingress_proof(&proof_a, chunks.iter(), chain_id)?);
        assert!(verify_ingress_proof(&proof_b, chunks.iter(), chain_id)?);

        Ok(())
    }

    #[test]
    fn v2_rlp_roundtrip() -> eyre::Result<()> {
        use bytes::BytesMut;

        let config = ConsensusConfig::testing();
        let chunk_size = config.chunk_size as usize;
        let data_bytes = kzg_safe_data(chunk_size, 42);

        let leaves = generate_leaves(vec![data_bytes.clone()].into_iter().map(Ok), chunk_size)?;
        let root = generate_data_root(leaves)?;
        let data_root = H256(root.id);

        let signer = IrysSigner::random_signer(&config);
        let chunks: Vec<Vec<u8>> = vec![data_bytes];
        let kzg_settings = crate::kzg::default_kzg_settings();

        let original = generate_ingress_proof_v2(
            &signer,
            data_root,
            &chunks,
            42,
            H256::random(),
            kzg_settings,
        )?;

        let mut buf = BytesMut::new();
        alloy_rlp::Encodable::encode(&original, &mut buf);
        let mut slice = buf.as_ref();
        let decoded = IngressProof::decode(&mut slice)?;

        match (&original, &decoded) {
            (IngressProof::V2(orig), IngressProof::V2(dec)) => {
                assert_eq!(orig.data_root, dec.data_root);
                assert_eq!(orig.kzg_commitment, dec.kzg_commitment);
                assert_eq!(orig.composite_commitment, dec.composite_commitment);
                assert_eq!(orig.chain_id, dec.chain_id);
                assert_eq!(orig.anchor, dec.anchor);
                assert_eq!(orig.source_type, dec.source_type);
            }
            _ => panic!("expected V2 proofs"),
        }

        Ok(())
    }

    #[test]
    fn v2_tampered_kzg_commitment_fails() -> eyre::Result<()> {
        let config = ConsensusConfig::testing();
        let chunk_size = config.chunk_size as usize;
        let data_bytes = kzg_safe_data(chunk_size * 2, 42);

        let leaves = generate_leaves(vec![data_bytes.clone()].into_iter().map(Ok), chunk_size)?;
        let root = generate_data_root(leaves)?;
        let data_root = H256(root.id);

        let signer = IrysSigner::random_signer(&config);
        let chunks: Vec<Vec<u8>> = data_bytes.chunks(chunk_size).map(Vec::from).collect();

        let kzg_settings = crate::kzg::default_kzg_settings();
        let mut proof = generate_ingress_proof_v2(
            &signer,
            data_root,
            &chunks,
            1,
            H256::random(),
            kzg_settings,
        )?;

        if let IngressProof::V2(ref mut v2) = proof {
            v2.kzg_commitment.0[0] ^= 0xFF;
        }

        assert!(!verify_ingress_proof(&proof, chunks.iter(), 1)?);

        Ok(())
    }
}
