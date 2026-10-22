//! Chunk-tree shape: `depth` and the RFC 9162 inclusion walk (spec §4, §5).

use crate::hash::node;
use crate::types::{HASH_SIZE, NodeHash};

/// Largest power of two strictly below `chunk_count` (`k` in spec §4).
///
/// `chunk_count >= 2`. `floor_log2(chunk_count - 1)` is
/// `u32::BITS - 1 - (chunk_count - 1).leading_zeros()`. Shifting 1 by that
/// amount is the greatest power of two that is `<= chunk_count - 1`, so it is
/// strictly below `chunk_count`.
fn largest_power_of_two_below(chunk_count: u32) -> u32 {
    debug_assert!(chunk_count >= 2);
    1_u32 << (u32::BITS - 1 - (chunk_count - 1).leading_zeros())
}

/// Chunk-level siblings in the proof of chunk `c` (spec §5).
///
/// Defined for `c < C`. Returns `None` when `chunk_count` is 0 or
/// `chunk_index >= chunk_count`. The length of a proof comes from this
/// function, not from a builder stack (spec §4).
///
/// `c` is `chunk_index`. `C` is `chunk_count`.
pub fn depth(chunk_index: u32, chunk_count: u32) -> Option<u32> {
    if chunk_count == 0 || chunk_index >= chunk_count {
        return None;
    }
    let mut chunk_index = chunk_index;
    let mut chunk_count = chunk_count;
    let mut sibling_count = 0_u32;
    while chunk_count > 1 {
        let left_subtree_size = largest_power_of_two_below(chunk_count);
        if chunk_index < left_subtree_size {
            // `k` is a power of two, so `trailing_zeros` is `log2(k)` (spec §5).
            return Some(sibling_count + 1 + left_subtree_size.trailing_zeros());
        }
        chunk_index -= left_subtree_size;
        chunk_count -= left_subtree_size;
        sibling_count += 1;
    }
    Some(sibling_count)
}

/// RFC 9162 §2.1.3.2 inclusion walk. `node()` is the hash (spec §5).
///
/// `node_index` is the RFC's `fn`. `last_index` is the RFC's `sn`.
/// `running_hash` starts as the chunk-level leaf (`r` in spec §5).
///
/// Returns `None` for a bad index (`chunk_count` is 0 or `chunk_index` is not
/// below `chunk_count`), a length that is not a multiple of [`HASH_SIZE`], or
/// a sibling count other than `depth`. A sibling list of that length returns
/// `Some`. The caller compares that hash with the anchored commitment.
///
/// `c` is `chunk_index`.
pub(crate) fn climb(
    mut running_hash: NodeHash,
    chunk_index: u32,
    chunk_count: u32,
    proof: &[u8],
) -> Option<NodeHash> {
    if chunk_count == 0 || chunk_index >= chunk_count || !proof.len().is_multiple_of(HASH_SIZE) {
        return None;
    }
    // `fn` / `sn` in RFC 9162 §2.1.3.2.
    let mut node_index = chunk_index;
    let mut last_index = chunk_count - 1;
    for sibling in proof.chunks_exact(HASH_SIZE) {
        if last_index == 0 {
            return None;
        }
        let bytes = <[u8; HASH_SIZE]>::try_from(sibling)
            .expect("chunks_exact yields HASH_SIZE-byte slices");
        let sibling = NodeHash::from(bytes);
        // `fn` is odd, or it is the last index: the sibling is on the left (spec §5).
        if node_index & 1 == 1 || node_index == last_index {
            running_hash = node(&sibling, &running_hash);
            // While `fn` is even and not zero, both indices move up one level (spec §5).
            while node_index & 1 == 0 && node_index != 0 {
                node_index >>= 1;
                last_index >>= 1;
            }
        } else {
            running_hash = node(&running_hash, &sibling);
        }
        node_index >>= 1;
        last_index >>= 1;
    }
    if last_index != 0 {
        return None;
    }
    Some(running_hash)
}
