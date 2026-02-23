use crate::{IrysAddress, H256};
use alloy_eips::eip4844::env_settings::EnvKzgSettings;
use bytes::BufMut;
use c_kzg::{Blob, KzgCommitment, KzgSettings};
use openssl::sha;
use reth_codecs::Compact;
use serde::{Deserialize, Serialize};

pub const BLOB_SIZE: usize = 131_072;
pub const CHUNK_SIZE_FOR_KZG: usize = 2 * BLOB_SIZE;
pub const COMMITMENT_SIZE: usize = 48;
pub const PROOF_SIZE: usize = 48;
pub const SCALAR_SIZE: usize = 32;
pub const DOMAIN_SEPARATOR: &[u8] = b"IRYS_KZG_INGRESS_V1";

/// A 48-byte KZG commitment (compressed BLS12-381 G1 point).
///
/// Newtype wrapper around `[u8; 48]` providing the trait implementations
/// that raw arrays lack (serde for N>32, Default for N>32, Compact).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct KzgCommitmentBytes(pub [u8; COMMITMENT_SIZE]);

impl Default for KzgCommitmentBytes {
    fn default() -> Self {
        Self([0_u8; COMMITMENT_SIZE])
    }
}

impl std::fmt::Debug for KzgCommitmentBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "0x")?;
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl std::ops::Deref for KzgCommitmentBytes {
    type Target = [u8; COMMITMENT_SIZE];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<[u8; COMMITMENT_SIZE]> for KzgCommitmentBytes {
    fn as_ref(&self) -> &[u8; COMMITMENT_SIZE] {
        &self.0
    }
}

impl From<[u8; COMMITMENT_SIZE]> for KzgCommitmentBytes {
    fn from(bytes: [u8; COMMITMENT_SIZE]) -> Self {
        Self(bytes)
    }
}

impl From<KzgCommitmentBytes> for [u8; COMMITMENT_SIZE] {
    fn from(val: KzgCommitmentBytes) -> Self {
        val.0
    }
}

impl Serialize for KzgCommitmentBytes {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            let mut s = String::with_capacity(2 + COMMITMENT_SIZE * 2);
            s.push_str("0x");
            s.push_str(&alloy_primitives::hex::encode(self.0));
            serializer.serialize_str(&s)
        } else {
            serializer.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for KzgCommitmentBytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        fn bytes_to_commitment<E: serde::de::Error>(
            bytes: Vec<u8>,
        ) -> Result<[u8; COMMITMENT_SIZE], E> {
            bytes.try_into().map_err(|v: Vec<u8>| {
                E::custom(format!("expected {COMMITMENT_SIZE} bytes, got {}", v.len()))
            })
        }

        if deserializer.is_human_readable() {
            let s = String::deserialize(deserializer)?;
            let s = s.strip_prefix("0x").unwrap_or(&s);
            let bytes = alloy_primitives::hex::decode(s).map_err(serde::de::Error::custom)?;
            Ok(Self(bytes_to_commitment::<D::Error>(bytes)?))
        } else {
            let bytes = <Vec<u8>>::deserialize(deserializer)?;
            Ok(Self(bytes_to_commitment::<D::Error>(bytes)?))
        }
    }
}

impl Compact for KzgCommitmentBytes {
    fn to_compact<B: BufMut + AsMut<[u8]>>(&self, buf: &mut B) -> usize {
        self.0.to_compact(buf)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (arr, rest) = <[u8; COMMITMENT_SIZE]>::from_compact(buf, len);
        (Self(arr), rest)
    }
}

impl arbitrary::Arbitrary<'_> for KzgCommitmentBytes {
    fn arbitrary(u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Self> {
        let bytes: [u8; COMMITMENT_SIZE] = u.arbitrary()?;
        Ok(Self(bytes))
    }
}

impl alloy_rlp::Encodable for KzgCommitmentBytes {
    fn encode(&self, out: &mut dyn BufMut) {
        self.0.encode(out);
    }

    fn length(&self) -> usize {
        self.0.length()
    }
}

impl alloy_rlp::Decodable for KzgCommitmentBytes {
    fn decode(buf: &mut &[u8]) -> Result<Self, alloy_rlp::Error> {
        let arr = <[u8; COMMITMENT_SIZE]>::decode(buf)?;
        Ok(Self(arr))
    }
}

