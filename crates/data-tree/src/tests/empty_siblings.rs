//! An in-chunk sibling whose subtree lies entirely past the data is `empty(ℓ)`
//! (spec §5, §10).
//!
//! The pin does not open a sibling that is only partly past the data. A bad
//! hash in an empty leaf of that mixed sibling leaves the other data segments
//! verifying. The rightmost data segment's proof fails, and `verify_chunk`
//! fails for every byte string of length `chunk_len`. Honest proofs already
//! carry `empty(ℓ)`, so they still verify.

use proptest::prelude::*;
use rstest::rstest;

use super::strategies;
use crate::error::VerifyError;
use crate::geometry::{Geometry, MAX_SUBTREE_HEIGHT};
use crate::shape::climb;
use crate::types::{DataCommitment, DataTreeRoot, HASH_SIZE, NodeHash};
use crate::verify::{verify_chunk, verify_segment};
use crate::{DataTree, chunk_root, data_commitment, empty, leaf, node, size_witness};

/// Byte repeated through the garbage hash in the published attack.
const GARBAGE_BYTE: u8 = 0xAB;

/// Property cases. `PROPTEST_CASES` replaces this (see `strategies`).
const PAST_DATA_SIBLING_CASES: u32 = 64;

/// `segments_per_chunk` at the subtree-height bound (spec §2). `h` is 8.
const MAX_SUBTREE_HEIGHT_SEGMENTS: u32 = 1 << MAX_SUBTREE_HEIGHT;

/// Test geometry, `n = 40`: chunk 1 holds segment 0 only. The bad chunk root is
/// `node(node(leaf(seg0), empty(0)), G)` with `G = 0xAB × 32`. The segment proof
/// is `[empty(0), G, R0, size_witness(40)]`.
#[test]
fn malformed_last_chunk_of_the_published_attack_is_rejected() {
    let geometry = Geometry::TESTING;
    let data_size = 40;
    reject_malformed_right_half(&geometry, data_size, 1);
    let tree = honest_tree(&geometry, data_size);
    let bytes = tree_bytes(data_size);
    let chunk = chunk_bytes(&geometry, &bytes, data_size, 1);
    let segment = &chunk[..];
    let garbage = NodeHash::from([GARBAGE_BYTE; HASH_SIZE]);
    let left = node(&leaf(segment), &empty(0));
    let bad_chunk_root = node(&left, &garbage);
    let chunk_zero = tree.chunk_root(0).unwrap();
    let mut proof = Vec::new();
    proof.extend_from_slice(empty(0).as_bytes());
    proof.extend_from_slice(garbage.as_bytes());
    proof.extend_from_slice(chunk_zero.as_bytes());
    proof.extend_from_slice(size_witness(&geometry, data_size).as_bytes());
    let root = node(&chunk_zero, &bad_chunk_root);
    let commitment = data_commitment(&geometry, &DataTreeRoot::from_hash(root), data_size);
    assert_eq!(
        verify_segment(&geometry, &commitment, data_size, 1, 0, segment, &proof),
        Err(VerifyError::CommitmentMismatch)
    );
}

/// Same attack at other heights: the right half of the last chunk is garbage.
/// SPC 8 and 256 use one-byte segments. Every data segment of that chunk fails.
#[rstest]
#[case::spc_8_one_segment(8, 1)]
#[case::spc_8_three_segments(8, 3)]
#[case::spc_256_one_segment(256, 1)]
#[case::spc_256_three_segments(256, 3)]
fn malformed_last_chunk_rejects_every_data_segment(
    #[case] segments_per_chunk: u32,
    #[case] data_segments: u32,
) {
    let geometry = Geometry::new(u64::from(segments_per_chunk), segments_per_chunk, 2).unwrap();
    let data_size = geometry.chunk_size() + geometry.segment_size() * u64::from(data_segments);
    reject_malformed_right_half(&geometry, data_size, data_segments);
}

/// One chunk, so the proof has no chunk-level sibling. The pin is in-chunk.
#[test]
fn malformed_only_chunk_rejects_its_data_segment() {
    let geometry = Geometry::new(8, 8, 2).unwrap();
    let data_size = 1;
    reject_malformed_right_half(&geometry, data_size, 1);
}

