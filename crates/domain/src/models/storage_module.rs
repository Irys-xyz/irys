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

use atomic_write_file::AtomicWriteFile;
use derive_more::derive::{Deref, DerefMut};
use eyre::{Context as _, OptionExt as _, Result, ensure, eyre};
#[cfg(feature = "bench")]
use irys_database::submodule::RocksTuning;
#[cfg(test)]
use irys_database::submodule::tables::SUBMODULE_SCHEMA_V3;
use irys_database::submodule::{
    SubmoduleIndex, SubmoduleRead, SubmoduleStore as _, install_process_rocks_resources,
    tables::{
        DataRootInfo, DataRootInfos, OffsetMeta, PackedOffsetRow, PendingBodyMigration, Placement,
        TxLeafBinding,
    },
};
use irys_packing::capacity_single::compute_entropy_chunk;
use irys_packing::unpack;
use irys_types::{
    Base64, ChunkBytes, ChunkDataPath, ChunkPathHash, Config, DataLedger, DataRoot,
    DataTransactionHeader, DataTransactionLedger, DatabaseConfig, H256, IrysAddress,
    LedgerChunkOffset, LedgerChunkRange, PackedChunk, PartitionChunkOffset, PartitionChunkRange,
    ProofDeserialize as _, RelativeChunkOffset, SubmoduleIndexEngine, TxChunkOffset, TxPath,
    UnpackedChunk, get_leaf_proof, ledger_chunk_offset_ie,
    partition::{PartitionAssignment, PartitionHash},
    partition_chunk_offset_ii,
};
use nodit::{
    DiscreteFinite as _, InclusiveInterval as _, Interval, NoditMap, NoditSet, interval::ii,
};
use openssl::sha;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::sync::Condvar;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{self, File, OpenOptions},
    io::{Read as _, Seek as _, SeekFrom, Write as _},
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Once, RwLock,
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

#[path = "chunk_crc.rs"]
mod chunk_crc;

#[path = "recall_direct.rs"]
mod recall_direct;
pub use recall_direct::open_recall_direct;

#[cfg(feature = "bench")]
#[path = "split_backing.rs"]
mod split_backing;
#[cfg(feature = "bench")]
pub use split_backing::SplitBacking;

type SubmodulePath = PathBuf;