/// A single chunk's KZG commitment stored during ingress.
/// Maps (data_root, chunk_index) → KzgCommitmentBytes in the database.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, Compact)]
pub struct PerChunkCommitment {
    pub chunk_index: u32,
    pub commitment: KzgCommitmentBytes,
}

impl arbitrary::Arbitrary<'_> for PerChunkCommitment {
    fn arbitrary(u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Self> {
        Ok(Self {
            chunk_index: u.arbitrary()?,
            commitment: u.arbitrary()?,
        })
    }
}

/// Returns a reference to the default (Ethereum mainnet) trusted setup KZG settings.
/// Lazily initialized on first call, thread-safe.
pub fn default_kzg_settings() -> &'static KzgSettings {
    EnvKzgSettings::Default.get()
}

/// Compute a KZG commitment for a single 128KB blob (4096 field elements).
///
/// `data` must be exactly [`BLOB_SIZE`] bytes. If the data is shorter, the caller
/// must zero-pad it before calling this function.
pub fn compute_blob_commitment(
    data: &[u8; BLOB_SIZE],
    settings: &KzgSettings,
) -> eyre::Result<KzgCommitment> {
    let blob = Blob::new(*data);
    settings
        .blob_to_kzg_commitment(&blob)
        .map_err(|e| eyre::eyre!("KZG blob commitment failed: {e}"))
}

/// Aggregate two G1 commitments: C = C1 + r·C2 where r = SHA256(C1 || C2)
/// interpreted as a BLS12-381 scalar.
///
/// Uses the `blst` library (transitive dependency via `c-kzg`) for elliptic
/// curve point operations on BLS12-381 G1.
pub fn aggregate_commitments(
    c1: &KzgCommitment,
    c2: &KzgCommitment,
) -> eyre::Result<KzgCommitment> {
    use blst::min_pk::PublicKey;
    use blst::{blst_p1, blst_p1_affine, blst_scalar};

    let mut hasher = sha::Sha256::new();
    hasher.update(c1.as_ref());
    hasher.update(c2.as_ref());
    let r_bytes = hasher.finish();

    let mut r_scalar = blst_scalar::default();
    // SAFETY: `r_bytes` is a 32-byte SHA256 digest; `blst_scalar_from_bendian` reads
    // exactly 32 bytes from the pointer, which is within bounds.
    unsafe {
        blst::blst_scalar_from_bendian(&mut r_scalar, r_bytes.as_ptr());
    }

    let p1 = PublicKey::from_bytes(c1.as_ref())
        .map_err(|e| eyre::eyre!("failed to decompress C1: {e:?}"))?;
    let p2 = PublicKey::from_bytes(c2.as_ref())
        .map_err(|e| eyre::eyre!("failed to decompress C2: {e:?}"))?;

    let p1_affine: &blst_p1_affine = (&p1).into();
    let p2_affine: &blst_p1_affine = (&p2).into();

    let mut p2_proj = blst_p1::default();
    let mut r_c2 = blst_p1::default();
    // SAFETY: All blst_p1 types are initialized via `default()`. `blst_p1_from_affine`
    // converts a valid affine point (from `PublicKey::from_bytes` which validated the
    // curve point) to projective form. `blst_p1_mult` multiplies a valid projective
    // point by a 256-bit scalar — both inputs are well-formed.
    unsafe {
        blst::blst_p1_from_affine(&mut p2_proj, p2_affine);
        blst::blst_p1_mult(&mut r_c2, &p2_proj, r_scalar.b.as_ptr(), 256);
    }

    let mut result = blst_p1::default();
    // SAFETY: `c1_proj` is initialised from a validated affine point. `r_c2` is the
    // result of a valid scalar multiplication. `blst_p1_add` adds two projective points.
    unsafe {
        let mut c1_proj = blst_p1::default();
        blst::blst_p1_from_affine(&mut c1_proj, p1_affine);
        blst::blst_p1_add(&mut result, &c1_proj, &r_c2);
    }

    let mut compressed = [0_u8; COMMITMENT_SIZE];
    // SAFETY: `result` is a valid projective G1 point from the addition above.
    // `compressed` is a 48-byte buffer matching the compressed G1 point size.
    unsafe {
        blst::blst_p1_compress(compressed.as_mut_ptr(), &result);
    }

    Ok(KzgCommitment::from(compressed))
}

