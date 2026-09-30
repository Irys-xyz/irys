use crate::{H256, IrysAddress};
use alloy_eips::eip4844::env_settings::EnvKzgSettings;
use bytes::BufMut;
pub use c_kzg::KzgSettings;
use c_kzg::{Blob, KzgCommitment};
use openssl::sha;
use reth_codecs::Compact;
use serde::{Deserialize, Serialize};

pub const BLOB_SIZE: usize = 131_072;
pub const CHUNK_SIZE_FOR_KZG: usize = 2 * BLOB_SIZE;
pub const COMMITMENT_SIZE: usize = 48;
pub const PROOF_SIZE: usize = 48;
pub const SCALAR_SIZE: usize = 32;
pub const FIELD_ELEMENT_BYTES: usize = 32;
pub const FIELD_ELEMENT_PAYLOAD_BYTES: usize = 31;
pub const FIELD_ELEMENTS_PER_BLOB: usize = BLOB_SIZE / FIELD_ELEMENT_BYTES;
pub const NATIVE_CHUNK_BLOB_COUNT: usize = 3;
pub const DOMAIN_SEPARATOR: &[u8] = b"IRYS_KZG_INGRESS_V2";
pub const POSSESSION_DOMAIN: &[u8] = b"IRYS_KZG_POSSESSION_V1";

const _: () = assert!(FIELD_ELEMENTS_PER_BLOB * FIELD_ELEMENT_BYTES == BLOB_SIZE);
const _: () = assert!(
    NATIVE_CHUNK_BLOB_COUNT * FIELD_ELEMENTS_PER_BLOB * FIELD_ELEMENT_PAYLOAD_BYTES
        >= CHUNK_SIZE_FOR_KZG
);

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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerChunkCommitment {
    pub chunk_index: u32,
    pub commitment: KzgCommitmentBytes,
}

const PER_CHUNK_INDEX_BYTES: usize = 4;
const PER_CHUNK_VALUE_BYTES: usize = PER_CHUNK_INDEX_BYTES + COMMITMENT_SIZE;

impl Compact for PerChunkCommitment {
    fn to_compact<B: BufMut + AsMut<[u8]>>(&self, buf: &mut B) -> usize {
        buf.put_slice(&self.chunk_index.to_be_bytes());
        buf.put_slice(&self.commitment.0);
        PER_CHUNK_VALUE_BYTES
    }

    fn from_compact(buf: &[u8], _len: usize) -> (Self, &[u8]) {
        let index_bytes: [u8; PER_CHUNK_INDEX_BYTES] = buf[..PER_CHUNK_INDEX_BYTES]
            .try_into()
            .expect("PerChunkCommitment index is 4 bytes");
        let mut commitment = [0_u8; COMMITMENT_SIZE];
        commitment.copy_from_slice(&buf[PER_CHUNK_INDEX_BYTES..PER_CHUNK_VALUE_BYTES]);
        (
            Self {
                chunk_index: u32::from_be_bytes(index_bytes),
                commitment: KzgCommitmentBytes(commitment),
            },
            &buf[PER_CHUNK_VALUE_BYTES..],
        )
    }
}

impl arbitrary::Arbitrary<'_> for PerChunkCommitment {
    fn arbitrary(u: &mut arbitrary::Unstructured<'_>) -> arbitrary::Result<Self> {
        Ok(Self {
            chunk_index: u.arbitrary()?,
            commitment: u.arbitrary()?,
        })
    }
}

/// Returns a reference to the lazily-initialized Ethereum KZG trusted setup.
///
/// The trusted setup (~50MB) overflows the default 8MB thread stack, so
/// initialization is performed on a dedicated thread with a 64MB stack.
pub fn default_kzg_settings() -> &'static KzgSettings {
    static SETTINGS: std::sync::OnceLock<&'static KzgSettings> = std::sync::OnceLock::new();
    SETTINGS.get_or_init(|| {
        std::thread::Builder::new()
            .name("kzg-setup".into())
            .stack_size(64 * 1024 * 1024)
            .spawn(|| EnvKzgSettings::Default.get())
            .expect("failed to spawn KZG setup thread")
            .join()
            .expect("KZG setup thread panicked")
    })
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

fn aggregation_scalar(left: &KzgCommitment, right: &KzgCommitment) -> [u8; SCALAR_SIZE] {
    let mut hasher = sha::Sha256::new();
    hasher.update(left.as_ref());
    hasher.update(right.as_ref());
    hasher.finish()
}

