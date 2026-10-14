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
        out.extend(placements_for_interval(reader, &interval, start, end)?);
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
    Ok(placements_for_interval(reader, interval, probe, probe)?
        .into_iter()
        .next())
}

/// One placement per data-root info that falls inside `interval`.
///
/// Schema v2 merges adjacent rows that share a tx-path hash. Two txs with
/// the same bytes then occupy one interval. The info start inside that
/// interval is the next tx. A negative start does not split: the tx began
/// before this partition, and the first segment keeps that info.
fn placements_for_interval<R: SubmoduleRead + ?Sized>(
    reader: &R,
    interval: &IntervalRow,
    query_start: PartitionChunkOffset,
    query_end: PartitionChunkOffset,
) -> eyre::Result<Vec<Placement>> {
    let Some(data_root) = data_root_for_stored_path(reader, interval.tx_path_hash)? else {
        return Ok(Vec::new());
    };
    let Some(infos) = reader.get_data_root_infos_for_data_root(data_root)? else {
        return Ok(Vec::new());
    };
    let mut cuts = vec![interval.start];
    for info in &infos.0 {
        let Ok(at) = u32::try_from(info.start_offset.0) else {
            continue;
        };
        if at > interval.start.0 && at <= interval.end.0 {
            cuts.push(PartitionChunkOffset(at));
        }
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = Vec::new();
    for (index, cut) in cuts.iter().enumerate() {
        let seg_end = match cuts.get(index + 1) {
            Some(next) => PartitionChunkOffset(next.0 - 1),
            None => interval.end,
        };
        if seg_end < query_start || *cut > query_end {
            continue;
        }
        let Some(info) = covering_info(&infos.0, *cut)? else {
            continue;
        };
        out.push(Placement {
            data_root,
            tx_start_offset: info.start_offset,
            data_size: info.data_size,
            ledger: None,
            start: *cut,
            end: seg_end,
            block_height: None,
            tx_index: None,
        });
    }
    Ok(out)
}

/// Greatest `start_offset` that is still `<= probe`.
///
/// This is the same selection `StorageModule::resolve_placement` used before
/// the placement read lived here. An empty list, or a list whose first start
/// is still above the probe, is not a covering placement.
fn covering_info(
    infos: &[DataRootInfo],
    probe: PartitionChunkOffset,
) -> eyre::Result<Option<DataRootInfo>> {
    if infos.is_empty() {
        return Ok(None);
    }
    let mut sorted = infos.to_vec();
    sorted.sort_unstable();
    let probe_rel = RelativeChunkOffset(
        i32::try_from(probe.0)
            .map_err(|_| eyre!("partition offset {} does not fit in i32", probe.0))?,
    );
    let index = sorted
        .partition_point(|info| info.start_offset <= probe_rel)
        .saturating_sub(1);
    let Some(info) = sorted.get(index) else {
        return Ok(None);
    };
    Ok((info.start_offset <= probe_rel).then(|| info.clone()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covering_info_rejects_an_offset_above_i32_max() {
        let infos = [DataRootInfo {
            start_offset: RelativeChunkOffset(0),
            data_size: 1,
        }];
        let err = covering_info(&infos, PartitionChunkOffset((i32::MAX as u32) + 1))
            .expect_err("offset above i32::MAX");
        assert!(err.to_string().contains("does not fit in i32"), "{err}");
    }
}
