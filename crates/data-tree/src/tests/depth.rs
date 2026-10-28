use crate::geometry::Geometry;
use crate::shape::depth;

#[test]
fn depth_matches_the_spec_table() {
    assert_eq!(depth(0, 1), Some(0));

    assert_eq!(depth(0, 3), Some(2));
    assert_eq!(depth(1, 3), Some(2));
    assert_eq!(depth(2, 3), Some(1));

    for chunk_index in 0..4 {
        assert_eq!(depth(chunk_index, 5), Some(3), "c={chunk_index}");
    }
    assert_eq!(depth(4, 5), Some(1));

    for chunk_index in 0..4096 {
        assert_eq!(depth(chunk_index, 4096), Some(12), "c={chunk_index}");
    }

    // 5 TiB: 25 siblings below this index, 23 at and above it (spec §5).
    const DEPTH_DROP_CHUNK_INDEX: u32 = 1 << 24;
    let max_chunk_count = Geometry::MAINNET.max_data_tx_chunks();
    let split = DEPTH_DROP_CHUNK_INDEX;
    assert_eq!(depth(0, max_chunk_count), Some(25));
    assert_eq!(depth(split - 1, max_chunk_count), Some(25));
    assert_eq!(depth(split, max_chunk_count), Some(23));
    assert_eq!(depth(max_chunk_count - 1, max_chunk_count), Some(23));
    for chunk_index in 0..max_chunk_count {
        let expect = if chunk_index < split { 25 } else { 23 };
        assert_eq!(
            depth(chunk_index, max_chunk_count),
            Some(expect),
            "c={chunk_index}"
        );
    }
}

#[test]
fn depth_is_none_when_the_index_is_outside_the_count() {
    // spec §5: `depth` is defined for `c < C`. These inputs are outside that.
    assert_eq!(depth(0, 0), None);
    assert_eq!(depth(5, 3), None);
    assert_eq!(depth(u32::MAX, 2), None);
    // `c == C` is outside the range for every count the table above uses.
    for chunk_count in [1_u32, 3, 5, 4096] {
        assert_eq!(depth(chunk_count, chunk_count), None, "C={chunk_count}");
    }
}
