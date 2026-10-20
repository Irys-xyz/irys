use std::collections::HashMap;

use crate::DataTreeBuilder;
use crate::geometry::Geometry;
use crate::hash::node;
use crate::shape::{climb, depth};
use crate::types::NodeHash;

/// Builder cache and proofs against the recursive `T` and `path` (spec §4, §12).
///
/// Every `C` in `1..=4097` checks layers, the recursive root, and the chunk
/// count. Every chunk proof is checked for `C` in `1..=300`. It is also checked
/// for each power of two in `512..=4096`, for that value ±1, and for `4097`.
/// A proof for every chunk of every `C` does not finish before the test timeout.
#[test]
fn builder_matches_spec_shape_for_every_chunk_count() {
    let geometry = Geometry::TESTING;
    let chunk_size = geometry.chunk_size();
    let mut builder = DataTreeBuilder::new(geometry);
    let mut leaves = Vec::<NodeHash>::with_capacity(4097);
    // One entry per contiguous leaf range. Rebuilding every prefix from scratch
    // repeats those hashes (opt-level 0) and does not finish in the test budget.
    let mut layer_cache = HashMap::<(usize, usize), NodeHash>::with_capacity(16_384);
    let mut root_cache = HashMap::<(usize, usize), NodeHash>::with_capacity(16_384);
    for chunk_count in 1_u32..=4097 {
        let leaf = synthetic(chunk_count - 1);
        leaves.push(leaf);
        builder.push_chunk_root(leaf, chunk_size).unwrap();
        let tree = builder.clone().finish().unwrap();
        let expect_layers = spec_layers(&leaves, &mut layer_cache);
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
        let expect_root = spec_t_cached(&leaves, &mut root_cache);
        assert_eq!(
            tree.data_tree_root().as_bytes(),
            expect_root.as_bytes(),
            "root C={chunk_count}"
        );
        if checks_every_chunk_proof(chunk_count) {
            // A cold cache must match the reused one, and `path` must match `T`.
            let mut fresh_layers = HashMap::new();
            let mut fresh_roots = HashMap::new();
            assert_eq!(
                spec_layers(&leaves, &mut fresh_layers),
                expect_layers,
                "fresh layers C={chunk_count}"
            );
            let (path_root, expect_paths) = spec_paths(&leaves);
            assert_eq!(
                spec_t_cached(&leaves, &mut fresh_roots),
                path_root,
                "fresh root C={chunk_count}"
            );
            assert_eq!(expect_root, path_root, "cached root C={chunk_count}");
            for (chunk_index, expect_path) in expect_paths.iter().enumerate() {
                let chunk_index = u32::try_from(chunk_index).unwrap();
                let got = tree.chunk_proof(chunk_index).unwrap();
                let flat = flatten(expect_path);
                assert_eq!(got, flat, "path C={chunk_count} c={chunk_index}");
                assert_eq!(
                    got.len(),
                    usize::try_from(depth(chunk_index, chunk_count)).unwrap() * 32,
                    "depth C={chunk_count} c={chunk_index}"
                );
                assert_eq!(
                    climb(
                        leaves[usize::try_from(chunk_index).unwrap()],
                        chunk_index,
                        chunk_count,
                        &got
                    ),
                    Some(path_root),
                    "climb C={chunk_count} c={chunk_index}"
                );
            }
        }
    }
}

/// `1..=300`, each power of two in `512..=4096` and that value ±1, plus `4097`.
fn checks_every_chunk_proof(chunk_count: u32) -> bool {
    if chunk_count <= 300 {
        return true;
    }
    const BOUNDARY: [u32; 12] = [
        511, 512, 513, 1023, 1024, 1025, 2047, 2048, 2049, 4095, 4096, 4097,
    ];
    BOUNDARY.binary_search(&chunk_count).is_ok()
}

fn synthetic(chunk_index: u32) -> NodeHash {
    let mut bytes = [0_u8; 32];
    bytes[..4].copy_from_slice(&chunk_index.to_be_bytes());
    NodeHash(bytes)
}

