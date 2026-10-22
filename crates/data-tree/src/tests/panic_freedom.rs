//! Verifiers, lengths, and `depth` return on untrusted input (spec §5, §10).
//!
//! A panic fails the test. These functions take plain values so a later fuzzer
//! can call them. See `strategies` for the `PROPTEST_CASES` soak command.

use proptest::prelude::*;

use super::strategies;
use crate::chunk_key;
use crate::depth;
use crate::geometry::Geometry;
use crate::verify::{verify_chunk, verify_segment, verify_size};

/// Proptest's usual count. The byte cap bounds hashing, so this stays under 10 s.
const UNTRUSTED_CASES: u32 = 256;

/// Length arithmetic is cheap. The full walk stops at [`FULL_CHUNK_WALK`].
const LENGTH_CASES: u32 = 256;

/// `depth` is a loop over a `u32` chunk count. No allocation.
const DEPTH_CASES: u32 = 256;

/// Past this many chunks the length sum is checked on the first chunk, the
/// last chunk, and one probe. `chunk_count` can be `u32::MAX` (spec §2).
const FULL_CHUNK_WALK: u32 = 64;

proptest! {
    #![proptest_config(strategies::case_config(UNTRUSTED_CASES))]

    #[test]
    fn untrusted_input_never_panics_or_verifies(
        input in strategies::untrusted_verifier_input()
    ) {
        check_untrusted(&input);
    }
}

proptest! {
    #![proptest_config(strategies::case_config(LENGTH_CASES))]

    #[test]
    fn lengths_follow_the_size_definition(
        (geometry, data_size, probe) in length_input()
    ) {
        check_lengths(geometry, data_size, probe);
    }
}

proptest! {
    #![proptest_config(strategies::case_config(DEPTH_CASES))]

    #[test]
    fn depth_returns_below_the_chunk_count(
        (chunk_index, chunk_count) in strategies::chunk_index_below_count()
    ) {
        // Precondition: `chunk_index < chunk_count` (spec §5). `chunk_count >= 1`.
        assert!(chunk_index < chunk_count);
        let _ = depth(chunk_index, chunk_count).expect("chunk_index < chunk_count");
    }
}

fn length_input() -> impl Strategy<Value = (Geometry, u64, u32)> {
    strategies::any_geometry().prop_flat_map(|geometry| {
        (
            strategies::edge_biased_data_size(geometry),
            strategies::edge_biased_u32(),
        )
            .prop_map(move |(data_size, probe)| (geometry, data_size, probe))
    })
}

/// Calls every untrusted entry point. The commitment is not a tree root, so
/// `Ok` would be a SHA-256 preimage (spec §3, §10).
fn check_untrusted(input: &strategies::UntrustedVerifierInput) {
    let geometry = input.geometry;
    let _ = geometry.chunk_count(input.data_size);
    if let Ok(chunk_length) = geometry.chunk_len(input.data_size, input.chunk_index) {
        assert!(
            (1..=geometry.chunk_size()).contains(&chunk_length),
            "chunk_len {chunk_length} is outside 1..={}",
            geometry.chunk_size()
        );
    }
    if let Ok(segment_length) =
        geometry.segment_len(input.data_size, input.chunk_index, input.segment_index)
    {
        assert!(
            segment_length <= geometry.segment_size(),
            "segment_len {segment_length} is above segment_size {}",
            geometry.segment_size()
        );
    }
    if let Ok(chunk_count) = geometry.chunk_count(input.data_size)
        && input.chunk_index < chunk_count
    {
        // This input is in range. `depth` is `None` outside it (spec §5).
        let _ = depth(input.chunk_index, chunk_count).expect("chunk_index < chunk_count");
    }
    let _ = chunk_key(&input.commitment, input.chunk_index);

    let chunk = verify_chunk(
        &geometry,
        &input.commitment,
        input.data_size,
        input.chunk_index,
        &input.bytes,
        &input.proof,
    );
    assert!(
        chunk.is_err(),
        "verify_chunk accepted a commitment that was not built from these bytes (spec §6, §10)"
    );
    let segment = verify_segment(
        &geometry,
        &input.commitment,
        input.data_size,
        input.chunk_index,
        input.segment_index,
        &input.bytes,
        &input.proof,
    );
    assert!(
        segment.is_err(),
        "verify_segment accepted a commitment that was not built from these bytes (spec §6, §10)"
    );
    let size = verify_size(
        &geometry,
        &input.commitment,
        &input.data_tree_root,
        input.data_size,
    );
    assert!(
        size.is_err(),
        "verify_size accepted a commitment that was not built from this root (spec §5, §10)"
    );
}

