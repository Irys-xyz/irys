//! Exhaustive proofs of data-tree length arithmetic (spec §2) and `depth` (spec §5).
//!
//! `cargo kani` in this crate. The harnesses call the production
//! functions. Kani checks every input, not a sample, and a panic fails the proof.

#[cfg(kani)]
mod proofs {
    use irys_data_tree::Geometry;

    #[kani::proof]
    fn testing_chunk_count_is_ceil_div() {
        chunk_count_is_ceil_div(Geometry::TESTING);
    }

    #[kani::proof]
    fn mainnet_chunk_count_is_ceil_div() {
        chunk_count_is_ceil_div(Geometry::MAINNET);
    }

    #[kani::proof]
    fn testing_chunk_len_matches_spec() {
        chunk_len_matches_spec(Geometry::TESTING);
    }

    #[kani::proof]
    fn mainnet_chunk_len_matches_spec() {
        chunk_len_matches_spec(Geometry::MAINNET);
    }

    #[kani::proof]
    fn testing_segment_len_matches_spec() {
        segment_len_matches_spec(Geometry::TESTING);
    }

    #[kani::proof]
    fn mainnet_segment_len_matches_spec() {
        segment_len_matches_spec(Geometry::MAINNET);
    }

    #[kani::proof]
    #[kani::unwind(5)]
    fn testing_segments_partition_a_chunk() {
        segments_partition_a_chunk::<4>(Geometry::TESTING);
    }

    #[kani::proof]
    #[kani::unwind(65)]
    fn mainnet_segments_partition_a_chunk() {
        segments_partition_a_chunk::<64>(Geometry::MAINNET);
    }

    fn chunk_count_is_ceil_div(geometry: Geometry) {
        let data_size: u64 = kani::any();
        match geometry.chunk_count(data_size) {
            Ok(count) => {
                assert!(data_size >= 1);
                assert!(data_size <= geometry.max_data_size());
                assert!(count >= 1);
                assert!(count <= geometry.max_data_tx_chunks());
                assert_eq!(u64::from(count), data_size.div_ceil(geometry.chunk_size()));
            }
            Err(_) => {
                assert!(data_size == 0 || data_size > geometry.max_data_size());
            }
        }
    }

    /// `c < count` makes `count - 1` full chunks plus a final remainder, which
    /// is `n`. One symbolic `c` covers every index.
    fn chunk_len_matches_spec(geometry: Geometry) {
        let data_size: u64 = kani::any();
        let chunk_index: u32 = kani::any();
        match geometry.chunk_len(data_size, chunk_index) {
            Ok(len) => {
                let chunk_size = geometry.chunk_size();
                let start = u64::from(chunk_index)
                    .checked_mul(chunk_size)
                    .expect("c < count and max_data_tx_chunks * chunk_size fits");
                assert!(start < data_size);
                let remain = data_size - start;
                let expected = if remain < chunk_size {
                    remain
                } else {
                    chunk_size
                };
                assert_eq!(len, expected);
                assert!(len >= 1);
                let count = geometry
                    .chunk_count(data_size)
                    .expect("chunk_len succeeded, so the data size is in range");
                assert!(chunk_index < count);
                if chunk_index + 1 < count {
                    assert_eq!(len, chunk_size);
                } else {
                    assert_eq!(start + len, data_size);
                }
            }
            Err(_) => match geometry.chunk_count(data_size) {
                Ok(count) => assert!(chunk_index >= count),
                Err(_) => {
                    assert!(data_size == 0 || data_size > geometry.max_data_size());
                }
            },
        }
    }

