//! Rejected proofs (spec §5, §6, §13). Length checks run before any hash.

use super::pattern;
use crate::DataTree;
use crate::chunk::chunk_root;
use crate::error::{SizeError, VerifyError};
use crate::geometry::Geometry;
use crate::hash::{self, leaf, node};
use crate::shape::depth;
use crate::types::NodeHash;
use crate::verify::{verify_chunk, verify_segment, verify_size};

#[test]
fn range_checks_fail_before_hashing() {
    let geometry = Geometry::TESTING;
    let data_size = 32_u64;
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let commitment = tree.data_commitment();
    let max_data_size = geometry.max_data_size();

    assert_eq!(
        verify_chunk(&geometry, &commitment, 0, 0, &data, &[]).unwrap_err(),
        VerifyError::Size(SizeError::DataSizeOutOfRange {
            data_size: 0,
            max_data_size
        })
    );
    assert_eq!(
        verify_segment(&geometry, &commitment, 0, 0, 0, &[], &[]).unwrap_err(),
        VerifyError::Size(SizeError::DataSizeOutOfRange {
            data_size: 0,
            max_data_size
        })
    );
    assert_eq!(
        verify_chunk(&geometry, &commitment, max_data_size + 1, 0, &data, &[]).unwrap_err(),
        VerifyError::Size(SizeError::DataSizeOutOfRange {
            data_size: max_data_size + 1,
            max_data_size
        })
    );
    // `C = 1`, so index 1 is past the tree. The empty proof is not hashed.
    assert_eq!(
        verify_chunk(&geometry, &commitment, data_size, 1, &data, &[]).unwrap_err(),
        VerifyError::Size(SizeError::ChunkIndexOutOfRange {
            chunk_index: 1,
            chunk_count: 1
        })
    );
    assert_eq!(
        verify_segment(
            &geometry,
            &commitment,
            data_size,
            0,
            geometry.segments_per_chunk(),
            &[],
            &[]
        )
        .unwrap_err(),
        VerifyError::Size(SizeError::SegmentIndexOutOfRange {
            segment_index: geometry.segments_per_chunk(),
            segments_per_chunk: geometry.segments_per_chunk()
        })
    );
}

#[test]
fn proof_length_is_checked_before_hashing() {
    let geometry = Geometry::TESTING;
    // Three full chunks: `depth(0, 3) = 2` (spec §5).
    let data_size = 96_u64;
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let commitment = tree.data_commitment();
    let chunk = data[..32].to_vec();
    let proof = tree.chunk_proof(0).unwrap();
    assert_eq!(depth(0, 3), 2);
    assert_eq!(proof.len(), 64);
    verify_chunk(&geometry, &commitment, data_size, 0, &chunk, &proof).unwrap();

    let mut too_long = proof.clone();
    too_long.extend_from_slice(&[0_u8; 32]);
    assert_eq!(
        verify_chunk(&geometry, &commitment, data_size, 0, &chunk, &too_long).unwrap_err(),
        VerifyError::InvalidProofLength {
            expected: 64,
            actual: 96
        }
    );
    assert_eq!(
        verify_chunk(&geometry, &commitment, data_size, 0, &chunk, &proof[..32]).unwrap_err(),
        VerifyError::InvalidProofLength {
            expected: 64,
            actual: 32
        }
    );
    let mut not_aligned = proof;
    not_aligned.push(0);
    assert_eq!(
        verify_chunk(&geometry, &commitment, data_size, 0, &chunk, &not_aligned).unwrap_err(),
        VerifyError::InvalidProofLength {
            expected: 64,
            actual: 65
        }
    );

    // `C = 2`: an empty proof is one hash too short (spec §5).
    let data_size = 64_u64;
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let chunk = data[..32].to_vec();
    assert_eq!(depth(0, 2), 1);
    assert_eq!(
        verify_chunk(
            &geometry,
            &tree.data_commitment(),
            data_size,
            0,
            &chunk,
            &[]
        )
        .unwrap_err(),
        VerifyError::InvalidProofLength {
            expected: 32,
            actual: 0
        }
    );
}

#[test]
fn byte_length_is_checked_before_hashing() {
    let geometry = Geometry::TESTING;
    let data_size = 32_u64;
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let commitment = tree.data_commitment();
    let proof = tree.chunk_proof(0).unwrap();
    assert_eq!(
        verify_chunk(&geometry, &commitment, data_size, 0, &data[..31], &proof).unwrap_err(),
        VerifyError::InvalidChunkLength {
            expected: 32,
            actual: 31
        }
    );
    let mut extra = data.clone();
    extra.push(0);
    assert_eq!(
        verify_chunk(&geometry, &commitment, data_size, 0, &extra, &proof).unwrap_err(),
        VerifyError::InvalidChunkLength {
            expected: 32,
            actual: 33
        }
    );

    let segment_proof = tree.segment_proof(0, 0, &data).unwrap();
    let segment = &data[..8];
    assert_eq!(
        verify_segment(
            &geometry,
            &commitment,
            data_size,
            0,
            0,
            &segment[..7],
            &segment_proof
        )
        .unwrap_err(),
        VerifyError::InvalidSegmentLength {
            expected: 8,
            actual: 7
        }
    );
    assert_eq!(
        verify_segment(
            &geometry,
            &commitment,
            data_size,
            0,
            0,
            &data[..9],
            &segment_proof
        )
        .unwrap_err(),
        VerifyError::InvalidSegmentLength {
            expected: 8,
            actual: 9
        }
    );
}

