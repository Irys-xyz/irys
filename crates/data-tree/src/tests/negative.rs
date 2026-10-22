//! Rejected proofs (spec §5, §6, §13). Length checks run before any hash.

use super::pattern;
use crate::DataTree;
use crate::chunk::chunk_root;
use crate::error::{SizeError, VerifyError};
use crate::geometry::Geometry;
use crate::hash::{self, leaf, node};
use crate::shape::depth;
use crate::types::{HASH_SIZE, NodeHash};
use crate::verify::{verify_chunk, verify_segment, verify_size};

#[test]
fn range_checks_fail_before_hashing() {
    let geometry = Geometry::TESTING;
    let data_size = geometry.chunk_size();
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
    // `C = 1`, so index 1 is past the tree. The proof is not hashed.
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
    let chunk_bytes = testing_chunk_bytes();
    // Three full chunks: `depth(0, 3) = 2` (spec §5).
    let data_size = 3 * geometry.chunk_size();
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let commitment = tree.data_commitment();
    let chunk = data[..chunk_bytes].to_vec();
    let proof = tree.chunk_proof(0).unwrap();
    assert_eq!(depth(0, 3), Some(2));
    // Two siblings and the size witness (spec §5).
    assert_eq!(proof.len(), 3 * HASH_SIZE);
    verify_chunk(&geometry, &commitment, data_size, 0, &chunk, &proof).unwrap();

    let mut too_long = proof.clone();
    too_long.extend_from_slice(&[0_u8; HASH_SIZE]);
    assert_eq!(
        verify_chunk(&geometry, &commitment, data_size, 0, &chunk, &too_long).unwrap_err(),
        VerifyError::InvalidProofLength {
            expected: 3 * HASH_SIZE,
            actual: 4 * HASH_SIZE
        }
    );
    // The old form is the siblings without the witness: one hash too short.
    assert_eq!(
        verify_chunk(
            &geometry,
            &commitment,
            data_size,
            0,
            &chunk,
            &proof[..proof.len() - HASH_SIZE]
        )
        .unwrap_err(),
        VerifyError::InvalidProofLength {
            expected: 3 * HASH_SIZE,
            actual: 2 * HASH_SIZE
        }
    );
    let mut not_aligned = proof;
    not_aligned.push(0);
    assert_eq!(
        verify_chunk(&geometry, &commitment, data_size, 0, &chunk, &not_aligned).unwrap_err(),
        VerifyError::InvalidProofLength {
            expected: 3 * HASH_SIZE,
            actual: 3 * HASH_SIZE + 1
        }
    );

    // `C = 2`: an empty proof is two hashes too short (one sibling and the witness).
    let data_size = 2 * geometry.chunk_size();
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let chunk = data[..chunk_bytes].to_vec();
    assert_eq!(depth(0, 2), Some(1));
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
            expected: 2 * HASH_SIZE,
            actual: 0
        }
    );
}

#[test]
fn an_empty_proof_is_rejected_for_one_chunk() {
    // `C = 1`: the chunk proof is `[size_witness(n)]`. Empty is the old form.
    let geometry = Geometry::TESTING;
    let data_size = geometry.chunk_size();
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let proof = tree.chunk_proof(0).unwrap();
    assert_eq!(proof.len(), HASH_SIZE);
    assert_eq!(depth(0, 1), Some(0));
    verify_chunk(
        &geometry,
        &tree.data_commitment(),
        data_size,
        0,
        &data,
        &proof,
    )
    .unwrap();
    assert_eq!(
        verify_chunk(&geometry, &tree.data_commitment(), data_size, 0, &data, &[]).unwrap_err(),
        VerifyError::InvalidProofLength {
            expected: HASH_SIZE,
            actual: 0
        }
    );
    let segment = &data[..testing_segment_bytes()];
    let segment_proof = tree.segment_proof(0, 0, &data).unwrap();
    let segment_hashes = geometry.height() + 1;
    assert_eq!(
        segment_proof.len(),
        usize::try_from(segment_hashes).unwrap() * HASH_SIZE
    );
    assert_eq!(
        verify_segment(
            &geometry,
            &tree.data_commitment(),
            data_size,
            0,
            0,
            segment,
            &[]
        )
        .unwrap_err(),
        VerifyError::InvalidProofLength {
            expected: segment_proof.len(),
            actual: 0
        }
    );
}

