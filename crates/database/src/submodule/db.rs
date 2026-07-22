use std::path::Path;

use irys_types::{
    ChunkDataPath, ChunkPathHash, DataRoot, PartitionChunkOffset, TxPath, TxPathHash,
};
use reth_db::{
    DatabaseEnv,
    cursor::DbCursorRO as _,
    mdbx::DatabaseArguments,
    transaction::{DbTx, DbTxMut},
};

use crate::{
    open_or_create_db,
    submodule::tables::{DataRootInfo, DataRootInfosByDataRoot},
};

use super::tables::{
    ChunkDataPathByPathHash, ChunkPathHashes, ChunkPathHashesByOffset, DataRootInfos,
    SubmoduleTables, TxLeafBinding, TxLeafBindingByTxPathHash, TxPathByTxPathHash,
};

/// Creates or opens a *submodule* MDBX database with the given [`DatabaseArguments`].
///
/// The caller supplies fully-built args (sync mode, geometry, etc.) — typically
/// via [`crate::submodule_db_args`] — so DB tuning lives in one place instead of
/// being threaded through as individual parameters.
pub fn create_or_open_submodule_db<P: AsRef<Path>>(
    path: P,
    args: DatabaseArguments,
) -> eyre::Result<DatabaseEnv> {
    open_or_create_db(path, SubmoduleTables::ALL, args)
}

/// gets the full data path for the chunk with the provided offset
pub fn get_data_path_by_offset<T: DbTx>(
    tx: &T,
    offset: PartitionChunkOffset,
) -> eyre::Result<Option<ChunkDataPath>> {
    if let Some(data_path_hash) =
        get_path_hashes_by_offset(tx, offset)?.and_then(|h| h.data_path_hash)
    {
        Ok(get_full_data_path(tx, data_path_hash)?)
    } else {
        Ok(None)
    }
}

/// gets the full tx path for the chunk with the provided offset
pub fn get_tx_path_by_offset<T: DbTx>(
    tx: &T,
    offset: PartitionChunkOffset,
) -> eyre::Result<Option<TxPath>> {
    if let Some(tx_path_hash) = get_path_hashes_by_offset(tx, offset)?.and_then(|h| h.tx_path_hash)
    {
        Ok(get_full_tx_path(tx, tx_path_hash)?)
    } else {
        Ok(None)
    }
}

pub fn get_path_hashes_by_offset<T: DbTx>(
    tx: &T,
    offset: PartitionChunkOffset,
) -> eyre::Result<Option<ChunkPathHashes>> {
    Ok(tx.get::<ChunkPathHashesByOffset>(offset)?)
}

/// First offset in half-open `[start, end)` absent from a fallible sorted key stream.
///
/// Returns as soon as the first hole is found (does not scan past it). Use
/// [`gaps_with`] when every hole is required.
///
/// `next_key` yields the next ascending key (or `None` when exhausted). Shared by
/// the MDBX cursor path and pure unit tests so both stay identical.
pub fn first_gap_with(
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
    mut next_key: impl FnMut() -> eyre::Result<Option<PartitionChunkOffset>>,
) -> eyre::Result<Option<PartitionChunkOffset>> {
    if start >= end {
        return Ok(None);
    }
    let mut expected = start;
    while let Some(key) = next_key()? {
        if key >= end {
            break;
        }
        if key > expected {
            return Ok(Some(expected));
        }
        expected = key + 1;
        if expected >= end {
            return Ok(None);
        }
    }
    Ok(if expected < end { Some(expected) } else { None })
}

