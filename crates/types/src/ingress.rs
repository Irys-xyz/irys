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

    pub fn check_version_accepted(
        &self,
        accept_kzg: bool,
        require_kzg: bool,
    ) -> Result<(), &'static str> {
        match self {
            Self::V2(_) if !accept_kzg => Err("V2 proofs not accepted"),
            Self::V1(_) if require_kzg => Err("V1 proofs rejected (V2 required)"),
            _ => Ok(()),
        }
    }

    /// Unique identifier for gossip deduplication.
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

impl From<DataSourceType> for u8 {
    fn from(val: DataSourceType) -> Self {
        val as Self // safe: #[repr(u8)]
    }
}

impl TryFrom<u8> for DataSourceType {
    type Error = eyre::Report;

    fn try_from(val: u8) -> eyre::Result<Self> {
        match val {
            0 => Ok(Self::NativeData),
            1 => Ok(Self::EvmBlob),
            _ => Err(eyre::eyre!("unknown DataSourceType discriminant: {val}")),
        }
    }
}

impl<'a> Arbitrary<'a> for DataSourceType {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        Self::try_from(u.int_in_range(0..=1)?).map_err(|_| arbitrary::Error::IncorrectFormat)
    }
}

impl Compact for DataSourceType {
    fn to_compact<B: bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) -> usize {
        buf.put_u8(u8::from(*self));
        1
    }

    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        // Compact deserialization: default to NativeData for forward compatibility
        // with unknown discriminants in stored data
        let source = Self::try_from(buf[0]).unwrap_or_default();
        (source, &buf[1..])
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
        let mut written = 0_usize;
        written += self.signature.to_compact(buf);
        written += self.data_root.to_compact(buf);
        written += self.kzg_commitment.to_compact(buf);
        written += self.composite_commitment.to_compact(buf);
        written += self.chain_id.to_compact(buf);
        written += self.anchor.to_compact(buf);
        written += self.source_type.to_compact(buf);
        written
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

