use crate::error::{GeometryError, SizeError};
use crate::geometry::Geometry;
use rstest::rstest;

#[test]
fn presets_match_new() {
    assert_eq!(
        Geometry::MAINNET,
        Geometry::new(262_144, 64, 20_971_520).unwrap()
    );
    assert_eq!(Geometry::TESTING, Geometry::new(32, 4, 20_971_520).unwrap());
    assert_eq!(Geometry::MAINNET.segment_size(), 4_096);
    assert_eq!(Geometry::MAINNET.height(), 6);
    assert_eq!(Geometry::TESTING.segment_size(), 8);
    assert_eq!(Geometry::TESTING.height(), 2);
    assert_eq!(Geometry::MAINNET.chunk_size(), 262_144);
    assert_eq!(Geometry::MAINNET.segments_per_chunk(), 64);
    assert_eq!(Geometry::MAINNET.max_data_tx_chunks(), 20_971_520);
    assert_eq!(Geometry::MAINNET.max_data_size(), 262_144 * 20_971_520);
}

#[test]
fn segments_per_chunk_of_one_is_allowed() {
    let geometry = Geometry::new(8, 1, 4).unwrap();
    assert_eq!(geometry.height(), 0);
    assert_eq!(geometry.segment_size(), 8);
}

#[rstest]
#[case(32, 0, 1, GeometryError::SegmentsPerChunk { segments_per_chunk: 0 })]
#[case(32, 3, 1, GeometryError::SegmentsPerChunk { segments_per_chunk: 3 })]
#[case(512, 512, 1, GeometryError::SegmentsPerChunk { segments_per_chunk: 512 })]
#[case(0, 1, 1, GeometryError::ChunkSizeZero)]
#[case(30, 4, 1, GeometryError::ChunkSizeNotMultiple { chunk_size: 30, segments_per_chunk: 4 })]
#[case(32, 4, 0, GeometryError::MaxChunksZero)]
#[case(1 << 63, 1, 3, GeometryError::MaxDataSizeOverflow)]
#[case(u64::MAX, 1, 2, GeometryError::MaxDataSizeOverflow)]
fn geometry_new_rejects(
    #[case] chunk_size: u64,
    #[case] segments_per_chunk: u32,
    #[case] max_chunks: u32,
    #[case] expect: GeometryError,
) {
    assert_eq!(
        Geometry::new(chunk_size, segments_per_chunk, max_chunks),
        Err(expect)
    );
}

#[test]
fn lengths_follow_the_spec() {
    let geometry = Geometry::TESTING;
    assert_eq!(
        geometry.chunk_count(0),
        Err(SizeError::DataSizeOutOfRange {
            data_size: 0,
            max_data_size: geometry.max_data_size()
        })
    );
    assert_eq!(geometry.chunk_count(1).unwrap(), 1);
    assert_eq!(geometry.chunk_count(32).unwrap(), 1);
    assert_eq!(geometry.chunk_count(33).unwrap(), 2);
    assert_eq!(geometry.chunk_len(33, 0).unwrap(), 32);
    assert_eq!(geometry.chunk_len(33, 1).unwrap(), 1);
    assert_eq!(
        geometry.chunk_len(33, 2),
        Err(SizeError::ChunkIndexOutOfRange {
            chunk_index: 2,
            chunk_count: 2
        })
    );

    // `n = 9`: segment 0 is full, segment 1 is the last byte, the rest are past the data.
    assert_eq!(geometry.segment_len(9, 0, 0).unwrap(), 8);
    assert_eq!(geometry.segment_len(9, 0, 1).unwrap(), 1);
    assert_eq!(geometry.segment_len(9, 0, 2).unwrap(), 0);
    assert_eq!(geometry.segment_len(9, 0, 3).unwrap(), 0);
    assert_eq!(
        geometry.segment_len(9, 0, 4),
        Err(SizeError::SegmentIndexOutOfRange {
            segment_index: 4,
            segments_per_chunk: 4
        })
    );
    let max_data_size = geometry.max_data_size();
    assert_eq!(
        geometry.chunk_count(max_data_size + 1),
        Err(SizeError::DataSizeOutOfRange {
            data_size: max_data_size + 1,
            max_data_size
        })
    );
}