#[test]
fn a_wrong_size_witness_is_rejected() {
    // The last hash is `size_witness(n + 1)`, not `size_witness(n)` (spec §5).
    let geometry = Geometry::TESTING;
    let data_size = 3 * geometry.chunk_size();
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let commitment = tree.data_commitment();
    let chunk = &data[..testing_chunk_bytes()];
    let mut proof = tree.chunk_proof(0).unwrap();
    verify_chunk(&geometry, &commitment, data_size, 0, chunk, &proof).unwrap();
    let wrong_witness = hash::size_witness(&geometry, data_size + 1);
    let witness_start = proof.len() - HASH_SIZE;
    proof[witness_start..].copy_from_slice(wrong_witness.as_bytes());
    assert_eq!(
        verify_chunk(&geometry, &commitment, data_size, 0, chunk, &proof).unwrap_err(),
        VerifyError::SizeWitnessMismatch
    );

    let chunk3_len = testing_chunk_bytes();
    let chunk3 = &data[2 * chunk3_len..3 * chunk3_len];
    let mut segment_proof = tree.segment_proof(2, 0, chunk3).unwrap();
    let segment = &chunk3[..testing_segment_bytes()];
    verify_segment(
        &geometry,
        &commitment,
        data_size,
        2,
        0,
        segment,
        &segment_proof,
    )
    .unwrap();
    let witness_start = segment_proof.len() - HASH_SIZE;
    segment_proof[witness_start..].copy_from_slice(wrong_witness.as_bytes());
    assert_eq!(
        verify_segment(
            &geometry,
            &commitment,
            data_size,
            2,
            0,
            segment,
            &segment_proof
        )
        .unwrap_err(),
        VerifyError::SizeWitnessMismatch
    );
}

#[test]
fn byte_length_is_checked_before_hashing() {
    let geometry = Geometry::TESTING;
    let data_size = geometry.chunk_size();
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let commitment = tree.data_commitment();
    let proof = tree.chunk_proof(0).unwrap();
    let short_chunk = usize::try_from(data_size - 1).unwrap();
    assert_eq!(
        verify_chunk(
            &geometry,
            &commitment,
            data_size,
            0,
            &data[..short_chunk],
            &proof
        )
        .unwrap_err(),
        VerifyError::InvalidChunkLength {
            expected: data_size,
            actual: data_size - 1
        }
    );
    let mut extra = data.clone();
    extra.push(0);
    assert_eq!(
        verify_chunk(&geometry, &commitment, data_size, 0, &extra, &proof).unwrap_err(),
        VerifyError::InvalidChunkLength {
            expected: data_size,
            actual: data_size + 1
        }
    );

    let segment_bytes = testing_segment_bytes();
    let segment_proof = tree.segment_proof(0, 0, &data).unwrap();
    let segment = &data[..segment_bytes];
    assert_eq!(
        verify_segment(
            &geometry,
            &commitment,
            data_size,
            0,
            0,
            &segment[..segment_bytes - 1],
            &segment_proof
        )
        .unwrap_err(),
        VerifyError::InvalidSegmentLength {
            expected: geometry.segment_size(),
            actual: geometry.segment_size() - 1
        }
    );
    assert_eq!(
        verify_segment(
            &geometry,
            &commitment,
            data_size,
            0,
            0,
            &data[..segment_bytes + 1],
            &segment_proof
        )
        .unwrap_err(),
        VerifyError::InvalidSegmentLength {
            expected: geometry.segment_size(),
            actual: geometry.segment_size() + 1
        }
    );
}