/// Signature excluded from RLP: this encoding is used for signature_hash computation
impl alloy_rlp::Encodable for IngressProofV2 {
    fn encode(&self, out: &mut dyn BufMut) {
        let header = alloy_rlp::Header {
            list: true,
            payload_length: self.data_root.length()
                + self.kzg_commitment.length()
                + self.composite_commitment.length()
                + self.chain_id.length()
                + self.anchor.length()
                + u8::from(self.source_type).length(),
        };
        header.encode(out);
        self.data_root.encode(out);
        self.kzg_commitment.encode(out);
        self.composite_commitment.encode(out);
        self.chain_id.encode(out);
        self.anchor.encode(out);
        u8::from(self.source_type).encode(out);
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
            source_type: DataSourceType::try_from(source_type_u8)
                .map_err(|_| alloy_rlp::Error::Custom("unknown DataSourceType discriminant"))?,
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

/// Generates KZG commitment over chunks and binds it to signer via composite commitment.
pub fn generate_ingress_proof_v2(
    signer: &IrysSigner,
    data_root: DataRoot,
    chunks: &[impl AsRef<[u8]>],
    chain_id: u64,
    anchor: H256,
    kzg_settings: &c_kzg::KzgSettings,
) -> eyre::Result<(IngressProof, Vec<KzgCommitmentBytes>)> {
    use crate::kzg::{
        aggregate_all_commitments, compute_chunk_commitment, compute_composite_commitment,
        KzgCommitmentBytes,
    };

    let chunk_commitments: Vec<c_kzg::KzgCommitment> = chunks
        .iter()
        .map(|chunk| compute_chunk_commitment(chunk.as_ref(), kzg_settings))
        .collect::<eyre::Result<Vec<_>>>()?;

    let per_chunk_bytes: Vec<KzgCommitmentBytes> = chunk_commitments
        .iter()
        .map(|c| {
            Ok(KzgCommitmentBytes::from(crate::kzg::commitment_to_bytes(
                c,
            )?))
        })
        .collect::<eyre::Result<Vec<_>>>()?;

    let aggregated = aggregate_all_commitments(&chunk_commitments)?;
    let kzg_bytes = crate::kzg::commitment_to_bytes(&aggregated)?;

    let composite = compute_composite_commitment(&kzg_bytes, &signer.address());

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
    Ok((proof, per_chunk_bytes))
}

/// Generate a V2 ingress proof for blob-derived data (EIP-4844).
///
/// The KZG commitment is taken directly from the blob transaction sidecar
/// rather than recomputed — the sidecar uses the same c-kzg trusted setup.
/// The blob data (128KB) is zero-padded to a 256KB Irys chunk for data_root
/// computation.
pub fn generate_ingress_proof_v2_from_blob(
    signer: &IrysSigner,
    blob_data: &[u8],
    kzg_commitment: &[u8; 48],
    chain_id: u64,
    anchor: H256,
) -> eyre::Result<IngressProof> {
    use crate::kzg::{compute_composite_commitment, KzgCommitmentBytes};

    let padded = crate::kzg::zero_pad_to_chunk_size(blob_data)?;

    // Use regular leaves (without signer) for data_root — consistent with native V2 path
    let (_, regular_leaves) = generate_ingress_leaves(
        std::iter::once(Ok(padded.as_slice())),
        signer.address(),
        true,
    )?;
    let root = generate_data_root(
        regular_leaves
            .ok_or_eyre("generate_ingress_leaves with and_regular=true must return Some")?,
    )?;

    let composite = compute_composite_commitment(kzg_commitment, &signer.address());

    let mut proof = IngressProof::V2(IngressProofV2 {
        signature: Default::default(),
        data_root: root.id.into(),
        kzg_commitment: KzgCommitmentBytes::from(*kzg_commitment),
        composite_commitment: composite,
        chain_id,
        anchor,
        source_type: DataSourceType::EvmBlob,
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
            let kzg_bytes = crate::kzg::commitment_to_bytes(&aggregated)?;

            if kzg_bytes != v2.kzg_commitment.0 {
                return Ok(false);
            }

            let (_, regular_leaves) = generate_ingress_leaves(
                chunks_vec.iter().map(|c| Ok(c.as_ref())),
                recovered_address,
                true,
            )?;
            let computed_root = generate_data_root(
                regular_leaves
                    .ok_or_eyre("generate_ingress_leaves with and_regular=true must return Some")?,
            )?;
            if H256(computed_root.id) != v2.data_root {
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

    use super::{
        generate_ingress_proof, generate_ingress_proof_v2, generate_ingress_proof_v2_from_blob,
    };

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

    fn test_chunk_size() -> usize {
        usize::try_from(ConsensusConfig::testing().chunk_size).expect("chunk_size fits in usize")
    }

    struct V2TestSetup {
        data_root: H256,
        signer: IrysSigner,
        chunks: Vec<Vec<u8>>,
        chain_id: u64,
        anchor: H256,
        kzg_settings: &'static c_kzg::KzgSettings,
    }

    impl V2TestSetup {
        fn new(byte_count: usize) -> eyre::Result<Self> {
            let config = ConsensusConfig::testing();
            let chunk_size = test_chunk_size();
            let data_bytes = kzg_safe_data(byte_count, 42);
            let leaves = generate_leaves(vec![data_bytes.clone()].into_iter().map(Ok), chunk_size)?;
            let root = generate_data_root(leaves)?;
            Ok(Self {
                data_root: H256(root.id),
                signer: IrysSigner::random_signer(&config),
                chunks: data_bytes.chunks(chunk_size).map(Vec::from).collect(),
                chain_id: 1,
                anchor: H256::random(),
                kzg_settings: crate::kzg::default_kzg_settings(),
            })
        }

        fn generate_proof(&self) -> eyre::Result<IngressProof> {
            let (proof, _per_chunk) = generate_ingress_proof_v2(
                &self.signer,
                self.data_root,
                &self.chunks,
                self.chain_id,
                self.anchor,
                self.kzg_settings,
            )?;
            Ok(proof)
        }
    }

    #[test]
    fn v2_generate_and_verify_roundtrip() -> eyre::Result<()> {
        let cs = test_chunk_size();
        let s = V2TestSetup::new(cs * 5 / 2)?;
        let proof = s.generate_proof()?;
        assert!(matches!(proof, IngressProof::V2(_)));
        assert!(verify_ingress_proof(&proof, s.chunks.iter(), s.chain_id)?);
        Ok(())
    }

    #[test]
    fn v2_wrong_chunks_fails_verification() -> eyre::Result<()> {
        let cs = test_chunk_size();
        let s = V2TestSetup::new(cs * 5 / 2)?;
        let proof = s.generate_proof()?;

        let mut bad_chunks = s.chunks.clone(); // clone: need original for reversed test
        bad_chunks[0] = kzg_safe_data(bad_chunks[0].len(), 7);
        assert!(!verify_ingress_proof(
            &proof,
            bad_chunks.iter(),
            s.chain_id
        )?);

        if s.chunks.len() > 1 {
            let mut reversed = s.chunks;
            reversed.reverse();
            assert!(!verify_ingress_proof(&proof, reversed.iter(), s.chain_id)?);
        }
        Ok(())
    }

    #[test]
    fn v2_wrong_chain_id_fails_verification() -> eyre::Result<()> {
        let s = V2TestSetup::new(test_chunk_size() * 2)?;
        let proof = s.generate_proof()?;
        assert!(!verify_ingress_proof(&proof, s.chunks.iter(), 2)?);
        Ok(())
    }

    #[test]
    fn v2_composite_commitment_binds_to_signer() -> eyre::Result<()> {
        let s = V2TestSetup::new(test_chunk_size() * 2)?;
        let signer_b = IrysSigner::random_signer(&ConsensusConfig::testing());

        let proof_a = s.generate_proof()?;
        let (proof_b, _) = generate_ingress_proof_v2(
            &signer_b,
            s.data_root,
            &s.chunks,
            s.chain_id,
            s.anchor,
            s.kzg_settings,
        )?;

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
        assert!(verify_ingress_proof(&proof_a, s.chunks.iter(), s.chain_id)?);
        assert!(verify_ingress_proof(&proof_b, s.chunks.iter(), s.chain_id)?);
        Ok(())
    }

    #[test]
    fn v2_rlp_roundtrip() -> eyre::Result<()> {
        use bytes::BytesMut;

        let s = V2TestSetup::new(test_chunk_size())?;
        let original = s.generate_proof()?;

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
        let s = V2TestSetup::new(test_chunk_size() * 2)?;
        let mut proof = s.generate_proof()?;

        if let IngressProof::V2(ref mut v2) = proof {
            v2.kzg_commitment.0[0] ^= 0xFF;
        }
        assert!(!verify_ingress_proof(&proof, s.chunks.iter(), s.chain_id)?);
        Ok(())
    }

    #[test]
    fn v2_blob_generate_and_verify_roundtrip() -> eyre::Result<()> {
        use crate::ingress::DataSourceType;

        let config = ConsensusConfig::testing();
        let signer = IrysSigner::random_signer(&config);
        let chain_id = 1_u64;
        let anchor = H256::random();

        // Simulate a 128KB EIP-4844 blob with KZG-safe data
        let blob_size = 131_072;
        let blob_data = kzg_safe_data(blob_size, 42);

        // Compute a real KZG commitment from the blob data (zero-padded to 256KB)
        let kzg_settings = crate::kzg::default_kzg_settings();
        let kzg_commitment = crate::kzg::compute_chunk_commitment(&blob_data, kzg_settings)?;
        let commitment_bytes: [u8; 48] = kzg_commitment.as_ref().try_into().unwrap();

        let proof = generate_ingress_proof_v2_from_blob(
            &signer,
            &blob_data,
            &commitment_bytes,
            chain_id,
            anchor,
        )?;

        // Source type must be EvmBlob
        match &proof {
            IngressProof::V2(v2) => {
                assert_eq!(v2.source_type, DataSourceType::EvmBlob);
                assert_eq!(v2.kzg_commitment.0, commitment_bytes);
            }
            _ => panic!("expected V2 proof"),
        }

        let padded = crate::kzg::zero_pad_to_chunk_size(&blob_data).unwrap();

        assert!(verify_ingress_proof(&proof, [padded.as_slice()], chain_id)?);

        Ok(())
    }

    #[test]
    fn v2_blob_wrong_data_fails_verification() -> eyre::Result<()> {
        let config = ConsensusConfig::testing();
        let signer = IrysSigner::random_signer(&config);
        let chain_id = 1_u64;

        let blob_data = kzg_safe_data(131_072, 42);
        let kzg_settings = crate::kzg::default_kzg_settings();
        let kzg_commitment = crate::kzg::compute_chunk_commitment(&blob_data, kzg_settings)?;
        let commitment_bytes: [u8; 48] = kzg_commitment.as_ref().try_into().unwrap();

        let proof = generate_ingress_proof_v2_from_blob(
            &signer,
            &blob_data,
            &commitment_bytes,
            chain_id,
            H256::random(),
        )?;

        // Verify with different data (wrong fill value) — should fail
        let bad_blob = kzg_safe_data(131_072, 7);
        let bad_padded = crate::kzg::zero_pad_to_chunk_size(&bad_blob).unwrap();

        assert!(!verify_ingress_proof(
            &proof,
            [bad_padded.as_slice()],
            chain_id
        )?);

        Ok(())
    }

    #[test]
    fn v2_blob_wrong_chain_id_fails() -> eyre::Result<()> {
        let config = ConsensusConfig::testing();
        let signer = IrysSigner::random_signer(&config);

        let blob_data = kzg_safe_data(131_072, 42);
        let kzg_settings = crate::kzg::default_kzg_settings();
        let kzg_commitment = crate::kzg::compute_chunk_commitment(&blob_data, kzg_settings)?;
        let commitment_bytes: [u8; 48] = kzg_commitment.as_ref().try_into().unwrap();

        let proof = generate_ingress_proof_v2_from_blob(
            &signer,
            &blob_data,
            &commitment_bytes,
            1,
            H256::random(),
        )?;

        let padded = crate::kzg::zero_pad_to_chunk_size(&blob_data).unwrap();

        // Verify with wrong chain_id — should fail
        assert!(!verify_ingress_proof(&proof, [padded.as_slice()], 2)?);

        Ok(())
    }
}
