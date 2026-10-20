//! Chunk-tree shape: `depth` and the RFC 9162 inclusion walk (spec §4, §5).

use crate::hash::node;
use crate::types::NodeHash;

/// Largest power of two strictly below `chunk_count`.
///
/// `chunk_count >= 2`. `1 << (31 - (C - 1).leading_zeros())` (spec §4).
fn largest_power_of_two_below(chunk_count: u32) -> u32 {
    debug_assert!(chunk_count >= 2);
    1_u32 << (31 - (chunk_count - 1).leading_zeros())
}

/// Chunk-level siblings in the proof of chunk `c` (spec §5).
///
/// The caller guarantees `c < chunk_count`. The length of a proof comes from
/// this function, not from a builder stack (spec §4).
///
/// `c` is `chunk_index`.
pub fn depth(chunk_index: u32, chunk_count: u32) -> u32 {
    debug_assert!(chunk_index < chunk_count);
    let mut chunk_index = chunk_index;
    let mut chunk_count = chunk_count;
    let mut sibling_count = 0_u32;
    while chunk_count > 1 {
        let left_subtree_size = largest_power_of_two_below(chunk_count);
        if chunk_index < left_subtree_size {
            return sibling_count + 1 + left_subtree_size.trailing_zeros();
        }
        chunk_index -= left_subtree_size;
        chunk_count -= left_subtree_size;
        sibling_count += 1;
    }
    sibling_count
}

/// RFC 9162 §2.1.3.2 inclusion walk. `node` is the parent hash (spec §5).
///
/// `node_index` is the RFC's `fn`. `last_index` is the RFC's `sn`.
///
/// Returns `None` when the proof is too long, too short, or not a sequence of
/// 32-byte hashes. A verifier checks the proof length before calling this, so
/// `None` here means the bytes do not walk to a root.
///
/// `c` is `chunk_index`.
pub(crate) fn climb(
    mut root: NodeHash,
    chunk_index: u32,
    chunk_count: u32,
    proof: &[u8],
) -> Option<NodeHash> {
    if chunk_count == 0 || chunk_index >= chunk_count || !proof.len().is_multiple_of(32) {
        return None;
    }
    // `fn` / `sn` in RFC 9162 §2.1.3.2.
    let mut node_index = chunk_index;
    let mut last_index = chunk_count - 1;
    for sibling in proof.chunks_exact(32) {
        if last_index == 0 {
            return None;
        }
        let Ok(bytes) = <[u8; 32]>::try_from(sibling) else {
            return None;
        };
        let sibling = NodeHash(bytes);
        if node_index & 1 == 1 || node_index == last_index {
            root = node(&sibling, &root);
            while node_index & 1 == 0 && node_index != 0 {
                node_index >>= 1;
                last_index >>= 1;
            }
        } else {
            root = node(&root, &sibling);
        }
        node_index >>= 1;
        last_index >>= 1;
    }
    if last_index != 0 {
        return None;
    }
    Some(root)
}