#[test]
fn zero_padding_is_rejected_and_hashes_differ() {
    // `n = chunk_size + 1`: the last chunk is one byte. Padding must not reach the hasher.
    let geometry = Geometry::TESTING;
    let chunk_bytes = testing_chunk_bytes();
    let data_size = geometry.chunk_size() + 1;
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let commitment = tree.data_commitment();
    let short = data[chunk_bytes..].to_vec();
    assert_eq!(short.len(), 1);
    let proof = tree.chunk_proof(1).unwrap();
    let mut padded_chunk = short.clone();
    padded_chunk.resize(chunk_bytes, 0);
    assert_ne!(
        chunk_root(&geometry, &short).unwrap(),
        chunk_root(&geometry, &padded_chunk).unwrap()
    );
    assert_eq!(
        verify_chunk(&geometry, &commitment, data_size, 1, &padded_chunk, &proof).unwrap_err(),
        VerifyError::InvalidChunkLength {
            expected: 1,
            actual: geometry.chunk_size()
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
    let chunk_bytes = testing_chunk_bytes();
    let data_size = 3 * geometry.chunk_size();
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let commitment = tree.data_commitment();
    let proof_0 = tree.chunk_proof(0).unwrap();
    let chunk_1 = &data[chunk_bytes..2 * chunk_bytes];
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
fn a_wrong_data_size_with_the_true_witness_is_rejected() {
    // Three full chunks and two and a half chunks are both `C = 3`. Chunk 0 is full.
    // The proof carries `size_witness` of the real size. The length matches, so
    // the witness check rejects the claim before the climb (spec §5, §15 Q10).
    let geometry = Geometry::TESTING;
    let data_size = 3 * geometry.chunk_size();
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let chunk = &data[..testing_chunk_bytes()];
    let proof = tree.chunk_proof(0).unwrap();
    let wrong_data_size = 2 * geometry.chunk_size() + geometry.chunk_size() / 2;
    assert_eq!(
        geometry.chunk_count(data_size).unwrap(),
        geometry.chunk_count(wrong_data_size).unwrap()
    );
    assert_eq!(
        geometry.chunk_len(wrong_data_size, 0).unwrap(),
        geometry.chunk_size()
    );
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
        VerifyError::SizeWitnessMismatch
    );
}

#[test]
fn wrong_size_with_another_chunk_count_rejects_the_proof_length() {
    // Right-hand chunk: `depth(2, 3) = 1`, `depth(2, 4) = 2` (spec §5).
    let geometry = Geometry::TESTING;
    let chunk_bytes = testing_chunk_bytes();
    let data_size = 3 * geometry.chunk_size();
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let chunk = &data[2 * chunk_bytes..3 * chunk_bytes];
    let proof = tree.chunk_proof(2).unwrap();
    // One sibling and the size witness (spec §5).
    assert_eq!(proof.len(), 2 * HASH_SIZE);
    assert_eq!(depth(2, 3), Some(1));
    assert_eq!(depth(2, 4), Some(2));
    let wrong_data_size = 4 * geometry.chunk_size();
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
            expected: 3 * HASH_SIZE,
            actual: 2 * HASH_SIZE
        }
    );
}

#[test]
fn every_flipped_sibling_bit_mismatches() {
    let geometry = Geometry::TESTING;
    let chunk_bytes = testing_chunk_bytes();
    let segment_bytes = testing_segment_bytes();
    // Three full chunks and a half chunk, so the last chunk still has two segments.
    let data_size = 3 * geometry.chunk_size() + geometry.chunk_size() / 2;
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let commitment = tree.data_commitment();

    let chunk = &data[2 * chunk_bytes..3 * chunk_bytes];
    let chunk_proof = tree.chunk_proof(2).unwrap();
    verify_chunk(&geometry, &commitment, data_size, 2, chunk, &chunk_proof).unwrap();
    assert_every_bit_flip_mismatches(&chunk_proof, |flipped_proof| {
        verify_chunk(&geometry, &commitment, data_size, 2, chunk, flipped_proof)
    });

    let chunk3 = &data[3 * chunk_bytes..3 * chunk_bytes + chunk_bytes / 2];
    let segment = &chunk3[segment_bytes..2 * segment_bytes];
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
    assert!(
        proof.len() >= HASH_SIZE,
        "a proof ends with the size witness"
    );
    let witness_start = proof.len() - HASH_SIZE;
    for byte_index in 0..proof.len() {
        // A flipped witness fails the witness check. A flipped sibling fails
        // the commitment check (spec §5).
        let expected = if byte_index >= witness_start {
            VerifyError::SizeWitnessMismatch
        } else {
            VerifyError::CommitmentMismatch
        };
        for bit in 0..u8::BITS {
            let mut flipped_proof = proof.to_vec();
            flipped_proof[byte_index] ^= 1 << bit;
            assert_eq!(
                check(&flipped_proof).unwrap_err(),
                expected,
                "byte {byte_index} bit {bit}"
            );
        }
    }
}

