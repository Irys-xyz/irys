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
//! before it returns. MDBX syncs the group on a count, a durable write, or
//! 50 ms. RocksDB leaves it unsynced until the next synced write, the WAL
//! backstop, or shutdown. A failed RocksDB WAL sync rejects later `update`
//! and `update_covered` calls.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use irys_types::{
    ChunkDataPath, ChunkPathHash, DataRoot, PartitionChunkOffset, TxPath, TxPathHash,
};
use reth_db::mdbx::ffi;
use reth_db::{Database as _, DatabaseEnv, mdbx::DatabaseArguments};

use super::group::{EnableStep, EngineLifetime, GroupCommit};

use super::db::{
    delete_presence_in_ranges, delete_reset_marker, has_pending_body_migration_in_range,
    list_data_root_infos, presence_in_ranges, put_reset_marker, read_reset_marker,
    trim_data_root_placements, tx_interval_overlaps,
};
#[cfg(feature = "rocksdb")]
use super::rocks::{
    LegacySstSpan, RocksBackground, RocksIngestStats, RocksSubmoduleStore, RocksTuning,
};
use super::{
    add_data_path_hash_to_offset_index, add_data_root_info, add_full_data_path, add_full_tx_path,
    add_pending_body_migration, add_tx_leaf_binding, add_tx_path_hash_to_offset_index,
    add_tx_path_hash_to_offset_range, clear_paths_in_inclusive_range, clear_submodule_database,
    del_path_hashes_by_offset, del_pending_body_migration, del_pending_body_migrations_in_range,
    first_missing_path_hash_offset_in_tx, get_data_path_by_offset,
    get_data_root_infos_for_data_root, get_full_tx_path, get_path_hashes_by_offset,
    get_pending_body_migration, get_tx_leaf_binding, get_tx_path_by_offset,
    missing_path_hash_ranges_in_tx, path_hashes_in_inclusive_range, pending_body_migrations_from,
    set_data_root_infos_for_data_root, set_path_hashes_by_offset,
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

    /// Jobs at or after `start` (`None` is the first key), in ascending offset
    /// order. `limit` stops the cursor after that many rows. `None` reads the
    /// rest.
    fn pending_body_migrations_from(
        &self,
        start: Option<PartitionChunkOffset>,
        limit: Option<usize>,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, PendingBodyMigration)>>;

    /// `true` when a pending-job key lies in `[start, end]`.
    ///
    /// The key is the offset. The scan does not read the job value. The
    /// iterator stops at `end`.
    fn has_pending_in_inclusive_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<bool>;

    /// `true` when a tx-path interval overlaps `[start, end]`.
    ///
    /// The value holds the inclusive end, so the scan reads it.
    fn tx_interval_overlaps(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<bool>;

    /// Every data-root row.
    ///
    /// The key is a hash. It has no offset order. The value lists placements,
    /// so a span check has to read it. The scan does not fill the block cache.
    fn data_root_entries(&self) -> eyre::Result<Vec<(DataRoot, DataRootInfos)>>;
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

/// Rows removed from the two presence families.
///
/// A presence row says the bytes at that offset are durable data. Tx-path
/// intervals, tx paths, data-root info, tx-leaf bindings, pending body
/// migrations, and the schema row are not presence.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PresenceReconcile {
    pub path_hash_rows: u64,
    pub data_path_rows: u64,
}

impl PresenceReconcile {
    pub fn rows_removed(self) -> u64 {
        self.path_hash_rows.saturating_add(self.data_path_rows)
    }
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

    /// Delete presence rows in each inclusive span.
    ///
    /// Presence is the offset path-hash row and the offset data-path row.
    /// An empty list, or spans that hold no presence row, does not commit.
    /// One commit covers every span that has a row. Cost is one seek per span
    /// per presence family, plus the rows removed. RocksDB deletes with
    /// `DeleteRange` and does not read values. MDBX walks a cursor inside
    /// each span.
    fn drop_presence_in_ranges(
        &self,
        ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) -> eyre::Result<PresenceReconcile>;

    /// Drop tx intervals, data-root placements, and pending jobs that overlap
    /// `spans`.
    ///
    /// Callers pass `Uninitialized` spans only. An `Entropy` span keeps its
    /// tx index: those rows name bytes that are not stored yet. An empty
    /// list, or spans with no such rows, does not commit. A tx-path blob and
    /// its leaf binding stay while any interval in this submodule names that
    /// hash. The last such interval deletes both rows. A tx interval that
    /// starts before the span is split, not deleted as a key range.
    fn drop_uninitialized_coverage(
        &self,
        spans: &[(PartitionChunkOffset, PartitionChunkOffset)],
        chunk_size: u64,
    ) -> eyre::Result<()>;

    /// `true` when a reset started and has not dropped its marker.
    fn reset_in_progress(&self) -> eyre::Result<bool>;

    /// Durable marker. Same sync as `update`. `clear` does not remove it.
    fn set_reset_in_progress(&self) -> eyre::Result<()>;

    /// Remove the reset marker. Same sync as `update`.
    fn clear_reset_in_progress(&self) -> eyre::Result<()>;
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
        Ok(Self::open_with_stats(path, args)?.0)
    }

    /// Opens the index and reports whether schema v1 was migrated.
    ///
    /// A v1 directory is migrated in place before this returns. The exclusive
    /// lock is the caller's `args` flag.
    pub(super) fn open_with_stats(
        path: impl AsRef<Path>,
        args: DatabaseArguments,
    ) -> eyre::Result<(Self, super::migrate::MigrationStats)> {
        let (env, stats) = super::db::open_submodule_index(
            path,
            args,
            super::migrate::SubmoduleMigrationOptions::default(),
        )?;
        Ok((Self::from_env(env), stats))
    }

    /// Creates an MDBX directory for conversion and writes the incomplete marker.
    ///
    /// Does not run the v1 migration and does not write [`super::tables::SUBMODULE_SCHEMA`].
    /// That stamp is [`Self::put_schema_bytes`] after the copy. A crash before
    /// the stamp leaves the incomplete marker, which open refuses.
    #[cfg(feature = "rocksdb")]
    pub(super) fn open_for_convert(
        path: impl AsRef<Path>,
        args: DatabaseArguments,
    ) -> eyre::Result<Self> {
        let env = crate::open_or_create_db(path, super::tables::SubmoduleTables::ALL, args)?;
        let store = Self::from_env(env);
        match store.schema_bytes()? {
            Some(bytes) if bytes.as_slice() == super::tables::SUBMODULE_SCHEMA.as_bytes() => {
                eyre::bail!("refusing to convert into a completed MDBX index");
            }
            _ => {}
        }
        store.put_schema_bytes(super::tables::SUBMODULE_CONVERT_INCOMPLETE.as_bytes())?;
        Ok(store)
    }

    fn from_env(env: DatabaseEnv) -> Self {
        let group = GroupCommit::new();
        Self {
            lifetime: EngineLifetime::new(Arc::clone(&group)),
            env: Arc::new(env),
            group,
            nosync_enable: Arc::new(Mutex::new(())),
        }
    }

    #[cfg(feature = "rocksdb")]
    pub(super) fn schema_bytes(&self) -> eyre::Result<Option<Vec<u8>>> {
        self.env.view_eyre(super::db::read_schema_bytes)
    }

    #[cfg(feature = "rocksdb")]
    pub(super) fn put_schema_bytes(&self, bytes: &[u8]) -> eyre::Result<()> {
        let owned = bytes.to_vec();
        self.env
            .update_eyre(|tx| super::db::write_schema_bytes(tx, &owned))?;
        sync_env(&self.env)
    }

    /// Next rows after `after`, in key order. One row is returned even when
    /// it is larger than `limit_bytes`, so the scan always moves forward.
    #[cfg(feature = "rocksdb")]
    pub(super) fn read_rows<T, F>(
        &self,
        after: Option<T::Key>,
        limit_rows: usize,
        limit_bytes: usize,
        value_len: F,
    ) -> eyre::Result<Vec<(T::Key, T::Value)>>
    where
        T: reth_db::table::Table,
        T::Key: Copy + Ord,
        F: Fn(&T::Value) -> usize,
    {
        use reth_db::cursor::DbCursorRO as _;
        use reth_db::transaction::DbTx as _;

        let limit_rows = limit_rows.max(1);
        let limit_bytes = limit_bytes.max(1);
        self.env.view_eyre(|tx| {
            let mut cursor = tx.cursor_read::<T>()?;
            let walker = cursor.walk(after)?;
            let mut out = Vec::new();
            let mut bytes = 0_usize;
            for entry in walker {
                let (key, value) = entry?;
                if after.is_some_and(|prev| key <= prev) {
                    continue;
                }
                let next_bytes = bytes.saturating_add(value_len(&value));
                if !out.is_empty() && (out.len() >= limit_rows || next_bytes > limit_bytes) {
                    break;
                }
                bytes = next_bytes;
                out.push((key, value));
                if out.len() >= limit_rows || bytes >= limit_bytes {
                    break;
                }
            }
            Ok(out)
        })
    }

    /// Next keys after `after`, in key order. Values are not kept.
    #[cfg(feature = "rocksdb")]
    pub(super) fn read_keys<T>(
        &self,
        after: Option<T::Key>,
        limit: usize,
    ) -> eyre::Result<Vec<T::Key>>
    where
        T: reth_db::table::Table,
        T::Key: Copy + Ord,
    {
        use reth_db::cursor::DbCursorRO as _;
        use reth_db::transaction::DbTx as _;

        let limit = limit.max(1);
        self.env.view_eyre(|tx| {
            let mut cursor = tx.cursor_read::<T>()?;
            let walker = cursor.walk(after)?;
            let mut out = Vec::new();
            for entry in walker {
                let (key, _) = entry?;
                if after.is_some_and(|prev| key <= prev) {
                    continue;
                }
                out.push(key);
                if out.len() == limit {
                    break;
                }
            }
            Ok(out)
        })
    }

    #[cfg(test)]
    fn interval_row_count(&self) -> eyre::Result<usize> {
        use reth_db::transaction::DbTx as _;

        self.env
            .view(|tx| Ok(tx.entries::<super::tables::TxPathIntervalByStart>()?))?
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

    /// Same durability as [`Self::update`]. MDBX has no WAL to ride.
    pub(super) fn update_covered<R>(
        &self,
        f: impl FnOnce(&mut dyn SubmoduleWrite) -> eyre::Result<R>,
    ) -> eyre::Result<R> {
        self.update(f)
    }

    fn spawn_delay(&self) -> eyre::Result<()> {
        let env = Arc::downgrade(&self.env);
        let group = Arc::clone(&self.group);
        self.lifetime
            .spawn(Arc::clone(&self.group), move || match env.upgrade() {
                Some(env) => {
                    let result = sync_if_dirty(&env, &group);
                    if let Err(ref err) = result {
                        tracing::error!(%err, "submodule index group sync failed");
                    }
                    result
                }
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

    /// Latest committed write id. A read leaves it unchanged.
    pub(super) fn committed_sequence(&self) -> eyre::Result<u64> {
        self.env
            .view_eyre(|tx| tx.id().map_err(|err| eyre::eyre!("mdbx txn id: {err}")))
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
                limit: Option<usize>,
            ) -> eyre::Result<Vec<(PartitionChunkOffset, PendingBodyMigration)>> {
                pending_body_migrations_from(self.tx, start, limit)
            }

            fn has_pending_in_inclusive_range(
                &self,
                start: PartitionChunkOffset,
                end: PartitionChunkOffset,
            ) -> eyre::Result<bool> {
                has_pending_body_migration_in_range(self.tx, start, end)
            }

            fn tx_interval_overlaps(
                &self,
                start: PartitionChunkOffset,
                end: PartitionChunkOffset,
            ) -> eyre::Result<bool> {
                tx_interval_overlaps(self.tx, start, end)
            }

            fn data_root_entries(&self) -> eyre::Result<Vec<(DataRoot, DataRootInfos)>> {
                list_data_root_infos(self.tx)
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

    fn drop_presence_in_ranges(
        &self,
        ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) -> eyre::Result<PresenceReconcile> {
        if ranges.iter().all(|(start, end)| start > end) {
            return Ok(PresenceReconcile::default());
        }
        // One seek per span per family. A miss does not open a write txn.
        let present = self.env.view_eyre(|tx| presence_in_ranges(tx, ranges))?;
        if !present {
            return Ok(PresenceReconcile::default());
        }
        let commit = |tx: &_| {
            let (path_hash_rows, data_path_rows) = delete_presence_in_ranges(tx, ranges)?;
            Ok(PresenceReconcile {
                path_hash_rows,
                data_path_rows,
            })
        };
        // Same durability split as `update`. Group commit is off at module
        // open, so this commit is a normal synced write.
        if self.group.is_enabled() {
            let stats = self.env.update_eyre(commit)?;
            finish_durable(&self.env, &self.group)?;
            return Ok(stats);
        }
        let _guard = self
            .nosync_enable
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let stats = self.env.update_eyre(commit)?;
        finish_durable(&self.env, &self.group)?;
        Ok(stats)
    }

    fn drop_uninitialized_coverage(
        &self,
        spans: &[(PartitionChunkOffset, PartitionChunkOffset)],
        chunk_size: u64,
    ) -> eyre::Result<()> {
        drop_uninitialized_coverage_on(self, spans, chunk_size)
    }

    fn reset_in_progress(&self) -> eyre::Result<bool> {
        self.env.view_eyre(read_reset_marker)
    }

    fn set_reset_in_progress(&self) -> eyre::Result<()> {
        self.commit_marker(true)
    }

    fn clear_reset_in_progress(&self) -> eyre::Result<()> {
        self.commit_marker(false)
    }
}

impl MdbxSubmoduleStore {
    /// Same durability split as `update`. The marker is not a registration.
    fn commit_marker(&self, present: bool) -> eyre::Result<()> {
        let commit = |tx: &_| {
            if present {
                put_reset_marker(tx)
            } else {
                delete_reset_marker(tx)
            }
        };
        if self.group.is_enabled() {
            self.env.update_eyre(commit)?;
            finish_durable(&self.env, &self.group)?;
            return Ok(());
        }
        let _guard = self
            .nosync_enable
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.env.update_eyre(commit)?;
        finish_durable(&self.env, &self.group)?;
        Ok(())
    }
}

fn spans_need_coverage_drop(
    tx: &mut dyn SubmoduleRead,
    spans: &[(PartitionChunkOffset, PartitionChunkOffset)],
    chunk_size: u64,
) -> eyre::Result<bool> {
    for &(start, end) in spans {
        if tx.has_pending_in_inclusive_range(start, end)? || tx.tx_interval_overlaps(start, end)? {
            return Ok(true);
        }
    }
    for (_, infos) in tx.data_root_entries()? {
        for info in &infos.0 {
            if spans.iter().any(|(start, end)| {
                super::db::placement_overlaps_inclusive_span(info, *start, *end, chunk_size)
            }) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn drop_coverage_rows(
    tx: &mut dyn SubmoduleWrite,
    spans: &[(PartitionChunkOffset, PartitionChunkOffset)],
    chunk_size: u64,
) -> eyre::Result<()> {
    for &(start, end) in spans {
        tx.add_tx_path_hash_to_offset_range(start, end, None)?;
        tx.del_pending_body_migrations_in_range(start, end)?;
    }
    for (root, infos) in tx.data_root_entries()? {
        if let Some(remaining) = trim_data_root_placements(infos, spans, chunk_size) {
            tx.set_data_root_infos_for_data_root(root, remaining)?;
        }
    }
    Ok(())
}

pub(super) fn drop_uninitialized_coverage_on(
    store: &impl SubmoduleStore,
    spans: &[(PartitionChunkOffset, PartitionChunkOffset)],
    chunk_size: u64,
) -> eyre::Result<()> {
    let spans: Vec<_> = spans
        .iter()
        .copied()
        .filter(|(start, end)| start <= end)
        .collect();
    if spans.is_empty() {
        return Ok(());
    }
    let present = store.view(|tx| spans_need_coverage_drop(tx, &spans, chunk_size))?;
    if !present {
        return Ok(());
    }
    store.update(|tx| drop_coverage_rows(tx, &spans, chunk_size))
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
    /// This open skips the table-property scan during `DB::Open`.
    /// Production [`Self::open_rocks`] keeps every SST open and exports tickers.
    #[cfg(feature = "rocksdb")]
    pub fn open_rocks_with(path: impl AsRef<Path>, tuning: RocksTuning) -> eyre::Result<Self> {
        Ok(Self::Rocks(RocksSubmoduleStore::open_with_stats(
            path, tuning,
        )?))
    }

    /// SST and blob files per column family. Empty on MDBX.
    #[cfg(feature = "rocksdb")]
    pub fn column_file_usage(&self) -> eyre::Result<Vec<crate::submodule::ColumnFileUsage>> {
        match self {
            Self::Mdbx(_) => Ok(Vec::new()),
            Self::Rocks(store) => store.column_file_usage(),
        }
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

    /// Publish RocksDB gauges and ticker counters. MDBX does nothing.
    pub fn report_metrics(&self) {
        match self {
            Self::Mdbx(_) => {}
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.report_metrics(),
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

    /// RocksDB write-stall, compaction, and flush counters.
    ///
    /// `None` for MDBX. The counters are property reads on the open database.
    #[cfg(feature = "rocksdb")]
    pub fn rocks_ingest_stats(&self) -> eyre::Result<Option<RocksIngestStats>> {
        match self {
            Self::Mdbx(_) => Ok(None),
            Self::Rocks(store) => Ok(Some(store.ingest_stats()?)),
        }
    }

    /// Turn on group commit for this index. `txs_per_sync` of 0 is rejected.
    ///
    /// Registration writes stay visible before they return. MDBX syncs every
    /// `txs_per_sync` registrations, on [`Self::sync_group`], on the next
    /// durable [`SubmoduleStore::update`], or 50 ms after the first unsynced
    /// registration. RocksDB does not sync on that count. The next synced
    /// write on that database, the WAL backstop, or clean shutdown makes the
    /// records durable. [`Self::set_wal_backstop`] sets the Rocks bound.
    /// An unset bound is one second.
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
    /// visible to the next view. MDBX syncs with the group. RocksDB does not
    /// sync this write. A caller that publishes an external effect must
    /// [`Self::sync_group`] each touched index first.
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

    /// Commit rows that must exist before a later durable write, and need
    /// not sync on their own.
    ///
    /// MDBX delegates to [`SubmoduleStore::update`], so the commit stays
    /// durable. RocksDB with group commit on appends the WAL without
    /// syncing. [`Self::published_generation`] advances.
    /// [`Self::wait_until_durable`] waits until a later synced write or the
    /// backstop covers that generation. With group commit off, this is
    /// [`SubmoduleStore::update`]. A RocksDB WAL sync failure makes this
    /// and [`SubmoduleStore::update`] return an error.
    pub fn update_covered<R>(
        &self,
        f: impl FnOnce(&mut dyn SubmoduleWrite) -> eyre::Result<R>,
    ) -> eyre::Result<R> {
        match self {
            Self::Mdbx(store) => store.update_covered(f),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.update_covered(f),
        }
    }

    /// Bound how long a RocksDB index may leave an unsynced write in the
    /// page cache. MDBX ignores this and keeps its 50 ms group delay.
    ///
    /// [`Duration::ZERO`] makes the backstop thread sync as soon as it
    /// wakes. It does not sync on this thread. Call this before
    /// [`Self::enable_group_commit`] so the delay thread starts with the
    /// bound. A later call updates a running thread.
    pub fn set_wal_backstop(&self, delay: Duration) {
        match self {
            // The parameter is read only by the RocksDB engine.
            Self::Mdbx(_) => {
                let _ = delay;
            }
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.set_wal_backstop(delay),
        }
    }

    /// Covered writes visible so far. Not a durability watermark.
    ///
    /// Sample this after the write returns. A concurrent newer write can
    /// only make [`Self::wait_until_durable`] wait longer.
    pub fn published_generation(&self) -> u64 {
        match self {
            Self::Mdbx(store) => store.group.generation(),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.published_generation(),
        }
    }

    /// Highest generation a synced write or the backstop has covered.
    pub fn durable_generation(&self) -> u64 {
        match self {
            Self::Mdbx(store) => store.group.synced_generation(),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.durable_generation(),
        }
    }

    /// Block until `generation` is covered by a synced write or the backstop.
    ///
    /// Do not call this while holding the index write lock. After the index
    /// stops, this returns an error if `generation` is still unsynced for 5 s.
    pub fn wait_until_durable(&self, generation: u64) -> eyre::Result<()> {
        match self {
            Self::Mdbx(store) => store.group.wait_until_synced(generation),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.wait_until_durable(generation),
        }
    }

    /// Synced WAL writes since open, including backstop syncs. MDBX is 0.
    pub fn wal_sync_count(&self) -> u64 {
        match self {
            Self::Mdbx(_) => 0,
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.wal_sync_count(),
        }
    }

    /// Backstop, explicit group sync, and shutdown WAL syncs. MDBX is 0.
    pub fn wal_backstop_sync_count(&self) -> u64 {
        match self {
            Self::Mdbx(_) => 0,
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.wal_backstop_sync_count(),
        }
    }

    /// The next dirty RocksDB WAL sync fails once. Later writes then fail.
    #[cfg(all(feature = "rocksdb", any(test, feature = "test-utils")))]
    pub fn fail_next_wal_sync(&self) {
        match self {
            Self::Mdbx(_) => panic!("fail_next_wal_sync is rocks only"),
            Self::Rocks(store) => store.fail_next_wal_sync(),
        }
    }

    /// `true` after a RocksDB WAL sync failure until this index is dropped.
    #[cfg(all(feature = "rocksdb", any(test, feature = "test-utils")))]
    pub fn wal_sync_failed(&self) -> bool {
        match self {
            Self::Mdbx(_) => false,
            Self::Rocks(store) => store.wal_sync_failed(),
        }
    }

    #[cfg(all(test, feature = "rocksdb"))]
    fn wal_backstop_delay(&self) -> Duration {
        match self {
            Self::Mdbx(store) => store.group.delay(),
            Self::Rocks(store) => store.wal_backstop_delay(),
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

    /// Latest committed write id.
    ///
    /// A read leaves it unchanged. A reconcile that finds no presence row does
    /// not move it. MDBX reports the snapshot id. RocksDB reports the WAL
    /// sequence.
    pub fn committed_sequence(&self) -> eyre::Result<u64> {
        match self {
            Self::Mdbx(store) => store.committed_sequence(),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.committed_sequence(),
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

    fn drop_presence_in_ranges(
        &self,
        ranges: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) -> eyre::Result<PresenceReconcile> {
        match self {
            Self::Mdbx(store) => store.drop_presence_in_ranges(ranges),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.drop_presence_in_ranges(ranges),
        }
    }

    fn drop_uninitialized_coverage(
        &self,
        spans: &[(PartitionChunkOffset, PartitionChunkOffset)],
        chunk_size: u64,
    ) -> eyre::Result<()> {
        match self {
            Self::Mdbx(store) => store.drop_uninitialized_coverage(spans, chunk_size),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.drop_uninitialized_coverage(spans, chunk_size),
        }
    }

    fn reset_in_progress(&self) -> eyre::Result<bool> {
        match self {
            Self::Mdbx(store) => store.reset_in_progress(),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.reset_in_progress(),
        }
    }

    fn set_reset_in_progress(&self) -> eyre::Result<()> {
        match self {
            Self::Mdbx(store) => store.set_reset_in_progress(),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.set_reset_in_progress(),
        }
    }

    fn clear_reset_in_progress(&self) -> eyre::Result<()> {
        match self {
            Self::Mdbx(store) => store.clear_reset_in_progress(),
            #[cfg(feature = "rocksdb")]
            Self::Rocks(store) => store.clear_reset_in_progress(),
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

    fn index_tx_span(
        tx: &mut dyn SubmoduleWrite,
        hash: H256,
        start: u32,
        end: u32,
    ) -> eyre::Result<()> {
        tx.add_full_tx_path(hash, vec![hash.0[0]])?;
        tx.add_tx_leaf_binding(
            hash,
            &TxLeafBinding {
                data_root: hash,
                prefix_hash: H256::repeat_byte(1),
            },
        )?;
        tx.add_tx_path_hash_to_offset_range(
            PartitionChunkOffset::from(start),
            PartitionChunkOffset::from(end),
            Some(hash),
        )
    }

    fn leaf_for(hash: H256) -> TxLeafBinding {
        TxLeafBinding {
            data_root: hash,
            prefix_hash: H256::repeat_byte(1),
        }
    }

    #[test]
    fn a_shared_tx_path_stays_until_the_last_interval_is_gone() -> eyre::Result<()> {
        each_engine("submodule_tx_path_gc", |store| {
            let shared = H256::repeat_byte(3);
            store.update(|tx| {
                index_tx_span(tx, shared, 0, 1)?;
                index_tx_span(tx, shared, 10, 11)?;
                tx.add_tx_path_hash_to_offset_range(
                    PartitionChunkOffset::from(0),
                    PartitionChunkOffset::from(1),
                    None,
                )
            })?;
            assert_eq!(
                store.view(|tx| tx.get_full_tx_path(shared))?,
                Some(vec![shared.0[0]])
            );
            assert_eq!(
                store.view(|tx| tx.get_tx_leaf_binding(shared))?,
                Some(leaf_for(shared))
            );
            let far = store
                .view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(10)))?
                .expect("far interval");
            assert_eq!(far.tx_path_hash, Some(shared));
            assert!(
                store
                    .view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(0)))?
                    .is_none()
            );

            store.update(|tx| {
                tx.add_tx_path_hash_to_offset_range(
                    PartitionChunkOffset::from(10),
                    PartitionChunkOffset::from(11),
                    None,
                )
            })?;
            assert!(store.view(|tx| tx.get_full_tx_path(shared))?.is_none());
            assert!(store.view(|tx| tx.get_tx_leaf_binding(shared))?.is_none());
            Ok(())
        })
    }

    #[test]
    fn replacing_a_span_drops_the_old_tx_path() -> eyre::Result<()> {
        each_engine("submodule_tx_path_replace", |store| {
            let old = H256::repeat_byte(4);
            let new = H256::repeat_byte(8);
            store.update(|tx| index_tx_span(tx, old, 0, 4))?;
            store.update(|tx| {
                tx.add_full_tx_path(new, vec![new.0[0]])?;
                tx.add_tx_leaf_binding(new, &leaf_for(new))?;
                tx.add_tx_path_hash_to_offset_range(
                    PartitionChunkOffset::from(0),
                    PartitionChunkOffset::from(4),
                    Some(new),
                )
            })?;
            assert!(store.view(|tx| tx.get_full_tx_path(old))?.is_none());
            assert!(store.view(|tx| tx.get_tx_leaf_binding(old))?.is_none());
            assert_eq!(
                store.view(|tx| tx.get_full_tx_path(new))?,
                Some(vec![new.0[0]])
            );
            assert_eq!(
                store.view(|tx| tx.get_tx_leaf_binding(new))?,
                Some(leaf_for(new))
            );
            let at_zero = store
                .view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(0)))?
                .expect("replacement interval");
            assert_eq!(at_zero.tx_path_hash, Some(new));
            Ok(())
        })
    }

    #[test]
    fn empty_data_root_infos_delete_the_row() -> eyre::Result<()> {
        each_engine("submodule_empty_data_root", |store| {
            let root = H256::repeat_byte(5);
            let info = DataRootInfo {
                start_offset: RelativeChunkOffset(1),
                data_size: 32,
            };
            store.update(|tx| tx.add_data_root_info(root, &info))?;
            assert_eq!(
                store.view(|tx| tx.get_data_root_infos_for_data_root(root))?,
                Some(DataRootInfos(vec![info.clone()]))
            );
            store.update(|tx| tx.set_data_root_infos_for_data_root(root, DataRootInfos(vec![])))?;
            assert!(
                store
                    .view(|tx| tx.get_data_root_infos_for_data_root(root))?
                    .is_none()
            );
            store.update(|tx| tx.add_data_root_info(root, &info))?;
            assert_eq!(
                store.view(|tx| tx.get_data_root_infos_for_data_root(root))?,
                Some(DataRootInfos(vec![info]))
            );
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

    fn pending_job(data_root: H256) -> PendingBodyMigration {
        PendingBodyMigration {
            data_root,
            data_size: 32,
            start_offset: RelativeChunkOffset(0),
            block_height: 1,
            attempts: 0,
        }
    }

    /// A page is a prefix of the ascending walk. The next page starts at the
    /// key after the last one this page returned. `None` still returns every row.
    #[test]
    fn pending_body_migration_page_stops_at_the_limit() -> eyre::Result<()> {
        each_engine("submodule_pending_page", |store| {
            let root = H256::repeat_byte(6);
            store.update(|tx| {
                for key in [0_u32, 4, 9] {
                    tx.add_pending_body_migration(
                        PartitionChunkOffset::from(key),
                        &pending_job(root),
                    )?;
                }
                Ok(())
            })?;
            let all = store.view(|tx| tx.pending_body_migrations_from(None, None))?;
            assert_eq!(all.len(), 3);
            let page = store.view(|tx| tx.pending_body_migrations_from(None, Some(2)))?;
            assert_eq!(
                page.iter().map(|(key, _)| key.0).collect::<Vec<_>>(),
                vec![0, 4]
            );
            let next = page.last().expect("page").0;
            let rest = store.view(|tx| {
                tx.pending_body_migrations_from(Some(PartitionChunkOffset(next.0 + 1)), Some(2))
            })?;
            assert_eq!(
                rest.iter().map(|(key, _)| key.0).collect::<Vec<_>>(),
                vec![9]
            );
            let none = store.view(|tx| tx.pending_body_migrations_from(None, Some(0)))?;
            assert!(none.is_empty());
            Ok(())
        })
    }

    /// Inclusive range delete drops both ends and keeps the neighbors.
    /// `u32::MAX` is inside `[MAX, MAX]` and outside `[1, 5]`.
    #[test]
    fn offset_range_delete_keeps_neighbors_and_the_max_end() -> eyre::Result<()> {
        each_engine("submodule_range_bounds", |store| {
            let keys = [0_u32, 1, 5, u32::MAX - 1, u32::MAX];
            let data_root = H256::repeat_byte(4);
            store.update(|tx| {
                for key in keys {
                    let offset = PartitionChunkOffset::from(key);
                    tx.write_data_path_updates(vec![(
                        offset,
                        H256::repeat_byte((key % 200) as u8),
                        vec![key as u8],
                    )])?;
                    tx.add_pending_body_migration(offset, &pending_job(data_root))?;
                }
                Ok(())
            })?;

            store.update(|tx| {
                tx.clear_paths_in_inclusive_range(
                    PartitionChunkOffset::from(1),
                    PartitionChunkOffset::from(5),
                )?;
                let removed = tx.del_pending_body_migrations_in_range(
                    PartitionChunkOffset::from(1),
                    PartitionChunkOffset::from(5),
                )?;
                assert_eq!(removed, 2);
                assert!(
                    tx.get_data_path_by_offset(PartitionChunkOffset::from(1))?
                        .is_none()
                );
                assert!(
                    tx.get_pending_body_migration(PartitionChunkOffset::from(5))?
                        .is_none()
                );
                assert!(
                    tx.get_data_path_by_offset(PartitionChunkOffset::from(0))?
                        .is_some()
                );
                Ok(())
            })?;
            for key in [0_u32, u32::MAX - 1, u32::MAX] {
                assert!(
                    store
                        .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(key)))?
                        .is_some(),
                    "offset {key} stays"
                );
                assert!(
                    store
                        .view(|tx| tx.get_pending_body_migration(PartitionChunkOffset::from(key)))?
                        .is_some(),
                    "pending {key} stays"
                );
            }
            for key in [1_u32, 5] {
                assert!(
                    store
                        .view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(key)))?
                        .is_none(),
                    "offset {key} is gone"
                );
                assert!(
                    store
                        .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(key)))?
                        .is_none()
                );
                assert!(
                    store
                        .view(|tx| tx.get_pending_body_migration(PartitionChunkOffset::from(key)))?
                        .is_none()
                );
            }

            let max = PartitionChunkOffset::from(u32::MAX);
            let before_max = PartitionChunkOffset::from(u32::MAX - 1);
            store.update(|tx| {
                let removed = tx.del_pending_body_migrations_in_range(max, max)?;
                assert_eq!(removed, 1);
                tx.clear_paths_in_inclusive_range(max, max)?;
                assert!(tx.get_data_path_by_offset(max)?.is_none());
                assert!(tx.get_data_path_by_offset(before_max)?.is_some());
                Ok(())
            })?;
            assert!(
                store
                    .view(|tx| tx.get_pending_body_migration(before_max))?
                    .is_some()
            );
            assert!(store.view(|tx| tx.get_data_path_by_offset(max))?.is_none());
            assert!(
                store
                    .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(0)))?
                    .is_some()
            );

            store.update(|tx| {
                let removed = tx.del_pending_body_migrations_in_range(before_max, max)?;
                assert_eq!(removed, 1);
                tx.clear_paths_in_inclusive_range(before_max, max)?;
                let outside = tx.del_pending_body_migrations_in_range(
                    PartitionChunkOffset::from(1),
                    PartitionChunkOffset::from(5),
                )?;
                assert_eq!(outside, 0);
                Ok(())
            })?;
            assert!(
                store
                    .view(|tx| tx.get_pending_body_migration(before_max))?
                    .is_none()
            );
            assert!(
                store
                    .view(|tx| tx.get_data_path_by_offset(before_max))?
                    .is_none()
            );
            assert!(
                store
                    .view(|tx| tx.get_pending_body_migration(PartitionChunkOffset::from(0)))?
                    .is_some()
            );

            // A put after the range delete in the same batch stays.
            store.update(|tx| {
                tx.clear_paths_in_inclusive_range(
                    PartitionChunkOffset::from(0),
                    PartitionChunkOffset::from(0),
                )?;
                tx.write_data_path_updates(vec![(
                    PartitionChunkOffset::from(0),
                    H256::repeat_byte(8),
                    vec![8],
                )])?;
                assert_eq!(
                    tx.get_data_path_by_offset(PartitionChunkOffset::from(0))?,
                    Some(vec![8])
                );
                Ok(())
            })?;
            assert_eq!(
                store.view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(0)))?,
                Some(vec![8])
            );
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

    /// `clear` removes index families and leaves the reset marker.
    #[test]
    fn reset_marker_survives_clear() -> eyre::Result<()> {
        each_engine("submodule_reset_marker", |store| {
            store.update(|tx| {
                tx.set_path_hashes_by_offset(
                    PartitionChunkOffset::from(1),
                    ChunkPathHashes {
                        data_path_hash: Some(H256::repeat_byte(3)),
                        tx_path_hash: None,
                    },
                )
            })?;
            assert!(!store.reset_in_progress()?);
            store.set_reset_in_progress()?;
            store.update(|tx| tx.clear())?;
            assert!(store.reset_in_progress()?);
            assert!(
                store
                    .view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(1)))?
                    .is_none()
            );
            store.clear_reset_in_progress()?;
            assert!(!store.reset_in_progress()?);
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
        let dir = TempDirBuilder::new()
            .prefix("submodule_store_group_n")
            .build();
        let store =
            SubmoduleIndex::open_mdbx(dir.path().join("db"), DatabaseArguments::irys_testing()?)?;
        store.enable_group_commit(2)?;
        put_registration(&store, 1, 1)?;
        assert_eq!(store.group_sync_count(), 0);
        put_registration(&store, 2, 2)?;
        assert_eq!(store.group_sync_count(), 1);
        put_registration(&store, 3, 3)?;
        assert_eq!(store.group_sync_count(), 1);
        assert!(path_at(&store, 3)?.is_some());
        Ok(())
    }

    #[cfg(feature = "rocksdb")]
    #[test]
    fn rocks_full_group_does_not_sync_until_a_durable_write() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_group_n")
            .build();
        let store = SubmoduleIndex::open_rocks(dir.path().join("index"))?;
        store.set_wal_backstop(Duration::from_secs(60));
        store.enable_group_commit(2)?;
        put_registration(&store, 1, 1)?;
        put_registration(&store, 2, 2)?;
        assert_eq!(store.group_sync_count(), 0);
        assert_eq!(store.wal_sync_count(), 0);
        assert_eq!(store.wal_backstop_sync_count(), 0);
        let published = store.published_generation();
        assert!(published > store.durable_generation());
        store.update_covered(|tx| {
            tx.set_path_hashes_by_offset(
                PartitionChunkOffset::from(3),
                ChunkPathHashes {
                    data_path_hash: Some(H256::repeat_byte(3)),
                    tx_path_hash: None,
                },
            )
        })?;
        assert_eq!(store.wal_sync_count(), 0);
        store.update(|tx| {
            tx.set_path_hashes_by_offset(
                PartitionChunkOffset::from(4),
                ChunkPathHashes {
                    data_path_hash: Some(H256::repeat_byte(4)),
                    tx_path_hash: None,
                },
            )
        })?;
        assert_eq!(store.wal_sync_count(), 1);
        assert_eq!(store.wal_backstop_sync_count(), 0);
        assert_eq!(store.group_sync_count(), 0);
        assert!(store.durable_generation() >= published);
        store.wait_until_durable(published)?;
        Ok(())
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
    fn drop_presence_removes_only_presence_rows() -> eyre::Result<()> {
        each_engine("submodule_presence_reconcile", |store| {
            let tx_hash = H256::repeat_byte(9);
            let data_root = H256::repeat_byte(4);
            let tx_path = vec![9_u8];
            let leaf = TxLeafBinding {
                data_root,
                prefix_hash: H256::repeat_byte(1),
            };
            let info = DataRootInfo {
                start_offset: RelativeChunkOffset(1),
                data_size: 64,
            };
            let pending = PendingBodyMigration {
                data_root,
                data_size: 64,
                start_offset: RelativeChunkOffset(1),
                block_height: 7,
                attempts: 1,
            };
            let max = PartitionChunkOffset::from(u32::MAX);
            store.update(|tx| {
                tx.write_data_path_updates(vec![
                    (PartitionChunkOffset::from(1), H256::repeat_byte(1), vec![1]),
                    (PartitionChunkOffset::from(3), H256::repeat_byte(3), vec![3]),
                    (max, H256::repeat_byte(7), vec![7]),
                ])?;
                tx.add_full_tx_path(tx_hash, tx_path.clone())?;
                tx.add_tx_path_hash_to_offset_range(
                    PartitionChunkOffset::from(1),
                    PartitionChunkOffset::from(3),
                    Some(tx_hash),
                )?;
                tx.add_tx_leaf_binding(tx_hash, &leaf)?;
                tx.add_data_root_info(data_root, &info)?;
                tx.add_pending_body_migration(PartitionChunkOffset::from(1), &pending)?;
                Ok(())
            })?;

            let before = store.committed_sequence()?;
            let stats = store.drop_presence_in_ranges(&[
                (PartitionChunkOffset::from(3), PartitionChunkOffset::from(3)),
                (max, max),
            ])?;
            assert_eq!(stats.path_hash_rows, 2);
            assert_eq!(stats.data_path_rows, 2);
            assert!(store.committed_sequence()? > before);

            let data_at =
                |offset: PartitionChunkOffset| store.view(|tx| tx.get_data_path_by_offset(offset));
            assert_eq!(data_at(PartitionChunkOffset::from(1))?, Some(vec![1]));
            assert_eq!(data_at(PartitionChunkOffset::from(3))?, None);
            assert_eq!(data_at(max)?, None);

            let rows = store
                .view(|tx| tx.path_hashes_in_inclusive_range(PartitionChunkOffset::from(0), max))?;
            assert_eq!(
                rows.into_iter()
                    .map(|(offset, _)| offset.0)
                    .collect::<Vec<_>>(),
                vec![1]
            );
            assert_eq!(
                store.view(|tx| tx.get_tx_path_by_offset(PartitionChunkOffset::from(3)))?,
                Some(tx_path.clone())
            );
            assert_eq!(
                store.view(|tx| tx.get_full_tx_path(tx_hash))?,
                Some(tx_path)
            );
            assert_eq!(
                store.view(|tx| tx.get_tx_leaf_binding(tx_hash))?,
                Some(leaf)
            );
            assert_eq!(
                store.view(|tx| tx.get_data_root_infos_for_data_root(data_root))?,
                Some(DataRootInfos(vec![info]))
            );
            assert_eq!(
                store.view(|tx| tx.get_pending_body_migration(PartitionChunkOffset::from(1)))?,
                Some(pending)
            );

            let seq = store.committed_sequence()?;
            let again = store.drop_presence_in_ranges(&[
                (PartitionChunkOffset::from(3), PartitionChunkOffset::from(3)),
                (max, max),
            ])?;
            assert_eq!(again.rows_removed(), 0);
            let gap = store.drop_presence_in_ranges(&[(
                PartitionChunkOffset::from(10),
                PartitionChunkOffset::from(20),
            )])?;
            assert_eq!(gap.rows_removed(), 0);
            let empty = store.drop_presence_in_ranges(&[])?;
            assert_eq!(empty.rows_removed(), 0);
            let backwards = store.drop_presence_in_ranges(&[(
                PartitionChunkOffset::from(5),
                PartitionChunkOffset::from(4),
            )])?;
            assert_eq!(backwards.rows_removed(), 0);
            assert_eq!(store.committed_sequence()?, seq);
            Ok(())
        })
    }

    #[test]
    fn drop_presence_on_empty_index_does_not_commit() -> eyre::Result<()> {
        each_engine("submodule_presence_empty", |store| {
            let seq = store.committed_sequence()?;
            let stats = store.drop_presence_in_ranges(&[(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(u32::MAX),
            )])?;
            assert_eq!(stats.rows_removed(), 0);
            assert_eq!(store.committed_sequence()?, seq);
            Ok(())
        })
    }

    #[test]
    fn quiet_registration_syncs_after_the_delay() -> eyre::Result<()> {
        each_engine("submodule_store_group_delay", |store| {
            // Rocks uses this bound. MDBX keeps its 50 ms delay.
            store.set_wal_backstop(Duration::from_millis(50));
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

    /// A clean `DB` drop is not a power loss: close may sync the WAL, and
    /// shutdown does sync it. This checks the covering write, then reopen.
    #[cfg(feature = "rocksdb")]
    #[test]
    fn rocks_synced_write_makes_an_unsynced_registration_durable() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_cover_reopen")
            .build();
        let path = dir.path().join("index");
        {
            let store = SubmoduleIndex::open_rocks(&path)?;
            store.set_wal_backstop(Duration::from_secs(60));
            store.enable_group_commit(8)?;
            put_registration(&store, 1, 1)?;
            assert_eq!(store.wal_sync_count(), 0);
            assert_eq!(store.wal_backstop_sync_count(), 0);
            let published = store.published_generation();
            assert!(published > store.durable_generation());
            store.update(|tx| {
                tx.set_path_hashes_by_offset(
                    PartitionChunkOffset::from(2),
                    ChunkPathHashes {
                        data_path_hash: Some(H256::repeat_byte(2)),
                        tx_path_hash: None,
                    },
                )
            })?;
            assert!(store.durable_generation() >= published);
            assert_eq!(store.wal_backstop_sync_count(), 0);
            assert_eq!(store.wal_sync_count(), 1);
            store.wait_until_durable(published)?;
        }
        let store = SubmoduleIndex::open_rocks(&path)?;
        assert!(path_at(&store, 1)?.is_some());
        assert!(path_at(&store, 2)?.is_some());
        Ok(())
    }

    #[cfg(feature = "rocksdb")]
    #[test]
    fn rocks_shutdown_syncs_a_quiet_registration() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_shutdown_sync")
            .build();
        let path = dir.path().join("index");
        {
            let store = SubmoduleIndex::open_rocks(&path)?;
            store.set_wal_backstop(Duration::from_secs(60));
            store.enable_group_commit(8)?;
            put_registration(&store, 6, 6)?;
            assert_eq!(store.wal_sync_count(), 0);
            assert!(store.published_generation() > store.durable_generation());
        }
        let store = SubmoduleIndex::open_rocks(&path)?;
        assert!(path_at(&store, 6)?.is_some());
        Ok(())
    }

    #[cfg(feature = "rocksdb")]
    #[test]
    fn rocks_backstop_syncs_a_quiet_registration() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_backstop")
            .build();
        let store = SubmoduleIndex::open_rocks(dir.path().join("index"))?;
        let delay = Duration::from_millis(80);
        store.set_wal_backstop(delay);
        store.enable_group_commit(1_000)?;
        assert_eq!(store.wal_backstop_delay(), delay);
        let started = Instant::now();
        put_registration(&store, 4, 4)?;
        let published = store.published_generation();
        assert_eq!(store.wal_sync_count(), 0);
        assert_eq!(store.wal_backstop_sync_count(), 0);
        store.wait_until_durable(published)?;
        let elapsed = started.elapsed();
        assert!(
            elapsed >= delay,
            "backstop sync ran after {elapsed:?}, bound is {delay:?}"
        );
        assert_eq!(store.wal_backstop_sync_count(), 1);
        assert_eq!(store.wal_sync_count(), 1);
        assert_eq!(store.group_sync_count(), 1);
        assert!(store.durable_generation() >= published);
        assert!(path_at(&store, 4)?.is_some());
        Ok(())
    }

    /// RocksDB `SyncWAL` (`FlushWAL(true)`) calls `WALIOStatusCheck` on an IO
    /// error. With paranoid checks left on, that is a background error and a
    /// file-scope WAL error stops later writes. RocksDB may clear it. This
    /// flag does not, so `update` and `update_covered` keep failing. The
    /// backstop thread and an explicit sync share that path. Shutdown's last
    /// sync does too.
    #[cfg(feature = "rocksdb")]
    #[test]
    fn rocks_wal_sync_failure_rejects_later_writes() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_wal_sync_fail")
            .build();
        let store = SubmoduleIndex::open_rocks(dir.path().join("index"))?;
        let label = dir
            .path()
            .file_name()
            .expect("temp dir name")
            .to_string_lossy();
        store.set_wal_backstop(Duration::from_secs(60));
        store.enable_group_commit(8)?;
        put_registration(&store, 1, 1)?;
        assert!(!store.wal_sync_failed());
        store.fail_next_wal_sync();
        let err = store.sync_group().unwrap_err();
        assert!(
            err.to_string().contains(label.as_ref()),
            "sync error names the submodule: {err}"
        );
        assert!(store.wal_sync_failed());
        assert!(path_at(&store, 1)?.is_some(), "the row stays visible");
        let update_err = store
            .update(|tx| {
                tx.set_path_hashes_by_offset(
                    PartitionChunkOffset::from(2),
                    ChunkPathHashes {
                        data_path_hash: Some(H256::repeat_byte(2)),
                        tx_path_hash: None,
                    },
                )
            })
            .unwrap_err();
        assert!(update_err.to_string().contains("read-only"), "{update_err}");
        let covered_err = store
            .update_covered(|tx| {
                tx.set_path_hashes_by_offset(
                    PartitionChunkOffset::from(3),
                    ChunkPathHashes {
                        data_path_hash: Some(H256::repeat_byte(3)),
                        tx_path_hash: None,
                    },
                )
            })
            .unwrap_err();
        assert!(
            covered_err.to_string().contains(label.as_ref()),
            "{covered_err}"
        );
        assert!(path_at(&store, 2)?.is_none());
        assert!(path_at(&store, 3)?.is_none());
        Ok(())
    }

    /// The backstop thread uses the same sync as shutdown's last flush.
    #[cfg(feature = "rocksdb")]
    #[test]
    fn rocks_backstop_sync_failure_rejects_later_writes() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_backstop_fail")
            .build();
        let store = SubmoduleIndex::open_rocks(dir.path().join("index"))?;
        store.set_wal_backstop(Duration::from_millis(30));
        store.enable_group_commit(1_000)?;
        store.fail_next_wal_sync();
        put_registration(&store, 4, 4)?;
        let started = Instant::now();
        while !store.wal_sync_failed() {
            if started.elapsed() > Duration::from_secs(2) {
                eyre::bail!("backstop sync failure did not stick");
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            store
                .update_registration(|tx| {
                    tx.set_path_hashes_by_offset(
                        PartitionChunkOffset::from(5),
                        ChunkPathHashes {
                            data_path_hash: Some(H256::repeat_byte(5)),
                            tx_path_hash: None,
                        },
                    )
                })
                .is_err()
        );
        assert!(path_at(&store, 4)?.is_some());
        assert!(path_at(&store, 5)?.is_none());
        Ok(())
    }

    #[cfg(feature = "rocksdb")]
    #[test]
    fn rocks_backstop_defaults_to_one_second() -> eyre::Result<()> {
        let dir = TempDirBuilder::new()
            .prefix("submodule_rocks_backstop_default")
            .build();
        let store = SubmoduleIndex::open_rocks(dir.path().join("index"))?;
        store.enable_group_commit(8)?;
        assert_eq!(
            store.wal_backstop_delay(),
            Duration::from_millis(irys_types::DEFAULT_INDEX_WAL_BACKSTOP_MS)
        );
        Ok(())
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