    fn segment_len_matches_spec(geometry: Geometry) {
        let data_size: u64 = kani::any();
        let chunk_index: u32 = kani::any();
        let segment_index: u32 = kani::any();
        match geometry.segment_len(data_size, chunk_index, segment_index) {
            Ok(len) => {
                let chunk_size = geometry.chunk_size();
                let segment_size = geometry.segment_size();
                let chunk_start = u64::from(chunk_index)
                    .checked_mul(chunk_size)
                    .expect("c < count and max_data_tx_chunks * chunk_size fits");
                let segment_offset = u64::from(segment_index)
                    .checked_mul(segment_size)
                    .expect("j < segments_per_chunk");
                let start = chunk_start
                    .checked_add(segment_offset)
                    .expect("segment start is below max_data_size");
                if start >= data_size {
                    assert_eq!(len, 0);
                } else {
                    let remain = data_size - start;
                    let expected = if remain < segment_size {
                        remain
                    } else {
                        segment_size
                    };
                    assert_eq!(len, expected);
                }
                assert!(len <= segment_size);
                let count = geometry
                    .chunk_count(data_size)
                    .expect("segment_len succeeded, so the data size is in range");
                assert!(chunk_index < count);
                assert!(segment_index < geometry.segments_per_chunk());
            }
            Err(_) => {
                let past_chunks = geometry
                    .chunk_count(data_size)
                    .map(|count| chunk_index >= count)
                    .unwrap_or(true);
                let past_segments = segment_index >= geometry.segments_per_chunk();
                assert!(past_chunks || past_segments);
            }
        }
    }

    fn segments_partition_a_chunk<const SEGMENTS: u32>(geometry: Geometry) {
        assert_eq!(geometry.segments_per_chunk(), SEGMENTS);
        let data_size: u64 = kani::any();
        kani::assume(data_size >= 1 && data_size <= geometry.max_data_size());
        let chunk_index: u32 = kani::any();
        let count = geometry
            .chunk_count(data_size)
            .expect("data size is in range");
        kani::assume(chunk_index < count);
        let chunk_len = geometry
            .chunk_len(data_size, chunk_index)
            .expect("chunk index is in range");
        let mut sum = 0u64;
        let mut segment_index = 0u32;
        while segment_index < SEGMENTS {
            let len = geometry
                .segment_len(data_size, chunk_index, segment_index)
                .expect("segment index is in range");
            sum = sum
                .checked_add(len)
                .expect("segment lengths in one chunk fit in u64");
            segment_index += 1;
        }
        assert_eq!(sum, chunk_len);
    }

    /// Spec §5. `None` outside `c < C`. Otherwise `depth` is the sibling count:
    /// `log2(C)` when `C` is a power of two, and otherwise the left/right split
    /// on the greatest power of two strictly below `C`. The loop at least
    /// halves, so 32 iterations cover every `u32`.
    #[kani::proof]
    #[kani::unwind(32)]
    fn depth_matches_spec() {
        let chunk_index: u32 = kani::any();
        let chunk_count: u32 = kani::any();
        match irys_data_tree::depth(chunk_index, chunk_count) {
            None => {
                assert!(chunk_count == 0 || chunk_index >= chunk_count);
            }
            Some(got) => {
                assert!(chunk_count >= 1 && chunk_index < chunk_count);
                assert!(got <= 32);
                if chunk_count.is_power_of_two() {
                    assert_eq!(got, chunk_count.trailing_zeros());
                }
                if chunk_count > 1 {
                    let k = spec_largest_power_of_two_below(chunk_count);
                    assert!(k.is_power_of_two() && k < chunk_count);
                    match k.checked_mul(2) {
                        Some(twice) => assert!(twice >= chunk_count),
                        None => assert_eq!(k, 1_u32 << 31),
                    }
                    if chunk_index < k {
                        assert_eq!(got, 1 + k.trailing_zeros());
                    } else {
                        let rest = irys_data_tree::depth(chunk_index - k, chunk_count - k)
                            .expect("c - k is below C - k");
                        assert_eq!(got, rest + 1);
                    }
                }
            }
        }
    }

    /// Spec §4: greatest power of two strictly below `chunk_count` (`chunk_count >= 2`).
    fn spec_largest_power_of_two_below(chunk_count: u32) -> u32 {
        1_u32 << (u32::BITS - 1 - (chunk_count - 1).leading_zeros())
    }
}