#[test]
fn a_64_byte_segment_equal_to_two_hashes_is_a_leaf() {
    // spec §3: a segment of `l ‖ r` is `leaf`, never `node(l, r)`.
    let left = NodeHash::from([1_u8; HASH_SIZE]);
    let right = NodeHash::from([2_u8; HASH_SIZE]);
    // A segment equal to `l ‖ r`: two digests (spec §10).
    let mut bytes = [0_u8; HASH_SIZE * 2];
    bytes[..HASH_SIZE].copy_from_slice(left.as_bytes());
    bytes[HASH_SIZE..].copy_from_slice(right.as_bytes());
    assert_ne!(leaf(&bytes), node(&left, &right));
    let geometry = Geometry::new(u64::try_from(HASH_SIZE * 2).unwrap(), 1, 1).unwrap();
    assert_eq!(geometry.height(), 0);
    assert_eq!(chunk_root(&geometry, &bytes).unwrap(), leaf(&bytes));
}

#[test]
fn verify_size_rejects_a_different_size_with_no_range_check() {
    // spec §5: `verify_size` takes `(G, data_commitment, data_tree_root, data_size)`
    // and no proof bytes. Any `u64` `data_size` is taken as input, including 0.
    // There is no range check. The comparison fails unless `data_size` and the
    // geometry are the committed ones.
    let geometry = Geometry::TESTING;
    let data_size = geometry.chunk_size();
    let tree =
        DataTree::from_bytes(geometry, &pattern(usize::try_from(data_size).unwrap())).unwrap();
    let commitment = tree.data_commitment();
    let root = tree.data_tree_root();
    verify_size(&geometry, &commitment, &root, data_size).unwrap();
    assert_eq!(
        verify_size(&geometry, &commitment, &root, 0).unwrap_err(),
        VerifyError::CommitmentMismatch
    );
    assert_eq!(
        verify_size(&geometry, &commitment, &root, data_size - 1).unwrap_err(),
        VerifyError::CommitmentMismatch
    );
}

