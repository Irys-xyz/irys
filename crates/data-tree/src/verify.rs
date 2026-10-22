//! Proof checks (spec §5).
//!
//! The order is fixed: range, byte length, proof length, the size witness, then
//! the climb. The expected proof length is `32 × (k + 1)` bytes: `k` sibling
//! hashes and `size_witness` (spec §5). A length that is not a multiple of 32
//! fails that check. Length checks run before any hashing (spec §10).

use crate::chunk::chunk_root;
use crate::error::VerifyError;
use crate::geometry::Geometry;
use crate::hash::{self, leaf, node, size_witness};
use crate::shape::{climb, depth};
use crate::types::{DataCommitment, DataTreeRoot, HASH_SIZE, NodeHash};

/// Check one chunk against an anchored commitment (spec §5, §6).
///
/// `c` is `chunk_index`.
pub fn verify_chunk(
    geometry: &Geometry,
    commitment: &DataCommitment,
    data_size: u64,
    chunk_index: u32,
    bytes: &[u8],
    proof: &[u8],
) -> Result<(), VerifyError> {
    // `chunk_len` checks `1 <= n <= max` and `c < C`, then returns the length (spec §5).
    let chunk_length = geometry.chunk_len(data_size, chunk_index)?;
    if let Err(actual) = exact_byte_length(bytes, chunk_length) {
        return Err(VerifyError::InvalidChunkLength {
            expected: chunk_length,
            actual,
        });
    }
    let chunk_count = geometry.chunk_count(data_size)?;
    // `depth(c, C) + 1`: the chunk-level siblings and the size witness (spec §5).
    let hash_count = depth(chunk_index, chunk_count) + 1;
    let expected_proof = proof_byte_length(hash_count);
    if proof.len() != expected_proof {
        return Err(VerifyError::InvalidProofLength {
            expected: expected_proof,
            actual: proof.len(),
        });
    }
    // Check the witness before the climb. A false `data_size` must not pass on
    // the true witness (spec §5, §15 Q10).
    let siblings = require_size_witness(proof, data_size)?;
    // The length check enforces `1..=chunk_size`, which is `chunk_root`'s precondition.
    let root = chunk_root(geometry, bytes).expect("chunk length is in 1..=chunk_size");
    let climbed =
        climb(root, chunk_index, chunk_count, siblings).ok_or(VerifyError::CommitmentMismatch)?;
    commitment_matches(commitment, climbed, data_size)
}

/// Check one segment against an anchored commitment (spec §5, §6).
///
/// Several segments of one chunk are checked one by one: each carries its own
/// `h` in-chunk siblings and they share one chunk proof (spec §5, §15 Q2).
/// A segment's proof is those siblings concatenated with the chunk proof. The
/// chunk proof ends with the size witness, so the concatenation is the whole
/// proof in every case, including one chunk. A compact multiproof that drops
/// siblings another revealed segment computes was designed and not adopted
/// (spec §5).
///
/// `c` is `chunk_index`. `j` is `segment_index`.
pub fn verify_segment(
    geometry: &Geometry,
    commitment: &DataCommitment,
    data_size: u64,
    chunk_index: u32,
    segment_index: u32,
    bytes: &[u8],
    proof: &[u8],
) -> Result<(), VerifyError> {
    // `segment_len` checks `1 <= n <= max`, `c < C`, and `j < SPC` (spec §5).
    let segment_length = geometry.segment_len(data_size, chunk_index, segment_index)?;
    if let Err(actual) = exact_byte_length(bytes, segment_length) {
        return Err(VerifyError::InvalidSegmentLength {
            expected: segment_length,
            actual,
        });
    }
    let chunk_count = geometry.chunk_count(data_size)?;
    // `h + depth(c, C) + 1` (spec §5): in-chunk siblings, chunk-level siblings,
    // and the size witness. `h` is at most `MAX_SUBTREE_HEIGHT` and `depth`
    // fits the bit width of a chunk count, so the sum fits `u32`.
    let hash_count = geometry.height() + depth(chunk_index, chunk_count) + 1;
    let expected_proof = proof_byte_length(hash_count);
    if proof.len() != expected_proof {
        return Err(VerifyError::InvalidProofLength {
            expected: expected_proof,
            actual: proof.len(),
        });
    }
    // Check the witness before the climb (spec §5, §15 Q10).
    let siblings = require_size_witness(proof, data_size)?;
    let in_chunk_bytes = usize::try_from(geometry.height()).expect("height fits usize") * HASH_SIZE;
    // The length check guarantees the siblings cover the `h` in-chunk hashes.
    let (in_chunk, chunk_siblings) = siblings
        .split_at_checked(in_chunk_bytes)
        .expect("proof covers the in-chunk siblings");
    let mut root = leaf(bytes);
    let mut segment_index = segment_index;
    for sibling in in_chunk.chunks_exact(HASH_SIZE) {
        let bytes = <[u8; HASH_SIZE]>::try_from(sibling)
            .expect("chunks_exact yields HASH_SIZE-byte slices");
        let sibling = NodeHash(bytes);
        // Bit `level` of `j`, after the shifts above, picks the side (spec §5).
        // Even: this segment is the left child.
        root = if segment_index & 1 == 0 {
            node(&root, &sibling)
        } else {
            node(&sibling, &root)
        };
        segment_index >>= 1;
    }
    let climbed = climb(root, chunk_index, chunk_count, chunk_siblings)
        .ok_or(VerifyError::CommitmentMismatch)?;
    commitment_matches(commitment, climbed, data_size)
}

/// `node(data_tree_root, size_witness(n)) == commitment` (spec §5).
///
/// `n` is `data_size`.
pub fn verify_size(
    commitment: &DataCommitment,
    root: &DataTreeRoot,
    data_size: u64,
) -> Result<(), VerifyError> {
    if hash::data_commitment(root, data_size) == *commitment {
        Ok(())
    } else {
        Err(VerifyError::CommitmentMismatch)
    }
}

/// Last hash equals `size_witness(data_size)`. Returns the sibling hashes.
///
/// The witness is not a sibling. Climbing it would let a false `data_size`
/// pass when the proof carries the true witness (spec §5, §15 Q10).
fn require_size_witness(proof: &[u8], data_size: u64) -> Result<&[u8], VerifyError> {
    let sibling_len = proof.len() - HASH_SIZE;
    let (siblings, witness) = proof
        .split_at_checked(sibling_len)
        .expect("proof length includes the size witness");
    if witness == size_witness(data_size).as_bytes() {
        Ok(siblings)
    } else {
        Err(VerifyError::SizeWitnessMismatch)
    }
}

fn commitment_matches(
    commitment: &DataCommitment,
    root: NodeHash,
    data_size: u64,
) -> Result<(), VerifyError> {
    let root = DataTreeRoot::from_hash(root);
    if hash::data_commitment(&root, data_size) == *commitment {
        Ok(())
    } else {
        Err(VerifyError::CommitmentMismatch)
    }
}

fn exact_byte_length(bytes: &[u8], expected: u64) -> Result<(), u64> {
    match u64::try_from(bytes.len()) {
        Ok(actual) if actual == expected => Ok(()),
        Ok(actual) => Err(actual),
        // A length that does not fit `u64` is not the expected length. Fail closed.
        Err(_) => Err(u64::MAX),
    }
}

fn proof_byte_length(hash_count: u32) -> usize {
    // `32 × (k + 1)` (spec §5). `k + 1` here is at most `h + depth + 1`.
    usize::try_from(hash_count).expect("hash count fits usize") * HASH_SIZE
}