/// Test geometry, `n = 20` (spec §5). Leaf 3 is `G` instead of `empty(0)`.
/// `s = 3`. Segments 0 and 1 verify. Segments 2 and 3 fail.
/// Segment 2 is the rightmost data segment. `verify_chunk` fails.
#[test]
fn mixed_empty_leaf_of_twenty_bytes_lets_segments_0_and_1_verify() {
    assert_mixed_empty_leaf(&Geometry::TESTING, 20, &[true, true, false, false]);
}

/// Eight segments, one byte each, `n = 5` (spec §5). Leaf 7 is `G` instead of
/// `empty(0)`. `s = 5`. Segments 0, 1, 2 and 3 verify. Segment 4, the rightmost
/// data segment, fails. Segments 5, 6 and 7 are past the data and fail.
/// `verify_chunk` fails.
#[test]
fn mixed_empty_leaf_of_five_bytes_lets_segments_0_through_3_verify() {
    let geometry = Geometry::new(8, 8, 2).unwrap();
    assert_mixed_empty_leaf(
        &geometry,
        5,
        &[true, true, true, true, false, false, false, false],
    );
}

/// `empty(ℓ+1)` and a real data node both look like hashes. Neither is `empty(ℓ)`.
#[test]
fn empty_region_sibling_replaced_by_another_hash_is_rejected() {
    let geometry = Geometry::TESTING;
    let data_size = 40;
    let level = 1;
    let segment = last_chunk_segment(&geometry, data_size, 0);
    let left = node(&leaf(&segment), &empty(0));
    reject_substitute(&geometry, data_size, 0, level, empty(level + 1));
    reject_substitute(&geometry, data_size, 0, level, left);

    // Height 8: the top in-chunk sibling is `empty(7)`. `empty(8)` is the next table entry.
    let geometry = Geometry::new(
        u64::from(MAX_SUBTREE_HEIGHT_SEGMENTS),
        MAX_SUBTREE_HEIGHT_SEGMENTS,
        2,
    )
    .unwrap();
    let data_size = geometry.chunk_size() + 1;
    let level = geometry.height() - 1;
    assert_eq!(level, MAX_SUBTREE_HEIGHT - 1);
    reject_substitute(&geometry, data_size, 0, level, empty(level + 1));
}

/// spec §13 sweeps `1..=199`. `n = 200` is the next size. Every height is
/// included, with segments past the data on the partial last chunk.
#[test]
fn honest_segment_proofs_verify_past_the_data_and_at_every_height() {
    for data_size in 1_u64..=200 {
        assert_honest_segments(&Geometry::TESTING, data_size);
    }
    for height in 0..=MAX_SUBTREE_HEIGHT {
        let segments_per_chunk = 1_u32 << height;
        let geometry = Geometry::new(u64::from(segments_per_chunk), segments_per_chunk, 2).unwrap();
        let chunk_size = geometry.chunk_size();
        assert_honest_segments(&geometry, 1);
        assert_honest_segments(&geometry, chunk_size);
        assert_honest_segments(&geometry, chunk_size + 1);
    }
}

proptest! {
    #![proptest_config(strategies::case_config(PAST_DATA_SIBLING_CASES))]

    #[test]
    fn a_replaced_past_data_sibling_is_rejected(
        (geometry, bytes, probe) in upload_with_a_partial_last_chunk()
    ) {
        reject_one_replaced_sibling(&geometry, &bytes, probe);
    }
}

