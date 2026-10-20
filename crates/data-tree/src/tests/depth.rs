use crate::shape::depth;

#[test]
fn depth_matches_the_spec_table() {
    assert_eq!(depth(0, 1), 0);

    assert_eq!(depth(0, 3), 2);
    assert_eq!(depth(1, 3), 2);
    assert_eq!(depth(2, 3), 1);

    for c in 0..4 {
        assert_eq!(depth(c, 5), 3, "c={c}");
    }
    assert_eq!(depth(4, 5), 1);

    for c in 0..4096 {
        assert_eq!(depth(c, 4096), 12, "c={c}");
    }

    // 5 TiB: 25 siblings below 2^24, 23 at and above it (spec §5).
    let max = 20_971_520_u32;
    let split = 1_u32 << 24;
    assert_eq!(depth(0, max), 25);
    assert_eq!(depth(split - 1, max), 25);
    assert_eq!(depth(split, max), 23);
    assert_eq!(depth(max - 1, max), 23);
    for c in 0..max {
        let expect = if c < split { 25 } else { 23 };
        assert_eq!(depth(c, max), expect, "c={c}");
    }
}
