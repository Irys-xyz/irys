//! Push sequences do not panic, and a rejected push leaves the accepted tree
//! (spec §4, §10).
//!
//! The finished tree matches plain sequential `push_chunk` of the bytes those
//! pushes accepted. See `strategies` for the `PROPTEST_CASES` soak command.

use proptest::prelude::*;

use super::strategies;
use crate::chunk::chunk_root;
use crate::error::BuildError;
use crate::geometry::{Geometry, MAX_SEGMENTS_PER_CHUNK};
use crate::types::{DataCommitment, HASH_SIZE, NodeHash};
use crate::{DataTree, DataTreeBuilder};

/// Proptest's usual count. Each sequence hashes a few hundred bytes, so this
/// stays under 10 s. `PROPTEST_CASES` replaces it (see `strategies`).
const BUILDER_CASES: u32 = 256;

/// How many pushes one sequence draws, including the empty sequence.
const BUILDER_PUSH_COUNT: usize = 6;

/// Largest builder chunk. Derived from [`MAX_SEGMENTS_PER_CHUNK`] so one-byte
/// segments at that width still fit, and a push stays a few hundred bytes.
fn builder_max_chunk_size() -> u64 {
    u64::from(MAX_SEGMENTS_PER_CHUNK)
}

proptest! {
    #![proptest_config(strategies::case_config(BUILDER_CASES))]

    #[test]
    fn push_sequences_match_the_accepted_chunks(
        (geometry, pushes) in push_sequences()
    ) {
        check_push_sequence(geometry, &pushes);
    }
}

#[derive(Debug, Clone)]
enum GeneratedPush {
    /// `push_chunk` of these bytes.
    Chunk(Vec<u8>),
    /// `push_chunk_root`. A legal length uses `chunk_root` of these bytes.
    ChunkRoot(Vec<u8>),
    /// `push_chunks_parallel` of this batch. Absent when `parallel` is off.
    #[cfg(feature = "parallel")]
    Parallel(Vec<u8>),
}

fn check_push_sequence(geometry: Geometry, pushes: &[GeneratedPush]) {
    let mut builder = DataTreeBuilder::new(geometry);
    let mut accepted: Vec<Vec<u8>> = Vec::new();
    for push in pushes {
        match apply_push(&mut builder, geometry, push) {
            Ok(chunks) => accepted.extend(chunks),
            Err(()) => {
                let after = tree_signature(builder.clone().finish());
                let expected = tree_signature(sequential_tree(geometry, &accepted));
                assert_eq!(
                    after, expected,
                    "a rejected push changed the tree of the accepted chunks"
                );
            }
        }
    }
    let finished = tree_signature(builder.finish());
    let expected = tree_signature(sequential_tree(geometry, &accepted));
    assert_eq!(
        finished, expected,
        "accepted pushes differ from sequential push_chunk"
    );
}

fn apply_push(
    builder: &mut DataTreeBuilder,
    geometry: Geometry,
    push: &GeneratedPush,
) -> Result<Vec<Vec<u8>>, ()> {
    match push {
        GeneratedPush::Chunk(bytes) => match builder.push_chunk(bytes) {
            Ok(()) => Ok(vec![bytes.clone()]),
            Err(_) => Err(()),
        },
        GeneratedPush::ChunkRoot(bytes) => apply_chunk_root(builder, geometry, bytes),
        #[cfg(feature = "parallel")]
        GeneratedPush::Parallel(bytes) => apply_parallel(builder, geometry, bytes),
    }
}

fn apply_chunk_root(
    builder: &mut DataTreeBuilder,
    geometry: Geometry,
    bytes: &[u8],
) -> Result<Vec<Vec<u8>>, ()> {
    let chunk_length = u64::try_from(bytes.len()).expect("push length fits u64");
    match chunk_root(&geometry, bytes) {
        Ok(root) => match builder.push_chunk_root(root, chunk_length) {
            Ok(()) => Ok(vec![bytes.to_vec()]),
            Err(_) => Err(()),
        },
        Err(_) => {
            // The length is illegal. `push_chunk_root` rejects it before it
            // uses the root (spec §4, §10). The root value must not matter.
            let ignored = NodeHash([0_u8; HASH_SIZE]);
            match builder.push_chunk_root(ignored, chunk_length) {
                Ok(()) => panic!("push_chunk_root accepted a length chunk_root rejected"),
                Err(_) => Err(()),
            }
        }
    }
}

