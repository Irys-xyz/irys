//! RocksDB v1 records.
//!
//! One fixed big-endian layout. No SCALE and no Compact. A row belongs to
//! one placement: the data-root point key is removed in the same batch as
//! its interval.

use irys_types::{H256, PartitionChunkOffset, RelativeChunkOffset};

use super::tables::{OffsetMeta, PendingBodyMigration};

/// Format byte at the end of a RocksDB v1 offset value.
const OFFSET_META_FORMAT: u8 = 1;
/// The value begins with a data proof.
const OFFSET_FLAG_HAS_PROOF: u8 = 0x01;
/// The trailer contains the CRC32C of the packed chunk.
const OFFSET_FLAG_CRC_PRESENT: u8 = 0x02;

/// Encode one offset row: proof bytes (when stored), then a trailer of the
/// CRC32C (little-endian, when the flag is set), the flags byte, and
/// [`OFFSET_META_FORMAT`]. A stored CRC of zero is a real checksum; unknown
/// is the flag being unset. The leaf is the last 64 bytes of the proof, not
/// of this value.
///
/// Why the metadata is a trailer, not a header: neighbouring chunks' proofs
/// share their upper merkle branches, so consecutive values in an SST block
/// begin with long identical runs. RocksDB delta-encodes keys, not values, so
/// that sharing is only recovered by block compression (LZ4), which finds it
/// most cheaply when every value starts with it. Measured after compaction
/// (`submodule_index_bench --profile crc-rows`, which still writes both
/// layouts): CRC as trailer costs +5.77 B/chunk on mid-size shared proofs and
/// +4.10 on the largest; as a prefix it costs +1.09 and +2.86 B/chunk more.
/// Prefix wins only on unrelated (uniform) proofs, by ~1.06 B/chunk, which is
/// not the stored shape. The format byte can sit last because RocksDB values
/// are length-delimited. The schema marker names the layout. These bytes do not.
pub fn encode_offset_meta(meta: &OffsetMeta) -> eyre::Result<Vec<u8>> {
    let proof = meta.proof.as_deref().unwrap_or(&[]);
    let has_proof = meta.proof.is_some();
    let has_crc = meta.crc.is_some();
    if !has_proof && !has_crc {
        eyre::bail!("offset row needs a proof or a checksum");
    }
    if has_proof && proof.is_empty() {
        eyre::bail!("offset row proof is empty");
    }
    let mut out = Vec::with_capacity(proof.len() + 6);
    if has_proof {
        out.extend_from_slice(proof);
    }
    let mut flags = 0_u8;
    if has_proof {
        flags |= OFFSET_FLAG_HAS_PROOF;
    }
    if let Some(crc) = meta.crc {
        flags |= OFFSET_FLAG_CRC_PRESENT;
        out.extend_from_slice(&crc.to_le_bytes());
    }
    out.push(flags);
    out.push(OFFSET_META_FORMAT);
    Ok(out)
}

/// Decode one RocksDB v1 offset value.
pub fn decode_offset_meta(bytes: &[u8]) -> eyre::Result<OffsetMeta> {
    if bytes.len() < 2 {
        eyre::bail!("offset row is {} bytes", bytes.len());
    }
    let format = bytes[bytes.len() - 1];
    if format != OFFSET_META_FORMAT {
        eyre::bail!("offset row format {format} is not {OFFSET_META_FORMAT}");
    }
    let flags = bytes[bytes.len() - 2];
    if flags & !(OFFSET_FLAG_HAS_PROOF | OFFSET_FLAG_CRC_PRESENT) != 0 {
        eyre::bail!("offset row flags {flags:#x} are not known");
    }
    let has_proof = flags & OFFSET_FLAG_HAS_PROOF != 0;
    let has_crc = flags & OFFSET_FLAG_CRC_PRESENT != 0;
    if !has_proof && !has_crc {
        eyre::bail!("offset row has neither a proof nor a checksum");
    }
    let mut head = bytes.len() - 2;
    let crc = if has_crc {
        if head < 4 {
            eyre::bail!("offset row checksum is truncated");
        }
        head -= 4;
        let mut word = [0_u8; 4];
        word.copy_from_slice(&bytes[head..head + 4]);
        Some(u32::from_le_bytes(word))
    } else {
        None
    };
    let proof = if has_proof {
        if head == 0 {
            eyre::bail!("offset row proof is empty");
        }
        Some(bytes[..head].to_vec())
    } else if head != 0 {
        eyre::bail!("offset row has {head} bytes before a checksum-only trailer");
    } else {
        None
    };
    Ok(OffsetMeta { crc, proof })
}

pub(super) const PLACEMENT_LEN: usize = 64;
pub(super) const DATA_ROOT_KEY_LEN: usize = 36;
/// `content_hash` (32 bytes) then partition offset (u32 big-endian).
pub(super) const CONTENT_KEY_LEN: usize = 36;
pub(super) const DATA_ROOT_VALUE_LEN: usize = 16;
pub(super) const PENDING_LEN: usize = 56;
/// Stored when the writer did not name a ledger, a height, or a tx index.
pub(super) const UNSET_U32: u32 = u32::MAX;
pub(super) const UNSET_U64: u64 = u64::MAX;

/// The only RocksDB layout this process opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IndexLayout {
    /// Fixed records. Tx paths are not stored.
    RocksV1,
}

/// Locator copied onto intervals assigned after a RocksDB v1 open sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PlacementOrigin {
    pub ledger: u32,
    pub block_height: u64,
    pub tx_index: u32,
}

