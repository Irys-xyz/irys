use crate::geometry::Geometry;
use crate::hash::{chunk_leaf, node, size_witness};
use crate::shape::{climb, depth};
use crate::types::{HASH_SIZE, NodeHash};
use crate::{DataTree, DataTreeBuilder};

/// Every chunk count from 1 through this count is checked against `build_layer`
/// and recursive `T` (spec §4).
const SHAPE_LOCK_MAX_CHUNK_COUNT: u32 = 4_097;

/// Spec §10: the reference implementation checked the builder against the
/// recursive definition for every chunk count from 1 to this count.
const REFERENCE_CHECK_LAST_CHUNK_COUNT: u32 = 299;

/// Proofs are checked for every chunk through this count.
///
/// One past [`REFERENCE_CHECK_LAST_CHUNK_COUNT`], so the spec §10 range sits
/// inside the checked counts.
const PROOF_EVERY_CHUNK_MAX_CHUNK_COUNT: u32 = REFERENCE_CHECK_LAST_CHUNK_COUNT + 1;

/// Builder layers and root for every chunk count through
/// [`SHAPE_LOCK_MAX_CHUNK_COUNT`] (spec §4).
///
/// For every chunk count from 1 through [`SHAPE_LOCK_MAX_CHUNK_COUNT`] this
/// test builds a new tree. The layers must equal `build_layer`. The root must
/// equal recursive `T`. `chunk_count` and `data_size` must match the chunks
/// that were pushed.
///
/// A carry depends on the whole binary form of the chunk count. Checking a
/// few powers of two does not cover the other counts, so this test checks
/// every count in the lock.
///
/// The oracle is plain `build_layer` and recursive `T`. It keeps no cache.
/// Proof checks run in
/// [`builder_proofs_match_spec_path_for_even_chunk_indexes`] and
/// [`builder_proofs_match_spec_path_for_odd_chunk_indexes`], so nextest can run
/// this test and the proof tests at the same time.
#[test]
fn builder_layers_and_root_match_spec_for_every_chunk_count() {
    let chunk_size = Geometry::TESTING.chunk_size();
    for chunk_count in 1..=SHAPE_LOCK_MAX_CHUNK_COUNT {
        let (leaves, tree) = tree_for_chunk_count(chunk_count);
        let expect_layers = build_layer(&leaves);
        assert_eq!(
            tree.layers().len(),
            expect_layers.len(),
            "layers C={chunk_count}"
        );
        for (level, (built, expect)) in tree.layers().iter().zip(&expect_layers).enumerate() {
            assert_eq!(built, expect, "layer {level} C={chunk_count}");
        }
        assert_eq!(tree.chunk_count(), chunk_count);
        assert_eq!(tree.data_size(), u64::from(chunk_count) * chunk_size);
        let expect_root = spec_t(&leaves);
        assert_eq!(
            tree.data_tree_root().as_bytes(),
            expect_root.as_bytes(),
            "root C={chunk_count}"
        );
    }
}

/// Even chunk indexes against recursive `path` (spec §4, §5, §10).
///
/// See [`assert_proofs_for_chunk_index_half`]. The odd indexes are
/// [`builder_proofs_match_spec_path_for_odd_chunk_indexes`]. One test that
/// checked every index sat on nextest's 30 s slow period: each proof calls
/// `path`, and `path` recomputes every sibling subtree.
#[test]
fn builder_proofs_match_spec_path_for_even_chunk_indexes() {
    assert_proofs_for_chunk_index_half(ChunkIndexHalf::Even);
}

/// Odd chunk indexes against recursive `path` (spec §4, §5, §10).
///
/// See [`assert_proofs_for_chunk_index_half`]. Index 0 of a one-chunk tree is
/// even, so this test checks no index there. The even test checks that index.
#[test]
fn builder_proofs_match_spec_path_for_odd_chunk_indexes() {
    assert_proofs_for_chunk_index_half(ChunkIndexHalf::Odd);
}

/// Even or odd chunk indexes. The two halves are complements.
enum ChunkIndexHalf {
    Even,
    Odd,
}

