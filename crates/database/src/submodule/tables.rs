use crate::metadata::MetadataKey;
use irys_types::{
    ChunkDataPath, DataCommitment, DataTreeRoot, H256, PartitionChunkOffset, RelativeChunkOffset,
    TxPath, TxPathHash,
};
use paste::paste;
use reth_codecs::Compact;
use reth_db::table::TableInfo;
use reth_db::{TableSet, tables};
use reth_db::{TableType, TableViewer};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;

/// Bytes stored in the submodule `Metadata` schema row after open succeeds.
///
/// Schema 3 keys data placements by the `data_commitment`. A schema v1
/// directory has no row. Open migrates it to [`SUBMODULE_SCHEMA_DATA_ROOT_KEYS`]
/// first. An index with that row and no data rows gets this value. Any other
/// row is refused.
// MDBX-SUBMODULE-INDEX: delete this MDBX schema marker with the MDBX engine. RocksDB v1 uses SUBMODULE_SCHEMA_ROCKS_V1.
pub const SUBMODULE_SCHEMA: &str = "irys-submodule-index 3\n";

/// Schema row of an MDBX index that keys data placements by the data root.
///
/// Open stamps [`SUBMODULE_SCHEMA`] over it when the index has no data rows,
/// and refuses it when it has data rows: those rows cannot be re-keyed, so
/// the network must reset.
// MDBX-SUBMODULE-INDEX: delete with the MDBX engine.
pub(crate) const SUBMODULE_SCHEMA_DATA_ROOT_KEYS: &str = "irys-submodule-index 2\n";

/// Schema file for a RocksDB v1 index.
///
/// The layout stores placements, inline data proofs, chunk checksums in the
/// offset row, and `ChunkOffsetsByContentHash`. MDBX stays on
/// [`SUBMODULE_SCHEMA`]. Any other `rocks/SCHEMA` text is refused.
///
/// The metadata row held the same text while placements were keyed by the
/// data root. The row is now [`SUBMODULE_SCHEMA_ROCKS_ROW`]. The file text
/// does not change: operator tooling reads it to tell a RocksDB v1 directory.
pub const SUBMODULE_SCHEMA_ROCKS_V1: &str = "irys-submodule-index rocksdb 1\n";

/// Metadata schema row of a RocksDB v1 index that keys placements by the
/// `data_commitment`.
///
/// Open stamps it over a [`SUBMODULE_SCHEMA_ROCKS_V1`] row when the index has
/// no data rows, and refuses that row when it has data rows. An older binary
/// refuses this row as an unrecognised layout.
#[cfg(feature = "rocksdb")]
pub(crate) const SUBMODULE_SCHEMA_ROCKS_ROW: &str =
    "irys-submodule-index rocksdb 1 data-commitment-keys\n";

/// Error for an index whose data rows are keyed by the data root.
///
/// Those rows cannot be re-keyed by the `data_commitment` in place: the
/// index does not store the geometry, and the stored proofs are for the old
/// data tree. Entropy and packing state are kept; only data must be reset.
pub(crate) fn data_root_keyed_index(path: &std::path::Path) -> String {
    format!(
        "submodule index {} holds data rows keyed by the data root. This build keys data by the data commitment and cannot convert them. The network must reset: reset this storage module",
        path.display()
    )
}

/// RocksDB column family for a RocksDB v1 offset record.
///
/// The value is the versioned proof and checksum. MDBX uses
/// `ChunkDataPathByOffset` and stores only the proof.
pub const CHUNK_META_BY_OFFSET_CF: &str = "ChunkMetaByOffset";

