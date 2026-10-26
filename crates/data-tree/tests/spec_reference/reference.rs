//! Reference oracle for the data tree, written from `docs/spec.md` only.
//!
//! It imports nothing from `irys_data_tree`. It uses plain integers, byte
//! vectors and its own small types. Every hash builds its whole preimage in one
//! `Vec<u8>` and calls SHA-256 once. Every definition follows the spec
//! literally and ignores speed: recursion where the spec recurses, a chunk
//! subtree built level by level, no caches.

use sha2::{Digest as _, Sha256};

/// Width of every hash in bytes (spec §3: SHA-256).
pub(crate) const HASH_SIZE: usize = 32;

/// One SHA-256 digest.
pub(crate) type Hash = [u8; HASH_SIZE];

/// Tag of a segment leaf (spec §3).
const LEAF_TAG: u8 = 0x00;
/// Tag of an internal node (spec §3).
const NODE_TAG: u8 = 0x01;
/// Tag of the size witness (spec §3).
const SIZE_TAG: u8 = 0x02;
/// Tag of a chunk key (spec §3).
const CHUNK_KEY_TAG: u8 = 0x03;

/// Largest `segments_per_chunk` the tree accepts (spec §2).
const MAX_SEGMENTS_PER_CHUNK: u32 = 256;

/// `G = (chunk_size, segments_per_chunk, max_data_tx_chunks)` (spec §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Geometry {
    pub(crate) chunk_size: u64,
    pub(crate) segments_per_chunk: u32,
    pub(crate) max_data_tx_chunks: u32,
}

impl Geometry {
    /// Apply the spec §2 validity rules.
    pub(crate) fn new(
        chunk_size: u64,
        segments_per_chunk: u32,
        max_data_tx_chunks: u32,
    ) -> Result<Self, &'static str> {
        if !segments_per_chunk.is_power_of_two() || segments_per_chunk > MAX_SEGMENTS_PER_CHUNK {
            return Err("segments_per_chunk must be a power of two in 1..=256");
        }
        if chunk_size < 1 {
            return Err("chunk_size must be at least 1");
        }
        if !chunk_size.is_multiple_of(u64::from(segments_per_chunk)) {
            return Err("chunk_size must be a multiple of segments_per_chunk");
        }
        if max_data_tx_chunks < 1 {
            return Err("max_data_tx_chunks must be at least 1");
        }
        if chunk_size
            .checked_mul(u64::from(max_data_tx_chunks))
            .is_none()
        {
            return Err("chunk_size * max_data_tx_chunks must fit in u64");
        }
        Ok(Self {
            chunk_size,
            segments_per_chunk,
            max_data_tx_chunks,
        })
    }

    /// `segment_size = chunk_size / segments_per_chunk` (spec §2).
    pub(crate) fn segment_size(&self) -> u64 {
        self.chunk_size / u64::from(self.segments_per_chunk)
    }

    /// `h = log2(segments_per_chunk)`, the height of a chunk subtree (spec §2).
    pub(crate) fn subtree_height(&self) -> u32 {
        self.segments_per_chunk.trailing_zeros()
    }

    /// `max = max_data_tx_chunks × chunk_size` (spec §5).
    pub(crate) fn max_data_size(&self) -> u64 {
        self.chunk_size * u64::from(self.max_data_tx_chunks)
    }

    /// `C = ceil(n / chunk_size)` (spec §2). `1 ≤ n ≤ max`.
    pub(crate) fn chunk_count(&self, data_size: u64) -> u32 {
        assert!((1..=self.max_data_size()).contains(&data_size));
        u32::try_from(data_size.div_ceil(self.chunk_size)).expect("chunk count fits u32")
    }

    /// `chunk_len(n, c) = min(chunk_size, n − c × chunk_size)` (spec §2). `c < C`.
    pub(crate) fn chunk_len(&self, data_size: u64, chunk_index: u32) -> u64 {
        assert!(chunk_index < self.chunk_count(data_size));
        let chunk_start = u64::from(chunk_index) * self.chunk_size;
        self.chunk_size.min(data_size - chunk_start)
    }

    /// `segment_len(n, c, j)` (spec §2): 0 when the segment starts at or past the data.
    pub(crate) fn segment_len(&self, data_size: u64, chunk_index: u32, segment_index: u32) -> u64 {
        let segment_start = u64::from(chunk_index) * self.chunk_size
            + u64::from(segment_index) * self.segment_size();
        if segment_start >= data_size {
            0
        } else {
            self.segment_size().min(data_size - segment_start)
        }
    }
}