/// Compute the aggregated KZG commitment for a 256KB native Irys chunk.
///
/// Splits the chunk into two 128KB halves, commits each half as a separate
/// blob, then aggregates: C = C1 + r·C2 where r = SHA256(C1 || C2).
///
/// If `chunk_data` is shorter than [`CHUNK_SIZE_FOR_KZG`], it is zero-padded.
/// If it is longer, returns an error.
pub fn compute_chunk_commitment(
    chunk_data: &[u8],
    settings: &KzgSettings,
) -> eyre::Result<KzgCommitment> {
    if chunk_data.len() > CHUNK_SIZE_FOR_KZG {
        return Err(eyre::eyre!(
            "chunk data too large: {} bytes (max {})",
            chunk_data.len(),
            CHUNK_SIZE_FOR_KZG
        ));
    }

    let mut padded = [0_u8; CHUNK_SIZE_FOR_KZG];
    padded[..chunk_data.len()].copy_from_slice(chunk_data);

    let (first_half, second_half) = padded.split_at(BLOB_SIZE);
    let first_half: &[u8; BLOB_SIZE] = first_half
        .try_into()
        .expect("split_at guarantees BLOB_SIZE");
    let second_half: &[u8; BLOB_SIZE] = second_half
        .try_into()
        .expect("split_at guarantees BLOB_SIZE");

    let c1 = compute_blob_commitment(first_half, settings)?;
    let c2 = compute_blob_commitment(second_half, settings)?;

    aggregate_commitments(&c1, &c2)
}

/// Aggregate an arbitrary number of KZG commitments into a single commitment
/// via iterative pairwise aggregation: `C = aggregate(C_prev, C_next)`.
///
/// Returns an error if `commitments` is empty.
/// For a single commitment, returns it unchanged.
pub fn aggregate_all_commitments(commitments: &[KzgCommitment]) -> eyre::Result<KzgCommitment> {
    match commitments.len() {
        0 => Err(eyre::eyre!("cannot aggregate zero commitments")),
        1 => Ok(commitments[0]),
        _ => {
            let mut acc = commitments[0];
            for c in &commitments[1..] {
                acc = aggregate_commitments(&acc, c)?;
            }
            Ok(acc)
        }
    }
}

/// Compute a composite commitment binding a KZG commitment to a signer's address.
///
/// `composite = SHA256(DOMAIN_SEPARATOR || kzg_commitment || signer_address)`
///
/// This prevents one signer from claiming another's KZG commitment as their own.
pub fn compute_composite_commitment(
    kzg_commitment: &[u8; COMMITMENT_SIZE],
    signer_address: &IrysAddress,
) -> H256 {
    let mut hasher = sha::Sha256::new();
    hasher.update(DOMAIN_SEPARATOR);
    hasher.update(kzg_commitment);
    hasher.update(&signer_address.0 .0);
    H256(hasher.finish())
}

// ---------------------------------------------------------------------------
// BLS12-381 scalar field (Fr) arithmetic helpers
// ---------------------------------------------------------------------------

/// Convert 32 big-endian bytes into a BLS12-381 scalar field element.
/// The input is automatically reduced modulo the scalar field order.
fn fr_from_bytes(bytes: &[u8; SCALAR_SIZE]) -> blst::blst_fr {
    let mut scalar = blst::blst_scalar::default();
    let mut fr = blst::blst_fr::default();
    // SAFETY: `bytes` is exactly 32 bytes; `blst_scalar_from_bendian` reads 32 bytes
    // from the pointer. `blst_fr_from_scalar` reduces modulo the field order.
    unsafe {
        blst::blst_scalar_from_bendian(&mut scalar, bytes.as_ptr());
        blst::blst_fr_from_scalar(&mut fr, &scalar);
    }
    fr
}

/// Convert a BLS12-381 scalar field element back to 32 big-endian bytes.
fn fr_to_bytes(fr: &blst::blst_fr) -> [u8; SCALAR_SIZE] {
    let mut scalar = blst::blst_scalar::default();
    let mut bytes = [0_u8; SCALAR_SIZE];
    // SAFETY: `blst_scalar_from_fr` writes a valid scalar from the field element.
    // `blst_bendian_from_scalar` writes exactly 32 bytes.
    unsafe {
        blst::blst_scalar_from_fr(&mut scalar, fr);
        blst::blst_bendian_from_scalar(bytes.as_mut_ptr(), &scalar);
    }
    bytes
}

