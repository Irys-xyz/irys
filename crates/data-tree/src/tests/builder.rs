use super::pattern;
use crate::chunk::chunk_root;
use crate::error::{BuildError, ChunkLengthError};
use crate::geometry::Geometry;
use crate::types::{HASH_SIZE, NodeHash};
use crate::{DataTree, DataTreeBuilder};

#[test]
fn finish_without_chunks_fails() {
    let err = DataTreeBuilder::new(Geometry::TESTING)
        .finish()
        .unwrap_err();
    assert_eq!(err, BuildError::Empty);
    assert_eq!(
        DataTree::from_bytes(Geometry::TESTING, b"").unwrap_err(),
        BuildError::Empty
    );
}

#[test]
fn push_rejects_empty_long_and_too_many() {
    let geometry = Geometry::new(
        Geometry::TESTING.chunk_size(),
        Geometry::TESTING.segments_per_chunk(),
        2,
    )
    .unwrap();
    let mut builder = DataTreeBuilder::new(geometry);
    assert_eq!(
        builder.push_chunk_root(synthetic(0), 0).unwrap_err(),
        BuildError::ChunkLength(ChunkLengthError::Empty)
    );
    assert_eq!(
        builder.push_chunk(b"").unwrap_err(),
        BuildError::ChunkLength(ChunkLengthError::Empty)
    );
    assert_eq!(
        builder
            .push_chunk_root(synthetic(0), geometry.chunk_size() + 1)
            .unwrap_err(),
        BuildError::ChunkLength(ChunkLengthError::TooLong {
            chunk_length: geometry.chunk_size() + 1,
            chunk_size: geometry.chunk_size()
        })
    );
    builder
        .push_chunk_root(synthetic(0), geometry.chunk_size())
        .unwrap();
    builder
        .push_chunk_root(synthetic(1), geometry.chunk_size())
        .unwrap();
    assert_eq!(
        builder
            .push_chunk_root(synthetic(2), geometry.chunk_size())
            .unwrap_err(),
        BuildError::TooManyChunks {
            max_data_tx_chunks: 2
        }
    );
}

#[test]
fn push_after_a_short_chunk_fails() {
    let mut builder = DataTreeBuilder::new(Geometry::TESTING);
    let short_chunk = Geometry::TESTING.chunk_size() / 2;
    builder.push_chunk_root(synthetic(0), short_chunk).unwrap();
    assert_eq!(
        builder
            .push_chunk_root(synthetic(1), Geometry::TESTING.chunk_size())
            .unwrap_err(),
        BuildError::AfterShortChunk
    );
    assert_eq!(
        builder
            .push_chunk(&[0; Geometry::TESTING.chunk_size() as usize])
            .unwrap_err(),
        BuildError::AfterShortChunk
    );
    let tree = builder.finish().unwrap();
    assert_eq!(tree.chunk_count(), 1);
    assert_eq!(tree.data_size(), short_chunk);
}

#[test]
fn push_chunk_matches_push_chunk_root() {
    let geometry = Geometry::TESTING;
    let bytes = pattern(usize::try_from(geometry.chunk_size()).unwrap());
    let mut from_bytes = DataTreeBuilder::new(geometry);
    from_bytes.push_chunk(&bytes).unwrap();
    let mut from_root = DataTreeBuilder::new(geometry);
    from_root
        .push_chunk_root(
            chunk_root(&geometry, &bytes).unwrap(),
            geometry.chunk_size(),
        )
        .unwrap();
    assert_eq!(
        from_bytes.finish().unwrap().data_tree_root(),
        from_root.finish().unwrap().data_tree_root()
    );
}

fn synthetic(chunk_index: u32) -> NodeHash {
    let mut bytes = [0_u8; HASH_SIZE];
    let chunk_index_bytes = chunk_index.to_be_bytes();
    bytes[..chunk_index_bytes.len()].copy_from_slice(&chunk_index_bytes);
    NodeHash(bytes)
}

/// Final length of each cache layer (spec §4 `build_layer`).
///
/// Layer 0 is the chunk roots. Each next layer is `ceil(previous / 2)`, which
/// counts the carried unpaired node. Stop at one node. This is the spec rule,
/// not the builder's cache.
fn spec_layer_lengths(chunk_count: u32) -> Vec<usize> {
    let mut layer_length = usize::try_from(chunk_count).expect("chunk count fits usize");
    let mut lengths = vec![layer_length];
    while layer_length > 1 {
        layer_length = layer_length.div_ceil(2);
        lengths.push(layer_length);
    }
    lengths
}

fn push_bytes(builder: &mut DataTreeBuilder, geometry: Geometry, bytes: &[u8]) {
    let chunk_size = usize::try_from(geometry.chunk_size()).expect("chunk size fits usize");
    for piece in bytes.chunks(chunk_size) {
        builder.push_chunk(piece).unwrap();
    }
}

