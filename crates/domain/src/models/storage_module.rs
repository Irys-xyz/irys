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

use atomic_write_file::AtomicWriteFile;
use derive_more::derive::{Deref, DerefMut};
use eyre::{Context as _, OptionExt as _, Result, ensure, eyre};
use irys_database::{
    db::IrysDatabaseExt as _,
    submodule::{
        add_data_path_hash_to_offset_index, add_data_root_info, add_full_data_path,
        add_full_tx_path, add_tx_path_hash_to_offset_index, clear_submodule_database,
        create_or_open_submodule_db, get_data_path_by_offset, get_data_root_infos_for_data_root,
        get_tx_path_by_offset,
        tables::{DataRootInfo, DataRootInfos},
    },
};
use irys_packing::{capacity_single::compute_entropy_chunk, packing_xor_vec_u8};
use irys_types::{
    Base64, ChunkBytes, ChunkDataPath, ChunkPathHash, Config, DataLedger, DataRoot,
    DataTransactionHeader, H256, IrysAddress, LedgerChunkOffset, LedgerChunkRange, PackedChunk,
    PartitionChunkOffset, PartitionChunkRange, ProofDeserialize as _, RelativeChunkOffset,
    TxChunkOffset, TxPath, UnpackedChunk,
    app_state::DatabaseProvider,
    get_leaf_proof, ledger_chunk_offset_ie,
    partition::{PartitionAssignment, PartitionHash},
    partition_chunk_offset_ii,
};
use nodit::{InclusiveInterval as _, Interval, NoditMap, NoditSet, interval::ii};
use openssl::sha;
use reth_db::Database as _;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read as _, Seek as _, SeekFrom, Write as _},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};
use tracing::{debug, error, info, warn};

use crate::{CircularBuffer, StorageModulesReadGuard};

type SubmodulePath = PathBuf;

// In-memory chunk data indexed by offset within partition
type ChunkMap = BTreeMap<PartitionChunkOffset, (ChunkBytes, ChunkType)>;

/// Storage submodules mapped to their chunk ranges
type SubmoduleMap =
    NoditMap<PartitionChunkOffset, Interval<PartitionChunkOffset>, StorageSubmodule>;

/// Tracks storage state of chunk ranges across all submodules
type StorageIntervals = NoditMap<PartitionChunkOffset, Interval<PartitionChunkOffset>, ChunkType>;

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
    /// In-memory chunk buffer awaiting disk write
    pending_writes: Arc<RwLock<ChunkMap>>,
    /// Tracks the wall clock time of the last pending write
    last_pending_write: RwLock<Instant>,
    /// Tracks the storage state of each chunk across all submodules
    intervals: Arc<RwLock<StorageIntervals>>,
    /// Physical storage locations indexed by chunk ranges
    submodules: SubmoduleMap,
    /// Track the speed/throughput of recent disk writes
    recent_chunk_times: Arc<RwLock<CircularBuffer<ChunkTimeRecord>>>,
    /// Runtime configuration parameters
    pub config: Config,
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
}

