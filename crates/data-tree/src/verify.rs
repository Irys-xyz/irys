//! Proof checks (spec §5).
//!
//! The order is fixed: range, byte length, proof length, then hashing.
//! A proof whose length is not a multiple of 32 fails the length check,
//! because the expected length is always a multiple of 32 (spec §10).

use crate::chunk::chunk_root;
use crate::error::VerifyError;
use crate::geometry::Geometry;
use crate::hash::{self, leaf, node};
use crate::shape::{climb, depth};
use crate::types::{DataCommitment, DataTreeRoot, NodeHash};

/// Check one chunk against an anchored commitment (spec §5, §6).
pub fn verify_chunk(
    geom: &Geometry,
    commitment: &DataCommitment,
    data_size: u64,
    c: u32,
    bytes: &[u8],
    proof: &[u8],
) -> Result<(), VerifyError> {
    let chunk_len = geom.chunk_len(data_size, c)?;
    if let Err(actual) = exact_len(bytes, chunk_len) {
        return Err(VerifyError::InvalidChunkLength {
            expected: chunk_len,
            actual,
        });
    }
    let chunk_count = geom.chunk_count(data_size)?;
    let expected_proof = proof_byte_len(depth(c, chunk_count));
    if proof.len() != expected_proof {
        return Err(VerifyError::InvalidProofLength {
            expected: expected_proof,
            actual: proof.len(),
        });
    }
    let root = match chunk_root(geom, bytes) {
        Ok(root) => root,
        Err(_) => {
            return Err(VerifyError::InvalidChunkLength {
                expected: chunk_len,
                actual: len_u64(bytes),
            });
        }
    };
    let climbed = climb(root, c, chunk_count, proof).ok_or(VerifyError::CommitmentMismatch)?;
    commitment_matches(commitment, climbed, data_size)
}

/// Check one segment against an anchored commitment (spec §5, §6).
pub fn verify_segment(
    geom: &Geometry,
    commitment: &DataCommitment,
    data_size: u64,
    c: u32,
    j: u32,
    bytes: &[u8],
    proof: &[u8],
) -> Result<(), VerifyError> {
    let segment_len = geom.segment_len(data_size, c, j)?;
    if let Err(actual) = exact_len(bytes, segment_len) {
        return Err(VerifyError::InvalidSegmentLength {
            expected: segment_len,
            actual,
        });
    }
    let chunk_count = geom.chunk_count(data_size)?;
    let hash_count = match geom.height().checked_add(depth(c, chunk_count)) {
        Some(count) => count,
        None => {
            return Err(VerifyError::InvalidProofLength {
                expected: usize::MAX,
                actual: proof.len(),
            });
        }
    };
    let expected_proof = proof_byte_len(hash_count);
    if proof.len() != expected_proof {
        return Err(VerifyError::InvalidProofLength {
            expected: expected_proof,
            actual: proof.len(),
        });
    }
    let in_chunk_bytes = usize::try_from(geom.height())
        .unwrap_or(0)
        .saturating_mul(32);
    let Some((in_chunk, chunk_proof)) = proof.split_at_checked(in_chunk_bytes) else {
        return Err(VerifyError::InvalidProofLength {
            expected: expected_proof,
            actual: proof.len(),
        });
    };
    let mut root = leaf(bytes);
    let mut index = j;
    for sibling in in_chunk.chunks_exact(32) {
        let Ok(bytes) = <[u8; 32]>::try_from(sibling) else {
            return Err(VerifyError::InvalidProofLength {
                expected: expected_proof,
                actual: proof.len(),
            });
        };
        let sibling = NodeHash(bytes);
        root = if index & 1 == 0 {
            node(&root, &sibling)
        } else {
            node(&sibling, &root)
        };
        index >>= 1;
    }
    let climbed =
        climb(root, c, chunk_count, chunk_proof).ok_or(VerifyError::CommitmentMismatch)?;
    commitment_matches(commitment, climbed, data_size)
}

/// `node(data_tree_root, size_witness(n)) == commitment` (spec §5).
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

fn exact_len(bytes: &[u8], expected: u64) -> Result<(), u64> {
    match u64::try_from(bytes.len()) {
        Ok(actual) if actual == expected => Ok(()),
        Ok(actual) => Err(actual),
        Err(_) => Err(u64::MAX),
    }
}

fn len_u64(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}

fn proof_byte_len(hash_count: u32) -> usize {
    usize::try_from(hash_count)
        .ok()
        .and_then(|count| count.checked_mul(32))
        .unwrap_or(usize::MAX)
}