fn assert_same_tree(left: &DataTree, right: &DataTree) {
    assert_eq!(left.data_size(), right.data_size());
    assert_eq!(left.chunk_count(), right.chunk_count());
    assert_eq!(left.layers(), right.layers());
    assert_eq!(left.data_tree_root(), right.data_tree_root());
    assert_eq!(left.size_witness(), right.size_witness());
    assert_eq!(left.data_commitment(), right.data_commitment());
    for chunk_index in 0..left.chunk_count() {
        assert_eq!(
            left.chunk_proof(chunk_index).unwrap(),
            right.chunk_proof(chunk_index).unwrap(),
            "chunk {chunk_index}"
        );
    }
}

#[test]
fn with_capacity_rejects_the_same_sizes_as_chunk_count() {
    // Catches a capacity hint that accepts 0 or a size above the geometry max,
    // or that returns a different `SizeError` than `Geometry::chunk_count`.
    let geometry = Geometry::TESTING;
    let max_data_size = geometry.max_data_size();
    for data_size in [0, max_data_size + 1] {
        assert_eq!(
            DataTreeBuilder::with_capacity(geometry, data_size).unwrap_err(),
            geometry.chunk_count(data_size).unwrap_err()
        );
    }
    assert!(DataTreeBuilder::with_capacity(geometry, max_data_size).is_ok());
}

#[test]
fn with_capacity_reserves_the_final_layer_lengths() {
    // Catches a reservation that is short (the layer later grows) or long
    // (spare capacity remains after the exact upload).
    // Spec §4: `C = 5` is `node(node(node(R0, R1), node(R2, R3)), R4)`.
    assert_eq!(spec_layer_lengths(1), vec![1]);
    assert_eq!(spec_layer_lengths(5), vec![5, 3, 2, 1]);

    let geometry = Geometry::TESTING;
    // Same exhaustive range as the proof roundtrip (spec §13).
    for data_size in 1_u64..=199 {
        let bytes = pattern(usize::try_from(data_size).unwrap());
        let mut builder = DataTreeBuilder::with_capacity(geometry, data_size).unwrap();
        push_bytes(&mut builder, geometry, &bytes);
        let tree = builder.finish().unwrap();
        let expected = spec_layer_lengths(geometry.chunk_count(data_size).unwrap());
        assert_eq!(tree.layers().len(), expected.len(), "n={data_size}");
        for (layer, expected_length) in tree.layers().iter().zip(expected) {
            assert_eq!(layer.len(), expected_length, "n={data_size}");
            assert_eq!(
                layer.capacity(),
                expected_length,
                "n={data_size} grew or reserved spare slots"
            );
        }
    }
}

#[test]
fn with_capacity_matches_new_for_the_same_bytes() {
    // Catches a capacity hint that changes layers, the root, or a proof.
    let geometry = Geometry::TESTING;
    for data_size in 1_u64..=199 {
        let bytes = pattern(usize::try_from(data_size).unwrap());
        let mut reserved = DataTreeBuilder::with_capacity(geometry, data_size).unwrap();
        push_bytes(&mut reserved, geometry, &bytes);
        let mut plain = DataTreeBuilder::new(geometry);
        push_bytes(&mut plain, geometry, &bytes);
        assert_same_tree(&reserved.finish().unwrap(), &plain.finish().unwrap());
    }
}

#[test]
fn with_capacity_hint_does_not_change_the_tree() {
    // Catches a hint that rejects a push above or below the reserved size,
    // or that builds a different tree for that push.
    let geometry = Geometry::TESTING;
    let chunk_size = geometry.chunk_size();
    let actual_size = chunk_size * 3 + chunk_size / 2;
    let bytes = pattern(usize::try_from(actual_size).unwrap());
    let mut plain = DataTreeBuilder::new(geometry);
    push_bytes(&mut plain, geometry, &bytes);
    let plain = plain.finish().unwrap();

    let mut low_hint = DataTreeBuilder::with_capacity(geometry, chunk_size).unwrap();
    push_bytes(&mut low_hint, geometry, &bytes);
    assert_same_tree(&plain, &low_hint.finish().unwrap());

    let mut high_hint = DataTreeBuilder::with_capacity(geometry, chunk_size * 6).unwrap();
    push_bytes(&mut high_hint, geometry, &bytes);
    assert_same_tree(&plain, &high_hint.finish().unwrap());
}

#[cfg(feature = "parallel")]
fn push_varied_parallel_batches(builder: &mut DataTreeBuilder, geometry: Geometry, bytes: &[u8]) {
    let chunk_size = usize::try_from(geometry.chunk_size()).expect("chunk size fits usize");
    let mut offset = 0_usize;
    // Whole-chunk batches of 1, then 2, then 3, and so on. A short tail is its
    // own last batch: a short chunk ends the upload (spec §4).
    let mut batch_chunk_count = 1_usize;
    while offset < bytes.len() {
        let remaining = bytes.len() - offset;
        let whole_chunks = remaining / chunk_size;
        let take = if whole_chunks == 0 {
            remaining
        } else {
            batch_chunk_count.min(whole_chunks) * chunk_size
        };
        builder
            .push_chunks_parallel(&bytes[offset..offset + take])
            .unwrap();
        offset += take;
        batch_chunk_count += 1;
    }
}

