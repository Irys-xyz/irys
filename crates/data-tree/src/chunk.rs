//! One chunk is a perfect subtree of `segments_per_chunk` leaves (spec §4).
//!
//! A segment hashes at its exact length. A segment past the bytes is `empty(0)`.
//! A fully empty right subtree is `empty(k)` from the fixed table, not a walk
//! over empty leaves (spec §9).

use crate::error::ChunkLengthError;
use crate::geometry::Geometry;
use crate::hash::{empty, leaf, node};
use crate::types::NodeHash;

/// Chunk root of `bytes`. The length must be in `1..=chunk_size` (spec §4).
pub fn chunk_root(geometry: &Geometry, bytes: &[u8]) -> Result<NodeHash, ChunkLengthError> {
    let chunk_length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
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
/// The caller guarantees `j < segments_per_chunk` and that `bytes` is a legal
/// chunk. An out-of-range `j` still returns hashes; it does not panic.
///
/// `j` is `segment_index`.
pub(crate) fn segment_siblings(
    geometry: &Geometry,
    bytes: &[u8],
    segment_index: u32,
) -> Vec<NodeHash> {
    let subtree_height = geometry.height();
    let mut siblings = Vec::with_capacity(usize::try_from(subtree_height).unwrap_or(0));
    let mut segment_index = segment_index;
    for level in 0..subtree_height {
        // `j < segments_per_chunk <= 256` and `level < height <= 8`, so this fits.
        let start_leaf = (segment_index ^ 1).checked_shl(level).unwrap_or(u32::MAX);
        siblings.push(subtree(geometry, bytes, start_leaf, level));
        segment_index >>= 1;
    }
    siblings
}

/// Root of the aligned subtree of height `subtree_height` whose first leaf is `start_leaf`.
fn subtree(geometry: &Geometry, bytes: &[u8], start_leaf: u32, subtree_height: u32) -> NodeHash {
    let chunk_length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let Some(range_start) = (start_leaf as u64).checked_mul(geometry.segment_size()) else {
        return empty(subtree_height.min(8));
    };
    if range_start >= chunk_length {
        return empty(subtree_height.min(8));
    }
    if subtree_height == 0 {
        let end = range_start
            .saturating_add(geometry.segment_size())
            .min(chunk_length);
        // `range_start < len` and `len` came from `bytes.len()`, so both ends fit.
        // Clamp anyway: a verifier must not panic on a slice (spec §10).
        let start = usize::try_from(range_start).unwrap_or(bytes.len());
        let end = usize::try_from(end).unwrap_or(bytes.len());
        let start = start.min(bytes.len());
        let end = end.min(bytes.len()).max(start);
        return leaf(&bytes[start..end]);
    }
    // `height` is at most 8, so the shift and the leaf index fit.
    let left_subtree_size = 1_u32 << (subtree_height - 1);
    let left = subtree(geometry, bytes, start_leaf, subtree_height - 1);
    let right_start = start_leaf.saturating_add(left_subtree_size);
    let right = subtree(geometry, bytes, right_start, subtree_height - 1);
    node(&left, &right)
}