/// Why a verifier rejects an input: one variant per spec §5 `require` group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rejection {
    /// `n` outside `1 ..= max`, `c ≥ C`, or `j ≥ segments_per_chunk`.
    Range,
    /// `len(bytes)` is not `chunk_len` or `segment_len`.
    ByteLength,
    /// `len(proof)` is not the exact length for `(n, c[, j])`.
    ProofLength,
    /// The last hash of the proof is not `size_witness(G, n)`.
    SizeWitness,
    /// The climbed root, or a pinned empty sibling, does not open the commitment.
    Commitment,
}

/// SHA-256 of a complete preimage (spec §2: `H`).
pub(crate) fn sha256(preimage: &[u8]) -> Hash {
    Sha256::digest(preimage).into()
}

/// `leaf(bytes) = H(0x00 ‖ bytes)` (spec §3).
pub(crate) fn leaf(bytes: &[u8]) -> Hash {
    let mut preimage = vec![LEAF_TAG];
    preimage.extend_from_slice(bytes);
    sha256(&preimage)
}

/// `node(l, r) = H(0x01 ‖ l ‖ r)` (spec §3).
pub(crate) fn node(left: &Hash, right: &Hash) -> Hash {
    let mut preimage = vec![NODE_TAG];
    preimage.extend_from_slice(left);
    preimage.extend_from_slice(right);
    sha256(&preimage)
}

/// `size_witness(G, n) = H(0x02 ‖ be64(n) ‖ be64(chunk_size) ‖ be16(segments_per_chunk))` (spec §3).
pub(crate) fn size_witness(geometry: &Geometry, data_size: u64) -> Hash {
    let segments_per_chunk = u16::try_from(geometry.segments_per_chunk)
        .expect("segments_per_chunk is at most 256 (spec §2)");
    let mut preimage = vec![SIZE_TAG];
    preimage.extend_from_slice(&data_size.to_be_bytes());
    preimage.extend_from_slice(&geometry.chunk_size.to_be_bytes());
    preimage.extend_from_slice(&segments_per_chunk.to_be_bytes());
    sha256(&preimage)
}

/// `empty(0) = leaf("")`, `empty(k) = node(empty(k−1), empty(k−1))` (spec §3).
pub(crate) fn empty(height: u32) -> Hash {
    if height == 0 {
        leaf(&[])
    } else {
        node(&empty(height - 1), &empty(height - 1))
    }
}

/// `chunk_key(D, c) = H(0x03 ‖ D ‖ be32(c))` (spec §3).
pub(crate) fn chunk_key(commitment: &Hash, chunk_index: u32) -> Hash {
    let mut preimage = vec![CHUNK_KEY_TAG];
    preimage.extend_from_slice(commitment);
    preimage.extend_from_slice(&chunk_index.to_be_bytes());
    sha256(&preimage)
}

