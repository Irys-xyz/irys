//! One chunk is a perfect subtree of `segments_per_chunk` leaves (spec §4).
//!
//! A segment hashes at its exact length. A segment past the bytes is `empty(0)`.
//! A fully empty right subtree is `empty(k)` from the fixed table, not a walk
//! over empty leaves (spec §8).

use crate::error::ChunkLengthError;
use crate::geometry::{Geometry, MAX_SUBTREE_HEIGHT};
use crate::hash::{empty, leaf, node};
use crate::types::NodeHash;

/// Chunk root of `bytes`. The length must be in `1..=chunk_size` (spec §4).
pub fn chunk_root(geometry: &Geometry, bytes: &[u8]) -> Result<NodeHash, ChunkLengthError> {
    // A slice length fits `u64` on every supported target.
    let chunk_length = u64::try_from(bytes.len()).expect("byte length fits u64");
    if chunk_length == 0 {
        return Err(ChunkLengthError::Empty);
    }
    if chunk_length > geometry.chunk_size() {
        return Err(ChunkLengthError::TooLong {
            chunk_length,
            chunk_size: geometry.chunk_size(),
        });
    }
    Ok(subtree(geometry, bytes, 0, geometry.height()))
}

/// `h` in-chunk siblings for segment `j`, level 0 first (spec §4, §5).
///
/// The caller guarantees `j < segments_per_chunk` and that `bytes` is a legal chunk.
///
/// `j` is `segment_index`.
pub(crate) fn segment_siblings(
    geometry: &Geometry,
    bytes: &[u8],
    segment_index: u32,
) -> Vec<NodeHash> {
    debug_assert!(segment_index < geometry.segments_per_chunk());
    let subtree_height = geometry.height();
    // `h = log2(segments_per_chunk)` fits `usize`.
    let mut siblings =
        Vec::with_capacity(usize::try_from(subtree_height).expect("height fits usize"));
    // `node_index` starts as `j` and is `j >> ℓ` at level `ℓ` (spec §5).
    let mut node_index = segment_index;
    for level in 0..subtree_height {
        // Sibling of the current node: flip bit 0 (`i ^ 1`), then move that
        // leaf back to this level. `j < segments_per_chunk <= MAX_SEGMENTS_PER_CHUNK`
        // and `level < h <= MAX_SUBTREE_HEIGHT`, so the shift fits in `u32`.
        let start_leaf = (node_index ^ 1) << level;
        siblings.push(subtree(geometry, bytes, start_leaf, level));
        node_index >>= 1;
    }
    siblings
}

/// Root of the aligned subtree of height `subtree_height` whose first leaf is `start_leaf`.
fn subtree(geometry: &Geometry, bytes: &[u8], start_leaf: u32, subtree_height: u32) -> NodeHash {
    // `chunk_root` already rejected an empty or over-long slice, and a slice
    // length fits `u64`. `start_leaf` stays below `segments_per_chunk`, so
    // `start_leaf * segment_size` is strictly below `chunk_size` and fits `u64`.
    let chunk_length = u64::try_from(bytes.len()).expect("byte length fits u64");
    let range_start = (u64::from(start_leaf))
        .checked_mul(geometry.segment_size())
        .expect("segment start fits u64");
    if range_start >= chunk_length {
        // Past the bytes: `empty(k)` (spec §4, §8). `subtree_height` is at most
        // `MAX_SUBTREE_HEIGHT`; `empty` panics above that.
        debug_assert!(subtree_height <= MAX_SUBTREE_HEIGHT);
        return empty(subtree_height);
    }
    if subtree_height == 0 {
        // `min(segment_size, chunk_length - range_start)` (spec §2). The sum fits:
        // it is at most `chunk_size`.
        let segment_end = range_start
            .checked_add(geometry.segment_size())
            .expect("segment end fits u64");
        let end = segment_end.min(chunk_length);
        // `range_start < chunk_length` and `chunk_length` came from `bytes.len()`.
        let start = usize::try_from(range_start).expect("range start fits usize");
        let end = usize::try_from(end).expect("range end fits usize");
        return leaf(&bytes[start..end]);
    }
    // Left half of this perfect subtree. `subtree_height` is at most
    // `MAX_SUBTREE_HEIGHT`, so the shift fits.
    debug_assert!(subtree_height <= MAX_SUBTREE_HEIGHT);
    let left_subtree_size = 1_u32 << (subtree_height - 1);
    let left = subtree(geometry, bytes, start_leaf, subtree_height - 1);
    let right_start = start_leaf
        .checked_add(left_subtree_size)
        .expect("right subtree start fits");
    let right = subtree(geometry, bytes, right_start, subtree_height - 1);
    node(&left, &right)
}
