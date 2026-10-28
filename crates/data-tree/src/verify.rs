//! Proof checks (spec §5).
//!
//! The order is fixed: range, byte length, proof length, the size witness, then
//! the climb. The climb starts at the chunk leaf `chunk_leaf(c, R_c)`. A chunk
//! proof is `32 × (depth(c, C) + 1)` bytes. A segment proof is
//! `32 × (h + depth(c, C) + 1)` bytes (spec §5). A length that is not a
//! multiple of 32 fails that check. Length checks run before any hashing
//! (spec §9). The empty-sibling pin runs in `verify_segment`'s in-chunk loop,
//! before `climb`: a sibling subtree that lies entirely past the data must be
//! `empty(ℓ)` (spec §5, §9).

use crate::chunk::chunk_root;
use crate::error::VerifyError;
use crate::geometry::{Geometry, SubtreeHeight};
use crate::hash::{self, chunk_leaf, empty, leaf, node, size_witness};
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
    let hash_count =
        depth(chunk_index, chunk_count).expect("chunk_len requires chunk_index < chunk_count") + 1;
    let expected_proof = proof_byte_length(hash_count);
    if proof.len() != expected_proof {
        return Err(VerifyError::InvalidProofLength {
            expected: expected_proof,
            actual: proof.len(),
        });
    }
    // Check the witness before the climb. A false `data_size` must not pass on
    // the true witness (spec §5, §11 Q10).
    let siblings = require_size_witness(geometry, proof, data_size)?;
    // The length check enforces `1..=chunk_size`, which is `chunk_root`'s precondition.
    let root = chunk_root(geometry, bytes).expect("chunk length is in 1..=chunk_size");
    // The climb starts at `chunk_leaf(c, R_c)`: the index is hashed into the
    // leaf and also picks the left/right steps (spec §5).
    let leaf_of_chunk = chunk_leaf(chunk_index, &root);
    let climbed = climb(leaf_of_chunk, chunk_index, chunk_count, siblings)
        .ok_or(VerifyError::CommitmentMismatch)?;
    commitment_matches(geometry, commitment, climbed, data_size)
}

/// Check one segment against an anchored commitment (spec §5, §6).
///
/// Several segments of one chunk are checked one by one: each carries its own
/// `h` in-chunk siblings and they share one chunk proof (spec §5, §11 Q2).
/// A segment's proof is those siblings concatenated with the chunk proof. The
/// chunk proof ends with the size witness, so the concatenation is the whole
/// proof in every case, including one chunk.
///
/// An in-chunk sibling whose subtree lies entirely past the data must be
/// `empty(ℓ)`. The first segment of that subtree follows from `(n, c, j)`
/// before the climb (spec §5, §9). The pin does not open a sibling that is
/// only partly past the data. A bad empty leaf inside it does not fail every
/// data segment.
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
    let hash_count = geometry.height()
        + depth(chunk_index, chunk_count).expect("segment_len requires chunk_index < chunk_count")
        + 1;
    let expected_proof = proof_byte_length(hash_count);
    if proof.len() != expected_proof {
        return Err(VerifyError::InvalidProofLength {
            expected: expected_proof,
            actual: proof.len(),
        });
    }
    // Check the witness before the climb (spec §5, §11 Q10).
    let siblings = require_size_witness(geometry, proof, data_size)?;
    let subtree_height = geometry.height();
    let in_chunk_bytes = usize::try_from(subtree_height).expect("height fits usize") * HASH_SIZE;
    // The length check guarantees the siblings cover the `h` in-chunk hashes.
    let (in_chunk, chunk_siblings) = siblings
        .split_at_checked(in_chunk_bytes)
        .expect("proof covers the in-chunk siblings");
    // `s = ceil(chunk_len(n, c) / segment_size)`. A segment at or past `s` is
    // empty. `(n, c)` fixes this before the climb (spec §5).
    let chunk_length = geometry
        .chunk_len(data_size, chunk_index)
        .expect("segment_len already accepted this chunk");
    let data_segments = u32::try_from(chunk_length.div_ceil(geometry.segment_size()))
        .expect("ceil(chunk_len / segment_size) is at most segments_per_chunk");
    let mut root = leaf(bytes);
    // `node_index` starts as `j` and is `j >> ℓ` at level `ℓ` (spec §5).
    let mut node_index = segment_index;
    for (level, sibling) in in_chunk.chunks_exact(HASH_SIZE).enumerate() {
        let sibling_bytes = <[u8; HASH_SIZE]>::try_from(sibling)
            .expect("chunks_exact yields HASH_SIZE-byte slices");
        let sibling = NodeHash::from(sibling_bytes);
        // `level` is `ℓ`. `node_index` is `j >> ℓ`. The sibling subtree
        // starts at `((j >> ℓ) ^ 1) << ℓ` (spec §5). `ℓ < h`, and `h` is at
        // most `MAX_SUBTREE_HEIGHT`, so the shift fits in `u32`.
        let level = u32::try_from(level).expect("height fits u32");
        debug_assert!(level < u32::BITS);
        let sibling_start = (node_index ^ 1) << level;
        // Entirely past the data: the sibling must be `empty(ℓ)`. Any other hash
        // fails. A sibling that still holds a data segment is not opened
        // (spec §5, §9).
        if sibling_start >= data_segments
            && sibling != empty(SubtreeHeight::new(level).expect("ℓ < h <= MAX_SUBTREE_HEIGHT"))
        {
            return Err(VerifyError::CommitmentMismatch);
        }
        // Bit `ℓ` of `j` picks the side (spec §5). Even: this segment is the left child.
        root = if node_index & 1 == 0 {
            node(&root, &sibling)
        } else {
            node(&sibling, &root)
        };
        node_index >>= 1;
    }
    // `root` is now the chunk root. It takes its index before the chunk level (spec §5).
    let leaf_of_chunk = chunk_leaf(chunk_index, &root);
    let climbed = climb(leaf_of_chunk, chunk_index, chunk_count, chunk_siblings)
        .ok_or(VerifyError::CommitmentMismatch)?;
    commitment_matches(geometry, commitment, climbed, data_size)
}