/// Every chunk count the proof tests share, each index in one half.
///
/// Counts are 1 through [`PROOF_EVERY_CHUNK_MAX_CHUNK_COUNT`], then each power
/// of two from 512 through 4,096 with the count on each side, through 4,097.
/// Spec §10 records a reference check for every count from 1 to 299. The range
/// here goes one count further. 512 is the next power of two after 300. 4,096
/// is the greatest power of two below [`SHAPE_LOCK_MAX_CHUNK_COUNT`], and
/// 4,096 + 1 is that count. 256 is already inside 1..=300.
///
/// Each expected proof is one call to `path`. Sibling subtrees are recomputed.
/// The oracle keeps no cache. Even and odd indexes are separate tests so each
/// stays under the nextest slow period. The number of even indexes plus the
/// number of odd indexes is the chunk count, so the two tests together still
/// check every chunk.
fn assert_proofs_for_chunk_index_half(half: ChunkIndexHalf) {
    for chunk_count in proof_chunk_counts() {
        let even_index_count = chunk_count.div_ceil(2);
        let odd_index_count = chunk_count / 2;
        assert_eq!(
            even_index_count + odd_index_count,
            chunk_count,
            "the two proof tests cover every chunk of C={chunk_count}"
        );
        let (leaves, tree) = tree_for_chunk_count(chunk_count);
        let expect_root = spec_t(&leaves);
        let mut checked = 0_u32;
        for chunk_index in 0..chunk_count {
            let even_index = chunk_index % 2 == 0;
            let in_this_half = match half {
                ChunkIndexHalf::Even => even_index,
                ChunkIndexHalf::Odd => !even_index,
            };
            if !in_this_half {
                continue;
            }
            let expect_path = spec_path(chunk_index, &leaves);
            assert_chunk_proof(&tree, &leaves, chunk_index, &expect_path, expect_root);
            checked += 1;
        }
        let expect_checked = match half {
            ChunkIndexHalf::Even => even_index_count,
            ChunkIndexHalf::Odd => odd_index_count,
        };
        assert_eq!(checked, expect_checked, "C={chunk_count}");
    }
}

/// Counts checked by both proof tests: 1..=300, then the large powers.
fn proof_chunk_counts() -> Vec<u32> {
    let mut counts: Vec<u32> = (1..=PROOF_EVERY_CHUNK_MAX_CHUNK_COUNT).collect();
    let large = large_proof_chunk_counts();
    assert!(
        large
            .iter()
            .all(|chunk_count| *chunk_count > PROOF_EVERY_CHUNK_MAX_CHUNK_COUNT),
        "large proof counts must sit past the every-chunk range"
    );
    counts.extend(large);
    counts
}

/// Powers of two from the first one above [`PROOF_EVERY_CHUNK_MAX_CHUNK_COUNT`]
/// through the greatest power of two below [`SHAPE_LOCK_MAX_CHUNK_COUNT`],
/// each with the counts on either side.
///
/// The last neighbor is the shape-lock maximum: 4,096 + 1 = 4,097.
fn large_proof_chunk_counts() -> Vec<u32> {
    let mut counts = Vec::new();
    let mut power_of_two = PROOF_EVERY_CHUNK_MAX_CHUNK_COUNT.next_power_of_two();
    while power_of_two < SHAPE_LOCK_MAX_CHUNK_COUNT {
        counts.push(power_of_two - 1);
        counts.push(power_of_two);
        counts.push(power_of_two + 1);
        let Some(next_power_of_two) = power_of_two.checked_mul(2) else {
            break;
        };
        power_of_two = next_power_of_two;
    }
    assert_eq!(
        counts.last().copied(),
        Some(SHAPE_LOCK_MAX_CHUNK_COUNT),
        "the large proof set must include the shape-lock maximum"
    );
    counts
}

/// One new tree of `chunk_count` full chunks. Nothing is reused from another count.
///
/// Returns the chunk leaves `L_c = chunk_leaf(c, R_c)` of the pushed chunk
/// roots, the leaves of `build_layer`, `T` and `path` (spec §4).
fn tree_for_chunk_count(chunk_count: u32) -> (Vec<NodeHash>, DataTree) {
    let geometry = Geometry::TESTING;
    let chunk_size = geometry.chunk_size();
    let mut builder = DataTreeBuilder::new(geometry);
    let mut leaves = Vec::new();
    for chunk_index in 0..chunk_count {
        let chunk_root = chunk_root_for_index(chunk_index);
        builder
            .push_chunk_root(chunk_root, chunk_size)
            .expect("a full testing chunk is accepted");
        leaves.push(chunk_leaf(chunk_index, &chunk_root));
    }
    let tree = builder.finish().expect("a non-empty builder finishes");
    (leaves, tree)
}