#[cfg(feature = "parallel")]
fn assert_parallel_matches_sequential(geometry: Geometry, data_size: u64) {
    let bytes = pattern(usize::try_from(data_size).expect("data size fits usize"));
    let mut sequential = DataTreeBuilder::new(geometry);
    push_bytes(&mut sequential, geometry, &bytes);
    let sequential = sequential.finish().unwrap();

    let mut one_batch = DataTreeBuilder::new(geometry);
    one_batch.push_chunks_parallel(&bytes).unwrap();
    assert_same_tree(&sequential, &one_batch.finish().unwrap());

    let mut varied = DataTreeBuilder::new(geometry);
    push_varied_parallel_batches(&mut varied, geometry, &bytes);
    assert_same_tree(&sequential, &varied.finish().unwrap());

    assert_same_tree(
        &sequential,
        &DataTree::from_bytes(geometry, &bytes).unwrap(),
    );
}

#[cfg(feature = "parallel")]
#[test]
fn parallel_batches_match_sequential_pushes_on_testing_geometry() {
    // Catches a split, an index order, or a `from_bytes` path that differs
    // from `push_chunk`. Chunk roots are independent (spec §9) and the tree
    // must stay deterministic (spec §10). The range is spec §13.
    let geometry = Geometry::TESTING;
    for data_size in 1_u64..=199 {
        assert_parallel_matches_sequential(geometry, data_size);
    }
}

#[cfg(feature = "parallel")]
#[test]
fn parallel_batches_match_sequential_pushes_on_mainnet_geometry() {
    // Spec §13 mainnet rows: one byte, one chunk, one chunk plus one byte,
    // and five chunks.
    let geometry = Geometry::MAINNET;
    let chunk_size = geometry.chunk_size();
    for data_size in [1, chunk_size, chunk_size + 1, chunk_size * 5] {
        assert_parallel_matches_sequential(geometry, data_size);
    }
}

#[cfg(feature = "parallel")]
#[test]
fn parallel_batch_errors_leave_the_builder_unchanged() {
    // Catches a batch that appends before the whole batch is valid.
    let geometry = Geometry::TESTING;
    let chunk_size = usize::try_from(geometry.chunk_size()).expect("chunk size fits usize");
    let one_chunk = pattern(chunk_size);

    let mut builder = DataTreeBuilder::new(geometry);
    assert_eq!(
        builder.push_chunks_parallel(b"").unwrap_err(),
        BuildError::ChunkLength(ChunkLengthError::Empty)
    );
    assert_eq!(builder.finish().unwrap_err(), BuildError::Empty);

    let mut builder = DataTreeBuilder::new(geometry);
    builder.push_chunk(&one_chunk).unwrap();
    let before = builder.clone().finish().unwrap();
    assert_eq!(
        builder.push_chunks_parallel(b"").unwrap_err(),
        BuildError::ChunkLength(ChunkLengthError::Empty)
    );
    assert_same_tree(&before, &builder.finish().unwrap());

    let short_chunk = pattern(chunk_size / 2);
    let mut builder = DataTreeBuilder::new(geometry);
    builder.push_chunk(&short_chunk).unwrap();
    let before = builder.clone().finish().unwrap();
    assert_eq!(
        builder.push_chunks_parallel(&one_chunk).unwrap_err(),
        BuildError::AfterShortChunk
    );
    assert_same_tree(&before, &builder.finish().unwrap());

    let limited = Geometry::new(geometry.chunk_size(), geometry.segments_per_chunk(), 2).unwrap();
    let two_more_chunks = pattern(chunk_size * 2);
    let mut builder = DataTreeBuilder::new(limited);
    builder.push_chunk(&one_chunk).unwrap();
    let before = builder.clone().finish().unwrap();
    assert_eq!(
        builder.push_chunks_parallel(&two_more_chunks).unwrap_err(),
        BuildError::TooManyChunks {
            max_data_tx_chunks: 2
        }
    );
    assert_same_tree(&before, &builder.clone().finish().unwrap());
    builder.push_chunk(&one_chunk).unwrap();
    assert_eq!(builder.finish().unwrap().chunk_count(), 2);

    let three_chunks = pattern(chunk_size * 3);
    let mut builder = DataTreeBuilder::new(limited);
    assert_eq!(
        builder.push_chunks_parallel(&three_chunks).unwrap_err(),
        BuildError::TooManyChunks {
            max_data_tx_chunks: 2
        }
    );
    assert_eq!(builder.finish().unwrap_err(), BuildError::Empty);
}
