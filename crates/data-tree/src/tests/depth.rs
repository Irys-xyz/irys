use crate::geometry::Geometry;
use crate::shape::depth;

#[test]
fn depth_matches_the_spec_table() {
    assert_eq!(depth(0, 1), 0);

    assert_eq!(depth(0, 3), 2);
    assert_eq!(depth(1, 3), 2);
    assert_eq!(depth(2, 3), 1);

    for chunk_index in 0..4 {
        assert_eq!(depth(chunk_index, 5), 3, "c={chunk_index}");
    }
    assert_eq!(depth(4, 5), 1);

    for chunk_index in 0..4096 {
        assert_eq!(depth(chunk_index, 4096), 12, "c={chunk_index}");
    }

    // 5 TiB: 25 siblings below this index, 23 at and above it (spec §5).
    const DEPTH_DROP_CHUNK_INDEX: u32 = 1 << 24;
    let max_chunk_count = Geometry::MAINNET.max_data_tx_chunks();
    let split = DEPTH_DROP_CHUNK_INDEX;
    assert_eq!(depth(0, max_chunk_count), 25);
    assert_eq!(depth(split - 1, max_chunk_count), 25);
    assert_eq!(depth(split, max_chunk_count), 23);
    assert_eq!(depth(max_chunk_count - 1, max_chunk_count), 23);
    for chunk_index in 0..max_chunk_count {
        let expect = if chunk_index < split { 25 } else { 23 };
        assert_eq!(
            depth(chunk_index, max_chunk_count),
            expect,
            "c={chunk_index}"
        );
    }
}