/// Distinct chunk root. The first four bytes are the chunk index.
fn chunk_root_for_index(chunk_index: u32) -> NodeHash {
    let mut bytes = [0_u8; HASH_SIZE];
    bytes[..4].copy_from_slice(&chunk_index.to_be_bytes());
    NodeHash::from(bytes)
}

fn concatenated_path(path: &[NodeHash]) -> Vec<u8> {
    let mut out = Vec::with_capacity(path.len() * HASH_SIZE);
    for hash in path {
        out.extend_from_slice(hash.as_bytes());
    }
    out
}

fn assert_chunk_proof(
    tree: &DataTree,
    leaves: &[NodeHash],
    chunk_index: u32,
    expect_path: &[NodeHash],
    expect_root: NodeHash,
) {
    let chunk_count = u32::try_from(leaves.len()).expect("chunk count fits u32");
    let proof = tree
        .chunk_proof(chunk_index)
        .expect("chunk index is inside this tree");
    let mut expect_proof = concatenated_path(expect_path);
    expect_proof.extend_from_slice(size_witness(&Geometry::TESTING, tree.data_size()).as_bytes());
    assert_eq!(
        proof, expect_proof,
        "path ‖ size_witness C={chunk_count} c={chunk_index}"
    );
    let hash_count = depth(chunk_index, chunk_count).expect("chunk_index is below chunk_count") + 1;
    assert_eq!(
        proof.len(),
        usize::try_from(hash_count).expect("depth fits usize") * HASH_SIZE,
        "depth + witness C={chunk_count} c={chunk_index}"
    );
    let siblings = &proof[..proof.len() - HASH_SIZE];
    let leaf_index = usize::try_from(chunk_index).expect("chunk index fits usize");
    assert_eq!(
        climb(leaves[leaf_index], chunk_index, chunk_count, siblings),
        Some(expect_root),
        "climb C={chunk_count} c={chunk_index}"
    );
}

/// `build_layer`: pair left to right, carry an odd tail unchanged (spec §4).
fn build_layer(leaves: &[NodeHash]) -> Vec<Vec<NodeHash>> {
    let mut layers = vec![leaves.to_vec()];
    while layers.last().is_some_and(|layer| layer.len() > 1) {
        let previous = layers.last().expect("the loop condition saw a layer");
        let mut next = Vec::with_capacity(previous.len().div_ceil(2));
        let mut index = 0;
        while index + 1 < previous.len() {
            next.push(node(&previous[index], &previous[index + 1]));
            index += 2;
        }
        if index < previous.len() {
            next.push(previous[index]);
        }
        layers.push(next);
    }
    layers
}

/// Recursive `T` (spec §4).
fn spec_t(nodes: &[NodeHash]) -> NodeHash {
    if nodes.len() == 1 {
        return nodes[0];
    }
    let left_subtree_size = largest_power_of_two_below(nodes.len());
    node(
        &spec_t(&nodes[..left_subtree_size]),
        &spec_t(&nodes[left_subtree_size..]),
    )
}

/// Recursive `path(c, nodes)` (spec §4). Leaf to root.
fn spec_path(chunk_index: u32, nodes: &[NodeHash]) -> Vec<NodeHash> {
    if nodes.len() == 1 {
        return Vec::new();
    }
    let left_subtree_size = largest_power_of_two_below(nodes.len());
    let chunk_index = usize::try_from(chunk_index).expect("chunk index fits usize");
    if chunk_index < left_subtree_size {
        let mut path = spec_path(
            u32::try_from(chunk_index).expect("chunk index fits u32"),
            &nodes[..left_subtree_size],
        );
        path.push(spec_t(&nodes[left_subtree_size..]));
        path
    } else {
        let right_index = chunk_index - left_subtree_size;
        let mut path = spec_path(
            u32::try_from(right_index).expect("chunk index fits u32"),
            &nodes[left_subtree_size..],
        );
        path.push(spec_t(&nodes[..left_subtree_size]));
        path
    }
}

fn largest_power_of_two_below(chunk_count: usize) -> usize {
    // spec §4: "the largest power of two < C". This loop is independent of
    // `shape::largest_power_of_two_below`, so a wrong production formula cannot
    // pass by agreeing with itself.
    let mut power_of_two = 1;
    while power_of_two * 2 < chunk_count {
        power_of_two *= 2;
    }
    power_of_two
}
