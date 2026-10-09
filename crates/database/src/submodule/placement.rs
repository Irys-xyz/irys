//! Schema v2 placement reads.
//!
//! A placement is the tx-path interval, the data root recovered from the
//! stored path and its leaf binding, and the [`DataRootInfo`] that covers the
//! probe offset. v2 does not store a ledger, a block height, or a tx index.

use eyre::eyre;
use irys_types::{
    Base64, DataRoot, DataTransactionLedger, H256, PartitionChunkOffset, ProofDeserialize as _,
    RelativeChunkOffset, get_leaf_proof,
};

use super::interval::IntervalRow;
use super::store::SubmoduleRead;
use super::tables::{DataRootInfo, Placement};

pub(super) fn read_placement_at<R: SubmoduleRead + ?Sized>(
    reader: &R,
    offset: PartitionChunkOffset,
) -> eyre::Result<Option<Placement>> {
    let Some(interval) = reader.tx_interval_at(offset)? else {
        return Ok(None);
    };
    placement_for(reader, &interval, offset)
}

pub(super) fn read_placements_in_range<R: SubmoduleRead + ?Sized>(
    reader: &R,
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
) -> eyre::Result<Vec<Placement>> {
    if start > end {
        return Ok(Vec::new());
    }
    let intervals = reader.tx_intervals_in_range(start, end)?;
    let mut out = Vec::with_capacity(intervals.len());
    for interval in intervals {
        if let Some(placement) = placement_for(reader, &interval, interval.start)? {
            out.push(placement);
        }
    }
    Ok(out)
}

pub(super) fn read_placements_for_data_root<R: SubmoduleRead + ?Sized>(
    reader: &R,
    data_root: DataRoot,
) -> eyre::Result<Vec<Placement>> {
    let Some(infos) = reader.get_data_root_infos_for_data_root(data_root)? else {
        return Ok(Vec::new());
    };
    let mut infos = infos.0;
    if infos.is_empty() {
        return Ok(Vec::new());
    }
    infos.sort_unstable();

    let mut out = Vec::new();
    let mut seen_starts = Vec::new();
    let mut missed = Vec::new();
    for info in &infos {
        let probe = probe_offset(info.start_offset);
        if let Some(found) = reader.placement_at(probe)?
            && found.data_root == data_root
            && found.tx_start_offset == info.start_offset
        {
            push_unique(&mut out, &mut seen_starts, found);
            continue;
        }
        // The interval starts after the probe when this submodule clipped the
        // tx, or a later write covered the probe offset.
        missed.push(info.start_offset);
    }
    if missed.is_empty() {
        return Ok(out);
    }

    let all =
        reader.placements_in_range(PartitionChunkOffset(0), PartitionChunkOffset(u32::MAX))?;
    for start_offset in missed {
        for found in &all {
            if found.data_root == data_root && found.tx_start_offset == start_offset {
                push_unique(&mut out, &mut seen_starts, found.clone());
            }
        }
    }
    out.sort_by_key(|placement| placement.start);
    Ok(out)
}

fn push_unique(
    out: &mut Vec<Placement>,
    seen: &mut Vec<PartitionChunkOffset>,
    placement: Placement,
) {
    if seen.contains(&placement.start) {
        return;
    }
    seen.push(placement.start);
    out.push(placement);
}

fn probe_offset(start: RelativeChunkOffset) -> PartitionChunkOffset {
    if start.0 < 0 {
        PartitionChunkOffset(0)
    } else {
        PartitionChunkOffset(start.0 as u32)
    }
}

fn placement_for<R: SubmoduleRead + ?Sized>(
    reader: &R,
    interval: &IntervalRow,
    probe: PartitionChunkOffset,
) -> eyre::Result<Option<Placement>> {
    let Some(data_root) = data_root_for_stored_path(reader, interval.tx_path_hash)? else {
        return Ok(None);
    };
    let Some(infos) = reader.get_data_root_infos_for_data_root(data_root)? else {
        return Ok(None);
    };
    let Some(info) = covering_info(&infos.0, probe) else {
        return Ok(None);
    };
    Ok(Some(Placement {
        data_root,
        tx_start_offset: info.start_offset,
        data_size: info.data_size,
        ledger: None,
        start: interval.start,
        end: interval.end,
        block_height: None,
        tx_index: None,
    }))
}

/// Greatest `start_offset` that is still `<= probe`.
///
/// This is the same selection `StorageModule::resolve_placement` used before
/// the placement read lived here. An empty list, or a list whose first start
/// is still above the probe, is not a covering placement.
fn covering_info(infos: &[DataRootInfo], probe: PartitionChunkOffset) -> Option<DataRootInfo> {
    if infos.is_empty() {
        return None;
    }
    let mut sorted = infos.to_vec();
    sorted.sort_unstable();
    let probe_rel = RelativeChunkOffset::from(probe);
    let index = sorted
        .partition_point(|info| info.start_offset <= probe_rel)
        .saturating_sub(1);
    let info = sorted.get(index)?;
    (info.start_offset <= probe_rel).then(|| info.clone())
}

/// Real data root for a stored tx path, checked against the proof leaf.
///
/// `Ok(None)` when the tx-path bytes are absent. A missing binding, or a leaf
/// that does not match the stored fold, is corruption.
fn data_root_for_stored_path<R: SubmoduleRead + ?Sized>(
    reader: &R,
    tx_path_hash: H256,
) -> eyre::Result<Option<DataRoot>> {
    let Some(tx_path) = reader.get_full_tx_path(tx_path_hash)? else {
        return Ok(None);
    };
    let leaf = get_leaf_proof(&Base64::from(tx_path))?
        .hash()
        .map(H256::from)
        .ok_or_else(|| eyre!("Unable to parse tx_path leaf hash"))?;
    let binding = reader.get_tx_leaf_binding(tx_path_hash)?.ok_or_else(|| {
        eyre!("missing tx_path -> (data_root, prefix_hash) binding for stored tx_path")
    })?;
    let expected = DataTransactionLedger::fold_tx_root_leaf(binding.data_root, binding.prefix_hash);
    eyre::ensure!(
        leaf == expected,
        "tx_path leaf {leaf:?} != fold(stored data_root {:?}, prefix_hash {:?}) = {expected:?} \
         — submodule corruption (binding out of sync with stored proof)",
        binding.data_root,
        binding.prefix_hash,
    );
    Ok(Some(binding.data_root))
}
