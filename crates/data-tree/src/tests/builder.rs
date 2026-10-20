use super::pattern;
use crate::chunk::chunk_root;
use crate::error::{BuildError, ChunkLengthError};
use crate::geometry::Geometry;
use crate::types::NodeHash;
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
    let geometry = Geometry::new(32, 4, 2).unwrap();
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
        builder.push_chunk_root(synthetic(0), 33).unwrap_err(),
        BuildError::ChunkLength(ChunkLengthError::TooLong {
            chunk_length: 33,
            chunk_size: 32
        })
    );
    builder.push_chunk_root(synthetic(0), 32).unwrap();
    builder.push_chunk_root(synthetic(1), 32).unwrap();
    assert_eq!(
        builder.push_chunk_root(synthetic(2), 32).unwrap_err(),
        BuildError::TooManyChunks {
            max_data_tx_chunks: 2
        }
    );
}

#[test]
fn push_after_a_short_chunk_fails() {
    let mut builder = DataTreeBuilder::new(Geometry::TESTING);
    builder.push_chunk_root(synthetic(0), 16).unwrap();
    assert_eq!(
        builder.push_chunk_root(synthetic(1), 32).unwrap_err(),
        BuildError::AfterShortChunk
    );
    assert_eq!(
        builder.push_chunk(&[0; 32]).unwrap_err(),
        BuildError::AfterShortChunk
    );
    let tree = builder.finish().unwrap();
    assert_eq!(tree.chunk_count(), 1);
    assert_eq!(tree.data_size(), 16);
}

#[test]
fn push_chunk_matches_push_chunk_root() {
    let geometry = Geometry::TESTING;
    let bytes = pattern(32);
    let mut from_bytes = DataTreeBuilder::new(geometry);
    from_bytes.push_chunk(&bytes).unwrap();
    let mut from_root = DataTreeBuilder::new(geometry);
    from_root
        .push_chunk_root(chunk_root(&geometry, &bytes).unwrap(), 32)
        .unwrap();
    assert_eq!(
        from_bytes.finish().unwrap().data_tree_root(),
        from_root.finish().unwrap().data_tree_root()
    );
}

fn synthetic(chunk_index: u32) -> NodeHash {
    let mut bytes = [0_u8; 32];
    bytes[..4].copy_from_slice(&chunk_index.to_be_bytes());
    NodeHash(bytes)
}
