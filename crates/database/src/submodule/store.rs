//! Submodule index boundary.
//!
//! Callers open one read view or one write batch. They do not see the
//! engine. A view is one committed snapshot: it does not see a commit that
//! starts after the view opens. A write batch reads its own uncommitted
//! writes, and `update` is exclusive until the closure returns. The batch
//! commits only when the closure returns `Ok`. A later engine has to keep
//! those three rules; MDBX gets them from its transaction.

use std::{path::Path, sync::Arc};

use irys_types::{
    ChunkDataPath, ChunkPathHash, DataRoot, PartitionChunkOffset, TxPath, TxPathHash,
};
use reth_db::{Database as _, DatabaseEnv, mdbx::DatabaseArguments};

use super::{
    add_data_path_hash_to_offset_index, add_data_root_info, add_full_data_path, add_full_tx_path,
    add_pending_body_migration, add_tx_leaf_binding, add_tx_path_hash_to_offset_index,
    add_tx_path_hash_to_offset_range, clear_submodule_database, create_or_open_submodule_db,
    del_path_hashes_by_offset, del_pending_body_migration, del_pending_body_migrations_in_range,
    first_missing_path_hash_offset_in_tx, get_data_path_by_offset,
    get_data_root_infos_for_data_root, get_full_data_path, get_full_tx_path,
    get_path_hashes_by_offset, get_pending_body_migration, get_tx_leaf_binding,
    get_tx_path_by_offset, missing_path_hash_ranges_in_tx, path_hashes_in_inclusive_range,
    pending_body_migrations_from, set_data_root_infos_for_data_root, set_path_hashes_by_offset,
    tables::{ChunkPathHashes, DataRootInfo, DataRootInfos, PendingBodyMigration, TxLeafBinding},
    write_data_path_updates,
};
use crate::db::IrysDatabaseExt as _;

