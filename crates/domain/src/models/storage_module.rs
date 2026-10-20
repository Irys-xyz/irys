//! One storage module owns one partition. The node addresses partitions by
//! hash. The module maps partition chunk offsets onto submodules. Each
//! submodule is one directory and is not visible outside the module.
//! A typical disk is one submodule per module. Several smaller drives can
//! share one module.

use std::os::unix::fs::FileExt as _;

use atomic_write_file::AtomicWriteFile;
use derive_more::derive::{Deref, DerefMut};
use eyre::{Context as _, OptionExt as _, Result, ensure, eyre};
#[cfg(feature = "bench")]
use irys_database::submodule::RocksTuning;
use irys_database::submodule::db::placement_overlaps_inclusive_span;
#[cfg(test)]
use irys_database::submodule::tables::{SUBMODULE_SCHEMA, SUBMODULE_SCHEMA_ROCKS_V1};
use irys_database::submodule::{
    PackingRecord, SubmoduleIndex, SubmoduleRead, SubmoduleStore as _,
    install_process_rocks_resources, packing_params_valid_for_module,
    tables::{
        DataRootInfo, DataRootInfos, OffsetMeta, PackedOffsetRow, PendingBodyMigration, Placement,
        TxLeafBinding,
    },
};
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
#[cfg(test)]
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
        Arc, Mutex, Once, RwLock, RwLockReadGuard,
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
            // No placement: missing path bytes are skipped. A data root with
            // no info is an error.
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
        // MDBX-SUBMODULE-INDEX: delete the else. MDBX has no checksum on the offset.
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
            // MDBX-SUBMODULE-INDEX: delete the else. It is the MDBX proof read. RocksDB reads the proof from the offset row.
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
                // `u32::MAX` chunks does not fit `TxChunkOffset`. Skip it
                // instead of failing the span.
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
        // RocksDB v1 exposes the data root in `tx_path_hash` and stores no tx path.
        let tx_path = match hashes.tx_path_hash {
            // MDBX-SUBMODULE-INDEX: delete this arm. Only MDBX stores tx-path bytes. RocksDB v1 derives them.
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
    /// When the pwrite of a still-queued chunk returned. Ingress latency only.
    pwrite_at: HashMap<PartitionChunkOffset, Instant>,
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
    /// When the pwrite returned. `None` if that stamp was not kept.
    pwrite_at: Option<Instant>,
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

impl PendingWrites {
    /// Stored priority, or Packing for entropy and Ingress for every other type.
    fn indexed(&self) -> Vec<(disk_lane::WritePriority, PartitionChunkOffset)> {
        self.chunks
            .iter()
            .map(|(offset, state)| {
                let priority = self
                    .priorities
                    .get(offset)
                    .copied()
                    .unwrap_or(match state.1 {
                        ChunkType::Entropy => disk_lane::WritePriority::Packing,
                        _ => disk_lane::WritePriority::Ingress,
                    });
                (priority, *offset)
            })
            .collect()
    }
}

/// Storage submodules mapped to their chunk ranges
type SubmoduleMap =
    NoditMap<PartitionChunkOffset, Interval<PartitionChunkOffset>, StorageSubmodule>;

/// Tracks storage state of chunk ranges across all submodules
type StorageIntervals = NoditMap<PartitionChunkOffset, Interval<PartitionChunkOffset>, ChunkType>;

/// Read guard over the live interval map.
pub struct LabelRuns<'a> {
    guard: RwLockReadGuard<'a, StorageIntervals>,
    #[cfg(any(test, feature = "test-utils"))]
    visits: &'a AtomicU64,
}

impl LabelRuns<'_> {
    /// Every stored run, in offset order.
    pub fn iter(
        &self,
    ) -> impl DoubleEndedIterator<Item = (&Interval<PartitionChunkOffset>, &ChunkType)> + '_ {
        #[cfg(any(test, feature = "test-utils"))]
        let visits = self.visits;
        self.guard.iter().inspect(move |_| {
            #[cfg(any(test, feature = "test-utils"))]
            visits.fetch_add(1, Ordering::SeqCst);
        })
    }

    /// Stored runs that overlap `range`, in offset order. [`Iterator::rev`]
    /// walks that overlap from the high end and still stops early.
    pub fn overlapping(
        &self,
        range: Interval<PartitionChunkOffset>,
    ) -> impl DoubleEndedIterator<Item = (&Interval<PartitionChunkOffset>, &ChunkType)> + '_ {
        #[cfg(any(test, feature = "test-utils"))]
        let visits = self.visits;
        self.guard.overlapping(range).inspect(move |_| {
            #[cfg(any(test, feature = "test-utils"))]
            visits.fetch_add(1, Ordering::SeqCst);
        })
    }
}

/// Offsets whose on-disk label must stay `Interrupted` until the completion is
/// stored, or until a clean shutdown writes the live map.
type IntervalMask = NoditSet<PartitionChunkOffset, Interval<PartitionChunkOffset>>;

/// Bytes of `Uninitialized` marked `Interrupted` ahead of one write, per
/// submodule. A crash reloads that span as `Uninitialized`. The node packs
/// or syncs those chunks again. The run just written is extra. Following
/// `Entropy` is not marked: a crash would drop its placements.
const INTERVAL_WRITE_AHEAD_BYTES: u64 = 512 * 1024 * 1024;

/// Last `intervals.json` image (`written`, including `Interrupted`) and the
/// completions not in it yet. Load turns `Interrupted` into `Uninitialized`.
/// An ahead mark lives only in `mask`, so the live map still says `Entropy`.
///
/// Mark `Interrupted` before `pwrite` (skip a span already marked). The same
/// rewrite may mark up to 512 MiB of following `Uninitialized` in the
/// submodule, stopping at live `Data` or `Entropy`.
/// Completions are stored after `fdatasync` and the presence commit, at most
/// every `intervals_persist_secs`, or sooner on the next write-ahead,
/// `force_sync`, or shutdown. A failed `pwrite` leaves the file
/// `Interrupted`. A CRC mismatch cuts the live label to `Uninitialized` and
/// rewrites that submodule's `intervals.json` before the confirm returns.
/// The rewrite keeps the ahead mask, so it does not publish `Entropy` over a
/// span the file still has to treat as `Interrupted`. A crash reloads
/// `Uninitialized`, and open drops the placement. Reset publishes
/// `Uninitialized` after the index clear. The metadata marker covers a crash
/// before that rewrite.
///
/// Period 0 rewrites on every sync and does not mark ahead. Queued or parked
/// offsets stay `Interrupted`. Shutdown force-syncs, writes the live map, and
/// drops the mask. A crash leaves the mask, so open treats it as
/// `Uninitialized`. The checksum commits
/// with the presence row, after `fdatasync` and before the completion label,
/// while that mark still covers the bytes.
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
    /// Authoritative interval replace has not committed. The ahead mask must
    /// still describe the previous file.
    BeforeIntervalImageCommit,
    /// Rollback has committed the `Uninitialized` interval image. The proof
    /// index clear has not started.
    AfterRollbackIntervalImage,
    /// Reset marker write has not started. Pending is still the caller's.
    ResetMarkerWrite,
    /// Reset marker is durable. The index clear has not run.
    AfterResetMarker,
    /// Index is cleared. The interval file is not rewritten yet.
    AfterResetClear,
    /// Interval file is `Uninitialized`. The reset marker is still set.
    AfterResetIntervals,
    /// `store_assignment` has not written. The reset, if it ran, is done.
    AssignmentStore,
}

/// Test barrier. `AfterPresenceCopy` runs inside a durable point that holds
/// `sync_in_progress`. `AfterIntervalSnapshot` does not: the writer has
/// copied the interval map and has not taken the file lock.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[expect(
    clippy::enum_variant_names,
    reason = "the prefix is the pause moment, after a named step"
)]
enum SyncPausePoint {
    /// Proofs are copied out of `pending`. The index write has not started.
    AfterPresenceCopy,
    /// The interval map is copied. `intervals.json` is not locked yet.
    AfterIntervalSnapshot,
    /// A due completion has cloned the live map. The file replace has not
    /// started. A checksum cut can land in this gap.
    AfterCompletionClone,
    /// `reset` has cleared `pending` once. The interval map is still the
    /// pre-reset labels.
    AfterResetPendingClear,
    /// A confirmed checksum mismatch is about to cut the live label.
    /// The cut has not run.
    AfterCrcConfirm,
    /// Pwrite has returned. The live label for that run has not been updated.
    AfterPwrite,
    /// The no-lane entropy pread has returned and dropped the file lock.
    /// The sweep has not checked the slot or queued packed bytes.
    AfterEntropyPread,
    /// Placements for one unpacked chunk are resolved. The occupy has not
    /// read the interval map or taken `pending_writes`.
    AfterOccupyResolved,
    /// `generate_full_chunks` has copied the bodies. It has not classified
    /// them against the checksum loaded before that read.
    AfterServeBodyRead,
}

/// Test barrier in a registration, after the rows are computed and before
/// they are committed. The registration does not hold `sync_in_progress`
/// or the assignment lock while it waits.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegistrationPausePoint {
    AfterRowsComputed,
    /// The assignment read lock is held. The index transaction has not started.
    DuringCommit,
}

/// One armed pause. `wait_if` clears the point before it waits, so a second
/// caller at that point returns. `reset` can rewrite the interval file while
/// a test still holds the first writer.
#[cfg(test)]
#[derive(Debug)]
struct PauseGate<T> {
    point: Mutex<Option<T>>,
    arrived: Mutex<bool>,
    arrived_cv: Condvar,
    release: Mutex<bool>,
    release_cv: Condvar,
}

#[cfg(test)]
impl<T: Copy + PartialEq> PauseGate<T> {
    fn new() -> Self {
        Self {
            point: Mutex::new(None),
            arrived: Mutex::new(false),
            arrived_cv: Condvar::new(),
            release: Mutex::new(false),
            release_cv: Condvar::new(),
        }
    }

    fn arm(&self, point: T) {
        *self.release.lock().unwrap() = false;
        *self.arrived.lock().unwrap() = false;
        *self.point.lock().unwrap() = Some(point);
    }

    fn wait_arrived(&self, timeout: Duration) -> bool {
        let guard = self.arrived.lock().unwrap();
        let (guard, _) = self
            .arrived_cv
            .wait_timeout_while(guard, timeout, |arrived| !*arrived)
            .unwrap();
        *guard
    }

    fn release(&self) {
        *self.release.lock().unwrap() = true;
        self.release_cv.notify_all();
    }

    fn wait_if(&self, point: T) {
        {
            let mut armed = self.point.lock().unwrap();
            if *armed != Some(point) {
                return;
            }
            *armed = None;
        }
        {
            let mut arrived = self.arrived.lock().unwrap();
            *arrived = true;
            self.arrived_cv.notify_all();
        }
        let mut release = self.release.lock().unwrap();
        while !*release {
            release = self.release_cv.wait(release).unwrap();
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
    /// Bumped under the assignment write lock at the start of
    /// [`Self::rollback_unassigned_range`], before the index clear.
    /// A registration captured at the previous value does not commit.
    rollback_generation: AtomicU64,
    /// Shared across one index commit. Exclusive from the start of partition
    /// recovery until the block-index truncate returns. Lock order: this
    /// gate, then `partition_assignment`.
    index_recovery: RwLock<()>,
    /// In-memory chunk buffer awaiting disk write. Occupancy (index ops in
    /// flight) lives under this same lock.
    pending_writes: RwLock<PendingWrites>,
    /// Entropy read past a demand span. Lock order: `intervals`, then
    /// `pending_writes`, then this cache. A write takes `pending_writes`
    /// and then this cache. An entropy write takes `intervals` before
    /// `pending_writes`. The reverse deadlocks a reader that already holds
    /// `intervals`.
    entropy_cache: Mutex<disk_lane::EntropyPadCache>,
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
    /// Intervals examined by a label scan. A range query must not walk the
    /// fragments outside that range.
    #[cfg(any(test, feature = "test-utils"))]
    interval_visits: AtomicU64,
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
    /// Armed sync failure and how many hits of that point to ignore.
    #[cfg(test)]
    sync_failure: Mutex<Option<(SyncFailurePoint, u64)>>,
    /// Pauses one durable point so a test can call `reset` in the gap.
    #[cfg(test)]
    sync_pause: PauseGate<SyncPausePoint>,
    /// Pauses one checksum check after the bytes are read and before the
    /// offset row is loaded. The durable point can commit in that gap.
    #[cfg(test)]
    crc_check_pause: PauseGate<SyncPausePoint>,
    /// Pauses one interval rewrite after the map copy and before the file lock.
    /// The pause disarms itself, so `reset` does not wait on its own rewrite.
    #[cfg(test)]
    interval_image_pause: PauseGate<SyncPausePoint>,
    /// Pauses one registration after its rows are computed and before commit.
    #[cfg(test)]
    registration_pause: PauseGate<RegistrationPausePoint>,
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
    /// Held across an `intervals.json` replace. Lock order: this lock, then
    /// `interval_disk`, then the submodule file mutex. Do not take it while
    /// `intervals` or `interval_disk` is held. The map clone stays outside
    /// the lock; the fsync is the section that has to be ordered.
    interval_publish: Mutex<()>,
    /// Bumped under [`Self::interval_publish`] when a checksum cut replaces
    /// the file. A completion that cloned the map before the bump does not
    /// commit that image and does not clear `dirty`. This is not
    /// [`Self::interval_image_epoch`]: that bump makes
    /// [`Self::persist_write_ahead`] skip its `Interrupted` mark.
    crc_cut_epoch: AtomicU64,
    /// Bumped inside the rollback mutation, under the intervals write lock,
    /// together with the live-map flip, before the file replace. A completion
    /// that cloned the map before the bump does not commit that image and
    /// does not clear `dirty`. This is not [`Self::interval_image_epoch`] and
    /// not [`Self::crc_cut_epoch`].
    publish_generation: AtomicU64,
    /// Bumped when an entropy `pwrite` lands. A disk read that captured an
    /// older value must not pack those bytes or cache them.
    entropy_write_epoch: AtomicU64,
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
    /// A complete module leaves this set: a later checksum write does not
    /// open a gap, so it must not walk the offset span again.
    crc_scan_done: AtomicBool,
    /// Submodule start of the first missing checksum. `u64::MAX` is none.
    /// The next settled scan starts at [`Self::crc_gap_offset`] so it does
    /// not decode the checksummed prefix again.
    crc_gap_submodule: AtomicU64,
    /// Offset of that checksum hole. `u64::MAX` is none.
    crc_gap_offset: AtomicU64,
    /// Calls to `offset_crc_missing` from the settled scan. One submodule
    /// span is one call. Test code that reads a span does not count.
    #[cfg(test)]
    crc_span_scans: AtomicU64,
    /// Start offset of the last counted span scan. `u64::MAX` is none.
    #[cfg(test)]
    crc_scan_from: AtomicU64,
    /// Crash tests drop the module without the final fsync or interval rewrite.
    #[cfg(test)]
    abandon_on_drop: AtomicBool,
    /// Interval-tree edits inside [`Self::rollback_unassigned_range`].
    /// One span is one cut and one insert.
    #[cfg(test)]
    rollback_interval_edits: AtomicU64,
    /// Set when rollback's assignment `try_write` returns [`std::sync::TryLockError::WouldBlock`].
    #[cfg(test)]
    rollback_assignment_blocked: AtomicBool,
    /// When set, rollback and reset record a contended `sync_in_progress` try.
    #[cfg(test)]
    sync_lock_probe: AtomicBool,
    /// Set when that try returns [`std::sync::TryLockError::WouldBlock`].
    #[cfg(test)]
    sync_lock_blocked: AtomicBool,
    /// The next lane `pwrite` panics once, before the kernel write.
    #[cfg(test)]
    pwrite_panic_once: AtomicBool,
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

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq, Copy)]
pub struct PackingParams {
    pub packing_address: IrysAddress,
    pub partition_hash: Option<H256>,
    pub ledger: Option<u32>,
    pub slot: Option<usize>,
    pub last_updated_height: Option<u64>,
    /// Iterations this module was packed with. Absent on a file written
    /// before the field existed. Open fills it from the current consensus
    /// and writes the file back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entropy_packing_iterations: Option<u32>,
    /// Chunk size this module was packed with. Same backfill as iterations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_size: Option<u64>,
}

impl PackingParams {
    /// Loads the [`PackingParams`] from a TOML file at the given path.
    /// A read or parse error names `path`.
    pub fn from_toml(path: impl AsRef<Path>) -> eyre::Result<Self> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path)
            .wrap_err_with(|| format!("packing params at {}", path.display()))?;
        let config: Self = toml::from_str(&contents)
            .wrap_err_with(|| format!("packing params at {}", path.display()))?;
        Ok(config)
    }

    /// Replace `path` by writing a temp file and renaming it, the same way
    /// `intervals.json` is replaced. A crash during the write leaves the
    /// previous file in place.
    pub fn write_to_disk(&self, path: &Path) -> eyre::Result<()> {
        let toml = toml::to_string(self).wrap_err("serialize packing params")?;
        let mut file = get_atomic_file(path)?;
        file.write_all(toml.as_bytes())?;
        file.commit()?;
        Ok(())
    }

    /// Fills iterations and chunk size from the current consensus when the
    /// file omitted them. Returns true when a field was filled. The caller
    /// writes the file back.
    pub fn backfill_recorded_packing(&mut self, consensus: &irys_types::ConsensusConfig) -> bool {
        let mut filled = false;
        if self.entropy_packing_iterations.is_none() {
            self.entropy_packing_iterations = Some(consensus.entropy_packing_iterations);
            filled = true;
        }
        if self.chunk_size.is_none() {
            self.chunk_size = Some(consensus.chunk_size);
            filled = true;
        }
        filled
    }
}

pub static PACKING_PARAMS_FILE_NAME: &str = "packing_params.toml";

/// Load `packing_params.toml`. When iterations or chunk size are absent,
/// fill them from the current consensus and write the file back.
fn ensure_recorded_packing_params(
    path: &Path,
    consensus: &irys_types::ConsensusConfig,
) -> eyre::Result<PackingParams> {
    let mut params = PackingParams::from_toml(path)?;
    if params.backfill_recorded_packing(consensus) {
        params.write_to_disk(path)?;
    }
    Ok(params)
}

fn packing_record_from_params(
    params: &PackingParams,
    chain_id: u64,
) -> eyre::Result<PackingRecord> {
    let iterations = params
        .entropy_packing_iterations
        .ok_or_else(|| eyre!("packing params have no entropy_packing_iterations"))?;
    let chunk_size = params
        .chunk_size
        .ok_or_else(|| eyre!("packing params have no chunk_size"))?;
    Ok(PackingRecord {
        packing_address: params.packing_address.into(),
        iterations,
        chunk_size,
        chain_id,
    })
}

fn expected_packing_record(config: &Config) -> PackingRecord {
    PackingRecord {
        packing_address: config.node_config.miner_address().into(),
        iterations: config.consensus.entropy_packing_iterations,
        chunk_size: config.consensus.chunk_size,
        chain_id: config.consensus.chain_id,
    }
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

/// Result of an entropy insert. The caller must handle a refusal.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntropyWrite {
    Queued,
    Rejected(EntropyRejection),
}

/// Why an entropy insert did not queue.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntropyRejection {
    /// No assignment, or its hash is not the hash the bytes were packed for.
    NotAssigned,
    /// Live label is not `Uninitialized`, or queued data or a proof owns the offset.
    /// `None` means the interval map has no label at the offset.
    NotUninitialized(Option<ChunkType>),
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
        let pread_started = Instant::now();
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
        index_read_metrics::note_recall_pread(pread_started.elapsed());
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

// MDBX-SUBMODULE-INDEX: delete the Mdbx arm. Open rocks/ only.
fn open_submodule_index(
    module_dir: &Path,
    database: &DatabaseConfig,
) -> eyre::Result<SubmoduleIndex> {
    let path = match database.submodule_index {
        // MDBX-SUBMODULE-INDEX: delete this arm.
        SubmoduleIndexEngine::Mdbx => module_dir.join("db"),
        SubmoduleIndexEngine::Rocksdb => module_dir.join("rocks"),
    };
    debug!("submodule_db_path: {:?}", path);

    let opened = match database.submodule_index {
        // MDBX-SUBMODULE-INDEX: delete this arm. It opens db/ and migrates schema v1 in place.
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
            // MDBX-SUBMODULE-INDEX: delete this refusal once db/ is no longer a live index and the rollback window is closed.
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
                        // MDBX-SUBMODULE-INDEX: delete this log with the kept db/ once rollback is retired.
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
// MDBX-SUBMODULE-INDEX: delete the Mdbx variant. The bench opens rocks/ only.
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
// MDBX-SUBMODULE-INDEX: delete this helper with the MDBX bench open.
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

/// True when the file is shorter than the submodule, or when it has that
/// length but still has a hole (`st_blocks * 512 < st_size`).
///
/// A full file is left alone. XFS `fallocate` mode 0 of an allocated file
/// walks the extent map under the inode I/O lock, one transaction per extent.
/// The maximum extent is 8 GiB, so an 18 TiB file is about 2300 extents.
/// That walk blocks every read and write, and it can take seconds when a
/// checkpoint hits a HDD. Open pays it only for a short file or a hole.
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

/// `EFBIG` from the logical-size check. The index has not been opened.
fn chunks_file_too_big(path: &Path, full_len: u64) -> eyre::Report {
    eyre!(
        "EFBIG: chunks file {} cannot be {full_len} bytes on this filesystem. ext4's maximum file size is 16 TiB - 4 KiB. The submodule index was not migrated. Format the drive with XFS, or move the storage module to a filesystem that can hold the file",
        path.display()
    )
}

/// Logical size only. A test can force `EFBIG` without creating a large file.
fn set_chunks_logical_len(file: &File, full_len: u64) -> std::io::Result<()> {
    #[cfg(test)]
    if CHUNKS_LEN_EFBIG.with(std::cell::Cell::get) {
        return Err(std::io::Error::from_raw_os_error(libc::EFBIG));
    }
    file.set_len(full_len)
}

/// The filesystem must be able to name `full_len` before the index migrates.
///
/// `set_len` does not allocate blocks. On ext4 a mainnet partition
/// (18.01 TiB) returns `EFBIG`: the maximum file is 16 TiB - 4 KiB. The
/// schema row is unchanged. `EFBIG` is not reported as `ENOSPC`.
fn ensure_chunks_len_fits(path: &Path, full_len: u64) -> eyre::Result<()> {
    if full_len == 0 {
        return Err(eyre!(
            "refusing to allocate a zero-length chunks file at {}",
            path.display()
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .wrap_err_with(|| format!("Failed to create or open chunks file: {}", path.display()))?;
    let len = file
        .metadata()
        .wrap_err_with(|| format!("stat {}", path.display()))?
        .len();
    if len >= full_len {
        return Ok(());
    }
    match set_chunks_logical_len(&file, full_len) {
        Ok(()) => Ok(()),
        Err(err) if err.raw_os_error() == Some(libc::EFBIG) => {
            Err(chunks_file_too_big(path, full_len))
        }
        Err(err) if err.raw_os_error() == Some(libc::ENOSPC) => {
            Err(no_space(&file, path, full_len))
        }
        Err(err) => Err(eyre!(
            "set_len({full_len}) failed on {}: {err}",
            path.display()
        )),
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
    // The logical length is already set. This is the hole reserve, and it
    // runs only after the index has migrated: the reserve can take the free
    // space a copy of the old index still needs.
    note_submodule_open("reserve");
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

#[cfg(test)]
thread_local! {
    static SUBMODULE_OPEN_ORDER: std::cell::RefCell<Vec<&'static str>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Test open returns `EFBIG` from the logical-size check and does not
    /// call `set_len`. Production still calls `set_len`.
    static CHUNKS_LEN_EFBIG: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Test open fails after the reset marker is durable and before
    /// `crc_complete` is cleared. Production writes both, marker first.
    static FAIL_AFTER_OPEN_RESET_MARKER: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

fn note_submodule_open(step: &'static str) {
    #[cfg(test)]
    SUBMODULE_OPEN_ORDER.with(|order| order.borrow_mut().push(step));
    #[cfg(not(test))]
    {
        let _ = step;
    }
}

#[cfg(test)]
fn clear_submodule_open_order() {
    SUBMODULE_OPEN_ORDER.with(|order| order.borrow_mut().clear());
}

#[cfg(test)]
fn submodule_open_order() -> Vec<&'static str> {
    SUBMODULE_OPEN_ORDER.with(|order| order.borrow().clone())
}

/// A leftover sidecar is not the checksum. RocksDB v1 stores it in the offset row.
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

/// A poisoned mutex still yields its guard. The poison flag stays set.
/// [`StorageModule::lock_sync`] clears that flag: a later `try_lock` must
/// not keep failing after the panic.
fn recover_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn recover_read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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

    /// Both the span built at open and the loaded file must cover the partition.
    /// The only legal gap is the tail from `num_chunks` through `u32::MAX`.
    fn ensure_partition_coverage(
        intervals: &StorageIntervals,
        num_chunks: u64,
    ) -> eyre::Result<()> {
        let gaps = intervals
            .gaps_untrimmed(partition_chunk_offset_ii!(0, u32::MAX))
            .collect::<Vec<_>>();
        let expected = vec![partition_chunk_offset_ii!(
            u32::try_from(num_chunks).expect("Value exceeds u32::MAX"),
            u32::MAX
        )];
        if gaps != expected {
            return Err(eyre!(
                "Invalid storage module config, expected range {:?}, got range {:?}",
                &expected,
                &gaps
            ));
        }
        Ok(())
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
        // Hash, ledger, and slot changes are written only after
        // `bind_packing_identity` accepts. A refused open leaves the previous
        // file in place. The reset marker is written after every check that
        // can refuse. A hash change sets it. A ledger or slot change sets it
        // when the module already holds a ledger. Capacity gaining a ledger
        // keeps the stored bytes.
        let mut rebind_after_bind: Vec<(PathBuf, Option<H256>, Option<u32>, Option<usize>)> =
            Vec::new();
        let mut reset_dbs: Vec<SubmoduleIndex> = Vec::new();
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
            // Name the file before the migration. `set_len` does not allocate,
            // so a v1 copy still has the free space it needs. ext4 returns
            // `EFBIG` above 16 TiB - 4 KiB and the schema row stays put.
            // A bench split reserves its write file, not `chunks.dat`.
            #[cfg(feature = "bench")]
            let check_chunks_len = split.is_none();
            #[cfg(not(feature = "bench"))]
            let check_chunks_len = true;
            if check_chunks_len {
                ensure_chunks_len_fits(&sub_base_path.join("chunks.dat"), full_len)?;
            }
            // Migrate the index before reserving the partition. A v1 index
            // keeps its rows until the v2 copy commits, and the reserve can
            // take the free space that copy needs. The index does not read
            // the chunks file.
            let submodule_db = Self::open_submodule_index(
                &sub_base_path,
                config,
                #[cfg(feature = "bench")]
                split.is_some(),
                #[cfg(feature = "bench")]
                index,
            )?;
            let opened = Self::open_submodule_file(
                &sub_base_path,
                full_len,
                config.node_config.storage.recall_direct_io,
                #[cfg(feature = "bench")]
                split.as_ref(),
            )?;
            let chunks_file = opened.file;

            let params_path = sub_base_path.join(PACKING_PARAMS_FILE_NAME);
            // if we don't have an existing packing params file, write it
            if !params_path.exists() {
                let mut params = PackingParams {
                    packing_address: config.node_config.miner_address(),
                    entropy_packing_iterations: Some(config.consensus.entropy_packing_iterations),
                    chunk_size: Some(config.consensus.chunk_size),
                    ..Default::default()
                };
                if let Some(pa) = storage_module_info.partition_assignment {
                    params.partition_hash = Some(pa.partition_hash);
                    params.ledger = pa.ledger_id;
                    params.slot = pa.slot_index;
                }
                params.write_to_disk(&params_path)?;
            } else {
                // Load the packing params and check to see if they match.
                // A short or corrupt file is an open error. It must not abort
                // the process before the address check.
                let params = PackingParams::from_toml(&params_path)?;

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

                // A different partition clears the old labels and index rows.
                // A ledger or slot change does the same when the module already
                // holds a ledger: the bytes belong to the previous assignment.
                // The marker is written after packing validation, interval
                // repair, and coverage. A refusal before that leaves the bytes
                // alone. Capacity gaining a ledger keeps the bytes.
                let ledger_or_slot_changed =
                    params.ledger != want_ledger || params.slot != want_slot;
                let leaving_a_ledger = params.ledger.is_some() && ledger_or_slot_changed;
                let wipe = params.partition_hash != want_hash || leaving_a_ledger;
                if wipe || ledger_or_slot_changed {
                    if wipe {
                        reset_dbs.push(submodule_db.clone());
                    }
                    rebind_after_bind.push((
                        params_path.clone(),
                        want_hash,
                        want_ledger,
                        want_slot,
                    ));
                }
            }

            let params = ensure_recorded_packing_params(&params_path, &config.consensus)
                .wrap_err_with(|| format!("packing params at {}", params_path.display()))?;
            let recorded = packing_record_from_params(&params, config.consensus.chain_id)?;
            let expected = expected_packing_record(config);
            if let Err(err) = packing_params_valid_for_module(&recorded, &expected) {
                return Err(eyre!("{err} at {}", params_path.display()));
            }
            // The file still names the old hash until bind accepts.
            let bound_partition = rebind_after_bind
                .iter()
                .find(|(path, ..)| path == &params_path)
                .map(|(_, hash, ..)| *hash)
                .unwrap_or(params.partition_hash);

            let intervals_file_path = sub_base_path.join("intervals.json");

            let submodules_intervals_file = PathBuf::from(&intervals_file_path);

            ensure_default_intervals(
                &submodule_interval,
                &submodules_intervals_file,
                &submodule_db,
            )?;

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
                        index_drain: index_drain::IndexDrain::start(
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
            pack_binds.push((submodule_db, bound_partition, recorded));

            global_intervals
                .insert_merge_touching_if_values_equal(submodule_interval, ChunkType::Uninitialized)
                .map_err(|e| eyre::eyre!("Failed to insert submodule interval: {:?}", e))?;
        }

        Self::ensure_partition_coverage(
            &global_intervals,
            config.consensus.num_chunks_in_partition,
        )?;

        // The file has no chain id. Compare the stored Rocks record before
        // any wipe. `None` does not bind a partition: the reset clear would
        // drop one bound here, and a different partition must refuse only
        // after that clear. MDBX ignores this call.
        for (index, _partition, packing) in &pack_binds {
            index.bind_packing_identity(None, packing)?;
        }
        // Marker first. A crash before it leaves `crc_complete` set on an
        // intact module. `finish_pending_resets` clears the flag again.
        for index in &reset_dbs {
            index.set_reset_in_progress()?;
            #[cfg(test)]
            if FAIL_AFTER_OPEN_RESET_MARKER.with(|flag| flag.replace(false)) {
                eyre::bail!("injected failure after the open reset marker");
            }
            index.set_chunk_crc_complete(false)?;
        }

        // A crash during reset leaves the marker set. Finish the clear and
        // rewrite that submodule's intervals before the file is loaded.
        // The packing bind runs after that clear, so a new partition hash is
        // the one that stays.
        Self::finish_pending_resets(&submodule_map)?;
        for (index, partition, packing) in pack_binds {
            index.bind_packing_identity(partition, &packing)?;
        }
        // Bind accepted. The file may now name the new partition, ledger, and slot.
        for (path, hash, ledger, slot) in rebind_after_bind {
            let mut params = PackingParams::from_toml(&path)?;
            params.partition_hash = hash;
            params.ledger = ledger;
            params.slot = slot;
            params.last_updated_height = Some(0);
            params.write_to_disk(&path)?;
        }

        // Attempt to load a global set of intervals from the submodules
        let loaded_intervals: NoditMap<
            PartitionChunkOffset,
            Interval<PartitionChunkOffset>,
            ChunkType,
        > = Self::load_intervals_from_submodules(&submodule_map, storage_module_info.id)?;

        Self::ensure_partition_coverage(
            &loaded_intervals,
            config.consensus.num_chunks_in_partition,
        )?;

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
            // MDBX-SUBMODULE-INDEX: delete this engine check and always enable group commit. MDBX is left off because its SAFE_NOSYNC path aborts inside libmdbx.
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
            rollback_generation: AtomicU64::new(0),
            index_recovery: RwLock::new(()),
            pending_writes: RwLock::new(PendingWrites::default()),
            entropy_cache: Mutex::new(disk_lane::EntropyPadCache::default()),
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
            #[cfg(any(test, feature = "test-utils"))]
            interval_visits: AtomicU64::new(0),
            sync_in_progress: Mutex::new(()),
            last_durable_at: Mutex::new(None),
            headroom_open: AtomicBool::new(true),
            durable_points: DurablePointAtoms::default(),
            data_writes_paused: AtomicBool::new(false),
            #[cfg(test)]
            sync_failure: Mutex::new(None),
            #[cfg(test)]
            sync_pause: PauseGate::new(),
            #[cfg(test)]
            crc_check_pause: PauseGate::new(),
            #[cfg(test)]
            interval_image_pause: PauseGate::new(),
            #[cfg(test)]
            registration_pause: PauseGate::new(),
            #[cfg(test)]
            registration_drops: AtomicU64::new(0),
            last_pending_write: RwLock::new(Instant::now()),
            intervals: Arc::new(RwLock::new(loaded_intervals)),
            interval_disk: Mutex::new(Self::load_interval_disk(&submodule_map)),
            interval_image_epoch: AtomicU64::new(0),
            interval_publish: Mutex::new(()),
            crc_cut_epoch: AtomicU64::new(0),
            publish_generation: AtomicU64::new(0),
            entropy_write_epoch: AtomicU64::new(0),
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
            crc_gap_submodule: AtomicU64::new(u64::MAX),
            crc_gap_offset: AtomicU64::new(u64::MAX),
            #[cfg(test)]
            crc_span_scans: AtomicU64::new(0),
            #[cfg(test)]
            crc_scan_from: AtomicU64::new(u64::MAX),
            #[cfg(test)]
            abandon_on_drop: AtomicBool::new(false),
            #[cfg(test)]
            rollback_interval_edits: AtomicU64::new(0),
            #[cfg(test)]
            rollback_assignment_blocked: AtomicBool::new(false),
            #[cfg(test)]
            sync_lock_probe: AtomicBool::new(false),
            #[cfg(test)]
            sync_lock_blocked: AtomicBool::new(false),
            #[cfg(test)]
            pwrite_panic_once: AtomicBool::new(false),
        })
    }

    fn open_submodule_index(
        sub_base_path: &Path,
        config: &Config,
        #[cfg(feature = "bench")] split: bool,
        #[cfg(feature = "bench")] index: Option<BenchIndex>,
    ) -> eyre::Result<SubmoduleIndex> {
        note_submodule_open("index");
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
                // MDBX-SUBMODULE-INDEX: delete this MDBX bench open.
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
        note_submodule_open("file");
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
                let recall = recover_mutex(&split.recall);
                advise_chunks_random(&recall, Path::new("split recall"))
                    .wrap_err("Failed to disable read-ahead on the recall file")?;
            }
            {
                let write = recover_mutex(&split.write);
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

    pub fn assign_partition(
        &self,
        partition_assignment: PartitionAssignment,
        update_height: u64,
    ) -> eyre::Result<()> {
        let mut pa = self.partition_assignment.write().unwrap();
        self.store_assignment(&mut pa, Some(partition_assignment), update_height)
    }

    /// Clears the current partition assignment and persists the change on disk.
    ///
    /// Writes `packing_params.toml` with `partition_hash = None`, `ledger = None`,
    /// `slot = None`, and updates `last_updated_height`. In-memory assignment is set to `None`.
    pub fn clear_assignment(&self, update_height: u64) -> eyre::Result<()> {
        let mut pa = self.partition_assignment.write().unwrap();
        self.store_assignment(&mut pa, None, update_height)
    }

    /// Store `next` and write each submodule's `packing_params.toml`.
    /// The caller holds the assignment write lock. A parse or write error
    /// leaves the in-memory assignment unchanged.
    fn store_assignment(
        &self,
        assignment: &mut Option<PartitionAssignment>,
        next: Option<PartitionAssignment>,
        update_height: u64,
    ) -> eyre::Result<()> {
        #[cfg(test)]
        self.inject_sync_failure(SyncFailurePoint::AssignmentStore)?;
        let old_hash = assignment.as_ref().map(|current| current.partition_hash);
        let new_hash = next.map(|current| current.partition_hash);
        let mut updated = Vec::new();
        for (_, submodule) in self.submodules.iter() {
            let params_path = submodule.path.join(PACKING_PARAMS_FILE_NAME);
            let mut params = if params_path.exists() {
                PackingParams::from_toml(&params_path)?
            } else {
                PackingParams {
                    packing_address: self.config.node_config.miner_address(),
                    ..Default::default()
                }
            };
            params.packing_address = self.config.node_config.miner_address();
            params.partition_hash = next.map(|assignment| assignment.partition_hash);
            params.ledger = next.and_then(|assignment| assignment.ledger_id);
            params.slot = next.and_then(|assignment| assignment.slot_index);
            params.last_updated_height = Some(update_height);
            // Keep the iterations this module was packed with. Fill them only
            // when the file never recorded them.
            params.backfill_recorded_packing(&self.config.consensus);
            updated.push((params_path, params));
        }
        for (path, params) in &updated {
            params.write_to_disk(path)?;
        }
        *assignment = next;
        if old_hash != new_hash {
            self.entropy_cache.lock().unwrap().clear();
        }
        Ok(())
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

    /// Shared gate for one index commit. A poisoned lock is recovered.
    pub fn lock_index_recovery_shared(&self) -> std::sync::RwLockReadGuard<'_, ()> {
        self.index_recovery
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Exclusive gate for partition recovery, held through the block-index
    /// truncate. A poisoned lock is recovered.
    pub fn lock_index_recovery_exclusive(&self) -> std::sync::RwLockWriteGuard<'_, ()> {
        self.index_recovery
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `Some` when this thread took the exclusive recovery gate. `None` when
    /// another guard still holds it. Drop the guard before the shared holder
    /// continues, or that release cannot finish.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn try_lock_index_recovery_exclusive(&self) -> Option<std::sync::RwLockWriteGuard<'_, ()>> {
        match self.index_recovery.try_write() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }

    /// The assignment and the height it was stored at. The height is read
    /// from `packing_params.toml` under the assignment lock, so it belongs
    /// to this assignment. `None` when the file never recorded one.
    pub fn partition_assignment_with_height(
        &self,
    ) -> eyre::Result<(Option<PartitionAssignment>, Option<u64>)> {
        let pa = self.partition_assignment.read().unwrap();
        let Some((_, submodule)) = self.submodules.iter().next() else {
            return Ok((*pa, None));
        };
        let params = PackingParams::from_toml(submodule.path.join(PACKING_PARAMS_FILE_NAME))?;
        Ok((*pa, params.last_updated_height))
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
        // A failed pwrite keeps these bytes queued. The lane sleeps until
        // the retry instant instead of issuing the same write again.
        if self.disk.write_retry_pending() {
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
        let mut indexed = pending.indexed();
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

    /// True when `chunks`, `written_ahead`, or `occupancy` holds an offset in
    /// the inclusive span. An empty or inverted span is false. The scan is
    /// the pending set, not the span. An empty-byte map entry still overlaps.
    pub fn pending_overlaps_inclusive(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> bool {
        if start > end {
            return false;
        }
        let pending = self.pending_writes.read().unwrap();
        pending.range(start..=end).next().is_some()
            || pending.written_ahead.range(start..=end).next().is_some()
            || pending
                .occupancy
                .keys()
                .any(|offset| *offset >= start && *offset <= end)
    }

    /// True while queued chunk bytes are under [`Self::pending_memory_ceiling_bytes`].
    /// Producers that can outrun the disk (packing) wait for this: the disk
    /// lane drains asynchronously, so `sync_pending_chunks` does not block.
    pub fn has_pending_room(&self) -> bool {
        self.pending_write_bytes() < self.pending_memory_ceiling_bytes()
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
            self.entropy_cache.lock().unwrap().clear();
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
        *self.sync_failure.lock().unwrap() = Some((point, 0));
    }

    /// The `skip + 1`th hit of `point` fails. Earlier hits of that point run.
    #[cfg(test)]
    fn fail_sync_after(&self, point: SyncFailurePoint, skip: u64) {
        *self.sync_failure.lock().unwrap() = Some((point, skip));
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

    /// The next lane `pwrite` panics once, before the kernel write.
    #[cfg(test)]
    fn panic_next_pwrite_for_test(&self) {
        self.pwrite_panic_once.store(true, Ordering::SeqCst);
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

    /// Intervals a label scan has examined.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn interval_visit_count(&self) -> u64 {
        self.interval_visits.load(Ordering::SeqCst)
    }

    fn note_interval_visit(&self) {
        #[cfg(any(test, feature = "test-utils"))]
        self.interval_visits.fetch_add(1, Ordering::SeqCst);
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
        if let Some((armed, skip)) = *failure
            && armed == point
        {
            if skip > 0 {
                *failure = Some((armed, skip - 1));
                return Ok(());
            }
            *failure = None;
            eyre::bail!("injected storage-module sync failure at {point:?}");
        }
        Ok(())
    }

    /// The next durable point waits at `point` until [`Self::release_sync_pause`].
    #[cfg(test)]
    fn pause_next_sync_at(&self, point: SyncPausePoint) {
        self.sync_pause.arm(point);
    }

    /// `true` when the armed point was reached before `timeout`.
    #[cfg(test)]
    fn wait_for_sync_pause(&self, timeout: Duration) -> bool {
        self.sync_pause.wait_arrived(timeout)
    }

    /// Let a paused durable point continue.
    #[cfg(test)]
    fn release_sync_pause(&self) {
        self.sync_pause.release();
    }

    #[cfg(test)]
    fn wait_if_sync_paused(&self, point: SyncPausePoint) {
        self.sync_pause.wait_if(point);
    }

    /// The next `generate_full_chunks` waits after the body read.
    #[cfg(test)]
    fn pause_next_serve_after_body_read(&self) {
        self.sync_pause.arm(SyncPausePoint::AfterServeBodyRead);
    }

    /// The next checksum check waits after its bytes are read.
    #[cfg(test)]
    fn pause_next_crc_check(&self) {
        self.crc_check_pause.arm(SyncPausePoint::AfterPresenceCopy);
    }

    /// `true` when the checksum check reached its pause before `timeout`.
    #[cfg(test)]
    fn wait_for_crc_check_pause(&self, timeout: Duration) -> bool {
        self.crc_check_pause.wait_arrived(timeout)
    }

    /// Let a paused checksum check continue.
    #[cfg(test)]
    fn release_crc_check_pause(&self) {
        self.crc_check_pause.release();
    }

    #[cfg(test)]
    fn wait_if_crc_check_paused(&self) {
        self.crc_check_pause
            .wait_if(SyncPausePoint::AfterPresenceCopy);
    }

    /// The next interval-file rewrite waits after it copies the live map.
    #[cfg(test)]
    fn pause_next_interval_image(&self) {
        self.interval_image_pause
            .arm(SyncPausePoint::AfterIntervalSnapshot);
    }

    /// `true` when the interval rewrite reached its pause before `timeout`.
    #[cfg(test)]
    fn wait_for_interval_image_pause(&self, timeout: Duration) -> bool {
        self.interval_image_pause.wait_arrived(timeout)
    }

    /// Let a paused interval rewrite continue.
    #[cfg(test)]
    fn release_interval_image_pause(&self) {
        self.interval_image_pause.release();
    }

    /// One-shot. The first writer disarms the pause before it waits, so a
    /// `reset` that rewrites the file while the test holds the writer does
    /// not block on this gate. `reset` holds `sync_in_progress`.
    #[cfg(test)]
    fn wait_if_interval_image_paused(&self) {
        self.interval_image_pause
            .wait_if(SyncPausePoint::AfterIntervalSnapshot);
    }

    /// The next due completion waits after it clones the live map and before
    /// it replaces `intervals.json`. The pause disarms itself.
    #[cfg(test)]
    fn pause_next_completion_clone(&self) {
        self.interval_image_pause
            .arm(SyncPausePoint::AfterCompletionClone);
    }

    #[cfg(test)]
    fn wait_if_completion_clone_paused(&self) {
        self.interval_image_pause
            .wait_if(SyncPausePoint::AfterCompletionClone);
    }

    /// The next registration waits after its rows are computed.
    #[cfg(test)]
    fn pause_next_registration_at(&self, point: RegistrationPausePoint) {
        self.registration_pause.arm(point);
    }

    /// `true` when the armed registration pause was reached before `timeout`.
    #[cfg(test)]
    fn wait_for_registration_pause(&self, timeout: Duration) -> bool {
        self.registration_pause.wait_arrived(timeout)
    }

    /// Let a paused registration continue.
    #[cfg(test)]
    fn release_registration_pause(&self) {
        self.registration_pause.release();
    }

    #[cfg(test)]
    fn wait_if_registration_paused(&self, point: RegistrationPausePoint) {
        self.registration_pause.wait_if(point);
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
    /// `pending.chunks` or `written_ahead`. A checksum index also requires a
    /// stored offset row: a `Data` label with no presence commit is not durable.
    pub fn is_data_chunk_durable_at(&self, offset: PartitionChunkOffset) -> bool {
        let pending = self.pending_writes.read().unwrap();
        if pending.contains_key(&offset) || pending.written_ahead.contains_key(&offset) {
            return false;
        }
        drop(pending);
        let labeled_data = self
            .intervals
            .read()
            .unwrap()
            .get_at_point(offset)
            .is_some_and(|chunk_type| *chunk_type == ChunkType::Data);
        if !labeled_data {
            return false;
        }
        self.data_presence_committed(offset)
    }

    /// `true` when `offset` has a presence row, or the index does not store
    /// checksums. A read error is not durable. MDBX does not store a row for
    /// a proof-less write, so the label is enough there.
    fn data_presence_committed(&self, offset: PartitionChunkOffset) -> bool {
        let Ok((_, submodule)) = self.get_submodule_for_offset(offset) else {
            return false;
        };
        // MDBX-SUBMODULE-INDEX: delete this arm. MDBX treats the interval label as presence because it stores no checksum row.
        if !submodule.db.stores_offset_crc() {
            return true;
        }
        match submodule.db.view(|tx| tx.get_offset_meta(offset)) {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(err) => {
                warn!("offset presence read failed: {err}");
                false
            }
        }
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

    /// Transaction offsets whose bodies are fsynced as `Data` and already
    /// stored as `Data` in `intervals.json`.
    ///
    /// Cached-body reclamation uses this. The live label flips at `pwrite`,
    /// and the file can lag by `intervals_persist_secs`. A crash in that lag
    /// reloads the offset as `Uninitialized` and drops its rows. A fresh
    /// upload may have no other replica, so the cache stays until the file
    /// says `Data` ([`Self::is_data_chunk_persisted_at`]).
    pub fn durable_tx_offsets_for_data_root(
        &self,
        data_root: DataRoot,
        candidates: &BTreeSet<TxChunkOffset>,
    ) -> eyre::Result<BTreeSet<TxChunkOffset>> {
        // One short lock acquisition per offset rather than holding
        // pending/intervals across the whole loop: this runs on the
        // background prune path and must not stall chunk writes.
        self.tx_offsets_placed_where(data_root, candidates, |offset| {
            self.is_data_chunk_persisted_at(offset)
        })
    }

    /// Candidate offsets of `data_root` that this module is assigned to
    /// store: a local placement covers them, durable or not.
    pub fn placed_tx_offsets_for_data_root(
        &self,
        data_root: DataRoot,
        candidates: &BTreeSet<TxChunkOffset>,
    ) -> eyre::Result<BTreeSet<TxChunkOffset>> {
        self.tx_offsets_placed_where(data_root, candidates, |_| true)
    }

    /// Candidates with a local placement whose partition offset satisfies `keep`.
    /// Resolves the submodule index once per root.
    fn tx_offsets_placed_where(
        &self,
        data_root: DataRoot,
        candidates: &BTreeSet<TxChunkOffset>,
        keep: impl Fn(PartitionChunkOffset) -> bool,
    ) -> eyre::Result<BTreeSet<TxChunkOffset>> {
        let infos = self.collect_data_root_infos(data_root)?;
        let mut out = BTreeSet::new();
        if infos.0.is_empty() {
            return Ok(out);
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
                if keep(PartitionChunkOffset::from(relative_offset)) {
                    out.insert(*tx_offset);
                    break;
                }
            }
        }
        Ok(out)
    }

    /// `sync_in_progress` carries `()`. A panic while it is held does not
    /// leave that state inconsistent, so a poisoned guard is recovered.
    /// `into_inner` alone leaves the flag set, and a later `try_lock` would
    /// still report poison. The caller still has to finish or retry the
    /// work that panicked.
    fn lock_sync(&self) -> std::sync::MutexGuard<'_, ()> {
        self.sync_in_progress.clear_poison();
        self.sync_in_progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record whether `sync_in_progress` is already held.
    ///
    /// A free lock is taken and dropped here, before the real wait. Arm the
    /// probe only while another caller holds the lock, or from the thread
    /// that takes it next, so this try does not hand the mutex to someone else.
    #[cfg(test)]
    fn note_sync_lock_probe(&self) {
        if !self.sync_lock_probe.load(Ordering::SeqCst) {
            return;
        }
        self.sync_lock_blocked
            .store(self.sync_lock_is_held(), Ordering::SeqCst);
    }

    /// `true` only when another caller holds `sync_in_progress`.
    ///
    /// A free poisoned lock is not held: `lock_sync` clears that poison.
    /// This does not wait, so it is safe while `pending_writes` is held.
    /// The durable point takes `sync_in_progress` first.
    fn sync_lock_is_held(&self) -> bool {
        match self.sync_in_progress.try_lock() {
            Ok(guard) => {
                drop(guard);
                false
            }
            Err(std::sync::TryLockError::WouldBlock) => true,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                drop(poisoned.into_inner());
                false
            }
        }
    }

    /// Bump the index generation, wait out ops that captured the old one, and
    /// drop queued bytes. Lock order: `pending_writes`, then `entropy_cache`.
    fn clear_pending_for_reset(&self) {
        let mut pending = self.pending_writes.write().unwrap();
        self.index_write_generation.fetch_add(1, Ordering::SeqCst);
        for (_, submodule) in self.submodules.iter() {
            submodule.index_drain.wait_idle();
        }
        pending.chunks.clear();
        pending.occupancy.clear();
        pending.priorities.clear();
        pending.queued_at.clear();
        pending.pwrite_at.clear();
        pending.proofs.clear();
        pending.written_ahead.clear();
        self.entropy_cache.lock().unwrap().clear();
    }

    /// Erase the index, then mark every offset `Uninitialized`.
    ///
    /// Holds `sync_in_progress` for the whole call, and takes it before
    /// `pending_writes`. A durable point takes that lock first and copies
    /// proofs out of `pending`. The clear must not run in that gap: the
    /// copied rows would land in the empty index. Interval files are written
    /// here. Waiting on the lane deadlocks, because the lane takes
    /// `sync_in_progress` to flush. [`Self::interval_image_epoch`] drops a
    /// rewrite that snapshotted the map before this call.
    ///
    /// The metadata marker is durable before the clear. A crash before the
    /// marker is dropped is finished on the next open. The in-memory map
    /// flips when the clear commits, so this process does not serve the old
    /// bytes while the file still names them `Data`.
    ///
    /// This does not change the assignment. A caller that resets because the
    /// assignment changes must use [`Self::reset_with_assignment`], which
    /// stores the new assignment before it releases the assignment lock.
    /// After `reset` alone, the previous assignment is still visible.
    pub fn reset(&self) -> eyre::Result<Interval<PartitionChunkOffset>> {
        self.wipe_for_reset(true)
    }

    /// Wipe the module. `drop_marker` is false when the caller still has to
    /// store an assignment: the marker stays until that store succeeds, so a
    /// failed store leaves the wipe unfinished and a later matching assignment
    /// can pack it.
    fn wipe_for_reset(&self, drop_marker: bool) -> eyre::Result<Interval<PartitionChunkOffset>> {
        #[cfg(test)]
        self.note_sync_lock_probe();
        let _sync_guard = self.lock_sync();
        #[cfg(any(test, feature = "test-utils"))]
        if self.reset_fail_next.swap(false, Ordering::SeqCst) {
            self.reset_incomplete.store(true, Ordering::SeqCst);
            eyre::bail!("injected storage module reset failure");
        }
        self.reset_incomplete.store(true, Ordering::SeqCst);
        // The marker commits before pending is cleared. A failed marker
        // write returns here, so `written_ahead` still fences parked bytes.
        // The live label stays `Data`: a later presence commit must record
        // the bytes that are still parked, not `Interrupted`.
        for (_, submodule) in self.submodules.iter() {
            #[cfg(test)]
            self.inject_sync_failure(SyncFailurePoint::ResetMarkerWrite)?;
            submodule.db.set_reset_in_progress()?;
        }
        self.clear_pending_for_reset();
        // A recall may already have queued a suspect against the old bytes.
        // Confirming it after this clear would judge the wiped module.
        // Repairs belong to those bytes. The next assignment must not repack them.
        recover_mutex(&self.crc_suspects).clear();
        recover_mutex(&self.crc_repairs).clear();
        self.fail_queued_sweeps();
        // A body write can occupy after the clear above and before the
        // interval map flips. The test freezes that window here.
        #[cfg(test)]
        self.wait_if_sync_paused(SyncPausePoint::AfterResetPendingClear);

        #[cfg(test)]
        self.inject_sync_failure(SyncFailurePoint::AfterResetMarker)?;

        // The marker is durable before this flag clear. A crash before the
        // marker leaves `crc_complete` set, so an intact module stays strict
        // and open does not scan it. A crash after this clear is finished by
        // `finish_pending_resets`, which clears the flag again and wipes.
        self.clear_chunk_crc_complete()?;

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
        // A body drain can pass the first clear, pack, and insert `Data`
        // after this map is `Uninitialized`. `commit_packed` does not read
        // the map. Drop that insert. Leave `data_writes_paused` alone: a
        // caller that paused before reset still resumes.
        self.clear_pending_for_reset();
        self.fail_queued_sweeps();
        #[cfg(test)]
        self.inject_sync_failure(SyncFailurePoint::AfterResetIntervals)?;

        if drop_marker {
            self.clear_reset_marker_in_sync()?;
        }
        Ok(storage_interval)
    }

    /// Drop the reset marker. The caller holds `sync_in_progress`.
    fn clear_reset_marker_in_sync(&self) -> eyre::Result<()> {
        for (_, submodule) in self.submodules.iter() {
            submodule.db.clear_reset_in_progress()?;
        }
        self.reset_incomplete.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Drop the reset marker after the assignment store has succeeded.
    fn drop_reset_marker(&self) -> eyre::Result<()> {
        let _sync_guard = self.lock_sync();
        self.clear_reset_marker_in_sync()
    }

    /// Clear the in-memory reset flag when no durable marker exists.
    ///
    /// Returns `Ok(true)` when the flag is cleared and nothing was wiped.
    /// Open recovery does not wipe in that case. Returns `Ok(false)` when a
    /// submodule still has the marker: the caller must finish the clear.
    /// A read error leaves the flag set.
    ///
    /// Holds `sync_in_progress`, the same lock as [`Self::reset`], so this
    /// cannot observe a marker that `reset` is still writing.
    pub fn clear_reset_incomplete_without_marker(&self) -> eyre::Result<bool> {
        let _sync_guard = self.lock_sync();
        for (_, submodule) in self.submodules.iter() {
            if submodule.db.reset_in_progress()? {
                return Ok(false);
            }
        }
        self.reset_incomplete.store(false, Ordering::SeqCst);
        Ok(true)
    }

    /// Reset the index and install `next` before a registration can see the
    /// cleared module under the old assignment.
    ///
    /// Lock order, outer to inner: `partition_assignment` write, then
    /// `sync_in_progress` (inside [`Self::reset`]), then `pending_writes`
    /// and the interval map. A registration holds the assignment read lock
    /// only across its equality check and commit, and it does not take
    /// `sync_in_progress`. No holder of `sync_in_progress` takes
    /// `partition_assignment`. Readers drop the assignment read lock before
    /// `pending_writes`, `intervals`, or `sync_in_progress`.
    /// `write_entropy_chunk` holds the assignment read lock, then the
    /// interval read lock, across its pending insert, so reset cannot
    /// clear `pending` and then lose it.
    ///
    /// `next` of `None` clears the assignment. A failed reset leaves the
    /// previous assignment in place. A wipe that commits and then fails to
    /// store `next` leaves the marker set, so the span can still be packed.
    pub fn reset_with_assignment(
        &self,
        next: Option<PartitionAssignment>,
        update_height: u64,
    ) -> eyre::Result<Interval<PartitionChunkOffset>> {
        let mut assignment = self.partition_assignment.write().unwrap();
        let interval = self.wipe_for_reset(false)?;
        self.store_assignment(&mut assignment, next, update_height)?;
        self.drop_reset_marker()?;
        Ok(interval)
    }

    /// Reset to capacity when `expired_hash` is still assigned and every
    /// submodule file was stored before `notice_height`.
    ///
    /// The assignment write lock covers the read and the reset. A file stored
    /// at or after the notice stays: that write is this epoch's ledger slot,
    /// or the capacity assignment another writer already stored. A different
    /// hash stays too. `Ok(None)` means the module was not reset. A read
    /// error is `Err` and does not reset. The capacity assignment is stored
    /// before the lock is released. `reset` returning `Ok` is not enough:
    /// `Err` after that means the capacity assignment was not stored.
    pub fn reset_expired_to_capacity(
        &self,
        expired_hash: PartitionHash,
        notice_height: u64,
    ) -> eyre::Result<Option<Interval<PartitionChunkOffset>>> {
        let mut assignment = self.partition_assignment.write().unwrap();
        let Some(current) = *assignment else {
            return Ok(None);
        };
        if current.partition_hash != expired_hash {
            return Ok(None);
        }
        // Read every file before `reset`. One unreadable file must not clear
        // the index. The lock keeps a storage-task install from landing
        // between this read and the capacity store.
        let stored = self.read_submodule_packing_params()?;
        let still_expired = stored.iter().all(|params| {
            params.partition_hash == Some(expired_hash)
                && params
                    .last_updated_height
                    .is_none_or(|height| height < notice_height)
        });
        if !still_expired {
            return Ok(None);
        }
        let capacity = PartitionAssignment {
            partition_hash: expired_hash,
            miner_address: current.miner_address,
            ledger_id: None,
            slot_index: None,
        };
        let interval = self.wipe_for_reset(false)?;
        self.store_assignment(&mut assignment, Some(capacity), notice_height)?;
        self.drop_reset_marker()?;
        Ok(Some(interval))
    }

    /// Packing params for every submodule. The caller holds the assignment
    /// write lock. An empty module or one bad file fails the read.
    fn read_submodule_packing_params(&self) -> eyre::Result<Vec<PackingParams>> {
        let mut stored = Vec::new();
        for (_, submodule) in self.submodules.iter() {
            stored.push(PackingParams::from_toml(
                submodule.path.join(PACKING_PARAMS_FILE_NAME),
            )?);
        }
        if stored.is_empty() {
            eyre::bail!("storage module has no submodule");
        }
        Ok(stored)
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

    /// Wake the disk lane to drain pending chunk runs.
    ///
    /// Does not force a short tail out. Shutdown, recovery, and an idle module
    /// use [`Self::force_sync_pending_chunks`].
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
    pub fn force_sync_pending_chunks(&self) -> eyre::Result<()> {
        // Deposited bodies are not in the packed map yet. Pack them first so
        // the flush below persists the bytes the caller asked to finish.
        self.drain_entropy_queue();
        self.sync_pending_chunks_inner(true)?;
        // A reread blocked on the rows just committed is still only queued.
        // The drain above returned without reading it. Pack that body and
        // flush it before this call reports the module durable. One pass: a
        // write that lands during the sync can block the slot again.
        self.sync_queued_after_force()
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

    /// Flush batches the derived ceiling holds per submodule: one the storage
    /// service is flushing and one the worker is filling.
    const PENDING_MEMORY_CEILING_BATCHES: u64 = 2;

    /// Floor on that derived ceiling, in chunks per submodule. A small
    /// `num_writes_before_sync` must not throttle the worker to a couple of
    /// chunks per flush tick.
    const PENDING_MEMORY_CEILING_MIN_CHUNKS: u64 = 256;

    /// Bytes one module may hold in `pending_writes` before body migration
    /// stops writing to it. Packing and the durable-point headroom use the
    /// same value. A configured cap is floored at one flush batch so a small
    /// cap cannot stall that worker on the idle flush.
    pub fn pending_memory_ceiling_bytes(&self) -> u64 {
        let storage = &self.config.node_config.storage;
        let submodules = self.submodule_count() as u64;
        let chunk = self.config.consensus.chunk_size;
        let one_batch = storage
            .num_writes_before_sync
            .saturating_mul(chunk)
            .saturating_mul(submodules);
        match storage.max_pending_write_bytes {
            Some(configured) => configured.max(one_batch),
            None => Self::PENDING_MEMORY_CEILING_BATCHES
                .saturating_mul(storage.num_writes_before_sync)
                .max(Self::PENDING_MEMORY_CEILING_MIN_CHUNKS)
                .saturating_mul(chunk)
                .saturating_mul(submodules),
        }
    }

    fn load_last_durable_at(&self) -> Option<Instant> {
        *recover_mutex(&self.last_durable_at)
    }

    fn stamp_durable_point(&self) {
        *recover_mutex(&self.last_durable_at) = Some(Instant::now());
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
        let _sync_guard = self.lock_sync();
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
                let pwrite_at = pending.pwrite_at.remove(offset);
                pending.priorities.remove(offset);
                pending.written_ahead.insert(
                    *offset,
                    WrittenAhead {
                        bytes,
                        chunk_type,
                        priority: *priority,
                        queued_at,
                        pwrite_at,
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
        let index_started = Instant::now();
        let uncovered = self.commit_presence_rows(&durable_batch)?;
        // fdatasync has returned and no chunk command is in the kernel.
        // This commit is HDD-idle time on the lane thread.
        if self.disk.no_chunk_command() {
            index_read_metrics::note_disk_idle("durable_point", index_started.elapsed());
        }
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
                let pwrite_at = pending.pwrite_at.remove(chunk_offset);
                let priority = pending.priorities.remove(chunk_offset);
                pending.remove(chunk_offset);
                pending.queued_at.remove(chunk_offset);
                pending.proofs.remove(chunk_offset);
                if priority == Some(disk_lane::WritePriority::Ingress)
                    && let Some(pwrite_at) = pwrite_at
                {
                    index_read_metrics::note_ingest_stage("pwrite_to_durable", pwrite_at.elapsed());
                }
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
                let parked = pending
                    .written_ahead
                    .remove(chunk_offset)
                    .expect("matched above");
                pending.proofs.remove(chunk_offset);
                if parked.priority == disk_lane::WritePriority::Ingress
                    && let Some(pwrite_at) = parked.pwrite_at
                {
                    index_read_metrics::note_ingest_stage("pwrite_to_durable", pwrite_at.elapsed());
                }
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
        // Several callers can flush at once. This lock keeps `pending_writes`
        // the durability fence until the commit finishes.
        let _sync_guard = self.lock_sync();

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
    /// covering interval is not stored, and neither is its checksum. A data
    /// snapshot whose queued body no longer matches, and that has no proof,
    /// is not stored either: the caller puts that offset back to
    /// `Uninitialized` and leaves the new body queued. `write_chunk` of
    /// `Data` does not queue a proof; when the queued body still matches,
    /// that snapshot completes as `Data`. MDBX stores proofs only. The
    /// returned offsets were not stored.
    fn commit_presence_rows(
        &self,
        write_batch: &[(
            PartitionChunkOffset,
            (ChunkBytes, ChunkType),
            disk_lane::WritePriority,
        )],
    ) -> eyre::Result<Vec<PartitionChunkOffset>> {
        let mut by_start: BTreeMap<PartitionChunkOffset, Vec<PackedOffsetRow>> = BTreeMap::new();
        let mut replaced = Vec::new();
        // Entropy rows carry no proof. A data row from `write_chunk` also
        // has no proof, and that row must keep the path already stored.
        let mut entropy_offsets: BTreeSet<PartitionChunkOffset> = BTreeSet::new();
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
                // The bytes on disk are this snapshot. A queued body that no
                // longer matches them, with no proof left, must not complete
                // as Data. The new body stays queued.
                if *chunk_type == ChunkType::Data
                    && proof.is_none()
                    && !Self::queued_body_matches(&pending, *offset, bytes, *chunk_type)
                {
                    replaced.push(*offset);
                    continue;
                }
                if *chunk_type == ChunkType::Entropy {
                    entropy_offsets.insert(*offset);
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
        let mut uncovered = replaced;
        for (start, rows) in by_start {
            let (_, submodule) = self.get_submodule_for_offset(start)?;
            // MDBX-SUBMODULE-INDEX: delete this arm. MDBX stores proofs only and drops an entropy path.
            if !submodule.db.stores_offset_crc() {
                // MDBX stores proofs only. An entropy row has no proof.
                // Leaving the previous data path would serve those bytes
                // under it. Drop that path. A data row with no proof is
                // `write_chunk`: it does not replace the stored path. A data
                // row that has a proof still commits it. The tx interval stays.
                let drops: Vec<(PartitionChunkOffset, PartitionChunkOffset)> = rows
                    .iter()
                    .filter(|row| entropy_offsets.contains(&row.offset))
                    .map(|row| (row.offset, row.offset))
                    .collect();
                if !drops.is_empty() {
                    submodule.db.drop_presence_in_ranges(&drops)?;
                }
                let proved: Vec<_> = rows.into_iter().filter(|row| row.proof.is_some()).collect();
                if proved.is_empty() {
                    continue;
                }
                let missed = submodule
                    .db
                    .update(|tx| tx.write_packed_offset_rows(proved))?;
                uncovered.extend(missed);
                continue;
            }
            // A written checksum can close a gap only while some checksum
            // submodule is still incomplete, and only at or after the
            // recorded hole in that submodule. A write before the hole
            // stores `crc: Some` and does not open a hole in the prefix, so
            // the latch stays set. No recorded hole: any checksum write may
            // settle the module. Once every such submodule is complete, a
            // write does not open a gap, so the latch stays set.
            // `clear_chunk_crc_complete` arms the next scan.
            let scan_still_open = self.submodules.iter().any(|(_, submodule)| {
                submodule.db.stores_offset_crc() && !submodule.crc_complete.load(Ordering::SeqCst)
            });
            if !rows.is_empty()
                && scan_still_open
                && self.checksum_write_can_close_crc_gap(start, &rows)
            {
                self.crc_scan_done.store(false, Ordering::SeqCst);
            }
            let missed = submodule
                .db
                .update(|tx| tx.write_packed_offset_rows(rows))?;
            uncovered.extend(missed);
        }
        Ok(uncovered)
    }

    /// Write each submodule's slice of the live interval map.
    /// An empty slice is stored as one `Uninitialized` span covering that submodule.
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
        recover_mutex(&self.interval_disk)
    }

    fn load_interval_image_epoch(&self) -> u64 {
        self.interval_image_epoch.load(Ordering::SeqCst)
    }

    fn lock_interval_publish(&self) -> std::sync::MutexGuard<'_, ()> {
        recover_mutex(&self.interval_publish)
    }

    fn load_crc_cut_epoch(&self) -> u64 {
        self.crc_cut_epoch.load(Ordering::SeqCst)
    }

    fn load_publish_generation(&self) -> u64 {
        self.publish_generation.load(Ordering::SeqCst)
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

    /// `false` when a checksum cut published after this snapshot was copied.
    /// The caller must not commit the pre-cut image or clear `dirty`.
    fn crc_cut_snapshot_current(&self, snapshot_crc_epoch: u64) -> bool {
        let current = snapshot_crc_epoch == self.load_crc_cut_epoch();
        if !current {
            debug!("skipping interval rewrite snapshotted before a checksum cut");
        }
        current
    }

    /// `false` when rollback published a newer map after this snapshot was copied.
    /// The caller must not commit the pre-rollback image or clear `dirty`.
    fn publish_snapshot_current(&self, snapshot_publish: u64) -> bool {
        let current = snapshot_publish == self.load_publish_generation();
        if !current {
            debug!("skipping interval rewrite snapshotted before rollback");
        }
        current
    }

    fn interval_image_acceptable(
        &self,
        snapshot_epoch: u64,
        snapshot_crc_epoch: u64,
        snapshot_publish: u64,
    ) -> bool {
        self.interval_snapshot_current(snapshot_epoch)
            && self.crc_cut_snapshot_current(snapshot_crc_epoch)
            && self.publish_snapshot_current(snapshot_publish)
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
    /// [`INTERVAL_WRITE_AHEAD_BYTES`] of following `Uninitialized` in the
    /// same submodule. That ahead range is not written
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
        // Read both epochs and the publish generation before the map. A later
        // reset fails this snapshot instead of letting it put the old mask
        // back. A checksum cut fails it the same way, without using
        // `interval_image_epoch`: that bump would take the success return
        // below and skip this `Interrupted` mark. The publish generation is
        // not part of that early return for the same reason. The replace
        // further down does consult it.
        let snapshot_epoch = self.load_interval_image_epoch();
        let snapshot_crc_epoch = self.load_crc_cut_epoch();
        let snapshot_publish = self.load_publish_generation();
        // The file already says Interrupted for a covered batch. Return before
        // cloning the interval map: that clone is the whole fragment list, and
        // this runs on the durable-point path. The lock is not held across the
        // clone below.
        {
            let disk = self.lock_interval_disk();
            if !self.interval_snapshot_current(snapshot_epoch)
                || Self::runs_covered(&disk.mask, runs)
            {
                return Ok(());
            }
        }
        let memory = recover_read(&self.intervals).clone();
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
        let needs_write = self.projection_differs(&disk.written, projection.as_ref(), touched);
        if !needs_write {
            // The file already has these Interrupted labels. Remember them so
            // the next batch inside the span does not rewrite.
            disk.mask = mask;
            return Ok(());
        }
        drop(disk);
        self.write_interval_image(
            projection.as_ref(),
            Some(touched),
            snapshot_epoch,
            snapshot_crc_epoch,
            snapshot_publish,
            false,
        )?;
        let mut disk = self.lock_interval_disk();
        // The replace already dropped the publish lock. A cut that landed
        // after it rewrote the file; installing this mask would claim those
        // spans are already `Interrupted`.
        if self.interval_image_acceptable(snapshot_epoch, snapshot_crc_epoch, snapshot_publish) {
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
        let snapshot_crc_epoch = self.load_crc_cut_epoch();
        let snapshot_publish = self.load_publish_generation();
        // Due-ness does not need the interval map. A lane pass with nothing
        // due must not clone that map: after a crash it is the fragment list.
        let due_starts = {
            let disk = self.lock_interval_disk();
            if !self.interval_snapshot_current(snapshot_epoch) {
                return Ok(());
            }
            Self::due_completion_starts(&disk, &self.submodules, period, force)
        };
        if due_starts.is_empty() {
            return Ok(());
        }
        let memory = recover_read(&self.intervals).clone();
        let disk = self.lock_interval_disk();
        if !self.interval_snapshot_current(snapshot_epoch) {
            return Ok(());
        }
        // A reset can clear `dirty` while the map is cloned.
        let touched = Self::due_completion_starts(&disk, &self.submodules, period, force);
        if touched.is_empty() {
            return Ok(());
        }
        let projection = Self::project_interval_file(&memory, &disk.mask)?;
        drop(disk);
        // The clone is done and the interval lock is released. A checksum cut
        // can publish before this image reaches the file.
        #[cfg(test)]
        self.wait_if_completion_clone_paused();
        self.write_interval_image(
            &projection,
            Some(&touched),
            snapshot_epoch,
            snapshot_crc_epoch,
            snapshot_publish,
            false,
        )
    }

    fn due_completion_starts(
        disk: &IntervalDisk,
        submodules: &SubmoduleMap,
        period: Duration,
        force: bool,
    ) -> BTreeSet<PartitionChunkOffset> {
        let mut touched = BTreeSet::new();
        for (interval, _) in submodules.iter() {
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
        touched
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
        let snapshot_crc_epoch = self.load_crc_cut_epoch();
        let snapshot_publish = self.load_publish_generation();
        let mut memory = recover_read(&self.intervals).clone();
        let nondurable = self.snapshot_nondurable_offsets();
        Self::overlay_nondurable_as_interrupted(&mut memory, &nondurable);
        // The map copy is done. The file lock is later, inside
        // `write_interval_image`. A test can `reset` in this gap. The pause
        // disarms before it waits, so this call from `reset` does not stop.
        #[cfg(test)]
        self.wait_if_interval_image_paused();
        {
            let disk = self.lock_interval_disk();
            // A snapshot from before `reset` must not clear a mask that reset,
            // or a later write-ahead, already published.
            if !self.interval_snapshot_current(snapshot_epoch) {
                return Ok(());
            }
            if !force_replace && disk.written == memory {
                drop(disk);
                // The file already has this image. Remember the nondurable
                // spans so the next batch inside them does not rewrite.
                self.install_authoritative_mask(
                    &nondurable,
                    snapshot_epoch,
                    snapshot_crc_epoch,
                    snapshot_publish,
                );
                return Ok(());
            }
        }
        self.write_interval_image(
            &memory,
            None,
            snapshot_epoch,
            snapshot_crc_epoch,
            snapshot_publish,
            force_replace,
        )?;
        // The mask means the file already says `Interrupted`. Install it
        // only after the replace commits. A failed replace leaves the old
        // mask, so the next write-ahead still rewrites those spans.
        self.install_authoritative_mask(
            &nondurable,
            snapshot_epoch,
            snapshot_crc_epoch,
            snapshot_publish,
        );
        Ok(())
    }

    fn install_authoritative_mask(
        &self,
        nondurable: &BTreeSet<PartitionChunkOffset>,
        snapshot_epoch: u64,
        snapshot_crc_epoch: u64,
        snapshot_publish: u64,
    ) {
        let mut disk = self.lock_interval_disk();
        if !self.interval_image_acceptable(snapshot_epoch, snapshot_crc_epoch, snapshot_publish) {
            return;
        }
        disk.mask = Self::mask_for_offsets(nondurable);
        disk.dirty.clear();
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
        let snapshot_crc_epoch = self.load_crc_cut_epoch();
        let snapshot_publish = self.load_publish_generation();
        let mut memory = recover_read(&self.intervals).clone();
        let mut nondurable = self.snapshot_nondurable_offsets();
        if let Some(durable_now) = durable_now {
            for offset in durable_now {
                nondurable.remove(offset);
            }
        }
        Self::overlay_nondurable_as_interrupted(&mut memory, &nondurable);
        self.write_interval_image(
            &memory,
            only,
            snapshot_epoch,
            snapshot_crc_epoch,
            snapshot_publish,
            false,
        )
    }

    /// `pending.chunks` and `pending.written_ahead`. Caller released
    /// `intervals` first. This does not take `interval_disk`.
    fn snapshot_nondurable_offsets(&self) -> BTreeSet<PartitionChunkOffset> {
        let pending = recover_read(&self.pending_writes);
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
    /// bumps that epoch after it publishes `Uninitialized`. `snapshot_crc_epoch`
    /// is the checksum-cut epoch read at the same time. `snapshot_publish` is
    /// the publish generation read before the copy. Rollback bumps that
    /// generation with the live-map flip. A replace whose epoch
    /// no longer matches does not write the file and does not change `written`
    /// or the dirty set. `Ok` here is not proof that this image was stored.
    ///
    /// The publish lock covers the replace. A checksum cut holds it across
    /// its own bump and replace, so this snapshot cannot commit afterwards.
    fn write_interval_image(
        &self,
        image: &StorageIntervals,
        only: Option<&BTreeSet<PartitionChunkOffset>>,
        snapshot_epoch: u64,
        snapshot_crc_epoch: u64,
        snapshot_publish: u64,
        force_replace: bool,
    ) -> eyre::Result<()> {
        let _publish = self.lock_interval_publish();
        self.write_interval_image_locked(
            image,
            only,
            snapshot_epoch,
            snapshot_crc_epoch,
            snapshot_publish,
            force_replace,
        )
    }

    fn write_interval_image_locked(
        &self,
        image: &StorageIntervals,
        only: Option<&BTreeSet<PartitionChunkOffset>>,
        snapshot_epoch: u64,
        snapshot_crc_epoch: u64,
        snapshot_publish: u64,
        force_replace: bool,
    ) -> eyre::Result<()> {
        for (submodule_interval, submodule) in self.submodules.iter() {
            if !self.interval_image_acceptable(snapshot_epoch, snapshot_crc_epoch, snapshot_publish)
            {
                return Ok(());
            }
            let start = submodule_interval.start();
            if only.is_some_and(|starts| !starts.contains(&start)) {
                continue;
            }
            let Some(slice) = Self::interval_slice_cow(image, *submodule_interval) else {
                continue;
            };
            let unchanged = !force_replace && {
                let disk = self.lock_interval_disk();
                Self::interval_slice_cow(&disk.written, *submodule_interval).as_ref()
                    == Some(&slice)
            };
            if unchanged {
                let mut disk = self.lock_interval_disk();
                // The epoch check and the dirty-bit clear share this lock.
                // `reset` bumps the epoch under the same lock, so a stale
                // snapshot cannot drop a dirty bit published after the bump.
                if !self.interval_image_acceptable(
                    snapshot_epoch,
                    snapshot_crc_epoch,
                    snapshot_publish,
                ) {
                    return Ok(());
                }
                disk.dirty.remove(&start);
                continue;
            }
            let path = recover_mutex(&submodule.intervals_file);
            // `reset` bumps the epoch before it takes this file lock. A
            // snapshot from before that bump must not replace the new image.
            // A checksum cut bumps `crc_cut_epoch` under the publish lock
            // this caller already holds, so a pre-cut snapshot stops here.
            if !self.interval_image_acceptable(snapshot_epoch, snapshot_crc_epoch, snapshot_publish)
            {
                return Ok(());
            }
            #[cfg(test)]
            self.inject_sync_failure(SyncFailurePoint::BeforeIntervalImageCommit)?;
            let mut file = get_atomic_file(path.clone())?;
            // `file` is a temporary file. `commit` renames it over the
            // original after fsync, then fsyncs the directory.
            file.write_all(serde_json::to_string(slice.as_ref())?.as_bytes())?;
            file.commit()?;
            drop(path);
            let mut disk = self.lock_interval_disk();
            if !self.interval_image_acceptable(snapshot_epoch, snapshot_crc_epoch, snapshot_publish)
            {
                return Ok(());
            }
            Self::splice_interval_slice(&mut disk.written, *submodule_interval, slice.as_ref());
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
            let file_slice = Self::interval_slice_cow(written, *interval);
            let next_slice = Self::interval_slice_cow(projection, *interval);
            if file_slice != next_slice {
                return true;
            }
        }
        false
    }

    fn project_interval_file<'a>(
        memory: &'a StorageIntervals,
        mask: &IntervalMask,
    ) -> eyre::Result<std::borrow::Cow<'a, StorageIntervals>> {
        // An empty mask is the live map. Borrow it. One submodule per
        // partition would otherwise clone the fragment list to copy it.
        if mask.iter().next().is_none() {
            return Ok(std::borrow::Cow::Borrowed(memory));
        }
        let mut projected = memory.clone();
        for interval in mask.iter().copied() {
            let _ = projected.cut(interval);
            projected
                .insert_merge_touching_if_values_equal(interval, ChunkType::Interrupted)
                .map_err(|error| eyre!("failed to overlay interrupted interval: {error:?}"))?;
        }
        Ok(std::borrow::Cow::Owned(projected))
    }

    /// Insert `run`, then walk forward. Stop at `Data` or `Entropy`, at the
    /// submodule boundary, when the budget is spent, or when the next point
    /// is already masked. Do not jump the mask: that would add another
    /// 512 MiB on every later commit and rewrite every gap. The run itself
    /// is still inserted: those bytes are about to change. Following
    /// `Entropy` is not marked. A crash reloads `Interrupted` as
    /// `Uninitialized` and drops placements there, and this run did not write
    /// those bytes. `Uninitialized` ahead is still marked: it has no packed
    /// bytes, and one rewrite covers the next pack.
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
            // Data and Entropy stay labelled. Marking them Interrupted would
            // make a crash reload them as Uninitialized. Data bytes are
            // already stored. Entropy can hold a placement before its bytes
            // are replaced. Uninitialized has no packed bytes.
            if matches!(*chunk_type, ChunkType::Data | ChunkType::Entropy) {
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

    /// The submodule's slice of `image`. Empty overlap becomes one
    /// `Uninitialized` range so the file always spans the submodule.
    /// `None` when clipped intervals overlap.
    ///
    /// A map that already lies inside `range` is that slice. One submodule
    /// per partition hits that path. [`Self::interval_slice`] clones it.
    /// [`Self::interval_slice_cow`] borrows it. Clipping by copying the map
    /// and rebuilding a tree is O(n log n) in the fragment count.
    fn interval_lies_within(
        image: &StorageIntervals,
        range: Interval<PartitionChunkOffset>,
    ) -> bool {
        match (image.iter().next(), image.iter().next_back()) {
            (Some((first, _)), Some((last, _))) => {
                first.start() >= range.start() && last.end() <= range.end()
            }
            _ => false,
        }
    }

    fn interval_slice_cow<'a>(
        image: &'a StorageIntervals,
        range: Interval<PartitionChunkOffset>,
    ) -> Option<std::borrow::Cow<'a, StorageIntervals>> {
        if Self::interval_lies_within(image, range) {
            return Some(std::borrow::Cow::Borrowed(image));
        }
        Self::interval_slice(image, range).map(std::borrow::Cow::Owned)
    }

    fn interval_slice(
        image: &StorageIntervals,
        range: Interval<PartitionChunkOffset>,
    ) -> Option<StorageIntervals> {
        if Self::interval_lies_within(image, range) {
            return Some(image.clone());
        }

        let mut slice = StorageIntervals::new();
        for (interval, chunk_type) in image.overlapping(range) {
            let start = interval.start().max(range.start());
            let end = interval.end().min(range.end());
            slice.insert_strict(ii(start, end), *chunk_type).ok()?;
        }
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
            let path = recover_mutex(&submodule.intervals_file);
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

    /// Merge every submodule `intervals.json` into one map.
    ///
    /// `Interrupted` becomes `Uninitialized`. A file that cannot be read or
    /// parsed returns an error that names that path.
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
                    // One summary below. A line per span stalls open when a
                    // crash left hundreds of thousands of Interrupted points.
                    ChunkType::Interrupted => {
                        interrupted_count += 1;
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
    /// Runs after `Interrupted` is reloaded as `Uninitialized`, and before
    /// group commit or the disk lane. A fully `Data` module does not touch
    /// the index. On a checksum index, an `Entropy` span keeps a checksum-only
    /// row and drops a proof row: that checksum described data bytes, and
    /// this pass does not read `chunks.dat`. `Uninitialized` drops every row.
    /// MDBX and schema v2 drop every non-`Data` row.
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
            // MDBX-SUBMODULE-INDEX: delete this else arm. MDBX drops every non-Data presence row. RocksDB v1 keeps an entropy checksum.
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
            span_count = spans.len(),
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
            // `clear` drops the partition binding and leaves the marker.
            submodule.db.set_chunk_crc_complete(false)?;
            submodule.crc_complete.store(false, Ordering::SeqCst);
            submodule.db.update(|tx| tx.clear())?;
            let path = recover_mutex(&submodule.intervals_file).clone();
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
                    let guard = recover_mutex(&file_arc);
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
        // MDBX-SUBMODULE-INDEX: delete this arm. MDBX has no checksum to reject.
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
                recover_mutex(&self.crc_suspects).insert(offset);
                continue;
            }
            match self.confirm_crc_mismatch(offset, kind, caller)? {
                CrcConfirm::NotConfirmed(bytes) => {
                    chunk_map.insert(offset, (bytes, kind));
                }
                CrcConfirm::Confirmed | CrcConfirm::Unsettled => {}
            }
        }
        Ok(by_offset)
    }

    /// `disk_lane` confirms one suspect offset under `sync_in_progress`.
    fn note_if_crc_mismatch(
        &self,
        offset: PartitionChunkOffset,
        kind: ChunkType,
        caller: &'static str,
    ) -> eyre::Result<CrcConfirm> {
        self.confirm_crc_mismatch(offset, kind, caller)
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
    /// `pending_writes`, then the `chunks.dat` lock, then `intervals` inside
    /// `note_crc_mismatch`. The note runs before this lock drops. A rewrite
    /// cannot commit between the verdict and the live cut.
    fn confirm_crc_mismatch(
        &self,
        offset: PartitionChunkOffset,
        kind: ChunkType,
        caller: &'static str,
    ) -> eyre::Result<CrcConfirm> {
        let _sync_guard = self.lock_sync();
        if self.crc_bytes_are_unsettled(offset) {
            return Ok(CrcConfirm::Unsettled);
        }
        let bytes = self.pread_settled_chunk(offset)?;
        let (_, submodule) = self.get_submodule_for_offset(offset)?;
        // MDBX-SUBMODULE-INDEX: delete the else. MDBX has no checksum, so the value is None.
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
        // MDBX-SUBMODULE-INDEX: delete this arm. MDBX does not confirm a checksum.
        if !submodule.db.stores_offset_crc() {
            return Ok(CrcConfirm::NotConfirmed(bytes));
        }
        let complete = submodule.crc_complete.load(Ordering::SeqCst);
        match chunk_crc::classify(crc, &bytes, complete) {
            chunk_crc::CrcVerdict::Mismatch => {
                self.note_crc_mismatch(offset, kind, caller)?;
                Ok(CrcConfirm::Confirmed)
            }
            _ => Ok(CrcConfirm::NotConfirmed(bytes)),
        }
    }

    /// Re-read a proof that has no checksum, under `sync_in_progress`.
    ///
    /// The caller loaded that proof before `yield_to_writes`. Serve the pair
    /// read while this lock is held. No proof means the row is checksum-only,
    /// so omit it. A mismatch is omitted too. The checksum in a served pair
    /// is the one committed with those bytes. The caller counts unknown only
    /// when that checksum is still absent. A missing checksum is not queued
    /// as a repair.
    ///
    /// Lock order matches `finish_durable_point`: this lock, then
    /// `pending_writes`, then `chunks.dat`.
    fn reload_unchecksummed_chunk(
        &self,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<
        Option<(
            ChunkBytes,
            DataRoot,
            u64,
            Base64,
            TxChunkOffset,
            Option<u32>,
        )>,
    > {
        let _sync_guard = self.lock_sync();
        if self.crc_bytes_are_unsettled(offset) {
            return Ok(None);
        }
        let bytes = self.pread_settled_chunk(offset)?;
        if self.crc_bytes_are_unsettled(offset) {
            return Ok(None);
        }
        let Some((data_root, data_size, data_path, tx_offset, crc)) =
            self.chunk_index_metas(offset, offset)?.remove(&offset)
        else {
            return Ok(None);
        };
        let (_, submodule) = self.get_submodule_for_offset(offset)?;
        let complete = submodule.crc_complete.load(Ordering::SeqCst);
        if matches!(
            chunk_crc::classify(crc, &bytes, complete),
            chunk_crc::CrcVerdict::Mismatch
        ) {
            return Ok(None);
        }
        Ok(Some((
            bytes, data_root, data_size, data_path, tx_offset, crc,
        )))
    }

    /// One `pread` of the bytes the offset checksum describes.
    ///
    /// The caller holds `sync_in_progress`, so no pwrite runs during this read.
    /// Packed writes and the entropy they are checksummed against land in the
    /// write file. A bench split keeps mining recall on the other descriptor.
    fn pread_settled_chunk(&self, offset: PartitionChunkOffset) -> eyre::Result<ChunkBytes> {
        let (file_arc, file_offset, _) = self.locate_io(offset, true)?;
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
            let mut queued = recover_mutex(&self.crc_suspects);
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
            match self.confirm_crc_mismatch(offset, kind, index_read_metrics::RECALL) {
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
        recover_mutex(&self.crc_suspects).extend(retry);
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
    #[cfg(test)]
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
        // MDBX-SUBMODULE-INDEX: delete this arm. MDBX returns Skip because it stores no checksum.
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
    /// the bytes. This rewrites that submodule's `intervals.json` before it
    /// returns, with the ahead mask still applied. A crash reloads
    /// `Uninitialized`, and open drops the presence row and the placement.
    /// Packing repacks the span. The index is rebuilt from the ledger, and
    /// data sync refills the bytes. The bytes were already checksum-bad.
    ///
    /// The caller holds `sync_in_progress`, so a durable point is not between
    /// its map clone and its file replace. A failed replace puts the previous
    /// live label back and leaves the checksum-cut epoch unchanged. The next
    /// settled read can confirm the offset again. The repair is queued only
    /// after the replace commits.
    fn note_crc_mismatch(
        &self,
        offset: PartitionChunkOffset,
        kind: ChunkType,
        caller: &'static str,
    ) -> eyre::Result<()> {
        #[cfg(test)]
        self.wait_if_sync_paused(SyncPausePoint::AfterCrcConfirm);
        chunk_crc::note_mismatch(caller, kind);
        chunk_crc::warn_mismatch(self.id, offset.0);
        let previous = {
            let intervals = self.intervals.read().unwrap();
            intervals.get_at_point(offset).copied()
        };
        {
            let mut intervals = self.intervals.write().unwrap();
            Self::cut_then_insert_interval_if_touching(
                &mut intervals,
                offset,
                ChunkType::Uninitialized,
            );
        }
        // The file replace follows the live cut. A crash after the replace
        // reloads the cut even if this process dies before the repair is queued.
        if let Err(err) = self.publish_crc_cut(offset) {
            // The replace did not commit. Restore the label this cut replaced
            // when it is still the cut, so a writer that landed another label
            // is left alone. The caller holds `sync_in_progress`, which a
            // label update also needs.
            if let Some(previous) = previous {
                let mut intervals = self.intervals.write().unwrap();
                if intervals.get_at_point(offset).copied() == Some(ChunkType::Uninitialized) {
                    Self::cut_then_insert_interval_if_touching(&mut intervals, offset, previous);
                }
            }
            return Err(err);
        }
        recover_mutex(&self.crc_repairs).insert(offset);
        self.entropy_cache.lock().unwrap().invalidate(offset);
        Ok(())
    }

    /// Store the live map for the submodule that holds `offset`.
    ///
    /// Pending and parked offsets stay `Interrupted`. Spans in the ahead mask
    /// stay `Interrupted` too. This does not replace the mask and does not
    /// bump [`Self::interval_image_epoch`]: that bump makes
    /// [`Self::persist_write_ahead`] skip its `Interrupted` mark.
    fn publish_crc_cut(&self, offset: PartitionChunkOffset) -> eyre::Result<()> {
        let snapshot_epoch = self.load_interval_image_epoch();
        let snapshot_publish = self.load_publish_generation();
        let mut memory = recover_read(&self.intervals).clone();
        let nondurable = self.snapshot_nondurable_offsets();
        Self::overlay_nondurable_as_interrupted(&mut memory, &nondurable);
        let mask = self.lock_interval_disk().mask.clone();
        let projection = Self::project_interval_file(&memory, &mask)?;
        let (interval, _) = self.get_submodule_for_offset(offset)?;
        let touched = BTreeSet::from([interval.start()]);
        let _publish = self.lock_interval_publish();
        // The bump is under the same lock as the replace. A completion that
        // cloned before this point fails its epoch check and does not commit.
        // A failed replace rolls the bump back before this lock is released,
        // so that completion does not observe a cut the file does not have.
        let crc_epoch = self.crc_cut_epoch.fetch_add(1, Ordering::SeqCst) + 1;
        if let Err(err) = self.write_interval_image_locked(
            &projection,
            Some(&touched),
            snapshot_epoch,
            crc_epoch,
            snapshot_publish,
            false,
        ) {
            let _ = self.crc_cut_epoch.fetch_sub(1, Ordering::SeqCst);
            return Err(err);
        }
        Ok(())
    }

    /// Contiguous repair ranges. The storage-module service drains these.
    pub fn take_crc_repair_ranges(&self) -> Vec<Interval<PartitionChunkOffset>> {
        let offsets = std::mem::take(&mut *recover_mutex(&self.crc_repairs));
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
        let mut repairs = recover_mutex(&self.crc_repairs);
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
        self.clear_crc_gap();
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
    /// unknown. Reset, a finished crashed reset, and partition recovery clear
    /// the flag and allow another scan. A scan error does not fail the durable
    /// point and does not stick the flag.
    ///
    /// A recorded hole resumes at that offset. The search that records a
    /// later hole starts each probe after the prefix already known to be
    /// present, so that prefix is decoded once.
    fn maybe_mark_crc_complete(&self) {
        if self.crc_scan_done.load(Ordering::SeqCst) {
            return;
        }
        // MDBX-SUBMODULE-INDEX: delete this return. It skips the checksum scan when every submodule index is MDBX.
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
        let resume = self.crc_gap_if_current();
        for (interval, submodule) in self.submodules.iter() {
            // MDBX-SUBMODULE-INDEX: delete this continue. An MDBX index stores no checksum.
            if !submodule.db.stores_offset_crc() {
                continue;
            }
            let sub_start = u64::from(interval.start().0);
            if resume.is_some_and(|(gap_sub, _)| sub_start < gap_sub) {
                continue;
            }
            let scan_start = match resume {
                Some((gap_sub, gap_off)) if gap_sub == sub_start => u32::try_from(gap_off)
                    .ok()
                    .map(PartitionChunkOffset)
                    .filter(|offset| offset.0 >= interval.start().0 && offset.0 <= interval.end().0)
                    .unwrap_or(interval.start()),
                _ => interval.start(),
            };
            #[cfg(test)]
            {
                self.crc_span_scans.fetch_add(1, Ordering::SeqCst);
                self.crc_scan_from
                    .store(u64::from(scan_start.0), Ordering::SeqCst);
            }
            match submodule
                .db
                .view(|tx| tx.offset_crc_missing(scan_start, interval.end()))
            {
                Ok(true) => {
                    if let Some(gap) =
                        Self::first_missing_checksum(&submodule.db, scan_start, interval.end())
                    {
                        self.store_crc_gap(sub_start, u64::from(gap.0));
                    }
                    self.crc_scan_done.store(true, Ordering::SeqCst);
                    return;
                }
                Ok(false) => {
                    if resume.is_some_and(|(gap_sub, _)| gap_sub == sub_start) {
                        self.clear_crc_gap();
                    }
                }
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
            // MDBX-SUBMODULE-INDEX: delete this continue. An MDBX index stores no checksum.
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
        self.clear_crc_gap();
        self.crc_scan_done.store(true, Ordering::SeqCst);
    }

    fn clear_crc_gap(&self) {
        self.crc_gap_submodule.store(u64::MAX, Ordering::SeqCst);
        self.crc_gap_offset.store(u64::MAX, Ordering::SeqCst);
    }

    fn store_crc_gap(&self, submodule_start: u64, offset: u64) {
        self.crc_gap_submodule
            .store(submodule_start, Ordering::SeqCst);
        self.crc_gap_offset.store(offset, Ordering::SeqCst);
    }

    /// Recorded hole, when that submodule still covers the offset.
    fn crc_gap_if_current(&self) -> Option<(u64, u64)> {
        let submodule_start = self.crc_gap_submodule.load(Ordering::SeqCst);
        let offset = self.crc_gap_offset.load(Ordering::SeqCst);
        if submodule_start == u64::MAX || offset == u64::MAX {
            return None;
        }
        let covers = self.submodules.iter().any(|(interval, submodule)| {
            submodule.db.stores_offset_crc()
                && u64::from(interval.start().0) == submodule_start
                && offset >= u64::from(interval.start().0)
                && offset <= u64::from(interval.end().0)
        });
        covers.then_some((submodule_start, offset))
    }

    /// A checksum write closes the recorded hole only at or after it, in
    /// the same submodule. No recorded hole: any checksum write may settle
    /// the module.
    fn checksum_write_can_close_crc_gap(
        &self,
        submodule_start: PartitionChunkOffset,
        rows: &[PackedOffsetRow],
    ) -> bool {
        let Some((gap_sub, gap_off)) = self.crc_gap_if_current() else {
            return true;
        };
        u64::from(submodule_start.0) == gap_sub
            && rows.iter().any(|row| u64::from(row.offset.0) >= gap_off)
    }

    /// First offset in `[start, end]` with no checksum.
    ///
    /// The span scan already walked to the hole. This records where the
    /// next scan starts. A present prefix is not decoded again: the next
    /// probe begins at the first offset the previous probe did not cover.
    fn first_missing_checksum(
        index: &SubmoduleIndex,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> Option<PartitionChunkOffset> {
        let missing = |from: PartitionChunkOffset, to: PartitionChunkOffset| -> Option<bool> {
            index.view(|tx| tx.offset_crc_missing(from, to)).ok()
        };
        if missing(start, start)? {
            return Some(start);
        }
        let mut lo = u64::from(start.0).saturating_add(1);
        let mut hi = u64::from(end.0);
        if lo > hi {
            return None;
        }
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let from = PartitionChunkOffset(u32::try_from(lo).ok()?);
            let to = PartitionChunkOffset(u32::try_from(mid).ok()?);
            if missing(from, to)? {
                hi = mid;
            } else {
                lo = mid.saturating_add(1);
            }
        }
        let found = PartitionChunkOffset(u32::try_from(lo).ok()?);
        missing(found, found)?.then_some(found)
    }

    #[cfg(test)]
    fn abandon_on_drop_for_test(&self) {
        self.abandon_on_drop.store(true, Ordering::SeqCst);
    }

    /// Settled scans that called `offset_crc_missing`. One span is one call.
    #[cfg(test)]
    fn crc_span_scan_count(&self) -> u64 {
        self.crc_span_scans.load(Ordering::SeqCst)
    }

    /// Start offset of the last counted span scan.
    #[cfg(test)]
    fn crc_scan_start_for_test(&self) -> Option<u32> {
        let raw = self.crc_scan_from.load(Ordering::SeqCst);
        u32::try_from(raw).ok()
    }

    #[cfg(test)]
    fn chunk_crc_complete_for_test(&self) -> bool {
        let mut any = false;
        for (_, submodule) in self.submodules.iter() {
            // MDBX-SUBMODULE-INDEX: delete this continue. An MDBX index stores no checksum.
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

    /// `true` when any offset in the inclusive span has no checksum.
    #[cfg(test)]
    fn offset_crc_missing_for_test(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
    ) -> eyre::Result<bool> {
        let anchor = if start <= end { start } else { end };
        let (_, submodule) = self.get_submodule_for_offset(anchor)?;
        submodule.db.view(|tx| tx.offset_crc_missing(start, end))
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

    /// One chunk from the submodule file that holds `chunk_offset`.
    #[cfg(test)]
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
                match self.confirm_crc_mismatch(chunk_offset, kind, caller)? {
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

    /// Intervals of `chunk_type`, ascending, with pending writes applied.
    ///
    /// Touching matches merge. The stored map is scanned once. Each pending
    /// offset is a binary search. Pending is a bounded batch. Rebuilding a
    /// tree is O(n log n) in the fragment count.
    pub fn get_intervals(&self, chunk_type: ChunkType) -> Vec<Interval<PartitionChunkOffset>> {
        let intervals = self.intervals.read().unwrap();
        let mut spans = self.intervals_matching(&intervals, chunk_type);
        drop(intervals);
        if chunk_type == ChunkType::Interrupted {
            return spans;
        }

        // Pending is applied after the interval lock is released. The other
        // order deadlocks with paths that hold pending and then intervals.
        let pending = self
            .pending_writes
            .read()
            .expect("to be able to read pending writes data");

        match chunk_type {
            ChunkType::Entropy => {
                // Add pending entropy, then cut pending data. One offset has
                // one pending type, so the two passes do not fight.
                for (offset, (_, pending_type)) in pending.iter() {
                    if *pending_type == ChunkType::Entropy {
                        Self::insert_interval_point(&mut spans, *offset);
                    }
                }
                for (offset, (_, pending_type)) in pending.iter() {
                    if *pending_type == ChunkType::Data {
                        Self::cut_interval_point(&mut spans, *offset);
                    }
                }
            }
            ChunkType::Data => {
                for (offset, (_, pending_type)) in pending.iter() {
                    if *pending_type == ChunkType::Data {
                        Self::insert_interval_point(&mut spans, *offset);
                    }
                }
            }
            ChunkType::Uninitialized => {
                for offset in pending.keys() {
                    Self::cut_interval_point(&mut spans, *offset);
                }
            }
            ChunkType::Interrupted => {}
        }

        spans
    }

    /// Uninitialized spans that overlap `range`, clipped to it.
    ///
    /// The stored map is walked only for that overlap. Pending is applied
    /// after the interval lock drops: a queued body is not uninitialized.
    pub fn uninitialized_spans_overlapping(
        &self,
        range: Interval<PartitionChunkOffset>,
    ) -> Vec<Interval<PartitionChunkOffset>> {
        let intervals = self.intervals.read().unwrap();
        let mut spans: Vec<Interval<PartitionChunkOffset>> = Vec::new();
        for (interval, stored) in intervals.overlapping(range) {
            self.note_interval_visit();
            if *stored != ChunkType::Uninitialized {
                continue;
            }
            let start = PartitionChunkOffset(interval.start().0.max(range.start().0));
            let end = PartitionChunkOffset(interval.end().0.min(range.end().0));
            if start.0 > end.0 {
                continue;
            }
            if let Some(last) = spans.last_mut()
                && Self::spans_touch(last.end(), start)
            {
                let merged_start = last.start();
                let merged_end = PartitionChunkOffset(last.end().0.max(end.0));
                *last = ii(merged_start, merged_end);
            } else {
                spans.push(ii(start, end));
            }
        }
        drop(intervals);

        let pending = self
            .pending_writes
            .read()
            .expect("to be able to read pending writes data");
        let range_start = range.start().0;
        let range_end = range.end().0;
        for offset in pending.keys() {
            if offset.0 < range_start || offset.0 > range_end {
                continue;
            }
            Self::cut_interval_point(&mut spans, *offset);
        }
        spans
    }

    /// Live `Uninitialized` spans inside `range`.
    ///
    /// Empty when the module is unassigned or the hash is not `packed_for`.
    /// A queued body is cut, same as [`Self::uninitialized_spans_overlapping`].
    /// This is a hint. [`Self::write_entropy_chunk`] is the guard.
    pub fn entropy_gaps(
        &self,
        packed_for: PartitionHash,
        range: Interval<PartitionChunkOffset>,
    ) -> Vec<Interval<PartitionChunkOffset>> {
        let assignment = self.partition_assignment.read().unwrap();
        if assignment
            .as_ref()
            .map(|assignment| assignment.partition_hash)
            != Some(packed_for)
        {
            return Vec::new();
        }
        self.uninitialized_spans_overlapping(range)
    }

    /// Matching intervals, ascending. Touching equals become one span.
    fn intervals_matching(
        &self,
        intervals: &StorageIntervals,
        chunk_type: ChunkType,
    ) -> Vec<Interval<PartitionChunkOffset>> {
        let mut spans: Vec<Interval<PartitionChunkOffset>> = Vec::new();
        for (interval, stored) in intervals.iter() {
            self.note_interval_visit();
            if *stored != chunk_type {
                continue;
            }
            if let Some(last) = spans.last_mut()
                && Self::spans_touch(last.end(), interval.start())
            {
                let start = last.start();
                let end = last.end().max(interval.end());
                *last = ii(start, end);
            } else {
                spans.push(*interval);
            }
        }
        spans
    }

    /// Inclusive spans touch when they share a point or sit on adjacent points.
    fn spans_touch(left_end: PartitionChunkOffset, right_start: PartitionChunkOffset) -> bool {
        left_end.0 == u32::MAX || left_end.0.saturating_add(1) >= right_start.0
    }

    /// Insert one pending point, merging it with spans it touches.
    fn insert_interval_point(
        spans: &mut Vec<Interval<PartitionChunkOffset>>,
        point: PartitionChunkOffset,
    ) {
        let idx = spans.partition_point(|interval| {
            let end = interval.end().0;
            end != u32::MAX && end.saturating_add(1) < point.0
        });
        if idx == spans.len() {
            spans.push(ii(point, point));
            return;
        }
        let current = spans[idx];
        let gap_before = match point.0.checked_add(1) {
            Some(next) => current.start().0 > next,
            None => false,
        };
        if gap_before {
            spans.insert(idx, ii(point, point));
            return;
        }
        let start = current.start().min(point);
        let mut end = current.end().max(point);
        let absorb_next = spans
            .get(idx + 1)
            .is_some_and(|next| Self::spans_touch(end, next.start()));
        if absorb_next {
            end = end.max(spans[idx + 1].end());
        }
        spans[idx] = ii(start, end);
        if absorb_next {
            spans.remove(idx + 1);
        }
    }

    /// Remove one point from the spans. A miss is a no-op.
    fn cut_interval_point(
        spans: &mut Vec<Interval<PartitionChunkOffset>>,
        point: PartitionChunkOffset,
    ) {
        let idx = spans.partition_point(|interval| interval.end().0 < point.0);
        if idx == spans.len() {
            return;
        }
        let interval = spans[idx];
        if interval.start().0 > point.0 {
            return;
        }
        let start = interval.start().0;
        let end = interval.end().0;
        if start == end {
            spans.remove(idx);
            return;
        }
        if start == point.0 {
            spans[idx] = ii(PartitionChunkOffset(point.0 + 1), interval.end());
            return;
        }
        if end == point.0 {
            spans[idx] = ii(interval.start(), PartitionChunkOffset(point.0 - 1));
            return;
        }
        spans[idx] = ii(interval.start(), PartitionChunkOffset(point.0 - 1));
        spans.insert(
            idx + 1,
            ii(PartitionChunkOffset(point.0 + 1), interval.end()),
        );
    }

    /// Queues chunk data for later disk write. Chunks are batched for efficiency
    /// and written during periodic sync operations.
    pub fn write_chunk(
        &self,
        chunk_offset: PartitionChunkOffset,
        bytes: Vec<u8>,
        chunk_type: ChunkType,
    ) -> bool {
        self.enqueue_pending_chunk(chunk_offset, bytes, chunk_type)
    }

    /// The live interval label. Pending is not this map.
    fn live_label(intervals: &StorageIntervals, offset: PartitionChunkOffset) -> Option<ChunkType> {
        intervals.get_at_point(offset).copied()
    }

    /// Entropy packed for `packed_for`.
    ///
    /// One section, in this order: the assignment read lock, the interval
    /// read lock, then the pending write lock. None of them drop before the
    /// insert or the refusal. The hash must be the assignment. The live label
    /// must be `Uninitialized`. A live `Interrupted` label is the in-flight
    /// durable-point mark. A queued data body, a parked data body, or a proof
    /// blocks the offset even when the durable point has not taken its lock.
    /// A queued entropy body may be replaced.
    ///
    /// [`Self::reset_with_assignment`] takes the assignment write lock before
    /// it clears `pending`, so this insert cannot land bytes for a partition
    /// the module no longer has. A raw [`Self::write_chunk`] of
    /// `ChunkType::Entropy` does not check the assignment or the label. That
    /// path is for bytes that were not packed for a hash.
    pub fn write_entropy_chunk(
        &self,
        chunk_offset: PartitionChunkOffset,
        bytes: Vec<u8>,
        packed_for: PartitionHash,
    ) -> EntropyWrite {
        let assignment = self.partition_assignment.read().unwrap();
        if assignment
            .as_ref()
            .map(|assignment| assignment.partition_hash)
            != Some(packed_for)
        {
            debug!(
                target: "irys::packing",
                module_id = self.id,
                offset = chunk_offset.0,
                %packed_for,
                "refusing entropy packed for a partition this module is not assigned"
            );
            return EntropyWrite::Rejected(EntropyRejection::NotAssigned);
        }
        let intervals = self.intervals.read().unwrap();
        let label = Self::live_label(&intervals, chunk_offset);
        if label != Some(ChunkType::Uninitialized) {
            return EntropyWrite::Rejected(EntropyRejection::NotUninitialized(label));
        }
        let mut pending = self.pending_writes.write().unwrap();
        if Self::queued_data_or_proof(&pending, chunk_offset) {
            return EntropyWrite::Rejected(EntropyRejection::NotUninitialized(Some(
                ChunkType::Data,
            )));
        }
        self.queue_pending_body(&mut pending, chunk_offset, bytes, ChunkType::Entropy);
        drop(pending);
        drop(intervals);
        drop(assignment);
        self.fail_sweep_offset(chunk_offset);
        self.disk.notify();
        EntropyWrite::Queued
    }

    fn enqueue_pending_chunk(
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
        // The durable point holds `sync_in_progress` while the live label is
        // still `Interrupted`. Replacing this offset's queued data body, or
        // its proof, would publish Data for bytes the point no longer owns.
        // Entropy that does not replace data still queues. `write_entropy_chunk`
        // refuses that replacement even when this lock is free.
        if Self::queued_data_or_proof(&pending, chunk_offset) && self.sync_lock_is_held() {
            return false;
        }
        self.queue_pending_body(&mut pending, chunk_offset, bytes, chunk_type);
        drop(pending);
        // A sweep slot may already hold a pad copied before this write.
        // Fail that slot so a later fold cannot XOR the old bytes.
        self.fail_sweep_offset(chunk_offset);
        // The lane writes the next window when it is woken.
        self.disk.notify();
        true
    }

    fn queue_pending_body(
        &self,
        pending: &mut PendingWrites,
        chunk_offset: PartitionChunkOffset,
        bytes: Vec<u8>,
        chunk_type: ChunkType,
    ) {
        let priority = match chunk_type {
            ChunkType::Entropy => disk_lane::WritePriority::Packing,
            _ => disk_lane::WritePriority::Ingress,
        };
        pending.written_ahead.remove(&chunk_offset);
        pending.insert(chunk_offset, (bytes, chunk_type));
        pending.priorities.insert(chunk_offset, priority);
        pending.queued_at.insert(chunk_offset, Instant::now());
        pending.pwrite_at.remove(&chunk_offset);
        // A raw write has no data-path proof. Drop a proof left by a packed
        // chunk this write replaced, so the fsync cannot publish that path.
        pending.proofs.remove(&chunk_offset);
        self.entropy_cache.lock().unwrap().invalidate(chunk_offset);
        *self.last_pending_write.write().unwrap() = Instant::now();
    }

    /// Queued `Data` at `offset`, or a proof waiting for that offset.
    fn queued_data_or_proof(pending: &PendingWrites, offset: PartitionChunkOffset) -> bool {
        let data_body = pending
            .get(&offset)
            .is_some_and(|(_, kind)| *kind == ChunkType::Data)
            || pending
                .written_ahead
                .get(&offset)
                .is_some_and(|ahead| ahead.chunk_type == ChunkType::Data);
        data_body || pending.proofs.contains_key(&offset)
    }

    /// The queued body is still the snapshot this durable point pwrote.
    fn queued_body_matches(
        pending: &PendingWrites,
        offset: PartitionChunkOffset,
        bytes: &[u8],
        chunk_type: ChunkType,
    ) -> bool {
        if let Some((queued, kind)) = pending.get(&offset) {
            return *kind == chunk_type && queued.as_slice() == bytes;
        }
        pending
            .written_ahead
            .get(&offset)
            .is_some_and(|ahead| ahead.chunk_type == chunk_type && ahead.bytes.as_slice() == bytes)
    }

    /// Take the assignment write lock for rollback.
    ///
    /// A registration inside its commit already holds the read lock. The first
    /// `try_write` records that block, then this waits so the clear cannot
    /// start until that commit returns.
    fn lock_assignment_for_rollback(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, Option<irys_types::partition::PartitionAssignment>> {
        #[cfg(test)]
        match self.partition_assignment.try_write() {
            Ok(guard) => {
                self.rollback_assignment_blocked
                    .store(false, Ordering::SeqCst);
                return guard;
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                self.rollback_assignment_blocked
                    .store(true, Ordering::SeqCst);
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                self.rollback_assignment_blocked
                    .store(false, Ordering::SeqCst);
            }
        }
        self.partition_assignment.write().unwrap()
    }

    /// Clear an orphaned partition range and publish `Uninitialized`.
    ///
    /// The `Uninitialized` image is durable before the proof index is cleared.
    /// A crash before that image leaves the previous file and the previous
    /// proofs. A crash after it leaves `Uninitialized`. Open drops any proofs
    /// still stored under that label. The file is not `Data` with an empty
    /// proof index. This holds `sync_in_progress` across the replace and does
    /// not wait on the disk lane: the lane takes that lock to flush. A failed
    /// replace restores the live map and does not clear the index.
    ///
    /// The publish generation bumps in the same intervals-lock section as the
    /// live-map flip. A completion that cloned the map before the bump does
    /// not replace the file and does not clear `dirty`.
    ///
    /// Lock order, outer to inner: `partition_assignment` write, then
    /// `sync_in_progress`, then `intervals` (released before
    /// `interval_publish`). The assignment write lock covers the generation
    /// bump, the file replace, and the index clear. The assignment value
    /// stays. A registration that already holds the read lock finishes its
    /// commit, and this clear then removes those rows. A registration that
    /// has not entered captures the new generation. The assignment is
    /// unchanged, so that storage commit still proceeds. Chunk migration
    /// holds `index_recovery` across the commit and writes only when the
    /// block is still at this height.
    pub fn rollback_unassigned_range(
        &self,
        start: PartitionChunkOffset,
        end: PartitionChunkOffset,
        data_roots: &[H256],
    ) -> eyre::Result<()> {
        let _assignment = self.lock_assignment_for_rollback();
        let _ = self.rollback_generation.fetch_add(1, Ordering::SeqCst);
        #[cfg(test)]
        self.note_sync_lock_probe();
        let _sync = self.lock_sync();
        self.drop_pending_writes_in_range(start, end);

        let previous_generation = self.load_publish_generation();
        let previous_map = {
            let mut guard = self.intervals.write().unwrap();
            let previous = guard.clone();
            // The bump and the flip share this lock. A reader clones either
            // the old map at the old generation or the new map at the new one.
            self.publish_generation.fetch_add(1, Ordering::SeqCst);
            // One label covers the inclusive span. One cut splits the
            // neighbors. The insert merges with touching `Uninitialized`.
            let span = ii(start, end);
            let mut intervals = RollbackSpanEdits {
                map: &mut guard,
                #[cfg(test)]
                edits: &self.rollback_interval_edits,
            };
            intervals.cut(span);
            intervals.insert_merge_touching_if_values_equal(span, ChunkType::Uninitialized);
            previous
        };

        // Read after the bump, after the intervals lock is released.
        let snapshot_epoch = self.load_interval_image_epoch();
        let snapshot_crc_epoch = self.load_crc_cut_epoch();
        let snapshot_publish = self.load_publish_generation();
        let mut memory = recover_read(&self.intervals).clone();
        let nondurable = self.snapshot_nondurable_offsets();
        Self::overlay_nondurable_as_interrupted(&mut memory, &nondurable);
        // Not `write_intervals_to_submodules`: that waits on the lane, and a
        // suppressed flush would skip the replace. `force_replace` still
        // commits when the tracked image already matches.
        if let Err(err) = self.write_interval_image(
            &memory,
            None,
            snapshot_epoch,
            snapshot_crc_epoch,
            snapshot_publish,
            true,
        ) {
            self.restore_rollback_intervals(previous_map, previous_generation);
            return Err(err);
        }
        // `Ok` is also a snapshot that did not replace the file. Do not
        // clear proofs over the previous image.
        if !self.interval_image_acceptable(snapshot_epoch, snapshot_crc_epoch, snapshot_publish) {
            self.restore_rollback_intervals(previous_map, previous_generation);
            eyre::bail!("rollback interval image was not published");
        }
        self.install_authoritative_mask(
            &nondurable,
            snapshot_epoch,
            snapshot_crc_epoch,
            snapshot_publish,
        );
        #[cfg(test)]
        self.inject_sync_failure(SyncFailurePoint::AfterRollbackIntervalImage)?;
        self.clear_offset_index_in_range(start, end)?;
        self.clear_data_root_infos_in_range(start, end, data_roots)?;
        self.purge_pending_body_migrations_in_range(start, end)?;
        Ok(())
    }

    /// Put the pre-rollback map and publish generation back. `store` writes
    /// the captured value. A decrement would move a generation this call did
    /// not own.
    fn restore_rollback_intervals(&self, previous_map: StorageIntervals, previous_generation: u64) {
        let mut guard = self.intervals.write().unwrap();
        *guard = previous_map;
        self.publish_generation
            .store(previous_generation, Ordering::SeqCst);
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
                .pwrite_at
                .retain(|offset, _| *offset < start || *offset > end);
            pending
                .proofs
                .retain(|offset, _| *offset < start || *offset > end);
            pending
                .written_ahead
                .retain(|offset, _| *offset < start || *offset > end);
            self.entropy_cache
                .lock()
                .unwrap()
                .invalidate_range(start, end);
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
                // RocksDB v1 data-root rows belong to their intervals. Clearing
                // the interval removes the row. Trimming the info list here
                // would delete rows whose interval fragments remain.
                if tx.intervals_own_data_root_rows() {
                    return Ok(());
                }
                for data_root in data_roots {
                    let Some(infos) = tx.get_data_root_infos_for_data_root(*data_root)? else {
                        continue;
                    };
                    let mut remaining = Vec::new();
                    for di in infos.0 {
                        // Keep the placement iff its full on-disk extent
                        // [start_offset, start_offset + ceil(data_size / chunk_size) - 1]
                        // does NOT intersect the orphaned range. Matching on
                        // start_offset alone would leave a stale entry for a
                        // placement that begins before the range (e.g. a
                        // cross-partition placement in a higher module, whose
                        // start_offset is negative) but extends into it.
                        let overlaps = placement_overlaps_inclusive_span(
                            &di,
                            start,
                            end,
                            self.config.consensus.chunk_size,
                        )?;
                        if !overlaps {
                            remaining.push(di);
                        }
                    }
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

    /// Index one tx into every overlapping submodule.
    ///
    /// The same transaction records a `PendingBodyMigrationsByOffset` row, so
    /// the body is owed once the index is visible (see
    /// [`Self::settle_pending_body_migration`]). `block_height` is the
    /// canonical block whose migration is indexing the tx.
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
    /// The rows are computed for the assignment and rollback generation
    /// captured at the start. The commit holds the assignment read lock and
    /// writes only when both are still current. A mismatch, including
    /// `None`, drops the rows and returns `Ok`. This path does not take
    /// `sync_in_progress`.
    pub fn index_transaction_data_in(
        &self,
        data_tx: &DataTransactionHeader,
        tx_path: &TxPath,
        chunk_range: LedgerChunkRange,
        block_height: u64,
        ledger: Option<DataLedger>,
        tx_index: Option<u32>,
    ) -> eyre::Result<()> {
        let (captured, captured_generation) = {
            let assignment = self.partition_assignment.read().unwrap();
            let Some(assignment) = *assignment else {
                return Err(eyre::eyre!("Partition not assigned!"));
            };
            let generation = self.rollback_generation.load(Ordering::SeqCst);
            (assignment, generation)
        };
        // Use the captured slot. A later read could see a different slot and
        // back, and the equality check would then commit the wrong rows.
        let storage_range = self.ledger_range_for_assignment(&captured)?;
        let tx_path_hash = H256::from(irys_types::hash_sha256(tx_path));
        let (partition_overlap, start_offset) =
            self.partition_overlap_in(chunk_range, storage_range)?;

        // Rows are computed. The commit has not started. A test can reset
        // and change the assignment in this gap. The wait holds neither
        // `sync_in_progress` nor the assignment lock.
        #[cfg(test)]
        self.wait_if_registration_paused(RegistrationPausePoint::AfterRowsComputed);

        let assignment = self.partition_assignment.read().unwrap();
        let generation = self.rollback_generation.load(Ordering::SeqCst);
        if *assignment != Some(captured) || generation != captured_generation {
            let current = *assignment;
            drop(assignment);
            self.note_registration_dropped(captured, current);
            return Ok(());
        }

        // The read lock is still held. A rollback that needs the write lock
        // waits here, then clears whatever this commit wrote.
        #[cfg(test)]
        self.wait_if_registration_paused(RegistrationPausePoint::DuringCommit);

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
        let start = *(overlap.start() - storage_range.start());
        let end = *(overlap.end() - storage_range.start());
        let partition_overlap = PartitionChunkRange(ii(
            PartitionChunkOffset(
                u32::try_from(start).wrap_err("partition overlap start does not fit in u32")?,
            ),
            PartitionChunkOffset(
                u32::try_from(end).wrap_err("partition overlap end does not fit in u32")?,
            ),
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
        let partition_offset =
            PartitionChunkOffset(self.make_offset_partition_relative_guarded(ledger_offset)?);

        self.generate_full_chunk(partition_offset)
    }

    /// One packed chunk at a partition offset.
    ///
    /// A proof with no checksum is served only with bytes read again while
    /// `sync_in_progress` is held. A later commit must not keep the old proof.
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
    /// chunk the disk does not hold. A checksum mismatch is omitted. A row
    /// with no checksum is served only after a re-read under the sync lock.
    /// A checksum-only row has no proof and is omitted.
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
        // The checksum rows above were loaded before this read. A durable
        // point can commit a new pair in the gap. The match arm re-reads the
        // checksum under `sync_in_progress` before it serves the old bytes.
        #[cfg(test)]
        self.wait_if_sync_paused(SyncPausePoint::AfterServeBodyRead);
        let packing_address = self.config.node_config.miner_address();
        let partition_hash = self
            .partition_hash()
            .ok_or_eyre("storage module has no partition")?;
        let caller = index_read_metrics::caller();
        let mut out = BTreeMap::new();
        for (offset, meta) in metas {
            let Some((mut bytes, kind)) = bodies.remove(&offset) else {
                if start == end {
                    return Err(eyre!("Could not find chunk bytes on disk"));
                }
                continue;
            };
            // Bytes not yet committed with their index row would be served
            // under the previous row's proof.
            if self.crc_bytes_are_unsettled(offset) {
                continue;
            }
            let (mut data_root, mut data_size, mut data_path, mut tx_offset, crc) = meta;
            let (_, submodule) = self.get_submodule_for_offset(offset)?;
            if submodule.db.stores_offset_crc() {
                let complete = submodule.crc_complete.load(Ordering::SeqCst);
                match chunk_crc::classify(crc, &bytes, complete) {
                    chunk_crc::CrcVerdict::Mismatch => {
                        match self.confirm_crc_mismatch(offset, kind, caller)? {
                            // A commit landed after the row was read: serve the
                            // fresh bytes only with the row committed with them.
                            CrcConfirm::NotConfirmed(fresh) => {
                                let Some((root, size, path, tx, fresh_crc)) =
                                    self.chunk_index_metas(offset, offset)?.remove(&offset)
                                else {
                                    continue;
                                };
                                if matches!(
                                    chunk_crc::classify(fresh_crc, &fresh, complete),
                                    chunk_crc::CrcVerdict::Mismatch
                                ) {
                                    continue;
                                }
                                (data_root, data_size, data_path, tx_offset) =
                                    (root, size, path, tx);
                                bytes = fresh;
                            }
                            CrcConfirm::Confirmed | CrcConfirm::Unsettled => continue,
                        }
                    }
                    chunk_crc::CrcVerdict::Unknown => {
                        // The proof above was loaded before `yield_to_writes`.
                        // A commit can replace the bytes while this read waits.
                        // Serve only the pair read while the sync lock is held.
                        // Count unknown only when that pair still has no checksum.
                        // An omitted offset, and a row that now has one, are not
                        // chunks served without a stored checksum.
                        match self.reload_unchecksummed_chunk(offset)? {
                            Some((fresh, root, size, path, tx, fresh_crc)) => {
                                if fresh_crc.is_none() {
                                    chunk_crc::note_unknown(caller);
                                }
                                (data_root, data_size, data_path, tx_offset) =
                                    (root, size, path, tx);
                                bytes = fresh;
                            }
                            None => continue,
                        }
                    }
                    chunk_crc::CrcVerdict::Skip => {}
                    chunk_crc::CrcVerdict::Match => {
                        // The body was copied before this checksum was
                        // checked. A later commit can keep the checksum and
                        // replace the path. Serve these bytes only while
                        // both still match. The lock is held through the
                        // insert, then dropped: the next offset may confirm
                        // or reload, and both take this lock.
                        let sync = self.lock_sync();
                        let row = submodule.db.view(|tx| tx.get_offset_meta(offset))?;
                        let stored = row.as_ref().and_then(|row| row.crc);
                        let path_matches = row.as_ref().and_then(|row| row.proof.as_deref())
                            == Some(data_path.0.as_slice());
                        if stored == crc && path_matches {
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
                            continue;
                        }
                        drop(sync);
                        match self.reload_unchecksummed_chunk(offset)? {
                            Some((fresh, root, size, path, tx, fresh_crc)) => {
                                if matches!(
                                    chunk_crc::classify(fresh_crc, &fresh, complete),
                                    chunk_crc::CrcVerdict::Mismatch
                                ) {
                                    continue;
                                }
                                (data_root, data_size, data_path, tx_offset) =
                                    (root, size, path, tx);
                                bytes = fresh;
                            }
                            None => continue,
                        }
                    }
                }
            } else {
                // The path above was loaded before `yield_to_writes`. This
                // engine stores no checksum, so a later commit can replace
                // the path with no checksum change. Serve only the pair read
                // while `sync_in_progress` is held.
                match self.reload_unchecksummed_chunk(offset)? {
                    Some((fresh, root, size, path, tx, _crc)) => {
                        (data_root, data_size, data_path, tx_offset) = (root, size, path, tx);
                        bytes = fresh;
                    }
                    None => continue,
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
            // covering info still resolves through the leaf binding.
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
                    out[idx] = match placements
                        .get(placement_index)
                        .filter(|placement| placement.start <= cur && placement.end >= cur)
                    {
                        Some(placement) => {
                            Self::tx_offset_in_placement(placement, cur, chunk_size)?
                        }
                        None => None,
                    };
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
    /// the tx begins before this module. A partition offset that does not fit
    /// in `i32` is an error.
    fn tx_offset_in_placement(
        placement: &Placement,
        partition_offset: PartitionChunkOffset,
        chunk_size: u64,
    ) -> eyre::Result<Option<(DataRoot, TxChunkOffset, u64)>> {
        let partition_rel =
            RelativeChunkOffset(i32::try_from(partition_offset.0).map_err(|_| {
                eyre!(
                    "partition offset {} does not fit in i32",
                    partition_offset.0
                )
            })?);
        if placement.tx_start_offset > partition_rel {
            return Ok(None);
        }
        let tx_offset_i32 = *partition_rel - *placement.tx_start_offset;
        if tx_offset_i32 < 0 {
            return Ok(None);
        }
        let tx_offset_u32 = u32::try_from(tx_offset_i32)
            .map_err(|_| eyre!("tx offset {tx_offset_i32} does not fit in u32"))?;
        let chunk_byte_offset = u64::from(tx_offset_u32).saturating_mul(chunk_size);
        if chunk_byte_offset >= placement.data_size {
            return Ok(None);
        }
        Ok(Some((
            placement.data_root,
            TxChunkOffset::from(tx_offset_u32),
            placement.data_size,
        )))
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
        Self::tx_offset_in_placement(&placement, partition_offset, chunk_size)
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

    /// Live labels. Pending writes are not applied. The guard stays held for
    /// the iterator, so a caller that stops early does not read the rest.
    pub fn label_runs(&self) -> LabelRuns<'_> {
        #[cfg(any(test, feature = "test-utils"))]
        let visits = &self.interval_visits;
        LabelRuns {
            guard: self.intervals.read().unwrap(),
            #[cfg(any(test, feature = "test-utils"))]
            visits,
        }
    }

    /// Mutable map for tests and the bench harness. Production reads use
    /// [`Self::label_runs`].
    #[cfg(any(test, feature = "test-utils", feature = "bench"))]
    pub fn intervals(&self) -> &Arc<RwLock<StorageIntervals>> {
        &self.intervals
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn cut_then_insert_interval_if_touching(
        intervals: &mut StorageIntervals,
        chunk_offset: PartitionChunkOffset,
        chunk_type: ChunkType,
    ) {
        let chunk_interval = ii(chunk_offset, chunk_offset);
        let _ = intervals.cut(chunk_interval);
        let _ = intervals.insert_merge_touching_if_values_equal(chunk_interval, chunk_type);
    }

    #[cfg(not(any(test, feature = "test-utils")))]
    fn cut_then_insert_interval_if_touching(
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
        let mut indexed = pending.indexed();
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
        let mut indexed = pending.indexed();
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
    fn plan_ranked_runs<'a>(
        &self,
        batch: impl IntoIterator<
            Item = &'a (
                PartitionChunkOffset,
                (ChunkBytes, ChunkType),
                disk_lane::WritePriority,
            ),
        >,
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
            runs.extend(self.plan_ranked_runs(items, cap));
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

    /// Stamp the pwrite return and record ingress packed-to-pwrite latency.
    /// The intervals lock is not held here.
    fn stamp_pwrite(&self, run: &WriteRun, at: Instant) {
        let mut pending = self.pending_writes.write().unwrap();
        for offset in &run.offsets {
            let priority = pending.priorities.get(offset).copied();
            let queued = pending.queued_at.get(offset).copied();
            pending.pwrite_at.insert(*offset, at);
            self.entropy_cache.lock().unwrap().invalidate(*offset);
            if priority == Some(disk_lane::WritePriority::Ingress)
                && let Some(queued) = queued
            {
                index_read_metrics::note_ingest_stage(
                    "packed_to_pwrite",
                    at.saturating_duration_since(queued),
                );
            }
        }
    }

    fn note_write_run(
        &self,
        run: &WriteRun,
        start_time: Instant,
        write_result: eyre::Result<()>,
    ) -> eyre::Result<()> {
        match write_result {
            Ok(()) => {
                #[cfg(test)]
                self.wait_if_sync_paused(SyncPausePoint::AfterPwrite);
                let mut intervals = self
                    .intervals
                    .write()
                    .map_err(|e| eyre::eyre!("Failed to acquire write lock on intervals: {}", e))?;
                if run.chunk_type == ChunkType::Entropy {
                    self.entropy_write_epoch.fetch_add(1, Ordering::SeqCst);
                }
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
        self.stamp_pwrite(run, completion_time);
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

    /// Test-only positional write. Does not `fsync` and does not rewrite
    /// `intervals.json`. The live map is updated after the write. Callers
    /// that need a durable image use [`Self::sync_pending_chunks_inner`].
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

    /// Calculate write throughput in bytes per second based on chunk records.
    ///
    /// The buffer holds up to 8_000 samples. Only completions inside the last
    /// 10 seconds count. A fast burst that then stops reports 0 once that
    /// window passes, instead of keeping the burst until the count wraps.
    /// Returns 0 if no record in the window is available.
    pub fn write_throughput_bps(&self) -> u64 {
        const WINDOW: Duration = Duration::from_secs(10);
        let chunk_size = self.config.consensus.chunk_size;
        let recent_chunk_times = self.recent_chunk_times.read().unwrap();
        let now = Instant::now();
        let mut count = 0_u64;
        let mut first_start = None;
        let mut last_completion = None;
        for record in recent_chunk_times.iter() {
            if now.saturating_duration_since(record.completion_time) > WINDOW {
                continue;
            }
            if first_start.is_none() {
                first_start = Some(record.start_time);
            }
            last_completion = Some(record.completion_time);
            count += 1;
        }
        let (Some(start_time), Some(completion_time)) = (first_start, last_completion) else {
            tracing::debug!("write_throughput_bps: no sample in the 10s window, returning 0");
            return 0;
        };

        tracing::debug!(
            "write_throughput_bps: window_len={} chunk_size={} front_start={:?} back_completion={:?}",
            count,
            chunk_size,
            start_time,
            completion_time
        );

        // Integer math only. A sub-millisecond span has no duration to divide.
        let time_span = completion_time.duration_since(start_time);
        let total_bytes = chunk_size * count;
        let secs = time_span.as_secs();
        if secs >= 1 {
            return (total_bytes + secs / 2) / secs;
        }

        let millis = time_span.as_millis();
        if millis == 0 {
            return total_bytes;
        }

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

    /// Ledger range as partition offsets.
    ///
    /// Errors when `chunk_range` is not fully inside this module. The
    /// subtraction would underflow, or the result would pass the partition
    /// end. An overlapping caller must intersect with
    /// [`Self::get_storage_module_ledger_offsets`] first.
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
        let start = *(chunk_range.start() - storage_module_range.start());
        let end = *(chunk_range.end() - storage_module_range.start());
        Ok(PartitionChunkRange(ii(
            PartitionChunkOffset(
                u32::try_from(start).wrap_err("partition start does not fit in u32")?,
            ),
            PartitionChunkOffset(u32::try_from(end).wrap_err("partition end does not fit in u32")?),
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
        Ok(u32::try_from(local_offset)?)
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

/// Counts each interval-tree call. One rollback span is one `cut` and one
/// `insert`. A loop that cuts one offset at a time counts one edit per cut.
struct RollbackSpanEdits<'a> {
    map: &'a mut StorageIntervals,
    #[cfg(test)]
    edits: &'a AtomicU64,
}

impl RollbackSpanEdits<'_> {
    fn cut(&mut self, span: Interval<PartitionChunkOffset>) {
        let _ = self.map.cut(span);
        #[cfg(test)]
        self.edits.fetch_add(1, Ordering::SeqCst);
    }

    fn insert_merge_touching_if_values_equal(
        &mut self,
        span: Interval<PartitionChunkOffset>,
        value: ChunkType,
    ) {
        let _ = self.map.insert_merge_touching_if_values_equal(span, value);
        #[cfg(test)]
        self.edits.fetch_add(1, Ordering::SeqCst);
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
    index: &SubmoduleIndex,
) -> eyre::Result<()> {
    let empty = match fs::metadata(intervals_path) {
        Ok(meta) => meta.len() == 0,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
        Err(err) => {
            return Err(err).wrap_err_with(|| intervals_file_error(intervals_path, "stat"));
        }
    };
    if !empty {
        return Ok(());
    }
    // A default file labels the whole span Uninitialized, and open then drops
    // every placement under it. Only a module with no tx intervals may start
    // from scratch.
    let indexed = index.view(|tx| {
        tx.tx_intervals_in_range(submodule_interval.start(), submodule_interval.end())
    })?;
    if !indexed.is_empty() {
        return Err(eyre!(
            "intervals.json at {} is missing or empty, but the submodule index holds {} tx intervals. Starting would label the span Uninitialized and drop them. Restore intervals.json, or stop the node and clear this submodule directory (index, intervals.json, chunks.dat) so it can repack.",
            intervals_path.display(),
            indexed.len()
        ));
    }
    let mut intervals = StorageIntervals::new();
    intervals
        .insert_merge_touching_if_values_equal(*submodule_interval, ChunkType::Uninitialized)
        .expect("to insert a default interval to the submodule intervals");
    let mut file = get_atomic_file(intervals_path)?;
    file.write_all(serde_json::to_string(&intervals)?.as_bytes())?;
    file.commit()?;
    Ok(())
}

/// Operator steps when `intervals.json` cannot be read. The open error names
/// the file. The same steps are in `docs/60-indexes/06-storage-module-indexes.md`.
const INTERVALS_FILE_REMEDY: &str = "Stop the node, move this intervals.json aside, and clear this submodule directory (index, intervals.json, chunks.dat) so it can repack. See docs/60-indexes/06-storage-module-indexes.md.";

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

fn assigned_to_ledger(module: &StorageModule, ledger: DataLedger) -> bool {
    module
        .partition_assignment
        .read()
        .unwrap()
        .and_then(|assignment| assignment.ledger_id)
        == Some(ledger as u32)
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
            assigned_to_ledger(module, ledger)
                && module
                    .get_storage_module_ledger_offsets()
                    .is_ok_and(|range| range.overlaps(tx_chunk_range))
        })
        .cloned()
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
            assigned_to_ledger(module, ledger)
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
    use rstest::rstest;

    fn testing_config(dir: &Path, chunk_size: u64, chunks: u64) -> Config {
        testing_config_tuned(dir, chunk_size, chunks, |_| {})
    }

    fn testing_config_tuned(
        dir: &Path,
        chunk_size: u64,
        chunks: u64,
        tune: impl FnOnce(&mut NodeConfig),
    ) -> Config {
        let mut node = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: chunks,
                ..ConsensusConfig::testing()
            }),
            base_directory: dir.to_path_buf(),
            ..NodeConfig::testing()
        };
        tune(&mut node);
        Config::new_with_random_peer_id(node)
    }

    fn submit_assignment(slot: usize) -> PartitionAssignment {
        PartitionAssignment {
            ledger_id: Some(DataLedger::Submit.into()),
            slot_index: Some(slot),
            miner_address: irys_types::IrysAddress::from([0xAA; 20]),
            partition_hash: H256::random(),
        }
    }

    fn one_span(
        last: u32,
        path: impl Into<PathBuf>,
        assignment: Option<PartitionAssignment>,
    ) -> StorageModuleInfo {
        StorageModuleInfo {
            id: 0,
            partition_assignment: assignment,
            submodules: vec![(partition_chunk_offset_ii!(0, last), path.into())],
        }
    }

    /// A module reassigned onto a higher slot no longer owns the old ledger
    /// offset. That read is an error. The new slot's first offset still reads.
    #[test]
    fn ledger_offset_below_a_reassigned_module_is_an_error() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new()
            .prefix("ledger_offset_reassign")
            .build();
        let config = testing_config(tmp.path(), 32, 8);
        let assignment = submit_assignment(0);
        let storage = StorageModule::new(
            &one_span(7, tmp.path().join("sub"), Some(assignment)),
            &config,
        )?;

        let at_start = storage.generate_full_chunk_ledger_offset(LedgerChunkOffset::from(0))?;
        assert!(at_start.is_none(), "an empty in-range offset is a miss");
        let at_end = storage.generate_full_chunk_ledger_offset(LedgerChunkOffset::from(7))?;
        assert!(at_end.is_none(), "the last in-range offset is a miss");

        let mut next = assignment;
        next.slot_index = Some(1);
        storage.assign_partition(next, 3)?;

        let below = storage
            .generate_full_chunk_ledger_offset(LedgerChunkOffset::from(0))
            .expect_err("the old slot is below the module");
        assert!(
            below
                .to_string()
                .contains("chunk offset not in storage module"),
            "{below}"
        );
        let moved = storage.generate_full_chunk_ledger_offset(LedgerChunkOffset::from(8))?;
        assert!(
            moved.is_none(),
            "the new slot's first offset is still a miss, not an error"
        );
        Ok(())
    }

    fn data_header(data_root: H256, data_size: u64) -> DataTransactionHeader {
        DataTransactionHeader::V1(irys_types::DataTransactionHeaderV1WithMetadata {
            tx: DataTransactionHeaderV1 {
                data_root,
                data_size,
                ..Default::default()
            },
            metadata: irys_types::DataTransactionMetadata::new(),
        })
    }

    fn five_byte_chunk(data_root: H256) -> UnpackedChunk {
        UnpackedChunk {
            data_root,
            data_size: 5,
            data_path: vec![4, 3, 2, 1].into(),
            bytes: vec![0, 1, 2, 3, 4].into(),
            tx_offset: TxChunkOffset::from(0),
        }
    }

    fn fresh_module_dir(
        prefix: &str,
    ) -> eyre::Result<(irys_testing_utils::utils::tempfile::TempDir, PathBuf)> {
        let tmp = TempDirBuilder::new().prefix(prefix).build();
        let module_dir = tmp.path().join("module");
        fs::create_dir_all(&module_dir)?;
        Ok((tmp, module_dir))
    }

    fn park_second_chunk(storage: &StorageModule) -> eyre::Result<PartitionChunkOffset> {
        write_spacing_chunk(storage, 0, 1);
        storage.sync_pending_chunks()?;
        write_spacing_chunk(storage, 1, 2);
        storage.sync_pending_chunks()?;
        let deferred = PartitionChunkOffset::from(1);
        assert!(!storage.is_data_chunk_durable_at(deferred));
        Ok(deferred)
    }

    fn wait_until_drained(module: &StorageModule, timeout: Duration, message: &str) {
        let started = Instant::now();
        while module.has_pending_writes() {
            assert!(started.elapsed() < timeout, "{message}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    struct SyncPauseRelease<'a>(&'a StorageModule);
    impl Drop for SyncPauseRelease<'_> {
        fn drop(&mut self) {
            self.0.release_sync_pause();
        }
    }

    fn sample_placement(tx_start: i32, data_size: u64) -> Placement {
        Placement {
            data_root: H256::repeat_byte(1),
            tx_start_offset: RelativeChunkOffset(tx_start),
            data_size,
            ledger: None,
            start: PartitionChunkOffset(0),
            end: PartitionChunkOffset(u32::MAX),
            block_height: None,
            tx_index: None,
        }
    }

    #[test]
    fn tx_offset_above_i32_max_is_an_error() {
        let err = StorageModule::tx_offset_in_placement(
            &sample_placement(0, u64::MAX),
            PartitionChunkOffset((i32::MAX as u32) + 1),
            1,
        )
        .expect_err("partition offset above i32::MAX");
        assert!(err.to_string().contains("does not fit in i32"), "{err}");
    }

    #[test]
    fn tx_offset_counts_chunks_from_the_tx_start() -> eyre::Result<()> {
        let got = StorageModule::tx_offset_in_placement(
            &sample_placement(-1, 100),
            PartitionChunkOffset(4),
            8,
        )?;
        let (root, offset, size) = got.expect("inside the extent");
        assert_eq!(root, H256::repeat_byte(1));
        assert_eq!(offset, TxChunkOffset::from(5_u32));
        assert_eq!(size, 100);
        assert!(
            StorageModule::tx_offset_in_placement(
                &sample_placement(0, 8),
                PartitionChunkOffset(1),
                8,
            )?
            .is_none()
        );
        Ok(())
    }

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
        let config = testing_config(tmp.path(), RESERVE_CHUNK, RESERVE_CHUNKS);
        let last = u32::try_from(RESERVE_CHUNKS - 1).expect("chunk count");
        let info = one_span(last, "hdd0", None);
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
    // MDBX-SUBMODULE-INDEX: retarget this bench test at BenchIndex::Rocks. It opens MDBX.
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
        let config = testing_config(tmp.path(), RESERVE_CHUNK, RESERVE_CHUNKS);
        let info = one_span(
            u32::try_from(RESERVE_CHUNKS - 1).expect("chunk count"),
            "sub",
            None,
        );
        // MDBX-SUBMODULE-INDEX: delete this MDBX bench open.
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

    /// Recall bytes differ from the write file. A checksum confirm must read
    /// the write file: that is the image the checksum was computed from.
    #[cfg(feature = "bench")]
    // MDBX-SUBMODULE-INDEX: retarget this helper at BenchIndex::Rocks, or delete it with the MDBX bench open.
    fn open_labeled_split(
        prefix: &str,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        StorageModule,
        PathBuf,
        PathBuf,
    )> {
        let chunk = 32_u64;
        let chunks = 4_u64;
        let bytes = (chunk * chunks) as usize;
        let tmp = TempDirBuilder::new().prefix(prefix).build();
        let sub = tmp.path().join("sub");
        fs::create_dir_all(&sub)?;
        let recall_path = tmp.path().join("recall");
        let write_path = tmp.path().join("write");
        {
            let mut recall = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&recall_path)?;
            recall.write_all(&vec![0x22_u8; bytes])?;
            let mut write = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&write_path)?;
            write.write_all(&vec![0x11_u8; bytes])?;
        }
        let recall = OpenOptions::new().read(true).open(&recall_path)?;
        let write = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&write_path)?;
        let split = Arc::new(SplitBacking::open(recall, write, 0, chunks)?);
        let config = testing_config(tmp.path(), chunk, chunks);
        let info = one_span(3, "sub", Some(submit_assignment(0)));
        // MDBX-SUBMODULE-INDEX: delete this MDBX bench open.
        let storage = StorageModule::open_split(&info, &config, split, BenchIndex::Mdbx)?;
        Ok((tmp, storage, recall_path, write_path))
    }

    #[cfg(feature = "bench")]
    #[test]
    fn entropy_confirm_reads_the_split_write_file() -> eyre::Result<()> {
        let (_tmp, storage, recall_path, write_path) = open_labeled_split("split_entropy_pread")?;
        let offset = PartitionChunkOffset::from(0);
        let packed_for = storage
            .partition_hash()
            .ok_or_else(|| eyre::eyre!("partition hash missing"))?;
        let entropy = vec![0xE1_u8; 32];
        eyre::ensure!(
            storage.write_entropy_chunk(offset, entropy.clone(), packed_for)
                == EntropyWrite::Queued,
            "entropy write was refused"
        );
        storage.sync_pending_chunks()?;
        assert_eq!(storage.get_chunk_type(&offset), Some(ChunkType::Entropy));
        let _sync = storage.lock_sync();
        let read = storage.pread_settled_chunk(offset)?;
        drop(_sync);
        eyre::ensure!(
            read == entropy,
            "entropy confirm read the recall file, not the write file"
        );
        eyre::ensure!(
            read_prefix(&recall_path, 32)? == vec![0x22_u8; 32],
            "recall file changed"
        );
        eyre::ensure!(
            read_prefix(&write_path, 32)? == entropy,
            "write file does not hold the entropy"
        );
        Ok(())
    }

    /// Data already selects the write file. A confirm must keep doing that.
    #[cfg(feature = "bench")]
    #[test]
    fn data_confirm_reads_the_split_write_file() -> eyre::Result<()> {
        let (_tmp, storage, ..) = open_labeled_split("split_data_pread")?;
        let offset = PartitionChunkOffset::from(0);
        let data = vec![0xD4_u8; 32];
        eyre::ensure!(
            storage.write_chunk(offset, data.clone(), ChunkType::Data),
            "data write was refused"
        );
        storage.sync_pending_chunks()?;
        assert_eq!(storage.get_chunk_type(&offset), Some(ChunkType::Data));
        let _sync = storage.lock_sync();
        let read = storage.pread_settled_chunk(offset)?;
        eyre::ensure!(read == data, "data confirm did not read the write file");
        Ok(())
    }

    /// The index migration needs a second copy of the index. Reserving the
    /// partition first can fill the disk before that copy is written.
    #[test]
    fn index_opens_before_the_chunks_file_is_reserved() -> eyre::Result<()> {
        clear_submodule_open_order();
        let tmp = TempDirBuilder::new().prefix("open_order").build();
        let config = testing_config(tmp.path(), 32, 4);
        let info = one_span(3, "sub", None);
        let storage = StorageModule::new(&info, &config)?;
        drop(storage);
        let order = submodule_open_order();
        let index_at = order
            .iter()
            .position(|step| *step == "index")
            .ok_or_else(|| eyre::eyre!("index open was not recorded: {order:?}"))?;
        let file_at = order
            .iter()
            .position(|step| *step == "file")
            .ok_or_else(|| eyre::eyre!("chunks file open was not recorded: {order:?}"))?;
        eyre::ensure!(
            index_at < file_at,
            "chunks file was reserved before the index opened: {order:?}"
        );
        Ok(())
    }

    /// Arms the logical-size check to return `EFBIG` and clears it on drop.
    struct ChunksLenEfbig;

    impl ChunksLenEfbig {
        fn arm() -> Self {
            CHUNKS_LEN_EFBIG.with(|flag| flag.set(true));
            Self
        }
    }

    impl Drop for ChunksLenEfbig {
        fn drop(&mut self) {
            CHUNKS_LEN_EFBIG.with(|flag| flag.set(false));
        }
    }

    /// ext4 cannot name a mainnet partition. Open must refuse before the
    /// schema row changes. A length the filesystem can name still migrates,
    /// then reserves the file.
    #[test]
    fn efbig_refuses_open_before_the_index_is_migrated() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new().prefix("chunks_efbig").build();
        let sub = tmp.path().join("sub");
        fs::create_dir_all(&sub)?;
        let db_dir = write_unstamped_mdbx(&sub)?;
        assert_eq!(mdbx_schema_row(&db_dir)?, None, "fixture has no schema row");
        let before = directory_fingerprint(&db_dir)?;
        clear_submodule_open_order();
        let _efbig = ChunksLenEfbig::arm();
        let config = testing_config(tmp.path(), 32, 4);
        let err = StorageModule::new(&one_span(3, "sub", None), &config)
            .expect_err("a file the filesystem cannot name must not open");
        let message = err.to_string();
        assert!(message.contains("EFBIG"), "{message}");
        assert!(message.contains("16 TiB - 4 KiB"), "{message}");
        assert!(message.contains("index was not migrated"), "{message}");
        assert!(!message.contains("not enough free space"), "{message}");
        assert!(!message.contains("ENOSPC"), "{message}");
        assert_eq!(
            mdbx_schema_row(&db_dir)?,
            None,
            "EFBIG must not migrate the index"
        );
        assert_eq!(
            directory_fingerprint(&db_dir)?,
            before,
            "EFBIG must not rewrite the index"
        );
        let chunks = sub.join("chunks.dat");
        let len = fs::metadata(&chunks)?.len();
        assert_eq!(len, 0, "EFBIG must not extend chunks.dat, len {len}");
        assert!(
            !sub.join(PACKING_PARAMS_FILE_NAME).exists(),
            "open continued past the length check"
        );
        let order = submodule_open_order();
        let index_opens = order.iter().filter(|step| **step == "index").count();
        assert_eq!(
            index_opens, 0,
            "the index must not open after EFBIG: {order:?}"
        );
        Ok(())
    }

    #[test]
    fn fitting_chunks_len_still_migrates_the_index_then_reserves() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new().prefix("chunks_len_fits").build();
        let sub = tmp.path().join("sub");
        fs::create_dir_all(&sub)?;
        let db_dir = write_unstamped_mdbx(&sub)?;
        assert_eq!(mdbx_schema_row(&db_dir)?, None);
        clear_submodule_open_order();
        let config = testing_config(tmp.path(), RESERVE_CHUNK, RESERVE_CHUNKS);
        let last = u32::try_from(RESERVE_CHUNKS - 1).expect("chunk count");
        let storage = StorageModule::new(&one_span(last, "sub", None), &config)?;
        drop(storage);
        assert_eq!(
            mdbx_schema_row(&db_dir)?.as_deref(),
            Some(SUBMODULE_SCHEMA.as_bytes()),
            "a length the filesystem can name still migrates"
        );
        let order = submodule_open_order();
        let index_at = order
            .iter()
            .position(|step| *step == "index")
            .ok_or_else(|| eyre::eyre!("index open was not recorded: {order:?}"))?;
        let file_at = order
            .iter()
            .position(|step| *step == "file")
            .ok_or_else(|| eyre::eyre!("chunks file open was not recorded: {order:?}"))?;
        assert!(
            index_at < file_at,
            "the chunks file opened before the index: {order:?}"
        );
        let reserve_at = order
            .iter()
            .position(|step| *step == "reserve")
            .ok_or_else(|| eyre::eyre!("chunks reserve was not recorded: {order:?}"))?;
        assert!(
            index_at < reserve_at,
            "the hole reserve must follow the index migration: {order:?}"
        );
        assert!(
            !order[..index_at].contains(&"reserve"),
            "reserve ran before the index returned: {order:?}"
        );
        assert_eq!(
            order.iter().filter(|step| **step == "index").count(),
            1,
            "{order:?}"
        );
        #[cfg(target_os = "linux")]
        assert_reserved(&sub.join("chunks.dat"), reserve_bytes())?;
        Ok(())
    }

    #[test]
    // MDBX-SUBMODULE-INDEX: delete this test with the MDBX open path.
    fn submodule_index_engine_mdbx_fresh_does_not_create_rocks() -> eyre::Result<()> {
        let (_tmp_dir, module_dir) = fresh_module_dir("submodule_index_engine_mdbx")?;

        let index = open_submodule_index(&module_dir, &index_database(SubmoduleIndexEngine::Mdbx))?;
        drop(index);

        assert!(module_dir.join("db").join("mdbx.dat").is_file());
        assert!(!module_dir.join("rocks").exists());
        Ok(())
    }

    #[test]
    fn submodule_index_engine_rocksdb_fresh_reads_back_a_row() -> eyre::Result<()> {
        let (_tmp_dir, module_dir) = fresh_module_dir("submodule_index_engine_rocks")?;

        let index =
            open_submodule_index(&module_dir, &index_database(SubmoduleIndexEngine::Rocksdb))?;
        let schema = fs::read_to_string(module_dir.join("rocks").join("SCHEMA"))?;
        assert_eq!(schema, SUBMODULE_SCHEMA_ROCKS_V1);
        assert!(!module_dir.join("db").join("mdbx.dat").exists());

        // RocksDB v1 drops a data path that no interval covers. The interval
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
                .expect_err("crashed first open must not open");
        let message = format!("{error:#}");
        // CURRENT and no CONVERTING is a crashed first open. The converter
        // flag does not delete that directory.
        assert!(message.contains("SCHEMA"), "{message}");
        assert!(message.contains("no CONVERTING"), "{message}");
        assert!(message.contains("Delete this directory"), "{message}");
        assert!(!message.contains("with --replace-incomplete"), "{message}");
        assert!(!rocks_dir.join("SCHEMA").exists());
        Ok(())
    }

    #[test]
    // MDBX-SUBMODULE-INDEX: delete this test with the MDBX open path.
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
    // MDBX-SUBMODULE-INDEX: delete this helper with the MDBX open tests.
    fn write_unstamped_mdbx(module_dir: &Path) -> eyre::Result<PathBuf> {
        use irys_database::open_or_create_db;
        use irys_database::submodule::tables::SubmoduleTables;

        let db_dir = module_dir.join("db");
        let args = irys_database::submodule_db_args(&index_database(SubmoduleIndexEngine::Mdbx))?;
        let env = open_or_create_db(&db_dir, SubmoduleTables::ALL, args)?;
        drop(env);
        Ok(db_dir)
    }

    // MDBX-SUBMODULE-INDEX: delete this helper with the MDBX open tests.
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
    // MDBX-SUBMODULE-INDEX: delete this test with the MDBX open path.
    fn submodule_index_engine_mdbx_refuses_when_both_indexes_exist() -> eyre::Result<()> {
        let (_tmp_dir, module_dir) = fresh_module_dir("submodule_index_engine_both")?;
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
        let (_tmp_dir, module_dir) = fresh_module_dir("submodule_index_engine_rocks_keeps_db")?;

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
            SUBMODULE_SCHEMA_ROCKS_V1
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
            let config = testing_config_tuned(tmp.path(), 32, 5, |node| {
                node.database = index_database(engine);
            });
            let storage = StorageModule::new(&one_span(4, "hdd0", None), &config)?;
            Ok((tmp, storage))
        }

        let (_rocks_tmp, rocks) = open("rocks_group_commit", SubmoduleIndexEngine::Rocksdb)?;
        // MDBX-SUBMODULE-INDEX: delete the MDBX half of this test. The RocksDB group-commit assertion stays.
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
        // RocksDB v1 stores the data-root row on the interval. A row with no
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

        // A synced write that is not the chunk commit. RocksDB v1 has no
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
        let config = testing_config_tuned(tmp.path(), 32, 8, |node| {
            node.storage = StorageSyncConfig {
                num_writes_before_sync: 1,
                intervals_persist_secs,
                min_durable_interval_ms: 0,
                ..StorageSyncConfig::default()
            };
            node.database = index_database(engine);
        });
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
        // MDBX-SUBMODULE-INDEX: drop Mdbx from this list and keep the RocksDB run.
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

    /// RocksDB v1: a filled placement stays consistent across a second
    /// registration, an in-range pending move, and a settle.
    #[test]
    fn rocks_settle_advance_and_reregister_stay_consistent() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("rocks_body_audit")
            .with_tracing()
            .build();
        let chunk_size = 32_u64;
        let config = testing_config_tuned(tmp_dir.path(), chunk_size, 20, |node| {
            node.database = index_database(SubmoduleIndexEngine::Rocksdb);
        });
        let storage_module =
            StorageModule::new(&one_span(19, "hdd0", Some(submit_assignment(0))), &config)?;
        let data_root = H256::repeat_byte(4);
        let data_size = 8 * chunk_size;
        let header = data_header(data_root, data_size);
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
    // MDBX-SUBMODULE-INDEX: delete the stored_paths arms in this helper. They check the tx path only MDBX keeps.
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
    fn missing_or_empty_intervals_file_refuses_an_indexed_module() -> eyre::Result<()> {
        for empty_file in [false, true] {
            for_each_index_engine("intervals_missing", |config, storage| {
                seed_mixed_index(&storage)?;
                storage.suppress_interval_flush_for_test();
                let path = submodule_interval_path(&storage, 0);
                drop(storage);
                if empty_file {
                    fs::write(&path, b"")?;
                } else {
                    fs::remove_file(&path)?;
                }
                let Err(err) = reopen_presence(config) else {
                    eyre::bail!("open relabelled an indexed span as Uninitialized");
                };
                assert!(format!("{err:#}").contains("missing or empty"), "{err:#}");
                // Refusing leaves the index for the operator to restore.
                fs::write(&path, b"")?;
                Ok(())
            })?;
        }
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

    /// The probe is armed and nobody else holds `sync_in_progress`. Reset
    /// still clears the label, and the try does not look blocked. A later
    /// write is accepted.
    #[test]
    fn reset_with_the_sync_probe_armed_still_finishes_when_the_lock_is_free() -> eyre::Result<()> {
        let (tmp, _config, storage) = interval_batch_module("reset_probe_free", 0)?;
        assert!(storage.write_chunk(
            PartitionChunkOffset::from(0),
            vec![7_u8; 32],
            ChunkType::Data,
        ));
        storage.sync_lock_blocked.store(false, Ordering::SeqCst);
        storage.sync_lock_probe.store(true, Ordering::SeqCst);
        storage.reset()?;
        assert!(
            !storage.sync_lock_blocked.load(Ordering::SeqCst),
            "a free sync lock looked blocked"
        );
        assert!(storage.get_intervals(ChunkType::Data).is_empty());
        assert!(storage.write_chunk(
            PartitionChunkOffset::from(1),
            vec![8_u8; 32],
            ChunkType::Entropy,
        ));
        drop(storage);
        drop(tmp);
        Ok(())
    }

    /// `reset` during the presence-copy gap must not leave that proof in the index.
    ///
    /// The durable point holds `sync_in_progress` at the pause. `reset` has to
    /// take the same lock, so it runs only after the point commits and drops
    /// the lock. The copied rows are then cleared with the rest of the index.
    #[test]
    fn reset_during_presence_copy_leaves_no_data_path() -> eyre::Result<()> {
        let mut errors = Vec::new();
        // MDBX-SUBMODULE-INDEX: drop Mdbx from this list and keep the RocksDB run.
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
                // RocksDB v1 writes the interval only when this batch also stores
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
                let (sync_result, reset_result, reset_blocked_on_sync, sync_still_held) =
                    std::thread::scope(|scope| {
                        let _release = SyncPauseRelease(storage_ref);
                        let sync_thread =
                            scope.spawn(move || storage_ref.force_sync_pending_chunks());
                        assert!(
                            storage_ref.wait_for_sync_pause(Duration::from_secs(5)),
                            "durable point did not pause after the proof copy"
                        );
                        storage_ref.sync_lock_blocked.store(false, Ordering::SeqCst);
                        storage_ref.sync_lock_probe.store(true, Ordering::SeqCst);
                        let (reset_tx, reset_rx) = std::sync::mpsc::channel();
                        scope.spawn(move || {
                            let result = storage_ref.reset();
                            let _ = reset_tx.send(result);
                        });

                        // A timeout is a failure. Not finishing inside the window
                        // is not proof that reset reached the mutex.
                        let deadline = Instant::now() + Duration::from_secs(5);
                        let mut blocked = false;
                        let mut early = None;
                        while Instant::now() < deadline {
                            if storage_ref.sync_lock_blocked.load(Ordering::SeqCst) {
                                blocked = true;
                                break;
                            }
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
                        assert!(blocked, "reset did not block on the sync lock");
                        assert!(
                            early.is_none(),
                            "reset finished before it blocked on the sync lock"
                        );
                        let sync_still_held = storage_ref.sync_in_progress.try_lock().is_err();
                        storage_ref.sync_lock_probe.store(false, Ordering::SeqCst);
                        storage_ref.release_sync_pause();
                        let sync_result = sync_thread.join().expect("sync thread");
                        let reset_result = match early {
                            Some(result) => result,
                            None => reset_rx.recv_timeout(Duration::from_secs(5)).expect(
                                "reset did not finish after the durable point was released",
                            ),
                        };
                        (sync_result, reset_result, blocked, sync_still_held)
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
                        && reset_blocked_on_sync
                        && sync_still_held
                        && pending_clear
                        && intervals == vec![partition_chunk_offset_ii!(0, 7)]
                        && storage.get_intervals(ChunkType::Data).is_empty(),
                    "path={path:?} reset_blocked_on_sync={reset_blocked_on_sync} sync_still_held={sync_still_held} pending_clear={pending_clear} intervals={intervals:?}"
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

    fn seed_proof_coverage(storage: &StorageModule) -> eyre::Result<()> {
        let tx_hash = H256::repeat_byte(9);
        let data_root = H256::repeat_byte(4);
        let leaf = TxLeafBinding {
            data_root,
            prefix_hash: H256::repeat_byte(1),
        };
        let folded = DataTransactionLedger::fold_tx_root_leaf(data_root, leaf.prefix_hash);
        let mut tx_path = folded.0.to_vec();
        tx_path.extend_from_slice(&[0_u8; 32]);
        let (_, submodule) = storage.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        submodule.db.update(|tx| {
            tx.add_full_tx_path(tx_hash, tx_path)?;
            tx.add_tx_leaf_binding(tx_hash, &leaf)?;
            tx.add_tx_path_hash_to_offset_range(
                PartitionChunkOffset::from(1),
                PartitionChunkOffset::from(6),
                Some(tx_hash),
            )?;
            Ok(())
        })?;
        Ok(())
    }

    /// Assigned module. `store_assignment` writes `submodule.path`, and a
    /// relative name misses this temp dir, so the submodule path is absolute.
    fn open_assigned_presence(
        prefix: &str,
        engine: SubmoduleIndexEngine,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        Config,
        StorageModule,
        H256,
    )> {
        let tmp = TempDirBuilder::new().prefix(prefix).build();
        let config = testing_config_tuned(tmp.path(), 32, 8, |node| {
            node.storage = StorageSyncConfig {
                num_writes_before_sync: 1,
                intervals_persist_secs: 0,
                min_durable_interval_ms: 0,
                ..StorageSyncConfig::default()
            };
            node.database = index_database(engine);
        });
        let hash = H256::repeat_byte(0x11);
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(partition_chunk_offset_ii!(0, 7), tmp.path().join("sub"))],
        };
        let storage = StorageModule::new(&info, &config)?;
        Ok((tmp, config, storage, hash))
    }

    fn queue_proved_data(storage: &StorageModule, offset: PartitionChunkOffset, byte: u8) {
        let mut pending = storage.pending_writes.write().unwrap();
        pending.insert(offset, (vec![byte; 32], ChunkType::Data));
        pending
            .priorities
            .insert(offset, disk_lane::WritePriority::Ingress);
        pending.queued_at.insert(offset, Instant::now());
        pending
            .proofs
            .insert(offset, (H256::repeat_byte(7), vec![7, 7, 7]));
    }

    fn open_entropy_module(
        prefix: &str,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        StorageModule,
        H256,
    )> {
        let tmp = TempDirBuilder::new().prefix(prefix).build();
        let config = testing_config(tmp.path(), 32, 4);
        let hash = H256::repeat_byte(0x11);
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment {
                partition_hash: hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            }),
            submodules: vec![(partition_chunk_offset_ii!(0, 3), tmp.path().join("sub"))],
        };
        let storage = StorageModule::new(&info, &config)?;
        Ok((tmp, storage, hash))
    }

    /// Queued data, with its proof, and no durable-point lock. Entropy for
    /// that offset must leave both in place. A parked data body is the same
    /// refusal.
    #[test]
    fn entropy_write_keeps_queued_data_when_sync_is_not_held() -> eyre::Result<()> {
        let (_tmp, storage, hash) = open_entropy_module("entropy_keeps_queued_data")?;
        let offset = PartitionChunkOffset::from(1);
        queue_proved_data(&storage, offset, 0xD4);
        assert!(
            !storage.sync_lock_is_held(),
            "the durable point must not hold the sync lock"
        );
        let wrote = storage.write_entropy_chunk(offset, vec![0xEE; 32], hash);
        let pending = storage.pending_writes.read().unwrap();
        let queued = pending
            .get(&offset)
            .map(|(bytes, kind)| (bytes.clone(), *kind));
        let proof = pending.proofs.get(&offset).cloned();
        drop(pending);
        assert_eq!(
            wrote,
            EntropyWrite::Rejected(EntropyRejection::NotUninitialized(Some(ChunkType::Data))),
            "queued data was replaced: wrote={wrote:?} queued={queued:?} proof={proof:?}"
        );
        assert_eq!(queued, Some((vec![0xD4_u8; 32], ChunkType::Data)));
        assert!(proof.is_some(), "the queued proof was removed");

        let parked = PartitionChunkOffset::from(2);
        {
            let mut pending = storage.pending_writes.write().unwrap();
            pending.written_ahead.insert(
                parked,
                WrittenAhead {
                    bytes: vec![0xA1; 32],
                    chunk_type: ChunkType::Data,
                    priority: disk_lane::WritePriority::Ingress,
                    queued_at: Instant::now(),
                    pwrite_at: None,
                },
            );
            pending
                .proofs
                .insert(parked, (H256::repeat_byte(9), vec![9, 9]));
        }
        let wrote_parked = storage.write_entropy_chunk(parked, vec![0xEE; 32], hash);
        let pending = storage.pending_writes.read().unwrap();
        let ahead = pending
            .written_ahead
            .get(&parked)
            .map(|ahead| (ahead.bytes.clone(), ahead.chunk_type));
        let parked_proof = pending.proofs.contains_key(&parked);
        let parked_body = pending
            .get(&parked)
            .map(|(bytes, kind)| (bytes.clone(), *kind));
        drop(pending);
        assert_eq!(
            wrote_parked,
            EntropyWrite::Rejected(EntropyRejection::NotUninitialized(Some(ChunkType::Data))),
            "parked data was replaced: wrote={wrote_parked:?} ahead={ahead:?} proof={parked_proof} body={parked_body:?}"
        );
        assert_eq!(ahead, Some((vec![0xA1_u8; 32], ChunkType::Data)));
        assert!(parked_proof, "the parked proof was removed");
        assert!(parked_body.is_none(), "entropy was queued over parked data");
        Ok(())
    }

    /// A live label other than `Uninitialized` is not an entropy target.
    /// The interval map is the label. Pending is not.
    #[test]
    fn entropy_write_rejects_a_live_label_that_is_not_uninitialized() -> eyre::Result<()> {
        let cases = [ChunkType::Interrupted, ChunkType::Data, ChunkType::Entropy];
        for kind in cases {
            let (_tmp, storage, hash) = open_entropy_module("entropy_live_label")?;
            let offset = PartitionChunkOffset::from(0);
            {
                let mut intervals = storage.intervals.write().unwrap();
                StorageModule::cut_then_insert_interval_if_touching(&mut intervals, offset, kind);
            }
            let wrote = storage.write_entropy_chunk(offset, vec![0xEE; 32], hash);
            assert_eq!(
                wrote,
                EntropyWrite::Rejected(EntropyRejection::NotUninitialized(Some(kind))),
                "live {kind:?} accepted an entropy write"
            );
            let pending = storage.pending_writes.read().unwrap();
            assert!(
                pending.get(&offset).is_none(),
                "entropy body queued on live {kind:?}"
            );
        }
        Ok(())
    }

    /// The assigned hash queues entropy on `Uninitialized` and may replace a
    /// queued entropy body. A different hash, or no assignment, does not.
    #[test]
    fn entropy_write_queues_only_for_the_assigned_uninitialized_offset() -> eyre::Result<()> {
        let (_tmp, storage, hash) = open_entropy_module("entropy_write_assigned")?;
        let offset = PartitionChunkOffset::from(0);
        assert_eq!(
            storage.write_entropy_chunk(offset, vec![0xA1; 32], hash),
            EntropyWrite::Queued
        );
        assert_eq!(
            storage.write_entropy_chunk(offset, vec![0xB2; 32], hash),
            EntropyWrite::Queued,
            "a second entropy body must replace the queued one"
        );
        let pending = storage.pending_writes.read().unwrap();
        let queued = pending
            .get(&offset)
            .map(|(bytes, kind)| (bytes.clone(), *kind));
        drop(pending);
        assert_eq!(queued, Some((vec![0xB2_u8; 32], ChunkType::Entropy)));

        let other = PartitionChunkOffset::from(1);
        let wrong = H256::repeat_byte(0x22);
        assert_eq!(
            storage.write_entropy_chunk(other, vec![0xEE; 32], wrong),
            EntropyWrite::Rejected(EntropyRejection::NotAssigned)
        );
        assert!(storage.pending_writes.read().unwrap().get(&other).is_none());

        storage.reset_with_assignment(None, 1)?;
        assert_eq!(
            storage.write_entropy_chunk(PartitionChunkOffset::from(2), vec![0xEE; 32], hash),
            EntropyWrite::Rejected(EntropyRejection::NotAssigned)
        );
        Ok(())
    }

    /// Entropy inserts and the durable point share the module. A reversed
    /// lock order hangs this loop. The deadline is monotonic.
    #[test]
    fn entropy_write_does_not_deadlock_against_the_durable_point() {
        let (_tmp, storage, hash) =
            open_entropy_module("entropy_write_lock_order").expect("module");
        let storage = Arc::new(storage);
        let deadline = Instant::now() + Duration::from_secs(2);
        let (tx, rx) = std::sync::mpsc::channel();
        let writer_storage = Arc::clone(&storage);
        let writer = std::thread::spawn(move || {
            let mut writes = 0_u64;
            while Instant::now() < deadline {
                let offset = PartitionChunkOffset::from((writes % 4) as u32);
                let outcome = writer_storage.write_entropy_chunk(offset, vec![0xEE; 32], hash);
                // Put an entropy label back so the next pass takes `pending`
                // again while the durable point is in its own critical section.
                if matches!(
                    outcome,
                    EntropyWrite::Rejected(EntropyRejection::NotUninitialized(Some(
                        ChunkType::Entropy
                    )))
                ) {
                    let mut intervals = writer_storage.intervals().write().unwrap();
                    StorageModule::cut_then_insert_interval_if_touching(
                        &mut intervals,
                        offset,
                        ChunkType::Uninitialized,
                    );
                }
                writes += 1;
            }
            let _ = tx.send(writes);
        });
        let syncer_storage = Arc::clone(&storage);
        let syncer = std::thread::spawn(move || {
            while Instant::now() < deadline {
                let _ = syncer_storage.force_sync_pending_chunks();
            }
        });
        let writes = rx
            .recv_timeout(Duration::from_secs(4))
            .expect("entropy write hung against the durable point");
        writer.join().expect("writer");
        syncer.join().expect("syncer");
        assert!(writes > 0, "the writer did not run");
    }

    /// A data durable point holds `sync_in_progress` across pwrite. The live
    /// label is still `Interrupted`. Entropy for that offset must not replace
    /// the queued body or its proof. Entropy for a different offset, including
    /// a newer entropy body, still queues.
    #[test]
    fn entropy_write_during_interrupted_data_pwrite_drops_the_proof() -> eyre::Result<()> {
        let mut errors = Vec::new();
        // MDBX-SUBMODULE-INDEX: drop Mdbx from this list and keep the RocksDB run.
        for engine in [SubmoduleIndexEngine::Mdbx, SubmoduleIndexEngine::Rocksdb] {
            let prefix = format!("entropy_during_data_pwrite_{engine:?}");
            let outcome = (|| -> eyre::Result<()> {
                let (_tmp, _config, storage, hash) = open_assigned_presence(&prefix, engine)?;
                seed_proof_coverage(&storage)?;
                let offset = PartitionChunkOffset::from(2);
                let other = PartitionChunkOffset::from(3);
                queue_proved_data(&storage, offset, 0xD4);
                storage.pause_next_sync_at(SyncPausePoint::AfterPwrite);
                let storage_ref = &storage;
                let sync_result = std::thread::scope(|scope| {
                    let _release = SyncPauseRelease(storage_ref);
                    let sync_thread = scope.spawn(move || storage_ref.force_sync_pending_chunks());
                    assert!(
                        storage_ref.wait_for_sync_pause(Duration::from_secs(5)),
                        "data sync did not pause after pwrite"
                    );
                    assert_eq!(
                        live_label(storage_ref, offset.0),
                        Some(ChunkType::Interrupted),
                        "the in-flight label is still Interrupted"
                    );
                    assert_eq!(
                        storage_ref.write_entropy_chunk(other, vec![0xAB; 32], hash),
                        EntropyWrite::Queued,
                        "entropy for an offset with no queued data still queues"
                    );
                    assert_eq!(
                        storage_ref.write_entropy_chunk(other, vec![0xCD; 32], hash),
                        EntropyWrite::Queued,
                        "a newer entropy body replaces queued entropy while sync is held"
                    );
                    assert_eq!(
                        storage_ref.write_entropy_chunk(offset, vec![0xEE; 32], hash),
                        EntropyWrite::Rejected(EntropyRejection::NotUninitialized(Some(
                            ChunkType::Interrupted,
                        ))),
                        "entropy must not replace an in-flight data write"
                    );
                    storage_ref.release_sync_pause();
                    sync_thread.join().expect("sync thread")
                });
                sync_result?;
                assert_eq!(live_label(&storage, offset.0), Some(ChunkType::Data));
                assert_eq!(file_label(&storage, offset.0)?, Some(ChunkType::Data));
                let pending = storage.pending_writes.read().unwrap();
                assert!(
                    pending.get(&offset).is_none(),
                    "the data body was durable and must leave the queue"
                );
                assert!(!pending.proofs.contains_key(&offset));
                let other_queued = pending
                    .get(&other)
                    .map(|(bytes, kind)| (bytes.clone(), *kind));
                drop(pending);
                assert_eq!(
                    other_queued,
                    Some((vec![0xCD_u8; 32], ChunkType::Entropy)),
                    "the newer entropy body stays queued"
                );
                assert_eq!(data_path_at(&storage, offset.0)?, Some(vec![7, 7, 7]));
                let loaded = storage.read_chunks(partition_chunk_offset_ii!(offset, offset))?;
                let (bytes, kind) = loaded.get(&offset).expect("data bytes on disk");
                assert_eq!(*kind, ChunkType::Data);
                assert_eq!(bytes.as_slice(), &[0xD4_u8; 32]);
                Ok(())
            })();
            if let Err(err) = outcome {
                errors.push(format!("{engine:?}: {err:#}"));
            }
        }
        eyre::ensure!(errors.is_empty(), "{}", errors.join(" | "));
        Ok(())
    }

    /// Replacing the snapshotted data body and dropping its proof, while the
    /// point still holds `sync_in_progress`, must not publish `Data`.
    #[test]
    fn replaced_data_bytes_without_a_proof_do_not_publish_data() -> eyre::Result<()> {
        let mut errors = Vec::new();
        // MDBX-SUBMODULE-INDEX: drop Mdbx from this list and keep the RocksDB run.
        for engine in [SubmoduleIndexEngine::Mdbx, SubmoduleIndexEngine::Rocksdb] {
            let prefix = format!("replaced_data_snapshot_{engine:?}");
            let outcome = (|| -> eyre::Result<()> {
                let (_tmp, _config, storage) = open_presence_module_with(&prefix, engine, 0)?;
                seed_proof_coverage(&storage)?;
                let offset = PartitionChunkOffset::from(2);
                queue_proved_data(&storage, offset, 0xD4);
                storage.pause_next_sync_at(SyncPausePoint::AfterPwrite);
                let storage_ref = &storage;
                let sync_result = std::thread::scope(|scope| {
                    let _release = SyncPauseRelease(storage_ref);
                    let sync_thread = scope.spawn(move || storage_ref.force_sync_pending_chunks());
                    assert!(
                        storage_ref.wait_for_sync_pause(Duration::from_secs(5)),
                        "data sync did not pause after pwrite"
                    );
                    {
                        let mut pending = storage_ref.pending_writes.write().unwrap();
                        pending.insert(offset, (vec![0xEE; 32], ChunkType::Entropy));
                        pending
                            .priorities
                            .insert(offset, disk_lane::WritePriority::Packing);
                        pending.proofs.remove(&offset);
                    }
                    storage_ref.release_sync_pause();
                    sync_thread.join().expect("sync thread")
                });
                sync_result?;
                assert_ne!(
                    live_label(&storage, offset.0),
                    Some(ChunkType::Data),
                    "a proof-less replaced snapshot must not stay Data"
                );
                assert_ne!(
                    file_label(&storage, offset.0)?,
                    Some(ChunkType::Data),
                    "the interval file must not publish Data"
                );
                assert!(data_path_at(&storage, offset.0)?.is_none());
                let pending = storage.pending_writes.read().unwrap();
                let queued = pending
                    .get(&offset)
                    .map(|(bytes, kind)| (bytes.clone(), *kind));
                drop(pending);
                assert_eq!(queued, Some((vec![0xEE_u8; 32], ChunkType::Entropy)));
                Ok(())
            })();
            if let Err(err) = outcome {
                errors.push(format!("{engine:?}: {err:#}"));
            }
        }
        eyre::ensure!(errors.is_empty(), "{}", errors.join(" | "));
        Ok(())
    }

    /// `write_chunk` of `Data` does not queue a proof. That write still
    /// completes as `Data`.
    #[test]
    fn data_chunk_without_a_proof_still_becomes_data() -> eyre::Result<()> {
        for_each_index_engine("data_without_proof", |_config, storage| {
            let offset = PartitionChunkOffset::from(2);
            assert!(storage.write_chunk(offset, vec![0xD4_u8; 32], ChunkType::Data));
            storage.force_sync_pending_chunks()?;
            assert_eq!(live_label(&storage, 2), Some(ChunkType::Data));
            assert_eq!(file_label(&storage, 2)?, Some(ChunkType::Data));
            let loaded = storage.read_chunks(partition_chunk_offset_ii!(2, 2))?;
            assert_eq!(
                loaded.get(&offset).map(|(bytes, _)| bytes.as_slice()),
                Some([0xD4_u8; 32].as_slice())
            );
            Ok(())
        })
    }

    /// A registration that has computed its rows must not commit them after
    /// the assignment changes. The same ledger and slot still commits.
    /// A registration that starts after the reset returns commits normally.
    #[test]
    fn registration_commit_follows_the_assignment() -> eyre::Result<()> {
        let mut errors = Vec::new();
        // MDBX-SUBMODULE-INDEX: drop Mdbx from this list and keep the RocksDB run.
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

    /// Rollback does not change the assignment. An in-flight registration
    /// must not write the rows rollback just cleared. A registration that
    /// starts after rollback, and one paused across a same-slot reset, still
    /// commit. Both index engines.
    #[test]
    fn registration_after_rollback_does_not_restore_rows() -> eyre::Result<()> {
        let mut errors = Vec::new();
        // MDBX-SUBMODULE-INDEX: drop Mdbx from this list and keep the RocksDB run.
        for engine in [SubmoduleIndexEngine::Mdbx, SubmoduleIndexEngine::Rocksdb] {
            let prefix = format!("registration_rollback_{engine:?}");
            let outcome = (|| -> eyre::Result<()> {
                let (tmp, storage, original) = open_publish_module(&prefix, engine)?;
                let _tmp = tmp;
                index_one(&storage, 2)?;
                eyre::ensure!(
                    registration_rows(&storage)?.is_present(),
                    "seed registration did not commit"
                );

                storage.pause_next_registration_at(RegistrationPausePoint::AfterRowsComputed);
                let (result_tx, result_rx) = std::sync::mpsc::channel();
                std::thread::scope(|scope| -> eyre::Result<()> {
                    struct Release<'a>(&'a StorageModule);
                    impl Drop for Release<'_> {
                        fn drop(&mut self) {
                            self.0.release_registration_pause();
                        }
                    }
                    let _release = Release(&storage);
                    scope.spawn(|| {
                        let _ = result_tx.send(index_one(&storage, 1));
                    });
                    eyre::ensure!(
                        storage.wait_for_registration_pause(Duration::from_secs(5)),
                        "registration did not pause after computing its rows"
                    );
                    storage.rollback_unassigned_range(
                        PartitionChunkOffset::from(0),
                        PartitionChunkOffset::from(7),
                        &[H256::repeat_byte(4)],
                    )?;
                    let cleared = registration_rows(&storage)?;
                    eyre::ensure!(
                        cleared.is_empty(),
                        "rollback left rows while the registration was paused: {cleared:?}"
                    );
                    eyre::ensure!(
                        storage.partition_assignment() == Some(original),
                        "rollback changed the assignment"
                    );
                    storage.release_registration_pause();
                    result_rx
                        .recv_timeout(Duration::from_secs(5))
                        .map_err(|_| eyre::eyre!("registration did not finish after release"))??;
                    Ok(())
                })?;
                let restored = registration_rows(&storage)?;
                eyre::ensure!(
                    restored.is_empty(),
                    "in-flight registration restored rows after rollback: {restored:?}"
                );
                eyre::ensure!(
                    storage.registration_drops.load(Ordering::SeqCst) == 1,
                    "drops={}",
                    storage.registration_drops.load(Ordering::SeqCst)
                );

                index_one(&storage, 3)?;
                let after = registration_rows(&storage)?;
                eyre::ensure!(
                    after.is_present(),
                    "registration that started after rollback did not commit: {after:?}"
                );
                eyre::ensure!(
                    storage.registration_drops.load(Ordering::SeqCst) == 1,
                    "post-rollback registration was dropped"
                );

                let kept = race_registration(&storage, Some(original), 9, "same-slot")?;
                eyre::ensure!(
                    kept.reset_finished_while_paused
                        && kept.rows.is_present()
                        && storage.partition_assignment() == Some(original)
                        && kept.registration_drops == 1,
                    "same-slot reset dropped a registration: {kept:?}"
                );
                Ok(())
            })();
            if let Err(err) = outcome {
                errors.push(format!("{engine:?}: {err}"));
            }
        }
        eyre::ensure!(errors.is_empty(), "{}", errors.join(" | "));
        Ok(())
    }

    /// The registration holds the assignment read lock across its commit.
    /// Rollback takes that lock's write side, so it cannot clear the index
    /// between the check and the commit. It clears the committed rows after.
    #[test]
    fn rollback_waits_for_the_registration_commit_then_clears() -> eyre::Result<()> {
        let mut errors = Vec::new();
        // MDBX-SUBMODULE-INDEX: drop Mdbx from this list and keep the RocksDB run.
        for engine in [SubmoduleIndexEngine::Mdbx, SubmoduleIndexEngine::Rocksdb] {
            let prefix = format!("registration_rollback_held_{engine:?}");
            let outcome = (|| -> eyre::Result<()> {
                let (tmp, storage, original) = open_publish_module(&prefix, engine)?;
                let _tmp = tmp;
                let storage = Arc::new(storage);
                storage.pause_next_registration_at(RegistrationPausePoint::DuringCommit);
                let (result_tx, result_rx) = std::sync::mpsc::channel();
                std::thread::scope(|scope| -> eyre::Result<()> {
                    struct Release<'a>(&'a StorageModule);
                    impl Drop for Release<'_> {
                        fn drop(&mut self) {
                            self.0.release_registration_pause();
                        }
                    }
                    let _release = Release(&storage);
                    let registering = Arc::clone(&storage);
                    scope.spawn(move || {
                        let _ = result_tx.send(index_one(&registering, 1));
                    });
                    eyre::ensure!(
                        storage.wait_for_registration_pause(Duration::from_secs(5)),
                        "registration did not pause while holding the assignment lock"
                    );
                    let (roll_tx, roll_rx) = std::sync::mpsc::channel();
                    let rolling = Arc::clone(&storage);
                    scope.spawn(move || {
                        let result = rolling.rollback_unassigned_range(
                            PartitionChunkOffset::from(0),
                            PartitionChunkOffset::from(7),
                            &[H256::repeat_byte(4)],
                        );
                        let _ = roll_tx.send(result);
                    });
                    let deadline = std::time::Instant::now() + Duration::from_millis(400);
                    let mut blocked = false;
                    while std::time::Instant::now() < deadline {
                        if storage.rollback_assignment_blocked.load(Ordering::SeqCst) {
                            blocked = true;
                            break;
                        }
                        if roll_rx.try_recv().is_ok() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    eyre::ensure!(
                        blocked,
                        "rollback did not block on the assignment lock while the registration held it"
                    );
                    storage.release_registration_pause();
                    result_rx
                        .recv_timeout(Duration::from_secs(5))
                        .map_err(|_| eyre::eyre!("registration did not finish after release"))??;
                    roll_rx
                        .recv_timeout(Duration::from_secs(10))
                        .map_err(|_| {
                            eyre::eyre!("rollback did not finish after the registration")
                        })??;
                    Ok(())
                })?;
                let rows = registration_rows(&storage)?;
                eyre::ensure!(
                    rows.is_empty(),
                    "rollback did not clear the rows the registration committed: {rows:?}"
                );
                eyre::ensure!(
                    storage.partition_assignment() == Some(original),
                    "rollback changed the assignment"
                );
                eyre::ensure!(
                    storage.registration_drops.load(Ordering::SeqCst) == 0,
                    "the registration that held the lock was dropped"
                );
                index_one(&storage, 4)?;
                let after = registration_rows(&storage)?;
                eyre::ensure!(
                    after.is_present(),
                    "registration after the waited rollback did not commit: {after:?}"
                );
                Ok(())
            })();
            if let Err(err) = outcome {
                errors.push(format!("{engine:?}: {err}"));
            }
        }
        eyre::ensure!(errors.is_empty(), "{}", errors.join(" | "));
        Ok(())
    }

    fn open_publish_module(
        prefix: &str,
        engine: SubmoduleIndexEngine,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        StorageModule,
        PartitionAssignment,
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
                intervals_persist_secs: 0,
                min_durable_interval_ms: 0,
                ..StorageSyncConfig::default()
            },
            base_directory: tmp.path().to_path_buf(),
            database: index_database(engine),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
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
                submodules: vec![(partition_chunk_offset_ii!(0, 7), tmp.path().join("sub"))],
            },
            &config,
        )?;
        Ok((tmp, storage, original))
    }

    fn paint_live_intervals(storage: &StorageModule, spans: &[(u32, u32, ChunkType)]) {
        let mut map = StorageIntervals::new();
        for &(start, end, kind) in spans {
            map.insert_merge_touching_if_values_equal(partition_chunk_offset_ii!(start, end), kind)
                .unwrap_or_else(|_| panic!("interval {start}..={end}"));
        }
        *storage.intervals.write().unwrap() = map;
    }

    fn stored_spans(storage: &StorageModule) -> Vec<(u32, u32, ChunkType)> {
        storage
            .intervals
            .read()
            .unwrap()
            .iter()
            .map(|(interval, kind)| (interval.start().0, interval.end().0, *kind))
            .collect()
    }

    fn file_spans(storage: &StorageModule, nth: usize) -> eyre::Result<Vec<(u32, u32, ChunkType)>> {
        let map = read_intervals_file(&submodule_interval_path(storage, nth))?;
        Ok(map
            .iter()
            .map(|(interval, kind)| (interval.start().0, interval.end().0, *kind))
            .collect())
    }

    fn expect_one_span_edit(
        storage: &StorageModule,
        label: &str,
        before: &[(u32, u32, ChunkType)],
        start: u32,
        end: u32,
        expected_live: &[(u32, u32, ChunkType)],
        expected_files: &[Vec<(u32, u32, ChunkType)>],
    ) -> eyre::Result<()> {
        paint_live_intervals(storage, before);
        let edits_before = storage.rollback_interval_edits.load(Ordering::SeqCst);
        storage.rollback_unassigned_range(
            PartitionChunkOffset::from(start),
            PartitionChunkOffset::from(end),
            &[],
        )?;
        let edits = storage.rollback_interval_edits.load(Ordering::SeqCst) - edits_before;
        let live = stored_spans(storage);
        eyre::ensure!(
            live == expected_live,
            "{label}: live {live:?} want {expected_live:?}"
        );
        for (nth, expected) in expected_files.iter().enumerate() {
            let file = file_spans(storage, nth)?;
            eyre::ensure!(
                file == *expected,
                "{label}: file {nth} {file:?} want {expected:?}"
            );
        }
        eyre::ensure!(edits == 2, "{label}: interval edits {edits}, want 2");
        Ok(())
    }

    /// Assigned module, four chunks, the chosen index engine.
    fn open_assigned_module(
        prefix: &str,
        engine: SubmoduleIndexEngine,
        persist_secs: u64,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        Config,
        StorageModule,
    )> {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).build();
        let chunk_size = 32_u64;
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: 4,
                entropy_packing_iterations: 1,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
                entropy_sweep_max_bytes: chunk_size * 4,
                entropy_read_grace_ms: 0,
                entropy_read_batch_bytes: chunk_size * 4,
                min_durable_interval_ms: 0,
                intervals_persist_secs: persist_secs,
                ..StorageSyncConfig::default()
            },
            database: index_database(engine),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage = StorageModule::new(&crc_hold_info(tmp_dir.path()), &config)?;
        Ok((tmp_dir, config, storage))
    }

    /// File already says `Data` for a proved chunk at offset 0.
    fn settle_proved_prefix(storage: &StorageModule) -> eyre::Result<(Vec<u8>, H256)> {
        storage.pack_with_zeros();
        let (unpacked, header, _) = index_partition(storage)?;
        storage.write_data_chunk(&unpacked[0])?;
        storage.force_sync_pending_chunks()?;
        let offset = PartitionChunkOffset::from(0);
        let bytes = storage.read_chunk_internal(offset)?;
        assert_eq!(file_label(storage, 0)?, Some(ChunkType::Data));
        assert_eq!(live_label(storage, 0), Some(ChunkType::Data));
        assert!(data_path_at(storage, 0)?.is_some());
        assert!(storage.generate_full_chunk(offset)?.is_some());
        assert!(
            !storage
                .collect_data_root_infos(header.data_root)?
                .0
                .is_empty()
        );
        assert!(
            storage
                .pending_body_migrations()?
                .iter()
                .any(|(pending, _)| pending.0 == 0)
        );
        Ok((bytes, header.data_root))
    }

    fn reopen_assigned(config: &Config, tmp: &std::path::Path) -> eyre::Result<StorageModule> {
        StorageModule::new(&crc_hold_info(tmp), config)
    }

    fn span_covers(intervals: &[Interval<PartitionChunkOffset>], offset: u32) -> bool {
        intervals
            .iter()
            .any(|interval| interval.start().0 <= offset && offset <= interval.end().0)
    }

    /// The interval replace fails before it commits. The proof index, the
    /// placement, and the file label stay as they were. Reopen does not see
    /// `Data` with an empty proof.
    #[rstest]
    // MDBX-SUBMODULE-INDEX: delete this MDBX case and keep the RocksDB case.
    #[case::mdbx(SubmoduleIndexEngine::Mdbx)]
    #[case::rocksdb(SubmoduleIndexEngine::Rocksdb)]
    fn rollback_crash_before_interval_image_keeps_the_proof(
        #[case] engine: SubmoduleIndexEngine,
    ) -> eyre::Result<()> {
        let prefix = format!("rollback_image_crash_{engine:?}");
        let (tmp, config, storage) = open_assigned_module(&prefix, engine, 30)?;
        let (bytes, data_root) = settle_proved_prefix(&storage)?;
        let offset = PartitionChunkOffset::from(0);
        let publish_before = storage.publish_generation.load(Ordering::SeqCst);
        storage.fail_next_sync_at(SyncFailurePoint::BeforeIntervalImageCommit);
        let failed = storage
            .rollback_unassigned_range(offset, PartitionChunkOffset::from(3), &[data_root])
            .expect_err("the interval replace was injected to fail");
        assert!(
            format!("{failed:#}").contains("BeforeIntervalImageCommit"),
            "{engine:?}: {failed:#}"
        );
        assert!(
            data_path_at(&storage, 0)?.is_some(),
            "{engine:?}: the proof clear committed before the interval replace"
        );
        assert_eq!(
            live_label(&storage, 0),
            Some(ChunkType::Data),
            "{engine:?}: a failed replace left the live map uninitialized"
        );
        assert_eq!(file_label(&storage, 0)?, Some(ChunkType::Data));
        assert_eq!(
            storage.publish_generation.load(Ordering::SeqCst),
            publish_before,
            "{engine:?}: a failed replace left the publish generation bumped"
        );

        storage.abandon_on_drop_for_test();
        drop(storage);
        let storage = reopen_assigned(&config, tmp.path())?;
        assert_eq!(live_label(&storage, 0), Some(ChunkType::Data));
        assert_eq!(file_label(&storage, 0)?, Some(ChunkType::Data));
        assert_eq!(storage.read_chunk_internal(offset)?, bytes);
        assert!(storage.generate_full_chunk(offset)?.is_some());
        assert!(data_path_at(&storage, 0)?.is_some());
        assert!(!storage.collect_data_root_infos(data_root)?.0.is_empty());
        assert!(
            storage
                .pending_body_migrations()?
                .iter()
                .any(|(pending, _)| pending.0 == 0)
        );
        assert!(
            !span_covers(&storage.get_intervals(ChunkType::Uninitialized), 0),
            "{engine:?}: reopen would pack a proved chunk"
        );
        drop(storage);
        drop(tmp);
        Ok(())
    }

    /// The interval image is durable and the proof clear has not started.
    /// Reopen loads `Uninitialized` and drops the proof. It does not reload
    /// `Data` with an empty proof index.
    #[rstest]
    // MDBX-SUBMODULE-INDEX: delete this MDBX case and keep the RocksDB case.
    #[case::mdbx(SubmoduleIndexEngine::Mdbx)]
    #[case::rocksdb(SubmoduleIndexEngine::Rocksdb)]
    fn rollback_crash_between_image_and_proof_clear_reloads_uninitialized(
        #[case] engine: SubmoduleIndexEngine,
    ) -> eyre::Result<()> {
        let prefix = format!("rollback_between_clear_{engine:?}");
        let (tmp, config, storage) = open_assigned_module(&prefix, engine, 30)?;
        let (_bytes, data_root) = settle_proved_prefix(&storage)?;
        let offset = PartitionChunkOffset::from(0);
        storage.fail_next_sync_at(SyncFailurePoint::AfterRollbackIntervalImage);
        let failed = storage
            .rollback_unassigned_range(offset, PartitionChunkOffset::from(3), &[data_root])
            .expect_err("rollback must stop after the interval image and before the proof clear");
        assert!(
            format!("{failed:#}").contains("AfterRollbackIntervalImage"),
            "{engine:?}: {failed:#}"
        );
        assert!(
            data_path_at(&storage, 0)?.is_some(),
            "{engine:?}: the proof clear committed before the interval image was durable"
        );
        assert_eq!(
            file_label(&storage, 0)?,
            Some(ChunkType::Uninitialized),
            "{engine:?}: the interval image was not durable while the proof was still stored"
        );
        assert_eq!(live_label(&storage, 0), Some(ChunkType::Uninitialized));

        storage.abandon_on_drop_for_test();
        drop(storage);
        let storage = reopen_assigned(&config, tmp.path())?;
        let label = live_label(&storage, 0);
        let proof = data_path_at(&storage, 0)?.is_some();
        let pack = span_covers(&storage.get_intervals(ChunkType::Uninitialized), 0);
        assert_eq!(
            label,
            Some(ChunkType::Uninitialized),
            "{engine:?}: label={label:?} proof={proof} pack={pack}"
        );
        assert_eq!(file_label(&storage, 0)?, Some(ChunkType::Uninitialized));
        assert!(!proof, "{engine:?}: open left a proof under Uninitialized");
        assert!(
            storage.collect_data_root_infos(data_root)?.0.is_empty(),
            "{engine:?}: open left the placement"
        );
        assert!(
            storage
                .pending_body_migrations()?
                .iter()
                .all(|(pending, _)| pending.0 != 0),
            "{engine:?}: open left the body job"
        );
        assert!(pack, "{engine:?}: reopen did not hand the span to packing");
        assert!(storage.generate_full_chunk(offset)?.is_none());
        assert!(!storage.is_data_chunk_durable_at(offset));
        drop(storage);
        drop(tmp);
        Ok(())
    }

    /// A finished rollback stores `Uninitialized` and then drops the proof.
    /// One span is still one cut and one insert. The publish generation is
    /// the counter that moves. Reset's epoch and the checksum-cut epoch stay.
    #[rstest]
    // MDBX-SUBMODULE-INDEX: delete this MDBX case and keep the RocksDB case.
    #[case::mdbx(SubmoduleIndexEngine::Mdbx)]
    #[case::rocksdb(SubmoduleIndexEngine::Rocksdb)]
    fn rollback_publishes_uninitialized_then_clears_the_proof(
        #[case] engine: SubmoduleIndexEngine,
    ) -> eyre::Result<()> {
        let prefix = format!("rollback_image_ok_{engine:?}");
        let (tmp, _config, storage) = open_assigned_module(&prefix, engine, 30)?;
        let (_bytes, data_root) = settle_proved_prefix(&storage)?;
        let edits_before = storage.rollback_interval_edits.load(Ordering::SeqCst);
        let image_epoch = storage.interval_image_epoch.load(Ordering::SeqCst);
        let crc_epoch = storage.crc_cut_epoch.load(Ordering::SeqCst);
        let publish_before = storage.publish_generation.load(Ordering::SeqCst);
        storage.rollback_unassigned_range(
            PartitionChunkOffset::from(0),
            PartitionChunkOffset::from(3),
            &[data_root],
        )?;
        let edits = storage.rollback_interval_edits.load(Ordering::SeqCst) - edits_before;
        assert_eq!(edits, 2, "{engine:?}: interval edits {edits}");
        assert_eq!(live_label(&storage, 0), Some(ChunkType::Uninitialized));
        assert_eq!(file_label(&storage, 0)?, Some(ChunkType::Uninitialized));
        assert!(data_path_at(&storage, 0)?.is_none());
        assert!(storage.collect_data_root_infos(data_root)?.0.is_empty());
        assert!(
            storage
                .pending_body_migrations()?
                .iter()
                .all(|(pending, _)| pending.0 != 0)
        );
        assert!(span_covers(
            &storage.get_intervals(ChunkType::Uninitialized),
            0
        ));
        assert_eq!(
            storage.interval_image_epoch.load(Ordering::SeqCst),
            image_epoch,
            "{engine:?}: rollback reused the reset epoch"
        );
        assert_eq!(
            storage.crc_cut_epoch.load(Ordering::SeqCst),
            crc_epoch,
            "{engine:?}: rollback reused the checksum-cut epoch"
        );
        assert_eq!(
            storage.publish_generation.load(Ordering::SeqCst),
            publish_before + 1,
            "{engine:?}: rollback did not bump the publish generation"
        );
        drop(storage);
        drop(tmp);
        Ok(())
    }

    /// A completion that copied the map before rollback must not write that
    /// `Data` image back over the `Uninitialized` file.
    #[rstest]
    // MDBX-SUBMODULE-INDEX: delete this MDBX case and keep the RocksDB case.
    #[case::mdbx(SubmoduleIndexEngine::Mdbx)]
    #[case::rocksdb(SubmoduleIndexEngine::Rocksdb)]
    fn stale_interval_snapshot_cannot_restore_data_after_rollback(
        #[case] engine: SubmoduleIndexEngine,
    ) -> eyre::Result<()> {
        let prefix = format!("rollback_stale_image_{engine:?}");
        let (tmp, _config, storage) = open_assigned_module(&prefix, engine, 30)?;
        let (_bytes, data_root) = settle_proved_prefix(&storage)?;
        let storage = Arc::new(storage);
        storage.pause_next_interval_image();
        let writer = Arc::clone(&storage);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| -> eyre::Result<()> {
            struct Release<'a>(&'a StorageModule);
            impl Drop for Release<'_> {
                fn drop(&mut self) {
                    self.0.release_interval_image_pause();
                }
            }
            let _release = Release(&storage);
            scope.spawn(move || {
                let _ = done_tx.send(writer.write_intervals_to_submodules());
            });
            assert!(
                storage.wait_for_interval_image_pause(Duration::from_secs(5)),
                "{engine:?}: the interval rewrite did not pause"
            );
            storage.rollback_unassigned_range(
                PartitionChunkOffset::from(0),
                PartitionChunkOffset::from(3),
                &[data_root],
            )?;
            assert_eq!(file_label(&storage, 0)?, Some(ChunkType::Uninitialized));
            assert!(data_path_at(&storage, 0)?.is_none());
            storage.release_interval_image_pause();
            done_rx
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| eyre::eyre!("{engine:?}: the paused rewrite did not finish"))??;
            assert_eq!(
                file_label(&storage, 0)?,
                Some(ChunkType::Uninitialized),
                "{engine:?}: a pre-rollback snapshot republished Data"
            );
            assert_eq!(live_label(&storage, 0), Some(ChunkType::Uninitialized));
            assert!(data_path_at(&storage, 0)?.is_none());
            Ok(())
        })?;
        drop(storage);
        drop(tmp);
        Ok(())
    }

    /// One inclusive rollback span is one cut and one insert. The labels match
    /// a per-offset walk: the cut splits the neighbors, and `Uninitialized`
    /// merges with a touching run of the same label. A one-point span is still
    /// those two tree edits. A span that crosses two submodules is still those
    /// two edits, and each file receives its clipped slice. The edit count is
    /// the number of `cut` and `insert` calls. A loop that cuts once per
    /// offset counts more than two.
    #[test]
    // MDBX-SUBMODULE-INDEX: this test opens only MDBX. Retarget it at RocksDB. The interval edit is not MDBX-specific.
    fn rollback_marks_a_span_in_one_interval_edit() -> eyre::Result<()> {
        let (tmp, storage, _) = open_publish_module("rollback_span", SubmoduleIndexEngine::Mdbx)?;
        let _tmp = tmp;
        let mut errors = Vec::new();
        let cases: Vec<(
            &str,
            Vec<(u32, u32, ChunkType)>,
            u32,
            u32,
            Vec<(u32, u32, ChunkType)>,
        )> = vec![
            (
                "one point",
                vec![(0, 7, ChunkType::Data)],
                3,
                3,
                vec![
                    (0, 2, ChunkType::Data),
                    (3, 3, ChunkType::Uninitialized),
                    (4, 7, ChunkType::Data),
                ],
            ),
            (
                "split",
                vec![(0, 7, ChunkType::Data)],
                2,
                4,
                vec![
                    (0, 1, ChunkType::Data),
                    (2, 4, ChunkType::Uninitialized),
                    (5, 7, ChunkType::Data),
                ],
            ),
            (
                "merge both sides",
                vec![
                    (0, 1, ChunkType::Uninitialized),
                    (2, 4, ChunkType::Entropy),
                    (5, 7, ChunkType::Uninitialized),
                ],
                2,
                4,
                vec![(0, 7, ChunkType::Uninitialized)],
            ),
            (
                "merge one side",
                vec![(0, 2, ChunkType::Entropy), (3, 7, ChunkType::Data)],
                3,
                5,
                vec![
                    (0, 2, ChunkType::Entropy),
                    (3, 5, ChunkType::Uninitialized),
                    (6, 7, ChunkType::Data),
                ],
            ),
            (
                "already uninitialized",
                vec![
                    (0, 1, ChunkType::Data),
                    (2, 4, ChunkType::Uninitialized),
                    (5, 7, ChunkType::Data),
                ],
                2,
                4,
                vec![
                    (0, 1, ChunkType::Data),
                    (2, 4, ChunkType::Uninitialized),
                    (5, 7, ChunkType::Data),
                ],
            ),
            (
                "interrupted",
                vec![(0, 7, ChunkType::Interrupted)],
                1,
                2,
                vec![
                    (0, 0, ChunkType::Interrupted),
                    (1, 2, ChunkType::Uninitialized),
                    (3, 7, ChunkType::Interrupted),
                ],
            ),
            (
                "whole module",
                vec![
                    (0, 2, ChunkType::Data),
                    (3, 5, ChunkType::Entropy),
                    (6, 7, ChunkType::Interrupted),
                ],
                0,
                7,
                vec![(0, 7, ChunkType::Uninitialized)],
            ),
        ];
        for (label, before, start, end, expected) in cases {
            let files = vec![expected.clone()];
            if let Err(err) =
                expect_one_span_edit(&storage, label, &before, start, end, &expected, &files)
            {
                errors.push(err.to_string());
            }
        }

        let split = (|| -> eyre::Result<()> {
            let split_tmp = TempDirBuilder::new().prefix("rollback_span_split").build();
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
                base_directory: split_tmp.path().to_path_buf(),
                database: index_database(SubmoduleIndexEngine::Mdbx),
                ..NodeConfig::testing()
            };
            let config = Config::new_with_random_peer_id(node_config);
            let storage = StorageModule::new(
                &StorageModuleInfo {
                    id: 0,
                    partition_assignment: None,
                    submodules: vec![
                        (partition_chunk_offset_ii!(0, 3), split_tmp.path().join("a")),
                        (partition_chunk_offset_ii!(4, 7), split_tmp.path().join("b")),
                    ],
                },
                &config,
            )?;
            expect_one_span_edit(
                &storage,
                "crosses submodules",
                &[(0, 7, ChunkType::Data)],
                2,
                5,
                &[
                    (0, 1, ChunkType::Data),
                    (2, 5, ChunkType::Uninitialized),
                    (6, 7, ChunkType::Data),
                ],
                &[
                    vec![(0, 1, ChunkType::Data), (2, 3, ChunkType::Uninitialized)],
                    vec![(4, 5, ChunkType::Uninitialized), (6, 7, ChunkType::Data)],
                ],
            )?;
            drop(storage);
            drop(split_tmp);
            Ok(())
        })();
        if let Err(err) = split {
            errors.push(err.to_string());
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
        std::fs::write(schema.join("SCHEMA"), b"not-this-release\n").unwrap();
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
        let config = testing_config(tmp.path(), 32, 4);
        let info = one_span(3, "sub", None);

        let rocks_err = StorageModule::open_split(
            &info,
            &config,
            Arc::clone(&split),
            BenchIndex::Rocks(RocksTuning::baseline()),
        )
        .expect_err("rocks must refuse an mdbx directory");
        assert!(rocks_err.to_string().contains("MDBX"), "{rocks_err}");

        // MDBX-SUBMODULE-INDEX: delete this MDBX bench open.
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
        let config = testing_config(tmp_dir.path(), 32, 20);

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
        let data_tx = data_header(data_root, data_size);
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
        let config = testing_config(tmp_dir.path(), chunk_size, 20);
        let storage_module =
            StorageModule::new(&one_span(19, "hdd0", Some(submit_assignment(0))), &config)?;
        let height = 4_u64;
        let place = |root: H256, at: u64| {
            let header = data_header(root, chunk_size);
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
        let config = testing_config(tmp_dir.path(), chunk_size, 20);
        let storage_module =
            StorageModule::new(&one_span(19, "hdd0", Some(submit_assignment(0))), &config)?;
        let height = 9_u64;
        let root = H256::repeat_byte(7);
        let header = data_header(root, chunk_size * 6);
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
        let later_header = data_header(later, chunk_size);
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
        let config = testing_config(tmp_dir.path(), 5, 5);
        let storage_module =
            StorageModule::new(&one_span(4, "hdd0", Some(submit_assignment(0))), &config)?;
        storage_module.pack_with_zeros();
        storage_module.force_sync_pending_chunks()?;

        let data_root = H256::random();
        let data_tx = data_header(data_root, 5);
        storage_module.index_transaction_data(
            &data_tx,
            &vec![5, 6, 7, 8],
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            0,
        )?;
        let chunk = five_byte_chunk(data_root);

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
        let config = testing_config(tmp_dir.path(), 5, 5);
        let storage_module =
            StorageModule::new(&one_span(4, "hdd0", Some(submit_assignment(0))), &config)?;
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
        let data_tx = data_header(data_root, 5);
        storage_module.index_transaction_data(
            &data_tx,
            &vec![5, 6, 7, 8],
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            0,
        )?;
        storage_module.write_data_chunk(&five_byte_chunk(data_root))?;
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
        // A long interval must not delay the sweep. The lane and `drive_group`
        // run it when the slot is queued.
        let config = testing_config_tuned(tmp_dir.path(), 5, 5, |node| {
            node.storage.entropy_sweep_interval_millis = 60_000;
        });
        let storage_module =
            StorageModule::new(&one_span(4, "hdd0", Some(submit_assignment(0))), &config)?;
        storage_module.pack_with_zeros();
        storage_module.force_sync_pending_chunks()?;
        let data_root = H256::random();
        let data_tx = data_header(data_root, 5);
        storage_module.index_transaction_data(
            &data_tx,
            &vec![5, 6, 7, 8],
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            0,
        )?;
        let chunk = five_byte_chunk(data_root);
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
        let config = testing_config(tmp_dir.path(), 5, 5);
        let storage_module =
            StorageModule::new(&one_span(4, "hdd0", Some(submit_assignment(0))), &config)?;
        let offset = PartitionChunkOffset::from(0);
        assert!(storage_module.write_chunk(offset, vec![0; 5], ChunkType::Entropy));
        let data_root = H256::random();
        let data_tx = data_header(data_root, 5);
        storage_module.index_transaction_data(
            &data_tx,
            &vec![5, 6, 7, 8],
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            0,
        )?;
        let chunk = five_byte_chunk(data_root);
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
        let config = testing_config(tmp_dir.path(), 32, 5);
        let storage_module =
            StorageModule::new(&one_span(4, "hdd0", Some(submit_assignment(0))), &config)?;
        storage_module.pack_with_zeros();

        for (ledger_offset, root_byte, path) in
            [(0_u64, 1_u8, vec![1_u8, 2, 3, 4]), (2, 2, vec![5, 6, 7, 8])]
        {
            let data_root = H256::from([root_byte; 32]);
            let data_tx = data_header(data_root, 32);
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
        let config = testing_config(tmp_dir.path(), 32, 5);
        let storage_module =
            StorageModule::new(&one_span(4, "hdd0", Some(submit_assignment(0))), &config)?;
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
            let data_tx = data_header(data_root, 32);
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

    fn grace_pack_module(
        prefix: &str,
        grace_ms: u64,
    ) -> eyre::Result<(irys_testing_utils::utils::tempfile::TempDir, StorageModule)> {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let chunk_size = 32;
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: 4,
                entropy_packing_iterations: 1,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
                entropy_sweep_max_bytes: chunk_size * 4,
                entropy_read_grace_ms: grace_ms,
                entropy_read_batch_bytes: chunk_size * 4,
                min_durable_interval_ms: 0,
                intervals_persist_secs: 0,
                ..StorageSyncConfig::default()
            },
            database: index_database(SubmoduleIndexEngine::Rocksdb),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage =
            StorageModule::new(&one_span(3, "hdd0", Some(submit_assignment(0))), &config)?;
        storage.pack_with_zeros();
        Ok((tmp_dir, storage))
    }

    fn ingest_packed(storage: &StorageModule, byte: u8) -> eyre::Result<(Vec<u8>, Option<u32>)> {
        let data_root = H256::from([byte; 32]);
        let path = vec![byte, 2, 3, 4];
        let data_tx = data_header(data_root, 32);
        storage.index_transaction_data(
            &data_tx,
            &path,
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            0,
        )?;
        storage.deposit_data_chunk(&UnpackedChunk {
            data_root,
            data_size: 32,
            data_path: path.into(),
            bytes: vec![byte; 32].into(),
            tx_offset: TxChunkOffset::from(0),
        })?;
        storage.drive_entropy_queue_for_test();
        storage.sync_pending_chunks()?;
        let loaded = storage.read_chunks(partition_chunk_offset_ii!(0, 0))?;
        let (bytes, _) = loaded
            .get(&PartitionChunkOffset::from(0))
            .expect("packed chunk");
        let crc = storage
            .offset_meta_for_test(PartitionChunkOffset::from(0))?
            .and_then(|row| row.crc);
        Ok((bytes.clone(), crc))
    }

    #[test]
    fn entropy_read_grace_does_not_change_packed_bytes() -> eyre::Result<()> {
        let (_fast_dir, fast) = grace_pack_module("grace_bytes_0", 0)?;
        let (_slow_dir, slow) = grace_pack_module("grace_bytes_40", 40)?;
        let (fast_bytes, fast_crc) = ingest_packed(&fast, 0x5A)?;
        let (slow_bytes, slow_crc) = ingest_packed(&slow, 0x5A)?;
        assert_eq!(fast_bytes, slow_bytes);
        assert_eq!(fast_crc, slow_crc);
        assert_eq!(fast_crc, Some(chunk_crc::checksum(&fast_bytes)));
        assert_eq!(fast.disk.entropy_preads.load(Ordering::SeqCst), 1);
        assert_eq!(slow.disk.entropy_preads.load(Ordering::SeqCst), 1);
        Ok(())
    }

    /// Four-chunk partition. `readahead` and `cache_bytes` are the two new knobs.
    /// Grace is 0 so a lone ingress chunk is issued in this call.
    fn readahead_module(
        prefix: &str,
        readahead: u64,
        cache_bytes: u64,
    ) -> eyre::Result<(irys_testing_utils::utils::tempfile::TempDir, StorageModule)> {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let chunk_size = 32;
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: 4,
                entropy_packing_iterations: 1,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
                entropy_sweep_max_bytes: chunk_size * 4,
                entropy_read_grace_ms: 0,
                entropy_read_batch_bytes: chunk_size * 4,
                entropy_readahead_bytes: readahead,
                entropy_readahead_cache_bytes: cache_bytes,
                min_durable_interval_ms: 0,
                intervals_persist_secs: 0,
                ..StorageSyncConfig::default()
            },
            database: index_database(SubmoduleIndexEngine::Rocksdb),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(irys_types::partition::PartitionAssignment {
                    ledger_id: Some(DataLedger::Submit.into()),
                    slot_index: Some(0),
                    miner_address: irys_types::IrysAddress::from([0xAA; 20]),
                    partition_hash: H256::repeat_byte(0x11),
                }),
                // Absolute. `assign_partition` writes `packing_params.toml` at
                // `submodule.path`, and a relative name would miss this temp dir.
                submodules: vec![(
                    partition_chunk_offset_ii!(0, 3),
                    tmp_dir.path().join("hdd0"),
                )],
            },
            &config,
        )?;
        Ok((tmp_dir, storage))
    }

    fn index_four_chunks(storage: &StorageModule) -> eyre::Result<Vec<UnpackedChunk>> {
        let signer = IrysSigner::random_signer(&storage.config.consensus);
        let data = (0..4_u8)
            .flat_map(|byte| vec![byte; 32])
            .collect::<Vec<_>>();
        let tx = signer.sign_transaction(signer.create_transaction(data, H256::zero())?)?;
        let unpacked = tx.data_chunks()?;
        assert_eq!(unpacked.len(), 4, "one tx covers the partition");
        let (_, proofs) = DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&tx.header));
        storage.index_transaction_data(
            &tx.header,
            &proofs[0].proof,
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 3)),
            0,
        )?;
        Ok(unpacked)
    }

    /// The same partition index as [`index_four_chunks`], plus the header and
    /// tx path a later reopen uses to put the placement back.
    fn index_partition(
        storage: &StorageModule,
    ) -> eyre::Result<(Vec<UnpackedChunk>, DataTransactionHeader, TxPath)> {
        let signer = IrysSigner::random_signer(&storage.config.consensus);
        let data = (0..4_u8)
            .flat_map(|byte| vec![byte; 32])
            .collect::<Vec<_>>();
        let tx = signer.sign_transaction(signer.create_transaction(data, H256::zero())?)?;
        let unpacked = tx.data_chunks()?;
        assert_eq!(unpacked.len(), 4, "one tx covers the partition");
        let (_, proofs) = DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&tx.header));
        let tx_path = proofs[0].proof.clone();
        storage.index_transaction_data(
            &tx.header,
            &tx_path,
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 3)),
            0,
        )?;
        Ok((unpacked, tx.header, tx_path))
    }

    fn synced_packed_rows(storage: &StorageModule) -> eyre::Result<Vec<(Vec<u8>, Option<u32>)>> {
        storage.sync_pending_chunks()?;
        let loaded = storage.read_chunks(partition_chunk_offset_ii!(0, 3))?;
        let mut rows = Vec::new();
        for offset in 0..4_u32 {
            let key = PartitionChunkOffset::from(offset);
            let (bytes, kind) = loaded.get(&key).expect("packed chunk");
            assert_eq!(*kind, ChunkType::Data, "offset {offset}");
            let crc = storage.offset_meta_for_test(key)?.and_then(|row| row.crc);
            rows.push((bytes.clone(), crc));
        }
        Ok(rows)
    }

    #[test]
    fn readahead_hit_avoids_a_second_read() -> eyre::Result<()> {
        let chunk = 32_u64;
        let (_dir, storage) = readahead_module("readahead_hit", chunk * 4, chunk * 4)?;
        storage.pack_with_zeros();
        let unpacked = index_four_chunks(&storage)?;
        // Offset 2 arrives before 1. The first pread already covers both.
        storage.write_data_chunk(&unpacked[0])?;
        assert_eq!(storage.disk.entropy_preads.load(Ordering::SeqCst), 1);
        assert_eq!(
            storage.disk.entropy_pread_bytes.load(Ordering::SeqCst),
            chunk * 4
        );
        assert_eq!(storage.entropy_cache_offsets_for_test(), vec![1, 2, 3]);
        for index in [2_usize, 1, 3] {
            storage.write_data_chunk(&unpacked[index])?;
        }
        assert_eq!(storage.disk.entropy_preads.load(Ordering::SeqCst), 1);
        assert_eq!(
            storage.disk.entropy_readahead_hits.load(Ordering::SeqCst),
            3
        );
        assert_eq!(
            storage.disk.entropy_readahead_stores.load(Ordering::SeqCst),
            3
        );
        assert!(storage.entropy_cache_offsets_for_test().is_empty());
        Ok(())
    }

    #[test]
    fn migration_deposit_does_not_read_ahead() -> eyre::Result<()> {
        let chunk = 32_u64;
        let (_dir, storage) = readahead_module("readahead_migration", chunk * 4, chunk * 4)?;
        storage.pack_with_zeros();
        let unpacked = index_four_chunks(&storage)?;
        storage.deposit_data_chunk(&unpacked[0])?;
        storage.drive_entropy_queue_for_test();
        assert_eq!(storage.disk.entropy_preads.load(Ordering::SeqCst), 1);
        assert_eq!(
            storage.disk.entropy_pread_bytes.load(Ordering::SeqCst),
            chunk
        );
        assert_eq!(
            storage.disk.entropy_readahead_stores.load(Ordering::SeqCst),
            0
        );
        assert!(storage.entropy_cache_offsets_for_test().is_empty());
        Ok(())
    }

    #[test]
    fn readahead_cache_drops_on_reset_reassignment_and_write() -> eyre::Result<()> {
        let chunk = 32_u64;
        {
            let (_dir, storage) = readahead_module("readahead_reset", chunk * 4, chunk * 4)?;
            storage.pack_with_zeros();
            let unpacked = index_four_chunks(&storage)?;
            storage.write_data_chunk(&unpacked[0])?;
            assert_eq!(storage.entropy_cache_offsets_for_test(), vec![1, 2, 3]);
            storage.reset()?;
            assert!(storage.entropy_cache_offsets_for_test().is_empty());
        }
        {
            let (_dir, storage) = readahead_module("readahead_reassign", chunk * 4, chunk * 4)?;
            storage.pack_with_zeros();
            let unpacked = index_four_chunks(&storage)?;
            storage.write_data_chunk(&unpacked[0])?;
            let assignment = storage.partition_assignment().expect("assigned");
            storage.assign_partition(assignment, 7)?;
            assert_eq!(
                storage.entropy_cache_offsets_for_test(),
                vec![1, 2, 3],
                "a height update keeps pads for the same partition hash"
            );
            let mut changed = assignment;
            changed.partition_hash = H256::repeat_byte(0x5C);
            storage.assign_partition(changed, 8)?;
            assert!(storage.entropy_cache_offsets_for_test().is_empty());
        }
        {
            let (_dir, storage) = readahead_module("readahead_write", chunk * 4, chunk * 4)?;
            storage.pack_with_zeros();
            let unpacked = index_four_chunks(&storage)?;
            storage.write_data_chunk(&unpacked[0])?;
            assert!(storage.write_chunk(
                PartitionChunkOffset::from(2),
                vec![0_u8; 32],
                ChunkType::Entropy,
            ));
            assert_eq!(storage.entropy_cache_offsets_for_test(), vec![1, 3]);
            // The entropy overwrite has to reach the file. A pad still in
            // `pending` would pack this chunk with no second pread.
            storage.sync_pending_chunks()?;
            storage.write_data_chunk(&unpacked[2])?;
            assert_eq!(storage.disk.entropy_preads.load(Ordering::SeqCst), 2);
            assert!(!storage.entropy_cache_offsets_for_test().contains(&2));
            Ok(())
        }
    }

    #[test]
    fn readahead_does_not_change_packed_bytes() -> eyre::Result<()> {
        let chunk = 32_u64;
        let (_off_dir, off) = readahead_module("readahead_bytes_off", 0, 0)?;
        let (_on_dir, on) = readahead_module("readahead_bytes_on", chunk * 4, chunk * 4)?;
        for storage in [&off, &on] {
            storage.pack_with_zeros();
            let unpacked = index_four_chunks(storage)?;
            for body in &unpacked {
                storage.write_data_chunk(body)?;
            }
        }
        let off_rows = synced_packed_rows(&off)?;
        let on_rows = synced_packed_rows(&on)?;
        assert_eq!(off_rows, on_rows);
        for (bytes, crc) in &on_rows {
            assert_eq!(*crc, Some(chunk_crc::checksum(bytes)));
        }
        assert_eq!(off.disk.entropy_preads.load(Ordering::SeqCst), 4);
        assert_eq!(on.disk.entropy_preads.load(Ordering::SeqCst), 1);
        assert_eq!(
            on.disk.entropy_pread_bytes.load(Ordering::SeqCst),
            chunk * 4
        );
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
                entropy_read_grace_ms: 0,
                entropy_read_batch_bytes: sweep_bytes,
                entropy_readahead_bytes: 0,
                entropy_readahead_cache_bytes: 0,
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

    /// The test panic hook aborts the process. An injected panic must not.
    fn quiet_panics<R>(body: impl FnOnce() -> R) -> R {
        quiet_panics_with(|_| {}, body)
    }

    /// Same as [`quiet_panics`], and `on_panic` runs from the panic hook.
    /// The hook must not panic: a second panic aborts the process.
    fn quiet_panics_with<R>(
        on_panic: impl Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static,
        body: impl FnOnce() -> R,
    ) -> R {
        struct RestoreHook(
            Option<Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>>,
        );
        impl Drop for RestoreHook {
            fn drop(&mut self) {
                if let Some(hook) = self.0.take() {
                    std::panic::set_hook(hook);
                }
            }
        }
        let _restore = RestoreHook(Some(std::panic::take_hook()));
        std::panic::set_hook(Box::new(on_panic));
        body()
    }

    /// `try_lock` on `sync_in_progress`. `held` is a live guard. `poisoned`
    /// is a guard that was dropped while its thread was unwinding.
    fn classify_sync_lock(module: &StorageModule) -> &'static str {
        if module.sync_in_progress.is_poisoned() {
            return "poisoned";
        }
        match module.sync_in_progress.try_lock() {
            Ok(guard) => {
                drop(guard);
                "free"
            }
            Err(std::sync::TryLockError::WouldBlock) => "held",
            Err(std::sync::TryLockError::Poisoned(_)) => "poisoned",
        }
    }

    /// A panic while `sync_in_progress` is held must not stop `reset` or a
    /// later commit. The mutex payload is `()`.
    #[test]
    fn poisoned_sync_lock_still_resets_and_commits_a_write() -> eyre::Result<()> {
        let (_tmp, _config, module) =
            open_presence_module("sync_poison_recover", SubmoduleIndexEngine::Rocksdb)?;
        let poisoned = quiet_panics(|| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = module.sync_in_progress.lock().unwrap();
                panic!("injected sync lock poison");
            }))
            .is_err()
        });
        assert!(poisoned, "the sync lock was not poisoned");
        module.reset()?;
        let offset = PartitionChunkOffset::from(0);
        assert!(module.write_chunk(offset, vec![0x11_u8; 32], ChunkType::Entropy));
        module.sync_pending_chunks()?;
        assert!(
            module.offset_meta_for_test(offset)?.is_some(),
            "a commit after a poisoned sync lock did not store the row"
        );
        assert!(
            module.sync_in_progress.try_lock().is_ok(),
            "the sync lock stayed poisoned"
        );
        Ok(())
    }

    /// A pwrite worker panic is an error from the flush. The lane thread
    /// stays up. The panic does not poison `sync_in_progress`: the lane
    /// holds that lock on the parent thread, and `catch_unwind` stops the
    /// worker from unwinding it.
    ///
    /// The chunk stays pending, so the next commit writes it and holds the
    /// lock again. `try_lock` is `WouldBlock` for that whole commit. The
    /// hold is not poison. That commit still stores the row.
    #[test]
    fn lane_pwrite_panic_returns_and_keeps_the_sync_lock() -> eyre::Result<()> {
        let (_tmp, _config, module) =
            open_presence_module("lane_pwrite_panic", SubmoduleIndexEngine::Rocksdb)?;
        let module = std::sync::Arc::new(module);
        let lane = LaneGuard::start(std::sync::Arc::clone(&module))?;
        let offset = PartitionChunkOffset::from(1);
        // The successful pwrite, not the panicking one, stops here. The
        // lane thread is inside `join` and still holds `sync_in_progress`.
        module.pause_next_sync_at(SyncPausePoint::AfterPwrite);
        module.panic_next_pwrite_for_test();

        let poisoned_during_panic = std::sync::Arc::new(AtomicBool::new(false));
        let hook_ran = std::sync::Arc::new(AtomicBool::new(false));
        let module_watch = std::sync::Arc::clone(&module);
        let (obs_tx, obs_rx) = std::sync::mpsc::channel();
        let _watch = std::thread::spawn(move || {
            let arrived = module_watch.wait_for_sync_pause(Duration::from_secs(15));
            let kind = classify_sync_lock(&module_watch);
            let poisoned = module_watch.sync_in_progress.is_poisoned();
            module_watch.release_sync_pause();
            let _ = obs_tx.send((arrived, kind, poisoned));
        });

        let module_flush = std::sync::Arc::clone(&module);
        let poisoned_flag = std::sync::Arc::clone(&poisoned_during_panic);
        let hook_flag = std::sync::Arc::clone(&hook_ran);
        let module_hook = std::sync::Arc::clone(&module);
        let (tx, rx) = std::sync::mpsc::channel();
        let first = quiet_panics_with(
            move |info| {
                let message = info.payload().downcast_ref::<&str>().copied().unwrap_or("");
                if message == "injected chunk write panic" {
                    hook_flag.store(true, Ordering::SeqCst);
                    // Parent of the worker still holds `sync_in_progress`.
                    // The worker does not. Poison is set only when that
                    // guard drops during unwind.
                    poisoned_flag
                        .store(module_hook.sync_in_progress.is_poisoned(), Ordering::SeqCst);
                }
            },
            || {
                assert!(module.write_chunk(offset, vec![0x44_u8; 32], ChunkType::Entropy));
                let _flush = std::thread::spawn(move || {
                    let result = module_flush.force_sync_pending_chunks();
                    let _ = tx.send(result);
                });
                // The pause can sit inside this flush. The watcher releases
                // it. Wait for that observation before the flush result.
                let observed = obs_rx
                    .recv_timeout(Duration::from_secs(15))
                    .expect("pwrite pause observation was dropped");
                let flushed = rx
                    .recv_timeout(Duration::from_secs(15))
                    .unwrap_or_else(|_| panic!("lane flush did not return after a worker panic"));
                (flushed, observed)
            },
        );
        let (first, (arrived, kind, poisoned)) = first;
        assert!(
            hook_ran.load(Ordering::SeqCst),
            "the injected panic did not reach the hook: {first:?}"
        );
        assert!(
            !poisoned_during_panic.load(Ordering::SeqCst),
            "the worker panic poisoned the sync lock while the lane still held it: {first:?}"
        );
        assert!(
            module.disk_lane_running(),
            "a pwrite panic killed the lane thread: {first:?}"
        );
        if let Err(error) = &first {
            assert!(
                error.to_string().contains("chunk write panicked"),
                "unexpected flush error: {error}"
            );
        }
        assert!(
            arrived,
            "the pwrite after the panic did not reach AfterPwrite"
        );
        assert!(!poisoned, "AfterPwrite saw a poisoned sync lock: {first:?}");
        // The successful pwrite runs on a worker. The lane thread is in
        // `join` and still holds `sync_in_progress`. `WouldBlock` is that
        // hold. It is not poison.
        assert_eq!(
            kind, "held",
            "a pwrite panic poisoned the sync lock: {kind} {first:?}"
        );

        let stored = Instant::now() + Duration::from_secs(15);
        while module.offset_meta_for_test(offset)?.is_none() {
            assert_ne!(
                classify_sync_lock(&module),
                "poisoned",
                "the commit after the panic poisoned the sync lock"
            );
            assert!(
                Instant::now() < stored,
                "the commit after a pwrite panic did not store the row: {first:?}"
            );
            std::thread::yield_now();
        }
        let free = Instant::now() + Duration::from_secs(15);
        loop {
            match classify_sync_lock(&module) {
                "free" => break,
                "poisoned" => panic!("the sync lock stayed poisoned after the commit"),
                "held" => {
                    assert!(
                        Instant::now() < free,
                        "the sync lock stayed held after the row was stored"
                    );
                    std::thread::yield_now();
                }
                other => panic!("unexpected sync lock state {other}"),
            }
        }
        module.force_sync_pending_chunks()?;
        assert!(module.offset_meta_for_test(offset)?.is_some());
        // `finish_lane_request` wakes this caller before the lane pass
        // returns. A later step in that pass may hold the lock. That hold
        // ends. It is not poison.
        let settled = Instant::now() + Duration::from_secs(15);
        loop {
            match classify_sync_lock(&module) {
                "free" => break,
                "poisoned" => panic!("the sync lock stayed poisoned after the later flush"),
                "held" => {
                    assert!(
                        Instant::now() < settled,
                        "the sync lock stayed held after the later flush"
                    );
                    std::thread::yield_now();
                }
                other => panic!("unexpected sync lock state {other}"),
            }
        }
        drop(lane);
        Ok(())
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
        let config = testing_config(&base_path, 32, 20);

        let storage_module_info = &infos[0];
        let storage_module = StorageModule::new(storage_module_info, &config)?;

        let params_path = base_path.join("hdd0-4TB").join("packing_params.toml");
        let params = PackingParams::from_toml(params_path).expect("packing params to load");
        assert_eq!(params.partition_hash, None);

        let unpacked = storage_module.get_intervals(ChunkType::Uninitialized);
        assert_eq!(unpacked, [partition_chunk_offset_ii!(0, 19)]);

        let entropy_chunk = vec![0xff; 32];
        storage_module.write_chunk(
            PartitionChunkOffset::from(1),
            entropy_chunk.clone(),
            ChunkType::Entropy,
        );

        let _ = storage_module.sync_pending_chunks();

        let unpacked = storage_module.get_intervals(ChunkType::Uninitialized);
        assert_eq!(
            unpacked,
            [
                partition_chunk_offset_ii!(0, 0),
                partition_chunk_offset_ii!(2, 19)
            ]
        );

        let packed = storage_module.get_intervals(ChunkType::Entropy);
        assert_eq!(packed, [partition_chunk_offset_ii!(1, 1)]);

        let chunks = storage_module
            .read_chunks(partition_chunk_offset_ii!(1, 1))
            .unwrap();
        let chunk = chunks.get(&PartitionChunkOffset::from(1)).unwrap();
        assert_eq!(*chunk, (entropy_chunk.clone(), ChunkType::Entropy));

        let chunks = storage_module
            .read_chunks(partition_chunk_offset_ii!(1, 2))
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(*chunk, (entropy_chunk.clone(), ChunkType::Entropy));

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

        let num_pending_writes: usize;
        {
            num_pending_writes = storage_module.pending_writes.read().unwrap().len();
        }
        assert_eq!(num_pending_writes, 2);

        let _ = storage_module.sync_pending_chunks();

        let data = storage_module.get_intervals(ChunkType::Data);
        assert_eq!(data, [partition_chunk_offset_ii!(4, 5)]);

        let unpacked = storage_module.get_intervals(ChunkType::Uninitialized);
        assert_eq!(
            unpacked,
            [
                partition_chunk_offset_ii!(0, 0),
                partition_chunk_offset_ii!(2, 3),
                partition_chunk_offset_ii!(6, 19)
            ]
        );

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
        let intervals = storage_module.reset().unwrap();

        // The hole storage interval is returned
        assert_eq!(intervals, partition_chunk_offset_ii!(0, 19));

        let unpacked = storage_module.get_intervals(ChunkType::Uninitialized);
        assert_eq!(unpacked, [partition_chunk_offset_ii!(0, 19)]);

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
        let config = testing_config(tmp_dir.path(), 32, 20);
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
        let infos = [one_span(
            9,
            "hdd0-test",
            Some(PartitionAssignment::default()),
        )];
        let tmp_dir = TempDirBuilder::new()
            .prefix("drop_pending_writes_test")
            .build();
        let config = testing_config(tmp_dir.path(), 32, 10);
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

        let infos = [one_span(
            9,
            "hdd0-test",
            Some(PartitionAssignment::default()),
        )];
        let tmp_dir = TempDirBuilder::new()
            .prefix("clear_dri_extent_test")
            .build();
        let config = testing_config(tmp_dir.path(), 32, 10);
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
        let config = testing_config(tmp_dir.path(), chunk_size, 100);
        let sm = StorageModule::new(
            &one_span(99, "hdd0", Some(PartitionAssignment::default())),
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

        let infos = [one_span(
            9,
            "hdd0-test",
            Some(PartitionAssignment::default()),
        )];
        let tmp_dir = TempDirBuilder::new()
            .prefix("clear_offset_index_test")
            .build();
        let config = testing_config(tmp_dir.path(), 32, 10);
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

        let storage_module_info = &infos[0];
        let storage_module = StorageModule::new(storage_module_info, &config)?;

        let entropy_bytes = vec![0_u8; chunk_size];
        for chunk_offset in 0..10_u32 {
            storage_module.write_chunk(
                PartitionChunkOffset::from(chunk_offset),
                entropy_bytes.clone(),
                ChunkType::Entropy,
            );
        }

        storage_module.sync_pending_chunks()?;
        assert!(
            (0..10_u32)
                .map(PartitionChunkOffset::from)
                .all(|offset| !storage_module.is_data_chunk_durable_at(offset)),
            "entropy writes must never be reported as durable transaction data"
        );

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

            let (pending_chunk_bytes, pending_chunk_type) = pending.get(&chunk_offset).unwrap();
            assert_eq!(pending_chunk_bytes, &bytes);
            assert_eq!(*pending_chunk_type, ChunkType::Data);
        }

        // Test - write a data chunk that overwrites a stored entropy chunk
        let bytes = vec![20_u8; chunk_size];
        let chunk_offset = PartitionChunkOffset::from(2);
        storage_module.write_chunk(chunk_offset, bytes, ChunkType::Data);

        {
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
            let data = storage_module.get_intervals(ChunkType::Data);
            assert_eq!(data.len(), 3);
            assert_eq!(data[0], partition_chunk_offset_ii!(2, 2));
            assert_eq!(data[1], partition_chunk_offset_ii!(11, 11));
            assert_eq!(data[2], partition_chunk_offset_ii!(19, 19));

            let uninitialized = storage_module.get_intervals(ChunkType::Uninitialized);
            assert_eq!(uninitialized.len(), 1);
            assert_eq!(uninitialized[0], partition_chunk_offset_ii!(21, 50));
        }

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
            let pending = storage_module
                .pending_writes
                .read()
                .expect("to read pending writes");

            assert_eq!(pending.len(), 0);
        }

        let after_chunks = storage_module
            .read_chunks(read_range)
            .expect("to read chunks");

        assert_eq!(
            before_chunks.len(),
            after_chunks.len(),
            "Maps have different sizes"
        );

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
        let config = testing_config(&base_path, 5, 5);

        let storage_module_info = &infos[0];
        let storage_module = StorageModule::new(storage_module_info, &config)?;
        let chunk_data = vec![0, 1, 2, 3, 4];
        let data_path = vec![4, 3, 2, 1];
        let tx_path = vec![5, 6, 7, 8];
        let data_root = H256::zero();
        let data_size = chunk_data.len() as u64;

        storage_module.pack_with_zeros();

        let data_tx = data_header(data_root, data_size);

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

        let _intervals = storage_module.reset().unwrap();

        let (tx_path, ret_path) = storage_module.read_tx_data_path(LedgerChunkOffset::from(0))?;

        assert!(tx_path.is_none());
        assert!(ret_path.is_none());

        Ok(())
    }

    #[test]
    fn test_write_interruption_flow() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("write_interruption_test")
            .with_tracing()
            .build();
        let base_path = tmp_dir.path().to_path_buf();

        let infos = [one_span(10, "test-submodule", None)];

        let config = testing_config(&base_path, 32, 11);

        let storage_module = StorageModule::new(&infos[0], &config)?;

        let uninitialized = storage_module.get_intervals(ChunkType::Uninitialized);
        assert_eq!(uninitialized, [partition_chunk_offset_ii!(0, 10)]);

        let test_chunk = vec![0x42; 32];
        storage_module.write_chunk_internal(
            PartitionChunkOffset::from(5),
            test_chunk,
            ChunkType::Data,
        )?;

        let data = storage_module.get_intervals(ChunkType::Data);
        assert_eq!(data, [partition_chunk_offset_ii!(5, 5)]);

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

        storage_module.write_intervals_to_submodules()?;

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

        let infos = [one_span(10, "test-submodule", None)];

        let config = testing_config(&base_path, 32, 11);

        let storage_module = StorageModule::new(&infos[0], &config)?;

        let data_chunk = vec![0xDA; 32];
        let entropy_chunk = vec![0xEE; 32];

        storage_module.write_chunk_internal(
            PartitionChunkOffset::from(3),
            data_chunk,
            ChunkType::Data,
        )?;

        storage_module.write_chunk_internal(
            PartitionChunkOffset::from(7),
            entropy_chunk,
            ChunkType::Entropy,
        )?;

        let data_intervals = storage_module.get_intervals(ChunkType::Data);
        assert_eq!(data_intervals, [partition_chunk_offset_ii!(3, 3)]);

        let entropy_intervals = storage_module.get_intervals(ChunkType::Entropy);
        assert_eq!(entropy_intervals, [partition_chunk_offset_ii!(7, 7)]);

        let interrupted = storage_module.get_intervals(ChunkType::Interrupted);
        assert_eq!(interrupted, vec![]);

        let uninitialized = storage_module.get_intervals(ChunkType::Uninitialized);
        assert_eq!(
            uninitialized,
            [
                partition_chunk_offset_ii!(0, 2),
                partition_chunk_offset_ii!(4, 6),
                partition_chunk_offset_ii!(8, 10),
            ]
        );

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

        let infos = [one_span(10, "test-submodule", None)];

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

            storage_module.write_intervals_to_submodules()?;

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

        let interrupted = storage_module.get_intervals(ChunkType::Interrupted);
        assert_eq!(
            interrupted,
            vec![],
            "Interrupted chunks should be reset on load"
        );

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
        let config = testing_config_tuned(tmp_dir.path(), 32, 11, |node| {
            node.storage = StorageSyncConfig {
                num_writes_before_sync: 1,
                intervals_persist_secs: persist_secs,
                min_durable_interval_ms: 0,
                ..StorageSyncConfig::default()
            };
        });
        let storage = StorageModule::new(&one_span(10, "sub", None), &config)?;
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
        let opened = StorageModule::new(&one_span(10, "sub", None), &config);
        let err = opened.expect_err("a corrupt intervals.json must not panic");
        let message = format!("{err:#}");
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(
            message.contains("clear this submodule directory (index, intervals.json, chunks.dat)"),
            "{message}"
        );
        drop(tmp);
        Ok(())
    }

    #[test]
    fn write_throughput_ignores_samples_older_than_the_window() -> eyre::Result<()> {
        let (_tmp, _config, storage) = interval_batch_module("throughput_window", 0)?;
        let chunk = storage.config.consensus.chunk_size;
        let start = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(30))
            .expect("the clock can represent 30s ago");
        let completion = start + std::time::Duration::from_millis(10);
        {
            let mut samples = storage.recent_chunk_times.write().unwrap();
            for index in 0..40_u32 {
                samples.push(ChunkTimeRecord {
                    chunk_offset: PartitionChunkOffset::from(index),
                    start_time: start,
                    completion_time: completion,
                    duration: std::time::Duration::from_millis(10),
                });
            }
        }
        assert_eq!(
            storage.write_throughput_bps(),
            0,
            "a fast window older than 10s must not keep throttling"
        );

        let now = std::time::Instant::now();
        let fresh_start = now
            .checked_sub(std::time::Duration::from_secs(2))
            .expect("the clock can represent 2s ago");
        storage
            .recent_chunk_times
            .write()
            .unwrap()
            .push(ChunkTimeRecord {
                chunk_offset: PartitionChunkOffset::from(0),
                start_time: fresh_start,
                completion_time: now,
                duration: std::time::Duration::from_secs(2),
            });
        // One chunk over 2s, same rounding as the meter. The 40 stale samples stay out.
        let secs = 2;
        let expected = (chunk + secs / 2) / secs;
        assert_eq!(storage.write_throughput_bps(), expected);
        Ok(())
    }

    #[test]
    fn corrupt_packing_params_names_its_path() -> eyre::Result<()> {
        let (tmp, config, storage) = interval_batch_module("corrupt_packing", 0)?;
        let path = tmp.path().join("sub").join(PACKING_PARAMS_FILE_NAME);
        drop(storage);
        fs::write(&path, b"not toml {{{")?;
        let opened = StorageModule::new(&one_span(10, "sub", None), &config);
        let err = opened.expect_err("a corrupt packing_params.toml must not panic");
        let message = format!("{err:#}");
        assert!(message.contains(&path.display().to_string()), "{message}");
        drop(tmp);
        Ok(())
    }

    /// A later assignment must not panic, and must not publish, when the
    /// file on disk no longer parses.
    #[test]
    fn corrupt_packing_params_fail_assignment_store() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new()
            .prefix("corrupt_packing_store")
            .build();
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size: 32,
                num_chunks_in_partition: 4,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1_000,
                ..StorageSyncConfig::default()
            },
            base_directory: tmp.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        // Absolute. `store_assignment` writes `submodule.path`, not
        // `base_directory.join`.
        let sub = tmp.path().join("sub");
        let storage = StorageModule::new(&one_span(3, sub.clone(), None), &config)?;
        let path = sub.join(PACKING_PARAMS_FILE_NAME);
        fs::write(&path, b"not toml {{{")?;
        let err = storage
            .assign_partition(
                PartitionAssignment {
                    partition_hash: H256::repeat_byte(0x44),
                    miner_address: config.node_config.miner_address(),
                    ledger_id: None,
                    slot_index: None,
                },
                2,
            )
            .expect_err("a corrupt packing_params.toml must not panic the assignment store");
        let message = format!("{err:#}");
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(storage.partition_assignment().is_none());
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
        let config = testing_config_tuned(tmp_dir.path(), 32, 8, |node| {
            node.storage = StorageSyncConfig {
                num_writes_before_sync: 1,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 0,
                intervals_persist_secs: persist_secs,
                min_durable_interval_ms: 60_000,
                ..StorageSyncConfig::default()
            };
            node.database = index_database(SubmoduleIndexEngine::Rocksdb);
        });
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

        let reloaded = StorageModule::new(&one_span(10, "sub", None), &config)?;
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
            Some(ChunkType::Entropy)
        );
        drop(storage);

        let reloaded = StorageModule::new(&one_span(10, "sub", None), &config)?;
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

    fn indexed_entropy_info(config: &Config) -> StorageModuleInfo {
        one_span(
            10,
            "sub",
            Some(submit_slot(config, H256::repeat_byte(0x5A), 0)),
        )
    }

    /// Entropy across 0..=10, one tx at offset 5 only. The data write under
    /// test is offset 0, so a crash fence on that run must not overlap the tx.
    fn indexed_entropy_module(
        prefix: &str,
        engine: SubmoduleIndexEngine,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        Config,
        StorageModule,
        H256,
    )> {
        let tmp = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let config = testing_config_tuned(tmp.path(), 32, 11, |node| {
            node.storage = StorageSyncConfig {
                num_writes_before_sync: 1,
                intervals_persist_secs: 30,
                min_durable_interval_ms: 0,
                ..StorageSyncConfig::default()
            };
            node.database = index_database(engine);
        });
        let storage = StorageModule::new(&indexed_entropy_info(&config), &config)?;
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
        let data_root = H256::repeat_byte(0x71);
        storage.index_transaction_data(
            &data_header(data_root, config.consensus.chunk_size),
            &vec![9, 8, 7, 6],
            LedgerChunkRange(ledger_chunk_offset_ii!(5, 5)),
            40,
        )?;
        for (_, submodule) in storage.submodules.iter() {
            submodule.db.sync_group()?;
        }
        assert_indexed_entropy(&storage, data_root)?;
        Ok((tmp, config, storage, data_root))
    }

    fn assert_indexed_entropy(storage: &StorageModule, data_root: H256) -> eyre::Result<()> {
        let offset = PartitionChunkOffset::from(5);
        let (_, submodule) = storage.get_submodule_for_offset(offset)?;
        let rows = submodule
            .db
            .view(|tx| tx.tx_intervals_in_range(offset, offset))?;
        assert!(!rows.is_empty(), "tx interval at offset 5 is missing");
        let jobs = storage.pending_body_migrations()?;
        assert_eq!(jobs.len(), 1, "pending body job count");
        assert_eq!(jobs[0].0, offset);
        let infos = storage.collect_data_root_infos(data_root)?;
        assert!(!infos.0.is_empty(), "data-root info is missing");
        Ok(())
    }

    /// After fdatasync and the presence commit, a kill must keep entropy the
    /// run did not write. The run itself stays `Interrupted` until the
    /// completion period, so reopen still treats that offset as unpacked.
    #[rstest]
    // MDBX-SUBMODULE-INDEX: delete this MDBX case and keep the RocksDB case.
    #[case::mdbx(SubmoduleIndexEngine::Mdbx)]
    #[case::rocksdb(SubmoduleIndexEngine::Rocksdb)]
    fn crash_reload_of_write_ahead_keeps_indexed_entropy(
        #[case] engine: SubmoduleIndexEngine,
    ) -> eyre::Result<()> {
        let prefix = format!("ahead_entropy_done_{engine:?}");
        let (tmp, config, storage, data_root) = indexed_entropy_module(&prefix, engine)?;
        let written = PartitionChunkOffset::from(0);
        assert!(storage.write_chunk(written, vec![7_u8; 32], ChunkType::Data));
        storage.sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(written));
        assert_eq!(live_label(&storage, 5), Some(ChunkType::Entropy));
        assert_eq!(
            file_label(&storage, 0)?,
            Some(ChunkType::Interrupted),
            "{engine:?}: the run fence was published before the completion period"
        );
        assert_eq!(
            file_label(&storage, 5)?,
            Some(ChunkType::Entropy),
            "{engine:?}: the durable point marked entropy the run did not write"
        );
        storage.abandon_on_drop_for_test();
        drop(storage);

        let reopened = StorageModule::new(&indexed_entropy_info(&config), &config)?;
        assert_eq!(
            live_label(&reopened, 0),
            Some(ChunkType::Uninitialized),
            "{engine:?}: reopen published the run before its completion"
        );
        assert_eq!(
            live_label(&reopened, 5),
            Some(ChunkType::Entropy),
            "{engine:?}: reopen relabelled entropy the run did not write"
        );
        assert_indexed_entropy(&reopened, data_root)?;
        drop(reopened);
        drop(tmp);
        Ok(())
    }

    /// A crash after the write-ahead and before fdatasync still reloads the
    /// run as `Uninitialized`. Entropy after the run keeps its label and rows.
    #[rstest]
    // MDBX-SUBMODULE-INDEX: delete this MDBX case and keep the RocksDB case.
    #[case::mdbx(SubmoduleIndexEngine::Mdbx)]
    #[case::rocksdb(SubmoduleIndexEngine::Rocksdb)]
    fn crash_during_the_run_keeps_the_write_fence(
        #[case] engine: SubmoduleIndexEngine,
    ) -> eyre::Result<()> {
        let prefix = format!("ahead_entropy_inflight_{engine:?}");
        let (tmp, config, storage, data_root) = indexed_entropy_module(&prefix, engine)?;
        storage.fail_next_sync_at(SyncFailurePoint::BeforeDataFsync);
        assert!(storage.write_chunk(
            PartitionChunkOffset::from(0),
            vec![7_u8; 32],
            ChunkType::Data,
        ));
        let failed = storage
            .sync_pending_chunks()
            .expect_err("chunk fsync was injected to fail");
        assert!(
            format!("{failed:#}").contains("BeforeDataFsync"),
            "{engine:?}: {failed:#}"
        );
        assert_eq!(
            file_label(&storage, 0)?,
            Some(ChunkType::Interrupted),
            "{engine:?}: the in-flight run was not fenced"
        );
        assert_eq!(
            file_label(&storage, 5)?,
            Some(ChunkType::Entropy),
            "{engine:?}: the in-flight run marked entropy it did not write"
        );
        storage.abandon_on_drop_for_test();
        drop(storage);

        let reopened = StorageModule::new(&indexed_entropy_info(&config), &config)?;
        assert_eq!(
            live_label(&reopened, 0),
            Some(ChunkType::Uninitialized),
            "{engine:?}: a crash during the run published Data"
        );
        assert_eq!(
            live_label(&reopened, 5),
            Some(ChunkType::Entropy),
            "{engine:?}: a crash during the run relabelled entropy"
        );
        assert_indexed_entropy(&reopened, data_root)?;
        drop(reopened);
        drop(tmp);
        Ok(())
    }

    /// A settled Rocks module with one checksum hole scans from that hole.
    /// A later write before the hole does not walk the prefix. Filling the
    /// hole still sets `crc_complete`. An empty durable point does not scan.
    #[test]
    fn checksum_gap_scan_resumes_at_the_first_hole() -> eyre::Result<()> {
        let (_tmp, _config, module) =
            open_presence_module("crc_gap_resume", SubmoduleIndexEngine::Rocksdb)?;
        for offset in 0..8_u32 {
            assert!(module.write_chunk(
                PartitionChunkOffset::from(offset),
                vec![0x21_u8; 32],
                ChunkType::Entropy,
            ));
            module.sync_pending_chunks()?;
        }
        assert!(
            module.chunk_crc_complete_for_test(),
            "the fill scan set crc_complete"
        );
        module.set_offset_crc_for_test(PartitionChunkOffset::from(5), None)?;
        module.clear_chunk_crc_complete()?;
        assert!(module.write_chunk(
            PartitionChunkOffset::from(6),
            vec![0x31_u8; 32],
            ChunkType::Entropy,
        ));
        module.sync_pending_chunks()?;
        assert!(
            !module.chunk_crc_complete_for_test(),
            "offset 5 still has no checksum"
        );

        let before_resume = module.crc_span_scan_count();
        assert!(module.write_chunk(
            PartitionChunkOffset::from(7),
            vec![0x32_u8; 32],
            ChunkType::Entropy,
        ));
        module.sync_pending_chunks()?;
        assert_eq!(
            module.crc_span_scan_count(),
            before_resume + 1,
            "the open gap must be scanned once"
        );
        assert_eq!(
            module.crc_scan_start_for_test(),
            Some(5),
            "the scan restarted at the submodule start"
        );

        let before_early = module.crc_span_scan_count();
        assert!(module.write_chunk(
            PartitionChunkOffset::from(2),
            vec![0x33_u8; 32],
            ChunkType::Entropy,
        ));
        module.sync_pending_chunks()?;
        assert_eq!(
            module.crc_span_scan_count(),
            before_early,
            "a write before the gap must not scan"
        );

        assert!(module.write_chunk(
            PartitionChunkOffset::from(5),
            vec![0x34_u8; 32],
            ChunkType::Entropy,
        ));
        module.sync_pending_chunks()?;
        assert!(
            module.chunk_crc_complete_for_test(),
            "filling the only gap sets crc_complete"
        );
        let after_fill = module.crc_span_scan_count();
        module.force_sync_pending_chunks()?;
        assert_eq!(
            module.crc_span_scan_count(),
            after_fill,
            "an empty durable point must not scan"
        );
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

    /// A failed authoritative replace must not install the ahead mask. The
    /// next write-ahead still stores `Interrupted` before the chunk fsync.
    /// Period 0 does not consult that mask. A crash after the write-ahead
    /// reloads `Uninitialized`.
    #[rstest]
    #[case::period_0(0_u64)]
    #[case::period_30(30_u64)]
    fn failed_authoritative_interval_write_does_not_skip_write_ahead(
        #[case] persist_secs: u64,
    ) -> eyre::Result<()> {
        let prefix = format!("interval_mask_fail_{persist_secs}");
        let (tmp, config, storage) = interval_batch_module(&prefix, persist_secs)?;
        let offset = PartitionChunkOffset::from(0);
        assert_eq!(
            file_intervals(&storage)?.get_at_point(offset).copied(),
            Some(ChunkType::Uninitialized),
            "a new module starts Uninitialized"
        );
        assert!(storage.write_chunk(offset, vec![7_u8; 32], ChunkType::Data));
        storage.fail_next_sync_at(SyncFailurePoint::BeforeIntervalImageCommit);
        let failed = storage
            .write_intervals_to_submodules()
            .expect_err("the interval replace was injected to fail");
        assert!(
            format!("{failed:#}").contains("BeforeIntervalImageCommit"),
            "{failed:#}"
        );
        assert_eq!(
            file_intervals(&storage)?.get_at_point(offset).copied(),
            Some(ChunkType::Uninitialized),
            "a failed replace published the new image"
        );

        storage.fail_next_sync_at(SyncFailurePoint::BeforeDataFsync);
        let fsync_failed = storage
            .sync_pending_chunks()
            .expect_err("chunk fsync was injected to fail");
        assert!(
            format!("{fsync_failed:#}").contains("BeforeDataFsync"),
            "{fsync_failed:#}"
        );
        assert_eq!(
            file_intervals(&storage)?.get_at_point(offset).copied(),
            Some(ChunkType::Interrupted),
            "write-ahead trusted a mask the failed replace never stored"
        );

        storage.abandon_on_drop_for_test();
        drop(storage);
        let reopened = StorageModule::new(&one_span(10, "sub", None), &config)?;
        assert_eq!(
            reopened.get_chunk_type(&offset),
            Some(ChunkType::Uninitialized),
            "open kept a durable Data label"
        );
        assert_ne!(
            file_intervals(&reopened)?.get_at_point(offset).copied(),
            Some(ChunkType::Data),
            "reopen file is Data"
        );
        assert!(!reopened.is_data_chunk_durable_at(offset));
        drop(reopened);
        drop(tmp);
        Ok(())
    }

    /// A durable point paused after pwrite must not publish `Data` after
    /// rollback cleared the index. Rollback waits out that point, then
    /// publishes `Uninitialized`. Period 0 and period 30 both reload that
    /// label, and open drops the placement.
    #[rstest]
    #[case::period_0(0_u64)]
    #[case::period_30(30_u64)]
    fn inflight_durable_point_cannot_publish_data_after_rollback(
        #[case] persist_secs: u64,
    ) -> eyre::Result<()> {
        let prefix = format!("rollback_inflight_{persist_secs}");
        let (tmp, config, storage) = crc_hold_module(&prefix, persist_secs)?;
        let storage = Arc::new(storage);
        let offset = PartitionChunkOffset::from(0);
        assert!(storage.write_chunk(offset, vec![0xAB; 32], ChunkType::Data));
        storage.pause_next_sync_at(SyncPausePoint::AfterPwrite);
        let sync_mod = Arc::clone(&storage);
        let (sync_tx, sync_rx) = std::sync::mpsc::channel();
        let sync_worker = std::thread::spawn(move || {
            let _ = sync_tx.send(sync_mod.sync_pending_chunks());
        });
        assert!(
            storage.wait_for_sync_pause(Duration::from_secs(5)),
            "the durable point did not reach the label update"
        );

        storage.sync_lock_blocked.store(false, Ordering::SeqCst);
        storage.sync_lock_probe.store(true, Ordering::SeqCst);
        let rollback_mod = Arc::clone(&storage);
        let (roll_tx, roll_rx) = std::sync::mpsc::channel();
        let rollback_worker = std::thread::spawn(move || {
            let _ = roll_tx.send(rollback_mod.rollback_unassigned_range(offset, offset, &[]));
        });
        // A timeout is a failure. Not finishing inside the window is not
        // proof that rollback reached the mutex.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_block = false;
        while Instant::now() < deadline {
            if storage.sync_lock_blocked.load(Ordering::SeqCst) {
                saw_block = true;
                break;
            }
            match roll_rx.try_recv() {
                Ok(finished) => {
                    panic!("rollback finished before it blocked on the sync lock: {finished:?}")
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    panic!("rollback thread dropped the result");
                }
            }
        }
        assert!(saw_block, "rollback did not block on the sync lock");
        assert!(
            storage.sync_in_progress.try_lock().is_err(),
            "the sync lock was free when rollback blocked"
        );
        storage.sync_lock_probe.store(false, Ordering::SeqCst);
        storage.release_sync_pause();
        roll_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|err| eyre!("rollback did not finish: {err}"))??;
        sync_rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|err| eyre!("durable point did not finish: {err}"))??;

        assert_eq!(
            live_label(&storage, 0),
            Some(ChunkType::Uninitialized),
            "the durable point restored Data after the index clear"
        );
        assert_eq!(
            file_label(&storage, 0)?,
            Some(ChunkType::Uninitialized),
            "intervals.json published Data after rollback"
        );
        assert!(
            !storage.is_data_chunk_durable_at(offset),
            "rollback left the offset durable"
        );
        assert!(!storage.is_data_root_index_ready_at(offset));

        // A worker sends its result before its closure drops its clone, so join
        // both before taking the module back.
        sync_worker
            .join()
            .map_err(|_| eyre!("durable point thread panicked"))?;
        rollback_worker
            .join()
            .map_err(|_| eyre!("rollback thread panicked"))?;
        let storage =
            Arc::try_unwrap(storage).map_err(|_| eyre!("a worker still holds the module"))?;
        storage.abandon_on_drop_for_test();
        drop(storage);
        let reopened = reopen_crc_module(tmp.path(), &config)?;
        assert_eq!(
            reopened.get_chunk_type(&offset),
            Some(ChunkType::Uninitialized),
            "reopen reloaded Data"
        );
        assert_eq!(
            file_label(&reopened, 0)?,
            Some(ChunkType::Uninitialized),
            "reopen file is not Uninitialized"
        );
        assert_not_served_as_data(&reopened, offset)?;
        assert!(!reopened.is_data_root_index_ready_at(offset));
        drop(reopened);
        drop(tmp);
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

        let config = testing_config_tuned(&base_path, chunk_size, 10_000, |node| {
            node.storage = StorageSyncConfig {
                num_writes_before_sync: 1000,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 0,
                min_durable_interval_ms: 0,
                ..StorageSyncConfig::default()
            };
        });

        let infos = [StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment::default()),
            submodules: vec![(
                partition_chunk_offset_ii!(0, config.consensus.num_chunks_in_partition - 1),
                "hdd0".into(),
            )],
        }];

        let storage_module_info = &infos[0];
        let storage_module = StorageModule::new(storage_module_info, &config)?;

        storage_module.pack_with_zeros();

        // log2(100 txs/block) is 7 segments of 32 bytes, plus one 64-byte leaf.
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
        let config = testing_config_tuned(tmp_dir.path(), 32, 4, |node| {
            node.storage = StorageSyncConfig {
                num_writes_before_sync: 1,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 0,
                min_durable_interval_ms: 0,
                ..StorageSyncConfig::default()
            };
        });
        let storage_module = StorageModule::new(
            &one_span(3, "chunks", Some(PartitionAssignment::default())),
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
        let config = testing_config_tuned(tmp_dir.path(), chunk_size, 4, |node| {
            node.storage = StorageSyncConfig {
                num_writes_before_sync: 1,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 0,
                intervals_persist_secs: 0,
                min_durable_interval_ms: 60_000,
                ..StorageSyncConfig::default()
            };
            node.database = index_database(SubmoduleIndexEngine::Rocksdb);
        });
        let storage = StorageModule::new(
            &one_span(3, "chunks", Some(PartitionAssignment::default())),
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

    /// Park one data chunk inside the durable interval, on RocksDB.
    /// Offset 0 is presence-committed. `parked` is pwritten, labeled `Data`,
    /// and still in `written_ahead`, with no offset row.
    fn park_one_rocks_chunk(
        prefix: &str,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        StorageModule,
        PartitionChunkOffset,
    )> {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let chunk_size = 32_u64;
        let config = testing_config_tuned(tmp_dir.path(), chunk_size, 4, |node| {
            node.storage = StorageSyncConfig {
                num_writes_before_sync: 1,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 0,
                intervals_persist_secs: 0,
                min_durable_interval_ms: 60_000,
                ..StorageSyncConfig::default()
            };
            node.database = index_database(SubmoduleIndexEngine::Rocksdb);
        });
        let storage = StorageModule::new(
            &one_span(3, "chunks", Some(PartitionAssignment::default())),
            &config,
        )?;
        assert!(storage.write_chunk(
            PartitionChunkOffset::from(0),
            vec![1_u8; chunk_size as usize],
            ChunkType::Data,
        ));
        storage.sync_pending_chunks()?;
        let parked = PartitionChunkOffset::from(1);
        assert!(storage.write_chunk(parked, vec![0xA1_u8; chunk_size as usize], ChunkType::Data));
        storage.sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(PartitionChunkOffset::from(0)));
        assert!(!storage.is_data_chunk_durable_at(parked));
        assert!(
            storage
                .pending_writes
                .read()
                .unwrap()
                .written_ahead
                .contains_key(&parked)
        );
        assert!(storage.offset_meta_for_test(parked)?.is_none());
        assert_eq!(live_label(&storage, parked.0), Some(ChunkType::Data));
        Ok((tmp_dir, storage, parked))
    }

    /// The reset marker write fails after the chunk is parked. The parked
    /// fence must still be there when `reset` returns, and a `Data` label
    /// with no presence row must not look durable. No marker means a later
    /// settle keeps the bytes. The interval file stays `Interrupted`.
    #[test]
    fn failed_reset_marker_keeps_the_parked_chunk_fence() -> eyre::Result<()> {
        let (_tmp, storage, parked) = park_one_rocks_chunk("reset_marker_fence")?;
        let (_, submodule) = storage.get_submodule_for_offset(parked)?;
        storage.fail_next_sync_at(SyncFailurePoint::ResetMarkerWrite);
        let err = storage.reset().unwrap_err();
        assert!(err.to_string().contains("ResetMarkerWrite"), "{err}");
        assert!(storage.reset_incomplete());
        assert!(!submodule.db.reset_in_progress()?);
        assert!(
            storage.sync_in_progress.try_lock().is_ok(),
            "reset released the sync lock"
        );
        assert!(
            storage
                .pending_writes
                .read()
                .unwrap()
                .written_ahead
                .contains_key(&parked),
            "the parked fence must survive a failed marker write"
        );
        assert!(storage.has_pending_writes());
        assert!(
            !storage.is_data_chunk_durable_at(parked),
            "a parked Data label with no presence row is not durable"
        );
        assert!(!storage.is_data_chunk_persisted_at(parked));
        assert!(storage.offset_meta_for_test(parked)?.is_none());
        assert_eq!(live_label(&storage, parked.0), Some(ChunkType::Data));
        assert!(
            storage.is_data_chunk_durable_at(PartitionChunkOffset::from(0)),
            "a chunk that already has a presence row stays durable"
        );

        storage.write_intervals_to_submodules()?;
        assert_eq!(
            file_label(&storage, parked.0)?,
            Some(ChunkType::Interrupted),
            "shutdown must not publish Data for a chunk with no presence row"
        );
        assert!(!storage.is_data_chunk_persisted_at(parked));

        assert!(
            storage.clear_reset_incomplete_without_marker()?,
            "no marker: settle keeps the bytes"
        );
        assert!(!storage.reset_incomplete());
        assert!(
            storage
                .pending_writes
                .read()
                .unwrap()
                .written_ahead
                .contains_key(&parked)
        );
        assert!(!storage.is_data_chunk_durable_at(parked));

        // The predicate does not trust the fence alone.
        storage
            .pending_writes
            .write()
            .unwrap()
            .written_ahead
            .clear();
        assert!(
            !storage.is_data_chunk_durable_at(parked),
            "a Data label that never received a presence commit is not durable"
        );
        Ok(())
    }

    /// The second submodule's marker write fails. The first marker is
    /// durable, so settle must not clear the flag, and the parked bytes
    /// stay fenced.
    #[test]
    fn failed_reset_marker_on_later_submodule_keeps_the_fence() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("reset_marker_second")
            .with_tracing()
            .build();
        let chunk_size = 32_u64;
        let config = testing_config_tuned(tmp_dir.path(), chunk_size, 4, |node| {
            node.storage = StorageSyncConfig {
                num_writes_before_sync: 1,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 0,
                intervals_persist_secs: 0,
                min_durable_interval_ms: 60_000,
                ..StorageSyncConfig::default()
            };
            node.database = index_database(SubmoduleIndexEngine::Rocksdb);
        });
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment::default()),
            submodules: vec![
                (partition_chunk_offset_ii!(0, 1), "a".into()),
                (partition_chunk_offset_ii!(2, 3), "b".into()),
            ],
        };
        let storage = StorageModule::new(&info, &config)?;
        assert!(storage.write_chunk(
            PartitionChunkOffset::from(0),
            vec![1_u8; chunk_size as usize],
            ChunkType::Data,
        ));
        storage.sync_pending_chunks()?;
        let parked = PartitionChunkOffset::from(2);
        assert!(storage.write_chunk(parked, vec![0xB2_u8; chunk_size as usize], ChunkType::Data));
        storage.sync_pending_chunks()?;
        let (_, first) = storage.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        let (_, second) = storage.get_submodule_for_offset(parked)?;
        storage.fail_sync_after(SyncFailurePoint::ResetMarkerWrite, 1);
        let err = storage.reset().unwrap_err();
        assert!(err.to_string().contains("injected"), "{err}");
        assert!(first.db.reset_in_progress()?);
        assert!(!second.db.reset_in_progress()?);
        assert!(
            storage
                .pending_writes
                .read()
                .unwrap()
                .written_ahead
                .contains_key(&parked)
        );
        assert!(!storage.is_data_chunk_durable_at(parked));
        assert!(
            !storage.clear_reset_incomplete_without_marker()?,
            "a durable marker on any submodule must not be settled as keep-bytes"
        );
        assert!(storage.reset_incomplete());

        storage.reset()?;
        assert!(!storage.reset_incomplete());
        assert!(!storage.has_pending_writes());
        assert!(!first.db.reset_in_progress()?);
        assert!(!second.db.reset_in_progress()?);
        assert_eq!(
            storage.get_intervals(ChunkType::Uninitialized),
            vec![partition_chunk_offset_ii!(0, 3)]
        );
        assert!(storage.write_chunk(parked, vec![0xC3_u8; chunk_size as usize], ChunkType::Data));
        storage.force_sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(parked));
        assert!(storage.offset_meta_for_test(parked)?.is_some());
        Ok(())
    }

    /// A reset that finishes drops the parked fence, wipes the span, and a
    /// later write can commit again.
    #[test]
    fn reset_marker_success_wipes_a_parked_chunk_and_a_new_write_commits() -> eyre::Result<()> {
        let (_tmp, storage, parked) = park_one_rocks_chunk("reset_parked_ok")?;
        storage.reset()?;
        assert!(!storage.reset_incomplete());
        assert!(!storage.has_pending_writes());
        let (_, submodule) = storage.get_submodule_for_offset(parked)?;
        assert!(!submodule.db.reset_in_progress()?);
        assert_eq!(
            storage.get_intervals(ChunkType::Uninitialized),
            vec![partition_chunk_offset_ii!(0, 3)]
        );
        assert!(storage.write_chunk(parked, vec![0xC3_u8; 32], ChunkType::Data));
        storage.force_sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(parked));
        assert!(storage.offset_meta_for_test(parked)?.is_some());
        Ok(())
    }

    /// The reset error before any marker write keeps the parked bytes.
    /// Settle sees no marker. A later sync commits the presence row.
    #[test]
    fn reset_marker_not_yet_written_keeps_the_parked_bytes() -> eyre::Result<()> {
        let (_tmp, storage, parked) = park_one_rocks_chunk("reset_before_marker")?;
        storage.fail_next_reset_for_test();
        let err = storage.reset().unwrap_err();
        assert!(
            err.to_string()
                .contains("injected storage module reset failure"),
            "{err}"
        );
        assert!(storage.reset_incomplete());
        let (_, submodule) = storage.get_submodule_for_offset(parked)?;
        assert!(!submodule.db.reset_in_progress()?);
        assert!(
            storage
                .pending_writes
                .read()
                .unwrap()
                .written_ahead
                .contains_key(&parked)
        );
        assert!(storage.clear_reset_incomplete_without_marker()?);
        assert!(!storage.reset_incomplete());
        storage.force_sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(parked));
        assert!(storage.offset_meta_for_test(parked)?.is_some());
        assert!(!storage.has_pending_writes());
        Ok(())
    }

    /// After the marker write fails and settle keeps the bytes, a later sync
    /// commits the parked chunk.
    #[test]
    fn parked_chunk_commits_after_reset_marker_failure() -> eyre::Result<()> {
        let (_tmp, storage, parked) = park_one_rocks_chunk("reset_marker_then_sync")?;
        storage.fail_next_sync_at(SyncFailurePoint::ResetMarkerWrite);
        let err = storage.reset().unwrap_err();
        assert!(err.to_string().contains("ResetMarkerWrite"), "{err}");
        assert!(storage.clear_reset_incomplete_without_marker()?);
        storage.force_sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(parked));
        assert!(storage.offset_meta_for_test(parked)?.is_some());
        assert!(!storage.has_pending_writes());
        Ok(())
    }

    /// RocksDB module, every offset a packed data chunk, `crc_complete` set.
    fn settled_rocks_module(
        prefix: &str,
        split: bool,
    ) -> eyre::Result<(
        irys_testing_utils::tempfile::TempDir,
        Config,
        StorageModuleInfo,
        StorageModule,
    )> {
        let tmp = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let config = testing_config_tuned(tmp.path(), 32, 4, |node| {
            if let irys_types::ConsensusOptions::Custom(consensus) = &mut node.consensus {
                consensus.entropy_packing_iterations = 1;
            }
            node.storage = StorageSyncConfig {
                num_writes_before_sync: 1,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 0,
                intervals_persist_secs: 0,
                min_durable_interval_ms: 0,
                ..StorageSyncConfig::default()
            };
            node.database = index_database(SubmoduleIndexEngine::Rocksdb);
        });
        let assignment = PartitionAssignment {
            ledger_id: Some(DataLedger::Submit.into()),
            slot_index: Some(0),
            miner_address: config.node_config.miner_address(),
            partition_hash: H256::repeat_byte(0x41),
        };
        let info = StorageModuleInfo {
            id: 0,
            partition_assignment: Some(assignment),
            submodules: if split {
                vec![
                    (partition_chunk_offset_ii!(0, 1), tmp.path().join("a")),
                    (partition_chunk_offset_ii!(2, 3), tmp.path().join("b")),
                ]
            } else {
                vec![(partition_chunk_offset_ii!(0, 3), tmp.path().join("chunks"))]
            },
        };
        let storage = StorageModule::new(&info, &config)?;
        for offset in 0..4_u32 {
            assert!(storage.write_chunk(
                PartitionChunkOffset::from(offset),
                vec![0x21; 32],
                ChunkType::Entropy,
            ));
            storage.sync_pending_chunks()?;
        }
        let signer = IrysSigner::random_signer(&storage.config.consensus);
        let tx =
            signer.sign_transaction(signer.create_transaction(vec![0x5A; 32], H256::zero())?)?;
        let unpacked = tx.data_chunks()?;
        let (_, proofs) = DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&tx.header));
        storage.index_transaction_data(
            &tx.header,
            &proofs[0].proof,
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            0,
        )?;
        storage
            .write_data_chunk(&unpacked[0])
            .expect("pack the indexed chunk");
        storage.force_sync_pending_chunks()?;
        assert!(
            storage.chunk_crc_complete_for_test(),
            "every settled offset has a checksum"
        );
        assert!(storage.crc_span_scan_count() >= 1);
        Ok((tmp, config, info, storage))
    }

    /// The marker write fails before it commits. The bytes are unchanged and
    /// there is no marker, so `crc_complete` must stay set. An empty sync and
    /// a later open must not walk the span. A missing checksum stays omitted.
    /// A reset that then finishes still clears the flag and does not scan.
    #[test]
    fn failed_reset_marker_keeps_crc_complete_on_an_intact_module() -> eyre::Result<()> {
        let (tmp, config, info, storage) = settled_rocks_module("crc_marker_fail", false)?;
        let scans = storage.crc_span_scan_count();
        storage.fail_next_sync_at(SyncFailurePoint::ResetMarkerWrite);
        let err = storage.reset().unwrap_err();
        assert!(err.to_string().contains("ResetMarkerWrite"), "{err}");
        let (_, submodule) = storage.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        assert!(!submodule.db.reset_in_progress()?);
        assert_eq!(
            storage.get_chunk_type(&PartitionChunkOffset::from(1)),
            Some(ChunkType::Entropy),
            "a failed marker leaves the labels"
        );
        let row = storage
            .offset_meta_for_test(PartitionChunkOffset::from(0))?
            .expect("the data row stays");
        assert!(row.crc.is_some());
        assert!(row.proof.is_some());
        assert!(
            storage.chunk_crc_complete_for_test(),
            "an intact module stays strict when the marker did not commit"
        );
        storage.force_sync_pending_chunks()?;
        assert_eq!(
            storage.crc_span_scan_count(),
            scans,
            "an empty sync must not walk OffsetMeta"
        );
        assert!(storage.chunk_crc_complete_for_test());
        assert!(
            storage.clear_reset_incomplete_without_marker()?,
            "no marker: settle keeps the bytes"
        );
        assert!(storage.chunk_crc_complete_for_test());
        storage.abandon_on_drop_for_test();
        drop(storage);

        let reopened = StorageModule::new(&info, &config)?;
        assert!(reopened.chunk_crc_complete_for_test());
        assert_eq!(reopened.crc_span_scan_count(), 0);
        assert_eq!(
            reopened.get_chunk_type(&PartitionChunkOffset::from(1)),
            Some(ChunkType::Entropy)
        );
        assert!(
            reopened
                .offset_meta_for_test(PartitionChunkOffset::from(0))?
                .and_then(|row| row.crc)
                .is_some()
        );
        reopened.force_sync_pending_chunks()?;
        assert_eq!(
            reopened.crc_span_scan_count(),
            0,
            "open and an empty sync must not walk OffsetMeta"
        );
        reopened.set_offset_crc_for_test(PartitionChunkOffset::from(0), None)?;
        assert!(
            reopened
                .generate_full_chunks(PartitionChunkOffset::from(0), PartitionChunkOffset::from(0))?
                .is_empty(),
            "a missing checksum on a strict module is omitted"
        );

        let scans = reopened.crc_span_scan_count();
        reopened.reset()?;
        assert!(!reopened.chunk_crc_complete_for_test());
        assert_eq!(
            reopened.crc_span_scan_count(),
            scans,
            "a finished reset clears the flag and does not scan"
        );
        assert_eq!(
            reopened.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Uninitialized)
        );
        let (_, submodule) = reopened.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        assert!(!submodule.db.reset_in_progress()?);
        drop(tmp);
        Ok(())
    }

    /// The first marker commits and the second write fails. The bytes are
    /// intact, so the flag stays set, and settle must not treat the module
    /// as a wipe that never started.
    #[test]
    fn failed_later_reset_marker_keeps_crc_complete() -> eyre::Result<()> {
        let (_tmp, _config, _info, storage) = settled_rocks_module("crc_marker_second", true)?;
        storage.fail_sync_after(SyncFailurePoint::ResetMarkerWrite, 1);
        let err = storage.reset().unwrap_err();
        assert!(err.to_string().contains("ResetMarkerWrite"), "{err}");
        let (_, first) = storage.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        let (_, second) = storage.get_submodule_for_offset(PartitionChunkOffset::from(2))?;
        assert!(first.db.reset_in_progress()?);
        assert!(!second.db.reset_in_progress()?);
        assert!(
            storage.chunk_crc_complete_for_test(),
            "a partial marker leaves the bytes, so the flag stays set"
        );
        assert!(
            !storage.clear_reset_incomplete_without_marker()?,
            "a durable marker on any submodule must not be settled as keep-bytes"
        );
        assert_eq!(
            storage.get_chunk_type(&PartitionChunkOffset::from(2)),
            Some(ChunkType::Entropy)
        );
        Ok(())
    }

    /// The marker is durable and the index clear has not run. The flag stays
    /// set while the bytes are still there. Open finishes the wipe and does
    /// not scan.
    #[test]
    fn reset_crash_after_marker_keeps_crc_until_reopen_wipes() -> eyre::Result<()> {
        let (_tmp, config, info, storage) = settled_rocks_module("crc_after_marker", false)?;
        let scans = storage.crc_span_scan_count();
        storage.fail_next_sync_at(SyncFailurePoint::AfterResetMarker);
        let err = storage.reset().unwrap_err();
        assert!(err.to_string().contains("AfterResetMarker"), "{err}");
        let (_, submodule) = storage.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        assert!(submodule.db.reset_in_progress()?);
        assert!(
            storage
                .offset_meta_for_test(PartitionChunkOffset::from(0))?
                .is_some(),
            "the index clear has not run"
        );
        assert!(
            storage.chunk_crc_complete_for_test(),
            "the flag stays set until the wipe passes the durable marker"
        );
        assert_eq!(storage.crc_span_scan_count(), scans);
        storage.abandon_on_drop_for_test();
        drop(storage);

        let reopened = StorageModule::new(&info, &config)?;
        assert_eq!(reopened.crc_span_scan_count(), 0);
        assert!(!reopened.chunk_crc_complete_for_test());
        assert_eq!(
            reopened.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Uninitialized)
        );
        let (_, submodule) = reopened.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        assert!(!submodule.db.reset_in_progress()?);
        reopened.force_sync_pending_chunks()?;
        assert_eq!(
            reopened.crc_span_scan_count(),
            0,
            "finishing the crashed reset must not scan"
        );
        Ok(())
    }

    /// The wipe committed and the assignment file could not be parsed.
    /// The marker stays, memory keeps the old ledger, and a later store
    /// still installs the new assignment and drops the marker.
    #[test]
    fn failed_assignment_store_keeps_the_reset_marker() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new()
            .prefix("reset_store_fail")
            .with_tracing()
            .build();
        let config = testing_config(tmp.path(), 32, 4);
        let hash = H256::repeat_byte(0x31);
        let mut current = submit_assignment(0);
        current.partition_hash = hash;
        current.miner_address = config.node_config.miner_address();
        current.ledger_id = Some(DataLedger::Publish.into());
        // Absolute. `store_assignment` writes `packing_params.toml` at
        // `submodule.path`, and a relative name would miss this temp dir.
        let sub = tmp.path().join("chunks");
        let storage = StorageModule::new(&one_span(3, sub.clone(), Some(current)), &config)?;
        let offset = PartitionChunkOffset::from(0);
        assert!(storage.write_chunk(offset, vec![0x11; 32], ChunkType::Data));
        storage.force_sync_pending_chunks()?;
        let params_path = sub.join(PACKING_PARAMS_FILE_NAME);
        let saved = fs::read(&params_path)?;
        fs::write(&params_path, b"not-toml")?;

        let next = PartitionAssignment {
            partition_hash: hash,
            miner_address: current.miner_address,
            ledger_id: None,
            slot_index: None,
        };
        let err = storage
            .reset_with_assignment(Some(next), 4)
            .expect_err("a bad packing-params file fails the store");
        assert!(err.to_string().contains("packing params"), "{err}");
        assert!(
            storage.reset_incomplete(),
            "a failed store must leave the wipe unfinished"
        );
        let (_, submodule) = storage.get_submodule_for_offset(offset)?;
        assert!(
            submodule.db.reset_in_progress()?,
            "a failed store must keep the reset marker"
        );
        assert_eq!(
            storage.partition_assignment().unwrap().ledger_id,
            Some(DataLedger::Publish.into()),
            "memory keeps the assignment the store did not replace"
        );
        assert_eq!(
            storage.get_chunk_type(&offset),
            Some(ChunkType::Uninitialized),
            "the wipe already committed"
        );
        assert_eq!(fs::read(&params_path)?, b"not-toml");

        fs::write(&params_path, saved)?;
        storage.reset_with_assignment(Some(next), 5)?;
        assert!(!storage.reset_incomplete());
        let (_, submodule) = storage.get_submodule_for_offset(offset)?;
        assert!(!submodule.db.reset_in_progress()?);
        assert_eq!(storage.partition_assignment().unwrap().ledger_id, None);
        assert_eq!(storage.partition_assignment().unwrap().slot_index, None);
        let stored = PackingParams::from_toml(&params_path)?;
        assert_eq!(stored.ledger, None);
        assert_eq!(stored.slot, None);
        assert_eq!(stored.last_updated_height, Some(5));
        Ok(())
    }

    /// Expiry reads the file, wipes, then stores capacity. A store error
    /// after that wipe keeps the marker. The retry still moves to capacity.
    #[test]
    fn failed_expiry_store_keeps_the_reset_marker() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new()
            .prefix("expiry_store_fail")
            .with_tracing()
            .build();
        let config = testing_config(tmp.path(), 32, 4);
        let hash = H256::repeat_byte(0x32);
        let mut current = submit_assignment(0);
        current.partition_hash = hash;
        current.miner_address = config.node_config.miner_address();
        current.ledger_id = Some(DataLedger::Publish.into());
        let sub = tmp.path().join("chunks");
        let storage = StorageModule::new(&one_span(3, sub, Some(current)), &config)?;
        let offset = PartitionChunkOffset::from(0);
        assert!(storage.write_chunk(offset, vec![0x22; 32], ChunkType::Data));
        storage.force_sync_pending_chunks()?;
        storage.fail_next_sync_at(SyncFailurePoint::AssignmentStore);

        let err = storage
            .reset_expired_to_capacity(hash, 9)
            .expect_err("the capacity store is the injected failure");
        assert!(err.to_string().contains("AssignmentStore"), "{err}");
        assert!(storage.reset_incomplete());
        let (_, submodule) = storage.get_submodule_for_offset(offset)?;
        assert!(submodule.db.reset_in_progress()?);
        let assignment = storage.partition_assignment().unwrap();
        assert_eq!(assignment.partition_hash, hash);
        assert_eq!(assignment.ledger_id, Some(DataLedger::Publish.into()));
        assert_eq!(
            storage.get_chunk_type(&offset),
            Some(ChunkType::Uninitialized)
        );

        let finished = storage
            .reset_expired_to_capacity(hash, 9)?
            .expect("the same expiry still resets once the store can finish");
        assert_eq!(finished, partition_chunk_offset_ii!(0, 3));
        assert!(!storage.reset_incomplete());
        let (_, submodule) = storage.get_submodule_for_offset(offset)?;
        assert!(!submodule.db.reset_in_progress()?);
        assert_eq!(storage.partition_assignment().unwrap().ledger_id, None);
        assert_eq!(storage.partition_assignment().unwrap().slot_index, None);
        Ok(())
    }

    fn recall_flush_fixture(
        prefix: &str,
    ) -> eyre::Result<(irys_testing_utils::tempfile::TempDir, StorageModule)> {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let chunk_size = 32;
        let config = testing_config_tuned(tmp_dir.path(), chunk_size, 8, |node| {
            node.storage = StorageSyncConfig {
                num_writes_before_sync: 1000,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 60_000,
                entropy_sweep_max_bytes: chunk_size,
                min_durable_interval_ms: 0,
                ..StorageSyncConfig::default()
            };
        });
        let storage_module = StorageModule::new(
            &one_span(7, "chunks", Some(PartitionAssignment::default())),
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
        // MDBX-SUBMODULE-INDEX: drop Mdbx from this list and keep the RocksDB run.
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

    /// `Ok(true)` when an explicit open of this module's `chunks.dat` is
    /// rejected with `EINVAL`. That is the filesystem, not a missed flag.
    fn direct_open_rejected(storage: &StorageModule) -> eyre::Result<bool> {
        let (_, submodule) = storage.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        let intervals = submodule
            .intervals_file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let chunks = intervals
            .parent()
            .ok_or_else(|| eyre!("intervals.json has no directory"))?
            .join("chunks.dat");
        match recall_direct::open_recall_direct(&chunks)? {
            None => {
                eprintln!(
                    "O_DIRECT probe of {} returned EINVAL; this filesystem does not support O_DIRECT",
                    chunks.display()
                );
                Ok(true)
            }
            Some(file) => {
                if !recall_direct::fd_is_direct(&file) {
                    eyre::bail!(
                        "O_DIRECT probe of {} opened a descriptor that is not O_DIRECT",
                        chunks.display()
                    );
                }
                Ok(false)
            }
        }
    }

    /// `Ok(true)` when this filesystem rejected `O_DIRECT`. The caller skips.
    ///
    /// A requested direct open that did not stick fails the test when the
    /// probe above succeeds. A skip is only the filesystem rejecting the flag.
    fn skip_without_direct(storage: &StorageModule, test: &str) -> eyre::Result<bool> {
        let (_, submodule) = storage.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        let direct = submodule
            .recall_direct
            .as_ref()
            .is_some_and(|file| recall_direct::fd_is_direct(file));
        if !direct {
            if direct_open_rejected(storage)? {
                eprintln!(
                    "skip {test}: this filesystem rejected O_DIRECT; mining recall stays on the buffered descriptor"
                );
                return Ok(true);
            }
            eyre::bail!(
                "{test}: recall_direct_io was requested and the filesystem accepts O_DIRECT, but the module descriptor is not direct"
            );
        }
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
    fn recall_io_mode_is_direct_when_the_filesystem_supports_it() -> eyre::Result<()> {
        for_each_recall_io(
            "recall_direct_mode",
            true,
            RECALL_DIRECT_CHUNK,
            0,
            |storage| {
                let mode = storage.recall_io_mode();
                if mode != recall_direct::MODE_DIRECT && direct_open_rejected(storage)? {
                    eprintln!(
                        "skip recall_io_mode_is_direct_when_the_filesystem_supports_it: filesystem rejected O_DIRECT"
                    );
                    return Ok(());
                }
                assert_eq!(mode, recall_direct::MODE_DIRECT);
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
        wait_until_drained(
            storage_module,
            Duration::from_secs(2),
            "packed chunks stayed pending",
        );
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
        wait_until_drained(
            &lane.module,
            Duration::from_secs(15),
            "idle disk left the short run queued",
        );
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
        wait_until_drained(
            &lane.module,
            Duration::from_secs(15),
            "idle disk left a short run queued",
        );
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
        wait_until_drained(
            &lane.module,
            Duration::from_secs(15),
            "full run stayed pending",
        );
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
        wait_until_drained(
            &lane.module,
            Duration::from_secs(15),
            "idle disk left the short run queued",
        );
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
        wait_until_drained(
            &lane.module,
            Duration::from_secs(15),
            "idle disk left the short run queued after recall",
        );
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
        // These tests read intervals.json as soon as sync returns.
        let config = testing_config_tuned(tmp_dir.path(), 32, 10, |node| {
            node.storage = StorageSyncConfig {
                num_writes_before_sync: writes_before_sync,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 60_000,
                entropy_sweep_max_bytes: 32,
                // These tests read intervals.json as soon as sync returns.
                intervals_persist_secs: 0,
                min_durable_interval_ms: 0,
                ..StorageSyncConfig::default()
            };
        });
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
        let config = testing_config_tuned(
            tmp_dir.path(),
            chunk_size,
            u64::from(last_offset) + 1,
            |node| {
                node.storage = StorageSyncConfig {
                    num_writes_before_sync: writes_before_sync,
                    max_pending_write_bytes: None,
                    entropy_sweep_interval_millis: 60_000,
                    entropy_sweep_max_bytes: chunk_size,
                    min_durable_interval_ms: 0,
                    ..StorageSyncConfig::default()
                };
            },
        );
        let storage = StorageModule::new(
            &one_span(last_offset, "chunks", Some(PartitionAssignment::default())),
            &config,
        )?;
        Ok((tmp_dir, storage))
    }

    fn coalesce_fixture(
        prefix: &str,
        chunk_size: u64,
    ) -> eyre::Result<(irys_testing_utils::tempfile::TempDir, StorageModule)> {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let config = testing_config_tuned(tmp_dir.path(), chunk_size, 200, |node| {
            node.storage = StorageSyncConfig {
                num_writes_before_sync: 1000,
                max_pending_write_bytes: None,
                entropy_sweep_interval_millis: 60_000,
                entropy_sweep_max_bytes: chunk_size,
                min_durable_interval_ms: 0,
                ..StorageSyncConfig::default()
            };
        });
        let storage = StorageModule::new(
            &one_span(199, "chunks", Some(PartitionAssignment::default())),
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
        let config = testing_config_tuned(tmp_dir.path(), chunk_size, 16, |node| {
            node.storage = StorageSyncConfig {
                num_writes_before_sync: 1,
                max_pending_write_bytes,
                entropy_sweep_interval_millis: 60_000,
                entropy_sweep_max_bytes: chunk_size,
                intervals_persist_secs: 0,
                min_durable_interval_ms,
                durable_headroom_permille,
                ..StorageSyncConfig::default()
            };
        });
        let storage = StorageModule::new(
            &one_span(15, "chunks", Some(PartitionAssignment::default())),
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
        let deferred = park_second_chunk(&storage)?;
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
        let deferred = park_second_chunk(&storage)?;
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
        let deferred = park_second_chunk(&storage)?;
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
    fn durable_point_decisions() {
        // An owed recall flush does not fdatasync only because the line broke.
        // A finished wait is the interval even when the latch is closed.
        // Pending bytes equal to the ceiling still defer. The bypass is strict `>`.
        //
        // pending, since_ms (-1 is the first point), interval_ms, force, recall,
        // open_in, ceiling, headroom, cause, open_out.
        // cause: 0 defer, 1 Forced, 2 Ceiling, 3 Headroom, 4 Interval.
        // Each row is a full query. The decision is not written back.
        let rows = [
            (400, 1, 250, false, false, true, 1_000, 400, 0, true),
            (300, 1, 250, false, false, true, 1_000, 400, 0, true),
            (0, 249, 250, false, false, true, 1_000, 400, 0, true),
            (401, 1, 250, false, false, true, 1_000, 400, 3, false),
            (401, 1, 250, false, true, true, 1_000, 400, 0, false),
            (201, 1, 250, false, false, false, 1_000, 400, 3, false),
            (200, 1, 250, false, false, false, 1_000, 400, 0, true),
            (0, 1, 250, false, false, false, 1_000, 400, 0, true),
            (0, 250, 250, false, false, true, 1_000, 400, 4, true),
            (401, 250, 250, false, false, false, 1_000, 400, 4, false),
            (0, 1, 0, false, false, true, 1_000, 400, 4, true),
            (5_000, -1, 250, false, false, true, 1_000, 400, 4, false),
            (0, 1, 250, true, false, true, 1_000, 400, 1, true),
            (1_001, 1, 250, false, true, true, 1_000, 400, 2, false),
            (1_000, 1, 250, false, false, true, 1_000, 1_000, 0, true),
        ];
        for (
            i,
            (pending, since, interval, force, recall, open_in, ceiling, headroom, cause, open_out),
        ) in rows.into_iter().enumerate()
        {
            let mut query = defer_query();
            query.pending_bytes = pending;
            query.since_last = (since >= 0)
                .then(|| Duration::from_millis(u64::try_from(since).expect("since_ms")));
            query.interval = Duration::from_millis(interval);
            query.force = force;
            query.recall_flush_owed = recall;
            query.headroom_open = open_in;
            query.ceiling_bytes = ceiling;
            query.headroom_bytes = headroom;
            let decision = durable_point_decision(query);
            let expected = match cause {
                0 => None,
                1 => Some(DurablePointCause::Forced),
                2 => Some(DurablePointCause::Ceiling),
                3 => Some(DurablePointCause::Headroom),
                4 => Some(DurablePointCause::Interval),
                _ => panic!("cause {cause}"),
            };
            assert_eq!(decision.cause, expected, "row {i}");
            assert!(
                decision.headroom_open == open_out,
                "row {i}: latch is {} want {open_out}",
                decision.headroom_open
            );
        }
    }

    fn submit_slot(config: &Config, hash: H256, slot: usize) -> PartitionAssignment {
        PartitionAssignment {
            ledger_id: Some(DataLedger::Submit.into()),
            slot_index: Some(slot),
            miner_address: config.node_config.miner_address(),
            partition_hash: hash,
        }
    }

    fn module_info(path: &Path, assignment: Option<PartitionAssignment>) -> StorageModuleInfo {
        one_span(3, path, assignment)
    }

    /// One entropy chunk and one tx row. `sub` is the submodule directory.
    fn seed_assigned_entropy(
        prefix: &str,
        engine: SubmoduleIndexEngine,
        hash: H256,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        Config,
        PathBuf,
    )> {
        let tmp = TempDirBuilder::new().prefix(prefix).build();
        let config = testing_config_tuned(tmp.path(), 32, 4, |node| {
            node.database = index_database(engine);
        });
        let sub = tmp.path().join("sub");
        let storage = StorageModule::new(
            &module_info(&sub, Some(submit_slot(&config, hash, 0))),
            &config,
        )?;
        let offset = PartitionChunkOffset::from(0);
        assert!(storage.write_chunk(offset, vec![0xE1_u8; 32], ChunkType::Entropy));
        storage.force_sync_pending_chunks()?;
        assert_eq!(storage.get_chunk_type(&offset), Some(ChunkType::Entropy));
        let root = H256::repeat_byte(0x44);
        let (_, submodule) = storage.get_submodule_for_offset(offset)?;
        submodule.db.update(|tx| {
            tx.add_tx_leaf_binding(
                root,
                &TxLeafBinding {
                    data_root: root,
                    prefix_hash: H256::zero(),
                },
            )?;
            tx.add_tx_path_hash_to_offset_range(offset, offset, Some(root))
        })?;
        let rows = submodule
            .db
            .view(|tx| tx.tx_intervals_in_range(offset, offset))?;
        eyre::ensure!(!rows.is_empty(), "tx row was not stored");
        if submodule.db.stores_offset_crc() {
            submodule.db.set_chunk_crc_complete(true)?;
        }
        drop(storage);
        Ok((tmp, config, sub))
    }

    fn interval_label(sub: &Path, offset: u32) -> eyre::Result<Option<ChunkType>> {
        let intervals = read_intervals_file(&sub.join("intervals.json"))?;
        Ok(intervals
            .get_at_point(PartitionChunkOffset::from(offset))
            .copied())
    }

    fn config_with_chain_id(config: &Config, chain_id: u64) -> Config {
        let mut node = config.node_config.clone();
        let irys_types::ConsensusOptions::Custom(consensus) = &mut node.consensus else {
            panic!("test config uses a custom consensus");
        };
        consensus.chain_id = chain_id;
        Config::new(node, config.peer_id())
    }

    fn live_tx_rows(storage: &StorageModule) -> eyre::Result<usize> {
        let offset = PartitionChunkOffset::from(0);
        let (_, submodule) = storage.get_submodule_for_offset(offset)?;
        let rows = submodule
            .db
            .view(|tx| tx.tx_intervals_in_range(offset, offset))?;
        Ok(rows.len())
    }

    /// A packing refusal on the unassign arm must not store the reset marker.
    /// The next open that still assigns the old hash keeps the entropy. A
    /// later unassign whose packing matches still wipes.
    #[test]
    fn refused_unassign_open_does_not_leave_a_reset_marker() -> eyre::Result<()> {
        // MDBX-SUBMODULE-INDEX: drop Mdbx from this list and keep the RocksDB run.
        for engine in [SubmoduleIndexEngine::Mdbx, SubmoduleIndexEngine::Rocksdb] {
            let hash = H256::repeat_byte(0x61);
            let prefix = format!("refuse_unassign_{engine:?}");
            let (_tmp, config, sub) = seed_assigned_entropy(&prefix, engine, hash)?;
            let params_path = sub.join(PACKING_PARAMS_FILE_NAME);
            let mut params = PackingParams::from_toml(&params_path)?;
            let recorded = params
                .entropy_packing_iterations
                .expect("open records iterations");
            params.entropy_packing_iterations = Some(recorded + 1);
            params.write_to_disk(&params_path)?;

            let err = StorageModule::new(&module_info(&sub, None), &config)
                .expect_err("iterations do not match this module");
            assert!(
                err.to_string().contains("packing identity"),
                "{engine:?}: {err}"
            );
            assert_eq!(
                PackingParams::from_toml(&params_path)?.partition_hash,
                Some(hash),
                "{engine:?}: refused unassign rewrote the packing file"
            );
            assert_eq!(
                interval_label(&sub, 0)?,
                Some(ChunkType::Entropy),
                "{engine:?}: refused unassign rewrote intervals"
            );
            {
                let index = open_submodule_index(&sub, &config.node_config.database)?;
                assert!(
                    !index.reset_in_progress()?,
                    "{engine:?}: refused unassign left a reset marker"
                );
                if index.stores_offset_crc() {
                    assert!(
                        index.chunk_crc_complete()?,
                        "{engine:?}: refused unassign cleared crc_complete"
                    );
                }
                let rows = index.view(|tx| {
                    tx.tx_intervals_in_range(
                        PartitionChunkOffset::from(0),
                        PartitionChunkOffset::from(0),
                    )
                })?;
                assert!(
                    !rows.is_empty(),
                    "{engine:?}: refused unassign cleared the tx row"
                );
            }

            params.entropy_packing_iterations = Some(recorded);
            params.write_to_disk(&params_path)?;
            let reopened = StorageModule::new(
                &module_info(&sub, Some(submit_slot(&config, hash, 0))),
                &config,
            )?;
            assert_eq!(
                reopened.get_chunk_type(&PartitionChunkOffset::from(0)),
                Some(ChunkType::Entropy),
                "{engine:?}: matching reopen wiped a module whose hash never changed"
            );
            {
                let (_, submodule) =
                    reopened.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
                if submodule.db.stores_offset_crc() {
                    assert!(
                        submodule.db.chunk_crc_complete()?,
                        "{engine:?}: matching reopen lost crc_complete"
                    );
                }
            }
            drop(reopened);

            let unassigned = StorageModule::new(&module_info(&sub, None), &config)?;
            assert_eq!(
                unassigned.get_chunk_type(&PartitionChunkOffset::from(0)),
                Some(ChunkType::Uninitialized),
                "{engine:?}: a matching unassign must still wipe"
            );
            assert!(
                !unassigned
                    .get_submodule_for_offset(PartitionChunkOffset::from(0))?
                    .1
                    .db
                    .reset_in_progress()?,
                "{engine:?}: a finished unassign left the marker"
            );
            assert_eq!(
                live_tx_rows(&unassigned)?,
                0,
                "{engine:?}: a finished unassign kept the tx row"
            );
        }
        Ok(())
    }

    /// The file has no chain id. A Rocks unassign must compare the stored
    /// packing record before it wipes. The same chain id still unassigns.
    #[test]
    fn rocks_unassign_with_a_different_chain_id_does_not_wipe() -> eyre::Result<()> {
        let hash = H256::repeat_byte(0x62);
        let (_tmp, config, sub) =
            seed_assigned_entropy("refuse_chain", SubmoduleIndexEngine::Rocksdb, hash)?;
        let other = config_with_chain_id(&config, config.consensus.chain_id + 1);
        let err = StorageModule::new(&module_info(&sub, None), &other)
            .expect_err("stored chain id does not match this open");
        let message = err.to_string();
        assert!(message.contains("packing identity"), "{message}");
        assert!(
            message.contains(&config.consensus.chain_id.to_string()),
            "{message}"
        );
        assert!(
            message.contains(&other.consensus.chain_id.to_string()),
            "{message}"
        );
        assert_eq!(
            PackingParams::from_toml(sub.join(PACKING_PARAMS_FILE_NAME))?.partition_hash,
            Some(hash)
        );
        assert_eq!(interval_label(&sub, 0)?, Some(ChunkType::Entropy));
        {
            let index = open_submodule_index(&sub, &config.node_config.database)?;
            assert!(!index.reset_in_progress()?);
            assert_eq!(
                index
                    .view(|tx| tx.tx_intervals_in_range(
                        PartitionChunkOffset::from(0),
                        PartitionChunkOffset::from(0),
                    ))?
                    .len(),
                1
            );
        }

        let reopened = StorageModule::new(
            &module_info(&sub, Some(submit_slot(&config, hash, 0))),
            &config,
        )?;
        assert_eq!(
            reopened.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Entropy),
            "a bind refusal must not wipe before it returns"
        );
        drop(reopened);

        let unassigned = StorageModule::new(&module_info(&sub, None), &config)?;
        assert_eq!(
            unassigned.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Uninitialized),
            "the same chain id still unassigns"
        );
        Ok(())
    }

    /// Arms a failure after the open reset marker and clears it on drop.
    struct OpenResetMarkerFail;

    impl OpenResetMarkerFail {
        fn arm() -> Self {
            FAIL_AFTER_OPEN_RESET_MARKER.with(|flag| flag.set(true));
            Self
        }
    }

    impl Drop for OpenResetMarkerFail {
        fn drop(&mut self) {
            FAIL_AFTER_OPEN_RESET_MARKER.with(|flag| flag.set(false));
        }
    }

    /// A crash after the unassign marker, and before `crc_complete` is
    /// cleared, must leave the flag set. The next open finishes the wipe.
    /// An unassign that is not interrupted still wipes and clears the flag.
    #[test]
    fn crash_after_open_reset_marker_keeps_crc_complete() -> eyre::Result<()> {
        let hash = H256::repeat_byte(0x63);
        let (_tmp, config, sub) =
            seed_assigned_entropy("open_marker_crc", SubmoduleIndexEngine::Rocksdb, hash)?;
        {
            let _fail = OpenResetMarkerFail::arm();
            let err = StorageModule::new(&module_info(&sub, None), &config)
                .expect_err("the open stops after the marker");
            assert!(
                err.to_string()
                    .contains("injected failure after the open reset marker"),
                "{err}"
            );
        }
        assert_eq!(
            PackingParams::from_toml(sub.join(PACKING_PARAMS_FILE_NAME))?.partition_hash,
            Some(hash),
            "the interrupted unassign published the packing file"
        );
        assert_eq!(interval_label(&sub, 0)?, Some(ChunkType::Entropy));
        {
            let index = open_submodule_index(&sub, &config.node_config.database)?;
            let marker = index.reset_in_progress()?;
            let crc = index.chunk_crc_complete()?;
            assert!(
                marker && crc,
                "a crash after the marker must keep crc_complete set; marker={marker} crc={crc}"
            );
            let rows = index.view(|tx| {
                tx.tx_intervals_in_range(
                    PartitionChunkOffset::from(0),
                    PartitionChunkOffset::from(0),
                )
            })?;
            assert!(
                !rows.is_empty(),
                "the interrupted unassign cleared the index"
            );
        }

        let recovered = StorageModule::new(
            &module_info(&sub, Some(submit_slot(&config, hash, 0))),
            &config,
        )?;
        assert_eq!(
            recovered.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Uninitialized),
            "the next open must finish a wipe whose marker is durable"
        );
        let (_, submodule) = recovered.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        assert!(!submodule.db.reset_in_progress()?);
        assert!(!submodule.db.chunk_crc_complete()?);
        assert_eq!(live_tx_rows(&recovered)?, 0);
        drop(recovered);

        let (_kept, config, sub) =
            seed_assigned_entropy("open_marker_done", SubmoduleIndexEngine::Rocksdb, hash)?;
        let unassigned = StorageModule::new(&module_info(&sub, None), &config)?;
        assert_eq!(
            unassigned.get_chunk_type(&PartitionChunkOffset::from(0)),
            Some(ChunkType::Uninitialized),
            "an uninterrupted unassign must still wipe"
        );
        let (_, submodule) = unassigned.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
        assert!(!submodule.db.reset_in_progress()?);
        assert!(
            !submodule.db.chunk_crc_complete()?,
            "a finished unassign must clear crc_complete"
        );
        Ok(())
    }

    /// Same hash, new slot. A packing refusal and a coverage refusal both
    /// return before the file is replaced. A matching open publishes the
    /// slot and wipes the labels and index rows of the ledger it left.
    /// The chunk file is not the proof of a wipe: the label and the row are.
    #[test]
    fn refused_open_keeps_the_same_hash_slot() -> eyre::Result<()> {
        let tmp = TempDirBuilder::new()
            .prefix("slot_refuse")
            .with_tracing()
            .build();
        let config = testing_config(tmp.path(), 32, 4);
        let hash = H256::repeat_byte(0x51);
        let sub = tmp.path().join("chunks");
        let storage = StorageModule::new(
            &one_span(3, sub.clone(), Some(submit_slot(&config, hash, 0))),
            &config,
        )?;
        drop(storage);
        let params_path = sub.join(PACKING_PARAMS_FILE_NAME);
        let mut params = PackingParams::from_toml(&params_path)?;
        assert_eq!(params.slot, Some(0));
        assert_eq!(params.last_updated_height, None);
        let recorded = params
            .entropy_packing_iterations
            .expect("open records iterations");
        params.entropy_packing_iterations = Some(recorded + 1);
        params.write_to_disk(&params_path)?;
        let before = fs::read(&params_path)?;

        let err = StorageModule::new(
            &one_span(3, sub.clone(), Some(submit_slot(&config, hash, 1))),
            &config,
        )
        .expect_err("iterations do not match this module");
        assert!(err.to_string().contains("packing identity"), "{err}");
        assert_eq!(
            fs::read(&params_path)?,
            before,
            "a refused open must leave the packing file bytes"
        );
        let loaded = PackingParams::from_toml(&params_path)?;
        assert_eq!(loaded.ledger, Some(DataLedger::Submit.into()));
        assert_eq!(loaded.slot, Some(0));
        assert_eq!(loaded.last_updated_height, None);

        params.entropy_packing_iterations = Some(recorded);
        params.write_to_disk(&params_path)?;
        let before = fs::read(&params_path)?;
        let mut node = config.node_config.clone();
        let irys_types::ConsensusOptions::Custom(consensus) = &mut node.consensus else {
            panic!("test config uses a custom consensus");
        };
        consensus.num_chunks_in_partition = 8;
        let short = Config::new(node, config.peer_id());
        let err = StorageModule::new(
            &one_span(3, sub.clone(), Some(submit_slot(&short, hash, 1))),
            &short,
        )
        .expect_err("the span does not cover the partition");
        assert!(err.to_string().contains("Invalid storage module"), "{err}");
        assert_eq!(
            fs::read(&params_path)?,
            before,
            "a coverage refusal must leave the packing file bytes"
        );

        let bytes = vec![0xE1_u8; 32];
        let offset = PartitionChunkOffset::from(0);
        let seeded = StorageModule::new(
            &one_span(3, sub.clone(), Some(submit_slot(&config, hash, 0))),
            &config,
        )?;
        assert!(seeded.write_chunk(offset, bytes, ChunkType::Data));
        seeded.force_sync_pending_chunks()?;
        assert_eq!(seeded.get_chunk_type(&offset), Some(ChunkType::Data));
        let root = H256::repeat_byte(0x44);
        let (_, submodule) = seeded.get_submodule_for_offset(offset)?;
        submodule.db.update(|tx| {
            tx.add_tx_leaf_binding(
                root,
                &TxLeafBinding {
                    data_root: root,
                    prefix_hash: H256::zero(),
                },
            )?;
            tx.add_tx_path_hash_to_offset_range(offset, offset, Some(root))
        })?;
        assert_eq!(live_tx_rows(&seeded)?, 1, "tx row was not stored");
        drop(seeded);

        let opened = StorageModule::new(
            &one_span(3, sub.clone(), Some(submit_slot(&config, hash, 1))),
            &config,
        )?;
        assert_eq!(
            opened.get_chunk_type(&offset),
            Some(ChunkType::Uninitialized),
            "a same-hash slot change must wipe the live label"
        );
        assert_eq!(interval_label(&sub, 0)?, Some(ChunkType::Uninitialized));
        assert_eq!(
            live_tx_rows(&opened)?,
            0,
            "a same-hash slot change must wipe the index row"
        );
        assert!(
            !opened
                .get_submodule_for_offset(offset)?
                .1
                .db
                .reset_in_progress()?,
            "a slot change left the reset marker set"
        );
        drop(opened);
        let loaded = PackingParams::from_toml(&params_path)?;
        assert_eq!(loaded.ledger, Some(DataLedger::Submit.into()));
        assert_eq!(loaded.slot, Some(1));
        assert_eq!(loaded.last_updated_height, Some(0));
        assert_eq!(loaded.partition_hash, Some(hash));
        assert_eq!(loaded.entropy_packing_iterations, Some(recorded));
        Ok(())
    }

    fn assignment(
        config: &Config,
        hash: H256,
        ledger: Option<u32>,
        slot: Option<usize>,
    ) -> PartitionAssignment {
        PartitionAssignment {
            ledger_id: ledger,
            slot_index: slot,
            miner_address: config.node_config.miner_address(),
            partition_hash: hash,
        }
    }

    /// One `Data` chunk and one tx row on a fresh module.
    fn seed_labeled_data(
        config: &Config,
        sub: &Path,
        assigned: PartitionAssignment,
    ) -> eyre::Result<()> {
        let storage = StorageModule::new(&one_span(3, sub, Some(assigned)), config)?;
        let offset = PartitionChunkOffset::from(0);
        assert!(storage.write_chunk(offset, vec![0xE1_u8; 32], ChunkType::Data));
        storage.force_sync_pending_chunks()?;
        let root = H256::repeat_byte(0x44);
        let (_, submodule) = storage.get_submodule_for_offset(offset)?;
        submodule.db.update(|tx| {
            tx.add_tx_leaf_binding(
                root,
                &TxLeafBinding {
                    data_root: root,
                    prefix_hash: H256::zero(),
                },
            )?;
            tx.add_tx_path_hash_to_offset_range(offset, offset, Some(root))
        })?;
        eyre::ensure!(live_tx_rows(&storage)? == 1, "tx row was not stored");
        drop(storage);
        Ok(())
    }

    fn assert_kept(storage: &StorageModule, sub: &Path, why: &str) -> eyre::Result<()> {
        let offset = PartitionChunkOffset::from(0);
        assert_eq!(
            storage.read_chunk_internal(offset)?,
            vec![0xE1_u8; 32],
            "{why}: the chunk bytes changed"
        );
        assert_eq!(
            storage.get_chunk_type(&offset),
            Some(ChunkType::Data),
            "{why}: the label was wiped"
        );
        assert_eq!(interval_label(sub, 0)?, Some(ChunkType::Data), "{why}");
        assert_eq!(live_tx_rows(storage)?, 1, "{why}: the index row was wiped");
        assert!(
            !storage
                .get_submodule_for_offset(offset)?
                .1
                .db
                .reset_in_progress()?
        );
        Ok(())
    }

    fn assert_wiped(storage: &StorageModule, sub: &Path, why: &str) -> eyre::Result<()> {
        let offset = PartitionChunkOffset::from(0);
        assert_eq!(
            storage.get_chunk_type(&offset),
            Some(ChunkType::Uninitialized),
            "{why}: the label stayed"
        );
        assert_eq!(
            interval_label(sub, 0)?,
            Some(ChunkType::Uninitialized),
            "{why}"
        );
        assert_eq!(live_tx_rows(storage)?, 0, "{why}: the index row stayed");
        assert!(
            !storage
                .get_submodule_for_offset(offset)?
                .1
                .db
                .reset_in_progress()?,
            "{why}: the marker stayed set"
        );
        Ok(())
    }

    fn chunk_file_prefix(sub: &Path, len: usize) -> eyre::Result<Vec<u8>> {
        let mut file = std::fs::File::open(sub.join("chunks.dat"))?;
        let mut buf = vec![0_u8; len];
        std::io::Read::read_exact(&mut file, &mut buf)?;
        Ok(buf)
    }

    fn index_root_on_slot(
        storage: &StorageModule,
        data_root: H256,
        slot: usize,
    ) -> eyre::Result<()> {
        let chunk_size = storage.config.consensus.chunk_size;
        let chunks = storage.config.consensus.num_chunks_in_partition;
        let data_size = chunks * chunk_size;
        let prefix_hash = H256::zero();
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
        let start = slot as u64 * chunks;
        let range = LedgerChunkRange(ledger_chunk_offset_ii!(start, start + chunks - 1));
        storage.index_transaction_data_in(
            &header,
            &tx_path,
            range,
            11,
            Some(DataLedger::Submit),
            Some(0),
        )
    }

    /// Same hash, same ledger, and same slot keeps the bytes. Gaining a ledger
    /// from capacity keeps them too. Leaving a ledger wipes: another ledger,
    /// another slot, or capacity. A wiped expiry still accepts a repack.
    #[test]
    fn same_hash_ledger_drift_wipes_only_when_leaving_a_ledger() -> eyre::Result<()> {
        // MDBX-SUBMODULE-INDEX: drop Mdbx from this list and keep the RocksDB run.
        for engine in [SubmoduleIndexEngine::Mdbx, SubmoduleIndexEngine::Rocksdb] {
            let tmp = TempDirBuilder::new()
                .prefix(&format!("ledger_drift_{engine:?}"))
                .build();
            let config = testing_config_tuned(tmp.path(), 32, 4, |node| {
                node.database = index_database(engine);
            });
            let hash = H256::repeat_byte(0x71);
            let submit = Some(u32::from(DataLedger::Submit));
            let publish = Some(u32::from(DataLedger::Publish));

            let sub = tmp.path().join("same");
            seed_labeled_data(&config, &sub, assignment(&config, hash, submit, Some(0)))?;
            let opened = StorageModule::new(
                &one_span(3, &sub, Some(assignment(&config, hash, submit, Some(0)))),
                &config,
            )?;
            assert_kept(&opened, &sub, "same ledger and slot")?;
            drop(opened);

            let sub = tmp.path().join("slot");
            seed_labeled_data(&config, &sub, assignment(&config, hash, submit, Some(0)))?;
            let opened = StorageModule::new(
                &one_span(3, &sub, Some(assignment(&config, hash, submit, Some(1)))),
                &config,
            )?;
            assert_wiped(&opened, &sub, "same hash, new slot")?;
            let loaded = PackingParams::from_toml(sub.join(PACKING_PARAMS_FILE_NAME))?;
            assert_eq!(loaded.slot, Some(1));
            assert_eq!(loaded.ledger, submit);
            assert_eq!(loaded.partition_hash, Some(hash));
            drop(opened);

            let sub = tmp.path().join("publish");
            seed_labeled_data(&config, &sub, assignment(&config, hash, submit, Some(0)))?;
            let opened = StorageModule::new(
                &one_span(3, &sub, Some(assignment(&config, hash, publish, Some(0)))),
                &config,
            )?;
            assert_wiped(&opened, &sub, "submit to publish")?;
            drop(opened);

            let sub = tmp.path().join("expiry");
            seed_labeled_data(&config, &sub, assignment(&config, hash, submit, Some(0)))?;
            let opened = StorageModule::new(
                &one_span(3, &sub, Some(assignment(&config, hash, None, None))),
                &config,
            )?;
            assert_wiped(&opened, &sub, "ledger to capacity")?;
            let loaded = PackingParams::from_toml(sub.join(PACKING_PARAMS_FILE_NAME))?;
            assert_eq!(loaded.ledger, None);
            assert_eq!(loaded.partition_hash, Some(hash));
            assert_eq!(
                opened.write_entropy_chunk(PartitionChunkOffset::from(0), vec![0x11_u8; 32], hash),
                EntropyWrite::Queued,
                "a wiped expiry did not accept a repack"
            );
            opened.force_sync_pending_chunks()?;
            assert_eq!(
                opened.get_chunk_type(&PartitionChunkOffset::from(0)),
                Some(ChunkType::Entropy),
                "a wiped expiry did not store the repack"
            );
            drop(opened);

            let sub = tmp.path().join("gain");
            seed_labeled_data(&config, &sub, assignment(&config, hash, None, None))?;
            let opened = StorageModule::new(
                &one_span(3, &sub, Some(assignment(&config, hash, submit, Some(0)))),
                &config,
            )?;
            assert_kept(&opened, &sub, "capacity to ledger")?;
            let loaded = PackingParams::from_toml(sub.join(PACKING_PARAMS_FILE_NAME))?;
            assert_eq!(loaded.ledger, submit);
            assert_eq!(loaded.slot, Some(0));
            drop(opened);
        }
        Ok(())
    }

    /// A crash after the slot-change marker, and before the wipe finishes,
    /// leaves the old file and the old label. The next open finishes the wipe.
    #[test]
    fn crash_during_slot_change_wipe_finishes_on_the_next_open() -> eyre::Result<()> {
        // MDBX-SUBMODULE-INDEX: drop Mdbx from this list and keep the RocksDB run.
        for engine in [SubmoduleIndexEngine::Mdbx, SubmoduleIndexEngine::Rocksdb] {
            let tmp = TempDirBuilder::new()
                .prefix(&format!("slot_marker_{engine:?}"))
                .build();
            let config = testing_config_tuned(tmp.path(), 32, 4, |node| {
                node.database = index_database(engine);
            });
            let hash = H256::repeat_byte(0x72);
            let sub = tmp.path().join("sub");
            let submit = Some(u32::from(DataLedger::Submit));
            seed_labeled_data(&config, &sub, assignment(&config, hash, submit, Some(0)))?;
            {
                let index = open_submodule_index(&sub, &config.node_config.database)?;
                if index.stores_offset_crc() {
                    index.set_chunk_crc_complete(true)?;
                }
            }
            {
                let _fail = OpenResetMarkerFail::arm();
                let err = StorageModule::new(
                    &one_span(3, &sub, Some(assignment(&config, hash, submit, Some(1)))),
                    &config,
                )
                .expect_err("the open stops after the marker");
                assert!(
                    err.to_string()
                        .contains("injected failure after the open reset marker"),
                    "{engine:?}: {err}"
                );
            }
            let loaded = PackingParams::from_toml(sub.join(PACKING_PARAMS_FILE_NAME))?;
            assert_eq!(loaded.slot, Some(0), "{engine:?}: the file was published");
            assert_eq!(interval_label(&sub, 0)?, Some(ChunkType::Data));
            {
                let index = open_submodule_index(&sub, &config.node_config.database)?;
                assert!(index.reset_in_progress()?, "{engine:?}: marker missing");
                if index.stores_offset_crc() {
                    assert!(
                        index.chunk_crc_complete()?,
                        "{engine:?}: crc_complete was cleared before the wipe"
                    );
                }
                let rows = index.view(|tx| {
                    tx.tx_intervals_in_range(
                        PartitionChunkOffset::from(0),
                        PartitionChunkOffset::from(0),
                    )
                })?;
                assert!(
                    !rows.is_empty(),
                    "{engine:?}: the interrupted wipe cleared the row"
                );
            }

            let opened = StorageModule::new(
                &one_span(3, &sub, Some(assignment(&config, hash, submit, Some(1)))),
                &config,
            )?;
            assert_wiped(&opened, &sub, "the next open finishes the slot wipe")?;
            let loaded = PackingParams::from_toml(sub.join(PACKING_PARAMS_FILE_NAME))?;
            assert_eq!(loaded.slot, Some(1));
            let (_, submodule) = opened.get_submodule_for_offset(PartitionChunkOffset::from(0))?;
            if submodule.db.stores_offset_crc() {
                assert!(!submodule.db.chunk_crc_complete()?);
            }
        }
        Ok(())
    }

    /// A restart that does not change the assignment keeps the bytes and the
    /// label. A same-hash slot change wipes them. The new root indexed on
    /// that slot is not already stored, so its body can be written.
    #[test]
    fn restart_across_same_hash_slot_change_writes_the_new_root() -> eyre::Result<()> {
        // MDBX-SUBMODULE-INDEX: drop Mdbx from this list and keep the RocksDB run.
        for engine in [SubmoduleIndexEngine::Mdbx, SubmoduleIndexEngine::Rocksdb] {
            let tmp = TempDirBuilder::new()
                .prefix(&format!("slot_restart_{engine:?}"))
                .build();
            let config = testing_config_tuned(tmp.path(), 32, 4, |node| {
                node.database = index_database(engine);
            });
            let hash = H256::repeat_byte(0x73);
            let submit = Some(u32::from(DataLedger::Submit));
            let offset = PartitionChunkOffset::from(0);

            let sub = tmp.path().join("same");
            seed_labeled_data(&config, &sub, assignment(&config, hash, submit, Some(0)))?;
            let opened = StorageModule::new(
                &one_span(3, &sub, Some(assignment(&config, hash, submit, Some(0)))),
                &config,
            )?;
            assert_kept(&opened, &sub, "same assignment restart")?;
            drop(opened);

            let sub = tmp.path().join("slot");
            seed_labeled_data(&config, &sub, assignment(&config, hash, submit, Some(0)))?;
            let opened = StorageModule::new(
                &one_span(3, &sub, Some(assignment(&config, hash, submit, Some(1)))),
                &config,
            )?;
            assert_wiped(&opened, &sub, "same-hash slot change")?;
            assert!(
                !opened.is_data_chunk_persisted_at(offset),
                "{engine:?}: the old Data label still counts as stored"
            );
            assert!(
                opened.data_root_and_tx_offset_at(offset)?.is_none(),
                "{engine:?}: the old placement stayed"
            );
            assert_eq!(
                opened.write_entropy_chunk(offset, vec![0x11_u8; 32], hash),
                EntropyWrite::Queued,
                "{engine:?}: the wiped span did not accept a repack"
            );
            opened.force_sync_pending_chunks()?;
            assert_eq!(
                opened.get_chunk_type(&offset),
                Some(ChunkType::Entropy),
                "{engine:?}: the repack did not store entropy"
            );

            let new_root = H256::repeat_byte(0xB2);
            index_root_on_slot(&opened, new_root, 1)?;
            let jobs = opened.pending_body_migrations()?;
            assert_eq!(jobs.len(), 1, "{engine:?}: the new root has no body job");
            assert_eq!(jobs[0].1.data_root, new_root);
            assert!(
                !opened.is_data_chunk_persisted_at(offset),
                "{engine:?}: the new root was stored before its body"
            );
            let writable = opened.get_writeable_offsets(&UnpackedChunk {
                data_root: new_root,
                tx_offset: TxChunkOffset::from(0),
                ..UnpackedChunk::default()
            })?;
            assert_eq!(
                writable,
                vec![offset],
                "{engine:?}: the new root is not writable"
            );

            let body = vec![0xB2_u8; 32];
            assert!(opened.write_chunk(offset, body.clone(), ChunkType::Data));
            opened.force_sync_pending_chunks()?;
            assert_eq!(opened.read_chunk_internal(offset)?, body);
            assert_eq!(opened.get_chunk_type(&offset), Some(ChunkType::Data));
            assert_eq!(interval_label(&sub, 0)?, Some(ChunkType::Data));
            assert!(
                opened.is_data_chunk_persisted_at(offset),
                "{engine:?}: the new body is not persisted"
            );
            let resolved = opened
                .data_root_and_tx_offset_at(offset)?
                .expect("the new root is placed");
            assert_eq!(resolved.0, new_root);
            assert_eq!(*resolved.1, 0);
            let durable = opened.durable_tx_offsets_for_data_root(
                new_root,
                &BTreeSet::from([TxChunkOffset::from(0)]),
            )?;
            assert_eq!(
                durable,
                BTreeSet::from([TxChunkOffset::from(0)]),
                "{engine:?}: the new body is not durable"
            );
            drop(opened);
        }
        Ok(())
    }

    /// A same-hash slot change that packing refuses writes no reset marker.
    /// The bytes and the label stay. The next open of the original slot
    /// still keeps them.
    #[test]
    fn refused_slot_change_open_writes_no_reset_marker() -> eyre::Result<()> {
        // MDBX-SUBMODULE-INDEX: drop Mdbx from this list and keep the RocksDB run.
        for engine in [SubmoduleIndexEngine::Mdbx, SubmoduleIndexEngine::Rocksdb] {
            let tmp = TempDirBuilder::new()
                .prefix(&format!("slot_refuse_marker_{engine:?}"))
                .build();
            let config = testing_config_tuned(tmp.path(), 32, 4, |node| {
                node.database = index_database(engine);
            });
            let hash = H256::repeat_byte(0x74);
            let submit = Some(u32::from(DataLedger::Submit));
            let sub = tmp.path().join("sub");
            seed_labeled_data(&config, &sub, assignment(&config, hash, submit, Some(0)))?;
            let bytes_before = chunk_file_prefix(&sub, 32)?;
            let params_path = sub.join(PACKING_PARAMS_FILE_NAME);
            let mut params = PackingParams::from_toml(&params_path)?;
            let recorded = params
                .entropy_packing_iterations
                .expect("open records iterations");
            params.entropy_packing_iterations = Some(recorded + 1);
            params.write_to_disk(&params_path)?;
            let file_before = fs::read(&params_path)?;

            let err = StorageModule::new(
                &one_span(3, &sub, Some(assignment(&config, hash, submit, Some(1)))),
                &config,
            )
            .expect_err("iterations do not match");
            assert!(
                err.to_string().contains("packing identity"),
                "{engine:?}: {err}"
            );
            assert_eq!(
                fs::read(&params_path)?,
                file_before,
                "{engine:?}: a refused open published the packing file"
            );
            {
                let index = open_submodule_index(&sub, &config.node_config.database)?;
                assert!(
                    !index.reset_in_progress()?,
                    "{engine:?}: a refused slot change wrote the reset marker"
                );
            }
            assert_eq!(interval_label(&sub, 0)?, Some(ChunkType::Data));
            assert_eq!(chunk_file_prefix(&sub, 32)?, bytes_before);

            params.entropy_packing_iterations = Some(recorded);
            params.write_to_disk(&params_path)?;
            let opened = StorageModule::new(
                &one_span(3, &sub, Some(assignment(&config, hash, submit, Some(0)))),
                &config,
            )?;
            assert_kept(&opened, &sub, "the original slot after a refused open")?;
        }
        Ok(())
    }

    /// A crash after the slot-change marker leaves the old bytes. A later
    /// open that packing refuses does not finish that wipe. The next
    /// accepted open does.
    #[test]
    fn refused_open_does_not_finish_a_slot_change_marker() -> eyre::Result<()> {
        // MDBX-SUBMODULE-INDEX: drop Mdbx from this list and keep the RocksDB run.
        for engine in [SubmoduleIndexEngine::Mdbx, SubmoduleIndexEngine::Rocksdb] {
            let tmp = TempDirBuilder::new()
                .prefix(&format!("slot_marker_refuse_{engine:?}"))
                .build();
            let config = testing_config_tuned(tmp.path(), 32, 4, |node| {
                node.database = index_database(engine);
            });
            let hash = H256::repeat_byte(0x75);
            let submit = Some(u32::from(DataLedger::Submit));
            let sub = tmp.path().join("sub");
            seed_labeled_data(&config, &sub, assignment(&config, hash, submit, Some(0)))?;
            let bytes_before = chunk_file_prefix(&sub, 32)?;
            {
                let _fail = OpenResetMarkerFail::arm();
                let err = StorageModule::new(
                    &one_span(3, &sub, Some(assignment(&config, hash, submit, Some(1)))),
                    &config,
                )
                .expect_err("the open stops after the marker");
                assert!(
                    err.to_string()
                        .contains("injected failure after the open reset marker"),
                    "{engine:?}: {err}"
                );
            }
            let params_path = sub.join(PACKING_PARAMS_FILE_NAME);
            let mut params = PackingParams::from_toml(&params_path)?;
            let recorded = params
                .entropy_packing_iterations
                .expect("open records iterations");
            params.entropy_packing_iterations = Some(recorded + 1);
            params.write_to_disk(&params_path)?;
            let err = StorageModule::new(
                &one_span(3, &sub, Some(assignment(&config, hash, submit, Some(1)))),
                &config,
            )
            .expect_err("a refused open must not finish the wipe");
            assert!(
                err.to_string().contains("packing identity"),
                "{engine:?}: {err}"
            );
            assert_eq!(
                interval_label(&sub, 0)?,
                Some(ChunkType::Data),
                "{engine:?}: the refused open finished the wipe"
            );
            assert_eq!(chunk_file_prefix(&sub, 32)?, bytes_before);
            {
                let index = open_submodule_index(&sub, &config.node_config.database)?;
                assert!(
                    index.reset_in_progress()?,
                    "{engine:?}: the refused open cleared the crash marker"
                );
                let loaded = PackingParams::from_toml(&params_path)?;
                assert_eq!(loaded.slot, Some(0));
            }
            params.entropy_packing_iterations = Some(recorded);
            params.write_to_disk(&params_path)?;
            let opened = StorageModule::new(
                &one_span(3, &sub, Some(assignment(&config, hash, submit, Some(1)))),
                &config,
            )?;
            assert_wiped(&opened, &sub, "the accepted open finishes the crash wipe")?;
        }
        Ok(())
    }

    #[test]
    fn packing_params_without_the_new_field_backfills_and_is_rewritten() {
        let dir = TempDirBuilder::new()
            .prefix("packing_params_backfill")
            .build();
        let path = dir.path().join(PACKING_PARAMS_FILE_NAME);
        let address = IrysAddress::from([0x11; 20]);
        let legacy = PackingParams {
            packing_address: address,
            ..PackingParams::default()
        };
        legacy.write_to_disk(&path).expect("write packing params");
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("entropy_packing_iterations"), "{text}");
        assert!(!text.contains("chunk_size"), "{text}");

        let consensus = ConsensusConfig::testing();
        let loaded = ensure_recorded_packing_params(&path, &consensus).unwrap();
        assert_eq!(
            loaded.entropy_packing_iterations,
            Some(consensus.entropy_packing_iterations)
        );
        assert_eq!(loaded.chunk_size, Some(consensus.chunk_size));
        assert_eq!(loaded.packing_address, address);

        let rewritten = fs::read_to_string(&path).unwrap();
        assert!(
            rewritten.contains("entropy_packing_iterations"),
            "{rewritten}"
        );
        assert!(
            rewritten.contains(&consensus.entropy_packing_iterations.to_string()),
            "{rewritten}"
        );
        assert!(
            rewritten.contains(&format!("chunk_size = {}", consensus.chunk_size)),
            "{rewritten}"
        );
        let again = PackingParams::from_toml(&path).unwrap();
        assert_eq!(again, loaded);
        let second = ensure_recorded_packing_params(&path, &consensus).unwrap();
        assert_eq!(second, loaded);
    }

    /// Alternating single-chunk Entropy / Uninitialized. Touching equals would
    /// merge, so this is the fragmented shape left by a crash.
    fn fragmented_intervals(count: u32) -> StorageIntervals {
        let mut map = StorageIntervals::new();
        for offset in 0..count {
            let kind = if offset % 2 == 0 {
                ChunkType::Entropy
            } else {
                ChunkType::Uninitialized
            };
            map.insert_strict(
                ii(PartitionChunkOffset(offset), PartitionChunkOffset(offset)),
                kind,
            )
            .expect("fragment");
        }
        map
    }

    /// NoditSet rebuild. The production query must stay equivalent to this
    /// and must not pay for it on every call.
    fn intervals_via_set(
        module: &StorageModule,
        chunk_type: ChunkType,
    ) -> Vec<Interval<PartitionChunkOffset>> {
        let intervals = module.intervals.read().unwrap();
        let mut set = NoditSet::new();
        for (interval, ct) in intervals.iter() {
            if *ct == chunk_type {
                let _ = set.insert_merge_touching_or_overlapping(*interval);
            }
        }
        drop(intervals);
        let pending = module.pending_writes.read().unwrap();
        match chunk_type {
            ChunkType::Entropy => {
                pending
                    .iter()
                    .filter(|(_, (_, chunk_type))| *chunk_type == ChunkType::Entropy)
                    .for_each(|(offset, _)| {
                        let _ = set.insert_merge_touching_or_overlapping(ii(*offset, *offset));
                    });
                pending
                    .iter()
                    .filter(|(_, (_, chunk_type))| *chunk_type == ChunkType::Data)
                    .for_each(|(offset, _)| {
                        let _ = set.cut(ii(*offset, *offset));
                    });
            }
            ChunkType::Data => {
                pending
                    .iter()
                    .filter(|(_, (_, pending_chunk_type))| *pending_chunk_type == ChunkType::Data)
                    .for_each(|(offset, _)| {
                        let _ = set.insert_merge_touching_or_overlapping(ii(*offset, *offset));
                    });
            }
            ChunkType::Uninitialized => {
                for offset in pending.keys() {
                    let _ = set.cut(ii(*offset, *offset));
                }
            }
            ChunkType::Interrupted => {}
        }
        set.into_iter().collect()
    }

    fn install_intervals(module: &StorageModule, spans: &[(u32, u32, ChunkType)]) {
        let mut map = StorageIntervals::new();
        for &(start, end, kind) in spans {
            map.insert_merge_touching_if_values_equal(
                ii(PartitionChunkOffset(start), PartitionChunkOffset(end)),
                kind,
            )
            .expect("span");
        }
        *module.intervals.write().unwrap() = map;
    }

    fn queue_pending(module: &StorageModule, offset: u32, kind: ChunkType) {
        module
            .pending_writes
            .write()
            .unwrap()
            .insert(PartitionChunkOffset(offset), (vec![1_u8; 32], kind));
    }

    /// A prefix read counts only the intervals it yields. Pending writes stay
    /// out of this map. A later cut still replaces the stored label.
    #[test]
    fn label_runs_stops_at_the_visited_prefix() -> eyre::Result<()> {
        let (_tmp, _config, storage) =
            open_presence_module("label_runs_prefix", SubmoduleIndexEngine::Rocksdb)?;
        let spans: Vec<(u32, u32, ChunkType)> = (0..8)
            .map(|offset| {
                let kind = if offset % 2 == 0 {
                    ChunkType::Entropy
                } else {
                    ChunkType::Uninitialized
                };
                (offset, offset, kind)
            })
            .collect();
        install_intervals(&storage, &spans);

        let before = storage.interval_visit_count();
        let point: Vec<_> = storage
            .label_runs()
            .overlapping(ii(PartitionChunkOffset(3), PartitionChunkOffset(3)))
            .map(|(interval, kind)| (*interval.start(), *interval.end(), *kind))
            .collect();
        let visits = storage.interval_visit_count() - before;
        assert_eq!(point, vec![(3, 3, ChunkType::Uninitialized)]);
        assert_eq!(visits, 1, "a point overlap read {visits} intervals");

        let before = storage.interval_visit_count();
        let prefix: Vec<_> = storage
            .label_runs()
            .iter()
            .take(3)
            .map(|(interval, kind)| (*interval.start(), *kind))
            .collect();
        let visits = storage.interval_visit_count() - before;
        assert_eq!(
            prefix,
            vec![
                (0, ChunkType::Entropy),
                (1, ChunkType::Uninitialized),
                (2, ChunkType::Entropy),
            ]
        );
        assert_eq!(visits, 3, "take(3) read {visits} intervals");

        let before = storage.interval_visit_count();
        let high: Vec<_> = storage
            .label_runs()
            .overlapping(ii(PartitionChunkOffset(0), PartitionChunkOffset(7)))
            .rev()
            .take(1)
            .map(|(interval, kind)| (*interval.start(), *interval.end(), *kind))
            .collect();
        let visits = storage.interval_visit_count() - before;
        assert_eq!(high, vec![(7, 7, ChunkType::Uninitialized)]);
        assert_eq!(visits, 1, "a reverse prefix read {visits} intervals");

        queue_pending(&storage, 3, ChunkType::Data);
        let raw: Vec<_> = storage
            .label_runs()
            .overlapping(ii(PartitionChunkOffset(3), PartitionChunkOffset(3)))
            .map(|(_, kind)| *kind)
            .collect();
        assert_eq!(raw, vec![ChunkType::Uninitialized]);

        {
            let mut intervals = storage.intervals().write().unwrap();
            StorageModule::cut_then_insert_interval_if_touching(
                &mut intervals,
                PartitionChunkOffset(3),
                ChunkType::Data,
            );
        }
        let stored: Vec<_> = storage
            .label_runs()
            .overlapping(ii(PartitionChunkOffset(3), PartitionChunkOffset(3)))
            .map(|(_, kind)| *kind)
            .collect();
        assert_eq!(stored, vec![ChunkType::Data]);
        Ok(())
    }

    #[test]
    fn get_intervals_matches_the_pending_overlay() -> eyre::Result<()> {
        let (_tmp, _config, storage) =
            open_presence_module("interval_overlay", SubmoduleIndexEngine::Rocksdb)?;
        let cases: &[(&[(u32, u32, ChunkType)], &[(u32, ChunkType)])] = &[
            (
                &[(0, 4, ChunkType::Entropy), (5, 7, ChunkType::Uninitialized)],
                &[],
            ),
            (
                &[(0, 2, ChunkType::Entropy), (3, 7, ChunkType::Uninitialized)],
                &[(3, ChunkType::Entropy)],
            ),
            (
                &[(0, 4, ChunkType::Entropy), (5, 7, ChunkType::Uninitialized)],
                &[(1, ChunkType::Data)],
            ),
            (
                &[
                    (0, 0, ChunkType::Entropy),
                    (2, 2, ChunkType::Entropy),
                    (1, 1, ChunkType::Uninitialized),
                    (3, 7, ChunkType::Data),
                ],
                &[(1, ChunkType::Entropy)],
            ),
            (
                &[(0, 7, ChunkType::Uninitialized)],
                &[(2, ChunkType::Data), (4, ChunkType::Entropy)],
            ),
            (
                &[(0, 3, ChunkType::Interrupted), (4, 7, ChunkType::Entropy)],
                &[(1, ChunkType::Data)],
            ),
            (
                &[(0, 0, ChunkType::Entropy), (1, 7, ChunkType::Uninitialized)],
                &[(0, ChunkType::Data)],
            ),
        ];
        for (spans, pending) in cases {
            install_intervals(&storage, spans);
            storage.pending_writes.write().unwrap().clear();
            for &(offset, kind) in *pending {
                queue_pending(&storage, offset, kind);
            }
            for kind in [
                ChunkType::Entropy,
                ChunkType::Data,
                ChunkType::Uninitialized,
                ChunkType::Interrupted,
            ] {
                assert_eq!(
                    storage.get_intervals(kind),
                    intervals_via_set(&storage, kind),
                    "{kind:?} spans {spans:?} pending {pending:?}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn interval_slice_clips_and_fills_gaps() {
        let mut map = StorageIntervals::new();
        map.insert_merge_touching_if_values_equal(
            ii(PartitionChunkOffset(0), PartitionChunkOffset(10)),
            ChunkType::Entropy,
        )
        .unwrap();
        map.insert_merge_touching_if_values_equal(
            ii(PartitionChunkOffset(11), PartitionChunkOffset(20)),
            ChunkType::Data,
        )
        .unwrap();
        let slice = StorageModule::interval_slice(
            &map,
            ii(PartitionChunkOffset(5), PartitionChunkOffset(15)),
        )
        .unwrap();
        let got: Vec<_> = slice.into_iter().collect();
        assert_eq!(
            got,
            vec![
                (
                    ii(PartitionChunkOffset(5), PartitionChunkOffset(10)),
                    ChunkType::Entropy
                ),
                (
                    ii(PartitionChunkOffset(11), PartitionChunkOffset(15)),
                    ChunkType::Data
                ),
            ]
        );

        let empty = StorageModule::interval_slice(
            &StorageIntervals::new(),
            ii(PartitionChunkOffset(0), PartitionChunkOffset(3)),
        )
        .unwrap();
        let got: Vec<_> = empty.into_iter().collect();
        assert_eq!(
            got,
            vec![(
                ii(PartitionChunkOffset(0), PartitionChunkOffset(3)),
                ChunkType::Uninitialized
            )]
        );
    }

    #[test]
    fn fragmented_interval_queries_stay_near_the_clone_cost() -> eyre::Result<()> {
        let count = 100_000_u32;
        let map = fragmented_intervals(count);
        let started = std::time::Instant::now();
        let _cloned = map.clone();
        let clone_cost = started.elapsed();

        let range = ii(PartitionChunkOffset(0), PartitionChunkOffset(count - 1));
        let started = std::time::Instant::now();
        let slice = StorageModule::interval_slice(&map, range).expect("slice");
        let slice_cost = started.elapsed();
        assert_eq!(slice.len(), count as usize);
        let slice_limit = clone_cost
            .saturating_mul(4)
            .max(std::time::Duration::from_millis(40));
        assert!(
            slice_cost < slice_limit,
            "interval_slice {slice_cost:?} clone {clone_cost:?} limit {slice_limit:?}"
        );
        eprintln!("interval_slice {slice_cost:?} clone {clone_cost:?} limit {slice_limit:?}");
        let mut working = map.clone();
        let cut: Vec<_> = working.cut(range).collect();
        let rebuilt =
            NoditMap::from_iter_strict(cut.into_iter()).expect("cut intervals do not overlap");
        assert_eq!(slice, rebuilt);

        let (_tmp, _config, storage) =
            open_presence_module("fragmented_query", SubmoduleIndexEngine::Rocksdb)?;
        *storage.intervals.write().unwrap() = map;
        let started = std::time::Instant::now();
        let entropy = storage.get_intervals(ChunkType::Entropy);
        let query_cost = started.elapsed();
        assert_eq!(entropy.len(), (count / 2) as usize);
        let query_limit = clone_cost
            .saturating_mul(8)
            .max(std::time::Duration::from_millis(40));
        assert!(
            query_cost < query_limit,
            "get_intervals {query_cost:?} clone {clone_cost:?} limit {query_limit:?}"
        );
        eprintln!("get_intervals {query_cost:?} clone {clone_cost:?} limit {query_limit:?}");
        Ok(())
    }

    /// A covered write-ahead used to clone the interval map before it noticed
    /// the mask already held the batch.
    #[test]
    fn covered_write_ahead_skips_the_interval_clone() -> eyre::Result<()> {
        let count = 100_000_u32;
        let map = fragmented_intervals(count);
        let started = std::time::Instant::now();
        let _cloned = map.clone();
        let clone_cost = started.elapsed();

        let (_tmp, _config, storage) =
            open_presence_module_with("covered_ahead", SubmoduleIndexEngine::Rocksdb, 30)?;
        *storage.intervals.write().unwrap() = map;
        {
            let mut disk = storage.lock_interval_disk();
            disk.mask.insert_merge_touching_or_overlapping(ii(
                PartitionChunkOffset(0),
                PartitionChunkOffset(0),
            ));
        }
        let run = WriteRun {
            start: PartitionChunkOffset(0),
            chunk_type: ChunkType::Entropy,
            bytes: vec![1_u8; 32],
            offsets: vec![PartitionChunkOffset(0)],
        };
        let started = std::time::Instant::now();
        storage.persist_write_ahead(&[run], &std::collections::BTreeSet::new())?;
        let cost = started.elapsed();
        let limit = clone_cost
            .checked_div(2)
            .unwrap_or(clone_cost)
            .max(std::time::Duration::from_millis(1));
        assert!(
            cost < limit,
            "covered persist {cost:?} clone {clone_cost:?} limit {limit:?}"
        );
        eprintln!("covered persist {cost:?} clone {clone_cost:?} limit {limit:?}");
        Ok(())
    }

    /// Unassign while the process is down, then bind a new partition.
    /// The old labels must not survive either open. Both index engines.
    #[test]
    fn unassign_then_rebind_drops_old_labels() -> eyre::Result<()> {
        // MDBX-SUBMODULE-INDEX: drop Mdbx from this list and keep the RocksDB run.
        for engine in [SubmoduleIndexEngine::Mdbx, SubmoduleIndexEngine::Rocksdb] {
            let prefix = format!("unassign_rebind_{engine:?}");
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
            let hash_a = H256::repeat_byte(0xA1);
            let hash_b = H256::repeat_byte(0xB2);
            let assignment = |hash| PartitionAssignment {
                partition_hash: hash,
                miner_address: config.node_config.miner_address(),
                ledger_id: None,
                slot_index: None,
            };
            let info = |assignment| StorageModuleInfo {
                id: 0,
                partition_assignment: assignment,
                submodules: vec![(partition_chunk_offset_ii!(0, 7), "sub".into())],
            };
            let params_path = tmp.path().join("sub").join(PACKING_PARAMS_FILE_NAME);

            let first = StorageModule::new(&info(Some(assignment(hash_a))), &config)?;
            assert_eq!(
                PackingParams::from_toml(&params_path)?.partition_hash,
                Some(hash_a)
            );
            assert!(first.write_chunk(
                PartitionChunkOffset::from(0),
                vec![0x11; 32],
                ChunkType::Entropy
            ));
            first.force_sync_pending_chunks()?;
            assert!(
                !first.get_intervals(ChunkType::Entropy).is_empty(),
                "{engine:?}: entropy was not stored"
            );
            drop(first);

            let unassigned = StorageModule::new(&info(None), &config)?;
            let params = PackingParams::from_toml(&params_path)?;
            assert_eq!(
                params.partition_hash, None,
                "{engine:?}: hash was not cleared"
            );
            assert!(
                unassigned.get_intervals(ChunkType::Entropy).is_empty(),
                "{engine:?}: unassign kept entropy labels {:?}",
                unassigned.get_intervals(ChunkType::Entropy)
            );
            assert_eq!(
                unassigned.get_intervals(ChunkType::Uninitialized),
                vec![partition_chunk_offset_ii!(0, 7)],
                "{engine:?}"
            );
            drop(unassigned);

            let rebound = StorageModule::new(&info(Some(assignment(hash_b))), &config)?;
            assert_eq!(
                PackingParams::from_toml(&params_path)?.partition_hash,
                Some(hash_b),
                "{engine:?}"
            );
            assert!(
                rebound.get_intervals(ChunkType::Entropy).is_empty(),
                "{engine:?}: rebind kept entropy labels {:?}",
                rebound.get_intervals(ChunkType::Entropy)
            );
            assert_eq!(
                rebound.get_intervals(ChunkType::Uninitialized),
                vec![partition_chunk_offset_ii!(0, 7)],
                "{engine:?}"
            );
        }
        Ok(())
    }

    /// An idle completion flush must not clone the interval map. The caller
    /// holds the write lock, so a clone blocks until that lock drops.
    #[test]
    // MDBX-SUBMODULE-INDEX: this test opens only MDBX. Retarget it at RocksDB.
    fn idle_interval_flush_does_not_lock_the_map() -> eyre::Result<()> {
        let (_tmp, _config, storage) =
            open_presence_module_with("idle_flush", SubmoduleIndexEngine::Mdbx, 30)?;
        let intervals = Arc::clone(&storage.intervals);
        let guard = intervals.write().unwrap();
        let returned = std::thread::scope(|scope| {
            let (tx, rx) = std::sync::mpsc::channel();
            scope.spawn(move || {
                let result = storage.flush_interval_completions_locked(false);
                let _ = tx.send(result);
            });
            // Release the map after the wait so a flush that cloned it can
            // finish. The timeout is the result that matters.
            let returned = rx.recv_timeout(Duration::from_secs(2));
            drop(guard);
            returned
        });
        match returned {
            Ok(result) => result?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                eyre::bail!("idle flush waited on the interval map while no completion was due");
            }
            Err(err) => return Err(eyre!(err)),
        }
        Ok(())
    }

    /// A proof with no checksum must not be paired with bytes a later commit
    /// wrote. A stable proof-only row is still served.
    #[test]
    fn proof_only_row_is_not_served_under_a_later_entropy_commit() -> eyre::Result<()> {
        let (_dir, storage) = readahead_module("proof_only_serve", 0, 0)?;
        storage.pack_with_zeros();
        let unpacked = index_four_chunks(&storage)?;
        let offset = PartitionChunkOffset::from(0);
        storage.write_data_chunk(&unpacked[0])?;
        storage.sync_pending_chunks()?;
        storage.set_offset_crc_for_test(offset, None)?;
        // A second offset with no checksum keeps the scan from latching
        // `crc_complete` when this offset is rewritten. The read stays unknown.
        storage.set_offset_crc_for_test(PartitionChunkOffset::from(1), None)?;
        storage.clear_chunk_crc_complete()?;
        assert!(!storage.chunk_crc_complete_for_test());
        let row = storage.offset_meta_for_test(offset)?.expect("row");
        assert!(row.proof.is_some() && row.crc.is_none());

        let stable = storage.generate_full_chunks(offset, offset)?;
        assert_eq!(
            stable.len(),
            1,
            "a stable proof-only row must still be served"
        );
        assert_ne!(stable[&offset].bytes.0, vec![0xEE_u8; 32]);

        storage.pause_next_sync_at(SyncPausePoint::AfterPresenceCopy);
        let storage_ref = &storage;
        let outcome = std::thread::scope(|scope| -> eyre::Result<()> {
            let _release = SyncPauseRelease(storage_ref);
            let writer = scope.spawn(move || {
                assert!(storage_ref.write_chunk(offset, vec![0xEE; 32], ChunkType::Entropy));
                storage_ref.force_sync_pending_chunks()
            });
            assert!(
                storage_ref.wait_for_sync_pause(Duration::from_secs(5)),
                "entropy commit did not pause"
            );
            let paused = storage_ref
                .offset_meta_for_test(offset)?
                .expect("paused row");
            assert!(paused.proof.is_some() && paused.crc.is_none());

            let before = storage_ref.index_view_count();
            let (tx, rx) = std::sync::mpsc::channel();
            scope.spawn(move || {
                let served = storage_ref.generate_full_chunks(offset, offset);
                let _ = tx.send(served);
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while storage_ref.index_view_count() == before {
                if Instant::now() >= deadline {
                    eyre::bail!("reader did not load the old row");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            storage_ref.release_sync_pause();
            let served = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("reader finished")?;
            writer.join().expect("writer")?;
            assert!(
                served.is_empty(),
                "entropy bytes were served under the proof loaded before the commit: {served:?}"
            );
            let finished = storage.offset_meta_for_test(offset)?.expect("finished row");
            assert!(finished.crc.is_some() && finished.proof.is_none());
            Ok(())
        });
        outcome?;
        Ok(())
    }

    /// A body write that packs after `reset` has cleared `pending`, and before
    /// the interval map flips, must not survive the reset.
    #[test]
    fn reset_drops_a_data_write_that_lands_after_the_pending_clear() -> eyre::Result<()> {
        let tmp_dir = TempDirBuilder::new()
            .prefix("reset_late_data")
            .with_tracing()
            .build();
        let config = testing_config(tmp_dir.path(), 5, 5);
        let storage_module =
            StorageModule::new(&one_span(4, "hdd0", Some(submit_assignment(0))), &config)?;
        storage_module.pack_with_zeros();
        storage_module.force_sync_pending_chunks()?;
        let data_root = H256::random();
        let data_tx = data_header(data_root, 5);
        storage_module.index_transaction_data(
            &data_tx,
            &vec![5, 6, 7, 8],
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            0,
        )?;
        let chunk = five_byte_chunk(data_root);

        storage_module.pause_next_sync_at(SyncPausePoint::AfterResetPendingClear);
        let storage_ref = &storage_module;
        let outcome = std::thread::scope(|scope| -> eyre::Result<()> {
            let _release = SyncPauseRelease(storage_ref);
            let reset_thread = scope.spawn(move || storage_ref.reset());
            assert!(
                storage_ref.wait_for_sync_pause(Duration::from_secs(5)),
                "reset did not pause after the pending clear"
            );
            storage_ref
                .write_data_chunk(&chunk)
                .expect("body write in the reset window");
            assert!(
                storage_ref.has_pending_writes(),
                "the body write did not land after the clear"
            );
            storage_ref.release_sync_pause();
            reset_thread.join().expect("reset thread")?;
            assert!(
                !storage_ref.has_pending_writes(),
                "reset kept a body write that landed after its pending clear"
            );
            assert_eq!(
                storage_ref.get_intervals(ChunkType::Uninitialized),
                vec![partition_chunk_offset_ii!(0, 4)]
            );
            Ok(())
        });
        outcome?;
        Ok(())
    }

    fn crc_hold_module(
        prefix: &str,
        persist_secs: u64,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        Config,
        StorageModule,
    )> {
        let tmp_dir = TempDirBuilder::new().prefix(prefix).with_tracing().build();
        let chunk_size = 32_u64;
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: 4,
                entropy_packing_iterations: 1,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
                entropy_sweep_max_bytes: chunk_size * 4,
                entropy_read_grace_ms: 0,
                entropy_read_batch_bytes: chunk_size * 4,
                min_durable_interval_ms: 0,
                intervals_persist_secs: persist_secs,
                ..StorageSyncConfig::default()
            },
            database: index_database(SubmoduleIndexEngine::Rocksdb),
            base_directory: tmp_dir.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage = StorageModule::new(&crc_hold_info(tmp_dir.path()), &config)?;
        Ok((tmp_dir, config, storage))
    }

    fn crc_hold_info(dir: &std::path::Path) -> StorageModuleInfo {
        StorageModuleInfo {
            id: 0,
            partition_assignment: Some(irys_types::partition::PartitionAssignment {
                ledger_id: Some(DataLedger::Submit.into()),
                slot_index: Some(0),
                miner_address: irys_types::IrysAddress::from([0xAA; 20]),
                partition_hash: H256::repeat_byte(0x11),
            }),
            submodules: vec![(partition_chunk_offset_ii!(0, 3), dir.join("hdd0"))],
        }
    }

    fn file_label(storage: &StorageModule, offset: u32) -> eyre::Result<Option<ChunkType>> {
        Ok(file_intervals(storage)?
            .get_at_point(PartitionChunkOffset::from(offset))
            .copied())
    }

    fn live_label(storage: &StorageModule, offset: u32) -> Option<ChunkType> {
        storage
            .intervals
            .read()
            .unwrap()
            .get_at_point(PartitionChunkOffset::from(offset))
            .copied()
    }

    /// Durable `Data` at offset 0, file already completed to `Data`.
    fn settle_data_offset(
        prefix: &str,
        persist_secs: u64,
    ) -> eyre::Result<(
        irys_testing_utils::utils::tempfile::TempDir,
        Config,
        StorageModule,
        Vec<UnpackedChunk>,
        DataTransactionHeader,
        TxPath,
    )> {
        let (tmp, config, storage) = crc_hold_module(prefix, persist_secs)?;
        storage.pack_with_zeros();
        let (unpacked, header, tx_path) = index_partition(&storage)?;
        storage.write_data_chunk(&unpacked[0])?;
        storage.force_sync_pending_chunks()?;
        let offset = PartitionChunkOffset::from(0);
        assert_eq!(
            file_label(&storage, 0)?,
            Some(ChunkType::Data),
            "settled file"
        );
        assert!(storage.is_data_chunk_durable_at(offset), "settled durable");
        assert!(
            storage.generate_full_chunk(offset)?.is_some(),
            "settled proof"
        );
        Ok((tmp, config, storage, unpacked, header, tx_path))
    }

    /// Queue entropy, then a new proved body, and sync both. The offset's
    /// checksum and data root change. A settled `Data` label is not a
    /// `write_data_chunk` target, so the entropy write has to land first.
    fn replace_settled_data(
        storage: &StorageModule,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<DataRoot> {
        let chunk_size = storage.config.consensus.chunk_size as usize;
        eyre::ensure!(
            storage.write_chunk(offset, vec![0xE1; chunk_size], ChunkType::Entropy),
            "entropy replacement was refused"
        );
        let signer = IrysSigner::random_signer(&storage.config.consensus);
        let tx = signer
            .sign_transaction(signer.create_transaction(vec![0xAB; chunk_size], H256::zero())?)?;
        let replacement = tx.data_chunks()?;
        let (_, proofs) = DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&tx.header));
        storage.index_transaction_data(
            &tx.header,
            &proofs[0].proof,
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 0)),
            1,
        )?;
        storage.write_data_chunk(&replacement[0])?;
        storage.force_sync_pending_chunks()?;
        Ok(tx.header.data_root)
    }

    /// A serve that already copied the old bytes must return the pair a
    /// durable point commits afterwards. A serve with no later commit still
    /// returns the copied bytes.
    #[test]
    fn serve_after_body_read_returns_the_committed_pair() -> eyre::Result<()> {
        let (tmp, _config, storage, ..) = settle_data_offset("serve_replace", 0)?;
        let _tmp = tmp;
        let storage = Arc::new(storage);
        let offset = PartitionChunkOffset::from(0);
        let old_packed = storage
            .read_chunks(partition_chunk_offset_ii!(0, 0))?
            .remove(&offset)
            .ok_or_else(|| eyre::eyre!("settled bytes missing"))?
            .0;
        let old_crc = storage
            .offset_meta_for_test(offset)?
            .and_then(|row| row.crc);
        let stable = storage
            .generate_full_chunk(offset)?
            .ok_or_else(|| eyre::eyre!("settled serve omitted the offset"))?;
        eyre::ensure!(
            stable.bytes.0 == old_packed,
            "a serve with no later commit did not return the copied bytes"
        );

        storage.pause_next_serve_after_body_read();
        let reading = Arc::clone(&storage);
        let (tx_serve, rx_serve) = std::sync::mpsc::channel();
        std::thread::scope(|scope| -> eyre::Result<()> {
            let _release = SyncPauseRelease(&storage);
            scope.spawn(move || {
                let _ = tx_serve.send(reading.generate_full_chunk(offset));
            });
            eyre::ensure!(
                storage.wait_for_sync_pause(Duration::from_secs(5)),
                "serve did not pause after the body read"
            );
            let new_root = replace_settled_data(&storage, offset)?;
            let new_crc = storage
                .offset_meta_for_test(offset)?
                .and_then(|row| row.crc);
            eyre::ensure!(
                new_crc != old_crc,
                "replacement did not commit a new checksum"
            );
            let committed = storage
                .read_chunks(partition_chunk_offset_ii!(0, 0))?
                .remove(&offset)
                .ok_or_else(|| eyre::eyre!("committed bytes missing"))?
                .0;
            eyre::ensure!(
                committed != old_packed,
                "replacement did not change the bytes on disk"
            );
            drop(_release);
            let served = rx_serve
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| eyre::eyre!("serve did not finish"))??
                .ok_or_else(|| eyre::eyre!("serve omitted the offset"))?;
            eyre::ensure!(
                served.bytes.0 == committed,
                "served the pre-commit bytes after the new checksum committed"
            );
            eyre::ensure!(
                served.data_root == new_root,
                "served proof is not the committed data root"
            );
            Ok(())
        })?;
        let again = storage
            .generate_full_chunk(offset)?
            .ok_or_else(|| eyre::eyre!("stable serve omitted the offset"))?;
        eyre::ensure!(
            again.bytes.0
                == storage
                    .read_chunks(partition_chunk_offset_ii!(0, 0))?
                    .remove(&offset)
                    .ok_or_else(|| eyre::eyre!("stable bytes missing"))?
                    .0,
            "a later serve did not return the committed bytes"
        );
        Ok(())
    }

    /// A durable point for a different offset does not retire this offset's
    /// copied bytes. The re-read still sees the checksum they matched.
    #[test]
    fn serve_after_body_read_keeps_bytes_when_its_checksum_stays() -> eyre::Result<()> {
        let (tmp, _config, storage, unpacked, ..) = settle_data_offset("serve_neighbor", 0)?;
        let _tmp = tmp;
        let storage = Arc::new(storage);
        let offset = PartitionChunkOffset::from(0);
        let old_packed = storage
            .read_chunks(partition_chunk_offset_ii!(0, 0))?
            .remove(&offset)
            .ok_or_else(|| eyre::eyre!("settled bytes missing"))?
            .0;
        let old_crc = storage
            .offset_meta_for_test(offset)?
            .and_then(|row| row.crc);

        storage.pause_next_serve_after_body_read();
        let reading = Arc::clone(&storage);
        let (tx_serve, rx_serve) = std::sync::mpsc::channel();
        std::thread::scope(|scope| -> eyre::Result<()> {
            let _release = SyncPauseRelease(&storage);
            scope.spawn(move || {
                let _ = tx_serve.send(reading.generate_full_chunk(offset));
            });
            eyre::ensure!(
                storage.wait_for_sync_pause(Duration::from_secs(5)),
                "serve did not pause after the body read"
            );
            storage.write_data_chunk(&unpacked[1])?;
            storage.force_sync_pending_chunks()?;
            let crc = storage
                .offset_meta_for_test(offset)?
                .and_then(|row| row.crc);
            eyre::ensure!(
                crc == old_crc,
                "neighbor write changed this offset's checksum"
            );
            drop(_release);
            let served = rx_serve
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| eyre::eyre!("serve did not finish"))??
                .ok_or_else(|| eyre::eyre!("serve omitted the offset"))?;
            eyre::ensure!(
                served.bytes.0 == old_packed,
                "serve dropped bytes whose checksum was still current"
            );
            Ok(())
        })?;
        Ok(())
    }

    /// Assigned module with one-iteration packing. `engine` selects the index.
    fn pair_module(
        prefix: &str,
        engine: SubmoduleIndexEngine,
    ) -> eyre::Result<(irys_testing_utils::utils::tempfile::TempDir, StorageModule)> {
        let tmp = TempDirBuilder::new().prefix(prefix).build();
        let chunk_size = 32_u64;
        let node_config = NodeConfig {
            consensus: irys_types::ConsensusOptions::Custom(ConsensusConfig {
                chunk_size,
                num_chunks_in_partition: 4,
                entropy_packing_iterations: 1,
                ..ConsensusConfig::testing()
            }),
            storage: StorageSyncConfig {
                num_writes_before_sync: 1,
                entropy_sweep_max_bytes: chunk_size * 4,
                entropy_read_grace_ms: 0,
                entropy_read_batch_bytes: chunk_size * 4,
                min_durable_interval_ms: 0,
                intervals_persist_secs: 0,
                ..StorageSyncConfig::default()
            },
            database: index_database(engine),
            base_directory: tmp.path().to_path_buf(),
            ..NodeConfig::testing()
        };
        let config = Config::new_with_random_peer_id(node_config);
        let storage = StorageModule::new(
            &StorageModuleInfo {
                id: 0,
                partition_assignment: Some(irys_types::partition::PartitionAssignment {
                    ledger_id: Some(DataLedger::Submit.into()),
                    slot_index: Some(0),
                    miner_address: config.node_config.miner_address(),
                    partition_hash: H256::repeat_byte(0x11),
                }),
                submodules: vec![(partition_chunk_offset_ii!(0, 3), tmp.path().join("hdd0"))],
            },
            &config,
        )?;
        Ok((tmp, storage))
    }

    /// Index `pattern` as one signed transaction. One byte is one chunk.
    fn index_pattern(
        storage: &StorageModule,
        pattern: &[u8],
        block_height: u64,
    ) -> eyre::Result<(Vec<UnpackedChunk>, DataRoot)> {
        let chunk_size = storage.config.consensus.chunk_size as usize;
        let signer = IrysSigner::random_signer(&storage.config.consensus);
        let data = pattern
            .iter()
            .flat_map(|byte| vec![*byte; chunk_size])
            .collect::<Vec<_>>();
        let tx = signer.sign_transaction(signer.create_transaction(data, H256::zero())?)?;
        let unpacked = tx.data_chunks()?;
        eyre::ensure!(
            unpacked.len() == pattern.len(),
            "pattern {}, chunks {}",
            pattern.len(),
            unpacked.len()
        );
        let (_, proofs) = DataTransactionLedger::merklize_tx_root(std::slice::from_ref(&tx.header));
        let last = u32::try_from(pattern.len() - 1)?;
        storage.index_transaction_data(
            &tx.header,
            &proofs[0].proof,
            LedgerChunkRange(ledger_chunk_offset_ii!(0, last)),
            block_height,
        )?;
        Ok((unpacked, tx.header.data_root))
    }

    fn packed_prefix(
        storage: &StorageModule,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<Vec<u8>> {
        storage
            .read_chunks(partition_chunk_offset_ii!(0, 0))?
            .remove(&offset)
            .map(|(bytes, _)| bytes)
            .ok_or_else(|| eyre::eyre!("packed bytes missing at {}", offset.0))
    }

    /// A serve started while a queued body is paused after pwrite.
    ///
    /// `write_data_chunk` only queues the packed bytes. The pause is the
    /// following sync. Returns that serve. The sync is released if the serve
    /// is still blocked on the write hold, then the sync is joined.
    fn serve_during_paused_pwrite(
        storage: &Arc<StorageModule>,
        offset: PartitionChunkOffset,
        chunk: &UnpackedChunk,
    ) -> eyre::Result<Option<PackedChunk>> {
        storage
            .write_data_chunk(chunk)
            .map_err(|err| eyre::eyre!("body write failed: {err}"))?;
        storage.pause_next_sync_at(SyncPausePoint::AfterPwrite);
        let (tx_write, rx_write) = std::sync::mpsc::channel();
        let (tx_serve, rx_serve) = std::sync::mpsc::channel();
        std::thread::scope(|scope| -> eyre::Result<Option<PackedChunk>> {
            let mut release = Some(SyncPauseRelease(storage.as_ref()));
            let writing = Arc::clone(storage);
            scope.spawn(move || {
                let _ = tx_write.send(writing.force_sync_pending_chunks());
            });
            eyre::ensure!(
                storage.wait_for_sync_pause(Duration::from_secs(5)),
                "body sync did not pause after pwrite"
            );
            let views = storage.index_view_count();
            let reading = Arc::clone(storage);
            scope.spawn(move || {
                let _ = tx_serve.send(reading.generate_full_chunk(offset));
            });
            let started = Instant::now();
            let mut saw_view: Option<Instant> = None;
            let served = loop {
                if let Ok(result) = rx_serve.try_recv() {
                    break result;
                }
                if storage.index_view_count() > views && saw_view.is_none() {
                    saw_view = Some(Instant::now());
                }
                // An empty index returns before the write hold. A loaded path
                // waits in `yield_to_writes` until this pause ends.
                if saw_view.is_some_and(|at| at.elapsed() >= Duration::from_secs(1)) {
                    release.take();
                    break rx_serve.recv_timeout(Duration::from_secs(5)).map_err(|_| {
                        eyre::eyre!("serve did not finish after the write hold dropped")
                    })?;
                }
                eyre::ensure!(
                    started.elapsed() < Duration::from_secs(5),
                    "serve did not load the index during the paused body"
                );
                std::thread::sleep(Duration::from_millis(10));
            }?;
            release.take();
            rx_write
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| eyre::eyre!("body write did not finish"))?
                .map_err(|err| eyre::eyre!("body write failed: {err}"))?;
            Ok(served)
        })
    }

    /// Entropy bytes are not served under the data path from the previous
    /// commit. A placement update does not bring that path back. A body that
    /// is durable only through pwrite is not paired with it either. After the
    /// new body commits, the serve is that body's path and bytes.
    #[rstest]
    // MDBX-SUBMODULE-INDEX: delete this MDBX case and keep the RocksDB case.
    #[case::mdbx(SubmoduleIndexEngine::Mdbx)]
    #[case::rocksdb(SubmoduleIndexEngine::Rocksdb)]
    fn entropy_overwrite_is_not_served_under_the_previous_path(
        #[case] engine: SubmoduleIndexEngine,
    ) -> eyre::Result<()> {
        let prefix = format!("entropy_path_{engine:?}");
        let (tmp, storage) = pair_module(&prefix, engine)?;
        let _tmp = tmp;
        let storage = Arc::new(storage);
        storage.pack_with_zeros();
        let offset = PartitionChunkOffset::from(0);
        let (first, root1) = index_pattern(&storage, &[0x11], 0)?;
        storage.write_data_chunk(&first[0])?;
        storage.force_sync_pending_chunks()?;
        let path1 =
            data_path_at(&storage, 0)?.ok_or_else(|| eyre::eyre!("settled path missing"))?;
        let stable = storage
            .generate_full_chunk(offset)?
            .ok_or_else(|| eyre::eyre!("settled serve omitted the offset"))?;
        eyre::ensure!(
            stable.data_root == root1 && stable.data_path.0 == path1,
            "{engine:?} settled serve is not the committed pair"
        );
        eyre::ensure!(
            stable.bytes.0 == packed_prefix(&storage, offset)?,
            "{engine:?} settled serve bytes are not the committed body"
        );

        let chunk_size = storage.config.consensus.chunk_size as usize;
        eyre::ensure!(
            storage.write_chunk(offset, vec![0xE1; chunk_size], ChunkType::Entropy),
            "entropy overwrite was refused"
        );
        storage.force_sync_pending_chunks()?;
        let entropy = packed_prefix(&storage, offset)?;
        eyre::ensure!(
            entropy.first() == Some(&0xE1),
            "{engine:?} entropy bytes did not land"
        );
        let after_entropy = storage.generate_full_chunk(offset)?;
        let path_after_entropy = data_path_at(&storage, 0)?;
        if let Some(chunk) = &after_entropy {
            eyre::bail!(
                "{engine:?} after entropy: root {root1:?} served {:?} b0 {:02x} path_is_old {}",
                chunk.data_root,
                chunk.bytes.0.first().copied().unwrap_or(0xff),
                path_after_entropy
                    .as_ref()
                    .is_some_and(|bytes| bytes == &chunk.data_path.0)
            );
        }
        eyre::ensure!(
            path_after_entropy.is_none(),
            "{engine:?} kept the data path after the entropy commit"
        );

        let (_second, root2) = index_pattern(&storage, &[0x22], 1)?;
        let placed = storage.data_root_and_tx_offset_at(offset)?;
        eyre::ensure!(
            placed.as_ref().map(|(root, _)| *root) == Some(root2),
            "{engine:?} placement did not move to the new root"
        );
        let after_index = storage.generate_full_chunk(offset)?;
        if let Some(chunk) = &after_index {
            eyre::bail!(
                "{engine:?} after index, before the body: root {:?} b0 {:02x} path_len {}",
                chunk.data_root,
                chunk.bytes.0.first().copied().unwrap_or(0xff),
                chunk.data_path.0.len()
            );
        }

        let (replacement, _) = index_pattern(&storage, &[0x33], 2)?;
        let during = serve_during_paused_pwrite(&storage, offset, &replacement[0])?;
        if let Some(chunk) = &during {
            eyre::bail!(
                "{engine:?} uncommitted body paired with a stored path: root {:?} b0 {:02x} path_is_old {}",
                chunk.data_root,
                chunk.bytes.0.first().copied().unwrap_or(0xff),
                chunk.data_path.0 == path1
            );
        }
        let committed = storage
            .generate_full_chunk(offset)?
            .ok_or_else(|| eyre::eyre!("{engine:?} committed serve omitted the offset"))?;
        let path_now = data_path_at(&storage, 0)?
            .ok_or_else(|| eyre::eyre!("{engine:?} committed path missing"))?;
        eyre::ensure!(
            committed.data_root == replacement[0].data_root && committed.data_path.0 == path_now,
            "{engine:?} committed serve is not the new pair"
        );
        eyre::ensure!(
            committed.bytes.0 == packed_prefix(&storage, offset)?
                && committed.bytes.0.first() != Some(&0xE1),
            "{engine:?} committed serve is still the entropy body"
        );
        Ok(())
    }

    /// The path loaded before `yield_to_writes` is not the path committed
    /// with the new body. The live label is `Entropy` so the write is legal,
    /// and the previous path is still stored because no entropy commit ran.
    #[test]
    // MDBX-SUBMODULE-INDEX: delete this test. It checks the tx path MDBX commits with the bytes.
    fn mdbx_serve_after_pwrite_uses_the_path_committed_with_the_bytes() -> eyre::Result<()> {
        let (tmp, storage) = pair_module("mdbx_pwrite_path", SubmoduleIndexEngine::Mdbx)?;
        let _tmp = tmp;
        let storage = Arc::new(storage);
        storage.pack_with_zeros();
        let offset = PartitionChunkOffset::from(0);
        let (first, _) = index_pattern(&storage, &[0x11], 0)?;
        storage.write_data_chunk(&first[0])?;
        storage.force_sync_pending_chunks()?;
        let path_a = data_path_at(&storage, 0)?.ok_or_else(|| eyre::eyre!("path A missing"))?;
        {
            let mut intervals = storage.intervals.write().unwrap();
            let span = ii(offset, offset);
            let _ = intervals.cut(span);
            let _ = intervals.insert_merge_touching_if_values_equal(span, ChunkType::Entropy);
        }
        eyre::ensure!(
            live_label(&storage, 0) == Some(ChunkType::Entropy),
            "offset 0 is not a data-write target"
        );
        eyre::ensure!(
            data_path_at(&storage, 0)?.as_ref() == Some(&path_a),
            "painting the live label dropped the stored path"
        );
        let (second, root2) = index_pattern(&storage, &[0x22], 1)?;
        let during = serve_during_paused_pwrite(&storage, offset, &second[0])?;
        let served = during.ok_or_else(|| eyre::eyre!("serve omitted the committed body"))?;
        let path_now =
            data_path_at(&storage, 0)?.ok_or_else(|| eyre::eyre!("committed path missing"))?;
        eyre::ensure!(path_now != path_a, "the body commit kept path A");
        eyre::ensure!(
            served.data_path.0 == path_now,
            "served a data path from before the body commit"
        );
        eyre::ensure!(
            served.data_root == root2,
            "served proof is not the committed data root"
        );
        let disk = packed_prefix(&storage, offset)?;
        eyre::ensure!(
            served.bytes.0 == disk,
            "served bytes are not the bytes committed with the path"
        );
        let again = storage
            .generate_full_chunk(offset)?
            .ok_or_else(|| eyre::eyre!("later serve omitted the offset"))?;
        eyre::ensure!(
            again.data_path.0 == served.data_path.0 && again.bytes.0 == served.bytes.0,
            "a later serve does not match the overlapped serve"
        );
        Ok(())
    }

    /// Two transactions share chunk 0's unpacked bytes and differ in chunk 1.
    /// The packed checksum stays. The data path does not. A serve that copied
    /// the first body must return the path committed with that checksum.
    #[test]
    fn serve_after_body_read_reloads_when_the_path_changes_and_the_crc_stays() -> eyre::Result<()> {
        let (tmp, storage) = pair_module("serve_same_crc", SubmoduleIndexEngine::Rocksdb)?;
        let _tmp = tmp;
        let storage = Arc::new(storage);
        storage.pack_with_zeros();
        let offset = PartitionChunkOffset::from(0);
        let (first, root1) = index_pattern(&storage, &[0x11, 0x22], 0)?;
        storage.write_data_chunk(&first[0])?;
        storage.force_sync_pending_chunks()?;
        let path1 = data_path_at(&storage, 0)?.ok_or_else(|| eyre::eyre!("first path missing"))?;
        let crc1 = storage
            .offset_meta_for_test(offset)?
            .and_then(|row| row.crc);
        let packed = packed_prefix(&storage, offset)?;
        let stable = storage
            .generate_full_chunk(offset)?
            .ok_or_else(|| eyre::eyre!("settled serve omitted the offset"))?;
        eyre::ensure!(
            stable.data_root == root1 && stable.data_path.0 == path1 && stable.bytes.0 == packed,
            "a serve with no later commit did not return the copied pair"
        );

        storage.pause_next_serve_after_body_read();
        let reading = Arc::clone(&storage);
        let (tx_serve, rx_serve) = std::sync::mpsc::channel();
        std::thread::scope(|scope| -> eyre::Result<()> {
            let _release = SyncPauseRelease(&storage);
            scope.spawn(move || {
                let _ = tx_serve.send(reading.generate_full_chunk(offset));
            });
            eyre::ensure!(
                storage.wait_for_sync_pause(Duration::from_secs(5)),
                "serve did not pause after the body read"
            );
            let chunk_size = storage.config.consensus.chunk_size as usize;
            // The same pad the first pack read. The unpacked bytes match, so
            // the packed checksum stays and only the data path changes.
            // Raw write: the live label is already Data. An entropy pack
            // refuses that label.
            eyre::ensure!(
                storage.write_chunk(offset, vec![0; chunk_size], ChunkType::Entropy),
                "entropy overwrite was refused"
            );
            storage.force_sync_pending_chunks()?;
            let (second, root2) = index_pattern(&storage, &[0x11, 0x33], 1)?;
            storage.write_data_chunk(&second[0])?;
            storage.force_sync_pending_chunks()?;
            let path2 =
                data_path_at(&storage, 0)?.ok_or_else(|| eyre::eyre!("second path missing"))?;
            let crc2 = storage
                .offset_meta_for_test(offset)?
                .and_then(|row| row.crc);
            eyre::ensure!(
                crc1.is_some() && crc1 == crc2,
                "the shared payload did not keep the checksum: {crc1:?} then {crc2:?}"
            );
            eyre::ensure!(
                path1 != path2,
                "the second transaction reused the data path"
            );
            eyre::ensure!(
                packed_prefix(&storage, offset)? == packed,
                "the shared payload did not pack to the same bytes"
            );
            drop(_release);
            let served = rx_serve
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| eyre::eyre!("serve did not finish"))??
                .ok_or_else(|| {
                    eyre::eyre!("serve omitted a body whose checksum is still current")
                })?;
            eyre::ensure!(
                served.data_path.0 == path2,
                "served the data path from the previous transaction"
            );
            eyre::ensure!(
                served.data_root == root2,
                "served proof is not the committed data root"
            );
            eyre::ensure!(
                served.bytes.0 == packed,
                "serve dropped bytes whose checksum was still current"
            );
            Ok(())
        })?;
        let again = storage
            .generate_full_chunk(offset)?
            .ok_or_else(|| eyre::eyre!("later serve omitted the offset"))?;
        eyre::ensure!(
            again.data_path.0
                == data_path_at(&storage, 0)?.ok_or_else(|| eyre::eyre!("later path missing"))?
                && again.bytes.0 == packed,
            "a later serve is not the committed pair"
        );
        Ok(())
    }

    fn reopen_crc_module(tmp: &std::path::Path, config: &Config) -> eyre::Result<StorageModule> {
        StorageModule::new(&crc_hold_info(tmp), config)
    }

    /// Reload must not treat the offset as durable data or serve a proof.
    fn assert_not_served_as_data(
        storage: &StorageModule,
        offset: PartitionChunkOffset,
    ) -> eyre::Result<()> {
        assert_ne!(
            storage.get_chunk_type(&offset),
            Some(ChunkType::Data),
            "reopen kept Data"
        );
        assert!(
            !storage.is_data_chunk_durable_at(offset),
            "reopen reports the offset durable"
        );
        assert!(
            storage.generate_full_chunk(offset)?.is_none(),
            "reopen served a proof"
        );
        Ok(())
    }

    /// A due completion clones the live map, then a checksum cut publishes
    /// `Uninitialized`, then the completion commits. The committed file must
    /// stay `Uninitialized`. Period 0 does not clone: batching is off, so the
    /// flush returns before the pause. A crash after either order reloads
    /// `Uninitialized` and open drops the placement.
    #[rstest]
    #[case::period_0(0_u64)]
    #[case::period_30(30_u64)]
    fn crc_cut_beats_completion_cloned_before_the_cut(
        #[case] persist_secs: u64,
    ) -> eyre::Result<()> {
        let prefix = format!("crc_cut_vs_flush_{persist_secs}");
        let (tmp, config, storage, ..) = settle_data_offset(&prefix, persist_secs)?;
        let storage = Arc::new(storage);
        let offset = PartitionChunkOffset::from(0);
        {
            let mut disk = storage.lock_interval_disk();
            disk.dirty.insert(PartitionChunkOffset::from(0));
            disk.last_completion.clear();
        }
        storage.pause_next_completion_clone();
        if persist_secs == 0 {
            storage.flush_interval_completions_locked(true)?;
            assert!(
                !storage.wait_for_interval_image_pause(Duration::from_millis(200)),
                "period 0 cloned a completion"
            );
            storage.note_crc_mismatch(offset, ChunkType::Data, "test")?;
        } else {
            let flush_mod = Arc::clone(&storage);
            let (tx, rx) = std::sync::mpsc::channel();
            let flush_worker = std::thread::spawn(move || {
                let _ = tx.send(flush_mod.flush_interval_completions_locked(true));
            });
            assert!(
                storage.wait_for_interval_image_pause(Duration::from_secs(5)),
                "the completion did not clone"
            );
            let noted = storage.note_crc_mismatch(offset, ChunkType::Data, "test");
            storage.release_interval_image_pause();
            let flushed = rx.recv_timeout(Duration::from_secs(10));
            noted?;
            flushed.map_err(|err| eyre!("completion flush did not finish: {err}"))??;
            // The worker sends before its closure drops its clone.
            flush_worker
                .join()
                .map_err(|_| eyre!("completion flush thread panicked"))?;
        }
        assert_eq!(
            file_label(&storage, 0)?,
            Some(ChunkType::Uninitialized),
            "a stale completion published Data"
        );
        assert!(
            !storage.is_data_chunk_persisted_at(offset),
            "persisted check trusts the pre-cut image"
        );
        let storage =
            Arc::try_unwrap(storage).map_err(|_| eyre!("flush still holds the module"))?;
        storage.abandon_on_drop_for_test();
        drop(storage);

        let reopened = reopen_crc_module(tmp.path(), &config)?;
        assert_eq!(
            reopened.get_chunk_type(&offset),
            Some(ChunkType::Uninitialized),
            "reopen reloaded Data"
        );
        assert_eq!(
            file_label(&reopened, 0)?,
            Some(ChunkType::Uninitialized),
            "reopen file is not Uninitialized"
        );
        assert_not_served_as_data(&reopened, offset)?;
        assert!(
            !reopened.is_data_root_index_ready_at(offset),
            "open kept the placement"
        );
        drop(reopened);
        drop(tmp);
        Ok(())
    }

    /// A failed `intervals.json` replace must not leave the live label
    /// `Uninitialized` with an empty repair set. The next note publishes the
    /// cut and queues the repair. The file stays on the previous label until
    /// that publish commits.
    #[rstest]
    #[case::period_0(0_u64)]
    #[case::period_30(30_u64)]
    fn failed_crc_publish_keeps_the_previous_label(#[case] persist_secs: u64) -> eyre::Result<()> {
        let prefix = format!("crc_publish_fail_{persist_secs}");
        let (tmp, _config, storage, ..) = settle_data_offset(&prefix, persist_secs)?;
        let offset = PartitionChunkOffset::from(0);
        let epoch_before = storage.crc_cut_epoch.load(Ordering::SeqCst);
        storage.fail_next_sync_at(SyncFailurePoint::BeforeIntervalImageCommit);
        let failed = storage
            .note_crc_mismatch(offset, ChunkType::Data, "test")
            .expect_err("the interval replace was injected to fail");
        assert!(
            format!("{failed:#}").contains("BeforeIntervalImageCommit"),
            "{failed:#}"
        );
        assert_eq!(
            live_label(&storage, 0),
            Some(ChunkType::Data),
            "a failed replace left the live label cut"
        );
        assert_eq!(
            file_label(&storage, 0)?,
            Some(ChunkType::Data),
            "a failed replace committed the cut"
        );
        assert!(
            storage.take_crc_repair_ranges().is_empty(),
            "a failed replace queued a repair"
        );
        assert_eq!(
            storage.crc_cut_epoch.load(Ordering::SeqCst),
            epoch_before,
            "a failed replace left the checksum-cut epoch bumped"
        );

        storage.note_crc_mismatch(offset, ChunkType::Data, "test")?;
        assert_eq!(live_label(&storage, 0), Some(ChunkType::Uninitialized));
        assert_eq!(
            file_label(&storage, 0)?,
            Some(ChunkType::Uninitialized),
            "the retry did not publish the cut"
        );
        assert_eq!(
            storage.take_crc_repair_ranges(),
            vec![partition_chunk_offset_ii!(0, 0)]
        );
        assert_eq!(
            storage.crc_cut_epoch.load(Ordering::SeqCst),
            epoch_before + 1,
            "the committed cut did not bump the checksum-cut epoch"
        );
        drop(storage);
        drop(tmp);
        Ok(())
    }

    /// Entropy uses the same publish. A failed replace restores Entropy, and
    /// the following note still cuts it.
    #[test]
    fn failed_crc_publish_keeps_an_entropy_label() -> eyre::Result<()> {
        let (tmp, _config, storage) = crc_hold_module("crc_publish_fail_entropy", 0)?;
        let offset = PartitionChunkOffset::from(0);
        assert!(storage.write_chunk(offset, vec![0xEE; 32], ChunkType::Entropy));
        storage.force_sync_pending_chunks()?;
        assert_eq!(live_label(&storage, 0), Some(ChunkType::Entropy));
        assert_eq!(file_label(&storage, 0)?, Some(ChunkType::Entropy));
        storage.fail_next_sync_at(SyncFailurePoint::BeforeIntervalImageCommit);
        let failed = storage
            .note_crc_mismatch(offset, ChunkType::Entropy, "test")
            .expect_err("the interval replace was injected to fail");
        assert!(
            format!("{failed:#}").contains("BeforeIntervalImageCommit"),
            "{failed:#}"
        );
        assert_eq!(live_label(&storage, 0), Some(ChunkType::Entropy));
        assert_eq!(file_label(&storage, 0)?, Some(ChunkType::Entropy));
        assert!(storage.take_crc_repair_ranges().is_empty());
        assert_eq!(storage.crc_cut_epoch.load(Ordering::SeqCst), 0);

        storage.note_crc_mismatch(offset, ChunkType::Entropy, "test")?;
        assert_eq!(live_label(&storage, 0), Some(ChunkType::Uninitialized));
        assert_eq!(file_label(&storage, 0)?, Some(ChunkType::Uninitialized));
        assert_eq!(
            storage.take_crc_repair_ranges(),
            vec![partition_chunk_offset_ii!(0, 0)]
        );
        drop(storage);
        drop(tmp);
        Ok(())
    }

    /// A completion cloned before a failed cut still has the pre-cut epoch.
    /// The failed replace must not make that completion skip. A later
    /// successful cut still wins over a completion cloned before it.
    #[test]
    fn failed_crc_publish_does_not_invalidate_a_cloned_completion() -> eyre::Result<()> {
        let (tmp, _config, storage, ..) = settle_data_offset("crc_publish_fail_completion", 30)?;
        let storage = Arc::new(storage);
        let offset = PartitionChunkOffset::from(0);
        {
            let mut disk = storage.lock_interval_disk();
            disk.dirty.insert(PartitionChunkOffset::from(0));
            disk.last_completion.clear();
        }
        storage.pause_next_completion_clone();
        let flush_mod = Arc::clone(&storage);
        let (tx, rx) = std::sync::mpsc::channel();
        let first_flush = std::thread::spawn(move || {
            let _ = tx.send(flush_mod.flush_interval_completions_locked(true));
        });
        assert!(
            storage.wait_for_interval_image_pause(Duration::from_secs(5)),
            "the completion did not clone"
        );
        storage.fail_next_sync_at(SyncFailurePoint::BeforeIntervalImageCommit);
        let failed = storage
            .note_crc_mismatch(offset, ChunkType::Data, "test")
            .expect_err("the interval replace was injected to fail");
        assert!(
            format!("{failed:#}").contains("BeforeIntervalImageCommit"),
            "{failed:#}"
        );
        assert_eq!(live_label(&storage, 0), Some(ChunkType::Data));
        assert_eq!(storage.crc_cut_epoch.load(Ordering::SeqCst), 0);
        storage.release_interval_image_pause();
        let flushed = rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|err| eyre!("completion flush did not finish: {err}"));
        // The worker sends before its closure drops its clone.
        first_flush
            .join()
            .map_err(|_| eyre!("completion flush thread panicked"))?;
        flushed??;
        assert_eq!(
            file_label(&storage, 0)?,
            Some(ChunkType::Data),
            "the completion skipped after a failed cut"
        );
        assert_eq!(live_label(&storage, 0), Some(ChunkType::Data));
        assert!(storage.take_crc_repair_ranges().is_empty());

        {
            let mut disk = storage.lock_interval_disk();
            disk.dirty.insert(PartitionChunkOffset::from(0));
            disk.last_completion.clear();
        }
        storage.pause_next_completion_clone();
        let flush_mod = Arc::clone(&storage);
        let (tx, rx) = std::sync::mpsc::channel();
        let second_flush = std::thread::spawn(move || {
            let _ = tx.send(flush_mod.flush_interval_completions_locked(true));
        });
        assert!(
            storage.wait_for_interval_image_pause(Duration::from_secs(5)),
            "the second completion did not clone"
        );
        storage.note_crc_mismatch(offset, ChunkType::Data, "test")?;
        storage.release_interval_image_pause();
        let flushed = rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|err| eyre!("second completion flush did not finish: {err}"));
        // The worker sends before its closure drops its clone.
        second_flush
            .join()
            .map_err(|_| eyre!("completion flush thread panicked"))?;
        flushed??;
        assert_eq!(
            file_label(&storage, 0)?,
            Some(ChunkType::Uninitialized),
            "a stale completion published Data over the committed cut"
        );
        assert_eq!(live_label(&storage, 0), Some(ChunkType::Uninitialized));
        assert_eq!(
            storage.take_crc_repair_ranges(),
            vec![partition_chunk_offset_ii!(0, 0)]
        );
        // Both joins finished above. Take the module back only after the clones drop.
        let storage =
            Arc::try_unwrap(storage).map_err(|_| eyre!("a flush worker still holds the module"))?;
        drop(storage);
        drop(tmp);
        Ok(())
    }

    /// A confirmed mismatch rewrites the interval file before it returns.
    /// A crash with no later rewrite reloads `Uninitialized`. Open drops the
    /// presence row and the placement. Period 0 and period 30 both rewrite
    /// here: the completion flush does not, because the cut is not a dirty
    /// completion.
    #[rstest]
    #[case::period_0(0_u64)]
    #[case::period_30(30_u64)]
    fn crc_cut_survives_crash_before_the_next_interval_rewrite(
        #[case] persist_secs: u64,
    ) -> eyre::Result<()> {
        let prefix = format!("crc_cut_durable_{persist_secs}");
        let (tmp, config, storage, ..) = settle_data_offset(&prefix, persist_secs)?;
        let offset = PartitionChunkOffset::from(0);
        storage.note_crc_mismatch(offset, ChunkType::Data, "test")?;
        assert_eq!(
            live_label(&storage, 0),
            Some(ChunkType::Uninitialized),
            "the live map was not cut"
        );
        assert_eq!(
            file_label(&storage, 0)?,
            Some(ChunkType::Uninitialized),
            "the checksum cut left intervals.json as Data"
        );
        assert!(
            !storage.is_data_chunk_persisted_at(offset),
            "persisted check still trusts the pre-cut file"
        );
        storage.abandon_on_drop_for_test();
        drop(storage);

        let reopened = reopen_crc_module(tmp.path(), &config)?;
        assert_eq!(
            reopened.get_chunk_type(&offset),
            Some(ChunkType::Uninitialized),
            "reopen reloaded Data"
        );
        assert_eq!(
            file_label(&reopened, 0)?,
            Some(ChunkType::Uninitialized),
            "reopen file is not Uninitialized"
        );
        assert_not_served_as_data(&reopened, offset)?;
        assert!(
            !reopened.is_data_root_index_ready_at(offset),
            "open kept the placement"
        );
        assert!(
            reopened.take_crc_repair_ranges().is_empty(),
            "the repair queue survived the crash"
        );
        drop(reopened);
        drop(tmp);
        Ok(())
    }

    /// A mismatch publish, then a crash, reloads `Uninitialized`. Open drops
    /// the placement. Indexing the same tx puts the placement back. The bytes
    /// stay unserved until a later refill.
    #[rstest]
    #[case::period_0(0_u64)]
    #[case::period_30(30_u64)]
    fn crc_crash_publish_reloads_uninitialized(#[case] persist_secs: u64) -> eyre::Result<()> {
        let prefix = format!("crc_crash_publish_{persist_secs}");
        let (tmp, config, storage, _unpacked, header, tx_path) =
            settle_data_offset(&prefix, persist_secs)?;
        let offset = PartitionChunkOffset::from(0);
        storage.note_crc_mismatch(offset, ChunkType::Data, "test")?;
        storage.write_intervals_to_submodules()?;
        assert_ne!(
            file_label(&storage, 0)?,
            Some(ChunkType::Data),
            "the interval publish kept Data for a checksum mismatch"
        );
        storage.abandon_on_drop_for_test();
        drop(storage);

        let reopened = reopen_crc_module(tmp.path(), &config)?;
        assert_not_served_as_data(&reopened, offset)?;
        assert!(
            !reopened.is_data_root_index_ready_at(offset),
            "open kept the placement under the mismatch"
        );
        reopened.index_transaction_data(
            &header,
            &tx_path,
            LedgerChunkRange(ledger_chunk_offset_ii!(0, 3)),
            1,
        )?;
        assert!(
            reopened.is_data_root_index_ready_at(offset),
            "re-index did not restore the placement"
        );
        assert!(!reopened.is_data_chunk_durable_at(offset));
        drop(reopened);
        drop(tmp);
        Ok(())
    }

    /// Entropy repair commits, then the process dies before the completion
    /// rewrite. The file stays `Interrupted`. Open loads that as
    /// `Uninitialized` and drops the placement. Period 0 closes the window
    /// before `sync` returns, so the test stops at the interval commit.
    #[rstest]
    #[case::period_0(0_u64)]
    #[case::period_30(30_u64)]
    fn crc_crash_repair_before_completion_reloads_uninitialized(
        #[case] persist_secs: u64,
    ) -> eyre::Result<()> {
        let prefix = format!("crc_crash_repair_{persist_secs}");
        let (tmp, config, storage, ..) = settle_data_offset(&prefix, persist_secs)?;
        let offset = PartitionChunkOffset::from(0);
        storage.note_crc_mismatch(offset, ChunkType::Data, "test")?;
        assert!(storage.write_chunk(offset, vec![0xEE; 32], ChunkType::Entropy));
        storage.fail_next_sync_at(SyncFailurePoint::BeforeIntervalCommit);
        let err = storage.sync_pending_chunks().unwrap_err();
        assert!(err.to_string().contains("BeforeIntervalCommit"), "{err}");
        assert_ne!(
            file_label(&storage, 0)?,
            Some(ChunkType::Data),
            "write-ahead kept Data across the entropy repair"
        );
        storage.abandon_on_drop_for_test();
        drop(storage);

        let reopened = reopen_crc_module(tmp.path(), &config)?;
        assert_eq!(
            reopened.get_chunk_type(&offset),
            Some(ChunkType::Uninitialized),
            "open did not convert Interrupted"
        );
        assert_eq!(
            file_label(&reopened, 0)?,
            Some(ChunkType::Interrupted),
            "the completion rewrite ran before the crash"
        );
        assert_not_served_as_data(&reopened, offset)?;
        assert!(
            !reopened.is_data_root_index_ready_at(offset),
            "open kept the placement"
        );
        drop(reopened);
        drop(tmp);
        Ok(())
    }

    /// Rollback clears the index, marks the range `Uninitialized`, and publishes.
    /// A crash after that publish must not reload `Data`.
    #[rstest]
    #[case::period_0(0_u64)]
    #[case::period_30(30_u64)]
    fn crc_crash_rollback_publish_reloads_uninitialized(
        #[case] persist_secs: u64,
    ) -> eyre::Result<()> {
        let prefix = format!("crc_crash_rollback_{persist_secs}");
        let (tmp, config, storage, ..) = settle_data_offset(&prefix, persist_secs)?;
        let offset = PartitionChunkOffset::from(0);
        storage.note_crc_mismatch(offset, ChunkType::Data, "test")?;
        storage.drop_pending_writes_in_range(offset, offset);
        storage.clear_offset_index_in_range(offset, offset)?;
        {
            let mut intervals = storage.intervals.write().unwrap();
            StorageModule::cut_then_insert_interval_if_touching(
                &mut intervals,
                offset,
                ChunkType::Uninitialized,
            );
        }
        storage.write_intervals_to_submodules()?;
        assert_ne!(
            file_label(&storage, 0)?,
            Some(ChunkType::Data),
            "rollback publish restored Data over the cleared index"
        );
        storage.abandon_on_drop_for_test();
        drop(storage);

        let reopened = reopen_crc_module(tmp.path(), &config)?;
        assert_not_served_as_data(&reopened, offset)?;
        assert!(!reopened.is_data_root_index_ready_at(offset));
        drop(reopened);
        drop(tmp);
        Ok(())
    }

    /// Mismatch, repack, refill, then a clean shutdown. The offset is durable
    /// `Data` and the stored checksum matches the bytes.
    #[rstest]
    #[case::period_0(0_u64)]
    #[case::period_30(30_u64)]
    fn crc_crash_repack_refill_is_durable_data(#[case] persist_secs: u64) -> eyre::Result<()> {
        let prefix = format!("crc_crash_refill_{persist_secs}");
        let (tmp, config, storage, unpacked, ..) = settle_data_offset(&prefix, persist_secs)?;
        let offset = PartitionChunkOffset::from(0);
        storage.note_crc_mismatch(offset, ChunkType::Data, "test")?;
        assert!(storage.write_chunk(offset, vec![0xEE; 32], ChunkType::Entropy));
        storage.sync_pending_chunks()?;
        assert_eq!(live_label(&storage, 0), Some(ChunkType::Entropy));
        storage.write_data_chunk(&unpacked[0])?;
        storage.sync_pending_chunks()?;
        assert!(storage.is_data_chunk_durable_at(offset), "refilled durable");
        let loaded = storage.read_chunks(partition_chunk_offset_ii!(0, 0))?;
        let bytes = &loaded.get(&offset).expect("refilled bytes").0;
        let row = storage.offset_meta_for_test(offset)?.expect("refilled row");
        assert_eq!(row.crc, Some(chunk_crc::checksum(bytes)));
        assert!(row.proof.is_some(), "refill stored a proof");
        assert!(storage.generate_full_chunk(offset)?.is_some());
        drop(storage);

        let reopened = reopen_crc_module(tmp.path(), &config)?;
        assert_eq!(
            reopened.get_chunk_type(&offset),
            Some(ChunkType::Data),
            "clean shutdown reloaded a different label"
        );
        assert_eq!(
            file_label(&reopened, 0)?,
            Some(ChunkType::Data),
            "clean shutdown left a different file label"
        );
        assert!(reopened.is_data_chunk_durable_at(offset));
        let loaded = reopened.read_chunks(partition_chunk_offset_ii!(0, 0))?;
        let bytes = &loaded.get(&offset).expect("reopened bytes").0;
        let row = reopened
            .offset_meta_for_test(offset)?
            .expect("reopened row");
        assert_eq!(row.crc, Some(chunk_crc::checksum(bytes)));
        assert!(reopened.generate_full_chunk(offset)?.is_some());
        drop(reopened);
        drop(tmp);
        Ok(())
    }

    fn capacity_assignment() -> irys_types::partition::PartitionAssignment {
        irys_types::partition::PartitionAssignment {
            ledger_id: None,
            slot_index: None,
            miner_address: irys_types::IrysAddress::from([0xAA; 20]),
            partition_hash: H256::repeat_byte(0x11),
        }
    }

    /// Entropy is on disk and the placement is still indexed. Ingress packs it.
    #[test]
    fn occupy_packs_entropy_when_the_placement_remains() -> eyre::Result<()> {
        let (_tmp, _config, storage) = crc_hold_module("occupy_keeps_placement", 0)?;
        storage.pack_with_zeros();
        let (unpacked, ..) = index_partition(&storage)?;
        let offset = PartitionChunkOffset::from(0);
        assert!(storage.write_chunk(offset, vec![0x22; 32], ChunkType::Entropy));
        storage.sync_pending_chunks()?;
        storage.write_data_chunk(&unpacked[0])?;
        let pending = storage.pending_writes.read().unwrap();
        let (bytes, kind) = pending.get(&offset).expect("packed data");
        assert_eq!(*kind, ChunkType::Data);
        // Chunk 0 of this tx is zeros, so the packed byte is the entropy byte.
        assert_eq!(bytes.as_slice(), &[0x22; 32]);
        Ok(())
    }

    /// Placements were resolved, then reset, then entropy was queued. The
    /// interval is still `Uninitialized`. That occupy must not queue `Data`.
    #[test]
    fn occupy_after_reset_does_not_pack_pending_entropy() -> eyre::Result<()> {
        let (_tmp, _config, storage, unpacked, ..) = settle_data_offset("occupy_reset_pending", 0)?;
        let offset = PartitionChunkOffset::from(0);
        let hash = H256::repeat_byte(0x11);
        storage.pause_next_sync_at(SyncPausePoint::AfterOccupyResolved);
        let storage_ref = &storage;
        let pending_kind = std::thread::scope(|scope| {
            let write = scope.spawn(move || storage_ref.write_data_chunk(&unpacked[0]));
            assert!(
                storage_ref.wait_for_sync_pause(Duration::from_secs(5)),
                "occupy did not pause after resolving placements"
            );
            storage_ref
                .reset_with_assignment(Some(capacity_assignment()), 1)
                .expect("reset");
            assert_eq!(
                storage_ref.write_entropy_chunk(offset, vec![0x22; 32], hash),
                EntropyWrite::Queued
            );
            assert_eq!(
                live_label(storage_ref, 0),
                Some(ChunkType::Uninitialized),
                "entropy is queued, not yet written"
            );
            storage_ref.release_sync_pause();
            let wrote = write.join().expect("occupy thread");
            let pending_kind = storage_ref
                .pending_writes
                .read()
                .unwrap()
                .get(&offset)
                .map(|(_, kind)| *kind);
            assert_ne!(
                pending_kind,
                Some(ChunkType::Data),
                "occupy committed the pre-reset placement onto pending entropy: {wrote:?}"
            );
            pending_kind
        });
        assert_eq!(pending_kind, Some(ChunkType::Entropy));
        Ok(())
    }

    /// The same occupy, after the post-reset entropy is already durable.
    /// The index is gone. The occupy must not pack over that entropy.
    #[test]
    fn occupy_after_reset_does_not_pack_synced_entropy() -> eyre::Result<()> {
        let (_tmp, _config, storage, unpacked, ..) = settle_data_offset("occupy_reset_synced", 0)?;
        let offset = PartitionChunkOffset::from(0);
        let hash = H256::repeat_byte(0x11);
        storage.pause_next_sync_at(SyncPausePoint::AfterOccupyResolved);
        let storage_ref = &storage;
        std::thread::scope(|scope| {
            let write = scope.spawn(move || storage_ref.write_data_chunk(&unpacked[0]));
            assert!(
                storage_ref.wait_for_sync_pause(Duration::from_secs(5)),
                "occupy did not pause after resolving placements"
            );
            storage_ref
                .reset_with_assignment(Some(capacity_assignment()), 1)
                .expect("reset");
            assert_eq!(
                storage_ref.write_entropy_chunk(offset, vec![0x22; 32], hash),
                EntropyWrite::Queued
            );
            storage_ref.sync_pending_chunks().expect("entropy sync");
            assert_eq!(live_label(storage_ref, 0), Some(ChunkType::Entropy));
            assert!(
                storage_ref
                    .pending_writes
                    .read()
                    .unwrap()
                    .get(&offset)
                    .is_none(),
                "synced entropy is still pending"
            );
            storage_ref.release_sync_pause();
            let wrote = write.join().expect("occupy thread");
            let pending_kind = storage_ref
                .pending_writes
                .read()
                .unwrap()
                .get(&offset)
                .map(|(_, kind)| *kind);
            assert_ne!(
                pending_kind,
                Some(ChunkType::Data),
                "occupy committed the pre-reset placement onto synced entropy: {wrote:?}"
            );
            assert_eq!(live_label(storage_ref, 0), Some(ChunkType::Entropy));
            Ok(())
        })
    }
}
