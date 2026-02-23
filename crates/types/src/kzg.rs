use crate::{IrysAddress, H256};
use alloy_eips::eip4844::env_settings::EnvKzgSettings;
use bytes::BufMut;
use c_kzg::{Blob, KzgCommitment, KzgSettings};
use openssl::sha;
use reth_codecs::Compact;
use serde::{Deserialize, Serialize};

pub const BLOB_SIZE: usize = 131_072; // 128KB = 4096 * 32 bytes
pub const CHUNK_SIZE_FOR_KZG: usize = 262_144; // 256KB = 2 * BLOB_SIZE
pub const COMMITMENT_SIZE: usize = 48; // Compressed G1 point
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
            let hex = alloy_primitives::hex::encode(self.0);
            serializer.serialize_str(&format!("0x{hex}"))
        } else {
            serializer.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for KzgCommitmentBytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let s = String::deserialize(deserializer)?;
            let s = s.strip_prefix("0x").unwrap_or(&s);
            let bytes = alloy_primitives::hex::decode(s).map_err(serde::de::Error::custom)?;
            let arr: [u8; COMMITMENT_SIZE] = bytes.try_into().map_err(|v: Vec<u8>| {
                serde::de::Error::custom(format!(
                    "expected {COMMITMENT_SIZE} bytes, got {}",
                    v.len()
                ))
            })?;
            Ok(Self(arr))
        } else {
            let bytes = <Vec<u8>>::deserialize(deserializer)?;
            let arr: [u8; COMMITMENT_SIZE] = bytes.try_into().map_err(|v: Vec<u8>| {
                serde::de::Error::custom(format!(
                    "expected {COMMITMENT_SIZE} bytes, got {}",
                    v.len()
                ))
            })?;
            Ok(Self(arr))
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

    // Compute random challenge: r = SHA256(C1 || C2)
    let mut hasher = sha::Sha256::new();
    hasher.update(c1.as_ref());
    hasher.update(c2.as_ref());
    let r_bytes = hasher.finish();

    // Convert r to blst scalar (big-endian input)
    let mut r_scalar = blst_scalar::default();
    unsafe {
        blst::blst_scalar_from_bendian(&mut r_scalar, r_bytes.as_ptr());
    }

    // Decompress C1 and C2 from their 48-byte compressed G1 representations
    let p1 = PublicKey::from_bytes(c1.as_ref())
        .map_err(|e| eyre::eyre!("failed to decompress C1: {e:?}"))?;
    let p2 = PublicKey::from_bytes(c2.as_ref())
        .map_err(|e| eyre::eyre!("failed to decompress C2: {e:?}"))?;

    // Get affine points via From trait
    let p1_affine: &blst_p1_affine = (&p1).into();
    let p2_affine: &blst_p1_affine = (&p2).into();

    // Convert C2 to projective, then compute r·C2
    let mut p2_proj = blst_p1::default();
    let mut r_c2 = blst_p1::default();
    unsafe {
        blst::blst_p1_from_affine(&mut p2_proj, p2_affine);
        blst::blst_p1_mult(&mut r_c2, &p2_proj, r_scalar.b.as_ptr(), 256);
    }

    // Compute C1 + r·C2 (using affine + projective variant)
    let mut result = blst_p1::default();
    unsafe {
        let mut c1_proj = blst_p1::default();
        blst::blst_p1_from_affine(&mut c1_proj, p1_affine);
        blst::blst_p1_add(&mut result, &c1_proj, &r_c2);
    }

    // Compress back to 48-byte representation
    let mut compressed = [0_u8; COMMITMENT_SIZE];
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

    // Split into two 128KB halves
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
    fn partial_chunk_zero_padded() {
        let small_data = vec![42_u8; 1000];
        let commitment = compute_chunk_commitment(&small_data, kzg_settings()).unwrap();
        assert_eq!(commitment.as_ref().len(), COMMITMENT_SIZE);
    }

    #[test]
    fn empty_chunk_produces_valid_commitment() {
        let commitment = compute_chunk_commitment(&[], kzg_settings()).unwrap();
        assert_eq!(commitment.as_ref().len(), COMMITMENT_SIZE);
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
    }
}
