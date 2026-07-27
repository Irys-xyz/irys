//! Unit tests for `index_heal` pure helpers.
//! Included as `mod tests` from `index_heal.rs` under `#[cfg(test)]`.

use super::{
    INDEX_HEAL_MAX_BLOCKS_PER_PASS, PlacementSpan, collect_placement_blocks_for_gaps,
    exclusive_partition_end, readiness_sample_offsets,
};
use irys_types::{BlockHash, PartitionChunkOffset};
use std::collections::BTreeMap;

mod readiness_sample_tests {
    use super::{PartitionChunkOffset, readiness_sample_offsets};

    #[test]
    fn empty_window() {
        assert!(readiness_sample_offsets(PartitionChunkOffset::from(0)).is_empty());
    }

    #[test]
    fn single_offset() {
        assert_eq!(
            readiness_sample_offsets(PartitionChunkOffset::from(1)),
            vec![PartitionChunkOffset::from(0)]
        );
    }

    #[test]
    fn three_distinct_samples() {
        assert_eq!(
            readiness_sample_offsets(PartitionChunkOffset::from(100)),
            vec![
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(49),
                PartitionChunkOffset::from(99),
            ]
        );
    }
}

mod exclusive_partition_end_tests {
    use super::exclusive_partition_end;

    #[test]
    fn full_sm_includes_last_chunk_when_frontier_past_sm() {
        assert_eq!(exclusive_partition_end(0, 99, Some(1_000)), 100);
        assert_eq!(exclusive_partition_end(0, 99, Some(100)), 100);
    }

    #[test]
    fn partial_frontier_inside_sm_is_exclusive_relative() {
        assert_eq!(exclusive_partition_end(0, 99, Some(50)), 50);
    }

    #[test]
    fn frontier_before_sm_is_empty() {
        assert_eq!(exclusive_partition_end(100, 199, Some(50)), 0);
        assert_eq!(exclusive_partition_end(100, 199, Some(100)), 0);
    }

    #[test]
    fn frontier_none_is_empty() {
        assert_eq!(exclusive_partition_end(0, 99, None), 0);
    }

    #[test]
    fn non_zero_sm_start() {
        assert_eq!(exclusive_partition_end(100, 199, Some(150)), 50);
        assert_eq!(exclusive_partition_end(100, 199, Some(500)), 100);
    }
}

mod placement_collect_tests {
    use super::{
        BTreeMap, BlockHash, INDEX_HEAL_MAX_BLOCKS_PER_PASS, PartitionChunkOffset, PlacementSpan,
        collect_placement_blocks_for_gaps,
    };

    fn hash(n: u8) -> BlockHash {
        BlockHash::from([n; 32])
    }