/// All half-open missing ranges `[gap_start, gap_end)` inside `[start, end)`.
///
/// Consecutive missing offsets form one range. Indexed runs between holes are
/// **not** included — callers can re-index only the holes instead of the whole
/// tail after the first gap.
pub fn gaps_with(
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
    mut next_key: impl FnMut() -> eyre::Result<Option<PartitionChunkOffset>>,
) -> eyre::Result<Vec<(PartitionChunkOffset, PartitionChunkOffset)>> {
    if start >= end {
        return Ok(Vec::new());
    }
    let mut gaps = Vec::new();
    let mut expected = start;
    while let Some(key) = next_key()? {
        if key >= end {
            break;
        }
        if key > expected {
            gaps.push((expected, key));
        }
        // Advance past this present key (and any implied dense run of one).
        expected = key + 1;
        if expected >= end {
            return Ok(gaps);
        }
    }
    if expected < end {
        gaps.push((expected, end));
    }
    Ok(gaps)
}

/// First offset in half-open `[start, end)` absent from a sorted key stream.
///
/// Infallible wrapper around [`first_gap_with`] for tests and pure call sites.
pub fn first_gap_in_sorted_keys(
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
    keys: impl IntoIterator<Item = PartitionChunkOffset>,
) -> Option<PartitionChunkOffset> {
    let mut iter = keys.into_iter();
    first_gap_with(start, end, || Ok(iter.next())).expect("infallible key stream")
}

/// All half-open missing ranges in `[start, end)` from a sorted key stream.
pub fn gaps_in_sorted_keys(
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
    keys: impl IntoIterator<Item = PartitionChunkOffset>,
) -> Vec<(PartitionChunkOffset, PartitionChunkOffset)> {
    let mut iter = keys.into_iter();
    gaps_with(start, end, || Ok(iter.next())).expect("infallible key stream")
}

/// Returns true when the table is exactly the dense range `[start, end)`.
fn path_hash_table_is_dense_range<T: DbTx>(
    tx: &T,
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
) -> eyre::Result<bool> {
    let expected_len = (*end as u64).saturating_sub(*start as u64);
    if expected_len == 0 {
        return Ok(true);
    }
    let mut cursor = tx.cursor_read::<ChunkPathHashesByOffset>()?;
    let first = cursor.first()?;
    let last = cursor.last()?;
    let count = tx.entries::<ChunkPathHashesByOffset>()? as u64;
    Ok(match (first, last) {
        (Some((first_key, _)), Some((last_key, _))) => {
            // Compare last == end-1 without wrapping u32::MAX + 1.
            let last_is_range_end = *end > 0 && last_key == PartitionChunkOffset(*end - 1);
            first_key == start && last_is_range_end && count == expected_len
        }
        _ => false,
    })
}

/// First offset in half-open `[start, end)` with no `ChunkPathHashesByOffset` key.
///
/// Healthy full indexes take an O(1) density check (`first` + `last` + `entries`).
/// Otherwise walks the sorted keyspace until the first hole (one RO tx), not a
/// full multi-hole collect — use [`missing_path_hash_ranges_in_tx`] for that.
pub fn first_missing_path_hash_offset_in_tx<T: DbTx>(
    tx: &T,
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
) -> eyre::Result<Option<PartitionChunkOffset>> {
    if start >= end {
        return Ok(None);
    }
    if path_hash_table_is_dense_range(tx, start, end)? {
        return Ok(None);
    }

    let mut cursor = tx.cursor_read::<ChunkPathHashesByOffset>()?;
    let mut walker = cursor.walk(Some(start))?;
    first_gap_with(start, end, || {
        Ok(walker.next().transpose()?.map(|(key, _)| key))
    })
}

/// All half-open path-hash holes `[gap_start, gap_end)` in `[start, end)`.
///
/// Used by index heal to re-migrate **only** blocks overlapping holes, not the
/// entire tail after the first gap.
pub fn missing_path_hash_ranges_in_tx<T: DbTx>(
    tx: &T,
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
) -> eyre::Result<Vec<(PartitionChunkOffset, PartitionChunkOffset)>> {
    if start >= end {
        return Ok(Vec::new());
    }

    // Density fast path: prove the *entire table* is exactly the dense range
    // [start, end). Anchors on `cursor.first()` (table minimum), not `seek(start)`:
    // keys below `start` must not be allowed to pad `entries()` while holes sit
    // inside the range (e.g. keys {0,5,6,7,9} over [5,10) must not report dense).
    if path_hash_table_is_dense_range(tx, start, end)? {
        return Ok(Vec::new());
    }

    let mut cursor = tx.cursor_read::<ChunkPathHashesByOffset>()?;
    let mut walker = cursor.walk(Some(start))?;
    gaps_with(start, end, || {
        Ok(walker.next().transpose()?.map(|(key, _)| key))
    })
}