/// Add two BLS12-381 scalars (mod field order). Inputs/outputs are big-endian.
pub fn bls_fr_add(a: &[u8; SCALAR_SIZE], b: &[u8; SCALAR_SIZE]) -> [u8; SCALAR_SIZE] {
    let fr_a = fr_from_bytes(a);
    let fr_b = fr_from_bytes(b);
    let mut result = blst::blst_fr::default();
    // SAFETY: All `blst_fr` values are initialized. `blst_fr_add` computes
    // the modular sum of two valid field elements.
    unsafe {
        blst::blst_fr_add(&mut result, &fr_a, &fr_b);
    }
    fr_to_bytes(&result)
}

/// Multiply two BLS12-381 scalars (mod field order). Inputs/outputs are big-endian.
pub fn bls_fr_mul(a: &[u8; SCALAR_SIZE], b: &[u8; SCALAR_SIZE]) -> [u8; SCALAR_SIZE] {
    let fr_a = fr_from_bytes(a);
    let fr_b = fr_from_bytes(b);
    let mut result = blst::blst_fr::default();
    // SAFETY: Both `blst_fr` values are initialized. `blst_fr_mul` computes
    // the modular product of two valid field elements.
    unsafe {
        blst::blst_fr_mul(&mut result, &fr_a, &fr_b);
    }
    fr_to_bytes(&result)
}

/// Compute P1 + scalar·P2 for two compressed BLS12-381 G1 points.
///
/// Both `p1_bytes` and `p2_bytes` are 48-byte compressed G1 points.
/// `scalar_bytes` is a 32-byte big-endian scalar.
pub fn g1_add_scaled(
    p1_bytes: &[u8; PROOF_SIZE],
    p2_bytes: &[u8; PROOF_SIZE],
    scalar_bytes: &[u8; SCALAR_SIZE],
) -> eyre::Result<[u8; PROOF_SIZE]> {
    use blst::min_pk::PublicKey;
    use blst::{blst_p1, blst_p1_affine, blst_scalar};

    let mut r_scalar = blst_scalar::default();
    // SAFETY: `scalar_bytes` is exactly 32 bytes.
    unsafe {
        blst::blst_scalar_from_bendian(&mut r_scalar, scalar_bytes.as_ptr());
    }

    let p1 = PublicKey::from_bytes(p1_bytes)
        .map_err(|e| eyre::eyre!("failed to decompress P1: {e:?}"))?;
    let p2 = PublicKey::from_bytes(p2_bytes)
        .map_err(|e| eyre::eyre!("failed to decompress P2: {e:?}"))?;

    let p1_affine: &blst_p1_affine = (&p1).into();
    let p2_affine: &blst_p1_affine = (&p2).into();

    let mut p2_proj = blst_p1::default();
    let mut r_p2 = blst_p1::default();
    // SAFETY: All blst_p1 types are initialized via `default()`. `blst_p1_from_affine`
    // converts a validated affine point to projective. `blst_p1_mult` multiplies a
    // valid projective point by a 256-bit scalar.
    unsafe {
        blst::blst_p1_from_affine(&mut p2_proj, p2_affine);
        blst::blst_p1_mult(&mut r_p2, &p2_proj, r_scalar.b.as_ptr(), 256);
    }

    let mut result = blst_p1::default();
    // SAFETY: Both projective points are valid (from validated affine points
    // and scalar multiplication). `blst_p1_add` adds two projective points.
    unsafe {
        let mut p1_proj = blst_p1::default();
        blst::blst_p1_from_affine(&mut p1_proj, p1_affine);
        blst::blst_p1_add(&mut result, &p1_proj, &r_p2);
    }

    let mut compressed = [0_u8; PROOF_SIZE];
    // SAFETY: `result` is a valid projective G1 point. `compressed` is a 48-byte
    // buffer matching compressed G1 point size.
    unsafe {
        blst::blst_p1_compress(compressed.as_mut_ptr(), &result);
    }

    Ok(compressed)
}

// ---------------------------------------------------------------------------
// KZG opening proof functions
// ---------------------------------------------------------------------------

