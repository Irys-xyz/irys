//! Size proofs (spec §5, §9).
//!
//! A verifier computes `rightmost_chunk(n')` and `rightmost_segment(n')` from
//! the size `n'` it checks. A proof at those indexes that opens
//! `node(data_tree_root, size_witness(G, n'))` fixes `n'`: for the honest root
//! of `n` bytes, only `n' = n` passes.
//!
//! The candidates for each claimed `n' ≠ n` are every honest chunk proof and
//! every honest segment proof of the `n`-byte tree, shown at the indexes
//! computed from `n'` with `size_witness(G, n')` as the last hash, and the
//! honest rightmost proofs of the `n'`-byte upload of the same pattern. A
//! segment candidate carries its honest bytes, or the bytes of the `n'`-byte
//! upload at the claimed segment, so a padding slot can stand as a claimed data
//! segment. Every candidate must fail.
//!
//! A sweep with no candidate that reaches the commitment check would pass
//! with no binding at all. Each sweep counts the candidates that reach it, and
//! the moved-index candidates whose left/right steps climb the honest chunk
//! leaf to the honest root: only the index in the leaf stops those (spec §6).

use rstest::rstest;

use super::{empty_at_height, pattern};
use crate::DataTree;
use crate::error::VerifyError;
use crate::geometry::Geometry;
use crate::hash::{data_commitment, size_witness};
use crate::shape::climb;
use crate::types::{DataCommitment, HASH_SIZE, NodeHash};
use crate::verify::{verify_chunk, verify_segment};

/// spec §10: every `data_size` from 1 through this size in test geometry.
const TESTING_LAST_DATA_SIZE: u64 = 199;

/// spec §10: every claimed `n'` from 1 through this size in test geometry.
const TESTING_LAST_CLAIMED_DATA_SIZE: u64 = 224;

/// Chunks in the largest upload of the other geometries. Six chunks give a
/// chunk level of three levels and a carried node (`C` = 3, 5, 6).
const OTHER_GEOMETRY_LAST_CHUNK_COUNT: u64 = 6;

/// Claimed sizes go one chunk past the largest upload.
const OTHER_GEOMETRY_LAST_CLAIMED_CHUNK_COUNT: u64 = OTHER_GEOMETRY_LAST_CHUNK_COUNT + 1;

/// Test geometry. Every `C = 1` tail (one byte, a segment boundary, mid
/// segment, a full chunk) and `C` up to 7.
#[test]
fn rightmost_proofs_verify_only_for_the_true_size_in_test_geometry() {
    let geometry = Geometry::TESTING;
    let tally = sweep(
        &geometry,
        TESTING_LAST_DATA_SIZE,
        TESTING_LAST_CLAIMED_DATA_SIZE,
    );
    tally.assert_not_vacuous();
}

/// One segment per chunk (`h = 0`), two segments, and eight segments of 3 bytes.
#[rstest]
#[case::one_segment_per_chunk(8, 1)]
#[case::two_segments_per_chunk(16, 2)]
#[case::eight_segments_per_chunk(24, 8)]
fn rightmost_proofs_verify_only_for_the_true_size(
    #[case] chunk_size: u64,
    #[case] segments_per_chunk: u32,
) {
    let geometry = Geometry::new(
        chunk_size,
        segments_per_chunk,
        Geometry::TESTING.max_data_tx_chunks(),
    )
    .unwrap();
    let tally = sweep(
        &geometry,
        OTHER_GEOMETRY_LAST_CHUNK_COUNT * chunk_size,
        OTHER_GEOMETRY_LAST_CLAIMED_CHUNK_COUNT * chunk_size,
    );
    tally.assert_not_vacuous();
}

/// Candidates that reached the commitment check, by kind.
#[derive(Default)]
struct Tally {
    /// Chunk candidates with the claimed byte length and proof length.
    chunk_reached_commitment: usize,
    /// Segment candidates that passed every check before the commitment.
    segment_reached_commitment: usize,
    /// Chunk candidates at another index whose steps climb the honest chunk
    /// leaf to the honest root.
    chunk_steps_match_at_moved_index: usize,
    /// The same for segment candidates at their honest segment index.
    segment_steps_match_at_moved_index: usize,
}

