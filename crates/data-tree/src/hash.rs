//! Domain-separated SHA-256 (spec §3).
//!
//! Each function finalizes one hasher. Parts are not hashed on their own and then
//! hashed again: v1 did that, and the inner hashes add no binding (spec §10).

use std::sync::LazyLock;

use sha2::{Digest as _, Sha256};

use crate::geometry::MAX_SUBTREE_HEIGHT;
use crate::types::{ChunkKey, DataCommitment, DataTreeRoot, HASH_SIZE, NodeHash, SizeWitness};

/// `empty(0) ..= empty(MAX_SUBTREE_HEIGHT)` (spec §3, §9).
const EMPTY_TABLE_LEN: usize = MAX_SUBTREE_HEIGHT as usize + 1;

/// Data-tree domain tags (spec §3).
///
/// Every data-tree preimage starts with one of these bytes. `tx_root` v2
/// assigns `0x05` to `0x07` (spec §3, Appendix C). Those tags are not part of
/// this tree, and this enum does not include them until `tx_root` v2 is
/// implemented. Any later tag set must avoid `0x00` to `0x07`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum TreeDomainTag {
    /// `0x00 ‖ segment bytes`, at the segment's exact length.
    Leaf = 0x00,
    /// `0x01 ‖ left ‖ right` (one tag byte plus two digests).
    Node = 0x01,
    /// `0x02 ‖ be64(data_size)` (one tag byte plus a `u64`).
    Size = 0x02,
    /// `0x03 ‖ data_commitment ‖ be32(c)` (one tag byte, one digest, and a `u32`).
    ChunkKey = 0x03,
    /// Reserved: defined by the segment-PoA spec; not used by the tree.
    Ticket = 0x04,
}

impl TreeDomainTag {
    pub const fn as_byte(self) -> u8 {
        self as u8
    }
}

fn digest(tag: u8, parts: &[&[u8]]) -> [u8; HASH_SIZE] {
    let mut hasher = Sha256::new();
    hasher.update([tag]);
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// `H(Leaf ‖ bytes)`. `bytes` is hashed at its exact length: no padding (spec §3).
pub fn leaf(bytes: &[u8]) -> NodeHash {
    NodeHash(digest(TreeDomainTag::Leaf.as_byte(), &[bytes]))
}

/// `H(Node ‖ left ‖ right)` (spec §3).
pub fn node(left: &NodeHash, right: &NodeHash) -> NodeHash {
    NodeHash(digest(
        TreeDomainTag::Node.as_byte(),
        &[left.as_bytes(), right.as_bytes()],
    ))
}

/// `H(Size ‖ be64(data_size))` (spec §3). The witness covers `data_size` only.
pub fn size_witness(data_size: u64) -> SizeWitness {
    SizeWitness(digest(
        TreeDomainTag::Size.as_byte(),
        &[&data_size.to_be_bytes()],
    ))
}

/// `H(ChunkKey ‖ data_commitment ‖ be32(c))` (spec §3).
///
/// `c` is `chunk_index`.
pub fn chunk_key(commitment: &DataCommitment, chunk_index: u32) -> ChunkKey {
    ChunkKey(digest(
        TreeDomainTag::ChunkKey.as_byte(),
        &[commitment.as_bytes(), &chunk_index.to_be_bytes()],
    ))
}

/// Root of an empty perfect subtree of height `k` (spec §3, §9).
///
/// `empty(0)` is `leaf("")`. `empty(k)` is `node(empty(k - 1), empty(k - 1))`.
/// Panics if `k > MAX_SUBTREE_HEIGHT`. A chunk subtree never asks for more:
/// `segments_per_chunk` is at most [`MAX_SEGMENTS_PER_CHUNK`](crate::geometry::MAX_SEGMENTS_PER_CHUNK),
/// so its height is at most [`MAX_SUBTREE_HEIGHT`] (spec §2).
///
/// `k` is `empty_height`.
pub fn empty(empty_height: u32) -> NodeHash {
    static TABLE: LazyLock<[NodeHash; EMPTY_TABLE_LEN]> = LazyLock::new(|| {
        let mut table = [NodeHash([0; HASH_SIZE]); EMPTY_TABLE_LEN];
        table[0] = leaf(b"");
        for height in 1..table.len() {
            table[height] = node(&table[height - 1], &table[height - 1]);
        }
        table
    });
    assert!(
        empty_height <= MAX_SUBTREE_HEIGHT,
        "empty height {empty_height} exceeds {MAX_SUBTREE_HEIGHT}"
    );
    TABLE[empty_height as usize]
}

/// `node(data_tree_root, size_witness(data_size))` (spec §4).
pub fn data_commitment(root: &DataTreeRoot, data_size: u64) -> DataCommitment {
    let root_hash = NodeHash(*root.as_bytes());
    let witness = NodeHash(*size_witness(data_size).as_bytes());
    DataCommitment(*node(&root_hash, &witness).as_bytes())
}