/// Last chunk's right half is `garbage` instead of `empty(h − 1)`.
///
/// The data fits in the left half, so every data segment's proof carries that
/// sibling. The commitment is the climb of the modified proof.
fn reject_malformed_right_half(geometry: &Geometry, data_size: u64, data_segments: u32) {
    assert!(
        geometry.height() >= 1,
        "the right half is an in-chunk sibling"
    );
    assert!(data_segments >= 1);
    assert!(
        data_segments <= geometry.segments_per_chunk() / 2,
        "the data fits in the left half, so the right half is empty(h-1)"
    );
    let bytes = tree_bytes(data_size);
    let tree = DataTree::from_bytes(*geometry, &bytes).unwrap();
    let chunk_index = tree.chunk_count() - 1;
    let chunk = chunk_bytes(geometry, &bytes, data_size, chunk_index);
    let level = geometry.height() - 1;
    let garbage = NodeHash::from([GARBAGE_BYTE; HASH_SIZE]);
    assert_ne!(garbage, empty(level));

    let mut checked = 0_u32;
    let mut commitment = None;
    for segment_index in 0..geometry.segments_per_chunk() {
        let segment_length = geometry
            .segment_len(data_size, chunk_index, segment_index)
            .unwrap();
        if segment_length == 0 {
            continue;
        }
        let segment = segment_bytes(geometry, &bytes, data_size, chunk_index, segment_index);
        let honest = tree
            .segment_proof(chunk_index, segment_index, &chunk)
            .unwrap();
        verify_segment(
            geometry,
            &tree.data_commitment(),
            data_size,
            chunk_index,
            segment_index,
            &segment,
            &honest,
        )
        .unwrap_or_else(|err| {
            panic!("honest segment n={data_size} c={chunk_index} j={segment_index}: {err}")
        });
        assert_eq!(
            sibling_at(&honest, level),
            empty(level),
            "n={data_size} j={segment_index}: the right half is empty({level})"
        );
        let bad = replace_sibling(&honest, level, &garbage);
        let climbed = commitment_from_segment_proof(
            geometry,
            data_size,
            chunk_index,
            segment_index,
            &segment,
            &bad,
        );
        match commitment {
            None => commitment = Some(climbed),
            Some(shared) => assert_eq!(climbed, shared, "one bad right half, one commitment"),
        }
        let err = verify_segment(
            geometry,
            &climbed,
            data_size,
            chunk_index,
            segment_index,
            &segment,
            &bad,
        )
        .unwrap_err();
        assert_eq!(
            err,
            VerifyError::CommitmentMismatch,
            "n={data_size} c={chunk_index} j={segment_index}"
        );
        checked += 1;
    }
    assert_eq!(checked, data_segments);
    let commitment = commitment.expect("at least one data segment");
    let chunk_proof = tree.chunk_proof(chunk_index).unwrap();
    assert_eq!(
        verify_chunk(
            geometry,
            &commitment,
            data_size,
            chunk_index,
            &chunk,
            &chunk_proof
        ),
        Err(VerifyError::CommitmentMismatch)
    );
}

fn reject_substitute(
    geometry: &Geometry,
    data_size: u64,
    segment_index: u32,
    level: u32,
    substitute: NodeHash,
) {
    assert_ne!(substitute, empty(level));
    let bytes = tree_bytes(data_size);
    let tree = DataTree::from_bytes(*geometry, &bytes).unwrap();
    let chunk_index = tree.chunk_count() - 1;
    let chunk = chunk_bytes(geometry, &bytes, data_size, chunk_index);
    let segment = segment_bytes(geometry, &bytes, data_size, chunk_index, segment_index);
    let honest = tree
        .segment_proof(chunk_index, segment_index, &chunk)
        .unwrap();
    assert_eq!(sibling_at(&honest, level), empty(level));
    let bad = replace_sibling(&honest, level, &substitute);
    let commitment = commitment_from_segment_proof(
        geometry,
        data_size,
        chunk_index,
        segment_index,
        &segment,
        &bad,
    );
    assert_eq!(
        verify_segment(
            geometry,
            &commitment,
            data_size,
            chunk_index,
            segment_index,
            &segment,
            &bad,
        ),
        Err(VerifyError::CommitmentMismatch),
        "n={data_size} j={segment_index} level={level}"
    );
}

fn reject_one_replaced_sibling(geometry: &Geometry, bytes: &[u8], probe: u64) {
    let data_size = u64::try_from(bytes.len()).expect("upload fits u64");
    let tree = DataTree::from_bytes(*geometry, bytes)
        .unwrap_or_else(|err| panic!("upload of {data_size} bytes: {err}"));
    let chunk_index = tree.chunk_count() - 1;
    // A short last chunk can still fill every segment: the last segment is then
    // partial, and no sibling subtree is empty. There is nothing to replace.
    let has_empty_segment = (0..geometry.segments_per_chunk()).any(|segment_index| {
        geometry
            .segment_len(data_size, chunk_index, segment_index)
            .unwrap()
            == 0
    });
    if !has_empty_segment {
        return;
    }
    let pinned = pinned_data_segment_siblings(geometry, data_size, chunk_index);
    assert!(
        !pinned.is_empty(),
        "an empty segment leaves a past-data sibling on a data segment (n={data_size}, h={})",
        geometry.height()
    );
    let pick = usize::try_from(probe % u64::try_from(pinned.len()).expect("pin count fits u64"))
        .expect("pick fits usize");
    let (segment_index, level) = pinned[pick];
    let chunk = chunk_bytes(geometry, bytes, data_size, chunk_index);
    let segment = segment_bytes(geometry, bytes, data_size, chunk_index, segment_index);
    let honest = tree
        .segment_proof(chunk_index, segment_index, &chunk)
        .unwrap();
    assert_eq!(
        sibling_at(&honest, level),
        empty(level),
        "builder sibling at level {level} is empty({level})"
    );
    let substitute = hash_other_than(empty(level), level, &segment, probe);
    let bad = replace_sibling(&honest, level, &substitute);
    let commitment = commitment_from_segment_proof(
        geometry,
        data_size,
        chunk_index,
        segment_index,
        &segment,
        &bad,
    );
    let err = verify_segment(
        geometry,
        &commitment,
        data_size,
        chunk_index,
        segment_index,
        &segment,
        &bad,
    )
    .unwrap_err();
    assert_eq!(err, VerifyError::CommitmentMismatch);
}