/// The leaf of every segment of one chunk (spec §4).
///
/// A segment past the data is `leaf("") = empty(0)`. `1 ≤ len(bytes) ≤ chunk_size`.
pub(crate) fn segment_leaves(geometry: &Geometry, chunk_bytes: &[u8]) -> Vec<Hash> {
    assert!(!chunk_bytes.is_empty());
    assert!(chunk_bytes.len() as u64 <= geometry.chunk_size);
    let segment_size = usize::try_from(geometry.segment_size()).expect("segment size fits usize");
    (0..geometry.segments_per_chunk)
        .map(|segment_index| {
            let segment_start = usize::try_from(segment_index).expect("index fits") * segment_size;
            if segment_start >= chunk_bytes.len() {
                leaf(&[])
            } else {
                let segment_end = (segment_start + segment_size).min(chunk_bytes.len());
                leaf(&chunk_bytes[segment_start..segment_end])
            }
        })
        .collect()
}

/// The chunk root `R_c`: a perfect tree over the segment leaves, built level by level (spec §4).
pub(crate) fn chunk_root(geometry: &Geometry, chunk_bytes: &[u8]) -> Hash {
    let mut level = segment_leaves(geometry, chunk_bytes);
    while level.len() > 1 {
        level = level
            .chunks(2)
            .map(|pair| node(&pair[0], &pair[1]))
            .collect();
    }
    level[0]
}

/// Root of a perfect subtree over a power-of-two number of leaves (spec §4).
fn perfect_subtree_root(leaves: &[Hash]) -> Hash {
    assert!(leaves.len().is_power_of_two());
    if leaves.len() == 1 {
        leaves[0]
    } else {
        let (left_half, right_half) = leaves.split_at(leaves.len() / 2);
        node(
            &perfect_subtree_root(left_half),
            &perfect_subtree_root(right_half),
        )
    }
}

/// The largest power of two strictly below `count` (spec §4: `k`). `count > 1`.
fn largest_power_of_two_below(count: usize) -> usize {
    assert!(count > 1);
    let mut power = 1;
    while power * 2 < count {
        power *= 2;
    }
    power
}

/// `T(R[0..C])`: the chunk-level root (spec §4).
pub(crate) fn data_tree_root(chunk_roots: &[Hash]) -> Hash {
    assert!(!chunk_roots.is_empty());
    if chunk_roots.len() == 1 {
        return chunk_roots[0];
    }
    let left_count = largest_power_of_two_below(chunk_roots.len());
    node(
        &data_tree_root(&chunk_roots[..left_count]),
        &data_tree_root(&chunk_roots[left_count..]),
    )
}

/// `data_commitment = node(data_tree_root, size_witness(G, n))` (spec §4).
pub(crate) fn data_commitment(geometry: &Geometry, root: &Hash, data_size: u64) -> Hash {
    node(root, &size_witness(geometry, data_size))
}

/// `path(c, R[0..C])`: the sibling hashes of chunk `c`, leaf to root (spec §4).
pub(crate) fn path(chunk_index: usize, chunk_roots: &[Hash]) -> Vec<Hash> {
    assert!(chunk_index < chunk_roots.len());
    if chunk_roots.len() == 1 {
        return Vec::new();
    }
    let left_count = largest_power_of_two_below(chunk_roots.len());
    if chunk_index < left_count {
        let mut siblings = path(chunk_index, &chunk_roots[..left_count]);
        siblings.push(data_tree_root(&chunk_roots[left_count..]));
        siblings
    } else {
        let mut siblings = path(chunk_index - left_count, &chunk_roots[left_count..]);
        siblings.push(data_tree_root(&chunk_roots[..left_count]));
        siblings
    }
}

/// `depth(c, C)`: the chunk-level siblings in the proof of chunk `c` (spec §5). `c < C`.
pub(crate) fn depth(mut chunk_index: u32, mut chunk_count: u32) -> u32 {
    assert!(chunk_index < chunk_count);
    let mut chunk_level_steps = 0;
    while chunk_count > 1 {
        let left_count = largest_power_of_two_below(chunk_count as usize) as u32;
        if chunk_index < left_count {
            return chunk_level_steps + 1 + left_count.trailing_zeros();
        }
        chunk_index -= left_count;
        chunk_count -= left_count;
        chunk_level_steps += 1;
    }
    chunk_level_steps
}

