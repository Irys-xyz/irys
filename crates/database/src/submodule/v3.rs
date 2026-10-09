//! RocksDB schema v3 records.
//!
//! One fixed big-endian layout. No SCALE and no Compact. A row belongs to
//! one placement: the data-root point key is removed in the same batch as
//! its interval.

use irys_types::{H256, RelativeChunkOffset};

use super::tables::PendingBodyMigration;

pub(super) const PLACEMENT_LEN: usize = 64;
pub(super) const DATA_ROOT_KEY_LEN: usize = 36;
pub(super) const DATA_ROOT_VALUE_LEN: usize = 16;
pub(super) const PENDING_LEN: usize = 56;
/// Stored when the writer did not name a ledger, a height, or a tx index.
pub(super) const UNSET_U32: u32 = u32::MAX;
pub(super) const UNSET_U64: u64 = u64::MAX;

/// Column families this process opens for one index directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IndexLayout {
    /// Compact rows, including stored tx paths. MDBX, and a RocksDB
    /// directory whose schema file is still version 2.
    V2,
    /// Fixed records. Tx paths are not stored.
    V3,
}

/// Locator copied onto intervals assigned after [`IndexLayout::V3`] sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PlacementOrigin {
    pub ledger: u32,
    pub block_height: u64,
    pub tx_index: u32,
}