    /// Spans keyed by absolute ledger offset → block that owns it.
    fn span_lookup(
        spans: &[(u64, PlacementSpan)],
    ) -> impl FnMut(u64) -> Option<PlacementSpan> + '_ {
        move |ledger_abs| {
            spans
                .iter()
                .find(|(_, s)| {
                    ledger_abs >= s.start_chunk_offset && ledger_abs < s.end_chunk_offset
                })
                .map(|(_, s)| *s)
        }
    }

    #[test]
    fn single_block_hole_one_lookup_and_jumps_to_end() {
        // Block height 10 covers absolute [0, 10). Hole is partition [0, 10).
        let spans = [(
            0_u64,
            PlacementSpan {
                height: 10,
                block_hash: hash(1),
                start_chunk_offset: 0,
                end_chunk_offset: 10,
            },
        )];
        let gaps = [(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(10),
        )];
        let r = collect_placement_blocks_for_gaps(
            &gaps,
            /* sm_ledger_start */ 0,
            /* max_chunk_offset */ 100,
            PartitionChunkOffset::from(100),
            span_lookup(&spans),
        );
        assert_eq!(r.placement_blocks.len(), 1);
        assert_eq!(r.placement_blocks.get(&10), Some(&hash(1)));
        // One bounds lookup — jump covers the rest of the hole.
        assert_eq!(r.bounds_lookups, 1);
        assert_eq!(r.sample_offsets, vec![PartitionChunkOffset::from(0)]);
        assert!(!r.any_soft_skip);
        assert_eq!(r.recheck_max, PartitionChunkOffset::from(10));
    }

    #[test]
    fn multi_block_hole_collects_each_placement_once() {
        // Hole [0, 25): block 1 covers [0,10), block 2 covers [10,25).
        let spans = [
            (
                0_u64,
                PlacementSpan {
                    height: 1,
                    block_hash: hash(1),
                    start_chunk_offset: 0,
                    end_chunk_offset: 10,
                },
            ),
            (
                10_u64,
                PlacementSpan {
                    height: 2,
                    block_hash: hash(2),
                    start_chunk_offset: 10,
                    end_chunk_offset: 25,
                },
            ),
        ];
        let gaps = [(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(25),
        )];
        let r = collect_placement_blocks_for_gaps(
            &gaps,
            0,
            100,
            PartitionChunkOffset::from(100),
            span_lookup(&spans),
        );
        assert_eq!(
            r.placement_blocks,
            BTreeMap::from([(1, hash(1)), (2, hash(2))])
        );
        // Two lookups — one per block, not one per chunk.
        assert_eq!(r.bounds_lookups, 2);
        assert_eq!(
            r.sample_offsets,
            vec![
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(10)
            ]
        );
    }

    #[test]
    fn disjoint_holes_do_not_fill_span_between_them() {
        // Holes [0,5) and [20,25). Block at [0,5) and block at [20,30). Heights 1 and 5.
        // No intermediate heights should appear.
        let spans = [
            (
                0_u64,
                PlacementSpan {
                    height: 1,
                    block_hash: hash(1),
                    start_chunk_offset: 0,
                    end_chunk_offset: 5,
                },
            ),
            (
                20_u64,
                PlacementSpan {
                    height: 5,
                    block_hash: hash(5),
                    start_chunk_offset: 20,
                    end_chunk_offset: 30,
                },
            ),
        ];
        let gaps = [
            (PartitionChunkOffset::from(0), PartitionChunkOffset::from(5)),
            (
                PartitionChunkOffset::from(20),
                PartitionChunkOffset::from(25),
            ),
        ];
        let r = collect_placement_blocks_for_gaps(
            &gaps,
            0,
            100,
            PartitionChunkOffset::from(100),
            span_lookup(&spans),
        );
        assert_eq!(
            r.placement_blocks.keys().copied().collect::<Vec<_>>(),
            vec![1, 5]
        );
        assert_eq!(r.bounds_lookups, 2);
        // recheck_max tracks the farthest hole end (25 exclusive).
        assert_eq!(r.recheck_max, PartitionChunkOffset::from(25));
    }

    #[test]
    fn soft_skip_mid_hole_continues_and_marks_partial() {
        // Hole [0, 20). Offset 0→block1 [0,5); 5..=9 soft-skip; 10→block2 [10,20).
        let r = collect_placement_blocks_for_gaps(
            &[(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(20),
            )],
            0,
            100,
            PartitionChunkOffset::from(100),
            |ledger_abs| {
                if ledger_abs < 5 {
                    Some(PlacementSpan {
                        height: 1,
                        block_hash: hash(1),
                        start_chunk_offset: 0,
                        end_chunk_offset: 5,
                    })
                } else if (5..10).contains(&ledger_abs) {
                    None
                } else {
                    Some(PlacementSpan {
                        height: 2,
                        block_hash: hash(2),
                        start_chunk_offset: 10,
                        end_chunk_offset: 20,
                    })
                }
            },
        );
        assert!(r.any_soft_skip);
        assert_eq!(
            r.placement_blocks,
            BTreeMap::from([(1, hash(1)), (2, hash(2))])
        );
        // 1 lookup for block1 + 5 soft-skips (5..10) + 1 for block2 = 7
        assert_eq!(r.bounds_lookups, 7);
    }

    #[test]
    fn hole_past_frontier_is_ignored() {
        let gaps = [(
            PartitionChunkOffset::from(50),
            PartitionChunkOffset::from(60),
        )];
        let r = collect_placement_blocks_for_gaps(
            &gaps,
            0,
            /* max_chunk_offset */ 40, // hole starts past frontier
            PartitionChunkOffset::from(100),
            |_abs| {
                panic!("lookup must not run for past-frontier hole");
            },
        );
        assert!(r.placement_blocks.is_empty());
        assert!(!r.any_soft_skip);
        assert_eq!(r.bounds_lookups, 0);
    }

    #[test]
    fn non_zero_sm_ledger_start_maps_relative_offsets() {
        // SM starts at absolute 1000. Hole partition [0, 10) → abs [1000, 1010).
        // Block covers [1000, 1010).
        let spans = [(
            1000_u64,
            PlacementSpan {
                height: 42,
                block_hash: hash(42),
                start_chunk_offset: 1000,
                end_chunk_offset: 1010,
            },
        )];
        let gaps = [(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(10),
        )];
        let r = collect_placement_blocks_for_gaps(
            &gaps,
            /* sm_ledger_start */ 1000,
            /* max_chunk_offset */ 5000,
            PartitionChunkOffset::from(100),
            span_lookup(&spans),
        );
        assert_eq!(r.placement_blocks.get(&42), Some(&hash(42)));
        assert_eq!(r.bounds_lookups, 1);
        assert_eq!(r.recheck_max, PartitionChunkOffset::from(10));
    }

    #[test]
    fn degenerate_end_leq_start_still_advances() {
        // end_chunk_offset <= ledger_abs must still advance by 1 (no stuck loop).
        let mut calls = 0_u32;
        let r = collect_placement_blocks_for_gaps(
            &[(PartitionChunkOffset::from(0), PartitionChunkOffset::from(3))],
            0,
            100,
            PartitionChunkOffset::from(100),
            |ledger_abs| {
                calls += 1;
                Some(PlacementSpan {
                    height: ledger_abs, // unique per offset
                    block_hash: hash(ledger_abs as u8),
                    start_chunk_offset: ledger_abs,
                    // Degenerate: end == start (empty span)
                    end_chunk_offset: ledger_abs,
                })
            },
        );
        assert_eq!(calls, 3);
        assert_eq!(r.placement_blocks.len(), 3);
        assert_eq!(r.bounds_lookups, 3);
    }

    #[test]
    fn bounds_lookup_cap_stops_failed_lookup_storm() {
        // Large hole of all soft-skips would otherwise scan every offset.
        let gap_end = PartitionChunkOffset::from(10_000_u32);
        let mut calls = 0_u64;
        let r = collect_placement_blocks_for_gaps(
            &[(PartitionChunkOffset::from(0), gap_end)],
            0,
            100_000,
            gap_end,
            |_ledger_abs| {
                calls += 1;
                None
            },
        );
        assert_eq!(calls, INDEX_HEAL_MAX_BLOCKS_PER_PASS as u64);
        assert_eq!(r.bounds_lookups, INDEX_HEAL_MAX_BLOCKS_PER_PASS as u64);
        assert!(r.any_soft_skip);
        assert!(r.placement_blocks.is_empty());
    }

    #[test]
    fn bounds_lookup_cap_preserves_jump_and_marks_partial() {
        // Each success jumps by 1 chunk (end == ledger+1). Cap still applies to
        // the total lookup count; leftover work is soft-skipped for needs_retry.
        let mut calls = 0_u64;
        let r = collect_placement_blocks_for_gaps(
            &[(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(500),
            )],
            0,
            10_000,
            PartitionChunkOffset::from(500),
            |ledger_abs| {
                calls += 1;
                Some(PlacementSpan {
                    height: ledger_abs,
                    block_hash: hash((ledger_abs % 256) as u8),
                    start_chunk_offset: ledger_abs,
                    end_chunk_offset: ledger_abs.saturating_add(1),
                })
            },
        );
        assert_eq!(calls, INDEX_HEAL_MAX_BLOCKS_PER_PASS as u64);
        assert_eq!(r.placement_blocks.len(), INDEX_HEAL_MAX_BLOCKS_PER_PASS);
        // Cap hit mid-hole → partial so next heal continues.
        assert!(r.any_soft_skip);
    }
}