impl Tally {
    fn assert_not_vacuous(&self) {
        assert!(self.chunk_reached_commitment > 0, "no chunk candidate");
        assert!(self.segment_reached_commitment > 0, "no segment candidate");
        assert!(
            self.chunk_steps_match_at_moved_index > 0,
            "no chunk candidate that only the index stops"
        );
        assert!(
            self.segment_steps_match_at_moved_index > 0,
            "no segment candidate that only the index stops"
        );
    }
}

/// One honest proof: its indexes, its bytes, and its siblings without the witness.
struct HonestProof {
    chunk_index: u32,
    segment_index: u32,
    bytes: Vec<u8>,
    siblings: Vec<u8>,
}

/// The rightmost chunk and the rightmost data segment of one upload, with proofs.
struct RightmostProofs {
    chunk: Vec<u8>,
    chunk_proof: Vec<u8>,
    segment: Vec<u8>,
    segment_proof: Vec<u8>,
}

fn sweep(geometry: &Geometry, last_data_size: u64, last_claimed_data_size: u64) -> Tally {
    let rightmost: Vec<RightmostProofs> = (1..=last_claimed_data_size)
        .map(|data_size| rightmost_proofs(geometry, data_size))
        .collect();
    let mut tally = Tally::default();
    for data_size in 1..=last_data_size {
        let data = pattern(usize::try_from(data_size).unwrap());
        let tree = DataTree::from_bytes(*geometry, &data).unwrap();
        let honest = &rightmost[index_of(data_size)];
        let (chunk_index, segment_index) = rightmost_segment(geometry, data_size);
        verify_chunk(
            geometry,
            &tree.data_commitment(),
            data_size,
            chunk_index,
            &honest.chunk,
            &honest.chunk_proof,
        )
        .unwrap_or_else(|err| panic!("n {data_size}: honest rightmost chunk: {err}"));
        verify_segment(
            geometry,
            &tree.data_commitment(),
            data_size,
            chunk_index,
            segment_index,
            &honest.segment,
            &honest.segment_proof,
        )
        .unwrap_or_else(|err| panic!("n {data_size}: honest rightmost segment: {err}"));

        let chunk_proofs = honest_chunk_proofs(geometry, &data, &tree);
        let segment_proofs = honest_segment_proofs(geometry, &data, &tree);
        for claimed_data_size in 1..=last_claimed_data_size {
            if claimed_data_size == data_size {
                continue;
            }
            let claim = Claim::new(geometry, &tree, claimed_data_size);
            let claimed_upload = &rightmost[index_of(claimed_data_size)];
            assert!(
                claim
                    .verify_chunk(&claimed_upload.chunk, &claimed_upload.chunk_proof)
                    .is_err(),
                "{}: the claimed upload's chunk proof passed",
                claim.label()
            );
            assert!(
                claim
                    .verify_segment(&claimed_upload.segment, &claimed_upload.segment_proof)
                    .is_err(),
                "{}: the claimed upload's segment proof passed",
                claim.label()
            );
            for candidate in &chunk_proofs {
                claim.reject_honest_chunk(&tree, candidate, &mut tally);
            }
            for candidate in &segment_proofs {
                claim.reject_honest_segment(&tree, candidate, &claimed_upload.segment, &mut tally);
            }
        }
    }
    tally
}

/// The verifier's inputs for a claimed size, all computed from that size (spec §5).
struct Claim<'a> {
    geometry: &'a Geometry,
    data_size: u64,
    chunk_count: u32,
    chunk_index: u32,
    segment_index: u32,
    commitment: DataCommitment,
}

impl<'a> Claim<'a> {
    fn new(geometry: &'a Geometry, tree: &DataTree, data_size: u64) -> Self {
        let (chunk_index, segment_index) = rightmost_segment(geometry, data_size);
        Self {
            geometry,
            data_size,
            chunk_count: geometry.chunk_count(data_size).unwrap(),
            chunk_index,
            segment_index,
            // `node(data_tree_root(n), size_witness(G, n'))` (spec §9).
            commitment: data_commitment(geometry, &tree.data_tree_root(), data_size),
        }
    }

