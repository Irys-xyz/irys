use crate::metadata::MetadataKey;
use irys_types::{
    ChunkDataPath, ChunkPathHash, DataRoot, PartitionChunkOffset, RelativeChunkOffset, TxPath,
    TxPathHash, H256,
};
use paste::paste;
use reth_codecs::Compact;
use reth_db::table::TableInfo;
use reth_db::{tables, TableSet};
use reth_db::{TableType, TableViewer};
use serde::{Deserialize, Serialize};
use std::fmt;

// Per-submodule database tables
tables! {
    SubmoduleTables;
  /// Maps a partition relative offset to a chunk's path hashes
    /// note: mdbx keys are always sorted, so range queries work :)
    /// TODO: use custom Compact impl for Vec<u8> so we don't have problems
    /// Also change/split this to leverage key-sorting to only store a single tx_path_hash entry/data_root
    table ChunkPathHashesByOffset {
        type Key = PartitionChunkOffset;
        type Value = ChunkPathHashes;
    }

    /// Maps a chunk's data path hash to the full data path
    /// TODO: change how we store these to reduce duplication (use dupsort + tree traversal indices)
    table ChunkDataPathByPathHash {
        type Key = ChunkPathHash;
        type Value = ChunkDataPath;
    }

    /// Maps a tx path hash to the full tx path
    table TxPathByTxPathHash {
        type Key = TxPathHash;
        type Value = TxPath;
    }

    /// Maps a chunk path hash to the list of submodule-relative offsets it should inhabit
    table ChunkOffsetsByPathHash {
        type Key = ChunkPathHash;
        type Value = ChunkOffsets;
    }

    /// Maps a data root to the list of submodule-relative start offsets
    table StartOffsetsByDataRoot {
        type Key = DataRoot;
        type Value = RelativeStartOffsets;
    }

    /// Maps a data root to it's data size (used for validation)
    table DataSizeByDataRoot {
        type Key = DataRoot;
        type Value = u64;
    }

    /// Table to store various metadata, such as the current db schema version
    table Metadata {
        type Key = MetadataKey;
        type Value = Vec<u8>;
    }

}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, Compact)]
/// chunk offsets
/// TODO: use a custom Compact as the default for Vec<T> sucks (make a custom one using const generics so we can optimize for fixed-size types?)
pub struct ChunkOffsets(pub Vec<PartitionChunkOffset>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, Compact)]
/// compound value, containing the data path and tx path hashes
pub struct ChunkPathHashes {
    pub data_path_hash: Option<H256>, // ChunkPathHash - we can't use the alias types as proc_macro just deals with tokens
    pub tx_path_hash: Option<H256>,   // TxPathHash
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, Compact)]
/// chunk offsets
/// TODO: use a custom Compact as the default for Vec<T> sucks (make a custom one using const generics so we can optimize for fixed-size types?)
pub struct RelativeStartOffsets(pub Vec<RelativeChunkOffset>);

#[cfg(test)]
mod tests {
    use crate::open_or_create_db;

    use super::*;

    #[test]
    fn test_offset_range_queries() -> eyre::Result<()> {
        use irys_testing_utils::utils::setup_tracing_and_temp_dir;
        use reth_db::cursor::*;
        use reth_db::transaction::*;
        use reth_db::*;

        let temp_dir = setup_tracing_and_temp_dir(Some("test_offset_range_queries"), false);

        let db = open_or_create_db(temp_dir, SubmoduleTables::ALL, None).unwrap();

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
