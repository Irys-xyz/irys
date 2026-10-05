//! # Storage Abstraction Layers
//!
//! ```text
//! +------------------+
//! |       Node       |
//! |  +------------+  |  +--------------------------+
//! |  | Partition1 |<----| Storage Module A         |<--+ Submodule i
//! |  +------------+  |  +--------------------------+
//! |                  |
//! |  +------------+  |  +--------------------------+
//! |  | Partition2 |<----| Storage Module B         |<--+ Submodule i
//! |  +------------+  |  |                          |<--+ Submodule ii
//! |                  |  +--------------------------+
//! |                  |
//! |  +------------+  |  +--------------------------+
//! |  | unpledged  |<----| Storage Module C         |<--+ Submodule i
//! |  +------------+  |  |                          |<--+ Submodule ii
//! |                  |  |                          |<--+ Submodule iii
//! |                  |  +--------------------------+
//! +------------------+
//! ```
//!
//! ## Node Level of Abstraction
//! - Node operates only on partitions, identified by partition_hash
//! - Each partition contains CONFIG.num_chunks_in_partition chunks
//! - Partition hashes map to Ledger slots, or the capacity partitions list in the epoch_service
//!
//! ## Storage Module Level of Abstraction
//! - Storage modules manage reading/writing of chunks for an entire partition
//! - Storage modules can span multiple physical drives via submodules
//! - Typical deployment: Single 16TB HDD submodule per partition (and storage module)
//! - Alternative setup: Multiple smaller drives (e.g., 4x 4TB) as submodules to the storage module
//!
//! ## Submodule Level of Abstraction
//! - Submodules are owned and managed exclusively by Storage Modules
//! - Invisible to rest of the node
//! - Storage Module handles mapping of partition chunk offsets to appropriate submodule

use std::os::unix::fs::FileExt as _;
use std::os::unix::io::AsRawFd as _;

use atomic_write_file::AtomicWriteFile;
use derive_more::derive::{Deref, DerefMut};
use eyre::{Context as _, OptionExt as _, Result, ensure, eyre};
use irys_database::{
    db::IrysDatabaseExt as _,
    submodule::{
        add_data_root_info, add_full_tx_path, add_pending_body_migration, add_tx_leaf_binding,
        add_tx_path_hash_to_offset_range, clear_submodule_database, create_or_open_submodule_db,
        del_path_hashes_by_offset, del_pending_body_migration,
        del_pending_body_migrations_in_range, get_data_root_infos_for_data_root,
        get_full_data_path, get_full_tx_path, get_path_hashes_by_offset,
        get_pending_body_migration, get_tx_leaf_binding, missing_path_hash_ranges_in_tx,
        path_hashes_in_inclusive_range, pending_body_migrations_from,
        set_data_root_infos_for_data_root,
        tables::{DataRootInfo, DataRootInfos, PendingBodyMigration, TxLeafBinding},
    },
};
use irys_packing::capacity_single::compute_entropy_chunk;
use irys_packing::unpack;
use irys_types::{
    Base64, ChunkBytes, ChunkDataPath, ChunkPathHash, Config, DataLedger, DataRoot,
    DataTransactionHeader, DataTransactionLedger, H256, IrysAddress, LedgerChunkOffset,
    LedgerChunkRange, PackedChunk, PartitionChunkOffset, PartitionChunkRange,
    ProofDeserialize as _, RelativeChunkOffset, TxChunkOffset, TxPath, UnpackedChunk,
    app_state::DatabaseProvider,
    get_leaf_proof, ledger_chunk_offset_ie,
    partition::{PartitionAssignment, PartitionHash},
    partition_chunk_offset_ii,
};
use nodit::{InclusiveInterval as _, Interval, NoditMap, NoditSet, interval::ii};
use openssl::sha;
use reth_db::Database as _;
use reth_db::transaction::DbTx;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{self, File, OpenOptions},
    io::{Read as _, Seek as _, SeekFrom, Write as _},
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock, TryLockError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tracing::{debug, error, info, warn};

use super::index_read_metrics::{self, METADATA, MIGRATION, RECALL};
use crate::{CircularBuffer, StorageModulesReadGuard};

#[path = "index_drain.rs"]
mod index_drain;

#[path = "disk_lane.rs"]
mod disk_lane;

type SubmodulePath = PathBuf;

/// Recover the verified real `data_root` and this offset's `data_path` hash in a single
/// offset-index read.
///
/// After the `prefix_hash` softfork, the tx_path leaf stores the folded
/// `hash_all_sha256([data_root, prefix_hash])` (the ledger `tx_root` leaf value), so the
/// raw `data_root` can no longer be read out of the proof leaf. The real `data_root` comes
/// from the stored `TxLeafBinding` and is cross-checked against the proof leaf via the same
/// fold — so the binding and the proof can never silently disagree; a mismatch is surfaced
/// as corruption rather than served.
///
/// Returns `Ok(None)` if no tx_path is indexed at this offset. The returned
/// `data_path_hash` (the chunk's own path hash, `None` if its chunk hasn't been written)
/// lets the caller fetch the `data_path` directly without re-reading the offset index.
fn recover_tx_path_data_root<T: DbTx>(
    tx: &T,
    partition_offset: PartitionChunkOffset,
) -> eyre::Result<Option<(DataRoot, Option<ChunkPathHash>)>> {
    // Single read of the offset index; both the tx_path_hash (for data_root recovery) and
    // the data_path_hash (returned to the caller) come from this one lookup.
    let Some(path_hashes) = get_path_hashes_by_offset(tx, partition_offset)? else {
        return Ok(None);
    };
    let Some(tx_path_hash) = path_hashes.tx_path_hash else {
        return Ok(None);
    };
    let Some(data_root) = data_root_for_tx_path_hash(tx, tx_path_hash)? else {
        return Ok(None);
    };
    Ok(Some((data_root, path_hashes.data_path_hash)))
}

/// Real `data_root` for a stored tx path, checked against the proof leaf.
///
/// `Ok(None)` when the tx-path bytes are absent. A missing binding, or a leaf
/// that does not match the stored fold, is corruption.
fn data_root_for_tx_path_hash<T: DbTx>(
    tx: &T,
    tx_path_hash: irys_types::TxPathHash,
) -> eyre::Result<Option<DataRoot>> {
    index_read_metrics::note("tx_path");
    let Some(tx_path) = get_full_tx_path(tx, tx_path_hash)? else {
        return Ok(None);
    };
    index_read_metrics::note("tx_leaf");

    let leaf = get_leaf_proof(&Base64::from(tx_path))?
        .hash()
        .map(H256::from)
        .ok_or_eyre("Unable to parse tx_path leaf hash")?;

    let binding = get_tx_leaf_binding(tx, tx_path_hash)?
        .ok_or_eyre("missing tx_path -> (data_root, prefix_hash) binding for stored tx_path")?;
    // Re-verify via the single canonical fold (same formula block production/validation use).
    let expected_leaf =
        DataTransactionLedger::fold_tx_root_leaf(binding.data_root, binding.prefix_hash);
    eyre::ensure!(
        leaf == expected_leaf,
        "tx_path leaf {leaf:?} != fold(stored data_root {:?}, prefix_hash {:?}) = {expected_leaf:?} \
         — submodule corruption (binding out of sync with stored proof)",
        binding.data_root,
        binding.prefix_hash,
    );

    Ok(Some(binding.data_root))
}

/// Index rows for `[start, end]` resolved inside the caller's read transaction.
///
/// A tx path, a data-root placement list, and a data path that several offsets
/// share are loaded once. An offset with no tx path, no data path, or no
/// stored path bytes is omitted.
fn metas_in_tx<T: DbTx>(
    tx: &T,
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
    chunk_size: u64,
) -> eyre::Result<BTreeMap<PartitionChunkOffset, (DataRoot, u64, Base64, TxChunkOffset)>> {
    index_read_metrics::note("offset_walk");
    let rows = path_hashes_in_inclusive_range(tx, start, end)?;
    let mut tx_roots: HashMap<irys_types::TxPathHash, Option<DataRoot>> = HashMap::new();
    let mut infos: HashMap<DataRoot, DataRootInfos> = HashMap::new();
    let mut data_paths: HashMap<ChunkPathHash, Option<Base64>> = HashMap::new();
    let mut out = BTreeMap::new();
    for (offset, hashes) in rows {
        let Some(tx_path_hash) = hashes.tx_path_hash else {
            continue;
        };
        let data_root = if let Some(cached) = tx_roots.get(&tx_path_hash) {
            *cached
        } else {
            let loaded = data_root_for_tx_path_hash(tx, tx_path_hash)?;
            tx_roots.insert(tx_path_hash, loaded);
            loaded
        };
        let Some(data_root) = data_root else {
            continue;
        };
        let data_size = data_size_for_offset(tx, data_root, offset, &mut infos)?;
        let Some(data_path_hash) = hashes.data_path_hash else {
            continue;
        };
        let path_buff = if let Some(cached) = data_paths.get(&data_path_hash) {
            cached.clone()
        } else {
            index_read_metrics::note("data_path");
            let loaded = get_full_data_path(tx, data_path_hash)?.map(Base64::from);
            data_paths.insert(data_path_hash, loaded.clone());
            loaded
        };
        let Some(path_buff) = path_buff else {
            continue;
        };
        let proof = get_leaf_proof(&path_buff)?;
        let chunk_offset = (proof.offset() as u64).div_ceil(chunk_size) - 1;
        out.insert(
            offset,
            (
                data_root,
                data_size,
                path_buff,
                TxChunkOffset(chunk_offset.try_into().expect("Value exceeds u32::MAX")),
            ),
        );
    }
    Ok(out)
}

fn data_size_for_offset<T: DbTx>(
    tx: &T,
    data_root: DataRoot,
    partition_offset: PartitionChunkOffset,
    cache: &mut HashMap<DataRoot, DataRootInfos>,
) -> eyre::Result<u64> {
    if !cache.contains_key(&data_root) {
        index_read_metrics::note("data_root");
        let mut loaded = get_data_root_infos_for_data_root(tx, data_root)
            .expect("Database read should succeed")
            .expect(
                "there should be at least one start_offset for any data_root stored in the submodule",
            );
        loaded.0.sort_unstable();
        cache.insert(data_root, loaded);
    }
    let infos = cache.get(&data_root).expect("just inserted");
    let index = infos
        .0
        .partition_point(|info| info.start_offset <= partition_offset.into())
        .saturating_sub(1);
    if index < infos.0.len() {
        Ok(infos.0[index].data_size)
    } else {
        Err(eyre!("could not find DataRootInfo for partition_offset"))
    }
}

fn paths_in_tx<T: DbTx>(
    tx: &T,
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
) -> eyre::Result<BTreeMap<PartitionChunkOffset, (Option<TxPath>, Option<ChunkDataPath>)>> {
    index_read_metrics::note("offset_walk");
    let rows = path_hashes_in_inclusive_range(tx, start, end)?;
    let mut tx_paths: HashMap<irys_types::TxPathHash, Option<TxPath>> = HashMap::new();
    let mut data_paths: HashMap<ChunkPathHash, Option<ChunkDataPath>> = HashMap::new();
    let mut out = BTreeMap::new();
    for (offset, hashes) in rows {
        let tx_path = match hashes.tx_path_hash {
            Some(hash) => cached_bytes(&mut tx_paths, hash, || {
                index_read_metrics::note("tx_path");
                get_full_tx_path(tx, hash)
            })?,
            None => None,
        };
        let data_path = match hashes.data_path_hash {
            Some(hash) => cached_bytes(&mut data_paths, hash, || {
                index_read_metrics::note("data_path");
                get_full_data_path(tx, hash)
            })?,
            None => None,
        };
        out.insert(offset, (tx_path, data_path));
    }
    Ok(out)
}

fn cached_bytes<K, V, E>(
    cache: &mut HashMap<K, Option<V>>,
    key: K,
    load: impl FnOnce() -> Result<Option<V>, E>,
) -> Result<Option<V>, E>
where
    K: Eq + std::hash::Hash + Copy,
    V: Clone,
{
    if let Some(hit) = cache.get(&key) {
        return Ok(hit.clone());
    }
    let loaded = load()?;
    cache.insert(key, loaded.clone());
    Ok(loaded)
}

// In-memory chunk data indexed by offset within partition
type ChunkMap = BTreeMap<PartitionChunkOffset, (ChunkBytes, ChunkType)>;

#[derive(Debug, Default)]
struct PendingWrites {
    chunks: ChunkMap,
    /// Offset → generation that reserved it. Only that generation may release.
    occupancy: HashMap<PartitionChunkOffset, u64>,
    /// Who queued each packed chunk. Equal-length runs break ties with this.
    priorities: HashMap<PartitionChunkOffset, disk_lane::WritePriority>,
    /// When each packed chunk was queued. While the disk is busy, the oldest
    /// short run may jump a longer one after `reorder_grace`. Further short
    /// runs wait another `reorder_grace`. An idle disk writes them.
    queued_at: HashMap<PartitionChunkOffset, Instant>,
    /// Unpacked bytes waiting on the entropy read. Counted in `pending_write_bytes`.
    queued_unpacked_bytes: u64,
}

impl Deref for PendingWrites {
    type Target = ChunkMap;
    fn deref(&self) -> &ChunkMap {
        &self.chunks
    }
}

impl DerefMut for PendingWrites {
    fn deref_mut(&mut self) -> &mut ChunkMap {
        &mut self.chunks
    }
}

/// Storage submodules mapped to their chunk ranges
type SubmoduleMap =
    NoditMap<PartitionChunkOffset, Interval<PartitionChunkOffset>, StorageSubmodule>;

/// Tracks storage state of chunk ranges across all submodules
type StorageIntervals = NoditMap<PartitionChunkOffset, Interval<PartitionChunkOffset>, ChunkType>;

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SyncFailurePoint {
    BeforeDataFsync,
    BeforeIntervalCommit,
}

#[derive(Debug, Clone)]
pub struct ChunkTimeRecord {
    pub chunk_offset: PartitionChunkOffset,
    pub start_time: Instant,
    pub completion_time: Instant,
    pub duration: Duration,
}

impl Default for ChunkTimeRecord {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            chunk_offset: PartitionChunkOffset::from(0),
            start_time: now,
            completion_time: now,
            duration: Duration::ZERO,
        }
    }
}

/// Maps a logical partition (fixed size) to physical storage across multiple drives
#[derive(Debug)]
pub struct StorageModule {
    /// an integer uniquely identifying the module
    pub id: usize,
    /// The (Optional) info about a partition assigned to this storage module
    pub partition_assignment: RwLock<Option<PartitionAssignment>>,
    /// In-memory chunk buffer awaiting disk write. Occupancy (index ops in
    /// flight) lives under this same lock.
    pending_writes: RwLock<PendingWrites>,
    /// Shared with submodule drains so tests can fail the next index commit.
    #[cfg(any(test, feature = "test-utils"))]
    index_commit_fail_next: Arc<AtomicBool>,
    #[cfg(test)]
    fail_entropy_read_nth: AtomicU64,
    #[cfg(test)]
    entropy_read_seq: AtomicU64,
    /// Submodule `view`s opened by a range index read. One slice of the range
    /// is one view, shared by every offset in that slice.
    #[cfg(test)]
    index_views: AtomicU64,
    /// Serializes flushes so two callers cannot claim and write the same
    /// pending batch concurrently. Pending entries remain present until the
    /// data fsync and interval commit both succeed, and therefore serve as the
    /// durability fence themselves.
    sync_in_progress: Mutex<()>,
    /// Set while network-partition recovery rewrites this module's ranges.
    /// Data writes re-check it under the `pending_writes` lock at insert time,
    /// so once it is set no body can be queued behind recovery's
    /// `drop_pending_writes_in_range`. Entropy (packing) writes are unaffected.
    data_writes_paused: AtomicBool,
    #[cfg(test)]
    sync_failure: Mutex<Option<SyncFailurePoint>>,
    /// Monotonic instant of the last pending write, used only for idle flushing.
    last_pending_write: RwLock<Instant>,
    /// Tracks the storage state of each chunk across all submodules
    intervals: Arc<RwLock<StorageIntervals>>,
    /// Shared with every submodule drain. `pause_data_writes` / `reset` bump it
    /// so queued index ops with a stale generation are cancelled.
    index_write_generation: Arc<AtomicU64>,
    /// Physical storage locations indexed by chunk ranges
    submodules: SubmoduleMap,
    /// Track the speed/throughput of recent disk writes
    recent_chunk_times: Arc<RwLock<CircularBuffer<ChunkTimeRecord>>>,
    /// Runtime configuration parameters
    pub config: Config,
    /// Recall, packed writes, and entropy reads. Callers enqueue; this lane
    /// takes `chunks.dat`.
    disk: disk_lane::DiskGate,
}

/// On-disk metadata for StorageModule persistence
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StorageModuleInfo {
    /// An integer uniquely identifying the module
    pub id: usize,
    /// Hash of partition this storage module belongs to, if assigned
    pub partition_assignment: Option<PartitionAssignment>,
    /// Range of chunk offsets and path for each submodule
    pub submodules: Vec<(Interval<PartitionChunkOffset>, SubmodulePath)>,
}

impl StorageModuleInfo {
    /// Loads the [`StorageModuleInfo`] from a JSON file at the given path
    pub fn from_json(path: impl AsRef<Path>) -> eyre::Result<Self> {
        let contents = fs::read_to_string(path)?;
        let config: Self = serde_json::from_str(&contents)?;
        Ok(config)
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq, Copy)]
pub struct PackingParams {
    pub packing_address: IrysAddress,
    pub partition_hash: Option<H256>,
    pub ledger: Option<u32>,
    pub slot: Option<usize>,
    pub last_updated_height: Option<u64>,
}

impl PackingParams {
    /// Loads the [`PackingParams`] from a TOML file at the given path
    pub fn from_toml(path: impl AsRef<Path>) -> eyre::Result<Self> {
        let contents = fs::read_to_string(path)?;
        let config: Self = toml::from_str(&contents)?;
        Ok(config)
    }

    pub fn write_to_disk(&self, path: &Path) {
        let toml = toml::to_string(self).expect("Able to serialize config");
        fs::write(path, toml).unwrap_or_else(|_| panic!("Failed to write config to {:?}", path));
    }
}

pub static PACKING_PARAMS_FILE_NAME: &str = "packing_params.toml";

/// Manages chunk storage on a single physical drive
#[derive(Debug)]
pub struct StorageSubmodule {
    /// Persistent database env
    pub db: DatabaseProvider,
    /// path to this Submodule
    pub path: PathBuf,
    /// Persistent storage handle
    file: Arc<Mutex<File>>,
    /// Mutex containing the interval file path
    /// we create an [`AtomicWriteFile`] for each interval file update, to ensure we are never left with interrupted writes
    intervals_file: Arc<Mutex<PathBuf>>,
    index_drain: index_drain::IndexDrain,
}

pub fn get_atomic_file<P: AsRef<Path> + std::fmt::Debug>(path: P) -> eyre::Result<AtomicWriteFile> {
    AtomicWriteFile::options()
        .read(true)
        .open(&path)
        .wrap_err_with(|| format!("Failed to create or open atomic file for {:?}", path))
}

/// Defines how chunk data is processed and stored
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
pub enum ChunkType {
    /// Chunk containing matrix-packed entropy only
    Entropy,
    /// Chunk containing packed blockchain transaction data
    Data,
    /// Chunk has not been initialized
    Uninitialized,
    /// Chunk write was interrupted, should be reinitialized
    Interrupted,
}

/// Errors from [`StorageModule::write_data_chunk`].
#[derive(Debug, thiserror::Error)]
pub enum WriteDataChunkError {
    /// No `DataRootInfos` entry for this chunk's data_root (needs index rebuild).
    #[error("Chunks data_root not found in storage module")]
    DataRootNotFound,
    /// Network-partition recovery is rewriting this module's ranges; the write
    /// was refused and should be retried later.
    #[error("storage module data writes are paused for recovery")]
    WritesPaused,
    /// Any other write failure (IO, index update, packing, etc.).
    #[error(transparent)]
    Other(#[from] eyre::Report),
}

/// One body from a multi-chunk enqueue.
///
/// Every queued body in the slice is published to the sweep queue before the
/// lane is woken, so a running lane cannot take a prefix of the run.
#[derive(Debug, PartialEq, Eq)]
pub enum BatchEnqueueItem {
    /// Entropy targets that accepted this body.
    Queued(Vec<PartitionChunkOffset>),
    /// No entropy target accepted this body.
    NotQueued,
    /// No index entry for this data root.
    DataRootNotFound,
}

// we can't put this in `types` due to dependency cycles
#[derive(Debug, Clone, Deref, DerefMut)]
pub struct StorageModules(pub StorageModuleVec);

pub type StorageModuleVec = Vec<Arc<StorageModule>>;

impl StorageModules {
    pub fn inner(self) -> StorageModuleVec {
        self.0
    }