#[test]
fn zero_padding_is_rejected_and_hashes_differ() {
    // `n = 33`: the last chunk is one byte. Padding must not reach the hasher.
    let geometry = Geometry::TESTING;
    let data_size = 33_u64;
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let commitment = tree.data_commitment();
    let short = data[32..].to_vec();
    assert_eq!(short.len(), 1);
    let proof = tree.chunk_proof(1).unwrap();
    let mut padded_chunk = short.clone();
    padded_chunk.resize(32, 0);
    assert_ne!(
        chunk_root(&geometry, &short).unwrap(),
        chunk_root(&geometry, &padded_chunk).unwrap()
    );
    assert_eq!(
        verify_chunk(&geometry, &commitment, data_size, 1, &padded_chunk, &proof).unwrap_err(),
        VerifyError::InvalidChunkLength {
            expected: 1,
            actual: 32
        }
    );

    let segment_proof = tree.segment_proof(1, 0, &short).unwrap();
    let mut padded_segment = short.clone();
    padded_segment.resize(usize::try_from(geometry.segment_size()).unwrap(), 0);
    assert_ne!(leaf(&short), leaf(&padded_segment));
    assert_eq!(
        verify_segment(
            &geometry,
            &commitment,
            data_size,
            1,
            0,
            &padded_segment,
            &segment_proof
        )
        .unwrap_err(),
        VerifyError::InvalidSegmentLength {
            expected: 1,
            actual: geometry.segment_size()
        }
    );
}

#[test]
fn a_proof_for_another_index_does_not_verify() {
    let geometry = Geometry::TESTING;
    let data_size = 96_u64;
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let commitment = tree.data_commitment();
    let proof_0 = tree.chunk_proof(0).unwrap();
    let chunk_1 = &data[32..64];
    verify_chunk(
        &geometry,
        &commitment,
        data_size,
        1,
        chunk_1,
        &tree.chunk_proof(1).unwrap(),
    )
    .unwrap();
    // Same proof length (`depth` is 2 for both), different bytes.
    assert_eq!(
        verify_chunk(&geometry, &commitment, data_size, 1, chunk_1, &proof_0).unwrap_err(),
        VerifyError::CommitmentMismatch
    );
}

#[test]
fn wrong_size_with_the_same_chunk_count_mismatches() {
    // `n = 96` and `n = 80` are both `C = 3`. Chunk 0 is full either way.
    let geometry = Geometry::TESTING;
    let data_size = 96_u64;
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let chunk = &data[..32];
    let proof = tree.chunk_proof(0).unwrap();
    let wrong_data_size = 80_u64;
    assert_eq!(
        geometry.chunk_count(data_size).unwrap(),
        geometry.chunk_count(wrong_data_size).unwrap()
    );
    assert_eq!(geometry.chunk_len(wrong_data_size, 0).unwrap(), 32);
    assert_eq!(
        verify_chunk(
            &geometry,
            &tree.data_commitment(),
            wrong_data_size,
            0,
            chunk,
            &proof
        )
        .unwrap_err(),
        VerifyError::CommitmentMismatch
    );
}

#[test]
fn wrong_size_with_another_chunk_count_rejects_the_proof_length() {
    // Right-hand chunk: `depth(2, 3) = 1`, `depth(2, 4) = 2` (spec §5).
    let geometry = Geometry::TESTING;
    let data_size = 96_u64;
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let chunk = &data[64..96];
    let proof = tree.chunk_proof(2).unwrap();
    assert_eq!(proof.len(), 32);
    assert_eq!(depth(2, 3), 1);
    assert_eq!(depth(2, 4), 2);
    let wrong_data_size = 128_u64;
    assert_eq!(
        verify_chunk(
            &geometry,
            &tree.data_commitment(),
            wrong_data_size,
            2,
            chunk,
            &proof
        )
        .unwrap_err(),
        VerifyError::InvalidProofLength {
            expected: 64,
            actual: 32
        }
    );
}