/// Compute a KZG opening proof for a 256KB chunk at evaluation point `z`.
///
/// Splits the chunk into two 128KB halves (same scheme as `compute_chunk_commitment`),
/// computes per-half KZG proofs, then aggregates:
/// - `π = π1 + r·π2` (G1 point addition)
/// - `y = y1 + r·y2` (scalar field addition)
/// where `r = SHA256(C1 || C2)` and `C1, C2` are the per-half commitments.
///
/// Returns `(proof_bytes, evaluation_bytes)` = (π, y).
pub fn compute_chunk_opening_proof(
    chunk_data: &[u8],
    z_bytes: &[u8; SCALAR_SIZE],
    settings: &KzgSettings,
) -> eyre::Result<([u8; PROOF_SIZE], [u8; SCALAR_SIZE])> {
    if chunk_data.len() > CHUNK_SIZE_FOR_KZG {
        return Err(eyre::eyre!(
            "chunk data too large: {} bytes (max {})",
            chunk_data.len(),
            CHUNK_SIZE_FOR_KZG
        ));
    }

    let mut padded = [0_u8; CHUNK_SIZE_FOR_KZG];
    padded[..chunk_data.len()].copy_from_slice(chunk_data);

    let (first_half, second_half) = padded.split_at(BLOB_SIZE);
    let first_half: &[u8; BLOB_SIZE] = first_half
        .try_into()
        .expect("split_at guarantees BLOB_SIZE");
    let second_half: &[u8; BLOB_SIZE] = second_half
        .try_into()
        .expect("split_at guarantees BLOB_SIZE");

    let blob1 = Blob::new(*first_half);
    let blob2 = Blob::new(*second_half);

    // Per-half commitments needed for aggregation scalar r
    let c1 = compute_blob_commitment(first_half, settings)?;
    let c2 = compute_blob_commitment(second_half, settings)?;

    // r = SHA256(C1 || C2) — same derivation as aggregate_commitments
    let mut hasher = sha::Sha256::new();
    hasher.update(c1.as_ref());
    hasher.update(c2.as_ref());
    let r_bytes = hasher.finish();

    // Compute KZG opening proofs for each half
    let z = c_kzg::Bytes32::new(*z_bytes);
    let (proof1, y1) = settings
        .compute_kzg_proof(&blob1, &z)
        .map_err(|e| eyre::eyre!("KZG proof computation failed for first half: {e}"))?;
    let (proof2, y2) = settings
        .compute_kzg_proof(&blob2, &z)
        .map_err(|e| eyre::eyre!("KZG proof computation failed for second half: {e}"))?;

    // Aggregate proof: π = π1 + r·π2
    let proof1_bytes: [u8; PROOF_SIZE] = *proof1.to_bytes().as_ref();
    let proof2_bytes: [u8; PROOF_SIZE] = *proof2.to_bytes().as_ref();
    let aggregated_proof = g1_add_scaled(&proof1_bytes, &proof2_bytes, &r_bytes)?;

    // Aggregate evaluation: y = y1 + r·y2
    let y1_bytes: [u8; SCALAR_SIZE] = *y1.as_ref();
    let y2_bytes: [u8; SCALAR_SIZE] = *y2.as_ref();
    let r_y2 = bls_fr_mul(&y2_bytes, &r_bytes);
    let aggregated_y = bls_fr_add(&y1_bytes, &r_y2);

    Ok((aggregated_proof, aggregated_y))
}

/// Verify a KZG opening proof against a commitment.
///
/// Checks that `p(z) = y` using the provided proof, where `p` is the polynomial
/// committed to by `commitment`.
pub fn verify_chunk_opening_proof(
    commitment: &KzgCommitmentBytes,
    z_bytes: &[u8; SCALAR_SIZE],
    y_bytes: &[u8; SCALAR_SIZE],
    proof_bytes: &[u8; PROOF_SIZE],
    settings: &KzgSettings,
) -> eyre::Result<bool> {
    let commitment_48 = c_kzg::Bytes48::new(commitment.0);
    let z = c_kzg::Bytes32::new(*z_bytes);
    let y = c_kzg::Bytes32::new(*y_bytes);
    let proof_48 = c_kzg::Bytes48::new(*proof_bytes);

    settings
        .verify_kzg_proof(&commitment_48, &z, &y, &proof_48)
        .map_err(|e| eyre::eyre!("KZG proof verification failed: {e}"))
}

// ---------------------------------------------------------------------------
// Challenge point derivation
// ---------------------------------------------------------------------------

