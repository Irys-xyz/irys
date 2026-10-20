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
pub fn depth(c: u32, chunk_count: u32) -> u32 {
    debug_assert!(c < chunk_count);
    let mut c = c;
    let mut chunk_count = chunk_count;
    let mut siblings = 0_u32;
    while chunk_count > 1 {
        let k = largest_power_of_two_below(chunk_count);
        if c < k {
            return siblings + 1 + k.trailing_zeros();
        }
        c -= k;
        chunk_count -= k;
        siblings += 1;
    }
    siblings
}

/// RFC 9162 §2.1.3.2 inclusion walk. `node` is the parent hash (spec §5).
///
/// Returns `None` when the proof is too long, too short, or not a sequence of
/// 32-byte hashes. A verifier checks the proof length before calling this, so
/// `None` here means the bytes do not walk to a root.
pub(crate) fn climb(
    mut root: NodeHash,
    c: u32,
    chunk_count: u32,
    proof: &[u8],
) -> Option<NodeHash> {
    if chunk_count == 0 || c >= chunk_count || !proof.len().is_multiple_of(32) {
        return None;
    }
    let mut fn_idx = c;
    let mut sn = chunk_count - 1;
    for sibling in proof.chunks_exact(32) {
        if sn == 0 {
            return None;
        }
        let Ok(bytes) = <[u8; 32]>::try_from(sibling) else {
            return None;
        };
        let sibling = NodeHash(bytes);
        if fn_idx & 1 == 1 || fn_idx == sn {
            root = node(&sibling, &root);
            while fn_idx & 1 == 0 && fn_idx != 0 {
                fn_idx >>= 1;
                sn >>= 1;
            }
        } else {
            root = node(&root, &sibling);
        }
        fn_idx >>= 1;
        sn >>= 1;
    }
    if sn != 0 {
        return None;
    }
    Some(root)
}