fn concatenate(hashes: &[Hash]) -> Vec<u8> {
    hashes.concat()
}

/// Chunk proof: `path(c) ++ [size_witness(G, n)]` (spec §4, §5).
pub(crate) fn chunk_proof(
    geometry: &Geometry,
    data_size: u64,
    chunk_roots: &[Hash],
    chunk_index: u32,
) -> Vec<u8> {
    let mut hashes = path(
        usize::try_from(chunk_index).expect("index fits"),
        chunk_roots,
    );
    hashes.push(size_witness(geometry, data_size));
    concatenate(&hashes)
}

/// First segment of the sibling subtree at `level`: `((j >> level) ^ 1) << level` (spec §5).
fn sibling_subtree_start(segment_index: u32, level: u32) -> u32 {
    ((segment_index >> level) ^ 1) << level
}

/// The `h` in-chunk siblings of segment `j`, level 0 first (spec §4, §5).
pub(crate) fn in_chunk_siblings(
    geometry: &Geometry,
    chunk_bytes: &[u8],
    segment_index: u32,
) -> Vec<Hash> {
    let leaves = segment_leaves(geometry, chunk_bytes);
    (0..geometry.subtree_height())
        .map(|level| {
            let start = usize::try_from(sibling_subtree_start(segment_index, level))
                .expect("start fits usize");
            let width = 1_usize << level;
            perfect_subtree_root(&leaves[start..start + width])
        })
        .collect()
}

/// Segment proof: the in-chunk siblings, then the chunk proof (spec §4, §5).
pub(crate) fn segment_proof(
    geometry: &Geometry,
    data_size: u64,
    chunk_roots: &[Hash],
    chunk_index: u32,
    segment_index: u32,
    chunk_bytes: &[u8],
) -> Vec<u8> {
    let mut proof = concatenate(&in_chunk_siblings(geometry, chunk_bytes, segment_index));
    proof.extend(chunk_proof(geometry, data_size, chunk_roots, chunk_index));
    proof
}

/// Split a proof of `32 × k` bytes into its `k` hashes. The caller checked the length.
fn proof_hashes(proof: &[u8]) -> Vec<Hash> {
    proof
        .chunks_exact(HASH_SIZE)
        .map(|hash| Hash::try_from(hash).expect("chunks_exact yields whole hashes"))
        .collect()
}

/// `climb(r, c, C, hashes)`: the RFC 9162 §2.1.3.2 inclusion walk with `node()` (spec §5).
fn climb(
    start_hash: Hash,
    chunk_index: u32,
    chunk_count: u32,
    hashes: &[Hash],
) -> Result<Hash, Rejection> {
    let mut running_hash = start_hash;
    // The RFC names these `fn` and `sn`.
    let mut node_index = chunk_index;
    let mut last_index = chunk_count - 1;
    for sibling in hashes {
        if last_index == 0 {
            // Proof too long.
            return Err(Rejection::ProofLength);
        }
        if node_index % 2 == 1 || node_index == last_index {
            running_hash = node(sibling, &running_hash);
            while node_index.is_multiple_of(2) && node_index != 0 {
                node_index >>= 1;
                last_index >>= 1;
            }
        } else {
            running_hash = node(&running_hash, sibling);
        }
        node_index >>= 1;
        last_index >>= 1;
    }
    if last_index != 0 {
        // Proof too short.
        return Err(Rejection::ProofLength);
    }
    Ok(running_hash)
}

/// `1 ≤ n ≤ max`, then `C`, then `c < C` (spec §5). Returns `C`.
fn require_chunk_in_range(
    geometry: &Geometry,
    data_size: u64,
    chunk_index: u32,
) -> Result<u32, Rejection> {
    if !(1..=geometry.max_data_size()).contains(&data_size) {
        return Err(Rejection::Range);
    }
    let chunk_count = geometry.chunk_count(data_size);
    if chunk_index >= chunk_count {
        return Err(Rejection::Range);
    }
    Ok(chunk_count)
}