/// `(segment_index, level)` whose sibling subtree is entirely past the data.
///
/// The start index is `((j >> ℓ) ^ 1) << ℓ` (spec §5). Each leaf of that
/// subtree has `segment_len` 0.
fn pinned_data_segment_siblings(
    geometry: &Geometry,
    data_size: u64,
    chunk_index: u32,
) -> Vec<(u32, u32)> {
    let mut pinned = Vec::new();
    for segment_index in 0..geometry.segments_per_chunk() {
        if geometry
            .segment_len(data_size, chunk_index, segment_index)
            .unwrap()
            == 0
        {
            continue;
        }
        let mut walked_index = segment_index;
        for level in 0..geometry.height() {
            let start = (walked_index ^ 1) << level;
            let width = 1_u32 << level;
            let entirely_past = (0..width).all(|offset| {
                geometry
                    .segment_len(data_size, chunk_index, start + offset)
                    .unwrap()
                    == 0
            });
            if entirely_past {
                pinned.push((segment_index, level));
            }
            walked_index >>= 1;
        }
    }
    pinned
}

/// Climb the segment proof the way spec §5 does, with no empty-sibling pin.
///
/// The result is the commitment a proof of this shape opens. The pin must
/// reject it when a past-data sibling is not `empty(ℓ)`.
fn commitment_from_segment_proof(
    geometry: &Geometry,
    data_size: u64,
    chunk_index: u32,
    segment_index: u32,
    segment: &[u8],
    proof: &[u8],
) -> DataCommitment {
    let chunk_count = geometry.chunk_count(data_size).unwrap();
    let in_chunk_bytes = usize::try_from(geometry.height()).unwrap() * HASH_SIZE;
    let siblings = &proof[..proof.len() - HASH_SIZE];
    let (in_chunk, chunk_siblings) = siblings.split_at(in_chunk_bytes);
    let mut root = leaf(segment);
    let mut walked_index = segment_index;
    for sibling in in_chunk.chunks_exact(HASH_SIZE) {
        let bytes = <[u8; HASH_SIZE]>::try_from(sibling).unwrap();
        let sibling = NodeHash::from(bytes);
        root = if walked_index & 1 == 0 {
            node(&root, &sibling)
        } else {
            node(&sibling, &root)
        };
        walked_index >>= 1;
    }
    let climbed = climb(root, chunk_index, chunk_count, chunk_siblings)
        .expect("the modified proof is one hash per level");
    data_commitment(geometry, &DataTreeRoot::from_hash(climbed), data_size)
}

fn hash_other_than(required: NodeHash, level: u32, segment: &[u8], probe: u64) -> NodeHash {
    let choices = [
        empty(level + 1),
        NodeHash::from([GARBAGE_BYTE; HASH_SIZE]),
        leaf(segment),
    ];
    let start = usize::try_from(probe % u64::try_from(choices.len()).unwrap()).unwrap();
    for step in 0..choices.len() {
        let candidate = choices[(start + step) % choices.len()];
        if candidate != required {
            return candidate;
        }
    }
    unreachable!("three hashes, and empty(ℓ+1) is not empty(ℓ)");
}

fn sibling_at(proof: &[u8], level: u32) -> NodeHash {
    let offset = usize::try_from(level).unwrap() * HASH_SIZE;
    let bytes = <[u8; HASH_SIZE]>::try_from(&proof[offset..offset + HASH_SIZE]).unwrap();
    NodeHash::from(bytes)
}

