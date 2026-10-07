//! Submodule index boundary.
//!
//! Callers open one read view or one write batch. They do not see the
//! engine. A view is one committed snapshot: it does not see a commit that
//! starts after the view opens. A write batch reads its own uncommitted
//! writes, and `update` is exclusive until the closure returns. The batch
//! commits only when the closure returns `Ok`. A later engine has to keep
//! those three rules; MDBX gets them from its transaction.
//!
//! `update` stays durable. [`SubmoduleIndex::update_registration`] is the
//! same until group commit is enabled. Then the registration is visible
//! before it returns, and the fsync waits for the group.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use irys_types::{
    ChunkDataPath, ChunkPathHash, DataRoot, PartitionChunkOffset, TxPath, TxPathHash,
};
use reth_db::mdbx::ffi;
use reth_db::{Database as _, DatabaseEnv, mdbx::DatabaseArguments};

use super::group::{EnableStep, EngineLifetime, GroupCommit};

#[cfg(feature = "rocksdb")]
use super::rocks::{LegacySstSpan, RocksBackground, RocksSubmoduleStore, RocksTuning};
use super::{
    add_data_path_hash_to_offset_index, add_data_root_info, add_full_data_path, add_full_tx_path,
    add_pending_body_migration, add_tx_leaf_binding, add_tx_path_hash_to_offset_index,
    add_tx_path_hash_to_offset_range, clear_paths_in_inclusive_range, clear_submodule_database,
    create_or_open_submodule_db, del_path_hashes_by_offset, del_pending_body_migration,
    del_pending_body_migrations_in_range, first_missing_path_hash_offset_in_tx,
    get_data_path_by_offset, get_data_root_infos_for_data_root, get_full_tx_path,
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

    /// First offset in half-open `[start, end)` with no tx-path interval.
    fn first_missing_path_hash_offset(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Option<PartitionChunkOffset>>;

    /// Half-open spans inside `[start, end)` that no tx-path interval covers.
    fn missing_path_hash_ranges(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, PartitionChunkOffset)>>;

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
        offset: PartitionChunkOffset,
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

    /// Record one tx-path interval for the inclusive range `[start, end]`.
    ///
    /// Overlapping intervals are split. `None` removes coverage. A data-path
    /// hash already stored on an offset stays.
    fn add_tx_path_hash_to_offset_range(
        &mut self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
        path_hash: Option<TxPathHash>,
    ) -> eyre::Result<()>;

    /// Store each chunk's data path under its offset, and the hash on the offset row.
    ///
    /// The tx-path interval is left in place.
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

    /// Drop tx coverage and per-chunk path rows in the inclusive range.
    fn clear_paths_in_inclusive_range(
        &mut self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<()>;

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
    /// Dropped first so the delay thread joins before `env` closes.
    lifetime: Arc<EngineLifetime>,
    env: Arc<DatabaseEnv>,
    group: Arc<GroupCommit>,
    /// Held across `MDBX_SAFE_NOSYNC` and the enabled bit, and across a
    /// durable commit that still sees group commit as off. The flag makes
    /// the next commit skip fsync. `finish_durable` skips its own sync until
    /// the bit is set, so a commit between the two would return `Ok` and
    /// vanish on crash.
    nosync_enable: Arc<Mutex<()>>,
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
        let group = GroupCommit::new();
        Ok(Self {
            lifetime: EngineLifetime::new(Arc::clone(&group)),
            env: Arc::new(env),
            group,
            nosync_enable: Arc::new(Mutex::new(())),
        })
    }

    #[cfg(test)]
    fn interval_row_count(&self) -> eyre::Result<usize> {
        use reth_db::transaction::DbTx as _;

        self.env
            .view(|tx| Ok(tx.entries::<super::tables::TxPathIntervalByStart>()? as usize))?
    }

    /// Visible registration commits, one sync per group.
    ///
    /// `MDBX_SAFE_NOSYNC` is env-wide, so every later commit on this environment
    /// skips fsync. `update` syncs itself. A crash rolls back to the last sync
    /// and does not corrupt the file.
    pub(super) fn enable_group_commit(&self, txs_per_sync: u32) -> eyre::Result<()> {
        match self.group.prepare(txs_per_sync)? {
            EnableStep::On => Ok(()),
            EnableStep::Spawn => {
                if let Err(err) = self.spawn_delay() {
                    self.group.clear_spawned();
                    return Err(err);
                }
                self.arm_group()
            }
            EnableStep::Arm => self.arm_group(),
        }
    }

    pub(super) fn sync_group(&self) -> eyre::Result<()> {
        if !self.group.is_enabled() {
            return Ok(());
        }
        sync_if_dirty(&self.env, &self.group)
    }

    pub(super) fn update_registration<R>(
        &self,
        f: impl FnOnce(&mut dyn SubmoduleWrite) -> eyre::Result<R>,
    ) -> eyre::Result<R> {
        if !self.group.is_enabled() {
            return self.update(f);
        }
        let result = self.update_unsynced(f)?;
        if self.group.note_visible() {
            sync_if_dirty(&self.env, &self.group)?;
        }
        Ok(result)
    }

    fn spawn_delay(&self) -> eyre::Result<()> {
        let env = Arc::downgrade(&self.env);
        let group = Arc::clone(&self.group);
        self.lifetime
            .spawn(Arc::clone(&self.group), move || match env.upgrade() {
                Some(env) => sync_if_dirty(&env, &group),
                None => Ok(()),
            })
    }

    fn arm_group(&self) -> eyre::Result<()> {
        let _guard = self
            .nosync_enable
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        set_safe_nosync(&self.env)?;
        self.group.finish_enable();
        Ok(())
    }

    fn update_unsynced<R>(
        &self,
        f: impl FnOnce(&mut dyn SubmoduleWrite) -> eyre::Result<R>,
    ) -> eyre::Result<R> {
        self.env.update_eyre(|tx| {
            let mut write = MdbxWrite { tx };
            f(&mut write)
        })
    }

    #[cfg(test)]
    fn safe_nosync(&self) -> eyre::Result<bool> {
        let flags = env_flags(&self.env)?;
        Ok(flags & ffi::MDBX_SAFE_NOSYNC != 0)
    }
}

fn sync_if_dirty(env: &DatabaseEnv, group: &GroupCommit) -> eyre::Result<()> {
    let Some(generation) = group.dirty_generation() else {
        return Ok(());
    };
    sync_env(env)?;
    group.mark_synced(generation, true);
    Ok(())
}

fn finish_durable(env: &DatabaseEnv, group: &GroupCommit) -> eyre::Result<()> {
    if !group.is_enabled() {
        return Ok(());
    }
    // This commit is not a registration generation. Sync it, and cover any
    // registration that was already visible before the sync.
    let generation = group.generation();
    sync_env(env)?;
    group.mark_synced(generation, false);
    Ok(())
}

fn sync_env(env: &DatabaseEnv) -> eyre::Result<()> {
    let tx = env.tx()?;
    let environment = tx.inner().env().clone();
    drop(tx);
    environment
        .sync(true)
        .map_err(|err| eyre::eyre!("mdbx env sync: {err}"))?;
    Ok(())
}

fn set_safe_nosync(env: &DatabaseEnv) -> eyre::Result<()> {
    with_env(env, |ptr| {
        let rc = unsafe { ffi::mdbx_env_set_flags(ptr, ffi::MDBX_SAFE_NOSYNC, true) };
        if rc != 0 {
            eyre::bail!("mdbx_env_set_flags: {}", mdbx_error(rc));
        }
        Ok(())
    })
}

#[cfg(test)]
fn env_flags(env: &DatabaseEnv) -> eyre::Result<std::os::raw::c_uint> {
    with_env(env, |ptr| {
        let mut flags = 0;
        let rc = unsafe { ffi::mdbx_env_get_flags(ptr, &mut flags) };
        if rc != 0 {
            eyre::bail!("mdbx_env_get_flags: {}", mdbx_error(rc));
        }
        Ok(flags)
    })
}

fn with_env<T>(
    env: &DatabaseEnv,
    f: impl FnOnce(*mut ffi::MDBX_env) -> eyre::Result<T>,
) -> eyre::Result<T> {
    let tx = env.tx()?;
    let environment = tx.inner().env().clone();
    drop(tx);
    environment.with_raw_env_ptr(f)
}

fn mdbx_error(rc: i32) -> String {
    let msg = unsafe { std::ffi::CStr::from_ptr(ffi::mdbx_strerror(rc)) };
    format!("rc={rc} {}", msg.to_string_lossy())
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
        offset: PartitionChunkOffset,
        data_path: ChunkDataPath,
    ) -> eyre::Result<()> {
        add_full_data_path(self.tx, offset, data_path)
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

    fn clear_paths_in_inclusive_range(
        &mut self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<()> {
        clear_paths_in_inclusive_range(self.tx, start, end)
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
        if self.group.is_enabled() {
            let result = self.update_unsynced(f)?;
            finish_durable(&self.env, &self.group)?;
            return Ok(result);
        }
        // Group commit is off, or `arm_group` is between the flag and the
        // bit. Hold the same lock as `arm_group` so this commit cannot use
        // `MDBX_SAFE_NOSYNC` and then skip `sync_env`.
        let _guard = self
            .nosync_enable
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let result = self.update_unsynced(f)?;
        finish_durable(&self.env, &self.group)?;
        Ok(result)
    }
}

/// Which engine holds a submodule index. The node opens MDBX.
/// A RocksDB variant is compiled with the `rocksdb` feature.
#[derive(Clone, Debug)]
pub enum SubmoduleIndex {
    Mdbx(MdbxSubmoduleStore),
    #[cfg(feature = "rocksdb")]
    Rocks(RocksSubmoduleStore),
}

impl SubmoduleIndex {
    pub fn open_mdbx(path: impl AsRef<Path>, args: DatabaseArguments) -> eyre::Result<Self> {
        Ok(Self::Mdbx(MdbxSubmoduleStore::open(path, args)?))
    }

    #[cfg(feature = "rocksdb")]
    pub fn open_rocks(path: impl AsRef<Path>) -> eyre::Result<Self> {
        Ok(Self::Rocks(RocksSubmoduleStore::open(path)?))
    }

    /// Bench entry. Production uses [`Self::open_rocks`] and the shared cache size.
    #[cfg(feature = "rocksdb")]
    pub fn open_rocks_with_block_cache(
        path: impl AsRef<Path>,
        block_cache_bytes: usize,
    ) -> eyre::Result<Self> {
        Ok(Self::Rocks(RocksSubmoduleStore::open_with_block_cache(
            path,
            block_cache_bytes,
        )?))
    }

    /// Bench entry for one named preset. [`Self::open_rocks`] stays on the baseline.
    ///
    /// This open counts table opens and preloads every table (`max_open_files=-1`).
    /// Production [`Self::open_rocks`] does neither and stays at 512.
    #[cfg(feature = "rocksdb")]
    pub fn open_rocks_with(path: impl AsRef<Path>, tuning: RocksTuning) -> eyre::Result<Self> {
        Ok(Self::Rocks(RocksSubmoduleStore::open_with_stats(
            path, tuning,
        )?))
    }

    /// Bench entry for a schema-v1 Rocks directory. Does not rewrite the marker.
    #[cfg(feature = "rocksdb")]
    pub fn open_rocks_v1_probe(path: impl AsRef<Path>, tuning: RocksTuning) -> eyre::Result<Self> {
        Ok(Self::Rocks(RocksSubmoduleStore::open_v1_probe(
            path, tuning,
        )?))
    }

    /// Point read of one v1 path-hash row. `Ok(false)` is a missing key.
    #[cfg(feature = "rocksdb")]
    pub fn legacy_path_hash_present(&self, offset: PartitionChunkOffset) -> eyre::Result<bool> {
        match self {
            Self::Mdbx(_) => eyre::bail!("legacy path-hash probe is rocks only"),
            Self::Rocks(store) => store.legacy_path_hash_present(offset),
        }
    }

    /// Rewrite the v1 path-hash SST with this open's table options.
    #[cfg(feature = "rocksdb")]
    pub fn compact_legacy_path_hashes(&self) -> eyre::Result<()> {
        match self {
            Self::Mdbx(_) => eyre::bail!("legacy path-hash probe is rocks only"),
            Self::Rocks(store) => store.compact_legacy_path_hashes(),
        }
    }

    /// Live SST files in the v1 path-hash family.
    #[cfg(feature = "rocksdb")]
    pub fn legacy_path_hash_ssts(&self) -> eyre::Result<Vec<LegacySstSpan>> {
        match self {
            Self::Mdbx(_) => eyre::bail!("legacy path-hash probe is rocks only"),
            Self::Rocks(store) => store.legacy_path_hash_ssts(),
        }
    }

    /// Compaction and flush state. `None` on MDBX.
    #[cfg(feature = "rocksdb")]
    pub fn rocks_background(&self) -> eyre::Result<Option<RocksBackground>> {
        match self {
            Self::Mdbx(_) => Ok(None),
            Self::Rocks(store) => Ok(Some(store.background()?)),
        }
    }

    /// Settle on-disk files before a directory-size measurement.
    ///
    /// MDBX durable commits are already on disk. RocksDB flushes memtables,
    /// compacts each column family, and syncs the WAL.
    pub fn settle_files(&self) -> eyre::Result<()> {
        match self {
            Self::Mdbx(_) => Ok(()),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.flush_and_compact(),
        }
    }

    /// Turn on group commit for this index. `txs_per_sync` of 0 is rejected.
    ///
    /// Registration writes stay visible before they return. The fsync runs
    /// every `txs_per_sync` registrations, on [`Self::sync_group`], on the
    /// next durable [`SubmoduleStore::update`], or 50 ms after the first
    /// unsynced registration.
    pub fn enable_group_commit(&self, txs_per_sync: u32) -> eyre::Result<()> {
        match self {
            Self::Mdbx(store) => store.enable_group_commit(txs_per_sync),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.enable_group_commit(txs_per_sync),
        }
    }

    pub fn group_commit_enabled(&self) -> bool {
        match self {
            Self::Mdbx(store) => store.group.is_enabled(),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.group_commit_enabled(),
        }
    }

    /// Sync an open group. Does nothing when group commit is off or the group is clean.
    pub fn sync_group(&self) -> eyre::Result<()> {
        match self {
            Self::Mdbx(store) => store.sync_group(),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.sync_group(),
        }
    }

    /// Commit one index registration.
    ///
    /// Durable, unless [`Self::enable_group_commit`] is on. Then the rows are
    /// visible to the next view and the sync waits for the group.
    pub fn update_registration<R>(
        &self,
        f: impl FnOnce(&mut dyn SubmoduleWrite) -> eyre::Result<R>,
    ) -> eyre::Result<R> {
        match self {
            Self::Mdbx(store) => store.update_registration(f),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.update_registration(f),
        }
    }

    #[cfg(test)]
    fn interval_row_count(&self) -> eyre::Result<usize> {
        match self {
            Self::Mdbx(store) => store.interval_row_count(),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.interval_row_count(),
        }
    }

    #[cfg(test)]
    fn group_sync_count(&self) -> u64 {
        match self {
            Self::Mdbx(store) => store.group.sync_count(),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.group_sync_count(),
        }
    }

    #[cfg(test)]
    fn mdbx_safe_nosync(&self) -> eyre::Result<bool> {
        match self {
            Self::Mdbx(store) => store.safe_nosync(),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(_) => eyre::bail!("not an mdbx index"),
        }
    }
}

impl SubmoduleStore for SubmoduleIndex {
    fn view<R>(
        &self,
        f: impl FnOnce(&mut dyn SubmoduleRead) -> eyre::Result<R>,
    ) -> eyre::Result<R> {
        match self {
            Self::Mdbx(store) => store.view(f),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.view(f),
        }
    }

    fn update<R>(
        &self,
        f: impl FnOnce(&mut dyn SubmoduleWrite) -> eyre::Result<R>,
    ) -> eyre::Result<R> {
        match self {
            Self::Mdbx(store) => store.update(f),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.update(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IrysDatabaseArgs as _;
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::{DbSyncMode, H256, RelativeChunkOffset};
    use std::thread;
    use std::time::{Duration, Instant};

    fn put_registration(store: &SubmoduleIndex, offset: u32, byte: u8) -> eyre::Result<()> {
        store.update_registration(|tx| {
            tx.set_path_hashes_by_offset(
                PartitionChunkOffset::from(offset),
                ChunkPathHashes {
                    data_path_hash: Some(H256::repeat_byte(byte)),
                    tx_path_hash: None,
                },
            )
        })
    }

    fn path_at(store: &SubmoduleIndex, offset: u32) -> eyre::Result<Option<ChunkPathHashes>> {
        store.view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(offset)))
    }

    fn each_engine(
        prefix: &str,
        f: impl Fn(&SubmoduleIndex) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        let dir = TempDirBuilder::new().prefix(prefix).build();
        let mdbx =
            SubmoduleIndex::open_mdbx(dir.path().join("db"), DatabaseArguments::irys_testing()?)?;
        f(&mdbx)?;
        #[cfg(feature = "rocksdb")]
        {
            let rocks = SubmoduleIndex::open_rocks(dir.path().join("index"))?;
            f(&rocks)?;
        }
        Ok(())
    }

    #[test]
    fn update_commit_is_visible_to_the_next_view() -> eyre::Result<()> {
        each_engine("submodule_store_commit", |store| {
            let offset = PartitionChunkOffset::from(3);
            let hashes = ChunkPathHashes {
                data_path_hash: Some(H256::repeat_byte(1)),
                tx_path_hash: Some(H256::repeat_byte(2)),
            };
            store.update(|tx| tx.set_path_hashes_by_offset(offset, hashes.clone()))?;
            let got = store.view(|tx| tx.get_path_hashes_by_offset(offset))?;
            assert_eq!(got, Some(hashes));
            Ok(())
        })
    }

    #[test]
    fn failed_update_does_not_commit() -> eyre::Result<()> {
        each_engine("submodule_store_abort", |store| {
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
        })
    }

    #[test]
    fn write_batch_reads_its_own_uncommitted_appends() -> eyre::Result<()> {
        each_engine("submodule_store_ryw", |store| {
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
        })
    }

    #[test]
    fn tx_range_preserves_data_path_hashes_and_appends() -> eyre::Result<()> {
        each_engine("submodule_store_tx_range", |store| {
            let data_hash = H256::repeat_byte(4);
            let old_tx = H256::repeat_byte(5);
            let new_tx = H256::repeat_byte(6);
            let tail_tx = H256::repeat_byte(7);
            store.update(|tx| {
                tx.set_path_hashes_by_offset(
                    PartitionChunkOffset::from(2),
                    ChunkPathHashes {
                        data_path_hash: Some(data_hash),
                        tx_path_hash: Some(old_tx),
                    },
                )?;
                tx.add_tx_path_hash_to_offset_range(
                    PartitionChunkOffset::from(0),
                    PartitionChunkOffset::from(3),
                    Some(new_tx),
                )?;
                tx.add_tx_path_hash_to_offset_range(
                    PartitionChunkOffset::from(4),
                    PartitionChunkOffset::from(5),
                    Some(tail_tx),
                )
            })?;
            let at_two = path_at(store, 2)?.expect("offset 2");
            assert_eq!(at_two.data_path_hash, Some(data_hash));
            assert_eq!(at_two.tx_path_hash, Some(new_tx));
            let at_zero = path_at(store, 0)?.expect("offset 0");
            assert_eq!(at_zero.data_path_hash, None);
            assert_eq!(at_zero.tx_path_hash, Some(new_tx));
            let at_five = path_at(store, 5)?.expect("offset 5");
            assert_eq!(at_five.data_path_hash, None);
            assert_eq!(at_five.tx_path_hash, Some(tail_tx));
            store.update(|tx| {
                tx.add_tx_path_hash_to_offset_range(
                    PartitionChunkOffset::from(6),
                    PartitionChunkOffset::from(7),
                    Some(tail_tx),
                )
            })?;
            let at_six = path_at(store, 6)?.expect("offset 6");
            assert_eq!(at_six.data_path_hash, None);
            assert_eq!(at_six.tx_path_hash, Some(tail_tx));
            let at_two = path_at(store, 2)?.expect("offset 2 after append");
            assert_eq!(at_two.data_path_hash, Some(data_hash));
            Ok(())
        })
    }

    #[test]
    fn reassign_merges_with_the_left_neighbor() -> eyre::Result<()> {
        each_engine("submodule_store_merge_left", |store| {
            let hash_a = H256::repeat_byte(1);
            let hash_b = H256::repeat_byte(2);
            store.update(|tx| {
                tx.add_tx_path_hash_to_offset_range(
                    PartitionChunkOffset::from(0),
                    PartitionChunkOffset::from(4),
                    Some(hash_a),
                )?;
                tx.add_tx_path_hash_to_offset_range(
                    PartitionChunkOffset::from(5),
                    PartitionChunkOffset::from(9),
                    Some(hash_b),
                )
            })?;
            assert_eq!(store.interval_row_count()?, 2);
            store.update(|tx| {
                tx.add_tx_path_hash_to_offset_range(
                    PartitionChunkOffset::from(5),
                    PartitionChunkOffset::from(9),
                    Some(hash_a),
                )
            })?;
            assert_eq!(store.interval_row_count()?, 1);
            let at_nine = store
                .view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(9)))?
                .expect("merged span covers the old right edge");
            assert_eq!(at_nine.tx_path_hash, Some(hash_a));
            store.update(|tx| {
                tx.add_tx_path_hash_to_offset_range(
                    PartitionChunkOffset::from(0),
                    PartitionChunkOffset::from(2),
                    Some(hash_b),
                )
            })?;
            assert_eq!(store.interval_row_count()?, 2);
            let at_three = store
                .view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(3)))?
                .expect("right remnant");
            assert_eq!(at_three.tx_path_hash, Some(hash_a));
            let at_nine = store
                .view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(9)))?
                .expect("right remnant still reaches the old end");
            assert_eq!(at_nine.tx_path_hash, Some(hash_a));
            Ok(())
        })
    }

    #[test]
    fn data_path_batch_keeps_an_existing_tx_path_hash() -> eyre::Result<()> {
        each_engine("submodule_store_data_path_batch", |store| {
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
            store.update(|tx| {
                tx.write_data_path_updates(vec![(offset, data_hash, vec![1, 2, 3])])
            })?;
            let hashes = store
                .view(|tx| tx.get_path_hashes_by_offset(offset))?
                .expect("offset row");
            assert_eq!(hashes.tx_path_hash, Some(tx_hash));
            assert_eq!(hashes.data_path_hash, Some(data_hash));
            let path = store.view(|tx| tx.get_data_path_by_offset(offset))?;
            assert_eq!(path, Some(vec![1, 2, 3]));
            Ok(())
        })
    }

    #[test]
    fn uncommitted_writes_change_gap_scans() -> eyre::Result<()> {
        each_engine("submodule_store_gaps", |store| {
            store.update(|tx| {
                let tx_hash = H256::repeat_byte(3);
                tx.add_tx_path_hash_to_offset_range(
                    PartitionChunkOffset::from(0),
                    PartitionChunkOffset::from(0),
                    Some(tx_hash),
                )?;
                tx.add_tx_path_hash_to_offset_range(
                    PartitionChunkOffset::from(2),
                    PartitionChunkOffset::from(2),
                    Some(tx_hash),
                )?;
                tx.write_data_path_updates(vec![(
                    PartitionChunkOffset::from(1),
                    H256::repeat_byte(4),
                    vec![1],
                )])?;
                assert_eq!(
                    tx.missing_path_hash_ranges(
                        PartitionChunkOffset::from(0),
                        PartitionChunkOffset::from(4),
                    )?,
                    vec![
                        (PartitionChunkOffset::from(1), PartitionChunkOffset::from(2)),
                        (PartitionChunkOffset::from(3), PartitionChunkOffset::from(4)),
                    ]
                );
                tx.del_path_hashes_by_offset(PartitionChunkOffset::from(0))?;
                assert_eq!(
                    tx.first_missing_path_hash_offset(
                        PartitionChunkOffset::from(0),
                        PartitionChunkOffset::from(4),
                    )?,
                    Some(PartitionChunkOffset::from(0))
                );
                assert!(
                    tx.get_data_path_by_offset(PartitionChunkOffset::from(1))?
                        .is_some(),
                    "a data path does not fill a tx-coverage hole"
                );
                Ok(())
            })?;
            Ok(())
        })
    }

    #[test]
    fn clear_then_write_in_one_batch_drops_only_the_old_rows() -> eyre::Result<()> {
        each_engine("submodule_store_clear", |store| {
            let hashes = ChunkPathHashes {
                data_path_hash: Some(H256::repeat_byte(5)),
                tx_path_hash: None,
            };
            store.update(|tx| {
                tx.set_path_hashes_by_offset(PartitionChunkOffset::from(5), hashes.clone())?;
                tx.clear()?;
                assert!(
                    tx.get_path_hashes_by_offset(PartitionChunkOffset::from(5))?
                        .is_none()
                );
                tx.set_path_hashes_by_offset(PartitionChunkOffset::from(6), hashes.clone())?;
                Ok(())
            })?;
            assert!(
                store
                    .view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(5)))?
                    .is_none()
            );
            assert_eq!(
                store.view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(6)))?,
                Some(hashes)
            );
            Ok(())
        })
    }

    #[test]
    fn registration_without_group_commit_stays_durable() -> eyre::Result<()> {
        each_engine("submodule_store_reg_durable", |store| {
            assert!(!store.group_commit_enabled());
            put_registration(store, 1, 1)?;
            assert_eq!(store.group_sync_count(), 0);
            assert!(path_at(store, 1)?.is_some());
            store.sync_group()?;
            assert_eq!(store.group_sync_count(), 0);
            Ok(())
        })
    }

    #[test]
    fn group_commit_write_is_visible_before_sync() -> eyre::Result<()> {
        each_engine("submodule_store_group_visible", |store| {
            store.enable_group_commit(1_000)?;
            put_registration(store, 3, 4)?;
            assert_eq!(store.group_sync_count(), 0);
            assert_eq!(
                path_at(store, 3)?
                    .expect("visible before sync")
                    .data_path_hash,
                Some(H256::repeat_byte(4))
            );
            store.sync_group()?;
            assert_eq!(store.group_sync_count(), 1);
            assert!(path_at(store, 3)?.is_some());
            Ok(())
        })
    }

    #[test]
    fn group_commit_syncs_every_n_registrations() -> eyre::Result<()> {
        each_engine("submodule_store_group_n", |store| {
            store.enable_group_commit(2)?;
            put_registration(store, 1, 1)?;
            assert_eq!(store.group_sync_count(), 0);
            put_registration(store, 2, 2)?;
            assert_eq!(store.group_sync_count(), 1);
            put_registration(store, 3, 3)?;
            assert_eq!(store.group_sync_count(), 1);
            assert!(path_at(store, 3)?.is_some());
            Ok(())
        })
    }

    #[test]
    fn durable_write_covers_an_open_group() -> eyre::Result<()> {
        each_engine("submodule_store_group_cover", |store| {
            store.enable_group_commit(8)?;
            put_registration(store, 1, 1)?;
            assert_eq!(store.group_sync_count(), 0);
            store.update(|tx| {
                tx.set_path_hashes_by_offset(
                    PartitionChunkOffset::from(2),
                    ChunkPathHashes {
                        data_path_hash: Some(H256::repeat_byte(2)),
                        tx_path_hash: None,
                    },
                )
            })?;
            assert_eq!(store.group_sync_count(), 0);
            store.sync_group()?;
            assert_eq!(store.group_sync_count(), 0);
            assert!(path_at(store, 1)?.is_some());
            assert!(path_at(store, 2)?.is_some());
            Ok(())
        })
    }

    #[test]
    fn group_commit_rejects_zero() -> eyre::Result<()> {
        each_engine("submodule_store_group_zero", |store| {
            let err = store.enable_group_commit(0).unwrap_err();
            assert!(err.to_string().contains("at least 1"));
            assert!(!store.group_commit_enabled());
            Ok(())
        })
    }

    #[test]
    fn quiet_registration_syncs_after_the_delay() -> eyre::Result<()> {
        each_engine("submodule_store_group_delay", |store| {
            store.enable_group_commit(1_000)?;
            let started = Instant::now();
            put_registration(store, 7, 7)?;
            assert_eq!(store.group_sync_count(), 0);
            while store.group_sync_count() == 0 {
                if started.elapsed() > Duration::from_secs(2) {
                    eyre::bail!("group sync did not run within 2s");
                }
                thread::sleep(Duration::from_millis(10));
            }
            let elapsed = started.elapsed();
            assert!(
                elapsed >= Duration::from_millis(40),
                "group sync ran after {elapsed:?}"
            );
            assert!(path_at(store, 7)?.is_some());
            Ok(())
        })
    }

    #[test]
    fn mdbx_group_commit_sets_safe_nosync() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_store_safe_nosync")
            .build();
        let args = DatabaseArguments::irys_default(DbSyncMode::Durable)?
            .with_geometry_max_size(Some(irys_types::TEST_DB_GEOMETRY_MAX_SIZE));
        let store = SubmoduleIndex::open_mdbx(dir.path().join("db"), args)?;
        assert!(!store.mdbx_safe_nosync()?);
        put_registration(&store, 1, 1)?;
        assert_eq!(store.group_sync_count(), 0);
        store.enable_group_commit(8)?;
        assert!(store.mdbx_safe_nosync()?);
        assert!(store.group_commit_enabled());
        Ok(())
    }
}