/// One message for every `rocks/` marker this build does not read.
///
/// The text names RocksDB v1 and the recovery (move the directory aside and
/// convert from MDBX, or reset the storage module). It does not name any
/// other layout.
pub fn unrecognised_rocks_layout(marker: &str) -> String {
    // Ansible (Irys-xyz/ansible) greps the irys binary for the sentence after
    // the layout marker. Keep that sentence in this one string literal.
    //   shared/canary_update.yml
    //   shared/submodule_index_rocksdb.yml
    //   shared/tasks/flip_and_restart.yml
    //   shared/tasks/submodule_index_reverify_if_moved.sh
    // tests/submodule_index_rocksdb_test.py checks the same sentence.
    format!(
        "unrecognised RocksDB index layout {marker:?}; this build reads RocksDB v1 only. If this directory came from a pre-release build, move it aside and convert from MDBX or reset the storage module"
    )
}

/// Metadata row written while a conversion into MDBX is still copying.
///
/// Open refuses this value. The converter replaces it with
/// [`SUBMODULE_SCHEMA`] only after every row is durable. A crash leaves
/// this row, so the directory is incomplete and must be deleted and redone.
// MDBX-SUBMODULE-INDEX: remove with rocks-to-mdbx. Open refuses a directory that still has this row.
pub const SUBMODULE_CONVERT_INCOMPLETE: &str = "irys-submodule-index convert-incomplete\n";

// Per-submodule database tables
// MDBX-SUBMODULE-INDEX: delete this MDBX table set with the MDBX engine. OffsetMeta, PackedOffsetRow, and Placement below are shared with RocksDB v1 and stay.
tables! {
    SubmoduleTables;
    /// Maps a partition offset to that chunk's data-path hash.
    ///
    /// The tx hash lives in [`TxPathIntervalByStart`], one row per placement.
    /// `tx_path_hash` on the value stays so an older row still decodes. New
    /// writes store `None` there.
    table ChunkPathHashesByOffset {
        type Key = PartitionChunkOffset;
        type Value = ChunkPathHashes;
    }

    /// One data proof at a partition offset.
    ///
    /// MDBX uses this name. The value is the raw proof. RocksDB v1 stores the
    /// versioned offset record in [`CHUNK_META_BY_OFFSET_CF`]. Offset order
    /// keeps compaction on sequential keys.
    table ChunkDataPathByOffset {
        type Key = PartitionChunkOffset;
        type Value = ChunkDataPath;
    }

    /// One inclusive placement: key is `start`, value is `(end, tx_path_hash)`.
    ///
    /// Intervals in one submodule do not overlap. A floor seek finds the tx
    /// hash for an offset. A covered offset with no data-path row is indexed.
    table TxPathIntervalByStart {
        type Key = PartitionChunkOffset;
        type Value = TxPathInterval;
    }

    /// Maps a tx path hash to the full tx path
    table TxPathByTxPathHash {
        type Key = TxPathHash;
        type Value = TxPath;
    }

    /// Maps a `data_commitment` to the list of DataRootInfos for each occurrence of that
    /// data in the submodule including their submodule relative start_offset and data_size
    table DataRootInfosByDataRoot {
        type Key = DataCommitment;
        type Value = DataRootInfos;
    }

    /// Maps a tx_path hash to the `(data_root, prefix_hash)` of the transaction that
    /// produced it. After the `prefix_hash` softfork, a tx_path leaf stores the folded
    /// `hash_all_sha256([data_root, prefix_hash])` (the ledger `tx_root` leaf value), NOT
    /// the raw `data_root`, so the `data_root` can no longer be read directly out of the
    /// proof leaf. The stored pair is the source of the real `data_root` on read and is
    /// cross-checked against the proof leaf via the same fold, so the binding and the
    /// proof can never silently disagree (a mismatch indicates corruption).
    table TxLeafBindingByTxPathHash {
        type Key = TxPathHash;
        type Value = TxLeafBinding;
    }


    /// Body-migration work still owed for a data_root placement in this submodule.
    ///
    /// Written in the same txn as the index (`StorageModule::index_transaction_data`)
    /// so "indexed" and "bytes still owed" can never disagree; deleted by the body
    /// worker once every offset in the range is durable or the job is retired.
    /// Keyed on the partition-relative start of the tx range *clipped to this
    /// submodule's interval*, so a cursor walk drains in ascending disk order.
    /// Empty on a healthy, caught-up node — this is an intent log, not an index.
    table PendingBodyMigrationsByOffset {
        type Key = PartitionChunkOffset;
        type Value = PendingBodyMigration;
    }

    /// Table to store various metadata, such as the current db schema version
    table Metadata {
        type Key = MetadataKey;
        type Value = Vec<u8>;
    }

}