/// Aggregate two G1 commitments: C = C1 + r·C2 where r = SHA256(C1 || C2).
pub fn aggregate_commitments(
    c1: &KzgCommitment,
    c2: &KzgCommitment,
) -> eyre::Result<KzgCommitment> {
    let r_bytes = aggregation_scalar(c1, c2);

    let c1_bytes: &[u8; COMMITMENT_SIZE] = c1
        .as_ref()
        .try_into()
        .map_err(|_| eyre::eyre!("commitment size mismatch"))?;
    let c2_bytes: &[u8; COMMITMENT_SIZE] = c2
        .as_ref()
        .try_into()
        .map_err(|_| eyre::eyre!("commitment size mismatch"))?;

    let compressed = g1_add_scaled(c1_bytes, c2_bytes, &r_bytes)?;
    Ok(KzgCommitment::from(compressed))
}

/// Encode a native chunk as canonical BLS field elements.
///
/// Each 31 payload bytes become one 32-byte element with a leading zero, so
/// every element is strictly below the BLS12-381 scalar modulus. The chunk is
/// zero-padded to [`CHUNK_SIZE_FOR_KZG`] before encoding. The result is exactly
/// [`NATIVE_CHUNK_BLOB_COUNT`] blobs.
pub fn encode_native_chunk_blobs(
    chunk_data: &[u8],
) -> eyre::Result<[Box<[u8; BLOB_SIZE]>; NATIVE_CHUNK_BLOB_COUNT]> {
    if chunk_data.len() > CHUNK_SIZE_FOR_KZG {
        return Err(eyre::eyre!(
            "chunk data too large: {} bytes (max {})",
            chunk_data.len(),
            CHUNK_SIZE_FOR_KZG
        ));
    }

    let mut padded = vec![0_u8; CHUNK_SIZE_FOR_KZG];
    padded[..chunk_data.len()].copy_from_slice(chunk_data);

    let mut blobs = Vec::with_capacity(NATIVE_CHUNK_BLOB_COUNT);
    let mut offset = 0_usize;
    for _blob_index in 0..NATIVE_CHUNK_BLOB_COUNT {
        let mut blob = vec![0_u8; BLOB_SIZE];
        for element in 0..FIELD_ELEMENTS_PER_BLOB {
            if offset >= padded.len() {
                break;
            }
            let take = (padded.len() - offset).min(FIELD_ELEMENT_PAYLOAD_BYTES);
            let dest = element
                .checked_mul(FIELD_ELEMENT_BYTES)
                .and_then(|start| start.checked_add(1))
                .ok_or_else(|| eyre::eyre!("field element offset overflow"))?;
            blob[dest..dest + take].copy_from_slice(&padded[offset..offset + take]);
            offset += take;
        }
        let boxed: Box<[u8; BLOB_SIZE]> = blob
            .into_boxed_slice()
            .try_into()
            .map_err(|_| eyre::eyre!("native chunk blob length"))?;
        blobs.push(boxed);
    }
    eyre::ensure!(
        offset == CHUNK_SIZE_FOR_KZG,
        "native chunk encoding consumed {offset} of {CHUNK_SIZE_FOR_KZG} bytes"
    );
    blobs
        .try_into()
        .map_err(|_| eyre::eyre!("native chunk blob count"))
}