/// Consistent read of one submodule index.
pub trait SubmoduleRead {
    fn get_data_path_by_offset(
        &self,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<Option<ChunkDataPath>>;

    fn get_tx_path_by_offset(&self, offset: PartitionChunkOffset) -> eyre::Result<Option<TxPath>>;

    fn get_path_hashes_by_offset(
        &self,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<Option<ChunkPathHashes>>;

    /// Inclusive rows in `[start, end]`, in offset order. Missing offsets are absent.
    fn path_hashes_in_inclusive_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, ChunkPathHashes)>>;

    /// First offset in half-open `[start, end)` with no path-hash key.
    fn first_missing_path_hash_offset(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Option<PartitionChunkOffset>>;

    /// Half-open path-hash holes `[gap_start, gap_end)` inside `[start, end)`.
    fn missing_path_hash_ranges(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, PartitionChunkOffset)>>;

    fn get_full_data_path(&self, path_hash: ChunkPathHash) -> eyre::Result<Option<ChunkDataPath>>;

    fn get_full_tx_path(&self, path_hash: TxPathHash) -> eyre::Result<Option<TxPath>>;

    fn get_tx_leaf_binding(&self, path_hash: TxPathHash) -> eyre::Result<Option<TxLeafBinding>>;

    fn get_data_root_infos_for_data_root(
        &self,
        data_root: DataRoot,
    ) -> eyre::Result<Option<DataRootInfos>>;

    fn get_pending_body_migration(
        &self,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<Option<PendingBodyMigration>>;

    /// Jobs at or after `start` (`None` is every job), in ascending offset order.
    fn pending_body_migrations_from(
        &self,
        start: Option<PartitionChunkOffset>,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, PendingBodyMigration)>>;
}

/// Exclusive write batch. Reads inside the batch see its own writes.
pub trait SubmoduleWrite: SubmoduleRead {
    fn add_full_data_path(
        &mut self,
        path_hash: ChunkPathHash,
        data_path: ChunkDataPath,
    ) -> eyre::Result<()>;

    fn add_full_tx_path(&mut self, path_hash: TxPathHash, tx_path: TxPath) -> eyre::Result<()>;

    fn add_tx_leaf_binding(
        &mut self,
        path_hash: TxPathHash,
        binding: &TxLeafBinding,
    ) -> eyre::Result<()>;

    fn add_data_path_hash_to_offset_index(
        &mut self,
        offset: PartitionChunkOffset,
        path_hash: Option<ChunkPathHash>,
    ) -> eyre::Result<()>;

    fn add_tx_path_hash_to_offset_index(
        &mut self,
        offset: PartitionChunkOffset,
        path_hash: Option<TxPathHash>,
    ) -> eyre::Result<()>;

    /// Set `tx_path_hash` on every offset in the inclusive range `[start, end]`.
    ///
    /// A range past the last key is appended. An overlapping range keeps any
    /// `data_path_hash` already stored on those offsets.
    fn add_tx_path_hash_to_offset_range(
        &mut self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
        path_hash: Option<TxPathHash>,
    ) -> eyre::Result<()>;

    /// Store each chunk's data path and its offset-index hash in this batch.
    ///
    /// An existing `tx_path_hash` on the same offset stays in place.
    fn write_data_path_updates(
        &mut self,
        updates: Vec<(PartitionChunkOffset, ChunkPathHash, ChunkDataPath)>,
    ) -> eyre::Result<()>;

    fn set_path_hashes_by_offset(
        &mut self,
        offset: PartitionChunkOffset,
        path_hashes: ChunkPathHashes,
    ) -> eyre::Result<()>;

    fn del_path_hashes_by_offset(&mut self, offset: PartitionChunkOffset) -> eyre::Result<()>;

    fn set_data_root_infos_for_data_root(
        &mut self,
        data_root: DataRoot,
        infos: DataRootInfos,
    ) -> eyre::Result<()>;

    fn add_data_root_info(&mut self, data_root: DataRoot, info: &DataRootInfo) -> eyre::Result<()>;

    fn add_pending_body_migration(
        &mut self,
        offset: PartitionChunkOffset,
        job: &PendingBodyMigration,
    ) -> eyre::Result<()>;

    /// `Ok(true)` when a row existed at `offset`.
    fn del_pending_body_migration(&mut self, offset: PartitionChunkOffset) -> eyre::Result<bool>;

    /// Delete jobs whose key lies in `[start, end]`. Returns how many were removed.
    fn del_pending_body_migrations_in_range(
        &mut self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<usize>;

    /// Delete index rows. Does not delete the schema-version metadata row.
    fn clear(&mut self) -> eyre::Result<()>;
}

/// Open, read, and commit one submodule index.
///
/// `view` and `update` borrow the engine only for the closure. The closure's
/// `Err` aborts the batch. `update` commits on `Ok`.
pub trait SubmoduleStore: Send + Sync {
    fn view<R>(&self, f: impl FnOnce(&mut dyn SubmoduleRead) -> eyre::Result<R>)
    -> eyre::Result<R>;

    fn update<R>(
        &self,
        f: impl FnOnce(&mut dyn SubmoduleWrite) -> eyre::Result<R>,
    ) -> eyre::Result<R>;
}

/// MDBX submodule index. One environment, the tables in [`super::tables`].
#[derive(Clone)]
pub struct MdbxSubmoduleStore {
    env: Arc<DatabaseEnv>,
}

impl std::fmt::Debug for MdbxSubmoduleStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MdbxSubmoduleStore")
            .field("path", &self.env.path())
            .finish()
    }
}

impl MdbxSubmoduleStore {
    pub fn open(path: impl AsRef<Path>, args: DatabaseArguments) -> eyre::Result<Self> {
        let env = create_or_open_submodule_db(path, args)?;
        Ok(Self { env: Arc::new(env) })
    }
}

struct MdbxRead<'a> {
    tx: &'a <DatabaseEnv as reth_db::Database>::TX,
}

struct MdbxWrite<'a> {
    tx: &'a <DatabaseEnv as reth_db::Database>::TXMut,
}