/// Decoded [`CHUNK_META_BY_OFFSET_CF`] value on RocksDB v1.
///
/// `crc` is `None` when the row does not carry a checksum. That is unknown,
/// not a stored zero. `proof` is `None` on an entropy checksum row. A data
/// chunk has both. Callers that ask "is a data chunk stored here" read
/// `proof`, not the key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OffsetMeta {
    pub crc: Option<u32>,
    pub proof: Option<ChunkDataPath>,
}

/// One packed-chunk index write. `proof` of `None` is an entropy checksum.
///
/// Both fields `None` deletes the row. A proof with no covering interval is
/// not stored, and its checksum is not stored either.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackedOffsetRow {
    pub offset: PartitionChunkOffset,
    pub crc: Option<u32>,
    pub proof: Option<ChunkDataPath>,
}

/// One tx placement as a reader sees it.
///
/// MDBX leaves `ledger`, `block_height`, and `tx_index` empty. The data
/// tree root and commitment come from the tx-path leaf binding, and the
/// unclipped start and `data_size` come from [`DataRootInfo`]. `start` and `end` are the clipped
/// interval stored for this submodule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub data_tree_root: DataTreeRoot,
    /// Key of the placement's data: `data_tree_root` and `data_size` under
    /// the consensus geometry.
    pub data_commitment: DataCommitment,
    pub tx_start_offset: RelativeChunkOffset,
    pub data_size: u64,
    pub ledger: Option<u32>,
    pub start: PartitionChunkOffset,
    pub end: PartitionChunkOffset,
    pub block_height: Option<u64>,
    pub tx_index: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, Compact)]
/// Inclusive end offset and the tx-path hash for the placement that starts at the key.
pub struct TxPathInterval {
    pub end: PartitionChunkOffset,
    pub tx_path_hash: H256,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, Compact)]
/// compound value, containing the data path and tx path hashes
pub struct ChunkPathHashes {
    pub data_path_hash: Option<H256>, // ChunkPathHash - we can't use the alias types as proc_macro just deals with tokens
    pub tx_path_hash: Option<H256>,   // TxPathHash
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, Compact)]
/// relative start offsets and data_size for each instance of a data_root in the submodule
/// TODO: use a custom Compact as the default for `Vec<T>` sucks (make a custom one using const generics so we can optimize for fixed-size types?)
pub struct DataRootInfos(pub Vec<DataRootInfo>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, Compact)]
pub struct DataRootInfo {
    pub start_offset: RelativeChunkOffset,
    pub data_size: u64, // The data_size from the data transaction that paid to store the data_root at this start_offset
    /// Root of the data tree. The row key is the commitment of this root and
    /// `data_size`; the root is kept to build chunks for serving.
    pub data_tree_root: DataTreeRoot,
}

/// The `(data_root, prefix_hash)` a tx_path leaf folds from. Stored per tx_path hash so
/// the real `data_root` can be recovered (and the proof leaf re-verified via the fold)
/// after `prefix_hash` is folded into the ledger `tx_root` leaf.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, Compact)]
pub struct TxLeafBinding {
    pub data_root: H256,
    pub prefix_hash: H256,
    /// Commitment of the tx's data: `data_root` and the tx's `data_size`
    /// under the consensus geometry. It keys the tx's [`DataRootInfos`] row.
    pub data_commitment: DataCommitment,
}