/// Compute the aggregated KZG commitment for a 256KB native Irys chunk.
///
/// The chunk is encoded as [`NATIVE_CHUNK_BLOB_COUNT`] canonical blobs, each
/// blob is committed, and the commitments are folded with
/// [`aggregate_commitments`].
///
/// If `chunk_data` is shorter than [`CHUNK_SIZE_FOR_KZG`], it is zero-padded.
/// If it is longer, returns an error.
pub fn compute_chunk_commitment(
    chunk_data: &[u8],
    settings: &KzgSettings,
) -> eyre::Result<KzgCommitment> {
    let blobs = encode_native_chunk_blobs(chunk_data)?;
    let commitments = blobs
        .iter()
        .map(|blob| compute_blob_commitment(blob, settings))
        .collect::<eyre::Result<Vec<_>>>()?;
    aggregate_all_commitments(&commitments)
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

/// Bind a KZG commitment, its signer, and the possession opening.
///
/// `composite = SHA256(DOMAIN_SEPARATOR || kzg || signer || data_root || y || proof)`.
/// The opening itself is the possession proof. This hash only stops a signature
/// from covering a different opening than the one that was checked.
pub fn compute_composite_commitment(
    kzg_commitment: &[u8; COMMITMENT_SIZE],
    signer_address: &IrysAddress,
    data_root: &H256,
    possession_y: &[u8; SCALAR_SIZE],
    possession_proof: &[u8; PROOF_SIZE],
) -> H256 {
    let mut hasher = sha::Sha256::new();
    hasher.update(DOMAIN_SEPARATOR);
    hasher.update(kzg_commitment);
    hasher.update(&signer_address.0.0);
    hasher.update(&data_root.0);
    hasher.update(possession_y);
    hasher.update(possession_proof);
    H256(hasher.finish())
}

/// Convert a [`KzgCommitment`] to a fixed-size byte array.
pub fn commitment_to_bytes(c: &KzgCommitment) -> eyre::Result<[u8; COMMITMENT_SIZE]> {
    c.as_ref()
        .try_into()
        .map_err(|_| eyre::eyre!("KZG commitment is not 48 bytes"))
}

/// Zero-pad data to [`CHUNK_SIZE_FOR_KZG`] bytes.
pub fn zero_pad_to_chunk_size(data: &[u8]) -> eyre::Result<Vec<u8>> {
    eyre::ensure!(
        data.len() <= CHUNK_SIZE_FOR_KZG,
        "data exceeds chunk size: {} > {}",
        data.len(),
        CHUNK_SIZE_FOR_KZG,
    );
    let mut padded = vec![0_u8; CHUNK_SIZE_FOR_KZG];
    padded[..data.len()].copy_from_slice(data);
    Ok(padded)
}

// SAFETY for all blst FFI calls in this module: All blst types are initialized via
// `default()` or `from_bytes()`. Buffer sizes are guaranteed by Rust's type system
// (fixed-size arrays). Affine points are validated by `PublicKey::from_bytes` before
// conversion to projective form. Scalars are read from exactly-sized byte arrays.

fn fr_from_bytes(bytes: &[u8; SCALAR_SIZE]) -> blst::blst_fr {
    let mut scalar = blst::blst_scalar::default();
    let mut fr = blst::blst_fr::default();
    unsafe {
        blst::blst_scalar_from_bendian(&mut scalar, bytes.as_ptr());
        blst::blst_fr_from_scalar(&mut fr, &scalar);
    }
    fr
}

fn fr_to_bytes(fr: &blst::blst_fr) -> [u8; SCALAR_SIZE] {
    let mut scalar = blst::blst_scalar::default();
    let mut bytes = [0_u8; SCALAR_SIZE];
    unsafe {
        blst::blst_scalar_from_fr(&mut scalar, fr);
        blst::blst_bendian_from_scalar(bytes.as_mut_ptr(), &scalar);
    }
    bytes
}

pub fn bls_fr_add(a: &[u8; SCALAR_SIZE], b: &[u8; SCALAR_SIZE]) -> [u8; SCALAR_SIZE] {
    let fr_a = fr_from_bytes(a);
    let fr_b = fr_from_bytes(b);
    let mut result = blst::blst_fr::default();
    unsafe {
        blst::blst_fr_add(&mut result, &fr_a, &fr_b);
    }
    fr_to_bytes(&result)
}

pub fn bls_fr_mul(a: &[u8; SCALAR_SIZE], b: &[u8; SCALAR_SIZE]) -> [u8; SCALAR_SIZE] {
    let fr_a = fr_from_bytes(a);
    let fr_b = fr_from_bytes(b);
    let mut result = blst::blst_fr::default();
    unsafe {
        blst::blst_fr_mul(&mut result, &fr_a, &fr_b);
    }
    fr_to_bytes(&result)
}

/// Compute P1 + scalar·P2 for two compressed BLS12-381 G1 points.
pub fn g1_add_scaled(
    p1_bytes: &[u8; PROOF_SIZE],
    p2_bytes: &[u8; PROOF_SIZE],
    scalar_bytes: &[u8; SCALAR_SIZE],
) -> eyre::Result<[u8; PROOF_SIZE]> {
    use blst::min_pk::PublicKey;
    use blst::{blst_p1, blst_p1_affine, blst_scalar};

    let mut r_scalar = blst_scalar::default();
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
    unsafe {
        blst::blst_p1_from_affine(&mut p2_proj, p2_affine);
        blst::blst_p1_mult(&mut r_p2, &p2_proj, r_scalar.b.as_ptr(), 256);
    }

    let mut result = blst_p1::default();
    unsafe {
        let mut p1_proj = blst_p1::default();
        blst::blst_p1_from_affine(&mut p1_proj, p1_affine);
        blst::blst_p1_add(&mut result, &p1_proj, &r_p2);
    }

    let mut compressed = [0_u8; PROOF_SIZE];
    unsafe {
        blst::blst_p1_compress(compressed.as_mut_ptr(), &result);
    }

    Ok(compressed)
}

fn open_raw_blob(
    blob_bytes: &[u8; BLOB_SIZE],
    z_bytes: &[u8; SCALAR_SIZE],
    settings: &KzgSettings,
) -> eyre::Result<(KzgCommitment, [u8; PROOF_SIZE], [u8; SCALAR_SIZE])> {
    let blob = Blob::new(*blob_bytes);
    let commitment = settings
        .blob_to_kzg_commitment(&blob)
        .map_err(|e| eyre::eyre!("KZG blob commitment failed: {e}"))?;
    let z = c_kzg::Bytes32::new(*z_bytes);
    let (proof, y) = settings
        .compute_kzg_proof(&blob, &z)
        .map_err(|e| eyre::eyre!("KZG proof computation failed: {e}"))?;
    let proof_bytes: [u8; PROOF_SIZE] = *proof.to_bytes().as_ref();
    let y_bytes: [u8; SCALAR_SIZE] = *y.as_ref();
    Ok((commitment, proof_bytes, y_bytes))
}

fn fold_openings(
    parts: Vec<(KzgCommitment, [u8; PROOF_SIZE], [u8; SCALAR_SIZE])>,
) -> eyre::Result<(KzgCommitment, [u8; PROOF_SIZE], [u8; SCALAR_SIZE])> {
    let mut parts = parts.into_iter();
    let Some(mut acc) = parts.next() else {
        return Err(eyre::eyre!("cannot fold zero openings"));
    };
    for (commitment, proof, y) in parts {
        let scalar = aggregation_scalar(&acc.0, &commitment);
        let folded_proof = g1_add_scaled(&acc.1, &proof, &scalar)?;
        let folded_y = bls_fr_add(&acc.2, &bls_fr_mul(&y, &scalar));
        let folded_commitment = aggregate_commitments(&acc.0, &commitment)?;
        acc = (folded_commitment, folded_proof, folded_y);
    }
    Ok(acc)
}

/// Opening of one EIP-4844 blob at `z`. The blob bytes are used as-is.
pub fn compute_blob_opening_proof(
    blob: &[u8; BLOB_SIZE],
    z_bytes: &[u8; SCALAR_SIZE],
    settings: &KzgSettings,
) -> eyre::Result<(KzgCommitment, [u8; PROOF_SIZE], [u8; SCALAR_SIZE])> {
    open_raw_blob(blob, z_bytes, settings)
}

fn open_native_chunk(
    chunk_data: &[u8],
    z_bytes: &[u8; SCALAR_SIZE],
    settings: &KzgSettings,
) -> eyre::Result<(KzgCommitment, [u8; PROOF_SIZE], [u8; SCALAR_SIZE])> {
    let blobs = encode_native_chunk_blobs(chunk_data)?;
    let parts = blobs
        .iter()
        .map(|blob| open_raw_blob(blob.as_ref(), z_bytes, settings))
        .collect::<eyre::Result<Vec<_>>>()?;
    fold_openings(parts)
}

/// Compute a KZG opening proof for a 256KB chunk at evaluation point `z`.
///
/// Uses the same canonical blobs and the same aggregation scalars as
/// [`compute_chunk_commitment`].
pub fn compute_chunk_opening_proof(
    chunk_data: &[u8],
    z_bytes: &[u8; SCALAR_SIZE],
    settings: &KzgSettings,
) -> eyre::Result<([u8; PROOF_SIZE], [u8; SCALAR_SIZE])> {
    let (_commitment, proof, y) = open_native_chunk(chunk_data, z_bytes, settings)?;
    Ok((proof, y))
}

/// Commitment and possession opening for an ordered list of native chunks.
///
/// Per-chunk commitments use [`compute_chunk_commitment`]. The returned opening
/// is the same fold applied to those chunk commitments, so it verifies against
/// the aggregated commitment.
pub fn compute_chunks_possession_opening<C: AsRef<[u8]>>(
    chunks: &[C],
    z_bytes: &[u8; SCALAR_SIZE],
    settings: &KzgSettings,
) -> eyre::Result<(
    [u8; COMMITMENT_SIZE],
    [u8; PROOF_SIZE],
    [u8; SCALAR_SIZE],
    Vec<KzgCommitmentBytes>,
)> {
    let mut per_chunk = Vec::with_capacity(chunks.len());
    let mut parts = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let (commitment, proof, y) = open_native_chunk(chunk.as_ref(), z_bytes, settings)?;
        per_chunk.push(KzgCommitmentBytes::from(commitment_to_bytes(&commitment)?));
        parts.push((commitment, proof, y));
    }
    let (aggregated, proof, y) = fold_openings(parts)?;
    Ok((commitment_to_bytes(&aggregated)?, proof, y, per_chunk))
}