fn replace_sibling(proof: &[u8], level: u32, sibling: &NodeHash) -> Vec<u8> {
    let mut proof = proof.to_vec();
    let offset = usize::try_from(level).unwrap() * HASH_SIZE;
    proof[offset..offset + HASH_SIZE].copy_from_slice(sibling.as_bytes());
    proof
}

fn assert_honest_segments(geometry: &Geometry, data_size: u64) {
    let bytes = tree_bytes(data_size);
    let tree = DataTree::from_bytes(*geometry, &bytes).unwrap();
    let commitment = tree.data_commitment();
    for chunk_index in 0..tree.chunk_count() {
        let chunk = chunk_bytes(geometry, &bytes, data_size, chunk_index);
        for segment_index in 0..geometry.segments_per_chunk() {
            let segment = segment_bytes(geometry, &bytes, data_size, chunk_index, segment_index);
            let proof = tree
                .segment_proof(chunk_index, segment_index, &chunk)
                .unwrap();
            verify_segment(
                geometry,
                &commitment,
                data_size,
                chunk_index,
                segment_index,
                &segment,
                &proof,
            )
            .unwrap_or_else(|err| {
                panic!(
                    "honest segment h={} n={data_size} c={chunk_index} j={segment_index}: {err}",
                    geometry.height()
                )
            });
        }
    }
}

fn honest_tree(geometry: &Geometry, data_size: u64) -> DataTree {
    DataTree::from_bytes(*geometry, &tree_bytes(data_size)).unwrap()
}

fn tree_bytes(data_size: u64) -> Vec<u8> {
    super::pattern(usize::try_from(data_size).unwrap())
}

fn last_chunk_segment(geometry: &Geometry, data_size: u64, segment_index: u32) -> Vec<u8> {
    let bytes = tree_bytes(data_size);
    let tree = honest_tree(geometry, data_size);
    let chunk_index = tree.chunk_count() - 1;
    segment_bytes(geometry, &bytes, data_size, chunk_index, segment_index)
}

fn chunk_bytes(geometry: &Geometry, data: &[u8], data_size: u64, chunk_index: u32) -> Vec<u8> {
    let start = usize::try_from(u64::from(chunk_index) * geometry.chunk_size()).unwrap();
    let chunk_length =
        usize::try_from(geometry.chunk_len(data_size, chunk_index).unwrap()).unwrap();
    data[start..start + chunk_length].to_vec()
}

