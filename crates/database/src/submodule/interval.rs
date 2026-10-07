//! Tx-path coverage for one submodule.
//!
//! One placement is one inclusive interval. Intervals in a submodule do not
//! overlap. The same tx hash on two touching spans is one row.

use irys_types::{H256, PartitionChunkOffset};

use super::tables::ChunkPathHashes;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct IntervalRow {
    pub start: PartitionChunkOffset,
    pub end: PartitionChunkOffset,
    pub tx_path_hash: H256,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct IntervalEdit {
    pub delete: Vec<PartitionChunkOffset>,
    pub put: Vec<IntervalRow>,
}

/// Split, replace, and merge `existing` so `[start, end]` has `tx_path_hash`.
///
/// `None` removes coverage. `existing` is the overlapping rows plus the
/// adjacent neighbors. Rows outside that set are left alone.
pub(super) fn plan_coverage(
    existing: &[IntervalRow],
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
    tx_path_hash: Option<H256>,
) -> IntervalEdit {
    if start > end {
        return IntervalEdit {
            delete: Vec::new(),
            put: Vec::new(),
        };
    }

    let mut kept = Vec::new();
    for row in existing {
        if row.end < start || row.start > end {
            kept.push(*row);
            continue;
        }
        if row.start < start {
            kept.push(IntervalRow {
                start: row.start,
                end: PartitionChunkOffset(start.0 - 1),
                tx_path_hash: row.tx_path_hash,
            });
        }
        if row.end > end {
            kept.push(IntervalRow {
                start: PartitionChunkOffset(end.0 + 1),
                end: row.end,
                tx_path_hash: row.tx_path_hash,
            });
        }
    }
    if let Some(tx_path_hash) = tx_path_hash {
        kept.push(IntervalRow {
            start,
            end,
            tx_path_hash,
        });
    }

    let merged = merge_adjacent(kept);
    diff(existing, &merged)
}

/// Half-open holes in `[start, end)` that no interval covers.
pub(super) fn coverage_gaps(
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
    intervals: &[IntervalRow],
) -> Vec<(PartitionChunkOffset, PartitionChunkOffset)> {
    if start >= end {
        return Vec::new();
    }
    let mut gaps = Vec::new();
    let mut expected = start.0;
    let window_end = end.0;
    for row in intervals {
        if row.end.0 < start.0 {
            continue;
        }
        if row.start.0 >= window_end {
            break;
        }
        let cover_from = row.start.0.max(start.0);
        if cover_from > expected {
            gaps.push((
                PartitionChunkOffset(expected),
                PartitionChunkOffset(cover_from),
            ));
        }
        let next = row.end.0.saturating_add(1);
        if next > expected {
            expected = next;
        }
        if expected >= window_end {
            return gaps;
        }
    }
    if expected < window_end {
        gaps.push((PartitionChunkOffset(expected), end));
    }
    gaps
}

/// Tx hash comes from the interval. The offset row only contributes its data-path hash.
pub(super) fn combine_hashes(
    stored: Option<ChunkPathHashes>,
    tx_path_hash: Option<H256>,
) -> Option<ChunkPathHashes> {
    let data_path_hash = stored.and_then(|hashes| hashes.data_path_hash);
    if data_path_hash.is_none() && tx_path_hash.is_none() {
        None
    } else {
        Some(ChunkPathHashes {
            data_path_hash,
            tx_path_hash,
        })
    }
}

fn merge_adjacent(mut rows: Vec<IntervalRow>) -> Vec<IntervalRow> {
    rows.sort_by_key(|row| row.start);
    let mut out: Vec<IntervalRow> = Vec::new();
    for row in rows {
        if let Some(prev) = out.last_mut()
            && prev.tx_path_hash == row.tx_path_hash
            && prev.end.0.checked_add(1) == Some(row.start.0)
        {
            prev.end = row.end;
            continue;
        }
        out.push(row);
    }
    out
}

fn diff(existing: &[IntervalRow], merged: &[IntervalRow]) -> IntervalEdit {
    let mut delete = Vec::new();
    let mut put = Vec::new();
    let mut old: Vec<&IntervalRow> = existing.iter().collect();
    for row in merged {
        if let Some(pos) = old
            .iter()
            .position(|candidate| candidate.start == row.start)
        {
            let previous = old.remove(pos);
            if previous.end != row.end || previous.tx_path_hash != row.tx_path_hash {
                put.push(*row);
            }
        } else {
            put.push(*row);
        }
    }
    for row in old {
        delete.push(row.start);
    }
    delete.sort_by_key(|start| start.0);
    put.sort_by_key(|row| row.start);
    IntervalEdit { delete, put }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(start: u32, end: u32, byte: u8) -> IntervalRow {
        IntervalRow {
            start: PartitionChunkOffset(start),
            end: PartitionChunkOffset(end),
            tx_path_hash: H256::repeat_byte(byte),
        }
    }

    fn at(offset: u32) -> PartitionChunkOffset {
        PartitionChunkOffset(offset)
    }

    #[test]
    fn empty_span_is_one_put() {
        let edit = plan_coverage(&[], at(2), at(4), Some(H256::repeat_byte(1)));
        assert!(edit.delete.is_empty());
        assert_eq!(edit.put, vec![row(2, 4, 1)]);
    }

    #[test]
    fn same_span_and_hash_is_a_no_op() {
        let edit = plan_coverage(&[row(2, 4, 1)], at(2), at(4), Some(H256::repeat_byte(1)));
        assert!(edit.delete.is_empty());
        assert!(edit.put.is_empty());
    }

    #[test]
    fn a_different_hash_splits_the_middle() {
        let edit = plan_coverage(&[row(0, 4, 1)], at(2), at(2), Some(H256::repeat_byte(2)));
        // The left remnant keeps start 0, so the old key is overwritten.
        assert!(edit.delete.is_empty());
        assert_eq!(edit.put, vec![row(0, 1, 1), row(2, 2, 2), row(3, 4, 1)]);
    }

    #[test]
    fn the_same_hash_collapses_back_to_one_row() {
        let edit = plan_coverage(&[row(0, 4, 1)], at(2), at(2), Some(H256::repeat_byte(1)));
        assert!(edit.delete.is_empty());
        assert!(edit.put.is_empty());
    }

    #[test]
    fn clearing_the_middle_leaves_both_sides() {
        let edit = plan_coverage(&[row(0, 4, 1)], at(2), at(2), None);
        assert!(edit.delete.is_empty());
        assert_eq!(edit.put, vec![row(0, 1, 1), row(3, 4, 1)]);
    }

    #[test]
    fn adjacent_spans_with_one_hash_merge() {
        let edit = plan_coverage(
            &[row(0, 1, 1), row(3, 4, 1)],
            at(2),
            at(2),
            Some(H256::repeat_byte(1)),
        );
        // Start 0 is overwritten with the merged span. Start 3 is gone.
        assert_eq!(edit.delete, vec![at(3)]);
        assert_eq!(edit.put, vec![row(0, 4, 1)]);
    }

    #[test]
    fn gaps_follow_coverage() {
        assert!(coverage_gaps(at(0), at(5), &[row(0, 4, 1)]).is_empty());
        assert_eq!(
            coverage_gaps(at(0), at(5), &[row(0, 1, 1), row(3, 4, 1)]),
            vec![(at(2), at(3))]
        );
        assert_eq!(coverage_gaps(at(5), at(10), &[]), vec![(at(5), at(10))]);
        assert!(coverage_gaps(at(5), at(10), &[row(0, 9, 1)]).is_empty());
    }

    #[test]
    fn combine_ignores_a_tx_hash_stored_on_the_offset_row() {
        let stored = ChunkPathHashes {
            data_path_hash: Some(H256::repeat_byte(4)),
            tx_path_hash: Some(H256::repeat_byte(9)),
        };
        let combined = combine_hashes(Some(stored), Some(H256::repeat_byte(3))).unwrap();
        assert_eq!(combined.data_path_hash, Some(H256::repeat_byte(4)));
        assert_eq!(combined.tx_path_hash, Some(H256::repeat_byte(3)));
        assert!(combine_hashes(None, None).is_none());
    }
}