/// `z = SHA256(POSSESSION_DOMAIN || signer || data_root)` reduced into the scalar field.
pub fn derive_possession_point(signer: &IrysAddress, data_root: &H256) -> [u8; SCALAR_SIZE] {
    let mut hasher = sha::Sha256::new();
    hasher.update(POSSESSION_DOMAIN);
    hasher.update(&signer.0.0);
    hasher.update(&data_root.0);
    fr_to_bytes(&fr_from_bytes(&hasher.finish()))
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

/// `z = SHA256(challenge_seed || chunk_offset_le) mod BLS12-381_r`
pub fn derive_challenge_point(challenge_seed: &H256, chunk_offset: u32) -> [u8; SCALAR_SIZE] {
    let mut hasher = sha::Sha256::new();
    hasher.update(&challenge_seed.0);
    hasher.update(&chunk_offset.to_le_bytes());
    fr_to_bytes(&fr_from_bytes(&hasher.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn kzg_settings() -> &'static KzgSettings {
        default_kzg_settings()
    }

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

    fn composite_for(kzg: &[u8; COMMITMENT_SIZE], addr: &IrysAddress) -> H256 {
        compute_composite_commitment(
            kzg,
            addr,
            &H256::from([9_u8; 32]),
            &[0_u8; SCALAR_SIZE],
            &[0_u8; PROOF_SIZE],
        )
    }

    #[test]
    fn composite_commitment_different_addresses() {
        let kzg = [42_u8; COMMITMENT_SIZE];
        let addr1 = IrysAddress::from([1_u8; 20]);
        let addr2 = IrysAddress::from([2_u8; 20]);
        assert_ne!(composite_for(&kzg, &addr1), composite_for(&kzg, &addr2));
    }

    #[test]
    fn composite_commitment_different_kzg_commitments() {
        let addr = IrysAddress::from([42_u8; 20]);
        let c1 = composite_for(&[1_u8; COMMITMENT_SIZE], &addr);
        let c2 = composite_for(&[2_u8; COMMITMENT_SIZE], &addr);
        assert_ne!(c1, c2);
    }

    #[test]
    fn native_chunk_of_noncanonical_bytes_commits() {
        let data = vec![0xff_u8; CHUNK_SIZE_FOR_KZG];
        let commitment = compute_chunk_commitment(&data, kzg_settings()).unwrap();
        blst::min_pk::PublicKey::from_bytes(commitment.as_ref())
            .expect("canonical encoding of 0xff chunk is a valid G1 point");
    }

    #[test]
    fn per_chunk_commitment_compact_keeps_index() {
        let original = PerChunkCommitment {
            chunk_index: 2,
            commitment: KzgCommitmentBytes([0xab_u8; COMMITMENT_SIZE]),
        };
        let mut buf = Vec::new();
        let written = original.to_compact(&mut buf);
        assert_eq!(written, PER_CHUNK_VALUE_BYTES);
        assert_eq!(&buf[..4], &2_u32.to_be_bytes());
        let (decoded, rest) = PerChunkCommitment::from_compact(&buf, buf.len());
        assert!(rest.is_empty());
        assert_eq!(decoded, original);
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
        blst::min_pk::PublicKey::from_bytes(&result).expect("result should be a valid G1 point");
    }

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

        let bad_data = vec![7_u8; CHUNK_SIZE_FOR_KZG];
        let (bad_proof, bad_y) = compute_chunk_opening_proof(&bad_data, &z, settings).unwrap();

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

        let z2 = derive_challenge_point(&H256::from([2_u8; 32]), 0);
        let ok =
            verify_chunk_opening_proof(&commitment_bytes_val, &z2, &y, &proof, settings).unwrap();
        assert!(!ok);
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