#[cfg(feature = "parallel")]
fn apply_parallel(
    builder: &mut DataTreeBuilder,
    geometry: Geometry,
    bytes: &[u8],
) -> Result<Vec<Vec<u8>>, ()> {
    match builder.push_chunks_parallel(bytes) {
        Ok(()) => {
            assert!(
                !bytes.is_empty(),
                "push_chunks_parallel accepted an empty batch"
            );
            Ok(split_chunks(geometry, bytes))
        }
        Err(_) => Err(()),
    }
}

#[cfg(feature = "parallel")]
fn split_chunks(geometry: Geometry, bytes: &[u8]) -> Vec<Vec<u8>> {
    let chunk_size = usize::try_from(geometry.chunk_size()).expect("builder chunk_size fits usize");
    bytes.chunks(chunk_size).map(<[u8]>::to_vec).collect()
}

fn sequential_tree(geometry: Geometry, chunks: &[Vec<u8>]) -> Result<DataTree, BuildError> {
    let mut builder = DataTreeBuilder::new(geometry);
    for chunk in chunks {
        builder
            .push_chunk(chunk)
            .expect("a chunk the sequence accepted");
    }
    builder.finish()
}

fn tree_signature(
    result: Result<DataTree, BuildError>,
) -> Result<(u64, DataCommitment, Vec<NodeHash>), BuildError> {
    result.map(|tree| {
        let chunk_roots = (0..tree.chunk_count())
            .map(|chunk_index| {
                tree.chunk_root(chunk_index)
                    .expect("chunk index is below chunk_count")
            })
            .collect();
        (tree.data_size(), tree.data_commitment(), chunk_roots)
    })
}

fn push_sequences() -> impl Strategy<Value = (Geometry, Vec<GeneratedPush>)> {
    builder_geometry().prop_flat_map(|geometry| {
        let chunk_size = geometry.chunk_size();
        prop::collection::vec(generated_push(chunk_size), 0..=BUILDER_PUSH_COUNT)
            .prop_map(move |pushes| (geometry, pushes))
    })
}

fn generated_push(chunk_size: u64) -> impl Strategy<Value = GeneratedPush> {
    let kind_count = if cfg!(feature = "parallel") {
        3_u8
    } else {
        2_u8
    };
    (
        0..kind_count,
        strategies::edge_biased_push_length(chunk_size),
    )
        .prop_flat_map(|(kind, length)| {
            prop::collection::vec(any::<u8>(), length).prop_map(move |bytes| match kind {
                0 => GeneratedPush::Chunk(bytes),
                1 => GeneratedPush::ChunkRoot(bytes),
                #[cfg(feature = "parallel")]
                2 => GeneratedPush::Parallel(bytes),
                _ => unreachable!("kind is drawn from 0..kind_count"),
            })
        })
}

fn builder_geometry() -> impl Strategy<Value = Geometry> {
    builder_segments_per_chunk().prop_flat_map(|segments_per_chunk| {
        let segment_count = u64::from(segments_per_chunk);
        let max_segment_size = builder_max_chunk_size() / segment_count;
        (builder_max_chunks(), 1_u64..=max_segment_size).prop_map(
            move |(max_data_tx_chunks, segment_size)| {
                Geometry::new(
                    segment_count * segment_size,
                    segments_per_chunk,
                    max_data_tx_chunks,
                )
                .expect("builder geometry fits the chunk cap")
            },
        )
    })
}

fn builder_segments_per_chunk() -> impl Strategy<Value = u32> {
    let max_shift = builder_max_chunk_size().trailing_zeros();
    prop_oneof![
        3 => Just(1_u32),
        2 => Just(1_u32 << max_shift),
        1 => (0..=max_shift).prop_map(|shift| 1_u32 << shift),
    ]
}

fn builder_max_chunks() -> impl Strategy<Value = u32> {
    let push_limit = u32::try_from(BUILDER_PUSH_COUNT).expect("push count fits u32");
    prop_oneof![
        3 => Just(1_u32),
        3 => Just(2_u32),
        2 => Just(push_limit),
        1 => Just(u32::MAX),
    ]
}