/// Real `data_root` for a stored tx path, checked against the proof leaf.
///
/// `Ok(None)` when the tx-path bytes are absent. A missing binding, or a leaf
/// that does not match the stored fold, is corruption.
fn data_root_for_tx_path_hash(
    tx: &dyn SubmoduleRead,
    tx_path_hash: irys_types::TxPathHash,
) -> eyre::Result<Option<DataRoot>> {
    index_read_metrics::note("tx_path");
    let Some(tx_path) = tx.get_full_tx_path(tx_path_hash)? else {
        return Ok(None);
    };
    index_read_metrics::note("tx_leaf");

    let leaf = get_leaf_proof(&Base64::from(tx_path))?
        .hash()
        .map(H256::from)
        .ok_or_eyre("Unable to parse tx_path leaf hash")?;

    let binding = tx
        .get_tx_leaf_binding(tx_path_hash)?
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
fn metas_in_tx(
    tx: &dyn SubmoduleRead,
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
    chunk_size: u64,
) -> eyre::Result<BTreeMap<PartitionChunkOffset, (DataRoot, u64, Base64, TxChunkOffset, Option<u32>)>>
{
    index_read_metrics::note("offset_walk");
    let rows = tx.path_hashes_in_inclusive_range(start, end)?;
    let placements = tx.placements_in_range(start, end)?;
    let mut noted_roots: HashMap<DataRoot, ()> = HashMap::new();
    for placement in &placements {
        index_read_metrics::note("tx_path");
        index_read_metrics::note("tx_leaf");
        if noted_roots.insert(placement.data_root, ()).is_none() {
            index_read_metrics::note("data_root");
        }
    }
    let mut tx_roots: HashMap<irys_types::TxPathHash, Option<DataRoot>> = HashMap::new();
    let mut infos: HashMap<DataRoot, DataRootInfos> = HashMap::new();
    let mut data_paths: HashMap<ChunkPathHash, Option<Base64>> = HashMap::new();
    let mut out = BTreeMap::new();
    let mut placement_index = 0;
    for (offset, hashes) in rows {
        let Some(tx_path_hash) = hashes.tx_path_hash else {
            continue;
        };
        while placement_index < placements.len() && placements[placement_index].end < offset {
            placement_index += 1;
        }
        let covered = placements
            .get(placement_index)
            .filter(|placement| placement.start <= offset && placement.end >= offset);
        let (data_root, data_size) = if let Some(placement) = covered {
            (placement.data_root, placement.data_size)
        } else {
            // No placement: the path bytes are missing (skip) or the data root
            // has no info (the same error as before).
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
            (data_root, data_size)
        };
        let Some(data_path_hash) = hashes.data_path_hash else {
            continue;
        };
        // The checksum lives on this offset. A shared proof is cached by hash,
        // so a cache hit still reads this offset when the engine stores one.
        // MDBX has no checksum and keeps the single proof get.
        let stored = if tx.stores_offset_crc() {
            tx.get_offset_meta(offset)?
        } else {
            None
        };
        let checksum = stored.as_ref().and_then(|row| row.crc);
        let path_buff = if let Some(cached) = data_paths.get(&data_path_hash) {
            cached.clone()
        } else {
            index_read_metrics::note("data_path");
            let loaded = if tx.stores_offset_crc() {
                stored.and_then(|row| row.proof).map(Base64::from)
            } else {
                tx.get_data_path_by_offset(offset)?.map(Base64::from)
            };
            data_paths.insert(data_path_hash, loaded.clone());
            loaded
        };
        let Some(path_buff) = path_buff else {
            continue;
        };
        let proof = get_leaf_proof(&path_buff)?;
        let tx_offset = match tx_chunk_offset_from_leaf(proof.offset(), chunk_size) {
            Ok(tx_offset) => tx_offset,
            Err(error) => {
                // A zero leaf underflows the chunk index. A leaf past
                // `u32::MAX` chunks does not fit `TxChunkOffset`. Either one
                // used to panic the worker and fail the whole span.
                warn!(
                    partition.offset = offset.0,
                    error = %error,
                    "skipping stored data path with an unusable leaf offset"
                );
                index_read_metrics::note_bad_leaf_offset();
                continue;
            }
        };
        out.insert(
            offset,
            (data_root, data_size, path_buff, tx_offset, checksum),
        );
    }
    Ok(out)
}

fn data_size_for_offset(
    tx: &dyn SubmoduleRead,
    data_root: DataRoot,
    partition_offset: PartitionChunkOffset,
    cache: &mut HashMap<DataRoot, DataRootInfos>,
) -> eyre::Result<u64> {
    if let std::collections::hash_map::Entry::Vacant(entry) = cache.entry(data_root) {
        index_read_metrics::note("data_root");
        let mut loaded = tx
            .get_data_root_infos_for_data_root(data_root)?
            .unwrap_or_default();
        loaded.0.sort_unstable();
        entry.insert(loaded);
    }
    let infos = &cache[&data_root];
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

/// Chunk index of a data-path leaf, or an error when the leaf note is 0 or
/// the index does not fit in `u32`.
fn tx_chunk_offset_from_leaf(
    leaf_byte_offset: usize,
    chunk_size: u64,
) -> eyre::Result<TxChunkOffset> {
    let bytes = u64::try_from(leaf_byte_offset)
        .map_err(|_| eyre!("data path leaf offset does not fit in u64"))?;
    let chunks = bytes.div_ceil(chunk_size);
    let index = chunks
        .checked_sub(1)
        .ok_or_else(|| eyre!("data path leaf offset is 0"))?;
    u32::try_from(index)
        .map(TxChunkOffset::from)
        .map_err(|_| eyre!("data path leaf offset exceeds u32::MAX chunks"))
}

fn paths_in_tx(
    tx: &dyn SubmoduleRead,
    start: PartitionChunkOffset,
    end: PartitionChunkOffset,
) -> eyre::Result<BTreeMap<PartitionChunkOffset, (Option<TxPath>, Option<ChunkDataPath>)>> {
    index_read_metrics::note("offset_walk");
    let rows = tx.path_hashes_in_inclusive_range(start, end)?;
    let mut tx_paths: HashMap<irys_types::TxPathHash, Option<TxPath>> = HashMap::new();
    let mut data_paths: HashMap<ChunkPathHash, Option<ChunkDataPath>> = HashMap::new();
    let mut out = BTreeMap::new();
    for (offset, hashes) in rows {
        // Schema v3 exposes the data root in `tx_path_hash` and stores no tx path.
        let tx_path = match hashes.tx_path_hash {
            Some(hash) if tx.stores_tx_paths() => cached_bytes(&mut tx_paths, hash, || {
                index_read_metrics::note("tx_path");
                tx.get_full_tx_path(hash)
            })?,
            Some(_) | None => None,
        };
        let data_path = match hashes.data_path_hash {
            Some(hash) => cached_bytes(&mut data_paths, hash, || {
                index_read_metrics::note("data_path");
                tx.get_data_path_by_offset(offset)
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

/// Mining recall. `chunks` are the packed bytes. `rows` are the offset records
/// those bytes were checked against, including checksum-only rows.
///
/// `rows` is empty when the index does not store checksums. A later scrub
/// reads the proofs from this map. This read does not check them.
#[derive(Debug, Default)]
pub struct RecallChunks {
    pub chunks: BTreeMap<PartitionChunkOffset, (ChunkBytes, ChunkType)>,
    pub rows: BTreeMap<PartitionChunkOffset, OffsetMeta>,
}

#[derive(Debug, Default)]
struct PendingWrites {
    chunks: ChunkMap,
    /// Offset → generation that reserved it. Only that generation may release.
    occupancy: HashMap<PartitionChunkOffset, u64>,
    /// Who queued each packed chunk. A priority change starts a new run.
    priorities: HashMap<PartitionChunkOffset, disk_lane::WritePriority>,
    /// When each packed chunk was queued. While the disk is busy, a short run
    /// waits out `reorder_grace`. Every aged short run is then eligible. An
    /// idle disk writes them without that wait.
    queued_at: HashMap<PartitionChunkOffset, Instant>,
    /// Unpacked bytes waiting on the entropy read. Counted in `pending_write_bytes`.
    queued_unpacked_bytes: u64,
    /// Data-path proof for a packed data chunk. The index row, including the
    /// packed-byte checksum, is written only after `chunks.dat` is fdatasync'd.
    /// Entropy has no proof.
    proofs: HashMap<PartitionChunkOffset, (ChunkPathHash, ChunkDataPath)>,
    /// Pwritten bytes not yet covered by a durable point. Still pending: they
    /// count toward `pending_write_bytes` and are not reported durable. The
    /// next point fdatasyncs them, commits presence, then drops them. A failed
    /// `fdatasync` moves the entry back into `chunks` with the saved priority
    /// and `queued_at`, so the next point pwrites it again. A newer enqueue
    /// for the same offset removes the entry so the new bytes are pwritten.
    written_ahead: BTreeMap<PartitionChunkOffset, WrittenAhead>,
}

/// One parked pwrite. `priority` and `queued_at` are the values the chunk had
/// in `pending.chunks`, kept so a failed `fdatasync` can queue the same write.
#[derive(Clone, Debug)]
struct WrittenAhead {
    bytes: ChunkBytes,
    chunk_type: ChunkType,
    priority: disk_lane::WritePriority,
    queued_at: Instant,
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

/// Offsets whose on-disk label must stay `Interrupted` until the completion is
/// stored, or until a clean shutdown writes the live map.
type IntervalMask = NoditSet<PartitionChunkOffset, Interval<PartitionChunkOffset>>;

/// Bytes of `Entropy` or `Uninitialized` marked `Interrupted` ahead of one
/// write, per submodule. A crash reloads that span as `Uninitialized`. The
/// node packs or syncs those chunks again. The run just written is extra.
const INTERVAL_WRITE_AHEAD_BYTES: u64 = 512 * 1024 * 1024;

/// Last `intervals.json` image and the completions that are not in it yet.
///
/// `written` is the raw file, including `Interrupted`. The live interval map
/// is not that image: load turns `Interrupted` into `Uninitialized`, and an
/// ahead mark lives only in `mask`. `get_chunk_type` keeps reading the live
/// map, so mining and migration still see `Entropy` under an ahead mark.
///
/// # State machine
///
/// | Transition | When it is stored | Class | Under 16 MiB/s ingest |
/// | --- | --- | --- | --- |
/// | `Entropy` / `Uninitialized` / `Data` → `Interrupted` for offsets about to be written | Before the `pwrite`, atomic replace | Write-ahead | Was once per durable gap (~0.96/s). A covered span does not write again |
/// | Ahead `Entropy` / `Uninitialized` → `Interrupted` on disk only | Same rewrite, up to 512 MiB forward inside the submodule | Write-ahead coalesce | About one rewrite per 512 MiB (~0.03/s) while the frontier is contiguous |
/// | `Interrupted` → `Data` or `Entropy` | After `fdatasync` and the presence commit. At most every `intervals_persist_secs`, sooner on the next write-ahead rewrite, on `force_sync`, or on clean shutdown | Completion | Was ~0.96/s. Now rides the write-ahead or the 30 s timer |
/// | Failed `pwrite` → live `Uninitialized`, file stays `Interrupted` | No extra rewrite. Load maps both to `Uninitialized` | Completion lag is safe | Rare |
/// | Any → `Uninitialized` on reset | After the index clear. A metadata marker covers a crash before the file rewrite | Write-ahead | Rare |
/// | `index_transaction_data`, group commit, chunk `fdatasync` | Not an interval rewrite | Durable point is the chunk `fdatasync` plus the presence commit | `fdatasync` stays ~0.96/s |
///
/// `intervals_persist_secs = 0` does both file rewrites on every sync and
/// does not mark ahead. Offsets still queued or parked are stored as
/// `Interrupted` in that image. A completion of a point that already
/// fsynced those offsets stores the live label.
///
/// A completion is never stored before its chunk `fdatasync` and presence
/// commit. A write-ahead mark is never dropped or delayed. Clean shutdown
/// force-syncs first, so nothing is pending, then writes the live map and
/// drops the ahead mask. A restart does not turn valid entropy into
/// `Uninitialized`. A crash leaves the mask on disk. An image built from
/// the live map keeps `Interrupted` for offsets still in `pending.chunks`
/// or `pending.written_ahead`, and keeps those offsets in the mask.
///
/// The packed-byte checksum is not a file. It is committed in the same index
/// batch as the presence row, after the chunk `fdatasync` and before the
/// completion label. The `Interrupted` mark still covers the bytes: a crash
/// reloads that span as `Uninitialized` and drops the row.
#[derive(Debug)]
struct IntervalDisk {
    written: StorageIntervals,
    mask: IntervalMask,
    /// Submodule starts whose live types are ahead of `written`.
    dirty: BTreeSet<PartitionChunkOffset>,
    /// Monotonic time of the last completion rewrite. The first wait starts
    /// at load, so opening a module does not rewrite the file.
    last_completion: BTreeMap<PartitionChunkOffset, Instant>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SyncFailurePoint {
    BeforeDataFsync,
    BeforeIntervalCommit,
    /// Reset marker is durable. The index clear has not run.
    AfterResetMarker,
    /// Index is cleared. The interval file is not rewritten yet.
    AfterResetClear,
    /// Interval file is `Uninitialized`. The reset marker is still set.
    AfterResetIntervals,
}

/// Test barrier inside a durable point. The point holds `sync_in_progress`.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SyncPausePoint {
    /// Proofs are copied out of `pending`. The index write has not started.
    AfterPresenceCopy,
}

/// One armed pause. The durable point waits until the test releases it.
#[cfg(test)]
#[derive(Debug)]
struct SyncPauseGate {
    point: Mutex<Option<SyncPausePoint>>,
    arrived: Mutex<bool>,
    arrived_cv: Condvar,
    release: Mutex<bool>,
    release_cv: Condvar,
}

/// Test barrier in a registration, after the rows are computed and before
/// they are committed. The registration does not hold `sync_in_progress`
/// or the assignment lock while it waits.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegistrationPausePoint {
    AfterRowsComputed,
}

/// One armed registration pause.
#[cfg(test)]
#[derive(Debug)]
struct RegistrationPauseGate {
    point: Mutex<Option<RegistrationPausePoint>>,
    arrived: Mutex<bool>,
    arrived_cv: Condvar,
    release: Mutex<bool>,
    release_cv: Condvar,
}

#[cfg(test)]
impl RegistrationPauseGate {
    fn new() -> Self {
        Self {
            point: Mutex::new(None),
            arrived: Mutex::new(false),
            arrived_cv: Condvar::new(),
            release: Mutex::new(false),
            release_cv: Condvar::new(),
        }
    }
}

#[cfg(test)]
impl SyncPauseGate {
    fn new() -> Self {
        Self {
            point: Mutex::new(None),
            arrived: Mutex::new(false),
            arrived_cv: Condvar::new(),
            release: Mutex::new(false),
            release_cv: Condvar::new(),
        }
    }
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

/// Why a durable point runs now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DurablePointCause {
    /// `force_sync`, shutdown, or drop.
    Forced,
    /// Pending bytes are above the memory ceiling.
    Ceiling,
    /// Pending bytes crossed the headroom line, or the latch is still closed.
    Headroom,
    /// The interval is zero, this is the first point, or the interval elapsed.
    Interval,
}

impl DurablePointCause {
    fn label(self) -> &'static str {
        match self {
            Self::Forced => "forced",
            Self::Ceiling => "ceiling",
            Self::Headroom => "headroom",
            Self::Interval => "interval",
        }
    }
}

/// Inputs for [`durable_point_decision`]. Durations are monotonic.
#[derive(Clone, Copy)]
pub(super) struct DurableDeferQuery {
    pub force: bool,
    pub interval: Duration,
    pub since_last: Option<Duration>,
    pub pending_bytes: u64,
    pub ceiling_bytes: u64,
    pub headroom_bytes: u64,
    /// `true` while deferral is allowed. A pending-byte level above
    /// `headroom_bytes` stores `false`. It stays `false` until pending bytes
    /// are at or under half the line.
    pub headroom_open: bool,
    /// An owed recall flush must not fdatasync only because headroom is gone.
    pub recall_flush_owed: bool,
}

/// `cause == None` defers the point. `headroom_open` is the latch to store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DurablePointDecision {
    pub cause: Option<DurablePointCause>,
    pub headroom_open: bool,
}

/// Defer only while the interval is the longest wait and the backlog is inside
/// the headroom line. A larger backlog never starts a new deferral. The
/// interval is an upper bound: once it has elapsed the point runs even when
/// the backlog is small. Force, a zero interval, the first point, and the
/// memory ceiling run it immediately. Headroom loss does too, except while a
/// recall is waiting on the owed flush.
pub(super) fn durable_point_decision(query: DurableDeferQuery) -> DurablePointDecision {
    let mut headroom_open = query.headroom_open;
    if query.pending_bytes > query.headroom_bytes {
        headroom_open = false;
    } else if query.pending_bytes <= query.headroom_bytes / 2 {
        headroom_open = true;
    }

    let cause = if query.force {
        Some(DurablePointCause::Forced)
    } else if query.interval.is_zero() || query.since_last.is_none() {
        Some(DurablePointCause::Interval)
    } else if query.pending_bytes > query.ceiling_bytes {
        Some(DurablePointCause::Ceiling)
    } else if query.since_last.expect("checked above") >= query.interval {
        Some(DurablePointCause::Interval)
    } else if !headroom_open && !query.recall_flush_owed {
        Some(DurablePointCause::Headroom)
    } else {
        None
    };
    DurablePointDecision {
        cause,
        headroom_open,
    }
}

/// Durable points since this module was opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct DurablePointCounts {
    /// Ran in the window that wrote the bytes, or cut a parked point short,
    /// because the caller forced it.
    pub immediate_forced: u64,
    /// Same, because pending bytes were above the memory ceiling.
    pub immediate_ceiling: u64,
    /// Same, because pending bytes were over the headroom line.
    pub immediate_headroom: u64,
    /// Same, because the interval was zero, this was the first point, or the
    /// interval had already elapsed and nothing was parked.
    pub immediate_interval: u64,
    /// Parked bytes whose interval then elapsed.
    pub deferred_interval: u64,
}

impl DurablePointCounts {
    pub fn immediate(self) -> u64 {
        self.immediate_forced
            .saturating_add(self.immediate_ceiling)
            .saturating_add(self.immediate_headroom)
            .saturating_add(self.immediate_interval)
    }

    pub fn saturating_delta(self, earlier: Self) -> Self {
        Self {
            immediate_forced: self
                .immediate_forced
                .saturating_sub(earlier.immediate_forced),
            immediate_ceiling: self
                .immediate_ceiling
                .saturating_sub(earlier.immediate_ceiling),
            immediate_headroom: self
                .immediate_headroom
                .saturating_sub(earlier.immediate_headroom),
            immediate_interval: self
                .immediate_interval
                .saturating_sub(earlier.immediate_interval),
            deferred_interval: self
                .deferred_interval
                .saturating_sub(earlier.deferred_interval),
        }
    }
}

#[derive(Debug, Default)]
struct DurablePointAtoms {
    immediate_forced: AtomicU64,
    immediate_ceiling: AtomicU64,
    immediate_headroom: AtomicU64,
    immediate_interval: AtomicU64,
    deferred_interval: AtomicU64,
}

impl DurablePointAtoms {
    fn record(&self, parked: bool, cause: DurablePointCause) {
        let deferred = parked && cause == DurablePointCause::Interval;
        let slot = match (deferred, cause) {
            (true, _) => &self.deferred_interval,
            (false, DurablePointCause::Forced) => &self.immediate_forced,
            (false, DurablePointCause::Ceiling) => &self.immediate_ceiling,
            (false, DurablePointCause::Headroom) => &self.immediate_headroom,
            (false, DurablePointCause::Interval) => &self.immediate_interval,
        };
        slot.fetch_add(1, Ordering::Relaxed);
        let timing = if deferred { "deferred" } else { "immediate" };
        super::index_read_metrics::note_durable_point(timing, cause.label());
    }

    fn snapshot(&self) -> DurablePointCounts {
        DurablePointCounts {
            immediate_forced: self.immediate_forced.load(Ordering::Relaxed),
            immediate_ceiling: self.immediate_ceiling.load(Ordering::Relaxed),
            immediate_headroom: self.immediate_headroom.load(Ordering::Relaxed),
            immediate_interval: self.immediate_interval.load(Ordering::Relaxed),
            deferred_interval: self.deferred_interval.load(Ordering::Relaxed),
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
    /// Set while `reset` has not finished. Heal must not rebuild this index.
    reset_incomplete: AtomicBool,
    #[cfg(any(test, feature = "test-utils"))]
    reset_fail_next: AtomicBool,
    #[cfg(test)]
    fail_entropy_read_nth: AtomicU64,
    #[cfg(test)]
    entropy_read_seq: AtomicU64,
    /// Submodule `view`s opened by a range index read. One slice of the range
    /// is one view, shared by every offset in that slice.
    #[cfg(any(test, feature = "test-utils"))]
    index_views: AtomicU64,
    /// Serializes flushes so two callers cannot claim and write the same
    /// pending batch concurrently. Pending entries remain present until the
    /// data fsync and interval commit both succeed, and therefore serve as the
    /// durability fence themselves. `written_ahead` is that fence after the
    /// pwrite and before the durable point.
    sync_in_progress: Mutex<()>,
    /// Monotonic time of the last successful durable point. `None` until the
    /// first one, so that point is not deferred.
    last_durable_at: Mutex<Option<Instant>>,
    /// `true` while pending bytes are inside the headroom line, so a durable
    /// point may wait out `min_durable_interval_ms`. Crossing the line stores
    /// `false` until pending bytes fall to half the line.
    headroom_open: AtomicBool,
    /// Counts of durable points by timing and cause. The meter exports the
    /// same counts.
    durable_points: DurablePointAtoms,
    /// Set while network-partition recovery rewrites this module's ranges.
    /// Data writes re-check it under the `pending_writes` lock at insert time,
    /// so once it is set no body can be queued behind recovery's
    /// `drop_pending_writes_in_range`. Entropy (packing) writes are unaffected.
    data_writes_paused: AtomicBool,
    #[cfg(test)]
    sync_failure: Mutex<Option<SyncFailurePoint>>,
    /// Pauses one durable point so a test can call `reset` in the gap.
    #[cfg(test)]
    sync_pause: SyncPauseGate,
    /// Pauses one checksum check after the bytes are read and before the
    /// offset row is loaded. The durable point can commit in that gap.
    #[cfg(test)]
    crc_check_pause: SyncPauseGate,
    /// Pauses one registration after its rows are computed and before commit.
    #[cfg(test)]
    registration_pause: RegistrationPauseGate,
    /// Registrations dropped because the assignment changed. Tests read this.
    /// The production counter is `irys.storage.registration_dropped_total`.
    #[cfg(test)]
    registration_drops: AtomicU64,
    /// Monotonic instant of the last pending write, used only for idle flushing.
    last_pending_write: RwLock<Instant>,
    /// Tracks the storage state of each chunk across all submodules
    intervals: Arc<RwLock<StorageIntervals>>,
    /// `intervals.json` image, write-ahead mask, and completion timers.
    interval_disk: Mutex<IntervalDisk>,
    /// Bumped when `reset` publishes a new interval map. A file rewrite that
    /// read the map before the bump does not replace the reset image, and it
    /// does not put the old write-ahead mask back.
    interval_image_epoch: AtomicU64,
    /// Crash tests drop the module without storing batched completions.
    #[cfg(test)]
    suppress_interval_flush: AtomicBool,
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
    /// Settled offsets whose bytes failed the crc check. The storage-module
    /// service drains this into the existing packing queue.
    crc_repairs: Mutex<BTreeSet<PartitionChunkOffset>>,
    /// Recall mismatches not yet checked under `sync_in_progress`. The
    /// mining thread must not take that lock: it still holds the recall,
    /// and a durable point waits on the recall while holding the lock.
    /// The storage-module service confirms these. A module with no lane
    /// confirms them after the recall hold drops.
    crc_suspects: Mutex<BTreeSet<PartitionChunkOffset>>,
    /// Set after a fully settled module has been scanned for stored-zero
    /// entries. Reset clears it. The scan does not run on every durable point.
    crc_scan_done: AtomicBool,
    /// Crash tests drop the module without the final fsync or interval rewrite.
    #[cfg(test)]
    abandon_on_drop: AtomicBool,
    /// Mining-recall preads by mode. The meter exports the same counts.
    #[cfg(test)]
    recall_io: RecallIoAtoms,
}

/// Per-module recall `pread` counts. Parallel tests do not share these.
#[cfg(test)]
#[derive(Debug)]
struct RecallIoAtoms {
    direct: AtomicU64,
    buffered: AtomicU64,
    fallback: AtomicU64,
}

#[cfg(test)]
impl RecallIoAtoms {
    fn new() -> Self {
        Self {
            direct: AtomicU64::new(0),
            buffered: AtomicU64::new(0),
            fallback: AtomicU64::new(0),
        }
    }
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

/// Address, iterations, chunk size, and chain id. The schema-v3 index stores
/// this at open and refuses a later mismatch.
fn packing_identity_hash(
    address: &IrysAddress,
    iterations: u32,
    chunk_size: u64,
    chain_id: u64,
) -> [u8; 32] {
    let mut bytes = Vec::with_capacity(40);
    bytes.extend_from_slice(address.as_slice());
    bytes.extend_from_slice(&iterations.to_be_bytes());
    bytes.extend_from_slice(&chunk_size.to_be_bytes());
    bytes.extend_from_slice(&chain_id.to_be_bytes());
    irys_types::hash_sha256(&bytes)
}

/// Manages chunk storage on a single physical drive
#[derive(Debug)]
pub struct StorageSubmodule {
    /// Submodule index. Callers open a read view or a write batch.
    pub db: SubmoduleIndex,
    /// path to this Submodule
    pub path: PathBuf,
    /// Persistent storage handle. The durability `sync_data` uses this file.
    /// With a bench split this is the write file, never the read-only recall
    /// file, so the presence row is committed only after those bytes are durable.
    file: Arc<Mutex<File>>,
    /// Read-only `O_DIRECT` descriptor for mining recall.
    ///
    /// `None` when `storage.recall_direct_io` is false, or when the open
    /// failed and recall stays on `file` (or the bench recall file). Serving,
    /// the entropy sweep, and packing validation never use this descriptor.
    recall_direct: Option<Arc<File>>,
    /// Cached Metadata `crc_complete`. MDBX stays false: it stores no checksum.
    crc_complete: AtomicBool,
    /// Bench-only second file. `None` on every production open.
    #[cfg(feature = "bench")]
    split: Option<Arc<SplitBacking>>,
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

/// Second look at one checksum suspect. Taken under `sync_in_progress`.
enum CrcConfirm {
    /// The fresh bytes still disagree with the stable checksum row.
    Confirmed,
    /// The fresh bytes agree with the row, or this index stores no checksum.
    NotConfirmed(ChunkBytes),
    /// The offset is in `pending.chunks` or `pending.written_ahead`.
    Unsettled,
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
    /// The bytes are not stored yet. A writer already holds a placement, or
    /// the disk lane gave the disk back before the write finished. The caller
    /// retries later.
    #[error("storage write backpressure")]
    Backpressure,
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
/// clone chunk bytes. The age is read by tests.
struct RunMeta {
    byte_len: u64,
    #[cfg_attr(not(test), expect(dead_code))]
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
    let mut done = 0_u64;
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

/// Byte offset of one recall-file chunk.
///
/// `origin_chunks` is 0 in production. The bench sets it so partition chunk 0
/// is that chunk of the recall file. Buffered and `O_DIRECT` reads share it.
fn recall_file_byte_offset(origin_chunks: u32, submodule_offset: u32, chunk_size: u64) -> u64 {
    u64::from(origin_chunks)
        .saturating_add(u64::from(submodule_offset))
        .saturating_mul(chunk_size)
}

/// Two `pread`s of one recall run share `buffered`. `buffered` is a dup, so
/// these `pread`s do not wait for packed writes already in the kernel.
/// `direct`, when present, is a dup of the `O_DIRECT` descriptor and is the
/// only file mining recall reads when the range is 4096-byte aligned.
/// Chunk bytes land in `chunk_map` after each wave joins. `note` records the
/// mode of each `pread`.
fn read_recall_run(
    buffered: &File,
    direct: Option<&File>,
    direct_requested: bool,
    start: PartitionChunkOffset,
    len: u64,
    chunk_type: ChunkType,
    interval_start: PartitionChunkOffset,
    origin_chunks: u32,
    chunk_size: u64,
    chunk_len: usize,
    max_chunks: u64,
    chunk_map: &mut ChunkMap,
    note: &impl Fn(&'static str),
) -> eyre::Result<()> {
    let pieces = recall_piece_plan(len, max_chunks);
    let mut next = 0;
    while next < pieces.len() {
        let end = (next + RECALL_INFLIGHT).min(pieces.len());
        let mut batch = Vec::with_capacity(end - next);
        for &(done, n) in &pieces[next..end] {
            let piece = PartitionChunkOffset(start.0 + done as u32);
            let file_offset =
                recall_file_byte_offset(origin_chunks, *(piece - interval_start), chunk_size);
            let nbytes = n as usize * chunk_len;
            batch.push((piece, file_offset, n, nbytes));
        }
        let loaded = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(batch.len());
            for (piece, file_offset, n, nbytes) in batch {
                handles.push(scope.spawn(move || {
                    let result = recall_direct::read_recall_piece(
                        buffered,
                        direct,
                        direct_requested,
                        file_offset,
                        nbytes,
                    );
                    (piece, n, result)
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().expect("recall read"))
                .collect::<Vec<_>>()
        });
        for (piece, n, result) in loaded {
            let loaded =
                result.wrap_err_with(|| format!("recall read at offset {piece} count {n}"))?;
            note(loaded.mode);
            let buf = loaded.bytes();
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

fn rocks_index_present(rocks_dir: &Path) -> bool {
    rocks_dir.join("SCHEMA").is_file() || rocks_dir.join("CURRENT").is_file()
}

fn open_submodule_index(
    module_dir: &Path,
    database: &DatabaseConfig,
) -> eyre::Result<SubmoduleIndex> {
    let path = match database.submodule_index {
        SubmoduleIndexEngine::Mdbx => module_dir.join("db"),
        SubmoduleIndexEngine::Rocksdb => module_dir.join("rocks"),
    };
    debug!("submodule_db_path: {:?}", path);

    let opened = match database.submodule_index {
        SubmoduleIndexEngine::Mdbx => {
            let rocks_dir = module_dir.join("rocks");
            let mdbx_dat = path.join("mdbx.dat");
            // Check before open. Open migrates a schema v1 db/ in place, and a
            // stale db/ would hide placements written to rocks/ after conversion.
            if rocks_index_present(&rocks_dir) && mdbx_dat.is_file() {
                Err(eyre!(
                    "refusing to open {}: both indexes exist in {}. {} is present and {} is present. The config selects MDBX, which is also the default when database.submodule_index is unset. After a conversion, rocks/ is the live index, so set database.submodule_index = \"rocksdb\". If MDBX is really intended (a verified `rocks-to-mdbx` rollback), move {} out of the module directory (for example, rename it to rocks.retired) and start again.",
                    path.display(),
                    module_dir.display(),
                    mdbx_dat.display(),
                    rocks_dir.display(),
                    rocks_dir.display()
                ))
            } else if rocks_index_present(&rocks_dir) {
                // An empty MDBX index would hide the live Rocks rows.
                Err(eyre!(
                    "refusing to open {}: {} exists and the Rocks index was not copied. Stop the node and run `submodule-index-convert rocks-to-mdbx` (see --help for --consensus-db and the partition geometry) from {} to {}, then verify. Set database.submodule_index to mdbx only after verify passes.",
                    path.display(),
                    rocks_dir.display(),
                    rocks_dir.display(),
                    path.display()
                ))
            } else {
                // Args (incl. the test geometry cap) derived from the DatabaseConfig.
                let args = irys_database::submodule_db_args(database)?;
                SubmoduleIndex::open_mdbx(&path, args)
            }
        }
        SubmoduleIndexEngine::Rocksdb => {
            let mdbx_dir = module_dir.join("db");
            let mdbx_dat = mdbx_dir.join("mdbx.dat");
            // An empty Rocks index would hide the live MDBX rows.
            if mdbx_dat.is_file() && !rocks_index_present(&path) {
                Err(eyre!(
                    "refusing to open {}: {} exists and the MDBX index was not copied. Stop the node and run `submodule-index-convert mdbx-to-rocks` (see --help for --consensus-db and the partition geometry) from {} to {}, then verify. Set database.submodule_index to rocksdb only after verify passes. Keep db/ until rollback is proven.",
                    path.display(),
                    mdbx_dat.display(),
                    mdbx_dir.display(),
                    path.display()
                ))
            } else {
                // The first submodule installs the process cache. The rest share it.
                let opened = install_process_rocks_resources(database)
                    .and_then(|_| SubmoduleIndex::open_rocks(&path));
                // One line per module. db/ stays for rollback and is not read.
                if opened.is_ok() && mdbx_dat.is_file() {
                    info!(
                        module = %module_dir.display(),
                        db = %mdbx_dir.display(),
                        rocks = %path.display(),
                        "db/ is kept beside rocks/ and is not read"
                    );
                }
                opened
            }
        }
    };

    opened.map_err(|error| {
        eyre!(
            "Failed to create or open submodule database: {} - {}",
            path.display(),
            error
        )
    })
}

struct OpenedChunkFile {
    file: Arc<Mutex<File>>,
    recall_direct: Option<Arc<File>>,
    #[cfg(feature = "bench")]
    split: Option<Arc<SplitBacking>>,
}

/// Which submodule index `open_split` opens. Production `new` uses config.
#[cfg(feature = "bench")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BenchIndex {
    /// `{dir}/db`.
    Mdbx,
    /// `{dir}/rocks`, opened with this preset.
    Rocks(RocksTuning),
}

#[cfg(feature = "bench")]
impl BenchIndex {
    pub fn engine_name(self) -> &'static str {
        match self {
            Self::Mdbx => "mdbx",
            Self::Rocks(_) => "rocks",
        }
    }

    /// Directory name under the submodule path.
    pub fn dir_name(self) -> &'static str {
        match self {
            Self::Mdbx => "db",
            Self::Rocks(_) => "rocks",
        }
    }
}

/// Refuse an index directory that already belongs to the other engine.
///
/// An empty or missing directory is either engine. `mdbx.dat` is MDBX.
/// `CURRENT` or `SCHEMA` is RocksDB.
#[cfg(feature = "bench")]
pub fn refuse_foreign_index(path: &Path, rocks: bool) -> eyre::Result<()> {
    let mdbx = path.join("mdbx.dat").is_file();
    let rocks_files = path.join("CURRENT").is_file() || path.join("SCHEMA").is_file();
    if mdbx && rocks_files {
        return Err(eyre!(
            "index directory {} contains both MDBX and RocksDB files",
            path.display()
        ));
    }
    if rocks && mdbx {
        return Err(eyre!(
            "refusing to open MDBX index {} with RocksDB",
            path.display()
        ));
    }
    if !rocks && rocks_files {
        return Err(eyre!(
            "refusing to open RocksDB index {} with MDBX",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(feature = "bench")]
fn open_mdbx_index(path: &Path, config: &Config) -> eyre::Result<SubmoduleIndex> {
    SubmoduleIndex::open_mdbx(
        path,
        // Args (incl. the test geometry cap) derived from the DatabaseConfig.
        irys_database::submodule_db_args(&config.node_config.database)?,
    )
    .map_err(|error| {
        eyre!(
            "Failed to create or open submodule database: {} - {}",
            path.display(),
            error
        )
    })
}

// libc binds both `posix_fadvise` and `POSIX_FADV_RANDOM` on these targets.
// Apple and the other BSDs do not. A missing advise must not stop the open.
// `AsRawFd` itself is unix-wide: Linux `fallocate` and `fstatvfs` use it.
use std::os::unix::io::AsRawFd as _;

/// `posix_fadvise(RANDOM)` for the whole file. Length 0 means from
/// `offset` to the end. A failed advise is fatal: the descriptor would
/// keep fetching past every large pread.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "emscripten",
    target_os = "l4re",
    target_os = "freebsd",
))]
fn advise_chunks_random(file: &File, path: &Path) -> eyre::Result<()> {
    // Safety: `file` owns a live descriptor. The advice does not change bytes.
    let rc = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_RANDOM) };
    if rc != 0 {
        return Err(eyre!(
            "posix_fadvise(POSIX_FADV_RANDOM) failed on {}: error {rc}",
            path.display()
        ));
    }
    Ok(())
}

/// This target has no `posix_fadvise` binding. The open continues, and
/// read-ahead stays at the kernel default.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "emscripten",
    target_os = "l4re",
    target_os = "freebsd",
)))]
fn advise_chunks_random(_file: &File, path: &Path) -> eyre::Result<()> {
    warn!(
        path = %path.display(),
        os = std::env::consts::OS,
        "posix_fadvise(POSIX_FADV_RANDOM) is not available; chunks.dat keeps the kernel read-ahead"
    );
    Ok(())
}

/// `st_blocks` is in 512-byte units. A hole makes this smaller than `st_size`.
fn allocated_bytes(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    meta.blocks().saturating_mul(512)
}

/// True when the file is shorter than the submodule, or when it already has
/// that length but still has a hole (`st_blocks * 512 < st_size`).
///
/// A file that is already at least `full_len` and has a block for every byte
/// is left alone. XFS `fallocate` mode 0 of a fully allocated file still
/// walks the extent map: `xfs_alloc_file_space` commits one transaction per
/// extent while it holds the inode I/O lock. The maximum XFS extent is 8 GiB,
/// so an 18 TiB file is about 2300 extents. That walk is tens of milliseconds
/// when the log stays in memory, and it can reach seconds when a checkpoint
/// hits a HDD. Every read and write of the file waits for it. Open pays that
/// only for a short file or a hole.
fn chunks_file_needs_reserve(len: u64, allocated: u64, full_len: u64) -> bool {
    len < full_len || allocated < len
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Bytes free for this process (`f_bavail * f_frsize`).
fn filesystem_free_bytes(file: &File) -> eyre::Result<u64> {
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // Safety: `file` owns a live descriptor. On success `fstatvfs` writes
    // the whole struct.
    let rc = unsafe { libc::fstatvfs(file.as_raw_fd(), stat.as_mut_ptr()) };
    if rc != 0 {
        return Err(eyre!(
            "fstatvfs failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // Safety: `rc == 0`, so the kernel filled `stat`.
    let stat = unsafe { stat.assume_init() };
    let frsize = if stat.f_frsize == 0 {
        stat.f_bsize
    } else {
        stat.f_frsize
    };
    let avail = u128::from(stat.f_bavail).saturating_mul(u128::from(frsize));
    Ok(u64::try_from(avail).unwrap_or(u64::MAX))
}

fn no_space(file: &File, path: &Path, wanted: u64) -> eyre::Report {
    match filesystem_free_bytes(file) {
        Ok(free) => eyre!(
            "not enough free space to allocate {}: wanted {wanted} bytes, {free} bytes free",
            path.display()
        ),
        Err(err) => eyre!(
            "not enough free space to allocate {}: wanted {wanted} bytes ({err})",
            path.display()
        ),
    }
}

/// Extend a short file to `full_len`. Does not shrink, and does not reserve
/// blocks for an existing hole.
fn extend_with_set_len(file: &File, path: &Path, full_len: u64) -> eyre::Result<()> {
    let len = file
        .metadata()
        .wrap_err_with(|| format!("stat {}", path.display()))?
        .len();
    if len >= full_len {
        warn!(
            path = %path.display(),
            bytes = full_len,
            len,
            "chunks file stays sparse; this path cannot reserve existing holes"
        );
        return Ok(());
    }
    let started = Instant::now();
    file.set_len(full_len).map_err(|err| {
        if err.raw_os_error() == Some(libc::ENOSPC) {
            no_space(file, path, full_len)
        } else {
            eyre!("set_len({full_len}) failed on {}: {err}", path.display())
        }
    })?;
    let elapsed_ms = elapsed_ms(started);
    info!(
        path = %path.display(),
        bytes = full_len,
        elapsed_ms,
        "set_len chunks file"
    );
    Ok(())
}

/// `fallocate` mode 0. `EOPNOTSUPP` falls back to `set_len`.
#[cfg(target_os = "linux")]
fn fallocate_chunks(file: &File, path: &Path, full_len: u64) -> eyre::Result<()> {
    let len = libc::off_t::try_from(full_len).map_err(|_| {
        eyre!(
            "chunks file {} length {full_len} does not fit in off_t",
            path.display()
        )
    })?;
    let started = Instant::now();
    loop {
        // Safety: `file` owns a live descriptor. Mode 0 allocates unwritten
        // extents for holes and does not rewrite written bytes.
        let rc = unsafe { libc::fallocate(file.as_raw_fd(), 0, 0, len) };
        if rc == 0 {
            let elapsed_ms = elapsed_ms(started);
            info!(
                path = %path.display(),
                bytes = full_len,
                elapsed_ms,
                "fallocate chunks file"
            );
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EOPNOTSUPP) => {
                warn!(
                    path = %path.display(),
                    bytes = full_len,
                    "fallocate is not supported on this filesystem; setting the logical size without reserving blocks"
                );
                return extend_with_set_len(file, path, full_len);
            }
            Some(libc::ENOSPC) => return Err(no_space(file, path, full_len)),
            _ => {
                return Err(eyre!(
                    "fallocate mode 0 of {full_len} bytes failed on {}: {err}",
                    path.display()
                ));
            }
        }
    }
}

/// Reserve `full_len` bytes for the submodule data file.
///
/// Linux calls `fallocate` mode 0. A filesystem without `fallocate` gets
/// `set_len` and a warning. Other operating systems do that directly.
fn allocate_chunks_file(file: &File, path: &Path, full_len: u64) -> eyre::Result<()> {
    if full_len == 0 {
        return Err(eyre!(
            "refusing to allocate a zero-length chunks file at {}",
            path.display()
        ));
    }
    let meta = file
        .metadata()
        .wrap_err_with(|| format!("stat {}", path.display()))?;
    let len = meta.len();
    let allocated = allocated_bytes(&meta);
    if !chunks_file_needs_reserve(len, allocated, full_len) {
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        fallocate_chunks(file, path, full_len)
    }
    #[cfg(not(target_os = "linux"))]
    {
        warn!(
            path = %path.display(),
            bytes = full_len,
            os = std::env::consts::OS,
            "fallocate is not available on this operating system; setting the logical size without reserving blocks"
        );
        extend_with_set_len(file, path, full_len)
    }
}

/// Byte length to reserve for this submodule.
///
/// The inclusive interval is the production file. A bench split reserves the
/// write-region window instead: that file is not the partition `chunks.dat`.
fn submodule_reserve_len(
    interval: &Interval<PartitionChunkOffset>,
    chunk_size: u64,
    #[cfg(feature = "bench")] split: Option<&Arc<SplitBacking>>,
) -> eyre::Result<u64> {
    #[cfg(feature = "bench")]
    if let Some(split) = split {
        return split
            .write_chunks
            .checked_mul(chunk_size)
            .ok_or_else(|| eyre!("split write region byte length overflow"));
    }
    let start = u64::from(interval.start().0);
    let end = u64::from(interval.end().0);
    let chunks = end
        .checked_sub(start)
        .and_then(|span| span.checked_add(1))
        .ok_or_else(|| eyre!("submodule interval {start}..={end} chunk count overflow"))?;
    chunks.checked_mul(chunk_size).ok_or_else(|| {
        eyre!(
            "submodule interval {start}..={end} ({chunks} chunks of {chunk_size} bytes) overflows"
        )
    })
}

/// A leftover sidecar is not the checksum. Schema 3.2 stores it in the offset row.
fn forget_chunks_crc(dir: &Path) {
    let path = dir.join("chunks.crc");
    match fs::remove_file(&path) {
        Ok(()) => {
            tracing::info!(
                path = %path.display(),
                "removed leftover chunks.crc; checksums live in the offset row"
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                "could not remove leftover chunks.crc: {error}"
            );
        }
    }
}

/// Path of an already-open split write file, for the allocation log.
#[cfg(feature = "bench")]
fn split_write_label(file: &File, sub_base_path: &Path) -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        let link = format!("/proc/self/fd/{}", file.as_raw_fd());
        if let Ok(target) = fs::read_link(link) {
            return target;
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = file;
    sub_base_path.join("split-write")
}

impl StorageModule {
    /// Initializes a new StorageModule. The index engine comes from
    /// `config.node_config.database.submodule_index`.
    pub fn new(storage_module_info: &StorageModuleInfo, config: &Config) -> eyre::Result<Self> {
        Self::open(
            storage_module_info,
            config,
            #[cfg(feature = "bench")]
            None,
            #[cfg(feature = "bench")]
            None,
        )
    }

    /// One submodule, starting at partition offset 0, backed by `split` instead
    /// of `{dir}/chunks.dat`. The bench uses this so recall reads and packed
    /// writes share one disk lane while living in different files.
    ///
    /// `index` selects the submodule index under the submodule directory:
    /// `db` for MDBX, `rocks` for RocksDB. A directory that already belongs
    /// to the other engine is refused.
    #[cfg(feature = "bench")]
    pub fn open_split(
        storage_module_info: &StorageModuleInfo,
        config: &Config,
        split: Arc<SplitBacking>,
        index: BenchIndex,
    ) -> eyre::Result<Self> {
        Self::open(storage_module_info, config, Some(split), Some(index))
    }

    fn open(
        storage_module_info: &StorageModuleInfo,
        config: &Config,
        #[cfg(feature = "bench")] split: Option<Arc<SplitBacking>>,
        #[cfg(feature = "bench")] index: Option<BenchIndex>,
    ) -> eyre::Result<Self> {
        #[cfg(feature = "bench")]
        if split.is_some() {
            ensure!(
                storage_module_info.submodules.len() == 1,
                "split backing requires exactly one submodule"
            );
            let (interval, _) = &storage_module_info.submodules[0];
            ensure!(
                *interval.start() == 0,
                "split backing requires the submodule interval to start at partition offset 0"
            );
        }
        let mut submodule_map = NoditMap::new();
        let mut global_intervals = StorageIntervals::new();
        // Bind after a pending reset clears the old partition hash. Binding
        // first refuses a new partition, or the reset clear drops the new one.
        let mut pack_binds = Vec::new();
        let index_write_generation = Arc::new(AtomicU64::new(0));
        let index_commit_fail_next = Arc::new(AtomicBool::new(false));
        let index_gap = Arc::new(index_drain::IndexGap::new());

        // Initialize the submodules from the StorageModuleInfo
        for (submodule_interval, dir) in storage_module_info.submodules.clone() {
            let sub_base_path = config.node_config.base_directory.join(dir.clone());

            tracing::info!(custom.sub_base_path = ?sub_base_path);
            fs::create_dir_all(&sub_base_path)?; // Ensure the directory exists (for component tests)
            forget_chunks_crc(&sub_base_path);

            let full_len = submodule_reserve_len(
                &submodule_interval,
                config.consensus.chunk_size,
                #[cfg(feature = "bench")]
                split.as_ref(),
            )?;
            let opened = Self::open_submodule_file(
                &sub_base_path,
                full_len,
                config.node_config.storage.recall_direct_io,
                #[cfg(feature = "bench")]
                split.as_ref(),
            )?;
            let chunks_file = opened.file;
            let submodule_db = Self::open_submodule_index(
                &sub_base_path,
                config,
                #[cfg(feature = "bench")]
                split.is_some(),
                #[cfg(feature = "bench")]
                index,
            )?;

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

            let params = PackingParams::from_toml(&params_path)
                .wrap_err_with(|| format!("packing params at {}", params_path.display()))?;
            let packing = packing_identity_hash(
                &params.packing_address,
                config.consensus.entropy_packing_iterations,
                config.consensus.chunk_size,
                config.consensus.chain_id,
            );
            let bound_partition = params.partition_hash;

            let intervals_file_path = sub_base_path.join("intervals.json");

            let submodules_intervals_file = PathBuf::from(&intervals_file_path);

            // Ensure the intervals.json has a default range
            ensure_default_intervals(&submodule_interval, &submodules_intervals_file)
                .expect("to ensure default intervals exist for submodule");

            // The submodule_map maps submodule intervals to specific instance of StorageSubmodule
            // that maintains system resources connected to the files in that submodule
            submodule_map
                .insert_strict(
                    submodule_interval,
                    StorageSubmodule {
                        path: dir,
                        file: chunks_file,
                        recall_direct: opened.recall_direct,
                        crc_complete: AtomicBool::new(submodule_db.chunk_crc_complete()?),
                        #[cfg(feature = "bench")]
                        split: opened.split,
                        db: submodule_db.clone(),
                        intervals_file: Arc::new(Mutex::new(submodules_intervals_file)),
                        index_drain: index_drain::IndexDrain::spawn(
                            submodule_db.clone(),
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
            pack_binds.push((submodule_db, bound_partition, packing));

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

        // A crash during reset leaves the marker set. Finish the clear and
        // rewrite that submodule's intervals before the file is loaded.
        // The packing bind runs after that clear, so a new partition hash is
        // the one that stays.
        Self::finish_pending_resets(&submodule_map)?;
        for (index, partition, packing) in pack_binds {
            index.bind_packing_identity(partition, &packing)?;
        }

        // Attempt to load a global set of intervals from the submodules
        let loaded_intervals: NoditMap<
            PartitionChunkOffset,
            Interval<PartitionChunkOffset>,
            ChunkType,
        > = Self::load_intervals_from_submodules(&submodule_map, storage_module_info.id)?;

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

        // A bench split writes synthetic presence under Entropy after this
        // open returns. Reconciling here would delete a resumed prepopulate.
        // Production open has no split.
        #[cfg(feature = "bench")]
        let reconcile = split.is_none();
        #[cfg(not(feature = "bench"))]
        let reconcile = true;
        if reconcile {
            Self::reconcile_presence(&submodule_map, &loaded_intervals)?;
            Self::reconcile_uninitialized_coverage(
                &submodule_map,
                &loaded_intervals,
                config.consensus.chunk_size,
            )?;
        }

        // A RocksDB registration returns once its rows are visible. The WAL
        // becomes durable at the next synced write on that database, at
        // `index_wal_backstop_ms`, or on clean shutdown. Chunk migration
        // does that synced write once per touched submodule before it
        // publishes the block. MDBX keeps one durable commit per
        // registration: its group-commit path aborts inside libmdbx.
        // Set the backstop before enable, including a bench split that arms
        // group commit after this open returns.
        for (_, submodule) in submodule_map.iter() {
            submodule.db.set_wal_backstop(Duration::from_millis(
                config.node_config.storage.index_wal_backstop_ms,
            ));
        }
        // A bench split picks its own engine. The bench turns group commit
        // on for that open; this path must not arm MDBX from a Rocks config.
        #[cfg(feature = "bench")]
        let use_config_engine = split.is_none();
        #[cfg(not(feature = "bench"))]
        let use_config_engine = true;
        if use_config_engine
            && config.node_config.database.submodule_index == SubmoduleIndexEngine::Rocksdb
        {
            for (_, submodule) in submodule_map.iter() {
                submodule
                    .db
                    .enable_group_commit(Self::ROCKS_INDEX_TXS_PER_SYNC)?;
            }
        }

        #[cfg(not(any(test, feature = "test-utils")))]
        drop(index_commit_fail_next);

        Ok(Self {
            id: storage_module_info.id,
            partition_assignment: RwLock::new(storage_module_info.partition_assignment),
            pending_writes: RwLock::new(PendingWrites::default()),
            #[cfg(any(test, feature = "test-utils"))]
            index_commit_fail_next,
            reset_incomplete: AtomicBool::new(false),
            #[cfg(any(test, feature = "test-utils"))]
            reset_fail_next: AtomicBool::new(false),
            #[cfg(test)]
            fail_entropy_read_nth: AtomicU64::new(0),
            #[cfg(test)]
            entropy_read_seq: AtomicU64::new(0),
            #[cfg(any(test, feature = "test-utils"))]
            index_views: AtomicU64::new(0),
            sync_in_progress: Mutex::new(()),
            last_durable_at: Mutex::new(None),
            headroom_open: AtomicBool::new(true),
            durable_points: DurablePointAtoms::default(),
            data_writes_paused: AtomicBool::new(false),
            #[cfg(test)]
            sync_failure: Mutex::new(None),
            #[cfg(test)]
            sync_pause: SyncPauseGate::new(),
            #[cfg(test)]
            crc_check_pause: SyncPauseGate::new(),
            #[cfg(test)]
            registration_pause: RegistrationPauseGate::new(),
            #[cfg(test)]
            registration_drops: AtomicU64::new(0),
            last_pending_write: RwLock::new(Instant::now()),
            intervals: Arc::new(RwLock::new(loaded_intervals)),
            interval_disk: Mutex::new(Self::load_interval_disk(&submodule_map)),
            interval_image_epoch: AtomicU64::new(0),
            #[cfg(test)]
            suppress_interval_flush: AtomicBool::new(false),
            index_write_generation,
            submodules: submodule_map,
            recent_chunk_times: Arc::new(RwLock::new(CircularBuffer::new(8_000))), // sample window 10s = 10s x 800 chunks/s = capacity 8_000
            config: config.clone(),
            disk: disk_lane::DiskGate::with_index_gap(index_gap),
            #[cfg(test)]
            recall_io: RecallIoAtoms::new(),
            crc_repairs: Mutex::new(BTreeSet::new()),
            crc_suspects: Mutex::new(BTreeSet::new()),
            crc_scan_done: AtomicBool::new(false),
            #[cfg(test)]
            abandon_on_drop: AtomicBool::new(false),
        })
    }

    fn open_submodule_index(
        sub_base_path: &Path,
        config: &Config,
        #[cfg(feature = "bench")] split: bool,
        #[cfg(feature = "bench")] index: Option<BenchIndex>,
    ) -> eyre::Result<SubmoduleIndex> {
        #[cfg(feature = "bench")]
        if split {
            let Some(index) = index else {
                return Err(eyre!("split backing requires a bench index"));
            };
            let path = sub_base_path.join(index.dir_name());
            // The bench directory is already under a `loadtest` component.
            // A symlink must not point the index somewhere else.
            if path.exists() {
                let base = sub_base_path
                    .canonicalize()
                    .wrap_err_with(|| format!("canonicalize {}", sub_base_path.display()))?;
                let opened = path
                    .canonicalize()
                    .wrap_err_with(|| format!("canonicalize {}", path.display()))?;
                if !opened.starts_with(&base) {
                    return Err(eyre!(
                        "index directory {} is outside {}",
                        opened.display(),
                        base.display()
                    ));
                }
            }
            let rocks = matches!(index, BenchIndex::Rocks(_));
            refuse_foreign_index(&path, rocks)?;
            debug!("submodule index path: {:?}", path);
            return match index {
                BenchIndex::Mdbx => open_mdbx_index(&path, config),
                BenchIndex::Rocks(tuning) => SubmoduleIndex::open_rocks_with(&path, tuning)
                    .map_err(|error| {
                        eyre!(
                            "Failed to create or open submodule database: {} - {}",
                            path.display(),
                            error
                        )
                    }),
            };
        }
        // Production open, including `new` compiled with the bench feature.
        // The free function selects the engine from config. It refuses MDBX
        // when rocks/ is also present, and it refuses an empty index beside
        // a live index of the other engine.
        open_submodule_index(sub_base_path, &config.node_config.database)
    }

    fn open_submodule_file(
        sub_base_path: &Path,
        full_len: u64,
        recall_direct_io: bool,
        #[cfg(feature = "bench")] split: Option<&Arc<SplitBacking>>,
    ) -> eyre::Result<OpenedChunkFile> {
        #[cfg(feature = "bench")]
        if let Some(split) = split {
            let chunks_path = sub_base_path.join("chunks.dat");
            if chunks_path.exists() {
                warn!(
                    "split backing leaves existing chunks.dat closed: {}",
                    chunks_path.display()
                );
            }
            // Same advise as chunks.dat: a dup shares the open-file description,
            // and a large pread must not fetch past the range the caller sized.
            {
                let recall = split
                    .recall
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                advise_chunks_random(&recall, Path::new("split recall"))
                    .wrap_err("Failed to disable read-ahead on the recall file")?;
            }
            {
                let write = split
                    .write
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // The write region stands in for `chunks.dat`. Reserve it the
                // same way. The recall file stays untouched: it is read-only.
                let label = split_write_label(&write, sub_base_path);
                allocate_chunks_file(&write, &label, full_len)
                    .wrap_err("Failed to allocate the split write file")?;
                advise_chunks_random(&write, &label)
                    .wrap_err("Failed to disable read-ahead on the write file")?;
            }
            // The bench opens the `O_DIRECT` descriptor on `--recall-file`.
            // Entropy-for-XOR and serve stay on the buffered recall descriptor.
            let recall_direct = if recall_direct_io {
                split.recall_direct.clone()
            } else {
                None
            };
            return Ok(OpenedChunkFile {
                file: Arc::clone(&split.write),
                recall_direct,
                split: Some(Arc::clone(split)),
            });
        }

        // Get a file handle to the chunks.data file in the submodule
        let path = sub_base_path.join("chunks.dat");
        let chunks_file = OpenOptions::new()
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
            })?;
        // Mode 0 reserves holes up to the submodule length and leaves bytes
        // that are already written where they are. A partition that does not
        // fit fails here, before the first packed write.
        allocate_chunks_file(&chunks_file, &path, full_len)
            .wrap_err_with(|| format!("Failed to allocate {}", path.display()))?;
        // Every chunks.dat read is a pread of a range the caller already
        // sized. The next read is not the bytes after that range. Read-ahead
        // state belongs to this open file description, and a dup shares it,
        // so one advise covers recall, entropy, and serve. A large buffered
        // read otherwise fetches past its range by the device's maximum
        // request size.
        advise_chunks_random(&chunks_file, &path)?;
        // Mining recall only. The buffered descriptor above still serves,
        // sweeps entropy, and validates packing. `Ok(None)` is a filesystem
        // that rejected `O_DIRECT`; recall stays on `chunks_file`.
        let recall_direct = if recall_direct_io {
            open_recall_direct(&path)?.map(Arc::new)
        } else {
            None
        };
        Ok(OpenedChunkFile {
            file: Arc::new(Mutex::new(chunks_file)),
            recall_direct,
            #[cfg(feature = "bench")]
            split: None,
        })
    }

    /// File, byte offset, and submodule-relative chunk index for one chunk.
    ///
    /// `mapped` is the write-region window (packed writes, the entropy pread
    /// that those writes XOR, and Data reads). Recall and other reads pass
    /// `false` and stay at the submodule-relative offset. Without a bench
    /// split both routes are `chunks.dat` at that offset.
    fn locate_io(
        &self,
        offset: PartitionChunkOffset,
        mapped: bool,
    ) -> eyre::Result<(Arc<Mutex<File>>, u64, PartitionChunkOffset)> {
        let (interval, submodule) = self.get_submodule_for_offset(offset)?;
        let submodule_offset = offset - interval.start();
        let chunk_size = self.config.consensus.chunk_size;
        #[cfg(feature = "bench")]
        if let Some(split) = submodule.split.as_ref() {
            if mapped {
                let file_offset = split.mapped_offset(*submodule_offset, chunk_size)?;
                return Ok((Arc::clone(&split.write), file_offset, submodule_offset));
            }
            return Ok((
                Arc::clone(&split.recall),
                recall_file_byte_offset(split.recall_origin, *submodule_offset, chunk_size),
                submodule_offset,
            ));
        }
        let _ = mapped;
        Ok((
            Arc::clone(&submodule.file),
            u64::from(submodule_offset.0) * chunk_size,
            submodule_offset,
        ))
    }

    pub fn assign_partition(&self, partition_assignment: PartitionAssignment, update_height: u64) {
        let mut pa = self.partition_assignment.write().unwrap();
        self.store_assignment(&mut pa, Some(partition_assignment), update_height);
    }

    /// Clears the current partition assignment and persists the change on disk.
    ///
    /// Writes `packing_params.toml` with `partition_hash = None`, `ledger = None`,
    /// `slot = None`, and updates `last_updated_height`. In-memory assignment is set to `None`.
    pub fn clear_assignment(&self, update_height: u64) {
        let mut pa = self.partition_assignment.write().unwrap();
        self.store_assignment(&mut pa, None, update_height);
    }

    /// Store `next` and write each submodule's `packing_params.toml`.
    /// The caller holds the assignment write lock.
    fn store_assignment(
        &self,
        assignment: &mut Option<PartitionAssignment>,
        next: Option<PartitionAssignment>,
        update_height: u64,
    ) {
        *assignment = next;
        for (_, submodule) in self.submodules.iter() {
            let params = PackingParams {
                packing_address: self.config.node_config.miner_address(),
                partition_hash: next.map(|assignment| assignment.partition_hash),
                ledger: next.and_then(|assignment| assignment.ledger_id),
                slot: next.and_then(|assignment| assignment.slot_index),
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
        let pending = self.pending_writes.read().unwrap();
        !pending.is_empty() || !pending.written_ahead.is_empty()
    }

    /// A packed run is ready for the disk when the chunk disk is idle, one
    /// run fills the write cap, the pending set has reached the durability
    /// count, or a short run has waited `reorder_grace`. Young short runs
    /// stay in memory so a neighbor can join while the disk stays busy.
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
        self.has_full_write_run(&pending, cap) || self.has_aged_pending(&pending, chunk)
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
    /// every run is still waiting out `reorder_grace`. An idle disk, the
    /// durability count, and an owed recall flush count every queued run.
    #[cfg(test)]
    fn longest_ready_write_bytes(&self) -> u64 {
        let pending = self.pending_writes.read().unwrap();
        if pending.is_empty() {
            return 0;
        }
        let chunk = self.config.consensus.chunk_size.max(1);
        let cap = disk_lane::WRITE_RUN_MAX_BYTES.max(chunk);
        let grace = disk_lane::reorder_grace(chunk);
        let due_all = self.disk.chunk_disk_idle() || self.short_runs_due(pending.len());
        self.write_run_metas(&pending, cap)
            .into_iter()
            .filter(|run| due_all || run.byte_len >= cap || run.oldest.elapsed() >= grace)
            .map(|run| run.byte_len)
            .max()
            .unwrap_or(0)
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
        indexed.truncate(disk_lane::write_reorder_scan(
            self.config.node_config.storage.num_writes_before_sync,
        ));

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
            || pending
                .written_ahead
                .get(&offset)
                .is_some_and(|ahead| ahead.chunk_type == ChunkType::Data)
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
        let ahead = pending
            .written_ahead
            .values()
            .map(|ahead| ahead.bytes.len() as u64)
            .sum::<u64>();
        packed
            .saturating_add(ahead)
            .saturating_add(pending.queued_unpacked_bytes)
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

    /// `true` while `reset` has not finished in this process.
    pub fn reset_incomplete(&self) -> bool {
        self.reset_incomplete.load(Ordering::SeqCst)
    }

    /// The next `reset` returns an error before it writes.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn fail_next_reset_for_test(&self) {
        self.reset_fail_next.store(true, Ordering::SeqCst);
    }

    /// `true` once. The packed chunk is not queued, and no index row is written.
    #[cfg(any(test, feature = "test-utils"))]
    fn take_index_commit_failure(&self) -> bool {
        self.index_commit_fail_next.swap(false, Ordering::SeqCst)
    }

    /// Submodule views opened since the last clear. One slice is one view.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn index_view_count(&self) -> u64 {
        self.index_views.load(Ordering::SeqCst)
    }

    /// Zero the submodule view counter.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn clear_index_view_count(&self) {
        self.index_views.store(0, Ordering::SeqCst);
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

    /// The next durable point waits at `point` until [`Self::release_sync_pause`].
    #[cfg(test)]
    fn pause_next_sync_at(&self, point: SyncPausePoint) {
        *self.sync_pause.release.lock().unwrap() = false;
        *self.sync_pause.arrived.lock().unwrap() = false;
        *self.sync_pause.point.lock().unwrap() = Some(point);
    }

    /// `true` when the armed point was reached before `timeout`.
    #[cfg(test)]
    fn wait_for_sync_pause(&self, timeout: Duration) -> bool {
        let guard = self.sync_pause.arrived.lock().unwrap();
        let (guard, _) = self
            .sync_pause
            .arrived_cv
            .wait_timeout_while(guard, timeout, |arrived| !*arrived)
            .unwrap();
        *guard
    }

    /// Let a paused durable point continue.
    #[cfg(test)]
    fn release_sync_pause(&self) {
        *self.sync_pause.release.lock().unwrap() = true;
        self.sync_pause.release_cv.notify_all();
    }

    #[cfg(test)]
    fn wait_if_sync_paused(&self, point: SyncPausePoint) {
        Self::wait_on_pause_gate(&self.sync_pause, point);
    }

    /// The next checksum check waits after its bytes are read.
    #[cfg(test)]
    fn pause_next_crc_check(&self) {
        *self.crc_check_pause.release.lock().unwrap() = false;
        *self.crc_check_pause.arrived.lock().unwrap() = false;
        *self.crc_check_pause.point.lock().unwrap() = Some(SyncPausePoint::AfterPresenceCopy);
    }

    /// `true` when the checksum check reached its pause before `timeout`.
    #[cfg(test)]
    fn wait_for_crc_check_pause(&self, timeout: Duration) -> bool {
        let guard = self.crc_check_pause.arrived.lock().unwrap();
        let (guard, _) = self
            .crc_check_pause
            .arrived_cv
            .wait_timeout_while(guard, timeout, |arrived| !*arrived)
            .unwrap();
        *guard
    }

    /// Let a paused checksum check continue.
    #[cfg(test)]
    fn release_crc_check_pause(&self) {
        *self.crc_check_pause.release.lock().unwrap() = true;
        self.crc_check_pause.release_cv.notify_all();
    }

    #[cfg(test)]
    fn wait_if_crc_check_paused(&self) {
        Self::wait_on_pause_gate(&self.crc_check_pause, SyncPausePoint::AfterPresenceCopy);
    }

    #[cfg(test)]
    fn wait_on_pause_gate(gate: &SyncPauseGate, point: SyncPausePoint) {
        {
            let mut armed = gate.point.lock().unwrap();
            if *armed != Some(point) {
                return;
            }
            *armed = None;
        }
        {
            let mut arrived = gate.arrived.lock().unwrap();
            *arrived = true;
            gate.arrived_cv.notify_all();
        }
        let mut release = gate.release.lock().unwrap();
        while !*release {
            release = gate.release_cv.wait(release).unwrap();
        }
    }

    /// The next registration waits after its rows are computed.
    #[cfg(test)]
    fn pause_next_registration_at(&self, point: RegistrationPausePoint) {
        *self.registration_pause.release.lock().unwrap() = false;
        *self.registration_pause.arrived.lock().unwrap() = false;
        *self.registration_pause.point.lock().unwrap() = Some(point);
    }

    /// `true` when the armed registration pause was reached before `timeout`.
    #[cfg(test)]
    fn wait_for_registration_pause(&self, timeout: Duration) -> bool {
        let guard = self.registration_pause.arrived.lock().unwrap();
        let (guard, _) = self
            .registration_pause
            .arrived_cv
            .wait_timeout_while(guard, timeout, |arrived| !*arrived)
            .unwrap();
        *guard
    }

    /// Let a paused registration continue.
    #[cfg(test)]
    fn release_registration_pause(&self) {
        *self.registration_pause.release.lock().unwrap() = true;
        self.registration_pause.release_cv.notify_all();
    }

    #[cfg(test)]
    fn wait_if_registration_paused(&self, point: RegistrationPausePoint) {
        {
            let mut armed = self.registration_pause.point.lock().unwrap();
            if *armed != Some(point) {
                return;
            }
            *armed = None;
        }
        {
            let mut arrived = self.registration_pause.arrived.lock().unwrap();
            *arrived = true;
            self.registration_pause.arrived_cv.notify_all();
        }
        let mut release = self.registration_pause.release.lock().unwrap();
        while !*release {
            release = self.registration_pause.release_cv.wait(release).unwrap();
        }
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

    /// True when the offset's live interval is `Data` and no buffered write
    /// supersedes it.
    ///
    /// The live interval label flips to `Data` when `pwrite` returns
    /// (`note_write_run`). This stays false while the offset is still in
    /// `pending.chunks` or `written_ahead`.
    pub fn is_data_chunk_durable_at(&self, offset: PartitionChunkOffset) -> bool {
        let pending = self.pending_writes.read().unwrap();
        if pending.contains_key(&offset) || pending.written_ahead.contains_key(&offset) {
            return false;
        }
        drop(pending);
        self.intervals
            .read()
            .unwrap()
            .get_at_point(offset)
            .is_some_and(|chunk_type| *chunk_type == ChunkType::Data)
    }

    /// [`Self::is_data_chunk_durable_at`], and `intervals.json` already stores
    /// the offset as `Data`.
    ///
    /// The live label leads the file by up to `storage.intervals_persist_secs`.
    /// A crash in that lag reloads the offset as `Uninitialized`, and open
    /// drops its presence and placement rows. The bytes are on disk, but the
    /// node no longer knows it holds them. Anything that discards another copy
    /// of the chunk must wait for this, not for durability alone.
    pub fn is_data_chunk_persisted_at(&self, offset: PartitionChunkOffset) -> bool {
        if !self.is_data_chunk_durable_at(offset) {
            return false;
        }
        self.lock_interval_disk()
            .written
            .get_at_point(offset)
            .is_some_and(|chunk_type| *chunk_type == ChunkType::Data)
    }

    /// Candidate transaction-relative offsets of `data_root` whose bodies are
    /// fsynced in this module as `ChunkType::Data` and recorded as `Data` in
    /// `intervals.json`.
    ///
    /// This is the gate cached-body reclamation uses. The live interval label
    /// flips to `Data` when `pwrite` returns (`note_write_run`). Durability
    /// stays false until that offset's `pending.chunks` and `written_ahead`
    /// entries are gone, which is after the `fdatasync` and the presence
    /// commit. `intervals.json` can lag that by `storage.intervals_persist_secs`,
    /// and a crash in the lag forgets the chunk. A freshly uploaded chunk may
    /// have no other replica, so the cached body is kept until the file says
    /// `Data` ([`Self::is_data_chunk_persisted_at`]); after a crash, data sync
    /// reads it from the local cache. Resolves the submodule index once per
    /// root and checks only bodies the cache can actually reclaim, rather than
    /// walking the transaction's entire funded range.
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
                if self.is_data_chunk_persisted_at(PartitionChunkOffset::from(relative_offset)) {
                    durable.insert(*tx_offset);
                    break;
                }
            }
        }
        Ok(durable)
    }

    /// Erase the index, then mark every offset `Uninitialized`.
    ///
    /// Holds `sync_in_progress` for the whole call, and takes it before
    /// `pending_writes`. A durable point takes that lock first, copies proofs
    /// out of `pending`, then writes them. The clear must not run in that
    /// gap: the copied rows would land in the empty index.
    ///
    /// Interval files are written on this thread. The lane handoff waits on
    /// the lane, and the lane takes `sync_in_progress` to flush chunks.
    /// Waiting here would deadlock. [`Self::interval_image_epoch`] drops a
    /// lane rewrite that snapshotted the map before this call.
    ///
    /// The metadata marker is durable before the clear. A crash after the
    /// marker and before the marker is dropped is finished on the next open:
    /// the index is cleared again and that submodule's interval file is
    /// rewritten to `Uninitialized`. The in-memory map flips to
    /// `Uninitialized` as soon as the clear commits, so this process does not
    /// serve the old bytes while the file still names them `Data`.
    ///
    /// This does not change the partition assignment. A caller that resets
    /// because the assignment changes must use [`Self::reset_with_assignment`],
    /// which stores the new assignment before releasing the assignment lock.
    /// After `reset` alone, the previous assignment is still visible and a
    /// registration can commit rows for it into the cleared index.
    pub fn reset(&self) -> eyre::Result<Interval<PartitionChunkOffset>> {
        let _sync_guard = self
            .sync_in_progress
            .lock()
            .map_err(|error| eyre::eyre!("storage-module sync lock poisoned: {error}"))?;
        #[cfg(any(test, feature = "test-utils"))]
        if self.reset_fail_next.swap(false, Ordering::SeqCst) {
            self.reset_incomplete.store(true, Ordering::SeqCst);
            eyre::bail!("injected storage module reset failure");
        }
        // Drop crc_complete before the index clear and the interval rewrite.
        // Repack writes fresh entries. A crash after this flag clear is
        // finished by `finish_pending_resets`, which clears the flag again.
        self.clear_chunk_crc_complete()?;
        self.reset_incomplete.store(true, Ordering::SeqCst);
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
            pending.proofs.clear();
            pending.written_ahead.clear();
        }
        // A recall may already have queued a suspect against the old bytes.
        // Confirming it after this clear would judge the wiped module.
        self.crc_suspects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        self.fail_queued_sweeps();
        self.poll_acks();

        for (_, submodule) in self.submodules.iter() {
            submodule.db.set_reset_in_progress()?;
        }
        #[cfg(test)]
        self.inject_sync_failure(SyncFailurePoint::AfterResetMarker)?;

        for (_, submodule) in self.submodules.iter() {
            submodule.db.update(|tx| tx.clear())?;
        }

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
        // Publish the epoch with the mask clear. A rewrite that already read
        // the old map fails its epoch check and does not restore that mask.
        {
            let mut disk = self.lock_interval_disk();
            self.interval_image_epoch.fetch_add(1, Ordering::SeqCst);
            disk.mask = IntervalMask::new();
            disk.dirty.clear();
        }
        #[cfg(test)]
        self.inject_sync_failure(SyncFailurePoint::AfterResetClear)?;

        // Not `write_intervals_to_submodules`: that waits for the lane, and
        // the lane needs `sync_in_progress`, which this call holds.
        // Always replace the files. A lane rewrite may already hold the file
        // lock with a pre-reset snapshot; this waits for that lock and writes
        // `Uninitialized` after it.
        self.persist_intervals_authoritative(true)
            .wrap_err("Could not update submodule interval files")?;
        #[cfg(test)]
        self.inject_sync_failure(SyncFailurePoint::AfterResetIntervals)?;

        for (_, submodule) in self.submodules.iter() {
            submodule.db.clear_reset_in_progress()?;
        }
        self.reset_incomplete.store(false, Ordering::SeqCst);
        Ok(storage_interval)
    }

    /// Reset the index and install `next` before any registration can observe
    /// the cleared module under the old assignment.
    ///
    /// Lock order, outer to inner: `partition_assignment` write, then
    /// `sync_in_progress` (taken by [`Self::reset`]), then `pending_writes`
    /// and the interval map. A registration takes the assignment read lock
    /// only across its equality check and index commit. It does not take
    /// `sync_in_progress`. No holder of `sync_in_progress` takes
    /// `partition_assignment`. Readers (mining, the chunk provider, data
    /// sync, the body worker) take the assignment read lock and drop it
    /// before they take `pending_writes`, `intervals`, or `sync_in_progress`.
    ///
    /// `next` of `None` clears the assignment. A failed reset leaves the
    /// previous assignment in place.
    pub fn reset_with_assignment(
        &self,
        next: Option<PartitionAssignment>,
        update_height: u64,
    ) -> eyre::Result<Interval<PartitionChunkOffset>> {
        let mut assignment = self.partition_assignment.write().unwrap();
        let interval = self.reset()?;
        self.store_assignment(&mut assignment, next, update_height);
        Ok(interval)
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

    /// Milliseconds configured between durable points. The node default is
    /// 250 for HDD storage modules. `0` is every point.
    fn min_durable_interval(&self) -> Duration {
        Duration::from_millis(self.config.node_config.storage.min_durable_interval_ms)
    }

    /// Pending bytes above this end a deferral early. The memory ceiling is
    /// the hard stop above it. Permille above 1000 is the ceiling.
    fn durable_headroom_bytes(&self) -> u64 {
        let permille = self
            .config
            .node_config
            .storage
            .durable_headroom_permille
            .min(1000);
        self.pending_memory_ceiling_bytes().saturating_mul(permille) / 1000
    }

    fn durable_cause(&self, force: bool) -> Option<DurablePointCause> {
        let decision = durable_point_decision(DurableDeferQuery {
            force,
            interval: self.min_durable_interval(),
            since_last: self.load_last_durable_at().map(|last| last.elapsed()),
            pending_bytes: self.pending_write_bytes(),
            ceiling_bytes: self.pending_memory_ceiling_bytes(),
            headroom_bytes: self.durable_headroom_bytes(),
            headroom_open: self.headroom_open.load(Ordering::Relaxed),
            recall_flush_owed: self.disk.recall_flush_is_owed(),
        });
        self.headroom_open
            .store(decision.headroom_open, Ordering::Relaxed);
        decision.cause
    }

    fn has_written_ahead(&self) -> bool {
        !self.pending_writes.read().unwrap().written_ahead.is_empty()
    }

    /// Same ceiling the body-migration worker uses. A configured value is
    /// floored at one flush batch so a small cap cannot stall that worker.
    fn pending_memory_ceiling_bytes(&self) -> u64 {
        let storage = &self.config.node_config.storage;
        let submodules = self.submodule_count() as u64;
        let chunk = self.config.consensus.chunk_size;
        let one_batch = storage
            .num_writes_before_sync
            .saturating_mul(chunk)
            .saturating_mul(submodules);
        match storage.max_pending_write_bytes {
            Some(configured) => configured.max(one_batch),
            None => {
                const BATCHES: u64 = 2;
                const MIN_CHUNKS: u64 = 256;
                BATCHES
                    .saturating_mul(storage.num_writes_before_sync)
                    .max(MIN_CHUNKS)
                    .saturating_mul(chunk)
                    .saturating_mul(submodules)
            }
        }
    }

    fn load_last_durable_at(&self) -> Option<Instant> {
        *self
            .last_durable_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn stamp_durable_point(&self) {
        *self
            .last_durable_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
    }

    /// True when this window must not fdatasync yet.
    ///
    /// Force, a zero interval, the first point, pending bytes over the
    /// headroom line, and pending bytes above the ceiling all return false.
    /// A closed headroom latch stays closed until pending bytes fall to half
    /// the line. An owed recall flush still returns true on headroom loss, so
    /// that recall does not wait on the fdatasync. The interval is the longest
    /// wait: a younger point is never held past it.
    fn defer_durable_point(&self, force: bool) -> bool {
        self.durable_cause(force).is_none()
    }

    /// Time until parked bytes may take a durable point. `None` when the
    /// interval is off or nothing is parked. Zero when the point is due.
    fn durable_point_remaining(&self) -> Option<Duration> {
        if self.min_durable_interval().is_zero() || !self.has_written_ahead() {
            return None;
        }
        if !self.defer_durable_point(false) {
            return Some(Duration::ZERO);
        }
        let last = self.load_last_durable_at()?;
        Some(self.min_durable_interval().saturating_sub(last.elapsed()))
    }

    /// Durable points since this module was opened, by timing and cause.
    pub fn durable_point_counts(&self) -> DurablePointCounts {
        self.durable_points.snapshot()
    }

    /// Run a due durable point that an earlier window parked. No new pwrite.
    fn finish_deferred_durable_point(&self) -> eyre::Result<()> {
        if self.durable_point_remaining() != Some(Duration::ZERO) {
            return Ok(());
        }
        let _sync_guard = self
            .sync_in_progress
            .lock()
            .map_err(|error| eyre::eyre!("storage-module sync lock poisoned: {error}"))?;
        if self.durable_point_remaining() != Some(Duration::ZERO) {
            return Ok(());
        }
        // The checks above already found the point due. Read the cause again
        // under the sync lock. A new owed flush can still defer a headroom
        // point. Force, the ceiling, and an elapsed interval stay due.
        let Some(cause) = self.durable_cause(false) else {
            return Ok(());
        };
        self.finish_durable_point(false, &[], Vec::new(), &BTreeSet::new(), cause)
    }

    fn park_written_ahead(
        &self,
        batch: &[(
            PartitionChunkOffset,
            (ChunkBytes, ChunkType),
            disk_lane::WritePriority,
        )],
    ) {
        let mut pending = self.pending_writes.write().unwrap();
        for (offset, state, priority) in batch {
            if pending.get(offset) == Some(state) {
                let (bytes, chunk_type) = pending.remove(offset).expect("pending state matched");
                let queued_at = pending
                    .queued_at
                    .remove(offset)
                    .unwrap_or_else(Instant::now);
                pending.priorities.remove(offset);
                pending.written_ahead.insert(
                    *offset,
                    WrittenAhead {
                        bytes,
                        chunk_type,
                        priority: *priority,
                        queued_at,
                    },
                );
            }
        }
    }

    /// Put this point's parked bytes back on the pwrite queue.
    ///
    /// Only an entry still identical to the snapshot moves. A newer enqueue
    /// already replaced it and must stay. `priority` and `queued_at` are the
    /// values saved when the bytes were parked. Bytes pwritten in this call
    /// are still in `pending.chunks`; this function does not touch them.
    fn requeue_written_ahead_after_sync_error(
        &self,
        ahead: &[(
            PartitionChunkOffset,
            (ChunkBytes, ChunkType),
            disk_lane::WritePriority,
        )],
    ) {
        let mut pending = self.pending_writes.write().unwrap();
        for (offset, state, _) in ahead {
            let Some(parked) = pending.written_ahead.get(offset) else {
                continue;
            };
            if parked.bytes != state.0 || parked.chunk_type != state.1 {
                continue;
            }
            let parked = pending.written_ahead.remove(offset).expect("matched above");
            if pending.contains_key(offset) {
                continue;
            }
            pending.priorities.insert(*offset, parked.priority);
            pending.queued_at.insert(*offset, parked.queued_at);
            pending.insert(*offset, (parked.bytes, parked.chunk_type));
        }
    }

    fn snapshot_written_ahead(
        &self,
    ) -> Vec<(
        PartitionChunkOffset,
        (ChunkBytes, ChunkType),
        disk_lane::WritePriority,
    )> {
        let pending = self.pending_writes.read().unwrap();
        pending
            .written_ahead
            .iter()
            .map(|(offset, ahead)| {
                (
                    *offset,
                    (ahead.bytes.clone(), ahead.chunk_type),
                    ahead.priority,
                )
            })
            .collect()
    }

    /// `fdatasync` every `chunks.dat` this point wrote. A test failure and an
    /// I/O error both return before presence is committed.
    fn sync_touched_chunk_files(
        &self,
        touched: &BTreeSet<PartitionChunkOffset>,
    ) -> eyre::Result<()> {
        #[cfg(test)]
        self.inject_sync_failure(SyncFailurePoint::BeforeDataFsync)?;
        for (interval, submodule) in self.submodules.iter() {
            if !touched.contains(&interval.start()) {
                continue;
            }
            self.disk.yield_to_recall();
            let file_arc = Arc::clone(&submodule.file);
            let file = self.disk.lock_chunks(&file_arc);
            file.sync_data()
                .map_err(|e| eyre::eyre!("Failed to sync data to disk: {}", e))?;
        }
        Ok(())
    }

    /// Fdatasync, presence commit, interval completion, then drop the fence.
    /// `write_batch` was pwritten by this call. Parked bytes from earlier
    /// windows join the same point. The caller holds `sync_in_progress`.
    /// `cause` is the decision that started this point. A non-empty point
    /// counts once, after the sync succeeds.
    fn finish_durable_point(
        &self,
        force: bool,
        write_batch: &[(
            PartitionChunkOffset,
            (ChunkBytes, ChunkType),
            disk_lane::WritePriority,
        )],
        mut runs: Vec<WriteRun>,
        touched_starts: &BTreeSet<PartitionChunkOffset>,
        cause: DurablePointCause,
    ) -> eyre::Result<()> {
        let ahead = self.snapshot_written_ahead();
        if write_batch.is_empty() && ahead.is_empty() {
            return Ok(());
        }
        // Parked means an earlier window already wrote these bytes and waited.
        // A headroom or ceiling cause that ends that wait counts as immediate.
        let parked = !ahead.is_empty();
        let mut durable_batch = write_batch.to_vec();
        durable_batch.extend(ahead.iter().cloned());
        let ahead_runs = self.plan_write_runs(&ahead, self.config.consensus.chunk_size.max(1));
        runs.extend(ahead_runs);
        let durable_runs = runs;
        let mut touched = touched_starts.clone();
        for (offset, _, _) in &ahead {
            let (interval, _) = self.get_submodule_for_offset(*offset)?;
            touched.insert(interval.start());
        }

        // fdatasync this write batch before any row that says the chunk is
        // present. A crash after the bytes land and before that row is safe:
        // intervals and the index still say the chunk is absent.
        // A recall window syncs only the files it wrote.
        // A failed sync can drop the dirty pages and the next sync can return
        // success, so parked bytes go back on the pwrite queue. Bytes pwritten
        // in this call are still in `pending.chunks` and are pwritten again
        // with that queue.
        if let Err(error) = self.sync_touched_chunk_files(&touched) {
            error!("Failed to sync data to disk: {error}");
            super::index_read_metrics::note_chunk_fsync_failure();
            self.requeue_written_ahead_after_sync_error(&ahead);
            return Err(error);
        }

        // One durable index batch per submodule in this gap. It holds the
        // data-path rows for the chunks just synced. A synced RocksDB write
        // also covers registrations still waiting on group commit.
        let uncovered = self.commit_presence_rows(&durable_batch)?;
        // A proof with no covering interval was not stored. Put those offsets
        // back to Uninitialized before the completion file is written, and
        // leave them queued. A crash must not reload Data with no proof.
        let skipped: BTreeSet<PartitionChunkOffset> = uncovered.into_iter().collect();
        if !skipped.is_empty() {
            index_read_metrics::note_uncovered_data_path(skipped.len() as u64);
            let mut intervals = self
                .intervals
                .write()
                .map_err(|e| eyre::eyre!("Failed to acquire write lock on intervals: {}", e))?;
            for offset in &skipped {
                Self::cut_then_insert_interval_if_touching(
                    &mut intervals,
                    *offset,
                    ChunkType::Uninitialized,
                );
            }
        }

        #[cfg(test)]
        self.inject_sync_failure(SyncFailurePoint::BeforeIntervalCommit)?;

        // Completions are true only after the fdatasync and the presence
        // commit above. A batched completion waits for the persist period
        // unless this sync was forced. Period 0 writes the live map now.
        self.persist_interval_completion(force, &durable_runs, &touched)
            .wrap_err("Could not update submodule interval files, if this is a component test with storage_module that drops after the test, this error is benign")?;

        // Remove only the exact values written by this snapshot. A concurrent
        // producer may have replaced an entry while the disk I/O was running;
        // that newer value must remain queued for the next batch. An offset
        // whose proof was not stored stays queued with that proof.
        let mut pending = self.pending_writes.write().unwrap();
        for (chunk_offset, state, _) in write_batch {
            if skipped.contains(chunk_offset) {
                continue;
            }
            if pending.get(chunk_offset) == Some(state) {
                pending.remove(chunk_offset);
                pending.priorities.remove(chunk_offset);
                pending.queued_at.remove(chunk_offset);
                pending.proofs.remove(chunk_offset);
            }
        }
        for (chunk_offset, state, _) in &ahead {
            if skipped.contains(chunk_offset) {
                continue;
            }
            if pending
                .written_ahead
                .get(chunk_offset)
                .is_some_and(|ahead| ahead.bytes == state.0 && ahead.chunk_type == state.1)
            {
                pending.written_ahead.remove(chunk_offset);
                pending.proofs.remove(chunk_offset);
            }
        }
        drop(pending);
        self.stamp_durable_point();
        self.durable_points.record(parked, cause);
        self.maybe_mark_crc_complete();
        Ok(())
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
        // every short run. A busy disk writes full runs and aged short runs.
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
        let mut write_batch = {
            let pending = self.pending_writes.read().unwrap();

            let pending_writes = if coalesce && let Some(limit) = max_runs {
                self.select_coalesced_window(
                    &pending,
                    limit,
                    disk_lane::write_reorder_scan(
                        self.config.node_config.storage.num_writes_before_sync,
                    ),
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
            pending_writes
        };

        if write_batch.is_empty() {
            if let Some(cause) = self.durable_cause(force) {
                self.finish_durable_point(force, &[], Vec::new(), &BTreeSet::new(), cause)?;
            }
            return self.flush_forced_interval_completions(force);
        }

        let mut runs = if coalesce {
            self.plan_ranked_runs(&write_batch, run_cap)
        } else {
            self.plan_write_runs(&write_batch, run_cap)
        };
        if let Some(limit) = max_runs {
            runs.truncate(limit);
        }
        if runs.is_empty() {
            if let Some(cause) = self.durable_cause(force) {
                self.finish_durable_point(force, &[], Vec::new(), &BTreeSet::new(), cause)?;
            }
            return self.flush_forced_interval_completions(force);
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

        // The live map records this batch as Interrupted before the bytes
        // change. An ahead span stays out of the live map: it is only an
        // on-disk mask, so mining and migration still see Entropy.
        for (chunk_offset, _, _) in write_batch.iter() {
            Self::cut_then_insert_interval_if_touching(
                &mut intervals,
                *chunk_offset,
                ChunkType::Interrupted,
            );
        }

        drop(intervals);

        self.persist_write_ahead(&runs, &touched_starts)
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

        // Hold the fdatasync and the presence commit when the previous point
        // is younger than `min_durable_interval_ms` and pending bytes are
        // inside the headroom line. The pwrites above already ran. Force,
        // headroom loss, the pending-byte ceiling, and a zero interval do
        // not hold the pair. `0` is every point.
        let Some(cause) = self.durable_cause(force) else {
            self.park_written_ahead(&write_batch);
            return Ok(());
        };
        self.finish_durable_point(force, &write_batch, runs, &touched_starts, cause)?;

        debug!(
            "sync_pending_chunks took {:.3}s for {} chunks",
            &then.elapsed().as_secs_f64(),
            &len
        );

        Ok(())
    }

    /// Write checksums, and data-path rows, for `write_batch`.
    ///
    /// Call this only after `chunks.dat` has been fdatasync'd and before the
    /// completion label. A data chunk stores its queued proof and the checksum
    /// of the packed bytes. A data chunk with no queued proof keeps a proof
    /// already stored at that offset and replaces the checksum. An entropy
    /// chunk stores the checksum only and drops a proof. A proof with no
    /// covering interval is not stored, and neither is its checksum. MDBX
    /// stores proofs only. The returned offsets had a proof and no covering
    /// interval.
    fn commit_presence_rows(
        &self,
        write_batch: &[(
            PartitionChunkOffset,
            (ChunkBytes, ChunkType),
            disk_lane::WritePriority,
        )],
    ) -> eyre::Result<Vec<PartitionChunkOffset>> {
        let mut by_start: BTreeMap<PartitionChunkOffset, Vec<PackedOffsetRow>> = BTreeMap::new();
        {
            let pending = self.pending_writes.read().unwrap();
            for (offset, (bytes, chunk_type), _) in write_batch {
                if !matches!(*chunk_type, ChunkType::Data | ChunkType::Entropy) {
                    continue;
                }
                let (interval, submodule) = self.get_submodule_for_offset(*offset)?;
                let mut proof = if *chunk_type == ChunkType::Data {
                    pending.proofs.get(offset).map(|(_, path)| path.clone())
                } else {
                    None
                };
                // `write_chunk` does not queue a proof. Replacing the row
                // with a checksum alone would delete a proof stored earlier.
                if *chunk_type == ChunkType::Data
                    && proof.is_none()
                    && submodule.db.stores_offset_crc()
                {
                    proof = submodule
                        .db
                        .view(|tx| tx.get_offset_meta(*offset))?
                        .and_then(|row| row.proof);
                }
                by_start
                    .entry(PartitionChunkOffset(*interval.start()))
                    .or_default()
                    .push(PackedOffsetRow {
                        offset: *offset,
                        crc: Some(chunk_crc::checksum(bytes)),
                        proof,
                    });
            }
        }
        // The copies are no longer tied to `pending`. A test can run `reset`
        // in this gap. The caller still holds `sync_in_progress`.
        #[cfg(test)]
        if !by_start.is_empty() {
            self.wait_if_sync_paused(SyncPausePoint::AfterPresenceCopy);
        }
        let mut uncovered = Vec::new();
        for (start, rows) in by_start {
            let (_, submodule) = self.get_submodule_for_offset(start)?;
            // MDBX ignores checksums. An entropy-only batch must not commit
            // an empty proof write.
            if !submodule.db.stores_offset_crc() && rows.iter().all(|row| row.proof.is_none()) {
                continue;
            }
            let missed = submodule
                .db
                .update(|tx| tx.write_packed_offset_rows(rows))?;
            uncovered.extend(missed);
        }
        Ok(uncovered)
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
        // A running lane writes the files itself. This caller waits. A stopped
        // lane, and the lane thread, write here.
        if self.disk.lane_running()
            && !self.disk.on_lane_thread()
            && Self::lane_request_done(self.disk.request_lane_persist())?
        {
            return Ok(());
        }
        self.persist_intervals_authoritative(false)
    }

    fn intervals_persist_period(&self) -> Duration {
        Duration::from_secs(self.config.node_config.storage.intervals_persist_secs)
    }

    /// `0` keeps both `intervals.json` rewrites on every sync.
    fn intervals_batched(&self) -> bool {
        self.config.node_config.storage.intervals_persist_secs > 0
    }

    fn interval_flush_suppressed(&self) -> bool {
        #[cfg(test)]
        {
            self.suppress_interval_flush.load(Ordering::Relaxed)
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    #[cfg(test)]
    fn suppress_interval_flush_for_test(&self) {
        self.suppress_interval_flush.store(true, Ordering::Relaxed);
    }

    fn lock_interval_disk(&self) -> std::sync::MutexGuard<'_, IntervalDisk> {
        self.interval_disk
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn load_interval_image_epoch(&self) -> u64 {
        self.interval_image_epoch.load(Ordering::SeqCst)
    }

    /// `false` when `reset` published a newer map after this snapshot was taken.
    /// The caller must not replace the file or restore that snapshot's mask.
    fn interval_snapshot_current(&self, snapshot_epoch: u64) -> bool {
        let current = snapshot_epoch == self.load_interval_image_epoch();
        if !current {
            debug!("skipping interval rewrite snapshotted before reset");
        }
        current
    }

    /// Time until a dirty completion may be rewritten. `None` when batching
    /// is off, the flush is suppressed, or nothing is waiting.
    fn interval_flush_remaining(&self) -> Option<Duration> {
        if !self.intervals_batched() || self.interval_flush_suppressed() {
            return None;
        }
        let period = self.intervals_persist_period();
        let disk = self.lock_interval_disk();
        if disk.dirty.is_empty() {
            return None;
        }
        let mut soonest: Option<Duration> = None;
        for start in &disk.dirty {
            let elapsed = disk
                .last_completion
                .get(start)
                .map(Instant::elapsed)
                .unwrap_or(period);
            let remain = period.saturating_sub(elapsed);
            soonest = Some(match soonest {
                Some(current) => current.min(remain),
                None => remain,
            });
        }
        soonest
    }

    fn flush_forced_interval_completions(&self, force: bool) -> eyre::Result<()> {
        if force {
            self.flush_interval_completions_locked(true)?;
        }
        Ok(())
    }

    /// Store `Interrupted` for this batch before chunk bytes change.
    ///
    /// Period `0` rewrites the live map, which is already `Interrupted` for
    /// the batch. Offsets parked by an earlier point are stored as
    /// `Interrupted` too. A positive period also marks up to
    /// [`INTERVAL_WRITE_AHEAD_BYTES`] of following `Entropy` or
    /// `Uninitialized` in the same submodule. That ahead range is not written
    /// into the live map. The mask is updated only after the file replace
    /// succeeds, or immediately when the file already has those labels.
    fn persist_write_ahead(
        &self,
        runs: &[WriteRun],
        touched: &BTreeSet<PartitionChunkOffset>,
    ) -> eyre::Result<()> {
        if !self.intervals_batched() {
            return self.write_intervals_files(Some(touched), None);
        }
        // Read the epoch before the map. A later reset fails this snapshot
        // instead of letting it put the old mask back.
        let snapshot_epoch = self.load_interval_image_epoch();
        let memory = self
            .intervals
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let chunk = self.config.consensus.chunk_size.max(1);
        let budget = INTERVAL_WRITE_AHEAD_BYTES / chunk;
        let mut disk = self.lock_interval_disk();
        if !self.interval_snapshot_current(snapshot_epoch) {
            return Ok(());
        }
        let covered = Self::runs_covered(&disk.mask, runs);
        // The file already says Interrupted for this batch. A rewrite here
        // would only publish retired completions, and those wait for the
        // persist period so a contiguous frontier does not rewrite every gap.
        if covered {
            return Ok(());
        }
        let mut mask = disk.mask.clone();
        for run in runs {
            let submodule = *self.get_submodule_for_offset(run.start)?.0;
            Self::extend_write_ahead(&mut mask, &memory, run, submodule, budget);
        }
        let projection = Self::project_interval_file(&memory, &mask)?;
        let needs_write = self.projection_differs(&disk.written, &projection, touched);
        if !needs_write {
            // The file already has these Interrupted labels. Remember them so
            // the next batch inside the span does not rewrite.
            disk.mask = mask;
            return Ok(());
        }
        drop(disk);
        self.write_interval_image(&projection, Some(touched), snapshot_epoch, false)?;
        let mut disk = self.lock_interval_disk();
        if self.interval_snapshot_current(snapshot_epoch) {
            disk.mask = mask;
        }
        Ok(())
    }

    /// Drop the write-ahead mask for runs whose bytes and presence rows are
    /// durable. The file still says `Interrupted` until a completion rewrite.
    fn retire_write_ahead(&self, runs: &[WriteRun]) -> eyre::Result<()> {
        if !self.intervals_batched() {
            return Ok(());
        }
        let mut cuts = Vec::with_capacity(runs.len());
        for run in runs {
            let (start, end) = Self::run_bounds(run);
            let submodule_start = self.get_submodule_for_offset(run.start)?.0.start();
            cuts.push((start, end, submodule_start));
        }
        let mut disk = self.lock_interval_disk();
        for (start, end, submodule_start) in cuts {
            let _ = disk.mask.cut(ii(start, end));
            disk.dirty.insert(submodule_start);
        }
        Ok(())
    }

    fn persist_interval_completion(
        &self,
        force: bool,
        runs: &[WriteRun],
        touched: &BTreeSet<PartitionChunkOffset>,
    ) -> eyre::Result<()> {
        if self.intervals_batched() {
            self.retire_write_ahead(runs)?;
            self.flush_interval_completions_locked(force)?;
        } else {
            // These runs are fsynced and presence-committed. Pending still
            // holds them until the caller drops the fence. Exclude them so
            // the file stores the live label. Every other queued or parked
            // offset stays `Interrupted`.
            let durable_now = Self::offsets_in_runs(runs);
            self.write_intervals_files(Some(touched), Some(&durable_now))?;
        }
        Ok(())
    }

    /// Rewrite dirty completions. `force` ignores the persist period. The
    /// ahead mask stays, so this does not publish `Entropy` over a span that
    /// a crash must still treat as `Uninitialized`.
    fn flush_interval_completions_locked(&self, force: bool) -> eyre::Result<()> {
        if self.interval_flush_suppressed() || !self.intervals_batched() {
            return Ok(());
        }
        let period = self.intervals_persist_period();
        let snapshot_epoch = self.load_interval_image_epoch();
        let memory = self
            .intervals
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let disk = self.lock_interval_disk();
        if !self.interval_snapshot_current(snapshot_epoch) {
            return Ok(());
        }
        let mut touched = BTreeSet::new();
        for (interval, _) in self.submodules.iter() {
            let start = interval.start();
            if !disk.dirty.contains(&start) {
                continue;
            }
            let due = disk
                .last_completion
                .get(&start)
                .is_none_or(|then| then.elapsed() >= period);
            if force || due {
                touched.insert(start);
            }
        }
        if touched.is_empty() {
            return Ok(());
        }
        let projection = Self::project_interval_file(&memory, &disk.mask)?;
        drop(disk);
        self.write_interval_image(&projection, Some(&touched), snapshot_epoch, false)
    }

    /// Write the live map and replace the ahead mask. Offsets still in
    /// `pending.chunks` or `pending.written_ahead` stay `Interrupted` in the
    /// file and stay in the mask, so a later completion flush does not
    /// publish them as `Data`. Shutdown calls this after a successful
    /// `force_sync`, when nothing is pending, so the image equals the live
    /// map. Reset and rollback set the live map to `Uninitialized` first.
    ///
    /// Lock order: `interval_image_epoch`, `intervals` (released),
    /// `pending_writes` (released), then `interval_disk`. The locks are not
    /// nested. `intervals` is snapshotted before `pending_writes`, so a park
    /// that lands between the two is still projected as `Interrupted`.
    ///
    /// `force_replace` writes every submodule file even when the tracked image
    /// already matches. `reset` sets it so an in-flight rewrite cannot commit
    /// a pre-reset snapshot after this call returns.
    fn persist_intervals_authoritative(&self, force_replace: bool) -> eyre::Result<()> {
        if self.interval_flush_suppressed() {
            return Ok(());
        }
        let snapshot_epoch = self.load_interval_image_epoch();
        let mut memory = self
            .intervals
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let nondurable = self.snapshot_nondurable_offsets();
        Self::overlay_nondurable_as_interrupted(&mut memory, &nondurable);
        {
            let mut disk = self.lock_interval_disk();
            // A snapshot from before `reset` must not clear a mask that reset,
            // or a later write-ahead, already published.
            if !self.interval_snapshot_current(snapshot_epoch) {
                return Ok(());
            }
            disk.mask = Self::mask_for_offsets(&nondurable);
            disk.dirty.clear();
            if !force_replace && disk.written == memory {
                return Ok(());
            }
        }
        self.write_interval_image(&memory, None, snapshot_epoch, force_replace)
    }

    /// Rewrite `intervals.json` from the live map. Offsets still queued or
    /// parked are stored as `Interrupted`, except `durable_now`: those runs
    /// are already fsynced and presence-committed, and pending still holds
    /// them until the caller drops the fence. `only` limits the write to
    /// those submodule starts. `None` rewrites every submodule. Period `0`
    /// uses this. Batching uses [`Self::write_interval_image`] on a projection.
    ///
    /// Lock order matches [`Self::persist_intervals_authoritative`].
    fn write_intervals_files(
        &self,
        only: Option<&BTreeSet<PartitionChunkOffset>>,
        durable_now: Option<&BTreeSet<PartitionChunkOffset>>,
    ) -> eyre::Result<()> {
        let snapshot_epoch = self.load_interval_image_epoch();
        let mut memory = self
            .intervals
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut nondurable = self.snapshot_nondurable_offsets();
        if let Some(durable_now) = durable_now {
            for offset in durable_now {
                nondurable.remove(offset);
            }
        }
        Self::overlay_nondurable_as_interrupted(&mut memory, &nondurable);
        self.write_interval_image(&memory, only, snapshot_epoch, false)
    }

    /// `pending.chunks` and `pending.written_ahead`. Caller released
    /// `intervals` first. This does not take `interval_disk`.
    fn snapshot_nondurable_offsets(&self) -> BTreeSet<PartitionChunkOffset> {
        let pending = self
            .pending_writes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut offsets = BTreeSet::new();
        offsets.extend(pending.chunks.keys().copied());
        offsets.extend(pending.written_ahead.keys().copied());
        offsets
    }

    fn overlay_nondurable_as_interrupted(
        image: &mut StorageIntervals,
        nondurable: &BTreeSet<PartitionChunkOffset>,
    ) {
        for offset in nondurable {
            Self::cut_then_insert_interval_if_touching(image, *offset, ChunkType::Interrupted);
        }
    }

    fn mask_for_offsets(offsets: &BTreeSet<PartitionChunkOffset>) -> IntervalMask {
        let mut mask = IntervalMask::new();
        for offset in offsets {
            mask.insert_merge_touching_or_overlapping(ii(*offset, *offset));
        }
        mask
    }

    fn offsets_in_runs(runs: &[WriteRun]) -> BTreeSet<PartitionChunkOffset> {
        runs.iter()
            .flat_map(|run| run.offsets.iter().copied())
            .collect()
    }

    /// Atomic replace of each selected submodule file. Temp names stay
    /// `.intervals.json.*` / `intervals.json.*`. An unchanged slice is not
    /// rewritten. A successful replace updates `written` and the completion
    /// timer for that submodule.
    ///
    /// `snapshot_epoch` is the value read before `image` was copied. `reset`
    /// bumps the epoch after it publishes `Uninitialized`. A replace whose
    /// epoch no longer matches does not write the file and does not change
    /// `written` or the dirty set.
    fn write_interval_image(
        &self,
        image: &StorageIntervals,
        only: Option<&BTreeSet<PartitionChunkOffset>>,
        snapshot_epoch: u64,
        force_replace: bool,
    ) -> eyre::Result<()> {
        for (submodule_interval, submodule) in self.submodules.iter() {
            if !self.interval_snapshot_current(snapshot_epoch) {
                return Ok(());
            }
            let start = submodule_interval.start();
            if only.is_some_and(|starts| !starts.contains(&start)) {
                continue;
            }
            let Some(slice) = Self::interval_slice(image, *submodule_interval) else {
                continue;
            };
            let unchanged = !force_replace && {
                let disk = self.lock_interval_disk();
                Self::interval_slice(&disk.written, *submodule_interval).as_ref() == Some(&slice)
            };
            if unchanged {
                let mut disk = self.lock_interval_disk();
                // The epoch check and the dirty-bit clear share this lock.
                // `reset` bumps the epoch under the same lock, so a stale
                // snapshot cannot drop a dirty bit published after the bump.
                if !self.interval_snapshot_current(snapshot_epoch) {
                    return Ok(());
                }
                disk.dirty.remove(&start);
                continue;
            }
            let path = submodule
                .intervals_file
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // `reset` bumps the epoch before it takes this file lock. A
            // snapshot from before that bump must not replace the new image.
            if !self.interval_snapshot_current(snapshot_epoch) {
                return Ok(());
            }
            let mut file = get_atomic_file(path.clone())?;
            // `file` is a temporary file. `commit` renames it over the
            // original after fsync, then fsyncs the directory.
            file.write_all(serde_json::to_string(&slice)?.as_bytes())?;
            file.commit()?;
            drop(path);
            let mut disk = self.lock_interval_disk();
            if !self.interval_snapshot_current(snapshot_epoch) {
                return Ok(());
            }
            Self::splice_interval_slice(&mut disk.written, *submodule_interval, &slice);
            disk.dirty.remove(&start);
            disk.last_completion.insert(start, Instant::now());
        }
        Ok(())
    }

    fn projection_differs(
        &self,
        written: &StorageIntervals,
        projection: &StorageIntervals,
        touched: &BTreeSet<PartitionChunkOffset>,
    ) -> bool {
        for (interval, _) in self.submodules.iter() {
            if !touched.contains(&interval.start()) {
                continue;
            }
            let file_slice = Self::interval_slice(written, *interval);
            let next_slice = Self::interval_slice(projection, *interval);
            if file_slice != next_slice {
                return true;
            }
        }
        false
    }

    fn project_interval_file(
        memory: &StorageIntervals,
        mask: &IntervalMask,
    ) -> eyre::Result<StorageIntervals> {
        let mut projected = memory.clone();
        for interval in mask.iter().copied() {
            let _ = projected.cut(interval);
            projected
                .insert_merge_touching_if_values_equal(interval, ChunkType::Interrupted)
                .map_err(|error| eyre!("failed to overlay interrupted interval: {error:?}"))?;
        }
        Ok(projected)
    }

    /// Insert `run`, then walk forward. Stop at `Data`, at the submodule
    /// boundary, when the budget is spent, or when the next point is already
    /// masked. Do not jump the mask: that would add another 512 MiB on every
    /// later commit and rewrite every gap.
    fn extend_write_ahead(
        mask: &mut IntervalMask,
        memory: &StorageIntervals,
        run: &WriteRun,
        submodule: Interval<PartitionChunkOffset>,
        budget_chunks: u64,
    ) {
        let (start, end) = Self::run_bounds(run);
        mask.insert_merge_touching_or_overlapping(ii(start, end));
        let Some(mut cursor) = end.up() else {
            return;
        };
        let mut remaining = budget_chunks;
        while remaining > 0 && cursor <= submodule.end() {
            if mask.contains_point(cursor) {
                break;
            }
            let Ok((interval, chunk_type)) = memory.get_key_value_at_point(cursor) else {
                break;
            };
            // Data not in this run stays labelled Data. Marking it Interrupted
            // would make a crash reload durable bytes as Uninitialized.
            if *chunk_type == ChunkType::Data {
                break;
            }
            let mut span_end = interval.end().min(submodule.end());
            if let Some(hit) = mask.overlapping(ii(cursor, span_end)).next().copied() {
                if hit.start() <= cursor {
                    break;
                }
                let Some(before) = hit.start().down() else {
                    break;
                };
                span_end = before;
            }
            if span_end < cursor {
                break;
            }
            let span = u64::from(span_end.0 - cursor.0) + 1;
            let take = span.min(remaining);
            let extra = u32::try_from(take - 1).unwrap_or(u32::MAX);
            let taken_end = PartitionChunkOffset(cursor.0.saturating_add(extra).min(span_end.0));
            let actual = u64::from(taken_end.0 - cursor.0) + 1;
            mask.insert_merge_touching_or_overlapping(ii(cursor, taken_end));
            remaining -= actual;
            let Some(next) = taken_end.up() else {
                break;
            };
            cursor = next;
        }
    }

    fn runs_covered(mask: &IntervalMask, runs: &[WriteRun]) -> bool {
        runs.iter().all(|run| {
            let (start, end) = Self::run_bounds(run);
            mask.contains_interval(ii(start, end))
        })
    }

    fn run_bounds(run: &WriteRun) -> (PartitionChunkOffset, PartitionChunkOffset) {
        let start = run.offsets.iter().copied().min().unwrap_or(run.start);
        let end = run.offsets.iter().copied().max().unwrap_or(run.start);
        (start, end)
    }

    /// The submodule's slice of `image`. Empty input becomes one
    /// `Uninitialized` range so the file always spans the submodule.
    /// `None` when the cut intervals overlap, matching the old skip.
    fn interval_slice(
        image: &StorageIntervals,
        range: Interval<PartitionChunkOffset>,
    ) -> Option<StorageIntervals> {
        let mut working = image.clone();
        let cut: Vec<_> = working.cut(range).collect();
        let mut slice = NoditMap::from_iter_strict(cut.into_iter()).ok()?;
        if slice.is_empty() {
            slice
                .insert_merge_touching_if_values_equal(range, ChunkType::Uninitialized)
                .expect("empty slice accepts the submodule range");
        }
        Some(slice)
    }

    fn splice_interval_slice(
        written: &mut StorageIntervals,
        range: Interval<PartitionChunkOffset>,
        slice: &StorageIntervals,
    ) {
        let _ = written.cut(range);
        for (interval, chunk_type) in slice.iter() {
            let _ = written.insert_merge_touching_if_values_equal(*interval, *chunk_type);
        }
    }

    /// Raw file image. `Interrupted` stays `Interrupted` and seeds the mask.
    /// The live map is built separately and normalizes those ranges.
    fn load_interval_disk(submodules: &SubmoduleMap) -> IntervalDisk {
        let mut written = StorageIntervals::new();
        let mut mask = IntervalMask::new();
        let mut last_completion = BTreeMap::new();
        let now = Instant::now();
        for (submodule_interval, submodule) in submodules.iter() {
            last_completion.insert(submodule_interval.start(), now);
            let path = submodule
                .intervals_file
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Ok(raw) = read_intervals_file(&path) else {
                continue;
            };
            for (interval, chunk_type) in raw {
                if chunk_type == ChunkType::Interrupted {
                    mask.insert_merge_touching_or_overlapping(interval);
                }
                let _ = written.insert_merge_touching_if_values_equal(interval, chunk_type);
            }
        }
        IntervalDisk {
            written,
            mask,
            dirty: BTreeSet::new(),
            last_completion,
        }
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
    /// # Errors
    /// A submodule `intervals.json` that cannot be read or parsed. The error
    /// names that file. Stop the node, move the file aside, and reset that
    /// storage module so it can repack.
    ///
    /// # Panics
    /// * If unable to lock a submodule's intervals file mutex
    fn load_intervals_from_submodules(
        submodules: &SubmoduleMap,
        module_id: usize,
    ) -> eyre::Result<StorageIntervals> {
        let mut global_intervals = StorageIntervals::new();
        let mut interrupted_count = 0;

        for (_, submodule) in submodules.iter() {
            let file = submodule
                .intervals_file
                .lock()
                .expect("to lock the submodule intervals file mutex");
            let submodule_intervals = read_intervals_file(&file)?;

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
                    .map_err(|error| {
                        eyre!(
                            "failed to insert interval from {}: {error:?}. {INTERVALS_FILE_REMEDY}",
                            file.display()
                        )
                    })?;
            }
        }

        if interrupted_count > 0 {
            error!(
                "Found {} interrupted writes in storage module {}",
                interrupted_count, module_id
            );
        }

        Ok(global_intervals)
    }

    /// Drop presence rows whose loaded interval is not `Data`.
    ///
    /// Runs after `Interrupted` has been reloaded as `Uninitialized`, and
    /// before group commit or the disk lane. Cost is one in-memory walk of
    /// the interval map, then one seek per non-`Data` span per presence
    /// family, plus the rows removed. A fully `Data` module does not touch
    /// the index. A span with no presence row does not commit. A capacity
    /// partition that is all `Entropy` and has an empty index is those seeks
    /// and no write.
    ///
    /// On a checksum index, an `Entropy` span keeps a checksum-only row and
    /// drops a proof row. The checksum on a proof row described data bytes,
    /// and this pass does not read `chunks.dat` to prove otherwise.
    /// `Uninitialized` drops every row. MDBX and schema v2 drop every
    /// non-`Data` row.
    fn reconcile_presence(
        submodules: &SubmoduleMap,
        intervals: &StorageIntervals,
    ) -> eyre::Result<()> {
        for (submodule_interval, submodule) in submodules.iter() {
            let spans = Self::non_data_spans(intervals, *submodule_interval);
            if spans.is_empty() {
                continue;
            }
            if submodule.db.stores_offset_crc() {
                let mut entropy = Vec::new();
                let mut dropped = Vec::new();
                for span in spans {
                    if intervals.get_at_point(span.0).copied() == Some(ChunkType::Entropy) {
                        entropy.push(span);
                    } else {
                        dropped.push(span);
                    }
                }
                if !entropy.is_empty() {
                    let stats = submodule.db.retain_entropy_crc_rows(&entropy)?;
                    Self::note_reconciled(submodule, &stats, &entropy);
                }
                if !dropped.is_empty() {
                    let stats = submodule.db.drop_presence_in_ranges(&dropped)?;
                    Self::note_reconciled(submodule, &stats, &dropped);
                }
            } else {
                let stats = submodule.db.drop_presence_in_ranges(&spans)?;
                Self::note_reconciled(submodule, &stats, &spans);
            }
        }
        Ok(())
    }

    fn note_reconciled(
        submodule: &StorageSubmodule,
        stats: &irys_database::submodule::PresenceReconcile,
        spans: &[(PartitionChunkOffset, PartitionChunkOffset)],
    ) {
        if stats.path_hash_rows == 0 && stats.data_path_rows == 0 {
            return;
        }
        warn!(
            submodule = %submodule.path.display(),
            path_hash_rows = stats.path_hash_rows,
            data_path_rows = stats.data_path_rows,
            ranges = ?spans,
            "removed presence rows under intervals that are not Data"
        );
        index_read_metrics::note_presence_reconciled(
            stats.path_hash_rows.saturating_add(stats.data_path_rows),
        );
    }

    /// Finish a reset that crashed after the marker was written.
    ///
    /// The index is cleared again. That submodule's interval file becomes
    /// `Uninitialized` for its whole span. Then the marker is dropped.
    fn finish_pending_resets(submodules: &SubmoduleMap) -> eyre::Result<()> {
        for (interval, submodule) in submodules.iter() {
            if !submodule.db.reset_in_progress()? {
                continue;
            }
            // The reset marker won the crash. The flag clear may not have.
            // `clear` does not touch the Metadata family.
            submodule.db.set_chunk_crc_complete(false)?;
            submodule.crc_complete.store(false, Ordering::SeqCst);
            submodule.db.update(|tx| tx.clear())?;
            let path = submodule
                .intervals_file
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let mut map = StorageIntervals::new();
            map.insert_strict(*interval, ChunkType::Uninitialized)
                .map_err(|err| eyre::eyre!("reset interval span: {err:?}"))?;
            let mut file = get_atomic_file(&path)?;
            file.write_all(serde_json::to_string(&map)?.as_bytes())?;
            file.commit()?;
            submodule.db.clear_reset_in_progress()?;
        }
        Ok(())
    }

    /// Drop tx intervals, data-root placements, and pending jobs under
    /// `Uninitialized` spans. A tx path and its leaf binding are deleted when
    /// no interval in that submodule still names the hash.
    ///
    /// `Entropy` is not included. A tx can be indexed there before its bytes
    /// are stored. Presence under `Entropy` is still removed by
    /// [`Self::reconcile_presence`]. A span with no coverage row does not commit.
    fn reconcile_uninitialized_coverage(
        submodules: &SubmoduleMap,
        intervals: &StorageIntervals,
        chunk_size: u64,
    ) -> eyre::Result<()> {
        for (submodule_interval, submodule) in submodules.iter() {
            let spans = Self::uninitialized_spans(intervals, *submodule_interval);
            if spans.is_empty() {
                continue;
            }
            submodule
                .db
                .drop_uninitialized_coverage(&spans, chunk_size)?;
        }
        Ok(())
    }

    /// Inclusive `Uninitialized` spans clipped to one submodule.
    fn uninitialized_spans(
        intervals: &StorageIntervals,
        submodule: Interval<PartitionChunkOffset>,
    ) -> Vec<(PartitionChunkOffset, PartitionChunkOffset)> {
        let mut spans = Vec::new();
        for (interval, kind) in intervals.overlapping(submodule) {
            if *kind != ChunkType::Uninitialized {
                continue;
            }
            let start = interval.start().max(submodule.start());
            let end = interval.end().min(submodule.end());
            if start <= end {
                spans.push((start, end));
            }
        }
        spans
    }

    /// Inclusive non-`Data` spans clipped to one submodule.
    fn non_data_spans(
        intervals: &StorageIntervals,
        submodule: Interval<PartitionChunkOffset>,
    ) -> Vec<(PartitionChunkOffset, PartitionChunkOffset)> {
        let mut spans = Vec::new();
        for (interval, kind) in intervals.overlapping(submodule) {
            if *kind == ChunkType::Data {
                continue;
            }
            let start = interval.start().max(submodule.start());
            let end = interval.end().min(submodule.end());
            if start <= end {
                spans.push((start, end));
            }
        }
        spans
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

    /// How mining recall is armed.
    ///
    /// `buffered` when `storage.recall_direct_io` is false. `direct` when the
    /// `O_DIRECT` descriptor is open. `fallback` when the flag is set and the
    /// open failed. An unaligned range still uses the buffered descriptor for
    /// that read and is counted as `fallback`.
    pub fn recall_io_mode(&self) -> &'static str {
        let requested = self.config.node_config.storage.recall_direct_io;
        let opened = self
            .submodules
            .iter()
            .any(|(_, submodule)| submodule.recall_direct.is_some());
        match (requested, opened) {
            (false, _) => recall_direct::MODE_BUFFERED,
            (true, true) => recall_direct::MODE_DIRECT,
            (true, false) => recall_direct::MODE_FALLBACK,
        }
    }

    fn record_recall_io(&self, mode: &'static str) {
        index_read_metrics::note_recall_io(mode);
        #[cfg(test)]
        {
            let counter = match mode {
                recall_direct::MODE_DIRECT => &self.recall_io.direct,
                recall_direct::MODE_FALLBACK => &self.recall_io.fallback,
                _ => &self.recall_io.buffered,
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[cfg(test)]
    fn recall_io_counts_for_test(&self) -> (u64, u64, u64) {
        (
            self.recall_io.direct.load(Ordering::Relaxed),
            self.recall_io.buffered.load(Ordering::Relaxed),
            self.recall_io.fallback.load(Ordering::Relaxed),
        )
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
        Ok(self.read_chunks_inner(chunk_range, false, true)?.0)
    }

    fn read_chunks_inner(
        &self,
        chunk_range: Interval<PartitionChunkOffset>,
        recall: bool,
        check_crc: bool,
    ) -> eyre::Result<(ChunkMap, BTreeMap<PartitionChunkOffset, OffsetMeta>)> {
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
            let mut run_len = 0_u64;
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
                        // Pending bytes are still in memory. No durable crc
                        // entry exists for them yet, so this read does not
                        // check a checksum.
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

        let caller = if recall {
            index_read_metrics::RECALL
        } else {
            index_read_metrics::caller()
        };
        let rows = self.read_disk_runs(&disk_runs, &mut chunk_map, recall, check_crc, caller)?;

        // A chunk that landed in pending while the disk read held the file
        // lock must win over the bytes just read.
        let pending = self.pending_writes.read().unwrap();
        for offset in visited {
            if let Some((bytes, chunk_type)) = pending.get(&offset) {
                chunk_map.insert(offset, (bytes.clone(), *chunk_type));
            }
        }
        drop(pending);

        Ok((chunk_map, rows))
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
        check_crc: bool,
        caller: &'static str,
    ) -> eyre::Result<BTreeMap<PartitionChunkOffset, OffsetMeta>> {
        let mut rows = BTreeMap::new();
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
            let fallback = Arc::clone(&submodule.file);
            #[cfg(feature = "bench")]
            let split_files = submodule.split.as_ref().map(|split| {
                (
                    Arc::clone(&split.recall),
                    Arc::clone(&split.write),
                    split.write_origin,
                    split.write_chunks,
                    split.recall_origin,
                )
            });
            #[cfg(feature = "bench")]
            let file_arc = if let Some((recall_file, write_file, ..)) = &split_files {
                if recall || runs[index].2 != ChunkType::Data {
                    Arc::clone(recall_file)
                } else {
                    Arc::clone(write_file)
                }
            } else {
                Arc::clone(&fallback)
            };
            #[cfg(not(feature = "bench"))]
            let file_arc = fallback;
            #[cfg(feature = "bench")]
            drop(fallback);
            let mut group = Vec::new();
            if recall {
                // `file_arc` is the buffered descriptor. Mining recall uses
                // `recall_direct` when that descriptor is open and the range
                // is 4096-byte aligned. A dirty buffered page for the same
                // range is written back by the kernel before the direct read,
                // so `written_ahead` bytes (pwritten, not yet fdatasync'd)
                // come back as the new bytes. `pending.chunks` is overlaid
                // after this function returns.
                let buffered = {
                    let guard = file_arc
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard
                        .try_clone()
                        .wrap_err("recall could not dup chunks.dat")?
                };
                let direct = match submodule.recall_direct.as_ref() {
                    Some(file) => Some(
                        file.try_clone()
                            .wrap_err("recall could not dup the O_DIRECT descriptor")?,
                    ),
                    None => None,
                };
                let direct_requested = self.config.node_config.storage.recall_direct_io;
                #[cfg(feature = "bench")]
                let origin_chunks = split_files
                    .as_ref()
                    .map(|(_, _, _, _, origin)| *origin)
                    .unwrap_or(0);
                #[cfg(not(feature = "bench"))]
                let origin_chunks = 0;
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
                        &buffered,
                        direct.as_ref(),
                        direct_requested,
                        start,
                        len,
                        chunk_type,
                        run_interval.start(),
                        origin_chunks,
                        chunk_size,
                        chunk_len,
                        max_chunks,
                        chunk_map,
                        &|mode| self.record_recall_io(mode),
                    )?;
                    group.push((start, len, chunk_type));
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
                    #[cfg(feature = "bench")]
                    if let Some((recall_file, write_file, ..)) = &split_files {
                        let run_write = chunk_type == ChunkType::Data;
                        let opened_write = Arc::ptr_eq(&file_arc, write_file);
                        let opened_recall = Arc::ptr_eq(&file_arc, recall_file);
                        if run_write != opened_write || (!run_write && !opened_recall) {
                            break;
                        }
                    }
                    let mut done = 0_u64;
                    while done < len {
                        let n = (len - done).min(max_chunks);
                        let piece = PartitionChunkOffset(start.0 + done as u32);
                        let relative = *(piece - run_interval.start());
                        let file_offset = u64::from(relative) * chunk_size;
                        #[cfg(feature = "bench")]
                        let file_offset = if chunk_type == ChunkType::Data
                            && let Some((_, _, write_origin, write_chunks, _)) =
                                split_files.as_ref()
                        {
                            split_backing::map_write_offset(
                                *write_origin,
                                *write_chunks,
                                relative,
                                chunk_size,
                            )?
                        } else if let Some((_, _, _, _, origin)) = split_files.as_ref() {
                            recall_file_byte_offset(*origin, relative, chunk_size)
                        } else {
                            file_offset
                        };
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
                    group.push((start, len, chunk_type));
                    index += 1;
                }
                drop(file);
            }
            // The chunk-file lock is not held. A mismatch takes the interval
            // lock, which must not nest under the file lock.
            if check_crc && self.crc_covers_file(&file_arc) {
                // Recall still holds `begin_recall`. Confirming under
                // `sync_in_progress` here deadlocks with `yield_to_recall`.
                rows.extend(self.reject_settled_crc(&group, chunk_map, caller, recall)?);
            }
        }
        Ok(rows)
    }

    /// Bench recall bytes are not the bytes the offset-row checksum describes.
    /// Production has one file, so every settled read is checked.
    fn crc_covers_file(&self, file: &Arc<Mutex<File>>) -> bool {
        #[cfg(feature = "bench")]
        {
            for (_, submodule) in self.submodules.iter() {
                if let Some(split) = submodule.split.as_ref()
                    && Arc::ptr_eq(file, &split.recall)
                {
                    return false;
                }
            }
        }
        #[cfg(not(feature = "bench"))]
        let _ = file;
        true
    }

    /// Drop settled chunks whose checksum does not match. Unknown rows are
    /// kept and counted. `Interrupted` runs and offsets still in
    /// `written_ahead` or `pending.chunks` are not checked.
    ///
    /// One range read covers the group's offsets. The returned map is every
    /// stored row in that span, including checksum-only rows.
    ///
    /// A mismatch here is only a suspect. The bytes were read before this
    /// row load, and a presence commit can land between the two. `recall`
    /// omits the chunk and queues it. Any other caller confirms under
    /// `sync_in_progress` before `note_crc_mismatch`. A false suspect is
    /// put back from the fresh read.
    fn reject_settled_crc(
        &self,
        runs: &[(PartitionChunkOffset, u64, ChunkType)],
        chunk_map: &mut ChunkMap,
        caller: &'static str,
        recall: bool,
    ) -> eyre::Result<BTreeMap<PartitionChunkOffset, OffsetMeta>> {
        let Some(&(first, _, _)) = runs.first() else {
            return Ok(BTreeMap::new());
        };
        let (_, submodule) = self.get_submodule_for_offset(first)?;
        if !submodule.db.stores_offset_crc() {
            return Ok(BTreeMap::new());
        }
        let mut min = u32::MAX;
        let mut max = 0_u32;
        let mut any = false;
        for &(start, len, chunk_type) in runs {
            if !matches!(chunk_type, ChunkType::Data | ChunkType::Entropy) || len == 0 {
                continue;
            }
            any = true;
            min = min.min(start.0);
            let end = start
                .0
                .saturating_add(u32::try_from(len.saturating_sub(1)).unwrap_or(u32::MAX));
            max = max.max(end);
        }
        if !any {
            return Ok(BTreeMap::new());
        }
        #[cfg(test)]
        self.wait_if_crc_check_paused();
        let stored = submodule.db.view(|tx| {
            tx.offset_metas_in_range(PartitionChunkOffset(min), PartitionChunkOffset(max))
        })?;
        let by_offset: BTreeMap<_, _> = stored.into_iter().collect();
        let complete = submodule.crc_complete.load(Ordering::SeqCst);
        let mut mismatches = Vec::new();
        for &(start, len, chunk_type) in runs {
            if !matches!(chunk_type, ChunkType::Data | ChunkType::Entropy) || len == 0 {
                continue;
            }
            for step in 0..len {
                let off = PartitionChunkOffset(start.0 + step as u32);
                if self.crc_bytes_are_unsettled(off) {
                    continue;
                }
                let Some((bytes, _)) = chunk_map.get(&off) else {
                    continue;
                };
                let crc = by_offset.get(&off).and_then(|meta| meta.crc);
                match chunk_crc::classify(crc, bytes, complete) {
                    chunk_crc::CrcVerdict::Unknown => chunk_crc::note_unknown(caller),
                    chunk_crc::CrcVerdict::Mismatch => mismatches.push((off, chunk_type)),
                    chunk_crc::CrcVerdict::Match | chunk_crc::CrcVerdict::Skip => {}
                }
            }
        }
        for (offset, _) in &mismatches {
            chunk_map.remove(offset);
        }
        for (offset, kind) in mismatches {
            if recall {
                self.crc_suspects
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(offset);
                continue;
            }
            match self.note_if_crc_mismatch(offset, kind, caller)? {
                CrcConfirm::NotConfirmed(bytes) => {
                    chunk_map.insert(offset, (bytes, kind));
                }
                CrcConfirm::Confirmed | CrcConfirm::Unsettled => {}
            }
        }
        Ok(by_offset)
    }

    /// One suspect offset, judged again under `sync_in_progress`.
    ///
    /// `Confirmed` has already called `note_crc_mismatch`. `NotConfirmed`
    /// carries the fresh bytes that match the stable row. `Unsettled` is
    /// still in `pending` or `written_ahead`: the caller omits it and does
    /// not repair it.
    fn note_if_crc_mismatch(
        &self,
        offset: PartitionChunkOffset,
        kind: ChunkType,
        caller: &'static str,
    ) -> eyre::Result<CrcConfirm> {
        let verdict = self.confirm_crc_mismatch(offset)?;
        if matches!(verdict, CrcConfirm::Confirmed) {
            self.note_crc_mismatch(offset, kind, caller);
        }
        Ok(verdict)
    }

    /// Re-read one offset and its checksum under `sync_in_progress`.
    ///
    /// Every write to an offset is in `pending.chunks` before its pwrite and
    /// stays in `pending.chunks` or `pending.written_ahead` until
    /// `finish_durable_point` removes it. `commit_presence_rows` writes the
    /// checksum, then that removal runs, and both run while the caller holds
    /// this lock. An offset in neither map therefore has stable bytes and a
    /// stable checksum row for as long as this lock is held.
    ///
    /// Lock order matches `finish_durable_point`: `sync_in_progress`, then
    /// `pending_writes`, then the `chunks.dat` lock. Release this lock before
    /// `note_crc_mismatch`, which takes `intervals`.
    fn confirm_crc_mismatch(&self, offset: PartitionChunkOffset) -> eyre::Result<CrcConfirm> {
        let _sync_guard = self
            .sync_in_progress
            .lock()
            .map_err(|error| eyre::eyre!("storage-module sync lock poisoned: {error}"))?;
        if self.crc_bytes_are_unsettled(offset) {
            return Ok(CrcConfirm::Unsettled);
        }
        let bytes = self.pread_settled_chunk(offset)?;
        let (_, submodule) = self.get_submodule_for_offset(offset)?;
        let crc = if submodule.db.stores_offset_crc() {
            submodule
                .db
                .view(|tx| tx.get_offset_meta(offset))?
                .and_then(|row| row.crc)
        } else {
            None
        };
        // A `write_chunk` can land in `pending` during the pread. The pwrite
        // cannot: it needs this lock. The bytes above are still the previous
        // generation. Treat the offset as not yet the durable pair.
        if self.crc_bytes_are_unsettled(offset) {
            return Ok(CrcConfirm::Unsettled);
        }
        if !submodule.db.stores_offset_crc() {
            return Ok(CrcConfirm::NotConfirmed(bytes));
        }
        let complete = submodule.crc_complete.load(Ordering::SeqCst);
        match chunk_crc::classify(crc, &bytes, complete) {
            chunk_crc::CrcVerdict::Mismatch => Ok(CrcConfirm::Confirmed),
            _ => Ok(CrcConfirm::NotConfirmed(bytes)),
        }
    }

    /// One `pread` of the buffered `chunks.dat` descriptor.
    ///
    /// The caller holds `sync_in_progress`, so no pwrite runs during this read.
    fn pread_settled_chunk(&self, offset: PartitionChunkOffset) -> eyre::Result<ChunkBytes> {
        let mapped = self.get_chunk_type(&offset) == Some(ChunkType::Data);
        let (file_arc, file_offset, _) = self.locate_io(offset, mapped)?;
        let mut buf = vec![0_u8; self.config.consensus.chunk_size as usize];
        let file = self.disk.lock_chunks(&file_arc);
        file.read_exact_at(&mut buf, file_offset)
            .wrap_err_with(|| format!("crc confirm read at offset {offset}"))?;
        Ok(buf)
    }

    /// Judge recall suspects. Safe to call when the recall hold is not held.
    ///
    /// The storage-module service calls this on its tick, including while the
    /// disk lane is running. `read_recall_chunks` calls it only when that
    /// lane is not running, after it has dropped the recall hold.
    pub fn confirm_crc_suspects(&self) {
        let suspects = {
            let mut queued = self
                .crc_suspects
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *queued)
        };
        let mut retry = BTreeSet::new();
        for offset in suspects {
            let kind = self
                .intervals
                .read()
                .unwrap()
                .get_at_point(offset)
                .copied()
                .unwrap_or(ChunkType::Uninitialized);
            match self.note_if_crc_mismatch(offset, kind, index_read_metrics::RECALL) {
                Ok(_) => {}
                Err(error) => {
                    warn!(
                        module_id = self.id,
                        offset = offset.0,
                        "chunk crc confirm failed; the offset stays suspect: {error}"
                    );
                    retry.insert(offset);
                }
            }
        }
        if retry.is_empty() {
            return;
        }
        self.crc_suspects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(retry);
    }

    /// Pending and written-ahead bytes are not the durable checksum's bytes.
    fn crc_bytes_are_unsettled(&self, offset: PartitionChunkOffset) -> bool {
        let pending = self.pending_writes.read().unwrap();
        pending.contains_key(&offset) || pending.written_ahead.contains_key(&offset)
    }

    /// One verdict per chunk in a dense `buf` that starts at `start`.
    /// Unsettled offsets are `Skip`. An engine with no checksum row is `Skip`
    /// for the whole span, and this function does not count that. Metrics are
    /// left to the caller. A span that crosses a submodule is one range read
    /// per submodule.
    fn crc_span_verdicts(
        &self,
        start: PartitionChunkOffset,
        buf: &[u8],
    ) -> eyre::Result<Vec<chunk_crc::CrcVerdict>> {
        let chunk_len = usize::try_from(self.config.consensus.chunk_size.max(1))
            .wrap_err("chunk size does not fit usize")?;
        if chunk_len == 0 || !buf.len().is_multiple_of(chunk_len) {
            return Err(eyre!(
                "crc span of {} bytes is not a multiple of {chunk_len}",
                buf.len()
            ));
        }
        let count = buf.len() / chunk_len;
        if count == 0 {
            return Ok(Vec::new());
        }
        let last = u32::try_from(count - 1).wrap_err("crc span is longer than u32::MAX chunks")?;
        let end_off = start
            .0
            .checked_add(last)
            .ok_or_else(|| eyre!("crc span at {start:?} overflows"))?;
        let mut settled = vec![false; count];
        {
            let intervals = self.intervals.read().unwrap();
            for (step, flag) in settled.iter_mut().enumerate() {
                let off = PartitionChunkOffset(start.0 + step as u32);
                *flag = matches!(
                    intervals.get_at_point(off).copied(),
                    Some(ChunkType::Data | ChunkType::Entropy)
                );
            }
        }
        {
            let pending = self.pending_writes.read().unwrap();
            for (step, flag) in settled.iter_mut().enumerate() {
                let off = PartitionChunkOffset(start.0 + step as u32);
                if pending.contains_key(&off) || pending.written_ahead.contains_key(&off) {
                    *flag = false;
                }
            }
        }
        let mut verdicts = vec![chunk_crc::CrcVerdict::Skip; count];
        let mut cursor = start.0;
        while cursor <= end_off {
            let off = PartitionChunkOffset(cursor);
            let (interval, submodule) = self.get_submodule_for_offset(off)?;
            let slice_end = (*interval.end()).min(end_off);
            let base = (cursor - start.0) as usize;
            let steps = (slice_end - cursor) as usize + 1;
            if submodule.db.stores_offset_crc() {
                let stored = submodule
                    .db
                    .view(|tx| tx.offset_metas_in_range(off, PartitionChunkOffset(slice_end)))?;
                let by_offset: BTreeMap<_, _> = stored.into_iter().collect();
                let complete = submodule.crc_complete.load(Ordering::SeqCst);
                for step in 0..steps {
                    let idx = base + step;
                    if !settled[idx] {
                        continue;
                    }
                    let chunk_off = PartitionChunkOffset(cursor + step as u32);
                    let at = idx * chunk_len;
                    let crc = by_offset.get(&chunk_off).and_then(|meta| meta.crc);
                    verdicts[idx] = chunk_crc::classify(crc, &buf[at..at + chunk_len], complete);
                }
            }
            if slice_end == end_off || slice_end == u32::MAX {
                break;
            }
            cursor = slice_end + 1;
        }
        Ok(verdicts)
    }

    /// One settled chunk. `Skip` means the bytes are not durable yet, the
    /// label is `Interrupted` / `Uninitialized`, or this index stores no checksum.
    fn crc_verdict(
        &self,
        offset: PartitionChunkOffset,
        bytes: &[u8],
        caller: &'static str,
    ) -> eyre::Result<chunk_crc::CrcVerdict> {
        {
            let pending = self.pending_writes.read().unwrap();
            if pending.contains_key(&offset) || pending.written_ahead.contains_key(&offset) {
                return Ok(chunk_crc::CrcVerdict::Skip);
            }
        }
        let kind = self.intervals.read().unwrap().get_at_point(offset).copied();
        if !matches!(kind, Some(ChunkType::Data | ChunkType::Entropy)) {
            return Ok(chunk_crc::CrcVerdict::Skip);
        }
        let (_, submodule) = self.get_submodule_for_offset(offset)?;
        if !submodule.db.stores_offset_crc() {
            return Ok(chunk_crc::CrcVerdict::Skip);
        }
        let crc = submodule
            .db
            .view(|tx| tx.get_offset_meta(offset))?
            .and_then(|row| row.crc);
        let verdict =
            chunk_crc::classify(crc, bytes, submodule.crc_complete.load(Ordering::SeqCst));
        if matches!(verdict, chunk_crc::CrcVerdict::Unknown) {
            chunk_crc::note_unknown(caller);
        }
        Ok(verdict)
    }

    /// Mark one offset untrustworthy and hand it to the packing queue.
    ///
    /// The live map becomes `Uninitialized`, so a later read does not serve
    /// the bytes. The interval file is not rewritten here: an authoritative
    /// rewrite would drop the write-ahead mask. Packing's own write stores
    /// the new label. A crash before that reloads the old label and the next
    /// read queues the offset again. The offset index is left in place so
    /// data sync can refill the body after entropy is packed.
    fn note_crc_mismatch(
        &self,
        offset: PartitionChunkOffset,
        kind: ChunkType,
        caller: &'static str,
    ) {
        chunk_crc::note_mismatch(caller, kind);
        chunk_crc::warn_mismatch(self.id, offset.0);
        {
            let mut intervals = self.intervals.write().unwrap();
            Self::cut_then_insert_interval_if_touching(
                &mut intervals,
                offset,
                ChunkType::Uninitialized,
            );
        }
        self.crc_repairs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(offset);
    }

    /// Contiguous repair ranges. The storage-module service drains these.
    pub fn take_crc_repair_ranges(&self) -> Vec<Interval<PartitionChunkOffset>> {
        let offsets = std::mem::take(
            &mut *self
                .crc_repairs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let mut ranges = Vec::new();
        let mut iter = offsets.into_iter();
        let Some(first) = iter.next() else {
            return ranges;
        };
        let mut start = first;
        let mut prev = first;
        for offset in iter {
            if offset.0 == prev.0.saturating_add(1) {
                prev = offset;
                continue;
            }
            ranges.push(ii(start, prev));
            start = offset;
            prev = offset;
        }
        ranges.push(ii(start, prev));
        ranges
    }

    /// Put a range back when the packing channel did not accept it.
    pub fn requeue_crc_repair_range(&self, range: Interval<PartitionChunkOffset>) {
        let mut repairs = self
            .crc_repairs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut raw = range.start().0;
        loop {
            repairs.insert(PartitionChunkOffset::from(raw));
            if raw == range.end().0 {
                break;
            }
            raw = raw.saturating_add(1);
        }
    }

    /// Clear `crc_complete` and allow the settled scan to run again.
    ///
    /// Called before a reset rewrites the module, when a crashed reset is
    /// finished on open, and before partition recovery repacks a range.
    /// One mismatched offset does not clear the flag. `clear` does not
    /// touch the Metadata family, so this deletes the flag itself.
    pub fn clear_chunk_crc_complete(&self) -> eyre::Result<()> {
        self.crc_scan_done.store(false, Ordering::SeqCst);
        for (_, submodule) in self.submodules.iter() {
            submodule.db.set_chunk_crc_complete(false)?;
            submodule.crc_complete.store(false, Ordering::SeqCst);
        }
        Ok(())
    }

    /// Set `crc_complete` once every offset is settled and every offset has a checksum.
    ///
    /// A converted module is settled and has proofs with no checksum. The scan
    /// records that and leaves the flag unset, so a missing checksum stays
    /// unknown. Reset and repack clear the flag and allow another scan. A scan
    /// error does not fail the durable point and does not stick the flag.
    fn maybe_mark_crc_complete(&self) {
        if self.crc_scan_done.load(Ordering::SeqCst) {
            return;
        }
        if !self
            .submodules
            .iter()
            .any(|(_, submodule)| submodule.db.stores_offset_crc())
        {
            return;
        }
        {
            let intervals = self.intervals.read().unwrap();
            let settled = intervals
                .iter()
                .all(|(_, kind)| matches!(*kind, ChunkType::Data | ChunkType::Entropy));
            if !settled {
                return;
            }
        }
        {
            let pending = self.pending_writes.read().unwrap();
            if !pending.is_empty() || !pending.written_ahead.is_empty() {
                return;
            }
        }
        for (interval, submodule) in self.submodules.iter() {
            if !submodule.db.stores_offset_crc() {
                continue;
            }
            match submodule.db.view(|tx| {
                tx.offset_crc_missing(
                    PartitionChunkOffset(*interval.start()),
                    PartitionChunkOffset(*interval.end()),
                )
            }) {
                Ok(true) => {
                    self.crc_scan_done.store(true, Ordering::SeqCst);
                    return;
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(
                        module_id = self.id,
                        path = %submodule.path.display(),
                        "chunk crc scan failed; crc_complete stays unset: {error}"
                    );
                    return;
                }
            }
        }
        for (_, submodule) in self.submodules.iter() {
            if !submodule.db.stores_offset_crc() {
                continue;
            }
            if let Err(error) = submodule.db.set_chunk_crc_complete(true) {
                tracing::warn!(
                    module_id = self.id,
                    path = %submodule.path.display(),
                    "chunk crc flag update failed; crc_complete stays unset: {error}"
                );
                return;
            }
            submodule.crc_complete.store(true, Ordering::SeqCst);
        }
        self.crc_scan_done.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn abandon_on_drop_for_test(&self) {
        self.abandon_on_drop.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn chunk_crc_complete_for_test(&self) -> bool {
        let mut any = false;
        for (_, submodule) in self.submodules.iter() {
            if !submodule.db.stores_offset_crc() {
                continue;
            }
            any = true;
            if !submodule.crc_complete.load(Ordering::SeqCst) {
                return false;
            }
        }
        any
    }

    #[cfg(test)]
    fn offset_meta_for_test(
        &self,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<Option<OffsetMeta>> {
        let (_, submodule) = self.get_submodule_for_offset(offset)?;
        submodule.db.view(|tx| tx.get_offset_meta(offset))
    }

    /// Replace the checksum at `offset`. `None` with no proof deletes the row.
    #[cfg(test)]
    fn set_offset_crc_for_test(
        &self,
        offset: PartitionChunkOffset,
        crc: Option<u32>,
    ) -> eyre::Result<()> {
        let (_, submodule) = self.get_submodule_for_offset(offset)?;
        submodule.db.update(|tx| {
            let proof = tx.get_offset_meta(offset)?.and_then(|row| row.proof);
            tx.write_packed_offset_rows(vec![PackedOffsetRow { offset, crc, proof }])?;
            Ok(())
        })
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
        // Data lives in the write window when a bench split is installed.
        // Every other type stays on the entropy file. With no split both are
        // `chunks.dat` at the submodule-relative offset.
        let mapped = self.get_chunk_type(&chunk_offset) == Some(ChunkType::Data);
        let (file_arc, file_offset, _) = self.locate_io(chunk_offset, mapped)?;
        let mut buf = vec![0_u8; self.config.consensus.chunk_size as usize];

        // Positional read. A seek on the shared handle would move the cursor
        // seen by the next reader of this file.
        let file = self.disk.lock_chunks(&file_arc);
        file.read_exact_at(&mut buf, file_offset)
            .wrap_err_with(|| format!("chunk read at offset {chunk_offset}"))?;
        drop(file);

        // A bench recall file is not the bytes the offset-row checksum describes.
        if self.crc_covers_file(&file_arc) {
            let caller = index_read_metrics::caller();
            if matches!(
                self.crc_verdict(chunk_offset, &buf, caller)?,
                chunk_crc::CrcVerdict::Mismatch
            ) {
                let kind = self
                    .intervals
                    .read()
                    .unwrap()
                    .get_at_point(chunk_offset)
                    .copied()
                    .unwrap_or(ChunkType::Uninitialized);
                match self.note_if_crc_mismatch(chunk_offset, kind, caller)? {
                    CrcConfirm::NotConfirmed(fresh) => return Ok(fresh),
                    CrcConfirm::Confirmed => {
                        return Err(eyre!("chunk crc mismatch at offset {chunk_offset}"));
                    }
                    // The next read sees the committed bytes.
                    CrcConfirm::Unsettled => return Ok(buf),
                }
            }
        }

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
        pending.written_ahead.remove(&chunk_offset);
        pending.insert(chunk_offset, (bytes, chunk_type));
        pending.priorities.insert(chunk_offset, priority);
        pending.queued_at.insert(chunk_offset, Instant::now());
        // A raw write has no data-path proof. Drop a proof left by a packed
        // chunk this write replaced, so the fsync cannot publish that path.
        pending.proofs.remove(&chunk_offset);
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
            pending
                .proofs
                .retain(|offset, _| *offset < start || *offset > end);
            pending
                .written_ahead
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
            submodule.db.update(|tx| {
                // Removing coverage makes heal see a hole. Deleting the path
                // rows makes a later read miss the data path.
                tx.clear_paths_in_inclusive_range(
                    PartitionChunkOffset::from(cursor),
                    PartitionChunkOffset::from(slice_end),
                )
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
            submodule.db.update(|tx| {
                // Schema v3 data-root rows belong to their intervals. Clearing
                // the interval removes the row. Trimming the info list here
                // would delete rows whose interval fragments remain.
                if tx.intervals_own_data_root_rows() {
                    return Ok(());
                }
                for data_root in data_roots {
                    let Some(infos) = tx.get_data_root_infos_for_data_root(*data_root)? else {
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
                    tx.set_data_root_infos_for_data_root(*data_root, DataRootInfos(remaining))?;
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

    /// Publish RocksDB index gauges for every submodule. MDBX indexes do nothing.
    pub fn report_index_metrics(&self) {
        for (_, submodule) in self.submodules.iter() {
            submodule.db.report_metrics();
        }
    }

    /// MDBX registrations per sync. RocksDB does not sync on this count.
    /// Chunk migration syncs each touched submodule once per block. The
    /// WAL backstop or shutdown covers a group that is still open.
    const ROCKS_INDEX_TXS_PER_SYNC: u32 = 8;

    /// Make index registration commits visible without an fsync on every call.
    ///
    /// MDBX syncs every `txs_per_sync` registrations, on the next durable
    /// index write, or after 50 ms. RocksDB ignores the count. A registration
    /// stays unsynced until the next synced write on that database, the
    /// configured WAL backstop, or clean shutdown. [`Self::new`] turns this
    /// on for a RocksDB index and sets the backstop from
    /// `storage.index_wal_backstop_ms`. An MDBX index does not call it, so
    /// each of its registrations is durable before it returns.
    /// `txs_per_sync` of 0 is rejected.
    pub fn enable_index_group_commit(&self, txs_per_sync: u32) -> eyre::Result<()> {
        for (_, submodule) in self.submodules.iter() {
            submodule.db.enable_group_commit(txs_per_sync)?;
        }
        Ok(())
    }

    /// Make this module's visible index rows durable.
    ///
    /// One synced WAL write per submodule that still has unsynced rows.
    /// A clean submodule does not sync. MDBX with group commit off returns
    /// without a sync: each registration was already durable. Do not call
    /// this while holding an index write lock.
    pub fn sync_index_registrations(&self) -> eyre::Result<()> {
        for (_, submodule) in self.submodules.iter() {
            submodule.db.sync_group()?;
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
        self.index_transaction_data_in(data_tx, tx_path, chunk_range, block_height, None, None)
    }

    /// [`Self::index_transaction_data`], plus the ledger placement that owns this copy.
    ///
    /// `ledger` and `tx_index` locate the tx in the block whose ledger tree
    /// covers this placement. Publish uses the promotion block, not the Submit
    /// inclusion. `None` leaves those fields unset. Schema v2 ignores them.
    ///
    /// The rows are computed for the assignment captured at the start. The
    /// commit holds the assignment read lock and writes only when that
    /// assignment is still current. A mismatch, including `None`, drops the
    /// rows and returns `Ok`. This path does not take `sync_in_progress`.
    pub fn index_transaction_data_in(
        &self,
        data_tx: &DataTransactionHeader,
        tx_path: &TxPath,
        chunk_range: LedgerChunkRange,
        block_height: u64,
        ledger: Option<DataLedger>,
        tx_index: Option<u32>,
    ) -> eyre::Result<()> {
        let captured = {
            let assignment = self.partition_assignment.read().unwrap();
            let Some(assignment) = *assignment else {
                return Err(eyre::eyre!("Partition not assigned!"));
            };
            assignment
        };
        // Use the captured slot. A later read could see a different slot and
        // back, and the equality check would then commit the wrong rows.
        let storage_range = self.ledger_range_for_assignment(&captured)?;
        let tx_path_hash = H256::from(hash_sha256(tx_path).unwrap());
        let (partition_overlap, start_offset) =
            self.partition_overlap_in(chunk_range, storage_range)?;

        // Rows are computed. The commit has not started. A test can reset
        // and change the assignment in this gap. The wait holds neither
        // `sync_in_progress` nor the assignment lock.
        #[cfg(test)]
        self.wait_if_registration_paused(RegistrationPausePoint::AfterRowsComputed);

        let assignment = self.partition_assignment.read().unwrap();
        if *assignment != Some(captured) {
            let current = *assignment;
            drop(assignment);
            self.note_registration_dropped(captured, current);
            return Ok(());
        }

        for (interval, submodule) in self.submodules.overlapping(partition_overlap) {
            submodule.db.update_registration(|tx| -> eyre::Result<()> {
                if let (Some(ledger), Some(tx_index)) = (ledger, tx_index) {
                    tx.note_placement_origin(ledger.get_id(), block_height, tx_index)?;
                }
                // Because each submodule index receives a copy of the path, we need to clone it
                tx.add_full_tx_path(tx_path_hash, tx_path.clone())?;
                // Record the (data_root, prefix_hash) this tx_path leaf folds from, so the
                // real data_root can be recovered on read (the leaf now stores the folded
                // hash_all_sha256([data_root, prefix_hash]), not the raw data_root) and the
                // proof leaf re-verified against it.
                tx.add_tx_leaf_binding(
                    tx_path_hash,
                    &TxLeafBinding {
                        data_root: data_tx.data_root,
                        prefix_hash: data_tx.prefix_hash,
                    },
                )?;
                if let Some(range) = interval.intersection(&partition_overlap) {
                    // One interval for this submodule's slice of the tx. A later
                    // placement that overlaps it is split. Data-path hashes stay.
                    tx.add_tx_path_hash_to_offset_range(
                        range.start(),
                        range.end(),
                        Some(tx_path_hash),
                    )?;
                    // Add the DataRootInfo to the Infos for this data_root
                    let info = DataRootInfo {
                        start_offset,
                        data_size: data_tx.data_size,
                    };
                    tx.add_data_root_info(data_tx.data_root, &info)?;
                    // Same txn as the index, so "indexed" can never be true while
                    // "bodies owed" is unrecorded. The body worker drains and
                    // deletes this row; `range.start()` is the key it uses,
                    // clipped to this submodule (the DataRootInfo above keeps the
                    // unclipped tx start).
                    tx.add_pending_body_migration(
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

    /// Count a registration whose rows were computed for an assignment this
    /// module no longer has. No index rows are written.
    fn note_registration_dropped(
        &self,
        captured: PartitionAssignment,
        current: Option<PartitionAssignment>,
    ) {
        #[cfg(test)]
        self.registration_drops.fetch_add(1, Ordering::SeqCst);
        static DESCRIBED: Once = Once::new();
        DESCRIBED.call_once(|| {
            metrics::describe_counter!(
                "irys.storage.registration_dropped_total",
                "Index registrations dropped because the storage module assignment changed before commit"
            );
        });
        metrics::counter!(
            "irys.storage.registration_dropped_total",
            "reason" => "assignment_changed"
        )
        .increment(1);
        debug!(
            storage_module.id = self.id,
            captured_partition = %captured.partition_hash,
            captured_ledger = ?captured.ledger_id,
            captured_slot = ?captured.slot_index,
            current_partition = ?current.map(|assignment| assignment.partition_hash),
            current_ledger = ?current.and_then(|assignment| assignment.ledger_id),
            current_slot = ?current.and_then(|assignment| assignment.slot_index),
            "dropped index registration; the partition assignment changed before commit"
        );
    }

    /// Partition-relative slice of `chunk_range` that lands in `storage_range`,
    /// plus the unclipped partition-relative start of the whole tx (negative
    /// when the tx began before this module's ledger range).
    ///
    /// Does not read `partition_assignment`. The caller passes the ledger
    /// range of the assignment the rows were computed for.
    ///
    /// The `PendingBodyMigrationsByOffset` key for each submodule is the start of
    /// this slice clipped to that submodule's interval; the body worker recovers
    /// the same range from the row's `start_offset` + `data_size`.
    ///
    /// # Errors
    /// Returns error if `chunk_range` doesn't overlap `storage_range`.
    fn partition_overlap_in(
        &self,
        chunk_range: LedgerChunkRange,
        storage_range: LedgerChunkRange,
    ) -> eyre::Result<(PartitionChunkRange, RelativeChunkOffset)> {
        let overlap = storage_range
            .intersection(&chunk_range)
            .ok_or_else(|| eyre::eyre!("chunk_range does not overlap storage module range"))?;
        let start = overlap.start() - storage_range.start();
        let end = overlap.end() - storage_range.start();
        let partition_overlap = PartitionChunkRange(ii(
            PartitionChunkOffset::from(start),
            PartitionChunkOffset::from(end),
        ));
        let relative = *chunk_range.start() as i64 - *storage_range.start() as i64;
        let start_offset = RelativeChunkOffset::from(i32::try_from(relative)?);
        Ok((partition_overlap, start_offset))
    }

    fn ledger_range_for_assignment(
        &self,
        assignment: &PartitionAssignment,
    ) -> eyre::Result<LedgerChunkRange> {
        let Some(slot_index) = assignment.slot_index else {
            return Err(eyre::eyre!("Ledger slot not assigned!"));
        };
        let start = slot_index as u64 * self.config.consensus.num_chunks_in_partition;
        let end = start + self.config.consensus.num_chunks_in_partition;
        Ok(LedgerChunkRange(ledger_chunk_offset_ie!(start, end)))
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
                .view(|tx| tx.pending_body_migrations_from(None, None))?;
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

    /// Partition intervals of this module's submodules, ascending.
    pub fn submodule_intervals(&self) -> Vec<Interval<PartitionChunkOffset>> {
        self.submodules
            .iter()
            .map(|(interval, _)| *interval)
            .collect()
    }

    /// At most `limit` jobs at or after `start` in the submodule that contains
    /// `submodule_at`. `start` of `None` is that submodule's first key.
    ///
    /// The body worker pages with this. [`Self::pending_body_migrations`]
    /// still returns every row.
    pub fn pending_body_migrations_page(
        &self,
        submodule_at: PartitionChunkOffset,
        start: Option<PartitionChunkOffset>,
        limit: usize,
    ) -> eyre::Result<Vec<(PartitionChunkOffset, PendingBodyMigration)>> {
        let (_interval, submodule) = self.get_submodule_for_offset(submodule_at)?;
        submodule
            .db
            .view(|tx| tx.pending_body_migrations_from(start, Some(limit)))
    }

    /// Inclusive partition offsets a live body-migration job still owns.
    ///
    /// The span starts at the **current row key**. A resume moves that key
    /// forward, so the durable prefix is not part of the span. The end matches
    /// the body worker: `min(tx end, submodule end)`. Touching spans are merged.
    ///
    /// Each submodule contributes at most [`PENDING_BODY_COVERAGE_ROWS`] rows,
    /// the same page the body worker reads. Jobs past that page are not held;
    /// data sync fetches them from peers. This does not call
    /// [`Self::pending_body_migration_batches`].
    pub fn pending_body_coverage_ranges(&self) -> eyre::Result<Vec<(u32, u32)>> {
        self.pending_body_coverage_ranges_limited(PENDING_BODY_COVERAGE_ROWS)
    }

    fn pending_body_coverage_ranges_limited(
        &self,
        row_cap: usize,
    ) -> eyre::Result<Vec<(u32, u32)>> {
        let chunk_size = self.config.consensus.chunk_size;
        let row_cap = row_cap.max(1);
        let mut ranges = Vec::new();
        for interval in self.submodule_intervals() {
            let interval_end = i64::from(*interval.end());
            let rows = self.pending_body_migrations_page(
                PartitionChunkOffset::from(*interval.start()),
                None,
                row_cap,
            )?;
            for (key, job) in rows {
                if let Some(range) = body_job_coverage(key, &job, interval_end, chunk_size) {
                    ranges.push(range);
                }
            }
        }
        Ok(merge_inclusive_ranges(ranges))
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
        let removed =
            submodule
                .db
                .update_covered(|tx| match tx.get_pending_body_migration(key)? {
                    Some(job) if job.data_root == data_root && job.block_height == block_height => {
                        tx.del_pending_body_migration(key)
                    }
                    _ => Ok(false),
                })?;
        // The delete is visible now. On RocksDB it stays unsynced until the
        // next synced write, the WAL backstop, or shutdown. Losing it leaves
        // the job row, and the body worker retries the same data_root and
        // block_height. MDBX commits this durably.
        Ok(removed)
    }

    /// Move the job at `from` to `to` once a prefix of its offsets is durable.
    ///
    /// The value is unchanged. The delete and the put commit in one batch, so
    /// a crash keeps the old key or the new key. A lost move repeats the
    /// durable prefix. Returns the key the job lives at after the call:
    /// `to` when the move landed, `from` when the row is gone, writes are
    /// paused, `to` is another submodule, or `to` already holds a different job.
    pub fn advance_pending_body_migration(
        &self,
        from: PartitionChunkOffset,
        to: PartitionChunkOffset,
        job: &PendingBodyMigration,
    ) -> eyre::Result<PartitionChunkOffset> {
        if self.data_writes_paused() || from == to {
            return Ok(from);
        }
        let from_submodule = {
            let (interval, _) = self.get_submodule_for_offset(from)?;
            *interval.start()
        };
        let to_submodule = {
            let (interval, _) = self.get_submodule_for_offset(to)?;
            *interval.start()
        };
        if from_submodule != to_submodule {
            return Ok(from);
        }
        let (_interval, submodule) = self.get_submodule_for_offset(from)?;
        submodule.db.update_covered(|tx| {
            match tx.get_pending_body_migration(from)? {
                Some(existing) if Self::same_pending_job(&existing, job) => {}
                _ => return Ok(from),
            }
            match tx.get_pending_body_migration(to)? {
                Some(existing) if Self::same_pending_job(&existing, job) => {
                    tx.del_pending_body_migration(from)?;
                    Ok(to)
                }
                Some(_) => Ok(from),
                None => {
                    tx.del_pending_body_migration(from)?;
                    tx.add_pending_body_migration(to, job)?;
                    Ok(to)
                }
            }
        })
    }

    /// Identity of a placement. Attempts change as the worker stalls, so they
    /// do not decide whether two rows are the same job.
    fn same_pending_job(left: &PendingBodyMigration, right: &PendingBodyMigration) -> bool {
        left.data_root == right.data_root
            && left.block_height == right.block_height
            && left.data_size == right.data_size
            && left.start_offset == right.start_offset
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
                .update(|tx| tx.del_pending_body_migrations_in_range(start, end))?;
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
            .update_covered(|tx| match tx.get_pending_body_migration(key)? {
                Some(mut job) if job.data_root == data_root && job.block_height == block_height => {
                    job.attempts = job.attempts.saturating_add(1);
                    tx.add_pending_body_migration(key, &job)?;
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
            index_read_metrics::trace_index_read(index_read_metrics::PLACEMENT, || {
                self.partition_offsets_for_data_root_chunk(chunk.data_root, chunk.tx_offset)
            })?
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
            index_read_metrics::trace_index_read(index_read_metrics::PLACEMENT, || {
                self.partition_offsets_for_data_root_chunk(chunk.data_root, chunk.tx_offset)
            })
        else {
            return false;
        };
        let pending = self.pending_writes.read().unwrap();
        offsets
            .iter()
            .any(|offset| pending.occupancy.contains_key(offset))
    }

    /// Waits until the packed bytes are in the pending map as `ChunkType::Data`.
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
            if let Some(submodule_index) = submodule
                .db
                .view(|tx| tx.get_data_root_infos_for_data_root(data_root))?
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
        meta: Option<&(DataRoot, u64, Base64, TxChunkOffset, Option<u32>)>,
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
        let Some((data_root, data_size, data_path, tx_offset, _crc)) = meta.cloned() else {
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
    ) -> Result<BTreeMap<PartitionChunkOffset, (DataRoot, u64, Base64, TxChunkOffset, Option<u32>)>>
    {
        let chunk_size = self.config.consensus.chunk_size;
        let slices =
            self.map_submodule_slices(start, end, |submodule, slice_start, slice_end| {
                submodule
                    .db
                    .view(|tx| metas_in_tx(tx, slice_start, slice_end, chunk_size))
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
                    .view(|tx| paths_in_tx(tx, slice_start, slice_end))
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
            #[cfg(any(test, feature = "test-utils"))]
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
    /// One index view loads every proof and the checksum stored beside it.
    /// The body read does not scan those rows again. An offset with no stored
    /// chunk is absent. A one-offset span still errors when the index names a
    /// chunk the disk does not hold. A checksum mismatch is omitted. Unknown
    /// stays in the result.
    pub fn generate_full_chunks(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> Result<BTreeMap<PartitionChunkOffset, PackedChunk>> {
        let metas = self.chunk_index_metas(start, end)?;
        if metas.is_empty() {
            return Ok(BTreeMap::new());
        }

        self.disk.yield_to_recall();
        self.disk.yield_to_writes();
        let (mut bodies, _) =
            self.read_chunks_inner(partition_chunk_offset_ii!(start, end), false, false)?;
        let packing_address = self.config.node_config.miner_address();
        let partition_hash = self
            .partition_hash()
            .ok_or_eyre("storage module has no partition")?;
        let caller = index_read_metrics::caller();
        let mut out = BTreeMap::new();
        for (offset, (data_root, data_size, data_path, tx_offset, crc)) in metas {
            let Some((mut bytes, kind)) = bodies.remove(&offset) else {
                if start == end {
                    return Err(eyre!("Could not find chunk bytes on disk"));
                }
                continue;
            };
            let (_, submodule) = self.get_submodule_for_offset(offset)?;
            if submodule.db.stores_offset_crc() && !self.crc_bytes_are_unsettled(offset) {
                let complete = submodule.crc_complete.load(Ordering::SeqCst);
                match chunk_crc::classify(crc, &bytes, complete) {
                    chunk_crc::CrcVerdict::Mismatch => {
                        match self.note_if_crc_mismatch(offset, kind, caller)? {
                            CrcConfirm::NotConfirmed(fresh) => bytes = fresh,
                            CrcConfirm::Confirmed | CrcConfirm::Unsettled => continue,
                        }
                    }
                    chunk_crc::CrcVerdict::Unknown => chunk_crc::note_unknown(caller),
                    chunk_crc::CrcVerdict::Match | chunk_crc::CrcVerdict::Skip => {}
                }
            }
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
            let hashes = tx.get_path_hashes_by_offset(partition_offset)?;
            let Some(hashes) = hashes else {
                return Ok(None);
            };
            if hashes.data_path_hash.is_none() {
                return Ok(None);
            }
            // The data root comes from the placement. A tx path with no
            // covering info still resolves through the leaf binding, which is
            // what this read did before the placement API.
            let data_root = if let Some(placement) = tx.placement_at(partition_offset)? {
                index_read_metrics::note("tx_path");
                index_read_metrics::note("tx_leaf");
                placement.data_root
            } else {
                let Some(tx_path_hash) = hashes.tx_path_hash else {
                    return Ok(None);
                };
                let Some(data_root) = data_root_for_tx_path_hash(tx, tx_path_hash)? else {
                    return Ok(None);
                };
                data_root
            };
            index_read_metrics::note("data_path");
            let Some(data_path) = tx.get_data_path_by_offset(partition_offset)? else {
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
            Ok(Self::resolve_placement(tx, partition_offset, chunk_size)?
                .map(|(data_root, tx_offset, _data_size)| (data_root, tx_offset)))
        })
    }

    /// Same resolution as [`Self::data_root_and_tx_offset_at`] for every offset in
    /// the inclusive range `[start, end]`. One submodule view covers each
    /// contiguous slice. `data_size` is the covering placement's funded size.
    ///
    /// `Ok(None)` at an index has the same meaning as `data_root_and_tx_offset_at`.
    pub fn data_root_and_tx_offset_span(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<Vec<Option<(DataRoot, TxChunkOffset, u64)>>> {
        if start > end {
            return Ok(Vec::new());
        }
        let chunk_size = self.config.consensus.chunk_size;
        let len = usize::try_from(u64::from(end.0 - start.0) + 1)
            .map_err(|_| eyre!("placement span does not fit in memory"))?;
        let mut out = vec![None; len];
        let mut offset = start;
        loop {
            let (interval, submodule) = self.get_submodule_for_offset(offset)?;
            let sub_last = PartitionChunkOffset(std::cmp::min(end.0, interval.end().0));
            let group_start = offset;
            submodule.db.view(|tx| {
                let placements = tx.placements_in_range(group_start, sub_last)?;
                for _placement in &placements {
                    index_read_metrics::note("tx_path");
                    index_read_metrics::note("tx_leaf");
                }
                let mut placement_index = 0;
                let mut cur = group_start;
                loop {
                    let idx = usize::try_from(cur.0 - start.0)?;
                    while placement_index < placements.len()
                        && placements[placement_index].end < cur
                    {
                        placement_index += 1;
                    }
                    out[idx] = placements
                        .get(placement_index)
                        .filter(|placement| placement.start <= cur && placement.end >= cur)
                        .and_then(|placement| {
                            Self::tx_offset_in_placement(placement, cur, chunk_size)
                        });
                    if cur == sub_last {
                        break;
                    }
                    cur = PartitionChunkOffset(cur.0 + 1);
                }
                Ok(())
            })?;
            if sub_last == end {
                break;
            }
            let Some(next) = sub_last.0.checked_add(1) else {
                break;
            };
            offset = PartitionChunkOffset(next);
        }
        Ok(out)
    }

    /// `tx_offset` is `partition_offset - placement.tx_start_offset`.
    ///
    /// The offset must fall inside `data_size`. The start can be negative when
    /// the tx begins before this module.
    fn tx_offset_in_placement(
        placement: &Placement,
        partition_offset: PartitionChunkOffset,
        chunk_size: u64,
    ) -> Option<(DataRoot, TxChunkOffset, u64)> {
        let partition_rel = RelativeChunkOffset::from(partition_offset);
        if placement.tx_start_offset > partition_rel {
            return None;
        }
        let tx_offset_i32 = *partition_rel - *placement.tx_start_offset;
        if tx_offset_i32 < 0 {
            return None;
        }
        let tx_offset_u32 = tx_offset_i32 as u32;
        let chunk_byte_offset = u64::from(tx_offset_u32).saturating_mul(chunk_size);
        if chunk_byte_offset >= placement.data_size {
            return None;
        }
        Some((
            placement.data_root,
            TxChunkOffset::from(tx_offset_u32),
            placement.data_size,
        ))
    }

    /// Placement of one partition offset: `(data_root, tx_offset, data_size)`.
    ///
    /// `data_path_hash` may be absent. That is the residual hole after the index
    /// is written and before the body is. `tx_offset` is
    /// `partition_offset - DataRootInfo.start_offset` for the placement that covers
    /// the offset, and the offset must fall inside `data_size`.
    fn resolve_placement(
        tx: &mut dyn SubmoduleRead,
        partition_offset: PartitionChunkOffset,
        chunk_size: u64,
    ) -> eyre::Result<Option<(DataRoot, TxChunkOffset, u64)>> {
        let Some(placement) = tx.placement_at(partition_offset)? else {
            return Ok(None);
        };
        index_read_metrics::note("tx_path");
        index_read_metrics::note("tx_leaf");
        Ok(Self::tx_offset_in_placement(
            &placement,
            partition_offset,
            chunk_size,
        ))
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
        S: FnOnce(&mut dyn SubmoduleRead) -> eyre::Result<R>,
    {
        let (_, submodule) = self.get_submodule_for_offset(chunk_offset)?;
        submodule.db.view(fetch_from_db)
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
                .view(|tx| tx.missing_path_hash_ranges(offset, sub_end))?;
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
        let mut completed = 0_usize;
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
    /// bytes. Runs stay in offset order. `hold_young` leaves a short run
    /// queued until its oldest chunk has waited `reorder_grace`. Full runs
    /// stay. The caller passes false when the disk is idle or every short
    /// run is due, so those short runs go out.
    fn select_coalesced_window(
        &self,
        pending: &PendingWrites,
        max_runs: usize,
        scan_chunks: usize,
        cap: u64,
        hold_young: bool,
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
        if hold_young {
            runs.retain(|run| run.byte_len >= cap || run.aged);
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

    /// Runs in the order `batch` already has. The coalesce window is offset
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
    /// `plan_ranked_runs` so a later priority does not jump an earlier offset.
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
        let (file_arc, file_offset, submodule_offset) = self.locate_io(run.start, true)?;
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
                drop(file);
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
    #[cfg(test)]
    fn write_chunk_internal(
        &self,
        chunk_offset: PartitionChunkOffset,
        bytes: Vec<u8>,
        chunk_type: ChunkType,
    ) -> eyre::Result<()> {
        let start_time = Instant::now();

        // Get the submodule relative offset of the chunk
        let (file_arc, file_offset, submodule_offset) = self.locate_io(chunk_offset, true)?;

        // Attempt to write the chunk data, handling errors by resetting to Uninitialized
        let write_result = (|| -> eyre::Result<()> {
            // Positional write. The shared handle keeps its cursor.
            let file = self.disk.lock_chunks(&file_arc);
            file.write_all_at(bytes.as_slice(), file_offset)
                .map_err(|e| {
                    error!("Failed to write chunk @ chunk_offset {chunk_offset} submodule_offset {submodule_offset}: {}", e);
                    eyre::eyre!("Failed to write chunk @ chunk_offset {chunk_offset} submodule_offset {submodule_offset}: {}", e)
                })?;
            drop(file);
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

    /// Ledger offsets whose stored data-proof leaf is `content_hash`.
    ///
    /// Each submodule reports partition offsets. This adds the module's
    /// ledger start. Schema v2 and MDBX return the not-supported error.
    /// The caller must already have a partition assignment.
    pub fn ledger_offsets_for_content_hash(
        &self,
        content_hash: H256,
    ) -> eyre::Result<Vec<LedgerChunkOffset>> {
        let base = *self.get_storage_module_ledger_offsets()?.start();
        let mut found = Vec::new();
        for (_, submodule) in self.submodules.iter() {
            let offsets = submodule
                .db
                .view(|tx| tx.offsets_for_content_hash(content_hash))?;
            for offset in offsets {
                let ledger = base.checked_add(u64::from(offset.0)).ok_or_else(|| {
                    eyre::eyre!("ledger offset overflow for partition offset {}", offset.0)
                })?;
                found.push(LedgerChunkOffset::from(ledger));
            }
        }
        found.sort_unstable();
        found.dedup();
        Ok(found)
    }

    /// Utility method asking the StorageModule to return its chunk range in
    /// ledger relative coordinates
    pub fn get_storage_module_ledger_offsets(&self) -> eyre::Result<LedgerChunkRange> {
        let pa = self.partition_assignment.read().unwrap();
        let Some(part_assign) = *pa else {
            return Err(eyre::eyre!("Partition not assigned!"));
        };
        self.ledger_range_for_assignment(&part_assign)
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

    /// Counts the lane keeps for a load-test sample. Production code does not call this.
    #[cfg(feature = "bench")]
    pub fn bench_queue_snapshot(&self) -> BenchQueueSnapshot {
        let pending = self.pending_writes.read().unwrap();
        let pending_chunks = pending.len().saturating_add(pending.written_ahead.len());
        let occupancy = pending.occupancy.len();
        let packed = pending
            .values()
            .map(|(bytes, _)| bytes.len() as u64)
            .sum::<u64>();
        let ahead = pending
            .written_ahead
            .values()
            .map(|ahead| ahead.bytes.len() as u64)
            .sum::<u64>();
        let pending_bytes = packed
            .saturating_add(ahead)
            .saturating_add(pending.queued_unpacked_bytes);
        drop(pending);
        BenchQueueSnapshot {
            pending_chunks,
            occupancy,
            pending_bytes,
            sweep_slots: self.disk.bench_sweep_slots(),
        }
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
        // A crash test closes the files here. The durable point stays unfinished,
        // so intervals.json keeps the Interrupted mark and reload does not check crc.
        #[cfg(test)]
        if self.abandon_on_drop.load(Ordering::SeqCst) {
            return;
        }
        // Index submits for deposited bodies must finish before the drains stop.
        self.drain_entropy_queue();
        for (_, submodule) in self.submodules.iter() {
            submodule.index_drain.shutdown();
        }
        for (_, submodule) in self.submodules.iter() {
            if let Err(err) = submodule.db.sync_group() {
                error!(
                    "Unable to sync submodule index while dropping SM {} - {:?}",
                    &self.id, &err
                );
            }
        }
        info!("Syncing SM {} to disk...", &self.id);
        // Authoritative interval rewrite only after the chunk fsync. A failed
        // flush can leave live `Data` for bytes that are not durable yet.
        match self.force_sync_pending_chunks() {
            Ok(()) => {
                if let Err(e) = self.persist_intervals_authoritative(false) {
                    error!(
                        "Unable to persist intervals while dropping SM {} - {:?}",
                        &self.id, &e
                    );
                }
            }
            Err(e) => {
                error!(
                    "Unable to sync writes while dropping SM {} - {:?}",
                    &self.id, &e
                );
            }
        }
    }
}

/// Pending-queue depth for one bench sample.
#[cfg(feature = "bench")]
#[derive(Debug, Clone, Copy)]
pub struct BenchQueueSnapshot {
    pub pending_chunks: usize,
    pub occupancy: usize,
    pub pending_bytes: u64,
    pub sweep_slots: usize,
}

/// Mark `[0, chunks)` as `Entropy` in `intervals.json`.
///
/// A file that already holds `Data` or `Entropy` is left in place so a rerun
/// keeps durable chunks. An empty file, a missing file, or a file that is only
/// `Uninitialized` / `Interrupted` is replaced. The real packer does not run.
#[cfg(feature = "bench")]
pub fn write_entropy_intervals(path: &Path, chunks: u32) -> eyre::Result<()> {
    if chunks == 0 {
        return Err(eyre!("partition chunk count is 0"));
    }
    if path.exists() {
        match read_intervals_file(path) {
            Ok(existing) => {
                let keep = existing.iter().any(|(_, chunk_type)| {
                    matches!(chunk_type, ChunkType::Data | ChunkType::Entropy)
                });
                if keep {
                    return Ok(());
                }
            }
            Err(error) => {
                let len = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
                if len > 0 {
                    return Err(error)
                        .wrap_err("intervals.json is not empty and could not be read");
                }
            }
        }
    } else if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .wrap_err_with(|| format!("Failed to create intervals file at {}", path.display()))?;

    let mut intervals = StorageIntervals::new();
    let end = chunks - 1;
    intervals
        .insert_merge_touching_if_values_equal(
            partition_chunk_offset_ii!(0, end),
            ChunkType::Entropy,
        )
        .map_err(|error| eyre!("failed to build entropy intervals: {error:?}"))?;
    let mut file = get_atomic_file(path)?;
    file.write_all(serde_json::to_string(&intervals)?.as_bytes())?;
    file.commit()?;
    Ok(())
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

/// Operator steps when `intervals.json` cannot be read. The open error names
/// the file. The same steps are in `docs/60-indexes/06-storage-module-indexes.md`.
const INTERVALS_FILE_REMEDY: &str = "Stop the node, move this intervals.json aside, and reset that storage module so it can repack. See docs/60-indexes/06-storage-module-indexes.md.";

fn intervals_file_error(path: &Path, action: &str) -> String {
    format!(
        "Failed to {action} intervals.json at {}. {INTERVALS_FILE_REMEDY}",
        path.display()
    )
}

/// Reads and deserializes intervals from storage state file
///
/// Loads the stored interval mapping that tracks chunk states.
/// Expects a JSON-formatted file containing StorageIntervals.
/// A read or parse failure names `path` and how to reset that module.
pub fn read_intervals_file(path: &Path) -> eyre::Result<StorageIntervals> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .wrap_err_with(|| intervals_file_error(path, "open"))?;

    let size = file
        .metadata()
        .wrap_err_with(|| intervals_file_error(path, "stat"))?
        .len() as usize;

    if size == 0 {
        return Err(eyre!(
            "Intervals file is empty at {}. {INTERVALS_FILE_REMEDY}",
            path.display()
        ));
    }

    let mut contents = String::with_capacity(size);
    file.seek(SeekFrom::Start(0))
        .wrap_err_with(|| intervals_file_error(path, "seek"))?;
    file.read_to_string(&mut contents)
        .wrap_err_with(|| intervals_file_error(path, "read"))?;
    let intervals = serde_json::from_str(&contents).wrap_err_with(|| {
        format!(
            "intervals.json at {} is corrupt. {INTERVALS_FILE_REMEDY}",
            path.display()
        )
    })?;
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

/// Pending-body rows one coverage read decodes per submodule.
///
/// Matches the body worker's page. A data-sync tick must not load the rest
/// of the table.
const PENDING_BODY_COVERAGE_ROWS: usize = 64;

/// Inclusive `[key, min(tx end, submodule end)]` for one body-migration job.
/// Same span the body worker drains. `None` when the job covers no offset.
///
/// `key` is the current row key. After a resume it is past the durable prefix.
fn body_job_coverage(
    key: PartitionChunkOffset,
    job: &PendingBodyMigration,
    interval_end: i64,
    chunk_size: u64,
) -> Option<(u32, u32)> {
    if chunk_size == 0 {
        return None;
    }
    let num_chunks = i64::try_from(job.data_size.div_ceil(chunk_size)).ok()?;
    if num_chunks == 0 {
        return None;
    }
    let tx_start = i64::from(*job.start_offset);
    let first = i64::from(*key);
    let last = (tx_start + num_chunks - 1).min(interval_end);
    if last < first {
        return None;
    }
    Some((u32::try_from(first).ok()?, u32::try_from(last).ok()?))
}

fn merge_inclusive_ranges(mut ranges: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    ranges.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut()
            && start <= last.1.saturating_add(1)
        {
            last.1 = last.1.max(end);
            continue;
        }
        merged.push((start, end));
    }
    merged
}

//==============================================================================
// Tests
//------------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use irys_testing_utils::{chunk_bytes_gen, utils::TempDirBuilder};
    use irys_types::{
        ConsensusConfig, DataTransactionHeaderV1, DataTransactionLedger, DbSyncMode, H256,
        NodeConfig, SimpleRNG, StorageSyncConfig, TxChunkOffset, irys::IrysSigner,
        ledger_chunk_offset_ii, partition_chunk_offset_ii,
    };
    use nodit::interval::ii;

    fn index_database(engine: SubmoduleIndexEngine) -> DatabaseConfig {
        DatabaseConfig {
            sync_mode: DbSyncMode::UtterlyNoSync,
            cache_sync_mode: DbSyncMode::UtterlyNoSync,
            geometry_max_size: Some(irys_types::TEST_DB_GEOMETRY_MAX_SIZE),
            submodule_index: engine,
            ..DatabaseConfig::default()
        }
    }

    /// Larger than one filesystem block, so a sparse hole is visible in `st_blocks`.
    const RESERVE_CHUNK: u64 = 64 * 1024;
    const RESERVE_CHUNKS: u64 = 4;

    fn reserve_bytes() -> u64 {
        RESERVE_CHUNK * RESERVE_CHUNKS
    }

    fn reserve_fixture(
        prefix: &str,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        Config,
        StorageModuleInfo,
    )> {
        let tmp = TempDirBuilder::new().prefix(prefix).build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: RESERVE_CHUNK,
                num_chunks_in_partition: RESERVE_CHUNKS,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let last = u32::try_from(RESERVE_CHUNKS - 1).expect("chunk count");
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: None,
            submodules: vec![(partition_chunk_offset_ii!(0, last), "hdd0".into())],
        };
        Ok((tmp, config, info))
    }

    fn chunks_path(tmp: &irys_testing_utils::utils::tempfile::TempDir) -> PathBuf {
        tmp.path().join("hdd0").join("chunks.dat")
    }

    fn read_prefix(path: &Path, len: usize) -> eyre::Result<Vec<u8>> {
        let mut file = File::open(path)?;
        let mut buf = vec![0_u8; len];
        file.read_exact(&mut buf)?;
        Ok(buf)
    }

    #[cfg(target_os = "linux")]
    fn assert_reserved(path: &Path, full_len: u64) -> eyre::Result<()> {
        use std::os::unix::fs::MetadataExt as _;
        let meta = fs::metadata(path)?;
        assert_eq!(meta.len(), full_len, "{}", path.display());
        let allocated = meta.blocks().saturating_mul(512);
        assert!(
            allocated >= full_len,
            "{} allocated {allocated} bytes, full length is {full_len}",
            path.display()
        );
        Ok(())
    }

    #[test]
    fn chunks_reserve_runs_for_a_short_or_sparse_file() {
        assert!(chunks_file_needs_reserve(0, 0, 100));
        // A dense prefix that is still shorter than the submodule.
        assert!(chunks_file_needs_reserve(50, 50, 100));
        // Full logical size, with a hole.
        assert!(chunks_file_needs_reserve(100, 40, 100));
        // Longer than the submodule, with a hole somewhere.
        assert!(chunks_file_needs_reserve(150, 40, 100));
        assert!(!chunks_file_needs_reserve(100, 100, 100));
        // Block rounding can report more allocated bytes than `st_size`.
        assert!(!chunks_file_needs_reserve(100, 128, 100));
        // Longer and dense: do not shrink.
        assert!(!chunks_file_needs_reserve(180, 180, 100));
    }

    #[test]
    fn submodule_reserve_len_counts_inclusive_offsets() -> eyre::Result<()> {
        let interval = partition_chunk_offset_ii!(0, 4);
        let len = submodule_reserve_len(
            &interval,
            32,
            #[cfg(feature = "bench")]
            None,
        )?;
        assert_eq!(len, 5 * 32);
        Ok(())
    }

    #[test]
    fn no_space_names_the_path_the_wanted_bytes_and_the_free_bytes() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new().prefix("chunks_no_space").build();
        let path = tmp.path().join("chunks.dat");
        let file = File::create(&path)?;
        let text = no_space(&file, &path, 1234).to_string();
        assert!(text.contains(&path.display().to_string()), "{text}");
        assert!(text.contains("wanted 1234 bytes"), "{text}");
        assert!(text.contains("bytes free"), "{text}");
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn new_chunks_file_is_reserved() -> eyre::Result<()> {
        let (tmp, config, info) = reserve_fixture("chunks_reserve_new")?;
        let storage = StorageModule::new(&info, &config)?;
        let path = chunks_path(&tmp);
        assert_reserved(&path, reserve_bytes())?;
        let crc_path = tmp.path().join("hdd0").join("chunks.crc");
        assert!(
            !crc_path.exists(),
            "open does not create a chunks.crc sidecar"
        );
        let head = read_prefix(&path, 1)?;
        assert_eq!(head, [0]);
        drop(storage);
        Ok(())
    }

    #[test]
    fn partial_chunks_file_keeps_its_bytes() -> eyre::Result<()> {
        let (tmp, config, info) = reserve_fixture("chunks_reserve_partial")?;
        let path = chunks_path(&tmp);
        fs::create_dir_all(path.parent().expect("parent"))?;
        let pattern = vec![0xA5_u8; RESERVE_CHUNK as usize];
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)?;
            file.write_all(&pattern)?;
        }
        assert!(fs::metadata(&path)?.len() < reserve_bytes());

        let storage = StorageModule::new(&info, &config)?;
        assert_eq!(read_prefix(&path, pattern.len())?, pattern);
        assert_eq!(fs::metadata(&path)?.len(), reserve_bytes());
        #[cfg(target_os = "linux")]
        assert_reserved(&path, reserve_bytes())?;
        drop(storage);

        // The second open finds a file that already covers the submodule.
        let reopened = StorageModule::new(&info, &config)?;
        assert_eq!(read_prefix(&path, pattern.len())?, pattern);
        assert_eq!(fs::metadata(&path)?.len(), reserve_bytes());
        drop(reopened);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sparse_full_chunks_file_keeps_its_bytes_and_is_reserved() -> eyre::Result<()> {
        use std::os::unix::fs::MetadataExt as _;
        let (tmp, config, info) = reserve_fixture("chunks_reserve_sparse")?;
        let path = chunks_path(&tmp);
        fs::create_dir_all(path.parent().expect("parent"))?;
        let pattern = vec![0x5A_u8; RESERVE_CHUNK as usize];
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)?;
            file.write_all(&pattern)?;
            file.set_len(reserve_bytes())?;
        }
        let before = fs::metadata(&path)?;
        assert_eq!(before.len(), reserve_bytes());
        let allocated = before.blocks().saturating_mul(512);
        assert!(
            allocated < before.len(),
            "setup file is not sparse: allocated {allocated}, len {}",
            before.len()
        );

        let storage = StorageModule::new(&info, &config)?;
        assert_eq!(read_prefix(&path, pattern.len())?, pattern);
        assert_reserved(&path, reserve_bytes())?;
        drop(storage);
        Ok(())
    }

    #[cfg(feature = "bench")]
    #[test]
    fn split_write_region_is_reserved_and_recall_is_unchanged() -> eyre::Result<()> {
        use std::os::unix::fs::MetadataExt as _;
        let tmp = TempDirBuilder::new().prefix("split_reserve").build();
        let sub = tmp.path().join("sub");
        fs::create_dir_all(&sub)?;
        let recall_path = tmp.path().join("recall");
        let write_path = tmp.path().join("write");
        let recall_pattern = vec![0x22_u8; 4096];
        let write_pattern = vec![0x11_u8; RESERVE_CHUNK as usize];
        {
            let mut recall = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&recall_path)?;
            recall.write_all(&recall_pattern)?;
            recall.set_len(reserve_bytes())?;
            let mut write = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&write_path)?;
            write.write_all(&write_pattern)?;
        }
        let recall_before = fs::metadata(&recall_path)?;
        let recall = OpenOptions::new().read(true).open(&recall_path)?;
        let write = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&write_path)?;
        let split = Arc::new(SplitBacking::open(recall, write, 0, RESERVE_CHUNKS)?);
        let config = Config::new_with_random_peer_id(NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: RESERVE_CHUNK,
                num_chunks_in_partition: RESERVE_CHUNKS,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp.path().to_path_buf(),
            ..NodeConfig::testing()
        });
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: None,
            submodules: vec![(
                partition_chunk_offset_ii!(
                    0,
                    u32::try_from(RESERVE_CHUNKS - 1).expect("chunk count")
                ),
                "sub".into(),
            )],
        };
        let storage = StorageModule::open_split(&info, &config, split, BenchIndex::Mdbx)?;
        drop(storage);

        assert!(!sub.join("chunks.dat").exists());
        let recall_after = fs::metadata(&recall_path)?;
        assert_eq!(recall_after.len(), recall_before.len());
        assert_eq!(recall_after.blocks(), recall_before.blocks());
        assert_eq!(
            read_prefix(&recall_path, recall_pattern.len())?,
            recall_pattern
        );
        assert_eq!(
            read_prefix(&write_path, write_pattern.len())?,
            write_pattern
        );
        assert_eq!(fs::metadata(&write_path)?.len(), reserve_bytes());
        #[cfg(target_os = "linux")]
        assert_reserved(&write_path, reserve_bytes())?;
        Ok(())
    }

    #[test]
    fn submodule_index_engine_mdbx_fresh_does_not_create_rocks() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("submodule_index_engine_mdbx")
            .build();
        let module_dir = tmp_dir.path().join("module");
        fs::create_dir_all(&module_dir)?;

        let index = open_submodule_index(&module_dir, &index_database(SubmoduleIndexEngine::Mdbx))?;
        drop(index);

        assert!(module_dir.join("db").join("mdbx.dat").is_file());
        assert!(!module_dir.join("rocks").exists());
        Ok(())
    }

    #[test]
    fn submodule_index_engine_rocksdb_fresh_reads_back_a_row() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("submodule_index_engine_rocks")
            .build();
        let module_dir = tmp_dir.path().join("module");
        fs::create_dir_all(&module_dir)?;

        let index =
            open_submodule_index(&module_dir, &index_database(SubmoduleIndexEngine::Rocksdb))?;
        let schema = fs::read_to_string(module_dir.join("rocks").join("SCHEMA"))?;
        assert_eq!(schema, SUBMODULE_SCHEMA_V3);
        assert!(!module_dir.join("db").join("mdbx.dat").exists());

        // Schema v3 drops a data path that no interval covers. The interval
        // hash is what a later read reports as `tx_path_hash`.
        let offset = PartitionChunkOffset::from(7);
        let root = H256::repeat_byte(1);
        index.update(|tx| {
            tx.add_tx_leaf_binding(
                root,
                &TxLeafBinding {
                    data_root: root,
                    prefix_hash: H256::zero(),
                },
            )?;
            tx.add_tx_path_hash_to_offset_range(offset, offset, Some(root))?;
            tx.add_data_root_info(
                root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(0),
                    data_size: 1,
                },
            )?;
            tx.add_full_data_path(offset, vec![3])?;
            Ok(())
        })?;
        let got = index
            .view(|tx| tx.get_path_hashes_by_offset(offset))?
            .expect("data-path bytes are the presence row");
        assert_eq!(
            got.data_path_hash,
            Some(H256(irys_types::hash_sha256(&[3])))
        );
        assert_eq!(got.tx_path_hash, Some(root));
        assert_eq!(
            index.view(|tx| tx.get_data_path_by_offset(offset))?,
            Some(vec![3])
        );
        Ok(())
    }

    #[test]
    fn submodule_index_engine_rocksdb_refuses_uncopied_mdbx() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("submodule_index_engine_refuse")
            .build();
        let module_dir = tmp_dir.path().join("module");
        let mdbx_dat = module_dir.join("db").join("mdbx.dat");
        let rocks_dir = module_dir.join("rocks");
        fs::create_dir_all(mdbx_dat.parent().expect("db parent"))?;
        fs::write(&mdbx_dat, b"live")?;

        let error =
            open_submodule_index(&module_dir, &index_database(SubmoduleIndexEngine::Rocksdb))
                .expect_err("live MDBX data must not open an empty Rocks index");
        let message = error.to_string();
        assert!(
            message.contains(&rocks_dir.display().to_string()),
            "{message}"
        );
        assert!(
            message.contains(&mdbx_dat.display().to_string()),
            "{message}"
        );
        assert!(
            message.contains("the MDBX index was not copied"),
            "{message}"
        );
        assert!(!rocks_dir.join("SCHEMA").exists());
        assert!(!rocks_dir.join("CURRENT").exists());
        Ok(())
    }

    #[test]
    fn submodule_index_engine_rocksdb_refuses_incomplete_conversion() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("submodule_index_engine_incomplete")
            .build();
        let module_dir = tmp_dir.path().join("module");
        let mdbx_dat = module_dir.join("db").join("mdbx.dat");
        let rocks_dir = module_dir.join("rocks");
        fs::create_dir_all(mdbx_dat.parent().expect("db parent"))?;
        fs::write(&mdbx_dat, b"live")?;
        fs::create_dir_all(&rocks_dir)?;
        fs::write(rocks_dir.join("CURRENT"), b"MANIFEST-000001\n")?;

        let error =
            open_submodule_index(&module_dir, &index_database(SubmoduleIndexEngine::Rocksdb))
                .expect_err("incomplete rocks conversion must not open");
        let message = format!("{error:#}");
        assert!(message.contains("submodule-index-convert"), "{message}");
        assert!(message.contains("SCHEMA"), "{message}");
        assert!(!rocks_dir.join("SCHEMA").exists());
        Ok(())
    }

    #[test]
    fn submodule_index_engine_mdbx_refuses_uncopied_rocks() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("submodule_index_engine_refuse_rocks")
            .build();
        let module_dir = tmp_dir.path().join("module");
        let mdbx_dat = module_dir.join("db").join("mdbx.dat");
        let rocks_dir = module_dir.join("rocks");
        fs::create_dir_all(&rocks_dir)?;
        fs::write(rocks_dir.join("CURRENT"), b"live")?;

        let error = open_submodule_index(&module_dir, &index_database(SubmoduleIndexEngine::Mdbx))
            .expect_err("live Rocks data must not open an empty MDBX index");
        let message = error.to_string();
        assert!(
            message.contains(&module_dir.join("db").display().to_string()),
            "{message}"
        );
        assert!(
            message.contains(&rocks_dir.display().to_string()),
            "{message}"
        );
        assert!(
            message.contains("the Rocks index was not copied"),
            "{message}"
        );
        assert!(!mdbx_dat.exists());
        assert!(rocks_dir.join("CURRENT").is_file());
        Ok(())
    }

    /// `db/` with no schema row. Node open of this directory stamps the schema,
    /// so a later byte compare shows whether open ran.
    fn write_unstamped_mdbx(module_dir: &Path) -> eyre::Result<PathBuf> {
        use irys_database::open_or_create_db;
        use irys_database::submodule::tables::SubmoduleTables;

        let db_dir = module_dir.join("db");
        let args = irys_database::submodule_db_args(&index_database(SubmoduleIndexEngine::Mdbx))?;
        let env = open_or_create_db(&db_dir, SubmoduleTables::ALL, args)?;
        drop(env);
        Ok(db_dir)
    }

    fn mdbx_schema_row(db_dir: &Path) -> eyre::Result<Option<Vec<u8>>> {
        use irys_database::metadata::MetadataKey;
        use irys_database::open_or_create_db;
        use irys_database::submodule::tables::{Metadata, SubmoduleTables};
        use reth_db::Database as _;
        use reth_db::transaction::DbTx as _;

        let args = irys_database::submodule_db_args(&index_database(SubmoduleIndexEngine::Mdbx))?;
        let env = open_or_create_db(db_dir, SubmoduleTables::ALL, args)?;
        let tx = env.tx()?;
        Ok(tx.get::<Metadata>(MetadataKey::DBSchemaVersion)?)
    }

    fn directory_fingerprint(dir: &Path) -> eyre::Result<BTreeMap<String, (u64, [u8; 32])>> {
        use std::io::Read as _;

        let mut out = BTreeMap::new();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let meta = entry.metadata()?;
            let mut file = File::open(&path)?;
            let mut hasher = sha::Sha256::new();
            let mut buf = [0_u8; 64 * 1024];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            out.insert(
                entry.file_name().to_string_lossy().into_owned(),
                (meta.len(), hasher.finish()),
            );
        }
        Ok(out)
    }

    #[test]
    fn submodule_index_engine_mdbx_refuses_when_both_indexes_exist() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("submodule_index_engine_both")
            .build();
        let module_dir = tmp_dir.path().join("module");
        fs::create_dir_all(&module_dir)?;
        let db_dir = write_unstamped_mdbx(&module_dir)?;
        let before = directory_fingerprint(&db_dir)?;
        assert!(before.contains_key("mdbx.dat"), "{before:?}");

        let rocks_dir = module_dir.join("rocks");
        let rocks_current = rocks_dir.join("CURRENT");
        fs::create_dir_all(&rocks_dir)?;
        fs::write(&rocks_current, b"live")?;

        let error = open_submodule_index(&module_dir, &index_database(SubmoduleIndexEngine::Mdbx))
            .expect_err("MDBX must not open when rocks/ is also present");
        let message = error.to_string();
        assert!(
            message.contains(&module_dir.display().to_string()),
            "{message}"
        );
        assert!(
            message.contains(&db_dir.join("mdbx.dat").display().to_string()),
            "{message}"
        );
        assert!(
            message.contains(&rocks_dir.display().to_string()),
            "{message}"
        );
        assert!(message.contains("both indexes exist"), "{message}");
        assert!(
            message.contains("database.submodule_index is unset"),
            "{message}"
        );
        assert!(
            message.contains("database.submodule_index = \"rocksdb\""),
            "{message}"
        );
        assert!(message.contains("rocks-to-mdbx"), "{message}");
        assert!(message.contains("rocks.retired"), "{message}");

        assert_eq!(directory_fingerprint(&db_dir)?, before);
        assert_eq!(mdbx_schema_row(&db_dir)?, None);
        assert_eq!(fs::read(&rocks_current)?, b"live");
        Ok(())
    }

    #[test]
    fn submodule_index_engine_rocksdb_reads_rocks_row_when_both_indexes_exist() -> eyre::Result<()>
    {
        let tmp_dir = TempDirBuilder::new()
            .prefix("submodule_index_engine_rocks_keeps_db")
            .build();
        let module_dir = tmp_dir.path().join("module");
        fs::create_dir_all(&module_dir)?;

        let index =
            open_submodule_index(&module_dir, &index_database(SubmoduleIndexEngine::Rocksdb))?;
        let offset = PartitionChunkOffset::from(7);
        let root = H256::repeat_byte(1);
        index.update(|tx| {
            tx.add_tx_leaf_binding(
                root,
                &TxLeafBinding {
                    data_root: root,
                    prefix_hash: H256::zero(),
                },
            )?;
            tx.add_tx_path_hash_to_offset_range(offset, offset, Some(root))?;
            tx.add_data_root_info(
                root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(0),
                    data_size: 1,
                },
            )?;
            tx.add_full_data_path(offset, vec![9, 9, 9])?;
            Ok(())
        })?;
        drop(index);

        // db/ is added after rocks/ exists. The node must not build rocks/ on
        // top of a live MDBX index, and it must not read this db/.
        let db_dir = write_unstamped_mdbx(&module_dir)?;
        let before = directory_fingerprint(&db_dir)?;

        let again =
            open_submodule_index(&module_dir, &index_database(SubmoduleIndexEngine::Rocksdb))?;
        let got = again
            .view(|tx| tx.get_data_path_by_offset(offset))?
            .expect("the row exists only in rocks/");
        assert_eq!(got, vec![9, 9, 9]);
        drop(again);

        assert_eq!(directory_fingerprint(&db_dir)?, before);
        assert_eq!(mdbx_schema_row(&db_dir)?, None);
        assert_eq!(
            fs::read_to_string(module_dir.join("rocks").join("SCHEMA"))?,
            SUBMODULE_SCHEMA_V3
        );
        Ok(())
    }

    #[test]
    fn rocks_module_enables_group_commit_and_mdbx_does_not() -> eyre::Result<()> {
        fn open(
            prefix: &str,
            engine: SubmoduleIndexEngine,
        ) -> eyre::Result<(irys_testing_utils::utils::tempfile::TempDir, StorageModule)> {
            let tmp = TempDirBuilder::new().prefix(prefix).build();
            let node_config = NodeConfig {
                consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                    chunk_size: 32,
                    num_chunks_in_partition: 5,
                    ..ConsensusConfig::testing()
                }),
                base_directory: tmp.path().to_path_buf(),
                database: index_database(engine),
                ..NodeConfig::testing()
            };
            let config = Config::new_with_random_peer_id(node_config);
            let storage = StorageModule::new(
                &StorageModuleInfo {
                    id: 0,
                    partition_assignment: None,
                    submodules: vec![(partition_chunk_offset_ii!(0, 4), "hdd0".into())],
                },
                &config,
            )?;
            Ok((tmp, storage))
        }

        let (_rocks_tmp, rocks) = open("rocks_group_commit", SubmoduleIndexEngine::Rocksdb)?;
        let (_mdbx_tmp, mdbx) = open("mdbx_group_commit", SubmoduleIndexEngine::Mdbx)?;
        let (_, rocks_sub) = rocks.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        assert!(rocks_sub.db.group_commit_enabled());
        let (_, mdbx_sub) = mdbx.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        assert!(!mdbx_sub.db.group_commit_enabled());
        Ok(())
    }

    /// A visible data-root row is not a durable body. The prune gate stays
    /// closed until the chunk interval is `Data`, which happens after the
    /// chunk `fdatasync` and the presence commit. A synced index write that
    /// is not that commit does not open the gate, and it does cover the
    /// earlier registration.
    #[test]
    fn unsynced_data_root_does_not_satisfy_the_prune_gate() -> eyre::Result<()> {
        let (_tmp, _config, storage) =
            open_presence_module("prune_gate", SubmoduleIndexEngine::Rocksdb)?;
        let (_, submodule) = storage.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        submodule.db.set_wal_backstop(Duration::from_secs(60));
        let data_root = H256::repeat_byte(4);
        let syncs_before = submodule.db.wal_sync_count();
        // Schema v3 stores the data-root row on the interval. A row with no
        // interval is refused. The registration stays unsynced.
        submodule.db.update_registration(|tx| {
            tx.add_tx_leaf_binding(
                data_root,
                &TxLeafBinding {
                    data_root,
                    prefix_hash: H256::zero(),
                },
            )?;
            tx.add_tx_path_hash_to_offset_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(0),
                Some(data_root),
            )?;
            tx.add_data_root_info(
                data_root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(0),
                    data_size: 32,
                },
            )
        })?;
        let published = submodule.db.published_generation();
        assert!(published > submodule.db.durable_generation());
        assert_eq!(submodule.db.wal_sync_count(), syncs_before);
        assert_eq!(submodule.db.wal_backstop_sync_count(), 0);
        assert!(
            !storage.collect_data_root_infos(data_root)?.0.is_empty(),
            "the registration is visible before it is durable"
        );
        let offset = PartitionChunkOffset::from(0);
        let candidates = BTreeSet::from([TxChunkOffset::from(0)]);
        assert!(!storage.is_data_chunk_durable_at(offset));
        assert!(
            storage
                .durable_tx_offsets_for_data_root(data_root, &candidates)?
                .is_empty()
        );

        // A synced write that is not the chunk commit. Schema v3 has no
        // path-hash family, and clearing the interval here would drop the
        // registration this write is meant to cover.
        submodule.db.update(|tx| {
            tx.add_data_path_hash_to_offset_index(offset, Some(H256::repeat_byte(1)))
        })?;
        assert!(submodule.db.durable_generation() >= published);
        assert_eq!(submodule.db.wal_backstop_sync_count(), 0);
        assert!(submodule.db.wal_sync_count() > syncs_before);
        assert!(!storage.is_data_chunk_durable_at(offset));
        assert!(
            storage
                .durable_tx_offsets_for_data_root(data_root, &candidates)?
                .is_empty()
        );

        assert!(storage.write_chunk(offset, vec![9_u8; 32], ChunkType::Data));
        storage.force_sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(offset));
        assert_eq!(
            storage.durable_tx_offsets_for_data_root(data_root, &candidates)?,
            candidates
        );
        Ok(())
    }

    /// The prune gate waits for `intervals.json`, not only the durable point.
    /// With batched completions a durable chunk is still `Interrupted` in the
    /// file. A crash then forgets it: it reloads `Uninitialized` with its
    /// placement dropped. Its cached body may be the only copy, so the gate
    /// stays closed until the completion rewrite.
    #[test]
    fn prune_gate_waits_for_the_persisted_completion() -> eyre::Result<()> {
        let (_tmp, config, storage) =
            open_presence_module_with("prune_gate_persisted", SubmoduleIndexEngine::Rocksdb, 3600)?;
        let data_root = H256::repeat_byte(5);
        let offset = PartitionChunkOffset::from(0);
        let candidates = BTreeSet::from([TxChunkOffset::from(0)]);
        let register = |storage: &StorageModule| -> eyre::Result<()> {
            let (_, submodule) = storage.get_submodule_for_offset(offset)?;
            submodule.db.update_registration(|tx| {
                tx.add_tx_leaf_binding(
                    data_root,
                    &TxLeafBinding {
                        data_root,
                        prefix_hash: H256::zero(),
                    },
                )?;
                tx.add_tx_path_hash_to_offset_range(offset, offset, Some(data_root))?;
                tx.add_data_root_info(
                    data_root,
                    &DataRootInfo {
                        start_offset: RelativeChunkOffset(0),
                        data_size: 32,
                    },
                )
            })
        };

        register(&storage)?;
        assert!(storage.write_chunk(offset, vec![9_u8; 32], ChunkType::Data));
        storage.sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(offset));
        assert!(!storage.is_data_chunk_persisted_at(offset));
        assert!(
            storage
                .durable_tx_offsets_for_data_root(data_root, &candidates)?
                .is_empty()
        );

        // Crash before the completion rewrite: the node forgets the chunk.
        storage.suppress_interval_flush_for_test();
        drop(storage);
        let storage = reopen_presence(&config)?;
        assert_eq!(
            storage.get_chunk_type(&offset),
            Some(ChunkType::Uninitialized)
        );
        assert!(storage.collect_data_root_infos(data_root)?.0.is_empty());

        // Written again, the gate opens once the file says `Data`.
        register(&storage)?;
        assert!(storage.write_chunk(offset, vec![9_u8; 32], ChunkType::Data));
        storage.sync_pending_chunks()?;
        assert!(
            storage
                .durable_tx_offsets_for_data_root(data_root, &candidates)?
                .is_empty()
        );
        storage.flush_interval_completions_locked(true)?;
        assert!(storage.is_data_chunk_persisted_at(offset));
        assert_eq!(
            storage.durable_tx_offsets_for_data_root(data_root, &candidates)?,
            candidates
        );
        Ok(())
    }

    /// Rollback clears the interval while a packed chunk is still queued.
    /// The durable point must not mark that chunk stored: the proof has
    /// nowhere to go, so the chunk stays pending and the interval stays
    /// `Uninitialized`.
    #[test]
    fn uncovered_proof_stays_queued_after_interval_rollback() -> eyre::Result<()> {
        let (_tmp, _config, storage) =
            open_presence_module("uncovered_proof", SubmoduleIndexEngine::Rocksdb)?;
        let (_, submodule) = storage.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        let data_root = H256::repeat_byte(4);
        submodule.db.update(|tx| {
            tx.add_tx_leaf_binding(
                data_root,
                &TxLeafBinding {
                    data_root,
                    prefix_hash: H256::zero(),
                },
            )?;
            tx.add_tx_path_hash_to_offset_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(0),
                Some(data_root),
            )?;
            tx.add_data_root_info(
                data_root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(0),
                    data_size: 32,
                },
            )
        })?;

        let offset = PartitionChunkOffset::from(0);
        {
            let mut pending = storage.pending_writes.write().unwrap();
            pending.insert(offset, (vec![9_u8; 32], ChunkType::Data));
            pending
                .priorities
                .insert(offset, disk_lane::WritePriority::Ingress);
            pending.queued_at.insert(offset, Instant::now());
            pending
                .proofs
                .insert(offset, (H256::repeat_byte(1), vec![1, 2, 3]));
        }
        storage.clear_offset_index_in_range(offset, offset)?;

        let before = index_read_metrics::uncovered_data_path_for_test();
        storage.force_sync_pending_chunks()?;
        assert!(
            index_read_metrics::uncovered_data_path_for_test() > before,
            "an uncovered proof must count"
        );
        assert!(!storage.is_data_chunk_durable_at(offset));
        assert!(storage.is_data_write_pending_at(offset));
        assert!(data_path_at(&storage, 0)?.is_none());
        assert!(
            storage
                .pending_writes
                .read()
                .unwrap()
                .proofs
                .contains_key(&offset)
        );
        assert_eq!(
            storage
                .intervals()
                .read()
                .unwrap()
                .get_at_point(offset)
                .copied(),
            Some(ChunkType::Uninitialized)
        );
        Ok(())
    }

    struct SeededIndex {
        tx_hash: H256,
        data_root: H256,
        tx_path: Vec<u8>,
        leaf: TxLeafBinding,
        info: DataRootInfo,
    }

    fn presence_info() -> StorageModuleInfo {
        StorageModuleInfo {
            id: 0,
            partition_assignment: None,
            submodules: vec![(partition_chunk_offset_ii!(0, 7), "sub".into())],
        }
    }

    fn open_presence_module(
        prefix: &str,
        engine: SubmoduleIndexEngine,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        Config,
        StorageModule,
    )> {
        open_presence_module_with(prefix, engine, 0)
    }

    fn open_presence_module_with(
        prefix: &str,
        engine: SubmoduleIndexEngine,
        intervals_persist_secs: u64,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        Config,
        StorageModule,
    )> {
        let tmp = TempDirBuilder::new().prefix(prefix).build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 8,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
                intervals_persist_secs,
                min_durable_interval_ms: 0,
                ..StorageSyncConfig::default()
            },
            base_directory: tmp.path().to_path_buf(),
            database: index_database(engine),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage = StorageModule::new(&presence_info(), &config)?;
        Ok((tmp, config, storage))
    }

    fn reopen_presence(config: &Config) -> eyre::Result<StorageModule> {
        StorageModule::new(&presence_info(), config)
    }

    fn for_each_index_engine(
        prefix: &str,
        mut f: impl FnMut(&Config, StorageModule) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        for engine in [SubmoduleIndexEngine::Mdbx, SubmoduleIndexEngine::Rocksdb] {
            let (tmp, config, storage) =
                open_presence_module(&format!("{prefix}_{engine:?}"), engine)?;
            f(&config, storage).map_err(|err| eyre!("{engine:?}: {err}"))?;
            drop(tmp);
        }
        Ok(())
    }

    fn seed_mixed_index(module: &StorageModule) -> eyre::Result<SeededIndex> {
        let tx_hash = H256::repeat_byte(9);
        let data_root = H256::repeat_byte(4);
        let leaf = TxLeafBinding {
            data_root,
            prefix_hash: H256::repeat_byte(1),
        };
        // Readiness re-checks the tx-path leaf against fold(data_root, prefix_hash).
        // A leaf-only proof is the shortest bytes that check accepts.
        let folded = DataTransactionLedger::fold_tx_root_leaf(data_root, leaf.prefix_hash);
        let mut tx_path = folded.0.to_vec();
        tx_path.extend_from_slice(&[0_u8; 32]);
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
        let (_, submodule) = module.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        submodule.db.update(|tx| {
            tx.add_full_tx_path(tx_hash, tx_path.clone())?;
            tx.add_tx_leaf_binding(tx_hash, &leaf)?;
            tx.add_tx_path_hash_to_offset_range(
                PartitionChunkOffset::from(1),
                PartitionChunkOffset::from(6),
                Some(tx_hash),
            )?;
            tx.add_data_root_info(data_root, &info)?;
            tx.write_data_path_updates(vec![
                (PartitionChunkOffset::from(2), H256::repeat_byte(2), vec![2]),
                (PartitionChunkOffset::from(3), H256::repeat_byte(3), vec![3]),
                (PartitionChunkOffset::from(6), H256::repeat_byte(6), vec![6]),
            ])?;
            tx.add_pending_body_migration(PartitionChunkOffset::from(3), &pending)?;
            Ok(())
        })?;
        assert_index_consistent(module)?;
        Ok(SeededIndex {
            tx_hash,
            data_root,
            tx_path,
            leaf,
            info,
        })
    }

    fn data_path_at(module: &StorageModule, offset: u32) -> eyre::Result<Option<Vec<u8>>> {
        let (_, submodule) = module.get_submodule_for_offset(PartitionChunkOffset::from(offset))?;
        submodule
            .db
            .view(|tx| tx.get_data_path_by_offset(PartitionChunkOffset::from(offset)))
    }

    fn presence_offsets(module: &StorageModule) -> eyre::Result<Vec<u32>> {
        let (_, submodule) = module.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        let rows = submodule.db.view(|tx| {
            tx.path_hashes_in_inclusive_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(7),
            )
        })?;
        Ok(rows.into_iter().map(|(offset, _)| offset.0).collect())
    }

    fn assert_index_consistent(module: &StorageModule) -> eyre::Result<()> {
        for (_, submodule) in module.submodules.iter() {
            let findings = submodule.db.audit_index()?;
            eyre::ensure!(
                findings.is_empty(),
                "index audit found {} row(s): {findings:?}",
                findings.len()
            );
        }
        Ok(())
    }

    /// Schema v3: a filled placement stays consistent across a second
    /// registration, an in-range pending move, and a settle.
    #[test]
    fn rocks_settle_advance_and_reregister_stay_consistent() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("rocks_body_audit")
            .with_tracing()
            .build();
        let chunk_size = 32_u64;
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: 20,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp_dir.path().to_path_buf(),
            database: index_database(SubmoduleIndexEngine::Rocksdb),
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
                submodules: vec![(partition_chunk_offset_ii!(0, 19), "hdd0".into())],
            },
            &config,
        )?;
        let data_root = H256::repeat_byte(4);
        let data_size = 8 * chunk_size;
        let header = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_root,
                data_size,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });
        let range = LedgerChunkRange(ledger_chunk_offset_ii!(0, 7));
        let height = 11_u64;
        storage_module.index_transaction_data(&header, &vec![1, 2, 3, 4], range, height)?;
        assert_index_consistent(&storage_module)?;
        // Same tx again: the interval, data-root row, and pending job stay owned.
        storage_module.index_transaction_data(&header, &vec![1, 2, 3, 4], range, height)?;
        assert_index_consistent(&storage_module)?;

        let jobs = storage_module.pending_body_migrations()?;
        assert_eq!(jobs.len(), 1);
        let moved = storage_module.advance_pending_body_migration(
            jobs[0].0,
            PartitionChunkOffset::from(3),
            &jobs[0].1,
        )?;
        assert_eq!(moved, PartitionChunkOffset::from(3));
        assert_index_consistent(&storage_module)?;

        assert!(storage_module.settle_pending_body_migration(
            PartitionChunkOffset::from(3),
            data_root,
            height
        )?);
        assert!(storage_module.pending_body_migrations()?.is_empty());
        assert_index_consistent(&storage_module)?;
        Ok(())
    }

    /// Uninitialized `[3, 5]` drops pending and splits the tx interval.
    /// Entropy `[6, 7]` keeps the tx remnant. The data-root extent is `[1, 2]`.
    fn assert_entropy_hole_kept(module: &StorageModule, seeded: &SeededIndex) -> eyre::Result<()> {
        let (_, submodule) = module.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        let stored_paths = submodule.db.stores_tx_paths();
        if stored_paths {
            assert_eq!(
                submodule
                    .db
                    .view(|tx| tx.get_full_tx_path(seeded.tx_hash))?,
                Some(seeded.tx_path.clone())
            );
            assert_eq!(
                submodule
                    .db
                    .view(|tx| tx.get_tx_leaf_binding(seeded.tx_hash))?,
                Some(seeded.leaf.clone())
            );
        } else {
            assert!(
                submodule
                    .db
                    .view(|tx| tx.get_full_tx_path(seeded.tx_hash))?
                    .is_none()
            );
            assert!(
                submodule
                    .db
                    .view(|tx| tx.get_tx_leaf_binding(seeded.tx_hash))?
                    .is_none()
            );
        }
        assert_eq!(
            submodule
                .db
                .view(|tx| tx.get_data_root_infos_for_data_root(seeded.data_root))?,
            Some(DataRootInfos(vec![seeded.info.clone()]))
        );
        assert!(
            submodule
                .db
                .view(|tx| tx.get_pending_body_migration(PartitionChunkOffset::from(3)))?
                .is_none()
        );
        assert!(
            submodule
                .db
                .view(|tx| tx.get_tx_path_by_offset(PartitionChunkOffset::from(3)))?
                .is_none()
        );
        if stored_paths {
            assert_eq!(
                submodule
                    .db
                    .view(|tx| tx.get_tx_path_by_offset(PartitionChunkOffset::from(2)))?,
                Some(seeded.tx_path.clone())
            );
            assert_eq!(
                submodule
                    .db
                    .view(|tx| tx.get_tx_path_by_offset(PartitionChunkOffset::from(6)))?,
                Some(seeded.tx_path.clone())
            );
        } else {
            assert!(
                submodule
                    .db
                    .view(|tx| tx.get_tx_path_by_offset(PartitionChunkOffset::from(2)))?
                    .is_none()
            );
            assert!(
                submodule
                    .db
                    .view(|tx| tx.get_tx_path_by_offset(PartitionChunkOffset::from(6)))?
                    .is_none()
            );
        }
        let at_two = submodule
            .db
            .view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(2)))?
            .expect("data span keeps the row");
        if stored_paths {
            assert_eq!(at_two.tx_path_hash, Some(seeded.tx_hash));
            assert!(at_two.data_path_hash.is_some());
        } else {
            // The interval stores the data root. The data-path hash is the bytes.
            assert_eq!(at_two.tx_path_hash, Some(seeded.data_root));
            assert_eq!(
                at_two.data_path_hash,
                Some(H256(irys_types::hash_sha256(&[2])))
            );
        }
        assert!(
            submodule
                .db
                .view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(3)))?
                .is_none()
        );
        let at_six = submodule
            .db
            .view(|tx| tx.get_path_hashes_by_offset(PartitionChunkOffset::from(6)))?
            .expect("entropy keeps tx coverage");
        if stored_paths {
            assert_eq!(at_six.tx_path_hash, Some(seeded.tx_hash));
        } else {
            assert_eq!(at_six.tx_path_hash, Some(seeded.data_root));
        }
        assert!(at_six.data_path_hash.is_none());
        assert!(module.is_data_root_index_ready_at(PartitionChunkOffset::from(2)));
        assert!(!module.is_data_root_index_ready_at(PartitionChunkOffset::from(3)));
        Ok(())
    }

    /// A fully `Uninitialized` span drops intervals, placements, pending jobs,
    /// and the tx path once nothing in the submodule names it.
    fn assert_uninitialized_coverage_dropped(
        module: &StorageModule,
        seeded: &SeededIndex,
    ) -> eyre::Result<()> {
        let (_, submodule) = module.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        assert!(
            submodule
                .db
                .view(|tx| tx.get_full_tx_path(seeded.tx_hash))?
                .is_none()
        );
        assert!(
            submodule
                .db
                .view(|tx| tx.get_tx_leaf_binding(seeded.tx_hash))?
                .is_none()
        );
        for offset in [2_u32, 3, 6] {
            assert!(
                submodule
                    .db
                    .view(|tx| tx.get_tx_path_by_offset(PartitionChunkOffset::from(offset)))?
                    .is_none(),
                "offset {offset}"
            );
        }
        assert!(
            submodule
                .db
                .view(|tx| tx.get_pending_body_migration(PartitionChunkOffset::from(3)))?
                .is_none()
        );
        let infos = submodule
            .db
            .view(|tx| tx.get_data_root_infos_for_data_root(seeded.data_root))?;
        assert!(infos.as_ref().is_none_or(|infos| infos.0.is_empty()));
        assert!(!module.is_data_root_index_ready_at(PartitionChunkOffset::from(2)));
        Ok(())
    }

    fn index_sequence(module: &StorageModule) -> eyre::Result<u64> {
        let (_, submodule) = module.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        submodule.db.committed_sequence()
    }

    fn write_interval_spans(
        module: &StorageModule,
        spans: &[(u32, u32, ChunkType)],
    ) -> eyre::Result<()> {
        let mut map = StorageIntervals::new();
        for &(start, end, kind) in spans {
            map.insert_merge_touching_if_values_equal(partition_chunk_offset_ii!(start, end), kind)
                .expect("interval span");
        }
        let path = submodule_interval_path(module, 0);
        let mut file = get_atomic_file(&path)?;
        file.write_all(serde_json::to_string(&map)?.as_bytes())?;
        file.commit()?;
        Ok(())
    }

    #[test]
    fn reopen_drops_presence_under_interrupted_and_keeps_data() -> eyre::Result<()> {
        for_each_index_engine("presence_interrupted", |config, storage| {
            let seeded = seed_mixed_index(&storage)?;
            write_interval_spans(
                &storage,
                &[
                    (0, 2, ChunkType::Data),
                    (3, 5, ChunkType::Interrupted),
                    (6, 7, ChunkType::Entropy),
                ],
            )?;
            // Shutdown must not rewrite the crash image.
            storage.suppress_interval_flush_for_test();
            drop(storage);

            let reloaded = reopen_presence(config)?;
            assert_eq!(
                reloaded.get_chunk_type(&PartitionChunkOffset::from(2)),
                Some(ChunkType::Data)
            );
            assert_eq!(
                reloaded.get_chunk_type(&PartitionChunkOffset::from(3)),
                Some(ChunkType::Uninitialized)
            );
            assert_eq!(
                reloaded.get_chunk_type(&PartitionChunkOffset::from(6)),
                Some(ChunkType::Entropy)
            );
            assert_eq!(data_path_at(&reloaded, 2)?, Some(vec![2]));
            assert_eq!(data_path_at(&reloaded, 3)?, None);
            assert_eq!(data_path_at(&reloaded, 6)?, None);
            assert_eq!(presence_offsets(&reloaded)?, vec![2]);
            assert_entropy_hole_kept(&reloaded, &seeded)?;
            assert_index_consistent(&reloaded)?;
            Ok(())
        })
    }

    #[test]
    fn offline_uninitialized_edit_drops_presence() -> eyre::Result<()> {
        for_each_index_engine("presence_offline_edit", |config, storage| {
            let seeded = seed_mixed_index(&storage)?;
            {
                let mut intervals = storage.intervals.write().unwrap();
                *intervals = StorageIntervals::new();
                intervals
                    .insert_merge_touching_if_values_equal(
                        partition_chunk_offset_ii!(0, 7),
                        ChunkType::Data,
                    )
                    .expect("data span");
            }
            storage.write_intervals_to_submodules()?;
            let path = submodule_interval_path(&storage, 0);
            drop(storage);

            let text = fs::read_to_string(&path)?;
            assert!(text.contains("\"Data\""), "{text}");
            let edited = text.replace("\"Data\"", "\"Uninitialized\"");
            fs::write(&path, edited)?;

            let reloaded = reopen_presence(config)?;
            assert_eq!(
                reloaded.get_chunk_type(&PartitionChunkOffset::from(2)),
                Some(ChunkType::Uninitialized)
            );
            assert_eq!(data_path_at(&reloaded, 2)?, None);
            assert_eq!(data_path_at(&reloaded, 3)?, None);
            assert_eq!(data_path_at(&reloaded, 6)?, None);
            assert!(presence_offsets(&reloaded)?.is_empty());
            assert_uninitialized_coverage_dropped(&reloaded, &seeded)?;
            assert_index_consistent(&reloaded)?;
            Ok(())
        })
    }

    #[test]
    fn reset_crash_at_each_step_serves_nothing_on_reopen() -> eyre::Result<()> {
        for point in [
            SyncFailurePoint::AfterResetMarker,
            SyncFailurePoint::AfterResetClear,
            SyncFailurePoint::AfterResetIntervals,
        ] {
            for_each_index_engine(&format!("presence_reset_{point:?}"), |config, storage| {
                let seeded = seed_mixed_index(&storage)?;
                assert!(storage.write_chunk(
                    PartitionChunkOffset::from(2),
                    vec![9_u8; 32],
                    ChunkType::Data
                ));
                storage.force_sync_pending_chunks()?;
                storage.fail_next_sync_at(point);
                let err = storage.reset().unwrap_err();
                assert!(err.to_string().contains(&format!("{point:?}")), "{err}");
                let (_, submodule) =
                    storage.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
                assert!(submodule.db.reset_in_progress()?);
                match point {
                    SyncFailurePoint::AfterResetMarker => {
                        assert_eq!(data_path_at(&storage, 2)?, Some(vec![2]));
                        assert_eq!(
                            storage.get_chunk_type(&PartitionChunkOffset::from(2)),
                            Some(ChunkType::Data)
                        );
                    }
                    SyncFailurePoint::AfterResetClear | SyncFailurePoint::AfterResetIntervals => {
                        assert!(data_path_at(&storage, 2)?.is_none());
                        assert_eq!(
                            storage.get_chunk_type(&PartitionChunkOffset::from(2)),
                            Some(ChunkType::Uninitialized)
                        );
                        assert!(
                            storage
                                .generate_full_chunk(PartitionChunkOffset::from(2))?
                                .is_none()
                        );
                    }
                    _ => {}
                }
                storage.suppress_interval_flush_for_test();
                drop(storage);

                let reloaded = reopen_presence(config)?;
                assert_eq!(
                    reloaded.get_chunk_type(&PartitionChunkOffset::from(2)),
                    Some(ChunkType::Uninitialized)
                );
                assert!(data_path_at(&reloaded, 2)?.is_none());
                assert!(data_path_at(&reloaded, 3)?.is_none());
                assert!(data_path_at(&reloaded, 6)?.is_none());
                assert!(presence_offsets(&reloaded)?.is_empty());
                // The reset clear removes every index family, including the
                // tx-path blob. Open coverage repair only runs when the clear
                // did not.
                let (_, reloaded_sub) =
                    reloaded.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
                assert!(
                    reloaded_sub
                        .db
                        .view(|tx| tx.get_full_tx_path(seeded.tx_hash))?
                        .is_none()
                );
                assert!(
                    reloaded_sub
                        .db
                        .view(|tx| tx.get_tx_path_by_offset(PartitionChunkOffset::from(2)))?
                        .is_none()
                );
                assert!(
                    reloaded_sub
                        .db
                        .view(|tx| tx.get_pending_body_migration(PartitionChunkOffset::from(3)))?
                        .is_none()
                );
                let infos = reloaded_sub
                    .db
                    .view(|tx| tx.get_data_root_infos_for_data_root(seeded.data_root))?;
                assert!(infos.as_ref().is_none_or(|infos| infos.0.is_empty()));
                assert!(!reloaded.is_data_root_index_ready_at(PartitionChunkOffset::from(2)));
                assert!(
                    reloaded
                        .generate_full_chunk(PartitionChunkOffset::from(2))?
                        .is_none()
                );
                assert!(!reloaded_sub.db.reset_in_progress()?);
                assert!(!reloaded.reset_incomplete());
                assert_index_consistent(&reloaded)?;
                Ok(())
            })?;
        }
        Ok(())
    }

    #[test]
    fn reset_error_stays_incomplete() -> eyre::Result<()> {
        for_each_index_engine("presence_reset_err", |_config, storage| {
            storage.fail_next_reset_for_test();
            let err = storage.reset().unwrap_err();
            assert!(err.to_string().contains("injected"), "{err}");
            assert!(storage.reset_incomplete());
            let (_, submodule) = storage.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
            assert!(!submodule.db.reset_in_progress()?);
            storage.reset()?;
            assert!(!storage.reset_incomplete());
            assert_index_consistent(&storage)?;
            Ok(())
        })
    }

    /// `reset` during the presence-copy gap must not leave that proof in the index.
    ///
    /// The durable point holds `sync_in_progress` at the pause. `reset` has to
    /// take the same lock, so it runs only after the point commits and drops
    /// the lock. The copied rows are then cleared with the rest of the index.
    #[test]
    fn reset_during_presence_copy_leaves_no_data_path() -> eyre::Result<()> {
        let mut errors = Vec::new();
        for engine in [SubmoduleIndexEngine::Mdbx, SubmoduleIndexEngine::Rocksdb] {
            let prefix = format!("reset_during_presence_copy_{engine:?}");
            let outcome = (|| -> eyre::Result<()> {
                let (_tmp, _config, storage) = open_presence_module(&prefix, engine)?;
                let offset = PartitionChunkOffset::from(2);
                let tx_hash = H256::repeat_byte(9);
                let data_root = H256::repeat_byte(4);
                let leaf = TxLeafBinding {
                    data_root,
                    prefix_hash: H256::repeat_byte(1),
                };
                // Schema v3 writes the interval only when this batch also stores
                // the leaf. That interval is what lets a pre-clear proof commit
                // on RocksDB. MDBX stores the proof either way.
                let folded = DataTransactionLedger::fold_tx_root_leaf(data_root, leaf.prefix_hash);
                let mut tx_path = folded.0.to_vec();
                tx_path.extend_from_slice(&[0_u8; 32]);
                let (_, submodule) = storage.get_submodule_for_offset(offset)?;
                submodule.db.update(|tx| {
                    tx.add_full_tx_path(tx_hash, tx_path.clone())?;
                    tx.add_tx_leaf_binding(tx_hash, &leaf)?;
                    tx.add_tx_path_hash_to_offset_range(
                        PartitionChunkOffset::from(1),
                        PartitionChunkOffset::from(6),
                        Some(tx_hash),
                    )?;
                    Ok(())
                })?;
                {
                    let mut pending = storage.pending_writes.write().unwrap();
                    pending.insert(offset, (vec![9_u8; 32], ChunkType::Data));
                    pending
                        .priorities
                        .insert(offset, disk_lane::WritePriority::Ingress);
                    pending.queued_at.insert(offset, Instant::now());
                    pending
                        .proofs
                        .insert(offset, (H256::repeat_byte(7), vec![7, 7, 7]));
                }
                storage.pause_next_sync_at(SyncPausePoint::AfterPresenceCopy);

                let storage_ref = &storage;
                let (sync_result, reset_result, reset_finished_while_paused, sync_still_held) =
                    std::thread::scope(|scope| {
                        struct ReleasePause<'a>(&'a StorageModule);
                        impl Drop for ReleasePause<'_> {
                            fn drop(&mut self) {
                                self.0.release_sync_pause();
                            }
                        }
                        let _release = ReleasePause(storage_ref);
                        let sync_thread =
                            scope.spawn(move || storage_ref.force_sync_pending_chunks());
                        assert!(
                            storage_ref.wait_for_sync_pause(Duration::from_secs(5)),
                            "durable point did not pause after the proof copy"
                        );
                        let (reset_tx, reset_rx) = std::sync::mpsc::channel();
                        scope.spawn(move || {
                            let result = storage_ref.reset();
                            let _ = reset_tx.send(result);
                        });

                        let deadline = Instant::now() + Duration::from_millis(500);
                        let mut early = None;
                        while Instant::now() < deadline {
                            match reset_rx.try_recv() {
                                Ok(result) => {
                                    early = Some(result);
                                    break;
                                }
                                Err(std::sync::mpsc::TryRecvError::Empty) => {
                                    std::thread::sleep(Duration::from_millis(10));
                                }
                                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                    panic!("reset thread dropped the result");
                                }
                            }
                        }
                        let reset_finished_while_paused = early.is_some();
                        let sync_still_held = storage_ref.sync_in_progress.try_lock().is_err();
                        storage_ref.release_sync_pause();
                        let sync_result = sync_thread.join().expect("sync thread");
                        let reset_result = match early {
                            Some(result) => result,
                            None => reset_rx.recv_timeout(Duration::from_secs(5)).expect(
                                "reset did not finish after the durable point was released",
                            ),
                        };
                        (
                            sync_result,
                            reset_result,
                            reset_finished_while_paused,
                            sync_still_held,
                        )
                    });

                sync_result?;
                reset_result?;
                let path = data_path_at(&storage, offset.0)?;
                let pending = storage.pending_writes.read().unwrap();
                let pending_clear = pending.chunks.is_empty()
                    && pending.proofs.is_empty()
                    && pending.written_ahead.is_empty()
                    && pending.occupancy.is_empty();
                drop(pending);
                let intervals = storage.get_intervals(ChunkType::Uninitialized);
                eyre::ensure!(
                    path.is_none()
                        && !reset_finished_while_paused
                        && sync_still_held
                        && pending_clear
                        && intervals == vec![partition_chunk_offset_ii!(0, 7)]
                        && storage.get_intervals(ChunkType::Data).is_empty(),
                    "path={path:?} reset_finished_while_paused={reset_finished_while_paused} sync_still_held={sync_still_held} pending_clear={pending_clear} intervals={intervals:?}"
                );
                let (_, submodule) = storage.get_submodule_for_offset(offset)?;
                assert!(
                    submodule
                        .db
                        .view(|tx| tx.get_tx_path_by_offset(offset))?
                        .is_none()
                );
                assert_index_consistent(&storage)?;
                assert!(!storage.reset_incomplete());
                Ok(())
            })();
            if let Err(err) = outcome {
                errors.push(format!("{engine:?}: {err}"));
            }
        }
        eyre::ensure!(errors.is_empty(), "{}", errors.join(" | "));
        Ok(())
    }

    /// A registration that has computed its rows must not commit them after
    /// the assignment changes. The same ledger and slot still commits.
    /// A registration that starts after the reset returns commits normally.
    #[test]
    fn registration_commit_follows_the_assignment() -> eyre::Result<()> {
        let mut errors = Vec::new();
        for engine in [SubmoduleIndexEngine::Mdbx, SubmoduleIndexEngine::Rocksdb] {
            let prefix = format!("registration_assignment_{engine:?}");
            let outcome = (|| -> eyre::Result<()> {
                let tmp = TempDirBuilder::new().prefix(&prefix).build();
                let node_config = NodeConfig {
                    consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                        chunk_size: 32,
                        num_chunks_in_partition: 8,
                        ..ConsensusConfig::testing()
                    }),
                    storage: StorageSyncConfig {
                        num_writes_before_sync: 1,
                        intervals_persist_secs: 0,
                        min_durable_interval_ms: 0,
                        ..StorageSyncConfig::default()
                    },
                    base_directory: tmp.path().to_path_buf(),
                    database: index_database(engine),
                    ..NodeConfig::testing()
                };
                let config = Config::new_with_random_peer_id(node_config);
                let height = 9_u64;
                let original = PartitionAssignment {
                    partition_hash: H256::repeat_byte(0x44),
                    miner_address: config.node_config.miner_address(),
                    ledger_id: Some(DataLedger::Publish.into()),
                    slot_index: Some(0),
                };
                let storage = StorageModule::new(
                    &StorageModuleInfo {
                        id: 0,
                        partition_assignment: Some(original),
                        submodules: vec![(
                            partition_chunk_offset_ii!(0, 7),
                            tmp.path().join("sub"),
                        )],
                    },
                    &config,
                )?;
                let _tmp = tmp;
                let mut other_slot = original;
                other_slot.slot_index = Some(1);
                let mut capacity = original;
                capacity.ledger_id = None;
                capacity.slot_index = None;

                let mut case_errors = Vec::new();
                let dropped = race_registration(&storage, None, height, "unassign")?;
                if !(dropped.reset_finished_while_paused
                    && dropped.rows.is_empty()
                    && storage.partition_assignment().is_none()
                    && dropped.registration_drops == 1)
                {
                    case_errors.push(format!(
                        "unassign: {dropped:?} assignment={:?}",
                        storage.partition_assignment()
                    ));
                }
                assert_index_consistent(&storage)?;

                storage.reset_with_assignment(Some(original), height)?;
                let kept = race_registration(&storage, Some(original), height, "same-slot")?;
                if !(kept.reset_finished_while_paused
                    && kept.rows.is_present()
                    && storage.partition_assignment() == Some(original)
                    && kept.registration_drops == dropped.registration_drops)
                {
                    case_errors.push(format!("same-slot: {kept:?}"));
                }
                assert_index_consistent(&storage)?;

                let moved = race_registration(&storage, Some(other_slot), height, "other-slot")?;
                if !(moved.reset_finished_while_paused
                    && moved.rows.is_empty()
                    && storage.partition_assignment() == Some(other_slot)
                    && moved.registration_drops == kept.registration_drops + 1)
                {
                    case_errors.push(format!(
                        "other-slot: {moved:?} assignment={:?}",
                        storage.partition_assignment()
                    ));
                }
                assert_index_consistent(&storage)?;

                storage.reset_with_assignment(Some(original), height)?;
                let expired =
                    race_registration(&storage, Some(capacity), height, "expiry-capacity")?;
                if !(expired.reset_finished_while_paused
                    && expired.rows.is_empty()
                    && storage.partition_assignment() == Some(capacity)
                    && expired.registration_drops == moved.registration_drops + 1)
                {
                    case_errors.push(format!(
                        "expiry-capacity: {expired:?} assignment={:?}",
                        storage.partition_assignment()
                    ));
                }
                assert_index_consistent(&storage)?;

                storage.reset_with_assignment(Some(original), height)?;
                let before_drops = storage.registration_drops.load(Ordering::SeqCst);
                index_one(&storage, 2)?;
                let after = registration_rows(&storage)?;
                if !(after.is_present()
                    && storage.registration_drops.load(Ordering::SeqCst) == before_drops
                    && storage.partition_assignment() == Some(original))
                {
                    case_errors.push(format!("registration after reset: {after:?}"));
                }
                assert_index_consistent(&storage)?;
                eyre::ensure!(case_errors.is_empty(), "{}", case_errors.join("; "));
                Ok(())
            })();
            if let Err(err) = outcome {
                errors.push(format!("{engine:?}: {err}"));
            }
        }
        eyre::ensure!(errors.is_empty(), "{}", errors.join(" | "));
        Ok(())
    }

    struct RegistrationRows {
        placement: bool,
        infos: usize,
        jobs: usize,
    }

    impl RegistrationRows {
        fn is_empty(&self) -> bool {
            !self.placement && self.infos == 0 && self.jobs == 0
        }

        fn is_present(&self) -> bool {
            self.placement && self.infos == 1 && self.jobs == 1
        }
    }

    impl std::fmt::Debug for RegistrationRows {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("RegistrationRows")
                .field("placement", &self.placement)
                .field("infos", &self.infos)
                .field("jobs", &self.jobs)
                .finish()
        }
    }

    struct RaceOutcome {
        rows: RegistrationRows,
        reset_finished_while_paused: bool,
        registration_drops: u64,
    }

    impl std::fmt::Debug for RaceOutcome {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("RaceOutcome")
                .field("rows", &self.rows)
                .field(
                    "reset_finished_while_paused",
                    &self.reset_finished_while_paused,
                )
                .field("registration_drops", &self.registration_drops)
                .finish()
        }
    }

    fn registration_rows(storage: &StorageModule) -> eyre::Result<RegistrationRows> {
        let (_, submodule) = storage.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        let placement = submodule
            .db
            .view(|tx| tx.placement_at(PartitionChunkOffset::from(0)))?
            .is_some();
        let infos = storage
            .collect_data_root_infos(H256::repeat_byte(4))?
            .0
            .len();
        let jobs = storage.pending_body_migrations()?.len();
        Ok(RegistrationRows {
            placement,
            infos,
            jobs,
        })
    }

    fn index_one(storage: &StorageModule, tx_index: u32) -> eyre::Result<()> {
        let data_root = H256::repeat_byte(4);
        let prefix_hash = H256::repeat_byte(1);
        let chunk_size = storage.config.consensus.chunk_size;
        let chunks = storage.config.consensus.num_chunks_in_partition;
        let data_size = chunks * chunk_size;
        let header = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_root,
                data_size,
                prefix_hash,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });
        let folded = DataTransactionLedger::fold_tx_root_leaf(data_root, prefix_hash);
        let mut tx_path = folded.0.to_vec();
        tx_path.extend_from_slice(&[0_u8; 32]);
        let range = LedgerChunkRange(ledger_chunk_offset_ii!(0, chunks - 1));
        storage.index_transaction_data_in(
            &header,
            &tx_path,
            range,
            11,
            Some(DataLedger::Publish),
            Some(tx_index),
        )
    }

    /// Pause a registration, then reset and install `next` the way production
    /// does, then release the registration.
    fn race_registration(
        storage: &StorageModule,
        next: Option<PartitionAssignment>,
        update_height: u64,
        _label: &'static str,
    ) -> eyre::Result<RaceOutcome> {
        storage.pause_next_registration_at(RegistrationPausePoint::AfterRowsComputed);
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let reset_finished_while_paused = std::thread::scope(|scope| -> eyre::Result<bool> {
            struct ReleasePause<'a>(&'a StorageModule);
            impl Drop for ReleasePause<'_> {
                fn drop(&mut self) {
                    self.0.release_registration_pause();
                }
            }
            let _release = ReleasePause(storage);
            scope.spawn(|| {
                let result = index_one(storage, 1);
                let _ = result_tx.send(result);
            });
            eyre::ensure!(
                storage.wait_for_registration_pause(Duration::from_secs(5)),
                "registration did not pause after computing its rows"
            );
            let (reset_tx, reset_rx) = std::sync::mpsc::channel();
            scope.spawn(move || {
                let result = storage.reset_with_assignment(next, update_height);
                let _ = reset_tx.send(result);
            });
            let finished = reset_rx.recv_timeout(Duration::from_secs(5));
            let reset_finished_while_paused = match &finished {
                Ok(Ok(_)) => true,
                Ok(Err(err)) => eyre::bail!("reset failed while registration was paused: {err}"),
                Err(_) => false,
            };
            storage.release_registration_pause();
            result_rx
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| eyre::eyre!("registration did not finish after release"))??;
            if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = finished {
                reset_rx
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|_| eyre::eyre!("reset did not finish after release"))??;
            }
            Ok(reset_finished_while_paused)
        })?;
        Ok(RaceOutcome {
            rows: registration_rows(storage)?,
            reset_finished_while_paused,
            registration_drops: storage.registration_drops.load(Ordering::SeqCst),
        })
    }

    #[test]
    fn clean_reopen_does_not_commit() -> eyre::Result<()> {
        for_each_index_engine("presence_noop_reopen", |config, storage| {
            let seq = index_sequence(&storage)?;
            drop(storage);
            let reloaded = reopen_presence(config)?;
            assert_eq!(index_sequence(&reloaded)?, seq);
            assert_index_consistent(&reloaded)?;
            Ok(())
        })
    }

    #[cfg(feature = "bench")]
    #[test]
    fn refuse_foreign_index_rejects_the_other_engine() {
        let dir = TempDirBuilder::new().prefix("index_engine_guard").build();
        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        refuse_foreign_index(&empty, false).unwrap();
        refuse_foreign_index(&empty, true).unwrap();

        let mdbx = dir.path().join("mdbx");
        std::fs::create_dir_all(&mdbx).unwrap();
        std::fs::write(mdbx.join("mdbx.dat"), b"x").unwrap();
        refuse_foreign_index(&mdbx, false).unwrap();
        let rocks_on_mdbx = refuse_foreign_index(&mdbx, true).unwrap_err();
        assert!(
            rocks_on_mdbx.to_string().contains("MDBX"),
            "{rocks_on_mdbx}"
        );

        let rocks = dir.path().join("rocks");
        std::fs::create_dir_all(&rocks).unwrap();
        std::fs::write(rocks.join("CURRENT"), b"MANIFEST-000001\n").unwrap();
        refuse_foreign_index(&rocks, true).unwrap();
        let mdbx_on_rocks = refuse_foreign_index(&rocks, false).unwrap_err();
        assert!(
            mdbx_on_rocks.to_string().contains("RocksDB"),
            "{mdbx_on_rocks}"
        );

        let schema = dir.path().join("schema");
        std::fs::create_dir_all(&schema).unwrap();
        std::fs::write(schema.join("SCHEMA"), b"irys-submodule-rocks 1\n").unwrap();
        assert!(refuse_foreign_index(&schema, false).is_err());
    }

    #[cfg(feature = "bench")]
    #[test]
    fn open_split_refuses_a_foreign_index_directory() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new().prefix("split_index_engine").build();
        let sub = tmp.path().join("sub");
        std::fs::create_dir_all(sub.join("rocks"))?;
        std::fs::write(sub.join("rocks").join("mdbx.dat"), b"not-rocks")?;
        std::fs::create_dir_all(sub.join("db"))?;
        std::fs::write(sub.join("db").join("CURRENT"), b"MANIFEST-000001\n")?;

        let recall_path = tmp.path().join("recall");
        let write_path = tmp.path().join("write");
        std::fs::write(&recall_path, vec![0_u8; 128])?;
        std::fs::write(&write_path, vec![0_u8; 128])?;
        let recall = OpenOptions::new().read(true).open(&recall_path)?;
        let write = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&write_path)?;
        let split = Arc::new(SplitBacking::open(recall, write, 0, 4)?);
        let config = Config::new_with_random_peer_id(NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 4,
                ..ConsensusConfig::testing()
            }),
            base_directory: tmp.path().to_path_buf(),
            ..NodeConfig::testing()
        });
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: None,
            submodules: vec![(partition_chunk_offset_ii!(0, 3), "sub".into())],
        };

        let rocks_err = StorageModule::open_split(
            &info,
            &config,
            Arc::clone(&split),
            BenchIndex::Rocks(RocksTuning::baseline()),
        )
        .expect_err("rocks must refuse an mdbx directory");
        assert!(rocks_err.to_string().contains("MDBX"), "{rocks_err}");

        let mdbx_err = StorageModule::open_split(&info, &config, split, BenchIndex::Mdbx)
            .expect_err("mdbx must refuse a rocks directory");
        assert!(mdbx_err.to_string().contains("RocksDB"), "{mdbx_err}");
        Ok(())
    }

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
        assert_index_consistent(&storage_module)?;
        Ok(())
    }

    /// Moving a job re-keys the same value. A different job at the destination
    /// stays, and so does the source. A paused module does not move the row.
    #[test]
    fn advance_pending_body_migration_rekeys_and_refuses_a_conflict() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("pending_body_advance_test")
            .with_tracing()
            .build();
        let chunk_size = 32_u64;
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: 20,
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
                submodules: vec![(partition_chunk_offset_ii!(0, 19), "hdd0".into())],
            },
            &config,
        )?;
        let height = 4_u64;
        let place = |root: H256, at: u64| {
            let header =
                DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
                    tx: DataTransactionHeaderV1 {
                        data_root: root,
                        data_size: chunk_size,
                        ..Default::default()
                    },
                    metadata: irys_types::DataTransactionMetadata::new(),
                });
            storage_module.index_transaction_data(
                &header,
                &vec![at as u8],
                LedgerChunkRange(ledger_chunk_offset_ii!(at, at)),
                height,
            )
        };
        let root_a = H256::repeat_byte(1);
        let root_b = H256::repeat_byte(2);
        place(root_a, 0)?;
        place(root_b, 1)?;
        assert_index_consistent(&storage_module)?;
        let jobs = storage_module.pending_body_migrations()?;
        let job_a = jobs[0].1.clone();
        assert_eq!(jobs[0].0, PartitionChunkOffset::from(0));

        let stayed = storage_module.advance_pending_body_migration(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(0),
            &job_a,
        )?;
        assert_eq!(stayed, PartitionChunkOffset::from(0));

        let moved = storage_module.advance_pending_body_migration(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(4),
            &job_a,
        )?;
        assert_eq!(moved, PartitionChunkOffset::from(4));
        let jobs = storage_module.pending_body_migrations()?;
        assert_eq!(
            jobs.iter().map(|(key, _)| key.0).collect::<Vec<_>>(),
            vec![1, 4]
        );
        assert_eq!(jobs[1].1, job_a);

        let job_b = jobs[0].1.clone();
        let refused = storage_module.advance_pending_body_migration(
            PartitionChunkOffset::from(1),
            PartitionChunkOffset::from(4),
            &job_b,
        )?;
        assert_eq!(refused, PartitionChunkOffset::from(1));
        assert_eq!(
            storage_module
                .pending_body_migrations()?
                .iter()
                .map(|(key, job)| (key.0, job.data_root))
                .collect::<Vec<_>>(),
            vec![(1, root_b), (4, root_a)]
        );

        storage_module.pause_data_writes();
        let paused = storage_module.advance_pending_body_migration(
            PartitionChunkOffset::from(1),
            PartitionChunkOffset::from(3),
            &job_b,
        )?;
        assert_eq!(paused, PartitionChunkOffset::from(1));
        storage_module.resume_data_writes();
        assert_eq!(storage_module.pending_body_migrations()?.len(), 2);
        Ok(())
    }

    /// Coverage starts at the current row key. A resume drops the durable
    /// prefix. A read stops after `row_cap` rows in that submodule.
    #[test]
    fn coverage_follows_the_advanced_key_and_stops_at_the_page() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("pending_body_coverage_key_test")
            .with_tracing()
            .build();
        let chunk_size = 32_u64;
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: 20,
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
                submodules: vec![(partition_chunk_offset_ii!(0, 19), "hdd0".into())],
            },
            &config,
        )?;
        let height = 9_u64;
        let root = H256::repeat_byte(7);
        let header = DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_root: root,
                data_size: chunk_size * 6,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        });
        storage_module.index_transaction_data(
            &header,
            &vec![1, 2, 3, 4, 5, 6],
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 5)),
            height,
        )?;
        assert_eq!(
            storage_module.pending_body_coverage_ranges()?,
            vec![(0, 5)],
            "the row key is the span start"
        );

        let job = storage_module.pending_body_migrations()?[0].1.clone();
        let moved = storage_module.advance_pending_body_migration(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(3),
            &job,
        )?;
        assert_eq!(moved, PartitionChunkOffset::from(3));
        assert_eq!(
            storage_module.pending_body_coverage_ranges()?,
            vec![(3, 5)],
            "the durable prefix is outside the span"
        );

        // A later job is visible when the cap includes it, and absent when the
        // cap is the first row only.
        let later = H256::repeat_byte(8);
        let later_header =
            DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
                tx: DataTransactionHeaderV1 {
                    data_root: later,
                    data_size: chunk_size,
                    ..Default::default()
                },
                metadata: irys_types::DataTransactionMetadata::new(),
            });
        storage_module.index_transaction_data(
            &later_header,
            &vec![9],
            LedgerChunkRange(ledger_chunk_offset_ii!(8, 8)),
            height,
        )?;
        assert_eq!(
            storage_module.pending_body_coverage_ranges()?,
            vec![(3, 5), (8, 8)]
        );
        assert_eq!(
            storage_module.pending_body_coverage_ranges_limited(1)?,
            vec![(3, 5)],
            "rows past the page are not loaded"
        );
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
            .view(|tx| {
                assert!(
                    tx.get_path_hashes_by_offset(PartitionChunkOffset::from(1))?
                        .is_none()
                );
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
            matches!(err, WriteDataChunkError::Backpressure),
            "expected backpressure, got {err}"
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
    fn busy_placement_blocks_the_free_one() -> eyre::Result<()> {
        use irys_database::submodule::tables::DataRootInfo;
        use irys_types::RelativeChunkOffset;

        let (_tmp, storage_module, chunk) = packed_submit_fixture("busy_placement_blocks_free")?;
        let (_, submodule) =
            storage_module.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        submodule.db.update(|tx| {
            tx.add_data_root_info(
                chunk.data_root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(1),
                    data_size: 5,
                },
            )
        })?;

        let busy = PartitionChunkOffset::from(0);
        let free = PartitionChunkOffset::from(1);
        storage_module.occupy_offset_for_test(busy);
        let err = storage_module
            .write_data_chunk(&chunk)
            .expect_err("a busy placement must not start a partial write");
        assert!(matches!(err, WriteDataChunkError::Backpressure));
        assert_eq!(
            storage_module.get_chunk_type(&busy),
            Some(ChunkType::Entropy)
        );
        assert_eq!(
            storage_module.get_chunk_type(&free),
            Some(ChunkType::Entropy)
        );
        let pending = storage_module.pending_writes.read().unwrap();
        assert!(pending.occupancy.contains_key(&busy));
        assert!(!pending.occupancy.contains_key(&free));
        assert!(pending.get(&free).is_none());
        Ok(())
    }

    #[test]
    fn entropy_read_failure_releases_all_reserved_offsets() -> eyre::Result<()> {
        use irys_database::submodule::tables::DataRootInfo;
        use irys_types::RelativeChunkOffset;

        let (_tmp, storage_module, chunk) = packed_submit_fixture("entropy_read_releases")?;
        let (_, submodule) =
            storage_module.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        submodule.db.update(|tx| {
            tx.add_data_root_info(
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
                recall_direct_io: false,
                intervals_persist_secs: 0,
                index_wal_backstop_ms: irys_types::DEFAULT_INDEX_WAL_BACKSTOP_MS,
                tx_path_derive_threads: irys_types::DEFAULT_TX_PATH_DERIVE_THREADS,
                min_durable_interval_ms: 0,
                durable_headroom_permille: irys_types::DEFAULT_DURABLE_HEADROOM_PERMILLE,
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
        )?;

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
        )?;

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
        use irys_database::submodule::tables::DataRootInfo;
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
        submodule.db.update(|tx| {
            // Canonical: extent [0, 0], entirely below the orphaned range.
            tx.add_data_root_info(
                data_root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(0),
                    data_size: 32,
                },
            )?;
            // Orphaned: starts at offset 2 (below range_start 3) but spans 3
            // chunks → extent [2, 4], reaching into the range. `start_offset`
            // alone (2 < 3) would wrongly keep it; extent overlap drops it.
            tx.add_data_root_info(
                data_root,
                &DataRootInfo {
                    start_offset: RelativeChunkOffset(2),
                    data_size: 96,
                },
            )?;
            // Orphaned: extent [4, 4], fully inside the range.
            tx.add_data_root_info(
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
            tx.add_data_root_info(
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
        use irys_database::submodule::tables::ChunkPathHashes;

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
        submodule.db.update(|tx| {
            for offset in 0..10_u32 {
                tx.set_path_hashes_by_offset(
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
                min_durable_interval_ms: 0,
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
        assert!(ret_path.is_none());

        storage_module.force_sync_pending_chunks()?;

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

    fn interval_batch_module(
        prefix: &str,
        persist_secs: u64,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        Config,
        StorageModule,
    )> {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 11,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
                intervals_persist_secs: persist_secs,
                min_durable_interval_ms: 0,
                ..StorageSyncConfig::default()
            },
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: None,
                submodules: vec![(partition_chunk_offset_ii!(0, 10), "sub".into())],
            },
            &config,
        )?;
        Ok((tmp_dir, config, storage))
    }

    fn file_intervals(module: &StorageModule) -> eyre::Result<StorageIntervals> {
        read_intervals_file(&submodule_interval_path(module, 0))
    }

    #[test]
    fn corrupt_intervals_file_names_its_path() -> eyre::Result<()> {
        let (tmp, config, storage) = interval_batch_module("corrupt_intervals", 0)?;
        let path = submodule_interval_path(&storage, 0);
        drop(storage);
        fs::write(&path, b"not-json{{")?;
        let opened = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: None,
                submodules: vec![(partition_chunk_offset_ii!(0, 10), "sub".into())],
            },
            &config,
        );
        let err = opened.expect_err("a corrupt intervals.json must not panic");
        let message = format!("{err:#}");
        assert!(message.contains(&path.display().to_string()), "{message}");
        drop(tmp);
        Ok(())
    }

    #[test]
    fn period_zero_writes_the_completion_immediately() -> eyre::Result<()> {
        let (_tmp, _config, storage) = interval_batch_module("intervals_period_0", 0)?;
        assert!(storage.write_chunk(
            PartitionChunkOffset::from(0),
            vec![7_u8; 32],
            ChunkType::Data,
        ));
        storage.sync_pending_chunks()?;
        let file = file_intervals(&storage)?;
        assert_eq!(
            file.get_at_point(PartitionChunkOffset::from(0)).copied(),
            Some(ChunkType::Data)
        );
        assert_eq!(
            file.get_at_point(PartitionChunkOffset::from(1)).copied(),
            Some(ChunkType::Uninitialized)
        );
        assert!(
            file.iter()
                .all(|(_, chunk_type)| *chunk_type != ChunkType::Interrupted)
        );
        Ok(())
    }

    /// RocksDB module whose second sync parks. The first point is never deferred.
    fn parked_interval_module(
        prefix: &str,
        persist_secs: u64,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        Config,
        StorageModule,
    )> {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 8,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 0,
                intervals_persist_secs: persist_secs,
                min_durable_interval_ms: 60_000,
                ..StorageSyncConfig::default()
            },
            base_directory: tmp_dir.path().to_path_buf(),
            database: index_database(SubmoduleIndexEngine::Rocksdb),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage = StorageModule::new(&parked_interval_info(), &config)?;
        Ok((tmp_dir, config, storage))
    }

    fn parked_interval_info() -> StorageModuleInfo {
        StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment::default()),
            submodules: vec![(partition_chunk_offset_ii!(0, 7), "sub".into())],
        }
    }

    fn reopen_parked_interval(config: &Config) -> eyre::Result<StorageModule> {
        StorageModule::new(&parked_interval_info(), config)
    }

    /// Offset 0 starts the durable interval. Offset 1 is pwritten and parked.
    fn park_one_data_chunk(storage: &StorageModule) -> eyre::Result<PartitionChunkOffset> {
        let chunk_size = storage.config.consensus.chunk_size as usize;
        assert!(storage.write_chunk(
            PartitionChunkOffset::from(0),
            vec![1_u8; chunk_size],
            ChunkType::Data,
        ));
        storage.sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(PartitionChunkOffset::from(0)));
        let parked = PartitionChunkOffset::from(1);
        assert!(storage.write_chunk(parked, vec![0xA1_u8; chunk_size], ChunkType::Data,));
        storage.sync_pending_chunks()?;
        assert!(
            !storage.is_data_chunk_durable_at(parked),
            "a point inside the interval must park"
        );
        assert!(storage.is_data_write_pending_at(parked));
        assert_eq!(
            storage.get_chunk_type(&parked),
            Some(ChunkType::Data),
            "the live map already says Data"
        );
        Ok(parked)
    }

    #[test]
    fn authoritative_persist_keeps_parked_data_interrupted() -> eyre::Result<()> {
        let (tmp, config, storage) = parked_interval_module("parked_intervals_batch", 30)?;
        let parked = park_one_data_chunk(&storage)?;
        storage.write_intervals_to_submodules()?;
        assert_eq!(
            file_intervals(&storage)?.get_at_point(parked).copied(),
            Some(ChunkType::Interrupted)
        );
        storage.suppress_interval_flush_for_test();
        drop(storage);

        let reloaded = reopen_parked_interval(&config)?;
        assert_eq!(
            reloaded.get_chunk_type(&parked),
            Some(ChunkType::Uninitialized)
        );
        assert!(reloaded.offset_meta_for_test(parked)?.is_none());
        drop(reloaded);
        drop(tmp);
        Ok(())
    }

    #[test]
    fn period_zero_persist_keeps_parked_data_interrupted() -> eyre::Result<()> {
        let (tmp, config, storage) = parked_interval_module("parked_intervals_period_0", 0)?;
        let parked = park_one_data_chunk(&storage)?;
        storage.write_intervals_to_submodules()?;
        assert_eq!(
            file_intervals(&storage)?.get_at_point(parked).copied(),
            Some(ChunkType::Interrupted)
        );
        // The next write-ahead rewrites the live map. The parked offset is
        // still not durable, so the file must keep Interrupted.
        let chunk_size = storage.config.consensus.chunk_size as usize;
        assert!(storage.write_chunk(
            PartitionChunkOffset::from(2),
            vec![2_u8; chunk_size],
            ChunkType::Data,
        ));
        storage.sync_pending_chunks()?;
        assert_eq!(
            file_intervals(&storage)?.get_at_point(parked).copied(),
            Some(ChunkType::Interrupted)
        );
        // Period 0 completion ignores the flush suppress flag. Abandon is the
        // crash: drop must not finish the point and publish Data.
        storage.abandon_on_drop_for_test();
        drop(storage);

        let reloaded = reopen_parked_interval(&config)?;
        assert_eq!(
            reloaded.get_chunk_type(&parked),
            Some(ChunkType::Uninitialized)
        );
        assert!(reloaded.offset_meta_for_test(parked)?.is_none());
        drop(reloaded);
        drop(tmp);
        Ok(())
    }

    #[test]
    fn durable_persist_publishes_parked_data() -> eyre::Result<()> {
        let (tmp, _config, storage) = parked_interval_module("parked_intervals_done", 30)?;
        let parked = park_one_data_chunk(&storage)?;
        storage.force_sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(parked));
        storage.write_intervals_to_submodules()?;
        assert_eq!(
            file_intervals(&storage)?.get_at_point(parked).copied(),
            Some(ChunkType::Data)
        );
        drop(storage);
        drop(tmp);
        Ok(())
    }

    #[test]
    fn batched_completion_lags_the_file_until_shutdown() -> eyre::Result<()> {
        let (tmp, config, storage) = interval_batch_module("intervals_batch_crash", 30)?;
        assert!(storage.write_chunk(
            PartitionChunkOffset::from(0),
            vec![7_u8; 32],
            ChunkType::Data,
        ));
        storage.sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(PartitionChunkOffset::from(0)));
        let path = submodule_interval_path(&storage, 0);
        let before = fs::read(&path)?;
        let file = file_intervals(&storage)?;
        assert_eq!(
            file.get_at_point(PartitionChunkOffset::from(0)).copied(),
            Some(ChunkType::Interrupted)
        );
        assert!(
            file.iter()
                .all(|(_, chunk_type)| *chunk_type != ChunkType::Data)
        );
        // The ahead mark covers the rest of this small submodule. A second
        // sync inside it must not rewrite the file.
        assert!(storage.write_chunk(
            PartitionChunkOffset::from(1),
            vec![8_u8; 32],
            ChunkType::Data,
        ));
        storage.sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(PartitionChunkOffset::from(1)));
        assert_eq!(fs::read(&path)?, before);
        assert_eq!(
            storage.get_chunk_type(&PartitionChunkOffset::from(2)),
            Some(ChunkType::Uninitialized)
        );

        storage.suppress_interval_flush_for_test();
        drop(storage);

        let reloaded = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: None,
                submodules: vec![(partition_chunk_offset_ii!(0, 10), "sub".into())],
            },
            &config,
        )?;
        assert_eq!(
            reloaded.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Uninitialized)
        );
        assert_eq!(
            reloaded.get_chunk_type(&PartitionChunkOffset::from(1)),
            Some(ChunkType::Uninitialized)
        );
        assert!(reloaded.get_intervals(ChunkType::Data).is_empty());
        drop(reloaded);
        drop(tmp);
        Ok(())
    }

    #[test]
    fn clean_shutdown_persists_completions_and_keeps_entropy() -> eyre::Result<()> {
        let (tmp, config, storage) = interval_batch_module("intervals_batch_shutdown", 30)?;
        {
            let mut intervals = storage.intervals.write().unwrap();
            *intervals = StorageIntervals::new();
            intervals
                .insert_merge_touching_if_values_equal(
                    partition_chunk_offset_ii!(0, 10),
                    ChunkType::Entropy,
                )
                .expect("entropy span");
        }
        storage.write_intervals_to_submodules()?;
        assert!(storage.write_chunk(
            PartitionChunkOffset::from(0),
            vec![7_u8; 32],
            ChunkType::Data,
        ));
        storage.sync_pending_chunks()?;
        assert_eq!(
            storage.get_chunk_type(&PartitionChunkOffset::from(1)),
            Some(ChunkType::Entropy)
        );
        let file = file_intervals(&storage)?;
        assert_eq!(
            file.get_at_point(PartitionChunkOffset::from(0)).copied(),
            Some(ChunkType::Interrupted)
        );
        assert_eq!(
            file.get_at_point(PartitionChunkOffset::from(1)).copied(),
            Some(ChunkType::Interrupted)
        );
        drop(storage);

        let reloaded = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: None,
                submodules: vec![(partition_chunk_offset_ii!(0, 10), "sub".into())],
            },
            &config,
        )?;
        assert_eq!(
            reloaded.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Data)
        );
        assert_eq!(
            reloaded.get_chunk_type(&PartitionChunkOffset::from(1)),
            Some(ChunkType::Entropy)
        );
        drop(reloaded);
        drop(tmp);
        Ok(())
    }

    #[test]
    fn write_ahead_stops_before_durable_data() -> eyre::Result<()> {
        let (_tmp, _config, storage) = interval_batch_module("intervals_stop_at_data", 30)?;
        assert!(storage.write_chunk(
            PartitionChunkOffset::from(5),
            vec![5_u8; 32],
            ChunkType::Data,
        ));
        storage.sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(PartitionChunkOffset::from(5)));
        assert!(storage.write_chunk(
            PartitionChunkOffset::from(0),
            vec![1_u8; 32],
            ChunkType::Data,
        ));
        storage.sync_pending_chunks()?;
        let file = file_intervals(&storage)?;
        assert_eq!(
            file.get_at_point(PartitionChunkOffset::from(0)).copied(),
            Some(ChunkType::Interrupted)
        );
        assert_eq!(
            file.get_at_point(PartitionChunkOffset::from(4)).copied(),
            Some(ChunkType::Interrupted)
        );
        assert_eq!(
            file.get_at_point(PartitionChunkOffset::from(5)).copied(),
            Some(ChunkType::Data)
        );
        assert_eq!(
            storage.get_chunk_type(&PartitionChunkOffset::from(6)),
            Some(ChunkType::Uninitialized)
        );
        assert_eq!(
            file.get_at_point(PartitionChunkOffset::from(6)).copied(),
            Some(ChunkType::Interrupted)
        );
        Ok(())
    }

    #[test]
    fn failed_fsync_does_not_publish_data_intervals() -> eyre::Result<()> {
        let (_tmp, _config, storage) = interval_batch_module("intervals_before_fsync", 30)?;
        storage.fail_next_sync_at(SyncFailurePoint::BeforeDataFsync);
        assert!(storage.write_chunk(
            PartitionChunkOffset::from(0),
            vec![7_u8; 32],
            ChunkType::Data,
        ));
        assert!(storage.force_sync_pending_chunks().is_err());
        assert!(storage.has_pending_writes());
        assert!(!storage.is_data_chunk_durable_at(PartitionChunkOffset::from(0)));
        let file = file_intervals(&storage)?;
        assert_eq!(
            file.get_at_point(PartitionChunkOffset::from(0)).copied(),
            Some(ChunkType::Interrupted)
        );
        assert!(
            file.iter()
                .all(|(_, chunk_type)| *chunk_type != ChunkType::Data)
        );
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
                min_durable_interval_ms: 0,
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
                min_durable_interval_ms: 0,
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

    /// A failed `fdatasync` can drop the dirty pages. The next point must
    /// pwrite parked bytes again before it commits presence and the checksum.
    #[test]
    fn failed_fsync_of_parked_bytes_rewrites_them_before_commit() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("parked_fsync_retry")
            .with_tracing()
            .build();
        let chunk_size = 32_u64;
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: 4,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 0,
                intervals_persist_secs: 0,
                min_durable_interval_ms: 60_000,
                ..StorageSyncConfig::default()
            },
            base_directory: tmp_dir.path().to_path_buf(),
            database: index_database(SubmoduleIndexEngine::Rocksdb),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(PartitionAssignment::default()),
                submodules: vec![(partition_chunk_offset_ii!(0, 3), "chunks".into())],
            },
            &config,
        )?;

        let parked_offset = PartitionChunkOffset::from(1);
        let fresh_offset = PartitionChunkOffset::from(2);
        let parked_bytes = vec![0xA1_u8; chunk_size as usize];
        let fresh_bytes = vec![0xB2_u8; chunk_size as usize];

        // The first point is never deferred. It starts the interval.
        assert!(storage.write_chunk(
            PartitionChunkOffset::from(0),
            vec![1_u8; chunk_size as usize],
            ChunkType::Data,
        ));
        storage.sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(PartitionChunkOffset::from(0)));

        assert!(storage.write_chunk(parked_offset, parked_bytes.clone(), ChunkType::Data));
        storage.sync_pending_chunks()?;
        assert!(
            !storage.is_data_chunk_durable_at(parked_offset),
            "a point inside the interval must park"
        );
        {
            let pending = storage.pending_writes.read().unwrap();
            assert!(pending.written_ahead.contains_key(&parked_offset));
        }

        assert!(storage.write_chunk(fresh_offset, fresh_bytes.clone(), ChunkType::Data));
        storage.fail_next_sync_at(SyncFailurePoint::BeforeDataFsync);
        let failures_before = super::super::index_read_metrics::chunk_fsync_failures_for_test();
        let err = storage.force_sync_pending_chunks().unwrap_err();
        assert!(err.to_string().contains("injected"), "{err}");
        assert!(
            super::super::index_read_metrics::chunk_fsync_failures_for_test() > failures_before
        );
        {
            let pending = storage.pending_writes.read().unwrap();
            assert!(
                pending.chunks.contains_key(&parked_offset),
                "a failed fsync must queue parked bytes for another pwrite"
            );
            assert!(!pending.written_ahead.contains_key(&parked_offset));
            assert!(
                pending.chunks.contains_key(&fresh_offset),
                "bytes pwritten in this call stay in pending.chunks"
            );
        }

        let (_, submodule) = storage.get_submodule_for_offset(parked_offset)?;
        // `path` is the relative submodule directory. The file lives beside
        // `intervals.json`, under the node base directory.
        let path = submodule
            .intervals_file
            .lock()
            .expect("intervals path")
            .with_file_name("chunks.dat");
        {
            use std::os::unix::fs::FileExt as _;
            let file = OpenOptions::new().write(true).open(&path)?;
            let zeros = vec![0_u8; chunk_size as usize];
            file.write_all_at(&zeros, u64::from(parked_offset.0) * chunk_size)?;
            file.write_all_at(&zeros, u64::from(fresh_offset.0) * chunk_size)?;
        }

        storage.force_sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(parked_offset));
        assert!(storage.is_data_chunk_durable_at(fresh_offset));
        let loaded = storage.read_chunks(partition_chunk_offset_ii!(1, 2))?;
        assert_eq!(loaded.get(&parked_offset).unwrap().0, parked_bytes);
        assert_eq!(loaded.get(&fresh_offset).unwrap().0, fresh_bytes);
        let parked_row = storage
            .offset_meta_for_test(parked_offset)?
            .expect("durable data stores a checksum");
        assert_eq!(parked_row.crc, Some(chunk_crc::checksum(&parked_bytes)));
        let fresh_row = storage
            .offset_meta_for_test(fresh_offset)?
            .expect("fresh checksum");
        assert_eq!(fresh_row.crc, Some(chunk_crc::checksum(&fresh_bytes)));
        Ok(())
    }

    fn recall_flush_fixture(
        prefix: &str,
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
                min_durable_interval_ms: 0,
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

    /// 256 KiB is the production chunk size and a multiple of the 4096-byte
    /// `O_DIRECT` alignment.
    const RECALL_DIRECT_CHUNK: u64 = 256 * 1024;

    /// Clears an injected `EINVAL` from `open_recall_direct` when the test ends.
    struct RecallDirectOpenGuard;

    impl RecallDirectOpenGuard {
        fn arm() -> Self {
            recall_direct::force_recall_direct_open_einval(true);
            Self
        }
    }

    impl Drop for RecallDirectOpenGuard {
        fn drop(&mut self) {
            recall_direct::force_recall_direct_open_einval(false);
        }
    }

    fn open_recall_io_module(
        prefix: &str,
        engine: SubmoduleIndexEngine,
        direct: bool,
        chunk_size: u64,
        min_durable_interval_ms: u64,
    ) -> eyre::Result<(irys_testing_utils::utils::tempfile::TempDir, StorageModule)> {
        let tmp = TempDirBuilder::new().prefix(prefix).build();
        let chunks = 2_u64;
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
                intervals_persist_secs: 0,
                min_durable_interval_ms,
                recall_direct_io: direct,
                ..StorageSyncConfig::default()
            },
            base_directory: tmp.path().to_path_buf(),
            database: index_database(engine),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: None,
                submodules: vec![(partition_chunk_offset_ii!(0, chunks - 1), "hdd0".into())],
            },
            &config,
        )?;
        Ok((tmp, storage))
    }

    fn for_each_recall_io(
        prefix: &str,
        direct: bool,
        chunk_size: u64,
        min_durable_interval_ms: u64,
        mut f: impl FnMut(&StorageModule) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        for engine in [SubmoduleIndexEngine::Mdbx, SubmoduleIndexEngine::Rocksdb] {
            let (tmp, storage) = open_recall_io_module(
                &format!("{prefix}_{engine:?}"),
                engine,
                direct,
                chunk_size,
                min_durable_interval_ms,
            )?;
            f(&storage).map_err(|err| eyre!("{engine:?}: {err}"))?;
            drop(storage);
            drop(tmp);
        }
        Ok(())
    }

    fn chunk_fill(byte: u8, chunk_size: u64) -> Vec<u8> {
        vec![byte; chunk_size as usize]
    }

    fn write_synced(storage: &StorageModule, offset: u32, bytes: Vec<u8>) -> eyre::Result<()> {
        assert!(storage.write_chunk(PartitionChunkOffset::from(offset), bytes, ChunkType::Data));
        storage.force_sync_pending_chunks()?;
        Ok(())
    }

    fn chunk_bytes(
        map: &BTreeMap<PartitionChunkOffset, (ChunkBytes, ChunkType)>,
        offset: u32,
    ) -> eyre::Result<Vec<u8>> {
        map.get(&PartitionChunkOffset::from(offset))
            .map(|(bytes, _)| bytes.clone())
            .ok_or_else(|| eyre!("offset {offset} was omitted"))
    }

    fn served_chunk(storage: &StorageModule, offset: u32) -> eyre::Result<Vec<u8>> {
        let map = storage.read_chunks(partition_chunk_offset_ii!(offset, offset))?;
        chunk_bytes(&map, offset)
    }

    fn recalled_chunk(storage: &StorageModule, offset: u32) -> eyre::Result<Vec<u8>> {
        let recalled = storage.read_recall_chunks(partition_chunk_offset_ii!(offset, offset))?;
        chunk_bytes(&recalled.chunks, offset)
    }

    /// `Ok(true)` when this filesystem rejected `O_DIRECT`. The caller skips.
    fn skip_without_direct(storage: &StorageModule, test: &str) -> eyre::Result<bool> {
        let (_, submodule) = storage.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        let Some(file) = submodule.recall_direct.as_ref() else {
            eprintln!(
                "skip {test}: this filesystem rejected O_DIRECT; mining recall stays on the buffered descriptor"
            );
            return Ok(true);
        };
        assert!(
            recall_direct::fd_is_direct(file),
            "recall descriptor is not O_DIRECT"
        );
        assert_eq!(storage.recall_io_mode(), recall_direct::MODE_DIRECT);
        Ok(false)
    }

    #[test]
    fn recall_direct_io_reads_the_same_bytes_as_buffered() -> eyre::Result<()> {
        for_each_recall_io(
            "recall_direct_same",
            true,
            RECALL_DIRECT_CHUNK,
            0,
            |storage| {
                if skip_without_direct(
                    storage,
                    "recall_direct_io_reads_the_same_bytes_as_buffered",
                )? {
                    return Ok(());
                }
                let synced = chunk_fill(0x11, RECALL_DIRECT_CHUNK);
                write_synced(storage, 0, synced.clone())?;
                assert_eq!(served_chunk(storage, 0)?, synced);
                assert_eq!(storage.recall_io_counts_for_test(), (0, 0, 0));
                assert_eq!(recalled_chunk(storage, 0)?, synced);
                assert_eq!(storage.recall_io_counts_for_test(), (1, 0, 0));

                // Offset 1 is still in `pending.chunks`. Recall overlays it and
                // does not pread it. Offset 0 is the one direct pread.
                let pending = chunk_fill(0x22, RECALL_DIRECT_CHUNK);
                assert!(storage.write_chunk(
                    PartitionChunkOffset::from(1),
                    pending.clone(),
                    ChunkType::Data,
                ));
                let recalled = storage.read_recall_chunks(partition_chunk_offset_ii!(0, 1))?;
                assert_eq!(chunk_bytes(&recalled.chunks, 0)?, synced);
                assert_eq!(chunk_bytes(&recalled.chunks, 1)?, pending);
                assert_eq!(storage.recall_io_counts_for_test(), (2, 0, 0));
                Ok(())
            },
        )
    }

    /// A buffered `pwrite` that has not been `fdatasync`'d still has dirty pages.
    /// Linux writes that range back before the `O_DIRECT` read (XFS and ext4
    /// iomap), so mining recall returns the new bytes. `written_ahead` is not
    /// overlaid from memory: the bytes come from the direct read. The checksum
    /// is not stored until the durable point, and an offset still in
    /// `written_ahead` is not checked.
    #[test]
    fn recall_direct_io_sees_dirty_buffered_pages() -> eyre::Result<()> {
        for_each_recall_io(
            "recall_direct_dirty",
            true,
            RECALL_DIRECT_CHUNK,
            60_000,
            |storage| {
                if skip_without_direct(storage, "recall_direct_io_sees_dirty_buffered_pages")? {
                    return Ok(());
                }
                // The first point is immediate. It arms the interval so the
                // next `pwrite` can be parked.
                write_synced(storage, 0, chunk_fill(0x11, RECALL_DIRECT_CHUNK))?;
                let fresh = chunk_fill(0x22, RECALL_DIRECT_CHUNK);
                assert!(storage.write_chunk(
                    PartitionChunkOffset::from(1),
                    fresh.clone(),
                    ChunkType::Data,
                ));
                // `pwrite` returns here. `fdatasync` waits out the interval.
                storage.sync_pending_chunks()?;
                {
                    let pending = storage.pending_writes.read().unwrap();
                    let offset = PartitionChunkOffset::from(1);
                    assert!(
                        pending.written_ahead.contains_key(&offset),
                        "the new bytes must be pwritten and not yet fdatasync'd"
                    );
                    assert!(!pending.contains_key(&offset));
                }
                assert!(!storage.is_data_chunk_durable_at(PartitionChunkOffset::from(1)));
                let before = storage.recall_io_counts_for_test();
                assert_eq!(recalled_chunk(storage, 1)?, fresh);
                assert_eq!(
                    storage.recall_io_counts_for_test(),
                    (before.0 + 1, before.1, before.2)
                );
                Ok(())
            },
        )
    }

    #[test]
    fn recall_direct_io_falls_back_when_open_fails() -> eyre::Result<()> {
        let _guard = RecallDirectOpenGuard::arm();
        for_each_recall_io(
            "recall_direct_einval",
            true,
            RECALL_DIRECT_CHUNK,
            0,
            |storage| {
                let (_, submodule) =
                    storage.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
                assert!(submodule.recall_direct.is_none());
                assert_eq!(storage.recall_io_mode(), recall_direct::MODE_FALLBACK);
                let bytes = chunk_fill(0x33, RECALL_DIRECT_CHUNK);
                write_synced(storage, 0, bytes.clone())?;
                assert_eq!(served_chunk(storage, 0)?, bytes);
                assert_eq!(storage.recall_io_counts_for_test(), (0, 0, 0));
                assert_eq!(recalled_chunk(storage, 0)?, bytes);
                assert_eq!(storage.recall_io_counts_for_test(), (0, 0, 1));
                Ok(())
            },
        )
    }

    #[test]
    fn recall_direct_io_off_matches_buffered_recall() -> eyre::Result<()> {
        for_each_recall_io(
            "recall_direct_off",
            false,
            RECALL_DIRECT_CHUNK,
            0,
            |storage| {
                let (_, submodule) =
                    storage.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
                assert!(submodule.recall_direct.is_none());
                assert_eq!(storage.recall_io_mode(), recall_direct::MODE_BUFFERED);
                let bytes = chunk_fill(0x44, RECALL_DIRECT_CHUNK);
                write_synced(storage, 0, bytes.clone())?;
                assert_eq!(served_chunk(storage, 0)?, bytes);
                assert_eq!(storage.recall_io_counts_for_test(), (0, 0, 0));
                assert_eq!(recalled_chunk(storage, 0)?, bytes);
                assert_eq!(storage.recall_io_counts_for_test(), (0, 1, 0));
                Ok(())
            },
        )
    }

    #[test]
    fn recall_direct_io_unaligned_range_falls_back() -> eyre::Result<()> {
        // 32 is not a multiple of 4096. The kernel may still accept the read.
        // Userspace falls back before the syscall.
        let chunk_size = 32;
        for_each_recall_io("recall_direct_unaligned", true, chunk_size, 0, |storage| {
            let bytes = chunk_fill(0x55, chunk_size);
            write_synced(storage, 0, bytes.clone())?;
            assert_eq!(served_chunk(storage, 0)?, bytes);
            assert_eq!(storage.recall_io_counts_for_test(), (0, 0, 0));
            assert_eq!(recalled_chunk(storage, 0)?, bytes);
            assert_eq!(storage.recall_io_counts_for_test(), (0, 0, 1));
            Ok(())
        })
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
        let (_tmp, storage_module) = recall_flush_fixture("recall_flush_window")?;
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
        storage.disk.release_for_test();
        assert!(storage.pending_run_ready());
        assert_eq!(storage.longest_ready_write_bytes(), 32);
        Ok(())
    }

    #[test]
    fn busy_disk_writes_every_aged_short_run() -> eyre::Result<()> {
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
        storage.write_chunk(
            PartitionChunkOffset::from(4),
            vec![3_u8; 32],
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
        let offsets = {
            let pending = storage.pending_writes.read().unwrap();
            let batch = storage.select_coalesced_window(&pending, 8, 512, 32 * 4, true);
            batch
                .iter()
                .map(|(offset, _, _)| offset.0)
                .collect::<Vec<_>>()
        };
        assert_eq!(offsets, vec![0, 2]);
        storage.disk.release_for_test();
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
                // These tests read intervals.json as soon as sync returns.
                intervals_persist_secs: 0,
                min_durable_interval_ms: 0,
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
        let chunk_size = 32_u64;
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
        let low_offset = storage.select_coalesced_window(&pending, 1, 512, cap, false);
        let low_offsets: Vec<u32> = low_offset.iter().map(|(offset, _, _)| offset.0).collect();
        assert_eq!(low_offsets, vec![0, 1, 2, 3]);
        drop(pending);

        let aged_at = Instant::now()
            .checked_sub(Duration::from_secs(2))
            .expect("test clock");
        {
            let mut pending = storage.pending_writes.write().unwrap();
            pending
                .queued_at
                .insert(PartitionChunkOffset::from(50), aged_at);
            let stayed = storage.select_coalesced_window(&pending, 1, 512, cap, false);
            let stayed_offsets: Vec<u32> = stayed.iter().map(|(offset, _, _)| offset.0).collect();
            assert_eq!(stayed_offsets, vec![0, 1, 2, 3]);
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
    fn aged_short_runs_join_the_full_runs() -> eyre::Result<()> {
        let chunk_size = 32_u64;
        let (_tmp, storage) = coalesce_fixture("aged_short_runs", chunk_size)?;
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
        storage.write_chunk(PartitionChunkOffset::from(30), body(30), ChunkType::Data);
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
        assert_eq!(offsets, vec![0, 1, 2, 3, 10, 20]);

        let released = storage.select_coalesced_window(&pending, 8, 512, cap, false);
        let released_offsets: Vec<u32> = released.iter().map(|(offset, _, _)| offset.0).collect();
        assert_eq!(released_offsets, vec![0, 1, 2, 3, 10, 20, 30]);
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
                min_durable_interval_ms: 0,
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
                min_durable_interval_ms: 0,
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

    /// One submodule, one chunk per sync, completion files rewritten immediately.
    fn durable_spacing_module(
        prefix: &str,
        min_durable_interval_ms: u64,
        max_pending_write_bytes: Option<u64>,
        durable_headroom_permille: u64,
    ) -> eyre::Result<(irys_testing_utils::tempfile::TempDir, StorageModule)> {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let chunk_size = 32;
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: 16,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
                max_pending_write_bytes,
                entropy_sweep_interval_millis: 60_000,
                entropy_sweep_max_bytes: chunk_size,
                intervals_persist_secs: 0,
                min_durable_interval_ms,
                durable_headroom_permille,
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
                submodules: vec![(partition_chunk_offset_ii!(0, 15), "chunks".into())],
            },
            &config,
        )?;
        Ok((tmp_dir, storage))
    }

    fn write_spacing_chunk(storage: &StorageModule, offset: u32, byte: u8) {
        assert!(storage.write_chunk(
            PartitionChunkOffset::from(offset),
            vec![byte; 32],
            ChunkType::Data,
        ));
    }

    /// Sleep on the monotonic clock until `start` is at least `interval` old.
    fn wait_monotonic(start: Instant, interval: Duration) {
        let deadline = start + interval;
        while Instant::now() < deadline {
            std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
        }
    }

    #[test]
    fn zero_durable_interval_syncs_every_batch() -> eyre::Result<()> {
        let (_tmp, storage) = durable_spacing_module(
            "durable_interval_zero",
            0,
            None,
            irys_types::DEFAULT_DURABLE_HEADROOM_PERMILLE,
        )?;
        write_spacing_chunk(&storage, 0, 1);
        storage.sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(PartitionChunkOffset::from(0)));
        assert!(!storage.has_pending_writes());
        write_spacing_chunk(&storage, 1, 2);
        storage.sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(PartitionChunkOffset::from(1)));
        assert!(!storage.has_pending_writes());
        let written = read_intervals_file(&submodule_interval_path(&storage, 0))?;
        assert_eq!(
            written.get_at_point(PartitionChunkOffset::from(1)).copied(),
            Some(ChunkType::Data)
        );
        let counts = storage.durable_point_counts();
        assert_eq!(counts.immediate_interval, 2);
        assert_eq!(counts.deferred_interval, 0);
        assert_eq!(counts.immediate(), 2);
        Ok(())
    }

    #[test]
    fn durable_points_stay_at_least_the_interval_apart() -> eyre::Result<()> {
        let interval = Duration::from_millis(1_000);
        let (_tmp, storage) = durable_spacing_module(
            "durable_interval_space",
            1_000,
            None,
            irys_types::DEFAULT_DURABLE_HEADROOM_PERMILLE,
        )?;
        write_spacing_chunk(&storage, 0, 1);
        storage.sync_pending_chunks()?;
        let stamped = Instant::now();
        assert!(storage.is_data_chunk_durable_at(PartitionChunkOffset::from(0)));

        write_spacing_chunk(&storage, 1, 2);
        storage.sync_pending_chunks()?;
        let deferred = PartitionChunkOffset::from(1);
        assert!(!storage.is_data_chunk_durable_at(deferred));
        assert!(storage.pending_write_bytes() > 0);
        assert_eq!(storage.durable_point_counts().immediate_interval, 1);
        assert_eq!(storage.durable_point_counts().deferred_interval, 0);
        assert_eq!(storage.read_chunk_internal(deferred)?, vec![2_u8; 32]);
        let intervals = read_intervals_file(&submodule_interval_path(&storage, 0))?;
        assert_eq!(
            intervals.get_at_point(deferred).copied(),
            Some(ChunkType::Interrupted)
        );

        wait_monotonic(stamped, interval);
        storage.sync_pending_chunks()?;
        assert!(stamped.elapsed() >= interval);
        assert!(storage.is_data_chunk_durable_at(deferred));
        assert!(!storage.has_pending_writes());
        let completed = read_intervals_file(&submodule_interval_path(&storage, 0))?;
        assert_eq!(
            completed.get_at_point(deferred).copied(),
            Some(ChunkType::Data)
        );
        let counts = storage.durable_point_counts();
        assert_eq!(counts.immediate_interval, 1);
        assert_eq!(counts.deferred_interval, 1);
        assert_eq!(counts.immediate_headroom, 0);
        Ok(())
    }

    #[test]
    fn force_sync_bypasses_the_durable_interval() -> eyre::Result<()> {
        let (_tmp, storage) = durable_spacing_module(
            "durable_interval_force",
            60_000,
            None,
            irys_types::DEFAULT_DURABLE_HEADROOM_PERMILLE,
        )?;
        write_spacing_chunk(&storage, 0, 1);
        storage.sync_pending_chunks()?;
        write_spacing_chunk(&storage, 1, 2);
        storage.sync_pending_chunks()?;
        let deferred = PartitionChunkOffset::from(1);
        assert!(!storage.is_data_chunk_durable_at(deferred));
        let started = Instant::now();
        storage.force_sync_pending_chunks()?;
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "force sync waited out the durable interval"
        );
        assert!(storage.is_data_chunk_durable_at(deferred));
        assert!(!storage.has_pending_writes());
        let counts = storage.durable_point_counts();
        assert_eq!(counts.immediate_forced, 1);
        assert_eq!(counts.deferred_interval, 0);
        Ok(())
    }

    #[test]
    fn shutdown_bypasses_the_durable_interval() -> eyre::Result<()> {
        let (_tmp, storage) = durable_spacing_module(
            "durable_interval_stop",
            60_000,
            None,
            irys_types::DEFAULT_DURABLE_HEADROOM_PERMILLE,
        )?;
        write_spacing_chunk(&storage, 0, 1);
        storage.sync_pending_chunks()?;
        write_spacing_chunk(&storage, 1, 2);
        storage.sync_pending_chunks()?;
        let storage = Arc::new(storage);
        let deferred = PartitionChunkOffset::from(1);
        {
            let lane = LaneGuard::start(Arc::clone(&storage))?;
            std::thread::sleep(Duration::from_millis(100));
            assert!(!lane.module.is_data_chunk_durable_at(deferred));
        }
        assert!(storage.is_data_chunk_durable_at(deferred));
        assert!(!storage.has_pending_writes());
        Ok(())
    }

    #[test]
    fn drop_bypasses_the_durable_interval() -> eyre::Result<()> {
        let (_tmp, storage) = durable_spacing_module(
            "durable_interval_drop",
            60_000,
            None,
            irys_types::DEFAULT_DURABLE_HEADROOM_PERMILLE,
        )?;
        write_spacing_chunk(&storage, 0, 1);
        storage.sync_pending_chunks()?;
        write_spacing_chunk(&storage, 1, 2);
        storage.sync_pending_chunks()?;
        let deferred = PartitionChunkOffset::from(1);
        assert!(!storage.is_data_chunk_durable_at(deferred));
        let path = submodule_interval_path(&storage, 0);
        let started = Instant::now();
        drop(storage);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "drop waited out the durable interval"
        );
        let written = read_intervals_file(&path)?;
        assert_eq!(
            written.get_at_point(deferred).copied(),
            Some(ChunkType::Data)
        );
        Ok(())
    }

    #[test]
    fn reset_discards_a_deferred_durable_point() -> eyre::Result<()> {
        let (_tmp, storage) = durable_spacing_module(
            "durable_interval_reset",
            60_000,
            None,
            irys_types::DEFAULT_DURABLE_HEADROOM_PERMILLE,
        )?;
        write_spacing_chunk(&storage, 0, 1);
        storage.sync_pending_chunks()?;
        write_spacing_chunk(&storage, 1, 2);
        storage.sync_pending_chunks()?;
        let deferred = PartitionChunkOffset::from(1);
        assert!(!storage.is_data_chunk_durable_at(deferred));
        assert!(storage.pending_write_bytes() > 0);
        let started = Instant::now();
        storage.reset()?;
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "reset waited out the durable interval"
        );
        assert!(!storage.has_pending_writes());
        assert!(!storage.is_data_chunk_durable_at(deferred));
        assert_eq!(
            storage.get_chunk_type(&deferred),
            Some(ChunkType::Uninitialized)
        );
        Ok(())
    }

    #[test]
    fn pending_byte_ceiling_bypasses_the_durable_interval() -> eyre::Result<()> {
        // One chunk is the ceiling (`num_writes_before_sync` × chunk size).
        // Permille 1000 puts the headroom line on that ceiling, so this test
        // still trips the strict `>` ceiling bypass and not the headroom line.
        // One parked chunk stays deferred. The next chunk pushes the total over.
        let (_tmp, storage) =
            durable_spacing_module("durable_interval_ceiling", 60_000, Some(32), 1000)?;
        write_spacing_chunk(&storage, 0, 1);
        storage.sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(PartitionChunkOffset::from(0)));

        write_spacing_chunk(&storage, 1, 2);
        storage.sync_pending_chunks()?;
        assert!(!storage.is_data_chunk_durable_at(PartitionChunkOffset::from(1)));
        assert_eq!(storage.pending_write_bytes(), 32);

        write_spacing_chunk(&storage, 2, 3);
        let started = Instant::now();
        storage.sync_pending_chunks()?;
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the pending-byte ceiling waited out the durable interval"
        );
        assert!(storage.is_data_chunk_durable_at(PartitionChunkOffset::from(1)));
        assert!(storage.is_data_chunk_durable_at(PartitionChunkOffset::from(2)));
        assert!(!storage.has_pending_writes());
        assert!(storage.durable_point_counts().immediate_ceiling >= 1);
        assert_eq!(storage.durable_point_counts().deferred_interval, 0);
        Ok(())
    }

    #[test]
    fn headroom_loss_runs_the_durable_point_immediately() -> eyre::Result<()> {
        // Ceiling is max(2 × num_writes_before_sync, 256) × chunk size = 8192.
        // Permille 4 is 32 bytes. One parked chunk equals the line and stays
        // deferred. The next chunk crosses it and must not wait out 60 s.
        let (_tmp, storage) = durable_spacing_module("durable_interval_headroom", 60_000, None, 4)?;
        write_spacing_chunk(&storage, 0, 1);
        storage.sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(PartitionChunkOffset::from(0)));

        write_spacing_chunk(&storage, 1, 2);
        storage.sync_pending_chunks()?;
        assert!(!storage.is_data_chunk_durable_at(PartitionChunkOffset::from(1)));
        assert_eq!(storage.pending_write_bytes(), 32);

        write_spacing_chunk(&storage, 2, 3);
        let started = Instant::now();
        storage.sync_pending_chunks()?;
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "headroom loss waited out the durable interval"
        );
        assert!(storage.is_data_chunk_durable_at(PartitionChunkOffset::from(1)));
        assert!(storage.is_data_chunk_durable_at(PartitionChunkOffset::from(2)));
        assert!(!storage.has_pending_writes());
        let counts = storage.durable_point_counts();
        assert_eq!(counts.immediate_interval, 1);
        assert!(counts.immediate_headroom >= 1);
        assert_eq!(counts.deferred_interval, 0);
        Ok(())
    }

    #[test]
    fn recall_does_not_wait_on_a_deferred_durable_point() -> eyre::Result<()> {
        let (_tmp, storage) = durable_spacing_module(
            "durable_interval_recall",
            60_000,
            None,
            irys_types::DEFAULT_DURABLE_HEADROOM_PERMILLE,
        )?;
        write_spacing_chunk(&storage, 0, 1);
        storage.sync_pending_chunks()?;
        write_spacing_chunk(&storage, 1, 2);
        let storage = Arc::new(storage);
        let waiter = Arc::clone(&storage);
        storage.disk.arm_recall_flush();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _hold = waiter.disk.begin_recall();
            let _ = tx.send(());
        });
        let lane = LaneGuard::start(Arc::clone(&storage))?;
        rx.recv_timeout(Duration::from_secs(5))
            .expect("recall waited on a deferred durable point");
        let deferred = PartitionChunkOffset::from(1);
        assert!(!lane.module.is_data_chunk_durable_at(deferred));
        assert_eq!(lane.module.read_chunk_internal(deferred)?, vec![2_u8; 32]);
        Ok(())
    }

    fn defer_query() -> DurableDeferQuery {
        DurableDeferQuery {
            force: false,
            interval: Duration::from_millis(250),
            since_last: Some(Duration::from_millis(1)),
            pending_bytes: 0,
            ceiling_bytes: 1_000,
            headroom_bytes: 400,
            headroom_open: true,
            recall_flush_owed: false,
        }
    }

    #[test]
    fn durable_point_defers_while_pending_bytes_stay_inside_the_line() {
        let mut query = defer_query();
        query.pending_bytes = 400;
        let decision = durable_point_decision(query);
        assert_eq!(decision.cause, None);
        assert!(decision.headroom_open);

        query.pending_bytes = 300;
        let inside = durable_point_decision(query);
        assert_eq!(inside.cause, None);
        assert!(inside.headroom_open);

        query.since_last = Some(Duration::from_millis(249));
        query.pending_bytes = 0;
        let young = durable_point_decision(query);
        assert_eq!(young.cause, None);
        assert!(young.headroom_open);
    }

    #[test]
    fn durable_point_runs_when_the_headroom_signal_trips() {
        let mut query = defer_query();
        query.pending_bytes = 401;
        let crossed = durable_point_decision(query);
        assert_eq!(crossed.cause, Some(DurablePointCause::Headroom));
        assert!(!crossed.headroom_open);

        // An owed recall flush does not fdatasync only because the line broke.
        // The latch stays closed for the next call.
        query.recall_flush_owed = true;
        let owed = durable_point_decision(query);
        assert_eq!(owed.cause, None);
        assert!(!owed.headroom_open);

        query.recall_flush_owed = false;
        query.headroom_open = false;
        query.pending_bytes = 201;
        let held = durable_point_decision(query);
        assert_eq!(held.cause, Some(DurablePointCause::Headroom));
        assert!(!held.headroom_open);

        query.pending_bytes = 200;
        let reopened = durable_point_decision(query);
        assert_eq!(reopened.cause, None);
        assert!(reopened.headroom_open);

        query.pending_bytes = 0;
        query.headroom_open = false;
        let cleared = durable_point_decision(query);
        assert_eq!(cleared.cause, None);
        assert!(cleared.headroom_open);
    }

    #[test]
    fn durable_point_never_waits_past_the_interval() {
        let mut query = defer_query();
        query.since_last = Some(Duration::from_millis(250));
        query.pending_bytes = 0;
        let elapsed = durable_point_decision(query);
        assert_eq!(elapsed.cause, Some(DurablePointCause::Interval));
        assert!(elapsed.headroom_open);

        // A completed wait counts as the interval even when the latch is closed.
        query.pending_bytes = 401;
        query.headroom_open = false;
        let both = durable_point_decision(query);
        assert_eq!(both.cause, Some(DurablePointCause::Interval));
        assert!(!both.headroom_open);

        query.interval = Duration::ZERO;
        query.since_last = Some(Duration::from_millis(1));
        query.pending_bytes = 0;
        query.headroom_open = true;
        let off = durable_point_decision(query);
        assert_eq!(off.cause, Some(DurablePointCause::Interval));

        query.interval = Duration::from_millis(250);
        query.since_last = None;
        query.pending_bytes = 5_000;
        let first = durable_point_decision(query);
        assert_eq!(first.cause, Some(DurablePointCause::Interval));
        assert!(!first.headroom_open);
    }

    #[test]
    fn durable_point_bypasses_are_unchanged() {
        let mut query = defer_query();
        query.force = true;
        query.pending_bytes = 0;
        let forced = durable_point_decision(query);
        assert_eq!(forced.cause, Some(DurablePointCause::Forced));

        query.force = false;
        query.pending_bytes = 1_001;
        query.recall_flush_owed = true;
        let ceiling = durable_point_decision(query);
        assert_eq!(ceiling.cause, Some(DurablePointCause::Ceiling));
        assert!(!ceiling.headroom_open);

        // Pending bytes equal to the ceiling still defer. The bypass is strict `>`.
        query.pending_bytes = 1_000;
        query.ceiling_bytes = 1_000;
        query.headroom_bytes = 1_000;
        query.headroom_open = true;
        query.recall_flush_owed = false;
        query.since_last = Some(Duration::from_millis(1));
        let equal = durable_point_decision(query);
        assert_eq!(equal.cause, None);
        assert!(equal.headroom_open);
    }
}