pub fn get_full_data_path<T: DbTx>(
    tx: &T,
    path_hash: ChunkPathHash,
) -> eyre::Result<Option<ChunkDataPath>> {
    Ok(tx.get::<ChunkDataPathByPathHash>(path_hash)?)
}

pub fn add_full_data_path<T: DbTxMut>(
    tx: &T,
    path_hash: ChunkPathHash,
    data_path: ChunkDataPath,
) -> eyre::Result<()> {
    tx.put::<ChunkDataPathByPathHash>(path_hash, data_path)?;
    Ok(())
}

pub fn get_full_tx_path<T: DbTx>(tx: &T, path_hash: TxPathHash) -> eyre::Result<Option<TxPath>> {
    Ok(tx.get::<TxPathByTxPathHash>(path_hash)?)
}

pub fn add_full_tx_path<T: DbTxMut>(
    tx: &T,
    path_hash: TxPathHash,
    tx_path: TxPath,
) -> eyre::Result<()> {
    tx.put::<TxPathByTxPathHash>(path_hash, tx_path)?;
    Ok(())
}

/// Get the `(data_root, prefix_hash)` binding for a tx_path hash.
pub fn get_tx_leaf_binding<T: DbTx>(
    tx: &T,
    path_hash: TxPathHash,
) -> eyre::Result<Option<TxLeafBinding>> {
    Ok(tx.get::<TxLeafBindingByTxPathHash>(path_hash)?)
}

/// Store the `(data_root, prefix_hash)` a tx_path leaf folds from, keyed by the tx_path
/// hash, so the real `data_root` can be recovered (and the proof leaf re-verified via the
/// fold) on read once `prefix_hash` is folded into the ledger `tx_root` leaf.
pub fn add_tx_leaf_binding<T: DbTxMut>(
    tx: &T,
    path_hash: TxPathHash,
    binding: &TxLeafBinding,
) -> eyre::Result<()> {
    tx.put::<TxLeafBindingByTxPathHash>(path_hash, binding.clone())?;
    Ok(())
}

pub fn add_data_path_hash_to_offset_index<T: DbTxMut + DbTx>(
    tx: &T,
    offset: PartitionChunkOffset,
    path_hash: Option<ChunkPathHash>,
) -> eyre::Result<()> {
    let mut chunk_hashes = get_path_hashes_by_offset(tx, offset)?.unwrap_or_default();
    chunk_hashes.data_path_hash = path_hash;
    set_path_hashes_by_offset(tx, offset, chunk_hashes)?;
    Ok(())
}

pub fn add_tx_path_hash_to_offset_index<T: DbTxMut + DbTx>(
    tx: &T,
    offset: PartitionChunkOffset,
    path_hash: Option<TxPathHash>,
) -> eyre::Result<()> {
    let mut chunk_hashes = get_path_hashes_by_offset(tx, offset)?.unwrap_or_default();
    chunk_hashes.tx_path_hash = path_hash;
    set_path_hashes_by_offset(tx, offset, chunk_hashes)?;
    Ok(())
}

pub fn set_path_hashes_by_offset<T: DbTxMut>(
    tx: &T,
    offset: PartitionChunkOffset,
    path_hashes: ChunkPathHashes,
) -> eyre::Result<()> {
    Ok(tx.put::<ChunkPathHashesByOffset>(offset, path_hashes)?)
}

/// get DataRootInfos for the `data_root`
pub fn get_data_root_infos_for_data_root<T: DbTx>(
    tx: &T,
    data_root: DataRoot,
) -> eyre::Result<Option<DataRootInfos>> {
    Ok(tx.get::<DataRootInfosByDataRoot>(data_root)?)
}