    fn verify_chunk(&self, bytes: &[u8], proof: &[u8]) -> Result<(), VerifyError> {
        verify_chunk(
            self.geometry,
            &self.commitment,
            self.data_size,
            self.chunk_index,
            bytes,
            proof,
        )
    }

    fn verify_segment(&self, bytes: &[u8], proof: &[u8]) -> Result<(), VerifyError> {
        verify_segment(
            self.geometry,
            &self.commitment,
            self.data_size,
            self.chunk_index,
            self.segment_index,
            bytes,
            proof,
        )
    }

    fn reject_chunk(&self, bytes: &[u8], proof: &[u8]) -> Result<(), VerifyError> {
        let result = self.verify_chunk(bytes, proof);
        assert!(result.is_err(), "{}: chunk size proof passed", self.label());
        result
    }

    fn reject_segment(&self, bytes: &[u8], proof: &[u8]) -> Result<(), VerifyError> {
        let result = self.verify_segment(bytes, proof);
        assert!(
            result.is_err(),
            "{}: segment size proof passed",
            self.label()
        );
        result
    }

    fn with_witness(&self, siblings: &[u8]) -> Vec<u8> {
        let mut proof = siblings.to_vec();
        proof.extend_from_slice(size_witness(self.geometry, self.data_size).as_bytes());
        proof
    }

    fn reject_honest_chunk(&self, tree: &DataTree, candidate: &HonestProof, tally: &mut Tally) {
        let result = self.reject_chunk(&candidate.bytes, &self.with_witness(&candidate.siblings));
        if result != Err(VerifyError::CommitmentMismatch) {
            return;
        }
        tally.chunk_reached_commitment += 1;
        if candidate.chunk_index != self.chunk_index
            && self.steps_climb_the_honest_leaf(tree, candidate.chunk_index, &candidate.siblings)
        {
            tally.chunk_steps_match_at_moved_index += 1;
        }
    }

    fn reject_honest_segment(
        &self,
        tree: &DataTree,
        candidate: &HonestProof,
        claimed_bytes: &[u8],
        tally: &mut Tally,
    ) {
        let proof = self.with_witness(&candidate.siblings);
        for bytes in [candidate.bytes.as_slice(), claimed_bytes] {
            if self.reject_segment(bytes, &proof) != Err(VerifyError::CommitmentMismatch) {
                continue;
            }
            tally.segment_reached_commitment += 1;
            // Honest bytes at the honest segment index rebuild the honest chunk
            // root. The in-chunk pin must hold for the claim (spec §5).
            let in_chunk_length = self.in_chunk_sibling_bytes();
            if bytes == candidate.bytes.as_slice()
                && candidate.segment_index == self.segment_index
                && candidate.chunk_index != self.chunk_index
                && self.pins_hold(&candidate.siblings[..in_chunk_length])
                && self.steps_climb_the_honest_leaf(
                    tree,
                    candidate.chunk_index,
                    &candidate.siblings[in_chunk_length..],
                )
            {
                tally.segment_steps_match_at_moved_index += 1;
            }
        }
    }

    /// The claimed `(c', C')` steps climb the honest chunk leaf of index `c`
    /// to the honest root.
    fn steps_climb_the_honest_leaf(
        &self,
        tree: &DataTree,
        honest_chunk_index: u32,
        chunk_siblings: &[u8],
    ) -> bool {
        let honest_leaf = tree.chunk_leaf(honest_chunk_index).unwrap();
        let honest_root = NodeHash::from(*tree.data_tree_root().as_bytes());
        climb(
            honest_leaf,
            self.chunk_index,
            self.chunk_count,
            chunk_siblings,
        ) == Some(honest_root)
    }

    /// Every in-chunk sibling at or past the claimed data-segment count is `empty(ℓ)` (spec §5).
    fn pins_hold(&self, in_chunk_siblings: &[u8]) -> bool {
        let data_segment_count = self.segment_index + 1;
        in_chunk_siblings
            .chunks_exact(HASH_SIZE)
            .enumerate()
            .all(|(level, sibling)| {
                let level = u32::try_from(level).unwrap();
                let start = ((self.segment_index >> level) ^ 1) << level;
                start < data_segment_count || sibling == empty_at_height(level).as_bytes()
            })
    }

