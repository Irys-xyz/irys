//! Domain-separated SHA-256 (spec §3).
//!
//! Each function finalizes one hasher. Parts are not hashed on their own and then
//! hashed again: v1 did that, and the inner hashes add no binding (spec §9).

use std::sync::LazyLock;

use sha2::{Digest as _, Sha256};

use crate::geometry::{Geometry, MAX_SUBTREE_HEIGHT, SubtreeHeight};
use crate::types::{ChunkKey, DataCommitment, DataTreeRoot, HASH_SIZE, NodeHash, SizeWitness};

/// `empty(0) ..= empty(MAX_SUBTREE_HEIGHT)` (spec §3, §8).
const EMPTY_TABLE_LEN: usize = MAX_SUBTREE_HEIGHT as usize + 1;

/// Data-tree domain tags (spec §3).
///
/// Every data-tree preimage starts with one of these bytes. Tags `0x05` to
/// `0x07` are reserved for proposed designs. Do not assign them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum TreeDomainTag {
    /// `0x00 ‖ segment bytes`, at the segment's exact length.
    Leaf = 0x00,
    /// `0x01 ‖ left ‖ right` (one tag byte plus two digests).
    Node = 0x01,
    /// `0x02 ‖ be64(data_size) ‖ be64(chunk_size) ‖ be16(segments_per_chunk)` (19 B).
    Size = 0x02,
    /// `0x03 ‖ data_commitment ‖ be32(c)` (one tag byte, one digest, and a `u32`).
    ChunkKey = 0x03,
    /// Mining ticket. Defined by the segment-PoA spec. Not used by this tree (spec §3).
    Ticket = 0x04,
}

impl TreeDomainTag {
    /// The tag byte this variant hashes (spec §3).
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

/// `H(0x00 ‖ bytes)`. `bytes` is hashed at its exact length: no padding (spec §3).
pub fn leaf(bytes: &[u8]) -> NodeHash {
    NodeHash::from(digest(TreeDomainTag::Leaf.as_byte(), &[bytes]))
}

/// `H(0x01 ‖ left ‖ right)` (spec §3).
pub fn node(left: &NodeHash, right: &NodeHash) -> NodeHash {
    NodeHash::from(digest(
        TreeDomainTag::Node.as_byte(),
        &[left.as_bytes(), right.as_bytes()],
    ))
}

/// `H(0x02 ‖ be64(data_size) ‖ be64(chunk_size) ‖ be16(segments_per_chunk))` (spec §3).
///
/// The witness binds the size and the geometry. `Geometry::new` keeps
/// `chunk_size` in `u64` and `segments_per_chunk` at most
/// [`MAX_SEGMENTS_PER_CHUNK`](crate::geometry::MAX_SEGMENTS_PER_CHUNK), so the
/// `be16` write does not truncate.
pub fn size_witness(geometry: &Geometry, data_size: u64) -> SizeWitness {
    let segments_per_chunk = u16::try_from(geometry.segments_per_chunk())
        .expect("segments_per_chunk fits be16: Geometry::new caps it at MAX_SEGMENTS_PER_CHUNK");
    SizeWitness::from(digest(
        TreeDomainTag::Size.as_byte(),
        &[
            &data_size.to_be_bytes(),
            &geometry.chunk_size().to_be_bytes(),
            &segments_per_chunk.to_be_bytes(),
        ],
    ))
}

/// `H(0x03 ‖ data_commitment ‖ be32(c))` (spec §3).
///
/// `c` is `chunk_index`.
pub fn chunk_key(commitment: &DataCommitment, chunk_index: u32) -> ChunkKey {
    ChunkKey::from(digest(
        TreeDomainTag::ChunkKey.as_byte(),
        &[commitment.as_bytes(), &chunk_index.to_be_bytes()],
    ))
}

/// Root of an empty perfect subtree of height `k` (spec §3, §8).
///
/// `empty(0)` is `leaf("")`. `empty(k)` is `node(empty(k - 1), empty(k - 1))`.
/// [`SubtreeHeight`] holds at most [`MAX_SUBTREE_HEIGHT`], so every height has
/// a table entry (spec §2).
///
/// `k` is `empty_height`.
pub fn empty(empty_height: SubtreeHeight) -> NodeHash {
    static TABLE: LazyLock<[NodeHash; EMPTY_TABLE_LEN]> = LazyLock::new(|| {
        let mut table = [NodeHash::from([0; HASH_SIZE]); EMPTY_TABLE_LEN];
        table[0] = leaf(b"");
        for height in 1..table.len() {
            table[height] = node(&table[height - 1], &table[height - 1]);
        }
        table
    });
    let index = usize::try_from(empty_height.get()).expect("subtree height fits usize");
    TABLE[index]
}

/// `node(data_tree_root, size_witness(G, data_size))` (spec §4).
pub fn data_commitment(geometry: &Geometry, root: &DataTreeRoot, data_size: u64) -> DataCommitment {
    let root_hash = NodeHash::from(*root.as_bytes());
    let witness = NodeHash::from(*size_witness(geometry, data_size).as_bytes());
    DataCommitment::from(*node(&root_hash, &witness).as_bytes())
}