/// `node(data_tree_root, size_witness(G, n)) == commitment` (spec §5).
///
/// `n` is `data_size`. The geometry is the one the caller is checking under.
/// Any `u64` `data_size` is accepted: this check does not range-check it.
pub fn verify_size(
    geometry: &Geometry,
    commitment: &DataCommitment,
    root: &DataTreeRoot,
    data_size: u64,
) -> Result<(), VerifyError> {
    if hash::data_commitment(geometry, root, data_size) == *commitment {
        Ok(())
    } else {
        Err(VerifyError::CommitmentMismatch)
    }
}

/// Last hash equals `size_witness(G, data_size)`. Returns the sibling hashes.
///
/// The witness is not a sibling. Climbing it would let a false `data_size`
/// pass when the proof carries the true witness (spec §5, §11 Q10).
fn require_size_witness<'a>(
    geometry: &Geometry,
    proof: &'a [u8],
    data_size: u64,
) -> Result<&'a [u8], VerifyError> {
    let sibling_len = proof.len() - HASH_SIZE;
    let (siblings, witness) = proof
        .split_at_checked(sibling_len)
        .expect("proof length includes the size witness");
    if witness == size_witness(geometry, data_size).as_bytes() {
        Ok(siblings)
    } else {
        Err(VerifyError::SizeWitnessMismatch)
    }
}

fn commitment_matches(
    geometry: &Geometry,
    commitment: &DataCommitment,
    root: NodeHash,
    data_size: u64,
) -> Result<(), VerifyError> {
    let root = DataTreeRoot::from_hash(root);
    if hash::data_commitment(geometry, &root, data_size) == *commitment {
        Ok(())
    } else {
        Err(VerifyError::CommitmentMismatch)
    }
}

fn exact_byte_length(bytes: &[u8], expected: u64) -> Result<(), u64> {
    // A slice length fits `u64` on every supported target.
    let actual = u64::try_from(bytes.len()).expect("byte length fits u64");
    if actual == expected {
        Ok(())
    } else {
        Err(actual)
    }
}

fn proof_byte_length(hash_count: u32) -> usize {
    // `32 × (depth(c, C) + 1)` or `32 × (h + depth(c, C) + 1)` (spec §5).
    usize::try_from(hash_count).expect("hash count fits usize") * HASH_SIZE
}