/// set (overwrite) the DataRootInfos for the `data_root`
pub fn set_data_root_infos_for_data_root<T: DbTxMut>(
    tx: &T,
    data_root: DataRoot,
    infos: DataRootInfos,
) -> eyre::Result<()> {
    Ok(tx.put::<DataRootInfosByDataRoot>(data_root, infos)?)
}

/// Track the start_offset and data_size metadata for the `data_root`
pub fn add_data_root_info<T: DbTxMut + DbTx>(
    tx: &T,
    data_root: DataRoot,
    info: &DataRootInfo,
) -> eyre::Result<()> {
    let mut data_root_infos = get_data_root_infos_for_data_root(tx, data_root)?.unwrap_or_default();
    if !data_root_infos.0.contains(info) {
        data_root_infos.0.push(info.clone());
    }
    set_data_root_infos_for_data_root(tx, data_root, data_root_infos)?;
    Ok(())
}

/// clear db
pub fn clear_submodule_database<T: DbTxMut>(tx: &T) -> eyre::Result<()> {
    tx.clear::<ChunkPathHashesByOffset>()?;
    tx.clear::<ChunkDataPathByPathHash>()?;
    tx.clear::<TxPathByTxPathHash>()?;
    tx.clear::<DataRootInfosByDataRoot>()?;
    tx.clear::<TxLeafBindingByTxPathHash>()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::db::IrysDatabaseExt as _;
    use crate::submodule::tables::DataRootInfos;
    use crate::submodule::{
        add_data_root_info, get_data_root_infos_for_data_root, set_data_root_infos_for_data_root,
    };
    use crate::{
        IrysDatabaseArgs as _, open_or_create_db,
        submodule::tables::{DataRootInfo, SubmoduleTables},
    };
    use irys_types::H256;
    use irys_types::RelativeChunkOffset;
    use reth_db::Database as _;
    use reth_db::mdbx::DatabaseArguments;
    use reth_db::transaction::DbTx as _;

    #[test]
    fn db_compact_dataroot_info() -> eyre::Result<()> {
        let tmpdir = irys_testing_utils::utils::TempDirBuilder::new()
            .prefix("irys-datarootinfo-")
            .keep()
            .build();

        let db = open_or_create_db(
            tmpdir,
            SubmoduleTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;
        let write_tx = db.tx_mut()?;
        let infos = vec![
            DataRootInfo {
                start_offset: RelativeChunkOffset(0),
                data_size: 0,
            },
            DataRootInfo {
                start_offset: RelativeChunkOffset(-20),
                data_size: 100,
            },
            DataRootInfo {
                start_offset: RelativeChunkOffset(10000),
                data_size: 4000,
            },
            DataRootInfo {
                start_offset: RelativeChunkOffset(i32::MIN),
                data_size: u64::MAX,
            },
            DataRootInfo {
                start_offset: RelativeChunkOffset(i32::MAX),
                data_size: u64::MAX,
            },
        ];
        let data_root = H256::zero();
        set_data_root_infos_for_data_root(&write_tx, data_root, DataRootInfos(infos.clone()))?;
        write_tx.commit()?;

        let infos2 = db
            .view_eyre(|tx| get_data_root_infos_for_data_root(tx, data_root))?
            .unwrap();

        assert_eq!(infos, infos2.0);

        // add a DataRootInfo to existing DataRootInfos
        let new_data_root_info = DataRootInfo {
            start_offset: RelativeChunkOffset(1),
            data_size: 100,
        };

        db.update_eyre(|tx| add_data_root_info(tx, data_root, &new_data_root_info))?;

        let infos3 = db
            .view_eyre(|tx| get_data_root_infos_for_data_root(tx, data_root))?
            .unwrap();

        assert_eq!(infos3.0.len(), 6);
        assert_eq!(*infos3.0.last().unwrap(), new_data_root_info);

        let random_data_root = H256::random();
        let missing_infos =
            db.view_eyre(|tx| get_data_root_infos_for_data_root(tx, random_data_root))?;

        assert!(missing_infos.is_none());

        Ok(())
    }

    fn o(v: u32) -> irys_types::PartitionChunkOffset {
        irys_types::PartitionChunkOffset::from(v)
    }

    #[test]
    fn first_gap_in_sorted_keys_covers_hole_patterns() {
        use super::first_gap_in_sorted_keys;

        assert_eq!(first_gap_in_sorted_keys(o(0), o(0), []), None);
        assert_eq!(first_gap_in_sorted_keys(o(0), o(10), []), Some(o(0)));
        assert_eq!(
            first_gap_in_sorted_keys(
                o(0),
                o(10),
                [o(0), o(1), o(2), o(3), o(4), o(5), o(6), o(7), o(8), o(9)]
            ),
            None
        );
        // trailing suffix hole
        assert_eq!(
            first_gap_in_sorted_keys(o(0), o(10), [o(0), o(1), o(2), o(3), o(4)]),
            Some(o(5))
        );
        // middle hole (binary-search failure mode)
        assert_eq!(
            first_gap_in_sorted_keys(o(0), o(10), [o(0), o(1), o(5), o(6), o(7), o(8), o(9)]),
            Some(o(2))
        );
        // unindexed prefix
        assert_eq!(
            first_gap_in_sorted_keys(o(0), o(10), [o(5), o(6), o(7), o(8), o(9)]),
            Some(o(0))
        );
        // multi-hole from offset 1
        assert_eq!(
            first_gap_in_sorted_keys(o(1), o(6), [o(1), o(3), o(5)]),
            Some(o(2))
        );
    }

    #[test]
    fn gaps_in_sorted_keys_reports_each_hole_not_full_tail() {
        use super::gaps_in_sorted_keys;

        // Dense
        assert!(
            gaps_in_sorted_keys(
                o(0),
                o(10),
                [o(0), o(1), o(2), o(3), o(4), o(5), o(6), o(7), o(8), o(9)]
            )
            .is_empty()
        );

        // Two disjoint holes: [2,5) and [8,9) — not [2,10)
        assert_eq!(
            gaps_in_sorted_keys(o(0), o(10), [o(0), o(1), o(5), o(6), o(7), o(9)]),
            vec![(o(2), o(5)), (o(8), o(9))]
        );

        // Prefix + middle
        assert_eq!(
            gaps_in_sorted_keys(o(0), o(10), [o(2), o(3), o(7), o(8), o(9)]),
            vec![(o(0), o(2)), (o(4), o(7))]
        );

        // Fully empty
        assert_eq!(gaps_in_sorted_keys(o(0), o(5), []), vec![(o(0), o(5))]);
    }

    #[test]
    fn first_missing_path_hash_offset_cursor_finds_middle_hole() -> eyre::Result<()> {
        use super::{
            ChunkPathHashes, first_missing_path_hash_offset_in_tx, set_path_hashes_by_offset,
        };
        use crate::submodule::tables::SubmoduleTables;
        use crate::{IrysDatabaseArgs as _, open_or_create_db};
        use irys_testing_utils::utils::TempDirBuilder;
        use irys_types::H256;
        use reth_db::Database as _;
        use reth_db::mdbx::DatabaseArguments;

        let temp_dir = TempDirBuilder::new()
            .prefix("path_hash_gap")
            .with_tracing()
            .build();
        let db = open_or_create_db(
            temp_dir,
            SubmoduleTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;

        let path_hashes = ChunkPathHashes {
            data_path_hash: Some(H256::random()),
            tx_path_hash: Some(H256::random()),
        };
        {
            let tx = db.tx_mut()?;
            // indexed 0,1 and 5..9 — hole at 2
            for off in [0_u32, 1, 5, 6, 7, 8, 9] {
                set_path_hashes_by_offset(&tx, o(off), path_hashes.clone())?;
            }
            tx.commit()?;
        }

        let gap = db.view_eyre(|tx| first_missing_path_hash_offset_in_tx(tx, o(0), o(10)))?;
        assert_eq!(gap, Some(o(2)));

        let none = db.view_eyre(|tx| first_missing_path_hash_offset_in_tx(tx, o(5), o(10)))?;
        assert_eq!(none, None);

        let head = db.view_eyre(|tx| first_missing_path_hash_offset_in_tx(tx, o(0), o(5)))?;
        assert_eq!(head, Some(o(2)));

        Ok(())
    }

    #[test]
    fn first_missing_path_hash_offset_density_fast_path_no_gap() -> eyre::Result<()> {
        use super::{
            ChunkPathHashes, first_missing_path_hash_offset_in_tx, set_path_hashes_by_offset,
        };
        use crate::submodule::tables::SubmoduleTables;
        use crate::{IrysDatabaseArgs as _, open_or_create_db};
        use irys_testing_utils::utils::TempDirBuilder;
        use irys_types::H256;
        use reth_db::Database as _;
        use reth_db::mdbx::DatabaseArguments;

        let temp_dir = TempDirBuilder::new()
            .prefix("path_hash_dense")
            .with_tracing()
            .build();
        let db = open_or_create_db(
            temp_dir,
            SubmoduleTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;

        let path_hashes = ChunkPathHashes {
            data_path_hash: Some(H256::random()),
            tx_path_hash: Some(H256::random()),
        };
        {
            let tx = db.tx_mut()?;
            for off in 0_u32..10 {
                set_path_hashes_by_offset(&tx, o(off), path_hashes.clone())?;
            }
            tx.commit()?;
        }

        // Dense [0, 10) — first/last/entries prove completeness without relying on a hole.
        let none = db.view_eyre(|tx| first_missing_path_hash_offset_in_tx(tx, o(0), o(10)))?;
        assert_eq!(none, None);

        // Partial dense range with extra keys outside still falls through correctly.
        let none_mid = db.view_eyre(|tx| first_missing_path_hash_offset_in_tx(tx, o(3), o(7)))?;
        assert_eq!(none_mid, None);

        Ok(())
    }

    /// Keys below `start` must not pad `entries()` while a hole sits in-range.
    /// With `seek(start)` + whole-table count, {0,5,6,7,9} over [5,10) looked dense.
    #[test]
    fn first_missing_path_hash_offset_density_not_fooled_by_keys_below_start() -> eyre::Result<()> {
        use super::{
            ChunkPathHashes, first_missing_path_hash_offset_in_tx, set_path_hashes_by_offset,
        };
        use crate::submodule::tables::SubmoduleTables;
        use crate::{IrysDatabaseArgs as _, open_or_create_db};
        use irys_testing_utils::utils::TempDirBuilder;
        use irys_types::H256;
        use reth_db::Database as _;
        use reth_db::mdbx::DatabaseArguments;

        let temp_dir = TempDirBuilder::new()
            .prefix("path_hash_pad")
            .with_tracing()
            .build();
        let db = open_or_create_db(
            temp_dir,
            SubmoduleTables::ALL,
            DatabaseArguments::irys_testing()?,
        )?;

        let path_hashes = ChunkPathHashes {
            data_path_hash: Some(H256::random()),
            tx_path_hash: Some(H256::random()),
        };
        {
            let tx = db.tx_mut()?;
            // 5 keys total; expected_len([5,10))=5. Hole at 8 inside the range.
            for off in [0_u32, 5, 6, 7, 9] {
                set_path_hashes_by_offset(&tx, o(off), path_hashes.clone())?;
            }
            tx.commit()?;
        }

        let gap = db.view_eyre(|tx| first_missing_path_hash_offset_in_tx(tx, o(5), o(10)))?;
        assert_eq!(gap, Some(o(8)), "must not treat padded entries() as dense");

        Ok(())
    }
}
