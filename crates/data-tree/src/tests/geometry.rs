use crate::error::{GeometryError, SizeError, VerifyError};
use crate::geometry::{Geometry, MAX_SEGMENTS_PER_CHUNK};
use crate::shape::depth;
use crate::types::{DataCommitment, HASH_SIZE};
use crate::verify_segment;
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

/// Greatest `max_data_size` at [`MAX_SEGMENTS_PER_CHUNK`].
///
/// `chunk_size` is the greatest multiple of the segment count. One chunk is the
/// greatest product: any larger chunk count forces a smaller `chunk_size`.
fn largest_accepted_geometry() -> Geometry {
    let segments_per_chunk = MAX_SEGMENTS_PER_CHUNK;
    let segment_count = u64::from(segments_per_chunk);
    let chunk_size = u64::MAX - (u64::MAX % segment_count);
    const ONE_CHUNK: u32 = 1;
    Geometry::new(chunk_size, segments_per_chunk, ONE_CHUNK)
        .expect("one chunk of the greatest multiple fits u64")
}

/// `u32::MAX` chunks and the greatest `chunk_size` whose product still fits,
/// at [`MAX_SEGMENTS_PER_CHUNK`]. The last chunk index times `chunk_size` is the
/// largest product `chunk_len` forms at this segment count.
fn geometry_at_max_chunk_count() -> Geometry {
    let segments_per_chunk = MAX_SEGMENTS_PER_CHUNK;
    let max_data_tx_chunks = u32::MAX;
    let segment_count = u64::from(segments_per_chunk);
    let largest_chunk_size = u64::MAX / u64::from(max_data_tx_chunks);
    let chunk_size = largest_chunk_size - (largest_chunk_size % segment_count);
    let geometry = Geometry::new(chunk_size, segments_per_chunk, max_data_tx_chunks)
        .expect("max chunk count times this chunk size fits u64");
    assert_eq!(
        Geometry::new(
            chunk_size + segment_count,
            segments_per_chunk,
            max_data_tx_chunks
        ),
        Err(GeometryError::MaxDataSizeOverflow)
    );
    geometry
}

fn last_indexes(geometry: &Geometry) -> (u64, u32, u32) {
    let data_size = geometry.max_data_size();
    let chunk_count = geometry
        .chunk_count(data_size)
        .expect("max data size is in range");
    let chunk_index = chunk_count - 1;
    let segment_index = geometry.segments_per_chunk() - 1;
    (data_size, chunk_index, segment_index)
}

#[test]
fn chunk_len_holds_for_the_last_chunk_of_the_extreme_geometries() {
    for geometry in [largest_accepted_geometry(), geometry_at_max_chunk_count()] {
        let (data_size, chunk_index, _) = last_indexes(&geometry);
        assert_eq!(
            geometry.chunk_len(data_size, chunk_index).unwrap(),
            geometry.chunk_size()
        );
    }
}

#[test]
fn segment_len_holds_for_the_last_segment_of_the_extreme_geometries() {
    for geometry in [largest_accepted_geometry(), geometry_at_max_chunk_count()] {
        let (data_size, chunk_index, segment_index) = last_indexes(&geometry);
        assert_eq!(
            geometry
                .segment_len(data_size, chunk_index, segment_index)
                .unwrap(),
            geometry.segment_size()
        );
        // The segment starts past this one byte. The result is 0, and the
        // length is not produced by a subtraction below zero.
        assert_eq!(geometry.segment_len(1, 0, segment_index).unwrap(), 0);
    }
}

#[test]
fn verify_segment_does_not_panic_on_the_extreme_geometries() {
    let commitment = DataCommitment([0x11; HASH_SIZE]);

    let geometry = largest_accepted_geometry();
    let (data_size, chunk_index, segment_index) = last_indexes(&geometry);
    // The last segment is `segment_size` bytes. That length does not fit a
    // test allocation. An empty slice fails the length check, which runs after
    // `segment_len` and before hashing.
    assert_eq!(
        verify_segment(
            &geometry,
            &commitment,
            data_size,
            chunk_index,
            segment_index,
            &[],
            &[],
        ),
        Err(VerifyError::InvalidSegmentLength {
            expected: geometry.segment_size(),
            actual: 0,
        })
    );

    let geometry = geometry_at_max_chunk_count();
    // `1 << u32::BITS` is the greatest `chunk_size` this chunk count accepts.
    assert_eq!(geometry.chunk_size(), 1_u64 << u32::BITS);
    let (data_size, chunk_index, segment_index) = last_indexes(&geometry);
    let chunk_count = geometry.chunk_count(data_size).unwrap();
    let segment_length = geometry
        .segment_len(data_size, chunk_index, segment_index)
        .unwrap();
    let segment_bytes = usize::try_from(segment_length).unwrap();
    let bytes = vec![0_u8; segment_bytes];
    let hash_count = geometry.height() + depth(chunk_index, chunk_count);
    let proof = vec![0_u8; usize::try_from(hash_count).unwrap() * HASH_SIZE];
    assert_eq!(
        verify_segment(
            &geometry,
            &commitment,
            data_size,
            chunk_index,
            segment_index,
            &bytes,
            &proof,
        ),
        Err(VerifyError::CommitmentMismatch)
    );
}