macro_rules! impl_submodule_read {
    ($ty:ty) => {
        impl SubmoduleRead for $ty {
            fn get_data_path_by_offset(
                &self,
                offset: PartitionChunkOffset,
            ) -> eyre::Result<Option<ChunkDataPath>> {
                get_data_path_by_offset(self.tx, offset)
            }

            fn get_tx_path_by_offset(
                &self,
                offset: PartitionChunkOffset,
            ) -> eyre::Result<Option<TxPath>> {
                get_tx_path_by_offset(self.tx, offset)
            }

            fn get_path_hashes_by_offset(
                &self,
                offset: PartitionChunkOffset,
            ) -> eyre::Result<Option<ChunkPathHashes>> {
                get_path_hashes_by_offset(self.tx, offset)
            }

            fn path_hashes_in_inclusive_range(
                &self,
                start: PartitionChunkOffset,
                end: PartitionChunkOffset,
            ) -> eyre::Result<Vec<(PartitionChunkOffset, ChunkPathHashes)>> {
                path_hashes_in_inclusive_range(self.tx, start, end)
            }

            fn first_missing_path_hash_offset(
                &self,
                start: PartitionChunkOffset,
                end: PartitionChunkOffset,
            ) -> eyre::Result<Option<PartitionChunkOffset>> {
                first_missing_path_hash_offset_in_tx(self.tx, start, end)
            }

            fn missing_path_hash_ranges(
                &self,
                start: PartitionChunkOffset,
                end: PartitionChunkOffset,
            ) -> eyre::Result<Vec<(PartitionChunkOffset, PartitionChunkOffset)>> {
                missing_path_hash_ranges_in_tx(self.tx, start, end)
            }

            fn get_full_data_path(
                &self,
                path_hash: ChunkPathHash,
            ) -> eyre::Result<Option<ChunkDataPath>> {
                get_full_data_path(self.tx, path_hash)
            }

            fn get_full_tx_path(&self, path_hash: TxPathHash) -> eyre::Result<Option<TxPath>> {
                get_full_tx_path(self.tx, path_hash)
            }

            fn get_tx_leaf_binding(
                &self,
                path_hash: TxPathHash,
            ) -> eyre::Result<Option<TxLeafBinding>> {
                get_tx_leaf_binding(self.tx, path_hash)
            }

            fn get_data_root_infos_for_data_root(
                &self,
                data_root: DataRoot,
            ) -> eyre::Result<Option<DataRootInfos>> {
                get_data_root_infos_for_data_root(self.tx, data_root)
            }

            fn get_pending_body_migration(
                &self,
                offset: PartitionChunkOffset,
            ) -> eyre::Result<Option<PendingBodyMigration>> {
                get_pending_body_migration(self.tx, offset)
            }

            fn pending_body_migrations_from(
                &self,
                start: Option<PartitionChunkOffset>,
            ) -> eyre::Result<Vec<(PartitionChunkOffset, PendingBodyMigration)>> {
                pending_body_migrations_from(self.tx, start)
            }
        }
    };
}