#[test]
fn every_flipped_sibling_bit_mismatches() {
    let geometry = Geometry::TESTING;
    let data_size = 112_u64;
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let commitment = tree.data_commitment();

    let chunk = &data[64..96];
    let chunk_proof = tree.chunk_proof(2).unwrap();
    verify_chunk(&geometry, &commitment, data_size, 2, chunk, &chunk_proof).unwrap();
    assert_every_bit_flip_mismatches(&chunk_proof, |flipped_proof| {
        verify_chunk(&geometry, &commitment, data_size, 2, chunk, flipped_proof)
    });

    let chunk3 = &data[96..112];
    let segment = &chunk3[8..16];
    let segment_proof = tree.segment_proof(3, 1, chunk3).unwrap();
    verify_segment(
        &geometry,
        &commitment,
        data_size,
        3,
        1,
        segment,
        &segment_proof,
    )
    .unwrap();
    assert_every_bit_flip_mismatches(&segment_proof, |flipped_proof| {
        verify_segment(
            &geometry,
            &commitment,
            data_size,
            3,
            1,
            segment,
            flipped_proof,
        )
    });
}

fn assert_every_bit_flip_mismatches(
    proof: &[u8],
    check: impl Fn(&[u8]) -> Result<(), VerifyError>,
) {
    assert!(!proof.is_empty());
    for byte_index in 0..proof.len() {
        for bit in 0..8 {
            let mut flipped_proof = proof.to_vec();
            flipped_proof[byte_index] ^= 1 << bit;
            assert_eq!(
                check(&flipped_proof).unwrap_err(),
                VerifyError::CommitmentMismatch,
                "byte {byte_index} bit {bit}"
            );
        }
    }
}

#[test]
fn a_64_byte_segment_equal_to_two_hashes_is_a_leaf() {
    // spec §3: a segment of `l ‖ r` is `leaf`, never `node(l, r)`.
    let left = NodeHash([1_u8; 32]);
    let right = NodeHash([2_u8; 32]);
    let mut bytes = [0_u8; 64];
    bytes[..32].copy_from_slice(left.as_bytes());
    bytes[32..].copy_from_slice(right.as_bytes());
    assert_ne!(leaf(&bytes), node(&left, &right));
    let geometry = Geometry::new(64, 1, 1).unwrap();
    assert_eq!(geometry.height(), 0);
    assert_eq!(chunk_root(&geometry, &bytes).unwrap(), leaf(&bytes));
}

#[test]
fn verify_size_rejects_a_different_size_with_no_range_check() {
    // spec §5: `verify_size` takes any `u64`, including 0. No geometry.
    let geometry = Geometry::TESTING;
    let data_size = 32_u64;
    let tree =
        DataTree::from_bytes(geometry, &pattern(usize::try_from(data_size).unwrap())).unwrap();
    let commitment = tree.data_commitment();
    let root = tree.data_tree_root();
    verify_size(&commitment, &root, data_size).unwrap();
    assert_eq!(
        verify_size(&commitment, &root, 0).unwrap_err(),
        VerifyError::CommitmentMismatch
    );
    assert_eq!(
        verify_size(&commitment, &root, data_size - 1).unwrap_err(),
        VerifyError::CommitmentMismatch
    );
}

#[test]
fn an_unanchored_commitment_passes_only_when_the_path_shape_matches() {
    // spec §6: a commitment derived from an unauthenticated `(root, n)` repeats
    // the verifier's last step. It passes for a left-subtree chunk whose path
    // has the same shape, and the anchored commitment does not.
    //
    // Test geometry, `n = 96` (`C = 3`), `c = 0`, `depth = 2`.
    // `n = 97` is `C = 4`, and `depth(0, 4) = 2` as well. The two-hash proof
    // `[R1, R2]` climbs to the original root under `C = 4`.
    let geometry = Geometry::TESTING;
    let data_size = 96_u64;
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let anchored = tree.data_commitment();
    let root = tree.data_tree_root();
    let chunk = &data[..32];
    let proof = tree.chunk_proof(0).unwrap();
    assert_eq!(depth(0, 3), 2);
    assert_eq!(depth(0, 4), 2);
    verify_chunk(&geometry, &anchored, data_size, 0, chunk, &proof).unwrap();

    let wrong_data_size = 97_u64;
    let rederived = hash::data_commitment(&root, wrong_data_size);
    verify_chunk(&geometry, &rederived, wrong_data_size, 0, chunk, &proof).unwrap();
    assert_eq!(
        verify_chunk(&geometry, &anchored, wrong_data_size, 0, chunk, &proof).unwrap_err(),
        VerifyError::CommitmentMismatch
    );

    // The right-hand chunk does not share that path length.
    let right = &data[64..96];
    let right_proof = tree.chunk_proof(2).unwrap();
    assert_eq!(depth(2, 3), 1);
    assert_eq!(right_proof.len(), 32);
    assert_eq!(
        verify_chunk(
            &geometry,
            &rederived,
            wrong_data_size,
            2,
            right,
            &right_proof
        )
        .unwrap_err(),
        VerifyError::InvalidProofLength {
            expected: 64,
            actual: 32
        }
    );
}