pub fn get_atomic_file<P: AsRef<Path> + std::fmt::Debug>(path: P) -> eyre::Result<AtomicWriteFile> {
    AtomicWriteFile::options()
        .read(true)
        .open(&path)
        .wrap_err_with(|| format!("Failed to create or open atomic file for {:?}", &path))
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

impl StorageModule {
    /// Initializes a new StorageModule
    pub fn new(storage_module_info: &StorageModuleInfo, config: &Config) -> eyre::Result<Self> {
        let mut submodule_map = NoditMap::new();
        let mut global_intervals = StorageIntervals::new();

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
            let submodule_db = create_or_open_submodule_db(&submodule_db_path).map_err(|e| {
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
            submodule_map
                .insert_strict(
                    submodule_interval,
                    StorageSubmodule {
                        path: dir,
                        file: chunks_file,
                        db: DatabaseProvider(Arc::new(submodule_db)),
                        intervals_file: Arc::new(Mutex::new(submodules_intervals_file)),
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

        Ok(Self {
            id: storage_module_info.id,
            partition_assignment: RwLock::new(storage_module_info.partition_assignment),
            pending_writes: Arc::new(RwLock::new(ChunkMap::new())),
            last_pending_write: RwLock::new(Instant::now()),
            intervals: Arc::new(RwLock::new(loaded_intervals)),
            submodules: submodule_map,
            recent_chunk_times: Arc::new(RwLock::new(CircularBuffer::new(8_000))), // sample window 10s = 10s x 800 chunks/s = capacity 8_000
            config: config.clone(),
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

    /// Reinit intervals setting them as Uninitialized, and erase db
    pub fn reset(&self) -> eyre::Result<Interval<PartitionChunkOffset>> {
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

    /// Force syncs (writes) all pending writes for this SM
    ///
    /// Process:
    /// 1. Collects pending writes for each submodule
    /// 2. Acquires write lock only if some pending writes exist
    /// 3. Writes chunks to disk and removes them from pending queue
    ///
    pub fn force_sync_pending_chunks(&self) -> eyre::Result<()> {
        self.sync_pending_chunks_inner(true)
    }

    fn sync_pending_chunks_inner(&self, force: bool) -> eyre::Result<()> {
        // TODO: rework this function
        // 1.) use batches for fsync, instead of the all the pending writes (reduces impact of write errors)
        // 2.) pending writes are per-sm, we should delegate flushing to the StorageSubmodule
        // doing this removes having to locate the correct submodule again in `write_chunk_internal`,
        // and lets us increase throughput - we can spawn blocking tasks in parallel for each submodule (as each is it's own IO domain/drive)

        let then = Instant::now();
        let threshold = if force {
            0
        } else {
            self.config.node_config.storage.num_writes_before_sync
        };

        let arc = self.pending_writes.clone();

        // First use read lock to check if we have work to do
        let write_batch = {
            let pending = arc.read().unwrap();

            let pending_writes = self
                .submodules
                .iter()
                .flat_map(|(interval, _submodule)| {
                    let submodule_writes: Vec<_> = pending
                        .iter()
                        .filter(|(offset, _)| interval.contains_point(**offset))
                        .map(|(offset, state)| (*offset, state.clone()))
                        .collect();

                    // each submodule is it's own "IO domain", so we don't process pending writes for one if it's queue is too small
                    if submodule_writes.len() as u64 >= threshold {
                        submodule_writes
                    } else {
                        Vec::new()
                    }
                })
                .collect::<Vec<_>>();

            drop(pending);
            pending_writes
        };

        if write_batch.is_empty() {
            return Ok(());
        }

        let mut intervals = self
            .intervals
            .write()
            .map_err(|e| eyre::eyre!("Failed to acquire write lock on intervals: {}", e))?;

        // write every offset to the intervals file as Interrupted
        // we sync the correct intervals state once the entire batch is written
        for (chunk_offset, _) in write_batch.iter() {
            Self::cut_then_insert_interval_if_touching(
                &mut intervals,
                *chunk_offset,
                ChunkType::Interrupted,
            );
        }

        drop(intervals);

        self.write_intervals_to_submodules()
            .wrap_err("Could not update submodule interval files, if this is a component test with storage_module that drops after the test, this error is benign")?;

        let len = write_batch.len();

        let mut pending = arc.write().unwrap();
        for (chunk_offset, (bytes, chunk_type)) in write_batch {
            // self.intervals are updated by write_chunk_internal()
            match self.write_chunk_internal(chunk_offset, bytes, chunk_type) {
                Ok(_) => {}
                Err(e) => {
                    // persist state

                    // make sure fsync succeeds
                    for (_, submodule) in self.submodules.iter() {
                        let file = submodule.file.lock().unwrap();
                        file.sync_all()
                            .map_err(|e| eyre::eyre!("Failed to sync data to disk: {}", e))?;
                    }

                    // we don't remove the failed write from pending - we can retry it
                    self.write_intervals_to_submodules()?;
                    return Err(e);
                }
            }
            pending.remove(&chunk_offset); // Clean up written chunks
        }
        drop(pending);

        // fsync this write batch BEFORE committing the interval state
        // as fsync can error on us if the underlying storage has issues
        for (_, submodule) in self.submodules.iter() {
            let file = submodule.file.lock().unwrap();
            // Ensure data is flushed to disk prior to drop
            file.sync_all()
                .map_err(|e| eyre::eyre!("Failed to sync data to disk: {}", e))?;
        }

        // persist the state from all the write calls
        // save the updated intervals
        self.write_intervals_to_submodules()
            .wrap_err("Could not update submodule interval files, if this is a component test with storage_module that drops after the test, this error is benign")?;

        debug!(
            "sync_pending_chunks took {:.3}s for {} chunks",
            &then.elapsed().as_secs_f64(),
            &len
        );

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
    fn write_intervals_to_submodules(&self) -> eyre::Result<()> {
        let intervals = self.intervals.read().unwrap();

        // Loop though each of the submodule ranges
        for (submodule_interval, submodule) in self.submodules.iter() {
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
                    _ => chunk_type,
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
        let mut chunk_map = ChunkMap::new();

        // Query overlapping intervals from storage map
        let intervals = self.intervals.read().unwrap();
        let intervals2 = intervals.clone();
        // this is here to prevent a deadlock with the later `pending_writes.read()` rwlock acquisition
        // this locking order (intervals -> pending_writes) is reversed in sync_pending_chunks (pending_writes -> intervals), which is the cause of the deadlock.
        drop(intervals);
        // TODO: figure out how to only clone the overlap instead of the entire interval map (low prio as the intervals should be small generally speaking)
        let overlapping = intervals2.overlapping(chunk_range);

        // Process each overlapping interval
        for (interval, interval_chunk_type) in overlapping {
            // Get intersection with requested range
            let start = *chunk_range.start().max(interval.start());
            let end = *chunk_range.end().min(interval.end());

            // Process each chunk in the clipped range
            for chunk_offset in start..=end {
                let partition_chunk_offset = PartitionChunkOffset::from(chunk_offset);

                // Check for pending writes first
                let pending = self.pending_writes.read().unwrap();
                let pending_chunk = pending
                    .get(&partition_chunk_offset)
                    .map(|(bytes, chunk_type)| (bytes.clone(), *chunk_type));
                drop(pending);

                // Use pending chunk if available, otherwise use storage based on chunk type
                match (pending_chunk, interval_chunk_type) {
                    // Case 1: We have a pending chunk - use it regardless of interval_chunk_type
                    (Some(chunk_data), _) => {
                        chunk_map.insert(partition_chunk_offset, chunk_data);
                    }

                    // Case 2: No pending chunk, uninitialized interval_chunk_type - skip
                    (None, ChunkType::Uninitialized) => continue,

                    // Case 3: No pending chunk, Data or Entropy interval_chunk_type - read from storage
                    (None, _) => {
                        let bytes = self.read_chunk_internal(partition_chunk_offset)?;
                        chunk_map.insert(partition_chunk_offset, (bytes, *interval_chunk_type));
                    }
                }
            }
        }

        Ok(chunk_map)
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

        // Read chunk from file
        let mut file = submodule.file.lock().unwrap();
        file.seek(SeekFrom::Start(file_offset))?;
        file.read_exact(&mut buf)?;

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
                for (offset, _) in pending.iter() {
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
    ) {
        let mut pending = self.pending_writes.write().unwrap();
        pending.insert(chunk_offset, (bytes, chunk_type));
        *self.last_pending_write.write().unwrap() = Instant::now();
        drop(pending);
    }

    /// Test utility function
    pub fn print_pending_writes(&self) {
        let pending = self.pending_writes.read().unwrap();
        debug!("pending_writes: {:?}", pending);
    }

    /// Indexes transaction data by mapping chunks to transaction paths across storage submodules.
    /// Stores three mappings: tx path hashes -> tx_path, chunk offsets -> tx paths, and data roots -> start offset.
    /// Updates all overlapping submodules within the given chunk range.
    ///
    /// # Errors
    /// Returns error if chunk range doesn't overlap with storage module range.
    pub fn index_transaction_data(
        &self,
        data_tx: &DataTransactionHeader,
        tx_path: &TxPath,
        chunk_range: LedgerChunkRange,
    ) -> eyre::Result<()> {
        let storage_range = self.get_storage_module_ledger_offsets()?;
        let tx_path_hash = H256::from(hash_sha256(tx_path).unwrap());

        let overlap = storage_range
            .intersection(&chunk_range)
            .ok_or_else(|| eyre::eyre!("chunk_range does not overlap storage module range"))?;

        // Compute the partition relative overlapping chunk range
        let partition_overlap = self.make_range_partition_relative(overlap)?;
        // Compute the Partition relative start offset
        let start_offset =
            RelativeChunkOffset::from(self.make_offset_partition_relative(chunk_range.start())?);

        for (interval, submodule) in self.submodules.overlapping(partition_overlap) {
            submodule.db.update_eyre(|tx| -> eyre::Result<()> {
                // Because each submodule index receives a copy of the path, we need to clone it
                add_full_tx_path(tx, tx_path_hash, tx_path.clone())?;
                if let Some(range) = interval.intersection(&partition_overlap) {
                    // Add the tx_path_hash to every offset in the intersecting range
                    for offset in *range.start()..=*range.end() {
                        let part_offset = PartitionChunkOffset::from(offset);
                        add_tx_path_hash_to_offset_index(tx, part_offset, Some(tx_path_hash))?;
                    }
                    // Add the DataRootInfo to the Infos for this data_root
                    let info = DataRootInfo {
                        start_offset,
                        data_size: data_tx.data_size,
                    };
                    add_data_root_info(tx, data_tx.data_root, &info)?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    /// Stores the data_path and offset lookups in the correct submodule index
    pub fn add_data_path_to_index(
        &self,
        data_path_hash: ChunkPathHash,
        data_path: ChunkDataPath,
        partition_offset: PartitionChunkOffset,
    ) -> eyre::Result<()> {
        // Find submodule containing this chunk
        let res = self.submodules.get_key_value_at_point(partition_offset);

        if let Ok((_interval, submodule)) = res {
            submodule.db.update_eyre(|tx| -> eyre::Result<()> {
                add_full_data_path(tx, data_path_hash, data_path)?;
                add_data_path_hash_to_offset_index(tx, partition_offset, Some(data_path_hash))?;
                Ok(())
            })?;
            Ok(())
        } else {
            Err(eyre::eyre!(
                "No submodule found for Partition Offset {:?}",
                partition_offset
            ))
        }
    }

    pub fn get_writeable_offsets(
        &self,
        chunk: &UnpackedChunk,
    ) -> eyre::Result<Vec<PartitionChunkOffset>> {
        let data_root_infos = self.collect_data_root_infos(chunk.data_root)?;

        if data_root_infos.0.is_empty() {
            debug!("Chunks data_root not found in storage module");
            return Ok(Vec::new());
        }

        let intervals = self.intervals.read().unwrap();
        let chunk_size = self.config.consensus.chunk_size;
        let num_chunks_in_partition = self.config.consensus.num_chunks_in_partition;

        // The data_root_infos contain all the start_offsets and data_sizes for this chunks data_root
        // what we need to do is find all the ones where the chunks tx_offset fits within the data_size
        // for the data_root at that offset.
        Ok(data_root_infos
            .0
            .iter()
            .filter_map(|info| {
                // Check if the chunk's tx_offset + start offset exceeds the data_size for the data root
                let tx_offset: u32 = chunk.tx_offset.into();
                let chunk_byte_offset: u64 = tx_offset as u64 * chunk_size;

                if chunk_byte_offset >= info.data_size {
                    // Skip any chunks that go past the data_size (don't error, just filter out)
                    warn!("Skipping chunk with tx_offset {:?} for data_root {:?} with start_offset {:?} because the tx_offset is larger than the data_size {:?}",tx_offset, chunk.data_root, info.start_offset, info.data_size );
                    None
                } else {
                    // Calculate the relative partition offset, this can sometimes be negative if the
                    // data_root overlaps two partitions
                    let relative_offset = info.start_offset + tx_offset as i32;

                    // Only include a writeable offset for a partition if it has a >= 0
                    // partition relative offset
                    if relative_offset.0 >= 0
                        && (relative_offset.0 as u64) < num_chunks_in_partition
                    {
                        // Convert the partition relative offset to a proper PartitionChunkOffset
                        let partition_offset = PartitionChunkOffset::from(relative_offset.0 as u32);

                        // Check if this offset is in an Entropy interval
                        if intervals
                            .get_at_point(partition_offset)
                            .is_some_and(|s| *s == ChunkType::Entropy)
                        {
                            Some(partition_offset)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
            })
            .collect())
    }

    /// Writes chunk data and its data_path to relevant storage locations
    pub fn write_data_chunk(&self, chunk: &UnpackedChunk) -> eyre::Result<()> {
        let data_path = &chunk.data_path.0;
        let data_path_hash = UnpackedChunk::hash_data_path(data_path);

        // Get all the places this chunks data_root starts in the partition
        let data_root_infos = self.collect_data_root_infos(chunk.data_root)?;

        if data_root_infos.0.is_empty() {
            return Err(eyre::eyre!("Chunks data_root not found in storage module"));
        }

        // Lists for both types of offsets to process
        let mut writeable_offsets = vec![];
        let mut pending_offsets = vec![];

        // Scan all potential locations and categorize them
        for info in data_root_infos.0 {
            let partition_offset =
                PartitionChunkOffset::from(info.start_offset + (*chunk.tx_offset as i32));

            // Calculate the number of chunks the data tx paid for
            let data_size_in_chunks =
                (info.data_size.div_ceil(self.config.consensus.chunk_size)) as i32;

            // Calculate the last valid chunk offset for this data transaction.
            // Convert data_size (bytes) to chunk count, then add to start_offset to get the
            // exclusive end boundary. Subtract 1 to convert from count to the inclusive
            // last offset.
            // Example: 3-chunk tx starting at offset 5 reserves offsets [5,6,7]
            //   → start=5, chunks=3, end_boundary=8, last_offset=7
            let last_chunk_offset = (info.start_offset.0 + data_size_in_chunks)
                .saturating_sub(1)
                .into();

            // Check to see if the offset being written is past the end of what was paid for
            if partition_offset > last_chunk_offset {
                // Skips writing to any partition chunk offsets that exceed the data_size
                // amount of chunks paid for by the data transaction
                continue;
            }

            // Check if there's an entropy chunk in the intervals map at this location and collect if present
            let intervals = self.intervals.read().unwrap();
            if intervals
                .get_at_point(partition_offset)
                .is_some_and(|s| *s == ChunkType::Entropy)
            {
                writeable_offsets.push(partition_offset);
                continue;
            }
            drop(intervals);

            // Check and collect offsets with pending entropy chunks
            let pending = self.pending_writes.read().unwrap();
            if pending
                .get(&partition_offset)
                .is_some_and(|(_, chunk_type)| *chunk_type == ChunkType::Entropy)
            {
                pending_offsets.push(partition_offset);
            }
        }

        // Process chunk offsets with entropy in pending writes list
        for partition_offset in pending_offsets {
            let mut pending = self.pending_writes.write().unwrap();

            match pending.get(&partition_offset) {
                Some((entropy_bytes, ChunkType::Entropy)) => {
                    // Pack the data with entropy and update pending
                    let packed_data = packing_xor_vec_u8(entropy_bytes.clone(), &chunk.bytes.0);
                    // Update the existing pending Entropy write to be a Data write
                    pending.insert(partition_offset, (packed_data, ChunkType::Data));
                    self.add_data_path_to_index(
                        data_path_hash,
                        data_path.clone(),
                        partition_offset,
                    )?;

                    *self.last_pending_write.write().unwrap() = Instant::now();
                }

                _ => continue,
            }
        }

        // Process all locations with entropy on disk in storage module
        for partition_offset in writeable_offsets {
            // read entropy from the storage module
            let entropy = self.read_chunk_internal(partition_offset)?;

            // xor is commutative, so we can avoid a clone of the chunk's data and use the entropy as the mutable component
            // (this also handles cases where the chunk's data isn't the full size, as the entropy will be)
            let packed_data = packing_xor_vec_u8(entropy, &chunk.bytes.0);

            self.write_chunk(partition_offset, packed_data, ChunkType::Data);
            self.add_data_path_to_index(data_path_hash, data_path.clone(), partition_offset)?;
        }

        Ok(())
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
            if let Ok(Some(submodule_index)) = submodule
                .db
                .view(|tx| get_data_root_infos_for_data_root(tx, data_root))?
            {
                data_root_info_list.0.extend(submodule_index.0);
            }
        }
        Ok(data_root_info_list)
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
        // Get paths and process them
        let Some((data_root, data_size, data_path, chunk_offset)) =
            self.query_submodule_db_by_offset(partition_offset, |tx| {
                let tx_path = match get_tx_path_by_offset(tx, partition_offset) {
                    Ok(Some(tx_path)) => tx_path,
                    Ok(None) => return Ok(None),
                    Err(err) => return Err(eyre::eyre!("Database read should succeed: {:?}", err)),
                };

                // Extract the data_root form the tx_path leaf node
                let path_buff = Base64::from(tx_path);
                let proof = get_leaf_proof(&path_buff)?;
                let data_root = proof
                    .hash()
                    .map(H256::from)
                    .ok_or_eyre("Unable to parse data_root from tx_path")?;

                // Retrieve all DataRootInfo entries for this data_root from the database.
                // Each entry contains a start_offset and the data_size paid for in the tx that provided the data_root
                let mut data_root_infos = get_data_root_infos_for_data_root(tx, data_root)
                    .expect("Database read should succeed")
                    .expect("there should be at least one start_offset for any data_root stored in the submodule");

                // Sort DataRootInfo entries by start_offset to enable binary search.
                // Multiple entries can exist for the same data_root it is posted by multiple transactions
                // but each will have a unique start_offset in the storage module
                data_root_infos.0.sort_unstable();

                // Binary search to find the DataRootInfo entry that contains our partition_offset.
                // partition_point returns the index of the first element where start_offset >= partition_offset
                // (which we -1 from to find the last element with a start_offset < then partition offset)
                // which means our target DataRootInfo is at this index (or doesn't exist if index is out of bounds).
                let index = data_root_infos
                    .0
                    .partition_point(|info| info.start_offset <= partition_offset.into()).saturating_sub(1);

                // Extract the data_size from the located DataRootInfo.
                // If the index is valid, we've found the correct funding transaction's data_size.
                // If not, the partition_offset doesn't belong to any known DataRootInfo entry.
                let data_size = if index < data_root_infos.0.len() {
                    data_root_infos.0[index].data_size
                } else {
                    return Err(eyre!("could not find DataRootInfo for partition_offset"));
                };

                // Storage behavior for multi-chunk transactions:
                // - First chunk write creates both tx_path and its specific data_path
                // - Subsequent chunks only create their own data_paths under the existing tx_path
                // - Reading an unwritten chunk returns None (tx_path exists, but chunk's data_path doesn't)
                let data_path =  match  get_data_path_by_offset(tx, partition_offset) {
                    Ok(Some(data_path)) => data_path,
                    Ok(None) => return Ok(None),
                    Err(err) => return Err(eyre::eyre!("Database read should succeed: {:?}", err)),
                };

                let path_buff = Base64::from(data_path);
                let proof = get_leaf_proof(&path_buff)?;
                // -1 as it starts with 0
                let chunk_offset =
                    (proof.offset() as u64).div_ceil(self.config.consensus.chunk_size) - 1;

                Ok(Some((
                    data_root,
                    data_size,
                    path_buff,
                    TxChunkOffset(chunk_offset.try_into().expect("Value exceeds u32::MAX")),
                )))
            })?
            else {
                return Ok(None);
            };

        let mut chunks = self.read_chunks(partition_chunk_offset_ii!(
            partition_offset,
            partition_offset
        ))?;
        let chunk_info = chunks
            .remove(&partition_offset)
            .ok_or_eyre("Could not find chunk bytes on disk")?;

        Ok(Some(PackedChunk {
            data_root,
            data_size,
            data_path,
            bytes: Base64::from(chunk_info.0),
            partition_offset,
            tx_offset: chunk_offset,
            packing_address: self.config.node_config.miner_address(),
            partition_hash: self.partition_hash().unwrap(),
        }))
    }

    /// Returns chunk metadata (data_root and data_path) without reading chunk bytes.
    ///
    /// This is more efficient than `generate_full_chunk` when only the merkle proof
    /// metadata is needed (e.g., for path validation).
    pub fn get_chunk_metadata(
        &self,
        partition_offset: PartitionChunkOffset,
    ) -> Result<Option<(DataRoot, Base64)>> {
        self.query_submodule_db_by_offset(partition_offset, |tx| {
            let Some(tx_path) = get_tx_path_by_offset(tx, partition_offset)? else {
                return Ok(None);
            };

            // Extract the data_root from the tx_path leaf node
            let path_buff = Base64::from(tx_path);
            let proof = get_leaf_proof(&path_buff)?;
            let data_root = proof
                .hash()
                .map(H256::from)
                .ok_or_eyre("Unable to parse data_root from tx_path")?;

            let Some(data_path) = get_data_path_by_offset(tx, partition_offset)? else {
                return Ok(None);
            };

            Ok(Some((data_root, Base64::from(data_path))))
        })
    }

    /// Gets the tx_path and data_path for a chunk using its ledger relative offset
    pub fn read_tx_data_path(
        &self,
        chunk_offset: LedgerChunkOffset,
    ) -> eyre::Result<(Option<TxPath>, Option<ChunkDataPath>)> {
        self.query_submodule_db_by_offset(PartitionChunkOffset::from(chunk_offset), |tx| {
            Ok((
                get_tx_path_by_offset(tx, PartitionChunkOffset::from(chunk_offset))?,
                get_data_path_by_offset(tx, PartitionChunkOffset::from(chunk_offset))?,
            ))
        })
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

    pub(crate) fn cut_then_insert_interval_if_touching(
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
        let write_result = (|| -> eyre::Result<()> {
            // Lock to the submodules internal file handle & write the chunk
            let mut file = submodule.file.lock().unwrap();

            file.seek(SeekFrom::Start(u64::from(submodule_offset) * chunk_size))?;
            let bytes_written = file.write(bytes.as_slice()).map_err(|e| {
                error!("Failed to write chunk @ chunk_offset {chunk_offset} submodule_offset {submodule_offset}: {}", e);
                eyre::eyre!("Failed to write chunk @ chunk_offset {chunk_offset} submodule_offset {submodule_offset}: {}", e)
            })?;
            // Ensure all bytes were written
            if bytes_written != bytes.len() {
                return Err(eyre::eyre!(
                    "Incomplete write: expected {} bytes, wrote {} bytes @ chunk_offset {chunk_offset} submodule_offset {submodule_offset}",
                    bytes.len(),
                    bytes_written
                ));
            }
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
    fn make_range_partition_relative(
        &self,
        chunk_range: LedgerChunkRange,
    ) -> eyre::Result<PartitionChunkRange> {
        let storage_module_range = self.get_storage_module_ledger_offsets()?;
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
    use irys_testing_utils::{chunk_bytes_gen, utils::setup_tracing_and_temp_dir};
    use irys_types::{
        ConsensusConfig, DataTransactionHeaderV1, H256, NodeConfig, SimpleRNG, StorageSyncConfig,
        TxChunkOffset, irys::IrysSigner, ledger_chunk_offset_ii, partition_chunk_offset_ii,
    };
    use nodit::interval::ii;

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

        let tmp_dir = setup_tracing_and_temp_dir(Some("data_path_test"), false);
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
    fn pending_writes_test() -> eyre::Result<()> {
        let infos = [StorageModuleInfo {
            id: 0,
            partition_assignment: Some(PartitionAssignment::default()),
            submodules: vec![
                (partition_chunk_offset_ii!(0, 50), "hdd0-test".into()), // 0 to 50 inclusive
            ],
        }];

        std::env::set_var("RUST_LOG", "debug");
        let tmp_dir = setup_tracing_and_temp_dir(Some("pending_writes_test"), false);
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

        // Write 9 more entropy chunks
        for chunk_offset in 10..19_u32 {
            storage_module.write_chunk(
                PartitionChunkOffset::from(chunk_offset),
                entropy_bytes.clone(),
                ChunkType::Entropy,
            );
        }

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

        let tmp_dir = setup_tracing_and_temp_dir(Some("data_path_test"), false);
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

        let tmp_dir = setup_tracing_and_temp_dir(Some("write_interruption_test"), false);
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
        let tmp_dir = setup_tracing_and_temp_dir(Some("successful_write_test"), false);
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
        let tmp_dir = setup_tracing_and_temp_dir(Some("interrupted_load_test"), false);
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
        std::env::set_var("RUST_LOG", "info");
        let tmp_dir = setup_tracing_and_temp_dir(Some("data_path_test"), true);

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
}
