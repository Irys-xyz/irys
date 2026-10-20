use crate::error::{GeometryError, SizeError};
use crate::geometry::{Geometry, MAX_SEGMENTS_PER_CHUNK};
use rstest::rstest;

#[test]
fn presets_match_new() {
    // spec §2: the literals below are the mainnet and `testing()` parameter table.
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
    // Any multiple of one segment. Height is 0 because `h = log2(1)` (spec §2).
    const CHUNK_SIZE: u64 = 8;
    const MAX_CHUNKS: u32 = 4;
    let geometry = Geometry::new(CHUNK_SIZE, 1, MAX_CHUNKS).unwrap();
    assert_eq!(geometry.height(), 0);
    assert_eq!(geometry.segment_size(), CHUNK_SIZE);
}

// Rejected `Geometry::new` inputs (spec §2). `1 << 63` times 3 does not fit `u64`.
#[rstest]
#[case(Geometry::TESTING.chunk_size(), 0, 1, GeometryError::SegmentsPerChunk { segments_per_chunk: 0 })]
#[case(Geometry::TESTING.chunk_size(), 3, 1, GeometryError::SegmentsPerChunk { segments_per_chunk: 3 })]
#[case(
    u64::from(MAX_SEGMENTS_PER_CHUNK) * 2,
    MAX_SEGMENTS_PER_CHUNK * 2,
    1,
    GeometryError::SegmentsPerChunk { segments_per_chunk: MAX_SEGMENTS_PER_CHUNK * 2 }
)]
#[case(0, 1, 1, GeometryError::ChunkSizeZero)]
#[case(
    Geometry::TESTING.chunk_size() - 2,
    Geometry::TESTING.segments_per_chunk(),
    1,
    GeometryError::ChunkSizeNotMultiple {
        chunk_size: Geometry::TESTING.chunk_size() - 2,
        segments_per_chunk: Geometry::TESTING.segments_per_chunk(),
    }
)]
#[case(
    Geometry::TESTING.chunk_size(),
    Geometry::TESTING.segments_per_chunk(),
    0,
    GeometryError::MaxChunksZero
)]
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
    let chunk_size = geometry.chunk_size();
    assert_eq!(geometry.chunk_count(1).unwrap(), 1);
    assert_eq!(geometry.chunk_count(chunk_size).unwrap(), 1);
    assert_eq!(geometry.chunk_count(chunk_size + 1).unwrap(), 2);
    assert_eq!(geometry.chunk_len(chunk_size + 1, 0).unwrap(), chunk_size);
    assert_eq!(geometry.chunk_len(chunk_size + 1, 1).unwrap(), 1);
    assert_eq!(
        geometry.chunk_len(chunk_size + 1, 2),
        Err(SizeError::ChunkIndexOutOfRange {
            chunk_index: 2,
            chunk_count: 2
        })
    );

    // One byte past a full segment: segment 0 is full, segment 1 holds that byte.
    let segment_size = geometry.segment_size();
    let data_size = segment_size + 1;
    assert_eq!(geometry.segment_len(data_size, 0, 0).unwrap(), segment_size);
    assert_eq!(geometry.segment_len(data_size, 0, 1).unwrap(), 1);
    for segment_index in 2..geometry.segments_per_chunk() {
        assert_eq!(
            geometry.segment_len(data_size, 0, segment_index).unwrap(),
            0
        );
    }
    assert_eq!(
        geometry.segment_len(data_size, 0, geometry.segments_per_chunk()),
        Err(SizeError::SegmentIndexOutOfRange {
            segment_index: geometry.segments_per_chunk(),
            segments_per_chunk: geometry.segments_per_chunk()
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