    // returns the first SM (if any) with the provided partition hash
    pub fn get_by_partition_hash(
        &self,
        partition_hash: PartitionHash,
    ) -> Option<Arc<StorageModule>> {
        self.0
            .iter()
            .find(|sm| sm.partition_hash().is_some_and(|ph| ph == partition_hash))
            .cloned()
    }
}

/// Progress of one packed run while choosing which chunks fill the window.
struct RunCursor {
    start: PartitionChunkOffset,
    last: PartitionChunkOffset,
    chunk_type: ChunkType,
    chunk_count: usize,
    byte_len: u64,
}

/// One adjacent run gathered from the reorder buffer.
struct CoalescedRun {
    priority: disk_lane::WritePriority,
    byte_len: u64,
    oldest: Instant,
    aged: bool,
    items: Vec<(PartitionChunkOffset, (ChunkBytes, ChunkType))>,
}

/// Length and age of one packed run. The peek path uses this so it does not
/// clone chunk bytes.
struct RunMeta {
    start: PartitionChunkOffset,
    byte_len: u64,
    oldest: Instant,
}

/// One `pwrite` of strictly adjacent chunks: same type, same `chunks.dat`,
/// no hole, and no mix of priorities. A short chunk is its own run.
struct WriteRun {
    start: PartitionChunkOffset,
    chunk_type: ChunkType,
    bytes: ChunkBytes,
    offsets: Vec<PartitionChunkOffset>,
}

/// Mining-recall `pread`s kept in the kernel together. The lane window is
/// separate: the lane waits for the whole recall before it submits.
const RECALL_INFLIGHT: usize = 2;

/// Widths of the `pread`s for one contiguous recall run, as
/// `(chunks from the run start, chunk count)`.
/// A run that fits in one `num_chunks_in_recall_range` slice is split in
/// half so two commands reach the controller. A longer run keeps that
/// slice width and pairs the slices. A one-chunk tail stays one `pread`.
fn recall_piece_plan(len: u64, max_chunks: u64) -> Vec<(u64, u64)> {
    let mut pieces = Vec::new();
    let mut done = 0u64;
    let cap = max_chunks.max(1);
    while done < len {
        let n = (len - done).min(cap);
        pieces.push((done, n));
        done += n;
    }
    if pieces.len() % RECALL_INFLIGHT != 0
        && let Some((off, n)) = pieces.pop()
    {
        if n > 1 {
            let first = n.div_ceil(2);
            pieces.push((off, first));
            pieces.push((off + first, n - first));
        } else {
            pieces.push((off, n));
        }
    }
    pieces
}

/// Two `pread`s of one recall run share `file`. `file` is a dup, so these
/// `pread`s do not wait for packed writes already in the kernel. Chunk bytes
/// land in `chunk_map` after each wave joins.
fn read_recall_run(
    file: &File,
    start: PartitionChunkOffset,
    len: u64,
    chunk_type: ChunkType,
    interval_start: PartitionChunkOffset,
    chunk_size: u64,
    chunk_len: usize,
    max_chunks: u64,
    chunk_map: &mut ChunkMap,
) -> eyre::Result<()> {
    let pieces = recall_piece_plan(len, max_chunks);
    let mut next = 0;
    while next < pieces.len() {
        let end = (next + RECALL_INFLIGHT).min(pieces.len());
        let mut batch = Vec::with_capacity(end - next);
        for &(done, n) in &pieces[next..end] {
            let piece = PartitionChunkOffset(start.0 + done as u32);
            let file_offset = *(piece - interval_start) as u64 * chunk_size;
            batch.push((piece, file_offset, n, vec![0_u8; n as usize * chunk_len]));
        }
        let loaded = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(batch.len());
            for (piece, file_offset, n, mut buf) in batch {
                handles.push(scope.spawn(move || {
                    let result = file.read_exact_at(&mut buf, file_offset);
                    (piece, n, buf, result)
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().expect("recall read"))
                .collect::<Vec<_>>()
        });
        for (piece, n, buf, result) in loaded {
            result.wrap_err_with(|| format!("recall read at offset {piece} count {n}"))?;
            for step in 0..n {
                let at = step as usize * chunk_len;
                let off = PartitionChunkOffset(piece.0 + step as u32);
                chunk_map.insert(off, (buf[at..at + chunk_len].to_vec(), chunk_type));
            }
        }
        next = end;
    }
    Ok(())
}

impl StorageModule {
    /// Initializes a new StorageModule
    pub fn new(storage_module_info: &StorageModuleInfo, config: &Config) -> eyre::Result<Self> {
        let mut submodule_map = NoditMap::new();
        let mut global_intervals = StorageIntervals::new();
        let index_write_generation = Arc::new(AtomicU64::new(0));
        let index_commit_fail_next = Arc::new(AtomicBool::new(false));
        let index_gap = Arc::new(index_drain::IndexGap::new());

        // Initialize the submodules from the StorageModuleInfo
        for (submodule_interval, dir) in storage_module_info.submodules.clone() {
            let sub_base_path = config.node_config.base_directory.join(dir.clone());

            tracing::info!(custom.sub_base_path = ?sub_base_path);
            fs::create_dir_all(&sub_base_path)?; // Ensure the directory exists (for component tests)

            // Get a file handle to the chunks.data file in the submodule
            let path = sub_base_path.join("chunks.dat");
            let chunks_file: Arc<Mutex<File>> = Arc::new(Mutex::new(
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true) // Optional: creates file if it doesn't exist
                    .truncate(false)
                    .open(&path)
                    .map_err(|e| {
                        eyre!(
                            "Failed to create or open chunks file: {} - {}",
                            path.display(),
                            e
                        )
                    })?,
            ));

            let submodule_db_path = sub_base_path.join("db");
            debug!("submodule_db_path: {:?}", submodule_db_path);
            let submodule_db = create_or_open_submodule_db(
                &submodule_db_path,
                // Args (incl. the test geometry cap) derived from the DatabaseConfig.
                irys_database::submodule_db_args(&config.node_config.database)?,
            )
            .map_err(|e| {
                eyre!(
                    "Failed to create or open submodule database: {} - {}",
                    submodule_db_path.display(),
                    e
                )
            })?;

            let params_path = sub_base_path.join(PACKING_PARAMS_FILE_NAME);
            // if we don't have an existing packing params file, write it
            if !params_path.exists() {
                let mut params = PackingParams {
                    packing_address: config.node_config.miner_address(),
                    ..Default::default()
                };
                if let Some(pa) = storage_module_info.partition_assignment {
                    params.partition_hash = Some(pa.partition_hash);
                    params.ledger = pa.ledger_id;
                    params.slot = pa.slot_index;
                }
                params.write_to_disk(&params_path);
            } else {
                // Load the packing params and check to see if they match
                let mut params =
                    PackingParams::from_toml(&params_path).expect("packing params to load");

                ensure!(
                    params.packing_address == config.node_config.miner_address(),
                    "Active mining address: {} does not match partition packing address {}",
                    config.node_config.miner_address(),
                    params.packing_address
                );

                // If disk has a partition hash that conflicts with the epoch
                // snapshot, the submodule was packed against a different genesis.
                if let (Some(ph), Some(pa)) = (
                    params.partition_hash,
                    storage_module_info.partition_assignment,
                ) {
                    ensure!(
                        ph == pa.partition_hash,
                        "Partition hash mismatch:\nexpected: {}\nfound   : {}\n\nError: Submodule partition assignments are out of sync with genesis block. \
                        This occurs when a new genesis block is created with a different last_epoch_hash, but submodules still have partition_hashes \
                        assigned from the previous genesis. To fix: clear the contents of the submodule directories and let them be repacked with the current genesis",
                        pa.partition_hash,
                        ph,
                    );
                }

                // Derive the desired state from the epoch snapshot
                let (want_hash, want_ledger, want_slot) =
                    match storage_module_info.partition_assignment {
                        Some(pa) => (Some(pa.partition_hash), pa.ledger_id, pa.slot_index),
                        None => (None, None, None),
                    };

                // Sync disk params to the desired state if anything drifted
                if params.partition_hash != want_hash
                    || params.ledger != want_ledger
                    || params.slot != want_slot
                {
                    params.partition_hash = want_hash;
                    params.ledger = want_ledger;
                    params.slot = want_slot;
                    params.last_updated_height = Some(0);
                    params.write_to_disk(&params_path);
                }
            }

            let intervals_file_path = sub_base_path.join("intervals.json");

            let submodules_intervals_file = PathBuf::from(&intervals_file_path);

            // Ensure the intervals.json has a default range
            ensure_default_intervals(&submodule_interval, &submodules_intervals_file)
                .expect("to ensure default intervals exist for submodule");

            // The submodule_map maps submodule intervals to specific instance of StorageSubmodule
            // that maintains system resources connected to the files in that submodule
            let db = DatabaseProvider(Arc::new(submodule_db));
            submodule_map
                .insert_strict(
                    submodule_interval,
                    StorageSubmodule {
                        path: dir,
                        file: chunks_file,
                        db: db.clone(),
                        intervals_file: Arc::new(Mutex::new(submodules_intervals_file)),
                        index_drain: index_drain::IndexDrain::spawn(
                            db,
                            Arc::clone(&index_write_generation),
                            Arc::clone(&index_commit_fail_next),
                            Arc::clone(&index_gap),
                        )?,
                    },
                )
                .map_err(|e| {
                    eyre!(
                        "Failed to insert submodule over interval: {}-{}, {:?}",
                        submodule_interval.start(),
                        submodule_interval.end(),
                        e
                    )
                })?;

            // Initially just mark the global intervals as Uninitialized for this submodules interval
            global_intervals
                .insert_merge_touching_if_values_equal(submodule_interval, ChunkType::Uninitialized)
                .map_err(|e| eyre::eyre!("Failed to insert submodule interval: {:?}", e))?;
        }

        // TODO: if there are any gaps, or the range doesn't cover a full module range panic
        let gaps = global_intervals
            .gaps_untrimmed(partition_chunk_offset_ii!(0, u32::MAX))
            .collect::<Vec<_>>();
        let expected = vec![partition_chunk_offset_ii!(
            TryInto::<u32>::try_into(config.consensus.num_chunks_in_partition)
                .expect("Value exceeds u32::MAX"),
            u32::MAX
        )];
        if gaps != expected {
            return Err(eyre!(
                "Invalid storage module config, expected range {:?}, got range {:?}",
                &expected,
                &gaps
            ));
        }

        // Attempt to load a global set of intervals from the submodules
        let loaded_intervals: NoditMap<
            PartitionChunkOffset,
            Interval<PartitionChunkOffset>,
            ChunkType,
        > = Self::load_intervals_from_submodules(&submodule_map, storage_module_info.id);

        // validate that the loaded intervals span the correct range
        let gaps = loaded_intervals
            .gaps_untrimmed(partition_chunk_offset_ii!(0, u32::MAX))
            .collect::<Vec<_>>();
        let expected = vec![partition_chunk_offset_ii!(
            TryInto::<u32>::try_into(config.consensus.num_chunks_in_partition)
                .expect("Value exceeds u32::MAX"),
            u32::MAX
        )];

        if gaps != expected {
            return Err(eyre!(
                "Invalid storage module config, expected range {:?}, got range {:?}",
                &expected,
                &gaps
            ));
        }

        #[cfg(not(any(test, feature = "test-utils")))]
        drop(index_commit_fail_next);

        Ok(Self {
            id: storage_module_info.id,
            partition_assignment: RwLock::new(storage_module_info.partition_assignment),
            pending_writes: RwLock::new(PendingWrites::default()),
            #[cfg(any(test, feature = "test-utils"))]
            index_commit_fail_next,
            #[cfg(test)]
            fail_entropy_read_nth: AtomicU64::new(0),
            #[cfg(test)]
            entropy_read_seq: AtomicU64::new(0),
            #[cfg(test)]
            index_views: AtomicU64::new(0),
            sync_in_progress: Mutex::new(()),
            data_writes_paused: AtomicBool::new(false),
            #[cfg(test)]
            sync_failure: Mutex::new(None),
            last_pending_write: RwLock::new(Instant::now()),
            intervals: Arc::new(RwLock::new(loaded_intervals)),
            index_write_generation,
            submodules: submodule_map,
            recent_chunk_times: Arc::new(RwLock::new(CircularBuffer::new(8_000))), // sample window 10s = 10s x 800 chunks/s = capacity 8_000
            config: config.clone(),
            disk: disk_lane::DiskGate::with_index_gap(index_gap),
        })
    }

    pub fn assign_partition(&self, partition_assignment: PartitionAssignment, update_height: u64) {
        let mut pa = self.partition_assignment.write().unwrap();
        *pa = Some(partition_assignment);

        // Also update the packing params file in each submodule
        for (_, submodule) in self.submodules.iter() {
            let params = PackingParams {
                packing_address: self.config.node_config.miner_address(),
                partition_hash: Some(partition_assignment.partition_hash),
                ledger: partition_assignment.ledger_id,
                slot: partition_assignment.slot_index,
                last_updated_height: Some(update_height),
            };

            let params_path = submodule.path.join(PACKING_PARAMS_FILE_NAME);
            params.write_to_disk(&params_path);
        }
    }

    /// Clears the current partition assignment and persists the change on disk.
    ///
    /// Writes `packing_params.toml` with `partition_hash = None`, `ledger = None`,
    /// `slot = None`, and updates `last_updated_height`. In-memory assignment is set to `None`.
    pub fn clear_assignment(&self, update_height: u64) {
        // Clear in-memory assignment
        let mut pa = self.partition_assignment.write().unwrap();
        *pa = None;

        // Persist to each submodule's packing params
        for (_, submodule) in self.submodules.iter() {
            let params = PackingParams {
                packing_address: self.config.node_config.miner_address(),
                partition_hash: None,
                ledger: None,
                slot: None,
                last_updated_height: Some(update_height),
            };

            let params_path = submodule.path.join(PACKING_PARAMS_FILE_NAME);
            params.write_to_disk(&params_path);
        }
    }

    /// Returns the StorageModules partition_hash if assigned
    pub fn partition_hash(&self) -> Option<PartitionHash> {
        let pa = self.partition_assignment.read().unwrap();
        (*pa).map(|part_assign| part_assign.partition_hash)
    }

    pub fn partition_assignment(&self) -> Option<PartitionAssignment> {
        let pa = self.partition_assignment.read().unwrap();
        *pa
    }

    pub fn last_pending_write(&self) -> Instant {
        *self.last_pending_write.read().unwrap()
    }

    pub fn has_pending_writes(&self) -> bool {
        !self.pending_writes.read().unwrap().is_empty()
    }

    /// A packed run is ready for the disk when the chunk disk is idle, one
    /// run fills the write cap, the pending set has reached the durability
    /// count, or the oldest short run has waited `reorder_grace` and no
    /// earlier short run still holds that grace. Other short runs stay in
    /// memory so a neighbor can join while the disk stays busy.
    fn pending_run_ready(&self) -> bool {
        let pending = self.pending_writes.read().unwrap();
        if pending.is_empty() {
            return false;
        }
        if self.disk.chunk_disk_idle() {
            return true;
        }
        let threshold = self.config.node_config.storage.num_writes_before_sync;
        if pending.len() as u64 >= threshold {
            return true;
        }
        let chunk = self.config.consensus.chunk_size.max(1);
        let cap = disk_lane::WRITE_RUN_MAX_BYTES.max(chunk);
        self.has_full_write_run(&pending, cap)
            || (self.has_aged_pending(&pending, chunk) && !self.disk.short_writes_held())
    }

    /// True when the oldest chunk in some run has waited out `reorder_grace`.
    fn has_aged_pending(&self, pending: &PendingWrites, chunk: u64) -> bool {
        let grace = disk_lane::reorder_grace(chunk);
        pending
            .queued_at
            .values()
            .any(|queued_at| queued_at.elapsed() >= grace)
    }

    /// Bytes of the longest packed run that may hit the disk now. Zero when
    /// every run is still held for a neighbor. While the disk is busy, a
    /// short-run hold leaves every short run out of this count, so entropy
    /// can use the disk while those runs wait for a neighbor. An idle disk
    /// counts every queued run.
    fn longest_ready_write_bytes(&self) -> u64 {
        let pending = self.pending_writes.read().unwrap();
        if pending.is_empty() {
            return 0;
        }
        let chunk = self.config.consensus.chunk_size.max(1);
        let cap = disk_lane::WRITE_RUN_MAX_BYTES.max(chunk);
        if self.disk.chunk_disk_idle() {
            return self
                .write_run_metas(&pending, cap)
                .into_iter()
                .map(|run| run.byte_len)
                .max()
                .unwrap_or(0);
        }
        let grace = disk_lane::reorder_grace(chunk);
        let due_all = self.short_runs_due(pending.len());
        let held = self.disk.short_writes_held();
        let mut best_full = 0_u64;
        let mut best_short: Option<(Instant, PartitionChunkOffset, u64)> = None;
        for run in self.write_run_metas(&pending, cap) {
            if due_all || run.byte_len >= cap {
                best_full = best_full.max(run.byte_len);
                continue;
            }
            if held || run.oldest.elapsed() < grace {
                continue;
            }
            let take = match best_short {
                None => true,
                Some((oldest, start, _)) => {
                    run.oldest < oldest || (run.oldest == oldest && run.start < start)
                }
            };
            if take {
                best_short = Some((run.oldest, run.start, run.byte_len));
            }
        }
        best_full.max(best_short.map(|(_, _, bytes)| bytes).unwrap_or(0))
    }

    /// Durability and an owed recall flush write short runs. The count is
    /// the whole pending set, matching `num_writes_before_sync`.
    fn short_runs_due(&self, pending_count: usize) -> bool {
        let threshold = self.config.node_config.storage.num_writes_before_sync;
        self.disk.recall_flush_is_owed() || pending_count as u64 >= threshold
    }

    /// True when the reorder scan holds one contiguous run of at least `cap`
    /// bytes. This mirrors `can_extend_run` and does not copy chunk bytes.
    fn has_full_write_run(&self, pending: &PendingWrites, cap: u64) -> bool {
        self.write_run_metas(pending, cap)
            .iter()
            .any(|run| run.byte_len >= cap)
    }

    /// Runs inside one reorder scan, in offset order. The scan is offset
    /// order so a pile of one-chunk migration writes cannot hide a long
    /// ingress run once the pending set is wider than the scan.
    fn write_run_metas(&self, pending: &PendingWrites, cap: u64) -> Vec<RunMeta> {
        let chunk_size = self.config.consensus.chunk_size.max(1);
        let mut indexed: Vec<(disk_lane::WritePriority, PartitionChunkOffset)> = pending
            .iter()
            .map(|(offset, state)| {
                let priority = pending
                    .priorities
                    .get(offset)
                    .copied()
                    .unwrap_or(match state.1 {
                        ChunkType::Entropy => disk_lane::WritePriority::Packing,
                        _ => disk_lane::WritePriority::Ingress,
                    });
                (priority, *offset)
            })
            .collect();
        indexed.sort_by_key(|(_, offset)| *offset);
        indexed.truncate(disk_lane::WRITE_REORDER_CHUNKS);

        let mut runs = Vec::new();
        let mut current: Option<RunCursor> = None;
        let mut current_priority: Option<disk_lane::WritePriority> = None;
        let mut current_oldest: Option<Instant> = None;
        for (priority, offset) in indexed {
            let Some((bytes, chunk_type)) = pending.get(&offset) else {
                continue;
            };
            let next_len = bytes.len() as u64;
            let next_type = *chunk_type;
            let queued_at = pending
                .queued_at
                .get(&offset)
                .copied()
                .unwrap_or_else(Instant::now);
            let extend = current_priority == Some(priority)
                && current.as_ref().is_some_and(|run| {
                    self.can_extend_run(chunk_size, cap, run, offset, next_type, next_len)
                });
            if extend {
                let run = current.as_mut().expect("extend checks the current run");
                run.last = offset;
                run.chunk_count += 1;
                run.byte_len += next_len;
                if let Some(oldest) = current_oldest.as_mut()
                    && queued_at < *oldest
                {
                    *oldest = queued_at;
                }
                continue;
            }
            if let Some(cursor) = current.take() {
                runs.push(RunMeta {
                    start: cursor.start,
                    byte_len: cursor.byte_len,
                    oldest: current_oldest.unwrap_or_else(Instant::now),
                });
            }
            current_priority = Some(priority);
            current_oldest = Some(queued_at);
            current = Some(RunCursor {
                start: offset,
                last: offset,
                chunk_type: next_type,
                chunk_count: 1,
                byte_len: next_len,
            });
        }
        if let Some(cursor) = current.take() {
            runs.push(RunMeta {
                start: cursor.start,
                byte_len: cursor.byte_len,
                oldest: current_oldest.unwrap_or_else(Instant::now),
            });
        }
        runs
    }

    /// True when a *data* write for `offset` is queued but not yet flushed. A
    /// queued Entropy write does not count: `write_data_chunk` folds data into
    /// that entry itself, so such an offset is still writable.
    pub fn is_data_write_pending_at(&self, offset: PartitionChunkOffset) -> bool {
        let pending = self.pending_writes.read().unwrap();
        pending.occupancy.contains_key(&offset)
            || pending
                .get(&offset)
                .is_some_and(|(_, chunk_type)| *chunk_type == ChunkType::Data)
    }

    /// Bytes of chunk data queued in memory awaiting flush. Background body
    /// migration pauses writing to this module once this reaches its ceiling
    /// (`storage.max_pending_write_bytes`).
    pub fn pending_write_bytes(&self) -> u64 {
        let pending = self.pending_writes.read().unwrap();
        let packed = pending
            .values()
            .map(|(bytes, _)| bytes.len() as u64)
            .sum::<u64>();
        packed.saturating_add(pending.queued_unpacked_bytes)
    }

    /// Number of submodules (independent disks) backing this module.
    pub fn submodule_count(&self) -> usize {
        self.submodules.len()
    }

    /// Refuse new data writes until [`Self::resume_data_writes`]. Taken under
    /// the `pending_writes` write lock so no writer is mid-insert when the flag
    /// flips: whatever was queued before this call is exactly what
    /// `drop_pending_writes_in_range` removes, and nothing can be queued after.
    /// Every data writer — the body worker, data sync, chunk ingress — goes
    /// through [`Self::write_data_chunk`], so this is the one gate for all of
    /// them. Entropy (packing) writes are unaffected.
    pub fn pause_data_writes(&self) {
        {
            let _pending = self.pending_writes.write().unwrap();
            self.data_writes_paused.store(true, Ordering::SeqCst);
            self.index_write_generation.fetch_add(1, Ordering::SeqCst);
        }
        // Slots already queued must not be packed into a paused module.
        self.fail_queued_sweeps();
    }

    pub fn resume_data_writes(&self) {
        self.data_writes_paused.store(false, Ordering::SeqCst);
    }

    pub fn data_writes_paused(&self) -> bool {
        self.data_writes_paused.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn fail_next_sync_at(&self, point: SyncFailurePoint) {
        *self.sync_failure.lock().unwrap() = Some(point);
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn fail_next_index_commit(&self) {
        self.index_commit_fail_next.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn occupy_offset_for_test(&self, offset: PartitionChunkOffset) {
        let generation = self.index_write_generation.load(Ordering::SeqCst);
        self.pending_writes
            .write()
            .unwrap()
            .occupancy
            .insert(offset, generation);
    }

    #[cfg(test)]
    fn release_occupied_offset_for_test(&self, offset: PartitionChunkOffset) {
        self.pending_writes
            .write()
            .unwrap()
            .occupancy
            .remove(&offset);
    }

    #[cfg(test)]
    fn fail_entropy_read_at(&self, n: u64) {
        self.fail_entropy_read_nth.store(n, Ordering::SeqCst);
        self.entropy_read_seq.store(0, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn submit_index_op_for_test(
        &self,
        offset: PartitionChunkOffset,
        data_path: Vec<u8>,
        generation: u64,
    ) -> std::sync::mpsc::Receiver<Result<(), WriteDataChunkError>> {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let submodule = self
            .submodules
            .get_at_point(offset)
            .expect("test offset must belong to a submodule");
        submodule.index_drain.submit(index_drain::IndexOp {
            path_hash: UnpackedChunk::hash_data_path(&data_path),
            data_path,
            offset,
            generation,
            done: done_tx,
            wake: None,
        });
        done_rx
    }

    #[cfg(test)]
    fn inject_sync_failure(&self, point: SyncFailurePoint) -> eyre::Result<()> {
        let mut failure = self.sync_failure.lock().unwrap();
        if *failure == Some(point) {
            *failure = None;
            eyre::bail!("injected storage-module sync failure at {point:?}");
        }
        Ok(())
    }

    /// Resolves every local partition placement funded for one transaction
    /// chunk. This is the canonical forward mapping used by ingress writes,
    /// migration, and durability queries.
    ///
    /// `None` means this storage module has no index entry for `data_root`.
    /// `Some([])` means the root is indexed, but this transaction-relative
    /// offset is not funded inside this partition.
    pub fn partition_offsets_for_data_root_chunk(
        &self,
        data_root: DataRoot,
        tx_offset: TxChunkOffset,
    ) -> eyre::Result<Option<Vec<PartitionChunkOffset>>> {
        let infos = self.collect_data_root_infos(data_root)?;
        if infos.0.is_empty() {
            return Ok(None);
        }

        let raw_tx_offset = u32::from(tx_offset);
        let chunk_byte_offset = u64::from(raw_tx_offset)
            .checked_mul(self.config.consensus.chunk_size)
            .ok_or_else(|| eyre::eyre!("chunk byte offset overflow"))?;
        let num_chunks_in_partition = self.config.consensus.num_chunks_in_partition;
        let mut offsets = Vec::new();
        for info in infos.0 {
            if chunk_byte_offset >= info.data_size {
                warn!(
                    %data_root,
                    %tx_offset,
                    start_offset = %info.start_offset,
                    data_size = info.data_size,
                    "Skipping storage-module placement past the data root's declared size"
                );
                continue;
            }
            let relative_offset = i64::from(info.start_offset.0) + i64::from(raw_tx_offset);
            if relative_offset < 0 || relative_offset as u64 >= num_chunks_in_partition {
                continue;
            }
            let relative_offset = u32::try_from(relative_offset)
                .map_err(|_| eyre::eyre!("partition-relative chunk offset exceeds u32"))?;
            offsets.push(PartitionChunkOffset::from(relative_offset));
        }
        offsets.sort_unstable();
        offsets.dedup();
        Ok(Some(offsets))
    }

    /// Returns true only when the offset is on disk as transaction data and no
    /// buffered write currently supersedes it.
    pub fn is_data_chunk_durable_at(&self, offset: PartitionChunkOffset) -> bool {
        if self.pending_writes.read().unwrap().contains_key(&offset) {
            return false;
        }
        self.intervals
            .read()
            .unwrap()
            .get_at_point(offset)
            .is_some_and(|chunk_type| *chunk_type == ChunkType::Data)
    }

    /// Candidate transaction-relative offsets of `data_root` whose bodies are
    /// fsynced in this module as `ChunkType::Data`.
    ///
    /// This is the durability signal cached-body reclamation is gated on. It is
    /// derived from the same interval state the node reloads after a restart, so
    /// it cannot disagree with what recovery will see. Resolves the submodule
    /// index once per root and checks only bodies the cache can actually
    /// reclaim, rather than walking the transaction's entire funded range.
    pub fn durable_tx_offsets_for_data_root(
        &self,
        data_root: DataRoot,
        candidates: &BTreeSet<TxChunkOffset>,
    ) -> eyre::Result<BTreeSet<TxChunkOffset>> {
        let infos = self.collect_data_root_infos(data_root)?;
        let mut durable = BTreeSet::new();
        if infos.0.is_empty() {
            return Ok(durable);
        }

        let chunk_size = self.config.consensus.chunk_size;
        let num_chunks_in_partition = self.config.consensus.num_chunks_in_partition;
        for tx_offset in candidates {
            let raw_offset = u32::from(*tx_offset);
            let chunk_byte_offset = u64::from(raw_offset)
                .checked_mul(chunk_size)
                .ok_or_else(|| eyre::eyre!("chunk byte offset overflow"))?;
            for info in &infos.0 {
                if chunk_byte_offset >= info.data_size {
                    continue;
                }
                let relative_offset = i64::from(info.start_offset.0) + i64::from(raw_offset);
                if relative_offset < 0 || relative_offset as u64 >= num_chunks_in_partition {
                    continue;
                }
                let relative_offset = u32::try_from(relative_offset)
                    .map_err(|_| eyre::eyre!("partition-relative chunk offset exceeds u32"))?;
                // One short lock acquisition per offset rather than holding
                // pending/intervals across the whole loop: this runs on the
                // background prune path and must not stall chunk writes.
                if self.is_data_chunk_durable_at(PartitionChunkOffset::from(relative_offset)) {
                    durable.insert(*tx_offset);
                    break;
                }
            }
        }
        Ok(durable)
    }

    /// Reinit intervals setting them as Uninitialized, and erase db
    pub fn reset(&self) -> eyre::Result<Interval<PartitionChunkOffset>> {
        {
            let mut pending = self.pending_writes.write().unwrap();
            self.index_write_generation.fetch_add(1, Ordering::SeqCst);
            for (_, submodule) in self.submodules.iter() {
                submodule.index_drain.wait_idle();
            }
            pending.chunks.clear();
            pending.occupancy.clear();
            pending.priorities.clear();
            pending.queued_at.clear();
        }
        self.fail_queued_sweeps();
        self.poll_acks();
        let storage_interval = {
            let mut intervals = self.intervals.write().unwrap();
            let start = intervals.first_key_value().unwrap().0.start();
            let end = intervals.last_key_value().unwrap().0.end();
            let storage_interval = ii(start, end);
            *intervals = StorageIntervals::new();
            intervals
                .insert_strict(storage_interval, ChunkType::Uninitialized)
                .expect("Failed to create new interval, should never happen as interval is empty!");
            storage_interval
        };
        self.write_intervals_to_submodules()
            .wrap_err("Could not update submodule interval files")?;

        for (_interval, submodule) in self.submodules.iter() {
            submodule.db.update_eyre(clear_submodule_database)?;
        }

        Ok(storage_interval)
    }

    /// Returns whether the given chunk offset falls within this StorageModules assigned range
    pub fn contains_offset(&self, chunk_offset: LedgerChunkOffset) -> bool {
        self.partition_assignment
            .read()
            .unwrap()
            .and_then(|part| part.slot_index)
            .map(|slot_index| {
                let start_offset =
                    slot_index as u64 * self.config.consensus.num_chunks_in_partition;
                let end_offset = start_offset + self.config.consensus.num_chunks_in_partition;
                (start_offset..end_offset).contains(&*chunk_offset)
            })
            .unwrap_or(false)
    }

    /// Only used in testing to get a db reference to verify insertions happened.
    pub fn get_submodule(&self, local_offset: PartitionChunkOffset) -> Option<&StorageSubmodule> {
        if let Some(submodule) = self.submodules.get_at_point(local_offset) {
            Some(submodule)
        } else {
            None
        }
    }

    /// Synchronizes chunks to disk when sufficient writes have accumulated
    ///
    /// Process:
    /// 1. Collects pending writes that meet threshold for each submodule
    /// 2. Acquires write lock only if batched writes exist
    /// 3. Writes chunks to disk and removes them from pending queue
    ///
    /// The sync threshold is configured via `min_writes_before_sync` to optimize
    /// disk writes and minimize fragmentation.
    pub fn sync_pending_chunks(&self) -> eyre::Result<()> {
        self.sync_pending_chunks_inner(false)
    }

    /// Force syncs (writes) all pending writes for this storage module.
    ///
    /// Use only for outlier tail-drain situations such as orderly shutdown,
    /// recovery, or a module that has become idle. Calling this on a normal
    /// ingestion or migration hot path defeats the pending-chunk buffer: it
    /// prevents efficient disk striping and increases on-disk fragmentation.
    /// Normal writers must use [`Self::sync_pending_chunks`], which preserves
    /// the configured batching threshold.
    ///
    /// Process:
    /// 1. Collects pending writes for each submodule
    /// 2. Acquires write lock only if some pending writes exist
    /// 3. Writes chunks to disk and removes them from pending queue
    ///
    pub fn force_sync_pending_chunks(&self) -> eyre::Result<()> {
        // Deposited bodies are not in the packed map yet. Pack them first so
        // the flush below persists the bytes the caller asked to finish.
        self.drain_entropy_queue();
        self.sync_pending_chunks_inner(true)
    }

    fn sync_pending_chunks_inner(&self, force: bool) -> eyre::Result<()> {
        // The lane thread owns the chunk write and the interval files around
        // it. A non-force caller only wakes that thread. A force caller waits
        // until the lane has drained the pending runs. With no lane, or on the
        // lane thread itself, this caller writes.
        if self.disk.lane_running() && !self.disk.on_lane_thread() {
            if !force {
                self.disk.notify();
                return Ok(());
            }
            if Self::lane_request_done(self.disk.request_lane_force())? {
                return Ok(());
            }
        }
        self.commit_pending_runs(force, None, false)
    }

    /// `Ok(true)` when the lane finished the request. `Ok(false)` when the
    /// lane stopped and this caller must do the write.
    fn lane_request_done(result: Result<(), disk_lane::LaneHandoffError>) -> eyre::Result<bool> {
        match result {
            Ok(()) => Ok(true),
            Err(disk_lane::LaneHandoffError::Stopped) => Ok(false),
            Err(disk_lane::LaneHandoffError::Failed(error)) => Err(eyre!(error)),
        }
    }

    /// `max_runs` commits at most that many pwrites and leaves the rest pending.
    /// `None` commits every run that meets the threshold.
    /// `coalesce` sorts a reorder buffer and joins adjacent chunks up to the
    /// write cap. The no-lane path passes false and keeps the entropy cap.
    ///
    /// A recall flush holds the owed flag across this call. An unlimited sync
    /// that observes the flag returns without writing, so the recall waits
    /// only for the window and not for the rest of the batch.
    fn commit_pending_runs(
        &self,
        force: bool,
        max_runs: Option<usize>,
        coalesce: bool,
    ) -> eyre::Result<()> {
        // TODO: rework this function
        // 1.) use batches for fsync, instead of the all the pending writes (reduces impact of write errors)
        // 2.) pending writes are per-sm, we should delegate flushing to the StorageSubmodule
        // doing this removes having to locate the correct submodule again in `write_chunk_internal`,
        // and lets us increase throughput - we can spawn blocking tasks in parallel for each submodule (as each is it's own IO domain/drive)

        // Multiple services may request a flush concurrently. Serializing the
        // operation avoids duplicate writes and lets `pending_writes` remain
        // the single durability fence until this commit finishes.
        let _sync_guard = self
            .sync_in_progress
            .lock()
            .map_err(|error| eyre::eyre!("storage-module sync lock poisoned: {error}"))?;

        if max_runs.is_none() && self.disk.recall_flush_is_owed() {
            return Ok(());
        }

        let then = Instant::now();
        let threshold = if force || max_runs.is_some() {
            0
        } else {
            self.config.node_config.storage.num_writes_before_sync
        };
        // Capture before this commit's own write hold. An idle disk writes
        // every short run and does not arm the hold. A busy disk keeps one
        // aged short run so the others can still gain a neighbor.
        let disk_idle = self.disk.chunk_disk_idle();
        let chunk = self.config.consensus.chunk_size.max(1);
        let run_cap = if coalesce {
            disk_lane::WRITE_RUN_MAX_BYTES.max(chunk)
        } else {
            self.config
                .node_config
                .storage
                .entropy_sweep_max_bytes
                .max(chunk)
        };

        // First use read lock to check if we have work to do
        let (mut write_batch, pending_count, queued_at) = {
            let pending = self.pending_writes.read().unwrap();
            let pending_count = pending.len();
            let queued_at = if coalesce {
                pending.queued_at.clone()
            } else {
                HashMap::new()
            };

            let pending_writes = if coalesce && let Some(limit) = max_runs {
                self.select_coalesced_window(
                    &pending,
                    limit,
                    disk_lane::WRITE_REORDER_CHUNKS,
                    run_cap,
                    !disk_idle && !self.short_runs_due(pending.len()),
                )
            } else if let Some(limit) = max_runs {
                self.select_pending_window(&pending, limit, run_cap)
            } else {
                self.submodules
                    .iter()
                    .flat_map(|(interval, _submodule)| {
                        let submodule_writes: Vec<_> = pending
                            .iter()
                            .filter(|(offset, _)| interval.contains_point(**offset))
                            .map(|(offset, state)| {
                                let priority = pending.priorities.get(offset).copied().unwrap_or(
                                    match state.1 {
                                        ChunkType::Entropy => disk_lane::WritePriority::Packing,
                                        _ => disk_lane::WritePriority::Ingress,
                                    },
                                );
                                (*offset, state.clone(), priority)
                            })
                            .collect();

                        // each submodule is it's own "IO domain", so we don't process pending writes for one if it's queue is too small
                        if submodule_writes.len() as u64 >= threshold {
                            submodule_writes
                        } else {
                            Vec::new()
                        }
                    })
                    .collect::<Vec<_>>()
            };

            drop(pending);
            (pending_writes, pending_count, queued_at)
        };

        if write_batch.is_empty() {
            return Ok(());
        }

        let mut runs = if coalesce {
            self.plan_ranked_runs(&write_batch, run_cap)
        } else {
            self.plan_write_runs(&write_batch, run_cap)
        };
        if let Some(limit) = max_runs {
            runs.truncate(limit);
        }
        // A short run seeks about as much as a full one and moves far less
        // data. While the disk is busy, one pass writes every full run and
        // the oldest aged short run. The hold keeps the other short runs
        // queued for a neighbor. Durability, a recall flush, and an idle
        // disk write every run. An idle release does not arm the hold.
        let mut emitted_short = false;
        if coalesce && !force && !disk_idle && !self.short_runs_due(pending_count) {
            let full = disk_lane::WRITE_RUN_MAX_BYTES.max(chunk);
            let grace = disk_lane::reorder_grace(chunk);
            let held = self.disk.short_writes_held();
            let mut oldest_short: Option<(Instant, u32, usize)> = None;
            if !held {
                for (index, run) in runs.iter().enumerate() {
                    if run.bytes.len() as u64 >= full {
                        continue;
                    }
                    let Some(oldest) = run
                        .offsets
                        .iter()
                        .filter_map(|offset| queued_at.get(offset).copied())
                        .min()
                    else {
                        continue;
                    };
                    if oldest.elapsed() < grace {
                        continue;
                    }
                    let start = run.start.0;
                    let take = match oldest_short {
                        None => true,
                        Some((prev, prev_start, _)) => {
                            oldest < prev || (oldest == prev && start < prev_start)
                        }
                    };
                    if take {
                        oldest_short = Some((oldest, start, index));
                    }
                }
            }
            let keep_short = oldest_short.map(|(_, _, index)| index);
            let mut kept = Vec::with_capacity(runs.len());
            for (index, run) in runs.into_iter().enumerate() {
                if run.bytes.len() as u64 >= full {
                    kept.push(run);
                } else if keep_short == Some(index) {
                    emitted_short = true;
                    kept.push(run);
                }
            }
            runs = kept;
        }
        if runs.is_empty() {
            return Ok(());
        }
        if max_runs.is_some() {
            let kept: BTreeSet<PartitionChunkOffset> = runs
                .iter()
                .flat_map(|run| run.offsets.iter().copied())
                .collect();
            write_batch.retain(|(offset, _, _)| kept.contains(offset));
        }

        // Only the submodules this batch writes. An untouched `intervals.json`
        // stays as it was, and its `chunks.dat` is not synced again.
        let mut touched_starts = BTreeSet::new();
        for run in &runs {
            let (interval, _) = self.get_submodule_for_offset(run.start)?;
            touched_starts.insert(interval.start());
        }

        #[cfg(test)]
        self.disk.record_commit_thread();

        let mut intervals = self
            .intervals
            .write()
            .map_err(|e| eyre::eyre!("Failed to acquire write lock on intervals: {}", e))?;

        // write every offset to the intervals file as Interrupted
        // we sync the correct intervals state once this commit is written
        for (chunk_offset, _, _) in write_batch.iter() {
            Self::cut_then_insert_interval_if_touching(
                &mut intervals,
                *chunk_offset,
                ChunkType::Interrupted,
            );
        }

        drop(intervals);

        self.write_intervals_files(Some(&touched_starts))
            .wrap_err("Could not update submodule interval files, if this is a component test with storage_module that drops after the test, this error is benign")?;

        let len = write_batch.len();

        // One capped pwrite per adjacent run. The lane keeps a few of those
        // calls in flight. A mining recall queued between batches starts
        // before the next batch. The hold covers the flush so an entropy
        // read does not cut in, and it ends before this function returns.
        let _write_hold = self.disk.hold_writes();
        if self.disk.lane_running() {
            self.write_runs_window(&runs)?;
        } else {
            for run in &runs {
                self.write_run(run)?;
            }
        }

        #[cfg(test)]
        self.inject_sync_failure(SyncFailurePoint::BeforeDataFsync)?;

        // fsync this write batch BEFORE committing the interval state
        // as fsync can error on us if the underlying storage has issues.
        // A recall window fsyncs only the files it wrote.
        for (interval, submodule) in self.submodules.iter() {
            if !touched_starts.contains(&interval.start()) {
                continue;
            }
            self.disk.yield_to_recall();
            let file_arc = Arc::clone(&submodule.file);
            let file = self.disk.lock_chunks(&file_arc);
            // Ensure data is flushed to disk prior to drop
            file.sync_all()
                .map_err(|e| eyre::eyre!("Failed to sync data to disk: {}", e))?;
        }

        #[cfg(test)]
        self.inject_sync_failure(SyncFailurePoint::BeforeIntervalCommit)?;

        // persist the state from all the write calls
        // save the updated intervals
        self.write_intervals_files(Some(&touched_starts))
            .wrap_err("Could not update submodule interval files, if this is a component test with storage_module that drops after the test, this error is benign")?;

        // Remove only the exact values written by this snapshot. A concurrent
        // producer may have replaced an entry while the disk I/O was running;
        // that newer value must remain queued for the next batch.
        let mut pending = self.pending_writes.write().unwrap();
        for (chunk_offset, state, _) in &write_batch {
            if pending.get(chunk_offset) == Some(state) {
                pending.remove(chunk_offset);
                pending.priorities.remove(chunk_offset);
                pending.queued_at.remove(chunk_offset);
            }
        }

        debug!(
            "sync_pending_chunks took {:.3}s for {} chunks",
            &then.elapsed().as_secs_f64(),
            &len
        );

        if emitted_short {
            self.disk.arm_short_hold(disk_lane::reorder_grace(chunk));
        }

        Ok(())
    }

    /// Persists partition interval data to individual submodules.
    ///
    /// # Overview
    /// While the parent StorageModule maintains a global view of all partition intervals
    /// across its submodules, each submodule records its own localized view. This function:
    ///
    /// 1. Takes the global intervals from the StorageModule
    /// 2. For each submodule, extracts only the interval portions relevant to that submodule
    /// 3. Writes the filtered intervals to an `intervals.json` file in each submodule's directory
    ///
    /// # Parameters
    /// * `intervals` - Reference to the global StorageIntervals containing all chunk mappings
    /// * `submodules` - Map of submodule intervals to their respective submodule instances
    ///
    /// # Returns
    /// * `eyre::Result<()>` - Success or error during the write operation
    ///
    /// # Note
    /// If a submodule has no intervals after filtering, a default `Uninitialized` interval
    /// is created spanning the submodule's entire range to ensure consistency.
    pub fn write_intervals_to_submodules(&self) -> eyre::Result<()> {
        // A running lane writes the files itself, before and after its chunk
        // writes. This caller waits. A stopped lane, and the lane thread, write
        // here.
        if self.disk.lane_running()
            && !self.disk.on_lane_thread()
            && Self::lane_request_done(self.disk.request_lane_persist())?
        {
            return Ok(());
        }
        self.write_intervals_files(None)
    }

    /// Rewrite `intervals.json`. `only` limits the write to those submodule
    /// starts. `None` rewrites every submodule.
    fn write_intervals_files(
        &self,
        only: Option<&BTreeSet<PartitionChunkOffset>>,
    ) -> eyre::Result<()> {
        let intervals = self.intervals.read().unwrap();

        // Loop though each of the submodule ranges
        for (submodule_interval, submodule) in self.submodules.iter() {
            if only.is_some_and(|starts| !starts.contains(&submodule_interval.start())) {
                continue;
            }
            // Split out the ChunkType intervals that overlap the submodule interval
            let mut working_copy = intervals.clone();
            let cut_iter = working_copy.cut(*submodule_interval);
            drop(working_copy);
            // Write them to the submodules disk
            if let Ok(mut submodule_intervals) = NoditMap::from_iter_strict(cut_iter) {
                // Make sure the there is at least one interval spanning the submodule range
                if submodule_intervals.is_empty() {
                    submodule_intervals
                        .insert_merge_touching_if_values_equal(
                            *submodule_interval,
                            ChunkType::Uninitialized,
                        )
                        .expect("to insert a default range to the submodule intervals");
                }

                let path = submodule.intervals_file.lock().unwrap();

                let mut file = get_atomic_file(path.clone())?;
                // this `file` is actually a temporary file that will get renamed over the original, once we commit
                file.write_all(serde_json::to_string(&submodule_intervals)?.as_bytes())?;
                file.commit()?;

                drop(path);
            }
        }
        drop(intervals);

        Ok(())
    }

    /// Reconstructs the global StorageIntervals by loading and merging interval data from all submodules.
    ///
    /// # Overview
    /// This function rebuilds a complete view of all chunk storage intervals by:
    ///
    /// 1. Creating an empty global intervals container
    /// 2. Reading each submodule's `intervals.json` file
    /// 3. Merging all submodule intervals into the global container
    ///
    /// # Parameters
    /// * `submodules` - Map containing all storage submodules
    ///
    /// # Returns
    /// * `StorageIntervals` - A complete, merged map of all intervals across all submodules
    ///
    /// # Panics
    /// * If unable to lock a submodule's intervals file mutex
    /// * If reading a submodule's intervals file fails
    /// * If interval insertion into the global map fails due to overlapping intervals
    fn load_intervals_from_submodules(
        submodules: &SubmoduleMap,
        module_id: usize,
    ) -> StorageIntervals {
        let mut global_intervals = StorageIntervals::new();
        let mut interrupted_count = 0;

        for (_, submodule) in submodules.iter() {
            let file = submodule
                .intervals_file
                .lock()
                .expect("to lock the submodule intervals file mutex");
            let submodule_intervals =
                read_intervals_file(&file).expect("to read submodule intervals file");

            for (interval, chunk_type) in submodule_intervals {
                let set_chunk_type = match chunk_type {
                    ChunkType::Interrupted => {
                        interrupted_count += 1;
                        warn!(
                            "Chunk @ interval ({}, {}) was interrupted, resetting to Uninitialized",
                            interval.start(),
                            interval.end()
                        );
                        ChunkType::Uninitialized
                    }
                    ChunkType::Entropy | ChunkType::Data | ChunkType::Uninitialized => chunk_type,
                };
                global_intervals
                    .insert_merge_touching_if_values_equal(interval, set_chunk_type)
                    .expect("to insert interval into global intervals map");
            }
        }

        if interrupted_count > 0 {
            error!(
                "Found {} interrupted writes in storage module {}",
                interrupted_count, module_id
            );
        }

        global_intervals
    }

    pub fn get_chunk_type(&self, chunk_offset: &PartitionChunkOffset) -> Option<ChunkType> {
        // Check pending writes first
        if let Some((_, chunk_type)) = self.pending_writes.read().unwrap().get(chunk_offset) {
            return Some(*chunk_type);
        }

        // Fall back to on-disk data
        self.intervals
            .read()
            .unwrap()
            .get_at_point(*chunk_offset)
            .copied()
    }

    /// Reads chunks from the specified range and returns their data and storage state
    ///
    /// Takes a range [start, end) of partition-relative offsets (end exclusive).
    /// Returns a map of chunk offsets to their data and type, excluding uninitialized chunks.
    /// Chunks are read from physical storage for initialized intervals that overlap the range.
    pub fn read_chunks(
        &self,
        chunk_range: Interval<PartitionChunkOffset>,
    ) -> eyre::Result<ChunkMap> {
        // Entropy-class read: mining recalls and an in-progress flush go first.
        self.disk.yield_to_recall();
        self.disk.yield_to_writes();
        self.read_chunks_inner(chunk_range, false)
    }

    fn read_chunks_inner(
        &self,
        chunk_range: Interval<PartitionChunkOffset>,
        recall: bool,
    ) -> eyre::Result<ChunkMap> {
        let mut chunk_map = ChunkMap::new();

        // Snapshot only the relevant interval metadata. Physical reads and
        // pending-write lookups must not hold the global interval lock.
        let overlapping = self
            .intervals
            .read()
            .unwrap()
            .overlapping(chunk_range)
            .map(|(interval, chunk_type)| (*interval, *chunk_type))
            .collect::<Vec<_>>();

        // Plan disk runs first. Pending and uninitialized offsets split a run
        // so a hole is not filled from the next chunk. A mining recall dups
        // each chunks.dat and reads at once. Any other read holds the file
        // lock across the runs on that file.
        let mut disk_runs: Vec<(PartitionChunkOffset, u64, ChunkType)> = Vec::new();
        let mut visited: Vec<PartitionChunkOffset> = Vec::new();

        for (interval, interval_chunk_type) in overlapping {
            let start = *chunk_range.start().max(interval.start());
            let end = *chunk_range.end().min(interval.end());

            let mut run_start: Option<PartitionChunkOffset> = None;
            let mut run_len = 0u64;
            for chunk_offset in start..=end {
                let partition_chunk_offset = PartitionChunkOffset::from(chunk_offset);
                visited.push(partition_chunk_offset);

                let pending = self.pending_writes.read().unwrap();
                let pending_chunk = pending
                    .get(&partition_chunk_offset)
                    .map(|(bytes, chunk_type)| (bytes.clone(), *chunk_type));
                drop(pending);

                match (pending_chunk, interval_chunk_type) {
                    (Some(chunk_data), _) => {
                        if run_len > 0 {
                            disk_runs.push((
                                run_start.take().expect("run length has a start"),
                                run_len,
                                interval_chunk_type,
                            ));
                            run_len = 0;
                        }
                        chunk_map.insert(partition_chunk_offset, chunk_data);
                    }
                    (None, ChunkType::Uninitialized) => {
                        if run_len > 0 {
                            disk_runs.push((
                                run_start.take().expect("run length has a start"),
                                run_len,
                                interval_chunk_type,
                            ));
                            run_len = 0;
                        }
                    }
                    (None, _) => {
                        let crosses_file = run_start.is_some_and(|start| {
                            !self.same_chunks_file(start, partition_chunk_offset)
                        });
                        if crosses_file && run_len > 0 {
                            disk_runs.push((
                                run_start.take().expect("run length has a start"),
                                run_len,
                                interval_chunk_type,
                            ));
                            run_len = 0;
                        }
                        if run_len == 0 {
                            run_start = Some(partition_chunk_offset);
                        }
                        run_len += 1;
                    }
                }
            }
            if run_len > 0 {
                disk_runs.push((
                    run_start.take().expect("run length has a start"),
                    run_len,
                    interval_chunk_type,
                ));
            }
        }

        self.read_disk_runs(&disk_runs, &mut chunk_map, recall)?;

        // A chunk that landed in pending while the disk read held the file
        // lock must win over the bytes just read.
        let pending = self.pending_writes.read().unwrap();
        for offset in visited {
            if let Some((bytes, chunk_type)) = pending.get(&offset) {
                chunk_map.insert(offset, (bytes.clone(), *chunk_type));
            }
        }
        drop(pending);

        if recall && self.config.node_config.storage.drop_recall_page_cache {
            self.drop_recall_page_cache(&disk_runs);
        }

        Ok(chunk_map)
    }

    /// True when both offsets live in the same `chunks.dat`.
    fn same_chunks_file(&self, left: PartitionChunkOffset, right: PartitionChunkOffset) -> bool {
        let Ok((left_span, _)) = self.submodules.get_key_value_at_point(left) else {
            return false;
        };
        let Ok((right_span, _)) = self.submodules.get_key_value_at_point(right) else {
            return false;
        };
        left_span.start() == right_span.start()
    }

    /// Read every planned run. A mining recall dups `chunks.dat` and starts
    /// its `pread`s without waiting for inflight writes. Any other read takes
    /// the file lock, which waits until those writes return, then issues one
    /// `pread`.
    fn read_disk_runs(
        &self,
        runs: &[(PartitionChunkOffset, u64, ChunkType)],
        chunk_map: &mut ChunkMap,
        recall: bool,
    ) -> eyre::Result<()> {
        let chunk_size = self.config.consensus.chunk_size;
        let chunk_len = chunk_size as usize;
        let max_chunks = self.config.consensus.num_chunks_in_recall_range.max(1);
        let mut index = 0;
        while index < runs.len() {
            let (origin_interval, submodule) = self
                .submodules
                .get_key_value_at_point(runs[index].0)
                .expect("disk run starts at a mapped offset");
            let origin = *origin_interval.start();
            let file_arc = Arc::clone(&submodule.file);
            if recall {
                let file = {
                    let guard = file_arc
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard
                        .try_clone()
                        .wrap_err("recall could not dup chunks.dat")?
                };
                while index < runs.len() {
                    let (start, len, chunk_type) = runs[index];
                    let (run_interval, _) = self
                        .submodules
                        .get_key_value_at_point(start)
                        .expect("disk run starts at a mapped offset");
                    if *run_interval.start() != origin {
                        break;
                    }
                    read_recall_run(
                        &file,
                        start,
                        len,
                        chunk_type,
                        run_interval.start(),
                        chunk_size,
                        chunk_len,
                        max_chunks,
                        chunk_map,
                    )?;
                    index += 1;
                }
            } else {
                let file = self.disk.lock_chunks(&file_arc);
                while index < runs.len() {
                    let (start, len, chunk_type) = runs[index];
                    let (run_interval, _) = self
                        .submodules
                        .get_key_value_at_point(start)
                        .expect("disk run starts at a mapped offset");
                    if *run_interval.start() != origin {
                        break;
                    }
                    let mut done = 0u64;
                    while done < len {
                        let n = (len - done).min(max_chunks);
                        let piece = PartitionChunkOffset(start.0 + done as u32);
                        let file_offset = *(piece - run_interval.start()) as u64 * chunk_size;
                        let mut buf = vec![0_u8; n as usize * chunk_len];
                        file.read_exact_at(&mut buf, file_offset)
                            .wrap_err_with(|| format!("recall read at offset {piece} count {n}"))?;
                        #[cfg(test)]
                        self.disk.source_preads.fetch_add(1, Ordering::SeqCst);
                        for step in 0..n {
                            let at = step as usize * chunk_len;
                            let off = PartitionChunkOffset(piece.0 + step as u32);
                            chunk_map.insert(off, (buf[at..at + chunk_len].to_vec(), chunk_type));
                        }
                        done += n;
                    }
                    index += 1;
                }
            }
        }
        Ok(())
    }

    /// Drop clean pages for recall runs that no longer hold a packed write.
    ///
    /// Called while the recall hold is still live, so the lane starts no new
    /// write. Linux `DONTNEED` starts writeback of dirty pages in the range
    /// before it drops clean pages. Offsets stay in `pending_writes` until
    /// after `sync_all`, so a run that overlaps pending is left cached. The
    /// file mutex is try-locked: a flush that already holds it waits on this
    /// recall, and waiting here would deadlock.
    fn drop_recall_page_cache(&self, runs: &[(PartitionChunkOffset, u64, ChunkType)]) {
        let chunk_size = self.config.consensus.chunk_size;
        if chunk_size == 0 {
            return;
        }

        let mut targets: Vec<(Arc<Mutex<File>>, u64, u64)> = Vec::new();
        {
            let pending = self.pending_writes.read().unwrap();
            for &(start, len, _) in runs {
                if len == 0 {
                    continue;
                }
                let Some(last) = u32::try_from(len - 1)
                    .ok()
                    .and_then(|count| start.0.checked_add(count))
                else {
                    continue;
                };
                if pending
                    .chunks
                    .range(start..=PartitionChunkOffset(last))
                    .next()
                    .is_some()
                {
                    continue;
                }
                let Ok((interval, submodule)) = self.submodules.get_key_value_at_point(start)
                else {
                    continue;
                };
                if last > interval.end().0 {
                    continue;
                }
                let file_offset = u64::from(*(start - interval.start())) * chunk_size;
                let Some(byte_len) = len.checked_mul(chunk_size) else {
                    continue;
                };
                targets.push((Arc::clone(&submodule.file), file_offset, byte_len));
            }
        }

        for (file_arc, offset, len) in targets {
            let dup = {
                let file = match file_arc.try_lock() {
                    Ok(guard) => guard,
                    Err(TryLockError::WouldBlock) => continue,
                    Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                };
                match file.try_clone() {
                    Ok(dup) => dup,
                    Err(error) => {
                        warn!("recall page cache drop could not dup chunks.dat: {error}");
                        continue;
                    }
                }
            };
            self.advise_dontneed(&dup, offset, len);
        }
    }

    /// `posix_fadvise(DONTNEED)` on one copied recall range. A failed advise
    /// leaves the recall result intact: the bytes are already in memory.
    fn advise_dontneed(&self, file: &File, offset: u64, len: u64) {
        if len == 0 {
            return;
        }
        let (Ok(offset), Ok(len)) = (i64::try_from(offset), i64::try_from(len)) else {
            warn!("recall page cache drop skipped a range that does not fit off_t");
            return;
        };
        // Safety: `file` owns a live descriptor. The advice does not change bytes.
        let rc = unsafe {
            libc::posix_fadvise(
                file.as_raw_fd(),
                offset as libc::off_t,
                len as libc::off_t,
                libc::POSIX_FADV_DONTNEED,
            )
        };
        if rc != 0 {
            warn!("recall page cache drop failed at file offset {offset} length {len}: error {rc}");
            return;
        }
        #[cfg(test)]
        self.disk.recall_cache_drops.fetch_add(1, Ordering::SeqCst);
    }

    /// Reads a single chunk from its physical storage location
    ///
    /// Given a logical chunk offset, this function:
    /// 1. Locates the appropriate submodule containing the chunk
    /// 2. Calculates the physical file offset
    /// 3. Reads the chunk data into a buffer
    ///
    /// Returns the chunk bytes or an error if read fails
    fn read_chunk_internal(&self, chunk_offset: PartitionChunkOffset) -> eyre::Result<ChunkBytes> {
        // Find submodule containing this chunk
        let (interval, submodule) = self
            .submodules
            .get_key_value_at_point(chunk_offset)
            .unwrap();

        // Calculate file offset and prepare buffer
        let chunk_size = self.config.consensus.chunk_size;
        let file_offset = *(chunk_offset - interval.start()) as u64 * chunk_size;
        let mut buf = vec![0_u8; chunk_size as usize];

        // Positional read. A seek on the shared handle would move the cursor
        // seen by the next reader of this file.
        let file_arc = Arc::clone(&submodule.file);
        let file = self.disk.lock_chunks(&file_arc);
        file.read_exact_at(&mut buf, file_offset)
            .wrap_err_with(|| format!("chunk read at offset {chunk_offset}"))?;

        Ok(buf)
    }

    /// Gets all chunk intervals in a given storage state, merging adjacent ranges
    ///
    /// Collects all intervals matching the requested state and combines them when:
    /// - Intervals are touching (e.g., 0-5 and 6-10)
    /// - Intervals overlap (e.g., 0-5 and 3-8)
    ///
    /// Returns a NoditSet containing the merged intervals for efficient range operations
    pub fn get_intervals(&self, chunk_type: ChunkType) -> Vec<Interval<PartitionChunkOffset>> {
        let intervals = self.intervals.read().unwrap();
        let mut set = NoditSet::new();
        for (interval, ct) in intervals.iter() {
            if *ct == chunk_type {
                let _ = set.insert_merge_touching_or_overlapping(*interval);
            }
        }
        drop(intervals);

        // Also loop though pending write for matching chunks
        let pending = self
            .pending_writes
            .read()
            .expect("to be able to read pending writes data");

        match chunk_type {
            ChunkType::Entropy => {
                // First, add any pending entropy chunks to the set
                pending
                    .iter()
                    .filter(|(_, (_, chunk_type))| *chunk_type == ChunkType::Entropy)
                    .for_each(|(offset, _)| {
                        let interval = partition_chunk_offset_ii!(*offset, *offset);
                        let _ = set.insert_merge_touching_or_overlapping(interval);
                    });

                // Then, remove any entropy offsets that have pending data chunks
                pending
                    .iter()
                    .filter(|(_, (_, chunk_type))| *chunk_type == ChunkType::Data)
                    .for_each(|(offset, _)| {
                        let point_interval = ii(*offset, *offset);
                        let _ = set.cut(point_interval);
                    });
            }
            ChunkType::Data => {
                pending
                    .iter()
                    .filter(|(_, (_, pending_chunk_type))| *pending_chunk_type == ChunkType::Data)
                    .for_each(|(offset, _)| {
                        let interval = partition_chunk_offset_ii!(*offset, *offset);
                        let _ = set.insert_merge_touching_or_overlapping(interval);
                    });
            }
            ChunkType::Uninitialized => {
                for offset in pending.keys() {
                    let point_interval = ii(*offset, *offset);
                    let _ = set.cut(point_interval);
                }
            }
            ChunkType::Interrupted => {
                // Do nothing
            }
        }

        // NoditSet is a BTreeMap underneath, meaning collecting them into a vec
        // is done in ascending key order.
        set.into_iter().collect::<Vec<_>>()
    }

    /// Queues chunk data for later disk write. Chunks are batched for efficiency
    /// and written during periodic sync operations.
    pub fn write_chunk(
        &self,
        chunk_offset: PartitionChunkOffset,
        bytes: Vec<u8>,
        chunk_type: ChunkType,
    ) -> bool {
        let mut pending = self.pending_writes.write().unwrap();
        // Checked under the same lock as the insert so a pause taken while a
        // writer is between its own pre-check and this point still wins.
        if chunk_type == ChunkType::Data && self.data_writes_paused() {
            return false;
        }
        let priority = match chunk_type {
            ChunkType::Entropy => disk_lane::WritePriority::Packing,
            _ => disk_lane::WritePriority::Ingress,
        };
        pending.insert(chunk_offset, (bytes, chunk_type));
        pending.priorities.insert(chunk_offset, priority);
        pending.queued_at.insert(chunk_offset, Instant::now());
        *self.last_pending_write.write().unwrap() = Instant::now();
        drop(pending);
        // The lane writes the next window when it is woken.
        self.disk.notify();
        true
    }

    /// Test utility function
    pub fn print_pending_writes(&self) {
        let pending = self.pending_writes.read().unwrap();
        debug!("pending_writes: {:?}", pending);
    }

    /// Drops any queued (not-yet-synced) chunk writes whose partition offset
    /// falls in the inclusive range `[start, end]`.
    ///
    /// Used by network-partition rollback: after orphaned offsets are cleared
    /// and re-marked `Uninitialized`, a stale pending write left in the queue
    /// would be replayed by `sync_pending_chunks` and flip the interval back to
    /// `Data` with a now-empty offset index.
    pub fn drop_pending_writes_in_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) {
        for (interval, submodule) in self.submodules.iter() {
            if *interval.end() >= *start && *interval.start() <= *end {
                submodule.index_drain.wait_idle_in_range(start, end);
            }
        }
        {
            let mut pending = self.pending_writes.write().unwrap();
            pending.retain(|offset, _| *offset < start || *offset > end);
            pending
                .occupancy
                .retain(|offset, _| *offset < start || *offset > end);
            pending
                .priorities
                .retain(|offset, _| *offset < start || *offset > end);
            pending
                .queued_at
                .retain(|offset, _| *offset < start || *offset > end);
        }
        self.cancel_sweep_range(start, end);
    }

    /// Clears the per-offset tx-path and data-path offset-index entries in the
    /// inclusive partition range `[start, end]`, so orphaned chunk offsets no
    /// longer resolve to a tx/data path.
    ///
    /// Used by network-partition rollback alongside
    /// [`Self::clear_data_root_infos_in_range`]. Walks each submodule covering
    /// the range once, clearing its slice of the range in a single write
    /// transaction.
    pub fn clear_offset_index_in_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<()> {
        let range_start = *start;
        let range_end = *end;
        let mut cursor = range_start;
        while cursor <= range_end {
            let (interval, submodule) =
                self.get_submodule_for_offset(PartitionChunkOffset::from(cursor))?;
            let submodule_end = *interval.end();
            let slice_end = submodule_end.min(range_end);
            submodule.db.update_eyre(|tx| {
                for offset in cursor..=slice_end {
                    let part_offset = PartitionChunkOffset::from(offset);
                    // Delete the key entirely so gap scans see a real hole.
                    // Writing `{None,None}` placeholders left "present" keys that
                    // the density check treated as indexed, hiding the gap from heal.
                    del_path_hashes_by_offset(tx, part_offset)?;
                }
                Ok(())
            })?;
            // Advance to the next submodule covering the range.
            if submodule_end >= range_end {
                break;
            }
            cursor = submodule_end + 1;
        }
        Ok(())
    }

    /// Removes the `DataRootInfo` placements whose on-disk extent
    /// `[start_offset, start_offset + ceil(data_size / chunk_size) - 1]`
    /// intersects the inclusive range `[start, end]`, for each of `data_roots`,
    /// leaving placements that lie entirely outside the range intact.
    ///
    /// Used by network-partition rollback to un-index only the *orphaned*
    /// placement of a data_root. A data_root shared with a still-canonical
    /// block (a Submit→Publish promotion, or a duplicate upload of identical
    /// data) keeps its canonical placement, whose extent sits outside the
    /// orphaned range. Walks each submodule covering the range once, so it
    /// does not depend on the per-chunk offset.
    pub fn clear_data_root_infos_in_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
        data_roots: &[H256],
    ) -> eyre::Result<()> {
        let range_start = *start;
        let range_end = *end;
        let mut cursor = range_start;
        while cursor <= range_end {
            let (interval, submodule) =
                self.get_submodule_for_offset(PartitionChunkOffset::from(cursor))?;
            let submodule_end = *interval.end();
            submodule.db.update_eyre(|tx| {
                for data_root in data_roots {
                    let Some(infos) = get_data_root_infos_for_data_root(tx, *data_root)? else {
                        continue;
                    };
                    let remaining: Vec<_> = infos
                        .0
                        .into_iter()
                        .filter(|di| {
                            // Keep the placement iff its full on-disk extent
                            // [start_offset, start_offset + ceil(data_size / chunk_size) - 1]
                            // does NOT intersect the orphaned range. Matching on
                            // start_offset alone would leave a stale entry for a
                            // placement that begins before the range (e.g. a
                            // cross-partition placement in a higher module, whose
                            // start_offset is negative) but extends into it.
                            let extent_start = di.start_offset.0;
                            let extent_chunks =
                                di.data_size.div_ceil(self.config.consensus.chunk_size) as i32;
                            let extent_end = (extent_start + extent_chunks).saturating_sub(1);
                            extent_end < range_start as i32 || extent_start > range_end as i32
                        })
                        .collect();
                    set_data_root_infos_for_data_root(tx, *data_root, DataRootInfos(remaining))?;
                }
                Ok(())
            })?;
            // Advance to the next submodule covering the range.
            if submodule_end >= range_end {
                break;
            }
            cursor = submodule_end + 1;
        }
        Ok(())
    }

    /// Indexes transaction data by mapping chunks to transaction paths across storage submodules.
    /// Stores three mappings: tx path hashes -> tx_path, chunk offsets -> tx paths, and data roots -> start offset.
    /// Updates all overlapping submodules within the given chunk range.
    ///
    /// Also records, in the same per-submodule transaction, a
    /// `PendingBodyMigrationsByOffset` row saying this tx's chunk bodies are still
    /// owed to that submodule (see [`Self::settle_pending_body_migration`]).
    /// `block_height` is the canonical block whose migration is indexing the tx.
    ///
    /// # Errors
    /// Returns error if chunk range doesn't overlap with storage module range.
    pub fn index_transaction_data(
        &self,
        data_tx: &DataTransactionHeader,
        tx_path: &TxPath,
        chunk_range: LedgerChunkRange,
        block_height: u64,
    ) -> eyre::Result<()> {
        let tx_path_hash = H256::from(hash_sha256(tx_path).unwrap());
        let (partition_overlap, start_offset) = self.partition_overlap_for(chunk_range)?;

        for (interval, submodule) in self.submodules.overlapping(partition_overlap) {
            submodule.db.update_eyre(|tx| -> eyre::Result<()> {
                // Because each submodule index receives a copy of the path, we need to clone it
                add_full_tx_path(tx, tx_path_hash, tx_path.clone())?;
                // Record the (data_root, prefix_hash) this tx_path leaf folds from, so the
                // real data_root can be recovered on read (the leaf now stores the folded
                // hash_all_sha256([data_root, prefix_hash]), not the raw data_root) and the
                // proof leaf re-verified against it.
                add_tx_leaf_binding(
                    tx,
                    tx_path_hash,
                    &TxLeafBinding {
                        data_root: data_tx.data_root,
                        prefix_hash: data_tx.prefix_hash,
                    },
                )?;
                if let Some(range) = interval.intersection(&partition_overlap) {
                    // One cursor for the intersecting offsets. A tip range appends;
                    // an overlap keeps any data path already stored on those keys.
                    add_tx_path_hash_to_offset_range(
                        tx,
                        range.start(),
                        range.end(),
                        Some(tx_path_hash),
                    )?;
                    // Add the DataRootInfo to the Infos for this data_root
                    let info = DataRootInfo {
                        start_offset,
                        data_size: data_tx.data_size,
                    };
                    add_data_root_info(tx, data_tx.data_root, &info)?;
                    // Same txn as the index, so "indexed" can never be true while
                    // "bodies owed" is unrecorded. The body worker drains and
                    // deletes this row; `range.start()` is the key it uses,
                    // clipped to this submodule (the DataRootInfo above keeps the
                    // unclipped tx start).
                    add_pending_body_migration(
                        tx,
                        range.start(),
                        &PendingBodyMigration {
                            data_root: data_tx.data_root,
                            data_size: data_tx.data_size,
                            start_offset,
                            block_height,
                            attempts: 0,
                        },
                    )?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    /// Partition-relative slice of `chunk_range` that lands in this module, plus
    /// the unclipped partition-relative start of the whole tx (negative when the
    /// tx began before this module's ledger range).
    ///
    /// The `PendingBodyMigrationsByOffset` key for each submodule is the start of
    /// this slice clipped to that submodule's interval; the body worker recovers
    /// the same range from the row's `start_offset` + `data_size`.
    ///
    /// # Errors
    /// Returns error if `chunk_range` doesn't overlap the storage module range.
    fn partition_overlap_for(
        &self,
        chunk_range: LedgerChunkRange,
    ) -> eyre::Result<(PartitionChunkRange, RelativeChunkOffset)> {
        let storage_range = self.get_storage_module_ledger_offsets()?;
        let overlap = storage_range
            .intersection(&chunk_range)
            .ok_or_else(|| eyre::eyre!("chunk_range does not overlap storage module range"))?;
        // Compute the partition relative overlapping chunk range
        let partition_overlap = self.make_range_partition_relative(overlap)?;
        // Compute the Partition relative start offset
        let start_offset =
            RelativeChunkOffset::from(self.make_offset_partition_relative(chunk_range.start())?);
        Ok((partition_overlap, start_offset))
    }

    /// Outstanding body-migration jobs grouped by submodule, each group in
    /// ascending offset order, paired with that submodule's partition interval
    /// so the worker can clip each job's range to the disk that owns it. Every
    /// submodule is its own IO domain, so groups may be drained in parallel.
    pub fn pending_body_migration_batches(
        &self,
    ) -> eyre::Result<
        Vec<(
            Interval<PartitionChunkOffset>,
            Vec<(PartitionChunkOffset, PendingBodyMigration)>,
        )>,
    > {
        let mut batches = Vec::with_capacity(self.submodules.len());
        for (interval, submodule) in self.submodules.iter() {
            let rows = submodule
                .db
                .view_eyre(|tx| pending_body_migrations_from(tx, None))?;
            batches.push((*interval, rows));
        }
        Ok(batches)
    }

    /// Every outstanding body-migration job across this module's submodules, in
    /// ascending partition-offset order. Empty on a caught-up node.
    pub fn pending_body_migrations(
        &self,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, PendingBodyMigration)>> {
        Ok(self
            .pending_body_migration_batches()?
            .into_iter()
            .flat_map(|(_, rows)| rows)
            .collect())
    }

    /// Delete the job at `key` once every offset it covers is durable (or the
    /// job is being retired). Returns whether a row was removed.
    ///
    /// Only a row whose `data_root` **and** `block_height` match is removed: a
    /// late settle from an orphaned migration must not delete the replacement
    /// at the same offset. Refuses while data writes are paused so a mid-pass
    /// rollback cannot look like a successful drain.
    pub fn settle_pending_body_migration(
        &self,
        key: PartitionChunkOffset,
        data_root: DataRoot,
        block_height: u64,
    ) -> eyre::Result<bool> {
        if self.data_writes_paused() {
            return Ok(false);
        }
        let (_interval, submodule) = self
            .submodules
            .get_key_value_at_point(key)
            .map_err(|_| eyre::eyre!("No submodule found for Partition Offset {:?}", key))?;
        submodule
            .db
            .update_eyre(|tx| match get_pending_body_migration(tx, key)? {
                Some(job) if job.data_root == data_root && job.block_height == block_height => {
                    del_pending_body_migration(tx, key)
                }
                _ => Ok(false),
            })
    }

    /// Delete outstanding body-migration jobs keyed in `[start, end]` (partition
    /// relative) across the submodules covering that range. Network-partition
    /// recovery calls this alongside `clear_data_root_infos_in_range`: the
    /// offsets are being unassigned, so the bodies they owed are moot and the
    /// worker must not write them. Returns the number of rows removed.
    pub fn purge_pending_body_migrations_in_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<usize> {
        let mut removed = 0;
        for (_interval, submodule) in self.submodules.overlapping(ii(start, end)) {
            removed += submodule
                .db
                .update_eyre(|tx| del_pending_body_migrations_in_range(tx, start, end))?;
        }
        Ok(removed)
    }

    /// Record one drain pass that attempted the job at `key` and made no
    /// progress. Returns the new attempt count, or `None` if the row is gone,
    /// belongs to another job, or data writes are paused.
    pub fn bump_pending_body_migration_attempts(
        &self,
        key: PartitionChunkOffset,
        data_root: DataRoot,
        block_height: u64,
    ) -> eyre::Result<Option<u32>> {
        if self.data_writes_paused() {
            return Ok(None);
        }
        let (_interval, submodule) = self
            .submodules
            .get_key_value_at_point(key)
            .map_err(|_| eyre::eyre!("No submodule found for Partition Offset {:?}", key))?;
        submodule
            .db
            .update_eyre(|tx| match get_pending_body_migration(tx, key)? {
                Some(mut job) if job.data_root == data_root && job.block_height == block_height => {
                    job.attempts = job.attempts.saturating_add(1);
                    add_pending_body_migration(tx, key, &job)?;
                    Ok(Some(job.attempts))
                }
                _ => Ok(None),
            })
    }

    pub fn get_writeable_offsets(
        &self,
        chunk: &UnpackedChunk,
    ) -> eyre::Result<Vec<PartitionChunkOffset>> {
        let Some(offsets) =
            self.partition_offsets_for_data_root_chunk(chunk.data_root, chunk.tx_offset)?
        else {
            debug!("Chunks data_root not found in storage module");
            return Ok(Vec::new());
        };

        let intervals = self.intervals.read().unwrap();
        let entropy_offsets: Vec<_> = offsets
            .iter()
            .copied()
            .filter(|partition_offset| {
                intervals
                    .get_at_point(*partition_offset)
                    .is_some_and(|s| *s == ChunkType::Entropy)
            })
            .collect();
        drop(intervals);
        let pending = self.pending_writes.read().unwrap();
        Ok(entropy_offsets
            .into_iter()
            .filter(|offset| !pending.occupancy.contains_key(offset))
            .collect())
    }

    /// True when this module has an index op in flight for any placement of `chunk`.
    pub fn has_in_flight_index_for(&self, chunk: &UnpackedChunk) -> bool {
        let Ok(Some(offsets)) =
            self.partition_offsets_for_data_root_chunk(chunk.data_root, chunk.tx_offset)
        else {
            return false;
        };
        let pending = self.pending_writes.read().unwrap();
        offsets
            .iter()
            .any(|offset| pending.occupancy.contains_key(offset))
    }

    /// Waits until the index ACK has inserted `ChunkType::Data` into the pending map.
    /// When the disk-lane thread is running, that thread locks `chunks.dat`.
    /// Otherwise this call drives the sweep itself.
    pub fn write_data_chunk(&self, chunk: &UnpackedChunk) -> Result<(), WriteDataChunkError> {
        match self.enqueue_unpacked(chunk, disk_lane::WritePriority::Ingress, true)? {
            Some(group) if self.disk.lane_running() => self.wait_group(group),
            Some(group) => self.drive_group(group),
            None => Ok(()),
        }
    }

    /// Same wait as [`Self::write_data_chunk`]. When the disk-lane thread is
    /// running, this task waits on a oneshot and does not take a blocking
    /// thread. Otherwise a multi-thread runtime moves the worker aside for
    /// the wait, and a current-thread runtime drives the wait here.
    pub async fn write_data_chunk_queued(
        &self,
        chunk: &UnpackedChunk,
    ) -> Result<(), WriteDataChunkError> {
        if self.disk.lane_running() {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let Some(group) = self.enqueue_unpacked_with(
                chunk,
                disk_lane::WritePriority::Ingress,
                true,
                Some(tx),
            )?
            else {
                return Ok(());
            };
            return self.await_swept_group(group, rx).await;
        }
        let Some(group) = self.enqueue_unpacked(chunk, disk_lane::WritePriority::Ingress, true)?
        else {
            return Ok(());
        };
        // Let the runtime poll other tasks once before this thread blocks on
        // the index ACK. The drain itself is a std thread.
        tokio::task::yield_now().await;
        let multi_thread = tokio::runtime::Handle::try_current()
            .ok()
            .is_some_and(|handle| {
                handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
            });
        if multi_thread {
            tokio::task::block_in_place(|| self.drive_group(group))
        } else {
            self.drive_group(group)
        }
    }

    fn injected_entropy_read_error(&self) -> Option<WriteDataChunkError> {
        #[cfg(test)]
        {
            let at = self.fail_entropy_read_nth.load(Ordering::SeqCst);
            if at != 0 {
                let n = self.entropy_read_seq.fetch_add(1, Ordering::SeqCst) + 1;
                if n == at {
                    return Some(WriteDataChunkError::Other(eyre::eyre!(
                        "injected entropy read failure"
                    )));
                }
            }
        }
        None
    }

    /// Aggregates DataRootInfo entries for a data_root across all submodules.
    ///
    /// Returns a combined DataRootInfoList containing all start_offsets and data_sizes
    /// for this data_root found in any submodule's database.
    ///
    /// # Returns
    /// * `Ok(DataRootInfoList)` - Combined DataRootInfo List (may be empty if not found)
    /// * `Err` - If any database read fails
    pub fn collect_data_root_infos(&self, data_root: DataRoot) -> eyre::Result<DataRootInfos> {
        let mut data_root_info_list = DataRootInfos::default();
        for (_, submodule) in self.submodules.iter() {
            index_read_metrics::note("data_root");
            if let Ok(Some(submodule_index)) = submodule
                .db
                .view(|tx| get_data_root_infos_for_data_root(tx, data_root))?
            {
                data_root_info_list.0.extend(submodule_index.0);
            }
        }
        Ok(data_root_info_list)
    }

    /// Inclusive partition offsets. One entry per offset: `Some` is the unpacked
    /// body, `None` is a hole. The disk read is the same planner as
    /// [`Self::read_chunks`]: one `pread` per contiguous durable run, split on a
    /// pending write, an uninitialized offset, or a `chunks.dat` boundary.
    /// A range longer than one entropy sweep is rejected so the file lock stays
    /// inside that cap.
    pub fn read_durable_bodies(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> Result<Vec<Option<UnpackedChunk>>> {
        ensure!(end >= start, "durable body range ends before it starts");
        let count = u64::from(end.0 - start.0) + 1;
        let limit = self.sweep_chunk_limit();
        ensure!(
            count <= limit,
            "durable body range of {count} chunks exceeds one sweep of {limit}"
        );
        let loaded = self.read_chunks(partition_chunk_offset_ii!(start, end))?;
        let chunk_size = usize::try_from(self.config.consensus.chunk_size)
            .wrap_err("configured chunk size does not fit usize")?;
        // Entropy and pending offsets have no proof row to read. Skip the
        // view entirely when the range has no durable data.
        let needs_index = (start.0..=end.0).any(|raw| {
            let offset = PartitionChunkOffset::from(raw);
            !self.is_data_write_pending_at(offset)
                && loaded
                    .get(&offset)
                    .is_some_and(|(_, kind)| *kind == ChunkType::Data)
        });
        let metas = if needs_index {
            index_read_metrics::with_caller(MIGRATION, || self.chunk_index_metas(start, end))?
        } else {
            BTreeMap::new()
        };
        let mut bodies = Vec::with_capacity(count as usize);
        for raw in start.0..=end.0 {
            let offset = PartitionChunkOffset::from(raw);
            bodies.push(self.unpack_durable_body(
                offset,
                &loaded,
                chunk_size,
                metas.get(&offset),
            )?);
        }
        Ok(bodies)
    }

    fn sweep_chunk_limit(&self) -> u64 {
        let chunk = self.config.consensus.chunk_size.max(1);
        self.config
            .node_config
            .storage
            .entropy_sweep_max_bytes
            .max(chunk)
            / chunk
    }

    /// `None` when the offset is not durable transaction data. A pending write
    /// is a hole here: the bytes are not on disk yet, and the planner already
    /// split the `pread` around them.
    fn unpack_durable_body(
        &self,
        offset: PartitionChunkOffset,
        loaded: &BTreeMap<PartitionChunkOffset, (ChunkBytes, ChunkType)>,
        chunk_size: usize,
        meta: Option<&(DataRoot, u64, Base64, TxChunkOffset)>,
    ) -> Result<Option<UnpackedChunk>> {
        if self.is_data_write_pending_at(offset) {
            return Ok(None);
        }
        let Some((bytes, chunk_type)) = loaded.get(&offset) else {
            return Ok(None);
        };
        if *chunk_type != ChunkType::Data {
            return Ok(None);
        }
        let Some((data_root, data_size, data_path, tx_offset)) = meta.cloned() else {
            return Ok(None);
        };
        let packed = PackedChunk {
            data_root,
            data_size,
            data_path,
            bytes: Base64::from(bytes.clone()),
            partition_offset: offset,
            tx_offset,
            packing_address: self.config.node_config.miner_address(),
            partition_hash: self
                .partition_hash()
                .ok_or_eyre("storage module has no partition")?,
        };
        Ok(Some(unpack(
            &packed,
            self.config.consensus.entropy_packing_iterations,
            chunk_size,
            self.config.consensus.chain_id,
        )))
    }

    /// One `view` per submodule covering `[start, end]`. Shared tx paths, data
    /// roots, and data paths are read once inside that view.
    fn chunk_index_metas(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> Result<BTreeMap<PartitionChunkOffset, (DataRoot, u64, Base64, TxChunkOffset)>> {
        let chunk_size = self.config.consensus.chunk_size;
        let slices =
            self.map_submodule_slices(start, end, |submodule, slice_start, slice_end| {
                submodule
                    .db
                    .view_eyre(|tx| metas_in_tx(tx, slice_start, slice_end, chunk_size))
            })?;
        let mut out = BTreeMap::new();
        for slice in slices {
            out.extend(slice);
        }
        Ok(out)
    }

    /// Tx path and data path for every indexed offset in the inclusive range.
    ///
    /// One `view` per submodule. Offsets with no row are absent.
    pub fn read_tx_data_paths(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<BTreeMap<PartitionChunkOffset, (Option<TxPath>, Option<ChunkDataPath>)>> {
        let slices = index_read_metrics::with_caller(RECALL, || {
            self.map_submodule_slices(start, end, |submodule, slice_start, slice_end| {
                submodule
                    .db
                    .view_eyre(|tx| paths_in_tx(tx, slice_start, slice_end))
            })
        })?;
        let mut out = BTreeMap::new();
        for slice in slices {
            out.extend(slice);
        }
        Ok(out)
    }

    /// Calls `map_slice` once per submodule that covers `[start, end]`.
    /// The closure opens that slice's view.
    fn map_submodule_slices<T>(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
        mut map_slice: impl FnMut(
            &StorageSubmodule,
            PartitionChunkOffset,
            PartitionChunkOffset,
        ) -> eyre::Result<T>,
    ) -> eyre::Result<Vec<T>> {
        let mut out = Vec::new();
        if end < start {
            return Ok(out);
        }
        let mut cursor = start.0;
        let range_end = end.0;
        while cursor <= range_end {
            let (interval, submodule) =
                self.get_submodule_for_offset(PartitionChunkOffset::from(cursor))?;
            let slice_end = (*interval.end()).min(range_end);
            let slice = map_slice(
                submodule,
                PartitionChunkOffset::from(cursor),
                PartitionChunkOffset::from(slice_end),
            )?;
            #[cfg(test)]
            self.index_views.fetch_add(1, Ordering::SeqCst);
            index_read_metrics::note_view(u64::from(slice_end - cursor) + 1);
            out.push(slice);
            if slice_end >= range_end {
                break;
            }
            cursor = slice_end + 1;
        }
        Ok(out)
    }

    pub fn generate_full_chunk_ledger_offset(
        &self,
        ledger_offset: LedgerChunkOffset,
    ) -> Result<Option<PackedChunk>> {
        let range = self.get_storage_module_ledger_offsets()?;
        let partition_offset = PartitionChunkOffset::from(*(ledger_offset - range.start()));

        self.generate_full_chunk(partition_offset)
    }

    /// Constructs a Chunk struct for the given ledger offset
    ///
    /// This function:
    /// 1. Retrieves and validates tx and data paths
    /// 2. Extracts data_root and size from merkle proofs
    /// 3. Calculates chunk position within its parent transaction
    /// 4. Returns None if any step fails or chunk not found
    ///
    /// Note: Handles cases where data spans partition boundaries by supporting
    /// negative offsets in the calculation of chunk position
    /// this is why the input offset is a LedgerOffset and not a PartitionOffset
    pub fn generate_full_chunk(
        &self,
        partition_offset: PartitionChunkOffset,
    ) -> Result<Option<PackedChunk>> {
        Ok(self
            .generate_full_chunks(partition_offset, partition_offset)?
            .remove(&partition_offset))
    }

    /// Packed chunks for the inclusive partition span `[start, end]`.
    ///
    /// One index view loads every proof. One `read_chunks` loads the bodies.
    /// An offset with no stored chunk is absent. A one-offset span still errors
    /// when the index names a chunk the disk does not hold.
    pub fn generate_full_chunks(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> Result<BTreeMap<PartitionChunkOffset, PackedChunk>> {
        let metas = self.chunk_index_metas(start, end)?;
        if metas.is_empty() {
            return Ok(BTreeMap::new());
        }

        let mut bodies = self.read_chunks(partition_chunk_offset_ii!(start, end))?;
        let packing_address = self.config.node_config.miner_address();
        let partition_hash = self.partition_hash().unwrap();
        let mut out = BTreeMap::new();
        for (offset, (data_root, data_size, data_path, tx_offset)) in metas {
            let Some((bytes, _)) = bodies.remove(&offset) else {
                if start == end {
                    return Err(eyre!("Could not find chunk bytes on disk"));
                }
                continue;
            };
            out.insert(
                offset,
                PackedChunk {
                    data_root,
                    data_size,
                    data_path,
                    bytes: Base64::from(bytes),
                    partition_offset: offset,
                    tx_offset,
                    packing_address,
                    partition_hash,
                },
            );
        }
        Ok(out)
    }

    /// Returns chunk metadata (data_root and data_path) without reading chunk bytes.
    ///
    /// This is more efficient than `generate_full_chunk` when only the merkle proof
    /// metadata is needed (e.g., for path validation).
    pub fn get_chunk_metadata(
        &self,
        partition_offset: PartitionChunkOffset,
    ) -> Result<Option<(DataRoot, Base64)>> {
        index_read_metrics::with_caller(METADATA, || {
            self.get_chunk_metadata_in_caller(partition_offset)
        })
    }

    fn get_chunk_metadata_in_caller(
        &self,
        partition_offset: PartitionChunkOffset,
    ) -> Result<Option<(DataRoot, Base64)>> {
        index_read_metrics::note_view(1);
        self.query_submodule_db_by_offset(partition_offset, |tx| {
            // Recover the real data_root from the stored tx-leaf binding (verified against
            // the tx_path leaf via the (data_root, prefix_hash) fold) plus this offset's
            // data_path hash, from a single offset-index read.
            let Some((data_root, data_path_hash)) =
                recover_tx_path_data_root(tx, partition_offset)?
            else {
                return Ok(None);
            };

            let Some(data_path_hash) = data_path_hash else {
                return Ok(None);
            };
            index_read_metrics::note("data_path");
            let Some(data_path) = get_full_data_path(tx, data_path_hash)? else {
                return Ok(None);
            };

            Ok(Some((data_root, Base64::from(data_path))))
        })
    }

    /// Resolves `(data_root, tx_offset)` for a partition offset from the SM index alone.
    ///
    /// Unlike [`Self::get_chunk_metadata`] / [`Self::generate_full_chunk`], this does **not**
    /// require a written chunk body (`data_path_hash` may be missing). That is the residual
    /// Entropy-hole case after tx migration: `tx_path` + `DataRootInfos` present, body never
    /// written. data_sync uses this to address proof-signer peers with
    /// `GET /chunk/data-root/{ledger}/{data_root}/{tx_offset}` instead of ledger-offset.
    ///
    /// `tx_offset` is derived as `partition_offset - DataRootInfo.start_offset` for the
    /// placement that covers this offset (validated against `data_size`).
    ///
    /// Returns `Ok(None)` when no tx_path is indexed, no covering `DataRootInfo` exists, or
    /// the offset falls outside the funded extent.
    pub fn data_root_and_tx_offset_at(
        &self,
        partition_offset: PartitionChunkOffset,
    ) -> eyre::Result<Option<(DataRoot, TxChunkOffset)>> {
        let chunk_size = self.config.consensus.chunk_size;
        self.query_submodule_db_by_offset(partition_offset, |tx| {
            // data_path_hash may be None — that is the residual-hole case we support.
            let Some((data_root, _data_path_hash)) =
                recover_tx_path_data_root(tx, partition_offset)?
            else {
                return Ok(None);
            };

            let Some(mut data_root_infos) = get_data_root_infos_for_data_root(tx, data_root)?
            else {
                return Ok(None);
            };
            if data_root_infos.0.is_empty() {
                return Ok(None);
            }

            // Same placement selection as generate_full_chunk: last start_offset that is
            // still ≤ partition_offset (handles multi-placement of the same data_root).
            data_root_infos.0.sort_unstable();
            let partition_rel = RelativeChunkOffset::from(partition_offset);
            let index = data_root_infos
                .0
                .partition_point(|info| info.start_offset <= partition_rel)
                .saturating_sub(1);
            if index >= data_root_infos.0.len() {
                return Ok(None);
            }
            let info = &data_root_infos.0[index];
            if info.start_offset > partition_rel {
                return Ok(None);
            }

            // partition_offset = start_offset + tx_offset (write path); invert it.
            // start_offset can be negative when a multi-partition tx straddles modules.
            let tx_offset_i32 = *partition_rel - *info.start_offset;
            if tx_offset_i32 < 0 {
                return Ok(None);
            }
            let tx_offset_u32 = tx_offset_i32 as u32;

            // Funded extent: last valid byte is data_size - 1; reject offsets past it.
            let chunk_byte_offset = u64::from(tx_offset_u32).saturating_mul(chunk_size);
            if chunk_byte_offset >= info.data_size {
                return Ok(None);
            }

            Ok(Some((data_root, TxChunkOffset::from(tx_offset_u32))))
        })
    }

    /// Whether the local index can resolve `data_root` + placement for `partition_offset`.
    ///
    /// **Canonical readiness predicate** for data_sync re-arm and index-heal
    /// completion: path-hash density alone is not enough (DataRootInfos residual).
    #[inline]
    pub fn is_data_root_index_ready_at(&self, partition_offset: PartitionChunkOffset) -> bool {
        matches!(
            self.data_root_and_tx_offset_at(partition_offset),
            Ok(Some(_))
        )
    }

    /// Gets the tx_path and data_path for a chunk using its ledger relative offset
    pub fn read_tx_data_path(
        &self,
        chunk_offset: LedgerChunkOffset,
    ) -> eyre::Result<(Option<TxPath>, Option<ChunkDataPath>)> {
        let offset = PartitionChunkOffset::from(chunk_offset);
        Ok(self
            .read_tx_data_paths(offset, offset)?
            .remove(&offset)
            .unwrap_or((None, None)))
    }

    #[inline]
    pub fn query_submodule_db_by_offset<S, R>(
        &self,
        chunk_offset: PartitionChunkOffset,
        fetch_from_db: S,
    ) -> eyre::Result<R>
    where
        S: FnOnce(&mut reth_db::mdbx::tx::Tx<reth_db::mdbx::RO>) -> eyre::Result<R>,
    {
        let (_, submodule) = self.get_submodule_for_offset(chunk_offset)?;
        submodule.db.view(fetch_from_db)?
    }

    /// All half-open path-hash holes `[gap_start, gap_end)` in `[start, end)`.
    ///
    /// Walks each submodule once. Used by index heal to re-migrate only blocks
    /// that overlap holes (not the full SM tail after the first gap).
    ///
    /// For a single first-hole lookup at the DB layer, see
    /// [`irys_database::submodule::first_missing_path_hash_offset_in_tx`].
    pub fn missing_path_hash_ranges(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, PartitionChunkOffset)>> {
        if start >= end {
            return Ok(Vec::new());
        }

        let mut ranges = Vec::new();
        let mut offset = start;
        while offset < end {
            let (interval, submodule) = self.get_submodule_for_offset(offset)?;
            // Submodule intervals are inclusive; convert end to exclusive for the scan.
            // Deref of PartitionChunkOffset would make `+ 1` yield u32; construct explicitly.
            let submodule_end_excl = PartitionChunkOffset(*interval.end() + 1);
            let sub_end = std::cmp::min(end, submodule_end_excl);

            let mut sub_gaps = submodule
                .db
                .view(|tx| missing_path_hash_ranges_in_tx(tx, offset, sub_end))??;
            ranges.append(&mut sub_gaps);
            offset = sub_end;
        }
        Ok(ranges)
    }

    pub fn intervals(&self) -> &Arc<RwLock<StorageIntervals>> {
        &self.intervals
    }

    pub fn cut_then_insert_interval_if_touching(
        intervals: &mut StorageIntervals,
        chunk_offset: PartitionChunkOffset,
        chunk_type: ChunkType,
    ) {
        let chunk_interval = ii(chunk_offset, chunk_offset);
        let _ = intervals.cut(chunk_interval);
        let _ = intervals.insert_merge_touching_if_values_equal(chunk_interval, chunk_type);
    }

    #[inline]
    pub fn get_submodule_for_offset(
        &self,
        chunk_offset: PartitionChunkOffset,
    ) -> eyre::Result<(&Interval<PartitionChunkOffset>, &StorageSubmodule)> {
        self.submodules
            .get_key_value_at_point(chunk_offset)
            .map_err(|e| eyre!("Unable to get submodule for offset {:?}", &e))
    }

    /// Chunks already packed, in flush order, that fill at most `max_runs` pwrites.
    fn select_pending_window(
        &self,
        pending: &PendingWrites,
        max_runs: usize,
        cap: u64,
    ) -> Vec<(
        PartitionChunkOffset,
        (ChunkBytes, ChunkType),
        disk_lane::WritePriority,
    )> {
        if max_runs == 0 {
            return Vec::new();
        }
        let chunk_size = self.config.consensus.chunk_size.max(1);
        let mut indexed: Vec<(disk_lane::WritePriority, PartitionChunkOffset)> = pending
            .iter()
            .map(|(offset, state)| {
                let priority = pending
                    .priorities
                    .get(offset)
                    .copied()
                    .unwrap_or(match state.1 {
                        ChunkType::Entropy => disk_lane::WritePriority::Packing,
                        _ => disk_lane::WritePriority::Ingress,
                    });
                (priority, *offset)
            })
            .collect();
        indexed.sort_by_key(|(priority, offset)| (*priority, *offset));

        let mut out = Vec::new();
        let mut completed = 0usize;
        let mut current: Option<RunCursor> = None;
        let mut current_priority: Option<disk_lane::WritePriority> = None;
        for (priority, offset) in indexed {
            let Some((bytes, chunk_type)) = pending.get(&offset) else {
                continue;
            };
            let next_len = bytes.len() as u64;
            let next_type = *chunk_type;
            if current_priority != Some(priority) {
                if current.take().is_some() {
                    completed += 1;
                    if completed >= max_runs {
                        break;
                    }
                }
                current_priority = Some(priority);
            }
            let extend = current.as_ref().is_some_and(|run| {
                self.can_extend_run(chunk_size, cap, run, offset, next_type, next_len)
            });
            let start_new = !extend;
            if extend {
                let run = current.as_mut().expect("extend checks the current run");
                run.last = offset;
                run.chunk_count += 1;
                run.byte_len += next_len;
            } else if current.is_some() {
                completed += 1;
                if completed >= max_runs {
                    break;
                }
            }
            if start_new {
                current = Some(RunCursor {
                    start: offset,
                    last: offset,
                    chunk_type: next_type,
                    chunk_count: 1,
                    byte_len: next_len,
                });
            }
            out.push((offset, (bytes.clone(), next_type), priority));
        }
        out
    }

    /// Pending chunks inside a reorder scan, joined into runs of at most `cap`
    /// bytes. An aged run fills an in-flight slot first, then a longer run.
    /// `one_short` keeps every full run and the oldest aged short run, so a
    /// pile of short runs cannot fill the window and hide a full run. The
    /// caller passes false when the disk is idle, so the short runs go out.
    fn select_coalesced_window(
        &self,
        pending: &PendingWrites,
        max_runs: usize,
        scan_chunks: usize,
        cap: u64,
        one_short: bool,
    ) -> Vec<(
        PartitionChunkOffset,
        (ChunkBytes, ChunkType),
        disk_lane::WritePriority,
    )> {
        if max_runs == 0 || scan_chunks == 0 {
            return Vec::new();
        }
        let chunk_size = self.config.consensus.chunk_size.max(1);
        let mut indexed: Vec<(disk_lane::WritePriority, PartitionChunkOffset)> = pending
            .iter()
            .map(|(offset, state)| {
                let priority = pending
                    .priorities
                    .get(offset)
                    .copied()
                    .unwrap_or(match state.1 {
                        ChunkType::Entropy => disk_lane::WritePriority::Packing,
                        _ => disk_lane::WritePriority::Ingress,
                    });
                (priority, *offset)
            })
            .collect();
        indexed.sort_by_key(|(_, offset)| *offset);
        indexed.truncate(scan_chunks);

        let mut runs: Vec<CoalescedRun> = Vec::new();
        let mut current: Option<RunCursor> = None;
        let mut current_priority: Option<disk_lane::WritePriority> = None;
        let mut current_oldest: Option<Instant> = None;
        let mut current_items: Vec<(PartitionChunkOffset, (ChunkBytes, ChunkType))> = Vec::new();
        for (priority, offset) in indexed {
            let Some((bytes, chunk_type)) = pending.get(&offset) else {
                continue;
            };
            let next_len = bytes.len() as u64;
            let next_type = *chunk_type;
            let queued_at = pending
                .queued_at
                .get(&offset)
                .copied()
                .unwrap_or_else(Instant::now);
            let extend = current_priority == Some(priority)
                && current.as_ref().is_some_and(|run| {
                    self.can_extend_run(chunk_size, cap, run, offset, next_type, next_len)
                });
            if extend {
                let run = current.as_mut().expect("extend checks the current run");
                run.last = offset;
                run.chunk_count += 1;
                run.byte_len += next_len;
                if let Some(oldest) = current_oldest.as_mut()
                    && queued_at < *oldest
                {
                    *oldest = queued_at;
                }
                current_items.push((offset, (bytes.clone(), next_type)));
                continue;
            }
            if let Some(cursor) = current.take() {
                runs.push(CoalescedRun {
                    priority: current_priority.expect("a run has a priority"),
                    byte_len: cursor.byte_len,
                    oldest: current_oldest.unwrap_or_else(Instant::now),
                    aged: false,
                    items: std::mem::take(&mut current_items),
                });
            }
            current_priority = Some(priority);
            current_oldest = Some(queued_at);
            current = Some(RunCursor {
                start: offset,
                last: offset,
                chunk_type: next_type,
                chunk_count: 1,
                byte_len: next_len,
            });
            current_items.push((offset, (bytes.clone(), next_type)));
        }
        if let Some(cursor) = current.take() {
            runs.push(CoalescedRun {
                priority: current_priority.expect("a run has a priority"),
                byte_len: cursor.byte_len,
                oldest: current_oldest.unwrap_or_else(Instant::now),
                aged: false,
                items: current_items,
            });
        }

        let grace = disk_lane::reorder_grace(chunk_size);
        for run in &mut runs {
            run.aged = run.oldest.elapsed() >= grace;
        }
        runs.sort_by(|left, right| {
            right
                .aged
                .cmp(&left.aged)
                .then(right.byte_len.cmp(&left.byte_len))
                .then(left.oldest.cmp(&right.oldest))
                .then(left.priority.cmp(&right.priority))
                .then_with(|| {
                    let left_off = left.items.first().map(|(offset, _)| *offset);
                    let right_off = right.items.first().map(|(offset, _)| *offset);
                    left_off.cmp(&right_off)
                })
        });
        if one_short {
            let held = self.disk.short_writes_held();
            let mut chosen: Option<usize> = None;
            if !held {
                for (index, run) in runs.iter().enumerate() {
                    if !run.aged || run.byte_len >= cap {
                        continue;
                    }
                    let start = run.items.first().map(|(offset, _)| *offset);
                    let take = match chosen.and_then(|picked| runs.get(picked)) {
                        None => true,
                        Some(prev) => {
                            let prev_start = prev.items.first().map(|(offset, _)| *offset);
                            run.oldest < prev.oldest
                                || (run.oldest == prev.oldest && start < prev_start)
                        }
                    };
                    if take {
                        chosen = Some(index);
                    }
                }
            }
            let mut kept = Vec::with_capacity(runs.len());
            let mut short_run = None;
            for (index, run) in runs.into_iter().enumerate() {
                if run.byte_len >= cap {
                    kept.push(run);
                } else if chosen == Some(index) {
                    short_run = Some(run);
                }
            }
            if let Some(short_run) = short_run {
                kept.insert(0, short_run);
            }
            runs = kept;
        }
        runs.truncate(max_runs);

        let mut out = Vec::new();
        for run in runs {
            for (offset, state) in run.items {
                out.push((offset, state, run.priority));
            }
        }
        out
    }

    fn can_extend_run(
        &self,
        chunk_size: u64,
        cap: u64,
        run: &RunCursor,
        next_offset: PartitionChunkOffset,
        next_type: ChunkType,
        next_len: u64,
    ) -> bool {
        next_len == chunk_size
            && run.chunk_count as u64 * chunk_size == run.byte_len
            && run.chunk_type == next_type
            && run.last.0.saturating_add(1) == next_offset.0
            && run.byte_len + next_len <= cap
            && self.same_chunks_file(run.start, next_offset)
    }

    /// Runs in the order `batch` already has. The coalesce window ranks that
    /// order. A new priority or a break in the adjacent range starts a run.
    fn plan_ranked_runs(
        &self,
        batch: &[(
            PartitionChunkOffset,
            (ChunkBytes, ChunkType),
            disk_lane::WritePriority,
        )],
        cap: u64,
    ) -> Vec<WriteRun> {
        let chunk_size = self.config.consensus.chunk_size.max(1);
        let mut runs = Vec::new();
        let mut current: Option<WriteRun> = None;
        let mut current_priority: Option<disk_lane::WritePriority> = None;
        for (offset, (bytes, chunk_type), priority) in batch {
            let can_extend = current_priority == Some(*priority)
                && current.as_ref().is_some_and(|run| {
                    let Some(last) = run.offsets.last().copied() else {
                        return false;
                    };
                    self.can_extend_run(
                        chunk_size,
                        cap,
                        &RunCursor {
                            start: run.start,
                            last,
                            chunk_type: run.chunk_type,
                            chunk_count: run.offsets.len(),
                            byte_len: run.bytes.len() as u64,
                        },
                        *offset,
                        *chunk_type,
                        bytes.len() as u64,
                    )
                });
            if can_extend {
                let run = current.as_mut().expect("extend checks the current run");
                run.bytes.extend_from_slice(bytes);
                run.offsets.push(*offset);
            } else {
                if let Some(run) = current.take() {
                    runs.push(run);
                }
                current_priority = Some(*priority);
                current = Some(WriteRun {
                    start: *offset,
                    chunk_type: *chunk_type,
                    bytes: bytes.clone(),
                    offsets: vec![*offset],
                });
            }
        }
        if let Some(run) = current.take() {
            runs.push(run);
        }
        runs
    }

    /// Flush order is priority, then ascending offset. `cap` bounds one
    /// `pwrite`. A run is always at least one chunk. The coalesce path uses
    /// `plan_ranked_runs` so this order does not put a short high-priority
    /// run ahead of a longer one.
    fn plan_write_runs(
        &self,
        batch: &[(
            PartitionChunkOffset,
            (ChunkBytes, ChunkType),
            disk_lane::WritePriority,
        )],
        cap: u64,
    ) -> Vec<WriteRun> {
        let chunk_size = self.config.consensus.chunk_size.max(1);
        let mut runs = Vec::new();
        for priority in [
            disk_lane::WritePriority::Migration,
            disk_lane::WritePriority::Packing,
            disk_lane::WritePriority::Ingress,
        ] {
            let mut items: Vec<_> = batch
                .iter()
                .filter(|(_, _, item_priority)| *item_priority == priority)
                .collect();
            items.sort_by_key(|(offset, _, _)| *offset);
            let mut current: Option<WriteRun> = None;
            for (offset, (bytes, chunk_type), _) in items {
                let can_extend = current.as_ref().is_some_and(|run| {
                    let Some(last) = run.offsets.last().copied() else {
                        return false;
                    };
                    self.can_extend_run(
                        chunk_size,
                        cap,
                        &RunCursor {
                            start: run.start,
                            last,
                            chunk_type: run.chunk_type,
                            chunk_count: run.offsets.len(),
                            byte_len: run.bytes.len() as u64,
                        },
                        *offset,
                        *chunk_type,
                        bytes.len() as u64,
                    )
                });
                if can_extend {
                    let run = current.as_mut().expect("extend checks the current run");
                    run.bytes.extend_from_slice(bytes);
                    run.offsets.push(*offset);
                } else {
                    if let Some(run) = current.take() {
                        runs.push(run);
                    }
                    current = Some(WriteRun {
                        start: *offset,
                        chunk_type: *chunk_type,
                        bytes: bytes.clone(),
                        offsets: vec![*offset],
                    });
                }
            }
            if let Some(run) = current.take() {
                runs.push(run);
            }
        }
        runs
    }

    /// Caller already holds the write gate. This drops the file lock before it
    /// waits, so a queued mining recall starts before the next run.
    fn write_run(&self, run: &WriteRun) -> eyre::Result<()> {
        let chunk_size = self.config.consensus.chunk_size;
        let (file_arc, file_offset, submodule_offset) = {
            let (interval, submodule) = self.get_submodule_for_offset(run.start)?;
            let submodule_offset = run.start - interval.start();
            (
                Arc::clone(&submodule.file),
                u64::from(submodule_offset) * chunk_size,
                submodule_offset,
            )
        };
        let start_time = Instant::now();

        let write_result = (|| -> eyre::Result<()> {
            loop {
                self.disk.yield_to_recall();
                let file = self.disk.lock_chunks(&file_arc);
                if self.disk.recall_pending() {
                    drop(file);
                    continue;
                }
                file.write_all_at(run.bytes.as_slice(), file_offset)
                    .map_err(|e| {
                        error!(
                            "Failed to write chunk @ chunk_offset {} submodule_offset {}: {}",
                            run.start, submodule_offset, e
                        );
                        eyre::eyre!(
                            "Failed to write chunk @ chunk_offset {} submodule_offset {}: {}",
                            run.start,
                            submodule_offset,
                            e
                        )
                    })?;
                return Ok(());
            }
        })();
        self.note_write_run(run, start_time, write_result)
    }

    fn note_write_run(
        &self,
        run: &WriteRun,
        start_time: Instant,
        write_result: eyre::Result<()>,
    ) -> eyre::Result<()> {
        match write_result {
            Ok(()) => {
                let mut intervals = self
                    .intervals
                    .write()
                    .map_err(|e| eyre::eyre!("Failed to acquire write lock on intervals: {}", e))?;
                for chunk_offset in &run.offsets {
                    Self::cut_then_insert_interval_if_touching(
                        &mut intervals,
                        *chunk_offset,
                        run.chunk_type,
                    );
                }
            }
            Err(e) => {
                error!(
                    "Write failed, resetting interval to Uninitialized for chunk_offset {}",
                    run.start
                );
                match self.intervals.write() {
                    Ok(mut intervals) => {
                        for chunk_offset in &run.offsets {
                            Self::cut_then_insert_interval_if_touching(
                                &mut intervals,
                                *chunk_offset,
                                ChunkType::Uninitialized,
                            );
                        }
                    }
                    Err(write_err) => {
                        error!(
                            "CRITICAL: Failed to acquire write lock to reset interval to Uninitialized: {}. \
                     The interval state may be inconsistent!",
                            write_err
                        );
                    }
                }
                return Err(e);
            }
        }

        let completion_time = Instant::now();
        let mut recent_chunk_times = self.recent_chunk_times.write().map_err(|e| {
            eyre::eyre!("Failed to acquire write lock on recent_chunk_times: {}", e)
        })?;
        for chunk_offset in &run.offsets {
            recent_chunk_times.push(ChunkTimeRecord {
                chunk_offset: *chunk_offset,
                start_time,
                completion_time,
                duration: completion_time - start_time,
            });
        }
        Ok(())
    }

    /// Writes chunk data to physical storage and updates state tracking
    ///    DO NOT USE THIS FUNCTION STANDALONE
    ///    READ `sync_pending_chunks_inner`
    ///    notable hazards: fsync is NOT called, and the interval files are NOT updated by this function
    /// Process:
    /// 1. Locates correct submodule for chunk offset
    /// 2. Sets the chunk status to Interrupted before writing
    /// 3. Writes the intervals file to persist the Interrupted status
    /// 4. Calculates physical storage position
    /// 5. Writes chunk data to disk
    /// 6. Updates interval tracking with new chunk state
    ///
    /// Note: Chunk size must match size in StorageModule.config
    fn write_chunk_internal(
        &self,
        chunk_offset: PartitionChunkOffset,
        bytes: Vec<u8>,
        chunk_type: ChunkType,
    ) -> eyre::Result<()> {
        let chunk_size = self.config.consensus.chunk_size;

        // Get the correct submodule reference based on chunk_offset
        let (interval, submodule) = self.get_submodule_for_offset(chunk_offset)?;

        let start_time = Instant::now();

        // Get the submodule relative offset of the chunk
        let submodule_offset = chunk_offset - interval.start();

        // Attempt to write the chunk data, handling errors by resetting to Uninitialized
        let file_arc = Arc::clone(&submodule.file);
        let write_result = (|| -> eyre::Result<()> {
            // Positional write. The shared handle keeps its cursor.
            let file = self.disk.lock_chunks(&file_arc);
            file.write_all_at(bytes.as_slice(), u64::from(submodule_offset) * chunk_size)
                .map_err(|e| {
                    error!("Failed to write chunk @ chunk_offset {chunk_offset} submodule_offset {submodule_offset}: {}", e);
                    eyre::eyre!("Failed to write chunk @ chunk_offset {chunk_offset} submodule_offset {submodule_offset}: {}", e)
                })?;
            // note: we don't fsync here for performance reasons
            Ok(())
        })();

        // Update the interval based on whether the write succeeded or failed
        match write_result {
            Ok(()) => {
                // If successful, update the StorageModules interval state with the actual chunk type
                let mut intervals = self
                    .intervals
                    .write()
                    .map_err(|e| eyre::eyre!("Failed to acquire write lock on intervals: {}", e))?;
                Self::cut_then_insert_interval_if_touching(
                    &mut intervals,
                    chunk_offset,
                    chunk_type,
                );
            }
            Err(e) => {
                // If write failed, reset the interval to Uninitialized
                error!(
                    "Write failed, resetting interval to Uninitialized for chunk_offset {}",
                    chunk_offset
                );

                // Try to reset to Uninitialized, but don't propagate lock errors
                match self.intervals.write() {
                    Ok(mut intervals) => {
                        Self::cut_then_insert_interval_if_touching(
                            &mut intervals,
                            chunk_offset,
                            ChunkType::Uninitialized,
                        );
                    }
                    Err(write_err) => {
                        // Log the failure but don't return early - we still need to propagate the original error
                        error!(
                            "CRITICAL: Failed to acquire write lock to reset interval to Uninitialized: {}. \
                     The interval state may be inconsistent!",
                            write_err
                        );
                    }
                }

                // Always return the original write error
                return Err(e);
            }
        }

        let completion_time = Instant::now();

        let chunk_time_record = ChunkTimeRecord {
            chunk_offset,
            start_time,
            completion_time,
            duration: completion_time - start_time,
        };

        self.recent_chunk_times
            .write()
            .map_err(|e| eyre::eyre!("Failed to acquire write lock on recent_chunk_times: {}", e))?
            .push(chunk_time_record);

        Ok(())
    }

    /// Calculate write throughput in bytes per second based on chunk records
    /// Returns 0 if no records are available
    pub fn write_throughput_bps(&self) -> u64 {
        let chunk_size = self.config.consensus.chunk_size;
        let recent_chunk_times = self.recent_chunk_times.read().unwrap();

        if recent_chunk_times.is_empty() {
            tracing::debug!("write_throughput_bps: empty buffer, returning 0");
            return 0;
        }

        let front = recent_chunk_times.front().unwrap();
        let back = recent_chunk_times.back().unwrap();

        tracing::debug!(
            "write_throughput_bps: buffer_len={} chunk_size={} front_start={:?} back_completion={:?}",
            recent_chunk_times.len(),
            chunk_size,
            front.start_time,
            back.completion_time
        );

        // Calculate the actual time span covered by our records.
        //
        // Why this exists:
        // - The storage module batches writes of fixed-size chunks (consensus chunk_size).
        // - We want a lightweight, real-time estimate of sustained write throughput (bytes/sec)
        //   to make backpressure decisions in the data sync layer (e.g., throttling request rate).
        // - We derive throughput from the recorded timing of recent chunk writes to avoid heavy I/O stats.
        //
        // Behavior:
        // - Computes total bytes written over the time window spanned by the first and last sample.
        // - If there are no samples, returns 0 (no signal).
        // - If the window is extremely small, we treat it conservatively (see below) to avoid spikes.
        let time_span = back.completion_time.duration_since(front.start_time);

        // Total bytes processed in this time span: chunk_size × number_of_chunks_in_window
        let total_bytes = chunk_size * recent_chunk_times.len() as u64;

        // Throughput calculation (integer-only to avoid non-deterministic floating point):
        // - For spans >= 1s: return rounded division total_bytes / secs.
        // - For spans < 1s: scale using milliseconds with rounding, i.e.
        //     bytes_per_sec = round((total_bytes * 1000) / millis).
        // This keeps the signal smooth and deterministic while remaining inexpensive.
        let secs = time_span.as_secs();
        if secs >= 1 {
            // Rounded integer division for stable signal over longer spans
            return (total_bytes + secs / 2) / secs;
        }

        let millis = time_span.as_millis();
        if millis == 0 {
            // Extremely small span (sub-millisecond): avoid division-by-zero and
            // treat this as an instantaneous estimate bounded by total_bytes/sec.
            return total_bytes;
        }

        // Scale to per-second using millisecond precision with rounding.
        // Use u128 intermediates for headroom, then convert back to u64.
        let scaled = (total_bytes as u128) * 1000_u128;
        let per_sec = (scaled + millis / 2) / millis;
        per_sec as u64
    }

    /// Utility method asking the StorageModule to return its chunk range in
    /// ledger relative coordinates
    pub fn get_storage_module_ledger_offsets(&self) -> eyre::Result<LedgerChunkRange> {
        let pa = self.partition_assignment.read().unwrap();
        if let Some(part_assign) = *pa {
            if let Some(slot_index) = part_assign.slot_index {
                let start = slot_index as u64 * self.config.consensus.num_chunks_in_partition;
                let end = start + self.config.consensus.num_chunks_in_partition;
                Ok(LedgerChunkRange(ledger_chunk_offset_ie!(start, end)))
            } else {
                Err(eyre::eyre!("Ledger slot not assigned!"))
            }
        } else {
            Err(eyre::eyre!("Partition not assigned!"))
        }
    }

    /// Internal utility function to take a ledger relative range and make it
    /// Partition relative (relative to the partition assigned to the
    /// StorageModule)
    ///
    /// Errors if `chunk_range` is not fully contained in this module's ledger
    /// range: the offset subtraction would otherwise underflow (range starting
    /// below the module) or yield offsets past the partition end (range ending
    /// above it). Callers holding a merely overlapping range must intersect it
    /// with [`Self::get_storage_module_ledger_offsets`] first.
    pub fn make_range_partition_relative(
        &self,
        chunk_range: LedgerChunkRange,
    ) -> eyre::Result<PartitionChunkRange> {
        let storage_module_range = self.get_storage_module_ledger_offsets()?;
        eyre::ensure!(
            storage_module_range.contains_interval(&chunk_range),
            "chunk_range {:?} is not contained in the storage module's ledger range {:?}; intersect it with the module range first",
            chunk_range,
            storage_module_range
        );
        let start = chunk_range.start() - storage_module_range.start();
        let end = chunk_range.end() - storage_module_range.start();
        Ok(PartitionChunkRange(ii(
            PartitionChunkOffset::from(start),
            PartitionChunkOffset::from(end),
        )))
    }

    /// utility function to take a ledger relative offset and makes it
    /// Partition relative (relative to the partition assigned to the
    /// StorageModule)
    pub fn make_offset_partition_relative(
        &self,
        start_offset: LedgerChunkOffset,
    ) -> eyre::Result<i32> {
        let storage_module_range = self.get_storage_module_ledger_offsets()?;
        let start = *start_offset as i64 - *storage_module_range.start() as i64;
        Ok(start.try_into()?)
    }

    /// utility function to take a ledger relative offset and makes it
    /// Partition relative (relative to the partition assigned to the
    /// StorageModule)
    /// This version will return an Err if the provided ledger chunk offset is out of range for this storage module
    pub fn make_offset_partition_relative_guarded(
        &self,
        start_offset: LedgerChunkOffset,
    ) -> eyre::Result<u32> {
        let local_offset = self.make_offset_partition_relative(start_offset)?;
        if local_offset < 0 {
            return Err(eyre::eyre!("chunk offset not in storage module"));
        }
        // no need to worry about this conversion failing since we are already handling the negative case
        Ok(local_offset as u32)
    }

    /// Test utility function to mark a StorageModule as packed
    pub fn pack_with_zeros(&self) {
        let entropy_bytes = vec![0_u8; self.config.consensus.chunk_size as usize];
        for chunk_offset in 0..self.config.consensus.num_chunks_in_partition as u32 {
            self.write_chunk(
                PartitionChunkOffset::from(chunk_offset),
                entropy_bytes.clone(),
                ChunkType::Entropy,
            );
            self.sync_pending_chunks().unwrap();
        }
    }
}

impl Drop for StorageModule {
    fn drop(&mut self) {
        // Index submits for deposited bodies must finish before the drains stop.
        self.drain_entropy_queue();
        for (_, submodule) in self.submodules.iter() {
            submodule.index_drain.shutdown();
        }
        info!("Syncing SM {} to disk...", &self.id);
        if let Err(e) = self.force_sync_pending_chunks() {
            error!(
                "Unable to sync writes while dropping SM {} - {:?}",
                &self.id, &e
            );
        }
    }
}

fn ensure_default_intervals(
    submodule_interval: &Interval<PartitionChunkOffset>,
    intervals_path: &Path,
) -> eyre::Result<()> {
    let mut intervals = StorageIntervals::new();
    intervals
        .insert_merge_touching_if_values_equal(*submodule_interval, ChunkType::Uninitialized)
        .expect("to insert a default interval to the submodule intervals");

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(intervals_path)
        .wrap_err_with(|| {
            format!(
                "Failed to create or open intervals file at {}",
                intervals_path.display()
            )
        })?;

    let file_size = file.metadata()?.len();
    if file_size == 0 {
        let mut file = get_atomic_file(intervals_path)?;
        file.write_all(serde_json::to_string(&intervals)?.as_bytes())?;
        file.commit()?;
    }
    Ok(())
}

/// Reads and deserializes intervals from storage state file
///
/// Loads the stored interval mapping that tracks chunk states.
/// Expects a JSON-formatted file containing StorageIntervals.
pub fn read_intervals_file(path: &Path) -> eyre::Result<StorageIntervals> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .wrap_err_with(|| {
            format!(
                "Failed to create or open intervals file at {}",
                path.display()
            )
        })?;

    let size = file.metadata().unwrap().len() as usize;

    if size == 0 {
        return Err(eyre!("Intervals file is empty"));
    }

    let mut contents = String::with_capacity(size);
    file.seek(SeekFrom::Start(0))?;
    file.read_to_string(&mut contents).unwrap();
    let intervals = serde_json::from_str(&contents)?;
    Ok(intervals)
}

/// Loads storage module info from disk
pub fn read_info_file(path: &Path) -> eyre::Result<StorageModuleInfo> {
    let mut info_file = OpenOptions::new()
        .read(true)
        .open(path)
        .unwrap_or_else(|_| panic!("Failed to open: {}", path.display()));

    let mut contents = String::new();
    info_file.read_to_string(&mut contents).unwrap();
    let info = serde_json::from_str(&contents)?;
    Ok(info)
}

/// Saves storage module info to disk
pub fn write_info_file(path: &Path, info: &StorageModuleInfo) -> eyre::Result<()> {
    let mut info_file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap_or_else(|_| panic!("Failed to open: {}", path.display()));

    info_file.write_all(serde_json::to_string_pretty(info)?.as_bytes())?;
    Ok(())
}

fn hash_sha256(message: &[u8]) -> Result<[u8; 32], eyre::Error> {
    let mut hasher = sha::Sha256::new();
    hasher.update(message);
    let result = hasher.finish();
    Ok(result)
}

/// Retrieves all the storage modules overlapped by a range in a given ledger
pub fn get_overlapped_storage_modules(
    storage_modules_guard: &StorageModulesReadGuard,
    ledger: DataLedger,
    tx_chunk_range: &LedgerChunkRange,
) -> Vec<Arc<StorageModule>> {
    storage_modules_guard
        .read()
        .iter()
        .filter(|module| {
            (module
                .partition_assignment
                .read()
                .unwrap()
                .and_then(|pa| pa.ledger_id)
                == Some(ledger as u32))
                && module
                    .get_storage_module_ledger_offsets()
                    .is_ok_and(|range| range.overlaps(tx_chunk_range))
        })
        .cloned() // Clone the Arc, which is cheap
        .collect()
}

/// For a given ledger and ledger offset this function attempts to find
/// a storage module that overlaps the offset
pub fn get_storage_module_at_offset(
    storage_modules_guard: &StorageModulesReadGuard,
    ledger: DataLedger,
    chunk_offset: LedgerChunkOffset,
) -> Option<Arc<StorageModule>> {
    storage_modules_guard
        .read()
        .iter()
        .find(|module| {
            (module
                .partition_assignment
                .read()
                .unwrap()
                .and_then(|pa| pa.ledger_id)
                == Some(ledger as u32))
                && module
                    .get_storage_module_ledger_offsets()
                    .is_ok_and(|range| range.contains_point(chunk_offset))
        })
        .cloned()
}

pub const fn checked_add_i32_u64(a: i32, b: u64) -> Option<u64> {
    if a < 0 {
        // If a is negative, check if its absolute value is less than b
        let abs_a = a.unsigned_abs() as u64;
        b.checked_sub(abs_a)
    } else {
        // If a is positive or zero, convert to u64 and add
        let a_u64 = a as u64;
        b.checked_add(a_u64)
    }
}

// TODO: expand this, right now it's very specific
pub fn find_invalid_packing_starts(sm: Arc<StorageModule>) -> Vec<PartitionChunkOffset> {
    let mut invalid_starts = vec![];
    for range in sm.get_intervals(ChunkType::Entropy) {
        // binary search through packing, figuring out where the bad packing range starts
        // we assume the packing will have a clear cut line where the invalid packing starts
        let mut left = range.start();
        let mut right = range.end();

        while left < right {
            let mid = left + (*right - *left) / 2;

            if validate_packing_at_point(&sm, *mid).is_ok_and(|r| r) {
                left = mid + 1;
            } else {
                right = mid;
            }
        }
        if left != range.end() {
            invalid_starts.push(left - PartitionChunkOffset::from(1_u64))
        }
    }
    invalid_starts
}

pub fn validate_packing_at_point(sm: &Arc<StorageModule>, point: u32) -> eyre::Result<bool> {
    let chunk = sm.read_chunk_internal(PartitionChunkOffset::from(point))?;
    let chunk_size = sm.config.consensus.chunk_size;
    let mut out = Vec::with_capacity(chunk_size.try_into().unwrap());

    compute_entropy_chunk(
        sm.config.node_config.miner_address(),
        point as u64,
        sm.partition_hash().unwrap().0,
        sm.config.consensus.entropy_packing_iterations,
        chunk_size.try_into()?,
        &mut out,
        sm.config.consensus.chain_id,
    );

    Ok(out == chunk)
}

//==============================================================================
// Tests
//------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use irys_testing_utils::{chunk_bytes_gen, utils::TempDirBuilder};
    use irys_types::{
        ConsensusConfig, DataTransactionHeaderV1, DataTransactionLedger, H256, NodeConfig,
        SimpleRNG, StorageSyncConfig, TxChunkOffset, irys::IrysSigner, ledger_chunk_offset_ii,
        partition_chunk_offset_ii,
    };
    use nodit::interval::ii;

    #[test]
    fn recall_piece_plan_pairs_preads() {
        assert_eq!(
            super::recall_piece_plan(400, 400),
            vec![(0, 200), (200, 200)]
        );
        assert_eq!(super::recall_piece_plan(1, 400), vec![(0, 1)]);
        assert_eq!(super::recall_piece_plan(2, 400), vec![(0, 1), (1, 1)]);
        assert_eq!(
            super::recall_piece_plan(800, 400),
            vec![(0, 400), (400, 400)]
        );
        assert_eq!(
            super::recall_piece_plan(900, 400),
            vec![(0, 400), (400, 400), (800, 50), (850, 50)]
        );
        assert_eq!(super::recall_piece_plan(3, 1), vec![(0, 1), (1, 1), (2, 1)]);
    }

    /// Indexing a tx that starts before this module and straddles all three
    /// submodules must leave one job row per submodule, keyed on that
    /// submodule's clipped start, all carrying the *unclipped* (negative) tx
    /// start. Resolve removes exactly those rows, only for the matching
    /// data_root and block_height, and leaves the index itself untouched.
    #[test]
    fn index_writes_one_body_job_per_submodule_and_resolve_clears_them() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("pending_body_index_test")
            .with_tracing()
            .build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 20,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);

        // Slot 1 => this module owns ledger offsets [20, 40).
        let storage_module = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(irys_types::partition::PartitionAssignment {
                    ledger_id: Some(DataLedger::Submit.into()),
                    slot_index: Some(1),
                    miner_address: irys_types::IrysAddress::from([0xAA; 20]),
                    partition_hash: H256::random(),
                }),
                submodules: vec![
                    (partition_chunk_offset_ii!(0, 4), "hdd0".into()),
                    (partition_chunk_offset_ii!(5, 9), "hdd1".into()),
                    (partition_chunk_offset_ii!(10, 19), "hdd2".into()),
                ],
            },
            &config,
        )?;

        // 17-chunk tx spanning ledger offsets [15, 31]: begins 5 chunks before
        // this module, ends inside its third submodule.
        let data_root = H256::random();
        let data_size = 17 * config.consensus.chunk_size;
        let data_tx = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_root,
                data_size,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });
        let tx_range = LedgerChunkRange(ledger_chunk_offset_ii!(15, 31));
        let block_height = 29_875;

        storage_module.index_transaction_data(
            &data_tx,
            &vec![5, 6, 7, 8],
            tx_range,
            block_height,
        )?;

        let jobs = storage_module.pending_body_migrations()?;
        let keys: Vec<_> = jobs.iter().map(|(key, _)| *key).collect();
        assert_eq!(
            keys,
            vec![
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(5),
                PartitionChunkOffset::from(10),
            ],
            "one row per overlapping submodule, keyed on its clipped start"
        );
        for (_, job) in &jobs {
            assert_eq!(
                *job,
                PendingBodyMigration {
                    data_root,
                    data_size,
                    start_offset: RelativeChunkOffset(-5),
                    block_height,
                    attempts: 0,
                }
            );
        }

        // Batches follow submodule order and carry each submodule's interval.
        let batches = storage_module.pending_body_migration_batches()?;
        assert_eq!(batches.len(), 3);
        assert_eq!(batches[1].0, partition_chunk_offset_ii!(5, 9));
        assert_eq!(batches[1].1.len(), 1);

        // Wrong data_root: nothing settled, nothing bumped.
        let key = PartitionChunkOffset::from(5);
        assert!(!storage_module.settle_pending_body_migration(
            key,
            H256::random(),
            block_height
        )?);
        assert_eq!(
            storage_module.bump_pending_body_migration_attempts(
                key,
                H256::random(),
                block_height
            )?,
            None
        );
        assert_eq!(storage_module.pending_body_migrations()?.len(), 3);

        // Wrong block_height: same. A late settle from an orphaned height
        // must not take the replacement row at this offset.
        assert!(!storage_module.settle_pending_body_migration(key, data_root, block_height + 1)?);
        assert_eq!(
            storage_module.bump_pending_body_migration_attempts(
                key,
                data_root,
                block_height + 1
            )?,
            None
        );
        assert_eq!(storage_module.pending_body_migrations()?.len(), 3);

        // While paused, even a matching identity is left alone so a mid-pass
        // rollback cannot look like a successful drain.
        storage_module.pause_data_writes();
        assert!(!storage_module.settle_pending_body_migration(key, data_root, block_height)?);
        assert_eq!(
            storage_module.bump_pending_body_migration_attempts(key, data_root, block_height)?,
            None
        );
        assert_eq!(storage_module.pending_body_migrations()?.len(), 3);
        storage_module.resume_data_writes();

        // A no-progress pass is counted on the row, not by deleting it.
        assert_eq!(
            storage_module.bump_pending_body_migration_attempts(key, data_root, block_height)?,
            Some(1)
        );
        assert_eq!(storage_module.pending_body_migrations()?[1].1.attempts, 1);

        // Recovery purge is range-scoped: unassigning partition offsets [5, 19]
        // drops the two rows keyed there and leaves the first submodule's.
        assert_eq!(
            storage_module.purge_pending_body_migrations_in_range(
                PartitionChunkOffset::from(5),
                PartitionChunkOffset::from(19),
            )?,
            2
        );
        let keys: Vec<_> = storage_module
            .pending_body_migrations()?
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys, vec![PartitionChunkOffset::from(0)]);
        // Put them back so the settle path below is exercised on all three.
        storage_module.index_transaction_data(
            &data_tx,
            &vec![5, 6, 7, 8],
            tx_range,
            block_height,
        )?;
        assert_eq!(storage_module.pending_body_migrations()?.len(), 3);

        // Right data_root and height: each key settles once; index still answers.
        for key in [0, 5, 10] {
            assert!(storage_module.settle_pending_body_migration(
                PartitionChunkOffset::from(key),
                data_root,
                block_height
            )?);
        }
        assert!(storage_module.pending_body_migrations()?.is_empty());
        assert_eq!(
            storage_module
                .partition_offsets_for_data_root_chunk(data_root, TxChunkOffset::from(5))?,
            Some(vec![PartitionChunkOffset::from(0)]),
            "tx chunk 5 sits at partition offset 0 (start_offset -5); index survives settle"
        );

        // Settling again is a no-op.
        assert!(!storage_module.settle_pending_body_migration(
            PartitionChunkOffset::from(0),
            data_root,
            block_height
        )?);
        Ok(())
    }

    /// While paused, a data write is refused before anything is queued or
    /// indexed; packing's entropy writes still land; resuming lets the same
    /// write through.
    #[test]
    fn pause_data_writes_refuses_data_but_not_entropy() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("pause_data_writes_test")
            .with_tracing()
            .build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 5,
                num_chunks_in_partition: 5,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage_module = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(irys_types::partition::PartitionAssignment {
                    ledger_id: Some(DataLedger::Submit.into()),
                    slot_index: Some(0),
                    miner_address: irys_types::IrysAddress::from([0xAA; 20]),
                    partition_hash: H256::random(),
                }),
                submodules: vec![(partition_chunk_offset_ii!(0, 4), "hdd0".into())],
            },
            &config,
        )?;
        storage_module.pack_with_zeros();
        storage_module.force_sync_pending_chunks()?;

        let data_root = H256::random();
        let data_tx = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_root,
                data_size: 5,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });
        storage_module.index_transaction_data(
            &data_tx,
            &vec![5, 6, 7, 8],
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            0,
        )?;
        let chunk = UnpackedChunk {
            data_root,
            data_size: 5,
            data_path: vec![4, 3, 2, 1].into(),
            bytes: vec![0, 1, 2, 3, 4].into(),
            tx_offset: TxChunkOffset::from(0),
        };

