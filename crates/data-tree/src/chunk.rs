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
pub fn chunk_root(geom: &Geometry, bytes: &[u8]) -> Result<NodeHash, ChunkLengthError> {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if len == 0 {
        return Err(ChunkLengthError::Empty);
    }
    if len > geom.chunk_size() {
        return Err(ChunkLengthError::TooLong {
            len,
            chunk_size: geom.chunk_size(),
        });
    }
    Ok(subtree(geom, bytes, 0, geom.height()))
}

/// `h` in-chunk siblings for segment `j`, level 0 first (spec §4, §5).
///
/// The caller guarantees `j < segments_per_chunk` and that `bytes` is a legal
/// chunk. An out-of-range `j` still returns hashes; it does not panic.
pub(crate) fn segment_siblings(geom: &Geometry, bytes: &[u8], j: u32) -> Vec<NodeHash> {
    let height = geom.height();
    let mut siblings = Vec::with_capacity(usize::try_from(height).unwrap_or(0));
    let mut index = j;
    for level in 0..height {
        // `j < segments_per_chunk <= 256` and `level < height <= 8`, so this fits.
        let start_leaf = (index ^ 1).checked_shl(level).unwrap_or(u32::MAX);
        siblings.push(subtree(geom, bytes, start_leaf, level));
        index >>= 1;
    }
    siblings
}

/// Root of the aligned subtree of height `height` whose first leaf is `start_leaf`.
fn subtree(geom: &Geometry, bytes: &[u8], start_leaf: u32, height: u32) -> NodeHash {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let Some(range_start) = (start_leaf as u64).checked_mul(geom.segment_size()) else {
        return empty(height.min(8));
    };
    if range_start >= len {
        return empty(height.min(8));
    }
    if height == 0 {
        let end = range_start.saturating_add(geom.segment_size()).min(len);
        // `range_start < len` and `len` came from `bytes.len()`, so both ends fit.
        // Clamp anyway: a verifier must not panic on a slice (spec §10).
        let start = usize::try_from(range_start).unwrap_or(bytes.len());
        let end = usize::try_from(end).unwrap_or(bytes.len());
        let start = start.min(bytes.len());
        let end = end.min(bytes.len()).max(start);
        return leaf(&bytes[start..end]);
    }
    // `height` is at most 8, so the shift and the leaf index fit.
    let half = 1_u32 << (height - 1);
    let left = subtree(geom, bytes, start_leaf, height - 1);
    let right_start = start_leaf.saturating_add(half);
    let right = subtree(geom, bytes, right_start, height - 1);
    node(&left, &right)
}