    fn in_chunk_sibling_bytes(&self) -> usize {
        usize::try_from(self.geometry.height()).unwrap() * HASH_SIZE
    }

    fn label(&self) -> String {
        format!(
            "chunk_size {} segments_per_chunk {} n' {}",
            self.geometry.chunk_size(),
            self.geometry.segments_per_chunk(),
            self.data_size
        )
    }
}

/// `rightmost_segment(n) = (C − 1, s_last − 1)` (spec §5).
///
/// `s_last = ceil(chunk_len(n, C − 1) / segment_size)`.
fn rightmost_segment(geometry: &Geometry, data_size: u64) -> (u32, u32) {
    let chunk_index = geometry.chunk_count(data_size).unwrap() - 1;
    let chunk_length = geometry.chunk_len(data_size, chunk_index).unwrap();
    let last_data_segment_count = chunk_length.div_ceil(geometry.segment_size());
    (
        chunk_index,
        u32::try_from(last_data_segment_count - 1).unwrap(),
    )
}

fn rightmost_proofs(geometry: &Geometry, data_size: u64) -> RightmostProofs {
    let data = pattern(usize::try_from(data_size).unwrap());
    let tree = DataTree::from_bytes(*geometry, &data).unwrap();
    let (chunk_index, segment_index) = rightmost_segment(geometry, data_size);
    let chunk = chunk_bytes(geometry, &data, chunk_index);
    let segment = segment_bytes(geometry, &chunk, segment_index);
    let segment_proof = tree
        .segment_proof(chunk_index, segment_index, &chunk)
        .unwrap();
    RightmostProofs {
        chunk_proof: tree.chunk_proof(chunk_index).unwrap(),
        segment,
        segment_proof,
        chunk,
    }
}

fn honest_chunk_proofs(geometry: &Geometry, data: &[u8], tree: &DataTree) -> Vec<HonestProof> {
    (0..tree.chunk_count())
        .map(|chunk_index| HonestProof {
            chunk_index,
            segment_index: 0,
            bytes: chunk_bytes(geometry, data, chunk_index),
            siblings: without_witness(tree.chunk_proof(chunk_index).unwrap()),
        })
        .collect()
}

fn honest_segment_proofs(geometry: &Geometry, data: &[u8], tree: &DataTree) -> Vec<HonestProof> {
    let mut proofs = Vec::new();
    for chunk_index in 0..tree.chunk_count() {
        let chunk = chunk_bytes(geometry, data, chunk_index);
        for segment_index in 0..geometry.segments_per_chunk() {
            let proof = tree
                .segment_proof(chunk_index, segment_index, &chunk)
                .unwrap();
            proofs.push(HonestProof {
                chunk_index,
                segment_index,
                bytes: segment_bytes(geometry, &chunk, segment_index),
                siblings: without_witness(proof),
            });
        }
    }
    proofs
}

fn without_witness(mut proof: Vec<u8>) -> Vec<u8> {
    proof.truncate(proof.len() - HASH_SIZE);
    proof
}

fn chunk_bytes(geometry: &Geometry, data: &[u8], chunk_index: u32) -> Vec<u8> {
    let chunk_size = usize::try_from(geometry.chunk_size()).unwrap();
    let start = usize::try_from(chunk_index).unwrap() * chunk_size;
    let end = (start + chunk_size).min(data.len());
    data[start..end].to_vec()
}

/// Segment `j` of a chunk at its exact length. Empty past the data (spec §2).
fn segment_bytes(geometry: &Geometry, chunk: &[u8], segment_index: u32) -> Vec<u8> {
    let segment_size = usize::try_from(geometry.segment_size()).unwrap();
    let start = (usize::try_from(segment_index).unwrap() * segment_size).min(chunk.len());
    let end = (start + segment_size).min(chunk.len());
    chunk[start..end].to_vec()
}

/// Position of size `n` in a list that starts at size 1.
fn index_of(data_size: u64) -> usize {
    usize::try_from(data_size - 1).unwrap()
}
