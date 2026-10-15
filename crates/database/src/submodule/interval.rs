//! Tx-path coverage for one submodule.
//!
//! One placement is one inclusive interval. Intervals in a submodule do not
//! overlap. The same tx hash on two touching spans is one row.

use std::collections::{BTreeMap, HashSet};

use irys_types::{H256, PartitionChunkOffset};

use super::tables::ChunkPathHashes;

/// Inclusive tx-path coverage in one submodule, and the hash of the stored path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntervalRow {
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

#[cfg(test)]
thread_local! {
    static DIFF_MERGED_ROWS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_diff_merged_rows() {
    DIFF_MERGED_ROWS.with(|cell| cell.set(0));
}

#[cfg(test)]
fn diff_merged_rows() -> usize {
    DIFF_MERGED_ROWS.with(|cell| cell.get())
}

#[cfg(test)]
fn note_diff_merged_row() {
    DIFF_MERGED_ROWS.with(|cell| cell.set(cell.get().saturating_add(1)));
}

fn diff(existing: &[IntervalRow], merged: &[IntervalRow]) -> IntervalEdit {
    let mut old: BTreeMap<PartitionChunkOffset, IntervalRow> =
        existing.iter().map(|row| (row.start, *row)).collect();
    let mut put = Vec::new();
    for row in merged {
        #[cfg(test)]
        note_diff_merged_row();
        match old.remove(&row.start) {
            Some(previous)
                if previous.end == row.end && previous.tx_path_hash == row.tx_path_hash => {}
            Some(_) | None => put.push(*row),
        }
    }
    let delete: Vec<_> = old.into_keys().collect();
    put.sort_by_key(|row| row.start);
    IntervalEdit { delete, put }
}

/// Hashes that this edit removed from its window.
///
/// A hash is included when no `edit.put` row and no unchanged `existing` row
/// still names it. An interval outside `existing` can still name the hash.
/// The caller scans the submodule before it deletes the path.
pub(super) fn dropped_tx_hashes(existing: &[IntervalRow], edit: &IntervalEdit) -> Vec<H256> {
    let deleted: HashSet<PartitionChunkOffset> = edit.delete.iter().copied().collect();
    let put_at: BTreeMap<PartitionChunkOffset, H256> = edit
        .put
        .iter()
        .map(|row| (row.start, row.tx_path_hash))
        .collect();
    // A put names the hash, and so does a row the edit did not touch.
    let mut kept: HashSet<H256> = edit.put.iter().map(|row| row.tx_path_hash).collect();
    for row in existing {
        if deleted.contains(&row.start) || put_at.contains_key(&row.start) {
            continue;
        }
        kept.insert(row.tx_path_hash);
    }
    let mut dropped = Vec::new();
    let mut seen = HashSet::new();
    for row in existing {
        let replaced = put_at
            .get(&row.start)
            .is_some_and(|hash| *hash != row.tx_path_hash);
        let removed = deleted.contains(&row.start) || replaced;
        if !removed || !seen.insert(row.tx_path_hash) {
            continue;
        }
        if !kept.contains(&row.tx_path_hash) {
            dropped.push(row.tx_path_hash);
        }
    }
    dropped
}

/// Overlapping or touching inclusive spans become one span. Inverted spans drop.
///
/// Touching means the next start is `end + 1`. A gap stays a gap: the offset
/// in that gap is still live and must not share a range tombstone.
pub(super) fn coalesce_inclusive_spans(
    spans: &[(PartitionChunkOffset, PartitionChunkOffset)],
) -> Vec<(PartitionChunkOffset, PartitionChunkOffset)> {
    let mut ordered: Vec<(u32, u32)> = spans
        .iter()
        .filter(|(start, end)| start.0 <= end.0)
        .map(|(start, end)| (start.0, end.0))
        .collect();
    ordered.sort_unstable();
    let mut out: Vec<(u32, u32)> = Vec::new();
    for (start, end) in ordered {
        if let Some(prev) = out.last_mut() {
            let touches = match prev.1.checked_add(1) {
                Some(next) => start <= next,
                // `prev` already ends at `u32::MAX`, so it covers every later offset.
                None => true,
            };
            if touches {
                if end > prev.1 {
                    prev.1 = end;
                }
                continue;
            }
        }
        out.push((start, end));
    }
    out.into_iter()
        .map(|(start, end)| (PartitionChunkOffset(start), PartitionChunkOffset(end)))
        .collect()
}

/// Span index that contains `offset`. `spans` is the output of [`coalesce_inclusive_spans`].
#[cfg(test)]
pub(super) fn coalesced_span_index(
    spans: &[(PartitionChunkOffset, PartitionChunkOffset)],
    offset: u32,
) -> Option<usize> {
    let idx = spans.partition_point(|(start, _)| start.0 <= offset);
    idx.checked_sub(1)
        .filter(|&index| spans[index].1.0 >= offset)
}

/// Remove every coalesced span from `existing` in one pass.
///
/// A hole that ends at `u32::MAX` has no right remnant. Adjacent rows with
/// the same tx hash still merge, including a remnant that touches a neighbor
/// the hole did not cover.
pub(super) fn clear_coverage(
    existing: &[IntervalRow],
    spans: &[(PartitionChunkOffset, PartitionChunkOffset)],
) -> IntervalEdit {
    let spans = coalesce_inclusive_spans(spans);
    if spans.is_empty() || existing.is_empty() {
        return IntervalEdit {
            delete: Vec::new(),
            put: Vec::new(),
        };
    }
    let mut rows = existing.to_vec();
    rows.sort_by_key(|row| row.start);
    let mut kept = Vec::new();
    let mut span_index = 0_usize;
    for row in &rows {
        while span_index < spans.len() && spans[span_index].1 < row.start {
            span_index += 1;
        }
        let mut cursor = row.start;
        let mut local = span_index;
        let mut alive = true;
        while local < spans.len() && spans[local].0 <= row.end {
            let (hole_start, hole_end) = spans[local];
            if hole_end < cursor {
                local += 1;
                continue;
            }
            if cursor < hole_start {
                kept.push(IntervalRow {
                    start: cursor,
                    end: PartitionChunkOffset(hole_start.0 - 1),
                    tx_path_hash: row.tx_path_hash,
                });
            }
            match hole_end.0.checked_add(1) {
                Some(next) if next <= row.end.0 => {
                    cursor = PartitionChunkOffset(next);
                    local += 1;
                }
                _ => {
                    alive = false;
                    break;
                }
            }
        }
        if alive && cursor <= row.end {
            kept.push(IntervalRow {
                start: cursor,
                end: row.end,
                tx_path_hash: row.tx_path_hash,
            });
        }
    }
    let merged = merge_adjacent(kept);
    diff(&rows, &merged)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

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
    fn a_cleared_span_drops_its_hash() {
        let existing = [row(0, 4, 1)];
        let edit = plan_coverage(&existing, at(0), at(4), None);
        assert_eq!(
            dropped_tx_hashes(&existing, &edit),
            vec![H256::repeat_byte(1)]
        );
    }

    #[test]
    fn a_split_keeps_the_old_hash_on_the_remnants() {
        let existing = [row(0, 4, 1)];
        let middle = plan_coverage(&existing, at(2), at(2), Some(H256::repeat_byte(2)));
        assert!(dropped_tx_hashes(&existing, &middle).is_empty());
        let left = plan_coverage(&existing, at(0), at(1), None);
        assert!(dropped_tx_hashes(&existing, &left).is_empty());
    }

    #[test]
    fn replacing_the_span_drops_the_old_hash_only() {
        let existing = [row(0, 4, 1)];
        let edit = plan_coverage(&existing, at(0), at(4), Some(H256::repeat_byte(2)));
        assert_eq!(
            dropped_tx_hashes(&existing, &edit),
            vec![H256::repeat_byte(1)]
        );
    }

    #[test]
    fn a_second_row_in_the_window_keeps_the_shared_hash() {
        let existing = [row(0, 1, 1), row(3, 4, 1)];
        let edit = plan_coverage(&existing, at(0), at(1), None);
        assert!(dropped_tx_hashes(&existing, &edit).is_empty());
        let merged = plan_coverage(&existing, at(2), at(2), Some(H256::repeat_byte(1)));
        assert!(dropped_tx_hashes(&existing, &merged).is_empty());
    }

    #[test]
    fn a_hash_missing_from_the_window_is_still_a_candidate() {
        // The far interval is not in `existing`. The scan, not this list,
        // decides whether the path stays.
        let existing = [row(0, 1, 1)];
        let edit = plan_coverage(&existing, at(0), at(1), None);
        assert_eq!(
            dropped_tx_hashes(&existing, &edit),
            vec![H256::repeat_byte(1)]
        );
    }

    #[test]
    fn touching_spans_coalesce_and_a_gap_stays_open() {
        let spans = coalesce_inclusive_spans(&[
            (at(3), at(4)),
            (at(0), at(1)),
            (at(8), at(7)),
            (at(2), at(2)),
        ]);
        assert_eq!(spans, vec![(at(0), at(4))]);
        let split = coalesce_inclusive_spans(&[(at(0), at(1)), (at(8), at(8)), (at(6), at(6))]);
        assert_eq!(split, vec![(at(0), at(1)), (at(6), at(6)), (at(8), at(8))]);
        assert_eq!(coalesced_span_index(&split, 6), Some(1));
        assert_eq!(coalesced_span_index(&split, 5), None);
        assert_eq!(coalesced_span_index(&split, 7), None);
        // A span that already ends at u32::MAX covers every later offset.
        // [1, 2] and [u32::MAX, u32::MAX] do not touch: the gap between them stays.
        let maxed = coalesce_inclusive_spans(&[(at(0), at(u32::MAX)), (at(5), at(5))]);
        assert_eq!(maxed, vec![(at(0), at(u32::MAX))]);
        let split_max = coalesce_inclusive_spans(&[(at(1), at(2)), (at(u32::MAX), at(u32::MAX))]);
        assert_eq!(
            split_max,
            vec![(at(1), at(2)), (at(u32::MAX), at(u32::MAX))]
        );
    }

    fn apply_edit(rows: &mut Vec<IntervalRow>, edit: &IntervalEdit) {
        rows.retain(|row| !edit.delete.contains(&row.start));
        for put in &edit.put {
            if let Some(slot) = rows.iter_mut().find(|row| row.start == put.start) {
                *slot = *put;
            } else {
                rows.push(*put);
            }
        }
        rows.sort_by_key(|row| row.start);
    }

    #[test]
    fn clear_coverage_matches_one_span_at_a_time() {
        let cases = [
            vec![row(0, 10, 1)],
            vec![row(0, 5, 1), row(6, 10, 1)],
            vec![row(0, 1, 1), row(3, 4, 2), row(8, 9, 1)],
            vec![row(0, u32::MAX, 1)],
        ];
        let punches = [
            vec![(at(2), at(4))],
            vec![(at(2), at(4)), (at(6), at(6))],
            vec![(at(0), at(10))],
            vec![(at(4), at(6)), (at(5), at(8))],
            vec![(at(u32::MAX), at(u32::MAX))],
        ];
        for existing in cases {
            for spans in &punches {
                let mut folded = existing.clone();
                for (start, end) in spans {
                    let edit = plan_coverage(&folded, *start, *end, None);
                    apply_edit(&mut folded, &edit);
                }
                let mut once = existing.clone();
                apply_edit(&mut once, &clear_coverage(&existing, spans));
                assert_eq!(once, folded, "spans {spans:?} existing {existing:?}");
            }
        }
    }

    #[test]
    fn clear_coverage_stays_linear_in_fragment_count() {
        let n = 20_000_u32;
        let existing: Vec<_> = (0..n).map(|i| row(i * 2, i * 2, (i % 200) as u8)).collect();
        let spans: Vec<_> = (0..n).map(|i| (at(i * 2), at(i * 2))).collect();
        // A full wipe deletes every row, so this call does not walk the merge.
        let edit = clear_coverage(&existing, &spans);
        assert_eq!(edit.delete.len(), n as usize);
        assert!(edit.put.is_empty());

        // The span sits past every point row. The merge keeps one row per
        // fragment, and each row matches the start already stored.
        // One step per merged row. A scan of every stored row for each
        // merged row counts n * n steps.
        let outside = n * 2 + 10;
        reset_diff_merged_rows();
        let started = Instant::now();
        let edit = clear_coverage(&existing, &[(at(outside), at(outside))]);
        let elapsed = started.elapsed();
        assert!(edit.delete.is_empty());
        assert!(edit.put.is_empty());
        assert_eq!(diff_merged_rows(), n as usize);
        assert!(
            elapsed < Duration::from_secs(2),
            "clear of {n} fragments took {elapsed:?}"
        );

        // The last row uses hash byte 199 and stays. Every other residue is
        // only on a deleted row, so the drop list has the other 199 hashes.
        let partial: Vec<_> = (0..n - 1).map(|i| (at(i * 2), at(i * 2))).collect();
        let started = Instant::now();
        let dropped = dropped_tx_hashes(&existing, &clear_coverage(&existing, &partial));
        let elapsed = started.elapsed();
        assert!(!dropped.contains(&H256::repeat_byte(199)));
        assert_eq!(dropped.len(), 199);
        assert!(
            elapsed < Duration::from_secs(2),
            "dropped-hash scan of {n} rows took {elapsed:?}"
        );
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
