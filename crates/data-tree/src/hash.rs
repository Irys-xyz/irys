//! Domain-separated SHA-256 (spec §3).
//!
//! Each function finalizes one hasher. Parts are not hashed on their own and then
//! hashed again: v1 did that, and the inner hashes add no binding (spec §10).

use std::sync::LazyLock;

use sha2::{Digest as _, Sha256};

use crate::types::{ChunkKey, DataCommitment, DataTreeRoot, NodeHash, SizeWitness};

/// Single registry of domain tags (spec §3).
///
/// Every preimage starts with one of these bytes. Any later tag set (for
/// example `tx_root` tags, spec §14 Q3) must avoid these bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum TreeDomainTag {
    /// `0x00 ‖ segment bytes`, at the segment's exact length.
    Leaf = 0x00,
    /// `0x01 ‖ left ‖ right` (65 bytes).
    Node = 0x01,
    /// `0x02 ‖ be64(data_size)` (9 bytes).
    Size = 0x02,
    /// `0x03 ‖ data_commitment ‖ be32(c)` (37 bytes).
    ChunkKey = 0x03,
    /// Reserved: defined by the segment-PoA spec; not used by the tree.
    Ticket = 0x04,
}

impl TreeDomainTag {
    pub const fn as_byte(self) -> u8 {
        self as u8
    }
}

fn digest(tag: u8, parts: &[&[u8]]) -> [u8; 32] {
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
pub fn chunk_key(commitment: &DataCommitment, c: u32) -> ChunkKey {
    ChunkKey(digest(
        TreeDomainTag::ChunkKey.as_byte(),
        &[commitment.as_bytes(), &c.to_be_bytes()],
    ))
}

/// Root of an empty perfect subtree of height `k` (spec §3, §9).
///
/// `empty(0)` is `leaf("")`. `empty(k)` is `node(empty(k - 1), empty(k - 1))`.
/// Panics if `k > 8`. A chunk subtree never asks for more: `segments_per_chunk`
/// is at most 256, so its height is at most 8 (spec §2).
pub fn empty(k: u32) -> NodeHash {
    static TABLE: LazyLock<[NodeHash; 9]> = LazyLock::new(|| {
        let mut table = [NodeHash([0; 32]); 9];
        table[0] = leaf(b"");
        for height in 1..table.len() {
            table[height] = node(&table[height - 1], &table[height - 1]);
        }
        table
    });
    assert!(k <= 8, "empty height {k} exceeds 8");
    TABLE[k as usize]
}

/// `node(data_tree_root, size_witness(data_size))` (spec §4).
pub fn data_commitment(root: &DataTreeRoot, data_size: u64) -> DataCommitment {
    let root_hash = NodeHash(*root.as_bytes());
    let witness = NodeHash(*size_witness(data_size).as_bytes());
    DataCommitment(*node(&root_hash, &witness).as_bytes())
}
