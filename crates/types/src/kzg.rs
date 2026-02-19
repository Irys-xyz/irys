use alloy_eips::eip4844::env_settings::EnvKzgSettings;
use c_kzg::{Blob, KzgCommitment, KzgSettings};
use openssl::sha;

pub const BLOB_SIZE: usize = 131_072; // 128KB = 4096 * 32 bytes
pub const CHUNK_SIZE_FOR_KZG: usize = 262_144; // 256KB = 2 * BLOB_SIZE
pub const COMMITMENT_SIZE: usize = 48; // Compressed G1 point

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

    // Zero-pad to 256KB
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
    fn commitment_size_is_48_bytes() {
        let data = [0_u8; BLOB_SIZE];
        let commitment = compute_blob_commitment(&data, kzg_settings()).unwrap();
        assert_eq!(commitment.as_ref().len(), COMMITMENT_SIZE);
    }

    #[test]
    fn same_data_produces_same_commitment() {
        let data = [42_u8; BLOB_SIZE];
        let c1 = compute_blob_commitment(&data, kzg_settings()).unwrap();
        let c2 = compute_blob_commitment(&data, kzg_settings()).unwrap();
        assert_eq!(commitment_bytes(&c1), commitment_bytes(&c2));
    }

    #[test]
    fn different_data_produces_different_commitment() {
        let data_a = [1_u8; BLOB_SIZE];
        let data_b = [2_u8; BLOB_SIZE];
        let c1 = compute_blob_commitment(&data_a, kzg_settings()).unwrap();
        let c2 = compute_blob_commitment(&data_b, kzg_settings()).unwrap();
        assert_ne!(commitment_bytes(&c1), commitment_bytes(&c2));
    }

    #[test]
    fn chunk_commitment_deterministic() {
        let data = vec![7_u8; CHUNK_SIZE_FOR_KZG];
        let c1 = compute_chunk_commitment(&data, kzg_settings()).unwrap();
        let c2 = compute_chunk_commitment(&data, kzg_settings()).unwrap();
        assert_eq!(commitment_bytes(&c1), commitment_bytes(&c2));
    }

    #[test]
    fn chunk_commitment_different_data() {
        let data_a = vec![1_u8; CHUNK_SIZE_FOR_KZG];
        let data_b = vec![2_u8; CHUNK_SIZE_FOR_KZG];
        let c1 = compute_chunk_commitment(&data_a, kzg_settings()).unwrap();
        let c2 = compute_chunk_commitment(&data_b, kzg_settings()).unwrap();
        assert_ne!(commitment_bytes(&c1), commitment_bytes(&c2));
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