impl_submodule_read!(MdbxRead<'_>);
impl_submodule_read!(MdbxWrite<'_>);

impl SubmoduleWrite for MdbxWrite<'_> {
    fn add_full_data_path(
        &mut self,
        path_hash: ChunkPathHash,
        data_path: ChunkDataPath,
    ) -> eyre::Result<()> {
        add_full_data_path(self.tx, path_hash, data_path)
    }

    fn add_full_tx_path(&mut self, path_hash: TxPathHash, tx_path: TxPath) -> eyre::Result<()> {
        add_full_tx_path(self.tx, path_hash, tx_path)
    }

    fn add_tx_leaf_binding(
        &mut self,
        path_hash: TxPathHash,
        binding: &TxLeafBinding,
    ) -> eyre::Result<()> {
        add_tx_leaf_binding(self.tx, path_hash, binding)
    }

    fn add_data_path_hash_to_offset_index(
        &mut self,
        offset: PartitionChunkOffset,
        path_hash: Option<ChunkPathHash>,
    ) -> eyre::Result<()> {
        add_data_path_hash_to_offset_index(self.tx, offset, path_hash)
    }

    fn add_tx_path_hash_to_offset_index(
        &mut self,
        offset: PartitionChunkOffset,
        path_hash: Option<TxPathHash>,
    ) -> eyre::Result<()> {
        add_tx_path_hash_to_offset_index(self.tx, offset, path_hash)
    }

    fn add_tx_path_hash_to_offset_range(
        &mut self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
        path_hash: Option<TxPathHash>,
    ) -> eyre::Result<()> {
        add_tx_path_hash_to_offset_range(self.tx, start, end, path_hash)
    }

    fn write_data_path_updates(
        &mut self,
        updates: Vec<(PartitionChunkOffset, ChunkPathHash, ChunkDataPath)>,
    ) -> eyre::Result<()> {
        write_data_path_updates(self.tx, updates)
    }

    fn set_path_hashes_by_offset(
        &mut self,
        offset: PartitionChunkOffset,
        path_hashes: ChunkPathHashes,
    ) -> eyre::Result<()> {
        set_path_hashes_by_offset(self.tx, offset, path_hashes)
    }

    fn del_path_hashes_by_offset(&mut self, offset: PartitionChunkOffset) -> eyre::Result<()> {
        del_path_hashes_by_offset(self.tx, offset)
    }

    fn set_data_root_infos_for_data_root(
        &mut self,
        data_root: DataRoot,
        infos: DataRootInfos,
    ) -> eyre::Result<()> {
        set_data_root_infos_for_data_root(self.tx, data_root, infos)
    }

    fn add_data_root_info(&mut self, data_root: DataRoot, info: &DataRootInfo) -> eyre::Result<()> {
        add_data_root_info(self.tx, data_root, info)
    }

    fn add_pending_body_migration(
        &mut self,
        offset: PartitionChunkOffset,
        job: &PendingBodyMigration,
    ) -> eyre::Result<()> {
        add_pending_body_migration(self.tx, offset, job)
    }

    fn del_pending_body_migration(&mut self, offset: PartitionChunkOffset) -> eyre::Result<bool> {
        del_pending_body_migration(self.tx, offset)
    }

    fn del_pending_body_migrations_in_range(
        &mut self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<usize> {
        del_pending_body_migrations_in_range(self.tx, start, end)
    }

    fn clear(&mut self) -> eyre::Result<()> {
        clear_submodule_database(self.tx)
    }
}

impl SubmoduleStore for MdbxSubmoduleStore {
    fn view<R>(
        &self,
        f: impl FnOnce(&mut dyn SubmoduleRead) -> eyre::Result<R>,
    ) -> eyre::Result<R> {
        // `Database::view` returns the closure value. The `?` turns an open
        // failure into `eyre` and leaves the closure's own `Result` in place.
        self.env.view(|tx| {
            let mut read = MdbxRead { tx };
            f(&mut read)
        })?
    }

    fn update<R>(
        &self,
        f: impl FnOnce(&mut dyn SubmoduleWrite) -> eyre::Result<R>,
    ) -> eyre::Result<R> {
        self.env.update_eyre(|tx| {
            let mut write = MdbxWrite { tx };
            f(&mut write)
        })
    }
}

/// Which engine holds a submodule index. MDBX is the only variant today.
#[derive(Clone, Debug)]
pub enum SubmoduleIndex {
    Mdbx(MdbxSubmoduleStore),
}

impl SubmoduleIndex {
    pub fn open_mdbx(path: impl AsRef<Path>, args: DatabaseArguments) -> eyre::Result<Self> {
        Ok(Self::Mdbx(MdbxSubmoduleStore::open(path, args)?))
    }
}

impl SubmoduleStore for SubmoduleIndex {
    fn view<R>(
        &self,
        f: impl FnOnce(&mut dyn SubmoduleRead) -> eyre::Result<R>,
    ) -> eyre::Result<R> {
        match self {
            Self::Mdbx(store) => store.view(f),
        }
    }

    fn update<R>(
        &self,
        f: impl FnOnce(&mut dyn SubmoduleWrite) -> eyre::Result<R>,
    ) -> eyre::Result<R> {
        match self {
            Self::Mdbx(store) => store.update(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IrysDatabaseArgs as _;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{H256, RelativeChunkOffset};

    fn open_store(prefix: &str) -> eyre::Result<(impl Drop, SubmoduleIndex)> {
        let dir = TempDirBuilder::new().prefix(prefix).build();
        let store =
            SubmoduleIndex::open_mdbx(dir.path().join("db"), DatabaseArguments::irys_testing()?)?;
        Ok((dir, store))
    }

    #[test]
    fn update_commit_is_visible_to_the_next_view() -> eyre::Result<()> {
        let (_dir, store) = open_store("submodule_store_commit")?;
        let offset = PartitionChunkOffset::from(3);
        let hashes = ChunkPathHashes {
            data_path_hash: Some(H256::repeat_byte(1)),
            tx_path_hash: Some(H256::repeat_byte(2)),
        };
        store.update(|tx| tx.set_path_hashes_by_offset(offset, hashes.clone()))?;
        let got = store.view(|tx| tx.get_path_hashes_by_offset(offset))?;
        assert_eq!(got, Some(hashes));
        Ok(())
    }

    #[test]
    fn failed_update_does_not_commit() -> eyre::Result<()> {
        let (_dir, store) = open_store("submodule_store_abort")?;
        let offset = PartitionChunkOffset::from(1);
        let err = store
            .update(|tx| -> eyre::Result<()> {
                tx.set_path_hashes_by_offset(
                    offset,
                    ChunkPathHashes {
                        data_path_hash: Some(H256::repeat_byte(9)),
                        tx_path_hash: None,
                    },
                )?;
                eyre::bail!("reject batch");
            })
            .unwrap_err();
        assert!(err.to_string().contains("reject batch"));
        let got = store.view(|tx| tx.get_path_hashes_by_offset(offset))?;
        assert_eq!(got, None);
        Ok(())
    }

    #[test]
    fn write_batch_reads_its_own_uncommitted_appends() -> eyre::Result<()> {
        let (_dir, store) = open_store("submodule_store_ryw")?;
        let data_root = H256::repeat_byte(4);
        store.update(|tx| {
            tx.add_data_root_info(
                data_root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(0),
                    data_size: 32,
                },
            )?;
            tx.add_data_root_info(
                data_root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(8),
                    data_size: 64,
                },
            )?;
            let infos = tx
                .get_data_root_infos_for_data_root(data_root)?
                .expect("uncommitted appends are visible in this batch");
            assert_eq!(infos.0.len(), 2);
            Ok(())
        })?;
        let infos = store.view(|tx| tx.get_data_root_infos_for_data_root(data_root))?;
        assert_eq!(infos.expect("committed list").0.len(), 2);
        Ok(())
    }

    #[test]
    fn data_path_batch_keeps_an_existing_tx_path_hash() -> eyre::Result<()> {
        let (_dir, store) = open_store("submodule_store_data_path_batch")?;
        let offset = PartitionChunkOffset::from(4);
        let tx_hash = H256::repeat_byte(7);
        let data_hash = H256::repeat_byte(8);
        store.update(|tx| {
            tx.set_path_hashes_by_offset(
                offset,
                ChunkPathHashes {
                    data_path_hash: None,
                    tx_path_hash: Some(tx_hash),
                },
            )
        })?;
        store.update(|tx| tx.write_data_path_updates(vec![(offset, data_hash, vec![1, 2, 3])]))?;
        let hashes = store
            .view(|tx| tx.get_path_hashes_by_offset(offset))?
            .expect("offset row");
        assert_eq!(hashes.tx_path_hash, Some(tx_hash));
        assert_eq!(hashes.data_path_hash, Some(data_hash));
        let path = store.view(|tx| tx.get_full_data_path(data_hash))?;
        assert_eq!(path, Some(vec![1, 2, 3]));
        Ok(())
    }
}