#[test]
fn an_unanchored_commitment_passes_only_when_the_path_shape_matches() {
    // spec §6: a commitment derived from an unauthenticated
    // `(data_tree_root, data_size)` under a chosen geometry repeats the
    // verifier's last step. This case is one chunk proof, same geometry:
    // chunk 0, whose step sequence is the same under a larger `C`, and
    // `chunk_len` matches. The pass is against the re-derived commitment.
    // The anchored commitment rejects it. A smaller `C` and a moved index
    // are the next test.
    //
    // Test geometry, three full chunks (`C = 3`), `c = 0`, `depth = 2`.
    // One byte more is `C = 4`, and `depth(0, 4) = 2` as well. The sibling list
    // climbs to the original root under `C = 4` when the proof's witness is
    // `size_witness` of the claimed size. The true witness does not.
    let geometry = Geometry::TESTING;
    let chunk_bytes = testing_chunk_bytes();
    let data_size = 3 * geometry.chunk_size();
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let anchored = tree.data_commitment();
    let root = tree.data_tree_root();
    let chunk = &data[..chunk_bytes];
    let proof = tree.chunk_proof(0).unwrap();
    assert_eq!(depth(0, 3), Some(2));
    assert_eq!(depth(0, 4), Some(2));
    assert_eq!(proof.len(), 3 * HASH_SIZE);
    verify_chunk(&geometry, &anchored, data_size, 0, chunk, &proof).unwrap();

    let wrong_data_size = data_size + 1;
    let rederived = hash::data_commitment(&geometry, &root, wrong_data_size);
    // Same depth, so the length matches. The true witness is not the claimed size.
    assert_eq!(
        verify_chunk(&geometry, &rederived, wrong_data_size, 0, chunk, &proof).unwrap_err(),
        VerifyError::SizeWitnessMismatch
    );
    assert_eq!(
        verify_chunk(&geometry, &anchored, wrong_data_size, 0, chunk, &proof).unwrap_err(),
        VerifyError::SizeWitnessMismatch
    );

    let mut claimed_witness = proof;
    let witness = hash::size_witness(&geometry, wrong_data_size);
    let witness_start = claimed_witness.len() - HASH_SIZE;
    claimed_witness[witness_start..].copy_from_slice(witness.as_bytes());
    verify_chunk(
        &geometry,
        &rederived,
        wrong_data_size,
        0,
        chunk,
        &claimed_witness,
    )
    .unwrap();
    assert_eq!(
        verify_chunk(
            &geometry,
            &anchored,
            wrong_data_size,
            0,
            chunk,
            &claimed_witness
        )
        .unwrap_err(),
        VerifyError::CommitmentMismatch
    );

    // Under this claimed size the count is 4. Chunk 2's proof is shorter than
    // `depth(2, 4) + 1`, so the length check rejects it. That is this `C'`
    // only: the same proof matches chunk 1 of `C' = 2` (the next test).
    assert_eq!(depth(2, 4), Some(2));
    let right = &data[2 * chunk_bytes..3 * chunk_bytes];
    let right_proof = tree.chunk_proof(2).unwrap();
    assert_eq!(depth(2, 3), Some(1));
    assert_eq!(right_proof.len(), 2 * HASH_SIZE);
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
            expected: 3 * HASH_SIZE,
            actual: 2 * HASH_SIZE
        }
    );
}

#[test]
fn an_unanchored_commitment_passes_for_a_smaller_count_and_a_moved_index() {
    // spec §6: for a chunk proof, same geometry, the left/right steps and
    // `chunk_len(n', c') == chunk_len(n, c)` are the condition. The claimed
    // size may be smaller, and the claimed index may differ. The bytes are
    // the honest chunk. The pass is against the re-derived commitment. The
    // anchored commitment rejects both.
    let geometry = Geometry::TESTING;
    let chunk_bytes = testing_chunk_bytes();

    // Smaller `C`, same index. `n = 128` (`C = 4`), chunk 0, as chunk 0 of
    // `n' = 96` (`C' = 3`). Both step sequences have length 2. `chunk_len` is
    // one full chunk on each side.
    let data_size = 4 * geometry.chunk_size();
    let claimed_data_size = 3 * geometry.chunk_size();
    assert_eq!(geometry.chunk_count(data_size).unwrap(), 4);
    assert_eq!(geometry.chunk_count(claimed_data_size).unwrap(), 3);
    assert_eq!(depth(0, 4), depth(0, 3));
    assert_eq!(
        geometry.chunk_len(data_size, 0).unwrap(),
        geometry.chunk_len(claimed_data_size, 0).unwrap()
    );
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let chunk = &data[..chunk_bytes];
    let honest = tree.chunk_proof(0).unwrap();
    verify_chunk(
        &geometry,
        &tree.data_commitment(),
        data_size,
        0,
        chunk,
        &honest,
    )
    .unwrap();
    let proof = proof_ending_with_size_witness(&geometry, honest, claimed_data_size);
    let rederived = hash::data_commitment(&geometry, &tree.data_tree_root(), claimed_data_size);
    verify_chunk(&geometry, &rederived, claimed_data_size, 0, chunk, &proof).unwrap();
    assert_eq!(
        verify_chunk(
            &geometry,
            &tree.data_commitment(),
            claimed_data_size,
            0,
            chunk,
            &proof,
        )
        .unwrap_err(),
        VerifyError::CommitmentMismatch
    );

    // Moved index, smaller `C`. `n = 96` (`C = 3`), chunk 2, as chunk 1 of
    // `n' = 64` (`C' = 2`). Both step sequences have length 1.
    let data_size = 3 * geometry.chunk_size();
    let claimed_data_size = 2 * geometry.chunk_size();
    assert_eq!(geometry.chunk_count(data_size).unwrap(), 3);
    assert_eq!(geometry.chunk_count(claimed_data_size).unwrap(), 2);
    assert_eq!(depth(2, 3), depth(1, 2));
    assert_eq!(
        geometry.chunk_len(data_size, 2).unwrap(),
        geometry.chunk_len(claimed_data_size, 1).unwrap()
    );
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let chunk = &data[2 * chunk_bytes..3 * chunk_bytes];
    let honest = tree.chunk_proof(2).unwrap();
    verify_chunk(
        &geometry,
        &tree.data_commitment(),
        data_size,
        2,
        chunk,
        &honest,
    )
    .unwrap();
    let proof = proof_ending_with_size_witness(&geometry, honest, claimed_data_size);
    let rederived = hash::data_commitment(&geometry, &tree.data_tree_root(), claimed_data_size);
    verify_chunk(&geometry, &rederived, claimed_data_size, 1, chunk, &proof).unwrap();
    assert_eq!(
        verify_chunk(
            &geometry,
            &tree.data_commitment(),
            claimed_data_size,
            1,
            chunk,
            &proof,
        )
        .unwrap_err(),
        VerifyError::CommitmentMismatch
    );
}

