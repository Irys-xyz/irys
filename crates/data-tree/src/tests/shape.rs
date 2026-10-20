use crate::DataTreeBuilder;
use crate::geometry::Geometry;
use crate::hash::node;
use crate::shape::{climb, depth};
use crate::types::{HASH_SIZE, NodeHash};

/// Builder layers and proofs against the recursive `T`, `path`, and `build_layer`
/// (spec §4, §12).
///
/// A full check (layers, `T`, every chunk's `path`, `depth`, and `climb`) runs
/// for every chunk count in `1..=128`, and for each power of two from 256
/// through 1024 together with that value ±1. `1..=128` is every small shape.
/// The later counts are where an unpaired node is carried, and the counts next
/// to those. 2048 and 4096 repeat that carry one and two doublings up; a full
/// `path` check there recomputes every sibling tree and does not fit the test
/// budget (spec §12 names 4097 as the shape-lock top; the builder's pairing is
/// the same recurrence, and 1024 already passes a deeper tree than `1..=128`).
///
/// The oracle is the plain recursive spec. It does not cache ranges. The counts
/// between those powers are omitted: they add time without a new carry pattern.
#[test]
fn builder_matches_spec_shape_for_every_chunk_count() {
    let geometry = Geometry::TESTING;
    let chunk_size = geometry.chunk_size();
    for chunk_count in full_check_counts() {
        let leaves: Vec<NodeHash> = (0..chunk_count).map(synthetic).collect();
        let mut builder = DataTreeBuilder::new(geometry);
        for leaf in &leaves {
            builder.push_chunk_root(*leaf, chunk_size).unwrap();
        }
        let tree = builder.finish().unwrap();
        let expect_layers = build_layer(&leaves);
        assert_eq!(
            tree.layers().len(),
            expect_layers.len(),
            "layers C={chunk_count}"
        );
        for (level, (got, expect)) in tree.layers().iter().zip(&expect_layers).enumerate() {
            assert_eq!(got, expect, "layer {level} C={chunk_count}");
        }
        assert_eq!(tree.chunk_count(), chunk_count);
        assert_eq!(tree.data_size(), u64::from(chunk_count) * chunk_size);
        let expect_root = spec_t(&leaves);
        assert_eq!(
            tree.data_tree_root().as_bytes(),
            expect_root.as_bytes(),
            "root C={chunk_count}"
        );
        for chunk_index in 0..chunk_count {
            let expect_path = spec_path(chunk_index, &leaves);
            let got = tree.chunk_proof(chunk_index).unwrap();
            assert_eq!(
                got,
                flatten(&expect_path),
                "path C={chunk_count} c={chunk_index}"
            );
            assert_eq!(
                got.len(),
                usize::try_from(depth(chunk_index, chunk_count)).unwrap() * HASH_SIZE,
                "depth C={chunk_count} c={chunk_index}"
            );
            let leaf_index = usize::try_from(chunk_index).unwrap();
            assert_eq!(
                climb(leaves[leaf_index], chunk_index, chunk_count, &got),
                Some(expect_root),
                "climb C={chunk_count} c={chunk_index}"
            );
        }
    }
}

/// `1..=128`, then each power of two from 256 through 1024 with its neighbors.
///
/// 2048 and 4096 are the same carry pattern one and two doublings up. A full
/// `path` check there recomputes every sibling tree from the leaves, and that
/// does not fit the test budget. 1024 already forces a subtree deeper than the
/// small range.
fn full_check_counts() -> impl Iterator<Item = u32> {
    (1_u32..=128).chain([255, 256, 257, 511, 512, 513, 1023, 1024, 1025])
}

fn synthetic(chunk_index: u32) -> NodeHash {
    let mut bytes = [0_u8; HASH_SIZE];
    bytes[..4].copy_from_slice(&chunk_index.to_be_bytes());
    NodeHash(bytes)
}

fn flatten(path: &[NodeHash]) -> Vec<u8> {
    let mut out = Vec::with_capacity(path.len() * HASH_SIZE);
    for hash in path {
        out.extend_from_slice(hash.as_bytes());
    }
    out
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
    // Same bit formula as `shape::largest_power_of_two_below`, on `usize`.
    1_usize << (usize::BITS - 1 - (chunk_count - 1).leading_zeros())
}
