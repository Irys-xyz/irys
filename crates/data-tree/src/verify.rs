//! Proof checks (spec §5).
//!
//! The order is fixed: range, byte length, proof length, then hashing.
//! The expected proof length is `32 × k` hashes (spec §5), so a length that is
//! not a multiple of 32 fails that check. Length checks run before any hashing
//! (spec §10).

use crate::chunk::chunk_root;
use crate::error::VerifyError;
use crate::geometry::Geometry;
use crate::hash::{self, leaf, node};
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
    let expected_proof = proof_byte_length(depth(chunk_index, chunk_count));
    if proof.len() != expected_proof {
        return Err(VerifyError::InvalidProofLength {
            expected: expected_proof,
            actual: proof.len(),
        });
    }
    // The length check enforces `1..=chunk_size`, which is `chunk_root`'s precondition.
    let root = chunk_root(geometry, bytes).expect("chunk length is in 1..=chunk_size");
    let climbed =
        climb(root, chunk_index, chunk_count, proof).ok_or(VerifyError::CommitmentMismatch)?;
    commitment_matches(commitment, climbed, data_size)
}

/// Check one segment against an anchored commitment (spec §5, §6).
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
    // `h + depth(c, C)` (spec §5). `h` is at most `MAX_SUBTREE_HEIGHT` and
    // `depth` fits the bit width of a chunk count, so the sum fits `u32`.
    let hash_count = geometry.height() + depth(chunk_index, chunk_count);
    let expected_proof = proof_byte_length(hash_count);
    if proof.len() != expected_proof {
        return Err(VerifyError::InvalidProofLength {
            expected: expected_proof,
            actual: proof.len(),
        });
    }
    let in_chunk_bytes = usize::try_from(geometry.height()).expect("height fits usize") * HASH_SIZE;
    // The length check guarantees `proof` covers the `h` in-chunk siblings.
    let (in_chunk, chunk_proof) = proof
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
    let climbed = climb(root, chunk_index, chunk_count, chunk_proof)
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
    // `32 × k` (spec §5). `k` here is at most `h + depth`, which fits `usize`.
    usize::try_from(hash_count).expect("hash count fits usize") * HASH_SIZE
}