/// Derive a BLS12-381 field element from a challenge seed and chunk offset.
///
/// Used as the evaluation point `z` for custody opening proofs.
/// Result is `SHA256(challenge_seed || chunk_offset_le)` reduced modulo
/// the BLS12-381 scalar field order.
pub fn derive_challenge_point(challenge_seed: &H256, chunk_offset: u32) -> [u8; SCALAR_SIZE] {
    let mut hasher = sha::Sha256::new();
    hasher.update(&challenge_seed.0);
    hasher.update(&chunk_offset.to_le_bytes());
    let hash = hasher.finish();

    // Reduce modulo BLS12-381 scalar field order via blst
    fr_to_bytes(&fr_from_bytes(&hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn kzg_settings() -> &'static KzgSettings {
        default_kzg_settings()
    }

    /// Helper to compare KzgCommitment values by their byte representation,
    /// since the c-kzg type doesn't implement PartialEq.
    fn commitment_bytes(c: &KzgCommitment) -> &[u8] {
        c.as_ref()
    }

    #[test]
    fn aggregate_commitment_produces_valid_point() {
        let data_a = [1_u8; BLOB_SIZE];
        let data_b = [2_u8; BLOB_SIZE];
        let c1 = compute_blob_commitment(&data_a, kzg_settings()).unwrap();
        let c2 = compute_blob_commitment(&data_b, kzg_settings()).unwrap();
        let agg = aggregate_commitments(&c1, &c2).unwrap();

        assert_eq!(agg.as_ref().len(), COMMITMENT_SIZE);
        blst::min_pk::PublicKey::from_bytes(agg.as_ref())
            .expect("aggregate commitment should be a valid G1 point");
    }

    #[test]
    fn zero_padded_blob_matches_single_commitment() {
        // A chunk that fits in a single blob (≤128KB) should still produce a
        // valid aggregated commitment. The second half is all zeros.
        let small_data = vec![99_u8; BLOB_SIZE];
        let commitment = compute_chunk_commitment(&small_data, kzg_settings()).unwrap();

        assert_eq!(commitment.as_ref().len(), COMMITMENT_SIZE);
        blst::min_pk::PublicKey::from_bytes(commitment.as_ref())
            .expect("commitment should be a valid G1 point");
    }

    #[test]
    fn oversized_chunk_rejected() {
        let oversized = vec![0_u8; CHUNK_SIZE_FOR_KZG + 1];
        let result = compute_chunk_commitment(&oversized, kzg_settings());
        assert!(result.is_err());
    }

    #[test]
    fn composite_commitment_different_addresses() {
        let kzg = [42_u8; COMMITMENT_SIZE];
        let addr1 = IrysAddress::from([1_u8; 20]);
        let addr2 = IrysAddress::from([2_u8; 20]);
        let c1 = compute_composite_commitment(&kzg, &addr1);
        let c2 = compute_composite_commitment(&kzg, &addr2);
        assert_ne!(c1, c2);
    }

    #[test]
    fn composite_commitment_different_kzg_commitments() {
        let kzg1 = [1_u8; COMMITMENT_SIZE];
        let kzg2 = [2_u8; COMMITMENT_SIZE];
        let addr = IrysAddress::from([42_u8; 20]);
        let c1 = compute_composite_commitment(&kzg1, &addr);
        let c2 = compute_composite_commitment(&kzg2, &addr);
        assert_ne!(c1, c2);
    }

    #[test]
    fn aggregate_all_empty_returns_error() {
        assert!(aggregate_all_commitments(&[]).is_err());
    }

    #[test]
    fn aggregate_all_deterministic() {
        let c1 = compute_blob_commitment(&[1_u8; BLOB_SIZE], kzg_settings()).unwrap();
        let c2 = compute_blob_commitment(&[2_u8; BLOB_SIZE], kzg_settings()).unwrap();
        let c3 = compute_blob_commitment(&[3_u8; BLOB_SIZE], kzg_settings()).unwrap();
        let agg1 = aggregate_all_commitments(&[c1, c2, c3]).unwrap();
        let agg2 = aggregate_all_commitments(&[c1, c2, c3]).unwrap();
        assert_eq!(commitment_bytes(&agg1), commitment_bytes(&agg2));
    }

    #[test]
    fn aggregate_all_order_matters() {
        let c1 = compute_blob_commitment(&[1_u8; BLOB_SIZE], kzg_settings()).unwrap();
        let c2 = compute_blob_commitment(&[2_u8; BLOB_SIZE], kzg_settings()).unwrap();
        let agg_12 = aggregate_all_commitments(&[c1, c2]).unwrap();
        let agg_21 = aggregate_all_commitments(&[c2, c1]).unwrap();
        assert_ne!(commitment_bytes(&agg_12), commitment_bytes(&agg_21));
    }

    // BLS12-381 field modulus starts with 0x73; filling a blob with any byte
    // >= 0x74 (116) makes each 32-byte field element exceed the modulus,
    // causing C_KZG_BADARGS. Seeds must stay in 0..114 for uniform-fill blobs.
    const MAX_VALID_SEED: u8 = 114;

    // KZG commitment computation is expensive (~150ms per blob in debug mode).
    // Limit proptest cases to keep test runtime reasonable.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(20))]

        #[test]
        fn blob_commitment_roundtrip(seed in 0_u8..MAX_VALID_SEED) {
            let data = [seed; BLOB_SIZE];
            let c1 = compute_blob_commitment(&data, kzg_settings()).unwrap();
            let c2 = compute_blob_commitment(&data, kzg_settings()).unwrap();
            prop_assert_eq!(commitment_bytes(&c1), commitment_bytes(&c2));
        }

        #[test]
        fn chunk_commitment_roundtrip(seed in 0_u8..MAX_VALID_SEED) {
            let data = vec![seed; CHUNK_SIZE_FOR_KZG];
            let c1 = compute_chunk_commitment(&data, kzg_settings()).unwrap();
            let c2 = compute_chunk_commitment(&data, kzg_settings()).unwrap();
            prop_assert_eq!(commitment_bytes(&c1), commitment_bytes(&c2));
        }

        #[test]
        fn different_seeds_different_chunk_commitments(
            seed_a in 0_u8..57,
            seed_b in 57_u8..MAX_VALID_SEED,
        ) {
            let data_a = vec![seed_a; CHUNK_SIZE_FOR_KZG];
            let data_b = vec![seed_b; CHUNK_SIZE_FOR_KZG];
            let c1 = compute_chunk_commitment(&data_a, kzg_settings()).unwrap();
            let c2 = compute_chunk_commitment(&data_b, kzg_settings()).unwrap();
            prop_assert_ne!(commitment_bytes(&c1), commitment_bytes(&c2));
        }

        #[test]
        fn opening_proof_roundtrip(seed in 0_u8..MAX_VALID_SEED) {
            let data = vec![seed; CHUNK_SIZE_FOR_KZG];
            let settings = kzg_settings();
            let commitment = compute_chunk_commitment(&data, settings).unwrap();
            let commitment_bytes_val = KzgCommitmentBytes::from(
                <[u8; COMMITMENT_SIZE]>::try_from(commitment.as_ref()).unwrap(),
            );

            let z = derive_challenge_point(&H256::from([seed; 32]), 0);
            let (proof, y) = compute_chunk_opening_proof(&data, &z, settings).unwrap();
            let ok = verify_chunk_opening_proof(
                &commitment_bytes_val, &z, &y, &proof, settings,
            ).unwrap();
            prop_assert!(ok);
        }
    }

    // -- BLS scalar field arithmetic -------------------------------------------

    #[test]
    fn bls_fr_add_identity() {
        let zero = [0_u8; SCALAR_SIZE];
        let a = [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 42,
        ];
        assert_eq!(bls_fr_add(&a, &zero), a);
    }

    #[test]
    fn bls_fr_mul_identity() {
        let one = {
            let mut b = [0_u8; SCALAR_SIZE];
            b[SCALAR_SIZE - 1] = 1;
            b
        };
        let a = [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 42,
        ];
        assert_eq!(bls_fr_mul(&a, &one), a);
    }

    #[test]
    fn bls_fr_mul_zero() {
        let zero = [0_u8; SCALAR_SIZE];
        let a = [
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 42,
        ];
        assert_eq!(bls_fr_mul(&a, &zero), zero);
    }

    #[test]
    fn g1_add_scaled_valid_points() {
        let data1 = [1_u8; BLOB_SIZE];
        let data2 = [2_u8; BLOB_SIZE];
        let c1 = compute_blob_commitment(&data1, kzg_settings()).unwrap();
        let c2 = compute_blob_commitment(&data2, kzg_settings()).unwrap();
        let p1: [u8; PROOF_SIZE] = c1.as_ref().try_into().unwrap();
        let p2: [u8; PROOF_SIZE] = c2.as_ref().try_into().unwrap();
        let scalar = {
            let mut s = [0_u8; SCALAR_SIZE];
            s[SCALAR_SIZE - 1] = 1;
            s
        };
        let result = g1_add_scaled(&p1, &p2, &scalar).unwrap();
        // p1 + 1*p2 should be a valid G1 point
        blst::min_pk::PublicKey::from_bytes(&result).expect("result should be a valid G1 point");
    }

    // -- Opening proof tests ---------------------------------------------------

    #[test]
    fn opening_proof_wrong_data_fails() {
        let data = vec![42_u8; CHUNK_SIZE_FOR_KZG];
        let settings = kzg_settings();
        let commitment = compute_chunk_commitment(&data, settings).unwrap();
        let commitment_bytes_val = KzgCommitmentBytes::from(
            <[u8; COMMITMENT_SIZE]>::try_from(commitment.as_ref()).unwrap(),
        );

        let z = derive_challenge_point(&H256::from([1_u8; 32]), 0);
        let (_proof, _y) = compute_chunk_opening_proof(&data, &z, settings).unwrap();

        // Compute proof for different data
        let bad_data = vec![7_u8; CHUNK_SIZE_FOR_KZG];
        let (bad_proof, bad_y) = compute_chunk_opening_proof(&bad_data, &z, settings).unwrap();

        // Verify with original commitment but bad proof/y — should fail
        let ok =
            verify_chunk_opening_proof(&commitment_bytes_val, &z, &bad_y, &bad_proof, settings)
                .unwrap();
        assert!(!ok);
    }

    #[test]
    fn opening_proof_wrong_z_fails() {
        // Non-constant data: vary each 32-byte field element so the
        // polynomial is non-trivial and p(z1) != p(z2).
        let mut data = vec![0_u8; CHUNK_SIZE_FOR_KZG];
        for (i, chunk) in data.chunks_mut(SCALAR_SIZE).enumerate() {
            let val = u8::try_from(i % usize::from(MAX_VALID_SEED)).unwrap_or(0);
            chunk[1] = val; // byte 0 stays 0 (< 0x74), byte 1 varies
        }

        let settings = kzg_settings();
        let commitment = compute_chunk_commitment(&data, settings).unwrap();
        let commitment_bytes_val = KzgCommitmentBytes::from(
            <[u8; COMMITMENT_SIZE]>::try_from(commitment.as_ref()).unwrap(),
        );

        let z1 = derive_challenge_point(&H256::from([1_u8; 32]), 0);
        let (proof, y) = compute_chunk_opening_proof(&data, &z1, settings).unwrap();

        // Verify with a different z — should fail
        let z2 = derive_challenge_point(&H256::from([2_u8; 32]), 0);
        let ok =
            verify_chunk_opening_proof(&commitment_bytes_val, &z2, &y, &proof, settings).unwrap();
        assert!(!ok);
    }

    // -- Challenge point derivation tests --------------------------------------

    #[test]
    fn derive_challenge_point_deterministic() {
        let seed = H256::from([42_u8; 32]);
        let z1 = derive_challenge_point(&seed, 0);
        let z2 = derive_challenge_point(&seed, 0);
        assert_eq!(z1, z2);
    }

    #[test]
    fn derive_challenge_point_different_offsets() {
        let seed = H256::from([42_u8; 32]);
        let z0 = derive_challenge_point(&seed, 0);
        let z1 = derive_challenge_point(&seed, 1);
        assert_ne!(z0, z1);
    }

    #[test]
    fn derive_challenge_point_different_seeds() {
        let z1 = derive_challenge_point(&H256::from([1_u8; 32]), 0);
        let z2 = derive_challenge_point(&H256::from([2_u8; 32]), 0);
        assert_ne!(z1, z2);
    }

    #[test]
    fn derive_challenge_point_valid_field_element() {
        // BLS12-381 scalar field order (big-endian)
        let bls_order: [u8; 32] = [
            0x73, 0xed, 0xa7, 0x53, 0x29, 0x9d, 0x7d, 0x48, 0x33, 0x39, 0xd8, 0x08, 0x09, 0xa1,
            0xd8, 0x05, 0x53, 0xbd, 0xa4, 0x02, 0xff, 0xfe, 0x5b, 0xfe, 0xff, 0xff, 0xff, 0xff,
            0x00, 0x00, 0x00, 0x01,
        ];
        let z = derive_challenge_point(&H256::from([0xff_u8; 32]), 0);
        // z must be strictly less than the field order
        assert!(z < bls_order);
    }
}