fn flatten(path: &[NodeHash]) -> Vec<u8> {
    let mut out = Vec::with_capacity(path.len() * 32);
    for hash in path {
        out.extend_from_slice(hash.as_bytes());
    }
    out
}

/// `build_layer`: pair left to right, carry an odd tail unchanged (spec §4).
///
/// `cache` stores the hash of a leaf range. A later prefix reuses it.
fn spec_layers(
    leaves: &[NodeHash],
    cache: &mut HashMap<(usize, usize), NodeHash>,
) -> Vec<Vec<NodeHash>> {
    let mut layers = vec![leaves.to_vec()];
    let mut ranges: Vec<(usize, usize)> = (0..leaves.len()).map(|i| (i, i + 1)).collect();
    while layers.last().is_some_and(|layer| layer.len() > 1) {
        let prev = layers.last().expect("the loop condition saw a layer");
        let mut next = Vec::with_capacity(prev.len().div_ceil(2));
        let mut next_ranges = Vec::with_capacity(prev.len().div_ceil(2));
        let mut index = 0;
        while index + 1 < prev.len() {
            let start = ranges[index].0;
            let end = ranges[index + 1].1;
            next.push(cached_node(
                cache,
                start,
                end,
                &prev[index],
                &prev[index + 1],
            ));
            next_ranges.push((start, end));
            index += 2;
        }
        if index < prev.len() {
            next.push(prev[index]);
            next_ranges.push(ranges[index]);
        }
        layers.push(next);
        ranges = next_ranges;
    }
    layers
}

fn cached_node(
    cache: &mut HashMap<(usize, usize), NodeHash>,
    start: usize,
    end: usize,
    left: &NodeHash,
    right: &NodeHash,
) -> NodeHash {
    if let Some(hit) = cache.get(&(start, end)) {
        return *hit;
    }
    let value = node(left, right);
    cache.insert((start, end), value);
    value
}

/// Recursive `T` (spec §4). `cache` stores each leaf range once.
fn spec_t_cached(nodes: &[NodeHash], cache: &mut HashMap<(usize, usize), NodeHash>) -> NodeHash {
    spec_t_range(nodes, 0, nodes.len(), cache)
}

fn spec_t_range(
    nodes: &[NodeHash],
    start: usize,
    end: usize,
    cache: &mut HashMap<(usize, usize), NodeHash>,
) -> NodeHash {
    if let Some(hit) = cache.get(&(start, end)) {
        return *hit;
    }
    let value = if end - start == 1 {
        nodes[start]
    } else {
        let left_subtree_size = largest_power_of_two_below(end - start);
        let left = spec_t_range(nodes, start, start + left_subtree_size, cache);
        let right = spec_t_range(nodes, start + left_subtree_size, end, cache);
        node(&left, &right)
    };
    cache.insert((start, end), value);
    value
}

/// Recursive `T` and `path` (spec §4). One walk fills every leaf's audit path.
fn spec_paths(nodes: &[NodeHash]) -> (NodeHash, Vec<Vec<NodeHash>>) {
    if nodes.len() == 1 {
        return (nodes[0], vec![Vec::new()]);
    }
    let left_subtree_size = largest_power_of_two_below(nodes.len());
    let (left_root, left_paths) = spec_paths(&nodes[..left_subtree_size]);
    let (right_root, right_paths) = spec_paths(&nodes[left_subtree_size..]);
    let root = node(&left_root, &right_root);
    let mut paths = Vec::with_capacity(nodes.len());
    for mut path in left_paths {
        path.push(right_root);
        paths.push(path);
    }
    for mut path in right_paths {
        path.push(left_root);
        paths.push(path);
    }
    (root, paths)
}

fn largest_power_of_two_below(chunk_count: usize) -> usize {
    1_usize << (usize::BITS - 1 - (chunk_count - 1).leading_zeros())
}