/// One chunk. Leaf `segments_per_chunk - 1` is `G` instead of `empty(0)`.
///
/// `segment_verifies[j]` is the spec §5 result for that segment: `true` is
/// `Ok(())`, `false` is `CommitmentMismatch`. The bad leaf shares a subtree
/// with a data segment, so the segments that verify meet it only through a
/// mixed sibling. The rightmost data segment meets it through a sibling that
/// lies entirely past the data.
fn assert_mixed_empty_leaf(geometry: &Geometry, data_size: u64, segment_verifies: &[bool]) {
    let segments_per_chunk = geometry.segments_per_chunk();
    assert_eq!(
        segment_verifies.len(),
        usize::try_from(segments_per_chunk).unwrap()
    );
    assert_eq!(geometry.chunk_count(data_size).unwrap(), 1, "one chunk");
    let chunk_index = 0;
    let chunk_length = geometry.chunk_len(data_size, chunk_index).unwrap();
    let data_segments = u32::try_from(chunk_length.div_ceil(geometry.segment_size()))
        .expect("ceil(chunk_len / segment_size) fits segments_per_chunk");
    let bad_leaf = segments_per_chunk - 1;
    assert_eq!(
        geometry
            .segment_len(data_size, chunk_index, bad_leaf)
            .unwrap(),
        0,
        "the replaced leaf is past the data"
    );
    // The bad leaf starts at or past `chunk_len`, so `chunk_root` is `empty(0)`
    // there for every string of this length (spec §4, §5).
    assert!(u64::from(bad_leaf) * geometry.segment_size() >= chunk_length);

    let data = tree_bytes(data_size);
    let bad_root = subtree_with_bad_leaf(
        geometry,
        &data,
        chunk_length,
        0,
        geometry.height(),
        Some(bad_leaf),
    );
    let canonical =
        subtree_with_bad_leaf(geometry, &data, chunk_length, 0, geometry.height(), None);
    assert_eq!(
        chunk_root(geometry, &data).unwrap(),
        canonical,
        "the oracle matches chunk_root when no leaf is replaced"
    );
    assert_ne!(bad_root, canonical, "G is not empty(0)");
    let commitment = data_commitment(geometry, &DataTreeRoot::from_hash(bad_root), data_size);

    let rightmost_data_segment = (0..segments_per_chunk)
        .rev()
        .find(|segment_index| {
            geometry
                .segment_len(data_size, chunk_index, *segment_index)
                .unwrap()
                > 0
        })
        .expect("the chunk has a data segment");
    let rightmost = usize::try_from(rightmost_data_segment).unwrap();
    assert!(
        !segment_verifies[rightmost],
        "the rightmost data segment fails"
    );
    assert!(
        meets_bad_leaf_through_pinned_sibling(
            geometry,
            data_segments,
            rightmost_data_segment,
            bad_leaf
        ),
        "the rightmost data segment meets the bad leaf through an entirely empty sibling"
    );

    for (segment_index, verifies) in segment_verifies.iter().copied().enumerate() {
        let segment_index = u32::try_from(segment_index).unwrap();
        let segment = segment_bytes(geometry, &data, data_size, chunk_index, segment_index);
        let proof = segment_proof_with_bad_leaf(
            geometry,
            &data,
            data_size,
            chunk_length,
            segment_index,
            bad_leaf,
        );
        let result = verify_segment(
            geometry,
            &commitment,
            data_size,
            chunk_index,
            segment_index,
            &segment,
            &proof,
        );
        if verifies {
            assert_eq!(result, Ok(()), "segment {segment_index} verifies");
            assert!(
                meets_bad_leaf_through_mixed_sibling(
                    geometry,
                    data_segments,
                    segment_index,
                    bad_leaf
                ),
                "segment {segment_index} meets the bad leaf only through a mixed sibling"
            );
        } else {
            assert_eq!(
                result,
                Err(VerifyError::CommitmentMismatch),
                "segment {segment_index} fails"
            );
        }
    }

    let chunk_proof = DataTree::from_bytes(*geometry, &data)
        .unwrap()
        .chunk_proof(chunk_index)
        .unwrap();
    let length = usize::try_from(chunk_length).unwrap();
    for bytes in [data, vec![0x00; length], vec![0xFF; length]] {
        assert_ne!(chunk_root(geometry, &bytes).unwrap(), bad_root);
        assert_eq!(
            verify_chunk(
                geometry,
                &commitment,
                data_size,
                chunk_index,
                &bytes,
                &chunk_proof
            ),
            Err(VerifyError::CommitmentMismatch),
            "verify_chunk fails for a string of length chunk_len"
        );
    }
}

/// `true` when some sibling of `segment_index` contains `bad_leaf` and starts
/// before the first empty segment. The pin does not apply to that sibling.
fn meets_bad_leaf_through_mixed_sibling(
    geometry: &Geometry,
    data_segments: u32,
    segment_index: u32,
    bad_leaf: u32,
) -> bool {
    let mut walked_index = segment_index;
    for level in 0..geometry.height() {
        let start = (walked_index ^ 1) << level;
        if sibling_covers(start, level, bad_leaf) && start < data_segments {
            return true;
        }
        walked_index >>= 1;
    }
    false
}

/// `true` when some sibling of `segment_index` contains `bad_leaf` and lies
/// entirely past the data. The pin requires that sibling to be `empty(ℓ)`.
fn meets_bad_leaf_through_pinned_sibling(
    geometry: &Geometry,
    data_segments: u32,
    segment_index: u32,
    bad_leaf: u32,
) -> bool {
    let mut walked_index = segment_index;
    for level in 0..geometry.height() {
        let start = (walked_index ^ 1) << level;
        if sibling_covers(start, level, bad_leaf) && start >= data_segments {
            return true;
        }
        walked_index >>= 1;
    }
    false
}

fn sibling_covers(start: u32, level: u32, leaf: u32) -> bool {
    let width = 1_u32 << level;
    let end = start.checked_add(width).expect("sibling end fits");
    leaf >= start && leaf < end
}

