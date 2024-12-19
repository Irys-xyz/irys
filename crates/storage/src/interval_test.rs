#[test]
fn interval_overwrite_test() -> eyre::Result<()> {
    let mut map = NoditMap::<u32, Interval<u32>, IntervalStateWrapped>::new();
    let psm = IntervalStateWrapped::new(IntervalState {
        chunk_state: ChunkState::Unpacked,
    });
    map.insert_merge_touching_if_values_equal(ii(0, 1024), psm.clone())
        .unwrap();
    let size1 = std::mem::size_of_val(&map);
    dbg!(size1);

    let mut psm2 = psm.clone();
    psm2.chunk_state = ChunkState::Packed;
    map.insert_merge_touching_if_values_equal(ii(1025, 1032), psm2.clone())
        .unwrap();
    map.insert_merge_touching_if_values_equal(ii(1033, 1042), psm2.clone())
        .unwrap();
    map.insert_merge_touching_if_values_equal(ii(1043, 1050), psm.clone())
        .unwrap();
    let psm3 = IntervalStateWrapped::new(IntervalState {
        chunk_state: ChunkState::Data,
    });
    // insert_overwrite will cut the intervals it overlaps with and inserts this new interval in the gap, trimming intervals on the edge to account for this new interval
    let _ = map.insert_overwrite(ii(1030, 1040), psm3.clone());
    let mut iter = map.iter();

    assert_eq!(iter.next(), Some((&ii(0, 1024), &psm)));
    assert_eq!(iter.next(), Some((&ii(1025, 1029), &psm2)));
    assert_eq!(iter.next(), Some((&ii(1030, 1040), &psm3))); // inserted interval
    assert_eq!(iter.next(), Some((&ii(1041, 1042), &psm2)));
    assert_eq!(iter.next(), Some((&ii(1043, 1050), &psm)));

    let size2 = std::mem::size_of_val(&map);
    dbg!(size2);
    Ok(())
}