fn check_lengths(geometry: Geometry, data_size: u64, probe: u32) {
    // Out-of-range inputs must return, not panic (spec §2).
    let _ = geometry.chunk_count(data_size);
    let _ = geometry.chunk_len(data_size, probe);
    let _ = geometry.segment_len(data_size, probe, probe);

    let Ok(chunk_count) = geometry.chunk_count(data_size) else {
        return;
    };
    if chunk_count <= FULL_CHUNK_WALK {
        check_every_chunk(&geometry, data_size, chunk_count);
    } else {
        check_chunk_spots(&geometry, data_size, chunk_count, probe);
    }
    for chunk_index in spot_chunks(chunk_count, probe) {
        check_segments(&geometry, data_size, chunk_index);
    }
}

fn check_every_chunk(geometry: &Geometry, data_size: u64, chunk_count: u32) {
    let mut sum = 0_u64;
    for chunk_index in 0..chunk_count {
        let chunk_length = chunk_length_matches_spec(geometry, data_size, chunk_index);
        sum = sum
            .checked_add(chunk_length)
            .expect("sum of chunk lengths is data_size, which fits u64");
        check_segments(geometry, data_size, chunk_index);
    }
    assert_eq!(sum, data_size, "sum of chunk_len over every chunk");
}

fn check_chunk_spots(geometry: &Geometry, data_size: u64, chunk_count: u32, probe: u32) {
    let chunk_size = geometry.chunk_size();
    let last_index = chunk_count - 1;
    let last = chunk_length_matches_spec(geometry, data_size, last_index);
    if chunk_count == 1 {
        assert_eq!(last, data_size);
    } else {
        let first = chunk_length_matches_spec(geometry, data_size, 0);
        assert_eq!(
            first, chunk_size,
            "a non-final chunk is a full chunk (spec §2)"
        );
        // `(C - 1) × chunk_size + chunk_len(last) = n` (spec §2). Checked here
        // instead of a loop: `C` can be `u32::MAX`.
        let full_chunks = u64::from(chunk_count - 1)
            .checked_mul(chunk_size)
            .expect("max_data_tx_chunks * chunk_size fits u64");
        assert_eq!(
            full_chunks.checked_add(last).expect("the sum fits u64"),
            data_size
        );
    }
    let probe_index = probe % chunk_count;
    let _ = chunk_length_matches_spec(geometry, data_size, probe_index);
}

fn spot_chunks(chunk_count: u32, probe: u32) -> Vec<u32> {
    if chunk_count <= FULL_CHUNK_WALK {
        return Vec::new();
    }
    let mut chunks = vec![0, chunk_count - 1, probe % chunk_count];
    chunks.sort_unstable();
    chunks.dedup();
    chunks
}

fn chunk_length_matches_spec(geometry: &Geometry, data_size: u64, chunk_index: u32) -> u64 {
    let chunk_length = geometry
        .chunk_len(data_size, chunk_index)
        .expect("chunk_index was drawn below chunk_count");
    let expected = spec_chunk_length(geometry.chunk_size(), data_size, chunk_index);
    assert_eq!(chunk_length, expected, "chunk {chunk_index}");
    assert!(
        (1..=geometry.chunk_size()).contains(&chunk_length),
        "chunk_len {chunk_length} is outside 1..={}",
        geometry.chunk_size()
    );
    chunk_length
}

fn check_segments(geometry: &Geometry, data_size: u64, chunk_index: u32) {
    for segment_index in 0..geometry.segments_per_chunk() {
        let segment_length = geometry
            .segment_len(data_size, chunk_index, segment_index)
            .expect("indexes are in range");
        let expected = spec_segment_length(
            geometry.chunk_size(),
            geometry.segment_size(),
            data_size,
            chunk_index,
            segment_index,
        );
        assert_eq!(
            segment_length, expected,
            "segment {segment_index} of chunk {chunk_index}"
        );
        assert!(segment_length <= geometry.segment_size());
    }
}

/// `min(chunk_size, n − c × chunk_size)` for `c < C` (spec §2).
fn spec_chunk_length(chunk_size: u64, data_size: u64, chunk_index: u32) -> u64 {
    let start = u64::from(chunk_index)
        .checked_mul(chunk_size)
        .expect("c < C and max_data_tx_chunks * chunk_size fits u64");
    let remaining = data_size
        .checked_sub(start)
        .expect("c < C implies the chunk starts below data_size");
    remaining.min(chunk_size)
}

/// `segment_len` (spec §2). The subtraction runs only when the start is below `n`.
fn spec_segment_length(
    chunk_size: u64,
    segment_size: u64,
    data_size: u64,
    chunk_index: u32,
    segment_index: u32,
) -> u64 {
    let chunk_start = u64::from(chunk_index)
        .checked_mul(chunk_size)
        .expect("c < C and max_data_tx_chunks * chunk_size fits u64");
    let segment_offset = u64::from(segment_index)
        .checked_mul(segment_size)
        .expect("j < segments_per_chunk, so the offset is below chunk_size");
    let start = chunk_start
        .checked_add(segment_offset)
        .expect("the segment start is at most max_data_size");
    if start >= data_size {
        return 0;
    }
    data_size
        .checked_sub(start)
        .expect("segment start is below data_size")
        .min(segment_size)
}