/// Spec §4 subtree. `bad_leaf` is `G` instead of `empty(0)`. A subtree that
/// does not contain that leaf, and that starts at or past the bytes, is
/// `empty(k)`.
fn subtree_with_bad_leaf(
    geometry: &Geometry,
    data: &[u8],
    chunk_length: u64,
    start_leaf: u32,
    subtree_height: u32,
    bad_leaf: Option<u32>,
) -> NodeHash {
    let width = 1_u32 << subtree_height;
    let end_leaf = start_leaf.checked_add(width).expect("subtree end fits");
    let contains_bad = bad_leaf.is_some_and(|leaf| leaf >= start_leaf && leaf < end_leaf);
    let range_start = u64::from(start_leaf) * geometry.segment_size();
    if !contains_bad && range_start >= chunk_length {
        return empty(subtree_height);
    }
    if subtree_height == 0 {
        if contains_bad {
            return NodeHash::from([GARBAGE_BYTE; HASH_SIZE]);
        }
        let end = (range_start + geometry.segment_size()).min(chunk_length);
        let start = usize::try_from(range_start).unwrap();
        let end = usize::try_from(end).unwrap();
        return leaf(&data[start..end]);
    }
    let left_width = width / 2;
    let left = subtree_with_bad_leaf(
        geometry,
        data,
        chunk_length,
        start_leaf,
        subtree_height - 1,
        bad_leaf,
    );
    let right = subtree_with_bad_leaf(
        geometry,
        data,
        chunk_length,
        start_leaf + left_width,
        subtree_height - 1,
        bad_leaf,
    );
    node(&left, &right)
}

fn segment_proof_with_bad_leaf(
    geometry: &Geometry,
    data: &[u8],
    data_size: u64,
    chunk_length: u64,
    segment_index: u32,
    bad_leaf: u32,
) -> Vec<u8> {
    let mut proof = Vec::new();
    let mut walked_index = segment_index;
    for level in 0..geometry.height() {
        let start = (walked_index ^ 1) << level;
        let sibling =
            subtree_with_bad_leaf(geometry, data, chunk_length, start, level, Some(bad_leaf));
        proof.extend_from_slice(sibling.as_bytes());
        walked_index >>= 1;
    }
    proof.extend_from_slice(size_witness(geometry, data_size).as_bytes());
    proof
}

fn segment_bytes(
    geometry: &Geometry,
    data: &[u8],
    data_size: u64,
    chunk_index: u32,
    segment_index: u32,
) -> Vec<u8> {
    let segment_length = geometry
        .segment_len(data_size, chunk_index, segment_index)
        .unwrap();
    if segment_length == 0 {
        return Vec::new();
    }
    let start = u64::from(chunk_index) * geometry.chunk_size()
        + u64::from(segment_index) * geometry.segment_size();
    let start = usize::try_from(start).unwrap();
    let segment_length = usize::try_from(segment_length).unwrap();
    data[start..start + segment_length].to_vec()
}

fn upload_with_a_partial_last_chunk() -> impl Strategy<Value = (Geometry, Vec<u8>, u64)> {
    (1_u32..=MAX_SUBTREE_HEIGHT)
        .prop_flat_map(|height| {
            let segments_per_chunk = 1_u32 << height;
            (1_u64..=4).prop_map(move |segment_size| {
                Geometry::new(
                    segment_size * u64::from(segments_per_chunk),
                    segments_per_chunk,
                    2,
                )
                .expect("one- to four-byte segments, at most 256 of them, fit")
            })
        })
        .prop_flat_map(|geometry| {
            let chunk_size = geometry.chunk_size();
            // The last segment starts here. A longer last chunk fills every segment.
            let empty_segment_bound =
                u64::from(geometry.segments_per_chunk() - 1) * geometry.segment_size();
            // One or two chunks. The last one stops before its last segment.
            let max_data_size = chunk_size * 2 - 1;
            (1_u64..=max_data_size, any::<u64>()).prop_filter_map(
                "the last chunk has no empty segment",
                move |(data_size, probe)| {
                    let remainder = data_size % chunk_size;
                    let last_length = if remainder == 0 {
                        chunk_size
                    } else {
                        remainder
                    };
                    if last_length > empty_segment_bound {
                        None
                    } else {
                        Some((geometry, data_size, probe))
                    }
                },
            )
        })
        .prop_flat_map(|(geometry, data_size, probe)| {
            let length = usize::try_from(data_size).expect("two small chunks fit usize");
            prop::collection::vec(any::<u8>(), length)
                .prop_map(move |bytes| (geometry, bytes, probe))
        })
}