/// One outstanding body-migration job: the chunk bodies a data_root placement
/// still owes this submodule. See [`PendingBodyMigrationsByOffset`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, Compact)]
pub struct PendingBodyMigration {
    /// Which data owes bytes at this placement. It keys the chunk cache.
    pub data_commitment: DataCommitment,
    /// `data_size` of the paying tx; gives the worker its chunk count without a
    /// second lookup.
    pub data_size: u64,
    /// Mirrors `DataRootInfo::start_offset` — the *unclipped* partition-relative
    /// tx start (legitimately negative when the data_root begins before this
    /// module's range). Maps partition offset <-> tx_chunk_offset for the cache
    /// lookup without re-reading `DataRootInfosByDataRoot`.
    pub start_offset: RelativeChunkOffset,
    /// Canonical height whose migration enqueued this job. Matched on settle/bump
    /// so a late drain cannot delete a replacement at the same offset.
    pub block_height: u64,
    /// Drain passes that ended with offsets still outstanding. Drives retirement
    /// so a job whose bodies are gone from the cache is not retried forever.
    pub attempts: u32,
}

impl PartialOrd for DataRootInfo {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Orders DataRootInfo by start_offset in ascending order
impl Ord for DataRootInfo {
    fn cmp(&self, other: &Self) -> Ordering {
        self.start_offset.cmp(&other.start_offset)
    }
}

/// Schema v1 table kept so migration can read hash-keyed data paths.
///
/// Not part of [`SubmoduleTables`]. A new database does not create it.
// MDBX-SUBMODULE-INDEX: remove with the v1 to v2 migration. This is the schema v1 data-path table.
pub mod v1 {
    use irys_types::{ChunkDataPath, ChunkPathHash};
    use paste::paste;
    use reth_db::table::TableInfo;
    use reth_db::{TableSet, TableType, TableViewer, tables};
    use std::fmt;