/// Inclusive placement stored under its start offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PlacementRecord {
    pub end: u32,
    pub data_root: [u8; 32],
    pub tx_start: i32,
    pub data_size: u64,
    pub ledger: u32,
    pub block_height: u64,
    pub tx_index: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PlacementSpan {
    pub start: u32,
    pub value: PlacementRecord,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DataRootPlacement {
    pub end: u32,
    pub tx_start: i32,
    pub data_size: u64,
}

pub(super) fn encode_placement(value: &PlacementRecord) -> [u8; PLACEMENT_LEN] {
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

pub(super) fn decode_placement(bytes: &[u8]) -> eyre::Result<PlacementRecord> {
    if bytes.len() != PLACEMENT_LEN {
        eyre::bail!(
            "placement interval value is {} bytes, expected {PLACEMENT_LEN}",
            bytes.len()
        );
    }
    Ok(PlacementRecord {
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

pub(super) fn content_key(hash: &H256, offset: PartitionChunkOffset) -> [u8; CONTENT_KEY_LEN] {
    let mut key = [0_u8; CONTENT_KEY_LEN];
    key[..32].copy_from_slice(&hash.0);
    key[32..].copy_from_slice(&offset.0.to_be_bytes());
    key
}

pub(super) fn decode_content_key(bytes: &[u8]) -> Option<(H256, PartitionChunkOffset)> {
    if bytes.len() != CONTENT_KEY_LEN {
        return None;
    }
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(&bytes[..32]);
    let offset = u32::from_be_bytes(bytes[32..].try_into().expect("4"));
    Some((H256(hash), PartitionChunkOffset(offset)))
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
) -> PlacementRecord {
    let (ledger, block_height, tx_index) = match origin {
        Some(origin) => (origin.ledger, origin.block_height, origin.tx_index),
        None => (UNSET_U32, UNSET_U64, UNSET_U32),
    };
    PlacementRecord {
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
pub(super) fn awaits_info(value: &PlacementRecord) -> bool {
    value.tx_start == 0 && value.data_size == 0 && value.data_root != [0; 32]
}

pub(super) fn same_identity(left: &PlacementRecord, right: &PlacementRecord) -> bool {
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
pub(super) fn plan_placement(
    existing: &[PlacementSpan],
    start: u32,
    end: u32,
    next: Option<PlacementRecord>,
) -> (Vec<u32>, Vec<PlacementSpan>) {
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
        kept.push(PlacementSpan { start, value });
    }
    let merged = merge_adjacent(kept);
    diff(existing, &merged)
}

fn merge_adjacent(mut rows: Vec<PlacementSpan>) -> Vec<PlacementSpan> {
    rows.sort_by_key(|row| row.start);
    let mut out: Vec<PlacementSpan> = Vec::new();
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

fn diff(existing: &[PlacementSpan], merged: &[PlacementSpan]) -> (Vec<u32>, Vec<PlacementSpan>) {
    let mut delete = Vec::new();
    let mut put = Vec::new();
    let mut old: Vec<&PlacementSpan> = existing.iter().collect();
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

    #[test]
    fn offset_meta_round_trip_keeps_the_proof_as_a_prefix() {
        let proof = vec![0x11_u8; 80];
        let meta = OffsetMeta {
            crc: Some(0),
            proof: Some(proof.clone()),
        };
        let bytes = encode_offset_meta(&meta).unwrap();
        assert_eq!(&bytes[..proof.len()], proof.as_slice());
        assert_eq!(bytes[bytes.len() - 1], OFFSET_META_FORMAT);
        assert_eq!(
            bytes[bytes.len() - 2],
            OFFSET_FLAG_HAS_PROOF | OFFSET_FLAG_CRC_PRESENT
        );
        assert_eq!(decode_offset_meta(&bytes).unwrap(), meta);

        let crc_only = OffsetMeta {
            crc: Some(7),
            proof: None,
        };
        let only = encode_offset_meta(&crc_only).unwrap();
        assert_eq!(only.len(), 6);
        assert_eq!(decode_offset_meta(&only).unwrap(), crc_only);

        let proof_only = OffsetMeta {
            crc: None,
            proof: Some(b"proof".to_vec()),
        };
        assert_eq!(
            decode_offset_meta(&encode_offset_meta(&proof_only).unwrap()).unwrap(),
            proof_only
        );
        assert!(encode_offset_meta(&OffsetMeta::default()).is_err());
        assert!(decode_offset_meta(&[1, 0]).is_err());
        assert!(decode_offset_meta(&[OFFSET_FLAG_HAS_PROOF, OFFSET_META_FORMAT]).is_err());
    }

    fn placed(end: u32, byte: u8, size: u64) -> PlacementRecord {
        PlacementRecord {
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
        let value = PlacementRecord {
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
        let left = PlacementSpan {
            start: 0,
            value: placed(4, 1, 10),
        };
        let (delete, put) = plan_placement(&[left], 5, 9, Some(placed(0, 1, 10)));
        assert!(delete.is_empty());
        assert_eq!(put.len(), 1);
        assert_eq!(put[0].start, 0);
        assert_eq!(put[0].value.end, 9);
    }

    #[test]
    fn a_different_data_root_does_not_merge() {
        let left = PlacementSpan {
            start: 0,
            value: placed(4, 1, 10),
        };
        let (_delete, put) = plan_placement(&[left], 5, 9, Some(placed(0, 2, 10)));
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