        storage_module.pause_data_writes();
        assert!(storage_module.data_writes_paused());
        assert!(matches!(
            storage_module.write_data_chunk(&chunk),
            Err(WriteDataChunkError::WritesPaused)
        ));
        assert!(!storage_module.has_pending_writes());
        assert!(
            storage_module.write_chunk(
                PartitionChunkOffset::from(4),
                vec![0; 5],
                ChunkType::Entropy
            ),
            "packing is not gated"
        );
        assert!(
            !storage_module.write_chunk(PartitionChunkOffset::from(3), vec![0; 5], ChunkType::Data),
            "a direct data insert is refused under the lock"
        );

        storage_module.resume_data_writes();
        storage_module.write_data_chunk(&chunk)?;
        assert!(storage_module.is_data_write_pending_at(PartitionChunkOffset::from(0)));
        Ok(())
    }

    #[test]
    fn reset_waits_for_inflight_drain_and_clears_pending() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("reset_clears_pending")
            .with_tracing()
            .build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 5,
                num_chunks_in_partition: 5,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage_module = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(irys_types::partition::PartitionAssignment {
                    ledger_id: Some(DataLedger::Submit.into()),
                    slot_index: Some(0),
                    miner_address: irys_types::IrysAddress::from([0xAA; 20]),
                    partition_hash: H256::random(),
                }),
                submodules: vec![(partition_chunk_offset_ii!(0, 4), "hdd0".into())],
            },
            &config,
        )?;
        storage_module.pack_with_zeros();
        storage_module.force_sync_pending_chunks()?;

        assert!(storage_module.write_chunk(
            PartitionChunkOffset::from(0),
            vec![9; 5],
            ChunkType::Data
        ));
        assert!(storage_module.has_pending_writes());

        storage_module.pause_data_writes();
        let queued = storage_module.submit_index_op_for_test(
            PartitionChunkOffset::from(1),
            vec![1, 2, 3],
            0,
        );
        let _ = storage_module.reset()?;
        assert!(!storage_module.has_pending_writes());
        assert!(matches!(
            queued.recv()?,
            Err(WriteDataChunkError::WritesPaused)
        ));
        storage_module
            .get_submodule(PartitionChunkOffset::from(1))
            .ok_or_eyre("submodule")?
            .db
            .view_eyre(|tx| {
                assert!(get_path_hashes_by_offset(tx, PartitionChunkOffset::from(1))?.is_none());
                Ok(())
            })?;

        storage_module.resume_data_writes();
        storage_module.pack_with_zeros();
        storage_module.force_sync_pending_chunks()?;
        let data_root = H256::random();
        let data_tx = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_root,
                data_size: 5,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });
        storage_module.index_transaction_data(
            &data_tx,
            &vec![5, 6, 7, 8],
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            0,
        )?;
        storage_module.write_data_chunk(&UnpackedChunk {
            data_root,
            data_size: 5,
            data_path: vec![4, 3, 2, 1].into(),
            bytes: vec![0, 1, 2, 3, 4].into(),
            tx_offset: TxChunkOffset::from(0),
        })?;
        assert!(storage_module.is_data_write_pending_at(PartitionChunkOffset::from(0)));
        Ok(())
    }

    fn packed_submit_fixture(
        prefix: &str,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        StorageModule,
        UnpackedChunk,
    )> {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let mut node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 5,
                num_chunks_in_partition: 5,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        // A long interval must not delay the sweep. The lane and `drive_group`
        // run it when the slot is queued.
        node_config.storage.entropy_sweep_interval_millis = 60_000;
        let config = Config::new_with_random_peer_id(node_config);
        let storage_module = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(irys_types::partition::PartitionAssignment {
                    ledger_id: Some(DataLedger::Submit.into()),
                    slot_index: Some(0),
                    miner_address: irys_types::IrysAddress::from([0xAA; 20]),
                    partition_hash: H256::random(),
                }),
                submodules: vec![(partition_chunk_offset_ii!(0, 4), "hdd0".into())],
            },
            &config,
        )?;
        storage_module.pack_with_zeros();
        storage_module.force_sync_pending_chunks()?;
        let data_root = H256::random();
        let data_tx = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_root,
                data_size: 5,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });
        storage_module.index_transaction_data(
            &data_tx,
            &vec![5, 6, 7, 8],
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            0,
        )?;
        let chunk = UnpackedChunk {
            data_root,
            data_size: 5,
            data_path: vec![4, 3, 2, 1].into(),
            bytes: vec![0, 1, 2, 3, 4].into(),
            tx_offset: TxChunkOffset::from(0),
        };
        Ok((tmp_dir, storage_module, chunk))
    }

    #[test]
    fn failed_index_commit_does_not_insert_pending_data() -> eyre::Result<()> {
        let (_tmp, storage_module, chunk) = packed_submit_fixture("failed_index_no_pending")?;
        let offset = PartitionChunkOffset::from(0);
        storage_module.fail_next_index_commit();
        assert!(storage_module.write_data_chunk(&chunk).is_err());
        assert!(!storage_module.is_data_write_pending_at(offset));
        assert!(!storage_module.is_data_chunk_durable_at(offset));
        assert_eq!(
            storage_module.get_chunk_type(&offset),
            Some(ChunkType::Entropy)
        );
        storage_module.write_data_chunk(&chunk)?;
        assert!(storage_module.is_data_write_pending_at(offset));
        Ok(())
    }

    #[test]
    fn pending_entropy_is_replaced_only_after_index_ack() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("pending_entropy_after_ack")
            .with_tracing()
            .build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 5,
                num_chunks_in_partition: 5,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage_module = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(irys_types::partition::PartitionAssignment {
                    ledger_id: Some(DataLedger::Submit.into()),
                    slot_index: Some(0),
                    miner_address: irys_types::IrysAddress::from([0xAA; 20]),
                    partition_hash: H256::random(),
                }),
                submodules: vec![(partition_chunk_offset_ii!(0, 4), "hdd0".into())],
            },
            &config,
        )?;
        let offset = PartitionChunkOffset::from(0);
        assert!(storage_module.write_chunk(offset, vec![0; 5], ChunkType::Entropy));
        let data_root = H256::random();
        let data_tx = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_root,
                data_size: 5,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });
        storage_module.index_transaction_data(
            &data_tx,
            &vec![5, 6, 7, 8],
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            0,
        )?;
        let chunk = UnpackedChunk {
            data_root,
            data_size: 5,
            data_path: vec![4, 3, 2, 1].into(),
            bytes: vec![0, 1, 2, 3, 4].into(),
            tx_offset: TxChunkOffset::from(0),
        };
        storage_module.fail_next_index_commit();
        assert!(storage_module.write_data_chunk(&chunk).is_err());
        assert_eq!(
            storage_module.get_chunk_type(&offset),
            Some(ChunkType::Entropy)
        );
        assert!(
            !storage_module
                .pending_writes
                .read()
                .unwrap()
                .occupancy
                .contains_key(&offset)
        );
        storage_module.write_data_chunk(&chunk)?;
        assert_eq!(
            storage_module.get_chunk_type(&offset),
            Some(ChunkType::Data)
        );
        Ok(())
    }

    #[test]
    fn inflight_index_occupies_offset_against_second_writer() -> eyre::Result<()> {
        let (_tmp, storage_module, chunk) = packed_submit_fixture("inflight_occupies")?;
        let offset = PartitionChunkOffset::from(0);
        storage_module.occupy_offset_for_test(offset);
        assert!(storage_module.is_data_write_pending_at(offset));
        assert!(
            !storage_module
                .get_writeable_offsets(&chunk)?
                .contains(&offset)
        );
        let err = storage_module
            .write_data_chunk(&chunk)
            .expect_err("second writer must not succeed while occupied");
        assert!(
            err.to_string().contains("index write already in flight"),
            "expected in-flight error, got {err}"
        );
        assert_eq!(
            storage_module.get_chunk_type(&offset),
            Some(ChunkType::Entropy)
        );
        storage_module.release_occupied_offset_for_test(offset);
        storage_module.write_data_chunk(&chunk)?;
        assert_eq!(
            storage_module.get_chunk_type(&offset),
            Some(ChunkType::Data)
        );
        Ok(())
    }

    #[test]
    fn entropy_read_failure_releases_all_reserved_offsets() -> eyre::Result<()> {
        use irys_database::submodule::{add_data_root_info, tables::DataRootInfo};
        use irys_types::RelativeChunkOffset;

        let (_tmp, storage_module, chunk) = packed_submit_fixture("entropy_read_releases")?;
        let (_, submodule) =
            storage_module.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        submodule.db.update_eyre(|tx| {
            add_data_root_info(
                tx,
                chunk.data_root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(1),
                    data_size: 5,
                },
            )
        })?;

        storage_module.fail_entropy_read_at(2);
        assert!(storage_module.write_data_chunk(&chunk).is_err());
        storage_module.fail_entropy_read_at(0);
        assert!(!storage_module.is_data_write_pending_at(PartitionChunkOffset::from(0)));
        assert!(!storage_module.is_data_write_pending_at(PartitionChunkOffset::from(1)));

        storage_module.write_data_chunk(&chunk)?;
        assert!(storage_module.is_data_write_pending_at(PartitionChunkOffset::from(0)));
        assert!(storage_module.is_data_write_pending_at(PartitionChunkOffset::from(1)));
        Ok(())
    }

    #[test]
    fn entropy_sweep_reads_through_a_hole() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("entropy_sweep_hole")
            .with_tracing()
            .build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 5,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage_module = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(irys_types::partition::PartitionAssignment {
                    ledger_id: Some(DataLedger::Submit.into()),
                    slot_index: Some(0),
                    miner_address: irys_types::IrysAddress::from([0xAA; 20]),
                    partition_hash: H256::random(),
                }),
                submodules: vec![(partition_chunk_offset_ii!(0, 4), "hdd0".into())],
            },
            &config,
        )?;
        storage_module.pack_with_zeros();

        for (ledger_offset, root_byte, path) in
            [(0_u64, 1_u8, vec![1_u8, 2, 3, 4]), (2, 2, vec![5, 6, 7, 8])]
        {
            let data_root = H256::from([root_byte; 32]);
            let data_tx =
                DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
                    tx: DataTransactionHeaderV1 {
                        data_root,
                        data_size: 32,
                        ..Default::default()
                    },
                    metadata: irys_types::DataTransactionMetadata::new(),
                });
            storage_module.index_transaction_data(
                &data_tx,
                &path,
                LedgerChunkRange(ledger_chunk_offset_ii!(ledger_offset, ledger_offset)),
                0,
            )?;
            storage_module.deposit_data_chunk(&UnpackedChunk {
                data_root,
                data_size: 32,
                data_path: path.into(),
                bytes: vec![root_byte; 32].into(),
                tx_offset: TxChunkOffset::from(0),
            })?;
        }

        assert!(storage_module.is_data_write_pending_at(PartitionChunkOffset::from(0)));
        assert!(storage_module.is_data_write_pending_at(PartitionChunkOffset::from(2)));
        assert!(!storage_module.is_data_write_pending_at(PartitionChunkOffset::from(1)));
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Entropy)
        );
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset::from(2)),
            Some(ChunkType::Entropy)
        );
        assert_eq!(storage_module.disk.entropy_preads.load(Ordering::SeqCst), 0);

        storage_module.drive_entropy_queue_for_test();

        assert_eq!(storage_module.disk.entropy_preads.load(Ordering::SeqCst), 1);
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Data)
        );
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset::from(2)),
            Some(ChunkType::Data)
        );
        assert_eq!(
            storage_module.get_chunk_type(&PartitionChunkOffset::from(1)),
            Some(ChunkType::Entropy)
        );
        let hole = storage_module.read_chunks(partition_chunk_offset_ii!(1, 1))?;
        let (hole_bytes, hole_type) = hole
            .get(&PartitionChunkOffset::from(1))
            .expect("offset 1 stays on disk");
        assert_eq!(*hole_type, ChunkType::Entropy);
        assert_eq!(hole_bytes, &vec![0_u8; 32]);
        {
            let pending = storage_module.pending_writes.read().unwrap();
            assert_eq!(pending.queued_unpacked_bytes, 0);
            let runs =
                storage_module.write_run_metas(&pending, super::disk_lane::WRITE_RUN_MAX_BYTES);
            assert_eq!(runs.len(), 2);
        }
        Ok(())
    }

    fn entropy_disk_module(
        prefix: &str,
    ) -> eyre::Result<(irys_testing_utils::utils::tempfile::TempDir, StorageModule)> {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 5,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage_module = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(irys_types::partition::PartitionAssignment {
                    ledger_id: Some(DataLedger::Submit.into()),
                    slot_index: Some(0),
                    miner_address: irys_types::IrysAddress::from([0xAA; 20]),
                    partition_hash: H256::random(),
                }),
                submodules: vec![(partition_chunk_offset_ii!(0, 4), "hdd0".into())],
            },
            &config,
        )?;
        storage_module.pack_with_zeros();
        Ok((tmp_dir, storage_module))
    }

    fn queue_packed_chunk(
        storage: &StorageModule,
        ledger_offset: u64,
        root_byte: u8,
        path: Vec<u8>,
        priority: super::disk_lane::WritePriority,
    ) -> eyre::Result<()> {
        let data_root = H256::from([root_byte; 32]);
        let data_tx = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_root,
                data_size: 32,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });
        storage.index_transaction_data(
            &data_tx,
            &path,
            LedgerChunkRange(ledger_chunk_offset_ii!(ledger_offset, ledger_offset)),
            0,
        )?;
        storage.enqueue_unpacked(
            &UnpackedChunk {
                data_root,
                data_size: 32,
                data_path: path.into(),
                bytes: vec![root_byte; 32].into(),
                tx_offset: TxChunkOffset::from(0),
            },
            priority,
            false,
        )?;
        Ok(())
    }

    #[test]
    fn span_island_stays_out_of_the_buffer_until_every_ack() -> eyre::Result<()> {
        let (_tmp, storage) = entropy_disk_module("span_island_ack")?;
        queue_packed_chunk(
            &storage,
            0,
            1,
            vec![1, 2, 3, 4],
            super::disk_lane::WritePriority::Migration,
        )?;
        queue_packed_chunk(
            &storage,
            1,
            2,
            vec![5, 6, 7, 8],
            super::disk_lane::WritePriority::Migration,
        )?;
        assert!(storage.sweep_one_for_test());
        assert_eq!(storage.disk.entropy_preads.load(Ordering::SeqCst), 1);
        assert_eq!(storage.inflight_len_for_test(), 2);

        let started = Instant::now();
        while !storage.finish_one_ready_ack_for_test() {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "index ack did not arrive"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        {
            let pending = storage.pending_writes.read().unwrap();
            assert!(pending.get(&PartitionChunkOffset::from(0)).is_none());
            assert!(pending.get(&PartitionChunkOffset::from(1)).is_none());
        }

        let started = Instant::now();
        loop {
            storage.poll_acks();
            let pending = storage.pending_writes.read().unwrap();
            if pending.len() == 2 {
                let runs = storage.write_run_metas(&pending, super::disk_lane::WRITE_RUN_MAX_BYTES);
                assert_eq!(runs.len(), 1);
                assert_eq!(runs[0].byte_len, 64);
                let queued_at: Vec<_> = pending.queued_at.values().copied().collect();
                assert_eq!(queued_at.len(), 2);
                assert_eq!(queued_at[0], queued_at[1]);
                break;
            }
            drop(pending);
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "island did not enter the buffer"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    #[test]
    fn span_island_shares_one_priority() -> eyre::Result<()> {
        let (_tmp, storage) = entropy_disk_module("span_island_priority")?;
        queue_packed_chunk(
            &storage,
            0,
            1,
            vec![1, 2, 3, 4],
            super::disk_lane::WritePriority::Ingress,
        )?;
        queue_packed_chunk(
            &storage,
            1,
            2,
            vec![5, 6, 7, 8],
            super::disk_lane::WritePriority::Migration,
        )?;
        storage.drive_entropy_queue_for_test();
        let pending = storage.pending_writes.read().unwrap();
        let runs = storage.write_run_metas(&pending, super::disk_lane::WRITE_RUN_MAX_BYTES);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].byte_len, 64);
        assert_eq!(
            pending
                .priorities
                .get(&PartitionChunkOffset::from(0))
                .copied(),
            Some(super::disk_lane::WritePriority::Migration)
        );
        assert_eq!(
            pending
                .priorities
                .get(&PartitionChunkOffset::from(1))
                .copied(),
            Some(super::disk_lane::WritePriority::Migration)
        );
        Ok(())
    }

    #[test]
    fn contiguous_deposit_is_one_sweep_and_one_write_run() -> eyre::Result<()> {
        let (_tmp, storage) = entropy_disk_module("batch_span")?;
        let mut chunks = Vec::new();
        for (ledger_offset, root_byte, path) in [
            (0_u64, 1_u8, vec![1_u8, 2, 3, 4]),
            (1, 2, vec![5, 6, 7, 8]),
            (2, 3, vec![9, 10, 11, 12]),
        ] {
            let data_root = H256::from([root_byte; 32]);
            let data_tx =
                DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
                    tx: DataTransactionHeaderV1 {
                        data_root,
                        data_size: 32,
                        ..Default::default()
                    },
                    metadata: irys_types::DataTransactionMetadata::new(),
                });
            storage.index_transaction_data(
                &data_tx,
                &path,
                LedgerChunkRange(ledger_chunk_offset_ii!(ledger_offset, ledger_offset)),
                0,
            )?;
            chunks.push(UnpackedChunk {
                data_root,
                data_size: 32,
                data_path: path.into(),
                bytes: vec![root_byte; 32].into(),
                tx_offset: TxChunkOffset::from(0),
            });
        }

        let before = storage.disk.enqueue_notifies.load(Ordering::SeqCst);
        let items = storage.deposit_data_chunks(&chunks)?;
        assert_eq!(items.len(), 3);
        assert!(
            items
                .iter()
                .all(|item| matches!(item, BatchEnqueueItem::Queued(_)))
        );
        assert_eq!(
            storage.disk.enqueue_notifies.load(Ordering::SeqCst) - before,
            1,
            "one run publishes once"
        );
        assert_eq!(storage.sweep_slot_count_for_test(), 3);

        storage.drive_entropy_queue_for_test();
        assert_eq!(storage.disk.entropy_preads.load(Ordering::SeqCst), 1);
        let pending = storage.pending_writes.read().unwrap();
        let runs = storage.write_run_metas(&pending, super::disk_lane::WRITE_RUN_MAX_BYTES);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].byte_len, 96);
        Ok(())
    }

    fn durable_body_module(
        prefix: &str,
        chunks: u64,
        sweep_bytes: u64,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        Config,
        StorageModule,
    )> {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let chunk_size = 32;
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: chunks,
                num_chunks_in_recall_range: chunks,
                entropy_packing_iterations: 1,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 1000,
                entropy_sweep_max_bytes: sweep_bytes,
                entropy_coalesce_hole_bytes: sweep_bytes,
                drop_recall_page_cache: false,
            },
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let partition_hash = H256::repeat_byte(0x11);
        let storage = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(irys_types::partition::PartitionAssignment {
                    ledger_id: Some(DataLedger::Submit.into()),
                    slot_index: Some(0),
                    miner_address: config.node_config.miner_address(),
                    partition_hash,
                }),
                submodules: vec![(
                    ii(
                        PartitionChunkOffset::from(0),
                        PartitionChunkOffset::from(chunks - 1),
                    ),
                    "hdd0".into(),
                )],
            },
            &config,
        )?;
        Ok((tmp_dir, config, storage))
    }

    fn write_real_entropy(
        storage: &StorageModule,
        config: &Config,
        chunks: u32,
    ) -> eyre::Result<()> {
        let chunk_size = config.consensus.chunk_size as usize;
        let partition_hash = storage.partition_hash().expect("assigned partition");
        for offset in 0..chunks {
            let mut entropy = Vec::with_capacity(chunk_size);
            irys_packing::capacity_single::compute_entropy_chunk(
                config.node_config.miner_address(),
                u64::from(offset),
                partition_hash.0,
                config.consensus.entropy_packing_iterations,
                chunk_size,
                &mut entropy,
                config.consensus.chain_id,
            );
            assert!(storage.write_chunk(
                PartitionChunkOffset::from(offset),
                entropy,
                ChunkType::Entropy,
            ));
        }
        storage.force_sync_pending_chunks()?;
        Ok(())
    }

    #[test]
    fn a_durable_range_returns_every_body_in_one_pread() -> eyre::Result<()> {
        let chunks = 8_u64;
        let (_tmp, config, storage) = durable_body_module("durable_range", chunks, 256)?;
        write_real_entropy(&storage, &config, chunks as u32)?;
        let signer = IrysSigner::random_signer(&config.consensus);
        let data = (0..chunks as u8).flat_map(|byte| vec![byte; 32]).collect();
        let tx = signer.sign_transaction(signer.create_transaction(data, H256::zero())?)?;
        let unpacked = tx.data_chunks()?;
        let (_, proofs) = DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&tx.header));
        storage.index_transaction_data(
            &tx.header,
            &proofs[0].proof,
            LedgerChunkRange(ledger_chunk_offset_ii!(0, chunks - 1)),
            0,
        )?;
        for chunk in &unpacked {
            storage.write_data_chunk(chunk)?;
        }
        storage.force_sync_pending_chunks()?;
        storage.disk.source_preads.store(0, Ordering::SeqCst);
        storage.index_views.store(0, Ordering::SeqCst);

        let bodies = storage.read_durable_bodies(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(chunks as u32 - 1),
        )?;
        assert_eq!(bodies.len(), chunks as usize);
        for (index, body) in bodies.iter().enumerate() {
            let body = body.as_ref().expect("durable offset has a body");
            assert_eq!(body.bytes.0, unpacked[index].bytes.0);
        }
        assert_eq!(storage.disk.source_preads.load(Ordering::SeqCst), 1);
        assert_eq!(storage.index_views.load(Ordering::SeqCst), 1);

        storage.index_views.store(0, Ordering::SeqCst);
        let paths = storage.read_tx_data_paths(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(chunks as u32 - 1),
        )?;
        assert_eq!(paths.len(), chunks as usize);
        assert!(
            paths
                .values()
                .all(|(tx_path, data_path)| tx_path.is_some() && data_path.is_some())
        );
        assert_eq!(storage.index_views.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[test]
    fn an_uninitialized_hole_splits_the_durable_read() -> eyre::Result<()> {
        let chunks = 8_u64;
        let (_tmp, config, storage) = durable_body_module("durable_hole", chunks, 256)?;
        write_real_entropy(&storage, &config, chunks as u32)?;
        let signer = IrysSigner::random_signer(&config.consensus);
        let data = (0..chunks as u8).flat_map(|byte| vec![byte; 32]).collect();
        let tx = signer.sign_transaction(signer.create_transaction(data, H256::zero())?)?;
        let unpacked = tx.data_chunks()?;
        let (_, proofs) = DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&tx.header));
        storage.index_transaction_data(
            &tx.header,
            &proofs[0].proof,
            LedgerChunkRange(ledger_chunk_offset_ii!(0, chunks - 1)),
            0,
        )?;
        for chunk in unpacked.iter().filter(|chunk| chunk.tx_offset.0 != 3) {
            storage.write_data_chunk(chunk)?;
        }
        storage.force_sync_pending_chunks()?;
        // The skipped offset is still the entropy this fixture wrote. Cut it
        // back to uninitialized so the planner treats it as a hole.
        {
            let mut intervals = storage.intervals.write().unwrap();
            let point = partition_chunk_offset_ii!(3, 3);
            let _ = intervals.cut(point);
            let _ =
                intervals.insert_merge_touching_if_values_equal(point, ChunkType::Uninitialized);
        }
        storage.disk.source_preads.store(0, Ordering::SeqCst);
        storage.index_views.store(0, Ordering::SeqCst);

        let bodies = storage.read_durable_bodies(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(chunks as u32 - 1),
        )?;
        assert_eq!(bodies.len(), chunks as usize);
        assert!(bodies[3].is_none());
        for (index, body) in bodies.iter().enumerate() {
            if index == 3 {
                continue;
            }
            let body = body.as_ref().expect("durable offset has a body");
            assert_eq!(body.bytes.0, unpacked[index].bytes.0);
        }
        assert_eq!(storage.disk.source_preads.load(Ordering::SeqCst), 2);
        assert_eq!(storage.index_views.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[test]
    fn a_range_past_one_sweep_is_rejected() -> eyre::Result<()> {
        let (_tmp, _config, storage) = durable_body_module("sweep_cap", 8, 128)?;
        let error = storage
            .read_durable_bodies(PartitionChunkOffset::from(0), PartitionChunkOffset::from(4))
            .expect_err("five chunks are longer than a four-chunk sweep");
        assert!(
            error.to_string().contains("sweep"),
            "unexpected error: {error}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn write_data_chunk_queued_inserts_pending_after_ack() -> eyre::Result<()> {
        let (_tmp, storage_module, chunk) = packed_submit_fixture("queued_after_ack")?;
        storage_module.write_data_chunk_queued(&chunk).await?;
        assert!(storage_module.is_data_write_pending_at(PartitionChunkOffset::from(0)));
        Ok(())
    }

    struct LaneGuard {
        module: std::sync::Arc<StorageModule>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl LaneGuard {
        fn start(module: std::sync::Arc<StorageModule>) -> eyre::Result<Self> {
            let handle = module
                .spawn_disk_lane()?
                .ok_or_else(|| eyre::eyre!("disk lane thread"))?;
            Ok(Self {
                module,
                handle: Some(handle),
            })
        }
    }

    impl Drop for LaneGuard {
        fn drop(&mut self) {
            self.module.stop_disk_lane();
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    #[tokio::test]
    async fn disk_lane_thread_acks_queued_write() -> eyre::Result<()> {
        let (_tmp, storage_module, chunk) = packed_submit_fixture("lane_thread_ack")?;
        let lane = LaneGuard::start(std::sync::Arc::new(storage_module))?;
        lane.module.write_data_chunk_queued(&chunk).await?;
        let offset = PartitionChunkOffset::from(0);
        let started = Instant::now();
        while !lane.module.is_data_chunk_durable_at(offset) {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "lane did not write the packed chunk"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    #[test]
    fn storage_module_test() -> eyre::Result<()> {
        let infos = [StorageModuleInfo {
            id: 0,
            partition_assignment: None,
            submodules: vec![
                (partition_chunk_offset_ii!(0, 4), "hdd0-4TB".into()), // 0 to 4 inclusive
                (partition_chunk_offset_ii!(5, 9), "hdd1-4TB".into()), // 5 to 9 inclusive
                (partition_chunk_offset_ii!(10, 19), "hdd-8TB".into()), // 10 to 19 inclusive
            ],
        }];

        let tmp_dir = TempDirBuilder::new()
            .prefix("data_path_test")
            .with_tracing()
            .build();
        let base_path = tmp_dir.path().to_path_buf();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 20,
                ..ConsensusConfig::testing()
            }),
            base_directory: base_path.clone(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);

        // Create a StorageModule with the specified submodules and config
        let storage_module_info = &infos[0];
        let storage_module = StorageModule::new(storage_module_info, &config)?;

        // Verify the packing params file was crated in the submodule
        let params_path = base_path.join("hdd0-4TB").join("packing_params.toml");
        let params = PackingParams::from_toml(params_path).expect("packing params to load");
        assert_eq!(params.partition_hash, None);

        // Verify the entire storage module range is uninitialized
        let unpacked = storage_module.get_intervals(ChunkType::Uninitialized);
        assert_eq!(unpacked, [partition_chunk_offset_ii!(0, 19)]);

        // Create a test (fake) entropy chunk
        let entropy_chunk = vec![0xff; 32]; // All bytes set to 0xff
        storage_module.write_chunk(
            PartitionChunkOffset::from(1),
            entropy_chunk.clone(),
            ChunkType::Entropy,
        );

        // Invoke the sync task so it gets written to disk
        let _ = storage_module.sync_pending_chunks();

        // Validate the uninitialized intervals have been updated to reflect the new chunk
        let unpacked = storage_module.get_intervals(ChunkType::Uninitialized);
        assert_eq!(
            unpacked,
            [
                partition_chunk_offset_ii!(0, 0),
                partition_chunk_offset_ii!(2, 19)
            ]
        );

        // Validate the Entropy (Packed/unsynced) intervals have been updated
        let packed = storage_module.get_intervals(ChunkType::Entropy);
        assert_eq!(packed, [partition_chunk_offset_ii!(1, 1)]);

        // Validate entropy chunk can be read after writing
        let chunks = storage_module
            .read_chunks(partition_chunk_offset_ii!(1, 1))
            .unwrap();
        let chunk = chunks.get(&PartitionChunkOffset::from(1)).unwrap();
        assert_eq!(*chunk, (entropy_chunk.clone(), ChunkType::Entropy));

        // Validate that uninitialized chunks are not returned by read_chunks
        let chunks = storage_module
            .read_chunks(partition_chunk_offset_ii!(1, 2))
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(*chunk, (entropy_chunk.clone(), ChunkType::Entropy));

        // Write and sync two sequential data chunks that span a submodule boundary
        let data1_chunk = vec![0x4; 32];
        let data2_chunk = vec![0x5; 32];

        storage_module.write_chunk(
            PartitionChunkOffset::from(4),
            data1_chunk.clone(),
            ChunkType::Data,
        );
        storage_module.write_chunk(
            PartitionChunkOffset::from(5),
            data2_chunk.clone(),
            ChunkType::Data,
        );

        // Validate that the pending_writes has two entries
        let num_pending_writes: usize;
        {
            num_pending_writes = storage_module.pending_writes.read().unwrap().len();
        }
        assert_eq!(num_pending_writes, 2);

        // Write the data chunks to disk
        let _ = storage_module.sync_pending_chunks();

        // Validate the data intervals
        let data = storage_module.get_intervals(ChunkType::Data);
        assert_eq!(data, [partition_chunk_offset_ii!(4, 5)]);

        // Validate the unpacked intervals are updated
        let unpacked = storage_module.get_intervals(ChunkType::Uninitialized);
        assert_eq!(
            unpacked,
            [
                partition_chunk_offset_ii!(0, 0),
                partition_chunk_offset_ii!(2, 3),
                partition_chunk_offset_ii!(6, 19)
            ]
        );

        // Validate a read_chunks operation across submodule boundaries
        let chunks = storage_module
            .read_chunks(partition_chunk_offset_ii!(4, 5))
            .unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks.into_iter().collect::<Vec<_>>(),
            [
                (
                    PartitionChunkOffset::from(4),
                    (data1_chunk.clone(), ChunkType::Data)
                ),
                (
                    PartitionChunkOffset::from(5),
                    (data2_chunk.clone(), ChunkType::Data)
                )
            ]
        );

        // Query past the range of the StorageModule
        let chunks = storage_module
            .read_chunks(partition_chunk_offset_ii!(0, 25))
            .unwrap();

        // Verify only initialized chunks are returned
        assert_eq!(
            chunks.into_iter().collect::<Vec<_>>(),
            [
                (
                    PartitionChunkOffset::from(1),
                    (entropy_chunk, ChunkType::Entropy)
                ),
                (
                    PartitionChunkOffset::from(4),
                    (data1_chunk.clone(), ChunkType::Data)
                ),
                (
                    PartitionChunkOffset::from(5),
                    (data2_chunk, ChunkType::Data)
                )
            ]
        );

        // Make sure read_chunks does not return adjacent/touching chunks
        let chunks = storage_module
            .read_chunks(partition_chunk_offset_ii!(4, 4))
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks.into_iter().collect::<Vec<_>>(),
            [(
                PartitionChunkOffset::from(4),
                (data1_chunk, ChunkType::Data)
            ),]
        );

        // Load up the intervals from file
        let intervals = StorageModule::load_intervals_from_submodules(
            &storage_module.submodules,
            storage_module.id,
        );

        {
            let file_intervals = intervals.into_iter().collect::<Vec<_>>();
            let ints = storage_module.intervals.read().unwrap();
            let module_intervals = ints.clone().into_iter().collect::<Vec<_>>();
            assert_eq!(file_intervals, module_intervals);
        }
        // Test intervals reset
        let intervals = storage_module.reset().unwrap();

        // The hole storage interval is returned
        assert_eq!(intervals, partition_chunk_offset_ii!(0, 19));

        // Verify the entire storage module range is uninitialized again
        let unpacked = storage_module.get_intervals(ChunkType::Uninitialized);
        assert_eq!(unpacked, [partition_chunk_offset_ii!(0, 19)]);

        // Check intervals file is also reinitialized
        let intervals = StorageModule::load_intervals_from_submodules(
            &storage_module.submodules,
            storage_module.id,
        );

        {
            let file_intervals = intervals.into_iter().collect::<Vec<_>>();
            let ints = storage_module.intervals.read().unwrap();
            let module_intervals = ints.clone().into_iter().collect::<Vec<_>>();
            assert_eq!(file_intervals, module_intervals);
        }

        Ok(())
    }

    #[test]
    fn make_range_partition_relative_rejects_ranges_outside_the_module() -> eyre::Result<()> {
        // Slot 1 with 20 chunks per partition: ledger offsets [20, 39].
        let infos = [StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                slot_index: Some(1),
                ..PartitionAssignment::default()
            }),
            submodules: vec![(partition_chunk_offset_ii!(0, 19), "hdd0-test".into())],
        }];

        let tmp_dir = TempDirBuilder::new()
            .prefix("range_partition_relative_test")
            .build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 20,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage_module = StorageModule::new(&infos[0], &config)?;

        let full = storage_module
            .make_range_partition_relative(LedgerChunkRange(ledger_chunk_offset_ii!(20, 39)))?;
        assert_eq!(full, PartitionChunkRange(partition_chunk_offset_ii!(0, 19)));
        let inner = storage_module
            .make_range_partition_relative(LedgerChunkRange(ledger_chunk_offset_ii!(25, 30)))?;
        assert_eq!(
            inner,
            PartitionChunkRange(partition_chunk_offset_ii!(5, 10))
        );

        // A range starting below the module used to underflow the offset
        // subtraction and panic; it must error instead.
        assert!(
            storage_module
                .make_range_partition_relative(LedgerChunkRange(ledger_chunk_offset_ii!(10, 25)))
                .is_err()
        );

        // A range ending above the module used to silently produce offsets
        // past the partition end; it must error instead.
        assert!(
            storage_module
                .make_range_partition_relative(LedgerChunkRange(ledger_chunk_offset_ii!(25, 45)))
                .is_err()
        );

        Ok(())
    }

    #[test]
    fn drop_pending_writes_in_range_purges_only_the_range() -> eyre::Result<()> {
        let infos = [StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment::default()),
            submodules: vec![(partition_chunk_offset_ii!(0, 9), "hdd0-test".into())],
        }];
        let tmp_dir = TempDirBuilder::new()
            .prefix("drop_pending_writes_test")
            .build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 10,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage_module = StorageModule::new(&infos[0], &config)?;

        // Queue writes at offsets 0..6.
        let bytes = vec![0_u8; config.consensus.chunk_size as usize];
        for offset in 0..6_u32 {
            storage_module.write_chunk(
                PartitionChunkOffset::from(offset),
                bytes.clone(),
                ChunkType::Entropy,
            );
        }

        // Purge the orphaned range [2, 4] (inclusive).
        storage_module.drop_pending_writes_in_range(
            PartitionChunkOffset::from(2),
            PartitionChunkOffset::from(4),
        );

        let remaining: Vec<u32> = storage_module
            .pending_writes
            .read()
            .unwrap()
            .keys()
            .map(|o| **o)
            .collect();
        assert_eq!(
            remaining,
            vec![0, 1, 5],
            "only offsets inside [2, 4] are dropped; those outside remain queued"
        );

        Ok(())
    }

    #[test]
    fn clear_data_root_infos_in_range_drops_placements_overlapping_by_extent() -> eyre::Result<()> {
        use irys_database::submodule::{add_data_root_info, tables::DataRootInfo};
        use irys_types::RelativeChunkOffset;

        let infos = [StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment::default()),
            submodules: vec![(partition_chunk_offset_ii!(0, 9), "hdd0-test".into())],
        }];
        let tmp_dir = TempDirBuilder::new()
            .prefix("clear_dri_extent_test")
            .build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 10,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage_module = StorageModule::new(&infos[0], &config)?;

        // Three placements of the same (orphaned) data_root; orphaned range is [3, 5].
        let data_root = H256::random();
        let (_, submodule) =
            storage_module.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        submodule.db.update_eyre(|tx| {
            // Canonical: extent [0, 0], entirely below the orphaned range.
            add_data_root_info(
                tx,
                data_root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(0),
                    data_size: 32,
                },
            )?;
            // Orphaned: starts at offset 2 (below range_start 3) but spans 3
            // chunks → extent [2, 4], reaching into the range. `start_offset`
            // alone (2 < 3) would wrongly keep it; extent overlap drops it.
            add_data_root_info(
                tx,
                data_root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(2),
                    data_size: 96,
                },
            )?;
            // Orphaned: extent [4, 4], fully inside the range.
            add_data_root_info(
                tx,
                data_root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(4),
                    data_size: 32,
                },
            )?;
            // Orphaned cross-partition placement anchored in a lower module:
            // negative start_offset whose extent [-2, 3] (ceil(192 / 32) = 6
            // chunks) reaches into the range. start_offset alone (-2 < 3) would
            // wrongly keep it; this exercises the negative-i32 extent path.
            add_data_root_info(
                tx,
                data_root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(-2),
                    data_size: 192,
                },
            )
        })?;

        storage_module.clear_data_root_infos_in_range(
            PartitionChunkOffset::from(3),
            PartitionChunkOffset::from(5),
            &[data_root],
        )?;

        let remaining = storage_module.collect_data_root_infos(data_root)?;
        assert_eq!(
            remaining.0,
            vec![DataRootInfo {
                start_offset: RelativeChunkOffset(0),
                data_size: 32,
            }],
            "only the canonical placement whose extent lies entirely outside [3, 5] \
             survives; the placement starting at offset 2 but extending into the \
             range is dropped by extent overlap"
        );

        Ok(())
    }

    /// Residual Entropy hole: tx migration wrote `tx_path` + `DataRootInfos` but
    /// no chunk body / `data_path_hash`. `data_root_and_tx_offset_at` must still
    /// resolve addressing for data_root-based fetch (proof-signer path).
    #[test]
    fn data_root_and_tx_offset_at_works_without_chunk_body() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("data_root_tx_offset_residual")
            .with_tracing()
            .build();
        let chunk_size = 32_u64;
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: 100,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let sm = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(PartitionAssignment::default()),
                submodules: vec![(partition_chunk_offset_ii!(0, 99), "hdd0".into())],
            },
            &config,
        )?;
        sm.pack_with_zeros();

        // 3 chunks (2.5 * chunk_size rounded up)
        let data_size = (chunk_size as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0_u8; data_size];
        for (i, b) in data_bytes.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let irys = IrysSigner::random_signer(&config.consensus);
        let tx = irys
            .create_transaction(data_bytes.clone(), H256::zero())
            .unwrap();
        let tx = irys.sign_transaction(tx).unwrap();
        let data_root = tx.header.data_root;

        // Place the tx at partition-relative start 10 → offsets 10, 11, 12.
        let start = 10_u32;
        let num_chunks = data_size.div_ceil(chunk_size as usize) as u32;
        assert_eq!(num_chunks, 3);
        let end = start + num_chunks - 1;
        let (_tx_root, proofs) =
            DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&tx.header));
        sm.index_transaction_data(
            &tx.header,
            &proofs[0].proof,
            LedgerChunkRange(ledger_chunk_offset_ii!(start, end)),
            0,
        )?;

        // No write_data_chunk: every offset is a residual hole (Entropy, no data_path).
        for tx_off in 0..num_chunks {
            let part_off = PartitionChunkOffset::from(start + tx_off);
            assert_eq!(
                sm.get_chunk_type(&part_off),
                Some(ChunkType::Entropy),
                "offset {part_off} should still be Entropy (no body written)"
            );
            // get_chunk_metadata requires data_path — residual holes must return None.
            assert!(
                sm.get_chunk_metadata(part_off)?.is_none(),
                "get_chunk_metadata needs data_path_hash; residual holes have none"
            );

            let resolved = sm
                .data_root_and_tx_offset_at(part_off)?
                .expect("residual hole must still resolve data_root + tx_offset");
            assert_eq!(resolved.0, data_root);
            assert_eq!(*resolved.1, tx_off);
        }

        // Write only the middle chunk body; outer residual holes still resolve.
        let mid = 1_u32;
        let min = tx.chunks[mid as usize].min_byte_range;
        let max = tx.chunks[mid as usize].max_byte_range;
        sm.write_data_chunk(&UnpackedChunk {
            data_root,
            data_size: data_size as u64,
            data_path: Base64(tx.proofs[mid as usize].proof.clone()),
            bytes: Base64(data_bytes[min..max].to_vec()),
            tx_offset: TxChunkOffset::from(mid),
        })?;
        sm.sync_pending_chunks()?;

        for tx_off in 0..num_chunks {
            let part_off = PartitionChunkOffset::from(start + tx_off);
            let resolved = sm
                .data_root_and_tx_offset_at(part_off)?
                .expect("resolution works with or without body");
            assert_eq!(resolved.0, data_root);
            assert_eq!(*resolved.1, tx_off);
        }

        // Unindexed offset → None
        assert!(
            sm.data_root_and_tx_offset_at(PartitionChunkOffset::from(50))?
                .is_none()
        );

        Ok(())
    }

    /// Clearing path-hash indexes must leave real key absences so the gap scan
    /// (and thus index heal) can see the range — not present-with-None tombstones.
    #[test]
    fn clear_offset_index_in_range_leaves_the_range_as_a_gap() -> eyre::Result<()> {
        use irys_database::submodule::{set_path_hashes_by_offset, tables::ChunkPathHashes};

        let infos = [StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment::default()),
            submodules: vec![(partition_chunk_offset_ii!(0, 9), "hdd0-test".into())],
        }];
        let tmp_dir = TempDirBuilder::new()
            .prefix("clear_offset_index_test")
            .build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 10,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage_module = StorageModule::new(&infos[0], &config)?;

        // Index a dense range [0, 9].
        let path_hashes = ChunkPathHashes {
            data_path_hash: Some(H256::random()),
            tx_path_hash: Some(H256::random()),
        };
        let (_, submodule) =
            storage_module.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        submodule.db.update_eyre(|tx| {
            for offset in 0..10_u32 {
                set_path_hashes_by_offset(
                    tx,
                    PartitionChunkOffset::from(offset),
                    path_hashes.clone(),
                )?;
            }
            Ok(())
        })?;

        assert!(
            storage_module
                .missing_path_hash_ranges(
                    PartitionChunkOffset::from(0),
                    PartitionChunkOffset::from(10)
                )?
                .is_empty(),
            "dense range must report no gaps before clear"
        );

        storage_module.clear_offset_index_in_range(
            PartitionChunkOffset::from(3),
            PartitionChunkOffset::from(5),
        )?;

        let gaps = storage_module.missing_path_hash_ranges(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(10),
        )?;
        assert_eq!(
            gaps,
            vec![(PartitionChunkOffset::from(3), PartitionChunkOffset::from(6))],
            "cleared [3,5] inclusive must be a half-open [3,6) gap"
        );

        Ok(())
    }

    #[test]
    fn pending_writes_test() -> eyre::Result<()> {
        let infos = [StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment::default()),
            submodules: vec![
                (partition_chunk_offset_ii!(0, 50), "hdd0-test".into()), // 0 to 50 inclusive
            ],
        }];

        // SAFETY: test is single-threaded; setting env var before any tracing init.
        unsafe { std::env::set_var("RUST_LOG", "debug") };
        let tmp_dir = TempDirBuilder::new()
            .prefix("pending_writes_test")
            .with_tracing()
            .build();
        let base_path = tmp_dir.path().to_path_buf();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 51,
                block_migration_depth: 1,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 10,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 0,
                ..StorageSyncConfig::default()
            },
            base_directory: base_path,
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let chunk_size = config.consensus.chunk_size as usize;

        // Create a StorageModule with the specified submodules and config
        let storage_module_info = &infos[0];
        let storage_module = StorageModule::new(storage_module_info, &config)?;

        // Queue up some entropy chunks in the pending writes queue
        let entropy_bytes = vec![0_u8; chunk_size];
        for chunk_offset in 0..10_u32 {
            storage_module.write_chunk(
                PartitionChunkOffset::from(chunk_offset),
                entropy_bytes.clone(),
                ChunkType::Entropy,
            );
        }

        // Sync the chunks
        storage_module.sync_pending_chunks()?;
        assert!(
            (0..10_u32)
                .map(PartitionChunkOffset::from)
                .all(|offset| !storage_module.is_data_chunk_durable_at(offset)),
            "entropy writes must never be reported as durable transaction data"
        );

        // Write 9 more entropy chunks
        for chunk_offset in 10..19_u32 {
            storage_module.write_chunk(
                PartitionChunkOffset::from(chunk_offset),
                entropy_bytes.clone(),
                ChunkType::Entropy,
            );
        }

        // A normal sync below the configured threshold must preserve the buffer.
        storage_module.sync_pending_chunks()?;
        assert!(
            storage_module.has_pending_writes(),
            "a normal sync below the configured threshold must preserve the write buffer"
        );

        let entropy = storage_module.get_intervals(ChunkType::Entropy);
        let uninitialized = storage_module.get_intervals(ChunkType::Uninitialized);

        // Verify that the intervals returned by the storage module
        // are a union of the stored chunks and the pending writes
        assert_eq!(entropy.len(), 1);
        assert_eq!(entropy[0], partition_chunk_offset_ii!(0, 18));

        assert_eq!(uninitialized.len(), 1);
        assert_eq!(uninitialized[0], partition_chunk_offset_ii!(19, 50));

        {
            // Verify that the correct number of writes are still pending
            let pending = storage_module
                .pending_writes
                .read()
                .expect("to read pending writes");
            assert_eq!(pending.len(), 9);
        }

        // Test - write a data chunk that overwrites a pending entropy chunk
        let bytes = vec![10_u8; chunk_size];
        let chunk_offset = PartitionChunkOffset::from(11);
        storage_module.write_chunk(chunk_offset, bytes.clone(), ChunkType::Data);

        {
            // Verify the resulting intervals
            let data = storage_module.get_intervals(ChunkType::Data);
            assert_eq!(data.len(), 1);
            assert_eq!(
                data[0],
                partition_chunk_offset_ii!(chunk_offset, chunk_offset)
            );

            let pending = storage_module
                .pending_writes
                .read()
                .expect("to read pending writes");

            // Verify the pending chunk now has the data bytes and correct chunk type
            let (pending_chunk_bytes, pending_chunk_type) = pending.get(&chunk_offset).unwrap();
            assert_eq!(pending_chunk_bytes, &bytes);
            assert_eq!(*pending_chunk_type, ChunkType::Data);
        }

        // Test - write a data chunk that overwrites a stored entropy chunk
        let bytes = vec![20_u8; chunk_size];
        let chunk_offset = PartitionChunkOffset::from(2);
        storage_module.write_chunk(chunk_offset, bytes, ChunkType::Data);

        {
            // Verify the resulting intervals
            let data = storage_module.get_intervals(ChunkType::Data);
            assert_eq!(data.len(), 2);
            assert_eq!(
                data[0],
                partition_chunk_offset_ii!(chunk_offset, chunk_offset)
            );
            // data chunk from previous test
            assert_eq!(data[1], partition_chunk_offset_ii!(11, 11));
        }

        // Test - write an pending entropy chunk to an uninitialized offset on disk
        let bytes = vec![30_u8; chunk_size];
        let chunk_offset = PartitionChunkOffset::from(20);
        storage_module.write_chunk(chunk_offset, bytes, ChunkType::Entropy);
        {
            // Verify the resulting intervals
            let entropy = storage_module.get_intervals(ChunkType::Entropy);
            debug!("{:#?}", entropy);
            assert_eq!(entropy.len(), 4);
            assert_eq!(entropy[0], partition_chunk_offset_ii!(0, 1));
            // chunk offset 2 is a (pending) data chunk
            assert_eq!(entropy[1], partition_chunk_offset_ii!(3, 10));
            // chunk_offset 11 is data
            assert_eq!(entropy[2], partition_chunk_offset_ii!(12, 18));
            // entropy[19] is uninitialized
            assert_eq!(entropy[3], partition_chunk_offset_ii!(20, 20));

            let uninitialized = storage_module.get_intervals(ChunkType::Uninitialized);
            assert_eq!(uninitialized.len(), 2);
            assert_eq!(uninitialized[0], partition_chunk_offset_ii!(19, 19));
            assert_eq!(uninitialized[1], partition_chunk_offset_ii!(21, 50));
        }

        // Test - write a pending data chunk to an uninitialized offset on disk
        let bytes = vec![40_u8; chunk_size];
        let chunk_offset = PartitionChunkOffset::from(19);
        storage_module.write_chunk(chunk_offset, bytes, ChunkType::Data);
        {
            // Verify the resulting intervals
            let data = storage_module.get_intervals(ChunkType::Data);
            assert_eq!(data.len(), 3);
            assert_eq!(data[0], partition_chunk_offset_ii!(2, 2));
            assert_eq!(data[1], partition_chunk_offset_ii!(11, 11));
            assert_eq!(data[2], partition_chunk_offset_ii!(19, 19));

            let uninitialized = storage_module.get_intervals(ChunkType::Uninitialized);
            assert_eq!(uninitialized.len(), 1);
            assert_eq!(uninitialized[0], partition_chunk_offset_ii!(21, 50));
        }

        // Record the chunks before sync
        let read_range = partition_chunk_offset_ii!(0, 50);
        let before_chunks = storage_module
            .read_chunks(read_range)
            .expect("to read chunks");

        // Test that they are stable and expected values after disk sync
        storage_module.sync_pending_chunks()?;

        for offset in [2_u32, 11, 19].map(PartitionChunkOffset::from) {
            assert!(
                storage_module.is_data_chunk_durable_at(offset),
                "fsynced data must be reported as durable at {offset}"
            );
        }

        {
            // Ensure all pending writes were written to disk
            let pending = storage_module
                .pending_writes
                .read()
                .expect("to read pending writes");

            assert_eq!(pending.len(), 0);
        }

        // Get the chunks after the sync
        let after_chunks = storage_module
            .read_chunks(read_range)
            .expect("to read chunks");

        // Compare before and after chunk maps
        assert_eq!(
            before_chunks.len(),
            after_chunks.len(),
            "Maps have different sizes"
        );

        // compare all the values in the before map to the after map
        for (key, before_value) in before_chunks.iter() {
            match after_chunks.get(key) {
                Some(after_value) => {
                    assert_eq!(before_value, after_value, "Values differ for key {:?}", key)
                }
                None => panic!(
                    "Key {:?} exists in before_chunks but not in after_chunks",
                    key
                ),
            }
        }

        // check that after_chunks doesn't have extra keys
        for key in after_chunks.keys() {
            assert!(
                before_chunks.contains_key(key),
                "Key {:?} exists in after_chunks but not in before_chunks",
                key
            );
        }

        Ok(())
    }

    #[test]
    fn data_path_test() -> eyre::Result<()> {
        let infos = [StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment::default()),
            submodules: vec![
                (partition_chunk_offset_ii!(0, 4), "hdd0-4TB".into()), // 0 to 4 inclusive
            ],
        }];

        let tmp_dir = TempDirBuilder::new()
            .prefix("data_path_test")
            .with_tracing()
            .build();
        let base_path = tmp_dir.path().to_path_buf();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 5,
                num_chunks_in_partition: 5,
                ..ConsensusConfig::testing()
            }),
            base_directory: base_path,
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);

        // Create a StorageModule with the specified submodules and config
        let storage_module_info = &infos[0];
        let storage_module = StorageModule::new(storage_module_info, &config)?;
        let chunk_data = vec![0, 1, 2, 3, 4];
        let data_path = vec![4, 3, 2, 1];
        let tx_path = vec![5, 6, 7, 8];
        let data_root = H256::zero();
        let data_size = chunk_data.len() as u64;

        // Pack the storage module
        storage_module.pack_with_zeros();

        // Create a dummy data tx header to provide the data_size and data_root
        let data_tx = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_root,
                data_size,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });

        let _ = storage_module.index_transaction_data(
            &data_tx,
            &tx_path,
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            0,
        );

        let chunk = UnpackedChunk {
            data_root: H256::zero(),
            data_size,
            data_path: data_path.clone().into(),
            bytes: chunk_data.into(),
            tx_offset: TxChunkOffset::from(0),
        };

        storage_module.write_data_chunk(&chunk)?;

        let (_, ret_path) = storage_module.read_tx_data_path(LedgerChunkOffset::from(0))?;

        assert_eq!(ret_path, Some(data_path));

        // check db is cleared
        let _intervals = storage_module.reset().unwrap();

        let (tx_path, ret_path) = storage_module.read_tx_data_path(LedgerChunkOffset::from(0))?;

        assert!(tx_path.is_none());
        assert!(ret_path.is_none());

        Ok(())
    }

    #[test]
    fn test_write_interruption_flow() -> eyre::Result<()> {
        // This test verifies the interruption flow by checking the interval
        // files that are written during the write_chunk_internal process

        let tmp_dir = TempDirBuilder::new()
            .prefix("write_interruption_test")
            .with_tracing()
            .build();
        let base_path = tmp_dir.path().to_path_buf();

        let infos = [StorageModuleInfo {
            id: 0,
            partition_assignment: None,
            submodules: vec![(partition_chunk_offset_ii!(0, 10), "test-submodule".into())],
        }];

        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 11,
                ..ConsensusConfig::testing()
            }),
            base_directory: base_path,
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);

        let storage_module = StorageModule::new(&infos[0], &config)?;

        // Verify initial state is uninitialized
        let uninitialized = storage_module.get_intervals(ChunkType::Uninitialized);
        assert_eq!(uninitialized, [partition_chunk_offset_ii!(0, 10)]);

        // Write a successful chunk to verify normal flow
        let test_chunk = vec![0x42; 32];
        storage_module.write_chunk_internal(
            PartitionChunkOffset::from(5),
            test_chunk,
            ChunkType::Data,
        )?;

        // After successful write, chunk should be marked as Data
        let data = storage_module.get_intervals(ChunkType::Data);
        assert_eq!(data, [partition_chunk_offset_ii!(5, 5)]);

        // Verify no Interrupted chunks remain after successful write
        let interrupted = storage_module.get_intervals(ChunkType::Interrupted);
        assert_eq!(interrupted, vec![]);

        // Now test that if we manually set a chunk to Interrupted and write intervals,
        // it persists until a new StorageModule loads it
        {
            let mut intervals = storage_module.intervals.write().unwrap();
            let _ = intervals.cut(partition_chunk_offset_ii!(7, 7));
            let _ = intervals.insert_merge_touching_if_values_equal(
                partition_chunk_offset_ii!(7, 7),
                ChunkType::Interrupted,
            );
        }

        // Write the intervals to disk
        storage_module.write_intervals_to_submodules()?;

        // Verify the Interrupted chunk is in memory
        let interrupted = storage_module.get_intervals(ChunkType::Interrupted);
        assert_eq!(interrupted, [partition_chunk_offset_ii!(7, 7)]);

        Ok(())
    }

    #[test]
    fn test_successful_write_updates_from_interrupted_to_actual_type() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("successful_write_test")
            .with_tracing()
            .build();
        let base_path = tmp_dir.path().to_path_buf();

        let infos = [StorageModuleInfo {
            id: 0,
            partition_assignment: None,
            submodules: vec![(partition_chunk_offset_ii!(0, 10), "test-submodule".into())],
        }];

        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 11,
                ..ConsensusConfig::testing()
            }),
            base_directory: base_path,
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);

        let storage_module = StorageModule::new(&infos[0], &config)?;

        // Create test chunks
        let data_chunk = vec![0xDA; 32];
        let entropy_chunk = vec![0xEE; 32];

        // Write a data chunk successfully
        storage_module.write_chunk_internal(
            PartitionChunkOffset::from(3),
            data_chunk,
            ChunkType::Data,
        )?;

        // Write an entropy chunk successfully
        storage_module.write_chunk_internal(
            PartitionChunkOffset::from(7),
            entropy_chunk,
            ChunkType::Entropy,
        )?;

        // Verify the chunks are marked with their correct types
        let data_intervals = storage_module.get_intervals(ChunkType::Data);
        assert_eq!(data_intervals, [partition_chunk_offset_ii!(3, 3)]);

        let entropy_intervals = storage_module.get_intervals(ChunkType::Entropy);
        assert_eq!(entropy_intervals, [partition_chunk_offset_ii!(7, 7)]);

        // Verify no Interrupted chunks remain
        let interrupted = storage_module.get_intervals(ChunkType::Interrupted);
        assert_eq!(interrupted, vec![]);

        // Verify uninitialized chunks are correctly updated
        let uninitialized = storage_module.get_intervals(ChunkType::Uninitialized);
        assert_eq!(
            uninitialized,
            [
                partition_chunk_offset_ii!(0, 2),
                partition_chunk_offset_ii!(4, 6),
                partition_chunk_offset_ii!(8, 10),
            ]
        );

        // Verify we can read the chunks back
        let chunks = storage_module.read_chunks(partition_chunk_offset_ii!(3, 7))?;
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks.get(&PartitionChunkOffset::from(3)).unwrap().1,
            ChunkType::Data
        );
        assert_eq!(
            chunks.get(&PartitionChunkOffset::from(7)).unwrap().1,
            ChunkType::Entropy
        );

        Ok(())
    }

    #[test]
    fn test_interrupted_chunks_reset_on_load() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("interrupted_load_test")
            .with_tracing()
            .build();
        let base_path = tmp_dir.path().to_path_buf();

        let infos = [StorageModuleInfo {
            id: 0,
            partition_assignment: None,
            submodules: vec![(partition_chunk_offset_ii!(0, 10), "test-submodule".into())],
        }];

        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 11,
                ..ConsensusConfig::testing()
            }),
            base_directory: base_path,
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config.clone());

        {
            let storage_module = StorageModule::new(&infos[0], &config)?;

            // Manually set some intervals to Interrupted (simulating a crash during write)
            {
                let mut intervals = storage_module.intervals.write().unwrap();
                StorageModule::cut_then_insert_interval_if_touching(
                    &mut intervals,
                    PartitionChunkOffset::from(3),
                    ChunkType::Interrupted,
                );
                StorageModule::cut_then_insert_interval_if_touching(
                    &mut intervals,
                    PartitionChunkOffset::from(7),
                    ChunkType::Interrupted,
                );
                StorageModule::cut_then_insert_interval_if_touching(
                    &mut intervals,
                    PartitionChunkOffset::from(8),
                    ChunkType::Interrupted,
                );
            }

            // Write the intervals to disk
            storage_module.write_intervals_to_submodules()?;

            // Verify Interrupted chunks are set
            let interrupted = storage_module.get_intervals(ChunkType::Interrupted);
            assert_eq!(
                interrupted,
                [
                    partition_chunk_offset_ii!(3, 3),
                    partition_chunk_offset_ii!(7, 8),
                ]
            );
        }

        // Create a new StorageModule instance (simulating restart)
        let storage_module =
            StorageModule::new(&infos[0], &Config::new_with_random_peer_id(node_config))?;

        // Verify that Interrupted chunks are reset to Uninitialized on load
        let interrupted = storage_module.get_intervals(ChunkType::Interrupted);
        assert_eq!(
            interrupted,
            vec![],
            "Interrupted chunks should be reset on load"
        );

        // Verify all chunks are now Uninitialized
        let uninitialized = storage_module.get_intervals(ChunkType::Uninitialized);
        assert_eq!(uninitialized, [partition_chunk_offset_ii!(0, 10)]);

        Ok(())
    }

    #[ignore]
    #[test]
    // note: this requires you to change the submodule database args to set the growth and shrink step to 1 and 2 respectively to produce accurate results
    // IT ALSO KEEPS THE TEST DIR
    fn mdbx_metadata_size_test() -> eyre::Result<()> {
        // SAFETY: test is single-threaded; setting env var before any tracing init.
        unsafe { std::env::set_var("RUST_LOG", "info") };
        let tmp_dir = TempDirBuilder::new()
            .prefix("data_path_test")
            .keep()
            .with_tracing()
            .build();

        let base_path = tmp_dir.path().to_path_buf();
        let chunk_size = 1;

        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: 10_000,
                ..ConsensusConfig::testing()
            }),
            base_directory: base_path.clone(),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1000,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 0,
                ..StorageSyncConfig::default()
            },
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);

        let infos = [StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment::default()),
            submodules: vec![(
                partition_chunk_offset_ii!(0, config.consensus.num_chunks_in_partition - 1),
                "hdd0".into(),
            )],
        }];

        // Create a StorageModule with the specified submodules and config
        let storage_module_info = &infos[0];
        let storage_module = StorageModule::new(storage_module_info, &config)?;

        storage_module.pack_with_zeros();

        // create & write 100_000 chunks worth of txs
        // randomly select the size in chunks for the tx
        // assume we have 100 txs/block
        // so we have log2(100) = 6.6 (so 7)
        // 7 32B segments + 1 64B leaf (leaf & note)
        let tx_path = [1; (7 * 32) + 64].to_vec();
        let mut chunks_left = config.consensus.num_chunks_in_partition as u32;
        let mut rng = SimpleRNG::new(42);

        let signer = IrysSigner::random_signer(&config.consensus);
        let mut seed = 0;
        while chunks_left > 0 {
            let chunk_count = rng.next_range(chunks_left).max(1);
            info!("writing {chunk_count} chunks.. ({chunks_left} left)");
            let tx = signer.create_transaction_from_iter(
                chunk_bytes_gen(chunk_count as u64, chunk_size as usize, seed),
                H256::zero(),
                true,
            )?;

            let _ = storage_module.index_transaction_data(
                &tx.header,
                &tx_path,
                LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
                0,
            );

            for chunk in tx.data_chunks()? {
                storage_module.write_data_chunk(&chunk)?;
            }

            seed += 1;
            chunks_left = chunks_left.saturating_sub(chunk_count);
        }

        let db_path = base_path
            .join(
                storage_module
                    .submodules
                    .first_key_value()
                    .unwrap()
                    .1
                    .path
                    .clone(),
            )
            .join("db");
        info!("DB PATH {:?}", &db_path.canonicalize()?);

        Ok(())
    }

    #[test]
    fn failed_sync_keeps_batch_pending_for_retry() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("sync_retry_test")
            .with_tracing()
            .build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 4,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 0,
                ..StorageSyncConfig::default()
            },
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage_module = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(PartitionAssignment::default()),
                submodules: vec![(partition_chunk_offset_ii!(0, 3), "chunks".into())],
            },
            &config,
        )?;

        for (raw_offset, failure_point) in [
            (0_u32, SyncFailurePoint::BeforeDataFsync),
            (1_u32, SyncFailurePoint::BeforeIntervalCommit),
        ] {
            let offset = PartitionChunkOffset::from(raw_offset);
            storage_module.write_chunk(offset, vec![raw_offset as u8; 32], ChunkType::Data);
            storage_module.fail_next_sync_at(failure_point);

            assert!(storage_module.force_sync_pending_chunks().is_err());
            assert!(storage_module.has_pending_writes());
            assert!(!storage_module.is_data_chunk_durable_at(offset));

            storage_module.force_sync_pending_chunks()?;
            assert!(storage_module.is_data_chunk_durable_at(offset));
            assert!(!storage_module.has_pending_writes());
        }

        Ok(())
    }

    fn recall_flush_fixture(
        prefix: &str,
        drop_pages: bool,
    ) -> eyre::Result<(irys_testing_utils::tempfile::TempDir, StorageModule)> {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let chunk_size = 32;
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: 8,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1000,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 60_000,
                entropy_sweep_max_bytes: chunk_size,
                drop_recall_page_cache: drop_pages,
                ..StorageSyncConfig::default()
            },
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage_module = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(PartitionAssignment::default()),
                submodules: vec![(partition_chunk_offset_ii!(0, 7), "chunks".into())],
            },
            &config,
        )?;
        for offset in 0..6_u32 {
            storage_module.write_chunk(
                PartitionChunkOffset::from(offset),
                vec![offset as u8; chunk_size as usize],
                ChunkType::Data,
            );
        }
        Ok((tmp_dir, storage_module))
    }

    fn assert_recall_window(storage_module: &StorageModule, durable_through: u32) {
        for offset in 0..6_u32 {
            let point = PartitionChunkOffset::from(offset);
            if offset < durable_through {
                assert!(
                    storage_module.is_data_chunk_durable_at(point),
                    "offset {offset} should be durable"
                );
            } else {
                assert!(
                    !storage_module.is_data_chunk_durable_at(point),
                    "offset {offset} should still be pending"
                );
                assert!(
                    storage_module
                        .pending_writes
                        .read()
                        .unwrap()
                        .contains_key(&point)
                );
            }
        }
    }

    #[test]
    fn recall_flushes_one_write_window() -> eyre::Result<()> {
        let (_tmp, storage_module) = recall_flush_fixture("recall_flush_window", false)?;
        storage_module.read_recall_chunks(partition_chunk_offset_ii!(0, 0))?;
        assert_recall_window(&storage_module, 3);
        // Below the sync threshold the rest stays pending.
        storage_module.sync_pending_chunks()?;
        assert_recall_window(&storage_module, 3);
        storage_module.read_recall_chunks(partition_chunk_offset_ii!(0, 0))?;
        assert!(!storage_module.has_pending_writes());
        assert_recall_window(&storage_module, 6);
        Ok(())
    }

    #[test]
    fn recall_page_cache_drops_synced_runs() -> eyre::Result<()> {
        let (_tmp, storage_module) = recall_flush_fixture("recall_drop_pages", true)?;
        storage_module.force_sync_pending_chunks()?;
        let got = storage_module.read_recall_chunks(partition_chunk_offset_ii!(0, 5))?;
        assert_eq!(got.len(), 6);
        assert_eq!(
            storage_module
                .disk
                .recall_cache_drops
                .load(Ordering::SeqCst),
            1
        );
        Ok(())
    }

    #[test]
    fn recall_page_cache_stays_when_the_flag_is_off() -> eyre::Result<()> {
        let (_tmp, storage_module) = recall_flush_fixture("recall_keep_pages", false)?;
        storage_module.force_sync_pending_chunks()?;
        let got = storage_module.read_recall_chunks(partition_chunk_offset_ii!(0, 5))?;
        assert_eq!(got.len(), 6);
        assert_eq!(
            storage_module
                .disk
                .recall_cache_drops
                .load(Ordering::SeqCst),
            0
        );
        Ok(())
    }

    #[test]
    fn recall_page_cache_skips_a_run_with_a_packed_write() -> eyre::Result<()> {
        let (_tmp, storage_module) = recall_flush_fixture("recall_drop_pending", true)?;
        storage_module.force_sync_pending_chunks()?;
        assert!(storage_module.write_chunk(
            PartitionChunkOffset::from(2),
            vec![9_u8; 32],
            ChunkType::Data,
        ));
        storage_module.drop_recall_page_cache(&[(
            PartitionChunkOffset::from(0),
            6,
            ChunkType::Data,
        )]);
        assert_eq!(
            storage_module
                .disk
                .recall_cache_drops
                .load(Ordering::SeqCst),
            0
        );
        storage_module.force_sync_pending_chunks()?;
        storage_module.drop_recall_page_cache(&[(
            PartitionChunkOffset::from(0),
            6,
            ChunkType::Data,
        )]);
        assert_eq!(
            storage_module
                .disk
                .recall_cache_drops
                .load(Ordering::SeqCst),
            1
        );
        Ok(())
    }

    fn wait_for_pending_drain(storage_module: &StorageModule) {
        let started = Instant::now();
        while storage_module.has_pending_writes() {
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "packed chunks stayed pending"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn lane_writes_a_short_run_on_an_idle_disk() -> eyre::Result<()> {
        let chunk_size = 256 * 1024;
        let (_tmp, storage_module) = seek_run_fixture("lane_idle_short", chunk_size, 10_000, 8)?;
        let body = vec![1_u8; chunk_size as usize];
        for offset in 0..6_u32 {
            storage_module.write_chunk(
                PartitionChunkOffset::from(offset),
                body.clone(),
                ChunkType::Data,
            );
        }
        let storage_module = Arc::new(storage_module);
        let lane = LaneGuard::start(Arc::clone(&storage_module))?;
        let started = Instant::now();
        while lane.module.has_pending_writes() {
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "idle disk left the short run queued"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            lane.module
                .is_data_chunk_durable_at(PartitionChunkOffset::from(0))
        );
        assert!(
            lane.module
                .is_data_chunk_durable_at(PartitionChunkOffset::from(5))
        );
        Ok(())
    }

    #[test]
    fn lane_writes_every_short_run_on_an_idle_disk() -> eyre::Result<()> {
        let chunk_size = 256 * 1024;
        let (_tmp, storage_module) = seek_run_fixture("lane_idle_shorts", chunk_size, 10_000, 8)?;
        let body = vec![1_u8; chunk_size as usize];
        storage_module.write_chunk(PartitionChunkOffset::from(0), body.clone(), ChunkType::Data);
        storage_module.write_chunk(PartitionChunkOffset::from(2), body, ChunkType::Data);
        let older = Instant::now()
            .checked_sub(Duration::from_secs(30))
            .expect("test clock");
        let newer = Instant::now()
            .checked_sub(Duration::from_secs(20))
            .expect("test clock");
        {
            let mut pending = storage_module.pending_writes.write().unwrap();
            pending
                .queued_at
                .insert(PartitionChunkOffset::from(0), older);
            pending
                .queued_at
                .insert(PartitionChunkOffset::from(2), newer);
        }
        let storage_module = Arc::new(storage_module);
        let lane = LaneGuard::start(Arc::clone(&storage_module))?;
        let started = Instant::now();
        while lane.module.has_pending_writes() {
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "idle disk left a short run queued"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            lane.module
                .is_data_chunk_durable_at(PartitionChunkOffset::from(0))
        );
        assert!(
            lane.module
                .is_data_chunk_durable_at(PartitionChunkOffset::from(2))
        );
        Ok(())
    }

    #[test]
    fn busy_disk_holds_a_young_short_write() -> eyre::Result<()> {
        let (_tmp, storage) = seek_run_fixture("busy_young_write", 32, 10_000, 8)?;
        storage.write_chunk(
            PartitionChunkOffset::from(0),
            vec![1_u8; 32],
            ChunkType::Data,
        );
        assert!(storage.pending_run_ready());
        assert_eq!(storage.longest_ready_write_bytes(), 32);
        storage.disk.occupy_for_test();
        assert!(!storage.pending_run_ready());
        assert_eq!(storage.longest_ready_write_bytes(), 0);
        storage.disk.arm_short_hold(Duration::from_secs(30));
        storage.disk.release_for_test();
        assert!(storage.pending_run_ready());
        assert_eq!(storage.longest_ready_write_bytes(), 32);
        Ok(())
    }

    #[test]
    fn busy_disk_holds_further_short_writes_after_one_aged_run() -> eyre::Result<()> {
        let (_tmp, storage) = seek_run_fixture("busy_aged_write", 32, 10_000, 8)?;
        storage.write_chunk(
            PartitionChunkOffset::from(0),
            vec![1_u8; 32],
            ChunkType::Data,
        );
        storage.write_chunk(
            PartitionChunkOffset::from(2),
            vec![2_u8; 32],
            ChunkType::Data,
        );
        let aged = Instant::now()
            .checked_sub(Duration::from_secs(2))
            .expect("test clock");
        {
            let mut pending = storage.pending_writes.write().unwrap();
            pending
                .queued_at
                .insert(PartitionChunkOffset::from(0), aged);
            pending
                .queued_at
                .insert(PartitionChunkOffset::from(2), aged);
        }
        storage.disk.occupy_for_test();
        assert!(storage.pending_run_ready());
        assert_eq!(storage.longest_ready_write_bytes(), 32);
        storage.disk.arm_short_hold(Duration::from_secs(30));
        assert!(!storage.pending_run_ready());
        assert_eq!(storage.longest_ready_write_bytes(), 0);
        storage.disk.release_for_test();
        assert!(storage.pending_run_ready());
        assert_eq!(storage.longest_ready_write_bytes(), 32);
        Ok(())
    }

    #[test]
    fn lane_flushes_short_runs_at_the_sync_threshold() -> eyre::Result<()> {
        let (_tmp, storage_module) = seek_run_fixture("lane_threshold", 32, 4, 8)?;
        for offset in 0..4_u32 {
            storage_module.write_chunk(
                PartitionChunkOffset::from(offset),
                vec![offset as u8; 32],
                ChunkType::Data,
            );
        }
        let storage_module = Arc::new(storage_module);
        let lane = LaneGuard::start(Arc::clone(&storage_module))?;
        wait_for_pending_drain(&lane.module);
        for offset in 0..4_u32 {
            assert!(
                lane.module
                    .is_data_chunk_durable_at(PartitionChunkOffset::from(offset))
            );
        }
        Ok(())
    }

    #[test]
    fn lane_flushes_a_full_run() -> eyre::Result<()> {
        let chunk_size = 256 * 1024;
        let (_tmp, storage_module) = seek_run_fixture("lane_full_run", chunk_size, 10_000, 80)?;
        let body = vec![1_u8; chunk_size as usize];
        for offset in 0..40_u32 {
            storage_module.write_chunk(
                PartitionChunkOffset::from(offset),
                body.clone(),
                ChunkType::Data,
            );
        }
        let storage_module = Arc::new(storage_module);
        let lane = LaneGuard::start(Arc::clone(&storage_module))?;
        let started = Instant::now();
        while lane.module.has_pending_writes() {
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "full run stayed pending"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            lane.module
                .is_data_chunk_durable_at(PartitionChunkOffset::from(0))
        );
        assert!(
            lane.module
                .is_data_chunk_durable_at(PartitionChunkOffset::from(39))
        );
        Ok(())
    }

    #[test]
    fn lane_holds_a_short_run_while_the_disk_is_busy() -> eyre::Result<()> {
        let chunk_size = 256 * 1024;
        let (_tmp, storage_module) = seek_run_fixture("lane_under_cap", chunk_size, 10_000, 80)?;
        let body = vec![1_u8; chunk_size as usize];
        for offset in 0..39_u32 {
            storage_module.write_chunk(
                PartitionChunkOffset::from(offset),
                body.clone(),
                ChunkType::Data,
            );
        }
        storage_module.disk.occupy_for_test();
        let storage_module = Arc::new(storage_module);
        let lane = LaneGuard::start(Arc::clone(&storage_module))?;
        std::thread::sleep(Duration::from_millis(200));
        assert!(lane.module.has_pending_writes());
        assert!(
            !lane
                .module
                .is_data_chunk_durable_at(PartitionChunkOffset::from(0))
        );
        lane.module.disk.release_for_test();
        let started = Instant::now();
        while lane.module.has_pending_writes() {
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "idle disk left the short run queued"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            lane.module
                .is_data_chunk_durable_at(PartitionChunkOffset::from(0))
        );
        assert!(
            lane.module
                .is_data_chunk_durable_at(PartitionChunkOffset::from(38))
        );
        Ok(())
    }

    #[test]
    fn lane_pauses_writes_while_recall_holds_the_disk() -> eyre::Result<()> {
        let chunk_size = 256 * 1024;
        let (_tmp, storage_module) =
            seek_run_fixture("lane_pause_for_recall", chunk_size, 10_000, 8)?;
        let body = vec![1_u8; chunk_size as usize];
        for offset in 0..6_u32 {
            storage_module.write_chunk(
                PartitionChunkOffset::from(offset),
                body.clone(),
                ChunkType::Data,
            );
        }
        let storage_module = Arc::new(storage_module);
        let hold = storage_module.disk.begin_recall();
        let lane = LaneGuard::start(Arc::clone(&storage_module))?;
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !lane
                .module
                .is_data_chunk_durable_at(PartitionChunkOffset::from(0))
        );
        drop(hold);
        let started = Instant::now();
        while lane.module.has_pending_writes() {
            assert!(
                started.elapsed() < Duration::from_secs(15),
                "idle disk left the short run queued after recall"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            lane.module
                .is_data_chunk_durable_at(PartitionChunkOffset::from(0))
        );
        Ok(())
    }

    fn submodule_interval_path(module: &StorageModule, nth: usize) -> std::path::PathBuf {
        module
            .submodules
            .iter()
            .nth(nth)
            .expect("submodule")
            .1
            .intervals_file
            .lock()
            .expect("intervals path")
            .clone()
    }

    fn two_disk_fixture(
        prefix: &str,
        writes_before_sync: u64,
    ) -> eyre::Result<(irys_testing_utils::tempfile::TempDir, StorageModule)> {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 10,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: writes_before_sync,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 60_000,
                entropy_sweep_max_bytes: 32,
                ..StorageSyncConfig::default()
            },
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(PartitionAssignment::default()),
                submodules: vec![
                    (partition_chunk_offset_ii!(0, 4), "hdd0".into()),
                    (partition_chunk_offset_ii!(5, 9), "hdd1".into()),
                ],
            },
            &config,
        )?;
        Ok((tmp_dir, storage))
    }

    #[test]
    fn interval_file_follows_the_written_submodule() -> eyre::Result<()> {
        let (_tmp, storage) = two_disk_fixture("interval_touched", 1)?;
        let untouched = submodule_interval_path(&storage, 1);
        std::fs::write(&untouched, b"untouched-marker")?;
        storage.write_chunk(
            PartitionChunkOffset::from(0),
            vec![7_u8; 32],
            ChunkType::Data,
        );
        storage.sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(PartitionChunkOffset::from(0)));
        assert_eq!(
            storage.disk.last_commit_thread_id(),
            Some(std::thread::current().id())
        );
        let written = read_intervals_file(&submodule_interval_path(&storage, 0))?;
        assert_eq!(
            written.get_at_point(PartitionChunkOffset::from(0)).copied(),
            Some(ChunkType::Data)
        );
        assert_eq!(std::fs::read(&untouched)?, b"untouched-marker");
        Ok(())
    }

    #[test]
    fn lane_persists_intervals_for_an_external_flush() -> eyre::Result<()> {
        // An idle disk writes the young chunk on the lane thread. The
        // external force waits on that same thread. The other submodule
        // file stays as it was.
        let (_tmp, storage) = two_disk_fixture("interval_lane", 10_000)?;
        let untouched = submodule_interval_path(&storage, 1);
        std::fs::write(&untouched, b"untouched-marker")?;
        storage.write_chunk(
            PartitionChunkOffset::from(0),
            vec![7_u8; 32],
            ChunkType::Data,
        );
        let storage = Arc::new(storage);
        let lane = LaneGuard::start(Arc::clone(&storage))?;
        let module = Arc::clone(&lane.module);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(module.force_sync_pending_chunks());
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("disk lane did not finish the external flush")?;
        assert!(
            lane.module
                .is_data_chunk_durable_at(PartitionChunkOffset::from(0))
        );
        let lane_id = lane.module.disk.lane_thread_id().expect("lane thread");
        assert_eq!(lane.module.disk.last_commit_thread_id(), Some(lane_id));
        assert_ne!(lane_id, std::thread::current().id());
        let written = read_intervals_file(&submodule_interval_path(&lane.module, 0))?;
        assert_eq!(
            written.get_at_point(PartitionChunkOffset::from(0)).copied(),
            Some(ChunkType::Data)
        );
        assert_eq!(std::fs::read(&untouched)?, b"untouched-marker");
        Ok(())
    }

    #[test]
    fn coalesced_window_joins_adjacent_chunks_and_keeps_the_long_run() -> eyre::Result<()> {
        let chunk_size = 32u64;
        let (_tmp, storage) = coalesce_fixture("coalesce_window", chunk_size)?;
        let body = |offset: u32| vec![offset as u8; chunk_size as usize];
        for offset in 0..8_u32 {
            storage.write_chunk(
                PartitionChunkOffset::from(offset),
                body(offset),
                ChunkType::Data,
            );
        }
        for offset in [100_u32, 102, 104] {
            storage.write_chunk(
                PartitionChunkOffset::from(offset),
                body(offset),
                ChunkType::Data,
            );
        }

        let cap = chunk_size * 4;
        let pending = storage.pending_writes.read().unwrap();
        let batch = storage.select_coalesced_window(&pending, 2, 512, cap, false);
        let mut chosen: Vec<u32> = batch.iter().map(|(offset, _, _)| offset.0).collect();
        chosen.sort_unstable();
        assert_eq!(chosen, (0..8).collect::<Vec<_>>());
        let runs = storage.plan_write_runs(&batch, cap);
        assert_eq!(runs.len(), 2);
        assert!(runs.iter().all(|run| run.offsets.len() == 4));

        let scanned = storage.select_coalesced_window(&pending, 4, 3, chunk_size * 8, false);
        let scanned_offsets: Vec<u32> = scanned.iter().map(|(offset, _, _)| offset.0).collect();
        assert_eq!(scanned_offsets, vec![0, 1, 2]);
        drop(pending);

        storage.write_chunk(PartitionChunkOffset::from(50), body(50), ChunkType::Entropy);
        let pending = storage.pending_writes.read().unwrap();
        let long_first = storage.select_coalesced_window(&pending, 1, 512, cap, false);
        let long_offsets: Vec<u32> = long_first.iter().map(|(offset, _, _)| offset.0).collect();
        assert_eq!(long_offsets, vec![0, 1, 2, 3]);
        drop(pending);

        let aged_at = Instant::now()
            .checked_sub(Duration::from_secs(2))
            .expect("test clock");
        {
            let mut pending = storage.pending_writes.write().unwrap();
            pending
                .queued_at
                .insert(PartitionChunkOffset::from(50), aged_at);
            let jumped = storage.select_coalesced_window(&pending, 1, 512, cap, false);
            assert_eq!(jumped.len(), 1);
            assert_eq!(jumped[0].0, PartitionChunkOffset::from(50));
        }

        let (_hole_tmp, holed) = coalesce_fixture("coalesce_hole", chunk_size)?;
        for offset in [0_u32, 1, 2, 4] {
            holed.write_chunk(
                PartitionChunkOffset::from(offset),
                body(offset),
                ChunkType::Data,
            );
        }
        let pending = holed.pending_writes.read().unwrap();
        let wide = chunk_size * 8;
        let batch = holed.select_coalesced_window(&pending, 4, 16, wide, false);
        let runs = holed.plan_write_runs(&batch, wide);
        let lengths: Vec<usize> = runs.iter().map(|run| run.offsets.len()).collect();
        assert_eq!(lengths, vec![3, 1]);
        Ok(())
    }

    #[test]
    fn one_short_window_keeps_full_runs_and_the_oldest() -> eyre::Result<()> {
        let chunk_size = 32_u64;
        let (_tmp, storage) = coalesce_fixture("one_short_window", chunk_size)?;
        let body = |offset: u32| vec![offset as u8; chunk_size as usize];
        for offset in 0..4_u32 {
            storage.write_chunk(
                PartitionChunkOffset::from(offset),
                body(offset),
                ChunkType::Data,
            );
        }
        storage.write_chunk(PartitionChunkOffset::from(10), body(10), ChunkType::Data);
        storage.write_chunk(PartitionChunkOffset::from(20), body(20), ChunkType::Data);
        let older = Instant::now()
            .checked_sub(Duration::from_secs(3))
            .expect("test clock");
        let newer = Instant::now()
            .checked_sub(Duration::from_secs(2))
            .expect("test clock");
        let mut pending = storage.pending_writes.write().unwrap();
        pending
            .queued_at
            .insert(PartitionChunkOffset::from(20), older);
        pending
            .queued_at
            .insert(PartitionChunkOffset::from(10), newer);
        let cap = chunk_size * 4;
        let batch = storage.select_coalesced_window(&pending, 8, 512, cap, true);
        let offsets: Vec<u32> = batch.iter().map(|(offset, _, _)| offset.0).collect();
        assert_eq!(offsets, vec![20, 0, 1, 2, 3]);

        storage.disk.arm_short_hold(Duration::from_secs(30));
        let held = storage.select_coalesced_window(&pending, 8, 512, cap, true);
        let held_offsets: Vec<u32> = held.iter().map(|(offset, _, _)| offset.0).collect();
        assert_eq!(held_offsets, vec![0, 1, 2, 3]);
        Ok(())
    }

    fn seek_run_fixture(
        prefix: &str,
        chunk_size: u64,
        writes_before_sync: u64,
        last_offset: u32,
    ) -> eyre::Result<(irys_testing_utils::tempfile::TempDir, StorageModule)> {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: u64::from(last_offset) + 1,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: writes_before_sync,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 60_000,
                entropy_sweep_max_bytes: chunk_size,
                ..StorageSyncConfig::default()
            },
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(PartitionAssignment::default()),
                submodules: vec![(partition_chunk_offset_ii!(0, last_offset), "chunks".into())],
            },
            &config,
        )?;
        Ok((tmp_dir, storage))
    }

    fn coalesce_fixture(
        prefix: &str,
        chunk_size: u64,
    ) -> eyre::Result<(irys_testing_utils::tempfile::TempDir, StorageModule)> {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: 200,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1000,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 60_000,
                entropy_sweep_max_bytes: chunk_size,
                ..StorageSyncConfig::default()
            },
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(PartitionAssignment::default()),
                submodules: vec![(partition_chunk_offset_ii!(0, 199), "chunks".into())],
            },
            &config,
        )?;
        Ok((tmp_dir, storage))
    }
}
