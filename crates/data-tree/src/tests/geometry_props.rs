//! Accepted geometries obey spec §2, and rejected triples return an error.
//!
//! See `strategies` for the `PROPTEST_CASES` soak command.

use proptest::prelude::*;

use super::strategies;
use crate::geometry::{Geometry, MAX_SEGMENTS_PER_CHUNK};

/// Proptest's usual count. Geometry checks are arithmetic, so this stays far
/// under 10 s. `PROPTEST_CASES` replaces it (see `strategies`).
const GEOMETRY_CASES: u32 = 256;

proptest! {
    #![proptest_config(strategies::case_config(GEOMETRY_CASES))]

    #[test]
    fn accepted_geometry_matches_its_parameters(geometry in strategies::any_geometry()) {
        check_accepted_geometry(geometry);
    }

    #[test]
    fn rejected_geometry_returns_an_error(parts in strategies::rejected_geometry()) {
        let result = Geometry::new(
            parts.chunk_size,
            parts.segments_per_chunk,
            parts.max_data_tx_chunks,
        );
        assert!(
            result.is_err(),
            "Geometry::new accepted chunk_size={} segments_per_chunk={} max_data_tx_chunks={}",
            parts.chunk_size,
            parts.segments_per_chunk,
            parts.max_data_tx_chunks
        );
    }
}

fn check_accepted_geometry(geometry: Geometry) {
    let segments_per_chunk = geometry.segments_per_chunk();
    let chunk_size = geometry.chunk_size();
    let max_data_tx_chunks = geometry.max_data_tx_chunks();

    assert!(segments_per_chunk.is_power_of_two());
    assert!(segments_per_chunk >= 1);
    assert!(segments_per_chunk <= MAX_SEGMENTS_PER_CHUNK);
    assert!(chunk_size >= 1);
    assert!(max_data_tx_chunks >= 1);
    assert_eq!(chunk_size % u64::from(segments_per_chunk), 0);
    assert!(
        chunk_size
            .checked_mul(u64::from(max_data_tx_chunks))
            .is_some()
    );
    assert_eq!(
        geometry.segment_size() * u64::from(segments_per_chunk),
        chunk_size
    );
    // `h = log2(segments_per_chunk)` (spec §2).
    assert_eq!(geometry.height(), segments_per_chunk.trailing_zeros());

    let again = Geometry::new(chunk_size, segments_per_chunk, max_data_tx_chunks)
        .expect("parameters taken from an accepted geometry");
    assert_eq!(again, geometry);
    assert_eq!(
        again.max_data_size(),
        chunk_size * u64::from(max_data_tx_chunks)
    );
}