    tables! {
        V1DataPathTables;
        /// Data paths written by schema v1, keyed by the data-path hash.
        table ChunkDataPathByPathHash {
            type Key = ChunkPathHash;
            type Value = ChunkDataPath;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{IrysDatabaseArgs as _, open_or_create_db};

    use reth_db::mdbx::DatabaseArguments;

    use super::*;

    /// Ansible greps the irys binary for this sentence. The formatted error
    /// must contain it as one contiguous literal.
    #[test]
    fn unrecognised_rocks_layout_keeps_the_ansible_sentence() {
        let message = unrecognised_rocks_layout("");
        assert_eq!(
            message,
            "unrecognised RocksDB index layout \"\"; this build reads RocksDB v1 only. If this directory came from a pre-release build, move it aside and convert from MDBX or reset the storage module"
        );
        assert!(
            unrecognised_rocks_layout("irys-submodule-index rocksdb 9\n")
                .contains("; this build reads RocksDB v1 only. If this directory came from a pre-release build, move it aside and convert from MDBX or reset the storage module"),
            "the sentence must follow the layout marker"
        );
    }

    #[test]
    // MDBX-SUBMODULE-INDEX: delete this test with the MDBX table set.
    fn test_offset_range_queries() -> eyre::Result<()> {
        use irys_testing_utils::utils::TempDirBuilder;
        use reth_db::cursor::*;
        use reth_db::transaction::*;
        use reth_db::*;

        let temp_dir = TempDirBuilder::new()
            .prefix("test_offset_range_queries")
            .with_tracing()
            .build();

        let db = open_or_create_db(
            temp_dir,
            SubmoduleTables::ALL,
            DatabaseArguments::irys_testing()?,
        )
        .unwrap();

        let write_tx = db.tx_mut()?;

        let data_path_hash = H256::random();
        let tx_path_hash = H256::random();

        let path_hashes = ChunkPathHashes {
            data_path_hash: Some(data_path_hash),
            tx_path_hash: Some(tx_path_hash),
        };

        write_tx
            .put::<ChunkPathHashesByOffset>(PartitionChunkOffset::from(1), path_hashes.clone())?;
        write_tx
            .put::<ChunkPathHashesByOffset>(PartitionChunkOffset::from(100), path_hashes.clone())?;
        write_tx
            .put::<ChunkPathHashesByOffset>(PartitionChunkOffset::from(0), path_hashes.clone())?;

        write_tx.commit()?;

        let read_tx = db.tx()?;

        let mut read_cursor = read_tx.cursor_read::<ChunkPathHashesByOffset>()?;

        let walker = read_cursor.walk(None)?;

        let res = walker.collect::<Result<Vec<_>, _>>()?;

        assert_eq!(
            res,
            vec![
                (PartitionChunkOffset::from(0), path_hashes.clone()),
                (PartitionChunkOffset::from(1), path_hashes.clone()),
                (PartitionChunkOffset::from(100), path_hashes)
            ]
        );

        Ok(())
    }
}

/// Table sets that match schema v1. Test writers use these to build a `db/`
/// the way master and the release binaries do: same names, same encodings,
/// no schema row.
#[cfg(test)]
// MDBX-SUBMODULE-INDEX: remove with the v1 to v2 migration. Test writers use this to build a release-4.0.8 db/.
pub mod v1_fixture {
    pub mod master {
        use crate::metadata::MetadataKey;
        use irys_types::{
            ChunkDataPath, ChunkPathHash, H256, PartitionChunkOffset, TxPath, TxPathHash,
        };
        use paste::paste;
        use reth_db::table::TableInfo;
        use reth_db::{TableSet, TableType, TableViewer, tables};
        use std::fmt;

        use super::super::{ChunkPathHashes, DataRootInfos, PendingBodyMigration, TxLeafBinding};

        tables! {
            MasterV1Tables;
            table ChunkPathHashesByOffset {
                type Key = PartitionChunkOffset;
                type Value = ChunkPathHashes;
            }
            table ChunkDataPathByPathHash {
                type Key = ChunkPathHash;
                type Value = ChunkDataPath;
            }
            table TxPathByTxPathHash {
                type Key = TxPathHash;
                type Value = TxPath;
            }
            table DataRootInfosByDataRoot {
                type Key = H256;
                type Value = DataRootInfos;
            }
            table TxLeafBindingByTxPathHash {
                type Key = TxPathHash;
                type Value = TxLeafBinding;
            }
            table PendingBodyMigrationsByOffset {
                type Key = PartitionChunkOffset;
                type Value = PendingBodyMigration;
            }
            table Metadata {
                type Key = MetadataKey;
                type Value = Vec<u8>;
            }
        }
    }

    /// Release mainnet 4.x has no `PendingBodyMigrationsByOffset` table.
    pub mod mainnet {
        use crate::metadata::MetadataKey;
        use irys_types::{
            ChunkDataPath, ChunkPathHash, H256, PartitionChunkOffset, TxPath, TxPathHash,
        };
        use paste::paste;
        use reth_db::table::TableInfo;
        use reth_db::{TableSet, TableType, TableViewer, tables};
        use std::fmt;

        use super::super::{ChunkPathHashes, DataRootInfos, TxLeafBinding};

        tables! {
            MainnetV1Tables;
            table ChunkPathHashesByOffset {
                type Key = PartitionChunkOffset;
                type Value = ChunkPathHashes;
            }
            table ChunkDataPathByPathHash {
                type Key = ChunkPathHash;
                type Value = ChunkDataPath;
            }
            table TxPathByTxPathHash {
                type Key = TxPathHash;
                type Value = TxPath;
            }
            table DataRootInfosByDataRoot {
                type Key = H256;
                type Value = DataRootInfos;
            }
            table TxLeafBindingByTxPathHash {
                type Key = TxPathHash;
                type Value = TxLeafBinding;
            }
            table Metadata {
                type Key = MetadataKey;
                type Value = Vec<u8>;
            }
        }
    }
}