#[test]
fn an_unanchored_segment_proof_passes_for_the_spec_region() {
    // spec §6: test geometry, same geometry. Honest `n = 56`, chunk 1, segment 0.
    // `chunk_len` is 24 and `s` is 3. The rewritten segment proof passes as that
    // segment for every claimed size in `49..=64` (`chunk_len` 17 to 32). Sizes
    // in `33..=48` do not. `chunk_len` need not match. The anchored commitment
    // rejects every claim whose size is not 56. A chunk proof of those bytes
    // still requires `chunk_len`.
    let geometry = Geometry::TESTING;
    let chunk_size = geometry.chunk_size();
    let segment_size = geometry.segment_size();
    let chunk_index = 1;
    let segment_index = 0;
    // One empty segment at the end of the chunk, so `s = 3`.
    let data_segments = geometry.segments_per_chunk() - 1;
    let honest_chunk_len = segment_size * u64::from(data_segments);
    let data_size = chunk_size + honest_chunk_len;
    // `C' = 2` starts one byte into the second chunk and ends at two full chunks.
    let smallest_two_chunks = chunk_size + 1;
    let two_full_chunks = chunk_size * 2;
    // Segment 0's in-chunk siblings start at 1 and 2. `s' >= 3` pins neither.
    let first_accepted = chunk_size + u64::from(data_segments - 1) * segment_size + 1;
    let first_full_segment = chunk_size + segment_size;
    assert_eq!(data_size, 56, "spec §6");
    assert_eq!(honest_chunk_len, 24, "spec §6");
    assert_eq!(smallest_two_chunks, 33, "spec §6");
    assert_eq!(first_accepted, 49, "spec §6");
    assert_eq!(two_full_chunks, 64, "spec §6");
    assert_eq!(geometry.chunk_count(data_size).unwrap(), 2);
    assert_eq!(
        geometry.chunk_len(data_size, chunk_index).unwrap(),
        honest_chunk_len
    );
    assert_eq!(geometry.chunk_len(first_accepted, chunk_index).unwrap(), 17);
    assert_eq!(
        geometry.chunk_len(two_full_chunks, chunk_index).unwrap(),
        32
    );
    assert_eq!(
        geometry
            .segment_len(data_size, chunk_index, segment_index)
            .unwrap(),
        segment_size
    );

    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let chunk_start = usize::try_from(chunk_size).unwrap();
    let chunk_end = chunk_start + usize::try_from(honest_chunk_len).unwrap();
    let chunk = &data[chunk_start..chunk_end];
    let segment_end = usize::try_from(segment_size).unwrap();
    let segment = &chunk[..segment_end];
    let honest = tree
        .segment_proof(chunk_index, segment_index, chunk)
        .unwrap();
    let anchored = tree.data_commitment();
    let root = tree.data_tree_root();

    let claimed_full = two_full_chunks;
    let rederived_full = hash::data_commitment(&geometry, &root, claimed_full);
    assert_eq!(
        verify_segment(
            &geometry,
            &rederived_full,
            claimed_full,
            chunk_index,
            segment_index,
            segment,
            &honest,
        )
        .unwrap_err(),
        VerifyError::SizeWitnessMismatch
    );

    for claimed in smallest_two_chunks..=two_full_chunks {
        let proof = proof_ending_with_size_witness(&geometry, honest.clone(), claimed);
        let rederived = hash::data_commitment(&geometry, &root, claimed);
        let result = verify_segment(
            &geometry,
            &rederived,
            claimed,
            chunk_index,
            segment_index,
            segment,
            &proof,
        );
        if claimed >= first_accepted {
            result.unwrap_or_else(|err| panic!("n' = {claimed} should pass: {err}"));
            assert_eq!(
                geometry
                    .segment_len(claimed, chunk_index, segment_index)
                    .unwrap(),
                segment_size,
                "n' = {claimed}"
            );
            let anchored_result = verify_segment(
                &geometry,
                &anchored,
                claimed,
                chunk_index,
                segment_index,
                segment,
                &proof,
            );
            if claimed == data_size {
                anchored_result.unwrap_or_else(|err| panic!("honest size: {err}"));
            } else {
                assert_eq!(
                    anchored_result.unwrap_err(),
                    VerifyError::CommitmentMismatch,
                    "n' = {claimed}"
                );
                assert_ne!(
                    geometry.chunk_len(claimed, chunk_index).unwrap(),
                    honest_chunk_len,
                    "n' = {claimed} should not have chunk_len {honest_chunk_len}"
                );
            }
        } else {
            let err = result.unwrap_err();
            if claimed < first_full_segment {
                assert_eq!(
                    err,
                    VerifyError::InvalidSegmentLength {
                        expected: claimed - chunk_size,
                        actual: segment_size,
                    },
                    "n' = {claimed}"
                );
            } else {
                assert_eq!(err, VerifyError::CommitmentMismatch, "n' = {claimed}");
            }
        }
    }

    // The same bytes are not a chunk of `n' = 64`. `chunk_len` is 32.
    let chunk_proof = proof_ending_with_size_witness(
        &geometry,
        tree.chunk_proof(chunk_index).unwrap(),
        claimed_full,
    );
    assert_eq!(
        verify_chunk(
            &geometry,
            &rederived_full,
            claimed_full,
            chunk_index,
            chunk,
            &chunk_proof,
        )
        .unwrap_err(),
        VerifyError::InvalidChunkLength {
            expected: chunk_size,
            actual: honest_chunk_len,
        }
    );
}

/// Replace the proof's last hash with `size_witness(G, data_size)` (spec §6).
fn proof_ending_with_size_witness(
    geometry: &Geometry,
    mut proof: Vec<u8>,
    data_size: u64,
) -> Vec<u8> {
    let witness = hash::size_witness(geometry, data_size);
    let witness_start = proof.len() - HASH_SIZE;
    proof[witness_start..].copy_from_slice(witness.as_bytes());
    proof
}

fn testing_chunk_bytes() -> usize {
    usize::try_from(Geometry::TESTING.chunk_size()).unwrap()
}

fn testing_segment_bytes() -> usize {
    usize::try_from(Geometry::TESTING.segment_size()).unwrap()
}