/// Inclusive placement stored under its start offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct V3Placement {
    pub end: u32,
    pub data_root: [u8; 32],
    pub tx_start: i32,
    pub data_size: u64,
    pub ledger: u32,
    pub block_height: u64,
    pub tx_index: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct V3Interval {
    pub start: u32,
    pub value: V3Placement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DataRootPlacement {
    pub end: u32,
    pub tx_start: i32,
    pub data_size: u64,
}

pub(super) fn encode_placement(value: &V3Placement) -> [u8; PLACEMENT_LEN] {
    let mut out = [0_u8; PLACEMENT_LEN];
    out[0..4].copy_from_slice(&value.end.to_be_bytes());
    out[4..36].copy_from_slice(&value.data_root);
    out[36..40].copy_from_slice(&value.tx_start.to_be_bytes());
    out[40..48].copy_from_slice(&value.data_size.to_be_bytes());
    out[48..52].copy_from_slice(&value.ledger.to_be_bytes());
    out[52..60].copy_from_slice(&value.block_height.to_be_bytes());
    out[60..64].copy_from_slice(&value.tx_index.to_be_bytes());
    out
}

pub(super) fn decode_placement(bytes: &[u8]) -> eyre::Result<V3Placement> {
    if bytes.len() != PLACEMENT_LEN {
        eyre::bail!(
            "placement interval value is {} bytes, expected {PLACEMENT_LEN}",
            bytes.len()
        );
    }
    Ok(V3Placement {
        end: u32::from_be_bytes(bytes[0..4].try_into().expect("4")),
        data_root: bytes[4..36].try_into().expect("32"),
        tx_start: i32::from_be_bytes(bytes[36..40].try_into().expect("4")),
        data_size: u64::from_be_bytes(bytes[40..48].try_into().expect("8")),
        ledger: u32::from_be_bytes(bytes[48..52].try_into().expect("4")),
        block_height: u64::from_be_bytes(bytes[52..60].try_into().expect("8")),
        tx_index: u32::from_be_bytes(bytes[60..64].try_into().expect("4")),
    })
}

pub(super) fn data_root_key(data_root: &H256, start: u32) -> [u8; DATA_ROOT_KEY_LEN] {
    let mut key = [0_u8; DATA_ROOT_KEY_LEN];
    key[..32].copy_from_slice(&data_root.0);
    key[32..].copy_from_slice(&start.to_be_bytes());
    key
}

pub(super) fn decode_data_root_key(bytes: &[u8]) -> eyre::Result<(H256, u32)> {
    if bytes.len() != DATA_ROOT_KEY_LEN {
        eyre::bail!(
            "data-root placement key is {} bytes, expected {DATA_ROOT_KEY_LEN}",
            bytes.len()
        );
    }
    let mut root = [0_u8; 32];
    root.copy_from_slice(&bytes[..32]);
    let start = u32::from_be_bytes(bytes[32..].try_into().expect("4"));
    Ok((H256(root), start))
}

pub(super) fn encode_data_root(value: &DataRootPlacement) -> [u8; DATA_ROOT_VALUE_LEN] {
    let mut out = [0_u8; DATA_ROOT_VALUE_LEN];
    out[0..4].copy_from_slice(&value.end.to_be_bytes());
    out[4..8].copy_from_slice(&value.tx_start.to_be_bytes());
    out[8..16].copy_from_slice(&value.data_size.to_be_bytes());
    out
}

pub(super) fn decode_data_root(bytes: &[u8]) -> eyre::Result<DataRootPlacement> {
    if bytes.len() != DATA_ROOT_VALUE_LEN {
        eyre::bail!(
            "data-root placement value is {} bytes, expected {DATA_ROOT_VALUE_LEN}",
            bytes.len()
        );
    }
    Ok(DataRootPlacement {
        end: u32::from_be_bytes(bytes[0..4].try_into().expect("4")),
        tx_start: i32::from_be_bytes(bytes[4..8].try_into().expect("4")),
        data_size: u64::from_be_bytes(bytes[8..16].try_into().expect("8")),
    })
}

pub(super) fn encode_pending(job: &PendingBodyMigration) -> [u8; PENDING_LEN] {
    let mut out = [0_u8; PENDING_LEN];
    out[0..32].copy_from_slice(&job.data_root.0);
    out[32..40].copy_from_slice(&job.data_size.to_be_bytes());
    out[40..44].copy_from_slice(&job.start_offset.0.to_be_bytes());
    out[44..52].copy_from_slice(&job.block_height.to_be_bytes());
    out[52..56].copy_from_slice(&job.attempts.to_be_bytes());
    out
}

pub(super) fn decode_pending(bytes: &[u8]) -> eyre::Result<PendingBodyMigration> {
    if bytes.len() != PENDING_LEN {
        eyre::bail!(
            "pending body value is {} bytes, expected {PENDING_LEN}",
            bytes.len()
        );
    }
    let mut data_root = [0_u8; 32];
    data_root.copy_from_slice(&bytes[..32]);
    Ok(PendingBodyMigration {
        data_root: H256(data_root),
        data_size: u64::from_be_bytes(bytes[32..40].try_into().expect("8")),
        start_offset: RelativeChunkOffset(i32::from_be_bytes(bytes[40..44].try_into().expect("4"))),
        block_height: u64::from_be_bytes(bytes[44..52].try_into().expect("8")),
        attempts: u32::from_be_bytes(bytes[52..56].try_into().expect("4")),
    })
}

pub(super) fn fresh_placement(
    end: u32,
    data_root: H256,
    origin: Option<PlacementOrigin>,
) -> V3Placement {
    let (ledger, block_height, tx_index) = match origin {
        Some(origin) => (origin.ledger, origin.block_height, origin.tx_index),
        None => (UNSET_U32, UNSET_U64, UNSET_U32),
    };
    V3Placement {
        end,
        data_root: data_root.0,
        tx_start: 0,
        data_size: 0,
        ledger,
        block_height,
        tx_index,
    }
}

/// A placement still waiting for its `DataRootInfo` in this batch.
pub(super) fn awaits_info(value: &V3Placement) -> bool {
    value.tx_start == 0 && value.data_size == 0 && value.data_root != [0; 32]
}

pub(super) fn same_identity(left: &V3Placement, right: &V3Placement) -> bool {
    left.data_root == right.data_root
        && left.tx_start == right.tx_start
        && left.data_size == right.data_size
        && left.ledger == right.ledger
        && left.block_height == right.block_height
        && left.tx_index == right.tx_index
        && left.data_root != [0; 32]
}

/// Split, replace, and merge so `[start, end]` has `next`.
///
/// `None` removes coverage. Fragments keep the parent record. Adjacent rows
/// merge when every identity field matches. An all-zero data root does not
/// merge: the caller did not name a placement.
pub(super) fn plan_v3(
    existing: &[V3Interval],
    start: u32,
    end: u32,
    next: Option<V3Placement>,
) -> (Vec<u32>, Vec<V3Interval>) {
    if start > end {
        return (Vec::new(), Vec::new());
    }
    let mut kept = Vec::new();
    for row in existing {
        if row.value.end < start || row.start > end {
            kept.push(*row);
            continue;
        }
        if row.start < start {
            let mut left = *row;
            left.value.end = start - 1;
            kept.push(left);
        }
        if row.value.end > end {
            let mut right = *row;
            right.start = end + 1;
            kept.push(right);
        }
    }
    if let Some(mut value) = next {
        value.end = end;
        kept.push(V3Interval { start, value });
    }
    let merged = merge_adjacent(kept);
    diff(existing, &merged)
}

fn merge_adjacent(mut rows: Vec<V3Interval>) -> Vec<V3Interval> {
    rows.sort_by_key(|row| row.start);
    let mut out: Vec<V3Interval> = Vec::new();
    for row in rows {
        if let Some(prev) = out.last_mut()
            && same_identity(&prev.value, &row.value)
            && prev.value.end.checked_add(1) == Some(row.start)
        {
            prev.value.end = row.value.end;
            continue;
        }
        out.push(row);
    }
    out
}

fn diff(existing: &[V3Interval], merged: &[V3Interval]) -> (Vec<u32>, Vec<V3Interval>) {
    let mut delete = Vec::new();
    let mut put = Vec::new();
    let mut old: Vec<&V3Interval> = existing.iter().collect();
    for row in merged {
        if let Some(pos) = old
            .iter()
            .position(|candidate| candidate.start == row.start)
        {
            let previous = old.remove(pos);
            if previous.value != row.value {
                put.push(*row);
            }
        } else {
            put.push(*row);
        }
    }
    for row in old {
        delete.push(row.start);
    }
    delete.sort_unstable();
    put.sort_by_key(|row| row.start);
    (delete, put)
}

pub(super) fn optional_u32(value: u32) -> Option<u32> {
    (value != UNSET_U32).then_some(value)
}

pub(super) fn optional_u64(value: u64) -> Option<u64> {
    (value != UNSET_U64).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(end: u32, byte: u8, size: u64) -> V3Placement {
        V3Placement {
            end,
            data_root: [byte; 32],
            tx_start: 0,
            data_size: size,
            ledger: 1,
            block_height: 4,
            tx_index: 2,
        }
    }

    #[test]
    fn placement_bytes_round_trip() {
        let value = V3Placement {
            end: 9,
            data_root: [7; 32],
            tx_start: -3,
            data_size: 99,
            ledger: 20,
            block_height: 1_000,
            tx_index: 4,
        };
        assert_eq!(decode_placement(&encode_placement(&value)).unwrap(), value);
    }

    #[test]
    fn adjacent_same_identity_is_one_interval() {
        let left = V3Interval {
            start: 0,
            value: placed(4, 1, 10),
        };
        let (delete, put) = plan_v3(&[left], 5, 9, Some(placed(0, 1, 10)));
        assert!(delete.is_empty());
        assert_eq!(put.len(), 1);
        assert_eq!(put[0].start, 0);
        assert_eq!(put[0].value.end, 9);
    }

    #[test]
    fn a_different_data_root_does_not_merge() {
        let left = V3Interval {
            start: 0,
            value: placed(4, 1, 10),
        };
        let (_delete, put) = plan_v3(&[left], 5, 9, Some(placed(0, 2, 10)));
        assert_eq!(put.len(), 1);
        assert_eq!(put[0].start, 5);
    }

    #[test]
    fn pending_bytes_round_trip() {
        let job = PendingBodyMigration {
            data_root: H256::repeat_byte(3),
            data_size: 64,
            start_offset: RelativeChunkOffset(-2),
            block_height: 8,
            attempts: 1,
        };
        assert_eq!(decode_pending(&encode_pending(&job)).unwrap(), job);
    }
}