/// Require the last hash of the proof to equal `size_witness(G, n)`, else stop (spec §5).
fn require_size_witness_last(
    geometry: &Geometry,
    data_size: u64,
    hashes: &[Hash],
) -> Result<(), Rejection> {
    let (last_hash, _) = hashes
        .split_last()
        .expect("the proof length is at least one hash");
    if *last_hash == size_witness(geometry, data_size) {
        Ok(())
    } else {
        Err(Rejection::SizeWitness)
    }
}

/// `verify_chunk(G, D, n, c, bytes, proof)` in the exact spec §5 order.
pub(crate) fn verify_chunk(
    geometry: &Geometry,
    commitment: &Hash,
    data_size: u64,
    chunk_index: u32,
    bytes: &[u8],
    proof: &[u8],
) -> Result<(), Rejection> {
    let chunk_count = require_chunk_in_range(geometry, data_size, chunk_index)?;
    if bytes.len() as u64 != geometry.chunk_len(data_size, chunk_index) {
        return Err(Rejection::ByteLength);
    }
    let expected_hash_count = depth(chunk_index, chunk_count) as usize + 1;
    if proof.len() != HASH_SIZE * expected_hash_count {
        return Err(Rejection::ProofLength);
    }
    let hashes = proof_hashes(proof);
    require_size_witness_last(geometry, data_size, &hashes)?;
    let (witness, siblings) = hashes.split_last().expect("proof holds the witness");
    let root = climb(
        chunk_root(geometry, bytes),
        chunk_index,
        chunk_count,
        siblings,
    )?;
    if node(&root, witness) == *commitment {
        Ok(())
    } else {
        Err(Rejection::Commitment)
    }
}

/// `verify_segment(G, D, n, c, j, bytes, proof)` in the exact spec §5 order.
pub(crate) fn verify_segment(
    geometry: &Geometry,
    commitment: &Hash,
    data_size: u64,
    chunk_index: u32,
    segment_index: u32,
    bytes: &[u8],
    proof: &[u8],
) -> Result<(), Rejection> {
    let chunk_count = require_chunk_in_range(geometry, data_size, chunk_index)?;
    if segment_index >= geometry.segments_per_chunk {
        return Err(Rejection::Range);
    }
    if bytes.len() as u64 != geometry.segment_len(data_size, chunk_index, segment_index) {
        return Err(Rejection::ByteLength);
    }
    let subtree_height = geometry.subtree_height();
    let expected_hash_count =
        subtree_height as usize + depth(chunk_index, chunk_count) as usize + 1;
    if proof.len() != HASH_SIZE * expected_hash_count {
        return Err(Rejection::ProofLength);
    }
    let hashes = proof_hashes(proof);
    require_size_witness_last(geometry, data_size, &hashes)?;
    let (witness, siblings) = hashes.split_last().expect("proof holds the witness");
    // `s`: a segment at or past this index is empty.
    let data_segment_count = geometry
        .chunk_len(data_size, chunk_index)
        .div_ceil(geometry.segment_size());
    let mut running_hash = leaf(bytes);
    let mut position = segment_index;
    for level in 0..subtree_height {
        let sibling = &siblings[level as usize];
        let start = sibling_subtree_start(segment_index, level);
        if u64::from(start) >= data_segment_count && *sibling != empty(level) {
            return Err(Rejection::Commitment);
        }
        running_hash = if position.is_multiple_of(2) {
            node(&running_hash, sibling)
        } else {
            node(sibling, &running_hash)
        };
        position >>= 1;
    }
    let root = climb(
        running_hash,
        chunk_index,
        chunk_count,
        &siblings[subtree_height as usize..],
    )?;
    if node(&root, witness) == *commitment {
        Ok(())
    } else {
        Err(Rejection::Commitment)
    }
}
